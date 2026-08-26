//! 项目日志、任务记录和进程输出边界建立的封闭诊断问题。

use serde::{Deserialize, Serialize};

use super::DiagnosticStage;
use super::issue::IoFailure;
use super::model::DiagnosticResolution;
use super::safe_value::SafePath;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityComponent {
    ProjectLog,
    TaskRecord,
    Stdout,
    Stderr,
}

impl ObservabilityComponent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectLog => "project_log",
            Self::TaskRecord => "task_record",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityOperation {
    Create,
    Serialize,
    Write,
    Flush,
    Sync,
    Channel,
    Backpressure,
    Worker,
    Render,
    Cleanup,
    Contract,
}

impl ObservabilityOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Serialize => "serialize",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Sync => "sync",
            Self::Channel => "channel",
            Self::Backpressure => "backpressure",
            Self::Worker => "worker",
            Self::Render => "render",
            Self::Cleanup => "cleanup",
            Self::Contract => "contract",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ObservabilityEventCode {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.cancel_requested")]
    CancellationRequested,
    #[serde(rename = "phase.started")]
    PhaseStarted,
    #[serde(rename = "phase.completed")]
    PhaseCompleted,
    #[serde(rename = "phase.stopped")]
    PhaseStopped,
    #[serde(rename = "run_plan.resolved")]
    RunPlanResolved,
    #[serde(rename = "run_plan.finalized")]
    RunPlanFinalized,
    #[serde(rename = "task.started")]
    TaskStarted,
    #[serde(rename = "task.finished")]
    TaskFinished,
    #[serde(rename = "translation.finished")]
    TranslationFinished,
    #[serde(rename = "retry.summary")]
    RetrySummary,
    #[serde(rename = "publication.started")]
    PublicationStarted,
    #[serde(rename = "publication.finished")]
    PublicationFinished,
    #[serde(rename = "lua.print")]
    LuaPrint,
    #[serde(rename = "diagnostic.run")]
    RunDiagnostic,
    #[serde(rename = "diagnostic.run_plan")]
    RunPlanDiagnostic,
    #[serde(rename = "diagnostic.translation_task")]
    TranslationTaskDiagnostic,
    #[serde(rename = "diagnostic.extract")]
    ExtractDiagnostic,
    #[serde(rename = "diagnostic.write_back")]
    WriteBackDiagnostic,
    #[serde(rename = "diagnostic.publication")]
    PublicationDiagnostic,
    #[serde(rename = "diagnostic.task_record")]
    TaskRecordDiagnostic,
    #[serde(rename = "diagnostic.project_log")]
    ProjectLogDiagnostic,
    #[serde(rename = "performance.counters")]
    PerformanceCounters,
    #[serde(rename = "run.finished")]
    RunFinished,
}

/// 可观测性故障的发生次数。溢出后不把下界伪装成精确值。
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObservabilityFailureCount {
    Exact { count: u64 },
    AtLeast { minimum: u64 },
}

impl ObservabilityFailureCount {
    pub(crate) const fn exact(count: u64) -> Self {
        Self::Exact { count }
    }

    pub(crate) const fn minimum(self) -> u64 {
        match self {
            Self::Exact { count } => count,
            Self::AtLeast { minimum } => minimum,
        }
    }

    /// `AtLeast` 只会在精确计数溢出时产生；之后保留最后可表示的下界。
    pub(crate) const fn increment(self) -> Self {
        match self {
            Self::Exact { count } if count < u64::MAX => Self::Exact { count: count + 1 },
            Self::Exact { .. } => Self::AtLeast { minimum: u64::MAX },
            Self::AtLeast { minimum } => Self::AtLeast { minimum },
        }
    }

    /// 返回相对于前一快照可以安全说明的新增数量。已经溢出的下界无法再表示精确增量，
    /// 因此只在首次观察到时呈现其下界。
    pub(crate) const fn additional_since(self, previous: Option<Self>) -> Option<Self> {
        match (self, previous) {
            (Self::Exact { count }, None) if count > 0 => Some(Self::Exact { count }),
            (Self::Exact { count }, Some(Self::Exact { count: observed })) if count > observed => {
                Some(Self::Exact {
                    count: count - observed,
                })
            }
            (Self::AtLeast { minimum }, None) => Some(Self::AtLeast { minimum }),
            (Self::AtLeast { minimum }, Some(Self::Exact { count: observed })) => {
                // 从 Exact 进入 AtLeast 必然已经至少多发生了一次；当两个下界都等于
                // u64::MAX 时，减法本身无法表达这条事实，故保留 1 作为真实下界。
                let additional_minimum = minimum.saturating_sub(observed);
                Some(Self::AtLeast {
                    minimum: if additional_minimum == 0 {
                        1
                    } else {
                        additional_minimum
                    },
                })
            }
            (Self::Exact { .. }, None)
            | (Self::Exact { .. }, Some(Self::Exact { .. }))
            | (Self::Exact { .. }, Some(Self::AtLeast { .. }))
            | (Self::AtLeast { .. }, Some(Self::AtLeast { .. })) => None,
        }
    }
}

