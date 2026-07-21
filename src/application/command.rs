//! 生产命令装配与最终结果呈现。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::application::config::{
    ConfigurationLoadError, ConfiguredExtractCommand, ConfiguredInitCommand,
    ConfiguredRpgMakerCommand, ConfiguredTranslateCommand, ConfiguredWriteBackCommand,
    SelectedLuaConfiguration, TranslateConfiguration,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::i18n::{
    UiLocale, UiLocalizer, UiMessage, project_log_phase_label, project_log_task_outcome_label,
    project_log_value_source_label,
};
use crate::progress::{
    ProgressAmount, ProgressMode, ProgressObserver, ProgressSnapshot, TerminalProgress,
    TerminalProgressObserver,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::SelectedLua;
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
};
use crate::rpg_maker::extract::builtin::{BuiltInExtractionService, MvDialogueDefinitionSelection};
use crate::rpg_maker::extract::document::RpgMakerProjectDocumentReadingService;
use crate::rpg_maker::extract::lua::LuaExtractionService;
use crate::rpg_maker::extract::rules::{RulesExtractionService, RulesProgram};
use crate::rpg_maker::extract::service::ExtractService;
use crate::rpg_maker::extract::service::ExtractServiceError;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStore;
use crate::rpg_maker::extract::{ExtractInput, ExtractOutput, ExtractProgressPhase, SelectedRules};
use crate::rpg_maker::init::{
    InitInput, InitOutcome, InitOutput, InitProgressPhase, InitService, InitServiceError,
    InitStaleOwner, ProjectWorkspaceConvergenceFailureImpact, ProjectWorkspaceConvergenceService,
};
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingService;
use crate::rpg_maker::lua::lua54::TrustedLua54Runtime;
use crate::rpg_maker::lua::runtime::OwnedLuaProgram;
use crate::rpg_maker::project::ExistingProjectOpeningService;
use crate::rpg_maker::project_database::{
    ExtractRulesCanonicalJson, ExtractRunPlan, FinalProjectRunPlanPersistenceService, InitRunPlan,
    LuaProgramSnapshot, ProjectDatabaseCreationService, ProjectDatabaseRecordReadingService,
    ProjectDatabaseStateReconciliationService, ProjectRunPlanFinalizer,
    ProjectRunPlanPersistenceService, ProjectRunPlanReadError, ProjectRunPlanReplaceError,
    ProjectRunPlanReplacement, ProjectRunPlanRepository, ProjectWorkspaceLayout, TranslateRunPlan,
    WriteBackRunPlan,
};
use crate::rpg_maker::project_lease::{
    AlreadyHeldProjectCommandLeaseProvider, ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::rpg_maker::translate::TranslateInput;
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::translate::asset_reader::RpgMakerStandardTranslationAssetReadingService;
use crate::rpg_maker::translate::executor::{
    AsyncDelay, RpgMakerStandardTranslationTaskExecutionService,
    TranslationTaskResponseProcessingService,
};
use crate::rpg_maker::translate::lua::LuaTranslationService;
use crate::rpg_maker::translate::placeholder::Pcre2PlaceholderService;
use crate::rpg_maker::translate::planner::RpgMakerStandardTranslationTaskPlanningService;
use crate::rpg_maker::translate::planning_resource::TranslationPlanningResourceReadingService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
};
use crate::rpg_maker::translate::result_store::RpgMakerStandardTranslationResultStorageService;
use crate::rpg_maker::translate::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder, TranslateService,
    TranslateServiceError,
};
use crate::rpg_maker::translate::standard::{
    StandardTranslationFailureImpact, StandardTranslationLog, StandardTranslationLogEvent,
    StandardTranslationLogTaskOutcome, StandardTranslationService,
};
use crate::rpg_maker::write_back::asset_reader::RpgMakerStandardWriteBackAssetReadingService;
use crate::rpg_maker::write_back::lua::LuaWriteBackService;
use crate::rpg_maker::write_back::publisher::StandardWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingService;
use crate::rpg_maker::write_back::standard::{
    ConservativeRpgMakerWriteBackTextLayouter, StandardWriteBackService,
};
use crate::rpg_maker::write_back::{
    WriteBackFailureImpact, WriteBackInput, WriteBackLog, WriteBackLogEvent,
    WriteBackLogPublicationOutcome, WriteBackOutput, WriteBackProgressPhase, WriteBackService,
    WriteBackServiceError,
};
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::llm::{OpenAiChatCompletionClient, OpenAiChatCompletionExecutor};
use crate::runtime::project_log::{
    ProjectLog, ProjectLogAmount, ProjectLogCode, ProjectLogContext, ProjectLogEvent,
    ProjectLogLevel, ProjectLogPayload, ProjectLogRunOutcome, ProjectLogRuntime,
    ProjectLogValueSource, ProjectLogWarning, ProjectLogger, start_project_log,
};
use crate::runtime::run_id::generate_run_id;
use crate::runtime::sqlite::{RusqliteFinalTransactionExecutor, RusqliteStorage};
use crate::storage::file_system::{ExistingDirectoryResolver, FileReader, ReadFileError};

type BoxedError = Box<dyn Error + Send + Sync + 'static>;
const RPG_MAKER_PROMPT_DIRECTORY_NAME: &str = "rpg_maker";
const MAX_MV_DIALOGUE_DEFINITION_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
struct TokioAsyncDelay;

/// Translate 终端只解释本纵向切片拥有的阶段；任务计数来自已提交终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslateProgressPhase {
    Planning,
    ConfirmedTasks,
    NoWork,
}

impl AsyncDelay for TokioAsyncDelay {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod async_delay_tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn waits_for_requested_duration() {
        let started = Instant::now();
        TokioAsyncDelay.wait(Duration::from_millis(5)).await;
        assert!(started.elapsed() >= Duration::from_millis(5));
    }
}

#[derive(Clone)]
struct ProductionLuaSelection {
    program: OwnedLuaProgram,
    runtime: TrustedLua54Runtime,
}

/// 一个 RPG Maker 命令成功完成后的类型化结果。
pub(crate) enum RpgMakerCommandOutput {
    Init {
        output: InitOutput,
        plan_source: ProjectLogValueSource,
        reused_path: Option<PathBuf>,
    },
    Extract {
        output: ExtractOutput,
        plan_source: ProjectLogValueSource,
        owners: Vec<String>,
        disabled_owners: Vec<&'static str>,
        has_saved_plan: bool,
    },
    Translate {
        output: TranslateOutput,
        profile_source: ProjectLogValueSource,
        lua_source: ProjectLogValueSource,
        lua_cleared: bool,
    },
    WriteBack {
        output: WriteBackOutput,
        plan_source: ProjectLogValueSource,
        lua_cleared: bool,
    },
}

fn init_terminal_progress(
    mode: ProgressMode,
    locale: UiLocale,
) -> TerminalProgress<InitProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let checking = localizer.format(UiMessage::ProgressInitCheckProject);
    let scanning = localizer.format(UiMessage::ProgressInitScanSource);
    let preparing = localizer.format(UiMessage::ProgressInitBuildCandidate);
    let updating = localizer.format(UiMessage::ProgressInitConvergeDatabase);
    let publishing = localizer.format(UiMessage::ProgressInitPublish);
    TerminalProgress::stderr(mode, move |phase| match phase {
        InitProgressPhase::CheckingProject => checking.clone(),
        InitProgressPhase::ScanningSource => scanning.clone(),
        InitProgressPhase::PreparingCandidate => preparing.clone(),
        InitProgressPhase::UpdatingDatabase => updating.clone(),
        InitProgressPhase::Publishing => publishing.clone(),
    })
}

fn extract_terminal_progress(
    mode: ProgressMode,
    locale: UiLocale,
) -> TerminalProgress<ExtractProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let builtin = localizer.format(UiMessage::ProgressExtractOwner { owner: "Builtin" });
    let builtin_documents = localizer.format(UiMessage::ProgressExtractDocuments);
    let builtin_work_units = localizer.format(UiMessage::ProgressExtractBuiltin);
    let builtin_commit = localizer.format(UiMessage::ProgressExtractCommit);
    let rules = localizer.format(UiMessage::ProgressExtractOwner { owner: "Rules" });
    let rules_documents = localizer.format(UiMessage::ProgressExtractDocuments);
    let rules_matches = localizer.format(UiMessage::ProgressExtractRules);
    let rules_commit = localizer.format(UiMessage::ProgressExtractCommit);
    let lua = localizer.format(UiMessage::ProgressExtractOwner { owner: "Lua" });
    let lua_execution = localizer.format(UiMessage::ProgressExtractLua);
    let lua_commit = localizer.format(UiMessage::ProgressExtractCommit);
    TerminalProgress::stderr(mode, move |phase| match phase {
        ExtractProgressPhase::Builtin => builtin.clone(),
        ExtractProgressPhase::BuiltinDocuments => builtin_documents.clone(),
        ExtractProgressPhase::BuiltinWorkUnits => builtin_work_units.clone(),
        ExtractProgressPhase::BuiltinCommit => builtin_commit.clone(),
        ExtractProgressPhase::Rules => rules.clone(),
        ExtractProgressPhase::RulesDocuments => rules_documents.clone(),
        ExtractProgressPhase::RulesMatches => rules_matches.clone(),
        ExtractProgressPhase::RulesCommit => rules_commit.clone(),
        ExtractProgressPhase::Lua => lua.clone(),
        ExtractProgressPhase::LuaExecution => lua_execution.clone(),
        ExtractProgressPhase::LuaCommit => lua_commit.clone(),
    })
}

fn translate_terminal_progress(
    mode: ProgressMode,
    locale: UiLocale,
) -> TerminalProgress<TranslateProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let planning = localizer.format(UiMessage::ProgressTranslatePlanning);
    let confirmed = localizer.format(UiMessage::ProgressTranslateConfirmed);
    let no_work = localizer.format(UiMessage::ProgressTranslateNoWork);
    TerminalProgress::stderr(mode, move |phase| match phase {
        TranslateProgressPhase::Planning => planning.clone(),
        TranslateProgressPhase::ConfirmedTasks => confirmed.clone(),
        TranslateProgressPhase::NoWork => no_work.clone(),
    })
}

fn write_back_terminal_progress(
    mode: ProgressMode,
    locale: UiLocale,
) -> TerminalProgress<WriteBackProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let reading = localizer.format(UiMessage::ProgressWriteBackReadAssets);
    let planning = localizer.format(UiMessage::ProgressWriteBackPlanning);
    let rewriting = localizer.format(UiMessage::ProgressWriteBackDocuments);
    let preparing = planning.clone();
    let lua = localizer.format(UiMessage::ProgressWriteBackLua);
    let validating = localizer.format(UiMessage::ProgressWriteBackValidateCandidate);
    let publishing = localizer.format(UiMessage::ProgressWriteBackPublish);
    TerminalProgress::stderr(mode, move |phase| match phase {
        WriteBackProgressPhase::ReadingAssets => reading.clone(),
        WriteBackProgressPhase::PlanningStandard => planning.clone(),
        WriteBackProgressPhase::RewritingDocuments => rewriting.clone(),
        WriteBackProgressPhase::PreparingCandidate => preparing.clone(),
        WriteBackProgressPhase::RunningLua => lua.clone(),
        WriteBackProgressPhase::ValidatingCandidate => validating.clone(),
        WriteBackProgressPhase::Publishing => publishing.clone(),
    })
}

fn progress_safe_stopping(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressSafeStopping)
}

fn progress_finalizing(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressFinalizing)
}

fn progress_saving_plan(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressSaveRunPlan)
}

/// 按本次命令只构造实际需要的 RPG Maker 生产纵向切片。
pub(crate) struct ProductionRpgMakerCommandRunner {
    layout: RpgMakerLayout,
    locale: UiLocale,
    progress_mode: ProgressMode,
}

impl ProductionRpgMakerCommandRunner {
    pub(crate) const fn new(
        layout: RpgMakerLayout,
        locale: UiLocale,
        progress_mode: ProgressMode,
    ) -> Self {
        Self {
            layout,
            locale,
            progress_mode,
        }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredRpgMakerCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        match command {
            ConfiguredRpgMakerCommand::Init(command) => {
                self.run_init(command, termination_signals).await
            }
            ConfiguredRpgMakerCommand::Extract(command) => {
                self.run_extract(command, termination_signals).await
            }
            ConfiguredRpgMakerCommand::Translate(command) => {
                self.run_translate(*command, termination_signals).await
            }
            ConfiguredRpgMakerCommand::WriteBack(command) => {
                self.run_write_back(command, termination_signals).await
            }
        }
    }

