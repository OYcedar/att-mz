//! `rusqlite` 生产存储根。

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use tokio::sync::oneshot;

use crate::runtime::windows::{
    FileIdentity, WindowsFsError, delete_regular_file_if_identity, pin_directory_without_reparse,
    pin_path_without_reparse,
};
use crate::storage::sqlite::{
    CreateDatabaseError, ExecuteTransactionError, QueryExistingDatabaseError,
    SnapshotDatabaseError, SqliteCommand, SqliteDatabaseCreator, SqliteDatabaseSnapshotter,
    SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};
use crate::storage::sqlite_session::{
    OpenSqliteInteractiveSessionError, OpenedSqliteInteractiveSession,
    SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
    SqliteInteractiveSessionFinalization, SqliteInteractiveSessionFinalizationError,
    SqliteInteractiveSessionFinalizationFailure, SqliteInteractiveSessionFinalizer,
    SqliteInteractiveSessionOperations,
};

const STORAGE_RUNNING: u8 = 0;
const STORAGE_SHUTTING_DOWN: u8 = 1;
const STORAGE_CLOSED: u8 = 2;
const SESSION_OPEN: u8 = 0;
const SESSION_INDETERMINATE: u8 = 1;
const SESSION_FINALIZING: u8 = 2;
const SESSION_CLOSED: u8 = 3;
const MAX_QUERIES_PER_READ_SNAPSHOT: usize = 4;

/// SQLite journal mode 的受信配置值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqliteJournalMode {
    Delete,
    Truncate,
    Persist,
    Wal,
}

impl SqliteJournalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::Persist => "PERSIST",
            Self::Wal => "WAL",
        }
    }
}

/// SQLite synchronous 策略的受信配置值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqliteSynchronous {
    Normal,
    Full,
    Extra,
}

impl SqliteSynchronous {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }
}

/// SQLite 根所有线程、连接与单次结果的资源预算。
#[derive(Clone, Debug)]
pub(crate) struct RusqliteStorageConfiguration {
    short_worker_threads: NonZeroUsize,
    short_queue_capacity: NonZeroUsize,
    max_open_connections: NonZeroUsize,
    worker_stack_bytes: NonZeroUsize,
    max_statement_bytes: NonZeroUsize,
    max_parameter_bytes: NonZeroUsize,
    max_rows_per_query: NonZeroUsize,
    max_result_bytes_per_query: NonZeroUsize,
    busy_timeout: Duration,
    journal_mode: SqliteJournalMode,
    synchronous: SqliteSynchronous,
}

impl RusqliteStorageConfiguration {
    #[allow(clippy::too_many_arguments, reason = "每项外部资源选择均必须显式注入")]
    pub(crate) fn new(
        short_worker_threads: NonZeroUsize,
        short_queue_capacity: NonZeroUsize,
        max_open_connections: NonZeroUsize,
        worker_stack_bytes: NonZeroUsize,
        max_statement_bytes: NonZeroUsize,
        max_parameter_bytes: NonZeroUsize,
        max_rows_per_query: NonZeroUsize,
        max_result_bytes_per_query: NonZeroUsize,
        busy_timeout: Duration,
        journal_mode: SqliteJournalMode,
        synchronous: SqliteSynchronous,
    ) -> Result<Self, SqliteRuntimeError> {
        if busy_timeout.is_zero() {
            return Err(SqliteRuntimeError::InvalidConfiguration(
                "busy_timeout_ms 必须大于零",
            ));
        }

        Ok(Self {
            short_worker_threads,
            short_queue_capacity,
            max_open_connections,
            worker_stack_bytes,
            max_statement_bytes,
            max_parameter_bytes,
            max_rows_per_query,
            max_result_bytes_per_query,
            busy_timeout,
            journal_mode,
            synchronous,
        })
    }
}

/// SQLite 生产根本身的机制错误。
#[derive(Debug)]
pub(crate) enum SqliteRuntimeError {
    InvalidConfiguration(&'static str),
    InsufficientConnectionCapacity {
        operation: &'static str,
        required: usize,
        configured: usize,
    },
    Closed,
    InteractiveSessionAlreadyOpen,
    WorkerSpawn {
        worker: String,
        source: io::Error,
    },
    WorkerPanicked(&'static str),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    WindowsFileSystem {
        operation: &'static str,
        path: PathBuf,
        source: WindowsFsError,
    },
    Driver {
        operation: &'static str,
        source: rusqlite::Error,
    },
    InvalidTarget {
        path: PathBuf,
    },
    UnexpectedArtifact {
        path: PathBuf,
    },
    ResourceLimit {
        resource: &'static str,
        limit: usize,
    },
    InvalidValue(&'static str),
    Internal(&'static str),
    BackupIncomplete(&'static str),
    Cleanup {
        primary: Box<SqliteRuntimeError>,
        failures: Vec<String>,
    },
}

impl SqliteRuntimeError {
    fn driver(operation: &'static str, source: rusqlite::Error) -> Self {
        Self::Driver { operation, source }
    }

    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    fn windows_file_system(operation: &'static str, path: &Path, source: WindowsFsError) -> Self {
        Self::WindowsFileSystem {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for SqliteRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => write!(formatter, "SQLite 配置无效：{reason}"),
            Self::InsufficientConnectionCapacity {
                operation,
                required,
                configured,
            } => write!(
                formatter,
                "SQLite {operation} 需要同时占用 {required} 个连接，但当前连接上限为 {configured}"
            ),
            Self::Closed => formatter.write_str("SQLite 存储根已经关闭"),
            Self::InteractiveSessionAlreadyOpen => {
                formatter.write_str("当前 SQLite 存储根已有活动交互会话")
            }
            Self::WorkerSpawn { worker, source } => {
                write!(formatter, "无法启动 SQLite 工作线程 {worker}：{source}")
            }
            Self::WorkerPanicked(worker) => {
                write!(formatter, "SQLite 工作线程 {worker} 发生 panic")
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "SQLite {operation} {} 失败：{source}",
                path.display()
            ),
            Self::WindowsFileSystem {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "SQLite {operation} {} 失败：{source}",
                path.display()
            ),
            Self::Driver { operation, source } => {
                write!(formatter, "SQLite {operation} 失败：{source}")
            }
            Self::InvalidTarget { path } => {
                write!(formatter, "SQLite 目标不是普通文件：{}", path.display())
            }
            Self::UnexpectedArtifact { path } => write!(
                formatter,
                "SQLite 新建数据库之前已存在未归属本次操作的伴生文件：{}",
                path.display()
            ),
            Self::ResourceLimit { resource, limit } => {
                write!(formatter, "SQLite {resource} 超过配置上限 {limit}")
            }
            Self::InvalidValue(reason) => write!(formatter, "SQLite 值无效：{reason}"),
            Self::Internal(reason) => write!(formatter, "SQLite 内部不变量破坏：{reason}"),
            Self::BackupIncomplete(state) => {
                write!(formatter, "SQLite online backup 未完成：{state}")
            }
            Self::Cleanup { primary, failures } => {
                write!(formatter, "{primary}；清理失败")?;
                for failure in failures {
                    write!(formatter, "；{failure}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for SqliteRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkerSpawn { source, .. } | Self::Io { source, .. } => Some(source),
            Self::WindowsFileSystem { source, .. } => Some(source),
            Self::Driver { source, .. } => Some(source),
            Self::Cleanup { primary, .. } => Some(primary),
            Self::InvalidConfiguration(_)
            | Self::InsufficientConnectionCapacity { .. }
            | Self::Closed
            | Self::InteractiveSessionAlreadyOpen
            | Self::WorkerPanicked(_)
            | Self::InvalidTarget { .. }
            | Self::UnexpectedArtifact { .. }
            | Self::ResourceLimit { .. }
            | Self::InvalidValue(_)
            | Self::Internal(_)
            | Self::BackupIncomplete(_) => None,
        }
    }
}

struct PermitPool {
    maximum: usize,
    active: Mutex<usize>,
    changed: Condvar,
}

impl PermitPool {
    fn new(maximum: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            maximum: maximum.get(),
            active: Mutex::new(0),
            changed: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> PoolPermit {
        self.acquire_many(1)
    }

    fn acquire_many(self: &Arc<Self>, count: usize) -> PoolPermit {
        assert!(count > 0 && count <= self.maximum);
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while self.maximum - *active < count {
            active = self
                .changed
                .wait(active)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *active += count;
        PoolPermit {
            pool: Arc::clone(self),
            count,
        }
    }

    fn wait_until_empty(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *active != 0 {
            active = self
                .changed
                .wait(active)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct PoolPermit {
    pool: Arc<PermitPool>,
    count: usize,
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        let mut active = self
            .pool
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active -= self.count;
        self.pool.changed.notify_all();
    }
}

fn validate_statement(
    statement: &str,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    if statement.len() > config.max_statement_bytes.get() {
        return Err(SqliteRuntimeError::ResourceLimit {
            resource: "statement 字节数",
            limit: config.max_statement_bytes.get(),
        });
    }
    if statement.trim().is_empty() {
        return Err(SqliteRuntimeError::InvalidValue("statement 不得为空"));
    }
    Ok(())
}

fn sqlite_value_bytes(value: &SqliteValue) -> Result<usize, SqliteRuntimeError> {
    match value {
        SqliteValue::Null => Ok(0),
        SqliteValue::Integer(_) | SqliteValue::Real(_) => {
            if matches!(value, SqliteValue::Real(number) if !number.is_finite()) {
                Err(SqliteRuntimeError::InvalidValue("REAL 参数必须是有限数值"))
            } else {
                Ok(std::mem::size_of::<i64>())
            }
        }
        SqliteValue::Text(text) => Ok(text.len()),
        SqliteValue::Blob(bytes) => Ok(bytes.len()),
    }
}

fn validate_parameters(
    parameters: &[SqliteValue],
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    let mut bytes = 0usize;
    for value in parameters {
        bytes = bytes.checked_add(sqlite_value_bytes(value)?).ok_or(
            SqliteRuntimeError::ResourceLimit {
                resource: "参数字节数",
                limit: config.max_parameter_bytes.get(),
            },
        )?;
        if bytes > config.max_parameter_bytes.get() {
            return Err(SqliteRuntimeError::ResourceLimit {
                resource: "参数字节数",
                limit: config.max_parameter_bytes.get(),
            });
        }
    }
    Ok(())
}

fn validate_command(
    command: &SqliteCommand,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    validate_statement(command.statement(), config)?;
    validate_parameters(command.parameters(), config)
}

fn validate_query(
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    validate_statement(query.statement(), config)?;
    validate_parameters(query.parameters(), config)
}

fn validate_query_snapshot(
    queries: &[SqliteQuery],
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    if queries.is_empty() {
        return Err(SqliteRuntimeError::InvalidValue("只读快照查询集合不得为空"));
    }
    if queries.len() > MAX_QUERIES_PER_READ_SNAPSHOT {
        return Err(SqliteRuntimeError::ResourceLimit {
            resource: "只读快照查询数",
            limit: MAX_QUERIES_PER_READ_SNAPSHOT,
        });
    }

    for query in queries {
        validate_query(query, config)?;
    }
    Ok(())
}

impl ToSql for SqliteValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            SqliteValue::Null => ValueRef::Null,
            SqliteValue::Integer(value) => ValueRef::Integer(*value),
            SqliteValue::Real(value) => ValueRef::Real(*value),
            SqliteValue::Text(value) => ValueRef::Text(value.as_bytes()),
            SqliteValue::Blob(value) => ValueRef::Blob(value),
        }))
    }
}

fn preflight_existing_file(path: &Path) -> Result<(), ExistingFileError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(ExistingFileError::InvalidTarget),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(ExistingFileError::NotFound),
        Err(error) => Err(ExistingFileError::Io(error)),
    }
}

enum ExistingFileError {
    NotFound,
    InvalidTarget,
    Io(io::Error),
}

fn open_existing_read_only(
    path: &Path,
    config: &RusqliteStorageConfiguration,
) -> Result<Connection, ExistingFileErrorOrRuntime> {
    preflight_existing_file(path).map_err(ExistingFileErrorOrRuntime::Existing)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| {
        if matches!(fs::metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound) {
            ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound)
        } else {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver(
                "以只读方式打开数据库",
                source,
            ))
        }
    })?;
    connection
        .busy_timeout(config.busy_timeout)
        .map_err(|source| {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver(
                "设置 busy timeout",
                source,
            ))
        })?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|source| {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver(
                "启用只读连接外键约束",
                source,
            ))
        })?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|source| {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver("设置只读连接", source))
        })?;
    connection
        .pragma_update(None, "synchronous", config.synchronous.as_str())
        .map_err(|source| {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver(
                "设置只读 synchronous",
                source,
            ))
        })?;
    Ok(connection)
}

fn open_existing_read_write(
    path: &Path,
    config: &RusqliteStorageConfiguration,
) -> Result<Connection, ExistingFileErrorOrRuntime> {
    preflight_existing_file(path).map_err(ExistingFileErrorOrRuntime::Existing)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| {
        if matches!(fs::metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound) {
            ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound)
        } else {
            ExistingFileErrorOrRuntime::Runtime(SqliteRuntimeError::driver(
                "以读写方式打开数据库",
                source,
            ))
        }
    })?;
    apply_read_write_policy(&connection, config).map_err(ExistingFileErrorOrRuntime::Runtime)?;
    Ok(connection)
}

enum ExistingFileErrorOrRuntime {
    Existing(ExistingFileError),
    Runtime(SqliteRuntimeError),
}

fn apply_read_write_policy(
    connection: &Connection,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    connection
        .busy_timeout(config.busy_timeout)
        .map_err(|source| SqliteRuntimeError::driver("设置 busy timeout", source))?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|source| SqliteRuntimeError::driver("启用 foreign key 约束", source))?;
    connection
        .pragma_update(None, "journal_mode", config.journal_mode.as_str())
        .map_err(|source| SqliteRuntimeError::driver("设置 journal_mode", source))?;
    connection
        .pragma_update(None, "synchronous", config.synchronous.as_str())
        .map_err(|source| SqliteRuntimeError::driver("设置 synchronous", source))?;
    Ok(())
}

#[derive(Default)]
struct QueryResultBudget {
    rows: usize,
    bytes: usize,
}

