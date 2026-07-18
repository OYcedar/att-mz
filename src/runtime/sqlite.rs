//! `rusqlite` 生产存储根。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::types::{Value as RusqliteValue, ValueRef};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use tokio::sync::oneshot;

use crate::att_mz::lua::session::{
    OpenSqliteInteractiveSessionError, OpenedSqliteInteractiveSession,
    SqliteInteractiveConnectionCloseOutcome, SqliteInteractiveRollbackOutcome,
    SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
    SqliteInteractiveSessionFinalizationReport, SqliteInteractiveSessionFinalizer,
    SqliteInteractiveSessionOperations, SqliteInteractiveTransactionObservation,
};
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

const STORAGE_RUNNING: u8 = 0;
const STORAGE_SHUTTING_DOWN: u8 = 1;
const STORAGE_CLOSED: u8 = 2;
const SESSION_OPEN: u8 = 0;
const SESSION_INDETERMINATE: u8 = 1;
const SESSION_FINALIZING: u8 = 2;
const SESSION_CLOSED: u8 = 3;

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
    max_interactive_sessions: NonZeroUsize,
    interactive_open_queue_capacity: NonZeroUsize,
    interactive_command_queue_capacity: NonZeroUsize,
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
        max_interactive_sessions: NonZeroUsize,
        interactive_open_queue_capacity: NonZeroUsize,
        interactive_command_queue_capacity: NonZeroUsize,
        worker_stack_bytes: NonZeroUsize,
        max_statement_bytes: NonZeroUsize,
        max_parameter_bytes: NonZeroUsize,
        max_rows_per_query: NonZeroUsize,
        max_result_bytes_per_query: NonZeroUsize,
        busy_timeout: Duration,
        journal_mode: SqliteJournalMode,
        synchronous: SqliteSynchronous,
    ) -> Result<Self, SqliteRuntimeError> {
        if max_open_connections.get() < 2 {
            return Err(SqliteRuntimeError::InvalidConfiguration(
                "max_open_connections 必须至少为 2，以原子容纳 SQLite online backup 的源与目标连接",
            ));
        }
        if max_interactive_sessions > max_open_connections {
            return Err(SqliteRuntimeError::InvalidConfiguration(
                "max_interactive_sessions 不得大于 max_open_connections",
            ));
        }
        if busy_timeout.is_zero() {
            return Err(SqliteRuntimeError::InvalidConfiguration(
                "busy_timeout_ms 必须大于零",
            ));
        }

        Ok(Self {
            short_worker_threads,
            short_queue_capacity,
            max_open_connections,
            max_interactive_sessions,
            interactive_open_queue_capacity,
            interactive_command_queue_capacity,
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
    Closed,
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
            Self::Closed => formatter.write_str("SQLite 存储根已经关闭"),
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
            | Self::Closed
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

fn owned_driver_values(values: &[SqliteValue]) -> Vec<RusqliteValue> {
    values
        .iter()
        .map(|value| match value {
            SqliteValue::Null => RusqliteValue::Null,
            SqliteValue::Integer(value) => RusqliteValue::Integer(*value),
            SqliteValue::Real(value) => RusqliteValue::Real(*value),
            SqliteValue::Text(value) => RusqliteValue::Text(value.clone()),
            SqliteValue::Blob(value) => RusqliteValue::Blob(value.clone()),
        })
        .collect()
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

fn read_query_rows(
    connection: &Connection,
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
) -> Result<Vec<SqliteRow>, SqliteRuntimeError> {
    validate_query(query, config)?;
    let parameters = owned_driver_values(query.parameters());
    let mut statement = connection
        .prepare(query.statement())
        .map_err(|source| SqliteRuntimeError::driver("准备查询", source))?;
    let column_count = statement.column_count();
    let mut cursor = statement
        .query(params_from_iter(parameters.iter()))
        .map_err(|source| SqliteRuntimeError::driver("绑定查询参数", source))?;
    let mut result = Vec::new();
    let mut result_bytes = 0usize;

    while let Some(row) = cursor
        .next()
        .map_err(|source| SqliteRuntimeError::driver("读取查询行", source))?
    {
        if result.len() == config.max_rows_per_query.get() {
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
            result_bytes = result_bytes
                .checked_add(sqlite_value_bytes(&value)?)
                .ok_or(SqliteRuntimeError::ResourceLimit {
                    resource: "查询结果字节数",
                    limit: config.max_result_bytes_per_query.get(),
                })?;
            if result_bytes > config.max_result_bytes_per_query.get() {
                return Err(SqliteRuntimeError::ResourceLimit {
                    resource: "查询结果字节数",
                    limit: config.max_result_bytes_per_query.get(),
                });
            }
            values.push(value);
        }
        result.push(SqliteRow::new(values));
    }
    Ok(result)
}

fn run_query_existing(
    path: &Path,
    query: &SqliteQuery,
    config: &RusqliteStorageConfiguration,
) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<SqliteRuntimeError>> {
    let connection = open_existing_read_only(path, config).map_err(|error| match error {
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
    })?;
    read_query_rows(&connection, query, config).map_err(QueryExistingDatabaseError::QueryFailed)
}

fn validate_transaction_plan(
    plan: &SqliteTransactionPlan,
    config: &RusqliteStorageConfiguration,
) -> Result<(), SqliteRuntimeError> {
    for step in plan.steps() {
        match step {
            SqliteTransactionStep::Execute(command) => validate_command(command, config)?,
            SqliteTransactionStep::ExecuteMany(batch) => {
                validate_statement(batch.statement(), config)?;
                for parameters in batch.parameter_sets() {
                    validate_parameters(parameters, config)?;
                }
            }
            SqliteTransactionStep::RequireNoRows { query, .. } => {
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
        let result = match step {
            SqliteTransactionStep::Execute(command) => {
                let parameters = owned_driver_values(command.parameters());
                connection
                    .execute(command.statement(), params_from_iter(parameters.iter()))
                    .map(|_| ())
                    .map_err(|source| SqliteRuntimeError::driver("执行事务命令", source))
            }
            SqliteTransactionStep::ExecuteMany(batch) => (|| {
                let mut statement = connection
                    .prepare(batch.statement())
                    .map_err(|source| SqliteRuntimeError::driver("准备批量命令", source))?;
                let mut result = Ok(());
                for parameters in batch.parameter_sets() {
                    let parameters = owned_driver_values(parameters);
                    if let Err(source) = statement.execute(params_from_iter(parameters.iter())) {
                        result = Err(SqliteRuntimeError::driver("执行批量命令", source));
                        break;
                    }
                }
                result
            })(),
            SqliteTransactionStep::RequireNoRows { check_id, query } => {
                let exists = (|| {
                    let parameters = owned_driver_values(query.parameters());
                    let mut statement = connection
                        .prepare(query.statement())
                        .map_err(|source| SqliteRuntimeError::driver("准备事务条件查询", source))?;
                    statement
                        .exists(params_from_iter(parameters.iter()))
                        .map_err(|source| SqliteRuntimeError::driver("执行事务条件查询", source))
                })();
                match exists {
                    Ok(false) => Ok(()),
                    Ok(true) => {
                        return match connection.execute_batch("ROLLBACK") {
                            Ok(()) if connection.is_autocommit() => {
                                Err(ExecuteTransactionError::RequirementFailed {
                                    check_id: check_id.clone(),
                                })
                            }
                            Ok(()) => Err(ExecuteTransactionError::OutcomeUnknown(
                                SqliteRuntimeError::Internal("事务条件失败回滚后仍非 autocommit"),
                            )),
                            Err(source) => Err(ExecuteTransactionError::OutcomeUnknown(
                                SqliteRuntimeError::driver("回滚事务条件失败", source),
                            )),
                        };
                    }
                    Err(source) => Err(source),
                }
            }
        };

        if let Err(primary) = result {
            return match rollback_after_failure(&connection, primary) {
                Ok(primary) => Err(ExecuteTransactionError::NotCommitted(primary)),
                Err(source) => Err(ExecuteTransactionError::OutcomeUnknown(source)),
            };
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
            let parameters = owned_driver_values(command.parameters());
            if let Err(source) =
                connection.execute(command.statement(), params_from_iter(parameters.iter()))
            {
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
                let parameters = owned_driver_values(command.parameters());
                connection
                    .execute(command.statement(), params_from_iter(parameters.iter()))
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

type InteractiveFinalizationReport = SqliteInteractiveSessionFinalizationReport<SqliteRuntimeError>;

fn finalize_interactive_connection(
    connection: Connection,
    transaction: InteractiveTransactionState,
    lifecycle: &AtomicU8,
) -> InteractiveFinalizationReport {
    let observation = match transaction {
        InteractiveTransactionState::Idle => SqliteInteractiveTransactionObservation::Idle,
        InteractiveTransactionState::Active => SqliteInteractiveTransactionObservation::Active,
        InteractiveTransactionState::Indeterminate => {
            SqliteInteractiveTransactionObservation::Indeterminate
        }
    };
    let rollback = if connection.is_autocommit() {
        SqliteInteractiveRollbackOutcome::NotRequired
    } else {
        match connection.execute_batch("ROLLBACK") {
            Ok(()) if connection.is_autocommit() => SqliteInteractiveRollbackOutcome::RolledBack,
            Ok(()) => SqliteInteractiveRollbackOutcome::OutcomeUnknown(
                SqliteRuntimeError::Internal("回滚成功后交互式连接仍非 autocommit"),
            ),
            Err(source) if connection.is_autocommit() => {
                SqliteInteractiveRollbackOutcome::OutcomeUnknown(SqliteRuntimeError::driver(
                    "终结交互式事务",
                    source,
                ))
            }
            Err(source) => SqliteInteractiveRollbackOutcome::Failed(SqliteRuntimeError::driver(
                "终结交互式事务",
                source,
            )),
        }
    };
    let connection = match connection.close() {
        Ok(()) => SqliteInteractiveConnectionCloseOutcome::Closed,
        Err((_connection, source)) => SqliteInteractiveConnectionCloseOutcome::Failed(
            SqliteRuntimeError::driver("关闭交互式连接", source),
        ),
    };
    lifecycle.store(SESSION_CLOSED, Ordering::Release);
    SqliteInteractiveSessionFinalizationReport::new(observation, rollback, connection)
}

fn panicked_finalization_report() -> InteractiveFinalizationReport {
    SqliteInteractiveSessionFinalizationReport::new(
        SqliteInteractiveTransactionObservation::Unavailable(SqliteRuntimeError::WorkerPanicked(
            "交互式 actor",
        )),
        SqliteInteractiveRollbackOutcome::NotAttempted,
        SqliteInteractiveConnectionCloseOutcome::OutcomeUnknown(
            SqliteRuntimeError::WorkerPanicked("交互式 actor"),
        ),
    )
}

fn run_interactive_actor(
    connection: Connection,
    config: Arc<RusqliteStorageConfiguration>,
    commands: async_channel::Receiver<InteractiveCommand>,
    control: mpsc::Receiver<()>,
    lifecycle: Arc<AtomicU8>,
    _connection_permit: PoolPermit,
) -> InteractiveFinalizationReport {
    let mut transaction = if connection.is_autocommit() {
        InteractiveTransactionState::Idle
    } else {
        InteractiveTransactionState::Active
    };
    while let Ok(command) = commands.recv_blocking() {
        process_interactive_command(&connection, &config, &mut transaction, &lifecycle, command);
    }
    let _ = control.recv();
    finalize_interactive_connection(connection, transaction, &lifecycle)
}

struct InteractiveSessionRegistryState {
    accepting: bool,
    active: BTreeMap<u64, Arc<InteractiveSessionControl>>,
}

struct InteractiveSessionRegistry {
    state: Mutex<InteractiveSessionRegistryState>,
    changed: Condvar,
}

impl InteractiveSessionRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InteractiveSessionRegistryState {
                accepting: true,
                active: BTreeMap::new(),
            }),
            changed: Condvar::new(),
        })
    }

    fn is_accepting(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting
    }

    fn register(&self, session_id: u64, control: Arc<InteractiveSessionControl>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return false;
        }
        let previous = state.active.insert(session_id, control);
        debug_assert!(previous.is_none(), "SQLite session ID 必须唯一");
        true
    }

    fn begin_shutdown(&self) -> Vec<Arc<InteractiveSessionControl>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        state.active.values().cloned().collect()
    }

    fn complete(&self, session_id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active.remove(&session_id);
        if state.active.is_empty() {
            self.changed.notify_all();
        }
    }

    fn wait_until_empty(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.active.is_empty() {
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
    actor: JoinHandle<InteractiveFinalizationReport>,
    report: oneshot::Sender<InteractiveFinalizationReport>,
    session_permit: PoolPermit,
}

struct InteractiveSessionControl {
    session_id: u64,
    lifecycle: Arc<AtomicU8>,
    resources: Mutex<Option<InteractiveFinalizationResources>>,
    reaper: mpsc::Sender<ReaperJob>,
    registry: Arc<InteractiveSessionRegistry>,
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
        let job = ReaperJob::Finalize {
            control: Arc::clone(self),
            actor: resources.actor,
            report: resources.report,
            session_permit: resources.session_permit,
        };
        if let Err(error) = self.reaper.send(job) {
            complete_reaper_job(error.0);
        }
    }
}

enum ReaperJob {
    Finalize {
        control: Arc<InteractiveSessionControl>,
        actor: JoinHandle<InteractiveFinalizationReport>,
        report: oneshot::Sender<InteractiveFinalizationReport>,
        session_permit: PoolPermit,
    },
    Shutdown,
}

fn complete_reaper_job(job: ReaperJob) {
    let ReaperJob::Finalize {
        control,
        actor,
        report,
        session_permit,
    } = job
    else {
        return;
    };
    let finalization = actor
        .join()
        .unwrap_or_else(|_| panicked_finalization_report());
    let _ = report.send(finalization);
    drop(session_permit);
    control.registry.complete(control.session_id);
}

fn run_reaper(receiver: mpsc::Receiver<ReaperJob>) {
    while let Ok(job) = receiver.recv() {
        match job {
            ReaperJob::Finalize { .. } => complete_reaper_job(job),
            ReaperJob::Shutdown => break,
        }
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
    report: Option<oneshot::Receiver<InteractiveFinalizationReport>>,
}

impl RusqliteInteractiveSessionFinalizer {
    fn initiate(&mut self) -> oneshot::Receiver<InteractiveFinalizationReport> {
        self.control.initiate();
        self.report.take().expect("终结令牌必须拥有唯一报告接收端")
    }
}

impl SqliteInteractiveSessionFinalizer for RusqliteInteractiveSessionFinalizer {
    type Error = SqliteRuntimeError;

    fn finalize(mut self) -> impl Future<Output = InteractiveFinalizationReport> + Send {
        let receiver = self.initiate();
        async move {
            receiver
                .await
                .unwrap_or_else(|_| panicked_finalization_report())
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
    Transaction {
        path: PathBuf,
        plan: SqliteTransactionPlan,
        response: oneshot::Sender<Result<(), ExecuteTransactionError<SqliteRuntimeError>>>,
        #[cfg(test)]
        panic_after_operation: bool,
    },
}

struct OpenJob {
    path: PathBuf,
    response: oneshot::Sender<
        Result<OpenedRusqliteSession, OpenSqliteInteractiveSessionError<SqliteRuntimeError>>,
    >,
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

#[allow(clippy::too_many_arguments, reason = "actor 必须一次取得全部唯一资源")]
fn open_interactive_session(
    path: &Path,
    config: Arc<RusqliteStorageConfiguration>,
    connections: &Arc<PermitPool>,
    sessions: &Arc<PermitPool>,
    reaper: &mpsc::Sender<ReaperJob>,
    registry: &Arc<InteractiveSessionRegistry>,
    session_id: u64,
) -> Result<OpenedRusqliteSession, OpenSqliteInteractiveSessionError<SqliteRuntimeError>> {
    if !registry.is_accepting() {
        return Err(OpenSqliteInteractiveSessionError::OpenFailed(
            SqliteRuntimeError::Closed,
        ));
    }
    let session_permit = sessions.acquire();
    if !registry.is_accepting() {
        return Err(OpenSqliteInteractiveSessionError::OpenFailed(
            SqliteRuntimeError::Closed,
        ));
    }
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
    let (command_sender, command_receiver) =
        async_channel::bounded(config.interactive_command_queue_capacity.get());
    let actor_receiver = command_receiver.clone();
    let (control_sender, control_receiver) = mpsc::channel();
    let lifecycle = Arc::new(AtomicU8::new(SESSION_OPEN));
    let actor_lifecycle = Arc::clone(&lifecycle);
    let actor_config = Arc::clone(&config);
    let (report_sender, report_receiver) = oneshot::channel();
    let actor = thread::Builder::new()
        .name(format!("att-sqlite-session-{session_id}"))
        .stack_size(config.worker_stack_bytes.get())
        .spawn(move || {
            run_interactive_actor(
                connection,
                actor_config,
                actor_receiver,
                control_receiver,
                actor_lifecycle,
                connection_permit,
            )
        })
        .map_err(|source| {
            OpenSqliteInteractiveSessionError::OpenFailed(SqliteRuntimeError::WorkerSpawn {
                worker: format!("interactive-{session_id}"),
                source,
            })
        })?;
    let operations = Arc::new(RusqliteInteractiveSessionOperations {
        commands: command_sender,
        lifecycle: Arc::clone(&lifecycle),
    });
    let control = Arc::new(InteractiveSessionControl {
        session_id,
        lifecycle,
        resources: Mutex::new(Some(InteractiveFinalizationResources {
            command_receiver,
            control: control_sender,
            actor,
            report: report_sender,
            session_permit,
        })),
        reaper: reaper.clone(),
        registry: Arc::clone(registry),
    });
    if !registry.register(session_id, Arc::clone(&control)) {
        control.initiate();
        return Err(OpenSqliteInteractiveSessionError::OpenFailed(
            SqliteRuntimeError::Closed,
        ));
    }
    let finalizer = RusqliteInteractiveSessionFinalizer {
        control,
        report: Some(report_receiver),
    };
    Ok(OpenedSqliteInteractiveSession::new(operations, finalizer))
}

fn run_open_worker(
    receiver: async_channel::Receiver<OpenJob>,
    config: Arc<RusqliteStorageConfiguration>,
    connections: Arc<PermitPool>,
    sessions: Arc<PermitPool>,
    reaper: mpsc::Sender<ReaperJob>,
    registry: Arc<InteractiveSessionRegistry>,
    next_session_id: Arc<AtomicU64>,
) {
    while let Ok(job) = receiver.recv_blocking() {
        let session_id = next_session_id.fetch_add(1, Ordering::Relaxed);
        let result = catch_unwind(AssertUnwindSafe(|| {
            open_interactive_session(
                &job.path,
                Arc::clone(&config),
                &connections,
                &sessions,
                &reaper,
                &registry,
                session_id,
            )
        }))
        .unwrap_or_else(|_| {
            Err(OpenSqliteInteractiveSessionError::OpenFailed(
                SqliteRuntimeError::WorkerPanicked("交互式打开"),
            ))
        });
        let _ = job.response.send(result);
    }
}

struct RusqliteStorageInner {
    config: Arc<RusqliteStorageConfiguration>,
    accepting: AtomicBool,
    lifecycle: AtomicU8,
    short_sender: async_channel::Sender<ShortJob>,
    open_sender: async_channel::Sender<OpenJob>,
    short_workers: Mutex<Option<Vec<JoinHandle<()>>>>,
    open_worker: Mutex<Option<JoinHandle<()>>>,
    reaper_sender: mpsc::Sender<ReaperJob>,
    reaper_worker: Mutex<Option<JoinHandle<()>>>,
    connections: Arc<PermitPool>,
    sessions: Arc<PermitPool>,
    interactive_sessions: Arc<InteractiveSessionRegistry>,
}

impl Drop for RusqliteStorageInner {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        self.short_sender.close();
        self.open_sender.close();
        if self.lifecycle.load(Ordering::Acquire) == STORAGE_CLOSED {
            return;
        }

        let short_workers = self
            .short_workers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let open_worker = self
            .open_worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let reaper_worker = self
            .reaper_worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if short_workers.is_empty() && open_worker.is_none() && reaper_worker.is_none() {
            return;
        }
        let connections = Arc::clone(&self.connections);
        let sessions = Arc::clone(&self.sessions);
        let interactive_sessions = Arc::clone(&self.interactive_sessions);
        let controls = interactive_sessions.begin_shutdown();
        for control in controls {
            control.initiate();
        }
        let reaper = self.reaper_sender.clone();
        let stack_size = self.config.worker_stack_bytes.get();
        let _ = thread::Builder::new()
            .name("att-sqlite-drop-shutdown".to_owned())
            .stack_size(stack_size)
            .spawn(move || {
                for worker in short_workers {
                    let _ = worker.join();
                }
                if let Some(worker) = open_worker {
                    let _ = worker.join();
                }
                interactive_sessions.wait_until_empty();
                connections.wait_until_empty();
                sessions.wait_until_empty();
                let _ = reaper.send(ReaperJob::Shutdown);
                if let Some(worker) = reaper_worker {
                    let _ = worker.join();
                }
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
        let sessions = PermitPool::new(config.max_interactive_sessions);
        let interactive_sessions = InteractiveSessionRegistry::new();
        let (short_sender, short_receiver) =
            async_channel::bounded(config.short_queue_capacity.get());
        let (open_sender, open_receiver) =
            async_channel::bounded(config.interactive_open_queue_capacity.get());
        let (reaper_sender, reaper_receiver) = mpsc::channel();
        let reaper_worker = thread::Builder::new()
            .name("att-sqlite-reaper".to_owned())
            .stack_size(config.worker_stack_bytes.get())
            .spawn(move || run_reaper(reaper_receiver))
            .map_err(|source| SqliteRuntimeError::WorkerSpawn {
                worker: "reaper".to_owned(),
                source,
            })?;

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
                    open_sender.close();
                    for worker in short_workers {
                        let _ = worker.join();
                    }
                    let _ = reaper_sender.send(ReaperJob::Shutdown);
                    let _ = reaper_worker.join();
                    return Err(SqliteRuntimeError::WorkerSpawn {
                        worker: format!("short-{index}"),
                        source,
                    });
                }
            };
            short_workers.push(worker);
        }

        let open_config = Arc::clone(&config);
        let open_connections = Arc::clone(&connections);
        let open_sessions = Arc::clone(&sessions);
        let open_interactive_sessions = Arc::clone(&interactive_sessions);
        let open_reaper = reaper_sender.clone();
        let next_session_id = Arc::new(AtomicU64::new(1));
        let open_worker = match thread::Builder::new()
            .name("att-sqlite-open".to_owned())
            .stack_size(config.worker_stack_bytes.get())
            .spawn(move || {
                run_open_worker(
                    open_receiver,
                    open_config,
                    open_connections,
                    open_sessions,
                    open_reaper,
                    open_interactive_sessions,
                    next_session_id,
                )
            }) {
            Ok(worker) => worker,
            Err(source) => {
                short_sender.close();
                open_sender.close();
                for worker in short_workers {
                    let _ = worker.join();
                }
                let _ = reaper_sender.send(ReaperJob::Shutdown);
                let _ = reaper_worker.join();
                return Err(SqliteRuntimeError::WorkerSpawn {
                    worker: "interactive-open".to_owned(),
                    source,
                });
            }
        };

        Ok(Self {
            inner: Arc::new(RusqliteStorageInner {
                config,
                accepting: AtomicBool::new(true),
                lifecycle: AtomicU8::new(STORAGE_RUNNING),
                short_sender,
                open_sender,
                short_workers: Mutex::new(Some(short_workers)),
                open_worker: Mutex::new(Some(open_worker)),
                reaper_sender,
                reaper_worker: Mutex::new(Some(reaper_worker)),
                connections,
                sessions,
                interactive_sessions,
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
        self.inner.open_sender.close();
        let controls = self.inner.interactive_sessions.begin_shutdown();
        for control in controls {
            control.initiate();
        }
        let short_workers = self
            .inner
            .short_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let open_worker = self
            .inner
            .open_worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let reaper_worker = self
            .inner
            .reaper_worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
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
                if let Some(worker) = open_worker {
                    panic |= worker.join().is_err();
                }
                inner.interactive_sessions.wait_until_empty();
                inner.connections.wait_until_empty();
                inner.sessions.wait_until_empty();
                let _ = inner.reaper_sender.send(ReaperJob::Shutdown);
                if let Some(worker) = reaper_worker {
                    panic |= worker.join().is_err();
                }
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
        async move {
            accepting.map_err(SnapshotDatabaseError::NotCreated)?;
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
        let sender = self.inner.open_sender.clone();
        let accepting = self.ensure_accepting();
        async move {
            accepting.map_err(OpenSqliteInteractiveSessionError::OpenFailed)?;
            let (response, receiver) = oneshot::channel();
            sender.send(OpenJob { path, response }).await.map_err(|_| {
                OpenSqliteInteractiveSessionError::OpenFailed(SqliteRuntimeError::Closed)
            })?;
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

    use crate::storage::sqlite::{SqliteBatch, SqliteCheckId};
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

    fn configuration() -> RusqliteStorageConfiguration {
        RusqliteStorageConfiguration::new(
            nonzero(2),
            nonzero(8),
            nonzero(4),
            nonzero(2),
            nonzero(4),
            nonzero(4),
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
    fn configuration_reserves_two_connections_for_online_backup() {
        let error = RusqliteStorageConfiguration::new(
            nonzero(1),
            nonzero(1),
            nonzero(1),
            nonzero(1),
            nonzero(1),
            nonzero(1),
            nonzero(1024 * 1024),
            nonzero(64 * 1024),
            nonzero(64 * 1024),
            nonzero(100),
            nonzero(1024 * 1024),
            Duration::from_secs(1),
            SqliteJournalMode::Delete,
            SqliteSynchronous::Full,
        )
        .expect_err("online backup 需要同时占用两个连接许可");

        assert!(matches!(error, SqliteRuntimeError::InvalidConfiguration(_)));
        assert!(error.to_string().contains("至少为 2"));
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

        let check_id = SqliteCheckId::new("duplicate");
        let result = storage
            .execute_transaction(
                database.clone(),
                SqliteTransactionPlan::new(vec![
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id) VALUES (1)",
                        Vec::new(),
                    )),
                    SqliteTransactionStep::RequireNoRows {
                        check_id: check_id.clone(),
                        query: SqliteQuery::new(
                            "SELECT 1 FROM values_table WHERE id = 1",
                            Vec::new(),
                        ),
                    },
                    SqliteTransactionStep::Execute(SqliteCommand::new(
                        "INSERT INTO values_table (id) VALUES (2)",
                        Vec::new(),
                    )),
                ]),
            )
            .await;
        assert!(matches!(
            result,
            Err(ExecuteTransactionError::RequirementFailed { check_id: actual })
                if actual == check_id
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
        let report = finalizer.finalize().await;
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Active
        ));
        assert!(matches!(
            report.rollback(),
            SqliteInteractiveRollbackOutcome::RolledBack
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
        let mut config = configuration();
        config.interactive_command_queue_capacity = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
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
            .expect("队列填满不得阻断独立终结通道");
        first_response
            .await
            .expect("交互式 actor 必须返回首条命令结果")
            .expect("第一条已接管命令应完成");
        second
            .await
            .expect("第二个调用任务不应 panic")
            .expect("第二条已接管命令应完成");
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Idle
        ));
        assert!(matches!(
            report.rollback(),
            SqliteInteractiveRollbackOutcome::NotRequired
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
            .expect("shutdown 后仍应可领取唯一终结报告");
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Idle
        ));
        assert!(matches!(
            report.rollback(),
            SqliteInteractiveRollbackOutcome::NotRequired
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
        let report = finalizer.finalize().await;
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Active
        ));
        assert!(matches!(
            report.rollback(),
            SqliteInteractiveRollbackOutcome::RolledBack
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
        let mut config = configuration();
        config.interactive_command_queue_capacity = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
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
        let report = finalizer.finalize().await;
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
    async fn shutdown_releases_capacity_for_an_open_already_waiting_on_session_permit() {
        let directory = TestDirectory::new();
        let database = directory.database("shutdown-waiting-open.db");
        let mut config = configuration();
        config.max_interactive_sessions = nonzero(1);
        let storage = RusqliteStorage::start(config).expect("根应可启动");
        storage
            .create_new_database(database.clone(), schema_commands())
            .await
            .expect("数据库应可创建");
        let first = storage
            .open_existing(database.clone())
            .await
            .expect("第一个会话应占满容量");
        let (operations, finalizer) = first.into_parts();
        let waiting_storage = storage.clone();
        let waiting_database = database.clone();
        let waiting =
            tokio::spawn(async move { waiting_storage.open_existing(waiting_database).await });
        wait_until("第二个 open 进入 session permit 等待", || {
            storage.inner.open_sender.is_empty() && !waiting.is_finished()
        })
        .await;
        assert!(
            !waiting.is_finished(),
            "第二个 open 应正在等待 session permit"
        );

        tokio::time::timeout(Duration::from_secs(5), storage.shutdown())
            .await
            .expect("等待 session permit 的 open 不得与 shutdown 死锁")
            .expect("根应可关闭");
        assert!(matches!(
            waiting.await.expect("第二个 open 任务不应 panic"),
            Err(OpenSqliteInteractiveSessionError::OpenFailed(
                SqliteRuntimeError::Closed
            ))
        ));
        let report = finalizer.finalize().await;
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
        assert!(matches!(
            operations
                .query(SqliteQuery::new("SELECT 1", Vec::new()))
                .await,
            Err(SqliteInteractiveSessionError::Closed)
        ));
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
        let report = report.expect("唯一 finalizer 任务不应 panic");
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Idle
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
            .expect("最后一个 storage 句柄丢弃后仍应产生终结报告");
        assert!(matches!(
            report.transaction(),
            SqliteInteractiveTransactionObservation::Idle
        ));
        assert!(matches!(
            report.rollback(),
            SqliteInteractiveRollbackOutcome::NotRequired
        ));
        assert!(matches!(
            report.connection(),
            SqliteInteractiveConnectionCloseOutcome::Closed
        ));
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
