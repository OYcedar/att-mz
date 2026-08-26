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

use super::TranslationTerminalSummary;
use super::project_log::{
    ActiveProjectLog, CommandLogStart, PendingProjectLog, ProjectLogHandle, ProjectLogLuaPrintSink,
    start_command_log,
};
use super::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError,
    assemble_translation_system_prompt_with_cancellation,
    ensure_no_prompt_template_variables_with_cancellation, parse_prompt_resource_with_cancellation,
    read_unparsed_prompt_resource, render_system_prompt_template_with_cancellation,
    translation_prompt_resource_paths,
};
#[cfg(test)]
use super::translation_prompt::{
    ensure_no_prompt_template_variables, render_system_prompt_template,
};
use crate::application::config::{
    ConfigurationLoadError, ConfiguredExtractCommand, ConfiguredInitCommand,
    ConfiguredManualCommand, ConfiguredProjectLuaCommand, ConfiguredRpgMakerCommand,
    ConfiguredTranslateCommand, ConfiguredWriteBackCommand, TranslateConfiguration,
};
use crate::diagnostic::{
    BoxedError, Diagnostic, DiagnosticIssue, DiagnosticReport, DiagnosticStage, IoFailure,
    PromptProblem, RelatedFailureRelation, ReportedFailure, RpgMakerDiagnosticStage, RpgMakerIssue,
    RpgMakerProjectProblem, RuntimeBoundaryOperation, RuntimeComponent, RuntimeEngine,
    RuntimeIssue, RuntimeOperation, RuntimePanicBoundary, SafeIdentifier, SafePath,
    SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure, SqliteIssue,
    SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect, TranslationIssue,
    public_path, render_diagnostic_report, render_state_effect_impact,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage, project_log_value_source_label};
use crate::language::LanguageModuleCatalogError;
use crate::llm::ApiKeyRedactor;
use crate::manual::{
    ManualCommandError, ManualCommandSummary, execute_rpg_maker_manual_command,
    render_manual_command_error, render_manual_command_summary,
};
use crate::progress::{
    ProgressAmount, ProgressObserver, ProgressSnapshot, TerminalProgress, TerminalProgressFailures,
    TerminalProgressObserver,
};
use crate::project_lease::{
    AlreadyHeldProjectCommandLeaseProvider, ProjectCommandLease, ProjectCommandLeaseError,
    ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaFailure, ProjectLuaProgram, ProjectLuaProject,
    ProjectLuaRunError, ProjectLuaRunRequest, compile_project_lua_program_with_cancellation,
    rpg_maker_project_lua_adapter, run_project_lua,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
    external_invalid_utf8_diagnostic_report,
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
    InitStaleOwner, ProjectWorkspaceConvergenceError, ProjectWorkspaceConvergenceService,
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
    RpgMakerTranslationRunReport, RpgMakerTranslationService,
};
use crate::rpg_maker::translate::placeholder::{
    Pcre2PlaceholderConstructionError, Pcre2PlaceholderService,
};
use crate::rpg_maker::translate::planner::RpgMakerTranslationTaskPlanningService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt, RpgMakerSystemPromptError,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
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
#[cfg(test)]
use crate::rpg_maker::write_back::WriteBackPublishFailureState;
use crate::rpg_maker::write_back::asset_reader::{
    RpgMakerWriteBackAssetReadingError, RpgMakerWriteBackAssetReadingService,
    RpgMakerWriteBackLayoutRulesInput,
};
use crate::rpg_maker::write_back::planner::{
    RpgMakerWriteBackService, RpgMakerWriteBackServiceError, write_back_planning_compute_report,
};
use crate::rpg_maker::write_back::publisher::RpgMakerWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::{
    RpgMakerWriteBackDocumentRewritingError, RpgMakerWriteBackDocumentRewritingService,
};
use crate::rpg_maker::write_back::{
    WriteBackInput, WriteBackLog, WriteBackLogEvent, WriteBackLogPublicationOutcome,
    WriteBackOutput, WriteBackProgressPhase, WriteBackPublishingDiagnostic, WriteBackService,
    WriteBackServiceError,
};
use crate::runtime::cpu::{
    CpuExecutorConfig, CpuExecutorShutdownError, CpuExecutorStartError, CpuExecutorUnavailable,
    RayonCpuExecutor,
};
use crate::runtime::filesystem::{
    SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemConfig, SystemFileSystemError,
};
use crate::runtime::llm::{
    OpenAiCompatibleClient, OpenAiCompatibleError, OpenAiCompatibleExecutor,
    OpenAiExecutorBuildError,
};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticOccurrenceId, DiagnosticScope, ExtractOwnerSelection, PhaseStopOutcome,
    ProjectLogAmount, ProjectLogCommand, ProjectLogEngine, ProjectLogEvent, ProjectLogPhase,
    PublicationFinished, PublicationSummary, ResolvedRunPlan, RpgMakerPublicationSummary,
    RpgMakerTranslationSummary, RunPlanFinalization, RunPlanTransactionState,
    RunPlanValueSource as ProjectLogValueSource, TaskCounterInvariantError, TaskFinishedOutcome,
    TaskPosition, TranslationEngineSummary, TranslationFinished, TranslationTaskCounters,
};
use crate::runtime::sqlite::{
    RusqliteFinalTransactionExecutor, RusqliteStorage, RusqliteStorageConfiguration,
    SqliteRuntimeError,
};
#[cfg(test)]
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError, DirectoryRecoveryError,
    StagingCleanupFailure,
};
use crate::storage::file_system::{
    DirectoryRecoveryOutcome, ExistingDirectoryResolver, FileReader, ReadFileError,
    RecoverableDirectoryPublisher, ResolveDirectoryError,
};
use crate::translation::planning_resource::TranslationPlanningResourceReadingService;
use crate::translation::task_record::TaskRecordDiagnosticRecorder;
use crate::translation_protocol::TranslationResponseMode;

#[derive(Clone, Copy, Debug, Default)]
struct TokioAsyncDelay;

/// Translate 终端只解释本纵向切片拥有的阶段；任务计数来自已提交终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslateProgressPhase {
    Planning,
    ConfirmedTasks,
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
        run_plan_warnings: Vec<DiagnosticReport>,
        has_saved_plan: bool,
    },
    Translate {
        output: TranslateOutput,
        profile_source: ProjectLogValueSource,
    },
    WriteBack {
        output: WriteBackOutput,
    },
    Manual {
        summary: ManualCommandSummary,
    },
    Lua {
        project: crate::project_name::ProjectName,
    },
}

fn init_terminal_progress(locale: UiLocale) -> TerminalProgress<InitProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let checking = localizer.format(UiMessage::ProgressInitCheckProject);
    let scanning = localizer.format(UiMessage::ProgressInitScanSource);
    let preparing = localizer.format(UiMessage::ProgressInitBuildCandidate);
    let updating = localizer.format(UiMessage::ProgressInitConvergeDatabase);
    let publishing = localizer.format(UiMessage::ProgressInitPublish);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(
        move |phase| match phase {
            InitProgressPhase::CheckingProject => checking.clone(),
            InitProgressPhase::ScanningSource => scanning.clone(),
            InitProgressPhase::PreparingCandidate => preparing.clone(),
            InitProgressPhase::UpdatingDatabase => updating.clone(),
            InitProgressPhase::Publishing => publishing.clone(),
        },
        no_work,
    )
}

fn extract_terminal_progress(locale: UiLocale) -> TerminalProgress<ExtractProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let builtin = localizer.format(UiMessage::ProgressExtractOwner { owner: "Builtin" });
    let builtin_documents = localizer.format(UiMessage::ProgressExtractDocuments);
    let builtin_work_units = localizer.format(UiMessage::ProgressExtractBuiltin);
    let builtin_commit = localizer.format(UiMessage::ProgressExtractCommit);
    let rules = localizer.format(UiMessage::ProgressExtractOwner { owner: "Rules" });
    let rules_documents = localizer.format(UiMessage::ProgressExtractDocuments);
    let rules_matches = localizer.format(UiMessage::ProgressExtractRules);
    let rules_commit = localizer.format(UiMessage::ProgressExtractCommit);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(
        move |phase| match phase {
            ExtractProgressPhase::Builtin => builtin.clone(),
            ExtractProgressPhase::BuiltinDocuments => builtin_documents.clone(),
            ExtractProgressPhase::BuiltinWorkUnits => builtin_work_units.clone(),
            ExtractProgressPhase::BuiltinCommit => builtin_commit.clone(),
            ExtractProgressPhase::Rules => rules.clone(),
            ExtractProgressPhase::RulesDocuments => rules_documents.clone(),
            ExtractProgressPhase::RulesMatches => rules_matches.clone(),
            ExtractProgressPhase::RulesCommit => rules_commit.clone(),
        },
        no_work,
    )
}

fn translate_terminal_progress(locale: UiLocale) -> TerminalProgress<TranslateProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let planning = localizer.format(UiMessage::ProgressTranslatePlanning);
    let confirmed = localizer.format(UiMessage::ProgressTranslateConfirmed);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(
        move |phase| match phase {
            TranslateProgressPhase::Planning => planning.clone(),
            TranslateProgressPhase::ConfirmedTasks => confirmed.clone(),
        },
        no_work,
    )
}

fn project_lua_terminal_progress(locale: UiLocale) -> TerminalProgress<ProjectLuaProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let running = localizer.format(UiMessage::ProgressProjectLua);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(move |_| running.clone(), no_work)
}

fn write_back_terminal_progress(locale: UiLocale) -> TerminalProgress<WriteBackProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let reading = localizer.format(UiMessage::ProgressWriteBackReadAssets);
    let planning = localizer.format(UiMessage::ProgressWriteBackPlanning);
    let rewriting = localizer.format(UiMessage::ProgressWriteBackDocuments);
    let preparing = planning.clone();
    let validating = localizer.format(UiMessage::ProgressWriteBackValidateCandidate);
    let publishing = localizer.format(UiMessage::ProgressWriteBackPublish);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(
        move |phase| match phase {
            WriteBackProgressPhase::ReadingAssets => reading.clone(),
            WriteBackProgressPhase::PlanningTranslations => planning.clone(),
            WriteBackProgressPhase::RewritingDocuments => rewriting.clone(),
            WriteBackProgressPhase::PreparingCandidate => preparing.clone(),
            WriteBackProgressPhase::ValidatingCandidate => validating.clone(),
            WriteBackProgressPhase::Publishing => publishing.clone(),
        },
        no_work,
    )
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

fn defer_terminal_progress_status(result: Result<(), TerminalProgressFailures>) {
    if let Err(failures) = result {
        // `TerminalProgress` 同时把这些事实保存在共享健康状态中；这里不能改变业务
        // future 的返回类型，最终 `finish` 会把同一批失败完整交给 shutdown 结果。
        debug_assert!(!failures.failures().is_empty());
    }
}

fn finish_terminal_progress<P>(
    progress: TerminalProgress<P>,
    mut shutdown: ShutdownFailures,
) -> ShutdownFailures {
    if let Err(failures) = progress.finish() {
        let report = failures.diagnostic_report();
        shutdown.push("terminal progress", failures, report);
    }
    shutdown
}

#[derive(Clone, Default)]
pub(crate) struct CommandPanicBoundary {
    state: Arc<Mutex<Option<CommandPanicContext>>>,
}

#[derive(Clone)]
struct CommandPanicContext {
    report: DiagnosticReport,
    command: Option<CommandPanicFacts>,
    panic_log_path: Option<PathBuf>,
    selected_api_key_redactor: Option<Arc<ApiKeyRedactor>>,
}

#[derive(Clone)]
struct CommandPanicFacts {
    engine: RuntimeEngine,
    command: crate::diagnostic::RuntimeCommand,
    project_workspace: PathBuf,
}

impl CommandPanicBoundary {
    pub(crate) fn from_report(report: DiagnosticReport) -> Self {
        Self {
            state: Arc::new(Mutex::new(Some(CommandPanicContext {
                report,
                command: None,
                panic_log_path: None,
                selected_api_key_redactor: None,
            }))),
        }
    }

    fn prepare(
        &self,
        engine: RuntimeEngine,
        command: crate::diagnostic::RuntimeCommand,
        project_workspace: &Path,
    ) {
        let facts = CommandPanicFacts {
            engine,
            command,
            project_workspace: project_workspace.to_path_buf(),
        };
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CommandPanicContext {
            report: command_panic_report(&facts, None),
            command: Some(facts),
            panic_log_path: None,
            selected_api_key_redactor: None,
        });
    }

    fn observe_selected_api_key_redactor(&self, redactor: Arc<ApiKeyRedactor>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(context) = state.as_mut() else {
            return;
        };
        if let Some(current) = &context.selected_api_key_redactor {
            assert!(
                Arc::ptr_eq(current, &redactor),
                "一次 Translate 运行不能改选另一个 API key 替换器"
            );
        } else {
            context.selected_api_key_redactor = Some(redactor);
        }
    }

    fn selected_api_key_redactor(&self) -> Option<Arc<ApiKeyRedactor>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|context| context.selected_api_key_redactor.clone())
    }

    fn observe_project_log(&self, project_log: &ActiveProjectLog) {
        let Some(path) = project_log.established_log_path().map(Path::to_path_buf) else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(context) = state.as_mut() else {
            return;
        };
        let Some(facts) = context.command.as_ref() else {
            return;
        };
        context.report = command_panic_report(facts, Some(&path));
        context.panic_log_path = Some(path);
    }

    fn panic_log_path(&self) -> Option<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|context| context.panic_log_path.clone())
    }

    pub(crate) fn panic_error(&self) -> ProductionCommandError {
        let context = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| CommandPanicContext {
                report: DiagnosticReport::new(
                    StateEffect::OutcomeUnknown,
                    Diagnostic::runtime(RuntimeIssue::ProcessPanicked {
                        boundary: RuntimePanicBoundary::AfterCliParsing,
                    }),
                ),
                command: None,
                panic_log_path: None,
                selected_api_key_redactor: None,
            });
        ProductionCommandError::Internal(Box::new(ReportedFailure::new(
            context.report,
            ApplicationScopePanicked,
        )))
    }
}

fn command_panic_report(facts: &CommandPanicFacts, log_path: Option<&Path>) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: facts.engine,
            command: facts.command,
            project_workspace: SafePath::new(&facts.project_workspace),
            log_path: log_path.map(SafePath::new),
        }),
    )
}

#[derive(Debug)]
struct ApplicationScopePanicked;

impl fmt::Display for ApplicationScopePanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("application scope panicked")
    }
}

impl Error for ApplicationScopePanicked {}

trait CommandRootShutdown: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    const COMPONENT: &'static str;

    fn shutdown_root(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn shutdown_diagnostic_report(source: &Self::Error) -> DiagnosticReport;
}

impl CommandRootShutdown for RayonCpuExecutor {
    type Error = CpuExecutorShutdownError;

    const COMPONENT: &'static str = "CPU";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown()
    }

    fn shutdown_diagnostic_report(source: &Self::Error) -> DiagnosticReport {
        DiagnosticReport::new(StateEffect::AppliedFinalizationFailed, source.diagnostic())
    }
}

impl CommandRootShutdown for SystemFileSystem {
    type Error = SystemFileSystemError;

    const COMPONENT: &'static str = "FileSystem";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown().await
    }

    fn shutdown_diagnostic_report(source: &Self::Error) -> DiagnosticReport {
        source.shutdown_diagnostic_report()
    }
}

impl CommandRootShutdown for RusqliteStorage {
    type Error = SqliteRuntimeError;

    const COMPONENT: &'static str = "SQLite";

    async fn shutdown_root(&self) -> Result<(), Self::Error> {
        self.shutdown().await
    }

    fn shutdown_diagnostic_report(source: &Self::Error) -> DiagnosticReport {
        source.shutdown_diagnostic_report()
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
        let report = R::shutdown_diagnostic_report(&source);
        failures.push(R::COMPONENT, source, report);
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

        fn shutdown_diagnostic_report(_source: &Self::Error) -> DiagnosticReport {
            DiagnosticReport::new(
                StateEffect::AppliedFinalizationFailed,
                Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                    component: RuntimeComponent::Process,
                    operation: RuntimeOperation::Shutdown,
                }),
            )
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
    panic_boundary: CommandPanicBoundary,
}