    async fn run_init(
        self,
        command: ConfiguredInitCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let progress = init_terminal_progress(self.progress_mode, self.locale);
        let project_log = start_command_log(
            command.common(),
            self.locale,
            self.layout,
            command.arguments.project.name.as_str(),
            "init",
            None,
        );
        let progress_observer =
            ProductionProgressObserver::new(progress.observer(), &project_log, init_phase_code);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let sqlite = match RusqliteStorage::start(sqlite_configuration.clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(sqlite) => sqlite,
            Err(error) => {
                return observed_construction_failure(
                    project_log,
                    error,
                    ShutdownFailures::default(),
                )
                .await;
            }
        };
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(file_system) => file_system,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let arguments = &command.arguments;
        let project_name = arguments.project.name.clone();
        let project_workspace =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name);
        let database_path = project_workspace.database_path().to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine(),
            file_system.clone(),
        );
        let project_lease_guard = match lease_provider.acquire(&project_name).await {
            Ok(lease) => lease,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ProjectUnavailable(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        let (game_root, plan_source) = match arguments.path.clone() {
            Some(path) => (path, ProjectLogValueSource::Explicit),
            None => {
                let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
                match repository.read(database_path.clone()).await {
                    Ok(plans) => match plans.init() {
                        Some(plan) => (
                            plan.source_path().to_path_buf(),
                            ProjectLogValueSource::ProjectState,
                        ),
                        None => {
                            let mut shutdown = ShutdownFailures::default();
                            if let Err(error) = sqlite.shutdown().await {
                                shutdown.push("SQLite", error);
                            }
                            if let Err(error) = file_system.shutdown().await {
                                shutdown.push("FileSystem", error);
                            }
                            drop(project_lease_guard);
                            return observed_construction_failure(
                                project_log,
                                ProductionCommandError::ConfigurationOrInput(Box::new(
                                    RunPlanResolutionError::InitPathRequired,
                                )),
                                shutdown,
                            )
                            .await;
                        }
                    },
                    Err(ProjectRunPlanReadError::DatabaseNotFound { .. }) => {
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", error);
                        }
                        if let Err(error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ConfigurationOrInput(Box::new(
                                RunPlanResolutionError::InitPathRequired,
                            )),
                            shutdown,
                        )
                        .await;
                    }
                    Err(error) => {
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(shutdown_error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", shutdown_error);
                        }
                        if let Err(shutdown_error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", shutdown_error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ProjectState(Box::new(error)),
                            shutdown,
                        )
                        .await;
                    }
                }
            }
        };
        let resolved_game_root = match file_system.resolve_existing_directory(game_root).await {
            Ok(path) => path,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(shutdown_error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", shutdown_error);
                }
                if let Err(shutdown_error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", shutdown_error);
                }
                drop(project_lease_guard);
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ConfigurationOrInput(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            localized_plan_resolution(&project_log.localizer, "init", plan_source),
            ProjectLogPayload::RunPlan {
                source: plan_source,
                lua_source: None,
                selections: vec![resolved_game_root.to_string_lossy().into_owned()],
                lua_enabled: None,
            },
        ));
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let database = ProjectDatabaseCreationService::new(sqlite.clone());
        let database_reconciler =
            ProjectDatabaseStateReconciliationService::new(sqlite.clone(), sqlite.clone());
        let workspace = ProjectWorkspaceConvergenceService::new(
            projects_root.clone(),
            self.layout,
            database,
            sqlite.clone(),
            database_reconciler,
            file_system.clone(),
            directory_publisher,
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let service = InitService::new(
            workspace,
            AlreadyHeldProjectCommandLeaseProvider,
            cancellation.clone(),
        );
        let input = InitInput {
            name: project_name,
            game_root: resolved_game_root.clone(),
            source_language: arguments.source_language.clone(),
            target_language: arguments.target_language.clone(),
            dialogue_max_fullwidth_chars: arguments.dialogue_max_fullwidth_chars,
            scrolling_text_max_fullwidth_chars: arguments.scrolling_text_max_fullwidth_chars,
            help_description_max_fullwidth_chars: arguments.help_description_max_fullwidth_chars,
        };

        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_command(
            service.execute(input),
            &cancellation,
            termination_signals,
            || {
                progress.safe_stopping(safe_stopping);
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await
        .map(|result| {
            result.map_err(|error| map_init_error(error, |workspace| workspace.failure_impact()))
        });
        if matches!(&execution, DrivenCommand::Interrupted(_)) {
            let (confirmed, total) = progress_observer.confirmed_amount();
            project_log.emit_cancellation(ProjectLogCode::SafeStopFinished, confirmed, total);
        }
        progress_observer.finish();
        let mut shutdown = ShutdownFailures::default();
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        let reused_path = (plan_source == ProjectLogValueSource::ProjectState)
            .then(|| resolved_game_root.clone());
        let replacement = InitRunPlan::new(resolved_game_root)
            .map(ProjectRunPlanReplacement::Init)
            .map_err(ProductionCommandError::construct);
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            progress.finalizing(progress_finalizing(self.locale));
        }
        let plan_result = match replacement {
            Ok(replacement) => {
                if business_completed(&execution) && shutdown.is_empty() {
                    progress.finalizing(progress_saving_plan(self.locale));
                }
                finalize_run_plan(
                    &execution,
                    &shutdown,
                    database_path,
                    replacement,
                    sqlite_configuration,
                    &project_log,
                )
                .await
            }
            Err(error) => Err(error),
        };
        execution = replace_success_with_plan_error(execution, plan_result);
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let log_warning = finish_project_log(project_log, log_outcome);
        ProductionCommandRunReport::from_completion_with_log_warning(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Init {
                        output,
                        plan_source,
                        reused_path,
                    })
                })
            }),
            shutdown,
            log_warning,
        )
    }

    async fn run_extract(
        self,
        command: ConfiguredExtractCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let progress = extract_terminal_progress(self.progress_mode, self.locale);
        let project_log = start_command_log(
            command.common(),
            self.locale,
            self.layout,
            command.project_name().as_str(),
            "extract",
            None,
        );
        let progress_observer =
            ProductionProgressObserver::new(progress.observer(), &project_log, extract_phase_code);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let sqlite = match RusqliteStorage::start(sqlite_configuration.clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return observed_construction_failure(
                    project_log,
                    error,
                    ShutdownFailures::default(),
                )
                .await;
            }
        };
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let project_name = command.project_name().clone();
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine(),
            file_system.clone(),
        );
        let project_lease_guard = match lease_provider.acquire(&project_name).await {
            Ok(lease) => lease,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ProjectUnavailable(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        let explicit_selection = command.rpg_maker().builtin()
            || command.rpg_maker().rules().is_some()
            || command.lua().is_some();
        let (saved_extract, plan_source) = if explicit_selection {
            (None, ProjectLogValueSource::Explicit)
        } else {
            let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
            match repository.read(database_path.clone()).await {
                Ok(plans) => match plans.extract().cloned() {
                    Some(plan) => (Some(plan), ProjectLogValueSource::ProjectState),
                    None => {
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", error);
                        }
                        if let Err(error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ConfigurationOrInput(Box::new(
                                RunPlanResolutionError::NoReusableExtractPlan,
                            )),
                            shutdown,
                        )
                        .await;
                    }
                },
                Err(ProjectRunPlanReadError::DatabaseNotFound { .. }) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", error);
                    }
                    if let Err(error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(
                            RunPlanResolutionError::NoReusableExtractPlan,
                        )),
                        shutdown,
                    )
                    .await;
                }
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(shutdown_error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", shutdown_error);
                    }
                    if let Err(shutdown_error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", shutdown_error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ProjectState(Box::new(error)),
                        shutdown,
                    )
                    .await;
                }
            }
        };
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
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", error);
                        }
                        if let Err(error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ConfigurationOrInput(Box::new(source)),
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
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", error);
                        }
                        if let Err(error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ConfigurationOrInput(Box::new(source)),
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
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(shutdown_error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", shutdown_error);
                        }
                        if let Err(shutdown_error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", shutdown_error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ProjectState(Box::new(error)),
                            shutdown,
                        )
                        .await;
                    }
                },
                None => None,
            },
            (None, None) => None,
        };
        let lua = match (command.lua(), saved_extract.as_ref()) {
            (Some(selected), _) => match load_lua_selection(&file_system, selected).await {
                Ok(program) => Some(program),
                Err(source) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", error);
                    }
                    if let Err(error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(source)),
                        shutdown,
                    )
                    .await;
                }
            },
            (None, Some(plan)) => match plan.lua_program() {
                Some(snapshot) => match command.resolve_lua_runtime() {
                    Ok(runtime) => Some(ProductionLuaSelection {
                        program: OwnedLuaProgram::new(
                            snapshot.resolved_path().to_path_buf(),
                            snapshot.source().to_vec(),
                        ),
                        runtime: TrustedLua54Runtime::new(
                            runtime,
                            tokio::runtime::Handle::current(),
                        ),
                    }),
                    Err(error) => {
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(shutdown_error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", shutdown_error);
                        }
                        if let Err(shutdown_error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", shutdown_error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(
                            project_log,
                            ProductionCommandError::ConfigurationOrInput(Box::new(error)),
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
            let lua_program = lua
                .as_ref()
                .filter(|selection| !selection.program.source().is_empty())
                .map(|selection| {
                    LuaProgramSnapshot::new(
                        selection.program.main_script_path().to_path_buf(),
                        selection.program.source().to_vec(),
                    )
                })
                .transpose();
            match (rules_definition, lua_program) {
                (Ok(rules_definition), Ok(lua_program)) => {
                    match ExtractRunPlan::new(builtin_enabled, rules_definition, lua_program) {
                        Ok(plan) => Ok(ProjectRunPlanReplacement::Extract(Some(plan))),
                        Err(crate::rpg_maker::project_database::InvalidRunPlanValue::EmptyExtractOwners) => {
                            Ok(ProjectRunPlanReplacement::Extract(None))
                        }
                        Err(error) => Err(error),
                    }
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        };
        let replacement = match replacement {
            Ok(replacement) => replacement,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Some(selected) = lua.as_ref()
                    && let Err(source) = selected.runtime.shutdown().await
                {
                    shutdown.push("Lua", source);
                }
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::Internal(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        let mut selections = Vec::new();
        if builtin_enabled {
            selections.push(String::from("Builtin"));
        }
        if rules_program
            .as_ref()
            .is_some_and(|program| !program.is_empty())
        {
            selections.push(String::from("Rules"));
        }
        if lua
            .as_ref()
            .is_some_and(|selection| !selection.program.source().is_empty())
        {
            selections.push(String::from("Lua"));
        }
        let disabled_owners = [
            command
                .rpg_maker()
                .rules()
                .zip(rules_program.as_ref())
                .filter(|(_, program)| program.is_empty())
                .map(|_| "Rules"),
            command
                .lua()
                .zip(lua.as_ref())
                .filter(|(_, selection)| selection.program.source().is_empty())
                .map(|_| "Lua"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let has_saved_plan = !selections.is_empty();
        let presented_owners = selections.clone();
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            localized_plan_resolution(&project_log.localizer, "extract", plan_source),
            ProjectLogPayload::RunPlan {
                source: plan_source,
                lua_source: None,
                selections,
                lua_enabled: Some(
                    lua.as_ref()
                        .is_some_and(|selection| !selection.program.source().is_empty()),
                ),
            },
        ));
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let document_config = command.rpg_maker().document();
        let store_config = command.rpg_maker().extract_store();
        let builtin = builtin_enabled.then(|| {
            let reader = RpgMakerProjectDocumentReadingService::new(
                file_system.clone(),
                file_system.clone(),
                cpu.clone(),
                document_config,
            );
            let store =
                RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
            match mv_dialogue_selection {
                Some(selection) => {
                    BuiltInExtractionService::for_mv(reader, store, cpu.clone(), selection)
                }
                None => BuiltInExtractionService::new(reader, store, cpu.clone()),
            }
        });
        let selected_rules = rules_program.map(|program| {
            let reader = RpgMakerProjectDocumentReadingService::new(
                file_system.clone(),
                file_system.clone(),
                cpu.clone(),
                document_config,
            );
            let store =
                RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
            SelectedRules::new(
                program,
                RulesExtractionService::new(reader, store, cpu.clone()),
            )
        });
        let selected_lua = lua.as_ref().map(|selected| {
                let host = TrustedLuaExecutionHostingService::<_, OpenAiChatCompletionExecutor, _, _>::without_llm(
                    file_system.clone(), selected.runtime.clone(), sqlite.clone(),
                );
                let store = RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
                SelectedLua::new(
                    selected.program.clone(),
                    LuaExtractionService::new(host, store),
                )
        });
        let service = ExtractService::new(
            opener,
            builtin,
            selected_rules,
            selected_lua,
            AlreadyHeldProjectCommandLeaseProvider,
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let input = ExtractInput {
            name: command.project_name().clone(),
        };
        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_command(
            service.execute(input),
            &cancellation,
            termination_signals,
            || {
                progress.safe_stopping(safe_stopping);
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await
        .map(|result| result.map_err(map_extract_error));
        if matches!(&execution, DrivenCommand::Interrupted(_)) {
            let (confirmed, total) = progress_observer.confirmed_amount();
            project_log.emit_cancellation(ProjectLogCode::SafeStopFinished, confirmed, total);
        }
        progress_observer.finish();
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            progress.finalizing(progress_finalizing(self.locale));
        }
        if business_completed(&execution) && shutdown.is_empty() {
            progress.finalizing(progress_saving_plan(self.locale));
        }
        let plan_result = finalize_run_plan(
            &execution,
            &shutdown,
            database_path,
            replacement,
            sqlite_configuration,
            &project_log,
        )
        .await;
        execution = replace_success_with_plan_error(execution, plan_result);
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let log_warning = finish_project_log(project_log, log_outcome);
        ProductionCommandRunReport::from_completion_with_log_warning(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Extract {
                        output,
                        plan_source,
                        owners: presented_owners,
                        disabled_owners,
                        has_saved_plan,
                    })
                })
            }),
            shutdown,
            log_warning,
        )
    }

    async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let progress = translate_terminal_progress(self.progress_mode, self.locale);
        let explicit_profile = command.resolved_profile_id().map(str::to_owned);
        let mut project_log = start_command_log(
            command.common(),
            self.locale,
            self.layout,
            command.project_name().as_str(),
            "translate",
            explicit_profile.as_deref(),
        );
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return observed_construction_failure(
                    project_log,
                    error,
                    ShutdownFailures::default(),
                )
                .await;
            }
        };
        let sqlite = match RusqliteStorage::start(sqlite_configuration.clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let project_name = command.project_name().clone();
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine(),
            file_system.clone(),
        );
        let project_lease_guard = match lease_provider.acquire(&project_name).await {
            Ok(lease) => lease,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ProjectUnavailable(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
        let plans = match repository.read(database_path.clone()).await {
            Ok(plans) => plans,
            Err(error) => {
                let error = match error {
                    error @ ProjectRunPlanReadError::DatabaseNotFound { .. } => {
                        ProductionCommandError::ProjectUnavailable(Box::new(error))
                    }
                    error => ProductionCommandError::ProjectState(Box::new(error)),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(shutdown_error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", shutdown_error);
                }
                if let Err(shutdown_error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", shutdown_error);
                }
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
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
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", error);
                    }
                    if let Err(error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(
                            RunPlanResolutionError::ProfileRequired,
                        )),
                        shutdown,
                    )
                    .await;
                }
            },
        };
        let command = match command.resolve_profile(&profile_id) {
            Ok(command) => command,
            Err(ConfigurationLoadError::TranslationProfileNotFound { .. })
                if profile_source == ProjectLogValueSource::ProjectState =>
            {
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                drop(project_lease_guard);
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ConfigurationOrInput(Box::new(
                        RunPlanResolutionError::SavedProfileUnavailable { profile_id },
                    )),
                    shutdown,
                )
                .await;
            }
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(shutdown_error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", shutdown_error);
                }
                if let Err(shutdown_error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", shutdown_error);
                }
                drop(project_lease_guard);
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ConfigurationOrInput(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        project_log.set_profile(&profile_id);
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            translate_phase_code,
        );
        let explicit_lua_requested = command.lua().is_some();
        let (lua, lua_source) = match command.lua() {
            Some(selected) => match load_lua_selection(&file_system, selected).await {
                Ok(program) => (
                    (!program.program.source().is_empty()).then_some(program),
                    ProjectLogValueSource::Explicit,
                ),
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(shutdown_error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", shutdown_error);
                    }
                    if let Err(shutdown_error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", shutdown_error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(error)),
                        shutdown,
                    )
                    .await;
                }
            },
            None => {
                let lua_source = if saved_translate.is_some() {
                    ProjectLogValueSource::ProjectState
                } else {
                    ProjectLogValueSource::ProductDefault
                };
                let lua = match saved_translate
                    .as_ref()
                    .and_then(TranslateRunPlan::lua_program)
                {
                    Some(snapshot) => match command.resolve_lua_runtime() {
                        Ok(runtime) => Some(ProductionLuaSelection {
                            program: OwnedLuaProgram::new(
                                snapshot.resolved_path().to_path_buf(),
                                snapshot.source().to_vec(),
                            ),
                            runtime: TrustedLua54Runtime::new(
                                runtime,
                                tokio::runtime::Handle::current(),
                            ),
                        }),
                        Err(error) => {
                            let mut shutdown = ShutdownFailures::default();
                            if let Err(shutdown_error) = sqlite.shutdown().await {
                                shutdown.push("SQLite", shutdown_error);
                            }
                            if let Err(shutdown_error) = file_system.shutdown().await {
                                shutdown.push("FileSystem", shutdown_error);
                            }
                            drop(project_lease_guard);
                            return observed_construction_failure(
                                project_log,
                                ProductionCommandError::ConfigurationOrInput(Box::new(error)),
                                shutdown,
                            )
                            .await;
                        }
                    },
                    None => None,
                };
                (lua, lua_source)
            }
        };
        let lua_cleared = explicit_lua_requested && lua.is_none();
        let lua_snapshot = lua
            .as_ref()
            .map(|selection| {
                LuaProgramSnapshot::new(
                    selection.program.main_script_path().to_path_buf(),
                    selection.program.source().to_vec(),
                )
            })
            .transpose();
        let replacement =
            match lua_snapshot.and_then(|lua| TranslateRunPlan::new(profile_id.clone(), lua)) {
                Ok(plan) => ProjectRunPlanReplacement::Translate(plan),
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Some(selected) = lua.as_ref()
                        && let Err(source) = selected.runtime.shutdown().await
                    {
                        shutdown.push("Lua", source);
                    }
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::Internal(Box::new(error)),
                        shutdown,
                    )
                    .await;
                }
            };
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            localized_translate_plan_resolution(&project_log.localizer, profile_source, lua_source),
            ProjectLogPayload::RunPlan {
                source: profile_source,
                lua_source: Some(lua_source),
                selections: vec![profile_id.clone()],
                lua_enabled: Some(lua.is_some()),
            },
        ));
        let additional_pem_roots =
            match load_additional_pem_roots(&file_system, command.llm()).await {
                Ok(value) => value,
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Some(selected) = lua.as_ref()
                        && let Err(source) = selected.runtime.shutdown().await
                    {
                        shutdown.push("Lua", source);
                    }
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(project_log, error, shutdown).await;
                }
            };
        let llm = match OpenAiChatCompletionExecutor::new(
            command.llm().with_pem_roots(additional_pem_roots),
        )
        .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Some(selected) = lua.as_ref()
                    && let Err(source) = selected.runtime.shutdown().await
                {
                    shutdown.push("Lua", source);
                }
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                llm.shutdown().await;
                let mut shutdown = ShutdownFailures::default();
                if let Some(selected) = lua.as_ref()
                    && let Err(source) = selected.runtime.shutdown().await
                {
                    shutdown.push("Lua", source);
                }
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let business_log =
            ProductionBusinessLog::for_translation(&project_log, progress_observer.clone());
        let builder = ProductionSelectedTranslationExecutionBuilder {
            configuration: command.rpg_maker(),
            file_system: file_system.clone(),
            cpu: cpu.clone(),
            sqlite: sqlite.clone(),
            llm: llm.clone(),
            lua: lua.clone(),
            log: business_log.clone(),
            cancellation: cancellation.clone(),
        };
        let service = TranslateService::new(
            opener,
            builder,
            AlreadyHeldProjectCommandLeaseProvider,
            cancellation.clone(),
        );
        let input = TranslateInput {
            name: command.project_name().clone(),
            terminology_path: command.terminology_path().map(Path::to_path_buf),
            placeholder_rules_path: command.placeholder_rules_path().map(Path::to_path_buf),
        };
        progress_observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::Planning,
        ));
        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_command(
            service.execute(input),
            &cancellation,
            termination_signals,
            || {
                progress.safe_stopping(safe_stopping);
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await
        .map(|result| {
            result.map_err(|error| {
                map_translate_error(
                    error,
                    ProductionTranslationExecutionBuildError::failure_impact,
                    |standard| standard.failure_impact(),
                )
            })
        });
        if matches!(&execution, DrivenCommand::Interrupted(_)) {
            let (confirmed, total) = progress_observer.confirmed_amount();
            project_log.emit_cancellation(ProjectLogCode::SafeStopFinished, confirmed, total);
        }
        business_log.emit_retry_summary();
        let no_model_work = matches!(
            &execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
                if output.standard.total_tasks == 0
        );
        if no_model_work {
            progress_observer.observe(ProgressSnapshot::indeterminate(
                TranslateProgressPhase::NoWork,
            ));
            let reason = project_log
                .localizer
                .format(UiMessage::NoticeNoModelRequest);
            project_log.logger.emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::NoWork,
                project_log.context.clone(),
                project_log
                    .localizer
                    .format(UiMessage::LogNoWork { reason: &reason }),
                ProjectLogPayload::NoWork {
                    reason_code: "translation_up_to_date".to_owned(),
                },
            ));
        }
        progress_observer.finish();
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        llm.shutdown().await;
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            progress.finalizing(progress_finalizing(self.locale));
        }
        if business_completed(&execution) && shutdown.is_empty() {
            progress.finalizing(progress_saving_plan(self.locale));
        }
        let plan_result = finalize_run_plan(
            &execution,
            &shutdown,
            database_path,
            replacement,
            sqlite_configuration,
            &project_log,
        )
        .await;
        execution = replace_success_with_plan_error(execution, plan_result);
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let log_warning = finish_project_log(project_log, log_outcome);
        ProductionCommandRunReport::from_completion_with_log_warning(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Translate {
                        output,
                        profile_source,
                        lua_source,
                        lua_cleared,
                    })
                })
            }),
            shutdown,
            log_warning,
        )
    }

    async fn run_write_back(
        self,
        command: ConfiguredWriteBackCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let progress = write_back_terminal_progress(self.progress_mode, self.locale);
        let project_log = start_command_log(
            command.common(),
            self.locale,
            self.layout,
            command.project_name().as_str(),
            "write-back",
            None,
        );
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            write_back_phase_code,
        );
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return observed_construction_failure(
                    project_log,
                    error,
                    ShutdownFailures::default(),
                )
                .await;
            }
        };
        let sqlite = match RusqliteStorage::start(sqlite_configuration.clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let project_name = command.project_name().clone();
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine(),
            file_system.clone(),
        );
        let project_lease_guard = match lease_provider.acquire(&project_name).await {
            Ok(lease) => lease,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return observed_construction_failure(
                    project_log,
                    ProductionCommandError::ProjectUnavailable(Box::new(error)),
                    shutdown,
                )
                .await;
            }
        };
        let explicit_lua_requested = command.lua().is_some();
        let lua = match command.lua() {
            Some(selected) => match load_lua_selection(&file_system, selected).await {
                Ok(program) => (!program.program.source().is_empty()).then_some(program),
                Err(source) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", error);
                    }
                    if let Err(error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", error);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ConfigurationOrInput(Box::new(source)),
                        shutdown,
                    )
                    .await;
                }
            },
            None => {
                let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
                match repository.read(database_path.clone()).await {
                    Ok(plans) => match plans.write_back().and_then(WriteBackRunPlan::lua_program) {
                        Some(snapshot) => match command.resolve_lua_runtime() {
                            Ok(runtime) => Some(ProductionLuaSelection {
                                program: OwnedLuaProgram::new(
                                    snapshot.resolved_path().to_path_buf(),
                                    snapshot.source().to_vec(),
                                ),
                                runtime: TrustedLua54Runtime::new(
                                    runtime,
                                    tokio::runtime::Handle::current(),
                                ),
                            }),
                            Err(error) => {
                                let mut shutdown = ShutdownFailures::default();
                                if let Err(shutdown_error) = sqlite.shutdown().await {
                                    shutdown.push("SQLite", shutdown_error);
                                }
                                if let Err(shutdown_error) = file_system.shutdown().await {
                                    shutdown.push("FileSystem", shutdown_error);
                                }
                                drop(project_lease_guard);
                                return observed_construction_failure(
                                    project_log,
                                    ProductionCommandError::ConfigurationOrInput(Box::new(error)),
                                    shutdown,
                                )
                                .await;
                            }
                        },
                        None => None,
                    },
                    Err(error) => {
                        let error = match error {
                            error @ ProjectRunPlanReadError::DatabaseNotFound { .. } => {
                                ProductionCommandError::ProjectUnavailable(Box::new(error))
                            }
                            error => ProductionCommandError::ProjectState(Box::new(error)),
                        };
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(shutdown_error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", shutdown_error);
                        }
                        if let Err(shutdown_error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", shutdown_error);
                        }
                        drop(project_lease_guard);
                        return observed_construction_failure(project_log, error, shutdown).await;
                    }
                }
            }
        };
        let lua_cleared = explicit_lua_requested && lua.is_none();
        let plan_source = if command.lua().is_some() {
            ProjectLogValueSource::Explicit
        } else {
            let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
            match repository.read(database_path.clone()).await {
                Ok(plans) if plans.write_back().is_some() => ProjectLogValueSource::ProjectState,
                Ok(_) => ProjectLogValueSource::ProductDefault,
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Some(selected) = lua.as_ref()
                        && let Err(source) = selected.runtime.shutdown().await
                    {
                        shutdown.push("Lua", source);
                    }
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::ProjectState(Box::new(error)),
                        shutdown,
                    )
                    .await;
                }
            }
        };
        let replacement = match lua.as_ref() {
            Some(selection) => match LuaProgramSnapshot::new(
                selection.program.main_script_path().to_path_buf(),
                selection.program.source().to_vec(),
            ) {
                Ok(snapshot) => {
                    ProjectRunPlanReplacement::WriteBack(WriteBackRunPlan::with_lua(snapshot))
                }
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(source) = selection.runtime.shutdown().await {
                        shutdown.push("Lua", source);
                    }
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    drop(project_lease_guard);
                    return observed_construction_failure(
                        project_log,
                        ProductionCommandError::Internal(Box::new(error)),
                        shutdown,
                    )
                    .await;
                }
            },
            None => ProjectRunPlanReplacement::WriteBack(WriteBackRunPlan::standard_only()),
        };
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            localized_plan_resolution(&project_log.localizer, "write-back", plan_source),
            ProjectLogPayload::RunPlan {
                source: plan_source,
                lua_source: None,
                selections: vec![String::from("Standard")],
                lua_enabled: Some(lua.is_some()),
            },
        ));
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Some(selected) = lua.as_ref()
                    && let Err(source) = selected.runtime.shutdown().await
                {
                    shutdown.push("Lua", source);
                }
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let asset_reader = RpgMakerStandardWriteBackAssetReadingService::new(
            sqlite.clone(),
            cpu.clone(),
            command.rpg_maker().standard_asset(),
        );
        let document_reader = RpgMakerProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            command.rpg_maker().document(),
        );
        let rewriter = RpgMakerWriteBackDocumentRewritingService::new(document_reader, cpu.clone())
            .with_progress(progress_observer.clone());
        let standard = StandardWriteBackService::new(
            asset_reader,
            ConservativeRpgMakerWriteBackTextLayouter,
            rewriter,
            cpu.clone(),
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let publisher = StandardWriteBackPublishingService::new(directory_publisher.clone());
        let selected_lua = lua.as_ref().map(|selected| {
                let host = TrustedLuaExecutionHostingService::<_, OpenAiChatCompletionExecutor, _, _>::without_llm(
                    file_system.clone(), selected.runtime.clone(), sqlite.clone(),
                );
                SelectedLua::new(
                    selected.program.clone(),
                    LuaWriteBackService::new(host, directory_publisher),
                )
        });
        let service = WriteBackService::new(
            opener,
            standard,
            publisher,
            selected_lua,
            ProductionBusinessLog::from_active(&project_log),
            AlreadyHeldProjectCommandLeaseProvider,
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let input = WriteBackInput {
            name: command.project_name().clone(),
        };
        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_command(
            service.execute(input),
            &cancellation,
            termination_signals,
            || {
                progress.safe_stopping(safe_stopping);
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await
        .map(|result| result.map_err(map_write_back_error));
        if matches!(&execution, DrivenCommand::Interrupted(_)) {
            let (confirmed, total) = progress_observer.confirmed_amount();
            project_log.emit_cancellation(ProjectLogCode::SafeStopFinished, confirmed, total);
        }
        progress_observer.finish();
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            progress.finalizing(progress_finalizing(self.locale));
        }
        if business_completed(&execution) && shutdown.is_empty() {
            progress.finalizing(progress_saving_plan(self.locale));
        }
        let plan_result = finalize_run_plan(
            &execution,
            &shutdown,
            database_path,
            replacement,
            sqlite_configuration,
            &project_log,
        )
        .await;
        execution = replace_success_with_plan_error(execution, plan_result);
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let log_warning = finish_project_log(project_log, log_outcome);
        ProductionCommandRunReport::from_completion_with_log_warning(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::WriteBack {
                        output,
                        plan_source,
                        lua_cleared,
                    })
                })
            }),
            shutdown,
            log_warning,
        )
    }
}

