//! 生产命令装配与最终结果呈现。

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;

use crate::application::config::{
    ConfigurationLoadError, ConfiguredExtractCommand, ConfiguredInitCommand,
    ConfiguredRpgMakerCommand, ConfiguredTranslateCommand, ConfiguredWriteBackCommand,
    SelectedLuaConfiguration, TranslateConfiguration,
};
use crate::diagnostic::{
    BoxedError, DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact,
    DiagnosticReason, DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact,
    ReportedFailure, SafeDiagnostic, SafeDiagnosticSource, render_failure_report,
    render_safe_diagnostic,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage, project_log_value_source_label};
use crate::language::LanguageModuleCatalogError;
use crate::progress::{
    ProgressAmount, ProgressMode, ProgressObserver, ProgressSnapshot, TerminalProgress,
    TerminalProgressObserver,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::SelectedLua;
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
};
use crate::rpg_maker::extract::builtin::{
    BuiltInExtractionError, BuiltInExtractionService, MvDialogueDefinitionSelection,
};
use crate::rpg_maker::extract::document::{
    CommandScopedRpgMakerDocumentReader, RpgMakerProjectDocumentReadingService,
};
use crate::rpg_maker::extract::lua::{LuaExtractionError, LuaExtractionService};
use crate::rpg_maker::extract::rules::{
    RulesExtractionError, RulesExtractionService, RulesProgram,
};
use crate::rpg_maker::extract::service::ExtractService;
use crate::rpg_maker::extract::service::ExtractServiceError;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStore;
use crate::rpg_maker::extract::{ExtractInput, ExtractOutput, ExtractProgressPhase, SelectedRules};
use crate::rpg_maker::init::{
    InitInput, InitOutcome, InitOutput, InitProgressPhase, InitService, InitServiceError,
    InitStaleOwner, MissingInitialProjectSetting, ProjectWorkspaceCandidateFailure,
    ProjectWorkspaceConvergenceError, ProjectWorkspaceConvergenceService,
};
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingService;
use crate::rpg_maker::lua::lua54::TrustedLua54Runtime;
use crate::rpg_maker::lua::runtime::OwnedLuaProgram;
use crate::rpg_maker::project::{
    ExistingProjectOpener, ExistingProjectOpeningError, ExistingProjectOpeningService,
    OpenedProject,
};
use crate::rpg_maker::project_database::{
    ExtractRulesCanonicalJson, ExtractRunPlan, FinalProjectRunPlanPersistenceService, InitRunPlan,
    InvalidRunPlanValue, LuaProgramSnapshot, ProjectDatabaseCreateError,
    ProjectDatabaseCreationService, ProjectDatabaseInspectionError, ProjectDatabaseReadError,
    ProjectDatabaseReconciliationError, ProjectDatabaseRecordReadingService,
    ProjectDatabaseStateReconciliationService, ProjectRunPlanFinalizer,
    ProjectRunPlanPersistenceService, ProjectRunPlanReadError, ProjectRunPlanReplaceError,
    ProjectRunPlanReplacement, ProjectRunPlanRepository, ProjectWorkspaceLayout, TranslateRunPlan,
    WriteBackRunPlan,
};
use crate::rpg_maker::project_lease::{
    AlreadyHeldProjectCommandLeaseProvider, ProjectCommandLease, ProjectCommandLeaseError,
    ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::rpg_maker::translate::TranslateInput;
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::translate::asset_reader::RpgMakerStandardTranslationAssetReadingService;
use crate::rpg_maker::translate::executor::{
    AsyncDelay, RpgMakerStandardTranslationTaskExecutionError,
    RpgMakerStandardTranslationTaskExecutionService, TranslationTaskResponseProcessingService,
};
use crate::rpg_maker::translate::lua::{LuaTranslationError, LuaTranslationService};
use crate::rpg_maker::translate::placeholder::{
    Pcre2PlaceholderConstructionError, Pcre2PlaceholderService,
};
use crate::rpg_maker::translate::planner::RpgMakerStandardTranslationTaskPlanningService;
use crate::rpg_maker::translate::planning_resource::TranslationPlanningResourceReadingService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt, RpgMakerSystemPromptError,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
    TranslationResponseEnvelope,
};
use crate::rpg_maker::translate::result_store::{
    RpgMakerStandardTranslationResultStorageError, RpgMakerStandardTranslationResultStorageService,
};
use crate::rpg_maker::translate::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder, TranslateService,
    TranslateServiceError,
};
use crate::rpg_maker::translate::standard::{
    StandardTranslationLog, StandardTranslationLogEvent, StandardTranslationLogTaskOutcome,
    StandardTranslationService,
};
use crate::rpg_maker::write_back::asset_reader::RpgMakerStandardWriteBackAssetReadingService;
use crate::rpg_maker::write_back::lua::LuaWriteBackService;
use crate::rpg_maker::write_back::publisher::StandardWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingService;
use crate::rpg_maker::write_back::standard::{
    ConservativeRpgMakerWriteBackTextLayouter, StandardWriteBackService,
};
use crate::rpg_maker::write_back::{
    WriteBackInput, WriteBackLog, WriteBackLogEvent, WriteBackLogPublicationOutcome,
    WriteBackLuaDiagnostic, WriteBackOutput, WriteBackProgressPhase, WriteBackPublishFailureState,
    WriteBackPublishingDiagnostic, WriteBackService, WriteBackServiceError,
};
use crate::runtime::cpu::{CpuExecutorStartError, RayonCpuExecutor};
use crate::runtime::filesystem::{
    SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemError,
};
use crate::runtime::llm::{
    OpenAiChatCompletionClient, OpenAiChatCompletionError, OpenAiChatCompletionExecutor,
    OpenAiExecutorBuildError,
};
use crate::runtime::llm_call_log::LlmCallRecorder;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLog, ProjectLogAmount, ProjectLogCode, ProjectLogContext, ProjectLogEvent,
    ProjectLogLevel, ProjectLogNoWorkReason, ProjectLogPayload, ProjectLogPhase,
    ProjectLogRunOutcome, ProjectLogRuntime, ProjectLogValueSource, ProjectLogWarning,
    ProjectLogger, start_project_log,
};
use crate::runtime::run_id::generate_run_id;
use crate::runtime::sqlite::{
    RusqliteFinalTransactionExecutor, RusqliteStorage, SqliteRuntimeError,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError,
    DirectoryStageRequestError, DirectoryTreeFingerprintError, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFileError, ResolveDirectoryError, StagingCleanupFailure,
};
use crate::storage::sqlite::SnapshotDatabaseError;

const RPG_MAKER_PROMPT_DIRECTORY_NAME: &str = "rpg_maker";
const SYSTEM_PROMPT_FILE_NAME: &str = "system.md";
const THINKING_PROMPT_FILE_NAME: &str = "thinking.md";
const SOURCE_LANGUAGE_TEMPLATE_VARIABLE: &str = "{{source_language}}";
const TARGET_LANGUAGE_TEMPLATE_VARIABLE: &str = "{{target_language}}";

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

#[derive(Clone)]
struct PreopenedProject {
    project: OpenedProject,
}

type ProductionProjectOpeningError = ExistingProjectOpeningError<
    ProjectDatabaseReadError<SqliteRuntimeError>,
    SystemFileSystemError,
    Box<SystemFileSystemError>,
>;

type ProductionWorkspaceConvergenceError = ProjectWorkspaceConvergenceError<
    ProjectDatabaseCreateError<SqliteRuntimeError>,
    SqliteRuntimeError,
    ProjectDatabaseInspectionError<SqliteRuntimeError>,
    ProjectDatabaseReconciliationError<SqliteRuntimeError, SqliteRuntimeError>,
    SystemFileSystemError,
    Box<SystemFileSystemError>,
    Box<SystemFileSystemError>,
>;

impl PreopenedProject {
    fn new(project: OpenedProject) -> Self {
        Self { project }
    }
}

impl ExistingProjectOpener for PreopenedProject {
    type Error = Infallible;

    async fn open(
        &self,
        name: &crate::rpg_maker::ProjectName,
    ) -> Result<OpenedProject, Self::Error> {
        debug_assert_eq!(self.project.name(), name);
        Ok(self.project.clone())
    }
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

#[derive(Clone, Default)]
pub(crate) struct CommandPanicBoundary {
    state: Arc<Mutex<Option<CommandPanicContext>>>,
}

#[derive(Clone)]
struct CommandPanicContext {
    diagnostics: Vec<SafeDiagnostic>,
    logger: Option<ProjectLogger>,
}

impl CommandPanicBoundary {
    pub(crate) fn from_logged(diagnostics: Vec<SafeDiagnostic>, logger: ProjectLogger) -> Self {
        let boundary = Self::default();
        boundary.register_project_log(diagnostics, logger);
        boundary
    }

    fn prepare(&self, command: &'static str, stage: DiagnosticStage, project_workspace: &Path) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CommandPanicContext {
            diagnostics: vec![command_panic_diagnostic(
                command,
                stage,
                project_workspace,
                None,
            )],
            logger: None,
        });
    }

    fn register_project_log(&self, diagnostics: Vec<SafeDiagnostic>, logger: ProjectLogger) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CommandPanicContext {
            diagnostics,
            logger: Some(logger),
        });
    }

    pub(crate) fn panic_error(&self) -> ProductionCommandError {
        let context = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| CommandPanicContext {
                diagnostics: vec![SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    DiagnosticStage::CommandPreparation,
                    DiagnosticSubject::operation("run_command"),
                    DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                    DiagnosticImpact::OutcomeUnknown,
                    DiagnosticAction::ReportBug,
                )],
                logger: None,
            });
        let mut diagnostics = context.diagnostics.into_iter();
        let primary = diagnostics.next().unwrap_or_else(|| {
            SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::operation("run_command"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::ReportBug,
            )
        });
        let mut report =
            FailureReport::new(ReportedFailure::new(primary, ApplicationScopePanicked));
        for diagnostic in diagnostics {
            report = report.with_related(ReportedFailure::new(
                diagnostic,
                RelatedFailurePreservedDuringPanic,
            ));
        }
        if let Some(diagnostic) = context
            .logger
            .and_then(|logger| logger.take_warning())
            .and_then(|warning| warning.diagnostic)
        {
            report = report.with_related(ReportedFailure::new(
                diagnostic,
                ProjectLogDegradedWhileReportingPanic,
            ));
        }
        ProductionCommandError::Internal(Box::new(report))
    }
}

#[derive(Debug)]
struct ApplicationScopePanicked;

impl fmt::Display for ApplicationScopePanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("application scope panicked")
    }
}

impl Error for ApplicationScopePanicked {}

#[derive(Debug)]
struct RelatedFailurePreservedDuringPanic;

impl fmt::Display for RelatedFailurePreservedDuringPanic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a related safe failure was preserved while reporting a panic")
    }
}

impl Error for RelatedFailurePreservedDuringPanic {}

#[derive(Debug)]
struct ProjectLogDegradedWhileReportingPanic;

impl fmt::Display for ProjectLogDegradedWhileReportingPanic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("project logging degraded while reporting a command panic")
    }
}

impl Error for ProjectLogDegradedWhileReportingPanic {}

/// 按本次命令只构造实际需要的 RPG Maker 生产纵向切片。
pub(crate) struct ProductionRpgMakerCommandRunner {
    layout: RpgMakerLayout,
    locale: UiLocale,
    progress_mode: ProgressMode,
    panic_boundary: CommandPanicBoundary,
}

impl ProductionRpgMakerCommandRunner {
    pub(crate) fn new(
        layout: RpgMakerLayout,
        locale: UiLocale,
        progress_mode: ProgressMode,
    ) -> Self {
        Self {
            layout,
            locale,
            progress_mode,
            panic_boundary: CommandPanicBoundary::default(),
        }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredRpgMakerCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let (command_name, stage, project_workspace) = command_panic_context(self.layout, &command);
        self.panic_boundary
            .prepare(command_name, stage, &project_workspace);
        let panic_boundary = self.panic_boundary.clone();
        catch_command_panic(panic_boundary, async move {
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
        })
        .await
    }

