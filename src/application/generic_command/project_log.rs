//! Generic 项目日志的任务计数、阶段与唯一终态。

use super::diagnostics::{GenericCommandError, generic_command_error_report};
use super::lifecycle::Driven;
use super::tasks::GenericTaskTerminal;
use super::{
    GenericCommandOutput, GenericTranslationSummary, generic_count, generic_task_ordinal,
    generic_workspace,
};
use crate::application::TranslationTerminalSummary;
use crate::application::project_log::{
    ActiveProjectLog, CommandLogStart, PendingProjectLog, ProjectLogHandle, start_command_log,
};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation,
    SafeText, StateEffect,
};
use crate::i18n::UiLocale;
use crate::llm::ApiKeyRedactor;
use crate::project_name::ProjectName;
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticScope, GenericTranslationSummary as ProjectLogGenericTranslationSummary,
    PhaseStopOutcome, ProjectLogAmount, ProjectLogCommand, ProjectLogEngine, ProjectLogEvent,
    ProjectLogPhase, ResolvedRunPlan, RunPlanFinalization, RunPlanTransactionState,
    RunPlanValueSource, TaskFinishedOutcome, TaskPosition, TranslationEngineSummary,
    TranslationFinished, TranslationTaskCounters,
};
use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
    TaskRecordDiagnosticRecorder,
};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct GenericTaskProjectLog {
    pub(super) handle: ProjectLogHandle,
    pub(super) state: Arc<Mutex<GenericTaskLogState>>,
}

#[derive(Default)]
pub(super) struct GenericTaskLogState {
    pub(super) planned: u64,
    pub(super) started: u64,
    pub(super) complete: u64,
    pub(super) partial: u64,
    pub(super) unavailable: u64,
    pub(super) failed: u64,
    pub(super) cancelled: u64,
    pub(super) in_flight: HashSet<usize>,
    pub(super) failure_occurrence: Option<(
        crate::runtime::project_log::DiagnosticOccurrenceId,
        StateEffect,
    )>,
}

impl GenericTaskProjectLog {
    pub(super) fn new(handle: ProjectLogHandle, total_tasks: usize) -> Self {
        Self {
            handle,
            state: Arc::new(Mutex::new(GenericTaskLogState {
                planned: generic_count(total_tasks),
                ..GenericTaskLogState::default()
            })),
        }
    }

    pub(super) fn position(&self, task_index: usize) -> TaskPosition {
        let total = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .planned;
        TaskPosition::new(generic_task_ordinal(task_index), total)
            .expect("Generic task index 必须位于已确认的计划范围内")
    }