async fn load_rules_program(
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

async fn load_lua_selection(
    file_system: &SystemFileSystem,
    selected: &SelectedLuaConfiguration,
) -> Result<ProductionLuaSelection, LuaProgramInputError> {
    let requested_path = selected.script_path().to_path_buf();
    let file = file_system
        .read_file(requested_path.clone())
        .await
        .map_err(|source| LuaProgramInputError::Read {
            path: requested_path,
            source,
        })?;
    let resolved_path = file.resolved_path().to_path_buf();
    let bytes = file.into_bytes();
    Ok(ProductionLuaSelection {
        program: OwnedLuaProgram::new(resolved_path, bytes),
        runtime: TrustedLua54Runtime::new(selected.runtime(), tokio::runtime::Handle::current()),
    })
}

#[derive(Debug)]
enum RulesProgramInputError {
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

#[derive(Debug)]
enum LuaProgramInputError {
    Read {
        path: PathBuf,
        source: ReadFileError<SystemFileSystemError>,
    },
}

#[derive(Debug)]
enum RunPlanResolutionError {
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

impl fmt::Display for LuaProgramInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "读取 Lua 主程序失败 {}：{source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for LuaProgramInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
        }
    }
}

async fn load_mv_dialogue_definition(
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
    if bytes.len() > MAX_MV_DIALOGUE_DEFINITION_BYTES {
        return Err(MvDialogueDefinitionInputError::TooLarge {
            path: requested_path,
            actual: bytes.len(),
            maximum: MAX_MV_DIALOGUE_DEFINITION_BYTES,
        });
    }
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
enum MvDialogueDefinitionInputError {
    Read {
        path: PathBuf,
        source: ReadFileError<SystemFileSystemError>,
    },
    TooLarge {
        path: PathBuf,
        actual: usize,
        maximum: usize,
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
            Self::TooLarge {
                path,
                actual,
                maximum,
            } => write!(
                formatter,
                "MV 对话定义 {} 过大：实际 {actual} 字节，上限 {maximum} 字节",
                path.display(),
            ),
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
            Self::TooLarge { .. } => None,
        }
    }
}

