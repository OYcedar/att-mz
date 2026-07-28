//! ATT 托管翻译任务的可读、非权威旁路记录。
//!
//! Standard 与 Managed 各自的一个已开始 TaskBlock 最多形成一个不可变 Markdown 文档。
//! 模型请求、响应解析、逐 ID 验收和数据库提交仍分别由原有语义所有者负责；本模块只接收
//! 它们建立的确定事实并呈现，不参与恢复、重放、验收、提交或退出码判断。

use std::fmt::Write as _;
#[cfg(test)]
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::llm_request::LlmRequestAttemptOutcome as TranslationTaskAttemptOutcome;
pub(crate) use crate::execution::llm_request::{
    LlmRequestAttemptRecord as TranslationTaskAttemptRecord,
    LlmRequestRetryWaitRecord as TranslationTaskRetryWaitRecord,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::json_diagnostic::JsonErrorCategory;
use crate::llm::{ApiKeyRedactor, ChatMessage, ChatMessageRole, LlmClientRecordMetadata};
#[cfg(test)]
use crate::llm::{LlmFinishReason, LlmResponse, LlmUsage};
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::project_log::ProjectLogger;
#[cfg(test)]
pub(crate) use crate::translation_protocol::TranslationTaskResponseJsonErrorCategory;
pub(crate) use crate::translation_protocol::{
    TranslationAssistantValueError, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};

use super::standard::{
    StandardTranslationTaskIndex, TranslationProtocolDiagnostic, TranslationTaskBlock,
    TranslationTaskOutcome, TranslationTaskUnavailableReason, TranslationUnitRejectionReason,
};

/// Assistant JSON 中保持原始顺序的一个条目。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationAssistantEntry {
    id: String,
    value: Value,
    canonical_id: Option<usize>,
    value_error: Option<TranslationAssistantValueError>,
}

impl TranslationAssistantEntry {
    #[cfg(test)]
    pub(crate) fn new(id: String, value: Value) -> Self {
        Self {
            id,
            value,
            canonical_id: None,
            value_error: None,
        }
    }

    pub(crate) fn projected(
        id: String,
        value: Value,
        canonical_id: Option<usize>,
        value_error: Option<TranslationAssistantValueError>,
    ) -> Self {
        Self {
            id,
            value,
            canonical_id,
            value_error,
        }
    }
}

/// 唯一响应解析器建立的任务记录投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskResponseRecord {
    raw_assistant: String,
    thinking: Option<String>,
    ordered_entries: Option<Vec<TranslationAssistantEntry>>,
    parse_error: Option<TranslationTaskResponseParseError>,
}

impl TranslationTaskResponseRecord {
    pub(crate) fn parsed(
        raw_assistant: String,
        thinking: Option<String>,
        ordered_entries: Vec<TranslationAssistantEntry>,
    ) -> Self {
        Self {
            raw_assistant,
            thinking,
            ordered_entries: Some(ordered_entries),
            parse_error: None,
        }
    }

    pub(crate) fn invalid(
        raw_assistant: String,
        parse_error: TranslationTaskResponseParseError,
    ) -> Self {
        Self {
            raw_assistant,
            thinking: None,
            ordered_entries: None,
            parse_error: Some(parse_error),
        }
    }

    pub(crate) fn unprocessed(raw_assistant: String) -> Self {
        Self {
            raw_assistant,
            thinking: None,
            ordered_entries: None,
            parse_error: None,
        }
    }
}

/// 一个已启动任务从开始到 Executor 返回的不可变执行证据。
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Debug)]
pub(crate) struct TranslationTaskExecutionFailure<E> {
    source: E,
    evidence: TranslationTaskExecutionEvidence,
    diagnostic: Option<SafeDiagnostic>,
    cancelled: bool,
}

