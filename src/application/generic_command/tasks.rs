//! Generic 模型任务的并发请求、自然顺序验收与提交。

use super::diagnostics::{
    GenericCommandError, generic_accepted_task_diagnostics, generic_cpu_execution_failure,
    generic_preparation_failure, generic_response_parse_diagnostic,
    generic_response_problem_diagnostic, generic_task_execution_error_report,
    generic_task_response_diagnostic,
};
use super::lifecycle::{GenericProgressPhase, run_project_blocking};
use super::project_log::{
    GenericTaskProjectLog, GenericTranslateProjectLogStateRef,
    mark_generic_translate_run_plan_saved, update_generic_translate_summary,
};
use super::{GenericTranslationSummary, generic_count};
use crate::diagnostic::{
    DiagnosticReport, GenericDiagnosticStage, GenericTaskResponseProblem, StateEffect,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::llm_request::{
    LlmRequestExecutionOutcome, LlmRequestRetryPolicy, TokioAsyncDelay,
    execute_llm_request_with_retry_observed,
};
use crate::fingerprint::Sha256Fingerprint;
use crate::generic::user_message::render_generic_user_message_with_cancellation;
use crate::generic::{
    CommitTranslationResultsOutcome, CommitTranslationsOutcome, GenericCompiledPlaceholderRules,
    GenericPlaceholderRuleSource, GenericPreparationError, GenericProjectStore,
    GenericTaskRecordDocument, GenericTaskRecordState, GenericUnitMap, GenericValidationFact,
    PlannedTask, RejectedTranslationWrite, TranslationReview, TranslationWrite,
    accept_generic_response_with_cancellation, clone_generic_cpu_text,
    ensure_generic_response_processing_running,
};
use crate::language::LanguageModule;
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmFinishReason, LlmRequestFailure,
};
#[cfg(not(test))]
use crate::progress::ProgressObserver;
use crate::progress::{ProgressSnapshot, TerminalProgressObserver};
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::llm::OpenAiCompatibleExecutor;
use crate::translation::planning_resource::CompiledTerminology;
use crate::translation::task_record::ConfiguredTranslationTaskRecordSink;
use crate::translation_protocol::{
    TranslationResponseMode, parse_translation_response_with_cancellation,
};
use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(super) struct GenericTaskExecution {
    pub(super) store: GenericProjectStore,
    pub(super) expected_raw_fingerprint: Sha256Fingerprint,
    pub(super) profile_id: String,
    pub(super) tasks: Vec<PlannedTask>,
    pub(super) facts: Arc<GenericUnitMap<GenericValidationFact>>,
    pub(super) placeholder_rules: GenericCompiledPlaceholderRules,
    pub(super) placeholder_rule_source: GenericPlaceholderRuleSource,
    pub(super) terminology: Arc<CompiledTerminology>,
    pub(super) language_module: Arc<dyn LanguageModule>,
    pub(super) system_prompt: String,
    pub(super) response_mode: TranslationResponseMode,
    pub(super) client: Arc<crate::runtime::llm::OpenAiCompatibleClient>,
    pub(super) llm: OpenAiCompatibleExecutor,
    pub(super) retry_delays: Vec<Duration>,
    pub(super) max_retry_after: Duration,
    pub(super) cpu: RayonCpuExecutor,
    pub(super) cancellation: CooperativeCancellation,
    pub(super) task_records: ConfiguredTranslationTaskRecordSink,
    pub(super) project_log: GenericTaskProjectLog,
    pub(super) translate_project_log: GenericTranslateProjectLogStateRef,
    pub(super) progress: TerminalProgressObserver<GenericProgressPhase>,
}

#[derive(Clone, Copy)]
pub(super) enum GenericTaskTerminal {
    Complete,
    Partial,
    Unavailable,
    Failed,
    NotCommittedAfterEarlierFailure,
    Cancelled,
}

/// 一次已验收并完成提交尝试的 Generic Task 唯一终态。
///
/// Review 诊断可以随 Complete 一起保留；只有响应问题或提交冲突使终态成为 Partial。
pub(super) struct GenericCommittedTaskFinalResult {
    pub(super) terminal: GenericTaskTerminal,
    pub(super) accepted_output_ids: Vec<usize>,
    pub(super) written_units: usize,
    pub(super) diagnostics: Vec<DiagnosticReport>,
}