    pub(super) fn started(&self, task_index: usize) {
        let task = self.position(task_index);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.started = state
                .started
                .checked_add(1)
                .expect("Generic task started 计数不得溢出");
            assert!(
                state.in_flight.insert(task_index),
                "同一 Generic task 不得重复开始"
            );
        }
        self.handle.emit(ProjectLogEvent::TaskStarted { task });
    }

    pub(super) fn finished(
        &self,
        task_index: usize,
        attempts: usize,
        provider: Option<&str>,
        terminal: GenericTaskTerminal,
        diagnostics: impl IntoIterator<Item = DiagnosticReport>,
    ) {
        if attempts == 0 {
            return;
        }
        let task = self.position(task_index);
        let earlier_failure = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                state.in_flight.remove(&task_index),
                "Generic task 终态必须对应已经开始且尚未结束的任务"
            );
            let counter = match terminal {
                GenericTaskTerminal::Complete => &mut state.complete,
                GenericTaskTerminal::Partial => &mut state.partial,
                GenericTaskTerminal::Unavailable => &mut state.unavailable,
                GenericTaskTerminal::Failed
                | GenericTaskTerminal::NotCommittedAfterEarlierFailure => &mut state.failed,
                GenericTaskTerminal::Cancelled => &mut state.cancelled,
            };
            *counter = counter
                .checked_add(1)
                .expect("Generic task 终态计数不得溢出");
            match terminal {
                GenericTaskTerminal::NotCommittedAfterEarlierFailure => {
                    state.failure_occurrence.map(|(occurrence, _)| occurrence)
                }
                _ => None,
            }
        };
        let mut occurrence = None;
        for diagnostic in diagnostics {
            let effect = diagnostic.effect();
            let id = self
                .handle
                .record_diagnostic(DiagnosticScope::TranslationTask, diagnostic);
            if occurrence.is_none() {
                occurrence = id;
            }
            if matches!(terminal, GenericTaskTerminal::Failed)
                && let Some(id) = id
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.failure_occurrence.is_none() {
                    state.failure_occurrence = Some((id, effect));
                }
            }
        }
        let outcome = match terminal {
            GenericTaskTerminal::Complete => TaskFinishedOutcome::Complete,
            GenericTaskTerminal::Partial => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Partial { diagnostic }
            }
            GenericTaskTerminal::Unavailable => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Unavailable { diagnostic }
            }
            GenericTaskTerminal::Failed => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Failed { diagnostic }
            }
            GenericTaskTerminal::NotCommittedAfterEarlierFailure => {
                let Some(diagnostic) = earlier_failure else {
                    return;
                };
                TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic }
            }
            GenericTaskTerminal::Cancelled => TaskFinishedOutcome::Cancelled,
        };
        self.handle.emit(ProjectLogEvent::TaskFinished {
            task,
            attempts: generic_count(attempts),
            provider: provider.map(SafeText::new),
            outcome,
        });
    }

    pub(super) fn counters(&self) -> TranslationTaskCounters {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(counters) = self.handle.translation_task_counters(state.planned) {
            return counters;
        }
        // logger 已不可用时不会再有可持久化的 JSONL；保留本地快照仅供进程终态分类，
        // 不把它当作已写入的任务事实。
        let not_started = state
            .planned
            .checked_sub(state.started)
            .expect("Generic 已开始任务数不得超过计划数");
        TranslationTaskCounters::new(
            state.planned,
            state.started,
            state.complete,
            state.partial,
            state.unavailable,
            state.failed,
            state.cancelled,
            not_started,
        )
        .expect("Generic task 日志计数必须满足状态机恒等式")
    }

    pub(super) fn fail_in_flight_after_panic(&self, report: DiagnosticReport) {
        let mut in_flight = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .in_flight
            .iter()
            .copied()
            .collect::<Vec<_>>();
        in_flight.sort_unstable();
        for task_index in in_flight {
            self.finished(
                task_index,
                1,
                None,
                GenericTaskTerminal::Failed,
                [report.clone()],
            );
        }
    }

    pub(super) fn failure_occurrence(
        &self,
    ) -> Option<(
        crate::runtime::project_log::DiagnosticOccurrenceId,
        StateEffect,
    )> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure_occurrence
    }
}

pub(super) type GenericProjectLogSlot = Arc<Mutex<Option<ActiveProjectLog>>>;

#[derive(Default)]
pub(super) struct GenericTranslateProjectLogState {
    pub(super) database_path: Option<PathBuf>,
    pub(super) run_plan_resolved: bool,
    pub(super) run_plan_finalized: bool,
    pub(super) translation_finished: bool,
    pub(super) run_plan_saved: bool,
    pub(super) summary: Option<GenericTranslationSummary>,
    pub(super) tasks: Option<GenericTaskProjectLog>,
    pub(super) active_phase: Option<ProjectLogPhase>,
}

#[derive(Default)]
pub(super) struct GenericExtractProjectLogState {
    pub(super) database_path: Option<PathBuf>,
    pub(super) run_plan_resolved: bool,
    pub(super) run_plan_finalized: bool,
    pub(super) phase_started: bool,
}

pub(super) type GenericExtractProjectLogStateRef = Arc<Mutex<GenericExtractProjectLogState>>;

pub(super) type GenericTranslateProjectLogStateRef = Arc<Mutex<GenericTranslateProjectLogState>>;
#[derive(Clone, Copy)]
pub(super) struct GenericTerminalOccurrence {
    pub(super) diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
    pub(super) outcome: GenericTerminalRunOutcome,
}

#[derive(Clone, Copy)]
pub(super) enum GenericTerminalRunOutcome {
    FromEffect(StateEffect),
    RecoveryRequired,
}