    async fn run_init(
        self,
        command: ConfiguredInitCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = init_terminal_progress(self.progress_mode, self.locale);
        let progress_observer =
            ProductionProgressObserver::without_project_log(progress.observer(), init_phase_code);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let sqlite = match RusqliteStorage::start_with_performance(
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::sqlite_start)
        {
            Ok(sqlite) => sqlite,
            Err(error) => return ProductionCommandRunReport::failed_before_logging(error),
        };
        let file_system = match SystemFileSystem::new_with_performance(
            command.common().filesystem().clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::file_system_build)
        {
            Ok(file_system) => file_system,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
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
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
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
                            return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                                ProductionCommandError::run_plan_resolution(
                                    RunPlanResolutionError::InitPathRequired,
                                ),
                                shutdown,
                            );
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
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::run_plan_resolution(
                                RunPlanResolutionError::InitPathRequired,
                            ),
                            shutdown,
                        );
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
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::project_run_plan_read(error),
                            shutdown,
                        );
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
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::input_directory(error),
                    shutdown,
                );
            }
        };
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
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                progress.safe_stopping(safe_stopping);
            },
        )
        .await
        .map(|result| result.map_err(map_init_error));
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
        let workspace_is_legal = matches!(
            &execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
                | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(_)))
                | DrivenCommand::SignalFailed {
                    result: Ok(OperationCompletion::Completed(_)),
                    ..
                }
        );
        if !workspace_is_legal {
            drop(project_lease_guard);
            progress.finish();
            return ProductionCommandRunReport::from_completion_with_project_log(
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
                None,
            );
        }
        let project_log = match start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            layout: self.layout,
            project: command.arguments.project.name.as_str(),
            command: "init",
            stage: DiagnosticStage::Init,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: &self.panic_boundary,
        }) {
            Ok(log) => log,
            Err(error) => {
                drop(project_lease_guard);
                progress.finish();
                let report = error
                    .into_failure_report()
                    .with_primary_impact(DiagnosticImpact::StateAppliedFinalizationFailed);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report)),
                    shutdown,
                );
            }
        };
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            ProjectLogPayload::RunPlan {
                source: plan_source,
                lua_source: None,
                selections: vec![resolved_game_root.to_string_lossy().into_owned()],
                lua_enabled: None,
            },
        ));
        let replacement = InitRunPlan::new(resolved_game_root)
            .map(ProjectRunPlanReplacement::Init)
            .map_err(ProductionCommandError::invalid_run_plan);
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            progress.finalizing(progress_finalizing(self.locale));
        }
        execution = match replacement {
            Ok(replacement) => {
                if business_completed(&execution) && shutdown.is_empty() {
                    progress.finalizing(progress_saving_plan(self.locale));
                }
                finalize_run_plan(
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
                        progress.safe_stopping(progress_safe_stopping(self.locale));
                        let (confirmed, total) = progress_observer.confirmed_amount();
                        project_log.emit_cancellation(
                            ProjectLogCode::CancellationRequested,
                            confirmed,
                            total,
                        );
                    },
                )
                .await
            }
            Err(error) => replace_success_with_plan_error(execution, Err(error)),
        };
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let failure_diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
        let pending_project_log =
            PendingProjectLog::new(project_log, log_outcome, failure_diagnostics);
        ProductionCommandRunReport::from_completion_with_project_log(
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
            Some(pending_project_log),
        )
    }

    async fn run_extract(
        self,
        command: ConfiguredExtractCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = extract_terminal_progress(self.progress_mode, self.locale);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let sqlite = match RusqliteStorage::start_with_performance(
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::sqlite_start)
        {
            Ok(value) => value,
            Err(error) => return ProductionCommandRunReport::failed_before_logging(error),
        };
        let file_system = match SystemFileSystem::new_with_performance(
            command.common().filesystem().clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::file_system_build)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
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
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || {
                progress.safe_stopping(progress_safe_stopping(self.locale));
            },
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
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
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::run_plan_resolution(
                                RunPlanResolutionError::NoReusableExtractPlan,
                            ),
                            shutdown,
                        );
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
                    return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        ProductionCommandError::run_plan_resolution(
                            RunPlanResolutionError::NoReusableExtractPlan,
                        ),
                        shutdown,
                    );
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(project) => {
                        drop(project);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let project_log = match start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            layout: self.layout,
            project: command.project_name().as_str(),
            command: "extract",
            stage: DiagnosticStage::Extract,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: &self.panic_boundary,
        }) {
            Ok(log) => log,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
        };
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
                        let diagnostic = source.safe_diagnostic();
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
                                ProductionCommandError::report_diagnostic(source, diagnostic),
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
                        let diagnostic = source.safe_diagnostic();
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
                                ProductionCommandError::report_diagnostic(source, diagnostic),
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
                        let diagnostic = error.safe_diagnostic(&database_path);
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
                            ProductionCommandError::ProjectState(Box::new(
                                ProductionCommandError::report_diagnostic(error, diagnostic),
                            )),
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
                    let diagnostic = source.safe_diagnostic();
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
                            ProductionCommandError::report_diagnostic(source, diagnostic),
                        )),
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
                            ProductionCommandError::configuration_load(error),
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
                    ProductionCommandError::invalid_run_plan(error),
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
            .map_err(ProductionCommandError::cpu_start)
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
        let opener = PreopenedProject::new(opened_project);
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
        let selected_lua = lua.as_ref().map(|selected| {
                let host = TrustedLuaExecutionHostingService::<_, OpenAiChatCompletionExecutor, _, _>::without_llm(
                    file_system.clone(), selected.runtime.clone(), sqlite.clone(),
                );
                let store = RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone());
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
            termination_signals,
            || {
                cancellation.request();
                cpu.cancel_waits();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
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
                progress.safe_stopping(progress_safe_stopping(self.locale));
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await;
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let failure_diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
        let pending_project_log =
            PendingProjectLog::new(project_log, log_outcome, failure_diagnostics);
        ProductionCommandRunReport::from_completion_with_project_log(
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
            Some(pending_project_log),
        )
    }

    async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = translate_terminal_progress(self.progress_mode, self.locale);
        let explicit_profile = command.resolved_profile_id().map(str::to_owned);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let file_system = match SystemFileSystem::new_with_performance(
            command.common().filesystem().clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::file_system_build)
        {
            Ok(value) => value,
            Err(error) => return ProductionCommandRunReport::failed_before_logging(error),
        };
        let sqlite = match RusqliteStorage::start_with_performance(
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::sqlite_start)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let project_name = command.project_name().clone();
        let project_workspace =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name);
        let database_path = project_workspace.database_path().to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine(),
            file_system.clone(),
        );
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                progress.finish();
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
                let mut shutdown = ShutdownFailures::default();
                if let Err(shutdown_error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", shutdown_error);
                }
                if let Err(shutdown_error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", shutdown_error);
                }
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
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", error);
                    }
                    if let Err(error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", error);
                    }
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(project) => {
                        drop(project);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                drop(project_lease_guard);
                progress.finish();
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
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::run_plan_resolution(
                        RunPlanResolutionError::SavedProfileUnavailable { profile_id },
                    ),
                    shutdown,
                );
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
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::configuration_load(error),
                    shutdown,
                );
            }
        };
        let mut project_log = match start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            layout: self.layout,
            project: command.project_name().as_str(),
            command: "translate",
            stage: DiagnosticStage::Translate,
            profile: Some(&profile_id),
            performance: Arc::clone(&performance),
            panic_boundary: &self.panic_boundary,
        }) {
            Ok(log) => log,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
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
                    let diagnostic = error.safe_diagnostic();
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
                        ProductionCommandError::ConfigurationOrInput(Box::new(
                            ProductionCommandError::report_diagnostic(error, diagnostic),
                        )),
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
                                ProductionCommandError::configuration_load(error),
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
                        ProductionCommandError::invalid_run_plan(error),
                        shutdown,
                    )
                    .await;
                }
            };
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
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
        let call_recorder = command.record_calls().then(|| {
            LlmCallRecorder::new(
                project_workspace
                    .workspace_root()
                    .join("llm-calls")
                    .join(project_log.run_id()),
                project_log.run_id().to_owned(),
                file_system.clone(),
                project_log.logger.clone(),
            )
        });
        let llm = match OpenAiChatCompletionExecutor::new(
            command.llm().with_pem_roots(additional_pem_roots),
        )
        .map_err(ProductionCommandError::http_client_build)
        {
            Ok(value) => {
                if let Some(recorder) = call_recorder {
                    value.with_call_recorder(recorder)
                } else {
                    value
                }
            }
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
            .map_err(ProductionCommandError::cpu_start)
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
        let opener = PreopenedProject::new(opened_project);
        let business_log =
            ProductionBusinessLog::for_translation(&project_log, progress_observer.clone());
        let builder = ProductionSelectedTranslationExecutionBuilder {
            configuration: command.rpg_maker(),
            ui_locale: self.locale,
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
            termination_signals,
            || {
                cancellation.request();
                cpu.cancel_waits();
                file_system.cancel_waits();
                sqlite.cancel_waits();
                llm.cancel_waits();
            },
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
                    ProductionCommandError::translation_execution_build,
                    ProductionExternalModelFailure::into_external_model_failure,
                    map_translate_lua_error,
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
            project_log.logger.emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::NoWork,
                project_log.context.clone(),
                ProjectLogPayload::NoWork {
                    reason: ProjectLogNoWorkReason::TranslationUpToDate,
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
                progress.safe_stopping(progress_safe_stopping(self.locale));
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await;
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let failure_diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
        let pending_project_log =
            PendingProjectLog::new(project_log, log_outcome, failure_diagnostics);
        ProductionCommandRunReport::from_completion_with_project_log(
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
            Some(pending_project_log),
        )
    }

    async fn run_write_back(
        self,
        command: ConfiguredWriteBackCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = write_back_terminal_progress(self.progress_mode, self.locale);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let file_system = match SystemFileSystem::new_with_performance(
            command.common().filesystem().clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::file_system_build)
        {
            Ok(value) => value,
            Err(error) => return ProductionCommandRunReport::failed_before_logging(error),
        };
        let sqlite = match RusqliteStorage::start_with_performance(
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .map_err(ProductionCommandError::sqlite_start)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
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
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let explicit_lua_requested = command.lua().is_some();
        let saved_write_back = if explicit_lua_requested {
            None
        } else {
            let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
            match repository.read(database_path.clone()).await {
                Ok(plans) => plans.write_back().cloned(),
                Err(error) => {
                    let error = ProductionCommandError::project_run_plan_read(error);
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(shutdown_error) = sqlite.shutdown().await {
                        shutdown.push("SQLite", shutdown_error);
                    }
                    if let Err(shutdown_error) = file_system.shutdown().await {
                        shutdown.push("FileSystem", shutdown_error);
                    }
                    drop(project_lease_guard);
                    return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    );
                }
            }
        };
        let plan_source = if explicit_lua_requested {
            ProjectLogValueSource::Explicit
        } else if saved_write_back.is_some() {
            ProjectLogValueSource::ProjectState
        } else {
            ProjectLogValueSource::ProductDefault
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(project) => {
                        drop(project);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                if let Err(error) = file_system.shutdown().await {
                    shutdown.push("FileSystem", error);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let project_log = match start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            layout: self.layout,
            project: command.project_name().as_str(),
            command: "write-back",
            stage: DiagnosticStage::WriteBack,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: &self.panic_boundary,
        }) {
            Ok(log) => log,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            write_back_phase_code,
        );
        let lua = match command.lua() {
            Some(selected) => match load_lua_selection(&file_system, selected).await {
                Ok(program) => (!program.program.source().is_empty()).then_some(program),
                Err(source) => {
                    let diagnostic = source.safe_diagnostic();
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
                            ProductionCommandError::report_diagnostic(source, diagnostic),
                        )),
                        shutdown,
                    )
                    .await;
                }
            },
            None => match saved_write_back
                .as_ref()
                .and_then(WriteBackRunPlan::lua_program)
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
                            ProductionCommandError::configuration_load(error),
                            shutdown,
                        )
                        .await;
                    }
                },
                None => None,
            },
        };
        let lua_cleared = explicit_lua_requested && lua.is_none();
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
                        ProductionCommandError::invalid_run_plan(error),
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
            ProjectLogPayload::RunPlan {
                source: plan_source,
                lua_source: None,
                selections: vec![String::from("Standard")],
                lua_enabled: Some(lua.is_some()),
            },
        ));
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::cpu_start)
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
        let opener = PreopenedProject::new(opened_project);
        let asset_reader =
            RpgMakerStandardWriteBackAssetReadingService::new(sqlite.clone(), cpu.clone());
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
            termination_signals,
            || {
                cancellation.request();
                cpu.cancel_waits();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
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
                progress.safe_stopping(progress_safe_stopping(self.locale));
                let (confirmed, total) = progress_observer.confirmed_amount();
                project_log.emit_cancellation(
                    ProjectLogCode::CancellationRequested,
                    confirmed,
                    total,
                );
            },
        )
        .await;
        drop(project_lease_guard);
        progress.finish();
        let log_outcome = project_log_outcome(&execution, &shutdown);
        let failure_diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
        let pending_project_log =
            PendingProjectLog::new(project_log, log_outcome, failure_diagnostics);
        ProductionCommandRunReport::from_completion_with_project_log(
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
            Some(pending_project_log),
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

impl RulesProgramInputError {
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Read { source, .. } => safe_command_input_file_read(
                source,
                DiagnosticCode::ExtractRules,
                DiagnosticAction::FixInput,
            ),
            Self::Invalid { path, source } => source.safe_diagnostic(path),
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

impl LuaProgramInputError {
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Read { source, .. } => safe_command_input_file_read(
                source,
                DiagnosticCode::LuaExecution,
                DiagnosticAction::FixInput,
            ),
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
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Read { source, .. } => safe_command_input_file_read(
                source,
                DiagnosticCode::ExtractBuiltin,
                DiagnosticAction::FixInput,
            ),
            Self::InvalidUtf8 { path, source } => SafeDiagnostic::new(
                DiagnosticCode::ExtractBuiltin,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: u64::try_from(source.valid_up_to()).unwrap_or(u64::MAX),
                    error_len: source
                        .error_len()
                        .map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            Self::InvalidDefinition { path, source } => source
                .safe_diagnostic_source(
                    DiagnosticStage::CommandPreparation,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::FixInput,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        }
    }
}

fn safe_command_input_file_read(
    source: &ReadFileError<SystemFileSystemError>,
    code: DiagnosticCode,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    match source {
        ReadFileError::NotFound { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ReadFileError::NotFile { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ReadFileError::Io { source, .. } => source.safe_diagnostic(
            DiagnosticStage::CommandPreparation,
            DiagnosticImpact::Unchanged,
            action,
        ),
    }
}

async fn catch_command_panic(
    panic_boundary: CommandPanicBoundary,
    future: impl Future<Output = ProductionCommandRunReport>,
) -> ProductionCommandRunReport {
    let result = {
        let guarded = AssertUnwindSafe(future).catch_unwind();
        guarded.await
    };
    match result {
        Ok(report) => report,
        Err(payload) => {
            // payload 可能含 Prompt、模型正文、Lua、SQL 或用户文本；只丢弃，绝不读取。
            drop(payload);
            ProductionCommandRunReport::panicked(panic_boundary.panic_error())
        }
    }
}

struct ActiveProjectLog {
    run_id: String,
    runtime: ProjectLogRuntime,
    logger: ProjectLogger,
    context: ProjectLogContext,
    performance: Arc<RunPerformanceCounters>,
}

pub(crate) struct PendingProjectLog {
    active: ActiveProjectLog,
    outcome: ProjectLogRunOutcome,
    failures: Vec<SafeDiagnostic>,
}

impl PendingProjectLog {
    fn new(
        active: ActiveProjectLog,
        outcome: ProjectLogRunOutcome,
        failures: Vec<SafeDiagnostic>,
    ) -> Self {
        Self {
            active,
            outcome,
            failures,
        }
    }

    pub(crate) fn finish(self) -> Option<ProjectLogWarning> {
        finish_project_log(self.active, self.outcome, self.failures)
    }

    pub(crate) fn finish_with_failure(
        self,
        error: &ProductionCommandError,
    ) -> Option<ProjectLogWarning> {
        let failures = error
            .failure_report()
            .public_diagnostics()
            .cloned()
            .collect();
        finish_project_log(self.active, ProjectLogRunOutcome::Failed, failures)
    }

    /// 在最终终端呈现前把 runtime 的 Drop 兜底切换为进程输出 panic。
    ///
    /// 返回的边界保留同一组安全投影供 CLI 使用；若呈现期间 unwind 消费并丢弃
    /// `PendingProjectLog`，runtime 会先写出这些诊断和未知终态。
    pub(crate) fn arm_presentation_panic(&mut self) -> CommandPanicBoundary {
        let mut diagnostics = self
            .active
            .runtime
            .unfinished_failures()
            .unwrap_or_default();
        let presentation = match diagnostics.first().cloned() {
            Some(mut diagnostic) => {
                diagnostic.stage = DiagnosticStage::ProcessOutput;
                diagnostic.impact = DiagnosticImpact::OutcomeUnknown;
                diagnostic.action = DiagnosticAction::ReportBug;
                diagnostic
            }
            None => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::ProcessOutput,
                DiagnosticSubject::operation("render_command_result"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::ReportBug,
            ),
        };
        diagnostics.clear();
        diagnostics.push(presentation);
        diagnostics.extend(self.failures.iter().cloned());
        self.active.runtime.arm_unfinished_terminal(
            self.active.context.clone(),
            diagnostics.clone(),
            Arc::clone(&self.active.performance),
        );
        CommandPanicBoundary::from_logged(diagnostics, self.active.logger.clone())
    }
}

#[derive(Clone)]
struct ProductionProgressObserver<P> {
    terminal: TerminalProgressObserver<P>,
    project_log: Option<ProgressProjectLog>,
    phase_code: fn(P) -> ProjectLogPhase,
    state: Arc<Mutex<ProgressLogState<P>>>,
}

#[derive(Clone)]
struct ProgressProjectLog {
    logger: ProjectLogger,
    context: ProjectLogContext,
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
        phase_code: fn(P) -> ProjectLogPhase,
    ) -> Self {
        Self {
            terminal,
            project_log: Some(ProgressProjectLog {
                logger: project_log.logger.clone(),
                context: project_log.context.clone(),
            }),
            phase_code,
            state: Arc::new(Mutex::new(ProgressLogState {
                phase: None,
                amount: ProgressAmount::Indeterminate,
                finished: false,
            })),
        }
    }

    fn without_project_log(
        terminal: TerminalProgressObserver<P>,
        phase_code: fn(P) -> ProjectLogPhase,
    ) -> Self {
        Self {
            terminal,
            project_log: None,
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
        let Some(project_log) = &self.project_log else {
            return;
        };
        let phase_code = (self.phase_code)(phase);
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            code,
            project_log.context.clone(),
            ProjectLogPayload::Phase {
                phase: phase_code,
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

const fn init_phase_code(phase: InitProgressPhase) -> ProjectLogPhase {
    match phase {
        InitProgressPhase::CheckingProject => ProjectLogPhase::CheckProject,
        InitProgressPhase::ScanningSource => ProjectLogPhase::ScanSource,
        InitProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        InitProgressPhase::UpdatingDatabase => ProjectLogPhase::UpdateDatabase,
        InitProgressPhase::Publishing => ProjectLogPhase::Publish,
    }
}

const fn extract_phase_code(phase: ExtractProgressPhase) -> ProjectLogPhase {
    match phase {
        ExtractProgressPhase::Builtin => ProjectLogPhase::Builtin,
        ExtractProgressPhase::BuiltinDocuments => ProjectLogPhase::BuiltinDocuments,
        ExtractProgressPhase::BuiltinWorkUnits => ProjectLogPhase::BuiltinWorkUnits,
        ExtractProgressPhase::BuiltinCommit => ProjectLogPhase::BuiltinCommit,
        ExtractProgressPhase::Rules => ProjectLogPhase::Rules,
        ExtractProgressPhase::RulesDocuments => ProjectLogPhase::RulesDocuments,
        ExtractProgressPhase::RulesMatches => ProjectLogPhase::RulesMatches,
        ExtractProgressPhase::RulesCommit => ProjectLogPhase::RulesCommit,
        ExtractProgressPhase::Lua => ProjectLogPhase::Lua,
        ExtractProgressPhase::LuaExecution => ProjectLogPhase::LuaExecution,
        ExtractProgressPhase::LuaCommit => ProjectLogPhase::LuaCommit,
    }
}

const fn translate_phase_code(phase: TranslateProgressPhase) -> ProjectLogPhase {
    match phase {
        TranslateProgressPhase::Planning => ProjectLogPhase::Planning,
        TranslateProgressPhase::ConfirmedTasks => ProjectLogPhase::ConfirmedTasks,
        TranslateProgressPhase::NoWork => ProjectLogPhase::NoWork,
    }
}

const fn write_back_phase_code(phase: WriteBackProgressPhase) -> ProjectLogPhase {
    match phase {
        WriteBackProgressPhase::ReadingAssets => ProjectLogPhase::ReadAssets,
        WriteBackProgressPhase::PlanningStandard => ProjectLogPhase::PlanStandard,
        WriteBackProgressPhase::RewritingDocuments => ProjectLogPhase::RewriteDocuments,
        WriteBackProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        WriteBackProgressPhase::RunningLua => ProjectLogPhase::Lua,
        WriteBackProgressPhase::ValidatingCandidate => ProjectLogPhase::ValidateCandidate,
        WriteBackProgressPhase::Publishing => ProjectLogPhase::Publish,
    }
}

impl ActiveProjectLog {
    fn run_id(&self) -> &str {
        &self.run_id
    }

    fn set_profile(&mut self, profile: &str) {
        self.context = self.context.clone().with_profile(profile);
    }

    fn emit_cancellation(&self, code: ProjectLogCode, confirmed: u64, total: Option<u64>) {
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            code,
            self.context.clone(),
            ProjectLogPayload::Cancellation { confirmed, total },
        ));
    }
}

fn command_panic_context(
    layout: RpgMakerLayout,
    command: &ConfiguredRpgMakerCommand,
) -> (&'static str, DiagnosticStage, PathBuf) {
    let (command_name, stage, common, project_name) = match command {
        ConfiguredRpgMakerCommand::Init(command) => (
            "init",
            DiagnosticStage::Init,
            command.common(),
            command.arguments.project.name.as_str(),
        ),
        ConfiguredRpgMakerCommand::Extract(command) => (
            "extract",
            DiagnosticStage::Extract,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::Translate(command) => (
            "translate",
            DiagnosticStage::Translate,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::WriteBack(command) => (
            "write-back",
            DiagnosticStage::WriteBack,
            command.common(),
            command.project_name().as_str(),
        ),
    };
    (
        command_name,
        stage,
        common
            .projects_root()
            .join(layout.engine().storage_name())
            .join(project_name),
    )
}

fn command_panic_diagnostic(
    command: &'static str,
    stage: DiagnosticStage,
    project_workspace: &Path,
    log_path: Option<&Path>,
) -> SafeDiagnostic {
    let mut diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        stage,
        DiagnosticSubject::command(command),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::ReportBug,
    )
    .with_recovery(RecoveryFact::path(project_workspace));
    if let Some(log_path) = log_path {
        diagnostic = diagnostic.with_recovery(RecoveryFact::path(log_path));
    }
    diagnostic
}

struct CommandLogStart<'a> {
    common: &'a crate::application::config::CommonCommandConfiguration,
    locale: UiLocale,
    layout: RpgMakerLayout,
    project: &'a str,
    command: &'static str,
    stage: DiagnosticStage,
    profile: Option<&'a str>,
    performance: Arc<RunPerformanceCounters>,
    panic_boundary: &'a CommandPanicBoundary,
}

fn start_command_log(
    input: CommandLogStart<'_>,
) -> Result<ActiveProjectLog, ProductionCommandError> {
    let CommandLogStart {
        common,
        locale,
        layout,
        project,
        command,
        stage,
        profile,
        performance,
        panic_boundary,
    } = input;
    let run_id = generate_run_id()
        .map_err(ProductionCommandError::run_id)?
        .to_string();
    let logs_root = common
        .projects_root()
        .join(layout.engine().storage_name())
        .join(project)
        .join("logs");
    let project_workspace = logs_root
        .parent()
        .expect("logs 路径必须位于项目工作区内")
        .to_path_buf();
    let mut runtime = start_project_log(logs_root, run_id.clone());
    let logger = runtime.logger();
    let mut context = ProjectLogContext::new(locale.as_str())
        .with_engine(layout.engine().storage_name())
        .with_project(project)
        .with_command(command);
    if let Some(profile) = profile {
        context = context.with_profile(profile);
    }
    let panic_diagnostic =
        command_panic_diagnostic(command, stage, &project_workspace, runtime.path());
    runtime.arm_unfinished_terminal(
        context.clone(),
        vec![panic_diagnostic.clone()],
        Arc::clone(&performance),
    );
    panic_boundary.register_project_log(vec![panic_diagnostic], logger.clone());
    logger.emit(ProjectLogEvent::new(
        ProjectLogLevel::Info,
        ProjectLogCode::RunStarted,
        context.clone(),
        ProjectLogPayload::Run { outcome: None },
    ));
    Ok(ActiveProjectLog {
        run_id,
        runtime,
        logger,
        context,
        performance,
    })
}

fn finish_project_log(
    project_log: ActiveProjectLog,
    outcome: ProjectLogRunOutcome,
    failures: Vec<SafeDiagnostic>,
) -> Option<ProjectLogWarning> {
    let performance = project_log.performance.snapshot();
    let logger = project_log.logger;
    let _health = project_log.runtime.finish_with_performance(
        outcome,
        project_log.context,
        failures,
        performance,
    );
    logger.take_warning()
}

fn project_log_failure_diagnostics<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
) -> Vec<SafeDiagnostic> {
    let mut diagnostics = match execution {
        DrivenCommand::Interrupted(Err(error)) if error.was_cancelled_wait() => Vec::new(),
        DrivenCommand::Finished(Err(error)) | DrivenCommand::Interrupted(Err(error)) => error
            .failure_report()
            .public_diagnostics()
            .cloned()
            .collect(),
        DrivenCommand::SignalFailed { source, result } => {
            let signal_impact = if matches!(result, Ok(OperationCompletion::Completed(_))) {
                DiagnosticImpact::StateAppliedFinalizationFailed
            } else {
                DiagnosticImpact::Unchanged
            };
            let signal = signal_diagnostic(source, signal_impact);
            match result {
                Err(error) => error
                    .failure_report()
                    .public_diagnostics()
                    .cloned()
                    .chain(std::iter::once(signal))
                    .collect(),
                Ok(_) => vec![signal],
            }
        }
        DrivenCommand::Finished(Ok(_)) | DrivenCommand::Interrupted(Ok(_)) => Vec::new(),
    };
    diagnostics.extend(shutdown.public_diagnostics().cloned());
    diagnostics
}

fn signal_diagnostic(source: &io::Error, impact: DiagnosticImpact) -> SafeDiagnostic {
    SafeDiagnostic::io(
        DiagnosticCode::SignalRegistration,
        DiagnosticStage::Shutdown,
        DiagnosticSubject::component("Windows control signal"),
        "receive_signal",
        source,
        impact,
        DiagnosticAction::Retry,
    )
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
    let mut diagnostics = error
        .failure_report()
        .public_diagnostics()
        .cloned()
        .collect::<Vec<_>>();
    diagnostics.extend(shutdown.public_diagnostics().cloned());
    let pending_project_log = PendingProjectLog::new(project_log, outcome, diagnostics);
    ProductionCommandRunReport::construction_failed_with_shutdown_and_project_log(
        error,
        shutdown,
        Some(pending_project_log),
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

struct RunPlanFinalizationInput {
    database_path: PathBuf,
    replacement: ProjectRunPlanReplacement,
    sqlite_configuration: crate::runtime::sqlite::RusqliteStorageConfiguration,
}

async fn finalize_run_plan<T>(
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
        Arc::clone(&project_log.performance),
    );
    let finalizer = FinalProjectRunPlanPersistenceService::new(transaction_executor.clone());
    let finalization = drive_command(
        finalizer.replace_final(database_path, replacement),
        termination_signals,
        || transaction_executor.cancel_waits(),
        on_cancellation,
    )
    .await;
    merge_run_plan_finalization(execution, finalization, project_log)
}

fn merge_run_plan_finalization<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    finalization: DrivenCommand<Result<(), ProjectRunPlanReplaceError<SqliteRuntimeError>>>,
    project_log: &ActiveProjectLog,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    match finalization {
        DrivenCommand::Finished(result) => {
            replace_success_with_plan_error(execution, observe_run_plan_result(result, project_log))
        }
        DrivenCommand::Interrupted(result) => match result {
            Ok(()) => mark_successful_execution_interrupted(execution),
            Err(error) if run_plan_wait_was_cancelled(&error) => {
                mark_successful_execution_interrupted(execution)
            }
            Err(error) => {
                DrivenCommand::Interrupted(Err(observe_run_plan_error(error, project_log)))
            }
        },
        DrivenCommand::SignalFailed { source, result } => {
            let result = match result {
                Ok(()) => take_successful_execution_result(execution),
                Err(error) if run_plan_wait_was_cancelled(&error) => {
                    take_successful_execution_result(execution)
                }
                Err(error) => Err(observe_run_plan_error(error, project_log)),
            };
            DrivenCommand::SignalFailed { source, result }
        }
    }
}

fn observe_run_plan_result(
    result: Result<(), ProjectRunPlanReplaceError<SqliteRuntimeError>>,
    project_log: &ActiveProjectLog,
) -> Result<(), ProductionCommandError> {
    match result {
        Ok(()) => {
            project_log.logger.emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::RunPlanSaved,
                project_log.context.clone(),
                ProjectLogPayload::None,
            ));
            Ok(())
        }
        Err(error) => Err(observe_run_plan_error(error, project_log)),
    }
}

fn observe_run_plan_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
    project_log: &ActiveProjectLog,
) -> ProductionCommandError {
    let (level, code) = run_plan_replace_log_fact(&error);
    project_log.logger.emit(ProjectLogEvent::new(
        level,
        code,
        project_log.context.clone(),
        ProjectLogPayload::None,
    ));
    map_run_plan_replace_error(error)
}

fn run_plan_wait_was_cancelled(error: &ProjectRunPlanReplaceError<SqliteRuntimeError>) -> bool {
    matches!(
        error,
        ProjectRunPlanReplaceError::RollbackConfirmed {
            source: SqliteRuntimeError::Cancelled { .. },
            ..
        }
    )
}

fn mark_successful_execution_interrupted<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    DrivenCommand::Interrupted(take_successful_execution_result(execution))
}

fn take_successful_execution_result<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> Result<OperationCompletion<T>, ProductionCommandError> {
    match execution {
        DrivenCommand::Finished(result @ Ok(OperationCompletion::Completed(_))) => result,
        _ => unreachable!("只有成功业务执行才会进入运行方案最终化"),
    }
}

fn run_plan_replace_log_fact<E>(
    error: &ProjectRunPlanReplaceError<E>,
) -> (ProjectLogLevel, ProjectLogCode) {
    match error {
        ProjectRunPlanReplaceError::DatabaseNotFound { .. }
        | ProjectRunPlanReplaceError::RequirementFailed { .. }
        | ProjectRunPlanReplaceError::RollbackConfirmed { .. } => {
            (ProjectLogLevel::Warn, ProjectLogCode::RunPlanSaveFailed)
        }
        ProjectRunPlanReplaceError::OutcomeUnknown { .. } => (
            ProjectLogLevel::Error,
            ProjectLogCode::RunPlanSaveOutcomeUnknown,
        ),
        ProjectRunPlanReplaceError::CommittedButFinalizationFailed { .. } => (
            ProjectLogLevel::Error,
            ProjectLogCode::RunPlanSavedFinalizationFailed,
        ),
    }
}

