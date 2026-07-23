//! 在同一 SQLite 连接上连续执行多个完整事务计划的窄根能力。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::storage::sqlite::{ExecuteTransactionError, SqliteTransactionPlan};
use crate::storage::sqlite_session::SqliteInteractiveSessionFinalizer;

/// 打开现存数据库的长寿命事务计划会话失败。
#[derive(Debug)]
pub(crate) enum OpenSqliteTransactionSessionError<E> {
    NotFound,
    OpenFailed(E),
}

impl<E: fmt::Display> fmt::Display for OpenSqliteTransactionSessionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::OpenFailed(source) => write!(formatter, "无法打开事务计划会话：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for OpenSqliteTransactionSessionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound => None,
            Self::OpenFailed(source) => Some(source),
        }
    }
}

/// 在已经固定的同一连接上执行完整事务计划。
///
/// 每次调用仍然拥有独立的 `BEGIN IMMEDIATE` / `COMMIT` 原子边界；会话只复用连接、
/// PRAGMA 和驱动 statement cache，不把多个业务任务合并成一个事务。
pub(crate) trait SqliteTransactionSessionOperations: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn execute_transaction(
        &self,
        plan: SqliteTransactionPlan,
    ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send;
}

/// 工厂交付的一次长寿命事务计划会话。
#[must_use = "打开的 SQLite 事务计划会话必须显式终结"]
pub(crate) struct OpenedSqliteTransactionSession<O, F> {
    operations: Arc<O>,
    finalizer: F,
}

pub(crate) type SqliteTransactionSessionOpenResult<O, F, E> =
    Result<OpenedSqliteTransactionSession<O, F>, OpenSqliteTransactionSessionError<E>>;

impl<O, F> OpenedSqliteTransactionSession<O, F> {
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

/// 打开一个不会创建缺失数据库、可连续执行完整事务计划的会话。
pub(crate) trait SqliteTransactionSessionFactory: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type Operations: SqliteTransactionSessionOperations<Error = Self::Error>;
    type Finalizer: SqliteInteractiveSessionFinalizer<Error = Self::Error>;

    fn open_existing_transaction_session(
        &self,
        path: PathBuf,
    ) -> impl Future<
        Output = SqliteTransactionSessionOpenResult<Self::Operations, Self::Finalizer, Self::Error>,
    > + Send;
}
