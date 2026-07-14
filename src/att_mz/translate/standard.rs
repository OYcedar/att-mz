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

use futures_util::stream::{self, StreamExt};

use crate::att_mz::text::{MzLocation, TextGroupKind};
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
    terminology_dependencies: Vec<TerminologyDependency>,
}

impl ExpectedTranslationOutput {
    pub(crate) fn new(
        id: usize,
        identity: TranslationLeafIdentity,
        propagation_targets: Vec<TranslationLeafIdentity>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        terminology_dependencies: Vec<TerminologyDependency>,
    ) -> Self {
        Self {
            id,
            identity,
            propagation_targets,
            applied_placeholders,
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

/// Executor 对一个任务块的完整验收结果。
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

/// 执行一个已计划任务并返回可原子提交的验收结果。
///
/// 一次逻辑调用内的 LLM 重试、JSON 整理与 ID 校验、目标语言验收、源文残留检查、
/// 占位符验证与还原都属于该契约。Executor 不写项目数据库。
/// Executor 只能把 TaskBlock 已经建立的完整 `messages` 发送给 LLM；结构化位置、
/// 表归属、占位符反查和提交身份只用于程序内部验收，不能再拼入提示词。
pub(crate) trait StandardTranslationTaskExecutor: Send + Sync {
    type Profile: StandardTranslationProfile;
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        profile: &Self::Profile,
        task: TranslationTaskBlock,
    ) -> impl Future<Output = Result<ValidatedTranslationTaskResult, Self::Error>> + Send;
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

/// 完成项目数据库中标准 MZ 资产翻译的职责契约。
///
/// 成功表示全部标准资产已经完成，或当前没有待翻译资产。一次执行包含多个任务时，
/// 失败必须保留按确定顺序已经验收并提交的成功前缀；失败任务及其后续任务不得提交。
/// 本职责拥有它的重试和提交语义；顶层不重试、不推断提交范围，也不回滚已确认的成功前缀。
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
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 使用四个直接能力编排一次标准资产翻译。
pub(crate) struct StandardTranslationService<R, P, E, S> {
    asset_reader: R,
    task_planner: P,
    task_executor: E,
    result_store: S,
}

impl<R, P, E, S> StandardTranslationService<R, P, E, S> {
    pub(crate) fn new(asset_reader: R, task_planner: P, task_executor: E, result_store: S) -> Self {
        Self {
            asset_reader,
            task_planner,
            task_executor,
            result_store,
        }
    }
}

impl<R, P, E, S> StandardTranslation for StandardTranslationService<R, P, E, S>
where
    R: StandardTranslationAssetReader,
    P: StandardTranslationTaskPlanner,
    E: StandardTranslationTaskExecutor<Profile = P::Profile>,
    S: StandardTranslationResultStore,
{
    type Profile = P::Profile;
    type Error = StandardTranslationServiceError<R::Error, P::Error, E::Error, S::Error>;

    async fn run(
        &self,
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> Result<(), Self::Error> {
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
            let result = result.map_err(|(task_index, source)| {
                StandardTranslationServiceError::ExecuteTask { task_index, source }
            })?;
            let task_index = result.task_index();
            self.result_store
                .commit(project, result)
                .await
                .map_err(|source| StandardTranslationServiceError::CommitTask {
                    task_index,
                    source,
                })?;
        }

        Ok(())
    }
}

/// Standard 在四个直接依赖边界上遇到的阶段失败。
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
                    TranslationTaskBlock::new(
                        StandardTranslationTaskIndex::new(index),
                        TranslationLanguagePair::new("ja", "zh-Hans"),
                        Vec::new(),
                        Vec::new(),
                        vec![ChatMessage::new(
                            ChatMessageRole::User,
                            format!("# Task {index}"),
                        )],
                        Vec::new(),
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
    }

    impl StandardTranslationTaskExecutor for FakeExecutor {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn execute(
            &self,
            _profile: &Self::Profile,
            task: TranslationTaskBlock,
        ) -> Result<ValidatedTranslationTaskResult, Self::Error> {
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
                Ok(ValidatedTranslationTaskResult::new(task_index, Vec::new()))
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

    type Service = StandardTranslationService<FakeReader, FakePlanner, FakeExecutor, FakeStore>;

    struct Harness {
        service: Service,
        events: Arc<Mutex<Vec<Event>>>,
        planner_inputs: Arc<Mutex<Vec<StandardTranslationInput>>>,
        preparations: Arc<Mutex<Vec<TranslationPlanPreparation>>>,
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
        let events = Arc::new(Mutex::new(Vec::new()));
        let planner_inputs = Arc::new(Mutex::new(Vec::new()));
        let preparations = Arc::new(Mutex::new(Vec::new()));
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
                },
                FakeStore {
                    events: Arc::clone(&events),
                    preparations: Arc::clone(&preparations),
                    fail_preparation: preparation_failure,
                    fail_commit_at: commit_failure_at,
                },
            ),
            events,
            planner_inputs,
            preparations,
            max_active,
        }
    }

    #[tokio::test]
    async fn dispatches_all_stages_and_commits_each_task_in_plan_order() {
        let harness = harness(3, vec![1, 1, 1], false, false, false, None, None);

        harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect("标准翻译编排应该成功");

        let events = events(&harness.events);
        assert_eq!(&events[..3], &[Event::Read, Event::Plan, Event::Prepare]);
        assert_eq!(committed(&events), vec![0, 1, 2]);
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
        let harness = harness(4, vec![1; 4], false, false, false, None, Some(1));

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
        assert!(!events.contains(&Event::CommitAttempt(2)));
    }

    #[tokio::test]
    async fn empty_plan_still_applies_preparation_without_calling_executor_or_commit() {
        let harness = harness(0, Vec::new(), false, false, false, None, None);

        harness
            .service
            .run(&project(), &profile(2), input())
            .await
            .expect("空计划应该成功");

        assert_eq!(
            events(&harness.events),
            vec![Event::Read, Event::Plan, Event::Prepare]
        );
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
            vec![Event::Read, Event::Plan, Event::Prepare]
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

    fn translation_identity() -> TranslationLeafIdentity {
        let group_location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(10)],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(10), MzLocationStep::key("name")],
            ),
            "宝剑",
        )
    }

    fn project() -> StoredProjectRecord {
        StoredProjectRecord::new(
            project_name(),
            PathBuf::from("C:/Games/Alice"),
            PathBuf::from("C:/Projects/alice.db"),
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

    fn completed(events: &[Event]) -> Vec<usize> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Complete(index) => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn assert_send(_: impl Send) {}
}