fn map_run_plan_replace_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
) -> ProductionCommandError {
    let (diagnostic, terminal) = match &error {
        ProjectRunPlanReplaceError::DatabaseNotFound { path } => (
            SafeDiagnostic::new(
                DiagnosticCode::RunPlanSaveFailed,
                DiagnosticStage::RunPlanFinalization,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::ResultAppliedPlanNotSaved,
                DiagnosticAction::Retry,
            ),
            0_u8,
        ),
        ProjectRunPlanReplaceError::RequirementFailed { path } => (
            SafeDiagnostic::new(
                DiagnosticCode::RunPlanSaveFailed,
                DiagnosticStage::RunPlanFinalization,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::RequirementFailed),
                DiagnosticImpact::ResultAppliedPlanNotSaved,
                DiagnosticAction::CheckProjectState,
            ),
            0,
        ),
        ProjectRunPlanReplaceError::RollbackConfirmed { path, source } => (
            source
                .safe_diagnostic(
                    DiagnosticStage::RunPlanFinalization,
                    DiagnosticImpact::ResultAppliedPlanNotSaved,
                    DiagnosticAction::Retry,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path))
                .with_recovery(crate::diagnostic::RecoveryFact::transaction(
                    "rollback_confirmed",
                )),
            0,
        ),
        ProjectRunPlanReplaceError::OutcomeUnknown { path, source } => (
            source
                .safe_diagnostic(
                    DiagnosticStage::RunPlanFinalization,
                    DiagnosticImpact::OutcomeUnknown,
                    DiagnosticAction::PreserveRecoveryArtifacts,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path))
                .with_recovery(crate::diagnostic::RecoveryFact::transaction(
                    "outcome_unknown",
                )),
            1,
        ),
        ProjectRunPlanReplaceError::CommittedButFinalizationFailed { path, source } => (
            source
                .safe_diagnostic(
                    DiagnosticStage::RunPlanFinalization,
                    DiagnosticImpact::StateAppliedFinalizationFailed,
                    DiagnosticAction::Retry,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path))
                .with_recovery(crate::diagnostic::RecoveryFact::transaction("committed")),
            2,
        ),
    };
    let report = ProductionCommandError::report_diagnostic(error, diagnostic);
    match terminal {
        0 => ProductionCommandError::ResultAppliedButRunPlanNotSaved(Box::new(report)),
        1 => ProductionCommandError::RunPlanOutcomeUnknown(Box::new(report)),
        _ => ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report)),
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
                diagnostic,
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
                let attempt_count =
                    attempts.map(|value| u64::try_from(value.get()).unwrap_or(u64::MAX));
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
                let outcome = match outcome {
                    StandardTranslationLogTaskOutcome::Complete => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Complete
                    }
                    StandardTranslationLogTaskOutcome::Partial => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Partial
                    }
                    StandardTranslationLogTaskOutcome::Unavailable => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Unavailable
                    }
                    StandardTranslationLogTaskOutcome::ExecutionFailed
                    | StandardTranslationLogTaskOutcome::CommitFailed
                    | StandardTranslationLogTaskOutcome::NotCommitted
                    | StandardTranslationLogTaskOutcome::InvalidResult => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Failed
                    }
                };
                self.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Debug,
                    ProjectLogCode::TaskFinished,
                    self.context.clone(),
                    ProjectLogPayload::Task {
                        ordinal,
                        total,
                        outcome: Some(outcome),
                        attempts: attempt_count,
                    },
                ));
                if let Some(diagnostic) = diagnostic {
                    debug_assert!(attempt_count.is_some());
                    self.logger.emit(ProjectLogEvent::new(
                        ProjectLogLevel::Warn,
                        ProjectLogCode::TaskDiagnostic,
                        self.context.clone(),
                        ProjectLogPayload::TaskDiagnostic {
                            ordinal,
                            total,
                            attempts: attempt_count.unwrap_or(0),
                            diagnostic,
                        },
                    ));
                }
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
                    ProjectLogPayload::Publication {
                        outcome:
                            crate::runtime::project_log::ProjectLogPublicationOutcome::NotPublished,
                        published_items: None,
                    },
                ));
                let _ = output_root;
            }
            WriteBackLogEvent::PublicationFinished {
                output_root: _,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptResourceComponent {
    System,
    Thinking,
}

impl PromptResourceComponent {
    const fn as_str(self) -> &'static str {
        match self {
            Self::System => SYSTEM_PROMPT_FILE_NAME,
            Self::Thinking => THINKING_PROMPT_FILE_NAME,
        }
    }
}

async fn read_prompt_resource(
    file_system: &SystemFileSystem,
    path: &Path,
) -> Result<String, PromptResourceLoadError> {
    let file = file_system
        .read_file(path.to_owned())
        .await
        .map_err(PromptResourceLoadError::Read)?;
    if file.resolved_path().file_name() != path.file_name() {
        return Err(PromptResourceLoadError::ResolvedFileNameMismatch {
            requested_path: path.to_owned(),
            resolved_path: file.resolved_path().to_owned(),
        });
    }
    let text = String::from_utf8(file.into_bytes()).map_err(|source| {
        let utf8 = source.utf8_error();
        PromptResourceLoadError::InvalidUtf8 {
            path: path.to_owned(),
            valid_up_to: utf8.valid_up_to(),
            error_len: utf8.error_len(),
        }
    })?;
    let text = text.trim();
    if text.is_empty() {
        return Err(PromptResourceLoadError::Empty {
            path: path.to_owned(),
        });
    }
    Ok(text.to_owned())
}

#[derive(Debug)]
enum PromptResourceLoadError {
    Read(ReadFileError<SystemFileSystemError>),
    ResolvedFileNameMismatch {
        requested_path: PathBuf,
        resolved_path: PathBuf,
    },
    InvalidUtf8 {
        path: PathBuf,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    Empty {
        path: PathBuf,
    },
}

impl PromptResourceLoadError {
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Read(ReadFileError::NotFound { path }) => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Read(ReadFileError::NotFile { path }) => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InvalidValue,
                    "expected=file; actual=not_file",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Read(ReadFileError::Io { path, source }) => source
                .safe_diagnostic_source(
                    DiagnosticStage::CommandPreparation,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(RecoveryFact::path(path)),
            Self::ResolvedFileNameMismatch {
                requested_path,
                resolved_path,
            } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(requested_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::FileIdentityChanged,
                    format!(
                        "expected_file_name={}; actual_file_name={}",
                        requested_path
                            .file_name()
                            .map_or_else(|| "none".into(), |name| name.to_string_lossy()),
                        resolved_path
                            .file_name()
                            .map_or_else(|| "none".into(), |name| name.to_string_lossy())
                    ),
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
            .with_recovery(RecoveryFact::path(resolved_path)),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: *valid_up_to as u64,
                    error_len: error_len.map(|length| length as u64),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Empty { path } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    "resource=prompt; content=blank",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
        }
    }
}

impl fmt::Display for PromptResourceLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(source) => write!(formatter, "无法读取 Prompt 资源：{source}"),
            Self::ResolvedFileNameMismatch {
                requested_path,
                resolved_path,
            } => write!(
                formatter,
                "Prompt 资源文件身份不匹配：请求 {}，固定后为 {}",
                requested_path.display(),
                resolved_path.display()
            ),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => write!(
                formatter,
                "Prompt 资源不是 UTF-8：{}（valid_up_to={valid_up_to}, error_len={error_len:?}）",
                path.display()
            ),
            Self::Empty { path } => write!(formatter, "Prompt 资源正文为空：{}", path.display()),
        }
    }
}

impl Error for PromptResourceLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::ResolvedFileNameMismatch { .. }
            | Self::InvalidUtf8 { .. }
            | Self::Empty { .. } => None,
        }
    }
}

fn render_system_prompt_template(
    template: &str,
    language_pair: &crate::language::LanguagePair,
) -> Result<String, PromptTemplateError> {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    let mut source_seen = false;
    let mut target_seen = false;

    loop {
        let next_open = remaining.find("{{");
        let next_close = remaining.find("}}");
        let Some(open) = next_open else {
            if next_close.is_some() {
                return Err(PromptTemplateError::InvalidSyntax);
            }
            rendered.push_str(remaining);
            break;
        };
        if next_close.is_some_and(|close| close < open) {
            return Err(PromptTemplateError::InvalidSyntax);
        }

        rendered.push_str(&remaining[..open]);
        let after_open = &remaining[open + 2..];
        let close = after_open
            .find("}}")
            .ok_or(PromptTemplateError::InvalidSyntax)?;
        if after_open[..close].contains("{{") {
            return Err(PromptTemplateError::InvalidSyntax);
        }
        let variable = &remaining[open..open + 2 + close + 2];
        match variable {
            SOURCE_LANGUAGE_TEMPLATE_VARIABLE => {
                rendered.push_str(language_pair.source().as_str());
                source_seen = true;
            }
            TARGET_LANGUAGE_TEMPLATE_VARIABLE => {
                rendered.push_str(language_pair.target().as_str());
                target_seen = true;
            }
            _ => return Err(PromptTemplateError::UnknownVariable),
        }
        remaining = &after_open[close + 2..];
    }

    if !source_seen {
        return Err(PromptTemplateError::MissingSourceLanguage);
    }
    if !target_seen {
        return Err(PromptTemplateError::MissingTargetLanguage);
    }
    if rendered.contains("{{") || rendered.contains("}}") {
        return Err(PromptTemplateError::InvalidSyntax);
    }
    Ok(rendered)
}

fn ensure_no_prompt_template_variables(text: &str) -> Result<(), PromptTemplateError> {
    if text.contains("{{") || text.contains("}}") {
        return Err(PromptTemplateError::VariablesNotAllowed);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptTemplateError {
    InvalidSyntax,
    UnknownVariable,
    MissingSourceLanguage,
    MissingTargetLanguage,
    VariablesNotAllowed,
}

impl fmt::Display for PromptTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSyntax => formatter.write_str("Prompt 模板变量语法无效"),
            Self::UnknownVariable => formatter.write_str("Prompt 模板包含不受支持的变量"),
            Self::MissingSourceLanguage => {
                formatter.write_str("Prompt 模板缺少 source_language 变量")
            }
            Self::MissingTargetLanguage => {
                formatter.write_str("Prompt 模板缺少 target_language 变量")
            }
            Self::VariablesNotAllowed => formatter.write_str("该 Prompt 组件不允许包含模板变量"),
        }
    }
}

impl Error for PromptTemplateError {}

#[cfg(test)]
mod prompt_template_tests {
    use crate::language::{LanguageId, LanguagePair};

    use super::*;

    fn language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
        )
    }

    #[test]
    fn system_template_replaces_every_occurrence_of_both_supported_variables() {
        let rendered = render_system_prompt_template(
            "{{source_language}} -> {{target_language}} / {{source_language}}",
            &language_pair(),
        )
        .expect("两个受支持变量可多次渲染");

        assert_eq!(rendered, "ja -> zh-Hans / ja");
        assert!(!rendered.contains("{{"));
        assert!(!rendered.contains("}}"));
    }

    #[test]
    fn system_template_rejects_missing_unknown_and_malformed_variables() {
        for (template, expected) in [
            (
                "{{target_language}}",
                PromptTemplateError::MissingSourceLanguage,
            ),
            (
                "{{source_language}}",
                PromptTemplateError::MissingTargetLanguage,
            ),
            (
                "{{source_language}} {{target_language}} {{other}}",
                PromptTemplateError::UnknownVariable,
            ),
            (
                "{{source_language}} {{target_language}",
                PromptTemplateError::InvalidSyntax,
            ),
            (
                "{{source_language}} }} {{target_language}}",
                PromptTemplateError::InvalidSyntax,
            ),
            (
                "{{{{source_language}} {{target_language}}",
                PromptTemplateError::InvalidSyntax,
            ),
        ] {
            assert_eq!(
                render_system_prompt_template(template, &language_pair())
                    .expect_err("无效模板必须失败"),
                expected,
                "模板：{template}"
            );
        }
    }

    #[test]
    fn thinking_component_rejects_all_template_delimiters() {
        assert_eq!(ensure_no_prompt_template_variables("实际思考要求"), Ok(()));
        for text in ["{{source_language}}", "前缀 {{", "后缀 }}"] {
            assert_eq!(
                ensure_no_prompt_template_variables(text),
                Err(PromptTemplateError::VariablesNotAllowed)
            );
        }
    }
}

#[cfg(test)]
mod large_external_input_tests {
    use std::fs::File;
    use std::io::Write as _;

    use super::*;
    use crate::application::arguments::{
        ExtractArguments, MzCommand, ProductCommand, ProjectArguments,
    };
    use crate::application::config::load_product_configuration;

    const LARGE_SOURCE_PAYLOAD_BYTES: usize = 17 * 1024 * 1024 + 1;

    #[tokio::test]
    async fn rules_and_lua_larger_than_seventeen_mibibytes_cross_the_production_loaders() {
        let directory = tempfile::tempdir().expect("应建立临时目录");
        let rules_path = directory.path().join("large-rules.toml");
        write_large_text_file(
            &rules_path,
            b"#",
            LARGE_SOURCE_PAYLOAD_BYTES,
            b"\nrule = []\n",
        );
        let lua_path = directory.path().join("large-script.lua");
        write_large_text_file(
            &lua_path,
            b"--",
            LARGE_SOURCE_PAYLOAD_BYTES,
            b"\nreturn nil\n",
        );

        let configuration_path = directory.path().join("config.toml");
        std::fs::write(
            &configuration_path,
            include_str!("../../config.example.toml"),
        )
        .expect("应写入现行配置");
        let configured = load_product_configuration(
            &configuration_path,
            ProductCommand::Mz {
                command: MzCommand::Extract(ExtractArguments {
                    project: ProjectArguments {
                        name: "large-input".parse().expect("测试项目名称应合法"),
                    },
                    builtin: false,
                    rules: Some(rules_path.clone()),
                    lua: Some(lua_path.clone()),
                }),
            },
        )
        .expect("大文件不应被配置边界提前拒绝");
        let (_, ConfiguredRpgMakerCommand::Extract(command)) = configured.into_parts() else {
            panic!("应建立 Extract 配置");
        };
        let file_system =
            SystemFileSystem::new(command.common().filesystem().clone()).expect("文件系统应启动");

        let rules = load_rules_program(&file_system, &rules_path)
            .await
            .expect("17 MiB 以上 Rules 应通过生产读取和 TOML/PCRE2 解析边界");
        assert!(rules.is_empty());
        assert_eq!(
            rules.diagnostic_path(),
            rules_path.canonicalize().expect("Rules 路径应规范化")
        );

        let lua = load_lua_selection(&file_system, command.lua().expect("测试显式选择了 Lua"))
            .await
            .expect("17 MiB 以上 Lua 应通过生产读取和程序准备边界");
        assert!(lua.program.source().len() > 17 * 1024 * 1024);
        assert_eq!(
            lua.program.main_script_path(),
            lua_path.canonicalize().expect("Lua 路径应规范化")
        );

        file_system.shutdown().await.expect("文件系统应关闭");
    }

    fn write_large_text_file(path: &Path, prefix: &[u8], payload_bytes: usize, suffix: &[u8]) {
        let mut file = File::create(path).expect("应建立大文件");
        file.write_all(prefix).expect("应写入前缀");
        let chunk = vec![b'x'; 1024 * 1024];
        let mut remaining = payload_bytes;
        while remaining != 0 {
            let count = remaining.min(chunk.len());
            file.write_all(&chunk[..count]).expect("应写入大文件正文");
            remaining -= count;
        }
        file.write_all(suffix).expect("应写入后缀");
        file.sync_all().expect("应完整落盘测试输入");
    }
}

struct ProductionSelectedTranslationExecutionBuilder<'a> {
    configuration: &'a TranslateConfiguration,
    ui_locale: UiLocale,
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
        let prompt_locale = self.configuration.prompt_locale().resolve(self.ui_locale);
        let prompt_directory = self
            .configuration
            .prompt_root()
            .join(RPG_MAKER_PROMPT_DIRECTORY_NAME)
            .join(prompt_locale.as_str());
        let system_path = prompt_directory.join(SYSTEM_PROMPT_FILE_NAME);
        let system_template = read_prompt_resource(&self.file_system, &system_path)
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    prompt_locale,
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            })?;
        let mut markdown = render_system_prompt_template(&system_template, &language_pair)
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_template(
                    prompt_locale,
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            })?;
        let response_envelope = if self.configuration.thinking_output() {
            let thinking_path = prompt_directory.join(THINKING_PROMPT_FILE_NAME);
            let thinking = read_prompt_resource(&self.file_system, &thinking_path)
                .await
                .map_err(|source| {
                    ProductionTranslationExecutionBuildError::prompt_resource(
                        prompt_locale,
                        PromptResourceComponent::Thinking,
                        &thinking_path,
                        source,
                    )
                })?;
            ensure_no_prompt_template_variables(&thinking).map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_template(
                    prompt_locale,
                    PromptResourceComponent::Thinking,
                    &thinking_path,
                    source,
                )
            })?;
            markdown.push_str("\n\n");
            markdown.push_str(&thinking);
            TranslationResponseEnvelope::ThinkingThenJson
        } else {
            TranslationResponseEnvelope::JsonOnly
        };
        let system_prompt =
            RpgMakerSystemPrompt::new(language_pair.clone(), markdown, response_envelope).map_err(
                |source| {
                    ProductionTranslationExecutionBuildError::system_prompt(
                        prompt_locale,
                        PromptResourceComponent::System,
                        &system_path,
                        source,
                    )
                },
            )?;
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
            profile_configuration.target_task_user_message_characters(),
        );
        let profile = Arc::new(RpgMakerTranslationProfile::new(
            profile_configuration.id(),
            planning,
            profile_configuration.request().clone(),
            Arc::clone(self.configuration.client()),
        ));
        let placeholders = Pcre2PlaceholderService::new()
            .map_err(ProductionTranslationExecutionBuildError::builtin_placeholder_compile)?;
        let asset_reader = RpgMakerStandardTranslationAssetReadingService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
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
        >::new(
            self.llm.clone(),
            TokioAsyncDelay,
            processor,
            self.cancellation.clone(),
        );
        let result_store = RpgMakerStandardTranslationResultStorageService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
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
    class: TranslationExecutionBuildFailureClass,
    diagnostic: SafeDiagnostic,
    source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslationExecutionBuildFailureClass {
    ConfigurationOrInput,
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    fn prompt_resource(
        locale: UiLocale,
        component: PromptResourceComponent,
        path: &Path,
        source: PromptResourceLoadError,
    ) -> Self {
        let diagnostic = with_prompt_context(source.safe_diagnostic(), locale, component, path);
        Self::new(source, diagnostic)
    }

    fn prompt_template(
        locale: UiLocale,
        component: PromptResourceComponent,
        path: &Path,
        source: PromptTemplateError,
    ) -> Self {
        let diagnostic = with_prompt_context(
            prompt_template_diagnostic(source, path),
            locale,
            component,
            path,
        );
        Self::new(source, diagnostic)
    }

    fn system_prompt(
        locale: UiLocale,
        component: PromptResourceComponent,
        path: &Path,
        source: RpgMakerSystemPromptError,
    ) -> Self {
        let diagnostic = match &source {
            RpgMakerSystemPromptError::Blank => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    "resource=system_prompt; content=blank_after_template_render",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
        };
        let diagnostic = with_prompt_context(diagnostic, locale, component, path);
        Self::new(source, diagnostic)
    }

    fn language_module(
        language_pair: &crate::language::LanguagePair,
        source: LanguageModuleCatalogError,
    ) -> Self {
        let LanguageModuleCatalogError::UnknownLanguageId {
            language_id,
            available_ids,
        } = &source;
        let available_ids = available_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::LanguageModuleUnavailable,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::field("source_language"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::NotFound,
                format!(
                    "requested_id={language_id}; available_ids={}",
                    if available_ids.is_empty() {
                        "none"
                    } else {
                        &available_ids
                    }
                ),
            ),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        )
        .with_recovery(RecoveryFact::component(format!(
            "target_language={}",
            language_pair.target()
        )));
        Self::new(source, diagnostic)
    }

    fn builtin_placeholder_compile(source: Pcre2PlaceholderConstructionError) -> Self {
        let diagnostic = source.safe_diagnostic_source(
            DiagnosticStage::CommandPreparation,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        );
        Self::new(source, diagnostic)
    }

    fn new(source: impl Error + Send + Sync + 'static, diagnostic: SafeDiagnostic) -> Self {
        let class = if diagnostic.action == DiagnosticAction::ReportBug {
            TranslationExecutionBuildFailureClass::Internal
        } else {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput
        };
        Self {
            class,
            diagnostic,
            source: Box::new(source),
        }
    }

    const fn diagnostic(&self) -> &SafeDiagnostic {
        &self.diagnostic
    }
}

fn with_prompt_context(
    diagnostic: SafeDiagnostic,
    locale: UiLocale,
    component: PromptResourceComponent,
    path: &Path,
) -> SafeDiagnostic {
    diagnostic
        .with_recovery(RecoveryFact::path(path))
        .with_recovery(RecoveryFact::component(format!(
            "locale={locale}; component={}",
            component.as_str()
        )))
}

fn prompt_template_diagnostic(source: PromptTemplateError, path: &Path) -> SafeDiagnostic {
    let (failure, detail) = match source {
        PromptTemplateError::InvalidSyntax => (
            DiagnosticFailureKind::InvalidSyntax,
            "template_error=invalid_syntax; allowed_variables=source_language,target_language",
        ),
        PromptTemplateError::UnknownVariable => (
            DiagnosticFailureKind::InvalidValue,
            "template_error=unknown_variable; allowed_variables=source_language,target_language",
        ),
        PromptTemplateError::MissingSourceLanguage => (
            DiagnosticFailureKind::MissingRequiredValue,
            "template_error=missing_variable; variable=source_language",
        ),
        PromptTemplateError::MissingTargetLanguage => (
            DiagnosticFailureKind::MissingRequiredValue,
            "template_error=missing_variable; variable=target_language",
        ),
        PromptTemplateError::VariablesNotAllowed => (
            DiagnosticFailureKind::InvalidSyntax,
            "template_error=variables_not_allowed",
        ),
    };
    SafeDiagnostic::new(
        DiagnosticCode::PromptUnavailable,
        DiagnosticStage::CommandPreparation,
        DiagnosticSubject::path(path),
        DiagnosticReason::failure_with_detail(failure, detail),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::FixConfiguration,
    )
}

impl fmt::Debug for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTranslationExecutionBuildError")
            .field("class", &self.class)
            .field("diagnostic", &self.diagnostic)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to build the translation execution context ({})",
            self.diagnostic.code
        )
    }
}