fn read_query_rows_with_budget(
    connection: &Connection,
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
    budget: &mut QueryResultBudget,
) -> Result<Vec<SqliteRow>, SqliteRuntimeError> {
    let mut statement = connection
        .prepare(query.statement())
        .map_err(|source| SqliteRuntimeError::driver("准备查询", source))?;
    let column_count = statement.column_count();
    let mut cursor = statement
        .query(params_from_iter(query.parameters().iter()))
        .map_err(|source| SqliteRuntimeError::driver("绑定查询参数", source))?;
    let mut result = Vec::new();

    while let Some(row) = cursor
        .next()
        .map_err(|source| SqliteRuntimeError::driver("读取查询行", source))?
    {
        if budget.rows == config.max_rows_per_query.get() {
            return Err(SqliteRuntimeError::ResourceLimit {
                resource: "查询行数",
                limit: config.max_rows_per_query.get(),
            });
        }
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            let value = match row
                .get_ref(index)
                .map_err(|source| SqliteRuntimeError::driver("读取查询列", source))?
            {
                ValueRef::Null => SqliteValue::Null,
                ValueRef::Integer(value) => SqliteValue::Integer(value),
                ValueRef::Real(value) => SqliteValue::Real(value),
                ValueRef::Text(value) => SqliteValue::Text(
                    std::str::from_utf8(value)
                        .map_err(|_| SqliteRuntimeError::InvalidValue("TEXT 不是 UTF-8"))?
                        .to_owned(),
                ),
                ValueRef::Blob(value) => SqliteValue::Blob(value.to_vec()),
            };
            budget.bytes = budget
                .bytes
                .checked_add(sqlite_value_bytes(&value)?)
                .ok_or(SqliteRuntimeError::ResourceLimit {
                    resource: "查询结果字节数",
                    limit: config.max_result_bytes_per_query.get(),
                })?;
            if budget.bytes > config.max_result_bytes_per_query.get() {
                return Err(SqliteRuntimeError::ResourceLimit {
                    resource: "查询结果字节数",
                    limit: config.max_result_bytes_per_query.get(),
                });
            }
            values.push(value);
        }
        result.push(SqliteRow::new(values));
        budget.rows += 1;
    }
    Ok(result)
}

fn read_query_rows(
    connection: &Connection,
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
) -> Result<Vec<SqliteRow>, SqliteRuntimeError> {
    validate_query(query, config)?;
    read_query_rows_with_budget(connection, query, config, &mut QueryResultBudget::default())
}

fn run_query_existing(
    path: &Path,
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<SqliteRuntimeError>> {
    let connection =
        open_existing_read_only(path, config).map_err(|error| map_query_open_error(path, error))?;
    read_query_rows(&connection, query, config).map_err(QueryExistingDatabaseError::QueryFailed)
}

fn map_query_open_error(
    path: &Path,
    error: ExistingFileErrorOrRuntime,
) -> QueryExistingDatabaseError<SqliteRuntimeError> {
    match error {
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound) => {
            QueryExistingDatabaseError::NotFound
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::InvalidTarget) => {
            QueryExistingDatabaseError::QueryFailed(SqliteRuntimeError::InvalidTarget {
                path: path.to_path_buf(),
            })
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::Io(source)) => {
            QueryExistingDatabaseError::QueryFailed(SqliteRuntimeError::io(
                "检查数据库",
                path,
                source,
            ))
        }
        ExistingFileErrorOrRuntime::Runtime(source) => {
            QueryExistingDatabaseError::QueryFailed(source)
        }
    }
}

fn rollback_query_snapshot(
    connection: &Connection,
    primary: SqliteRuntimeError,
) -> SqliteRuntimeError {
    if connection.is_autocommit() {
        return primary;
    }
    match connection.execute_batch("ROLLBACK") {
        Ok(()) => primary,
        Err(source) => SqliteRuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: vec![format!("回滚只读查询快照失败：{source}")],
        },
    }
}

fn read_queries_in_snapshot(
    connection: &Connection,
    queries: &[SqliteQuery],
    config: &RusqliteStorageConfiguration,
    mut after_query: impl FnMut(usize),
) -> Result<Vec<Vec<SqliteRow>>, SqliteRuntimeError> {
    validate_query_snapshot(queries, config)?;
    connection
        .execute_batch("BEGIN")
        .map_err(|source| SqliteRuntimeError::driver("开始只读查询快照", source))?;

    let mut results = Vec::with_capacity(queries.len());
    for (index, query) in queries.iter().enumerate() {
        let mut budget = QueryResultBudget::default();
        match read_query_rows_with_budget(connection, query, config, &mut budget) {
            Ok(rows) => results.push(rows),
            Err(primary) => return Err(rollback_query_snapshot(connection, primary)),
        }
        after_query(index);
    }

    if let Err(source) = connection.execute_batch("COMMIT") {
        let primary = SqliteRuntimeError::driver("结束只读查询快照", source);
        return Err(rollback_query_snapshot(connection, primary));
    }
    if !connection.is_autocommit() {
        return Err(rollback_query_snapshot(
            connection,
            SqliteRuntimeError::Internal("只读查询快照提交后仍非 autocommit"),
        ));
    }
    Ok(results)
}

fn run_query_snapshot_existing(
    path: &Path,
    queries: &[SqliteQuery],
    config: &RusqliteStorageConfiguration,
) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<SqliteRuntimeError>> {
    validate_query_snapshot(queries, config).map_err(QueryExistingDatabaseError::QueryFailed)?;
    let connection =
        open_existing_read_only(path, config).map_err(|error| map_query_open_error(path, error))?;
    read_queries_in_snapshot(&connection, queries, config, |_| {})
        .map_err(QueryExistingDatabaseError::QueryFailed)
}

fn validate_transaction_plan(
    plan: &SqliteTransactionPlan,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    for step in plan.steps() {
        match step {
            SqliteTransactionStep::Execute(command) => validate_command(command, config)?,
            SqliteTransactionStep::ExecuteMany(batch)
            | SqliteTransactionStep::ExecuteManyExactlyOne(batch)
            | SqliteTransactionStep::RequireNoRowsMany(batch) => {
                validate_statement(batch.statement(), config)?;
                for parameters in batch.parameter_sets() {
                    validate_parameters(parameters, config)?;
                }
            }
            SqliteTransactionStep::RequireNoRows(query) => {
                validate_query(query, config)?;
            }
        }
    }
    Ok(())
}

fn rollback_after_failure(
    connection: &Connection,
    primary: SqliteRuntimeError,
) -> Result<SqliteRuntimeError, SqliteRuntimeError> {
    match connection.execute_batch("ROLLBACK") {
        Ok(()) if connection.is_autocommit() => Ok(primary),
        Ok(()) => Err(SqliteRuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: vec!["回滚返回成功后连接仍处于事务中".to_owned()],
        }),
        Err(source) => Err(SqliteRuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: vec![format!("回滚失败：{source}")],
        }),
    }
}

fn rollback_requirement_failure(
    connection: &Connection,
) -> Result<(), ExecuteTransactionError<SqliteRuntimeError>> {
    match connection.execute_batch("ROLLBACK") {
        Ok(()) if connection.is_autocommit() => Err(ExecuteTransactionError::RequirementFailed),
        Ok(()) => Err(ExecuteTransactionError::OutcomeUnknown(
            SqliteRuntimeError::Internal("事务条件失败回滚后仍非 autocommit"),
        )),
        Err(source) => Err(ExecuteTransactionError::OutcomeUnknown(
            SqliteRuntimeError::driver("回滚事务条件失败", source),
        )),
    }
}

fn run_transaction(
    path: &Path,
    plan: &SqliteTransactionPlan,
    config: &RusqliteStorageConfiguration,
) -> Result<(), ExecuteTransactionError<SqliteRuntimeError>> {
    validate_transaction_plan(plan, config).map_err(ExecuteTransactionError::NotCommitted)?;
    let connection = open_existing_read_write(path, config).map_err(|error| match error {
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound) => {
            ExecuteTransactionError::NotFound
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::InvalidTarget) => {
            ExecuteTransactionError::NotCommitted(SqliteRuntimeError::InvalidTarget {
                path: path.to_path_buf(),
            })
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::Io(source)) => {
            ExecuteTransactionError::NotCommitted(SqliteRuntimeError::io(
                "检查数据库",
                path,
                source,
            ))
        }
        ExistingFileErrorOrRuntime::Runtime(source) => {
            ExecuteTransactionError::NotCommitted(source)
        }
    })?;
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|source| {
            ExecuteTransactionError::NotCommitted(SqliteRuntimeError::driver("开始写事务", source))
        })?;

    for step in plan.steps() {
        let requirement_satisfied = match step {
            SqliteTransactionStep::Execute(command) => connection
                .execute(
                    command.statement(),
                    params_from_iter(command.parameters().iter()),
                )
                .map(|_| true)
                .map_err(|source| SqliteRuntimeError::driver("执行事务命令", source)),
            SqliteTransactionStep::ExecuteMany(batch) => (|| {
                let mut statement = connection
                    .prepare(batch.statement())
                    .map_err(|source| SqliteRuntimeError::driver("准备批量命令", source))?;
                for parameters in batch.parameter_sets() {
                    if let Err(source) = statement.execute(params_from_iter(parameters.iter())) {
                        return Err(SqliteRuntimeError::driver("执行批量命令", source));
                    }
                }
                Ok(true)
            })(),
            SqliteTransactionStep::ExecuteManyExactlyOne(batch) => (|| {
                let mut statement = connection
                    .prepare(batch.statement())
                    .map_err(|source| SqliteRuntimeError::driver("准备精确批量命令", source))?;
                for parameters in batch.parameter_sets() {
                    let affected = statement
                        .execute(params_from_iter(parameters.iter()))
                        .map_err(|source| SqliteRuntimeError::driver("执行精确批量命令", source))?;
                    if affected != 1 {
                        return Ok(false);
                    }
                }
                Ok(true)
            })(),
            SqliteTransactionStep::RequireNoRows(query) => (|| {
                let mut statement = connection
                    .prepare(query.statement())
                    .map_err(|source| SqliteRuntimeError::driver("准备事务条件查询", source))?;
                statement
                    .exists(params_from_iter(query.parameters().iter()))
                    .map(|exists| !exists)
                    .map_err(|source| SqliteRuntimeError::driver("执行事务条件查询", source))
            })(),
            SqliteTransactionStep::RequireNoRowsMany(batch) => (|| {
                let mut statement = connection
                    .prepare(batch.statement())
                    .map_err(|source| SqliteRuntimeError::driver("准备批量事务条件查询", source))?;
                for parameters in batch.parameter_sets() {
                    let exists = statement
                        .exists(params_from_iter(parameters.iter()))
                        .map_err(|source| {
                            SqliteRuntimeError::driver("执行批量事务条件查询", source)
                        })?;
                    if exists {
                        return Ok(false);
                    }
                }
                Ok(true)
            })(),
        };

        match requirement_satisfied {
            Ok(true) => {}
            Ok(false) => return rollback_requirement_failure(&connection),
            Err(primary) => {
                return match rollback_after_failure(&connection, primary) {
                    Ok(primary) => Err(ExecuteTransactionError::NotCommitted(primary)),
                    Err(source) => Err(ExecuteTransactionError::OutcomeUnknown(source)),
                };
            }
        }
        if connection.is_autocommit() {
            return Err(ExecuteTransactionError::OutcomeUnknown(
                SqliteRuntimeError::Internal("事务步骤意外结束了根事务"),
            ));
        }
    }

    match connection.execute_batch("COMMIT") {
        Ok(()) if connection.is_autocommit() => Ok(()),
        Ok(()) => Err(ExecuteTransactionError::OutcomeUnknown(
            SqliteRuntimeError::Internal("COMMIT 成功后连接仍非 autocommit"),
        )),
        Err(source) if connection.is_autocommit() => Err(ExecuteTransactionError::OutcomeUnknown(
            SqliteRuntimeError::driver("提交写事务", source),
        )),
        Err(source) => {
            let primary = SqliteRuntimeError::driver("提交写事务", source);
            match rollback_after_failure(&connection, primary) {
                Ok(primary) => Err(ExecuteTransactionError::NotCommitted(primary)),
                Err(source) => Err(ExecuteTransactionError::OutcomeUnknown(source)),
            }
        }
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn database_artifact_paths(path: &Path) -> [PathBuf; 4] {
    [
        path.to_path_buf(),
        sidecar_path(path, "-journal"),
        sidecar_path(path, "-wal"),
        sidecar_path(path, "-shm"),
    ]
}

struct TrackedDatabaseArtifact {
    path: PathBuf,
    identity: FileIdentity,
    is_main: bool,
}

struct CleanupAssessment {
    residual: bool,
    unknown: bool,
    failures: Vec<String>,
}

impl CleanupAssessment {
    fn clean() -> Self {
        Self {
            residual: false,
            unknown: false,
            failures: Vec::new(),
        }
    }
}

fn windows_not_found(error: &WindowsFsError) -> bool {
    matches!(
        error,
        WindowsFsError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
    )
}

fn observe_sidecar_artifacts(
    paths: &[PathBuf],
) -> (Vec<TrackedDatabaseArtifact>, CleanupAssessment) {
    let mut tracked = Vec::new();
    let mut assessment = CleanupAssessment::clean();
    for path in paths {
        match pin_path_without_reparse(path) {
            Ok(pinned) => match pinned.metadata() {
                Ok(metadata) if metadata.is_file() => match FileIdentity::of(pinned.file(), path) {
                    Ok(identity) => tracked.push(TrackedDatabaseArtifact {
                        path: path.clone(),
                        identity,
                        is_main: false,
                    }),
                    Err(source) => {
                        assessment.unknown = true;
                        assessment.failures.push(format!(
                            "无法确认 SQLite 伴生文件 {} 的物理身份：{source}",
                            path.display()
                        ));
                    }
                },
                Ok(_) => {
                    assessment.residual = true;
                    assessment.failures.push(format!(
                        "SQLite 伴生路径不是普通文件，已拒绝删除：{}",
                        path.display()
                    ));
                }
                Err(source) => {
                    assessment.unknown = true;
                    assessment.failures.push(format!(
                        "无法检查 SQLite 伴生文件 {} 的类型：{source}",
                        path.display()
                    ));
                }
            },
            Err(source) if windows_not_found(&source) => {}
            Err(source) => {
                assessment.unknown = true;
                assessment.failures.push(format!(
                    "无法固定 SQLite 伴生文件 {} 的物理身份：{source}",
                    path.display()
                ));
            }
        }
    }
    (tracked, assessment)
}

fn cleanup_database_artifacts(
    paths: &[PathBuf; 4],
    tracked: &[TrackedDatabaseArtifact],
    mut assessment: CleanupAssessment,
) -> CleanupAssessment {
    for artifact in tracked {
        if let Err(source) = delete_regular_file_if_identity(&artifact.path, artifact.identity) {
            if windows_not_found(&source) && !artifact.is_main {
                continue;
            }
            if artifact.is_main
                && (matches!(source, WindowsFsError::FileIdentityChanged { .. })
                    || windows_not_found(&source))
            {
                assessment.unknown = true;
            } else {
                assessment.residual = true;
            }
            assessment.failures.push(format!(
                "无法按已确认的物理身份删除 {}: {source}",
                artifact.path.display()
            ));
        }
    }

    for candidate in paths {
        match fs::symlink_metadata(candidate) {
            Ok(_) => assessment.residual = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                assessment.unknown = true;
                assessment.failures.push(format!(
                    "无法确认 {} 是否存在: {error}",
                    candidate.display()
                ));
            }
        }
    }
    assessment
}

fn creation_failure(
    primary: SqliteRuntimeError,
    cleanup: CleanupAssessment,
) -> CreateDatabaseError<SqliteRuntimeError> {
    let source = if cleanup.failures.is_empty() {
        primary
    } else {
        SqliteRuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: cleanup.failures,
        }
    };
    if cleanup.unknown {
        CreateDatabaseError::OutcomeUnknown(source)
    } else if cleanup.residual {
        CreateDatabaseError::ResidualArtifact(source)
    } else {
        CreateDatabaseError::NotCreated(source)
    }
}

