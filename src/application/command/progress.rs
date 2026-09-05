//! RPG Maker 命令阶段的终端进度、日志观察和结束状态。

#[cfg(test)]
use super::business_log::ProductionBusinessLog;
use super::error::ProductionCommandError;
#[cfg(test)]
use super::lifecycle::{
    CommandPanicBoundary, CommandRunResult, ProductionCommandRunReport, catch_command_panic,
};
use super::lifecycle::{ShutdownFailures, report_with_shutdown, shutdown_report, signal_report};
#[cfg(test)]
use crate::application::config::ConfiguredInitCommand;
use crate::application::project_log::{ActiveProjectLog, PendingProjectLog, ProjectLogHandle};
#[cfg(test)]
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::TerminationOutcome as DrivenCommand;
#[cfg(test)]
use crate::diagnostic::{
    Diagnostic, ReportedFailure, RuntimeComponent, RuntimeEngine, RuntimeIssue, RuntimeOperation,
    SafeIdentifier,
};
use crate::diagnostic::{DiagnosticReport, RelatedFailureRelation, StateEffect};
use crate::execution::OperationCompletion;
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
#[cfg(test)]
use crate::llm::ApiKeyRedactor;
use crate::progress::{
    ProgressAmount, ProgressObserver, ProgressSnapshot, TerminalProgress, TerminalProgressFailures,
    TerminalProgressObserver,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::extract::ExtractProgressPhase;
use crate::rpg_maker::init::InitProgressPhase;
use crate::rpg_maker::translate::TranslateOutput;
#[cfg(test)]
use crate::rpg_maker::translate::pipeline::{
    RpgMakerTranslationLog, RpgMakerTranslationLogEvent, RpgMakerTranslationLogTaskOutcome,
    RpgMakerTranslationRunReport,
};
use crate::rpg_maker::write_back::WriteBackProgressPhase;
#[cfg(test)]
use crate::runtime::performance::RunPerformanceCounters;
#[cfg(test)]
use crate::runtime::project_log::ProjectLogCommand;
use crate::runtime::project_log::{
    DiagnosticOccurrenceId, DiagnosticScope, PhaseStopOutcome, ProjectLogAmount, ProjectLogEngine,
    ProjectLogEvent, ProjectLogPhase,
};
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Translate 终端只解释本纵向切片拥有的阶段；任务计数来自已提交终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranslateProgressPhase {
    Planning,
    ConfirmedTasks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProjectLuaProgressPhase {
    Running,
}

pub(super) fn init_terminal_progress(locale: UiLocale) -> TerminalProgress<InitProgressPhase> {
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

pub(super) fn extract_terminal_progress(
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

pub(super) fn translate_terminal_progress(
    locale: UiLocale,
) -> TerminalProgress<TranslateProgressPhase> {
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

pub(super) fn project_lua_terminal_progress(
    locale: UiLocale,
) -> TerminalProgress<ProjectLuaProgressPhase> {
    let localizer = UiLocalizer::new(locale);
    let running = localizer.format(UiMessage::ProgressProjectLua);
    let no_work = localizer.format(UiMessage::ProgressNoWork);
    TerminalProgress::stderr(move |_| running.clone(), no_work)
}

pub(super) fn write_back_terminal_progress(
    locale: UiLocale,
) -> TerminalProgress<WriteBackProgressPhase> {
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

pub(super) fn progress_safe_stopping(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressSafeStopping)
}

pub(super) fn progress_finalizing(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressFinalizing)
}

pub(super) fn progress_saving_plan(locale: UiLocale) -> String {
    UiLocalizer::new(locale).format(UiMessage::ProgressSaveRunPlan)
}

pub(super) fn defer_terminal_progress_status(result: Result<(), TerminalProgressFailures>) {
    if let Err(failures) = result {
        // `TerminalProgress` 同时把这些事实保存在共享健康状态中；这里不能改变业务
        // future 的返回类型，最终 `finish` 会把同一批失败完整交给 shutdown 结果。
        debug_assert!(!failures.failures().is_empty());
    }
}

pub(super) fn finish_terminal_progress<P>(
    progress: TerminalProgress<P>,
    mut shutdown: ShutdownFailures,
) -> ShutdownFailures {
    if let Err(failures) = progress.finish() {
        let report = failures.diagnostic_report();
        shutdown.push("terminal progress", failures, report);
    }
    shutdown
}

#[derive(Clone)]
pub(super) struct ProductionProgressObserver<P> {
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
    pub(super) fn new(
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

    pub(super) fn without_project_log(
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

    pub(super) fn complete_phase(&self, target: P) {
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

    pub(super) fn stop_active(&self, outcome: PhaseStopOutcome) {
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

    pub(super) fn confirmed_amount(&self) -> (u64, Option<u64>) {
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

pub(super) const fn init_phase_code(phase: InitProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        InitProgressPhase::CheckingProject => ProjectLogPhase::CheckProject,
        InitProgressPhase::ScanningSource => ProjectLogPhase::ScanSource,
        InitProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        InitProgressPhase::UpdatingDatabase => ProjectLogPhase::UpdateDatabase,
        InitProgressPhase::Publishing => ProjectLogPhase::Publish,
    })
}

pub(super) const fn extract_phase_code(phase: ExtractProgressPhase) -> Option<ProjectLogPhase> {
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

pub(super) const fn translate_phase_code(phase: TranslateProgressPhase) -> Option<ProjectLogPhase> {
    Some(match phase {
        TranslateProgressPhase::Planning => ProjectLogPhase::Planning,
        TranslateProgressPhase::ConfirmedTasks => ProjectLogPhase::ConfirmedTasks,
    })
}

pub(super) const fn project_lua_phase_code(_: ProjectLuaProgressPhase) -> Option<ProjectLogPhase> {
    Some(ProjectLogPhase::Lua)
}

pub(super) const fn write_back_phase_code(
    phase: WriteBackProgressPhase,
) -> Option<ProjectLogPhase> {
    Some(match phase {
        WriteBackProgressPhase::ReadingAssets => ProjectLogPhase::ReadAssets,
        WriteBackProgressPhase::PlanningTranslations => ProjectLogPhase::PlanRpgMakerWriteBack,
        WriteBackProgressPhase::RewritingDocuments => ProjectLogPhase::RewriteDocuments,
        WriteBackProgressPhase::PreparingCandidate => ProjectLogPhase::PrepareCandidate,
        WriteBackProgressPhase::ValidatingCandidate => ProjectLogPhase::ValidateCandidate,
        WriteBackProgressPhase::Publishing => ProjectLogPhase::Publish,
    })
}

pub(super) const fn project_log_engine(layout: RpgMakerLayout) -> ProjectLogEngine {
    match layout.engine() {
        crate::rpg_maker::RpgMakerEngine::Mv => ProjectLogEngine::RpgMakerMv,
        crate::rpg_maker::RpgMakerEngine::Mz => ProjectLogEngine::RpgMakerMz,
    }
}

pub(super) fn pending_project_log_for_execution<T>(
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

pub(super) fn execution_failure_report<T>(
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

pub(super) fn record_failed_phase<P, T>(
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

pub(super) fn pending_project_log_with_occurrence<T>(
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

pub(super) fn pending_project_log_for_translation_execution(
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

pub(super) fn business_completed<T>(
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

pub(super) fn finish_progress_business_state<P, T>(
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

pub(super) fn completed_output<T>(
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
    use std::num::NonZeroUsize;

    use super::*;
    use crate::application::arguments::InitArguments;
    #[cfg(test)]
    use crate::application::arguments::ProjectArguments;
    use crate::rpg_maker::translate::TranslationSummary;
    use crate::rpg_maker::translate::pipeline::RpgMakerTaskDiagnosticReports;
    #[cfg(test)]
    use crate::rpg_maker::translate::pipeline::RpgMakerTranslationTaskIndex;

    fn active_project_log_for(
        projects_root: &Path,
        log_command: ProjectLogCommand,
    ) -> ActiveProjectLog {
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
            command: log_command,
            performance: Arc::new(RunPerformanceCounters::default()),
            selected_api_key_redactor: None,
        })
    }

    fn active_project_log(projects_root: &Path) -> ActiveProjectLog {
        active_project_log_for(projects_root, ProjectLogCommand::Init)
    }

    fn retry_exhausted_report() -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::http(crate::diagnostic::HttpIssue::Status {
                endpoint: crate::diagnostic::HttpEndpoint::new(
                    crate::diagnostic::HttpScheme::Https,
                    "example.test",
                    None,
                ),
                status: 429,
                retry_after_seconds: Some(2),
                provider_code: Some(
                    SafeIdentifier::new("busy").expect("测试 provider code 应有效"),
                ),
                provider_type: Some(
                    SafeIdentifier::new("service_error").expect("测试 provider type 应有效"),
                ),
                provider_message: None,
                response_read_failure: None,
            }),
        )
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

    #[test]
    fn retry_summary_uses_retry_attempts_for_each_outcome_bucket() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let project_log = active_project_log_for(temporary.path(), ProjectLogCommand::Translate);
        let log_path = project_log
            .established_log_path()
            .expect("Translate 测试应建立项目日志")
            .to_path_buf();
        let business_log = ProductionBusinessLog::from_active(&project_log);
        let final_summary = TranslationSummary {
            total_tasks: 2,
            started_tasks: 2,
            not_started_tasks: 0,
            complete_tasks: 1,
            partial_tasks: 0,
            unavailable_tasks: 1,
            accepted_decisions: 1,
            written_locations: 1,
            remaining_decisions: 1,
            remaining_locations: 1,
            rejected_locations: 0,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 1,
            request_admission_stopped: true,
            retained: 0,
            invalidated: 0,
            not_applicable: 0,
            reused: 0,
        };
        let first_task_summary = TranslationSummary {
            started_tasks: 1,
            not_started_tasks: 1,
            unavailable_tasks: 0,
            recoverable_request_exhaustions: 0,
            request_admission_stopped: false,
            ..final_summary
        };

        RpgMakerTranslationLog::emit(
            &business_log,
            RpgMakerTranslationLogEvent::PlanningCompleted {
                report: RpgMakerTranslationRunReport::with_reconciliation(2, 2, 2, 0, 0, 0, 0),
            },
        );
        for (task_index, attempts, retry_exhausted) in [(0, 4, false), (1, 3, true)] {
            RpgMakerTranslationLog::emit(
                &business_log,
                RpgMakerTranslationLogEvent::TaskStarted {
                    task_index: RpgMakerTranslationTaskIndex::new(task_index),
                    total_tasks: 2,
                },
            );
            RpgMakerTranslationLog::emit(
                &business_log,
                RpgMakerTranslationLogEvent::TaskFinished {
                    task_index: RpgMakerTranslationTaskIndex::new(task_index),
                    outcome: if retry_exhausted {
                        RpgMakerTranslationLogTaskOutcome::Unavailable {
                            diagnostics: RpgMakerTaskDiagnosticReports::for_test(
                                retry_exhausted_report(),
                            ),
                        }
                    } else {
                        RpgMakerTranslationLogTaskOutcome::Complete {
                            diagnostics: Vec::new(),
                        }
                    },
                    attempts: NonZeroUsize::new(attempts),
                    provider: Some("SiliconFlow".to_owned()),
                    retry_exhausted,
                    report: RpgMakerTranslationRunReport::from_summary_for_test(
                        if retry_exhausted {
                            final_summary
                        } else {
                            first_task_summary
                        },
                    ),
                },
            );
        }

        business_log.emit_retry_summary();
        let execution =
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(TranslateOutput {
                name: "phase-contract".parse().expect("测试项目名应有效"),
                profile_id: "default".to_owned(),
                summary: final_summary,
            })));
        assert!(business_log.emit_translation_finished(&execution).is_none());
        drop(business_log);
        assert!(
            project_log.pending_succeeded().finish().is_none(),
            "重试汇总各字段使用相同计数单位时应通过项目日志合同"
        );

        let records = std::fs::read_to_string(log_path)
            .expect("Translate 项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行应为 JSON"))
            .collect::<Vec<_>>();
        let retry_summary = records
            .iter()
            .find(|record| record["event"] == "retry.summary")
            .expect("重试发生后应写出 retry.summary");
        assert_eq!(
            retry_summary["payload"],
            serde_json::json!({
                "attempted": 5,
                "recovered": 3,
                "exhausted": 2,
            })
        );
        let task_finished = records
            .iter()
            .find(|record| record["event"] == "task.finished")
            .expect("任务终态应独立于 Markdown 任务记录写入 JSONL");
        assert_eq!(task_finished["payload"]["provider"], "SiliconFlow");
        assert!(
            task_finished["message"]
                .as_str()
                .expect("任务终态 message 应为文本")
                .contains("SiliconFlow")
        );
    }

    #[test]
    fn incomplete_translation_completes_confirmed_tasks_with_actual_amount() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let project_log = active_project_log_for(temporary.path(), ProjectLogCommand::Translate);
        let log_path = project_log
            .established_log_path()
            .expect("Translate 测试应建立项目日志")
            .to_path_buf();
        let terminal = TerminalProgress::with_writer(io::sink(), |_| String::new());
        let observer = ProductionProgressObserver::new(
            terminal.observer(),
            &project_log,
            translate_phase_code,
        );
        observer.observe(ProgressSnapshot::indeterminate(
            TranslateProgressPhase::Planning,
        ));
        let business_log = ProductionBusinessLog::for_translation(&project_log, observer.clone());
        let final_summary = TranslationSummary {
            total_tasks: 3,
            started_tasks: 1,
            not_started_tasks: 2,
            complete_tasks: 0,
            partial_tasks: 0,
            unavailable_tasks: 1,
            accepted_decisions: 0,
            written_locations: 0,
            remaining_decisions: 3,
            remaining_locations: 3,
            rejected_locations: 0,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 1,
            request_admission_stopped: true,
            retained: 0,
            invalidated: 0,
            not_applicable: 0,
            reused: 0,
        };

        RpgMakerTranslationLog::emit(
            &business_log,
            RpgMakerTranslationLogEvent::PlanningCompleted {
                report: RpgMakerTranslationRunReport::with_reconciliation(3, 3, 3, 0, 0, 0, 0),
            },
        );
        RpgMakerTranslationLog::emit(
            &business_log,
            RpgMakerTranslationLogEvent::TaskStarted {
                task_index: RpgMakerTranslationTaskIndex::new(0),
                total_tasks: 3,
            },
        );
        RpgMakerTranslationLog::emit(
            &business_log,
            RpgMakerTranslationLogEvent::TaskFinished {
                task_index: RpgMakerTranslationTaskIndex::new(0),
                outcome: RpgMakerTranslationLogTaskOutcome::Unavailable {
                    diagnostics: RpgMakerTaskDiagnosticReports::for_test(retry_exhausted_report()),
                },
                attempts: NonZeroUsize::new(3),
                provider: None,
                retry_exhausted: true,
                report: RpgMakerTranslationRunReport::from_summary_for_test(final_summary),
            },
        );
        let execution =
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(TranslateOutput {
                name: "phase-contract".parse().expect("测试项目名应有效"),
                profile_id: "default".to_owned(),
                summary: final_summary,
            })));

        assert!(business_log.emit_translation_finished(&execution).is_none());
        drop(business_log);
        assert!(
            project_log.pending_succeeded().finish().is_none(),
            "正常 Incomplete 的显式阶段终态应通过项目日志合同"
        );
        terminal.finish().expect("关闭静默进度不应失败");

        let records = std::fs::read_to_string(log_path)
            .expect("Translate 项目日志应可读取")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行应为 JSON"))
            .collect::<Vec<_>>();
        let completed = records
            .iter()
            .filter(|record| {
                record["event"] == "phase.completed"
                    && record["payload"]["phase"] == "confirmed_tasks"
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0]["payload"]["amount"],
            serde_json::json!({
                "kind": "determinate",
                "completed": 1,
                "total": 3,
            })
        );
        assert!(records.iter().any(|record| {
            record["event"] == "translation.finished"
                && record["payload"]["result"]["kind"] == "incomplete"
        }));
        assert!(records.iter().any(|record| {
            record["event"] == "task.finished" && record["payload"]["provider"].is_null()
        }));
        assert!(records.iter().any(|record| {
            record["event"] == "run.finished" && record["payload"]["result"]["kind"] == "succeeded"
        }));
    }
}