impl Error for ProductionTranslationExecutionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &crate::application::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, ProductionCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(ProductionCommandError::pem_read)?;
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

async fn drive_project_lease<P>(
    provider: &P,
    project: &crate::rpg_maker::ProjectName,
    file_system: &SystemFileSystem,
    sqlite: &RusqliteStorage,
    cancellation: &CooperativeCancellation,
    termination_signals: &mut TerminationSignals,
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<Result<ProjectCommandLease<P::LeaseState>, ProductionCommandError>>
where
    P: ProjectCommandLeaseProvider,
    P::Error: SafeDiagnosticSource,
{
    drive_command(
        provider.acquire(project),
        termination_signals,
        || {
            cancellation.request();
            file_system.cancel_waits();
            sqlite.cancel_waits();
        },
        on_cancellation,
    )
    .await
    .map(|result| result.map_err(ProductionCommandError::project_lease))
}

struct ProjectOpeningLocation {
    projects_root: PathBuf,
    layout: RpgMakerLayout,
}

async fn drive_existing_project_opening(
    location: ProjectOpeningLocation,
    project: &crate::rpg_maker::ProjectName,
    file_system: &SystemFileSystem,
    sqlite: &RusqliteStorage,
    cancellation: &CooperativeCancellation,
    termination_signals: &mut TerminationSignals,
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<Result<OpenedProject, ProductionCommandError>> {
    let project_reader = ProjectDatabaseRecordReadingService::new(
        location.projects_root,
        location.layout,
        sqlite.clone(),
    );
    let opener = ExistingProjectOpeningService::new(
        project_reader,
        file_system.clone(),
        file_system.clone(),
    );
    drive_command(
        opener.open(project),
        termination_signals,
        || {
            cancellation.request();
            file_system.cancel_waits();
            sqlite.cancel_waits();
        },
        on_cancellation,
    )
    .await
    .map(|result| result.map_err(ProductionCommandError::existing_project_opening))
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
    termination_signals: &mut TerminationSignals,
    cancel_waits: impl FnOnce(),
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<T> {
    drive_with_signal(
        future,
        termination_signals.recv(),
        cancel_waits,
        on_cancellation,
    )
    .await
}

async fn drive_with_signal<T>(
    future: impl Future<Output = T>,
    signal: impl Future<Output = io::Result<()>>,
    cancel_waits: impl FnOnce(),
    on_cancellation: impl FnOnce(),
) -> DrivenCommand<T> {
    tokio::pin!(future);
    tokio::pin!(signal);
    tokio::select! {
        biased;
        signal = &mut signal => match signal {
            Ok(()) => {
                cancel_waits();
                on_cancellation();
                DrivenCommand::Interrupted(future.await)
            }
            Err(error) => {
                cancel_waits();
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
    pub(crate) pending_project_log: Option<PendingProjectLog>,
}

pub(crate) enum CommandRunResult {
    Succeeded(RpgMakerCommandOutput),
    Interrupted,
    Failed(ProductionCommandError),
}

impl ProductionCommandRunReport {
    fn panicked(error: ProductionCommandError) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
            // panic 展开时 ActiveProjectLog 的 runtime 已用独立终态槽完成项目日志。
            pending_project_log: None,
        }
    }

    fn failed_before_logging(error: ProductionCommandError) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
            pending_project_log: None,
        }
    }

    fn failed_before_logging_with_shutdown(
        error: ProductionCommandError,
        shutdown: ShutdownFailures,
    ) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            pending_project_log: None,
        }
    }

    fn interrupted_before_logging(shutdown: ShutdownFailures) -> Self {
        Self {
            result: CommandRunResult::Interrupted,
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            pending_project_log: None,
        }
    }

    fn construction_failed_with_shutdown_and_project_log(
        error: ProductionCommandError,
        shutdown: ShutdownFailures,
        pending_project_log: Option<PendingProjectLog>,
    ) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            pending_project_log,
        }
    }

    fn from_completion_with_project_log(
        execution: DrivenCommand<
            Result<OperationCompletion<RpgMakerCommandOutput>, ProductionCommandError>,
        >,
        shutdown: ShutdownFailures,
        pending_project_log: Option<PendingProjectLog>,
    ) -> Self {
        let shutdown_error = (!shutdown.is_empty()).then_some(shutdown);
        match execution {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output))) => Self {
                result: CommandRunResult::Succeeded(output),
                shutdown_error,
                pending_project_log,
            },
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(_)) => Self {
                result: CommandRunResult::Interrupted,
                shutdown_error,
                pending_project_log,
            },
            DrivenCommand::Finished(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
                pending_project_log,
            },
            DrivenCommand::Interrupted(Err(error)) => Self {
                result: if error.was_cancelled_wait() {
                    CommandRunResult::Interrupted
                } else {
                    CommandRunResult::Failed(error)
                },
                shutdown_error,
                pending_project_log,
            },
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(OperationCompletion::Completed(_)) => {
                        SignalOutcomeSource::CompletedStateApplied
                    }
                    Ok(OperationCompletion::Cancelled) => SignalOutcomeSource::Cancelled,
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                Self {
                    result: CommandRunResult::Failed(ProductionCommandError::signal(
                        source, outcome,
                    )),
                    shutdown_error,
                    pending_project_log,
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

fn map_init_error(
    error: InitServiceError<ProductionWorkspaceConvergenceError, Infallible>,
) -> ProductionCommandError {
    match error {
        error @ InitServiceError::ProjectLease(_) => ProductionCommandError::prevalidated_boundary(
            error,
            DiagnosticStage::Init,
            "init_project_lease_already_held",
        ),
        InitServiceError::Workspace(source) => {
            let (class, report) = init_workspace_failure_report(source);
            match class {
                InitFailureClass::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(report))
                }
                InitFailureClass::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(report))
                }
                InitFailureClass::StateAppliedFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
                }
                InitFailureClass::OutcomeUnknown => {
                    ProductionCommandError::OutcomeUnknown(Box::new(report))
                }
                InitFailureClass::Internal => ProductionCommandError::Internal(Box::new(report)),
            }
        }
    }
}

fn init_workspace_failure_report(
    source: ProductionWorkspaceConvergenceError,
) -> (InitFailureClass, FailureReport) {
    match source {
        ProjectWorkspaceConvergenceError::CandidateFailure { failure, discard } => {
            let diagnostic = init_candidate_diagnostic(&failure);
            let mut report = ProductionCommandError::report_diagnostic(failure, diagnostic);
            if let Some(discard) = discard {
                report = report.with_related_report(init_candidate_discard_failure_report(discard));
            }
            (InitFailureClass::ProjectState, report)
        }
        source => {
            let (class, diagnostic) = init_workspace_diagnostic(&source);
            (
                class,
                ProductionCommandError::report_diagnostic(source, diagnostic),
            )
        }
    }
}

fn init_candidate_discard_failure_report(
    source: DirectoryDiscardError<Box<SystemFileSystemError>>,
) -> FailureReport {
    let (staging_root, source) = source.into_parts();
    (*source)
        .into_failure_report(
            DiagnosticStage::Init,
            DiagnosticImpact::RecoveryRequired,
            DiagnosticAction::PreserveRecoveryArtifacts,
        )
        .with_primary_recovery(crate::diagnostic::RecoveryFact::path(&staging_root))
        .with_primary_recovery(crate::diagnostic::RecoveryFact::component(
            "candidate_cleanup=failed",
        ))
}

#[derive(Clone, Copy)]
enum InitFailureClass {
    ConfigurationOrInput,
    ProjectState,
    StateAppliedFinalizationFailed,
    OutcomeUnknown,
    Internal,
}

fn init_workspace_diagnostic(
    source: &ProductionWorkspaceConvergenceError,
) -> (InitFailureClass, SafeDiagnostic) {
    use InitFailureClass as Class;

    match source {
        ProjectWorkspaceConvergenceError::SourceGameRoot(source) => (
            Class::ConfigurationOrInput,
            init_directory_diagnostic(
                source,
                DiagnosticCode::CommandInput,
                DiagnosticAction::FixInput,
            ),
        ),
        ProjectWorkspaceConvergenceError::ObserveGameLayout(source) => (
            Class::ConfigurationOrInput,
            init_directory_listing_diagnostic(
                source,
                DiagnosticCode::CommandInput,
                DiagnosticAction::FixInput,
            ),
        ),
        ProjectWorkspaceConvergenceError::InvalidGameLayout {
            game_root,
            engine,
            data_relative,
            js_relative,
            core_script,
        } => (
            Class::ConfigurationOrInput,
            SafeDiagnostic::new(
                DiagnosticCode::CommandInput,
                DiagnosticStage::Init,
                DiagnosticSubject::path(game_root),
                DiagnosticReason::failure(DiagnosticFailureKind::RequirementFailed),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                "engine={engine}; data={}; js={}; core={core_script}",
                data_relative.display(),
                js_relative.display(),
            ))),
        ),
        ProjectWorkspaceConvergenceError::EngineWorkspaceRoot(source) => (
            Class::ConfigurationOrInput,
            source.safe_diagnostic(
                DiagnosticStage::Init,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
        ),
        ProjectWorkspaceConvergenceError::WorkspaceRoot(source) => (
            Class::ProjectState,
            init_directory_diagnostic(
                source,
                DiagnosticCode::ProjectState,
                DiagnosticAction::CheckProjectState,
            ),
        ),
        ProjectWorkspaceConvergenceError::InspectExistingDatabase(source) => (
            Class::ProjectState,
            init_database_inspection_diagnostic(source),
        ),
        ProjectWorkspaceConvergenceError::MissingInitialSettings(settings) => {
            let mut diagnostic = SafeDiagnostic::new(
                DiagnosticCode::CommandInput,
                DiagnosticStage::Init,
                DiagnosticSubject::component("initial project settings"),
                DiagnosticReason::failure(DiagnosticFailureKind::MissingRequiredValue),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            );
            for setting in settings {
                diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::component(
                    init_setting_flag(*setting),
                ));
            }
            (Class::ConfigurationOrInput, diagnostic)
        }
        ProjectWorkspaceConvergenceError::ObserveWorkspaceStructure(source) => (
            Class::ProjectState,
            init_directory_listing_diagnostic(
                source,
                DiagnosticCode::ProjectState,
                DiagnosticAction::CheckProjectState,
            ),
        ),
        ProjectWorkspaceConvergenceError::ObserveExistingSource(source) => (
            Class::ProjectState,
            init_fingerprint_diagnostic(source, DiagnosticAction::CheckProjectState),
        ),
        ProjectWorkspaceConvergenceError::ObserveInputSource(source) => (
            Class::ConfigurationOrInput,
            init_fingerprint_diagnostic(source, DiagnosticAction::FixInput),
        ),
        ProjectWorkspaceConvergenceError::InvalidStageRequest(source) => {
            (Class::Internal, init_stage_request_diagnostic(source))
        }
        ProjectWorkspaceConvergenceError::Prepare(source) => {
            (Class::ProjectState, init_prepare_diagnostic(source))
        }
        ProjectWorkspaceConvergenceError::CandidateFailure { failure, .. } => {
            (Class::ProjectState, init_candidate_diagnostic(failure))
        }
        ProjectWorkspaceConvergenceError::CancellationCleanup(source) => {
            (Class::ProjectState, init_discard_diagnostic(source))
        }
        ProjectWorkspaceConvergenceError::Publish(source) => init_publish_diagnostic(source),
    }
}

const fn init_setting_flag(setting: MissingInitialProjectSetting) -> &'static str {
    match setting {
        MissingInitialProjectSetting::SourceLanguage => "--source-language",
        MissingInitialProjectSetting::TargetLanguage => "--target-language",
        MissingInitialProjectSetting::DialogueMaxFullwidthChars => "--dialogue-max-fullwidth-chars",
        MissingInitialProjectSetting::ScrollingTextMaxFullwidthChars => {
            "--scrolling-text-max-fullwidth-chars"
        }
        MissingInitialProjectSetting::HelpDescriptionMaxFullwidthChars => {
            "--help-description-max-fullwidth-chars"
        }
    }
}

fn init_directory_diagnostic(
    source: &ResolveDirectoryError<SystemFileSystemError>,
    code: DiagnosticCode,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    match source {
        ResolveDirectoryError::NotFound { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ResolveDirectoryError::NotDirectory { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ResolveDirectoryError::Io { path, source } => source
            .safe_diagnostic(DiagnosticStage::Init, DiagnosticImpact::Unchanged, action)
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
    }
}

fn init_directory_listing_diagnostic(
    source: &ListDirectoryError<SystemFileSystemError>,
    code: DiagnosticCode,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    match source {
        ListDirectoryError::NotFound { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ListDirectoryError::NotDirectory { path } => SafeDiagnostic::new(
            code,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ListDirectoryError::Io { path, source } => source
            .safe_diagnostic(DiagnosticStage::Init, DiagnosticImpact::Unchanged, action)
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
    }
}

fn init_fingerprint_diagnostic(
    source: &DirectoryTreeFingerprintError<Box<SystemFileSystemError>>,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    match source {
        DirectoryTreeFingerprintError::NotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            action,
        ),
        DirectoryTreeFingerprintError::NotDirectory { path } => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            action,
        ),
        DirectoryTreeFingerprintError::ChangedDuringObservation { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::FileIdentityChanged),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        ),
        DirectoryTreeFingerprintError::Failed { path, source } => source
            .safe_diagnostic_source(DiagnosticStage::Init, DiagnosticImpact::Unchanged, action)
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
    }
}

fn init_stage_request_diagnostic(source: &DirectoryStageRequestError) -> SafeDiagnostic {
    let mut diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Init,
        DiagnosticSubject::operation("prepare_workspace_candidate"),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::ReportBug,
    );
    match source {
        DirectoryStageRequestError::EmptyTargetRoot => {
            diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::component(
                "invalid_field=target_root",
            ));
        }
        DirectoryStageRequestError::EmptySourceDirectory => {
            diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::component(
                "invalid_field=source_directory",
            ));
        }
        DirectoryStageRequestError::EmptySourceMappings => {
            diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::component(
                "invalid_field=source_mappings",
            ));
        }
        DirectoryStageRequestError::InvalidRelativePath { path } => {
            diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::path(path));
        }
        DirectoryStageRequestError::OverlappingSourceTargets { first, second }
        | DirectoryStageRequestError::OverlappingOverlays { first, second }
        | DirectoryStageRequestError::OverlappingEmptyDirectories { first, second } => {
            diagnostic = diagnostic
                .with_recovery(crate::diagnostic::RecoveryFact::path(first))
                .with_recovery(crate::diagnostic::RecoveryFact::path(second));
        }
        DirectoryStageRequestError::OverlayOutsideSourceMappings { relative_file } => {
            diagnostic =
                diagnostic.with_recovery(crate::diagnostic::RecoveryFact::path(relative_file));
        }
        DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
            empty_directory,
            source_target,
        } => {
            diagnostic = diagnostic
                .with_recovery(crate::diagnostic::RecoveryFact::path(empty_directory))
                .with_recovery(crate::diagnostic::RecoveryFact::path(source_target));
        }
        DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
            empty_directory,
            overlay,
        } => {
            diagnostic = diagnostic
                .with_recovery(crate::diagnostic::RecoveryFact::path(empty_directory))
                .with_recovery(crate::diagnostic::RecoveryFact::path(overlay));
        }
    }
    diagnostic
}

fn init_database_inspection_diagnostic(
    source: &ProjectDatabaseInspectionError<SqliteRuntimeError>,
) -> SafeDiagnostic {
    match source {
        ProjectDatabaseInspectionError::DatabaseNotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ProjectDatabaseInspectionError::ReadDatabase {
            path,
            stage,
            query_ids,
            source,
        } => {
            let mut diagnostic = source.safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            );
            diagnostic = diagnostic
                .with_recovery(crate::diagnostic::RecoveryFact::path(path))
                .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                    "database_stage={stage}"
                )));
            for query_id in query_ids {
                diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::component(
                    format!("database_query_id={query_id}"),
                ));
            }
            diagnostic
        }
        ProjectDatabaseInspectionError::InvalidDatabase { path, reason } => {
            let mut diagnostic = SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::Init,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::StateMismatch,
                    reason.safe_fact(),
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            );
            if let Some(fact) = reason.recovery_fact() {
                diagnostic = diagnostic.with_recovery(fact.clone());
            }
            diagnostic
        }
    }
}

fn init_database_create_diagnostic(
    source: &ProjectDatabaseCreateError<SqliteRuntimeError>,
) -> SafeDiagnostic {
    match source {
        ProjectDatabaseCreateError::AlreadyExists { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ProjectDatabaseCreateError::NotCreated { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        ProjectDatabaseCreateError::OutcomeUnknown { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        ProjectDatabaseCreateError::ResidualArtifact { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::RecoveryRequired,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
    }
}

fn init_database_reconciliation_diagnostic(
    source: &ProjectDatabaseReconciliationError<SqliteRuntimeError, SqliteRuntimeError>,
) -> SafeDiagnostic {
    match source {
        ProjectDatabaseReconciliationError::Inspection(source) => {
            init_database_inspection_diagnostic(source)
        }
        ProjectDatabaseReconciliationError::ConcurrentModification { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::FileIdentityChanged),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        ),
        ProjectDatabaseReconciliationError::DatabaseNotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ProjectDatabaseReconciliationError::NotCommitted { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path))
            .with_recovery(crate::diagnostic::RecoveryFact::component(
                "database_stage=reconcile_project_database",
            ))
            .with_recovery(crate::diagnostic::RecoveryFact::transaction("rolled_back")),
        ProjectDatabaseReconciliationError::OutcomeUnknown { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::Init,
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path))
            .with_recovery(crate::diagnostic::RecoveryFact::component(
                "database_stage=reconcile_project_database",
            )),
    }
}

fn init_snapshot_database_diagnostic(
    source: &SnapshotDatabaseError<SqliteRuntimeError>,
) -> SafeDiagnostic {
    match source {
        SnapshotDatabaseError::SourceNotFound => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::operation("snapshot_project_database"),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        SnapshotDatabaseError::DestinationAlreadyExists => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::operation("snapshot_project_database"),
            DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        SnapshotDatabaseError::NotCreated(source) => source.safe_diagnostic_source(
            DiagnosticStage::Init,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        SnapshotDatabaseError::ResidualArtifact(source) => source.safe_diagnostic_source(
            DiagnosticStage::Init,
            DiagnosticImpact::RecoveryRequired,
            DiagnosticAction::PreserveRecoveryArtifacts,
        ),
        SnapshotDatabaseError::OutcomeUnknown(source) => source.safe_diagnostic_source(
            DiagnosticStage::Init,
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::PreserveRecoveryArtifacts,
        ),
    }
}

fn init_candidate_diagnostic(
    source: &ProjectWorkspaceCandidateFailure<
        ProjectDatabaseCreateError<SqliteRuntimeError>,
        SqliteRuntimeError,
        ProjectDatabaseReconciliationError<SqliteRuntimeError, SqliteRuntimeError>,
        Box<SystemFileSystemError>,
    >,
) -> SafeDiagnostic {
    match source {
        ProjectWorkspaceCandidateFailure::FingerprintCandidate(source) => {
            init_fingerprint_diagnostic(source, DiagnosticAction::CheckProjectState)
        }
        ProjectWorkspaceCandidateFailure::CreateDatabase(source) => {
            init_database_create_diagnostic(source)
        }
        ProjectWorkspaceCandidateFailure::SnapshotDatabase(source) => {
            init_snapshot_database_diagnostic(source)
        }
        ProjectWorkspaceCandidateFailure::ReconcileDatabase(source) => {
            init_database_reconciliation_diagnostic(source)
        }
    }
}

fn init_prepare_diagnostic(
    source: &DirectoryPrepareError<Box<SystemFileSystemError>>,
) -> SafeDiagnostic {
    match source {
        DirectoryPrepareError::NotPrepared {
            target_root,
            source,
            cleanup_failure,
        } => with_staging_cleanup(
            source
                .safe_diagnostic(
                    DiagnosticStage::Init,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(target_root)),
            cleanup_failure.as_ref(),
        ),
    }
}

fn init_discard_diagnostic(
    source: &DirectoryDiscardError<Box<SystemFileSystemError>>,
) -> SafeDiagnostic {
    source
        .source()
        .safe_diagnostic(
            DiagnosticStage::Init,
            DiagnosticImpact::RecoveryRequired,
            DiagnosticAction::PreserveRecoveryArtifacts,
        )
        .with_recovery(crate::diagnostic::RecoveryFact::path(source.staging_root()))
}

fn init_publish_diagnostic(
    source: &DirectoryPublishError<Box<SystemFileSystemError>>,
) -> (InitFailureClass, SafeDiagnostic) {
    use InitFailureClass as Class;

    match source {
        DirectoryPublishError::TargetAlreadyExists {
            target_root,
            cleanup_failure,
        } => (
            Class::ProjectState,
            with_staging_cleanup(
                SafeDiagnostic::new(
                    DiagnosticCode::ProjectState,
                    DiagnosticStage::Publication,
                    DiagnosticSubject::path(target_root),
                    DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                ),
                cleanup_failure.as_ref(),
            ),
        ),
        DirectoryPublishError::TargetMissing {
            target_root,
            cleanup_failure,
        } => (
            Class::ProjectState,
            with_staging_cleanup(
                SafeDiagnostic::new(
                    DiagnosticCode::ProjectState,
                    DiagnosticStage::Publication,
                    DiagnosticSubject::path(target_root),
                    DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                ),
                cleanup_failure.as_ref(),
            ),
        ),
        DirectoryPublishError::TargetNotDirectory {
            target_root,
            cleanup_failure,
        } => (
            Class::ProjectState,
            with_staging_cleanup(
                SafeDiagnostic::new(
                    DiagnosticCode::ProjectState,
                    DiagnosticStage::Publication,
                    DiagnosticSubject::path(target_root),
                    DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                ),
                cleanup_failure.as_ref(),
            ),
        ),
        DirectoryPublishError::NotAttempted {
            target_root,
            source,
            cleanup_failure,
        }
        | DirectoryPublishError::NotPublished {
            target_root,
            source,
            cleanup_failure,
        } => (
            Class::ProjectState,
            with_staging_cleanup(
                source
                    .safe_diagnostic(
                        DiagnosticStage::Publication,
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::CheckProjectState,
                    )
                    .with_recovery(crate::diagnostic::RecoveryFact::path(target_root)),
                cleanup_failure.as_ref(),
            ),
        ),
        DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path,
            source,
        } => (
            Class::StateAppliedFinalizationFailed,
            source
                .safe_diagnostic(
                    DiagnosticStage::Publication,
                    DiagnosticImpact::StateAppliedFinalizationFailed,
                    DiagnosticAction::PreserveRecoveryArtifacts,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(target_root))
                .with_recovery(crate::diagnostic::RecoveryFact::path(residual_path)),
        ),
        DirectoryPublishError::RecoveryRequired {
            target_root,
            recovery_artifacts,
            source,
        } => (
            Class::OutcomeUnknown,
            with_recovery_paths(
                source
                    .safe_diagnostic(
                        DiagnosticStage::Publication,
                        DiagnosticImpact::RecoveryRequired,
                        DiagnosticAction::PreserveRecoveryArtifacts,
                    )
                    .with_recovery(crate::diagnostic::RecoveryFact::path(target_root)),
                recovery_artifacts,
            ),
        ),
        DirectoryPublishError::OutcomeUnknown {
            target_root,
            recovery_artifacts,
            source,
        } => (
            Class::OutcomeUnknown,
            with_recovery_paths(
                source
                    .safe_diagnostic(
                        DiagnosticStage::Publication,
                        DiagnosticImpact::OutcomeUnknown,
                        DiagnosticAction::PreserveRecoveryArtifacts,
                    )
                    .with_recovery(crate::diagnostic::RecoveryFact::path(target_root)),
                recovery_artifacts,
            ),
        ),
    }
}

fn with_staging_cleanup<E>(
    mut diagnostic: SafeDiagnostic,
    cleanup: Option<&StagingCleanupFailure<E>>,
) -> SafeDiagnostic {
    if let Some(cleanup) = cleanup {
        diagnostic = diagnostic
            .with_recovery(crate::diagnostic::RecoveryFact::path(
                cleanup.residual_path(),
            ))
            .with_recovery(crate::diagnostic::RecoveryFact::component(
                "candidate_cleanup=failed",
            ));
    }
    diagnostic
}

fn with_recovery_paths(mut diagnostic: SafeDiagnostic, paths: &[PathBuf]) -> SafeDiagnostic {
    for path in paths {
        diagnostic = diagnostic.with_recovery(crate::diagnostic::RecoveryFact::path(path));
    }
    diagnostic
}

trait ExtractSafeDiagnostic {
    fn safe_diagnostic(&self) -> SafeDiagnostic;
}

trait ExtractFailureReport: Error + Send + Sync + Sized + 'static {
    fn into_extract_failure_report(self) -> FailureReport;
}