struct ActiveProjectLog {
    runtime: ProjectLogRuntime,
    logger: ProjectLogger,
    context: ProjectLogContext,
    localizer: Arc<UiLocalizer>,
    command: &'static str,
}

#[derive(Clone)]
struct ProductionProgressObserver<P> {
    terminal: TerminalProgressObserver<P>,
    logger: ProjectLogger,
    context: ProjectLogContext,
    localizer: Arc<UiLocalizer>,
    phase_code: fn(P) -> &'static str,
    state: Arc<Mutex<ProgressLogState<P>>>,
}

struct ProgressLogState<P> {
    phase: Option<P>,
    amount: ProgressAmount,
    finished: bool,
}

impl<P> ProductionProgressObserver<P>
where
    P: Copy + Eq,
{
    fn new(
        terminal: TerminalProgressObserver<P>,
        project_log: &ActiveProjectLog,
        phase_code: fn(P) -> &'static str,
    ) -> Self {
        Self {
            terminal,
            logger: project_log.logger.clone(),
            context: project_log.context.clone(),
            localizer: Arc::clone(&project_log.localizer),
            phase_code,
            state: Arc::new(Mutex::new(ProgressLogState {
                phase: None,
                amount: ProgressAmount::Indeterminate,
                finished: false,
            })),
        }
    }

    fn finish(&self) {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match (state.phase, state.finished) {
                (Some(phase), false) => {
                    state.finished = true;
                    Some((ProjectLogCode::PhaseFinished, phase, state.amount))
                }
                (None, _) | (Some(_), true) => None,
            }
        };
        if let Some((code, phase, amount)) = event {
            self.emit_log_event(code, phase, amount);
        }
    }

    fn confirmed_amount(&self) -> (u64, Option<u64>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.amount {
            ProgressAmount::Indeterminate => (0, None),
            ProgressAmount::Determinate { completed, total } => (completed, Some(total)),
        }
    }

    fn emit_log_event(&self, code: ProjectLogCode, phase: P, amount: ProgressAmount) {
        let phase_code = (self.phase_code)(phase);
        let phase = self.localizer.format(
            project_log_phase_label(phase_code).expect("每个生产阶段代码都必须具有本地化日志标签"),
        );
        let message = match code {
            ProjectLogCode::PhaseStarted => self
                .localizer
                .format(UiMessage::LogPhaseStarted { phase: &phase }),
            ProjectLogCode::PhaseFinished => self
                .localizer
                .format(UiMessage::LogPhaseFinished { phase: &phase }),
            _ => unreachable!("进度观察者只产生阶段开始和完成事件"),
        };
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            code,
            self.context.clone(),
            message,
            ProjectLogPayload::Phase {
                phase: phase_code.to_owned(),
                amount: match amount {
                    ProgressAmount::Indeterminate => ProjectLogAmount::Indeterminate,
                    ProgressAmount::Determinate { completed, total } => {
                        ProjectLogAmount::Determinate { completed, total }
                    }
                },
            },
        ));
    }
}

impl<P> ProgressObserver<P> for ProductionProgressObserver<P>
where
    P: Copy + Eq + Send + 'static,
{
    fn observe(&self, snapshot: ProgressSnapshot<P>) {
        self.terminal.observe(snapshot.clone());
        let mut events = Vec::with_capacity(3);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.phase != Some(snapshot.phase) {
                if let Some(previous) = state.phase
                    && !state.finished
                {
                    events.push((ProjectLogCode::PhaseFinished, previous, state.amount));
                }
                state.phase = Some(snapshot.phase);
                state.amount = snapshot.amount;
                state.finished = false;
                events.push((
                    ProjectLogCode::PhaseStarted,
                    snapshot.phase,
                    snapshot.amount,
                ));
            } else {
                state.amount = snapshot.amount;
            }
            if matches!(
                snapshot.amount,
                ProgressAmount::Determinate { completed, total }
                    if total > 0 && completed == total
            ) && !state.finished
            {
                state.finished = true;
                events.push((
                    ProjectLogCode::PhaseFinished,
                    snapshot.phase,
                    snapshot.amount,
                ));
            }
        }
        for (code, phase, amount) in events {
            self.emit_log_event(code, phase, amount);
        }
    }
}

const fn init_phase_code(phase: InitProgressPhase) -> &'static str {
    match phase {
        InitProgressPhase::CheckingProject => "check_project",
        InitProgressPhase::ScanningSource => "scan_source",
        InitProgressPhase::PreparingCandidate => "prepare_candidate",
        InitProgressPhase::UpdatingDatabase => "update_database",
        InitProgressPhase::Publishing => "publish",
    }
}

const fn extract_phase_code(phase: ExtractProgressPhase) -> &'static str {
    match phase {
        ExtractProgressPhase::Builtin => "builtin",
        ExtractProgressPhase::BuiltinDocuments => "builtin_documents",
        ExtractProgressPhase::BuiltinWorkUnits => "builtin_work_units",
        ExtractProgressPhase::BuiltinCommit => "builtin_commit",
        ExtractProgressPhase::Rules => "rules",
        ExtractProgressPhase::RulesDocuments => "rules_documents",
        ExtractProgressPhase::RulesMatches => "rules_matches",
        ExtractProgressPhase::RulesCommit => "rules_commit",
        ExtractProgressPhase::Lua => "lua",
        ExtractProgressPhase::LuaExecution => "lua_execution",
        ExtractProgressPhase::LuaCommit => "lua_commit",
    }
}

const fn translate_phase_code(phase: TranslateProgressPhase) -> &'static str {
    match phase {
        TranslateProgressPhase::Planning => "planning",
        TranslateProgressPhase::ConfirmedTasks => "confirmed_tasks",
        TranslateProgressPhase::NoWork => "no_work",
    }
}

const fn write_back_phase_code(phase: WriteBackProgressPhase) -> &'static str {
    match phase {
        WriteBackProgressPhase::ReadingAssets => "read_assets",
        WriteBackProgressPhase::PlanningStandard => "plan_standard",
        WriteBackProgressPhase::RewritingDocuments => "rewrite_documents",
        WriteBackProgressPhase::PreparingCandidate => "prepare_candidate",
        WriteBackProgressPhase::RunningLua => "lua",
        WriteBackProgressPhase::ValidatingCandidate => "validate_candidate",
        WriteBackProgressPhase::Publishing => "publish",
    }
}

impl ActiveProjectLog {
    fn set_profile(&mut self, profile: &str) {
        self.context = self.context.clone().with_profile(profile);
    }

    fn emit_cancellation(&self, code: ProjectLogCode, confirmed: u64, total: Option<u64>) {
        let message = match code {
            ProjectLogCode::CancellationRequested => {
                self.localizer.format(UiMessage::ProgressSafeStopping)
            }
            ProjectLogCode::SafeStopFinished => self.localizer.format(UiMessage::ResultCancelled),
            _ => unreachable!("取消观察只产生请求和安全停止完成事件"),
        };
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            code,
            self.context.clone(),
            message,
            ProjectLogPayload::Cancellation { confirmed, total },
        ));
    }
}

fn start_command_log(
    common: &crate::application::config::CommonCommandConfiguration,
    locale: UiLocale,
    layout: RpgMakerLayout,
    project: &str,
    command: &'static str,
    profile: Option<&str>,
) -> ActiveProjectLog {
    let runtime = start_project_log(
        common.observability_root().to_path_buf(),
        common.project_log(),
    );
    let logger = runtime.logger();
    let localizer = Arc::new(UiLocalizer::new(locale));
    let mut context = ProjectLogContext::new(locale.as_str())
        .with_engine(layout.engine().storage_name())
        .with_project(project)
        .with_command(command);
    if let Ok(run_id) = generate_run_id() {
        context = context.with_run_id(run_id.to_string());
    }
    if let Some(profile) = profile {
        context = context.with_profile(profile);
    }
    logger.emit(ProjectLogEvent::new(
        ProjectLogLevel::Info,
        ProjectLogCode::RunStarted,
        context.clone(),
        localizer.format(UiMessage::LogRunStarted { command }),
        ProjectLogPayload::Run { outcome: None },
    ));
    ActiveProjectLog {
        runtime,
        logger,
        context,
        localizer,
        command,
    }
}