impl GenericTerminalOccurrence {
    pub(super) const fn from_effect(
        diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
        effect: StateEffect,
    ) -> Self {
        Self {
            diagnostic,
            outcome: GenericTerminalRunOutcome::FromEffect(effect),
        }
    }

    pub(super) const fn recovery_required(
        diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
    ) -> Self {
        Self {
            diagnostic,
            outcome: GenericTerminalRunOutcome::RecoveryRequired,
        }
    }

    pub(super) fn into_pending(self, project_log: ActiveProjectLog) -> PendingProjectLog {
        match self.outcome {
            GenericTerminalRunOutcome::FromEffect(effect) => {
                project_log.pending_failure_with_occurrence(effect, self.diagnostic)
            }
            GenericTerminalRunOutcome::RecoveryRequired => {
                project_log.pending_recovery_required_with_occurrence(self.diagnostic)
            }
        }
    }
}
pub(super) type GenericTerminalOccurrenceSlot = Arc<Mutex<Option<GenericTerminalOccurrence>>>;

pub(super) fn generic_translate_project_log_state() -> GenericTranslateProjectLogStateRef {
    Arc::new(Mutex::new(GenericTranslateProjectLogState::default()))
}

pub(super) fn generic_terminal_occurrence_slot() -> GenericTerminalOccurrenceSlot {
    Arc::new(Mutex::new(None))
}

pub(super) fn generic_extract_project_log_state() -> GenericExtractProjectLogStateRef {
    Arc::new(Mutex::new(GenericExtractProjectLogState::default()))
}

pub(super) fn generic_project_log_slot() -> GenericProjectLogSlot {
    Arc::new(Mutex::new(None))
}

pub(super) fn start_existing_generic_project_log(
    slot: &GenericProjectLogSlot,
    common: &crate::application::config::CommonCommandConfiguration,
    locale: UiLocale,
    project: &ProjectName,
    command: ProjectLogCommand,
    performance: Arc<RunPerformanceCounters>,
) {
    let workspace = generic_workspace(common.projects_root(), project);
    // Path::is_dir 会把权限、I/O 和“同名普通文件”都压成 false，随后项目打开错误
    // 便失去本次运行的 JSONL。仅在明确不存在项目时不建立日志；其余情况让标准日志
    // 建立路径记录可观察的失败。
    if matches!(std::fs::metadata(&workspace), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return;
    }
    install_generic_project_log(
        slot,
        start_command_log(CommandLogStart {
            common,
            locale,
            engine: ProjectLogEngine::Generic,
            project: project.as_str(),
            command,
            performance,
            selected_api_key_redactor: None,
        }),
    );
}

pub(super) fn install_generic_project_log(
    slot: &GenericProjectLogSlot,
    project_log: ActiveProjectLog,
) {
    let mut current = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        current.is_none(),
        "一条 Generic 命令只能建立一个项目日志会话"
    );
    *current = Some(project_log);
}

pub(super) fn take_generic_project_log(slot: &GenericProjectLogSlot) -> Option<ActiveProjectLog> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

pub(super) fn generic_project_log_handle(slot: &GenericProjectLogSlot) -> Option<ProjectLogHandle> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|project_log| project_log.handle().clone())
}

pub(super) fn select_generic_project_log_api_key_redactor(
    slot: &GenericProjectLogSlot,
    redactor: Arc<ApiKeyRedactor>,
) {
    if let Some(project_log) = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        project_log.select_api_key_redactor(redactor);
    }
}

/// 所有 Generic 命令经过同一条合作取消路径。此时不把尚未确认的工作伪造为完成量；
/// logger 负责压缩重复信号，因此每次运行最多持久化一条 run.cancel_requested。
pub(super) fn emit_generic_cancellation_requested(project_log: &GenericProjectLogSlot) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::CancellationRequested {
            confirmed: 0,
            total: None,
        });
    }
}

pub(super) fn start_generic_extract_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_started(
            ProjectLogPhase::ScanSource,
            ProjectLogAmount::Indeterminate,
        ));
    }
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .phase_started = true;
}

