//! 与游戏引擎无关的项目数据库 Lua Host。
//!
//! 本模块只拥有 Lua VM、脚本自管的 SQLite 事务和通用值协议。项目租约、脚本读取、
//! 日志脱敏以及各引擎译文不变量由调用方和 [`ProjectLuaEngineAdapter`] 负责。

mod binding;
mod generic;
mod rpg_maker;

use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, InterruptHandle};

use self::binding::{PreparedProjectLua, prepare_lua, validate_program};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, LuaCompilationProblem, LuaCompilerCategory, LuaContextProblem,
    LuaEngine, LuaIssue, LuaOperation, LuaProblem, LuaScriptProblem, LuaValueViolation,
    RelatedFailureRelation, SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage,
    SqliteDriverFailure, SqliteIssue, SqliteOperation, SqliteProblem, SqliteTransactionState,
    StateEffect,
};

pub(crate) use self::generic::generic_project_lua_adapter_for_name;
pub(crate) use self::rpg_maker::rpg_maker_project_lua_adapter;

const LUA_CANCEL_CHECK_INSTRUCTIONS: NonZeroU32 =
    NonZeroU32::new(10_000).expect("Lua 取消检查间隔必须非零");
const PROJECT_LUA_SOURCE_CHUNK_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("Project Lua source 分块大小必须非零");
const SQLITE_CANCEL_CHECK_OPERATIONS: i32 = 1_000;
const SQLITE_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 引擎适配器或脚本输出接收器返回的稳定错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaCallError {
    issue: ProjectLuaCallIssue,
    engine: Option<LuaEngine>,
    operation: Option<LuaOperation>,
    field: Option<crate::diagnostic::SafeIdentifier>,
    cleanup_failures: Vec<ProjectLuaCallError>,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectLuaCallIssue {
    Cancelled,
    Violation(LuaValueViolation),
    Sqlite {
        failure: SqliteDriverFailure,
        source: Arc<rusqlite::Error>,
    },
}

impl PartialEq for ProjectLuaCallIssue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cancelled, Self::Cancelled) => true,
            (Self::Violation(left), Self::Violation(right)) => left == right,
            (Self::Sqlite { failure: left, .. }, Self::Sqlite { failure: right, .. }) => {
                left == right
            }
            _ => false,
        }
    }
}

impl Eq for ProjectLuaCallIssue {}

impl ProjectLuaCallError {
    pub(crate) const fn cancelled() -> Self {
        Self {
            issue: ProjectLuaCallIssue::Cancelled,
            engine: None,
            operation: None,
            field: None,
            cleanup_failures: Vec::new(),
        }
    }

    pub(crate) const fn violation(violation: LuaValueViolation) -> Self {
        Self {
            issue: ProjectLuaCallIssue::Violation(violation),
            engine: None,
            operation: None,
            field: None,
            cleanup_failures: Vec::new(),
        }
    }

    pub(crate) fn sqlite(source: rusqlite::Error) -> Self {
        Self {
            issue: ProjectLuaCallIssue::Sqlite {
                failure: SqliteDriverFailure::from_error(&source),
                source: Arc::new(source),
            },
            engine: None,
            operation: None,
            field: None,
            cleanup_failures: Vec::new(),
        }
    }

    fn with_cleanup_failure(mut self, cleanup: ProjectLuaCallError) -> Self {
        self.cleanup_failures.push(cleanup);
        self
    }

    pub(crate) fn with_field(mut self, field: impl AsRef<str>) -> Self {
        self.field = crate::diagnostic::SafeIdentifier::new(field.as_ref()).ok();
        self
    }

    pub(crate) fn with_operation(mut self, operation: LuaOperation) -> Self {
        self.operation = Some(operation);
        self
    }

    pub(crate) fn with_engine(mut self, engine: LuaEngine) -> Self {
        self.engine = Some(engine);
        self
    }

