#![allow(dead_code, reason = "Standard 直接依赖尚未接入生产实现")]

//! MZ 标准资产翻译的顶层编排。
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

use futures_util::stream::{self, StreamExt};

use crate::att_mz::text::{MzLocation, TextGroupKind};
use crate::language::LanguageAnalysis;
use crate::observability::PersistentEventLog;
use crate::project_database::StoredProjectRecord;

use super::profile::TranslationExecutionProfile;

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

impl<P> StandardTranslationProfile for TranslationExecutionProfile<P>
where
    P: Send + Sync + 'static,
{
    fn max_in_flight_tasks(&self) -> NonZeroUsize {
        self.max_in_flight_tasks()
    }
}

impl<P> StandardTranslationProfile for Arc<TranslationExecutionProfile<P>>
where
    P: Send + Sync + 'static,
{
    fn max_in_flight_tasks(&self) -> NonZeroUsize {
        TranslationExecutionProfile::max_in_flight_tasks(self.as_ref())
    }
}

/// 一个叶子的持久化身份与读取时的原文事实。
///
/// Store 在写入时可以用原文事实防止把旧计划提交到已变化的资产上。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TranslationLeafIdentity {
    kind: TextGroupKind,
    group_location: MzLocation,
    exact_location: MzLocation,
    original_text: String,
}

impl TranslationLeafIdentity {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: MzLocation,
        exact_location: MzLocation,
        original_text: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            group_location,
            exact_location,
            original_text: original_text.into(),
        }
    }

    /// 返回决定五张标准资产表中目标表的领域种类。
    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    /// 返回译文所属复合语义组的结构化位置。
    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
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
    field_name: String,
    translation: Option<String>,
    terminology_dependencies: Vec<TerminologyDependency>,
}

impl StandardTranslationAsset {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        field_name: impl Into<String>,
        translation: Option<String>,
        terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            identity,
            field_name: field_name.into(),
            translation,
            terminology_dependencies,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn field_name(&self) -> &str {
        &self.field_name
    }

    pub(crate) fn translation(&self) -> Option<&str> {
        self.translation.as_deref()
    }

    pub(crate) fn terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.terminology_dependencies
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationLeafIdentity,
        String,
        Option<String>,
        Vec<TerminologyDependency>,
    ) {
        (
            self.identity,
            self.field_name,
            self.translation,
            self.terminology_dependencies,
        )
    }
}

/// 一个不可拆散的 MZ 复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationGroup {
    kind: TextGroupKind,
    group_location: MzLocation,
    assets: Vec<StandardTranslationAsset>,
}

impl StandardTranslationGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: MzLocation,
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

    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

    pub(crate) fn assets(&self) -> &[StandardTranslationAsset] {
        &self.assets
    }

    pub(crate) fn into_assets(self) -> Vec<StandardTranslationAsset> {
        self.assets
    }
}

/// Reader 在同一个一致读视图中建立的完整标准翻译语料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationCorpus {
    groups: Vec<StandardTranslationGroup>,
}

impl StandardTranslationCorpus {
    pub(crate) fn new(groups: Vec<StandardTranslationGroup>) -> Self {
        Self { groups }
    }

    pub(crate) fn groups(&self) -> &[StandardTranslationGroup] {
        &self.groups
    }

    pub(crate) fn into_groups(self) -> Vec<StandardTranslationGroup> {
        self.groups
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
    expected_terminology_dependencies: Vec<TerminologyDependency>,
}

/// 可以直接复用的一条现有译文快照。
///
/// Store 必须在写入目标前确认种子仍保持读取时的译文和术语依赖，避免把
/// 已被并发修改的旧事实扩散到其他位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseSeed {
    identity: TranslationLeafIdentity,
    expected_translation: String,
    expected_terminology_dependencies: Vec<TerminologyDependency>,
}

impl TranslationReuseSeed {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        expected_translation: impl Into<String>,
        expected_terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            identity,
            expected_translation: expected_translation.into(),
            expected_terminology_dependencies,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> &str {
        &self.expected_translation
    }

    pub(crate) fn expected_terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.expected_terminology_dependencies
    }
}

/// 一个将被现有译文覆盖的目标及其读取时状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReuseTarget {
    identity: TranslationLeafIdentity,
    expected_translation: Option<String>,
    expected_terminology_dependencies: Vec<TerminologyDependency>,
}

impl TranslationReuseTarget {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        expected_translation: Option<String>,
        expected_terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            identity,
            expected_translation,
            expected_terminology_dependencies,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> Option<&str> {
        self.expected_translation.as_deref()
    }

    pub(crate) fn expected_terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.expected_terminology_dependencies
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
        expected_terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            identity,
            expected_translation: expected_translation.into(),
            expected_terminology_dependencies,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn expected_translation(&self) -> &str {
        &self.expected_translation
    }

    pub(crate) fn expected_terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.expected_terminology_dependencies
    }
}

