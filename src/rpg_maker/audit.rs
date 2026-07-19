//! RPG Maker 各引擎命令共享的强审计账本。
//!
//! 本模块拥有审计事件语义和稳定 JSON wire；通用 JSONL Runtime 只负责按物理顺序
//! 追加、刷盘、轮转与恢复完整记录，不理解 RPG Maker 项目、命令或位置。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::llm::LlmUsage;
use crate::observability::{EventId, OperationId, RunId};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::model::{LogicalTextLocation, TextFieldRole};
use crate::rpg_maker::project::RpgMakerWriteBackLayoutProfile;
use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};
use crate::rpg_maker::translate::standard::{
    LoggedAcceptedTranslationDecision, LoggedUnresolvedTranslationUnit,
    StandardTranslationTaskIndex, TranslationProtocolDiagnostic, TranslationTaskLogRecord,
    TranslationTaskUnavailableReason, TranslationUnitRejectionReason,
};
use crate::rpg_maker::write_back::StandardWriteBackSummary;
use crate::rpg_maker::write_back::standard::{
    ManualLayoutDiagnostic, RpgMakerWriteBackLayoutRegion, WriteBackRunLog,
};
use crate::runtime::json_lines::{
    JsonLineRecord, JsonLinesAppendError, JsonLinesEventLog, JsonLinesEventLogFinalizer,
    JsonLinesStartError, JsonLinesStreamConfig, start_stream,
};
use crate::runtime::run_id::{generate_event_id, generate_operation_id};
use crate::runtime::windows::WindowsFsError;

const AUDIT_STEM: &str = "audit";

/// 一次 RPG Maker 命令运行的稳定审计上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuditContext {
    run_id: RunId,
    engine: RpgMakerEngine,
    project: String,
    command: AuditCommandContext,
}

impl AuditContext {
    pub(crate) fn init(run_id: RunId, engine: RpgMakerEngine, project: impl Into<String>) -> Self {
        Self::new(run_id, engine, project, AuditCommandContext::Init)
    }

    pub(crate) fn extract(
        run_id: RunId,
        engine: RpgMakerEngine,
        project: impl Into<String>,
    ) -> Self {
        Self::new(run_id, engine, project, AuditCommandContext::Extract)
    }

    pub(crate) fn translate(
        run_id: RunId,
        engine: RpgMakerEngine,
        project: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        Self::new(
            run_id,
            engine,
            project,
            AuditCommandContext::Translate {
                profile: profile.into(),
            },
        )
    }

    pub(crate) fn write_back(
        run_id: RunId,
        engine: RpgMakerEngine,
        project: impl Into<String>,
    ) -> Self {
        Self::new(run_id, engine, project, AuditCommandContext::WriteBack)
    }

    fn new(
        run_id: RunId,
        engine: RpgMakerEngine,
        project: impl Into<String>,
        command: AuditCommandContext,
    ) -> Self {
        Self {
            run_id,
            engine,
            project: project.into(),
            command,
        }
    }

    pub(crate) const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub(crate) const fn engine(&self) -> RpgMakerEngine {
        self.engine
    }

    pub(crate) fn project(&self) -> &str {
        &self.project
    }

    pub(crate) const fn command(&self) -> AuditCommand {
        self.command.command()
    }

    pub(crate) fn profile(&self) -> Option<&str> {
        self.command.profile()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AuditCommandContext {
    Init,
    Extract,
    Translate { profile: String },
    WriteBack,
}

impl AuditCommandContext {
    const fn command(&self) -> AuditCommand {
        match self {
            Self::Init => AuditCommand::Init,
            Self::Extract => AuditCommand::Extract,
            Self::Translate { .. } => AuditCommand::Translate,
            Self::WriteBack => AuditCommand::WriteBack,
        }
    }

    fn profile(&self) -> Option<&str> {
        match self {
            Self::Translate { profile } => Some(profile),
            Self::Init | Self::Extract | Self::WriteBack => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuditCommand {
    Init,
    Extract,
    Translate,
    WriteBack,
}

/// 运行终态只记录用户能够据此判断命令结果的稳定类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuditRunOutcome {
    Succeeded,
    Interrupted,
    Failed(AuditFailureCategory),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuditFailureCategory {
    ConfigurationOrInput,
    ProjectUnavailable,
    ProjectState,
    ExternalModel,
    AuditLedger,
    StateAppliedButFinalizationFailed,
    OutcomeUnknown,
    Internal,
}

/// 翻译任务的终态。失败分支不保存任意底层错误文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskAuditResult {
    Completed(TranslationTaskLogRecord),
    CommitFailed(TranslationTaskLogRecord),
    /// 请求已经取得结果，但同一运行更早的技术失败阻止了该结果进入数据库。
    NotCommitted(TranslationTaskLogRecord),
    ExecutionFailed {
        task_index: StandardTranslationTaskIndex,
    },
}

/// 写回发布调用已经取得的事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackPublishAuditResult {
    Published(WriteBackRunLog),
    NotPublished {
        output_root: PathBuf,
        residual_paths: Vec<PathBuf>,
    },
    PublishedWithResiduals {
        output_root: PathBuf,
        residual_paths: Vec<PathBuf>,
    },
    RecoveryRequired {
        output_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
    },
    OutcomeUnknown {
        output_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
    },
}

/// 一次审计追加的领域事件；事件身份由生产账本在 append 边界生成。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AuditEvent {
    RunStarted,
    RunFinished {
        outcome: AuditRunOutcome,
    },
    TranslationTaskStarted {
        operation_id: OperationId,
        task_index: StandardTranslationTaskIndex,
    },
    TranslationTaskFinished {
        operation_id: OperationId,
        result: TranslationTaskAuditResult,
    },
    WriteBackPublishStarted {
        operation_id: OperationId,
        output_root: PathBuf,
    },
    WriteBackPublishFinished {
        operation_id: OperationId,
        result: WriteBackPublishAuditResult,
    },
}

impl AuditEvent {
    fn required_command(&self) -> Option<AuditCommand> {
        match self {
            Self::RunStarted | Self::RunFinished { .. } => None,
            Self::TranslationTaskStarted { .. } | Self::TranslationTaskFinished { .. } => {
                Some(AuditCommand::Translate)
            }
            Self::WriteBackPublishStarted { .. } | Self::WriteBackPublishFinished { .. } => {
                Some(AuditCommand::WriteBack)
            }
        }
    }
}

/// 已绑定单次运行上下文的强审计账本。
pub(crate) trait AuditLedger: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn new_operation_id(&self) -> Result<OperationId, Self::Error>;

    fn append(
        &self,
        event: AuditEvent,
    ) -> impl Future<Output = Result<EventId, Self::Error>> + Send;
}

/// 全进程唯一 audit JSONL 流；每次命令从它绑定一个运行上下文。
#[derive(Clone, Debug)]
pub(crate) struct JsonLinesAuditLedger {
    stream: JsonLinesEventLog<AuditRecord>,
}

impl JsonLinesAuditLedger {
    pub(crate) fn start(
        root: PathBuf,
        config: JsonLinesStreamConfig,
    ) -> Result<(Self, JsonLinesEventLogFinalizer), JsonLinesStartError> {
        let (stream, finalizer) = start_stream(root, AUDIT_STEM, config)?;
        Ok((Self { stream }, finalizer))
    }

