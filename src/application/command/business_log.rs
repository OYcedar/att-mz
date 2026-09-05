//! RPG Maker 翻译与发布事实到项目日志的适配。

use super::error::ProductionCommandError;
use super::progress::{ProductionProgressObserver, TranslateProgressPhase};
use crate::application::TranslationTerminalSummary;
use crate::application::project_log::{ActiveProjectLog, ProjectLogHandle};
use crate::application::termination::TerminationOutcome as DrivenCommand;
use crate::diagnostic::{Diagnostic, DiagnosticReport, RuntimeIssue, SafeText, StateEffect};
use crate::execution::OperationCompletion;
use crate::progress::{ProgressObserver, ProgressSnapshot};
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::translate::pipeline::{
    RpgMakerTranslationLog, RpgMakerTranslationLogEvent, RpgMakerTranslationLogTaskOutcome,
    RpgMakerTranslationRunReport,
};
use crate::rpg_maker::write_back::{
    WriteBackLog, WriteBackLogEvent, WriteBackLogPublicationOutcome,
};
use crate::runtime::project_log::{
    DiagnosticOccurrenceId, DiagnosticScope, ProjectLogEvent, PublicationFinished,
    PublicationSummary, RpgMakerPublicationSummary, RpgMakerTranslationSummary,
    TaskCounterInvariantError, TaskFinishedOutcome, TaskPosition, TranslationEngineSummary,
    TranslationFinished, TranslationTaskCounters,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct ProductionBusinessLog {
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
    pub(super) fn from_active(project_log: &ActiveProjectLog) -> Self {
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

    pub(super) fn for_translation(
        project_log: &ActiveProjectLog,
        progress: ProductionProgressObserver<TranslateProgressPhase>,
    ) -> Self {
        Self {
            translation_progress: Some(progress),
            ..Self::from_active(project_log)
        }
    }

    pub(super) fn emit_retry_summary(&self) {
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

    pub(super) fn translation_counters(&self) -> Result<TranslationTaskCounters, DiagnosticReport> {
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

    pub(super) fn translation_summary(output: &TranslateOutput) -> TranslationEngineSummary {
        let summary = output.summary;
        TranslationEngineSummary::RpgMaker(RpgMakerTranslationSummary {
            accepted_decisions: usize_to_u64(summary.accepted_decisions, "已接受决策数"),
            written_locations: usize_to_u64(summary.written_locations, "已写入位置数"),
            remaining_decisions: usize_to_u64(summary.remaining_decisions, "剩余决策数"),
            remaining_locations: usize_to_u64(summary.remaining_locations, "剩余位置数"),
            rejected_locations: usize_to_u64(summary.rejected_locations, "Rejected 位置数"),
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

    pub(super) fn translation_run_summary(
        summary: &RpgMakerTranslationRunReport,
    ) -> TranslationEngineSummary {
        TranslationEngineSummary::RpgMaker(RpgMakerTranslationSummary {
            accepted_decisions: usize_to_u64(summary.accepted_decisions(), "已接受决策数"),
            written_locations: usize_to_u64(summary.written_locations(), "已写入位置数"),
            remaining_decisions: usize_to_u64(summary.unresolved_decisions(), "剩余决策数"),
            remaining_locations: usize_to_u64(summary.unresolved_locations(), "剩余位置数"),
            rejected_locations: usize_to_u64(summary.rejected_locations(), "Rejected 位置数"),
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

    pub(super) fn current_translation_summary(&self) -> Option<TranslationEngineSummary> {
        self.translation_summary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(Self::translation_run_summary)
    }

    pub(super) fn terminal_translation_summary(&self) -> Option<TranslationTerminalSummary> {
        Some(TranslationTerminalSummary {
            tasks: self.translation_counters().ok()?,
            engine: self.current_translation_summary()?,
        })
    }

    /// 写出 Translate 命令唯一的业务终态；返回失败终态复用的 occurrence。
    pub(super) fn emit_translation_finished(
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
        if completed.is_some()
            && let Some(progress) = &self.translation_progress
        {
            progress.complete_phase(TranslateProgressPhase::ConfirmedTasks);
        }
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

    pub(super) fn has_pending_publication_failure(&self) -> bool {
        self.pending_publication_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    pub(super) fn emit_publication_failure(&self, diagnostic: DiagnosticOccurrenceId) {
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
            RpgMakerTranslationLogEvent::PreparationApplied { report } => {
                *self
                    .translation_summary
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
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
                provider,
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
                        increment_counter(
                            &self.translation_retry_exhausted,
                            retries,
                            "重试耗尽次数",
                        );
                    } else {
                        increment_counter(
                            &self.translation_retry_recovered,
                            retries,
                            "重试恢复次数",
                        );
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
                        provider: provider.map(SafeText::new),
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

pub(super) fn increment_counter(counter: &AtomicU64, amount: u64, name: &'static str) -> u64 {
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

pub(super) fn usize_to_u64(value: usize, name: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{name}必须能用 u64 表达"))
}
