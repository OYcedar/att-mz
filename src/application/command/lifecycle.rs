//! RPG Maker 命令运行根、取消、panic 与最终报告生命周期。

use super::RpgMakerCommandOutput;
use super::error::{ProductionCommandError, SignalOutcomeSource};
use crate::application::TranslationTerminalSummary;
use crate::application::config::ConfiguredRpgMakerCommand;
use crate::application::project_log::{ActiveProjectLog, PendingProjectLog};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RelatedFailureRelation, ReportedFailure,
    RuntimeComponent, RuntimeEngine, RuntimeIssue, RuntimeOperation, RuntimePanicBoundary,
    SafePath, StateEffect,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::llm::ApiKeyRedactor;
use crate::project_lease::{ProjectCommandLease, ProjectCommandLeaseProvider};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::init::ProjectWorkspaceConvergenceError;
use crate::rpg_maker::project::{
    ExistingProjectOpener, ExistingProjectOpeningError, ExistingProjectOpeningService,
    OpenedProject,
};
use crate::rpg_maker::project_database::{
    ProjectDatabaseCreateError, ProjectDatabaseInspectionError, ProjectDatabaseReadError,
    ProjectDatabaseReconciliationError, ProjectDatabaseRecordReadingService,
};
#[cfg(test)]
use crate::runtime::cpu::CpuExecutorStartError;
use crate::runtime::cpu::{CpuExecutorConfig, CpuExecutorShutdownError, RayonCpuExecutor};
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::sqlite::{RusqliteStorage, RusqliteStorageConfiguration, SqliteRuntimeError};
use futures_util::FutureExt;
use std::error::Error;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fmt, io};

pub(super) type ProductionProjectOpeningError = ExistingProjectOpeningError<
    ProjectDatabaseReadError<SqliteRuntimeError>,
    SystemFileSystemError,
    Box<SystemFileSystemError>,
    SystemFileSystemError,
>;

pub(super) type ProductionWorkspaceConvergenceError = ProjectWorkspaceConvergenceError<
    ProjectDatabaseCreateError<SqliteRuntimeError>,
    SqliteRuntimeError,
    ProjectDatabaseInspectionError<SqliteRuntimeError>,
    ProjectDatabaseReconciliationError<SqliteRuntimeError, SqliteRuntimeError>,
    SystemFileSystemError,
    Box<SystemFileSystemError>,
    Box<SystemFileSystemError>,