struct DatabaseInitializationFailure {
    primary: SqliteRuntimeError,
    sidecars: Vec<TrackedDatabaseArtifact>,
    assessment: CleanupAssessment,
}

fn initialize_new_database(
    path: &Path,
    sidecar_paths: &[PathBuf],
    commands: &[SqliteCommand],
    config: &RusqliteStorageConfiguration,
) -> Result<(), Box<DatabaseInitializationFailure>> {
    let connection = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(source) => {
            let (sidecars, assessment) = observe_sidecar_artifacts(sidecar_paths);
            return Err(Box::new(DatabaseInitializationFailure {
                primary: SqliteRuntimeError::driver("打开新数据库", source),
                sidecars,
                assessment,
            }));
        }
    };
    let result = (|| {
        apply_read_write_policy(&connection, config)?;
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|source| SqliteRuntimeError::driver("开始新数据库事务", source))?;

        for command in commands {
            if let Err(source) = connection.execute(
                command.statement(),
                params_from_iter(command.parameters().iter()),
            ) {
                let primary = SqliteRuntimeError::driver("初始化新数据库", source);
                return Err(match rollback_after_failure(&connection, primary) {
                    Ok(primary) | Err(primary) => primary,
                });
            }
            if connection.is_autocommit() {
                return Err(SqliteRuntimeError::Internal("初始化命令意外结束了根事务"));
            }
        }

        match connection.execute_batch("COMMIT") {
            Ok(()) if connection.is_autocommit() => Ok(()),
            Ok(()) => Err(SqliteRuntimeError::Internal(
                "初始化 COMMIT 成功后连接仍非 autocommit",
            )),
            Err(source) => {
                let primary = SqliteRuntimeError::driver("提交新数据库", source);
                if connection.is_autocommit() {
                    Err(primary)
                } else {
                    Err(match rollback_after_failure(&connection, primary) {
                        Ok(primary) | Err(primary) => primary,
                    })
                }
            }
        }
    })();
    match result {
        Ok(()) => Ok(()),
        Err(primary) => {
            let (sidecars, assessment) = observe_sidecar_artifacts(sidecar_paths);
            Err(Box::new(DatabaseInitializationFailure {
                primary,
                sidecars,
                assessment,
            }))
        }
    }
}

fn run_create_database(
    path: &Path,
    commands: &[SqliteCommand],
    config: &RusqliteStorageConfiguration,
) -> Result<(), CreateDatabaseError<SqliteRuntimeError>> {
    for command in commands {
        validate_command(command, config).map_err(CreateDatabaseError::NotCreated)?;
    }
    let absolute = std::path::absolute(path).map_err(|source| {
        CreateDatabaseError::NotCreated(SqliteRuntimeError::io(
            "建立新数据库绝对路径",
            path,
            source,
        ))
    })?;
    let parent_path = absolute.parent().ok_or_else(|| {
        CreateDatabaseError::NotCreated(SqliteRuntimeError::InvalidTarget {
            path: absolute.clone(),
        })
    })?;
    let file_name = absolute.file_name().ok_or_else(|| {
        CreateDatabaseError::NotCreated(SqliteRuntimeError::InvalidTarget {
            path: absolute.clone(),
        })
    })?;
    let parent = pin_directory_without_reparse(parent_path).map_err(|source| {
        CreateDatabaseError::NotCreated(SqliteRuntimeError::windows_file_system(
            "固定新数据库父目录",
            parent_path,
            source,
        ))
    })?;
    let stable_path = parent.resolved_path().join(file_name);
    let placeholder = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stable_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CreateDatabaseError::AlreadyExists);
        }
        Err(error) => {
            return Err(CreateDatabaseError::NotCreated(SqliteRuntimeError::io(
                "原子占有新数据库路径",
                &stable_path,
                error,
            )));
        }
    };
    let main_identity = match FileIdentity::of(&placeholder, &stable_path) {
        Ok(identity) => identity,
        Err(source) => {
            drop(placeholder);
            return Err(CreateDatabaseError::OutcomeUnknown(
                SqliteRuntimeError::windows_file_system(
                    "固定新数据库物理身份",
                    &stable_path,
                    source,
                ),
            ));
        }
    };
    drop(placeholder);
    let paths = database_artifact_paths(&stable_path);
    let main = TrackedDatabaseArtifact {
        path: stable_path.clone(),
        identity: main_identity,
        is_main: true,
    };
    for sidecar in &paths[1..] {
        match fs::symlink_metadata(sidecar) {
            Ok(_) => {
                let cleanup = cleanup_database_artifacts(
                    &paths,
                    std::slice::from_ref(&main),
                    CleanupAssessment {
                        residual: true,
                        unknown: false,
                        failures: Vec::new(),
                    },
                );
                return Err(creation_failure(
                    SqliteRuntimeError::UnexpectedArtifact {
                        path: sidecar.clone(),
                    },
                    cleanup,
                ));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                let cleanup = cleanup_database_artifacts(
                    &paths,
                    std::slice::from_ref(&main),
                    CleanupAssessment {
                        residual: false,
                        unknown: true,
                        failures: Vec::new(),
                    },
                );
                return Err(creation_failure(
                    SqliteRuntimeError::io("确认 SQLite 伴生路径初始不存在", sidecar, source),
                    cleanup,
                ));
            }
        }
    }

    match initialize_new_database(&stable_path, &paths[1..], commands, config) {
        Ok(()) => Ok(()),
        Err(failure) => {
            let failure = *failure;
            let mut tracked = vec![main];
            tracked.extend(failure.sidecars);
            let cleanup = cleanup_database_artifacts(&paths, &tracked, failure.assessment);
            Err(creation_failure(failure.primary, cleanup))
        }
    }
}

fn snapshot_failure(
    primary: SqliteRuntimeError,
    cleanup: CleanupAssessment,
) -> SnapshotDatabaseError<SqliteRuntimeError> {
    let source = if cleanup.failures.is_empty() {
        primary
    } else {
        SqliteRuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: cleanup.failures,
        }
    };
    if cleanup.unknown {
        SnapshotDatabaseError::OutcomeUnknown(source)
    } else if cleanup.residual {
        SnapshotDatabaseError::ResidualArtifact(source)
    } else {
        SnapshotDatabaseError::NotCreated(source)
    }
}

fn snapshot_source_connection(
    path: &Path,
    config: &RusqliteStorageConfiguration,
) -> Result<Connection, SnapshotDatabaseError<SqliteRuntimeError>> {
    open_existing_read_only(path, config).map_err(|error| match error {
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound) => {
            SnapshotDatabaseError::SourceNotFound
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::InvalidTarget) => {
            SnapshotDatabaseError::NotCreated(SqliteRuntimeError::InvalidTarget {
                path: path.to_path_buf(),
            })
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::Io(source)) => {
            SnapshotDatabaseError::NotCreated(SqliteRuntimeError::io(
                "检查快照源数据库",
                path,
                source,
            ))
        }
        ExistingFileErrorOrRuntime::Runtime(source) => SnapshotDatabaseError::NotCreated(source),
    })
}

fn run_snapshot_database(
    source_path: &Path,
    destination_path: &Path,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SnapshotDatabaseError<SqliteRuntimeError>> {
    let source = snapshot_source_connection(source_path, config)?;
    let absolute = std::path::absolute(destination_path).map_err(|source| {
        SnapshotDatabaseError::NotCreated(SqliteRuntimeError::io(
            "建立快照目标绝对路径",
            destination_path,
            source,
        ))
    })?;
    let parent_path = absolute.parent().ok_or_else(|| {
        SnapshotDatabaseError::NotCreated(SqliteRuntimeError::InvalidTarget {
            path: absolute.clone(),
        })
    })?;
    let file_name = absolute.file_name().ok_or_else(|| {
        SnapshotDatabaseError::NotCreated(SqliteRuntimeError::InvalidTarget {
            path: absolute.clone(),
        })
    })?;
    let parent = pin_directory_without_reparse(parent_path).map_err(|source| {
        SnapshotDatabaseError::NotCreated(SqliteRuntimeError::windows_file_system(
            "固定快照目标父目录",
            parent_path,
            source,
        ))
    })?;
    let stable_path = parent.resolved_path().join(file_name);
    let placeholder = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stable_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(SnapshotDatabaseError::DestinationAlreadyExists);
        }
        Err(error) => {
            return Err(SnapshotDatabaseError::NotCreated(SqliteRuntimeError::io(
                "原子占有快照目标路径",
                &stable_path,
                error,
            )));
        }
    };
    let main_identity = match FileIdentity::of(&placeholder, &stable_path) {
        Ok(identity) => identity,
        Err(source) => {
            drop(placeholder);
            return Err(SnapshotDatabaseError::OutcomeUnknown(
                SqliteRuntimeError::windows_file_system(
                    "固定快照目标物理身份",
                    &stable_path,
                    source,
                ),
            ));
        }
    };
    drop(placeholder);

    let paths = database_artifact_paths(&stable_path);
    let main = TrackedDatabaseArtifact {
        path: stable_path.clone(),
        identity: main_identity,
        is_main: true,
    };
    for sidecar in &paths[1..] {
        match fs::symlink_metadata(sidecar) {
            Ok(_) => {
                let cleanup = cleanup_database_artifacts(
                    &paths,
                    std::slice::from_ref(&main),
                    CleanupAssessment {
                        residual: true,
                        unknown: false,
                        failures: Vec::new(),
                    },
                );
                return Err(snapshot_failure(
                    SqliteRuntimeError::UnexpectedArtifact {
                        path: sidecar.clone(),
                    },
                    cleanup,
                ));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                let cleanup = cleanup_database_artifacts(
                    &paths,
                    std::slice::from_ref(&main),
                    CleanupAssessment {
                        residual: false,
                        unknown: true,
                        failures: Vec::new(),
                    },
                );
                return Err(snapshot_failure(
                    SqliteRuntimeError::io("确认快照目标伴生路径初始不存在", sidecar, source),
                    cleanup,
                ));
            }
        }
    }

    let mut destination = match Connection::open_with_flags(
        &stable_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(source) => {
            let (sidecars, assessment) = observe_sidecar_artifacts(&paths[1..]);
            let mut tracked = vec![main];
            tracked.extend(sidecars);
            let cleanup = cleanup_database_artifacts(&paths, &tracked, assessment);
            return Err(snapshot_failure(
                SqliteRuntimeError::driver("打开快照目标数据库", source),
                cleanup,
            ));
        }
    };
    let result = (|| {
        apply_read_write_policy(&destination, config)?;
        let backup = Backup::new(&source, &mut destination)
            .map_err(|source| SqliteRuntimeError::driver("建立 online backup", source))?;
        match backup
            .step(-1)
            .map_err(|source| SqliteRuntimeError::driver("执行 online backup", source))?
        {
            StepResult::Done => Ok(()),
            StepResult::More => Err(SqliteRuntimeError::BackupIncomplete("MORE")),
            StepResult::Busy => Err(SqliteRuntimeError::BackupIncomplete("BUSY")),
            StepResult::Locked => Err(SqliteRuntimeError::BackupIncomplete("LOCKED")),
            _ => Err(SqliteRuntimeError::BackupIncomplete("未知状态")),
        }
    })();
    drop(destination);
    drop(source);

    if let Err(primary) = result {
        let (sidecars, assessment) = observe_sidecar_artifacts(&paths[1..]);
        let mut tracked = vec![main];
        tracked.extend(sidecars);
        let cleanup = cleanup_database_artifacts(&paths, &tracked, assessment);
        return Err(snapshot_failure(primary, cleanup));
    }

    let pinned = pin_path_without_reparse(&stable_path).map_err(|source| {
        SnapshotDatabaseError::OutcomeUnknown(SqliteRuntimeError::windows_file_system(
            "复核快照目标物理身份",
            &stable_path,
            source,
        ))
    })?;
    let final_identity = FileIdentity::of(pinned.file(), &stable_path).map_err(|source| {
        SnapshotDatabaseError::OutcomeUnknown(SqliteRuntimeError::windows_file_system(
            "读取快照目标最终身份",
            &stable_path,
            source,
        ))
    })?;
    if final_identity != main_identity {
        return Err(SnapshotDatabaseError::OutcomeUnknown(
            SqliteRuntimeError::Internal("快照目标物理身份在 online backup 期间发生变化"),
        ));
    }
    Ok(())
}

type InteractiveQueryResult =
    Result<Vec<SqliteRow>, SqliteInteractiveSessionError<SqliteRuntimeError>>;
type InteractiveExecuteResult = Result<u64, SqliteInteractiveSessionError<SqliteRuntimeError>>;
type InteractiveUnitResult = Result<(), SqliteInteractiveSessionError<SqliteRuntimeError>>;

enum InteractiveCommand {
    Query {
        query: SqliteQuery,
        response: oneshot::Sender<InteractiveQueryResult>,
    },
    Execute {
        command: SqliteCommand,
        response: oneshot::Sender<InteractiveExecuteResult>,
    },
    Begin {
        response: oneshot::Sender<InteractiveUnitResult>,
    },
    Commit {
        response: oneshot::Sender<InteractiveUnitResult>,
    },
    Rollback {
        response: oneshot::Sender<InteractiveUnitResult>,
    },
    #[cfg(test)]
    ExecuteAfterGate {
        command: SqliteCommand,
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        response: oneshot::Sender<InteractiveExecuteResult>,
    },
}

#[derive(Clone, Copy)]
enum InteractiveTransactionState {
    Idle,
    Active,
    Indeterminate,
}

fn session_unavailable<T>(
    lifecycle: &AtomicU8,
) -> Option<Result<T, SqliteInteractiveSessionError<SqliteRuntimeError>>> {
    match lifecycle.load(Ordering::Acquire) {
        SESSION_OPEN => None,
        SESSION_INDETERMINATE => Some(Err(SqliteInteractiveSessionError::Indeterminate)),
        SESSION_FINALIZING | SESSION_CLOSED => Some(Err(SqliteInteractiveSessionError::Closed)),
        _ => Some(Err(SqliteInteractiveSessionError::OperationFailed(
            SqliteRuntimeError::Internal("交互式会话生命周期值无效"),
        ))),
    }
}

fn observe_operation<T>(
    connection: &Connection,
    transaction: &mut InteractiveTransactionState,
    lifecycle: &AtomicU8,
    before_autocommit: bool,
    result: Result<T, SqliteRuntimeError>,
) -> Result<T, SqliteInteractiveSessionError<SqliteRuntimeError>> {
    let after_autocommit = connection.is_autocommit();
    match result {
        Ok(value) => {
            *transaction = if after_autocommit {
                InteractiveTransactionState::Idle
            } else {
                InteractiveTransactionState::Active
            };
            Ok(value)
        }
        Err(source) if before_autocommit != after_autocommit => {
            *transaction = InteractiveTransactionState::Indeterminate;
            lifecycle.store(SESSION_INDETERMINATE, Ordering::Release);
            Err(SqliteInteractiveSessionError::OutcomeUnknown(source))
        }
        Err(source) => {
            *transaction = if after_autocommit {
                InteractiveTransactionState::Idle
            } else {
                InteractiveTransactionState::Active
            };
            Err(SqliteInteractiveSessionError::OperationFailed(source))
        }
    }
}