    fn primary_is_cancelled(&self) -> bool {
        match &self.issue {
            ProjectLuaCallIssue::Cancelled => true,
            ProjectLuaCallIssue::Sqlite { source, .. } => {
                source.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
            }
            ProjectLuaCallIssue::Violation(_) => false,
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cleanup_failures.is_empty() && self.primary_is_cancelled()
    }

    pub(crate) fn kind(&self) -> &'static str {
        if !self.cleanup_failures.is_empty() {
            return "cleanup_failed";
        }
        match self.issue {
            ProjectLuaCallIssue::Cancelled => "cancelled",
            ProjectLuaCallIssue::Violation(LuaValueViolation::UnknownUnit) => "unit_not_found",
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTranslation) => {
                "invalid_translation"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTable) => "invalid_table",
            ProjectLuaCallIssue::Violation(LuaValueViolation::CyclicTable) => "cyclic_table",
            ProjectLuaCallIssue::Violation(LuaValueViolation::TransactionLost) => {
                "transaction_open"
            }
            ProjectLuaCallIssue::Violation(_) => "invalid_value",
            ProjectLuaCallIssue::Sqlite { .. } if self.primary_is_cancelled() => "cancelled",
            ProjectLuaCallIssue::Sqlite { .. } => "sqlite",
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        if !self.cleanup_failures.is_empty() {
            return "Lua 调用失败，且保存点清理失败";
        }
        match self.issue {
            ProjectLuaCallIssue::Cancelled => "Lua 调用已取消",
            ProjectLuaCallIssue::Violation(LuaValueViolation::UnknownUnit) => "目标 Unit 不存在",
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTranslation) => {
                "译文不满足项目规则"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTable) => "Lua table 结构无效",
            ProjectLuaCallIssue::Violation(LuaValueViolation::CyclicTable) => {
                "Lua table 存在循环引用"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::TransactionLost) => {
                "Lua 脚本结束时仍有未关闭事务"
            }
            ProjectLuaCallIssue::Violation(_) => "Lua 参数或项目状态无效",
            ProjectLuaCallIssue::Sqlite { .. } if self.primary_is_cancelled() => "Lua 调用已取消",
            ProjectLuaCallIssue::Sqlite { .. } => "SQLite 调用失败",
        }
    }
}

impl fmt::Display for ProjectLuaCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl Error for ProjectLuaCallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.issue {
            ProjectLuaCallIssue::Sqlite { source, .. } => Some(source.as_ref()),
            ProjectLuaCallIssue::Cancelled | ProjectLuaCallIssue::Violation(_) => None,
        }
    }
}

const fn lua_host_operation_name(operation: LuaOperation) -> &'static str {
    match operation {
        LuaOperation::CreateContext => "binding",
        LuaOperation::CompileScript => "compile",
        LuaOperation::ExecuteScript => "execute",
        LuaOperation::SetTranslation => "translation.set",
        LuaOperation::ClearTranslation => "translation.clear",
        LuaOperation::QueryDatabase => "db.query",
        LuaOperation::RollbackTransaction => "transaction.rollback",
        LuaOperation::InstallAuthorizer => "authorizer.install",
        LuaOperation::RemoveAuthorizer => "authorizer.remove",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaTranslationStatus {
    Unfinished,
    Translated,
    NotNeeded,
    Outdated,
}

impl ProjectLuaTranslationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unfinished => "unfinished",
            Self::Translated => "translated",
            Self::NotNeeded => "not_needed",
            Self::Outdated => "outdated",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "unfinished" => Some(Self::Unfinished),
            "translated" => Some(Self::Translated),
            "not_needed" => Some(Self::NotNeeded),
            "outdated" => Some(Self::Outdated),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLuaTranslationFilter {
    pub(crate) status: Option<ProjectLuaTranslationStatus>,
    pub(crate) ids: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaOutdatedTranslation {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) source: Vec<String>,
    pub(crate) translation: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaTranslationRecord {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) source: Vec<String>,
    pub(crate) translation: Option<Vec<String>>,
    pub(crate) status: ProjectLuaTranslationStatus,
    pub(crate) origin: Option<String>,
    pub(crate) outdated_manual: Option<ProjectLuaOutdatedTranslation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaTranslationContext {
    pub(crate) id: String,
    pub(crate) speaker: Option<String>,
    pub(crate) translations: Vec<ProjectLuaTranslationRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaTerminologyEntry {
    pub(crate) term: String,
    pub(crate) translation: String,
}

/// 引擎为 Lua 提供的可读翻译 API。
///
/// 所有定位都使用 Manual 服务建立的当前可读 ID。原始 SQLite API 不经过本 trait，
/// 可以任意修改或破坏数据库。
pub(crate) trait ProjectLuaEngineAdapter: Send + Sync + 'static {
    fn list_translations(
        &self,
        connection: &Connection,
        filter: ProjectLuaTranslationFilter,
    ) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError>;

    fn translation_context(
        &self,
        connection: &Connection,
        ids: Vec<String>,
    ) -> Result<Vec<ProjectLuaTranslationContext>, ProjectLuaCallError>;

    fn set_translation(
        &self,
        connection: &Connection,
        id: String,
        translation: Vec<String>,
        cancellation: &ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError>;

    fn clear_translation(
        &self,
        connection: &Connection,
        id: String,
        cancellation: &ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError>;

    fn list_terminology(
        &self,
        connection: &Connection,
    ) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError>;
}

/// 接收脚本 `print` 的原始字节。
///
/// 实现必须在写入用户日志前按运行根的敏感信息规则处理内容。
pub(crate) trait ProjectLuaPrintSink: Send + Sync + 'static {
    fn print(&self, bytes: &[u8]) -> Result<(), ProjectLuaCallError>;
}

#[derive(Debug)]
struct IgnoreProjectLuaPrint;

impl ProjectLuaPrintSink for IgnoreProjectLuaPrint {
    fn print(&self, _bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        Ok(())
    }
}

/// Lua 可见的项目事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaEngine {
    Generic,
    Mv,
    Mz,
}

impl ProjectLuaEngine {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Mv => "mv",
            Self::Mz => "mz",
        }
    }

    const fn diagnostic(self) -> LuaEngine {
        match self {
            Self::Generic => LuaEngine::Generic,
            Self::Mv => LuaEngine::Mv,
            Self::Mz => LuaEngine::Mz,
        }
    }
}