impl GenericCommittedTaskFinalResult {
    pub(super) fn new(
        complete: bool,
        accepted_output_ids: Vec<usize>,
        written_units: usize,
        diagnostics: Vec<DiagnosticReport>,
    ) -> Self {
        Self {
            terminal: if complete {
                GenericTaskTerminal::Complete
            } else {
                GenericTaskTerminal::Partial
            },
            accepted_output_ids,
            written_units,
            diagnostics,
        }
    }

    pub(super) const fn is_complete(&self) -> bool {
        matches!(self.terminal, GenericTaskTerminal::Complete)
    }

    pub(super) fn task_record_state(&self) -> GenericTaskRecordState {
        GenericTaskRecordState::committed(
            self.is_complete(),
            self.accepted_output_ids.clone(),
            self.written_units,
            self.diagnostics.clone(),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct GenericTaskSummary {
    pub(super) started_tasks: usize,
    pub(super) not_started_tasks: usize,
    pub(super) complete_tasks: usize,
    pub(super) partial_tasks: usize,
    pub(super) unavailable_tasks: usize,
    pub(super) accepted_units: usize,
    pub(super) written_units: usize,
    pub(super) resolved_rejected_units: usize,
    pub(super) newly_rejected_units: usize,
    pub(super) conflicted_units: usize,
    pub(super) response_problems: usize,
    pub(super) recoverable_request_exhaustions: usize,
    pub(super) request_admission_stopped: bool,
}

pub(super) struct GenericTaskRecordDraft {
    pub(super) task_index: usize,
    pub(super) requested_outputs: usize,
    pub(super) user_message: String,
    pub(super) raw_assistant: Option<Arc<String>>,
    pub(super) provider: Option<String>,
}

pub(super) struct GenericTaskRecordInFlight {
    pub(super) task_index: usize,
    pub(super) requested_outputs: usize,
    pub(super) user_message: String,
}

impl GenericTaskRecordInFlight {
    pub(super) fn finish(
        self,
        raw_assistant: Option<Arc<String>>,
        provider: Option<String>,
    ) -> GenericTaskRecordDraft {
        GenericTaskRecordDraft {
            task_index: self.task_index,
            requested_outputs: self.requested_outputs,
            user_message: self.user_message,
            raw_assistant,
            provider,
        }
    }
}

impl GenericTaskRecordDraft {
    pub(super) fn finish(self, state: GenericTaskRecordState) -> GenericTaskRecordDocument {
        GenericTaskRecordDocument::new(
            self.task_index,
            self.requested_outputs,
            self.user_message,
            self.raw_assistant.map(|assistant| {
                Arc::try_unwrap(assistant).unwrap_or_else(|value| (*value).clone())
            }),
            self.provider,
            state,
        )
    }
}

pub(super) enum GenericPreparedTaskOutcome {
    Accepted {
        writes: Vec<TranslationWrite>,
        rejections: Vec<RejectedTranslationWrite>,
        diagnostics: Vec<DiagnosticReport>,
        finish_review: bool,
        reviews: Vec<TranslationReview>,
        accepted_units: usize,
        response_problems: usize,
        response_complete: bool,
        accepted_output_ids: Vec<usize>,
    },
    Unavailable {
        diagnostic: DiagnosticReport,
        request_exhausted: bool,
        stop_admission: bool,
    },
    Failed {
        error: GenericCommandError,
        preserve_admitted_results: bool,
    },
    Cancelled,
}

impl GenericPreparedTaskOutcome {
    /// 一旦后续任务暴露内部失败或合作取消，先前外部失败授予的“继续提交已准入结果”
    /// 只能收紧，不能被更晚的外部失败重新放宽。
    pub(super) fn blocks_later_commits_after_prior_failure(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                preserve_admitted_results: false,
                ..
            } | Self::Cancelled
        )
    }
}

pub(super) struct GenericPreparedTask {
    pub(super) task_index: usize,
    pub(super) outcome: GenericPreparedTaskOutcome,
    pub(super) record: Option<GenericTaskRecordDraft>,
    pub(super) attempt_count: usize,
    pub(super) provider: Option<String>,
}

pub(super) fn cancelled_generic_prepared_task(
    task_index: usize,
    record: Option<GenericTaskRecordInFlight>,
    raw_assistant: Option<Arc<String>>,
    attempt_count: usize,
    provider: Option<String>,
) -> GenericPreparedTask {
    GenericPreparedTask {
        task_index,
        outcome: GenericPreparedTaskOutcome::Cancelled,
        record: record.map(|record| record.finish(raw_assistant, provider.clone())),
        attempt_count,
        provider,
    }
}

#[derive(Clone)]
pub(super) struct GenericTaskRequestContext {
    pub(super) total_tasks: usize,
    pub(super) facts: Arc<GenericUnitMap<GenericValidationFact>>,
    pub(super) placeholder_rules: GenericCompiledPlaceholderRules,
    pub(super) placeholder_rule_source: GenericPlaceholderRuleSource,
    pub(super) terminology: Arc<CompiledTerminology>,
    pub(super) language_module: Arc<dyn LanguageModule>,
    pub(super) system_prompt: Arc<String>,
    pub(super) response_mode: TranslationResponseMode,
    pub(super) client: Arc<crate::runtime::llm::OpenAiCompatibleClient>,
    pub(super) llm: OpenAiCompatibleExecutor,
    pub(super) retry_delays: Arc<Vec<Duration>>,
    pub(super) max_retry_after: Duration,
    pub(super) cpu: RayonCpuExecutor,
    pub(super) cancellation: CooperativeCancellation,
    pub(super) record_evidence: bool,
    pub(super) admission_stopped: Arc<AtomicBool>,
    pub(super) project_log: GenericTaskProjectLog,
}

async fn execute_owned_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let render_terminology = Arc::clone(&context.terminology);
    let render_system_prompt = Arc::clone(&context.system_prompt);
    let render_cancellation = context.cancellation.clone();
    let (task, system_prompt, user_message) = context
        .cpu
        .execute(move || {
            let user_message = render_generic_user_message_with_cancellation(
                &task,
                render_terminology.as_ref(),
                &render_cancellation,
            )
            .map_err(GenericPreparationError::Planning)?;
            let system_prompt =
                clone_generic_cpu_text(render_system_prompt.as_str(), &render_cancellation)?;
            Ok::<_, GenericPreparationError>((task, system_prompt, user_message))
        })
        .await
        .map_err(generic_cpu_execution_failure)?
        .map_err(|source| {
            if source.is_cancelled() {
                GenericCommandError::Cancelled
            } else {
                generic_preparation_failure(source)
            }
        })?;
    execute_generic_task(
        context.total_tasks,
        task_index,
        task,
        user_message,
        Arc::clone(&context.facts),
        context.placeholder_rules.clone(),
        context.placeholder_rule_source.clone(),
        Arc::clone(&context.language_module),
        system_prompt,
        context.response_mode,
        context.client.as_ref(),
        &context.llm,
        context.retry_delays.as_slice(),
        context.max_retry_after,
        context.cpu.clone(),
        context.cancellation.clone(),
        context.record_evidence,
        Arc::clone(&context.admission_stopped),
        context.project_log.clone(),
    )
    .await
}