impl From<u64> for ObservabilityFailureCount {
    fn from(value: u64) -> Self {
        Self::exact(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityRenderTarget {
    EventMessage,
    Sequence,
    OccurrenceId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityProjectLogPhase {
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
    Planning,
    ConfirmedTasks,
    ReadAssets,
    PlanRpgMakerWriteBack,
    RewriteDocuments,
    ValidateCandidate,
}

impl ObservabilityProjectLogPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CheckProject => "check_project",
            Self::ScanSource => "scan_source",
            Self::PrepareCandidate => "prepare_candidate",
            Self::UpdateDatabase => "update_database",
            Self::Publish => "publish",
            Self::Builtin => "builtin",
            Self::BuiltinDocuments => "builtin_documents",
            Self::BuiltinWorkUnits => "builtin_work_units",
            Self::BuiltinCommit => "builtin_commit",
            Self::Rules => "rules",
            Self::RulesDocuments => "rules_documents",
            Self::RulesMatches => "rules_matches",
            Self::RulesCommit => "rules_commit",
            Self::Lua => "lua",
            Self::Planning => "planning",
            Self::ConfirmedTasks => "confirmed_tasks",
            Self::ReadAssets => "read_assets",
            Self::PlanRpgMakerWriteBack => "plan_rpg_maker_write_back",
            Self::RewriteDocuments => "rewrite_documents",
            Self::ValidateCandidate => "validate_candidate",
        }
    }
}

/// logger 生产者与运行时之间的封闭调用契约。每个变体只携带该失败边界已经确认的事实。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObservabilityContractViolation {
    InvalidContextIdentifier,
    ProducerClosed,
    RuntimeManagedEvent {
        event: ObservabilityEventCode,
    },
    UnknownDiagnostic {
        occurrence_id: u64,
    },
    InvalidPhaseTransition {
        phase: ObservabilityProjectLogPhase,
    },
    DuplicateRunPlan,
    InvalidRunPlanTransition,
    RunPlanCommandMismatch,
    InvalidRunPlanTransaction,
    DuplicateTask {
        ordinal: u64,
    },
    InvalidTaskTransition {
        ordinal: u64,
    },
    InvalidTaskAttempts {
        ordinal: u64,
        attempts: u64,
    },
    InconsistentTaskTotal,
    DuplicateTranslationFinished,
    DuplicatePublication,
    InvalidPublicationTransition,
    UnfinishedTasks,
    TaskSummaryMismatch,
    TaskCountDoesNotFit,
    EngineSummaryMismatch,
    InvalidRetrySummary {
        attempted: u64,
        recovered: u64,
        exhausted: u64,
    },
    InvalidCancellationCount {
        confirmed: u64,
        total: u64,
    },
    DiagnosticRequiresFailedTranslation,
    PlanningFailureStartedTasks,
    OccurrenceIdExhausted,
    SequenceExhausted,
    AlreadyFinished,
    ActivePhase,
    UnfinalizedRunPlan,
    MissingTranslationFinished,
    MissingPublicationFinished,
    InvalidTerminalDiagnostic {
        occurrence_id: u64,
    },
    ChannelClosed,
}