impl From<crate::rpg_maker::RpgMakerEngine> for ProjectLuaEngine {
    fn from(value: crate::rpg_maker::RpgMakerEngine) -> Self {
        match value {
            crate::rpg_maker::RpgMakerEngine::Mv => Self::Mv,
            crate::rpg_maker::RpgMakerEngine::Mz => Self::Mz,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaProject {
    name: String,
    engine: ProjectLuaEngine,
}

impl ProjectLuaProject {
    pub(crate) fn new(name: impl Into<String>, engine: ProjectLuaEngine) -> Self {
        Self {
            name: name.into(),
            engine,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn engine(&self) -> &'static str {
        self.engine.as_str()
    }

    const fn diagnostic_engine(&self) -> LuaEngine {
        self.engine.diagnostic()
    }
}

/// 已由 Rust 读取的单次脚本。
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaProgram {
    identity: String,
    source: Vec<u8>,
    arguments: Vec<String>,
}

impl ProjectLuaProgram {
    pub(crate) fn new(
        identity: impl Into<String>,
        source: impl Into<Vec<u8>>,
        arguments: Vec<String>,
    ) -> Self {
        Self {
            identity: identity.into(),
            source: source.into(),
            arguments,
        }
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    pub(crate) fn source(&self) -> &[u8] {
        &self.source
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

/// 一次 Lua 执行的完整输入。
pub(crate) struct ProjectLuaRunRequest {
    project: ProjectLuaProject,
    program: ProjectLuaProgram,
    adapter: Arc<dyn ProjectLuaEngineAdapter>,
    cancellation: ProjectLuaCancellation,
    print_sink: Arc<dyn ProjectLuaPrintSink>,
}

/// 在取得项目租约和打开数据库前完成脚本 UTF-8 与 Lua 语法检查。
///
/// 实际执行仍会在同样受限的 VM 中重新编译脚本；本预检只保证语法错误不会占用项目
/// 租约，也不会开始数据库事务。
pub(crate) fn compile_project_lua_program_with_cancellation(
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    validate_program(program, cancellation)
}

impl ProjectLuaRunRequest {
    pub(crate) fn new(
        project: ProjectLuaProject,
        program: ProjectLuaProgram,
        adapter: Arc<dyn ProjectLuaEngineAdapter>,
    ) -> Self {
        Self {
            project,
            program,
            adapter,
            cancellation: ProjectLuaCancellation::default(),
            print_sink: Arc::new(IgnoreProjectLuaPrint),
        }
    }

    pub(crate) fn with_cancellation(mut self, cancellation: ProjectLuaCancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn with_print_sink(mut self, print_sink: Arc<dyn ProjectLuaPrintSink>) -> Self {
        self.print_sink = print_sink;
        self
    }
}

#[derive(Default)]
struct ProjectLuaCancellationState {
    requested: AtomicBool,
    interrupt: Mutex<Option<InterruptHandle>>,
}

/// 可由命令取消监督者跨线程触发的一次 Lua 取消令牌。
#[derive(Clone, Default)]
pub(crate) struct ProjectLuaCancellation {
    state: Arc<ProjectLuaCancellationState>,
}

impl fmt::Debug for ProjectLuaCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectLuaCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl ProjectLuaCancellation {
    pub(crate) fn cancel(&self) {
        self.state.requested.store(true, Ordering::Release);
        let interrupt = self
            .state
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(interrupt) = interrupt.as_ref() {
            interrupt.interrupt();
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }

    pub(crate) fn ensure_running(&self) -> Result<(), ProjectLuaCallError> {
        if self.is_cancelled() {
            Err(ProjectLuaCallError::cancelled())
        } else {
            Ok(())
        }
    }

    fn register_interrupt(&self, interrupt: InterruptHandle) -> Result<(), ProjectLuaFailure> {
        let mut active = self
            .state
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(ProjectLuaFailure::Context(
                ProjectLuaContextFailure::InterruptRegistration,
            ));
        }
        *active = Some(interrupt);
        if self.is_cancelled()
            && let Some(interrupt) = active.as_ref()
        {
            interrupt.interrupt();
        }
        Ok(())
    }

    fn unregister_interrupt(&self) {
        let mut active = self
            .state
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = None;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectLuaSqliteError {
    operation: ProjectLuaSqliteOperation,
    failure: SqliteDriverFailure,
    source: Arc<rusqlite::Error>,
}

impl PartialEq for ProjectLuaSqliteError {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation && self.failure == other.failure
    }
}

impl Eq for ProjectLuaSqliteError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaSqliteOperation {
    InstallAuthorizer,
    RemoveAuthorizer,
    Rollback,
    InstallBusyHandler,
    InstallCancellation,
}

impl ProjectLuaSqliteOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InstallAuthorizer => "install_authorizer",
            Self::RemoveAuthorizer => "remove_authorizer",
            Self::Rollback => "rollback",
            Self::InstallBusyHandler => "install_sqlite_busy_handler",
            Self::InstallCancellation => "install_sqlite_cancellation",
        }
    }
}

impl AsRef<str> for ProjectLuaSqliteOperation {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ProjectLuaSqliteOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl ProjectLuaSqliteError {
    pub(crate) fn new(operation: ProjectLuaSqliteOperation, source: rusqlite::Error) -> Self {
        Self {
            operation,
            failure: SqliteDriverFailure::from_error(&source),
            source: Arc::new(source),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.source.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
    }
}

impl fmt::Display for ProjectLuaSqliteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} 失败", self.operation.as_str())
    }
}

impl Error for ProjectLuaSqliteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Lua VM、脚本或数据库调用的主失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaFailure {
    Cancelled,
    Context(ProjectLuaContextFailure),
    Compile {
        script_identity: String,
        failure: ProjectLuaCompilationFailure,
    },
    Script {
        script_identity: String,
        failure: ProjectLuaScriptFailure,
    },
    Host(ProjectLuaCallError),
    Database(ProjectLuaSqliteError),
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaContextFailure {
    InterruptRegistration,
    CancellationGuard,
    InstructionHook,
    ContextTable,
    PublishContext,
    Arguments,
    PrintBinding,
    RuntimeCreation,
    RemoveExternalCapability,
    ProtectedCallWrapper,
    ThreadCreation,
    ConcurrentSqliteWait,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaCompilationFailure {
    InvalidIdentity,
    InvalidUtf8,
    Backend {
        category: LuaCompilerCategory,
        line: Option<usize>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaScriptFailure {
    Yielded,
    Backend(LuaCompilerCategory),
    NonErrorValue,
}

impl fmt::Display for ProjectLuaFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Lua 执行已取消"),
            Self::Context(failure) => write!(formatter, "无法建立 Lua 执行环境：{failure:?}"),
            Self::Compile { failure, .. } => write!(formatter, "Lua 脚本编译失败：{failure:?}"),
            Self::Script { failure, .. } => write!(formatter, "Lua 脚本运行失败：{failure:?}"),
            Self::Host(source) => write!(formatter, "Lua Host 调用失败：{source}"),
            Self::Database(error) => error.fmt(formatter),
            Self::Panicked => formatter.write_str("Lua worker panic"),
        }
    }
}

impl Error for ProjectLuaFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Host(source) => Some(source),
            Self::Database(source) => Some(source),
            Self::Cancelled
            | Self::Context(_)
            | Self::Compile { .. }
            | Self::Script { .. }
            | Self::Panicked => None,
        }
    }
}

impl ProjectLuaFailure {
    fn into_typed_cancellation(self) -> Self {
        match &self {
            Self::Host(source) if source.is_cancelled() => Self::Cancelled,
            Self::Database(source) if source.is_cancelled() => Self::Cancelled,
            _ => self,
        }
    }

    /// 脚本预检尚未建立写事务；数据库类错误仍使用调用方提供的真实项目路径。
    pub(crate) fn preflight_diagnostic_report(
        &self,
        database_path: &std::path::Path,
    ) -> DiagnosticReport {
        failure_report(
            self,
            database_path,
            StateEffect::Unchanged,
            SqliteTransactionState::NotStarted,
        )
    }
}

/// Lua 执行失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaRunError {
    /// 脚本尚未开始执行，因此数据库没有变化。
    NotStarted(ProjectLuaFailure),
    /// 脚本失败时没有打开的事务；此前已经提交的修改保留。
    Failed(ProjectLuaFailure),
    /// 脚本失败时打开的事务已经确认回滚；此前已经提交的修改保留。
    RolledBack(ProjectLuaFailure),
    /// 主操作失败，且无法确认回滚是否完成。
    RollbackOutcomeUnknown {
        failure: ProjectLuaFailure,
        rollback: ProjectLuaSqliteError,
    },
}

impl fmt::Display for ProjectLuaRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted(failure) => write!(formatter, "Lua 未开始修改数据库：{failure}"),
            Self::Failed(failure) => write!(formatter, "Lua 执行失败：{failure}"),
            Self::RolledBack(failure) => write!(formatter, "Lua 修改已回滚：{failure}"),
            Self::RollbackOutcomeUnknown { failure, rollback } => write!(
                formatter,
                "Lua 失败且回滚结果未知；主失败：{failure}；回滚失败：{rollback}"
            ),
        }
    }
}