impl<RE, SE, CE> ExtractSafeDiagnostic for BuiltInExtractionError<RE, SE, CE>
where
    RE: SafeDiagnosticSource,
    SE: SafeDiagnosticSource,
    crate::execution::cpu::CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        BuiltInExtractionError::safe_diagnostic(self)
    }
}

impl<O, R, S> ExtractSafeDiagnostic
    for LuaExtractionError<crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<O, R>, S>
where
    O: SafeDiagnosticSource,
    R: SafeDiagnosticSource,
    S: SafeDiagnosticSource,
{
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        LuaExtractionError::safe_diagnostic(self)
    }
}

impl<RE, SE, CE> ExtractFailureReport for BuiltInExtractionError<RE, SE, CE>
where
    RE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    CE: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    fn into_extract_failure_report(self) -> FailureReport {
        let diagnostic = ExtractSafeDiagnostic::safe_diagnostic(&self);
        match self {
            BuiltInExtractionError::Persist(source) => source
                .into_failure_report(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_primary_recovery(crate::diagnostic::RecoveryFact::component("owner=builtin")),
            source => FailureReport::new(ReportedFailure::new(diagnostic, source)),
        }
    }
}

impl<DE, SE, CE> ExtractFailureReport for RulesExtractionError<DE, SE, CE>
where
    DE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    CE: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    fn into_extract_failure_report(self) -> FailureReport {
        RulesExtractionError::into_failure_report(self)
    }
}

impl<O, R, S> ExtractFailureReport
    for LuaExtractionError<crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<O, R>, S>
where
    O: Error + SafeDiagnosticSource + Send + Sync + 'static,
    R: Error + SafeDiagnosticSource + Send + Sync + 'static,
    S: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    fn into_extract_failure_report(self) -> FailureReport {
        LuaExtractionError::into_failure_report(self)
    }
}

fn map_extract_error<OE, BE, RE, LE, PE>(
    error: ExtractServiceError<OE, BE, RE, LE, PE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    BE: ExtractFailureReport,
    RE: ExtractFailureReport,
    LE: ExtractFailureReport,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ ExtractServiceError::ProjectLease(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Extract,
                "extract_project_lease_already_held",
            )
        }
        error @ ExtractServiceError::OpenProject(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Extract,
                "extract_project_already_opened",
            )
        }
        ExtractServiceError::BuiltIn(source) => {
            map_project_failure_report(source.into_extract_failure_report())
        }
        ExtractServiceError::Rules {
            rules_path: _,
            source,
        } => map_project_failure_report(source.into_extract_failure_report()),
        ExtractServiceError::Lua {
            script_path: _,
            source,
        } => map_project_failure_report(source.into_extract_failure_report()),
    }
}

trait ProductionExternalModelFailure {
    fn into_external_model_failure(self) -> ProductionCommandError;
}

trait ProductionTranslationResultStorageFailure: Error + Send + Sync + Sized + 'static {
    fn into_result_storage_failure_report(self) -> FailureReport;
}

impl<S, C> ProductionTranslationResultStorageFailure
    for RpgMakerStandardTranslationResultStorageError<S, C>
where
    S: Error + SafeDiagnosticSource + Send + Sync + 'static,
    C: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn into_result_storage_failure_report(self) -> FailureReport {
        self.into_failure_report()
    }
}

impl<R, P, E, S> ProductionExternalModelFailure
    for crate::rpg_maker::translate::standard::StandardTranslationServiceError<
        R,
        P,
        RpgMakerStandardTranslationTaskExecutionError<OpenAiChatCompletionError, E>,
        S,
    >
where
    R: Error + SafeDiagnosticSource + Send + Sync + 'static,
    P: Error + SafeDiagnosticSource + Send + Sync + 'static,
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
    S: ProductionTranslationResultStorageFailure,
{
    fn into_external_model_failure(self) -> ProductionCommandError {
        use crate::rpg_maker::translate::standard::StandardTranslationServiceError as StandardError;

        match self {
            StandardError::ReadAssets(source) => {
                let diagnostic = source.safe_diagnostic_source(
                    DiagnosticStage::Translate,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                );
                map_project_diagnostic(source, diagnostic)
            }
            StandardError::PlanTasks(source) => {
                let diagnostic = source.safe_diagnostic_source(
                    DiagnosticStage::Translate,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::FixInput,
                );
                if matches!(
                    diagnostic.action,
                    DiagnosticAction::FixInput
                        | DiagnosticAction::FixConfiguration
                        | DiagnosticAction::CheckPathAndPermissions
                ) {
                    ProductionCommandError::ConfigurationOrInput(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                } else {
                    map_project_diagnostic(source, diagnostic)
                }
            }
            StandardError::ApplyPreparation(source) => {
                map_project_failure_report(source.into_result_storage_failure_report())
            }
            StandardError::ExecuteTask { task_index, source } => match source {
                RpgMakerStandardTranslationTaskExecutionError::FatalRequest { attempt, source } => {
                    let diagnostic = source
                        .safe_diagnostic(None, DiagnosticImpact::ProgressPreserved)
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task={task_index}; attempt={attempt}"
                        )));
                    ProductionCommandError::ExternalModel(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
                RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                    attempt,
                    source,
                } => {
                    let diagnostic = source
                        .safe_diagnostic_source(
                            DiagnosticStage::ModelRequest,
                            DiagnosticImpact::ProgressPreserved,
                            DiagnosticAction::CheckModelService,
                        )
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task={task_index}; attempt={attempt}"
                        )));
                    let execution_error: RpgMakerStandardTranslationTaskExecutionError<
                        OpenAiChatCompletionError,
                        E,
                    > = RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                        attempt,
                        source,
                    };
                    map_project_diagnostic(execution_error, diagnostic)
                }
                source @ RpgMakerStandardTranslationTaskExecutionError::RetryWaitCancelled {
                    attempt,
                } => {
                    let diagnostic = SafeDiagnostic::new(
                        DiagnosticCode::ModelRequest,
                        DiagnosticStage::ModelRequest,
                        DiagnosticSubject::component("LLM retry wait"),
                        DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                        DiagnosticImpact::ProgressPreserved,
                        DiagnosticAction::Retry,
                    )
                    .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                        "task={task_index}; attempt={attempt}"
                    )));
                    ProductionCommandError::ExternalModel(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
                RpgMakerStandardTranslationTaskExecutionError::InternalInvariant { invariant } => {
                    let diagnostic = invariant
                        .safe_diagnostic(
                            DiagnosticStage::Translate,
                            DiagnosticImpact::ProgressPreserved,
                        )
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task_ordinal={task_index}"
                        )));
                    let source: RpgMakerStandardTranslationTaskExecutionError<
                        OpenAiChatCompletionError,
                        E,
                    > = RpgMakerStandardTranslationTaskExecutionError::InternalInvariant {
                        invariant,
                    };
                    ProductionCommandError::Internal(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
            },
            StandardError::CommitTask { task_index, source } => {
                let report = source
                    .into_result_storage_failure_report()
                    .with_primary_recovery(crate::diagnostic::RecoveryFact::component(format!(
                        "task={task_index}"
                    )));
                map_project_failure_report(report)
            }
            source @ StandardError::InvalidTaskResultSequence {
                expected_task_index,
                actual_task_index,
            } => {
                let diagnostic = SafeDiagnostic::new(
                    DiagnosticCode::StateFinalizationFailed,
                    DiagnosticStage::Translate,
                    DiagnosticSubject::operation("translation result ordering"),
                    DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                    DiagnosticImpact::StateAppliedFinalizationFailed,
                    DiagnosticAction::ReportBug,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                    "expected_task={expected_task_index}; actual_task={actual_task_index:?}"
                )));
                ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(
                    ProductionCommandError::report_diagnostic(source, diagnostic),
                ))
            }
            StandardError::FinalizeResultStore(source) => {
                map_project_failure_report(source.into_result_storage_failure_report())
            }
            StandardError::OperationAndFinalization {
                primary,
                finalization,
            } => primary
                .into_external_model_failure()
                .with_related_finalization_report(
                    finalization.into_result_storage_failure_report(),
                ),
        }
    }
}

fn map_project_diagnostic(
    source: impl Error + Send + Sync + 'static,
    diagnostic: SafeDiagnostic,
) -> ProductionCommandError {
    let report = ProductionCommandError::report_diagnostic(source, diagnostic);
    map_project_failure_report(report)
}

fn map_project_failure_report(report: FailureReport) -> ProductionCommandError {
    let impact = report.primary.public().impact;
    let action = report.primary.public().action;
    let related_outcome_unknown = report
        .related
        .iter()
        .any(|failure| failure.public().impact == DiagnosticImpact::OutcomeUnknown);
    let related_recovery_required = report.related.iter().any(|failure| {
        matches!(
            failure.public().impact,
            DiagnosticImpact::StateAppliedFinalizationFailed | DiagnosticImpact::RecoveryRequired
        )
    });
    if impact == DiagnosticImpact::OutcomeUnknown || related_outcome_unknown {
        ProductionCommandError::OutcomeUnknown(Box::new(report))
    } else if matches!(
        impact,
        DiagnosticImpact::StateAppliedFinalizationFailed | DiagnosticImpact::RecoveryRequired
    ) || related_recovery_required
    {
        ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
    } else if action == DiagnosticAction::ReportBug {
        ProductionCommandError::Internal(Box::new(report))
    } else {
        ProductionCommandError::ProjectState(Box::new(report))
    }
}

fn map_translate_error<RE, BE, SE, LE, PE>(
    error: TranslateServiceError<RE, BE, SE, LE, PE>,
    map_build: impl FnOnce(BE) -> ProductionCommandError,
    map_standard: impl FnOnce(SE) -> ProductionCommandError,
    map_lua: impl FnOnce(PathBuf, LE) -> ProductionCommandError,
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
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Translate,
                "translate_project_lease_already_held",
            )
        }
        error @ TranslateServiceError::ReadProject { .. } => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Translate,
                "translate_project_already_opened",
            )
        }
        TranslateServiceError::BuildExecution(source) => map_build(source),
        TranslateServiceError::Standard { source } => map_standard(source),
        error @ TranslateServiceError::MissingResolvedTranslationSemantics => {
            let diagnostic = SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::component("resolved translation semantics"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            );
            ProductionCommandError::Internal(Box::new(ProductionCommandError::report_diagnostic(
                error, diagnostic,
            )))
        }
        TranslateServiceError::Lua {
            script_path,
            source,
        } => map_lua(script_path, source),
    }
}

fn map_translate_lua_error<O, R>(
    script_path: PathBuf,
    source: LuaTranslationError<
        crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<O, R>,
    >,
) -> ProductionCommandError
where
    O: Error + SafeDiagnosticSource + Send + Sync + 'static,
    R: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    match source {
        LuaTranslationError::ExecuteHost {
            script_path,
            source,
        } => map_project_failure_report(source.into_failure_report(
            DiagnosticStage::Translate,
            &script_path,
            DiagnosticImpact::ProgressPreserved,
        )),
        source @ LuaTranslationError::UnexpectedManagedOutcome => {
            let diagnostic = SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(&script_path),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            );
            map_project_diagnostic(source, diagnostic)
        }
    }
}

fn map_write_back_error<OE, SE, PE, LE, KE>(
    error: WriteBackServiceError<OE, SE, PE, LE, KE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    PE: Error + WriteBackPublishingDiagnostic + Send + Sync + 'static,
    LE: Error + WriteBackLuaDiagnostic + Send + Sync + 'static,
    KE: Error + Send + Sync + 'static,
{
    match error {
        WriteBackServiceError::ProjectLease(source) => {
            let project = match &source {
                ProjectCommandLeaseError::Unavailable { project, .. } => project.to_string(),
            };
            let diagnostic = SafeDiagnostic::new(
                DiagnosticCode::ProjectUnavailable,
                DiagnosticStage::ProjectOpening,
                DiagnosticSubject::Project { name: project },
                DiagnosticReason::failure(DiagnosticFailureKind::Busy),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::RetryAfterResolvingContention,
            );
            ProductionCommandError::ProjectUnavailable(Box::new(
                ProductionCommandError::report_diagnostic(source, diagnostic),
            ))
        }
        WriteBackServiceError::CancellationDiscard {
            candidate_root: _,
            discard,
        } => {
            let report = discard.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::RecoveryRequired,
            );
            ProductionCommandError::ProjectState(Box::new(report))
        }
        WriteBackServiceError::OpenProject(source) => {
            let diagnostic = SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::ProjectOpening,
                DiagnosticSubject::operation("open_write_back_project"),
                DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            );
            ProductionCommandError::ProjectState(Box::new(
                ProductionCommandError::report_diagnostic(source, diagnostic),
            ))
        }
        WriteBackServiceError::Standard(source) => {
            let diagnostic = source.safe_diagnostic_source(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            );
            let report = ProductionCommandError::report_diagnostic(source, diagnostic);
            if report.primary.public().action == DiagnosticAction::ReportBug {
                ProductionCommandError::Internal(Box::new(report))
            } else {
                ProductionCommandError::ProjectState(Box::new(report))
            }
        }
        WriteBackServiceError::PrepareCandidate(source) => {
            let report = source.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
            );
            if report.primary.public().action == DiagnosticAction::ReportBug {
                ProductionCommandError::Internal(Box::new(report))
            } else {
                ProductionCommandError::ProjectState(Box::new(report))
            }
        }
        WriteBackServiceError::Lua {
            script_path,
            candidate_root,
            source,
        } => {
            let _ = (script_path, candidate_root);
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::LuaAndDiscard {
            script_path,
            candidate_root,
            source,
            discard,
        } => {
            let _ = (script_path, candidate_root);
            let report = source.into_write_back_failure_report();
            let discard_report = discard.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::RecoveryRequired,
            );
            let report = append_related_report(report, discard_report);
            map_project_failure_report(report)
        }
        WriteBackServiceError::ValidateCandidate {
            candidate_root: _,
            source,
        } => {
            let report = source.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
            );
            ProductionCommandError::ProjectState(Box::new(report))
        }
        WriteBackServiceError::ValidateCandidateAndDiscard {
            candidate_root,
            source,
            discard,
        } => {
            let _ = candidate_root;
            let report = source.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
            );
            let discard_report = discard.into_write_back_failure_report(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::RecoveryRequired,
            );
            let report = append_related_report(report, discard_report);
            ProductionCommandError::ProjectState(Box::new(report))
        }
        WriteBackServiceError::Publish { state, source } => {
            let (impact, variant) = match &state {
                WriteBackPublishFailureState::NotPublished { .. } => (
                    DiagnosticImpact::Unchanged,
                    WriteBackReportVariant::ProjectState,
                ),
                WriteBackPublishFailureState::PublishedWithResiduals { .. } => (
                    DiagnosticImpact::StateAppliedFinalizationFailed,
                    WriteBackReportVariant::StateAppliedFinalizationFailed,
                ),
                WriteBackPublishFailureState::RecoveryRequired { .. } => (
                    DiagnosticImpact::RecoveryRequired,
                    WriteBackReportVariant::OutcomeUnknown,
                ),
                WriteBackPublishFailureState::OutcomeUnknown { .. } => (
                    DiagnosticImpact::OutcomeUnknown,
                    WriteBackReportVariant::OutcomeUnknown,
                ),
            };
            let _ = state;
            let report =
                source.into_write_back_failure_report(DiagnosticStage::Publication, impact);
            match variant {
                WriteBackReportVariant::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(report))
                }
                WriteBackReportVariant::StateAppliedFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
                }
                WriteBackReportVariant::OutcomeUnknown => {
                    ProductionCommandError::OutcomeUnknown(Box::new(report))
                }
            }
        }
    }
}

fn append_related_report(mut primary: FailureReport, related: FailureReport) -> FailureReport {
    primary.related.push(related.primary);
    primary.related.extend(related.related);
    primary
}

enum WriteBackReportVariant {
    ProjectState,
    StateAppliedFinalizationFailed,
    OutcomeUnknown,
}

fn project_database_read_diagnostic(
    source: &ProjectDatabaseReadError<SqliteRuntimeError>,
) -> SafeDiagnostic {
    match source {
        ProjectDatabaseReadError::DatabaseNotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectUnavailable,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ProjectDatabaseReadError::ReadDatabase {
            path,
            stage,
            query_id,
            source,
        } => source
            .safe_diagnostic(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path))
            .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                "database_stage={stage}"
            )))
            .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                "database_query_id={query_id}"
            ))),
        ProjectDatabaseReadError::InvalidMetadata { path, reason } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::StateMismatch,
                reason.safe_fact(),
            ),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
    }
}

