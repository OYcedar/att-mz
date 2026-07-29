//! 可信 Lua VM 的根执行契约与 Host 绑定面。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::diagnostic::SafeDiagnostic;
use crate::llm::{ChatMessage, LlmResponse};
pub(crate) use crate::lua_host::TrustedLuaHostCallError;
use crate::lua_host::TrustedLuaHostFuture;
use crate::managed_translation::managed_translations_unavailable;
pub(crate) use crate::managed_translation::{
    ManagedPreparedContent, ManagedTranslationCandidateAcceptance,
    ManagedTranslationCandidateRequest, ManagedTranslationCandidateUnit,
    TrustedLuaManagedTranslateHostCalls, TrustedLuaManagedTranslationCollection,
    TrustedLuaManagedTranslationCollectionDeclaration, TrustedLuaManagedTranslationContent,
    TrustedLuaManagedTranslationReader, TrustedLuaManagedTranslationReport,
    TrustedLuaManagedTranslationResult, TrustedLuaManagedTranslationResultStatus,
    TrustedLuaManagedTranslationShape, TrustedLuaManagedTranslationSnapshot,
    TrustedLuaManagedTranslationUnit, TrustedLuaManagedTranslationUnitDeclaration,
    TrustedLuaManagedTranslationUnitStatus, TrustedLuaPreparedTranslation,
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaPreparedTranslationStatus,
    TrustedLuaTranslationTerm,
};
use crate::rpg_maker::extract::store::LuaSnapshot;
use crate::rpg_maker::lua::document::RpgMakerTextReplacement;
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::file_system::ScopedDirectoryPath;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

use super::{LuaPhase, LuaProjectContext, LuaSourcePath};

type HostFuture<T> = TrustedLuaHostFuture<T>;

/// 已完整读取并可交给专用 Lua worker 的主程序。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OwnedLuaProgram {
    main_script_path: PathBuf,
    source: Vec<u8>,
}

impl fmt::Debug for OwnedLuaProgram {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedLuaProgram")
            .field("main_script_path", &self.main_script_path)
            .field("source_bytes", &self.source.len())
            .finish_non_exhaustive()
    }
}

impl OwnedLuaProgram {
    pub(crate) fn new(main_script_path: PathBuf, source: Vec<u8>) -> Self {
        Self {
            main_script_path,
            source,
        }
    }

    pub(crate) fn main_script_path(&self) -> &Path {
        &self.main_script_path
    }

    pub(crate) fn source(&self) -> &[u8] {
        &self.source
    }
}

/// Host 在释放绑定资源后交还给编排层的事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaBindingFinalization {
    had_unclosed_transaction: bool,
}

impl TrustedLuaBindingFinalization {
    pub(crate) const fn new(had_unclosed_transaction: bool) -> Self {
        Self {
            had_unclosed_transaction,
        }
    }

    pub(crate) const fn had_unclosed_transaction(self) -> bool {
        self.had_unclosed_transaction
    }
}

/// 唯一终结器失败。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaBindingFinalizationError {
    message: String,
    safe_diagnostics: Vec<SafeDiagnostic>,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaBindingFinalizationError {
    pub(crate) fn new(
        message: impl Into<String>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            message: message.into(),
            safe_diagnostics: Vec::new(),
            source,
        }
    }

    /// 保存 SQLite 收尾的主失败与相关失败，顺序与底层终态一致。
    pub(crate) fn with_safe_diagnostics(mut self, diagnostics: Vec<SafeDiagnostic>) -> Self {
        self.safe_diagnostics = diagnostics;
        self
    }

    pub(crate) fn safe_diagnostics(&self) -> &[SafeDiagnostic] {
        &self.safe_diagnostics
    }

    fn supervisor_lost() -> Self {
        Self::new("Lua job supervisor 在资源终结前退出", None)
    }
}