    pub(crate) fn bind(&self, context: AuditContext) -> JsonLinesAuditRun {
        JsonLinesAuditRun {
            context: Arc::new(context),
            stream: self.stream.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JsonLinesAuditRun {
    context: Arc<AuditContext>,
    stream: JsonLinesEventLog<AuditRecord>,
}

#[derive(Debug)]
pub(crate) enum AuditLedgerError {
    GenerateEventId(WindowsFsError),
    GenerateOperationId(WindowsFsError),
    EventDoesNotBelongToCommand {
        event_command: AuditCommand,
        run_command: AuditCommand,
    },
    Append(JsonLinesAppendError),
}

impl fmt::Display for AuditLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenerateEventId(source) => write!(formatter, "无法生成审计事件身份：{source}"),
            Self::GenerateOperationId(source) => {
                write!(formatter, "无法生成审计操作身份：{source}")
            }
            Self::EventDoesNotBelongToCommand {
                event_command,
                run_command,
            } => write!(
                formatter,
                "审计事件属于 {event_command:?}，不能写入 {run_command:?} 运行"
            ),
            Self::Append(source) => source.fmt(formatter),
        }
    }
}

impl Error for AuditLedgerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::GenerateEventId(source) | Self::GenerateOperationId(source) => Some(source),
            Self::Append(source) => Some(source),
            Self::EventDoesNotBelongToCommand { .. } => None,
        }
    }
}

impl AuditLedger for JsonLinesAuditRun {
    type Error = AuditLedgerError;

    fn new_operation_id(&self) -> Result<OperationId, Self::Error> {
        generate_operation_id().map_err(AuditLedgerError::GenerateOperationId)
    }

    async fn append(&self, event: AuditEvent) -> Result<EventId, Self::Error> {
        if let Some(event_command) = event.required_command()
            && event_command != self.context.command()
        {
            return Err(AuditLedgerError::EventDoesNotBelongToCommand {
                event_command,
                run_command: self.context.command(),
            });
        }
        let event_id = generate_event_id().map_err(AuditLedgerError::GenerateEventId)?;
        self.stream
            .append(AuditRecord {
                context: Arc::clone(&self.context),
                event_id,
                event,
            })
            .await
            .map_err(AuditLedgerError::Append)?;
        Ok(event_id)
    }
}