pub(super) fn resolve_generic_extract_run_plan(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
    database_path: &Path,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::RunPlanResolved {
            plan: ResolvedRunPlan::generic_extract(RunPlanValueSource::ProductDefault),
        });
    }
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.database_path = Some(database_path.to_path_buf());
    state.run_plan_resolved = true;
}

fn finish_generic_extract_success(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
) {
    let Some(handle) = generic_project_log_handle(project_log) else {
        return;
    };
    let (database_path, complete_phase, finalize) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let database_path = state.database_path.clone();
        let complete_phase = std::mem::take(&mut state.phase_started);
        let finalize = state.run_plan_resolved && !state.run_plan_finalized;
        state.run_plan_finalized |= finalize;
        (database_path, complete_phase, finalize)
    };
    if complete_phase {
        handle.emit(ProjectLogEvent::phase_completed(
            ProjectLogPhase::ScanSource,
            ProjectLogAmount::Indeterminate,
        ));
    }
    if finalize {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("Generic Extract run plan 必须保存数据库路径"),
            ),
            result: RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            },
        });
    }
}

pub(super) fn finish_generic_extract_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTerminalOccurrence> {
    if matches!(
        driven,
        Driven::Finished(Ok(GenericCommandOutput::Extract { .. }))
            | Driven::Interrupted(Ok(GenericCommandOutput::Extract { .. }))
    ) {
        finish_generic_extract_success(project_log, state);
        return None;
    }
    let handle = generic_project_log_handle(project_log)?;
    let cancelled = match driven {
        Driven::CancellationWon(_) => true,
        Driven::Finished(Err(error)) | Driven::Interrupted(Err(error)) => error.is_cancelled(),
        Driven::Finished(Ok(_)) | Driven::Interrupted(Ok(_)) | Driven::SignalFailed { .. } => false,
    };
    let report = match driven {
        Driven::Finished(Err(error))
        | Driven::Interrupted(Err(error))
        | Driven::CancellationWon(Err(error)) => generic_command_error_report(error),
        Driven::SignalFailed { source, .. } => DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::TerminationSignals,
                operation: RuntimeOperation::ReceiveTerminationSignal,
                failure: IoFailure::from_error(source),
            }),
        ),
        Driven::CancellationWon(Ok(_)) => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        Driven::Finished(Ok(_)) | Driven::Interrupted(Ok(_)) => return None,
    };
    let (database_path, resolved, finalized, phase_started) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let values = (
            state.database_path.clone(),
            state.run_plan_resolved,
            state.run_plan_finalized,
            std::mem::take(&mut state.phase_started),
        );
        state.run_plan_finalized |= state.run_plan_resolved;
        values
    };
    let effect = report.effect();
    // 同一失败若还要表达 run_plan.finalized，必须以 RunPlan occurrence 引用；
    // phase.stopped 不要求专属 scope，因此可安全复用这个主诊断。
    let diagnostic = handle.record_diagnostic(
        if resolved {
            DiagnosticScope::RunPlan
        } else {
            DiagnosticScope::Extract
        },
        report,
    )?;
    if phase_started {
        handle.emit(ProjectLogEvent::phase_stopped(
            ProjectLogPhase::ScanSource,
            if cancelled {
                PhaseStopOutcome::Cancelled
            } else {
                PhaseStopOutcome::Failed { diagnostic }
            },
        ));
    }
    if resolved && !finalized {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("Generic Extract run plan 必须保存数据库路径"),
            ),
            result: if effect == StateEffect::OutcomeUnknown {
                RunPlanFinalization::OutcomeUnknown {
                    transaction: RunPlanTransactionState::OutcomeUnknown,
                    run_continues: false,
                    diagnostic,
                }
            } else {
                RunPlanFinalization::NotSaved {
                    transaction: RunPlanTransactionState::RolledBack,
                    run_continues: false,
                    diagnostic,
                }
            },
        });
    }
    Some(GenericTerminalOccurrence::from_effect(diagnostic, effect))
}