impl fmt::Display for TrustedLuaBindingFinalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TrustedLuaBindingFinalizationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Lua 各阶段与独立项目入口共同拥有的冻结来源、项目与数据库 Host 能力。
///
/// 这一调用面不包含任何阶段专属操作，也不拥有会话终结权。VM worker 只能将请求
/// 交回主 Tokio runtime 执行，不得在 Lua worker 内直接操作 SQLite 连接。
pub(crate) trait TrustedLuaCommonHostCalls: Send + Sync + 'static {
    fn project(&self) -> &LuaProjectContext;

    fn read_source(
        &self,
        path: LuaSourcePath,
    ) -> HostFuture<Result<Vec<u8>, TrustedLuaHostCallError>>;

    fn list_source(
        &self,
        path: LuaSourcePath,
    ) -> HostFuture<Result<Vec<String>, TrustedLuaHostCallError>>;

    fn query(
        &self,
        query: SqliteQuery,
    ) -> HostFuture<Result<Vec<SqliteRow>, TrustedLuaHostCallError>>;

    fn execute(&self, command: SqliteCommand) -> HostFuture<Result<u64, TrustedLuaHostCallError>>;

    fn begin(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;
    fn commit(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;
    fn rollback(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;
    fn transaction_active(&self) -> HostFuture<Result<bool, TrustedLuaHostCallError>>;
}

/// Lua Extract 主程序在内存中声明的 Standard 快照意图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardExtractIntent {
    /// 以完整快照收敛 Lua owner；空快照仍表示 active。
    Replace(LuaSnapshot),
    /// 停用 Lua owner 并清除其标准资产。
    Deactivate,
}

/// 一次干净结束的 Extract 同时交付的两个独立意图。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrustedLuaExtractIntent {
    standard: Option<TrustedLuaStandardExtractIntent>,
    managed: Option<TrustedLuaManagedTranslationSnapshot>,
}

impl TrustedLuaExtractIntent {
    pub(crate) fn new(
        standard: Option<TrustedLuaStandardExtractIntent>,
        managed: Option<TrustedLuaManagedTranslationSnapshot>,
    ) -> Self {
        Self { standard, managed }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Option<TrustedLuaStandardExtractIntent>,
        Option<TrustedLuaManagedTranslationSnapshot>,
    ) {
        (self.standard, self.managed)
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.standard.is_none() && self.managed.is_none()
    }
}

/// Extract 阶段专属 Host 能力。
///
/// Runtime 只把已校验的完整意图记录到内存，不在 VM 生命周期内写托管资产表。
pub(crate) trait TrustedLuaExtractHostCalls: Send + Sync + 'static {
    fn replace_standard(&self, snapshot: LuaSnapshot) -> Result<(), TrustedLuaHostCallError>;

    fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError>;

    fn replace_managed(
        &self,
        _snapshot: TrustedLuaManagedTranslationSnapshot,
    ) -> Result<(), TrustedLuaHostCallError> {
        Err(managed_translations_unavailable("translations.replace"))
    }
}

/// Translate 阶段专属 Host 能力。
pub(crate) trait TrustedLuaTranslateHostCalls: Send + Sync + 'static {
    fn system_prompt(&self) -> &str;
    fn source_language(&self) -> &str;
    fn target_language(&self) -> &str;

    fn prepare_translation(
        &self,
        kind: TextGroupKind,
        original: String,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>;

    fn prepare_content(
        &self,
        _kind: TextGroupKind,
        _shape: TrustedLuaManagedTranslationShape,
        _original: TrustedLuaManagedTranslationContent,
        _semantic_context: String,
    ) -> Result<Arc<ManagedPreparedContent>, TrustedLuaHostCallError> {
        Err(TrustedLuaHostCallError::new(
            "translation",
            "unavailable",
            "当前 Translate Host 未构造结构化准备能力",
            None,
            None,
        )
        .with_operation("translation.prepare_content"))
    }

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> HostFuture<Result<LlmResponse, TrustedLuaHostCallError>>;

    fn translate_managed(
        &self,
    ) -> HostFuture<Result<TrustedLuaManagedTranslationReport, TrustedLuaHostCallError>> {
        Box::pin(async { Err(managed_translations_unavailable("translations.translate")) })
    }

    fn open_managed(
        &self,
        _name: String,
    ) -> HostFuture<Result<Option<TrustedLuaManagedTranslationCollection>, TrustedLuaHostCallError>>
    {
        Box::pin(async { Err(managed_translations_unavailable("translations.open")) })
    }
}

