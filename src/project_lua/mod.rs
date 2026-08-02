//! 与游戏引擎无关的原子数据库 Lua Host。
//!
//! 本模块只拥有 Lua VM、SQLite 外层事务和通用值协议。项目租约、脚本读取、日志
//! 脱敏以及各引擎译文不变量由调用方和 [`ProjectLuaEngineAdapter`] 负责。

mod binding;
mod generic;
mod rpg_maker;

use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::mem;
use std::num::{NonZeroU32, NonZeroUsize};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, ErrorCode, InterruptHandle};
use sha2::{Digest, Sha256};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, LuaCompilationProblem, LuaCompilerCategory,
    LuaContextProblem, LuaEngine, LuaIssue, LuaLocator, LuaOperation, LuaProblem, LuaScriptProblem,
    LuaValidationProblem, LuaValueViolation, RelatedFailureRelation, RuntimeComponent,
    RuntimeIssue, RuntimeOperation, SafeIdentifier, SafePath, SqliteDiagnosticContext,
    SqliteDiagnosticStage, SqliteDriverFailure, SqliteIssue, SqliteOperation, SqliteProblem,
    SqliteTransactionState, StateEffect,
};
use crate::fingerprint::Sha256Fingerprint;

use self::binding::{BindingMetrics, PreparedProjectLua, prepare_lua, validate_program};

pub(crate) use self::generic::generic_project_lua_adapter;
pub(crate) use self::rpg_maker::rpg_maker_project_lua_adapter;

const LUA_CANCEL_CHECK_INSTRUCTIONS: NonZeroU32 =
    NonZeroU32::new(10_000).expect("Lua 取消检查间隔必须非零");
const PROJECT_LUA_SOURCE_CHUNK_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("Project Lua source 分块大小必须非零");
const SQLITE_CANCEL_CHECK_OPERATIONS: i32 = 1_000;
const SQLITE_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 交给引擎适配器的 Lua 值。
///
/// 该类型只用于 `ctx.translation` 边界；`ctx.db` 直接使用 SQLite 的五种存储类型。
#[derive(Debug, PartialEq)]
pub(crate) enum ProjectLuaValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl ProjectLuaValue {
    pub(crate) fn into_text(mut self) -> Option<String> {
        match &mut self {
            Self::Text(value) => Some(mem::take(value)),
            _ => None,
        }
    }

    pub(crate) fn into_array(mut self) -> Option<Vec<Self>> {
        match &mut self {
            Self::Array(values) => Some(mem::take(values)),
            _ => None,
        }
    }

    pub(crate) fn into_object(mut self) -> Option<Vec<(String, Self)>> {
        match &mut self {
            Self::Object(fields) => Some(mem::take(fields)),
            _ => None,
        }
    }
}

impl Drop for ProjectLuaValue {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        take_project_lua_value_children(self, &mut pending);
        while let Some(mut value) = pending.pop() {
            take_project_lua_value_children(&mut value, &mut pending);
        }
    }
}

fn take_project_lua_value_children(
    value: &mut ProjectLuaValue,
    pending: &mut Vec<ProjectLuaValue>,
) {
    match value {
        ProjectLuaValue::Array(values) => pending.append(values),
        ProjectLuaValue::Object(fields) => {
            for (_, value) in mem::take(fields) {
                pending.push(value);
            }
        }
        _ => {}
    }
}

fn project_lua_field_name_eq(candidate: &str, expected: &str) -> bool {
    candidate.len() == expected.len() && candidate.as_bytes() == expected.as_bytes()
}

pub(crate) fn project_lua_object_contains_field(
    fields: &[(String, ProjectLuaValue)],
    name: &str,
) -> bool {
    fields
        .iter()
        .any(|(candidate, _)| project_lua_field_name_eq(candidate, name))
}

pub(crate) fn take_project_lua_object_field(
    fields: &mut Vec<(String, ProjectLuaValue)>,
    name: &str,
) -> Option<ProjectLuaValue> {
    let index = fields
        .iter()
        .position(|(candidate, _)| project_lua_field_name_eq(candidate, name))?;
    Some(fields.swap_remove(index).1)
}