/// 在任何 LLM 请求前必须完成的标准资产准备。
///
/// 每项失效同时携带读取时的旧译文和术语依赖，Store 必须在清理前原子确认这些
/// 事实仍未变化，避免并发翻译把更新后的译文误删。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlanPreparation {
    invalidations: Vec<TranslationInvalidation>,
    reuses: Vec<TranslationReuse>,
}

impl TranslationPlanPreparation {
    pub(crate) fn new(
        invalidations: Vec<TranslationInvalidation>,
        reuses: Vec<TranslationReuse>,
    ) -> Self {
        Self {
            invalidations,
            reuses,
        }
    }

    pub(crate) fn invalidations(&self) -> &[TranslationInvalidation] {
        &self.invalidations
    }

    pub(crate) fn reuses(&self) -> &[TranslationReuse] {
        &self.reuses
    }

    pub(crate) fn into_parts(self) -> (Vec<TranslationInvalidation>, Vec<TranslationReuse>) {
        (self.invalidations, self.reuses)
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

/// 发送给 LLM 的消息角色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatMessageRole {
    System,
    User,
    Assistant,
}

/// Planner 已经建立的一条确定性消息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChatMessage {
    role: ChatMessageRole,
    content: String,
}

impl ChatMessage {
    pub(crate) fn new(role: ChatMessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    pub(crate) const fn role(&self) -> ChatMessageRole {
        self.role
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }
}

/// 一个任务使用的受信源语言与目标语言事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationLanguagePair {
    source_language: String,
    target_language: String,
}

impl TranslationLanguagePair {
    pub(crate) fn new(
        source_language: impl Into<String>,
        target_language: impl Into<String>,
    ) -> Self {
        Self {
            source_language: source_language.into(),
            target_language: target_language.into(),
        }
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }
}

/// 占位符来自 MZ 内置保护规格还是用户提供的自定义规则。
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

/// TaskBlock 中按 MZ 语义顺序排列的一个原文单元。
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

    pub(crate) fn field_name(&self) -> &str {
        &self.field_name
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn original_text(&self) -> &str {
        self.identity.original_text()
    }

    pub(crate) fn protected_text(&self) -> &str {
        &self.protected_text
    }

    pub(crate) fn applied_placeholders(&self) -> &[AppliedPlaceholder] {
        &self.applied_placeholders
    }

    pub(crate) const fn mode(&self) -> &TranslationTaskUnitMode {
        &self.mode
    }
}

/// 一个任务块中的不可拆复合组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskGroup {
    kind: TextGroupKind,
    group_location: MzLocation,
    units: Vec<TranslationTaskUnit>,
}

impl TranslationTaskGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: MzLocation,
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

    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

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
    terminology_dependencies: Vec<TerminologyDependency>,
}

impl ExpectedTranslationOutput {
    pub(crate) fn new(
        id: usize,
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationLeafIdentity>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        language_analysis: LanguageAnalysis,
        terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            id,
            identity,
            propagation_targets,
            applied_placeholders,
            language_analysis,
            terminology_dependencies,
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

    pub(crate) fn terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.terminology_dependencies
    }
}

/// 一个已完成语义切块、虚原文组装、术语注入和占位符保护的任务块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskBlock {
    index: StandardTranslationTaskIndex,
    language_pair: TranslationLanguagePair,
    groups: Vec<TranslationTaskGroup>,
    injected_terminology: Vec<TerminologyDependency>,
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl TranslationTaskBlock {
    pub(crate) fn new(
        index: StandardTranslationTaskIndex,
        language_pair: TranslationLanguagePair,
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

    pub(crate) fn language_pair(&self) -> &TranslationLanguagePair {
        &self.language_pair
    }

    pub(crate) fn groups(&self) -> &[TranslationTaskGroup] {
        &self.groups
    }

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
    preparation: TranslationPlanPreparation,
    tasks: Vec<TranslationTaskBlock>,
}

impl StandardTranslationPlan {
    pub(crate) fn new(
        preparation: TranslationPlanPreparation,
        tasks: Vec<TranslationTaskBlock>,
    ) -> Self {
        Self { preparation, tasks }
    }

    pub(crate) fn into_parts(self) -> (TranslationPlanPreparation, Vec<TranslationTaskBlock>) {
        (self.preparation, self.tasks)
    }
}

/// 经过 Executor 完整验收并可直接写入的一个叶子译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPatch {
    identity: TranslationLeafIdentity,
    propagation_targets: Vec<TranslationLeafIdentity>,
    translation: String,
    terminology_dependencies: Vec<TerminologyDependency>,
}

