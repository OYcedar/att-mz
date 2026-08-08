//! 应用层唯一的项目日志组合与生命周期入口。
//!
//! 领域调用方只使用 [`ProjectLogHandle`] 提交封闭事件和诊断；底层 logger 的契约错误、
//! writer 健康故障和 stderr 呈现失败都在此处转换成类型化 Observability 报告。日志无法
//! 建立时业务仍可继续，但 `ActiveProjectLog` 不会伪造一个可写 runtime。

use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, ObservabilityComponent, ObservabilityContractViolation,
    ObservabilityIssue, RelatedFailureRelation, RuntimeCommand, RuntimeEngine, RuntimeIssue,
    SafePath, StateEffect, render_diagnostic_report,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::project_lua::{ProjectLuaCallError, ProjectLuaPrintSink};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticOccurrenceId, DiagnosticScope, EmitDisposition, PreparedTerminalDiagnostic,
    ProjectLogCommand, ProjectLogContext, ProjectLogEngine, ProjectLogEvent,
    ProjectLogHealthCursor, ProjectLogHealthSnapshot, ProjectLogRuntime, ProjectLogger,
    RunFinished, TranslationTaskCounters,
};
use crate::runtime::run_id::reserve_run_log;
use crate::translation::task_record::TaskRecordDiagnosticRecorder;

use super::config::CommonCommandConfiguration;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLogWarning {
    pub(crate) log_path: Option<PathBuf>,
    pub(crate) project_log: Vec<DiagnosticReport>,
    pub(crate) task_records: Vec<DiagnosticReport>,
    pub(crate) presentation_failures: Vec<DiagnosticReport>,
}

impl ProjectLogWarning {
    pub(crate) fn is_empty(&self) -> bool {
        self.project_log.is_empty()
            && self.task_records.is_empty()
            && self.presentation_failures.is_empty()
    }
}

#[derive(Clone, Copy)]
enum WarningCategory {
    ProjectLog,
    TaskRecord,
}

struct ProjectLogWarningState {
    locale: UiLocale,
    log_path: Option<PathBuf>,
    project_log: Mutex<Vec<DiagnosticReport>>,
    task_records: Mutex<Vec<DiagnosticReport>>,
    presentation_failures: Mutex<Vec<DiagnosticReport>>,
    presenter: Mutex<Option<Sender<WarningPresentation>>>,
    health_cursor: Mutex<ProjectLogHealthCursor>,
}

#[derive(Clone)]
struct WarningPresentation {
    category: WarningCategory,
    report: DiagnosticReport,
}

struct ProjectLogWarningPresenter {
    state: Arc<ProjectLogWarningState>,
    worker: Option<JoinHandle<()>>,
}

impl ProjectLogWarningState {
    fn new(locale: UiLocale, log_path: Option<PathBuf>) -> Self {
        Self {
            locale,
            log_path,
            project_log: Mutex::new(Vec::new()),
            task_records: Mutex::new(Vec::new()),
            presentation_failures: Mutex::new(Vec::new()),
            presenter: Mutex::new(None),
            health_cursor: Mutex::new(ProjectLogHealthCursor::default()),
        }
    }

    fn record(&self, category: WarningCategory, report: DiagnosticReport) {
        match category {
            WarningCategory::ProjectLog => lock_unpoisoned(&self.project_log).push(report.clone()),
            WarningCategory::TaskRecord => lock_unpoisoned(&self.task_records).push(report.clone()),
        }
        // 任务记录和日志建立阶段的故障由命令终态统一呈现一次。只有 writer 健康
        // cursor 观察到的新故障走即时 stderr，避免同一诊断在终态前后重复出现。
    }

    /// 同一 cursor 同时服务即时呈现和终态汇总，已经可靠呈现的 writer 故障不会重复输出。
    fn present_health_snapshot(&self, health: &ProjectLogHealthSnapshot) {
        // cursor 与 fallback 列表使用固定的 cursor -> project_log 锁顺序。必须持有 cursor
        // 直到 fallback 入列完成，避免终态在“已消费但尚未保存”之间错过故障。
        let mut cursor = lock_unpoisoned(&self.health_cursor);
        let fresh = cursor.consume(health);
        for failure in fresh {
            let report = failure.diagnostic_report();
            if !self.enqueue_presentation(WarningCategory::ProjectLog, report.clone()) {
                self.record(WarningCategory::ProjectLog, report);
            }
        }
    }