impl Error for ProjectLuaRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotStarted(failure) | Self::Failed(failure) | Self::RolledBack(failure) => {
                Some(failure)
            }
            Self::RollbackOutcomeUnknown { failure, .. } => Some(failure),
        }
    }
}

impl ProjectLuaRunError {
    pub(crate) fn diagnostic_report(&self, database_path: &std::path::Path) -> DiagnosticReport {
        match self {
            Self::NotStarted(failure) => failure_report(
                failure,
                database_path,
                StateEffect::Unchanged,
                SqliteTransactionState::NotStarted,
            ),
            Self::Failed(failure) => failure_report(
                failure,
                database_path,
                StateEffect::ProgressPreserved,
                SqliteTransactionState::NotStarted,
            ),
            Self::RolledBack(failure) => failure_report(
                failure,
                database_path,
                StateEffect::ProgressPreserved,
                SqliteTransactionState::RolledBack,
            ),
            Self::RollbackOutcomeUnknown { failure, rollback } => failure_report(
                failure,
                database_path,
                StateEffect::OutcomeUnknown,
                SqliteTransactionState::OutcomeUnknown,
            )
            .with_related(
                RelatedFailureRelation::Rollback,
                sqlite_report(
                    rollback,
                    database_path,
                    StateEffect::OutcomeUnknown,
                    SqliteTransactionState::OutcomeUnknown,
                ),
            ),
        }
    }
}

