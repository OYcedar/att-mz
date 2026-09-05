//! RPG Maker 运行方案的最终保存及业务结果合并。

use super::error::ProductionCommandError;
use super::lifecycle::ShutdownFailures;
use super::progress::business_completed;
use crate::application::project_log::ActiveProjectLog;
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::ReportedFailure;
use crate::diagnostic::{SafePath, StateEffect};
use crate::execution::OperationCompletion;
use crate::rpg_maker::project_database::{
    FinalProjectRunPlanPersistenceService, ProjectRunPlanFinalizer, ProjectRunPlanReplaceError,
    ProjectRunPlanReplacement,
};
use crate::runtime::project_log::{
    DiagnosticScope, ProjectLogEvent, RunPlanFinalization, RunPlanTransactionState,
};
use crate::runtime::sqlite::{RusqliteFinalTransactionExecutor, SqliteRuntimeError};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) struct RunPlanFinalizationInput {
    pub(super) database_path: PathBuf,
    pub(super) replacement: ProjectRunPlanReplacement,
    pub(super) sqlite_configuration: crate::runtime::sqlite::RusqliteStorageConfiguration,
}

pub(super) async fn finalize_run_plan<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
    input: RunPlanFinalizationInput,
    project_log: &ActiveProjectLog,
    termination_signals: &mut TerminationSignals,
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    if !business_completed(&execution) || !shutdown.is_empty() {
        return execution;
    }
    let RunPlanFinalizationInput {
        database_path,
        replacement,
        sqlite_configuration,
    } = input;
    let transaction_executor = RusqliteFinalTransactionExecutor::new_with_performance(
        sqlite_configuration,
        Arc::clone(project_log.performance()),
    );
    let finalizer = FinalProjectRunPlanPersistenceService::new(transaction_executor.clone());
    let finalization = drive_with_termination(
        finalizer.replace_final(database_path.clone(), replacement),
        termination_signals,
        || transaction_executor.cancel_waits(),
        on_cancellation,
    )
    .await;
    merge_run_plan_finalization(execution, finalization, &database_path, project_log)
}

pub(super) fn merge_run_plan_finalization<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    finalization: DrivenCommand<Result<(), ProjectRunPlanReplaceError<SqliteRuntimeError>>>,
    database_path: &Path,
    project_log: &ActiveProjectLog,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    match finalization {
        DrivenCommand::Finished(result) => {
            match observe_run_plan_result(result, database_path, project_log) {
                Ok(()) => finish_successful_execution(execution),
                Err(error) => replace_success_with_plan_error(execution, Err(error)),
            }
        }
        DrivenCommand::Interrupted(result) => match result {
            // 保存期间到达信号但保存已成功：业务结果与运行方案都已生效。
            // 最终命令状态只表达已经完整完成的结果，不保留过时的中断形态。
            Ok(()) => {
                emit_run_plan_saved(project_log, database_path);
                finish_successful_execution(execution)
            }
            // 信号取消了方案保存本身：业务结果已完整生效并按成功呈现，
            // 方案未保存进入项目日志，由下次运行重新提供输入。
            Err(error) if run_plan_wait_was_cancelled(&error) => {
                emit_run_plan_error_fact(&error, database_path, true, project_log);
                finish_successful_execution(execution)
            }
            Err(error) => DrivenCommand::Interrupted(Err(observe_run_plan_error(
                error,
                database_path,
                false,
                project_log,
            ))),
        },
        DrivenCommand::SignalFailed { source, result } => {
            let result = match result {
                Ok(()) => {
                    emit_run_plan_saved(project_log, database_path);
                    take_successful_execution_result(execution)
                }
                Err(error) if run_plan_wait_was_cancelled(&error) => {
                    emit_run_plan_error_fact(&error, database_path, true, project_log);
                    take_successful_execution_result(execution)
                }
                Err(error) => Err(observe_run_plan_error(
                    error,
                    database_path,
                    false,
                    project_log,
                )),
            };
            DrivenCommand::SignalFailed { source, result }
        }
    }
}

pub(super) fn observe_run_plan_result(
    result: Result<(), ProjectRunPlanReplaceError<SqliteRuntimeError>>,
    database_path: &Path,
    project_log: &ActiveProjectLog,
) -> Result<(), ProductionCommandError> {
    match result {
        Ok(()) => {
            emit_run_plan_saved(project_log, database_path);
            Ok(())
        }
        Err(error) => Err(observe_run_plan_error(
            error,
            database_path,
            false,
            project_log,
        )),
    }
}

pub(super) fn emit_run_plan_saved(project_log: &ActiveProjectLog, database_path: &Path) {
    project_log
        .handle()
        .emit(ProjectLogEvent::RunPlanFinalized {
            database: SafePath::new(database_path),
            result: RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: true,
            },
        });
}

pub(super) fn observe_run_plan_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
    database_path: &Path,
    run_continues: bool,
    project_log: &ActiveProjectLog,
) -> ProductionCommandError {
    emit_run_plan_error_fact(&error, database_path, run_continues, project_log);
    map_run_plan_replace_error(error)
}