#[derive(Debug)]
struct AuditRecord {
    context: Arc<AuditContext>,
    event_id: EventId,
    event: AuditEvent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuditCommandWire {
    Init,
    Extract,
    Translate,
    WriteBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RpgMakerEngineWire {
    Mz,
    Mv,
}

impl From<RpgMakerEngine> for RpgMakerEngineWire {
    fn from(value: RpgMakerEngine) -> Self {
        match value {
            RpgMakerEngine::Mz => Self::Mz,
            RpgMakerEngine::Mv => Self::Mv,
        }
    }
}

impl From<AuditCommand> for AuditCommandWire {
    fn from(value: AuditCommand) -> Self {
        match value {
            AuditCommand::Init => Self::Init,
            AuditCommand::Extract => Self::Extract,
            AuditCommand::Translate => Self::Translate,
            AuditCommand::WriteBack => Self::WriteBack,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuditEventKindWire {
    RunStarted,
    TranslationTaskStarted,
    TranslationTaskFinished,
    WriteBackPublishStarted,
    WriteBackPublishFinished,
    RunFinished,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditEnvelopeWire<P> {
    recorded_at_utc: String,
    event_id: String,
    run_id: String,
    engine: RpgMakerEngineWire,
    project: String,
    command: AuditCommandWire,
    profile: Option<String>,
    event: AuditEventKindWire,
    payload: P,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EmptyPayloadWire {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunFinishedPayloadWire {
    outcome: AuditRunOutcomeWire,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AuditRunOutcomeWire {
    Succeeded,
    Interrupted,
    Failed { category: AuditFailureCategoryWire },
}

impl From<AuditRunOutcome> for AuditRunOutcomeWire {
    fn from(value: AuditRunOutcome) -> Self {
        match value {
            AuditRunOutcome::Succeeded => Self::Succeeded,
            AuditRunOutcome::Interrupted => Self::Interrupted,
            AuditRunOutcome::Failed(category) => Self::Failed {
                category: category.into(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuditFailureCategoryWire {
    ConfigurationOrInput,
    ProjectUnavailable,
    ProjectState,
    ExternalModel,
    AuditLedger,
    StateAppliedButFinalizationFailed,
    OutcomeUnknown,
    Internal,
}

impl From<AuditFailureCategory> for AuditFailureCategoryWire {
    fn from(value: AuditFailureCategory) -> Self {
        match value {
            AuditFailureCategory::ConfigurationOrInput => Self::ConfigurationOrInput,
            AuditFailureCategory::ProjectUnavailable => Self::ProjectUnavailable,
            AuditFailureCategory::ProjectState => Self::ProjectState,
            AuditFailureCategory::ExternalModel => Self::ExternalModel,
            AuditFailureCategory::AuditLedger => Self::AuditLedger,
            AuditFailureCategory::StateAppliedButFinalizationFailed => {
                Self::StateAppliedButFinalizationFailed
            }
            AuditFailureCategory::OutcomeUnknown => Self::OutcomeUnknown,
            AuditFailureCategory::Internal => Self::Internal,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskStartedPayloadWire {
    operation_id: String,
    task_index: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskFinishedPayloadWire {
    operation_id: String,
    result: TranslationTaskResultWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationTaskResultWire {
    Completed { task: TranslationTaskWire },
    CommitFailed { task: TranslationTaskWire },
    NotCommitted { task: TranslationTaskWire },
    ExecutionFailed { task_index: usize },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackPublishStartedPayloadWire {
    operation_id: String,
    output_root: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackPublishFinishedPayloadWire {
    operation_id: String,
    result: WriteBackPublishResultWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WriteBackPublishResultWire {
    Published {
        write_back: WriteBackPayloadWire,
    },
    NotPublished {
        output_root: String,
        residual_paths: Vec<String>,
    },
    PublishedWithResiduals {
        output_root: String,
        residual_paths: Vec<String>,
    },
    RecoveryRequired {
        output_root: String,
        recovery_artifacts: Vec<String>,
    },
    OutcomeUnknown {
        output_root: String,
        recovery_artifacts: Vec<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskWire {
    task_index: usize,
    status: TranslationTaskStatusWire,
    attempts: usize,
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: Option<String>,
    final_response_usage: Option<LlmUsageWire>,
    accepted_decisions: usize,
    confirmed_written_leaves: Option<usize>,
    accepted: Vec<AcceptedTranslationWire>,
    unresolved: Vec<UnresolvedTranslationWire>,
    diagnostics: Vec<ProtocolDiagnosticWire>,
}

impl TranslationTaskWire {
    fn from_record(record: &TranslationTaskLogRecord) -> Self {
        Self {
            task_index: record.task_index().get(),
            status: TranslationTaskStatusWire::from(record),
            attempts: record.attempts().get(),
            provider_request_id: record.provider_request_id().map(str::to_owned),
            provider_response_id: record.provider_response_id().map(str::to_owned),
            finish_reason: record.finish_reason().map(str::to_owned),
            final_response_usage: record.final_response_usage().map(LlmUsageWire::from),
            accepted_decisions: record.accepted_decisions(),
            confirmed_written_leaves: None,
            accepted: record
                .accepted()
                .iter()
                .map(AcceptedTranslationWire::from)
                .collect(),
            unresolved: record
                .unresolved()
                .iter()
                .map(UnresolvedTranslationWire::from)
                .collect(),
            diagnostics: record
                .diagnostics()
                .iter()
                .map(ProtocolDiagnosticWire::from)
                .collect(),
        }
    }

    fn from_confirmed_record(record: &TranslationTaskLogRecord) -> Self {
        let mut wire = Self::from_record(record);
        wire.confirmed_written_leaves = Some(
            wire.accepted
                .iter()
                .map(|accepted| 1 + accepted.propagation_targets.len())
                .sum(),
        );
        wire
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationTaskStatusWire {
    Complete,
    Partial,
    Unavailable {
        reason: TranslationTaskUnavailableReasonWire,
    },
}

impl From<&TranslationTaskLogRecord> for TranslationTaskStatusWire {
    fn from(record: &TranslationTaskLogRecord) -> Self {
        match record {
            TranslationTaskLogRecord::Complete { .. } => Self::Complete,
            TranslationTaskLogRecord::Partial { .. } => Self::Partial,
            TranslationTaskLogRecord::Unavailable { reason, .. } => Self::Unavailable {
                reason: TranslationTaskUnavailableReasonWire::from(reason),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationTaskUnavailableReasonWire {
    ModelResponseUnusable,
    AllOutputsRejected,
    RecoverableRequestExhausted {
        message: String,
    },
    RetryAfterExceedsConfiguredMaximum {
        retry_after_ms: u128,
        maximum_ms: u128,
        message: String,
    },
}

impl From<&TranslationTaskUnavailableReason> for TranslationTaskUnavailableReasonWire {
    fn from(reason: &TranslationTaskUnavailableReason) -> Self {
        match reason {
            TranslationTaskUnavailableReason::ModelResponseUnusable => Self::ModelResponseUnusable,
            TranslationTaskUnavailableReason::AllOutputsRejected => Self::AllOutputsRejected,
            TranslationTaskUnavailableReason::RecoverableRequestExhausted { message } => {
                Self::RecoverableRequestExhausted {
                    message: message.clone(),
                }
            }
            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                retry_after,
                maximum,
                message,
            } => Self::RetryAfterExceedsConfiguredMaximum {
                retry_after_ms: retry_after.as_millis(),
                maximum_ms: maximum.as_millis(),
                message: message.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LlmUsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

impl From<LlmUsage> for LlmUsageWire {
    fn from(usage: LlmUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens(),
            completion_tokens: usage.completion_tokens(),
            total_tokens: usage.total_tokens(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedTranslationWire {
    id: usize,
    leader: LogicalTextLocationWire,
    propagation_targets: Vec<LogicalTextLocationWire>,
}

impl From<&LoggedAcceptedTranslationDecision> for AcceptedTranslationWire {
    fn from(decision: &LoggedAcceptedTranslationDecision) -> Self {
        Self {
            id: decision.id(),
            leader: LogicalTextLocationWire::from(decision.leader()),
            propagation_targets: decision
                .propagation_targets()
                .iter()
                .map(LogicalTextLocationWire::from)
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnresolvedTranslationWire {
    id: usize,
    locations: Vec<LogicalTextLocationWire>,
    reason: TranslationUnitRejectionReasonWire,
}

impl From<&LoggedUnresolvedTranslationUnit> for UnresolvedTranslationWire {
    fn from(unit: &LoggedUnresolvedTranslationUnit) -> Self {
        Self {
            id: unit.id(),
            locations: unit
                .locations()
                .iter()
                .map(LogicalTextLocationWire::from)
                .collect(),
            reason: TranslationUnitRejectionReasonWire::from(unit.reason()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationUnitRejectionReasonWire {
    Missing,
    Duplicate,
    InvalidShape { message: String },
    BlankTranslation,
    InvalidSpeakerText,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch { token: String },
    UnexpectedPlaceholderToken { token: String },
    PlaceholderNormalizationAmbiguous { original: String },
    SourceResidual { fragment: String },
}

impl From<&TranslationUnitRejectionReason> for TranslationUnitRejectionReasonWire {
    fn from(reason: &TranslationUnitRejectionReason) -> Self {
        match reason {
            TranslationUnitRejectionReason::Missing => Self::Missing,
            TranslationUnitRejectionReason::Duplicate => Self::Duplicate,
            TranslationUnitRejectionReason::InvalidShape { message } => Self::InvalidShape {
                message: message.clone(),
            },
            TranslationUnitRejectionReason::BlankTranslation => Self::BlankTranslation,
            TranslationUnitRejectionReason::InvalidSpeakerText => Self::InvalidSpeakerText,
            TranslationUnitRejectionReason::NoNaturalLanguageText => Self::NoNaturalLanguageText,
            TranslationUnitRejectionReason::ContainsByteOrderMark => Self::ContainsByteOrderMark,
            TranslationUnitRejectionReason::PlaceholderMismatch { token } => {
                Self::PlaceholderMismatch {
                    token: token.clone(),
                }
            }
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token } => {
                Self::UnexpectedPlaceholderToken {
                    token: token.clone(),
                }
            }
            TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { original } => {
                Self::PlaceholderNormalizationAmbiguous {
                    original: original.clone(),
                }
            }
            TranslationUnitRejectionReason::SourceResidual { fragment } => Self::SourceResidual {
                fragment: fragment.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProtocolDiagnosticWire {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    UnknownId { item_index: usize, id: usize },
}

impl From<&TranslationProtocolDiagnostic> for ProtocolDiagnosticWire {
    fn from(diagnostic: &TranslationProtocolDiagnostic) -> Self {
        match diagnostic {
            TranslationProtocolDiagnostic::NonStopFinish { reason } => Self::NonStopFinish {
                reason: reason.clone(),
            },
            TranslationProtocolDiagnostic::InvalidResponse { message } => Self::InvalidResponse {
                message: message.clone(),
            },
            TranslationProtocolDiagnostic::UnknownId { item_index, id } => Self::UnknownId {
                item_index: *item_index,
                id: *id,
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackPayloadWire {
    layout_profile: LayoutProfileWire,
    output_root: String,
    lua_executed: bool,
    summary: WriteBackSummaryWire,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnosticWire>,
}

impl TryFrom<&WriteBackRunLog> for WriteBackPayloadWire {
    type Error = String;

    fn try_from(event: &WriteBackRunLog) -> Result<Self, Self::Error> {
        Ok(Self {
            layout_profile: LayoutProfileWire::from(event.layout_profile()),
            output_root: output_root_text(event.output_root())?,
            lua_executed: event.lua_executed(),
            summary: WriteBackSummaryWire::from(event.summary()),
            manual_layout_diagnostics: event
                .manual_layout_diagnostics()
                .iter()
                .map(ManualLayoutDiagnosticWire::from)
                .collect(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LayoutProfileWire {
    dialogue_body_max_fullwidth_chars: u32,
    scrolling_text_max_fullwidth_chars: u32,
    help_description_max_fullwidth_chars: u32,
}

impl From<RpgMakerWriteBackLayoutProfile> for LayoutProfileWire {
    fn from(profile: RpgMakerWriteBackLayoutProfile) -> Self {
        Self {
            dialogue_body_max_fullwidth_chars: profile.dialogue_body().get(),
            scrolling_text_max_fullwidth_chars: profile.scrolling_text().get(),
            help_description_max_fullwidth_chars: profile.help_description().get(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackSummaryWire {
    translated_leaves: usize,
    original_leaves: usize,
    auto_wrapped_units: usize,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
    manual_layout_units: usize,
}

impl From<StandardWriteBackSummary> for WriteBackSummaryWire {
    fn from(summary: StandardWriteBackSummary) -> Self {
        Self {
            translated_leaves: summary.translated_locations,
            original_leaves: summary.original_locations,
            auto_wrapped_units: summary.auto_wrapped_units,
            inserted_line_breaks: summary.inserted_line_breaks,
            inserted_fullwidth_indents: summary.inserted_fullwidth_indents,
            manual_layout_units: summary.manual_layout_units,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualLayoutDiagnosticWire {
    locations: Vec<LogicalTextLocationWire>,
    region: LayoutRegionWire,
    max_fullwidth_chars: u32,
}

impl From<&ManualLayoutDiagnostic> for ManualLayoutDiagnosticWire {
    fn from(diagnostic: &ManualLayoutDiagnostic) -> Self {
        Self {
            locations: diagnostic
                .locations()
                .iter()
                .map(LogicalTextLocationWire::from)
                .collect(),
            region: LayoutRegionWire::from(diagnostic.region()),
            max_fullwidth_chars: diagnostic.max_fullwidth_chars().get(),
        }
    }
}

impl ManualLayoutDiagnosticWire {
    fn validate(&self) -> Result<(), String> {
        if self.locations.is_empty() {
            return Err("人工布局诊断必须关联至少一个逻辑文本位置".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LayoutRegionWire {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

impl From<RpgMakerWriteBackLayoutRegion> for LayoutRegionWire {
    fn from(region: RpgMakerWriteBackLayoutRegion) -> Self {
        match region {
            RpgMakerWriteBackLayoutRegion::DialogueBody => Self::DialogueBody,
            RpgMakerWriteBackLayoutRegion::ScrollingText => Self::ScrollingText,
            RpgMakerWriteBackLayoutRegion::HelpDescription => Self::HelpDescription,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RpgMakerLocationWire {
    Value {
        source: RpgMakerSourceWire,
        steps: Vec<RpgMakerLocationStepWire>,
    },
    NoteTag {
        source: RpgMakerSourceWire,
        container_steps: Vec<RpgMakerLocationStepWire>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: RpgMakerSourceWire,
        command_steps: Vec<RpgMakerLocationStepWire>,
        tag_name: String,
        occurrence: usize,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LogicalTextLocationWire {
    group_location: RpgMakerLocationWire,
    field_role: TextFieldRoleWire,
}

impl From<&LogicalTextLocation> for LogicalTextLocationWire {
    fn from(location: &LogicalTextLocation) -> Self {
        Self {
            group_location: RpgMakerLocationWire::from(location.group_location()),
            field_role: TextFieldRoleWire::from(location.role()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TextFieldRoleWire {
    Scalar { field: String },
    DialogueSpeaker,
    DialogueBody { index: usize },
    ScrollingTextBody { index: usize },
}

impl From<&TextFieldRole> for TextFieldRoleWire {
    fn from(role: &TextFieldRole) -> Self {
        match role {
            TextFieldRole::Scalar(field) => Self::Scalar {
                field: field.as_str().to_owned(),
            },
            TextFieldRole::DialogueSpeaker => Self::DialogueSpeaker,
            TextFieldRole::DialogueBody { index } => Self::DialogueBody { index: *index },
            TextFieldRole::ScrollingTextBody { index } => Self::ScrollingTextBody { index: *index },
        }
    }
}

impl From<&RpgMakerLocation> for RpgMakerLocationWire {
    fn from(location: &RpgMakerLocation) -> Self {
        match location {
            RpgMakerLocation::Value { source, steps } => Self::Value {
                source: RpgMakerSourceWire::from(source),
                steps: steps.iter().map(RpgMakerLocationStepWire::from).collect(),
            },
            RpgMakerLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source: RpgMakerSourceWire::from(source),
                container_steps: container_steps
                    .iter()
                    .map(RpgMakerLocationStepWire::from)
                    .collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
            RpgMakerLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source: RpgMakerSourceWire::from(source),
                command_steps: command_steps
                    .iter()
                    .map(RpgMakerLocationStepWire::from)
                    .collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RpgMakerSourceWire {
    Data {
        file: String,
    },
    Map {
        map_id: u32,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl From<&RpgMakerSource> for RpgMakerSourceWire {
    fn from(source: &RpgMakerSource) -> Self {
        match source {
            RpgMakerSource::Data(file) => Self::Data {
                file: file.file_name().to_owned(),
            },
            RpgMakerSource::DataFile(file) => Self::Data {
                file: file.as_str().to_owned(),
            },
            RpgMakerSource::Map(map_id) => Self::Map { map_id: *map_id },
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Self::PluginParameter {
                plugin_index: *plugin_index,
                plugin_name: plugin_name.clone(),
                parameter_name: parameter_name.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RpgMakerLocationStepWire {
    ObjectKey { key: String },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl From<&RpgMakerLocationStep> for RpgMakerLocationStepWire {
    fn from(step: &RpgMakerLocationStep) -> Self {
        match step {
            RpgMakerLocationStep::ObjectKey(key) => Self::ObjectKey { key: key.clone() },
            RpgMakerLocationStep::ArrayIndex(index) => Self::ArrayIndex { index: *index },
            RpgMakerLocationStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

fn output_root_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "写回输出路径无法无损表示为 UTF-8".to_owned())
}

fn output_root_texts(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    paths.iter().map(|path| output_root_text(path)).collect()
}

fn validate_uuid_v4(value: &str, field: &str) -> Result<(), String> {
    let parsed = Uuid::parse_str(value).map_err(|_| format!("{field} 不是规范 UUID"))?;
    if parsed.get_version_num() != 4 || parsed.to_string() != value {
        return Err(format!("{field} 不是规范小写 UUID v4"));
    }
    Ok(())
}

fn validate_recorded_at_utc(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err("recorded_at_utc 不是 UTC 毫秒格式".to_owned());
    }
    let number = |range: std::ops::Range<usize>| -> Result<u32, String> {
        std::str::from_utf8(&bytes[range])
            .ok()
            .and_then(|part| part.parse().ok())
            .ok_or_else(|| "recorded_at_utc 包含非法数字".to_owned())
    };
    let year = i32::try_from(number(0..4)?).map_err(|_| "年份越界".to_owned())?;
    let month =
        time::Month::try_from(u8::try_from(number(5..7)?).map_err(|_| "月份越界".to_owned())?)
            .map_err(|_| "月份越界".to_owned())?;
    let day = u8::try_from(number(8..10)?).map_err(|_| "日期越界".to_owned())?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| "日期越界".to_owned())?;
    let hour = u8::try_from(number(11..13)?).map_err(|_| "小时越界".to_owned())?;
    let minute = u8::try_from(number(14..16)?).map_err(|_| "分钟越界".to_owned())?;
    let second = u8::try_from(number(17..19)?).map_err(|_| "秒越界".to_owned())?;
    let millisecond = u16::try_from(number(20..23)?).map_err(|_| "毫秒越界".to_owned())?;
    time::Time::from_hms_milli(hour, minute, second, millisecond)
        .map_err(|_| "时间越界".to_owned())?;
    Ok(())
}

fn validate_canonical_wire(bytes: &[u8], wire: &impl Serialize) -> Result<(), String> {
    let canonical = serde_json::to_vec(wire).map_err(|error| error.to_string())?;
    if canonical != bytes {
        return Err("记录不是当前紧凑 UTF-8 wire".to_owned());
    }
    Ok(())
}

trait AuditPayloadValidation {
    fn validate(&self) -> Result<(), String>;
}

impl AuditPayloadValidation for EmptyPayloadWire {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

impl AuditPayloadValidation for RunFinishedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

impl AuditPayloadValidation for TranslationTaskStartedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        validate_uuid_v4(&self.operation_id, "operation_id")
    }
}

impl AuditPayloadValidation for TranslationTaskFinishedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        validate_uuid_v4(&self.operation_id, "operation_id")?;
        let (task, committed) = match &self.result {
            TranslationTaskResultWire::Completed { task } => (Some(task), true),
            TranslationTaskResultWire::CommitFailed { task }
            | TranslationTaskResultWire::NotCommitted { task } => (Some(task), false),
            TranslationTaskResultWire::ExecutionFailed { .. } => (None, false),
        };
        if let Some(task) = task {
            if task.accepted_decisions != task.accepted.len() {
                return Err("accepted_decisions 与结构化决定数量不一致".to_owned());
            }
            let expected_written = task
                .accepted
                .iter()
                .map(|accepted| 1 + accepted.propagation_targets.len())
                .sum();
            if committed {
                if task.confirmed_written_leaves != Some(expected_written) {
                    return Err(
                        "completed 的 confirmed_written_leaves 必须等于结构化写入逻辑叶数"
                            .to_owned(),
                    );
                }
            } else if task.confirmed_written_leaves.is_some() {
                return Err("未提交任务的 confirmed_written_leaves 必须为 null".to_owned());
            }
        }
        Ok(())
    }
}

impl AuditPayloadValidation for WriteBackPublishStartedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        validate_uuid_v4(&self.operation_id, "operation_id")?;
        if self.output_root.trim().is_empty() {
            return Err("output_root 不能为空".to_owned());
        }
        Ok(())
    }
}

impl AuditPayloadValidation for WriteBackPublishFinishedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        validate_uuid_v4(&self.operation_id, "operation_id")?;
        match &self.result {
            WriteBackPublishResultWire::Published { write_back } => {
                if write_back.output_root.trim().is_empty() {
                    return Err("output_root 不能为空".to_owned());
                }
                if write_back.layout_profile.dialogue_body_max_fullwidth_chars == 0
                    || write_back.layout_profile.scrolling_text_max_fullwidth_chars == 0
                    || write_back
                        .layout_profile
                        .help_description_max_fullwidth_chars
                        == 0
                {
                    return Err("layout_profile 的三个实际宽度都必须大于零".to_owned());
                }
                if write_back.summary.manual_layout_units
                    != write_back.manual_layout_diagnostics.len()
                {
                    return Err("manual_layout_units 与结构化诊断数量不一致".to_owned());
                }
                for diagnostic in &write_back.manual_layout_diagnostics {
                    diagnostic.validate()?;
                }
            }
            WriteBackPublishResultWire::NotPublished { output_root, .. }
            | WriteBackPublishResultWire::PublishedWithResiduals { output_root, .. }
            | WriteBackPublishResultWire::RecoveryRequired { output_root, .. }
            | WriteBackPublishResultWire::OutcomeUnknown { output_root, .. } => {
                if output_root.trim().is_empty() {
                    return Err("output_root 不能为空".to_owned());
                }
            }
        }
        Ok(())
    }
}

fn validate_payload<P>(
    bytes: &[u8],
    expected_event: AuditEventKindWire,
    expected_command: Option<AuditCommandWire>,
) -> Result<(), String>
where
    P: AuditPayloadValidation + DeserializeOwned + Serialize,
{
    let wire: AuditEnvelopeWire<P> =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if wire.event != expected_event {
        return Err("审计事件类型与 payload 不一致".to_owned());
    }
    if expected_command.is_some_and(|command| command != wire.command) {
        return Err("审计事件不属于当前命令".to_owned());
    }
    validate_recorded_at_utc(&wire.recorded_at_utc)?;
    validate_uuid_v4(&wire.event_id, "event_id")?;
    validate_uuid_v4(&wire.run_id, "run_id")?;
    if wire.project.trim().is_empty() {
        return Err("project 不能为空".to_owned());
    }
    match wire.command {
        AuditCommandWire::Translate => {
            if wire
                .profile
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err("Translate 审计记录必须携带非空 profile".to_owned());
            }
        }
        AuditCommandWire::Init | AuditCommandWire::Extract | AuditCommandWire::WriteBack => {
            if wire.profile.is_some() {
                return Err("非 Translate 审计记录不得携带 profile".to_owned());
            }
        }
    }
    wire.payload.validate()?;
    validate_canonical_wire(bytes, &wire)
}

impl JsonLineRecord for AuditRecord {
    fn serialize(self, recorded_at_utc: String) -> Result<Vec<u8>, String> {
        macro_rules! envelope {
            ($event:expr, $payload:expr) => {
                AuditEnvelopeWire {
                    recorded_at_utc: recorded_at_utc.clone(),
                    event_id: self.event_id.to_string(),
                    run_id: self.context.run_id().to_string(),
                    engine: self.context.engine().into(),
                    project: self.context.project().to_owned(),
                    command: self.context.command().into(),
                    profile: self.context.profile().map(str::to_owned),
                    event: $event,
                    payload: $payload,
                }
            };
        }
        match self.event {
            AuditEvent::RunStarted => serde_json::to_vec(&envelope!(
                AuditEventKindWire::RunStarted,
                EmptyPayloadWire::default()
            )),
            AuditEvent::RunFinished { outcome } => serde_json::to_vec(&envelope!(
                AuditEventKindWire::RunFinished,
                RunFinishedPayloadWire {
                    outcome: outcome.into(),
                }
            )),
            AuditEvent::TranslationTaskStarted {
                operation_id,
                task_index,
            } => serde_json::to_vec(&envelope!(
                AuditEventKindWire::TranslationTaskStarted,
                TranslationTaskStartedPayloadWire {
                    operation_id: operation_id.to_string(),
                    task_index: task_index.get(),
                }
            )),
            AuditEvent::TranslationTaskFinished {
                operation_id,
                result,
            } => {
                let result = match result {
                    TranslationTaskAuditResult::Completed(task) => {
                        TranslationTaskResultWire::Completed {
                            task: TranslationTaskWire::from_confirmed_record(&task),
                        }
                    }
                    TranslationTaskAuditResult::CommitFailed(task) => {
                        TranslationTaskResultWire::CommitFailed {
                            task: TranslationTaskWire::from_record(&task),
                        }
                    }
                    TranslationTaskAuditResult::NotCommitted(task) => {
                        TranslationTaskResultWire::NotCommitted {
                            task: TranslationTaskWire::from_record(&task),
                        }
                    }
                    TranslationTaskAuditResult::ExecutionFailed { task_index } => {
                        TranslationTaskResultWire::ExecutionFailed {
                            task_index: task_index.get(),
                        }
                    }
                };
                serde_json::to_vec(&envelope!(
                    AuditEventKindWire::TranslationTaskFinished,
                    TranslationTaskFinishedPayloadWire {
                        operation_id: operation_id.to_string(),
                        result,
                    }
                ))
            }
            AuditEvent::WriteBackPublishStarted {
                operation_id,
                output_root,
            } => serde_json::to_vec(&envelope!(
                AuditEventKindWire::WriteBackPublishStarted,
                WriteBackPublishStartedPayloadWire {
                    operation_id: operation_id.to_string(),
                    output_root: output_root_text(&output_root)?,
                }
            )),
            AuditEvent::WriteBackPublishFinished {
                operation_id,
                result,
            } => {
                let result = match result {
                    WriteBackPublishAuditResult::Published(event) => {
                        WriteBackPublishResultWire::Published {
                            write_back: WriteBackPayloadWire::try_from(&event)?,
                        }
                    }
                    WriteBackPublishAuditResult::NotPublished {
                        output_root,
                        residual_paths,
                    } => WriteBackPublishResultWire::NotPublished {
                        output_root: output_root_text(&output_root)?,
                        residual_paths: output_root_texts(&residual_paths)?,
                    },
                    WriteBackPublishAuditResult::PublishedWithResiduals {
                        output_root,
                        residual_paths,
                    } => WriteBackPublishResultWire::PublishedWithResiduals {
                        output_root: output_root_text(&output_root)?,
                        residual_paths: output_root_texts(&residual_paths)?,
                    },
                    WriteBackPublishAuditResult::RecoveryRequired {
                        output_root,
                        recovery_artifacts,
                    } => WriteBackPublishResultWire::RecoveryRequired {
                        output_root: output_root_text(&output_root)?,
                        recovery_artifacts: output_root_texts(&recovery_artifacts)?,
                    },
                    WriteBackPublishAuditResult::OutcomeUnknown {
                        output_root,
                        recovery_artifacts,
                    } => WriteBackPublishResultWire::OutcomeUnknown {
                        output_root: output_root_text(&output_root)?,
                        recovery_artifacts: output_root_texts(&recovery_artifacts)?,
                    },
                };
                serde_json::to_vec(&envelope!(
                    AuditEventKindWire::WriteBackPublishFinished,
                    WriteBackPublishFinishedPayloadWire {
                        operation_id: operation_id.to_string(),
                        result,
                    }
                ))
            }
        }
        .map_err(|error| error.to_string())
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Probe {
            recorded_at_utc: Value,
            event_id: Value,
            run_id: Value,
            engine: Value,
            project: Value,
            command: Value,
            profile: Value,
            event: AuditEventKindWire,
            payload: Value,
        }

        let probe: Probe = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        let _ = (
            probe.recorded_at_utc,
            probe.event_id,
            probe.run_id,
            probe.engine,
            probe.project,
            probe.command,
            probe.profile,
            probe.payload,
        );
        match probe.event {
            AuditEventKindWire::RunStarted => {
                validate_payload::<EmptyPayloadWire>(bytes, AuditEventKindWire::RunStarted, None)
            }
            AuditEventKindWire::RunFinished => validate_payload::<RunFinishedPayloadWire>(
                bytes,
                AuditEventKindWire::RunFinished,
                None,
            ),
            AuditEventKindWire::TranslationTaskStarted => {
                validate_payload::<TranslationTaskStartedPayloadWire>(
                    bytes,
                    AuditEventKindWire::TranslationTaskStarted,
                    Some(AuditCommandWire::Translate),
                )
            }
            AuditEventKindWire::TranslationTaskFinished => {
                validate_payload::<TranslationTaskFinishedPayloadWire>(
                    bytes,
                    AuditEventKindWire::TranslationTaskFinished,
                    Some(AuditCommandWire::Translate),
                )
            }
            AuditEventKindWire::WriteBackPublishStarted => {
                validate_payload::<WriteBackPublishStartedPayloadWire>(
                    bytes,
                    AuditEventKindWire::WriteBackPublishStarted,
                    Some(AuditCommandWire::WriteBack),
                )
            }
            AuditEventKindWire::WriteBackPublishFinished => {
                validate_payload::<WriteBackPublishFinishedPayloadWire>(
                    bytes,
                    AuditEventKindWire::WriteBackPublishFinished,
                    Some(AuditCommandWire::WriteBack),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;

    fn run_id() -> RunId {
        RunId::from_uuid(Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0000))
    }

    fn config() -> JsonLinesStreamConfig {
        JsonLinesStreamConfig::new(8, Duration::from_secs(1), 16_384, 65_536, 2)
            .expect("测试账本配置应合法")
    }

    fn dialogue_location(index: usize) -> LogicalTextLocation {
        LogicalTextLocation::new(
            RpgMakerLocation::value(
                RpgMakerSource::map(1),
                vec![
                    RpgMakerLocationStep::key("events"),
                    RpgMakerLocationStep::index(2),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(7),
                ],
            ),
            TextFieldRole::DialogueBody { index },
        )
    }

    #[test]
    fn manual_layout_wire_contains_one_or_more_logical_locations_and_no_physical_unit() {
        let diagnostic = ManualLayoutDiagnostic::for_test(
            vec![dialogue_location(0), dialogue_location(1)],
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            crate::rpg_maker::project::MaxFullwidthChars::new(24).expect("测试行宽应合法"),
        );

        let wire = serde_json::to_value(ManualLayoutDiagnosticWire::from(&diagnostic))
            .expect("人工布局诊断 wire 应可序列化");
        assert_eq!(
            wire,
            json!({
                "locations": [
                    {
                        "group_location": {
                            "kind": "value",
                            "source": { "kind": "map", "map_id": 1 },
                            "steps": [
                                { "kind": "object_key", "key": "events" },
                                { "kind": "array_index", "index": 2 },
                                { "kind": "object_key", "key": "list" },
                                { "kind": "array_index", "index": 7 }
                            ]
                        },
                        "field_role": { "kind": "dialogue_body", "index": 0 }
                    },
                    {
                        "group_location": {
                            "kind": "value",
                            "source": { "kind": "map", "map_id": 1 },
                            "steps": [
                                { "kind": "object_key", "key": "events" },
                                { "kind": "array_index", "index": 2 },
                                { "kind": "object_key", "key": "list" },
                                { "kind": "array_index", "index": 7 }
                            ]
                        },
                        "field_role": { "kind": "dialogue_body", "index": 1 }
                    }
                ],
                "region": "dialogue_body",
                "max_fullwidth_chars": 24
            })
        );

        let empty = ManualLayoutDiagnosticWire {
            locations: Vec::new(),
            region: LayoutRegionWire::DialogueBody,
            max_fullwidth_chars: 24,
        };
        assert_eq!(
            empty.validate().expect_err("空位置诊断不属于当前 wire"),
            "人工布局诊断必须关联至少一个逻辑文本位置"
        );
    }

    #[test]
    fn context_only_allows_profile_on_translate() {
        assert_eq!(
            AuditContext::init(run_id(), RpgMakerEngine::Mz, "demo").profile(),
            None
        );
        assert_eq!(
            AuditContext::translate(run_id(), RpgMakerEngine::Mv, "demo", "quality").profile(),
            Some("quality")
        );
        assert_eq!(
            AuditContext::write_back(run_id(), RpgMakerEngine::Mz, "demo").profile(),
            None
        );
    }

    #[tokio::test]
    async fn task_intent_and_terminal_share_operation_but_not_event_identity() {
        let directory = tempdir().expect("临时目录应可创建");
        let (ledger, finalizer) =
            JsonLinesAuditLedger::start(directory.path().to_path_buf(), config())
                .expect("审计账本应可启动");
        let run = ledger.bind(AuditContext::translate(
            run_id(),
            RpgMakerEngine::Mv,
            "demo",
            "quality",
        ));
        let operation_id = run.new_operation_id().expect("操作身份应可生成");

        let started = run
            .append(AuditEvent::TranslationTaskStarted {
                operation_id,
                task_index: StandardTranslationTaskIndex::new(3),
            })
            .await
            .expect("任务意图应持久化");
        let finished = run
            .append(AuditEvent::TranslationTaskFinished {
                operation_id,
                result: TranslationTaskAuditResult::ExecutionFailed {
                    task_index: StandardTranslationTaskIndex::new(3),
                },
            })
            .await
            .expect("任务终态应持久化");
        assert_ne!(started, finished);
        finalizer.finalize().await.expect("账本应可排空");

        let bytes =
            std::fs::read(directory.path().join("audit.jsonl")).expect("统一活动账本应存在");
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                AuditRecord::validate(line).expect("每条审计记录都应满足强类型 wire");
                serde_json::from_slice::<Value>(line).expect("审计记录应是 JSON")
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "translation_task_started");
        assert_eq!(records[1]["event"], "translation_task_finished");
        assert_eq!(
            records[0]["payload"]["operation_id"],
            operation_id.to_string()
        );
        assert_eq!(
            records[1]["payload"]["operation_id"],
            operation_id.to_string()
        );
        assert_ne!(records[0]["event_id"], records[1]["event_id"]);
        assert_eq!(records[0]["run_id"], run_id().to_string());
        assert_eq!(records[0]["engine"], "mv");
        assert_eq!(records[0]["project"], "demo");
        assert_eq!(records[0]["command"], "translate");
        assert_eq!(records[0]["profile"], "quality");
    }

    #[tokio::test]
    async fn command_mismatch_is_rejected_before_any_record_is_enqueued() {
        let directory = tempdir().expect("临时目录应可创建");
        let (ledger, finalizer) =
            JsonLinesAuditLedger::start(directory.path().to_path_buf(), config())
                .expect("审计账本应可启动");
        let run = ledger.bind(AuditContext::init(run_id(), RpgMakerEngine::Mz, "demo"));
        let operation_id = run.new_operation_id().expect("操作身份应可生成");

        assert!(matches!(
            run.append(AuditEvent::WriteBackPublishStarted {
                operation_id,
                output_root: PathBuf::from("C:/att/projects/demo/write_back"),
            })
            .await,
            Err(AuditLedgerError::EventDoesNotBelongToCommand { .. })
        ));
        finalizer.finalize().await.expect("账本应可排空");
        assert!(!directory.path().join("audit.jsonl").exists());
    }

    #[tokio::test]
    async fn write_back_terminal_preserves_the_exact_publish_state() {
        let directory = tempdir().expect("临时目录应可创建");
        let (ledger, finalizer) =
            JsonLinesAuditLedger::start(directory.path().to_path_buf(), config())
                .expect("审计账本应可启动");
        let run = ledger.bind(AuditContext::write_back(
            run_id(),
            RpgMakerEngine::Mz,
            "demo",
        ));
        let operation_id = run.new_operation_id().expect("操作身份应可生成");
        let output_root = PathBuf::from("C:/att/projects/demo/write_back");
        let recovery_artifacts = vec![PathBuf::from("C:/att/projects/demo/.write_back-recovery")];

        run.append(AuditEvent::WriteBackPublishStarted {
            operation_id,
            output_root: output_root.clone(),
        })
        .await
        .expect("发布意图应持久化");
        run.append(AuditEvent::WriteBackPublishFinished {
            operation_id,
            result: WriteBackPublishAuditResult::RecoveryRequired {
                output_root,
                recovery_artifacts: recovery_artifacts.clone(),
            },
        })
        .await
        .expect("精确发布终态应持久化");
        finalizer.finalize().await.expect("账本应可排空");

        let bytes =
            std::fs::read(directory.path().join("audit.jsonl")).expect("统一活动账本应存在");
        let terminal = bytes
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).expect("终态应是 JSON"))
            .expect("应存在发布终态");
        assert_eq!(terminal["event"], "write_back_publish_finished");
        assert_eq!(terminal["payload"]["result"]["kind"], "recovery_required");
        assert_eq!(
            terminal["payload"]["result"]["recovery_artifacts"][0],
            recovery_artifacts[0].to_str().expect("测试路径应是 UTF-8")
        );
    }
}