fn finish_project_log(
    project_log: ActiveProjectLog,
    outcome: ProjectLogRunOutcome,
) -> Option<ProjectLogWarning> {
    let message = match outcome {
        ProjectLogRunOutcome::Succeeded => {
            project_log.localizer.format(UiMessage::LogRunSucceeded {
                command: project_log.command,
            })
        }
        ProjectLogRunOutcome::Cancelled => {
            project_log.localizer.format(UiMessage::LogRunCancelled {
                command: project_log.command,
            })
        }
        ProjectLogRunOutcome::Failed | ProjectLogRunOutcome::OutcomeUnknown => {
            project_log.localizer.format(UiMessage::LogRunFailed {
                command: project_log.command,
            })
        }
    };
    project_log.logger.emit(ProjectLogEvent::new(
        match outcome {
            ProjectLogRunOutcome::Succeeded | ProjectLogRunOutcome::Cancelled => {
                ProjectLogLevel::Info
            }
            ProjectLogRunOutcome::Failed | ProjectLogRunOutcome::OutcomeUnknown => {
                ProjectLogLevel::Error
            }
        },
        ProjectLogCode::RunFinished,
        project_log.context,
        message,
        ProjectLogPayload::Run {
            outcome: Some(outcome),
        },
    ));
    let logger = project_log.logger;
    let _health = project_log.runtime.shutdown();
    logger.take_warning()
}

async fn observed_construction_failure(
    project_log: ActiveProjectLog,
    error: ProductionCommandError,
    shutdown: ShutdownFailures,
) -> ProductionCommandRunReport {
    let outcome = if matches!(error, ProductionCommandError::OutcomeUnknown(_)) {
        ProjectLogRunOutcome::OutcomeUnknown
    } else {
        ProjectLogRunOutcome::Failed
    };
    let warning = finish_project_log(project_log, outcome);
    ProductionCommandRunReport::construction_failed_with_shutdown_and_log_warning(
        error, shutdown, warning,
    )
}

fn business_completed<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> bool {
    matches!(
        execution,
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
    )
}

async fn finalize_run_plan<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
    database_path: PathBuf,
    replacement: ProjectRunPlanReplacement,
    sqlite_configuration: crate::runtime::sqlite::RusqliteStorageConfiguration,
    project_log: &ActiveProjectLog,
) -> Result<(), ProductionCommandError> {
    if !business_completed(execution) || !shutdown.is_empty() {
        return Ok(());
    }
    let finalizer = FinalProjectRunPlanPersistenceService::new(
        RusqliteFinalTransactionExecutor::new(sqlite_configuration),
    );
    match finalizer.replace_final(database_path, replacement).await {
        Ok(()) => {
            project_log.logger.emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::RunPlanSaved,
                project_log.context.clone(),
                project_log.localizer.format(UiMessage::ResultPlanSaved),
                ProjectLogPayload::None,
            ));
            Ok(())
        }
        Err(error) => {
            let (level, code, message) = run_plan_replace_log_fact(&error, &project_log.localizer);
            project_log.logger.emit(ProjectLogEvent::new(
                level,
                code,
                project_log.context.clone(),
                message,
                ProjectLogPayload::None,
            ));
            Err(map_run_plan_replace_error(error))
        }
    }
}

fn run_plan_replace_log_fact<E>(
    error: &ProjectRunPlanReplaceError<E>,
    localizer: &UiLocalizer,
) -> (ProjectLogLevel, ProjectLogCode, String) {
    match error {
        ProjectRunPlanReplaceError::DatabaseNotFound { .. }
        | ProjectRunPlanReplaceError::RequirementFailed { .. }
        | ProjectRunPlanReplaceError::RollbackConfirmed { .. } => (
            ProjectLogLevel::Warn,
            ProjectLogCode::RunPlanSaveFailed,
            localizer.format(UiMessage::ErrorPlanSaveFailedApplied),
        ),
        ProjectRunPlanReplaceError::OutcomeUnknown { .. } => (
            ProjectLogLevel::Error,
            ProjectLogCode::RunPlanSaveOutcomeUnknown,
            localizer.format(UiMessage::ErrorPlanSaveOutcomeUnknown),
        ),
        ProjectRunPlanReplaceError::CommittedButFinalizationFailed { .. } => (
            ProjectLogLevel::Error,
            ProjectLogCode::RunPlanSavedFinalizationFailed,
            format!(
                "{} {}",
                localizer.format(UiMessage::ResultPlanSaved),
                localizer.format(UiMessage::ErrorStateAppliedFinalization)
            ),
        ),
    }
}

fn map_run_plan_replace_error<E>(error: ProjectRunPlanReplaceError<E>) -> ProductionCommandError
where
    E: Error + Send + Sync + 'static,
{
    match error {
        error @ (ProjectRunPlanReplaceError::DatabaseNotFound { .. }
        | ProjectRunPlanReplaceError::RequirementFailed { .. }
        | ProjectRunPlanReplaceError::RollbackConfirmed { .. }) => {
            ProductionCommandError::ResultAppliedButRunPlanNotSaved(Box::new(error))
        }
        error @ ProjectRunPlanReplaceError::OutcomeUnknown { .. } => {
            ProductionCommandError::RunPlanOutcomeUnknown(Box::new(error))
        }
        error @ ProjectRunPlanReplaceError::CommittedButFinalizationFailed { .. } => {
            ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
        }
    }
}

fn replace_success_with_plan_error<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    plan_result: Result<(), ProductionCommandError>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    match plan_result {
        Ok(()) => execution,
        Err(error) if business_completed(&execution) => DrivenCommand::Finished(Err(error)),
        Err(_) => execution,
    }
}

#[derive(Clone)]
struct ProductionBusinessLog {
    logger: ProjectLogger,
    context: ProjectLogContext,
    localizer: Arc<UiLocalizer>,
    translation_total: Arc<AtomicU64>,
    translation_confirmed: Arc<AtomicU64>,
    translation_retry_attempts: Arc<AtomicU64>,
    translation_retry_recovered: Arc<AtomicU64>,
    translation_retry_exhausted: Arc<AtomicU64>,
    translation_progress: Option<ProductionProgressObserver<TranslateProgressPhase>>,
}

impl ProductionBusinessLog {
    fn from_active(project_log: &ActiveProjectLog) -> Self {
        Self {
            logger: project_log.logger.clone(),
            context: project_log.context.clone(),
            localizer: Arc::clone(&project_log.localizer),
            translation_total: Arc::new(AtomicU64::new(0)),
            translation_confirmed: Arc::new(AtomicU64::new(0)),
            translation_retry_attempts: Arc::new(AtomicU64::new(0)),
            translation_retry_recovered: Arc::new(AtomicU64::new(0)),
            translation_retry_exhausted: Arc::new(AtomicU64::new(0)),
            translation_progress: None,
        }
    }

    fn for_translation(
        project_log: &ActiveProjectLog,
        progress: ProductionProgressObserver<TranslateProgressPhase>,
    ) -> Self {
        Self {
            translation_progress: Some(progress),
            ..Self::from_active(project_log)
        }
    }

    fn emit_retry_summary(&self) {
        let attempted = self.translation_retry_attempts.load(Ordering::Acquire);
        if attempted == 0 {
            return;
        }
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RetrySummary,
            self.context.clone(),
            self.localizer
                .format(UiMessage::LogRetrySummary { count: attempted }),
            ProjectLogPayload::RetrySummary {
                attempted,
                recovered: self.translation_retry_recovered.load(Ordering::Acquire),
                exhausted: self.translation_retry_exhausted.load(Ordering::Acquire),
            },
        ));
    }
}

impl StandardTranslationLog for ProductionBusinessLog {
    fn emit(&self, event: StandardTranslationLogEvent) {
        match event {
            StandardTranslationLogEvent::PlanningUnresolved { units } => {
                self.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Warn,
                    ProjectLogCode::PartialResult,
                    self.context.clone(),
                    self.localizer.format(UiMessage::LogPartialResult {
                        count: u64::try_from(units).unwrap_or(u64::MAX),
                    }),
                    ProjectLogPayload::ResultSummary {
                        complete: 0,
                        partial: 0,
                        unavailable: u64::try_from(units).unwrap_or(u64::MAX),
                        manual_review: 0,
                    },
                ));
            }
            StandardTranslationLogEvent::TaskStarted {
                task_index,
                total_tasks,
            } => {
                let total = u64::try_from(total_tasks).unwrap_or(u64::MAX);
                self.translation_total.store(total, Ordering::Relaxed);
                if let Some(progress) = &self.translation_progress {
                    progress.observe(ProgressSnapshot::determinate(
                        TranslateProgressPhase::ConfirmedTasks,
                        self.translation_confirmed.load(Ordering::Acquire),
                        total,
                    ));
                }
                let ordinal = u64::try_from(task_index.get())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                self.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Debug,
                    ProjectLogCode::TaskStarted,
                    self.context.clone(),
                    self.localizer.format(UiMessage::LogTranslationTaskStarted {
                        index: ordinal,
                        total,
                    }),
                    ProjectLogPayload::Task {
                        ordinal,
                        total,
                        outcome: None,
                        attempts: None,
                    },
                ));
            }
            StandardTranslationLogEvent::TaskFinished {
                task_index,
                outcome,
                attempts,
                retry_exhausted,
            } => {
                let ordinal = u64::try_from(task_index.get())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                let total = self.translation_total.load(Ordering::Relaxed);
                let is_confirmed = matches!(
                    outcome,
                    StandardTranslationLogTaskOutcome::Complete
                        | StandardTranslationLogTaskOutcome::Partial
                        | StandardTranslationLogTaskOutcome::Unavailable
                );
                let retries = attempts
                    .map(|value| {
                        u64::try_from(value.get())
                            .unwrap_or(u64::MAX)
                            .saturating_sub(1)
                    })
                    .unwrap_or(0);
                if retries > 0 {
                    let _ = self.translation_retry_attempts.fetch_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |current| Some(current.saturating_add(retries)),
                    );
                    if retry_exhausted {
                        let _ = self.translation_retry_exhausted.fetch_update(
                            Ordering::AcqRel,
                            Ordering::Acquire,
                            |current| Some(current.saturating_add(1)),
                        );
                    } else {
                        let _ = self.translation_retry_recovered.fetch_update(
                            Ordering::AcqRel,
                            Ordering::Acquire,
                            |current| Some(current.saturating_add(1)),
                        );
                    }
                }
                let (outcome_code, outcome) = match outcome {
                    StandardTranslationLogTaskOutcome::Complete => (
                        "complete",
                        crate::runtime::project_log::ProjectLogTaskOutcome::Complete,
                    ),
                    StandardTranslationLogTaskOutcome::Partial => (
                        "partial",
                        crate::runtime::project_log::ProjectLogTaskOutcome::Partial,
                    ),
                    StandardTranslationLogTaskOutcome::Unavailable => (
                        "unavailable",
                        crate::runtime::project_log::ProjectLogTaskOutcome::Unavailable,
                    ),
                    StandardTranslationLogTaskOutcome::ExecutionFailed
                    | StandardTranslationLogTaskOutcome::CommitFailed
                    | StandardTranslationLogTaskOutcome::NotCommitted
                    | StandardTranslationLogTaskOutcome::InvalidResult => (
                        "failed",
                        crate::runtime::project_log::ProjectLogTaskOutcome::Failed,
                    ),
                };
                let outcome_name = self.localizer.format(
                    project_log_task_outcome_label(outcome_code)
                        .expect("每个任务结果代码都必须具有本地化日志标签"),
                );
                self.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Debug,
                    ProjectLogCode::TaskFinished,
                    self.context.clone(),
                    self.localizer
                        .format(UiMessage::LogTranslationTaskFinished {
                            index: ordinal,
                            outcome: &outcome_name,
                        }),
                    ProjectLogPayload::Task {
                        ordinal,
                        total,
                        outcome: Some(outcome),
                        attempts: attempts
                            .map(|value| u64::try_from(value.get()).unwrap_or(u64::MAX)),
                    },
                ));
                if is_confirmed {
                    let confirmed = self
                        .translation_confirmed
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    if let Some(progress) = &self.translation_progress {
                        progress.observe(ProgressSnapshot::determinate(
                            TranslateProgressPhase::ConfirmedTasks,
                            confirmed,
                            total,
                        ));
                    }
                }
            }
        }
    }
}

impl WriteBackLog for ProductionBusinessLog {
    fn emit(&self, event: WriteBackLogEvent) {
        match event {
            WriteBackLogEvent::PublicationStarted { output_root } => {
                self.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Info,
                    ProjectLogCode::PublicationStarted,
                    self.context.clone(),
                    self.localizer.format(UiMessage::LogPhaseStarted {
                        phase: "publication",
                    }),
                    ProjectLogPayload::Publication {
                        outcome:
                            crate::runtime::project_log::ProjectLogPublicationOutcome::NotPublished,
                        published_items: None,
                    },
                ));
                let _ = output_root;
            }
            WriteBackLogEvent::PublicationFinished {
                output_root,
                outcome,
            } => {
                use crate::runtime::project_log::ProjectLogPublicationOutcome as LogOutcome;
                let (level, log_outcome, manual_review) = match outcome {
                    WriteBackLogPublicationOutcome::Published { standard, .. } => (
                        ProjectLogLevel::Info,
                        LogOutcome::Published,
                        u64::try_from(standard.manual_layout_units).unwrap_or(u64::MAX),
                    ),
                    WriteBackLogPublicationOutcome::NotPublished => {
                        (ProjectLogLevel::Warn, LogOutcome::NotPublished, 0)
                    }
                    WriteBackLogPublicationOutcome::PublishedWithResiduals => {
                        (ProjectLogLevel::Warn, LogOutcome::RecoveryRequired, 0)
                    }
                    WriteBackLogPublicationOutcome::RecoveryRequired => {
                        (ProjectLogLevel::Error, LogOutcome::RecoveryRequired, 0)
                    }
                    WriteBackLogPublicationOutcome::OutcomeUnknown => {
                        (ProjectLogLevel::Error, LogOutcome::OutcomeUnknown, 0)
                    }
                };
                self.logger.emit(ProjectLogEvent::new(
                    level,
                    ProjectLogCode::PublicationFinished,
                    self.context.clone(),
                    self.localizer.format(UiMessage::LogPublishFinished {
                        path: &output_root.to_string_lossy(),
                    }),
                    ProjectLogPayload::Publication {
                        outcome: log_outcome,
                        published_items: (manual_review > 0).then_some(manual_review),
                    },
                ));
            }
        }
    }
}

