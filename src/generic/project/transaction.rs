//! Generic 数据库连接策略及提交、回滚终态。

use super::error::{GenericProjectError, sqlite_error_is_busy};
use crate::execution::CooperativeCancellation;
use crate::runtime::performance::{
    RunPerformanceCounters, SqliteTransactionControl, SqliteTransactionScope,
};
use crate::runtime::sqlite::{
    AttSqliteCancellableConnection, AttSqliteCancellationHandle,
    apply_att_sqlite_cancellable_read_write_policy, apply_att_sqlite_new_database_page_policy,
    begin_cancellable_transaction, execute_transaction_control, suspend_att_sqlite_cancellation,
};
use rusqlite::{Connection, DropBehavior, OpenFlags, Transaction};
use std::error::Error;
use std::fmt;
use std::path::Path;

pub(super) fn open_sqlite_connection(
    database_path: &Path,
    create: bool,
    cancellation: CooperativeCancellation,
) -> Result<AttSqliteCancellableConnection, GenericProjectError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | if create {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        };
    let connection = Connection::open_with_flags(database_path, flags).map_err(|source| {
        GenericProjectError::Sqlite {
            operation: "打开 Generic 项目数据库",
            source,
        }
    })?;
    if create {
        apply_att_sqlite_new_database_page_policy(&connection).map_err(|source| {
            GenericProjectError::Sqlite {
                operation: "设置 Generic 新数据库物理页策略",
                source,
            }
        })?;
    }
    let wait_cancellation = cancellation.clone();
    apply_att_sqlite_cancellable_read_write_policy(connection, move || {
        wait_cancellation.is_requested()
    })
    .map_err(|source| {
        if cancellation.is_requested() && sqlite_error_is_busy(&source) {
            GenericProjectError::Cancelled
        } else {
            GenericProjectError::Sqlite {
                operation: "应用 Generic SQLite 读写策略",
                source,
            }
        }
    })
}

#[derive(Debug)]
pub(crate) enum GenericTransactionFinalizationFailure {
    Sqlite {
        operation: &'static str,
        source: rusqlite::Error,
    },
    InvalidState {
        violation: GenericTransactionFinalizationViolation,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenericTransactionFinalizationViolation {
    CommitSucceededButTransactionActive,
    CommitFailedButTransactionClosed,
    RollbackSucceededButTransactionActive,
}

impl fmt::Display for GenericTransactionFinalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite { operation, source } => {
                write!(formatter, "{operation}失败：{source}")
            }
            Self::InvalidState { violation } => formatter.write_str(match violation {
                GenericTransactionFinalizationViolation::CommitSucceededButTransactionActive => {
                    "COMMIT 返回成功后 Generic SQLite 连接仍处于事务中"
                }
                GenericTransactionFinalizationViolation::CommitFailedButTransactionClosed => {
                    "COMMIT 返回错误后 Generic SQLite 连接已离开事务，结果无法确认"
                }
                GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive => {
                    "ROLLBACK 返回成功后 Generic SQLite 连接仍处于事务中"
                }
            }),
        }
    }
}

impl Error for GenericTransactionFinalizationFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite { source, .. } => Some(source),
            Self::InvalidState { .. } => None,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "事务连接、取消、计数范围、三条终态诊断和业务体都是本边界的直接输入"
)]
pub(super) fn run_cancellable_transaction<T>(
    connection: &mut AttSqliteCancellableConnection,
    cancellation: &CooperativeCancellation,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    begin_operation: &'static str,
    commit_operation: &'static str,
    rollback_operation: &'static str,
    body: impl FnOnce(&Transaction<'_>) -> Result<T, GenericProjectError>,
) -> Result<T, GenericProjectError> {
    let cancellation_handle = connection.cancellation_handle();
    let mut transaction =
        begin_cancellable_transaction(connection, performance, scope).map_err(|source| {
            GenericProjectError::Sqlite {
                operation: begin_operation,
                source,
            }
        })?;
    let body_result = body(&transaction);

    // 从这里开始只允许本函数显式确定终态，不能再让 Transaction::drop 吞掉回滚错误。
    transaction.set_drop_behavior(DropBehavior::Ignore);
    let result = match body_result {
        Err(primary) => Err(rollback_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            primary,
            rollback_operation,
        )),
        Ok(_) if cancellation.is_requested() => Err(rollback_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            GenericProjectError::Cancelled,
            rollback_operation,
        )),
        Ok(value) => commit_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            commit_operation,
            rollback_operation,
        )
        .map(|()| value),
    };
    drop(transaction);
    result
}