/// 引擎适配器或脚本输出接收器返回的稳定错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaCallError {
    issue: ProjectLuaCallIssue,
    engine: Option<LuaEngine>,
    operation: Option<LuaOperation>,
    field: Option<crate::diagnostic::SafeIdentifier>,
    locator: Option<LuaLocator>,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectLuaCallIssue {
    Cancelled,
    Violation(LuaValueViolation),
    Sqlite {
        failure: SqliteDriverFailure,
        source: Arc<rusqlite::Error>,
    },
    WorkerSpawn {
        failure: IoFailure,
        source: Arc<std::io::Error>,
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
            (Self::WorkerSpawn { failure: left, .. }, Self::WorkerSpawn { failure: right, .. }) => {
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
            locator: None,
        }
    }

    pub(crate) const fn violation(violation: LuaValueViolation) -> Self {
        Self {
            issue: ProjectLuaCallIssue::Violation(violation),
            engine: None,
            operation: None,
            field: None,
            locator: None,
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
            locator: None,
        }
    }

    pub(crate) fn worker_spawn(source: std::io::Error) -> Self {
        Self {
            issue: ProjectLuaCallIssue::WorkerSpawn {
                failure: IoFailure::from_error(&source),
                source: Arc::new(source),
            },
            engine: None,
            operation: None,
            field: None,
            locator: None,
        }
    }

    pub(crate) fn with_field(mut self, field: impl AsRef<str>) -> Self {
        self.field = crate::diagnostic::SafeIdentifier::new(field.as_ref()).ok();
        self
    }

    pub(crate) fn with_generic_locator(
        mut self,
        relative_path: Option<&std::path::Path>,
        group_id: &str,
        unit_id: &str,
    ) -> Self {
        self.locator = Some(LuaLocator::Generic {
            relative_path: relative_path.map(SafePath::new),
            group_id: crate::diagnostic::SafeIdentifier::new(group_id).ok(),
            unit_id: crate::diagnostic::SafeIdentifier::new(unit_id).ok(),
        });
        self
    }

    pub(crate) fn with_rpg_maker_locator(
        mut self,
        owner: &str,
        group_location: &str,
        unit_role: &str,
    ) -> Self {
        self.locator = Some(LuaLocator::RpgMaker {
            owner: crate::diagnostic::SafeIdentifier::new(owner).ok(),
            group_location: crate::diagnostic::SafeIdentifier::new(group_location).ok(),
            unit_role: crate::diagnostic::SafeIdentifier::new(unit_role).ok(),
        });
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

    pub(crate) const fn kind(&self) -> &'static str {
        match self.issue {
            ProjectLuaCallIssue::Cancelled => "cancelled",
            ProjectLuaCallIssue::Violation(LuaValueViolation::UnknownUnit) => "unit_not_found",
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidLocator) => "invalid_locator",
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTranslation) => {
                "invalid_translation"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTable) => "invalid_table",
            ProjectLuaCallIssue::Violation(LuaValueViolation::CyclicTable) => "cyclic_table",
            ProjectLuaCallIssue::Violation(LuaValueViolation::TransactionLost) => {
                "transaction_lost"
            }
            ProjectLuaCallIssue::Violation(_) => "invalid_value",
            ProjectLuaCallIssue::Sqlite { .. } => "sqlite",
            ProjectLuaCallIssue::WorkerSpawn { .. } => "worker_spawn",
        }
    }

    pub(crate) const fn message(&self) -> &'static str {
        match self.issue {
            ProjectLuaCallIssue::Cancelled => "Lua 调用已取消",
            ProjectLuaCallIssue::Violation(LuaValueViolation::UnknownUnit) => "目标 Unit 不存在",
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidLocator) => {
                "Unit locator 无效"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTranslation) => {
                "译文不满足项目规则"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::InvalidTable) => "Lua table 结构无效",
            ProjectLuaCallIssue::Violation(LuaValueViolation::CyclicTable) => {
                "Lua table 存在循环引用"
            }
            ProjectLuaCallIssue::Violation(LuaValueViolation::TransactionLost) => {
                "Lua 脚本已失去外层事务"
            }
            ProjectLuaCallIssue::Violation(_) => "Lua 参数或项目状态无效",
            ProjectLuaCallIssue::Sqlite { .. } => "SQLite 调用失败",
            ProjectLuaCallIssue::WorkerSpawn { .. } => "无法启动 Lua 辅助 worker",
        }
    }

    #[cfg(test)]
    pub(crate) const fn operation(&self) -> Option<&'static str> {
        match self.operation {
            Some(operation) => Some(lua_host_operation_name(operation)),
            None => None,
        }
    }

    pub(crate) const fn is_worker_spawn(&self) -> bool {
        matches!(self.issue, ProjectLuaCallIssue::WorkerSpawn { .. })
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
            ProjectLuaCallIssue::WorkerSpawn { source, .. } => Some(source.as_ref()),
            ProjectLuaCallIssue::Cancelled | ProjectLuaCallIssue::Violation(_) => None,
        }
    }
}

const fn lua_host_operation_name(operation: LuaOperation) -> &'static str {
    match operation {
        LuaOperation::CreateContext => "binding",
        LuaOperation::CompileScript => "compile",
        LuaOperation::ExecuteScript => "execute",
        LuaOperation::ValidatePrerequisites => "translation.capture",
        LuaOperation::SetTranslation => "translation.set",
        LuaOperation::ClearTranslation => "translation.clear",
        LuaOperation::QueryDatabase => "db.query",
        LuaOperation::ValidateDatabase => "translation.validate",
        LuaOperation::BeginTransaction => "transaction.begin",
        LuaOperation::CommitTransaction => "transaction.commit",
        LuaOperation::RollbackTransaction => "transaction.rollback",
        LuaOperation::InstallAuthorizer => "authorizer.install",
        LuaOperation::RemoveAuthorizer => "authorizer.remove",
    }
}

/// 引擎在脚本执行前校验数据库时能够返回的失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaDatabasePrerequisiteError {
    Cancelled,
    InvalidProjectState {
        engine: crate::diagnostic::LuaEngine,
        violation: crate::diagnostic::LuaValueViolation,
    },
    Sqlite(ProjectLuaSqliteError),
}

impl ProjectLuaDatabasePrerequisiteError {
    pub(crate) const fn invalid_project_state(
        engine: crate::diagnostic::LuaEngine,
        violation: crate::diagnostic::LuaValueViolation,
    ) -> Self {
        Self::InvalidProjectState { engine, violation }
    }

    pub(crate) fn sqlite(operation: ProjectLuaSqliteOperation, source: rusqlite::Error) -> Self {
        Self::Sqlite(ProjectLuaSqliteError::new(operation, source))
    }
}

/// SQLite schema 对象类别。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ProjectLuaSchemaObjectKind {
    Table,
    Index,
    View,
    Trigger,
}

impl ProjectLuaSchemaObjectKind {
    fn from_sqlite(value: &str) -> Option<Self> {
        match value {
            "table" => Some(Self::Table),
            "index" => Some(Self::Index),
            "view" => Some(Self::View),
            "trigger" => Some(Self::Trigger),
            _ => None,
        }
    }
}

