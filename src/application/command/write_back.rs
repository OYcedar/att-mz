//! RPG Maker WriteBack 的生产装配与发布终态衔接。

use super::business_log::ProductionBusinessLog;
use super::error::{ProductionCommandError, SignalOutcomeSource, map_write_back_error};
use super::lifecycle::{
    ProductionCommandRootGuard, ProductionCommandRunReport, ProjectOpeningLocation,
    drive_existing_project_opening, drive_project_lease, interrupted_non_cancellation_error,
    map_completion, observed_construction_failure,
};
use super::progress::{
    ProductionProgressObserver, defer_terminal_progress_status, finish_progress_business_state,
    finish_terminal_progress, pending_project_log_with_occurrence, progress_safe_stopping,
    project_log_engine, record_failed_phase, write_back_phase_code, write_back_terminal_progress,
};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::ConfiguredWriteBackCommand;
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::ReportedFailure;
use crate::execution::CooperativeCancellation;
use crate::project_lease::ProjectCommandLeaseService;
use crate::rpg_maker::extract::document::RpgMakerProjectDocumentReadingService;
use crate::rpg_maker::write_back::WriteBackService;
use crate::rpg_maker::write_back::asset_reader::{
    RpgMakerWriteBackAssetReadingService, RpgMakerWriteBackLayoutRulesInput,
};
use crate::rpg_maker::write_back::planner::RpgMakerWriteBackService;
use crate::rpg_maker::write_back::publisher::RpgMakerWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingService;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{DiagnosticScope, ProjectLogCommand, ProjectLogEvent};
use crate::storage::file_system::FileReader;
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_write_back(
        self,
        command: ConfiguredWriteBackCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = write_back_terminal_progress(self.locale);
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
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: project_log_engine(self.layout),
            project: command.project_name().as_str(),
            command: ProjectLogCommand::WriteBack,
            performance: Arc::clone(&performance),
            selected_api_key_redactor: None,
        });
        self.panic_boundary.observe_project_log(&project_log);
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            write_back_phase_code,
        );
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let layout_rules_input = match command.layout_rules_path() {
            Some(path) => match file_system.read_file(path.to_path_buf()).await {
                Ok(file) => Some(RpgMakerWriteBackLayoutRulesInput::new(
                    file.resolved_path().to_path_buf(),
                    file.into_bytes(),
                )),
                Err(source) => {
                    let diagnostic = source.command_preparation_diagnostic_report();
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(
                            ReportedFailure::new(diagnostic, source),
                        )),
                        shutdown,
                    )
                    .await;
                }
            },
            None => None,
        };
        let asset_reader = RpgMakerWriteBackAssetReadingService::new(sqlite.clone(), cpu.clone())
            .with_layout_rules_input(layout_rules_input);
        let document_reader = RpgMakerProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            command.rpg_maker().document(),
        );
        let rewriter = RpgMakerWriteBackDocumentRewritingService::new(document_reader, cpu.clone())
            .with_progress(progress_observer.clone());
        let write_back = RpgMakerWriteBackService::new(
            asset_reader,
            rewriter,
            cpu.clone(),
            cancellation.clone(),
        )
        .with_text_options(
            command.rpg_maker().text().repair_punctuation(),
            command
                .rpg_maker()
                .text()
                .complete_continuation_whitespace(),
        )
        .with_progress(progress_observer.clone());
        let publisher = RpgMakerWriteBackPublishingService::new(
            file_system.clone(),
            directory_publisher.clone(),
        );
        let business_log = ProductionBusinessLog::from_active(&project_log);
        let service = WriteBackService::new(
            write_back,
            publisher,
            business_log.clone(),
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let safe_stopping = progress_safe_stopping(self.locale);
        let execution = drive_with_termination(
            service.execute(&opened_project),
            termination_signals,
            || {
                cancellation.request();
                cpu.cancel_waits();
                file_system.cancel_waits();
                sqlite.cancel_waits();
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
        .map(|result| result.map_err(map_write_back_error));
        finish_progress_business_state(&progress_observer, &execution);
        let shutdown = roots.shutdown().await;
        drop(project_lease_guard);
        let shutdown = finish_terminal_progress(progress, shutdown);
        let diagnostic_scope = if business_log.has_pending_publication_failure() {
            DiagnosticScope::Publication
        } else {
            DiagnosticScope::Run
        };
        let terminal_diagnostic = record_failed_phase(
            &progress_observer,
            &project_log,
            &execution,
            &shutdown,
            diagnostic_scope,
        );
        if let Some((_, diagnostic)) = terminal_diagnostic {
            business_log.emit_publication_failure(diagnostic);
        }
        let pending_project_log = pending_project_log_with_occurrence(
            project_log,
            &execution,
            &shutdown,
            terminal_diagnostic,
        );
        ProductionCommandRunReport::from_completion_with_project_log(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::WriteBack {
                        output,
                    })
                })
            }),
            shutdown,
            Some(pending_project_log),
        )
    }
}
