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

use crate::fingerprint::Sha256Fingerprint;
use crate::llm::{ChatMessage, LlmResponse};
use crate::rpg_maker::extract::store::LuaSnapshot;
use crate::storage::file_system::ScopedDirectoryPath;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

use super::{LuaPhase, LuaProjectContext, LuaSourcePath};
use crate::rpg_maker::text::TextGroupKind;

type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// 已完整读取并可交给专用 Lua worker 的主程序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnedLuaProgram {
    main_script_path: PathBuf,
    source: Vec<u8>,
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

/// Lua 能通过 `pcall` 检查的 Host 错误事实。
///
/// `domain` 与 `kind` 是稳定的机器字段；`message` 只用于人类诊断。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaHostCallError {
    domain: &'static str,
    kind: &'static str,
    message: String,
    retry_after_ms: Option<u64>,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaHostCallError {
    pub(crate) fn new(
        domain: &'static str,
        kind: &'static str,
        message: impl Into<String>,
        retry_after_ms: Option<u64>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            domain,
            kind,
            message: message.into(),
            retry_after_ms,
            source,
        }
    }

    pub(crate) const fn domain(&self) -> &'static str {
        self.domain
    }

    pub(crate) const fn kind(&self) -> &'static str {
        self.kind
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }
}

impl fmt::Display for TrustedLuaHostCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TrustedLuaHostCallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// 唯一终结器失败。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaBindingFinalizationError {
    message: String,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaBindingFinalizationError {
    pub(crate) fn new(
        message: impl Into<String>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            message: message.into(),
            source,
        }
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

/// Lua 三阶段共同拥有的冻结来源、项目与数据库 Host 能力。
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
}

/// Lua Extract 主程序在内存中声明的唯一标准快照意图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaExtractIntent {
    /// 以完整快照收敛 Lua owner；空快照仍表示 active。
    Replace(LuaSnapshot),
    /// 停用 Lua owner 并清除其标准资产。
    Deactivate,
}

/// Extract 阶段专属 Host 能力。
///
/// Runtime 只把已校验的完整意图记录到内存，不在 VM 生命周期内写标准资产表。
pub(crate) trait TrustedLuaExtractHostCalls: Send + Sync + 'static {
    fn replace_standard(&self, snapshot: LuaSnapshot) -> Result<(), TrustedLuaHostCallError>;

    fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError>;
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

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> HostFuture<Result<LlmResponse, TrustedLuaHostCallError>>;
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
}

/// Translate 共享语义对一段文本完成预处理后的稳定状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaPreparedTranslationStatus {
    Active,
    NonSourceLanguage,
    FullyProtected,
}

impl TrustedLuaPreparedTranslationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NonSourceLanguage => "non_source_language",
            Self::FullyProtected => "fully_protected",
        }
    }
}

/// 当前单段文本实际命中的一个有序术语对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaTranslationTerm {
    term: String,
    translation: String,
}

impl TrustedLuaTranslationTerm {
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

/// `PreparedText:accept` 的正常内容验收结果。
///
/// 内容不合格是 Lua 能继续处理的普通结果；只有共享语义本身无法执行时才返回
/// `TrustedLuaHostCallError`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaPreparedTranslationAcceptance {
    Accepted {
        translation: String,
        state: Sha256Fingerprint,
    },
    Rejected {
        reason: String,
    },
}

impl TrustedLuaPreparedTranslationAcceptance {
    pub(crate) fn accepted(translation: impl Into<String>, state: Sha256Fingerprint) -> Self {
        Self::Accepted {
            translation: translation.into(),
            state,
        }
    }

    pub(crate) fn rejected(reason: impl Into<String>) -> Self {
        Self::Rejected {
            reason: reason.into(),
        }
    }
}

/// 一个由 Translate 共享语义建立、不可由 Lua 伪造的预处理句柄。
pub(crate) trait TrustedLuaPreparedTranslation: Send + Sync + 'static {
    fn status(&self) -> TrustedLuaPreparedTranslationStatus;
    fn model_text(&self) -> &str;
    fn terms(&self) -> &[TrustedLuaTranslationTerm];

    /// 只比较脚本持久化的译文与 opaque state 是否仍等于当前语义。
    ///
    /// `state` 已由 Lua 边界验证为当前协议的 SHA-256 文本；这里不得重新执行
    /// `accept`，避免旧译文因新的正规化实现被反向改写或误判。
    fn is_current(
        &self,
        translation: String,
        state: Sha256Fingerprint,
    ) -> Result<bool, TrustedLuaHostCallError>;

    fn accept(
        &self,
        candidate: String,
    ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError>;
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

/// 三阶段共享调用面的明确所有权包装。
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
}

impl TrustedLuaPhaseBindings {
    pub(crate) const fn phase(&self) -> LuaPhase {
        match self {
            Self::Extract(_) => LuaPhase::Extract,
            Self::Translate(_) => LuaPhase::Translate,
            Self::WriteBack(_) => LuaPhase::WriteBack,
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