fn process_interactive_command(
    connection: &Connection,
    config: &RusqliteStorageConfiguration,
    transaction: &mut InteractiveTransactionState,
    lifecycle: &AtomicU8,
    command: InteractiveCommand,
) {
    if matches!(transaction, InteractiveTransactionState::Indeterminate) {
        match command {
            InteractiveCommand::Query { response, .. } => {
                let _ = response.send(Err(SqliteInteractiveSessionError::Indeterminate));
            }
            InteractiveCommand::Execute { response, .. } => {
                let _ = response.send(Err(SqliteInteractiveSessionError::Indeterminate));
            }
            #[cfg(test)]
            InteractiveCommand::ExecuteAfterGate { response, .. } => {
                let _ = response.send(Err(SqliteInteractiveSessionError::Indeterminate));
            }
            InteractiveCommand::Begin { response }
            | InteractiveCommand::Commit { response }
            | InteractiveCommand::Rollback { response } => {
                let _ = response.send(Err(SqliteInteractiveSessionError::Indeterminate));
            }
        }
        return;
    }

    let before_autocommit = connection.is_autocommit();
    match command {
        InteractiveCommand::Query { query, response } => {
            let result = read_query_rows(connection, &query, config);
            let _ = response.send(observe_operation(
                connection,
                transaction,
                lifecycle,
                before_autocommit,
                result,
            ));
        }
        InteractiveCommand::Execute { command, response } => {
            let result = validate_command(&command, config).and_then(|()| {
                connection
                    .execute(
                        command.statement(),
                        params_from_iter(command.parameters().iter()),
                    )
                    .map_err(|source| SqliteRuntimeError::driver("执行交互式命令", source))
                    .and_then(|affected| {
                        u64::try_from(affected)
                            .map_err(|_| SqliteRuntimeError::Internal("受影响行数无法表示为 u64"))
                    })
            });
            let _ = response.send(observe_operation(
                connection,
                transaction,
                lifecycle,
                before_autocommit,
                result,
            ));
        }
        InteractiveCommand::Begin { response } => {
            let result = if matches!(transaction, InteractiveTransactionState::Active) {
                Err(SqliteInteractiveSessionError::TransactionAlreadyActive)
            } else {
                observe_operation(
                    connection,
                    transaction,
                    lifecycle,
                    before_autocommit,
                    connection
                        .execute_batch("BEGIN DEFERRED")
                        .map_err(|source| SqliteRuntimeError::driver("开始交互式事务", source)),
                )
            };
            let _ = response.send(result);
        }
        InteractiveCommand::Commit { response } => {
            let result = if matches!(transaction, InteractiveTransactionState::Idle) {
                Err(SqliteInteractiveSessionError::NoActiveTransaction)
            } else {
                observe_operation(
                    connection,
                    transaction,
                    lifecycle,
                    before_autocommit,
                    connection
                        .execute_batch("COMMIT")
                        .map_err(|source| SqliteRuntimeError::driver("提交交互式事务", source)),
                )
            };
            let _ = response.send(result);
        }
        InteractiveCommand::Rollback { response } => {
            let result = if matches!(transaction, InteractiveTransactionState::Idle) {
                Err(SqliteInteractiveSessionError::NoActiveTransaction)
            } else {
                observe_operation(
                    connection,
                    transaction,
                    lifecycle,
                    before_autocommit,
                    connection
                        .execute_batch("ROLLBACK")
                        .map_err(|source| SqliteRuntimeError::driver("回滚交互式事务", source)),
                )
            };
            let _ = response.send(result);
        }
        #[cfg(test)]
        InteractiveCommand::ExecuteAfterGate {
            command,
            entered,
            release,
            response,
        } => {
            let _ = entered.send(());
            let _ = release.recv();
            process_interactive_command(
                connection,
                config,
                transaction,
                lifecycle,
                InteractiveCommand::Execute { command, response },
            );
        }
    }
}

type InteractiveFinalizationResult = Result<
    SqliteInteractiveSessionFinalization,
    SqliteInteractiveSessionFinalizationError<SqliteRuntimeError>,
>;

fn finalize_interactive_connection(
    connection: Connection,
    lifecycle: &AtomicU8,
) -> InteractiveFinalizationResult {
    let had_unclosed_transaction = !connection.is_autocommit();
    let primary = if had_unclosed_transaction {
        match connection.execute_batch("ROLLBACK") {
            Ok(()) if connection.is_autocommit() => None,
            Ok(()) => Some(SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
                SqliteRuntimeError::Internal("回滚成功后交互式连接仍非 autocommit"),
            )),
            Err(source) if connection.is_autocommit() => {
                Some(SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
                    SqliteRuntimeError::driver("终结交互式事务", source),
                ))
            }
            Err(source) => Some(SqliteInteractiveSessionFinalizationFailure::CleanupFailed(
                SqliteRuntimeError::driver("终结交互式事务", source),
            )),
        }
    } else {
        None
    };
    let close_failure = match connection.close() {
        Ok(()) => None,
        Err((_connection, source)) => Some(SqliteRuntimeError::driver("关闭交互式连接", source)),
    };
    lifecycle.store(SESSION_CLOSED, Ordering::Release);
    match (primary, close_failure) {
        (None, None) => Ok(SqliteInteractiveSessionFinalization::new(
            had_unclosed_transaction,
        )),
        (None, Some(source)) => Err(SqliteInteractiveSessionFinalizationError::new(
            SqliteInteractiveSessionFinalizationFailure::CleanupFailed(source),
            None,
        )),
        (Some(primary), connection_close) => Err(SqliteInteractiveSessionFinalizationError::new(
            primary,
            connection_close,
        )),
    }
}

fn panicked_finalization_result() -> InteractiveFinalizationResult {
    Err(SqliteInteractiveSessionFinalizationError::new(
        SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
            SqliteRuntimeError::WorkerPanicked("交互式 actor"),
        ),
        None,
    ))
}

fn run_interactive_actor(
    connection: Connection,
    config: Arc<RusqliteStorageConfiguration>,
    commands: async_channel::Receiver<InteractiveCommand>,
    control: mpsc::Receiver<()>,
    lifecycle: Arc<AtomicU8>,
    _connection_permit: PoolPermit,
) -> InteractiveFinalizationResult {
    let mut transaction = if connection.is_autocommit() {
        InteractiveTransactionState::Idle
    } else {
        InteractiveTransactionState::Active
    };
    while let Ok(command) = commands.recv_blocking() {
        process_interactive_command(&connection, &config, &mut transaction, &lifecycle, command);
    }
    let _ = control.recv();
    finalize_interactive_connection(connection, &lifecycle)
}

struct InteractiveSessionSlotState {
    accepting: bool,
    opening: bool,
    active: Option<Arc<InteractiveSessionControl>>,
}

struct InteractiveSessionSlot {
    state: Mutex<InteractiveSessionSlotState>,
    changed: Condvar,
}

impl InteractiveSessionSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InteractiveSessionSlotState {
                accepting: true,
                opening: false,
                active: None,
            }),
            changed: Condvar::new(),
        })
    }

    fn begin_open(&self) -> Result<(), SqliteRuntimeError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(SqliteRuntimeError::Closed);
        }
        if state.opening || state.active.is_some() {
            return Err(SqliteRuntimeError::InteractiveSessionAlreadyOpen);
        }
        state.opening = true;
        Ok(())
    }

    fn complete_open(&self, control: Arc<InteractiveSessionControl>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.opening && state.active.is_none());
        state.opening = false;
        state.active = Some(control);
        self.changed.notify_all();
        state.accepting
    }

    fn abort_open(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.opening = false;
        self.changed.notify_all();
    }

    fn recover_open_panic(&self) -> Option<Arc<InteractiveSessionControl>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.opening = false;
        self.changed.notify_all();
        state.active.clone()
    }

    fn complete(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = None;
        self.changed.notify_all();
    }

    fn begin_shutdown(&self) -> Option<Arc<InteractiveSessionControl>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        state.active.clone()
    }

    fn wait_until_empty(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.opening || state.active.is_some() {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct InteractiveFinalizationResources {
    command_receiver: async_channel::Receiver<InteractiveCommand>,
    control: mpsc::Sender<()>,
}

struct InteractiveSessionControl {
    lifecycle: Arc<AtomicU8>,
    resources: Mutex<Option<InteractiveFinalizationResources>>,
}

impl InteractiveSessionControl {
    fn initiate(self: &Arc<Self>) {
        let resources = self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(resources) = resources else {
            return;
        };
        self.lifecycle.store(SESSION_FINALIZING, Ordering::Release);
        resources.command_receiver.close();
        let _ = resources.control.send(());
    }
}

/// `rusqlite` 交互式会话的共享操作面。
pub(crate) struct RusqliteInteractiveSessionOperations {
    commands: async_channel::Sender<InteractiveCommand>,
    lifecycle: Arc<AtomicU8>,
}

impl RusqliteInteractiveSessionOperations {
    async fn await_query(
        &self,
        query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, SqliteInteractiveSessionError<SqliteRuntimeError>> {
        if let Some(result) = session_unavailable(&self.lifecycle) {
            return result;
        }
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(InteractiveCommand::Query { query, response })
            .await
            .map_err(|_| SqliteInteractiveSessionError::Closed)?;
        receiver.await.unwrap_or_else(|_| {
            Err(SqliteInteractiveSessionError::OperationFailed(
                SqliteRuntimeError::WorkerPanicked("交互式 actor"),
            ))
        })
    }

    async fn await_execute(
        &self,
        command: SqliteCommand,
    ) -> Result<u64, SqliteInteractiveSessionError<SqliteRuntimeError>> {
        if let Some(result) = session_unavailable(&self.lifecycle) {
            return result;
        }
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(InteractiveCommand::Execute { command, response })
            .await
            .map_err(|_| SqliteInteractiveSessionError::Closed)?;
        receiver.await.unwrap_or_else(|_| {
            Err(SqliteInteractiveSessionError::OperationFailed(
                SqliteRuntimeError::WorkerPanicked("交互式 actor"),
            ))
        })
    }

    async fn await_unit(
        &self,
        command: impl FnOnce(oneshot::Sender<InteractiveUnitResult>) -> InteractiveCommand,
    ) -> Result<(), SqliteInteractiveSessionError<SqliteRuntimeError>> {
        if let Some(result) = session_unavailable(&self.lifecycle) {
            return result;
        }
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(command(response))
            .await
            .map_err(|_| SqliteInteractiveSessionError::Closed)?;
        receiver.await.unwrap_or_else(|_| {
            Err(SqliteInteractiveSessionError::OperationFailed(
                SqliteRuntimeError::WorkerPanicked("交互式 actor"),
            ))
        })
    }
}

impl SqliteInteractiveSessionOperations for RusqliteInteractiveSessionOperations {
    type Error = SqliteRuntimeError;

    fn query(&self, query: SqliteQuery) -> impl Future<Output = InteractiveQueryResult> + Send {
        self.await_query(query)
    }

    fn execute(
        &self,
        command: SqliteCommand,
    ) -> impl Future<Output = InteractiveExecuteResult> + Send {
        self.await_execute(command)
    }

    fn begin(&self) -> impl Future<Output = InteractiveUnitResult> + Send {
        self.await_unit(|response| InteractiveCommand::Begin { response })
    }

    fn commit(&self) -> impl Future<Output = InteractiveUnitResult> + Send {
        self.await_unit(|response| InteractiveCommand::Commit { response })
    }

    fn rollback(&self) -> impl Future<Output = InteractiveUnitResult> + Send {
        self.await_unit(|response| InteractiveCommand::Rollback { response })
    }
}

/// `rusqlite` 交互式会话的唯一终结令牌。
pub(crate) struct RusqliteInteractiveSessionFinalizer {
    control: Arc<InteractiveSessionControl>,
    report: Option<oneshot::Receiver<InteractiveFinalizationResult>>,
}

impl RusqliteInteractiveSessionFinalizer {
    fn initiate(&mut self) -> oneshot::Receiver<InteractiveFinalizationResult> {
        self.control.initiate();
        self.report.take().expect("终结令牌必须拥有唯一报告接收端")
    }
}

impl SqliteInteractiveSessionFinalizer for RusqliteInteractiveSessionFinalizer {
    type Error = SqliteRuntimeError;

    fn finalize(mut self) -> impl Future<Output = InteractiveFinalizationResult> + Send {
        let receiver = self.initiate();
        async move {
            receiver
                .await
                .unwrap_or_else(|_| panicked_finalization_result())
        }
    }
}

impl Drop for RusqliteInteractiveSessionFinalizer {
    fn drop(&mut self) {
        self.control.initiate();
        if let Some(report) = self.report.take() {
            drop(report);
        }
    }
}

type OpenedRusqliteSession = OpenedSqliteInteractiveSession<
    RusqliteInteractiveSessionOperations,
    RusqliteInteractiveSessionFinalizer,
>;

enum ShortJob {
    Create {
        path: PathBuf,
        commands: Vec<SqliteCommand>,
        response: oneshot::Sender<Result<(), CreateDatabaseError<SqliteRuntimeError>>>,
        #[cfg(test)]
        panic_after_operation: bool,
    },
    Snapshot {
        source: PathBuf,
        destination: PathBuf,
        response: oneshot::Sender<Result<(), SnapshotDatabaseError<SqliteRuntimeError>>>,
        #[cfg(test)]
        panic_after_operation: bool,
    },
    Query {
        path: PathBuf,
        query: SqliteQuery,
        response:
            oneshot::Sender<Result<Vec<SqliteRow>, QueryExistingDatabaseError<SqliteRuntimeError>>>,
    },
    QuerySnapshot {
        path: PathBuf,
        queries: Vec<SqliteQuery>,
        response: oneshot::Sender<
            Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<SqliteRuntimeError>>,
        >,
    },
    Transaction {
        path: PathBuf,
        plan: SqliteTransactionPlan,
        response: oneshot::Sender<Result<(), ExecuteTransactionError<SqliteRuntimeError>>>,
        #[cfg(test)]
        panic_after_operation: bool,
    },
}

fn run_short_worker(
    receiver: async_channel::Receiver<ShortJob>,
    config: Arc<RusqliteStorageConfiguration>,
    connections: Arc<PermitPool>,
) {
    while let Ok(job) = receiver.recv_blocking() {
        let _permit = if matches!(&job, ShortJob::Snapshot { .. }) {
            connections.acquire_many(2)
        } else {
            connections.acquire()
        };
        match job {
            ShortJob::Create {
                path,
                commands,
                response,
                #[cfg(test)]
                panic_after_operation,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let result = run_create_database(&path, &commands, &config);
                    #[cfg(test)]
                    if panic_after_operation {
                        panic!("测试注入：建库副作用后 panic");
                    }
                    result
                }))
                .unwrap_or_else(|_| {
                    Err(CreateDatabaseError::OutcomeUnknown(
                        SqliteRuntimeError::WorkerPanicked("短操作"),
                    ))
                });
                let _ = response.send(result);
            }
            ShortJob::Snapshot {
                source,
                destination,
                response,
                #[cfg(test)]
                panic_after_operation,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let result = run_snapshot_database(&source, &destination, &config);
                    #[cfg(test)]
                    if panic_after_operation {
                        panic!("测试注入：数据库快照副作用后 panic");
                    }
                    result
                }))
                .unwrap_or_else(|_| {
                    Err(SnapshotDatabaseError::OutcomeUnknown(
                        SqliteRuntimeError::WorkerPanicked("短操作"),
                    ))
                });
                let _ = response.send(result);
            }
            ShortJob::Query {
                path,
                query,
                response,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_query_existing(&path, &query, &config)
                }))
                .unwrap_or_else(|_| {
                    Err(QueryExistingDatabaseError::QueryFailed(
                        SqliteRuntimeError::WorkerPanicked("短操作"),
                    ))
                });
                let _ = response.send(result);
            }
            ShortJob::QuerySnapshot {
                path,
                queries,
                response,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_query_snapshot_existing(&path, &queries, &config)
                }))
                .unwrap_or_else(|_| {
                    Err(QueryExistingDatabaseError::QueryFailed(
                        SqliteRuntimeError::WorkerPanicked("短操作"),
                    ))
                });
                let _ = response.send(result);
            }
            ShortJob::Transaction {
                path,
                plan,
                response,
                #[cfg(test)]
                panic_after_operation,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let result = run_transaction(&path, &plan, &config);
                    #[cfg(test)]
                    if panic_after_operation {
                        panic!("测试注入：短事务副作用后 panic");
                    }
                    result
                }))
                .unwrap_or_else(|_| {
                    Err(ExecuteTransactionError::OutcomeUnknown(
                        SqliteRuntimeError::WorkerPanicked("短操作"),
                    ))
                });
                let _ = response.send(result);
            }
        }
    }
}

