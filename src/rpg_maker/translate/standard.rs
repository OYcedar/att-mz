//! RPG Maker 标准资产翻译的顶层编排。
//!
//! Standard 只负责读取当前资产、建立任务计划、在外部上限内执行任务，
//! 并按计划顺序逐项提交。任务可以并发完成，但后续任务绝不能越过前序任务
//! 写入数据库，因此失败时始终只保留一个确定的成功前缀。

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::diagnostic::SafeDiagnostic;
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguagePair};
#[cfg(test)]
use crate::llm::LlmUsage;
use crate::llm::{ChatMessage, LlmClientConcurrency};
use crate::rpg_maker::model::{LogicalTextLocation, TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};

use super::executor::FinalLlmResponseMetadata;
use super::profile::RpgMakerTranslationProfile;
use super::task_record::{
    NoOpTranslationTaskRecordSink, TranslationTaskCommitFailure,
    TranslationTaskCommitFailureImpact, TranslationTaskCommitPhase, TranslationTaskExecution,
    TranslationTaskExecutionEvidence, TranslationTaskExecutionFailure,
    TranslationTaskRecordDocument, TranslationTaskRecordFinalState, TranslationTaskRecordSink,
};

/// 模型响应返回后仍要执行验收、传播准备和顺序提交；这些本地完成项可以在内存中
/// 等待前序任务，但不能反向扩大进入 Executor 的任务数。Executor worker 始终等于
/// Client 的活动请求上限。入场许可在 Executor 之前取得，因此最早未完成任务永远已
/// 占有位置，不会被后序完成项挤出而死锁。
///
/// 该倍率是产品内部吞吐策略，不是项目规模上限，也不进入用户配置。以 SSPV 在同一
/// Windows/MSVC Release 环境交错运行 7 轮后，无额外窗口、N、2N、4N 额外窗口的
/// Translate 中位耗时分别为 4.862、4.870、4.966、5.005 秒。前两项无法在慢首任务下
/// 持续补充 HTTP 工作，也无法及时发现窗口外的准备/错序失败，因此其性能成绩按核心
/// 行为差异作废。2N 和 4N 额外窗口都通过该压力契约，2N 更小且更快，故本地在途宽度
/// 固定为活动 HTTP 宽度加 2N 完成窗口，即 3N。
const EXECUTION_WINDOW_MULTIPLIER: usize = 2;

fn execution_worker_count(task_count: usize, max_concurrent_requests: usize) -> usize {
    task_count.min(max_concurrent_requests.max(1))
}

fn local_in_flight_window_count(task_count: usize, max_concurrent_requests: usize) -> usize {
    task_count.min(
        max_concurrent_requests
            .max(1)
            .saturating_mul(EXECUTION_WINDOW_MULTIPLIER.saturating_add(1))
            .max(1),
    )
}

/// 一次标准资产翻译需要的可选外部资料。
///
/// 该类型把两个可选路径作为一个拥有所有权的请求交给 Planner。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationInput {
    terminology_path: Option<PathBuf>,
    placeholder_rules_path: Option<PathBuf>,
}

impl StandardTranslationInput {
    pub(crate) fn new(
        terminology_path: Option<PathBuf>,
        placeholder_rules_path: Option<PathBuf>,
    ) -> Self {
        Self {
            terminology_path,
            placeholder_rules_path,
        }
    }

    /// 交出两个外部资料路径，供 Planner 拥有其生命周期。
    pub(crate) fn into_parts(self) -> (Option<PathBuf>, Option<PathBuf>) {
        (self.terminology_path, self.placeholder_rules_path)
    }
}

/// Standard 编排本身真正消费的配置事实。
///
/// Profile 由外部配置边界一次建立，只提供供应商允许的活动 HTTP 请求数。
/// 响应返回后的本地流水线窗口由 Standard 的产品策略拥有，不反向成为配置。
pub(crate) trait StandardTranslationProfile: Send + Sync + 'static {
    fn max_concurrent_requests(&self) -> NonZeroUsize;
}

impl<L> StandardTranslationProfile for RpgMakerTranslationProfile<L>
where
    L: LlmClientConcurrency + 'static,
{
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.llm_client().max_concurrent_requests()
    }
}

impl<L> StandardTranslationProfile for Arc<RpgMakerTranslationProfile<L>>
where
    L: LlmClientConcurrency + 'static,
{
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.as_ref().llm_client().max_concurrent_requests()
    }
}

/// 一个语义翻译单元的持久化身份与读取时的原文事实。
///
/// Store 在写入时可以用原文事实防止把旧计划提交到已变化的资产上。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TranslationUnitIdentity {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    logical_location: LogicalTextLocation,
    source_content: TextUnitContent,
    source_context_json: String,
}

impl TranslationUnitIdentity {
    pub(crate) fn new(
        owner: RpgMakerStandardAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
        source_content: TextUnitContent,
        source_context_json: impl Into<String>,
    ) -> Self {
        Self {
            owner,
            kind,
            logical_location: LogicalTextLocation::new(group_location, role),
            source_content,
            source_context_json: source_context_json.into(),
        }
    }

    pub(crate) const fn owner(&self) -> RpgMakerStandardAssetOwner {
        self.owner
    }

    /// 返回语义单元所属的领域组种类。
    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn role(&self) -> &TextUnitRole {
        self.logical_location.role()
    }

    pub(crate) fn role_label(&self) -> String {
        match self.role() {
            TextUnitRole::Scalar(key) => key.as_str().to_owned(),
            TextUnitRole::DialogueSpeaker => "speaker".to_owned(),
            TextUnitRole::DialogueBody => "body".to_owned(),
            TextUnitRole::Choices => "choices".to_owned(),
            TextUnitRole::ScrollingText => "scrolling_text".to_owned(),
        }
    }

    /// 返回译文所属复合语义组的结构化位置。
    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        self.logical_location.group_location()
    }

    /// 写回目标是否为 `<name:value>` 标签值。
    ///
    /// 标签值由提取协议保证不含 `>`；写回按字节区间拼接替换值，译文一旦引入
    /// `>` 就会提前闭合标签并把余文泄漏为 note/comment 正文,因此验收必须拒绝。
    pub(crate) fn targets_tag_value(&self) -> bool {
        matches!(
            self.group_location(),
            RpgMakerLocation::NoteTag { .. } | RpgMakerLocation::CommentTag { .. }
        )
    }

    pub(crate) fn source_content(&self) -> &TextUnitContent {
        &self.source_content
    }

    pub(crate) fn source_context_json(&self) -> &str {
        &self.source_context_json
    }
}

/// 一个候选译文最终会写入的完整目标集合所建立的约束。
///
/// 去重代表的物理位置不一定拥有最严格的写回协议，因此约束必须由主目标与全部
/// 传播/家族成员共同建立，不能只观察代表 identity。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTargetConstraints {
    targets_tag_value: bool,
}

impl TranslationTargetConstraints {
    pub(crate) fn from_identities<'a>(
        identities: impl IntoIterator<Item = &'a TranslationUnitIdentity>,
    ) -> Self {
        Self {
            targets_tag_value: identities
                .into_iter()
                .any(TranslationUnitIdentity::targets_tag_value),
        }
    }

    pub(crate) const fn targets_tag_value(self) -> bool {
        self.targets_tag_value
    }
}

/// 一条已经实际影响某个译文的术语事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminologyDependency {
    term: String,
    translation: String,
}

impl TerminologyDependency {
    pub(crate) fn new(term: impl Into<String>, translation: impl Into<String>) -> Self {
        Self {
            term: term.into(),
            translation: translation.into(),
        }
    }

    pub(crate) fn term(&self) -> &str {
        &self.term
    }

    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }
}

/// 从标准资产表读出的一个语义单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationAsset {
    identity: TranslationUnitIdentity,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
}

impl StandardTranslationAsset {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        translation: Option<TextUnitContent>,
        translation_state: Option<Sha256Fingerprint>,
    ) -> Self {
        Self {
            identity,
            translation,
            translation_state,
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationUnitIdentity,
        Option<TextUnitContent>,
        Option<Sha256Fingerprint>,
    ) {
        (self.identity, self.translation, self.translation_state)
    }
}

/// 一个不可拆散的 RPG Maker 复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    assets: Vec<StandardTranslationAsset>,
}

impl StandardTranslationGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        assets: Vec<StandardTranslationAsset>,
    ) -> Self {
        Self {
            kind,
            group_location,
            assets,
        }
    }

    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    #[cfg(test)]
    pub(crate) fn assets(&self) -> &[StandardTranslationAsset] {
        &self.assets
    }

    pub(crate) fn into_assets(self) -> Vec<StandardTranslationAsset> {
        self.assets
    }
}

/// Reader 在同一个一致读视图中建立的完整标准翻译语料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationSnapshotBaseline {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    owner_snapshots: Vec<TranslationOwnerSnapshot>,
    terminology_json: String,
    placeholder_rules_json: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationOwnerSnapshot {
    owner: RpgMakerStandardAssetOwner,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    asset_snapshot_fingerprint: AssetSnapshotFingerprint,
}

impl TranslationOwnerSnapshot {
    pub(crate) const fn new(
        owner: RpgMakerStandardAssetOwner,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        asset_snapshot_fingerprint: AssetSnapshotFingerprint,
    ) -> Self {
        Self {
            owner,
            source_snapshot_fingerprint,
            asset_snapshot_fingerprint,
        }
    }

    pub(crate) const fn owner(self) -> RpgMakerStandardAssetOwner {
        self.owner
    }

    pub(crate) const fn source_snapshot_fingerprint(self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) const fn asset_snapshot_fingerprint(self) -> AssetSnapshotFingerprint {
        self.asset_snapshot_fingerprint
    }
}

impl TranslationSnapshotBaseline {
    pub(crate) fn new(
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        owner_snapshots: Vec<TranslationOwnerSnapshot>,
        terminology_json: String,
        placeholder_rules_json: String,
    ) -> Self {
        Self {
            source_snapshot_fingerprint,
            owner_snapshots,
            terminology_json,
            placeholder_rules_json,
        }
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn owner_snapshots(&self) -> &[TranslationOwnerSnapshot] {
        &self.owner_snapshots
    }

    pub(crate) fn terminology_json(&self) -> &str {
        &self.terminology_json
    }

    pub(crate) fn placeholder_rules_json(&self) -> &str {
        &self.placeholder_rules_json
    }
}

/// Reader 在同一个一致读视图中建立的完整标准翻译语料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationCorpus {
    groups: Vec<StandardTranslationGroup>,
    baseline: TranslationSnapshotBaseline,
}

impl StandardTranslationCorpus {
    #[cfg(test)]
    pub(crate) fn new(groups: Vec<StandardTranslationGroup>) -> Self {
        Self::with_snapshot(
            groups,
            SourceSnapshotFingerprint::from_bytes([0; 32]),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
        )
    }

    pub(crate) fn with_snapshot(
        groups: Vec<StandardTranslationGroup>,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        owner_snapshots: Vec<TranslationOwnerSnapshot>,
        terminology_json: String,
        placeholder_rules_json: String,
    ) -> Self {
        Self {
            groups,
            baseline: TranslationSnapshotBaseline::new(
                source_snapshot_fingerprint,
                owner_snapshots,
                terminology_json,
                placeholder_rules_json,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[StandardTranslationGroup] {
        &self.groups
    }

    pub(crate) fn into_parts(self) -> (Vec<StandardTranslationGroup>, TranslationSnapshotBaseline) {
        (self.groups, self.baseline)
    }
}

/// 在任何 LLM 请求前必须完成的标准资产准备。
///
/// 具体动作由 Planner 从当前语料和本次外部资料推导；Standard 不重新解释
/// 术语差异或译文失效规则。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationInvalidation {
    identity: TranslationUnitIdentity,
    expected_translation: TextUnitContent,
    expected_translation_state: Sha256Fingerprint,
}

/// 可以直接复用的一条现有译文快照。
///
/// Store 必须在写入目标前确认种子仍保持读取时的译文和语义状态，避免把
/// 已被并发修改的旧事实扩散到其他位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseSeed {
    identity: TranslationUnitIdentity,
    expected_translation: TextUnitContent,
    expected_translation_state: Sha256Fingerprint,
}

impl TranslationReuseSeed {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            expected_translation,
            expected_translation_state,
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    #[cfg(test)]
    pub(crate) fn expected_translation(&self) -> &TextUnitContent {
        &self.expected_translation
    }

    #[cfg(test)]
    pub(crate) const fn expected_translation_state(&self) -> Sha256Fingerprint {
        self.expected_translation_state
    }

    pub(crate) fn into_parts(
        self,
    ) -> (TranslationUnitIdentity, TextUnitContent, Sha256Fingerprint) {
        (
            self.identity,
            self.expected_translation,
            self.expected_translation_state,
        )
    }
}

/// 一个将被现有译文覆盖的目标及其读取时状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseTarget {
    identity: TranslationUnitIdentity,
    expected_translation: Option<TextUnitContent>,
    expected_translation_state: Option<Sha256Fingerprint>,
    replacement_translation_state: Sha256Fingerprint,
}

impl TranslationReuseTarget {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        expected_translation: Option<TextUnitContent>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            expected_translation,
            expected_translation_state,
            replacement_translation_state,
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    #[cfg(test)]
    pub(crate) fn expected_translation(&self) -> Option<&TextUnitContent> {
        self.expected_translation.as_ref()
    }

    #[cfg(test)]
    pub(crate) const fn expected_translation_state(&self) -> Option<Sha256Fingerprint> {
        self.expected_translation_state
    }

    #[cfg(test)]
    pub(crate) const fn replacement_translation_state(&self) -> Sha256Fingerprint {
        self.replacement_translation_state
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationUnitIdentity,
        Option<TextUnitContent>,
        Option<Sha256Fingerprint>,
        Sha256Fingerprint,
    ) {
        (
            self.identity,
            self.expected_translation,
            self.expected_translation_state,
            self.replacement_translation_state,
        )
    }
}

/// 一条现有译文向一个或多个具体资产位置的复用计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuse {
    seed: TranslationReuseSeed,
    targets: Vec<TranslationReuseTarget>,
}

impl TranslationReuse {
    pub(crate) fn new(seed: TranslationReuseSeed, targets: Vec<TranslationReuseTarget>) -> Self {
        Self { seed, targets }
    }

    #[cfg(test)]
    pub(crate) fn seed(&self) -> &TranslationReuseSeed {
        &self.seed
    }

    pub(crate) fn targets(&self) -> &[TranslationReuseTarget] {
        &self.targets
    }

    pub(crate) fn into_parts(self) -> (TranslationReuseSeed, Vec<TranslationReuseTarget>) {
        (self.seed, self.targets)
    }
}

impl TranslationInvalidation {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            expected_translation,
            expected_translation_state,
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    #[cfg(test)]
    pub(crate) fn expected_translation(&self) -> &TextUnitContent {
        &self.expected_translation
    }

    #[cfg(test)]
    pub(crate) const fn expected_translation_state(&self) -> Sha256Fingerprint {
        self.expected_translation_state
    }

    pub(crate) fn into_parts(
        self,
    ) -> (TranslationUnitIdentity, TextUnitContent, Sha256Fingerprint) {
        (
            self.identity,
            self.expected_translation,
            self.expected_translation_state,
        )
    }
}

/// 标准翻译计划准备阶段的逐单元对账计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanPreparationCounts {
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
}

impl TranslationPlanPreparationCounts {
    pub(crate) const fn new(retained: usize, invalidated: usize, not_applicable: usize) -> Self {
        Self {
            retained,
            invalidated,
            not_applicable,
        }
    }
}