impl ProductionRpgMakerCommandRunner {
    pub(crate) fn new(layout: RpgMakerLayout, locale: UiLocale) -> Self {
        Self {
            layout,
            locale,
            panic_boundary: CommandPanicBoundary::default(),
        }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredRpgMakerCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let (engine, command_name, project_workspace) =
            command_panic_context(self.layout, &command);
        self.panic_boundary
            .prepare(engine, command_name, &project_workspace);
        if let ConfiguredRpgMakerCommand::Translate(command) = &command
            && command.resolved_profile_id().is_some()
        {
            self.panic_boundary.observe_selected_api_key_redactor(
                command.translation().client().api_key_redactor(),
            );
        }
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
                ConfiguredRpgMakerCommand::Manual(command) => {
                    self.run_manual(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Ownership(command)
                | ConfiguredRpgMakerCommand::Translation(command) => {
                    self.run_manual(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Lua(command) => {
                    self.run_atomic_project_lua(command, termination_signals)
                        .await
                }
            }
        })
        .await
    }

    async fn run_manual(
        self,
        command: ConfiguredManualCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let cancellation = CooperativeCancellation::default();
        let roots = match ProductionCommandRootGuard::start_init(
            command.common().filesystem().clone(),
            command.common().sqlite().clone(),
            performance,
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let project = command.project_name().clone();
        let database_path = ProjectWorkspaceLayout::for_project(
            command.common().projects_root(),
            self.layout,
            &project,
        )
        .database_path()
        .to_path_buf();
        let operation = command.operation();
        let file = command.file().to_path_buf();
        let export_selection = command.export_selection().cloned();
        let language_modules = command.language_modules().cloned();
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            self.layout.engine().storage_name(),
            file_system.clone(),
        );
        let engine = self.layout.engine();
        let operation_project = project.clone();
        let operation_cancellation = cancellation.clone();
        let execution = drive_command(
            async move {
                let _lease = lease_provider
                    .acquire(&operation_project)
                    .await
                    .map_err(ProductionCommandError::project_lease)?;
                if operation_cancellation.is_requested() {
                    return Ok(OperationCompletion::Cancelled);
                }
                let blocking_cancellation = operation_cancellation.clone();
                let summary = tokio::task::spawn_blocking(move || {
                    execute_rpg_maker_manual_command(
                        &database_path,
                        engine,
                        operation,
                        &file,
                        export_selection.as_ref(),
                        language_modules.as_ref(),
                        &blocking_cancellation,
                    )
                })
                .await
                .map_err(ProductionCommandError::manual_worker)?;
                let summary = match summary {
                    Ok(summary) => summary,
                    Err(source) if source.is_cancelled() => {
                        return Ok(OperationCompletion::Cancelled);
                    }
                    Err(source) => return Err(ProductionCommandError::manual(source)),
                };
                Ok(OperationCompletion::Completed(
                    RpgMakerCommandOutput::Manual { summary },
                ))
            },
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {},
        )
        .await;
        let shutdown = roots.shutdown().await;
        ProductionCommandRunReport::from_completion_with_project_log(execution, shutdown, None)
    }

    async fn run_atomic_project_lua(
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
        let language_modules = command.language_modules().clone();
        let database_path =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name)
                .database_path()
                .to_path_buf();
        let script_path = command.script().script_path().to_path_buf();
        let script_read = drive_command(
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
        let preparation = drive_command(
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
        let execution = drive_command(
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

    async fn run_init(
        self,
        command: ConfiguredInitCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = init_terminal_progress(self.locale);
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
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let recovery_target = project_workspace.workspace_root().to_path_buf();
        let engine_workspace_root = recovery_target
            .parent()
            .expect("固定项目工作区必有引擎父目录")
            .to_path_buf();
        let recovery = drive_command(
            async {
                match file_system
                    .resolve_existing_directory(engine_workspace_root)
                    .await
                {
                    Ok(_) => directory_publisher
                        .recover(recovery_target)
                        .await
                        .map_err(ProjectWorkspaceConvergenceError::Recover),
                    Err(ResolveDirectoryError::NotFound { .. }) => {
                        Ok(DirectoryRecoveryOutcome::Unchanged)
                    }
                    Err(source) => {
                        Err(ProjectWorkspaceConvergenceError::ObserveEngineWorkspaceRoot(source))
                    }
                }
            },
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
        .await
        .map(
            |result: Result<DirectoryRecoveryOutcome, ProductionWorkspaceConvergenceError>| {
                result.map_err(|source| map_init_error(InitServiceError::Workspace(source)))
            },
        );
        match recovery {
            DrivenCommand::Finished(Ok(_)) => {}
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                drop(project_lease_guard);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(DirectoryRecoveryOutcome::Recovered) => {
                        SignalOutcomeSource::CompletedStateApplied
                    }
                    Ok(DirectoryRecoveryOutcome::Unchanged) => SignalOutcomeSource::Cancelled,
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        }
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
                defer_terminal_progress_status(progress.safe_stopping(safe_stopping));
            },
        )
        .await
        .map(|result| result.map_err(map_init_error));
        finish_progress_business_state(&progress_observer, &execution);
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
            let shutdown = finish_terminal_progress(progress, shutdown);
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
            engine: project_log_engine(self.layout),
            project: command.arguments.project.name.as_str(),
            command: ProjectLogCommand::Init,
            performance: Arc::clone(&performance),
            selected_api_key_redactor: None,
        });
        self.panic_boundary.observe_project_log(&project_log);
        project_log.handle().emit(ProjectLogEvent::RunPlanResolved {
            plan: ResolvedRunPlan::init(plan_source, &resolved_game_root),
        });
        let replacement = InitRunPlan::new(resolved_game_root)
            .map(ProjectRunPlanReplacement::Init)
            .map_err(ProductionCommandError::invalid_run_plan);
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            defer_terminal_progress_status(progress.finalizing(progress_finalizing(self.locale)));
        }
        execution = match replacement {
            Ok(replacement) => {
                if business_completed(&execution) && shutdown.is_empty() {
                    defer_terminal_progress_status(
                        progress.finalizing(progress_saving_plan(self.locale)),
                    );
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
                        defer_terminal_progress_status(
                            progress.safe_stopping(progress_safe_stopping(self.locale)),
                        );
                        let (confirmed, total) = progress_observer.confirmed_amount();
                        project_log
                            .handle()
                            .emit(ProjectLogEvent::CancellationRequested { confirmed, total });
                    },
                )
                .await
            }
            Err(error) => replace_success_with_plan_error(execution, Err(error)),
        };
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
        let progress = extract_terminal_progress(self.locale);
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
                        let diagnostic = source.diagnostic_report();
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
                        let diagnostic = error.diagnostic_report(&database_path);
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

    async fn run_translate(
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
        let opener = PreopenedProject::new(opened_project);
        let business_log =
            ProductionBusinessLog::for_translation(&project_log, progress_observer.clone());
        let (task_records, record_translation_tasks) =
            if command.record_translation_tasks() {
                if let Some(run_id) = project_log.run_id() {
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
            retry_rejected: command.retry_rejected(),
        };
        progress_observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::Planning,
        ));
        let safe_stopping = progress_safe_stopping(self.locale);
        let translation_execution = async {
            drive_command(
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

    async fn run_write_back(
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
        let opener = PreopenedProject::new(opened_project);
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
                            ProductionCommandError::report_diagnostic(source, diagnostic),
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
        let publisher = RpgMakerWriteBackPublishingService::new(directory_publisher.clone());
        let business_log = ProductionBusinessLog::from_active(&project_log);
        let service = WriteBackService::new(
            opener,
            write_back,
            publisher,
            business_log.clone(),
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
    fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::Read { source, .. } => source.command_preparation_diagnostic_report(),
            Self::Invalid { path, source } => source.diagnostic_report(path),
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
    fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::Read { source, .. } => source.command_preparation_diagnostic_report(),
            Self::InvalidUtf8 { path, source } => {
                external_invalid_utf8_diagnostic_report(path, source)
            }
            Self::InvalidDefinition { path, source } => source.external_diagnostic_report(path),
        }
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
    let mut report = match result {
        Ok(report) => report,
        Err(payload) => {
            // payload 可能含 Prompt、模型正文、Lua、SQL 或用户文本；只丢弃，绝不读取。
            drop(payload);
            let panic_log_path = panic_boundary.panic_log_path();
            ProductionCommandRunReport::panicked(panic_boundary.panic_error(), panic_log_path)
        }
    };
    report.selected_api_key_redactor = panic_boundary.selected_api_key_redactor().or_else(|| {
        report
            .pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::selected_api_key_redactor)
    });
    report
}

async fn catch_translate_execution_panic<T>(
    panic_boundary: CommandPanicBoundary,
    future: impl Future<Output = DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>>,
) -> DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>> {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(execution) => execution,
        Err(payload) => {
            // 与进程外层边界相同，panic payload 可能包含正文或外部响应，不能读取。
            drop(payload);
            DrivenCommand::Finished(Err(panic_boundary.panic_error()))
        }
    }
}

#[cfg(test)]
mod translate_panic_boundary_tests {
    use super::*;

    #[tokio::test]
    async fn inner_translate_panic_becomes_a_driven_command_error() {
        let execution: DrivenCommand<Result<OperationCompletion<()>, ProductionCommandError>> =
            catch_translate_execution_panic(CommandPanicBoundary::default(), async {
                panic!("测试 Translate 内层 panic")
            })
            .await;

        assert!(matches!(
            execution,
            DrivenCommand::Finished(Err(ProductionCommandError::Internal(_)))
        ));
    }
}

#[derive(Clone)]
struct ProductionProgressObserver<P> {
    terminal: TerminalProgressObserver<P>,
    project_log: Option<ProgressProjectLog>,
    phase_code: fn(P) -> Option<ProjectLogPhase>,
    state: Arc<Mutex<ProgressLogState<P>>>,
}

#[derive(Clone)]
struct ProgressProjectLog {
    handle: ProjectLogHandle,
}

#[derive(Clone, Copy)]
enum PhaseEvent {
    Started,
    Completed,
    Stopped(PhaseStopOutcome),
}

struct ProgressLogState<P> {
    // 同一命令可在 owner 阶段内发布细分阶段，随后再回到 owner；日志必须保留每个
    // 阶段的独立终态，不能把最近快照误当成唯一活动阶段。
    latest_amount: ProgressAmount,
    phases: Vec<TrackedProgressPhase<P>>,
}

struct TrackedProgressPhase<P> {
    phase: P,
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
        phase_code: fn(P) -> Option<ProjectLogPhase>,
    ) -> Self {
        Self {
            terminal,
            project_log: Some(ProgressProjectLog {
                handle: project_log.handle().clone(),
            }),
            phase_code,
            state: Arc::new(Mutex::new(ProgressLogState {
                latest_amount: ProgressAmount::Indeterminate,
                phases: Vec::new(),
            })),
        }
    }

    fn without_project_log(
        terminal: TerminalProgressObserver<P>,
        phase_code: fn(P) -> Option<ProjectLogPhase>,
    ) -> Self {
        Self {
            terminal,
            project_log: None,
            phase_code,
            state: Arc::new(Mutex::new(ProgressLogState {
                latest_amount: ProgressAmount::Indeterminate,
                phases: Vec::new(),
            })),
        }
    }

    fn complete_phase(&self, target: P) {
        let completed = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .phases
                .iter_mut()
                .find(|phase| phase.phase == target && !phase.finished)
                .map(|phase| {
                    phase.finished = true;
                    (phase.phase, phase.amount)
                })
        };
        if let Some((phase, amount)) = completed {
            self.emit_log_event(PhaseEvent::Completed, phase, amount);
        }
    }

    fn stop_active(&self, outcome: PhaseStopOutcome) {
        self.finish_active(PhaseEvent::Stopped(outcome));
    }

    fn finish_active(&self, event: PhaseEvent) {
        let phases = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .phases
                .iter_mut()
                .filter(|phase| !phase.finished)
                .map(|phase| {
                    phase.finished = true;
                    (phase.phase, phase.amount)
                })
                .collect::<Vec<_>>()
        };
        for (phase, amount) in phases {
            self.emit_log_event(event, phase, amount);
        }
    }

    fn confirmed_amount(&self) -> (u64, Option<u64>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.latest_amount {
            ProgressAmount::Indeterminate => (0, None),
            ProgressAmount::Determinate { completed, total } => (completed, Some(total)),
        }
    }

    fn emit_log_event(&self, event: PhaseEvent, phase: P, amount: ProgressAmount) {
        let Some(project_log) = &self.project_log else {
            return;
        };
        let Some(phase_code) = (self.phase_code)(phase) else {
            return;
        };
        let amount = match amount {
            ProgressAmount::Indeterminate => ProjectLogAmount::Indeterminate,
            ProgressAmount::Determinate { completed, total } => {
                ProjectLogAmount::Determinate { completed, total }
            }
        };
        let event = match event {
            PhaseEvent::Started => ProjectLogEvent::phase_started(phase_code, amount),
            PhaseEvent::Completed => ProjectLogEvent::phase_completed(phase_code, amount),
            PhaseEvent::Stopped(outcome) => ProjectLogEvent::phase_stopped(phase_code, outcome),
        };
        project_log.handle.emit(event);
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
            state.latest_amount = snapshot.amount;
            let (phase, started) = match state
                .phases
                .iter()
                .position(|phase| phase.phase == snapshot.phase)
            {
                Some(index) => (&mut state.phases[index], false),
                None => {
                    state.phases.push(TrackedProgressPhase {
                        phase: snapshot.phase,
                        amount: snapshot.amount,
                        finished: false,
                    });
                    (
                        state.phases.last_mut().expect("刚插入的进度阶段必须可读取"),
                        true,
                    )
                }
            };
            if !phase.finished {
                phase.amount = snapshot.amount;
                if started {
                    events.push((PhaseEvent::Started, snapshot.phase, snapshot.amount));
                }
                if matches!(
                    snapshot.amount,
                    ProgressAmount::Determinate { completed, total } if completed == total
                ) {
                    phase.finished = true;
                    events.push((PhaseEvent::Completed, snapshot.phase, snapshot.amount));
                }
            }
        }
        for (code, phase, amount) in events {
            self.emit_log_event(code, phase, amount);
        }
    }
}

const fn init_phase_code(phase: InitProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        InitProgressPhase::CheckingProject => ProjectLogPhase::CheckProject,
        InitProgressPhase::ScanningSource => ProjectLogPhase::ScanSource,
        InitProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        InitProgressPhase::UpdatingDatabase => ProjectLogPhase::UpdateDatabase,
        InitProgressPhase::Publishing => ProjectLogPhase::Publish,
    })
}

const fn extract_phase_code(phase: ExtractProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        ExtractProgressPhase::Builtin => ProjectLogPhase::Builtin,
        ExtractProgressPhase::BuiltinDocuments => ProjectLogPhase::BuiltinDocuments,
        ExtractProgressPhase::BuiltinWorkUnits => ProjectLogPhase::BuiltinWorkUnits,
        ExtractProgressPhase::BuiltinCommit => ProjectLogPhase::BuiltinCommit,
        ExtractProgressPhase::Rules => ProjectLogPhase::Rules,
        ExtractProgressPhase::RulesDocuments => ProjectLogPhase::RulesDocuments,
        ExtractProgressPhase::RulesMatches => ProjectLogPhase::RulesMatches,
        ExtractProgressPhase::RulesCommit => ProjectLogPhase::RulesCommit,
    })
}

const fn translate_phase_code(phase: TranslateProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        TranslateProgressPhase::Planning => ProjectLogPhase::Planning,
        TranslateProgressPhase::ConfirmedTasks => ProjectLogPhase::ConfirmedTasks,
    })
}

const fn project_lua_phase_code(_: ProjectLuaProgressPhase) -> Option<ProjectLogPhase> {
    Some(ProjectLogPhase::Lua)
}

const fn write_back_phase_code(phase: WriteBackProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        WriteBackProgressPhase::ReadingAssets => ProjectLogPhase::ReadAssets,
        WriteBackProgressPhase::PlanningTranslations => ProjectLogPhase::PlanRpgMakerWriteBack,
        WriteBackProgressPhase::RewritingDocuments => ProjectLogPhase::RewriteDocuments,
        WriteBackProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        WriteBackProgressPhase::ValidatingCandidate => ProjectLogPhase::ValidateCandidate,
        WriteBackProgressPhase::Publishing => ProjectLogPhase::Publish,
    })
}

const fn project_log_engine(layout: RpgMakerLayout) -> ProjectLogEngine {
    match layout.engine() {
        crate::rpg_maker::RpgMakerEngine::Mv => ProjectLogEngine::RpgMakerMv,
        crate::rpg_maker::RpgMakerEngine::Mz => ProjectLogEngine::RpgMakerMz,
    }
}