    /// 生产者只向无界内存通道提交呈现请求，不在业务线程执行 stderr I/O。
    fn enqueue_presentation(&self, category: WarningCategory, report: DiagnosticReport) -> bool {
        let sender = lock_unpoisoned(&self.presenter).clone();
        let Some(sender) = sender else {
            return false;
        };
        if sender
            .send(WarningPresentation { category, report })
            .is_err()
        {
            self.record_presentation_failure(DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::channel(
                    ObservabilityComponent::Stderr,
                    None,
                    1,
                )),
            ));
            return false;
        }
        true
    }

    fn present(&self, category: WarningCategory, report: &DiagnosticReport) {
        let localizer = UiLocalizer::new(self.locale);
        let banner = match category {
            WarningCategory::ProjectLog => localizer.format(UiMessage::NoticeLogDegraded),
            WarningCategory::TaskRecord => localizer.format(UiMessage::NoticeTaskRecordsDegraded),
        };
        let rendered = render_diagnostic_report(report, &localizer);
        let mut stderr = io::stderr().lock();
        let write_result = (|| -> io::Result<()> {
            writeln!(stderr, "{banner}")?;
            writeln!(stderr, "{rendered}")
        })();
        let mut failed = false;
        if let Err(source) = write_result {
            failed = true;
            self.record_presentation_failure(DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::write(
                    ObservabilityComponent::Stderr,
                    None,
                    None,
                    1,
                    &source,
                )),
            ));
        }
        if let Err(source) = stderr.flush() {
            failed = true;
            self.record_presentation_failure(DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::flush(
                    ObservabilityComponent::Stderr,
                    None,
                    &source,
                )),
            ));
        }
        if failed {
            // 即时呈现没有可靠完成时，终态再呈现原始故障，不能只留下 stderr 自身的错误。
            self.record(category, report.clone());
        }
    }

    fn record_presentation_failure(&self, report: DiagnosticReport) {
        lock_unpoisoned(&self.presentation_failures).push(report);
    }

    fn presentation_failures(&self) -> Vec<DiagnosticReport> {
        lock_unpoisoned(&self.presentation_failures).clone()
    }

    fn install_presenter(&self, sender: Sender<WarningPresentation>) {
        *lock_unpoisoned(&self.presenter) = Some(sender);
    }

    fn close_presenter(&self) {
        lock_unpoisoned(&self.presenter).take();
    }

    fn warning(&self, health: &ProjectLogHealthSnapshot) -> Option<ProjectLogWarning> {
        let mut cursor = lock_unpoisoned(&self.health_cursor);
        let fresh = cursor.consume(health);
        let mut project_log = lock_unpoisoned(&self.project_log).clone();
        project_log.extend(fresh.into_iter().map(|failure| failure.diagnostic_report()));
        let warning = ProjectLogWarning {
            log_path: self.log_path.clone(),
            project_log,
            task_records: lock_unpoisoned(&self.task_records).clone(),
            presentation_failures: lock_unpoisoned(&self.presentation_failures).clone(),
        };
        (!warning.is_empty()).then_some(warning)
    }
}

impl ProjectLogWarningPresenter {
    fn start(state: &Arc<ProjectLogWarningState>) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<WarningPresentation>();
        let weak_state: Weak<ProjectLogWarningState> = Arc::downgrade(state);
        let worker = thread::Builder::new()
            .name("att-project-log-warning-presenter".to_owned())
            .spawn(move || {
                while let Ok(presentation) = receiver.recv() {
                    let Some(state) = weak_state.upgrade() else {
                        break;
                    };
                    if catch_unwind(AssertUnwindSafe(|| {
                        state.present(presentation.category, &presentation.report);
                    }))
                    .is_err()
                    {
                        // 当前报告及已经排队的报告都已被 health cursor 消费。worker panic
                        // 后必须转存到终态列表，再关闭所有发送者，不能只报告线程自身失败。
                        state.record(presentation.category, presentation.report);
                        state.record_presentation_failure(DiagnosticReport::new(
                            StateEffect::Unchanged,
                            Diagnostic::observability(ObservabilityIssue::worker(
                                ObservabilityComponent::Stderr,
                                1,
                            )),
                        ));
                        state.close_presenter();
                        for pending in receiver {
                            state.record(pending.category, pending.report);
                        }
                        break;
                    }
                }
            })?;
        state.install_presenter(sender);
        Ok(Self {
            state: Arc::clone(state),
            worker: Some(worker),
        })
    }

    fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.state.close_presenter();
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            self.state
                .record_presentation_failure(DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::observability(ObservabilityIssue::worker(
                        ObservabilityComponent::Stderr,
                        1,
                    )),
                ));
        }
    }
}