pub(super) fn start_generic_translate_phase(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    phase: ProjectLogPhase,
    amount: ProjectLogAmount,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_started(phase, amount));
    }
    let previous = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .replace(phase);
    assert!(previous.is_none(), "开始新阶段前必须显式结束上一阶段");
}

pub(super) fn complete_generic_translate_phase(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    phase: ProjectLogPhase,
    amount: ProjectLogAmount,
) {
    let active = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .take();
    assert_eq!(active, Some(phase), "只能完成当前活动阶段");
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_completed(phase, amount));
    }
}

fn generic_task_project_log(
    slot: &GenericProjectLogSlot,
    total_tasks: usize,
) -> GenericTaskProjectLog {
    let project_log = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project_log = project_log
        .as_ref()
        .expect("Generic Translate 必须在建立模型任务前建立项目日志");
    GenericTaskProjectLog::new(project_log.handle().clone(), total_tasks)
}

pub(super) fn resolve_generic_translate_run_plan(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    database_path: &Path,
    source: RunPlanValueSource,
    profile_id: &str,
    terminology_path: Option<&Path>,
    placeholder_rules_path: Option<&Path>,
) {
    let plan =
        ResolvedRunPlan::translate(source, profile_id, terminology_path, placeholder_rules_path)
            .expect("已解析的 Generic Profile ID 必须可用于项目日志");
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::RunPlanResolved { plan });
    }
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.database_path = Some(database_path.to_path_buf());
    state.run_plan_resolved = true;
}

pub(super) fn install_generic_translate_task_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    total_tasks: usize,
) -> GenericTaskProjectLog {
    let tasks = generic_task_project_log(project_log, total_tasks);
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tasks = Some(tasks.clone());
    tasks
}

pub(super) fn mark_generic_translate_run_plan_saved(state: &GenericTranslateProjectLogStateRef) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_plan_saved = true;
}

pub(super) fn set_generic_translate_summary(
    state: &GenericTranslateProjectLogStateRef,
    summary: GenericTranslationSummary,
) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summary = Some(summary);
}

pub(super) fn update_generic_translate_summary(
    state: &GenericTranslateProjectLogStateRef,
    update: impl FnOnce(&mut GenericTranslationSummary),
) {
    if let Some(summary) = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summary
        .as_mut()
    {
        update(summary);
    }
}

fn project_log_generic_translation_summary(
    summary: GenericTranslationSummary,
) -> ProjectLogGenericTranslationSummary {
    ProjectLogGenericTranslationSummary {
        planned_units: generic_count(summary.planned_units),
        remaining_units: generic_count(summary.remaining_units),
        rejected_units: generic_count(summary.rejected_units),
        cleared_units: generic_count(summary.cleared_units),
        reused_units: generic_count(summary.reused_units),
        accepted_units: generic_count(summary.accepted_units),
        written_units: generic_count(summary.written_units),
        conflicted_units: generic_count(summary.conflicted_units),
        response_problems: generic_count(summary.response_problems),
        recoverable_request_exhaustions: generic_count(summary.recoverable_request_exhaustions),
        request_admission_stopped: summary.request_admission_stopped,
    }
}

pub(super) fn generic_terminal_translation_summary(
    state: &GenericTranslateProjectLogStateRef,
) -> Option<TranslationTerminalSummary> {
    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let summary = state.summary?;
    let tasks = state.tasks.as_ref().map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须有效"),
        GenericTaskProjectLog::counters,
    );
    Some(TranslationTerminalSummary {
        tasks,
        engine: TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary)),
    })
}

fn finish_generic_translate_success(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    summary: GenericTranslationSummary,
) {
    let Some(handle) = generic_project_log_handle(project_log) else {
        return;
    };
    let (database_path, tasks, should_finalize) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let database_path = state.database_path.clone();
        let tasks = state.tasks.clone();
        let should_finalize = state.run_plan_resolved && !state.run_plan_finalized;
        state.run_plan_finalized |= should_finalize;
        state.translation_finished = true;
        (database_path, tasks, should_finalize)
    };
    if should_finalize {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("已解析的 Generic run plan 必须保存数据库路径"),
            ),
            result: RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            },
        });
    }
    let tasks = tasks.map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须满足恒等式"),
        |tasks| tasks.counters(),
    );
    let engine_summary =
        TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary));
    let result = if summary.is_incomplete() {
        TranslationFinished::Incomplete {
            tasks,
            summary: engine_summary,
        }
    } else if tasks.planned == 0 {
        TranslationFinished::NoWork {
            tasks,
            summary: engine_summary,
        }
    } else {
        TranslationFinished::Complete {
            tasks,
            summary: engine_summary,
        }
    };
    handle.emit(ProjectLogEvent::TranslationFinished { result });
}