/// 引擎在同一个外层事务中提供的译文操作和最终数据库校验。
///
/// `protects_schema_object` 只能检查传入身份，不得访问 SQLite 或执行其他副作用，因为
/// SQLite 会在准备 statement 的 authorizer 回调中调用它。
pub(crate) trait ProjectLuaEngineAdapter: Send + Sync + 'static {
    fn protects_schema_object(
        &self,
        kind: ProjectLuaSchemaObjectKind,
        name: &str,
        table_name: &str,
    ) -> bool;

    fn set_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
        translation: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError>;

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError>;

    /// 在脚本获得任何修改数据库的机会前校验当前引擎要求的数据库前置条件。
    ///
    /// 默认实现适用于只需要提交前校验的引擎。实现不得修改数据库。
    fn validate_database_before_script(
        &self,
        _connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaDatabasePrerequisiteError> {
        Ok(())
    }

    /// 在脚本执行前捕获最终校验需要的引擎事实。
    ///
    /// 适配器实例只服务一次 Lua 执行。默认实现适用于不需要比较脚本前后状态的引擎。
    fn capture_database_state(
        &self,
        _connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        Ok(())
    }

    /// 检查当前引擎的 metadata、表关系和译文不变量。
    ///
    /// Host 会在调用本方法前检查受保护 schema，在调用后检查 foreign keys 和数据库
    /// 物理结构；适配器无需重复这些通用检查。
    fn validate_database(
        &self,
        connection: &Connection,
        project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError>;
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLuaProject {
    name: String,
    engine: String,
}

impl ProjectLuaProject {
    pub(crate) fn new(name: impl Into<String>, engine: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            engine: engine.into(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn engine(&self) -> &str {
        &self.engine
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

/// 一次原子 Lua 执行的完整输入。
pub(crate) struct ProjectLuaRunRequest {
    project: ProjectLuaProject,
    program: ProjectLuaProgram,
    adapter: Arc<dyn ProjectLuaEngineAdapter>,
    cancellation: ProjectLuaCancellation,
    print_sink: Arc<dyn ProjectLuaPrintSink>,
    metrics: Arc<BindingMetrics>,
}

/// 在取得项目租约和打开数据库前完成脚本 UTF-8 与 Lua 语法检查。
///
/// 实际执行仍会在同样受限的 VM 中重新编译脚本；本预检只保证语法错误不会占用项目
/// 租约，也不会开始数据库事务。
#[cfg(test)]
pub(crate) fn compile_project_lua_program(
    program: &ProjectLuaProgram,
) -> Result<(), ProjectLuaFailure> {
    compile_project_lua_program_with_cancellation(program, &ProjectLuaCancellation::default())
}

pub(crate) fn compile_project_lua_program_with_cancellation(
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    validate_program(program, cancellation)
}

pub(crate) fn fingerprint_project_lua_program_with_cancellation(
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<Sha256Fingerprint, ProjectLuaFailure> {
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    let mut hasher = Sha256::new();
    for chunk in program
        .source()
        .chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get())
    {
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        hasher.update(chunk);
    }
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    Ok(Sha256Fingerprint::from_bytes(hasher.finalize().into()))
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
            metrics: Arc::new(BindingMetrics::default()),
        }
    }

    pub(crate) fn metrics(&self) -> ProjectLuaRunMetrics {
        ProjectLuaRunMetrics {
            metrics: Arc::clone(&self.metrics),
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

/// 不包含 SQL、参数、查询结果或游戏正文的执行计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLuaRunReport {
    database_calls: u64,
    changed_rows: u64,
    translation_calls: u64,
    printed_lines: u64,
}

/// 允许命令层在执行失败或取消后读取已经发生的 Lua Host 计数。
#[derive(Clone, Debug)]
pub(crate) struct ProjectLuaRunMetrics {
    metrics: Arc<BindingMetrics>,
}

impl ProjectLuaRunMetrics {
    pub(crate) fn report(&self) -> ProjectLuaRunReport {
        self.metrics.report()
    }
}

impl ProjectLuaRunReport {
    pub(crate) const fn database_calls(self) -> u64 {
        self.database_calls
    }

    pub(crate) const fn changed_rows(self) -> u64 {
        self.changed_rows
    }

    pub(crate) const fn translation_calls(self) -> u64 {
        self.translation_calls
    }

    pub(crate) const fn printed_lines(self) -> u64 {
        self.printed_lines
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
    EnableForeignKeys,
    BeginImmediate,
    InstallAuthorizer,
    RemoveAuthorizer,
    Commit,
    Rollback,
    ReadCurrentAttSchema,
    ReadProtectedSchema,
    QuickCheck,
    ReadTempSchema,
    InstallBusyHandler,
    InstallCancellation,
    ForeignKeyCheck,
}

impl ProjectLuaSqliteOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EnableForeignKeys => "enable_foreign_keys",
            Self::BeginImmediate => "begin_immediate",
            Self::InstallAuthorizer => "install_authorizer",
            Self::RemoveAuthorizer => "remove_authorizer",
            Self::Commit => "commit",
            Self::Rollback => "rollback",
            Self::ReadCurrentAttSchema => "read_current_att_schema",
            Self::ReadProtectedSchema => "read_protected_schema",
            Self::QuickCheck => "quick_check",
            Self::ReadTempSchema => "read_temp_schema",
            Self::InstallBusyHandler => "install_sqlite_busy_handler",
            Self::InstallCancellation => "install_sqlite_cancellation",
            Self::ForeignKeyCheck => "foreign_key_check",
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

    #[cfg(test)]
    pub(crate) const fn operation(&self) -> ProjectLuaSqliteOperation {
        self.operation
    }

    #[cfg(test)]
    pub(crate) const fn sqlite_codes(&self) -> Option<(i32, i32)> {
        match (self.failure.primary_code, self.failure.extended_code) {
            (Some(primary), Some(extended)) => Some((primary, extended)),
            _ => None,
        }
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

impl fmt::Display for ProjectLuaDatabasePrerequisiteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Lua 数据库前置检查已取消"),
            Self::InvalidProjectState { .. } => formatter.write_str("Lua 数据库前置状态无效"),
            Self::Sqlite(source) => source.fmt(formatter),
        }
    }
}

impl Error for ProjectLuaDatabasePrerequisiteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(source) => Some(source),
            Self::Cancelled | Self::InvalidProjectState { .. } => None,
        }
    }
}

/// 已知没有提交时的主失败。
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
    DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError),
    Host(ProjectLuaCallError),
    Database(ProjectLuaSqliteError),
    Validation {
        engine: crate::diagnostic::LuaEngine,
        failure: ProjectLuaValidationFailure,
    },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaValidationFailure {
    #[cfg(test)]
    AdapterState,
    ForeignKey,
    QuickCheck,
    ProtectedSchemaChanged,
    TemporarySchemaObject,
}

impl fmt::Display for ProjectLuaFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Lua 执行已取消"),
            Self::Context(failure) => write!(formatter, "无法建立 Lua 执行环境：{failure:?}"),
            Self::Compile { failure, .. } => write!(formatter, "Lua 脚本编译失败：{failure:?}"),
            Self::Script { failure, .. } => write!(formatter, "Lua 脚本运行失败：{failure:?}"),
            Self::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Cancelled) => {
                formatter.write_str("Lua 执行已取消")
            }
            Self::DatabasePrerequisite(
                ProjectLuaDatabasePrerequisiteError::InvalidProjectState { .. },
            ) => formatter.write_str("Lua 执行前项目数据库状态无效"),
            Self::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Sqlite(error)) => {
                write!(formatter, "Lua 执行前数据库检查失败：{error}")
            }
            Self::Host(source) => write!(formatter, "Lua Host 调用失败：{source}"),
            Self::Database(error) => error.fmt(formatter),
            Self::Validation { failure, .. } => {
                write!(formatter, "Lua 提交前数据库校验失败：{failure:?}")
            }
            Self::Panicked => formatter.write_str("Lua worker panic"),
        }
    }
}

