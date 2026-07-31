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
use rusqlite::{Connection, OpenFlags};

use super::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError, SYSTEM_PROMPT_FILE_NAME,
    THINKING_PROMPT_FILE_NAME, ensure_no_prompt_template_variables_with_cancellation,
    parse_prompt_resource_with_cancellation, read_unparsed_prompt_resource,
    render_system_prompt_template_with_cancellation,
};
#[cfg(test)]
use super::translation_prompt::{
    ensure_no_prompt_template_variables, render_system_prompt_template,
};
use crate::application::config::{
    ConfigurationLoadError, ConfiguredExtractCommand, ConfiguredInitCommand,
    ConfiguredProjectLuaCommand, ConfiguredRpgMakerCommand, ConfiguredTranslateCommand,
    ConfiguredWriteBackCommand, PromptLocaleResolutionError, TranslateConfiguration,
};
use crate::diagnostic::{
    BoxedError, DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact,
    DiagnosticReason, DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact,
    ReportedFailure, SafeDiagnostic, SafeDiagnosticSource, render_failure_report,
    render_safe_diagnostic,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage, project_log_value_source_label};
use crate::language::LanguageModuleCatalogError;
use crate::progress::{
    ProgressAmount, ProgressMode, ProgressObserver, ProgressSnapshot, TerminalProgress,
    TerminalProgressObserver,
};
use crate::project_lease::{
    AlreadyHeldProjectCommandLeaseProvider, ProjectCommandLease, ProjectCommandLeaseError,
    ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::project_lua::{
    ProjectLuaCallError, ProjectLuaCancellation, ProjectLuaDatabasePrerequisiteError,
    ProjectLuaFailure, ProjectLuaPrintSink, ProjectLuaProgram, ProjectLuaProject,
    ProjectLuaRunError, ProjectLuaRunReport, ProjectLuaRunRequest, ProjectLuaSqliteError,
    compile_project_lua_program_with_cancellation,
    fingerprint_project_lua_program_with_cancellation, rpg_maker_project_lua_adapter,
    run_project_lua,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
};
use crate::rpg_maker::extract::builtin::{
    BuiltInExtractionError, BuiltInExtractionService, MvDialogueDefinitionSelection,
};
use crate::rpg_maker::extract::document::{
    CommandScopedRpgMakerDocumentReader, RpgMakerProjectDocumentReadingDiagnostic,
    RpgMakerProjectDocumentReadingService,
};
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
use crate::rpg_maker::project::{
    ExistingProjectOpener, ExistingProjectOpeningError, ExistingProjectOpeningService,
    OpenedProject,
};
use crate::rpg_maker::project_database::{
    ExtractRulesCanonicalJson, ExtractRunPlan, FinalProjectRunPlanPersistenceService, InitRunPlan,
    InvalidRunPlanValue, ProjectDatabaseCreateError, ProjectDatabaseCreationService,
    ProjectDatabaseInspectionError, ProjectDatabaseReadError, ProjectDatabaseReconciliationError,
    ProjectDatabaseRecordReadingService, ProjectDatabaseStateReconciliationService,
    ProjectRunPlanFinalizer, ProjectRunPlanPersistenceService, ProjectRunPlanReadError,
    ProjectRunPlanReplaceError, ProjectRunPlanReplacement, ProjectRunPlanRepository,
    ProjectWorkspaceLayout, TranslateRunPlan,
};
use crate::rpg_maker::translate::TranslateInput;
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::translate::asset_reader::RpgMakerTranslationAssetReadingService;
use crate::rpg_maker::translate::executor::{
    AsyncDelay, RpgMakerTranslationTaskExecutionError, RpgMakerTranslationTaskExecutionService,
    TranslationTaskResponseProcessingService,
};
use crate::rpg_maker::translate::pipeline::{
    RpgMakerTranslationLog, RpgMakerTranslationLogEvent, RpgMakerTranslationLogTaskOutcome,
    RpgMakerTranslationService,
};
use crate::rpg_maker::translate::placeholder::{
    Pcre2PlaceholderConstructionError, Pcre2PlaceholderService,
};
use crate::rpg_maker::translate::planner::RpgMakerTranslationTaskPlanningService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt, RpgMakerSystemPromptError,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
    TranslationResponseEnvelope,
};
use crate::rpg_maker::translate::result_store::{
    RpgMakerTranslationResultStorageError, RpgMakerTranslationResultStorageService,
};
use crate::rpg_maker::translate::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder, TranslateService,
    TranslateServiceError,
};
use crate::rpg_maker::translate::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::rpg_maker::write_back::asset_reader::RpgMakerWriteBackAssetReadingService;
use crate::rpg_maker::write_back::planner::{
    ConservativeRpgMakerWriteBackTextLayouter, RpgMakerWriteBackService,
};
use crate::rpg_maker::write_back::publisher::RpgMakerWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingService;
use crate::rpg_maker::write_back::{
    WriteBackInput, WriteBackLog, WriteBackLogEvent, WriteBackLogPublicationOutcome,
    WriteBackOutput, WriteBackProgressPhase, WriteBackPublishFailureState,
    WriteBackPublishingDiagnostic, WriteBackService, WriteBackServiceError,
};
use crate::runtime::cpu::{
    CpuExecutorConfig, CpuExecutorShutdownError, CpuExecutorStartError, CpuExecutorUnavailable,
    RayonCpuExecutor,
};
use crate::runtime::filesystem::{
    SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemConfig, SystemFileSystemError,
};
use crate::runtime::llm::{
    OpenAiChatCompletionClient, OpenAiChatCompletionError, OpenAiChatCompletionExecutor,
    OpenAiExecutorBuildError,
};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLog, ProjectLogAmount, ProjectLogCode, ProjectLogContext, ProjectLogEvent,
    ProjectLogLevel, ProjectLogNoWorkReason, ProjectLogPayload, ProjectLogPhase,
    ProjectLogRunOutcome, ProjectLogRuntime, ProjectLogValueSource, ProjectLogWarning,
    ProjectLogger, disabled_project_log, start_project_log,
};
use crate::runtime::run_id::generate_run_id;
use crate::runtime::sqlite::{
    RusqliteFinalTransactionExecutor, RusqliteStorage, RusqliteStorageConfiguration,
    SqliteRuntimeError,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError,
    DirectoryStageRequestError, DirectoryTreeFingerprintError, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFileError, ResolveDirectoryError, StagingCleanupFailure,
};
use crate::storage::sqlite::SnapshotDatabaseError;
use crate::translation::planning_resource::TranslationPlanningResourceReadingService;
use crate::user_text::sanitize_user_text;

const RPG_MAKER_PROMPT_DIRECTORY_NAME: &str = "rpg_maker";

#[derive(Clone, Copy, Debug, Default)]
struct TokioAsyncDelay;