fn open_interactive_session(
    path: &Path,
    config: Arc<RusqliteStorageConfiguration>,
    connections: &Arc<PermitPool>,
    slot: Arc<InteractiveSessionSlot>,
) -> Result<OpenedRusqliteSession, OpenSqliteInteractiveSessionError<SqliteRuntimeError>> {
    let connection_permit = connections.acquire();
    let connection = open_existing_read_write(path, &config).map_err(|error| match error {
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::NotFound) => {
            OpenSqliteInteractiveSessionError::NotFound
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::InvalidTarget) => {
            OpenSqliteInteractiveSessionError::OpenFailed(SqliteRuntimeError::InvalidTarget {
                path: path.to_path_buf(),
            })
        }
        ExistingFileErrorOrRuntime::Existing(ExistingFileError::Io(source)) => {
            OpenSqliteInteractiveSessionError::OpenFailed(SqliteRuntimeError::io(
                "检查交互式数据库",
                path,
                source,
            ))
        }
        ExistingFileErrorOrRuntime::Runtime(source) => {
            OpenSqliteInteractiveSessionError::OpenFailed(source)
        }
    })?;
    let (command_sender, command_receiver) = async_channel::bounded(1);
    let actor_receiver = command_receiver.clone();
    let (control_sender, control_receiver) = mpsc::channel();
    let (start_sender, start_receiver) = mpsc::sync_channel(0);
    let lifecycle = Arc::new(AtomicU8::new(SESSION_OPEN));
    let actor_lifecycle = Arc::clone(&lifecycle);
    let actor_config = Arc::clone(&config);
    let (report_sender, report_receiver) = oneshot::channel();
    let control = Arc::new(InteractiveSessionControl {
        lifecycle: Arc::clone(&lifecycle),
        resources: Mutex::new(Some(InteractiveFinalizationResources {
            command_receiver,
            control: control_sender,
        })),
    });
    let actor_slot = Arc::clone(&slot);
    let actor = thread::Builder::new()
        .name("att-sqlite-session".to_owned())
        .stack_size(config.worker_stack_bytes.get())
        .spawn(move || {
            let result = if start_receiver.recv().is_err() {
                panicked_finalization_result()
            } else {
                catch_unwind(AssertUnwindSafe(|| {
                    run_interactive_actor(
                        connection,
                        actor_config,
                        actor_receiver,
                        control_receiver,
                        actor_lifecycle,
                        connection_permit,
                    )
                }))
                .unwrap_or_else(|_| panicked_finalization_result())
            };
            actor_slot.complete();
            let _ = report_sender.send(result);
        })
        .map_err(|source| {
            OpenSqliteInteractiveSessionError::OpenFailed(SqliteRuntimeError::WorkerSpawn {
                worker: "interactive".to_owned(),
                source,
            })
        })?;
    // actor 自行上报终态并释放唯一会话槽，不需要第二个回收线程持有 join handle。
    drop(actor);
    let accepted = slot.complete_open(Arc::clone(&control));
    let _ = start_sender.send(());
    if !accepted {
        control.initiate();
        return Err(OpenSqliteInteractiveSessionError::OpenFailed(
            SqliteRuntimeError::Closed,
        ));
    }
    let operations = Arc::new(RusqliteInteractiveSessionOperations {
        commands: command_sender,
        lifecycle: Arc::clone(&lifecycle),
    });
    let finalizer = RusqliteInteractiveSessionFinalizer {
        control,
        report: Some(report_receiver),
    };
    Ok(OpenedSqliteInteractiveSession::new(operations, finalizer))
}

struct RusqliteStorageInner {
    config: Arc<RusqliteStorageConfiguration>,
    accepting: AtomicBool,
    lifecycle: AtomicU8,
    short_sender: async_channel::Sender<ShortJob>,
    short_workers: Mutex<Option<Vec<JoinHandle<()>>>>,
    connections: Arc<PermitPool>,
    interactive_session: Arc<InteractiveSessionSlot>,
}

impl Drop for RusqliteStorageInner {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        self.short_sender.close();
        if self.lifecycle.load(Ordering::Acquire) == STORAGE_CLOSED {
            return;
        }

        let short_workers = self
            .short_workers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        if short_workers.is_empty() {
            return;
        }
        let connections = Arc::clone(&self.connections);
        let interactive_session = Arc::clone(&self.interactive_session);
        if let Some(control) = interactive_session.begin_shutdown() {
            control.initiate();
        }
        let stack_size = self.config.worker_stack_bytes.get();
        let _ = thread::Builder::new()
            .name("att-sqlite-drop-shutdown".to_owned())
            .stack_size(stack_size)
            .spawn(move || {
                for worker in short_workers {
                    let _ = worker.join();
                }
                interactive_session.wait_until_empty();
                connections.wait_until_empty();
            });
    }
}

/// 共享固定工作池、连接预算与交互会话的 `rusqlite` 生产根。
#[derive(Clone)]
pub(crate) struct RusqliteStorage {
    inner: Arc<RusqliteStorageInner>,
}

impl RusqliteStorage {
    pub(crate) fn start(config: RusqliteStorageConfiguration) -> Result<Self, SqliteRuntimeError> {
        let config = Arc::new(config);
        let connections = PermitPool::new(config.max_open_connections);
        let interactive_session = InteractiveSessionSlot::new();
        let (short_sender, short_receiver) =
            async_channel::bounded(config.short_queue_capacity.get());

        let mut short_workers: Vec<JoinHandle<()>> =
            Vec::with_capacity(config.short_worker_threads.get());
        for index in 0..config.short_worker_threads.get() {
            let worker_receiver = short_receiver.clone();
            let worker_config = Arc::clone(&config);
            let worker_connections = Arc::clone(&connections);
            let worker = match thread::Builder::new()
                .name(format!("att-sqlite-short-{index}"))
                .stack_size(config.worker_stack_bytes.get())
                .spawn(move || run_short_worker(worker_receiver, worker_config, worker_connections))
            {
                Ok(worker) => worker,
                Err(source) => {
                    short_sender.close();
                    for worker in short_workers {
                        let _ = worker.join();
                    }
                    return Err(SqliteRuntimeError::WorkerSpawn {
                        worker: format!("short-{index}"),
                        source,
                    });
                }
            };
            short_workers.push(worker);
        }

        Ok(Self {
            inner: Arc::new(RusqliteStorageInner {
                config,
                accepting: AtomicBool::new(true),
                lifecycle: AtomicU8::new(STORAGE_RUNNING),
                short_sender,
                short_workers: Mutex::new(Some(short_workers)),
                connections,
                interactive_session,
            }),
        })
    }

    fn ensure_accepting(&self) -> Result<(), SqliteRuntimeError> {
        if self.inner.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(SqliteRuntimeError::Closed)
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), SqliteRuntimeError> {
        match self.inner.lifecycle.compare_exchange(
            STORAGE_RUNNING,
            STORAGE_SHUTTING_DOWN,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(STORAGE_CLOSED) => return Ok(()),
            Err(_) => {
                return Err(SqliteRuntimeError::Internal(
                    "SQLite shutdown 已经在另一调用中进行",
                ));
            }
        }
        self.inner.accepting.store(false, Ordering::Release);
        self.inner.short_sender.close();
        if let Some(control) = self.inner.interactive_session.begin_shutdown() {
            control.initiate();
        }
        let short_workers = self
            .inner
            .short_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let inner = Arc::clone(&self.inner);
        let (response, receiver) = oneshot::channel();
        thread::Builder::new()
            .name("att-sqlite-shutdown".to_owned())
            .stack_size(self.inner.config.worker_stack_bytes.get())
            .spawn(move || {
                let mut panic = false;
                for worker in short_workers {
                    panic |= worker.join().is_err();
                }
                inner.interactive_session.wait_until_empty();
                inner.connections.wait_until_empty();
                inner.lifecycle.store(STORAGE_CLOSED, Ordering::Release);
                let result = if panic {
                    Err(SqliteRuntimeError::WorkerPanicked("shutdown"))
                } else {
                    Ok(())
                };
                let _ = response.send(result);
            })
            .map_err(|source| SqliteRuntimeError::WorkerSpawn {
                worker: "shutdown".to_owned(),
                source,
            })?;
        receiver
            .await
            .unwrap_or(Err(SqliteRuntimeError::WorkerPanicked("shutdown")))
    }
}

impl SqliteDatabaseCreator for RusqliteStorage {
    type Error = SqliteRuntimeError;

    fn create_new_database(
        &self,
        path: PathBuf,
        commands: Vec<SqliteCommand>,
    ) -> impl Future<Output = Result<(), CreateDatabaseError<Self::Error>>> + Send {
        let sender = self.inner.short_sender.clone();
        let accepting = self.ensure_accepting();
        async move {
            accepting.map_err(CreateDatabaseError::NotCreated)?;
            let (response, receiver) = oneshot::channel();
            sender
                .send(ShortJob::Create {
                    path,
                    commands,
                    response,
                    #[cfg(test)]
                    panic_after_operation: false,
                })
                .await
                .map_err(|_| CreateDatabaseError::NotCreated(SqliteRuntimeError::Closed))?;
            receiver
                .await
                .unwrap_or(Err(CreateDatabaseError::OutcomeUnknown(
                    SqliteRuntimeError::WorkerPanicked("短操作"),
                )))
        }
    }
}

impl SqliteDatabaseSnapshotter for RusqliteStorage {
    type Error = SqliteRuntimeError;

    fn snapshot_database(
        &self,
        source: PathBuf,
        destination: PathBuf,
    ) -> impl Future<Output = Result<(), SnapshotDatabaseError<Self::Error>>> + Send {
        let sender = self.inner.short_sender.clone();
        let accepting = self.ensure_accepting();
        let configured_connection_capacity = self.inner.config.max_open_connections.get();
        async move {
            accepting.map_err(SnapshotDatabaseError::NotCreated)?;
            if configured_connection_capacity < 2 {
                return Err(SnapshotDatabaseError::NotCreated(
                    SqliteRuntimeError::InsufficientConnectionCapacity {
                        operation: "online backup",
                        required: 2,
                        configured: configured_connection_capacity,
                    },
                ));
            }
            let (response, receiver) = oneshot::channel();
            sender
                .send(ShortJob::Snapshot {
                    source,
                    destination,
                    response,
                    #[cfg(test)]
                    panic_after_operation: false,
                })
                .await
                .map_err(|_| SnapshotDatabaseError::NotCreated(SqliteRuntimeError::Closed))?;
            receiver
                .await
                .unwrap_or(Err(SnapshotDatabaseError::OutcomeUnknown(
                    SqliteRuntimeError::WorkerPanicked("短操作"),
                )))
        }
    }
}

impl SqliteQueryExecutor for RusqliteStorage {
    type Error = SqliteRuntimeError;

    fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
    {
        let sender = self.inner.short_sender.clone();
        let accepting = self.ensure_accepting();
        async move {
            accepting.map_err(QueryExistingDatabaseError::QueryFailed)?;
            let (response, receiver) = oneshot::channel();
            sender
                .send(ShortJob::Query {
                    path,
                    query,
                    response,
                })
                .await
                .map_err(|_| QueryExistingDatabaseError::QueryFailed(SqliteRuntimeError::Closed))?;
            receiver
                .await
                .unwrap_or(Err(QueryExistingDatabaseError::QueryFailed(
                    SqliteRuntimeError::WorkerPanicked("短操作"),
                )))
        }
    }

    fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> impl Future<Output = Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>>> + Send
    {
        let sender = self.inner.short_sender.clone();
        let accepting = self.ensure_accepting();
        async move {
            accepting.map_err(QueryExistingDatabaseError::QueryFailed)?;
            let (response, receiver) = oneshot::channel();
            sender
                .send(ShortJob::QuerySnapshot {
                    path,
                    queries,
                    response,
                })
                .await
                .map_err(|_| QueryExistingDatabaseError::QueryFailed(SqliteRuntimeError::Closed))?;
            receiver
                .await
                .unwrap_or(Err(QueryExistingDatabaseError::QueryFailed(
                    SqliteRuntimeError::WorkerPanicked("短操作"),
                )))
        }
    }
}

impl SqliteTransactionExecutor for RusqliteStorage {
    type Error = SqliteRuntimeError;

    fn execute_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
        let sender = self.inner.short_sender.clone();
        let accepting = self.ensure_accepting();
        async move {
            accepting.map_err(ExecuteTransactionError::NotCommitted)?;
            let (response, receiver) = oneshot::channel();
            sender
                .send(ShortJob::Transaction {
                    path,
                    plan,
                    response,
                    #[cfg(test)]
                    panic_after_operation: false,
                })
                .await
                .map_err(|_| ExecuteTransactionError::NotCommitted(SqliteRuntimeError::Closed))?;
            receiver
                .await
                .unwrap_or(Err(ExecuteTransactionError::OutcomeUnknown(
                    SqliteRuntimeError::WorkerPanicked("短操作"),
                )))
        }
    }
}

impl SqliteInteractiveSessionFactory for RusqliteStorage {
    type Operations = RusqliteInteractiveSessionOperations;
    type Finalizer = RusqliteInteractiveSessionFinalizer;
    type Error = SqliteRuntimeError;