/// Standard 已解析并冻结、Lua 只借用其结果的一轮翻译语义。
///
/// 该边界确保 Lua 不重新读取术语或占位符资源，也不复制保护、语言分析与验收算法。
pub(crate) trait TrustedLuaTranslationSemantics: Send + Sync + 'static {
    fn system_prompt(&self) -> &str;
    fn source_language(&self) -> &str;
    fn target_language(&self) -> &str;

    fn prepare_translation(
        &self,
        kind: TextGroupKind,
        original: String,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>;

    /// 为 `lines` 保留物理槽边界完成同一 ID 的整体准备。
    fn prepare_translation_lines(
        &self,
        kind: TextGroupKind,
        original: Vec<String>,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>;
}

/// 独立项目 Lua 打开 Standard 人工验收会话的 Host 能力。
///
/// Profile、资源和数据库快照如何解析完全由 Standard 核心拥有。Lua Host 只负责把
/// 已冻结的只读单元投影为 userdata，并把候选批次原样交还给该会话。
pub(crate) trait TrustedLuaStandardHostCalls: Send + Sync + 'static {
    fn open(
        &self,
    ) -> HostFuture<Result<Arc<dyn TrustedLuaStandardSession>, TrustedLuaHostCallError>>;
}

/// 独立项目 Lua 打开 Managed 人工候选会话的 Host 能力。
pub(crate) trait TrustedLuaManagedEditHostCalls: Send + Sync + 'static {
    fn edit(
        &self,
    ) -> HostFuture<Result<Arc<dyn TrustedLuaManagedEditSession>, TrustedLuaHostCallError>>;
}

/// 一轮 Managed 冻结快照及其人工候选提交边界。
pub(crate) trait TrustedLuaManagedEditSession: Send + Sync + 'static {
    fn units(&self) -> Result<Vec<ManagedTranslationCandidateUnit>, TrustedLuaHostCallError>;

    fn get(
        &self,
        collection: String,
        key: String,
    ) -> Result<Option<ManagedTranslationCandidateUnit>, TrustedLuaHostCallError>;

    fn accept(
        &self,
        candidates: Vec<ManagedTranslationCandidateRequest>,
    ) -> HostFuture<Result<Vec<ManagedTranslationCandidateAcceptance>, TrustedLuaHostCallError>>;
}

/// 一轮只读 Standard 快照及其人工候选提交边界。
///
/// `units` 与 `get` 只读取 `open` 已冻结的内存事实；`accept` 才能产生副作用。每次
/// `accept` 对所有合法去重族使用同一个核心事务，并在返回前到达明确提交终态。
pub(crate) trait TrustedLuaStandardSession: Send + Sync + 'static {
    fn units(&self) -> Vec<TrustedLuaStandardUnit>;

    fn get(
        &self,
        owner: RpgMakerStandardAssetOwner,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
    ) -> Option<TrustedLuaStandardUnit>;

    fn accept(
        &self,
        candidates: Vec<TrustedLuaStandardCandidate>,
    ) -> HostFuture<Result<Vec<TrustedLuaStandardAcceptance>, TrustedLuaHostCallError>>;
}

/// Standard 核心会话中的一个不可由 Lua 构造的物理单元。
///
/// `handle` 只在当前核心会话内有意义；Lua Runtime 另加会话身份令牌，防止同一 VM
/// 中不同 `open()` 会话的 userdata 被混用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaStandardUnit {
    handle: usize,
    owner: RpgMakerStandardAssetOwner,
    group_kind: TextGroupKind,
    group_location: RpgMakerLocation,
    role: TextUnitRole,
    original: TextUnitContent,
    source_context_json: String,
    translation: Option<TextUnitContent>,
    model_text: TextUnitContent,
    terms: Vec<TrustedLuaTranslationTerm>,
    line_policy: TrustedLuaStandardLinePolicy,
    status: TrustedLuaStandardUnitStatus,
    family_size: usize,
}

