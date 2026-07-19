//! RPG Maker 标准资产翻译的顶层编排。
//!
//! Standard 只负责读取当前资产、建立任务计划、在外部上限内执行任务，
//! 并按计划顺序逐项提交。任务可以并发完成，但后续任务绝不能越过前序任务
//! 写入数据库，因此失败时始终只保留一个确定的成功前缀。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{FuturesOrdered, StreamExt};

use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguagePair};
use crate::llm::{ChatMessage, LlmUsage};
use crate::rpg_maker::audit::{AuditEvent, AuditLedger, TranslationTaskAuditResult};
use crate::rpg_maker::model::{LogicalTextLocation, TextFieldRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};

use super::executor::FinalLlmResponseMetadata;
use super::profile::RpgMakerTranslationProfile;

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
/// Profile 由外部配置边界一次建立。Standard 不提供默认并发数，也不根据
/// CPU 数量或任务数量自行缩放。
pub(crate) trait StandardTranslationProfile: Send + Sync + 'static {
    fn max_in_flight_tasks(&self) -> NonZeroUsize;
}

impl<L> StandardTranslationProfile for RpgMakerTranslationProfile<L>
where
    L: Send + Sync + 'static,
{
    fn max_in_flight_tasks(&self) -> NonZeroUsize {
        self.max_in_flight_tasks()
    }
}

impl<L> StandardTranslationProfile for Arc<RpgMakerTranslationProfile<L>>
where
    L: Send + Sync + 'static,
{
    fn max_in_flight_tasks(&self) -> NonZeroUsize {
        RpgMakerTranslationProfile::max_in_flight_tasks(self.as_ref())
    }
}

/// 一个叶子的持久化身份与读取时的原文事实。
///
/// Store 在写入时可以用原文事实防止把旧计划提交到已变化的资产上。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TranslationLeafIdentity {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    logical_location: LogicalTextLocation,
    original_text: String,
    translation_context_json: String,
}

impl TranslationLeafIdentity {
    pub(crate) fn new(
        owner: RpgMakerStandardAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        role: TextFieldRole,
        original_text: impl Into<String>,
        translation_context_json: impl Into<String>,
    ) -> Self {
        Self {
            owner,
            kind,
            logical_location: LogicalTextLocation::new(group_location, role),
            original_text: original_text.into(),
            translation_context_json: translation_context_json.into(),
        }
    }

    pub(crate) const fn owner(&self) -> RpgMakerStandardAssetOwner {
        self.owner
    }

    /// 返回逻辑叶所属的领域组种类。
    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn role(&self) -> &TextFieldRole {
        self.logical_location.role()
    }

    pub(crate) fn role_label(&self) -> String {
        match self.role() {
            TextFieldRole::Scalar(key) => key.as_str().to_owned(),
            TextFieldRole::DialogueSpeaker => "speaker".to_owned(),
            TextFieldRole::DialogueBody { index } => format!("body[{index}]"),
            TextFieldRole::ScrollingTextBody { index } => {
                format!("scrolling_body[{index}]")
            }
        }
    }

    /// 返回译文所属复合语义组的结构化位置。
    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        self.logical_location.group_location()
    }

    pub(crate) fn logical_location(&self) -> &LogicalTextLocation {
        &self.logical_location
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }

    pub(crate) fn translation_context_json(&self) -> &str {
        &self.translation_context_json
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

/// 从标准资产表读出的一个叶子。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationAsset {
    identity: TranslationLeafIdentity,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
}

impl StandardTranslationAsset {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        translation: Option<String>,
        translation_state: Option<Sha256Fingerprint>,
    ) -> Self {
        Self {
            identity,
            translation,
            translation_state,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationLeafIdentity,
        Option<String>,
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
    identity: TranslationLeafIdentity,
    expected_translation: String,
    expected_translation_state: Sha256Fingerprint,
}

/// 可以直接复用的一条现有译文快照。
///
/// Store 必须在写入目标前确认种子仍保持读取时的译文和语义状态，避免把
/// 已被并发修改的旧事实扩散到其他位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseSeed {
    identity: TranslationLeafIdentity,
    expected_translation: String,
    expected_translation_state: Sha256Fingerprint,
}

impl TranslationReuseSeed {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        expected_translation: impl Into<String>,
        expected_translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            expected_translation: expected_translation.into(),
            expected_translation_state,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> &str {
        &self.expected_translation
    }

    pub(crate) const fn expected_translation_state(&self) -> Sha256Fingerprint {
        self.expected_translation_state
    }
}

/// 一个将被现有译文覆盖的目标及其读取时状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseTarget {
    identity: TranslationLeafIdentity,
    expected_translation: Option<String>,
    expected_translation_state: Option<Sha256Fingerprint>,
    replacement_translation_state: Sha256Fingerprint,
}

impl TranslationReuseTarget {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        expected_translation: Option<String>,
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

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> Option<&str> {
        self.expected_translation.as_deref()
    }

    pub(crate) const fn expected_translation_state(&self) -> Option<Sha256Fingerprint> {
        self.expected_translation_state
    }

    pub(crate) const fn replacement_translation_state(&self) -> Sha256Fingerprint {
        self.replacement_translation_state
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

    pub(crate) fn seed(&self) -> &TranslationReuseSeed {
        &self.seed
    }

    pub(crate) fn targets(&self) -> &[TranslationReuseTarget] {
        &self.targets
    }
}

impl TranslationInvalidation {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        expected_translation: impl Into<String>,
        expected_translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            expected_translation: expected_translation.into(),
            expected_translation_state,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> &str {
        &self.expected_translation
    }

    pub(crate) const fn expected_translation_state(&self) -> Sha256Fingerprint {
        self.expected_translation_state
    }
}

/// 标准翻译计划准备阶段的逐叶对账计数。
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

    pub(crate) fn reused(&self) -> usize {
        self.reuses.iter().map(|reuse| reuse.targets().len()).sum()
    }

    pub(crate) fn requires_storage_changes(&self) -> bool {
        !self.invalidations.is_empty()
            || !self.reuses.is_empty()
            || self.terminology_json != self.snapshot_baseline.terminology_json()
            || self.placeholder_rules_json != self.snapshot_baseline.placeholder_rules_json()
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

/// Planner 为某个活跃叶子建立的一条占位符反查事实。
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

/// 一个叶子除最终译文以外的全部当前翻译语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationStateContext(Sha256Fingerprint);

impl TranslationStateContext {
    pub(crate) const fn new(fingerprint: Sha256Fingerprint) -> Self {
        Self(fingerprint)
    }

    pub(crate) fn finish(self, translation: &str) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-state");
        hasher
            .frame(1, self.0.as_bytes())
            .frame(2, translation.as_bytes());
        hasher.finish()
    }
}

/// 去重传播目标以及该逻辑叶子的独立语义上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPropagationTarget {
    identity: TranslationLeafIdentity,
    state_context: TranslationStateContext,
}