    fn open_existing(
        &self,
        path: PathBuf,
    ) -> impl Future<
        Output = Result<
            OpenedSqliteInteractiveSession<Self::Operations, Self::Finalizer>,
            OpenSqliteInteractiveSessionError<Self::Error>,
        >,
    > + Send {
        let accepting = self.ensure_accepting();
        let config = Arc::clone(&self.inner.config);
        let connections = Arc::clone(&self.inner.connections);
        let slot = Arc::clone(&self.inner.interactive_session);
        async move {
            accepting.map_err(OpenSqliteInteractiveSessionError::OpenFailed)?;
            slot.begin_open()
                .map_err(OpenSqliteInteractiveSessionError::OpenFailed)?;
            let (response, receiver) = oneshot::channel();
            let worker_slot = Arc::clone(&slot);
            if let Err(source) = thread::Builder::new()
                .name("att-sqlite-open".to_owned())
                .stack_size(config.worker_stack_bytes.get())
                .spawn(move || {
                    let result = match catch_unwind(AssertUnwindSafe(|| {
                        open_interactive_session(
                            &path,
                            config,
                            &connections,
                            Arc::clone(&worker_slot),
                        )
                    })) {
                        Ok(result) => {
                            if result.is_err() {
                                worker_slot.abort_open();
                            }
                            result
                        }
                        Err(_) => {
                            if let Some(control) = worker_slot.recover_open_panic() {
                                control.initiate();
                            }
                            Err(OpenSqliteInteractiveSessionError::OpenFailed(
                                SqliteRuntimeError::WorkerPanicked("交互式打开"),
                            ))
                        }
                    };
                    let _ = response.send(result);
                })
            {
                slot.abort_open();
                return Err(OpenSqliteInteractiveSessionError::OpenFailed(
                    SqliteRuntimeError::WorkerSpawn {
                        worker: "interactive-open".to_owned(),
                        source,
                    },
                ));
            }
            receiver
                .await
                .unwrap_or(Err(OpenSqliteInteractiveSessionError::OpenFailed(
                    SqliteRuntimeError::WorkerPanicked("交互式打开"),
                )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    use crate::storage::sqlite::SqliteBatch;
    use futures_util::task::noop_waker_ref;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "att-rusqlite-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).expect("测试目录应可创建");
            Self(path)
        }

        fn database(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试值必须非零")
    }

    async fn wait_until(description: &'static str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等待{description}超时"));
    }

    fn enqueue_execute_after_gate(
        operations: &Arc<RusqliteInteractiveSessionOperations>,
        command: SqliteCommand,
    ) -> (
        oneshot::Receiver<InteractiveExecuteResult>,
        mpsc::Receiver<()>,
        mpsc::SyncSender<()>,
    ) {
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let (response_sender, response_receiver) = oneshot::channel();
        operations
            .commands
            .try_send(InteractiveCommand::ExecuteAfterGate {
                command,
                entered: entered_sender,
                release: release_receiver,
                response: response_sender,
            })
            .unwrap_or_else(|_| panic!("首条门控命令必须立即进入空队列"));
        (response_receiver, entered_receiver, release_sender)
    }

    fn configuration_with_max_open_connections(
        max_open_connections: usize,
    ) -> RusqliteStorageConfiguration {
        RusqliteStorageConfiguration::new(
            nonzero(2),
            nonzero(8),
            nonzero(max_open_connections),
            nonzero(1024 * 1024),
            nonzero(64 * 1024),
            nonzero(64 * 1024),
            nonzero(100),
            nonzero(1024 * 1024),
            Duration::from_secs(2),
            SqliteJournalMode::Delete,
            SqliteSynchronous::Full,
        )
        .expect("测试配置应合法")
    }

    fn configuration() -> RusqliteStorageConfiguration {
        configuration_with_max_open_connections(4)
    }

    fn schema_commands() -> Vec<SqliteCommand> {
        vec![SqliteCommand::new(
            "CREATE TABLE values_table (id INTEGER PRIMARY KEY, n INTEGER, r REAL, t TEXT, b BLOB, z BLOB)",
            Vec::new(),
        )]
    }

    #[test]
    fn read_write_policy_enables_foreign_key_constraints() {
        let connection = Connection::open_in_memory().expect("内存数据库应可打开");
        apply_read_write_policy(&connection, &configuration()).expect("读写策略应可应用");

        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("应可读取 foreign_keys 状态");
        assert_eq!(foreign_keys, 1);

        connection
            .execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY);\
                 CREATE TABLE child (parent_id INTEGER REFERENCES parent(id));",
            )
            .expect("外键测试表应可创建");
        let error = connection
            .execute("INSERT INTO child (parent_id) VALUES (1)", [])
            .expect_err("不存在的父行必须触发外键约束");
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
    }

    #[test]
    fn read_only_policy_enables_foreign_key_constraints() {
        let directory = TestDirectory::new();
        let database = directory.database("read-only-foreign-keys.db");
        Connection::open(&database)
            .expect("测试数据库应可创建")
            .close()
            .expect("测试数据库应可关闭");

        let connection = match open_existing_read_only(&database, &configuration()) {
            Ok(connection) => connection,
            Err(_) => panic!("现存数据库应可只读打开"),
        };
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("应可读取 foreign_keys 状态");
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn multi_query_read_uses_one_consistent_wal_snapshot() {
        let directory = TestDirectory::new();
        let database = directory.database("read-snapshot-wal.db");
        let writer_path = database.clone();
        let initializer = Connection::open(&database).expect("测试数据库应可创建");
        initializer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("测试数据库应可启用 WAL");
        initializer
            .execute_batch(
                "CREATE TABLE snapshot_value (value INTEGER NOT NULL);\
                 INSERT INTO snapshot_value VALUES (1);",
            )
            .expect("测试数据应可建立");
        drop(initializer);

        let reader = open_existing_read_only(&database, &configuration())
            .unwrap_or_else(|_| panic!("测试数据库应可只读打开"));
        let (start_writer, writer_started) = mpsc::sync_channel(0);
        let (writer_finished, wait_writer) = mpsc::sync_channel(0);
        let writer = thread::spawn(move || {
            writer_started.recv().expect("应通知 writer 开始");
            let connection = Connection::open(writer_path).expect("writer 应可打开数据库");
            connection
                .execute("UPDATE snapshot_value SET value = 2", [])
                .expect("WAL writer 应可在 reader 快照期间提交");
            writer_finished.send(()).expect("应通知 writer 完成");
        });

        let queries = [
            SqliteQuery::new("SELECT value FROM snapshot_value", Vec::new()),
            SqliteQuery::new("SELECT value FROM snapshot_value", Vec::new()),
        ];
        let results =
            read_queries_in_snapshot(&reader, &queries, &configuration(), |completed_index| {
                if completed_index == 0 {
                    start_writer.send(()).expect("应释放 writer");
                    wait_writer.recv().expect("应等待 writer 提交");
                }
            })
            .expect("两条查询应共享同一 WAL 视图");
        writer.join().expect("writer 不应 panic");

        let expected = vec![SqliteRow::new(vec![SqliteValue::Integer(1)])];
        assert_eq!(results, [expected.clone(), expected]);
        let current: i64 = Connection::open(&database)
            .expect("应可重新打开测试数据库")
            .query_row("SELECT value FROM snapshot_value", [], |row| row.get(0))
            .expect("应可读取 writer 的新值");
        assert_eq!(current, 2);
    }

    #[test]
    fn multi_query_read_rolls_back_query_failure_and_reports_commit_failure_as_read_error() {
        let connection = Connection::open_in_memory().expect("内存数据库应可打开");
        let query_error = read_queries_in_snapshot(
            &connection,
            &[
                SqliteQuery::new("SELECT 1", Vec::new()),
                SqliteQuery::new("SELECT missing FROM absent", Vec::new()),
            ],
            &configuration(),
            |_| {},
        )
        .expect_err("第二条查询失败必须终止快照");
        assert!(connection.is_autocommit(), "查询失败后必须结束读事务");
        assert!(matches!(
            query_error,
            SqliteRuntimeError::Driver {
                operation: "准备查询",
                ..
            }
        ));

        let commit_error = read_queries_in_snapshot(
            &connection,
            &[SqliteQuery::new("SELECT 1", Vec::new())],
            &configuration(),
            |_| {
                connection
                    .execute_batch("ROLLBACK")
                    .expect("测试注入应先结束读事务")
            },
        )
        .expect_err("被测试注入提前结束的事务必须令 COMMIT 失败");
        assert!(connection.is_autocommit());
        assert!(matches!(
            commit_error,
            SqliteRuntimeError::Driver {
                operation: "结束只读查询快照",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn multi_query_read_preserves_order_and_applies_each_query_budget() {
        let directory = TestDirectory::new();
        let database = directory.database("read-snapshot-budget.db");
        Connection::open(&database)
            .expect("测试数据库应可创建")
            .close()
            .expect("测试数据库应可关闭");
        let mut config = configuration();
        config.max_rows_per_query = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("SQLite 根应可启动");

        let results = storage
            .query_existing_database_snapshot(
                database.clone(),
                vec![
                    SqliteQuery::new("SELECT 2", Vec::new()),
                    SqliteQuery::new("SELECT 1", Vec::new()),
                ],
            )
            .await
            .expect("两条查询应分别使用各自的一行预算");
        assert_eq!(
            results,
            [
                vec![SqliteRow::new(vec![SqliteValue::Integer(2)])],
                vec![SqliteRow::new(vec![SqliteValue::Integer(1)])],
            ]
        );

        let error = storage
            .query_existing_database_snapshot(
                database,
                vec![SqliteQuery::new("SELECT 1 UNION ALL SELECT 2", Vec::new())],
            )
            .await
            .expect_err("单条查询自身仍不得超过行预算");
        assert!(matches!(
            error,
            QueryExistingDatabaseError::QueryFailed(SqliteRuntimeError::ResourceLimit {
                resource: "查询行数",
                limit: 1,
            })
        ));
        storage.shutdown().await.expect("SQLite 根应可关闭");
    }

    #[test]
    fn multi_query_read_rejects_empty_or_too_many_queries_and_keeps_per_query_input_budgets() {
        assert!(matches!(
            run_query_snapshot_existing(
                Path::new("C:/不存在/且不应访问.db"),
                &[],
                &configuration(),
            ),
            Err(QueryExistingDatabaseError::QueryFailed(
                SqliteRuntimeError::InvalidValue("只读快照查询集合不得为空")
            ))
        ));

        let queries = (0..=MAX_QUERIES_PER_READ_SNAPSHOT)
            .map(|_| SqliteQuery::new("SELECT 1", Vec::new()))
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_query_snapshot(&queries, &configuration()),
            Err(SqliteRuntimeError::ResourceLimit {
                resource: "只读快照查询数",
                limit: MAX_QUERIES_PER_READ_SNAPSHOT,
            })
        ));

        let mut config = configuration();
        config.max_statement_bytes = nonzero("SELECT 1".len());
        config.max_parameter_bytes = nonzero(4);
        assert!(
            validate_query_snapshot(
                &[
                    SqliteQuery::new("SELECT 1", Vec::new()),
                    SqliteQuery::new("SELECT 2", Vec::new()),
                ],
                &config,
            )
            .is_ok(),
            "多条查询不得聚合收紧每条 statement 的现有预算"
        );
        config.max_statement_bytes = nonzero("SELECT ?1".len());
        assert!(
            validate_query_snapshot(
                &[
                    SqliteQuery::new("SELECT ?1", vec![SqliteValue::Text("1234".to_owned())]),
                    SqliteQuery::new("SELECT ?1", vec![SqliteValue::Text("5678".to_owned())]),
                ],
                &config,
            )
            .is_ok(),
            "多条查询不得聚合收紧每条参数的现有预算"
        );
    }

    #[test]
    fn configuration_accepts_one_connection_for_non_snapshot_operations() {
        let configuration = configuration_with_max_open_connections(1);

        assert_eq!(configuration.max_open_connections, nonzero(1));
    }

    #[tokio::test]
    async fn online_backup_rejects_insufficient_connection_capacity_without_waiting() {
        let directory = TestDirectory::new();
        let source = directory.database("single-connection-source.db");
        let destination = directory.database("single-connection-snapshot.db");
        let storage = RusqliteStorage::start(configuration_with_max_open_connections(1))
            .expect("单连接存储根应可启动");

        let error = tokio::time::timeout(
            Duration::from_millis(100),
            storage.snapshot_database(source, destination.clone()),
        )
        .await
        .expect("连接容量不足必须在入队前立即返回")
        .expect_err("单连接无法执行 online backup");

        assert!(matches!(
            error,
            SnapshotDatabaseError::NotCreated(SqliteRuntimeError::InsufficientConnectionCapacity {
                operation: "online backup",
                required: 2,
                configured: 1,
            })
        ));
        assert!(!destination.exists());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn online_backup_is_create_only_and_copies_one_consistent_database() {
        let directory = TestDirectory::new();
        let source = directory.database("source.db");
        let destination = directory.database("snapshot.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(source.clone(), schema_commands())
            .await
            .expect("源数据库应可创建");
        storage
            .execute_transaction(
                source.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::Execute(
                    SqliteCommand::new(
                        "INSERT INTO values_table (id, t) VALUES (?1, ?2)",
                        vec![
                            SqliteValue::Integer(1),
                            SqliteValue::Text("冻结值".to_owned()),
                        ],
                    ),
                )]),
            )
            .await
            .expect("源数据应可提交");

        storage
            .snapshot_database(source.clone(), destination.clone())
            .await
            .expect("online backup 应成功");
        let rows = storage
            .query_existing_database(
                destination.clone(),
                SqliteQuery::new("SELECT id, t FROM values_table", Vec::new()),
            )
            .await
            .expect("快照应可独立读取");
        assert_eq!(
            rows,
            vec![SqliteRow::new(vec![
                SqliteValue::Integer(1),
                SqliteValue::Text("冻结值".to_owned()),
            ])]
        );

        let error = storage
            .snapshot_database(source, destination)
            .await
            .expect_err("现存目标绝不能被覆盖");
        assert!(matches!(
            error,
            SnapshotDatabaseError::DestinationAlreadyExists
        ));
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn online_backup_reports_missing_source_and_cleans_invalid_copy() {
        let directory = TestDirectory::new();
        let missing = directory.database("missing.db");
        let destination = directory.database("missing-snapshot.db");
        let invalid = directory.database("invalid.db");
        let invalid_destination = directory.database("invalid-snapshot.db");
        fs::write(&invalid, b"not a sqlite database").expect("无效源文件应可创建");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");

        assert!(matches!(
            storage
                .snapshot_database(missing, destination.clone())
                .await,
            Err(SnapshotDatabaseError::SourceNotFound)
        ));
        assert!(!destination.exists());

        let error = storage
            .snapshot_database(invalid, invalid_destination.clone())
            .await
            .expect_err("无效 SQLite 源必须失败");
        assert!(matches!(error, SnapshotDatabaseError::NotCreated(_)));
        assert!(!invalid_destination.exists());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn create_query_and_transaction_preserve_all_sqlite_values() {
        let directory = TestDirectory::new();
        let database = directory.database("中文 数据库.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");

        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::ExecuteMany(
                    SqliteBatch::new(
                        "INSERT INTO values_table (id, n, r, t, b, z) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        vec![vec![
                            SqliteValue::Integer(1),
                            SqliteValue::Integer(-9),
                            SqliteValue::Real(1.25),
                            SqliteValue::Text("文本".to_owned()),
                            SqliteValue::Blob(vec![0, 1, 255]),
                            SqliteValue::Null,
                        ]],
                    ),
                )]),
            )
            .await
            .expect("批量事务应可提交");

        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new(
                    "SELECT n, r, t, b, z FROM values_table ORDER BY id",
                    Vec::new(),
                ),
            )
            .await
            .expect("查询应成功");
        assert_eq!(
            rows,
            vec![SqliteRow::new(vec![
                SqliteValue::Integer(-9),
                SqliteValue::Real(1.25),
                SqliteValue::Text("文本".to_owned()),
                SqliteValue::Blob(vec![0, 1, 255]),
                SqliteValue::Null,
            ])]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn batch_requirements_and_exact_updates_follow_natural_parameter_order() {
        let directory = TestDirectory::new();
        let database = directory.database("ordered-batches.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id, n) VALUES (1, 0)",
                        Vec::new(),
                    )),
                    SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
                        "SELECT 1 FROM values_table WHERE id = ?1 AND n <> ?2",
                        vec![
                            vec![SqliteValue::Integer(1), SqliteValue::Integer(0)],
                            vec![SqliteValue::Integer(2), SqliteValue::Integer(0)],
                        ],
                    )),
                    SqliteTransactionStep::ExecuteManyExactlyOne(SqliteBatch::new(
                        "UPDATE values_table SET n = ?1 WHERE id = 1 AND n = ?2",
                        vec![
                            vec![SqliteValue::Integer(1), SqliteValue::Integer(0)],
                            vec![SqliteValue::Integer(2), SqliteValue::Integer(1)],
                            vec![SqliteValue::Integer(3), SqliteValue::Integer(2)],
                        ],
                    )),
                ]),
            )
            .await
            .expect("批量步骤应按自然参数顺序提交");

        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT n FROM values_table WHERE id = 1", Vec::new()),
            )
            .await
            .expect("提交后应可查询");
        assert_eq!(rows, vec![SqliteRow::new(vec![SqliteValue::Integer(3)])]);
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn require_no_rows_many_reports_the_earliest_result_and_rolls_back() {
        let directory = TestDirectory::new();
        let database = directory.database("ordered-requirement.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::Execute(
                    SqliteCommand::new(
                        "INSERT INTO values_table (id, n) VALUES (1, 0)",
                        Vec::new(),
                    ),
                )]),
            )
            .await
            .expect("基础行应可提交");

        let result = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "UPDATE values_table SET n = 9 WHERE id = 1",
                        Vec::new(),
                    )),
                    SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
                        "SELECT CASE ?1 WHEN 1 THEN 1 ELSE abs(-9223372036854775808) END",
                        vec![vec![SqliteValue::Integer(1)], vec![SqliteValue::Integer(2)]],
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::RequirementFailed)
        ));

        let rows = storage
            .query_existing_database(
                database.clone(),
                SqliteQuery::new("SELECT n FROM values_table WHERE id = 1", Vec::new()),
            )
            .await
            .expect("回滚后应可查询");
        assert_eq!(rows, vec![SqliteRow::new(vec![SqliteValue::Integer(0)])]);

        let result = storage
            .execute_transaction(
                database,
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::RequireNoRowsMany(
                    SqliteBatch::new(
                        "SELECT CASE ?1 WHEN 1 THEN 1 ELSE abs(-9223372036854775808) END",
                        vec![vec![SqliteValue::Integer(2)], vec![SqliteValue::Integer(1)]],
                    ),
                )]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::NotCommitted(
                SqliteRuntimeError::Driver { .. }
            ))
        ));
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn execute_many_exactly_one_rejects_zero_or_multiple_rows_and_rolls_back() {
        let directory = TestDirectory::new();
        let database = directory.database("exactly-one-count.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::ExecuteMany(
                    SqliteBatch::new(
                        "INSERT INTO values_table (id, n) VALUES (?1, 0)",
                        vec![vec![SqliteValue::Integer(1)], vec![SqliteValue::Integer(2)]],
                    ),
                )]),
            )
            .await
            .expect("基础行应可提交");

        let zero_rows = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::ExecuteManyExactlyOne(SqliteBatch::new(
                        "UPDATE values_table SET n = ?1 WHERE id = ?2",
                        vec![
                            vec![SqliteValue::Integer(7), SqliteValue::Integer(1)],
                            vec![SqliteValue::Integer(8), SqliteValue::Integer(99)],
                        ],
                    )),
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "UPDATE values_table SET n = 10 WHERE id = 2",
                        Vec::new(),
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            zero_rows,
            Err(ExecuteTransactionError::RequirementFailed)
        ));

        let multiple_rows = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::ExecuteManyExactlyOne(SqliteBatch::new(
                        "UPDATE values_table SET n = ?1 WHERE n = ?2",
                        vec![vec![SqliteValue::Integer(3), SqliteValue::Integer(0)]],
                    )),
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "UPDATE values_table SET n = 10 WHERE id = 2",
                        Vec::new(),
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            multiple_rows,
            Err(ExecuteTransactionError::RequirementFailed)
        ));

        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id, n FROM values_table ORDER BY id", Vec::new()),
            )
            .await
            .expect("两笔失败事务后应可查询");
        assert_eq!(
            rows,
            vec![
                SqliteRow::new(vec![SqliteValue::Integer(1), SqliteValue::Integer(0)]),
                SqliteRow::new(vec![SqliteValue::Integer(2), SqliteValue::Integer(0)]),
            ]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn execute_many_exactly_one_driver_error_rolls_back_earlier_parameter_sets() {
        let directory = TestDirectory::new();
        let database = directory.database("exactly-one-driver.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::ExecuteMany(
                    SqliteBatch::new(
                        "INSERT INTO values_table (id) VALUES (?1)",
                        vec![vec![SqliteValue::Integer(1)], vec![SqliteValue::Integer(2)]],
                    ),
                )]),
            )
            .await
            .expect("基础行应可提交");

        let result = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::ExecuteManyExactlyOne(SqliteBatch::new(
                        "UPDATE values_table SET id = ?1 WHERE id = ?2",
                        vec![
                            vec![SqliteValue::Integer(3), SqliteValue::Integer(1)],
                            vec![SqliteValue::Integer(2), SqliteValue::Integer(3)],
                        ],
                    )),
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id) VALUES (4)",
                        Vec::new(),
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::NotCommitted(
                SqliteRuntimeError::Driver { .. }
            ))
        ));

        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table ORDER BY id", Vec::new()),
            )
            .await
            .expect("驱动失败回滚后应可查询");
        assert_eq!(
            rows,
            vec![
                SqliteRow::new(vec![SqliteValue::Integer(1)]),
                SqliteRow::new(vec![SqliteValue::Integer(2)]),
            ]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[test]
    fn requirement_rollback_failure_is_outcome_unknown() {
        let connection = Connection::open_in_memory().expect("内存数据库应可打开");

        let result = rollback_requirement_failure(&connection);

        assert!(matches!(
            result,
            Err(ExecuteTransactionError::OutcomeUnknown(
                SqliteRuntimeError::Driver { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn missing_query_never_creates_a_database() {
        let directory = TestDirectory::new();
        let database = directory.database("missing.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");

        let result = storage
            .query_existing_database(database.clone(), SqliteQuery::new("SELECT 1", Vec::new()))
            .await;
        assert!(matches!(result, Err(QueryExistingDatabaseError::NotFound)));
        assert!(!database.exists());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn statement_and_query_limits_reject_instead_of_truncating() {
        let directory = TestDirectory::new();
        let rejected_database = directory.database("statement-limit.db");
        let mut statement_config = configuration();
        statement_config.max_statement_bytes = nonzero(8);
        let statement_storage = RusqliteStorage::start(statement_config).expect("根应可启动");
        let rejected = statement_storage
            .create_new_database(rejected_database.clone(), schema_commands())
            .await;
        assert!(matches!(
            rejected,
            Err(CreateDatabaseError::NotCreated(
                SqliteRuntimeError::ResourceLimit {
                    resource: "statement 字节数",
                    ..
                }
            ))
        ));
        assert!(!rejected_database.exists(), "请求校验必须早于建库副作用");
        statement_storage.shutdown().await.expect("根应可关闭");

        let database = directory.database("row-limit.db");
        let mut row_config = configuration();
        row_config.max_rows_per_query = nonzero(1);
        let row_storage = RusqliteStorage::start(row_config).expect("根应可启动");
        let mut commands = schema_commands();
        commands.push(SqliteCommand::new(
            "INSERT INTO values_table (id) VALUES (1)",
            Vec::new(),
        ));
        commands.push(SqliteCommand::new(
            "INSERT INTO values_table (id) VALUES (2)",
            Vec::new(),
        ));
        row_storage
            .create_new_database(database.clone(), commands)
            .await
            .expect("数据库应可创建");
        let rows = row_storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table ORDER BY id", Vec::new()),
            )
            .await;
        assert!(matches!(
            rows,
            Err(QueryExistingDatabaseError::QueryFailed(
                SqliteRuntimeError::ResourceLimit {
                    resource: "查询行数",
                    ..
                }
            ))
        ));
        row_storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn concurrent_create_only_has_exactly_one_winner() {
        let directory = TestDirectory::new();
        let database = directory.database("race.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");

        let (first, second) = tokio::join!(
            storage.create_new_database(database.clone(), schema_commands()),
            storage.create_new_database(database.clone(), schema_commands()),
        );
        let successes = usize::from(first.is_ok()) + usize::from(second.is_ok());
        let already_exists = usize::from(matches!(first, Err(CreateDatabaseError::AlreadyExists)))
            + usize::from(matches!(second, Err(CreateDatabaseError::AlreadyExists)));
        assert_eq!(successes, 1);
        assert_eq!(already_exists, 1);
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn failed_creation_removes_main_file_and_all_sidecars() {
        let directory = TestDirectory::new();
        let database = directory.database("cleanup.db");
        let mut config = configuration();
        config.journal_mode = SqliteJournalMode::Wal;
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        let mut commands = schema_commands();
        commands.push(SqliteCommand::new("INVALID SQL", Vec::new()));

        let result = storage
            .create_new_database(database.clone(), commands)
            .await;
        assert!(
            matches!(&result, Err(CreateDatabaseError::NotCreated(_))),
            "实际结果：{result:?}"
        );
        for path in [
            database.clone(),
            sidecar_path(&database, "-journal"),
            sidecar_path(&database, "-wal"),
            sidecar_path(&database, "-shm"),
        ] {
            assert!(!path.exists(), "创建失败后不得残留 {}", path.display());
        }
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn preexisting_sidecar_is_never_claimed_or_deleted_by_database_creation() {
        let directory = TestDirectory::new();
        let database = directory.database("foreign-sidecar.db");
        let foreign = sidecar_path(&database, "-wal");
        fs::write(&foreign, b"foreign").expect("外来伴生文件应可建立");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");

        let result = storage
            .create_new_database(database.clone(), schema_commands())
            .await;
        assert!(
            matches!(
                &result,
            Err(CreateDatabaseError::ResidualArtifact(
                SqliteRuntimeError::UnexpectedArtifact { path }
            )) if path.file_name() == foreign.file_name()
            ),
            "实际结果：{result:?}"
        );
        assert!(!database.exists(), "本次占有的主文件应按身份清理");
        assert_eq!(fs::read(&foreign).expect("外来文件不得被删除"), b"foreign");
        storage.shutdown().await.expect("根应可关闭");
    }

    #[test]
    fn cleanup_refuses_a_path_replaced_after_identity_was_captured() {
        let directory = TestDirectory::new();
        let database = directory.database("replaced-cleanup.db");
        fs::write(&database, b"owned").expect("本次文件应可建立");
        let pinned = pin_path_without_reparse(&database).expect("本次文件应可固定");
        let identity = FileIdentity::of(pinned.file(), &database).expect("物理身份应可读取");
        drop(pinned);
        fs::remove_file(&database).expect("应可模拟外部替换");
        fs::write(&database, b"foreign").expect("外来替换文件应可建立");
        let paths = database_artifact_paths(&database);
        let assessment = cleanup_database_artifacts(
            &paths,
            &[TrackedDatabaseArtifact {
                path: database.clone(),
                identity,
                is_main: true,
            }],
            CleanupAssessment::clean(),
        );

        assert!(assessment.unknown, "身份变化后不得声称已知清理结果");
        assert!(assessment.residual, "外来替换对象应作为残留可见");
        assert_eq!(
            fs::read(database).expect("外来替换对象不得被误删"),
            b"foreign"
        );
    }

    #[tokio::test]
    async fn busy_timeout_returns_known_not_committed_state() {
        let directory = TestDirectory::new();
        let database = directory.database("busy.db");
        let mut config = configuration();
        config.busy_timeout = Duration::from_millis(25);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let blocker = Connection::open(&database).expect("测试锁连接应可打开");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("测试连接应可占有写锁");

        let result = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![SqliteTransactionStep::Execute(
                    SqliteCommand::new("INSERT INTO values_table (id) VALUES (1)", Vec::new()),
                )]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::NotCommitted(
                SqliteRuntimeError::Driver { .. }
            ))
        ));
        blocker.execute_batch("ROLLBACK").expect("测试锁应可释放");
        drop(blocker);
        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table", Vec::new()),
            )
            .await
            .expect("解锁后应可查询");
        assert!(rows.is_empty());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn requirement_failure_stops_later_steps_and_rolls_back() {
        let directory = TestDirectory::new();
        let database = directory.database("requirement.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");

        let result = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id) VALUES (1)",
                        Vec::new(),
                    )),
                    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
                        "SELECT 1 FROM values_table WHERE id = 1",
                        Vec::new(),
                    )),
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id) VALUES (2)",
                        Vec::new(),
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::RequirementFailed)
        ));
        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table", Vec::new()),
            )
            .await
            .expect("回滚后应可查询");
        assert!(rows.is_empty());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn unique_finalizer_rolls_back_active_transaction_and_closes_actor() {
        let directory = TestDirectory::new();
        let database = directory.database("interactive.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");

        let opened = storage
            .open_existing(database.clone())
            .await
            .expect("交互会话应可打开");
        let (operations, finalizer) = opened.into_parts();
        operations.begin().await.expect("应可开始事务");
        operations
            .execute(SqliteCommand::new(
                "INSERT INTO values_table (id, t) VALUES (?1, ?2)",
                vec![
                    SqliteValue::Integer(1),
                    SqliteValue::Text("未提交".to_owned()),
                ],
            ))
            .await
            .expect("应在同一事务中执行");
        let report = finalizer.finalize().await.expect("会话应可终结");
        assert!(report.had_unclosed_transaction());
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table", Vec::new()),
            )
            .await
            .expect("终结后应可重新查询");
        assert!(rows.is_empty());
        storage.shutdown().await.expect("根应可关闭");
    }

    #[test]
    fn failed_operation_that_crosses_autocommit_boundary_poisons_the_session() {
        let connection = Connection::open_in_memory().expect("内存数据库应可打开");
        let lifecycle = AtomicU8::new(SESSION_OPEN);
        let mut transaction = InteractiveTransactionState::Idle;
        let before_autocommit = connection.is_autocommit();
        connection
            .execute_batch("BEGIN DEFERRED")
            .expect("测试事务应可开始");

        let result = observe_operation::<()>(
            &connection,
            &mut transaction,
            &lifecycle,
            before_autocommit,
            Err(SqliteRuntimeError::Internal("测试不确定结果")),
        );

        assert!(matches!(
            result,
            Err(SqliteInteractiveSessionError::OutcomeUnknown(_))
        ));
        assert!(matches!(
            transaction,
            InteractiveTransactionState::Indeterminate
        ));
        assert_eq!(lifecycle.load(Ordering::Acquire), SESSION_INDETERMINATE);
        assert!(matches!(
            session_unavailable::<()>(&lifecycle),
            Some(Err(SqliteInteractiveSessionError::Indeterminate))
        ));
        connection
            .execute_batch("ROLLBACK")
            .expect("测试事务应可清理");
    }

    #[test]
    fn finalization_error_preserves_the_primary_and_connection_close_failures() {
        let error = SqliteInteractiveSessionFinalizationError::new(
            SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
                SqliteRuntimeError::Internal("回滚结果未知"),
            ),
            Some(SqliteRuntimeError::Internal("关闭失败")),
        );

        assert!(
            Error::source(&error)
                .expect("主失败应保留在错误链")
                .to_string()
                .contains("回滚结果未知")
        );
        let message = error.to_string();
        assert!(message.contains("回滚结果未知"));
        assert!(message.contains("关闭失败"));
    }

    #[tokio::test]
    async fn accepted_short_operation_survives_future_drop() {
        let directory = TestDirectory::new();
        let database = directory.database("accepted-drop.db");
        let mut config = configuration();
        config.short_worker_threads = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        let mut future = Box::pin(storage.create_new_database(
            database.clone(),
            vec![SqliteCommand::new(
                "CREATE TABLE accepted_result AS WITH RECURSIVE counter(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM counter WHERE x < 2000000) SELECT max(x) AS value FROM counter",
                Vec::new(),
            )],
        ));
        let mut context = Context::from_waker(noop_waker_ref());
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        drop(future);

        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT value FROM accepted_result", Vec::new()),
            )
            .await
            .expect("已入队建库的 Future 丢弃后仍应完成");
        assert_eq!(
            rows,
            vec![SqliteRow::new(vec![SqliteValue::Integer(2_000_000)])]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn accepted_create_worker_panic_after_side_effect_is_outcome_unknown() {
        let directory = TestDirectory::new();
        let database = directory.database("create-panic-after-effect.db");
        let mut config = configuration();
        config.short_worker_threads = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        let (response, receiver) = oneshot::channel();
        storage
            .inner
            .short_sender
            .send(ShortJob::Create {
                path: database.clone(),
                commands: schema_commands(),
                response,
                panic_after_operation: true,
            })
            .await
            .expect("建库任务应被 worker 接管");
        let result = receiver.await.expect("worker 应返回 panic 终态");
        assert!(matches!(
            result,
            Err(CreateDatabaseError::OutcomeUnknown(
                SqliteRuntimeError::WorkerPanicked("短操作")
            ))
        ));
        let connection = Connection::open(database).expect("panic 前建立的数据库应依然存在");
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'values_table'",
                [],
                |row| row.get(0),
            )
            .expect("应可观测 panic 前的建库副作用");
        assert_eq!(table_count, 1);
        drop(connection);
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn accepted_transaction_worker_panic_after_commit_is_outcome_unknown() {
        let directory = TestDirectory::new();
        let database = directory.database("transaction-panic-after-effect.db");
        let mut config = configuration();
        config.short_worker_threads = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let (response, receiver) = oneshot::channel();
        storage
            .inner
            .short_sender
            .send(ShortJob::Transaction {
                path: database.clone(),
                plan: SqliteTransactionPlan::new(vec![SqliteTransactionStep::Execute(
                    SqliteCommand::new(
                        "INSERT INTO values_table (id, t) VALUES (1, 'committed')",
                        Vec::new(),
                    ),
                )]),
                response,
                panic_after_operation: true,
            })
            .await
            .expect("短事务应被 worker 接管");
        let result = receiver.await.expect("worker 应返回 panic 终态");
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::OutcomeUnknown(
                SqliteRuntimeError::WorkerPanicked("短操作")
            ))
        ));
        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT t FROM values_table WHERE id = 1", Vec::new()),
            )
            .await
            .expect("应可观测 panic 前的提交副作用");
        assert_eq!(
            rows,
            vec![SqliteRow::new(vec![SqliteValue::Text(
                "committed".to_owned()
            )])]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn finalizer_bypasses_full_command_queue_and_drains_accepted_commands() {
        let directory = TestDirectory::new();
        let database = directory.database("full-command-queue.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(database.clone())
            .await
            .expect("交互会话应可打开");
        let (operations, finalizer) = opened.into_parts();

        let (first_response, first_entered, first_release) = enqueue_execute_after_gate(
            &operations,
            SqliteCommand::new("INSERT INTO values_table (id, n) VALUES (1, 1)", Vec::new()),
        );
        let mut context = Context::from_waker(noop_waker_ref());
        first_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("交互式 actor 必须接管首条门控命令");
        assert!(operations.commands.is_empty());

        let second_operations = Arc::clone(&operations);
        let mut second_future = Box::pin(async move {
            second_operations
                .execute(SqliteCommand::new(
                    "INSERT INTO values_table (id, n) VALUES (2, 2)",
                    Vec::new(),
                ))
                .await
        });
        assert!(matches!(
            second_future.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(operations.commands.len(), 1, "第二条命令应已被有界队列接管");
        let second = tokio::spawn(second_future);

        let finalization = finalizer.finalize();
        first_release.send(()).expect("必须释放首条门控命令");
        let report = tokio::time::timeout(Duration::from_secs(20), finalization)
            .await
            .expect("队列填满不得阻断独立终结通道")
            .expect("会话应可终结");
        first_response
            .await
            .expect("交互式 actor 必须返回首条命令结果")
            .expect("第一条已接管命令应完成");
        second
            .await
            .expect("第二个调用任务不应 panic")
            .expect("第二条已接管命令应完成");
        assert!(!report.had_unclosed_transaction());
        let rows = storage
            .query_existing_database(
                database,
                SqliteQuery::new("SELECT id FROM values_table ORDER BY id", Vec::new()),
            )
            .await
            .expect("终结后应可查询已接管结果");
        assert_eq!(
            rows,
            vec![
                SqliteRow::new(vec![SqliteValue::Integer(1)]),
                SqliteRow::new(vec![SqliteValue::Integer(2)]),
            ]
        );
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn shutdown_finalizes_a_held_idle_session_and_preserves_its_report() {
        let directory = TestDirectory::new();
        let database = directory.database("held-idle-session.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database, schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(directory.database("held-idle-session.db"))
            .await
            .expect("交互会话应可打开");
        let (operations, finalizer) = opened.into_parts();

        tokio::time::timeout(Duration::from_secs(5), storage.shutdown())
            .await
            .expect("持有会话不得使 shutdown 永久等待")
            .expect("根应可关闭");
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
        let report = tokio::time::timeout(Duration::from_secs(5), finalizer.finalize())
            .await
            .expect("shutdown 后仍应可领取唯一终结报告")
            .expect("会话应可终结");
        assert!(!report.had_unclosed_transaction());
    }

    #[tokio::test]
    async fn shutdown_rolls_back_a_held_active_transaction() {
        let directory = TestDirectory::new();
        let database = directory.database("held-active-session.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(database.clone())
            .await
            .expect("交互会话应可打开");
        let (operations, finalizer) = opened.into_parts();
        operations.begin().await.expect("事务应可开始");
        operations
            .execute(SqliteCommand::new(
                "INSERT INTO values_table (id, t) VALUES (1, 'uncommitted')",
                Vec::new(),
            ))
            .await
            .expect("未提交写入应可执行");

        tokio::time::timeout(Duration::from_secs(5), storage.shutdown())
            .await
            .expect("活动事务不得使 shutdown 永久等待")
            .expect("根应可关闭");
        let report = finalizer.finalize().await.expect("会话应可终结");
        assert!(report.had_unclosed_transaction());
        let connection = Connection::open(database).expect("终结后数据库应可重新打开");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM values_table", [], |row| row.get(0))
            .expect("应可检查回滚结果");
        assert_eq!(count, 0, "shutdown 必须回滚活动事务");
        drop(operations);
    }

    #[tokio::test]
    async fn shutdown_bypasses_a_full_session_queue_and_rejects_unaccepted_commands() {
        let directory = TestDirectory::new();
        let database = directory.database("shutdown-full-command-queue.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(database.clone())
            .await
            .expect("交互会话应可打开");
        let (operations, finalizer) = opened.into_parts();

        let (first_response, first_entered, first_release) = enqueue_execute_after_gate(
            &operations,
            SqliteCommand::new("INSERT INTO values_table (id, n) VALUES (1, 1)", Vec::new()),
        );
        let mut context = Context::from_waker(noop_waker_ref());
        first_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("交互式 actor 必须接管首条门控命令");
        assert!(operations.commands.is_empty());

        let second_operations = Arc::clone(&operations);
        let mut second_future = Box::pin(async move {
            second_operations
                .execute(SqliteCommand::new(
                    "INSERT INTO values_table (id, n) VALUES (2, 2)",
                    Vec::new(),
                ))
                .await
        });
        assert!(matches!(
            second_future.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(operations.commands.len(), 1, "第二条命令应已被队列接管");
        let second = tokio::spawn(second_future);

        let third_operations = Arc::clone(&operations);
        let mut third_future = Box::pin(async move {
            third_operations
                .execute(SqliteCommand::new(
                    "INSERT INTO values_table (id, n) VALUES (3, 3)",
                    Vec::new(),
                ))
                .await
        });
        assert!(matches!(
            third_future.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(operations.commands.len(), 1, "第三条命令尚未被队列接管");
        let third = tokio::spawn(third_future);

        let mut shutdown = Box::pin(storage.shutdown());
        assert!(matches!(
            shutdown.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert!(operations.commands.is_closed(), "shutdown 必须关闭命令入口");
        first_release.send(()).expect("必须释放首条门控命令");
        tokio::time::timeout(Duration::from_secs(20), shutdown)
            .await
            .expect("满命令队列不得阻断 shutdown")
            .expect("根应可关闭");
        first_response
            .await
            .expect("交互式 actor 必须返回首条命令结果")
            .expect("第一条已接管命令应完成");
        second
            .await
            .expect("第二个调用任务不应 panic")
            .expect("第二条已接管命令应完成");
        assert!(matches!(
            third.await.expect("第三个调用任务不应 panic"),
            Err(SqliteInteractiveSessionError::Closed)
        ));
        let report = finalizer.finalize().await.expect("会话应可终结");
        assert!(!report.had_unclosed_transaction());
        let connection = Connection::open(database).expect("终结后数据库应可打开");
        let ids = connection
            .prepare("SELECT id FROM values_table ORDER BY id")
            .expect("查询应可准备")
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("行应可读取");
        assert_eq!(ids, vec![1, 2]);
    }

    #[tokio::test]
    async fn single_active_session_rejects_a_second_open_and_allows_reopen_after_finalization() {
        let directory = TestDirectory::new();
        let database = directory.database("single-interactive-session.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let first = storage
            .open_existing(database.clone())
            .await
            .expect("第一个会话应可打开");
        let (operations, finalizer) = first.into_parts();
        assert!(matches!(
            storage.open_existing(database.clone()).await,
            Err(OpenSqliteInteractiveSessionError::OpenFailed(
                SqliteRuntimeError::InteractiveSessionAlreadyOpen
            ))
        ));
        let report = finalizer.finalize().await.expect("第一个会话应可终结");
        assert!(!report.had_unclosed_transaction());
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
        let reopened = storage
            .open_existing(database)
            .await
            .expect("终结后应可打开新会话");
        let (_operations, finalizer) = reopened.into_parts();
        finalizer.finalize().await.expect("新会话应可终结");
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn dropping_the_unique_finalizer_still_closes_the_session_once() {
        let directory = TestDirectory::new();
        let database = directory.database("dropped-finalizer.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(database.clone())
            .await
            .expect("会话应可打开");
        let (operations, finalizer) = opened.into_parts();

        drop(finalizer);
        wait_until("丢弃的终结令牌完成清理", || {
            let state = storage
                .inner
                .interactive_session
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !state.opening && state.active.is_none()
        })
        .await;
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
        let reopened = storage
            .open_existing(database)
            .await
            .expect("丢弃的令牌清理后应可重新打开");
        let (_operations, finalizer) = reopened.into_parts();
        finalizer.finalize().await.expect("新会话应可终结");
        storage.shutdown().await.expect("根应可关闭");
    }

    #[tokio::test]
    async fn shutdown_and_unique_finalizer_can_initiate_the_same_session_concurrently() {
        let directory = TestDirectory::new();
        let database = directory.database("concurrent-finalization.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database, schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(directory.database("concurrent-finalization.db"))
            .await
            .expect("会话应可打开");
        let (_operations, finalizer) = opened.into_parts();
        let finalization = tokio::spawn(finalizer.finalize());
        let shutdown = storage.shutdown();
        let (shutdown_result, report) = tokio::join!(shutdown, finalization);
        shutdown_result.expect("并发终结时根应可关闭");
        let report = report
            .expect("唯一 finalizer 任务不应 panic")
            .expect("会话应可终结");
        assert!(!report.had_unclosed_transaction());
    }

    #[tokio::test]
    async fn finalizer_can_claim_the_report_after_the_last_storage_handle_is_dropped() {
        let directory = TestDirectory::new();
        let database = directory.database("drop-storage-session.db");
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        storage
            .create_new_database(database, schema_commands())
            .await
            .expect("数据库应可创建");
        let opened = storage
            .open_existing(directory.database("drop-storage-session.db"))
            .await
            .expect("会话应可打开");
        let (operations, finalizer) = opened.into_parts();
        drop(storage);

        let report = tokio::time::timeout(Duration::from_secs(5), finalizer.finalize())
            .await
            .expect("最后一个 storage 句柄丢弃后仍应产生终结报告")
            .expect("会话应可终结");
        assert!(!report.had_unclosed_transaction());
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
    }

    #[test]
    fn root_futures_are_send() {
        fn assert_send(_: impl Send) {}

        let directory = TestDirectory::new();
        let storage = RusqliteStorage::start(configuration()).expect("根应可启动");
        assert_send(storage.create_new_database(directory.database("send.db"), schema_commands()));
        assert_send(storage.snapshot_database(
            directory.database("send.db"),
            directory.database("send-copy.db"),
        ));
        assert_send(storage.query_existing_database(
            directory.database("send.db"),
            SqliteQuery::new("SELECT 1", Vec::new()),
        ));
        assert_send(storage.execute_transaction(
            directory.database("send.db"),
            SqliteTransactionPlan::new(Vec::new()),
        ));
        assert_send(storage.open_existing(directory.database("send.db")));
        drop(storage);
    }
}
