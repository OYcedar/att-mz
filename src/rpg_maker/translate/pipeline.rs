//! RPG Maker 翻译的顶层编排。
//!
//! RPG Maker 只负责读取当前资产、建立任务计划、在外部上限内执行任务，
//! 并按计划顺序逐项提交。任务可以并发完成，但后续任务绝不能越过前序任务
//! 写入数据库，因此失败时始终只保留一个确定的成功前缀。

use std::collections::HashSet;
#[cfg(test)]
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, RpgMakerIssue, RpgMakerModelNonStopFinishReason,
    RpgMakerResponseInvariantProblem, RpgMakerResponseProcessingProblem,
    RpgMakerResponseProcessingScope, RpgMakerTaskResponseJsonCategory, RpgMakerTaskResponseProblem,
    RpgMakerTaskResponseReviewProblem, RpgMakerTaskResponseUnitProblem,
    RpgMakerTaskResponseValueProblem, RpgMakerUnitLocator, StateEffect,
};
use crate::execution::ordered::{
    OrderedExecutionError, OrderedExecutionHandler, OrderedExecutionLimits,
    OrderedFinalizationDisposition, OrderedTaskResult, execute_ordered,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::fingerprint::Sha256Fingerprint;
use crate::language::{LanguageAnalysis, LanguagePair};
use crate::llm::{ChatMessage, LlmClientConcurrency, LlmServiceStatus};
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::model::{LogicalTextLocation, TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::semantic_order::{RpgMakerSemanticOrderKey, RpgMakerSemanticScopeKey};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::{ProvenInvariantViolation, ReviewFinding};
use crate::translation::placeholder::{
    PlaceholderMatchRangeViolation, PlaceholderMatchReference, PlaceholderPcre2ErrorKind,
    PlaceholderRuleReference, PlaceholderWorkerOperation,
};
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderBindingIndex, PlaceholderMultisetError,
};
use crate::translation::task_planning::TaskId;
use crate::translation_protocol::{
    TranslationTaskResponseJsonErrorCategory, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};

use super::profile::RpgMakerTranslationProfile as ConfiguredRpgMakerTranslationProfile;
use super::task_record::{
    NoOpTranslationTaskRecordSink, TranslationAssistantValueError, TranslationTaskCommitFailure,
    TranslationTaskCommitFailureImpact, TranslationTaskCommitPhase, TranslationTaskExecution,
    TranslationTaskExecutionEvidence, TranslationTaskExecutionFailure,
    TranslationTaskExecutionState, TranslationTaskRecordDocument, TranslationTaskRecordFinalState,
    TranslationTaskRecordSink,
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
const STANDARD_IN_FLIGHT_WINDOW_MULTIPLIER: NonZeroUsize =
    NonZeroUsize::new(3).expect("RPG Maker 本地在途窗口倍率必须非零");

/// 一次 RPG Maker 资产翻译需要的可选外部资料。
///
/// 该类型把两个可选路径作为一个拥有所有权的请求交给 Planner。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationInput {
    terminology_path: Option<PathBuf>,
    placeholder_rules_path: Option<PathBuf>,
    retry_rejected: bool,
}

impl RpgMakerTranslationInput {
    pub(crate) fn new(
        terminology_path: Option<PathBuf>,
        placeholder_rules_path: Option<PathBuf>,
    ) -> Self {
        Self {
            terminology_path,
            placeholder_rules_path,
            retry_rejected: false,
        }
    }

    pub(crate) const fn with_retry_rejected(mut self, retry_rejected: bool) -> Self {
        self.retry_rejected = retry_rejected;
        self
    }

    /// 交出两个外部资料路径，供 Planner 拥有其生命周期。
    pub(crate) fn into_parts(self) -> (Option<PathBuf>, Option<PathBuf>, bool) {
        (
            self.terminology_path,
            self.placeholder_rules_path,
            self.retry_rejected,
        )
    }
}

/// RPG Maker 编排本身真正消费的配置事实。
///
/// Profile 由外部配置边界一次建立，只提供供应商允许的活动 HTTP 请求数。
/// 响应返回后的本地流水线窗口由 RPG Maker 的产品策略拥有，不反向成为配置。
pub(crate) trait RpgMakerTranslationExecutionProfile: Send + Sync + 'static {
    fn max_concurrent_requests(&self) -> NonZeroUsize;
}

impl<L> RpgMakerTranslationExecutionProfile for ConfiguredRpgMakerTranslationProfile<L>
where
    L: LlmClientConcurrency + 'static,
{
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.llm_client().max_concurrent_requests()
    }
}

impl<L> RpgMakerTranslationExecutionProfile for Arc<ConfiguredRpgMakerTranslationProfile<L>>
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
pub(crate) struct TranslationUnitIdentity(Arc<TranslationUnitIdentityInner>);

#[derive(Debug, Eq, Hash, PartialEq)]
struct TranslationUnitIdentityInner {
    owner: RpgMakerAssetOwner,
    kind: TextGroupKind,
    logical_location: LogicalTextLocation,
    source_content: TextUnitContent,
    source_context_json: String,
    physical_control_target: Option<(RpgMakerLocation, Option<i64>)>,
}

impl TranslationUnitIdentity {
    pub(crate) fn new(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
        source_content: TextUnitContent,
        source_context_json: impl Into<String>,
    ) -> Self {
        Self(Arc::new(TranslationUnitIdentityInner {
            owner,
            kind,
            logical_location: LogicalTextLocation::new(group_location, role),
            source_content,
            source_context_json: source_context_json.into(),
            physical_control_target: None,
        }))
    }

    pub(crate) fn with_physical_control_target(
        mut self,
        target: Option<(RpgMakerLocation, Option<i64>)>,
    ) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("物理消费者事实应在身份共享前建立")
            .physical_control_target = target;
        self
    }

    pub(crate) fn physical_control_target(&self) -> Option<&(RpgMakerLocation, Option<i64>)> {
        self.0.physical_control_target.as_ref()
    }

    pub(crate) fn owner(&self) -> RpgMakerAssetOwner {
        self.0.owner
    }

    /// 返回语义单元所属的领域组种类。
    pub(crate) fn kind(&self) -> TextGroupKind {
        self.0.kind
    }

    pub(crate) fn role(&self) -> &TextUnitRole {
        self.0.logical_location.role()
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
        self.0.logical_location.group_location()
    }

    pub(crate) fn source_content(&self) -> &TextUnitContent {
        &self.0.source_content
    }

    pub(crate) fn source_context_json(&self) -> &str {
        &self.0.source_context_json
    }

    pub(crate) fn readable_id(&self) -> String {
        crate::manual::readable_rpg_maker_id(self.group_location(), self.kind(), self.role())
    }
}

/// 从 RPG Maker 资产表读出的一个语义单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationAsset {
    identity: TranslationUnitIdentity,
    semantic_order_key: RpgMakerSemanticOrderKey,
    recipe_shape: String,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
    manual: bool,
    rejected: Option<RpgMakerStoredRejectedTranslation>,
}

/// 从当前项目快照读取、尚待 Planner 判断是否仍适用的硬拒绝候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerStoredRejectedTranslation {
    readable_id: String,
    origin: TranslationOrigin,
    source_content: TextUnitContent,
    source_context_json: String,
    candidate_json: String,
    translation: Option<Vec<String>>,
    violation: ProvenInvariantViolation,
    planning_state: Sha256Fingerprint,
}

impl RpgMakerStoredRejectedTranslation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        readable_id: String,
        origin: TranslationOrigin,
        source_content: TextUnitContent,
        source_context_json: String,
        candidate_json: String,
        translation: Option<Vec<String>>,
        violation: ProvenInvariantViolation,
        planning_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            readable_id,
            origin,
            source_content,
            source_context_json,
            candidate_json,
            translation,
            violation,
            planning_state,
        }
    }

    pub(crate) fn source_content(&self) -> &TextUnitContent {
        &self.source_content
    }

    pub(crate) fn source_context_json(&self) -> &str {
        &self.source_context_json
    }

    pub(crate) const fn planning_state(&self) -> Sha256Fingerprint {
        self.planning_state
    }
}

impl RpgMakerTranslationAsset {
    #[cfg(test)]
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        translation: Option<TextUnitContent>,
        translation_state: Option<Sha256Fingerprint>,
    ) -> Self {
        Self {
            identity,
            semantic_order_key: RpgMakerSemanticOrderKey::new(Vec::new(), 0),
            recipe_shape: "[]".to_owned(),
            translation,
            translation_state,
            manual: false,
            rejected: None,
        }
    }

    pub(crate) fn with_rejected_semantic_order_key(
        identity: TranslationUnitIdentity,
        semantic_order_key: RpgMakerSemanticOrderKey,
        recipe_shape: String,
        translation: Option<TextUnitContent>,
        translation_state: Option<Sha256Fingerprint>,
        rejected: Option<RpgMakerStoredRejectedTranslation>,
    ) -> Self {
        Self {
            identity,
            semantic_order_key,
            recipe_shape,
            translation,
            translation_state,
            manual: false,
            rejected,
        }
    }

    pub(crate) fn with_manual_semantic_order_key(
        identity: TranslationUnitIdentity,
        semantic_order_key: RpgMakerSemanticOrderKey,
        recipe_shape: String,
        translation: TextUnitContent,
        translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            semantic_order_key,
            recipe_shape,
            translation: Some(translation),
            translation_state: Some(translation_state),
            manual: true,
            rejected: None,
        }
    }

    pub(crate) fn semantic_order_key(&self) -> &RpgMakerSemanticOrderKey {
        &self.semantic_order_key
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationUnitIdentity,
        RpgMakerSemanticOrderKey,
        String,
        Option<TextUnitContent>,
        Option<Sha256Fingerprint>,
        bool,
        Option<RpgMakerStoredRejectedTranslation>,
    ) {
        (
            self.identity,
            self.semantic_order_key,
            self.recipe_shape,
            self.translation,
            self.translation_state,
            self.manual,
            self.rejected,
        )
    }
}

/// 一个不可拆散的 RPG Maker 复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    semantic_order_key: RpgMakerSemanticOrderKey,
    assets: Vec<RpgMakerTranslationAsset>,
}

impl RpgMakerTranslationGroup {
    #[cfg(test)]
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        assets: Vec<RpgMakerTranslationAsset>,
    ) -> Self {
        Self {
            kind,
            group_location,
            semantic_order_key: RpgMakerSemanticOrderKey::new(Vec::new(), 0),
            assets,
        }
    }

    pub(crate) fn with_semantic_order_key(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        semantic_order_key: RpgMakerSemanticOrderKey,
        assets: Vec<RpgMakerTranslationAsset>,
    ) -> Self {
        Self {
            kind,
            group_location,
            semantic_order_key,
            assets,
        }
    }

    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    #[cfg(test)]
    pub(crate) fn assets(&self) -> &[RpgMakerTranslationAsset] {
        &self.assets
    }

    pub(crate) fn into_assets(self) -> Vec<RpgMakerTranslationAsset> {
        self.assets
    }
}

/// Reader 已按同一语义范围和完整物理顺序整理的 Group 序列。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationScope {
    key: RpgMakerSemanticScopeKey,
    groups: Vec<RpgMakerTranslationGroup>,
}

impl RpgMakerTranslationScope {
    pub(crate) fn new(
        key: RpgMakerSemanticScopeKey,
        groups: Vec<RpgMakerTranslationGroup>,
    ) -> Self {
        debug_assert!(!groups.is_empty(), "Reader 不得建立空语义范围");
        Self { key, groups }
    }

    pub(crate) fn key(&self) -> &RpgMakerSemanticScopeKey {
        &self.key
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[RpgMakerTranslationGroup] {
        &self.groups
    }

    pub(crate) fn into_groups(self) -> Vec<RpgMakerTranslationGroup> {
        self.groups
    }
}

/// Reader 在同一个一致读视图中建立的完整 RPG Maker 翻译语料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationSnapshotBaseline {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    owner_snapshots: Vec<TranslationOwnerSnapshot>,
    terminology_json: String,
    placeholder_rules_json: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationOwnerSnapshot {
    owner: RpgMakerAssetOwner,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    asset_snapshot_fingerprint: AssetSnapshotFingerprint,
}

impl TranslationOwnerSnapshot {
    pub(crate) const fn new(
        owner: RpgMakerAssetOwner,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        asset_snapshot_fingerprint: AssetSnapshotFingerprint,
    ) -> Self {
        Self {
            owner,
            source_snapshot_fingerprint,
            asset_snapshot_fingerprint,
        }
    }

    pub(crate) const fn owner(self) -> RpgMakerAssetOwner {
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

/// Reader 在同一个一致读视图中建立的完整 RPG Maker 翻译语料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationCorpus {
    scopes: Vec<RpgMakerTranslationScope>,
    baseline: TranslationSnapshotBaseline,
}

impl RpgMakerTranslationCorpus {
    #[cfg(test)]
    pub(crate) fn new(groups: Vec<RpgMakerTranslationGroup>) -> Self {
        let mut scopes: Vec<RpgMakerTranslationScope> = Vec::new();
        for group in groups {
            let key = RpgMakerSemanticScopeKey::from_group_location(group.group_location())
                .expect("测试 Group 位置必须具有有效语义范围");
            if let Some(scope) = scopes.last_mut().filter(|scope| scope.key == key) {
                scope.groups.push(group);
            } else {
                scopes.push(RpgMakerTranslationScope::new(key, vec![group]));
            }
        }
        Self::with_snapshot(
            scopes,
            SourceSnapshotFingerprint::from_bytes([0; 32]),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
        )
    }

    pub(crate) fn with_snapshot(
        scopes: Vec<RpgMakerTranslationScope>,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        owner_snapshots: Vec<TranslationOwnerSnapshot>,
        terminology_json: String,
        placeholder_rules_json: String,
    ) -> Self {
        Self {
            scopes,
            baseline: TranslationSnapshotBaseline::new(
                source_snapshot_fingerprint,
                owner_snapshots,
                terminology_json,
                placeholder_rules_json,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn scopes(&self) -> &[RpgMakerTranslationScope] {
        &self.scopes
    }

    pub(crate) fn into_parts(self) -> (Vec<RpgMakerTranslationScope>, TranslationSnapshotBaseline) {
        (self.scopes, self.baseline)
    }

    pub(crate) fn natural_unit_ids(&self) -> HashSet<String> {
        self.scopes
            .iter()
            .flat_map(|scope| &scope.groups)
            .flat_map(|group| &group.assets)
            .map(|asset| asset.identity.readable_id())
            .collect()
    }
}

/// 在任何 LLM 请求前必须完成的 RPG Maker 资产准备。
///
/// 具体动作由 Planner 从当前语料和本次外部资料推导；RPG Maker 不重新解释
/// 术语差异或译文失效规则。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationInvalidation {
    identity: TranslationUnitIdentity,
    expected_translation: TextUnitContent,
    expected_translation_state: Sha256Fingerprint,
    rejected: Option<TranslationInvalidationRejection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TranslationInvalidationRejection {
    violation: ProvenInvariantViolation,
    planning_state: Sha256Fingerprint,
    origin: TranslationOrigin,
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
            rejected: None,
        }
    }

    pub(crate) fn rejected(
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
        violation: ProvenInvariantViolation,
        planning_state: Sha256Fingerprint,
        origin: TranslationOrigin,
    ) -> Self {
        Self {
            identity,
            expected_translation,
            expected_translation_state,
            rejected: Some(TranslationInvalidationRejection {
                violation,
                planning_state,
                origin,
            }),
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationUnitIdentity,
        TextUnitContent,
        Sha256Fingerprint,
        Option<(
            ProvenInvariantViolation,
            Sha256Fingerprint,
            TranslationOrigin,
        )>,
    ) {
        (
            self.identity,
            self.expected_translation,
            self.expected_translation_state,
            self.rejected
                .map(|rejected| (rejected.violation, rejected.planning_state, rejected.origin)),
        )
    }
}

/// RPG Maker 翻译计划准备阶段的逐单元对账计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanPreparationCounts {
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
    rejected_outside_tasks: usize,
    existing_rejected: usize,
    rejected_after_preparation: usize,
    resolved_rejected: usize,
}

impl TranslationPlanPreparationCounts {
    #[cfg(test)]
    pub(crate) const fn new(retained: usize, invalidated: usize, not_applicable: usize) -> Self {
        Self {
            retained,
            invalidated,
            not_applicable,
            rejected_outside_tasks: 0,
            existing_rejected: 0,
            rejected_after_preparation: 0,
            resolved_rejected: 0,
        }
    }

    pub(crate) const fn with_rejected_state(
        retained: usize,
        invalidated: usize,
        not_applicable: usize,
        rejected_outside_tasks: usize,
        existing_rejected: usize,
        rejected_after_preparation: usize,
        resolved_rejected: usize,
    ) -> Self {
        Self {
            retained,
            invalidated,
            not_applicable,
            rejected_outside_tasks,
            existing_rejected,
            rejected_after_preparation,
            resolved_rejected,
        }
    }
}

/// 在任何 LLM 请求前必须完成的 RPG Maker 资产准备。
///
/// 每项失效同时携带读取时的旧译文和语义状态，Store 必须在清理前原子确认这些
/// 事实仍未变化，避免并发翻译把更新后的译文误删。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanPreparation {
    invalidations: Vec<TranslationInvalidation>,
    reuses: Vec<TranslationReuse>,
    terminology_json: String,
    placeholder_rules_json: String,
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
    rejected_outside_tasks: usize,
    existing_rejected: usize,
    rejected_after_preparation: usize,
    resolved_rejected: usize,
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
        Self::with_baseline(
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
        )
    }