impl Drop for ProjectLogWarningPresenter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create_warning_state(
    locale: UiLocale,
    log_path: Option<PathBuf>,
) -> (
    Arc<ProjectLogWarningState>,
    Option<ProjectLogWarningPresenter>,
) {
    let state = Arc::new(ProjectLogWarningState::new(locale, log_path));
    match ProjectLogWarningPresenter::start(&state) {
        Ok(presenter) => (state, Some(presenter)),
        Err(source) => {
            state.record_presentation_failure(DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::worker_start(
                    ObservabilityComponent::Stderr,
                    &source,
                )),
            ));
            (state, None)
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProjectLogHandle {
    logger: Option<ProjectLogger>,
    warnings: Arc<ProjectLogWarningState>,
}

impl ProjectLogHandle {
    pub(crate) fn emit(&self, event: ProjectLogEvent) -> Option<EmitDisposition> {
        let logger = self.logger.as_ref()?;
        match logger.emit(event) {
            Ok(disposition) => Some(disposition),
            Err(error) => {
                self.record_contract_failure(error.diagnostic_report());
                None
            }
        }
    }

    pub(crate) fn record_diagnostic(
        &self,
        scope: DiagnosticScope,
        report: DiagnosticReport,
    ) -> Option<DiagnosticOccurrenceId> {
        let logger = self.logger.as_ref()?;
        match logger.record_diagnostic(scope, report) {
            Ok(id) => Some(id),
            Err(error) => {
                self.record_contract_failure(error.diagnostic_report());
                None
            }
        }
    }

    pub(crate) fn prepare_terminal_diagnostic(
        &self,
        scope: DiagnosticScope,
        report: DiagnosticReport,
    ) -> Option<PreparedTerminalDiagnostic> {
        let logger = self.logger.as_ref()?;
        match logger.prepare_terminal_diagnostic(scope, report) {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                self.record_contract_failure(error.diagnostic_report());
                None
            }
        }
    }

    pub(crate) fn health(&self) -> ProjectLogHealthSnapshot {
        self.logger
            .as_ref()
            .map_or_else(ProjectLogHealthSnapshot::default, ProjectLogger::health)
    }

    /// 使用 logger 已接受的 Task 事件建立 Translate 终态，避免应用层旁路计数因一次
    /// 诊断写入失败而让唯一 translation.finished 缺失。
    pub(crate) fn translation_task_counters(
        &self,
        planned: u64,
    ) -> Option<TranslationTaskCounters> {
        let logger = self.logger.as_ref()?;
        match logger.translation_task_counters(planned) {
            Ok(counters) => Some(counters),
            Err(error) => {
                self.record_contract_failure(error.diagnostic_report());
                None
            }
        }
    }

    fn record_contract_failure(&self, report: DiagnosticReport) {
        if let Some(logger) = &self.logger
            && let Err(related) =
                logger.record_diagnostic(DiagnosticScope::ProjectLog, report.clone())
        {
            self.warnings
                .record(WarningCategory::ProjectLog, related.diagnostic_report());
        }
        self.warnings.record(WarningCategory::ProjectLog, report);
    }

    fn record_task_record_failure(&self, report: DiagnosticReport) {
        let _ = self.record_diagnostic(DiagnosticScope::TaskRecord, report.clone());
        self.warnings.record(WarningCategory::TaskRecord, report);
    }
}

impl TaskRecordDiagnosticRecorder for ProjectLogHandle {
    fn record_task_record_diagnostic(&self, report: DiagnosticReport) {
        self.record_task_record_failure(report);
    }
}

/// RPG Maker 直接把当前 logger 交给任务记录器；任务记录器只负责建立
/// TaskRecord scope 的原子诊断，enqueue 失败由 logger health 统一统计。
impl TaskRecordDiagnosticRecorder for ProjectLogger {
    fn record_task_record_diagnostic(&self, report: DiagnosticReport) {
        let _ = self.record_diagnostic(DiagnosticScope::TaskRecord, report);
    }
}

#[derive(Clone)]
pub(crate) struct ProjectLogLuaPrintSink {
    handle: ProjectLogHandle,
}

impl ProjectLogLuaPrintSink {
    pub(crate) fn from_active(project_log: &ActiveProjectLog) -> Self {
        Self {
            handle: project_log.handle.clone(),
        }
    }
}

impl ProjectLuaPrintSink for ProjectLogLuaPrintSink {
    fn print(&self, bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        self.handle.emit(ProjectLogEvent::lua_print(
            String::from_utf8_lossy(bytes).as_ref(),
        ));
        Ok(())
    }
}

pub(crate) struct ActiveProjectLog {
    run_id: Option<String>,
    run_id_failure: Option<DiagnosticReport>,
    runtime: Option<ProjectLogRuntime>,
    handle: ProjectLogHandle,
    warning_presenter: Option<ProjectLogWarningPresenter>,
    performance: Arc<RunPerformanceCounters>,
    log_path: Option<PathBuf>,
    engine: ProjectLogEngine,
    command: ProjectLogCommand,
    project_workspace: PathBuf,
}

impl ActiveProjectLog {
    pub(crate) fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub(crate) fn run_id_failure(&self) -> Option<&DiagnosticReport> {
        self.run_id_failure.as_ref()
    }