impl<E> TranslationTaskExecutionFailure<E> {
    pub(crate) fn new(
        source: E,
        evidence: TranslationTaskExecutionEvidence,
        diagnostic: Option<SafeDiagnostic>,
        cancelled: bool,
    ) -> Self {
        Self {
            source,
            evidence,
            diagnostic,
            cancelled,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        E,
        TranslationTaskExecutionEvidence,
        Option<SafeDiagnostic>,
        bool,
    ) {
        (self.source, self.evidence, self.diagnostic, self.cancelled)
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
    diagnostic: Option<SafeDiagnostic>,
}

impl<E> TranslationTaskCommitFailure<E> {
    pub(crate) fn new(
        source: E,
        impact: TranslationTaskCommitFailureImpact,
        diagnostic: Option<SafeDiagnostic>,
    ) -> Self {
        Self {
            source,
            impact,
            diagnostic,
        }
    }

    pub(crate) fn not_applied(source: E, diagnostic: Option<SafeDiagnostic>) -> Self {
        Self::new(
            source,
            TranslationTaskCommitFailureImpact::NotApplied,
            diagnostic,
        )
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        E,
        TranslationTaskCommitFailureImpact,
        Option<SafeDiagnostic>,
    ) {
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
        diagnostic: Option<SafeDiagnostic>,
    },
    CommitNotApplied {
        outcome: Arc<TranslationTaskOutcome>,
        phase: TranslationTaskCommitPhase,
        diagnostic: Option<SafeDiagnostic>,
    },
    CommitOutcomeUnknown {
        outcome: Arc<TranslationTaskOutcome>,
        diagnostic: Option<SafeDiagnostic>,
    },
    NotCommittedAfterEarlierFailure {
        outcome: Arc<TranslationTaskOutcome>,
    },
    InvalidResultNoChanges {
        outcome: Arc<TranslationTaskOutcome>,
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
            | Self::NotCommittedAfterEarlierFailure { outcome }
            | Self::InvalidResultNoChanges { outcome } => Some(outcome),
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

    fn diagnostic(&self) -> Option<&SafeDiagnostic> {
        match self {
            Self::ExecutionFailedNoChanges { diagnostic }
            | Self::CommitNotApplied { diagnostic, .. }
            | Self::CommitOutcomeUnknown { diagnostic, .. } => diagnostic.as_ref(),
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
    task: TranslationTaskBlock,
    evidence: TranslationTaskExecutionEvidence,
    total_duration: Duration,
    state: TranslationTaskRecordFinalState,
}

impl TranslationTaskRecordDocument {
    pub(crate) fn new(
        total_tasks: usize,
        task: TranslationTaskBlock,
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
        }
    }

    pub(crate) const fn task_index(&self) -> StandardTranslationTaskIndex {
        self.task.index()
    }

    #[cfg(test)]
    pub(crate) const fn final_state(&self) -> &TranslationTaskRecordFinalState {
        &self.state
    }
}

/// 一轮 Translate 内跨 Standard 与 Managed 共用的零基自然序。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunWideTranslationTaskIndex(usize);

impl RunWideTranslationTaskIndex {
    pub(crate) const fn new(zero_based: usize) -> Self {
        Self(zero_based)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

/// Managed TaskBlock 内一个临时 ID 对应的 Lua unit 身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTaskUnitIdentity {
    id: usize,
    targets: Vec<ManagedTranslationTaskUnitTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTaskUnitTarget {
    collection: String,
    key: String,
}

impl ManagedTranslationTaskUnitIdentity {
    pub(crate) fn new(id: usize, targets: Vec<ManagedTranslationTaskUnitTarget>) -> Self {
        assert!(id != 0, "Managed 任务记录 ID 必须从 1 开始");
        assert!(
            !targets.is_empty(),
            "Managed 任务记录 ID 必须至少映射一个 unit"
        );
        Self { id, targets }
    }
}

impl ManagedTranslationTaskUnitTarget {
    pub(crate) fn new(collection: impl Into<String>, key: impl Into<String>) -> Self {
        let collection = collection.into();
        let key = key.into();
        assert!(
            !collection.is_empty(),
            "Managed 任务记录 collection 不得为空"
        );
        assert!(!key.is_empty(), "Managed 任务记录 key 不得为空");
        Self { collection, key }
    }
}

/// Managed 逐 ID 验收边界已经确认的结构化结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTaskUnitResult {
    id: usize,
    status: ManagedTranslationTaskUnitStatus,
}

impl ManagedTranslationTaskUnitResult {
    pub(crate) fn accepted(id: usize) -> Self {
        assert!(id != 0, "Managed 任务记录 ID 必须从 1 开始");
        Self {
            id,
            status: ManagedTranslationTaskUnitStatus::Accepted,
        }
    }

    pub(crate) fn rejected(id: usize, reason: impl Into<String>, details: Option<Value>) -> Self {
        assert!(id != 0, "Managed 任务记录 ID 必须从 1 开始");
        let reason = reason.into();
        assert!(!reason.is_empty(), "Managed 任务记录拒绝原因不得为空");
        Self {
            id,
            status: ManagedTranslationTaskUnitStatus::Rejected { reason, details },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManagedTranslationTaskUnitStatus {
    Accepted,
    Rejected {
        reason: String,
        details: Option<Value>,
    },
}

/// Managed 顺序 checkpoint 建立的终态；状态码与 Standard 记录共用同一呈现契约。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationTaskCheckpointState {
    Complete,
    Partial,
    Unavailable,
    ExecutionFailed,
    CommitPreparationFailed,
    CommitNotApplied,
    OutcomeUnknown,
    EarlierFailure,
    Cancelled,
}

impl ManagedTranslationTaskCheckpointState {
    const fn code(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
            Self::ExecutionFailed => "execution_failed",
            Self::CommitPreparationFailed => "commit_preparation_failed",
            Self::CommitNotApplied => "commit_not_applied",
            Self::OutcomeUnknown => "commit_outcome_unknown",
            Self::EarlierFailure => "not_committed_after_earlier_failure",
            Self::Cancelled => "cancelled",
        }
    }

    const fn permits_confirmed_commits(self) -> bool {
        matches!(self, Self::Complete | Self::Partial)
    }

    const fn outcome_unknown(self) -> bool {
        matches!(self, Self::OutcomeUnknown)
    }
}

/// Managed TaskBlock 的逐 ID 验收与顺序 checkpoint 投影。
pub(crate) struct ManagedTranslationTaskRecordFinalState {
    checkpoint: ManagedTranslationTaskCheckpointState,
    units: Vec<ManagedTranslationTaskUnitResult>,
    protocol_diagnostics: Vec<TranslationProtocolDiagnostic>,
    confirmed_committed_units: Option<usize>,
    diagnostic: Option<SafeDiagnostic>,
}

impl ManagedTranslationTaskRecordFinalState {
    pub(crate) fn new(
        checkpoint: ManagedTranslationTaskCheckpointState,
        units: Vec<ManagedTranslationTaskUnitResult>,
        confirmed_committed_units: Option<usize>,
        diagnostic: Option<SafeDiagnostic>,
    ) -> Self {
        assert_eq!(
            checkpoint.outcome_unknown(),
            confirmed_committed_units.is_none(),
            "Managed 提交终态未知必须且只能用未知的已确认提交数表示"
        );
        assert!(
            checkpoint.permits_confirmed_commits()
                || confirmed_committed_units.is_none_or(|count| count == 0),
            "只有 complete 或 partial 终态才能确认 Managed 提交进度"
        );
        Self {
            checkpoint,
            units,
            protocol_diagnostics: Vec::new(),
            confirmed_committed_units,
            diagnostic,
        }
    }

    pub(crate) fn with_protocol_diagnostics(
        mut self,
        diagnostics: Vec<TranslationProtocolDiagnostic>,
    ) -> Self {
        self.protocol_diagnostics = diagnostics;
        self
    }

    fn accepted_count(&self) -> usize {
        self.units
            .iter()
            .filter(|unit| matches!(unit.status, ManagedTranslationTaskUnitStatus::Accepted))
            .count()
    }
}

/// Managed 最终化线交给 sink 的完整不可变文档，不依赖 Standard 的 TaskBlock。
pub(crate) struct ManagedTranslationTaskRecordDocument {
    total_tasks: usize,
    task_index: RunWideTranslationTaskIndex,
    collection: String,
    messages: Vec<ChatMessage>,
    units: Vec<ManagedTranslationTaskUnitIdentity>,
    evidence: TranslationTaskExecutionEvidence,
    total_duration: Duration,
    state: ManagedTranslationTaskRecordFinalState,
}

impl ManagedTranslationTaskRecordDocument {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        total_tasks: usize,
        task_index: RunWideTranslationTaskIndex,
        collection: impl Into<String>,
        messages: Vec<ChatMessage>,
        units: Vec<ManagedTranslationTaskUnitIdentity>,
        evidence: TranslationTaskExecutionEvidence,
        state: ManagedTranslationTaskRecordFinalState,
    ) -> Self {
        assert!(total_tasks != 0, "Managed 任务记录总数不得为零");
        assert!(
            task_index.get() < total_tasks,
            "Managed 任务记录自然序必须落在本轮任务总数内"
        );
        let collection = collection.into();
        assert!(!collection.is_empty(), "Managed collection 名不得为空");
        assert!(!units.is_empty(), "Managed TaskBlock 必须至少包含一个 unit");
        assert!(
            units
                .iter()
                .enumerate()
                .all(|(index, unit)| unit.id == index + 1),
            "Managed 任务记录 ID 必须从 1 连续编号"
        );
        assert!(
            state
                .units
                .iter()
                .all(|result| units.iter().any(|unit| unit.id == result.id)),
            "Managed 逐 ID 结果不得引用任务外 ID"
        );
        assert!(
            state.units.iter().enumerate().all(|(index, result)| {
                !state.units[..index]
                    .iter()
                    .any(|previous| previous.id == result.id)
            }),
            "Managed 逐 ID 结果不得重复"
        );
        let accepted_targets = units
            .iter()
            .filter(|identity| {
                state.units.iter().any(|result| {
                    result.id == identity.id
                        && matches!(result.status, ManagedTranslationTaskUnitStatus::Accepted)
                })
            })
            .map(|identity| identity.targets.len())
            .sum::<usize>();
        assert!(
            state
                .confirmed_committed_units
                .is_none_or(|count| count <= accepted_targets),
            "Managed 已确认提交数不能超过已验收 ID 映射的 unit target 数"
        );
        let total_duration = evidence.elapsed_until_finalization();
        Self {
            total_tasks,
            task_index,
            collection,
            messages,
            units,
            evidence,
            total_duration,
            state,
        }
    }
}

/// Standard 与 Managed 最终化线共用的非权威记录入口。
pub(crate) trait TranslationTaskRecordSink: Send + Sync {
    fn enabled(&self) -> bool {
        true
    }

    /// 在 Managed 计划形成后声明两类任务合计总数；sink 只将它用于最终记录呈现。
    fn declare_total_tasks(&self, _total_tasks: usize) {}

    /// 只接收已经固定的不可变终态文档，不在 Standard 业务编排中执行渲染或文件 I/O。
    fn submit(&self, document: TranslationTaskRecordDocument);

    /// 只接收已经固定的 Managed 不可变终态文档。
    fn submit_managed(&self, _document: ManagedTranslationTaskRecordDocument) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoOpTranslationTaskRecordSink;

impl TranslationTaskRecordSink for NoOpTranslationTaskRecordSink {
    fn enabled(&self) -> bool {
        false
    }

    fn submit(&self, _document: TranslationTaskRecordDocument) {}
}

/// 组合根按显式配置建立的单一生产 sink。
#[derive(Clone)]
pub(crate) enum ConfiguredTranslationTaskRecordSink {
    Disabled(NoOpTranslationTaskRecordSink),
    Markdown(Box<MarkdownTranslationTaskRecordSink>),
}

impl ConfiguredTranslationTaskRecordSink {
    pub(crate) const fn disabled() -> Self {
        Self::Disabled(NoOpTranslationTaskRecordSink)
    }

    /// 在 Standard 与 Translate Lua 的业务终态全部固定后排空旁路文档。
    pub(crate) async fn finish(&self) {
        if let Self::Markdown(sink) = self {
            sink.finish().await;
        }
    }
}

impl TranslationTaskRecordSink for ConfiguredTranslationTaskRecordSink {
    fn enabled(&self) -> bool {
        match self {
            Self::Disabled(sink) => sink.enabled(),
            Self::Markdown(sink) => sink.enabled(),
        }
    }

    fn submit(&self, document: TranslationTaskRecordDocument) {
        match self {
            Self::Disabled(sink) => sink.submit(document),
            Self::Markdown(sink) => sink.submit(document),
        }
    }

    fn declare_total_tasks(&self, total_tasks: usize) {
        match self {
            Self::Disabled(sink) => sink.declare_total_tasks(total_tasks),
            Self::Markdown(sink) => sink.declare_total_tasks(total_tasks),
        }
    }

    fn submit_managed(&self, document: ManagedTranslationTaskRecordDocument) {
        match self {
            Self::Disabled(sink) => sink.submit_managed(document),
            Self::Markdown(sink) => sink.submit_managed(document),
        }
    }
}

enum BufferedTranslationTaskRecordDocument {
    Standard(TranslationTaskRecordDocument),
    Managed(ManagedTranslationTaskRecordDocument),
}

impl BufferedTranslationTaskRecordDocument {
    const fn task_index(&self) -> RunWideTranslationTaskIndex {
        match self {
            Self::Standard(document) => {
                RunWideTranslationTaskIndex::new(document.task_index().get())
            }
            Self::Managed(document) => document.task_index,
        }
    }

    const fn declared_total_tasks(&self) -> usize {
        match self {
            Self::Standard(document) => document.total_tasks,
            Self::Managed(document) => document.total_tasks,
        }
    }
}

#[derive(Default)]
struct PendingTranslationTaskRecords {
    total_tasks: usize,
    documents: Vec<BufferedTranslationTaskRecordDocument>,
}

/// 生产 Markdown sink；文件故障只并入项目日志健康状态。
#[derive(Clone)]
pub(crate) struct MarkdownTranslationTaskRecordSink {
    directory: PathBuf,
    run_id: String,
    client: LlmClientRecordMetadata,
    locale: UiLocale,
    file_system: SystemFileSystem,
    warnings: ProjectLogger,
    pending: Arc<Mutex<PendingTranslationTaskRecords>>,
}

impl MarkdownTranslationTaskRecordSink {
    pub(crate) fn new(
        directory: PathBuf,
        run_id: String,
        client: LlmClientRecordMetadata,
        locale: UiLocale,
        file_system: SystemFileSystem,
        warnings: ProjectLogger,
    ) -> Self {
        Self {
            directory,
            run_id,
            client,
            locale,
            file_system,
            warnings,
            pending: Arc::new(Mutex::new(PendingTranslationTaskRecords::default())),
        }
    }

    async fn finish(&self) {
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *pending)
        };
        let total_tasks =
            pending
                .documents
                .iter()
                .fold(pending.total_tasks, |total_tasks, document| {
                    total_tasks
                        .max(document.declared_total_tasks())
                        .max(document.task_index().get().saturating_add(1))
                });
        let mut writes = FuturesUnordered::new();
        for document in pending.documents {
            writes.push(self.write(document, total_tasks));
        }
        while writes.next().await.is_some() {}
        if let Err(error) = self.file_system.shutdown().await {
            let redactor = self.client.api_key_redactor();
            let diagnostics = error
                .into_failure_report(
                    DiagnosticStage::Logging,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .public_diagnostics()
                .cloned()
                .map(|diagnostic| diagnostic.map_dynamic_text(|value| redactor.redact(value)))
                .collect::<Vec<_>>();
            self.warnings.record_task_record_failures(diagnostics);
        }
    }

    async fn write(&self, document: BufferedTranslationTaskRecordDocument, total_tasks: usize) {
        let path = self.directory.join(format!(
            "task-{:06}.md",
            document.task_index().get().saturating_add(1)
        ));
        let markdown = match &document {
            BufferedTranslationTaskRecordDocument::Standard(document) => {
                render_standard_task_record(
                    &self.run_id,
                    &self.client,
                    self.locale,
                    document,
                    total_tasks,
                )
            }
            BufferedTranslationTaskRecordDocument::Managed(document) => render_managed_task_record(
                &self.run_id,
                &self.client,
                self.locale,
                document,
                total_tasks,
            ),
        };
        let markdown = match markdown {
            Ok(markdown) => markdown,
            Err(error) => {
                let path = self
                    .client
                    .api_key_redactor()
                    .redact(&path.to_string_lossy());
                let category = JsonErrorCategory::from(&error);
                self.warnings.record_task_record_failure(
                    SafeDiagnostic::new(
                        DiagnosticCode::LogSerialize,
                        DiagnosticStage::Logging,
                        DiagnosticSubject::path(&path),
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::RequestSerializationFailed,
                            format!(
                                "json_category={category}; line={}; column={}",
                                error.line(),
                                error.column()
                            ),
                        ),
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::ReportBug,
                    )
                    .with_recovery(RecoveryFact::path(&path)),
                );
                return;
            }
        };
        if let Err(error) = self
            .file_system
            .write_new_terminal_observation_file(path.clone(), markdown.into_bytes())
            .await
        {
            let diagnostics = error
                .into_failure_report(
                    DiagnosticStage::Logging,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .public_diagnostics()
                .cloned()
                .map(|diagnostic| {
                    let redactor = self.client.api_key_redactor();
                    let recovery_path = redactor.redact(&path.to_string_lossy());
                    diagnostic
                        .map_dynamic_text(|value| redactor.redact(value))
                        .with_recovery(RecoveryFact::path(recovery_path))
                })
                .collect::<Vec<_>>();
            self.warnings.record_task_record_failures(diagnostics);
        }
    }
}

impl TranslationTaskRecordSink for MarkdownTranslationTaskRecordSink {
    fn submit(&self, document: TranslationTaskRecordDocument) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.total_tasks = pending.total_tasks.max(document.total_tasks);
        pending
            .documents
            .push(BufferedTranslationTaskRecordDocument::Standard(document));
    }

    fn declare_total_tasks(&self, total_tasks: usize) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.total_tasks = pending.total_tasks.max(total_tasks);
    }

    fn submit_managed(&self, document: ManagedTranslationTaskRecordDocument) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.total_tasks = pending.total_tasks.max(document.total_tasks);
        pending
            .documents
            .push(BufferedTranslationTaskRecordDocument::Managed(document));
    }
}

#[cfg(test)]
fn render_task_record(
    run_id: &str,
    client: &LlmClientRecordMetadata,
    locale: UiLocale,
    document: &TranslationTaskRecordDocument,
) -> Result<String, serde_json::Error> {
    render_standard_task_record(run_id, client, locale, document, document.total_tasks)
}

fn render_standard_task_record(
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
        .redact_json_pretty(&Value::Object(client.parameters().clone()))?;
    output.push_str(&markdown_fence(&parameters, "json"));

    for message in document.task.messages() {
        let title = match message.role() {
            ChatMessageRole::System => "System",
            ChatMessageRole::User => "User",
            ChatMessageRole::Assistant => "Assistant",
        };
        let _ = write!(output, "\n## {title}\n\n");
        let content = api_key_redactor.redact(message.content());
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
            render_attempt(&mut output, &localizer, client.api_key_redactor(), attempt)?;
        }
    }

    if let Some(response) = &document.evidence.response {
        if let Some(thinking) = &response.thinking {
            output.push_str("\n## Thinking\n\n");
            let thinking = api_key_redactor.redact(thinking);
            output.push_str(&thinking);
            if !thinking.ends_with('\n') {
                output.push('\n');
            }
        }
        output.push_str("\n## Assistant\n\n");
        if let Some(entries) = &response.ordered_entries {
            if entries.is_empty() {
                let _ = writeln!(
                    output,
                    "_{}_",
                    task_record_text(&localizer, UiMessage::TaskRecordEmptyAssistant)
                );
            }
            for entry in entries {
                let id = api_key_redactor.redact(&entry.id);
                let _ = writeln!(output, "### ID {}\n", markdown_heading_id(&id));
                match &entry.value {
                    Value::Array(values)
                        if values.iter().all(|value| matches!(value, Value::String(_))) =>
                    {
                        for (line_index, value) in values.iter().enumerate() {
                            if line_index != 0 {
                                output.push_str("\n\n");
                            }
                            output.push_str(
                                &api_key_redactor.redact(value.as_str().expect("已验证为字符串")),
                            );
                        }
                        output.push('\n');
                    }
                    value => {
                        let value = client.api_key_redactor().redact_json_pretty(value)?;
                        output.push_str(&markdown_fence(&value, "json"));
                    }
                }
                output.push('\n');
            }
        } else {
            if let Some(error) = &response.parse_error {
                let category = match error.kind {
                    TranslationTaskResponseParseErrorKind::Json(category) => category.code(),
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
            output.push_str(&markdown_fence(
                &api_key_redactor.redact_text_with_json_strings(&response.raw_assistant),
                "text",
            ));
        }
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordFinalResultHeading)
    );
    render_final_result(&mut output, &localizer, client.api_key_redactor(), document)?;

    Ok(output)
}

fn render_managed_task_record(
    run_id: &str,
    client: &LlmClientRecordMetadata,
    locale: UiLocale,
    document: &ManagedTranslationTaskRecordDocument,
    total_tasks: usize,
) -> Result<String, serde_json::Error> {
    let localizer = UiLocalizer::new(locale);
    let api_key_redactor = client.api_key_redactor();
    let ordinal = document.task_index.get().saturating_add(1);
    let accepted = document.state.accepted_count();
    let expected = document.units.len();
    let attempts = document.evidence.attempt_count();

    let mut output = String::new();
    let state_label = task_record_text(
        &localizer,
        UiMessage::TaskRecordStateLabel {
            state: document.state.checkpoint.code(),
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
    let summary = task_record_text(
        &localizer,
        UiMessage::TaskRecordSummaryWithoutWritten {
            ordinal: ordinal as u64,
            total: total_tasks as u64,
            attempts: attempts as u64,
            accepted: accepted as u64,
            expected: expected as u64,
        },
    );
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

    output.push_str("\n## Managed\n\n");
    let collection = markdown_inline_code(&api_key_redactor.redact(&document.collection));
    let _ = writeln!(output, "- Collection: {collection}");
    output.push_str("- ID → collection/key:\n");
    for unit in &document.units {
        let id = markdown_inline_code(&unit.id.to_string());
        for target in &unit.targets {
            let collection = markdown_inline_code(&api_key_redactor.redact(&target.collection));
            let key = markdown_inline_code(&api_key_redactor.redact(&target.key));
            let _ = writeln!(output, "  - {id} → {collection}/{key}");
        }
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordCustomParametersHeading)
    );
    let parameters =
        api_key_redactor.redact_json_pretty(&Value::Object(client.parameters().clone()))?;
    output.push_str(&markdown_fence(&parameters, "json"));

    render_managed_messages_and_execution(
        &mut output,
        &localizer,
        api_key_redactor,
        &document.messages,
        &document.evidence,
    )?;

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordFinalResultHeading)
    );
    render_managed_final_result(&mut output, &localizer, api_key_redactor, document)?;

    Ok(output)
}

fn render_managed_messages_and_execution(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    messages: &[ChatMessage],
    evidence: &TranslationTaskExecutionEvidence,
) -> Result<(), serde_json::Error> {
    for message in messages {
        let title = match message.role() {
            ChatMessageRole::System => "System",
            ChatMessageRole::User => "User",
            ChatMessageRole::Assistant => "Assistant",
        };
        let _ = write!(output, "\n## {title}\n\n");
        let content = api_key_redactor.redact(message.content());
        output.push_str(&content);
        if !content.ends_with('\n') {
            output.push('\n');
        }
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(localizer, UiMessage::TaskRecordAttemptsHeading)
    );
    if evidence.attempts.is_empty() {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(localizer, UiMessage::TaskRecordNoRequest)
        );
    } else {
        for attempt in &evidence.attempts {
            render_attempt(output, localizer, api_key_redactor, attempt)?;
        }
    }

