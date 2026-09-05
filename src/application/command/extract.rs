//! RPG Maker Extract 的 owner 选择、规则加载与生产装配。

use super::error::{ProductionCommandError, SignalOutcomeSource, map_extract_error};
use super::lifecycle::{
    ProductionCommandRootGuard, ProductionCommandRunReport, ProjectOpeningLocation,
    drive_existing_project_opening, drive_project_lease, interrupted_non_cancellation_error,
    map_completion, observed_construction_failure,
};
use super::progress::{
    ProductionProgressObserver, business_completed, completed_output,
    defer_terminal_progress_status, extract_phase_code, extract_terminal_progress,
    finish_progress_business_state, finish_terminal_progress, pending_project_log_with_occurrence,
    progress_finalizing, progress_safe_stopping, progress_saving_plan, project_log_engine,
    record_failed_phase,
};
use super::run_plan::{RunPlanFinalizationInput, RunPlanResolutionError, finalize_run_plan};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::ConfiguredExtractCommand;
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::ReportedFailure;
use crate::diagnostic::{Diagnostic, DiagnosticReport, RpgMakerIssue, StateEffect};
use crate::execution::CooperativeCancellation;
use crate::project_lease::ProjectCommandLeaseService;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
    external_invalid_utf8_diagnostic_report,
};
use crate::rpg_maker::extract::SelectedRules;
use crate::rpg_maker::extract::builtin::{BuiltInExtractionService, MvDialogueDefinitionSelection};
use crate::rpg_maker::extract::document::{
    CommandScopedRpgMakerDocumentReader, RpgMakerProjectDocumentReadingService,
};
use crate::rpg_maker::extract::rules::{RulesExtractionService, RulesProgram};
use crate::rpg_maker::extract::service::ExtractService;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStore;
use crate::rpg_maker::project_database::{
    ExtractRulesCanonicalJson, ExtractRunPlan, ProjectRunPlanPersistenceService,
    ProjectRunPlanReadError, ProjectRunPlanReplacement, ProjectRunPlanRepository,
    ProjectWorkspaceLayout,
};
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticScope, ExtractOwnerSelection, ProjectLogCommand, ProjectLogEvent, ResolvedRunPlan,
    RunPlanValueSource as ProjectLogValueSource,
};
use crate::storage::file_system::{FileReader, ReadFileError};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_extract(
        self,
        command: ConfiguredExtractCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = extract_terminal_progress(self.locale);
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
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
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
        let explicit_selection =
            command.rpg_maker().builtin() || command.rpg_maker().rules().is_some();
        let (saved_extract, plan_source) = if explicit_selection {
            (None, ProjectLogValueSource::Explicit)
        } else {
            let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
            match repository.read(database_path.clone()).await {
                Ok(plans) => match plans.extract().cloned() {
                    Some(plan) => (Some(plan), ProjectLogValueSource::ProjectState),
                    None => {
                        let shutdown = roots.shutdown().await;
                        drop(project_lease_guard);
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::run_plan_resolution(
                                RunPlanResolutionError::NoReusableExtractPlan,
                            ),
                            shutdown,
                        );
                    }
                },
                Err(ProjectRunPlanReadError::DatabaseNotFound { .. }) => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        ProductionCommandError::run_plan_resolution(
                            RunPlanResolutionError::NoReusableExtractPlan,
                        ),
                        shutdown,
                    );
                }
                Err(error) => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        ProductionCommandError::project_run_plan_read(error),
                        shutdown,
                    );
                }
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
            command: ProjectLogCommand::Extract,
            performance: Arc::clone(&performance),
            selected_api_key_redactor: None,
        });
        self.panic_boundary.observe_project_log(&project_log);
        let progress_observer =
            ProductionProgressObserver::new(progress.observer(), &project_log, extract_phase_code);
        let builtin_enabled = saved_extract.as_ref().map_or_else(
            || command.rpg_maker().builtin(),
            ExtractRunPlan::builtin_enabled,
        );
        let mv_dialogue_selection = if self.layout == RpgMakerLayout::MV && builtin_enabled {
            match command.dialogue_rules_path() {
                Some(path) => match load_mv_dialogue_definition(&file_system, path).await {
                    Ok((definition, projector)) => Some(MvDialogueDefinitionSelection::Replace {
                        projector,
                        definition,
                    }),
                    Err(source) => {
                        let diagnostic = source.diagnostic_report();
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
                None => Some(MvDialogueDefinitionSelection::ReuseProjectDefinition),
            }
        } else {
            None
        };
        let rules_program = match (command.rpg_maker().rules(), saved_extract.as_ref()) {
            (Some(selected), _) => {
                match load_rules_program(&file_system, selected.rules_path()).await {
                    Ok(program) => Some(program),
                    Err(source) => {
                        let diagnostic = source.diagnostic_report();
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
                }
            }
            (None, Some(plan)) => match plan.rules_definition() {
                Some(definition) => match RulesProgram::from_canonical_json(
                    database_path.clone(),
                    definition.as_str(),
                ) {
                    Ok(program) => Some(program),
                    Err(error) => {
                        let diagnostic = error.diagnostic_report(&database_path);
                        let shutdown = roots.shutdown().await;
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ProjectState(Box::new(ReportedFailure::new(
                                diagnostic, error,
                            ))),
                            shutdown,
                        )
                        .await;
                    }
                },
                None => None,
            },
            (None, None) => None,
        };
        let replacement = {
            let rules_definition = rules_program
                .as_ref()
                .filter(|program| !program.is_empty())
                .map(|program| ExtractRulesCanonicalJson::new(program.canonical_json().to_owned()))
                .transpose();
            match rules_definition {
                Ok(rules_definition) => {
                    match ExtractRunPlan::new(builtin_enabled, rules_definition) {
                        Ok(plan) => Ok(ProjectRunPlanReplacement::Extract(Some(plan))),
                        Err(crate::rpg_maker::project_database::InvalidRunPlanValue::EmptyExtractOwners) => {
                            Ok(ProjectRunPlanReplacement::Extract(None))
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        };
        let replacement = match replacement {
            Ok(replacement) => replacement,
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
        let rules_enabled = rules_program
            .as_ref()
            .is_some_and(|program| !program.is_empty());
        let has_saved_plan = builtin_enabled || rules_enabled;
        let run_plan_warnings = rules_program
            .as_ref()
            .filter(|program| program.is_empty())
            .map(|program| {
                DiagnosticReport::new(
                    if has_saved_plan {
                        StateEffect::Applied
                    } else {
                        StateEffect::AppliedRunPlanNotSaved
                    },
                    Diagnostic::rpg_maker(RpgMakerIssue::rules_owner_disabled(
                        program.diagnostic_path(),
                    )),
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        let presented_owners = [
            builtin_enabled.then_some(String::from("Builtin")),
            rules_enabled.then_some(String::from("Rules")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        project_log.handle().emit(ProjectLogEvent::RunPlanResolved {
            plan: ResolvedRunPlan::rpg_maker_extract(
                plan_source,
                ExtractOwnerSelection::new(builtin_enabled, rules_enabled),
            ),
        });
        let document_config = command.rpg_maker().document();
        let document_reader =
            CommandScopedRpgMakerDocumentReader::new(RpgMakerProjectDocumentReadingService::new(
                file_system.clone(),
                file_system.clone(),
                cpu.clone(),
                document_config,
            ));
        let builtin = builtin_enabled.then({
            let reader = document_reader.clone();
            let sqlite = sqlite.clone();
            let cpu = cpu.clone();
            move || {
                let store = RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone());
                match mv_dialogue_selection {
                    Some(selection) => {
                        BuiltInExtractionService::for_mv(reader, store, cpu.clone(), selection)
                    }
                    None => BuiltInExtractionService::new(reader, store, cpu.clone()),
                }
            }
        });
        let selected_rules = rules_program.map({
            let reader = document_reader.clone();
            let sqlite = sqlite.clone();
            let cpu = cpu.clone();
            move |program| {
                let store = RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone());
                SelectedRules::new(
                    program,
                    RulesExtractionService::new(reader, store, cpu.clone()),
                )
            }
        });
        let service = ExtractService::new(builtin, selected_rules, cancellation.clone())
            .with_progress(progress_observer.clone());
        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_with_termination(
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
        .map(|result| result.map_err(map_extract_error));
        finish_progress_business_state(&progress_observer, &execution);
        let shutdown = roots.shutdown().await;
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
        if let Some(output) = completed_output(&execution) {
            for warning in &output.rules_warnings {
                project_log
                    .handle()
                    .record_diagnostic(DiagnosticScope::Extract, warning.diagnostic_report());
            }
            for warning in &run_plan_warnings {
                project_log
                    .handle()
                    .record_diagnostic(DiagnosticScope::Extract, warning.clone());
            }
        }
        drop(project_lease_guard);
        let shutdown = finish_terminal_progress(progress, shutdown);
        let terminal_diagnostic = record_failed_phase(
            &progress_observer,
            &project_log,
            &execution,
            &shutdown,
            DiagnosticScope::Run,
        );
        let pending_project_log = pending_project_log_with_occurrence(
            project_log,
            &execution,
            &shutdown,
            terminal_diagnostic,
        );
        ProductionCommandRunReport::from_completion_with_project_log(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Extract {
                        output,
                        plan_source,
                        owners: presented_owners,
                        run_plan_warnings,
                        has_saved_plan,
                    })
                })
            }),
            shutdown,
            Some(pending_project_log),
        )
    }
}
pub(super) async fn load_rules_program(
    file_system: &SystemFileSystem,
    requested_path: &Path,
) -> Result<RulesProgram, RulesProgramInputError> {
    let file = file_system
        .read_file(requested_path.to_path_buf())
        .await
        .map_err(|source| RulesProgramInputError::Read {
            path: requested_path.to_path_buf(),
            source,
        })?;
    let resolved_path = file.resolved_path().to_path_buf();
    RulesProgram::from_toml(resolved_path.clone(), file.into_bytes()).map_err(|source| {
        RulesProgramInputError::Invalid {
            path: resolved_path,
            source,
        }
    })
}

#[derive(Debug)]
pub(super) enum RulesProgramInputError {
    Read {
        path: PathBuf,
        source: ReadFileError<SystemFileSystemError>,
    },
    Invalid {
        path: PathBuf,
        source: crate::rpg_maker::extract::rules::RulesProgramError,
    },
}

impl fmt::Display for RulesProgramInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "读取 Extract Rules 失败 {}：{source}",
                    path.display()
                )
            }
            Self::Invalid { path, source } => {
                write!(
                    formatter,
                    "Extract Rules 定义无效 {}：{source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for RulesProgramInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Invalid { source, .. } => Some(source),
        }
    }
}

impl RulesProgramInputError {
    pub(super) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::Read { source, .. } => source.command_preparation_diagnostic_report(),
            Self::Invalid { path, source } => source.diagnostic_report(path),
        }
    }
}