/// 在任何 LLM 请求前必须完成的标准资产准备。
///
/// 每项失效同时携带读取时的旧译文和语义状态，Store 必须在清理前原子确认这些
/// 事实仍未变化，避免并发翻译把更新后的译文误删。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanPreparation {
    invalidations: Vec<TranslationInvalidation>,
    reuses: Vec<TranslationReuse>,
    planning_failures: Vec<TranslationPlanningFailure>,
    terminology_json: String,
    placeholder_rules_json: String,
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
    snapshot_baseline: TranslationSnapshotBaseline,
}

impl TranslationPlanPreparation {
    #[cfg(test)]
    pub(crate) fn new(
        invalidations: Vec<TranslationInvalidation>,
        reuses: Vec<TranslationReuse>,
        terminology_json: String,
        placeholder_rules_json: String,
        retained: usize,
        invalidated: usize,
        not_applicable: usize,
    ) -> Self {
        Self::with_baseline_and_planning_failures(
            invalidations,
            reuses,
            terminology_json,
            placeholder_rules_json,
            TranslationPlanPreparationCounts::new(retained, invalidated, not_applicable),
            TranslationSnapshotBaseline::new(
                SourceSnapshotFingerprint::from_bytes([0; 32]),
                Vec::new(),
                "[]".to_owned(),
                "[]".to_owned(),
            ),
            Vec::new(),
        )
    }

    pub(crate) fn with_baseline_and_planning_failures(
        invalidations: Vec<TranslationInvalidation>,
        reuses: Vec<TranslationReuse>,
        terminology_json: String,
        placeholder_rules_json: String,
        counts: TranslationPlanPreparationCounts,
        snapshot_baseline: TranslationSnapshotBaseline,
        planning_failures: Vec<TranslationPlanningFailure>,
    ) -> Self {
        Self {
            invalidations,
            reuses,
            planning_failures,
            terminology_json,
            placeholder_rules_json,
            retained: counts.retained,
            invalidated: counts.invalidated,
            not_applicable: counts.not_applicable,
            snapshot_baseline,
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidations(&self) -> &[TranslationInvalidation] {
        &self.invalidations
    }

    #[cfg(test)]
    pub(crate) fn reuses(&self) -> &[TranslationReuse] {
        &self.reuses
    }

    pub(crate) fn planning_failures(&self) -> &[TranslationPlanningFailure] {
        &self.planning_failures
    }

    #[cfg(test)]
    pub(crate) fn with_test_planning_failures(
        mut self,
        planning_failures: Vec<TranslationPlanningFailure>,
    ) -> Self {
        self.planning_failures = planning_failures;
        self
    }

    pub(crate) const fn retained(&self) -> usize {
        self.retained
    }

    pub(crate) const fn invalidated(&self) -> usize {
        self.invalidated
    }

    pub(crate) const fn not_applicable(&self) -> usize {
        self.not_applicable
    }

    pub(crate) fn reused(&self) -> usize {
        self.reuses.iter().map(|reuse| reuse.targets().len()).sum()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<TranslationInvalidation>,
        Vec<TranslationReuse>,
        String,
        String,
        usize,
        usize,
        TranslationSnapshotBaseline,
    ) {
        (
            self.invalidations,
            self.reuses,
            self.terminology_json,
            self.placeholder_rules_json,
            self.retained,
            self.not_applicable,
            self.snapshot_baseline,
        )
    }
}

/// Placeholder 投影阶段无法建立受信语义、因而不会进入任何 LLM 任务的标准单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanningFailure {
    identity: TranslationUnitIdentity,
    reason: TranslationPlanningFailureReason,
}

impl TranslationPlanningFailure {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        reason: TranslationPlanningFailureReason,
    ) -> Self {
        Self { identity, reason }
    }

    #[cfg(test)]
    pub(crate) fn reason(&self) -> &TranslationPlanningFailureReason {
        &self.reason
    }
}

/// 规划期失败与模型响应拒绝分属不同阶段，不共享 ID、attempt 或拒绝原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationPlanningFailureReason {
    PlaceholderProtection { message: String },
    PlaceholderProjection { message: String },
}

/// 任务在确定计划中的序号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct StandardTranslationTaskIndex(usize);

impl StandardTranslationTaskIndex {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for StandardTranslationTaskIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 占位符来自 RPG Maker 内置保护规格还是用户提供的自定义规则。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PlaceholderRuleOrigin {
    BuiltIn,
    Custom,
}

/// 占位符对应整个匹配，或结构化匹配中可翻译捕获组两侧的外壳。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PlaceholderSegment {
    Whole,
    Begin,
    End,
}

/// Planner 为某个活跃语义单元建立的一条占位符反查事实。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AppliedPlaceholder {
    token: String,
    original: String,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: String,
    segment: PlaceholderSegment,
}

impl AppliedPlaceholder {
    pub(crate) fn new(
        token: impl Into<String>,
        original: impl Into<String>,
        origin: PlaceholderRuleOrigin,
        label: impl Into<String>,
        scope: impl Into<String>,
        segment: PlaceholderSegment,
    ) -> Self {
        Self {
            token: token.into(),
            original: original.into(),
            origin,
            label: label.into(),
            scope: scope.into(),
            segment,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) const fn origin(&self) -> PlaceholderRuleOrigin {
        self.origin
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// 返回建立本绑定的稳定领域作用域。
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    pub(crate) const fn segment(&self) -> PlaceholderSegment {
        self.segment
    }
}

/// 一个语义单元除最终译文以外的全部当前翻译语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationStateContext(Sha256Fingerprint);

impl TranslationStateContext {
    pub(crate) const fn new(fingerprint: Sha256Fingerprint) -> Self {
        Self(fingerprint)
    }

    pub(crate) fn finish(self, translation: &TextUnitContent) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-unit-state");
        hasher.frame(1, self.0.as_bytes());
        match translation {
            TextUnitContent::Value(value) => {
                hasher.frame(2, b"value").frame(3, value.as_bytes());
            }
            TextUnitContent::Lines(lines) => {
                let line_count = u64::try_from(lines.len())
                    .expect("译文行数必须能表示为 u64")
                    .to_le_bytes();
                hasher.frame(2, b"lines").frame(3, &line_count);
                for line in lines {
                    hasher.frame(4, line.as_bytes());
                }
            }
        }
        hasher.finish()
    }
}

/// 去重传播目标以及该语义单元的独立语义上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPropagationTarget {
    identity: TranslationUnitIdentity,
    state_context: TranslationStateContext,
}

impl TranslationPropagationTarget {
    pub(crate) const fn new(
        identity: TranslationUnitIdentity,
        state_context: TranslationStateContext,
    ) -> Self {
        Self {
            identity,
            state_context,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) const fn state_context(&self) -> TranslationStateContext {
        self.state_context
    }
}

/// TaskBlock 单元是需要模型返回结果的活跃原文，或只提供上下文的虚原文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationVirtualReason {
    ExistingTranslation,
    NonSourceLanguage,
    FullyProtected,
    Duplicate {
        leader: Box<TranslationUnitIdentity>,
    },
    Reused {
        seed: Box<TranslationUnitIdentity>,
        translation: TextUnitContent,
    },
}

/// 需要 Executor 返回的一个活跃翻译单元。
///
/// 虚原文没有 ID，因此不会出现在该集合中。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedLineShape {
    /// 模型可以按译文语义返回任意非空行序列。
    Reflow,
    /// 模型必须返回精确数量的独立行。
    Aligned(NonZeroUsize),
}

/// Executor 验收一个模型 ID 所需的全部 Planner 事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedTranslationValidation {
    line_shape: ExpectedLineShape,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
}

impl ExpectedTranslationValidation {
    pub(crate) fn new(
        line_shape: ExpectedLineShape,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        language_analysis: LanguageAnalysis,
    ) -> Self {
        Self {
            line_shape,
            protected_text: protected_text.into(),
            applied_placeholders,
            language_analysis,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedTranslationOutput {
    id: usize,
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationUnitIdentity>,
    validation: ExpectedTranslationValidation,
    state_context: TranslationStateContext,
    propagation_state_contexts: Vec<TranslationStateContext>,
}

impl ExpectedTranslationOutput {
    pub(crate) fn new(
        id: usize,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
    ) -> Self {
        assert!(id > 0, "模型输出 ID 必须是正整数");
        assert_eq!(
            propagation_targets.len(),
            propagation_state_contexts.len(),
            "每个传播目标必须具有对应的独立状态上下文"
        );
        Self {
            id,
            identity,
            propagation_targets,
            validation,
            state_context,
            propagation_state_contexts,
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) const fn line_shape(&self) -> ExpectedLineShape {
        self.validation.line_shape
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationUnitIdentity] {
        &self.propagation_targets
    }

    pub(crate) fn protected_text(&self) -> &str {
        &self.validation.protected_text
    }

    pub(crate) fn applied_placeholders(&self) -> &[AppliedPlaceholder] {
        &self.validation.applied_placeholders
    }

    /// 返回 Planner 针对代表原文建立、供译后处理使用的唯一语言事实。
    pub(crate) fn language_analysis(&self) -> &LanguageAnalysis {
        &self.validation.language_analysis
    }

    pub(crate) const fn state_context(&self) -> TranslationStateContext {
        self.state_context
    }

    pub(crate) fn propagation_state_contexts(&self) -> &[TranslationStateContext] {
        &self.propagation_state_contexts
    }
}

/// 一个已经完成语义切块并生成最终最小消息的任务块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskBlock {
    index: StandardTranslationTaskIndex,
    language_pair: LanguagePair,
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl TranslationTaskBlock {
    pub(crate) fn new(
        index: StandardTranslationTaskIndex,
        language_pair: LanguagePair,
        messages: Vec<ChatMessage>,
        expected_outputs: Vec<ExpectedTranslationOutput>,
    ) -> Self {
        assert!(
            expected_outputs
                .iter()
                .enumerate()
                .all(|(index, output)| output.id() == index + 1),
            "任务内模型输出 ID 必须从 1 连续编号"
        );
        Self {
            index,
            language_pair,
            messages,
            expected_outputs,
        }
    }

    pub(crate) const fn index(&self) -> StandardTranslationTaskIndex {
        self.index
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) fn expected_outputs(&self) -> &[ExpectedTranslationOutput] {
        &self.expected_outputs
    }
}

/// Planner 建立的确定顺序计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationPlan {
    semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
    preparation: TranslationPlanPreparation,
    tasks: Vec<TranslationTaskBlock>,
}

impl StandardTranslationPlan {
    pub(crate) fn new(
        semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
        preparation: TranslationPlanPreparation,
        tasks: Vec<TranslationTaskBlock>,
    ) -> Self {
        assert!(
            tasks
                .iter()
                .enumerate()
                .all(|(ordinal, task)| task.index().get() == ordinal),
            "Standard 计划中的 TaskBlock 序号必须按自然计划顺序从零连续编号"
        );
        Self {
            semantics,
            preparation,
            tasks,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<super::semantics::ResolvedTranslationSemantics>,
        TranslationPlanPreparation,
        Vec<TranslationTaskBlock>,
    ) {
        (self.semantics, self.preparation, self.tasks)
    }
}

/// 经过 Executor 完整验收并可直接写入的一个语义单元译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPatch {
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationPropagationTarget>,
    translation: TextUnitContent,
    translation_state: Sha256Fingerprint,
}

impl TranslationPatch {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationPropagationTarget>,
        translation: TextUnitContent,
        translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            propagation_targets,
            translation,
            translation_state,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationPropagationTarget] {
        &self.propagation_targets
    }

    pub(crate) fn translation(&self) -> &TextUnitContent {
        &self.translation
    }

    pub(crate) const fn translation_state(&self) -> Sha256Fingerprint {
        self.translation_state
    }
}

/// 一个已经通过独立验收的任务 ID 及其可写 Patch。
///
/// ID 只属于本次 TaskBlock 协议，不进入资产表；保留在业务结果中用于准确关联
/// 已验收译文及其传播位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedTranslationDecision {
    id: usize,
    patch: TranslationPatch,
}

impl AcceptedTranslationDecision {
    pub(crate) fn new(id: usize, patch: TranslationPatch) -> Self {
        Self { id, patch }
    }

    #[cfg(test)]
    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationPropagationTarget] {
        self.patch.propagation_targets()
    }

    pub(crate) fn patch(&self) -> &TranslationPatch {
        &self.patch
    }

    #[cfg(test)]
    pub(crate) fn translation(&self) -> &TextUnitContent {
        self.patch.translation()
    }
}

/// 一个预期 ID 没有形成可写译文的正常业务原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationUnitRejectionReason {
    Missing,
    Duplicate,
    InvalidShape {
        message: String,
    },
    LineCountMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidLineText {
        line_index: usize,
    },
    BlankLineMismatch {
        line_index: usize,
        expected_blank: bool,
    },
    BlankTranslation,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch {
        token: String,
    },
    UnexpectedPlaceholderToken {
        token: String,
    },
    PlaceholderNormalizationAmbiguous {
        original: String,
    },
    SourceResidual {
        fragment: String,
    },
    /// 写回目标是 `<name:value>` 标签值：译文含 `>` 会提前闭合标签并破坏
    /// note/comment 的标签协议，必须整单元拒绝。
    TagValueContainsClosingDelimiter {
        line_index: usize,
    },
}

/// 一个仍需在后续 CLI 运行中重新翻译的预期单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnresolvedTranslationUnit {
    id: usize,
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationUnitIdentity>,
    reason: TranslationUnitRejectionReason,
}

impl UnresolvedTranslationUnit {
    pub(crate) fn new(
        id: usize,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        reason: TranslationUnitRejectionReason,
    ) -> Self {
        Self {
            id,
            identity,
            propagation_targets,
            reason,
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn reason(&self) -> &TranslationUnitRejectionReason {
        &self.reason
    }

    pub(crate) const fn location_count(&self) -> usize {
        1 + self.propagation_targets.len()
    }
}

/// 无法绑定为某个可写译文、但必须进入结构化运行诊断的模型协议事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationProtocolDiagnostic {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    InvalidId { item_index: usize },
    UnknownId { item_index: usize, id: usize },
}

/// 一个任务没有任何可用译文的正常原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskUnavailableReason {
    ModelResponseUnusable,
    AllOutputsRejected,
    RecoverableRequestExhausted {
        diagnostic: SafeDiagnostic,
    },
    RetryAfterExceedsConfiguredMaximum {
        retry_after: Duration,
        maximum: Duration,
        diagnostic: SafeDiagnostic,
    },
}

/// 一个非空的任务决定集合。
///
/// 非空性由类型本身承载，避免任务结果另外保存可与内容矛盾的状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NonEmptyTaskItems<T> {
    items: Vec<T>,
}

impl<T> NonEmptyTaskItems<T> {
    pub(crate) fn new(first: T, mut rest: Vec<T>) -> Self {
        rest.insert(0, first);
        Self { items: rest }
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.items
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskOutcomeContext {
    task_index: StandardTranslationTaskIndex,
    attempts: NonZeroUsize,
    diagnostics: Vec<TranslationProtocolDiagnostic>,
}

impl TranslationTaskOutcomeContext {
    pub(crate) fn new(
        task_index: StandardTranslationTaskIndex,
        attempts: NonZeroUsize,
        diagnostics: Vec<TranslationProtocolDiagnostic>,
    ) -> Self {
        Self {
            task_index,
            attempts,
            diagnostics,
        }
    }
}

/// 一个任务块的互斥正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskOutcome {
    Complete {
        context: TranslationTaskOutcomeContext,
        final_response: FinalLlmResponseMetadata,
        accepted: NonEmptyTaskItems<AcceptedTranslationDecision>,
    },
    Partial {
        context: TranslationTaskOutcomeContext,
        final_response: FinalLlmResponseMetadata,
        accepted: NonEmptyTaskItems<AcceptedTranslationDecision>,
        unresolved: NonEmptyTaskItems<UnresolvedTranslationUnit>,
    },
    Unavailable {
        context: TranslationTaskOutcomeContext,
        final_response: Option<FinalLlmResponseMetadata>,
        reason: TranslationTaskUnavailableReason,
        unresolved: NonEmptyTaskItems<UnresolvedTranslationUnit>,
    },
}

impl TranslationTaskOutcome {
    fn context(&self) -> &TranslationTaskOutcomeContext {
        match self {
            Self::Complete { context, .. }
            | Self::Partial { context, .. }
            | Self::Unavailable { context, .. } => context,
        }
    }

    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        match self {
            Self::Complete { context, .. }
            | Self::Partial { context, .. }
            | Self::Unavailable { context, .. } => context.task_index,
        }
    }