    let Some(response) = &evidence.response else {
        return Ok(());
    };
    if let Some(thinking) = &response.thinking {
        output.push_str("\n## Thinking\n\n");
        let thinking = api_key_redactor.redact(thinking);
        output.push_str(&thinking);
        if !thinking.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str("\n## Assistant\n\n");
    if let Some(entries) = &response.ordered_entries {
        if entries.is_empty() {
            let _ = writeln!(
                output,
                "_{}_",
                task_record_text(localizer, UiMessage::TaskRecordEmptyAssistant)
            );
        }
        for entry in entries {
            let id = api_key_redactor.redact(&entry.id);
            let _ = writeln!(output, "### ID {}\n", markdown_heading_id(&id));
            match &entry.value {
                Value::Array(values)
                    if values.iter().all(|value| matches!(value, Value::String(_))) =>
                {
                    for (line_index, value) in values.iter().enumerate() {
                        if line_index != 0 {
                            output.push_str("\n\n");
                        }
                        output.push_str(
                            &api_key_redactor.redact(value.as_str().expect("已验证为字符串")),
                        );
                    }
                    output.push('\n');
                }
                value => {
                    let value = api_key_redactor.redact_json_pretty(value)?;
                    output.push_str(&markdown_fence(&value, "json"));
                }
            }
            output.push('\n');
        }
    } else {
        if let Some(error) = &response.parse_error {
            let category = match error.kind {
                TranslationTaskResponseParseErrorKind::Json(category) => category.code(),
                _ => "",
            };
            let _ = writeln!(
                output,
                "> {}\n",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordParseError {
                        kind: error.kind.code(),
                        category,
                        line: error.line.get() as u64,
                        column: error.column.get() as u64,
                    }
                )
            );
        }
        output.push_str(&markdown_fence(
            &api_key_redactor.redact_text_with_json_strings(&response.raw_assistant),
            "text",
        ));
    }
    Ok(())
}