impl Error for ProjectLuaFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DatabasePrerequisite(source) => Some(source),
            Self::Host(source) => Some(source),
            Self::Database(source) => Some(source),
            Self::Cancelled
            | Self::Context(_)
            | Self::Compile { .. }
            | Self::Script { .. }
            | Self::Validation { .. }
            | Self::Panicked => None,
        }
    }
}

impl ProjectLuaFailure {
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

/// 原子 Lua 执行失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLuaRunError {
    /// 尚未开始写事务。
    NotStarted(ProjectLuaFailure),
    /// 外层事务已经确认回滚。
    RolledBack(ProjectLuaFailure),
    /// 主操作失败，且无法确认回滚是否完成。
    RollbackOutcomeUnknown {
        failure: ProjectLuaFailure,
        rollback: ProjectLuaSqliteError,
    },
    /// COMMIT 已经开始，SQLite 最终状态无法从当前连接确认。
    CommitOutcomeUnknown(ProjectLuaSqliteError),
}

impl fmt::Display for ProjectLuaRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted(failure) => write!(formatter, "Lua 未开始修改数据库：{failure}"),
            Self::RolledBack(failure) => write!(formatter, "Lua 修改已回滚：{failure}"),
            Self::RollbackOutcomeUnknown { failure, rollback } => write!(
                formatter,
                "Lua 失败且回滚结果未知；主失败：{failure}；回滚失败：{rollback}"
            ),
            Self::CommitOutcomeUnknown(error) => {
                write!(formatter, "Lua COMMIT 结果未知：{error}")
            }
        }
    }
}

impl Error for ProjectLuaRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotStarted(failure) | Self::RolledBack(failure) => Some(failure),
            Self::RollbackOutcomeUnknown { failure, .. } => Some(failure),
            Self::CommitOutcomeUnknown(source) => Some(source),
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
            Self::RolledBack(failure) => failure_report(
                failure,
                database_path,
                StateEffect::Unchanged,
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
            Self::CommitOutcomeUnknown(source) => sqlite_report(
                source,
                database_path,
                StateEffect::OutcomeUnknown,
                SqliteTransactionState::OutcomeUnknown,
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
        ProjectLuaFailure::Database(source)
        | ProjectLuaFailure::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Sqlite(
            source,
        )) => sqlite_report(source, database_path, effect, transaction),
        ProjectLuaFailure::Cancelled
        | ProjectLuaFailure::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Cancelled) => {
            DiagnosticReport::new(
                effect,
                Diagnostic::lua(LuaIssue::new(LuaProblem::Cancelled)),
            )
        }
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
        ProjectLuaFailure::DatabasePrerequisite(
            ProjectLuaDatabasePrerequisiteError::InvalidProjectState { engine, violation },
        ) => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::DatabasePrerequisite {
                engine: Some(*engine),
                violation: *violation,
            })),
        ),
        ProjectLuaFailure::Validation { engine, failure } => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::Validation {
                engine: *engine,
                problem: validation_problem(*failure),
            })),
        ),
        ProjectLuaFailure::Host(source) => match &source.issue {
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
                        query_id: source.operation.map(|operation| {
                            SafeIdentifier::from_validated(lua_host_operation_name(operation))
                        }),
                        query_ordinal: None,
                        failure: failure.clone(),
                    },
                )),
            ),
            ProjectLuaCallIssue::WorkerSpawn { failure, .. } => DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::CpuExecutor,
                    operation: RuntimeOperation::StartWorker,
                    failure: failure.clone(),
                }),
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
                        locator: source.locator.clone(),
                    })),
                ),
                _ => DiagnosticReport::new(
                    effect,
                    Diagnostic::lua(LuaIssue::new(LuaProblem::ContextCreation {
                        problem: LuaContextProblem::ContextTable,
                    })),
                ),
            },
        },
        ProjectLuaFailure::Panicked => DiagnosticReport::new(
            effect,
            Diagnostic::lua(LuaIssue::new(LuaProblem::WorkerPanicked)),
        ),
    }
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
                query_id: Some(SafeIdentifier::from_validated(source.operation.as_str())),
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

const fn validation_problem(failure: ProjectLuaValidationFailure) -> LuaValidationProblem {
    match failure {
        #[cfg(test)]
        ProjectLuaValidationFailure::AdapterState => LuaValidationProblem::AdapterState,
        ProjectLuaValidationFailure::ProtectedSchemaChanged => {
            LuaValidationProblem::ProtectedSchemaChanged
        }
        ProjectLuaValidationFailure::TemporarySchemaObject => {
            LuaValidationProblem::TemporarySchemaObject
        }
        ProjectLuaValidationFailure::ForeignKey => LuaValidationProblem::ForeignKey,
        ProjectLuaValidationFailure::QuickCheck => LuaValidationProblem::QuickCheck,
    }
}

fn project_lua_engine(project: &ProjectLuaProject) -> crate::diagnostic::LuaEngine {
    match project.engine() {
        "rpg_maker" => crate::diagnostic::LuaEngine::RpgMaker,
        _ => crate::diagnostic::LuaEngine::Generic,
    }
}

impl ProjectLuaSqliteOperation {
    const fn sqlite_operation(self) -> SqliteOperation {
        match self {
            Self::Commit | Self::Rollback | Self::BeginImmediate => SqliteOperation::Transaction,
            Self::ReadCurrentAttSchema
            | Self::ReadProtectedSchema
            | Self::QuickCheck
            | Self::ReadTempSchema => SqliteOperation::Query,
            Self::EnableForeignKeys
            | Self::InstallAuthorizer
            | Self::RemoveAuthorizer
            | Self::InstallBusyHandler
            | Self::InstallCancellation
            | Self::ForeignKeyCheck => SqliteOperation::Execute,
        }
    }
}

const fn lua_sqlite_operation(operation: LuaOperation) -> SqliteOperation {
    match operation {
        LuaOperation::QueryDatabase => SqliteOperation::Query,
        LuaOperation::BeginTransaction
        | LuaOperation::CommitTransaction
        | LuaOperation::RollbackTransaction => SqliteOperation::Transaction,
        LuaOperation::SetTranslation
        | LuaOperation::ClearTranslation
        | LuaOperation::ValidateDatabase
        | LuaOperation::ValidatePrerequisites
        | LuaOperation::InstallAuthorizer
        | LuaOperation::RemoveAuthorizer
        | LuaOperation::CreateContext
        | LuaOperation::CompileScript
        | LuaOperation::ExecuteScript => SqliteOperation::Execute,
    }
}