fn failure_report(
    failure: &ProjectLuaFailure,
    database_path: &std::path::Path,
    effect: StateEffect,
    transaction: SqliteTransactionState,
) -> DiagnosticReport {
    match failure {
        ProjectLuaFailure::Database(source) => {
            sqlite_report(source, database_path, effect, transaction)
        }
        ProjectLuaFailure::Cancelled => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::Cancelled)),
        ),
        ProjectLuaFailure::Context(problem) => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::ContextCreation {
                problem: context_problem(*problem),
            })),
        ),
        ProjectLuaFailure::Compile {
            script_identity,
            failure,
        } => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::Compilation {
                script: SafePath::new(script_identity),
                problem: compilation_problem(*failure),
                line: compilation_line(*failure),
            })),
        ),
        ProjectLuaFailure::Script {
            script_identity,
            failure,
        } => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::ScriptExecution {
                script: SafePath::new(script_identity),
                problem: script_problem(*failure),
            })),
        ),
        ProjectLuaFailure::Host(source) => {
            host_failure_report(source, database_path, effect, transaction)
        }
        ProjectLuaFailure::Panicked => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::WorkerPanicked)),
        ),
    }
}

fn host_failure_report(
    source: &ProjectLuaCallError,
    database_path: &std::path::Path,
    effect: StateEffect,
    transaction: SqliteTransactionState,
) -> DiagnosticReport {
    let mut report = match &source.issue {
        ProjectLuaCallIssue::Sqlite { failure, .. } => DiagnosticReport::new(
            effect,
            Diagnostic::sqlite(SqliteIssue::new(
                SqliteDiagnosticContext::new(
                    SqliteDiagnosticStage::Lua,
                    source
                        .operation
                        .map_or(SqliteOperation::Execute, lua_sqlite_operation),
                    transaction,
                ),
                SqliteProblem::Driver {
                    database: SafePath::new(database_path),
                    query_id: None,
                    query_ordinal: None,
                    failure: failure.clone(),
                },
            )),
        ),
        ProjectLuaCallIssue::Cancelled => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::Cancelled)),
        ),
        ProjectLuaCallIssue::Violation(violation) => match (source.engine, source.operation) {
            (Some(engine), Some(operation)) => DiagnosticReport::new(
                effect,
                Diagnostic::lua(LuaIssue::new(LuaProblem::HostCall {
                    engine,
                    operation,
                    violation: *violation,
                    field: source.field.clone(),
                    placeholder: None,
                })),
            ),
            _ => DiagnosticReport::new(
                effect,
                Diagnostic::lua(LuaIssue::new(LuaProblem::ContextCreation {
                    problem: LuaContextProblem::ContextTable,
                })),
            ),
        },
    };
    for cleanup in &source.cleanup_failures {
        report = report.with_related(
            RelatedFailureRelation::Cleanup,
            host_failure_report(cleanup, database_path, effect, transaction),
        );
    }
    report
}

