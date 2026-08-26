//! RPG Maker 翻译任务的可读、非权威旁路记录。
//!
//! 一个已开始 TaskBlock 最多形成一个不可变 Markdown 文档。模型请求、响应解析、
//! 逐 ID 验收和数据库提交仍分别由原有语义所有者负责；本模块只接收它们建立的
//! 确定事实并呈现，不参与恢复、重放、验收、提交或退出码判断。

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::diagnostic::{DiagnosticReport, render_diagnostic_report};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ApiKeyRedactor, ChatMessageRole};
pub(crate) use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::translation::task_record::{
    TranslationTaskRecordArtifact, TranslationTaskRecordOutputSummary, markdown_fence,
    markdown_json_fence, task_record_output_ids, task_record_text,
};

use super::pipeline::{
    RpgMakerExecutableTask, RpgMakerTranslationTaskIndex, TranslationTaskOutcome,
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

/// 模型请求边界交给任务记录的唯一响应事实。
#[derive(Debug)]
pub(crate) struct TranslationTaskResponseRecord {
    raw_assistant: Arc<String>,
}

impl TranslationTaskResponseRecord {
    pub(crate) fn new(raw_assistant: impl Into<Arc<String>>) -> Self {
        Self {
            raw_assistant: raw_assistant.into(),
        }
    }

    fn into_raw_assistant(self) -> Arc<String> {
        self.raw_assistant
    }

    #[cfg(test)]
    pub(crate) fn raw_assistant(&self) -> &str {
        self.raw_assistant.as_str()
    }
}

/// Executor 交给流水线的最小执行证据。
#[derive(Debug)]
pub(crate) struct TranslationTaskExecutionEvidence {
    attempt_count: usize,
    response: Option<TranslationTaskResponseRecord>,
}

impl TranslationTaskExecutionEvidence {
    pub(crate) fn from_execution(
        attempt_count: usize,
        response: Option<TranslationTaskResponseRecord>,
    ) -> Self {
        Self {
            attempt_count,
            response,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(attempts: NonZeroUsize) -> Self {
        Self {
            attempt_count: attempts.get(),
            response: None,
        }
    }

    pub(crate) const fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    #[cfg(test)]
    pub(crate) fn response(&self) -> Option<&TranslationTaskResponseRecord> {
        self.response.as_ref()
    }

    fn into_raw_assistant(self) -> Option<Arc<String>> {
        self.response
            .map(TranslationTaskResponseRecord::into_raw_assistant)
    }
}

/// Executor 的正常结果及其旁路证据。
#[derive(Debug)]
pub(crate) struct TranslationTaskExecution {
    state: TranslationTaskExecutionState,
    evidence: TranslationTaskExecutionEvidence,
}

#[derive(Debug)]
pub(crate) enum TranslationTaskExecutionState {
    Started(TranslationTaskOutcome),
    AdmissionStopped,
}

impl TranslationTaskExecution {
    pub(crate) fn new(
        outcome: TranslationTaskOutcome,
        evidence: TranslationTaskExecutionEvidence,
    ) -> Self {
        assert_ne!(
            evidence.attempt_count(),
            0,
            "模型任务结果必须对应至少一次真实外部 attempt"
        );
        Self {
            state: TranslationTaskExecutionState::Started(outcome),
            evidence,
        }
    }

    pub(crate) fn admission_stopped(evidence: TranslationTaskExecutionEvidence) -> Self {
        assert_eq!(
            evidence.attempt_count(),
            0,
            "请求准入停止不得携带模型 attempt"
        );
        Self {
            state: TranslationTaskExecutionState::AdmissionStopped,
            evidence,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(outcome: TranslationTaskOutcome) -> Self {
        let evidence = TranslationTaskExecutionEvidence::synthetic(outcome.attempts());
        Self {
            state: TranslationTaskExecutionState::Started(outcome),
            evidence,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TranslationTaskExecutionState,
        TranslationTaskExecutionEvidence,
    ) {
        (self.state, self.evidence)
    }

    pub(crate) fn outcome(&self) -> Option<&TranslationTaskOutcome> {
        match &self.state {
            TranslationTaskExecutionState::Started(outcome) => Some(outcome),
            TranslationTaskExecutionState::AdmissionStopped => None,
        }
    }

    pub(crate) const fn admission_was_stopped(&self) -> bool {
        matches!(&self.state, TranslationTaskExecutionState::AdmissionStopped)
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
    UnavailableRejectedCommitted {
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
            Self::UnavailableNoChanges { outcome }
            | Self::UnavailableRejectedCommitted { outcome } => {
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
            | Self::UnavailableRejectedCommitted { outcome }
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
            Self::UnavailableRejectedCommitted { .. } => "unavailable",
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
    task_index: RpgMakerTranslationTaskIndex,
    requested_outputs: usize,
    user_message: String,
    raw_assistant: Option<Arc<String>>,
    state: TranslationTaskRecordFinalState,
    /// 与项目日志共用、已在仍掌握 Unit/Task 事实的边界建立的安全诊断。
    ///
    /// 任务记录只呈现这些报告，不能重新从 Outcome 的业务枚举拼接另一套原因文本。
    outcome_diagnostics: Vec<DiagnosticReport>,
}

impl TranslationTaskRecordDocument {
    pub(crate) fn new(
        task: RpgMakerExecutableTask,
        evidence: TranslationTaskExecutionEvidence,
        state: TranslationTaskRecordFinalState,
    ) -> Self {
        assert!(
            state.outcome_kind_matches_state(),
            "任务记录的完成、部分完成或不可用终态必须与权威 Outcome 种类一致"
        );
        let requested_outputs = task.expected_outputs().len();
        let user_message = task
            .messages()
            .iter()
            .find(|message| message.role() == ChatMessageRole::User)
            .expect("RPG Maker 模型任务必须包含 User 消息")
            .content()
            .to_owned();
        Self {
            task_index: task.index(),
            requested_outputs,
            user_message,
            raw_assistant: evidence.into_raw_assistant(),
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
        self.task_index
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

    fn render(&self, redactor: &ApiKeyRedactor, locale: UiLocale) -> String {
        render_translation_task_record(redactor, locale, self)
    }
}

impl TranslationTaskRecordSink for MarkdownTranslationTaskRecordSink {
    fn submit(&self, document: TranslationTaskRecordDocument) {
        MarkdownTranslationTaskRecordSink::submit(self, document);
    }
}

fn render_translation_task_record(
    api_key_redactor: &ApiKeyRedactor,
    locale: UiLocale,
    document: &TranslationTaskRecordDocument,
) -> String {
    let localizer = UiLocalizer::new(locale);
    let mut output = format!(
        "# {}\n\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordTitle),
        task_record_text(&localizer, UiMessage::TaskRecordFinalResultHeading)
    );
    render_final_result(&mut output, &localizer, api_key_redactor, document);

    output.push_str("\n## User\n\n");
    let user = api_key_redactor
        .redact_text_with_markdown_ascii_punctuation_escaped(&document.user_message);
    output.push_str(&user);
    if !user.ends_with('\n') {
        output.push('\n');
    }

    if let Some(raw_assistant) = &document.raw_assistant {
        output.push_str("\n## Assistant\n\n");
        let assistant = api_key_redactor.redact_text_with_json_strings(raw_assistant);
        output.push_str(&markdown_json_fence(&assistant));
    }
    output
}

fn render_final_result(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    document: &TranslationTaskRecordDocument,
) {
    let accepted = document
        .state
        .outcome()
        .map(TranslationTaskOutcome::accepted)
        .unwrap_or_default();
    let summary = TranslationTaskRecordOutputSummary::new(
        document.requested_outputs,
        accepted.iter().map(|decision| decision.id().get()),
    );
    let accepted_ids = task_record_output_ids(summary.accepted());
    let unaccepted_ids = task_record_output_ids(summary.unaccepted());
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

    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordRequested {
                requested: summary.requested() as u64,
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
                            ids: &accepted_ids,
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
                            ids: &accepted_ids,
                        }
                    )
                );
            }
        }
    } else {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                localizer,
                UiMessage::TaskRecordAcceptedWritten {
                    accepted: 0,
                    written: 0,
                    ids: &accepted_ids,
                }
            )
        );
    }
    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordUnaccepted {
                unaccepted: summary.unaccepted().len() as u64,
                ids: &unaccepted_ids,
            }
        )
    );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_evidence_keeps_only_attempt_count_and_raw_assistant() {
        let evidence = TranslationTaskExecutionEvidence::from_execution(
            2,
            Some(TranslationTaskResponseRecord::new(
                "raw response".to_owned(),
            )),
        );

        assert_eq!(evidence.attempt_count(), 2);
        assert_eq!(
            evidence
                .into_raw_assistant()
                .expect("应保留原始 Assistant")
                .as_str(),
            "raw response"
        );
    }
}