/// Translate 终端只解释本纵向切片拥有的阶段；任务计数来自已提交终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslateProgressPhase {
    Planning,
    ConfirmedTasks,
    NoWork,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectLuaProgressPhase {
    Running,
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
        name: &crate::project_name::ProjectName,
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
    },
    WriteBack {
        output: WriteBackOutput,
    },
    Lua {
        project: crate::project_name::ProjectName,
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
    TerminalProgress::stderr(mode, move |phase| match phase {
        ExtractProgressPhase::Builtin => builtin.clone(),
        ExtractProgressPhase::BuiltinDocuments => builtin_documents.clone(),
        ExtractProgressPhase::BuiltinWorkUnits => builtin_work_units.clone(),
        ExtractProgressPhase::BuiltinCommit => builtin_commit.clone(),
        ExtractProgressPhase::Rules => rules.clone(),
        ExtractProgressPhase::RulesDocuments => rules_documents.clone(),
        ExtractProgressPhase::RulesMatches => rules_matches.clone(),
        ExtractProgressPhase::RulesCommit => rules_commit.clone(),
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

fn project_lua_terminal_progress(
    mode: ProgressMode,
    locale: UiLocale,
) -> TerminalProgress<ProjectLuaProgressPhase> {
    let running = UiLocalizer::new(locale).format(UiMessage::ProgressProjectLua);
    TerminalProgress::stderr(mode, move |_| running.clone())
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
    let validating = localizer.format(UiMessage::ProgressWriteBackValidateCandidate);
    let publishing = localizer.format(UiMessage::ProgressWriteBackPublish);
    TerminalProgress::stderr(mode, move |phase| match phase {
        WriteBackProgressPhase::ReadingAssets => reading.clone(),
        WriteBackProgressPhase::PlanningTranslations => planning.clone(),
        WriteBackProgressPhase::RewritingDocuments => rewriting.clone(),
        WriteBackProgressPhase::PreparingCandidate => preparing.clone(),
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
        if let Some(warning) = context.logger.and_then(|logger| logger.take_warning()) {
            for category in [warning.project_log, warning.task_records]
                .into_iter()
                .flatten()
            {
                if let Some(diagnostic) = category.diagnostic {
                    report = report.with_related(ReportedFailure::new(
                        diagnostic,
                        ObservabilityDegradedWhileReportingPanic,
                    ));
                }
                for diagnostic in category.related_diagnostics {
                    report = report.with_related(ReportedFailure::new(
                        diagnostic,
                        ObservabilityDegradedWhileReportingPanic,
                    ));
                }
            }
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
struct ObservabilityDegradedWhileReportingPanic;

impl fmt::Display for ObservabilityDegradedWhileReportingPanic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("observability degraded while reporting a command panic")
    }
}

impl Error for ObservabilityDegradedWhileReportingPanic {}

trait CommandRootShutdown: Send + Sync {
    type Error: Error + SafeDiagnosticSource + Send + Sync + 'static;

    const COMPONENT: &'static str;

    fn shutdown_root(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

impl CommandRootShutdown for RayonCpuExecutor {
    type Error = CpuExecutorShutdownError;

    const COMPONENT: &'static str = "CPU";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown()
    }
}

impl CommandRootShutdown for SystemFileSystem {
    type Error = SystemFileSystemError;

    const COMPONENT: &'static str = "FileSystem";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown().await
    }
}

impl CommandRootShutdown for RusqliteStorage {
    type Error = SqliteRuntimeError;

    const COMPONENT: &'static str = "SQLite";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown().await
    }
}

/// 拥有一次命令的必要非日志运行根，并以唯一逆序边界完成收尾。
///
/// 主命令按 CPU → FileSystem → SQLite 建立；Init 只使用 FileSystem → SQLite
/// 子集。启动中途失败与正常收尾都复用同一逆序关闭路径。
struct CommandRootGuard<C, F, S> {
    cpu: Option<C>,
    file_system: Option<F>,
    sqlite: Option<S>,
}

impl<C, F, S> CommandRootGuard<C, F, S> {
    const fn empty() -> Self {
        Self {
            cpu: None,
            file_system: None,
            sqlite: None,
        }
    }
}

impl<C, F, S> CommandRootGuard<C, F, S>
where
    C: CommandRootShutdown,
    F: CommandRootShutdown,
    S: CommandRootShutdown,
{
    async fn shutdown(mut self) -> ShutdownFailures {
        let mut failures = ShutdownFailures::default();
        shutdown_command_root(self.sqlite.take(), &mut failures).await;
        shutdown_command_root(self.file_system.take(), &mut failures).await;
        shutdown_command_root(self.cpu.take(), &mut failures).await;
        failures
    }
}

async fn shutdown_command_root<R>(root: Option<R>, failures: &mut ShutdownFailures)
where
    R: CommandRootShutdown,
{
    if let Some(root) = root
        && let Err(source) = root.shutdown_root().await
    {
        failures.push(R::COMPONENT, source);
    }
}

type ProductionCommandRootGuard =
    CommandRootGuard<RayonCpuExecutor, SystemFileSystem, RusqliteStorage>;

impl ProductionCommandRootGuard {
    async fn start_main(
        cpu_configuration: CpuExecutorConfig,
        file_system_configuration: SystemFileSystemConfig,
        sqlite_configuration: RusqliteStorageConfiguration,
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<Self, CommandRootStartupFailure> {
        let mut roots = Self::empty();
        roots.cpu = match RayonCpuExecutor::start(cpu_configuration) {
            Ok(cpu) => Some(cpu),
            Err(source) => {
                return Err(CommandRootStartupFailure::new(
                    ProductionCommandError::cpu_start(source),
                    roots.shutdown().await,
                ));
            }
        };
        roots.file_system = match SystemFileSystem::new_with_performance(
            file_system_configuration,
            Arc::clone(&performance),
        ) {
            Ok(file_system) => Some(file_system),
            Err(source) => {
                return Err(CommandRootStartupFailure::new(
                    ProductionCommandError::file_system_build(source),
                    roots.shutdown().await,
                ));
            }
        };
        roots.sqlite =
            match RusqliteStorage::start_with_performance(sqlite_configuration, performance) {
                Ok(sqlite) => Some(sqlite),
                Err(source) => {
                    return Err(CommandRootStartupFailure::new(
                        ProductionCommandError::sqlite_start(source),
                        roots.shutdown().await,
                    ));
                }
            };
        Ok(roots)
    }

    async fn start_init(
        file_system_configuration: SystemFileSystemConfig,
        sqlite_configuration: RusqliteStorageConfiguration,
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<Self, CommandRootStartupFailure> {
        let mut roots = Self::empty();
        roots.file_system = match SystemFileSystem::new_with_performance(
            file_system_configuration,
            Arc::clone(&performance),
        ) {
            Ok(file_system) => Some(file_system),
            Err(source) => {
                return Err(CommandRootStartupFailure::new(
                    ProductionCommandError::file_system_build(source),
                    roots.shutdown().await,
                ));
            }
        };
        roots.sqlite =
            match RusqliteStorage::start_with_performance(sqlite_configuration, performance) {
                Ok(sqlite) => Some(sqlite),
                Err(source) => {
                    return Err(CommandRootStartupFailure::new(
                        ProductionCommandError::sqlite_start(source),
                        roots.shutdown().await,
                    ));
                }
            };
        Ok(roots)
    }

    fn cpu(&self) -> &RayonCpuExecutor {
        self.cpu.as_ref().expect("主命令必须已启动 CPU 根")
    }

    fn file_system(&self) -> &SystemFileSystem {
        self.file_system
            .as_ref()
            .expect("命令必须已启动 FileSystem 根")
    }

    fn sqlite(&self) -> &RusqliteStorage {
        self.sqlite.as_ref().expect("命令必须已启动 SQLite 根")
    }
}

struct CommandRootStartupFailure {
    primary: ProductionCommandError,
    shutdown: ShutdownFailures,
}

impl CommandRootStartupFailure {
    const fn new(primary: ProductionCommandError, shutdown: ShutdownFailures) -> Self {
        Self { primary, shutdown }
    }

    fn into_report(self) -> ProductionCommandRunReport {
        if self.shutdown.is_empty() {
            ProductionCommandRunReport::failed_before_logging(self.primary)
        } else {
            ProductionCommandRunReport::failed_before_logging_with_shutdown(
                self.primary,
                self.shutdown,
            )
        }
    }
}

#[cfg(test)]
mod command_root_guard_tests {
    use super::*;

    trait TestRootComponent: Send + Sync {
        const NAME: &'static str;
    }

    struct TestCpu;
    struct TestFileSystem;
    struct TestSqlite;

    impl TestRootComponent for TestCpu {
        const NAME: &'static str = "CPU";
    }

    impl TestRootComponent for TestFileSystem {
        const NAME: &'static str = "FileSystem";
    }

    impl TestRootComponent for TestSqlite {
        const NAME: &'static str = "SQLite";
    }

    struct TestRoot<C> {
        failure: bool,
        observed: Arc<Mutex<Vec<&'static str>>>,
        _component: std::marker::PhantomData<C>,
    }

    impl<C> TestRoot<C> {
        fn new(failure: bool, observed: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                failure,
                observed,
                _component: std::marker::PhantomData,
            }
        }
    }

    #[derive(Debug)]
    struct TestRootShutdownError;

    impl fmt::Display for TestRootShutdownError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试命令根关闭失败")
        }
    }

    impl Error for TestRootShutdownError {}

    impl SafeDiagnosticSource for TestRootShutdownError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("test command root"),
                DiagnosticReason::failure(DiagnosticFailureKind::FinalizationFailed),
                impact,
                fallback_action,
            )
        }
    }

    impl<C> CommandRootShutdown for TestRoot<C>
    where
        C: TestRootComponent,
    {
        type Error = TestRootShutdownError;

        const COMPONENT: &'static str = C::NAME;

        async fn shutdown_root(&self) -> Result<(), Self::Error> {
            self.observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(C::NAME);
            if self.failure {
                Err(TestRootShutdownError)
            } else {
                Ok(())
            }
        }
    }

    type TestCommandRootGuard =
        CommandRootGuard<TestRoot<TestCpu>, TestRoot<TestFileSystem>, TestRoot<TestSqlite>>;

    fn root<C>(failure: bool, observed: &Arc<Mutex<Vec<&'static str>>>) -> TestRoot<C> {
        TestRoot::new(failure, Arc::clone(observed))
    }

    #[tokio::test]
    async fn every_started_prefix_and_init_subset_shutdown_in_reverse_order() {
        for (started, expected) in [
            ([false, false, false], Vec::<&'static str>::new()),
            ([true, false, false], vec!["CPU"]),
            ([true, true, false], vec!["FileSystem", "CPU"]),
            ([true, true, true], vec!["SQLite", "FileSystem", "CPU"]),
            ([false, true, false], vec!["FileSystem"]),
            ([false, true, true], vec!["SQLite", "FileSystem"]),
        ] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let roots = TestCommandRootGuard {
                cpu: started[0].then(|| root(false, &observed)),
                file_system: started[1].then(|| root(false, &observed)),
                sqlite: started[2].then(|| root(false, &observed)),
            };

            let failures = roots.shutdown().await;

            assert!(failures.is_empty());
            assert_eq!(
                *observed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                expected
            );
        }
    }

    #[tokio::test]
    async fn shutdown_continues_after_each_failure_and_preserves_failure_order() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let roots = TestCommandRootGuard {
            cpu: Some(root(true, &observed)),
            file_system: Some(root(true, &observed)),
            sqlite: Some(root(true, &observed)),
        };

        let failures = roots.shutdown().await;

        let expected = ["SQLite", "FileSystem", "CPU"];
        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            expected
        );
        assert_eq!(
            failures
                .failures
                .iter()
                .map(|failure| failure.component)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[tokio::test]
    async fn startup_primary_failure_is_not_replaced_by_partial_shutdown_failures() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let roots = TestCommandRootGuard {
            cpu: Some(root(true, &observed)),
            file_system: Some(root(true, &observed)),
            sqlite: None,
        };
        let shutdown = roots.shutdown().await;
        let report = CommandRootStartupFailure::new(
            ProductionCommandError::cpu_start(CpuExecutorStartError::TooManyWorkerThreads {
                requested: 2,
                maximum: 1,
            }),
            shutdown,
        )
        .into_report();

        assert!(matches!(
            report.result,
            CommandRunResult::Failed(ProductionCommandError::Internal(_))
        ));
        assert_eq!(
            report
                .shutdown_error
                .expect("部分启动根的关闭失败必须保留")
                .failures
                .iter()
                .map(|failure| failure.component)
                .collect::<Vec<_>>(),
            ["FileSystem", "CPU"]
        );
    }
}

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
                ConfiguredRpgMakerCommand::Lua(command) => {
                    self.run_atomic_project_lua(command, termination_signals)
                        .await
                }
            }
        })
        .await
    }

    async fn run_atomic_project_lua(
        self,
        command: ConfiguredProjectLuaCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = project_lua_terminal_progress(self.progress_mode, self.locale);
        let cancellation = CooperativeCancellation::default();
        let lua_cancellation = ProjectLuaCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let roots = match ProductionCommandRootGuard::start_init(
            command.common().filesystem().clone(),
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
        let script_path = command.script().script_path().to_path_buf();
        let script_read = drive_command(
            file_system.read_file(script_path),
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
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
            DrivenCommand::Interrupted(_) => {
                let shutdown = roots.shutdown().await;
                progress.finish();
                return ProductionCommandRunReport::interrupted_before_logging(shutdown);
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
                let shutdown = roots.shutdown().await;
                progress.finish();
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
            engine: self.layout.engine().storage_name(),
            project: project_name.as_str(),
            command: "lua",
            stage: DiagnosticStage::Lua,
            profile: None,
            performance,
            panic_boundary: Some(&self.panic_boundary),
        });

        let program_arguments = command.arguments().to_vec();
        let preflight_cancellation = lua_cancellation.clone();
        let preflight_logger = project_log.logger.clone();
        let preflight_context = project_log.context.clone();
        let preparation = drive_command(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let program =
                        ProjectLuaProgram::new(script_identity, script_source, program_arguments);
                    let fingerprint = fingerprint_project_lua_program_with_cancellation(
                        &program,
                        &preflight_cancellation,
                    )?;
                    preflight_logger.emit(ProjectLogEvent::new(
                        ProjectLogLevel::Info,
                        ProjectLogCode::LuaScript,
                        preflight_context,
                        ProjectLogPayload::LuaScript {
                            identity: program.identity().to_owned(),
                            fingerprint: fingerprint.hex(),
                        },
                    ));
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
                    Err(source) => Err(ProductionCommandError::project_lua_preflight(source)),
                }
            },
            termination_signals,
            || {
                cancellation.request();
                lua_cancellation.cancel();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let program = match preparation {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(prepared))) => prepared,
            terminal => {
                let execution = terminal.map(|result| {
                    result
                        .map(|_completion| OperationCompletion::<RpgMakerCommandOutput>::Cancelled)
                });
                let shutdown = roots.shutdown().await;
                progress.finish();
                let outcome = project_log_outcome(&execution, &shutdown);
                let diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
                let pending = PendingProjectLog::new(project_log, outcome, diagnostics);
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                let pending = PendingProjectLog::new(
                    project_log,
                    ProjectLogRunOutcome::Failed,
                    error
                        .failure_report()
                        .public_diagnostics()
                        .cloned()
                        .chain(shutdown.public_diagnostics().cloned())
                        .collect(),
                );
                return ProductionCommandRunReport::construction_failed_with_shutdown_and_project_log(
                    error,
                    shutdown,
                    Some(pending),
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let shutdown = roots.shutdown().await;
                progress.finish();
                let pending = PendingProjectLog::new(
                    project_log,
                    ProjectLogRunOutcome::Cancelled,
                    shutdown.public_diagnostics().cloned().collect(),
                );
                return ProductionCommandRunReport {
                    result: CommandRunResult::Interrupted,
                    shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
                    pending_project_log: Some(pending),
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
                let shutdown = roots.shutdown().await;
                progress.finish();
                let pending = PendingProjectLog::new(
                    project_log,
                    ProjectLogRunOutcome::Failed,
                    error
                        .failure_report()
                        .public_diagnostics()
                        .cloned()
                        .chain(shutdown.public_diagnostics().cloned())
                        .collect(),
                );
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
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let request = ProjectLuaRunRequest::new(
            ProjectLuaProject::new(project_name.as_str(), self.layout.engine().storage_name()),
            program,
            rpg_maker_project_lua_adapter(self.layout.engine(), lua_cancellation.clone()),
        )
        .with_cancellation(lua_cancellation.clone())
        .with_print_sink(Arc::new(ProjectLogLuaPrintSink::from_active(&project_log)));
        let execution = drive_command(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let connection = Connection::open_with_flags(
                        &database_path,
                        OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .map_err(|source| ProjectLuaExecutionError::Open {
                        path: database_path,
                        source,
                    })?;
                    run_project_lua(connection, request).map_err(ProjectLuaExecutionError::Run)
                })
                .await
                .map_err(ProductionCommandError::project_lua_worker)?;
                match result {
                    Ok(report) => Ok(OperationCompletion::Completed((
                        RpgMakerCommandOutput::Lua {
                            project: project_name,
                        },
                        report,
                    ))),
                    Err(ProjectLuaExecutionError::Run(error))
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        drop(project_lease_guard);
        progress_observer.finish();
        progress.finish();
        if let Some(report) = completed_project_lua_report(&execution) {
            project_log.logger.emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::LuaSummary,
                project_log.context.clone(),
                ProjectLogPayload::LuaSummary {
                    database_calls: report.database_calls(),
                    changed_rows: report.changed_rows(),
                    translation_calls: report.translation_calls(),
                    printed_lines: report.printed_lines(),
                },
            ));
        }
        let execution = execution.map(|result| {
            result.map(|completion| map_completion(completion, |(output, _report)| output))
        });
        let shutdown = roots.shutdown().await;
        let outcome = project_log_outcome(&execution, &shutdown);
        let diagnostics = project_log_failure_diagnostics(&execution, &shutdown);
        let pending = PendingProjectLog::new(project_log, outcome, diagnostics);
        ProductionCommandRunReport::from_completion_with_project_log(
            execution,
            shutdown,
            Some(pending),
        )
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
        let roots = match ProductionCommandRootGuard::start_init(
            command.common().filesystem().clone(),
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let arguments = &command.arguments;
        let project_name = arguments.project.name.clone();
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
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
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
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
                            let shutdown = roots.shutdown().await;
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
                        let shutdown = roots.shutdown().await;
                        drop(project_lease_guard);
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::run_plan_resolution(
                                RunPlanResolutionError::InitPathRequired,
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
            }
        };
        let resolved_game_root = match file_system.resolve_existing_directory(game_root).await {
            Ok(path) => path,
            Err(error) => {
                let shutdown = roots.shutdown().await;
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
            AlreadyHeldProjectCommandLeaseProvider::new(&project_lease_guard),
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
        let shutdown = roots.shutdown().await;
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
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: self.layout.engine().storage_name(),
            project: command.arguments.project.name.as_str(),
            command: "init",
            stage: DiagnosticStage::Init,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: Some(&self.panic_boundary),
        });
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            ProjectLogPayload::RunPlan {
                source: plan_source,
                selections: vec![resolved_game_root.to_string_lossy().into_owned()],
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
        let roots = match ProductionCommandRootGuard::start_main(
            command.cpu(),
            command.common().filesystem().clone(),
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
                progress.safe_stopping(progress_safe_stopping(self.locale));
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
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
                progress.finish();
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: self.layout.engine().storage_name(),
            project: command.project_name().as_str(),
            command: "extract",
            stage: DiagnosticStage::Extract,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: Some(&self.panic_boundary),
        });
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
                        let shutdown = roots.shutdown().await;
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
                        let shutdown = roots.shutdown().await;
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
                        let shutdown = roots.shutdown().await;
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
        let disabled_owners = [command
            .rpg_maker()
            .rules()
            .zip(rules_program.as_ref())
            .filter(|(_, program)| program.is_empty())
            .map(|_| "Rules")]
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
                selections,
            },
        ));
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
        let service = ExtractService::new(
            opener,
            builtin,
            selected_rules,
            AlreadyHeldProjectCommandLeaseProvider::new(&project_lease_guard),
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
        let shutdown = roots.shutdown().await;
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
        if let Some(output) = completed_output(&execution) {
            for warning in &output.rules_warnings {
                project_log.logger.emit(ProjectLogEvent::new(
                    ProjectLogLevel::Warn,
                    ProjectLogCode::ExtractRulesCommandNonStringSkipped,
                    project_log.context.clone(),
                    ProjectLogPayload::RulesCommandNonStringSkipped {
                        rule_number: u64::try_from(warning.rule_number).unwrap_or(u64::MAX),
                        source_file: warning.source_file.clone(),
                        command_code: warning.command_code,
                        parameter: u64::try_from(warning.parameter).unwrap_or(u64::MAX),
                        actual_type: warning.actual_type.as_str().to_owned(),
                        skipped_count: warning.skipped_count,
                    },
                ));
            }
        }
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
        let file_system_configuration = command.common().filesystem().clone();
        let roots = match ProductionCommandRootGuard::start_main(
            command.cpu(),
            file_system_configuration.clone(),
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
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
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
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
        let mut project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: self.layout.engine().storage_name(),
            project: command.project_name().as_str(),
            command: "translate",
            stage: DiagnosticStage::Translate,
            profile: Some(&profile_id),
            performance: Arc::clone(&performance),
            panic_boundary: Some(&self.panic_boundary),
        });
        project_log.set_profile(&profile_id);
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            translate_phase_code,
        );
        let replacement = match TranslateRunPlan::new(profile_id.clone()) {
            Ok(plan) => ProjectRunPlanReplacement::Translate(plan),
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
        project_log.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunPlanResolved,
            project_log.context.clone(),
            ProjectLogPayload::RunPlan {
                source: profile_source,
                selections: vec![profile_id.clone()],
            },
        ));
        let additional_pem_roots =
            match load_additional_pem_roots(&file_system, command.llm()).await {
                Ok(value) => value,
                Err(error) => {
                    let shutdown = roots.shutdown().await;
                    drop(project_lease_guard);
                    return observed_construction_failure(project_log, error, shutdown).await;
                }
            };
        let llm = match OpenAiChatCompletionExecutor::new(
            command.llm().with_pem_roots(additional_pem_roots),
        )
        .map_err(ProductionCommandError::http_client_build)
        {
            Ok(value) => value,
            Err(error) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return observed_construction_failure(project_log, error, shutdown).await;
            }
        };
        let opener = PreopenedProject::new(opened_project);
        let business_log =
            ProductionBusinessLog::for_translation(&project_log, progress_observer.clone());
        let (task_records, record_translation_tasks) = if let (true, Some(run_id)) =
            (command.record_translation_tasks(), project_log.run_id())
        {
            match SystemFileSystem::new_with_performance(
                file_system_configuration,
                Arc::clone(&performance),
            ) {
                Ok(observation_file_system) => (
                    ConfiguredTranslationTaskRecordSink::Markdown(Box::new(
                        MarkdownTranslationTaskRecordSink::new(
                            project_workspace
                                .workspace_root()
                                .join("task-records")
                                .join(run_id),
                            run_id.to_owned(),
                            command.translation().client().record_metadata(),
                            self.locale,
                            cpu.clone(),
                            observation_file_system,
                            project_log.logger.clone(),
                        ),
                    )),
                    true,
                ),
                Err(error) => {
                    project_log
                        .logger
                        .record_task_record_failure(error.safe_diagnostic());
                    (ConfiguredTranslationTaskRecordSink::disabled(), false)
                }
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
        let service = TranslateService::new(
            opener,
            builder,
            AlreadyHeldProjectCommandLeaseProvider::new(&project_lease_guard),
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
        .map(|result| result.map_err(map_translate_error));
        if matches!(&execution, DrivenCommand::Interrupted(_)) {
            let (confirmed, total) = progress_observer.confirmed_amount();
            project_log.emit_cancellation(ProjectLogCode::SafeStopFinished, confirmed, total);
        }
        business_log.emit_retry_summary();
        let no_model_work = matches!(
            &execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
                if output.summary.total_tasks == 0
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
        llm.shutdown().await;
        let shutdown = roots.shutdown().await;
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
        // 任务记录拥有独立的终态观察文件根；运行方案终态固定后才渲染、写入并关闭该根。
        // 因此旁路慢写、故障或此时到达的信号都不能改变业务、取消或运行方案语义。
        task_records.finish().await;
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
        let roots = match ProductionCommandRootGuard::start_main(
            command.cpu(),
            command.common().filesystem().clone(),
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
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
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
                progress.finish();
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
            || progress.safe_stopping(progress_safe_stopping(self.locale)),
        )
        .await;
        let opened_project = match project_opening {
            DrivenCommand::Finished(Ok(project)) => project,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                drop(result);
                let shutdown = roots.shutdown().await;
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
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                progress.finish();
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: self.layout.engine().storage_name(),
            project: command.project_name().as_str(),
            command: "write-back",
            stage: DiagnosticStage::WriteBack,
            profile: None,
            performance: Arc::clone(&performance),
            panic_boundary: Some(&self.panic_boundary),
        });
        let progress_observer = ProductionProgressObserver::new(
            progress.observer(),
            &project_log,
            write_back_phase_code,
        );
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let opener = PreopenedProject::new(opened_project);
        let asset_reader = RpgMakerWriteBackAssetReadingService::new(sqlite.clone(), cpu.clone());
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
            ConservativeRpgMakerWriteBackTextLayouter,
            rewriter,
            cpu.clone(),
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let publisher = RpgMakerWriteBackPublishingService::new(directory_publisher.clone());
        let service = WriteBackService::new(
            opener,
            write_back,
            publisher,
            ProductionBusinessLog::from_active(&project_log),
            AlreadyHeldProjectCommandLeaseProvider::new(&project_lease_guard),
            cancellation.clone(),
        )
        .with_progress(progress_observer.clone());
        let input = WriteBackInput {
            name: command.project_name().clone(),
        };
        let safe_stopping = progress_safe_stopping(self.locale);
        let execution = drive_command(
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
        let shutdown = roots.shutdown().await;
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

pub(crate) struct ActiveProjectLog {
    run_id: Option<String>,
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
    pub(crate) fn new(
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
        let mut failures = error
            .failure_report()
            .public_diagnostics()
            .cloned()
            .collect::<Vec<_>>();
        failures.extend(self.failures);
        finish_project_log(self.active, ProjectLogRunOutcome::Failed, failures)
    }

    pub(crate) fn finish_with_diagnostic(
        mut self,
        diagnostic: SafeDiagnostic,
    ) -> Option<ProjectLogWarning> {
        self.failures.insert(0, diagnostic);
        finish_project_log(self.active, ProjectLogRunOutcome::Failed, self.failures)
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
    }
}

const fn translate_phase_code(phase: TranslateProgressPhase) -> ProjectLogPhase {
    match phase {
        TranslateProgressPhase::Planning => ProjectLogPhase::Planning,
        TranslateProgressPhase::ConfirmedTasks => ProjectLogPhase::ConfirmedTasks,
        TranslateProgressPhase::NoWork => ProjectLogPhase::NoWork,
    }
}

const fn project_lua_phase_code(_: ProjectLuaProgressPhase) -> ProjectLogPhase {
    ProjectLogPhase::Lua
}

const fn write_back_phase_code(phase: WriteBackProgressPhase) -> ProjectLogPhase {
    match phase {
        WriteBackProgressPhase::ReadingAssets => ProjectLogPhase::ReadAssets,
        WriteBackProgressPhase::PlanningTranslations => ProjectLogPhase::PlanRpgMakerWriteBack,
        WriteBackProgressPhase::RewritingDocuments => ProjectLogPhase::RewriteDocuments,
        WriteBackProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        WriteBackProgressPhase::ValidatingCandidate => ProjectLogPhase::ValidateCandidate,
        WriteBackProgressPhase::Publishing => ProjectLogPhase::Publish,
    }
}

impl ActiveProjectLog {
    pub(crate) fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub(crate) fn set_profile(&mut self, profile: &str) {
        self.context = self.context.clone().with_profile(profile);
    }

    pub(crate) fn logger(&self) -> &ProjectLogger {
        &self.logger
    }

    pub(crate) fn context(&self) -> &ProjectLogContext {
        &self.context
    }

    pub(crate) fn performance(&self) -> &Arc<RunPerformanceCounters> {
        &self.performance
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
        ConfiguredRpgMakerCommand::Lua(command) => (
            "lua",
            DiagnosticStage::Lua,
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

pub(crate) struct CommandLogStart<'a> {
    pub(crate) common: &'a crate::application::config::CommonCommandConfiguration,
    pub(crate) locale: UiLocale,
    pub(crate) engine: &'a str,
    pub(crate) project: &'a str,
    pub(crate) command: &'static str,
    pub(crate) stage: DiagnosticStage,
    pub(crate) profile: Option<&'a str>,
    pub(crate) performance: Arc<RunPerformanceCounters>,
    pub(crate) panic_boundary: Option<&'a CommandPanicBoundary>,
}

pub(crate) fn start_command_log(input: CommandLogStart<'_>) -> ActiveProjectLog {
    start_command_log_with_run_id(input, generate_run_id())
}

fn start_command_log_with_run_id(
    input: CommandLogStart<'_>,
    generated_run_id: Result<crate::observability::RunId, WindowsFsError>,
) -> ActiveProjectLog {
    let CommandLogStart {
        common,
        locale,
        engine,
        project,
        command,
        stage,
        profile,
        performance,
        panic_boundary,
    } = input;
    let logs_root = common
        .projects_root()
        .join(engine)
        .join(project)
        .join("logs");
    let project_workspace = logs_root
        .parent()
        .expect("logs 路径必须位于项目工作区内")
        .to_path_buf();
    let (run_id, mut runtime) = start_project_log_with_run_id(logs_root, generated_run_id);
    let logger = runtime.logger();
    let mut context = ProjectLogContext::new(locale.as_str())
        .with_engine(engine)
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
    if let Some(panic_boundary) = panic_boundary {
        panic_boundary.register_project_log(vec![panic_diagnostic], logger.clone());
    }
    logger.emit(ProjectLogEvent::new(
        ProjectLogLevel::Info,
        ProjectLogCode::RunStarted,
        context.clone(),
        ProjectLogPayload::Run { outcome: None },
    ));
    ActiveProjectLog {
        run_id,
        runtime,
        logger,
        context,
        performance,
    }
}

fn start_project_log_with_run_id(
    logs_root: PathBuf,
    generated_run_id: Result<crate::observability::RunId, WindowsFsError>,
) -> (Option<String>, ProjectLogRuntime) {
    match generated_run_id {
        Ok(run_id) => {
            let run_id = run_id.to_string();
            let runtime = start_project_log(logs_root, run_id.clone());
            (Some(run_id), runtime)
        }
        Err(source) => (
            None,
            disabled_project_log(run_id_failure_diagnostic(&source)),
        ),
    }
}

fn run_id_failure_diagnostic(source: &WindowsFsError) -> SafeDiagnostic {
    source.safe_diagnostic(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::ProcessStartup,
        DiagnosticImpact::Unchanged,
        DiagnosticAction::Retry,
    )
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
    // 信号到达后业务仍自然冲线成功属于完整业务完成：结果已生效，
    // 运行方案照常保存，最终按成功呈现，不降级为“已取消”。
    matches!(
        execution,
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(_)))
    )
}

fn completed_output<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> Option<&T> {
    match execution {
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(output))) => Some(output),
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Finished(Err(_))
        | DrivenCommand::Interrupted(Err(_))
        | DrivenCommand::SignalFailed { .. } => None,
    }
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
        DrivenCommand::Finished(result) => match observe_run_plan_result(result, project_log) {
            Ok(()) => finish_successful_execution(execution),
            Err(error) => replace_success_with_plan_error(execution, Err(error)),
        },
        DrivenCommand::Interrupted(result) => match result {
            // 保存期间到达信号但保存已成功：业务结果与运行方案都已生效。
            // 最终命令状态只表达已经完整完成的结果，不保留过时的中断形态。
            Ok(()) => {
                emit_run_plan_saved(project_log);
                finish_successful_execution(execution)
            }
            // 信号取消了方案保存本身：业务结果已完整生效并按成功呈现，
            // 方案未保存进入项目日志，由下次运行重新提供输入。
            Err(error) if run_plan_wait_was_cancelled(&error) => {
                emit_run_plan_error_fact(&error, project_log);
                finish_successful_execution(execution)
            }
            Err(error) => {
                DrivenCommand::Interrupted(Err(observe_run_plan_error(error, project_log)))
            }
        },
        DrivenCommand::SignalFailed { source, result } => {
            let result = match result {
                Ok(()) => {
                    emit_run_plan_saved(project_log);
                    take_successful_execution_result(execution)
                }
                Err(error) if run_plan_wait_was_cancelled(&error) => {
                    emit_run_plan_error_fact(&error, project_log);
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
            emit_run_plan_saved(project_log);
            Ok(())
        }
        Err(error) => Err(observe_run_plan_error(error, project_log)),
    }
}

fn emit_run_plan_saved(project_log: &ActiveProjectLog) {
    project_log.logger.emit(ProjectLogEvent::new(
        ProjectLogLevel::Info,
        ProjectLogCode::RunPlanSaved,
        project_log.context.clone(),
        ProjectLogPayload::None,
    ));
}

fn observe_run_plan_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
    project_log: &ActiveProjectLog,
) -> ProductionCommandError {
    emit_run_plan_error_fact(&error, project_log);
    map_run_plan_replace_error(error)
}

fn emit_run_plan_error_fact<E>(
    error: &ProjectRunPlanReplaceError<E>,
    project_log: &ActiveProjectLog,
) {
    let (level, code) = run_plan_replace_log_fact(error);
    project_log.logger.emit(ProjectLogEvent::new(
        level,
        code,
        project_log.context.clone(),
        ProjectLogPayload::None,
    ));
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

/// 运行方案最终化没有产生根失败时，业务 `Completed` 是命令唯一有效的最终状态。
fn finish_successful_execution<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    DrivenCommand::Finished(take_successful_execution_result(execution))
}

fn take_successful_execution_result<T>(
    execution: DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) -> Result<OperationCompletion<T>, ProductionCommandError> {
    match execution {
        DrivenCommand::Finished(result @ Ok(OperationCompletion::Completed(_)))
        | DrivenCommand::Interrupted(result @ Ok(OperationCompletion::Completed(_))) => result,
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

#[derive(Clone)]
pub(crate) struct ProjectLogLuaPrintSink {
    logger: ProjectLogger,
    context: ProjectLogContext,
}

impl ProjectLogLuaPrintSink {
    pub(crate) fn from_active(project_log: &ActiveProjectLog) -> Self {
        Self {
            logger: project_log.logger.clone(),
            context: project_log.context.clone(),
        }
    }
}

impl ProjectLuaPrintSink for ProjectLogLuaPrintSink {
    fn print(&self, bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::LuaPrint,
            self.context.clone(),
            ProjectLogPayload::LuaPrint {
                message: String::from_utf8_lossy(bytes).into_owned(),
            },
        ));
        Ok(())
    }
}

#[derive(Debug)]
enum ProjectLuaExecutionError {
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    Run(ProjectLuaRunError),
}

impl fmt::Display for ProjectLuaExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => write!(
                formatter,
                "无法打开 RPG Maker 项目数据库 {}：{source}",
                path.display()
            ),
            Self::Run(source) => source.fmt(formatter),
        }
    }
}