fn command_panic_context(
    layout: RpgMakerLayout,
    command: &ConfiguredRpgMakerCommand,
) -> (RuntimeEngine, crate::diagnostic::RuntimeCommand, PathBuf) {
    let (command_name, common, project_name) = match command {
        ConfiguredRpgMakerCommand::Init(command) => (
            crate::diagnostic::RuntimeCommand::Init,
            command.common(),
            command.arguments.project.name.as_str(),
        ),
        ConfiguredRpgMakerCommand::Extract(command) => (
            crate::diagnostic::RuntimeCommand::Extract,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::Translate(command) => (
            crate::diagnostic::RuntimeCommand::Translate,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::WriteBack(command) => (
            crate::diagnostic::RuntimeCommand::WriteBack,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::Manual(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::Ownership(command)
        | ConfiguredRpgMakerCommand::Translation(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common(),
            command.project_name().as_str(),
        ),
        ConfiguredRpgMakerCommand::Lua(command) => (
            crate::diagnostic::RuntimeCommand::Lua,
            command.common(),
            command.project_name().as_str(),
        ),
    };
    (
        match layout.engine() {
            crate::rpg_maker::RpgMakerEngine::Mv => RuntimeEngine::RpgMakerMv,
            crate::rpg_maker::RpgMakerEngine::Mz => RuntimeEngine::RpgMakerMz,
        },
        command_name,
        common
            .projects_root()
            .join(layout.engine().storage_name())
            .join(project_name),
    )
}

fn report_with_shutdown(
    mut report: DiagnosticReport,
    shutdown: &ShutdownFailures,
) -> DiagnosticReport {
    for related in shutdown.diagnostic_reports() {
        report = report.with_related(RelatedFailureRelation::Shutdown, related.clone());
    }
    report
}

fn shutdown_report(shutdown: &ShutdownFailures) -> Option<DiagnosticReport> {
    let mut reports = shutdown.diagnostic_reports();
    let mut primary = reports.next()?.clone();
    for related in reports {
        primary = primary.with_related(RelatedFailureRelation::Shutdown, related.clone());
    }
    Some(primary)
}

fn signal_report(source: &io::Error, effect: StateEffect) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::runtime(RuntimeIssue::Io {
            component: RuntimeComponent::Process,
            operation: RuntimeOperation::ReceiveTerminationSignal,
            failure: IoFailure::from_error(source),
        }),
    )
}

fn pending_project_log_for_execution<T>(
    project_log: ActiveProjectLog,
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
) -> PendingProjectLog {
    let report = execution_failure_report(execution, shutdown).or_else(|| match execution {
        DrivenCommand::Finished(Ok(_)) | DrivenCommand::Interrupted(Ok(_)) => {
            shutdown_report(shutdown)
        }
        DrivenCommand::Finished(Err(_))
        | DrivenCommand::Interrupted(Err(_))
        | DrivenCommand::SignalFailed { .. } => None,
    });
    if let Some(report) = report {
        project_log.pending_failure(report)
    } else if matches!(
        execution,
        DrivenCommand::Interrupted(_) | DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
    ) {
        project_log.pending_cancelled()
    } else {
        project_log.pending_succeeded()
    }
}

fn execution_failure_report<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
) -> Option<DiagnosticReport> {
    match execution {
        DrivenCommand::Interrupted(Err(error)) if error.was_cancelled_wait() => None,
        DrivenCommand::Finished(Err(error)) | DrivenCommand::Interrupted(Err(error)) => Some(
            report_with_shutdown(error.failure_report().report().clone(), shutdown),
        ),
        DrivenCommand::SignalFailed { source, result } => {
            let effect = if matches!(result, Ok(OperationCompletion::Completed(_))) {
                StateEffect::AppliedFinalizationFailed
            } else {
                StateEffect::Unchanged
            };
            let signal = signal_report(source, effect);
            Some(match result {
                Err(error) => report_with_shutdown(
                    error
                        .failure_report()
                        .report()
                        .clone()
                        .with_related(RelatedFailureRelation::Shutdown, signal),
                    shutdown,
                ),
                Ok(_) => report_with_shutdown(signal, shutdown),
            })
        }
        DrivenCommand::Finished(Ok(_)) | DrivenCommand::Interrupted(Ok(_)) => None,
    }
}

fn record_failed_phase<P, T>(
    observer: &ProductionProgressObserver<P>,
    project_log: &ActiveProjectLog,
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
    scope: DiagnosticScope,
) -> Option<(StateEffect, DiagnosticOccurrenceId)>
where
    P: Copy + Eq,
{
    let report = execution_failure_report(execution, shutdown)?;
    let effect = report.effect();
    let diagnostic = project_log.handle().record_diagnostic(scope, report)?;
    observer.stop_active(PhaseStopOutcome::Failed { diagnostic });
    Some((effect, diagnostic))
}

fn pending_project_log_with_occurrence<T>(
    project_log: ActiveProjectLog,
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
    terminal_diagnostic: Option<(StateEffect, DiagnosticOccurrenceId)>,
) -> PendingProjectLog {
    if let Some((effect, diagnostic)) = terminal_diagnostic {
        project_log.pending_failure_with_occurrence(effect, diagnostic)
    } else {
        pending_project_log_for_execution(project_log, execution, shutdown)
    }
}

fn pending_project_log_for_translation_execution(
    project_log: ActiveProjectLog,
    execution: &DrivenCommand<Result<OperationCompletion<TranslateOutput>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
    terminal_diagnostic: Option<(StateEffect, DiagnosticOccurrenceId, &'static str)>,
) -> PendingProjectLog {
    if shutdown.is_empty()
        && matches!(
            execution,
            DrivenCommand::Finished(Err(_)) | DrivenCommand::Interrupted(Err(_))
        )
        && let Some((effect, diagnostic, code)) = terminal_diagnostic
        && execution_failure_report(execution, shutdown).is_some_and(|report| {
            report.effect() == effect
                && report.primary().code() == code
                && report.related().is_empty()
        })
    {
        return project_log.pending_failure_with_occurrence(effect, diagnostic);
    }
    pending_project_log_for_execution(project_log, execution, shutdown)
}

async fn observed_construction_failure(
    project_log: ActiveProjectLog,
    error: ProductionCommandError,
    shutdown: ShutdownFailures,
) -> ProductionCommandRunReport {
    let report = report_with_shutdown(error.failure_report().report().clone(), &shutdown);
    let pending_project_log = project_log.pending_failure(report);
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

fn finish_progress_business_state<P, T>(
    observer: &ProductionProgressObserver<P>,
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
) where
    P: Copy + Eq,
{
    if matches!(
        execution,
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::SignalFailed {
                result: Ok(OperationCompletion::Cancelled),
                ..
            }
    ) || matches!(
        execution,
        DrivenCommand::Interrupted(Err(error)) if error.was_cancelled_wait()
    ) {
        observer.stop_active(PhaseStopOutcome::Cancelled);
    }
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

#[cfg(test)]
mod progress_lifecycle_tests {
    use super::*;
    use crate::application::arguments::{InitArguments, ProjectArguments};

    fn active_project_log(projects_root: &Path) -> ActiveProjectLog {
        let command = ConfiguredInitCommand::for_test(
            InitArguments {
                project: ProjectArguments {
                    name: "phase-contract".parse().expect("测试项目名应有效"),
                },
                path: None,
                source_language: None,
                target_language: None,
            },
            projects_root,
            "mv",
        );
        start_command_log(CommandLogStart {
            common: command.common(),
            locale: UiLocale::SimplifiedChinese,
            engine: ProjectLogEngine::RpgMakerMv,
            project: "phase-contract",
            command: ProjectLogCommand::Init,
            performance: Arc::new(RunPerformanceCounters::default()),
            selected_api_key_redactor: None,
        })
    }

    #[tokio::test]
    async fn command_panic_after_log_start_preserves_established_log_path() {
        let temporary = tempfile::tempdir().expect("应建立 panic 日志测试目录");
        let active = active_project_log(temporary.path());
        let expected = active
            .established_log_path()
            .expect("测试项目日志 runtime 应建立")
            .to_path_buf();
        let boundary = CommandPanicBoundary::default();
        boundary.prepare(
            RuntimeEngine::RpgMakerMv,
            crate::diagnostic::RuntimeCommand::Init,
            &temporary.path().join("mv/phase-contract"),
        );
        let operation_boundary = boundary.clone();

        let report = catch_command_panic(boundary, async move {
            operation_boundary.observe_project_log(&active);
            panic!("测试项目日志建立后的命令 panic");
            #[allow(unreachable_code)]
            ProductionCommandRunReport::failed_before_logging(ProductionCommandError::stderr_write(
                io::Error::other("不可达"),
            ))
        })
        .await;

        assert_eq!(report.panic_log_path.as_deref(), Some(expected.as_path()));
        let CommandRunResult::Failed(error) = report.result else {
            panic!("命令 panic 必须报告失败");
        };
        let crate::diagnostic::DiagnosticIssue::Runtime(RuntimeIssue::CommandPanicked {
            log_path: Some(log_path),
            ..
        }) = error.failure_report().report().primary().issue()
        else {
            panic!("命令 panic 的类型化诊断必须保留已建立日志路径");
        };
        assert_eq!(log_path.as_str(), expected.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn command_report_keeps_the_selected_translate_redactor_after_panic() {
        let boundary = CommandPanicBoundary::default();
        boundary.prepare(
            RuntimeEngine::RpgMakerMv,
            crate::diagnostic::RuntimeCommand::Translate,
            Path::new("projects/mv/redactor-project"),
        );
        let redactor = Arc::new(ApiKeyRedactor::new(secrecy::SecretString::from(
            "selected-secret",
        )));
        boundary.observe_selected_api_key_redactor(Arc::clone(&redactor));

        let report = catch_command_panic(boundary, async { panic!("Translate panic") }).await;

        assert!(
            report
                .selected_api_key_redactor
                .as_ref()
                .is_some_and(|selected| Arc::ptr_eq(selected, &redactor))
        );
    }

    fn observer() -> (
        TerminalProgress<InitProgressPhase>,
        ProductionProgressObserver<InitProgressPhase>,
    ) {
        let terminal = TerminalProgress::with_writer(io::sink(), |_| String::new());
        let observer =
            ProductionProgressObserver::without_project_log(terminal.observer(), init_phase_code);
        (terminal, observer)
    }

    fn cancelled_wait_error() -> ProductionCommandError {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        ProductionCommandError::Internal(Box::new(ReportedFailure::new(
            report,
            io::Error::other("测试取消等待"),
        )))
    }

    #[test]
    fn zero_total_phase_is_completed_for_the_project_log_lifecycle() {
        let (terminal, observer) = observer();

        observer.observe(ProgressSnapshot::determinate(
            InitProgressPhase::ScanningSource,
            0,
            0,
        ));

        let state = observer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            state.phases[0].finished,
            "ProjectLog 生命周期必须把 0/0 作为已完成，而终端仍自行省略 0/0"
        );
        assert_eq!(state.phases.len(), 1);
        assert_eq!(state.phases[0].phase, InitProgressPhase::ScanningSource);
        drop(state);
        terminal.finish().expect("关闭静默进度不应失败");
    }

    #[test]
    fn interrupted_cancelled_wait_stops_the_active_phase() {
        let (terminal, observer) = observer();
        observer.observe(ProgressSnapshot::indeterminate(
            InitProgressPhase::ScanningSource,
        ));

        let execution: DrivenCommand<Result<OperationCompletion<()>, ProductionCommandError>> =
            DrivenCommand::Interrupted(Err(cancelled_wait_error()));
        finish_progress_business_state(&observer, &execution);

        let state = observer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            state.phases.iter().all(|phase| phase.finished),
            "取消等待必须停止已开始阶段，避免 ProjectLog 收尾时报告 active_phase"
        );
        drop(state);
        terminal.finish().expect("关闭静默进度不应失败");
    }

    #[test]
    fn nested_extract_progress_does_not_restart_the_owner_phase() {
        let terminal = TerminalProgress::with_writer(io::sink(), |_| String::new());
        let observer = ProductionProgressObserver::without_project_log(
            terminal.observer(),
            extract_phase_code,
        );

        observer.observe(ProgressSnapshot::determinate(
            ExtractProgressPhase::Builtin,
            0,
            1,
        ));
        observer.observe(ProgressSnapshot::indeterminate(
            ExtractProgressPhase::BuiltinDocuments,
        ));
        observer.observe(ProgressSnapshot::determinate(
            ExtractProgressPhase::BuiltinDocuments,
            0,
            0,
        ));
        observer.observe(ProgressSnapshot::determinate(
            ExtractProgressPhase::Builtin,
            1,
            1,
        ));

        let state = observer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            state.phases.len(),
            2,
            "同一 owner 回到完成快照时不得建立第二个日志阶段"
        );
        assert!(state.phases.iter().all(|phase| phase.finished));
        assert_eq!(state.phases[0].phase, ExtractProgressPhase::Builtin);
        assert_eq!(
            state.phases[1].phase,
            ExtractProgressPhase::BuiltinDocuments
        );
        drop(state);
        terminal.finish().expect("关闭静默进度不应失败");
    }

    #[test]
    fn successful_business_finish_leaves_active_phase_for_log_contract_validation() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let project_log = active_project_log(temporary.path());
        let terminal = TerminalProgress::with_writer(io::sink(), |_| String::new());
        let observer =
            ProductionProgressObserver::new(terminal.observer(), &project_log, init_phase_code);
        observer.observe(ProgressSnapshot::indeterminate(
            InitProgressPhase::CheckingProject,
        ));

        let execution: DrivenCommand<Result<OperationCompletion<()>, ProductionCommandError>> =
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(())));
        finish_progress_business_state(&observer, &execution);

        let state = observer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(state.phases.len(), 1);
        assert!(!state.phases[0].finished, "普通成功收尾不得合成阶段完成");
        drop(state);

        let warning = project_log
            .pending_succeeded()
            .finish()
            .expect("未完成阶段必须被项目日志合同捕获");
        assert!(
            warning.project_log.iter().any(|report| {
                let wire = serde_json::to_value(report).expect("合同诊断应可序列化");
                wire["primary"]["issue"]["details"]["problem"]["violation"]["kind"]
                    == "active_phase"
            }),
            "应保留 active_phase 项目日志合同诊断"
        );
        terminal.finish().expect("关闭静默进度不应失败");
    }

    #[test]
    fn planning_completed_only_finishes_the_planning_phase() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let project_log = active_project_log(temporary.path());
        let terminal = TerminalProgress::with_writer(io::sink(), |_| String::new());
        let observer = ProductionProgressObserver::new(
            terminal.observer(),
            &project_log,
            translate_phase_code,
        );
        observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::Planning,
        ));
        observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::ConfirmedTasks,
        ));
        let business_log = ProductionBusinessLog::for_translation(&project_log, observer.clone());

        RpgMakerTranslationLog::emit(
            &business_log,
            RpgMakerTranslationLogEvent::PlanningCompleted {
                report: RpgMakerTranslationRunReport::with_reconciliation(0, 0, 0, 0, 0, 0, 0),
            },
        );

        let state = observer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            state
                .phases
                .iter()
                .find(|phase| phase.phase == TranslateProgressPhase::Planning)
                .expect("Planning 阶段应已开始")
                .finished
        );
        assert!(
            !state
                .phases
                .iter()
                .find(|phase| phase.phase == TranslateProgressPhase::ConfirmedTasks)
                .expect("ConfirmedTasks 阶段应已开始")
                .finished,
            "PlanningCompleted 不得完成其他阶段"
        );
        drop(state);

        observer.stop_active(PhaseStopOutcome::Cancelled);
        drop(business_log);
        assert!(
            project_log.pending_cancelled().finish().is_none(),
            "显式完成 Planning 并停止其余阶段后应通过日志合同"
        );
        terminal.finish().expect("关闭静默进度不应失败");
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
        Arc::clone(project_log.performance()),
    );
    let finalizer = FinalProjectRunPlanPersistenceService::new(transaction_executor.clone());
    let finalization = drive_command(
        finalizer.replace_final(database_path.clone(), replacement),
        termination_signals,
        || transaction_executor.cancel_waits(),
        on_cancellation,
    )
    .await;
    merge_run_plan_finalization(execution, finalization, &database_path, project_log)
}