    pub(crate) fn with_baseline(
        invalidations: Vec<TranslationInvalidation>,
        reuses: Vec<TranslationReuse>,
        terminology_json: String,
        placeholder_rules_json: String,
        counts: TranslationPlanPreparationCounts,
        snapshot_baseline: TranslationSnapshotBaseline,
    ) -> Self {
        Self {
            invalidations,
            reuses,
            terminology_json,
            placeholder_rules_json,
            retained: counts.retained,
            invalidated: counts.invalidated,
            not_applicable: counts.not_applicable,
            rejected_outside_tasks: counts.rejected_outside_tasks,
            existing_rejected: counts.existing_rejected,
            rejected_after_preparation: counts.rejected_after_preparation,
            resolved_rejected: counts.resolved_rejected,
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

    pub(crate) const fn retained(&self) -> usize {
        self.retained
    }

    pub(crate) const fn invalidated(&self) -> usize {
        self.invalidated
    }

    pub(crate) const fn not_applicable(&self) -> usize {
        self.not_applicable
    }

    pub(crate) const fn rejected_outside_tasks(&self) -> usize {
        self.rejected_outside_tasks
    }

    pub(crate) const fn existing_rejected(&self) -> usize {
        self.existing_rejected
    }

    pub(crate) const fn rejected_after_preparation(&self) -> usize {
        self.rejected_after_preparation
    }

    pub(crate) const fn resolved_rejected(&self) -> usize {
        self.resolved_rejected
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

/// Placeholder 投影阶段无法建立受信语义、因而不会进入任何 LLM 任务的 RPG Maker 单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanningFailure {
    identity: TranslationUnitIdentity,
    reason: TranslationPlanningFailureReason,
    rule_source: TranslationPlaceholderRuleSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationPlaceholderRuleSource {
    ExternalFile(PathBuf),
    ProjectSnapshot,
}

impl TranslationPlanningFailure {
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        reason: TranslationPlanningFailureReason,
    ) -> Self {
        Self {
            identity,
            reason,
            rule_source: TranslationPlaceholderRuleSource::ProjectSnapshot,
        }
    }

    pub(crate) fn with_rule_source(
        mut self,
        rule_source: TranslationPlaceholderRuleSource,
    ) -> Self {
        self.rule_source = rule_source;
        self
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    #[cfg(test)]
    pub(crate) fn reason(&self) -> &TranslationPlanningFailureReason {
        &self.reason
    }

    /// 将规划器掌握的叶子原因和完整 RPG Maker Unit 身份一次性投影为公开诊断。
    ///
    /// 规则来源由 Planner 在解析本次资源时保存；调用方不得再从展示文本猜测来源。
    pub(crate) fn diagnostic_report(&self) -> crate::diagnostic::DiagnosticReport {
        crate::diagnostic::DiagnosticReport::new(
            crate::diagnostic::StateEffect::Unchanged,
            crate::diagnostic::Diagnostic::rpg_maker(self.diagnostic_issue()),
        )
    }

    fn diagnostic_issue(&self) -> crate::diagnostic::RpgMakerIssue {
        let unit = rpg_maker_diagnostic_unit(&self.identity);
        match &self.reason {
            TranslationPlanningFailureReason::PlaceholderProtection { failure } => {
                crate::diagnostic::RpgMakerIssue::placeholder_planning(
                    match &self.rule_source {
                        TranslationPlaceholderRuleSource::ExternalFile(path) => {
                            crate::diagnostic::PlaceholderRuleSource::external_file(path)
                        }
                        TranslationPlaceholderRuleSource::ProjectSnapshot => {
                            crate::diagnostic::PlaceholderRuleSource::ProjectSnapshot
                        }
                    },
                    unit,
                    placeholder_protection_diagnostic(failure),
                )
            }
            TranslationPlanningFailureReason::PlaceholderProjection { failure } => {
                crate::diagnostic::RpgMakerIssue::placeholder_projection(
                    match &self.rule_source {
                        TranslationPlaceholderRuleSource::ExternalFile(path) => {
                            crate::diagnostic::PlaceholderRuleSource::external_file(path)
                        }
                        TranslationPlaceholderRuleSource::ProjectSnapshot => {
                            crate::diagnostic::PlaceholderRuleSource::ProjectSnapshot
                        }
                    },
                    unit,
                    placeholder_projection_diagnostic(failure),
                )
            }
        }
    }
}

impl fmt::Display for TranslationPlanningFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker Unit 无法完成 Placeholder 或语言投影准备")
    }
}

impl Error for TranslationPlanningFailure {}

pub(super) fn rpg_maker_diagnostic_unit(
    identity: &TranslationUnitIdentity,
) -> crate::diagnostic::RpgMakerUnitLocator {
    crate::diagnostic::RpgMakerUnitLocator::new(
        crate::diagnostic::SafeText::new(identity.readable_id()),
        identity.owner().diagnostic_owner(),
        identity.kind().diagnostic_group_kind(),
        identity.group_location().diagnostic_location(),
        identity.role().diagnostic_role(),
    )
}

pub(crate) fn placeholder_protection_diagnostic(
    failure: &TranslationPlaceholderProtectionFailure,
) -> crate::diagnostic::PlaceholderIssue {
    use crate::diagnostic::{
        ByteRange, Pcre2Failure, Pcre2FailureKind, PlaceholderIssue,
        PlaceholderMatchRangeViolation as RangeViolation, PlaceholderRuleOrigin as RuleOrigin,
        PlaceholderWorkerOperation as DiagnosticWorkerOperation,
    };

    let range = |start, end| {
        ByteRange::new(start, end).expect("Placeholder 匹配器建立的已确认匹配范围必须有效")
    };
    let origin = |value| match value {
        PlaceholderRuleOrigin::BuiltIn => RuleOrigin::Builtin,
        PlaceholderRuleOrigin::Custom => RuleOrigin::Custom,
    };
    match failure {
        TranslationPlaceholderProtectionFailure::WorkerStart {
            operation,
            io_kind,
            raw_os_code,
        } => PlaceholderIssue::WorkerStart {
            operation: match operation {
                PlaceholderWorkerOperation::CompileCustomRules => {
                    DiagnosticWorkerOperation::CompileCustomRules
                }
                PlaceholderWorkerOperation::MatchText => DiagnosticWorkerOperation::MatchText,
            },
            io_kind: (*io_kind).into(),
            raw_os_code: *raw_os_code,
        },
        TranslationPlaceholderProtectionFailure::Pcre2 {
            rule,
            kind,
            code,
            offset,
        } => PlaceholderIssue::PatternMatch {
            rule_origin: Some(origin(rule.origin())),
            rule_number: rule.rule_number(),
            pcre2: Pcre2Failure {
                kind: match kind {
                    PlaceholderPcre2ErrorKind::Compile => Pcre2FailureKind::Compile,
                    PlaceholderPcre2ErrorKind::Jit => Pcre2FailureKind::Jit,
                    PlaceholderPcre2ErrorKind::Match => Pcre2FailureKind::Match,
                    PlaceholderPcre2ErrorKind::Info => Pcre2FailureKind::Info,
                    PlaceholderPcre2ErrorKind::Option => Pcre2FailureKind::Option,
                    PlaceholderPcre2ErrorKind::Unrecognized => Pcre2FailureKind::Unrecognized,
                },
                code: *code,
                offset: *offset,
            },
        },
        TranslationPlaceholderProtectionFailure::EmptyMatch { matched } => {
            PlaceholderIssue::EmptyMatch {
                rule_origin: origin(matched.rule().origin()),
                rule_number: matched.rule().rule_number(),
                match_range: range(matched.start_byte(), matched.end_byte()),
            }
        }
        TranslationPlaceholderProtectionFailure::MissingTextCapture {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
        } => PlaceholderIssue::MissingTextCapture {
            rule_number: *rule_number,
            match_range: range(*whole_match_start_byte, *whole_match_end_byte),
        },
        TranslationPlaceholderProtectionFailure::InvalidMatchRange {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
            capture_start_byte,
            capture_end_byte,
            violation,
        } => PlaceholderIssue::InvalidMatchRange {
            rule_number: *rule_number,
            whole_match_start_byte: *whole_match_start_byte,
            whole_match_end_byte: *whole_match_end_byte,
            capture_start_byte: *capture_start_byte,
            capture_end_byte: *capture_end_byte,
            violation: match violation {
                PlaceholderMatchRangeViolation::WholeStartAfterEnd => {
                    RangeViolation::WholeStartAfterEnd
                }
                PlaceholderMatchRangeViolation::WholeEndBeyondText => {
                    RangeViolation::WholeEndBeyondText
                }
                PlaceholderMatchRangeViolation::WholeStartNotUtf8Boundary => {
                    RangeViolation::WholeStartNotUtf8Boundary
                }
                PlaceholderMatchRangeViolation::WholeEndNotUtf8Boundary => {
                    RangeViolation::WholeEndNotUtf8Boundary
                }
                PlaceholderMatchRangeViolation::CaptureStartAfterEnd => {
                    RangeViolation::CaptureStartAfterEnd
                }
                PlaceholderMatchRangeViolation::CaptureEndBeyondText => {
                    RangeViolation::CaptureEndBeyondText
                }
                PlaceholderMatchRangeViolation::CaptureStartNotUtf8Boundary => {
                    RangeViolation::CaptureStartNotUtf8Boundary
                }
                PlaceholderMatchRangeViolation::CaptureEndNotUtf8Boundary => {
                    RangeViolation::CaptureEndNotUtf8Boundary
                }
                PlaceholderMatchRangeViolation::CaptureStartsBeforeWhole => {
                    RangeViolation::CaptureStartsBeforeWhole
                }
                PlaceholderMatchRangeViolation::CaptureEndsAfterWhole => {
                    RangeViolation::CaptureEndsAfterWhole
                }
            },
        },
        TranslationPlaceholderProtectionFailure::OverlappingMatches { first, second } => {
            PlaceholderIssue::OverlappingMatches {
                first_origin: origin(first.rule().origin()),
                first_rule_number: first.rule().rule_number(),
                first_range: range(first.start_byte(), first.end_byte()),
                second_origin: origin(second.rule().origin()),
                second_rule_number: second.rule().rule_number(),
                second_range: range(second.start_byte(), second.end_byte()),
            }
        }
        TranslationPlaceholderProtectionFailure::CrossesLineBoundary {
            matched,
            source_line_index,
        } => PlaceholderIssue::CrossesLineBoundary {
            rule_origin: origin(matched.rule().origin()),
            rule_number: matched.rule().rule_number(),
            source_line_index: *source_line_index,
        },
        TranslationPlaceholderProtectionFailure::ReservedTokenNamespace {
            start_byte,
            end_byte,
        } => PlaceholderIssue::ReservedTokenNamespace {
            range: range(*start_byte, *end_byte),
        },
    }
}

pub(crate) fn placeholder_projection_diagnostic(
    failure: &TranslationPlaceholderProjectionFailure,
) -> crate::diagnostic::RpgMakerPlaceholderProjectionProblem {
    use crate::diagnostic::RpgMakerPlaceholderProjectionProblem as Problem;

    match failure {
        TranslationPlaceholderProjectionFailure::TokenIndexConstruction => {
            Problem::TokenIndexConstruction
        }
        TranslationPlaceholderProjectionFailure::EmptyToken => Problem::EmptyToken,
        TranslationPlaceholderProjectionFailure::MissingToken { token } => {
            Problem::missing_token(token)
        }
        TranslationPlaceholderProjectionFailure::RepeatedToken { token } => {
            Problem::repeated_token(token)
        }
        TranslationPlaceholderProjectionFailure::OverlappingToken { token } => {
            Problem::overlapping_token(token)
        }
        TranslationPlaceholderProjectionFailure::ChangedTokenOrder {
            position,
            expected_token,
            actual_token,
        } => Problem::changed_token_order(*position, expected_token, actual_token),
        TranslationPlaceholderProjectionFailure::ChangedSegmentCount { expected, actual } => {
            Problem::ChangedSegmentCount {
                expected: *expected,
                actual: *actual,
            }
        }
        TranslationPlaceholderProjectionFailure::ChangedSegmentKind { segment_index } => {
            Problem::ChangedSegmentKind {
                segment_index: *segment_index,
            }
        }
        TranslationPlaceholderProjectionFailure::MissingOrderedToken { segment_index } => {
            Problem::MissingOrderedToken {
                segment_index: *segment_index,
            }
        }
        TranslationPlaceholderProjectionFailure::UnusedOrderedToken => Problem::UnusedOrderedToken,
        TranslationPlaceholderProjectionFailure::SourceBindingMismatch => {
            Problem::SourceBindingMismatch
        }
    }
}

/// 规划期失败与模型响应拒绝分属不同阶段，不共享 ID、attempt 或拒绝原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationPlanningFailureReason {
    PlaceholderProtection {
        failure: TranslationPlaceholderProtectionFailure,
    },
    PlaceholderProjection {
        failure: TranslationPlaceholderProjectionFailure,
    },
}

/// Placeholder 保护阶段对外可以安全保留的确定事实。
///
/// 该类型不保存原文、`Display` 正文或后端错误字符串。规则引用和匹配范围
/// 由 Placeholder 保护算法直接建立，不从展示文本反向解析。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationPlaceholderProtectionFailure {
    WorkerStart {
        operation: PlaceholderWorkerOperation,
        io_kind: std::io::ErrorKind,
        raw_os_code: Option<i32>,
    },
    Pcre2 {
        rule: PlaceholderRuleReference,
        kind: PlaceholderPcre2ErrorKind,
        code: i32,
        offset: Option<usize>,
    },
    EmptyMatch {
        matched: PlaceholderMatchReference,
    },
    MissingTextCapture {
        rule_number: usize,
        whole_match_start_byte: usize,
        whole_match_end_byte: usize,
    },
    InvalidMatchRange {
        rule_number: usize,
        whole_match_start_byte: usize,
        whole_match_end_byte: usize,
        capture_start_byte: Option<usize>,
        capture_end_byte: Option<usize>,
        violation: PlaceholderMatchRangeViolation,
    },
    OverlappingMatches {
        first: PlaceholderMatchReference,
        second: PlaceholderMatchReference,
    },
    CrossesLineBoundary {
        matched: PlaceholderMatchReference,
        source_line_index: usize,
    },
    ReservedTokenNamespace {
        start_byte: usize,
        end_byte: usize,
    },
}

/// Placeholder 语言投影阶段对外可以安全保留的确定事实。
///
/// token 由 Placeholder 保护算法生成，不保存被保护的游戏正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationPlaceholderProjectionFailure {
    TokenIndexConstruction,
    EmptyToken,
    MissingToken {
        token: String,
    },
    RepeatedToken {
        token: String,
    },
    OverlappingToken {
        token: String,
    },
    ChangedTokenOrder {
        position: usize,
        expected_token: String,
        actual_token: String,
    },
    ChangedSegmentCount {
        expected: usize,
        actual: usize,
    },
    ChangedSegmentKind {
        segment_index: usize,
    },
    MissingOrderedToken {
        segment_index: usize,
    },
    UnusedOrderedToken,
    SourceBindingMismatch,
}

/// 任务在确定计划中的序号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RpgMakerTranslationTaskIndex(usize);

impl RpgMakerTranslationTaskIndex {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for RpgMakerTranslationTaskIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
pub(crate) use crate::translation::placeholder::PlaceholderSegment;
pub(crate) use crate::translation::placeholder::{AppliedPlaceholder, PlaceholderRuleOrigin};

/// 一个完整 Group 的身份、完整原文、来源语境与 Unit 自然顺序指纹。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupContextFingerprint(Sha256Fingerprint);

impl GroupContextFingerprint {
    pub(crate) const fn new(fingerprint: Sha256Fingerprint) -> Self {
        Self(fingerprint)
    }

    pub(crate) const fn as_fingerprint(self) -> Sha256Fingerprint {
        self.0
    }
}

/// 一个语义单元的当前译文适用性。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationStateContext(Sha256Fingerprint);

impl TranslationStateContext {
    pub(crate) const fn new(fingerprint: Sha256Fingerprint) -> Self {
        Self(fingerprint)
    }

    pub(crate) const fn applicability(self) -> Sha256Fingerprint {
        self.0
    }

    pub(crate) fn is_current(self, stored: Sha256Fingerprint) -> bool {
        stored == self.0
    }
}

/// 去重传播目标以及该语义单元的独立语义上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPropagationTarget {
    identity: TranslationUnitIdentity,
    state_context: TranslationStateContext,
    expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
    was_current_rejected: bool,
}

impl TranslationPropagationTarget {
    #[cfg(test)]
    pub(crate) const fn new(
        identity: TranslationUnitIdentity,
        state_context: TranslationStateContext,
    ) -> Self {
        Self {
            identity,
            state_context,
            expected_previous: None,
            was_current_rejected: false,
        }
    }

    pub(crate) fn with_previous_and_rejected_state(
        identity: TranslationUnitIdentity,
        state_context: TranslationStateContext,
        expected_translation: Option<TextUnitContent>,
        expected_translation_state: Option<Sha256Fingerprint>,
        was_current_rejected: bool,
    ) -> Self {
        assert_eq!(
            expected_translation.is_some(),
            expected_translation_state.is_some(),
            "传播目标读取时的译文和状态必须同时存在或同时缺失"
        );
        Self {
            identity,
            state_context,
            expected_previous: expected_translation.zip(expected_translation_state),
            was_current_rejected,
        }
    }

    pub(crate) const fn was_current_rejected(&self) -> bool {
        self.was_current_rejected
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) const fn state_context(&self) -> TranslationStateContext {
        self.state_context
    }

    pub(crate) fn expected_previous(&self) -> Option<(&TextUnitContent, Sha256Fingerprint)> {
        self.expected_previous
            .as_ref()
            .map(|(translation, state)| (translation, *state))
    }
}

/// TaskBlock 单元是需要模型返回结果的活跃原文，或只提供上下文的虚原文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationVirtualReason {
    ExistingTranslation,
    RejectedCandidate,
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
    applied_placeholders: Arc<[AppliedPlaceholder]>,
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
            applied_placeholders: applied_placeholders.into(),
            language_analysis,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaceholderMultisetErrorKind {
    Mismatch,
    Unexpected,
    OrderMismatch,
    WrapperTopologyChanged,
}

impl PlaceholderMultisetErrorKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Mismatch => "mismatch",
            Self::Unexpected => "unexpected",
            Self::OrderMismatch => "order_mismatch",
            Self::WrapperTopologyChanged => "wrapper_topology_changed",
        }
    }
}

impl From<&PlaceholderMultisetError> for PlaceholderMultisetErrorKind {
    fn from(source: &PlaceholderMultisetError) -> Self {
        match source {
            PlaceholderMultisetError::Mismatch { .. } => Self::Mismatch,
            PlaceholderMultisetError::Unexpected { .. } => Self::Unexpected,
            PlaceholderMultisetError::OrderMismatch { .. } => Self::OrderMismatch,
            PlaceholderMultisetError::WrapperTopologyChanged { .. } => Self::WrapperTopologyChanged,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedTranslationOutputContractTarget {
    owner: RpgMakerAssetOwner,
    group_kind: TextGroupKind,
    group_location: RpgMakerLocation,
    role: TextUnitRole,
}

impl ExpectedTranslationOutputContractTarget {
    fn from_identity(identity: &TranslationUnitIdentity) -> Self {
        Self {
            owner: identity.owner(),
            group_kind: identity.kind(),
            group_location: identity.group_location().clone(),
            role: identity.role().clone(),
        }
    }

    fn diagnostic_unit_locator(&self) -> RpgMakerUnitLocator {
        RpgMakerUnitLocator::new(
            crate::diagnostic::SafeText::new(crate::manual::readable_rpg_maker_id(
                &self.group_location,
                self.group_kind,
                &self.role,
            )),
            self.owner.diagnostic_owner(),
            self.group_kind.diagnostic_group_kind(),
            self.group_location.diagnostic_location(),
            self.role.diagnostic_role(),
        )
    }
}

#[derive(Eq, PartialEq)]
pub(crate) enum ExpectedTranslationOutputContractError {
    PropagationContextCountMismatch {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        target_count: usize,
        context_count: usize,
    },
    PlaceholderIndexInvalid {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        source: LanguageTextProjectionError,
    },
    ProtectedPlaceholderMultisetMismatch {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        kind: PlaceholderMultisetErrorKind,
    },
    ProtectedPlaceholderCrossesLineBoundary {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        placeholder_index: usize,
    },
    ProtectedLineCountMismatch {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        expected: usize,
        actual: usize,
    },
    ScalarAlignedCountInvalid {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        actual: usize,
    },
    LinesAlignedCountMismatch {
        unit_id: TaskId,
        target: Box<ExpectedTranslationOutputContractTarget>,
        expected: usize,
        actual: usize,
    },
}

impl ExpectedTranslationOutputContractError {
    pub(crate) fn placeholder_index_invalid(
        unit_id: TaskId,
        identity: &TranslationUnitIdentity,
        source: LanguageTextProjectionError,
    ) -> Self {
        Self::PlaceholderIndexInvalid {
            unit_id,
            target: Box::new(ExpectedTranslationOutputContractTarget::from_identity(
                identity,
            )),
            source,
        }
    }

    fn target_and_unit(&self) -> (&ExpectedTranslationOutputContractTarget, TaskId) {
        match self {
            Self::PropagationContextCountMismatch {
                unit_id, target, ..
            }
            | Self::PlaceholderIndexInvalid {
                unit_id, target, ..
            }
            | Self::ProtectedPlaceholderMultisetMismatch {
                unit_id, target, ..
            }
            | Self::ProtectedPlaceholderCrossesLineBoundary {
                unit_id, target, ..
            }
            | Self::ProtectedLineCountMismatch {
                unit_id, target, ..
            }
            | Self::ScalarAlignedCountInvalid {
                unit_id, target, ..
            }
            | Self::LinesAlignedCountMismatch {
                unit_id, target, ..
            } => (target.as_ref(), *unit_id),
        }
    }

    pub(crate) fn diagnostic_unit_locator(&self) -> RpgMakerUnitLocator {
        self.target_and_unit().0.diagnostic_unit_locator()
    }

    pub(crate) fn diagnostic_task_id(&self) -> usize {
        self.target_and_unit().1.get()
    }
}

impl fmt::Debug for ExpectedTranslationOutputContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (target, unit_id) = self.target_and_unit();
        formatter
            .debug_struct("ExpectedTranslationOutputContractError")
            .field("unit_id", &unit_id.get())
            .field("target", target)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ExpectedTranslationOutputContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PropagationContextCountMismatch {
                unit_id,
                target_count,
                context_count,
                ..
            } => write!(
                formatter,
                "任务 {} 的传播目标有 {target_count} 项，但状态上下文有 {context_count} 项",
                unit_id.get()
            ),
            Self::PlaceholderIndexInvalid { unit_id, .. } => {
                write!(formatter, "任务 {} 的 Placeholder 索引无效", unit_id.get())
            }
            Self::ProtectedPlaceholderMultisetMismatch { unit_id, kind, .. } => write!(
                formatter,
                "任务 {} 的受保护 Placeholder 与原文不一致（{}）",
                unit_id.get(),
                kind.as_str()
            ),
            Self::ProtectedPlaceholderCrossesLineBoundary {
                unit_id,
                placeholder_index,
                ..
            } => write!(
                formatter,
                "任务 {} 的第 {placeholder_index} 个受保护 Placeholder 跨越了换行符",
                unit_id.get()
            ),
            Self::ProtectedLineCountMismatch {
                unit_id,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "任务 {} 的受保护文本应有 {expected} 行，实际为 {actual} 行",
                unit_id.get()
            ),
            Self::ScalarAlignedCountInvalid {
                unit_id, actual, ..
            } => write!(
                formatter,
                "任务 {} 的标量文本只能对齐 1 行，实际为 {actual} 行",
                unit_id.get()
            ),
            Self::LinesAlignedCountMismatch {
                unit_id,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "任务 {} 的行数应为 {expected}，实际为 {actual}",
                unit_id.get()
            ),
        }
    }
}

