//! 类型化项目日志内核。
//!
//! 事件的代码、级别和交付方式只由事件变体决定。生产者不能自由拼装这些事实，也不能
//! 选择“可靠写入”入口。所有事件进入同一个 FIFO；只有尽力事件占用固定的在途 permit，
//! 必要事件始终直接进入无界队列。日志只记录业务事实，不参与业务提交或恢复判断。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use async_channel::{Receiver, Sender};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, ObservabilityComponent,
    ObservabilityContractViolation, ObservabilityEventCode, ObservabilityFailureCount,
    ObservabilityIssue, ObservabilityProjectLogPhase, ObservabilityRenderTarget,
    ObservabilityWriteFailure, RelatedFailureRelation, ReportedFailure, SafeIdentifier, SafeIoKind,
    SafePath, SafeText, StateEffect, render_diagnostic_fields,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::observability::RunId;

use super::performance::{RunPerformanceCounters, RunPerformanceSnapshot};

/// 仅限制同时等待 writer 的尽力事件，不限制一个项目或一次运行的事件总量。
const BEST_EFFORT_IN_FLIGHT: usize = 8_192;
const FILE_BUFFER_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectLogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLogDelivery {
    Required,
    BestEffort,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) enum ProjectLogCode {
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
    #[serde(rename = "observability.project_log_degraded")]
    ProjectLogDegraded,
    #[serde(rename = "performance.counters")]
    PerformanceCounters,
    #[serde(rename = "run.finished")]
    RunFinished,
}

impl ProjectLogCode {
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
            Self::ProjectLogDegraded => "observability.project_log_degraded",
            Self::PerformanceCounters => "performance.counters",
            Self::RunFinished => "run.finished",
        }
    }
}

impl From<ProjectLogCode> for ObservabilityEventCode {
    fn from(value: ProjectLogCode) -> Self {
        match value {
            ProjectLogCode::RunStarted => Self::RunStarted,
            ProjectLogCode::CancellationRequested => Self::CancellationRequested,
            ProjectLogCode::PhaseStarted => Self::PhaseStarted,
            ProjectLogCode::PhaseCompleted => Self::PhaseCompleted,
            ProjectLogCode::PhaseStopped => Self::PhaseStopped,
            ProjectLogCode::RunPlanResolved => Self::RunPlanResolved,
            ProjectLogCode::RunPlanFinalized => Self::RunPlanFinalized,
            ProjectLogCode::TaskStarted => Self::TaskStarted,
            ProjectLogCode::TaskFinished => Self::TaskFinished,
            ProjectLogCode::TranslationFinished => Self::TranslationFinished,
            ProjectLogCode::RetrySummary => Self::RetrySummary,
            ProjectLogCode::PublicationStarted => Self::PublicationStarted,
            ProjectLogCode::PublicationFinished => Self::PublicationFinished,
            ProjectLogCode::LuaPrint => Self::LuaPrint,
            ProjectLogCode::RunDiagnostic => Self::RunDiagnostic,
            ProjectLogCode::RunPlanDiagnostic => Self::RunPlanDiagnostic,
            ProjectLogCode::TranslationTaskDiagnostic => Self::TranslationTaskDiagnostic,
            ProjectLogCode::ExtractDiagnostic => Self::ExtractDiagnostic,
            ProjectLogCode::WriteBackDiagnostic => Self::WriteBackDiagnostic,
            ProjectLogCode::PublicationDiagnostic => Self::PublicationDiagnostic,
            ProjectLogCode::TaskRecordDiagnostic => Self::TaskRecordDiagnostic,
            ProjectLogCode::ProjectLogDiagnostic => Self::ProjectLogDiagnostic,
            ProjectLogCode::ProjectLogDegraded => Self::ProjectLogDegraded,
            ProjectLogCode::PerformanceCounters => Self::PerformanceCounters,
            ProjectLogCode::RunFinished => Self::RunFinished,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ProjectLogLocale {
    #[serde(rename = "ar")]
    Arabic,
    #[serde(rename = "zh-Hans")]
    SimplifiedChinese,
    #[serde(rename = "zh-Hant")]
    TraditionalChinese,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "ru")]
    Russian,
    #[serde(rename = "es")]
    Spanish,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "ko")]
    Korean,
    #[serde(rename = "vi")]
    Vietnamese,
}

impl ProjectLogLocale {
    const fn ui_locale(self) -> UiLocale {
        match self {
            Self::Arabic => UiLocale::Arabic,
            Self::SimplifiedChinese => UiLocale::SimplifiedChinese,
            Self::TraditionalChinese => UiLocale::TraditionalChinese,
            Self::English => UiLocale::English,
            Self::French => UiLocale::French,
            Self::Russian => UiLocale::Russian,
            Self::Spanish => UiLocale::Spanish,
            Self::Japanese => UiLocale::Japanese,
            Self::Korean => UiLocale::Korean,
            Self::Vietnamese => UiLocale::Vietnamese,
        }
    }
}

