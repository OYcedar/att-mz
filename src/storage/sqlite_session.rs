//! 在单一 SQLite 连接上执行动态语句的交互式会话根能力。

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

/// 交互式会话已完整终结。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SqliteInteractiveSessionFinalization {
    had_unclosed_transaction: bool,
}

impl SqliteInteractiveSessionFinalization {
    pub(crate) fn new(had_unclosed_transaction: bool) -> Self {
        Self {
            had_unclosed_transaction,
        }
    }

    pub(crate) fn had_unclosed_transaction(self) -> bool {
        self.had_unclosed_transaction
    }
}

/// 交互式会话终结的主失败语义。
#[derive(Debug)]
pub(crate) enum SqliteInteractiveSessionFinalizationFailure<E> {
    CleanupFailed(E),
    OutcomeUnknown(E),
}

impl<E> SqliteInteractiveSessionFinalizationFailure<E> {
    pub(crate) fn source(&self) -> &E {
        match self {
            Self::CleanupFailed(source) | Self::OutcomeUnknown(source) => source,
        }
    }
}

/// 交互式会话终结失败。
///
/// `primary` 保留回滚、actor panic 或唯一的关闭失败；若回滚失败后
/// 连接关闭也失败，`connection_close` 同时保留第二个原因。
#[derive(Debug)]
pub(crate) struct SqliteInteractiveSessionFinalizationError<E> {
    primary: Box<SqliteInteractiveSessionFinalizationFailure<E>>,
    connection_close: Option<Box<E>>,
}

impl<E> SqliteInteractiveSessionFinalizationError<E> {
    pub(crate) fn new(
        primary: SqliteInteractiveSessionFinalizationFailure<E>,
        connection_close: Option<E>,
    ) -> Self {
        Self {
            primary: Box::new(primary),
            connection_close: connection_close.map(Box::new),
        }
    }

    pub(crate) fn primary(&self) -> &SqliteInteractiveSessionFinalizationFailure<E> {
        self.primary.as_ref()
    }

    pub(crate) fn connection_close(&self) -> Option<&E> {
        self.connection_close.as_deref()
    }

    /// 消费收尾错误，保留主失败与连接关闭失败两个独立原因。
    pub(crate) fn into_parts(self) -> (SqliteInteractiveSessionFinalizationFailure<E>, Option<E>) {
        (*self.primary, self.connection_close.map(|source| *source))
    }
}

impl<E: fmt::Display> fmt::Display for SqliteInteractiveSessionFinalizationError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.primary.as_ref() {
            SqliteInteractiveSessionFinalizationFailure::CleanupFailed(source) => {
                write!(formatter, "无法完整终结交互式数据库会话：{source}")?;
            }
            SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(source) => {
                write!(formatter, "交互式数据库会话的终结结果未知：{source}")?;
            }
        }
        if let Some(source) = &self.connection_close {
            write!(formatter, "；连接关闭也失败：{source}")?;
        }
        Ok(())
    }
}

impl<E: Error + 'static> Error for SqliteInteractiveSessionFinalizationError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.source())
    }
}

/// 在同一现存 SQLite 连接上执行的操作面。
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
    ) -> impl Future<
        Output = Result<
            SqliteInteractiveSessionFinalization,
            SqliteInteractiveSessionFinalizationError<Self::Error>,
        >,
    > + Send;
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

/// 打开一个不会创建缺失数据库的交互式会话。
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