impl Error for ExpectedTranslationOutputContractError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PlaceholderIndexInvalid { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedTranslationOutput {
    id: TaskId,
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationUnitIdentity>,
    validation: ExpectedTranslationValidation,
    placeholder_bindings: Arc<PlaceholderBindingIndex>,
    state_context: TranslationStateContext,
    propagation_state_contexts: Vec<TranslationStateContext>,
    expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
    propagation_expected_previous: Vec<Option<(TextUnitContent, Sha256Fingerprint)>>,
    was_current_rejected: bool,
    propagation_was_current_rejected: Vec<bool>,
}

impl ExpectedTranslationOutput {
    #[cfg(test)]
    pub(crate) fn try_new(
        id: TaskId,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
    ) -> Result<Self, ExpectedTranslationOutputContractError> {
        match Self::try_new_with_cancellation(
            id,
            identity,
            propagation_targets,
            validation,
            state_context,
            propagation_state_contexts,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    #[cfg(test)]
    pub(crate) fn try_new_with_cancellation<E>(
        id: TaskId,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, ExpectedTranslationOutputContractError>, E> {
        Self::try_new_with_previous_and_cancellation(
            id,
            identity,
            propagation_targets,
            validation,
            state_context,
            propagation_state_contexts,
            None,
            Vec::new(),
            ensure_running,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) fn try_new_with_previous_and_cancellation<E>(
        id: TaskId,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
        expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
        propagation_expected_previous: Vec<Option<(TextUnitContent, Sha256Fingerprint)>>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, ExpectedTranslationOutputContractError>, E> {
        Self::try_new_with_rejected_state_and_cancellation(
            id,
            identity,
            propagation_targets,
            validation,
            state_context,
            propagation_state_contexts,
            expected_previous,
            propagation_expected_previous,
            false,
            Vec::new(),
            ensure_running,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new_with_rejected_state_and_cancellation<E>(
        id: TaskId,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
        expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
        mut propagation_expected_previous: Vec<Option<(TextUnitContent, Sha256Fingerprint)>>,
        was_current_rejected: bool,
        mut propagation_was_current_rejected: Vec<bool>,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, ExpectedTranslationOutputContractError>, E> {
        ensure_running()?;
        if propagation_expected_previous.is_empty() {
            propagation_expected_previous.resize(propagation_targets.len(), None);
        }
        if propagation_was_current_rejected.is_empty() {
            propagation_was_current_rejected.resize(propagation_targets.len(), false);
        }
        if propagation_targets.len() != propagation_state_contexts.len() {
            return Ok(Err(
                ExpectedTranslationOutputContractError::PropagationContextCountMismatch {
                    unit_id: id,
                    target: Box::new(ExpectedTranslationOutputContractTarget::from_identity(
                        &identity,
                    )),
                    target_count: propagation_targets.len(),
                    context_count: propagation_state_contexts.len(),
                },
            ));
        }
        if propagation_targets.len() != propagation_expected_previous.len() {
            return Ok(Err(
                ExpectedTranslationOutputContractError::PropagationContextCountMismatch {
                    unit_id: id,
                    target: Box::new(ExpectedTranslationOutputContractTarget::from_identity(
                        &identity,
                    )),
                    target_count: propagation_targets.len(),
                    context_count: propagation_expected_previous.len(),
                },
            ));
        }
        if propagation_targets.len() != propagation_was_current_rejected.len() {
            return Ok(Err(
                ExpectedTranslationOutputContractError::PropagationContextCountMismatch {
                    unit_id: id,
                    target: Box::new(ExpectedTranslationOutputContractTarget::from_identity(
                        &identity,
                    )),
                    target_count: propagation_targets.len(),
                    context_count: propagation_was_current_rejected.len(),
                },
            ));
        }
        let placeholder_bindings = match PlaceholderBindingIndex::from_shared_with_cancellation(
            Arc::clone(&validation.applied_placeholders),
            &mut ensure_running,
        )? {
            Ok(bindings) => Arc::new(bindings),
            Err(source) => {
                return Ok(Err(
                    ExpectedTranslationOutputContractError::placeholder_index_invalid(
                        id, &identity, source,
                    ),
                ));
            }
        };
        if let Err(source) = validate_expected_translation_output_with_cancellation(
            id,
            &identity,
            &validation,
            placeholder_bindings.as_ref(),
            &mut ensure_running,
        )? {
            return Ok(Err(source));
        }
        ensure_running()?;
        Ok(Ok(Self {
            id,
            identity,
            propagation_targets,
            validation,
            placeholder_bindings,
            state_context,
            propagation_state_contexts,
            expected_previous,
            propagation_expected_previous,
            was_current_rejected,
            propagation_was_current_rejected,
        }))
    }

    #[cfg(test)]
    pub(crate) fn new(
        id: TaskId,
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        validation: ExpectedTranslationValidation,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
    ) -> Self {
        Self::try_new(
            id,
            identity,
            propagation_targets,
            validation,
            state_context,
            propagation_state_contexts,
        )
        .expect("测试 ExpectedTranslationOutput 必须满足静态 Planner 契约")
    }

    pub(crate) const fn id(&self) -> TaskId {
        self.id
    }

    pub(crate) fn expected_previous(&self) -> Option<(&TextUnitContent, Sha256Fingerprint)> {
        self.expected_previous
            .as_ref()
            .map(|(translation, state)| (translation, *state))
    }

    pub(crate) fn propagation_expected_previous(
        &self,
    ) -> &[Option<(TextUnitContent, Sha256Fingerprint)>] {
        &self.propagation_expected_previous
    }

    pub(crate) const fn was_current_rejected(&self) -> bool {
        self.was_current_rejected
    }

    pub(crate) fn propagation_was_current_rejected(&self) -> &[bool] {
        &self.propagation_was_current_rejected
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

    pub(super) fn placeholder_bindings(&self) -> &PlaceholderBindingIndex {
        self.placeholder_bindings.as_ref()
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

fn validate_expected_translation_output_with_cancellation<E>(
    unit_id: TaskId,
    identity: &TranslationUnitIdentity,
    validation: &ExpectedTranslationValidation,
    placeholder_bindings: &PlaceholderBindingIndex,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<(), ExpectedTranslationOutputContractError>, E> {
    ensure_running()?;
    let target = || {
        Box::new(ExpectedTranslationOutputContractTarget::from_identity(
            identity,
        ))
    };
    let protected_scan = placeholder_bindings
        .scan_with_cancellation(&validation.protected_text, &mut ensure_running)?;
    if matches!(identity.source_content(), TextUnitContent::Lines(_)) {
        for (placeholder_index, placeholder) in validation.applied_placeholders.iter().enumerate() {
            ensure_running()?;
            if text_contains_byte_with_cancellation(
                placeholder.original(),
                b'\n',
                &mut ensure_running,
            )? {
                return Ok(Err(
                    ExpectedTranslationOutputContractError::ProtectedPlaceholderCrossesLineBoundary {
                        unit_id,
                        target: target(),
                        placeholder_index,
                    },
                ));
            }
        }
    }
    if let Err(reason) = placeholder_bindings.validate_multiset_with_cancellation(
        std::slice::from_ref(&protected_scan),
        placeholder_bindings.all_binding_indices(),
        &mut ensure_running,
    )? {
        return Ok(Err(
            ExpectedTranslationOutputContractError::ProtectedPlaceholderMultisetMismatch {
                unit_id,
                target: target(),
                kind: (&reason).into(),
            },
        ));
    }
    if let ExpectedLineShape::Aligned(line_count) = validation.line_shape {
        let actual = line_count_with_cancellation(&validation.protected_text, &mut ensure_running)?;
        if actual != line_count.get() {
            return Ok(Err(
                ExpectedTranslationOutputContractError::ProtectedLineCountMismatch {
                    unit_id,
                    target: target(),
                    expected: line_count.get(),
                    actual,
                },
            ));
        }
    }
    let result = match (identity.source_content(), validation.line_shape) {
        (TextUnitContent::Value(_), ExpectedLineShape::Aligned(line_count))
            if line_count.get() == 1 =>
        {
            Ok(())
        }
        (TextUnitContent::Value(_), ExpectedLineShape::Reflow) => Ok(()),
        (TextUnitContent::Value(_), ExpectedLineShape::Aligned(line_count)) => Err(
            ExpectedTranslationOutputContractError::ScalarAlignedCountInvalid {
                unit_id,
                target: target(),
                actual: line_count.get(),
            },
        ),
        (TextUnitContent::Lines(source_lines), ExpectedLineShape::Aligned(line_count))
            if source_lines.len() != line_count.get() =>
        {
            Err(
                ExpectedTranslationOutputContractError::LinesAlignedCountMismatch {
                    unit_id,
                    target: target(),
                    expected: source_lines.len(),
                    actual: line_count.get(),
                },
            )
        }
        (TextUnitContent::Lines(_), _) => Ok(()),
    };
    ensure_running()?;
    Ok(result)
}

fn text_contains_byte_with_cancellation<E>(
    text: &str,
    needle: u8,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    const CHECK_BYTES: usize = 64 * 1024;
    for chunk in text.as_bytes().chunks(CHECK_BYTES) {
        ensure_running()?;
        if chunk.contains(&needle) {
            return Ok(true);
        }
    }
    ensure_running()?;
    Ok(false)
}

fn line_count_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    const CHECK_BYTES: usize = 64 * 1024;
    let mut lines = 1_usize;
    for chunk in text.as_bytes().chunks(CHECK_BYTES) {
        ensure_running()?;
        lines = lines.saturating_add(chunk.iter().filter(|byte| **byte == b'\n').count());
    }
    ensure_running()?;
    Ok(lines)
}

/// 一个已经完成语义切块并生成最终最小消息的任务块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerExecutableTask {
    index: RpgMakerTranslationTaskIndex,
    language_pair: LanguagePair,
    messages: Vec<ChatMessage>,
    expected_outputs: Arc<[ExpectedTranslationOutput]>,
    semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
}

impl RpgMakerExecutableTask {
    #[cfg(test)]
    pub(crate) fn new(
        index: RpgMakerTranslationTaskIndex,
        language_pair: LanguagePair,
        messages: Vec<ChatMessage>,
        expected_outputs: Vec<ExpectedTranslationOutput>,
    ) -> Self {
        Self::new_with_semantics(
            index,
            language_pair,
            messages,
            expected_outputs,
            Arc::new(super::semantics::ResolvedTranslationSemantics::for_test()),
        )
    }

    pub(crate) fn new_with_semantics(
        index: RpgMakerTranslationTaskIndex,
        language_pair: LanguagePair,
        messages: Vec<ChatMessage>,
        expected_outputs: Vec<ExpectedTranslationOutput>,
        semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
    ) -> Self {
        assert!(
            expected_outputs
                .iter()
                .enumerate()
                .all(|(index, output)| TaskId::new(index) == output.id()),
            "任务内模型输出 ID 必须从 0 连续编号"
        );
        Self {
            index,
            language_pair,
            messages,
            expected_outputs: expected_outputs.into(),
            semantics,
        }
    }

    pub(crate) const fn index(&self) -> RpgMakerTranslationTaskIndex {
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

    pub(crate) fn shared_expected_outputs(&self) -> Arc<[ExpectedTranslationOutput]> {
        Arc::clone(&self.expected_outputs)
    }

    pub(crate) fn shared_semantics(&self) -> Arc<super::semantics::ResolvedTranslationSemantics> {
        Arc::clone(&self.semantics)
    }
}

/// Planner 建立的确定顺序计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationPlan {
    semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
    preparation: TranslationPlanPreparation,
    tasks: Vec<RpgMakerExecutableTask>,
}

impl RpgMakerTranslationPlan {
    pub(crate) fn new(
        semantics: Arc<super::semantics::ResolvedTranslationSemantics>,
        preparation: TranslationPlanPreparation,
        tasks: Vec<RpgMakerExecutableTask>,
    ) -> Self {
        assert!(
            tasks
                .iter()
                .enumerate()
                .all(|(ordinal, task)| task.index().get() == ordinal),
            "RPG Maker 计划中的 TaskBlock 序号必须按自然计划顺序从零连续编号"
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
        Vec<RpgMakerExecutableTask>,
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
    expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
    was_current_rejected: bool,
}

impl TranslationPatch {
    #[cfg(test)]
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
            expected_previous: None,
            was_current_rejected: false,
        }
    }

    pub(crate) fn with_previous_and_rejected_state(
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationPropagationTarget>,
        translation: TextUnitContent,
        translation_state: Sha256Fingerprint,
        expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
        was_current_rejected: bool,
    ) -> Self {
        Self {
            identity,
            propagation_targets,
            translation,
            translation_state,
            expected_previous,
            was_current_rejected,
        }
    }

    pub(crate) const fn was_current_rejected(&self) -> bool {
        self.was_current_rejected
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

    pub(crate) fn expected_previous(&self) -> Option<(&TextUnitContent, Sha256Fingerprint)> {
        self.expected_previous
            .as_ref()
            .map(|(translation, state)| (translation, *state))
    }
}

/// 一个已经通过独立验收的任务 ID 及其可写 Patch。
///
/// ID 只属于本次 TaskBlock 协议，不进入资产表；保留在业务结果中用于准确关联
/// 已验收译文及其传播位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedTranslationDecision {
    id: TaskId,
    patch: TranslationPatch,
}

impl AcceptedTranslationDecision {
    pub(crate) fn new(id: TaskId, patch: TranslationPatch) -> Self {
        Self { id, patch }
    }

    pub(crate) const fn id(&self) -> TaskId {
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

/// 一个硬拒绝候选所绑定的当前 Unit 及该次规划语义状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedTranslationTarget {
    identity: TranslationUnitIdentity,
    planning_state: Sha256Fingerprint,
    expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
    was_current_rejected: bool,
}

impl RejectedTranslationTarget {
    #[cfg(test)]
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        planning_state: Sha256Fingerprint,
        expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
    ) -> Self {
        Self::with_rejected_state(identity, planning_state, expected_previous, false)
    }

    pub(crate) fn with_rejected_state(
        identity: TranslationUnitIdentity,
        planning_state: Sha256Fingerprint,
        expected_previous: Option<(TextUnitContent, Sha256Fingerprint)>,
        was_current_rejected: bool,
    ) -> Self {
        Self {
            identity,
            planning_state,
            expected_previous,
            was_current_rejected,
        }
    }

    pub(crate) const fn was_current_rejected(&self) -> bool {
        self.was_current_rejected
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) const fn planning_state(&self) -> Sha256Fingerprint {
        self.planning_state
    }

    pub(crate) fn expected_previous(&self) -> Option<(&TextUnitContent, Sha256Fingerprint)> {
        self.expected_previous
            .as_ref()
            .map(|(translation, state)| (translation, *state))
    }
}

/// 已唯一绑定且只因可证明不变量而未能成为有效译文的精确候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedTranslationCandidate {
    candidate_json: String,
    translation: Option<Vec<String>>,
    violation: ProvenInvariantViolation,
    targets: Vec<RejectedTranslationTarget>,
}

impl RejectedTranslationCandidate {
    pub(crate) fn new(
        candidate_json: String,
        translation: Option<Vec<String>>,
        violation: ProvenInvariantViolation,
        targets: Vec<RejectedTranslationTarget>,
    ) -> Self {
        assert!(!targets.is_empty(), "硬拒绝候选必须绑定至少一个当前 Unit");
        Self {
            candidate_json,
            translation,
            violation,
            targets,
        }
    }

    pub(crate) fn candidate_json(&self) -> &str {
        &self.candidate_json
    }

    pub(crate) fn translation(&self) -> Option<&[String]> {
        self.translation.as_deref()
    }

    pub(crate) fn violation(&self) -> &ProvenInvariantViolation {
        &self.violation
    }

    pub(crate) fn targets(&self) -> &[RejectedTranslationTarget] {
        &self.targets
    }
}

/// 一个预期 ID 没有形成可写译文的正常业务原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationUnitRejectionReason {
    Missing,
    Duplicate,
    InvalidShape {
        problem: TranslationAssistantValueError,
    },
    InvalidResponse,
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
    ContainsByteOrderMark,
    PlaceholderMismatch {
        token: String,
    },
    OrderMismatch {
        expected_token: String,
        actual_token: String,
    },
    WrapperTopologyChanged {
        token: String,
    },
    UnexpectedPlaceholderToken {
        token: String,
    },
    PlaceholderNormalizationAmbiguous {
        original: String,
    },
}

/// 一个仍需在后续 CLI 运行中重新翻译的预期单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnresolvedTranslationUnit {
    id: TaskId,
    unit: RpgMakerUnitLocator,
    reason: TranslationUnitRejectionReason,
    rejected_candidate: Option<RejectedTranslationCandidate>,
}

impl UnresolvedTranslationUnit {
    pub(crate) fn new(
        id: TaskId,
        unit: RpgMakerUnitLocator,
        reason: TranslationUnitRejectionReason,
    ) -> Self {
        Self {
            id,
            unit,
            reason,
            rejected_candidate: None,
        }
    }

    pub(crate) fn with_rejected_candidate(
        id: TaskId,
        unit: RpgMakerUnitLocator,
        reason: TranslationUnitRejectionReason,
        rejected_candidate: RejectedTranslationCandidate,
    ) -> Self {
        Self {
            id,
            unit,
            reason,
            rejected_candidate: Some(rejected_candidate),
        }
    }

    pub(crate) const fn id(&self) -> TaskId {
        self.id
    }

    /// 这个拒绝仍持有的完整 RPG Maker Unit 定位。
    ///
    /// Executor 在验收候选时建立它，后续诊断只能消费该事实，不能再按 ID 回查
    /// 可变计划或从展示文本猜测位置。
    pub(crate) fn unit(&self) -> &RpgMakerUnitLocator {
        &self.unit
    }

    pub(crate) fn reason(&self) -> &TranslationUnitRejectionReason {
        &self.reason
    }

    pub(crate) fn rejected_candidate(&self) -> Option<&RejectedTranslationCandidate> {
        self.rejected_candidate.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(id: TaskId, reason: TranslationUnitRejectionReason) -> Self {
        use crate::diagnostic::{
            RpgMakerDiagnosticGroupKind, RpgMakerDiagnosticLocation, RpgMakerDiagnosticOwner,
            RpgMakerDiagnosticRole, RpgMakerDiagnosticSource,
        };

        Self::new(
            id,
            RpgMakerUnitLocator::new(
                crate::diagnostic::SafeText::new("Items.json:1:description"),
                RpgMakerDiagnosticOwner::Builtin,
                RpgMakerDiagnosticGroupKind::DatabaseEntry,
                RpgMakerDiagnosticLocation::new(
                    RpgMakerDiagnosticSource::data("Items.json"),
                    Vec::new(),
                ),
                RpgMakerDiagnosticRole::scalar("description"),
            ),
            reason,
        )
    }
}

/// 无法绑定为某个可写译文、但必须进入结构化运行诊断的模型协议事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationProtocolDiagnostic {
    NonStopFinish {
        reason: RpgMakerModelNonStopFinishReason,
        finding: ReviewFinding,
    },
    CandidateReview {
        id: TaskId,
        unit: RpgMakerUnitLocator,
        finding: ReviewFinding,
    },
    InvalidResponse {
        error: TranslationTaskResponseParseError,
    },
    InvalidId {
        item_index: usize,
    },
    UnknownId {
        item_index: usize,
        id: TaskId,
    },
}

/// 一个任务没有任何可用译文的正常原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskUnavailableReason {
    ModelResponseUnusable,
    AllOutputsRejected,
    RequestAdmissionStopped {
        diagnostic: DiagnosticReport,
    },
    RecoverableRequestExhausted {
        diagnostic: DiagnosticReport,
        service_status: LlmServiceStatus,
    },
    RetryAfterExceedsConfiguredMaximum {
        retry_after: Duration,
        maximum: Duration,
        diagnostic: DiagnosticReport,
        service_status: LlmServiceStatus,
    },
}

impl TranslationTaskUnavailableReason {
    fn stops_admission(&self) -> bool {
        match self {
            Self::RequestAdmissionStopped { .. } => true,
            Self::RecoverableRequestExhausted { service_status, .. }
            | Self::RetryAfterExceedsConfiguredMaximum { service_status, .. } => {
                service_status.stops_admission_after_unavailable() || service_status.is_permanent()
            }
            Self::ModelResponseUnusable | Self::AllOutputsRejected => false,
        }
    }
}

/// 一个非空的任务决定集合。
///
/// 非空性由类型本身承载，避免任务结果另外保存可与内容矛盾的状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NonEmptyTaskItems<T> {
    items: Vec<T>,
}

impl<T> NonEmptyTaskItems<T> {
    #[cfg(test)]
    pub(crate) fn new(first: T, mut rest: Vec<T>) -> Self {
        let mut items = Vec::with_capacity(1 + rest.len());
        items.push(first);
        items.append(&mut rest);
        Self { items }
    }

    pub(crate) fn from_vec(items: Vec<T>) -> Option<Self> {
        (!items.is_empty()).then_some(Self { items })
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.items
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskOutcomeContext {
    task_index: RpgMakerTranslationTaskIndex,
    attempts: NonZeroUsize,
    diagnostics: Vec<TranslationProtocolDiagnostic>,
}

impl TranslationTaskOutcomeContext {
    pub(crate) fn new(
        task_index: RpgMakerTranslationTaskIndex,
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
        accepted: NonEmptyTaskItems<AcceptedTranslationDecision>,
    },
    Partial {
        context: TranslationTaskOutcomeContext,
        accepted: NonEmptyTaskItems<AcceptedTranslationDecision>,
        unresolved: NonEmptyTaskItems<UnresolvedTranslationUnit>,
    },
    Unavailable {
        context: TranslationTaskOutcomeContext,
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

    pub(crate) const fn task_index(&self) -> RpgMakerTranslationTaskIndex {
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

    pub(crate) fn rejected_candidate_count(&self) -> usize {
        self.unresolved()
            .iter()
            .filter(|unit| unit.rejected_candidate().is_some())
            .count()
    }

    pub(crate) fn rejected_location_count(&self) -> usize {
        self.unresolved()
            .iter()
            .filter_map(UnresolvedTranslationUnit::rejected_candidate)
            .map(|candidate| candidate.targets().len())
            .sum()
    }

    pub(crate) fn resolved_rejected_location_count(&self) -> usize {
        self.accepted()
            .iter()
            .map(|decision| {
                usize::from(decision.patch().was_current_rejected())
                    + decision
                        .propagation_targets()
                        .iter()
                        .filter(|target| target.was_current_rejected())
                        .count()
            })
            .sum()
    }

    pub(crate) fn newly_rejected_location_count(&self) -> usize {
        self.unresolved()
            .iter()
            .filter_map(UnresolvedTranslationUnit::rejected_candidate)
            .flat_map(RejectedTranslationCandidate::targets)
            .filter(|target| !target.was_current_rejected())
            .count()
    }

    pub(crate) fn accepted_location_count(&self) -> usize {
        self.accepted()
            .iter()
            .map(|decision| 1 + decision.propagation_targets().len())
            .sum()
    }
}

/// 将模型响应协议与单元验收仍持有的事实一次性投影成公开诊断。
///
/// 这必须发生在有完整 TaskBlock 和 Unit locator 的编排边界；项目日志和 CLI 只引用
/// 这里建立的 occurrence，不能由业务结果反推一个笼统的运行时错误。
fn task_response_report(
    scope: RpgMakerResponseProcessingScope,
    problem: RpgMakerTaskResponseProblem,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::rpg_maker(RpgMakerIssue::task_response(scope, problem)),
    )
}

/// 有序执行器在仍掌握计划位置时拒绝错序或缺失结果。
///
/// 这不是运行时字符串协议：RPG Maker 诊断保留期望与实际任务序号，终端错误和任务
/// 事件复用同一份报告。
fn task_result_sequence_report(
    expected_task_index: RpgMakerTranslationTaskIndex,
    actual_task_index: Option<RpgMakerTranslationTaskIndex>,
) -> DiagnosticReport {
    let problem = match actual_task_index {
        Some(actual_task_index) => RpgMakerResponseInvariantProblem::TaskResultIndexMismatch {
            expected_task_index: expected_task_index.get(),
            actual_task_index: actual_task_index.get(),
        },
        None => RpgMakerResponseInvariantProblem::TaskResultSequenceIncomplete {
            expected_task_index: expected_task_index.get(),
        },
    };
    DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::rpg_maker(RpgMakerIssue::response_processing(
            RpgMakerResponseProcessingScope::task(expected_task_index.get()),
            RpgMakerResponseProcessingProblem::InternalInvariant { problem },
        )),
    )
}

fn task_response_parse_problem(
    error: TranslationTaskResponseParseError,
) -> RpgMakerTaskResponseProblem {
    match error.kind() {
        TranslationTaskResponseParseErrorKind::Json(category) => {
            RpgMakerTaskResponseProblem::InvalidJson {
                category: match category {
                    TranslationTaskResponseJsonErrorCategory::Io => {
                        RpgMakerTaskResponseJsonCategory::Io
                    }
                    TranslationTaskResponseJsonErrorCategory::Syntax => {
                        RpgMakerTaskResponseJsonCategory::Syntax
                    }
                    TranslationTaskResponseJsonErrorCategory::Shape => {
                        RpgMakerTaskResponseJsonCategory::Shape
                    }
                    TranslationTaskResponseJsonErrorCategory::UnexpectedEof => {
                        RpgMakerTaskResponseJsonCategory::UnexpectedEof
                    }
                },
                line: error.line().get(),
                column: error.column().get(),
            }
        }
        TranslationTaskResponseParseErrorKind::ThinkingEmpty => {
            RpgMakerTaskResponseProblem::ThinkingEmpty {
                line: error.line().get(),
                column: error.column().get(),
            }
        }
    }
}

fn task_response_value_problem(
    problem: TranslationAssistantValueError,
) -> RpgMakerTaskResponseValueProblem {
    match problem {
        TranslationAssistantValueError::NotStringArray => {
            RpgMakerTaskResponseValueProblem::TranslationNotArray
        }
        TranslationAssistantValueError::NonStringItem { item } => {
            RpgMakerTaskResponseValueProblem::TranslationNonStringItem { item: item.get() }
        }
        TranslationAssistantValueError::SourceEchoNotObject => {
            RpgMakerTaskResponseValueProblem::SourceEchoNotObject
        }
        TranslationAssistantValueError::SourceEchoMissingSource => {
            RpgMakerTaskResponseValueProblem::SourceEchoMissingSource
        }
        TranslationAssistantValueError::SourceEchoMissingTranslation => {
            RpgMakerTaskResponseValueProblem::SourceEchoMissingTranslation
        }
        TranslationAssistantValueError::SourceEchoDuplicateSource => {
            RpgMakerTaskResponseValueProblem::SourceEchoDuplicateSource
        }
        TranslationAssistantValueError::SourceEchoDuplicateTranslation => {
            RpgMakerTaskResponseValueProblem::SourceEchoDuplicateTranslation
        }
        TranslationAssistantValueError::SourceEchoUnexpectedField => {
            RpgMakerTaskResponseValueProblem::SourceEchoUnexpectedField
        }
        TranslationAssistantValueError::SourceNotStringArray => {
            RpgMakerTaskResponseValueProblem::SourceNotArray
        }
        TranslationAssistantValueError::SourceNonStringItem { item } => {
            RpgMakerTaskResponseValueProblem::SourceNonStringItem { item: item.get() }
        }
    }
}

fn task_response_unit_problem(
    reason: &TranslationUnitRejectionReason,
) -> Option<RpgMakerTaskResponseUnitProblem> {
    Some(match reason {
        TranslationUnitRejectionReason::Missing => RpgMakerTaskResponseUnitProblem::Missing,
        TranslationUnitRejectionReason::Duplicate => RpgMakerTaskResponseUnitProblem::Duplicate,
        TranslationUnitRejectionReason::InvalidShape { problem } => {
            RpgMakerTaskResponseUnitProblem::InvalidValue {
                problem: task_response_value_problem(*problem),
            }
        }
        // JSON/Thinking 解析失败属于整个 TaskBlock，已经由协议诊断精确表达；不能伪造
        // 每个 Unit 各自发生了形状错误。
        TranslationUnitRejectionReason::InvalidResponse => return None,
        TranslationUnitRejectionReason::LineCountMismatch { expected, actual } => {
            RpgMakerTaskResponseUnitProblem::LineCountMismatch {
                expected: *expected,
                actual: *actual,
            }
        }
        TranslationUnitRejectionReason::InvalidLineText { line_index } => {
            RpgMakerTaskResponseUnitProblem::InvalidLineText {
                line_index: *line_index,
            }
        }
        TranslationUnitRejectionReason::BlankLineMismatch {
            line_index,
            expected_blank,
        } => RpgMakerTaskResponseUnitProblem::BlankLineMismatch {
            line_index: *line_index,
            expected_blank: *expected_blank,
        },
        TranslationUnitRejectionReason::BlankTranslation => {
            RpgMakerTaskResponseUnitProblem::BlankTranslation
        }
        TranslationUnitRejectionReason::ContainsByteOrderMark => {
            RpgMakerTaskResponseUnitProblem::ContainsByteOrderMark
        }
        TranslationUnitRejectionReason::PlaceholderMismatch { .. } => {
            RpgMakerTaskResponseUnitProblem::PlaceholderMismatch
        }
        TranslationUnitRejectionReason::OrderMismatch { .. } => {
            RpgMakerTaskResponseUnitProblem::OrderMismatch
        }
        TranslationUnitRejectionReason::WrapperTopologyChanged { .. } => {
            RpgMakerTaskResponseUnitProblem::WrapperTopologyChanged
        }
        TranslationUnitRejectionReason::UnexpectedPlaceholderToken { .. } => {
            RpgMakerTaskResponseUnitProblem::UnexpectedPlaceholderToken
        }
        TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. } => {
            RpgMakerTaskResponseUnitProblem::PlaceholderNormalizationAmbiguous
        }
    })
}

fn task_response_protocol_report(
    task_index: RpgMakerTranslationTaskIndex,
    diagnostic: &TranslationProtocolDiagnostic,
    applied_any_output: bool,
) -> DiagnosticReport {
    let (scope, problem) = match diagnostic {
        TranslationProtocolDiagnostic::NonStopFinish { reason, finding } => {
            debug_assert_eq!(finding, &ReviewFinding::NonStopFinish);
            (
                RpgMakerResponseProcessingScope::task(task_index.get()),
                RpgMakerTaskResponseProblem::NonStopFinish {
                    reason: reason.clone(),
                },
            )
        }
        TranslationProtocolDiagnostic::CandidateReview { id, unit, finding } => (
            RpgMakerResponseProcessingScope::unit(task_index.get(), unit.clone()),
            RpgMakerTaskResponseProblem::UnitReview {
                output_id: id.get(),
                finding: match finding {
                    ReviewFinding::SourceResidual => {
                        RpgMakerTaskResponseReviewProblem::SourceResidual
                    }
                    ReviewFinding::NonStopFinish => {
                        unreachable!("候选级 Review 不会产生 finish reason")
                    }
                },
            },
        ),
        TranslationProtocolDiagnostic::InvalidResponse { error } => (
            RpgMakerResponseProcessingScope::task(task_index.get()),
            task_response_parse_problem(*error),
        ),
        TranslationProtocolDiagnostic::InvalidId { item_index } => (
            RpgMakerResponseProcessingScope::task(task_index.get()),
            RpgMakerTaskResponseProblem::InvalidId {
                item_index: *item_index,
            },
        ),
        TranslationProtocolDiagnostic::UnknownId { item_index, id } => (
            RpgMakerResponseProcessingScope::task(task_index.get()),
            RpgMakerTaskResponseProblem::UnknownId {
                item_index: *item_index,
                output_id: id.get(),
            },
        ),
    };
    let effect = match &problem {
        RpgMakerTaskResponseProblem::UnitReview { .. } => StateEffect::Applied,
        RpgMakerTaskResponseProblem::NonStopFinish { .. } if applied_any_output => {
            StateEffect::Applied
        }
        _ => StateEffect::ProgressPreserved,
    };
    task_response_report(scope, problem, effect)
}

fn task_response_unit_report(
    task_index: RpgMakerTranslationTaskIndex,
    unresolved: &UnresolvedTranslationUnit,
) -> Option<DiagnosticReport> {
    let problem = task_response_unit_problem(unresolved.reason())?;
    Some(task_response_report(
        RpgMakerResponseProcessingScope::unit(task_index.get(), unresolved.unit().clone()),
        RpgMakerTaskResponseProblem::UnitRejected {
            output_id: unresolved.id().get(),
            problem,
        },
        StateEffect::ProgressPreserved,
    ))
}

/// 为本次正常业务结果取得它全部、且不泄露模型正文的诊断。
///
/// `Partial` 与 `Unavailable` 必定至少产生一条报告：前者来自被拒绝 Unit，后者来自
/// 解析、请求或全部拒绝事实。因此 ProjectLog 不再需要用内部不变量补洞。
fn task_outcome_diagnostics(
    task: &RpgMakerExecutableTask,
    outcome: &TranslationTaskOutcome,
) -> Vec<DiagnosticReport> {
    let task_index = task.index();
    let applied_any_output = !outcome.accepted().is_empty();
    let protocol_reports = || {
        outcome
            .diagnostics()
            .iter()
            .map(|diagnostic| {
                task_response_protocol_report(task_index, diagnostic, applied_any_output)
            })
            .collect::<Vec<_>>()
    };
    let unit_reports = || {
        outcome
            .unresolved()
            .iter()
            .filter_map(|unresolved| task_response_unit_report(task_index, unresolved))
            .collect::<Vec<_>>()
    };

    match outcome {
        TranslationTaskOutcome::Complete { .. } => protocol_reports(),
        TranslationTaskOutcome::Partial { .. } => {
            let mut reports = unit_reports();
            reports.extend(protocol_reports());
            debug_assert!(!reports.is_empty());
            reports
        }
        TranslationTaskOutcome::Unavailable { reason, .. } => match reason {
            TranslationTaskUnavailableReason::RequestAdmissionStopped { diagnostic }
            | TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                diagnostic, ..
            }
            | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                diagnostic,
                ..
            } => {
                vec![diagnostic.clone()]
            }
            TranslationTaskUnavailableReason::ModelResponseUnusable => {
                let reports = protocol_reports();
                if reports.is_empty() {
                    vec![task_response_report(
                        RpgMakerResponseProcessingScope::task(task_index.get()),
                        RpgMakerTaskResponseProblem::ModelResponseUnusable,
                        StateEffect::ProgressPreserved,
                    )]
                } else {
                    reports
                }
            }
            TranslationTaskUnavailableReason::AllOutputsRejected => {
                let mut reports = unit_reports();
                reports.extend(protocol_reports());
                if reports.is_empty() {
                    reports.push(task_response_report(
                        RpgMakerResponseProcessingScope::task(task_index.get()),
                        RpgMakerTaskResponseProblem::AllOutputsRejected,
                        StateEffect::ProgressPreserved,
                    ));
                }
                reports
            }
        },
    }
}

/// 一次 RPG Maker 运行已经确认的正常业务汇总。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationRunReport {
    total_tasks: usize,
    complete_tasks: usize,
    partial_tasks: usize,
    unavailable_tasks: usize,
    accepted_decisions: usize,
    written_locations: usize,
    unresolved_decisions: usize,
    unresolved_locations: usize,
    rejected_locations: usize,
    protocol_diagnostics: usize,
    recoverable_request_exhaustions: usize,
    request_admission_stopped: bool,
    retained: usize,
    invalidated: usize,
    not_applicable: usize,
    reused: usize,
}

impl RpgMakerTranslationRunReport {
    #[cfg(test)]
    pub(crate) const fn with_reconciliation(
        total_tasks: usize,
        planned_decisions: usize,
        planned_locations: usize,
        retained: usize,
        invalidated: usize,
        not_applicable: usize,
        reused: usize,
    ) -> Self {
        Self {
            total_tasks,
            complete_tasks: 0,
            partial_tasks: 0,
            unavailable_tasks: 0,
            accepted_decisions: 0,
            written_locations: 0,
            unresolved_decisions: planned_decisions,
            unresolved_locations: planned_locations,
            rejected_locations: 0,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 0,
            request_admission_stopped: false,
            retained,
            invalidated,
            not_applicable,
            reused,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_summary_for_test(summary: super::TranslationSummary) -> Self {
        assert_eq!(
            summary.started_tasks,
            summary.complete_tasks + summary.partial_tasks + summary.unavailable_tasks
        );
        assert_eq!(
            summary.total_tasks,
            summary.started_tasks + summary.not_started_tasks
        );
        Self {
            total_tasks: summary.total_tasks,
            complete_tasks: summary.complete_tasks,
            partial_tasks: summary.partial_tasks,
            unavailable_tasks: summary.unavailable_tasks,
            accepted_decisions: summary.accepted_decisions,
            written_locations: summary.written_locations,
            unresolved_decisions: summary.remaining_decisions,
            unresolved_locations: summary.remaining_locations,
            rejected_locations: summary.rejected_locations,
            protocol_diagnostics: summary.protocol_diagnostics,
            recoverable_request_exhaustions: summary.recoverable_request_exhaustions,
            request_admission_stopped: summary.request_admission_stopped,
            retained: summary.retained,
            invalidated: summary.invalidated,
            not_applicable: summary.not_applicable,
            reused: summary.reused,
        }
    }

    pub(crate) fn from_plan(
        total_tasks: usize,
        planned_decisions: usize,
        planned_locations: usize,
        rejected_locations: usize,
        preparation: &TranslationPlanPreparation,
    ) -> Self {
        Self {
            total_tasks,
            complete_tasks: 0,
            partial_tasks: 0,
            unavailable_tasks: 0,
            accepted_decisions: 0,
            written_locations: 0,
            unresolved_decisions: planned_decisions,
            unresolved_locations: planned_locations,
            rejected_locations,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 0,
            request_admission_stopped: false,
            retained: preparation.retained(),
            invalidated: preparation.invalidated(),
            not_applicable: preparation.not_applicable(),
            reused: preparation.reused(),
        }
    }

    #[cfg(test)]
    const fn with_initial_rejected_for_test(mut self, rejected_locations: usize) -> Self {
        self.rejected_locations = rejected_locations;
        self
    }

    pub(crate) fn record_preparation_applied(
        &mut self,
        rejected_locations: usize,
        resolved_rejected_locations: usize,
    ) {
        self.unresolved_decisions = self
            .unresolved_decisions
            .checked_sub(resolved_rejected_locations)
            .expect("准备阶段修复的 Rejected 不得超过剩余决策");
        self.unresolved_locations = self
            .unresolved_locations
            .checked_sub(resolved_rejected_locations)
            .expect("准备阶段修复的 Rejected 不得超过剩余位置");
        self.rejected_locations = rejected_locations;
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
                self.request_admission_stopped |= reason.stops_admission();
            }
        }
        self.accepted_decisions += outcome.accepted().len();
        self.written_locations += outcome.accepted_location_count();
        self.rejected_locations = self
            .rejected_locations
            .checked_sub(outcome.resolved_rejected_location_count())
            .and_then(|value| value.checked_add(outcome.newly_rejected_location_count()))
            .expect("RPG Maker Task 的 Rejected 终态计数必须保持有效");
        self.unresolved_decisions = self
            .unresolved_decisions
            .checked_sub(outcome.accepted().len())
            .expect("已接受决策不得超过本次计划决策数");
        self.unresolved_locations = self
            .unresolved_locations
            .checked_sub(outcome.accepted_location_count())
            .expect("已写入位置不得超过本次计划位置数");
        self.protocol_diagnostics += outcome.diagnostics().len();
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

    pub(crate) const fn started_tasks(&self) -> usize {
        self.complete_tasks + self.partial_tasks + self.unavailable_tasks
    }

    pub(crate) const fn not_started_tasks(&self) -> usize {
        self.total_tasks.saturating_sub(self.started_tasks())
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

    pub(crate) const fn rejected_locations(&self) -> usize {
        self.rejected_locations
    }

    pub(crate) const fn protocol_diagnostics(&self) -> usize {
        self.protocol_diagnostics
    }

    pub(crate) const fn recoverable_request_exhaustions(&self) -> usize {
        self.recoverable_request_exhaustions
    }

    pub(crate) const fn request_admission_stopped(&self) -> bool {
        self.request_admission_stopped
    }

    fn mark_request_admission_stopped(&mut self) {
        self.request_admission_stopped = true;
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

/// 在一个一致读视图中取得 RPG Maker 文本表的当前事实。
pub(crate) trait RpgMakerTranslationAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<RpgMakerTranslationCorpus, Self::Error>> + Send;
}

/// 把当前语料与本次外部资料建立为确定性翻译计划。
///
/// Planner 完整拥有 RPG Maker 自然排序、语义边界切块、源语言判定、虚原文、术语影响、
/// PCRE2 占位符保护和 Markdown 消息构造的上游承诺。RPG Maker 只依赖它返回的计划，
/// 不跨过 Planner 重新解释这些规则。
///
/// Planner 必须先在最大仍有关联的 RPG Maker 结构范围内组织复合 Group，再按外部 Profile
/// 提供的 user message 字符装箱目标切割；不得为了接近目标拼接无关范围。单个完整 Group
/// 超过目标时独立成块，不能拆组或拒绝规范内容，后续任务继续使用原目标。每个 TaskBlock
/// 内待翻译单元的 ID 从 0 连续递增，虚原文只保留原文且没有 ID。省略外部资源时复用项目
/// 当前快照；显式资源在全部解析成功后成为新快照。Planner 按每个语义单元实际触发的术语
/// 和占位符语义对账，并把资源更新、失效清理与可复用传播一并写入 Preparation。
pub(crate) trait RpgMakerTranslationTaskPlanner: Send + Sync {
    type Profile: RpgMakerTranslationExecutionProfile;
    type Error: Error + Send + Sync + 'static;

    /// 只根据 Planner 返回的类型化错误判断这次失败是否就是合作取消。
    ///
    /// 调用方不能再读取共享取消标志覆盖一个已经形成的真实规划错误。
    fn is_cancelled_error(error: &Self::Error) -> bool;

    fn plan(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        corpus: RpgMakerTranslationCorpus,
        input: RpgMakerTranslationInput,
    ) -> impl Future<Output = Result<RpgMakerTranslationPlan, Self::Error>> + Send;
}

/// 执行一个已计划任务并返回正常业务结果。
///
/// 只有可恢复网络请求可以按外部预算重试。模型内容全部、部分或完全没有形成译文
/// 都由 `TranslationTaskOutcome` 表达，不得转换成错误或阻断其他任务。
/// Executor 不写项目数据库。
/// Executor 只能把 TaskBlock 已经建立的完整 `messages` 发送给 LLM；结构化位置、
/// 表归属、占位符反查和提交身份只用于程序内部验收，不能再拼入提示词。
pub(crate) trait RpgMakerTranslationTaskExecutor: Send + Sync {
    type Profile: RpgMakerTranslationExecutionProfile;
    type Error: Error + Send + Sync + 'static;

    /// 外部请求失败已经停止新任务准入时，是否仍应提交其他已准入任务的验收结果。
    fn failure_preserves_admitted_results(_error: &Self::Error) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        profile: &'a Self::Profile,
        task: &'a RpgMakerExecutableTask,
        on_task_started: Box<dyn FnOnce() + Send + 'a>,
    ) -> impl Future<
        Output = Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>>,
    > + Send
    + 'a;
}

/// 拥有 RPG Maker 译文准备与单任务提交事务的存储边界。
pub(crate) trait RpgMakerTranslationResultStore: Send + Sync {
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

    /// 显式终结本轮 RPG Maker 翻译持有的存储会话。
    ///
    /// 无论主流程成功、失败或取消都会调用；实现必须完成残留事务观察、回滚和连接关闭，
    /// 并把收尾失败作为独立事实返回。
    fn finalize(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 完成一轮项目数据库 RPG Maker 资产翻译的职责契约。
///
/// 成功表示所有计划任务都已经得到正常业务结果并按序完成必要提交，不表示所有原文
/// 都获得了译文。模型内容不可用和可恢复网络预算耗尽都不会阻断后续任务。
/// 只有依赖无法继续履行契约时才返回错误。
pub(crate) trait RpgMakerTranslation: Send + Sync {
    /// 与配置解析器产物一致的执行配置。
    type Profile: Send + Sync + 'static;
    /// RPG Maker 翻译失败。
    type Error: Error + Send + Sync + 'static;

    /// 使用指定配置完成一次 RPG Maker 资产翻译。
    fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        input: RpgMakerTranslationInput,
    ) -> impl Future<Output = Result<OperationCompletion<RpgMakerTranslationRunReport>, Self::Error>>
    + Send;
}

/// RPG Maker 翻译向普通 JSONL 可观测性边界提交的摘要业务事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerTranslationLogEvent {
    PlanningCompleted {
        report: RpgMakerTranslationRunReport,
    },
    PreparationApplied {
        report: RpgMakerTranslationRunReport,
    },
    TaskStarted {
        task_index: RpgMakerTranslationTaskIndex,
        total_tasks: usize,
    },
    TaskFinished {
        task_index: RpgMakerTranslationTaskIndex,
        outcome: RpgMakerTranslationLogTaskOutcome,
        attempts: Option<NonZeroUsize>,
        provider: Option<String>,
        retry_exhausted: bool,
        report: RpgMakerTranslationRunReport,
    },
}

/// 一个业务终态必须引用的一组诊断。
///
/// 首项是该 TaskFinished 的主 occurrence，余项是同一响应中其余被拒绝 Unit 或协议
/// 问题。调用方不能构造空集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTaskDiagnosticReports {
    primary: DiagnosticReport,
    related: Vec<DiagnosticReport>,
}

impl RpgMakerTaskDiagnosticReports {
    #[cfg(test)]
    pub(crate) fn for_test(primary: DiagnosticReport) -> Self {
        Self {
            primary,
            related: Vec::new(),
        }
    }

    fn from_reports(reports: Vec<DiagnosticReport>) -> Self {
        let mut reports = reports.into_iter();
        let primary = reports
            .next()
            .expect("部分、不可用或失败的 RPG Maker 翻译任务必须在拥有原始边界事实时建立诊断");
        Self {
            primary,
            related: reports.collect(),
        }
    }

    fn reports(&self) -> impl Iterator<Item = &DiagnosticReport> {
        std::iter::once(&self.primary).chain(self.related.iter())
    }
}

/// 一项任务在顺序最终化边界得到的可观察终态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerTranslationLogTaskOutcome {
    Complete {
        diagnostics: Vec<DiagnosticReport>,
    },
    Partial {
        diagnostics: RpgMakerTaskDiagnosticReports,
    },
    Unavailable {
        diagnostics: RpgMakerTaskDiagnosticReports,
    },
    ExecutionFailed {
        diagnostic: DiagnosticReport,
    },
    CommitFailed {
        diagnostic: DiagnosticReport,
    },
    Cancelled,
    NotCommittedAfterEarlierFailure {
        /// 导致当前任务不再提交的首个真实失败；ProjectLog 复用其既有 occurrence。
        cause: DiagnosticReport,
    },
    InvalidResult {
        diagnostic: DiagnosticReport,
    },
}

impl RpgMakerTranslationLogTaskOutcome {
    pub(crate) fn diagnostics(&self) -> Box<dyn Iterator<Item = &DiagnosticReport> + '_> {
        match self {
            Self::Complete { diagnostics } => Box::new(diagnostics.iter()),
            Self::Partial { diagnostics } | Self::Unavailable { diagnostics } => {
                Box::new(diagnostics.reports())
            }
            Self::ExecutionFailed { diagnostic }
            | Self::CommitFailed { diagnostic }
            | Self::InvalidResult { diagnostic } => Box::new(std::iter::once(diagnostic)),
            Self::Cancelled | Self::NotCommittedAfterEarlierFailure { .. } => {
                Box::new(std::iter::empty())
            }
        }
    }
}

/// 同步、不可失败的 RPG Maker 翻译观察入口。
pub(crate) trait RpgMakerTranslationLog: Send + Sync {
    fn emit(&self, event: RpgMakerTranslationLogEvent);
}

enum PreparedTranslationTask<C> {
    Started {
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        prepared_commit: Option<C>,
    },
    AdmissionStopped,
}

enum TranslationTaskStageError<E, S> {
    Execution {
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: DiagnosticReport,
        preserve_admitted_results: bool,
    },
    Cancelled {
        source: E,
        evidence: TranslationTaskExecutionEvidence,
    },
    InvalidResult {
        actual_task_index: RpgMakerTranslationTaskIndex,
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
    },
    CommitPreparation {
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        failure: TranslationTaskCommitFailure<S>,
    },
}

#[derive(Debug)]
enum TranslationTaskPipelineError<E, S> {
    ExecuteTask {
        task_index: RpgMakerTranslationTaskIndex,
        source: E,
        diagnostic: DiagnosticReport,
        preserve_admitted_results: bool,
    },
    CommitTask {
        task_index: RpgMakerTranslationTaskIndex,
        source: S,
        diagnostic: DiagnosticReport,
    },
    InvalidTaskResultSequence {
        expected_task_index: RpgMakerTranslationTaskIndex,
        actual_task_index: Option<RpgMakerTranslationTaskIndex>,
        diagnostic: DiagnosticReport,
    },
}

impl<E, S> fmt::Display for TranslationTaskPipelineError<E, S>
where
    E: fmt::Display,
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecuteTask {
                task_index, source, ..
            } => {
                write!(
                    formatter,
                    "RPG Maker 翻译任务 {task_index} 执行失败：{source}"
                )
            }
            Self::CommitTask {
                task_index, source, ..
            } => {
                write!(
                    formatter,
                    "RPG Maker 翻译任务 {task_index} 提交失败：{source}"
                )
            }
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
                ..
            } => write!(
                formatter,
                "RPG Maker 翻译结果序列损坏：期待任务 {expected_task_index}，却收到任务 {actual_task_index}"
            ),
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: None,
                ..
            } => write!(
                formatter,
                "RPG Maker 翻译结果序列不完整：执行通道在任务 {expected_task_index} 返回前关闭"
            ),
        }
    }
}

