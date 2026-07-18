//! SQLite 数据库创建、查询与事务执行的根能力契约。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

/// SQLite 拥有型值。
///
/// 根接口返回完整存储类型，使上层持久化边界能够识别外部修改造成的字段类型错误，
/// 而不是把错误类型强制转换为期望值。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteValue {
    /// 返回适合诊断的 SQLite 存储类型名称。
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Integer(_) => "INTEGER",
            Self::Real(_) => "REAL",
            Self::Text(_) => "TEXT",
            Self::Blob(_) => "BLOB",
        }
    }
}

/// 一条参数化 SQLite 命令。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteCommand {
    statement: String,
    parameters: Vec<SqliteValue>,
}

impl SqliteCommand {
    pub(crate) fn new(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> Self {
        Self {
            statement: statement.into(),
            parameters,
        }
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn parameters(&self) -> &[SqliteValue] {
        &self.parameters
    }
}

/// 一条参数化 SQLite 查询。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteQuery {
    statement: String,
    parameters: Vec<SqliteValue>,
}

impl SqliteQuery {
    pub(crate) fn new(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> Self {
        Self {
            statement: statement.into(),
            parameters,
        }
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn parameters(&self) -> &[SqliteValue] {
        &self.parameters
    }
}

/// 查询返回的一行拥有型数据，列顺序与查询投影一致。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteRow {
    values: Vec<SqliteValue>,
}

impl SqliteRow {
    pub(crate) fn new(values: Vec<SqliteValue>) -> Self {
        Self { values }
    }

    pub(crate) fn values(&self) -> &[SqliteValue] {
        &self.values
    }

    pub(crate) fn into_values(self) -> Vec<SqliteValue> {
        self.values
    }
}

/// 使用同一已准备语句批量执行的参数组。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteBatch {
    statement: String,
    parameter_sets: Vec<Vec<SqliteValue>>,
}

impl SqliteBatch {
    pub(crate) fn new(statement: impl Into<String>, parameter_sets: Vec<Vec<SqliteValue>>) -> Self {
        Self {
            statement: statement.into(),
            parameter_sets,
        }
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn parameter_sets(&self) -> &[Vec<SqliteValue>] {
        &self.parameter_sets
    }
}

/// 事务条件的调用方标识，用于把失败恢复为领域错误。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SqliteCheckId(String);

impl SqliteCheckId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一个 SQLite 事务中的拥有型执行步骤。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SqliteTransactionStep {
    /// 执行一次参数化命令。
    Execute(SqliteCommand),
    /// 只准备一次语句，按顺序执行全部参数组。
    ExecuteMany(SqliteBatch),
    /// 查询必须不返回任何行，否则事务在后续步骤之前失败并回滚。
    RequireNoRows {
        check_id: SqliteCheckId,
        query: SqliteQuery,
    },
}

/// 必须在同一个连接和写事务中顺序执行的完整计划。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteTransactionPlan {
    steps: Vec<SqliteTransactionStep>,
}

impl SqliteTransactionPlan {
    pub(crate) fn new(steps: Vec<SqliteTransactionStep>) -> Self {
        Self { steps }
    }

    pub(crate) fn steps(&self) -> &[SqliteTransactionStep] {
        &self.steps
    }
}

/// 创建新数据库的明确终态。
#[derive(Debug)]
pub(crate) enum CreateDatabaseError<E> {
    AlreadyExists,
    NotCreated(E),
    OutcomeUnknown(E),
    ResidualArtifact(E),
}

/// 把一个现存 SQLite 数据库在线复制到 create-only 目标的明确终态。
#[derive(Debug)]
pub(crate) enum SnapshotDatabaseError<E> {
    SourceNotFound,
    DestinationAlreadyExists,
    NotCreated(E),
    ResidualArtifact(E),
    OutcomeUnknown(E),
}

impl<E: fmt::Display> fmt::Display for SnapshotDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound => formatter.write_str("源数据库不存在"),
            Self::DestinationAlreadyExists => formatter.write_str("快照目标数据库已经存在"),
            Self::NotCreated(source) => write!(formatter, "数据库快照未创建：{source}"),
            Self::ResidualArtifact(source) => {
                write!(formatter, "数据库快照创建失败且存在残留文件：{source}")
            }
            Self::OutcomeUnknown(source) => write!(formatter, "数据库快照结果未知：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for SnapshotDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceNotFound | Self::DestinationAlreadyExists => None,
            Self::NotCreated(source)
            | Self::ResidualArtifact(source)
            | Self::OutcomeUnknown(source) => Some(source),
        }
    }
}