impl TranslationPropagationTarget {
    pub(crate) const fn new(
        identity: TranslationLeafIdentity,
        state_context: TranslationStateContext,
    ) -> Self {
        Self {
            identity,
            state_context,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
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
        leader: Box<TranslationLeafIdentity>,
    },
    Reused {
        seed: Box<TranslationLeafIdentity>,
    },
}

/// TaskBlock 单元是需要模型返回结果的活跃原文，或只提供上下文的虚原文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskUnitMode {
    Active { id: usize },
    Virtual { reason: TranslationVirtualReason },
}

/// TaskBlock 中按 RPG Maker 语义顺序排列的一个原文单元。
///
/// 虚原文只保留原文上下文且不要求模型返回结果；活跃原文持有从 0 开始连续的 ID。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskUnit {
    field_name: String,
    identity: TranslationLeafIdentity,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    mode: TranslationTaskUnitMode,
}

impl TranslationTaskUnit {
    pub(crate) fn new(
        field_name: impl Into<String>,
        identity: TranslationLeafIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        mode: TranslationTaskUnitMode,
    ) -> Self {
        Self {
            field_name: field_name.into(),
            identity,
            protected_text: protected_text.into(),
            applied_placeholders,
            mode,
        }
    }

    pub(crate) fn active(
        field_name: impl Into<String>,
        identity: TranslationLeafIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        id: usize,
    ) -> Self {
        Self::new(
            field_name,
            identity,
            protected_text,
            applied_placeholders,
            TranslationTaskUnitMode::Active { id },
        )
    }

    pub(crate) fn virtual_context(
        field_name: impl Into<String>,
        identity: TranslationLeafIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        reason: TranslationVirtualReason,
    ) -> Self {
        Self::new(
            field_name,
            identity,
            protected_text,
            applied_placeholders,
            TranslationTaskUnitMode::Virtual { reason },
        )
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    #[cfg(test)]
    pub(crate) fn protected_text(&self) -> &str {
        &self.protected_text
    }

    #[cfg(test)]
    pub(crate) const fn mode(&self) -> &TranslationTaskUnitMode {
        &self.mode
    }
}

/// 一个任务块中的不可拆复合组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<TranslationTaskUnit>,
}

impl TranslationTaskGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<TranslationTaskUnit>,
    ) -> Self {
        Self {
            kind,
            group_location,
            units,
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
    pub(crate) fn units(&self) -> &[TranslationTaskUnit] {
        &self.units
    }
}

/// 需要 Executor 返回的一个活跃翻译单元。
///
/// 虚原文没有 ID，因此不会出现在该集合中。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedTranslationOutput {
    id: usize,
    identity: TranslationLeafIdentity,
    propagation_targets: Vec<TranslationLeafIdentity>,
    applied_placeholders: Vec<AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    state_context: TranslationStateContext,
    propagation_state_contexts: Vec<TranslationStateContext>,
}

impl ExpectedTranslationOutput {
    pub(crate) fn new(
        id: usize,
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationLeafIdentity>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        language_analysis: LanguageAnalysis,
        state_context: TranslationStateContext,
        propagation_state_contexts: Vec<TranslationStateContext>,
    ) -> Self {
        Self {
            id,
            identity,
            propagation_targets,
            applied_placeholders,
            language_analysis,
            state_context,
            propagation_state_contexts,
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationLeafIdentity] {
        &self.propagation_targets
    }

    pub(crate) fn applied_placeholders(&self) -> &[AppliedPlaceholder] {
        &self.applied_placeholders
    }

    /// 返回 Planner 针对代表原文建立、供译后处理使用的唯一语言事实。
    pub(crate) fn language_analysis(&self) -> &LanguageAnalysis {
        &self.language_analysis
    }

    pub(crate) const fn state_context(&self) -> TranslationStateContext {
        self.state_context
    }

    pub(crate) fn propagation_state_contexts(&self) -> &[TranslationStateContext] {
        &self.propagation_state_contexts
    }
}

/// 一个已完成语义切块、虚原文组装、术语注入和占位符保护的任务块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskBlock {
    index: StandardTranslationTaskIndex,
    language_pair: LanguagePair,
    groups: Vec<TranslationTaskGroup>,
    injected_terminology: Vec<TerminologyDependency>,
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl TranslationTaskBlock {
    pub(crate) fn new(
        index: StandardTranslationTaskIndex,
        language_pair: LanguagePair,
        groups: Vec<TranslationTaskGroup>,
        injected_terminology: Vec<TerminologyDependency>,
        messages: Vec<ChatMessage>,
        expected_outputs: Vec<ExpectedTranslationOutput>,
    ) -> Self {
        Self {
            index,
            language_pair,
            groups,
            injected_terminology,
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

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[TranslationTaskGroup] {
        &self.groups
    }

    #[cfg(test)]
    pub(crate) fn injected_terminology(&self) -> &[TerminologyDependency] {
        &self.injected_terminology
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

/// 经过 Executor 完整验收并可直接写入的一个叶子译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPatch {
    identity: TranslationLeafIdentity,
    propagation_targets: Vec<TranslationPropagationTarget>,
    translation: String,
    translation_state: Sha256Fingerprint,
}

impl TranslationPatch {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationPropagationTarget>,
        translation: impl Into<String>,
        translation_state: Sha256Fingerprint,
    ) -> Self {
        Self {
            identity,
            propagation_targets,
            translation: translation.into(),
            translation_state,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationPropagationTarget] {
        &self.propagation_targets
    }

    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }

    pub(crate) const fn translation_state(&self) -> Sha256Fingerprint {
        self.translation_state
    }
}

/// 一个已经通过独立验收的任务 ID 及其可写 Patch。
///
/// ID 只属于本次 TaskBlock 协议，不进入资产表；保留在业务结果中是为了让持久日志
/// 能准确表达“哪个 ID 成功，以及它会传播到哪些位置”。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedTranslationDecision {
    id: usize,
    patch: TranslationPatch,
}

impl AcceptedTranslationDecision {
    pub(crate) fn new(id: usize, patch: TranslationPatch) -> Self {
        Self { id, patch }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        self.patch.identity()
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationPropagationTarget] {
        self.patch.propagation_targets()
    }

    #[cfg(test)]
    pub(crate) fn translation(&self) -> &str {
        self.patch.translation()
    }

    fn into_patch(self) -> TranslationPatch {
        self.patch
    }
}

/// 只承载已经独立验收、可交给 Store 原子写入的译文 Patch。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedTranslationTaskResult {
    task_index: StandardTranslationTaskIndex,
    updates: Vec<TranslationPatch>,
}

impl ValidatedTranslationTaskResult {
    pub(crate) fn new(
        task_index: StandardTranslationTaskIndex,
        updates: Vec<TranslationPatch>,
    ) -> Self {
        Self {
            task_index,
            updates,
        }
    }