pub(super) fn finish_generic_translate_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTerminalOccurrence> {
    if let Some(summary) = generic_translate_success_summary(driven) {
        finish_generic_translate_success(project_log, state, summary);
        return None;
    }
    let handle = generic_project_log_handle(project_log)?;
    let cancelled = generic_translate_was_cancelled(driven);
    let error = generic_translate_driven_error(driven);
    let (database_path, resolved, finalized, run_plan_saved, summary, tasks, already_finished) = {
        let state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.database_path.clone(),
            state.run_plan_resolved,
            state.run_plan_finalized,
            state.run_plan_saved,
            state.summary,
            state.tasks.clone(),
            state.translation_finished,
        )
    };
    if already_finished {
        return None;
    }

    let task_occurrence = tasks
        .as_ref()
        .and_then(GenericTaskProjectLog::failure_occurrence);
    let report = match (error, driven) {
        (Some(error), _) => generic_command_error_report(error),
        (
            None,
            Driven::SignalFailed {
                source,
                result: Ok(_),
            },
        ) => DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::TerminationSignals,
                operation: RuntimeOperation::ReceiveTerminationSignal,
                failure: IoFailure::from_error(source),
            }),
        ),
        (None, _) => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
    };
    let report_effect = report.effect();
    let occurrence = task_occurrence.or_else(|| {
        handle
            .record_diagnostic(
                if resolved
                    && tasks
                        .as_ref()
                        .is_none_or(|tasks| tasks.counters().started == 0)
                {
                    DiagnosticScope::RunPlan
                } else {
                    DiagnosticScope::Run
                },
                report.clone(),
            )
            .map(|id| (id, report_effect))
    });

    let active_phase = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .take();
    if let Some(phase) = active_phase {
        let outcome = if cancelled {
            PhaseStopOutcome::Cancelled
        } else if let Some((diagnostic, _)) = occurrence {
            PhaseStopOutcome::Failed { diagnostic }
        } else {
            return None;
        };
        handle.emit(ProjectLogEvent::phase_stopped(phase, outcome));
    }

    let plan_occurrence = if resolved && !finalized && !run_plan_saved {
        let no_task_started = tasks
            .as_ref()
            .is_none_or(|tasks| tasks.counters().started == 0);
        if no_task_started {
            occurrence
        } else {
            // task.finished 和 run_plan.finalized 各自拥有不同 scope；不能把同一个
            // TranslationTask occurrence 偷渡给运行方案终态。
            handle
                .record_diagnostic(DiagnosticScope::RunPlan, report.clone())
                .map(|id| (id, report_effect))
        }
    } else {
        occurrence
    };
    if resolved
        && !finalized
        && let (Some(database_path), Some((diagnostic, effect))) =
            (database_path.as_ref(), plan_occurrence)
    {
        let result = if run_plan_saved {
            RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            }
        } else if effect == StateEffect::OutcomeUnknown {
            RunPlanFinalization::OutcomeUnknown {
                transaction: RunPlanTransactionState::OutcomeUnknown,
                run_continues: false,
                diagnostic,
            }
        } else {
            RunPlanFinalization::NotSaved {
                transaction: RunPlanTransactionState::NotStarted,
                run_continues: false,
                diagnostic,
            }
        };
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(database_path),
            result,
        });
    }

    let counters = tasks.as_ref().map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须满足恒等式"),
        GenericTaskProjectLog::counters,
    );
    let result = if cancelled {
        TranslationFinished::Cancelled {
            tasks: counters,
            summary: summary.map(|summary| {
                TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary))
            }),
        }
    } else if let Some((diagnostic, _)) = occurrence {
        TranslationFinished::Failed {
            tasks: counters,
            summary: summary.map(|summary| {
                TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary))
            }),
            diagnostic,
        }
    } else {
        // logger 无法登记诊断时，公共句柄已经记录了独立的日志契约故障；此时不再
        // 构造一个引用未知 occurrence 的翻译终态。
        return None;
    };
    handle.emit(ProjectLogEvent::TranslationFinished { result });
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.run_plan_finalized |= resolved;
    state.translation_finished = true;
    occurrence
        .map(|(diagnostic, effect)| GenericTerminalOccurrence::from_effect(diagnostic, effect))
}