impl TrustedLuaStandardUnit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        handle: usize,
        owner: RpgMakerStandardAssetOwner,
        group_kind: TextGroupKind,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
        original: TextUnitContent,
        source_context_json: String,
        translation: Option<TextUnitContent>,
        model_text: TextUnitContent,
        terms: Vec<TrustedLuaTranslationTerm>,
        line_policy: TrustedLuaStandardLinePolicy,
        status: TrustedLuaStandardUnitStatus,
        family_size: usize,
    ) -> Self {
        Self {
            handle,
            owner,
            group_kind,
            group_location,
            role,
            original,
            source_context_json,
            translation,
            model_text,
            terms,
            line_policy,
            status,
            family_size,
        }
    }

    pub(crate) const fn handle(&self) -> usize {
        self.handle
    }

    pub(crate) const fn owner(&self) -> RpgMakerStandardAssetOwner {
        self.owner
    }

    pub(crate) const fn group_kind(&self) -> TextGroupKind {
        self.group_kind
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn role(&self) -> &TextUnitRole {
        &self.role
    }

    pub(crate) fn original(&self) -> &TextUnitContent {
        &self.original
    }

    pub(crate) fn source_context_json(&self) -> &str {
        &self.source_context_json
    }

    pub(crate) fn translation(&self) -> Option<&TextUnitContent> {
        self.translation.as_ref()
    }

    pub(crate) fn model_text(&self) -> &TextUnitContent {
        &self.model_text
    }

    pub(crate) fn terms(&self) -> &[TrustedLuaTranslationTerm] {
        &self.terms
    }

    pub(crate) const fn content_kind(&self) -> TrustedLuaStandardContentKind {
        match self.original {
            TextUnitContent::Value(_) => TrustedLuaStandardContentKind::Value,
            TextUnitContent::Lines(_) => TrustedLuaStandardContentKind::Lines,
        }
    }

    pub(crate) const fn line_policy(&self) -> TrustedLuaStandardLinePolicy {
        self.line_policy
    }

    pub(crate) const fn status(&self) -> TrustedLuaStandardUnitStatus {
        self.status
    }

    pub(crate) const fn family_size(&self) -> usize {
        self.family_size
    }
}

/// Standard 单元保留的是标量值还是独立行槽。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardContentKind {
    Value,
    Lines,
}

impl TrustedLuaStandardContentKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Lines => "lines",
        }
    }
}

/// Standard 对候选行边界的验收策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardLinePolicy {
    Single,
    Aligned(usize),
    Reflow,
}

impl TrustedLuaStandardLinePolicy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Aligned(_) => "aligned",
            Self::Reflow => "reflow",
        }
    }

    pub(crate) const fn expected_line_count(self) -> Option<usize> {
        match self {
            Self::Single => Some(1),
            Self::Aligned(count) => Some(count),
            Self::Reflow => None,
        }
    }
}

/// Standard 对当前译文/state 配对的只读判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardUnitStatus {
    Current,
    Missing,
    Stale,
    NotApplicable,
    Unavailable,
}

impl TrustedLuaStandardUnitStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Stale => "stale",
            Self::NotApplicable => "not_applicable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Lua 已按 Value/Lines 边界解析的一项人工候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaStandardCandidate {
    handle: usize,
    candidate: TextUnitContent,
    replace_current: bool,
}

impl TrustedLuaStandardCandidate {
    pub(crate) fn new(handle: usize, candidate: TextUnitContent, replace_current: bool) -> Self {
        Self {
            handle,
            candidate,
            replace_current,
        }
    }

    #[cfg(test)]
    pub(crate) const fn handle(&self) -> usize {
        self.handle
    }

    #[cfg(test)]
    pub(crate) fn candidate(&self) -> &TextUnitContent {
        &self.candidate
    }

    #[cfg(test)]
    pub(crate) const fn replace_current(&self) -> bool {
        self.replace_current
    }

    pub(crate) fn into_parts(self) -> (usize, TextUnitContent, bool) {
        (self.handle, self.candidate, self.replace_current)
    }
}

/// Standard 正常处理一项人工候选后的逐项结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardAcceptance {
    Accepted {
        translation: TextUnitContent,
        changed_locations: usize,
    },
    Rejected {
        reason: String,
        details: Vec<TrustedLuaStandardRejectionDetail>,
    },
}

impl TrustedLuaStandardAcceptance {
    pub(crate) fn accepted(translation: TextUnitContent, changed_locations: usize) -> Self {
        Self::Accepted {
            translation,
            changed_locations,
        }
    }

    pub(crate) fn rejected(
        reason: impl Into<String>,
        details: Vec<TrustedLuaStandardRejectionDetail>,
    ) -> Self {
        Self::Rejected {
            reason: reason.into(),
            details,
        }
    }
}