    #[cfg(test)]
    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task_index
    }

    pub(crate) fn into_updates(self) -> Vec<TranslationPatch> {
        self.updates
    }
}

/// 一个预期 ID 没有形成可写译文的正常业务原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationUnitRejectionReason {
    Missing,
    Duplicate,
    InvalidShape { message: String },
    BlankTranslation,
    InvalidSpeakerText,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch { token: String },
    UnexpectedPlaceholderToken { token: String },
    PlaceholderNormalizationAmbiguous { original: String },
    SourceResidual { fragment: String },
}

/// 一个仍需在后续 CLI 运行中重新翻译的预期单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnresolvedTranslationUnit {
    id: usize,
    identity: TranslationLeafIdentity,
    propagation_targets: Vec<TranslationLeafIdentity>,
    reason: TranslationUnitRejectionReason,
}

impl UnresolvedTranslationUnit {
    pub(crate) fn new(
        id: usize,
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationLeafIdentity>,
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

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationLeafIdentity] {
        &self.propagation_targets
    }

    pub(crate) fn reason(&self) -> &TranslationUnitRejectionReason {
        &self.reason
    }

    pub(crate) const fn location_count(&self) -> usize {
        1 + self.propagation_targets.len()
    }
}

/// 无法绑定为某个可写译文、但必须进入持久日志的模型协议事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationProtocolDiagnostic {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    UnknownId { item_index: usize, id: usize },
}

/// 一个任务没有任何可用译文的正常原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskUnavailableReason {
    ModelResponseUnusable,
    AllOutputsRejected,
    RecoverableRequestExhausted {
        message: String,
    },
    RetryAfterExceedsConfiguredMaximum {
        retry_after: Duration,
        maximum: Duration,
        message: String,
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

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.items
    }

    fn map_ref<U>(&self, mut map: impl FnMut(&T) -> U) -> NonEmptyTaskItems<U> {
        let first = map(&self.items[0]);
        let rest = self.items[1..].iter().map(map).collect();
        NonEmptyTaskItems::new(first, rest)
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

    fn final_response(&self) -> Option<&FinalLlmResponseMetadata> {
        match self {
            Self::Complete { final_response, .. } | Self::Partial { final_response, .. } => {
                Some(final_response)
            }
            Self::Unavailable { final_response, .. } => final_response.as_ref(),
        }
    }

    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.final_response()
            .and_then(FinalLlmResponseMetadata::provider_request_id)
    }

    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.final_response()
            .and_then(FinalLlmResponseMetadata::provider_response_id)
    }

    pub(crate) fn finish_reason(&self) -> Option<&str> {
        self.final_response()
            .map(FinalLlmResponseMetadata::finish_reason)
    }

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

    pub(crate) fn validated_result(&self) -> Option<ValidatedTranslationTaskResult> {
        (!self.accepted().is_empty()).then(|| {
            ValidatedTranslationTaskResult::new(
                self.task_index(),
                self.accepted()
                    .iter()
                    .cloned()
                    .map(AcceptedTranslationDecision::into_patch)
                    .collect(),
            )
        })
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskLogContext {
    task_index: StandardTranslationTaskIndex,
    attempts: NonZeroUsize,
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: Option<String>,
    final_response_usage: Option<LlmUsage>,
    diagnostics: Vec<TranslationProtocolDiagnostic>,
}

/// 一个任务的脱敏强审计事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskLogRecord {
    Complete {
        context: TranslationTaskLogContext,
        accepted: NonEmptyTaskItems<LoggedAcceptedTranslationDecision>,
    },
    Partial {
        context: TranslationTaskLogContext,
        accepted: NonEmptyTaskItems<LoggedAcceptedTranslationDecision>,
        unresolved: NonEmptyTaskItems<LoggedUnresolvedTranslationUnit>,
    },
    Unavailable {
        context: TranslationTaskLogContext,
        reason: TranslationTaskUnavailableReason,
        unresolved: NonEmptyTaskItems<LoggedUnresolvedTranslationUnit>,
    },
}

impl TranslationTaskLogRecord {
    fn context(outcome: &TranslationTaskOutcome) -> TranslationTaskLogContext {
        TranslationTaskLogContext {
            task_index: outcome.task_index(),
            attempts: outcome.attempts(),
            provider_request_id: outcome.provider_request_id().map(str::to_owned),
            provider_response_id: outcome.provider_response_id().map(str::to_owned),
            finish_reason: outcome.finish_reason().map(str::to_owned),
            final_response_usage: outcome.final_response_usage(),
            diagnostics: outcome.diagnostics().to_vec(),
        }
    }

    pub(crate) fn from_outcome(outcome: &TranslationTaskOutcome) -> Self {
        let context = Self::context(outcome);
        match outcome {
            TranslationTaskOutcome::Complete { accepted, .. } => Self::Complete {
                context,
                accepted: accepted.map_ref(LoggedAcceptedTranslationDecision::from_accepted),
            },
            TranslationTaskOutcome::Partial {
                accepted,
                unresolved,
                ..
            } => Self::Partial {
                context,
                accepted: accepted.map_ref(LoggedAcceptedTranslationDecision::from_accepted),
                unresolved: unresolved.map_ref(LoggedUnresolvedTranslationUnit::from_unresolved),
            },
            TranslationTaskOutcome::Unavailable {
                reason, unresolved, ..
            } => Self::Unavailable {
                context,
                reason: reason.clone(),
                unresolved: unresolved.map_ref(LoggedUnresolvedTranslationUnit::from_unresolved),
            },
        }
    }