fn project_directory_diagnostic(
    source: &ResolveDirectoryError<SystemFileSystemError>,
) -> SafeDiagnostic {
    match source {
        ResolveDirectoryError::NotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ResolveDirectoryError::NotDirectory { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        ResolveDirectoryError::Io { path, source } => source
            .safe_diagnostic(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
    }
}

fn project_fingerprint_diagnostic(
    source: &DirectoryTreeFingerprintError<Box<SystemFileSystemError>>,
) -> SafeDiagnostic {
    match source {
        DirectoryTreeFingerprintError::NotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        DirectoryTreeFingerprintError::NotDirectory { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        DirectoryTreeFingerprintError::ChangedDuringObservation { path } => SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        ),
        DirectoryTreeFingerprintError::Failed { path, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
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
        DrivenCommand::Interrupted(Err(error))
            if error.was_cancelled_wait() && shutdown.is_empty() =>
        {
            ProjectLogRunOutcome::Cancelled
        }
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
    ConfigurationOrInput(Box<FailureReport>),
    ProjectUnavailable(Box<FailureReport>),
    ProjectState(Box<FailureReport>),
    ExternalModel(Box<FailureReport>),
    ResultAppliedButRunPlanNotSaved(Box<FailureReport>),
    RunPlanOutcomeUnknown(Box<FailureReport>),
    StateAppliedButFinalizationFailed(Box<FailureReport>),
    OutcomeUnknown(Box<FailureReport>),
    Internal(Box<FailureReport>),
    Signal(Box<FailureReport>),
}

impl ProductionCommandError {
    fn report(
        source: impl Error + Send + Sync + 'static,
        code: DiagnosticCode,
        stage: DiagnosticStage,
        subject: DiagnosticSubject,
        reason: DiagnosticReason,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> FailureReport {
        FailureReport::new(ReportedFailure::new(
            SafeDiagnostic::new(code, stage, subject, reason, impact, action),
            source,
        ))
    }

    fn report_diagnostic(
        source: impl Error + Send + Sync + 'static,
        diagnostic: SafeDiagnostic,
    ) -> FailureReport {
        FailureReport::new(ReportedFailure::new(diagnostic, source))
    }

    pub(crate) fn stdout_write(source: io::Error) -> Self {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::StateFinalizationFailed,
            DiagnosticStage::ProcessOutput,
            DiagnosticSubject::operation("write_stdout"),
            "write_stdout",
            &source,
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::Retry,
        );
        Self::StateAppliedButFinalizationFailed(Box::new(Self::report_diagnostic(
            source, diagnostic,
        )))
    }

    fn into_failure_report(self) -> FailureReport {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => *report,
        }
    }

    fn with_related_finalization_report(self, related: FailureReport) -> Self {
        let primary_outcome_unknown = matches!(
            &self,
            Self::OutcomeUnknown(_) | Self::RunPlanOutcomeUnknown(_)
        );
        let related_outcome_unknown = related
            .public_diagnostics()
            .any(|diagnostic| diagnostic.impact == DiagnosticImpact::OutcomeUnknown);
        let report = self.into_failure_report().with_related_report(related);
        if primary_outcome_unknown || related_outcome_unknown {
            Self::OutcomeUnknown(Box::new(report))
        } else {
            Self::StateAppliedButFinalizationFailed(Box::new(report))
        }
    }

    #[cfg(test)]
    fn configuration_or_input(source: impl Error + Send + Sync + 'static) -> Self {
        Self::ConfigurationOrInput(Box::new(Self::report(
            source,
            DiagnosticCode::CommandInput,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::command("rpg_maker"),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        )))
    }

    fn configuration_load(source: ConfigurationLoadError) -> Self {
        let diagnostic = match &source {
            ConfigurationLoadError::Open { path, source } => SafeDiagnostic::io(
                DiagnosticCode::ConfigurationOpen,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                "open_configuration",
                source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            ConfigurationLoadError::NotAFile { path } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationNotFile,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ConfigurationLoadError::Read { path, source } => SafeDiagnostic::io(
                DiagnosticCode::ConfigurationRead,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                "read_configuration",
                source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            ConfigurationLoadError::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidUtf8,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: u64::try_from(*valid_up_to).unwrap_or(u64::MAX),
                    error_len: error_len.map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ConfigurationLoadError::InvalidToml {
                path,
                location,
                resource,
                reason,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidToml,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidToml {
                    line: location.map(|value| u64::try_from(value.line()).unwrap_or(u64::MAX)),
                    column: location.map(|value| u64::try_from(value.column()).unwrap_or(u64::MAX)),
                    resource: crate::user_text::sanitize_user_text(resource),
                    classification: crate::user_text::sanitize_user_text(reason),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ConfigurationLoadError::InvalidValue(source) => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidValue,
                DiagnosticStage::Configuration,
                DiagnosticSubject::field(source.field()),
                DiagnosticReason::InvalidConfigurationValue {
                    rule: source.reason().clone(),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ConfigurationLoadError::InvalidValueAtPath { path, source } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidValue,
                DiagnosticStage::Configuration,
                DiagnosticSubject::field(source.field()),
                DiagnosticReason::InvalidConfigurationValue {
                    rule: source.reason().clone(),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
            ConfigurationLoadError::TranslationProfileNotFound { path, profile_id } => {
                SafeDiagnostic::new(
                    DiagnosticCode::ConfigurationProfileNotFound,
                    DiagnosticStage::Configuration,
                    DiagnosticSubject::profile(profile_id),
                    DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::FixConfiguration,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path))
            }
            ConfigurationLoadError::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationProfileConflict,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::ConflictingValues),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                "explicit_profile={}; requested_profile={}",
                crate::user_text::sanitize_user_text(explicit_profile),
                crate::user_text::sanitize_user_text(requested_profile)
            ))),
        };
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn input_directory(source: ResolveDirectoryError<SystemFileSystemError>) -> Self {
        let diagnostic = match &source {
            ResolveDirectoryError::NotFound { path } => SafeDiagnostic::new(
                DiagnosticCode::CommandInput,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            ResolveDirectoryError::NotDirectory { path } => SafeDiagnostic::new(
                DiagnosticCode::CommandInput,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            ResolveDirectoryError::Io { path, source } => source
                .safe_diagnostic(
                    DiagnosticStage::CommandPreparation,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        };
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn run_plan_resolution(source: RunPlanResolutionError) -> Self {
        let (subject, reason) = match &source {
            RunPlanResolutionError::InitPathRequired => (
                DiagnosticSubject::field("--path"),
                DiagnosticReason::failure(DiagnosticFailureKind::MissingRequiredValue),
            ),
            RunPlanResolutionError::NoReusableExtractPlan => (
                DiagnosticSubject::operation("extract_selection"),
                DiagnosticReason::failure(DiagnosticFailureKind::ExtractPlanRequired),
            ),
            RunPlanResolutionError::ProfileRequired => (
                DiagnosticSubject::field("PROFILE_ID"),
                DiagnosticReason::failure(DiagnosticFailureKind::MissingRequiredValue),
            ),
            RunPlanResolutionError::SavedProfileUnavailable { profile_id } => (
                DiagnosticSubject::profile(profile_id),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            ),
        };
        Self::ConfigurationOrInput(Box::new(Self::report(
            source,
            DiagnosticCode::CommandRunPlan,
            DiagnosticStage::CommandPreparation,
            subject,
            reason,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        )))
    }

    fn translation_execution_build(source: ProductionTranslationExecutionBuildError) -> Self {
        let class = source.class;
        let diagnostic = source.diagnostic().clone();
        let report = Box::new(FailureReport::new(ReportedFailure::new(diagnostic, source)));
        match class {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput => {
                Self::ConfigurationOrInput(report)
            }
            TranslationExecutionBuildFailureClass::Internal => Self::Internal(report),
        }
    }

    fn project_lease<E>(source: ProjectCommandLeaseError<E>) -> Self
    where
        E: Error + SafeDiagnosticSource + Send + Sync + 'static,
    {
        let diagnostic = match &source {
            ProjectCommandLeaseError::Unavailable { project, source } => source
                .safe_diagnostic_source(
                    DiagnosticStage::ProjectOpening,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::RetryAfterResolvingContention,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                    "project={project}"
                ))),
        };
        Self::ProjectUnavailable(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn prevalidated_boundary(
        source: impl Error + Send + Sync + 'static,
        stage: DiagnosticStage,
        operation: &'static str,
    ) -> Self {
        Self::Internal(Box::new(Self::report(
            source,
            DiagnosticCode::InternalOperation,
            stage,
            DiagnosticSubject::operation(operation),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        )))
    }

    fn existing_project_opening(source: ProductionProjectOpeningError) -> Self {
        let diagnostic = match &source {
            ExistingProjectOpeningError::ReadProjectRecord(source) => {
                project_database_read_diagnostic(source)
            }
            ExistingProjectOpeningError::ResolveSourceData(source)
            | ExistingProjectOpeningError::ResolveSourceJs(source) => {
                project_directory_diagnostic(source)
            }
            ExistingProjectOpeningError::FingerprintSource(source) => {
                project_fingerprint_diagnostic(source)
            }
            ExistingProjectOpeningError::SourceSnapshotMismatch {
                persisted,
                observed,
            } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::ProjectOpening,
                DiagnosticSubject::operation("verify_source_snapshot"),
                DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                "persisted={persisted:?}; observed={observed:?}"
            ))),
        };
        let unavailable = diagnostic.code == DiagnosticCode::ProjectUnavailable;
        let report = Self::report_diagnostic(source, diagnostic);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    fn project_run_plan_read(source: ProjectRunPlanReadError<SqliteRuntimeError>) -> Self {
        let (diagnostic, unavailable) = match &source {
            ProjectRunPlanReadError::DatabaseNotFound { path } => (
                SafeDiagnostic::new(
                    DiagnosticCode::ProjectUnavailable,
                    DiagnosticStage::ProjectOpening,
                    DiagnosticSubject::path(path),
                    DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                ),
                true,
            ),
            ProjectRunPlanReadError::ReadDatabase { path, source } => (
                source
                    .safe_diagnostic(
                        DiagnosticStage::ProjectOpening,
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::CheckProjectState,
                    )
                    .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
                false,
            ),
            ProjectRunPlanReadError::InvalidState { path, reason } => {
                let mut diagnostic = SafeDiagnostic::new(
                    DiagnosticCode::ProjectState,
                    DiagnosticStage::ProjectOpening,
                    DiagnosticSubject::path(path),
                    DiagnosticReason::failure_with_detail(
                        DiagnosticFailureKind::StateMismatch,
                        reason.safe_detail(),
                    ),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                    "run_plan_subject={}",
                    reason.safe_subject()
                )));
                if let Some(fact) = reason.recovery_fact() {
                    diagnostic = diagnostic.with_recovery(fact.clone());
                }
                (diagnostic, false)
            }
        };
        let report = Self::report_diagnostic(source, diagnostic);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    #[cfg(test)]
    fn external_model(source: impl Error + Send + Sync + 'static) -> Self {
        Self::ExternalModel(Box::new(Self::report(
            source,
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::component("LLM client"),
            DiagnosticReason::failure(DiagnosticFailureKind::ExternalServiceUnavailable),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckModelService,
        )))
    }

    #[cfg(test)]
    fn internal<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Internal(Box::new(Self::report(
            source,
            DiagnosticCode::InternalOperation,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::operation(std::any::type_name::<E>()),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        )))
    }

    fn file_system_build(source: SystemFileSystemBuildError) -> Self {
        let diagnostic = source.safe_diagnostic();
        Self::Internal(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn sqlite_start(source: SqliteRuntimeError) -> Self {
        let diagnostic = source.safe_diagnostic(
            DiagnosticStage::ProcessStartup,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );
        Self::Internal(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn http_client_build(source: OpenAiExecutorBuildError) -> Self {
        let diagnostic = source.safe_diagnostic();
        let report = Self::report_diagnostic(source, diagnostic);
        match report.primary.public().action {
            DiagnosticAction::FixConfiguration => Self::ConfigurationOrInput(Box::new(report)),
            _ => Self::Internal(Box::new(report)),
        }
    }

    fn cpu_start(source: CpuExecutorStartError) -> Self {
        let diagnostic = match &source {
            CpuExecutorStartError::AvailableParallelism(error) => SafeDiagnostic::io(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::ProcessStartup,
                DiagnosticSubject::component("Rayon CPU workers"),
                "detect_available_parallelism",
                error,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            ),
            CpuExecutorStartError::TooManyWorkerThreads { requested, maximum } => {
                SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    DiagnosticStage::ProcessStartup,
                    DiagnosticSubject::component("Rayon CPU workers"),
                    DiagnosticReason::Resource {
                        resource: "worker_threads".to_owned(),
                        actual: u64::try_from(*requested).unwrap_or(u64::MAX),
                        maximum: Some(u64::try_from(*maximum).unwrap_or(u64::MAX)),
                    },
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::ReportBug,
                )
            }
            CpuExecutorStartError::Build(_) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::ProcessStartup,
                DiagnosticSubject::component("Rayon CPU pool"),
                DiagnosticReason::failure(DiagnosticFailureKind::ExternalServiceUnavailable),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            ),
        };
        Self::Internal(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn run_id(source: WindowsFsError) -> Self {
        let diagnostic = source.safe_diagnostic(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::ProcessStartup,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );
        Self::Internal(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn pem_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = match &source {
            ReadFileError::NotFound { path } => SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ReadFileError::NotFile { path } => SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            ReadFileError::Io { source, .. } => source.safe_diagnostic(
                DiagnosticStage::CommandPreparation,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
        };
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn invalid_run_plan(source: InvalidRunPlanValue) -> Self {
        let failure = match &source {
            InvalidRunPlanValue::EmptyPath { .. }
            | InvalidRunPlanValue::EmptyLuaProgram
            | InvalidRunPlanValue::EmptyExtractOwners
            | InvalidRunPlanValue::EmptyRulesDefinition
            | InvalidRunPlanValue::EmptyProfileId => DiagnosticFailureKind::MissingRequiredValue,
            InvalidRunPlanValue::RelativePath { .. }
            | InvalidRunPlanValue::PathContainsNul { .. }
            | InvalidRunPlanValue::InvalidWindowsPathEncoding { .. } => {
                DiagnosticFailureKind::InvalidPath
            }
            InvalidRunPlanValue::LuaProgramHashMismatch => DiagnosticFailureKind::StateMismatch,
            InvalidRunPlanValue::LuaProgramHashLength { .. }
            | InvalidRunPlanValue::InvalidRulesCanonicalJson { .. }
            | InvalidRunPlanValue::RulesCanonicalJsonNotArray
            | InvalidRunPlanValue::RulesCanonicalJsonEncodingFailed { .. }
            | InvalidRunPlanValue::InvalidRulesSemantics { .. }
            | InvalidRunPlanValue::NonCanonicalRulesJson
            | InvalidRunPlanValue::ProfileIdHasOuterWhitespace => {
                DiagnosticFailureKind::InvalidValue
            }
        };
        let mut diagnostic = SafeDiagnostic::new(
            DiagnosticCode::CommandRunPlan,
            DiagnosticStage::RunPlanFinalization,
            DiagnosticSubject::field(source.safe_subject()),
            DiagnosticReason::failure_with_detail(failure, source.safe_detail()),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );
        if let Some(fact) = source.recovery_fact() {
            diagnostic = diagnostic.with_recovery(fact.clone());
        }
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn signal(source: io::Error, outcome: SignalOutcomeSource) -> Self {
        let impact = match &outcome {
            SignalOutcomeSource::CompletedStateApplied => {
                DiagnosticImpact::StateAppliedFinalizationFailed
            }
            SignalOutcomeSource::Cancelled | SignalOutcomeSource::CommandFailed(_) => {
                DiagnosticImpact::Unchanged
            }
        };
        let signal = ReportedFailure::new(signal_diagnostic(&source, impact), source);
        match outcome {
            SignalOutcomeSource::CommandFailed(command) => {
                Self::Signal(Box::new(command.into_failure_report().with_related(signal)))
            }
            SignalOutcomeSource::CompletedStateApplied | SignalOutcomeSource::Cancelled => {
                Self::Signal(Box::new(FailureReport::new(signal)))
            }
        }
    }

    fn failure_report(&self) -> &FailureReport {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => report.as_ref(),
        }
    }

    fn was_cancelled_wait(&self) -> bool {
        let report = self.failure_report();
        report.related.is_empty() && report.primary.public().reason.is_wait_cancelled()
    }
}

enum SignalOutcomeSource {
    CompletedStateApplied,
    Cancelled,
    CommandFailed(ProductionCommandError),
}

impl fmt::Display for ProductionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure_report().fmt(formatter)
    }
}

impl Error for ProductionCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure_report().primary.source_error())
    }
}

#[derive(Default)]
pub(crate) struct ShutdownFailures {
    failures: Vec<ShutdownFailure>,
}

impl ShutdownFailures {
    fn push(
        &mut self,
        component: &'static str,
        source: impl Error + SafeDiagnosticSource + Send + Sync + 'static,
    ) {
        let report = source.into_failure_report(
            DiagnosticStage::Shutdown,
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::Retry,
        );
        self.failures.push(ShutdownFailure {
            component,
            reported: report.primary,
        });
        self.failures
            .extend(report.related.into_iter().map(|reported| ShutdownFailure {
                component,
                reported,
            }));
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    fn public_diagnostics(&self) -> impl Iterator<Item = &SafeDiagnostic> {
        self.failures
            .iter()
            .map(|failure| failure.reported.public())
    }
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failures.first() {
            Some(failure) => failure.reported.fmt(formatter),
            None => formatter.write_str("shutdown.empty"),
        }
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
            .map(|failure| failure.reported.source_error())
    }
}

struct ShutdownFailure {
    component: &'static str,
    reported: ReportedFailure,
}

impl fmt::Debug for ShutdownFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownFailure")
            .field("component", &self.component)
            .field("reported", &self.reported)
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
        shutdown_error: Option<&ShutdownFailures>,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(error) = command_error {
            render_failure_report(error.failure_report(), localizer, stderr)?;
        }
        if let Some(shutdown) = shutdown_error {
            let related_offset =
                command_error.map(|error| error.failure_report().related.len().saturating_add(1));
            render_shutdown_failures(shutdown, related_offset, localizer, stderr)?;
        }
        Ok(())
    }

    pub(crate) fn render_applied_finalization_failure(
        shutdown_error: &ShutdownFailures,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        render_shutdown_failures(shutdown_error, None, localizer, stderr)
    }
}

fn render_shutdown_failures(
    failures: &ShutdownFailures,
    related_offset: Option<usize>,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let mut diagnostics = failures.public_diagnostics();
    if let Some(offset) = related_offset {
        for (index, diagnostic) in diagnostics.enumerate() {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    index: u64::try_from(offset.saturating_add(index)).unwrap_or(u64::MAX),
                })
            )?;
            render_safe_diagnostic(diagnostic, localizer, stderr)?;
        }
    } else if let Some(primary) = diagnostics.next() {
        render_safe_diagnostic(primary, localizer, stderr)?;
        for (index, diagnostic) in diagnostics.enumerate() {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    index: u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX),
                })
            )?;
            render_safe_diagnostic(diagnostic, localizer, stderr)?;
        }
    }
    Ok(())
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

#[cfg(test)]
mod command_error_rendering_tests {
    use std::sync::atomic::{AtomicBool, AtomicU8};

