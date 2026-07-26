//! 每次命令独占一个 JSONL 文件的项目日志运行时。
//!
//! 普通事件进入单 writer 的内部有界队列；队列饱和时生产者自然背压，不丢记录。最终
//! `performance.counters`、`failure.reported` 与 `run.finished` 保存在独立终态槽，
//! worker 排空普通事件后按固定顺序直接写入，因而不会被普通队列压力挤掉。
//! 已建立 writer 的 runtime 即使被意外丢弃，也会使用预登记的安全投影写出未知终态。
//! 日志失败只进入健康状态，不改变业务结果。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use async_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, render_safe_diagnostic,
};
use crate::i18n::{
    UiLocale, UiLocalizer, UiMessage, project_log_task_outcome_label,
    project_log_value_source_label,
};
use crate::user_text::sanitize_user_text;

use super::performance::{RunPerformanceCounters, RunPerformanceSnapshot};
use super::windows::{WindowsFsError, create_directories_without_reparse};

// 这些是单 writer 的内部吞吐策略，不是项目容量或用户策略。普通事件总量没有上限。
const QUEUE_CAPACITY: usize = 8_192;
const WRITER_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectLogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ProjectLogCode {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.finished")]
    RunFinished,
    #[serde(rename = "performance.counters")]
    PerformanceCounters,
    #[serde(rename = "failure.reported")]
    FailureReported,
    #[serde(rename = "run.cancel_requested")]
    CancellationRequested,
    #[serde(rename = "run.safe_stop_finished")]
    SafeStopFinished,
    #[serde(rename = "run_plan.resolved")]
    RunPlanResolved,
    #[serde(rename = "run_plan.saved")]
    RunPlanSaved,
    #[serde(rename = "run_plan.save_failed")]
    RunPlanSaveFailed,
    #[serde(rename = "run_plan.save_outcome_unknown")]
    RunPlanSaveOutcomeUnknown,
    #[serde(rename = "run_plan.saved_finalization_failed")]
    RunPlanSavedFinalizationFailed,
    #[serde(rename = "phase.started")]
    PhaseStarted,
    #[serde(rename = "phase.finished")]
    PhaseFinished,
    #[serde(rename = "retry.summary")]
    RetrySummary,
    #[serde(rename = "work.none")]
    NoWork,
    #[serde(rename = "result.partial")]
    PartialResult,
    #[serde(rename = "publication.started")]
    PublicationStarted,
    #[serde(rename = "publication.finished")]
    PublicationFinished,
    #[serde(rename = "task.started")]
    TaskStarted,
    #[serde(rename = "task.finished")]
    TaskFinished,
    #[serde(rename = "task.diagnostic")]
    TaskDiagnostic,
}