type ProductionTranslationProfile = Arc<RpgMakerTranslationProfile<OpenAiChatCompletionClient>>;
type ProductionTranslationAssetReader =
    RpgMakerStandardTranslationAssetReadingService<RusqliteStorage, RayonCpuExecutor>;
type ProductionTranslationPlanner = RpgMakerStandardTranslationTaskPlanningService<
    TranslationPlanningResourceReadingService<SystemFileSystem, RayonCpuExecutor>,
    RayonCpuExecutor,
    OpenAiChatCompletionClient,
>;
type ProductionTranslationExecutor = RpgMakerStandardTranslationTaskExecutionService<
    OpenAiChatCompletionExecutor,
    TokioAsyncDelay,
    TranslationTaskResponseProcessingService<RayonCpuExecutor>,
    ProductionTranslationProfile,
>;
type ProductionTranslationStore =
    RpgMakerStandardTranslationResultStorageService<RusqliteStorage, RayonCpuExecutor>;
type ProductionStandardTranslation = StandardTranslationService<
    ProductionTranslationAssetReader,
    ProductionTranslationPlanner,
    ProductionTranslationExecutor,
    ProductionTranslationStore,
    ProductionBusinessLog,
>;
type ProductionLuaHost = TrustedLuaExecutionHostingService<
    SystemFileSystem,
    OpenAiChatCompletionExecutor,
    TrustedLua54Runtime,
    RusqliteStorage,
>;
type ProductionLuaTranslation = LuaTranslationService<ProductionLuaHost>;

struct ProductionSelectedTranslationExecutionBuilder<'a> {
    configuration: &'a TranslateConfiguration,
    file_system: SystemFileSystem,
    cpu: RayonCpuExecutor,
    sqlite: RusqliteStorage,
    llm: OpenAiChatCompletionExecutor,
    lua: Option<ProductionLuaSelection>,
    log: ProductionBusinessLog,
    cancellation: CooperativeCancellation,
}

impl SelectedTranslationExecutionBuilder for ProductionSelectedTranslationExecutionBuilder<'_> {
    type Client = OpenAiChatCompletionClient;
    type Standard = ProductionStandardTranslation;
    type Lua = ProductionLuaTranslation;
    type Error = ProductionTranslationExecutionBuildError;

    async fn build(
        &self,
        project: &crate::rpg_maker::project::OpenedProject,
    ) -> Result<SelectedTranslationExecution<Self::Client, Self::Standard, Self::Lua>, Self::Error>
    {
        let profile_configuration = self.configuration.profile();
        let language_pair = project.language_pair().clone();
        let path = self
            .configuration
            .prompt_root()
            .join(RPG_MAKER_PROMPT_DIRECTORY_NAME)
            .join(format!(
                "{}--{}.md",
                language_pair.source(),
                language_pair.target()
            ));
        let file = self
            .file_system
            .read_file(path.clone())
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt(&language_pair, &path, source)
            })?;
        if file.resolved_path().file_name() != path.file_name() {
            return Err(ProductionTranslationExecutionBuildError::prompt(
                &language_pair,
                &path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "固定后的 Prompt 文件名与规范语言对派生文件名不一致",
                ),
            ));
        }
        let markdown = String::from_utf8(file.into_bytes()).map_err(|source| {
            let utf8 = source.utf8_error();
            ProductionTranslationExecutionBuildError::prompt(
                &language_pair,
                &path,
                Utf8ResourceError {
                    path: path.clone(),
                    valid_up_to: utf8.valid_up_to(),
                    error_len: utf8.error_len(),
                },
            )
        })?;
        let system_prompt =
            RpgMakerSystemPrompt::new(language_pair.clone(), markdown).map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt(&language_pair, &path, source)
            })?;
        let source_language = self
            .configuration
            .language_modules()
            .resolve(language_pair.source())
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::language_module(&language_pair, source)
            })?;
        let translation_resources = Arc::new(ResolvedRpgMakerTranslationResources::new(
            system_prompt,
            source_language,
        ));
        let planning = RpgMakerTranslationPlanningConfiguration::new(
            profile_configuration.planning().max_message_characters(),
        );
        let profile = Arc::new(RpgMakerTranslationProfile::new(
            profile_configuration.id(),
            profile_configuration.max_in_flight_tasks(),
            planning,
            profile_configuration.request().clone(),
            Arc::clone(self.configuration.client()),
        ));
        let placeholders = Pcre2PlaceholderService::new()
            .map_err(ProductionTranslationExecutionBuildError::internal)?;
        let asset_reader = RpgMakerStandardTranslationAssetReadingService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
            self.configuration.standard_asset(),
        );
        let resources = TranslationPlanningResourceReadingService::new(
            self.file_system.clone(),
            self.cpu.clone(),
        );
        let planner = RpgMakerStandardTranslationTaskPlanningService::<
            _,
            _,
            OpenAiChatCompletionClient,
        >::new(
            resources,
            Arc::clone(&translation_resources),
            placeholders,
            self.cpu.clone(),
        );
        let processor =
            TranslationTaskResponseProcessingService::new(self.cpu.clone(), translation_resources);
        let executor = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            ProductionTranslationProfile,
        >::new(self.llm.clone(), TokioAsyncDelay, processor);
        let result_store = RpgMakerStandardTranslationResultStorageService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
            self.configuration.translate_store(),
        );
        let standard = StandardTranslationService::new(
            asset_reader,
            planner,
            executor,
            result_store,
            self.log.clone(),
            self.cancellation.clone(),
        );
        let lua = self.lua.as_ref().map(|selected| {
            let host = TrustedLuaExecutionHostingService::with_llm(
                self.file_system.clone(),
                self.llm.clone(),
                selected.runtime.clone(),
                self.sqlite.clone(),
            );
            SelectedLua::new(selected.program.clone(), LuaTranslationService::new(host))
        });
        Ok(SelectedTranslationExecution::new(profile, standard, lua))
    }
}

struct ProductionTranslationExecutionBuildError {
    impact: TranslationExecutionBuildFailureImpact,
    diagnostic: TranslationExecutionBuildDiagnostic,
    source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslationExecutionBuildFailureImpact {
    ConfigurationOrInput,
    Internal,
}

#[derive(Debug)]
enum TranslationExecutionBuildDiagnostic {
    PromptUnavailable {
        source_language: String,
        target_language: String,
        path: PathBuf,
    },
    LanguageModuleUnavailable {
        source_language: String,
        target_language: String,
    },
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    fn prompt(
        language_pair: &crate::language::LanguagePair,
        path: &Path,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            diagnostic: TranslationExecutionBuildDiagnostic::PromptUnavailable {
                source_language: language_pair.source().to_string(),
                target_language: language_pair.target().to_string(),
                path: path.to_owned(),
            },
            source: Box::new(source),
        }
    }

    fn language_module(
        language_pair: &crate::language::LanguagePair,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            diagnostic: TranslationExecutionBuildDiagnostic::LanguageModuleUnavailable {
                source_language: language_pair.source().to_string(),
                target_language: language_pair.target().to_string(),
            },
            source: Box::new(source),
        }
    }

    fn internal(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::Internal,
            diagnostic: TranslationExecutionBuildDiagnostic::Internal,
            source: Box::new(source),
        }
    }

    const fn failure_impact(&self) -> TranslationExecutionBuildFailureImpact {
        self.impact
    }

    const fn diagnostic(&self) -> &TranslationExecutionBuildDiagnostic {
        &self.diagnostic
    }
}

impl fmt::Debug for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTranslationExecutionBuildError")
            .field("impact", &self.impact)
            .field("diagnostic", &self.diagnostic)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.diagnostic {
            TranslationExecutionBuildDiagnostic::PromptUnavailable {
                source_language,
                target_language,
                path,
            } => write!(
                formatter,
                "RPG Maker system prompt unavailable for {source_language} -> {target_language}: {}",
                path.display()
            ),
            TranslationExecutionBuildDiagnostic::LanguageModuleUnavailable {
                source_language,
                target_language,
            } => write!(
                formatter,
                "RPG Maker source language module unavailable for {source_language} -> {target_language}"
            ),
            TranslationExecutionBuildDiagnostic::Internal => {
                formatter.write_str("failed to build the translation execution context")
            }
        }
    }
}

impl Error for ProductionTranslationExecutionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct Utf8ResourceError {
    path: PathBuf,
    valid_up_to: usize,
    error_len: Option<usize>,
}

impl fmt::Debug for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Utf8ResourceError")
            .field("path", &self.path)
            .field("valid_up_to", &self.valid_up_to)
            .field("error_len", &self.error_len)
            .finish()
    }
}

impl fmt::Display for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "系统提示词文件不是有效 UTF-8：{}",
            self.path.display()
        )
    }
}

impl Error for Utf8ResourceError {}

async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &crate::application::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, ProductionCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(ProductionCommandError::construct)?;
        roots.push(file.into_bytes());
    }
    Ok(roots)
}

enum DrivenCommand<T> {
    Finished(T),
    Interrupted(T),
    SignalFailed { source: io::Error, result: T },
}

impl<T> DrivenCommand<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> DrivenCommand<U> {
        match self {
            Self::Finished(value) => DrivenCommand::Finished(map(value)),
            Self::Interrupted(value) => DrivenCommand::Interrupted(map(value)),
            Self::SignalFailed { source, result } => DrivenCommand::SignalFailed {
                source,
                result: map(result),
            },
        }
    }
}

/// 在整个命令生命周期保留 Windows 控制信号订阅，避免业务 future 先完成时
/// 接收器被提前丢弃并触发 Windows 默认终止处理器。
pub(crate) struct TerminationSignals {
    state: TerminationSignalState,
}

enum TerminationSignalState {
    Listening {
        ctrl_c: tokio::signal::windows::CtrlC,
        ctrl_break: tokio::signal::windows::CtrlBreak,
    },
    RegistrationFailed(Option<io::Error>),
}

impl TerminationSignals {
    pub(crate) fn new() -> Self {
        let state = match tokio::signal::windows::ctrl_c() {
            Ok(ctrl_c) => match tokio::signal::windows::ctrl_break() {
                Ok(ctrl_break) => TerminationSignalState::Listening { ctrl_c, ctrl_break },
                Err(error) => TerminationSignalState::RegistrationFailed(Some(error)),
            },
            Err(error) => TerminationSignalState::RegistrationFailed(Some(error)),
        };
        Self { state }
    }

    async fn recv(&mut self) -> io::Result<()> {
        match &mut self.state {
            TerminationSignalState::Listening { ctrl_c, ctrl_break } => {
                tokio::select! {
                    signal = ctrl_c.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-C 信号源意外关闭")),
                    signal = ctrl_break.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-Break 信号源意外关闭")),
                }
            }
            TerminationSignalState::RegistrationFailed(error) => Err(error
                .take()
                .unwrap_or_else(|| io::Error::other("Windows 控制信号源不可用"))),
        }
    }
}

async fn drive_command<T>(
    future: impl Future<Output = T>,
    cancellation: &CooperativeCancellation,
    termination_signals: &mut TerminationSignals,
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = termination_signals.recv() => match signal {
            Ok(()) => {
                cancellation.request();
                on_cancellation();
                DrivenCommand::Interrupted(future.await)
            }
            Err(error) => {
                let result = future.await;
                DrivenCommand::SignalFailed { source: error, result }
            }
        },
        result = &mut future => DrivenCommand::Finished(result),
    }
}

pub(crate) struct ProductionCommandRunReport {
    pub(crate) result: CommandRunResult,
    pub(crate) shutdown_error: Option<ShutdownFailures>,
    pub(crate) log_warning: Option<ProjectLogWarning>,
}

pub(crate) enum CommandRunResult {
    Succeeded(RpgMakerCommandOutput),
    Interrupted,
    Failed(ProductionCommandError),
}

impl ProductionCommandRunReport {
    fn construction_failed_with_shutdown_and_log_warning(
        error: ProductionCommandError,
        shutdown: ShutdownFailures,
        log_warning: Option<ProjectLogWarning>,
    ) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            log_warning,
        }
    }

    fn from_completion_with_log_warning(
        execution: DrivenCommand<
            Result<OperationCompletion<RpgMakerCommandOutput>, ProductionCommandError>,
        >,
        shutdown: ShutdownFailures,
        log_warning: Option<ProjectLogWarning>,
    ) -> Self {
        let shutdown_error = (!shutdown.is_empty()).then_some(shutdown);
        match execution {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output))) => Self {
                result: CommandRunResult::Succeeded(output),
                shutdown_error,
                log_warning,
            },
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(_)) => Self {
                result: CommandRunResult::Interrupted,
                shutdown_error,
                log_warning,
            },
            DrivenCommand::Finished(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
                log_warning,
            },
            DrivenCommand::Interrupted(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
                log_warning,
            },
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(OperationCompletion::Completed(_)) => SignalOutcome::CompletedStateApplied,
                    Ok(OperationCompletion::Cancelled) => SignalOutcome::Cancelled,
                    Err(error) => SignalOutcome::CommandFailed(Box::new(error)),
                };
                Self {
                    result: CommandRunResult::Failed(ProductionCommandError::Signal {
                        source,
                        outcome,
                    }),
                    shutdown_error,
                    log_warning,
                }
            }
        }
    }
}