impl<E, S> Error for TranslationTaskPipelineError<E, S>
where
    E: Error + 'static,
    S: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecuteTask { source, .. } => Some(source),
            Self::CommitTask { source, .. } => Some(source),
            Self::InvalidTaskResultSequence { .. } => None,
        }
    }
}

/// 使用四个业务能力和不可失败观察入口编排一次 RPG Maker 资产翻译。
pub(crate) struct RpgMakerTranslationService<R, P, E, S, J, K = NoOpTranslationTaskRecordSink> {
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

impl<R, P, E, S, J> RpgMakerTranslationService<R, P, E, S, J, NoOpTranslationTaskRecordSink> {
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

impl<R, P, E, S, J, K> RpgMakerTranslationService<R, P, E, S, J, K> {
    pub(crate) fn with_task_record_sink<N>(
        self,
        task_records: N,
    ) -> RpgMakerTranslationService<R, P, E, S, J, N> {
        RpgMakerTranslationService {
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

impl<R, P, E, S, J, K> RpgMakerTranslationService<R, P, E, S, J, K>
where
    P: RpgMakerTranslationTaskPlanner,
    E: RpgMakerTranslationTaskExecutor<Profile = P::Profile>,
    S: RpgMakerTranslationResultStore,
    J: RpgMakerTranslationLog,
    K: TranslationTaskRecordSink,
{
    fn record_task(&self, document: impl FnOnce() -> TranslationTaskRecordDocument) {
        if self.task_records.enabled() {
            self.task_records.submit(document());
        }
    }
}

struct RpgMakerOrderedExecutionHandler<'a, R, P, E, S, J, K>
where
    P: RpgMakerTranslationTaskPlanner,
{
    service: &'a RpgMakerTranslationService<R, P, E, S, J, K>,
    project: &'a OpenedProject,
    profile: &'a P::Profile,
    total_tasks: usize,
    prior_failure_diagnostic: Mutex<Option<DiagnosticReport>>,
}

impl<R, P, E, S, J, K> OrderedExecutionHandler<RpgMakerExecutableTask>
    for RpgMakerOrderedExecutionHandler<'_, R, P, E, S, J, K>
where
    R: RpgMakerTranslationAssetReader,
    P: RpgMakerTranslationTaskPlanner,
    E: RpgMakerTranslationTaskExecutor<Profile = P::Profile>,
    S: RpgMakerTranslationResultStore,
    J: RpgMakerTranslationLog,
    K: TranslationTaskRecordSink,
{
    type Executed = TranslationTaskExecution;
    type Prepared = PreparedTranslationTask<S::PreparedCommit>;
    type StageError = TranslationTaskStageError<E::Error, S::Error>;
    type State = RpgMakerTranslationRunReport;
    type Error = TranslationTaskPipelineError<E::Error, S::Error>;

    async fn execute(
        &self,
        _ordinal: usize,
        task: &RpgMakerExecutableTask,
    ) -> Result<Self::Executed, Self::StageError> {
        let task_index = task.index();
        let event_log = &self.service.event_log;
        let total_tasks = self.total_tasks;
        match self
            .service
            .task_executor
            .execute(
                self.profile,
                task,
                Box::new(move || {
                    event_log.emit(RpgMakerTranslationLogEvent::TaskStarted {
                        task_index,
                        total_tasks,
                    });
                }),
            )
            .await
        {
            Ok(execution) => Ok(execution),
            Err(TranslationTaskExecutionFailure::Failed {
                source,
                evidence,
                diagnostic,
            }) => {
                let preserve_admitted_results = E::failure_preserves_admitted_results(&source);
                Err(TranslationTaskStageError::Execution {
                    source,
                    evidence,
                    diagnostic,
                    preserve_admitted_results,
                })
            }
            Err(TranslationTaskExecutionFailure::Cancelled { source, evidence }) => {
                Err(TranslationTaskStageError::Cancelled { source, evidence })
            }
        }
    }

    async fn prepare(
        &self,
        _ordinal: usize,
        task: &RpgMakerExecutableTask,
        execution: Self::Executed,
    ) -> Result<Self::Prepared, Self::StageError> {
        let task_index = task.index();
        let (state, evidence) = execution.into_parts();
        let outcome = match state {
            TranslationTaskExecutionState::Started(outcome) => outcome,
            TranslationTaskExecutionState::AdmissionStopped => {
                return Ok(PreparedTranslationTask::AdmissionStopped);
            }
        };
        let outcome = Arc::new(outcome);
        if outcome.task_index() != task_index {
            return Err(TranslationTaskStageError::InvalidResult {
                actual_task_index: outcome.task_index(),
                outcome,
                evidence,
            });
        }
        let prepared_commit =
            if outcome.accepted().is_empty() && outcome.rejected_candidate_count() == 0 {
                None
            } else {
                match self
                    .service
                    .result_store
                    .prepare_commit(Arc::clone(&outcome))
                    .await
                {
                    Ok(prepared) => Some(prepared),
                    Err(failure) => {
                        return Err(TranslationTaskStageError::CommitPreparation {
                            outcome,
                            evidence,
                            failure,
                        });
                    }
                }
            };
        Ok(PreparedTranslationTask::Started {
            outcome,
            evidence,
            prepared_commit,
        })
    }

    fn executed_stops_admission(&self, _ordinal: usize, executed: &Self::Executed) -> bool {
        executed.admission_was_stopped()
            || matches!(
                executed.outcome(),
                Some(TranslationTaskOutcome::Unavailable { reason, .. }) if reason.stops_admission()
            )
    }

    async fn finalize(
        &self,
        _ordinal: usize,
        task: RpgMakerExecutableTask,
        result: OrderedTaskResult<Self::Prepared, Self::StageError>,
        disposition: OrderedFinalizationDisposition,
        report: &mut Self::State,
    ) -> Result<(), Self::Error> {
        let scheduled_task_index = task.index();
        match result {
            OrderedTaskResult::ExecutionFailed(TranslationTaskStageError::Execution {
                source,
                evidence,
                diagnostic,
                preserve_admitted_results,
            }) => {
                let attempts = NonZeroUsize::new(evidence.attempt_count());
                if preserve_admitted_results {
                    report.mark_request_admission_stopped();
                }
                self.remember_failure_diagnostic(&diagnostic);
                if attempts.is_some() {
                    self.service
                        .event_log
                        .emit(RpgMakerTranslationLogEvent::TaskFinished {
                            task_index: scheduled_task_index,
                            outcome: RpgMakerTranslationLogTaskOutcome::ExecutionFailed {
                                diagnostic: diagnostic.clone(),
                            },
                            attempts,
                            provider: evidence.provider().map(str::to_owned),
                            retry_exhausted: false,
                            report: report.clone(),
                        });
                    self.service.record_task(|| {
                        TranslationTaskRecordDocument::new(
                            task,
                            evidence,
                            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                                diagnostic: diagnostic.clone(),
                            },
                        )
                    });
                }
                Err(TranslationTaskPipelineError::ExecuteTask {
                    task_index: scheduled_task_index,
                    source,
                    diagnostic,
                    preserve_admitted_results,
                })
            }
            OrderedTaskResult::ExecutionFailed(TranslationTaskStageError::Cancelled {
                source,
                evidence,
            }) => {
                let attempts = NonZeroUsize::new(evidence.attempt_count());
                if attempts.is_some() {
                    self.service
                        .event_log
                        .emit(RpgMakerTranslationLogEvent::TaskFinished {
                            task_index: scheduled_task_index,
                            outcome: RpgMakerTranslationLogTaskOutcome::Cancelled,
                            attempts,
                            provider: evidence.provider().map(str::to_owned),
                            retry_exhausted: false,
                            report: report.clone(),
                        });
                    self.service.record_task(|| {
                        TranslationTaskRecordDocument::new(
                            task,
                            evidence,
                            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
                        )
                    });
                }
                drop(source);
                Ok(())
            }
            OrderedTaskResult::PreparationFailed(TranslationTaskStageError::InvalidResult {
                actual_task_index,
                outcome,
                evidence,
            }) => {
                let diagnostic =
                    task_result_sequence_report(scheduled_task_index, Some(actual_task_index));
                self.remember_failure_diagnostic(&diagnostic);
                if evidence.attempt_count() > 0 {
                    self.service
                        .event_log
                        .emit(RpgMakerTranslationLogEvent::TaskFinished {
                            task_index: scheduled_task_index,
                            outcome: RpgMakerTranslationLogTaskOutcome::InvalidResult {
                                diagnostic: diagnostic.clone(),
                            },
                            attempts: NonZeroUsize::new(evidence.attempt_count()),
                            provider: evidence.provider().map(str::to_owned),
                            retry_exhausted: false,
                            report: report.clone(),
                        });
                    self.service.record_task(|| {
                        TranslationTaskRecordDocument::new(
                            task,
                            evidence,
                            TranslationTaskRecordFinalState::InvalidResultNoChanges {
                                outcome: Arc::clone(&outcome),
                                diagnostic: diagnostic.clone(),
                            },
                        )
                    });
                }
                Err(TranslationTaskPipelineError::InvalidTaskResultSequence {
                    expected_task_index: scheduled_task_index,
                    actual_task_index: Some(actual_task_index),
                    diagnostic,
                })
            }
            OrderedTaskResult::PreparationFailed(
                TranslationTaskStageError::CommitPreparation {
                    outcome,
                    evidence,
                    failure,
                },
            ) => self.record_commit_failure(
                task,
                outcome,
                evidence,
                TranslationTaskCommitPhase::Preparation,
                failure,
                report,
            ),
            OrderedTaskResult::Prepared(PreparedTranslationTask::AdmissionStopped) => {
                report.mark_request_admission_stopped();
                Ok(())
            }
            OrderedTaskResult::Prepared(PreparedTranslationTask::Started {
                outcome,
                evidence,
                prepared_commit,
            }) => {
                // Unavailable 等没有任何已验收译文的结果不需要提交。它们已经形成自己的
                // 业务终态，不能仅因同时收到取消或更早任务失败而伪装成“未提交”。
                if prepared_commit.is_none() {
                    self.record_success(task, outcome, evidence, report);
                    return Ok(());
                }
                match disposition {
                    OrderedFinalizationDisposition::CancelledNoApply => {
                        self.record_not_applied(
                            task,
                            outcome,
                            evidence,
                            TranslationTaskRecordFinalStateKind::Cancelled,
                            report,
                        );
                        Ok(())
                    }
                    OrderedFinalizationDisposition::AfterEarlierFailureNoApply => {
                        self.record_not_applied(
                            task,
                            outcome,
                            evidence,
                            TranslationTaskRecordFinalStateKind::EarlierFailure,
                            report,
                        );
                        Ok(())
                    }
                    OrderedFinalizationDisposition::Apply => {
                        if let Some(prepared_commit) = prepared_commit
                            && let Err(failure) = self
                                .service
                                .result_store
                                .commit_prepared(self.project, prepared_commit)
                                .await
                        {
                            return self.record_commit_failure(
                                task,
                                outcome,
                                evidence,
                                TranslationTaskCommitPhase::Transaction,
                                failure,
                                report,
                            );
                        }
                        self.record_success(task, outcome, evidence, report);
                        Ok(())
                    }
                }
            }
            OrderedTaskResult::ExecutionFailed(source)
            | OrderedTaskResult::PreparationFailed(source) => {
                panic!(
                    "有序流水线必须按 RPG Maker 阶段返回对应错误：{}",
                    translation_stage_error_kind(&source)
                )
            }
        }
    }

    fn prepared_stops_admission(&self, _ordinal: usize, prepared: &Self::Prepared) -> bool {
        match prepared {
            PreparedTranslationTask::AdmissionStopped => true,
            PreparedTranslationTask::Started { outcome, .. } => matches!(
                outcome.as_ref(),
                TranslationTaskOutcome::Unavailable { reason, .. } if reason.stops_admission()
            ),
        }
    }

    fn finalization_failure_preserves_admitted_results(
        &self,
        _ordinal: usize,
        error: &Self::Error,
    ) -> bool {
        matches!(
            error,
            TranslationTaskPipelineError::ExecuteTask {
                preserve_admitted_results: true,
                ..
            }
        )
    }

    fn admission_stopped(&self) {
        #[cfg(test)]
        if let Some(notify) = &self.service.stop_admission_notify {
            notify.notify_one();
        }
    }
}

#[derive(Clone, Copy)]
enum TranslationTaskRecordFinalStateKind {
    Cancelled,
    EarlierFailure,
}

impl<R, P, E, S, J, K> RpgMakerOrderedExecutionHandler<'_, R, P, E, S, J, K>
where
    P: RpgMakerTranslationTaskPlanner,
    E: RpgMakerTranslationTaskExecutor<Profile = P::Profile>,
    S: RpgMakerTranslationResultStore,
    J: RpgMakerTranslationLog,
    K: TranslationTaskRecordSink,
{
    fn remember_failure_diagnostic(&self, diagnostic: &DiagnosticReport) {
        let mut prior = self
            .prior_failure_diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if prior.is_none() {
            *prior = Some(diagnostic.clone());
        }
    }

    fn prior_failure_diagnostic(&self) -> DiagnosticReport {
        self.prior_failure_diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("前序失败后停止提交的任务必须保留首个失败诊断")
    }

    fn record_not_applied(
        &self,
        task: RpgMakerExecutableTask,
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        kind: TranslationTaskRecordFinalStateKind,
        report: &RpgMakerTranslationRunReport,
    ) {
        let task_index = task.index();
        let prior_failure = match kind {
            TranslationTaskRecordFinalStateKind::Cancelled => None,
            TranslationTaskRecordFinalStateKind::EarlierFailure => {
                Some(self.prior_failure_diagnostic())
            }
        };
        let observed_outcome = match kind {
            TranslationTaskRecordFinalStateKind::Cancelled => {
                RpgMakerTranslationLogTaskOutcome::Cancelled
            }
            TranslationTaskRecordFinalStateKind::EarlierFailure => {
                RpgMakerTranslationLogTaskOutcome::NotCommittedAfterEarlierFailure {
                    cause: prior_failure
                        .as_ref()
                        .expect("前序失败终态必须持有其原始诊断")
                        .clone(),
                }
            }
        };
        self.service
            .event_log
            .emit(RpgMakerTranslationLogEvent::TaskFinished {
                task_index,
                outcome: observed_outcome,
                attempts: Some(outcome.attempts()),
                provider: evidence.provider().map(str::to_owned),
                retry_exhausted: false,
                report: report.clone(),
            });
        let state = match kind {
            TranslationTaskRecordFinalStateKind::Cancelled => {
                TranslationTaskRecordFinalState::CancelledNoChanges {
                    outcome: Some(Arc::clone(&outcome)),
                }
            }
            TranslationTaskRecordFinalStateKind::EarlierFailure => {
                TranslationTaskRecordFinalState::NotCommittedAfterEarlierFailure {
                    outcome: Arc::clone(&outcome),
                    diagnostic: prior_failure.expect("前序失败终态必须持有其原始诊断"),
                }
            }
        };
        self.service
            .record_task(|| TranslationTaskRecordDocument::new(task, evidence, state));
    }

    fn record_commit_failure(
        &self,
        task: RpgMakerExecutableTask,
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        phase: TranslationTaskCommitPhase,
        failure: TranslationTaskCommitFailure<S::Error>,
        report: &RpgMakerTranslationRunReport,
    ) -> Result<(), TranslationTaskPipelineError<E::Error, S::Error>> {
        let task_index = task.index();
        let (source, impact, diagnostic) = failure.into_parts();
        self.remember_failure_diagnostic(&diagnostic);
        self.service
            .event_log
            .emit(RpgMakerTranslationLogEvent::TaskFinished {
                task_index,
                outcome: RpgMakerTranslationLogTaskOutcome::CommitFailed {
                    diagnostic: diagnostic.clone(),
                },
                attempts: Some(outcome.attempts()),
                provider: evidence.provider().map(str::to_owned),
                retry_exhausted: false,
                report: report.clone(),
            });
        let state = match impact {
            TranslationTaskCommitFailureImpact::NotApplied => {
                TranslationTaskRecordFinalState::CommitNotApplied {
                    outcome: Arc::clone(&outcome),
                    phase,
                    diagnostic: diagnostic.clone(),
                }
            }
            TranslationTaskCommitFailureImpact::OutcomeUnknown => {
                TranslationTaskRecordFinalState::CommitOutcomeUnknown {
                    outcome: Arc::clone(&outcome),
                    diagnostic: diagnostic.clone(),
                }
            }
        };
        self.service
            .record_task(|| TranslationTaskRecordDocument::new(task, evidence, state));
        Err(TranslationTaskPipelineError::CommitTask {
            task_index,
            source,
            diagnostic,
        })
    }

    fn record_success(
        &self,
        task: RpgMakerExecutableTask,
        outcome: Arc<TranslationTaskOutcome>,
        evidence: TranslationTaskExecutionEvidence,
        report: &mut RpgMakerTranslationRunReport,
    ) {
        let task_index = task.index();
        assert_ne!(
            evidence.attempt_count(),
            0,
            "已开始任务的正常结果必须携带真实模型 attempt"
        );
        report.record(&outcome);
        let retry_exhausted = matches!(
            outcome.as_ref(),
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. }
                    | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum { .. },
                ..
            }
        );
        // 任务记录与项目日志必须消费同一份在 Task/Unit 定位仍完整时建立的诊断，不能
        // 各自从 Outcome 的展示枚举重新推导原因。
        let diagnostics = task_outcome_diagnostics(&task, outcome.as_ref());
        let observed_outcome = match outcome.as_ref() {
            TranslationTaskOutcome::Complete { .. } => {
                RpgMakerTranslationLogTaskOutcome::Complete {
                    diagnostics: diagnostics.clone(),
                }
            }
            TranslationTaskOutcome::Partial { .. } => RpgMakerTranslationLogTaskOutcome::Partial {
                diagnostics: RpgMakerTaskDiagnosticReports::from_reports(diagnostics.clone()),
            },
            TranslationTaskOutcome::Unavailable { .. } => {
                RpgMakerTranslationLogTaskOutcome::Unavailable {
                    diagnostics: RpgMakerTaskDiagnosticReports::from_reports(diagnostics.clone()),
                }
            }
        };
        self.service
            .event_log
            .emit(RpgMakerTranslationLogEvent::TaskFinished {
                task_index,
                outcome: observed_outcome,
                attempts: Some(outcome.attempts()),
                provider: evidence.provider().map(str::to_owned),
                retry_exhausted,
                report: report.clone(),
            });
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
                if outcome.rejected_candidate_count() == 0 {
                    TranslationTaskRecordFinalState::UnavailableNoChanges {
                        outcome: Arc::clone(&outcome),
                    }
                } else {
                    TranslationTaskRecordFinalState::UnavailableRejectedCommitted {
                        outcome: Arc::clone(&outcome),
                    }
                }
            }
        };
        self.service.record_task(|| {
            TranslationTaskRecordDocument::new(task, evidence, state)
                .with_outcome_diagnostics(diagnostics)
        });
    }
}