    pub(crate) const fn attempts(&self) -> NonZeroUsize {
        match self {
            Self::Complete { context, .. }
            | Self::Partial { context, .. }
            | Self::Unavailable { context, .. } => context.attempts,
        }
    }

    #[cfg(test)]
    fn final_response(&self) -> Option<&FinalLlmResponseMetadata> {
        match self {
            Self::Complete { final_response, .. } | Self::Partial { final_response, .. } => {
                Some(final_response)
            }
            Self::Unavailable { final_response, .. } => final_response.as_ref(),
        }
    }

    #[cfg(test)]
    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.final_response()
            .and_then(FinalLlmResponseMetadata::provider_request_id)
    }

    #[cfg(test)]
    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.final_response()
            .and_then(FinalLlmResponseMetadata::provider_response_id)
    }

    #[cfg(test)]
    pub(crate) fn final_response_usage(&self) -> Option<LlmUsage> {
        self.final_response()
            .and_then(FinalLlmResponseMetadata::usage)
    }

    pub(crate) fn accepted(&self) -> &[AcceptedTranslationDecision] {
        match self {
            Self::Complete { accepted, .. } | Self::Partial { accepted, .. } => accepted.as_slice(),
            Self::Unavailable { .. } => &[],
        }
    }

    pub(crate) fn unresolved(&self) -> &[UnresolvedTranslationUnit] {
        match self {
            Self::Partial { unresolved, .. } | Self::Unavailable { unresolved, .. } => {
                unresolved.as_slice()
            }
            Self::Complete { .. } => &[],
        }
    }

    pub(crate) fn diagnostics(&self) -> &[TranslationProtocolDiagnostic] {
        &self.context().diagnostics
    }

    /// 网络预算耗尽仍作为正常任务结果保留时，对应的安全、具体请求诊断。
    pub(crate) fn request_diagnostic(&self) -> Option<&SafeDiagnostic> {
        match self {
            Self::Unavailable {
                reason:
                    TranslationTaskUnavailableReason::RecoverableRequestExhausted { diagnostic }
                    | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                        diagnostic,
                        ..
                    },
                ..
            } => Some(diagnostic),
            Self::Complete { .. } | Self::Partial { .. } | Self::Unavailable { .. } => None,
        }
    }

    pub(crate) fn accepted_location_count(&self) -> usize {
        self.accepted()
            .iter()
            .map(|decision| 1 + decision.propagation_targets().len())
            .sum()
    }

    pub(crate) fn unresolved_location_count(&self) -> usize {
        self.unresolved()
            .iter()
            .map(UnresolvedTranslationUnit::location_count)
            .sum()
    }
}

/// 一次 Standard 运行已经确认的正常业务汇总。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationRunReport {
    semantics: Option<Arc<super::semantics::ResolvedTranslationSemantics>>,
    total_tasks: usize,
    complete_tasks: usize,
    partial_tasks: usize,
    unavailable_tasks: usize,
    accepted_decisions: usize,
    written_locations: usize,
    unresolved_decisions: usize,
    unresolved_locations: usize,
    protocol_diagnostics: usize,
    recoverable_request_exhaustions: usize,
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
    reused: usize,
}

impl StandardTranslationRunReport {
    #[cfg(test)]
    pub(crate) const fn empty(total_tasks: usize) -> Self {
        Self::with_reconciliation(total_tasks, 0, 0, 0, 0)
    }

    pub(crate) const fn with_reconciliation(
        total_tasks: usize,
        retained: usize,
        invalidated: usize,
        not_applicable: usize,
        reused: usize,
    ) -> Self {
        Self {
            semantics: None,
            total_tasks,
            complete_tasks: 0,
            partial_tasks: 0,
            unavailable_tasks: 0,
            accepted_decisions: 0,
            written_locations: 0,
            unresolved_decisions: 0,
            unresolved_locations: 0,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 0,
            retained,
            invalidated,
            not_applicable,
            reused,
        }
    }

    pub(crate) fn with_semantics(
        mut self,
        semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
    ) -> Self {
        self.semantics = Some(semantics);
        self
    }

    pub(crate) fn resolved_semantics(
        &self,
    ) -> Option<&Arc<super::semantics::ResolvedTranslationSemantics>> {
        self.semantics.as_ref()
    }

    pub(crate) fn record(&mut self, outcome: &TranslationTaskOutcome) {
        match outcome {
            TranslationTaskOutcome::Complete { .. } => self.complete_tasks += 1,
            TranslationTaskOutcome::Partial { .. } => self.partial_tasks += 1,
            TranslationTaskOutcome::Unavailable { reason, .. } => {
                self.unavailable_tasks += 1;
                if matches!(
                    reason,
                    TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. }
                        | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum { .. }
                ) {
                    self.recoverable_request_exhaustions += 1;
                }
            }
        }
        self.accepted_decisions += outcome.accepted().len();
        self.written_locations += outcome.accepted_location_count();
        self.unresolved_decisions += outcome.unresolved().len();
        self.unresolved_locations += outcome.unresolved_location_count();
        self.protocol_diagnostics += outcome.diagnostics().len();
    }

    pub(crate) fn record_planning_failures(&mut self, failures: &[TranslationPlanningFailure]) {
        self.unresolved_decisions += failures.len();
        self.unresolved_locations += failures.len();
    }

    pub(crate) const fn total_tasks(&self) -> usize {
        self.total_tasks
    }

    pub(crate) const fn complete_tasks(&self) -> usize {
        self.complete_tasks
    }

    pub(crate) const fn partial_tasks(&self) -> usize {
        self.partial_tasks
    }

    pub(crate) const fn unavailable_tasks(&self) -> usize {
        self.unavailable_tasks
    }

    pub(crate) fn accepted_decisions(&self) -> usize {
        self.accepted_decisions
    }

    pub(crate) const fn written_locations(&self) -> usize {
        self.written_locations
    }

    pub(crate) const fn unresolved_decisions(&self) -> usize {
        self.unresolved_decisions
    }

    pub(crate) const fn unresolved_locations(&self) -> usize {
        self.unresolved_locations
    }

    pub(crate) const fn protocol_diagnostics(&self) -> usize {
        self.protocol_diagnostics
    }

    pub(crate) const fn recoverable_request_exhaustions(&self) -> usize {
        self.recoverable_request_exhaustions
    }

    pub(crate) const fn retained(&self) -> usize {
        self.retained
    }

    pub(crate) const fn invalidated(&self) -> usize {
        self.invalidated
    }

    pub(crate) const fn not_applicable(&self) -> usize {
        self.not_applicable
    }

    pub(crate) const fn reused(&self) -> usize {
        self.reused
    }
}

/// 在一个一致读视图中取得统一标准文本表的当前事实。
pub(crate) trait StandardTranslationAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<StandardTranslationCorpus, Self::Error>> + Send;
}

/// 把当前语料与本次外部资料建立为确定性翻译计划。
///
/// Planner 完整拥有 RPG Maker 自然排序、语义边界切块、源语言判定、虚原文、术语影响、
/// PCRE2 占位符保护和 Markdown 消息构造的上游承诺。Standard 只依赖它返回的计划，
/// 不跨过 Planner 重新解释这些规则。
///
/// Planner 必须先在最大仍有关联的 RPG Maker 结构范围内组织复合 Group，再按外部 Profile
/// 提供的 user message 字符装箱目标切割；不得为了接近目标拼接无关范围。单个完整 Group
/// 超过目标时独立成块，不能拆组或拒绝规范内容，后续任务继续使用原目标。每个 TaskBlock
/// 内待翻译单元的 ID 从 1 连续递增，虚原文只保留原文且没有 ID。省略外部资源时复用项目
/// 当前快照；显式资源在全部解析成功后成为新快照。Planner 按每个语义单元实际触发的术语
/// 和占位符语义对账，并把资源更新、失效清理与可复用传播一并写入 Preparation。
pub(crate) trait StandardTranslationTaskPlanner: Send + Sync {
    type Profile: StandardTranslationProfile;
    type Error: Error + Send + Sync + 'static;

    fn plan(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        corpus: StandardTranslationCorpus,
        input: StandardTranslationInput,
    ) -> impl Future<Output = Result<StandardTranslationPlan, Self::Error>> + Send;
}

/// 执行一个已计划任务并返回正常业务结果。
///
/// 只有可恢复网络请求可以按外部预算重试。模型内容全部、部分或完全没有形成译文
/// 都由 `TranslationTaskOutcome` 表达，不得转换成错误或阻断其他任务。
/// Executor 不写项目数据库。
/// Executor 只能把 TaskBlock 已经建立的完整 `messages` 发送给 LLM；结构化位置、
/// 表归属、占位符反查和提交身份只用于程序内部验收，不能再拼入提示词。
pub(crate) trait StandardTranslationTaskExecutor: Send + Sync {
    type Profile: StandardTranslationProfile;
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        profile: &Self::Profile,
        task: &TranslationTaskBlock,
    ) -> impl Future<
        Output = Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>>,
    > + Send;
}

/// 拥有标准译文准备与单任务提交事务的存储边界。
pub(crate) trait StandardTranslationResultStore: Send + Sync {
    type PreparedCommit: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    /// 在任何 LLM 请求前原子应用一次 Planner 建立的准备。
    ///
    /// 对每个受影响语义单元同时清除译文及旧语义状态，并用预期原文阻止过时计划写入；
    /// 未列出的译文保持不变。
    fn apply_preparation(
        &self,
        project: &OpenedProject,
        preparation: TranslationPlanPreparation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// 把一个指定任务的全部验收译文编码为不产生副作用的提交计划。
    ///
    /// 不同任务的准备可以乱序并行；返回值只能由 `commit_prepared` 消费。
    /// 实现应只在本次 Future 内借助共享引用读取任务结果，且不应让
    /// `PreparedCommit` 长期持有它；生产实现借此让顺序最终化线在报告后以零复制
    /// 路径把结果移动进后续终态处理。替代实现即使保留共享引用也不得影响正确性。
    fn prepare_commit(
        &self,
        outcome: Arc<TranslationTaskOutcome>,
    ) -> impl Future<
        Output = Result<Self::PreparedCommit, TranslationTaskCommitFailure<Self::Error>>,
    > + Send;

    /// 按调用顺序原子提交一个已经准备好的任务结果。
    fn commit_prepared(
        &self,
        project: &OpenedProject,
        prepared: Self::PreparedCommit,
    ) -> impl Future<Output = Result<(), TranslationTaskCommitFailure<Self::Error>>> + Send;

    /// 显式终结本轮标准翻译持有的存储会话。
    ///
    /// 无论主流程成功、失败或取消都会调用；实现必须完成残留事务观察、回滚和连接关闭，
    /// 并把收尾失败作为独立事实返回。
    fn finalize(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 完成一轮项目数据库标准 RPG Maker 资产翻译的职责契约。
///
/// 成功表示所有计划任务都已经得到正常业务结果并按序完成必要提交，不表示所有原文
/// 都获得了译文。模型内容不可用和可恢复网络预算耗尽都不会阻断后续任务。
/// 只有依赖无法继续履行契约时才返回错误。
pub(crate) trait StandardTranslation: Send + Sync {
    /// 与配置解析器产物一致的执行配置。
    type Profile: Send + Sync + 'static;
    /// 标准翻译失败。
    type Error: Error + Send + Sync + 'static;

    /// 使用指定配置完成一次标准资产翻译。
    fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> impl Future<Output = Result<OperationCompletion<StandardTranslationRunReport>, Self::Error>>
    + Send;
}

/// Standard 翻译向普通 JSONL 可观测性边界提交的摘要业务事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardTranslationLogEvent {
    PlanningUnresolved {
        units: usize,
    },
    TaskStarted {
        task_index: StandardTranslationTaskIndex,
        total_tasks: usize,
    },
    TaskFinished {
        task_index: StandardTranslationTaskIndex,
        outcome: StandardTranslationLogTaskOutcome,
        attempts: Option<NonZeroUsize>,
        retry_exhausted: bool,
        diagnostic: Option<SafeDiagnostic>,
    },
}

/// 一项任务在顺序最终化边界得到的可观察终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StandardTranslationLogTaskOutcome {
    Complete,
    Partial,
    Unavailable,
    ExecutionFailed,
    CommitFailed,
    NotCommitted,
    InvalidResult,
}

/// 同步、不可失败的 Standard 翻译观察入口。
pub(crate) trait StandardTranslationLog: Send + Sync {
    fn emit(&self, event: StandardTranslationLogEvent);
}

/// 执行线交给顺序最终化线的一个已启动任务结果。
struct ScheduledTaskCompletion<E> {
    task: TranslationTaskBlock,
    execution: TaskExecutionCompletion<E>,
    in_flight_permit: OwnedSemaphorePermit,
}

/// 纯计算准备完成后交给顺序最终化线的任务结果。
struct PreparedScheduledTaskCompletion<E, S, C> {
    task: TranslationTaskBlock,
    execution: PreparedTaskExecutionCompletion<E, S, C>,
    in_flight_permit: OwnedSemaphorePermit,
}

impl<E, S, C> PreparedScheduledTaskCompletion<E, S, C> {
    fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task.index()
    }

    fn requires_stop_admission(&self) -> bool {
        match &self.execution {
            PreparedTaskExecutionCompletion::Outcome {
                outcome,
                prepared_commit,
                ..
            } => {
                outcome.task_index() != self.task_index() || matches!(prepared_commit, Some(Err(_)))
            }
            PreparedTaskExecutionCompletion::Failed { .. } => true,
        }
    }
}

enum PreparedTaskExecutionCompletion<E, S, C> {
    Outcome {
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        prepared_commit: Option<Result<C, TranslationTaskCommitFailure<S>>>,
    },
    Failed {
        task_index: StandardTranslationTaskIndex,
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: Option<SafeDiagnostic>,
        cancelled: bool,
    },
}

fn closed_result_sequence_violation<T>(
    result_slots: &[Option<T>],
    next_task_index: usize,
    cancelled: bool,
) -> Option<(
    StandardTranslationTaskIndex,
    Option<StandardTranslationTaskIndex>,
)> {
    let expected = StandardTranslationTaskIndex::new(next_task_index);
    if let Some(offset) = result_slots[next_task_index..]
        .iter()
        .position(Option::is_some)
    {
        return Some((
            expected,
            Some(StandardTranslationTaskIndex::new(next_task_index + offset)),
        ));
    }
    (!cancelled && next_task_index < result_slots.len()).then_some((expected, None))
}

enum TaskExecutionCompletion<E> {
    Outcome {
        outcome: Box<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
    },
    Failed {
        task_index: StandardTranslationTaskIndex,
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: Option<SafeDiagnostic>,
        cancelled: bool,
    },
}

/// 使用四个业务能力和不可失败观察入口编排一次标准资产翻译。
pub(crate) struct StandardTranslationService<R, P, E, S, J, K = NoOpTranslationTaskRecordSink> {
    asset_reader: R,
    task_planner: P,
    task_executor: E,
    result_store: S,
    event_log: J,
    task_records: K,
    cancellation: CooperativeCancellation,
    #[cfg(test)]
    stop_admission_notify: Option<Arc<tokio::sync::Notify>>,
}

