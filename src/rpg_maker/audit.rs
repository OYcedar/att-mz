//! RPG Maker 各引擎命令共享的强审计账本。
//!
//! 本模块拥有审计事件语义和稳定 JSON wire；通用 JSONL Runtime 只负责按物理顺序
//! 追加、刷盘、轮转与恢复完整记录，不理解 RPG Maker 项目、命令或位置。

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use uuid::Uuid;

use crate::llm::LlmUsage;
use crate::observability::{EventId, OperationId, RunId};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::model::{LogicalTextLocation, TextUnitRole};
use crate::rpg_maker::project::RpgMakerWriteBackLayoutProfile;
use crate::rpg_maker::text::{MapId, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};
use crate::rpg_maker::translate::standard::{
    LoggedAcceptedTranslationDecision, LoggedUnresolvedTranslationUnit,
    StandardTranslationTaskIndex, TranslationPlanningFailure, TranslationPlanningFailureReason,
    TranslationProtocolDiagnostic, TranslationTaskLogRecord, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason,
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
    TranslationPlanningUnresolved {
        failure: TranslationPlanningFailure,
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
            Self::TranslationTaskStarted { .. }
            | Self::TranslationTaskFinished { .. }
            | Self::TranslationPlanningUnresolved { .. } => Some(AuditCommand::Translate),
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
    TranslationPlanningUnresolved,
    WriteBackPublishStarted,
    WriteBackPublishFinished,
    RunFinished,
}

#[derive(Serialize)]
struct AuditEnvelopeWriteWire<'a, P> {
    recorded_at_utc: &'a str,
    event_id: DisplayWriteWire<EventId>,
    run_id: DisplayWriteWire<RunId>,
    engine: RpgMakerEngineWire,
    project: &'a str,
    command: AuditCommandWire,
    profile: Option<&'a str>,
    event: AuditEventKindWire,
    payload: P,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditEnvelopeProbe<'a> {
    recorded_at_utc: &'a str,
    event_id: &'a str,
    run_id: &'a str,
    engine: RpgMakerEngineWire,
    #[serde(borrow)]
    project: Cow<'a, str>,
    command: AuditCommandWire,
    #[serde(borrow)]
    profile: Option<Cow<'a, str>>,
    event: AuditEventKindWire,
    #[serde(borrow)]
    payload: &'a RawValue,
}

#[derive(Serialize)]
struct AuditEnvelopeRawWire<'a> {
    recorded_at_utc: &'a str,
    event_id: &'a str,
    run_id: &'a str,
    engine: RpgMakerEngineWire,
    project: &'a str,
    command: AuditCommandWire,
    profile: Option<&'a str>,
    event: AuditEventKindWire,
    payload: &'a RawValue,
}

#[derive(Clone, Copy)]
struct DisplayWriteWire<T>(T);

impl<T> Serialize for DisplayWriteWire<T>
where
    T: fmt::Display,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
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

