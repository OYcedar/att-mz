//! RPG Maker 翻译任务的可读、非权威旁路记录。
//!
//! 一个已开始 TaskBlock 最多形成一个不可变 Markdown 文档。模型请求、响应解析、
//! 逐 ID 验收和数据库提交仍分别由原有语义所有者负责；本模块只接收它们建立的
//! 确定事实并呈现，不参与恢复、重放、验收、提交或退出码判断。

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use serde_json::value::RawValue;
use time::OffsetDateTime;

use crate::diagnostic::{DiagnosticReport, render_diagnostic_report};
pub(crate) use crate::execution::llm_request::LlmRequestAttemptRecord as TranslationTaskAttemptRecord;
#[cfg(test)]
use crate::execution::llm_request::{
    LlmRequestAttemptOutcome as TranslationTaskAttemptOutcome,
    LlmRequestRetryWaitRecord as TranslationTaskRetryWaitRecord,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ApiKeyRedactor, ChatMessageRole, LlmClientRecordMetadata};
#[cfg(test)]
use crate::llm::{LlmFinishReason, LlmResponse, LlmUsage};
#[cfg(test)]
use crate::runtime::filesystem::SystemFileSystem;
#[cfg(test)]
use crate::translation::task_planning::TaskId;
pub(crate) use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::translation::task_record::{
    TranslationTaskRecordArtifact, markdown_fence, render_json_repairs, render_readable_assistant,
    render_task_record_attempt,
};
#[cfg(test)]
pub(crate) use crate::translation_protocol::TranslationTaskResponseJsonErrorCategory;
pub(crate) use crate::translation_protocol::{
    TranslationResponseRepair, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};

use super::pipeline::{
    RpgMakerExecutableTask, RpgMakerTranslationTaskIndex, TranslationTaskOutcome,
};
#[cfg(test)]
use super::pipeline::{
    TranslationProtocolDiagnostic, TranslationTaskUnavailableReason, TranslationUnitRejectionReason,
};

/// RPG Maker 响应值不满足字符串数组形状时的精确原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationAssistantValueError {
    NotStringArray,
    NonStringItem { item: NonZeroUsize },
    SourceEchoNotObject,
    SourceEchoMissingSource,
    SourceEchoMissingTranslation,
    SourceEchoDuplicateSource,
    SourceEchoDuplicateTranslation,
    SourceEchoUnexpectedField,
    SourceNotStringArray,
    SourceNonStringItem { item: NonZeroUsize },
}

/// 唯一响应解析器建立的任务记录投影。
#[derive(Debug)]
pub(crate) struct TranslationTaskResponseRecord {
    raw_assistant: Arc<String>,
    strict_json: bool,
    repairs: Vec<TranslationResponseRepair>,
    parse_error: Option<TranslationTaskResponseParseError>,
}

impl TranslationTaskResponseRecord {
    #[cfg(test)]
    pub(crate) fn parsed(raw_assistant: impl Into<Arc<String>>) -> Self {
        Self::parsed_with_repairs(raw_assistant, Vec::new())
    }

    pub(crate) fn parsed_with_repairs(
        raw_assistant: impl Into<Arc<String>>,
        repairs: Vec<TranslationResponseRepair>,
    ) -> Self {
        let strict_json = repairs.is_empty();
        Self {
            raw_assistant: raw_assistant.into(),
            strict_json,
            repairs,
            parse_error: None,
        }
    }

    pub(crate) fn invalid(
        raw_assistant: impl Into<Arc<String>>,
        parse_error: TranslationTaskResponseParseError,
    ) -> Self {
        Self {
            raw_assistant: raw_assistant.into(),
            strict_json: false,
            repairs: Vec::new(),
            parse_error: Some(parse_error),
        }
    }