fn sqlite_report(
    source: &ProjectLuaSqliteError,
    database_path: &std::path::Path,
    effect: StateEffect,
    transaction: SqliteTransactionState,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::sqlite(SqliteIssue::new(
            SqliteDiagnosticContext::new(
                SqliteDiagnosticStage::Lua,
                source.operation.sqlite_operation(),
                transaction,
            ),
            SqliteProblem::Driver {
                database: SafePath::new(database_path),
                query_id: None,
                query_ordinal: None,
                failure: source.failure.clone(),
            },
        )),
    )
}

const fn context_problem(failure: ProjectLuaContextFailure) -> LuaContextProblem {
    match failure {
        ProjectLuaContextFailure::InterruptRegistration => LuaContextProblem::InterruptRegistration,
        ProjectLuaContextFailure::CancellationGuard => LuaContextProblem::CancellationGuard,
        ProjectLuaContextFailure::InstructionHook => LuaContextProblem::InstructionHook,
        ProjectLuaContextFailure::ContextTable => LuaContextProblem::ContextTable,
        ProjectLuaContextFailure::PublishContext => LuaContextProblem::PublishContext,
        ProjectLuaContextFailure::Arguments => LuaContextProblem::Arguments,
        ProjectLuaContextFailure::PrintBinding => LuaContextProblem::PrintBinding,
        ProjectLuaContextFailure::RuntimeCreation => LuaContextProblem::RuntimeCreation,
        ProjectLuaContextFailure::RemoveExternalCapability => {
            LuaContextProblem::RemoveExternalCapability
        }
        ProjectLuaContextFailure::ProtectedCallWrapper => LuaContextProblem::ProtectedCallWrapper,
        ProjectLuaContextFailure::ThreadCreation => LuaContextProblem::ThreadCreation,
        ProjectLuaContextFailure::ConcurrentSqliteWait => LuaContextProblem::ConcurrentSqliteWait,
    }
}

const fn compilation_problem(failure: ProjectLuaCompilationFailure) -> LuaCompilationProblem {
    match failure {
        ProjectLuaCompilationFailure::InvalidIdentity => LuaCompilationProblem::InvalidIdentity,
        ProjectLuaCompilationFailure::InvalidUtf8 => LuaCompilationProblem::InvalidUtf8,
        ProjectLuaCompilationFailure::Backend { category, .. } => {
            LuaCompilationProblem::Backend { category }
        }
    }
}

const fn compilation_line(failure: ProjectLuaCompilationFailure) -> Option<usize> {
    match failure {
        ProjectLuaCompilationFailure::Backend { line, .. } => line,
        ProjectLuaCompilationFailure::InvalidIdentity
        | ProjectLuaCompilationFailure::InvalidUtf8 => None,
    }
}

const fn script_problem(failure: ProjectLuaScriptFailure) -> LuaScriptProblem {
    match failure {
        ProjectLuaScriptFailure::Yielded => LuaScriptProblem::Yielded,
        ProjectLuaScriptFailure::NonErrorValue => LuaScriptProblem::NonErrorValue,
        ProjectLuaScriptFailure::Backend(category) => LuaScriptProblem::Backend { category },
    }
}

fn project_lua_engine(project: &ProjectLuaProject) -> crate::diagnostic::LuaEngine {
    project.diagnostic_engine()
}

impl ProjectLuaSqliteOperation {
    const fn sqlite_operation(self) -> SqliteOperation {
        match self {
            Self::Rollback => SqliteOperation::Transaction,
            Self::InstallAuthorizer
            | Self::RemoveAuthorizer
            | Self::InstallBusyHandler
            | Self::InstallCancellation => SqliteOperation::Execute,
        }
    }
}