    pub(crate) fn handle(&self) -> &ProjectLogHandle {
        &self.handle
    }

    pub(crate) fn performance(&self) -> &Arc<RunPerformanceCounters> {
        &self.performance
    }

    pub(crate) fn pending_succeeded(self) -> PendingProjectLog {
        PendingProjectLog::new(self, PendingRunFinished::Known(RunFinished::Succeeded))
    }

    pub(crate) fn pending_cancelled(self) -> PendingProjectLog {
        PendingProjectLog::new(self, PendingRunFinished::Known(RunFinished::Cancelled))
    }

    pub(crate) fn pending_failure(self, report: DiagnosticReport) -> PendingProjectLog {
        let effect = report.effect();
        let prepared = self
            .handle
            .prepare_terminal_diagnostic(DiagnosticScope::Run, report);
        let result = prepared.map_or(PendingRunFinished::Unavailable, |prepared| {
            let result = run_finished_from_effect(effect, prepared.id());
            PendingRunFinished::Prepared {
                result,
                diagnostics: vec![prepared],
            }
        });
        PendingProjectLog::new(self, result)
    }

    pub(crate) fn pending_failure_with_occurrence(
        self,
        effect: StateEffect,
        diagnostic: DiagnosticOccurrenceId,
    ) -> PendingProjectLog {
        PendingProjectLog::new(
            self,
            PendingRunFinished::Known(run_finished_from_effect(effect, diagnostic)),
        )
    }

    /// 发布边界已经确认输出生效但仍有恢复现场时，进程终态必须保留恢复语义。
    ///
    /// `AppliedFinalizationFailed` 还可用于不要求恢复的普通收尾失败，不能仅凭 effect
    /// 全局推导；因此只有掌握 Publication 终态的调用方使用本入口。
    pub(crate) fn pending_recovery_required_with_occurrence(
        self,
        diagnostic: DiagnosticOccurrenceId,
    ) -> PendingProjectLog {
        PendingProjectLog::new(
            self,
            PendingRunFinished::Known(RunFinished::RecoveryRequired { diagnostic }),
        )
    }
}

enum PendingRunFinished {
    Known(RunFinished),
    Prepared {
        result: RunFinished,
        diagnostics: Vec<PreparedTerminalDiagnostic>,
    },
    Unavailable,
}

pub(crate) struct PendingProjectLog {
    active: ActiveProjectLog,
    finished: PendingRunFinished,
    result_presentation_prepared: bool,
}

impl PendingProjectLog {
    fn new(active: ActiveProjectLog, finished: PendingRunFinished) -> Self {
        Self {
            active,
            finished,
            result_presentation_prepared: false,
        }
    }

    pub(crate) fn finish(mut self) -> Option<ProjectLogWarning> {
        self.prepare_for_result_presentation();
        if let Some(runtime) = self.active.runtime.take() {
            match self.finished {
                PendingRunFinished::Known(result) => {
                    if let Err(error) = runtime.finish(result, Vec::new()) {
                        self.active
                            .handle
                            .record_contract_failure(error.diagnostic_report());
                    }
                }
                PendingRunFinished::Prepared {
                    result,
                    diagnostics,
                } => {
                    if let Err(error) = runtime.finish(result, diagnostics) {
                        self.active
                            .handle
                            .record_contract_failure(error.diagnostic_report());
                    }
                }
                PendingRunFinished::Unavailable => drop(runtime),
            }
        }
        if let Some(logger) = self.active.handle.logger.as_ref() {
            logger.clear_health_observer();
        }
        let health = self.active.handle.health();
        self.active.handle.warnings.warning(&health)
    }

    /// 在写最终业务结果前停止并排空后台 stderr presenter，同时保持项目日志可写。
    /// 这样最终多行诊断不会与后台 warning 交错，呈现线程自身的失败也能进入 run 终态。
    pub(crate) fn prepare_for_result_presentation(&mut self) {
        if self.result_presentation_prepared {
            return;
        }
        if let Some(logger) = self.active.handle.logger.as_ref() {
            logger.clear_health_observer();
        }
        if let Some(presenter) = self.active.warning_presenter.take() {
            presenter.finish();
        }
        for report in self.active.handle.warnings.presentation_failures() {
            self.append_terminal_report(report);
        }
        self.result_presentation_prepared = true;
    }

    /// 最终 stdout/stderr 呈现失败时，由这份新报告决定进程终态，同时保留此前已经
    /// 准备好的业务或 shutdown 终端诊断。
    #[cfg(test)]
    pub(crate) fn finish_with_diagnostic(
        self,
        report: DiagnosticReport,
    ) -> Option<ProjectLogWarning> {
        self.finish_with_diagnostics([report])
    }