fn render_managed_final_result(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    document: &ManagedTranslationTaskRecordDocument,
) -> Result<(), serde_json::Error> {
    let checkpoint = document.state.checkpoint.code();
    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordFinalStatus { state: checkpoint }
        )
    );
    let checkpoint = markdown_inline_code(checkpoint);
    let _ = writeln!(output, "- Managed checkpoint: {checkpoint}");
    let accepted = document.state.accepted_count();
    match document.state.confirmed_committed_units {
        Some(committed) => {
            let _ = writeln!(
                output,
                "- IDs: accepted `{accepted}/{}`; confirmed committed unit targets `{committed}`",
                document.units.len()
            );
        }
        None => {
            let _ = writeln!(
                output,
                "- IDs: accepted `{accepted}/{}`; unit-target commit outcome `unknown`",
                document.units.len()
            );
        }
    }
    output.push_str("- Unit acceptance:\n");
    for identity in &document.units {
        let id = markdown_inline_code(&identity.id.to_string());
        let result = document
            .state
            .units
            .iter()
            .find(|result| result.id == identity.id);
        for target in &identity.targets {
            let collection = markdown_inline_code(&api_key_redactor.redact(&target.collection));
            let key = markdown_inline_code(&api_key_redactor.redact(&target.key));
            match result.map(|result| &result.status) {
                Some(ManagedTranslationTaskUnitStatus::Accepted) => {
                    let _ = writeln!(
                        output,
                        "  - ID {id} → collection {collection}, key {key}: `accepted`"
                    );
                }
                Some(ManagedTranslationTaskUnitStatus::Rejected { reason, details }) => {
                    let reason = markdown_inline_code(&api_key_redactor.redact(reason));
                    let _ = write!(
                        output,
                        "  - ID {id} → collection {collection}, key {key}: `rejected`; reason {reason}"
                    );
                    if let Some(details) = details {
                        let details = api_key_redactor.redact_json(details)?;
                        let details = markdown_inline_code(&details);
                        let _ = write!(output, "; details {details}");
                    }
                    output.push('\n');
                }
                None => {
                    let _ = writeln!(
                        output,
                        "  - ID {id} → collection {collection}, key {key}: `not_evaluated`"
                    );
                }
            }
        }
    }
    for diagnostic in &document.state.protocol_diagnostics {
        let diagnostic = protocol_diagnostic(localizer, api_key_redactor, diagnostic);
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                localizer,
                UiMessage::TaskRecordProtocolDiagnostic {
                    diagnostic: &diagnostic,
                }
            )
        );
    }
    if let Some(diagnostic) = &document.state.diagnostic {
        let reason = markdown_inline_code(
            &api_key_redactor.redact(&diagnostic.reason.render_localized(localizer)),
        );
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                localizer,
                UiMessage::TaskRecordTaskDiagnostic {
                    code: diagnostic.code.as_str(),
                    reason: &reason,
                }
            )
        );
    }
    Ok(())
}

fn render_attempt(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    attempt: &TranslationTaskAttemptRecord,
) -> Result<(), serde_json::Error> {
    let number = attempt.attempt.get();
    let duration = render_duration(localizer, attempt.duration);
    match &attempt.outcome {
        TranslationTaskAttemptOutcome::Succeeded {
            finish_reason,
            provider_request_id,
            provider_response_id,
            usage,
        } => {
            let finish_reason =
                markdown_inline_code(&api_key_redactor.redact(&finish_reason.to_string()));
            let _ = write!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptSucceeded {
                        number: number as u64,
                        finish_reason: &finish_reason,
                    }
                )
            );
            if let Some(usage) = usage {
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptTokenUsage {
                        prompt: usage.prompt_tokens(),
                        completion: usage.completion_tokens(),
                        total: usage.total_tokens(),
                    },
                ));
            }
            output.push_str(&task_record_text(
                localizer,
                UiMessage::TaskRecordAttemptDuration {
                    duration: &duration,
                },
            ));
            if let Some(request_id) = provider_request_id {
                let request_id = markdown_inline_code(&api_key_redactor.redact(request_id));
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRequestId {
                        request_id: &request_id,
                    },
                ));
            }
            if let Some(response_id) = provider_response_id {
                let response_id = markdown_inline_code(&api_key_redactor.redact(response_id));
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptResponseId {
                        response_id: &response_id,
                    },
                ));
            }
            output.push('\n');
        }
        TranslationTaskAttemptOutcome::Retryable {
            diagnostic,
            retry_after,
            retry_wait,
        } => {
            let _ = write!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRetryable {
                        number: number as u64,
                        code: diagnostic.code.as_str(),
                        duration: &duration,
                    }
                )
            );
            if let Some(retry_after) = retry_after {
                let retry_after = render_duration(localizer, *retry_after);
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRetryAfter {
                        duration: &retry_after,
                    },
                ));
            }
            if let Some(retry_wait) = retry_wait {
                let rendered = match retry_wait {
                    TranslationTaskRetryWaitRecord::Retried { duration } => {
                        let duration = render_duration(localizer, *duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitRetry {
                                duration: &duration,
                            },
                        )
                    }
                    TranslationTaskRetryWaitRecord::CompletedBeforeNextAttempt { duration } => {
                        let duration = render_duration(localizer, *duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitCompleted {
                                duration: &duration,
                            },
                        )
                    }
                    TranslationTaskRetryWaitRecord::CancelledWhileWaiting { planned_duration } => {
                        let duration = render_duration(localizer, *planned_duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitCancelled {
                                duration: &duration,
                            },
                        )
                    }
                };
                output.push_str(&rendered);
            }
            output.push('\n');
            render_diagnostic_reason(output, localizer, api_key_redactor, diagnostic)?;
        }
        TranslationTaskAttemptOutcome::Failed { diagnostic } => {
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptFailed {
                        number: number as u64,
                        code: diagnostic.code.as_str(),
                        duration: &duration,
                    }
                )
            );
            render_diagnostic_reason(output, localizer, api_key_redactor, diagnostic)?;
        }
        TranslationTaskAttemptOutcome::Cancelled => {
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptCancelled {
                        number: number as u64,
                        duration: &duration,
                    }
                )
            );
        }
    }
    Ok(())
}