fn merge_run_plan_finalization<T>(
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

fn observe_run_plan_result(
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

fn emit_run_plan_saved(project_log: &ActiveProjectLog, database_path: &Path) {
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

fn observe_run_plan_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
    database_path: &Path,
    run_continues: bool,
    project_log: &ActiveProjectLog,
) -> ProductionCommandError {
    emit_run_plan_error_fact(&error, database_path, run_continues, project_log);
    map_run_plan_replace_error(error)
}

fn emit_run_plan_error_fact(
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

fn map_run_plan_replace_error(
    error: ProjectRunPlanReplaceError<SqliteRuntimeError>,
) -> ProductionCommandError {
    let diagnostic = error.diagnostic_report();
    let effect = diagnostic.effect();
    let report = ProductionCommandError::report_diagnostic(error, diagnostic);
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
    handle: ProjectLogHandle,
    translation_total: Arc<AtomicU64>,
    translation_started: Arc<AtomicU64>,
    translation_confirmed: Arc<AtomicU64>,
    translation_complete: Arc<AtomicU64>,
    translation_partial: Arc<AtomicU64>,
    translation_unavailable: Arc<AtomicU64>,
    translation_failed: Arc<AtomicU64>,
    translation_cancelled: Arc<AtomicU64>,
    terminal_task_diagnostic: Arc<Mutex<Option<(DiagnosticOccurrenceId, &'static str)>>>,
    pending_publication_failure: Arc<Mutex<Option<WriteBackLogPublicationOutcome>>>,
    translation_retry_attempts: Arc<AtomicU64>,
    translation_retry_recovered: Arc<AtomicU64>,
    translation_retry_exhausted: Arc<AtomicU64>,
    translation_summary: Arc<Mutex<Option<RpgMakerTranslationRunReport>>>,
    translation_progress: Option<ProductionProgressObserver<TranslateProgressPhase>>,
}

impl ProductionBusinessLog {
    fn from_active(project_log: &ActiveProjectLog) -> Self {
        Self {
            handle: project_log.handle().clone(),
            translation_total: Arc::new(AtomicU64::new(0)),
            translation_started: Arc::new(AtomicU64::new(0)),
            translation_confirmed: Arc::new(AtomicU64::new(0)),
            translation_complete: Arc::new(AtomicU64::new(0)),
            translation_partial: Arc::new(AtomicU64::new(0)),
            translation_unavailable: Arc::new(AtomicU64::new(0)),
            translation_failed: Arc::new(AtomicU64::new(0)),
            translation_cancelled: Arc::new(AtomicU64::new(0)),
            terminal_task_diagnostic: Arc::new(Mutex::new(None)),
            pending_publication_failure: Arc::new(Mutex::new(None)),
            translation_retry_attempts: Arc::new(AtomicU64::new(0)),
            translation_retry_recovered: Arc::new(AtomicU64::new(0)),
            translation_retry_exhausted: Arc::new(AtomicU64::new(0)),
            translation_summary: Arc::new(Mutex::new(None)),
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
        self.handle.emit(ProjectLogEvent::RetrySummary {
            attempted,
            recovered: self.translation_retry_recovered.load(Ordering::Acquire),
            exhausted: self.translation_retry_exhausted.load(Ordering::Acquire),
        });
    }

    fn translation_counters(&self) -> Result<TranslationTaskCounters, DiagnosticReport> {
        let planned = self.translation_total.load(Ordering::Acquire);
        let started = self.translation_started.load(Ordering::Acquire);
        let complete = self.translation_complete.load(Ordering::Acquire);
        let partial = self.translation_partial.load(Ordering::Acquire);
        let unavailable = self.translation_unavailable.load(Ordering::Acquire);
        let failed = self.translation_failed.load(Ordering::Acquire);
        let cancelled = self.translation_cancelled.load(Ordering::Acquire);
        let not_started = planned.saturating_sub(started);
        TranslationTaskCounters::new(
            planned,
            started,
            complete,
            partial,
            unavailable,
            failed,
            cancelled,
            not_started,
        )
        .map_err(|source| {
            let violation = match source {
                TaskCounterInvariantError::StartedBreakdown => {
                    crate::diagnostic::TranslationTaskCounterInvariant::StartedBreakdown
                }
                TaskCounterInvariantError::PlannedBreakdown => {
                    crate::diagnostic::TranslationTaskCounterInvariant::PlannedBreakdown
                }
                TaskCounterInvariantError::Overflow => {
                    crate::diagnostic::TranslationTaskCounterInvariant::Overflow
                }
            };
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::runtime(RuntimeIssue::TranslationTaskCountersInvalid {
                    planned,
                    started,
                    complete,
                    partial,
                    unavailable,
                    failed,
                    cancelled,
                    not_started,
                    violation,
                }),
            )
        })
    }

    fn translation_summary(output: &TranslateOutput) -> TranslationEngineSummary {
        let summary = output.summary;
        TranslationEngineSummary::RpgMaker(RpgMakerTranslationSummary {
            accepted_decisions: usize_to_u64(summary.accepted_decisions, "已接受决策数"),
            written_locations: usize_to_u64(summary.written_locations, "已写入位置数"),
            remaining_decisions: usize_to_u64(summary.remaining_decisions, "剩余决策数"),
            remaining_locations: usize_to_u64(summary.remaining_locations, "剩余位置数"),
            protocol_diagnostics: usize_to_u64(summary.protocol_diagnostics, "协议诊断数"),
            recoverable_request_exhaustions: usize_to_u64(
                summary.recoverable_request_exhaustions,
                "可恢复请求耗尽数",
            ),
            request_admission_stopped: summary.request_admission_stopped,
            retained: usize_to_u64(summary.retained, "保留决策数"),
            invalidated: usize_to_u64(summary.invalidated, "失效决策数"),
            not_applicable: usize_to_u64(summary.not_applicable, "不适用决策数"),
            reused: usize_to_u64(summary.reused, "复用决策数"),
        })
    }

    fn translation_run_summary(summary: &RpgMakerTranslationRunReport) -> TranslationEngineSummary {
        TranslationEngineSummary::RpgMaker(RpgMakerTranslationSummary {
            accepted_decisions: usize_to_u64(summary.accepted_decisions(), "已接受决策数"),
            written_locations: usize_to_u64(summary.written_locations(), "已写入位置数"),
            remaining_decisions: usize_to_u64(summary.unresolved_decisions(), "剩余决策数"),
            remaining_locations: usize_to_u64(summary.unresolved_locations(), "剩余位置数"),
            protocol_diagnostics: usize_to_u64(summary.protocol_diagnostics(), "协议诊断数"),
            recoverable_request_exhaustions: usize_to_u64(
                summary.recoverable_request_exhaustions(),
                "可恢复请求耗尽数",
            ),
            request_admission_stopped: summary.request_admission_stopped(),
            retained: usize_to_u64(summary.retained(), "保留决策数"),
            invalidated: usize_to_u64(summary.invalidated(), "失效决策数"),
            not_applicable: usize_to_u64(summary.not_applicable(), "不适用决策数"),
            reused: usize_to_u64(summary.reused(), "复用决策数"),
        })
    }

    fn current_translation_summary(&self) -> Option<TranslationEngineSummary> {
        self.translation_summary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(Self::translation_run_summary)
    }

    fn terminal_translation_summary(&self) -> Option<TranslationTerminalSummary> {
        Some(TranslationTerminalSummary {
            tasks: self.translation_counters().ok()?,
            engine: self.current_translation_summary()?,
        })
    }

    /// 写出 Translate 命令唯一的业务终态；返回失败终态复用的 occurrence。
    fn emit_translation_finished(
        &self,
        execution: &DrivenCommand<
            Result<OperationCompletion<TranslateOutput>, ProductionCommandError>,
        >,
    ) -> Option<(StateEffect, DiagnosticOccurrenceId, &'static str)> {
        let tasks = match self.translation_counters() {
            Ok(tasks) => tasks,
            Err(report) => {
                // task.finished 诊断未能登记时，应用层计数可能比 logger 实际接受的
                // 事件更靠前。终态改从 runtime 的状态机取数，仍写出唯一的 Failed，
                // 而不是让 finish() 以缺少 translation.finished 再次掩盖首因。
                let planned = self.translation_total.load(Ordering::Acquire);
                let code = report.primary().code();
                let diagnostic = self.handle.record_diagnostic(DiagnosticScope::Run, report);
                let tasks = self.handle.translation_task_counters(planned);
                if let (Some(diagnostic), Some(tasks)) = (diagnostic, tasks) {
                    self.handle.emit(ProjectLogEvent::TranslationFinished {
                        result: TranslationFinished::Failed {
                            tasks,
                            summary: self.current_translation_summary(),
                            diagnostic,
                        },
                    });
                    return Some((StateEffect::Unchanged, diagnostic, code));
                }
                // logger 自身已经不可用时，emit 仍由 handle 尝试一次最小终态；它会把
                // 无法表达的状态转换记录为独立 observability 诊断，绝不伪造成功。
                self.handle.emit(ProjectLogEvent::TranslationFinished {
                    result: TranslationFinished::NotStarted,
                });
                return None;
            }
        };
        let completed = match execution {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output)))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(output)))
            | DrivenCommand::SignalFailed {
                result: Ok(OperationCompletion::Completed(output)),
                ..
            } => Some(output),
            _ => None,
        };
        let result = if let Some(output) = completed {
            let summary = Self::translation_summary(output);
            if output.summary.is_incomplete() {
                TranslationFinished::Incomplete { tasks, summary }
            } else if output.summary.total_tasks == 0 {
                TranslationFinished::NoWork { tasks, summary }
            } else {
                TranslationFinished::Complete { tasks, summary }
            }
        } else if matches!(
            execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
                | DrivenCommand::Interrupted(Ok(_))
                | DrivenCommand::SignalFailed {
                    result: Ok(OperationCompletion::Cancelled),
                    ..
                }
        ) {
            TranslationFinished::Cancelled {
                tasks,
                summary: self.current_translation_summary(),
            }
        } else {
            let error = match execution {
                DrivenCommand::Finished(Err(error)) | DrivenCommand::Interrupted(Err(error)) => {
                    Some(error)
                }
                DrivenCommand::SignalFailed {
                    result: Err(error), ..
                } => Some(error),
                DrivenCommand::SignalFailed { result: Ok(_), .. }
                | DrivenCommand::Finished(Ok(_))
                | DrivenCommand::Interrupted(Ok(_)) => None,
            };
            let existing = *self
                .terminal_task_diagnostic
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let diagnostic = existing.or_else(|| {
                error.and_then(|error| {
                    let report = error.failure_report().report().clone();
                    let code = report.primary().code();
                    self.handle
                        .record_diagnostic(DiagnosticScope::RunPlan, report)
                        .map(|diagnostic| (diagnostic, code))
                })
            });
            let Some((diagnostic, code)) = diagnostic else {
                self.handle.emit(ProjectLogEvent::TranslationFinished {
                    result: TranslationFinished::NotStarted,
                });
                return None;
            };
            self.handle.emit(ProjectLogEvent::TranslationFinished {
                result: TranslationFinished::Failed {
                    tasks,
                    summary: self.current_translation_summary(),
                    diagnostic,
                },
            });
            let effect = error
                .map(|error| error.failure_report().report().effect())
                .unwrap_or(StateEffect::ProgressPreserved);
            return Some((effect, diagnostic, code));
        };
        self.handle
            .emit(ProjectLogEvent::TranslationFinished { result });
        None
    }

    fn has_pending_publication_failure(&self) -> bool {
        self.pending_publication_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn emit_publication_failure(&self, diagnostic: DiagnosticOccurrenceId) {
        let outcome = self
            .pending_publication_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(outcome) = outcome else {
            return;
        };
        let result = match outcome {
            WriteBackLogPublicationOutcome::NotPublished => {
                PublicationFinished::NotPublished { diagnostic }
            }
            WriteBackLogPublicationOutcome::PublishedWithResiduals
            | WriteBackLogPublicationOutcome::RecoveryRequired => {
                PublicationFinished::RecoveryRequired { diagnostic }
            }
            WriteBackLogPublicationOutcome::OutcomeUnknown => {
                PublicationFinished::OutcomeUnknown { diagnostic }
            }
            WriteBackLogPublicationOutcome::Published { .. } => {
                unreachable!("成功发布已经立即写出 publication.finished")
            }
        };
        self.handle
            .emit(ProjectLogEvent::PublicationFinished { result });
    }
}

#[derive(Debug)]
enum ProjectLuaExecutionError {
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
struct ProjectLuaPreflightError(ProjectLuaFailure);

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

fn project_lua_run_was_cancelled(error: &ProjectLuaRunError) -> bool {
    matches!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
            | ProjectLuaRunError::Failed(ProjectLuaFailure::Cancelled)
            | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    )
}

impl RpgMakerTranslationLog for ProductionBusinessLog {
    fn emit(&self, event: RpgMakerTranslationLogEvent) {
        match event {
            RpgMakerTranslationLogEvent::PlanningCompleted { report } => {
                let total_tasks = report.total_tasks();
                self.translation_total.store(
                    u64::try_from(total_tasks).expect("当前目标平台的任务总数必须能用 u64 表达"),
                    Ordering::Release,
                );
                *self
                    .translation_summary
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
                if let Some(progress) = &self.translation_progress {
                    progress.complete_phase(TranslateProgressPhase::Planning);
                }
            }
            RpgMakerTranslationLogEvent::TaskStarted {
                task_index,
                total_tasks,
            } => {
                let total =
                    u64::try_from(total_tasks).expect("当前目标平台的任务总数必须能用 u64 表达");
                let ordinal = u64::try_from(task_index.get())
                    .expect("当前目标平台的任务序号必须能用 u64 表达")
                    .checked_add(1)
                    .expect("任务序号加一不得溢出");
                let task = TaskPosition::new(ordinal, total)
                    .expect("Planner 必须产生处于任务总数范围内的序号");
                self.translation_total.store(total, Ordering::Release);
                increment_counter(&self.translation_started, 1, "已开始翻译任务");
                if let Some(progress) = &self.translation_progress {
                    progress.observe(ProgressSnapshot::determinate(
                        TranslateProgressPhase::ConfirmedTasks,
                        self.translation_confirmed.load(Ordering::Acquire),
                        total,
                    ));
                }
                self.handle.emit(ProjectLogEvent::TaskStarted { task });
            }
            RpgMakerTranslationLogEvent::TaskFinished {
                task_index,
                outcome,
                attempts,
                retry_exhausted,
                report,
            } => {
                let total = self.translation_total.load(Ordering::Acquire);
                let ordinal = u64::try_from(task_index.get())
                    .expect("当前目标平台的任务序号必须能用 u64 表达")
                    .checked_add(1)
                    .expect("任务序号加一不得溢出");
                let task =
                    TaskPosition::new(ordinal, total).expect("已开始任务必须保留原始任务总数");
                let attempts = attempts
                    .map(|value| {
                        u64::try_from(value.get()).expect("当前目标平台的尝试次数必须能用 u64 表达")
                    })
                    .unwrap_or(0);
                let retries = attempts.saturating_sub(1);
                if retries > 0 {
                    increment_counter(&self.translation_retry_attempts, retries, "翻译重试次数");
                    if retry_exhausted {
                        increment_counter(&self.translation_retry_exhausted, 1, "重试耗尽任务数");
                    } else {
                        increment_counter(&self.translation_retry_recovered, 1, "重试恢复任务数");
                    }
                }
                let diagnostics = outcome
                    .diagnostics()
                    .cloned()
                    .filter_map(|report| {
                        let code = report.primary().code();
                        self.handle
                            .record_diagnostic(DiagnosticScope::TranslationTask, report)
                            .map(|diagnostic| (diagnostic, code))
                    })
                    .collect::<Vec<_>>();
                let diagnostic = diagnostics.first().copied();
                let log_outcome = match &outcome {
                    RpgMakerTranslationLogTaskOutcome::Complete { .. } => {
                        increment_counter(&self.translation_complete, 1, "完整任务数");
                        Some(TaskFinishedOutcome::Complete)
                    }
                    RpgMakerTranslationLogTaskOutcome::Partial { .. } => {
                        increment_counter(&self.translation_partial, 1, "部分完成任务数");
                        diagnostic
                            .map(|(diagnostic, _)| TaskFinishedOutcome::Partial { diagnostic })
                    }
                    RpgMakerTranslationLogTaskOutcome::Unavailable { .. } => {
                        increment_counter(&self.translation_unavailable, 1, "Unavailable 任务数");
                        diagnostic
                            .map(|(diagnostic, _)| TaskFinishedOutcome::Unavailable { diagnostic })
                    }
                    RpgMakerTranslationLogTaskOutcome::Cancelled => {
                        increment_counter(&self.translation_cancelled, 1, "取消任务数");
                        Some(TaskFinishedOutcome::Cancelled)
                    }
                    RpgMakerTranslationLogTaskOutcome::ExecutionFailed { .. }
                    | RpgMakerTranslationLogTaskOutcome::CommitFailed { .. }
                    | RpgMakerTranslationLogTaskOutcome::InvalidResult { .. } => {
                        increment_counter(&self.translation_failed, 1, "失败任务数");
                        diagnostic.map(|(diagnostic, _)| TaskFinishedOutcome::Failed { diagnostic })
                    }
                    RpgMakerTranslationLogTaskOutcome::NotCommittedAfterEarlierFailure {
                        ..
                    } => {
                        increment_counter(&self.translation_failed, 1, "前序失败后未提交任务数");
                        self.terminal_task_diagnostic
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .as_ref()
                            .copied()
                            .map(|(diagnostic, _)| {
                                TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic }
                            })
                    }
                };
                if let Some(TaskFinishedOutcome::Failed {
                    diagnostic: task_diagnostic,
                }) = log_outcome
                {
                    let mut terminal = self
                        .terminal_task_diagnostic
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let code = diagnostic
                        .filter(|(occurrence, _)| *occurrence == task_diagnostic)
                        .map(|(_, code)| code)
                        .or_else(|| {
                            terminal
                                .as_ref()
                                .filter(|(occurrence, _)| *occurrence == task_diagnostic)
                                .map(|(_, code)| *code)
                        })
                        .expect("已写入项目日志的失败任务必须持有主诊断代码");
                    if terminal.is_none() {
                        *terminal = Some((task_diagnostic, code));
                    }
                }
                if let Some(log_outcome) = log_outcome {
                    self.handle.emit(ProjectLogEvent::TaskFinished {
                        task,
                        attempts,
                        outcome: log_outcome,
                    });
                }
                if matches!(
                    &outcome,
                    RpgMakerTranslationLogTaskOutcome::Complete { .. }
                        | RpgMakerTranslationLogTaskOutcome::Partial { .. }
                        | RpgMakerTranslationLogTaskOutcome::Unavailable { .. }
                ) {
                    let confirmed =
                        increment_counter(&self.translation_confirmed, 1, "已确认翻译任务");
                    if let Some(progress) = &self.translation_progress {
                        progress.observe(ProgressSnapshot::determinate(
                            TranslateProgressPhase::ConfirmedTasks,
                            confirmed,
                            total,
                        ));
                    }
                }
                *self
                    .translation_summary
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
            }
        }
    }
}