impl ObservabilityContractViolation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidContextIdentifier => "invalid_context_identifier",
            Self::ProducerClosed => "producer_closed",
            Self::RuntimeManagedEvent { .. } => "runtime_managed_event",
            Self::UnknownDiagnostic { .. } => "unknown_diagnostic",
            Self::InvalidPhaseTransition { .. } => "invalid_phase_transition",
            Self::DuplicateRunPlan => "duplicate_run_plan",
            Self::InvalidRunPlanTransition => "invalid_run_plan_transition",
            Self::RunPlanCommandMismatch => "run_plan_command_mismatch",
            Self::InvalidRunPlanTransaction => "invalid_run_plan_transaction",
            Self::DuplicateTask { .. } => "duplicate_task",
            Self::InvalidTaskTransition { .. } => "invalid_task_transition",
            Self::InvalidTaskAttempts { .. } => "invalid_task_attempts",
            Self::InconsistentTaskTotal => "inconsistent_task_total",
            Self::DuplicateTranslationFinished => "duplicate_translation_finished",
            Self::DuplicatePublication => "duplicate_publication",
            Self::InvalidPublicationTransition => "invalid_publication_transition",
            Self::UnfinishedTasks => "unfinished_tasks",
            Self::TaskSummaryMismatch => "task_summary_mismatch",
            Self::TaskCountDoesNotFit => "task_count_does_not_fit",
            Self::EngineSummaryMismatch => "engine_summary_mismatch",
            Self::InvalidRetrySummary { .. } => "invalid_retry_summary",
            Self::InvalidCancellationCount { .. } => "invalid_cancellation_count",
            Self::DiagnosticRequiresFailedTranslation => "diagnostic_requires_failed_translation",
            Self::PlanningFailureStartedTasks => "planning_failure_started_tasks",
            Self::OccurrenceIdExhausted => "occurrence_id_exhausted",
            Self::SequenceExhausted => "sequence_exhausted",
            Self::AlreadyFinished => "already_finished",
            Self::ActivePhase => "active_phase",
            Self::UnfinalizedRunPlan => "unfinalized_run_plan",
            Self::MissingTranslationFinished => "missing_translation_finished",
            Self::MissingPublicationFinished => "missing_publication_finished",
            Self::InvalidTerminalDiagnostic { .. } => "invalid_terminal_diagnostic",
            Self::ChannelClosed => "channel_closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityWorkerFailure {
    Start,
    Panicked,
    Cancelled,
}

impl ObservabilityWorkerFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Panicked => "panicked",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservabilityPathFailure {
    Invalid,
    ReparsePoint,
    NonLocalVolume,
    NonNtfsVolume,
    CaseSensitiveDirectory,
}

impl ObservabilityPathFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::ReparsePoint => "reparse_point",
            Self::NonLocalVolume => "non_local_volume",
            Self::NonNtfsVolume => "non_ntfs_volume",
            Self::CaseSensitiveDirectory => "case_sensitive_directory",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObservabilityWriteFailure {
    Io { failure: IoFailure },
    NotPersisted,
    TargetExists,
    Path { failure: ObservabilityPathFailure },
    IdentityChanged,
    WindowsStatus { status: i32 },
    ExecutorClosed,
    Cancelled,
    InvalidState,
    RecoveryRequired,
    OutcomeUnknown,
}

impl ObservabilityRenderTarget {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EventMessage => "event_message",
            Self::Sequence => "sequence",
            Self::OccurrenceId => "occurrence_id",
        }
    }
}

impl ObservabilityEventCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::CancellationRequested => "run.cancel_requested",
            Self::PhaseStarted => "phase.started",
            Self::PhaseCompleted => "phase.completed",
            Self::PhaseStopped => "phase.stopped",
            Self::RunPlanResolved => "run_plan.resolved",
            Self::RunPlanFinalized => "run_plan.finalized",
            Self::TaskStarted => "task.started",
            Self::TaskFinished => "task.finished",
            Self::TranslationFinished => "translation.finished",
            Self::RetrySummary => "retry.summary",
            Self::PublicationStarted => "publication.started",
            Self::PublicationFinished => "publication.finished",
            Self::LuaPrint => "lua.print",
            Self::RunDiagnostic => "diagnostic.run",
            Self::RunPlanDiagnostic => "diagnostic.run_plan",
            Self::TranslationTaskDiagnostic => "diagnostic.translation_task",
            Self::ExtractDiagnostic => "diagnostic.extract",
            Self::WriteBackDiagnostic => "diagnostic.write_back",
            Self::PublicationDiagnostic => "diagnostic.publication",
            Self::TaskRecordDiagnostic => "diagnostic.task_record",
            Self::ProjectLogDiagnostic => "diagnostic.project_log",
            Self::PerformanceCounters => "performance.counters",
            Self::RunFinished => "run.finished",
        }
    }
}