/// 在调用方已经打开的现存项目数据库上运行一个原子脚本。
///
/// 调用方应在进入本函数前取得项目排他租约，并在阻塞 worker 中调用。连接被本函数
/// 消费，确保所有 Lua callback、校验和事务终态只属于这一条连接。
pub(crate) fn run_project_lua(
    connection: Connection,
    request: ProjectLuaRunRequest,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    if request.cancellation.is_cancelled() {
        return Err(ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled));
    }

    let prepared = prepare_lua(connection, &request, LUA_CANCEL_CHECK_INSTRUCTIONS)
        .map_err(ProjectLuaRunError::NotStarted)?;

    execute_prepared(prepared, request)
}

fn execute_prepared(
    prepared: PreparedProjectLua,
    request: ProjectLuaRunRequest,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    let PreparedProjectLua {
        lua,
        function,
        connection,
        metrics,
        transaction_guard,
        script_identity,
    } = prepared;

    request
        .cancellation
        .register_interrupt(connection.borrow().get_interrupt_handle())
        .map_err(ProjectLuaRunError::NotStarted)?;

    if let Err(source) = connection
        .borrow()
        .execute_batch("PRAGMA foreign_keys = ON")
    {
        request.cancellation.unregister_interrupt();
        return Err(ProjectLuaRunError::NotStarted(ProjectLuaFailure::Database(
            ProjectLuaSqliteError::new(ProjectLuaSqliteOperation::EnableForeignKeys, source),
        )));
    }

    let _sqlite_cancellation_guard =
        install_sqlite_cancellation(Rc::clone(&connection), request.cancellation.clone()).map_err(
            |failure| {
                request.cancellation.unregister_interrupt();
                ProjectLuaRunError::NotStarted(failure)
            },
        )?;

    if let Err(source) = connection.borrow().execute_batch("BEGIN IMMEDIATE") {
        disable_sqlite_cancellation(&connection.borrow());
        request.cancellation.unregister_interrupt();
        let failure = if request.cancellation.is_cancelled()
            || matches!(
                source.sqlite_error_code(),
                Some(ErrorCode::OperationInterrupted)
            ) {
            ProjectLuaFailure::Cancelled
        } else {
            ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
                ProjectLuaSqliteOperation::BeginImmediate,
                source,
            ))
        };
        return Err(ProjectLuaRunError::NotStarted(failure));
    }

    let initial_validation = catch_unwind(AssertUnwindSafe(|| {
        request
            .adapter
            .validate_database_before_script(&connection.borrow(), &request.project)
    }));
    let initial_validation_failure = match initial_validation {
        Ok(Ok(())) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Ok(())) => None,
        Ok(Err(_)) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Err(ProjectLuaDatabasePrerequisiteError::Cancelled)) => {
            Some(ProjectLuaFailure::Cancelled)
        }
        Ok(Err(error)) => Some(ProjectLuaFailure::DatabasePrerequisite(error)),
        Err(_) => Some(ProjectLuaFailure::Panicked),
    };
    if let Some(failure) = initial_validation_failure {
        return rollback_after_failure(&connection, &request.cancellation, failure);
    }

    let schema_snapshot = catch_unwind(AssertUnwindSafe(|| {
        snapshot_protected_schema(
            &connection.borrow(),
            request.adapter.as_ref(),
            &request.cancellation,
        )
    }));
    let protected_schema = match schema_snapshot {
        Ok(Ok(_)) if request.cancellation.is_cancelled() => {
            return rollback_after_failure(
                &connection,
                &request.cancellation,
                ProjectLuaFailure::Cancelled,
            );
        }
        Ok(Ok(schema)) => schema,
        Ok(Err(failure)) => {
            return rollback_after_failure(
                &connection,
                &request.cancellation,
                if request.cancellation.is_cancelled() {
                    ProjectLuaFailure::Cancelled
                } else {
                    failure
                },
            );
        }
        Err(_) => {
            return rollback_after_failure(
                &connection,
                &request.cancellation,
                ProjectLuaFailure::Panicked,
            );
        }
    };

    let capture = catch_unwind(AssertUnwindSafe(|| {
        request
            .adapter
            .capture_database_state(&connection.borrow(), &request.project)
    }));
    let capture_failure = match capture {
        Ok(Ok(())) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Ok(())) => None,
        Ok(Err(_)) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Err(error)) if error.kind() == "cancelled" => Some(ProjectLuaFailure::Cancelled),
        Ok(Err(error)) => Some(ProjectLuaFailure::Host(
            error
                .with_engine(project_lua_engine(&request.project))
                .with_operation(LuaOperation::ValidatePrerequisites),
        )),
        Err(_) => Some(ProjectLuaFailure::Panicked),
    };
    if let Some(failure) = capture_failure {
        return rollback_after_failure(&connection, &request.cancellation, failure);
    }

    if let Err(source) =
        install_script_authorizer(&connection.borrow(), Arc::clone(&request.adapter))
    {
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
            &request.cancellation,
        )
    }));
    let failure = match execution {
        Ok(Ok(())) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Ok(())) => None,
        Ok(Err(failure)) => Some(if request.cancellation.is_cancelled() {
            ProjectLuaFailure::Cancelled
        } else {
            failure
        }),
        Err(_) => Some(ProjectLuaFailure::Panicked),
    };
    if transaction_guard.is_lost() || connection.borrow().is_autocommit() {
        return finish_confirmed_rollback(
            &connection,
            &request.cancellation,
            ProjectLuaFailure::Host(
                ProjectLuaCallError::violation(LuaValueViolation::TransactionLost)
                    .with_engine(project_lua_engine(&request.project))
                    .with_operation(LuaOperation::RollbackTransaction),
            ),
        );
    }
    if let Some(failure) = failure {
        return rollback_after_failure(&connection, &request.cancellation, failure);
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

    let validation = catch_unwind(AssertUnwindSafe(|| {
        validate_before_commit(
            &connection.borrow(),
            request.adapter.as_ref(),
            &request.project,
            &protected_schema,
            &request.cancellation,
        )
    }));
    let validation_failure = match validation {
        Ok(Ok(())) if request.cancellation.is_cancelled() => Some(ProjectLuaFailure::Cancelled),
        Ok(Ok(())) => None,
        Ok(Err(failure)) => Some(if request.cancellation.is_cancelled() {
            ProjectLuaFailure::Cancelled
        } else {
            failure
        }),
        Err(_) => Some(ProjectLuaFailure::Panicked),
    };
    if let Some(failure) = validation_failure {
        return rollback_after_failure(&connection, &request.cancellation, failure);
    }

    // COMMIT 一旦开始便不再允许取消中断；调用方必须等待 SQLite 返回明确终态。
    disable_sqlite_cancellation(&connection.borrow());
    request.cancellation.unregister_interrupt();
    let commit = connection.borrow().execute_batch("COMMIT");
    match commit {
        Ok(()) => Ok(metrics.report()),
        Err(source) if connection.borrow().is_autocommit() => {
            Err(ProjectLuaRunError::CommitOutcomeUnknown(
                ProjectLuaSqliteError::new(ProjectLuaSqliteOperation::Commit, source),
            ))
        }
        Err(source) => rollback_after_commit_failure(
            &connection,
            ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
                ProjectLuaSqliteOperation::Commit,
                source,
            )),
        ),
    }
}