fn translation_stage_error_kind<E, S>(error: &TranslationTaskStageError<E, S>) -> &'static str {
    match error {
        TranslationTaskStageError::Execution { .. } => "execution",
        TranslationTaskStageError::Cancelled { .. } => "cancelled",
        TranslationTaskStageError::InvalidResult { .. } => "invalid_result",
        TranslationTaskStageError::CommitPreparation { .. } => "commit_preparation",
    }
}

impl<R, P, E, S, J, K> RpgMakerTranslation for RpgMakerTranslationService<R, P, E, S, J, K>
where
    R: RpgMakerTranslationAssetReader,
    P: RpgMakerTranslationTaskPlanner,
    E: RpgMakerTranslationTaskExecutor<Profile = P::Profile>,
    S: RpgMakerTranslationResultStore,
    J: RpgMakerTranslationLog,
    K: TranslationTaskRecordSink,
{
    type Profile = P::Profile;
    type Error = RpgMakerTranslationServiceError<R::Error, P::Error, E::Error, S::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        input: RpgMakerTranslationInput,
    ) -> Result<OperationCompletion<RpgMakerTranslationRunReport>, Self::Error> {
        let operation: Result<_, Self::Error> = async {
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let corpus = self
                .asset_reader
                .read(project)
                .await
                .map_err(RpgMakerTranslationServiceError::ReadAssets)?;
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let plan = match self
                .task_planner
                .plan(project, profile, corpus, input)
                .await
            {
                Ok(plan) => plan,
                Err(source) if P::is_cancelled_error(&source) => {
                    return Ok(OperationCompletion::Cancelled);
                }
                Err(source) => {
                    return Err(RpgMakerTranslationServiceError::PlanTasks(source));
                }
            };
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let (_semantics, preparation, tasks) = plan.into_parts();
            let rejected_outside_tasks = preparation.rejected_outside_tasks();
            let existing_rejected = preparation.existing_rejected();
            let rejected_after_preparation = preparation.rejected_after_preparation();
            let resolved_rejected = preparation.resolved_rejected();
            let planned_decisions = tasks
                .iter()
                .map(|task| task.expected_outputs().len())
                .sum::<usize>()
                .checked_add(rejected_outside_tasks)
                .and_then(|value| value.checked_add(resolved_rejected))
                .expect("RPG Maker 计划决策数不得溢出");
            let planned_locations = tasks
                .iter()
                .flat_map(RpgMakerExecutableTask::expected_outputs)
                .map(|output| 1 + output.propagation_targets().len())
                .sum::<usize>()
                .checked_add(rejected_outside_tasks)
                .and_then(|value| value.checked_add(resolved_rejected))
                .expect("RPG Maker 计划位置数不得溢出");
            let mut report = RpgMakerTranslationRunReport::from_plan(
                tasks.len(),
                planned_decisions,
                planned_locations,
                existing_rejected,
                &preparation,
            );
            self.event_log
                .emit(RpgMakerTranslationLogEvent::PlanningCompleted {
                    report: report.clone(),
                });

            self.result_store
                .apply_preparation(project, preparation)
                .await
                .map_err(RpgMakerTranslationServiceError::ApplyPreparation)?;
            report.record_preparation_applied(rejected_after_preparation, resolved_rejected);
            self.event_log
                .emit(RpgMakerTranslationLogEvent::PreparationApplied {
                    report: report.clone(),
                });

            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }

            let task_count = tasks.len();
            let handler = RpgMakerOrderedExecutionHandler {
                service: self,
                project,
                profile,
                total_tasks: task_count,
                prior_failure_diagnostic: Mutex::new(None),
            };
            let limits = OrderedExecutionLimits::new(
                profile.max_concurrent_requests(),
                STANDARD_IN_FLIGHT_WINDOW_MULTIPLIER,
            );
            let completion = execute_ordered(tasks, limits, &self.cancellation, &handler, report)
                .await
                .map_err(|failure| match failure {
                    OrderedExecutionError::Finalization { source, .. } => match source {
                        TranslationTaskPipelineError::ExecuteTask {
                            task_index,
                            source,
                            diagnostic,
                            ..
                        } => RpgMakerTranslationServiceError::ExecuteTask {
                            task_index,
                            source,
                            diagnostic,
                        },
                        TranslationTaskPipelineError::CommitTask {
                            task_index,
                            source,
                            diagnostic,
                        } => RpgMakerTranslationServiceError::CommitTask {
                            task_index,
                            source,
                            diagnostic,
                        },
                        TranslationTaskPipelineError::InvalidTaskResultSequence {
                            expected_task_index,
                            actual_task_index,
                            diagnostic,
                        } => RpgMakerTranslationServiceError::InvalidTaskResultSequence {
                            expected_task_index,
                            actual_task_index,
                            diagnostic,
                        },
                    },
                    OrderedExecutionError::IncompleteResultSequence {
                        expected_ordinal,
                        actual_ordinal,
                    } => {
                        let expected_task_index =
                            RpgMakerTranslationTaskIndex::new(expected_ordinal);
                        let actual_task_index =
                            actual_ordinal.map(RpgMakerTranslationTaskIndex::new);
                        RpgMakerTranslationServiceError::InvalidTaskResultSequence {
                            expected_task_index,
                            actual_task_index,
                            diagnostic: task_result_sequence_report(
                                expected_task_index,
                                actual_task_index,
                            ),
                        }
                    }
                })?;
            Ok(completion)
        }
        .await;
        let storage_finalization = self.result_store.finalize().await;
        match (operation, storage_finalization) {
            (Ok(completion), Ok(())) => Ok(completion),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(source)) => {
                Err(RpgMakerTranslationServiceError::FinalizeResultStore(source))
            }
            (Err(primary), Err(finalization)) => {
                Err(RpgMakerTranslationServiceError::OperationAndFinalization {
                    primary: Box::new(primary),
                    finalization,
                })
            }
        }
    }
}