fn map_completion<T, U>(
    completion: OperationCompletion<T>,
    map: impl FnOnce(T) -> U,
) -> OperationCompletion<U> {
    match completion {
        OperationCompletion::Completed(value) => OperationCompletion::Completed(map(value)),
        OperationCompletion::Cancelled => OperationCompletion::Cancelled,
    }
}

fn map_init_error<W, P>(
    error: InitServiceError<W, P>,
    workspace_impact: impl FnOnce(&W) -> ProjectWorkspaceConvergenceFailureImpact,
) -> ProductionCommandError
where
    W: Error + Send + Sync + 'static,
    P: Error + Send + Sync + 'static,
{
    match error {
        error @ InitServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ InitServiceError::Workspace(_) => match &error {
            InitServiceError::Workspace(source) => match workspace_impact(source) {
                ProjectWorkspaceConvergenceFailureImpact::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::StateAppliedButFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::OutcomeUnknown => {
                    ProductionCommandError::OutcomeUnknown(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::Internal => {
                    ProductionCommandError::Internal(Box::new(error))
                }
            },
            _ => unreachable!("当前分支已经确认是工作区收敛错误"),
        },
    }
}

fn map_extract_error<OE, BE, RE, LE, PE>(
    error: ExtractServiceError<OE, BE, RE, LE, PE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    BE: Error + Send + Sync + 'static,
    RE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ ExtractServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ (ExtractServiceError::OpenProject(_)
        | ExtractServiceError::BuiltIn(_)
        | ExtractServiceError::Rules { .. }
        | ExtractServiceError::Lua { .. }) => ProductionCommandError::ProjectState(Box::new(error)),
    }
}

fn map_translate_error<RE, BE, SE, LE, PE>(
    error: TranslateServiceError<RE, BE, SE, LE, PE>,
    build_impact: impl FnOnce(&BE) -> TranslationExecutionBuildFailureImpact,
    standard_impact: impl FnOnce(&SE) -> StandardTranslationFailureImpact,
) -> ProductionCommandError
where
    RE: Error + Send + Sync + 'static,
    BE: Error + Send + Sync + 'static,
    SE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ TranslateServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ TranslateServiceError::ReadProject { .. } => {
            ProductionCommandError::ProjectState(Box::new(error))
        }
        TranslateServiceError::BuildExecution(source) => match build_impact(&source) {
            TranslationExecutionBuildFailureImpact::ConfigurationOrInput => {
                ProductionCommandError::ConfigurationOrInput(Box::new(source))
            }
            TranslationExecutionBuildFailureImpact::Internal => {
                ProductionCommandError::Internal(Box::new(source))
            }
        },
        error @ TranslateServiceError::Standard { .. } => match &error {
            TranslateServiceError::Standard { source } => match standard_impact(source) {
                StandardTranslationFailureImpact::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(error))
                }
                StandardTranslationFailureImpact::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(error))
                }
                StandardTranslationFailureImpact::ExternalModel => {
                    ProductionCommandError::ExternalModel(Box::new(error))
                }
                StandardTranslationFailureImpact::StateAppliedButFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
                }
            },
            _ => unreachable!("当前分支已经确认是 Standard 翻译错误"),
        },
        error @ (TranslateServiceError::MissingResolvedTranslationSemantics
        | TranslateServiceError::Lua { .. }) => ProductionCommandError::Internal(Box::new(error)),
    }
}

fn map_write_back_error<OE, SE, PE, LE, KE>(
    error: WriteBackServiceError<OE, SE, PE, LE, KE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    SE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    KE: Error + Send + Sync + 'static,
{
    match error.failure_impact() {
        WriteBackFailureImpact::ProjectUnavailable => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        WriteBackFailureImpact::ProjectState => {
            ProductionCommandError::ProjectState(Box::new(error))
        }
        WriteBackFailureImpact::StateAppliedButFinalizationFailed => {
            ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
        }
        WriteBackFailureImpact::OutcomeUnknown => {
            ProductionCommandError::OutcomeUnknown(Box::new(error))
        }
        WriteBackFailureImpact::Internal => ProductionCommandError::Internal(Box::new(error)),
    }
}

fn project_log_outcome<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
) -> ProjectLogRunOutcome {
    match execution {
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_))) if shutdown.is_empty() => {
            ProjectLogRunOutcome::Succeeded
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_))) => {
            ProjectLogRunOutcome::Failed
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(_))
            if shutdown.is_empty() =>
        {
            ProjectLogRunOutcome::Cancelled
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(_)) => ProjectLogRunOutcome::Failed,
        DrivenCommand::Finished(Err(
            ProductionCommandError::OutcomeUnknown(_)
            | ProductionCommandError::RunPlanOutcomeUnknown(_),
        ))
        | DrivenCommand::Interrupted(Err(
            ProductionCommandError::OutcomeUnknown(_)
            | ProductionCommandError::RunPlanOutcomeUnknown(_),
        )) => ProjectLogRunOutcome::OutcomeUnknown,
        DrivenCommand::Finished(Err(_)) | DrivenCommand::Interrupted(Err(_)) => {
            ProjectLogRunOutcome::Failed
        }
        DrivenCommand::SignalFailed {
            result:
                Err(
                    ProductionCommandError::OutcomeUnknown(_)
                    | ProductionCommandError::RunPlanOutcomeUnknown(_),
                ),
            ..
        } => ProjectLogRunOutcome::OutcomeUnknown,
        DrivenCommand::SignalFailed { .. } => ProjectLogRunOutcome::Failed,
    }
}

#[derive(Debug)]
pub(crate) enum ProductionCommandError {
    ConfigurationOrInput(BoxedError),
    ProjectUnavailable(BoxedError),
    ProjectState(BoxedError),
    ExternalModel(BoxedError),
    ResultAppliedButRunPlanNotSaved(BoxedError),
    RunPlanOutcomeUnknown(BoxedError),
    StateAppliedButFinalizationFailed(BoxedError),
    OutcomeUnknown(BoxedError),
    Internal(BoxedError),
    Signal {
        source: io::Error,
        outcome: SignalOutcome,
    },
}

#[derive(Debug)]
pub(crate) enum SignalOutcome {
    CompletedStateApplied,
    Cancelled,
    CommandFailed(Box<ProductionCommandError>),
}

impl ProductionCommandError {
    fn construct(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Internal(Box::new(source))
    }
}

impl fmt::Display for ProductionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ConfigurationOrInput(_) => "配置或输入错误",
            Self::ProjectUnavailable(_) => "项目不存在或正忙",
            Self::ProjectState(_) => "项目状态损坏或提取过期",
            Self::ExternalModel(_) => "外部模型不可用",
            Self::ResultAppliedButRunPlanNotSaved(_) => "结果已生效、运行方案未保存",
            Self::RunPlanOutcomeUnknown(_) => "结果已生效、运行方案状态无法确认",
            Self::StateAppliedButFinalizationFailed(_) => "状态已生效但收尾失败",
            Self::OutcomeUnknown(_) => "结果未知、必须保留现场",
            Self::Internal(_) => "内部技术故障",
            Self::Signal { outcome, .. } => match outcome {
                SignalOutcome::CompletedStateApplied => "状态已生效但收尾失败",
                SignalOutcome::Cancelled => "内部技术故障",
                SignalOutcome::CommandFailed(command) => return command.fmt(formatter),
            },
        })
    }
}

impl Error for ProductionCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConfigurationOrInput(source)
            | Self::ProjectUnavailable(source)
            | Self::ProjectState(source)
            | Self::ExternalModel(source)
            | Self::ResultAppliedButRunPlanNotSaved(source)
            | Self::RunPlanOutcomeUnknown(source)
            | Self::StateAppliedButFinalizationFailed(source)
            | Self::OutcomeUnknown(source)
            | Self::Internal(source) => Some(source.as_ref()),
            Self::Signal { source, outcome } => match outcome {
                SignalOutcome::CommandFailed(command) => Some(command.as_ref()),
                SignalOutcome::CompletedStateApplied | SignalOutcome::Cancelled => Some(source),
            },
        }
    }
}

#[derive(Default)]
pub(crate) struct ShutdownFailures {
    failures: Vec<ShutdownFailure>,
}

impl ShutdownFailures {
    fn push(&mut self, component: &'static str, source: impl Error + Send + Sync + 'static) {
        self.failures.push(ShutdownFailure {
            component,
            source: Box::new(source),
        });
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("内部技术故障")
    }
}

impl fmt::Debug for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.failures.iter().map(|failure| failure.component))
            .finish()
    }
}

impl Error for ShutdownFailures {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures
            .first()
            .map(|failure| failure.source.as_ref() as _)
    }
}

struct ShutdownFailure {
    component: &'static str,
    source: BoxedError,
}

impl fmt::Debug for ShutdownFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownFailure")
            .field("component", &self.component)
            .field("source", &self.source)
            .finish()
    }
}

/// 在命令和全部 shutdown 都成功后呈现最终业务结果。
pub(crate) struct CommandResultRenderer;

impl CommandResultRenderer {
    pub(crate) fn render_success(
        output: RpgMakerCommandOutput,
        localizer: &UiLocalizer,
        stdout: &mut dyn Write,
    ) -> io::Result<()> {
        match output {
            RpgMakerCommandOutput::Init {
                output,
                plan_source,
                reused_path,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultInitCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                match output.outcome {
                    InitOutcome::Created => {
                        writeln!(stdout, "{}", localizer.format(UiMessage::ResultInitCreated))?
                    }
                    InitOutcome::Unchanged => writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::ResultInitUnchanged)
                    )?,
                    InitOutcome::Updated { stale_owners } => {
                        writeln!(stdout, "{}", localizer.format(UiMessage::ResultInitUpdated))?;
                        if !stale_owners.is_empty() {
                            let owners = stale_owners
                                .into_iter()
                                .map(|owner| match owner {
                                    InitStaleOwner::Builtin => "Builtin",
                                    InitStaleOwner::Rules => "Rules",
                                    InitStaleOwner::Lua => "Lua",
                                })
                                .collect::<Vec<_>>()
                                .join("、");
                            writeln!(
                                stdout,
                                "{}",
                                localizer
                                    .format(UiMessage::ResultInitStaleOwners { owners: &owners })
                            )?;
                        }
                    }
                };
                if let Some(path) = reused_path {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeInitReusePath {
                            path: &path.to_string_lossy(),
                        })
                    )?;
                }
                render_saved_plan_source(localizer, plan_source, stdout)
            }
            RpgMakerCommandOutput::Extract {
                output,
                plan_source,
                owners,
                disabled_owners,
                has_saved_plan,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultExtractCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                if plan_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeExtractReuseOwners {
                            owners: &owners.join(", "),
                        })
                    )?;
                }
                for owner in disabled_owners {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeOwnerDisabled { owner })
                    )?;
                }
                if has_saved_plan {
                    render_saved_plan_source(localizer, plan_source, stdout)
                } else {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::ErrorNoExecutableExtractOwner)
                    )
                }
            }
            RpgMakerCommandOutput::Translate {
                output,
                profile_source,
                lua_source,
                lua_cleared,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateCompleted {
                        project: output.name.as_str(),
                        profile: &output.profile_id,
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateStandard {
                        total: u64::try_from(output.standard.total_tasks).unwrap_or(u64::MAX),
                        complete: u64::try_from(output.standard.complete_tasks).unwrap_or(u64::MAX),
                        partial: u64::try_from(output.standard.partial_tasks).unwrap_or(u64::MAX),
                        unavailable: u64::try_from(output.standard.unavailable_tasks)
                            .unwrap_or(u64::MAX),
                        written: u64::try_from(output.standard.written_locations)
                            .unwrap_or(u64::MAX),
                        remaining: u64::try_from(output.standard.remaining_locations)
                            .unwrap_or(u64::MAX),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateConvergence {
                        retained: u64::try_from(output.standard.retained).unwrap_or(u64::MAX),
                        invalidated: u64::try_from(output.standard.invalidated).unwrap_or(u64::MAX),
                        not_applicable: u64::try_from(output.standard.not_applicable)
                            .unwrap_or(u64::MAX),
                        reused: u64::try_from(output.standard.reused).unwrap_or(u64::MAX),
                    })
                )?;
                if output.standard.total_tasks == 0 {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeNoModelRequest)
                    )?;
                }
                if output.lua_executed {
                    writeln!(stdout, "{}", localizer.format(UiMessage::ResultLuaExecuted))?;
                }
                if profile_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeTranslateReuseProfile {
                            profile: &output.profile_id,
                        })
                    )?;
                }
                if lua_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeTranslateReuseLua)
                    )?;
                }
                if lua_cleared {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeLuaCleared { phase: "Translate" })
                    )?;
                }
                render_saved_translate_plan_sources(localizer, profile_source, lua_source, stdout)
            }
            RpgMakerCommandOutput::WriteBack {
                output,
                plan_source,
                lua_cleared,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultWriteBackCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultOutputDirectory {
                        path: &output.output_root.to_string_lossy(),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultWriteBackStandard {
                        translated: u64::try_from(output.standard.translated_units)
                            .unwrap_or(u64::MAX),
                        original: u64::try_from(output.standard.original_units).unwrap_or(u64::MAX),
                        auto_wrapped: u64::try_from(output.standard.auto_wrapped_units)
                            .unwrap_or(u64::MAX),
                        breaks: u64::try_from(output.standard.inserted_line_breaks)
                            .unwrap_or(u64::MAX),
                        indents: u64::try_from(output.standard.inserted_fullwidth_indents)
                            .unwrap_or(u64::MAX),
                        manual: u64::try_from(output.standard.manual_layout_units)
                            .unwrap_or(u64::MAX),
                    })
                )?;
                if output.standard.manual_layout_units > 0 {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeManualLayout {
                            count: u64::try_from(output.standard.manual_layout_units)
                                .unwrap_or(u64::MAX),
                        })
                    )?;
                }
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(if output.lua_executed {
                        UiMessage::ResultLuaExecuted
                    } else {
                        UiMessage::ResultLuaNotExecuted
                    })
                )?;
                if plan_source == ProjectLogValueSource::ProjectState && output.lua_executed {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeWriteBackReuseLua)
                    )?;
                } else if plan_source == ProjectLogValueSource::ProductDefault {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeWriteBackStandardOnly)
                    )?;
                }
                if lua_cleared {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeLuaCleared { phase: "WriteBack" })
                    )?;
                }
                render_saved_plan_source(localizer, plan_source, stdout)
            }
        }
    }

    pub(crate) fn render_failure(
        command_error: Option<&ProductionCommandError>,
        shutdown_error: Option<&dyn fmt::Display>,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(error) = command_error {
            render_command_failure(error, localizer, stderr)?;
        }
        if shutdown_error.is_some() {
            writeln!(stderr, "{}", localizer.format(UiMessage::ErrorShutdown))?;
        }
        Ok(())
    }

    pub(crate) fn render_applied_finalization_failure(
        shutdown_error: &dyn fmt::Display,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        let _ = shutdown_error;
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::ErrorStateAppliedFinalization)
        )
    }
}