fn rollback_after_failure(
    connection: &std::rc::Rc<std::cell::RefCell<Connection>>,
    cancellation: &ProjectLuaCancellation,
    failure: ProjectLuaFailure,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    let _ = disable_script_authorizer(&connection.borrow());
    disable_sqlite_cancellation(&connection.borrow());
    cancellation.unregister_interrupt();
    rollback(connection, failure)
}

fn finish_confirmed_rollback(
    connection: &std::rc::Rc<std::cell::RefCell<Connection>>,
    cancellation: &ProjectLuaCancellation,
    failure: ProjectLuaFailure,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    let _ = disable_script_authorizer(&connection.borrow());
    disable_sqlite_cancellation(&connection.borrow());
    cancellation.unregister_interrupt();
    Err(ProjectLuaRunError::RolledBack(failure))
}

fn rollback_after_commit_failure(
    connection: &std::rc::Rc<std::cell::RefCell<Connection>>,
    failure: ProjectLuaFailure,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    rollback(connection, failure)
}

fn rollback(
    connection: &std::rc::Rc<std::cell::RefCell<Connection>>,
    failure: ProjectLuaFailure,
) -> Result<ProjectLuaRunReport, ProjectLuaRunError> {
    if connection.borrow().is_autocommit() {
        return Err(ProjectLuaRunError::RolledBack(failure));
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

#[cfg(test)]
static PROJECT_LUA_FINALIZING_BUSY_TEST_SIGNAL: Mutex<Option<std::sync::mpsc::SyncSender<()>>> =
    Mutex::new(None);

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
    let mode = PROJECT_LUA_BUSY_WAIT_MODE.with(|mode| mode.borrow().clone());
    match mode {
        Some(ProjectLuaBusyWaitMode::Cancellable(cancellation)) => {
            if cancellation.is_cancelled() {
                return false;
            }
            std::thread::sleep(SQLITE_WAIT_POLL_INTERVAL);
            !cancellation.is_cancelled()
        }
        Some(ProjectLuaBusyWaitMode::Finalizing) => {
            #[cfg(test)]
            if let Some(sender) = PROJECT_LUA_FINALIZING_BUSY_TEST_SIGNAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = sender.try_send(());
            }
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

fn install_script_authorizer(
    connection: &Connection,
    adapter: Arc<dyn ProjectLuaEngineAdapter>,
) -> rusqlite::Result<()> {
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        authorize_script_statement(context, adapter.as_ref())
    }))
}

fn disable_script_authorizer(connection: &Connection) -> rusqlite::Result<()> {
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
}

fn authorize_script_statement(
    context: AuthContext<'_>,
    adapter: &dyn ProjectLuaEngineAdapter,
) -> Authorization {
    use AuthAction::{
        AlterTable, Attach, CreateIndex, CreateTable, CreateTempIndex, CreateTempTable,
        CreateTempTrigger, CreateTempView, CreateTrigger, CreateView, CreateVtable, Detach,
        DropIndex, DropTable, DropTempIndex, DropTempTable, DropTempTrigger, DropTempView,
        DropTrigger, DropView, DropVtable, Function, Pragma, Savepoint, Transaction,
    };

    let protected_table = |table_name: &str| {
        adapter.protects_schema_object(ProjectLuaSchemaObjectKind::Table, table_name, table_name)
    };
    let protected_child = |kind, name: &str, table_name: &str| {
        protected_table(table_name) || adapter.protects_schema_object(kind, name, table_name)
    };

    let denied = match context.action {
        Transaction { .. } | Savepoint { .. } | Attach { .. } | Detach { .. } | Pragma { .. } => {
            true
        }
        Function { function_name } => function_name.eq_ignore_ascii_case("load_extension"),
        AlterTable { table_name, .. } => protected_table(table_name),
        CreateTable { table_name }
        | CreateTempTable { table_name }
        | CreateVtable { table_name, .. } => protected_table(table_name),
        CreateIndex {
            index_name,
            table_name,
        }
        | CreateTempIndex {
            index_name,
            table_name,
        } => protected_child(ProjectLuaSchemaObjectKind::Index, index_name, table_name),
        CreateTrigger {
            trigger_name,
            table_name,
        }
        | CreateTempTrigger {
            trigger_name,
            table_name,
        } => protected_child(
            ProjectLuaSchemaObjectKind::Trigger,
            trigger_name,
            table_name,
        ),
        CreateView { view_name } | CreateTempView { view_name } => {
            protected_table(view_name)
                || adapter.protects_schema_object(
                    ProjectLuaSchemaObjectKind::View,
                    view_name,
                    view_name,
                )
        }
        DropTable { table_name } | DropTempTable { table_name } | DropVtable { table_name, .. } => {
            protected_table(table_name)
        }
        DropIndex {
            index_name,
            table_name,
        }
        | DropTempIndex {
            index_name,
            table_name,
        } => protected_child(ProjectLuaSchemaObjectKind::Index, index_name, table_name),
        DropTrigger {
            trigger_name,
            table_name,
        }
        | DropTempTrigger {
            trigger_name,
            table_name,
        } => protected_child(
            ProjectLuaSchemaObjectKind::Trigger,
            trigger_name,
            table_name,
        ),
        DropView { view_name } | DropTempView { view_name } => {
            protected_table(view_name)
                || adapter.protects_schema_object(
                    ProjectLuaSchemaObjectKind::View,
                    view_name,
                    view_name,
                )
        }
        _ => false,
    };

    if denied {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProtectedSchemaEntry {
    kind: ProjectLuaSchemaObjectKind,
    name: String,
    table_name: String,
    sql: Option<String>,
}

fn ensure_project_lua_running(
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    if cancellation.is_cancelled() {
        Err(ProjectLuaFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn sqlite_schema_text_with_cancellation(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
    operation: ProjectLuaSqliteOperation,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    match row
        .get_ref(index)
        .map_err(|source| project_lua_sqlite_failure(operation, source))?
    {
        rusqlite::types::ValueRef::Text(bytes) => {
            clone_sqlite_schema_text_with_cancellation(bytes, index, operation, cancellation)
        }
        value => {
            let source =
                rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type());
            Err(project_lua_sqlite_failure(operation, source))
        }
    }
}

fn sqlite_optional_schema_text_with_cancellation(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
    operation: ProjectLuaSqliteOperation,
    cancellation: &ProjectLuaCancellation,
) -> Result<Option<String>, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    match row
        .get_ref(index)
        .map_err(|source| project_lua_sqlite_failure(operation, source))?
    {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(bytes) => {
            clone_sqlite_schema_text_with_cancellation(bytes, index, operation, cancellation)
                .map(Some)
        }
        value => {
            let source =
                rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type());
            Err(project_lua_sqlite_failure(operation, source))
        }
    }
}

fn clone_sqlite_schema_text_with_cancellation(
    bytes: &[u8],
    index: usize,
    operation: ProjectLuaSqliteOperation,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    let mut text = String::with_capacity(bytes.len());
    let mut pending = Vec::with_capacity(PROJECT_LUA_SOURCE_CHUNK_BYTES.get() + 3);
    for chunk in bytes.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()) {
        ensure_project_lua_running(cancellation)?;
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
            }
            Err(source) if source.error_len().is_none() => {
                let valid_up_to = source.valid_up_to();
                let valid = std::str::from_utf8(&pending[..valid_up_to])
                    .expect("Utf8Error::valid_up_to 指向有效 UTF-8 前缀");
                text.push_str(valid);
                pending.copy_within(valid_up_to.., 0);
                pending.truncate(pending.len() - valid_up_to);
            }
            Err(source) => {
                let source = rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(source),
                );
                return Err(project_lua_sqlite_failure(operation, source));
            }
        }
    }
    if !pending.is_empty() {
        let source = std::str::from_utf8(&pending).expect_err("pending 只保留不完整 UTF-8 后缀");
        let source = rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(source),
        );
        return Err(project_lua_sqlite_failure(operation, source));
    }
    ensure_project_lua_running(cancellation)?;
    Ok(text)
}

