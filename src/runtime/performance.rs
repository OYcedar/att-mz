//! 单次命令独享的性能事实计数器。
//!
//! 这里记录 ATT 自己能够在真实执行边界精确观察的调用次数。计数器不参与业务判断，
//! 不形成容量限制，也不进入用户配置。每次命令创建独立实例，因此并行测试和并行进程
//! 不共享重置点或全局状态。

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// ATT 主动发出 SQLite 显式事务控制语句的职责范围。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqliteTransactionScope {
    ReadSnapshot,
    WritePlan,
    DatabaseInitialization,
    Interactive,
}

/// 一条 SQLite 显式事务控制语句。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqliteTransactionControl {
    Begin,
    Commit,
    Rollback,
}

/// 一类 SQLite 控制语句的调用与成功次数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionControlCount {
    pub(crate) attempted: u64,
    pub(crate) succeeded: u64,
}

/// 一个职责范围内三类显式事务控制语句的精确计数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqliteTransactionScopeCount {
    pub(crate) begin: TransactionControlCount,
    pub(crate) commit: TransactionControlCount,
    pub(crate) rollback: TransactionControlCount,
}

/// 单次命令的 SQLite 显式事务控制汇总。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqliteTransactionSummary {
    pub(crate) read_snapshot: SqliteTransactionScopeCount,
    pub(crate) write_plan: SqliteTransactionScopeCount,
    pub(crate) database_initialization: SqliteTransactionScopeCount,
    pub(crate) interactive: SqliteTransactionScopeCount,
}

impl SqliteTransactionSummary {
    pub(crate) const fn attempted_total(self) -> u64 {
        scope_attempted_total(self.read_snapshot)
            .saturating_add(scope_attempted_total(self.write_plan))
            .saturating_add(scope_attempted_total(self.database_initialization))
            .saturating_add(scope_attempted_total(self.interactive))
    }
}

const fn scope_attempted_total(count: SqliteTransactionScopeCount) -> u64 {
    count
        .begin
        .attempted
        .saturating_add(count.commit.attempted)
        .saturating_add(count.rollback.attempted)
}

/// WriteBack 完整 candidate 树校验的开始与完成次数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateValidationCount {
    pub(crate) started: u64,
    pub(crate) completed: u64,
}

/// 项目日志与性能 runner 共同消费的单次命令闭集快照。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunPerformanceSnapshot {
    pub(crate) sqlite_transactions: SqliteTransactionSummary,
    pub(crate) candidate_validations: CandidateValidationCount,
}

#[derive(Debug, Default)]
struct ControlCounter {
    attempted: AtomicU64,
    succeeded: AtomicU64,
}

impl ControlCounter {
    fn attempted(&self) {
        increment_saturating(&self.attempted);
    }

    fn succeeded(&self) {
        increment_saturating(&self.succeeded);
    }

    fn snapshot(&self) -> TransactionControlCount {
        TransactionControlCount {
            attempted: self.attempted.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
struct ScopeCounters {
    begin: ControlCounter,
    commit: ControlCounter,
    rollback: ControlCounter,
}

impl ScopeCounters {
    fn control(&self, control: SqliteTransactionControl) -> &ControlCounter {
        match control {
            SqliteTransactionControl::Begin => &self.begin,
            SqliteTransactionControl::Commit => &self.commit,
            SqliteTransactionControl::Rollback => &self.rollback,
        }
    }

    fn snapshot(&self) -> SqliteTransactionScopeCount {
        SqliteTransactionScopeCount {
            begin: self.begin.snapshot(),
            commit: self.commit.snapshot(),
            rollback: self.rollback.snapshot(),
        }
    }
}

/// 单次命令独享的并发安全计数器。
#[derive(Debug, Default)]
pub(crate) struct RunPerformanceCounters {
    read_snapshot: ScopeCounters,
    write_plan: ScopeCounters,
    database_initialization: ScopeCounters,
    interactive: ScopeCounters,
    candidate_validation_started: AtomicU64,
    candidate_validation_completed: AtomicU64,
}

impl RunPerformanceCounters {
    fn sqlite_scope(&self, scope: SqliteTransactionScope) -> &ScopeCounters {
        match scope {
            SqliteTransactionScope::ReadSnapshot => &self.read_snapshot,
            SqliteTransactionScope::WritePlan => &self.write_plan,
            SqliteTransactionScope::DatabaseInitialization => &self.database_initialization,
            SqliteTransactionScope::Interactive => &self.interactive,
        }
    }

    pub(crate) fn sqlite_control_attempted(
        &self,
        scope: SqliteTransactionScope,
        control: SqliteTransactionControl,
    ) {
        self.sqlite_scope(scope).control(control).attempted();
    }

    pub(crate) fn sqlite_control_succeeded(
        &self,
        scope: SqliteTransactionScope,
        control: SqliteTransactionControl,
    ) {
        self.sqlite_scope(scope).control(control).succeeded();
    }

    pub(crate) fn candidate_validation_started(&self) {
        increment_saturating(&self.candidate_validation_started);
    }

    pub(crate) fn candidate_validation_completed(&self) {
        increment_saturating(&self.candidate_validation_completed);
    }

    pub(crate) fn snapshot(&self) -> RunPerformanceSnapshot {
        RunPerformanceSnapshot {
            sqlite_transactions: SqliteTransactionSummary {
                read_snapshot: self.read_snapshot.snapshot(),
                write_plan: self.write_plan.snapshot(),
                database_initialization: self.database_initialization.snapshot(),
                interactive: self.interactive.snapshot(),
            },
            candidate_validations: CandidateValidationCount {
                started: self.candidate_validation_started.load(Ordering::Relaxed),
                completed: self.candidate_validation_completed.load(Ordering::Relaxed),
            },
        }
    }
}

fn increment_saturating(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_counters_are_exact_and_instance_isolated() {
        let first = RunPerformanceCounters::default();
        let second = RunPerformanceCounters::default();

        first.sqlite_control_attempted(
            SqliteTransactionScope::WritePlan,
            SqliteTransactionControl::Begin,
        );
        first.sqlite_control_succeeded(
            SqliteTransactionScope::WritePlan,
            SqliteTransactionControl::Begin,
        );
        first.sqlite_control_attempted(
            SqliteTransactionScope::WritePlan,
            SqliteTransactionControl::Commit,
        );
        first.candidate_validation_started();
        first.candidate_validation_completed();

        let snapshot = first.snapshot();
        assert_eq!(snapshot.sqlite_transactions.attempted_total(), 2);
        assert_eq!(snapshot.sqlite_transactions.write_plan.begin.attempted, 1);
        assert_eq!(snapshot.sqlite_transactions.write_plan.begin.succeeded, 1);
        assert_eq!(snapshot.sqlite_transactions.write_plan.commit.attempted, 1);
        assert_eq!(snapshot.sqlite_transactions.write_plan.commit.succeeded, 0);
        assert_eq!(
            snapshot.candidate_validations,
            CandidateValidationCount {
                started: 1,
                completed: 1,
            }
        );
        assert_eq!(second.snapshot(), RunPerformanceSnapshot::default());
    }
}