fn increment_counter(counter: &AtomicU64, amount: u64, name: &'static str) -> u64 {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount)
        })
        .unwrap_or_else(|_| panic!("{name}不得溢出"))
        .checked_add(amount)
        .expect("fetch_update 已验证加法")
}

impl WriteBackLog for ProductionBusinessLog {
    fn emit(&self, event: WriteBackLogEvent) {
        match event {
            WriteBackLogEvent::PublicationStarted { output_root } => {
                self.handle
                    .emit(ProjectLogEvent::publication_started(output_root));
            }
            WriteBackLogEvent::PublicationFinished {
                outcome: WriteBackLogPublicationOutcome::Published { summary },
                ..
            } => {
                self.handle.emit(ProjectLogEvent::PublicationFinished {
                    result: PublicationFinished::Published {
                        summary: PublicationSummary::RpgMaker(RpgMakerPublicationSummary {
                            translated_units: usize_to_u64(
                                summary.translated_units,
                                "已翻译写回单元数",
                            ),
                            original_units: usize_to_u64(
                                summary.original_units,
                                "保留原文写回单元数",
                            ),
                        }),
                    },
                });
            }
            WriteBackLogEvent::PublicationFinished { outcome, .. } => {
                *self
                    .pending_publication_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
            }
        }
    }
}

fn usize_to_u64(value: usize, name: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{name}必须能用 u64 表达"))
}

type ProductionTranslationProfile = Arc<RpgMakerTranslationProfile<OpenAiCompatibleClient>>;
type ProductionTranslationAssetReader =
    RpgMakerTranslationAssetReadingService<RusqliteStorage, RayonCpuExecutor>;
type ProductionTranslationPlanner = RpgMakerTranslationTaskPlanningService<
    TranslationPlanningResourceReadingService<SystemFileSystem, RayonCpuExecutor>,
    RayonCpuExecutor,
    OpenAiCompatibleClient,
>;
type ProductionTranslationExecutor = RpgMakerTranslationTaskExecutionService<
    OpenAiCompatibleExecutor,
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
    Rules,
    Example,
}

#[derive(Debug)]
enum RpgMakerPromptPreparationError {
    Cancelled,
    SystemResource(PromptResourceLoadError),
    ThinkingResource(PromptResourceLoadError),
    RulesResource(PromptResourceLoadError),
    ExampleResource(PromptResourceLoadError),
    SystemTemplate(PromptTemplateError),
    ThinkingTemplate(PromptTemplateError),
    RulesTemplate(PromptTemplateError),
    ExampleTemplate(PromptTemplateError),
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

fn assemble_rpg_maker_system_prompt_markdown_with_cancellation(
    rendered_system: String,
    thinking: Option<String>,
    rules: String,
    example: String,
    response_mode: TranslationResponseMode,
    cancellation: &CooperativeCancellation,
) -> Result<(String, TranslationResponseMode), RpgMakerPromptPreparationError> {
    let prompt = assemble_translation_system_prompt_with_cancellation(
        rendered_system,
        thinking,
        rules,
        example,
        || ensure_rpg_maker_prompt_preparation_running(cancellation),
    )?;
    Ok((prompt, response_mode))
}

#[cfg(test)]
fn assemble_rpg_maker_system_prompt_markdown(
    rendered_system: String,
    thinking: Option<&str>,
    rules: &str,
    example: &str,
    response_mode: TranslationResponseMode,
) -> (String, TranslationResponseMode) {
    assemble_rpg_maker_system_prompt_markdown_with_cancellation(
        rendered_system,
        thinking.map(str::to_owned),
        rules.to_owned(),
        example.to_owned(),
        response_mode,
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
    fn thinking_component_rejects_template_openers_and_allows_json_closers() {
        assert_eq!(ensure_no_prompt_template_variables("实际思考要求"), Ok(()));
        for text in ["{{source_language}}", "前缀 {{"] {
            assert_eq!(
                ensure_no_prompt_template_variables(text),
                Err(PromptTemplateError::VariablesNotAllowed)
            );
        }
        assert_eq!(ensure_no_prompt_template_variables("后缀 }}"), Ok(()));
    }

    #[test]
    fn rpg_maker_prompt_has_exact_json_only_assembly() {
        let mode = TranslationResponseMode::new(false, false);
        let (prompt, response_mode) = assemble_rpg_maker_system_prompt_markdown(
            "rendered system".to_owned(),
            None,
            "current rules",
            "current example",
            mode,
        );

        assert_eq!(
            prompt,
            "rendered system\n\ncurrent rules\n\ncurrent example"
        );
        assert_eq!(response_mode, mode);
    }

    #[test]
    fn rpg_maker_prompt_has_exact_thinking_assembly() {
        let mode = TranslationResponseMode::new(true, true);
        let (prompt, response_mode) = assemble_rpg_maker_system_prompt_markdown(
            "rendered system".to_owned(),
            Some("thinking requirement"),
            "current rules",
            "current example",
            mode,
        );

        assert_eq!(
            prompt,
            "rendered system\n\nthinking requirement\n\ncurrent rules\n\ncurrent example"
        );
        assert_eq!(response_mode, mode);
    }

    #[test]
    fn post_await_cancellation_does_not_overwrite_an_already_formed_build_error() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let failure = ProductionTranslationExecutionBuildError::prompt_template(
            PromptResourceComponent::System,
            Path::new("C:/att/prompts/translation/system.md"),
            PromptTemplateError::VariablesNotAllowed,
        );

        let error = complete_translation_execution_build_step::<()>(Err(failure), &cancellation)
            .expect_err("已经形成的 Prompt 错误必须先于后到取消返回");

        assert!(!error.is_cancelled());
        let cancelled = complete_translation_execution_build_step(Ok(()), &cancellation)
            .expect_err("成功步骤之后观察到取消时应返回类型化取消");
        assert!(cancelled.is_cancelled());
    }

    #[test]
    fn production_build_error_classifies_only_typed_cancellation_leaves() {
        let path = PathBuf::from("C:/att/prompts/translation/system.md");
        let cancelled = ProductionTranslationExecutionBuildError::prompt_resource(
            PromptResourceComponent::System,
            &path,
            PromptResourceLoadError::Read(ReadFileError::Io {
                path: path.clone(),
                source: SystemFileSystemError::Cancelled {
                    operation: "read_file",
                    path: path.clone(),
                },
            }),
        );
        assert!(cancelled.is_cancelled());

        let ordinary_io = ProductionTranslationExecutionBuildError::prompt_resource(
            PromptResourceComponent::System,
            &path,
            PromptResourceLoadError::Read(ReadFileError::Io {
                path: path.clone(),
                source: SystemFileSystemError::Io {
                    operation: "read_file",
                    path: path.clone(),
                    source: io::Error::other("disk failure"),
                },
            }),
        );
        assert!(!ordinary_io.is_cancelled());
        assert!(
            ProductionTranslationExecutionBuildError::prompt_cpu(CpuTaskExecutionError::Cancelled)
                .is_cancelled()
        );
    }
}

struct ProductionSelectedTranslationExecutionBuilder<'a> {
    configuration: &'a TranslateConfiguration,
    file_system: SystemFileSystem,
    cpu: RayonCpuExecutor,
    sqlite: RusqliteStorage,
    llm: OpenAiCompatibleExecutor,
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
    let response_mode =
        TranslationResponseMode::new(configuration.thinking_output(), configuration.source_echo());
    let prompt_paths =
        translation_prompt_resource_paths(configuration.prompt_root(), response_mode);
    let system_path = prompt_paths.system().to_path_buf();
    let thinking_path = prompt_paths.thinking().map(Path::to_path_buf);
    let rules_path = prompt_paths.rules().to_path_buf();
    let example_path = prompt_paths.example().to_path_buf();
    ensure_translation_execution_build_running(cancellation)?;
    let system_template = read_unparsed_prompt_resource(file_system, &system_path).await;
    let system_template = complete_translation_execution_build_step(
        system_template.map_err(|source| {
            ProductionTranslationExecutionBuildError::prompt_resource(
                PromptResourceComponent::System,
                &system_path,
                source,
            )
        }),
        cancellation,
    )?;
    let thinking = if let Some(path) = thinking_path.as_deref() {
        ensure_translation_execution_build_running(cancellation)?;
        let thinking = read_unparsed_prompt_resource(file_system, path).await;
        Some(complete_translation_execution_build_step(
            thinking.map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }),
            cancellation,
        )?)
    } else {
        None
    };
    ensure_translation_execution_build_running(cancellation)?;
    let rules = complete_translation_execution_build_step(
        read_unparsed_prompt_resource(file_system, &rules_path)
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }),
        cancellation,
    )?;
    let example = complete_translation_execution_build_step(
        read_unparsed_prompt_resource(file_system, &example_path)
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }),
        cancellation,
    )?;

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
            let rules = parse_prompt_resource_with_cancellation(rules, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::RulesResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&rules, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::RulesTemplate)?;
            let example = parse_prompt_resource_with_cancellation(example, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::ExampleResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&example, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::ExampleTemplate)?;
            let (prompt_markdown, response_mode) =
                assemble_rpg_maker_system_prompt_markdown_with_cancellation(
                    rendered_system,
                    thinking,
                    rules,
                    example,
                    response_mode,
                    &prompt_cancellation,
                )?;
            RpgMakerSystemPrompt::new_with_cancellation(
                prompt_language_pair,
                prompt_markdown,
                response_mode,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?
            .map_err(RpgMakerPromptPreparationError::SystemPrompt)
        })
        .await;
    let system_prompt = system_prompt
        .map_err(ProductionTranslationExecutionBuildError::prompt_cpu)?
        .map_err(|source| match source {
            RpgMakerPromptPreparationError::Cancelled => {
                ProductionTranslationExecutionBuildError::cancelled()
            }
            RpgMakerPromptPreparationError::SystemResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
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
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::RulesResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ExampleResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
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
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::RulesTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ExampleTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemPrompt(source) => {
                ProductionTranslationExecutionBuildError::system_prompt(
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
        });
    let system_prompt = complete_translation_execution_build_step(system_prompt, cancellation)?;
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
    type Client = OpenAiCompatibleClient;
    type Translation = ProductionRpgMakerTranslation;
    type Error = ProductionTranslationExecutionBuildError;

    fn is_cancelled_error(error: &Self::Error) -> bool {
        error.is_cancelled()
    }

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
        let placeholders = placeholders
            .map_err(ProductionTranslationExecutionBuildError::placeholder_cpu)?
            .map_err(|_cancelled| ProductionTranslationExecutionBuildError::cancelled())?
            .map_err(ProductionTranslationExecutionBuildError::builtin_placeholder_compile);
        let placeholders =
            complete_translation_execution_build_step(placeholders, &self.cancellation)?;
        let asset_reader =
            RpgMakerTranslationAssetReadingService::new(self.sqlite.clone(), self.cpu.clone());
        let resources = TranslationPlanningResourceReadingService::new(
            self.file_system.clone(),
            self.cpu.clone(),
        )
        .with_cancellation(self.cancellation.clone());
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, OpenAiCompatibleClient>::new(
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

fn complete_translation_execution_build_step<T>(
    result: Result<T, ProductionTranslationExecutionBuildError>,
    cancellation: &CooperativeCancellation,
) -> Result<T, ProductionTranslationExecutionBuildError> {
    let value = result?;
    ensure_translation_execution_build_running(cancellation)?;
    Ok(value)
}

struct ProductionTranslationExecutionBuildError {
    class: TranslationExecutionBuildFailureClass,
    cancelled: bool,
    diagnostic: Box<DiagnosticReport>,
    source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslationExecutionBuildFailureClass {
    ConfigurationOrInput,
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    fn cancelled() -> Self {
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        let mut error = Self::new(TranslationExecutionBuildCancelled, diagnostic);
        error.cancelled = true;
        error
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
        let cancelled = matches!(&source, CpuTaskExecutionError::Cancelled);
        let operation = match operation {
            "prepare_rpg_maker_prompt" => RuntimeOperation::PrepareRpgMakerPrompt,
            "compile_rpg_maker_builtin_placeholders" => {
                RuntimeOperation::CompileRpgMakerBuiltinPlaceholders
            }
            _ => RuntimeOperation::ExecuteTask,
        };
        let diagnostic =
            DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic_for(operation));
        let mut error = Self::new(source, diagnostic);
        error.cancelled = cancelled;
        error
    }

    fn prompt_resource(
        component: PromptResourceComponent,
        path: &Path,
        source: PromptResourceLoadError,
    ) -> Self {
        let _ = (component, path);
        let cancelled = matches!(
            &source,
            PromptResourceLoadError::Read(ReadFileError::Io {
                source: SystemFileSystemError::Cancelled { .. },
                ..
            })
        );
        let diagnostic = source.diagnostic_report();
        let mut error = Self::new(source, diagnostic);
        error.cancelled = cancelled;
        error
    }

    fn prompt_template(
        component: PromptResourceComponent,
        path: &Path,
        source: PromptTemplateError,
    ) -> Self {
        let _ = component;
        let diagnostic = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic(path));
        Self::new(source, diagnostic)
    }

    fn system_prompt(
        component: PromptResourceComponent,
        path: &Path,
        source: RpgMakerSystemPromptError,
    ) -> Self {
        let _ = component;
        let diagnostic = match &source {
            RpgMakerSystemPromptError::Blank => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::translation(TranslationIssue::Prompt {
                    path: SafePath::new(path),
                    problem: PromptProblem::Empty,
                }),
            ),
        };
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
        let available_languages = available_ids
            .iter()
            .map(SafeIdentifier::from_validated)
            .collect();
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::LanguageModuleUnavailable {
                requested_language: SafeIdentifier::from_validated(language_id),
                target_language: SafeIdentifier::from_validated(language_pair.target()),
                available_languages,
            }),
        );
        Self::new(source, diagnostic)
    }

    fn builtin_placeholder_compile(source: Pcre2PlaceholderConstructionError) -> Self {
        let diagnostic = source.diagnostic_report();
        Self::new(source, diagnostic)
    }

    fn new(source: impl Error + Send + Sync + 'static, diagnostic: DiagnosticReport) -> Self {
        let class = if diagnostic.primary().resolution()
            == crate::diagnostic::DiagnosticResolution::ReportBug
        {
            TranslationExecutionBuildFailureClass::Internal
        } else {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput
        };
        Self {
            class,
            cancelled: false,
            diagnostic: Box::new(diagnostic),
            source: Box::new(source),
        }
    }

    const fn diagnostic(&self) -> &DiagnosticReport {
        &self.diagnostic
    }

    const fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl fmt::Debug for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTranslationExecutionBuildError")
            .field("class", &self.class)
            .field("cancelled", &self.cancelled)
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
            self.diagnostic.primary().code()
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
    P: ProjectCommandLeaseProvider<Error = Box<SystemFileSystemError>>,
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

fn interrupted_non_cancellation_error<T>(
    result: Result<T, ProductionCommandError>,
) -> Option<ProductionCommandError> {
    match result {
        Ok(value) => {
            drop(value);
            None
        }
        Err(error) if error.was_cancelled_wait() => None,
        Err(error) => Some(error),
    }
}

pub(crate) struct ProductionCommandRunReport {
    pub(crate) result: CommandRunResult,
    pub(crate) shutdown_error: Option<ShutdownFailures>,
    pub(crate) pending_project_log: Option<PendingProjectLog>,
    pub(crate) panic_log_path: Option<PathBuf>,
    pub(crate) selected_api_key_redactor: Option<Arc<ApiKeyRedactor>>,
    pub(crate) translation_summary: Option<TranslationTerminalSummary>,
}

pub(crate) enum CommandRunResult {
    Succeeded(RpgMakerCommandOutput),
    Interrupted,
    Failed(ProductionCommandError),
}