impl TranslationPatch {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationLeafIdentity>,
        translation: impl Into<String>,
        terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            identity,
            propagation_targets,
            translation: translation.into(),
            terminology_dependencies,
        }
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        &self.identity
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationLeafIdentity] {
        &self.propagation_targets
    }

    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }

    pub(crate) fn terminology_dependencies(&self) -> &[TerminologyDependency] {
        &self.terminology_dependencies
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

    pub(crate) fn patch(&self) -> &TranslationPatch {
        &self.patch
    }

    pub(crate) fn identity(&self) -> &TranslationLeafIdentity {
        self.patch.identity()
    }

    pub(crate) fn propagation_targets(&self) -> &[TranslationLeafIdentity] {
        self.patch.propagation_targets()
    }

    pub(crate) fn translation(&self) -> &str {
        self.patch.translation()
    }

    pub(crate) fn terminology_dependencies(&self) -> &[TerminologyDependency] {
        self.patch.terminology_dependencies()
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

    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task_index
    }

    pub(crate) fn updates(&self) -> &[TranslationPatch] {
        &self.updates
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
        attempts: usize,
        message: String,
    },
    RetryAfterExceedsConfiguredMaximum {
        attempt: usize,
        retry_after: Duration,
        maximum: Duration,
        message: String,
    },
}

/// 一个任务块在本轮执行中的正常业务状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskStatus {
    Complete,
    Partial,
    Unavailable(TranslationTaskUnavailableReason),
}

/// 一个任务块的正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskOutcome {
    task_index: StandardTranslationTaskIndex,
    status: TranslationTaskStatus,
    attempts: usize,
    request_id: Option<String>,
    finish_reason: Option<String>,
    accepted: Vec<AcceptedTranslationDecision>,
    unresolved: Vec<UnresolvedTranslationUnit>,
    diagnostics: Vec<TranslationProtocolDiagnostic>,
}