    /// 最终结果的多个输出动作都失败时，逐项保留这些呈现错误，再一次性关闭项目日志。
    pub(crate) fn finish_with_diagnostics(
        mut self,
        reports: impl IntoIterator<Item = DiagnosticReport>,
    ) -> Option<ProjectLogWarning> {
        for report in reports {
            self.append_terminal_report(report);
        }
        self.finish()
    }

    fn append_terminal_report(&mut self, report: DiagnosticReport) {
        let effect = report.effect();
        let Some(prepared) = self
            .active
            .handle
            .prepare_terminal_diagnostic(DiagnosticScope::Run, report)
        else {
            return;
        };
        let candidate = run_finished_from_effect(effect, prepared.id());
        let previous = std::mem::replace(&mut self.finished, PendingRunFinished::Unavailable);
        let (result, mut diagnostics) = match previous {
            PendingRunFinished::Known(result) => {
                (stronger_run_finished(result, candidate), Vec::new())
            }
            PendingRunFinished::Prepared {
                result,
                diagnostics,
            } => (stronger_run_finished(result, candidate), diagnostics),
            PendingRunFinished::Unavailable => (candidate, Vec::new()),
        };
        diagnostics.push(prepared);
        self.finished = PendingRunFinished::Prepared {
            result,
            diagnostics,
        };
    }

    /// 最终结果呈现开始前，把 runtime 的 Drop 诊断切换到进程输出边界。
    pub(crate) fn arm_presentation_panic(&mut self) -> DiagnosticReport {
        let report = DiagnosticReport::new(
            StateEffect::OutcomeUnknown,
            Diagnostic::runtime(RuntimeIssue::ResultPresentationPanicked {
                engine: runtime_engine(self.active.engine),
                command: runtime_command(self.active.command),
                project_workspace: SafePath::new(&self.active.project_workspace),
                log_path: self.active.log_path.as_deref().map(SafePath::new),
            }),
        );
        let drop_report = match &self.finished {
            PendingRunFinished::Prepared { diagnostics, .. } if !diagnostics.is_empty() => {
                let mut combined = report.clone();
                for diagnostic in diagnostics {
                    combined = combined.with_related(
                        RelatedFailureRelation::Finalization,
                        diagnostic.report().clone(),
                    );
                }
                combined
            }
            PendingRunFinished::Known(_)
            | PendingRunFinished::Prepared { .. }
            | PendingRunFinished::Unavailable => report.clone(),
        };
        if let Some(runtime) = self.active.runtime.as_mut() {
            runtime.replace_drop_report(drop_report);
        }
        report
    }
}

fn stronger_run_finished(current: RunFinished, candidate: RunFinished) -> RunFinished {
    if run_finished_priority(current) >= run_finished_priority(candidate) {
        current
    } else {
        candidate
    }
}

const fn run_finished_priority(result: RunFinished) -> u8 {
    match result {
        RunFinished::Succeeded | RunFinished::Cancelled => 0,
        RunFinished::Failed { .. } => 1,
        RunFinished::RecoveryRequired { .. } => 2,
        RunFinished::OutcomeUnknown { .. } => 3,
    }
}

fn run_finished_from_effect(
    effect: StateEffect,
    diagnostic: DiagnosticOccurrenceId,
) -> RunFinished {
    match effect {
        StateEffect::RecoveryRequired => RunFinished::RecoveryRequired { diagnostic },
        StateEffect::OutcomeUnknown => RunFinished::OutcomeUnknown { diagnostic },
        StateEffect::Unchanged
        | StateEffect::ProgressPreserved
        | StateEffect::Applied
        | StateEffect::AppliedRunPlanNotSaved
        | StateEffect::AppliedFinalizationFailed => RunFinished::Failed { diagnostic },
    }
}

pub(crate) struct CommandLogStart<'a> {
    pub(crate) common: &'a CommonCommandConfiguration,
    pub(crate) locale: UiLocale,
    pub(crate) engine: ProjectLogEngine,
    pub(crate) project: &'a str,
    pub(crate) command: ProjectLogCommand,
    pub(crate) performance: Arc<RunPerformanceCounters>,
}

