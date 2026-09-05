//! Generic Manual 与 Lua 命令的生产装配。

use super::diagnostics::{
    GenericCommandError, GenericLuaExecutionError, GenericLuaPreflightError,
    generic_blocking_join_failure, generic_file_system_build_failure, generic_manual_failure,
    generic_project_lease_failure, generic_read_file_failure,
};
use super::lifecycle::{
    Driven, GenericCommandRunReport, GenericProgressPhase, drive_and_shutdown,
    ensure_generic_operation_running, generic_terminal_progress,
};
use super::project_log::{
    generic_project_log_slot, install_generic_project_log, take_generic_project_log,
};
use super::{
    GENERIC_ENGINE_NAME, GenericCommandOutput, ProductionGenericCommandRunner, generic_workspace,
};
use crate::application::config::{ConfiguredManualCommand, ConfiguredProjectLuaCommand};
use crate::application::project_log::{CommandLogStart, ProjectLogLuaPrintSink, start_command_log};
use crate::application::termination::TerminationSignals;
use crate::diagnostic::{FileSystemDiagnosticStage, StateEffect};
use crate::execution::CooperativeCancellation;
use crate::generic::GenericProjectStore;
use crate::manual::execute_generic_manual_command;
#[cfg(not(test))]
use crate::progress::ProgressObserver;
use crate::progress::ProgressSnapshot;
use crate::project_lease::{ProjectCommandLeaseProvider, ProjectCommandLeaseService};
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaEngine, ProjectLuaFailure, ProjectLuaProgram,
    ProjectLuaProject, ProjectLuaRunRequest, compile_project_lua_program_with_cancellation,
    generic_project_lua_adapter_for_name, run_project_lua,
};
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{ProjectLogCommand, ProjectLogEngine};
use crate::storage::file_system::FileReader;
use rusqlite::Connection;
use std::sync::Arc;