fn render_command_failure(
    error: &ProductionCommandError,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let message = match error {
        ProductionCommandError::ConfigurationOrInput(source) => {
            if let Some(error) = source.downcast_ref::<RunPlanResolutionError>() {
                let message = match error {
                    RunPlanResolutionError::InitPathRequired => UiMessage::ErrorInitPathRequired,
                    RunPlanResolutionError::NoReusableExtractPlan => {
                        UiMessage::ErrorNoReusableExtractPlan
                    }
                    RunPlanResolutionError::ProfileRequired => UiMessage::ErrorProfileRequired,
                    RunPlanResolutionError::SavedProfileUnavailable { profile_id } => {
                        UiMessage::ErrorSavedProfileUnavailable {
                            profile: profile_id,
                        }
                    }
                };
                localizer.format(message)
            } else if let Some(error) =
                source.downcast_ref::<ProductionTranslationExecutionBuildError>()
            {
                match error.diagnostic() {
                    TranslationExecutionBuildDiagnostic::PromptUnavailable {
                        source_language,
                        target_language,
                        path,
                    } => localizer.format(UiMessage::ErrorRpgMakerPromptUnavailable {
                        source_language,
                        target_language,
                        path: &path.to_string_lossy(),
                    }),
                    TranslationExecutionBuildDiagnostic::LanguageModuleUnavailable {
                        source_language,
                        target_language,
                    } => localizer.format(UiMessage::ErrorRpgMakerLanguageModuleUnavailable {
                        source_language,
                        target_language,
                    }),
                    TranslationExecutionBuildDiagnostic::Internal => {
                        localizer.format(UiMessage::ErrorInternal)
                    }
                }
            } else {
                localizer.format(UiMessage::ErrorConfigurationOrInputGeneric)
            }
        }
        ProductionCommandError::ProjectUnavailable(_) => {
            localizer.format(UiMessage::ErrorProjectUnavailable)
        }
        ProductionCommandError::ProjectState(_) => localizer.format(UiMessage::ErrorProjectState),
        ProductionCommandError::ExternalModel(_) => localizer.format(UiMessage::ErrorExternalModel),
        ProductionCommandError::ResultAppliedButRunPlanNotSaved(_) => {
            localizer.format(UiMessage::ErrorPlanSaveFailedApplied)
        }
        ProductionCommandError::RunPlanOutcomeUnknown(_) => {
            localizer.format(UiMessage::ErrorPlanSaveOutcomeUnknown)
        }
        ProductionCommandError::StateAppliedButFinalizationFailed(_) => {
            localizer.format(UiMessage::ErrorStateAppliedFinalization)
        }
        ProductionCommandError::OutcomeUnknown(_) => {
            localizer.format(UiMessage::ErrorOutcomeUnknown)
        }
        ProductionCommandError::Internal(_) => localizer.format(UiMessage::ErrorInternal),
        ProductionCommandError::Signal { outcome, .. } => match outcome {
            SignalOutcome::CompletedStateApplied => {
                localizer.format(UiMessage::ErrorStateAppliedFinalization)
            }
            SignalOutcome::Cancelled => localizer.format(UiMessage::ErrorInternal),
            SignalOutcome::CommandFailed(command) => {
                return render_command_failure(command, localizer, stderr);
            }
        },
    };
    writeln!(stderr, "{message}")
}

fn render_saved_plan_source(
    localizer: &UiLocalizer,
    source: ProjectLogValueSource,
    stdout: &mut dyn Write,
) -> io::Result<()> {
    let source = plan_source_message(source);
    writeln!(
        stdout,
        "{} ({})",
        localizer.format(UiMessage::ResultPlanSaved),
        localizer.format(source),
    )
}

fn render_saved_translate_plan_sources(
    localizer: &UiLocalizer,
    profile_source: ProjectLogValueSource,
    lua_source: ProjectLogValueSource,
    stdout: &mut dyn Write,
) -> io::Result<()> {
    let profile_source = localizer.format(plan_source_message(profile_source));
    let lua_source = localizer.format(plan_source_message(lua_source));
    writeln!(
        stdout,
        "{}",
        localizer.format(UiMessage::ResultTranslatePlanSources {
            profile_source: &profile_source,
            lua_source: &lua_source,
        })
    )
}

fn plan_source_message(source: ProjectLogValueSource) -> UiMessage<'static> {
    project_log_value_source_label(match source {
        ProjectLogValueSource::Explicit => "explicit",
        ProjectLogValueSource::ProjectState => "project_state",
        ProjectLogValueSource::ProductDefault => "product_default",
    })
    .expect("每个运行方案来源代码都必须具有本地化日志标签")
}

fn localized_plan_resolution(
    localizer: &UiLocalizer,
    command: &'static str,
    source: ProjectLogValueSource,
) -> String {
    let source = localizer.format(plan_source_message(source));
    localizer.format(UiMessage::LogPlanResolved {
        command,
        source: &source,
    })
}

fn localized_translate_plan_resolution(
    localizer: &UiLocalizer,
    profile_source: ProjectLogValueSource,
    lua_source: ProjectLogValueSource,
) -> String {
    let profile_source = localizer.format(plan_source_message(profile_source));
    let lua_source = localizer.format(plan_source_message(lua_source));
    localizer.format(UiMessage::LogTranslatePlanResolved {
        profile_source: &profile_source,
        lua_source: &lua_source,
    })
}

#[cfg(test)]
mod command_error_rendering_tests {
    use super::*;

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    #[test]
    fn configuration_or_input_failure_uses_localized_generic_guidance() {
        let error = ProductionCommandError::ConfigurationOrInput(Box::new(TestError(
            "RPG Maker system prompt 文件 prompts/rpg_maker/ja--zh-Hans.md 不存在",
        )));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");

        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        assert!(stderr.contains("配置或命令输入无效"));
        assert!(!stderr.contains("prompts/rpg_maker/ja--zh-Hans.md"));
    }

    #[test]
    fn prompt_build_failure_renders_actionable_localized_facts_without_source_detail() {
        let build = ProductionTranslationExecutionBuildError {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            diagnostic: TranslationExecutionBuildDiagnostic::PromptUnavailable {
                source_language: "ja".to_owned(),
                target_language: "zh-Hans".to_owned(),
                path: PathBuf::from("prompts/rpg_maker/ja--zh-Hans.md"),
            },
            source: Box::new(TestError("PROMPT_CONTENT_SENTINEL")),
        };
        let error = ProductionCommandError::ConfigurationOrInput(Box::new(build));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert!(stderr.contains("ja--zh-Hans.md"));
        assert!(stderr.contains("翻译尚未开始"));
        assert!(stderr.contains("非空 UTF-8"));
        assert!(!stderr.contains("PROMPT_CONTENT_SENTINEL"));
    }

    #[test]
    fn language_module_build_failure_is_localized_in_selected_ui_language() {
        let build = ProductionTranslationExecutionBuildError {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            diagnostic: TranslationExecutionBuildDiagnostic::LanguageModuleUnavailable {
                source_language: "ja".to_owned(),
                target_language: "zh-Hans".to_owned(),
            },
            source: Box::new(TestError("LANGUAGE_MODULE_SENTINEL")),
        };
        let error = ProductionCommandError::ConfigurationOrInput(Box::new(build));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::English);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert!(stderr.contains("source-language module"));
        assert!(stderr.contains("translation has not started"));
        assert!(!stderr.contains("LANGUAGE_MODULE_SENTINEL"));
        assert!(!stderr.contains("源语言模块"));
    }

    #[test]
    fn run_plan_log_fact_preserves_each_transaction_terminal_state() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let path = PathBuf::from("project.db");
        let cases = [
            (
                ProjectRunPlanReplaceError::RollbackConfirmed {
                    path: path.clone(),
                    source: TestError("rollback"),
                },
                ProjectLogLevel::Warn,
                ProjectLogCode::RunPlanSaveFailed,
                "新运行方案未保存",
            ),
            (
                ProjectRunPlanReplaceError::OutcomeUnknown {
                    path: path.clone(),
                    source: TestError("unknown"),
                },
                ProjectLogLevel::Error,
                ProjectLogCode::RunPlanSaveOutcomeUnknown,
                "无法确认运行方案提交结果",
            ),
            (
                ProjectRunPlanReplaceError::CommittedButFinalizationFailed {
                    path,
                    source: TestError("finalization"),
                },
                ProjectLogLevel::Error,
                ProjectLogCode::RunPlanSavedFinalizationFailed,
                "已保存本次成功运行方案",
            ),
        ];

        for (error, expected_level, expected_code, expected_message) in cases {
            let (level, code, message) = run_plan_replace_log_fact(&error, &localizer);
            assert_eq!(level, expected_level);
            assert_eq!(code, expected_code);
            assert!(message.contains(expected_message), "{message}");
        }
    }

    #[test]
    fn signal_failure_preserves_nested_user_repairable_category() {
        let error = ProductionCommandError::Signal {
            source: io::Error::other("SIGNAL_SECRET_SENTINEL"),
            outcome: SignalOutcome::CommandFailed(Box::new(
                ProductionCommandError::ConfigurationOrInput(Box::new(TestError(
                    "语言对 ja -> zh-Hans 缺少 Prompt 资源",
                ))),
            )),
        };
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert!(stderr.contains("配置或命令输入无效"));
        assert!(!stderr.contains("语言对 ja -> zh-Hans 缺少 Prompt 资源"));
        assert!(!stderr.contains("SIGNAL_SECRET_SENTINEL"));
    }

    #[test]
    fn internal_failure_never_renders_its_source() {
        let error = ProductionCommandError::Internal(Box::new(TestError(
            "API_KEY_SENTINEL CLIENT_PARAMETERS_SENTINEL PROMPT_CONTENT_SENTINEL",
        )));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert_eq!(stderr, "ATT 遇到内部故障；终端没有输出密钥或模型内容。\n");
        assert!(!stderr.contains("API_KEY_SENTINEL"));
        assert!(!stderr.contains("CLIENT_PARAMETERS_SENTINEL"));
        assert!(!stderr.contains("PROMPT_CONTENT_SENTINEL"));
    }

    #[test]
    fn internal_translation_build_failure_is_not_mapped_to_user_input() {
        let build = ProductionTranslationExecutionBuildError::internal(TestError(
            "CLIENT_PARAMETERS_SENTINEL",
        ));
        let error = TranslateServiceError::<
            TestError,
            ProductionTranslationExecutionBuildError,
            TestError,
            TestError,
            TestError,
        >::BuildExecution(build);

        let mapped = map_translate_error(
            error,
            ProductionTranslationExecutionBuildError::failure_impact,
            |_| StandardTranslationFailureImpact::ConfigurationOrInput,
        );
        let mut stderr = Vec::new();
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&mapped), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert_eq!(stderr, "ATT 遇到内部故障；终端没有输出密钥或模型内容。\n");
        assert!(!stderr.contains("CLIENT_PARAMETERS_SENTINEL"));
    }
}