/// `operation` 是 serde 的标签，因此任意字符串和不适用于该操作的字段都不能进入模型。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub(crate) enum ObservabilityProblem {
    Create {
        path: SafePath,
        failure: IoFailure,
    },
    Serialize {
        path: Option<SafePath>,
        event: Option<ObservabilityEventCode>,
        count: ObservabilityFailureCount,
    },
    Write {
        path: Option<SafePath>,
        event: Option<ObservabilityEventCode>,
        count: ObservabilityFailureCount,
        failure: ObservabilityWriteFailure,
    },
    Flush {
        path: Option<SafePath>,
        failure: IoFailure,
    },
    Sync {
        path: Option<SafePath>,
        failure: IoFailure,
    },
    Channel {
        event: Option<ObservabilityEventCode>,
        count: ObservabilityFailureCount,
    },
    Backpressure {
        event: ObservabilityEventCode,
        count: ObservabilityFailureCount,
    },
    Worker {
        kind: ObservabilityWorkerFailure,
        count: ObservabilityFailureCount,
        failure: Option<IoFailure>,
    },
    Render {
        target: ObservabilityRenderTarget,
        event: Option<ObservabilityEventCode>,
        count: ObservabilityFailureCount,
    },
    Cleanup {
        path: SafePath,
        failure: ObservabilityWriteFailure,
    },
    Contract {
        violation: ObservabilityContractViolation,
    },
}

impl ObservabilityProblem {
    pub(crate) const fn operation(&self) -> ObservabilityOperation {
        match self {
            Self::Create { .. } => ObservabilityOperation::Create,
            Self::Serialize { .. } => ObservabilityOperation::Serialize,
            Self::Write { .. } => ObservabilityOperation::Write,
            Self::Flush { .. } => ObservabilityOperation::Flush,
            Self::Sync { .. } => ObservabilityOperation::Sync,
            Self::Channel { .. } => ObservabilityOperation::Channel,
            Self::Backpressure { .. } => ObservabilityOperation::Backpressure,
            Self::Worker { .. } => ObservabilityOperation::Worker,
            Self::Render { .. } => ObservabilityOperation::Render,
            Self::Cleanup { .. } => ObservabilityOperation::Cleanup,
            Self::Contract { .. } => ObservabilityOperation::Contract,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservabilityIssue {
    component: ObservabilityComponent,
    problem: ObservabilityProblem,
}

impl ObservabilityIssue {
    pub(crate) fn create(
        component: ObservabilityComponent,
        path: SafePath,
        source: &std::io::Error,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Create {
                path,
                failure: IoFailure::from_error(source),
            },
        }
    }

    pub(crate) const fn create_failure(
        component: ObservabilityComponent,
        path: SafePath,
        failure: IoFailure,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Create { path, failure },
        }
    }

    pub(crate) fn write(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        event: Option<ObservabilityEventCode>,
        count: impl Into<ObservabilityFailureCount>,
        source: &std::io::Error,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Write {
                path,
                event,
                count: count.into(),
                failure: ObservabilityWriteFailure::Io {
                    failure: IoFailure::from_error(source),
                },
            },
        }
    }