impl<R, P, E, S, J> StandardTranslationService<R, P, E, S, J, NoOpTranslationTaskRecordSink> {
    pub(crate) fn new(
        asset_reader: R,
        task_planner: P,
        task_executor: E,
        result_store: S,
        event_log: J,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            asset_reader,
            task_planner,
            task_executor,
            result_store,
            event_log,
            task_records: NoOpTranslationTaskRecordSink,
            cancellation,
            #[cfg(test)]
            stop_admission_notify: None,
        }
    }
}

impl<R, P, E, S, J, K> StandardTranslationService<R, P, E, S, J, K> {
    pub(crate) fn with_task_record_sink<N>(
        self,
        task_records: N,
    ) -> StandardTranslationService<R, P, E, S, J, N> {
        StandardTranslationService {
            asset_reader: self.asset_reader,
            task_planner: self.task_planner,
            task_executor: self.task_executor,
            result_store: self.result_store,
            event_log: self.event_log,
            task_records,
            cancellation: self.cancellation,
            #[cfg(test)]
            stop_admission_notify: self.stop_admission_notify,
        }
    }
}

impl<R, P, E, S, J, K> StandardTranslationService<R, P, E, S, J, K>
where
    P: StandardTranslationTaskPlanner,
    E: StandardTranslationTaskExecutor<Profile = P::Profile>,
    S: StandardTranslationResultStore,
    J: StandardTranslationLog,
    K: TranslationTaskRecordSink,
{
    fn record_task(&self, document: impl FnOnce() -> TranslationTaskRecordDocument) {
        if self.task_records.enabled() {
            self.task_records.submit(document());
        }
    }

    async fn execute_planned_task(
        &self,
        profile: &P::Profile,
        task: &TranslationTaskBlock,
        total_tasks: usize,
    ) -> TaskExecutionCompletion<E::Error> {
        let task_index = task.index();
        self.event_log
            .emit(StandardTranslationLogEvent::TaskStarted {
                task_index,
                total_tasks,
            });

        match self.task_executor.execute(profile, task).await {
            Ok(execution) => {
                let (outcome, evidence) = execution.into_parts();
                TaskExecutionCompletion::Outcome {
                    outcome: Box::new(outcome),
                    evidence,
                }
            }
            Err(failure) => {
                let (source, evidence, diagnostic, cancelled) = failure.into_parts();
                TaskExecutionCompletion::Failed {
                    task_index,
                    source,
                    evidence,
                    diagnostic,
                    cancelled,
                }
            }
        }
    }

    async fn prepare_scheduled_task(
        &self,
        completion: ScheduledTaskCompletion<E::Error>,
    ) -> PreparedScheduledTaskCompletion<E::Error, S::Error, S::PreparedCommit> {
        let ScheduledTaskCompletion {
            task,
            execution,
            in_flight_permit,
        } = completion;
        let task_index = task.index();
        let execution = match execution {
            TaskExecutionCompletion::Outcome { outcome, evidence } => {
                let outcome: Arc<TranslationTaskOutcome> = Arc::from(outcome);
                let prepared_commit =
                    if outcome.task_index() == task_index && !outcome.accepted().is_empty() {
                        Some(self.result_store.prepare_commit(Arc::clone(&outcome)).await)
                    } else {
                        None
                    };
                PreparedTaskExecutionCompletion::Outcome {
                    outcome,
                    evidence,
                    prepared_commit,
                }
            }
            TaskExecutionCompletion::Failed {
                task_index,
                source,
                evidence,
                diagnostic,
                cancelled,
            } => PreparedTaskExecutionCompletion::Failed {
                task_index,
                source,
                evidence,
                diagnostic,
                cancelled,
            },
        };
        PreparedScheduledTaskCompletion {
            task,
            execution,
            in_flight_permit,
        }
    }
}

impl<R, P, E, S, J, K> StandardTranslation for StandardTranslationService<R, P, E, S, J, K>
where
    R: StandardTranslationAssetReader,
    P: StandardTranslationTaskPlanner,
    E: StandardTranslationTaskExecutor<Profile = P::Profile>,
    S: StandardTranslationResultStore,
    J: StandardTranslationLog,
    K: TranslationTaskRecordSink,
{
    type Profile = P::Profile;
    type Error = StandardTranslationServiceError<R::Error, P::Error, E::Error, S::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> Result<OperationCompletion<StandardTranslationRunReport>, Self::Error> {
        let operation: Result<_, Self::Error> = async {
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let corpus = self
                .asset_reader
                .read(project)
                .await
                .map_err(StandardTranslationServiceError::ReadAssets)?;
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let plan = self
                .task_planner
                .plan(project, profile, corpus, input)
                .await
                .map_err(StandardTranslationServiceError::PlanTasks)?;
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let (semantics, preparation, tasks) = plan.into_parts();
            let planning_failures = preparation.planning_failures().to_vec();
            let mut report = StandardTranslationRunReport::with_reconciliation(
                tasks.len(),
                preparation.retained(),
                preparation.invalidated(),
                preparation.not_applicable(),
                preparation.reused(),
            )
            .with_semantics(semantics);
            report.record_planning_failures(&planning_failures);

        self.result_store
            .apply_preparation(project, preparation)
            .await
            .map_err(StandardTranslationServiceError::ApplyPreparation)?;

        if !planning_failures.is_empty() {
            self.event_log
                .emit(StandardTranslationLogEvent::PlanningUnresolved {
                    units: planning_failures.len(),
                });
        }

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let max_concurrent_requests = profile.max_concurrent_requests().get();
        let task_count = tasks.len();
        let execution_worker_count =
            execution_worker_count(task_count, max_concurrent_requests);
        let local_in_flight_window = Arc::new(Semaphore::new(local_in_flight_window_count(
            task_count,
            max_concurrent_requests,
        )));
        let pending_tasks = Arc::new(std::sync::Mutex::new(
            tasks.into_iter().collect::<VecDeque<_>>(),
        ));
        let stop_admission = Arc::new(AtomicBool::new(false));
        let (execution_sender, mut execution_receiver) =
            tokio::sync::mpsc::unbounded_channel::<ScheduledTaskCompletion<E::Error>>();
        let (prepared_sender, mut prepared_receiver) = tokio::sync::mpsc::unbounded_channel::<
            PreparedScheduledTaskCompletion<E::Error, S::Error, S::PreparedCommit>,
        >();

        let execution_lane = {
            let pending_tasks = Arc::clone(&pending_tasks);
            let stop_admission = Arc::clone(&stop_admission);
            let local_in_flight_window = Arc::clone(&local_in_flight_window);
            async move {
                let mut workers = FuturesUnordered::new();
                for _ in 0..execution_worker_count {
                    let pending_tasks = Arc::clone(&pending_tasks);
                    let stop_admission = Arc::clone(&stop_admission);
                    let local_in_flight_window = Arc::clone(&local_in_flight_window);
                    let execution_sender = execution_sender.clone();
                    workers.push(async move {
                        loop {
                            if stop_admission.load(Ordering::Acquire)
                                || self.cancellation.is_requested()
                            {
                                break;
                            }

                            let in_flight_permit = Arc::clone(&local_in_flight_window)
                                .acquire_owned()
                                .await
                                .expect("Standard 本地在途窗口在运行期间不得关闭");
                            if stop_admission.load(Ordering::Acquire)
                                || self.cancellation.is_requested()
                            {
                                break;
                            }

                            // 临界区只移动一个已经物化的任务，不跨 await 持锁。
                            let task = pending_tasks
                                .lock()
                                .expect("Standard 待执行任务队列锁不应中毒")
                                .pop_front();
                            let Some(task) = task else {
                                break;
                            };

                            let execution =
                                self.execute_planned_task(profile, &task, task_count).await;
                            if matches!(
                                &execution,
                                TaskExecutionCompletion::Failed {
                                    cancelled: false,
                                    ..
                                }
                            ) {
                                stop_admission.store(true, Ordering::Release);
                            }
                            if execution_sender
                                .send(ScheduledTaskCompletion {
                                    task,
                                    execution,
                                    in_flight_permit,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
                drop(execution_sender);
                while workers.next().await.is_some() {}
            }
        };

        let preparation_lane = {
            let stop_admission = Arc::clone(&stop_admission);
            #[cfg(test)]
            let stop_admission_notify = self.stop_admission_notify.clone();
            async move {
                let mut preparations = FuturesUnordered::new();
                let mut execution_closed = false;

                loop {
                    if execution_closed && preparations.is_empty() {
                        break;
                    }
                    tokio::select! {
                        completion = execution_receiver.recv(), if !execution_closed => {
                            match completion {
                                Some(completion) => {
                                    preparations.push(self.prepare_scheduled_task(completion));
                                }
                                None => execution_closed = true,
                            }
                        }
                        prepared = preparations.next(), if !preparations.is_empty() => {
                            let prepared = prepared
                                .expect("非空的 Standard 准备任务集合必须返回结果");
                            if prepared.requires_stop_admission() {
                                stop_admission.store(true, Ordering::Release);
                                #[cfg(test)]
                                if let Some(notify) = &stop_admission_notify {
                                    notify.notify_one();
                                }
                            }
                            if prepared_sender.send(prepared).is_err() {
                                break;
                            }
                        }
                    }
                }
                drop(prepared_sender);
            }
        };

        let finalization_lane = {
            let stop_admission = Arc::clone(&stop_admission);
            async move {
                let mut result_slots = std::iter::repeat_with(|| None)
                    .take(task_count)
                    .collect::<Vec<
                        Option<
                            PreparedScheduledTaskCompletion<E::Error, S::Error, S::PreparedCommit>,
                        >,
                    >>();
                let mut next_task_index = 0;
                let mut primary_failure: Option<Self::Error> = None;

                while let Some(completion) = prepared_receiver.recv().await {
                    let completed_index = completion.task_index().get();
                    let slot = result_slots
                        .get_mut(completed_index)
                        .expect("Executor 必须返回计划中的任务序号");
                    assert!(slot.is_none(), "Executor 不得重复返回同一任务");
                    *slot = Some(completion);

                    while let Some(completion) =
                        result_slots.get_mut(next_task_index).and_then(Option::take)
                    {
                        next_task_index += 1;
                        let PreparedScheduledTaskCompletion {
                            task,
                            execution,
                            in_flight_permit,
                        } = completion;
                        let scheduled_task_index = task.index();

                        match execution {
                            PreparedTaskExecutionCompletion::Failed {
                                task_index,
                                source,
                                evidence,
                                diagnostic,
                                cancelled,
                            } => {
                                let attempts = NonZeroUsize::new(evidence.attempt_count());
                                if cancelled {
                                    self.event_log.emit(
                                        StandardTranslationLogEvent::TaskFinished {
                                            task_index,
                                            outcome:
                                                StandardTranslationLogTaskOutcome::NotCommitted,
                                            attempts,
                                            retry_exhausted: false,
                                            diagnostic: diagnostic.clone(),
                                        },
                                    );
                                    self.record_task(|| {
                                        TranslationTaskRecordDocument::new(
                                            task_count,
                                            task,
                                            evidence,
                                            TranslationTaskRecordFinalState::CancelledNoChanges {
                                                outcome: None,
                                            },
                                        )
                                    });
                                    drop(source);
                                } else {
                                    stop_admission.store(true, Ordering::Release);
                                    self.event_log.emit(
                                        StandardTranslationLogEvent::TaskFinished {
                                            task_index,
                                            outcome: StandardTranslationLogTaskOutcome::ExecutionFailed,
                                            attempts,
                                            retry_exhausted: false,
                                            diagnostic: diagnostic.clone(),
                                        },
                                    );
                                    self.record_task(|| {
                                        TranslationTaskRecordDocument::new(
                                            task_count,
                                            task,
                                            evidence,
                                            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                                                diagnostic,
                                            },
                                        )
                                    });
                                    if primary_failure.is_none() {
                                        primary_failure =
                                            Some(StandardTranslationServiceError::ExecuteTask {
                                                task_index,
                                                source,
                                            });
                                    }
                                }
                            }
                            PreparedTaskExecutionCompletion::Outcome {
                                outcome,
                                evidence,
                                prepared_commit,
                            } => {
                                let task_index = outcome.task_index();
                                if task_index != scheduled_task_index {
                                    stop_admission.store(true, Ordering::Release);
                                    self.event_log.emit(
                                        StandardTranslationLogEvent::TaskFinished {
                                            task_index: scheduled_task_index,
                                            outcome:
                                                StandardTranslationLogTaskOutcome::InvalidResult,
                                            attempts: Some(outcome.attempts()),
                                            retry_exhausted: false,
                                            diagnostic: None,
                                        },
                                    );
                                    self.record_task(|| {
                                        TranslationTaskRecordDocument::new(
                                            task_count,
                                            task,
                                            evidence,
                                            TranslationTaskRecordFinalState::InvalidResultNoChanges {
                                                outcome: Arc::clone(&outcome),
                                            },
                                        )
                                    });
                                    if primary_failure.is_none() {
                                        primary_failure = Some(
                                            StandardTranslationServiceError::InvalidTaskResultSequence {
                                                expected_task_index: scheduled_task_index,
                                                actual_task_index: Some(task_index),
                                            },
                                        );
                                    }
                                } else if self.cancellation.is_requested() {
                                    self.event_log.emit(
                                        StandardTranslationLogEvent::TaskFinished {
                                            task_index,
                                            outcome:
                                                StandardTranslationLogTaskOutcome::NotCommitted,
                                            attempts: Some(outcome.attempts()),
                                            retry_exhausted: false,
                                            diagnostic: None,
                                        },
                                    );
                                    self.record_task(|| {
                                        TranslationTaskRecordDocument::new(
                                            task_count,
                                            task,
                                            evidence,
                                            TranslationTaskRecordFinalState::CancelledNoChanges {
                                                outcome: Some(Arc::clone(&outcome)),
                                            },
                                        )
                                    });
                                } else if primary_failure.is_some() {
                                    self.event_log.emit(
                                        StandardTranslationLogEvent::TaskFinished {
                                            task_index,
                                            outcome:
                                                StandardTranslationLogTaskOutcome::NotCommitted,
                                            attempts: Some(outcome.attempts()),
                                            retry_exhausted: false,
                                            diagnostic: None,
                                        },
                                    );
                                    self.record_task(|| {
                                        TranslationTaskRecordDocument::new(
                                            task_count,
                                            task,
                                            evidence,
                                            TranslationTaskRecordFinalState::NotCommittedAfterEarlierFailure {
                                                outcome: Arc::clone(&outcome),
                                            },
                                        )
                                    });
                                } else {
                                    let commit_failure = match prepared_commit {
                                        Some(Ok(prepared)) => {
                                            self.result_store
                                                .commit_prepared(project, prepared)
                                                .await
                                                .err()
                                                .map(|failure| {
                                                    (
                                                        TranslationTaskCommitPhase::Transaction,
                                                        failure,
                                                    )
                                                })
                                        }
                                        Some(Err(failure)) => Some((
                                            TranslationTaskCommitPhase::Preparation,
                                            failure,
                                        )),
                                        None => None,
                                    };
                                    if let Some((phase, failure)) = commit_failure {
                                        let (commit_source, impact, diagnostic) =
                                            failure.into_parts();
                                        stop_admission.store(true, Ordering::Release);
                                        self.event_log.emit(
                                            StandardTranslationLogEvent::TaskFinished {
                                                task_index,
                                                outcome:
                                                    StandardTranslationLogTaskOutcome::CommitFailed,
                                                attempts: Some(outcome.attempts()),
                                                retry_exhausted: false,
                                                diagnostic: diagnostic.clone(),
                                            },
                                        );
                                        let state = match impact {
                                            TranslationTaskCommitFailureImpact::NotApplied => {
                                                TranslationTaskRecordFinalState::CommitNotApplied {
                                                    outcome: Arc::clone(&outcome),
                                                    phase,
                                                    diagnostic,
                                                }
                                            }
                                            TranslationTaskCommitFailureImpact::OutcomeUnknown => {
                                                TranslationTaskRecordFinalState::CommitOutcomeUnknown {
                                                    outcome: Arc::clone(&outcome),
                                                    diagnostic,
                                                }
                                            }
                                        };
                                        self.record_task(|| {
                                            TranslationTaskRecordDocument::new(
                                                task_count,
                                                task,
                                                evidence,
                                                state,
                                            )
                                        });
                                        primary_failure =
                                            Some(StandardTranslationServiceError::CommitTask {
                                                task_index,
                                                source: commit_source,
                                            });
                                    } else {
                                        report.record(&outcome);
                                        let retry_exhausted = matches!(
                                            outcome.as_ref(),
                                            TranslationTaskOutcome::Unavailable {
                                                reason:
                                                    TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. }
                                                    | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum { .. },
                                                ..
                                            }
                                        );
                                        let observed_outcome = match outcome.as_ref() {
                                            TranslationTaskOutcome::Complete { .. } => {
                                                StandardTranslationLogTaskOutcome::Complete
                                            }
                                            TranslationTaskOutcome::Partial { .. } => {
                                                StandardTranslationLogTaskOutcome::Partial
                                            }
                                            TranslationTaskOutcome::Unavailable { .. } => {
                                                StandardTranslationLogTaskOutcome::Unavailable
                                            }
                                        };
                                        self.event_log.emit(
                                            StandardTranslationLogEvent::TaskFinished {
                                                task_index,
                                                outcome: observed_outcome,
                                                attempts: Some(outcome.attempts()),
                                                retry_exhausted,
                                                diagnostic: outcome.request_diagnostic().cloned(),
                                            },
                                        );
                                        let state = match outcome.as_ref() {
                                            TranslationTaskOutcome::Complete { .. } => {
                                                TranslationTaskRecordFinalState::CompleteCommitted {
                                                    outcome: Arc::clone(&outcome),
                                                }
                                            }
                                            TranslationTaskOutcome::Partial { .. } => {
                                                TranslationTaskRecordFinalState::PartialCommitted {
                                                    outcome: Arc::clone(&outcome),
                                                }
                                            }
                                            TranslationTaskOutcome::Unavailable { .. } => {
                                                TranslationTaskRecordFinalState::UnavailableNoChanges {
                                                    outcome: Arc::clone(&outcome),
                                                }
                                            }
                                        };
                                        self.record_task(|| {
                                            TranslationTaskRecordDocument::new(
                                                task_count,
                                                task,
                                                evidence,
                                                state,
                                            )
                                        });
                                    }
                                }
                            }
                        }
                        drop(in_flight_permit);
                    }
                }
                if primary_failure.is_none()
                    && let Some((expected_task_index, actual_task_index)) =
                        closed_result_sequence_violation(
                            &result_slots,
                            next_task_index,
                            self.cancellation.is_requested(),
                        )
                {
                    primary_failure =
                        Some(StandardTranslationServiceError::InvalidTaskResultSequence {
                            expected_task_index,
                            actual_task_index,
                        });
                }
                if let Some(primary) = primary_failure {
                    return Err(primary);
                }

                if self.cancellation.is_requested() {
                    Ok(OperationCompletion::Cancelled)
                } else {
                    Ok(OperationCompletion::Completed(report))
                }
            }
        };

            // 三条流水线各自持有并发集合、任务终态和记录文档。装箱只缩小上层命令
            // Future 的静态栈布局，不改变并发、背压或生命周期语义。
            let (_, _, finalization) = future::join3(
                Box::pin(execution_lane),
                Box::pin(preparation_lane),
                Box::pin(finalization_lane),
            )
            .await;
            finalization
        }
        .await;

        let storage_finalization = self.result_store.finalize().await;
        match (operation, storage_finalization) {
            (Ok(completion), Ok(())) => Ok(completion),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(source)) => {
                Err(StandardTranslationServiceError::FinalizeResultStore(source))
            }
            (Err(primary), Err(finalization)) => {
                Err(StandardTranslationServiceError::OperationAndFinalization {
                    primary: Box::new(primary),
                    finalization,
                })
            }
        }
    }
}

/// Standard 在直接依赖边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum StandardTranslationServiceError<R, P, E, S> {
    ReadAssets(R),
    PlanTasks(P),
    ApplyPreparation(S),
    ExecuteTask {
        task_index: StandardTranslationTaskIndex,
        source: E,
    },
    CommitTask {
        task_index: StandardTranslationTaskIndex,
        source: S,
    },
    InvalidTaskResultSequence {
        expected_task_index: StandardTranslationTaskIndex,
        actual_task_index: Option<StandardTranslationTaskIndex>,
    },
    FinalizeResultStore(S),
    OperationAndFinalization {
        primary: Box<StandardTranslationServiceError<R, P, E, S>>,
        finalization: S,
    },
}

impl<R, P, E, S> fmt::Display for StandardTranslationServiceError<R, P, E, S>
where
    R: fmt::Display,
    P: fmt::Display,
    E: fmt::Display,
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadAssets(source) => write!(formatter, "无法读取标准翻译资产：{source}"),
            Self::PlanTasks(source) => write!(formatter, "无法建立标准翻译计划：{source}"),
            Self::ApplyPreparation(source) => {
                write!(formatter, "无法应用标准翻译准备：{source}")
            }
            Self::ExecuteTask { task_index, source } => {
                write!(formatter, "标准翻译任务 {task_index} 执行失败：{source}")
            }
            Self::CommitTask { task_index, source } => {
                write!(formatter, "标准翻译任务 {task_index} 提交失败：{source}")
            }
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
            } => write!(
                formatter,
                "标准翻译结果序列损坏：期待任务 {expected_task_index}，却收到任务 {actual_task_index}"
            ),
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: None,
            } => write!(
                formatter,
                "标准翻译结果序列不完整：执行通道在任务 {expected_task_index} 返回前关闭"
            ),
            Self::FinalizeResultStore(source) => {
                write!(formatter, "标准翻译数据库会话收尾失败：{source}")
            }
            Self::OperationAndFinalization {
                primary,
                finalization,
            } => write!(
                formatter,
                "{primary}；标准翻译数据库会话收尾也失败：{finalization}"
            ),
        }
    }
}