    fn context_ref(&self) -> &TranslationTaskLogContext {
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

    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.context_ref().provider_request_id.as_deref()
    }

    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.context_ref().provider_response_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> Option<&str> {
        self.context_ref().finish_reason.as_deref()
    }

    pub(crate) const fn final_response_usage(&self) -> Option<LlmUsage> {
        match self {
            Self::Complete { context, .. }
            | Self::Partial { context, .. }
            | Self::Unavailable { context, .. } => context.final_response_usage,
        }
    }

    pub(crate) fn accepted_decisions(&self) -> usize {
        match self {
            Self::Complete { accepted, .. } | Self::Partial { accepted, .. } => accepted.len(),
            Self::Unavailable { .. } => 0,
        }
    }

    pub(crate) fn accepted(&self) -> &[LoggedAcceptedTranslationDecision] {
        match self {
            Self::Complete { accepted, .. } | Self::Partial { accepted, .. } => accepted.as_slice(),
            Self::Unavailable { .. } => &[],
        }
    }

    pub(crate) fn unresolved(&self) -> &[LoggedUnresolvedTranslationUnit] {
        match self {
            Self::Partial { unresolved, .. } | Self::Unavailable { unresolved, .. } => {
                unresolved.as_slice()
            }
            Self::Complete { .. } => &[],
        }
    }

    pub(crate) fn diagnostics(&self) -> &[TranslationProtocolDiagnostic] {
        &self.context_ref().diagnostics
    }
}

/// 日志中的一个合格 ID 及其去重传播族。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoggedAcceptedTranslationDecision {
    id: usize,
    leader: LogicalTextLocation,
    propagation_targets: Vec<LogicalTextLocation>,
}