fn project_lua_sqlite_failure(
    operation: ProjectLuaSqliteOperation,
    source: rusqlite::Error,
) -> ProjectLuaFailure {
    ProjectLuaFailure::Database(ProjectLuaSqliteError::new(operation, source))
}

fn sqlite_result_with_cancellation<T>(
    result: rusqlite::Result<T>,
    operation: ProjectLuaSqliteOperation,
    cancellation: &ProjectLuaCancellation,
) -> Result<T, ProjectLuaFailure> {
    match result {
        Ok(value) => {
            ensure_project_lua_running(cancellation)?;
            Ok(value)
        }
        Err(source)
            if cancellation.is_cancelled()
                || matches!(
                    source.sqlite_error_code(),
                    Some(ErrorCode::OperationInterrupted)
                ) =>
        {
            Err(ProjectLuaFailure::Cancelled)
        }
        Err(source) => Err(project_lua_sqlite_failure(operation, source)),
    }
}

fn project_lua_text_eq_with_cancellation(
    left: &str,
    right: &str,
    cancellation: &ProjectLuaCancellation,
) -> Result<bool, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get())
        .zip(
            right
                .as_bytes()
                .chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()),
        )
    {
        ensure_project_lua_running(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_project_lua_running(cancellation)?;
    Ok(true)
}

fn protected_schema_eq_with_cancellation(
    left: &[ProtectedSchemaEntry],
    right: &[ProtectedSchemaEntry],
    cancellation: &ProjectLuaCancellation,
) -> Result<bool, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.iter().zip(right) {
        ensure_project_lua_running(cancellation)?;
        if left.kind != right.kind
            || !project_lua_text_eq_with_cancellation(&left.name, &right.name, cancellation)?
            || !project_lua_text_eq_with_cancellation(
                &left.table_name,
                &right.table_name,
                cancellation,
            )?
        {
            return Ok(false);
        }
        match (&left.sql, &right.sql) {
            (None, None) => {}
            (Some(left), Some(right))
                if project_lua_text_eq_with_cancellation(left, right, cancellation)? => {}
            _ => return Ok(false),
        }
    }
    ensure_project_lua_running(cancellation)?;
    Ok(true)
}

fn snapshot_protected_schema(
    connection: &Connection,
    adapter: &dyn ProjectLuaEngineAdapter,
    cancellation: &ProjectLuaCancellation,
) -> Result<Vec<ProtectedSchemaEntry>, ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    let mut statement = sqlite_result_with_cancellation(
        connection.prepare(
            "SELECT type, name, tbl_name, sql
             FROM main.sqlite_schema
             WHERE type IN ('table', 'index', 'view', 'trigger')
             ORDER BY type, name",
        ),
        ProjectLuaSqliteOperation::ReadProtectedSchema,
        cancellation,
    )?;
    let mut rows = sqlite_result_with_cancellation(
        statement.query([]),
        ProjectLuaSqliteOperation::ReadProtectedSchema,
        cancellation,
    )?;
    let mut entries = Vec::new();
    while let Some(row) = sqlite_result_with_cancellation(
        rows.next(),
        ProjectLuaSqliteOperation::ReadProtectedSchema,
        cancellation,
    )? {
        ensure_project_lua_running(cancellation)?;
        let raw_kind = sqlite_schema_text_with_cancellation(
            row,
            0,
            "type",
            ProjectLuaSqliteOperation::ReadProtectedSchema,
            cancellation,
        )?;
        let Some(kind) = ProjectLuaSchemaObjectKind::from_sqlite(&raw_kind) else {
            continue;
        };
        let name = sqlite_schema_text_with_cancellation(
            row,
            1,
            "name",
            ProjectLuaSqliteOperation::ReadProtectedSchema,
            cancellation,
        )?;
        let table_name = sqlite_schema_text_with_cancellation(
            row,
            2,
            "tbl_name",
            ProjectLuaSqliteOperation::ReadProtectedSchema,
            cancellation,
        )?;
        let sql = sqlite_optional_schema_text_with_cancellation(
            row,
            3,
            "sql",
            ProjectLuaSqliteOperation::ReadProtectedSchema,
            cancellation,
        )?;
        ensure_project_lua_running(cancellation)?;
        let protected = adapter.protects_schema_object(kind, &name, &table_name)
            || matches!(
                kind,
                ProjectLuaSchemaObjectKind::Index | ProjectLuaSchemaObjectKind::Trigger
            ) && adapter.protects_schema_object(
                ProjectLuaSchemaObjectKind::Table,
                &table_name,
                &table_name,
            );
        ensure_project_lua_running(cancellation)?;
        if protected {
            entries.push(ProtectedSchemaEntry {
                kind,
                name,
                table_name,
                sql,
            });
        }
    }
    ensure_project_lua_running(cancellation)?;
    Ok(entries)
}