    use super::*;

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    #[tokio::test]
    async fn command_scope_panic_uses_the_same_safe_projection_for_cli_and_jsonl() {
        const PRIVATE_PANIC_PAYLOAD: &str = "COMMAND_PANIC_PRIVATE_SENTINEL";
        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let project_workspace = directory.path().join("rpg_maker_mz").join("project");
        let logs_root = project_workspace.join("logs");
        let run_id = "550e8400-e29b-41d4-a716-446655440099";
        let mut runtime = start_project_log(logs_root, run_id.to_owned());
        let log_path = runtime.path().expect("真实日志应有路径").to_path_buf();
        let logger = runtime.logger();
        let context = ProjectLogContext::new("zh-Hans")
            .with_engine("rpg_maker_mz")
            .with_project("project")
            .with_command("extract");
        let diagnostic = command_panic_diagnostic(
            "extract",
            DiagnosticStage::Extract,
            &project_workspace,
            Some(&log_path),
        );
        let performance = Arc::new(RunPerformanceCounters::default());
        runtime.arm_unfinished_terminal(
            context.clone(),
            vec![diagnostic.clone()],
            Arc::clone(&performance),
        );
        let panic_boundary = CommandPanicBoundary::default();
        panic_boundary.prepare("extract", DiagnosticStage::Extract, &project_workspace);
        panic_boundary.register_project_log(vec![diagnostic.clone()], logger.clone());
        logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunStarted,
            context,
            ProjectLogPayload::Run { outcome: None },
        ));
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context: ProjectLogContext::new("zh-Hans").with_command("extract"),
            performance,
        };

        let report = catch_command_panic(panic_boundary, async move {
            let _active = active;
            std::panic::panic_any(Box::new(PRIVATE_PANIC_PAYLOAD));
        })
        .await;

        assert!(report.pending_project_log.is_none());
        assert!(report.shutdown_error.is_none());
        let CommandRunResult::Failed(error) = report.result else {
            panic!("panic 必须成为命令失败");
        };
        let public = error
            .failure_report()
            .public_diagnostics()
            .next()
            .expect("panic 必须具有安全诊断")
            .clone();
        assert_eq!(public, diagnostic);

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("panic 诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("panic 诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("internal.operation"));
        assert!(plain.contains("extract"));
        assert!(plain.contains(&project_workspace.to_string_lossy().to_string()));
        assert!(plain.contains(&log_path.to_string_lossy().to_string()));
        assert!(!stderr.contains(PRIVATE_PANIC_PAYLOAD));

        let raw = std::fs::read_to_string(&log_path).expect("panic 项目日志应可读取");
        assert!(!raw.contains(PRIVATE_PANIC_PAYLOAD));
        let records = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行应为 JSON"))
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .map(|record| record["code"].as_str().expect("日志 code 应为文本"))
                .collect::<Vec<_>>(),
            [
                "run.started",
                "performance.counters",
                "failure.reported",
                "run.finished",
            ]
        );
        assert_eq!(
            records[2]["payload"]["diagnostic"],
            serde_json::to_value(public).expect("安全诊断应可序列化")
        );
        assert_eq!(records[3]["payload"]["outcome"], "outcome_unknown");
    }

    struct InvalidRunPlanSnapshotQuery;

    impl crate::storage::sqlite::SqliteQueryExecutor for InvalidRunPlanSnapshotQuery {
        type Error = SqliteRuntimeError;

        fn query_existing_database(
            &self,
            _path: PathBuf,
            _query: crate::storage::sqlite::SqliteQuery,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<crate::storage::sqlite::SqliteRow>,
                crate::storage::sqlite::QueryExistingDatabaseError<Self::Error>,
            >,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn query_existing_database_snapshot(
            &self,
            _path: PathBuf,
            _queries: Vec<crate::storage::sqlite::SqliteQuery>,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<Vec<crate::storage::sqlite::SqliteRow>>,
                crate::storage::sqlite::QueryExistingDatabaseError<Self::Error>,
            >,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }
    }

    impl SafeDiagnosticSource for TestError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("test component"),
                DiagnosticReason::failure(DiagnosticFailureKind::FinalizationFailed),
                impact,
                fallback_action,
            )
        }
    }

    impl SafeDiagnosticSource for crate::execution::cpu::CpuTaskExecutionError<TestError> {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            match self {
                Self::Unavailable(source) => {
                    source.safe_diagnostic_source(stage, impact, fallback_action)
                }
                Self::Cancelled => SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    stage,
                    DiagnosticSubject::component("test CPU worker"),
                    DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                    impact,
                    DiagnosticAction::Retry,
                ),
                Self::TaskPanicked => SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    stage,
                    DiagnosticSubject::component("test CPU worker"),
                    DiagnosticReason::failure(DiagnosticFailureKind::WorkerPanicked),
                    impact,
                    DiagnosticAction::ReportBug,
                ),
            }
        }
    }

    #[tokio::test]
    async fn termination_signal_synchronously_cancels_root_waits_before_settling_business() {
        let order = Arc::new(AtomicU8::new(0));
        let waits = Arc::clone(&order);
        let presentation = Arc::clone(&order);
        let business = Arc::clone(&order);

        let driven = drive_with_signal(
            async move {
                assert_eq!(business.load(Ordering::Acquire), 2);
                7_u8
            },
            std::future::ready(Ok(())),
            move || {
                assert_eq!(waits.swap(1, Ordering::AcqRel), 0);
            },
            move || {
                assert_eq!(presentation.swap(2, Ordering::AcqRel), 1);
            },
        )
        .await;

        assert!(matches!(driven, DrivenCommand::Interrupted(7)));
    }

    #[tokio::test]
    async fn signal_receiver_failure_cancels_waits_without_claiming_user_cancellation() {
        let waits_cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_presented = Arc::new(AtomicBool::new(false));
        let waits = Arc::clone(&waits_cancelled);
        let presentation = Arc::clone(&cancellation_presented);

        let driven = drive_with_signal(
            std::future::ready(11_u8),
            std::future::ready(Err(io::Error::other("signal unavailable"))),
            move || waits.store(true, Ordering::Release),
            move || presentation.store(true, Ordering::Release),
        )
        .await;

        assert!(waits_cancelled.load(Ordering::Acquire));
        assert!(!cancellation_presented.load(Ordering::Acquire));
        assert!(matches!(
            driven,
            DrivenCommand::SignalFailed { result: 11, .. }
        ));
    }

    #[test]
    fn cancelled_final_run_plan_wait_is_a_command_interruption_not_a_root_failure() {
        let error = ProjectRunPlanReplaceError::RollbackConfirmed {
            path: PathBuf::from("project.db"),
            source: SqliteRuntimeError::Cancelled {
                operation: "begin_immediate",
            },
        };
        assert!(run_plan_wait_was_cancelled(&error));

        let execution = DrivenCommand::Finished(Ok(OperationCompletion::Completed(23_u8)));
        let interrupted = mark_successful_execution_interrupted(execution);
        assert!(matches!(
            interrupted,
            DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(23)))
        ));
    }

    #[test]
    fn signal_cancelled_resource_wait_is_exit_130_without_failure_log() {
        let error =
            ProductionCommandError::Internal(Box::new(FailureReport::new(ReportedFailure::new(
                SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    DiagnosticStage::Extract,
                    DiagnosticSubject::component("CPU worker"),
                    DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                ),
                TestError("等待取消不应作为根失败显示"),
            ))));
        let execution: DrivenCommand<
            Result<OperationCompletion<RpgMakerCommandOutput>, ProductionCommandError>,
        > = DrivenCommand::Interrupted(Err(error));
        let shutdown = ShutdownFailures::default();

        assert_eq!(
            project_log_outcome(&execution, &shutdown),
            ProjectLogRunOutcome::Cancelled
        );
        assert!(project_log_failure_diagnostics(&execution, &shutdown).is_empty());
        let report =
            ProductionCommandRunReport::from_completion_with_project_log(execution, shutdown, None);
        assert!(matches!(report.result, CommandRunResult::Interrupted));
    }

    #[test]
    fn generic_configuration_failure_does_not_render_arbitrary_source_text() {
        let error = ProductionCommandError::configuration_or_input(TestError(
            "ARBITRARY_CONFIGURATION_SOURCE_SENTINEL",
        ));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");

        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        assert!(stderr.contains("command.input"));
        assert!(stderr.contains("命令准备"));
        assert!(!stderr.contains("ARBITRARY_CONFIGURATION_SOURCE_SENTINEL"));
    }

    #[test]
    fn project_database_metadata_diagnostic_keeps_definition_stage_and_json_position() {
        use crate::rpg_maker::project_database::{
            InvalidProjectMetadata, ProjectDefinitionFailure, ProjectDefinitionStage,
            SafeJsonErrorCategory,
        };

        let source: ProjectDatabaseReadError<SqliteRuntimeError> =
            ProjectDatabaseReadError::InvalidMetadata {
                path: PathBuf::from("C:/projects/demo/project.db"),
                reason: InvalidProjectMetadata::InvalidDialogueDefinition {
                    stage: ProjectDefinitionStage::Decode,
                    failure: ProjectDefinitionFailure::InvalidJson {
                        category: SafeJsonErrorCategory::Data,
                        line: 7,
                        column: 19,
                    },
                },
            };
        let diagnostic = project_database_read_diagnostic(&source);
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert_eq!(diagnostic.stage, DiagnosticStage::ProjectOpening);
        assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
        assert!(matches!(
            &diagnostic.reason,
            DiagnosticReason::FailureWithDetail { detail, .. }
                if detail.contains("metadata=invalid_dialogue_definition")
        ));
        assert!(serialized.contains("C:/projects/demo/project.db"));
        assert!(serialized.contains("metadata=invalid_dialogue_definition"));
        assert!(serialized.contains("definition=mv_dialogue_rules"));
        assert!(serialized.contains("stage=decode"));
        assert!(serialized.contains("failure=invalid_json"));
        assert!(serialized.contains("category=data"));
        assert!(serialized.contains("line=7"));
        assert!(serialized.contains("column=19"));
    }

    #[test]
    fn project_database_sqlite_diagnostic_keeps_codes_path_stage_and_query_without_driver_text() {
        const SQL_PARAMETER_SENTINEL: &str = "SECRET_SQL_AND_PARAMETER_TEXT";
        let driver = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            Some(SQL_PARAMETER_SENTINEL.to_owned()),
        );
        let source = ProjectDatabaseReadError::ReadDatabase {
            path: PathBuf::from("C:/projects/demo/project.db"),
            stage: "read_project_record",
            query_id: "project_database.project_record".to_owned(),
            source: SqliteRuntimeError::Driver {
                operation: "prepare_query",
                source: driver,
            },
        };
        let diagnostic = project_database_read_diagnostic(&source);
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        let mut rendered = Vec::new();
        render_safe_diagnostic(
            &diagnostic,
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &mut rendered,
        )
        .expect("诊断应可呈现");
        let rendered = String::from_utf8(rendered).expect("诊断应为 UTF-8");

        assert!(matches!(
            diagnostic.reason,
            DiagnosticReason::Sqlite {
                primary_code: 19,
                extended_code: 2067,
            }
        ));
        assert!(serialized.contains("C:/projects/demo/project.db"));
        assert!(serialized.contains("database_stage=read_project_record"));
        assert!(serialized.contains("database_query_id=project_database.project_record"));
        assert!(rendered.contains("SQLite 主错误码 19"));
        assert!(rendered.contains("扩展错误码 2067"));
        assert!(!serialized.contains(SQL_PARAMETER_SENTINEL));
        assert!(!rendered.contains(SQL_PARAMETER_SENTINEL));
    }

    #[test]
    fn project_database_io_and_reconciliation_diagnostics_keep_safe_os_reason_and_terminal_state() {
        let io_source = ProjectDatabaseReadError::ReadDatabase {
            path: PathBuf::from("C:/projects/demo/project.db"),
            stage: "read_project_record",
            query_id: "project_database.project_record".to_owned(),
            source: SqliteRuntimeError::Io {
                operation: "open_database",
                path: PathBuf::from("C:/projects/demo/project.db"),
                source: io::Error::from_raw_os_error(5),
            },
        };
        let io_diagnostic = project_database_read_diagnostic(&io_source);
        assert!(matches!(
            io_diagnostic.reason,
            DiagnosticReason::Io {
                raw_os_code: Some(5),
                system_message: Some(_),
                ..
            }
        ));

        let not_committed = ProjectDatabaseReconciliationError::NotCommitted {
            path: PathBuf::from("C:/projects/demo/project.db"),
            source: SqliteRuntimeError::Driver {
                operation: "commit_reconciliation",
                source: rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    None,
                ),
            },
        };
        let not_committed = init_database_reconciliation_diagnostic(&not_committed);
        assert_eq!(not_committed.impact, DiagnosticImpact::Unchanged);
        assert!(not_committed.recovery.iter().any(|fact| matches!(
            fact,
            crate::diagnostic::RecoveryFact::Component { name }
                if name == "database_stage=reconcile_project_database"
        )));
        assert!(not_committed.recovery.iter().any(|fact| matches!(
            fact,
            crate::diagnostic::RecoveryFact::Transaction { state } if state == "rolled_back"
        )));

        let outcome_unknown = ProjectDatabaseReconciliationError::OutcomeUnknown {
            path: PathBuf::from("C:/projects/demo/project.db"),
            source: SqliteRuntimeError::Driver {
                operation: "commit_reconciliation",
                source: rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    None,
                ),
            },
        };
        let outcome_unknown = init_database_reconciliation_diagnostic(&outcome_unknown);
        assert_eq!(outcome_unknown.impact, DiagnosticImpact::OutcomeUnknown);
        assert!(outcome_unknown.recovery.iter().all(|fact| !matches!(
            fact,
            crate::diagnostic::RecoveryFact::Transaction { state } if state == "rolled_back"
        )));
    }

    #[test]
    fn prompt_build_failures_preserve_each_typed_fact_without_prompt_or_source_text() {
        enum ExpectedReason {
            Failure(DiagnosticFailureKind),
            Detail(DiagnosticFailureKind, &'static [&'static str]),
            InvalidUtf8 {
                valid_up_to: u64,
                error_len: Option<u64>,
            },
            Io {
                operation: &'static str,
                error_kind: crate::diagnostic::SafeIoKind,
            },
        }

        let locale = UiLocale::SimplifiedChinese;
        let component = PromptResourceComponent::System;
        let path = PathBuf::from("prompts/rpg_maker/zh-Hans/system.md");
        let prompt = |source| {
            ProductionTranslationExecutionBuildError::prompt_resource(
                locale, component, &path, source,
            )
        };
        let template = |source| {
            ProductionTranslationExecutionBuildError::prompt_template(
                locale, component, &path, source,
            )
        };
        let cases = vec![
            (
                "not_found",
                prompt(PromptResourceLoadError::Read(ReadFileError::NotFound {
                    path: path.clone(),
                })),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Failure(DiagnosticFailureKind::NotFound),
            ),
            (
                "not_file",
                prompt(PromptResourceLoadError::Read(ReadFileError::NotFile {
                    path: path.clone(),
                })),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::InvalidValue,
                    &["expected=file", "actual=not_file"],
                ),
            ),
            (
                "os_error",
                prompt(PromptResourceLoadError::Read(ReadFileError::Io {
                    path: path.clone(),
                    source: SystemFileSystemError::Io {
                        operation: "read_prompt_file",
                        path: path.clone(),
                        source: io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "PROMPT_CONTENT_SENTINEL",
                        ),
                    },
                })),
                DiagnosticCode::FileSystemOperation,
                ExpectedReason::Io {
                    operation: "read_prompt_file",
                    error_kind: crate::diagnostic::SafeIoKind::PermissionDenied,
                },
            ),
            (
                "identity",
                prompt(PromptResourceLoadError::ResolvedFileNameMismatch {
                    requested_path: path.clone(),
                    resolved_path: PathBuf::from("prompts/rpg_maker/zh-Hans/resolved-system.md"),
                }),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::FileIdentityChanged,
                    &[
                        "expected_file_name=system.md",
                        "actual_file_name=resolved-system.md",
                    ],
                ),
            ),
            (
                "invalid_utf8",
                prompt(PromptResourceLoadError::InvalidUtf8 {
                    path: path.clone(),
                    valid_up_to: 7,
                    error_len: Some(1),
                }),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::InvalidUtf8 {
                    valid_up_to: 7,
                    error_len: Some(1),
                },
            ),
            (
                "empty",
                prompt(PromptResourceLoadError::Empty { path: path.clone() }),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    &["resource=prompt", "content=blank"],
                ),
            ),
            (
                "template_invalid_syntax",
                template(PromptTemplateError::InvalidSyntax),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::InvalidSyntax,
                    &["template_error=invalid_syntax", "allowed_variables="],
                ),
            ),
            (
                "template_unknown_variable",
                template(PromptTemplateError::UnknownVariable),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::InvalidValue,
                    &["template_error=unknown_variable", "allowed_variables="],
                ),
            ),
            (
                "template_missing_source",
                template(PromptTemplateError::MissingSourceLanguage),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    &[
                        "template_error=missing_variable",
                        "variable=source_language",
                    ],
                ),
            ),
            (
                "template_missing_target",
                template(PromptTemplateError::MissingTargetLanguage),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    &[
                        "template_error=missing_variable",
                        "variable=target_language",
                    ],
                ),
            ),
            (
                "template_variables_forbidden",
                template(PromptTemplateError::VariablesNotAllowed),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::InvalidSyntax,
                    &["template_error=variables_not_allowed"],
                ),
            ),
            (
                "system_prompt_blank",
                ProductionTranslationExecutionBuildError::system_prompt(
                    locale,
                    component,
                    &path,
                    RpgMakerSystemPromptError::Blank,
                ),
                DiagnosticCode::PromptUnavailable,
                ExpectedReason::Detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    &[
                        "resource=system_prompt",
                        "content=blank_after_template_render",
                    ],
                ),
            ),
        ];

        for (name, build, expected_code, expected_reason) in cases {
            let error = ProductionCommandError::translation_execution_build(build);
            let diagnostic = error.failure_report().primary.public();
            assert_eq!(diagnostic.code, expected_code, "{name}");
            assert_eq!(
                diagnostic.stage,
                DiagnosticStage::CommandPreparation,
                "{name}"
            );
            assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged, "{name}");
            match (&diagnostic.reason, expected_reason) {
                (DiagnosticReason::Failure { failure }, ExpectedReason::Failure(expected)) => {
                    assert_eq!(*failure, expected, "{name}")
                }
                (
                    DiagnosticReason::FailureWithDetail { failure, detail },
                    ExpectedReason::Detail(expected, facts),
                ) => {
                    assert_eq!(*failure, expected, "{name}");
                    for fact in facts {
                        assert!(detail.contains(fact), "{name} 缺少 {fact}: {detail}");
                    }
                }
                (
                    DiagnosticReason::InvalidUtf8 {
                        valid_up_to,
                        error_len,
                    },
                    ExpectedReason::InvalidUtf8 {
                        valid_up_to: expected_offset,
                        error_len: expected_length,
                    },
                ) => {
                    assert_eq!(*valid_up_to, expected_offset, "{name}");
                    assert_eq!(*error_len, expected_length, "{name}");
                }
                (
                    DiagnosticReason::Io {
                        operation,
                        error_kind,
                        ..
                    },
                    ExpectedReason::Io {
                        operation: expected_operation,
                        error_kind: expected_kind,
                    },
                ) => {
                    assert_eq!(operation, expected_operation, "{name}");
                    assert_eq!(*error_kind, expected_kind, "{name}");
                }
                (actual, _) => panic!("{name} 的诊断原因不匹配：{actual:?}"),
            }

            let mut stderr = Vec::new();
            let localizer = UiLocalizer::new(locale);
            CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
                .expect("诊断应可写入");
            let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
            let json = serde_json::to_string(diagnostic).expect("日志诊断应可序列化");
            for output in [&stderr, &json] {
                assert!(
                    output.contains(diagnostic.code.as_str()),
                    "{name}: {output}"
                );
                assert!(output.contains("system.md"), "{name}: {output}");
                assert!(
                    !output.contains("PROMPT_CONTENT_SENTINEL"),
                    "{name} 泄露了 Prompt/source 正文：{output}"
                );
            }
        }
    }

    #[test]
    fn language_module_build_failure_reports_requested_and_available_ids() {
        let source_language = crate::language::LanguageId::parse("ja").expect("ja 应合法");
        let target_language =
            crate::language::LanguageId::parse("zh-Hans").expect("zh-Hans 应合法");
        let language_pair =
            crate::language::LanguagePair::new(source_language.clone(), target_language);
        let build = ProductionTranslationExecutionBuildError::language_module(
            &language_pair,
            LanguageModuleCatalogError::UnknownLanguageId {
                language_id: source_language,
                available_ids: vec![
                    crate::language::LanguageId::parse("en").expect("en 应合法"),
                    crate::language::LanguageId::parse("ko").expect("ko 应合法"),
                ],
            },
        );
        let error = ProductionCommandError::translation_execution_build(build);
        let diagnostic = error.failure_report().primary.public();
        let DiagnosticReason::FailureWithDetail { failure, detail } = &diagnostic.reason else {
            panic!("未知语言模块必须输出结构化 ID 事实");
        };
        assert_eq!(*failure, DiagnosticFailureKind::NotFound);
        for expected in ["requested_id=ja", "available_ids=en,ko"] {
            assert!(detail.contains(expected), "缺少 {expected}: {detail}");
        }

        let mut stderr = Vec::new();
        let localizer = UiLocalizer::new(UiLocale::English);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let json = serde_json::to_string(diagnostic).expect("日志诊断应可序列化");

        for output in [&stderr, &json] {
            assert!(output.contains("language_module.unavailable"));
            assert!(output.contains("requested_id=ja"));
            assert!(output.contains("available_ids=en,ko"));
            assert!(output.contains("target_language=zh-Hans"));
        }
    }

    #[test]
    fn run_plan_log_fact_preserves_each_transaction_terminal_state() {
        let path = PathBuf::from("project.db");
        let cases = [
            (
                ProjectRunPlanReplaceError::RollbackConfirmed {
                    path: path.clone(),
                    source: TestError("rollback"),
                },
                ProjectLogLevel::Warn,
                ProjectLogCode::RunPlanSaveFailed,
            ),
            (
                ProjectRunPlanReplaceError::OutcomeUnknown {
                    path: path.clone(),
                    source: TestError("unknown"),
                },
                ProjectLogLevel::Error,
                ProjectLogCode::RunPlanSaveOutcomeUnknown,
            ),
            (
                ProjectRunPlanReplaceError::CommittedButFinalizationFailed {
                    path,
                    source: TestError("finalization"),
                },
                ProjectLogLevel::Error,
                ProjectLogCode::RunPlanSavedFinalizationFailed,
            ),
        ];

        for (error, expected_level, expected_code) in cases {
            let (level, code) = run_plan_replace_log_fact(&error);
            assert_eq!(level, expected_level);
            assert_eq!(code, expected_code);
        }
    }

    #[test]
    fn signal_failure_preserves_nested_user_repairable_category() {
        let error = ProductionCommandError::signal(
            io::Error::other("SIGNAL_SECRET_SENTINEL"),
            SignalOutcomeSource::CommandFailed(ProductionCommandError::configuration_or_input(
                TestError("locale zh-Hans 的 system.md Prompt 资源缺失"),
            )),
        );
        let mut shutdown = ShutdownFailures::default();
        shutdown.push("SQLite", TestError("SQL_PARAMETER_SENTINEL"));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(
            Some(&error),
            Some(&shutdown),
            &localizer,
            &mut stderr,
        )
        .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");

        assert!(stderr.contains("command.input"));
        assert!(stderr.contains("signal.registration"));
        assert!(stderr.contains("shutdown.component"));
        assert!(plain.contains("相关错误 1"));
        assert!(plain.contains("相关错误 2"));
        assert!(!stderr.contains("locale zh-Hans 的 system.md Prompt 资源缺失"));
        assert!(!stderr.contains("SIGNAL_SECRET_SENTINEL"));
        assert!(!stderr.contains("SQL_PARAMETER_SENTINEL"));
    }

    fn render_and_persist_shutdown(
        shutdown: &ShutdownFailures,
        run_id: &str,
    ) -> (String, Vec<serde_json::Value>) {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(None, Some(shutdown), &localizer, &mut stderr)
            .expect("关闭诊断应可写入");

        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let context = ProjectLogContext::new("zh-Hans").with_command("write-back");
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context,
            performance: Arc::new(RunPerformanceCounters::default()),
        };
        let failures = shutdown.public_diagnostics().cloned().collect();
        let warning = finish_project_log(active, ProjectLogRunOutcome::Failed, failures);
        assert!(warning.is_none(), "测试日志不应降级");
        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = std::fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str(line).expect("项目日志行应是 JSON"))
            .collect();
        (
            String::from_utf8(stderr).expect("关闭诊断应为 UTF-8"),
            records,
        )
    }

    #[test]
    fn finish_project_log_persists_the_shared_performance_snapshot_before_terminal_records() {
        use crate::runtime::performance::{SqliteTransactionControl, SqliteTransactionScope};

        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440021";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let performance = Arc::new(RunPerformanceCounters::default());
        performance.sqlite_control_attempted(
            SqliteTransactionScope::WritePlan,
            SqliteTransactionControl::Begin,
        );
        performance.sqlite_control_succeeded(
            SqliteTransactionScope::WritePlan,
            SqliteTransactionControl::Begin,
        );
        performance.candidate_validation_started();
        performance.candidate_validation_completed();
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context: ProjectLogContext::new("zh-Hans").with_command("write-back"),
            performance,
        };
        let failure = SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::component("test terminal failure"),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        );

        assert!(finish_project_log(active, ProjectLogRunOutcome::Failed, vec![failure]).is_none());

        let records = std::fs::read_to_string(directory.path().join(format!("{run_id}.jsonl")))
            .expect("项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行应为 JSON"))
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .map(|record| record["code"].as_str().expect("日志 code 应为文本"))
                .collect::<Vec<_>>(),
            ["performance.counters", "failure.reported", "run.finished"]
        );
        let snapshot = &records[0]["payload"]["snapshot"];
        assert_eq!(
            snapshot["sqlite_transactions"]["write_plan"]["begin"]["attempted"],
            1
        );
        assert_eq!(
            snapshot["sqlite_transactions"]["write_plan"]["begin"]["succeeded"],
            1
        );
        assert_eq!(snapshot["candidate_validations"]["started"], 1);
        assert_eq!(snapshot["candidate_validations"]["completed"], 1);
    }

    fn render_and_persist_command_failure(
        error: &ProductionCommandError,
        run_id: &str,
    ) -> (String, Vec<serde_json::Value>) {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(error), None, &localizer, &mut stderr)
            .expect("命令诊断应可写入");

        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context: ProjectLogContext::new("zh-Hans").with_command("init"),
            performance: Arc::new(RunPerformanceCounters::default()),
        };
        let failures = error
            .failure_report()
            .public_diagnostics()
            .cloned()
            .collect();
        let warning = finish_project_log(active, ProjectLogRunOutcome::Failed, failures);
        assert!(warning.is_none(), "测试日志不应降级");
        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = std::fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str(line).expect("项目日志行应是 JSON"))
            .collect();
        (
            String::from_utf8(stderr).expect("命令诊断应为 UTF-8"),
            records,
        )
    }

    #[test]
    fn init_candidate_primary_and_discard_failure_reach_cli_and_jsonl() {
        let candidate_path = PathBuf::from(r"C:\projects\.game-stage\source");
        let staging_root = PathBuf::from(r"C:\projects\.game-stage");
        let workspace_error: ProductionWorkspaceConvergenceError =
            ProjectWorkspaceConvergenceError::CandidateFailure {
                failure: ProjectWorkspaceCandidateFailure::FingerprintCandidate(
                    DirectoryTreeFingerprintError::Failed {
                        path: candidate_path.clone(),
                        source: Box::new(SystemFileSystemError::Io {
                            operation: "fingerprint_candidate",
                            path: candidate_path.clone(),
                            source: io::Error::from_raw_os_error(2),
                        }),
                    },
                ),
                discard: Some(DirectoryDiscardError::new(
                    staging_root.clone(),
                    Box::new(SystemFileSystemError::Io {
                        operation: "discard_candidate",
                        path: staging_root.clone(),
                        source: io::Error::from_raw_os_error(5),
                    }),
                )),
            };
        let mapped = map_init_error(InitServiceError::Workspace(workspace_error));

        assert!(matches!(&mapped, ProductionCommandError::ProjectState(_)));
        let report = mapped.failure_report();
        assert_eq!(report.related.len(), 1);
        assert_eq!(report.primary.public().impact, DiagnosticImpact::Unchanged);
        assert_eq!(
            report.related[0].public().impact,
            DiagnosticImpact::RecoveryRequired
        );
        assert!(
            report.related[0]
                .public()
                .recovery
                .contains(&crate::diagnostic::RecoveryFact::path(&staging_root))
        );

        let (stderr, records) =
            render_and_persist_command_failure(&mapped, "550e8400-e29b-41d4-a716-446655440013");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(stderr.contains(candidate_path.to_string_lossy().as_ref()));
        assert!(stderr.contains("fingerprint_candidate"));
        assert!(stderr.contains("OS 2"));
        assert!(stderr.contains(staging_root.to_string_lossy().as_ref()));
        assert!(stderr.contains("discard_candidate"));
        assert!(stderr.contains("OS 5"));
        assert!(plain.contains("相关错误 1"));

        let failures = records
            .iter()
            .filter(|record| record["code"] == "failure.reported")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["payload"]["relation"], "primary");
        assert_eq!(failures[1]["payload"]["relation"], "related");
        assert_eq!(
            failures[0]["payload"]["diagnostic"]["reason"]["raw_os_code"].as_i64(),
            Some(2)
        );
        assert_eq!(
            failures[1]["payload"]["diagnostic"]["reason"]["raw_os_code"].as_i64(),
            Some(5)
        );
        let staging_root_text = staging_root.to_string_lossy();
        assert!(
            failures[1]["payload"]["diagnostic"]["recovery"]
                .as_array()
                .expect("相关诊断恢复事实应为数组")
                .iter()
                .any(|fact| fact["kind"].as_str() == Some("path")
                    && fact["path"].as_str() == Some(staging_root_text.as_ref()))
        );
    }

    #[test]
    fn filesystem_operation_and_failed_rollback_reach_cli_and_jsonl() {
        let candidate = PathBuf::from(r"C:\game\candidate");
        let mut shutdown = ShutdownFailures::default();
        shutdown.push(
            "FileSystem",
            SystemFileSystemError::DirectChildRollbackFailed {
                path: candidate.clone(),
                operation: Box::new(SystemFileSystemError::InvalidPath {
                    path: candidate.clone(),
                    reason: "候选目录身份在操作期间发生变化",
                }),
                rollback: Box::new(SystemFileSystemError::Io {
                    operation: "remove_candidate",
                    path: candidate,
                    source: io::Error::from_raw_os_error(5),
                }),
            },
        );

        assert_eq!(shutdown.public_diagnostics().count(), 2);
        let (stderr, records) =
            render_and_persist_shutdown(&shutdown, "550e8400-e29b-41d4-a716-446655440011");
        assert!(stderr.contains("候选目录身份在操作期间发生变化"));
        assert!(stderr.contains("rollback_failed"));
        assert!(stderr.contains("remove_candidate"));
        assert!(stderr.contains("OS 5"));
        let failures = records
            .iter()
            .filter(|record| record["code"] == "failure.reported")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["payload"]["relation"], "primary");
        assert_eq!(failures[1]["payload"]["relation"], "related");
        assert!(
            failures[0]["message"]
                .as_str()
                .unwrap()
                .contains("rollback_failed")
        );
        assert!(failures[1]["message"].as_str().unwrap().contains("OS 5"));
    }

    #[test]
    fn sqlite_primary_and_cleanup_failure_reach_cli_and_jsonl() {
        let database = PathBuf::from(r"C:\game\project.db");
        let mut shutdown = ShutdownFailures::default();
        shutdown.push(
            "SQLite",
            SqliteRuntimeError::Cleanup {
                primary: Box::new(SqliteRuntimeError::InvalidTarget {
                    path: database.clone(),
                }),
                failures: vec![SqliteRuntimeError::Io {
                    operation: "close_database",
                    path: database,
                    source: io::Error::from_raw_os_error(112),
                }],
            },
        );

        assert_eq!(shutdown.public_diagnostics().count(), 2);
        let (stderr, records) =
            render_and_persist_shutdown(&shutdown, "550e8400-e29b-41d4-a716-446655440012");
        assert!(stderr.contains("sqlite_cleanup_failures=1"));
        assert!(stderr.contains("close_database"));
        assert!(stderr.contains("OS 112"));
        let failures = records
            .iter()
            .filter(|record| record["code"] == "failure.reported")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["payload"]["relation"], "primary");
        assert_eq!(failures[1]["payload"]["relation"], "related");
        assert!(
            failures[0]["message"]
                .as_str()
                .unwrap()
                .contains("sqlite_cleanup_failures=1")
        );
        assert!(failures[1]["message"].as_str().unwrap().contains("OS 112"));
    }

    type ExtractionStoreError = crate::rpg_maker::extract::store::RpgMakerExtractionAssetStoreError<
        crate::runtime::cpu::CpuExecutorUnavailable,
        SqliteRuntimeError,
    >;

    fn outcome_unknown_store_error(operation: &'static str, os_code: i32) -> ExtractionStoreError {
        let database_path = PathBuf::from(r"C:\projects\alice\project.db");
        ExtractionStoreError::OutcomeUnknown {
            database_path: database_path.clone(),
            source: SqliteRuntimeError::Io {
                operation,
                path: database_path,
                source: io::Error::from_raw_os_error(os_code),
            },
        }
    }

    fn not_committed_store_error(operation: &'static str, os_code: i32) -> ExtractionStoreError {
        let database_path = PathBuf::from(r"C:\projects\alice\project.db");
        ExtractionStoreError::NotCommitted {
            database_path: database_path.clone(),
            source: SqliteRuntimeError::Io {
                operation,
                path: database_path,
                source: io::Error::from_raw_os_error(os_code),
            },
        }
    }

    #[test]
    fn builtin_rules_and_lua_store_keep_sqlite_rolled_back_terminal_state() {
        type BuiltinError = BuiltInExtractionError<
            TestError,
            ExtractionStoreError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >;
        type RulesError = RulesExtractionError<TestError, ExtractionStoreError, TestError>;
        type LuaError = LuaExtractionError<
            crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<TestError, TestError>,
            ExtractionStoreError,
        >;

        fn assert_rolled_back(report: FailureReport, expected_context: &str) {
            assert_eq!(report.primary.public().impact, DiagnosticImpact::Unchanged);
            let diagnostic =
                serde_json::to_string(report.primary.public()).expect("事务诊断应可序列化");
            assert!(diagnostic.contains("project.db"));
            assert!(diagnostic.contains("rolled_back"));
            assert!(diagnostic.contains("\"raw_os_code\":5"));
            assert!(diagnostic.contains(expected_context));
            assert!(matches!(
                map_project_failure_report(report),
                ProductionCommandError::ProjectState(_)
            ));
        }

        let builtin: ExtractServiceError<TestError, BuiltinError, RulesError, LuaError, TestError> =
            ExtractServiceError::BuiltIn(BuiltInExtractionError::Persist(
                not_committed_store_error("commit_builtin_snapshot", 5),
            ));
        let builtin = map_extract_error(builtin);
        assert!(matches!(&builtin, ProductionCommandError::ProjectState(_)));
        assert_rolled_back(builtin.into_failure_report(), "owner=builtin");

        let rules: ExtractServiceError<TestError, BuiltinError, RulesError, LuaError, TestError> =
            ExtractServiceError::Rules {
                rules_path: PathBuf::from("outer-rules.toml"),
                source: RulesExtractionError::Persist {
                    rules_path: PathBuf::from("rules/dialogue.toml"),
                    source: not_committed_store_error("commit_rules_snapshot", 5),
                },
            };
        let rules = map_extract_error(rules);
        assert!(matches!(&rules, ProductionCommandError::ProjectState(_)));
        assert_rolled_back(rules.into_failure_report(), "rules/dialogue.toml");

        let lua: ExtractServiceError<TestError, BuiltinError, RulesError, LuaError, TestError> =
            ExtractServiceError::Lua {
                script_path: PathBuf::from("scripts/extract.lua"),
                source: LuaExtractionError::StoreSnapshot(not_committed_store_error(
                    "commit_lua_snapshot",
                    5,
                )),
            };
        let lua = map_extract_error(lua);
        assert!(matches!(&lua, ProductionCommandError::ProjectState(_)));
        assert_rolled_back(lua.into_failure_report(), "owner=lua");
    }

    #[test]
    fn builtin_rules_and_lua_store_keep_sqlite_outcome_unknown_and_run_finished() {
        let builtin: BuiltInExtractionError<
            TestError,
            ExtractionStoreError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        > = BuiltInExtractionError::Persist(outcome_unknown_store_error(
            "commit_builtin_snapshot",
            1117,
        ));
        let builtin_report = builtin.into_extract_failure_report();
        assert_eq!(
            builtin_report.primary.public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        let builtin_error = map_project_failure_report(builtin_report);
        assert!(matches!(
            &builtin_error,
            ProductionCommandError::OutcomeUnknown(_)
        ));

        let rules: RulesExtractionError<TestError, ExtractionStoreError, TestError> =
            RulesExtractionError::Persist {
                rules_path: PathBuf::from("rules/dialogue.toml"),
                source: outcome_unknown_store_error("commit_rules_snapshot", 1117),
            };
        let rules_report = rules.into_extract_failure_report();
        assert_eq!(
            rules_report.primary.public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        assert!(
            serde_json::to_string(rules_report.primary.public())
                .expect("Rules 诊断应可序列化")
                .contains("rules/dialogue.toml")
        );
        assert!(matches!(
            map_project_failure_report(rules_report),
            ProductionCommandError::OutcomeUnknown(_)
        ));

        let lua: LuaExtractionError<
            crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<TestError, TestError>,
            ExtractionStoreError,
        > = LuaExtractionError::StoreSnapshot(outcome_unknown_store_error(
            "commit_lua_snapshot",
            1117,
        ));
        let lua_report = lua.into_extract_failure_report();
        assert_eq!(
            lua_report.primary.public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        assert!(matches!(
            map_project_failure_report(lua_report),
            ProductionCommandError::OutcomeUnknown(_)
        ));

        let execution: DrivenCommand<Result<OperationCompletion<()>, ProductionCommandError>> =
            DrivenCommand::Finished(Err(builtin_error));
        let shutdown = ShutdownFailures::default();
        let outcome = project_log_outcome(&execution, &shutdown);
        assert_eq!(outcome, ProjectLogRunOutcome::OutcomeUnknown);
        let failures = project_log_failure_diagnostics(&execution, &shutdown);
        assert!(serde_json::to_string(&failures).unwrap().contains("1117"));

        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440013";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context: ProjectLogContext::new("zh-Hans").with_command("extract"),
            performance: Arc::new(RunPerformanceCounters::default()),
        };
        assert!(finish_project_log(active, outcome, failures).is_none());
        let records = std::fs::read_to_string(directory.path().join(format!("{run_id}.jsonl")))
            .expect("项目日志应可读取");
        let finished = records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|record| record["code"] == "run.finished")
            .expect("必须写入 run.finished");
        assert_eq!(finished["payload"]["outcome"], "outcome_unknown");
        assert!(
            finished["message"]
                .as_str()
                .unwrap()
                .contains("最终结果未知")
        );
    }

    #[test]
    fn lua_runtime_and_cleanup_both_reach_command_failure_report() {
        let source: crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<
            TestError,
            TestError,
        > = crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError::RuntimeAndCleanup {
            runtime: crate::rpg_maker::lua::runtime::TrustedLuaRuntimeExecutionError::Execute(
                TestError("LUA_VM_SENTINEL"),
            ),
            cleanup: crate::rpg_maker::lua::runtime::TrustedLuaBindingFinalizationError::new(
                "SQL_SENTINEL",
                Some(std::sync::Arc::new(TestError("CLEANUP_SENTINEL"))),
            ),
        };
        let error: LuaExtractionError<_, ExtractionStoreError> = LuaExtractionError::ExecuteHost {
            script_path: PathBuf::from("scripts/extract.lua"),
            source,
        };
        let report = error.into_extract_failure_report();
        assert_eq!(report.related.len(), 1);
        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::RecoveryRequired
        );
        let mut rendered = Vec::new();
        render_failure_report(
            &report,
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &mut rendered,
        )
        .expect("Lua 主错和清理错应可渲染");
        let rendered = String::from_utf8(rendered).unwrap();
        let plain = rendered.replace(['\u{2068}', '\u{2069}'], "");
        assert!(rendered.contains("lua.execution"));
        assert!(plain.contains("相关错误 1"));
        assert!(rendered.contains("Lua Host 无法完成所有绑定资源的收尾"));
        for sentinel in ["LUA_VM_SENTINEL", "SQL_SENTINEL", "CLEANUP_SENTINEL"] {
            assert!(!rendered.contains(sentinel), "泄露了 {sentinel}");
        }

        let mapped = map_project_failure_report(report);
        assert!(matches!(
            &mapped,
            ProductionCommandError::StateAppliedButFinalizationFailed(_)
        ));
        let execution: DrivenCommand<Result<OperationCompletion<()>, ProductionCommandError>> =
            DrivenCommand::Finished(Err(mapped));
        let shutdown = ShutdownFailures::default();
        let failures = project_log_failure_diagnostics(&execution, &shutdown);
        assert_eq!(failures.len(), 2);
        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440014";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let active = ActiveProjectLog {
            run_id: run_id.to_owned(),
            runtime,
            logger,
            context: ProjectLogContext::new("zh-Hans").with_command("extract"),
            performance: Arc::new(RunPerformanceCounters::default()),
        };
        assert!(finish_project_log(active, ProjectLogRunOutcome::Failed, failures).is_none());
        let records = std::fs::read_to_string(directory.path().join(format!("{run_id}.jsonl")))
            .expect("项目日志应可读取");
        let relations = records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|record| record["code"] == "failure.reported")
            .map(|record| record["payload"]["relation"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(relations, vec!["primary".to_owned(), "related".to_owned()]);
    }

    #[tokio::test]
    async fn stored_run_plan_failure_keeps_query_counts_and_database_path() {
        let database_path = PathBuf::from("projects/demo/project.db");
        let repository = ProjectRunPlanPersistenceService::new(InvalidRunPlanSnapshotQuery);
        let source = repository
            .read(database_path.clone())
            .await
            .expect_err("缺少两组查询结果必须形成运行方案状态错误");

        let mapped = ProductionCommandError::project_run_plan_read(source);
        let diagnostic = mapped.failure_report().primary.public();

        assert_eq!(diagnostic.subject, DiagnosticSubject::path(&database_path));
        let serialized = serde_json::to_string(diagnostic).expect("运行方案诊断应可序列化");
        assert!(serialized.contains("应返回 2 组结果，实际为 0 组"));
        assert!(serialized.contains("projects/demo/project.db"));
    }

    #[test]
    fn translate_lua_runtime_and_cleanup_both_reach_command_failure_report() {
        let source: crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError<
            TestError,
            TestError,
        > = crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError::RuntimeAndCleanup {
            runtime: crate::rpg_maker::lua::runtime::TrustedLuaRuntimeExecutionError::Execute(
                TestError("TRANSLATE_LUA_VM_SENTINEL"),
            ),
            cleanup: crate::rpg_maker::lua::runtime::TrustedLuaBindingFinalizationError::new(
                "TRANSLATE_SQL_SENTINEL",
                Some(std::sync::Arc::new(TestError("TRANSLATE_CLEANUP_SENTINEL"))),
            ),
        };
        let error = LuaTranslationError::ExecuteHost {
            script_path: PathBuf::from("scripts/translate.lua"),
            source,
        };

        let mapped = map_translate_lua_error(PathBuf::from("scripts/translate.lua"), error);

        assert!(matches!(
            &mapped,
            ProductionCommandError::StateAppliedButFinalizationFailed(_)
        ));
        assert_eq!(mapped.failure_report().related.len(), 1);
        let mut rendered = Vec::new();
        render_failure_report(
            mapped.failure_report(),
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &mut rendered,
        )
        .expect("Translate Lua 主错和清理错应可渲染");
        let rendered = String::from_utf8(rendered).expect("诊断应为 UTF-8");
        assert!(rendered.contains("scripts/translate.lua"));
        assert!(rendered.contains("Lua Host 无法完成所有绑定资源的收尾"));
        for sentinel in [
            "TRANSLATE_LUA_VM_SENTINEL",
            "TRANSLATE_SQL_SENTINEL",
            "TRANSLATE_CLEANUP_SENTINEL",
        ] {
            assert!(!rendered.contains(sentinel), "泄露了 {sentinel}");
        }
    }

    #[test]
    fn translation_sqlite_finalization_preserves_connection_close_as_related_failure() {
        let source: RpgMakerStandardTranslationResultStorageError<TestError, TestError> =
            RpgMakerStandardTranslationResultStorageError::FinalizationFailed {
                database_path: PathBuf::from("projects/demo/project.db"),
                source: crate::storage::sqlite_session::SqliteInteractiveSessionFinalizationError::new(
                    crate::storage::sqlite_session::SqliteInteractiveSessionFinalizationFailure::CleanupFailed(
                        TestError("SQLITE_FINALIZATION_SENTINEL"),
                    ),
                    Some(TestError("SQLITE_CLOSE_SENTINEL")),
                ),
            };
        let report = source.into_result_storage_failure_report();

        assert_eq!(report.related.len(), 1);
        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::StateAppliedFinalizationFailed
        );
        assert_eq!(
            report.related[0].public().impact,
            DiagnosticImpact::StateAppliedFinalizationFailed
        );
        let serialized = serde_json::to_string(&report.public_diagnostics().collect::<Vec<_>>())
            .expect("SQLite 收尾诊断应可序列化");
        assert!(serialized.contains("projects/demo/project.db"));
        assert!(serialized.contains("cleanup_failed"));
        assert!(serialized.contains("sqlite_connection_close=failed"));
        assert!(!serialized.contains("SQLITE_FINALIZATION_SENTINEL"));
        assert!(!serialized.contains("SQLITE_CLOSE_SENTINEL"));

        let mapped = ProductionCommandError::external_model(TestError("MODEL_SENTINEL"))
            .with_related_finalization_report(report);
        assert!(matches!(
            &mapped,
            ProductionCommandError::StateAppliedButFinalizationFailed(_)
        ));
        assert_eq!(mapped.failure_report().related.len(), 2);
    }

    #[test]
    fn internal_failure_never_renders_its_source() {
        let error = ProductionCommandError::internal(TestError(
            "API_KEY_SENTINEL AUTHORIZATION_HEADER_SENTINEL CLIENT_PARAMETERS_SENTINEL \
             PROMPT_CONTENT_SENTINEL MODEL_BODY_SENTINEL SOURCE_TEXT_SENTINEL \
             TRANSLATION_TEXT_SENTINEL LUA_BODY_SENTINEL SQL_TEXT_SENTINEL \
             PANIC_PAYLOAD_SENTINEL",
        ));
        let mut stderr = Vec::new();

        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&error), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert!(stderr.contains("internal.operation"));
        assert!(stderr.contains("内部不变量被破坏；这是 ATT 缺陷"));
        for sentinel in [
            "API_KEY_SENTINEL",
            "AUTHORIZATION_HEADER_SENTINEL",
            "CLIENT_PARAMETERS_SENTINEL",
            "PROMPT_CONTENT_SENTINEL",
            "MODEL_BODY_SENTINEL",
            "SOURCE_TEXT_SENTINEL",
            "TRANSLATION_TEXT_SENTINEL",
            "LUA_BODY_SENTINEL",
            "SQL_TEXT_SENTINEL",
            "PANIC_PAYLOAD_SENTINEL",
        ] {
            assert!(!stderr.contains(sentinel), "泄露了 {sentinel}");
        }
    }

    #[test]
    fn builtin_placeholder_compile_failure_preserves_safe_pcre2_facts_without_source_text() {
        let build = ProductionTranslationExecutionBuildError::builtin_placeholder_compile(
            Pcre2PlaceholderConstructionError::for_test("(?<CLIENT_PARAMETERS_SENTINEL>"),
        );
        let error = TranslateServiceError::<
            TestError,
            ProductionTranslationExecutionBuildError,
            TestError,
            TestError,
            TestError,
        >::BuildExecution(build);

        let mapped = map_translate_error(
            error,
            ProductionCommandError::translation_execution_build,
            ProductionCommandError::external_model,
            |_, source| ProductionCommandError::internal(source),
        );
        assert!(matches!(&mapped, ProductionCommandError::Internal(_)));
        let diagnostic = mapped.failure_report().primary.public();
        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::CommandPreparation);
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::operation("builtin_placeholder_compile")
        );
        assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
        assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        let DiagnosticReason::FailureWithDetail { failure, detail } = &diagnostic.reason else {
            panic!("PCRE2 构造错误必须保留结构化底层事实");
        };
        assert_eq!(*failure, DiagnosticFailureKind::InternalInvariant);
        for expected in ["engine=pcre2", "kind=compile", "code=", "offset="] {
            assert!(detail.contains(expected), "缺少 {expected}: {detail}");
        }

        let mut stderr = Vec::new();
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        CommandResultRenderer::render_failure(Some(&mapped), None, &localizer, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let json = serde_json::to_string(diagnostic).expect("安全诊断应可序列化为项目日志 payload");

        assert!(stderr.contains("internal.operation"));
        assert!(stderr.contains("builtin_placeholder_compile"));
        for expected in ["engine=pcre2", "kind=compile", "code=", "offset="] {
            assert!(stderr.contains(expected), "CLI 缺少 {expected}: {stderr}");
            assert!(json.contains(expected), "JSONL 缺少 {expected}: {json}");
        }
        assert!(!stderr.contains("CLIENT_PARAMETERS_SENTINEL"));
        assert!(!json.contains("CLIENT_PARAMETERS_SENTINEL"));
    }
}