impl From<UiLocale> for ProjectLogLocale {
    fn from(value: UiLocale) -> Self {
        match value {
            UiLocale::Arabic => Self::Arabic,
            UiLocale::SimplifiedChinese => Self::SimplifiedChinese,
            UiLocale::TraditionalChinese => Self::TraditionalChinese,
            UiLocale::English => Self::English,
            UiLocale::French => Self::French,
            UiLocale::Russian => Self::Russian,
            UiLocale::Spanish => Self::Spanish,
            UiLocale::Japanese => Self::Japanese,
            UiLocale::Korean => Self::Korean,
            UiLocale::Vietnamese => Self::Vietnamese,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogEngine {
    Generic,
    RpgMakerMv,
    RpgMakerMz,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogCommand {
    Init,
    Extract,
    Builtin,
    Rules,
    Translate,
    WriteBack,
    Lua,
}

impl ProjectLogCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Extract => "extract",
            Self::Builtin => "builtin",
            Self::Rules => "rules",
            Self::Translate => "translate",
            Self::WriteBack => "write_back",
            Self::Lua => "lua",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectLogContext {
    locale: ProjectLogLocale,
    engine: ProjectLogEngine,
    project: SafeIdentifier,
    command: ProjectLogCommand,
}

impl ProjectLogContext {
    pub(crate) fn new(
        locale: UiLocale,
        engine: ProjectLogEngine,
        project: impl AsRef<str>,
        command: ProjectLogCommand,
    ) -> Result<Self, InvalidProjectLogIdentifier> {
        Ok(Self {
            locale: locale.into(),
            engine,
            project: SafeIdentifier::new(project).map_err(|_| InvalidProjectLogIdentifier)?,
            command,
        })
    }

    pub(crate) const fn locale(&self) -> ProjectLogLocale {
        self.locale
    }

    pub(crate) const fn engine(&self) -> ProjectLogEngine {
        self.engine
    }

    pub(crate) const fn command(&self) -> ProjectLogCommand {
        self.command
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidProjectLogIdentifier;

impl fmt::Display for InvalidProjectLogIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("项目日志标识符不能为空")
    }
}

impl std::error::Error for InvalidProjectLogIdentifier {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
    Planning,
    ConfirmedTasks,
    ReadAssets,
    PlanRpgMakerWriteBack,
    RewriteDocuments,
    ValidateCandidate,
}

impl ProjectLogPhase {
    const fn as_str(self) -> &'static str {
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

impl From<ProjectLogPhase> for ObservabilityProjectLogPhase {
    fn from(value: ProjectLogPhase) -> Self {
        match value {
            ProjectLogPhase::CheckProject => Self::CheckProject,
            ProjectLogPhase::ScanSource => Self::ScanSource,
            ProjectLogPhase::PrepareCandidate => Self::PrepareCandidate,
            ProjectLogPhase::UpdateDatabase => Self::UpdateDatabase,
            ProjectLogPhase::Publish => Self::Publish,
            ProjectLogPhase::Builtin => Self::Builtin,
            ProjectLogPhase::BuiltinDocuments => Self::BuiltinDocuments,
            ProjectLogPhase::BuiltinWorkUnits => Self::BuiltinWorkUnits,
            ProjectLogPhase::BuiltinCommit => Self::BuiltinCommit,
            ProjectLogPhase::Rules => Self::Rules,
            ProjectLogPhase::RulesDocuments => Self::RulesDocuments,
            ProjectLogPhase::RulesMatches => Self::RulesMatches,
            ProjectLogPhase::RulesCommit => Self::RulesCommit,
            ProjectLogPhase::Lua => Self::Lua,
            ProjectLogPhase::Planning => Self::Planning,
            ProjectLogPhase::ConfirmedTasks => Self::ConfirmedTasks,
            ProjectLogPhase::ReadAssets => Self::ReadAssets,
            ProjectLogPhase::PlanRpgMakerWriteBack => Self::PlanRpgMakerWriteBack,
            ProjectLogPhase::RewriteDocuments => Self::RewriteDocuments,
            ProjectLogPhase::ValidateCandidate => Self::ValidateCandidate,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogAmount {
    Indeterminate,
    Determinate { completed: u64, total: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PhaseStopOutcome {
    Failed {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunPlanValueSource {
    Explicit,
    ProjectState,
    ProductDefault,
}

impl RunPlanValueSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::ProjectState => "project_state",
            Self::ProductDefault => "product_default",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractOwnerSelection {
    pub(crate) builtin: bool,
    pub(crate) rules: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "engine", rename_all = "snake_case")]
pub(crate) enum ExtractRunPlanSelection {
    GenericJsonl,
    RpgMaker { owners: ExtractOwnerSelection },
}

impl ExtractOwnerSelection {
    pub(crate) const fn new(builtin: bool, rules: bool) -> Self {
        Self { builtin, rules }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ResolvedRunPlan {
    Init {
        source: RunPlanValueSource,
        game_root: SafePath,
    },
    Extract {
        source: RunPlanValueSource,
        selection: ExtractRunPlanSelection,
    },
    Translate {
        source: RunPlanValueSource,
        profile: SafeIdentifier,
        terminology: Option<SafePath>,
        placeholders: Option<SafePath>,
    },
}

impl ResolvedRunPlan {
    fn source(&self) -> RunPlanValueSource {
        match self {
            Self::Init { source, .. }
            | Self::Extract { source, .. }
            | Self::Translate { source, .. } => *source,
        }
    }

    pub(crate) fn init(source: RunPlanValueSource, game_root: impl AsRef<Path>) -> Self {
        Self::Init {
            source,
            game_root: SafePath::new(game_root),
        }
    }

    pub(crate) const fn generic_extract(source: RunPlanValueSource) -> Self {
        Self::Extract {
            source,
            selection: ExtractRunPlanSelection::GenericJsonl,
        }
    }

    pub(crate) const fn rpg_maker_extract(
        source: RunPlanValueSource,
        owners: ExtractOwnerSelection,
    ) -> Self {
        Self::Extract {
            source,
            selection: ExtractRunPlanSelection::RpgMaker { owners },
        }
    }

    pub(crate) fn translate(
        source: RunPlanValueSource,
        profile: impl AsRef<str>,
        terminology: Option<&Path>,
        placeholders: Option<&Path>,
    ) -> Result<Self, InvalidProjectLogIdentifier> {
        let profile = SafeIdentifier::new(profile).map_err(|_| InvalidProjectLogIdentifier)?;
        Ok(Self::translate_validated(
            source,
            profile,
            terminology,
            placeholders,
        ))
    }

    /// RunPlan 领域边界已经校验 Profile ID 时的无失败构造。
    /// Required 事件的生产者应使用此入口，不得忽略原始字符串的转换失败。
    pub(crate) fn translate_validated(
        source: RunPlanValueSource,
        profile: SafeIdentifier,
        terminology: Option<&Path>,
        placeholders: Option<&Path>,
    ) -> Self {
        Self::Translate {
            source,
            profile,
            terminology: terminology.map(SafePath::new),
            placeholders: placeholders.map(SafePath::new),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunPlanTransactionState {
    NotStarted,
    RolledBack,
    Committed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunPlanFinalization {
    Saved {
        transaction: RunPlanTransactionState,
        run_continues: bool,
    },
    NotSaved {
        transaction: RunPlanTransactionState,
        run_continues: bool,
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    SavedFinalizationFailed {
        transaction: RunPlanTransactionState,
        run_continues: bool,
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    OutcomeUnknown {
        transaction: RunPlanTransactionState,
        run_continues: bool,
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
}

impl RunPlanFinalization {
    fn diagnostic(self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::Saved { .. } => None,
            Self::NotSaved { diagnostic, .. }
            | Self::SavedFinalizationFailed { diagnostic, .. }
            | Self::OutcomeUnknown { diagnostic, .. } => Some(diagnostic),
        }
    }

    const fn is_consistent(self) -> bool {
        matches!(
            self,
            Self::Saved {
                transaction: RunPlanTransactionState::Committed,
                ..
            } | Self::NotSaved {
                transaction: RunPlanTransactionState::NotStarted
                    | RunPlanTransactionState::RolledBack,
                ..
            } | Self::SavedFinalizationFailed {
                transaction: RunPlanTransactionState::Committed,
                ..
            } | Self::OutcomeUnknown {
                transaction: RunPlanTransactionState::OutcomeUnknown,
                ..
            }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidTaskPosition;

impl fmt::Display for InvalidTaskPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("任务序号必须处于 1..=total")
    }
}

impl std::error::Error for InvalidTaskPosition {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskPosition {
    ordinal: u64,
    total: u64,
}

impl TaskPosition {
    pub(crate) fn new(ordinal: u64, total: u64) -> Result<Self, InvalidTaskPosition> {
        if ordinal == 0 || ordinal > total {
            Err(InvalidTaskPosition)
        } else {
            Ok(Self { ordinal, total })
        }
    }

    pub(crate) const fn ordinal(self) -> u64 {
        self.ordinal
    }
}

impl<'de> Deserialize<'de> for TaskPosition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            ordinal: u64,
            total: u64,
        }
        let value = Wire::deserialize(deserializer)?;
        Self::new(value.ordinal, value.total).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TaskFinishedOutcome {
    Complete,
    Partial {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    Unavailable {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    Failed {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    /// 此任务已得到可提交结果，但前序任务失败后编排器没有再应用它。
    ///
    /// 复用前序失败 occurrence，避免把别的任务的失败重新投影为当前 Task 的错误。
    NotCommittedAfterEarlierFailure {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    Cancelled,
}

impl TaskFinishedOutcome {
    const fn counter_kind(self) -> TaskCounterKind {
        match self {
            Self::Complete => TaskCounterKind::Complete,
            Self::Partial { .. } => TaskCounterKind::Partial,
            Self::Unavailable { .. } => TaskCounterKind::Unavailable,
            Self::Failed { .. } | Self::NotCommittedAfterEarlierFailure { .. } => {
                TaskCounterKind::Failed
            }
            Self::Cancelled => TaskCounterKind::Cancelled,
        }
    }

    const fn diagnostic(self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::Partial { diagnostic }
            | Self::Unavailable { diagnostic }
            | Self::Failed { diagnostic }
            | Self::NotCommittedAfterEarlierFailure { diagnostic } => Some(diagnostic),
            Self::Complete | Self::Cancelled => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial { .. } => "partial",
            Self::Unavailable { .. } => "unavailable",
            Self::Failed { .. } => "failed",
            Self::NotCommittedAfterEarlierFailure { .. } => "not_committed_after_earlier_failure",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskCounterKind {
    Complete,
    Partial,
    Unavailable,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskCounterInvariantError {
    StartedBreakdown,
    PlannedBreakdown,
    Overflow,
}

impl fmt::Display for TaskCounterInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StartedBreakdown => "started 与任务终态合计不一致",
            Self::PlannedBreakdown => "planned 与 started + not_started 不一致",
            Self::Overflow => "任务计数相加溢出",
        })
    }
}

impl std::error::Error for TaskCounterInvariantError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TranslationTaskCounters {
    pub(crate) planned: u64,
    pub(crate) started: u64,
    pub(crate) complete: u64,
    pub(crate) partial: u64,
    pub(crate) unavailable: u64,
    pub(crate) failed: u64,
    pub(crate) cancelled: u64,
    pub(crate) not_started: u64,
}

impl TranslationTaskCounters {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        planned: u64,
        started: u64,
        complete: u64,
        partial: u64,
        unavailable: u64,
        failed: u64,
        cancelled: u64,
        not_started: u64,
    ) -> Result<Self, TaskCounterInvariantError> {
        let terminal = complete
            .checked_add(partial)
            .and_then(|value| value.checked_add(unavailable))
            .and_then(|value| value.checked_add(failed))
            .and_then(|value| value.checked_add(cancelled))
            .ok_or(TaskCounterInvariantError::Overflow)?;
        if started != terminal {
            return Err(TaskCounterInvariantError::StartedBreakdown);
        }
        let accounted = started
            .checked_add(not_started)
            .ok_or(TaskCounterInvariantError::Overflow)?;
        if planned != accounted {
            return Err(TaskCounterInvariantError::PlannedBreakdown);
        }
        Ok(Self {
            planned,
            started,
            complete,
            partial,
            unavailable,
            failed,
            cancelled,
            not_started,
        })
    }
}

impl<'de> Deserialize<'de> for TranslationTaskCounters {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            planned: u64,
            started: u64,
            complete: u64,
            partial: u64,
            unavailable: u64,
            failed: u64,
            cancelled: u64,
            not_started: u64,
        }
        let value = Wire::deserialize(deserializer)?;
        Self::new(
            value.planned,
            value.started,
            value.complete,
            value.partial,
            value.unavailable,
            value.failed,
            value.cancelled,
            value.not_started,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericTranslationSummary {
    pub(crate) cleared_units: u64,
    pub(crate) reused_units: u64,
    pub(crate) accepted_units: u64,
    pub(crate) written_units: u64,
    pub(crate) conflicted_units: u64,
    pub(crate) response_problems: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerTranslationSummary {
    pub(crate) accepted_decisions: u64,
    pub(crate) written_locations: u64,
    pub(crate) remaining_decisions: u64,
    pub(crate) remaining_locations: u64,
    pub(crate) protocol_diagnostics: u64,
    pub(crate) recoverable_request_exhaustions: u64,
    pub(crate) retained: u64,
    pub(crate) invalidated: u64,
    pub(crate) not_applicable: u64,
    pub(crate) reused: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    deny_unknown_fields,
    tag = "engine",
    content = "summary",
    rename_all = "snake_case"
)]
pub(crate) enum TranslationEngineSummary {
    Generic(GenericTranslationSummary),
    RpgMaker(RpgMakerTranslationSummary),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranslationFinished {
    NotStarted,
    NoWork {
        tasks: TranslationTaskCounters,
        summary: TranslationEngineSummary,
    },
    Complete {
        tasks: TranslationTaskCounters,
        summary: TranslationEngineSummary,
    },
    Incomplete {
        tasks: TranslationTaskCounters,
        summary: TranslationEngineSummary,
    },
    Failed {
        tasks: TranslationTaskCounters,
        summary: Option<TranslationEngineSummary>,
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    Cancelled {
        tasks: TranslationTaskCounters,
        summary: Option<TranslationEngineSummary>,
    },
}

impl TranslationFinished {
    fn tasks(&self) -> Option<TranslationTaskCounters> {
        match self {
            Self::NotStarted => None,
            Self::NoWork { tasks, .. }
            | Self::Complete { tasks, .. }
            | Self::Incomplete { tasks, .. }
            | Self::Failed { tasks, .. }
            | Self::Cancelled { tasks, .. } => Some(*tasks),
        }
    }

    fn diagnostic(&self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::Failed { diagnostic, .. } => Some(*diagnostic),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericPublicationSummary {
    pub(crate) files: u64,
    pub(crate) translated_units: u64,
    pub(crate) retained_source_units: u64,
    pub(crate) symbol_repair_attempted_units: u64,
    pub(crate) symbol_repair_repaired_units: u64,
    pub(crate) symbol_repair_skipped_units: u64,
    pub(crate) symbol_repair_replacements: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerPublicationSummary {
    pub(crate) translated_units: u64,
    pub(crate) original_units: u64,
    pub(crate) auto_wrapped_units: u64,
    pub(crate) inserted_line_breaks: u64,
    pub(crate) inserted_fullwidth_indents: u64,
    pub(crate) manual_layout_units: u64,
    pub(crate) symbol_repair_attempted_units: u64,
    pub(crate) symbol_repair_repaired_units: u64,
    pub(crate) symbol_repair_skipped_units: u64,
    pub(crate) symbol_repair_replacements: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    deny_unknown_fields,
    tag = "engine",
    content = "summary",
    rename_all = "snake_case"
)]
pub(crate) enum PublicationSummary {
    Generic(GenericPublicationSummary),
    RpgMaker(RpgMakerPublicationSummary),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PublicationFinished {
    Published {
        summary: PublicationSummary,
    },
    NotPublished {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    RecoveryRequired {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    OutcomeUnknown {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
}

impl PublicationFinished {
    const fn diagnostic(self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::Published { .. } => None,
            Self::NotPublished { diagnostic }
            | Self::RecoveryRequired { diagnostic }
            | Self::OutcomeUnknown { diagnostic } => Some(diagnostic),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunFinished {
    Succeeded,
    Cancelled,
    Failed {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    RecoveryRequired {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
    OutcomeUnknown {
        #[serde(skip_serializing)]
        diagnostic: DiagnosticOccurrenceId,
    },
}

impl RunFinished {
    const fn diagnostic(self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::Succeeded | Self::Cancelled => None,
            Self::Failed { diagnostic }
            | Self::RecoveryRequired { diagnostic }
            | Self::OutcomeUnknown { diagnostic } => Some(diagnostic),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct DiagnosticOccurrenceId(u64);

impl DiagnosticOccurrenceId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }

    fn new(value: u64) -> Result<Self, OccurrenceIdExhausted> {
        if value == 0 {
            Err(OccurrenceIdExhausted)
        } else {
            Ok(Self(value))
        }
    }
}

impl<'de> Deserialize<'de> for DiagnosticOccurrenceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OccurrenceIdExhausted;

impl fmt::Display for OccurrenceIdExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("诊断 occurrence ID 已耗尽")
    }
}

impl std::error::Error for OccurrenceIdExhausted {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticScope {
    Run,
    RunPlan,
    TranslationTask,
    Extract,
    WriteBack,
    Publication,
    TaskRecord,
    ProjectLog,
}

impl DiagnosticScope {
    const fn code(self) -> ProjectLogCode {
        match self {
            Self::Run => ProjectLogCode::RunDiagnostic,
            Self::RunPlan => ProjectLogCode::RunPlanDiagnostic,
            Self::TranslationTask => ProjectLogCode::TranslationTaskDiagnostic,
            Self::Extract => ProjectLogCode::ExtractDiagnostic,
            Self::WriteBack => ProjectLogCode::WriteBackDiagnostic,
            Self::Publication => ProjectLogCode::PublicationDiagnostic,
            Self::TaskRecord => ProjectLogCode::TaskRecordDiagnostic,
            Self::ProjectLog => ProjectLogCode::ProjectLogDiagnostic,
        }
    }

    const fn level(self, _effect: StateEffect) -> ProjectLogLevel {
        match self {
            Self::Run | Self::RunPlan | Self::Publication => ProjectLogLevel::Error,
            Self::TranslationTask
            | Self::Extract
            | Self::WriteBack
            | Self::TaskRecord
            | Self::ProjectLog => ProjectLogLevel::Warn,
        }
    }
}

/// 一个问题及其相关失败作为单条 JSONL 事件写入，避免并发事件插入其内部。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiagnosticOccurrence {
    id: DiagnosticOccurrenceId,
    scope: DiagnosticScope,
    report: DiagnosticReport,
}

impl DiagnosticOccurrence {
    pub(crate) const fn id(&self) -> DiagnosticOccurrenceId {
        self.id
    }

    pub(crate) const fn scope(&self) -> DiagnosticScope {
        self.scope
    }

    pub(crate) fn report(&self) -> &DiagnosticReport {
        &self.report
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLogEvent {
    RunStarted,
    CancellationRequested {
        confirmed: u64,
        total: Option<u64>,
    },
    PhaseStarted {
        phase: ProjectLogPhase,
        amount: ProjectLogAmount,
    },
    PhaseCompleted {
        phase: ProjectLogPhase,
        amount: ProjectLogAmount,
    },
    PhaseStopped {
        phase: ProjectLogPhase,
        outcome: PhaseStopOutcome,
    },
    RunPlanResolved {
        plan: ResolvedRunPlan,
    },
    RunPlanFinalized {
        database: SafePath,
        result: RunPlanFinalization,
    },
    TaskStarted {
        task: TaskPosition,
    },
    TaskFinished {
        task: TaskPosition,
        attempts: u64,
        outcome: TaskFinishedOutcome,
    },
    TranslationFinished {
        result: TranslationFinished,
    },
    RetrySummary {
        attempted: u64,
        recovered: u64,
        exhausted: u64,
    },
    PublicationStarted {
        output_root: SafePath,
    },
    PublicationFinished {
        result: PublicationFinished,
    },
    LuaPrint {
        message: SafeText,
    },
    Diagnostic {
        occurrence: DiagnosticOccurrence,
    },
    ProjectLogDegraded {
        health: ProjectLogHealthSnapshot,
    },
    PerformanceCounters {
        snapshot: RunPerformanceSnapshot,
    },
    RunFinished {
        result: RunFinished,
    },
}

impl ProjectLogEvent {
    pub(crate) fn phase_started(phase: ProjectLogPhase, amount: ProjectLogAmount) -> Self {
        Self::PhaseStarted { phase, amount }
    }

    pub(crate) fn phase_completed(phase: ProjectLogPhase, amount: ProjectLogAmount) -> Self {
        Self::PhaseCompleted { phase, amount }
    }

    pub(crate) fn phase_stopped(phase: ProjectLogPhase, outcome: PhaseStopOutcome) -> Self {
        Self::PhaseStopped { phase, outcome }
    }

    pub(crate) fn publication_started(output_root: impl AsRef<Path>) -> Self {
        Self::PublicationStarted {
            output_root: SafePath::new(output_root),
        }
    }

    pub(crate) fn lua_print(message: impl AsRef<str>) -> Self {
        Self::LuaPrint {
            message: SafeText::new(message),
        }
    }

    pub(crate) const fn code(&self) -> ProjectLogCode {
        match self {
            Self::RunStarted => ProjectLogCode::RunStarted,
            Self::CancellationRequested { .. } => ProjectLogCode::CancellationRequested,
            Self::PhaseStarted { .. } => ProjectLogCode::PhaseStarted,
            Self::PhaseCompleted { .. } => ProjectLogCode::PhaseCompleted,
            Self::PhaseStopped { .. } => ProjectLogCode::PhaseStopped,
            Self::RunPlanResolved { .. } => ProjectLogCode::RunPlanResolved,
            Self::RunPlanFinalized { .. } => ProjectLogCode::RunPlanFinalized,
            Self::TaskStarted { .. } => ProjectLogCode::TaskStarted,
            Self::TaskFinished { .. } => ProjectLogCode::TaskFinished,
            Self::TranslationFinished { .. } => ProjectLogCode::TranslationFinished,
            Self::RetrySummary { .. } => ProjectLogCode::RetrySummary,
            Self::PublicationStarted { .. } => ProjectLogCode::PublicationStarted,
            Self::PublicationFinished { .. } => ProjectLogCode::PublicationFinished,
            Self::LuaPrint { .. } => ProjectLogCode::LuaPrint,
            Self::Diagnostic { occurrence } => occurrence.scope().code(),
            Self::ProjectLogDegraded { .. } => ProjectLogCode::ProjectLogDegraded,
            Self::PerformanceCounters { .. } => ProjectLogCode::PerformanceCounters,
            Self::RunFinished { .. } => ProjectLogCode::RunFinished,
        }
    }

    pub(crate) fn level(&self) -> ProjectLogLevel {
        match self {
            Self::RunStarted
            | Self::PhaseStarted { .. }
            | Self::PhaseCompleted { .. }
            | Self::RunPlanResolved { .. }
            | Self::RunPlanFinalized {
                result: RunPlanFinalization::Saved { .. },
                ..
            }
            | Self::TaskStarted { .. }
            | Self::TaskFinished {
                outcome: TaskFinishedOutcome::Complete,
                ..
            }
            | Self::TranslationFinished {
                result: TranslationFinished::NoWork { .. } | TranslationFinished::Complete { .. },
            }
            | Self::RetrySummary { .. }
            | Self::PublicationStarted { .. }
            | Self::PublicationFinished {
                result: PublicationFinished::Published { .. },
            }
            | Self::PerformanceCounters { .. }
            | Self::RunFinished {
                result: RunFinished::Succeeded | RunFinished::Cancelled,
            } => ProjectLogLevel::Info,
            Self::CancellationRequested { .. }
            | Self::PhaseStopped {
                outcome: PhaseStopOutcome::Cancelled,
                ..
            }
            | Self::TaskFinished {
                outcome:
                    TaskFinishedOutcome::Partial { .. }
                    | TaskFinishedOutcome::Unavailable { .. }
                    | TaskFinishedOutcome::Cancelled,
                ..
            }
            | Self::TranslationFinished {
                result:
                    TranslationFinished::NotStarted
                    | TranslationFinished::Incomplete { .. }
                    | TranslationFinished::Cancelled { .. },
            }
            | Self::ProjectLogDegraded { .. } => ProjectLogLevel::Warn,
            Self::PhaseStopped {
                outcome: PhaseStopOutcome::Failed { .. },
                ..
            }
            | Self::RunPlanFinalized { .. }
            | Self::TaskFinished {
                outcome:
                    TaskFinishedOutcome::Failed { .. }
                    | TaskFinishedOutcome::NotCommittedAfterEarlierFailure { .. },
                ..
            }
            | Self::TranslationFinished {
                result: TranslationFinished::Failed { .. },
            }
            | Self::PublicationFinished { .. }
            | Self::RunFinished { .. } => ProjectLogLevel::Error,
            Self::LuaPrint { .. } => ProjectLogLevel::Debug,
            Self::Diagnostic { occurrence } => {
                occurrence.scope().level(occurrence.report().effect())
            }
        }
    }

    pub(crate) const fn delivery(&self) -> ProjectLogDelivery {
        match self {
            Self::PhaseStarted { .. }
            | Self::PhaseCompleted { .. }
            | Self::TaskStarted { .. }
            | Self::RetrySummary { .. }
            | Self::LuaPrint { .. } => ProjectLogDelivery::BestEffort,
            Self::RunStarted
            | Self::CancellationRequested { .. }
            | Self::PhaseStopped { .. }
            | Self::RunPlanResolved { .. }
            | Self::RunPlanFinalized { .. }
            | Self::TaskFinished { .. }
            | Self::TranslationFinished { .. }
            | Self::PublicationStarted { .. }
            | Self::PublicationFinished { .. }
            | Self::Diagnostic { .. }
            | Self::ProjectLogDegraded { .. }
            | Self::PerformanceCounters { .. }
            | Self::RunFinished { .. } => ProjectLogDelivery::Required,
        }
    }

    /// `UiLocalizer` 会构建两份 Fluent bundle；writer 为整次运行复用同一个实例，不能在
    /// 每条 Task 事件上重复构建。
    fn message(&self, context: &ProjectLogContext, localizer: &UiLocalizer) -> String {
        match self {
            Self::RunStarted => localizer.format(UiMessage::LogRunStarted {
                command: context.command().as_str(),
            }),
            Self::RunFinished {
                result: RunFinished::Succeeded,
            } => localizer.format(UiMessage::LogRunSucceeded {
                command: context.command().as_str(),
            }),
            Self::RunFinished {
                result: RunFinished::Cancelled,
            } => localizer.format(UiMessage::LogRunCancelled {
                command: context.command().as_str(),
            }),
            Self::RunFinished {
                result: RunFinished::OutcomeUnknown { .. },
            } => localizer.format(UiMessage::LogRunOutcomeUnknown {
                command: context.command().as_str(),
            }),
            Self::RunFinished {
                result: RunFinished::RecoveryRequired { .. },
            } => localizer.format(UiMessage::LogRunRecoveryRequired {
                command: context.command().as_str(),
            }),
            Self::RunFinished { .. } => localizer.format(UiMessage::LogRunFailed {
                command: context.command().as_str(),
            }),
            Self::RunPlanResolved { plan } => localizer.format(UiMessage::LogPlanResolved {
                command: context.command().as_str(),
                source: plan.source().as_str(),
            }),
            Self::PhaseStarted { phase, .. } => localizer.format(UiMessage::LogPhaseStarted {
                phase: phase.as_str(),
            }),
            Self::PhaseCompleted { phase, .. } => localizer.format(UiMessage::LogPhaseCompleted {
                phase: phase.as_str(),
            }),
            Self::PhaseStopped { phase, outcome } => {
                let outcome = match outcome {
                    PhaseStopOutcome::Failed { .. } => "failed",
                    PhaseStopOutcome::Cancelled => "cancelled",
                };
                localizer.format(UiMessage::LogPhaseStopped {
                    phase: phase.as_str(),
                    outcome,
                })
            }
            Self::CancellationRequested { confirmed, total } => match total {
                Some(total) => localizer.format(UiMessage::LogCancellationRequested {
                    confirmed: *confirmed,
                    total: *total,
                }),
                None => localizer.format(UiMessage::LogCancellationRequestedIndeterminate {
                    confirmed: *confirmed,
                }),
            },
            Self::RunPlanFinalized { result, .. } => {
                let result = match result {
                    RunPlanFinalization::Saved { .. } => "saved",
                    RunPlanFinalization::NotSaved { .. } => "not_saved",
                    RunPlanFinalization::SavedFinalizationFailed { .. } => {
                        "saved_finalization_failed"
                    }
                    RunPlanFinalization::OutcomeUnknown { .. } => "outcome_unknown",
                };
                localizer.format(UiMessage::LogRunPlanFinalized { result })
            }
            Self::RetrySummary { attempted, .. } => {
                localizer.format(UiMessage::LogRetrySummary { count: *attempted })
            }
            Self::TaskStarted { task } => localizer.format(UiMessage::LogTranslationTaskStarted {
                index: task.ordinal,
                total: task.total,
            }),
            Self::TaskFinished { task, outcome, .. } => {
                let outcome = localizer.format(UiMessage::LogTaskOutcomeValue {
                    outcome: outcome.as_str(),
                });
                localizer.format(UiMessage::LogTranslationTaskFinished {
                    index: task.ordinal,
                    outcome: &outcome,
                })
            }
            Self::TranslationFinished { result } => {
                let result = match result {
                    TranslationFinished::NotStarted => "not_started",
                    TranslationFinished::NoWork { .. } => "no_work",
                    TranslationFinished::Complete { .. } => "complete",
                    TranslationFinished::Incomplete { .. } => "incomplete",
                    TranslationFinished::Failed { .. } => "failed",
                    TranslationFinished::Cancelled { .. } => "cancelled",
                };
                localizer.format(UiMessage::LogTranslationFinished { result })
            }
            Self::PublicationStarted { output_root } => {
                localizer.format(UiMessage::LogPublicationStarted {
                    path: output_root.as_str(),
                })
            }
            Self::PublicationFinished { result } => {
                let result = match result {
                    PublicationFinished::Published { .. } => "published",
                    PublicationFinished::NotPublished { .. } => "not_published",
                    PublicationFinished::RecoveryRequired { .. } => "recovery_required",
                    PublicationFinished::OutcomeUnknown { .. } => "outcome_unknown",
                };
                localizer.format(UiMessage::LogPublicationFinished { result })
            }
            Self::ProjectLogDegraded { health } => {
                let failure_kinds = u64::try_from(health.failures.len())
                    .expect("受支持平台的 usize 必须可表示为 u64");
                localizer.format(UiMessage::LogProjectLogDegraded { failure_kinds })
            }
            Self::LuaPrint { message } => localizer.format(UiMessage::LogLuaPrint {
                message: message.as_str(),
            }),
            Self::PerformanceCounters { snapshot } => {
                localizer.format(UiMessage::LogPerformanceCounters {
                    sqlite_control_attempted_total: snapshot.sqlite_transactions.attempted_total(),
                    candidate_validation_started: snapshot.candidate_validations.started,
                    candidate_validation_completed: snapshot.candidate_validations.completed,
                })
            }
            Self::Diagnostic { occurrence } => {
                let rendered = render_diagnostic_fields(occurrence.report(), localizer);
                format!("{}: {} {}", rendered.object, rendered.reason, rendered.help)
            }
        }
    }

    fn referenced_diagnostic(&self) -> Option<DiagnosticOccurrenceId> {
        match self {
            Self::PhaseStopped {
                outcome: PhaseStopOutcome::Failed { diagnostic },
                ..
            } => Some(*diagnostic),
            Self::RunPlanFinalized { result, .. } => result.diagnostic(),
            Self::TaskFinished { outcome, .. } => outcome.diagnostic(),
            Self::TranslationFinished { result } => result.diagnostic(),
            Self::PublicationFinished { result } => result.diagnostic(),
            Self::RunFinished { result } => result.diagnostic(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogFailureKey {
    BestEffortBackpressure {
        code: ProjectLogCode,
    },
    Serialize {
        path: Option<SafePath>,
        code: ProjectLogCode,
    },
    Write {
        path: Option<SafePath>,
        code: ProjectLogCode,
        io_kind: SafeIoKind,
        raw_os_code: Option<i32>,
    },
    Flush {
        path: Option<SafePath>,
        io_kind: SafeIoKind,
        raw_os_code: Option<i32>,
    },
    Sync {
        path: Option<SafePath>,
        io_kind: SafeIoKind,
        raw_os_code: Option<i32>,
    },
    ChannelClosed {
        code: Option<ProjectLogCode>,
    },
    WorkerPanicked,
    SequenceExhausted,
    OccurrenceIdExhausted,
    NotPersisted {
        code: ProjectLogCode,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectLogFailureCount {
    pub(crate) failure: ProjectLogFailureKey,
    pub(crate) count: ObservabilityFailureCount,
}

impl ProjectLogFailureCount {
    /// 将健康计数投影为可直接呈现的安全诊断，不解析错误正文。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let issue = match &self.failure {
            ProjectLogFailureKey::BestEffortBackpressure { code } => {
                ObservabilityIssue::backpressure(
                    ObservabilityComponent::ProjectLog,
                    (*code).into(),
                    self.count,
                )
            }
            ProjectLogFailureKey::ChannelClosed { code: Some(code) } => {
                ObservabilityIssue::channel(
                    ObservabilityComponent::ProjectLog,
                    Some((*code).into()),
                    self.count,
                )
            }
            ProjectLogFailureKey::Serialize { path, code } => ObservabilityIssue::serialize(
                ObservabilityComponent::ProjectLog,
                path.clone(),
                Some((*code).into()),
                self.count,
            ),
            ProjectLogFailureKey::Write {
                path,
                code,
                io_kind,
                raw_os_code,
            } => ObservabilityIssue::write_failure(
                ObservabilityComponent::ProjectLog,
                path.clone(),
                Some((*code).into()),
                self.count,
                ObservabilityWriteFailure::Io {
                    failure: IoFailure::from_parts(*io_kind, *raw_os_code),
                },
            ),
            ProjectLogFailureKey::Flush {
                path,
                io_kind,
                raw_os_code,
            } => ObservabilityIssue::flush_failure(
                ObservabilityComponent::ProjectLog,
                path.clone(),
                IoFailure::from_parts(*io_kind, *raw_os_code),
            ),
            ProjectLogFailureKey::Sync {
                path,
                io_kind,
                raw_os_code,
            } => ObservabilityIssue::sync_failure(
                ObservabilityComponent::ProjectLog,
                path.clone(),
                IoFailure::from_parts(*io_kind, *raw_os_code),
            ),
            ProjectLogFailureKey::ChannelClosed { code: None } => {
                ObservabilityIssue::channel(ObservabilityComponent::ProjectLog, None, self.count)
            }
            ProjectLogFailureKey::WorkerPanicked => {
                ObservabilityIssue::worker(ObservabilityComponent::ProjectLog, self.count)
            }
            ProjectLogFailureKey::SequenceExhausted => ObservabilityIssue::render(
                ObservabilityComponent::ProjectLog,
                ObservabilityRenderTarget::Sequence,
                None,
                self.count,
            ),
            ProjectLogFailureKey::OccurrenceIdExhausted => ObservabilityIssue::render(
                ObservabilityComponent::ProjectLog,
                ObservabilityRenderTarget::OccurrenceId,
                None,
                self.count,
            ),
            ProjectLogFailureKey::NotPersisted { code } => ObservabilityIssue::write_failure(
                ObservabilityComponent::ProjectLog,
                None,
                Some((*code).into()),
                self.count,
                ObservabilityWriteFailure::NotPersisted,
            ),
        };
        DiagnosticReport::new(StateEffect::Unchanged, Diagnostic::observability(issue))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectLogHealthSnapshot {
    pub(crate) failures: Vec<ProjectLogFailureCount>,
}

impl ProjectLogHealthSnapshot {
    pub(crate) fn is_healthy(&self) -> bool {
        self.failures.is_empty()
    }

    #[cfg(test)]
    fn count(&self, key: &ProjectLogFailureKey) -> u64 {
        self.failures
            .iter()
            .find(|entry| &entry.failure == key)
            .map_or(0, |entry| entry.count.minimum())
    }
}

/// stderr 呈现者持有自己的消费游标；每次只取得各故障键相对上次已呈现的新增次数。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLogHealthCursor {
    observed: BTreeMap<ProjectLogFailureKey, ObservabilityFailureCount>,
}

impl ProjectLogHealthCursor {
    pub(crate) fn consume(
        &mut self,
        snapshot: &ProjectLogHealthSnapshot,
    ) -> Vec<ProjectLogFailureCount> {
        let mut fresh = Vec::new();
        for entry in &snapshot.failures {
            if let Some(count) = entry
                .count
                .additional_since(self.observed.get(&entry.failure).copied())
            {
                fresh.push(ProjectLogFailureCount {
                    failure: entry.failure.clone(),
                    count,
                });
            }
            self.observed.insert(entry.failure.clone(), entry.count);
        }
        fresh
    }
}

#[derive(Default)]
struct ProjectLogHealth {
    counts: Mutex<BTreeMap<ProjectLogFailureKey, ObservabilityFailureCount>>,
    observer: Mutex<Option<Arc<ProjectLogHealthObserver>>>,
}

type ProjectLogHealthObserver = dyn Fn(ProjectLogHealthSnapshot) + Send + Sync + 'static;

impl ProjectLogHealth {
    fn record(&self, key: ProjectLogFailureKey) {
        let is_new = {
            let mut counts = lock_unpoisoned(&self.counts);
            let is_new = !counts.contains_key(&key);
            let count = counts
                .entry(key)
                .or_insert_with(|| ObservabilityFailureCount::exact(0));
            // 健康记录自身绝不能让业务线程 panic。精确 u64 溢出后转为 at_least，
            // 不把可表示下界伪装成真实的精确次数。
            *count = count.increment();
            is_new
        };
        // 同一种故障的重复发生只累计；即时呈现只通知一次，避免队列压力反过来阻塞
        // 业务线程。最终健康快照仍保留完整次数。
        if is_new {
            self.notify_observer();
        }
    }

    fn snapshot(&self) -> ProjectLogHealthSnapshot {
        ProjectLogHealthSnapshot {
            failures: lock_unpoisoned(&self.counts)
                .iter()
                .map(|(failure, count)| ProjectLogFailureCount {
                    failure: failure.clone(),
                    count: *count,
                })
                .collect(),
        }
    }

    fn install_observer(&self, observer: Arc<ProjectLogHealthObserver>) {
        *lock_unpoisoned(&self.observer) = Some(Arc::clone(&observer));
        observer(self.snapshot());
    }

    fn clear_observer(&self) {
        lock_unpoisoned(&self.observer).take();
    }

    fn notify_observer(&self) {
        let observer = lock_unpoisoned(&self.observer).clone();
        if let Some(observer) = observer {
            observer(self.snapshot());
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectLogRecord {
    timestamp: String,
    sequence: u64,
    run_id: String,
    level: ProjectLogLevel,
    code: ProjectLogCode,
    context: ProjectLogContext,
    payload: ProjectLogEvent,
    message: String,
}

impl ProjectLogRecord {
    fn new(
        timestamp: OffsetDateTime,
        sequence: u64,
        run_id: &str,
        context: &ProjectLogContext,
        localizer: &UiLocalizer,
        payload: ProjectLogEvent,
    ) -> Result<Self, RecordBuildError> {
        if sequence == 0 {
            return Err(RecordBuildError);
        }
        let timestamp = timestamp.format(&Rfc3339).map_err(|_| RecordBuildError)?;
        Ok(Self {
            timestamp,
            sequence,
            run_id: run_id.to_owned(),
            level: payload.level(),
            code: payload.code(),
            context: context.clone(),
            message: payload.message(context, localizer),
            payload,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectLogRecordRef<'a> {
    timestamp: &'a str,
    sequence: u64,
    run_id: &'a str,
    level: ProjectLogLevel,
    event: ProjectLogCode,
    context: &'a ProjectLogContext,
    payload: ProjectLogPayloadRef<'a>,
    message: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ProjectLogPayloadRef<'a> {
    RunStarted {},
    CancellationRequested {
        confirmed: &'a u64,
        total: &'a Option<u64>,
    },
    PhaseProgress {
        phase: &'a ProjectLogPhase,
        amount: &'a ProjectLogAmount,
    },
    PhaseStopped {
        phase: &'a ProjectLogPhase,
        outcome: &'a PhaseStopOutcome,
    },
    RunPlanResolved {
        plan: &'a ResolvedRunPlan,
    },
    RunPlanFinalized {
        database: &'a SafePath,
        result: &'a RunPlanFinalization,
    },
    TaskStarted {
        task: &'a TaskPosition,
    },
    TaskFinished {
        task: &'a TaskPosition,
        attempts: &'a u64,
        outcome: &'a TaskFinishedOutcome,
    },
    TranslationFinished {
        result: &'a TranslationFinished,
    },
    RetrySummary {
        attempted: &'a u64,
        recovered: &'a u64,
        exhausted: &'a u64,
    },
    PublicationStarted {
        output_root: &'a SafePath,
    },
    PublicationFinished {
        result: &'a PublicationFinished,
    },
    LuaPrint {
        message: &'a SafeText,
    },
    Diagnostic {
        object: SafeText,
        reason: SafeText,
        help: SafeText,
    },
    ProjectLogDegraded {
        issues: u64,
    },
    PerformanceCounters {
        snapshot: &'a RunPerformanceSnapshot,
    },
    RunFinished {
        result: &'a RunFinished,
    },
}

impl<'a> ProjectLogPayloadRef<'a> {
    fn from_event(event: &'a ProjectLogEvent, locale: ProjectLogLocale) -> Self {
        match event {
            ProjectLogEvent::RunStarted => Self::RunStarted {},
            ProjectLogEvent::CancellationRequested { confirmed, total } => {
                Self::CancellationRequested { confirmed, total }
            }
            ProjectLogEvent::PhaseStarted { phase, amount }
            | ProjectLogEvent::PhaseCompleted { phase, amount } => {
                Self::PhaseProgress { phase, amount }
            }
            ProjectLogEvent::PhaseStopped { phase, outcome } => {
                Self::PhaseStopped { phase, outcome }
            }
            ProjectLogEvent::RunPlanResolved { plan } => Self::RunPlanResolved { plan },
            ProjectLogEvent::RunPlanFinalized { database, result } => {
                Self::RunPlanFinalized { database, result }
            }
            ProjectLogEvent::TaskStarted { task } => Self::TaskStarted { task },
            ProjectLogEvent::TaskFinished {
                task,
                attempts,
                outcome,
            } => Self::TaskFinished {
                task,
                attempts,
                outcome,
            },
            ProjectLogEvent::TranslationFinished { result } => Self::TranslationFinished { result },
            ProjectLogEvent::RetrySummary {
                attempted,
                recovered,
                exhausted,
            } => Self::RetrySummary {
                attempted,
                recovered,
                exhausted,
            },
            ProjectLogEvent::PublicationStarted { output_root } => {
                Self::PublicationStarted { output_root }
            }
            ProjectLogEvent::PublicationFinished { result } => Self::PublicationFinished { result },
            ProjectLogEvent::LuaPrint { message } => Self::LuaPrint { message },
            ProjectLogEvent::Diagnostic { occurrence } => {
                let localizer = UiLocalizer::new(locale.ui_locale());
                let rendered = render_diagnostic_fields(occurrence.report(), &localizer);
                Self::Diagnostic {
                    object: SafeText::new(rendered.object),
                    reason: SafeText::new(rendered.reason),
                    help: SafeText::new(rendered.help),
                }
            }
            ProjectLogEvent::ProjectLogDegraded { health } => Self::ProjectLogDegraded {
                issues: u64::try_from(health.failures.len())
                    .expect("受支持平台的 usize 必须可表示为 u64"),
            },
            ProjectLogEvent::PerformanceCounters { snapshot } => {
                Self::PerformanceCounters { snapshot }
            }
            ProjectLogEvent::RunFinished { result } => Self::RunFinished { result },
        }
    }
}

impl Serialize for ProjectLogRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ProjectLogRecordRef {
            timestamp: &self.timestamp,
            sequence: self.sequence,
            run_id: &self.run_id,
            level: self.level,
            event: self.code,
            context: &self.context,
            payload: ProjectLogPayloadRef::from_event(&self.payload, self.context.locale()),
            message: &self.message,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordBuildError;

trait ProjectLogRecordEncoder: Send + 'static {
    fn encode(&mut self, record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError>;
}

#[derive(Default)]
struct JsonProjectLogRecordEncoder;

impl ProjectLogRecordEncoder for JsonProjectLogRecordEncoder {
    fn encode(&mut self, record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError> {
        let mut bytes = serde_json::to_vec(record).map_err(|_| RecordEncodeError)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordEncodeError;

/// writer 的最小边界。文件适配器可分别实现 write、flush 和持久同步，测试可精确注入
/// 第 N 次故障；内核不会从错误正文解析公开事实。
pub(crate) trait ProjectLogSink: Send + 'static {
    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;

    fn path(&self) -> Option<&SafePath> {
        None
    }
}

struct FileProjectLogSink {
    path: SafePath,
    writer: BufWriter<File>,
}

impl FileProjectLogSink {
    #[cfg(test)]
    fn create_new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Self::from_reserved(path, file))
    }

    fn from_reserved(path: &Path, file: File) -> Self {
        Self {
            path: SafePath::new(path),
            writer: BufWriter::with_capacity(FILE_BUFFER_BYTES, file),
        }
    }
}

impl ProjectLogSink for FileProjectLogSink {
    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn sync(&mut self) -> io::Result<()> {
        self.writer.get_ref().sync_all()
    }

    fn path(&self) -> Option<&SafePath> {
        Some(&self.path)
    }
}

trait ProjectLogClock: Send + Sync + 'static {
    fn now(&self) -> OffsetDateTime;
}

#[derive(Debug)]
struct SystemProjectLogClock;

impl ProjectLogClock for SystemProjectLogClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermitKind {
    Required,
    BestEffort,
}

#[derive(Debug)]
struct BestEffortPermits {
    in_flight: AtomicUsize,
}

impl BestEffortPermits {
    const fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
        }
    }

    fn try_acquire(&self) -> bool {
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < BEST_EFFORT_IN_FLIGHT).then_some(current + 1)
            })
            .is_ok()
    }

    fn release(&self) {
        let previous = self.in_flight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "尽力事件 permit 不能重复释放");
    }
}

struct QueuedEvent {
    emitted_at: OffsetDateTime,
    sequence: u64,
    event: ProjectLogEvent,
    permit: PermitKind,
}

struct FinalizeRequest {
    terminal_diagnostics: Vec<DiagnosticOccurrence>,
    performance: RunPerformanceSnapshot,
    result: RunFinished,
}

enum QueueItem {
    Event(QueuedEvent),
    Finalize(FinalizeRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OccurrenceState {
    Emitted,
    PreparedTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OccurrenceRegistration {
    state: OccurrenceState,
    scope: DiagnosticScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhaseState {
    Started,
    Completed,
    Stopped,
}

#[derive(Debug)]
struct ProducerState {
    sender: Option<Sender<QueueItem>>,
    finalized: bool,
    terminal_preparing: bool,
    cancellation_requested: bool,
    phases: BTreeMap<ProjectLogPhase, PhaseState>,
    run_plan_resolved: bool,
    run_plan_finalized: bool,
    tasks_started: BTreeSet<u64>,
    task_outcomes: BTreeMap<u64, TaskCounterKind>,
    task_total: Option<u64>,
    translation_finished: bool,
    publication_started: bool,
    publication_finished: bool,
    occurrences: BTreeMap<DiagnosticOccurrenceId, OccurrenceRegistration>,
}

impl ProducerState {
    fn new(sender: Sender<QueueItem>) -> Self {
        Self {
            sender: Some(sender),
            finalized: false,
            terminal_preparing: false,
            cancellation_requested: false,
            phases: BTreeMap::new(),
            run_plan_resolved: false,
            run_plan_finalized: false,
            tasks_started: BTreeSet::new(),
            task_outcomes: BTreeMap::new(),
            task_total: None,
            translation_finished: false,
            publication_started: false,
            publication_finished: false,
            occurrences: BTreeMap::new(),
        }
    }

    fn contains_emitted_diagnostic(&self, id: DiagnosticOccurrenceId) -> bool {
        self.occurrences
            .get(&id)
            .is_some_and(|entry| entry.state == OccurrenceState::Emitted)
    }

    fn diagnostic_scope(&self, id: DiagnosticOccurrenceId) -> Option<DiagnosticScope> {
        self.occurrences.get(&id).map(|entry| entry.scope)
    }

    fn validate_event(
        &mut self,
        event: &ProjectLogEvent,
        context: &ProjectLogContext,
    ) -> Result<EmitDisposition, EmitError> {
        if self.finalized || self.terminal_preparing {
            return Err(EmitError::Closed);
        }
        if let Some(diagnostic) = event.referenced_diagnostic()
            && !self.contains_emitted_diagnostic(diagnostic)
        {
            return Err(EmitError::UnknownDiagnostic(diagnostic));
        }
        if let Some(diagnostic) = event.referenced_diagnostic() {
            let expected_scope = match event {
                ProjectLogEvent::RunPlanFinalized { .. } => Some(DiagnosticScope::RunPlan),
                ProjectLogEvent::TaskFinished { .. } => Some(DiagnosticScope::TranslationTask),
                ProjectLogEvent::PublicationFinished { .. } => Some(DiagnosticScope::Publication),
                _ => None,
            };
            if expected_scope.is_some_and(|scope| self.diagnostic_scope(diagnostic) != Some(scope))
            {
                return Err(EmitError::InvalidDiagnosticScope(diagnostic));
            }
        }
        match event {
            ProjectLogEvent::RunStarted
            | ProjectLogEvent::ProjectLogDegraded { .. }
            | ProjectLogEvent::PerformanceCounters { .. }
            | ProjectLogEvent::RunFinished { .. }
            | ProjectLogEvent::Diagnostic { .. } => {
                return Err(EmitError::RuntimeManagedEvent {
                    event: event.code(),
                });
            }
            ProjectLogEvent::CancellationRequested { confirmed, total } => {
                if total.is_some_and(|total| *confirmed > total) {
                    return Err(EmitError::InvalidCancellationCount {
                        confirmed: *confirmed,
                        total: total.expect("is_some_and 已确认 total 存在"),
                    });
                }
                if self.cancellation_requested {
                    return Ok(EmitDisposition::DuplicateSuppressed);
                }
                self.cancellation_requested = true;
            }
            ProjectLogEvent::PhaseStarted { phase, .. } => {
                if self.phases.contains_key(phase) {
                    return Err(EmitError::InvalidPhaseTransition(*phase));
                }
                self.phases.insert(*phase, PhaseState::Started);
            }
            ProjectLogEvent::PhaseCompleted { phase, .. } => {
                if self.phases.get(phase) != Some(&PhaseState::Started) {
                    return Err(EmitError::InvalidPhaseTransition(*phase));
                }
                self.phases.insert(*phase, PhaseState::Completed);
            }
            ProjectLogEvent::PhaseStopped { phase, .. } => {
                if self.phases.get(phase) != Some(&PhaseState::Started) {
                    return Err(EmitError::InvalidPhaseTransition(*phase));
                }
                self.phases.insert(*phase, PhaseState::Stopped);
            }
            ProjectLogEvent::RunPlanResolved { plan } => {
                if self.run_plan_resolved {
                    return Err(EmitError::DuplicateRunPlan);
                }
                let command_matches = matches!(
                    (context.command(), plan),
                    (ProjectLogCommand::Init, ResolvedRunPlan::Init { .. })
                        | (ProjectLogCommand::Extract, ResolvedRunPlan::Extract { .. })
                        | (
                            ProjectLogCommand::Translate,
                            ResolvedRunPlan::Translate { .. }
                        )
                );
                if !command_matches {
                    return Err(EmitError::RunPlanCommandMismatch);
                }
                self.run_plan_resolved = true;
            }
            ProjectLogEvent::RunPlanFinalized { result, .. } => {
                if !self.run_plan_resolved || self.run_plan_finalized {
                    return Err(EmitError::InvalidRunPlanTransition);
                }
                if !result.is_consistent() {
                    return Err(EmitError::InvalidRunPlanTransaction);
                }
                self.run_plan_finalized = true;
            }
            ProjectLogEvent::TaskStarted { task } => {
                if self.task_total.is_some_and(|total| total != task.total) {
                    return Err(EmitError::InconsistentTaskTotal);
                }
                self.task_total = Some(task.total);
                if !self.tasks_started.insert(task.ordinal()) {
                    return Err(EmitError::DuplicateTask(task.ordinal()));
                }
            }
            ProjectLogEvent::TaskFinished { task, outcome, .. } => {
                if self.task_total != Some(task.total) {
                    return Err(EmitError::InconsistentTaskTotal);
                }
                if !self.tasks_started.contains(&task.ordinal())
                    || self
                        .task_outcomes
                        .insert(task.ordinal(), outcome.counter_kind())
                        .is_some()
                {
                    return Err(EmitError::InvalidTaskTransition(task.ordinal()));
                }
            }
            ProjectLogEvent::TranslationFinished { result } => {
                if self.translation_finished {
                    return Err(EmitError::DuplicateTranslationFinished);
                }
                if let Some(counters) = result.tasks() {
                    self.validate_task_counters(counters)?;
                    if let Some(total) = self.task_total
                        && counters.planned != total
                    {
                        return Err(EmitError::TaskSummaryMismatch);
                    }
                    if matches!(result, TranslationFinished::NoWork { .. }) && counters.planned != 0
                    {
                        return Err(EmitError::TaskSummaryMismatch);
                    }
                    let summary_matches = matches!(
                        (context.engine(), result),
                        (
                            ProjectLogEngine::Generic,
                            TranslationFinished::NoWork {
                                summary: TranslationEngineSummary::Generic(_),
                                ..
                            } | TranslationFinished::Complete {
                                summary: TranslationEngineSummary::Generic(_),
                                ..
                            } | TranslationFinished::Incomplete {
                                summary: TranslationEngineSummary::Generic(_),
                                ..
                            } | TranslationFinished::Failed {
                                summary: None | Some(TranslationEngineSummary::Generic(_)),
                                ..
                            } | TranslationFinished::Cancelled {
                                summary: None | Some(TranslationEngineSummary::Generic(_)),
                                ..
                            },
                        ) | (
                            ProjectLogEngine::RpgMakerMv | ProjectLogEngine::RpgMakerMz,
                            TranslationFinished::NoWork {
                                summary: TranslationEngineSummary::RpgMaker(_),
                                ..
                            } | TranslationFinished::Complete {
                                summary: TranslationEngineSummary::RpgMaker(_),
                                ..
                            } | TranslationFinished::Incomplete {
                                summary: TranslationEngineSummary::RpgMaker(_),
                                ..
                            } | TranslationFinished::Failed {
                                summary: None | Some(TranslationEngineSummary::RpgMaker(_)),
                                ..
                            } | TranslationFinished::Cancelled {
                                summary: None | Some(TranslationEngineSummary::RpgMaker(_)),
                                ..
                            },
                        )
                    );
                    if !summary_matches {
                        return Err(EmitError::EngineSummaryMismatch);
                    }
                } else if !self.tasks_started.is_empty() {
                    return Err(EmitError::TaskSummaryMismatch);
                }
                if matches!(result, TranslationFinished::NotStarted)
                    && self
                        .occurrences
                        .values()
                        .any(|entry| entry.scope == DiagnosticScope::RunPlan)
                {
                    return Err(EmitError::DiagnosticRequiresFailedTranslation);
                }
                if let TranslationFinished::Failed {
                    tasks, diagnostic, ..
                } = result
                    && self.diagnostic_scope(*diagnostic) == Some(DiagnosticScope::RunPlan)
                    && (tasks.started != 0 || tasks.not_started != tasks.planned)
                {
                    return Err(EmitError::PlanningFailureStartedTasks);
                }
                if self.tasks_started.len() != self.task_outcomes.len() {
                    return Err(EmitError::UnfinishedTasks);
                }
                self.translation_finished = true;
            }
            ProjectLogEvent::PublicationStarted { .. } => {
                if self.publication_started {
                    return Err(EmitError::DuplicatePublication);
                }
                self.publication_started = true;
            }
            ProjectLogEvent::PublicationFinished { result } => {
                if !self.publication_started || self.publication_finished {
                    return Err(EmitError::InvalidPublicationTransition);
                }
                if let PublicationFinished::Published { summary } = result {
                    let summary_matches = matches!(
                        (context.engine(), summary),
                        (ProjectLogEngine::Generic, PublicationSummary::Generic(_))
                            | (
                                ProjectLogEngine::RpgMakerMv | ProjectLogEngine::RpgMakerMz,
                                PublicationSummary::RpgMaker(_)
                            )
                    );
                    if !summary_matches {
                        return Err(EmitError::EngineSummaryMismatch);
                    }
                }
                self.publication_finished = true;
            }
            ProjectLogEvent::RetrySummary {
                attempted,
                recovered,
                exhausted,
            } => {
                if recovered.checked_add(*exhausted) != Some(*attempted) {
                    return Err(EmitError::InvalidRetrySummary {
                        attempted: *attempted,
                        recovered: *recovered,
                        exhausted: *exhausted,
                    });
                }
            }
            ProjectLogEvent::LuaPrint { .. } => {}
        }
        Ok(EmitDisposition::Accepted)
    }

    fn validate_task_counters(&self, counters: TranslationTaskCounters) -> Result<(), EmitError> {
        let started =
            u64::try_from(self.tasks_started.len()).map_err(|_| EmitError::TaskCountDoesNotFit)?;
        if counters.started != started {
            return Err(EmitError::TaskSummaryMismatch);
        }
        let mut actual = [0_u64; 5];
        for outcome in self.task_outcomes.values() {
            let index = match outcome {
                TaskCounterKind::Complete => 0,
                TaskCounterKind::Partial => 1,
                TaskCounterKind::Unavailable => 2,
                TaskCounterKind::Failed => 3,
                TaskCounterKind::Cancelled => 4,
            };
            actual[index] = actual[index]
                .checked_add(1)
                .ok_or(EmitError::TaskCountDoesNotFit)?;
        }
        if actual
            != [
                counters.complete,
                counters.partial,
                counters.unavailable,
                counters.failed,
                counters.cancelled,
            ]
        {
            return Err(EmitError::TaskSummaryMismatch);
        }
        Ok(())
    }

    fn validate_normal_finish(&self, context: &ProjectLogContext) -> Result<(), FinishError> {
        if self
            .phases
            .values()
            .any(|state| *state == PhaseState::Started)
        {
            return Err(FinishError::ActivePhase);
        }
        if self.tasks_started.len() != self.task_outcomes.len() {
            return Err(FinishError::UnfinishedTasks);
        }
        if self.run_plan_resolved && !self.run_plan_finalized {
            return Err(FinishError::UnfinalizedRunPlan);
        }
        if context.command() == ProjectLogCommand::Translate && !self.translation_finished {
            return Err(FinishError::MissingTranslationFinished);
        }
        if self.publication_started && !self.publication_finished {
            return Err(FinishError::MissingPublicationFinished);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmitDisposition {
    Accepted,
    BestEffortDropped,
    DuplicateSuppressed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmitError {
    Closed,
    RuntimeManagedEvent {
        event: ProjectLogCode,
    },
    UnknownDiagnostic(DiagnosticOccurrenceId),
    InvalidDiagnosticScope(DiagnosticOccurrenceId),
    InvalidPhaseTransition(ProjectLogPhase),
    DuplicateRunPlan,
    InvalidRunPlanTransition,
    RunPlanCommandMismatch,
    InvalidRunPlanTransaction,
    DuplicateTask(u64),
    InvalidTaskTransition(u64),
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
}

impl fmt::Display for EmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("项目日志生产者已经关闭"),
            Self::RuntimeManagedEvent { event } => {
                write!(
                    formatter,
                    "事件 {} 只能由项目日志运行时建立",
                    event.as_str()
                )
            }
            Self::UnknownDiagnostic(id) => {
                write!(formatter, "事件引用了未知诊断 occurrence {}", id.get())
            }
            Self::InvalidDiagnosticScope(id) => {
                write!(
                    formatter,
                    "事件引用的诊断 occurrence {} 不属于该终态 scope",
                    id.get()
                )
            }
            Self::InvalidPhaseTransition(phase) => {
                write!(formatter, "阶段 {} 的状态转换无效", phase.as_str())
            }
            Self::DuplicateRunPlan => formatter.write_str("运行计划已经解析"),
            Self::InvalidRunPlanTransition => formatter.write_str("运行计划收尾状态转换无效"),
            Self::RunPlanCommandMismatch => formatter.write_str("运行计划类型与命令不一致"),
            Self::InvalidRunPlanTransaction => formatter.write_str("运行计划事务终态不一致"),
            Self::DuplicateTask(ordinal) => write!(formatter, "任务 {ordinal} 重复开始"),
            Self::InvalidTaskTransition(ordinal) => {
                write!(formatter, "任务 {ordinal} 的终态转换无效")
            }
            Self::InconsistentTaskTotal => formatter.write_str("任务事件的 total 不一致"),
            Self::DuplicateTranslationFinished => {
                formatter.write_str("translation.finished 只能出现一次")
            }
            Self::DuplicatePublication => formatter.write_str("发布已经开始"),
            Self::InvalidPublicationTransition => formatter.write_str("发布终态转换无效"),
            Self::UnfinishedTasks => formatter.write_str("仍有已开始但没有终态的任务"),
            Self::TaskSummaryMismatch => formatter.write_str("任务事件与翻译汇总不一致"),
            Self::TaskCountDoesNotFit => formatter.write_str("任务数量无法表示为 u64"),
            Self::EngineSummaryMismatch => formatter.write_str("汇总类型与项目引擎不一致"),
            Self::InvalidRetrySummary {
                attempted,
                recovered,
                exhausted,
            } => write!(
                formatter,
                "重试汇总不一致：attempted={attempted}, recovered={recovered}, exhausted={exhausted}"
            ),
            Self::InvalidCancellationCount { confirmed, total } => write!(
                formatter,
                "取消汇总 confirmed={confirmed} 大于 total={total}"
            ),
            Self::DiagnosticRequiresFailedTranslation => {
                formatter.write_str("已有运行计划诊断时不能使用无诊断引用的 NotStarted")
            }
            Self::PlanningFailureStartedTasks => {
                formatter.write_str("规划失败必须把全部计划任务计入 not_started")
            }
            Self::OccurrenceIdExhausted => formatter.write_str("诊断 occurrence ID 已耗尽"),
            Self::SequenceExhausted => formatter.write_str("项目日志 sequence 已耗尽"),
        }
    }
}

impl EmitError {
    pub(crate) fn diagnostic_report(self) -> DiagnosticReport {
        let violation = match self {
            Self::Closed => ObservabilityContractViolation::ProducerClosed,
            Self::RuntimeManagedEvent { event } => {
                ObservabilityContractViolation::RuntimeManagedEvent {
                    event: event.into(),
                }
            }
            Self::UnknownDiagnostic(id) => ObservabilityContractViolation::UnknownDiagnostic {
                occurrence_id: id.get(),
            },
            Self::InvalidDiagnosticScope(id) => {
                ObservabilityContractViolation::InvalidTerminalDiagnostic {
                    occurrence_id: id.get(),
                }
            }
            Self::InvalidPhaseTransition(phase) => {
                ObservabilityContractViolation::InvalidPhaseTransition {
                    phase: phase.into(),
                }
            }
            Self::DuplicateRunPlan => ObservabilityContractViolation::DuplicateRunPlan,
            Self::InvalidRunPlanTransition => {
                ObservabilityContractViolation::InvalidRunPlanTransition
            }
            Self::RunPlanCommandMismatch => ObservabilityContractViolation::RunPlanCommandMismatch,
            Self::InvalidRunPlanTransaction => {
                ObservabilityContractViolation::InvalidRunPlanTransaction
            }
            Self::DuplicateTask(ordinal) => {
                ObservabilityContractViolation::DuplicateTask { ordinal }
            }
            Self::InvalidTaskTransition(ordinal) => {
                ObservabilityContractViolation::InvalidTaskTransition { ordinal }
            }
            Self::InconsistentTaskTotal => ObservabilityContractViolation::InconsistentTaskTotal,
            Self::DuplicateTranslationFinished => {
                ObservabilityContractViolation::DuplicateTranslationFinished
            }
            Self::DuplicatePublication => ObservabilityContractViolation::DuplicatePublication,
            Self::InvalidPublicationTransition => {
                ObservabilityContractViolation::InvalidPublicationTransition
            }
            Self::UnfinishedTasks => ObservabilityContractViolation::UnfinishedTasks,
            Self::TaskSummaryMismatch => ObservabilityContractViolation::TaskSummaryMismatch,
            Self::TaskCountDoesNotFit => ObservabilityContractViolation::TaskCountDoesNotFit,
            Self::EngineSummaryMismatch => ObservabilityContractViolation::EngineSummaryMismatch,
            Self::InvalidRetrySummary {
                attempted,
                recovered,
                exhausted,
            } => ObservabilityContractViolation::InvalidRetrySummary {
                attempted,
                recovered,
                exhausted,
            },
            Self::InvalidCancellationCount { confirmed, total } => {
                ObservabilityContractViolation::InvalidCancellationCount { confirmed, total }
            }
            Self::DiagnosticRequiresFailedTranslation => {
                ObservabilityContractViolation::DiagnosticRequiresFailedTranslation
            }
            Self::PlanningFailureStartedTasks => {
                ObservabilityContractViolation::PlanningFailureStartedTasks
            }
            Self::OccurrenceIdExhausted => ObservabilityContractViolation::OccurrenceIdExhausted,
            Self::SequenceExhausted => ObservabilityContractViolation::SequenceExhausted,
        };
        project_log_contract_report(violation)
    }
}

impl std::error::Error for EmitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinishError {
    AlreadyFinished,
    ActivePhase,
    UnfinishedTasks,
    UnfinalizedRunPlan,
    MissingTranslationFinished,
    MissingPublicationFinished,
    UnknownDiagnostic(DiagnosticOccurrenceId),
    InvalidTerminalDiagnostic(DiagnosticOccurrenceId),
    ChannelClosed,
}

impl fmt::Display for FinishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFinished => formatter.write_str("项目日志已经收尾"),
            Self::ActivePhase => formatter.write_str("仍有 started 阶段没有 completed 或 stopped"),
            Self::UnfinishedTasks => formatter.write_str("仍有已开始任务没有 task.finished"),
            Self::UnfinalizedRunPlan => formatter.write_str("运行计划已解析但尚未收尾"),
            Self::MissingTranslationFinished => {
                formatter.write_str("Translate 运行缺少唯一 translation.finished")
            }
            Self::MissingPublicationFinished => {
                formatter.write_str("已开始的发布缺少 publication.finished")
            }
            Self::UnknownDiagnostic(id) => {
                write!(formatter, "运行终态引用了未知诊断 occurrence {}", id.get())
            }
            Self::InvalidTerminalDiagnostic(id) => {
                write!(formatter, "诊断 occurrence {} 不是待写终端诊断", id.get())
            }
            Self::ChannelClosed => formatter.write_str("项目日志 channel 已关闭"),
        }
    }
}

impl FinishError {
    pub(crate) fn diagnostic_report(self) -> DiagnosticReport {
        let violation = match self {
            Self::AlreadyFinished => ObservabilityContractViolation::AlreadyFinished,
            Self::ActivePhase => ObservabilityContractViolation::ActivePhase,
            Self::UnfinishedTasks => ObservabilityContractViolation::UnfinishedTasks,
            Self::UnfinalizedRunPlan => ObservabilityContractViolation::UnfinalizedRunPlan,
            Self::MissingTranslationFinished => {
                ObservabilityContractViolation::MissingTranslationFinished
            }
            Self::MissingPublicationFinished => {
                ObservabilityContractViolation::MissingPublicationFinished
            }
            Self::UnknownDiagnostic(id) => ObservabilityContractViolation::UnknownDiagnostic {
                occurrence_id: id.get(),
            },
            Self::InvalidTerminalDiagnostic(id) => {
                ObservabilityContractViolation::InvalidTerminalDiagnostic {
                    occurrence_id: id.get(),
                }
            }
            Self::ChannelClosed => ObservabilityContractViolation::ChannelClosed,
        };
        project_log_contract_report(violation)
    }
}

impl std::error::Error for FinishError {}

fn project_log_contract_report(violation: ObservabilityContractViolation) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::observability(ObservabilityIssue::contract(
            ObservabilityComponent::ProjectLog,
            violation,
        )),
    )
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedTerminalDiagnostic {
    occurrence: DiagnosticOccurrence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrepareTerminalDiagnosticError {
    Closed,
    OccurrenceIdExhausted,
}

impl fmt::Display for PrepareTerminalDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "项目日志生产者已经关闭",
            Self::OccurrenceIdExhausted => "诊断 occurrence ID 已耗尽",
        })
    }
}

impl PrepareTerminalDiagnosticError {
    pub(crate) fn diagnostic_report(self) -> DiagnosticReport {
        project_log_contract_report(match self {
            Self::Closed => ObservabilityContractViolation::ProducerClosed,
            Self::OccurrenceIdExhausted => ObservabilityContractViolation::OccurrenceIdExhausted,
        })
    }
}

impl std::error::Error for PrepareTerminalDiagnosticError {}

impl PreparedTerminalDiagnostic {
    pub(crate) const fn id(&self) -> DiagnosticOccurrenceId {
        self.occurrence.id()
    }
}

struct LoggerInner {
    context: ProjectLogContext,
    state: Mutex<ProducerState>,
    health: Arc<ProjectLogHealth>,
    permits: Arc<BestEffortPermits>,
    clock: Arc<dyn ProjectLogClock>,
    next_sequence: Arc<AtomicU64>,
    next_occurrence: AtomicU64,
}

impl LoggerInner {
    fn allocate_sequence(&self) -> Result<u64, EmitError> {
        self.next_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                self.health.record(ProjectLogFailureKey::SequenceExhausted);
                EmitError::SequenceExhausted
            })
    }

    fn allocate_occurrence(&self) -> Result<DiagnosticOccurrenceId, OccurrenceIdExhausted> {
        let id = self
            .next_occurrence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                self.health
                    .record(ProjectLogFailureKey::OccurrenceIdExhausted);
                OccurrenceIdExhausted
            })?;
        DiagnosticOccurrenceId::new(id)
    }

    fn enqueue_locked(
        &self,
        state: &ProducerState,
        event: ProjectLogEvent,
    ) -> Result<EmitDisposition, EmitError> {
        let code = event.code();
        let permit = match event.delivery() {
            ProjectLogDelivery::Required => PermitKind::Required,
            ProjectLogDelivery::BestEffort => {
                if !self.permits.try_acquire() {
                    self.health
                        .record(ProjectLogFailureKey::BestEffortBackpressure { code });
                    return Ok(EmitDisposition::BestEffortDropped);
                }
                PermitKind::BestEffort
            }
        };
        let sequence = match self.allocate_sequence() {
            Ok(value) => value,
            Err(error) => {
                if permit == PermitKind::BestEffort {
                    self.permits.release();
                }
                return Err(error);
            }
        };
        let queued = QueueItem::Event(QueuedEvent {
            emitted_at: self.clock.now(),
            sequence,
            event,
            permit,
        });
        let Some(sender) = &state.sender else {
            if permit == PermitKind::BestEffort {
                self.permits.release();
            }
            self.health
                .record(ProjectLogFailureKey::ChannelClosed { code: Some(code) });
            self.health
                .record(ProjectLogFailureKey::NotPersisted { code });
            return Err(EmitError::Closed);
        };
        if sender.try_send(queued).is_err() {
            if permit == PermitKind::BestEffort {
                self.permits.release();
            }
            self.health
                .record(ProjectLogFailureKey::ChannelClosed { code: Some(code) });
            self.health
                .record(ProjectLogFailureKey::NotPersisted { code });
            return Err(EmitError::Closed);
        }
        Ok(EmitDisposition::Accepted)
    }
}

#[derive(Clone)]
pub(crate) struct ProjectLogger {
    inner: Arc<LoggerInner>,
}

impl ProjectLogger {
    /// 交付方式由事件本身决定；不存在调用方选择的 reliable 入口。
    pub(crate) fn emit(&self, event: ProjectLogEvent) -> Result<EmitDisposition, EmitError> {
        let mut state = lock_unpoisoned(&self.inner.state);
        let disposition = state.validate_event(&event, &self.inner.context)?;
        if disposition == EmitDisposition::DuplicateSuppressed {
            return Ok(disposition);
        }
        self.inner.enqueue_locked(&state, event)
    }

    pub(crate) fn record_diagnostic(
        &self,
        scope: DiagnosticScope,
        report: DiagnosticReport,
    ) -> Result<DiagnosticOccurrenceId, EmitError> {
        let mut state = lock_unpoisoned(&self.inner.state);
        if state.finalized || state.terminal_preparing {
            return Err(EmitError::Closed);
        }
        let id = self
            .inner
            .allocate_occurrence()
            .map_err(|_| EmitError::OccurrenceIdExhausted)?;
        let occurrence = DiagnosticOccurrence { id, scope, report };
        state.occurrences.insert(
            id,
            OccurrenceRegistration {
                state: OccurrenceState::Emitted,
                scope,
            },
        );
        match self
            .inner
            .enqueue_locked(&state, ProjectLogEvent::Diagnostic { occurrence })
        {
            Ok(EmitDisposition::Accepted) => Ok(id),
            Ok(_) => unreachable!("诊断始终是必要事件"),
            Err(error) => {
                state.occurrences.remove(&id);
                Err(error)
            }
        }
    }

    /// 终端诊断先取得 ID，随后由 runtime 在 performance 之后、run.finished 之前写入。
    pub(crate) fn prepare_terminal_diagnostic(
        &self,
        scope: DiagnosticScope,
        report: DiagnosticReport,
    ) -> Result<PreparedTerminalDiagnostic, PrepareTerminalDiagnosticError> {
        let mut state = lock_unpoisoned(&self.inner.state);
        if state.finalized {
            return Err(PrepareTerminalDiagnosticError::Closed);
        }
        let id = self
            .inner
            .allocate_occurrence()
            .map_err(|_| PrepareTerminalDiagnosticError::OccurrenceIdExhausted)?;
        // 只有成功取得 occurrence ID 后才封闭普通事件。否则 Drop 无法收尾时，
        // 仍可显式关闭 sender，不会因一个半完成状态永久等待 writer。
        state.terminal_preparing = true;
        let occurrence = DiagnosticOccurrence { id, scope, report };
        state.occurrences.insert(
            id,
            OccurrenceRegistration {
                state: OccurrenceState::PreparedTerminal,
                scope,
            },
        );
        Ok(PreparedTerminalDiagnostic { occurrence })
    }

    pub(crate) fn health(&self) -> ProjectLogHealthSnapshot {
        self.inner.health.snapshot()
    }

    /// 终态必须以 logger 实际接受的 task.started/task.finished 为准，而不是由应用层
    /// 重算一份可能在诊断入队失败后漂移的计数。planned 仍由 Planner 提供；尚未开始的
    /// 任务不会产生 task.started，因此由这里补齐。
    pub(crate) fn translation_task_counters(
        &self,
        planned: u64,
    ) -> Result<TranslationTaskCounters, EmitError> {
        let state = lock_unpoisoned(&self.inner.state);
        if state.task_total.is_some_and(|total| total != planned) {
            return Err(EmitError::TaskSummaryMismatch);
        }
        let started =
            u64::try_from(state.tasks_started.len()).map_err(|_| EmitError::TaskCountDoesNotFit)?;
        let not_started = planned
            .checked_sub(started)
            .ok_or(EmitError::TaskSummaryMismatch)?;
        let mut actual = [0_u64; 5];
        for outcome in state.task_outcomes.values() {
            match outcome {
                TaskCounterKind::Complete => actual[0] += 1,
                TaskCounterKind::Partial => actual[1] += 1,
                TaskCounterKind::Unavailable => actual[2] += 1,
                TaskCounterKind::Failed => actual[3] += 1,
                TaskCounterKind::Cancelled => actual[4] += 1,
            }
        }
        TranslationTaskCounters::new(
            planned,
            started,
            actual[0],
            actual[1],
            actual[2],
            actual[3],
            actual[4],
            not_started,
        )
        .map_err(|_| EmitError::TaskSummaryMismatch)
    }

    /// 安装一个健康快照观察者。安装时立即推送当前快照，之后每个新故障键计数变化都会
    /// 推送；消费方用自己的 [`ProjectLogHealthCursor`] 去重并计算新增数量。
    pub(crate) fn install_health_observer(
        &self,
        observer: impl Fn(ProjectLogHealthSnapshot) + Send + Sync + 'static,
    ) {
        self.inner.health.install_observer(Arc::new(observer));
    }

    /// 停止健康快照通知。调用后已有观察者闭包立即释放；writer 的健康统计继续保留。
    pub(crate) fn clear_health_observer(&self) {
        self.inner.health.clear_observer();
    }
}

#[derive(Debug)]
pub(crate) struct ProjectLogStartError {
    source: io::Error,
}

impl fmt::Display for ProjectLogStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("无法启动项目日志 writer")
    }
}

impl std::error::Error for ProjectLogStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogShutdown {
    pub(crate) health: ProjectLogHealthSnapshot,
}

pub(crate) struct ProjectLogRuntime {
    logger: ProjectLogger,
    worker: Option<JoinHandle<()>>,
    performance: Arc<RunPerformanceCounters>,
    drop_report: Option<DiagnosticReport>,
    finished: bool,
}

impl ProjectLogRuntime {
    pub(crate) fn start_reserved_file(
        path: &Path,
        file: File,
        context: ProjectLogContext,
        run_id: RunId,
        performance: Arc<RunPerformanceCounters>,
        drop_report: DiagnosticReport,
    ) -> Result<Self, ReportedFailure> {
        let safe_path = SafePath::new(path);
        let sink = FileProjectLogSink::from_reserved(path, file);
        Self::start(context, run_id, sink, performance, drop_report).map_err(|source| {
            let report = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::worker_start(
                    ObservabilityComponent::ProjectLog,
                    &source.source,
                )),
            );
            let mut failure = ReportedFailure::new(report, source);
            if let Err(cleanup) = std::fs::remove_file(path) {
                let cleanup_report = DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::observability(ObservabilityIssue::cleanup(
                        ObservabilityComponent::ProjectLog,
                        safe_path,
                        &cleanup,
                    )),
                );
                failure = failure.with_related(
                    RelatedFailureRelation::Cleanup,
                    ReportedFailure::new(cleanup_report, cleanup),
                );
            }
            failure
        })
    }

    #[cfg(test)]
    pub(crate) fn start_file(
        path: &Path,
        context: ProjectLogContext,
        run_id: RunId,
        performance: Arc<RunPerformanceCounters>,
        drop_report: DiagnosticReport,
    ) -> Result<Self, ReportedFailure> {
        let safe_path = SafePath::new(path);
        let sink = FileProjectLogSink::create_new(path).map_err(|source| {
            let report = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::create(
                    ObservabilityComponent::ProjectLog,
                    safe_path.clone(),
                    &source,
                )),
            );
            ReportedFailure::new(report, source)
        })?;

        Self::start(context, run_id, sink, performance, drop_report).map_err(|source| {
            let report = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(ObservabilityIssue::worker_start(
                    ObservabilityComponent::ProjectLog,
                    &source.source,
                )),
            );
            let mut failure = ReportedFailure::new(report, source);
            if let Err(cleanup) = std::fs::remove_file(path) {
                let cleanup_report = DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::observability(ObservabilityIssue::cleanup(
                        ObservabilityComponent::ProjectLog,
                        safe_path,
                        &cleanup,
                    )),
                );
                failure = failure.with_related(
                    RelatedFailureRelation::Cleanup,
                    ReportedFailure::new(cleanup_report, cleanup),
                );
            }
            failure
        })
    }

    pub(crate) fn start<S: ProjectLogSink>(
        context: ProjectLogContext,
        run_id: RunId,
        sink: S,
        performance: Arc<RunPerformanceCounters>,
        drop_report: DiagnosticReport,
    ) -> Result<Self, ProjectLogStartError> {
        Self::start_with_components(
            context,
            run_id,
            Box::new(sink),
            Box::new(JsonProjectLogRecordEncoder),
            Arc::new(SystemProjectLogClock),
            performance,
            drop_report,
        )
    }

    fn start_with_components(
        context: ProjectLogContext,
        run_id: RunId,
        sink: Box<dyn ProjectLogSink>,
        encoder: Box<dyn ProjectLogRecordEncoder>,
        clock: Arc<dyn ProjectLogClock>,
        performance: Arc<RunPerformanceCounters>,
        drop_report: DiagnosticReport,
    ) -> Result<Self, ProjectLogStartError> {
        let (sender, receiver) = async_channel::unbounded();
        let health = Arc::new(ProjectLogHealth::default());
        let permits = Arc::new(BestEffortPermits::new());
        let inner = Arc::new(LoggerInner {
            context: context.clone(),
            state: Mutex::new(ProducerState::new(sender)),
            health: Arc::clone(&health),
            permits: Arc::clone(&permits),
            clock: Arc::clone(&clock),
            next_sequence: Arc::new(AtomicU64::new(1)),
            next_occurrence: AtomicU64::new(1),
        });
        let worker_context = context;
        let worker_run_id = run_id.to_string();
        let worker_health = Arc::clone(&health);
        let worker_clock = clock;
        let worker_sequence = Arc::clone(&inner.next_sequence);
        let worker = thread::Builder::new()
            .name("att-project-log-writer".to_owned())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| {
                    writer_loop(
                        &receiver,
                        worker_context,
                        worker_run_id,
                        sink,
                        encoder,
                        worker_health.as_ref(),
                        permits.as_ref(),
                        worker_clock.as_ref(),
                        worker_sequence.as_ref(),
                    );
                }))
                .is_err()
                {
                    // catch_unwind 之外的 panic 也不能让已接收事件、尤其是
                    // BestEffort permit，悄悄遗失。receiver 仍由此线程持有，
                    // 因而可以把队列完整排空为明确的未持久化证据。
                    receiver.close();
                    discard_pending_queue(&receiver, worker_health.as_ref(), permits.as_ref());
                    worker_health.record(ProjectLogFailureKey::WorkerPanicked);
                }
            })
            .map_err(|source| ProjectLogStartError { source })?;
        let logger = ProjectLogger { inner };
        let initial_event = {
            let state = lock_unpoisoned(&logger.inner.state);
            // run.started 是运行时建立的首条必要事件。
            logger
                .inner
                .enqueue_locked(&state, ProjectLogEvent::RunStarted)
        };
        if initial_event.is_err() {
            let sender = lock_unpoisoned(&logger.inner.state).sender.take();
            if let Some(sender) = sender {
                sender.close();
            }
            let _ = worker.join();
            return Err(ProjectLogStartError {
                source: io::Error::new(io::ErrorKind::BrokenPipe, "project log channel closed"),
            });
        }
        Ok(Self {
            logger,
            worker: Some(worker),
            performance,
            drop_report: Some(drop_report),
            finished: false,
        })
    }

    pub(crate) fn logger(&self) -> ProjectLogger {
        self.logger.clone()
    }

    pub(crate) fn replace_drop_report(&mut self, report: DiagnosticReport) {
        if !self.finished {
            self.drop_report = Some(report);
        }
    }

    pub(crate) fn finish(
        mut self,
        result: RunFinished,
        terminal_diagnostics: Vec<PreparedTerminalDiagnostic>,
    ) -> Result<ProjectLogShutdown, FinishError> {
        match self.finalize(result, terminal_diagnostics, false) {
            Ok(()) => {
                self.finished = true;
                self.drop_report = None;
                self.join_worker()
            }
            Err(error) => {
                // `finish` 已经明确得到了真实的日志合同错误。随后 Drop 只能以这份
                // 错误建立 outcome_unknown，不能误报调用方原先预登记的命令 panic。
                self.drop_report = Some(
                    error
                        .diagnostic_report()
                        .with_effect(StateEffect::OutcomeUnknown),
                );
                Err(error)
            }
        }
    }

    fn finalize(
        &mut self,
        result: RunFinished,
        terminal_diagnostics: Vec<PreparedTerminalDiagnostic>,
        force: bool,
    ) -> Result<(), FinishError> {
        let mut state = lock_unpoisoned(&self.logger.inner.state);
        if state.finalized {
            return Err(FinishError::AlreadyFinished);
        }
        if !force {
            state.validate_normal_finish(&self.logger.inner.context)?;
        }
        let mut terminal_diagnostics = terminal_diagnostics
            .into_iter()
            .map(|prepared| {
                let id = prepared.id();
                match state.occurrences.get(&id) {
                    Some(OccurrenceRegistration {
                        state: OccurrenceState::PreparedTerminal,
                        ..
                    }) => Ok(prepared.occurrence),
                    _ => Err(FinishError::InvalidTerminalDiagnostic(id)),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        terminal_diagnostics.sort_by_key(DiagnosticOccurrence::id);
        let mut terminal_ids = BTreeSet::new();
        for occurrence in &terminal_diagnostics {
            if !terminal_ids.insert(occurrence.id()) {
                return Err(FinishError::InvalidTerminalDiagnostic(occurrence.id()));
            }
        }
        if let Some(diagnostic) = result.diagnostic() {
            match state.occurrences.get(&diagnostic) {
                Some(OccurrenceRegistration {
                    state: OccurrenceState::Emitted,
                    ..
                }) => {}
                Some(OccurrenceRegistration {
                    state: OccurrenceState::PreparedTerminal,
                    ..
                }) if terminal_ids.contains(&diagnostic) => {}
                _ => return Err(FinishError::UnknownDiagnostic(diagnostic)),
            }
        }
        state.finalized = true;
        let Some(sender) = state.sender.take() else {
            return Err(FinishError::ChannelClosed);
        };
        match sender.try_send(QueueItem::Finalize(FinalizeRequest {
            terminal_diagnostics,
            performance: self.performance.snapshot(),
            result,
        })) {
            Ok(()) => {}
            Err(error) => {
                let QueueItem::Finalize(request) = error.into_inner() else {
                    unreachable!("收尾发送只能提交 Finalize")
                };
                self.logger
                    .inner
                    .health
                    .record(ProjectLogFailureKey::ChannelClosed {
                        code: Some(ProjectLogCode::RunFinished),
                    });
                record_unpersisted_finalize(request, self.logger.inner.health.as_ref());
                return Err(FinishError::ChannelClosed);
            }
        }
        sender.close();
        Ok(())
    }

    fn join_worker(&mut self) -> Result<ProjectLogShutdown, FinishError> {
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            self.logger
                .inner
                .health
                .record(ProjectLogFailureKey::WorkerPanicked);
        }
        Ok(ProjectLogShutdown {
            health: self.logger.inner.health.snapshot(),
        })
    }

    /// 不再等待 writer 时必须先释放最后一个 sender；否则 Drop 中 join 会永久等待
    /// 一个仍认为生产者存活的 channel。
    fn close_producer(&self) {
        let sender = lock_unpoisoned(&self.logger.inner.state).sender.take();
        if let Some(sender) = sender {
            sender.close();
        }
    }
}

impl Drop for ProjectLogRuntime {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(report) = self.drop_report.take()
            && let Ok(prepared) = self
                .logger
                .prepare_terminal_diagnostic(DiagnosticScope::Run, report)
        {
            let result = RunFinished::OutcomeUnknown {
                diagnostic: prepared.id(),
            };
            let _ = self.finalize(result, vec![prepared], true);
        }
        // prepare_terminal_diagnostic 失败（例如 occurrence ID 耗尽）时不会进入
        // finalize；显式关闭生产者，保证下方 join 不会因为 self.logger 持有 sender
        // 而死锁。
        self.close_producer();
        let _ = self.join_worker();
        self.finished = true;
    }
}

struct WriterState {
    context: ProjectLogContext,
    localizer: UiLocalizer,
    run_id: String,
    sink: Box<dyn ProjectLogSink>,
    encoder: Box<dyn ProjectLogRecordEncoder>,
    write_disabled: bool,
}

/// persist 的控制结果只表达 writer 生命周期：序列化失败不使 sink 不可用，首个 I/O
/// write 失败则停止后续编码并先排空当时已接收的 FIFO；panic 需要立即结束 worker。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistDisposition {
    Continue,
    WriteDisabled,
    Panicked,
}

impl WriterState {
    fn persist(
        &mut self,
        emitted_at: OffsetDateTime,
        sequence: u64,
        event: ProjectLogEvent,
        health: &ProjectLogHealth,
    ) -> PersistDisposition {
        let code = event.code();
        match catch_unwind(AssertUnwindSafe(|| {
            self.persist_without_panic(emitted_at, sequence, event, health)
        })) {
            Ok(disposition) => disposition,
            Err(_) => {
                health.record(ProjectLogFailureKey::NotPersisted { code });
                PersistDisposition::Panicked
            }
        }
    }

    fn persist_without_panic(
        &mut self,
        emitted_at: OffsetDateTime,
        sequence: u64,
        event: ProjectLogEvent,
        health: &ProjectLogHealth,
    ) -> PersistDisposition {
        let code = event.code();
        if self.write_disabled {
            health.record(ProjectLogFailureKey::NotPersisted { code });
            return PersistDisposition::Continue;
        }
        let record = match ProjectLogRecord::new(
            emitted_at,
            sequence,
            &self.run_id,
            &self.context,
            &self.localizer,
            event,
        ) {
            Ok(record) => record,
            Err(_) => {
                health.record(ProjectLogFailureKey::Serialize {
                    path: self.sink.path().cloned(),
                    code,
                });
                health.record(ProjectLogFailureKey::NotPersisted { code });
                return PersistDisposition::Continue;
            }
        };
        let bytes = match self.encoder.encode(&record) {
            Ok(bytes) => bytes,
            Err(_) => {
                health.record(ProjectLogFailureKey::Serialize {
                    path: self.sink.path().cloned(),
                    code,
                });
                health.record(ProjectLogFailureKey::NotPersisted { code });
                return PersistDisposition::Continue;
            }
        };
        if let Err(error) = self.sink.write_record(&bytes) {
            health.record(ProjectLogFailureKey::Write {
                path: self.sink.path().cloned(),
                code,
                io_kind: error.kind().into(),
                raw_os_code: error.raw_os_error(),
            });
            health.record(ProjectLogFailureKey::NotPersisted { code });
            // 后续事件只计数，不再浪费 CPU 拼接确定无法写入的 JSON。
            self.write_disabled = true;
            return PersistDisposition::WriteDisabled;
        }
        PersistDisposition::Continue
    }

    fn flush_and_sync(&mut self, health: &ProjectLogHealth) -> bool {
        match catch_unwind(AssertUnwindSafe(|| {
            self.flush_and_sync_without_panic(health)
        })) {
            Ok(()) => true,
            Err(_) => {
                health.record(ProjectLogFailureKey::WorkerPanicked);
                false
            }
        }
    }

    fn flush_and_sync_without_panic(&mut self, health: &ProjectLogHealth) {
        if let Err(error) = self.sink.flush() {
            health.record(ProjectLogFailureKey::Flush {
                path: self.sink.path().cloned(),
                io_kind: error.kind().into(),
                raw_os_code: error.raw_os_error(),
            });
        }
        if let Err(error) = self.sink.sync() {
            health.record(ProjectLogFailureKey::Sync {
                path: self.sink.path().cloned(),
                io_kind: error.kind().into(),
                raw_os_code: error.raw_os_error(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn writer_loop(
    receiver: &Receiver<QueueItem>,
    context: ProjectLogContext,
    run_id: String,
    sink: Box<dyn ProjectLogSink>,
    encoder: Box<dyn ProjectLogRecordEncoder>,
    health: &ProjectLogHealth,
    permits: &BestEffortPermits,
    clock: &dyn ProjectLogClock,
    next_sequence: &AtomicU64,
) {
    let localizer = UiLocalizer::new(context.locale().ui_locale());
    let mut writer = WriterState {
        context,
        localizer,
        run_id,
        sink,
        encoder,
        write_disabled: false,
    };
    while let Ok(item) = receiver.recv_blocking() {
        match item {
            QueueItem::Event(queued) => {
                let disposition =
                    writer.persist(queued.emitted_at, queued.sequence, queued.event, health);
                if queued.permit == PermitKind::BestEffort {
                    permits.release();
                }
                match disposition {
                    PersistDisposition::Continue => {}
                    PersistDisposition::WriteDisabled => {
                        // 首个 write 失败后立刻释放当时 FIFO 中所有已接收事件，后续
                        // recv 只会走 write_disabled 分支计数，绝不再编码 JSON。
                        discard_pending_queue(receiver, health, permits);
                    }
                    PersistDisposition::Panicked => {
                        // persist 已在边界内捕获 panic；先关闭 receiver 阻止新的
                        // BestEffort 事件在排空与 worker 退出之间进入 FIFO，随后归还
                        // 所有已接收 permit，最后才公布 worker panic。
                        receiver.close();
                        discard_pending_queue(receiver, health, permits);
                        health.record(ProjectLogFailureKey::WorkerPanicked);
                        return;
                    }
                }
            }
            QueueItem::Finalize(request) => {
                let terminal_written =
                    write_terminal_sequence(&mut writer, request, health, clock, next_sequence);
                if terminal_written {
                    let _ = writer.flush_and_sync(health);
                }
                // Finalize 在同一 sender 临界区内成为最后一项；忽略其后的数据会暴露 bug，
                // 因此继续排空并把意外事件计为未持久化。
                discard_pending_queue(receiver, health, permits);
                return;
            }
        }
    }
    // channel 非正常关闭时仍尽力保留之前写入的证据，但不能伪造 run.finished。
    health.record(ProjectLogFailureKey::ChannelClosed { code: None });
    let _ = writer.flush_and_sync(health);
}

fn discard_pending_queue(
    receiver: &Receiver<QueueItem>,
    health: &ProjectLogHealth,
    permits: &BestEffortPermits,
) {
    while let Ok(item) = receiver.try_recv() {
        match item {
            QueueItem::Event(queued) => {
                health.record(ProjectLogFailureKey::NotPersisted {
                    code: queued.event.code(),
                });
                if queued.permit == PermitKind::BestEffort {
                    permits.release();
                }
            }
            QueueItem::Finalize(request) => record_unpersisted_finalize(request, health),
        }
    }
}

fn allocate_terminal_sequence(next_sequence: &AtomicU64, health: &ProjectLogHealth) -> Option<u64> {
    next_sequence
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| health.record(ProjectLogFailureKey::SequenceExhausted))
        .ok()
}

fn persist_terminal_event(
    writer: &mut WriterState,
    event: ProjectLogEvent,
    health: &ProjectLogHealth,
    clock: &dyn ProjectLogClock,
    next_sequence: &AtomicU64,
) -> bool {
    let code = event.code();
    let Some(sequence) = allocate_terminal_sequence(next_sequence, health) else {
        health.record(ProjectLogFailureKey::NotPersisted { code });
        return true;
    };
    writer.persist(clock.now(), sequence, event, health) != PersistDisposition::Panicked
}

fn record_unpersisted_finalize(request: FinalizeRequest, health: &ProjectLogHealth) {
    // writer 已经不可用时，这些原本由收尾序列生成的 Required 事件都必须留下
    // NotPersisted 计数。WorkerPanicked 已在调用方先记录，因此 degraded 也属于
    // 已无法持久化的终态证据。
    health.record(ProjectLogFailureKey::NotPersisted {
        code: ProjectLogCode::ProjectLogDegraded,
    });
    health.record(ProjectLogFailureKey::NotPersisted {
        code: ProjectLogCode::PerformanceCounters,
    });
    for occurrence in request.terminal_diagnostics {
        health.record(ProjectLogFailureKey::NotPersisted {
            code: occurrence.scope().code(),
        });
    }
    health.record(ProjectLogFailureKey::NotPersisted {
        code: ProjectLogCode::RunFinished,
    });
}

fn write_terminal_sequence(
    writer: &mut WriterState,
    request: FinalizeRequest,
    health: &ProjectLogHealth,
    clock: &dyn ProjectLogClock,
    next_sequence: &AtomicU64,
) -> bool {
    // 生产者已关闭且 FIFO 已排空到 Finalize，此时健康快照包含全部普通事件故障。
    let degraded = health.snapshot();
    let mut events = Vec::with_capacity(request.terminal_diagnostics.len() + 3);
    if !degraded.is_healthy() {
        events.push(ProjectLogEvent::ProjectLogDegraded { health: degraded });
    }
    events.push(ProjectLogEvent::PerformanceCounters {
        snapshot: request.performance,
    });
    for occurrence in request.terminal_diagnostics {
        events.push(ProjectLogEvent::Diagnostic { occurrence });
    }
    // run.finished 是唯一且最后一条业务事件。
    events.push(ProjectLogEvent::RunFinished {
        result: request.result,
    });
    let mut events = events.into_iter();
    while let Some(event) = events.next() {
        if !persist_terminal_event(writer, event, health, clock, next_sequence) {
            for remaining in events {
                health.record(ProjectLogFailureKey::NotPersisted {
                    code: remaining.code(),
                });
            }
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::diagnostic::{
        ByteRange, Diagnostic, GenericUnitLocator, PlaceholderIssue, PlaceholderRuleSource,
        RuntimeCommand, RuntimeEngine, RuntimeIssue, TranslationIssue,
    };

    #[derive(Clone, Default)]
    struct SharedBytes(Arc<Mutex<Vec<u8>>>);

    impl SharedBytes {
        fn records(&self) -> Vec<serde_json::Value> {
            let bytes = lock_unpoisoned(&self.0).clone();
            String::from_utf8(bytes)
                .expect("日志必须是 UTF-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("每行必须是 JSON"))
                .collect()
        }
    }

    impl ProjectLogSink for SharedBytes {
        fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
            lock_unpoisoned(&self.0).extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FixedClock;

    impl ProjectLogClock for FixedClock {
        fn now(&self) -> OffsetDateTime {
            OffsetDateTime::UNIX_EPOCH
        }
    }

    fn run_id() -> RunId {
        RunId::for_test(1)
    }

    fn context(command: ProjectLogCommand) -> ProjectLogContext {
        ProjectLogContext::new(
            UiLocale::English,
            ProjectLogEngine::Generic,
            "project-a",
            command,
        )
        .expect("测试 context 必须有效")
    }

    fn diagnostic_report(effect: StateEffect) -> DiagnosticReport {
        DiagnosticReport::new(
            effect,
            Diagnostic::translation(TranslationIssue::Placeholder {
                rule_source: PlaceholderRuleSource::ProjectSnapshot,
                unit: GenericUnitLocator::new("dialogue/a.jsonl", "group-3", "unit-7", None),
                problem: PlaceholderIssue::MissingTextCapture {
                    rule_number: 2,
                    match_range: ByteRange::new(4, 12).expect("测试范围必须有效"),
                },
            }),
        )
    }

    fn command_panic_drop_report() -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::OutcomeUnknown,
            Diagnostic::runtime(RuntimeIssue::CommandPanicked {
                engine: RuntimeEngine::Generic,
                command: RuntimeCommand::Extract,
                project_workspace: SafePath::new("project-a"),
                log_path: Some(SafePath::new("project-a/logs/run.jsonl")),
            }),
        )
    }

    fn runtime_with_components(
        command: ProjectLogCommand,
        sink: Box<dyn ProjectLogSink>,
        encoder: Box<dyn ProjectLogRecordEncoder>,
    ) -> ProjectLogRuntime {
        ProjectLogRuntime::start_with_components(
            context(command),
            run_id(),
            sink,
            encoder,
            Arc::new(FixedClock),
            Arc::new(RunPerformanceCounters::default()),
            diagnostic_report(StateEffect::OutcomeUnknown),
        )
        .expect("测试 runtime 必须启动")
    }

    #[test]
    fn record_wire_has_exact_top_level_and_derives_redundant_fields() {
        let context = context(ProjectLogCommand::Extract);
        let localizer = UiLocalizer::new(context.locale().ui_locale());
        let record = ProjectLogRecord::new(
            OffsetDateTime::UNIX_EPOCH,
            1,
            &run_id().to_string(),
            &context,
            &localizer,
            ProjectLogEvent::RunStarted,
        )
        .expect("测试记录必须可建");
        let text = serde_json::to_string(&record).expect("测试记录必须可序列化");
        assert_eq!(
            text,
            concat!(
                r#"{"timestamp":"1970-01-01T00:00:00Z","sequence":1,"run_id":"run-000001","level":"info","event":"run.started","context":{"locale":"en","engine":"generic","project":"project-a","command":"extract"},"payload":{},"message":"Command "#,
                "\u{2068}extract\u{2069}",
                r#" started."}"#,
            ),
            "封闭 wire 不得改变既有顶层、字段顺序、payload 或 message"
        );
        let value: serde_json::Value = serde_json::from_str(&text).expect("测试 JSON 必须有效");
        assert_eq!(
            value
                .as_object()
                .expect("顶层必须是对象")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "timestamp",
                "sequence",
                "run_id",
                "level",
                "event",
                "context",
                "payload",
                "message",
            ]
        );
        assert_eq!(value["payload"], serde_json::json!({}));
    }

    #[test]
    fn diagnostic_payload_contains_only_readable_guidance() {
        let occurrence = DiagnosticOccurrence {
            id: DiagnosticOccurrenceId::new(1).expect("非零 ID"),
            scope: DiagnosticScope::RunPlan,
            report: diagnostic_report(StateEffect::Unchanged),
        };
        let context = context(ProjectLogCommand::Translate);
        let localizer = UiLocalizer::new(context.locale().ui_locale());
        let record = ProjectLogRecord::new(
            OffsetDateTime::UNIX_EPOCH,
            1,
            &run_id().to_string(),
            &context,
            &localizer,
            ProjectLogEvent::Diagnostic { occurrence },
        )
        .expect("诊断记录必须可建");
        let value = serde_json::to_value(&record).expect("诊断记录必须可序列化");
        assert_eq!(value["event"], "diagnostic.run_plan");
        let payload = value["payload"]
            .as_object()
            .expect("诊断 payload 必须是对象");
        assert_eq!(
            payload.keys().map(String::as_str).collect::<Vec<_>>(),
            ["object", "reason", "help"]
        );
        for field in ["object", "reason", "help"] {
            assert!(
                payload[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "诊断 {field} 必须是非空可读文本"
            );
        }
        let serialized = serde_json::to_string(&value).expect("诊断记录必须可序列化");
        for forbidden in [
            "occurrence",
            "report",
            "effect",
            "stage",
            "issue",
            "resolution",
            "expected_fingerprint",
            "actual_fingerprint",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn typed_result_controls_level_and_run_plan_diagnostic_is_always_error() {
        let diagnostic = DiagnosticOccurrenceId::new(1).expect("非零 ID");
        assert_eq!(
            ProjectLogEvent::TaskFinished {
                task: TaskPosition::new(1, 1).expect("任务位置必须有效"),
                attempts: 1,
                outcome: TaskFinishedOutcome::Failed { diagnostic },
            }
            .level(),
            ProjectLogLevel::Error
        );
        assert_eq!(
            ProjectLogEvent::TaskFinished {
                task: TaskPosition::new(1, 1).expect("任务位置必须有效"),
                attempts: 1,
                outcome: TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic },
            }
            .level(),
            ProjectLogLevel::Error
        );
        assert_eq!(
            ProjectLogEvent::TranslationFinished {
                result: TranslationFinished::Cancelled {
                    tasks: TranslationTaskCounters::new(1, 1, 0, 0, 0, 0, 1, 0)
                        .expect("任务计数必须有效"),
                    summary: None,
                },
            }
            .level(),
            ProjectLogLevel::Warn
        );
        assert_eq!(
            ProjectLogEvent::RunPlanFinalized {
                database: SafePath::new("D:/project.sqlite3"),
                result: RunPlanFinalization::OutcomeUnknown {
                    transaction: RunPlanTransactionState::OutcomeUnknown,
                    run_continues: false,
                    diagnostic,
                },
            }
            .level(),
            ProjectLogLevel::Error
        );
        let occurrence = DiagnosticOccurrence {
            id: diagnostic,
            scope: DiagnosticScope::RunPlan,
            report: diagnostic_report(StateEffect::Unchanged),
        };
        assert_eq!(
            ProjectLogEvent::Diagnostic { occurrence }.level(),
            ProjectLogLevel::Error
        );
    }

    #[test]
    fn task_counters_reject_both_broken_equations() {
        assert_eq!(
            TranslationTaskCounters::new(2, 1, 0, 0, 0, 0, 0, 1),
            Err(TaskCounterInvariantError::StartedBreakdown)
        );
        assert_eq!(
            TranslationTaskCounters::new(3, 1, 1, 0, 0, 0, 0, 1),
            Err(TaskCounterInvariantError::PlannedBreakdown)
        );
        assert!(TranslationTaskCounters::new(2, 1, 0, 1, 0, 0, 0, 1).is_ok());
    }

    #[test]
    fn health_cursor_returns_only_new_counts() {
        let channel = ProjectLogFailureKey::ChannelClosed { code: None };
        let serialize = ProjectLogFailureKey::Serialize {
            path: None,
            code: ProjectLogCode::TaskFinished,
        };
        let mut cursor = ProjectLogHealthCursor::default();
        let first = ProjectLogHealthSnapshot {
            failures: vec![
                ProjectLogFailureCount {
                    failure: channel.clone(),
                    count: ObservabilityFailureCount::exact(2),
                },
                ProjectLogFailureCount {
                    failure: serialize.clone(),
                    count: ObservabilityFailureCount::exact(1),
                },
            ],
        };
        assert_eq!(
            cursor.consume(&first),
            vec![
                ProjectLogFailureCount {
                    failure: channel.clone(),
                    count: ObservabilityFailureCount::exact(2),
                },
                ProjectLogFailureCount {
                    failure: serialize.clone(),
                    count: ObservabilityFailureCount::exact(1),
                },
            ]
        );
        assert!(cursor.consume(&first).is_empty());
        let second = ProjectLogHealthSnapshot {
            failures: vec![
                ProjectLogFailureCount {
                    failure: channel.clone(),
                    count: ObservabilityFailureCount::exact(5),
                },
                ProjectLogFailureCount {
                    failure: serialize,
                    count: ObservabilityFailureCount::exact(1),
                },
                ProjectLogFailureCount {
                    failure: ProjectLogFailureKey::WorkerPanicked,
                    count: ObservabilityFailureCount::exact(1),
                },
            ],
        };
        assert_eq!(
            cursor.consume(&second),
            vec![
                ProjectLogFailureCount {
                    failure: channel,
                    count: ObservabilityFailureCount::exact(3),
                },
                ProjectLogFailureCount {
                    failure: ProjectLogFailureKey::WorkerPanicked,
                    count: ObservabilityFailureCount::exact(1),
                },
            ]
        );
    }

    #[test]
    fn health_cursor_preserves_the_first_overflow_as_an_at_least_delta() {
        let failure = ProjectLogFailureKey::WorkerPanicked;
        let mut cursor = ProjectLogHealthCursor::default();
        let exact = ProjectLogHealthSnapshot {
            failures: vec![ProjectLogFailureCount {
                failure: failure.clone(),
                count: ObservabilityFailureCount::exact(u64::MAX),
            }],
        };
        assert_eq!(
            cursor.consume(&exact),
            vec![ProjectLogFailureCount {
                failure: failure.clone(),
                count: ObservabilityFailureCount::exact(u64::MAX),
            }]
        );
        let overflowed = ProjectLogHealthSnapshot {
            failures: vec![ProjectLogFailureCount {
                failure: failure.clone(),
                count: ObservabilityFailureCount::AtLeast { minimum: u64::MAX },
            }],
        };
        assert_eq!(
            cursor.consume(&overflowed),
            vec![ProjectLogFailureCount {
                failure,
                count: ObservabilityFailureCount::AtLeast { minimum: 1 },
            }]
        );
        assert!(cursor.consume(&overflowed).is_empty());
    }

    #[test]
    fn health_observer_is_not_notified_for_repeated_failure_key() {
        let health = ProjectLogHealth::default();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let callback_values = Arc::clone(&observed);
        health.install_observer(Arc::new(move |snapshot| {
            lock_unpoisoned(&callback_values).push(snapshot);
        }));

        health.record(ProjectLogFailureKey::BestEffortBackpressure {
            code: ProjectLogCode::PhaseStarted,
        });
        health.record(ProjectLogFailureKey::BestEffortBackpressure {
            code: ProjectLogCode::PhaseStarted,
        });
        health.record(ProjectLogFailureKey::WorkerPanicked);

        let snapshots = lock_unpoisoned(&observed);
        assert_eq!(snapshots.len(), 3, "安装、首种故障、第二种故障应各通知一次");
        assert_eq!(
            snapshots.last().expect("必须有最终快照").count(
                &ProjectLogFailureKey::BestEffortBackpressure {
                    code: ProjectLogCode::PhaseStarted,
                }
            ),
            2
        );
    }

    #[test]
    fn health_counter_marks_overflow_as_at_least_without_panicking() {
        let health = ProjectLogHealth::default();
        let failure = ProjectLogFailureKey::WorkerPanicked;
        lock_unpoisoned(&health.counts)
            .insert(failure.clone(), ObservabilityFailureCount::exact(u64::MAX));

        health.record(failure.clone());

        let snapshot = health.snapshot();
        let entry = snapshot
            .failures
            .iter()
            .find(|entry| entry.failure == failure)
            .expect("溢出的故障键必须保留");
        assert_eq!(
            entry.count,
            ObservabilityFailureCount::AtLeast { minimum: u64::MAX }
        );
        let wire = serde_json::to_value(&snapshot).expect("健康快照必须可序列化");
        assert_eq!(
            wire["failures"][0]["count"],
            serde_json::json!({ "kind": "at_least", "minimum": u64::MAX })
        );
    }

    #[test]
    fn file_sink_creation_failure_returns_safe_diagnostic_report() {
        let temporary = tempfile::tempdir().expect("应可建立日志测试目录");
        let path = temporary.path().join("existing.jsonl");
        std::fs::write(&path, b"existing").expect("应可建立占位文件");

        let failure = match ProjectLogRuntime::start_file(
            &path,
            context(ProjectLogCommand::Extract),
            run_id(),
            Arc::new(RunPerformanceCounters::default()),
            diagnostic_report(StateEffect::OutcomeUnknown),
        ) {
            Ok(_) => panic!("create-new 必须拒绝既有日志文件"),
            Err(failure) => failure,
        };

        assert_eq!(
            failure.report().primary().code(),
            "observability.project_log.create"
        );
        assert_eq!(failure.report().effect(), StateEffect::Unchanged);
        let wire = serde_json::to_value(failure.report()).expect("诊断必须可序列化");
        assert_eq!(wire["primary"]["stage"], serde_json::json!("logging"));
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["operation"],
            serde_json::json!("create")
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["path"],
            serde_json::json!(SafePath::new(&path).as_str())
        );
        assert_eq!(std::fs::read(&path).expect("既有文件必须保留"), b"existing");
    }

    #[test]
    fn typed_event_messages_render_in_all_supported_locales() {
        let locales = [
            UiLocale::Arabic,
            UiLocale::SimplifiedChinese,
            UiLocale::TraditionalChinese,
            UiLocale::English,
            UiLocale::French,
            UiLocale::Russian,
            UiLocale::Spanish,
            UiLocale::Japanese,
            UiLocale::Korean,
            UiLocale::Vietnamese,
        ];
        let diagnostic = DiagnosticOccurrenceId::new(1).expect("非零 ID");
        for locale in locales {
            let context = ProjectLogContext::new(
                locale,
                ProjectLogEngine::Generic,
                "project-a",
                ProjectLogCommand::Translate,
            )
            .expect("测试 context 必须有效");
            let localizer = UiLocalizer::new(context.locale().ui_locale());
            let events = [
                ProjectLogEvent::CancellationRequested {
                    confirmed: 2,
                    total: Some(3),
                },
                ProjectLogEvent::PhaseCompleted {
                    phase: ProjectLogPhase::Planning,
                    amount: ProjectLogAmount::Indeterminate,
                },
                ProjectLogEvent::PhaseStopped {
                    phase: ProjectLogPhase::Planning,
                    outcome: PhaseStopOutcome::Cancelled,
                },
                ProjectLogEvent::RunPlanFinalized {
                    database: SafePath::new("project.sqlite3"),
                    result: RunPlanFinalization::NotSaved {
                        transaction: RunPlanTransactionState::RolledBack,
                        run_continues: false,
                        diagnostic,
                    },
                },
                ProjectLogEvent::TaskFinished {
                    task: TaskPosition::new(1, 1).expect("任务位置必须有效"),
                    attempts: 1,
                    outcome: TaskFinishedOutcome::Complete,
                },
                ProjectLogEvent::TaskFinished {
                    task: TaskPosition::new(1, 1).expect("任务位置必须有效"),
                    attempts: 1,
                    outcome: TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic },
                },
                ProjectLogEvent::TranslationFinished {
                    result: TranslationFinished::NotStarted,
                },
                ProjectLogEvent::publication_started("output"),
                ProjectLogEvent::PublicationFinished {
                    result: PublicationFinished::NotPublished { diagnostic },
                },
                ProjectLogEvent::ProjectLogDegraded {
                    health: ProjectLogHealthSnapshot {
                        failures: vec![ProjectLogFailureCount {
                            failure: ProjectLogFailureKey::WorkerPanicked,
                            count: ObservabilityFailureCount::exact(1),
                        }],
                    },
                },
                ProjectLogEvent::RunFinished {
                    result: RunFinished::RecoveryRequired { diagnostic },
                },
            ];
            for event in events {
                let message = event.message(&context, &localizer);
                assert!(!message.trim().is_empty());
                assert_ne!(message, event.code().as_str());
                assert!(!message.contains("__ATT_FALLBACK__"));
            }
        }
    }

    #[test]
    fn phase_never_completes_by_inference_and_cancel_is_written_once() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let logger = runtime.logger();
        assert_eq!(
            logger.emit(ProjectLogEvent::phase_completed(
                ProjectLogPhase::ScanSource,
                ProjectLogAmount::Indeterminate,
            )),
            Err(EmitError::InvalidPhaseTransition(
                ProjectLogPhase::ScanSource
            ))
        );
        logger
            .emit(ProjectLogEvent::phase_started(
                ProjectLogPhase::ScanSource,
                ProjectLogAmount::Indeterminate,
            ))
            .expect("阶段应该开始");
        logger
            .emit(ProjectLogEvent::phase_stopped(
                ProjectLogPhase::ScanSource,
                PhaseStopOutcome::Cancelled,
            ))
            .expect("阶段应该停止");
        logger
            .emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            })
            .expect("首个取消信号必须写入");
        assert_eq!(
            logger
                .emit(ProjectLogEvent::CancellationRequested {
                    confirmed: 0,
                    total: None,
                })
                .expect("重复信号应被抑制"),
            EmitDisposition::DuplicateSuppressed
        );
        runtime
            .finish(RunFinished::Cancelled, Vec::new())
            .expect("取消运行必须收尾");
        let records = bytes.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "run.cancel_requested")
                .count(),
            1
        );
        assert_eq!(records.last().expect("必须有终态")["event"], "run.finished");
    }

    #[test]
    fn publication_started_requires_publication_finished_before_normal_shutdown() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::WriteBack,
            Box::new(bytes),
            Box::new(JsonProjectLogRecordEncoder),
        );
        runtime
            .logger()
            .emit(ProjectLogEvent::publication_started("output"))
            .expect("发布应该开始");

        assert!(matches!(
            runtime.finish(RunFinished::Succeeded, Vec::new()),
            Err(FinishError::MissingPublicationFinished)
        ));
    }

    #[test]
    fn translate_requires_exact_task_terminals_and_one_translation_finished() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Translate,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let logger = runtime.logger();
        let task = TaskPosition::new(1, 1).expect("任务位置必须有效");
        logger
            .emit(ProjectLogEvent::TaskStarted { task })
            .expect("任务必须开始");
        let diagnostic = logger
            .record_diagnostic(
                DiagnosticScope::TranslationTask,
                diagnostic_report(StateEffect::ProgressPreserved),
            )
            .expect("任务诊断必须记录");
        logger
            .emit(ProjectLogEvent::TaskFinished {
                task,
                attempts: 1,
                outcome: TaskFinishedOutcome::Partial { diagnostic },
            })
            .expect("任务必须有终态");
        let tasks = TranslationTaskCounters::new(1, 1, 0, 1, 0, 0, 0, 0).expect("任务计数必须有效");
        let result = TranslationFinished::Incomplete {
            tasks,
            summary: TranslationEngineSummary::Generic(GenericTranslationSummary {
                cleared_units: 0,
                reused_units: 0,
                accepted_units: 1,
                written_units: 1,
                conflicted_units: 0,
                response_problems: 1,
            }),
        };
        logger
            .emit(ProjectLogEvent::TranslationFinished { result })
            .expect("翻译必须有终态");
        assert_eq!(
            logger.emit(ProjectLogEvent::TranslationFinished { result }),
            Err(EmitError::DuplicateTranslationFinished)
        );
        runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("运行必须正常收尾");
        let records = bytes.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "translation.finished")
                .count(),
            1
        );
        let finished = records
            .iter()
            .find(|record| record["event"] == "translation.finished")
            .expect("必须有翻译终态");
        assert_eq!(finished["level"], "warn");
        assert_eq!(finished["payload"]["result"]["tasks"]["not_started"], 0);
    }

    #[test]
    fn later_task_not_committed_after_failure_reuses_the_original_occurrence() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Translate,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let logger = runtime.logger();
        let first = TaskPosition::new(1, 2).expect("首个任务位置必须有效");
        let second = TaskPosition::new(2, 2).expect("后续任务位置必须有效");
        logger
            .emit(ProjectLogEvent::TaskStarted { task: first })
            .expect("首个任务必须开始");
        let diagnostic = logger
            .record_diagnostic(
                DiagnosticScope::TranslationTask,
                diagnostic_report(StateEffect::ProgressPreserved),
            )
            .expect("首个失败诊断必须记录");
        logger
            .emit(ProjectLogEvent::TaskFinished {
                task: first,
                attempts: 1,
                outcome: TaskFinishedOutcome::Failed { diagnostic },
            })
            .expect("首个任务必须失败");
        logger
            .emit(ProjectLogEvent::TaskStarted { task: second })
            .expect("后续任务必须开始");
        logger
            .emit(ProjectLogEvent::TaskFinished {
                task: second,
                attempts: 1,
                outcome: TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic },
            })
            .expect("后续任务必须明确表示未提交");
        let tasks =
            TranslationTaskCounters::new(2, 2, 0, 0, 0, 2, 0, 0).expect("两项失败任务计数必须有效");
        logger
            .emit(ProjectLogEvent::TranslationFinished {
                result: TranslationFinished::Failed {
                    tasks,
                    summary: None,
                    diagnostic,
                },
            })
            .expect("翻译终态必须复用首个失败 occurrence");
        runtime
            .finish(RunFinished::Failed { diagnostic }, Vec::new())
            .expect("失败运行必须正常收尾");

        let task_records = bytes
            .records()
            .into_iter()
            .filter(|record| record["event"] == "task.finished")
            .collect::<Vec<_>>();
        assert_eq!(task_records.len(), 2);
        assert_eq!(
            task_records[1]["payload"]["outcome"]["kind"],
            "not_committed_after_earlier_failure"
        );
        assert!(
            task_records[1]["payload"]["outcome"]
                .get("diagnostic")
                .is_none(),
            "终态只说明结果，不公开内部诊断关联"
        );
    }

    #[test]
    fn planning_diagnostic_requires_failed_translation_with_all_tasks_not_started() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Translate,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let logger = runtime.logger();
        let diagnostic = logger
            .record_diagnostic(
                DiagnosticScope::RunPlan,
                diagnostic_report(StateEffect::Unchanged),
            )
            .expect("规划诊断必须记录");
        assert_eq!(
            logger.emit(ProjectLogEvent::TranslationFinished {
                result: TranslationFinished::NotStarted,
            }),
            Err(EmitError::DiagnosticRequiresFailedTranslation)
        );
        let tasks =
            TranslationTaskCounters::new(3, 0, 0, 0, 0, 0, 0, 3).expect("全部未开始的计数必须有效");
        logger
            .emit(ProjectLogEvent::TranslationFinished {
                result: TranslationFinished::Failed {
                    tasks,
                    summary: None,
                    diagnostic,
                },
            })
            .expect("规划失败必须形成带诊断的翻译终态");
        runtime
            .finish(RunFinished::Failed { diagnostic }, Vec::new())
            .expect("失败运行仍必须完成日志收尾");
        let records = bytes.records();
        let diagnostic_record = records
            .iter()
            .find(|record| record["event"] == "diagnostic.run_plan")
            .expect("必须有运行计划诊断");
        assert_eq!(diagnostic_record["level"], "error");
        let translation = records
            .iter()
            .find(|record| record["event"] == "translation.finished")
            .expect("必须有翻译终态");
        assert_eq!(translation["payload"]["result"]["kind"], "failed");
        assert_eq!(translation["payload"]["result"]["tasks"]["not_started"], 3);
    }

    #[derive(Default)]
    struct GateState {
        entered: bool,
        open: bool,
        bytes: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct GateSink {
        shared: Arc<(Mutex<GateState>, Condvar)>,
    }

    impl GateSink {
        fn wait_until_blocked(&self) {
            let (mutex, changed) = &*self.shared;
            let mut state = lock_unpoisoned(mutex);
            while !state.entered {
                state = changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }

        fn release(&self) {
            let (mutex, changed) = &*self.shared;
            lock_unpoisoned(mutex).open = true;
            changed.notify_all();
        }

        fn records(&self) -> Vec<serde_json::Value> {
            let bytes = lock_unpoisoned(&self.shared.0).bytes.clone();
            String::from_utf8(bytes)
                .expect("日志必须是 UTF-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("每行必须是 JSON"))
                .collect()
        }
    }

    impl ProjectLogSink for GateSink {
        fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
            let (mutex, changed) = &*self.shared;
            let mut state = lock_unpoisoned(mutex);
            state.entered = true;
            changed.notify_all();
            while !state.open {
                state = changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            state.bytes.extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct GateFailAfterFirstSink {
        shared: Arc<(Mutex<GateState>, Condvar)>,
        writes: usize,
    }

    impl GateFailAfterFirstSink {
        fn wait_until_blocked(&self) {
            let (mutex, changed) = &*self.shared;
            let mut state = lock_unpoisoned(mutex);
            while !state.entered {
                state = changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }

        fn release(&self) {
            let (mutex, changed) = &*self.shared;
            lock_unpoisoned(mutex).open = true;
            changed.notify_all();
        }
    }

    impl ProjectLogSink for GateFailAfterFirstSink {
        fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes += 1;
            if self.writes != 1 {
                return Err(io::Error::from_raw_os_error(5));
            }
            let (mutex, changed) = &*self.shared;
            let mut state = lock_unpoisoned(mutex);
            state.entered = true;
            changed.notify_all();
            while !state.open {
                state = changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            state.bytes.extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn queue_pressure_drops_only_best_effort_and_keeps_required_event() {
        let sink = GateSink::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(sink.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        sink.wait_until_blocked();
        let logger = runtime.logger();
        for _ in 0..BEST_EFFORT_IN_FLIGHT {
            assert_eq!(
                logger
                    .emit(ProjectLogEvent::RetrySummary {
                        attempted: 1,
                        recovered: 1,
                        exhausted: 0,
                    })
                    .expect("permit 内事件必须接收"),
                EmitDisposition::Accepted
            );
        }
        assert_eq!(
            logger
                .emit(ProjectLogEvent::RetrySummary {
                    attempted: 1,
                    recovered: 0,
                    exhausted: 1,
                })
                .expect("超出 permit 的尽力事件应正常降级"),
            EmitDisposition::BestEffortDropped
        );
        logger
            .emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            })
            .expect("必要事件不能被压力丢弃");
        sink.release();
        let shutdown = runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("运行必须收尾");
        assert_eq!(
            shutdown
                .health
                .count(&ProjectLogFailureKey::BestEffortBackpressure {
                    code: ProjectLogCode::RetrySummary,
                }),
            1
        );
        assert_eq!(
            shutdown.health.count(&ProjectLogFailureKey::ChannelClosed {
                code: Some(ProjectLogCode::RetrySummary),
            }),
            0,
            "permit 用尽不是 channel 关闭"
        );
        let failure = shutdown
            .health
            .failures
            .iter()
            .find(|entry| {
                entry.failure
                    == ProjectLogFailureKey::BestEffortBackpressure {
                        code: ProjectLogCode::RetrySummary,
                    }
            })
            .expect("尽力事件背压必须保留类型化健康项");
        let report = failure.diagnostic_report();
        assert_eq!(
            report.primary().code(),
            "observability.project_log.backpressure"
        );
        let wire = serde_json::to_value(&report).expect("背压诊断必须可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"],
            serde_json::json!({
                "operation": "backpressure",
                "event": "retry.summary",
                "count": { "kind": "exact", "count": 1 }
            })
        );
        assert!(
            sink.records()
                .iter()
                .any(|record| record["event"] == "run.cancel_requested")
        );
    }

    struct FailNthEncoder {
        call: usize,
        fail_at: usize,
        delegate: JsonProjectLogRecordEncoder,
    }

    struct CountingEncoder {
        calls: Arc<AtomicUsize>,
        delegate: JsonProjectLogRecordEncoder,
    }

    impl ProjectLogRecordEncoder for CountingEncoder {
        fn encode(&mut self, record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.delegate.encode(record)
        }
    }

    impl ProjectLogRecordEncoder for FailNthEncoder {
        fn encode(&mut self, record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError> {
            self.call += 1;
            if self.call == self.fail_at {
                Err(RecordEncodeError)
            } else {
                self.delegate.encode(record)
            }
        }
    }

    #[test]
    fn serialize_failure_is_counted_and_later_terminal_events_are_written() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(bytes.clone()),
            Box::new(FailNthEncoder {
                call: 0,
                fail_at: 2,
                delegate: JsonProjectLogRecordEncoder,
            }),
        );
        runtime
            .logger()
            .emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            })
            .expect("必要事件必须入队");
        let shutdown = runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("运行必须收尾");
        assert_eq!(
            shutdown.health.count(&ProjectLogFailureKey::Serialize {
                path: None,
                code: ProjectLogCode::CancellationRequested,
            }),
            1
        );
        let records = bytes.records();
        assert!(
            records
                .iter()
                .any(|record| record["event"] == "observability.project_log_degraded")
        );
        assert_eq!(records.last().expect("必须有终态")["event"], "run.finished");
    }

    #[derive(Clone)]
    struct FaultSink {
        shared: SharedBytes,
        write_calls: usize,
        fail_write_at: Option<usize>,
        fail_flush: bool,
        fail_sync: bool,
    }

    impl ProjectLogSink for FaultSink {
        fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.write_calls += 1;
            if self.fail_write_at == Some(self.write_calls) {
                return Err(io::Error::from_raw_os_error(5));
            }
            self.shared.clone().write_record(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush sentinel"))
            } else {
                Ok(())
            }
        }

        fn sync(&mut self) -> io::Result<()> {
            if self.fail_sync {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "sync sentinel",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn first_write_failure_stops_serialization_and_counts_unpersisted_tail() {
        let bytes = SharedBytes::default();
        let encode_calls = Arc::new(AtomicUsize::new(0));
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(FaultSink {
                shared: bytes.clone(),
                write_calls: 0,
                fail_write_at: Some(2),
                fail_flush: false,
                fail_sync: false,
            }),
            Box::new(CountingEncoder {
                calls: Arc::clone(&encode_calls),
                delegate: JsonProjectLogRecordEncoder,
            }),
        );
        runtime
            .logger()
            .emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            })
            .expect("必要事件必须入队");
        let shutdown = runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("业务终态不能被日志写失败改写");
        assert_eq!(
            shutdown.health.count(&ProjectLogFailureKey::Write {
                path: None,
                code: ProjectLogCode::CancellationRequested,
                io_kind: SafeIoKind::PermissionDenied,
                raw_os_code: Some(5),
            }),
            1
        );
        assert!(
            shutdown.health.count(&ProjectLogFailureKey::NotPersisted {
                code: ProjectLogCode::RunFinished,
            }) > 0
        );
        assert_eq!(bytes.records().len(), 1);
        assert_eq!(
            encode_calls.load(Ordering::Relaxed),
            2,
            "首次 write 失败后不得再建立或序列化后续记录"
        );
    }

    #[test]
    fn first_write_failure_drains_queued_best_effort_events_before_finish() {
        let sink = GateFailAfterFirstSink::default();
        let encode_calls = Arc::new(AtomicUsize::new(0));
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(sink.clone()),
            Box::new(CountingEncoder {
                calls: Arc::clone(&encode_calls),
                delegate: JsonProjectLogRecordEncoder,
            }),
        );
        sink.wait_until_blocked();
        let logger = runtime.logger();
        logger
            .emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            })
            .expect("触发首个 write 失败的必要事件必须入队");
        for _ in 0..32 {
            assert_eq!(
                logger
                    .emit(ProjectLogEvent::RetrySummary {
                        attempted: 1,
                        recovered: 1,
                        exhausted: 0,
                    })
                    .expect("排空前的 BestEffort 事件必须能入队"),
                EmitDisposition::Accepted
            );
        }
        assert_eq!(
            logger.inner.permits.in_flight.load(Ordering::Acquire),
            32,
            "测试必须先确认所有待排空事件都持有 permit"
        );
        sink.release();
        let write_failure = ProjectLogFailureKey::Write {
            path: None,
            code: ProjectLogCode::CancellationRequested,
            io_kind: SafeIoKind::PermissionDenied,
            raw_os_code: Some(5),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let failure_count = logger.health().count(&write_failure);
            let permits_in_flight = logger.inner.permits.in_flight.load(Ordering::Acquire);
            if failure_count == 1 && permits_in_flight == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "等待 write 失败排空队列超时：failure_count={failure_count}, permits_in_flight={permits_in_flight}"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(logger.health().count(&write_failure), 1);
        assert_eq!(
            logger.inner.permits.in_flight.load(Ordering::Acquire),
            0,
            "首个 write 失败必须在业务收尾前释放已排空 BestEffort 的 permit"
        );
        let shutdown = runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("日志失败不改变业务收尾能力");
        assert_eq!(
            encode_calls.load(Ordering::Relaxed),
            2,
            "首个 write 失败后 FIFO 中的事件必须直接计数而非继续编码"
        );
        assert!(
            shutdown.health.count(&ProjectLogFailureKey::NotPersisted {
                code: ProjectLogCode::RetrySummary,
            }) >= 32
        );
    }

    #[test]
    fn flush_and_sync_failures_are_kept_independently() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(FaultSink {
                shared: bytes,
                write_calls: 0,
                fail_write_at: None,
                fail_flush: true,
                fail_sync: true,
            }),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let shutdown = runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("业务终态不能被日志收尾失败改写");
        assert_eq!(
            shutdown.health.count(&ProjectLogFailureKey::Flush {
                path: None,
                io_kind: SafeIoKind::BrokenPipe,
                raw_os_code: None,
            }),
            1
        );
        assert_eq!(
            shutdown.health.count(&ProjectLogFailureKey::Sync {
                path: None,
                io_kind: SafeIoKind::PermissionDenied,
                raw_os_code: None,
            }),
            1
        );
    }

    #[test]
    fn terminal_diagnostic_is_after_performance_and_before_unique_run_finished() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let prepared = runtime
            .logger()
            .prepare_terminal_diagnostic(
                DiagnosticScope::Run,
                diagnostic_report(StateEffect::OutcomeUnknown),
            )
            .expect("必须能预备终端诊断");
        let result = RunFinished::OutcomeUnknown {
            diagnostic: prepared.id(),
        };
        runtime
            .finish(result, vec![prepared])
            .expect("未知终态必须写出");
        let records = bytes.records();
        let codes = records
            .iter()
            .map(|record| record["event"].as_str().expect("event 必须是字符串"))
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "run.started",
                "performance.counters",
                "diagnostic.run",
                "run.finished",
            ]
        );
        assert_eq!(
            codes.iter().filter(|code| **code == "run.finished").count(),
            1
        );
    }

    #[test]
    fn runtime_drop_writes_pre_registered_unknown_outcome_without_panic_payload() {
        let bytes = SharedBytes::default();
        {
            let _runtime = runtime_with_components(
                ProjectLogCommand::Extract,
                Box::new(bytes.clone()),
                Box::new(JsonProjectLogRecordEncoder),
            );
        }
        let records = bytes.records();
        assert_eq!(
            records.last().expect("Drop 必须写终态")["event"],
            "run.finished"
        );
        assert_eq!(
            records.last().expect("Drop 必须写终态")["payload"]["result"]["kind"],
            "outcome_unknown"
        );
        let serialized = serde_json::to_string(&records).expect("测试记录必须可序列化");
        assert!(!serialized.contains("panic payload"));
    }

    #[test]
    fn finish_validation_failure_replaces_drop_panic_report_with_contract_diagnostic() {
        let bytes = SharedBytes::default();
        let runtime = ProjectLogRuntime::start_with_components(
            context(ProjectLogCommand::Extract),
            run_id(),
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
            Arc::new(FixedClock),
            Arc::new(RunPerformanceCounters::default()),
            command_panic_drop_report(),
        )
        .expect("测试 runtime 必须启动");
        runtime
            .logger()
            .emit(ProjectLogEvent::phase_started(
                ProjectLogPhase::ScanSource,
                ProjectLogAmount::Indeterminate,
            ))
            .expect("阶段开始必须入队");

        assert_eq!(
            runtime.finish(RunFinished::Succeeded, Vec::new()),
            Err(FinishError::ActivePhase)
        );

        let records = bytes.records();
        let terminal = records.last().expect("Drop 必须尽力写出终态");
        assert_eq!(terminal["event"], "run.finished");
        assert_eq!(terminal["payload"]["result"]["kind"], "outcome_unknown");
        let diagnostic = records
            .iter()
            .find(|record| record["event"] == "diagnostic.run")
            .expect("finish 合同错误必须成为终态诊断");
        let expected = render_diagnostic_fields(
            &FinishError::ActivePhase.diagnostic_report(),
            &UiLocalizer::new(UiLocale::English),
        );
        assert_eq!(diagnostic["payload"]["object"], expected.object);
        assert_eq!(diagnostic["payload"]["reason"], expected.reason);
        assert_eq!(diagnostic["payload"]["help"], expected.help);
        let serialized = serde_json::to_string(&records).expect("日志必须可序列化");
        assert!(!serialized.contains("runtime.command_panicked"));
    }

    struct PanicEncoder;

    impl ProjectLogRecordEncoder for PanicEncoder {
        fn encode(&mut self, _record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError> {
            panic!("writer panic sentinel")
        }
    }

    #[test]
    fn writer_panic_drains_queue_and_marks_terminal_events_unpersisted() {
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(SharedBytes::default()),
            Box::new(PanicEncoder),
        );
        let logger = runtime.logger();
        for _ in 0..100_000 {
            if logger.health().count(&ProjectLogFailureKey::WorkerPanicked) == 1 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(
            logger.health().count(&ProjectLogFailureKey::WorkerPanicked),
            1
        );
        assert_eq!(
            logger.emit(ProjectLogEvent::CancellationRequested {
                confirmed: 0,
                total: None,
            }),
            Err(EmitError::Closed)
        );
        drop(runtime);
        assert!(
            logger.health().count(&ProjectLogFailureKey::ChannelClosed {
                code: Some(ProjectLogCode::CancellationRequested),
            }) >= 1
        );
        assert!(
            logger.health().count(&ProjectLogFailureKey::NotPersisted {
                code: ProjectLogCode::RunFinished,
            }) >= 1,
            "writer panic 后无法入队的收尾事件必须留下未持久化计数"
        );
    }

    struct PanicSecondEncoder {
        calls: usize,
        delegate: JsonProjectLogRecordEncoder,
    }

    impl ProjectLogRecordEncoder for PanicSecondEncoder {
        fn encode(&mut self, record: &ProjectLogRecord) -> Result<Vec<u8>, RecordEncodeError> {
            self.calls += 1;
            if self.calls == 2 {
                panic!("writer second-record panic sentinel");
            }
            self.delegate.encode(record)
        }
    }

    #[test]
    fn writer_panic_releases_best_effort_permits() {
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(SharedBytes::default()),
            Box::new(PanicSecondEncoder {
                calls: 0,
                delegate: JsonProjectLogRecordEncoder,
            }),
        );
        let logger = runtime.logger();
        for _ in 0..32 {
            let _ = logger.emit(ProjectLogEvent::RetrySummary {
                attempted: 1,
                recovered: 1,
                exhausted: 0,
            });
        }
        for _ in 0..100_000 {
            if logger.health().count(&ProjectLogFailureKey::WorkerPanicked) != 0 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(
            logger.health().count(&ProjectLogFailureKey::WorkerPanicked),
            1
        );
        assert_eq!(
            logger.inner.permits.in_flight.load(Ordering::Acquire),
            0,
            "writer panic 后所有已接收的 BestEffort permit 必须归还"
        );
        drop(runtime);
    }

    #[test]
    fn terminal_events_reject_mismatched_diagnostic_scope() {
        let extract = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(SharedBytes::default()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let extract_logger = extract.logger();
        extract_logger
            .emit(ProjectLogEvent::RunPlanResolved {
                plan: ResolvedRunPlan::generic_extract(RunPlanValueSource::Explicit),
            })
            .expect("Extract 运行计划必须能建立");
        let extract_diagnostic = extract_logger
            .record_diagnostic(
                DiagnosticScope::Extract,
                diagnostic_report(StateEffect::Unchanged),
            )
            .expect("Extract 诊断必须能建立");
        assert_eq!(
            extract_logger.emit(ProjectLogEvent::RunPlanFinalized {
                database: SafePath::new("project.db"),
                result: RunPlanFinalization::NotSaved {
                    transaction: RunPlanTransactionState::NotStarted,
                    run_continues: false,
                    diagnostic: extract_diagnostic,
                },
            }),
            Err(EmitError::InvalidDiagnosticScope(extract_diagnostic))
        );

        let translate = runtime_with_components(
            ProjectLogCommand::Translate,
            Box::new(SharedBytes::default()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let translate_logger = translate.logger();
        let task = TaskPosition::new(1, 1).expect("任务位置合法");
        translate_logger
            .emit(ProjectLogEvent::TaskStarted { task })
            .expect("任务开始必须能建立");
        let run_diagnostic = translate_logger
            .record_diagnostic(
                DiagnosticScope::Run,
                diagnostic_report(StateEffect::Unchanged),
            )
            .expect("运行诊断必须能建立");
        assert_eq!(
            translate_logger.emit(ProjectLogEvent::TaskFinished {
                task,
                attempts: 1,
                outcome: TaskFinishedOutcome::Failed {
                    diagnostic: run_diagnostic,
                },
            }),
            Err(EmitError::InvalidDiagnosticScope(run_diagnostic))
        );

        let publication = runtime_with_components(
            ProjectLogCommand::WriteBack,
            Box::new(SharedBytes::default()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        let publication_logger = publication.logger();
        publication_logger
            .emit(ProjectLogEvent::publication_started("output"))
            .expect("发布开始必须能建立");
        let run_diagnostic = publication_logger
            .record_diagnostic(
                DiagnosticScope::Run,
                diagnostic_report(StateEffect::Unchanged),
            )
            .expect("运行诊断必须能建立");
        assert_eq!(
            publication_logger.emit(ProjectLogEvent::PublicationFinished {
                result: PublicationFinished::NotPublished {
                    diagnostic: run_diagnostic,
                },
            }),
            Err(EmitError::InvalidDiagnosticScope(run_diagnostic))
        );
    }

    #[test]
    fn drop_closes_sender_when_terminal_occurrence_cannot_be_allocated() {
        let bytes = SharedBytes::default();
        let runtime = runtime_with_components(
            ProjectLogCommand::Extract,
            Box::new(bytes.clone()),
            Box::new(JsonProjectLogRecordEncoder),
        );
        runtime
            .logger
            .inner
            .next_occurrence
            .store(u64::MAX, Ordering::Release);
        drop(runtime);
        assert_eq!(
            bytes.records().len(),
            1,
            "无法分配 Drop 终态时仍须关闭 sender 并结束 writer"
        );
    }
}