fn validate_before_commit(
    connection: &Connection,
    adapter: &dyn ProjectLuaEngineAdapter,
    project: &ProjectLuaProject,
    protected_schema: &[ProtectedSchemaEntry],
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    let current_schema = snapshot_protected_schema(connection, adapter, cancellation)?;
    if !protected_schema_eq_with_cancellation(&current_schema, protected_schema, cancellation)? {
        return Err(ProjectLuaFailure::Validation {
            engine: project_lua_engine(project),
            failure: ProjectLuaValidationFailure::ProtectedSchemaChanged,
        });
    }
    validate_no_protected_temp_schema(
        connection,
        adapter,
        project_lua_engine(project),
        cancellation,
    )?;

    ensure_project_lua_running(cancellation)?;
    if let Err(error) = adapter.validate_database(connection, project) {
        if error.kind() == "cancelled" {
            return Err(ProjectLuaFailure::Cancelled);
        }
        ensure_project_lua_running(cancellation)?;
        return Err(ProjectLuaFailure::Host(
            error
                .with_engine(project_lua_engine(project))
                .with_operation(LuaOperation::ValidateDatabase),
        ));
    }
    ensure_project_lua_running(cancellation)?;
    let has_foreign_key_violation = sqlite_result_with_cancellation(
        connection
            .prepare("PRAGMA main.foreign_key_check")
            .and_then(|mut statement| statement.exists([])),
        ProjectLuaSqliteOperation::ForeignKeyCheck,
        cancellation,
    )?;
    if has_foreign_key_violation {
        return Err(ProjectLuaFailure::Validation {
            engine: project_lua_engine(project),
            failure: ProjectLuaValidationFailure::ForeignKey,
        });
    }

    ensure_project_lua_running(cancellation)?;
    let mut statement = sqlite_result_with_cancellation(
        connection.prepare("PRAGMA main.quick_check"),
        ProjectLuaSqliteOperation::QuickCheck,
        cancellation,
    )?;
    let mut rows = sqlite_result_with_cancellation(
        statement.query([]),
        ProjectLuaSqliteOperation::QuickCheck,
        cancellation,
    )?;
    let mut row_count = 0_u64;
    while let Some(row) = sqlite_result_with_cancellation(
        rows.next(),
        ProjectLuaSqliteOperation::QuickCheck,
        cancellation,
    )? {
        ensure_project_lua_running(cancellation)?;
        row_count = row_count.saturating_add(1);
        let result = sqlite_schema_text_with_cancellation(
            row,
            0,
            "quick_check",
            ProjectLuaSqliteOperation::QuickCheck,
            cancellation,
        )?;
        if !project_lua_text_eq_with_cancellation(&result, "ok", cancellation)? {
            return Err(ProjectLuaFailure::Validation {
                engine: project_lua_engine(project),
                failure: ProjectLuaValidationFailure::QuickCheck,
            });
        }
    }
    if row_count != 1 {
        return Err(ProjectLuaFailure::Validation {
            engine: project_lua_engine(project),
            failure: ProjectLuaValidationFailure::QuickCheck,
        });
    }
    ensure_project_lua_running(cancellation)
}

fn validate_no_protected_temp_schema(
    connection: &Connection,
    adapter: &dyn ProjectLuaEngineAdapter,
    engine: LuaEngine,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    ensure_project_lua_running(cancellation)?;
    let mut statement = sqlite_result_with_cancellation(
        connection.prepare(
            "SELECT type, name, tbl_name
             FROM temp.sqlite_schema
             WHERE type IN ('table', 'index', 'view', 'trigger')",
        ),
        ProjectLuaSqliteOperation::ReadTempSchema,
        cancellation,
    )?;
    let mut rows = sqlite_result_with_cancellation(
        statement.query([]),
        ProjectLuaSqliteOperation::ReadTempSchema,
        cancellation,
    )?;
    while let Some(row) = sqlite_result_with_cancellation(
        rows.next(),
        ProjectLuaSqliteOperation::ReadTempSchema,
        cancellation,
    )? {
        ensure_project_lua_running(cancellation)?;
        let raw_kind = sqlite_schema_text_with_cancellation(
            row,
            0,
            "type",
            ProjectLuaSqliteOperation::ReadTempSchema,
            cancellation,
        )?;
        let Some(kind) = ProjectLuaSchemaObjectKind::from_sqlite(&raw_kind) else {
            continue;
        };
        let name = sqlite_schema_text_with_cancellation(
            row,
            1,
            "name",
            ProjectLuaSqliteOperation::ReadTempSchema,
            cancellation,
        )?;
        let table_name = sqlite_schema_text_with_cancellation(
            row,
            2,
            "tbl_name",
            ProjectLuaSqliteOperation::ReadTempSchema,
            cancellation,
        )?;
        ensure_project_lua_running(cancellation)?;
        let protected = adapter.protects_schema_object(kind, &name, &table_name)
            || adapter.protects_schema_object(ProjectLuaSchemaObjectKind::Table, &name, &name)
            || matches!(
                kind,
                ProjectLuaSchemaObjectKind::Index | ProjectLuaSchemaObjectKind::Trigger
            ) && adapter.protects_schema_object(
                ProjectLuaSchemaObjectKind::Table,
                &table_name,
                &table_name,
            );
        ensure_project_lua_running(cancellation)?;
        if protected {
            return Err(ProjectLuaFailure::Validation {
                engine,
                failure: ProjectLuaValidationFailure::TemporarySchemaObject,
            });
        }
    }
    ensure_project_lua_running(cancellation)
}

impl BindingMetrics {
    fn report(&self) -> ProjectLuaRunReport {
        ProjectLuaRunReport {
            database_calls: self.database_calls(),
            changed_rows: self.changed_rows(),
            translation_calls: self.translation_calls(),
            printed_lines: self.printed_lines(),
        }
    }
}

#[cfg(test)]
mod tests;
