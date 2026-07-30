//! 长寿命 SQLite 会话共用的终结契约。

use std::error::Error;
use std::fmt;
use std::future::Future;

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