const fn lua_sqlite_operation(operation: LuaOperation) -> SqliteOperation {
    match operation {
        LuaOperation::QueryDatabase => SqliteOperation::Query,
        LuaOperation::RollbackTransaction => SqliteOperation::Transaction,
        LuaOperation::SetTranslation
        | LuaOperation::ClearTranslation
        | LuaOperation::InstallAuthorizer
        | LuaOperation::RemoveAuthorizer
        | LuaOperation::CreateContext
        | LuaOperation::CompileScript
        | LuaOperation::ExecuteScript => SqliteOperation::Execute,
    }
}

fn rollback_translation_api_savepoint(
    connection: &Connection,
    engine: LuaEngine,
    failure: ProjectLuaCallError,
) -> ProjectLuaCallError {
    if let Err(source) = connection.execute_batch("ROLLBACK TO att_translation_api") {
        return failure.with_cleanup_failure(
            ProjectLuaCallError::sqlite(source)
                .with_engine(engine)
                .with_operation(LuaOperation::RollbackTransaction),
        );
    }
    if let Err(source) = connection.execute_batch("RELEASE att_translation_api") {
        return failure.with_cleanup_failure(
            ProjectLuaCallError::sqlite(source)
                .with_engine(engine)
                .with_operation(LuaOperation::RollbackTransaction),
        );
    }
    failure
}

/// 在调用方已经打开的项目数据库上运行脚本。
///
/// 调用方应在进入本函数前取得项目排他租约，并在阻塞 worker 中调用。连接被本函数
/// 消费，确保所有 Lua callback 和脚本事务只属于这一条连接。
pub(crate) fn run_project_lua(
    connection: Connection,
    request: ProjectLuaRunRequest,
) -> Result<(), ProjectLuaRunError> {
    if request.cancellation.is_cancelled() {
        return Err(ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled));
    }

    let prepared = prepare_lua(connection, &request, LUA_CANCEL_CHECK_INSTRUCTIONS)
        .map_err(|failure| ProjectLuaRunError::NotStarted(failure.into_typed_cancellation()))?;
    execute_prepared(prepared, request)
}

fn execute_prepared(
    prepared: PreparedProjectLua,
    request: ProjectLuaRunRequest,
) -> Result<(), ProjectLuaRunError> {
    let PreparedProjectLua {
        lua,
        function,
        connection,
        script_identity,
        hook_cancelled,
    } = prepared;

    request
        .cancellation
        .register_interrupt(connection.borrow().get_interrupt_handle())
        .map_err(|failure| ProjectLuaRunError::NotStarted(failure.into_typed_cancellation()))?;
    let _sqlite_cancellation_guard =
        install_sqlite_cancellation(Rc::clone(&connection), request.cancellation.clone()).map_err(
            |failure| {
                request.cancellation.unregister_interrupt();
                ProjectLuaRunError::NotStarted(failure.into_typed_cancellation())
            },
        )?;

    if let Err(source) = install_script_authorizer(&connection.borrow()) {
        return rollback_after_failure(
            &connection,
            &request.cancellation,
            ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
                ProjectLuaSqliteOperation::InstallAuthorizer,
                source,
            )),
        );
    }

    let execution = catch_unwind(AssertUnwindSafe(|| {
        binding::execute(
            &lua,
            function,
            &script_identity,
            project_lua_engine(&request.project),
            &hook_cancelled,
        )
    }));
    let failure = match execution {
        Ok(Ok(())) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Ok(())) => None,
        Ok(Err(failure)) => Some(failure),
        Err(_) => Some(ProjectLuaFailure::Panicked),
    };
    if let Some(failure) = failure {
        return rollback_after_failure(&connection, &request.cancellation, failure);
    }

    if !connection.borrow().is_autocommit() {
        return rollback_after_failure(
            &connection,
            &request.cancellation,
            ProjectLuaFailure::Host(
                ProjectLuaCallError::violation(LuaValueViolation::TransactionLost)
                    .with_engine(project_lua_engine(&request.project))
                    .with_operation(LuaOperation::RollbackTransaction),
            ),
        );
    }

    if let Err(source) = disable_script_authorizer(&connection.borrow()) {
        return rollback_after_failure(
            &connection,
            &request.cancellation,
            ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
                ProjectLuaSqliteOperation::RemoveAuthorizer,
                source,
            )),
        );
    }

    disable_sqlite_cancellation(&connection.borrow());
    request.cancellation.unregister_interrupt();
    Ok(())
}

fn rollback_after_failure(
    connection: &Rc<RefCell<Connection>>,
    cancellation: &ProjectLuaCancellation,
    failure: ProjectLuaFailure,
) -> Result<(), ProjectLuaRunError> {
    let _ = disable_script_authorizer(&connection.borrow());
    disable_sqlite_cancellation(&connection.borrow());
    cancellation.unregister_interrupt();
    rollback(connection, failure.into_typed_cancellation())
}