    pub(crate) fn unprocessed(raw_assistant: impl Into<Arc<String>>) -> Self {
        Self {
            raw_assistant: raw_assistant.into(),
            strict_json: false,
            repairs: Vec::new(),
            parse_error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn raw_assistant(&self) -> &str {
        self.raw_assistant.as_str()
    }

    #[cfg(test)]
    pub(crate) const fn is_strict_json(&self) -> bool {
        self.strict_json
    }

    #[cfg(test)]
    pub(crate) const fn parse_error(&self) -> Option<TranslationTaskResponseParseError> {
        self.parse_error
    }
}

/// 一个已启动任务从开始到 Executor 返回的不可变执行证据。
#[derive(Debug)]
pub(crate) struct TranslationTaskExecutionEvidence {
    started_at: Option<OffsetDateTime>,
    task_started: Option<Instant>,
    fixed_duration: Duration,
    attempt_count: usize,
    attempts: Vec<TranslationTaskAttemptRecord>,
    response: Option<TranslationTaskResponseRecord>,
}

impl TranslationTaskExecutionEvidence {
    #[cfg(test)]
    pub(crate) fn new(
        started_at: OffsetDateTime,
        duration: Duration,
        attempts: Vec<TranslationTaskAttemptRecord>,
        response: Option<TranslationTaskResponseRecord>,
    ) -> Self {
        let attempt_count = attempts.len();
        Self {
            started_at: Some(started_at),
            task_started: None,
            fixed_duration: duration,
            attempt_count,
            attempts,
            response,
        }
    }

    pub(crate) fn from_execution(
        started_at: Option<OffsetDateTime>,
        task_started: Option<Instant>,
        attempt_count: usize,
        attempts: Vec<TranslationTaskAttemptRecord>,
        response: Option<TranslationTaskResponseRecord>,
    ) -> Self {
        debug_assert_eq!(started_at.is_some(), task_started.is_some());
        debug_assert!(started_at.is_some() || attempts.is_empty());
        debug_assert!(started_at.is_some() || response.is_none());
        Self {
            started_at,
            task_started,
            fixed_duration: Duration::ZERO,
            attempt_count,
            attempts,
            response,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(attempts: NonZeroUsize) -> Self {
        Self {
            started_at: None,
            task_started: None,
            fixed_duration: Duration::ZERO,
            attempt_count: attempts.get(),
            attempts: Vec::new(),
            response: None,
        }
    }

    pub(crate) const fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    fn started_at(&self) -> OffsetDateTime {
        self.started_at.unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }

    fn elapsed_until_finalization(&self) -> Duration {
        self.task_started
            .map_or(self.fixed_duration, |started| started.elapsed())
    }

    #[cfg(test)]
    pub(crate) fn response(&self) -> Option<&TranslationTaskResponseRecord> {
        self.response.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn has_recorded_payload(&self) -> bool {
        self.started_at.is_some()
            || self.task_started.is_some()
            || !self.attempts.is_empty()
            || self.response.is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_cancelled_retry_wait(&self) -> bool {
        self.attempts.iter().any(|attempt| {
            matches!(
                &attempt.outcome,
                TranslationTaskAttemptOutcome::Retryable {
                    retry_wait: Some(TranslationTaskRetryWaitRecord::CancelledWhileWaiting { .. }),
                    ..
                }
            )
        })
    }
}

/// Executor 的正常结果及其旁路证据。
#[derive(Debug)]
pub(crate) struct TranslationTaskExecution {
    outcome: TranslationTaskOutcome,
    evidence: TranslationTaskExecutionEvidence,
}

impl TranslationTaskExecution {
    pub(crate) fn new(
        outcome: TranslationTaskOutcome,
        evidence: TranslationTaskExecutionEvidence,
    ) -> Self {
        Self { outcome, evidence }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(outcome: TranslationTaskOutcome) -> Self {
        let evidence = TranslationTaskExecutionEvidence::synthetic(outcome.attempts());
        Self { outcome, evidence }
    }

    pub(crate) fn into_parts(self) -> (TranslationTaskOutcome, TranslationTaskExecutionEvidence) {
        (self.outcome, self.evidence)
    }
}

/// Executor 技术失败及其已经建立的旁路证据。
///
/// 未取消的失败在仍掌握原始边界事实时必须已经形成诊断；取消是独立终态，不能用
/// `None` 伪装成普通失败。
#[derive(Debug)]
pub(crate) enum TranslationTaskExecutionFailure<E> {
    Failed {
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: DiagnosticReport,
    },
    Cancelled {
        source: E,
        evidence: TranslationTaskExecutionEvidence,
    },
}

impl<E> TranslationTaskExecutionFailure<E> {
    pub(crate) fn failed(
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: DiagnosticReport,
    ) -> Self {
        Self::Failed {
            source,
            evidence,
            diagnostic,
        }
    }

    pub(crate) fn cancelled(source: E, evidence: TranslationTaskExecutionEvidence) -> Self {
        Self::Cancelled { source, evidence }
    }
}

/// 提交失败仍持有类型化存储事实时建立的窄小投影。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskCommitFailureImpact {
    NotApplied,
    OutcomeUnknown,
}

#[derive(Debug)]
pub(crate) struct TranslationTaskCommitFailure<E> {
    source: E,
    impact: TranslationTaskCommitFailureImpact,
    diagnostic: DiagnosticReport,
}

impl<E> TranslationTaskCommitFailure<E> {
    pub(crate) fn new(
        source: E,
        impact: TranslationTaskCommitFailureImpact,
        diagnostic: DiagnosticReport,
    ) -> Self {
        Self {
            source,
            impact,
            diagnostic,
        }
    }

    pub(crate) fn not_applied(source: E, diagnostic: DiagnosticReport) -> Self {
        Self::new(
            source,
            TranslationTaskCommitFailureImpact::NotApplied,
            diagnostic,
        )
    }

    pub(crate) fn into_parts(self) -> (E, TranslationTaskCommitFailureImpact, DiagnosticReport) {
        (self.source, self.impact, self.diagnostic)
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &E {
        &self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskCommitPhase {
    Preparation,
    Transaction,
}

/// 一个已启动任务在顺序最终化边界确认的唯一终态。
pub(crate) enum TranslationTaskRecordFinalState {
    CompleteCommitted {
        outcome: Arc<TranslationTaskOutcome>,
    },
    PartialCommitted {
        outcome: Arc<TranslationTaskOutcome>,
    },
    UnavailableNoChanges {
        outcome: Arc<TranslationTaskOutcome>,
    },
    ExecutionFailedNoChanges {
        diagnostic: DiagnosticReport,
    },
    CommitNotApplied {
        outcome: Arc<TranslationTaskOutcome>,
        phase: TranslationTaskCommitPhase,
        diagnostic: DiagnosticReport,
    },
    CommitOutcomeUnknown {
        outcome: Arc<TranslationTaskOutcome>,
        diagnostic: DiagnosticReport,
    },
    NotCommittedAfterEarlierFailure {
        outcome: Arc<TranslationTaskOutcome>,
        diagnostic: DiagnosticReport,
    },
    InvalidResultNoChanges {
        outcome: Arc<TranslationTaskOutcome>,
        diagnostic: DiagnosticReport,
    },
    CancelledNoChanges {
        outcome: Option<Arc<TranslationTaskOutcome>>,
    },
}

impl TranslationTaskRecordFinalState {
    fn outcome_kind_matches_state(&self) -> bool {
        match self {
            Self::CompleteCommitted { outcome } => {
                matches!(outcome.as_ref(), TranslationTaskOutcome::Complete { .. })
            }
            Self::PartialCommitted { outcome } => {
                matches!(outcome.as_ref(), TranslationTaskOutcome::Partial { .. })
            }
            Self::UnavailableNoChanges { outcome } => {
                matches!(outcome.as_ref(), TranslationTaskOutcome::Unavailable { .. })
            }
            Self::ExecutionFailedNoChanges { .. }
            | Self::CommitNotApplied { .. }
            | Self::CommitOutcomeUnknown { .. }
            | Self::NotCommittedAfterEarlierFailure { .. }
            | Self::InvalidResultNoChanges { .. }
            | Self::CancelledNoChanges { .. } => true,
        }
    }

    fn outcome(&self) -> Option<&TranslationTaskOutcome> {
        match self {
            Self::CompleteCommitted { outcome }
            | Self::PartialCommitted { outcome }
            | Self::UnavailableNoChanges { outcome }
            | Self::CommitNotApplied { outcome, .. }
            | Self::CommitOutcomeUnknown { outcome, .. }
            | Self::NotCommittedAfterEarlierFailure { outcome, .. }
            | Self::InvalidResultNoChanges { outcome, .. } => Some(outcome),
            Self::ExecutionFailedNoChanges { .. } => None,
            Self::CancelledNoChanges { outcome } => outcome.as_deref(),
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::CompleteCommitted { .. } => "complete",
            Self::PartialCommitted { .. } => "partial",
            Self::UnavailableNoChanges { .. } => "unavailable",
            Self::ExecutionFailedNoChanges { .. } => "execution_failed",
            Self::CommitNotApplied {
                phase: TranslationTaskCommitPhase::Preparation,
                ..
            } => "commit_preparation_failed",
            Self::CommitNotApplied {
                phase: TranslationTaskCommitPhase::Transaction,
                ..
            } => "commit_not_applied",
            Self::CommitOutcomeUnknown { .. } => "commit_outcome_unknown",
            Self::NotCommittedAfterEarlierFailure { .. } => "not_committed_after_earlier_failure",
            Self::InvalidResultNoChanges { .. } => "invalid_result",
            Self::CancelledNoChanges { .. } => "cancelled",
        }
    }

    fn diagnostic(&self) -> Option<&DiagnosticReport> {
        match self {
            Self::ExecutionFailedNoChanges { diagnostic }
            | Self::CommitNotApplied { diagnostic, .. }
            | Self::CommitOutcomeUnknown { diagnostic, .. }
            | Self::NotCommittedAfterEarlierFailure { diagnostic, .. }
            | Self::InvalidResultNoChanges { diagnostic, .. } => Some(diagnostic),
            _ => None,
        }
    }

    fn confirmed_written_locations(&self) -> Option<usize> {
        match self {
            Self::CompleteCommitted { outcome } | Self::PartialCommitted { outcome } => {
                Some(outcome.accepted_location_count())
            }
            Self::CommitOutcomeUnknown { .. } => None,
            _ => Some(0),
        }
    }
}

/// 最终化线交给记录 sink 的完整不可变文档。
pub(crate) struct TranslationTaskRecordDocument {
    total_tasks: usize,
    task: RpgMakerExecutableTask,
    evidence: TranslationTaskExecutionEvidence,
    total_duration: Duration,
    state: TranslationTaskRecordFinalState,
    /// 与项目日志共用、已在仍掌握 Unit/Task 事实的边界建立的安全诊断。
    ///
    /// 任务记录只呈现这些报告，不能重新从 Outcome 的业务枚举拼接另一套原因文本。
    outcome_diagnostics: Vec<DiagnosticReport>,
}

impl TranslationTaskRecordDocument {
    pub(crate) fn new(
        total_tasks: usize,
        task: RpgMakerExecutableTask,
        evidence: TranslationTaskExecutionEvidence,
        state: TranslationTaskRecordFinalState,
    ) -> Self {
        assert!(
            state.outcome_kind_matches_state(),
            "任务记录的完成、部分完成或不可用终态必须与权威 Outcome 种类一致"
        );
        let total_duration = evidence.elapsed_until_finalization();
        Self {
            total_tasks,
            task,
            evidence,
            total_duration,
            state,
            outcome_diagnostics: Vec::new(),
        }
    }

    /// 附加本次正常业务 Outcome 已经建立的诊断。
    ///
    /// 只有流水线最终化线调用它：同一份报告会同时交给 ProjectLog 和 Markdown 任务记录。
    pub(crate) fn with_outcome_diagnostics(mut self, diagnostics: Vec<DiagnosticReport>) -> Self {
        self.outcome_diagnostics = diagnostics;
        self
    }

    pub(crate) const fn task_index(&self) -> RpgMakerTranslationTaskIndex {
        self.task.index()
    }

    #[cfg(test)]
    pub(crate) const fn final_state(&self) -> &TranslationTaskRecordFinalState {
        &self.state
    }
}

/// RPG Maker 翻译最终化线使用的非权威记录入口。
pub(crate) trait TranslationTaskRecordSink: Send + Sync {
    fn enabled(&self) -> bool {
        true
    }

    /// 只接收已经固定的不可变终态文档，不在翻译业务编排中执行渲染或文件 I/O。
    fn submit(&self, document: TranslationTaskRecordDocument);
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoOpTranslationTaskRecordSink;

impl TranslationTaskRecordSink for NoOpTranslationTaskRecordSink {
    fn enabled(&self) -> bool {
        false
    }

    fn submit(&self, _document: TranslationTaskRecordDocument) {}
}

impl TranslationTaskRecordSink for ConfiguredTranslationTaskRecordSink {
    fn enabled(&self) -> bool {
        ConfiguredTranslationTaskRecordSink::enabled(self)
    }

    fn submit(&self, document: TranslationTaskRecordDocument) {
        ConfiguredTranslationTaskRecordSink::submit(self, document);
    }
}

impl TranslationTaskRecordArtifact for TranslationTaskRecordDocument {
    fn task_index(&self) -> usize {
        self.task_index().get()
    }

    fn total_tasks(&self) -> usize {
        self.total_tasks
    }

    fn render(
        &self,
        run_id: &str,
        client: &LlmClientRecordMetadata,
        locale: UiLocale,
        total_tasks: usize,
    ) -> Result<String, serde_json::Error> {
        render_translation_task_record(run_id, client, locale, self, total_tasks)
    }
}

impl TranslationTaskRecordSink for MarkdownTranslationTaskRecordSink {
    fn submit(&self, document: TranslationTaskRecordDocument) {
        MarkdownTranslationTaskRecordSink::submit(self, document);
    }
}

#[cfg(test)]
fn render_task_record(
    run_id: &str,
    client: &LlmClientRecordMetadata,
    locale: UiLocale,
    document: &TranslationTaskRecordDocument,
) -> Result<String, serde_json::Error> {
    render_translation_task_record(run_id, client, locale, document, document.total_tasks)
}

fn render_translation_task_record(
    run_id: &str,
    client: &LlmClientRecordMetadata,
    locale: UiLocale,
    document: &TranslationTaskRecordDocument,
    total_tasks: usize,
) -> Result<String, serde_json::Error> {
    let localizer = UiLocalizer::new(locale);
    let api_key_redactor = client.api_key_redactor();
    let ordinal = document.task.index().get().saturating_add(1);
    let outcome = document.state.outcome();
    let accepted = outcome.map_or(0, |outcome| outcome.accepted().len());
    let expected = document.task.expected_outputs().len();
    let attempts = document
        .evidence
        .attempt_count()
        .max(outcome.map_or(0, |outcome| outcome.attempts().get()));

    let mut output = String::new();
    let state_label = task_record_text(
        &localizer,
        UiMessage::TaskRecordStateLabel {
            state: document.state.code(),
        },
    );
    let padded_ordinal = format!("{ordinal:06}");
    let title = task_record_text(
        &localizer,
        UiMessage::TaskRecordTitle {
            ordinal: &padded_ordinal,
            state: &state_label,
        },
    );
    let _ = writeln!(output, "# {title}\n");
    let summary = match document.state.confirmed_written_locations() {
        Some(written) => task_record_text(
            &localizer,
            UiMessage::TaskRecordSummaryWithWritten {
                ordinal: ordinal as u64,
                total: total_tasks as u64,
                attempts: attempts as u64,
                accepted: accepted as u64,
                expected: expected as u64,
                written: written as u64,
            },
        ),
        None => task_record_text(
            &localizer,
            UiMessage::TaskRecordSummaryWithoutWritten {
                ordinal: ordinal as u64,
                total: total_tasks as u64,
                attempts: attempts as u64,
                accepted: accepted as u64,
                expected: expected as u64,
            },
        ),
    };
    let _ = writeln!(output, "{summary}\n");
    let run_id = markdown_inline_code(&api_key_redactor.redact(run_id));
    let _ = writeln!(
        output,
        "- {}{run_id}",
        task_record_text(&localizer, UiMessage::TaskRecordRunIdLabel),
    );
    let started_at = markdown_inline_code(&recorded_at_utc(document.evidence.started_at()));
    let _ = writeln!(
        output,
        "- {}{started_at}",
        task_record_text(&localizer, UiMessage::TaskRecordStartedAtLabel),
    );
    let duration = markdown_inline_code(&render_duration(&localizer, document.total_duration));
    let _ = writeln!(
        output,
        "- {}{duration}",
        task_record_text(&localizer, UiMessage::TaskRecordDurationLabel),
    );
    let endpoint = markdown_inline_code(client.endpoint());
    let _ = writeln!(
        output,
        "- {}{endpoint}",
        task_record_text(&localizer, UiMessage::TaskRecordEndpointLabel),
    );
    let model = markdown_inline_code(client.model());
    let _ = writeln!(
        output,
        "- {}{model}",
        task_record_text(&localizer, UiMessage::TaskRecordModelLabel),
    );

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordCustomParametersHeading)
    );
    let parameters = client
        .api_key_redactor()
        .redact_json_pretty(client.parameters())?;
    output.push_str(&markdown_fence(&parameters, "json"));

    for message in document.task.messages() {
        let title = match message.role() {
            ChatMessageRole::System => "System",
            ChatMessageRole::User => "User",
        };
        let _ = write!(output, "\n## {title}\n\n");
        let content = match message.role() {
            ChatMessageRole::System => api_key_redactor.redact(message.content()),
            ChatMessageRole::User => api_key_redactor
                .redact_text_with_markdown_ascii_punctuation_escaped(message.content()),
        };
        output.push_str(&content);
        if !content.ends_with('\n') {
            output.push('\n');
        }
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordAttemptsHeading)
    );
    if document.evidence.attempts.is_empty() {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(&localizer, UiMessage::TaskRecordNoRequest)
        );
    } else {
        for attempt in &document.evidence.attempts {
            render_task_record_attempt(
                &mut output,
                &localizer,
                client.api_key_redactor(),
                attempt,
            )?;
        }
    }

    if let Some(response) = &document.evidence.response {
        output.push_str("\n## Assistant\n\n");
        if let Some(error) = &response.parse_error {
            let category = match error.kind {
                TranslationTaskResponseParseErrorKind::Json(category)
                | TranslationTaskResponseParseErrorKind::JsonRepair { category, .. } => {
                    category.code()
                }
                _ => "",
            };
            let _ = writeln!(
                output,
                "> {}\n",
                task_record_text(
                    &localizer,
                    UiMessage::TaskRecordParseError {
                        kind: error.kind.code(),
                        category,
                        line: error.line.get() as u64,
                        column: error.column.get() as u64,
                    }
                )
            );
        }
        output.push_str(&render_readable_assistant(
            &response.raw_assistant,
            response.strict_json,
            api_key_redactor,
        ));
        render_json_repairs(&mut output, &response.repairs);
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordFinalResultHeading)
    );
    render_final_result(&mut output, &localizer, client.api_key_redactor(), document)?;

    Ok(output)
}

fn render_final_result(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    document: &TranslationTaskRecordDocument,
) -> Result<(), serde_json::Error> {
    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordFinalStatus {
                state: document.state.code(),
            }
        )
    );

    if let Some(outcome) = document.state.outcome() {
        let written = document.state.confirmed_written_locations();
        match written {
            Some(written) => {
                let _ = writeln!(
                    output,
                    "- {}",
                    task_record_text(
                        localizer,
                        UiMessage::TaskRecordAcceptedWritten {
                            accepted: outcome.accepted().len() as u64,
                            written: written as u64,
                        }
                    )
                );
            }
            None => {
                let _ = writeln!(
                    output,
                    "- {}",
                    task_record_text(
                        localizer,
                        UiMessage::TaskRecordAcceptedOutcomeUnknown {
                            accepted: outcome.accepted().len() as u64,
                        }
                    )
                );
            }
        }
    }
    for diagnostic in document
        .outcome_diagnostics
        .iter()
        .chain(document.state.diagnostic())
    {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(localizer, UiMessage::TaskRecordTaskDiagnostic)
        );
        let rendered = api_key_redactor.redact(&render_diagnostic_report(diagnostic, localizer));
        output.push_str(&markdown_fence(&rendered, "text"));
    }
    Ok(())
}

fn markdown_inline_code(value: &str) -> String {
    let encoded;
    let value = if value.chars().any(char::is_control)
        || value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace)
    {
        encoded = serde_json::to_string(value).expect("字符串必须能够序列化为 JSON");
        encoded.as_str()
    } else {
        value
    };
    let longest_backtick_run = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_backtick_run.saturating_add(1).max(1));
    if value.starts_with('`') || value.ends_with('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn recorded_at_utc(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000,
    )
}