fn rollback_generic_transaction(
    transaction: &Transaction<'_>,
    cancellation: &AttSqliteCancellationHandle,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    primary: GenericProjectError,
    rollback_operation: &'static str,
) -> GenericProjectError {
    if transaction.is_autocommit() {
        return primary;
    }

    let suspension = suspend_att_sqlite_cancellation(cancellation);
    let rollback = execute_transaction_control(
        transaction,
        performance,
        scope,
        SqliteTransactionControl::Rollback,
        "ROLLBACK",
    );
    let is_autocommit = transaction.is_autocommit();
    drop(suspension);

    match rollback {
        Ok(()) if is_autocommit => primary,
        Ok(()) => GenericProjectError::TransactionOutcomeUnknown {
            operation: rollback_operation,
            primary: Some(Box::new(primary)),
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive,
            },
        },
        Err(source) => GenericProjectError::TransactionOutcomeUnknown {
            operation: rollback_operation,
            primary: Some(Box::new(primary)),
            finalization: GenericTransactionFinalizationFailure::Sqlite {
                operation: rollback_operation,
                source,
            },
        },
    }
}

fn commit_generic_transaction(
    transaction: &Transaction<'_>,
    cancellation: &AttSqliteCancellationHandle,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    commit_operation: &'static str,
    rollback_operation: &'static str,
) -> Result<(), GenericProjectError> {
    let suspension = suspend_att_sqlite_cancellation(cancellation);

    let commit = execute_transaction_control(
        transaction,
        performance,
        scope,
        SqliteTransactionControl::Commit,
        "COMMIT",
    );
    let is_autocommit = transaction.is_autocommit();
    let result = match commit {
        Ok(()) if is_autocommit => Ok(()),
        Ok(()) => Err(GenericProjectError::TransactionOutcomeUnknown {
            operation: commit_operation,
            primary: None,
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::CommitSucceededButTransactionActive,
            },
        }),
        Err(source) if is_autocommit => Err(GenericProjectError::TransactionOutcomeUnknown {
            operation: commit_operation,
            primary: Some(Box::new(GenericProjectError::Sqlite {
                operation: commit_operation,
                source,
            })),
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::CommitFailedButTransactionClosed,
            },
        }),
        Err(source) => {
            let rollback = execute_transaction_control(
                transaction,
                performance,
                scope,
                SqliteTransactionControl::Rollback,
                "ROLLBACK",
            );
            let rollback_autocommit = transaction.is_autocommit();
            match rollback {
                Ok(()) if rollback_autocommit => {
                    Err(GenericProjectError::TransactionNotCommitted {
                        operation: commit_operation,
                        source,
                    })
                }
                Ok(()) => Err(GenericProjectError::TransactionOutcomeUnknown {
                    operation: commit_operation,
                    primary: Some(Box::new(GenericProjectError::Sqlite {
                        operation: commit_operation,
                        source,
                    })),
                    finalization: GenericTransactionFinalizationFailure::InvalidState {
                        violation:
                            GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive,
                    },
                }),
                Err(rollback) => Err(GenericProjectError::TransactionOutcomeUnknown {
                    operation: commit_operation,
                    primary: Some(Box::new(GenericProjectError::Sqlite {
                        operation: commit_operation,
                        source,
                    })),
                    finalization: GenericTransactionFinalizationFailure::Sqlite {
                        operation: rollback_operation,
                        source: rollback,
                    },
                }),
            }
        }
    };
    drop(suspension);
    result
}