/// RPG Maker 在直接依赖边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum RpgMakerTranslationServiceError<R, P, E, S> {
    ReadAssets(R),
    PlanTasks(P),
    ApplyPreparation(S),
    ExecuteTask {
        task_index: RpgMakerTranslationTaskIndex,
        source: E,
        /// 任务仍持有 Planner Task/Unit 定位时建立的安全诊断。
        diagnostic: DiagnosticReport,
    },
    CommitTask {
        task_index: RpgMakerTranslationTaskIndex,
        source: S,
        diagnostic: DiagnosticReport,
    },
    InvalidTaskResultSequence {
        expected_task_index: RpgMakerTranslationTaskIndex,
        actual_task_index: Option<RpgMakerTranslationTaskIndex>,
        diagnostic: DiagnosticReport,
    },
    FinalizeResultStore(S),
    OperationAndFinalization {
        primary: Box<RpgMakerTranslationServiceError<R, P, E, S>>,
        finalization: S,
    },
}

impl<R, P, E, S> fmt::Display for RpgMakerTranslationServiceError<R, P, E, S>
where
    R: fmt::Display,
    P: fmt::Display,
    E: fmt::Display,
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadAssets(source) => {
                write!(formatter, "无法读取 RPG Maker 翻译资产：{source}")
            }
            Self::PlanTasks(source) => write!(formatter, "无法建立 RPG Maker 翻译计划：{source}"),
            Self::ApplyPreparation(source) => {
                write!(formatter, "无法应用 RPG Maker 翻译准备：{source}")
            }
            Self::ExecuteTask {
                task_index, source, ..
            } => {
                write!(
                    formatter,
                    "RPG Maker 翻译任务 {task_index} 执行失败：{source}"
                )
            }
            Self::CommitTask {
                task_index, source, ..
            } => {
                write!(
                    formatter,
                    "RPG Maker 翻译任务 {task_index} 提交失败：{source}"
                )
            }
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
                ..
            } => write!(
                formatter,
                "RPG Maker 翻译结果序列损坏：期待任务 {expected_task_index}，却收到任务 {actual_task_index}"
            ),
            Self::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: None,
                ..
            } => write!(
                formatter,
                "RPG Maker 翻译结果序列不完整：执行通道在任务 {expected_task_index} 返回前关闭"
            ),
            Self::FinalizeResultStore(source) => {
                write!(formatter, "RPG Maker 翻译数据库会话收尾失败：{source}")
            }
            Self::OperationAndFinalization {
                primary,
                finalization,
            } => write!(
                formatter,
                "{primary}；RPG Maker 翻译数据库会话收尾也失败：{finalization}"
            ),
        }
    }
}