fn generic_translate_success_summary(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTranslationSummary> {
    match driven {
        Driven::Finished(Ok(GenericCommandOutput::Translate { summary, .. }))
        | Driven::Interrupted(Ok(GenericCommandOutput::Translate { summary, .. })) => {
            Some(*summary)
        }
        Driven::Finished(Ok(_))
        | Driven::Interrupted(Ok(_))
        | Driven::CancellationWon(_)
        | Driven::Finished(Err(_))
        | Driven::Interrupted(Err(_))
        | Driven::SignalFailed { .. } => None,
    }
}

pub(super) fn generic_translate_driven_error(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<&GenericCommandError> {
    match driven {
        Driven::Finished(Err(error))
        | Driven::Interrupted(Err(error))
        | Driven::CancellationWon(Err(error)) => Some(error),
        Driven::SignalFailed {
            result: Err(error), ..
        } => Some(error),
        Driven::Finished(Ok(_))
        | Driven::Interrupted(Ok(_))
        | Driven::CancellationWon(Ok(_))
        | Driven::SignalFailed { result: Ok(_), .. } => None,
    }
}

fn generic_translate_was_cancelled(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> bool {
    match driven {
        Driven::CancellationWon(_) => true,
        Driven::Finished(Err(error)) | Driven::Interrupted(Err(error)) => error.is_cancelled(),
        Driven::Interrupted(Ok(_)) | Driven::Finished(Ok(_)) | Driven::SignalFailed { .. } => false,
    }
}

pub(super) fn configure_generic_task_records(
    requested: bool,
    project_log: &GenericProjectLogSlot,
    redactor: Arc<crate::llm::ApiKeyRedactor>,
    locale: UiLocale,
    cpu: RayonCpuExecutor,
    project_workspace: &Path,
) -> ConfiguredTranslationTaskRecordSink {
    if !requested {
        return ConfiguredTranslationTaskRecordSink::disabled();
    }
    let prepared = {
        let project_log = project_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(project_log) = project_log.as_ref() else {
            return ConfiguredTranslationTaskRecordSink::disabled();
        };
        project_log.run_id().map_or_else(
            || {
                Err((
                    project_log.handle().clone(),
                    project_log.run_id_failure().cloned(),
                ))
            },
            |run_id| {
                Ok((
                    run_id.to_owned(),
                    Arc::clone(project_log.performance()),
                    project_log.handle().clone(),
                ))
            },
        )
    };
    let (run_id, performance, project_log_handle) = match prepared {
        Ok(prepared) => prepared,
        Err((project_log_handle, Some(report))) => {
            project_log_handle.record_task_record_diagnostic(report);
            return ConfiguredTranslationTaskRecordSink::disabled();
        }
        Err((_project_log_handle, None)) => {
            return ConfiguredTranslationTaskRecordSink::disabled();
        }
    };
    match SystemFileSystem::new_with_performance(performance) {
        Ok(file_system) => ConfiguredTranslationTaskRecordSink::Markdown(Box::new(
            MarkdownTranslationTaskRecordSink::new(
                project_workspace.join("task-records").join(&run_id),
                redactor,
                locale,
                cpu,
                file_system,
                project_log_handle.clone(),
            ),
        )),
        Err(error) => {
            project_log_handle.record_task_record_diagnostic(DiagnosticReport::new(
                StateEffect::Unchanged,
                error.diagnostic(),
            ));
            ConfiguredTranslationTaskRecordSink::disabled()
        }
    }
}