pub(super) async fn load_mv_dialogue_definition(
    file_system: &SystemFileSystem,
    path: &Path,
) -> Result<(MvDialogueDefinition, MvDialogueProjector), MvDialogueDefinitionInputError> {
    let requested_path = path.to_path_buf();
    let file = file_system
        .read_file(requested_path.clone())
        .await
        .map_err(|source| MvDialogueDefinitionInputError::Read {
            path: requested_path.clone(),
            source,
        })?;
    let bytes = file.into_bytes();
    let source = std::str::from_utf8(&bytes).map_err(|source| {
        MvDialogueDefinitionInputError::InvalidUtf8 {
            path: requested_path.clone(),
            source,
        }
    })?;
    let definition = MvDialogueDefinition::parse_toml(source).map_err(|source| {
        MvDialogueDefinitionInputError::InvalidDefinition {
            path: requested_path.clone(),
            source,
        }
    })?;
    let projector = definition.compile().map_err(|source| {
        MvDialogueDefinitionInputError::InvalidDefinition {
            path: requested_path,
            source,
        }
    })?;
    Ok((definition, projector))
}

#[derive(Debug)]
pub(super) enum MvDialogueDefinitionInputError {
    Read {
        path: PathBuf,
        source: ReadFileError<SystemFileSystemError>,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: std::str::Utf8Error,
    },
    InvalidDefinition {
        path: PathBuf,
        source: MvDialogueDefinitionError,
    },
}

impl fmt::Display for MvDialogueDefinitionInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "无法读取 MV 对话定义 {}：{source}",
                    path.display()
                )
            }
            Self::InvalidUtf8 { path, source } => write!(
                formatter,
                "MV 对话定义 {} 不是有效 UTF-8：{source}",
                path.display(),
            ),
            Self::InvalidDefinition { path, source } => {
                write!(formatter, "MV 对话定义 {} 无效：{source}", path.display(),)
            }
        }
    }
}

impl Error for MvDialogueDefinitionInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidDefinition { source, .. } => Some(source),
        }
    }
}

impl MvDialogueDefinitionInputError {
    pub(super) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::Read { source, .. } => source.command_preparation_diagnostic_report(),
            Self::InvalidUtf8 { path, source } => {
                external_invalid_utf8_diagnostic_report(path, source)
            }
            Self::InvalidDefinition { path, source } => source.external_diagnostic_report(path),
        }
    }
}
