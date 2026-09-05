//! RPG Maker Translate 的项目资源、模型运行与运行方案衔接。

use super::business_log::ProductionBusinessLog;
use super::error::{ProductionCommandError, SignalOutcomeSource, map_translate_error};
use super::lifecycle::{
    ProductionCommandRootGuard, ProductionCommandRunReport, ProjectOpeningLocation,
    catch_translate_execution_panic, drive_existing_project_opening, drive_project_lease,
    interrupted_non_cancellation_error, map_completion, observed_construction_failure,
};
use super::progress::{
    ProductionProgressObserver, TranslateProgressPhase, business_completed,
    defer_terminal_progress_status, finish_progress_business_state, finish_terminal_progress,
    pending_project_log_for_translation_execution, progress_finalizing, progress_safe_stopping,
    progress_saving_plan, project_log_engine, translate_phase_code, translate_terminal_progress,
};
use super::run_plan::RunPlanResolutionError;
use super::run_plan::{RunPlanFinalizationInput, finalize_run_plan};
use super::translation_setup::{
    ProductionSelectedTranslationExecutionBuilder, load_additional_pem_roots,
};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::{ConfigurationLoadError, ConfiguredTranslateCommand};
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::{DiagnosticReport, StateEffect};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::{ProgressObserver, ProgressSnapshot};
use crate::project_lease::ProjectCommandLeaseService;
use crate::rpg_maker::project_database::{
    ProjectRunPlanPersistenceService, ProjectRunPlanReplacement, ProjectRunPlanRepository,
    ProjectWorkspaceLayout, TranslateRunPlan,
};
use crate::rpg_maker::translate::TranslateInput;
use crate::rpg_maker::translate::service::TranslateService;
use crate::rpg_maker::translate::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::llm::OpenAiCompatibleExecutor;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    PhaseStopOutcome, ProjectLogCommand, ProjectLogEvent, ResolvedRunPlan,
    RunPlanValueSource as ProjectLogValueSource,
};
use crate::translation::task_record::TaskRecordDiagnosticRecorder;
use std::path::Path;
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = translate_terminal_progress(self.locale);
        let explicit_profile = command.resolved_profile_id().map(str::to_owned);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let roots = match ProductionCommandRootGuard::start_main(
            command.cpu(),
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let cpu = roots.cpu().clone();
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let project_name = command.project_name().clone();
        let project_workspace =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name);
        let database_path = project_workspace.database_path().to_path_buf();
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
            },
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
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
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
        let plans = match repository.read(database_path.clone()).await {
            Ok(plans) => plans,
            Err(error) => {
                let error = ProductionCommandError::project_run_plan_read(error);
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let saved_translate = plans.translate().cloned();
        let (profile_id, profile_source) = match explicit_profile {
            Some(profile_id) => (profile_id, ProjectLogValueSource::Explicit),
            None => match saved_translate.as_ref() {
                Some(plan) => (
                    plan.profile_id().to_owned(),
                    ProjectLogValueSource::ProjectState,
                ),
                None => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        ProductionCommandError::run_plan_resolution(
                            RunPlanResolutionError::ProfileRequired,
                        ),
                        shutdown,
                    );
                }
            },
        };
        let project_opening = drive_existing_project_opening(
            ProjectOpeningLocation {
                projects_root: projects_root.clone(),
                layout: self.layout,
            },
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
            },
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                let shutdown = finish_terminal_progress(progress, shutdown);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                let shutdown = finish_terminal_progress(progress, shutdown);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(project) => {
                        drop(project);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                let shutdown = finish_terminal_progress(progress, shutdown);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let command = match command.resolve_profile(&profile_id) {
            Ok(command) => command,
            Err(ConfigurationLoadError::TranslationProfileNotFound { .. })
                if profile_source == ProjectLogValueSource::ProjectState =>
            {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::run_plan_resolution(
                        RunPlanResolutionError::SavedProfileUnavailable { profile_id },
                    ),
                    shutdown,
                );
            }
            Err(error) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::configuration_load(error),
                    shutdown,
                );
            }
        };
        self.panic_boundary
            .observe_selected_api_key_redactor(command.translation().client().api_key_redactor());
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: project_log_engine(self.layout),
            project: command.project_name().as_str(),
            command: ProjectLogCommand::Translate,
            performance: Arc::clone(&performance),
            selected_api_key_redactor: Some(command.translation().client().api_key_redactor()),
        });
        self.panic_boundary.observe_project_log(&project_log);
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            translate_phase_code,
        );
        let translate_plan = match TranslateRunPlan::new(profile_id.clone()) {
            Ok(plan) => plan,
            Err(error) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::invalid_run_plan(error),
                    shutdown,
                )
                .await;
            }
        };
        let resolved_plan = ResolvedRunPlan::translate_validated(
            profile_source,
            translate_plan.profile_identifier().clone(),
            command.terminology_path(),
            command.placeholder_rules_path(),
        );
        let replacement = ProjectRunPlanReplacement::Translate(translate_plan);
        project_log.handle().emit(ProjectLogEvent::RunPlanResolved {
            plan: resolved_plan,
        });
        let additional_pem_roots =
            match load_additional_pem_roots(&file_system, command.llm()).await {
                Ok(value) => value,
                Err(error) => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return observed_construction_failure(project_log, error, shutdown).await;
                }
            };
        let llm =
            match OpenAiCompatibleExecutor::new(command.llm().with_pem_roots(additional_pem_roots))
                .map_err(ProductionCommandError::http_client_build)
            {
                Ok(value) => value,
                Err(error) => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return observed_construction_failure(project_log, error, shutdown).await;
                }
            };
        let business_log =
            ProductionBusinessLog::for_translation(&project_log, progress_observer.clone());
        let (task_records, record_translation_tasks) =
            if command.record_translation_tasks() {
                if let Some(run_id) = project_log.run_id() {
                    match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
                        Ok(observation_file_system) => (
                            ConfiguredTranslationTaskRecordSink::Markdown(Box::new(
                                MarkdownTranslationTaskRecordSink::new(
                                    project_workspace
                                        .workspace_root()
                                        .join("task-records")
                                        .join(run_id),
                                    command.translation().client().api_key_redactor(),
                                    self.locale,
                                    cpu.clone(),
                                    observation_file_system,
                                    project_log.handle().clone(),
                                ),
                            )),
                            true,
                        ),
                        Err(error) => {
                            project_log.handle().record_task_record_diagnostic(
                                DiagnosticReport::new(StateEffect::Unchanged, error.diagnostic()),
                            );
                            (ConfiguredTranslationTaskRecordSink::disabled(), false)
                        }
                    }
                } else {
                    if let Some(report) = project_log.run_id_failure().cloned() {
                        project_log.handle().record_task_record_diagnostic(report);
                    }
                    (ConfiguredTranslationTaskRecordSink::disabled(), false)
                }
            } else {
                (ConfiguredTranslationTaskRecordSink::disabled(), false)
            };
        let builder = ProductionSelectedTranslationExecutionBuilder {
            configuration: command.translation(),
            file_system: file_system.clone(),
            cpu: cpu.clone(),
            sqlite: sqlite.clone(),
            llm: llm.clone(),
            log: business_log.clone(),
            task_records: task_records.clone(),
            record_translation_tasks,
            cancellation: cancellation.clone(),
        };
        let service = TranslateService::new(builder, cancellation.clone());
        let input = TranslateInput {
            terminology_path: command.terminology_path().map(Path::to_path_buf),
            placeholder_rules_path: command.placeholder_rules_path().map(Path::to_path_buf),
            retry_rejected: command.retry_rejected(),
        };
        progress_observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::Planning,
        ));
        let safe_stopping = progress_safe_stopping(self.locale);
        let translation_execution = async {
            drive_with_termination(
                service.execute(&opened_project, input),
                termination_signals,
                || {
                    cancellation.request();
                    cpu.cancel_waits();
                    file_system.cancel_waits();
                    sqlite.cancel_waits();
                    llm.cancel_waits();
                },
                || {
                    defer_terminal_progress_status(progress.safe_stopping(safe_stopping));
                    let (confirmed, total) = progress_observer.confirmed_amount();
                    project_log
                        .handle()
                        .emit(ProjectLogEvent::CancellationRequested { confirmed, total });
                },
            )
            .await
            .map(|result| result.map_err(map_translate_error))
        };
        let mut execution =
            catch_translate_execution_panic(self.panic_boundary.clone(), translation_execution)
                .await;
        business_log.emit_retry_summary();
        let no_model_work = matches!(
            &execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
                if output.summary.total_tasks == 0
        );
        if no_model_work {
            progress_observer.observe(ProgressSnapshot::determinate(
                TranslateProgressPhase::ConfirmedTasks,
                0,
                0,
            ));
        }
        finish_progress_business_state(&progress_observer, &execution);
        llm.shutdown().await;
        // 任务记录渲染仍使用本次命令的 CPU 根。必须在关闭根之前完成旁路记录；
        // 记录故障只写入日志健康状态，不改变翻译、取消或运行方案的终态。
        task_records.finish().await;
        let shutdown = roots.shutdown().await;
        // Translation 业务终态必须在 RunPlan 持久化前确定；后者失败不能把已经完成的
        // 翻译改写成 Failed，也不能丢失正常业务汇总。
        let terminal_diagnostic = business_log.emit_translation_finished(&execution);
        if let Some((_, diagnostic, _)) = terminal_diagnostic {
            progress_observer.stop_active(PhaseStopOutcome::Failed { diagnostic });
        }
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            defer_terminal_progress_status(progress.finalizing(progress_finalizing(self.locale)));
        }
        if business_completed(&execution) && shutdown.is_empty() {
            defer_terminal_progress_status(progress.finalizing(progress_saving_plan(self.locale)));
        }
        execution = finalize_run_plan(
            execution,
            &shutdown,
            RunPlanFinalizationInput {
                database_path,
                replacement,
                sqlite_configuration,
            },
            &project_log,
            termination_signals,
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
        let shutdown = finish_terminal_progress(progress, shutdown);
        let pending_project_log = pending_project_log_for_translation_execution(
            project_log,
            &execution,
            &shutdown,
            terminal_diagnostic,
        );
        ProductionCommandRunReport::from_completion_with_project_log(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Translate {
                        output,
                        profile_source,
                    })
                })
            }),
            shutdown,
            Some(pending_project_log),
        )
        .with_translation_summary(business_log.terminal_translation_summary())
    }
}