#[derive(Serialize)]
struct TranslationTaskStartedPayloadWriteWire {
    operation_id: DisplayWriteWire<OperationId>,
    task_index: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationPlanningUnresolvedPayloadWire {
    failure: TranslationPlanningFailureWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationPlanningFailureWire {
    location: LogicalTextLocationWire,
    reason: TranslationPlanningFailureReasonWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationPlanningFailureReasonWire {
    PlaceholderProtection { message: String },
    PlaceholderProjection { message: String },
}

#[derive(Serialize)]
struct TranslationPlanningUnresolvedPayloadWriteWire<'a> {
    failure: TranslationPlanningFailureWriteWire<'a>,
}

#[derive(Serialize)]
struct TranslationPlanningFailureWriteWire<'a> {
    location: LogicalTextLocationWriteWire<'a>,
    reason: TranslationPlanningFailureReasonWriteWire<'a>,
}

impl<'a> From<&'a TranslationPlanningFailure> for TranslationPlanningFailureWriteWire<'a> {
    fn from(failure: &'a TranslationPlanningFailure) -> Self {
        Self {
            location: LogicalTextLocationWriteWire::from(failure.identity().logical_location()),
            reason: TranslationPlanningFailureReasonWriteWire::from(failure.reason()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranslationPlanningFailureReasonWriteWire<'a> {
    PlaceholderProtection { message: &'a str },
    PlaceholderProjection { message: &'a str },
}

impl<'a> From<&'a TranslationPlanningFailureReason>
    for TranslationPlanningFailureReasonWriteWire<'a>
{
    fn from(reason: &'a TranslationPlanningFailureReason) -> Self {
        match reason {
            TranslationPlanningFailureReason::PlaceholderProtection { message } => {
                Self::PlaceholderProtection { message }
            }
            TranslationPlanningFailureReason::PlaceholderProjection { message } => {
                Self::PlaceholderProjection { message }
            }
        }
    }
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

#[derive(Serialize)]
struct TranslationTaskFinishedPayloadWriteWire<'a> {
    operation_id: DisplayWriteWire<OperationId>,
    result: TranslationTaskResultWriteWire<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranslationTaskResultWriteWire<'a> {
    Completed { task: TranslationTaskWriteWire<'a> },
    CommitFailed { task: TranslationTaskWriteWire<'a> },
    NotCommitted { task: TranslationTaskWriteWire<'a> },
    ExecutionFailed { task_index: usize },
}

impl<'a> From<&'a TranslationTaskAuditResult> for TranslationTaskResultWriteWire<'a> {
    fn from(result: &'a TranslationTaskAuditResult) -> Self {
        match result {
            TranslationTaskAuditResult::Completed(task) => Self::Completed {
                task: TranslationTaskWriteWire::new(task, true),
            },
            TranslationTaskAuditResult::CommitFailed(task) => Self::CommitFailed {
                task: TranslationTaskWriteWire::new(task, false),
            },
            TranslationTaskAuditResult::NotCommitted(task) => Self::NotCommitted {
                task: TranslationTaskWriteWire::new(task, false),
            },
            TranslationTaskAuditResult::ExecutionFailed { task_index } => Self::ExecutionFailed {
                task_index: task_index.get(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackPublishStartedPayloadWire {
    operation_id: String,
    output_root: String,
}

#[derive(Serialize)]
struct WriteBackPublishStartedPayloadWriteWire<'a> {
    operation_id: DisplayWriteWire<OperationId>,
    output_root: &'a str,
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

#[derive(Serialize)]
struct WriteBackPublishFinishedPayloadWriteWire<'a> {
    operation_id: DisplayWriteWire<OperationId>,
    result: WriteBackPublishResultWriteWire<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WriteBackPublishResultWriteWire<'a> {
    Published {
        write_back: WriteBackPayloadWriteWire<'a>,
    },
    NotPublished {
        output_root: &'a str,
        residual_paths: PathsWriteWire<'a>,
    },
    PublishedWithResiduals {
        output_root: &'a str,
        residual_paths: PathsWriteWire<'a>,
    },
    RecoveryRequired {
        output_root: &'a str,
        recovery_artifacts: PathsWriteWire<'a>,
    },
    OutcomeUnknown {
        output_root: &'a str,
        recovery_artifacts: PathsWriteWire<'a>,
    },
}

impl<'a> TryFrom<&'a WriteBackPublishAuditResult> for WriteBackPublishResultWriteWire<'a> {
    type Error = String;

    fn try_from(result: &'a WriteBackPublishAuditResult) -> Result<Self, Self::Error> {
        match result {
            WriteBackPublishAuditResult::Published(event) => Ok(Self::Published {
                write_back: WriteBackPayloadWriteWire::try_from(event)?,
            }),
            WriteBackPublishAuditResult::NotPublished {
                output_root,
                residual_paths,
            } => Ok(Self::NotPublished {
                output_root: output_root_text_ref(output_root)?,
                residual_paths: PathsWriteWire(residual_paths),
            }),
            WriteBackPublishAuditResult::PublishedWithResiduals {
                output_root,
                residual_paths,
            } => Ok(Self::PublishedWithResiduals {
                output_root: output_root_text_ref(output_root)?,
                residual_paths: PathsWriteWire(residual_paths),
            }),
            WriteBackPublishAuditResult::RecoveryRequired {
                output_root,
                recovery_artifacts,
            } => Ok(Self::RecoveryRequired {
                output_root: output_root_text_ref(output_root)?,
                recovery_artifacts: PathsWriteWire(recovery_artifacts),
            }),
            WriteBackPublishAuditResult::OutcomeUnknown {
                output_root,
                recovery_artifacts,
            } => Ok(Self::OutcomeUnknown {
                output_root: output_root_text_ref(output_root)?,
                recovery_artifacts: PathsWriteWire(recovery_artifacts),
            }),
        }
    }
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
    confirmed_written_units: Option<usize>,
    accepted: Vec<AcceptedTranslationWire>,
    unresolved: Vec<UnresolvedTranslationWire>,
    diagnostics: Vec<ProtocolDiagnosticWire>,
}

#[derive(Serialize)]
struct TranslationTaskWriteWire<'a> {
    task_index: usize,
    status: TranslationTaskStatusWriteWire<'a>,
    attempts: usize,
    provider_request_id: Option<&'a str>,
    provider_response_id: Option<&'a str>,
    finish_reason: Option<&'a str>,
    final_response_usage: Option<LlmUsageWire>,
    accepted_decisions: usize,
    confirmed_written_units: Option<usize>,
    accepted: AcceptedTranslationsWriteWire<'a>,
    unresolved: UnresolvedTranslationsWriteWire<'a>,
    diagnostics: ProtocolDiagnosticsWriteWire<'a>,
}

impl<'a> TranslationTaskWriteWire<'a> {
    fn new(record: &'a TranslationTaskLogRecord, committed: bool) -> Self {
        let confirmed_written_units = committed.then(|| {
            record
                .accepted()
                .iter()
                .map(|accepted| 1 + accepted.propagation_targets().len())
                .sum()
        });
        Self {
            task_index: record.task_index().get(),
            status: TranslationTaskStatusWriteWire::from(record),
            attempts: record.attempts().get(),
            provider_request_id: record.provider_request_id(),
            provider_response_id: record.provider_response_id(),
            finish_reason: record.finish_reason(),
            final_response_usage: record.final_response_usage().map(LlmUsageWire::from),
            accepted_decisions: record.accepted_decisions(),
            confirmed_written_units,
            accepted: AcceptedTranslationsWriteWire(record.accepted()),
            unresolved: UnresolvedTranslationsWriteWire(record.unresolved()),
            diagnostics: ProtocolDiagnosticsWriteWire(record.diagnostics()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranslationTaskStatusWriteWire<'a> {
    Complete,
    Partial,
    Unavailable {
        reason: TranslationTaskUnavailableReasonWriteWire<'a>,
    },
}

impl<'a> From<&'a TranslationTaskLogRecord> for TranslationTaskStatusWriteWire<'a> {
    fn from(record: &'a TranslationTaskLogRecord) -> Self {
        match record {
            TranslationTaskLogRecord::Complete { .. } => Self::Complete,
            TranslationTaskLogRecord::Partial { .. } => Self::Partial,
            TranslationTaskLogRecord::Unavailable { reason, .. } => Self::Unavailable {
                reason: TranslationTaskUnavailableReasonWriteWire::from(reason),
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranslationTaskUnavailableReasonWriteWire<'a> {
    ModelResponseUnusable,
    AllOutputsRejected,
    RecoverableRequestExhausted {
        message: &'a str,
    },
    RetryAfterExceedsConfiguredMaximum {
        retry_after_ms: u128,
        maximum_ms: u128,
        message: &'a str,
    },
}

impl<'a> From<&'a TranslationTaskUnavailableReason>
    for TranslationTaskUnavailableReasonWriteWire<'a>
{
    fn from(reason: &'a TranslationTaskUnavailableReason) -> Self {
        match reason {
            TranslationTaskUnavailableReason::ModelResponseUnusable => Self::ModelResponseUnusable,
            TranslationTaskUnavailableReason::AllOutputsRejected => Self::AllOutputsRejected,
            TranslationTaskUnavailableReason::RecoverableRequestExhausted { message } => {
                Self::RecoverableRequestExhausted { message }
            }
            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                retry_after,
                maximum,
                message,
            } => Self::RetryAfterExceedsConfiguredMaximum {
                retry_after_ms: retry_after.as_millis(),
                maximum_ms: maximum.as_millis(),
                message,
            },
        }
    }
}

struct AcceptedTranslationsWriteWire<'a>(&'a [LoggedAcceptedTranslationDecision]);

impl Serialize for AcceptedTranslationsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for decision in self.0 {
            sequence.serialize_element(&AcceptedTranslationWriteWire::from(decision))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct AcceptedTranslationWriteWire<'a> {
    id: usize,
    leader: LogicalTextLocationWriteWire<'a>,
    propagation_targets: LogicalTextLocationsWriteWire<'a>,
}

impl<'a> From<&'a LoggedAcceptedTranslationDecision> for AcceptedTranslationWriteWire<'a> {
    fn from(decision: &'a LoggedAcceptedTranslationDecision) -> Self {
        Self {
            id: decision.id(),
            leader: LogicalTextLocationWriteWire::from(decision.leader()),
            propagation_targets: LogicalTextLocationsWriteWire(decision.propagation_targets()),
        }
    }
}

struct UnresolvedTranslationsWriteWire<'a>(&'a [LoggedUnresolvedTranslationUnit]);

impl Serialize for UnresolvedTranslationsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for unit in self.0 {
            sequence.serialize_element(&UnresolvedTranslationWriteWire::from(unit))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct UnresolvedTranslationWriteWire<'a> {
    id: usize,
    locations: LogicalTextLocationsWriteWire<'a>,
    reason: TranslationUnitRejectionReasonWriteWire<'a>,
}

impl<'a> From<&'a LoggedUnresolvedTranslationUnit> for UnresolvedTranslationWriteWire<'a> {
    fn from(unit: &'a LoggedUnresolvedTranslationUnit) -> Self {
        Self {
            id: unit.id(),
            locations: LogicalTextLocationsWriteWire(unit.locations()),
            reason: TranslationUnitRejectionReasonWriteWire::from(unit.reason()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranslationUnitRejectionReasonWriteWire<'a> {
    Missing,
    Duplicate,
    InvalidShape {
        message: &'a str,
    },
    LineCountMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidLineText {
        line_index: usize,
    },
    BlankLineMismatch {
        line_index: usize,
        expected_blank: bool,
    },
    BlankTranslation,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch {
        token: &'a str,
    },
    UnexpectedPlaceholderToken {
        token: &'a str,
    },
    PlaceholderNormalizationAmbiguous {
        original: &'a str,
    },
    SourceResidual {
        fragment: &'a str,
    },
}

impl<'a> From<&'a TranslationUnitRejectionReason> for TranslationUnitRejectionReasonWriteWire<'a> {
    fn from(reason: &'a TranslationUnitRejectionReason) -> Self {
        match reason {
            TranslationUnitRejectionReason::Missing => Self::Missing,
            TranslationUnitRejectionReason::Duplicate => Self::Duplicate,
            TranslationUnitRejectionReason::InvalidShape { message } => {
                Self::InvalidShape { message }
            }
            TranslationUnitRejectionReason::LineCountMismatch { expected, actual } => {
                Self::LineCountMismatch {
                    expected: *expected,
                    actual: *actual,
                }
            }
            TranslationUnitRejectionReason::InvalidLineText { line_index } => {
                Self::InvalidLineText {
                    line_index: *line_index,
                }
            }
            TranslationUnitRejectionReason::BlankLineMismatch {
                line_index,
                expected_blank,
            } => Self::BlankLineMismatch {
                line_index: *line_index,
                expected_blank: *expected_blank,
            },
            TranslationUnitRejectionReason::BlankTranslation => Self::BlankTranslation,
            TranslationUnitRejectionReason::NoNaturalLanguageText => Self::NoNaturalLanguageText,
            TranslationUnitRejectionReason::ContainsByteOrderMark => Self::ContainsByteOrderMark,
            TranslationUnitRejectionReason::PlaceholderMismatch { token } => {
                Self::PlaceholderMismatch { token }
            }
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token } => {
                Self::UnexpectedPlaceholderToken { token }
            }
            TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { original } => {
                Self::PlaceholderNormalizationAmbiguous { original }
            }
            TranslationUnitRejectionReason::SourceResidual { fragment } => {
                Self::SourceResidual { fragment }
            }
        }
    }
}

struct ProtocolDiagnosticsWriteWire<'a>(&'a [TranslationProtocolDiagnostic]);

impl Serialize for ProtocolDiagnosticsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for diagnostic in self.0 {
            sequence.serialize_element(&ProtocolDiagnosticWriteWire::from(diagnostic))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProtocolDiagnosticWriteWire<'a> {
    NonStopFinish { reason: &'a str },
    InvalidResponse { message: &'a str },
    InvalidId { item_index: usize },
    UnknownId { item_index: usize, id: usize },
}

impl<'a> From<&'a TranslationProtocolDiagnostic> for ProtocolDiagnosticWriteWire<'a> {
    fn from(diagnostic: &'a TranslationProtocolDiagnostic) -> Self {
        match diagnostic {
            TranslationProtocolDiagnostic::NonStopFinish { reason } => {
                Self::NonStopFinish { reason }
            }
            TranslationProtocolDiagnostic::InvalidResponse { message } => {
                Self::InvalidResponse { message }
            }
            TranslationProtocolDiagnostic::InvalidId { item_index } => Self::InvalidId {
                item_index: *item_index,
            },
            TranslationProtocolDiagnostic::UnknownId { item_index, id } => Self::UnknownId {
                item_index: *item_index,
                id: *id,
            },
        }
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnresolvedTranslationWire {
    id: usize,
    locations: Vec<LogicalTextLocationWire>,
    reason: TranslationUnitRejectionReasonWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationUnitRejectionReasonWire {
    Missing,
    Duplicate,
    InvalidShape {
        message: String,
    },
    LineCountMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidLineText {
        line_index: usize,
    },
    BlankLineMismatch {
        line_index: usize,
        expected_blank: bool,
    },
    BlankTranslation,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch {
        token: String,
    },
    UnexpectedPlaceholderToken {
        token: String,
    },
    PlaceholderNormalizationAmbiguous {
        original: String,
    },
    SourceResidual {
        fragment: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProtocolDiagnosticWire {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    InvalidId { item_index: usize },
    UnknownId { item_index: usize, id: usize },
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

#[derive(Serialize)]
struct WriteBackPayloadWriteWire<'a> {
    layout_profile: LayoutProfileWire,
    output_root: &'a str,
    lua_executed: bool,
    summary: WriteBackSummaryWire,
    manual_layout_diagnostics: ManualLayoutDiagnosticsWriteWire<'a>,
}

impl<'a> TryFrom<&'a WriteBackRunLog> for WriteBackPayloadWriteWire<'a> {
    type Error = String;

    fn try_from(event: &'a WriteBackRunLog) -> Result<Self, Self::Error> {
        Ok(Self {
            layout_profile: LayoutProfileWire::from(event.layout_profile()),
            output_root: output_root_text_ref(event.output_root())?,
            lua_executed: event.lua_executed(),
            summary: WriteBackSummaryWire::from(event.summary()),
            manual_layout_diagnostics: ManualLayoutDiagnosticsWriteWire(
                event.manual_layout_diagnostics(),
            ),
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
    translated_units: usize,
    original_units: usize,
    auto_wrapped_units: usize,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
    manual_layout_units: usize,
}

impl From<StandardWriteBackSummary> for WriteBackSummaryWire {
    fn from(summary: StandardWriteBackSummary) -> Self {
        Self {
            translated_units: summary.translated_units,
            original_units: summary.original_units,
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

impl ManualLayoutDiagnosticWire {
    fn validate(&self) -> Result<(), String> {
        if self.locations.is_empty() {
            return Err("人工布局诊断必须关联至少一个逻辑文本位置".to_owned());
        }
        Ok(())
    }
}

struct ManualLayoutDiagnosticsWriteWire<'a>(&'a [ManualLayoutDiagnostic]);

impl Serialize for ManualLayoutDiagnosticsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for diagnostic in self.0 {
            sequence.serialize_element(&ManualLayoutDiagnosticWriteWire::from(diagnostic))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct ManualLayoutDiagnosticWriteWire<'a> {
    locations: LogicalTextLocationsWriteWire<'a>,
    region: LayoutRegionWire,
    max_fullwidth_chars: u32,
}

impl<'a> From<&'a ManualLayoutDiagnostic> for ManualLayoutDiagnosticWriteWire<'a> {
    fn from(diagnostic: &'a ManualLayoutDiagnostic) -> Self {
        Self {
            locations: LogicalTextLocationsWriteWire(diagnostic.locations()),
            region: LayoutRegionWire::from(diagnostic.region()),
            max_fullwidth_chars: diagnostic.max_fullwidth_chars().get(),
        }
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
    unit_role: TextUnitRoleWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TextUnitRoleWire {
    Scalar { field: String },
    DialogueSpeaker,
    DialogueBody,
    Choices,
    ScrollingText,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RpgMakerSourceWire {
    Data {
        file: String,
    },
    Map {
        map_id: MapIdWire,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

/// 审计 wire 中的 Map 身份。
///
/// 读写两侧都经过领域 `MapId`，避免拥有型校验模型在反序列化后短暂承载
/// `map_id = 0`，也避免写侧绕过正整数不变量。
#[derive(Clone, Copy, Debug)]
struct MapIdWire(MapId);

impl Serialize for MapIdWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0.get())
    }
}

impl<'de> Deserialize<'de> for MapIdWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        MapId::new(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RpgMakerLocationStepWire {
    ObjectKey { key: String },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

struct LogicalTextLocationsWriteWire<'a>(&'a [LogicalTextLocation]);

impl Serialize for LogicalTextLocationsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for location in self.0 {
            sequence.serialize_element(&LogicalTextLocationWriteWire::from(location))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct LogicalTextLocationWriteWire<'a> {
    group_location: RpgMakerLocationWriteWire<'a>,
    unit_role: TextUnitRoleWriteWire<'a>,
}

impl<'a> From<&'a LogicalTextLocation> for LogicalTextLocationWriteWire<'a> {
    fn from(location: &'a LogicalTextLocation) -> Self {
        Self {
            group_location: RpgMakerLocationWriteWire::from(location.group_location()),
            unit_role: TextUnitRoleWriteWire::from(location.role()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TextUnitRoleWriteWire<'a> {
    Scalar { field: &'a str },
    DialogueSpeaker,
    DialogueBody,
    Choices,
    ScrollingText,
}

impl<'a> From<&'a TextUnitRole> for TextUnitRoleWriteWire<'a> {
    fn from(role: &'a TextUnitRole) -> Self {
        match role {
            TextUnitRole::Scalar(field) => Self::Scalar {
                field: field.as_str(),
            },
            TextUnitRole::DialogueSpeaker => Self::DialogueSpeaker,
            TextUnitRole::DialogueBody => Self::DialogueBody,
            TextUnitRole::Choices => Self::Choices,
            TextUnitRole::ScrollingText => Self::ScrollingText,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RpgMakerLocationWriteWire<'a> {
    Value {
        source: RpgMakerSourceWriteWire<'a>,
        steps: RpgMakerLocationStepsWriteWire<'a>,
    },
    NoteTag {
        source: RpgMakerSourceWriteWire<'a>,
        container_steps: RpgMakerLocationStepsWriteWire<'a>,
        tag_name: &'a str,
        occurrence: usize,
    },
    CommentTag {
        source: RpgMakerSourceWriteWire<'a>,
        command_steps: RpgMakerLocationStepsWriteWire<'a>,
        tag_name: &'a str,
        occurrence: usize,
    },
}

impl<'a> From<&'a RpgMakerLocation> for RpgMakerLocationWriteWire<'a> {
    fn from(location: &'a RpgMakerLocation) -> Self {
        match location {
            RpgMakerLocation::Value { source, steps } => Self::Value {
                source: RpgMakerSourceWriteWire::from(source),
                steps: RpgMakerLocationStepsWriteWire(steps),
            },
            RpgMakerLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source: RpgMakerSourceWriteWire::from(source),
                container_steps: RpgMakerLocationStepsWriteWire(container_steps),
                tag_name,
                occurrence: *occurrence,
            },
            RpgMakerLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source: RpgMakerSourceWriteWire::from(source),
                command_steps: RpgMakerLocationStepsWriteWire(command_steps),
                tag_name,
                occurrence: *occurrence,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RpgMakerSourceWriteWire<'a> {
    Data {
        file: &'a str,
    },
    Map {
        map_id: MapIdWire,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: &'a str,
        parameter_name: &'a str,
    },
}

impl<'a> From<&'a RpgMakerSource> for RpgMakerSourceWriteWire<'a> {
    fn from(source: &'a RpgMakerSource) -> Self {
        match source {
            RpgMakerSource::Data(file) => Self::Data {
                file: file.file_name(),
            },
            RpgMakerSource::DataFile(file) => Self::Data {
                file: file.as_str(),
            },
            RpgMakerSource::Map(map_id) => Self::Map {
                map_id: MapIdWire(*map_id),
            },
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Self::PluginParameter {
                plugin_index: *plugin_index,
                plugin_name,
                parameter_name,
            },
        }
    }
}

struct RpgMakerLocationStepsWriteWire<'a>(&'a [RpgMakerLocationStep]);

impl Serialize for RpgMakerLocationStepsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for step in self.0 {
            sequence.serialize_element(&RpgMakerLocationStepWriteWire::from(step))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RpgMakerLocationStepWriteWire<'a> {
    ObjectKey { key: &'a str },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl<'a> From<&'a RpgMakerLocationStep> for RpgMakerLocationStepWriteWire<'a> {
    fn from(step: &'a RpgMakerLocationStep) -> Self {
        match step {
            RpgMakerLocationStep::ObjectKey(key) => Self::ObjectKey { key },
            RpgMakerLocationStep::ArrayIndex(index) => Self::ArrayIndex { index: *index },
            RpgMakerLocationStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

struct PathsWriteWire<'a>(&'a [PathBuf]);

impl Serialize for PathsWriteWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for path in self.0 {
            let path =
                output_root_text_ref(path).map_err(<S::Error as serde::ser::Error>::custom)?;
            sequence.serialize_element(path)?;
        }
        sequence.end()
    }
}

fn output_root_text_ref(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "写回输出路径无法无损表示为 UTF-8".to_owned())
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
    let mut comparison = CanonicalCompareWriter::new(bytes);
    serde_json::to_writer(&mut comparison, wire).map_err(|error| error.to_string())?;
    if !comparison.matches() {
        return Err("记录不是当前紧凑 UTF-8 wire".to_owned());
    }
    Ok(())
}

struct CanonicalCompareWriter<'a> {
    expected: &'a [u8],
    offset: usize,
    mismatch: bool,
}

impl<'a> CanonicalCompareWriter<'a> {
    const fn new(expected: &'a [u8]) -> Self {
        Self {
            expected,
            offset: 0,
            mismatch: false,
        }
    }

    fn matches(&self) -> bool {
        !self.mismatch && self.offset == self.expected.len()
    }
}

impl Write for CanonicalCompareWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let expected = self.expected.get(self.offset..).unwrap_or_default();
        let compared_length = expected.len().min(bytes.len());
        if expected[..compared_length] != bytes[..compared_length] || bytes.len() > expected.len() {
            self.mismatch = true;
        }
        self.offset = self.offset.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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

impl AuditPayloadValidation for TranslationPlanningUnresolvedPayloadWire {
    fn validate(&self) -> Result<(), String> {
        let message = match &self.failure.reason {
            TranslationPlanningFailureReasonWire::PlaceholderProtection { message }
            | TranslationPlanningFailureReasonWire::PlaceholderProjection { message } => message,
        };
        if message.trim().is_empty() {
            return Err("规划期未解决原因不能为空".to_owned());
        }
        Ok(())
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
                if task.confirmed_written_units != Some(expected_written) {
                    return Err(
                        "completed 的 confirmed_written_units 必须等于结构化写入逻辑单元数"
                            .to_owned(),
                    );
                }
            } else if task.confirmed_written_units.is_some() {
                return Err("未提交任务的 confirmed_written_units 必须为 null".to_owned());
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
    probe: &AuditEnvelopeProbe<'_>,
    expected_event: AuditEventKindWire,
    expected_command: Option<AuditCommandWire>,
) -> Result<(), String>
where
    P: AuditPayloadValidation + DeserializeOwned + Serialize,
{
    let payload: P =
        serde_json::from_str(probe.payload.get()).map_err(|error| error.to_string())?;
    if probe.event != expected_event {
        return Err("审计事件类型与 payload 不一致".to_owned());
    }
    if expected_command.is_some_and(|command| command != probe.command) {
        return Err("审计事件不属于当前命令".to_owned());
    }
    validate_recorded_at_utc(probe.recorded_at_utc)?;
    validate_uuid_v4(probe.event_id, "event_id")?;
    validate_uuid_v4(probe.run_id, "run_id")?;
    if probe.project.trim().is_empty() {
        return Err("project 不能为空".to_owned());
    }
    match probe.command {
        AuditCommandWire::Translate => {
            if probe
                .profile
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err("Translate 审计记录必须携带非空 profile".to_owned());
            }
        }
        AuditCommandWire::Init | AuditCommandWire::Extract | AuditCommandWire::WriteBack => {
            if probe.profile.is_some() {
                return Err("非 Translate 审计记录不得携带 profile".to_owned());
            }
        }
    }
    payload.validate()?;
    validate_canonical_wire(probe.payload.get().as_bytes(), &payload)?;
    validate_canonical_wire(
        bytes,
        &AuditEnvelopeRawWire {
            recorded_at_utc: probe.recorded_at_utc,
            event_id: probe.event_id,
            run_id: probe.run_id,
            engine: probe.engine,
            project: probe.project.as_ref(),
            command: probe.command,
            profile: probe.profile.as_deref(),
            event: probe.event,
            payload: probe.payload,
        },
    )
}

fn serialize_audit_envelope<P>(
    context: &AuditContext,
    event_id: EventId,
    recorded_at_utc: &str,
    event: AuditEventKindWire,
    payload: P,
    output: &mut Vec<u8>,
) -> Result<(), String>
where
    P: Serialize,
{
    serde_json::to_writer(
        output,
        &AuditEnvelopeWriteWire {
            recorded_at_utc,
            event_id: DisplayWriteWire(event_id),
            run_id: DisplayWriteWire(context.run_id()),
            engine: context.engine().into(),
            project: context.project(),
            command: context.command().into(),
            profile: context.profile(),
            event,
            payload,
        },
    )
    .map_err(|error| error.to_string())
}

impl JsonLineRecord for AuditRecord {
    fn serialize_into(self, recorded_at_utc: &str, output: &mut Vec<u8>) -> Result<(), String> {
        match &self.event {
            AuditEvent::RunStarted => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::RunStarted,
                EmptyPayloadWire::default(),
                output,
            ),
            AuditEvent::RunFinished { outcome } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::RunFinished,
                RunFinishedPayloadWire {
                    outcome: (*outcome).into(),
                },
                output,
            ),
            AuditEvent::TranslationTaskStarted {
                operation_id,
                task_index,
            } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::TranslationTaskStarted,
                TranslationTaskStartedPayloadWriteWire {
                    operation_id: DisplayWriteWire(*operation_id),
                    task_index: task_index.get(),
                },
                output,
            ),
            AuditEvent::TranslationTaskFinished {
                operation_id,
                result,
            } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::TranslationTaskFinished,
                TranslationTaskFinishedPayloadWriteWire {
                    operation_id: DisplayWriteWire(*operation_id),
                    result: TranslationTaskResultWriteWire::from(result),
                },
                output,
            ),
            AuditEvent::TranslationPlanningUnresolved { failure } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::TranslationPlanningUnresolved,
                TranslationPlanningUnresolvedPayloadWriteWire {
                    failure: TranslationPlanningFailureWriteWire::from(failure),
                },
                output,
            ),
            AuditEvent::WriteBackPublishStarted {
                operation_id,
                output_root,
            } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::WriteBackPublishStarted,
                WriteBackPublishStartedPayloadWriteWire {
                    operation_id: DisplayWriteWire(*operation_id),
                    output_root: output_root_text_ref(output_root)?,
                },
                output,
            ),
            AuditEvent::WriteBackPublishFinished {
                operation_id,
                result,
            } => serialize_audit_envelope(
                &self.context,
                self.event_id,
                recorded_at_utc,
                AuditEventKindWire::WriteBackPublishFinished,
                WriteBackPublishFinishedPayloadWriteWire {
                    operation_id: DisplayWriteWire(*operation_id),
                    result: WriteBackPublishResultWriteWire::try_from(result)?,
                },
                output,
            ),
        }
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        let probe: AuditEnvelopeProbe<'_> =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        match probe.event {
            AuditEventKindWire::RunStarted => validate_payload::<EmptyPayloadWire>(
                bytes,
                &probe,
                AuditEventKindWire::RunStarted,
                None,
            ),
            AuditEventKindWire::RunFinished => validate_payload::<RunFinishedPayloadWire>(
                bytes,
                &probe,
                AuditEventKindWire::RunFinished,
                None,
            ),
            AuditEventKindWire::TranslationTaskStarted => {
                validate_payload::<TranslationTaskStartedPayloadWire>(
                    bytes,
                    &probe,
                    AuditEventKindWire::TranslationTaskStarted,
                    Some(AuditCommandWire::Translate),
                )
            }
            AuditEventKindWire::TranslationTaskFinished => {
                validate_payload::<TranslationTaskFinishedPayloadWire>(
                    bytes,
                    &probe,
                    AuditEventKindWire::TranslationTaskFinished,
                    Some(AuditCommandWire::Translate),
                )
            }
            AuditEventKindWire::TranslationPlanningUnresolved => {
                validate_payload::<TranslationPlanningUnresolvedPayloadWire>(
                    bytes,
                    &probe,
                    AuditEventKindWire::TranslationPlanningUnresolved,
                    Some(AuditCommandWire::Translate),
                )
            }
            AuditEventKindWire::WriteBackPublishStarted => {
                validate_payload::<WriteBackPublishStartedPayloadWire>(
                    bytes,
                    &probe,
                    AuditEventKindWire::WriteBackPublishStarted,
                    Some(AuditCommandWire::WriteBack),
                )
            }
            AuditEventKindWire::WriteBackPublishFinished => {
                validate_payload::<WriteBackPublishFinishedPayloadWire>(
                    bytes,
                    &probe,
                    AuditEventKindWire::WriteBackPublishFinished,
                    Some(AuditCommandWire::WriteBack),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::rpg_maker::model::TextUnitContent;
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::TextGroupKind;
    use crate::rpg_maker::translate::standard::{
        NonEmptyTaskItems, TranslationPlanningFailure, TranslationPlanningFailureReason,
        TranslationTaskOutcome, TranslationTaskOutcomeContext, TranslationUnitIdentity,
        UnresolvedTranslationUnit,
    };

    fn run_id() -> RunId {
        RunId::from_uuid(Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0000))
    }

    fn config() -> JsonLinesStreamConfig {
        JsonLinesStreamConfig::new(8, Duration::from_secs(1), 16_384, 65_536, 2)
            .expect("测试账本配置应合法")
    }

    fn map_source_wire() -> RpgMakerSourceWire {
        RpgMakerSourceWire::Map {
            map_id: MapIdWire(MapId::new(1).expect("测试 Map ID 应为正整数")),
        }
    }

    fn map_value_location_wire() -> LogicalTextLocationWire {
        LogicalTextLocationWire {
            group_location: RpgMakerLocationWire::Value {
                source: map_source_wire(),
                steps: Vec::new(),
            },
            unit_role: TextUnitRoleWire::Scalar {
                field: "text".to_owned(),
            },
        }
    }

    fn map_note_tag_location_wire() -> LogicalTextLocationWire {
        LogicalTextLocationWire {
            group_location: RpgMakerLocationWire::NoteTag {
                source: map_source_wire(),
                container_steps: Vec::new(),
                tag_name: "Name".to_owned(),
                occurrence: 0,
            },
            unit_role: TextUnitRoleWire::Scalar {
                field: "text".to_owned(),
            },
        }
    }

    fn map_comment_tag_location_wire() -> LogicalTextLocationWire {
        LogicalTextLocationWire {
            group_location: RpgMakerLocationWire::CommentTag {
                source: map_source_wire(),
                command_steps: Vec::new(),
                tag_name: "Name".to_owned(),
                occurrence: 0,
            },
            unit_role: TextUnitRoleWire::Scalar {
                field: "text".to_owned(),
            },
        }
    }

    fn translation_task_wire(confirmed_written_units: Option<usize>) -> TranslationTaskWire {
        TranslationTaskWire {
            task_index: 0,
            status: TranslationTaskStatusWire::Partial,
            attempts: 1,
            provider_request_id: None,
            provider_response_id: None,
            finish_reason: None,
            final_response_usage: None,
            accepted_decisions: 1,
            confirmed_written_units,
            accepted: vec![AcceptedTranslationWire {
                id: 0,
                leader: map_note_tag_location_wire(),
                propagation_targets: vec![map_comment_tag_location_wire()],
            }],
            unresolved: vec![UnresolvedTranslationWire {
                id: 1,
                locations: vec![map_value_location_wire()],
                reason: TranslationUnitRejectionReasonWire::Missing,
            }],
            diagnostics: Vec::new(),
        }
    }

    fn serialize_test_payload<P>(
        context: &AuditContext,
        event: AuditEventKindWire,
        payload: P,
    ) -> Vec<u8>
    where
        P: Serialize,
    {
        let mut bytes = Vec::new();
        serialize_audit_envelope(
            context,
            EventId::from_uuid(Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0001)),
            "2026-07-20T12:34:56.789Z",
            event,
            payload,
            &mut bytes,
        )
        .expect("测试审计 payload 应可序列化");
        bytes
    }

    fn assert_positive_maps_pass_and_each_zero_is_rejected(
        case: &str,
        bytes: &[u8],
        expected_map_count: usize,
    ) {
        AuditRecord::validate(bytes)
            .unwrap_or_else(|error| panic!("{case} 的正 Map ID 应通过校验：{error}"));

        let wire = std::str::from_utf8(bytes).expect("审计 wire 应为 UTF-8");
        let needle = "\"map_id\":1";
        let positions = wire
            .match_indices(needle)
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        assert_eq!(
            positions.len(),
            expected_map_count,
            "{case} 的 Map 来源覆盖数应固定"
        );

        for (map_index, position) in positions.into_iter().enumerate() {
            let mut invalid = wire.to_owned();
            let digit = position + needle.len() - 1;
            invalid.replace_range(digit..digit + 1, "0");
            let error = match AuditRecord::validate(invalid.as_bytes()) {
                Ok(()) => panic!("{case} 的第 {map_index} 个 map_id=0 不得通过"),
                Err(error) => error,
            };
            assert!(
                error.contains("RPG Maker map ID 必须是正 u32 整数"),
                "{case} 的第 {map_index} 个零 Map ID 应由 MapId 边界拒绝，实际错误：{error}"
            );
        }
    }

    #[test]
    fn borrowed_writer_preserves_the_exact_audit_wire() {
        let event_id =
            EventId::from_uuid(Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0001));
        let operation_id =
            OperationId::from_uuid(Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0002));
        let record = AuditRecord {
            context: Arc::new(AuditContext::translate(
                run_id(),
                RpgMakerEngine::Mv,
                "demo",
                "quality",
            )),
            event_id,
            event: AuditEvent::TranslationTaskStarted {
                operation_id,
                task_index: StandardTranslationTaskIndex::new(3),
            },
        };
        let mut bytes = Vec::new();

        record
            .serialize_into("2026-07-20T12:34:56.789Z", &mut bytes)
            .expect("借用 wire 应可直接写入 worker 缓冲");

        let expected = r#"{"recorded_at_utc":"2026-07-20T12:34:56.789Z","event_id":"550e8400-e29b-41d4-a716-446655440001","run_id":"550e8400-e29b-41d4-a716-446655440000","engine":"mv","project":"demo","command":"translate","profile":"quality","event":"translation_task_started","payload":{"operation_id":"550e8400-e29b-41d4-a716-446655440002","task_index":3}}"#;
        assert_eq!(bytes, expected.as_bytes());
        AuditRecord::validate(&bytes).expect("固定 wire 应通过强类型校验");

        let noncanonical_payload = expected.replacen("\"payload\":{", "\"payload\":{ ", 1);
        assert_eq!(
            AuditRecord::validate(noncanonical_payload.as_bytes())
                .expect_err("payload 内部空白不得绕过规范 wire 校验"),
            "记录不是当前紧凑 UTF-8 wire"
        );
    }

    #[test]
    fn canonical_comparison_rejects_short_long_and_middle_differences() {
        let wire = json!({ "value": 7 });
        validate_canonical_wire(br#"{"value":7}"#, &wire).expect("完全相同的 wire 应通过");
        for candidate in [
            br#"{"value":7"#.as_slice(),
            br#"{"value":7} "#.as_slice(),
            br#"{"value":8}"#.as_slice(),
        ] {
            assert_eq!(
                validate_canonical_wire(candidate, &wire).expect_err("字节差异必须被拒绝"),
                "记录不是当前紧凑 UTF-8 wire"
            );
        }
    }

    #[test]
    fn every_location_bearing_audit_payload_requires_positive_map_ids() {
        let translate_context =
            AuditContext::translate(run_id(), RpgMakerEngine::Mz, "demo", "quality");
        let planning = serialize_test_payload(
            &translate_context,
            AuditEventKindWire::TranslationPlanningUnresolved,
            TranslationPlanningUnresolvedPayloadWire {
                failure: TranslationPlanningFailureWire {
                    location: map_value_location_wire(),
                    reason: TranslationPlanningFailureReasonWire::PlaceholderProjection {
                        message: "投影失败".to_owned(),
                    },
                },
            },
        );
        assert_positive_maps_pass_and_each_zero_is_rejected(
            "translation_planning_unresolved",
            &planning,
            1,
        );

        let operation_id = "550e8400-e29b-41d4-a716-446655440002".to_owned();
        for (case, result) in [
            (
                "translation_task_finished/completed",
                TranslationTaskResultWire::Completed {
                    task: translation_task_wire(Some(2)),
                },
            ),
            (
                "translation_task_finished/commit_failed",
                TranslationTaskResultWire::CommitFailed {
                    task: translation_task_wire(None),
                },
            ),
            (
                "translation_task_finished/not_committed",
                TranslationTaskResultWire::NotCommitted {
                    task: translation_task_wire(None),
                },
            ),
        ] {
            let task = serialize_test_payload(
                &translate_context,
                AuditEventKindWire::TranslationTaskFinished,
                TranslationTaskFinishedPayloadWire {
                    operation_id: operation_id.clone(),
                    result,
                },
            );
            assert_positive_maps_pass_and_each_zero_is_rejected(case, &task, 3);
        }

        let write_back_context = AuditContext::write_back(run_id(), RpgMakerEngine::Mv, "demo");
        let write_back = serialize_test_payload(
            &write_back_context,
            AuditEventKindWire::WriteBackPublishFinished,
            WriteBackPublishFinishedPayloadWire {
                operation_id,
                result: WriteBackPublishResultWire::Published {
                    write_back: WriteBackPayloadWire {
                        layout_profile: LayoutProfileWire {
                            dialogue_body_max_fullwidth_chars: 24,
                            scrolling_text_max_fullwidth_chars: 24,
                            help_description_max_fullwidth_chars: 24,
                        },
                        output_root: "output".to_owned(),
                        lua_executed: false,
                        summary: WriteBackSummaryWire {
                            translated_units: 1,
                            original_units: 0,
                            auto_wrapped_units: 0,
                            inserted_line_breaks: 0,
                            inserted_fullwidth_indents: 0,
                            manual_layout_units: 1,
                        },
                        manual_layout_diagnostics: vec![ManualLayoutDiagnosticWire {
                            locations: vec![map_comment_tag_location_wire()],
                            region: LayoutRegionWire::DialogueBody,
                            max_fullwidth_chars: 24,
                        }],
                    },
                },
            },
        );
        assert_positive_maps_pass_and_each_zero_is_rejected(
            "write_back_publish_finished/published",
            &write_back,
            1,
        );
    }

    #[test]
    fn borrowed_translation_payload_matches_the_strict_owned_wire_model() {
        let task_index = StandardTranslationTaskIndex::new(4);
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(
                RpgMakerSource::map(2),
                vec![
                    RpgMakerLocationStep::key("events"),
                    RpgMakerLocationStep::index(7),
                ],
            ),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["原文".to_owned()]),
            r#"{"speaker":null}"#,
        );
        let outcome = TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                task_index,
                NonZeroUsize::new(2).expect("测试尝试次数应非零"),
                vec![TranslationProtocolDiagnostic::InvalidResponse {
                    message: "响应形状错误".to_owned(),
                }],
            ),
            final_response: None,
            reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                message: "上游暂时不可用".to_owned(),
            },
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    9,
                    identity,
                    Vec::new(),
                    TranslationUnitRejectionReason::InvalidShape {
                        message: "缺少 translation".to_owned(),
                    },
                ),
                Vec::new(),
            ),
        };
        let record = AuditRecord {
            context: Arc::new(AuditContext::translate(
                run_id(),
                RpgMakerEngine::Mz,
                "demo",
                "quality",
            )),
            event_id: EventId::from_uuid(Uuid::from_u128(
                0x550e_8400_e29b_41d4_a716_4466_5544_0001,
            )),
            event: AuditEvent::TranslationTaskFinished {
                operation_id: OperationId::from_uuid(Uuid::from_u128(
                    0x550e_8400_e29b_41d4_a716_4466_5544_0002,
                )),
                result: TranslationTaskAuditResult::Completed(
                    TranslationTaskLogRecord::from_outcome(outcome),
                ),
            },
        };
        let mut bytes = Vec::new();

        record
            .serialize_into("2026-07-20T12:34:56.789Z", &mut bytes)
            .expect("复杂翻译终态应可借用序列化");

        AuditRecord::validate(&bytes)
            .expect("借用序列化结果必须逐字节等于严格拥有型 wire 的规范输出");
    }

    #[test]
    fn planning_unresolved_wire_has_location_and_no_llm_task_protocol_fields() {
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Rules,
            TextGroupKind::PluginParameter,
            RpgMakerLocation::value(
                RpgMakerSource::plugin_parameter(2, "QuestLog", "title"),
                Vec::new(),
            ),
            TextUnitRole::Scalar(
                crate::rpg_maker::model::ScalarFieldKey::new("title").expect("字段应合法"),
            ),
            TextUnitContent::Value("翻訳<BAD>".to_owned()),
            "{}",
        );
        let record = AuditRecord {
            context: Arc::new(AuditContext::translate(
                run_id(),
                RpgMakerEngine::Mz,
                "demo",
                "quality",
            )),
            event_id: EventId::from_uuid(Uuid::from_u128(
                0x550e_8400_e29b_41d4_a716_4466_5544_0001,
            )),
            event: AuditEvent::TranslationPlanningUnresolved {
                failure: TranslationPlanningFailure::new(
                    identity,
                    TranslationPlanningFailureReason::PlaceholderProtection {
                        message: "实际保护跨度冲突".to_owned(),
                    },
                ),
            },
        };
        let mut bytes = Vec::new();

        record
            .serialize_into("2026-07-20T12:34:56.789Z", &mut bytes)
            .expect("规划期未解决事件应可序列化");
        AuditRecord::validate(&bytes).expect("规划期未解决 wire 应通过强类型校验");
        let value: Value = serde_json::from_slice(&bytes).expect("审计 JSON 应有效");
        assert_eq!(value["event"], "translation_planning_unresolved");
        assert_eq!(
            value["payload"]["failure"]["reason"]["kind"],
            "placeholder_protection"
        );
        assert!(value["payload"]["failure"].get("id").is_none());
        assert!(value["payload"]["failure"].get("attempts").is_none());
    }

    fn dialogue_location() -> LogicalTextLocation {
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
            TextUnitRole::DialogueBody,
        )
    }

    #[test]
    fn manual_layout_wire_contains_one_or_more_logical_locations_and_no_physical_unit() {
        let diagnostic = ManualLayoutDiagnostic::for_test(
            vec![dialogue_location()],
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            crate::rpg_maker::project::MaxFullwidthChars::new(24).expect("测试行宽应合法"),
        );

        let wire = serde_json::to_value(ManualLayoutDiagnosticWriteWire::from(&diagnostic))
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
                        "unit_role": { "kind": "dialogue_body" }
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
    fn unit_location_wire_serializes_current_no_index_roles() {
        for (role, expected_kind) in [
            (TextUnitRole::DialogueBody, "dialogue_body"),
            (TextUnitRole::Choices, "choices"),
            (TextUnitRole::ScrollingText, "scrolling_text"),
        ] {
            let location = LogicalTextLocation::new(
                RpgMakerLocation::value(RpgMakerSource::map(3), Vec::new()),
                role,
            );
            let wire = serde_json::to_value(LogicalTextLocationWriteWire::from(&location))
                .expect("语义单元位置应可序列化");

            assert_eq!(wire["unit_role"], json!({ "kind": expected_kind }));
        }
    }

    #[test]
    fn line_validation_rejections_preserve_structured_counts_and_indexes() {
        let mismatch = TranslationUnitRejectionReason::LineCountMismatch {
            expected: 2,
            actual: 1,
        };
        assert_eq!(
            serde_json::to_value(TranslationUnitRejectionReasonWriteWire::from(&mismatch))
                .expect("行数拒绝应可序列化"),
            json!({ "kind": "line_count_mismatch", "expected": 2, "actual": 1 })
        );

        let invalid_line = TranslationUnitRejectionReason::InvalidLineText { line_index: 3 };
        assert_eq!(
            serde_json::to_value(TranslationUnitRejectionReasonWriteWire::from(&invalid_line))
                .expect("非法行拒绝应可序列化"),
            json!({ "kind": "invalid_line_text", "line_index": 3 })
        );

        let blank_mismatch = TranslationUnitRejectionReason::BlankLineMismatch {
            line_index: 1,
            expected_blank: true,
        };
        assert_eq!(
            serde_json::to_value(TranslationUnitRejectionReasonWriteWire::from(
                &blank_mismatch,
            ))
            .expect("空槽拒绝应可序列化"),
            json!({
                "kind": "blank_line_mismatch",
                "line_index": 1,
                "expected_blank": true
            })
        );
    }

    #[test]
    fn invalid_id_diagnostic_records_only_the_item_index() {
        let diagnostic = TranslationProtocolDiagnostic::InvalidId { item_index: 4 };
        let wire = serde_json::to_value(ProtocolDiagnosticWriteWire::from(&diagnostic))
            .expect("非法 ID 诊断应可序列化");

        assert_eq!(wire, json!({ "kind": "invalid_id", "item_index": 4 }));
        assert!(wire.get("id").is_none(), "非法原始键不得进入审计账本");
        assert!(serde_json::from_value::<ProtocolDiagnosticWire>(wire).is_ok());
    }

    #[test]
    fn unit_counter_wires_use_current_field_names() {
        let current_task = json!({
            "task_index": 0,
            "status": { "kind": "complete" },
            "attempts": 1,
            "provider_request_id": null,
            "provider_response_id": null,
            "finish_reason": null,
            "final_response_usage": null,
            "accepted_decisions": 0,
            "confirmed_written_units": 0,
            "accepted": [],
            "unresolved": [],
            "diagnostics": []
        });
        assert!(serde_json::from_value::<TranslationTaskWire>(current_task).is_ok());

        let current_summary = json!({
            "translated_units": 2,
            "original_units": 3,
            "auto_wrapped_units": 1,
            "inserted_line_breaks": 1,
            "inserted_fullwidth_indents": 0,
            "manual_layout_units": 0
        });
        assert!(serde_json::from_value::<WriteBackSummaryWire>(current_summary).is_ok());
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
    async fn escaped_profile_can_be_validated_during_consecutive_appends() {
        let directory = tempdir().expect("临时目录应可创建");
        let (ledger, finalizer) =
            JsonLinesAuditLedger::start(directory.path().to_path_buf(), config())
                .expect("审计账本应可启动");
        let profile = "quality\"x\\y";
        let run = ledger.bind(AuditContext::translate(
            run_id(),
            RpgMakerEngine::Mv,
            "demo",
            profile,
        ));
        let operation_id = run.new_operation_id().expect("操作身份应可生成");

        run.append(AuditEvent::TranslationTaskStarted {
            operation_id,
            task_index: StandardTranslationTaskIndex::new(0),
        })
        .await
        .expect("首条含转义 profile 的记录应持久化");
        run.append(AuditEvent::TranslationTaskFinished {
            operation_id,
            result: TranslationTaskAuditResult::ExecutionFailed {
                task_index: StandardTranslationTaskIndex::new(0),
            },
        })
        .await
        .expect("续写时应能重新校验含转义 profile 的现存记录");
        finalizer.finalize().await.expect("账本应可排空");

        let bytes =
            std::fs::read(directory.path().join("audit.jsonl")).expect("统一活动账本应存在");
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                AuditRecord::validate(line).expect("含转义 profile 的记录应保持规范 wire");
                serde_json::from_slice::<Value>(line).expect("审计记录应是 JSON")
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record["profile"] == profile));
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
        let terminal_line = bytes
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .expect("应存在发布终态");
        AuditRecord::validate(terminal_line).expect("借用路径列表 wire 应保持严格规范");
        let terminal = serde_json::from_slice::<Value>(terminal_line).expect("终态应是 JSON");
        assert_eq!(terminal["event"], "write_back_publish_finished");
        assert_eq!(terminal["payload"]["result"]["kind"], "recovery_required");
        assert_eq!(
            terminal["payload"]["result"]["recovery_artifacts"][0],
            recovery_artifacts[0].to_str().expect("测试路径应是 UTF-8")
        );
    }
}