>;
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

    pub(super) fn prepare(
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

    pub(super) fn observe_selected_api_key_redactor(&self, redactor: Arc<ApiKeyRedactor>) {
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

    pub(super) fn selected_api_key_redactor(&self) -> Option<Arc<ApiKeyRedactor>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|context| context.selected_api_key_redactor.clone())
    }

    pub(super) fn observe_project_log(&self, project_log: &ActiveProjectLog) {
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

    pub(super) fn panic_log_path(&self) -> Option<PathBuf> {
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

pub(super) trait CommandRootShutdown: Send + Sync {
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
pub(super) struct CommandRootGuard<C, F, S> {
    cpu: Option<C>,
    file_system: Option<F>,
    sqlite: Option<S>,
}

impl<C, F, S> CommandRootGuard<C, F, S> {
    pub(super) const fn empty() -> Self {
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
    pub(super) async fn shutdown(mut self) -> ShutdownFailures {
        let mut failures = ShutdownFailures::default();
        shutdown_command_root(self.sqlite.take(), &mut failures).await;
        shutdown_command_root(self.file_system.take(), &mut failures).await;
        shutdown_command_root(self.cpu.take(), &mut failures).await;
        failures
    }
}

pub(super) async fn shutdown_command_root<R>(root: Option<R>, failures: &mut ShutdownFailures)
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

pub(super) type ProductionCommandRootGuard =
    CommandRootGuard<RayonCpuExecutor, SystemFileSystem, RusqliteStorage>;

impl ProductionCommandRootGuard {
    pub(super) async fn start_main(
        cpu_configuration: CpuExecutorConfig,
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
        roots.file_system = match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
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

    pub(super) async fn start_init(
        sqlite_configuration: RusqliteStorageConfiguration,
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<Self, CommandRootStartupFailure> {
        let mut roots = Self::empty();
        roots.file_system = match SystemFileSystem::new_with_performance(Arc::clone(&performance)) {
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

    pub(super) fn cpu(&self) -> &RayonCpuExecutor {
        self.cpu.as_ref().expect("主命令必须已启动 CPU 根")
    }

    pub(super) fn file_system(&self) -> &SystemFileSystem {
        self.file_system
            .as_ref()
            .expect("命令必须已启动 FileSystem 根")
    }

    pub(super) fn sqlite(&self) -> &RusqliteStorage {
        self.sqlite.as_ref().expect("命令必须已启动 SQLite 根")
    }
}

pub(super) struct CommandRootStartupFailure {
    primary: ProductionCommandError,
    shutdown: ShutdownFailures,
}

impl CommandRootStartupFailure {
    pub(super) const fn new(primary: ProductionCommandError, shutdown: ShutdownFailures) -> Self {
        Self { primary, shutdown }
    }

    pub(super) fn into_report(self) -> ProductionCommandRunReport {
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
pub(super) async fn catch_command_panic(
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

pub(super) async fn catch_translate_execution_panic<T>(
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

pub(super) fn command_panic_context(
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

pub(super) fn report_with_shutdown(
    mut report: DiagnosticReport,
    shutdown: &ShutdownFailures,
) -> DiagnosticReport {
    for related in shutdown.diagnostic_reports() {
        report = report.with_related(RelatedFailureRelation::Shutdown, related.clone());
    }
    report
}

pub(super) fn shutdown_report(shutdown: &ShutdownFailures) -> Option<DiagnosticReport> {
    let mut reports = shutdown.diagnostic_reports();
    let mut primary = reports.next()?.clone();
    for related in reports {
        primary = primary.with_related(RelatedFailureRelation::Shutdown, related.clone());
    }
    Some(primary)
}

pub(super) fn signal_report(source: &io::Error, effect: StateEffect) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::runtime(RuntimeIssue::Io {
            component: RuntimeComponent::Process,
            operation: RuntimeOperation::ReceiveTerminationSignal,
            failure: IoFailure::from_error(source),
        }),
    )
}

pub(super) async fn observed_construction_failure(
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

pub(super) async fn drive_project_lease<P>(
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
    drive_with_termination(
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

pub(super) struct ProjectOpeningLocation {
    pub(super) projects_root: PathBuf,
    pub(super) layout: RpgMakerLayout,
}

pub(super) async fn drive_existing_project_opening(
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
    drive_with_termination(
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

pub(super) fn interrupted_non_cancellation_error<T>(
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
    pub(super) fn panicked(error: ProductionCommandError, panic_log_path: Option<PathBuf>) -> Self {
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

    pub(super) fn failed_before_logging(error: ProductionCommandError) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    pub(super) fn failed_before_logging_with_shutdown(
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

    pub(super) fn interrupted_before_logging(shutdown: ShutdownFailures) -> Self {
        Self {
            result: CommandRunResult::Interrupted,
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    pub(super) fn construction_failed_with_shutdown_and_project_log(
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

    pub(super) fn from_completion_with_project_log(
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

    pub(super) fn with_translation_summary(
        mut self,
        summary: Option<TranslationTerminalSummary>,
    ) -> Self {
        self.translation_summary = summary;
        self
    }
}

pub(super) fn map_completion<T, U>(
    completion: OperationCompletion<T>,
    map: impl FnOnce(T) -> U,
) -> OperationCompletion<U> {
    match completion {
        OperationCompletion::Completed(value) => OperationCompletion::Completed(map(value)),
        OperationCompletion::Cancelled => OperationCompletion::Cancelled,
    }
}

#[derive(Default)]
pub(crate) struct ShutdownFailures {
    failures: Vec<ShutdownFailure>,
}

impl ShutdownFailures {
    pub(super) fn push(
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
    pub(crate) fn push_for_test(
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

    pub(super) fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    pub(super) fn diagnostic_reports(&self) -> impl Iterator<Item = &DiagnosticReport> {
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
