//! Generic 命令入口与 Init / Extract 的生产装配。

mod diagnostics;
mod lifecycle;
mod project_commands;
mod project_log;
mod prompt;
mod tasks;
#[cfg(test)]
mod tests;
mod translate;
mod write_back;

use crate::application::config::ConfiguredGenericCommand;
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::TerminationSignals;
use crate::diagnostic::{GenericDiagnosticStage, StateEffect};
use crate::execution::CooperativeCancellation;
use crate::generic::{ExtractOutcome, GenericInitRequest, GenericProject, GenericProjectStore};
use crate::i18n::UiLocale;
use crate::manual::ManualCommandSummary;
#[cfg(not(test))]
use crate::progress::ProgressObserver;
use crate::progress::ProgressSnapshot;
use crate::project_lease::{ProjectCommandLeaseProvider, ProjectCommandLeaseService};
use crate::project_name::ProjectName;
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLogCommand, ProjectLogEngine, ProjectLogEvent, ResolvedRunPlan, RunPlanFinalization,
    RunPlanTransactionState, RunPlanValueSource,
};
#[cfg(test)]
pub(crate) use diagnostics::GenericCommandError;
pub(crate) use diagnostics::{GenericShutdownError, generic_command_error_report};
use diagnostics::{generic_file_system_build_failure, generic_project_lease_failure};
use lifecycle::{
    Driven, GenericCommandPanicContext, GenericProgressPhase, catch_generic_command_panic,
    drive_and_shutdown, drive_extract_and_shutdown, ensure_generic_operation_running,
    generic_command_panic_context, generic_terminal_progress, run_project_blocking,
};
pub(crate) use lifecycle::{GenericCommandRunReport, GenericCommandRunResult};
use project_log::{
    generic_extract_project_log_state, generic_project_log_handle, generic_project_log_slot,
    install_generic_project_log, resolve_generic_extract_run_plan,
    start_existing_generic_project_log, start_generic_extract_project_log,
    take_generic_project_log,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GENERIC_ENGINE_NAME: &str = "generic";
#[derive(Clone, Debug)]
pub(crate) enum GenericCommandOutput {
    Init {
        project: GenericProject,
    },
    Extract {
        project: ProjectName,
        outcome: ExtractOutcome,
    },
    Translate {
        project: ProjectName,
        profile_id: String,
        summary: GenericTranslationSummary,
    },
    WriteBack {
        project: ProjectName,
        output_root: PathBuf,
        translated_units: usize,
        retained_source_units: usize,
    },
    Manual {
        summary: ManualCommandSummary,
    },
    Lua {
        project: ProjectName,
    },
}

/// 一次 Generic Translate 的正常业务结果。
///
/// 模型请求不可用、响应部分无效和 CAS 冲突都属于可继续的部分结果，不升级为命令错误。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GenericTranslationSummary {
    pub(crate) total_tasks: usize,
    pub(crate) started_tasks: usize,
    pub(crate) not_started_tasks: usize,
    pub(crate) complete_tasks: usize,
    pub(crate) partial_tasks: usize,
    pub(crate) unavailable_tasks: usize,
    pub(crate) planned_units: usize,
    pub(crate) remaining_units: usize,
    pub(crate) rejected_units: usize,
    pub(crate) cleared_units: usize,
    pub(crate) reused_units: usize,
    pub(crate) accepted_units: usize,
    pub(crate) written_units: usize,
    pub(crate) conflicted_units: usize,
    pub(crate) response_problems: usize,
    pub(crate) recoverable_request_exhaustions: usize,
    pub(crate) request_admission_stopped: bool,
}

impl GenericTranslationSummary {
    /// Task 协议问题和 Unit 写入冲突都表示项目仍有未完成内容。
    pub(crate) const fn is_incomplete(self) -> bool {
        self.partial_tasks > 0
            || self.unavailable_tasks > 0
            || self.not_started_tasks > 0
            || self.remaining_units > 0
            || self.rejected_units > 0
            || self.conflicted_units > 0
            || self.response_problems > 0
    }
}