impl ProductionCommandRunReport {
    fn panicked(error: ProductionCommandError, panic_log_path: Option<PathBuf>) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
            // panic 展开时 ActiveProjectLog 的 runtime 已用独立终态槽完成项目日志。
            pending_project_log: None,
            panic_log_path,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    fn failed_before_logging(error: ProductionCommandError) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
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
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    fn interrupted_before_logging(shutdown: ShutdownFailures) -> Self {
        Self {
            result: CommandRunResult::Interrupted,
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
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
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
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
                panic_log_path: None,
                selected_api_key_redactor: None,
                translation_summary: None,
            },
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(OperationCompletion::Cancelled)) => Self {
                result: CommandRunResult::Interrupted,
                shutdown_error,
                pending_project_log,
                panic_log_path: None,
                selected_api_key_redactor: None,
                translation_summary: None,
            },
            DrivenCommand::Finished(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
                pending_project_log,
                panic_log_path: None,
                selected_api_key_redactor: None,
                translation_summary: None,
            },
            DrivenCommand::Interrupted(Err(error)) => Self {
                result: if error.was_cancelled_wait() {
                    CommandRunResult::Interrupted
                } else {
                    CommandRunResult::Failed(error)
                },
                shutdown_error,
                pending_project_log,
                panic_log_path: None,
                selected_api_key_redactor: None,
                translation_summary: None,
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
                    panic_log_path: None,
                    selected_api_key_redactor: None,
                    translation_summary: None,
                }
            }
        }
    }

    fn with_translation_summary(mut self, summary: Option<TranslationTerminalSummary>) -> Self {
        self.translation_summary = summary;
        self
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

#[derive(Clone, Copy)]
enum InitFailureClass {
    ConfigurationOrInput,
    ProjectState,
    StateAppliedFinalizationFailed,
    RecoveryRequired,
    OutcomeUnknown,
    Internal,
}

fn map_init_error(
    error: InitServiceError<ProductionWorkspaceConvergenceError, Infallible>,
) -> ProductionCommandError {
    match error {
        error @ InitServiceError::ProjectLease(_) => ProductionCommandError::prevalidated_boundary(
            error,
            DiagnosticStage::Init,
            RuntimeBoundaryOperation::InitProjectLeaseAlreadyHeld,
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
                InitFailureClass::RecoveryRequired => {
                    ProductionCommandError::RecoveryRequired(Box::new(report))
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
) -> (InitFailureClass, ReportedFailure) {
    let diagnostic = source.diagnostic_report();
    let class = match diagnostic.effect() {
        StateEffect::AppliedFinalizationFailed => InitFailureClass::StateAppliedFinalizationFailed,
        StateEffect::RecoveryRequired => InitFailureClass::RecoveryRequired,
        StateEffect::OutcomeUnknown => InitFailureClass::OutcomeUnknown,
        StateEffect::Unchanged
        | StateEffect::ProgressPreserved
        | StateEffect::Applied
        | StateEffect::AppliedRunPlanNotSaved => match &source {
            ProjectWorkspaceConvergenceError::SourceGameRoot(_)
            | ProjectWorkspaceConvergenceError::ObserveGameLayout(_)
            | ProjectWorkspaceConvergenceError::InvalidGameLayout { .. }
            | ProjectWorkspaceConvergenceError::EngineWorkspaceRoot(_)
            | ProjectWorkspaceConvergenceError::MissingInitialSettings(_)
            | ProjectWorkspaceConvergenceError::ObserveInputSource(_) => {
                InitFailureClass::ConfigurationOrInput
            }
            ProjectWorkspaceConvergenceError::InvalidStageRequest(_) => InitFailureClass::Internal,
            _ => InitFailureClass::ProjectState,
        },
    };
    (
        class,
        ProductionCommandError::report_diagnostic(source, diagnostic),
    )
}

trait BuiltInExtractDiagnostic: Error + Send + Sync + Sized + 'static {
    fn into_extract_diagnostic(self) -> ReportedFailure;
}

impl<RE, SE, CE> BuiltInExtractDiagnostic for BuiltInExtractionError<RE, SE, CE>
where
    RE: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
    SE: Error
        + crate::rpg_maker::extract::store::RpgMakerExtractionStoreDiagnostic
        + Send
        + Sync
        + 'static,
    CE: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<CE>:
        crate::rpg_maker::extract::RpgMakerExtractionCpuDiagnostic,
{
    fn into_extract_diagnostic(self) -> ReportedFailure {
        self.into_diagnostic_failure()
    }
}

trait RulesExtractDiagnostic: Error + Send + Sync + Sized + 'static {
    fn into_extract_diagnostic(self) -> ReportedFailure;
}

impl<DE, SE, CE> RulesExtractDiagnostic for RulesExtractionError<DE, SE, CE>
where
    DE: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
    SE: Error
        + crate::rpg_maker::extract::store::RpgMakerExtractionStoreDiagnostic
        + Send
        + Sync
        + 'static,
    CE: Error + Send + Sync + 'static,
    crate::execution::cpu::CpuTaskExecutionError<CE>:
        crate::rpg_maker::extract::RpgMakerExtractionCpuDiagnostic,
{
    fn into_extract_diagnostic(self) -> ReportedFailure {
        self.into_diagnostic_failure()
    }
}

fn map_extract_error<OE, BE, RE, PE>(
    error: ExtractServiceError<OE, BE, RE, PE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    BE: BuiltInExtractDiagnostic,
    RE: RulesExtractDiagnostic,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ ExtractServiceError::ProjectLease(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Extract,
                RuntimeBoundaryOperation::ExtractProjectLeaseAlreadyHeld,
            )
        }
        error @ ExtractServiceError::OpenProject(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Extract,
                RuntimeBoundaryOperation::ExtractProjectAlreadyOpened,
            )
        }
        ExtractServiceError::BuiltIn(source) => {
            map_project_failure_report(source.into_extract_diagnostic())
        }
        ExtractServiceError::Rules {
            rules_path: _,
            completed_owners,
            source,
        } => {
            let mut report = source.into_extract_diagnostic();
            if !completed_owners.is_empty() {
                report = report.with_effect(StateEffect::ProgressPreserved);
            }
            map_project_failure_report(report)
        }
    }
}

trait ProductionExternalModelFailure: Error + Send + Sync + 'static {
    fn into_external_model_failure(self) -> ProductionCommandError;
}

trait ProductionTranslationAssetFailure: Error + Send + Sync + Sized + 'static {
    fn into_asset_failure(self) -> ReportedFailure;
}

impl ProductionTranslationAssetFailure
    for crate::rpg_maker::translate::asset_reader::RpgMakerTranslationAssetReadingError<
        crate::runtime::sqlite::SqliteRuntimeError,
        crate::runtime::cpu::CpuExecutorUnavailable,
    >
{
    fn into_asset_failure(self) -> ReportedFailure {
        let report = self.diagnostic_report();
        ReportedFailure::new(report, self)
    }
}

trait ProductionTranslationPlanningFailure: Error + Send + Sync + Sized + 'static {
    fn into_planning_failure(self) -> ReportedFailure;
}

impl ProductionTranslationPlanningFailure
    for crate::rpg_maker::translate::planner::RpgMakerTranslationTaskPlanningError<
        crate::translation::planning_resource::TranslationPlanningResourceReadingError<
            crate::runtime::filesystem::SystemFileSystemError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >,
        crate::runtime::cpu::CpuExecutorUnavailable,
    >
{
    fn into_planning_failure(self) -> ReportedFailure {
        self.into_reported_failure()
    }
}

trait ProductionTranslationResultStorageFailure: Error + Send + Sync + Sized + 'static {
    fn into_result_storage_failure(self) -> ReportedFailure;
}

impl<S, C> ProductionTranslationResultStorageFailure for RpgMakerTranslationResultStorageError<S, C>
where
    S: Error + Send + Sync + 'static,
    C: Error + Send + Sync + 'static,
{
    fn into_result_storage_failure(self) -> ReportedFailure {
        self.into_reported_failure()
    }
}

impl<R, P, E, S> ProductionExternalModelFailure
    for crate::rpg_maker::translate::pipeline::RpgMakerTranslationServiceError<
        R,
        P,
        RpgMakerTranslationTaskExecutionError<OpenAiCompatibleError, E>,
        S,
    >
where
    R: ProductionTranslationAssetFailure,
    P: ProductionTranslationPlanningFailure,
    E: Error + Send + Sync + 'static,
    S: ProductionTranslationResultStorageFailure,
{
    fn into_external_model_failure(self) -> ProductionCommandError {
        use crate::rpg_maker::translate::pipeline::RpgMakerTranslationServiceError as TranslationError;

        match self {
            TranslationError::ReadAssets(source) => {
                map_project_failure_report(source.into_asset_failure())
            }
            TranslationError::PlanTasks(source) => {
                let report = source.into_planning_failure();
                if matches!(
                    report.report().primary().resolution(),
                    crate::diagnostic::DiagnosticResolution::FixInput
                        | crate::diagnostic::DiagnosticResolution::FixConfiguration
                        | crate::diagnostic::DiagnosticResolution::FixPlaceholderRules
                        | crate::diagnostic::DiagnosticResolution::CheckPathAndPermissions
                ) {
                    ProductionCommandError::ConfigurationOrInput(Box::new(report))
                } else {
                    map_project_failure_report(report)
                }
            }
            TranslationError::ApplyPreparation(source) => {
                map_project_failure_report(source.into_result_storage_failure())
            }
            TranslationError::ExecuteTask {
                task_index: _,
                source,
                diagnostic,
            } => match source {
                RpgMakerTranslationTaskExecutionError::FatalRequest { attempt, source } => {
                    let source: RpgMakerTranslationTaskExecutionError<OpenAiCompatibleError, E> =
                        RpgMakerTranslationTaskExecutionError::FatalRequest { attempt, source };
                    ProductionCommandError::ExternalModel(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
                RpgMakerTranslationTaskExecutionError::ProcessResponse { attempt, source } => {
                    let execution_error: RpgMakerTranslationTaskExecutionError<
                        OpenAiCompatibleError,
                        E,
                    > = RpgMakerTranslationTaskExecutionError::ProcessResponse { attempt, source };
                    map_project_diagnostic(execution_error, diagnostic)
                }
                source @ RpgMakerTranslationTaskExecutionError::LlmRequestCancelled { .. } => {
                    ProductionCommandError::ExternalModel(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
                RpgMakerTranslationTaskExecutionError::InternalInvariant { invariant } => {
                    let source: RpgMakerTranslationTaskExecutionError<OpenAiCompatibleError, E> =
                        RpgMakerTranslationTaskExecutionError::InternalInvariant { invariant };
                    ProductionCommandError::Internal(Box::new(
                        ProductionCommandError::report_diagnostic(source, diagnostic),
                    ))
                }
            },
            TranslationError::CommitTask {
                task_index: _,
                source,
                diagnostic,
            } => map_project_diagnostic(source, diagnostic),
            source @ TranslationError::InvalidTaskResultSequence { .. } => {
                let diagnostic = match &source {
                    TranslationError::InvalidTaskResultSequence { diagnostic, .. } => {
                        diagnostic.clone()
                    }
                    _ => unreachable!("匹配的翻译结果序列错误必须保留其结构化诊断"),
                };
                ProductionCommandError::Internal(Box::new(
                    ProductionCommandError::report_diagnostic(source, diagnostic),
                ))
            }
            TranslationError::FinalizeResultStore(source) => {
                map_project_failure_report(source.into_result_storage_failure())
            }
            TranslationError::OperationAndFinalization {
                primary,
                finalization,
            } => primary
                .into_external_model_failure()
                .with_related_finalization_report(finalization.into_result_storage_failure()),
        }
    }
}

fn map_project_diagnostic(
    source: impl Error + Send + Sync + 'static,
    diagnostic: DiagnosticReport,
) -> ProductionCommandError {
    let report = ProductionCommandError::report_diagnostic(source, diagnostic);
    map_project_failure_report(report)
}

fn map_project_failure_report(report: ReportedFailure) -> ProductionCommandError {
    let effect = report.report().effect();
    let resolution = report.report().primary().resolution();
    if effect == StateEffect::OutcomeUnknown {
        ProductionCommandError::OutcomeUnknown(Box::new(report))
    } else if effect == StateEffect::RecoveryRequired {
        ProductionCommandError::RecoveryRequired(Box::new(report))
    } else if effect == StateEffect::AppliedFinalizationFailed {
        ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
    } else if resolution == crate::diagnostic::DiagnosticResolution::ReportBug {
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
                RuntimeBoundaryOperation::TranslateProjectLeaseAlreadyHeld,
            )
        }
        error @ TranslateServiceError::ReadProject { .. } => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::Translate,
                RuntimeBoundaryOperation::TranslateProjectAlreadyOpened,
            )
        }
        TranslateServiceError::BuildExecution(source) => {
            ProductionCommandError::translation_execution_build(source)
        }
        TranslateServiceError::Translation { source } => source.into_external_model_failure(),
    }
}

trait ProductionWriteBackPreparationFailure: Error + Send + Sync + Sized + 'static {
    fn into_write_back_preparation_failure(self) -> ReportedFailure;
}

trait ProductionWriteBackAssetFailure: Error + Send + Sync + Sized + 'static {
    fn into_write_back_asset_failure(self) -> ReportedFailure;
}

impl ProductionWriteBackAssetFailure
    for RpgMakerWriteBackAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable>
{
    fn into_write_back_asset_failure(self) -> ReportedFailure {
        self.into_reported_failure()
    }
}

trait ProductionWriteBackRewriteFailure: Error + Send + Sync + Sized + 'static {
    fn into_write_back_rewrite_failure(self) -> ReportedFailure;
}

impl<R> ProductionWriteBackRewriteFailure
    for RpgMakerWriteBackDocumentRewritingError<R, CpuExecutorUnavailable>
where
    R: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
{
    fn into_write_back_rewrite_failure(self) -> ReportedFailure {
        self.into_reported_failure()
    }
}

impl<R, D> ProductionWriteBackPreparationFailure
    for RpgMakerWriteBackServiceError<R, D, CpuExecutorUnavailable>
where
    R: ProductionWriteBackAssetFailure,
    D: ProductionWriteBackRewriteFailure,
{
    fn into_write_back_preparation_failure(self) -> ReportedFailure {
        match self {
            RpgMakerWriteBackServiceError::ReadAssets(source) => {
                source.into_write_back_asset_failure()
            }
            RpgMakerWriteBackServiceError::SchedulePlanning(source) => {
                let report = write_back_planning_compute_report(&source);
                ReportedFailure::new(report, source)
            }
            RpgMakerWriteBackServiceError::InvalidPlaceholder(source) => {
                let report = source.diagnostic_report();
                ReportedFailure::new(report, source)
            }
            RpgMakerWriteBackServiceError::InvalidPlan(source) => {
                let report = source.diagnostic_report();
                ReportedFailure::new(report, source)
            }
            RpgMakerWriteBackServiceError::RewriteDocuments(source) => {
                source.into_write_back_rewrite_failure()
            }
        }
    }
}