pub(super) fn emit_run_plan_error_fact(
    error: &ProjectRunPlanReplaceError<SqliteRuntimeError>,
    database_path: &Path,
    run_continues: bool,
    project_log: &ActiveProjectLog,
) {
    let report = error.diagnostic_report();
    let Some(diagnostic) = project_log
        .handle()
        .record_diagnostic(DiagnosticScope::RunPlan, report)
    else {
        return;
    };
    let result = match error {
        ProjectRunPlanReplaceError::DatabaseNotFound { .. } => RunPlanFinalization::NotSaved {
            transaction: RunPlanTransactionState::NotStarted,
            run_continues,
            diagnostic,
        },
        ProjectRunPlanReplaceError::RequirementFailed { .. }
        | ProjectRunPlanReplaceError::RequirementFinalizationFailed { .. }
        | ProjectRunPlanReplaceError::RollbackConfirmed { .. } => RunPlanFinalization::NotSaved {
            transaction: RunPlanTransactionState::RolledBack,
            run_continues,
            diagnostic,
        },
        ProjectRunPlanReplaceError::RequirementOutcomeUnknown { .. }
        | ProjectRunPlanReplaceError::OutcomeUnknown { .. } => {
            RunPlanFinalization::OutcomeUnknown {
                transaction: RunPlanTransactionState::OutcomeUnknown,
                run_continues,
                diagnostic,
            }
        }
        ProjectRunPlanReplaceError::CommittedButFinalizationFailed { .. } => {
            RunPlanFinalization::SavedFinalizationFailed {
                transaction: RunPlanTransactionState::Committed,
                run_continues,
                diagnostic,
            }
        }
    };
    project_log
        .handle()
        .emit(ProjectLogEvent::RunPlanFinalized {
            database: SafePath::new(database_path),
            result,
        });
}

pub(super) fn run_plan_wait_was_cancelled(
    error: &ProjectRunPlanReplaceError<SqliteRuntimeError>,
) -> bool {
    matches!(
        error,
        ProjectRunPlanReplaceError::RollbackConfirmed {
            source: SqliteRuntimeError::Cancelled { .. },
            ..
        }
    )
}

/// 运行方案最终化没有产生根失败时，业务 `Completed` 是命令唯一有效的最终状态。
pub(super) fn finish_successful_execution<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    DrivenCommand::Finished(take_successful_execution_result(execution))
}

pub(super) fn take_successful_execution_result<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> Result<OperationCompletion<T>, ProductionCommandError> {
    match execution {
        DrivenCommand::Finished(result @ Ok(OperationCompletion::Completed(_)))
        | DrivenCommand::Interrupted(result @ Ok(OperationCompletion::Completed(_))) => result,
        _ => unreachable!("只有成功业务执行才会进入运行方案最终化"),
    }
}

pub(super) fn map_run_plan_replace_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
) -> ProductionCommandError {
    let diagnostic = error.diagnostic_report();
    let effect = diagnostic.effect();
    let report = ReportedFailure::new(diagnostic, error);
    match effect {
        StateEffect::OutcomeUnknown => {
            ProductionCommandError::RunPlanOutcomeUnknown(Box::new(report))
        }
        StateEffect::AppliedFinalizationFailed => {
            ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
        }
        StateEffect::RecoveryRequired => ProductionCommandError::RecoveryRequired(Box::new(report)),
        StateEffect::AppliedRunPlanNotSaved
        | StateEffect::Unchanged
        | StateEffect::ProgressPreserved
        | StateEffect::Applied => {
            ProductionCommandError::ResultAppliedButRunPlanNotSaved(Box::new(report))
        }
    }
}

pub(super) fn replace_success_with_plan_error<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    plan_result: Result<(), ProductionCommandError>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    match plan_result {
        Ok(()) => execution,
        Err(error) if business_completed(&execution) => DrivenCommand::Finished(Err(error)),
        Err(_) => execution,
    }
}

#[derive(Debug)]
pub(super) enum RunPlanResolutionError {
    InitPathRequired,
    NoReusableExtractPlan,
    ProfileRequired,
    SavedProfileUnavailable { profile_id: String },
}

impl fmt::Display for RunPlanResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InitPathRequired => {
                formatter.write_str("首次 Init 尚无可复用的来源路径，请提供 --path")
            }
            Self::NoReusableExtractPlan => {
                formatter.write_str("该项目尚未保存过 Extract 方案，请至少提供一个提取选项")
            }
            Self::ProfileRequired => {
                formatter.write_str("该项目尚未保存过 Translate Profile，请提供 PROFILE_ID")
            }
            Self::SavedProfileUnavailable { profile_id } => write!(
                formatter,
                "上次成功使用的 Profile {profile_id} 已不在当前配置中，请显式指定可用 Profile",
            ),
        }
    }
}

impl Error for RunPlanResolutionError {}
