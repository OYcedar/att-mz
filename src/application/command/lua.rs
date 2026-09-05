//! RPG Maker 项目 Lua 的预检、执行和事务失败衔接。

use super::error::{ProductionCommandError, SignalOutcomeSource};
use super::lifecycle::{
    CommandRunResult, ProductionCommandRootGuard, ProductionCommandRunReport, drive_project_lease,
    interrupted_non_cancellation_error, report_with_shutdown, shutdown_report,
};
use super::progress::{
    ProductionProgressObserver, ProjectLuaProgressPhase, business_completed,
    defer_terminal_progress_status, finish_progress_business_state, finish_terminal_progress,
    pending_project_log_for_execution, pending_project_log_with_occurrence, progress_safe_stopping,
    project_log_engine, project_lua_phase_code, project_lua_terminal_progress, record_failed_phase,
};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::ConfiguredProjectLuaCommand;
use crate::application::project_log::{CommandLogStart, ProjectLogLuaPrintSink, start_command_log};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::{ProgressObserver, ProgressSnapshot};
use crate::project_lease::ProjectCommandLeaseService;
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaFailure, ProjectLuaProgram, ProjectLuaProject,
    ProjectLuaRunError, ProjectLuaRunRequest, compile_project_lua_program_with_cancellation,
    rpg_maker_project_lua_adapter, run_project_lua,
};
use crate::rpg_maker::project_database::ProjectWorkspaceLayout;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{DiagnosticScope, ProjectLogCommand, ProjectLogEvent};
use crate::storage::file_system::FileReader;
use rusqlite::{Connection, OpenFlags};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_atomic_project_lua(
        self,
        command: ConfiguredProjectLuaCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = project_lua_terminal_progress(self.locale);
        let cancellation = CooperativeCancellation::default();
        let lua_cancellation = ProjectLuaCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let roots = match ProductionCommandRootGuard::start_init(
            sqlite_configuration,
            Arc::clone(&performance),
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let project_name = command.project_name().clone();
        let language_modules = command.language_modules().clone();
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let script_path = command.script().script_path().to_path_buf();
        let script_read = drive_with_termination(
            file_system.read_file(script_path),
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
            },
        )
        .await;
        let script = match script_read {
            DrivenCommand::Finished(Ok(script)) => script,
            DrivenCommand::Finished(Err(source)) => {
                let shutdown = roots.shutdown().await;
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::lua_script_read(source),
                    shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = match result {
                    Ok(script) => {
                        drop(script);
                        None
                    }
                    Err(source) => {
                        let error = ProductionCommandError::lua_script_read(source);
                        (!error.was_cancelled_wait()).then_some(error)
                    }
                };
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = result.map_or_else(
                    |error| {
                        SignalOutcomeSource::CommandFailed(ProductionCommandError::lua_script_read(
                            error,
                        ))
                    },
                    |_| SignalOutcomeSource::Cancelled,
                );
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };

        let script_identity = script.resolved_path().to_string_lossy().into_owned();
        let script_source = script.into_bytes();
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: project_log_engine(self.layout),
            project: project_name.as_str(),
            command: ProjectLogCommand::Lua,
            performance,
            selected_api_key_redactor: None,
        });
        self.panic_boundary.observe_project_log(&project_log);
        let program_arguments = command.arguments().to_vec();
        let preflight_cancellation = lua_cancellation.clone();
        let preflight_database_path = database_path.clone();
        let preparation = drive_with_termination(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let program =
                        ProjectLuaProgram::new(script_identity, script_source, program_arguments);
                    compile_project_lua_program_with_cancellation(
                        &program,
                        &preflight_cancellation,
                    )?;
                    Ok::<_, ProjectLuaFailure>(program)
                })
                .await
                .map_err(ProductionCommandError::project_lua_worker)?;
                match result {
                    Ok(prepared) => Ok(OperationCompletion::Completed(prepared)),
                    Err(ProjectLuaFailure::Cancelled) => Ok(OperationCompletion::Cancelled),
                    Err(source) => Err(ProductionCommandError::project_lua_preflight(
                        source,
                        &preflight_database_path,
                    )),
                }
            },
            termination_signals,
            || {
                cancellation.request();
                lua_cancellation.cancel();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
                project_log
                    .handle()
                    .emit(ProjectLogEvent::CancellationRequested {
                        confirmed: 0,
                        total: None,
                    });
            },
        )
        .await;
        let program = match preparation {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(prepared))) => prepared,
            terminal => {
                let execution = terminal.map(|result| {
                    result
                        .map(|_completion| OperationCompletion::<RpgMakerCommandOutput>::Cancelled)
                });
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                let pending = pending_project_log_for_execution(project_log, &execution, &shutdown);
                return ProductionCommandRunReport::from_completion_with_project_log(
                    execution,
                    shutdown,
                    Some(pending),
                );
            }
        };

        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine().storage_name(),
            file_system.clone(),
        );
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
                project_log
                    .handle()
                    .emit(ProjectLogEvent::CancellationRequested {
                        confirmed: 0,
                        total: None,
                    });
            },
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                let pending = project_log.pending_failure(report_with_shutdown(
                    error.failure_report().report().clone(),
                    &shutdown,
                ));
                return ProductionCommandRunReport::construction_failed_with_shutdown_and_project_log(
                    error,
                    shutdown,
                    Some(pending),
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                if let Some(error) = error {
                    let pending = project_log.pending_failure(report_with_shutdown(
                        error.failure_report().report().clone(),
                        &shutdown,
                    ));
                    return ProductionCommandRunReport::construction_failed_with_shutdown_and_project_log(
                        error,
                        shutdown,
                        Some(pending),
                    );
                }
                let pending = if let Some(report) = shutdown_report(&shutdown) {
                    project_log.pending_failure(report)
                } else {
                    project_log.pending_cancelled()
                };
                return ProductionCommandRunReport {
                    result: CommandRunResult::Interrupted,
                    shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
                    pending_project_log: Some(pending),
                    panic_log_path: None,
                    selected_api_key_redactor: None,
                    translation_summary: None,
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let error = ProductionCommandError::signal(source, outcome);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                let pending = project_log.pending_failure(report_with_shutdown(
                    error.failure_report().report().clone(),
                    &shutdown,
                ));
                return ProductionCommandRunReport::construction_failed_with_shutdown_and_project_log(
                    error,
                    shutdown,
                    Some(pending),
                );
            }
        };

        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            project_lua_phase_code,
        );
        progress_observer.observe(ProgressSnapshot::indeterminate(
            ProjectLuaProgressPhase::Running,
        ));
        let execution_database_path = database_path.clone();
        let request = ProjectLuaRunRequest::new(
            ProjectLuaProject::new(project_name.as_str(), self.layout.engine().into()),
            program,
            rpg_maker_project_lua_adapter(self.layout.engine(), language_modules),
        )
        .with_cancellation(lua_cancellation.clone())
        .with_print_sink(Arc::new(ProjectLogLuaPrintSink::from_active(&project_log)));
        let execution = drive_with_termination(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let connection = Connection::open_with_flags(
                        &execution_database_path,
                        OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .map_err(|source| ProjectLuaExecutionError::Open {
                        path: execution_database_path.clone(),
                        source,
                    })?;
                    run_project_lua(connection, request).map_err(|source| {
                        ProjectLuaExecutionError::Run {
                            path: execution_database_path,
                            source,
                        }
                    })
                })
                .await
                .map_err(ProductionCommandError::project_lua_worker)?;
                match result {
                    Ok(_) => Ok(OperationCompletion::Completed(RpgMakerCommandOutput::Lua {
                        project: project_name,
                    })),
                    Err(ProjectLuaExecutionError::Run { source: error, .. })
                        if project_lua_run_was_cancelled(&error) =>
                    {
                        Ok(OperationCompletion::Cancelled)
                    }
                    Err(error) => Err(ProductionCommandError::project_lua_execution(error)),
                }
            },
            termination_signals,
            || {
                cancellation.request();
                lua_cancellation.cancel();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log
                    .handle()
                    .emit(ProjectLogEvent::CancellationRequested { confirmed, total });
            },
        )
        .await;
        drop(project_lease_guard);
        if business_completed(&execution) {
            progress_observer.complete_phase(ProjectLuaProgressPhase::Running);
        }
        finish_progress_business_state(&progress_observer, &execution);
        let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
        let terminal_diagnostic = record_failed_phase(
            &progress_observer,
            &project_log,
            &execution,
            &shutdown,
            DiagnosticScope::Run,
        );
        let pending = pending_project_log_with_occurrence(
            project_log,
            &execution,
            &shutdown,
            terminal_diagnostic,
        );
        ProductionCommandRunReport::from_completion_with_project_log(
            execution,
            shutdown,
            Some(pending),
        )
    }
}
#[derive(Debug)]
pub(super) enum ProjectLuaExecutionError {
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    Run {
        path: PathBuf,
        source: ProjectLuaRunError,
    },
}

impl fmt::Display for ProjectLuaExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => write!(
                formatter,
                "无法打开 RPG Maker 项目数据库 {}：{source}",
                path.display()
            ),
            Self::Run { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for ProjectLuaExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Run { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub(super) struct ProjectLuaPreflightError(pub(super) ProjectLuaFailure);

impl fmt::Display for ProjectLuaPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for ProjectLuaPreflightError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

pub(super) fn project_lua_run_was_cancelled(error: &ProjectLuaRunError) -> bool {
    matches!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
            | ProjectLuaRunError::Failed(ProjectLuaFailure::Cancelled)
            | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    )
}