fn render_diagnostic_reason(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    diagnostic: &SafeDiagnostic,
) -> Result<(), serde_json::Error> {
    let reason = markdown_inline_code(
        &api_key_redactor.redact(&diagnostic.reason.render_localized(localizer)),
    );
    let _ = writeln!(
        output,
        "  - {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordStructuredReason { reason: &reason }
        )
    );
    Ok(())
}

fn render_final_result(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    document: &TranslationTaskRecordDocument,
) -> Result<(), serde_json::Error> {
    let response_parse_error = document
        .evidence
        .response
        .as_ref()
        .and_then(|response| response.parse_error.as_ref());
    let response_parse_error_message =
        response_parse_error.map(|parse_error| parse_error.business_message());
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
        if !outcome.unresolved().is_empty() {
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(localizer, UiMessage::TaskRecordRejectedHeading)
            );
            for unresolved in outcome.unresolved() {
                let reason = if matches!(
                    (unresolved.reason(), response_parse_error_message.as_deref()),
                    (
                        TranslationUnitRejectionReason::InvalidShape { message },
                        Some(parse_error)
                    ) if message == parse_error
                ) {
                    task_record_text(
                        localizer,
                        UiMessage::TaskRecordUnavailableDetail {
                            code: "model_response_unusable",
                        },
                    )
                } else {
                    rejection_reason(
                        localizer,
                        api_key_redactor,
                        unresolved.reason(),
                        response_value_error_for_id(document, unresolved.id()),
                    )
                };
                let id =
                    markdown_inline_code(&api_key_redactor.redact(&unresolved.id().to_string()));
                let _ = writeln!(
                    output,
                    "  - {}",
                    task_record_text(
                        localizer,
                        UiMessage::TaskRecordRejectedItem {
                            id: &id,
                            reason: &reason,
                        }
                    )
                );
            }
        }
        for diagnostic in outcome.diagnostics() {
            if matches!(
                (diagnostic, response_parse_error_message.as_deref()),
                (
                    TranslationProtocolDiagnostic::InvalidResponse { message },
                    Some(parse_error)
                ) if message == parse_error
            ) {
                continue;
            }
            let diagnostic = protocol_diagnostic(localizer, api_key_redactor, diagnostic);
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordProtocolDiagnostic {
                        diagnostic: &diagnostic,
                    }
                )
            );
        }
        if let TranslationTaskOutcome::Unavailable { reason, .. } = outcome {
            let reason = unavailable_reason(localizer, reason);
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordUnavailableReason { reason: &reason }
                )
            );
        }
    }
    if let Some(diagnostic) = document.state.diagnostic() {
        let reason = markdown_inline_code(
            &api_key_redactor.redact(&diagnostic.reason.render_localized(localizer)),
        );
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                localizer,
                UiMessage::TaskRecordTaskDiagnostic {
                    code: diagnostic.code.as_str(),
                    reason: &reason,
                }
            )
        );
    }
    Ok(())
}

fn response_value_error_for_id(
    document: &TranslationTaskRecordDocument,
    id: usize,
) -> Option<TranslationAssistantValueError> {
    let entries = document
        .evidence
        .response
        .as_ref()?
        .ordered_entries
        .as_ref()?;
    let mut matches = entries
        .iter()
        .filter(|entry| entry.canonical_id == Some(id));
    let entry = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    entry.value_error
}

fn rejection_reason(
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    reason: &TranslationUnitRejectionReason,
    value_error: Option<TranslationAssistantValueError>,
) -> String {
    let (code, line, expected, actual, detail, expected_blank) = match reason {
        TranslationUnitRejectionReason::Missing => ("missing", 0, 0, 0, String::new(), ""),
        TranslationUnitRejectionReason::Duplicate => ("duplicate", 0, 0, 0, String::new(), ""),
        TranslationUnitRejectionReason::InvalidShape { message } => match value_error {
            Some(TranslationAssistantValueError::NotStringArray) => {
                ("invalid_shape_array", 0, 0, 0, String::new(), "")
            }
            Some(TranslationAssistantValueError::NonStringItem { item }) => {
                ("invalid_shape_item", item.get(), 0, 0, String::new(), "")
            }
            None => (
                "invalid_shape",
                0,
                0,
                0,
                markdown_inline_code(&api_key_redactor.redact(message)),
                "",
            ),
        },
        TranslationUnitRejectionReason::LineCountMismatch { expected, actual } => (
            "line_count_mismatch",
            0,
            *expected,
            *actual,
            String::new(),
            "",
        ),
        TranslationUnitRejectionReason::InvalidLineText { line_index } => (
            "invalid_line_text",
            line_index.saturating_add(1),
            0,
            0,
            String::new(),
            "",
        ),
        TranslationUnitRejectionReason::BlankLineMismatch {
            line_index,
            expected_blank,
        } => (
            "blank_line_mismatch",
            line_index.saturating_add(1),
            0,
            0,
            String::new(),
            if *expected_blank {
                "blank"
            } else {
                "non_blank"
            },
        ),
        TranslationUnitRejectionReason::BlankTranslation => {
            ("blank_translation", 0, 0, 0, String::new(), "")
        }
        TranslationUnitRejectionReason::NoNaturalLanguageText => {
            ("no_natural_language_text", 0, 0, 0, String::new(), "")
        }
        TranslationUnitRejectionReason::ContainsByteOrderMark => {
            ("contains_byte_order_mark", 0, 0, 0, String::new(), "")
        }
        TranslationUnitRejectionReason::PlaceholderMismatch { token } => (
            "placeholder_mismatch",
            0,
            0,
            0,
            markdown_inline_code(&api_key_redactor.redact(token)),
            "",
        ),
        TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token } => (
            "unexpected_placeholder",
            0,
            0,
            0,
            markdown_inline_code(&api_key_redactor.redact(token)),
            "",
        ),
        TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { original } => (
            "placeholder_normalization_ambiguous",
            0,
            0,
            0,
            markdown_inline_code(&api_key_redactor.redact(original)),
            "",
        ),
        TranslationUnitRejectionReason::SourceResidual { fragment } => (
            "source_residual",
            0,
            0,
            0,
            markdown_inline_code(&api_key_redactor.redact(fragment)),
            "",
        ),
    };
    task_record_text(
        localizer,
        UiMessage::TaskRecordRejectionReason {
            code,
            line: line as u64,
            expected: expected as u64,
            actual: actual as u64,
            detail: &detail,
            expected_blank,
        },
    )
}

fn protocol_diagnostic(
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    diagnostic: &TranslationProtocolDiagnostic,
) -> String {
    let (code, index, detail) = match diagnostic {
        TranslationProtocolDiagnostic::NonStopFinish { reason } => (
            "non_stop_finish",
            0,
            markdown_inline_code(&api_key_redactor.redact(&reason.to_string())),
        ),
        TranslationProtocolDiagnostic::InvalidResponse { message } => (
            "invalid_response",
            0,
            markdown_inline_code(&api_key_redactor.redact(message)),
        ),
        TranslationProtocolDiagnostic::InvalidId { item_index } => {
            ("invalid_id", item_index.saturating_add(1), String::new())
        }
        TranslationProtocolDiagnostic::UnknownId { item_index, id } => (
            "unknown_id",
            item_index.saturating_add(1),
            markdown_inline_code(&api_key_redactor.redact(&id.to_string())),
        ),
    };
    task_record_text(
        localizer,
        UiMessage::TaskRecordProtocolDetail {
            code,
            index: index as u64,
            detail: &detail,
        },
    )
}