/// 任务文档只向 Fluent 传入本模块建立的序号、稳定代码和已转义诊断投影。
/// 移除 Fluent 为终端输出添加的方向隔离符，避免不可见字符进入 Markdown 契约。
fn task_record_text(localizer: &UiLocalizer, message: UiMessage<'_>) -> String {
    localizer
        .format(message)
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect()
}

fn render_duration(localizer: &UiLocalizer, duration: Duration) -> String {
    if duration.as_secs() != 0 {
        let value = format!("{:.3}", duration.as_secs_f64());
        task_record_text(
            localizer,
            UiMessage::TaskRecordDurationSeconds { value: &value },
        )
    } else {
        let value = duration.as_millis().to_string();
        task_record_text(
            localizer,
            UiMessage::TaskRecordDurationMilliseconds { value: &value },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use secrecy::SecretString;
    use serde_json::{Map, json};
    use tempfile::tempdir;
    use url::{Url, form_urlencoded};

    use super::*;
    use crate::diagnostic::{
        Diagnostic, DiagnosticReport, HttpEndpoint, HttpIssue, HttpScheme, HttpTransportKind,
        HttpTransportPhase, SafeIdentifier, SafeText, StateEffect,
    };
    use crate::fingerprint::Sha256Fingerprint;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguagePair,
        LanguageText,
    };
    use crate::llm::{ApiKeyRedactor, ChatMessage};
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::runtime::cpu::{CpuExecutorConfig, RayonCpuExecutor};
    use crate::runtime::filesystem::SystemFileSystemConfig;
    use crate::translation::task_record::TaskRecordDiagnosticRecorder;
    use crate::translation_protocol::parse_translation_response;

    use super::super::executor::FinalLlmResponseMetadata;
    use super::super::pipeline::{
        AcceptedTranslationDecision, ExpectedLineShape, ExpectedTranslationOutput,
        ExpectedTranslationValidation, NonEmptyTaskItems, TranslationPatch,
        TranslationStateContext, TranslationTaskOutcomeContext, TranslationUnitIdentity,
        UnresolvedTranslationUnit,
    };
    use super::super::profile::TranslationResponseMode;

    #[derive(Clone, Default)]
    struct RecordingTaskRecordDiagnostics(Arc<Mutex<Vec<DiagnosticReport>>>);

    impl TaskRecordDiagnosticRecorder for RecordingTaskRecordDiagnostics {
        fn record_task_record_diagnostic(&self, report: DiagnosticReport) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(report);
        }
    }

    impl RecordingTaskRecordDiagnostics {
        fn reports(&self) -> Vec<DiagnosticReport> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    fn language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言应合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
        )
    }

    fn test_http_status_report(
        status: u16,
        retry_after_seconds: Option<u64>,
        provider_code: Option<&str>,
        provider_type: Option<&str>,
        provider_message: Option<&str>,
    ) -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::http(HttpIssue::Status {
                endpoint: HttpEndpoint::new(HttpScheme::Https, "example.test", None),
                status,
                retry_after_seconds,
                provider_code: provider_code
                    .map(|value| SafeIdentifier::new(value).expect("测试 provider code 合法")),
                provider_type: provider_type
                    .map(|value| SafeIdentifier::new(value).expect("测试 provider type 合法")),
                provider_message: provider_message.map(SafeText::new),
                response_read_failure: None,
            }),
        )
    }

    fn test_terminal_diagnostic() -> DiagnosticReport {
        test_http_status_report(503, None, Some("busy"), Some("service_error"), None)
    }

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    fn test_identity(index: usize) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Actors),
                vec![crate::rpg_maker::text::RpgMakerLocationStep::index(index)],
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段名应合法")),
            TextUnitContent::Value("姫".to_owned()),
            "{}",
        )
    }

    fn test_expected_output(id: usize) -> ExpectedTranslationOutput {
        let language_analysis = JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("测试日语残留策略应合法"),
        )
        .analyze_source(&LanguageText::natural("姫"));
        ExpectedTranslationOutput::new(
            task_id(id),
            test_identity(id),
            Vec::new(),
            ExpectedTranslationValidation::new(
                ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                "姫",
                Vec::new(),
                language_analysis,
            ),
            TranslationStateContext::new(Sha256Fingerprint::from_bytes([id as u8; 32])),
            Vec::new(),
        )
    }

    fn test_complete_outcome() -> Arc<TranslationTaskOutcome> {
        Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(
                None,
                None,
                crate::diagnostic::RpgMakerModelFinishReason::Stop,
                None,
            ),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    task_id(0),
                    TranslationPatch::new(
                        test_identity(0),
                        Vec::new(),
                        TextUnitContent::Value("公主".to_owned()),
                        Sha256Fingerprint::from_bytes([0x22; 32]),
                    ),
                ),
                Vec::new(),
            ),
        })
    }

    fn test_partial_outcome() -> Arc<TranslationTaskOutcome> {
        Arc::new(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(
                None,
                None,
                crate::diagnostic::RpgMakerModelFinishReason::Stop,
                None,
            ),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    task_id(0),
                    TranslationPatch::new(
                        test_identity(0),
                        Vec::new(),
                        TextUnitContent::Value("公主".to_owned()),
                        Sha256Fingerprint::from_bytes([0x33; 32]),
                    ),
                ),
                Vec::new(),
            ),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::for_test(
                    task_id(1),
                    0,
                    TranslationUnitRejectionReason::Missing,
                ),
                Vec::new(),
            ),
        })
    }

    fn test_unavailable_outcome() -> Arc<TranslationTaskOutcome> {
        Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: None,
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::for_test(
                    task_id(0),
                    0,
                    TranslationUnitRejectionReason::Missing,
                ),
                Vec::new(),
            ),
        })
    }

    fn client(
        endpoint: &str,
        model: &str,
        parameters: Map<String, Value>,
        api_key: &str,
    ) -> LlmClientRecordMetadata {
        LlmClientRecordMetadata::new(
            endpoint.to_owned(),
            model.to_owned(),
            parameters,
            ApiKeyRedactor::new(SecretString::from(api_key)),
        )
    }

    fn document(
        messages: Vec<ChatMessage>,
        attempts: Vec<TranslationTaskAttemptRecord>,
        response: Option<TranslationTaskResponseRecord>,
        state: TranslationTaskRecordFinalState,
    ) -> TranslationTaskRecordDocument {
        TranslationTaskRecordDocument::new(
            3,
            RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(0),
                language_pair(),
                messages,
                Vec::new(),
            ),
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::from_millis(12),
                attempts,
                response,
            ),
            state,
        )
    }

    #[test]
    fn readable_markdown_keeps_native_roles_and_one_complete_assistant_json() {
        let response = LlmResponse::new(
            "raw",
            LlmFinishReason::Stop,
            None,
            None,
            Some(LlmUsage::new(10, 4, 14)),
        );
        let document = document(
            vec![
                ChatMessage::new(ChatMessageRole::System, "# 规则\n\n- 保持列表\n- 保持结构"),
                ChatMessage::new(
                    ChatMessageRole::User,
                    "> 引用\n\n| 原文 | 说明 |\n| --- | --- |\n| 姫 | 名称 |",
                ),
            ],
            vec![TranslationTaskAttemptRecord::succeeded(
                NonZeroUsize::MIN,
                Duration::from_millis(7),
                &response,
            )],
            Some(TranslationTaskResponseRecord::parsed(
                r#"{"think":"先核对占位符，再翻译。","translations":{"0":["公主","第二行"]}}"#
                    .to_owned(),
            )),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );
        let mut parameters = Map::new();
        parameters.insert("temperature".to_owned(), json!(0.2));
        let markdown = render_task_record(
            "run-000001",
            &client(
                "https://example.test/v1/chat/completions",
                "model-a",
                parameters,
                "unused-key",
            ),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert_eq!(
            markdown,
            concat!(
                r#"# 翻译任务 000001 · 执行失败

`任务 1/3` · `尝试 1 次` · `验收 0/0` · `写入 0 处`

- Run ID：`run-000001`
- 开始时间：`1970-01-01T00:00:00.000Z`
- 总耗时：`12 毫秒`
- Endpoint：`https://example.test/v1/chat/completions`
- Model：`model-a`

## 自定义参数

```json
{
  "temperature": 0.2
}
```

## System

# 规则

- 保持列表
- 保持结构

## User

> 引用

| 原文 | 说明 |
| --- | --- |
| 姫 | 名称 |

## 请求过程

- 尝试 1：成功；finish reason `stop`；token `10 / 4 / 14`；耗时 `7 毫秒`

## Assistant

```json
{
  "think": "先核对占位符，再翻译。",
  "translations": {
    "0": [
      "公主",
      "第二行"
    ]
  }
}
```

## 最终结果

- 状态：执行失败，未提交
- 任务诊断
```text
位置："#,
                "\u{2068}",
                r#"https://example.test"#,
                "\u{2069}",
                r#"
原因："#,
                "\u{2068}",
                r#"外部服务拒绝了请求 (HTTP 状态 503; 服务方 code：busy; 服务方 type：service_error)"#,
                "\u{2069}",
                r#"
处理办法："#,
                "\u{2068}",
                r#"检查模型服务响应和账户配额"#,
                "\u{2069}",
                r#"
```
"#,
            )
        );
    }

    #[test]
    fn non_thinking_noncanonical_fenced_json_records_repairs_and_safe_assistant_text() {
        const API_KEY: &str = "quote\"slash\\value";
        let encoded_api_key = serde_json::to_string(API_KEY).expect("API key 应可编码为 JSON");
        let encoded_fragment = &encoded_api_key[1..encoded_api_key.len() - 1];
        let raw_assistant = format!(
            "\u{2003}\r\n```json\r\n{{\"0\":[\"before-{encoded_fragment}-after\"]}}\r\n```\r\n"
        );
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect("非 thinking 的非规范围栏应可保守修复");
        let (_, _, repairs) = parsed.into_parts();
        let response = TranslationTaskResponseRecord::parsed_with_repairs(raw_assistant, repairs);
        let document = document(
            Vec::new(),
            Vec::new(),
            Some(response),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );

        let markdown = render_task_record(
            "run-repaired",
            &client("https://example.test", "model", Map::new(), API_KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("修复后的非 thinking 响应应该可渲染");

        assert!(markdown.contains("## JSON Repairs"));
        assert_eq!(markdown.matches("`removed_markdown_fence`").count(), 2);
        assert!(markdown.contains("| `removed_markdown_fence` | 2 | 1 |"));
        assert!(markdown.contains("| `removed_markdown_fence` | 4 | 1 |"));
        assert_eq!(markdown.matches("## Assistant").count(), 1);
        assert!(markdown.contains("## Assistant\n\n````text\n"));
        assert!(!markdown.contains("## Raw Assistant"));
        assert!(!markdown.contains(API_KEY));
        assert!(!markdown.contains(encoded_fragment));
        assert!(markdown.contains("before-[REDACTED API KEY]-after"));
    }

    #[test]
    fn non_thinking_canonical_fence_becomes_one_pretty_json_block() {
        let raw_assistant = "```json\n{\"0\":[\"严格响应\"]}\n```".to_owned();
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect("规范围栏应直接解析内部 JSON");
        let (_, _, repairs) = parsed.into_parts();
        let response = TranslationTaskResponseRecord::parsed_with_repairs(raw_assistant, repairs);
        let document = document(
            Vec::new(),
            Vec::new(),
            Some(response),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );

        let markdown = render_task_record(
            "run-strict",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("规范围栏的非 thinking 响应应该可渲染");

        assert_eq!(markdown.matches("## Assistant").count(), 1);
        assert!(
            markdown
                .contains("## Assistant\n\n```json\n{\n  \"0\": [\n    \"严格响应\"\n  ]\n}\n```")
        );
        assert!(!markdown.contains("## JSON Repairs"));
        assert!(!markdown.contains("## Raw Assistant"));
        assert!(!markdown.contains("### ID"));
    }

    #[test]
    fn duplicate_unknown_invalid_and_missing_ids_remain_in_one_readable_json_block() {
        let accepted_identity = test_identity(0);
        let accepted_translation = TextUnitContent::Value("公主".to_owned());
        let accepted = AcceptedTranslationDecision::new(
            task_id(0),
            TranslationPatch::new(
                accepted_identity,
                Vec::new(),
                accepted_translation,
                Sha256Fingerprint::from_bytes([0x11; 32]),
            ),
        );
        let outcome = Arc::new(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![
                    TranslationProtocolDiagnostic::UnknownId {
                        item_index: 2,
                        id: task_id(99),
                    },
                    TranslationProtocolDiagnostic::InvalidId { item_index: 3 },
                ],
            ),
            final_response: FinalLlmResponseMetadata::new(
                None,
                None,
                crate::diagnostic::RpgMakerModelFinishReason::Stop,
                None,
            ),
            accepted: NonEmptyTaskItems::new(accepted, Vec::new()),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::for_test(
                    task_id(1),
                    0,
                    TranslationUnitRejectionReason::Duplicate,
                ),
                vec![UnresolvedTranslationUnit::for_test(
                    task_id(2),
                    0,
                    TranslationUnitRejectionReason::Missing,
                )],
            ),
        });
        let task = RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(0),
            language_pair(),
            Vec::new(),
            (0..3).map(test_expected_output).collect(),
        );
        let document = TranslationTaskRecordDocument::new(
            1,
            task,
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                Some(TranslationTaskResponseRecord::parsed(
                    r#"{"0":["第一版"],"0":["第二版"],"99":["未知"],"bad":{"raw":true}}"#
                        .to_owned(),
                )),
            ),
            TranslationTaskRecordFinalState::PartialCommitted { outcome },
        )
        .with_outcome_diagnostics(vec![test_terminal_diagnostic()]);
        let markdown = render_task_record(
            "run-ids",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert_eq!(markdown.matches("## Assistant").count(), 1);
        assert!(markdown.contains(concat!(
            "```json\n{\n",
            "  \"0\": [\n    \"第一版\"\n  ],\n",
            "  \"0\": [\n    \"第二版\"\n  ],\n",
            "  \"99\": [\n    \"未知\"\n  ],\n",
            "  \"bad\": {\n    \"raw\": true\n  }\n",
            "}\n```"
        )));
        assert!(!markdown.contains("### ID"));
        assert!(markdown.contains("- 状态：部分完成，已确认提交"));
        assert!(markdown.contains("外部服务拒绝了请求"));
        assert!(!markdown.contains("http.status"));
        assert!(!markdown.contains("协议诊断："));
        assert!(markdown.contains("## Assistant\n\n```json"));
    }

    #[test]
    fn invalid_assistant_uses_dynamic_fence_and_keeps_one_precise_error() {
        let raw = "```json\n{\"0\":[\"译文\"]}\n```\n尾部";
        let document = document(
            Vec::new(),
            Vec::new(),
            Some(TranslationTaskResponseRecord::invalid(
                raw.to_owned(),
                TranslationTaskResponseParseError::new(
                    TranslationTaskResponseParseErrorKind::Json(
                        TranslationTaskResponseJsonErrorCategory::Syntax,
                    ),
                    NonZeroUsize::new(4).expect("测试行号非零"),
                    NonZeroUsize::MIN,
                ),
            )),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );
        let markdown = render_task_record(
            "run-invalid",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert_eq!(markdown.matches("> 解析错误：").count(), 1);
        assert!(markdown.contains("第 4 行、第 1 列"));
        assert!(markdown.contains("````text\n```json\n{\"0\":[\"译文\"]}\n```\n尾部\n````"));

        let english = render_task_record(
            "run-invalid",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::English,
            &document,
        )
        .expect("任务记录应可渲染");
        assert!(english.contains(
            "> Parse error: invalid model response JSON (category `syntax`) at line 4, column 1"
        ));
        assert!(!english.contains("模型响应 JSON 无效"));
    }

    #[test]
    fn outcome_diagnostic_is_rendered_from_the_document() {
        let document = document(
            Vec::new(),
            Vec::new(),
            None,
            TranslationTaskRecordFinalState::PartialCommitted {
                outcome: test_partial_outcome(),
            },
        )
        .with_outcome_diagnostics(vec![test_terminal_diagnostic()]);
        let markdown = render_task_record(
            "run-outcome-diagnostic",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::English,
            &document,
        )
        .expect("任务记录应直接渲染流水线传入的结构化诊断");

        assert!(markdown.contains("The external service rejected the request"));
        assert!(!markdown.contains("http.status"));
        assert!(!markdown.contains("Translation array item"));
    }

    #[test]
    fn invalid_response_parse_error_is_rendered_once_for_the_real_unavailable_outcome() {
        let parse_error = TranslationTaskResponseParseError::new(
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax,
            ),
            NonZeroUsize::new(4).expect("测试行号非零"),
            NonZeroUsize::MIN,
        );
        let unresolved = (0..2)
            .map(|id| {
                UnresolvedTranslationUnit::for_test(
                    task_id(id),
                    0,
                    TranslationUnitRejectionReason::InvalidResponse,
                )
            })
            .collect::<Vec<_>>();
        let outcome = Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![TranslationProtocolDiagnostic::InvalidResponse { error: parse_error }],
            ),
            final_response: Some(FinalLlmResponseMetadata::new(
                None,
                None,
                crate::diagnostic::RpgMakerModelFinishReason::Stop,
                None,
            )),
            reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
            unresolved: NonEmptyTaskItems::new(
                unresolved[0].clone(),
                unresolved.into_iter().skip(1).collect(),
            ),
        });
        let document = TranslationTaskRecordDocument::new(
            1,
            RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                (0..2).map(test_expected_output).collect(),
            ),
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                Some(TranslationTaskResponseRecord::invalid(
                    "不是 JSON".to_owned(),
                    parse_error,
                )),
            ),
            TranslationTaskRecordFinalState::UnavailableNoChanges { outcome },
        )
        .with_outcome_diagnostics(vec![test_terminal_diagnostic()]);
        let markdown = render_task_record(
            "run-invalid-real",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert_eq!(
            markdown
                .matches("> 解析错误：模型响应 JSON 无效（类别 `syntax`），第 4 行、第 1 列")
                .count(),
            1,
            "解析错误只应在原始 Assistant 上方出现一次"
        );
        assert!(markdown.contains("- 状态：不可用，项目未改变"));
        assert!(markdown.contains("外部服务拒绝了请求"));
        assert!(!markdown.contains("http.status"));
        assert!(!markdown.contains("- 不可用原因："));
        assert!(!markdown.contains("- 协议诊断："));
    }

    #[test]
    fn english_locale_translates_record_scaffolding_but_keeps_native_role_headings() {
        let document = document(
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Native system"),
                ChatMessage::new(ChatMessageRole::User, "Native user"),
            ],
            Vec::new(),
            None,
            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
        );
        let markdown = render_task_record(
            "run-en",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::English,
            &document,
        )
        .expect("任务记录应可渲染");

        assert!(markdown.starts_with("# Translation task 000001 · Cancelled\n"));
        assert!(markdown.contains("## Custom parameters\n"));
        assert!(
            markdown.contains("## Request attempts\n\n- No model request was ready to send.\n")
        );
        assert!(markdown.contains("## Final result\n\n- Status: cancelled; not committed\n"));
        assert!(markdown.contains("## System\n\n# Native system\n"));
        assert!(markdown.contains("## User\n\nNative user\n"));
        assert!(!markdown.contains("## 系统"));
        assert!(!markdown.contains("最终结果"));
    }

    #[test]
    fn terminal_state_codes_are_closed_and_distinguish_both_commit_phases() {
        let complete = test_complete_outcome();
        let partial = test_partial_outcome();
        let unavailable = test_unavailable_outcome();
        let states = vec![
            (
                TranslationTaskRecordFinalState::CompleteCommitted {
                    outcome: Arc::clone(&complete),
                },
                "complete",
            ),
            (
                TranslationTaskRecordFinalState::PartialCommitted { outcome: partial },
                "partial",
            ),
            (
                TranslationTaskRecordFinalState::UnavailableNoChanges {
                    outcome: unavailable,
                },
                "unavailable",
            ),
            (
                TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                    diagnostic: test_terminal_diagnostic(),
                },
                "execution_failed",
            ),
            (
                TranslationTaskRecordFinalState::CommitNotApplied {
                    outcome: Arc::clone(&complete),
                    phase: TranslationTaskCommitPhase::Preparation,
                    diagnostic: test_terminal_diagnostic(),
                },
                "commit_preparation_failed",
            ),
            (
                TranslationTaskRecordFinalState::CommitNotApplied {
                    outcome: Arc::clone(&complete),
                    phase: TranslationTaskCommitPhase::Transaction,
                    diagnostic: test_terminal_diagnostic(),
                },
                "commit_not_applied",
            ),
            (
                TranslationTaskRecordFinalState::CommitOutcomeUnknown {
                    outcome: Arc::clone(&complete),
                    diagnostic: test_terminal_diagnostic(),
                },
                "commit_outcome_unknown",
            ),
            (
                TranslationTaskRecordFinalState::NotCommittedAfterEarlierFailure {
                    outcome: Arc::clone(&complete),
                    diagnostic: test_terminal_diagnostic(),
                },
                "not_committed_after_earlier_failure",
            ),
            (
                TranslationTaskRecordFinalState::InvalidResultNoChanges {
                    outcome: Arc::clone(&complete),
                    diagnostic: test_terminal_diagnostic(),
                },
                "invalid_result",
            ),
            (
                TranslationTaskRecordFinalState::CancelledNoChanges {
                    outcome: Some(complete),
                },
                "cancelled",
            ),
        ];

        assert_eq!(
            states
                .iter()
                .map(|(state, _)| state.code())
                .collect::<Vec<_>>(),
            states
                .iter()
                .map(|(_, expected)| *expected)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[should_panic(expected = "任务记录的完成、部分完成或不可用终态必须与权威 Outcome 种类一致")]
    fn document_rejects_a_terminal_label_that_disagrees_with_the_outcome() {
        let _ = document(
            Vec::new(),
            Vec::new(),
            None,
            TranslationTaskRecordFinalState::PartialCommitted {
                outcome: test_complete_outcome(),
            },
        );
    }

    #[test]
    fn unknown_commit_result_never_claims_zero_written_locations() {
        let task = RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(0),
            language_pair(),
            Vec::new(),
            vec![test_expected_output(0)],
        );
        let document = TranslationTaskRecordDocument::new(
            1,
            task,
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                None,
            ),
            TranslationTaskRecordFinalState::CommitOutcomeUnknown {
                outcome: test_complete_outcome(),
                diagnostic: test_terminal_diagnostic(),
            },
        );
        let markdown = render_task_record(
            "run-unknown",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert!(markdown.contains("- 状态：提交结果未知"));
        assert!(markdown.contains("- 已验收：1 项；数据库提交终态无法确认"));
        assert!(
            !markdown.contains("写入 0 处") && !markdown.contains("写入 0 个实际位置"),
            "提交终态未知时不得伪造零写入"
        );
    }

    #[test]
    fn cancelled_retry_wait_never_claims_that_a_retry_happened() {
        let diagnostic = DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::http(HttpIssue::Transport {
                endpoint: HttpEndpoint::new(HttpScheme::Https, "example.test", None),
                phase: HttpTransportPhase::Send,
                transport: HttpTransportKind::Timeout,
                io_kind: None,
                raw_os_code: None,
            }),
        );
        let document = document(
            Vec::new(),
            vec![TranslationTaskAttemptRecord::retryable(
                NonZeroUsize::MIN,
                Duration::from_millis(10),
                diagnostic,
                None,
                Some(TranslationTaskRetryWaitRecord::CancelledWhileWaiting {
                    planned_duration: Duration::from_secs(1),
                }),
            )],
            None,
            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
        );
        let markdown = render_task_record(
            "run-cancelled-wait",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("取消终态记录应可渲染");

        assert!(markdown.contains("计划等待 `1.000 秒`，等待期间取消"));
        assert!(
            !markdown.contains("等待 `1.000 秒` 后重试"),
            "等待期间取消时不得声称等待完成或已经重试"
        );
        assert!(markdown.contains("收到有效响应前 HTTP 传输失败"));
        assert!(!markdown.contains("http.transport.timeout"));
        assert!(
            !markdown.contains(r#"{"kind":"failure""#),
            "任务记录必须渲染可读诊断，不能显示旧 reason 的 JSON 外壳"
        );
    }

    #[test]
    fn safe_http_provider_fields_are_written_to_attempt_records() {
        const API_KEY: &str = "task-record-secret";
        let message = format!("before {API_KEY} after");
        let diagnostic = test_http_status_report(
            400,
            None,
            Some("bad_request"),
            Some("invalid_request_error"),
            Some(&message),
        );
        let document = document(
            Vec::new(),
            vec![TranslationTaskAttemptRecord::failed(
                NonZeroUsize::MIN,
                Duration::from_millis(10),
                diagnostic,
            )],
            None,
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );

        let markdown = render_task_record(
            "run-provider-message",
            &client("https://example.test", "model", Map::new(), API_KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert!(!markdown.contains(API_KEY));
        assert!(markdown.contains("外部服务拒绝了请求"));
        for expected in [
            "bad_request",
            "invalid_request_error",
            "before [REDACTED API KEY] after",
        ] {
            assert!(markdown.contains(expected), "缺少 {expected:?}");
        }
        for forbidden in ["provider_code", "provider_type", "provider_message"] {
            assert!(!markdown.contains(forbidden));
        }
    }

    #[test]
    fn total_duration_is_frozen_when_the_final_state_is_constructed() {
        let started = Instant::now() - Duration::from_secs(2);
        let evidence = TranslationTaskExecutionEvidence::from_execution(
            Some(OffsetDateTime::UNIX_EPOCH),
            Some(started),
            0,
            Vec::new(),
            None,
        );
        let document = TranslationTaskRecordDocument::new(
            1,
            RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                Vec::new(),
            ),
            evidence,
            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
        );

        assert!(
            document.total_duration >= Duration::from_secs(2),
            "总耗时必须延续到顺序最终化构造终态，而不是停在 Executor 返回时"
        );
    }

    #[test]
    fn api_key_replacement_covers_every_record_field_without_hiding_neighbors() {
        const KEY: &str = "actual\"api\\key";
        const NEIGHBOR: &str = "ordinary-neighbor";
        let diagnostic_message = format!("{NEIGHBOR}:{KEY}");
        let diagnostic = test_http_status_report(503, None, None, None, Some(&diagnostic_message));
        let response = LlmResponse::new(
            format!("{NEIGHBOR}:{KEY}"),
            LlmFinishReason::Other(format!("{NEIGHBOR}:{KEY}")),
            Some(format!("{NEIGHBOR}:{KEY}")),
            Some(format!("{NEIGHBOR}:{KEY}")),
            None,
        );
        let raw_assistant = json!({
            "think": format!("``` {NEIGHBOR}:{KEY}"),
            "translations": {}
        })
        .to_string();
        let document = document(
            vec![
                ChatMessage::new(ChatMessageRole::System, format!("System {NEIGHBOR}:{KEY}")),
                ChatMessage::new(ChatMessageRole::User, format!("User {NEIGHBOR}:{KEY}")),
            ],
            vec![
                TranslationTaskAttemptRecord::succeeded(
                    NonZeroUsize::MIN,
                    Duration::from_millis(1),
                    &response,
                ),
                TranslationTaskAttemptRecord::failed(
                    NonZeroUsize::new(2).expect("测试 attempt 应非零"),
                    Duration::from_millis(1),
                    diagnostic.clone(),
                ),
            ],
            Some(TranslationTaskResponseRecord::parsed(raw_assistant)),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges { diagnostic },
        );
        let mut parameters = Map::new();
        parameters.insert(
            format!("{NEIGHBOR}:{KEY}"),
            json!(format!("{NEIGHBOR}:{KEY}")),
        );
        let mut endpoint =
            Url::parse("https://example.test/v1/chat/completions").expect("测试 endpoint 应合法");
        endpoint
            .query_pairs_mut()
            .append_pair("token", &format!("{NEIGHBOR}:{KEY}"));
        let markdown = render_task_record(
            "run-key",
            &client(
                endpoint.as_str(),
                &format!("{NEIGHBOR}:{KEY}"),
                parameters,
                KEY,
            ),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        assert!(!markdown.contains(KEY));
        let escaped_key = serde_json::to_string(KEY).expect("API key 应可序列化");
        assert!(
            !markdown.contains(&escaped_key[1..escaped_key.len() - 1]),
            "JSON 转义后的 API key 也不得进入记录"
        );
        let encoded_key = form_urlencoded::Serializer::new(String::new())
            .append_pair("", KEY)
            .finish();
        assert!(
            !markdown.contains(encoded_key.trim_start_matches('=')),
            "URL query 编码后的 API key 也不得进入记录"
        );
        let redacted_count = markdown.matches("[REDACTED API KEY]").count();
        assert!(
            redacted_count >= 8,
            "每个独立记录字段都应替换 API key 实际值，实际 {redacted_count} 处"
        );
        let neighbor_count = markdown.matches(NEIGHBOR).count();
        assert!(
            neighbor_count >= 8,
            "普通相邻文本不得随 API key 一起删除，实际 {neighbor_count} 处"
        );
        assert!(
            markdown.contains("## Assistant\n\n````json\n"),
            "thinking 成功响应应在唯一 Assistant 中保留脱敏后的完整 JSON"
        );
        assert!(!markdown.contains("## Thinking"));
        assert!(!markdown.contains("## Raw Assistant"));
    }

    #[test]
    fn deeply_nested_assistant_json_stays_valid_redacted_and_stack_safe() {
        const DEPTH: usize = 1_000;
        const KEY: &str = "quote\"slash\\value";

        let encoded_key = serde_json::to_string(KEY).expect("API key 应可序列化");
        let encoded_fragment = &encoded_key[1..encoded_key.len() - 1];
        let raw_json = format!(
            "{}{{\"secret\":\"before-{encoded_fragment}-after\"}}{}",
            "[".repeat(DEPTH),
            "]".repeat(DEPTH)
        );
        let raw_assistant = format!(r#"{{"0":{raw_json}}}"#);
        parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
            .expect("深层测试值必须是合法 JSON");
        let response = TranslationTaskResponseRecord::parsed(raw_assistant);
        let evidence = TranslationTaskExecutionEvidence::new(
            OffsetDateTime::UNIX_EPOCH,
            Duration::ZERO,
            Vec::new(),
            Some(response),
        );
        let document = TranslationTaskRecordDocument::new(
            1,
            RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                Vec::new(),
            ),
            evidence,
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: test_terminal_diagnostic(),
            },
        );

        let markdown = render_task_record(
            "run-deep-raw",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("深层 RawJson 任务记录应可渲染");
        let fenced = markdown
            .split_once("## Assistant\n\n```json\n")
            .expect("记录必须包含唯一 Assistant JSON")
            .1;
        let rendered_json = fenced.split_once("\n```").expect("JSON 围栏必须闭合").0;
        let reparsed =
            serde_json::from_str::<Box<RawValue>>(rendered_json).expect("脱敏后仍必须是合法 JSON");

        assert!(!rendered_json.contains(KEY));
        assert!(!rendered_json.contains(encoded_fragment));
        assert!(rendered_json.contains("[REDACTED API KEY]"));
        drop(reparsed);
        drop(document);
    }

    #[test]
    fn final_rejections_and_protocol_diagnostics_redact_keys_and_escape_markdown() {
        const KEY: &str = "api`key";
        const NEIGHBOR: &str = "ordinary``marker";
        let accepted = AcceptedTranslationDecision::new(
            task_id(0),
            TranslationPatch::new(
                test_identity(0),
                Vec::new(),
                TextUnitContent::Value("公主".to_owned()),
                Sha256Fingerprint::from_bytes([0x22; 32]),
            ),
        );
        let outcome = Arc::new(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![
                    TranslationProtocolDiagnostic::NonStopFinish {
                        reason:
                            crate::diagnostic::RpgMakerModelNonStopFinishReason::provider_specific(
                                format!("{NEIGHBOR}:{KEY}"),
                            ),
                    },
                    TranslationProtocolDiagnostic::InvalidId { item_index: 0 },
                ],
            ),
            final_response: FinalLlmResponseMetadata::new(
                None,
                None,
                crate::diagnostic::RpgMakerModelFinishReason::provider_specific(format!(
                    "{NEIGHBOR}:{KEY}"
                )),
                None,
            ),
            accepted: NonEmptyTaskItems::new(accepted, Vec::new()),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::for_test(
                    task_id(1),
                    0,
                    TranslationUnitRejectionReason::SourceResidual {
                        fragment: format!("{NEIGHBOR}:{KEY}\nsource"),
                    },
                ),
                vec![
                    UnresolvedTranslationUnit::for_test(
                        task_id(2),
                        0,
                        TranslationUnitRejectionReason::PlaceholderMismatch {
                            token: format!("{NEIGHBOR}:{KEY}"),
                        },
                    ),
                    UnresolvedTranslationUnit::for_test(
                        task_id(3),
                        0,
                        TranslationUnitRejectionReason::InvalidShape {
                            problem: TranslationAssistantValueError::SourceEchoUnexpectedField,
                        },
                    ),
                ],
            ),
        });
        let document = TranslationTaskRecordDocument::new(
            1,
            RpgMakerExecutableTask::new(
                RpgMakerTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                (0..4).map(test_expected_output).collect(),
            ),
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                None,
            ),
            TranslationTaskRecordFinalState::PartialCommitted { outcome },
        )
        .with_outcome_diagnostics(vec![test_terminal_diagnostic()]);
        let markdown = render_task_record(
            "run-final-redaction",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("最终诊断任务记录应可渲染");

        assert!(!markdown.contains(KEY));
        assert!(!markdown.contains(NEIGHBOR));
        assert!(markdown.contains("外部服务拒绝了请求"));
        assert!(!markdown.contains("http.status"));
    }

    #[test]
    fn unprocessed_raw_assistant_redacts_the_json_escaped_api_key_without_rewriting_its_shell() {
        const KEY: &str = "quote\"slash\\value";
        let encoded_key = serde_json::to_string(KEY).expect("API key 应可序列化");
        let encoded_key = &encoded_key[1..encoded_key.len() - 1];
        let raw = format!("prefix {{\"value\":\"before-{encoded_key}-after\"}} trailing {{");
        let document = document(
            Vec::new(),
            Vec::new(),
            Some(TranslationTaskResponseRecord::unprocessed(raw)),
            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
        );

        let markdown = render_task_record(
            "run-unprocessed",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("未处理 Assistant 记录应可渲染");

        assert!(!markdown.contains(KEY));
        assert!(!markdown.contains(encoded_key));
        assert!(
            markdown.contains("prefix {\"value\":\"before-[REDACTED API KEY]-after\"} trailing {")
        );
    }

    #[test]
    fn rendered_rpg_user_and_raw_quote_prefixed_assistant_key_are_both_redacted() {
        const KEY: &str = "\"secret[]\\value";
        let escaped_key = KEY.chars().fold(String::new(), |mut output, character| {
            if character.is_ascii_punctuation() {
                output.push('\\');
            }
            output.push(character);
            output
        });
        let document = document(
            vec![ChatMessage::new(
                ChatMessageRole::User,
                format!("unit={escaped_key}"),
            )],
            Vec::new(),
            Some(TranslationTaskResponseRecord::unprocessed(format!(
                "prefix {KEY} trailing {{"
            ))),
            TranslationTaskRecordFinalState::CancelledNoChanges { outcome: None },
        );

        let markdown = render_task_record(
            "run-rendered-redaction",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("RPG Maker 任务记录应可渲染");

        assert!(!markdown.contains(KEY));
        assert!(!markdown.contains(&escaped_key));
        assert_eq!(markdown.matches("[REDACTED API KEY]").count(), 2);
    }

    #[tokio::test]
    async fn existing_target_is_reported_without_overwrite_or_business_failure() {
        let directory = tempdir().expect("临时目录应可建立");
        let record_directory = directory.path().join("task-records").join("run-conflict");
        std::fs::create_dir_all(&record_directory).expect("任务记录目录应可建立");
        let target = record_directory.join("task-000001.md");
        std::fs::write(&target, "existing").expect("既有任务记录应可建立");

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = RayonCpuExecutor::start(CpuExecutorConfig::fixed(NonZeroUsize::MIN))
            .expect("CPU 执行根应可建立");
        let diagnostics = RecordingTaskRecordDiagnostics::default();
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory,
            "run-conflict".to_owned(),
            client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            diagnostics.clone(),
        );
        sink.submit(document(
            Vec::new(),
            Vec::new(),
            None,
            TranslationTaskRecordFinalState::CompleteCommitted {
                outcome: test_complete_outcome(),
            },
        ));

        sink.finish().await;

        assert_eq!(
            std::fs::read_to_string(&target).expect("既有文件应仍可读取"),
            "existing",
            "无覆盖写入失败不得改写既有任务记录"
        );
        let reports = diagnostics.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].primary().code(),
            "observability.task_record.write"
        );
        assert_eq!(reports[0].effect(), StateEffect::Unchanged);
        cpu.shutdown().expect("CPU 执行根应可关闭");
    }
}