impl LoggedAcceptedTranslationDecision {
    fn from_accepted(accepted: &AcceptedTranslationDecision) -> Self {
        Self {
            id: accepted.id(),
            leader: accepted.identity().logical_location().clone(),
            propagation_targets: accepted
                .propagation_targets()
                .iter()
                .map(|target| target.identity().logical_location().clone())
                .collect(),
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn leader(&self) -> &LogicalTextLocation {
        &self.leader
    }

    pub(crate) fn propagation_targets(&self) -> &[LogicalTextLocation] {
        &self.propagation_targets
    }
}

/// 日志只保留结构化位置和拒绝原因，不复制原文或模型响应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoggedUnresolvedTranslationUnit {
    id: usize,
    locations: Vec<LogicalTextLocation>,
    reason: TranslationUnitRejectionReason,
}

impl LoggedUnresolvedTranslationUnit {
    fn from_unresolved(unresolved: &UnresolvedTranslationUnit) -> Self {
        let locations = std::iter::once(unresolved.identity().logical_location().clone())
            .chain(
                unresolved
                    .propagation_targets()
                    .iter()
                    .map(|target| target.logical_location().clone()),
            )
            .collect();
        Self {
            id: unresolved.id(),
            locations,
            reason: unresolved.reason().clone(),
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn locations(&self) -> &[LogicalTextLocation] {
        &self.locations
    }

    pub(crate) fn reason(&self) -> &TranslationUnitRejectionReason {
        &self.reason
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
/// 提供的容量切割；不得为了填满容量拼接无关范围。每个 TaskBlock 内待翻译单元的
/// ID 从 0 连续递增，虚原文只保留原文且没有 ID。省略外部资源时复用项目当前快照；
/// 显式资源在全部解析成功后成为新快照。Planner 按每个叶子实际触发的术语和占位符
/// 语义对账，并把资源更新、失效清理与可复用传播一并写入 Preparation。
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
        task: TranslationTaskBlock,
    ) -> impl Future<Output = Result<TranslationTaskOutcome, Self::Error>> + Send;
}

/// 拥有标准译文准备与单任务提交事务的存储边界。
pub(crate) trait StandardTranslationResultStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    /// 在任何 LLM 请求前原子应用一次 Planner 建立的准备。
    ///
    /// 对每个受影响叶子同时清除译文及旧语义状态，并用预期原文阻止过时计划写入；
    /// 未列出的译文保持不变。
    fn apply_preparation(
        &self,
        project: &OpenedProject,
        preparation: TranslationPlanPreparation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// 原子提交一个指定任务的全部验收译文。
    fn commit(
        &self,
        project: &OpenedProject,
        result: ValidatedTranslationTaskResult,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
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

/// 使用四个业务能力和统一强审计账本编排一次标准资产翻译。
pub(crate) struct StandardTranslationService<R, P, E, S, J> {
    asset_reader: R,
    task_planner: P,
    task_executor: E,
    result_store: S,
    event_log: J,
    cancellation: CooperativeCancellation,
}

impl<R, P, E, S, J> StandardTranslationService<R, P, E, S, J> {
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
            cancellation,
        }
    }
}

impl<R, P, E, S, J> StandardTranslation for StandardTranslationService<R, P, E, S, J>
where
    R: StandardTranslationAssetReader,
    P: StandardTranslationTaskPlanner,
    E: StandardTranslationTaskExecutor<Profile = P::Profile>,
    S: StandardTranslationResultStore,
    J: AuditLedger,
{
    type Profile = P::Profile;
    type Error = StandardTranslationServiceError<R::Error, P::Error, E::Error, S::Error, J::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> Result<OperationCompletion<StandardTranslationRunReport>, Self::Error> {
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
        let mut report = StandardTranslationRunReport::with_reconciliation(
            tasks.len(),
            preparation.retained(),
            preparation.invalidated(),
            preparation.not_applicable(),
            preparation.reused(),
        )
        .with_semantics(semantics);

        self.result_store
            .apply_preparation(project, preparation)
            .await
            .map_err(StandardTranslationServiceError::ApplyPreparation)?;

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let max_in_flight = profile.max_in_flight_tasks().get();
        let mut tasks = tasks.into_iter();
        let execute_task = |task: TranslationTaskBlock| {
            let task_index = task.index();
            async move {
                if self.cancellation.is_requested() {
                    return Ok(None);
                }
                let operation_id = self.event_log.new_operation_id().map_err(|source| {
                    StandardTranslationServiceError::CreateTaskOperationId { task_index, source }
                })?;
                self.event_log
                    .append(AuditEvent::TranslationTaskStarted {
                        operation_id,
                        task_index,
                    })
                    .await
                    .map_err(
                        |source| StandardTranslationServiceError::RecordTaskStarted {
                            task_index,
                            source,
                        },
                    )?;
                match self.task_executor.execute(profile, task).await {
                    Ok(outcome) => Ok(Some((operation_id, outcome))),
                    Err(source) => {
                        let finish = self
                            .event_log
                            .append(AuditEvent::TranslationTaskFinished {
                                operation_id,
                                result: TranslationTaskAuditResult::ExecutionFailed { task_index },
                            })
                            .await;
                        match finish {
                            Ok(_) if self.cancellation.is_requested() => Ok(None),
                            Ok(_) => Err(StandardTranslationServiceError::ExecuteTask {
                                task_index,
                                source,
                            }),
                            Err(log_source) => Err(
                                StandardTranslationServiceError::ExecuteTaskAndRecordFailure {
                                    task_index,
                                    source,
                                    log_source,
                                },
                            ),
                        }
                    }
                }
            }
        };
        let mut results = FuturesOrdered::new();
        for _ in 0..max_in_flight {
            if self.cancellation.is_requested() {
                break;
            }
            let Some(task) = tasks.next() else {
                break;
            };
            results.push_back(execute_task(task));
        }

        let mut primary_failure = None;
        let mut drain_audit_failures = Vec::new();

        while let Some(result) = results.next().await {
            let Some((operation_id, outcome)) = (match result {
                Ok(value) => value,
                Err(StandardTranslationServiceError::ExecuteTaskAndRecordFailure {
                    task_index,
                    log_source,
                    ..
                }) if primary_failure.is_some() => {
                    drain_audit_failures.push(TaskDrainAuditFailure {
                        task_index,
                        source: log_source,
                    });
                    None
                }
                Err(source) => {
                    if primary_failure.is_none() {
                        primary_failure = Some(source);
                    }
                    None
                }
            }) else {
                continue;
            };
            let task_index = outcome.task_index();

            if primary_failure.is_some() {
                let terminal = AuditEvent::TranslationTaskFinished {
                    operation_id,
                    result: TranslationTaskAuditResult::NotCommitted(
                        TranslationTaskLogRecord::from_outcome(&outcome),
                    ),
                };
                if let Err(source) = self.event_log.append(terminal).await {
                    drain_audit_failures.push(TaskDrainAuditFailure { task_index, source });
                }
                continue;
            }

            if let Some(result) = outcome.validated_result()
                && let Err(commit_source) = self.result_store.commit(project, result).await
            {
                let event = AuditEvent::TranslationTaskFinished {
                    operation_id,
                    result: TranslationTaskAuditResult::CommitFailed(
                        TranslationTaskLogRecord::from_outcome(&outcome),
                    ),
                };
                let failure = match self.event_log.append(event).await {
                    Ok(_) => StandardTranslationServiceError::CommitTask {
                        task_index,
                        source: commit_source,
                    },
                    Err(log_source) => {
                        StandardTranslationServiceError::CommitTaskAndRecordFailure {
                            task_index,
                            commit_source,
                            log_source,
                        }
                    }
                };
                primary_failure = Some(failure);
                continue;
            }

            report.record(&outcome);
            if let Err(source) = self
                .event_log
                .append(AuditEvent::TranslationTaskFinished {
                    operation_id,
                    result: TranslationTaskAuditResult::Completed(
                        TranslationTaskLogRecord::from_outcome(&outcome),
                    ),
                })
                .await
            {
                primary_failure = Some(StandardTranslationServiceError::RecordTaskFinished {
                    task_index,
                    source,
                });
                continue;
            }

            if !self.cancellation.is_requested()
                && let Some(task) = tasks.next()
            {
                results.push_back(execute_task(task));
            }
        }

        if let Some(primary) = primary_failure {
            if drain_audit_failures.is_empty() {
                return Err(primary);
            }
            return Err(
                StandardTranslationServiceError::TaskFailureAndDrainAuditFailures {
                    primary: Box::new(primary),
                    drain_audit_failures,
                },
            );
        }

        if self.cancellation.is_requested() {
            Ok(OperationCompletion::Cancelled)
        } else {
            Ok(OperationCompletion::Completed(report))
        }
    }
}

/// Standard 在直接依赖边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum StandardTranslationServiceError<R, P, E, S, J> {
    ReadAssets(R),
    PlanTasks(P),
    ApplyPreparation(S),
    CreateTaskOperationId {
        task_index: StandardTranslationTaskIndex,
        source: J,
    },
    RecordTaskStarted {
        task_index: StandardTranslationTaskIndex,
        source: J,
    },
    ExecuteTask {
        task_index: StandardTranslationTaskIndex,
        source: E,
    },
    ExecuteTaskAndRecordFailure {
        task_index: StandardTranslationTaskIndex,
        source: E,
        log_source: J,
    },
    CommitTask {
        task_index: StandardTranslationTaskIndex,
        source: S,
    },
    CommitTaskAndRecordFailure {
        task_index: StandardTranslationTaskIndex,
        commit_source: S,
        log_source: J,
    },
    RecordTaskFinished {
        task_index: StandardTranslationTaskIndex,
        source: J,
    },
    TaskFailureAndDrainAuditFailures {
        primary: Box<StandardTranslationServiceError<R, P, E, S, J>>,
        drain_audit_failures: Vec<TaskDrainAuditFailure<J>>,
    },
}

/// Standard 翻译失败已经造成的最高层用户影响。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StandardTranslationFailureImpact {
    ConfigurationOrInput,
    ProjectState,
    ExternalModel,
    AuditLedger,
    StateAppliedButFinalizationFailed,
}

impl<R, P, E, S, J> StandardTranslationServiceError<R, P, E, S, J> {
    /// 将内部阶段失败归并为命令边界可以准确呈现的用户影响。
    pub(crate) fn failure_impact(&self) -> StandardTranslationFailureImpact {
        use StandardTranslationFailureImpact as Impact;

        match self {
            Self::ReadAssets(_) | Self::ApplyPreparation(_) | Self::CommitTask { .. } => {
                Impact::ProjectState
            }
            Self::PlanTasks(_) => Impact::ConfigurationOrInput,
            Self::CreateTaskOperationId { .. } | Self::RecordTaskStarted { .. } => {
                Impact::AuditLedger
            }
            Self::ExecuteTask { .. } => Impact::ExternalModel,
            Self::ExecuteTaskAndRecordFailure { .. } | Self::CommitTaskAndRecordFailure { .. } => {
                Impact::AuditLedger
            }
            Self::RecordTaskFinished { .. } => Impact::StateAppliedButFinalizationFailed,
            Self::TaskFailureAndDrainAuditFailures {
                primary,
                drain_audit_failures,
            } => {
                let primary = primary.failure_impact();
                if drain_audit_failures.is_empty()
                    || primary == Impact::StateAppliedButFinalizationFailed
                {
                    primary
                } else {
                    Impact::AuditLedger
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskDrainAuditFailure<J> {
    task_index: StandardTranslationTaskIndex,
    source: J,
}

impl<R, P, E, S, J> fmt::Display for StandardTranslationServiceError<R, P, E, S, J>
where
    R: fmt::Display,
    P: fmt::Display,
    E: fmt::Display,
    S: fmt::Display,
    J: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadAssets(source) => write!(formatter, "无法读取标准翻译资产：{source}"),
            Self::PlanTasks(source) => write!(formatter, "无法建立标准翻译计划：{source}"),
            Self::ApplyPreparation(source) => {
                write!(formatter, "无法应用标准翻译准备：{source}")
            }
            Self::CreateTaskOperationId { task_index, source } => write!(
                formatter,
                "标准翻译任务 {task_index} 无法建立审计操作身份：{source}"
            ),
            Self::RecordTaskStarted { task_index, source } => write!(
                formatter,
                "标准翻译任务 {task_index} 的执行意图未能写入审计账本：{source}"
            ),
            Self::ExecuteTask { task_index, source } => {
                write!(formatter, "标准翻译任务 {task_index} 执行失败：{source}")
            }
            Self::ExecuteTaskAndRecordFailure {
                task_index,
                source,
                log_source,
            } => write!(
                formatter,
                "标准翻译任务 {task_index} 执行失败且无法记录终态：执行：{source}；审计：{log_source}"
            ),
            Self::CommitTask { task_index, source } => {
                write!(formatter, "标准翻译任务 {task_index} 提交失败：{source}")
            }
            Self::CommitTaskAndRecordFailure {
                task_index,
                commit_source,
                log_source,
            } => write!(
                formatter,
                "标准翻译任务 {task_index} 提交失败且无法记录诊断：提交：{commit_source}；日志：{log_source}"
            ),
            Self::RecordTaskFinished { task_index, source } => {
                write!(
                    formatter,
                    "标准翻译任务 {task_index} 的已确认结果无法写入审计账本：{source}"
                )
            }
            Self::TaskFailureAndDrainAuditFailures {
                primary,
                drain_audit_failures,
            } => {
                write!(
                    formatter,
                    "{primary}；排空已启动任务时另有 {} 个终态未能写入审计账本",
                    drain_audit_failures.len()
                )?;
                for failure in drain_audit_failures {
                    write!(
                        formatter,
                        "；任务 {}：{}",
                        failure.task_index, failure.source
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl<R, P, E, S, J> Error for StandardTranslationServiceError<R, P, E, S, J>
where
    R: Error + 'static,
    P: Error + 'static,
    E: Error + 'static,
    S: Error + 'static,
    J: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadAssets(source) => Some(source),
            Self::PlanTasks(source) => Some(source),
            Self::ApplyPreparation(source) => Some(source),
            Self::CreateTaskOperationId { source, .. }
            | Self::RecordTaskStarted { source, .. }
            | Self::RecordTaskFinished { source, .. } => Some(source),
            Self::ExecuteTask { source, .. } => Some(source),
            Self::ExecuteTaskAndRecordFailure { source, .. } => Some(source),
            Self::CommitTask { source, .. } => Some(source),
            Self::CommitTaskAndRecordFailure { commit_source, .. } => Some(commit_source),
            Self::TaskFailureAndDrainAuditFailures { primary, .. } => Some(primary.as_ref()),
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
    use crate::observability::{EventId, OperationId};
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextFieldRole};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};
    use uuid::Uuid;

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
    fn task_block_keeps_prompt_context_and_internal_post_processing_facts_separate() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(10)],
        );
        let name_identity = TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextFieldRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            "宝剑",
            "{}",
        );
        let description_identity = TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextFieldRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法")),
            "装备后提升 \\N[1] 的攻击力",
            "{}",
        );
        let terminology = TerminologyDependency::new("攻击力", "Attack");
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
            vec![TranslationTaskGroup::new(
                TextGroupKind::DatabaseEntry,
                group_location.clone(),
                vec![
                    TranslationTaskUnit::virtual_context(
                        "name",
                        name_identity.clone(),
                        "宝剑",
                        Vec::new(),
                        TranslationVirtualReason::ExistingTranslation,
                    ),
                    TranslationTaskUnit::active(
                        "description",
                        description_identity.clone(),
                        "装备后提升 <att:actor-name:0> 的攻击力",
                        vec![placeholder.clone()],
                        0,
                    ),
                ],
            )],
            vec![terminology.clone()],
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Translation contract"),
                ChatMessage::new(ChatMessageRole::User, "# Content\n\n..."),
            ],
            vec![ExpectedTranslationOutput::new(
                0,
                description_identity,
                Vec::new(),
                vec![placeholder],
                test_language_analysis(),
                test_state_context(1),
                Vec::new(),
            )],
        );