impl TranslationTaskOutcome {
    pub(crate) fn complete(
        task_index: StandardTranslationTaskIndex,
        attempts: usize,
        request_id: Option<String>,
        finish_reason: Option<String>,
        accepted: Vec<AcceptedTranslationDecision>,
        diagnostics: Vec<TranslationProtocolDiagnostic>,
    ) -> Result<Self, TranslationTaskOutcomeInvariantError> {
        ensure_positive_attempts(attempts)?;
        if accepted.is_empty() {
            return Err(TranslationTaskOutcomeInvariantError::new(
                "Complete 必须包含至少一个合格翻译决定",
            ));
        }
        Ok(Self {
            task_index,
            status: TranslationTaskStatus::Complete,
            attempts,
            request_id,
            finish_reason,
            accepted,
            unresolved: Vec::new(),
            diagnostics,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn partial(
        task_index: StandardTranslationTaskIndex,
        attempts: usize,
        request_id: Option<String>,
        finish_reason: Option<String>,
        accepted: Vec<AcceptedTranslationDecision>,
        unresolved: Vec<UnresolvedTranslationUnit>,
        diagnostics: Vec<TranslationProtocolDiagnostic>,
    ) -> Result<Self, TranslationTaskOutcomeInvariantError> {
        ensure_positive_attempts(attempts)?;
        if accepted.is_empty() || unresolved.is_empty() {
            return Err(TranslationTaskOutcomeInvariantError::new(
                "Partial 必须同时包含合格与未完成翻译决定",
            ));
        }
        Ok(Self {
            task_index,
            status: TranslationTaskStatus::Partial,
            attempts,
            request_id,
            finish_reason,
            accepted,
            unresolved,
            diagnostics,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn unavailable(
        task_index: StandardTranslationTaskIndex,
        attempts: usize,
        request_id: Option<String>,
        finish_reason: Option<String>,
        reason: TranslationTaskUnavailableReason,
        unresolved: Vec<UnresolvedTranslationUnit>,
        diagnostics: Vec<TranslationProtocolDiagnostic>,
    ) -> Result<Self, TranslationTaskOutcomeInvariantError> {
        ensure_positive_attempts(attempts)?;
        if unresolved.is_empty() {
            return Err(TranslationTaskOutcomeInvariantError::new(
                "Unavailable 必须包含至少一个未完成翻译决定",
            ));
        }
        Ok(Self {
            task_index,
            status: TranslationTaskStatus::Unavailable(reason),
            attempts,
            request_id,
            finish_reason,
            accepted: Vec::new(),
            unresolved,
            diagnostics,
        })
    }

    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task_index
    }

    pub(crate) fn status(&self) -> &TranslationTaskStatus {
        &self.status
    }

    pub(crate) const fn attempts(&self) -> usize {
        self.attempts
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    pub(crate) fn accepted(&self) -> &[AcceptedTranslationDecision] {
        &self.accepted
    }

    pub(crate) fn unresolved(&self) -> &[UnresolvedTranslationUnit] {
        &self.unresolved
    }

    pub(crate) fn diagnostics(&self) -> &[TranslationProtocolDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn accepted_location_count(&self) -> usize {
        self.accepted
            .iter()
            .map(|decision| 1 + decision.propagation_targets().len())
            .sum()
    }

    pub(crate) fn unresolved_location_count(&self) -> usize {
        self.unresolved
            .iter()
            .map(UnresolvedTranslationUnit::location_count)
            .sum()
    }

    pub(crate) fn validated_result(&self) -> Option<ValidatedTranslationTaskResult> {
        (!self.accepted.is_empty()).then(|| {
            ValidatedTranslationTaskResult::new(
                self.task_index,
                self.accepted
                    .clone()
                    .into_iter()
                    .map(AcceptedTranslationDecision::into_patch)
                    .collect(),
            )
        })
    }
}

/// `TranslationTaskOutcome` 无法表达的内部非法状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskOutcomeInvariantError {
    message: &'static str,
}

impl TranslationTaskOutcomeInvariantError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for TranslationTaskOutcomeInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for TranslationTaskOutcomeInvariantError {}

fn ensure_positive_attempts(attempts: usize) -> Result<(), TranslationTaskOutcomeInvariantError> {
    if attempts == 0 {
        Err(TranslationTaskOutcomeInvariantError::new(
            "翻译任务尝试次数必须大于零",
        ))
    } else {
        Ok(())
    }
}

/// 一次 Standard 运行已经确认的正常业务汇总。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardTranslationRunReport {
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
}

impl StandardTranslationRunReport {
    pub(crate) const fn empty(total_tasks: usize) -> Self {
        Self {
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
        }
    }

    pub(crate) fn record(&mut self, outcome: &TranslationTaskOutcome) {
        match outcome.status() {
            TranslationTaskStatus::Complete => self.complete_tasks += 1,
            TranslationTaskStatus::Partial => self.partial_tasks += 1,
            TranslationTaskStatus::Unavailable(reason) => {
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

    pub(crate) const fn accepted_decisions(&self) -> usize {
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
}

/// Standard 交给唯一日志依赖的结构化事件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationLogEvent {
    TaskProcessed(TranslationTaskLogRecord),
    TaskCommitFailed(TranslationTaskCommitFailureLogRecord),
    RunCompleted(StandardTranslationRunReport),
}

/// 一个任务已经完成数据库提交后的日志事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskLogRecord {
    task_index: StandardTranslationTaskIndex,
    status: TranslationTaskStatus,
    attempts: usize,
    request_id: Option<String>,
    finish_reason: Option<String>,
    accepted_decisions: usize,
    confirmed_written_locations: Option<usize>,
    accepted: Vec<LoggedAcceptedTranslationDecision>,
    unresolved: Vec<LoggedUnresolvedTranslationUnit>,
    diagnostics: Vec<TranslationProtocolDiagnostic>,
}

impl TranslationTaskLogRecord {
    pub(crate) fn from_outcome(outcome: &TranslationTaskOutcome) -> Self {
        let accepted = outcome
            .accepted()
            .iter()
            .map(LoggedAcceptedTranslationDecision::from_accepted)
            .collect();
        let unresolved = outcome
            .unresolved()
            .iter()
            .map(LoggedUnresolvedTranslationUnit::from_unresolved)
            .collect();
        Self {
            task_index: outcome.task_index(),
            status: outcome.status().clone(),
            attempts: outcome.attempts(),
            request_id: outcome.request_id().map(str::to_owned),
            finish_reason: outcome.finish_reason().map(str::to_owned),
            accepted_decisions: outcome.accepted().len(),
            confirmed_written_locations: Some(outcome.accepted_location_count()),
            accepted,
            unresolved,
            diagnostics: outcome.diagnostics().to_vec(),
        }
    }

    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task_index
    }

    pub(crate) fn status(&self) -> &TranslationTaskStatus {
        &self.status
    }

    pub(crate) const fn attempts(&self) -> usize {
        self.attempts
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    pub(crate) const fn accepted_decisions(&self) -> usize {
        self.accepted_decisions
    }

    pub(crate) const fn confirmed_written_locations(&self) -> Option<usize> {
        self.confirmed_written_locations
    }

    pub(crate) fn accepted(&self) -> &[LoggedAcceptedTranslationDecision] {
        &self.accepted
    }

    pub(crate) fn unresolved(&self) -> &[LoggedUnresolvedTranslationUnit] {
        &self.unresolved
    }

    pub(crate) fn diagnostics(&self) -> &[TranslationProtocolDiagnostic] {
        &self.diagnostics
    }
}

/// Store 未能确认任务事务时持久化的内容验收事实。
///
/// 该事件不宣称任何位置已经写入；`commit_failure` 只保存适合调试的阶段错误文本，
/// 具体 `NotCommitted / StalePlan / OutcomeUnknown` 语义仍由原 Store 错误链负责。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskCommitFailureLogRecord {
    outcome: TranslationTaskLogRecord,
    commit_failure: String,
}

impl TranslationTaskCommitFailureLogRecord {
    pub(crate) fn new(outcome: &TranslationTaskOutcome, commit_failure: String) -> Self {
        let mut outcome = TranslationTaskLogRecord::from_outcome(outcome);
        outcome.confirmed_written_locations = None;
        Self {
            outcome,
            commit_failure,
        }
    }

    pub(crate) fn outcome(&self) -> &TranslationTaskLogRecord {
        &self.outcome
    }

    pub(crate) fn commit_failure(&self) -> &str {
        &self.commit_failure
    }
}

/// 日志中的一个合格 ID 及其去重传播族。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoggedAcceptedTranslationDecision {
    id: usize,
    leader: MzLocation,
    propagation_targets: Vec<MzLocation>,
}

impl LoggedAcceptedTranslationDecision {
    fn from_accepted(accepted: &AcceptedTranslationDecision) -> Self {
        Self {
            id: accepted.id(),
            leader: accepted.identity().exact_location().clone(),
            propagation_targets: accepted
                .propagation_targets()
                .iter()
                .map(|target| target.exact_location().clone())
                .collect(),
        }
    }

    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn leader(&self) -> &MzLocation {
        &self.leader
    }

    pub(crate) fn propagation_targets(&self) -> &[MzLocation] {
        &self.propagation_targets
    }
}

/// 日志只保留结构化位置和拒绝原因，不复制原文或模型响应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoggedUnresolvedTranslationUnit {
    id: usize,
    locations: Vec<MzLocation>,
    reason: TranslationUnitRejectionReason,
}

impl LoggedUnresolvedTranslationUnit {
    fn from_unresolved(unresolved: &UnresolvedTranslationUnit) -> Self {
        let locations = std::iter::once(unresolved.identity().exact_location().clone())
            .chain(
                unresolved
                    .propagation_targets()
                    .iter()
                    .map(|target| target.exact_location().clone()),
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

    pub(crate) fn locations(&self) -> &[MzLocation] {
        &self.locations
    }

    pub(crate) fn reason(&self) -> &TranslationUnitRejectionReason {
        &self.reason
    }
}

/// 在一个一致读视图中取得五张标准资产表的当前事实。
pub(crate) trait StandardTranslationAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &StoredProjectRecord,
    ) -> impl Future<Output = Result<StandardTranslationCorpus, Self::Error>> + Send;
}

/// 把当前语料与本次外部资料建立为确定性翻译计划。
///
/// Planner 完整拥有 MZ 自然排序、语义边界切块、源语言判定、虚原文、术语影响、
/// PCRE2 占位符保护和 Markdown 消息构造的上游承诺。Standard 只依赖它返回的计划，
/// 不跨过 Planner 重新解释这些规则。
///
/// Planner 必须先在最大仍有关联的 MZ 结构范围内组织复合 Group，再按外部 Profile
/// 提供的容量切割；不得为了填满容量拼接无关范围。每个 TaskBlock 内待翻译单元的
/// ID 从 0 连续递增，虚原文只保留原文且没有 ID。`--terms` 未提供时不得发起权威
/// 对账；显式空术语表、译名变化和删除术语的差异语义由 Planner 写入 Preparation，
/// 新增术语不让既有译文失效。
pub(crate) trait StandardTranslationTaskPlanner: Send + Sync {
    type Profile: StandardTranslationProfile;
    type Error: Error + Send + Sync + 'static;

    fn plan(
        &self,
        project: &StoredProjectRecord,
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
    /// 对每个受影响叶子同时清除译文及旧术语依赖，并用预期原文阻止过时计划写入；
    /// 未列出的译文保持不变。
    fn apply_preparation(
        &self,
        project: &StoredProjectRecord,
        preparation: TranslationPlanPreparation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// 原子提交一个指定任务的全部验收译文。
    fn commit(
        &self,
        project: &StoredProjectRecord,
        result: ValidatedTranslationTaskResult,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 完成一轮项目数据库标准 MZ 资产翻译的职责契约。
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
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> impl Future<Output = Result<StandardTranslationRunReport, Self::Error>> + Send;
}

/// 使用四个业务能力和唯一持久事件日志根编排一次标准资产翻译。
pub(crate) struct StandardTranslationService<R, P, E, S, J> {
    asset_reader: R,
    task_planner: P,
    task_executor: E,
    result_store: S,
    event_log: J,
}

impl<R, P, E, S, J> StandardTranslationService<R, P, E, S, J> {
    pub(crate) fn new(
        asset_reader: R,
        task_planner: P,
        task_executor: E,
        result_store: S,
        event_log: J,
    ) -> Self {
        Self {
            asset_reader,
            task_planner,
            task_executor,
            result_store,
            event_log,
        }
    }
}

impl<R, P, E, S, J> StandardTranslation for StandardTranslationService<R, P, E, S, J>
where
    R: StandardTranslationAssetReader,
    P: StandardTranslationTaskPlanner,
    E: StandardTranslationTaskExecutor<Profile = P::Profile>,
    S: StandardTranslationResultStore,
    J: PersistentEventLog<TranslationLogEvent>,
{
    type Profile = P::Profile;
    type Error = StandardTranslationServiceError<R::Error, P::Error, E::Error, S::Error, J::Error>;

    async fn run(
        &self,
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> Result<StandardTranslationRunReport, Self::Error> {
        let corpus = self
            .asset_reader
            .read(project)
            .await
            .map_err(StandardTranslationServiceError::ReadAssets)?;
        let plan = self
            .task_planner
            .plan(project, profile, corpus, input)
            .await
            .map_err(StandardTranslationServiceError::PlanTasks)?;
        let (preparation, tasks) = plan.into_parts();
        let mut report = StandardTranslationRunReport::empty(tasks.len());

        self.result_store
            .apply_preparation(project, preparation)
            .await
            .map_err(StandardTranslationServiceError::ApplyPreparation)?;

        let max_in_flight = profile.max_in_flight_tasks().get();
        let mut results = stream::iter(tasks.into_iter().map(|task| {
            let task_index = task.index();
            async move {
                self.task_executor
                    .execute(profile, task)
                    .await
                    .map_err(|source| (task_index, source))
            }
        }))
        .buffered(max_in_flight);

        while let Some(result) = results.next().await {
            let outcome = result.map_err(|(task_index, source)| {
                StandardTranslationServiceError::ExecuteTask { task_index, source }
            })?;
            let task_index = outcome.task_index();
            if let Some(result) = outcome.validated_result()
                && let Err(commit_source) = self.result_store.commit(project, result).await
            {
                let event = TranslationLogEvent::TaskCommitFailed(
                    TranslationTaskCommitFailureLogRecord::new(&outcome, commit_source.to_string()),
                );
                if let Err(log_source) = self.event_log.append(event).await {
                    return Err(
                        StandardTranslationServiceError::CommitTaskAndRecordFailure {
                            task_index,
                            commit_source,
                            log_source,
                        },
                    );
                }
                return Err(StandardTranslationServiceError::CommitTask {
                    task_index,
                    source: commit_source,
                });
            }

            report.record(&outcome);
            self.event_log
                .append(TranslationLogEvent::TaskProcessed(
                    TranslationTaskLogRecord::from_outcome(&outcome),
                ))
                .await
                .map_err(|source| StandardTranslationServiceError::RecordTaskEvent {
                    task_index,
                    source,
                })?;
        }

        self.event_log
            .append(TranslationLogEvent::RunCompleted(report.clone()))
            .await
            .map_err(StandardTranslationServiceError::RecordRunEvent)?;

        Ok(report)
    }
}

/// Standard 在直接依赖边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum StandardTranslationServiceError<R, P, E, S, J> {
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
    CommitTaskAndRecordFailure {
        task_index: StandardTranslationTaskIndex,
        commit_source: S,
        log_source: J,
    },
    RecordTaskEvent {
        task_index: StandardTranslationTaskIndex,
        source: J,
    },
    RecordRunEvent(J),
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
            Self::ExecuteTask { task_index, source } => {
                write!(formatter, "标准翻译任务 {task_index} 执行失败：{source}")
            }
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
            Self::RecordTaskEvent { task_index, source } => {
                write!(
                    formatter,
                    "标准翻译任务 {task_index} 无法写入持久日志：{source}"
                )
            }
            Self::RecordRunEvent(source) => {
                write!(formatter, "标准翻译运行汇总无法写入持久日志：{source}")
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
            Self::ExecuteTask { source, .. } => Some(source),
            Self::CommitTask { source, .. } => Some(source),
            Self::CommitTaskAndRecordFailure { commit_source, .. } => Some(commit_source),
            Self::RecordTaskEvent { source, .. } | Self::RecordRunEvent(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile};
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageModule, LanguageText,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[test]
    fn task_block_keeps_prompt_context_and_internal_post_processing_facts_separate() {
        let group_location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(10)],
        );
        let name_identity = TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(10), MzLocationStep::key("name")],
            ),
            "宝剑",
        );
        let description_identity = TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![
                    MzLocationStep::index(10),
                    MzLocationStep::key("description"),
                ],
            ),
            "装备后提升 \\N[1] 的攻击力",
        );
        let terminology = TerminologyDependency::new("攻击力", "Attack");
        let placeholder = AppliedPlaceholder::new(
            "<att:actor-name:0>",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            "mz.event.control_character.actor_name",
            PlaceholderSegment::Whole,
        );
        let block = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(4),
            TranslationLanguagePair::new("ja", "zh-Hans"),
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
                vec![terminology],
            )],
        );

        assert_eq!(block.index(), StandardTranslationTaskIndex::new(4));
        assert_eq!(block.language_pair().source_language(), "ja");
        assert_eq!(block.language_pair().target_language(), "zh-Hans");
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
            "mz.event.control_character.actor_name"
        );
        assert_eq!(
            block.expected_outputs()[0].applied_placeholders()[0].original(),
            "\\N[1]"
        );
    }

    #[test]
    fn task_outcome_constructors_reject_every_illegal_state_in_release_builds() {
        let task_index = StandardTranslationTaskIndex::new(0);
        assert!(
            TranslationTaskOutcome::complete(task_index, 1, None, None, Vec::new(), Vec::new())
                .is_err()
        );
        assert!(
            TranslationTaskOutcome::partial(
                task_index,
                1,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .is_err()
        );
        assert!(
            TranslationTaskOutcome::unavailable(
                task_index,
                1,
                None,
                None,
                TranslationTaskUnavailableReason::AllOutputsRejected,
                Vec::new(),
                Vec::new(),
            )
            .is_err()
        );
        assert!(
            TranslationTaskOutcome::unavailable(
                task_index,
                0,
                None,
                None,
                TranslationTaskUnavailableReason::AllOutputsRejected,
                vec![UnresolvedTranslationUnit::new(
                    0,
                    translation_identity(),
                    Vec::new(),
                    TranslationUnitRejectionReason::Missing,
                )],
                Vec::new(),
            )
            .is_err()
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
        Execute(usize),
        Complete(usize),
        CommitAttempt(usize),
        Commit(usize),
        LogTask(usize),
        LogCommitFailure(usize),
        LogRun,
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
            _project: &StoredProjectRecord,
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
            _project: &StoredProjectRecord,
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

            let tasks = (0..self.task_count)
                .map(|index| {
                    let expected_outputs = vec![
                        expected_output(index, 0, true),
                        expected_output(index, 1, false),
                    ];
                    TranslationTaskBlock::new(
                        StandardTranslationTaskIndex::new(index),
                        TranslationLanguagePair::new("ja", "zh-Hans"),
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
            _project: &StoredProjectRecord,
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
            _project: &StoredProjectRecord,
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
        records: Arc<Mutex<Vec<TranslationLogEvent>>>,
        fail_task_at: Option<usize>,
        fail_run: bool,
    }

    impl PersistentEventLog<TranslationLogEvent> for FakeEventLog {
        type Error = FakeError;

        async fn append(&self, event: TranslationLogEvent) -> Result<(), Self::Error> {
            let failure = match &event {
                TranslationLogEvent::TaskProcessed(task_record) => {
                    record(&self.events, Event::LogTask(task_record.task_index().get()));
                    self.fail_task_at == Some(task_record.task_index().get())
                }
                TranslationLogEvent::TaskCommitFailed(failure) => {
                    let task_index = failure.outcome().task_index().get();
                    record(&self.events, Event::LogCommitFailure(task_index));
                    self.fail_task_at == Some(task_index)
                }
                TranslationLogEvent::RunCompleted(_) => {
                    record(&self.events, Event::LogRun);
                    self.fail_run
                }
            };
            self.records
                .lock()
                .expect("日志事件记录锁不应中毒")
                .push(event);
            if failure {
                Err(FakeError("log"))
            } else {
                Ok(())
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
        log_records: Arc<Mutex<Vec<TranslationLogEvent>>>,
        max_active: Arc<AtomicUsize>,
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
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
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
        run_log_failure: bool,
    ) -> Harness {
        let events = Arc::new(Mutex::new(Vec::new()));
        let planner_inputs = Arc::new(Mutex::new(Vec::new()));
        let preparations = Arc::new(Mutex::new(Vec::new()));
        let log_records = Arc::new(Mutex::new(Vec::new()));
        let max_active = Arc::new(AtomicUsize::new(0));
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
                    fail_run: run_log_failure,
                },
            ),
            events,
            planner_inputs,
            preparations,
            log_records,
            max_active,
        }
    }

    #[tokio::test]
    async fn dispatches_all_stages_and_commits_each_task_in_plan_order() {
        let harness = harness(3, vec![1, 1, 1], false, false, false, None, None);

        let report = harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect("标准翻译编排应该成功");

        let events = events(&harness.events);
        assert_eq!(&events[..3], &[Event::Read, Event::Plan, Event::Prepare]);
        assert_eq!(committed(&events), vec![0, 1, 2]);
        assert_eq!(logged_tasks(&events), vec![0, 1, 2]);
        assert_eq!(events.last(), Some(&Event::LogRun));
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
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
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

        let records = harness.log_records.lock().expect("日志事件记录锁不应中毒");
        let failure = records
            .iter()
            .find_map(|event| match event {
                TranslationLogEvent::TaskCommitFailed(failure) => Some(failure),
                _ => None,
            })
            .expect("提交失败时应持久记录已经形成的内容诊断");
        assert_eq!(failure.outcome().accepted_decisions(), 1);
        assert_eq!(failure.outcome().confirmed_written_locations(), None);
        assert!(matches!(
            failure.outcome().unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
        assert_eq!(failure.commit_failure(), "commit");
    }

    #[tokio::test]
    async fn empty_plan_still_applies_preparation_without_calling_executor_or_commit() {
        let harness = harness(0, Vec::new(), false, false, false, None, None);

        let report = harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect("空计划应该成功");

        assert_eq!(
            events(&harness.events),
            vec![Event::Read, Event::Plan, Event::Prepare, Event::LogRun]
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
                vec![TerminologyDependency::new("术语", "Term")],
            )],
            Vec::new(),
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
            vec![Event::Read, Event::Plan, Event::Prepare, Event::LogRun]
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
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
            vec![
                FakeOutcomeKind::Partial,
                FakeOutcomeKind::Unavailable,
                FakeOutcomeKind::Complete,
            ],
            None,
            false,
        );

        let report = harness
            .service
            .run(&project(), &profile(3), input())
            .await
            .expect("正常的部分与无可用译文不应中断运行");

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
        let TranslationLogEvent::TaskProcessed(partial) = &records[0] else {
            panic!("首个日志事件应为部分结果");
        };
        assert!(matches!(partial.status(), TranslationTaskStatus::Partial));
        assert_eq!(partial.accepted_decisions(), 1);
        assert_eq!(partial.confirmed_written_locations(), Some(2));
        assert_eq!(partial.accepted()[0].id(), 0);
        assert_eq!(
            partial.accepted()[0].leader(),
            expected_output(0, 0, true).identity().exact_location()
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

        let TranslationLogEvent::TaskProcessed(unavailable) = &records[1] else {
            panic!("第二个日志事件应为无可用译文结果");
        };
        assert!(matches!(
            unavailable.status(),
            TranslationTaskStatus::Unavailable(
                TranslationTaskUnavailableReason::ModelResponseUnusable
            )
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
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
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
            StandardTranslationServiceError::RecordTaskEvent {
                task_index,
                source: FakeError("log")
            } if task_index == StandardTranslationTaskIndex::new(1)
        ));
        let events = events(&harness.events);
        assert_eq!(committed(&events), vec![0, 1]);
        assert!(!events.contains(&Event::CommitAttempt(2)));
        assert!(!events.contains(&Event::LogRun));
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
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
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
    async fn run_summary_log_failure_is_technical_after_all_task_logs() {
        let harness = harness_with_behavior(
            1,
            vec![1],
            false,
            false,
            false,
            None,
            None,
            TranslationPlanPreparation::new(Vec::new(), Vec::new()),
            vec![FakeOutcomeKind::Complete],
            None,
            true,
        );

        let error = harness
            .service
            .run(&project(), &profile(1), input())
            .await
            .expect_err("运行汇总日志失败必须是技术错误");

        assert!(matches!(
            error,
            StandardTranslationServiceError::RecordRunEvent(FakeError("log"))
        ));
        assert_eq!(committed(&events(&harness.events)), vec![0]);
        assert_eq!(logged_tasks(&events(&harness.events)), vec![0]);
        assert!(events(&harness.events).contains(&Event::LogRun));
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

    fn input() -> StandardTranslationInput {
        StandardTranslationInput::new(
            Some(PathBuf::from("config/terms.json")),
            Some(PathBuf::from("config/placeholders.json")),
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
            Vec::new(),
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
            AcceptedTranslationDecision::new(
                output.id(),
                TranslationPatch::new(
                    output.identity().clone(),
                    output.propagation_targets().to_vec(),
                    format!("译文 {}", output.id()),
                    output.terminology_dependencies().to_vec(),
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
            FakeOutcomeKind::Complete => TranslationTaskOutcome::complete(
                task_index,
                1,
                Some(format!("request-{}", task_index.get())),
                Some("stop".to_owned()),
                expected.iter().map(patch).collect(),
                Vec::new(),
            )
            .expect("完整测试结果必须满足状态不变量"),
            FakeOutcomeKind::Partial => TranslationTaskOutcome::partial(
                task_index,
                1,
                Some(format!("request-{}", task_index.get())),
                Some("stop".to_owned()),
                vec![patch(&expected[0])],
                vec![unresolved(
                    &expected[1],
                    TranslationUnitRejectionReason::Duplicate,
                )],
                vec![TranslationProtocolDiagnostic::UnknownId {
                    item_index: 3,
                    id: 99,
                }],
            )
            .expect("部分测试结果必须满足状态不变量"),
            FakeOutcomeKind::Unavailable => TranslationTaskOutcome::unavailable(
                task_index,
                1,
                Some(format!("request-{}", task_index.get())),
                Some("length".to_owned()),
                TranslationTaskUnavailableReason::ModelResponseUnusable,
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
                vec![TranslationProtocolDiagnostic::InvalidResponse {
                    message: "无法解析模型 JSON".to_owned(),
                }],
            )
            .expect("不可用测试结果必须满足状态不变量"),
        }
    }

    fn translation_identity() -> TranslationLeafIdentity {
        translation_identity_at(10, "name")
    }

    fn translation_identity_at(index: usize, field_name: &str) -> TranslationLeafIdentity {
        let group_location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(index)],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![
                    MzLocationStep::index(index),
                    MzLocationStep::key(field_name),
                ],
            ),
            "宝剑",
        )
    }

    fn project() -> StoredProjectRecord {
        StoredProjectRecord::new(
            project_name(),
            PathBuf::from("C:/Projects/alice"),
            PathBuf::from("C:/Projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
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