fn unavailable_reason(
    localizer: &UiLocalizer,
    reason: &TranslationTaskUnavailableReason,
) -> String {
    let code = match reason {
        TranslationTaskUnavailableReason::ModelResponseUnusable => "model_response_unusable",
        TranslationTaskUnavailableReason::AllOutputsRejected => "all_outputs_rejected",
        TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. } => {
            "recoverable_request_exhausted"
        }
        TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum { .. } => {
            "retry_after_exceeds_maximum"
        }
    };
    task_record_text(localizer, UiMessage::TaskRecordUnavailableDetail { code })
}

fn markdown_heading_id(id: &str) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        id.to_owned()
    } else {
        markdown_inline_code(id)
    }
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

fn markdown_fence(content: &str, language: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in content.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    let mut output = format!("{fence}{language}\n");
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output.push('\n');
    output
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
    use secrecy::SecretString;
    use serde_json::{Map, json};
    use tempfile::tempdir;
    use url::{Url, form_urlencoded};

    use super::*;
    use crate::diagnostic::{
        DiagnosticCode, DiagnosticFailureKind, DiagnosticReason, DiagnosticSubject,
    };
    use crate::fingerprint::Sha256Fingerprint;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguagePair,
        LanguageText,
    };
    use crate::llm::{ApiKeyRedactor, ChatMessage};
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::runtime::filesystem::SystemFileSystemConfig;
    use crate::runtime::project_log::start_project_log;

    use super::super::executor::FinalLlmResponseMetadata;
    use super::super::standard::{
        AcceptedTranslationDecision, ExpectedLineShape, ExpectedTranslationOutput,
        ExpectedTranslationValidation, NonEmptyTaskItems, TranslationPatch,
        TranslationStateContext, TranslationTaskOutcomeContext, TranslationUnitIdentity,
        UnresolvedTranslationUnit,
    };

    fn language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言应合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
        )
    }

    fn test_identity(index: usize) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
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
            None,
        )
        .analyze_source(&LanguageText::natural("姫"));
        ExpectedTranslationOutput::new(
            id,
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
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    1,
                    TranslationPatch::new(
                        test_identity(1),
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
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    1,
                    TranslationPatch::new(
                        test_identity(1),
                        Vec::new(),
                        TextUnitContent::Value("公主".to_owned()),
                        Sha256Fingerprint::from_bytes([0x33; 32]),
                    ),
                ),
                Vec::new(),
            ),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    2,
                    test_identity(2),
                    Vec::new(),
                    TranslationUnitRejectionReason::Missing,
                ),
                Vec::new(),
            ),
        })
    }

    fn test_unavailable_outcome() -> Arc<TranslationTaskOutcome> {
        Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: None,
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    1,
                    test_identity(1),
                    Vec::new(),
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
            TranslationTaskBlock::new(
                StandardTranslationTaskIndex::new(0),
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

    fn managed_document(
        task_index: usize,
        total_tasks: usize,
        state: ManagedTranslationTaskRecordFinalState,
        response: Option<TranslationTaskResponseRecord>,
    ) -> ManagedTranslationTaskRecordDocument {
        ManagedTranslationTaskRecordDocument::new(
            total_tasks,
            RunWideTranslationTaskIndex::new(task_index),
            "quest_titles",
            vec![
                ChatMessage::new(ChatMessageRole::System, "Managed system"),
                ChatMessage::new(ChatMessageRole::User, "Managed user"),
            ],
            vec![
                ManagedTranslationTaskUnitIdentity::new(
                    1,
                    vec![
                        ManagedTranslationTaskUnitTarget::new("quests", "quest:arrival"),
                        ManagedTranslationTaskUnitTarget::new(
                            "archive_quests",
                            "quest:arrival-copy",
                        ),
                    ],
                ),
                ManagedTranslationTaskUnitIdentity::new(
                    2,
                    vec![ManagedTranslationTaskUnitTarget::new(
                        "quests",
                        "quest:departure",
                    )],
                ),
            ],
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::from_millis(12),
                Vec::new(),
                response,
            ),
            state,
        )
    }

    #[test]
    fn managed_record_renders_structured_identity_protocol_and_checkpoint_facts() {
        const KEY: &str = "managed-secret";
        let document = managed_document(
            2,
            4,
            ManagedTranslationTaskRecordFinalState::new(
                ManagedTranslationTaskCheckpointState::Partial,
                vec![
                    ManagedTranslationTaskUnitResult::accepted(1),
                    ManagedTranslationTaskUnitResult::rejected(
                        2,
                        "placeholder_mismatch",
                        Some(json!({
                            "token": format!("ordinary:{KEY}"),
                            "expected": 1,
                            "actual": 0,
                        })),
                    ),
                ],
                Some(2),
                None,
            )
            .with_protocol_diagnostics(vec![
                TranslationProtocolDiagnostic::UnknownId {
                    item_index: 2,
                    id: 99,
                },
            ]),
            Some(TranslationTaskResponseRecord::parsed(
                format!(r#"{{"1":["抵达"],"2":["ordinary:{KEY}"],"99":["未知"]}}"#),
                Some(format!("why ordinary:{KEY}")),
                vec![
                    TranslationAssistantEntry::projected(
                        "1".to_owned(),
                        json!(["抵达"]),
                        Some(1),
                        None,
                    ),
                    TranslationAssistantEntry::projected(
                        "2".to_owned(),
                        json!([format!("ordinary:{KEY}")]),
                        Some(2),
                        None,
                    ),
                    TranslationAssistantEntry::new("99".to_owned(), json!(["未知"])),
                ],
            )),
        );

        let markdown = render_managed_task_record(
            "run-managed",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
            4,
        )
        .expect("Managed 任务记录应可渲染");

        assert!(markdown.starts_with("# 翻译任务 000003 · 部分完成\n"));
        assert!(markdown.contains("`任务 3/4`"));
        assert!(markdown.contains("- Collection: `quest_titles`"));
        assert!(markdown.contains("`1` → `quests`/`quest:arrival`"));
        assert!(
            markdown.contains("`1` → `archive_quests`/`quest:arrival-copy`"),
            "同一模型 ID 的全部去重扇出目标都必须出现在身份映射中"
        );
        assert!(markdown.contains("`2` → `quests`/`quest:departure`"));
        assert!(markdown.contains("## System\n\nManaged system"));
        assert!(markdown.contains("## User\n\nManaged user"));
        assert!(markdown.contains("## Thinking\n\nwhy ordinary:[REDACTED API KEY]"));
        assert!(markdown.contains("### ID 1\n\n抵达"));
        assert!(markdown.contains("### ID 99\n\n未知"));
        assert!(markdown.contains("- Managed checkpoint: `partial`"));
        assert!(
            markdown.contains("IDs: accepted `1/2`; confirmed committed unit targets `2`"),
            "一个已接受 ID 扇出的两个 unit target 必须按真实提交行数计数"
        );
        assert!(markdown.contains("ID `1` → collection `quests`, key `quest:arrival`: `accepted`"));
        assert!(
            markdown.contains(
                "ID `1` → collection `archive_quests`, key `quest:arrival-copy`: `accepted`"
            ),
            "最终验收结果必须展开同一 ID 的每个 collection/key target"
        );
        assert!(
            markdown.contains(
                "ID `2` → collection `quests`, key `quest:departure`: `rejected`; reason `placeholder_mismatch`"
            )
        );
        assert!(
            markdown.contains("协议诊断：模型第 3 个条目返回了未知 ID `99`"),
            "Managed 记录必须保留响应边界产生的未知 ID 协议诊断"
        );
        assert!(markdown.contains("[REDACTED API KEY]"));
        assert!(!markdown.contains(KEY));
    }

    #[test]
    fn managed_checkpoint_codes_cover_every_ordered_finalization_state() {
        let states = [
            (ManagedTranslationTaskCheckpointState::Complete, "complete"),
            (ManagedTranslationTaskCheckpointState::Partial, "partial"),
            (
                ManagedTranslationTaskCheckpointState::Unavailable,
                "unavailable",
            ),
            (
                ManagedTranslationTaskCheckpointState::ExecutionFailed,
                "execution_failed",
            ),
            (
                ManagedTranslationTaskCheckpointState::CommitPreparationFailed,
                "commit_preparation_failed",
            ),
            (
                ManagedTranslationTaskCheckpointState::CommitNotApplied,
                "commit_not_applied",
            ),
            (
                ManagedTranslationTaskCheckpointState::OutcomeUnknown,
                "commit_outcome_unknown",
            ),
            (
                ManagedTranslationTaskCheckpointState::EarlierFailure,
                "not_committed_after_earlier_failure",
            ),
            (
                ManagedTranslationTaskCheckpointState::Cancelled,
                "cancelled",
            ),
        ];

        assert_eq!(
            states.map(|(state, _)| state.code()),
            states.map(|(_, expected)| expected)
        );
    }

    #[tokio::test]
    async fn sink_uses_one_run_wide_ordinal_and_final_total_for_standard_and_managed() {
        let directory = tempdir().expect("临时目录应可建立");
        let record_directory = directory.path().join("task-records").join("run-combined");
        std::fs::create_dir_all(&record_directory).expect("任务记录目录应可建立");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let log_runtime =
            start_project_log(directory.path().join("logs"), "run-combined".to_owned());
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "run-combined".to_owned(),
            client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            file_system,
            log_runtime.logger(),
        );
        let mut standard = document(
            Vec::new(),
            Vec::new(),
            None,
            TranslationTaskRecordFinalState::CompleteCommitted {
                outcome: test_complete_outcome(),
            },
        );
        standard.total_tasks = 1;
        sink.submit(standard);
        sink.declare_total_tasks(3);
        for task_index in 1..=2 {
            sink.submit_managed(managed_document(
                task_index,
                3,
                ManagedTranslationTaskRecordFinalState::new(
                    ManagedTranslationTaskCheckpointState::Complete,
                    vec![
                        ManagedTranslationTaskUnitResult::accepted(1),
                        ManagedTranslationTaskUnitResult::accepted(2),
                    ],
                    Some(3),
                    None,
                ),
                None,
            ));
        }

        sink.finish().await;

        for ordinal in 1..=3 {
            let path = record_directory.join(format!("task-{ordinal:06}.md"));
            let markdown = std::fs::read_to_string(path).expect("连续编号任务记录应存在");
            assert!(
                markdown.contains(&format!("`任务 {ordinal}/3`")),
                "Standard 和 Managed 文档都必须在落盘时得到最终总数"
            );
        }
        assert!(
            std::fs::read_to_string(record_directory.join("task-000001.md"))
                .expect("Standard 记录应存在")
                .contains("## 最终结果"),
        );
        assert!(
            std::fs::read_to_string(record_directory.join("task-000002.md"))
                .expect("首个 Managed 记录应存在")
                .contains("- Collection: `quest_titles`"),
        );
        drop(log_runtime);
    }

    #[test]
    fn readable_markdown_keeps_native_roles_thinking_and_assistant_meaning() {
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
                ChatMessage::new(ChatMessageRole::Assistant, "已有上下文"),
            ],
            vec![TranslationTaskAttemptRecord::succeeded(
                NonZeroUsize::MIN,
                Duration::from_millis(7),
                &response,
            )],
            Some(TranslationTaskResponseRecord::parsed(
                "raw".to_owned(),
                Some("先核对占位符，再翻译。".to_owned()),
                vec![TranslationAssistantEntry::new(
                    "1".to_owned(),
                    json!(["公主", "第二行"]),
                )],
            )),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges { diagnostic: None },
        );
        let mut parameters = Map::new();
        parameters.insert("temperature".to_owned(), json!(0.2));
        let markdown = render_task_record(
            "run-1",
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
            r#"# 翻译任务 000001 · 执行失败

`任务 1/3` · `尝试 1 次` · `验收 0/0` · `写入 0 处`

- Run ID：`run-1`
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

## Assistant

已有上下文

## 请求过程

- 尝试 1：成功；finish reason `stop`；token `10 / 4 / 14`；耗时 `7 毫秒`

## Thinking

先核对占位符，再翻译。

## Assistant

### ID 1

公主

第二行


## 最终结果

- 状态：执行失败，未提交
"#
        );
    }

    #[test]
    fn duplicate_unknown_invalid_and_missing_ids_remain_readable_without_json_shell() {
        let accepted_identity = test_identity(1);
        let accepted_translation = TextUnitContent::Value("公主".to_owned());
        let accepted = AcceptedTranslationDecision::new(
            1,
            TranslationPatch::new(
                accepted_identity,
                Vec::new(),
                accepted_translation,
                Sha256Fingerprint::from_bytes([0x11; 32]),
            ),
        );
        let outcome = Arc::new(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![
                    TranslationProtocolDiagnostic::UnknownId {
                        item_index: 2,
                        id: 99,
                    },
                    TranslationProtocolDiagnostic::InvalidId { item_index: 3 },
                ],
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(accepted, Vec::new()),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    2,
                    test_identity(2),
                    Vec::new(),
                    TranslationUnitRejectionReason::Duplicate,
                ),
                vec![UnresolvedTranslationUnit::new(
                    3,
                    test_identity(3),
                    Vec::new(),
                    TranslationUnitRejectionReason::Missing,
                )],
            ),
        });
        let task = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(0),
            language_pair(),
            Vec::new(),
            (1..=3).map(test_expected_output).collect(),
        );
        let document = TranslationTaskRecordDocument::new(
            1,
            task,
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                Some(TranslationTaskResponseRecord::parsed(
                    r#"{"1":["第一版"],"1":["第二版"],"99":["未知"],"bad":{"raw":true}}"#
                        .to_owned(),
                    None,
                    vec![
                        TranslationAssistantEntry::new("1".to_owned(), json!(["第一版"])),
                        TranslationAssistantEntry::new("1".to_owned(), json!(["第二版"])),
                        TranslationAssistantEntry::new("99".to_owned(), json!(["未知"])),
                        TranslationAssistantEntry::new("bad".to_owned(), json!({"raw": true})),
                    ],
                )),
            ),
            TranslationTaskRecordFinalState::PartialCommitted { outcome },
        );
        let markdown = render_task_record(
            "run-ids",
            &client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("任务记录应可渲染");

        let headings = markdown
            .lines()
            .filter(|line| line.starts_with("### ID "))
            .collect::<Vec<_>>();
        assert_eq!(
            headings,
            ["### ID 1", "### ID 1", "### ID 99", "### ID bad"],
            "Assistant 必须完整保持重复、未知和非法 ID 的原始条目顺序"
        );
        assert!(markdown.contains("{\n  \"raw\": true\n}"));
        assert!(markdown.contains("- 状态：部分完成，已确认提交"));
        assert!(markdown.contains("  - `2`：重复模型输出"));
        assert!(markdown.contains("  - `3`：缺少模型输出"));
        assert!(markdown.contains("协议诊断：模型第 3 个条目返回了未知 ID `99`"));
        assert!(markdown.contains("协议诊断：模型第 4 个条目的 ID 非法"));
        assert!(!markdown.contains("## Assistant\n\n```json"));
    }

    #[test]
    fn invalid_assistant_uses_dynamic_fence_and_keeps_one_precise_error() {
        let raw = "```json\n{\"1\":[\"译文\"]}\n```\n尾部";
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
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges { diagnostic: None },
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
        assert!(markdown.contains("````text\n```json\n{\"1\":[\"译文\"]}\n```\n尾部\n````"));

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
    fn assistant_value_shape_error_is_localized_with_one_based_item_number() {
        let localizer = UiLocalizer::new(UiLocale::English);
        let redactor = ApiKeyRedactor::new(SecretString::from("unused-key"));
        let reason = rejection_reason(
            &localizer,
            &redactor,
            &TranslationUnitRejectionReason::InvalidShape {
                message: "译文数组第 1 项必须是字符串".to_owned(),
            },
            Some(TranslationAssistantValueError::NonStringItem {
                item: NonZeroUsize::MIN,
            }),
        );

        assert_eq!(
            reason, "Translation array item 1 must be a string",
            "任务记录必须消费结构化数组项错误，而不是权威 outcome 的中文兼容正文"
        );
    }

    #[test]
    fn invalid_response_parse_error_is_rendered_once_for_the_real_unavailable_outcome() {
        let parse_error = "模型响应 JSON 无效：类别 syntax，第 4 行、第 1 列";
        let unresolved = (1..=2)
            .map(|id| {
                UnresolvedTranslationUnit::new(
                    id,
                    test_identity(id),
                    Vec::new(),
                    TranslationUnitRejectionReason::InvalidShape {
                        message: parse_error.to_owned(),
                    },
                )
            })
            .collect::<Vec<_>>();
        let outcome = Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![TranslationProtocolDiagnostic::InvalidResponse {
                    message: parse_error.to_owned(),
                }],
            ),
            final_response: Some(FinalLlmResponseMetadata::new(None, None, "stop", None)),
            reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
            unresolved: NonEmptyTaskItems::new(
                unresolved[0].clone(),
                unresolved.into_iter().skip(1).collect(),
            ),
        });
        let document = TranslationTaskRecordDocument::new(
            1,
            TranslationTaskBlock::new(
                StandardTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                (1..=2).map(test_expected_output).collect(),
            ),
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                Some(TranslationTaskResponseRecord::invalid(
                    "不是 JSON".to_owned(),
                    TranslationTaskResponseParseError::new(
                        TranslationTaskResponseParseErrorKind::Json(
                            TranslationTaskResponseJsonErrorCategory::Syntax,
                        ),
                        NonZeroUsize::new(4).expect("测试行号非零"),
                        NonZeroUsize::MIN,
                    ),
                )),
            ),
            TranslationTaskRecordFinalState::UnavailableNoChanges { outcome },
        );
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
        assert!(markdown.contains("- 不可用原因：模型响应无法解析"));
        assert!(markdown.contains("- 未接受："));
        assert!(markdown.contains("  - `1`：模型响应无法解析"));
        assert!(markdown.contains("  - `2`：模型响应无法解析"));
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
                TranslationTaskRecordFinalState::ExecutionFailedNoChanges { diagnostic: None },
                "execution_failed",
            ),
            (
                TranslationTaskRecordFinalState::CommitNotApplied {
                    outcome: Arc::clone(&complete),
                    phase: TranslationTaskCommitPhase::Preparation,
                    diagnostic: None,
                },
                "commit_preparation_failed",
            ),
            (
                TranslationTaskRecordFinalState::CommitNotApplied {
                    outcome: Arc::clone(&complete),
                    phase: TranslationTaskCommitPhase::Transaction,
                    diagnostic: None,
                },
                "commit_not_applied",
            ),
            (
                TranslationTaskRecordFinalState::CommitOutcomeUnknown {
                    outcome: Arc::clone(&complete),
                    diagnostic: None,
                },
                "commit_outcome_unknown",
            ),
            (
                TranslationTaskRecordFinalState::NotCommittedAfterEarlierFailure {
                    outcome: Arc::clone(&complete),
                },
                "not_committed_after_earlier_failure",
            ),
            (
                TranslationTaskRecordFinalState::InvalidResultNoChanges {
                    outcome: Arc::clone(&complete),
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
        let task = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(0),
            language_pair(),
            Vec::new(),
            vec![test_expected_output(1)],
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
                diagnostic: None,
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
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::component("provider"),
            DiagnosticReason::failure(DiagnosticFailureKind::TransportFailed),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::Retry,
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
        assert!(markdown.contains("原因：`收到有效响应前 HTTP 传输失败`"));
        assert!(
            !markdown.contains(r#"{"kind":"failure""#),
            "任务记录必须渲染可读原因，不能再显示 DiagnosticReason 的 JSON 外壳"
        );
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
            TranslationTaskBlock::new(
                StandardTranslationTaskIndex::new(0),
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
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::component(format!("{NEIGHBOR}:{KEY}")),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::TransportFailed,
                format!("{NEIGHBOR}:{KEY}"),
            ),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::Retry,
        );
        let response = LlmResponse::new(
            format!("{NEIGHBOR}:{KEY}"),
            LlmFinishReason::Other(format!("{NEIGHBOR}:{KEY}")),
            Some(format!("{NEIGHBOR}:{KEY}")),
            Some(format!("{NEIGHBOR}:{KEY}")),
            None,
        );
        let document = document(
            vec![
                ChatMessage::new(ChatMessageRole::System, format!("System {NEIGHBOR}:{KEY}")),
                ChatMessage::new(ChatMessageRole::User, format!("User {NEIGHBOR}:{KEY}")),
                ChatMessage::new(
                    ChatMessageRole::Assistant,
                    format!("History {NEIGHBOR}:{KEY}"),
                ),
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
            Some(TranslationTaskResponseRecord::parsed(
                format!("Raw {NEIGHBOR}:{KEY}"),
                Some(format!("Thinking {NEIGHBOR}:{KEY}")),
                vec![TranslationAssistantEntry::new(
                    format!("{NEIGHBOR}:{KEY}"),
                    json!({format!("{NEIGHBOR}:{KEY}"): format!("{NEIGHBOR}:{KEY}")}),
                )],
            )),
            TranslationTaskRecordFinalState::ExecutionFailedNoChanges {
                diagnostic: Some(diagnostic),
            },
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
        assert!(
            markdown.matches("[REDACTED API KEY]").count() >= 12,
            "每个出现位置都应替换 API key 实际值"
        );
        assert!(
            markdown.matches(NEIGHBOR).count() >= 12,
            "普通相邻文本不得随 API key 一起删除"
        );
    }

    #[test]
    fn final_rejections_and_protocol_diagnostics_redact_keys_and_escape_markdown() {
        const KEY: &str = "api`key";
        const NEIGHBOR: &str = "ordinary``marker";
        let accepted = AcceptedTranslationDecision::new(
            1,
            TranslationPatch::new(
                test_identity(1),
                Vec::new(),
                TextUnitContent::Value("公主".to_owned()),
                Sha256Fingerprint::from_bytes([0x22; 32]),
            ),
        );
        let outcome = Arc::new(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                vec![
                    TranslationProtocolDiagnostic::NonStopFinish {
                        reason: format!("{NEIGHBOR}:{KEY}"),
                    },
                    TranslationProtocolDiagnostic::InvalidResponse {
                        message: format!("{NEIGHBOR}:{KEY}\nprotocol"),
                    },
                ],
            ),
            final_response: FinalLlmResponseMetadata::new(
                None,
                None,
                format!("{NEIGHBOR}:{KEY}"),
                None,
            ),
            accepted: NonEmptyTaskItems::new(accepted, Vec::new()),
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    2,
                    test_identity(2),
                    Vec::new(),
                    TranslationUnitRejectionReason::SourceResidual {
                        fragment: format!("{NEIGHBOR}:{KEY}\nsource"),
                    },
                ),
                vec![
                    UnresolvedTranslationUnit::new(
                        3,
                        test_identity(3),
                        Vec::new(),
                        TranslationUnitRejectionReason::PlaceholderMismatch {
                            token: format!("{NEIGHBOR}:{KEY}"),
                        },
                    ),
                    UnresolvedTranslationUnit::new(
                        4,
                        test_identity(4),
                        Vec::new(),
                        TranslationUnitRejectionReason::InvalidShape {
                            message: format!("{NEIGHBOR}:{KEY}\nshape"),
                        },
                    ),
                ],
            ),
        });
        let document = TranslationTaskRecordDocument::new(
            1,
            TranslationTaskBlock::new(
                StandardTranslationTaskIndex::new(0),
                language_pair(),
                Vec::new(),
                (1..=4).map(test_expected_output).collect(),
            ),
            TranslationTaskExecutionEvidence::new(
                OffsetDateTime::UNIX_EPOCH,
                Duration::ZERO,
                Vec::new(),
                None,
            ),
            TranslationTaskRecordFinalState::PartialCommitted { outcome },
        );
        let markdown = render_task_record(
            "run-final-redaction",
            &client("https://example.test", "model", Map::new(), KEY),
            UiLocale::SimplifiedChinese,
            &document,
        )
        .expect("最终诊断任务记录应可渲染");

        assert!(!markdown.contains(KEY));
        assert!(markdown.matches("[REDACTED API KEY]").count() >= 5);
        assert!(markdown.matches(NEIGHBOR).count() >= 5);
        assert!(
            markdown.contains("```\"ordinary``marker:[REDACTED API KEY]\\nsource\"```"),
            "含反引号和控制字符的动态诊断必须使用足够长的行内代码围栏"
        );
        assert!(
            markdown.contains("```\"ordinary``marker:[REDACTED API KEY]\\nshape\"```"),
            "InvalidShape 兜底详情同样必须保持最终结果列表结构"
        );
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

    #[tokio::test]
    async fn existing_target_is_reported_without_overwrite_or_business_failure() {
        let directory = tempdir().expect("临时目录应可建立");
        let record_directory = directory.path().join("task-records").join("run-conflict");
        std::fs::create_dir_all(&record_directory).expect("任务记录目录应可建立");
        let target = record_directory.join("task-000001.md");
        std::fs::write(&target, "existing").expect("既有任务记录应可建立");

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let log_runtime =
            start_project_log(directory.path().join("logs"), "run-conflict".to_owned());
        let logger = log_runtime.logger();
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory,
            "run-conflict".to_owned(),
            client("https://example.test", "model", Map::new(), "unused-key"),
            UiLocale::SimplifiedChinese,
            file_system,
            logger.clone(),
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
        assert_eq!(
            logger.health().task_record_failures,
            1,
            "任务记录故障必须进入独立、可见但非致命的任务记录健康状态"
        );
        drop(log_runtime);
    }
}