impl<R, P, E, S> Error for StandardTranslationServiceError<R, P, E, S>
where
    R: Error + 'static,
    P: Error + 'static,
    E: Error + 'static,
    S: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadAssets(source) => Some(source),
            Self::PlanTasks(source) => Some(source),
            Self::ApplyPreparation(source) => Some(source),
            Self::ExecuteTask { source, .. } => Some(source),
            Self::CommitTask { source, .. } => Some(source),
            Self::FinalizeResultStore(source) => Some(source),
            Self::OperationAndFinalization { primary, .. } => Some(primary.as_ref()),
            Self::InvalidTaskResultSequence { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguageText,
    };
    use crate::llm::ChatMessageRole;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    fn test_language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言应合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
        )
    }

    #[test]
    fn measured_strategy_keeps_two_extra_windows_beyond_request_workers() {
        assert_eq!(execution_worker_count(0, 8), 0);
        assert_eq!(execution_worker_count(3, 8), 3);
        assert_eq!(execution_worker_count(100, 8), 8);
        assert_eq!(execution_worker_count(usize::MAX, usize::MAX), usize::MAX);

        assert_eq!(local_in_flight_window_count(0, 8), 0);
        assert_eq!(local_in_flight_window_count(3, 8), 3);
        assert_eq!(local_in_flight_window_count(100, 8), 24);
        assert_eq!(
            local_in_flight_window_count(usize::MAX, usize::MAX),
            usize::MAX
        );
    }

    #[test]
    fn task_block_keeps_only_the_execution_contract() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(10)],
        );
        let description_identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法")),
            TextUnitContent::Value("装备后提升 \\N[1] 的攻击力".to_owned()),
            "{}",
        );
        let placeholder = AppliedPlaceholder::new(
            "<att:actor-name:0>",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            "rpg_maker.event.control_character.actor_name",
            PlaceholderSegment::Whole,
        );
        let block = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(4),
            test_language_pair(),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Translation contract"),
                ChatMessage::new(ChatMessageRole::User, "# Content\n\n..."),
            ],
            vec![ExpectedTranslationOutput::new(
                1,
                description_identity,
                Vec::new(),
                ExpectedTranslationValidation::new(
                    ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                    "装备后提升 <att:actor-name:0> 的攻击力",
                    vec![placeholder],
                    test_language_analysis(),
                ),
                test_state_context(1),
                Vec::new(),
            )],
        );

        assert_eq!(block.index(), StandardTranslationTaskIndex::new(4));
        assert_eq!(block.language_pair().source().as_str(), "ja");
        assert_eq!(block.language_pair().target().as_str(), "zh-Hans");
        assert_eq!(
            block.expected_outputs()[0].identity().kind(),
            TextGroupKind::DatabaseEntry
        );
        assert_eq!(
            block.expected_outputs()[0].identity().group_location(),
            &group_location
        );
        assert_eq!(block.messages().len(), 2);
        assert_eq!(block.expected_outputs()[0].id(), 1);
        assert_eq!(
            block.expected_outputs()[0].line_shape(),
            ExpectedLineShape::Aligned(NonZeroUsize::MIN)
        );
        assert_eq!(
            block.expected_outputs()[0].applied_placeholders()[0].scope(),
            "rpg_maker.event.control_character.actor_name"
        );
        assert_eq!(
            block.expected_outputs()[0].applied_placeholders()[0].original(),
            "\\N[1]"
        );
    }

    #[test]
    #[should_panic(expected = "Standard 计划中的 TaskBlock 序号必须按自然计划顺序从零连续编号")]
    fn plan_establishes_the_contiguous_task_ordinal_invariant_before_execution() {
        StandardTranslationPlan::new(
            Arc::new(super::super::semantics::ResolvedTranslationSemantics::for_test()),
            empty_preparation(),
            vec![TranslationTaskBlock::new(
                StandardTranslationTaskIndex::new(1),
                test_language_pair(),
                Vec::new(),
                Vec::new(),
            )],
        );
    }

    #[test]
    fn translation_state_preserves_content_kind_and_line_boundaries() {
        let context = test_state_context(7);
        let value = TextUnitContent::Value("甲\n乙".to_owned());
        let two_lines = TextUnitContent::Lines(vec!["甲".to_owned(), "乙".to_owned()]);
        let one_line = TextUnitContent::Lines(vec!["甲乙".to_owned()]);

        assert_ne!(context.finish(&value), context.finish(&two_lines));
        assert_ne!(context.finish(&two_lines), context.finish(&one_line));
    }

    #[derive(Clone, Copy)]
    struct FakeProfile {
        max_concurrent_requests: NonZeroUsize,
    }

    impl StandardTranslationProfile for FakeProfile {
        fn max_concurrent_requests(&self) -> NonZeroUsize {
            self.max_concurrent_requests
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        Read,
        Plan,
        Prepare,
        LogPlanningFailure,
        LogTaskStarted(usize),
        Execute(usize),
        Complete(usize),
        PrepareCommit(usize),
        PreparedCommit(usize),
        CommitAttempt(usize),
        Commit(usize),
        LogTask(usize),
        LogCommitFailure(usize),
        LogNotCommitted(usize),
        LogExecutionFailure(usize),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeOutcomeKind {
        Complete,
        Partial,
        Unavailable,
        RetryExhausted,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RecordedTaskState {
        CompleteCommitted,
        PartialCommitted,
        UnavailableNoChanges,
        ExecutionFailedNoChanges,
        CommitPreparationFailed,
        CommitNotApplied,
        CommitOutcomeUnknown,
        NotCommittedAfterEarlierFailure,
        InvalidResultNoChanges,
        CancelledNoChanges,
    }

    #[derive(Clone)]
    struct FakeTaskRecordSink {
        records: Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
    }

    fn recorded_task_state(document: &TranslationTaskRecordDocument) -> RecordedTaskState {
        match document.final_state() {
            TranslationTaskRecordFinalState::CompleteCommitted { .. } => {
                RecordedTaskState::CompleteCommitted
            }
            TranslationTaskRecordFinalState::PartialCommitted { .. } => {
                RecordedTaskState::PartialCommitted
            }
            TranslationTaskRecordFinalState::UnavailableNoChanges { .. } => {
                RecordedTaskState::UnavailableNoChanges
            }
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges { .. } => {
                RecordedTaskState::ExecutionFailedNoChanges
            }
            TranslationTaskRecordFinalState::CommitNotApplied {
                phase: TranslationTaskCommitPhase::Preparation,
                ..
            } => RecordedTaskState::CommitPreparationFailed,
            TranslationTaskRecordFinalState::CommitNotApplied {
                phase: TranslationTaskCommitPhase::Transaction,
                ..
            } => RecordedTaskState::CommitNotApplied,
            TranslationTaskRecordFinalState::CommitOutcomeUnknown { .. } => {
                RecordedTaskState::CommitOutcomeUnknown
            }
            TranslationTaskRecordFinalState::NotCommittedAfterEarlierFailure { .. } => {
                RecordedTaskState::NotCommittedAfterEarlierFailure
            }
            TranslationTaskRecordFinalState::InvalidResultNoChanges { .. } => {
                RecordedTaskState::InvalidResultNoChanges
            }
            TranslationTaskRecordFinalState::CancelledNoChanges { .. } => {
                RecordedTaskState::CancelledNoChanges
            }
        }
    }

    impl TranslationTaskRecordSink for FakeTaskRecordSink {
        fn submit(&self, document: TranslationTaskRecordDocument) {
            let state = recorded_task_state(&document);
            self.records
                .lock()
                .expect("任务记录锁不应中毒")
                .push((document.task_index().get(), state));
        }
    }

    #[derive(Clone)]
    struct FakeReader {
        events: Arc<Mutex<Vec<Event>>>,
        failure: bool,
    }

    impl StandardTranslationAssetReader for FakeReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
        ) -> Result<StandardTranslationCorpus, Self::Error> {
            record(&self.events, Event::Read);
            if self.failure {
                Err(FakeError("read"))
            } else {
                Ok(StandardTranslationCorpus::new(Vec::new()))
            }
        }
    }

    #[derive(Clone)]
    struct FakePlanner {
        events: Arc<Mutex<Vec<Event>>>,
        inputs: Arc<Mutex<Vec<StandardTranslationInput>>>,
        preparation: TranslationPlanPreparation,
        task_count: usize,
        failure: bool,
    }

    impl StandardTranslationTaskPlanner for FakePlanner {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn plan(
            &self,
            _project: &OpenedProject,
            _profile: &Self::Profile,
            _corpus: StandardTranslationCorpus,
            input: StandardTranslationInput,
        ) -> Result<StandardTranslationPlan, Self::Error> {
            record(&self.events, Event::Plan);
            self.inputs
                .lock()
                .expect("计划输入记录锁不应中毒")
                .push(input);
            if self.failure {
                return Err(FakeError("plan"));
            }

            let tasks: Vec<TranslationTaskBlock> = (0..self.task_count)
                .map(|index| {
                    let expected_outputs = vec![
                        expected_output(index, 1, true),
                        expected_output(index, 2, false),
                    ];
                    TranslationTaskBlock::new(
                        StandardTranslationTaskIndex::new(index),
                        test_language_pair(),
                        vec![ChatMessage::new(
                            ChatMessageRole::User,
                            format!("# Task {index}"),
                        )],
                        expected_outputs,
                    )
                })
                .collect();
            Ok(StandardTranslationPlan::new(
                Arc::new(super::super::semantics::ResolvedTranslationSemantics::for_test()),
                self.preparation.clone(),
                tasks,
            ))
        }
    }

    #[derive(Clone)]
    struct FakeExecutor {
        events: Arc<Mutex<Vec<Event>>>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        yields_by_task: Arc<Vec<usize>>,
        fail_at: Arc<Vec<usize>>,
        outcome_kinds: Arc<Vec<FakeOutcomeKind>>,
        cancel_on_start: Option<(usize, CooperativeCancellation)>,
        block_at: Option<(usize, Arc<Semaphore>)>,
        outcome_index_at: Option<(usize, usize)>,
    }

    impl StandardTranslationTaskExecutor for FakeExecutor {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn execute(
            &self,
            _profile: &Self::Profile,
            task: &TranslationTaskBlock,
        ) -> Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>>
        {
            let task_index = task.index();
            let index = task_index.get();
            record(&self.events, Event::Execute(index));
            if let Some((cancel_index, cancellation)) = &self.cancel_on_start
                && *cancel_index == index
            {
                cancellation.request();
            }
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);

            if let Some((blocked_index, gate)) = &self.block_at
                && *blocked_index == index
            {
                gate.acquire().await.expect("测试任务闸门不得关闭").forget();
            }

            for _ in 0..self.yields_by_task.get(index).copied().unwrap_or(1) {
                tokio::task::yield_now().await;
            }

            self.active.fetch_sub(1, Ordering::SeqCst);
            record(&self.events, Event::Complete(index));
            if self.fail_at.contains(&index) {
                let cancelled = self
                    .cancel_on_start
                    .as_ref()
                    .is_some_and(|(_, cancellation)| cancellation.is_requested());
                Err(TranslationTaskExecutionFailure::new(
                    FakeError("execute"),
                    TranslationTaskExecutionEvidence::synthetic(NonZeroUsize::MIN),
                    None,
                    cancelled,
                ))
            } else {
                let outcome_task_index = self
                    .outcome_index_at
                    .filter(|(source_index, _)| *source_index == index)
                    .map_or(task_index, |(_, outcome_index)| {
                        StandardTranslationTaskIndex::new(outcome_index)
                    });
                Ok(TranslationTaskExecution::synthetic(fake_outcome(
                    outcome_task_index,
                    task.expected_outputs(),
                    self.outcome_kinds
                        .get(index)
                        .copied()
                        .unwrap_or(FakeOutcomeKind::Complete),
                )))
            }
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        events: Arc<Mutex<Vec<Event>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
        finalizations: Arc<AtomicUsize>,
        fail_preparation: bool,
        fail_commit_preparation_at: Arc<Vec<usize>>,
        block_commit_preparation_at: Option<(usize, Arc<Semaphore>)>,
        retained_commit_outcomes: Option<Arc<Mutex<Vec<Arc<TranslationTaskOutcome>>>>>,
        fail_commit_at: Option<usize>,
        unknown_commit_at: Option<usize>,
        fail_finalization: bool,
    }

    impl StandardTranslationResultStore for FakeStore {
        type PreparedCommit = StandardTranslationTaskIndex;
        type Error = FakeError;

        async fn apply_preparation(
            &self,
            _project: &OpenedProject,
            preparation: TranslationPlanPreparation,
        ) -> Result<(), Self::Error> {
            record(&self.events, Event::Prepare);
            self.preparations
                .lock()
                .expect("准备记录锁不应中毒")
                .push(preparation);
            if self.fail_preparation {
                Err(FakeError("prepare"))
            } else {
                Ok(())
            }
        }

        async fn prepare_commit(
            &self,
            outcome: Arc<TranslationTaskOutcome>,
        ) -> Result<Self::PreparedCommit, TranslationTaskCommitFailure<Self::Error>> {
            let task_index = outcome.task_index();
            let index = task_index.get();
            record(&self.events, Event::PrepareCommit(index));
            if let Some(retained) = &self.retained_commit_outcomes {
                retained
                    .lock()
                    .expect("测试保留结果锁不应中毒")
                    .push(Arc::clone(&outcome));
            }
            if let Some((blocked_index, gate)) = &self.block_commit_preparation_at
                && *blocked_index == index
            {
                gate.acquire()
                    .await
                    .expect("测试提交准备闸门不得关闭")
                    .forget();
            }
            if self.fail_commit_preparation_at.contains(&index) {
                Err(TranslationTaskCommitFailure::not_applied(
                    FakeError("prepare-commit"),
                    None,
                ))
            } else {
                record(&self.events, Event::PreparedCommit(index));
                Ok(task_index)
            }
        }

        async fn commit_prepared(
            &self,
            _project: &OpenedProject,
            task_index: StandardTranslationTaskIndex,
        ) -> Result<(), TranslationTaskCommitFailure<Self::Error>> {
            let index = task_index.get();
            record(&self.events, Event::CommitAttempt(index));
            if self.unknown_commit_at == Some(index) {
                Err(TranslationTaskCommitFailure::new(
                    FakeError("commit-unknown"),
                    TranslationTaskCommitFailureImpact::OutcomeUnknown,
                    None,
                ))
            } else if self.fail_commit_at == Some(index) {
                Err(TranslationTaskCommitFailure::not_applied(
                    FakeError("commit"),
                    None,
                ))
            } else {
                record(&self.events, Event::Commit(index));
                Ok(())
            }
        }

        async fn finalize(&self) -> Result<(), Self::Error> {
            self.finalizations.fetch_add(1, Ordering::SeqCst);
            if self.fail_finalization {
                Err(FakeError("finalize"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeEventLog {
        events: Arc<Mutex<Vec<Event>>>,
        records: Arc<Mutex<Vec<StandardTranslationLogEvent>>>,
        started_not_finalized: Arc<AtomicUsize>,
        max_started_not_finalized: Arc<AtomicUsize>,
    }

    impl StandardTranslationLog for FakeEventLog {
        fn emit(&self, event: StandardTranslationLogEvent) {
            match &event {
                StandardTranslationLogEvent::TaskStarted { task_index, .. } => {
                    record(&self.events, Event::LogTaskStarted(task_index.get()));
                    let started = self.started_not_finalized.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_started_not_finalized
                        .fetch_max(started, Ordering::SeqCst);
                }
                StandardTranslationLogEvent::TaskFinished {
                    task_index,
                    outcome,
                    ..
                } => {
                    self.started_not_finalized.fetch_sub(1, Ordering::SeqCst);
                    match outcome {
                        StandardTranslationLogTaskOutcome::Complete
                        | StandardTranslationLogTaskOutcome::Partial
                        | StandardTranslationLogTaskOutcome::Unavailable => {
                            record(&self.events, Event::LogTask(task_index.get()));
                        }
                        StandardTranslationLogTaskOutcome::CommitFailed => {
                            record(&self.events, Event::LogCommitFailure(task_index.get()));
                        }
                        StandardTranslationLogTaskOutcome::NotCommitted => {
                            record(&self.events, Event::LogNotCommitted(task_index.get()));
                        }
                        StandardTranslationLogTaskOutcome::ExecutionFailed
                        | StandardTranslationLogTaskOutcome::InvalidResult => {
                            record(&self.events, Event::LogExecutionFailure(task_index.get()));
                        }
                    }
                }
                StandardTranslationLogEvent::PlanningUnresolved { .. } => {
                    record(&self.events, Event::LogPlanningFailure);
                }
            }
            self.records
                .lock()
                .expect("日志事件记录锁不应中毒")
                .push(event);
        }
    }

    type Service = StandardTranslationService<
        FakeReader,
        FakePlanner,
        FakeExecutor,
        FakeStore,
        FakeEventLog,
        FakeTaskRecordSink,
    >;

    struct Harness {
        service: Service,
        events: Arc<Mutex<Vec<Event>>>,
        planner_inputs: Arc<Mutex<Vec<StandardTranslationInput>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
        log_records: Arc<Mutex<Vec<StandardTranslationLogEvent>>>,
        max_active: Arc<AtomicUsize>,
        started_not_finalized: Arc<AtomicUsize>,
        max_started_not_finalized: Arc<AtomicUsize>,
        finalizations: Arc<AtomicUsize>,
        task_records: Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
        cancellation: CooperativeCancellation,
    }

    fn harness(
        task_count: usize,
        yields_by_task: Vec<usize>,
        read_failure: bool,
        plan_failure: bool,
        preparation_failure: bool,
        execute_failure_at: Option<usize>,
        commit_failure_at: Option<usize>,
    ) -> Harness {
        harness_with_preparation(
            task_count,
            yields_by_task,
            read_failure,
            plan_failure,
            preparation_failure,
            execute_failure_at,
            commit_failure_at,
            empty_preparation(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "测试需要独立控制五个失败阶段与准备内容"
    )]
    fn harness_with_preparation(
        task_count: usize,
        yields_by_task: Vec<usize>,
        read_failure: bool,
        plan_failure: bool,
        preparation_failure: bool,
        execute_failure_at: Option<usize>,
        commit_failure_at: Option<usize>,
        preparation: TranslationPlanPreparation,
    ) -> Harness {
        harness_with_behavior(
            task_count,
            yields_by_task,
            read_failure,
            plan_failure,
            preparation_failure,
            execute_failure_at,
            commit_failure_at,
            preparation,
            vec![FakeOutcomeKind::Complete; task_count],
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "测试需要独立控制业务结果与各技术失败阶段"
    )]
    fn harness_with_behavior(
        task_count: usize,
        yields_by_task: Vec<usize>,
        read_failure: bool,
        plan_failure: bool,
        preparation_failure: bool,
        execute_failure_at: Option<usize>,
        commit_failure_at: Option<usize>,
        preparation: TranslationPlanPreparation,
        outcome_kinds: Vec<FakeOutcomeKind>,
    ) -> Harness {
        let events = Arc::new(Mutex::new(Vec::new()));
        let planner_inputs = Arc::new(Mutex::new(Vec::new()));
        let preparations = Arc::new(Mutex::new(Vec::new()));
        let log_records = Arc::new(Mutex::new(Vec::new()));
        let max_active = Arc::new(AtomicUsize::new(0));
        let started_not_finalized = Arc::new(AtomicUsize::new(0));
        let max_started_not_finalized = Arc::new(AtomicUsize::new(0));
        let finalizations = Arc::new(AtomicUsize::new(0));
        let task_records = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CooperativeCancellation::default();
        Harness {
            service: StandardTranslationService::new(
                FakeReader {
                    events: Arc::clone(&events),
                    failure: read_failure,
                },
                FakePlanner {
                    events: Arc::clone(&events),
                    inputs: Arc::clone(&planner_inputs),
                    preparation,
                    task_count,
                    failure: plan_failure,
                },
                FakeExecutor {
                    events: Arc::clone(&events),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&max_active),
                    yields_by_task: Arc::new(yields_by_task),
                    fail_at: Arc::new(execute_failure_at.into_iter().collect()),
                    outcome_kinds: Arc::new(outcome_kinds),
                    cancel_on_start: None,
                    block_at: None,
                    outcome_index_at: None,
                },
                FakeStore {
                    events: Arc::clone(&events),
                    preparations: Arc::clone(&preparations),
                    finalizations: Arc::clone(&finalizations),
                    fail_preparation: preparation_failure,
                    fail_commit_preparation_at: Arc::new(Vec::new()),
                    block_commit_preparation_at: None,
                    retained_commit_outcomes: None,
                    fail_commit_at: commit_failure_at,
                    unknown_commit_at: None,
                    fail_finalization: false,
                },
                FakeEventLog {
                    events: Arc::clone(&events),
                    records: Arc::clone(&log_records),
                    started_not_finalized: Arc::clone(&started_not_finalized),
                    max_started_not_finalized: Arc::clone(&max_started_not_finalized),
                },
                cancellation.clone(),
            )
            .with_task_record_sink(FakeTaskRecordSink {
                records: Arc::clone(&task_records),
            }),
            events,
            planner_inputs,
            preparations,
            log_records,
            max_active,
            started_not_finalized,
            max_started_not_finalized,
            finalizations,
            task_records,
            cancellation,
        }
    }

    #[tokio::test]
    async fn dispatches_all_stages_and_commits_each_task_in_plan_order() {
        let harness = harness(3, vec![1, 1, 1], false, false, false, None, None);

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(2), input())
                .await
                .expect("标准翻译编排应该成功"),
        );

        let events = events(&harness.events);
        assert_eq!(&events[..3], &[Event::Read, Event::Plan, Event::Prepare]);
        assert_eq!(committed(&events), vec![0, 1, 2]);
        assert_eq!(logged_tasks(&events), vec![0, 1, 2]);
        assert_eq!(events.last(), Some(&Event::LogTask(2)));
        assert_eq!(report.complete_tasks(), 3);
        assert_eq!(report.accepted_decisions(), 6);
        assert_eq!(report.written_locations(), 9);
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::CompleteCommitted),
                (1, RecordedTaskState::CompleteCommitted),
                (2, RecordedTaskState::CompleteCommitted),
            ]
        );
    }

    #[tokio::test]
    async fn planning_unresolved_is_committed_observed_and_counted_without_blocking_good_tasks() {
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            Vec::new(),
            r#"[{"term":"勇者"}]"#.to_owned(),
            r#"[{"pattern":"<BAD>"}]"#.to_owned(),
            0,
            1,
            0,
        )
        .with_test_planning_failures(vec![TranslationPlanningFailure::new(
            translation_identity_at(99, "description"),
            TranslationPlanningFailureReason::PlaceholderProtection {
                message: "实际保护跨度冲突".to_owned(),
            },
        )]);
        let harness =
            harness_with_preparation(1, vec![1], false, false, false, None, None, preparation);

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(1), input())
                .await
                .expect("规划期未解决是正常部分结果"),
        );

        assert_eq!(report.complete_tasks(), 1);
        assert_eq!(report.unresolved_decisions(), 1);
        assert_eq!(report.unresolved_locations(), 1);
        assert_eq!(report.invalidated(), 1);
        assert!(events(&harness.events).contains(&Event::LogPlanningFailure));
        let preparations = harness.preparations.lock().expect("准备记录锁不应中毒");
        assert_eq!(preparations.len(), 1);
        assert_eq!(preparations[0].planning_failures().len(), 1);
        let records = harness.log_records.lock().expect("日志记录锁不应中毒");
        assert!(matches!(
            records.as_slice(),
            [
                StandardTranslationLogEvent::PlanningUnresolved { units: 1 },
                ..
            ]
        ));
    }

    #[tokio::test]
    async fn a_store_retaining_the_shared_outcome_does_not_break_final_observation() {
        let mut harness = harness(1, vec![1], false, false, false, None, None);
        let retained = Arc::new(Mutex::new(Vec::new()));
        harness.service.result_store.retained_commit_outcomes = Some(Arc::clone(&retained));

        harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect("替代 Store 暂存共享结果不应触发 panic 或改变终态");

        assert_eq!(retained.lock().expect("测试保留结果锁不应中毒").len(), 1);
        let final_events = events(&harness.events);
        assert_eq!(committed(&final_events), vec![0]);
        assert_eq!(logged_tasks(&final_events), vec![0]);
    }

    #[tokio::test]
    async fn executor_activity_never_exceeds_the_external_profile_limit() {
        let harness = harness(7, vec![2; 7], false, false, false, None, None);

        harness
            .service
            .run(&project(), &profile(3), input())
            .await
            .expect("并发执行应该成功");

        assert_eq!(harness.max_active.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cancellation_stops_task_refill_and_drains_the_started_task() {
        let mut harness = harness(4, vec![1; 4], false, false, false, None, None);
        harness.service.task_executor.cancel_on_start = Some((0, harness.cancellation.clone()));

        let completion = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect("取消不是技术错误");

        assert_eq!(completion, OperationCompletion::Cancelled);
        let events = events(&harness.events);
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Execute(index) => Some(*index),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [0]
        );
        assert!(committed(&events).is_empty());
        assert!(logged_tasks(&events).is_empty());
        assert!(!events.contains(&Event::LogTask(1)));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::CancelledNoChanges)],
            "每个已启动任务必须恰好交回一个取消终态，未启动任务不得生成记录"
        );
    }

    #[tokio::test]
    async fn out_of_order_completion_is_committed_in_plan_order() {
        let harness = harness(3, vec![4, 1, 2], false, false, false, None, None);

        harness
            .service
            .run(&project(), &profile(3), input())
            .await
            .expect("乱序完成不应改变提交顺序");

        let events = events(&harness.events);
        let completed = completed(&events);
        assert_ne!(completed, vec![0, 1, 2], "测试必须真正造成乱序完成");
        assert_eq!(committed(&events), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn commit_preparation_can_finish_out_of_order_without_reordering_finalization() {
        let mut harness = harness(3, vec![1; 3], false, false, false, None, None);
        let gate = Arc::new(Semaphore::new(0));
        harness.service.result_store.block_commit_preparation_at = Some((0, Arc::clone(&gate)));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(3);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_preparation = async move {
            for _ in 0..10_000 {
                let current = events(&recorded_events);
                if current.contains(&Event::PreparedCommit(1))
                    && current.contains(&Event::PreparedCommit(2))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let before_release = events(&recorded_events);
            assert!(before_release.contains(&Event::PreparedCommit(1)));
            assert!(before_release.contains(&Event::PreparedCommit(2)));
            assert!(!before_release.contains(&Event::PreparedCommit(0)));
            assert!(
                commit_attempts(&before_release).is_empty(),
                "后序准备不得越过首任务执行数据库提交"
            );
            gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_preparation);
        result.expect("释放首任务准备后运行应成功");
        let final_events = events(&harness.events);
        assert_eq!(committed(&final_events), vec![0, 1, 2]);
        assert_eq!(logged_tasks(&final_events), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn http_refills_while_an_earlier_commit_preparation_is_still_running() {
        let mut harness = harness(3, vec![1; 3], false, false, false, None, None);
        let gate = Arc::new(Semaphore::new(0));
        harness.service.result_store.block_commit_preparation_at = Some((0, Arc::clone(&gate)));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(1);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_refill = async move {
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::Complete(1)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let before_release = events(&recorded_events);
            assert!(before_release.contains(&Event::Complete(1)));
            assert!(!before_release.contains(&Event::PreparedCommit(0)));
            assert!(
                before_release.contains(&Event::Execute(2)),
                "顺序提交等待不得占住模型执行许可"
            );
            gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_refill);
        result.expect("释放提交准备后运行应成功");
        assert_eq!(harness.max_started_not_finalized.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn commit_preparation_failure_stops_admission_and_preserves_committed_prefix() {
        let mut harness = harness(8, vec![1; 8], false, false, false, None, None);
        let preparation_gate = Arc::new(Semaphore::new(0));
        harness.service.result_store.block_commit_preparation_at =
            Some((1, Arc::clone(&preparation_gate)));
        harness.service.result_store.fail_commit_preparation_at = Arc::new(vec![3]);
        let stop_notify = Arc::new(tokio::sync::Notify::new());
        harness.service.stop_admission_notify = Some(Arc::clone(&stop_notify));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(2);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_stop = async move {
            stop_notify.notified().await;
            let at_stop = events(&recorded_events);
            assert!(at_stop.contains(&Event::PrepareCommit(3)));
            let admitted_at_stop = at_stop
                .iter()
                .filter(|event| matches!(event, Event::Execute(_)))
                .count();
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                events(&recorded_events)
                    .iter()
                    .filter(|event| matches!(event, Event::Execute(_)))
                    .count(),
                admitted_at_stop,
                "准备失败一经得知，释放较早终态许可也不得继续领取新任务"
            );
            preparation_gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_stop);
        let error = result.expect_err("第四个任务的提交准备失败必须上交");
        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("prepare-commit"),
            } if task_index == StandardTranslationTaskIndex::new(3)
        ));
        let final_events = events(&harness.events);
        assert_eq!(committed(&final_events), vec![0, 1, 2]);
        assert!(final_events.contains(&Event::LogCommitFailure(3)));
        assert!(
            harness
                .task_records
                .lock()
                .expect("任务记录锁不应中毒")
                .contains(&(3, RecordedTaskState::CommitPreparationFailed)),
            "提交准备失败必须保留区别于事务未应用的任务终态"
        );
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn earlier_commit_failure_wins_over_a_later_out_of_order_preparation_failure() {
        let mut harness = harness(4, vec![1; 4], false, false, false, None, Some(0));
        let gate = Arc::new(Semaphore::new(0));
        harness.service.result_store.block_commit_preparation_at = Some((0, Arc::clone(&gate)));
        harness.service.result_store.fail_commit_preparation_at = Arc::new(vec![1]);
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(2);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let release_earlier = async move {
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::PrepareCommit(1)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, release_earlier);
        let error = result.expect_err("最小计划索引的提交错误必须成为主错误");
        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit"),
            } if task_index == StandardTranslationTaskIndex::new(0)
        ));
        let final_events = events(&harness.events);
        assert!(final_events.contains(&Event::LogCommitFailure(0)));
        assert!(final_events.contains(&Event::LogNotCommitted(1)));
        assert!(!final_events.contains(&Event::LogCommitFailure(1)));
    }

    #[tokio::test]
    async fn a_slow_first_task_does_not_stop_later_http_refill() {
        let mut harness = harness(12, vec![1; 12], false, false, false, None, None);
        let gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((0, Arc::clone(&gate)));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(4);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_refill = async move {
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::Complete(11)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let before_release = events(&recorded_events);
            assert!((1..12).all(|index| before_release.contains(&Event::Complete(index))));
            assert!(
                !before_release.contains(&Event::Complete(0)),
                "观察补位时 A 必须仍被闸门阻塞"
            );
            gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_refill);
        result.expect("释放 A 后全部任务应该成功");
        let events = events(&harness.events);
        assert_eq!(committed(&events), (0..12).collect::<Vec<_>>());
        assert_eq!(harness.max_started_not_finalized.load(Ordering::SeqCst), 12);
    }

    #[tokio::test]
    async fn refill_remains_unblocked_after_a_finalized_prefix() {
        let mut harness = harness(20, vec![1; 20], false, false, false, None, None);
        let gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((8, Arc::clone(&gate)));
        let recorded_events = Arc::clone(&harness.events);
        let started_not_finalized = Arc::clone(&harness.started_not_finalized);
        let project = project();
        let profile = profile(4);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_sliding_window = async move {
            for _ in 0..10_000 {
                let current_events = events(&recorded_events);
                if current_events.contains(&Event::LogTask(7))
                    && current_events.contains(&Event::Complete(19))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }

            let before_release = events(&recorded_events);
            assert!(
                before_release.contains(&Event::LogTask(7)),
                "中段慢任务开始前必须已经成功最终化一段前缀"
            );
            assert!(
                (9..=19).all(|index| before_release.contains(&Event::Complete(index))),
                "中段慢任务不得阻止其余 HTTP 工作继续补位"
            );
            assert_eq!(
                started_not_finalized.load(Ordering::SeqCst),
                12,
                "已完成但等待自然顺序提交的结果应留在内存，而不是阻塞模型请求"
            );
            gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_sliding_window);
        result.expect("释放中段慢任务后全部任务应该成功");
        let events = events(&harness.events);
        assert_eq!(committed(&events), (0..20).collect::<Vec<_>>());
        assert_eq!(harness.max_started_not_finalized.load(Ordering::SeqCst), 12);
    }

    #[test]
    fn a_closed_result_channel_reports_buffered_and_trailing_gaps() {
        assert_eq!(
            closed_result_sequence_violation(&[None, Some(())], 0, false),
            Some((
                StandardTranslationTaskIndex::new(0),
                Some(StandardTranslationTaskIndex::new(1)),
            ))
        );
        assert_eq!(
            closed_result_sequence_violation(&[None::<()>], 0, false),
            Some((StandardTranslationTaskIndex::new(0), None))
        );
        assert_eq!(
            closed_result_sequence_violation(&[None::<()>], 0, true),
            None,
            "合作取消允许尚未启动的尾部任务没有结果"
        );
    }

    #[tokio::test]
    async fn mismatched_executor_result_index_is_an_explicit_service_error() {
        let mut harness = harness(1, vec![1], false, false, false, None, None);
        harness.service.task_executor.outcome_index_at = Some((0, 9));

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("执行器返回其他计划位置时不得误报成功或 panic");

        assert!(matches!(
            error,
            StandardTranslationServiceError::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
            } if expected_task_index == StandardTranslationTaskIndex::new(0)
                && actual_task_index == StandardTranslationTaskIndex::new(9)
        ));
        let recorded_events = events(&harness.events);
        assert!(!recorded_events.contains(&Event::CommitAttempt(0)));
        assert!(recorded_events.contains(&Event::LogExecutionFailure(0)));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::InvalidResultNoChanges)]
        );
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn out_of_order_mismatched_result_stops_admission_before_earlier_finalization() {
        let mut harness = harness(8, vec![1; 8], false, false, false, None, None);
        harness.service.task_executor.outcome_index_at = Some((3, 9));
        let execution_gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((1, Arc::clone(&execution_gate)));
        let stop_notify = Arc::new(tokio::sync::Notify::new());
        harness.service.stop_admission_notify = Some(Arc::clone(&stop_notify));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(2);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_stop = async move {
            stop_notify.notified().await;
            let at_stop = events(&recorded_events);
            assert!(at_stop.contains(&Event::Complete(3)));
            let admitted_at_stop = at_stop
                .iter()
                .filter(|event| matches!(event, Event::Execute(_)))
                .count();
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                events(&recorded_events)
                    .iter()
                    .filter(|event| matches!(event, Event::Execute(_)))
                    .count(),
                admitted_at_stop,
                "错序结果已知后，释放较早终态许可也不得继续领取新任务"
            );
            execution_gate.add_permits(1);
        };

        let (result, ()) = tokio::join!(run, observe_stop);
        let error = result.expect_err("错序结果必须成为明确服务错误");
        assert!(matches!(
            error,
            StandardTranslationServiceError::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
            } if expected_task_index == StandardTranslationTaskIndex::new(3)
                && actual_task_index == StandardTranslationTaskIndex::new(9)
        ));
        assert!(
            harness
                .task_records
                .lock()
                .expect("任务记录锁不应中毒")
                .contains(&(3, RecordedTaskState::InvalidResultNoChanges))
        );
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn earliest_plan_index_is_primary_when_execution_failures_finish_out_of_order() {
        let mut harness = harness(4, vec![1; 4], false, false, false, None, None);
        harness.service.task_executor.fail_at = Arc::new(vec![1, 3]);
        let gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((1, Arc::clone(&gate)));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(4);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let release_earlier_failure = async move {
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::Complete(3)) {
                    gate.add_permits(1);
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("较晚序号的失败必须先完成，测试才能验证计划顺序");
        };

        let (result, ()) = tokio::join!(run, release_earlier_failure);
        let error = result.expect_err("两个执行失败必须上交最早计划序号");
        assert!(matches!(
            error,
            StandardTranslationServiceError::ExecuteTask {
                task_index,
                source: FakeError("execute")
            } if task_index == StandardTranslationTaskIndex::new(1)
        ));
        assert_eq!(
            events(&harness.events)
                .into_iter()
                .filter_map(|event| match event {
                    Event::LogExecutionFailure(index) => Some(index),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![1, 3],
            "执行失败终态也必须按计划顺序写入"
        );
    }

    #[tokio::test]
    async fn business_cancellation_discards_the_wakeup_failure_from_a_started_task() {
        let mut harness = harness(1, vec![1], false, false, false, Some(0), None);
        harness.service.task_executor.cancel_on_start = Some((0, harness.cancellation.clone()));

        let completion = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect("业务取消唤醒请求等待不是模型技术错误");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert!(logged_tasks(&events(&harness.events)).is_empty());
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::CancelledNoChanges)]
        );
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn executor_failure_preserves_only_the_committed_prefix() {
        let harness = harness(4, vec![4, 1, 1, 1], false, false, false, Some(1), None);

        let error = harness
            .service
            .run(&project(), &profile(4), input())
            .await
            .expect_err("第二个任务执行应该失败");

        assert!(matches!(
            error,
            StandardTranslationServiceError::ExecuteTask {
                task_index,
                source: FakeError("execute")
            } if task_index == StandardTranslationTaskIndex::new(1)
        ));
        let events = events(&harness.events);
        assert!(
            events.contains(&Event::Complete(2)),
            "测试必须证明后序请求结果已经完成但被丢弃"
        );
        assert_eq!(committed(&events), vec![0]);
        assert!(!events.contains(&Event::CommitAttempt(2)));
        assert!(events.contains(&Event::LogNotCommitted(2)));
        assert!(events.contains(&Event::LogNotCommitted(3)));
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::CompleteCommitted),
                (1, RecordedTaskState::ExecutionFailedNoChanges),
                (2, RecordedTaskState::NotCommittedAfterEarlierFailure),
                (3, RecordedTaskState::NotCommittedAfterEarlierFailure),
            ],
            "执行失败后，所有已启动任务仍必须各自收敛为唯一终态"
        );
    }

    #[tokio::test]
    async fn commit_failure_preserves_only_the_earlier_committed_prefix() {
        let harness = harness_with_behavior(
            4,
            vec![1; 4],
            false,
            false,
            false,
            None,
            Some(1),
            empty_preparation(),
            vec![
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Partial,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
            ],
        );

        let error = harness
            .service
            .run(&project(), &profile(4), input())
            .await
            .expect_err("第二个任务提交应该失败");

        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit")
            } if task_index == StandardTranslationTaskIndex::new(1)
        ));
        let events = events(&harness.events);
        assert_eq!(commit_attempts(&events), vec![0, 1]);
        assert_eq!(committed(&events), vec![0]);
        assert!(events.contains(&Event::LogCommitFailure(1)));
        assert!(!events.contains(&Event::CommitAttempt(2)));
        assert!(events.contains(&Event::LogNotCommitted(2)));
        assert!(events.contains(&Event::LogNotCommitted(3)));

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        assert!(records.iter().any(|event| matches!(
            event,
            StandardTranslationLogEvent::TaskFinished {
                task_index,
                outcome: StandardTranslationLogTaskOutcome::CommitFailed,
                attempts: Some(_),
                retry_exhausted: false,
                diagnostic: None,
            } if *task_index == StandardTranslationTaskIndex::new(1)
        )));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::CompleteCommitted),
                (1, RecordedTaskState::CommitNotApplied),
                (2, RecordedTaskState::NotCommittedAfterEarlierFailure),
                (3, RecordedTaskState::NotCommittedAfterEarlierFailure),
            ]
        );
    }

    #[tokio::test]
    async fn unknown_commit_outcome_is_preserved_as_its_own_task_record_terminal_state() {
        let mut harness = harness(1, vec![1], false, false, false, None, None);
        harness.service.result_store.unknown_commit_at = Some(0);

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("提交结果未知必须上交技术失败");

        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit-unknown")
            } if task_index == StandardTranslationTaskIndex::new(0)
        ));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::CommitOutcomeUnknown)]
        );
    }

    #[tokio::test]
    async fn commit_failure_stops_admission_without_waiting_on_observability() {
        let mut harness = harness(
            6,
            vec![1, 1, 10_000, 1, 1, 1],
            false,
            false,
            false,
            None,
            Some(0),
        );
        let executor_gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((1, Arc::clone(&executor_gate)));
        let recorded_events = Arc::clone(&harness.events);
        let project = project();
        let profile = profile(2);
        let input = input();

        let run = harness.service.run(&project, &profile, input);
        let observe_stop = async move {
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::LogCommitFailure(0)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                events(&recorded_events).contains(&Event::LogCommitFailure(0)),
                "测试必须先观察到提交失败业务事实"
            );

            executor_gate.add_permits(1);
            for _ in 0..10_000 {
                if events(&recorded_events).contains(&Event::Complete(1)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(events(&recorded_events).contains(&Event::Complete(1)));
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert!(
                !events(&recorded_events).contains(&Event::Execute(3)),
                "提交失败一经得知就必须停发"
            );
        };

        let (result, ()) = tokio::join!(run, observe_stop);
        let error = result.expect_err("首个任务提交失败必须上交");
        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit"),
            } if task_index == StandardTranslationTaskIndex::new(0)
        ));
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn empty_plan_still_applies_preparation_without_calling_executor_or_commit() {
        let harness = harness(0, Vec::new(), false, false, false, None, None);

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(2), input())
                .await
                .expect("空计划应该成功"),
        );

        assert_eq!(
            events(&harness.events),
            vec![Event::Read, Event::Plan, Event::Prepare]
        );
        assert_eq!(report.total_tasks(), 0);
    }

    #[tokio::test]
    async fn absent_external_files_reach_the_planner_without_invented_defaults() {
        let harness = harness(0, Vec::new(), false, false, false, None, None);
        let input = StandardTranslationInput::new(None, None);

        harness
            .service
            .run(&project(), &profile(2), input.clone())
            .await
            .expect("缺少可选文件不应触发默认发现");

        assert_eq!(
            *harness
                .planner_inputs
                .lock()
                .expect("计划输入记录锁不应中毒"),
            vec![input]
        );
    }

    #[tokio::test]
    async fn a_non_empty_invalidation_preparation_is_applied_before_any_task() {
        let preparation = TranslationPlanPreparation::new(
            vec![TranslationInvalidation::new(
                translation_identity(),
                TextUnitContent::Value("旧译文".to_owned()),
                Sha256Fingerprint::from_bytes([0x44; 32]),
            )],
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            1,
            0,
        );
        let harness = harness_with_preparation(
            0,
            Vec::new(),
            false,
            false,
            false,
            None,
            None,
            preparation.clone(),
        );

        harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect("带失效计划的空任务应该成功");

        assert_eq!(
            *harness.preparations.lock().expect("准备记录锁不应中毒"),
            vec![preparation]
        );
        assert_eq!(
            events(&harness.events),
            vec![Event::Read, Event::Plan, Event::Prepare]
        );
    }

    #[tokio::test]
    async fn partial_and_unavailable_results_are_logged_and_do_not_stop_later_tasks() {
        let harness = harness_with_behavior(
            3,
            vec![1; 3],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![
                FakeOutcomeKind::Partial,
                FakeOutcomeKind::Unavailable,
                FakeOutcomeKind::Complete,
            ],
        );

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(3), input())
                .await
                .expect("正常的部分与无可用译文不应中断运行"),
        );

        let events = events(&harness.events);
        assert_eq!(commit_attempts(&events), vec![0, 2]);
        assert_eq!(logged_tasks(&events), vec![0, 1, 2]);
        assert_eq!(report.complete_tasks(), 1);
        assert_eq!(report.partial_tasks(), 1);
        assert_eq!(report.unavailable_tasks(), 1);
        assert_eq!(report.accepted_decisions(), 3);
        assert_eq!(report.unresolved_decisions(), 3);
        assert_eq!(report.protocol_diagnostics(), 2);
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::PartialCommitted),
                (1, RecordedTaskState::UnavailableNoChanges),
                (2, RecordedTaskState::CompleteCommitted),
            ]
        );

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        let completed = records
            .iter()
            .filter_map(|event| match event {
                StandardTranslationLogEvent::TaskFinished {
                    outcome, attempts, ..
                } => Some((*outcome, *attempts)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed,
            vec![
                (
                    StandardTranslationLogTaskOutcome::Partial,
                    NonZeroUsize::new(1)
                ),
                (
                    StandardTranslationLogTaskOutcome::Unavailable,
                    NonZeroUsize::new(1)
                ),
                (
                    StandardTranslationLogTaskOutcome::Complete,
                    NonZeroUsize::new(1)
                ),
            ]
        );
    }

    #[tokio::test]
    async fn retry_exhaustion_keeps_its_safe_request_diagnostic_in_the_observation_event() {
        let harness = harness_with_behavior(
            1,
            vec![1],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![FakeOutcomeKind::RetryExhausted],
        );

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(1), input())
                .await
                .expect("网络预算耗尽应保留进度并完成运行"),
        );
        assert_eq!(report.unavailable_tasks(), 1);

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        let diagnostic = records
            .iter()
            .find_map(|event| match event {
                StandardTranslationLogEvent::TaskFinished {
                    outcome: StandardTranslationLogTaskOutcome::Unavailable,
                    attempts: Some(attempts),
                    retry_exhausted: true,
                    diagnostic: Some(diagnostic),
                    ..
                } if attempts.get() == 3 => Some(diagnostic),
                _ => None,
            })
            .expect("任务观察必须携带重试耗尽的安全诊断");
        assert!(matches!(
            &diagnostic.reason,
            crate::diagnostic::DiagnosticReason::Http {
                status: Some(503),
                retry_after_seconds: Some(2),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn each_pre_execution_failure_stops_all_later_stages() {
        let cases = [
            (
                harness(2, vec![1; 2], true, false, false, None, None),
                vec![Event::Read],
                "read",
            ),
            (
                harness(2, vec![1; 2], false, true, false, None, None),
                vec![Event::Read, Event::Plan],
                "plan",
            ),
            (
                harness(2, vec![1; 2], false, false, true, None, None),
                vec![Event::Read, Event::Plan, Event::Prepare],
                "prepare",
            ),
        ];

        for (harness, expected_events, expected_source) in cases {
            let error = harness
                .service
                .run(&project(), &profile(2), input())
                .await
                .expect_err("注入的前置阶段应该失败");
            assert_eq!(events(&harness.events), expected_events);
            assert_eq!(
                error.source().expect("阶段错误应保留 source").to_string(),
                expected_source
            );
            assert_eq!(harness.finalizations.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn successful_operation_reports_a_store_finalization_failure() {
        let mut harness = harness(0, Vec::new(), false, false, false, None, None);
        harness.service.result_store.fail_finalization = true;

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("存储收尾失败不能伪装成成功");

        assert!(matches!(
            error,
            StandardTranslationServiceError::FinalizeResultStore(FakeError("finalize"))
        ));
        assert_eq!(harness.finalizations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn operation_and_store_finalization_failures_are_both_preserved() {
        let mut harness = harness(0, Vec::new(), true, false, false, None, None);
        harness.service.result_store.fail_finalization = true;

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("主失败和收尾失败必须聚合");

        let StandardTranslationServiceError::OperationAndFinalization {
            primary,
            finalization,
        } = &error
        else {
            panic!("必须同时保留两个失败")
        };
        assert!(matches!(
            primary.as_ref(),
            StandardTranslationServiceError::ReadAssets(FakeError("read"))
        ));
        assert_eq!(*finalization, FakeError("finalize"));
        assert_eq!(harness.finalizations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn execution_future_is_send() {
        let harness = harness(1, vec![1], false, false, false, None, None);
        let project = project();
        let profile = profile(1);

        assert_send(harness.service.run(&project, &profile, input()));
    }

    fn profile(max_concurrent_requests: usize) -> FakeProfile {
        FakeProfile {
            max_concurrent_requests: NonZeroUsize::new(max_concurrent_requests)
                .expect("测试并发上限必须非零"),
        }
    }

    fn expect_completed<T>(completion: OperationCompletion<T>) -> T {
        match completion {
            OperationCompletion::Completed(value) => value,
            OperationCompletion::Cancelled => panic!("测试未请求取消"),
        }
    }

    fn input() -> StandardTranslationInput {
        StandardTranslationInput::new(
            Some(PathBuf::from("config/terms.toml")),
            Some(PathBuf::from("config/placeholders.toml")),
        )
    }

    fn expected_output(
        task_index: usize,
        id: usize,
        with_propagation_target: bool,
    ) -> ExpectedTranslationOutput {
        let identity = translation_identity_at(task_index * 10 + id, "name");
        let propagation_targets = with_propagation_target
            .then(|| translation_identity_at(1_000 + task_index * 10 + id, "name"))
            .into_iter()
            .collect();
        ExpectedTranslationOutput::new(
            id,
            identity,
            propagation_targets,
            ExpectedTranslationValidation::new(
                ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                "宝剑",
                Vec::new(),
                test_language_analysis(),
            ),
            test_state_context((task_index * 10 + id) as u8),
            with_propagation_target
                .then(|| test_state_context((100 + task_index * 10 + id) as u8))
                .into_iter()
                .collect(),
        )
    }

    fn test_language_analysis() -> LanguageAnalysis {
        JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(
                NonZeroUsize::new(1).expect("测试残留阈值必须非零"),
                Vec::new(),
            )
            .expect("测试日文残留策略应该有效"),
            None,
        )
        .analyze_source(&LanguageText::natural("宝剑"))
    }

    fn fake_outcome(
        task_index: StandardTranslationTaskIndex,
        expected: &[ExpectedTranslationOutput],
        kind: FakeOutcomeKind,
    ) -> TranslationTaskOutcome {
        let patch = |output: &ExpectedTranslationOutput| {
            let translation = TextUnitContent::Value(format!("译文 {}", output.id()));
            let propagation_targets = output
                .propagation_targets()
                .iter()
                .cloned()
                .zip(output.propagation_state_contexts().iter().copied())
                .map(|(identity, state)| TranslationPropagationTarget::new(identity, state))
                .collect();
            AcceptedTranslationDecision::new(
                output.id(),
                TranslationPatch::new(
                    output.identity().clone(),
                    propagation_targets,
                    translation.clone(),
                    output.state_context().finish(&translation),
                ),
            )
        };
        let unresolved = |output: &ExpectedTranslationOutput, reason| {
            UnresolvedTranslationUnit::new(
                output.id(),
                output.identity().clone(),
                output.propagation_targets().to_vec(),
                reason,
            )
        };

        match kind {
            FakeOutcomeKind::Complete => TranslationTaskOutcome::Complete {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    Vec::new(),
                ),
                final_response: FinalLlmResponseMetadata::new(
                    Some(format!("request-{}", task_index.get())),
                    Some(format!("response-{}", task_index.get())),
                    "stop",
                    None,
                ),
                accepted: test_non_empty(expected.iter().map(patch).collect()),
            },
            FakeOutcomeKind::Partial => TranslationTaskOutcome::Partial {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    vec![TranslationProtocolDiagnostic::UnknownId {
                        item_index: 3,
                        id: 99,
                    }],
                ),
                final_response: FinalLlmResponseMetadata::new(
                    Some(format!("request-{}", task_index.get())),
                    Some(format!("response-{}", task_index.get())),
                    "stop",
                    None,
                ),
                accepted: test_non_empty(vec![patch(&expected[0])]),
                unresolved: test_non_empty(vec![unresolved(
                    &expected[1],
                    TranslationUnitRejectionReason::Duplicate,
                )]),
            },
            FakeOutcomeKind::Unavailable => TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    vec![TranslationProtocolDiagnostic::InvalidResponse {
                        message: "无法解析模型 JSON".to_owned(),
                    }],
                ),
                final_response: Some(FinalLlmResponseMetadata::new(
                    Some(format!("request-{}", task_index.get())),
                    Some(format!("response-{}", task_index.get())),
                    "length",
                    None,
                )),
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                unresolved: test_non_empty(
                    expected
                        .iter()
                        .map(|output| {
                            unresolved(
                                output,
                                TranslationUnitRejectionReason::InvalidShape {
                                    message: "无法解析模型 JSON".to_owned(),
                                },
                            )
                        })
                        .collect(),
                ),
            },
            FakeOutcomeKind::RetryExhausted => TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::new(3).expect("测试尝试数必须非零"),
                    Vec::new(),
                ),
                final_response: None,
                reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                    diagnostic: SafeDiagnostic::new(
                        crate::diagnostic::DiagnosticCode::ModelRequest,
                        crate::diagnostic::DiagnosticStage::ModelRequest,
                        crate::diagnostic::DiagnosticSubject::component("test provider"),
                        crate::diagnostic::DiagnosticReason::Http {
                            status: Some(503),
                            retry_after_seconds: Some(2),
                            provider_code: Some("busy".to_owned()),
                            provider_type: Some("service_error".to_owned()),
                        },
                        crate::diagnostic::DiagnosticImpact::ProgressPreserved,
                        crate::diagnostic::DiagnosticAction::CheckModelService,
                    ),
                },
                unresolved: test_non_empty(
                    expected
                        .iter()
                        .map(|output| unresolved(output, TranslationUnitRejectionReason::Missing))
                        .collect(),
                ),
            },
        }
    }

    fn test_non_empty<T>(items: Vec<T>) -> NonEmptyTaskItems<T> {
        let mut items = items.into_iter();
        let first = items.next().expect("测试已建立非空决定集");
        NonEmptyTaskItems::new(first, items.collect())
    }

    fn translation_identity() -> TranslationUnitIdentity {
        translation_identity_at(10, "name")
    }

    fn translation_identity_at(index: usize, field_name: &str) -> TranslationUnitIdentity {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(index)],
        );
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location,
            TextUnitRole::Scalar(ScalarFieldKey::new(field_name).expect("字段键应合法")),
            TextUnitContent::Value("宝剑".to_owned()),
            "{}",
        )
    }

    fn test_state_context(byte: u8) -> TranslationStateContext {
        TranslationStateContext::new(Sha256Fingerprint::from_bytes([byte; 32]))
    }

    fn empty_preparation() -> TranslationPlanPreparation {
        TranslationPlanPreparation::new(
            Vec::new(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            0,
            0,
        )
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            project_name(),
            PathBuf::from("C:/Projects/alice"),
            PathBuf::from("C:/Projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("测试项目名称应该合法")
    }

    fn record(events: &Arc<Mutex<Vec<Event>>>, event: Event) {
        events.lock().expect("事件记录锁不应中毒").push(event);
    }

    fn events(recorded: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        recorded.lock().expect("事件记录锁不应中毒").clone()
    }

    fn committed(events: &[Event]) -> Vec<usize> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Commit(index) => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn commit_attempts(events: &[Event]) -> Vec<usize> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::CommitAttempt(index) => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn assert_all_started_tasks_observed(
        records: &Arc<Mutex<Vec<StandardTranslationLogEvent>>>,
        task_records: &Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
    ) {
        let records = records.lock().expect("观察记录锁不应中毒");
        let mut started = records
            .iter()
            .filter_map(|event| match event {
                StandardTranslationLogEvent::TaskStarted { task_index, .. } => Some(*task_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut finished = records
            .iter()
            .filter_map(|event| match event {
                StandardTranslationLogEvent::TaskFinished { task_index, .. } => Some(*task_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        started.sort_unstable();
        finished.sort_unstable();
        assert_eq!(finished, started, "正常返回前必须观察每个已启动任务的终态");

        let mut recorded = task_records
            .lock()
            .expect("任务记录锁不应中毒")
            .iter()
            .map(|(task_index, _)| StandardTranslationTaskIndex::new(*task_index))
            .collect::<Vec<_>>();
        recorded.sort_unstable();
        assert_eq!(
            recorded, started,
            "每个已启动任务必须恰好提交一份任务记录，未启动任务不得生成记录"
        );
    }

    fn completed(events: &[Event]) -> Vec<usize> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Complete(index) => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn logged_tasks(events: &[Event]) -> Vec<usize> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::LogTask(index) => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn assert_send(_: impl Send) {}
}