    pub(crate) fn write_failure(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        event: Option<ObservabilityEventCode>,
        count: impl Into<ObservabilityFailureCount>,
        failure: ObservabilityWriteFailure,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Write {
                path,
                event,
                count: count.into(),
                failure,
            },
        }
    }

    pub(crate) fn flush(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        source: &std::io::Error,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Flush {
                path,
                failure: IoFailure::from_error(source),
            },
        }
    }

    pub(crate) const fn flush_failure(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        failure: IoFailure,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Flush { path, failure },
        }
    }

    pub(crate) const fn sync_failure(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        failure: IoFailure,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Sync { path, failure },
        }
    }

    pub(crate) fn cleanup(
        component: ObservabilityComponent,
        path: SafePath,
        source: &std::io::Error,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Cleanup {
                path,
                failure: ObservabilityWriteFailure::Io {
                    failure: IoFailure::from_error(source),
                },
            },
        }
    }

    pub(crate) const fn cleanup_failure(
        component: ObservabilityComponent,
        path: SafePath,
        failure: ObservabilityWriteFailure,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Cleanup { path, failure },
        }
    }

    pub(crate) fn serialize(
        component: ObservabilityComponent,
        path: Option<SafePath>,
        event: Option<ObservabilityEventCode>,
        count: impl Into<ObservabilityFailureCount>,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Serialize {
                path,
                event,
                count: count.into(),
            },
        }
    }

    pub(crate) fn channel(
        component: ObservabilityComponent,
        event: Option<ObservabilityEventCode>,
        count: impl Into<ObservabilityFailureCount>,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Channel {
                event,
                count: count.into(),
            },
        }
    }

    pub(crate) fn backpressure(
        component: ObservabilityComponent,
        event: ObservabilityEventCode,
        count: ObservabilityFailureCount,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Backpressure { event, count },
        }
    }

    pub(crate) fn worker(
        component: ObservabilityComponent,
        count: impl Into<ObservabilityFailureCount>,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Worker {
                kind: ObservabilityWorkerFailure::Panicked,
                count: count.into(),
                failure: None,
            },
        }
    }

    pub(crate) fn worker_cancelled(
        component: ObservabilityComponent,
        count: impl Into<ObservabilityFailureCount>,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Worker {
                kind: ObservabilityWorkerFailure::Cancelled,
                count: count.into(),
                failure: None,
            },
        }
    }

    pub(crate) fn worker_start(component: ObservabilityComponent, source: &std::io::Error) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Worker {
                kind: ObservabilityWorkerFailure::Start,
                count: ObservabilityFailureCount::exact(1),
                failure: Some(IoFailure::from_error(source)),
            },
        }
    }

    pub(crate) fn render(
        component: ObservabilityComponent,
        target: ObservabilityRenderTarget,
        event: Option<ObservabilityEventCode>,
        count: impl Into<ObservabilityFailureCount>,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Render {
                target,
                event,
                count: count.into(),
            },
        }
    }

    pub(crate) const fn contract(
        component: ObservabilityComponent,
        violation: ObservabilityContractViolation,
    ) -> Self {
        Self {
            component,
            problem: ObservabilityProblem::Contract { violation },
        }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        match self.component {
            ObservabilityComponent::ProjectLog | ObservabilityComponent::TaskRecord => {
                DiagnosticStage::Logging
            }
            ObservabilityComponent::Stdout | ObservabilityComponent::Stderr => {
                DiagnosticStage::ProcessOutput
            }
        }
    }

    pub(crate) const fn code(&self) -> &'static str {
        let operation = self.problem.operation();
        match self.component {
            ObservabilityComponent::ProjectLog => match operation {
                ObservabilityOperation::Create => "observability.project_log.create",
                ObservabilityOperation::Serialize => "observability.project_log.serialize",
                ObservabilityOperation::Write => "observability.project_log.write",
                ObservabilityOperation::Flush => "observability.project_log.flush",
                ObservabilityOperation::Sync => "observability.project_log.sync",
                ObservabilityOperation::Channel => "observability.project_log.channel",
                ObservabilityOperation::Backpressure => "observability.project_log.backpressure",
                ObservabilityOperation::Worker => "observability.project_log.worker",
                ObservabilityOperation::Render => "observability.project_log.render",
                ObservabilityOperation::Cleanup => "observability.project_log.cleanup",
                ObservabilityOperation::Contract => "observability.project_log.contract",
            },
            ObservabilityComponent::TaskRecord => match operation {
                ObservabilityOperation::Create => "observability.task_record.create",
                ObservabilityOperation::Serialize => "observability.task_record.serialize",
                ObservabilityOperation::Write => "observability.task_record.write",
                ObservabilityOperation::Flush => "observability.task_record.flush",
                ObservabilityOperation::Sync => "observability.task_record.sync",
                ObservabilityOperation::Channel => "observability.task_record.channel",
                ObservabilityOperation::Backpressure => "observability.task_record.backpressure",
                ObservabilityOperation::Worker => "observability.task_record.worker",
                ObservabilityOperation::Render => "observability.task_record.render",
                ObservabilityOperation::Cleanup => "observability.task_record.cleanup",
                ObservabilityOperation::Contract => "observability.task_record.contract",
            },
            ObservabilityComponent::Stdout => match operation {
                ObservabilityOperation::Create => "observability.stdout.create",
                ObservabilityOperation::Serialize => "observability.stdout.serialize",
                ObservabilityOperation::Write => "observability.stdout.write",
                ObservabilityOperation::Flush => "observability.stdout.flush",
                ObservabilityOperation::Sync => "observability.stdout.sync",
                ObservabilityOperation::Channel => "observability.stdout.channel",
                ObservabilityOperation::Backpressure => "observability.stdout.backpressure",
                ObservabilityOperation::Worker => "observability.stdout.worker",
                ObservabilityOperation::Render => "observability.stdout.render",
                ObservabilityOperation::Cleanup => "observability.stdout.cleanup",
                ObservabilityOperation::Contract => "observability.stdout.contract",
            },
            ObservabilityComponent::Stderr => match operation {
                ObservabilityOperation::Create => "observability.stderr.create",
                ObservabilityOperation::Serialize => "observability.stderr.serialize",
                ObservabilityOperation::Write => "observability.stderr.write",
                ObservabilityOperation::Flush => "observability.stderr.flush",
                ObservabilityOperation::Sync => "observability.stderr.sync",
                ObservabilityOperation::Channel => "observability.stderr.channel",
                ObservabilityOperation::Backpressure => "observability.stderr.backpressure",
                ObservabilityOperation::Worker => "observability.stderr.worker",
                ObservabilityOperation::Render => "observability.stderr.render",
                ObservabilityOperation::Cleanup => "observability.stderr.cleanup",
                ObservabilityOperation::Contract => "observability.stderr.contract",
            },
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self.problem.operation() {
            ObservabilityOperation::Create
            | ObservabilityOperation::Write
            | ObservabilityOperation::Flush
            | ObservabilityOperation::Sync
            | ObservabilityOperation::Cleanup => DiagnosticResolution::CheckPathAndPermissions,
            ObservabilityOperation::Serialize
            | ObservabilityOperation::Channel
            | ObservabilityOperation::Worker
            | ObservabilityOperation::Render => DiagnosticResolution::ReportBug,
            ObservabilityOperation::Backpressure => DiagnosticResolution::Retry,
            ObservabilityOperation::Contract => DiagnosticResolution::ReportBug,
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self.problem.operation() {
            ObservabilityOperation::Write => match &self.problem {
                ObservabilityProblem::Write { failure, .. } => failure.summary_code(),
                _ => unreachable!(),
            },
            ObservabilityOperation::Create => match &self.problem {
                ObservabilityProblem::Create { failure, .. } => failure.summary_code(),
                _ => unreachable!(),
            },
            ObservabilityOperation::Flush | ObservabilityOperation::Sync => match &self.problem {
                ObservabilityProblem::Flush { failure, .. }
                | ObservabilityProblem::Sync { failure, .. } => failure.summary_code(),
                _ => unreachable!(),
            },
            ObservabilityOperation::Cleanup => match &self.problem {
                ObservabilityProblem::Cleanup { failure, .. } => failure.summary_code(),
                _ => unreachable!(),
            },
            ObservabilityOperation::Serialize => "request_serialization_failed",
            ObservabilityOperation::Channel => "worker_channel_closed",
            ObservabilityOperation::Backpressure => "resource_limit",
            ObservabilityOperation::Worker => match &self.problem {
                ObservabilityProblem::Worker {
                    kind: ObservabilityWorkerFailure::Start,
                    ..
                } => "worker_spawn_failed",
                ObservabilityProblem::Worker {
                    kind: ObservabilityWorkerFailure::Cancelled,
                    ..
                } => "worker_channel_closed",
                ObservabilityProblem::Worker {
                    kind: ObservabilityWorkerFailure::Panicked,
                    ..
                } => "worker_panicked",
                _ => unreachable!(),
            },
            ObservabilityOperation::Render => "invalid_value",
            ObservabilityOperation::Contract => "state_mismatch",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match &self.problem {
            ObservabilityProblem::Create { path, .. }
            | ObservabilityProblem::Cleanup { path, .. } => path.to_string(),
            ObservabilityProblem::Serialize { path, .. }
            | ObservabilityProblem::Write { path, .. }
            | ObservabilityProblem::Flush { path, .. }
            | ObservabilityProblem::Sync { path, .. } => path
                .as_ref()
                .map_or_else(|| self.component.as_str().to_owned(), ToString::to_string),
            ObservabilityProblem::Channel { .. }
            | ObservabilityProblem::Backpressure { .. }
            | ObservabilityProblem::Worker { .. }
            | ObservabilityProblem::Render { .. }
            | ObservabilityProblem::Contract { .. } => self.component.as_str().to_owned(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![
            ("component", self.component.as_str().to_owned()),
            ("operation", self.problem.operation().as_str().to_owned()),
        ];
        match &self.problem {
            ObservabilityProblem::Create { path, failure } => {
                facts.push(("path", path.to_string()));
                push_io_facts(&mut facts, failure);
            }
            ObservabilityProblem::Cleanup { path, failure } => {
                facts.push(("path", path.to_string()));
                push_write_failure_facts(&mut facts, failure);
            }
            ObservabilityProblem::Serialize { path, event, count } => {
                push_optional_path(&mut facts, path.as_ref());
                push_optional_event(&mut facts, *event);
                push_failure_count_facts(&mut facts, *count);
            }
            ObservabilityProblem::Write {
                path,
                event,
                count,
                failure,
            } => {
                push_optional_path(&mut facts, path.as_ref());
                push_optional_event(&mut facts, *event);
                push_failure_count_facts(&mut facts, *count);
                push_write_failure_facts(&mut facts, failure);
            }
            ObservabilityProblem::Flush { path, failure }
            | ObservabilityProblem::Sync { path, failure } => {
                push_optional_path(&mut facts, path.as_ref());
                push_io_facts(&mut facts, failure);
            }
            ObservabilityProblem::Channel { event, count } => {
                push_optional_event(&mut facts, *event);
                push_failure_count_facts(&mut facts, *count);
            }
            ObservabilityProblem::Backpressure { event, count } => {
                facts.push(("event_code", event.as_str().to_owned()));
                facts.push(("delivery", "best_effort".to_owned()));
                facts.push(("disposition", "dropped".to_owned()));
                push_failure_count_facts(&mut facts, *count);
            }
            ObservabilityProblem::Render {
                target,
                event,
                count,
            } => {
                facts.push(("target", target.as_str().to_owned()));
                push_optional_event(&mut facts, *event);
                push_failure_count_facts(&mut facts, *count);
            }
            ObservabilityProblem::Worker {
                kind,
                count,
                failure,
            } => {
                facts.push(("worker_failure", kind.as_str().to_owned()));
                push_failure_count_facts(&mut facts, *count);
                if let Some(failure) = failure {
                    push_io_facts(&mut facts, failure);
                }
            }
            ObservabilityProblem::Contract { violation } => {
                facts.push(("contract_violation", violation.as_str().to_owned()));
                match violation {
                    ObservabilityContractViolation::RuntimeManagedEvent { event } => {
                        facts.push(("event_code", event.as_str().to_owned()));
                    }
                    ObservabilityContractViolation::UnknownDiagnostic { occurrence_id }
                    | ObservabilityContractViolation::InvalidTerminalDiagnostic { occurrence_id } =>
                    {
                        facts.push(("occurrence_id", occurrence_id.to_string()));
                    }
                    ObservabilityContractViolation::InvalidPhaseTransition { phase } => {
                        facts.push(("phase", phase.as_str().to_owned()));
                    }
                    ObservabilityContractViolation::DuplicateTask { ordinal }
                    | ObservabilityContractViolation::InvalidTaskTransition { ordinal } => {
                        facts.push(("task_ordinal", ordinal.to_string()));
                    }
                    ObservabilityContractViolation::InvalidTaskAttempts { ordinal, attempts } => {
                        facts.push(("task_ordinal", ordinal.to_string()));
                        facts.push(("attempts", attempts.to_string()));
                    }
                    ObservabilityContractViolation::InvalidCancellationCount {
                        confirmed,
                        total,
                    } => {
                        facts.push(("confirmed", confirmed.to_string()));
                        facts.push(("total", total.to_string()));
                    }
                    ObservabilityContractViolation::InvalidRetrySummary {
                        attempted,
                        recovered,
                        exhausted,
                    } => {
                        facts.push(("attempted", attempted.to_string()));
                        facts.push(("recovered", recovered.to_string()));
                        facts.push(("exhausted", exhausted.to_string()));
                    }
                    ObservabilityContractViolation::InvalidContextIdentifier
                    | ObservabilityContractViolation::ProducerClosed
                    | ObservabilityContractViolation::DuplicateRunPlan
                    | ObservabilityContractViolation::InvalidRunPlanTransition
                    | ObservabilityContractViolation::RunPlanCommandMismatch
                    | ObservabilityContractViolation::InvalidRunPlanTransaction
                    | ObservabilityContractViolation::InconsistentTaskTotal
                    | ObservabilityContractViolation::DuplicateTranslationFinished
                    | ObservabilityContractViolation::DuplicatePublication
                    | ObservabilityContractViolation::InvalidPublicationTransition
                    | ObservabilityContractViolation::UnfinishedTasks
                    | ObservabilityContractViolation::TaskSummaryMismatch
                    | ObservabilityContractViolation::TaskCountDoesNotFit
                    | ObservabilityContractViolation::EngineSummaryMismatch
                    | ObservabilityContractViolation::DiagnosticRequiresFailedTranslation
                    | ObservabilityContractViolation::PlanningFailureStartedTasks
                    | ObservabilityContractViolation::OccurrenceIdExhausted
                    | ObservabilityContractViolation::SequenceExhausted
                    | ObservabilityContractViolation::AlreadyFinished
                    | ObservabilityContractViolation::ActivePhase
                    | ObservabilityContractViolation::UnfinalizedRunPlan
                    | ObservabilityContractViolation::MissingTranslationFinished
                    | ObservabilityContractViolation::MissingPublicationFinished
                    | ObservabilityContractViolation::ChannelClosed => {}
                }
            }
        }
        facts
    }
}

fn push_optional_path(facts: &mut Vec<(&'static str, String)>, path: Option<&SafePath>) {
    if let Some(path) = path {
        facts.push(("path", path.to_string()));
    }
}

fn push_optional_event(
    facts: &mut Vec<(&'static str, String)>,
    event: Option<ObservabilityEventCode>,
) {
    if let Some(event) = event {
        facts.push(("event_code", event.as_str().to_owned()));
    }
}

fn push_failure_count_facts(
    facts: &mut Vec<(&'static str, String)>,
    count: ObservabilityFailureCount,
) {
    let minimum = count.minimum().to_string();
    match count {
        ObservabilityFailureCount::Exact { .. } => {
            facts.push(("count", minimum));
        }
        ObservabilityFailureCount::AtLeast { .. } => {
            facts.push(("count_at_least", minimum));
        }
    }
}

fn push_io_facts(facts: &mut Vec<(&'static str, String)>, failure: &IoFailure) {
    facts.push(("io_kind", failure.kind.as_str().to_owned()));
    if let Some(code) = failure.raw_os_code {
        facts.push(("raw_os_code", code.to_string()));
    }
}

fn push_write_failure_facts(
    facts: &mut Vec<(&'static str, String)>,
    failure: &ObservabilityWriteFailure,
) {
    match failure {
        ObservabilityWriteFailure::Io { failure } => push_io_facts(facts, failure),
        ObservabilityWriteFailure::NotPersisted => {
            facts.push(("write_failure", "not_persisted".to_owned()));
        }
        ObservabilityWriteFailure::TargetExists => {
            facts.push(("write_failure", "target_exists".to_owned()));
        }
        ObservabilityWriteFailure::Path { failure } => {
            facts.push(("write_failure", "path".to_owned()));
            facts.push(("path_failure", failure.as_str().to_owned()));
        }
        ObservabilityWriteFailure::IdentityChanged => {
            facts.push(("write_failure", "identity_changed".to_owned()));
        }
        ObservabilityWriteFailure::WindowsStatus { status } => {
            facts.push(("write_failure", "windows_status".to_owned()));
            facts.push(("windows_status", status.to_string()));
        }
        ObservabilityWriteFailure::ExecutorClosed => {
            facts.push(("write_failure", "executor_closed".to_owned()));
        }
        ObservabilityWriteFailure::Cancelled => {
            facts.push(("write_failure", "cancelled".to_owned()));
        }
        ObservabilityWriteFailure::InvalidState => {
            facts.push(("write_failure", "invalid_state".to_owned()));
        }
        ObservabilityWriteFailure::RecoveryRequired => {
            facts.push(("write_failure", "recovery_required".to_owned()));
        }
        ObservabilityWriteFailure::OutcomeUnknown => {
            facts.push(("write_failure", "outcome_unknown".to_owned()));
        }
    }
}

impl ObservabilityWriteFailure {
    const fn summary_code(&self) -> &'static str {
        match self {
            Self::Io { failure } => failure.summary_code(),
            Self::WindowsStatus { .. } | Self::NotPersisted => "operation_failed",
            Self::TargetExists => "target_already_exists",
            Self::Path {
                failure: ObservabilityPathFailure::ReparsePoint,
            } => "reparse_point_forbidden",
            Self::Path {
                failure: ObservabilityPathFailure::NonLocalVolume,
            } => "non_local_volume",
            Self::Path {
                failure: ObservabilityPathFailure::NonNtfsVolume,
            } => "non_ntfs_volume",
            Self::Path {
                failure: ObservabilityPathFailure::CaseSensitiveDirectory,
            } => "case_sensitive_directory",
            Self::Path {
                failure: ObservabilityPathFailure::Invalid,
            } => "invalid_path",
            Self::IdentityChanged => "file_identity_changed",
            Self::ExecutorClosed => "executor_closed",
            Self::Cancelled => "lock_cancelled",
            Self::InvalidState => "state_mismatch",
            Self::RecoveryRequired => "finalization_failed",
            Self::OutcomeUnknown => "transaction_outcome_unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_rejects_unknown_problem_fields() {
        let value = serde_json::json!({
            "component": "project_log",
            "problem": {
                "operation": "serialize",
                "event": "run.started",
                "count": 1,
                "detail": "forbidden"
            }
        });
        assert!(serde_json::from_value::<ObservabilityIssue>(value).is_err());
    }
}