        assert_eq!(block.index(), StandardTranslationTaskIndex::new(4));
        assert_eq!(block.language_pair().source().as_str(), "ja");
        assert_eq!(block.language_pair().target().as_str(), "zh-Hans");
        assert_eq!(block.groups()[0].group_location(), &group_location);
        assert!(matches!(
            block.groups()[0].units()[0].mode(),
            TranslationTaskUnitMode::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert_eq!(block.groups()[0].units()[0].identity(), &name_identity);
        assert_eq!(
            block.groups()[0].units()[1].mode(),
            &TranslationTaskUnitMode::Active { id: 0 }
        );
        assert_eq!(
            block.groups()[0].units()[1].protected_text(),
            "装备后提升 <att:actor-name:0> 的攻击力"
        );
        assert_eq!(
            block.expected_outputs()[0].identity().kind(),
            TextGroupKind::DatabaseEntry
        );
        assert_eq!(
            block.expected_outputs()[0].identity().group_location(),
            &group_location
        );
        assert_eq!(block.injected_terminology()[0].term(), "攻击力");
        assert_eq!(block.messages().len(), 2);
        assert_eq!(block.expected_outputs()[0].id(), 0);
        assert_eq!(
            block.expected_outputs()[0].applied_placeholders()[0].scope(),
            "rpg_maker.event.control_character.actor_name"
        );
        assert_eq!(
            block.expected_outputs()[0].applied_placeholders()[0].original(),
            "\\N[1]"
        );
    }

    #[derive(Clone, Copy)]
    struct FakeProfile {
        max_in_flight_tasks: NonZeroUsize,
    }

    impl StandardTranslationProfile for FakeProfile {
        fn max_in_flight_tasks(&self) -> NonZeroUsize {
            self.max_in_flight_tasks
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        Read,
        Plan,
        Prepare,
        AuditTaskStarted(usize),
        Execute(usize),
        Complete(usize),
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
                        expected_output(index, 0, true),
                        expected_output(index, 1, false),
                    ];
                    TranslationTaskBlock::new(
                        StandardTranslationTaskIndex::new(index),
                        test_language_pair(),
                        Vec::new(),
                        Vec::new(),
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
        fail_at: Option<usize>,
        outcome_kinds: Arc<Vec<FakeOutcomeKind>>,
        cancel_on_start: Option<(usize, CooperativeCancellation)>,
    }

    impl StandardTranslationTaskExecutor for FakeExecutor {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn execute(
            &self,
            _profile: &Self::Profile,
            task: TranslationTaskBlock,
        ) -> Result<TranslationTaskOutcome, Self::Error> {
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

            for _ in 0..self.yields_by_task.get(index).copied().unwrap_or(1) {
                tokio::task::yield_now().await;
            }

            self.active.fetch_sub(1, Ordering::SeqCst);
            record(&self.events, Event::Complete(index));
            if self.fail_at == Some(index) {
                Err(FakeError("execute"))
            } else {
                Ok(fake_outcome(
                    task_index,
                    task.expected_outputs(),
                    self.outcome_kinds
                        .get(index)
                        .copied()
                        .unwrap_or(FakeOutcomeKind::Complete),
                ))
            }
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        events: Arc<Mutex<Vec<Event>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
        fail_preparation: bool,
        fail_commit_at: Option<usize>,
    }

    impl StandardTranslationResultStore for FakeStore {
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

        async fn commit(
            &self,
            _project: &OpenedProject,
            result: ValidatedTranslationTaskResult,
        ) -> Result<(), Self::Error> {
            let index = result.task_index().get();
            record(&self.events, Event::CommitAttempt(index));
            if self.fail_commit_at == Some(index) {
                Err(FakeError("commit"))
            } else {
                record(&self.events, Event::Commit(index));
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeEventLog {
        events: Arc<Mutex<Vec<Event>>>,
        records: Arc<Mutex<Vec<AuditEvent>>>,
        fail_task_at: Option<usize>,
        next_operation_id: Arc<AtomicUsize>,
    }

    impl AuditLedger for FakeEventLog {
        type Error = FakeError;

        fn new_operation_id(&self) -> Result<OperationId, Self::Error> {
            let value = self.next_operation_id.fetch_add(1, Ordering::SeqCst);
            Ok(OperationId::from_uuid(Uuid::from_u128(
                0x550e_8400_e29b_41d4_a716_4466_5544_0000 + value as u128,
            )))
        }

        async fn append(&self, event: AuditEvent) -> Result<EventId, Self::Error> {
            let failure = match &event {
                AuditEvent::TranslationTaskStarted { task_index, .. } => {
                    record(&self.events, Event::AuditTaskStarted(task_index.get()));
                    false
                }
                AuditEvent::TranslationTaskFinished { result, .. } => {
                    let task_index = match result {
                        TranslationTaskAuditResult::Completed(task) => {
                            record(&self.events, Event::LogTask(task.task_index().get()));
                            task.task_index().get()
                        }
                        TranslationTaskAuditResult::CommitFailed(task) => {
                            record(
                                &self.events,
                                Event::LogCommitFailure(task.task_index().get()),
                            );
                            task.task_index().get()
                        }
                        TranslationTaskAuditResult::NotCommitted(task) => {
                            record(
                                &self.events,
                                Event::LogNotCommitted(task.task_index().get()),
                            );
                            task.task_index().get()
                        }
                        TranslationTaskAuditResult::ExecutionFailed { task_index } => {
                            record(&self.events, Event::LogExecutionFailure(task_index.get()));
                            task_index.get()
                        }
                    };
                    self.fail_task_at == Some(task_index)
                }
                AuditEvent::RunStarted
                | AuditEvent::RunFinished { .. }
                | AuditEvent::WriteBackPublishStarted { .. }
                | AuditEvent::WriteBackPublishFinished { .. } => false,
            };
            self.records
                .lock()
                .expect("日志事件记录锁不应中毒")
                .push(event);
            if failure {
                Err(FakeError("log"))
            } else {
                Ok(EventId::from_uuid(Uuid::from_u128(
                    0x7c9e_6679_7425_40de_944b_e07f_c1f9_0ae7,
                )))
            }
        }
    }

    type Service =
        StandardTranslationService<FakeReader, FakePlanner, FakeExecutor, FakeStore, FakeEventLog>;

    struct Harness {
        service: Service,
        events: Arc<Mutex<Vec<Event>>>,
        planner_inputs: Arc<Mutex<Vec<StandardTranslationInput>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
        log_records: Arc<Mutex<Vec<AuditEvent>>>,
        max_active: Arc<AtomicUsize>,
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
            None,
            false,
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
        log_failure_at: Option<usize>,
        _run_log_failure: bool,
    ) -> Harness {
        let events = Arc::new(Mutex::new(Vec::new()));
        let planner_inputs = Arc::new(Mutex::new(Vec::new()));
        let preparations = Arc::new(Mutex::new(Vec::new()));
        let log_records = Arc::new(Mutex::new(Vec::new()));
        let max_active = Arc::new(AtomicUsize::new(0));
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
                    fail_at: execute_failure_at,
                    outcome_kinds: Arc::new(outcome_kinds),
                    cancel_on_start: None,
                },
                FakeStore {
                    events: Arc::clone(&events),
                    preparations: Arc::clone(&preparations),
                    fail_preparation: preparation_failure,
                    fail_commit_at: commit_failure_at,
                },
                FakeEventLog {
                    events: Arc::clone(&events),
                    records: Arc::clone(&log_records),
                    fail_task_at: log_failure_at,
                    next_operation_id: Arc::new(AtomicUsize::new(0)),
                },
                cancellation.clone(),
            ),
            events,
            planner_inputs,
            preparations,
            log_records,
            max_active,
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
            .run(&project(), &profile(2), input())
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
        assert_eq!(committed(&events), [0]);
        assert_eq!(logged_tasks(&events), [0]);
        assert!(!events.contains(&Event::LogTask(1)));
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
        assert_all_started_operations_finished(&harness.log_records);
    }

    #[tokio::test]
    async fn task_failure_preserves_audit_failures_while_draining_every_started_request() {
        let harness = harness_with_behavior(
            4,
            vec![1; 4],
            false,
            false,
            false,
            Some(1),
            None,
            empty_preparation(),
            vec![FakeOutcomeKind::Complete; 4],
            Some(2),
            false,
        );

        let error = harness
            .service
            .run(&project(), &profile(4), input())
            .await
            .expect_err("首因和排空审计失败都必须上交");
        assert!(matches!(
            error,
            StandardTranslationServiceError::TaskFailureAndDrainAuditFailures {
                primary,
                drain_audit_failures,
            } if matches!(
                *primary,
                StandardTranslationServiceError::ExecuteTask {
                    task_index,
                    source: FakeError("execute"),
                } if task_index == StandardTranslationTaskIndex::new(1)
            ) && drain_audit_failures.len() == 1
                && drain_audit_failures[0].task_index == StandardTranslationTaskIndex::new(2)
                && drain_audit_failures[0].source == FakeError("log")
        ));
        assert_all_started_operations_finished(&harness.log_records);
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
            None,
            false,
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
        let failure = records
            .iter()
            .find_map(|event| match event {
                AuditEvent::TranslationTaskFinished {
                    result: TranslationTaskAuditResult::CommitFailed(failure),
                    ..
                } => Some(failure),
                _ => None,
            })
            .expect("提交失败时应持久记录已经形成的内容诊断");
        assert_eq!(failure.accepted_decisions(), 1);
        assert!(matches!(
            failure.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
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
                "旧译文",
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
            None,
            false,
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

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        let completed = records
            .iter()
            .filter_map(|event| match event {
                AuditEvent::TranslationTaskFinished {
                    result: TranslationTaskAuditResult::Completed(task),
                    ..
                } => Some(task),
                _ => None,
            })
            .collect::<Vec<_>>();
        let partial = completed[0];
        assert!(matches!(partial, TranslationTaskLogRecord::Partial { .. }));
        assert_eq!(partial.accepted_decisions(), 1);
        assert_eq!(partial.accepted()[0].id(), 0);
        assert_eq!(
            partial.accepted()[0].leader(),
            expected_output(0, 0, true).identity().logical_location()
        );
        assert_eq!(partial.accepted()[0].propagation_targets().len(), 1);
        assert!(matches!(
            partial.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
        assert!(matches!(
            partial.diagnostics()[0],
            TranslationProtocolDiagnostic::UnknownId { id: 99, .. }
        ));

        let unavailable = completed[1];
        assert!(matches!(
            unavailable,
            TranslationTaskLogRecord::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
        assert_eq!(unavailable.unresolved().len(), 2);
        assert!(matches!(
            unavailable.diagnostics()[0],
            TranslationProtocolDiagnostic::InvalidResponse { .. }
        ));
    }

    #[tokio::test]
    async fn persistent_log_failure_is_a_technical_error_after_the_task_commit() {
        let harness = harness_with_behavior(
            3,
            vec![1; 3],
            false,
            false,
            false,
            None,
            None,
            empty_preparation(),
            vec![FakeOutcomeKind::Complete; 3],
            Some(1),
            false,
        );

        let error = harness
            .service
            .run(&project(), &profile(3), input())
            .await
            .expect_err("持久日志失败必须阻断后续任务");

        assert!(matches!(
            error,
            StandardTranslationServiceError::RecordTaskFinished {
                task_index,
                source: FakeError("log")
            } if task_index == StandardTranslationTaskIndex::new(1)
        ));
        let events = events(&harness.events);
        assert_eq!(committed(&events), vec![0, 1]);
        assert!(!events.contains(&Event::CommitAttempt(2)));
        assert!(events.contains(&Event::LogNotCommitted(2)));
    }

    #[tokio::test]
    async fn commit_and_diagnostic_log_failures_are_preserved_together() {
        let harness = harness_with_behavior(
            2,
            vec![1; 2],
            false,
            false,
            false,
            None,
            Some(1),
            empty_preparation(),
            vec![FakeOutcomeKind::Complete, FakeOutcomeKind::Partial],
            Some(1),
            false,
        );

        let error = harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect_err("提交和诊断日志同时失败必须保留两个原因");

        assert!(matches!(
            error,
            StandardTranslationServiceError::CommitTaskAndRecordFailure {
                task_index,
                commit_source: FakeError("commit"),
                log_source: FakeError("log")
            } if task_index == StandardTranslationTaskIndex::new(1)
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
        }
    }

    #[test]
    fn execution_future_is_send() {
        let harness = harness(1, vec![1], false, false, false, None, None);
        let project = project();
        let profile = profile(1);

        assert_send(harness.service.run(&project, &profile, input()));
    }

    fn profile(max_in_flight_tasks: usize) -> FakeProfile {
        FakeProfile {
            max_in_flight_tasks: NonZeroUsize::new(max_in_flight_tasks)
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
            Vec::new(),
            test_language_analysis(),
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
            let translation = format!("译文 {}", output.id());
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
        }
    }

    fn test_non_empty<T>(items: Vec<T>) -> NonEmptyTaskItems<T> {
        let mut items = items.into_iter();
        let first = items.next().expect("测试已建立非空决定集");
        NonEmptyTaskItems::new(first, items.collect())
    }

    fn translation_identity() -> TranslationLeafIdentity {
        translation_identity_at(10, "name")
    }

    fn translation_identity_at(index: usize, field_name: &str) -> TranslationLeafIdentity {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(index)],
        );
        TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location,
            TextFieldRole::Scalar(ScalarFieldKey::new(field_name).expect("字段键应合法")),
            "宝剑",
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

    fn assert_all_started_operations_finished(records: &Arc<Mutex<Vec<AuditEvent>>>) {
        let records = records.lock().expect("审计记录锁不应中毒");
        let mut started = records
            .iter()
            .filter_map(|event| match event {
                AuditEvent::TranslationTaskStarted { operation_id, .. } => Some(*operation_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut finished = records
            .iter()
            .filter_map(|event| match event {
                AuditEvent::TranslationTaskFinished { operation_id, .. } => Some(*operation_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        started.sort_unstable();
        finished.sort_unstable();
        assert_eq!(finished, started, "正常返回前必须终结每个已持久化意图");
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
