//! 可信 Lua 在单一 SQLite 连接上执行动态语句所需的根能力。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

/// 交互式 SQLite 会话当前可观察的事务状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqliteInteractiveTransactionState {
    Idle,
    Active,
}

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
            Self::Closed => formatter.write_str("交互式数据库会话已经关闭"),
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
            Self::Closed | Self::TransactionAlreadyActive | Self::NoActiveTransaction => None,
        }
    }
}

/// 在同一已存在的 SQLite 连接上为可信 Lua 保持显式事务状态。
///
/// 根实现必须真正异步地等待底层工作，并使用与其它 SQLite 根能力相同的外部全局
/// 预算和连接策略。会话不得隐式开始、提交或嵌套事务；`commit` 无法确认结果时必须
/// 返回 `OutcomeUnknown`。所有方法都作用于同一个连接。
pub(crate) trait SqliteInteractiveSession: Send + Sync + 'static {
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

    fn transaction_state(
        &self,
    ) -> impl Future<
        Output = Result<
            SqliteInteractiveTransactionState,
            SqliteInteractiveSessionError<Self::Error>,
        >,
    > + Send;

    /// 关闭连接；若仍有活动事务，根实现必须先回滚，绝不能隐式提交。
    fn close(
        &self,
    ) -> impl Future<Output = Result<(), SqliteInteractiveSessionError<Self::Error>>> + Send;
}

/// 为可信 Lua 打开一个不会创建缺失数据库的交互式会话。
pub(crate) trait SqliteInteractiveSessionFactory: Send + Sync {
    type Session: SqliteInteractiveSession;
    type Error: Error + Send + Sync + 'static;

    fn open_existing(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<Self::Session, OpenSqliteInteractiveSessionError<Self::Error>>> + Send;
}