fn map_write_back_error<OE, SE, PE, KE>(
    error: WriteBackServiceError<OE, SE, PE, KE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    SE: ProductionWriteBackPreparationFailure,
    PE: Error + WriteBackPublishingDiagnostic + Send + Sync + 'static,
    KE: Error + Send + Sync + 'static,
{
    match error {
        error @ WriteBackServiceError::ProjectLease(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::WriteBack,
                RuntimeBoundaryOperation::WriteBackProjectLeaseAlreadyHeld,
            )
        }
        WriteBackServiceError::CancellationDiscard {
            candidate_root: _,
            discard,
        } => {
            let report = discard.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        error @ WriteBackServiceError::OpenProject(_) => {
            ProductionCommandError::prevalidated_boundary(
                error,
                DiagnosticStage::WriteBack,
                RuntimeBoundaryOperation::WriteBackProjectAlreadyOpened,
            )
        }
        WriteBackServiceError::Prepare(source) => {
            map_project_failure_report(source.into_write_back_preparation_failure())
        }
        WriteBackServiceError::PrepareCandidate(source) => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::ValidateCandidate {
            candidate_root: _,
            source,
        } => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::ValidateCandidateAndDiscard {
            candidate_root: _,
            source,
            discard,
        } => {
            let report = source.into_write_back_failure_report().with_related(
                RelatedFailureRelation::Discard,
                discard.into_write_back_failure_report(),
            );
            map_project_failure_report(report)
        }
        WriteBackServiceError::Publish { state: _, source } => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProductionCommandError {
    ConfigurationOrInput(Box<ReportedFailure>),
    ProjectUnavailable(Box<ReportedFailure>),
    ProjectState(Box<ReportedFailure>),
    ExternalModel(Box<ReportedFailure>),
    ResultAppliedButRunPlanNotSaved(Box<ReportedFailure>),
    RunPlanOutcomeUnknown(Box<ReportedFailure>),
    StateAppliedButFinalizationFailed(Box<ReportedFailure>),
    RecoveryRequired(Box<ReportedFailure>),
    OutcomeUnknown(Box<ReportedFailure>),
    Internal(Box<ReportedFailure>),
    Signal(Box<ReportedFailure>),
}

impl ProductionCommandError {
    fn report_diagnostic(
        source: impl Error + Send + Sync + 'static,
        diagnostic: DiagnosticReport,
    ) -> ReportedFailure {
        ReportedFailure::new(diagnostic, source)
    }

    fn manual(source: ManualCommandError) -> Self {
        let report = source.diagnostic_report();
        let failure = Box::new(ReportedFailure::new(report.clone(), source));
        match report.effect() {
            StateEffect::Unchanged => Self::ConfigurationOrInput(failure),
            StateEffect::ProgressPreserved => Self::ProjectState(failure),
            StateEffect::Applied => Self::StateAppliedButFinalizationFailed(failure),
            StateEffect::AppliedRunPlanNotSaved => Self::ResultAppliedButRunPlanNotSaved(failure),
            StateEffect::AppliedFinalizationFailed => {
                Self::StateAppliedButFinalizationFailed(failure)
            }
            StateEffect::RecoveryRequired => Self::RecoveryRequired(failure),
            StateEffect::OutcomeUnknown => Self::OutcomeUnknown(failure),
        }
    }

    pub(crate) fn stdout_write(source: io::Error) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::WriteStdout,
                failure: IoFailure::from_error(&source),
            }),
        );
        Self::StateAppliedButFinalizationFailed(Box::new(ReportedFailure::new(report, source)))
    }

    pub(crate) fn stderr_write(source: io::Error) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::WriteStderr,
                failure: IoFailure::from_error(&source),
            }),
        );
        Self::StateAppliedButFinalizationFailed(Box::new(ReportedFailure::new(report, source)))
    }

    fn into_reported_failure(self) -> ReportedFailure {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::RecoveryRequired(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => *report,
        }
    }

    fn with_related_finalization_report(self, related: ReportedFailure) -> Self {
        let primary_outcome_unknown = matches!(
            &self,
            Self::OutcomeUnknown(_) | Self::RunPlanOutcomeUnknown(_)
        );
        let related_outcome_unknown = related.report().effect() == StateEffect::OutcomeUnknown;
        let primary_recovery_required = matches!(&self, Self::RecoveryRequired(_));
        let related_recovery_required = related.report().effect() == StateEffect::RecoveryRequired;
        let report = self
            .into_reported_failure()
            .with_related(RelatedFailureRelation::Finalization, related);
        if primary_outcome_unknown || related_outcome_unknown {
            Self::OutcomeUnknown(Box::new(report))
        } else if primary_recovery_required || related_recovery_required {
            Self::RecoveryRequired(Box::new(report))
        } else {
            Self::StateAppliedButFinalizationFailed(Box::new(report))
        }
    }

    fn configuration_load(source: ConfigurationLoadError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(report, source)))
    }

    fn input_directory(source: ResolveDirectoryError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn run_plan_resolution(source: RunPlanResolutionError) -> Self {
        let problem = match &source {
            RunPlanResolutionError::InitPathRequired
            | RunPlanResolutionError::NoReusableExtractPlan
            | RunPlanResolutionError::ProfileRequired => RpgMakerProjectProblem::RunPlanRequired,
            RunPlanResolutionError::SavedProfileUnavailable { profile_id } => {
                RpgMakerProjectProblem::SavedProfileUnavailable {
                    profile_id: SafeIdentifier::from_validated(profile_id),
                }
            }
        };
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::project(
                RpgMakerDiagnosticStage::CommandPreparation,
                problem,
            )),
        );
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn translation_execution_build(source: ProductionTranslationExecutionBuildError) -> Self {
        let class = source.class;
        let diagnostic = source.diagnostic().clone();
        let report = Box::new(ReportedFailure::new(diagnostic, source));
        match class {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput => {
                Self::ConfigurationOrInput(report)
            }
            TranslationExecutionBuildFailureClass::Internal => Self::Internal(report),
        }
    }

    fn project_lease(source: ProjectCommandLeaseError<Box<SystemFileSystemError>>) -> Self {
        let diagnostic =
            source.diagnostic_report_at(crate::diagnostic::FileSystemDiagnosticStage::Project);
        Self::ProjectUnavailable(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn prevalidated_boundary(
        source: impl Error + Send + Sync + 'static,
        stage: DiagnosticStage,
        operation: RuntimeBoundaryOperation,
    ) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::InternalInvariant {
                stage,
                component: RuntimeComponent::Process,
                operation,
            }),
        );
        Self::Internal(Box::new(Self::report_diagnostic(source, report)))
    }

    fn existing_project_opening(source: ProductionProjectOpeningError) -> Self {
        let diagnostic = source.diagnostic_report();
        let unavailable = matches!(
            &source,
            ExistingProjectOpeningError::ReadProjectRecord(
                ProjectDatabaseReadError::DatabaseNotFound { .. }
            ) | ExistingProjectOpeningError::ResolveSourceData(
                ResolveDirectoryError::NotFound { .. } | ResolveDirectoryError::NotDirectory { .. }
            ) | ExistingProjectOpeningError::ResolveSourceJs(
                ResolveDirectoryError::NotFound { .. } | ResolveDirectoryError::NotDirectory { .. }
            )
        );
        let report = Self::report_diagnostic(source, diagnostic);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    fn project_run_plan_read(source: ProjectRunPlanReadError<SqliteRuntimeError>) -> Self {
        let unavailable = matches!(source, ProjectRunPlanReadError::DatabaseNotFound { .. });
        let diagnostic = source.diagnostic_report();
        let report = Self::report_diagnostic(source, diagnostic);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    fn file_system_build(source: SystemFileSystemBuildError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    fn sqlite_start(source: SqliteRuntimeError) -> Self {
        let diagnostic = source.startup_diagnostic_report();
        Self::Internal(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn http_client_build(source: OpenAiExecutorBuildError) -> Self {
        let configuration = matches!(
            source,
            OpenAiExecutorBuildError::InvalidProxy(_)
                | OpenAiExecutorBuildError::InvalidCertificate(_)
        );
        let report = ReportedFailure::new(
            DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic()),
            source,
        );
        if configuration {
            Self::ConfigurationOrInput(Box::new(report))
        } else {
            Self::Internal(Box::new(report))
        }
    }

    fn cpu_start(source: CpuExecutorStartError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    fn pem_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn lua_script_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn project_lua_worker(source: tokio::task::JoinError) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    fn manual_worker(source: tokio::task::JoinError) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    fn project_lua_preflight(source: ProjectLuaFailure, database_path: &Path) -> Self {
        let class = match &source {
            ProjectLuaFailure::Context(_) | ProjectLuaFailure::Panicked => 1,
            _ => 2,
        };
        let report = source.preflight_diagnostic_report(database_path);
        let reported = ReportedFailure::new(report, ProjectLuaPreflightError(source));
        match class {
            0 => Self::ProjectState(Box::new(reported)),
            1 => Self::Internal(Box::new(reported)),
            _ => Self::ConfigurationOrInput(Box::new(reported)),
        }
    }

    fn project_lua_execution(source: ProjectLuaExecutionError) -> Self {
        let (class, report) = match &source {
            ProjectLuaExecutionError::Open { path, source } => (
                0_u8,
                DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::sqlite(SqliteIssue::new(
                        SqliteDiagnosticContext::new(
                            SqliteDiagnosticStage::Lua,
                            SqliteOperation::Open,
                            SqliteTransactionState::NotStarted,
                        ),
                        SqliteProblem::Driver {
                            database: SafePath::new(path),
                            query_id: None,
                            query_ordinal: None,
                            failure: SqliteDriverFailure::from_error(source),
                        },
                    )),
                ),
            ),
            ProjectLuaExecutionError::Run { path, source } => {
                let class = match source {
                    ProjectLuaRunError::RollbackOutcomeUnknown { .. }
                    | ProjectLuaRunError::SavepointOutcomeUnknown(_) => 1,
                    ProjectLuaRunError::NotStarted(failure)
                    | ProjectLuaRunError::Failed(failure)
                    | ProjectLuaRunError::RolledBack(failure) => match failure {
                        ProjectLuaFailure::Database(_) | ProjectLuaFailure::Cancelled => 0,
                        ProjectLuaFailure::Context(_) | ProjectLuaFailure::Panicked => 3,
                        _ => 2,
                    },
                };
                (class, source.diagnostic_report(path))
            }
        };
        let reported = ReportedFailure::new(report, source);
        match class {
            0 => Self::ProjectState(Box::new(reported)),
            1 => Self::OutcomeUnknown(Box::new(reported)),
            2 => Self::ConfigurationOrInput(Box::new(reported)),
            _ => Self::Internal(Box::new(reported)),
        }
    }

    fn invalid_run_plan(source: InvalidRunPlanValue) -> Self {
        let diagnostic = source.diagnostic_report(RpgMakerDiagnosticStage::CommandPreparation);
        Self::ConfigurationOrInput(Box::new(Self::report_diagnostic(source, diagnostic)))
    }

    fn signal(source: io::Error, outcome: SignalOutcomeSource) -> Self {
        let effect = match &outcome {
            SignalOutcomeSource::CompletedStateApplied => StateEffect::AppliedFinalizationFailed,
            SignalOutcomeSource::Cancelled | SignalOutcomeSource::CommandFailed(_) => {
                StateEffect::Unchanged
            }
        };
        let signal = ReportedFailure::new(
            DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TerminationSignals,
                    operation: RuntimeOperation::ReceiveTerminationSignal,
                    failure: IoFailure::from_error(&source),
                }),
            ),
            source,
        );
        match outcome {
            SignalOutcomeSource::CommandFailed(command) => Self::Signal(Box::new(
                command
                    .into_reported_failure()
                    .with_related(RelatedFailureRelation::Shutdown, signal),
            )),
            SignalOutcomeSource::CompletedStateApplied | SignalOutcomeSource::Cancelled => {
                Self::Signal(Box::new(signal))
            }
        }
    }

    pub(crate) fn failure_report(&self) -> &ReportedFailure {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::RecoveryRequired(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => report.as_ref(),
        }
    }

    pub(crate) fn manual_error(&self) -> Option<&ManualCommandError> {
        let Self::ConfigurationOrInput(report) = self else {
            return None;
        };
        report
            .report()
            .related()
            .is_empty()
            .then(|| report.source_error().downcast_ref::<ManualCommandError>())
            .flatten()
    }

    fn was_cancelled_wait(&self) -> bool {
        let report = self.failure_report();
        report.report().related().is_empty()
            && matches!(
                report.report().primary().issue(),
                DiagnosticIssue::Runtime(RuntimeIssue::Cancelled { .. })
            )
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
        Some(self.failure_report().source_error())
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
        source: impl Error + Send + Sync + 'static,
        report: DiagnosticReport,
    ) {
        self.failures.push(ShutdownFailure {
            component,
            reported: ReportedFailure::new(report, source),
        });
    }

    #[cfg(test)]
    pub(super) fn push_for_test(
        &mut self,
        component: &'static str,
        source: impl Error + Send + Sync + 'static,
    ) {
        let report = DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::Shutdown,
            }),
        );
        self.push(component, source, report);
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    fn diagnostic_reports(&self) -> impl Iterator<Item = &DiagnosticReport> {
        self.failures
            .iter()
            .map(|failure| failure.reported.report())
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
                    let path = public_path(path);
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeInitReusePath { path: &path })
                    )?;
                }
                render_saved_plan_source(localizer, *plan_source, stdout)
            }
            RpgMakerCommandOutput::Extract {
                output,
                plan_source,
                owners,
                run_plan_warnings: _,
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
                if *has_saved_plan {
                    render_saved_plan_source(localizer, *plan_source, stdout)
                } else {
                    Ok(())
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
                let status = if output.summary.is_incomplete() {
                    "incomplete"
                } else if output.summary.total_tasks == 0 {
                    "no_work"
                } else {
                    "complete"
                };
                let status = localizer.format(UiMessage::ResultTranslateStatusValue { status });
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateStatus { status: &status })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateSummary {
                        total: usize_to_u64(output.summary.total_tasks, "任务总数"),
                        started: usize_to_u64(output.summary.started_tasks, "已开始任务数"),
                        not_started: usize_to_u64(
                            output.summary.not_started_tasks,
                            "未开始任务数",
                        ),
                        complete: usize_to_u64(output.summary.complete_tasks, "完整任务数"),
                        partial: usize_to_u64(output.summary.partial_tasks, "部分任务数"),
                        unavailable: usize_to_u64(
                            output.summary.unavailable_tasks,
                            "不可用任务数",
                        ),
                        failed: 0,
                        cancelled: 0,
                        written: usize_to_u64(output.summary.written_locations, "已写位置数"),
                        remaining: usize_to_u64(
                            output.summary.remaining_locations,
                            "剩余位置数",
                        ),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateConvergence {
                        retained: usize_to_u64(output.summary.retained, "保留决策数"),
                        invalidated: usize_to_u64(output.summary.invalidated, "失效决策数"),
                        not_applicable: usize_to_u64(
                            output.summary.not_applicable,
                            "不适用决策数",
                        ),
                        reused: usize_to_u64(output.summary.reused, "复用决策数"),
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
                let output_root = public_path(&output.output_root);
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
                    localizer.format(UiMessage::ResultOutputDirectory { path: &output_root })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultWriteBackSummary {
                        translated: usize_to_u64(output.summary.translated_units, "已翻译 Unit 数"),
                        original: usize_to_u64(output.summary.original_units, "保留原文 Unit 数"),
                    })
                )?;
                Ok(())
            }
            RpgMakerCommandOutput::Manual { summary } => {
                render_manual_command_summary(summary, localizer, stdout)
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
        match output {
            RpgMakerCommandOutput::Extract {
                output,
                run_plan_warnings,
                ..
            } => {
                for warning in &output.rules_warnings {
                    writeln!(
                        stderr,
                        "{}",
                        localizer.format(UiMessage::DiagnosticWarningHeading)
                    )?;
                    writeln!(
                        stderr,
                        "{}",
                        render_diagnostic_report(&warning.diagnostic_report(), localizer)
                    )?;
                }
                for warning in run_plan_warnings {
                    writeln!(
                        stderr,
                        "{}",
                        localizer.format(UiMessage::DiagnosticWarningHeading)
                    )?;
                    writeln!(stderr, "{}", render_diagnostic_report(warning, localizer))?;
                }
            }
            RpgMakerCommandOutput::Translate { output, .. } if output.summary.is_incomplete() => {
                render_rpg_maker_incomplete_warning(output, localizer, stderr)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn render_failure(
        command_error: Option<&ProductionCommandError>,
        shutdown_error: Option<&ShutdownFailures>,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        let command_renders_its_own_headings = command_error
            .and_then(ProductionCommandError::manual_error)
            .is_some();
        if (command_error.is_some() && !command_renders_its_own_headings)
            || (command_error.is_none() && shutdown_error.is_some())
        {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticErrorHeading)
            )?;
        }
        if let Some(error) = command_error {
            if let Some(manual) = error.manual_error() {
                render_manual_command_error(manual, localizer, stderr)?;
            } else {
                writeln!(
                    stderr,
                    "{}",
                    render_diagnostic_report(error.failure_report().report(), localizer)
                )?;
            }
        }
        if let Some(shutdown) = shutdown_error {
            render_shutdown_failures(shutdown, command_error.is_some(), localizer, stderr)?;
        }
        Ok(())
    }

    pub(crate) fn render_applied_finalization_failure(
        shutdown_error: &ShutdownFailures,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticErrorHeading)
        )?;
        render_shutdown_failures(shutdown_error, false, localizer, stderr)
    }

    /// 进程结果呈现已经形成主错误时，把 shutdown 逐项呈现为相关错误。
    pub(crate) fn render_related_shutdown_failures(
        shutdown_error: &ShutdownFailures,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        render_shutdown_failures(shutdown_error, true, localizer, stderr)
    }
}

fn render_rpg_maker_incomplete_warning(
    output: &TranslateOutput,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let object = localizer.format(UiMessage::TranslateIncompleteObject {
        project: output.name.as_str(),
    });
    let reason = localizer.format(UiMessage::TranslateIncompleteRpgMakerReason {
        partial: usize_to_u64(output.summary.partial_tasks, "部分任务数"),
        unavailable: usize_to_u64(output.summary.unavailable_tasks, "不可用任务数"),
        protocol: usize_to_u64(output.summary.protocol_diagnostics, "协议问题数"),
        exhausted: usize_to_u64(output.summary.recoverable_request_exhaustions, "请求耗尽数"),
        admission: if output.summary.request_admission_stopped {
            "stopped"
        } else {
            "open"
        },
        not_started: usize_to_u64(output.summary.not_started_tasks, "未开始任务数"),
        remaining_decisions: usize_to_u64(output.summary.remaining_decisions, "剩余决策数"),
        remaining_locations: usize_to_u64(output.summary.remaining_locations, "剩余位置数"),
    });
    let impact = render_state_effect_impact(StateEffect::ProgressPreserved, localizer);
    let help = localizer.format(UiMessage::TranslateIncompleteHelp);
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticWarningHeading)
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticObject { subject: &object })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticExplanation { reason: &reason })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticImpact { impact: &impact })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticResolution { action: &help })
    )
}