async fn execute_indexed_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> (usize, Result<GenericPreparedTask, GenericCommandError>) {
    let result = execute_owned_generic_task(context, task_index, task).await;
    (task_index, result)
}

pub(super) async fn execute_generic_tasks(
    input: GenericTaskExecution,
) -> Result<GenericTaskSummary, GenericCommandError> {
    let GenericTaskExecution {
        store,
        expected_raw_fingerprint,
        profile_id,
        tasks,
        facts,
        placeholder_rules,
        placeholder_rule_source,
        terminology,
        language_module,
        system_prompt,
        response_mode,
        client,
        llm,
        retry_delays,
        max_retry_after,
        cpu,
        cancellation,
        task_records,
        project_log,
        translate_project_log,
        progress,
    } = input;
    let total_tasks = tasks.len();
    let record_evidence = task_records.enabled();
    let concurrency = client.max_concurrent_requests().get();
    let request_context = GenericTaskRequestContext {
        total_tasks,
        facts,
        placeholder_rules,
        placeholder_rule_source,
        terminology,
        language_module,
        system_prompt: Arc::new(system_prompt),
        response_mode,
        client,
        llm,
        retry_delays: Arc::new(retry_delays),
        max_retry_after,
        cpu,
        cancellation: cancellation.clone(),
        record_evidence,
        admission_stopped: Arc::new(AtomicBool::new(false)),
        project_log: project_log.clone(),
    };
    let mut remaining = tasks.into_iter().enumerate();
    let mut tasks = FuturesOrdered::new();
    for _ in 0..concurrency {
        let Some((task_index, task)) = remaining.next() else {
            break;
        };
        tasks.push_back(execute_indexed_generic_task(
            request_context.clone(),
            task_index,
            task,
        ));
    }

    let mut summary = GenericTaskSummary::default();
    let mut terminal_error = None;
    let mut preserve_admitted_results_after_error = false;
    let mut admission_stopped = false;
    while let Some((scheduled_task_index, prepared)) = tasks.next().await {
        let GenericPreparedTask {
            task_index,
            outcome,
            record,
            attempt_count,
            provider,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let report =
                    generic_task_execution_error_report(&error, scheduled_task_index, total_tasks);
                project_log.finished(
                    scheduled_task_index,
                    0,
                    None,
                    if error.is_cancelled() {
                        GenericTaskTerminal::Cancelled
                    } else {
                        GenericTaskTerminal::Failed
                    },
                    (!error.is_cancelled()).then_some(report),
                );
                if terminal_error.is_none() {
                    cancellation.request();
                    terminal_error = Some(error);
                }
                continue;
            }
        };
        if terminal_error.is_some()
            && !(preserve_admitted_results_after_error
                && matches!(&outcome, GenericPreparedTaskOutcome::Accepted { .. }))
        {
            if outcome.blocks_later_commits_after_prior_failure() {
                preserve_admitted_results_after_error = false;
                cancellation.request();
            }
            let prior_was_cancelled = terminal_error
                .as_ref()
                .is_some_and(GenericCommandError::is_cancelled);
            match &outcome {
                GenericPreparedTaskOutcome::Cancelled => project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Cancelled,
                    std::iter::empty(),
                ),
                GenericPreparedTaskOutcome::Unavailable { diagnostic, .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Unavailable,
                    [diagnostic.clone()],
                ),
                GenericPreparedTaskOutcome::Accepted { .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    if prior_was_cancelled {
                        GenericTaskTerminal::Cancelled
                    } else {
                        GenericTaskTerminal::NotCommittedAfterEarlierFailure
                    },
                    std::iter::empty(),
                ),
                GenericPreparedTaskOutcome::Failed { error, .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Failed,
                    [generic_task_execution_error_report(
                        error,
                        task_index,
                        total_tasks,
                    )],
                ),
            }
            if let Some(record) = record {
                let state = match outcome {
                    GenericPreparedTaskOutcome::Cancelled => GenericTaskRecordState::cancelled(),
                    GenericPreparedTaskOutcome::Unavailable { diagnostic, .. } => {
                        GenericTaskRecordState::unavailable(diagnostic)
                    }
                    GenericPreparedTaskOutcome::Accepted {
                        accepted_output_ids,
                        diagnostics,
                        finish_review,
                        reviews,
                        ..
                    } => {
                        let diagnostics = generic_accepted_task_diagnostics(
                            task_index,
                            total_tasks,
                            finish_review,
                            diagnostics,
                            reviews,
                            None,
                        );
                        if prior_was_cancelled {
                            GenericTaskRecordState::cancelled_after_acceptance(
                                accepted_output_ids,
                                diagnostics,
                            )
                        } else {
                            GenericTaskRecordState::not_committed_due_to_prior_failure(
                                accepted_output_ids,
                                diagnostics,
                            )
                        }
                    }
                    GenericPreparedTaskOutcome::Failed { ref error, .. } => {
                        GenericTaskRecordState::failed(generic_task_execution_error_report(
                            error,
                            task_index,
                            total_tasks,
                        ))
                    }
                };
                task_records.submit(record.finish(state));
            }
            continue;
        }
        match outcome {
            GenericPreparedTaskOutcome::Accepted {
                writes,
                rejections,
                diagnostics,
                finish_review,
                reviews,
                accepted_units,
                response_problems,
                response_complete,
                accepted_output_ids,
            } => {
                let commit = if writes.is_empty() && rejections.is_empty() {
                    Ok(CommitTranslationResultsOutcome {
                        committed: 0,
                        rejected: 0,
                        resolved_rejected: 0,
                        newly_rejected: 0,
                        conflicts: Vec::new(),
                    })
                } else {
                    let database_path = store.database_path().to_path_buf();
                    let store = store.clone();
                    let profile_id = profile_id.clone();
                    run_project_blocking(
                        GenericDiagnosticStage::Translate,
                        StateEffect::ProgressPreserved,
                        database_path,
                        move || {
                            store.commit_translation_results_for_profile(
                                expected_raw_fingerprint,
                                &writes,
                                &rejections,
                                &profile_id,
                            )
                        },
                    )
                    .await
                };
                let commit = match commit {
                    Ok(commit) => commit,
                    Err(error) => {
                        let report =
                            generic_task_execution_error_report(&error, task_index, total_tasks);
                        project_log.finished(
                            task_index,
                            attempt_count,
                            provider.as_deref(),
                            GenericTaskTerminal::Failed,
                            [report.clone()],
                        );
                        if let Some(record) = record {
                            let mut diagnostics = generic_accepted_task_diagnostics(
                                task_index,
                                total_tasks,
                                finish_review,
                                diagnostics,
                                reviews,
                                None,
                            );
                            diagnostics.push(report.clone());
                            task_records.submit(record.finish(
                                GenericTaskRecordState::failed_after_acceptance(
                                    accepted_output_ids,
                                    diagnostics,
                                ),
                            ));
                        }
                        cancellation.request();
                        preserve_admitted_results_after_error = false;
                        terminal_error.get_or_insert(error);
                        continue;
                    }
                };
                if commit.committed > 0 || commit.rejected > 0 {
                    mark_generic_translate_run_plan_saved(&translate_project_log);
                }
                summary.accepted_units += accepted_units;
                summary.response_problems += response_problems;
                summary.written_units += commit.committed;
                summary.resolved_rejected_units += commit.resolved_rejected;
                summary.newly_rejected_units += commit.newly_rejected;
                summary.conflicted_units += commit.conflicts.len();
                update_generic_translate_summary(&translate_project_log, |stored| {
                    stored.accepted_units += accepted_units;
                    stored.response_problems += response_problems;
                    stored.written_units += commit.committed;
                    stored.remaining_units = stored
                        .remaining_units
                        .checked_sub(commit.committed)
                        .expect("Generic 已写入模型 Unit 不得超过计划 Unit");
                    stored.rejected_units = stored
                        .rejected_units
                        .checked_sub(commit.resolved_rejected)
                        .and_then(|value| value.checked_add(commit.newly_rejected))
                        .expect("Generic Task 的 Rejected 终态计数必须保持有效");
                    stored.conflicted_units += commit.conflicts.len();
                });
                let mut diagnostics = generic_accepted_task_diagnostics(
                    task_index,
                    total_tasks,
                    finish_review,
                    diagnostics,
                    reviews,
                    Some(&commit),
                );
                if !commit.conflicts.is_empty() {
                    diagnostics.push(generic_task_response_diagnostic(
                        task_index,
                        total_tasks,
                        GenericTaskResponseProblem::CommitConflict {
                            count: generic_count(commit.conflicts.len()),
                        },
                    ));
                }
                let final_result = GenericCommittedTaskFinalResult::new(
                    response_complete && commit.conflicts.is_empty(),
                    accepted_output_ids,
                    commit.committed,
                    diagnostics,
                );
                if final_result.is_complete() {
                    summary.complete_tasks += 1;
                } else {
                    summary.partial_tasks += 1;
                }
                update_generic_translate_summary(&translate_project_log, |stored| {
                    if final_result.is_complete() {
                        stored.complete_tasks += 1;
                    } else {
                        stored.partial_tasks += 1;
                    }
                });
                project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    final_result.terminal,
                    final_result.diagnostics.clone(),
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(final_result.task_record_state()));
                }
            }
            GenericPreparedTaskOutcome::Unavailable {
                diagnostic,
                request_exhausted,
                stop_admission,
            } => {
                project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Unavailable,
                    [diagnostic.clone()],
                );
                if let Some(record) = record {
                    task_records
                        .submit(record.finish(GenericTaskRecordState::unavailable(diagnostic)));
                }
                if attempt_count > 0 {
                    summary.unavailable_tasks += 1;
                    summary.recoverable_request_exhaustions += usize::from(request_exhausted);
                }
                summary.request_admission_stopped |= stop_admission;
                admission_stopped |= stop_admission;
                update_generic_translate_summary(&translate_project_log, |stored| {
                    if attempt_count > 0 {
                        stored.unavailable_tasks += 1;
                        stored.recoverable_request_exhaustions += usize::from(request_exhausted);
                    }
                    stored.request_admission_stopped |= stop_admission;
                });
            }
            GenericPreparedTaskOutcome::Failed {
                error,
                preserve_admitted_results,
            } => {
                let diagnostic =
                    generic_task_execution_error_report(&error, task_index, total_tasks);
                project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Failed,
                    [diagnostic.clone()],
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::failed(diagnostic)));
                }
                if !preserve_admitted_results {
                    cancellation.request();
                } else {
                    summary.request_admission_stopped = true;
                    admission_stopped = true;
                    update_generic_translate_summary(&translate_project_log, |stored| {
                        stored.request_admission_stopped = true;
                    });
                }
                preserve_admitted_results_after_error = preserve_admitted_results;
                terminal_error = Some(error);
            }
            GenericPreparedTaskOutcome::Cancelled => {
                project_log.finished(
                    task_index,
                    attempt_count,
                    provider.as_deref(),
                    GenericTaskTerminal::Cancelled,
                    std::iter::empty(),
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::cancelled()));
                }
                cancellation.request();
                terminal_error = Some(GenericCommandError::Cancelled);
            }
        }
        if terminal_error.is_none() {
            let confirmed =
                summary.complete_tasks + summary.partial_tasks + summary.unavailable_tasks;
            progress.observe(ProgressSnapshot::determinate(
                GenericProgressPhase::ConfirmedTasks,
                generic_count(confirmed),
                generic_count(total_tasks),
            ));
        }
        if terminal_error.is_none()
            && !admission_stopped
            && !request_context.admission_stopped.load(Ordering::Acquire)
            && let Some((task_index, task)) = remaining.next()
        {
            tasks.push_back(execute_indexed_generic_task(
                request_context.clone(),
                task_index,
                task,
            ));
        }
    }
    match terminal_error {
        Some(error) => Err(error),
        None => {
            summary.started_tasks =
                summary.complete_tasks + summary.partial_tasks + summary.unavailable_tasks;
            summary.not_started_tasks = total_tasks.saturating_sub(summary.started_tasks);
            Ok(summary)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_generic_task(
    total_tasks: usize,
    task_index: usize,
    task: PlannedTask,
    user_message: String,
    facts: Arc<GenericUnitMap<GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    placeholder_rule_source: GenericPlaceholderRuleSource,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_mode: TranslationResponseMode,
    client: &crate::runtime::llm::OpenAiCompatibleClient,
    llm: &OpenAiCompatibleExecutor,
    retry_delays: &[Duration],
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
    admission_stopped: Arc<AtomicBool>,
    project_log: GenericTaskProjectLog,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let requested_outputs = task.expected_output_count();
    let recorded_user_message = record_evidence.then(|| user_message.clone());
    let messages = [
        ChatMessage::new(ChatMessageRole::System, system_prompt),
        ChatMessage::new(ChatMessageRole::User, user_message),
    ];
    let execution = execute_llm_request_with_retry_observed(
        llm,
        client,
        &messages,
        LlmRequestRetryPolicy::new(retry_delays, max_retry_after),
        &TokioAsyncDelay,
        &cancellation,
        move || project_log.started(task_index),
    )
    .await;
    let (outcome, evidence) = execution.into_parts();
    let stops_admission = match &outcome {
        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum { service_status, .. }
        | LlmRequestExecutionOutcome::RetryBudgetExhausted { service_status, .. } => {
            service_status.stops_admission_after_unavailable()
        }
        LlmRequestExecutionOutcome::Fatal { source, .. } => source.service_status().is_permanent(),
        LlmRequestExecutionOutcome::AdmissionStopped { .. } => true,
        LlmRequestExecutionOutcome::Response { .. } | LlmRequestExecutionOutcome::Cancelled => {
            false
        }
    };
    if stops_admission {
        admission_stopped.store(true, Ordering::Release);
    }
    let attempt_count = evidence.attempt_count();
    let provider = evidence.provider().map(str::to_owned);
    let record = (attempt_count > 0)
        .then_some(recorded_user_message)
        .flatten()
        .map(|user_message| GenericTaskRecordInFlight {
            task_index,
            requested_outputs,
            user_message,
        });
    let response_record = record.as_ref().and_then(|_| match &outcome {
        LlmRequestExecutionOutcome::Response { response, .. } => Some(response.shared_content()),
        _ => None,
    });
    let response_cancellation = cancellation.clone();
    let processing = cpu
        .execute(move || {
            ensure_generic_response_processing_running(&response_cancellation)?;
            let outcome = match outcome {
                LlmRequestExecutionOutcome::Response { response, .. } => {
                    let (content, finish_reason) = response.into_content_and_finish_reason();
                    ensure_generic_response_processing_running(&response_cancellation)?;
                    let finish_review = !matches!(finish_reason, LlmFinishReason::Stop);
                    match parse_translation_response_with_cancellation(
                        &content,
                        response_mode,
                        || ensure_generic_response_processing_running(&response_cancellation),
                    )? {
                        Ok(parsed) => {
                            ensure_generic_response_processing_running(&response_cancellation)?;
                            let acceptance = accept_generic_response_with_cancellation(
                                task,
                                &parsed,
                                facts.as_ref(),
                                &placeholder_rules,
                                &placeholder_rule_source,
                                language_module.as_ref(),
                                &response_cancellation,
                            )?;
                            let accepted_output_ids = acceptance
                                .accepted_output_ids()
                                .iter()
                                .map(|id| id.get())
                                .collect();
                            let (accepted, rejected, problems, reviews) = acceptance.into_parts();
                            let accepted_units = accepted.len();
                            let response_problems = problems.len();
                            let response_complete = problems.is_empty();
                            let mut diagnostics = Vec::with_capacity(problems.len());
                            for problem in &problems {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                diagnostics.push(generic_response_problem_diagnostic(
                                    task_index,
                                    total_tasks,
                                    problem,
                                ));
                            }
                            ensure_generic_response_processing_running(&response_cancellation)?;
                            let mut writes = Vec::with_capacity(accepted.len());
                            for accepted in accepted {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                writes.push(accepted.into_write());
                            }
                            let mut rejections = Vec::with_capacity(rejected.len());
                            for rejected in rejected {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                rejections.push(rejected.into_write());
                            }
                            GenericPreparedTaskOutcome::Accepted {
                                writes,
                                rejections,
                                diagnostics,
                                finish_review,
                                reviews,
                                accepted_units,
                                response_problems,
                                response_complete,
                                accepted_output_ids,
                            }
                        }
                        Err(error) => {
                            ensure_generic_response_processing_running(&response_cancellation)?;
                            let diagnostic =
                                generic_response_parse_diagnostic(task_index, total_tasks, error);
                            GenericPreparedTaskOutcome::Unavailable {
                                diagnostic,
                                request_exhausted: false,
                                stop_admission: false,
                            }
                        }
                    }
                }
                LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
                    diagnostic,
                    service_status,
                    ..
                } => GenericPreparedTaskOutcome::Unavailable {
                    diagnostic,
                    request_exhausted: true,
                    stop_admission: service_status.stops_admission_after_unavailable(),
                },
                LlmRequestExecutionOutcome::RetryBudgetExhausted {
                    diagnostic,
                    service_status,
                    ..
                } => GenericPreparedTaskOutcome::Unavailable {
                    diagnostic,
                    request_exhausted: true,
                    stop_admission: service_status.stops_admission_after_unavailable(),
                },
                LlmRequestExecutionOutcome::Fatal {
                    source, diagnostic, ..
                } => GenericPreparedTaskOutcome::Failed {
                    error: GenericCommandError::reported(source, diagnostic),
                    preserve_admitted_results: true,
                },
                LlmRequestExecutionOutcome::AdmissionStopped { diagnostic } => {
                    GenericPreparedTaskOutcome::Unavailable {
                        diagnostic,
                        request_exhausted: false,
                        stop_admission: true,
                    }
                }
                LlmRequestExecutionOutcome::Cancelled => GenericPreparedTaskOutcome::Cancelled,
            };
            Ok::<_, GenericPreparationError>(outcome)
        })
        .await;
    let outcome = match processing {
        Err(CpuTaskExecutionError::Cancelled) => {
            return Ok(cancelled_generic_prepared_task(
                task_index,
                record,
                response_record,
                attempt_count,
                provider,
            ));
        }
        Err(source) => {
            let error = generic_cpu_execution_failure(source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed {
                    error,
                    preserve_admitted_results: false,
                },
                record: record.map(|record| record.finish(response_record, provider.clone())),
                attempt_count,
                provider,
            });
        }
        Ok(Err(source)) if source.is_cancelled() => {
            return Ok(cancelled_generic_prepared_task(
                task_index,
                record,
                response_record,
                attempt_count,
                provider,
            ));
        }
        Ok(Err(source)) => {
            let error = generic_preparation_failure(source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed {
                    error,
                    preserve_admitted_results: false,
                },
                record: record.map(|record| record.finish(response_record, provider.clone())),
                attempt_count,
                provider,
            });
        }
        Ok(Ok(processed)) => processed,
    };
    Ok(GenericPreparedTask {
        task_index,
        outcome,
        record: record.map(|record| record.finish(response_record, provider.clone())),
        attempt_count,
        provider,
    })
}