impl ProductionGenericCommandRunner {
    pub(super) async fn run_manual(
        self,
        command: ConfiguredManualCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let file_system = match SystemFileSystem::new_with_performance(performance) {
            Ok(file_system) => file_system,
            Err(source) => {
                return GenericCommandRunReport::failed(generic_file_system_build_failure(source));
            }
        };
        let cancellation = CooperativeCancellation::default();
        let progress = generic_terminal_progress(self.locale);
        let project_log = generic_project_log_slot();
        let project = command.project_name().clone();
        let database_path =
            generic_workspace(command.common().projects_root(), &project).join("project.db");
        let operation = command.operation();
        let file = command.file().to_path_buf();
        let export_selection = command.export_selection().cloned();
        let language_modules = command.language_modules().cloned();
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let operation_project = project.clone();
        let operation_cancellation = cancellation.clone();
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            let _lease = lease_provider
                .acquire(&operation_project)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let blocking_cancellation = operation_cancellation.clone();
            let summary = tokio::task::spawn_blocking(move || {
                execute_generic_manual_command(
                    &database_path,
                    operation,
                    &file,
                    export_selection.as_ref(),
                    language_modules.as_ref(),
                    &blocking_cancellation,
                )
            })
            .await
            .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?
            .map_err(generic_manual_failure)?;
            Ok(GenericCommandOutput::Manual { summary })
        };
        let cancellation_file_system = file_system.clone();
        drive_and_shutdown(
            operation,
            termination_signals,
            move || {
                cancellation.request();
                cancellation_file_system.cancel_waits();
            },
            file_system,
            Vec::new(),
            project_log,
            progress,
        )
        .await
    }

    pub(super) async fn run_lua(
        self,
        command: ConfiguredProjectLuaCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let language_modules = command.language_modules().clone();
        let project_log = generic_project_log_slot();
        install_generic_project_log(
            &project_log,
            start_command_log(CommandLogStart {
                common: command.common(),
                locale: self.locale,
                engine: ProjectLogEngine::Generic,
                project: project_name.as_str(),
                command: ProjectLogCommand::Lua,
                performance: Arc::clone(&performance),
                selected_api_key_redactor: None,
            }),
        );
        self.panic_context().observe_project_log_slot(&project_log);
        let file_system = match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
            Ok(file_system) => file_system,
            Err(source) => {
                return GenericCommandRunReport::from_driven(
                    Driven::Finished(Err(generic_file_system_build_failure(source))),
                    Vec::new(),
                    take_generic_project_log(&project_log),
                );
            }
        };
        let cancellation = CooperativeCancellation::default();
        let lua_cancellation = ProjectLuaCancellation::default();
        let progress = generic_terminal_progress(self.locale);
        let operation_progress = progress.observer();
        let script_path = command.script().script_path().to_path_buf();
        let arguments = command.arguments().to_vec();
        let store = GenericProjectStore::for_workspace_with_cancellation(
            generic_workspace(command.common().projects_root(), &project_name),
            cancellation.clone(),
            Arc::clone(&performance),
        );
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let operation_file_system = file_system.clone();
        let operation_lua_cancellation = lua_cancellation.clone();
        let operation_cancellation = cancellation.clone();
        let cancellation_file_system = file_system.clone();
        let output_name = project_name.clone();
        let print_sink = {
            let project_log = project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            project_log.as_ref().map(|project_log| {
                Arc::new(ProjectLogLuaPrintSink::from_active(project_log))
                    as Arc<dyn crate::project_lua::ProjectLuaPrintSink>
            })
        };
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            let script = operation_file_system
                .read_file(script_path.clone())
                .await
                .map_err(|source| {
                    generic_read_file_failure(source, FileSystemDiagnosticStage::CommandPreparation)
                })?;
            let identity = script.resolved_path().to_string_lossy().into_owned();
            let source = script.into_bytes();
            let preflight_database_path = store.database_path().to_path_buf();
            let preflight_cancellation = operation_lua_cancellation.clone();
            let preparation = tokio::task::spawn_blocking(move || {
                let program = ProjectLuaProgram::new(identity, source, arguments);
                compile_project_lua_program_with_cancellation(&program, &preflight_cancellation)?;
                Ok::<_, ProjectLuaFailure>(program)
            })
            .await;
            let preparation = preparation
                .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?;
            let program = match preparation {
                Ok(prepared) => prepared,
                Err(ProjectLuaFailure::Cancelled) => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let report = source.preflight_diagnostic_report(&preflight_database_path);
                    let source = GenericLuaPreflightError(source);
                    return Err(GenericCommandError::reported(source, report));
                }
            };
            ensure_generic_operation_running(&operation_cancellation)?;

            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let database_path = store.database_path().to_path_buf();
            let lua_project_name = output_name.as_str().to_owned();
            let lua_adapter =
                generic_project_lua_adapter_for_name(lua_project_name.clone(), language_modules);
            let request = ProjectLuaRunRequest::new(
                ProjectLuaProject::new(lua_project_name, ProjectLuaEngine::Generic),
                program,
                lua_adapter,
            )
            .with_cancellation(operation_lua_cancellation);
            let request = match print_sink {
                Some(print_sink) => request.with_print_sink(print_sink),
                None => request,
            };
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::RunningLua,
            ));
            let diagnostic_database_path = database_path.clone();
            let execution = tokio::task::spawn_blocking(move || {
                let open_path = database_path.clone();
                let connection = Connection::open_with_flags(
                    &database_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                )
                .map_err(|source| GenericLuaExecutionError::Open {
                    path: open_path,
                    source,
                })?;
                run_project_lua(connection, request).map_err(GenericLuaExecutionError::Run)
            })
            .await;
            let execution = execution.map_err(|source| {
                generic_blocking_join_failure(source, StateEffect::OutcomeUnknown)
            })?;
            match execution {
                Ok(_) => {}
                Err(source) if source.is_cancelled() => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let report = source.diagnostic_report(&diagnostic_database_path);
                    return Err(GenericCommandError::reported(source, report));
                }
            }
            Ok(GenericCommandOutput::Lua {
                project: output_name,
            })
        };
        drive_and_shutdown(
            operation,
            termination_signals,
            move || {
                cancellation.request();
                lua_cancellation.cancel();
                cancellation_file_system.cancel_waits();
            },
            file_system,
            Vec::new(),
            project_log,
            progress,
        )
        .await
    }
}