fn rollback(
    connection: &Rc<RefCell<Connection>>,
    failure: ProjectLuaFailure,
) -> Result<(), ProjectLuaRunError> {
    if connection.borrow().is_autocommit() {
        return Err(ProjectLuaRunError::Failed(failure));
    }
    match connection.borrow().execute_batch("ROLLBACK") {
        Ok(()) => Err(ProjectLuaRunError::RolledBack(failure)),
        Err(source) => Err(ProjectLuaRunError::RollbackOutcomeUnknown {
            failure,
            rollback: ProjectLuaSqliteError::new(ProjectLuaSqliteOperation::Rollback, source),
        }),
    }
}

#[derive(Clone)]
enum ProjectLuaBusyWaitMode {
    Cancellable(ProjectLuaCancellation),
    Finalizing,
}

thread_local! {
    static PROJECT_LUA_BUSY_WAIT_MODE: RefCell<Option<ProjectLuaBusyWaitMode>> =
        const { RefCell::new(None) };
}

struct ProjectLuaSqliteCancellationGuard {
    connection: Rc<RefCell<Connection>>,
    cancellation: ProjectLuaCancellation,
}

impl Drop for ProjectLuaSqliteCancellationGuard {
    fn drop(&mut self) {
        if let Ok(connection) = self.connection.try_borrow() {
            let _ = connection.progress_handler(0, None::<fn() -> bool>);
            let _ = connection.busy_handler(None);
        }
        self.cancellation.unregister_interrupt();
        PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| {
            *mode.borrow_mut() = None;
        });
    }
}

fn install_sqlite_cancellation(
    connection: Rc<RefCell<Connection>>,
    cancellation: ProjectLuaCancellation,
) -> Result<ProjectLuaSqliteCancellationGuard, ProjectLuaFailure> {
    let installed = PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| {
        let mut mode = mode.borrow_mut();
        if mode.is_some() {
            false
        } else {
            *mode = Some(ProjectLuaBusyWaitMode::Cancellable(cancellation.clone()));
            true
        }
    });
    if !installed {
        return Err(ProjectLuaFailure::Context(
            ProjectLuaContextFailure::ConcurrentSqliteWait,
        ));
    }

    if let Err(source) = connection
        .borrow()
        .busy_handler(Some(wait_for_project_lua_sqlite_unlock))
    {
        PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| {
            *mode.borrow_mut() = None;
        });
        return Err(ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::InstallBusyHandler,
            source,
        )));
    }

    if let Err(source) = connection.borrow().progress_handler(
        SQLITE_CANCEL_CHECK_OPERATIONS,
        Some({
            let cancellation = cancellation.clone();
            move || cancellation.is_cancelled()
        }),
    ) {
        let _ = connection.borrow().busy_handler(None);
        PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| {
            *mode.borrow_mut() = None;
        });
        return Err(ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::InstallCancellation,
            source,
        )));
    }

    Ok(ProjectLuaSqliteCancellationGuard {
        connection,
        cancellation,
    })
}

fn wait_for_project_lua_sqlite_unlock(_attempt: i32) -> bool {
    match PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| mode.borrow().clone()) {
        Some(ProjectLuaBusyWaitMode::Cancellable(cancellation)) => {
            if cancellation.is_cancelled() {
                return false;
            }
            std::thread::sleep(SQLITE_WAIT_POLL_INTERVAL);
            !cancellation.is_cancelled()
        }
        Some(ProjectLuaBusyWaitMode::Finalizing) => {
            std::thread::sleep(SQLITE_WAIT_POLL_INTERVAL);
            true
        }
        None => false,
    }
}

fn disable_sqlite_cancellation(connection: &Connection) {
    let _ = connection.progress_handler(0, None::<fn() -> bool>);
    PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| {
        *mode.borrow_mut() = Some(ProjectLuaBusyWaitMode::Finalizing);
    });
}

fn install_script_authorizer(connection: &Connection) -> rusqlite::Result<()> {
    connection.authorizer(Some(authorize_script_statement))
}

fn disable_script_authorizer(connection: &Connection) -> rusqlite::Result<()> {
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
}

fn authorize_script_statement(context: AuthContext<'_>) -> Authorization {
    use AuthAction::{Attach, Detach, Function};

    let denied = match context.action {
        Attach { .. } | Detach { .. } => true,
        Function { function_name } => function_name.eq_ignore_ascii_case("load_extension"),
        _ => false,
    };
    if denied {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

#[cfg(test)]
mod tests;