impl ProjectLogCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunFinished => "run.finished",
            Self::PerformanceCounters => "performance.counters",
            Self::FailureReported => "failure.reported",
            Self::CancellationRequested => "run.cancel_requested",
            Self::SafeStopFinished => "run.safe_stop_finished",
            Self::RunPlanResolved => "run_plan.resolved",
            Self::RunPlanSaved => "run_plan.saved",
            Self::RunPlanSaveFailed => "run_plan.save_failed",
            Self::RunPlanSaveOutcomeUnknown => "run_plan.save_outcome_unknown",
            Self::RunPlanSavedFinalizationFailed => "run_plan.saved_finalization_failed",
            Self::PhaseStarted => "phase.started",
            Self::PhaseFinished => "phase.finished",
            Self::RetrySummary => "retry.summary",
            Self::NoWork => "work.none",
            Self::PartialResult => "result.partial",
            Self::PublicationStarted => "publication.started",
            Self::PublicationFinished => "publication.finished",
            Self::TaskStarted => "task.started",
            Self::TaskFinished => "task.finished",
            Self::TaskDiagnostic => "task.diagnostic",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogValueSource {
    Explicit,
    ProjectState,
    ProductDefault,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogAmount {
    Indeterminate,
    Determinate { completed: u64, total: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogRunOutcome {
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogTaskOutcome {
    Complete,
    Partial,
    Unavailable,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogPublicationOutcome {
    Published,
    NotPublished,
    RecoveryRequired,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogPhase {
    CheckProject,
    ScanSource,
    PrepareCandidate,
    UpdateDatabase,
    Publish,
    Builtin,
    BuiltinDocuments,
    BuiltinWorkUnits,
    BuiltinCommit,
    Rules,
    RulesDocuments,
    RulesMatches,
    RulesCommit,
    Lua,
    LuaExecution,
    LuaCommit,
    Planning,
    ConfirmedTasks,
    NoWork,
    ReadAssets,
    PlanStandard,
    RewriteDocuments,
    ValidateCandidate,
}

impl ProjectLogPhase {
    const fn label(self) -> UiMessage<'static> {
        match self {
            Self::CheckProject => UiMessage::LogLabelPhaseCheckProject,
            Self::ScanSource => UiMessage::LogLabelPhaseScanSource,
            Self::PrepareCandidate => UiMessage::LogLabelPhasePrepareCandidate,
            Self::UpdateDatabase => UiMessage::LogLabelPhaseUpdateDatabase,
            Self::Publish => UiMessage::LogLabelPhasePublish,
            Self::Builtin => UiMessage::LogLabelPhaseBuiltin,
            Self::BuiltinDocuments | Self::RulesDocuments => UiMessage::ProgressExtractDocuments,
            Self::BuiltinWorkUnits => UiMessage::ProgressExtractBuiltin,
            Self::BuiltinCommit | Self::RulesCommit | Self::LuaCommit => {
                UiMessage::ProgressExtractCommit
            }
            Self::Rules => UiMessage::LogLabelPhaseRules,
            Self::RulesMatches => UiMessage::ProgressExtractRules,
            Self::Lua => UiMessage::LogLabelPhaseLua,
            Self::LuaExecution => UiMessage::ProgressExtractLua,
            Self::Planning => UiMessage::LogLabelPhasePlanning,
            Self::ConfirmedTasks => UiMessage::LogLabelPhaseConfirmedTasks,
            Self::NoWork => UiMessage::LogLabelPhaseNoWork,
            Self::ReadAssets => UiMessage::LogLabelPhaseReadAssets,
            Self::PlanStandard => UiMessage::LogLabelPhasePlanStandard,
            Self::RewriteDocuments => UiMessage::LogLabelPhaseRewriteDocuments,
            Self::ValidateCandidate => UiMessage::LogLabelPhaseValidateCandidate,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogNoWorkReason {
    TranslationUpToDate,
}

impl ProjectLogNoWorkReason {
    const fn label(self) -> UiMessage<'static> {
        match self {
            Self::TranslationUpToDate => UiMessage::LogNoWorkTranslationUpToDate,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureRelation {
    Primary,
    Related,
}

/// 项目日志允许持久化的结构化业务事实。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogPayload {
    None,
    Run {
        outcome: Option<ProjectLogRunOutcome>,
    },
    Performance {
        snapshot: RunPerformanceSnapshot,
    },
    Failure {
        relation: FailureRelation,
        diagnostic: SafeDiagnostic,
    },
    RunPlan {
        source: ProjectLogValueSource,
        lua_source: Option<ProjectLogValueSource>,
        selections: Vec<String>,
        lua_enabled: Option<bool>,
    },
    Phase {
        phase: ProjectLogPhase,
        amount: ProjectLogAmount,
    },
    RetrySummary {
        attempted: u64,
        recovered: u64,
        exhausted: u64,
    },
    NoWork {
        reason: ProjectLogNoWorkReason,
    },
    ResultSummary {
        complete: u64,
        partial: u64,
        unavailable: u64,
        manual_review: u64,
    },
    Publication {
        outcome: ProjectLogPublicationOutcome,
        published_items: Option<u64>,
    },
    Task {
        ordinal: u64,
        total: u64,
        outcome: Option<ProjectLogTaskOutcome>,
        attempts: Option<u64>,
    },
    TaskDiagnostic {
        ordinal: u64,
        total: u64,
        attempts: u64,
        diagnostic: SafeDiagnostic,
    },
    Cancellation {
        confirmed: u64,
        total: Option<u64>,
    },
}

impl ProjectLogPayload {
    const fn kind_code(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Run { .. } => "run",
            Self::Performance { .. } => "performance",
            Self::Failure { .. } => "failure",
            Self::RunPlan { .. } => "run_plan",
            Self::Phase { .. } => "phase",
            Self::RetrySummary { .. } => "retry_summary",
            Self::NoWork { .. } => "no_work",
            Self::ResultSummary { .. } => "result_summary",
            Self::Publication { .. } => "publication",
            Self::Task { .. } => "task",
            Self::TaskDiagnostic { .. } => "task_diagnostic",
            Self::Cancellation { .. } => "cancellation",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectLogContext {
    engine: Option<String>,
    project: Option<String>,
    command: Option<String>,
    profile: Option<String>,
    locale: String,
}

impl ProjectLogContext {
    pub(crate) fn new(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            ..Self::default()
        }
    }

    pub(crate) fn with_engine(mut self, value: impl Into<String>) -> Self {
        self.engine = Some(value.into());
        self
    }

    pub(crate) fn with_project(mut self, value: impl Into<String>) -> Self {
        self.project = Some(value.into());
        self
    }

    pub(crate) fn with_command(mut self, value: impl Into<String>) -> Self {
        self.command = Some(value.into());
        self
    }

    pub(crate) fn with_profile(mut self, value: impl Into<String>) -> Self {
        self.profile = Some(value.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogEvent {
    level: ProjectLogLevel,
    code: ProjectLogCode,
    context: ProjectLogContext,
    payload: ProjectLogPayload,
}

impl ProjectLogEvent {
    pub(crate) fn new(
        level: ProjectLogLevel,
        code: ProjectLogCode,
        context: ProjectLogContext,
        payload: ProjectLogPayload,
    ) -> Self {
        Self {
            level,
            code,
            context,
            payload,
        }
    }
}

pub(crate) trait ProjectLog: Send + Sync {
    fn emit(&self, event: ProjectLogEvent);
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectLogRecord {
    time: String,
    level: ProjectLogLevel,
    code: ProjectLogCode,
    pid: u32,
    run_id: String,
    sequence: u64,
    engine: Option<String>,
    project: Option<String>,
    command: Option<String>,
    profile: Option<String>,
    locale: String,
    message: String,
    payload: ProjectLogPayload,
}

struct QueuedProjectLogEvent {
    emitted_at: OffsetDateTime,
    event: ProjectLogEvent,
}

#[derive(Clone)]
pub(crate) struct ProjectLogger {
    inner: Arc<LoggerInner>,
}

struct LoggerInner {
    sender: Option<Sender<QueuedProjectLogEvent>>,
    health: Arc<ProjectLogHealth>,
}

impl ProjectLogger {
    fn no_op(health: Arc<ProjectLogHealth>) -> Self {
        Self {
            inner: Arc::new(LoggerInner {
                sender: None,
                health,
            }),
        }
    }

    pub(crate) fn health(&self) -> ProjectLogHealthSnapshot {
        self.inner.health.snapshot()
    }

    pub(crate) fn take_warning(&self) -> Option<ProjectLogWarning> {
        self.inner.health.take_warning()
    }

    /// 将同一运行中的其他可观测性产物故障并入既有非致命日志降级提示。
    pub(crate) fn record_observability_failure(&self, diagnostic: SafeDiagnostic) {
        self.inner.health.record_observability_failure(diagnostic);
    }

    /// 同一可观测性操作的主错误与相关清理错误必须全部保留，不能互相覆盖。
    pub(crate) fn record_observability_failures(
        &self,
        diagnostics: impl IntoIterator<Item = SafeDiagnostic>,
    ) {
        for diagnostic in diagnostics {
            self.record_observability_failure(diagnostic);
        }
    }
}

impl ProjectLog for ProjectLogger {
    fn emit(&self, event: ProjectLogEvent) {
        let Some(sender) = &self.inner.sender else {
            self.inner.health.record_queue_closed();
            return;
        };
        let queued = QueuedProjectLogEvent {
            emitted_at: OffsetDateTime::now_utc(),
            event,
        };
        match sender.send_blocking(queued) {
            Ok(()) => self.inner.health.add_accepted(1),
            Err(_) => {
                self.inner.health.record_queue_closed();
                self.inner.health.add_dropped(1);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLogHealthSnapshot {
    pub(crate) accepted_records: u64,
    pub(crate) persisted_records: u64,
    pub(crate) dropped_records: u64,
    pub(crate) startup_failures: u64,
    pub(crate) queue_closed: u64,
    pub(crate) serialization_failures: u64,
    pub(crate) write_failures: u64,
    pub(crate) flush_failures: u64,
    pub(crate) sync_failures: u64,
    pub(crate) worker_panics: u64,
}

impl ProjectLogHealthSnapshot {
    pub(crate) const fn is_degraded(self) -> bool {
        self.startup_failures > 0
            || self.queue_closed > 0
            || self.serialization_failures > 0
            || self.write_failures > 0
            || self.flush_failures > 0
            || self.sync_failures > 0
            || self.worker_panics > 0
            || self.dropped_records > 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogWarning {
    pub(crate) diagnostic: Option<SafeDiagnostic>,
    pub(crate) related_diagnostics: Vec<SafeDiagnostic>,
}

#[derive(Default)]
struct ProjectLogHealth {
    accepted_records: AtomicU64,
    persisted_records: AtomicU64,
    dropped_records: AtomicU64,
    startup_failures: AtomicU64,
    queue_closed: AtomicU64,
    serialization_failures: AtomicU64,
    write_failures: AtomicU64,
    flush_failures: AtomicU64,
    sync_failures: AtomicU64,
    worker_panics: AtomicU64,
    warning_claimed: AtomicBool,
    first_failure: Mutex<Option<SafeDiagnostic>>,
    observation_failures: Mutex<Vec<SafeDiagnostic>>,
}

impl ProjectLogHealth {
    fn increment(counter: &AtomicU64, amount: u64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }

    fn add_accepted(&self, amount: u64) {
        Self::increment(&self.accepted_records, amount);
    }

    fn add_persisted(&self, amount: u64) {
        Self::increment(&self.persisted_records, amount);
    }

    fn add_dropped(&self, amount: u64) {
        Self::increment(&self.dropped_records, amount);
    }

    fn record_queue_closed(&self) {
        Self::increment(&self.queue_closed, 1);
    }

    fn record_failure(&self, counter: &AtomicU64, diagnostic: SafeDiagnostic) {
        Self::increment(counter, 1);
        let mut first = self
            .first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if first.is_none() {
            *first = Some(diagnostic);
        }
    }

    fn record_observability_failure(&self, diagnostic: SafeDiagnostic) {
        self.record_failure(&self.write_failures, diagnostic.clone());
        self.observation_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(diagnostic);
    }

    fn snapshot(&self) -> ProjectLogHealthSnapshot {
        ProjectLogHealthSnapshot {
            accepted_records: self.accepted_records.load(Ordering::Relaxed),
            persisted_records: self.persisted_records.load(Ordering::Relaxed),
            dropped_records: self.dropped_records.load(Ordering::Relaxed),
            startup_failures: self.startup_failures.load(Ordering::Relaxed),
            queue_closed: self.queue_closed.load(Ordering::Relaxed),
            serialization_failures: self.serialization_failures.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
            flush_failures: self.flush_failures.load(Ordering::Relaxed),
            sync_failures: self.sync_failures.load(Ordering::Relaxed),
            worker_panics: self.worker_panics.load(Ordering::Relaxed),
        }
    }

    fn take_warning(&self) -> Option<ProjectLogWarning> {
        let health = self.snapshot();
        if !health.is_degraded()
            || self
                .warning_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        let diagnostic = self
            .first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut related_diagnostics = self
            .observation_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(primary) = &diagnostic
            && let Some(position) = related_diagnostics
                .iter()
                .position(|candidate| candidate == primary)
        {
            related_diagnostics.remove(position);
        }
        Some(ProjectLogWarning {
            diagnostic,
            related_diagnostics,
        })
    }
}

#[derive(Clone)]
struct TerminalRecords {
    outcome: ProjectLogRunOutcome,
    context: ProjectLogContext,
    failures: Vec<SafeDiagnostic>,
    performance: RunPerformanceSnapshot,
}

#[derive(Clone)]
struct UnfinishedTerminalRecords {
    context: ProjectLogContext,
    failures: Vec<SafeDiagnostic>,
    performance: Arc<RunPerformanceCounters>,
}

pub(crate) struct ProjectLogRuntime {
    logger: ProjectLogger,
    terminal: Arc<Mutex<Option<TerminalRecords>>>,
    unfinished_terminal: Option<UnfinishedTerminalRecords>,
    worker: Option<JoinHandle<()>>,
    path: Option<PathBuf>,
}

impl ProjectLogRuntime {
    pub(crate) fn logger(&self) -> ProjectLogger {
        self.logger.clone()
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn unfinished_failures(&self) -> Option<Vec<SafeDiagnostic>> {
        self.unfinished_terminal
            .as_ref()
            .map(|terminal| terminal.failures.clone())
    }

    /// 为尚未正常收尾的命令登记确定性的失败终态。
    ///
    /// 正常 `finish_with_performance` 会覆盖该兜底；若上层因 panic 或实现缺陷直接丢弃
    /// runtime，`Drop` 使用这里保存的安全投影写出失败和 `run.finished`。panic payload
    /// 不进入此接口，也不会被日志运行时读取。
    pub(crate) fn arm_unfinished_terminal(
        &mut self,
        context: ProjectLogContext,
        mut failures: Vec<SafeDiagnostic>,
        performance: Arc<RunPerformanceCounters>,
    ) {
        if failures.is_empty() {
            failures.push(unfinished_log_diagnostic(self.path.as_deref()));
        }
        self.unfinished_terminal = Some(UnfinishedTerminalRecords {
            context,
            failures,
            performance,
        });
    }

    /// 使用空性能快照的测试便利入口。
    #[cfg(test)]
    pub(crate) fn finish(
        self,
        outcome: ProjectLogRunOutcome,
        context: ProjectLogContext,
        failures: Vec<SafeDiagnostic>,
    ) -> ProjectLogHealthSnapshot {
        self.finish_with_performance(
            outcome,
            context,
            failures,
            RunPerformanceSnapshot::default(),
        )
    }

    /// 原子设置性能计数、最终诊断与运行终态，然后排空普通事件并完成 flush/sync。
    pub(crate) fn finish_with_performance(
        mut self,
        outcome: ProjectLogRunOutcome,
        context: ProjectLogContext,
        failures: Vec<SafeDiagnostic>,
        performance: RunPerformanceSnapshot,
    ) -> ProjectLogHealthSnapshot {
        self.unfinished_terminal = None;
        *self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(TerminalRecords {
            outcome,
            context,
            failures,
            performance,
        });
        self.shutdown_inner();
        self.logger.health()
    }

    fn shutdown_inner(&mut self) {
        if let Some(sender) = &self.logger.inner.sender {
            sender.close();
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            let diagnostic = internal_log_diagnostic(
                DiagnosticCode::LogWorker,
                self.path.as_deref(),
                "join_writer",
            );
            self.logger
                .inner
                .health
                .record_failure(&self.logger.inner.health.worker_panics, diagnostic);
        }
    }
}

impl Drop for ProjectLogRuntime {
    fn drop(&mut self) {
        self.ensure_terminal_on_drop();
        self.shutdown_inner();
    }
}

impl ProjectLogRuntime {
    fn ensure_terminal_on_drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let mut terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if terminal.is_some() {
            return;
        }
        let fallback = self.unfinished_terminal.take().unwrap_or_else(|| {
            let diagnostic = unfinished_log_diagnostic(self.path.as_deref());
            UnfinishedTerminalRecords {
                context: ProjectLogContext::new("en"),
                failures: vec![diagnostic],
                performance: Arc::new(RunPerformanceCounters::default()),
            }
        });
        *terminal = Some(TerminalRecords {
            outcome: ProjectLogRunOutcome::OutcomeUnknown,
            context: fallback.context,
            failures: fallback.failures,
            performance: fallback.performance.snapshot(),
        });
    }
}

/// 启动当前 RunId 的独占日志文件；失败时返回带精确安全诊断的 no-op runtime。
pub(crate) fn start_project_log(logs_root: PathBuf, run_id: String) -> ProjectLogRuntime {
    let health = Arc::new(ProjectLogHealth::default());
    let terminal = Arc::new(Mutex::new(None));
    let pinned_root = match create_directories_without_reparse(&logs_root) {
        Ok(root) => root,
        Err(error) => {
            let diagnostic = windows_log_diagnostic(DiagnosticCode::LogStart, &error);
            health.record_failure(&health.startup_failures, diagnostic);
            return no_op_runtime(health, terminal);
        }
    };
    let resolved_root = pinned_root.resolved_path().to_path_buf();
    let path = resolved_root.join(format!("{run_id}.jsonl"));
    let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(source) => {
            let diagnostic = SafeDiagnostic::io(
                DiagnosticCode::LogStart,
                DiagnosticStage::Logging,
                DiagnosticSubject::path(&path),
                "create_new",
                &source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            );
            health.record_failure(&health.startup_failures, diagnostic);
            return no_op_runtime(health, terminal);
        }
    };
    let (sender, receiver) = async_channel::bounded(QUEUE_CAPACITY);
    let logger = ProjectLogger {
        inner: Arc::new(LoggerInner {
            sender: Some(sender),
            health: Arc::clone(&health),
        }),
    };
    let worker_health = Arc::clone(&health);
    let worker_terminal = Arc::clone(&terminal);
    let worker_run_id = run_id.clone();
    let worker_path = path.clone();
    let panic_path = path.clone();
    let worker = match thread::Builder::new()
        .name(format!("att-project-log-{run_id}"))
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_worker(
                    receiver,
                    file,
                    worker_path,
                    worker_run_id,
                    worker_terminal,
                    &worker_health,
                );
                drop(pinned_root);
            }));
            if result.is_err() {
                let diagnostic = internal_log_diagnostic(
                    DiagnosticCode::LogWorker,
                    Some(&panic_path),
                    "run_writer",
                );
                worker_health.record_failure(&worker_health.worker_panics, diagnostic);
            }
        }) {
        Ok(worker) => worker,
        Err(source) => {
            let diagnostic = SafeDiagnostic::io(
                DiagnosticCode::LogStart,
                DiagnosticStage::Logging,
                DiagnosticSubject::path(&path),
                "spawn_writer",
                &source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            );
            health.record_failure(&health.startup_failures, diagnostic);
            return no_op_runtime(health, terminal);
        }
    };
    ProjectLogRuntime {
        logger,
        terminal,
        unfinished_terminal: None,
        worker: Some(worker),
        path: Some(path),
    }
}

fn no_op_runtime(
    health: Arc<ProjectLogHealth>,
    terminal: Arc<Mutex<Option<TerminalRecords>>>,
) -> ProjectLogRuntime {
    ProjectLogRuntime {
        logger: ProjectLogger::no_op(health),
        terminal,
        unfinished_terminal: None,
        worker: None,
        path: None,
    }
}

fn run_worker(
    receiver: Receiver<QueuedProjectLogEvent>,
    file: File,
    path: PathBuf,
    run_id: String,
    terminal: Arc<Mutex<Option<TerminalRecords>>>,
    health: &ProjectLogHealth,
) {
    let mut writer = BufWriter::with_capacity(WRITER_BUFFER_BYTES, file);
    let mut sequence = 0_u64;
    while let Ok(queued) = receiver.recv_blocking() {
        write_event(
            &mut writer,
            &path,
            &run_id,
            queued.emitted_at,
            queued.event,
            &mut sequence,
            health,
        );
    }

    if let Some(terminal) = terminal
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        write_event(
            &mut writer,
            &path,
            &run_id,
            OffsetDateTime::now_utc(),
            ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::PerformanceCounters,
                terminal.context.clone(),
                ProjectLogPayload::Performance {
                    snapshot: terminal.performance,
                },
            ),
            &mut sequence,
            health,
        );
        for (index, diagnostic) in terminal.failures.into_iter().enumerate() {
            let event = ProjectLogEvent::new(
                ProjectLogLevel::Error,
                ProjectLogCode::FailureReported,
                terminal.context.clone(),
                ProjectLogPayload::Failure {
                    relation: if index == 0 {
                        FailureRelation::Primary
                    } else {
                        FailureRelation::Related
                    },
                    diagnostic,
                },
            );
            write_event(
                &mut writer,
                &path,
                &run_id,
                OffsetDateTime::now_utc(),
                event,
                &mut sequence,
                health,
            );
        }
        let level = match terminal.outcome {
            ProjectLogRunOutcome::Succeeded | ProjectLogRunOutcome::Cancelled => {
                ProjectLogLevel::Info
            }
            ProjectLogRunOutcome::Failed | ProjectLogRunOutcome::OutcomeUnknown => {
                ProjectLogLevel::Error
            }
        };
        write_event(
            &mut writer,
            &path,
            &run_id,
            OffsetDateTime::now_utc(),
            ProjectLogEvent::new(
                level,
                ProjectLogCode::RunFinished,
                terminal.context,
                ProjectLogPayload::Run {
                    outcome: Some(terminal.outcome),
                },
            ),
            &mut sequence,
            health,
        );
    }

    if let Err(source) = writer.flush() {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::LogFlush,
            DiagnosticStage::Logging,
            DiagnosticSubject::path(&path),
            "flush",
            &source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        );
        health.record_failure(&health.flush_failures, diagnostic);
        return;
    }
    if let Err(source) = writer.get_ref().sync_all() {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::LogSync,
            DiagnosticStage::Logging,
            DiagnosticSubject::path(&path),
            "sync_all",
            &source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        );
        health.record_failure(&health.sync_failures, diagnostic);
    }
}

fn write_event(
    writer: &mut BufWriter<File>,
    path: &Path,
    run_id: &str,
    emitted_at: OffsetDateTime,
    event: ProjectLogEvent,
    sequence: &mut u64,
    health: &ProjectLogHealth,
) {
    let Some(record_sequence) = sequence.checked_add(1) else {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::LogSerialize,
            DiagnosticStage::Logging,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        );
        health.record_failure(&health.serialization_failures, diagnostic);
        health.add_dropped(1);
        return;
    };
    let ProjectLogEvent {
        level,
        code,
        context,
        payload,
    } = event;
    let payload = sanitize_payload(payload);
    let message = render_message(code, &payload, &context);
    let record = ProjectLogRecord {
        time: recorded_at_utc(emitted_at),
        level,
        code,
        pid: std::process::id(),
        run_id: run_id.to_owned(),
        sequence: record_sequence,
        engine: sanitize_optional(context.engine),
        project: sanitize_optional(context.project),
        command: sanitize_optional(context.command),
        profile: sanitize_optional(context.profile),
        locale: sanitize_user_text(&context.locale),
        message,
        payload,
    };
    let mut bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(_) => {
            let diagnostic = SafeDiagnostic::new(
                DiagnosticCode::LogSerialize,
                DiagnosticStage::Logging,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            );
            health.record_failure(&health.serialization_failures, diagnostic);
            health.add_dropped(1);
            return;
        }
    };
    bytes.push(b'\n');
    if let Err(source) = writer.write_all(&bytes) {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::LogWrite,
            DiagnosticStage::Logging,
            DiagnosticSubject::path(path),
            "write_all",
            &source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        );
        health.record_failure(&health.write_failures, diagnostic);
        health.add_dropped(1);
        return;
    }
    *sequence = record_sequence;
    health.add_persisted(1);
}

fn render_message(
    code: ProjectLogCode,
    payload: &ProjectLogPayload,
    context: &ProjectLogContext,
) -> String {
    let locale = UiLocale::match_automatic(&context.locale).unwrap_or(UiLocale::English);
    let localizer = UiLocalizer::new(locale);
    match (code, payload) {
        (ProjectLogCode::RunStarted, ProjectLogPayload::Run { .. }) => {
            localizer.format(UiMessage::LogRunStarted {
                command: context.command.as_deref().unwrap_or("unknown"),
            })
        }
        (ProjectLogCode::RunFinished, ProjectLogPayload::Run { outcome }) => match outcome {
            Some(ProjectLogRunOutcome::Succeeded) => localizer.format(UiMessage::LogRunSucceeded {
                command: context.command.as_deref().unwrap_or("unknown"),
            }),
            Some(ProjectLogRunOutcome::Cancelled) => localizer.format(UiMessage::LogRunCancelled {
                command: context.command.as_deref().unwrap_or("unknown"),
            }),
            Some(ProjectLogRunOutcome::OutcomeUnknown) => {
                localizer.format(UiMessage::LogRunOutcomeUnknown {
                    command: context.command.as_deref().unwrap_or("unknown"),
                })
            }
            _ => localizer.format(UiMessage::LogRunFailed {
                command: context.command.as_deref().unwrap_or("unknown"),
            }),
        },
        (ProjectLogCode::PerformanceCounters, ProjectLogPayload::Performance { snapshot }) => {
            localizer.format(UiMessage::LogPerformanceCounters {
                sqlite_control_attempted_total: snapshot.sqlite_transactions.attempted_total(),
                candidate_validation_started: snapshot.candidate_validations.started,
                candidate_validation_completed: snapshot.candidate_validations.completed,
            })
        }
        (ProjectLogCode::FailureReported, ProjectLogPayload::Failure { diagnostic, .. }) => {
            render_failure_message(diagnostic, &localizer)
        }
        (
            ProjectLogCode::PhaseStarted | ProjectLogCode::PhaseFinished,
            ProjectLogPayload::Phase { phase, .. },
        ) => {
            let label = localizer.format(phase.label());
            localizer.format(if code == ProjectLogCode::PhaseStarted {
                UiMessage::LogPhaseStarted { phase: &label }
            } else {
                UiMessage::LogPhaseFinished { phase: &label }
            })
        }
        (ProjectLogCode::RunPlanResolved, ProjectLogPayload::RunPlan { source, .. }) => {
            let source = value_source_message(*source, &localizer);
            localizer.format(UiMessage::LogPlanResolved {
                command: context.command.as_deref().unwrap_or("unknown"),
                source: &source,
            })
        }
        (ProjectLogCode::TaskStarted, ProjectLogPayload::Task { ordinal, total, .. }) => localizer
            .format(UiMessage::LogTranslationTaskStarted {
                index: *ordinal,
                total: *total,
            }),
        (
            ProjectLogCode::TaskFinished,
            ProjectLogPayload::Task {
                ordinal, outcome, ..
            },
        ) => {
            let outcome = outcome
                .and_then(|outcome| project_log_task_outcome_label(task_outcome_code(outcome)))
                .map(|message| localizer.format(message))
                .unwrap_or_else(|| "unknown".to_owned());
            localizer.format(UiMessage::LogTranslationTaskFinished {
                index: *ordinal,
                outcome: &outcome,
            })
        }
        (
            ProjectLogCode::TaskDiagnostic,
            ProjectLogPayload::TaskDiagnostic {
                ordinal,
                attempts,
                diagnostic,
                ..
            },
        ) => {
            let diagnostic = render_failure_message(diagnostic, &localizer);
            localizer.format(UiMessage::LogTranslationTaskDiagnostic {
                index: *ordinal,
                attempts: *attempts,
                diagnostic: &diagnostic,
            })
        }
        (ProjectLogCode::RetrySummary, ProjectLogPayload::RetrySummary { attempted, .. }) => {
            localizer.format(UiMessage::LogRetrySummary { count: *attempted })
        }
        (ProjectLogCode::NoWork, ProjectLogPayload::NoWork { reason }) => {
            let reason = localizer.format(reason.label());
            localizer.format(UiMessage::LogNoWork { reason: &reason })
        }
        (ProjectLogCode::PartialResult, ProjectLogPayload::ResultSummary { partial, .. }) => {
            localizer.format(UiMessage::LogPartialResult { count: *partial })
        }
        (ProjectLogCode::RunPlanSaved, ProjectLogPayload::None) => {
            localizer.format(UiMessage::ResultPlanSaved)
        }
        (ProjectLogCode::RunPlanSaveFailed, ProjectLogPayload::None) => {
            localizer.format(UiMessage::ErrorPlanSaveFailedApplied)
        }
        (ProjectLogCode::RunPlanSaveOutcomeUnknown, ProjectLogPayload::None) => {
            localizer.format(UiMessage::ErrorPlanSaveOutcomeUnknown)
        }
        (ProjectLogCode::RunPlanSavedFinalizationFailed, ProjectLogPayload::None) => {
            localizer.format(UiMessage::ErrorStateAppliedFinalization)
        }
        (ProjectLogCode::CancellationRequested, ProjectLogPayload::Cancellation { .. }) => {
            localizer.format(UiMessage::ProgressSafeStopping)
        }
        (ProjectLogCode::SafeStopFinished, ProjectLogPayload::Cancellation { .. }) => {
            localizer.format(UiMessage::ResultCancelled)
        }
        (ProjectLogCode::PublicationStarted, ProjectLogPayload::Publication { .. }) => {
            let phase = localizer.format(UiMessage::LogLabelPhasePublish);
            localizer.format(UiMessage::LogPhaseStarted { phase: &phase })
        }
        (ProjectLogCode::PublicationFinished, ProjectLogPayload::Publication { outcome, .. }) => {
            let phase = localizer.format(UiMessage::LogLabelPhasePublish);
            let finished = localizer.format(UiMessage::LogPhaseFinished { phase: &phase });
            format!("{finished} outcome={}", publication_outcome_code(*outcome))
        }
        _ => format!(
            "log event {} cannot use payload {}; this is an ATT logging defect",
            code.as_str(),
            payload.kind_code()
        ),
    }
}

fn render_failure_message(diagnostic: &SafeDiagnostic, localizer: &UiLocalizer) -> String {
    let mut rendered = Vec::new();
    if render_safe_diagnostic(diagnostic, localizer, &mut rendered).is_err() {
        return format!("error [{}]", diagnostic.code);
    }
    String::from_utf8_lossy(&rendered)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

const fn publication_outcome_code(outcome: ProjectLogPublicationOutcome) -> &'static str {
    match outcome {
        ProjectLogPublicationOutcome::Published => "published",
        ProjectLogPublicationOutcome::NotPublished => "not_published",
        ProjectLogPublicationOutcome::RecoveryRequired => "recovery_required",
        ProjectLogPublicationOutcome::OutcomeUnknown => "outcome_unknown",
    }
}

fn value_source_message(source: ProjectLogValueSource, localizer: &UiLocalizer) -> String {
    let code = match source {
        ProjectLogValueSource::Explicit => "explicit",
        ProjectLogValueSource::ProjectState => "project_state",
        ProjectLogValueSource::ProductDefault => "product_default",
    };
    project_log_value_source_label(code)
        .map(|message| localizer.format(message))
        .unwrap_or_else(|| code.to_owned())
}

const fn task_outcome_code(outcome: ProjectLogTaskOutcome) -> &'static str {
    match outcome {
        ProjectLogTaskOutcome::Complete => "complete",
        ProjectLogTaskOutcome::Partial => "partial",
        ProjectLogTaskOutcome::Unavailable => "unavailable",
        ProjectLogTaskOutcome::Failed => "failed",
    }
}

fn sanitize_optional(value: Option<String>) -> Option<String> {
    value.map(|value| sanitize_user_text(&value))
}

fn sanitize_payload(payload: ProjectLogPayload) -> ProjectLogPayload {
    match payload {
        ProjectLogPayload::Failure {
            relation,
            diagnostic,
        } => ProjectLogPayload::Failure {
            relation,
            diagnostic: diagnostic.sanitized(),
        },
        ProjectLogPayload::TaskDiagnostic {
            ordinal,
            total,
            attempts,
            diagnostic,
        } => ProjectLogPayload::TaskDiagnostic {
            ordinal,
            total,
            attempts,
            diagnostic: diagnostic.sanitized(),
        },
        ProjectLogPayload::RunPlan {
            source,
            lua_source,
            selections,
            lua_enabled,
        } => ProjectLogPayload::RunPlan {
            source,
            lua_source,
            selections: selections
                .into_iter()
                .map(|selection| sanitize_user_text(&selection))
                .collect(),
            lua_enabled,
        },
        payload => payload,
    }
}

fn windows_log_diagnostic(code: DiagnosticCode, error: &WindowsFsError) -> SafeDiagnostic {
    error.safe_diagnostic(
        code,
        DiagnosticStage::Logging,
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckPathAndPermissions,
    )
}

fn internal_log_diagnostic(
    code: DiagnosticCode,
    path: Option<&Path>,
    operation: &'static str,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        code,
        DiagnosticStage::Logging,
        path.map_or_else(
            || DiagnosticSubject::operation(operation),
            DiagnosticSubject::path,
        ),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::ReportBug,
    )
}

fn unfinished_log_diagnostic(path: Option<&Path>) -> SafeDiagnostic {
    let mut diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Logging,
        path.map_or_else(
            || DiagnosticSubject::operation("finish_project_log"),
            DiagnosticSubject::path,
        ),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::ReportBug,
    );
    if let Some(path) = path {
        diagnostic = diagnostic.with_recovery(RecoveryFact::path(path));
    }
    diagnostic
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn context() -> ProjectLogContext {
        ProjectLogContext::new("en")
            .with_engine("rpg_maker_mz")
            .with_project("project")
            .with_command("extract")
    }

    fn event(index: u64) -> ProjectLogEvent {
        ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::PhaseFinished,
            context(),
            ProjectLogPayload::Phase {
                phase: ProjectLogPhase::Builtin,
                amount: ProjectLogAmount::Determinate {
                    completed: index,
                    total: 32,
                },
            },
        )
    }

    fn records(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("日志应可读取")
            .lines()
            .map(|line| serde_json::from_str(line).expect("每行都应是 JSON"))
            .collect()
    }

    #[test]
    fn observability_failures_remain_visible_after_an_earlier_project_log_failure() {
        let health = Arc::new(ProjectLogHealth::default());
        let logger = ProjectLogger::no_op(Arc::clone(&health));
        let diagnostic = |path: &str| {
            SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::Logging,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
        };
        health.record_failure(
            &health.startup_failures,
            SafeDiagnostic::new(
                DiagnosticCode::LogStart,
                DiagnosticStage::Logging,
                DiagnosticSubject::path("C:/project/logs/run.jsonl"),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
        );
        logger.record_observability_failures([
            diagnostic("C:/project/task-records/run/task-000001.md"),
            diagnostic("C:/project/task-records/run/.task-000001.tmp"),
        ]);

        let warning = logger
            .take_warning()
            .expect("项目日志与任务记录故障必须形成非致命警告");
        assert_eq!(
            warning
                .diagnostic
                .as_ref()
                .map(|diagnostic| diagnostic.code),
            Some(DiagnosticCode::LogStart)
        );
        assert_eq!(
            warning
                .related_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.subject.clone())
                .collect::<Vec<_>>(),
            [
                DiagnosticSubject::path("C:/project/task-records/run/task-000001.md"),
                DiagnosticSubject::path("C:/project/task-records/run/.task-000001.tmp"),
            ],
            "较早的 JSONL 故障不得覆盖任务记录主错误或清理错误"
        );
    }

    #[test]
    fn dropping_an_unfinished_runtime_writes_an_outcome_unknown_terminal_record() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655449998";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        runtime.logger().emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunStarted,
            context(),
            ProjectLogPayload::Run { outcome: None },
        ));

        drop(runtime);

        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = records(&path);
        assert_eq!(
            records
                .iter()
                .map(|record| record["code"].as_str().expect("code 应为文本"))
                .collect::<Vec<_>>(),
            [
                "run.started",
                "performance.counters",
                "failure.reported",
                "run.finished",
            ]
        );
        assert_eq!(
            records[2]["payload"]["diagnostic"]["code"],
            "internal.operation"
        );
        assert_eq!(
            records[2]["payload"]["diagnostic"]["impact"],
            "outcome_unknown"
        );
        assert_eq!(records[3]["payload"]["outcome"], "outcome_unknown");
    }

    #[test]
    fn armed_unfinished_terminal_uses_only_the_registered_safe_projection() {
        const PANIC_BODY: &str = "PANIC_BODY_SENTINEL";
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655449999";
        let mut runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let path = runtime.path().expect("真实日志应有路径").to_path_buf();
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Extract,
            DiagnosticSubject::path("C:\\game\\project"),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::ReportBug,
        )
        .with_recovery(RecoveryFact::path(&path));
        runtime.arm_unfinished_terminal(
            context(),
            vec![diagnostic.clone()],
            Arc::new(RunPerformanceCounters::default()),
        );

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _runtime = runtime;
            std::panic::panic_any(Box::new(PANIC_BODY));
        }));
        assert!(caught.is_err());
        drop(caught);

        let raw = std::fs::read_to_string(&path).expect("日志应可读取");
        assert!(!raw.contains(PANIC_BODY));
        let records = records(&path);
        assert_eq!(records[0]["code"], "performance.counters");
        assert_eq!(records[1]["code"], "failure.reported");
        assert_eq!(
            records[1]["payload"]["diagnostic"],
            serde_json::to_value(diagnostic).expect("诊断应可序列化")
        );
        assert_eq!(records[2]["code"], "run.finished");
        assert_eq!(records[2]["payload"]["outcome"], "outcome_unknown");
    }

    #[test]
    fn each_run_owns_one_file_and_terminal_records_are_last() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440000";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunStarted,
            context(),
            ProjectLogPayload::Run { outcome: None },
        ));
        for index in 1..=32 {
            logger.emit(event(index));
        }
        let failure = SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Extract,
            DiagnosticSubject::Project {
                name: "project".to_owned(),
            },
            DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );
        let health = runtime.finish(ProjectLogRunOutcome::Failed, context(), vec![failure]);
        assert!(!health.is_degraded());

        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = records(&path);
        assert_eq!(records[records.len() - 3]["code"], "performance.counters");
        assert_eq!(records[records.len() - 2]["code"], "failure.reported");
        assert_eq!(records.last().expect("应有终态")["code"], "run.finished");
        assert_eq!(records.last().expect("应有终态")["sequence"], 36);
    }

    #[test]
    fn debug_event_is_persisted_and_event_has_no_free_message() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440001";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        runtime.logger().emit(ProjectLogEvent::new(
            ProjectLogLevel::Debug,
            ProjectLogCode::TaskStarted,
            context(),
            ProjectLogPayload::Task {
                ordinal: 1,
                total: 1,
                outcome: None,
                attempts: None,
            },
        ));
        let health = runtime.finish(ProjectLogRunOutcome::Succeeded, context(), Vec::new());
        assert_eq!(health.accepted_records, 1);
        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = records(&path);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["code"], "task.started");
        assert_eq!(records[0]["level"], "debug");
        assert_eq!(records[1]["code"], "performance.counters");
        assert_eq!(records[2]["code"], "run.finished");
    }

    #[test]
    fn failure_payload_uses_only_the_stable_source_projection() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440002";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let wrapped = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "UNTYPED_SOURCE_SENTINEL",
        );
        let failure = SafeDiagnostic::io(
            DiagnosticCode::ProjectUnavailable,
            DiagnosticStage::ProjectOpening,
            DiagnosticSubject::path("C:\\game\n\u{1b}[31m"),
            "open",
            &wrapped,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        )
        .with_recovery(crate::diagnostic::RecoveryFact::path("C:\\game\\recovery"));
        let _ = runtime.finish(ProjectLogRunOutcome::Failed, context(), vec![failure]);
        let path = directory.path().join(format!("{run_id}.jsonl"));
        let raw = std::fs::read_to_string(&path).expect("日志应可读取");
        assert!(!raw.contains("UNTYPED_SOURCE_SENTINEL"));
        assert!(!raw.contains('\u{1b}'));
        assert!(raw.contains("project.unavailable"));
        assert!(raw.contains("permission_denied"));
        let records = records(&path);
        let message = records
            .iter()
            .find(|record| record["code"] == "failure.reported")
            .expect("应记录失败诊断")["message"]
            .as_str()
            .expect("消息应为文本")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(message.contains("Error [project.unavailable]"));
        assert!(message.contains("Stage: Project opening"));
        assert!(message.contains("Location: C:\\game [31m"));
        assert!(message.contains("Reason: Operation open: Permission denied"));
        assert!(message.contains("Impact: State was not changed"));
        assert!(message.contains("Action: Check the path, filesystem state, and permissions"));
        assert!(message.contains("Recovery: C:\\game\\recovery"));
    }

    #[test]
    fn primary_and_related_failures_preserve_the_exact_shared_projection() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440003";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let primary = SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            DiagnosticStage::CommandPreparation,
            DiagnosticSubject::field("PROFILE_ID"),
            DiagnosticReason::failure(DiagnosticFailureKind::MissingRequiredValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );
        let related = SafeDiagnostic::new(
            DiagnosticCode::ShutdownComponent,
            DiagnosticStage::Shutdown,
            DiagnosticSubject::component("SQLite"),
            DiagnosticReason::failure(DiagnosticFailureKind::FinalizationFailed),
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::Retry,
        );
        let _ = runtime.finish(
            ProjectLogRunOutcome::Failed,
            context(),
            vec![primary.clone(), related.clone()],
        );

        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = records(&path);
        assert_eq!(records.len(), 4);
        assert_eq!(records[0]["code"], "performance.counters");
        assert_eq!(records[1]["code"], "failure.reported");
        assert_eq!(records[1]["payload"]["relation"], "primary");
        assert_eq!(
            records[1]["payload"]["diagnostic"],
            serde_json::to_value(primary).expect("诊断应可序列化")
        );
        assert_eq!(records[2]["code"], "failure.reported");
        assert_eq!(records[2]["payload"]["relation"], "related");
        assert_eq!(
            records[2]["payload"]["diagnostic"],
            serde_json::to_value(related).expect("诊断应可序列化")
        );
        assert_eq!(records[3]["code"], "run.finished");
    }

    #[test]
    fn queue_pressure_backpressures_without_losing_events_or_terminal_records() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440004";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        let logger = runtime.logger();
        let event_count = u64::try_from(QUEUE_CAPACITY)
            .expect("队列容量应可转换")
            .saturating_add(257);
        for index in 1..=event_count {
            logger.emit(event(index));
        }
        let health = runtime.finish(ProjectLogRunOutcome::Succeeded, context(), Vec::new());
        assert_eq!(health.accepted_records, event_count);
        assert_eq!(health.dropped_records, 0);
        assert!(!health.is_degraded());

        let path = directory.path().join(format!("{run_id}.jsonl"));
        let raw = std::fs::read_to_string(path).expect("日志应可读取");
        assert_eq!(
            u64::try_from(raw.lines().count()).expect("日志条数应可转换"),
            event_count.saturating_add(2)
        );
        let terminal: serde_json::Value =
            serde_json::from_str(raw.lines().last().expect("应有终态")).expect("终态应为 JSON");
        assert_eq!(terminal["code"], "run.finished");
    }

    #[test]
    fn performance_snapshot_is_strictly_serialized_before_failures_and_run_finished() {
        let directory = tempdir().expect("临时目录应可建立");
        let run_id = "550e8400-e29b-41d4-a716-446655440005";
        let runtime = start_project_log(directory.path().to_path_buf(), run_id.to_owned());
        runtime.logger().emit(event(1));
        let mut snapshot = RunPerformanceSnapshot::default();
        snapshot.sqlite_transactions.write_plan.begin.attempted = 7;
        snapshot.sqlite_transactions.write_plan.commit.attempted = 5;
        snapshot.sqlite_transactions.write_plan.rollback.attempted = 1;
        snapshot.candidate_validations.started = 11;
        snapshot.candidate_validations.completed = 13;
        let failure = SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::operation("validate_candidate"),
            DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );

        let health = runtime.finish_with_performance(
            ProjectLogRunOutcome::Failed,
            context(),
            vec![failure],
            snapshot,
        );
        assert!(!health.is_degraded());

        let path = directory.path().join(format!("{run_id}.jsonl"));
        let records = records(&path);
        let codes = records
            .iter()
            .map(|record| record["code"].as_str().expect("code 应为文本"))
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "phase.finished",
                "performance.counters",
                "failure.reported",
                "run.finished",
            ]
        );
        let payload = records[1]["payload"].clone();
        assert_eq!(payload["kind"], "performance");
        assert_eq!(
            payload["snapshot"],
            serde_json::to_value(snapshot).expect("性能快照应可序列化")
        );
        assert_eq!(
            serde_json::from_value::<ProjectLogPayload>(payload.clone())
                .expect("闭集性能载荷应可反序列化"),
            ProjectLogPayload::Performance { snapshot }
        );

        let mut unknown_payload = payload.clone();
        unknown_payload
            .as_object_mut()
            .expect("性能载荷应为 object")
            .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<ProjectLogPayload>(unknown_payload).is_err());

        let mut unknown_snapshot = payload;
        unknown_snapshot["snapshot"]
            .as_object_mut()
            .expect("性能快照应为 object")
            .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<ProjectLogPayload>(unknown_snapshot).is_err());
    }

    #[test]
    fn performance_summary_uses_all_locales_and_preserves_each_counter() {
        let mut snapshot = RunPerformanceSnapshot::default();
        snapshot.sqlite_transactions.interactive.begin.attempted = 7;
        snapshot.candidate_validations.started = 11;
        snapshot.candidate_validations.completed = 13;
        let payload = ProjectLogPayload::Performance { snapshot };

        for locale in UiLocale::ALL {
            let localized_context = ProjectLogContext::new(locale.as_str());
            let rendered = render_message(
                ProjectLogCode::PerformanceCounters,
                &payload,
                &localized_context,
            )
            .replace(['\u{2068}', '\u{2069}'], "");
            for counter in ["7", "11", "13"] {
                assert!(
                    rendered.contains(counter),
                    "{locale} 性能摘要缺少计数 {counter}: {rendered}"
                );
            }
            assert!(!rendered.contains("cannot use payload"));
        }
    }
}