fn render_shutdown_failures(
    failures: &ShutdownFailures,
    follows_primary: bool,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let mut diagnostics = failures.diagnostic_reports();
    if follows_primary {
        for diagnostic in diagnostics {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: "shutdown",
                })
            )?;
            writeln!(
                stderr,
                "{}",
                render_diagnostic_report(diagnostic, localizer)
            )?;
        }
    } else if let Some(primary) = diagnostics.next() {
        writeln!(stderr, "{}", render_diagnostic_report(primary, localizer))?;
        for diagnostic in diagnostics {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: "shutdown",
                })
            )?;
            writeln!(
                stderr,
                "{}",
                render_diagnostic_report(diagnostic, localizer)
            )?;
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

#[cfg(test)]
mod command_result_renderer_tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct TestFailure;

    impl fmt::Display for TestFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试失败")
        }
    }

    impl Error for TestFailure {}

    fn test_report(effect: StateEffect) -> ReportedFailure {
        ProductionCommandError::report_diagnostic(
            TestFailure,
            DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                    component: RuntimeComponent::Process,
                    operation: RuntimeOperation::Shutdown,
                }),
            ),
        )
    }

    #[derive(Debug)]
    struct TestPublishingFailure {
        effect: StateEffect,
        related_effect: Option<StateEffect>,
    }

    impl fmt::Display for TestPublishingFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试发布失败")
        }
    }

    impl Error for TestPublishingFailure {}

    impl WriteBackPublishingDiagnostic for TestPublishingFailure {
        fn into_write_back_failure_report(self) -> ReportedFailure {
            let Self {
                effect,
                related_effect,
            } = self;
            let report = ReportedFailure::new(
                test_report(effect).into_report(),
                TestPublishingFailure {
                    effect,
                    related_effect,
                },
            );
            match related_effect {
                Some(related) => {
                    report.with_related(RelatedFailureRelation::Cleanup, test_report(related))
                }
                None => report,
            }
        }
    }

    #[derive(Debug)]
    struct TestPreparationFailure;

    impl fmt::Display for TestPreparationFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试写回准备失败")
        }
    }

    impl Error for TestPreparationFailure {}

    impl ProductionWriteBackPreparationFailure for TestPreparationFailure {
        fn into_write_back_preparation_failure(self) -> ReportedFailure {
            ReportedFailure::new(test_report(StateEffect::Unchanged).into_report(), self)
        }
    }

    fn report_tree_contains(
        report: &DiagnosticReport,
        predicate: &impl Fn(&DiagnosticReport) -> bool,
    ) -> bool {
        predicate(report)
            || report
                .related()
                .iter()
                .any(|related| report_tree_contains(related.report(), predicate))
    }

    fn manual_read_failure() -> ManualCommandError {
        ManualCommandError::Document(crate::manual::ManualDocumentError::Read {
            path: PathBuf::from("C:/project/manual.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "测试读取失败"),
        })
    }

    #[test]
    fn only_direct_manual_failure_uses_detailed_manual_renderer() {
        let direct = ProductionCommandError::manual(manual_read_failure());
        assert!(direct.manual_error().is_some());

        let signal = ProductionCommandError::signal(
            io::Error::other("测试信号失败"),
            SignalOutcomeSource::CommandFailed(ProductionCommandError::manual(
                manual_read_failure(),
            )),
        );
        assert!(
            signal.manual_error().is_none(),
            "Signal 外层的类型化主错误和 related 不能被递归 Manual 呈现替换"
        );
        assert_eq!(signal.failure_report().report().related().len(), 1);
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(&signal), None, &localizer, &mut stderr)
            .expect("Signal 外层诊断应可呈现");
        let stderr = String::from_utf8(stderr).expect("诊断必须是 UTF-8");
        assert!(
            stderr.contains(&localizer.format(UiMessage::DiagnosticRelated {
                relation: RelatedFailureRelation::Shutdown.as_str(),
            }))
        );

        let finalization = ProductionCommandError::manual(manual_read_failure())
            .with_related_finalization_report(test_report(StateEffect::AppliedFinalizationFailed));
        assert!(
            finalization.manual_error().is_none(),
            "Finalization 外层必须保留类型化相关报告"
        );
        assert_eq!(finalization.failure_report().report().related().len(), 1);
    }

    #[test]
    fn recovery_required_is_not_collapsed_into_state_applied_finalization_failure() {
        let report = test_report(StateEffect::Unchanged).with_related(
            RelatedFailureRelation::Cleanup,
            test_report(StateEffect::RecoveryRequired),
        );
        let error = map_project_failure_report(report);
        assert!(matches!(error, ProductionCommandError::RecoveryRequired(_)));

        assert_eq!(
            error.failure_report().report().effect(),
            StateEffect::RecoveryRequired
        );
    }

    #[test]
    fn outcome_unknown_has_priority_over_recovery_required() {
        let report = test_report(StateEffect::RecoveryRequired).with_related(
            RelatedFailureRelation::Finalization,
            test_report(StateEffect::OutcomeUnknown),
        );
        assert!(matches!(
            map_project_failure_report(report),
            ProductionCommandError::OutcomeUnknown(_)
        ));
    }

    #[test]
    fn init_prepare_preserves_recovery_and_unknown_source_impacts() {
        for (source, expected) in [
            (
                SystemFileSystemError::JournalCorrupt {
                    path: PathBuf::from("C:/project/.directory-publish/workspace/journal"),
                    violation: crate::diagnostic::FileSystemJournalViolation::CrcMismatch {
                        frame_index: 1,
                    },
                },
                StateEffect::RecoveryRequired,
            ),
            (
                SystemFileSystemError::OutcomeUnknown {
                    target_root: PathBuf::from("C:/project/workspace"),
                    artifacts: vec![PathBuf::from(
                        "C:/project/.directory-publish/workspace/journal",
                    )],
                    violation:
                        crate::diagnostic::FileSystemRecoveryViolation::TargetIdentityUnknown,
                },
                StateEffect::OutcomeUnknown,
            ),
        ] {
            let mapped = map_init_error(InitServiceError::Workspace(
                ProjectWorkspaceConvergenceError::Prepare(DirectoryPrepareError::NotPrepared {
                    target_root: PathBuf::from("C:/project/workspace"),
                    source: Box::new(source),
                    cleanup_failure: None,
                }),
            ));
            assert_eq!(mapped.failure_report().report().effect(), expected);
            assert!(matches!(
                (expected, mapped),
                (
                    StateEffect::RecoveryRequired,
                    ProductionCommandError::RecoveryRequired(_)
                ) | (
                    StateEffect::OutcomeUnknown,
                    ProductionCommandError::OutcomeUnknown(_)
                )
            ));
        }
    }

    #[test]
    fn init_explicit_recovery_preserves_recovery_and_unknown_impacts() {
        for (source, expected) in [
            (
                SystemFileSystemError::JournalCorrupt {
                    path: PathBuf::from("C:/project/.directory-publish/workspace/journal"),
                    violation: crate::diagnostic::FileSystemJournalViolation::CrcMismatch {
                        frame_index: 1,
                    },
                },
                StateEffect::RecoveryRequired,
            ),
            (
                SystemFileSystemError::OutcomeUnknown {
                    target_root: PathBuf::from("C:/project/workspace"),
                    artifacts: vec![PathBuf::from(
                        "C:/project/.directory-publish/workspace/journal",
                    )],
                    violation:
                        crate::diagnostic::FileSystemRecoveryViolation::TargetIdentityUnknown,
                },
                StateEffect::OutcomeUnknown,
            ),
        ] {
            let workspace_error: ProductionWorkspaceConvergenceError =
                ProjectWorkspaceConvergenceError::Recover(DirectoryRecoveryError::new(
                    PathBuf::from("C:/project/workspace"),
                    Box::new(source),
                ));
            let mapped = map_init_error(InitServiceError::Workspace(workspace_error));
            assert_eq!(mapped.failure_report().report().effect(), expected);
            assert!(matches!(
                (expected, mapped),
                (
                    StateEffect::RecoveryRequired,
                    ProductionCommandError::RecoveryRequired(_)
                ) | (
                    StateEffect::OutcomeUnknown,
                    ProductionCommandError::OutcomeUnknown(_)
                )
            ));
        }
    }

    #[test]
    fn write_back_preparation_and_discard_preserve_strongest_impact() {
        type Error = WriteBackServiceError<
            SystemFileSystemError,
            TestPreparationFailure,
            TestPublishingFailure,
            SystemFileSystemError,
        >;

        let prepared = map_write_back_error(Error::PrepareCandidate(TestPublishingFailure {
            effect: StateEffect::OutcomeUnknown,
            related_effect: None,
        }));
        assert!(matches!(
            prepared,
            ProductionCommandError::OutcomeUnknown(_)
        ));

        let discarded = map_write_back_error(Error::ValidateCandidateAndDiscard {
            candidate_root: PathBuf::from("C:/project/candidate"),
            source: TestPublishingFailure {
                effect: StateEffect::Unchanged,
                related_effect: None,
            },
            discard: TestPublishingFailure {
                effect: StateEffect::RecoveryRequired,
                related_effect: None,
            },
        });
        assert!(matches!(
            discarded,
            ProductionCommandError::RecoveryRequired(_)
        ));

        let publish_cleanup = map_write_back_error(Error::Publish {
            state: WriteBackPublishFailureState::NotPublished {
                output_root: PathBuf::from("C:/project/write_back"),
                residual_paths: vec![PathBuf::from("C:/project/.write-back-residual")],
            },
            source: TestPublishingFailure {
                effect: StateEffect::Unchanged,
                related_effect: Some(StateEffect::RecoveryRequired),
            },
        });
        assert!(matches!(
            publish_cleanup,
            ProductionCommandError::RecoveryRequired(_)
        ));
    }

    fn assert_init_recovery_required(error: ProductionWorkspaceConvergenceError) {
        let mapped = map_init_error(InitServiceError::Workspace(error));
        let ProductionCommandError::RecoveryRequired(report) = mapped else {
            panic!("Init 清理失败必须映射为 RecoveryRequired");
        };
        assert_eq!(report.report().effect(), StateEffect::RecoveryRequired);
    }

    #[test]
    fn init_cancellation_cleanup_failure_requires_recovery() {
        assert_init_recovery_required(ProjectWorkspaceConvergenceError::CancellationCleanup(
            DirectoryDiscardError::new(
                PathBuf::from("C:/project/.init-candidate"),
                Box::new(SystemFileSystemError::Closed),
            ),
        ));
    }

    #[test]
    fn init_prepare_cleanup_failure_preserves_related_recovery_diagnostic() {
        assert_init_recovery_required(ProjectWorkspaceConvergenceError::Prepare(
            DirectoryPrepareError::NotPrepared {
                target_root: PathBuf::from("C:/project/workspace"),
                source: Box::new(SystemFileSystemError::Closed),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("C:/project/.prepare-residual"),
                    Box::new(SystemFileSystemError::Closed),
                )),
            },
        ));
    }

    #[test]
    fn init_publish_cleanup_failure_preserves_publication_recovery_diagnostic() {
        let mapped = map_init_error(InitServiceError::Workspace(
            ProjectWorkspaceConvergenceError::Publish(DirectoryPublishError::NotPublished {
                target_root: PathBuf::from("C:/project/workspace"),
                source: Box::new(SystemFileSystemError::Closed),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("C:/project/.publish-residual"),
                    Box::new(SystemFileSystemError::Closed),
                )),
            }),
        ));
        let ProductionCommandError::RecoveryRequired(report) = mapped else {
            panic!("Init 发布清理失败必须映射为 RecoveryRequired");
        };
        assert!(report_tree_contains(report.report(), &|diagnostic| {
            diagnostic.primary().stage() == DiagnosticStage::Publication
                && diagnostic.effect() == StateEffect::RecoveryRequired
        }));
    }
}

#[cfg(test)]
mod init_entry_recovery_tests {
    use std::fs;

    use super::*;
    use crate::application::arguments::{InitArguments, ProjectArguments};
    use crate::language::LanguageId;
    use crate::runtime::filesystem::{
        DirectoryPublisherConfig, SystemFileSystemConfig, TestPublishFaultAction,
        TestPublishFaultPoint, register_test_publish_faults,
    };
    use crate::storage::file_system::{
        DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
        RecoverableDirectoryPublisher,
    };

    fn init_command(
        projects_root: &Path,
        game_root: Option<PathBuf>,
        include_settings: bool,
    ) -> ConfiguredInitCommand {
        ConfiguredInitCommand::for_test(
            InitArguments {
                project: ProjectArguments {
                    name: "entry-recovery".parse().expect("测试项目名应有效"),
                },
                path: game_root,
                source_language: include_settings
                    .then(|| "ja".parse::<LanguageId>().expect("测试原文语言应有效")),
                target_language: include_settings
                    .then(|| "zh-Hans".parse::<LanguageId>().expect("测试译文语言应有效")),
            },
            projects_root,
            "mz",
        )
    }

    fn write_minimal_mz_game(game_root: &Path) {
        fs::create_dir_all(game_root.join("data")).expect("测试 data 目录应可建立");
        fs::create_dir_all(game_root.join("js")).expect("测试 js 目录应可建立");
        fs::write(game_root.join("data/System.json"), b"{}").expect("测试数据文件应可写入");
        fs::write(game_root.join("js/rmmz_core.js"), b"/* MZ */").expect("测试核心脚本应可写入");
    }

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir(target).expect("候选根应可建立");
        for entry in fs::read_dir(source).expect("项目工作区应可读取") {
            let entry = entry.expect("项目工作区目录项应可读取");
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());
            if entry
                .file_type()
                .expect("项目工作区目录项类型应可读取")
                .is_dir()
            {
                copy_tree(&source_path, &target_path);
            } else {
                fs::copy(&source_path, &target_path).expect("项目工作区文件应可复制");
            }
        }
    }

    fn finish_successful_report(mut report: ProductionCommandRunReport) {
        match &report.result {
            CommandRunResult::Succeeded(_) => {}
            CommandRunResult::Interrupted => panic!("Init 不应取消"),
            CommandRunResult::Failed(error) => panic!("Init 应成功，实际为 {error}"),
        }
        assert!(report.shutdown_error.is_none());
        if let Some(project_log) = report.pending_project_log.take() {
            assert!(project_log.finish().is_none());
        }
    }

    #[tokio::test]
    async fn init_recovers_missing_workspace_before_reusing_omitted_path() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let projects_root = temporary.path().join("projects");
        let game_root = temporary.path().join("game");
        fs::create_dir(&projects_root).expect("项目根应可建立");
        write_minimal_mz_game(&game_root);

        let mut signals = TerminationSignals::new();
        let first =
            ProductionRpgMakerCommandRunner::new(RpgMakerLayout::MZ, UiLocale::SimplifiedChinese)
                .run_init(
                    init_command(&projects_root, Some(game_root.clone()), true),
                    &mut signals,
                )
                .await;
        finish_successful_report(first);

        let workspace = projects_root.join("mz/entry-recovery");
        let replacement = temporary.path().join("replacement");
        copy_tree(&workspace, &replacement);
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("测试文件系统根应可建立");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(
                projects_root.join(".att-locks/directory-publish/mz"),
            )
            .expect("测试发布配置应有效"),
        );
        let staged = publisher
            .prepare(
                DirectoryStageRequest::new(
                    workspace.clone(),
                    DirectoryPublishIntent::ReplaceExisting,
                    vec![
                        DirectorySourceMapping::new(replacement, PathBuf::new())
                            .expect("测试来源映射应有效"),
                    ],
                    Vec::new(),
                    Vec::new(),
                )
                .expect("测试候选请求应有效"),
            )
            .await
            .expect("测试替换候选应可准备");
        let canonical_workspace = workspace
            .parent()
            .expect("工作区应有父目录")
            .canonicalize()
            .expect("工作区父目录应可规范化")
            .join(workspace.file_name().expect("工作区应有名称"));
        register_test_publish_faults(
            canonical_workspace,
            [(
                TestPublishFaultPoint::BeforeBackupCleanup,
                TestPublishFaultAction::Error,
            )],
        );
        assert!(matches!(
            publisher.publish(staged).await,
            Err(DirectoryPublishError::PublishedWithResiduals { .. })
        ));
        file_system
            .shutdown()
            .await
            .expect("测试文件系统根应可关闭");

        fs::remove_dir_all(&workspace).expect("测试应可移除已发布目标以模拟中断现场");

        let mut signals = TerminationSignals::new();
        let resumed =
            ProductionRpgMakerCommandRunner::new(RpgMakerLayout::MZ, UiLocale::SimplifiedChinese)
                .run_init(init_command(&projects_root, None, false), &mut signals)
                .await;
        finish_successful_report(resumed);

        assert!(workspace.join("project.db").is_file());
        let publication_workspace = workspace
            .parent()
            .expect("工作区应有父目录")
            .join(".directory-publish")
            .join(workspace.file_name().expect("工作区应有名称"));
        assert_eq!(
            fs::read_dir(publication_workspace)
                .expect("目标目录发布工作目录应可读取")
                .count(),
            0,
            "恢复后不得留下 stage、backup 或 journal"
        );
    }
}