pub(crate) fn start_command_log(input: CommandLogStart<'_>) -> ActiveProjectLog {
    let CommandLogStart {
        common,
        locale,
        engine,
        project,
        command,
        performance,
    } = input;
    let project_workspace = common
        .projects_root()
        .join(engine_storage_name(engine))
        .join(project);
    let context = match ProjectLogContext::new(locale, engine, project, command) {
        Ok(context) => context,
        Err(_) => {
            let (warnings, warning_presenter) = create_warning_state(locale, None);
            let report = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::contract(
                    ObservabilityComponent::ProjectLog,
                    ObservabilityContractViolation::InvalidContextIdentifier,
                )),
            );
            warnings.record(WarningCategory::ProjectLog, report.clone());
            return ActiveProjectLog {
                run_id: None,
                run_id_failure: Some(report),
                runtime: None,
                handle: ProjectLogHandle {
                    logger: None,
                    warnings,
                },
                warning_presenter,
                performance,
                log_path: None,
                engine,
                command,
                project_workspace,
            };
        }
    };
    let logs_root = project_workspace.join("logs");
    let (run_id, log_path, reserved_file) = match reserve_run_log(&project_workspace) {
        Ok(reserved) => reserved,
        Err(source) => {
            let (warnings, warning_presenter) = create_warning_state(locale, None);
            let report = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::create(
                    ObservabilityComponent::ProjectLog,
                    SafePath::new(&logs_root),
                    &source,
                )),
            );
            warnings.record(WarningCategory::ProjectLog, report.clone());
            return ActiveProjectLog {
                run_id: None,
                run_id_failure: Some(report),
                runtime: None,
                handle: ProjectLogHandle {
                    logger: None,
                    warnings,
                },
                warning_presenter,
                performance,
                log_path: None,
                engine,
                command,
                project_workspace,
            };
        }
    };
    let run_id_text = run_id.to_string();
    let (warnings, warning_presenter) = create_warning_state(locale, Some(log_path.clone()));

    let drop_report = DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: runtime_engine(engine),
            command: runtime_command(command),
            project_workspace: SafePath::new(&project_workspace),
            log_path: Some(SafePath::new(&log_path)),
        }),
    );
    let runtime = match ProjectLogRuntime::start_reserved_file(
        &log_path,
        reserved_file,
        context,
        run_id,
        Arc::clone(&performance),
        drop_report,
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            warnings.record(WarningCategory::ProjectLog, error.into_report());
            return ActiveProjectLog {
                run_id: Some(run_id_text),
                run_id_failure: None,
                runtime: None,
                handle: ProjectLogHandle {
                    logger: None,
                    warnings,
                },
                warning_presenter,
                performance,
                log_path: Some(log_path),
                engine,
                command,
                project_workspace,
            };
        }
    };
    let logger = runtime.logger();
    let observer_warnings = Arc::clone(&warnings);
    logger.install_health_observer(move |snapshot| {
        observer_warnings.present_health_snapshot(&snapshot);
    });
    ActiveProjectLog {
        run_id: Some(run_id_text),
        run_id_failure: None,
        runtime: Some(runtime),
        handle: ProjectLogHandle {
            logger: Some(logger),
            warnings,
        },
        warning_presenter,
        performance,
        log_path: Some(log_path),
        engine,
        command,
        project_workspace,
    }
}

const fn engine_storage_name(engine: ProjectLogEngine) -> &'static str {
    match engine {
        ProjectLogEngine::Generic => "generic",
        ProjectLogEngine::RpgMakerMv => "mv",
        ProjectLogEngine::RpgMakerMz => "mz",
    }
}

const fn runtime_engine(engine: ProjectLogEngine) -> RuntimeEngine {
    match engine {
        ProjectLogEngine::Generic => RuntimeEngine::Generic,
        ProjectLogEngine::RpgMakerMv => RuntimeEngine::RpgMakerMv,
        ProjectLogEngine::RpgMakerMz => RuntimeEngine::RpgMakerMz,
    }
}