impl Error for ProjectLuaExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Run(source) => Some(source),
        }
    }
}

#[derive(Debug)]
struct ProjectLuaPreflightError(ProjectLuaFailure);

impl fmt::Display for ProjectLuaPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for ProjectLuaPreflightError {}

fn project_lua_sqlite_reason(
    source: &ProjectLuaSqliteError,
    fallback: DiagnosticFailureKind,
) -> DiagnosticReason {
    match source.sqlite_codes() {
        Some((primary_code, extended_code)) => DiagnosticReason::Sqlite {
            primary_code,
            extended_code,
        },
        None => DiagnosticReason::failure(fallback),
    }
}

fn project_lua_run_was_cancelled(error: &ProjectLuaRunError) -> bool {
    matches!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
            | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    )
}

fn completed_project_lua_report(
    execution: &DrivenCommand<
        Result<
            OperationCompletion<(RpgMakerCommandOutput, ProjectLuaRunReport)>,
            ProductionCommandError,
        >,
    >,
) -> Option<ProjectLuaRunReport> {
    match execution {
        DrivenCommand::Finished(Ok(OperationCompletion::Completed((_, report))))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed((_, report))))
        | DrivenCommand::SignalFailed {
            result: Ok(OperationCompletion::Completed((_, report))),
            ..
        } => Some(*report),
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Finished(Err(_))
        | DrivenCommand::Interrupted(Err(_))
        | DrivenCommand::SignalFailed { .. } => None,
    }
}