/// Generic 的生产命令执行器。
pub(crate) struct ProductionGenericCommandRunner {
    locale: UiLocale,
    panic_context: Option<GenericCommandPanicContext>,
}

impl ProductionGenericCommandRunner {
    pub(crate) const fn new(locale: UiLocale) -> Self {
        Self {
            locale,
            panic_context: None,
        }
    }

    fn panic_context(&self) -> &GenericCommandPanicContext {
        self.panic_context
            .as_ref()
            .expect("Generic 命令进入生产执行前必须建立 panic 上下文")
    }

    pub(crate) async fn run(
        mut self,
        command: ConfiguredGenericCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let panic_context = generic_command_panic_context(&command);
        self.panic_context = Some(panic_context.clone());
        catch_generic_command_panic(
            panic_context,
            self.run_without_panic_boundary(command, termination_signals),
        )
        .await
    }

    async fn run_without_panic_boundary(
        self,
        command: ConfiguredGenericCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        match command {
            ConfiguredGenericCommand::Init { arguments, common } => {
                let performance = Arc::new(RunPerformanceCounters::default());
                let file_system =
                    match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
                        Ok(file_system) => file_system,
                        Err(source) => {
                            return GenericCommandRunReport::failed(
                                generic_file_system_build_failure(source),
                            );
                        }
                    };
                let project_log = generic_project_log_slot();
                let cancellation = CooperativeCancellation::default();
                let progress = generic_terminal_progress(self.locale);
                let operation_progress = progress.observer();
                let project_name = arguments.project.name.clone();
                let workspace_root = generic_workspace(common.projects_root(), &project_name);
                let lease_provider = ProjectCommandLeaseService::new(
                    common.projects_root().to_path_buf(),
                    GENERIC_ENGINE_NAME,
                    file_system.clone(),
                );
                let operation_cancellation = cancellation.clone();
                let cancellation_file_system = file_system.clone();
                let operation_project_log = Arc::clone(&project_log);
                let operation_panic_context = self.panic_context().clone();
                let operation_performance = Arc::clone(&performance);
                let locale = self.locale;
                let operation = async move {
                    ensure_generic_operation_running(&operation_cancellation)?;
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::Initializing,
                    ));
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(generic_project_lease_failure)?;
                    ensure_generic_operation_running(&operation_cancellation)?;
                    let request = GenericInitRequest {
                        project_name,
                        workspace_root,
                        source_root: arguments.path,
                        source_language: arguments.source_language,
                        target_language: arguments.target_language,
                    };
                    let database_path = request.workspace_root.join("project.db");
                    let init_cancellation = operation_cancellation.clone();
                    let (_, project) = run_project_blocking(
                        GenericDiagnosticStage::Init,
                        StateEffect::Unchanged,
                        database_path,
                        move || {
                            GenericProjectStore::initialize_with_cancellation(
                                request,
                                init_cancellation,
                                operation_performance,
                            )
                        },
                    )
                    .await?;
                    install_generic_project_log(
                        &operation_project_log,
                        start_command_log(CommandLogStart {
                            common: &common,
                            locale,
                            engine: ProjectLogEngine::Generic,
                            project: project.project_name().as_str(),
                            command: ProjectLogCommand::Init,
                            performance,
                            selected_api_key_redactor: None,
                        }),
                    );
                    operation_panic_context.observe_project_log_slot(&operation_project_log);
                    if let Some(handle) = generic_project_log_handle(&operation_project_log) {
                        handle.emit(ProjectLogEvent::RunPlanResolved {
                            plan: ResolvedRunPlan::init(
                                RunPlanValueSource::Explicit,
                                project.source_root(),
                            ),
                        });
                        handle.emit(ProjectLogEvent::RunPlanFinalized {
                            database: crate::diagnostic::SafePath::new(project.database_path()),
                            result: RunPlanFinalization::Saved {
                                transaction: RunPlanTransactionState::Committed,
                                run_continues: false,
                            },
                        });
                    }
                    Ok(GenericCommandOutput::Init { project })
                };
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
            ConfiguredGenericCommand::Extract {
                project_name,
                common,
            } => {
                let performance = Arc::new(RunPerformanceCounters::default());
                let project_log = generic_project_log_slot();
                let extract_project_log = generic_extract_project_log_state();
                start_existing_generic_project_log(
                    &project_log,
                    &common,
                    self.locale,
                    &project_name,
                    ProjectLogCommand::Extract,
                    Arc::clone(&performance),
                );
                self.panic_context().observe_project_log_slot(&project_log);
                let file_system =
                    match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
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
                let progress = generic_terminal_progress(self.locale);
                let operation_progress = progress.observer();
                let store = GenericProjectStore::for_workspace_with_cancellation(
                    generic_workspace(common.projects_root(), &project_name),
                    cancellation.clone(),
                    Arc::clone(&performance),
                );
                let lease_provider = ProjectCommandLeaseService::new(
                    common.projects_root().to_path_buf(),
                    GENERIC_ENGINE_NAME,
                    file_system.clone(),
                );
                let output_name = project_name.clone();
                let operation_cancellation = cancellation.clone();
                let cancellation_file_system = file_system.clone();
                let operation_project_log = Arc::clone(&project_log);
                let operation_extract_project_log = Arc::clone(&extract_project_log);
                let operation = async move {
                    ensure_generic_operation_running(&operation_cancellation)?;
                    start_generic_extract_project_log(
                        &operation_project_log,
                        &operation_extract_project_log,
                    );
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::Extracting,
                    ));
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(generic_project_lease_failure)?;
                    ensure_generic_operation_running(&operation_cancellation)?;
                    let database_path = store.database_path().to_path_buf();
                    let open_store = store.clone();
                    let project = run_project_blocking(
                        GenericDiagnosticStage::ProjectOpening,
                        StateEffect::Unchanged,
                        database_path.clone(),
                        move || open_store.open(),
                    )
                    .await?;
                    resolve_generic_extract_run_plan(
                        &operation_project_log,
                        &operation_extract_project_log,
                        project.database_path(),
                    );
                    let outcome = run_project_blocking(
                        GenericDiagnosticStage::Extract,
                        StateEffect::ProgressPreserved,
                        database_path,
                        move || store.extract(),
                    )
                    .await?;
                    Ok(GenericCommandOutput::Extract {
                        project: output_name,
                        outcome,
                    })
                };
                drive_extract_and_shutdown(
                    operation,
                    termination_signals,
                    move || {
                        cancellation.request();
                        cancellation_file_system.cancel_waits();
                    },
                    file_system,
                    Vec::new(),
                    project_log,
                    extract_project_log,
                    progress,
                )
                .await
            }
            ConfiguredGenericCommand::Translate(command) => {
                self.run_translate(*command, termination_signals).await
            }
            ConfiguredGenericCommand::WriteBack(command) => {
                self.run_write_back(command, termination_signals).await
            }
            ConfiguredGenericCommand::Manual(command) => {
                self.run_manual(command, termination_signals).await
            }
            ConfiguredGenericCommand::Translation(command) => {
                self.run_manual(command, termination_signals).await
            }
            ConfiguredGenericCommand::Lua(command) => {
                self.run_lua(command, termination_signals).await
            }
        }
    }
}

fn generic_task_ordinal(task_index: usize) -> u64 {
    generic_count(task_index)
        .checked_add(1)
        .expect("Generic task ordinal 不得溢出")
}

fn generic_count(value: usize) -> u64 {
    u64::try_from(value).expect("当前平台 usize 必须能够无损表示为 u64")
}

fn generic_workspace(projects_root: &Path, project: &ProjectName) -> PathBuf {
    projects_root
        .join(GENERIC_ENGINE_NAME)
        .join(project.as_str())
}
