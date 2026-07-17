//! 可信 Lua 在单一 SQLite 连接上执行动态语句所需的根能力。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

/// 打开一个现存项目数据库的交互式会话失败。
#[derive(Debug)]
pub(crate) enum OpenSqliteInteractiveSessionError<E> {
    NotFound,
    OpenFailed(E),
}

impl<E> fmt::Display for OpenSqliteInteractiveSessionError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::OpenFailed(source) => write!(formatter, "无法打开交互式数据库会话：{source}"),
        }
    }
}

impl<E> Error for OpenSqliteInteractiveSessionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound => None,
            Self::OpenFailed(source) => Some(source),
        }
    }
}

/// 交互式会话中的单次操作失败。
#[derive(Debug)]
pub(crate) enum SqliteInteractiveSessionError<E> {
    Closed,
    Indeterminate,
    TransactionAlreadyActive,
    NoActiveTransaction,
    OperationFailed(E),
    OutcomeUnknown(E),
}

impl<E> fmt::Display for SqliteInteractiveSessionError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("交互式数据库会话已经进入终结阶段"),
            Self::Indeterminate => formatter.write_str("数据库会话结果已无法确定，只能终结会话"),
            Self::TransactionAlreadyActive => formatter.write_str("数据库事务已经开始"),
            Self::NoActiveTransaction => formatter.write_str("当前没有活动数据库事务"),
            Self::OperationFailed(source) => write!(formatter, "数据库会话操作失败：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库操作结果未知：{source}"),
        }
    }
}

impl<E> Error for SqliteInteractiveSessionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OperationFailed(source) | Self::OutcomeUnknown(source) => Some(source),
            Self::Closed
            | Self::Indeterminate
            | Self::TransactionAlreadyActive
            | Self::NoActiveTransaction => None,
        }
    }
}

/// 终结时观察到的事务状态。
#[derive(Debug)]
pub(crate) enum SqliteInteractiveTransactionObservation<E> {
    Idle,
    Active,
    Indeterminate,
    Unavailable(E),
}

/// 终结时对活动事务的回滚结果。
#[derive(Debug)]
pub(crate) enum SqliteInteractiveRollbackOutcome<E> {
    NotRequired,
    RolledBack,
    Failed(E),
    OutcomeUnknown(E),
    NotAttempted,
}

/// 交互式连接的关闭结果。
#[derive(Debug)]
pub(crate) enum SqliteInteractiveConnectionCloseOutcome<E> {
    Closed,
    Failed(E),
    OutcomeUnknown(E),
}

/// 交互式会话的完整终结报告。
///
/// 报告始终同时保留事务观察、回滚与连接关闭三个终态，因此关闭错误
/// 不会覆盖先发生的回滚错误。
#[derive(Debug)]
pub(crate) struct SqliteInteractiveSessionFinalizationReport<E> {
    transaction: SqliteInteractiveTransactionObservation<E>,
    rollback: SqliteInteractiveRollbackOutcome<E>,
    connection: SqliteInteractiveConnectionCloseOutcome<E>,
}

impl<E> SqliteInteractiveSessionFinalizationReport<E> {
    pub(crate) fn new(
        transaction: SqliteInteractiveTransactionObservation<E>,
        rollback: SqliteInteractiveRollbackOutcome<E>,
        connection: SqliteInteractiveConnectionCloseOutcome<E>,
    ) -> Self {
        Self {
            transaction,
            rollback,
            connection,
        }
    }

    pub(crate) fn transaction(&self) -> &SqliteInteractiveTransactionObservation<E> {
        &self.transaction
    }

    pub(crate) fn rollback(&self) -> &SqliteInteractiveRollbackOutcome<E> {
        &self.rollback
    }

    pub(crate) fn connection(&self) -> &SqliteInteractiveConnectionCloseOutcome<E> {
        &self.connection
    }
}

/// 可信 Lua 在同一现存 SQLite 连接上执行的操作面。
///
/// `OutcomeUnknown` 之后实现必须进入不可继续执行的状态；后续调用只能
/// 返回 `Indeterminate`，终结令牌仍然可以完成回滚观察和连接关闭。
pub(crate) trait SqliteInteractiveSessionOperations: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn query(
        &self,
        query: SqliteQuery,
    ) -> impl Future<Output = Result<Vec<SqliteRow>, SqliteInteractiveSessionError<Self::Error>>> + Send;

    fn execute(
        &self,
        command: SqliteCommand,
    ) -> impl Future<Output = Result<u64, SqliteInteractiveSessionError<Self::Error>>> + Send;

    fn begin(
        &self,
    ) -> impl Future<Output = Result<(), SqliteInteractiveSessionError<Self::Error>>> + Send;

    fn commit(
        &self,
    ) -> impl Future<Output = Result<(), SqliteInteractiveSessionError<Self::Error>>> + Send;

    fn rollback(
        &self,
    ) -> impl Future<Output = Result<(), SqliteInteractiveSessionError<Self::Error>>> + Send;
}

/// 一次性终结交互式 SQLite 会话的唯一令牌。
///
/// 具体令牌不得实现 `Clone` 或 `Copy`。`finalize` 按值消费令牌，终止新
/// 操作的控制通道不得受普通命令队列背压。
pub(crate) trait SqliteInteractiveSessionFinalizer: Send + 'static {
    type Error: Error + Send + Sync + 'static;

    fn finalize(
        self,
    ) -> impl Future<Output = SqliteInteractiveSessionFinalizationReport<Self::Error>> + Send;
}

/// 工厂交付的交互式 SQLite 会话。
///
/// 操作面可以被 Host 调用共享，终结令牌只能移交给唯一的运行监督者。
#[must_use = "打开的 SQLite 会话必须把终结令牌移交给运行监督者"]
pub(crate) struct OpenedSqliteInteractiveSession<O, F> {
    operations: Arc<O>,
    finalizer: F,
}

pub(crate) type OpenSqliteInteractiveSessionResult<O, F, E> =
    Result<OpenedSqliteInteractiveSession<O, F>, OpenSqliteInteractiveSessionError<E>>;

impl<O, F> OpenedSqliteInteractiveSession<O, F> {
    pub(crate) fn new(operations: Arc<O>, finalizer: F) -> Self {
        Self {
            operations,
            finalizer,
        }
    }

    pub(crate) fn into_parts(self) -> (Arc<O>, F) {
        (self.operations, self.finalizer)
    }
}

/// 为可信 Lua 打开一个不会创建缺失数据库的交互式会话。
pub(crate) trait SqliteInteractiveSessionFactory: Send + Sync {
    type Operations: SqliteInteractiveSessionOperations;
    type Finalizer: SqliteInteractiveSessionFinalizer<
        Error = <Self::Operations as SqliteInteractiveSessionOperations>::Error,
    >;
    type Error: Error + Send + Sync + 'static;

    fn open_existing(
        &self,
        path: PathBuf,
    ) -> impl Future<
        Output = OpenSqliteInteractiveSessionResult<Self::Operations, Self::Finalizer, Self::Error>,
    > + Send;
}