/// 一个稳定命名的候选拒绝详情字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaStandardRejectionDetail {
    name: String,
    value: TrustedLuaStandardRejectionValue,
}

impl TrustedLuaStandardRejectionDetail {
    pub(crate) fn new(name: impl Into<String>, value: TrustedLuaStandardRejectionValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &TrustedLuaStandardRejectionValue {
        &self.value
    }
}

/// Lua 拒绝结果可以无损表达的安全标量。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaStandardRejectionValue {
    String(String),
    Integer(usize),
    Boolean(bool),
}

/// WriteBack 候选目录中的一个直接子项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaOutputEntry {
    name: String,
    kind: TrustedLuaOutputEntryKind,
}

impl TrustedLuaOutputEntry {
    pub(crate) fn new(name: String, kind: TrustedLuaOutputEntryKind) -> Self {
        Self { name, kind }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn kind(&self) -> TrustedLuaOutputEntryKind {
        self.kind
    }
}

/// WriteBack 候选目录项的现实种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaOutputEntryKind {
    File,
    Directory,
}

impl TrustedLuaOutputEntryKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

/// Lua 请求共享布局内核处理的显示区域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaWriteBackLayoutRegion {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

/// Lua 交给共享布局内核的一个原文/当前文本对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaWriteBackLayoutPair {
    original: String,
    translation: Option<String>,
}

impl TrustedLuaWriteBackLayoutPair {
    pub(crate) fn new(original: String, translation: Option<String>) -> Self {
        Self {
            original,
            translation,
        }
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) fn translation(&self) -> Option<&str> {
        self.translation.as_deref()
    }
}

/// 共享布局内核交还给 Lua 的逐项对齐结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaWriteBackLayoutResult {
    status: TrustedLuaWriteBackLayoutStatus,
    texts: Vec<String>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

impl TrustedLuaWriteBackLayoutResult {
    pub(crate) fn new(
        status: TrustedLuaWriteBackLayoutStatus,
        texts: Vec<String>,
        inserted_line_breaks: usize,
        inserted_fullwidth_indents: usize,
    ) -> Self {
        Self {
            status,
            texts,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        }
    }

    pub(crate) const fn status(&self) -> TrustedLuaWriteBackLayoutStatus {
        self.status
    }

    pub(crate) fn texts(&self) -> &[String] {
        &self.texts
    }

    pub(crate) const fn inserted_line_breaks(&self) -> usize {
        self.inserted_line_breaks
    }

    pub(crate) const fn inserted_fullwidth_indents(&self) -> usize {
        self.inserted_fullwidth_indents
    }
}

/// 共享布局内核能否安全应用自动布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaWriteBackLayoutStatus {
    Applied,
    Manual,
}

impl TrustedLuaWriteBackLayoutStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Manual => "manual",
        }
    }
}

/// WriteBack 阶段专属 Host 能力。
///
/// 调用面只持有已经绑定到尚未发布候选物理身份的 scope，不持有、复制或终结 Publisher
/// token。每个异步调用返回时，该次文件操作已经到达明确终态。
pub(crate) trait TrustedLuaWriteBackHostCalls: Send + Sync + 'static {
    fn open_managed(
        &self,
        _name: String,
    ) -> HostFuture<Result<Option<TrustedLuaManagedTranslationCollection>, TrustedLuaHostCallError>>
    {
        Box::pin(async { Err(managed_translations_unavailable("translations.open")) })
    }

    /// 以冻结来源建立的结构化文本引用完整替换候选中的 RPG Maker string Value。
    fn replace_text(
        &self,
        _replacements: Vec<RpgMakerTextReplacement>,
    ) -> HostFuture<Result<(), TrustedLuaHostCallError>> {
        Box::pin(async {
            Err(TrustedLuaHostCallError::new(
                "write_back",
                "unavailable",
                "当前 WriteBack Host 未构造安全结构化文本写回能力",
                None,
                None,
            )
            .with_operation("write_back.replace_text"))
        })
    }

    fn read_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> HostFuture<Result<Vec<u8>, TrustedLuaHostCallError>>;

    fn list_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> HostFuture<Result<Vec<TrustedLuaOutputEntry>, TrustedLuaHostCallError>>;

    fn create_output_directory(
        &self,
        path: ScopedDirectoryPath,
    ) -> HostFuture<Result<(), TrustedLuaHostCallError>>;

    fn write_output(
        &self,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> HostFuture<Result<(), TrustedLuaHostCallError>>;

    fn remove_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> HostFuture<Result<(), TrustedLuaHostCallError>>;

    fn layout(
        &self,
        region: TrustedLuaWriteBackLayoutRegion,
        pairs: Vec<TrustedLuaWriteBackLayoutPair>,
    ) -> Result<TrustedLuaWriteBackLayoutResult, TrustedLuaHostCallError>;
}