impl RpgMakerTranslationLog for ProductionBusinessLog {
    fn emit(&self, event: RpgMakerTranslationLogEvent) {
        match event {
            RpgMakerTranslationLogEvent::PlanningUnresolved { units } => {
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
            RpgMakerTranslationLogEvent::TaskStarted {
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
            RpgMakerTranslationLogEvent::TaskFinished {
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
                    RpgMakerTranslationLogTaskOutcome::Complete
                        | RpgMakerTranslationLogTaskOutcome::Partial
                        | RpgMakerTranslationLogTaskOutcome::Unavailable
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
                    RpgMakerTranslationLogTaskOutcome::Complete => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Complete
                    }
                    RpgMakerTranslationLogTaskOutcome::Partial => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Partial
                    }
                    RpgMakerTranslationLogTaskOutcome::Unavailable => {
                        crate::runtime::project_log::ProjectLogTaskOutcome::Unavailable
                    }
                    RpgMakerTranslationLogTaskOutcome::ExecutionFailed
                    | RpgMakerTranslationLogTaskOutcome::CommitFailed
                    | RpgMakerTranslationLogTaskOutcome::NotCommitted
                    | RpgMakerTranslationLogTaskOutcome::InvalidResult => {
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
                    WriteBackLogPublicationOutcome::Published { summary, .. } => (
                        ProjectLogLevel::Info,
                        LogOutcome::Published,
                        u64::try_from(summary.manual_layout_units).unwrap_or(u64::MAX),
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
    RpgMakerTranslationAssetReadingService<RusqliteStorage, RayonCpuExecutor>;
type ProductionTranslationPlanner = RpgMakerTranslationTaskPlanningService<
    TranslationPlanningResourceReadingService<SystemFileSystem, RayonCpuExecutor>,
    RayonCpuExecutor,
    OpenAiChatCompletionClient,
>;
type ProductionTranslationExecutor = RpgMakerTranslationTaskExecutionService<
    OpenAiChatCompletionExecutor,
    TokioAsyncDelay,
    TranslationTaskResponseProcessingService<RayonCpuExecutor>,
    ProductionTranslationProfile,
>;
type ProductionTranslationStore =
    RpgMakerTranslationResultStorageService<RusqliteStorage, RayonCpuExecutor>;
type ProductionRpgMakerTranslation = RpgMakerTranslationService<
    ProductionTranslationAssetReader,
    ProductionTranslationPlanner,
    ProductionTranslationExecutor,
    ProductionTranslationStore,
    ProductionBusinessLog,
    ConfiguredTranslationTaskRecordSink,
>;

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

#[derive(Debug)]
enum RpgMakerPromptPreparationError {
    Cancelled,
    SystemResource(PromptResourceLoadError),
    ThinkingResource(PromptResourceLoadError),
    SystemTemplate(PromptTemplateError),
    ThinkingTemplate(PromptTemplateError),
    SystemPrompt(RpgMakerSystemPromptError),
}

fn ensure_rpg_maker_prompt_preparation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), RpgMakerPromptPreparationError> {
    if cancellation.is_requested() {
        Err(RpgMakerPromptPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

fn append_rpg_maker_prompt_text(
    output: &mut String,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), RpgMakerPromptPreparationError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < text.len() {
        ensure_rpg_maker_prompt_preparation_running(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_rpg_maker_prompt_preparation_running(cancellation)
}

fn assemble_rpg_maker_system_prompt_markdown_with_cancellation(
    rendered_system: String,
    thinking: Option<String>,
    cancellation: &CooperativeCancellation,
) -> Result<(String, TranslationResponseEnvelope), RpgMakerPromptPreparationError> {
    ensure_rpg_maker_prompt_preparation_running(cancellation)?;
    let mut prompt = rendered_system;
    let response_envelope = if let Some(thinking) = thinking {
        prompt.push_str("\n\n");
        append_rpg_maker_prompt_text(&mut prompt, &thinking, cancellation)?;
        TranslationResponseEnvelope::ThinkingThenJson
    } else {
        TranslationResponseEnvelope::JsonOnly
    };
    ensure_rpg_maker_prompt_preparation_running(cancellation)?;
    Ok((prompt, response_envelope))
}

#[cfg(test)]
fn assemble_rpg_maker_system_prompt_markdown(
    rendered_system: String,
    thinking: Option<&str>,
) -> (String, TranslationResponseEnvelope) {
    assemble_rpg_maker_system_prompt_markdown_with_cancellation(
        rendered_system,
        thinking.map(str::to_owned),
        &CooperativeCancellation::default(),
    )
    .expect("未请求取消时应完成 Prompt 拼接")
}

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

    #[test]
    fn rpg_maker_prompt_has_exact_json_only_assembly() {
        let (prompt, envelope) =
            assemble_rpg_maker_system_prompt_markdown("rendered system".to_owned(), None);

        assert_eq!(prompt, "rendered system");
        assert_eq!(envelope, TranslationResponseEnvelope::JsonOnly);
    }

    #[test]
    fn rpg_maker_prompt_has_exact_thinking_assembly() {
        let (prompt, envelope) = assemble_rpg_maker_system_prompt_markdown(
            "rendered system".to_owned(),
            Some("thinking requirement"),
        );

        assert_eq!(prompt, "rendered system\n\nthinking requirement");
        assert_eq!(envelope, TranslationResponseEnvelope::ThinkingThenJson);
    }
}

struct ProductionSelectedTranslationExecutionBuilder<'a> {
    configuration: &'a TranslateConfiguration,
    file_system: SystemFileSystem,
    cpu: RayonCpuExecutor,
    sqlite: RusqliteStorage,
    llm: OpenAiChatCompletionExecutor,
    log: ProductionBusinessLog,
    task_records: ConfiguredTranslationTaskRecordSink,
    record_translation_tasks: bool,
    cancellation: CooperativeCancellation,
}

async fn build_production_translation_profile(
    configuration: &TranslateConfiguration,
    file_system: &SystemFileSystem,
    cpu: &RayonCpuExecutor,
    project: &OpenedProject,
    cancellation: &CooperativeCancellation,
) -> Result<
    (
        ProductionTranslationProfile,
        Arc<ResolvedRpgMakerTranslationResources>,
    ),
    ProductionTranslationExecutionBuildError,
> {
    ensure_translation_execution_build_running(cancellation)?;
    let profile_configuration = configuration.profile();
    let language_pair = project.language_pair().clone();
    let prompt_locale = configuration
        .prompt_locale()
        .resolve(language_pair.target())
        .map_err(ProductionTranslationExecutionBuildError::prompt_locale)?;
    let prompt_directory = configuration
        .prompt_root()
        .join(RPG_MAKER_PROMPT_DIRECTORY_NAME)
        .join(prompt_locale.as_str());
    let system_path = prompt_directory.join(SYSTEM_PROMPT_FILE_NAME);
    ensure_translation_execution_build_running(cancellation)?;
    let system_template = read_unparsed_prompt_resource(file_system, &system_path).await;
    ensure_translation_execution_build_running(cancellation)?;
    let system_template = system_template.map_err(|source| {
        ProductionTranslationExecutionBuildError::prompt_resource(
            prompt_locale,
            PromptResourceComponent::System,
            &system_path,
            source,
        )
    })?;
    let thinking_path = configuration
        .thinking_output()
        .then(|| prompt_directory.join(THINKING_PROMPT_FILE_NAME));
    let thinking = if let Some(path) = thinking_path.as_deref() {
        ensure_translation_execution_build_running(cancellation)?;
        let thinking = read_unparsed_prompt_resource(file_system, path).await;
        ensure_translation_execution_build_running(cancellation)?;
        Some(thinking.map_err(|source| {
            ProductionTranslationExecutionBuildError::prompt_resource(
                prompt_locale,
                PromptResourceComponent::Thinking,
                path,
                source,
            )
        })?)
    } else {
        None
    };

    let prompt_language_pair = language_pair.clone();
    let prompt_cancellation = cancellation.clone();
    ensure_translation_execution_build_running(cancellation)?;
    let system_prompt = cpu
        .execute(move || {
            ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)?;
            let system_template = parse_prompt_resource_with_cancellation(system_template, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::SystemResource)?;
            let rendered_system = render_system_prompt_template_with_cancellation(
                &system_template,
                &prompt_language_pair,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?
            .map_err(RpgMakerPromptPreparationError::SystemTemplate)?;
            let thinking = match thinking {
                Some(thinking) => {
                    let thinking = parse_prompt_resource_with_cancellation(thinking, || {
                        ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
                    })?
                    .map_err(RpgMakerPromptPreparationError::ThinkingResource)?;
                    ensure_no_prompt_template_variables_with_cancellation(&thinking, || {
                        ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
                    })?
                    .map_err(RpgMakerPromptPreparationError::ThinkingTemplate)?;
                    Some(thinking)
                }
                None => None,
            };
            let (prompt_markdown, response_envelope) =
                assemble_rpg_maker_system_prompt_markdown_with_cancellation(
                    rendered_system,
                    thinking,
                    &prompt_cancellation,
                )?;
            RpgMakerSystemPrompt::new_with_cancellation(
                prompt_language_pair,
                prompt_markdown,
                response_envelope,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?
            .map_err(RpgMakerPromptPreparationError::SystemPrompt)
        })
        .await;
    ensure_translation_execution_build_running(cancellation)?;
    let system_prompt = system_prompt
        .map_err(ProductionTranslationExecutionBuildError::prompt_cpu)?
        .map_err(|source| match source {
            RpgMakerPromptPreparationError::Cancelled => {
                ProductionTranslationExecutionBuildError::cancelled()
            }
            RpgMakerPromptPreparationError::SystemResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    prompt_locale,
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ThinkingResource(source) => {
                let path = thinking_path
                    .as_deref()
                    .expect("thinking Prompt 错误只会在启用 thinking 输出时产生");
                ProductionTranslationExecutionBuildError::prompt_resource(
                    prompt_locale,
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    prompt_locale,
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ThinkingTemplate(source) => {
                let path = thinking_path
                    .as_deref()
                    .expect("thinking Prompt 错误只会在启用 thinking 输出时产生");
                ProductionTranslationExecutionBuildError::prompt_template(
                    prompt_locale,
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemPrompt(source) => {
                ProductionTranslationExecutionBuildError::system_prompt(
                    prompt_locale,
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
        })?;
    ensure_translation_execution_build_running(cancellation)?;
    let source_language = configuration
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
        Arc::clone(configuration.client()),
    ));
    ensure_translation_execution_build_running(cancellation)?;
    Ok((profile, translation_resources))
}

impl SelectedTranslationExecutionBuilder for ProductionSelectedTranslationExecutionBuilder<'_> {
    type Client = OpenAiChatCompletionClient;
    type Translation = ProductionRpgMakerTranslation;
    type Error = ProductionTranslationExecutionBuildError;

    async fn build(
        &self,
        project: &crate::rpg_maker::project::OpenedProject,
    ) -> Result<SelectedTranslationExecution<Self::Client, Self::Translation>, Self::Error> {
        let (profile, translation_resources) = build_production_translation_profile(
            self.configuration,
            &self.file_system,
            &self.cpu,
            project,
            &self.cancellation,
        )
        .await?;
        ensure_translation_execution_build_running(&self.cancellation)?;
        let placeholder_cancellation = self.cancellation.clone();
        let placeholders = self
            .cpu
            .execute(move || {
                Pcre2PlaceholderService::new_with_cancellation(|| {
                    if placeholder_cancellation.is_requested() {
                        Err(TranslationExecutionBuildCancelled)
                    } else {
                        Ok(())
                    }
                })
            })
            .await;
        ensure_translation_execution_build_running(&self.cancellation)?;
        let placeholders = placeholders
            .map_err(ProductionTranslationExecutionBuildError::placeholder_cpu)?
            .map_err(|_cancelled| ProductionTranslationExecutionBuildError::cancelled())?
            .map_err(ProductionTranslationExecutionBuildError::builtin_placeholder_compile)?;
        let asset_reader =
            RpgMakerTranslationAssetReadingService::new(self.sqlite.clone(), self.cpu.clone());
        let resources = TranslationPlanningResourceReadingService::new(
            self.file_system.clone(),
            self.cpu.clone(),
        )
        .with_cancellation(self.cancellation.clone());
        let planner =
            RpgMakerTranslationTaskPlanningService::<_, _, OpenAiChatCompletionClient>::new(
                resources,
                Arc::clone(&translation_resources),
                placeholders,
                self.cpu.clone(),
            )
            .with_cancellation(self.cancellation.clone());
        let processor =
            TranslationTaskResponseProcessingService::new(self.cpu.clone(), translation_resources)
                .with_cancellation(self.cancellation.clone());
        let executor =
            RpgMakerTranslationTaskExecutionService::<_, _, _, ProductionTranslationProfile>::new(
                self.llm.clone(),
                TokioAsyncDelay,
                processor,
                self.cancellation.clone(),
            )
            .with_task_recording(self.record_translation_tasks);
        let result_store =
            RpgMakerTranslationResultStorageService::new(self.sqlite.clone(), self.cpu.clone());
        let translation = RpgMakerTranslationService::new(
            asset_reader,
            planner,
            executor,
            result_store,
            self.log.clone(),
            self.cancellation.clone(),
        )
        .with_task_record_sink(self.task_records.clone());
        ensure_translation_execution_build_running(&self.cancellation)?;
        Ok(SelectedTranslationExecution::new(profile, translation))
    }
}

#[derive(Debug)]
struct TranslationExecutionBuildCancelled;

impl fmt::Display for TranslationExecutionBuildCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("翻译执行上下文构建已取消")
    }
}

impl Error for TranslationExecutionBuildCancelled {}

fn ensure_translation_execution_build_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ProductionTranslationExecutionBuildError> {
    if cancellation.is_requested() {
        Err(ProductionTranslationExecutionBuildError::cancelled())
    } else {
        Ok(())
    }
}

struct ProductionTranslationExecutionBuildError {
    class: TranslationExecutionBuildFailureClass,
    diagnostic: Box<SafeDiagnostic>,
    source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslationExecutionBuildFailureClass {
    ConfigurationOrInput,
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    fn cancelled() -> Self {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::operation("build_translation_execution"),
            DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );
        Self::new(TranslationExecutionBuildCancelled, diagnostic)
    }

    fn prompt_cpu(source: CpuTaskExecutionError<CpuExecutorUnavailable>) -> Self {
        Self::cpu_task("prepare_rpg_maker_prompt", source)
    }

    fn placeholder_cpu(source: CpuTaskExecutionError<CpuExecutorUnavailable>) -> Self {
        Self::cpu_task("compile_rpg_maker_builtin_placeholders", source)
    }

    fn cpu_task(
        operation: &'static str,
        source: CpuTaskExecutionError<CpuExecutorUnavailable>,
    ) -> Self {
        let diagnostic = source
            .safe_diagnostic_source(
                DiagnosticStage::CommandPreparation,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            )
            .with_recovery(RecoveryFact::component(operation));
        Self::new(source, diagnostic)
    }

    fn prompt_locale(source: PromptLocaleResolutionError) -> Self {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::PromptUnavailable,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::field("target_language"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::NotFound,
                format!(
                    "target_language={}; automatic_prompt_locale=unsupported",
                    source.target_language()
                ),
            ),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        );
        Self::new(source, diagnostic)
    }

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
            diagnostic: Box::new(diagnostic),
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
    project: &crate::project_name::ProjectName,
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
    project: &crate::project_name::ProjectName,
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

    pub(crate) async fn recv(&mut self) -> io::Result<()> {
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
            // 信号到达但业务已完整完成：结果已生效，按成功呈现全部输出。
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(output))) => Self {
                result: CommandRunResult::Succeeded(output),
                shutdown_error,
                pending_project_log,
            },
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled)) => Self {
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
        ProjectWorkspaceConvergenceError::ObservePreservedDirectory(source) => (
            Class::ProjectState,
            init_directory_diagnostic(
                source,
                DiagnosticCode::ProjectState,
                DiagnosticAction::CheckProjectState,
            ),
        ),
        ProjectWorkspaceConvergenceError::PreserveObservability { failure, .. } => (
            Class::ProjectState,
            init_preserve_observability_diagnostic(failure),
        ),
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

fn init_preserve_observability_diagnostic(
    source: &crate::rpg_maker::init::PreserveObservabilityFailure<
        SystemFileSystemError,
        Box<SystemFileSystemError>,
    >,
) -> SafeDiagnostic {
    use crate::rpg_maker::init::PreserveObservabilityFailure;
    use crate::storage::file_system::{ScopedDirectoryBindError, ScopedDirectoryEditError};

    let invalid_path = |path: &PathBuf, kind: DiagnosticFailureKind| {
        SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(kind),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        )
    };
    match source {
        PreserveObservabilityFailure::Bind(source) => match source {
            ScopedDirectoryBindError::WrongEditorInstance => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::Init,
                DiagnosticSubject::component("preserved observability candidate"),
                DiagnosticReason::failure(DiagnosticFailureKind::WrongPublisherInstance),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            ),
            ScopedDirectoryBindError::CandidateFinalized { root } => {
                invalid_path(root, DiagnosticFailureKind::StateMismatch)
            }
            ScopedDirectoryBindError::CandidateIdentityChanged { root } => {
                invalid_path(root, DiagnosticFailureKind::FileIdentityChanged)
            }
            ScopedDirectoryBindError::Failed { root, source } => source
                .safe_diagnostic(
                    DiagnosticStage::Init,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(root)),
        },
        PreserveObservabilityFailure::List { path, source } => init_directory_listing_diagnostic(
            source,
            DiagnosticCode::ProjectState,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        PreserveObservabilityFailure::Read { path, source } => match source {
            ReadFileError::NotFound { path } => invalid_path(path, DiagnosticFailureKind::NotFound),
            ReadFileError::NotFile { path } => {
                invalid_path(path, DiagnosticFailureKind::InvalidPath)
            }
            ReadFileError::Io { path, source } => source
                .safe_diagnostic(
                    DiagnosticStage::Init,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        }
        .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        PreserveObservabilityFailure::InvalidCandidatePath { path, .. } => {
            invalid_path(path, DiagnosticFailureKind::InvalidPath)
        }
        PreserveObservabilityFailure::Edit { path, source } => match source {
            ScopedDirectoryEditError::Failed { path, source } => source
                .safe_diagnostic(
                    DiagnosticStage::Init,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
            ScopedDirectoryEditError::WrongEditorInstance => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::Init,
                DiagnosticSubject::component("preserved observability candidate"),
                DiagnosticReason::failure(DiagnosticFailureKind::WrongPublisherInstance),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            ),
            ScopedDirectoryEditError::OutsideScope { path }
            | ScopedDirectoryEditError::ScopeRootMutation { path }
            | ScopedDirectoryEditError::NotFile { path }
            | ScopedDirectoryEditError::NotDirectory { path } => {
                invalid_path(path, DiagnosticFailureKind::InvalidPath)
            }
            ScopedDirectoryEditError::NotFound { path } => {
                invalid_path(path, DiagnosticFailureKind::NotFound)
            }
            ScopedDirectoryEditError::CandidateIdentityChanged { root } => {
                invalid_path(root, DiagnosticFailureKind::FileIdentityChanged)
            }
        }
        .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
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
    RE: RpgMakerProjectDocumentReadingDiagnostic,
    SE: SafeDiagnosticSource,
    crate::execution::cpu::CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        BuiltInExtractionError::safe_diagnostic(self)
    }
}

impl<RE, SE, CE> ExtractFailureReport for BuiltInExtractionError<RE, SE, CE>
where
    RE: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
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
    DE: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    CE: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    fn into_extract_failure_report(self) -> FailureReport {
        RulesExtractionError::into_failure_report(self)
    }
}

fn map_extract_error<OE, BE, RE, PE>(
    error: ExtractServiceError<OE, BE, RE, PE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    BE: ExtractFailureReport,
    RE: ExtractFailureReport,
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
    }
}

trait ProductionExternalModelFailure: Error + Send + Sync + 'static {
    fn into_external_model_failure(self) -> ProductionCommandError;
}

trait ProductionTranslationResultStorageFailure: Error + Send + Sync + Sized + 'static {
    fn into_result_storage_failure_report(self) -> FailureReport;
}

impl<S, C> ProductionTranslationResultStorageFailure for RpgMakerTranslationResultStorageError<S, C>
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
    for crate::rpg_maker::translate::pipeline::RpgMakerTranslationServiceError<
        R,
        P,
        RpgMakerTranslationTaskExecutionError<OpenAiChatCompletionError, E>,
        S,
    >
where
    R: Error + SafeDiagnosticSource + Send + Sync + 'static,
    P: Error + SafeDiagnosticSource + Send + Sync + 'static,
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
    S: ProductionTranslationResultStorageFailure,
{
    fn into_external_model_failure(self) -> ProductionCommandError {
        use crate::rpg_maker::translate::pipeline::RpgMakerTranslationServiceError as TranslationError;

        match self {
            TranslationError::ReadAssets(source) => {
                let diagnostic = source.safe_diagnostic_source(
                    DiagnosticStage::Translate,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                );
                map_project_diagnostic(source, diagnostic)
            }
            TranslationError::PlanTasks(source) => {
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
            TranslationError::ApplyPreparation(source) => {
                map_project_failure_report(source.into_result_storage_failure_report())
            }
            TranslationError::ExecuteTask { task_index, source } => match source {
                RpgMakerTranslationTaskExecutionError::FatalRequest { attempt, source } => {
                    let diagnostic = source
                        .safe_diagnostic(None, DiagnosticImpact::ProgressPreserved)
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task={task_index}; attempt={attempt}"
                        )));
                    ProductionCommandError::ExternalModel(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
                RpgMakerTranslationTaskExecutionError::ProcessResponse { attempt, source } => {
                    let diagnostic = source
                        .safe_diagnostic_source(
                            DiagnosticStage::ModelRequest,
                            DiagnosticImpact::ProgressPreserved,
                            DiagnosticAction::CheckModelService,
                        )
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task={task_index}; attempt={attempt}"
                        )));
                    let execution_error: RpgMakerTranslationTaskExecutionError<
                        OpenAiChatCompletionError,
                        E,
                    > = RpgMakerTranslationTaskExecutionError::ProcessResponse { attempt, source };
                    map_project_diagnostic(execution_error, diagnostic)
                }
                source @ RpgMakerTranslationTaskExecutionError::RetryWaitCancelled { attempt } => {
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
                RpgMakerTranslationTaskExecutionError::InternalInvariant { invariant } => {
                    let diagnostic = invariant
                        .safe_diagnostic(
                            DiagnosticStage::Translate,
                            DiagnosticImpact::ProgressPreserved,
                        )
                        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
                            "task_ordinal={task_index}"
                        )));
                    let source: RpgMakerTranslationTaskExecutionError<
                        OpenAiChatCompletionError,
                        E,
                    > = RpgMakerTranslationTaskExecutionError::InternalInvariant { invariant };
                    ProductionCommandError::Internal(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
            },
            TranslationError::CommitTask { task_index, source } => {
                let report = source
                    .into_result_storage_failure_report()
                    .with_primary_recovery(crate::diagnostic::RecoveryFact::component(format!(
                        "task={task_index}"
                    )));
                map_project_failure_report(report)
            }
            source @ TranslationError::InvalidTaskResultSequence {
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
            TranslationError::FinalizeResultStore(source) => {
                map_project_failure_report(source.into_result_storage_failure_report())
            }
            TranslationError::OperationAndFinalization {
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

fn map_translate_error<RE, TE, PE>(
    error: TranslateServiceError<RE, ProductionTranslationExecutionBuildError, TE, PE>,
) -> ProductionCommandError
where
    RE: Error + Send + Sync + 'static,
    TE: ProductionExternalModelFailure,
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
        TranslateServiceError::BuildExecution(source) => {
            ProductionCommandError::translation_execution_build(source)
        }
        TranslateServiceError::Translation { source } => source.into_external_model_failure(),
    }
}

fn map_write_back_error<OE, SE, PE, KE>(
    error: WriteBackServiceError<OE, SE, PE, KE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    PE: Error + WriteBackPublishingDiagnostic + Send + Sync + 'static,
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
        WriteBackServiceError::Prepare(source) => {
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
        // 信号到达但业务完整完成与正常完成同为成功终态。
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(_)))
            if shutdown.is_empty() =>
        {
            ProjectLogRunOutcome::Succeeded
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(_))) => {
            ProjectLogRunOutcome::Failed
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled))
            if shutdown.is_empty() =>
        {
            ProjectLogRunOutcome::Cancelled
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled)) => {
            ProjectLogRunOutcome::Failed
        }
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

    pub(crate) fn stderr_write(source: io::Error) -> Self {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::StateFinalizationFailed,
            DiagnosticStage::ProcessOutput,
            DiagnosticSubject::operation("write_stderr"),
            "write_stderr",
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

    fn configuration_load(source: ConfigurationLoadError) -> Self {
        let diagnostic = source.safe_diagnostic();
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

    fn lua_script_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = match &source {
            ReadFileError::NotFound { path } => SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            ReadFileError::NotFile { path } => SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            ReadFileError::Io { path, source } => source
                .safe_diagnostic(
                    DiagnosticStage::CommandPreparation,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(RecoveryFact::path(path)),
        };
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn project_lua_worker(source: tokio::task::JoinError) -> Self {
        Self::Internal(Box::new(Self::report(
            source,
            DiagnosticCode::LuaExecution,
            DiagnosticStage::Lua,
            DiagnosticSubject::component("Lua blocking worker"),
            DiagnosticReason::failure(DiagnosticFailureKind::WorkerPanicked),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        )))
    }

    fn project_lua_preflight(source: ProjectLuaFailure) -> Self {
        let source = ProjectLuaPreflightError(source);
        let detail = source.to_string();
        let (code, reason, action, class, subject) = match &source.0 {
            ProjectLuaFailure::Compile(_) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaCompilationFailed,
                    &detail,
                ),
                DiagnosticAction::FixInput,
                2_u8,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::Cancelled
            | ProjectLuaFailure::DatabasePrerequisite(
                ProjectLuaDatabasePrerequisiteError::Cancelled,
            ) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LockCancelled,
                    &detail,
                ),
                DiagnosticAction::Retry,
                2,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::Context(_) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaContextCreationFailed,
                    &detail,
                ),
                DiagnosticAction::ReportBug,
                1,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::Script(_) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaExecutionFailed,
                    &detail,
                ),
                DiagnosticAction::FixInput,
                2,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::DatabasePrerequisite(
                ProjectLuaDatabasePrerequisiteError::InvalidProjectState(state),
            ) => (
                DiagnosticCode::ProjectState,
                DiagnosticReason::failure_with_detail(DiagnosticFailureKind::StateMismatch, state),
                DiagnosticAction::CheckProjectState,
                0,
                DiagnosticSubject::operation("validate_project_lua_database"),
            ),
            ProjectLuaFailure::DatabasePrerequisite(
                ProjectLuaDatabasePrerequisiteError::Sqlite(error),
            )
            | ProjectLuaFailure::Database(error) => (
                DiagnosticCode::SqliteOperation,
                project_lua_sqlite_reason(error, DiagnosticFailureKind::LuaFinalizationFailed),
                DiagnosticAction::CheckProjectState,
                0,
                DiagnosticSubject::operation(error.operation()),
            ),
            ProjectLuaFailure::Host {
                kind: "worker_spawn",
                operation,
                ..
            } => (
                DiagnosticCode::InternalOperation,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::WorkerSpawnFailed,
                    &detail,
                ),
                DiagnosticAction::ReportBug,
                1,
                DiagnosticSubject::operation(operation),
            ),
            ProjectLuaFailure::Host { .. } => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaHostCallFailed,
                    &detail,
                ),
                DiagnosticAction::FixInput,
                2,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::Validation(_) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaFinalizationFailed,
                    &detail,
                ),
                DiagnosticAction::CheckProjectState,
                0,
                DiagnosticSubject::component("Lua program"),
            ),
            ProjectLuaFailure::Panicked => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::WorkerPanicked,
                    &detail,
                ),
                DiagnosticAction::ReportBug,
                1,
                DiagnosticSubject::component("Lua program"),
            ),
        };
        let diagnostic = SafeDiagnostic::new(
            code,
            DiagnosticStage::CommandPreparation,
            subject,
            reason,
            DiagnosticImpact::Unchanged,
            action,
        );
        let report = Self::report_diagnostic(source, diagnostic);
        match class {
            0 => Self::ProjectState(Box::new(report)),
            1 => Self::Internal(Box::new(report)),
            _ => Self::ConfigurationOrInput(Box::new(report)),
        }
    }

    fn project_lua_execution(source: ProjectLuaExecutionError) -> Self {
        let detail = source.to_string();
        let (code, reason, impact, action, class, subject) = match &source {
            ProjectLuaExecutionError::Open { path, .. } => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaDatabaseOpenFailed,
                    &detail,
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
                0_u8,
                DiagnosticSubject::path(path),
            ),
            ProjectLuaExecutionError::Run(
                ProjectLuaRunError::RollbackOutcomeUnknown { .. }
                | ProjectLuaRunError::CommitOutcomeUnknown(_),
            ) => (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::LuaFinalizationFailed,
                    &detail,
                ),
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
                1,
                DiagnosticSubject::operation("project_lua_transaction"),
            ),
            ProjectLuaExecutionError::Run(
                ProjectLuaRunError::NotStarted(failure) | ProjectLuaRunError::RolledBack(failure),
            ) => {
                let (code, reason, action, class, subject) = match failure {
                    ProjectLuaFailure::Compile(_) => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::LuaCompilationFailed,
                            &detail,
                        ),
                        DiagnosticAction::FixInput,
                        2,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                    ProjectLuaFailure::Script(_) => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::LuaExecutionFailed,
                            &detail,
                        ),
                        DiagnosticAction::FixInput,
                        2,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                    ProjectLuaFailure::DatabasePrerequisite(
                        ProjectLuaDatabasePrerequisiteError::InvalidProjectState(state),
                    ) => (
                        DiagnosticCode::ProjectState,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::StateMismatch,
                            state,
                        ),
                        DiagnosticAction::CheckProjectState,
                        0,
                        DiagnosticSubject::operation("validate_project_lua_database"),
                    ),
                    ProjectLuaFailure::DatabasePrerequisite(
                        ProjectLuaDatabasePrerequisiteError::Sqlite(error),
                    )
                    | ProjectLuaFailure::Database(error) => (
                        DiagnosticCode::SqliteOperation,
                        project_lua_sqlite_reason(error, DiagnosticFailureKind::LuaExecutionFailed),
                        DiagnosticAction::CheckProjectState,
                        0,
                        DiagnosticSubject::operation(error.operation()),
                    ),
                    ProjectLuaFailure::Host {
                        kind: "worker_spawn",
                        operation,
                        ..
                    } => (
                        DiagnosticCode::InternalOperation,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::WorkerSpawnFailed,
                            &detail,
                        ),
                        DiagnosticAction::ReportBug,
                        3,
                        DiagnosticSubject::operation(operation),
                    ),
                    ProjectLuaFailure::Host { .. } => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::LuaHostCallFailed,
                            &detail,
                        ),
                        DiagnosticAction::FixInput,
                        2,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                    ProjectLuaFailure::Validation(_) => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::LuaFinalizationFailed,
                            &detail,
                        ),
                        DiagnosticAction::FixInput,
                        2,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                    ProjectLuaFailure::Context(_) | ProjectLuaFailure::Panicked => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::WorkerPanicked,
                            &detail,
                        ),
                        DiagnosticAction::ReportBug,
                        3,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                    ProjectLuaFailure::Cancelled
                    | ProjectLuaFailure::DatabasePrerequisite(
                        ProjectLuaDatabasePrerequisiteError::Cancelled,
                    ) => (
                        DiagnosticCode::LuaExecution,
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::LockCancelled,
                            &detail,
                        ),
                        DiagnosticAction::Retry,
                        0,
                        DiagnosticSubject::operation("project_lua_transaction"),
                    ),
                };
                (
                    code,
                    reason,
                    DiagnosticImpact::Unchanged,
                    action,
                    class,
                    subject,
                )
            }
        };
        let diagnostic =
            SafeDiagnostic::new(code, DiagnosticStage::Lua, subject, reason, impact, action);
        let report = Self::report_diagnostic(source, diagnostic);
        match class {
            0 => Self::ProjectState(Box::new(report)),
            1 => Self::OutcomeUnknown(Box::new(report)),
            2 => Self::ConfigurationOrInput(Box::new(report)),
            _ => Self::Internal(Box::new(report)),
        }
    }

    fn invalid_run_plan(source: InvalidRunPlanValue) -> Self {
        let failure = match &source {
            InvalidRunPlanValue::EmptyPath { .. }
            | InvalidRunPlanValue::EmptyExtractOwners
            | InvalidRunPlanValue::EmptyRulesDefinition
            | InvalidRunPlanValue::EmptyProfileId => DiagnosticFailureKind::MissingRequiredValue,
            InvalidRunPlanValue::RelativePath { .. }
            | InvalidRunPlanValue::PathContainsNul { .. }
            | InvalidRunPlanValue::InvalidWindowsPathEncoding { .. } => {
                DiagnosticFailureKind::InvalidPath
            }
            InvalidRunPlanValue::InvalidRulesCanonicalJson { .. }
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

#[cfg(test)]
mod project_lua_diagnostic_tests {
    use super::*;

    fn primary(error: &ProductionCommandError) -> &SafeDiagnostic {
        error.failure_report().primary.public()
    }

    #[test]
    fn database_prerequisite_state_is_project_state_in_preflight_and_execution() {
        let failure = ProjectLuaFailure::DatabasePrerequisite(
            ProjectLuaDatabasePrerequisiteError::InvalidProjectState(
                "database_component=att_schema; violation=test".to_owned(),
            ),
        );
        let errors = [
            ProductionCommandError::project_lua_preflight(failure.clone()),
            ProductionCommandError::project_lua_execution(ProjectLuaExecutionError::Run(
                ProjectLuaRunError::RolledBack(failure),
            )),
        ];

        for error in &errors {
            assert!(matches!(error, ProductionCommandError::ProjectState(_)));
            let diagnostic = primary(error);
            assert_eq!(diagnostic.code, DiagnosticCode::ProjectState);
            assert_eq!(
                diagnostic.reason,
                DiagnosticReason::FailureWithDetail {
                    failure: DiagnosticFailureKind::StateMismatch,
                    detail: "database_component=att_schema; violation=test".to_owned(),
                }
            );
            assert_eq!(diagnostic.action, DiagnosticAction::CheckProjectState);
        }
    }

    #[test]
    fn sqlite_prerequisite_uses_structured_codes_without_driver_message() {
        let connection = Connection::open_in_memory().expect("应建立错误来源数据库");
        connection
            .execute_batch(
                "CREATE TABLE sensitive_driver_message (value TEXT UNIQUE);
                 INSERT INTO sensitive_driver_message VALUES ('secret-value');",
            )
            .expect("应建立唯一约束");
        let source = connection
            .execute(
                "INSERT INTO sensitive_driver_message VALUES ('secret-value')",
                [],
            )
            .expect_err("重复值应产生扩展 SQLite code");
        assert!(source.to_string().contains("sensitive_driver_message"));
        let sqlite = ProjectLuaSqliteError::new("read_current_att_schema", &source);
        let failure = ProjectLuaFailure::DatabasePrerequisite(
            ProjectLuaDatabasePrerequisiteError::Sqlite(sqlite),
        );
        let errors = [
            ProductionCommandError::project_lua_preflight(failure.clone()),
            ProductionCommandError::project_lua_execution(ProjectLuaExecutionError::Run(
                ProjectLuaRunError::RolledBack(failure),
            )),
        ];

        for error in &errors {
            assert!(matches!(error, ProductionCommandError::ProjectState(_)));
            let diagnostic = primary(error);
            assert_eq!(diagnostic.code, DiagnosticCode::SqliteOperation);
            assert_eq!(
                diagnostic.reason,
                DiagnosticReason::Sqlite {
                    primary_code: 19,
                    extended_code: 2067,
                }
            );
            assert_eq!(diagnostic.action, DiagnosticAction::CheckProjectState);
            let serialized = serde_json::to_string(diagnostic).expect("公开诊断应可序列化");
            assert!(!serialized.contains("sensitive_driver_message"));
            assert!(!serialized.contains("secret-value"));
        }
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

    #[cfg(test)]
    pub(super) fn push_for_test(
        &mut self,
        component: &'static str,
        source: impl Error + SafeDiagnosticSource + Send + Sync + 'static,
    ) {
        self.push(component, source);
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
        output: &RpgMakerCommandOutput,
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
                match &output.outcome {
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
                                .iter()
                                .map(|owner| match owner {
                                    InitStaleOwner::Builtin => "Builtin",
                                    InitStaleOwner::Rules => "Rules",
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
                render_saved_plan_source(localizer, *plan_source, stdout)
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
                if *plan_source == ProjectLogValueSource::ProjectState {
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
                if *has_saved_plan {
                    render_saved_plan_source(localizer, *plan_source, stdout)
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
                    localizer.format(UiMessage::ResultTranslateSummary {
                        total: u64::try_from(output.summary.total_tasks).unwrap_or(u64::MAX),
                        complete: u64::try_from(output.summary.complete_tasks).unwrap_or(u64::MAX),
                        partial: u64::try_from(output.summary.partial_tasks).unwrap_or(u64::MAX),
                        unavailable: u64::try_from(output.summary.unavailable_tasks)
                            .unwrap_or(u64::MAX),
                        written: u64::try_from(output.summary.written_locations)
                            .unwrap_or(u64::MAX),
                        remaining: u64::try_from(output.summary.remaining_locations)
                            .unwrap_or(u64::MAX),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateConvergence {
                        retained: u64::try_from(output.summary.retained).unwrap_or(u64::MAX),
                        invalidated: u64::try_from(output.summary.invalidated).unwrap_or(u64::MAX),
                        not_applicable: u64::try_from(output.summary.not_applicable)
                            .unwrap_or(u64::MAX),
                        reused: u64::try_from(output.summary.reused).unwrap_or(u64::MAX),
                    })
                )?;
                if output.summary.total_tasks == 0 {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeNoModelRequest)
                    )?;
                }
                if *profile_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeTranslateReuseProfile {
                            profile: &output.profile_id,
                        })
                    )?;
                }
                render_saved_plan_source(localizer, *profile_source, stdout)
            }
            RpgMakerCommandOutput::WriteBack { output } => {
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
                    localizer.format(UiMessage::ResultWriteBackSummary {
                        translated: u64::try_from(output.summary.translated_units)
                            .unwrap_or(u64::MAX),
                        original: u64::try_from(output.summary.original_units).unwrap_or(u64::MAX),
                        auto_wrapped: u64::try_from(output.summary.auto_wrapped_units)
                            .unwrap_or(u64::MAX),
                        breaks: u64::try_from(output.summary.inserted_line_breaks)
                            .unwrap_or(u64::MAX),
                        indents: u64::try_from(output.summary.inserted_fullwidth_indents)
                            .unwrap_or(u64::MAX),
                        manual: u64::try_from(output.summary.manual_layout_units)
                            .unwrap_or(u64::MAX),
                    })
                )?;
                if output.summary.manual_layout_units > 0 {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeManualLayout {
                            count: u64::try_from(output.summary.manual_layout_units)
                                .unwrap_or(u64::MAX),
                        })
                    )?;
                }
                Ok(())
            }
            RpgMakerCommandOutput::Lua { project } => writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultProjectLuaCompleted {
                    project: project.as_str(),
                })
            ),
        }
    }

    pub(crate) fn render_success_warnings(
        output: &RpgMakerCommandOutput,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        let RpgMakerCommandOutput::Extract { output, .. } = output else {
            return Ok(());
        };
        for warning in &output.rules_warnings {
            let source_file = sanitize_user_text(&warning.source_file);
            let command_code = warning.command_code.to_string();
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::WarningRulesCommandNonStringSkipped {
                    rule_number: u64::try_from(warning.rule_number).unwrap_or(u64::MAX),
                    source_file: &source_file,
                    command_code: &command_code,
                    parameter: u64::try_from(warning.parameter).unwrap_or(u64::MAX),
                    actual_type: warning.actual_type.as_str(),
                    skipped_count: warning.skipped_count,
                })
            )?;
        }
        Ok(())
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

fn plan_source_message(source: ProjectLogValueSource) -> UiMessage<'static> {
    project_log_value_source_label(match source {
        ProjectLogValueSource::Explicit => "explicit",
        ProjectLogValueSource::ProjectState => "project_state",
        ProjectLogValueSource::ProductDefault => "product_default",
    })
    .expect("每个运行方案来源代码都必须具有本地化日志标签")
}