impl<R, P, E, S> Error for RpgMakerTranslationServiceError<R, P, E, S>
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

    use tokio::sync::Semaphore;

    use super::*;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguageText,
    };
    use crate::llm::ChatMessageRole;
    use crate::project_name::ProjectName;
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
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

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    #[test]
    fn placeholder_order_and_wrapper_rejections_keep_distinct_public_diagnostics() {
        for (reason, expected_code, expected_reason) in [
            (
                TranslationUnitRejectionReason::OrderMismatch {
                    expected_token: "secret-expected-token".to_owned(),
                    actual_token: "secret-actual-token".to_owned(),
                },
                "rpg_maker.translation.response.unit.placeholder_order_mismatch",
                "控制 token 的顺序",
            ),
            (
                TranslationUnitRejectionReason::WrapperTopologyChanged {
                    token: "secret-wrapper-token".to_owned(),
                },
                "rpg_maker.translation.response.unit.placeholder_wrapper_topology_changed",
                "Placeholder 边界",
            ),
        ] {
            let unresolved = UnresolvedTranslationUnit::for_test(task_id(0), reason);
            let report =
                task_response_unit_report(RpgMakerTranslationTaskIndex::new(0), &unresolved)
                    .expect("具体候选拒绝必须形成 Unit 诊断");
            assert_eq!(report.primary().code(), expected_code);
            let rendered = crate::diagnostic::render_diagnostic_fields(
                &report,
                &crate::i18n::UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese),
            );
            assert!(rendered.reason.contains(expected_reason));
            let wire = serde_json::to_string(&report).expect("公开诊断必须可序列化");
            assert!(
                !wire.contains("secret-"),
                "公开诊断不得泄漏 Placeholder token"
            );
        }
    }

    #[test]
    fn committed_candidate_review_reports_the_applied_effect() {
        let unresolved = UnresolvedTranslationUnit::for_test(
            task_id(0),
            TranslationUnitRejectionReason::Missing,
        );
        let report = task_response_protocol_report(
            RpgMakerTranslationTaskIndex::new(0),
            &TranslationProtocolDiagnostic::CandidateReview {
                id: task_id(0),
                unit: unresolved.unit().clone(),
                finding: ReviewFinding::SourceResidual,
            },
            true,
        );

        assert_eq!(report.effect(), StateEffect::Applied);
        assert_eq!(
            report.primary().resolution(),
            crate::diagnostic::DiagnosticResolution::ReviewTranslation
        );
    }

    #[test]
    fn non_stop_finish_reports_whether_the_task_applied_output() {
        let diagnostic = TranslationProtocolDiagnostic::NonStopFinish {
            reason: RpgMakerModelNonStopFinishReason::Length,
            finding: ReviewFinding::NonStopFinish,
        };

        assert_eq!(
            task_response_protocol_report(
                RpgMakerTranslationTaskIndex::new(0),
                &diagnostic,
                false,
            )
            .effect(),
            StateEffect::ProgressPreserved
        );
        assert_eq!(
            task_response_protocol_report(RpgMakerTranslationTaskIndex::new(0), &diagnostic, true,)
                .effect(),
            StateEffect::Applied
        );
    }

    #[test]
    fn raw_response_errors_keep_the_rpg_maker_shape_and_syntax_categories() {
        for (raw, expected_code, expected_summary, expected_category) in [
            (
                r#"{"think":"判断"}"#,
                "rpg_maker.translation.response.invalid_shape",
                "response_shape_invalid",
                "shape",
            ),
            (
                "```json\n{\"0\":[\"first\"]}\n```\n```json\n{\"1\":[\"second\"]}\n```",
                "rpg_maker.translation.response.invalid_json",
                "response_json_invalid",
                "syntax",
            ),
        ] {
            let error = crate::translation_protocol::parse_translation_response(
                raw,
                crate::translation_protocol::TranslationResponseMode::new(true, false),
            )
            .expect_err("测试原始响应必须由共享解析器拒绝");
            let diagnostic = TranslationProtocolDiagnostic::InvalidResponse { error };
            let report = task_response_protocol_report(
                RpgMakerTranslationTaskIndex::new(0),
                &diagnostic,
                false,
            );

            assert_eq!(report.primary().code(), expected_code);
            assert_eq!(report.primary().issue().summary_code(), expected_summary);
            let wire = serde_json::to_string(&report).expect("RPG Maker 诊断必须可序列化");
            assert!(
                wire.contains(&format!(r#""category":"{expected_category}""#)),
                "诊断必须保留共享解析类别：{wire}"
            );
        }
    }

    fn test_retry_exhausted_report() -> crate::diagnostic::DiagnosticReport {
        crate::diagnostic::DiagnosticReport::new(
            crate::diagnostic::StateEffect::ProgressPreserved,
            crate::diagnostic::Diagnostic::http(crate::diagnostic::HttpIssue::Status {
                endpoint: crate::diagnostic::HttpEndpoint::new(
                    crate::diagnostic::HttpScheme::Https,
                    "example.test",
                    None,
                ),
                status: 503,
                retry_after_seconds: Some(2),
                provider_code: Some(
                    crate::diagnostic::SafeIdentifier::new("busy")
                        .expect("测试 provider code 合法"),
                ),
                provider_type: Some(
                    crate::diagnostic::SafeIdentifier::new("service_error")
                        .expect("测试 provider type 合法"),
                ),
                provider_message: None,
                response_read_failure: None,
            }),
        )
    }

    #[test]
    fn task_block_keeps_only_the_execution_contract() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(10)],
        );
        let description_identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
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
        let block = RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(4),
            test_language_pair(),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Translation contract"),
                ChatMessage::new(ChatMessageRole::User, "# Content\n\n..."),
            ],
            vec![ExpectedTranslationOutput::new(
                task_id(0),
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

        assert_eq!(block.index(), RpgMakerTranslationTaskIndex::new(4));
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
        assert_eq!(block.expected_outputs()[0].id(), task_id(0));
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
    fn expected_output_contract_scan_can_cancel_during_long_protected_text() {
        let mut polls = 0_usize;
        let result = ExpectedTranslationOutput::try_new_with_cancellation(
            task_id(0),
            translation_identity(),
            Vec::new(),
            ExpectedTranslationValidation::new(
                ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                "宝剑".repeat(100_000),
                Vec::new(),
                test_language_analysis(),
            ),
            test_state_context(1),
            Vec::new(),
            || {
                polls += 1;
                if polls >= 5 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert_eq!(result, Err("cancelled"));
        assert!(polls >= 5);
    }

    #[test]
    #[should_panic(expected = "RPG Maker 计划中的 TaskBlock 序号必须按自然计划顺序从零连续编号")]
    fn plan_establishes_the_contiguous_task_ordinal_invariant_before_execution() {
        RpgMakerTranslationPlan::new(
            Arc::new(super::super::semantics::ResolvedTranslationSemantics::for_test()),
            empty_preparation(),
            vec![RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(1),
                test_language_pair(),
                Vec::new(),
                Vec::new(),
            )],
        );
    }

    #[derive(Clone, Copy)]
    struct FakeProfile {
        max_concurrent_requests: NonZeroUsize,
    }

    impl RpgMakerTranslationExecutionProfile for FakeProfile {
        fn max_concurrent_requests(&self) -> NonZeroUsize {
            self.max_concurrent_requests
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        Read,
        Plan,
        Prepare,
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
        RequestAdmissionStopped,
        RetryExhausted,
        RateLimitExhausted,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RecordedTaskState {
        CompleteCommitted,
        PartialCommitted,
        UnavailableNoChanges,
        UnavailableRejectedCommitted,
        ExecutionFailedNoChanges,
        CommitPreparationFailed,
        CommitNotApplied,
        CommitOutcomeUnknown,
        NotCommittedAfterEarlierFailure,
        InvalidResultNoChanges,
        CancelledNoChanges,
    }

    type RecordedTaskProviders = Arc<Mutex<Vec<(usize, Option<String>)>>>;

    #[derive(Clone)]
    struct FakeTaskRecordSink {
        records: Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
        providers: RecordedTaskProviders,
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
            TranslationTaskRecordFinalState::UnavailableRejectedCommitted { .. } => {
                RecordedTaskState::UnavailableRejectedCommitted
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
            self.providers
                .lock()
                .expect("任务记录服务方锁不应中毒")
                .push((
                    document.task_index().get(),
                    document.provider().map(str::to_owned),
                ));
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

    impl RpgMakerTranslationAssetReader for FakeReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
        ) -> Result<RpgMakerTranslationCorpus, Self::Error> {
            record(&self.events, Event::Read);
            if self.failure {
                Err(FakeError("read"))
            } else {
                Ok(RpgMakerTranslationCorpus::new(Vec::new()))
            }
        }
    }

    #[derive(Clone)]
    struct FakePlanner {
        events: Arc<Mutex<Vec<Event>>>,
        inputs: Arc<Mutex<Vec<RpgMakerTranslationInput>>>,
        preparation: TranslationPlanPreparation,
        task_count: usize,
        failure: bool,
        cancel_on_plan: Option<CooperativeCancellation>,
    }

    impl RpgMakerTranslationTaskPlanner for FakePlanner {
        type Profile = FakeProfile;
        type Error = FakeError;

        fn is_cancelled_error(error: &Self::Error) -> bool {
            error.0 == "cancelled"
        }

        async fn plan(
            &self,
            _project: &OpenedProject,
            _profile: &Self::Profile,
            _corpus: RpgMakerTranslationCorpus,
            input: RpgMakerTranslationInput,
        ) -> Result<RpgMakerTranslationPlan, Self::Error> {
            record(&self.events, Event::Plan);
            self.inputs
                .lock()
                .expect("计划输入记录锁不应中毒")
                .push(input);
            if let Some(cancellation) = &self.cancel_on_plan {
                cancellation.request();
            }
            if self.failure {
                return Err(FakeError("plan"));
            }

            let tasks: Vec<RpgMakerExecutableTask> = (0..self.task_count)
                .map(|index| {
                    let expected_outputs = vec![
                        expected_output(index, 0, true),
                        expected_output(index, 1, false),
                    ];
                    RpgMakerExecutableTask::new(
                        RpgMakerTranslationTaskIndex::new(index),
                        test_language_pair(),
                        vec![ChatMessage::new(
                            ChatMessageRole::User,
                            format!("# Task {index}"),
                        )],
                        expected_outputs,
                    )
                })
                .collect();
            Ok(RpgMakerTranslationPlan::new(
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
        preserve_failure_at: Arc<Vec<usize>>,
        admission_stopped_at: Arc<Vec<usize>>,
        outcome_kinds: Arc<Vec<FakeOutcomeKind>>,
        cancel_on_start: Option<(usize, CooperativeCancellation)>,
        block_at: Option<(usize, Arc<Semaphore>)>,
        outcome_index_at: Option<(usize, usize)>,
    }

    impl RpgMakerTranslationTaskExecutor for FakeExecutor {
        type Profile = FakeProfile;
        type Error = FakeError;

        fn failure_preserves_admitted_results(error: &Self::Error) -> bool {
            error.0 == "external"
        }

        async fn execute<'a>(
            &'a self,
            _profile: &'a Self::Profile,
            task: &'a RpgMakerExecutableTask,
            on_task_started: Box<dyn FnOnce() + Send + 'a>,
        ) -> Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>>
        {
            let task_index = task.index();
            let index = task_index.get();
            record(&self.events, Event::Execute(index));
            if self.admission_stopped_at.contains(&index) {
                return Ok(TranslationTaskExecution::admission_stopped(
                    TranslationTaskExecutionEvidence::from_execution(0, None, None),
                ));
            }
            on_task_started();
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
                let evidence = TranslationTaskExecutionEvidence::synthetic(NonZeroUsize::MIN);
                if cancelled {
                    Err(TranslationTaskExecutionFailure::cancelled(
                        FakeError("execute"),
                        evidence,
                    ))
                } else {
                    let source = if self.preserve_failure_at.contains(&index) {
                        FakeError("external")
                    } else {
                        FakeError("execute")
                    };
                    Err(TranslationTaskExecutionFailure::failed(
                        source,
                        evidence,
                        test_retry_exhausted_report(),
                    ))
                }
            } else {
                let outcome_task_index = self
                    .outcome_index_at
                    .filter(|(source_index, _)| *source_index == index)
                    .map_or(task_index, |(_, outcome_index)| {
                        RpgMakerTranslationTaskIndex::new(outcome_index)
                    });
                let outcome_kind = self
                    .outcome_kinds
                    .get(index)
                    .copied()
                    .unwrap_or(FakeOutcomeKind::Complete);
                let outcome =
                    fake_outcome(outcome_task_index, task.expected_outputs(), outcome_kind);
                if outcome_kind == FakeOutcomeKind::RequestAdmissionStopped {
                    Ok(TranslationTaskExecution::new(
                        outcome,
                        TranslationTaskExecutionEvidence::from_execution(
                            1,
                            Some("First".to_owned()),
                            None,
                        ),
                    ))
                } else {
                    Ok(TranslationTaskExecution::synthetic(outcome))
                }
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

    impl RpgMakerTranslationResultStore for FakeStore {
        type PreparedCommit = RpgMakerTranslationTaskIndex;
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
                    test_retry_exhausted_report(),
                ))
            } else {
                record(&self.events, Event::PreparedCommit(index));
                Ok(task_index)
            }
        }

        async fn commit_prepared(
            &self,
            _project: &OpenedProject,
            task_index: RpgMakerTranslationTaskIndex,
        ) -> Result<(), TranslationTaskCommitFailure<Self::Error>> {
            let index = task_index.get();
            record(&self.events, Event::CommitAttempt(index));
            if self.unknown_commit_at == Some(index) {
                Err(TranslationTaskCommitFailure::new(
                    FakeError("commit-unknown"),
                    TranslationTaskCommitFailureImpact::OutcomeUnknown,
                    test_retry_exhausted_report(),
                ))
            } else if self.fail_commit_at == Some(index) {
                Err(TranslationTaskCommitFailure::not_applied(
                    FakeError("commit"),
                    test_retry_exhausted_report(),
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
        records: Arc<Mutex<Vec<RpgMakerTranslationLogEvent>>>,
        started_not_finalized: Arc<AtomicUsize>,
        max_started_not_finalized: Arc<AtomicUsize>,
    }

    impl RpgMakerTranslationLog for FakeEventLog {
        fn emit(&self, event: RpgMakerTranslationLogEvent) {
            match &event {
                RpgMakerTranslationLogEvent::TaskStarted { task_index, .. } => {
                    record(&self.events, Event::LogTaskStarted(task_index.get()));
                    let started = self.started_not_finalized.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_started_not_finalized
                        .fetch_max(started, Ordering::SeqCst);
                }
                RpgMakerTranslationLogEvent::TaskFinished {
                    task_index,
                    outcome,
                    ..
                } => {
                    self.started_not_finalized.fetch_sub(1, Ordering::SeqCst);
                    match outcome {
                        RpgMakerTranslationLogTaskOutcome::Complete { .. }
                        | RpgMakerTranslationLogTaskOutcome::Partial { .. }
                        | RpgMakerTranslationLogTaskOutcome::Unavailable { .. } => {
                            record(&self.events, Event::LogTask(task_index.get()));
                        }
                        RpgMakerTranslationLogTaskOutcome::CommitFailed { .. } => {
                            record(&self.events, Event::LogCommitFailure(task_index.get()));
                        }
                        RpgMakerTranslationLogTaskOutcome::Cancelled
                        | RpgMakerTranslationLogTaskOutcome::NotCommittedAfterEarlierFailure {
                            ..
                        } => {
                            record(&self.events, Event::LogNotCommitted(task_index.get()));
                        }
                        RpgMakerTranslationLogTaskOutcome::ExecutionFailed { .. }
                        | RpgMakerTranslationLogTaskOutcome::InvalidResult { .. } => {
                            record(&self.events, Event::LogExecutionFailure(task_index.get()));
                        }
                    }
                }
                RpgMakerTranslationLogEvent::PlanningCompleted { .. }
                | RpgMakerTranslationLogEvent::PreparationApplied { .. } => {}
            }
            self.records
                .lock()
                .expect("日志事件记录锁不应中毒")
                .push(event);
        }
    }

    type Service = RpgMakerTranslationService<
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
        planner_inputs: Arc<Mutex<Vec<RpgMakerTranslationInput>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
        log_records: Arc<Mutex<Vec<RpgMakerTranslationLogEvent>>>,
        max_active: Arc<AtomicUsize>,
        started_not_finalized: Arc<AtomicUsize>,
        max_started_not_finalized: Arc<AtomicUsize>,
        finalizations: Arc<AtomicUsize>,
        task_records: Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
        task_record_providers: RecordedTaskProviders,
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
        let task_record_providers = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CooperativeCancellation::default();
        Harness {
            service: RpgMakerTranslationService::new(
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
                    cancel_on_plan: None,
                },
                FakeExecutor {
                    events: Arc::clone(&events),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&max_active),
                    yields_by_task: Arc::new(yields_by_task),
                    fail_at: Arc::new(execute_failure_at.into_iter().collect()),
                    preserve_failure_at: Arc::new(Vec::new()),
                    admission_stopped_at: Arc::new(Vec::new()),
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
                providers: Arc::clone(&task_record_providers),
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
            task_record_providers,
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
                .expect("RPG Maker 翻译编排应该成功"),
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
    async fn cancellation_flag_does_not_overwrite_a_real_planner_failure() {
        let mut harness = harness(0, Vec::new(), false, true, false, None, None);
        harness.service.task_planner.cancel_on_plan = Some(harness.cancellation.clone());

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("共享取消标志不得覆盖 Planner 同时返回的真实错误");

        assert!(matches!(
            error,
            RpgMakerTranslationServiceError::PlanTasks(FakeError("plan"))
        ));
        assert_eq!(events(&harness.events), vec![Event::Read, Event::Plan]);
    }

    #[tokio::test]
    async fn planner_failure_stops_before_database_preparation_and_model_tasks() {
        let harness = harness(1, vec![1], false, true, false, None, None);

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("Planner 失败必须中止整次 Translate");

        assert!(matches!(
            error,
            RpgMakerTranslationServiceError::PlanTasks(FakeError("plan"))
        ));
        assert_eq!(events(&harness.events), vec![Event::Read, Event::Plan]);
        assert!(
            harness
                .preparations
                .lock()
                .expect("准备记录锁不应中毒")
                .is_empty()
        );
        assert!(
            harness
                .task_records
                .lock()
                .expect("任务记录锁不应中毒")
                .is_empty()
        );
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
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("prepare-commit"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(3)
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
    async fn earlier_commit_failure_stays_primary_but_later_preparation_failure_is_preserved() {
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
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(0)
        ));
        let final_events = events(&harness.events);
        assert!(final_events.contains(&Event::LogCommitFailure(0)));
        assert!(final_events.contains(&Event::LogCommitFailure(1)));
        assert!(!final_events.contains(&Event::LogNotCommitted(1)));
        assert!(
            harness
                .task_records
                .lock()
                .expect("任务记录锁不应中毒")
                .contains(&(1, RecordedTaskState::CommitPreparationFailed))
        );
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
            RpgMakerTranslationServiceError::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
                ..
            } if expected_task_index == RpgMakerTranslationTaskIndex::new(0)
                && actual_task_index == RpgMakerTranslationTaskIndex::new(9)
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
            RpgMakerTranslationServiceError::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index: Some(actual_task_index),
                ..
            } if expected_task_index == RpgMakerTranslationTaskIndex::new(3)
                && actual_task_index == RpgMakerTranslationTaskIndex::new(9)
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
        let RpgMakerTranslationServiceError::ExecuteTask {
            task_index,
            source: FakeError("execute"),
            diagnostic,
        } = error
        else {
            panic!("第二个任务执行失败应保留任务诊断")
        };
        assert_eq!(task_index, RpgMakerTranslationTaskIndex::new(1));
        assert_eq!(diagnostic.primary().code(), "http.status");
        assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
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
    async fn cancellation_does_not_hide_a_commit_preparation_failure() {
        let mut harness = harness(1, vec![1], false, false, false, None, None);
        harness.service.task_executor.cancel_on_start = Some((0, harness.cancellation.clone()));
        harness.service.result_store.fail_commit_preparation_at = Arc::new(vec![0]);

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("已经形成的提交准备失败不得被同时发生的取消覆盖");

        assert!(matches!(
            error,
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("prepare-commit"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(0)
        ));
        assert!(events(&harness.events).contains(&Event::LogCommitFailure(0)));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::CommitPreparationFailed)]
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_an_unavailable_task_as_unavailable() {
        let mut harness = harness_with_behavior(
            1,
            vec![1],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![FakeOutcomeKind::Unavailable],
        );
        harness.service.task_executor.cancel_on_start = Some((0, harness.cancellation.clone()));

        let completion = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect("Unavailable 任务本身没有待提交结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(logged_tasks(&events(&harness.events)), vec![0]);
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::UnavailableNoChanges)]
        );
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
            RpgMakerTranslationServiceError::ExecuteTask {
                task_index,
                source: FakeError("execute"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(1)
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
    async fn external_request_failure_commits_other_already_admitted_results() {
        let mut harness = harness(4, vec![4, 1, 1, 1], false, false, false, Some(1), None);
        harness.service.task_executor.preserve_failure_at = Arc::new(vec![1]);

        let error = harness
            .service
            .run(&project(), &profile(4), input())
            .await
            .expect_err("第二个外部请求失败仍必须成为 Translate 主错误");

        assert!(matches!(
            error,
            RpgMakerTranslationServiceError::ExecuteTask {
                task_index,
                source: FakeError("external"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(1)
        ));
        let events = events(&harness.events);
        assert_eq!(
            committed(&events),
            vec![0, 2, 3],
            "外部请求失败只能跳过自身，不能丢弃其他已验收的并发结果"
        );
        assert!(!events.contains(&Event::LogNotCommitted(2)));
        assert!(!events.contains(&Event::LogNotCommitted(3)));
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::CompleteCommitted),
                (1, RecordedTaskState::ExecutionFailedNoChanges),
                (2, RecordedTaskState::CompleteCommitted),
                (3, RecordedTaskState::CompleteCommitted),
            ]
        );
    }

    #[tokio::test]
    async fn earlier_failure_keeps_a_later_unavailable_task_as_unavailable() {
        let harness = harness_with_behavior(
            3,
            vec![1; 3],
            false,
            false,
            false,
            Some(0),
            None,
            empty_preparation(),
            vec![
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Unavailable,
                FakeOutcomeKind::Complete,
            ],
        );

        let error = harness
            .service
            .run(&project(), &profile(3), input())
            .await
            .expect_err("首项执行失败必须成为 Translate 主错误");
        assert!(matches!(
            error,
            RpgMakerTranslationServiceError::ExecuteTask {
                task_index,
                source: FakeError("execute"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(0)
        ));
        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![
                (0, RecordedTaskState::ExecutionFailedNoChanges),
                (1, RecordedTaskState::UnavailableNoChanges),
                (2, RecordedTaskState::NotCommittedAfterEarlierFailure),
            ]
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
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(1)
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
            RpgMakerTranslationLogEvent::TaskFinished {
                task_index,
                outcome: RpgMakerTranslationLogTaskOutcome::CommitFailed { .. },
                attempts: Some(_),
                retry_exhausted: false,
                ..
            } if *task_index == RpgMakerTranslationTaskIndex::new(1)
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
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit-unknown"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(0)
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
            RpgMakerTranslationServiceError::CommitTask {
                task_index,
                source: FakeError("commit"),
                ..
            } if task_index == RpgMakerTranslationTaskIndex::new(0)
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
        let input = RpgMakerTranslationInput::new(None, None);

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
                RpgMakerTranslationLogEvent::TaskFinished {
                    outcome, attempts, ..
                } => Some((
                    match outcome {
                        RpgMakerTranslationLogTaskOutcome::Complete { .. } => "complete",
                        RpgMakerTranslationLogTaskOutcome::Partial { .. } => "partial",
                        RpgMakerTranslationLogTaskOutcome::Unavailable { .. } => "unavailable",
                        _ => "unexpected",
                    },
                    *attempts,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed,
            vec![
                ("partial", NonZeroUsize::new(1)),
                ("unavailable", NonZeroUsize::new(1)),
                ("complete", NonZeroUsize::new(1)),
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
                RpgMakerTranslationLogEvent::TaskFinished {
                    outcome: RpgMakerTranslationLogTaskOutcome::Unavailable { diagnostics },
                    attempts: Some(attempts),
                    retry_exhausted: true,
                    ..
                } if attempts.get() == 3 => diagnostics.reports().next(),
                _ => None,
            })
            .expect("任务观察必须携带重试耗尽的安全诊断");
        assert_eq!(diagnostic.primary().code(), "http.status");
        let value = serde_json::to_value(diagnostic).expect("任务诊断应可序列化");
        assert_eq!(value["primary"]["issue"]["details"]["status"], 503);
        assert_eq!(
            value["primary"]["issue"]["details"]["retry_after_seconds"],
            2
        );
    }

    #[tokio::test]
    async fn admission_stop_after_attempt_records_one_unavailable_task_with_provider() {
        let harness = harness_with_behavior(
            1,
            vec![1],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![FakeOutcomeKind::RequestAdmissionStopped],
        );

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(1), input())
                .await
                .expect("重试准入停止应保留已开始任务的不可用终态"),
        );
        assert_eq!(report.started_tasks(), 1);
        assert_eq!(report.unavailable_tasks(), 1);
        assert_eq!(report.recoverable_request_exhaustions(), 0);
        assert!(report.request_admission_stopped());

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        let finished = records
            .iter()
            .find_map(|event| match event {
                RpgMakerTranslationLogEvent::TaskFinished {
                    outcome: RpgMakerTranslationLogTaskOutcome::Unavailable { diagnostics },
                    attempts: Some(attempts),
                    provider,
                    retry_exhausted: false,
                    ..
                } if attempts.get() == 1 && provider.as_deref() == Some("First") => {
                    Some(diagnostics)
                }
                _ => None,
            })
            .expect("已开始任务必须记录不可用终态和最后一次 attempt 的服务方");
        assert_eq!(
            finished.reports().next().unwrap().primary().code(),
            "http.status"
        );
        drop(records);

        assert_eq!(
            *harness.task_records.lock().expect("任务记录锁不应中毒"),
            vec![(0, RecordedTaskState::UnavailableNoChanges)]
        );
        assert_eq!(
            *harness
                .task_record_providers
                .lock()
                .expect("任务记录服务方锁不应中毒"),
            vec![(0, Some("First".to_owned()))]
        );
        assert_all_started_tasks_observed(&harness.log_records, &harness.task_records);
    }

    #[tokio::test]
    async fn rate_limit_exhaustion_stops_admission_before_slow_earlier_task_finishes() {
        let mut harness = harness_with_behavior(
            8,
            vec![1; 8],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::RateLimitExhausted,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
                FakeOutcomeKind::Complete,
            ],
        );
        let earlier_gate = Arc::new(Semaphore::new(0));
        harness.service.task_executor.block_at = Some((0, Arc::clone(&earlier_gate)));

        let project = project();
        let profile = profile(2);
        let run = harness.service.run(&project, &profile, input());
        let release = async {
            loop {
                if harness
                    .events
                    .lock()
                    .expect("测试事件记录锁不应中毒")
                    .contains(&Event::Complete(1))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            earlier_gate.add_permits(1);
        };
        let (completion, ()) = tokio::join!(run, release);
        let report = expect_completed(completion.expect("普通 429 耗尽应形成未完整结果"));

        let executed = events(&harness.events)
            .into_iter()
            .filter_map(|event| match event {
                Event::Execute(index) => Some(index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(executed, vec![0, 1]);
        assert_eq!(report.total_tasks(), 8);
        assert_eq!(report.started_tasks(), 2);
        assert_eq!(report.not_started_tasks(), 6);
        assert_eq!(report.complete_tasks(), 1);
        assert_eq!(report.unavailable_tasks(), 1);
        assert_eq!(report.recoverable_request_exhaustions(), 1);
        assert!(report.request_admission_stopped());
        assert_eq!(report.unresolved_decisions(), 14);
        assert_eq!(report.unresolved_locations(), 21);
    }

    #[tokio::test]
    async fn local_admission_stop_remains_a_zero_attempt_unstarted_task() {
        let mut harness = harness(4, vec![1; 4], false, false, false, None, None);
        harness.service.task_executor.admission_stopped_at = Arc::new(vec![0]);

        let report = expect_completed(
            harness
                .service
                .run(&project(), &profile(1), input())
                .await
                .expect("本地准入停止应保留项目进度并形成未完整结果"),
        );

        assert_eq!(
            events(&harness.events)
                .into_iter()
                .filter(|event| matches!(event, Event::Execute(_)))
                .collect::<Vec<_>>(),
            [Event::Execute(0)]
        );
        assert_eq!(report.total_tasks(), 4);
        assert_eq!(report.started_tasks(), 0);
        assert_eq!(report.not_started_tasks(), 4);
        assert_eq!(report.unavailable_tasks(), 0);
        assert!(report.request_admission_stopped());
        assert!(
            harness
                .log_records
                .lock()
                .expect("日志事件记录锁不应中毒")
                .iter()
                .all(|event| !matches!(
                    event,
                    RpgMakerTranslationLogEvent::TaskStarted { .. }
                        | RpgMakerTranslationLogEvent::TaskFinished { .. }
                )),
            "未准入任务不得产生 started 或 finished 事件"
        );
        assert!(
            harness
                .task_records
                .lock()
                .expect("任务记录锁不应中毒")
                .is_empty(),
            "未准入任务不得建立模型任务记录"
        );
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
            RpgMakerTranslationServiceError::FinalizeResultStore(FakeError("finalize"))
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

        let RpgMakerTranslationServiceError::OperationAndFinalization {
            primary,
            finalization,
        } = &error
        else {
            panic!("必须同时保留两个失败")
        };
        assert!(matches!(
            primary.as_ref(),
            RpgMakerTranslationServiceError::ReadAssets(FakeError("read"))
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

    #[test]
    fn report_tracks_rejected_locations_across_first_rejection_repeat_and_repair() {
        let rejected_outcome = |was_current_rejected| {
            let identity = translation_identity();
            let target = RejectedTranslationTarget::with_rejected_state(
                identity.clone(),
                test_state_context(9).applicability(),
                None,
                was_current_rejected,
            );
            TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    RpgMakerTranslationTaskIndex::new(0),
                    NonZeroUsize::MIN,
                    Vec::new(),
                ),
                reason: TranslationTaskUnavailableReason::AllOutputsRejected,
                unresolved: test_non_empty(vec![
                    UnresolvedTranslationUnit::with_rejected_candidate(
                        task_id(0),
                        rpg_maker_diagnostic_unit(&identity),
                        TranslationUnitRejectionReason::InvalidShape {
                            problem: TranslationAssistantValueError::NotStringArray,
                        },
                        RejectedTranslationCandidate::new(
                            "true".to_owned(),
                            None,
                            ProvenInvariantViolation::InvalidCandidateShape,
                            vec![target],
                        ),
                    ),
                ]),
            }
        };

        let mut first = RpgMakerTranslationRunReport::with_reconciliation(1, 1, 1, 0, 0, 0, 0);
        first.record(&rejected_outcome(false));
        assert_eq!(first.rejected_locations(), 1);
        assert_eq!(first.unresolved_locations(), 1);

        let mut repeated = RpgMakerTranslationRunReport::with_reconciliation(1, 1, 1, 0, 0, 0, 0)
            .with_initial_rejected_for_test(1);
        repeated.record(&rejected_outcome(true));
        assert_eq!(repeated.rejected_locations(), 1);
        assert_eq!(repeated.unresolved_locations(), 1);

        let identity = translation_identity();
        let translation = TextUnitContent::Value("译文".to_owned());
        let repaired = TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            accepted: test_non_empty(vec![AcceptedTranslationDecision::new(
                task_id(0),
                TranslationPatch::with_previous_and_rejected_state(
                    identity,
                    Vec::new(),
                    translation,
                    test_state_context(10).applicability(),
                    None,
                    true,
                ),
            )]),
        };
        let mut repaired_report =
            RpgMakerTranslationRunReport::with_reconciliation(1, 1, 1, 0, 0, 0, 0)
                .with_initial_rejected_for_test(1);
        repaired_report.record(&repaired);
        assert_eq!(repaired_report.rejected_locations(), 0);
        assert_eq!(repaired_report.unresolved_locations(), 0);
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

    fn input() -> RpgMakerTranslationInput {
        RpgMakerTranslationInput::new(
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
            task_id(id),
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
        )
        .analyze_source(&LanguageText::natural("宝剑"))
    }

    fn fake_outcome(
        task_index: RpgMakerTranslationTaskIndex,
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
                .zip(output.propagation_was_current_rejected().iter().copied())
                .map(|((identity, state), was_current_rejected)| {
                    TranslationPropagationTarget::with_previous_and_rejected_state(
                        identity,
                        state,
                        None,
                        None,
                        was_current_rejected,
                    )
                })
                .collect();
            AcceptedTranslationDecision::new(
                output.id(),
                TranslationPatch::with_previous_and_rejected_state(
                    output.identity().clone(),
                    propagation_targets,
                    translation.clone(),
                    output.state_context().applicability(),
                    None,
                    output.was_current_rejected(),
                ),
            )
        };
        let unresolved = |output: &ExpectedTranslationOutput, reason| {
            UnresolvedTranslationUnit::for_test(output.id(), reason)
        };

        match kind {
            FakeOutcomeKind::Complete => TranslationTaskOutcome::Complete {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    Vec::new(),
                ),
                accepted: test_non_empty(expected.iter().map(patch).collect()),
            },
            FakeOutcomeKind::Partial => TranslationTaskOutcome::Partial {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    vec![TranslationProtocolDiagnostic::UnknownId {
                        item_index: 3,
                        id: task_id(99),
                    }],
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
                        error: TranslationTaskResponseParseError::new(
                            TranslationTaskResponseParseErrorKind::Json(
                                TranslationTaskResponseJsonErrorCategory::Syntax,
                            ),
                            NonZeroUsize::MIN,
                            NonZeroUsize::MIN,
                        ),
                    }],
                ),
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                unresolved: test_non_empty(
                    expected
                        .iter()
                        .map(|output| {
                            unresolved(output, TranslationUnitRejectionReason::InvalidResponse)
                        })
                        .collect(),
                ),
            },
            FakeOutcomeKind::RequestAdmissionStopped => TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::MIN,
                    Vec::new(),
                ),
                reason: TranslationTaskUnavailableReason::RequestAdmissionStopped {
                    diagnostic: test_retry_exhausted_report(),
                },
                unresolved: test_non_empty(
                    expected
                        .iter()
                        .map(|output| unresolved(output, TranslationUnitRejectionReason::Missing))
                        .collect(),
                ),
            },
            FakeOutcomeKind::RetryExhausted => TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::new(3).expect("测试尝试数必须非零"),
                    Vec::new(),
                ),
                reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                    diagnostic: test_retry_exhausted_report(),
                    service_status: LlmServiceStatus::Other,
                },
                unresolved: test_non_empty(
                    expected
                        .iter()
                        .map(|output| unresolved(output, TranslationUnitRejectionReason::Missing))
                        .collect(),
                ),
            },
            FakeOutcomeKind::RateLimitExhausted => TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    task_index,
                    NonZeroUsize::new(3).expect("测试尝试数必须非零"),
                    Vec::new(),
                ),
                reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                    diagnostic: test_retry_exhausted_report(),
                    service_status: LlmServiceStatus::RateLimited,
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
            RpgMakerAssetOwner::Builtin,
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
        records: &Arc<Mutex<Vec<RpgMakerTranslationLogEvent>>>,
        task_records: &Arc<Mutex<Vec<(usize, RecordedTaskState)>>>,
    ) {
        let records = records.lock().expect("观察记录锁不应中毒");
        let mut started = records
            .iter()
            .filter_map(|event| match event {
                RpgMakerTranslationLogEvent::TaskStarted { task_index, .. } => Some(*task_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut finished = records
            .iter()
            .filter_map(|event| match event {
                RpgMakerTranslationLogEvent::TaskFinished { task_index, .. } => Some(*task_index),
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
            .map(|(task_index, _)| RpgMakerTranslationTaskIndex::new(*task_index))
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