const fn runtime_command(command: ProjectLogCommand) -> RuntimeCommand {
    match command {
        ProjectLogCommand::Init => RuntimeCommand::Init,
        ProjectLogCommand::Extract => RuntimeCommand::Extract,
        ProjectLogCommand::Builtin => RuntimeCommand::Builtin,
        ProjectLogCommand::Rules => RuntimeCommand::Rules,
        ProjectLogCommand::Translate => RuntimeCommand::Translate,
        ProjectLogCommand::WriteBack => RuntimeCommand::WriteBack,
        ProjectLogCommand::Lua => RuntimeCommand::Lua,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use crate::diagnostic::{
        IoFailure, ObservabilityFailureCount, RuntimeComponent, RuntimeOperation,
    };
    use crate::runtime::project_log::{ProjectLogFailureCount, ProjectLogFailureKey};

    use super::*;

    fn active_lua_log(root: &std::path::Path, project: &str) -> ActiveProjectLog {
        let common = CommonCommandConfiguration::for_test(root);
        let workspace = root.join("generic").join(project);
        fs::create_dir_all(&workspace).expect("应建立项目工作区");
        start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::English,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Lua,
            performance: Arc::new(RunPerformanceCounters::default()),
        })
    }

    fn process_io_report(operation: RuntimeOperation) -> DiagnosticReport {
        let source = io::Error::from_raw_os_error(5);
        DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation,
                failure: IoFailure::from_error(&source),
            }),
        )
    }

    fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str(line).expect("项目日志每行都必须是 JSON"))
            .collect()
    }

    #[test]
    fn failed_run_id_reservation_is_also_reported_when_task_records_were_requested() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let common = CommonCommandConfiguration::for_test(temporary.path());
        let workspace = temporary.path().join("generic/demo");
        fs::create_dir_all(&workspace).expect("应建立项目工作区");
        fs::write(workspace.join("logs"), b"not-a-directory").expect("应建立日志保留故障现场");
        let active = start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::English,
            engine: ProjectLogEngine::Generic,
            project: "demo",
            command: ProjectLogCommand::Translate,
            performance: Arc::new(RunPerformanceCounters::default()),
        });

        assert!(active.run_id().is_none());
        let failure = active
            .run_id_failure()
            .cloned()
            .expect("日志保留失败必须保留可呈现诊断");
        active.handle().record_task_record_diagnostic(failure);

        let warning = active
            .pending_succeeded()
            .finish()
            .expect("日志与任务记录故障必须进入最终警告");
        assert_eq!(warning.project_log.len(), 1);
        assert_eq!(warning.task_records.len(), 1);
    }

    #[test]
    fn presentation_failure_keeps_prepared_shutdown_diagnostic() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let active = active_lua_log(temporary.path(), "compound-failure");
        let log_path = active.log_path.clone().expect("项目日志必须建立");

        let warning = active
            .pending_failure(process_io_report(RuntimeOperation::ExecuteTask))
            .finish_with_diagnostic(process_io_report(RuntimeOperation::WriteStdout));

        assert!(warning.is_none(), "正常日志收尾不应产生额外警告");
        let records = read_records(&log_path);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "diagnostic.run")
                .count(),
            2,
            "shutdown 与最终呈现错误都必须进入项目日志"
        );
        let finished = records
            .iter()
            .find(|record| record["event"] == "run.finished")
            .expect("运行必须有终态");
        assert_eq!(finished["payload"]["result"]["kind"], "failed");
    }

    #[test]
    fn cancellation_presentation_failure_replaces_cancelled_run_result() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let active = active_lua_log(temporary.path(), "cancel-presentation");
        let log_path = active.log_path.clone().expect("项目日志必须建立");

        active
            .pending_cancelled()
            .finish_with_diagnostic(process_io_report(RuntimeOperation::WriteStderr));

        let records = read_records(&log_path);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "diagnostic.run")
                .count(),
            1
        );
        let finished = records
            .iter()
            .find(|record| record["event"] == "run.finished")
            .expect("运行必须有终态");
        assert_eq!(finished["payload"]["result"]["kind"], "failed");
    }

    #[test]
    fn presentation_failure_does_not_weaken_recovery_or_unknown_result() {
        for (project, effect, expected) in [
            (
                "recovery-presentation",
                StateEffect::RecoveryRequired,
                "recovery_required",
            ),
            (
                "unknown-presentation",
                StateEffect::OutcomeUnknown,
                "outcome_unknown",
            ),
        ] {
            let temporary = tempfile::tempdir().expect("应建立测试目录");
            let active = active_lua_log(temporary.path(), project);
            let log_path = active.log_path.clone().expect("项目日志必须建立");
            active
                .pending_failure(DiagnosticReport::new(
                    effect,
                    process_io_report(RuntimeOperation::ExecuteTask)
                        .primary()
                        .clone(),
                ))
                .finish_with_diagnostic(process_io_report(RuntimeOperation::WriteStderr));

            let records = read_records(&log_path);
            let finished = records
                .iter()
                .find(|record| record["event"] == "run.finished")
                .expect("运行必须有终态");
            assert_eq!(finished["payload"]["result"]["kind"], expected);
        }
    }

    #[test]
    fn equal_strength_presentation_failures_keep_the_first_terminal_cause() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let active = active_lua_log(temporary.path(), "first-presentation-failure");
        let mut pending = active.pending_succeeded();
        pending.append_terminal_report(process_io_report(RuntimeOperation::WriteStdout));
        let first = match &pending.finished {
            PendingRunFinished::Prepared {
                result: RunFinished::Failed { diagnostic },
                ..
            } => *diagnostic,
            _ => panic!("首个呈现错误必须建立待写终态"),
        };
        pending.append_terminal_report(process_io_report(RuntimeOperation::WriteStderr));
        let final_cause = match &pending.finished {
            PendingRunFinished::Prepared {
                result: RunFinished::Failed { diagnostic },
                ..
            } => *diagnostic,
            _ => panic!("多个呈现错误必须保留待写终态"),
        };
        assert_eq!(final_cause, first, "同强度错误必须保留最先发生者");
        pending.finish();
    }

    #[test]
    fn presenter_failure_is_known_before_successful_run_is_finished() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let active = active_lua_log(temporary.path(), "presenter-failure");
        let log_path = active.log_path.clone().expect("项目日志必须建立");
        active
            .handle
            .warnings
            .record_presentation_failure(process_io_report(RuntimeOperation::WriteStderr));
        let mut pending = active.pending_succeeded();

        pending.prepare_for_result_presentation();
        assert!(pending.active.warning_presenter.is_none());
        pending.finish();

        let records = read_records(&log_path);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "diagnostic.run")
                .count(),
            1
        );
        assert_eq!(
            records.last().expect("项目日志必须有终态")["payload"]["result"]["kind"],
            "failed"
        );
    }

    #[test]
    fn writer_health_is_not_repeated_after_successful_immediate_presentation() {
        let state = ProjectLogWarningState::new(UiLocale::English, None);
        let (sender, receiver) = mpsc::channel();
        state.install_presenter(sender);
        let health = ProjectLogHealthSnapshot {
            failures: vec![ProjectLogFailureCount {
                failure: ProjectLogFailureKey::ChannelClosed { code: None },
                count: ObservabilityFailureCount::exact(1),
            }],
        };

        state.present_health_snapshot(&health);
        receiver
            .recv()
            .expect("即时 presenter 必须收到唯一的新故障");
        state.close_presenter();

        assert!(
            state.warning(&health).is_none(),
            "已经交给即时 presenter 的同一故障不得在终态重复"
        );
    }

    #[test]
    fn writer_health_is_retained_when_no_immediate_presenter_exists() {
        let state = ProjectLogWarningState::new(UiLocale::English, None);
        let health = ProjectLogHealthSnapshot {
            failures: vec![ProjectLogFailureCount {
                failure: ProjectLogFailureKey::ChannelClosed { code: None },
                count: ObservabilityFailureCount::exact(1),
            }],
        };

        state.present_health_snapshot(&health);
        let warning = state
            .warning(&health)
            .expect("未呈现的 writer 故障必须进入终态");
        assert_eq!(warning.project_log.len(), 1);
    }

    #[test]
    fn terminal_warning_cannot_pass_consumed_health_before_fallback_is_recorded() {
        let state = Arc::new(ProjectLogWarningState::new(UiLocale::English, None));
        let health = ProjectLogHealthSnapshot {
            failures: vec![ProjectLogFailureCount {
                failure: ProjectLogFailureKey::ChannelClosed { code: None },
                count: ObservabilityFailureCount::exact(1),
            }],
        };
        let project_guard = lock_unpoisoned(&state.project_log);
        let presenter_state = Arc::clone(&state);
        let presenter_health = health.clone();
        let presenter = thread::spawn(move || {
            presenter_state.present_health_snapshot(&presenter_health);
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match state.health_cursor.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("health cursor 不得被 poison")
                }
                Ok(guard) => drop(guard),
            }
            assert!(
                Instant::now() < deadline,
                "即时路径必须先取得 cursor 并等待 fallback 列表"
            );
            thread::yield_now();
        }

        let warning_state = Arc::clone(&state);
        let warning_health = health.clone();
        let warning = thread::spawn(move || warning_state.warning(&warning_health));
        drop(project_guard);
        presenter.join().expect("即时路径不得 panic");
        let warning = warning
            .join()
            .expect("终态路径不得 panic")
            .expect("fallback 故障必须进入终态");
        assert_eq!(warning.project_log.len(), 1);
    }

    #[test]
    fn presentation_panic_keeps_prepared_shutdown_diagnostic() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let active = active_lua_log(temporary.path(), "presentation-panic");
        let log_path = active.log_path.clone().expect("项目日志必须建立");
        let mut pending = active.pending_failure(process_io_report(RuntimeOperation::ExecuteTask));

        let panic_report = pending.arm_presentation_panic();
        let expected = crate::diagnostic::render_diagnostic_fields(
            &panic_report,
            &UiLocalizer::new(UiLocale::English),
        );
        drop(pending);

        let records = read_records(&log_path);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "diagnostic.run")
                .count(),
            2,
            "原 shutdown 错误和最终呈现 panic 都必须进入项目日志"
        );
        let primary = records
            .iter()
            .find(|record| record["event"] == "diagnostic.run")
            .expect("呈现 panic 必须是未知终态的主诊断");
        assert_eq!(primary["payload"]["object"], expected.object);
        assert_eq!(primary["payload"]["reason"], expected.reason);
        assert_eq!(primary["payload"]["help"], expected.help);
        let finished = records
            .iter()
            .find(|record| record["event"] == "run.finished")
            .expect("运行必须有终态");
        assert_eq!(finished["payload"]["result"]["kind"], "outcome_unknown");
    }
}