/// 全部可信 Lua 调用共享面的明确所有权包装。
pub(crate) struct TrustedLuaCommonBindings {
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
}

impl TrustedLuaCommonBindings {
    pub(crate) fn new(calls: Arc<dyn TrustedLuaCommonHostCalls>) -> Self {
        Self { calls }
    }

    pub(crate) fn calls(&self) -> &Arc<dyn TrustedLuaCommonHostCalls> {
        &self.calls
    }
}

/// 一次 Lua 调用恰好拥有一个阶段能力集。
pub(crate) enum TrustedLuaPhaseBindings {
    Extract(Arc<dyn TrustedLuaExtractHostCalls>),
    Translate(Arc<dyn TrustedLuaTranslateHostCalls>),
    WriteBack(Arc<dyn TrustedLuaWriteBackHostCalls>),
    Project {
        arguments: Vec<String>,
        standard: Arc<dyn TrustedLuaStandardHostCalls>,
        managed: Arc<dyn TrustedLuaManagedEditHostCalls>,
    },
}

impl TrustedLuaPhaseBindings {
    pub(crate) const fn phase(&self) -> LuaPhase {
        match self {
            Self::Extract(_) => LuaPhase::Extract,
            Self::Translate(_) => LuaPhase::Translate,
            Self::WriteBack(_) => LuaPhase::WriteBack,
            Self::Project { .. } => LuaPhase::Project,
        }
    }
}

/// Host 会话的唯一终结权。
///
/// 终结器不可克隆；`finalize(self)` 通过按值消费保证最多执行一次。
pub(crate) trait TrustedLuaBindingFinalizer: Send + 'static {
    fn finalize(
        self: Box<Self>,
    ) -> HostFuture<Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>>;
}

/// 一次 Runtime 启动所需的公共能力、唯一阶段能力与唯一终结器。
#[must_use = "Lua bindings 必须同步移交给 Runtime"]
pub(crate) struct TrustedLuaRuntimeBindings {
    common: TrustedLuaCommonBindings,
    phase: TrustedLuaPhaseBindings,
    finalizer: Box<dyn TrustedLuaBindingFinalizer>,
}

impl TrustedLuaRuntimeBindings {
    pub(crate) fn extract(
        common: TrustedLuaCommonBindings,
        extract: Arc<dyn TrustedLuaExtractHostCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> Self {
        Self {
            common,
            phase: TrustedLuaPhaseBindings::Extract(extract),
            finalizer,
        }
    }

    pub(crate) fn translate(
        common: TrustedLuaCommonBindings,
        translate: Arc<dyn TrustedLuaTranslateHostCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> Self {
        Self {
            common,
            phase: TrustedLuaPhaseBindings::Translate(translate),
            finalizer,
        }
    }

    pub(crate) fn write_back(
        common: TrustedLuaCommonBindings,
        write_back: Arc<dyn TrustedLuaWriteBackHostCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> Self {
        Self {
            common,
            phase: TrustedLuaPhaseBindings::WriteBack(write_back),
            finalizer,
        }
    }

    pub(crate) fn project_with_managed(
        common: TrustedLuaCommonBindings,
        arguments: Vec<String>,
        standard: Arc<dyn TrustedLuaStandardHostCalls>,
        managed: Arc<dyn TrustedLuaManagedEditHostCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> Self {
        Self {
            common,
            phase: TrustedLuaPhaseBindings::Project {
                arguments,
                standard,
                managed,
            },
            finalizer,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TrustedLuaCommonBindings,
        TrustedLuaPhaseBindings,
        Box<dyn TrustedLuaBindingFinalizer>,
    ) {
        (self.common, self.phase, self.finalizer)
    }
}

/// Lua 根执行器自身的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaRuntimeExecutionError<R> {
    Unavailable(R),
    Context(R),
    Compile(R),
    Execute(R),
    Binding(TrustedLuaHostCallError),
    Cancelled,
    WorkerPanicked,
    SupervisorLost,
}

impl<R> fmt::Display for TrustedLuaRuntimeExecutionError<R>
where
    R: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "Lua 执行器不可用：{source}"),
            Self::Context(source) => write!(formatter, "Lua 上下文构造失败：{source}"),
            Self::Compile(source) => write!(formatter, "Lua 主程序编译失败：{source}"),
            Self::Execute(source) => write!(formatter, "Lua 主程序运行失败：{source}"),
            Self::Binding(source) => write!(formatter, "Lua Host 能力调用失败：{source}"),
            Self::Cancelled => formatter.write_str("Lua 主程序已取消"),
            Self::WorkerPanicked => formatter.write_str("Lua worker 意外 panic"),
            Self::SupervisorLost => formatter.write_str("Lua job supervisor 意外退出"),
        }
    }
}