pub(super) fn add_commit_outcome(
    summary: &mut GenericTranslationSummary,
    outcome: &CommitTranslationsOutcome,
) {
    summary.written_units += outcome.committed;
    summary.remaining_units = summary
        .remaining_units
        .checked_sub(outcome.resolved_rejected)
        .expect("复用修复的 Generic Rejected Unit 不得超过剩余 Unit");
    summary.rejected_units = summary
        .rejected_units
        .checked_sub(outcome.resolved_rejected)
        .expect("复用修复的 Generic Rejected Unit 不得超过当前 Rejected");
    summary.conflicted_units += outcome.conflicts.len();
}

pub(super) fn should_remember_profile_separately(summary: &GenericTranslationSummary) -> bool {
    summary.written_units == 0
}

pub(super) fn merge_task_summary(
    summary: &mut GenericTranslationSummary,
    tasks: GenericTaskSummary,
) {
    summary.started_tasks += tasks.started_tasks;
    summary.not_started_tasks += tasks.not_started_tasks;
    summary.complete_tasks += tasks.complete_tasks;
    summary.partial_tasks += tasks.partial_tasks;
    summary.unavailable_tasks += tasks.unavailable_tasks;
    summary.accepted_units += tasks.accepted_units;
    summary.written_units += tasks.written_units;
    summary.rejected_units = summary
        .rejected_units
        .checked_sub(tasks.resolved_rejected_units)
        .and_then(|value| value.checked_add(tasks.newly_rejected_units))
        .expect("Generic Task 的 Rejected 终态计数必须保持有效");
    summary.conflicted_units += tasks.conflicted_units;
    summary.response_problems += tasks.response_problems;
    summary.remaining_units = summary
        .remaining_units
        .checked_sub(tasks.written_units)
        .expect("Generic 已写入模型 Unit 不得超过计划 Unit");
    summary.recoverable_request_exhaustions += tasks.recoverable_request_exhaustions;
    summary.request_admission_stopped |= tasks.request_admission_stopped;
}