impl<E: fmt::Display> fmt::Display for CreateDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists => formatter.write_str("目标数据库已经存在"),
            Self::NotCreated(source) => write!(formatter, "数据库未创建：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库创建结果未知：{source}"),
            Self::ResidualArtifact(source) => {
                write!(formatter, "数据库创建失败且存在残留文件：{source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for CreateDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyExists => None,
            Self::NotCreated(source)
            | Self::OutcomeUnknown(source)
            | Self::ResidualArtifact(source) => Some(source),
        }
    }
}

/// 查询现存数据库的失败语义。
#[derive(Debug)]
pub(crate) enum QueryExistingDatabaseError<E> {
    /// 目标主数据库文件不存在；实现没有创建文件。
    NotFound,
    /// 打开、准备、绑定、读取或关闭查询资源失败。
    QueryFailed(E),
}

impl<E: fmt::Display> fmt::Display for QueryExistingDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::QueryFailed(source) => write!(formatter, "数据库查询失败：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for QueryExistingDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound => None,
            Self::QueryFailed(source) => Some(source),
        }
    }
}

/// 执行拥有型 SQLite 事务计划的明确终态。
#[derive(Debug)]
pub(crate) enum ExecuteTransactionError<E> {
    /// 目标主数据库文件不存在；实现没有创建文件。
    NotFound,
    /// 指定事务条件命中了至少一行；整个事务已回滚。
    RequirementFailed { check_id: SqliteCheckId },
    /// 事务未提交，驱动确认其修改均未生效。
    NotCommitted(E),
    /// 驱动无法确认提交是否已经生效。
    OutcomeUnknown(E),
}

impl<E: fmt::Display> fmt::Display for ExecuteTransactionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::RequirementFailed { check_id } => {
                write!(formatter, "事务条件未满足：{}", check_id.as_str())
            }
            Self::NotCommitted(source) => write!(formatter, "数据库事务未提交：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库事务结果未知：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for ExecuteTransactionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound | Self::RequirementFailed { .. } => None,
            Self::NotCommitted(source) | Self::OutcomeUnknown(source) => Some(source),
        }
    }
}

/// 以 create-only 语义创建并初始化一个 SQLite 数据库。
///
/// 调用方必须提供位于现存父目录中的目标路径；同一目标的并发调用最多一个成功。
/// 所有命令按给定顺序、使用参数绑定并在同一事务内执行。实现完整拥有连接、伴生
/// 文件、失败清理和资源上限；返回的 Future 必须为 `Send`，且不得阻塞异步执行器
/// 线程。一旦开始产生副作用，调用方必须持续等待到明确终态。
pub(crate) trait SqliteDatabaseCreator: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn create_new_database(
        &self,
        path: PathBuf,
        commands: Vec<SqliteCommand>,
    ) -> impl Future<Output = Result<(), CreateDatabaseError<Self::Error>>> + Send;
}

/// 使用 SQLite online backup 把现存数据库复制为一个全新的数据库文件。
///
/// 实现必须在开始前原子取得两个连接许可，并以一次 `step(-1)` 完成复制；源文件
/// 缺失时不得创建它，目标文件存在时不得覆盖，也不得在 BUSY/LOCKED 后自动重试。
/// 目标一旦被 create-only 占有，即使调用 Future 被丢弃，实现也必须继续到明确终态。
pub(crate) trait SqliteDatabaseSnapshotter: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn snapshot_database(
        &self,
        source: PathBuf,
        destination: PathBuf,
    ) -> impl Future<Output = Result<(), SnapshotDatabaseError<Self::Error>>> + Send;
}

/// 只读查询一个必须已经存在的 SQLite 数据库。
///
/// 实现不得因数据库缺失而创建主文件；查询完成后不向调用方泄漏连接、statement
/// 或行游标。返回的 Future 必须为 `Send`，且不得阻塞异步执行器线程。
pub(crate) trait SqliteQueryExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send;
}

/// 在现存数据库中执行一个拥有型事务计划。
///
/// 实现以单一连接和写事务按顺序执行全部步骤。`ExecuteMany` 只准备一次语句；
/// `RequireNoRows` 命中时必须在执行后续步骤前回滚。实现完整拥有连接、TEMP 对象、
/// statement、回滚、关闭、并发预算和背压，不向调用方暴露这些机制。Future 必须
/// 为 `Send` 且不得阻塞异步执行器线程；事务开始后，调用方必须等待到明确终态。
pub(crate) trait SqliteTransactionExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send;
}