impl<R> Error for TrustedLuaRuntimeExecutionError<R>
where
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(source)
            | Self::Context(source)
            | Self::Compile(source)
            | Self::Execute(source) => Some(source),
            Self::Binding(source) => Some(source),
            Self::Cancelled | Self::WorkerPanicked | Self::SupervisorLost => None,
        }
    }
}

/// VM 执行与 Host 资源收尾的两个独立终态。
pub(crate) struct TrustedLuaRuntimeExecutionReport<R> {
    runtime: Result<(), TrustedLuaRuntimeExecutionError<R>>,
    finalization: Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
}

impl<R> TrustedLuaRuntimeExecutionReport<R> {
    pub(crate) fn new(
        runtime: Result<(), TrustedLuaRuntimeExecutionError<R>>,
        finalization: Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
    ) -> Self {
        Self {
            runtime,
            finalization,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Result<(), TrustedLuaRuntimeExecutionError<R>>,
        Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
    ) {
        (self.runtime, self.finalization)
    }
}

/// `start` 同步移交资源后返回的执行句柄。
///
/// 丢弃句柄只请求合作式取消；job supervisor 继续拥有唯一终结器并完成收尾。
pub(crate) struct TrustedLuaExecutionHandle<R> {
    receiver: oneshot::Receiver<TrustedLuaRuntimeExecutionReport<R>>,
    cancelled: Arc<AtomicBool>,
    completed: bool,
}

impl<R> TrustedLuaExecutionHandle<R> {
    pub(crate) fn new(
        receiver: oneshot::Receiver<TrustedLuaRuntimeExecutionReport<R>>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            receiver,
            cancelled,
            completed: false,
        }
    }
}

impl<R> Future for TrustedLuaExecutionHandle<R> {
    type Output = TrustedLuaRuntimeExecutionReport<R>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(context) {
            Poll::Ready(Ok(report)) => {
                self.completed = true;
                Poll::Ready(report)
            }
            Poll::Ready(Err(_)) => {
                self.completed = true;
                Poll::Ready(TrustedLuaRuntimeExecutionReport::new(
                    Err(TrustedLuaRuntimeExecutionError::SupervisorLost),
                    Err(TrustedLuaBindingFinalizationError::supervisor_lost()),
                ))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R> Drop for TrustedLuaExecutionHandle<R> {
    fn drop(&mut self) {
        if !self.completed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

/// 在一次专用 OS 线程中运行完全可信的 Lua 程序。
///
/// `start` 同步接管 bindings 且不返回移交失败。接管后即使 Runtime 正在关闭、
/// worker 无法创建或执行 panic，也必须最终产生执行与清理报告。
pub(crate) trait TrustedLuaRuntimeExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn start(
        &self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_program_debug_uses_a_compact_source_projection() {
        const SENTINEL: &str = "ATT_LUA_BODY_SENTINEL";
        let program = OwnedLuaProgram::new(
            PathBuf::from("C:/scripts/main.lua"),
            SENTINEL.as_bytes().to_vec(),
        );

        let debug = format!("{program:?}");

        assert!(debug.contains("OwnedLuaProgram"));
        assert!(debug.contains("source_bytes"));
        assert!(
            !debug.contains(SENTINEL),
            "Debug 不应复制 Lua 正文：{debug}"
        );
    }
}
