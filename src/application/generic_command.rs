//! Generic JSONL 命令的生产装配。
//!
//! 本模块只编排 Generic 纵向流程。JSONL、动态 Extract、去重、译文状态和往返验证
//! 由 `generic` 领域负责；文件、CPU、LLM、租约和目录发布使用公共运行能力。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::FuturesOrdered;
use futures_util::{FutureExt, StreamExt};
use rayon::prelude::*;
use rusqlite::Connection;
use time::OffsetDateTime;

use super::command::{
    ActiveProjectLog, CommandLogStart, PendingProjectLog, ProjectLogLuaPrintSink,
    ProjectLuaSummaryGuard, TerminationSignals, start_command_log,
};
use super::config::{
    ConfiguredGenericCommand, ConfiguredGenericWriteBackCommand, ConfiguredProjectLuaCommand,
    ConfiguredTranslateCommand,
};
use super::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError,
    assemble_translation_system_prompt_with_cancellation,
    ensure_no_prompt_template_variables_with_cancellation, parse_prompt_resource_with_cancellation,
    read_unparsed_prompt_resource, render_system_prompt_template_with_cancellation,
    translation_prompt_resource_paths,
};
use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::llm_request::{
    AsyncDelay, LlmRequestExecutionOutcome, LlmRequestRetryPolicy, execute_llm_request_with_retry,
};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
#[cfg(test)]
use crate::generic::build_write_back_candidate;
use crate::generic::{
    AutomaticStateResources, CancellableTextMap, CommitTranslationsOutcome, ExtractOutcome,
    GenericCompiledPlaceholderRules, GenericInitRequest, GenericPlaceholderService,
    GenericPlanningError, GenericProject, GenericProjectError, GenericProjectStore,
    GenericProtectedText, GenericStoredSnapshot, GenericTaskRecordDocument, GenericTaskRecordIssue,
    GenericTaskRecordState, GenericTaskResponseRecord, GenericUnitKey, GenericUnitMap,
    GenericWriteBackCandidate, GenericWriteBackError, PlannedTask, PlanningUnit, ResponseProblem,
    TranslationAcceptance, TranslationPlan, TranslationWrite, ValidatedReuse,
    accept_parsed_response_with_cancellation, build_write_back_candidate_with_cancellation,
    current_translation_for_stored_with_cancellation,
    ensure_input_fingerprints_current_with_cancellation,
    plan_translation_with_validator_and_cancellation,
    terminology_hit_fingerprint_with_cancellation,
    validate_materialized_write_back_file_with_cancellation,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::language::{
    LanguageAnalysis, LanguageModule, LanguageOperationCancelled, LanguageText, LanguageTextSegment,
};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity, LlmFinishReason,
};
#[cfg(not(test))]
use crate::progress::ProgressObserver;
use crate::progress::{
    ProgressMode, ProgressSnapshot, TerminalProgress, TerminalProgressFailures,
    TerminalProgressObserver,
};
use crate::project_lease::{
    ProjectCommandLeaseError, ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaDatabasePrerequisiteError, ProjectLuaFailure,
    ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError, ProjectLuaRunRequest,
    ProjectLuaSqliteError, compile_project_lua_program_with_cancellation,
    fingerprint_project_lua_program_with_cancellation, generic_project_lua_adapter,
    run_project_lua,
};
use crate::project_name::ProjectName;
use crate::runtime::cpu::{CpuExecutorUnavailable, RayonCpuExecutor};
use crate::runtime::filesystem::{
    SystemDirectoryPublisher, SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemError,
};
use crate::runtime::llm::OpenAiChatCompletionExecutor;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLog, ProjectLogCode, ProjectLogContext, ProjectLogEvent, ProjectLogLevel,
    ProjectLogPayload, ProjectLogRunOutcome, ProjectLogTaskOutcome, ProjectLogger,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent, DirectorySourceMapping,
    DirectoryStageRequest, FileReader, ReadFileError, RecoverableDirectoryPublisher,
    StagingCleanupFailure,
};
use crate::translation::placeholder::PlaceholderRuleCompilationError;
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::placeholder_token;
use crate::translation::planning_resource::{
    CompiledTerminology, PlaceholderDefinitionError, TerminologyDefinitionError,
    TranslationPlanningResourceReader, TranslationPlanningResourceReadingError,
    TranslationPlanningResourceReadingService,
};
use crate::translation::task_planning::TaskId;
use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::translation::user_message::{
    TranslationReturnType, TranslationUserGroup, TranslationUserMessage,
    TranslationUserTerminology, TranslationUserUnit, render_translation_user_message,
};
#[cfg(test)]
use crate::translation_protocol::parse_translation_response;
use crate::translation_protocol::{
    ParsedTranslationResponse, TranslationResponseMode, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind, parse_translation_response_with_cancellation,
};

const GENERIC_ENGINE_NAME: &str = "generic";
const WRITE_BACK_SCRATCH_PREFIX: &str = ".generic-write-back-";
const WRITE_BACK_PUBLICATION_CANCELLABLE: u8 = 0;
const WRITE_BACK_PUBLICATION_CANCELLED: u8 = 1;
const WRITE_BACK_PUBLICATION_STARTED: u8 = 2;

/// 在合作取消与目录发布之间建立唯一、不可逆的先后决定。
///
/// 取消先取得状态时，候选仍可安全丢弃；发布先取得状态时，目录发布根已经接管候选，
/// 后续信号只等待它形成明确终态，不能再把目录交换留在中间状态。
#[derive(Clone, Default)]
struct GenericWriteBackPublicationGate {
    state: Arc<AtomicU8>,
}

impl GenericWriteBackPublicationGate {
    fn request_cancellation(&self) -> bool {
        self.state
            .compare_exchange(
                WRITE_BACK_PUBLICATION_CANCELLABLE,
                WRITE_BACK_PUBLICATION_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn begin_publication(&self) -> bool {
        self.state
            .compare_exchange(
                WRITE_BACK_PUBLICATION_CANCELLABLE,
                WRITE_BACK_PUBLICATION_STARTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

fn begin_generic_write_back_publication(
    gate: &GenericWriteBackPublicationGate,
    publication_started: impl FnOnce(),
) -> bool {
    if !gate.begin_publication() {
        return false;
    }
    publication_started();
    true
}

/// Generic 纵向切片能够确认的实时阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenericProgressPhase {
    Initializing,
    Extracting,
    PlanningTranslation,
    ConfirmedTasks,
    NoModelWork,
    RunningLua,
    PreparingWriteBack,
    PublishingWriteBack,
}

struct GenericTerminalProgress {
    terminal: TerminalProgress<GenericProgressPhase>,
    safe_stopping: String,
    finalizing: String,
}

impl GenericTerminalProgress {
    fn observer(&self) -> TerminalProgressObserver<GenericProgressPhase> {
        self.terminal.observer()
    }

    fn safe_stopping(&self) {
        defer_generic_terminal_progress_status(
            self.terminal.safe_stopping(self.safe_stopping.clone()),
        );
    }

    fn finalizing(&self) {
        defer_generic_terminal_progress_status(self.terminal.finalizing(self.finalizing.clone()));
    }

    fn finish(self) -> Result<(), TerminalProgressFailures> {
        self.terminal.finish()
    }
}

fn defer_generic_terminal_progress_status(result: Result<(), TerminalProgressFailures>) {
    if let Err(failures) = result {
        // 健康状态仍由 `TerminalProgress` 持有，最终 `finish` 会再次返回全部失败。
        debug_assert!(!failures.failures().is_empty());
    }
}

fn record_generic_terminal_progress_failures(
    result: Result<(), TerminalProgressFailures>,
    shutdown_errors: &mut Vec<GenericShutdownError>,
) {
    if let Err(failures) = result {
        shutdown_errors.extend(
            failures
                .failures()
                .iter()
                .cloned()
                .map(|failure| GenericShutdownError::new("terminal progress", failure)),
        );
    }
}

fn generic_terminal_progress(mode: ProgressMode, locale: UiLocale) -> GenericTerminalProgress {
    let localizer = UiLocalizer::new(locale);
    let initializing = localizer.format(UiMessage::ProgressGenericInit);
    let extracting = localizer.format(UiMessage::ProgressGenericExtract);
    let planning = localizer.format(UiMessage::ProgressTranslatePlanning);
    let confirmed = localizer.format(UiMessage::ProgressTranslateConfirmed);
    let no_work = localizer.format(UiMessage::ProgressTranslateNoWork);
    let lua = localizer.format(UiMessage::ProgressProjectLua);
    let preparing_write_back = localizer.format(UiMessage::ProgressWriteBackPlanning);
    let publishing_write_back = localizer.format(UiMessage::ProgressWriteBackPublish);
    let terminal = TerminalProgress::stderr(mode, move |phase| match phase {
        GenericProgressPhase::Initializing => initializing.clone(),
        GenericProgressPhase::Extracting => extracting.clone(),
        GenericProgressPhase::PlanningTranslation => planning.clone(),
        GenericProgressPhase::ConfirmedTasks => confirmed.clone(),
        GenericProgressPhase::NoModelWork => no_work.clone(),
        GenericProgressPhase::RunningLua => lua.clone(),
        GenericProgressPhase::PreparingWriteBack => preparing_write_back.clone(),
        GenericProgressPhase::PublishingWriteBack => publishing_write_back.clone(),
    });
    GenericTerminalProgress {
        terminal,
        safe_stopping: localizer.format(UiMessage::ProgressSafeStopping),
        finalizing: localizer.format(UiMessage::ProgressFinalizing),
    }
}

/// Generic 命令成功完成后的类型化结果。
#[derive(Clone, Debug)]
pub(crate) enum GenericCommandOutput {
    Init {
        project: GenericProject,
    },
    Extract {
        project: ProjectName,
        outcome: ExtractOutcome,
    },
    Translate {
        project: ProjectName,
        profile_id: String,
        summary: GenericTranslationSummary,
    },
    WriteBack {
        project: ProjectName,
        output_root: PathBuf,
        translated_units: usize,
        retained_source_units: usize,
    },
    Lua {
        project: ProjectName,
    },
}

/// 一次 Generic Translate 的正常业务结果。
///
/// 模型请求不可用、响应部分无效和 CAS 冲突都属于可继续的部分结果，不升级为命令错误。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GenericTranslationSummary {
    pub(crate) total_tasks: usize,
    pub(crate) complete_tasks: usize,
    pub(crate) partial_tasks: usize,
    pub(crate) unavailable_tasks: usize,
    pub(crate) cleared_units: usize,
    pub(crate) reused_units: usize,
    pub(crate) accepted_units: usize,
    pub(crate) written_units: usize,
    pub(crate) conflicted_units: usize,
    pub(crate) response_problems: usize,
}

/// Generic 命令的进程级终态。
pub(crate) enum GenericCommandRunResult {
    Succeeded(GenericCommandOutput),
    Interrupted,
    Failed(GenericCommandError),
}

/// Generic 自有业务结果与公共运行根收尾结果。
pub(crate) struct GenericCommandRunReport {
    pub(crate) result: GenericCommandRunResult,
    pub(crate) shutdown_errors: Vec<GenericShutdownError>,
    pub(crate) pending_project_log: Option<PendingProjectLog>,
}

/// 一个运行根在业务 future 完成后关闭失败。
#[derive(Debug)]
pub(crate) struct GenericShutdownError {
    component: &'static str,
    detail: String,
    diagnostic: SafeDiagnostic,
}

impl GenericShutdownError {
    fn new<E>(component: &'static str, source: E) -> Self
    where
        E: fmt::Display + SafeDiagnosticSource,
    {
        let diagnostic = source
            .safe_diagnostic_source(
                DiagnosticStage::Shutdown,
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            )
            .with_recovery(RecoveryFact::component(component));
        Self {
            component,
            detail: source.to_string(),
            diagnostic,
        }
    }

    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        self.diagnostic.clone()
    }
}

impl fmt::Display for GenericShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} 关闭失败：{}", self.component, self.detail)
    }
}

impl Error for GenericShutdownError {}

/// Generic 命令仍掌握完整阶段时建立的具体失败。
#[derive(Debug)]
pub(crate) enum GenericCommandError {
    Cancelled,
    Operation {
        stage: &'static str,
        diagnostic: SafeDiagnostic,
        source: Box<dyn Error + Send + Sync>,
    },
    Signal {
        source: io::Error,
        operation: Option<Box<GenericCommandError>>,
        state_applied: bool,
    },
    PublishDiscard {
        operation: Box<GenericCommandError>,
        discard: String,
        recovery_paths: Vec<PathBuf>,
    },
}

impl GenericCommandError {
    fn operation(stage: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Operation {
            stage,
            diagnostic: generic_operation_diagnostic(stage),
            source: Box::new(source),
        }
    }

    fn diagnosed(
        stage: &'static str,
        source: impl Error + Send + Sync + 'static,
        diagnostic: SafeDiagnostic,
    ) -> Self {
        Self::Operation {
            stage,
            diagnostic,
            source: Box::new(source),
        }
    }

    fn message(stage: &'static str, detail: impl Into<String>) -> Self {
        Self::operation(stage, MessageError(detail.into()))
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Cancelled => SafeDiagnostic::new(
                DiagnosticCode::ProjectUnavailable,
                DiagnosticStage::Shutdown,
                DiagnosticSubject::command("generic"),
                DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            ),
            Self::Operation { diagnostic, .. } => diagnostic.clone(),
            Self::Signal {
                source,
                operation,
                state_applied,
            } => {
                let impact = if *state_applied {
                    DiagnosticImpact::StateAppliedFinalizationFailed
                } else {
                    operation
                        .as_ref()
                        .map_or(DiagnosticImpact::Unchanged, |error| {
                            error.safe_diagnostic().impact
                        })
                };
                SafeDiagnostic::io(
                    DiagnosticCode::SignalRegistration,
                    DiagnosticStage::Shutdown,
                    DiagnosticSubject::component("Windows control signal"),
                    "receive_signal",
                    source,
                    impact,
                    DiagnosticAction::Retry,
                )
            }
            Self::PublishDiscard { recovery_paths, .. } => {
                let mut diagnostic = SafeDiagnostic::new(
                    DiagnosticCode::WriteBackDiscard,
                    DiagnosticStage::Publication,
                    DiagnosticSubject::operation("generic_write_back_candidate_cleanup"),
                    DiagnosticReason::failure(DiagnosticFailureKind::RollbackFailed),
                    DiagnosticImpact::RecoveryRequired,
                    DiagnosticAction::PreserveRecoveryArtifacts,
                );
                for path in recovery_paths {
                    diagnostic = diagnostic.with_recovery(RecoveryFact::path(path));
                }
                diagnostic
            }
        }
    }

    /// 信号接收或候选清理失败可能伴随一个更早的业务失败。
    pub(crate) fn related_diagnostic(&self) -> Option<SafeDiagnostic> {
        match self {
            Self::Signal {
                operation: Some(operation),
                ..
            }
            | Self::PublishDiscard { operation, .. } => Some(operation.safe_diagnostic()),
            Self::Cancelled
            | Self::Operation { .. }
            | Self::Signal {
                operation: None, ..
            } => None,
        }
    }
}

impl fmt::Display for GenericCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 命令已取消"),
            Self::Operation { stage, source, .. } => write!(formatter, "{stage} 失败：{source}"),
            Self::Signal {
                source, operation, ..
            } => {
                write!(formatter, "接收 Windows 终止信号失败：{source}")?;
                if let Some(operation) = operation {
                    write!(formatter, "；同时发生业务失败：{operation}")?;
                }
                Ok(())
            }
            Self::PublishDiscard {
                operation, discard, ..
            } => {
                write!(formatter, "{operation}；清理未发布候选也失败：{discard}")
            }
        }
    }
}

impl Error for GenericCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Operation { source, .. } => Some(source.as_ref()),
            Self::Signal { source, .. } => Some(source),
            Self::Cancelled | Self::PublishDiscard { .. } => None,
        }
    }
}

fn generic_operation_diagnostic(stage: &'static str) -> SafeDiagnostic {
    let (code, diagnostic_stage, failure, impact, action) = match stage {
        "启动文件运行能力" => (
            DiagnosticCode::FileSystemBuild,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        ),
        "启动 CPU 运行能力" => (
            DiagnosticCode::InternalOperation,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::WorkerSpawnFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        ),
        "取得项目租约" => (
            DiagnosticCode::ProjectUnavailable,
            DiagnosticStage::ProjectOpening,
            DiagnosticFailureKind::Busy,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::RetryAfterResolvingContention,
        ),
        "打开 Generic 项目" => (
            DiagnosticCode::ProjectUnavailable,
            DiagnosticStage::ProjectOpening,
            DiagnosticFailureKind::StateMismatch,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        "初始化 Generic 项目" => (
            DiagnosticCode::ProjectState,
            DiagnosticStage::Init,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "读取 Lua 脚本" => (
            DiagnosticCode::LuaExecution,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::NotFound,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "编译 Lua 脚本" => (
            DiagnosticCode::LuaExecution,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::LuaCompilationFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "执行 Generic Lua" => (
            DiagnosticCode::LuaExecution,
            DiagnosticStage::Lua,
            DiagnosticFailureKind::LuaExecutionFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "选择 Translate Profile" | "读取 Translate Profile" | "读取 WriteBack Profile" => (
            DiagnosticCode::ConfigurationProfileNotFound,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::MissingRequiredValue,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        "选择源语言模块" => (
            DiagnosticCode::LanguageModuleUnavailable,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        "读取翻译 system Prompt"
        | "渲染翻译 system Prompt"
        | "读取翻译 thinking Prompt"
        | "校验翻译 thinking Prompt"
        | "读取翻译响应规则 Prompt"
        | "校验翻译响应规则 Prompt"
        | "读取翻译示例 Prompt"
        | "校验翻译示例 Prompt" => (
            DiagnosticCode::PromptUnavailable,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::InvalidValue,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        "读取翻译资源" | "编译 Generic Placeholder" => (
            DiagnosticCode::CommandInput,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::InvalidValue,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "读取 Generic 翻译资源" => (
            DiagnosticCode::SqliteOperation,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::StateMismatch,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        "复查 Generic 输入" => (
            DiagnosticCode::ProjectState,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::GenericExtractRequired,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "复查 Generic 写回输入" => (
            DiagnosticCode::ProjectState,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::GenericExtractRequired,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "保存 Generic 翻译资源并清除失效译文" => (
            DiagnosticCode::SqliteOperation,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        "调度 Generic 翻译计划" => (
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        ),
        "建立 Generic 翻译计划" => (
            DiagnosticCode::CommandRunPlan,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "调度 Generic 模型消息" | "建立 Generic 模型消息" => (
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        ),
        "提交 Generic 去重复用译文" | "提交 Generic 模型译文" => (
            DiagnosticCode::SqliteOperation,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        ),
        "启动 LLM 运行能力" => (
            DiagnosticCode::HttpClientBuild,
            DiagnosticStage::ModelRequest,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::FixConfiguration,
        ),
        "读取附加 PEM" => (
            DiagnosticCode::FileSystemOperation,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::NotFound,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        "保存 Generic 最近 Profile" => (
            DiagnosticCode::StateFinalizationFailed,
            DiagnosticStage::Translate,
            DiagnosticFailureKind::FinalizationFailed,
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::CheckProjectState,
        ),
        "建立 Generic 写回候选" => (
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::WriteBackCandidateInvalid,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        "调度 Generic Current 复查" | "复查 Generic Current" => (
            DiagnosticCode::WriteBackValidate,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::WriteBackCandidateInvalid,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        "建立 Generic 写回暂存来源" | "建立 Generic 目录候选请求" => (
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::WriteBackOutputPathInvalid,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        "准备 Generic 写回候选" => (
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::Publication,
            DiagnosticFailureKind::WriteBackNotPublished,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        "清理 Generic 写回暂存来源" => (
            DiagnosticCode::WriteBackDiscard,
            DiagnosticStage::Publication,
            DiagnosticFailureKind::RollbackFailed,
            DiagnosticImpact::RecoveryRequired,
            DiagnosticAction::PreserveRecoveryArtifacts,
        ),
        "发布前复查 Generic 输入" => (
            DiagnosticCode::WriteBackValidate,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::WriteBackExtractionOutOfDate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        "检查 Generic 写回目录" => (
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticFailureKind::InvalidPath,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        "发布 Generic 写回目录" => (
            DiagnosticCode::WriteBackPublish,
            DiagnosticStage::Publication,
            DiagnosticFailureKind::WriteBackNotPublished,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        _ => (
            DiagnosticCode::InternalOperation,
            DiagnosticStage::CommandPreparation,
            DiagnosticFailureKind::InternalInvariant,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        ),
    };
    SafeDiagnostic::new(
        code,
        diagnostic_stage,
        DiagnosticSubject::operation(stage),
        DiagnosticReason::failure(failure),
        impact,
        action,
    )
}

fn generic_read_file_diagnostic(
    source: &ReadFileError<SystemFileSystemError>,
    stage: DiagnosticStage,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    match source {
        ReadFileError::NotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::FileSystemOperation,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ReadFileError::NotFile { path } => SafeDiagnostic::new(
            DiagnosticCode::FileSystemOperation,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                "expected=file; actual=not_file",
            ),
            DiagnosticImpact::Unchanged,
            action,
        ),
        ReadFileError::Io { path, source } => source
            .safe_diagnostic_source(stage, DiagnosticImpact::Unchanged, action)
            .with_recovery(RecoveryFact::path(path)),
    }
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MessageError {}

#[derive(Clone, Debug)]
struct GenericCommandPanicContext {
    command: &'static str,
    stage: DiagnosticStage,
    project_workspace: PathBuf,
}

#[derive(Debug)]
struct GenericApplicationScopePanicked;

impl fmt::Display for GenericApplicationScopePanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("application scope panicked")
    }
}

impl Error for GenericApplicationScopePanicked {}

fn generic_command_panic_context(command: &ConfiguredGenericCommand) -> GenericCommandPanicContext {
    let (command_name, stage, projects_root, project_name) = match command {
        ConfiguredGenericCommand::Init { arguments, common } => (
            "init",
            DiagnosticStage::Init,
            common.projects_root(),
            arguments.project.name.as_str(),
        ),
        ConfiguredGenericCommand::Extract {
            project_name,
            common,
        } => (
            "extract",
            DiagnosticStage::Extract,
            common.projects_root(),
            project_name.as_str(),
        ),
        ConfiguredGenericCommand::Translate(command) => (
            "translate",
            DiagnosticStage::Translate,
            command.common().projects_root(),
            command.project_name().as_str(),
        ),
        ConfiguredGenericCommand::WriteBack(command) => (
            "write-back",
            DiagnosticStage::WriteBack,
            command.common().projects_root(),
            command.project_name().as_str(),
        ),
        ConfiguredGenericCommand::Lua(command) => (
            "lua",
            DiagnosticStage::Lua,
            command.common().projects_root(),
            command.project_name().as_str(),
        ),
    };
    GenericCommandPanicContext {
        command: command_name,
        stage,
        project_workspace: projects_root.join(GENERIC_ENGINE_NAME).join(project_name),
    }
}

fn generic_command_panic_error(context: GenericCommandPanicContext) -> GenericCommandError {
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        context.stage,
        DiagnosticSubject::command(context.command),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::ReportBug,
    )
    .with_recovery(RecoveryFact::path(context.project_workspace));
    GenericCommandError::diagnosed(
        "执行 Generic 命令",
        GenericApplicationScopePanicked,
        diagnostic,
    )
}

async fn catch_generic_command_panic(
    context: GenericCommandPanicContext,
    future: impl Future<Output = GenericCommandRunReport>,
) -> GenericCommandRunReport {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(report) => report,
        Err(payload) => {
            // panic payload 可能包含模型正文、Lua、SQL 或用户文本；只丢弃，绝不读取。
            drop(payload);
            GenericCommandRunReport::failed(generic_command_panic_error(context))
        }
    }
}

/// Generic 的生产命令执行器。
pub(crate) struct ProductionGenericCommandRunner {
    locale: UiLocale,
    progress_mode: ProgressMode,
}

impl ProductionGenericCommandRunner {
    pub(crate) const fn new(locale: UiLocale, progress_mode: ProgressMode) -> Self {
        Self {
            locale,
            progress_mode,
        }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredGenericCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let panic_context = generic_command_panic_context(&command);
        catch_generic_command_panic(
            panic_context,
            self.run_without_panic_boundary(command, termination_signals),
        )
        .await
    }

    async fn run_without_panic_boundary(
        self,
        command: ConfiguredGenericCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        match command {
            ConfiguredGenericCommand::Init { arguments, common } => {
                let performance = Arc::new(RunPerformanceCounters::default());
                let file_system = match start_file_system(
                    common.filesystem().clone(),
                    Arc::clone(&performance),
                ) {
                    Ok(file_system) => file_system,
                    Err(source) => {
                        return GenericCommandRunReport::failed(GenericCommandError::operation(
                            "启动文件运行能力",
                            source,
                        ));
                    }
                };
                let project_log = generic_project_log_slot();
                let cancellation = CooperativeCancellation::default();
                let progress = generic_terminal_progress(self.progress_mode, self.locale);
                let operation_progress = progress.observer();
                let project_name = arguments.project.name.clone();
                let workspace_root = generic_workspace(common.projects_root(), &project_name);
                let lease_provider = ProjectCommandLeaseService::new(
                    common.projects_root().to_path_buf(),
                    GENERIC_ENGINE_NAME,
                    file_system.clone(),
                );
                let operation_cancellation = cancellation.clone();
                let cancellation_file_system = file_system.clone();
                let operation_project_log = Arc::clone(&project_log);
                let locale = self.locale;
                let operation = async move {
                    ensure_generic_operation_running(&operation_cancellation)?;
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::Initializing,
                    ));
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(generic_project_lease_failure)?;
                    ensure_generic_operation_running(&operation_cancellation)?;
                    let request = GenericInitRequest {
                        project_name,
                        workspace_root,
                        source_root: arguments.path,
                        source_language: arguments.source_language,
                        target_language: arguments.target_language,
                    };
                    let init_cancellation = operation_cancellation.clone();
                    let (_, project) = run_project_blocking(
                        "初始化 Generic 项目",
                        DiagnosticStage::Init,
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::FixInput,
                        move || {
                            GenericProjectStore::initialize_with_cancellation(
                                request,
                                init_cancellation,
                            )
                        },
                    )
                    .await?;
                    install_generic_project_log(
                        &operation_project_log,
                        start_command_log(CommandLogStart {
                            common: &common,
                            locale,
                            engine: GENERIC_ENGINE_NAME,
                            project: project.project_name().as_str(),
                            command: "init",
                            stage: DiagnosticStage::Init,
                            profile: None,
                            performance,
                            panic_boundary: None,
                        }),
                    );
                    Ok(GenericCommandOutput::Init { project })
                };
                drive_and_shutdown(
                    operation,
                    termination_signals,
                    move || {
                        cancellation.request();
                        cancellation_file_system.cancel_waits();
                    },
                    file_system,
                    Vec::new(),
                    project_log,
                    progress,
                )
                .await
            }
            ConfiguredGenericCommand::Extract {
                project_name,
                common,
            } => {
                let performance = Arc::new(RunPerformanceCounters::default());
                let project_log = generic_project_log_slot();
                start_existing_generic_project_log(
                    &project_log,
                    &common,
                    self.locale,
                    &project_name,
                    "extract",
                    DiagnosticStage::Extract,
                    Arc::clone(&performance),
                );
                let file_system = match start_file_system(
                    common.filesystem().clone(),
                    Arc::clone(&performance),
                ) {
                    Ok(file_system) => file_system,
                    Err(source) => {
                        return GenericCommandRunReport::from_driven(
                            Driven::Finished(Err(GenericCommandError::operation(
                                "启动文件运行能力",
                                source,
                            ))),
                            Vec::new(),
                            take_generic_project_log(&project_log),
                        );
                    }
                };
                let cancellation = CooperativeCancellation::default();
                let progress = generic_terminal_progress(self.progress_mode, self.locale);
                let operation_progress = progress.observer();
                let store = GenericProjectStore::for_workspace_with_cancellation(
                    generic_workspace(common.projects_root(), &project_name),
                    cancellation.clone(),
                );
                let lease_provider = ProjectCommandLeaseService::new(
                    common.projects_root().to_path_buf(),
                    GENERIC_ENGINE_NAME,
                    file_system.clone(),
                );
                let output_name = project_name.clone();
                let operation_cancellation = cancellation.clone();
                let cancellation_file_system = file_system.clone();
                let operation = async move {
                    ensure_generic_operation_running(&operation_cancellation)?;
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::Extracting,
                    ));
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(generic_project_lease_failure)?;
                    ensure_generic_operation_running(&operation_cancellation)?;
                    let open_store = store.clone();
                    run_project_blocking(
                        "打开 Generic 项目",
                        DiagnosticStage::ProjectOpening,
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::CheckProjectState,
                        move || open_store.open(),
                    )
                    .await?;
                    let outcome = run_project_blocking(
                        "同步 Generic JSONL",
                        DiagnosticStage::Extract,
                        DiagnosticImpact::ProgressPreserved,
                        DiagnosticAction::FixInput,
                        move || store.extract(),
                    )
                    .await?;
                    Ok(GenericCommandOutput::Extract {
                        project: output_name,
                        outcome,
                    })
                };
                drive_and_shutdown(
                    operation,
                    termination_signals,
                    move || {
                        cancellation.request();
                        cancellation_file_system.cancel_waits();
                    },
                    file_system,
                    Vec::new(),
                    project_log,
                    progress,
                )
                .await
            }
            ConfiguredGenericCommand::Translate(command) => {
                self.run_translate(*command, termination_signals).await
            }
            ConfiguredGenericCommand::WriteBack(command) => {
                self.run_write_back(command, termination_signals).await
            }
            ConfiguredGenericCommand::Lua(command) => {
                self.run_lua(command, termination_signals).await
            }
        }
    }

    async fn run_lua(
        self,
        command: ConfiguredProjectLuaCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let project_log = generic_project_log_slot();
        install_generic_project_log(
            &project_log,
            start_command_log(CommandLogStart {
                common: command.common(),
                locale: self.locale,
                engine: GENERIC_ENGINE_NAME,
                project: project_name.as_str(),
                command: "lua",
                stage: DiagnosticStage::Lua,
                profile: None,
                performance: Arc::clone(&performance),
                panic_boundary: None,
            }),
        );
        let mut lua_summary = {
            let project_log = project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ProjectLuaSummaryGuard::new(
                project_log
                    .as_ref()
                    .expect("Generic Lua 必须在建立摘要守卫前建立项目日志"),
            )
        };
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::from_driven(
                        Driven::Finished(Err(GenericCommandError::operation(
                            "启动文件运行能力",
                            source,
                        ))),
                        Vec::new(),
                        take_generic_project_log(&project_log),
                    );
                }
            };
        let cancellation = CooperativeCancellation::default();
        let lua_cancellation = ProjectLuaCancellation::default();
        let progress = generic_terminal_progress(self.progress_mode, self.locale);
        let operation_progress = progress.observer();
        let script_path = command.script().script_path().to_path_buf();
        let arguments = command.arguments().to_vec();
        let store = GenericProjectStore::for_workspace_with_cancellation(
            generic_workspace(command.common().projects_root(), &project_name),
            cancellation.clone(),
        );
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let operation_file_system = file_system.clone();
        let operation_lua_cancellation = lua_cancellation.clone();
        let operation_cancellation = cancellation.clone();
        let cancellation_file_system = file_system.clone();
        let output_name = project_name.clone();
        let (print_sink, preflight_log) = {
            let project_log = project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            project_log.as_ref().map_or((None, None), |project_log| {
                (
                    Some(Arc::new(ProjectLogLuaPrintSink::from_active(project_log))),
                    Some((project_log.logger().clone(), project_log.context().clone())),
                )
            })
        };
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            let script = operation_file_system
                .read_file(script_path.clone())
                .await
                .map_err(|source| {
                    generic_read_file_failure(
                        "读取 Lua 脚本",
                        source,
                        DiagnosticStage::CommandPreparation,
                        DiagnosticAction::FixInput,
                    )
                })?;
            let identity = script.resolved_path().to_string_lossy().into_owned();
            let source = script.into_bytes();
            let preflight_cancellation = operation_lua_cancellation.clone();
            let preparation = tokio::task::spawn_blocking(move || {
                let program = ProjectLuaProgram::new(identity, source, arguments);
                let fingerprint = fingerprint_project_lua_program_with_cancellation(
                    &program,
                    &preflight_cancellation,
                )?;
                if let Some((preflight_logger, preflight_context)) = preflight_log {
                    preflight_logger.emit(ProjectLogEvent::new(
                        ProjectLogLevel::Info,
                        ProjectLogCode::LuaScript,
                        preflight_context,
                        ProjectLogPayload::LuaScript {
                            identity: program.identity().to_owned(),
                            fingerprint: fingerprint.hex(),
                        },
                    ));
                }
                compile_project_lua_program_with_cancellation(&program, &preflight_cancellation)?;
                Ok::<_, ProjectLuaFailure>(program)
            })
            .await;
            let preparation = preparation
                .map_err(|source| GenericCommandError::operation("准备 Generic Lua", source))?;
            let program = match preparation {
                Ok(prepared) => prepared,
                Err(ProjectLuaFailure::Cancelled) => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let source = GenericLuaPreflightError(source);
                    let detail = source.to_string();
                    let diagnostic = project_lua_failure_diagnostic(
                        &source.0,
                        DiagnosticStage::CommandPreparation,
                        &detail,
                    );
                    return Err(GenericCommandError::diagnosed(
                        "编译 Lua 脚本",
                        source,
                        diagnostic,
                    ));
                }
            };
            ensure_generic_operation_running(&operation_cancellation)?;

            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let project = run_project_blocking(
                "打开 Generic 项目",
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
                move || store.open(),
            )
            .await?;
            let database_path = project.database_path().to_path_buf();
            let lua_project_name = output_name.as_str().to_owned();
            let lua_adapter = generic_project_lua_adapter(project, operation_cancellation.clone());
            let request = ProjectLuaRunRequest::new(
                ProjectLuaProject::new(lua_project_name, GENERIC_ENGINE_NAME),
                program,
                lua_adapter,
            )
            .with_cancellation(operation_lua_cancellation);
            let request = match print_sink {
                Some(print_sink) => request.with_print_sink(print_sink),
                None => request,
            };
            let lua_metrics = request.metrics();
            lua_summary.track(lua_metrics.clone());
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::RunningLua,
            ));
            let execution = tokio::task::spawn_blocking(move || {
                let open_path = database_path.clone();
                let connection = Connection::open_with_flags(
                    &database_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                )
                .map_err(|source| GenericLuaExecutionError::Open {
                    path: open_path,
                    source,
                })?;
                run_project_lua(connection, request).map_err(GenericLuaExecutionError::Run)
            })
            .await;
            let report = lua_metrics.report();
            lua_summary.emit(report);
            let execution = execution
                .map_err(|source| GenericCommandError::operation("执行 Generic Lua", source))?;
            match execution {
                Ok(_) => {}
                Err(source) if source.is_cancelled() => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let diagnostic = generic_lua_execution_diagnostic(&source);
                    return Err(GenericCommandError::diagnosed(
                        "执行 Generic Lua",
                        source,
                        diagnostic,
                    ));
                }
            }
            Ok(GenericCommandOutput::Lua {
                project: output_name,
            })
        };
        drive_and_shutdown(
            operation,
            termination_signals,
            move || {
                cancellation.request();
                lua_cancellation.cancel();
                cancellation_file_system.cancel_waits();
            },
            file_system,
            Vec::new(),
            project_log,
            progress,
        )
        .await
    }

    async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let project_log = generic_project_log_slot();
        start_existing_generic_project_log(
            &project_log,
            command.common(),
            self.locale,
            &project_name,
            "translate",
            DiagnosticStage::Translate,
            Arc::clone(&performance),
        );
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration.clone(), Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::from_driven(
                        Driven::Finished(Err(GenericCommandError::operation(
                            "启动文件运行能力",
                            source,
                        ))),
                        Vec::new(),
                        take_generic_project_log(&project_log),
                    );
                }
            };
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let error = GenericCommandError::operation("启动 CPU 运行能力", source);
                let mut shutdown_errors = Vec::new();
                if let Err(source) = file_system.shutdown().await {
                    shutdown_errors.push(GenericShutdownError::new("filesystem", source));
                }
                return GenericCommandRunReport::from_driven(
                    Driven::Finished(Err(error)),
                    shutdown_errors,
                    take_generic_project_log(&project_log),
                );
            }
        };
        let cancellation = CooperativeCancellation::default();
        let progress = generic_terminal_progress(self.progress_mode, self.locale);
        let operation_progress = progress.observer();
        let llm_holder = Arc::new(Mutex::new(None::<OpenAiChatCompletionExecutor>));
        let store = GenericProjectStore::for_workspace_with_cancellation(
            generic_workspace(command.common().projects_root(), &project_name),
            cancellation.clone(),
        );
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let operation_file_system = file_system.clone();
        let operation_cpu = cpu.clone();
        let operation_cancellation = cancellation.clone();
        let operation_llm_holder = Arc::clone(&llm_holder);
        let output_name = project_name.clone();
        let locale = self.locale;
        let operation_project_log = Arc::clone(&project_log);
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::PlanningTranslation,
            ));
            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;

            let initial_store = store.clone();
            let (snapshot, _live, current_resources) = run_project_blocking(
                "复查 Generic 输入",
                DiagnosticStage::Translate,
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::CheckProjectState,
                move || initial_store.load_current_translation_state(),
            )
            .await?;
            let project = snapshot.project().clone();
            let profile_id = command
                .resolved_profile_id()
                .map(str::to_owned)
                .or_else(|| project.last_profile_id().map(str::to_owned))
                .ok_or_else(|| {
                    GenericCommandError::message(
                        "选择 Translate Profile",
                        "首次 Generic Translate 必须显式提供 PROFILE_ID",
                    )
                })?;
            let command = command.resolve_profile(&profile_id).map_err(|source| {
                GenericCommandError::operation("读取 Translate Profile", source)
            })?;
            if let Some(project_log) = operation_project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
            {
                project_log.set_profile(&profile_id);
            }
            let configuration = command.translation();
            let source_language = configuration
                .language_modules()
                .resolve(project.language_pair().source())
                .map_err(|source| GenericCommandError::operation("选择源语言模块", source))?;

            let prompt = load_generic_prompt(
                &operation_file_system,
                &operation_cpu,
                configuration,
                project.language_pair(),
                &operation_cancellation,
            )
            .await?;
            let resource_clone_cancellation = operation_cancellation.clone();
            let (current_resources, current_terminology_json, current_placeholder_json) =
                operation_cpu
                    .execute(move || {
                        let terminology_json = clone_generic_cpu_text(
                            current_resources.terminology_json(),
                            &resource_clone_cancellation,
                        )?;
                        let placeholder_json = clone_generic_cpu_text(
                            current_resources.placeholder_rules_json(),
                            &resource_clone_cancellation,
                        )?;
                        Ok::<_, GenericPreparationError>((
                            current_resources,
                            terminology_json,
                            placeholder_json,
                        ))
                    })
                    .await
                    .map_err(|source| {
                        generic_cpu_execution_failure("调度 Generic 翻译资源复制", source)
                    })?
                    .map_err(|source| {
                        generic_preparation_failure("复制 Generic 翻译资源", source)
                    })?;
            let terminology_path = command.terminology_path().map(Path::to_path_buf);
            let placeholder_rules_path = command.placeholder_rules_path().map(Path::to_path_buf);
            let (terminology, placeholder_rules, terminology_json, placeholder_json) =
                if terminology_path.is_none() && placeholder_rules_path.is_none() {
                    (
                        current_resources.terminology(),
                        current_resources.placeholder_rules(),
                        current_terminology_json,
                        current_placeholder_json,
                    )
                } else {
                    let resource_reader = TranslationPlanningResourceReadingService::new(
                        operation_file_system.clone(),
                        operation_cpu.clone(),
                    )
                    .with_cancellation(operation_cancellation.clone());
                    let resources = resource_reader
                        .read(
                            terminology_path,
                            placeholder_rules_path,
                            current_terminology_json,
                            current_placeholder_json,
                        )
                        .await
                        .map_err(generic_translation_resource_failure)?;
                    let (terminology, placeholder_definitions, terminology_json, placeholder_json) =
                        resources.into_parts();
                    let placeholder_compile_cancellation = operation_cancellation.clone();
                    let placeholder_rules = operation_cpu
                        .execute(move || {
                            GenericPlaceholderService::default()
                                .compile_with_cancellation(placeholder_definitions, || {
                                    ensure_generic_cpu_running(&placeholder_compile_cancellation)
                                })?
                                .map_err(GenericPreparationError::Placeholder)
                        })
                        .await
                        .map_err(|source| {
                            generic_cpu_execution_failure("调度 Generic Placeholder 编译", source)
                        })?
                        .map_err(|source| {
                            generic_preparation_failure("编译 Generic Placeholder", source)
                        })?;
                    (
                        terminology,
                        placeholder_rules,
                        terminology_json,
                        placeholder_json,
                    )
                };

            let expected_raw_fingerprint = snapshot
                .project()
                .extracted_raw_fingerprint()
                .expect("load_current_translation_state 已确认存在 Extract 指纹");
            let planning_snapshot = snapshot;
            let planning_terms = Arc::clone(&terminology);
            let planning_rules = placeholder_rules.clone();
            let planning_language = Arc::clone(&source_language);
            let planning_prompt = prompt.fingerprint;
            let planning_client = Arc::clone(configuration.client());
            let planning_cancellation = operation_cancellation.clone();
            let target_characters = configuration
                .profile()
                .target_task_user_message_characters();
            let prepared = operation_cpu
                .execute(move || {
                    ensure_generic_cpu_running(&planning_cancellation)?;
                    let planning_client_fingerprint = planning_client.semantic_fingerprint();
                    ensure_generic_cpu_running(&planning_cancellation)?;
                    let planning_language_fingerprint = planning_language
                        .semantic_fingerprint_with_cancellation(&mut || {
                            ensure_generic_language_running(&planning_cancellation)
                        })
                        .map_err(|LanguageOperationCancelled| GenericPreparationError::Cancelled)?;
                    ensure_generic_cpu_running(&planning_cancellation)?;
                    prepare_generic_translation(
                        &planning_snapshot,
                        planning_terms,
                        &planning_rules,
                        planning_language,
                        AutomaticStateResources {
                            prompt: planning_prompt,
                            client_semantics: planning_client_fingerprint,
                            language_module: planning_language_fingerprint,
                            terminology_hits: empty_terminology_fingerprint(),
                        },
                        target_characters,
                        &planning_cancellation,
                    )
                })
                .await
                .map_err(|source| generic_cpu_execution_failure("调度 Generic 翻译计划", source))?
                .map_err(|source| generic_preparation_failure("建立 Generic 翻译计划", source))?;

            let PreparedGenericTranslation { plan, facts } = prepared;
            let (invalidations, reused, tasks) = plan.into_parts();
            let transformation_cancellation = operation_cancellation.clone();
            let (
                terminology_json,
                placeholder_json,
                invalidations,
                reuse_writes,
                apply_translation_resources,
            ) = operation_cpu
                .execute(move || {
                    let has_invalidations = !invalidations.is_empty();
                    let mut clears = Vec::with_capacity(invalidations.len());
                    for invalidation in invalidations {
                        ensure_generic_cpu_running(&transformation_cancellation)?;
                        clears.push(invalidation.into_clear());
                    }
                    let mut writes = Vec::with_capacity(reused.len());
                    for reuse in reused {
                        ensure_generic_cpu_running(&transformation_cancellation)?;
                        writes.push(reuse.into_write());
                    }
                    let resources_changed = !generic_cpu_text_equal(
                        current_resources.terminology_json(),
                        &terminology_json,
                        &transformation_cancellation,
                    )? || !generic_cpu_text_equal(
                        current_resources.placeholder_rules_json(),
                        &placeholder_json,
                        &transformation_cancellation,
                    )?;
                    ensure_generic_cpu_running(&transformation_cancellation)?;
                    Ok::<_, GenericPreparationError>((
                        terminology_json,
                        placeholder_json,
                        clears,
                        writes,
                        resources_changed || has_invalidations,
                    ))
                })
                .await
                .map_err(|source| {
                    generic_cpu_execution_failure("调度 Generic 翻译计划转换", source)
                })?
                .map_err(|source| generic_preparation_failure("转换 Generic 翻译计划", source))?;
            let mut summary = GenericTranslationSummary {
                total_tasks: tasks.len(),
                ..GenericTranslationSummary::default()
            };
            if tasks.is_empty() {
                operation_progress.observe(ProgressSnapshot::indeterminate(
                    GenericProgressPhase::NoModelWork,
                ));
            } else {
                operation_progress.observe(ProgressSnapshot::determinate(
                    GenericProgressPhase::ConfirmedTasks,
                    0,
                    u64::try_from(tasks.len()).unwrap_or(u64::MAX),
                ));
            }
            ensure_generic_operation_running(&operation_cancellation)?;
            if apply_translation_resources {
                let save_store = store.clone();
                let resource_outcome = run_project_blocking(
                    "保存 Generic 翻译资源并清除失效译文",
                    DiagnosticStage::Translate,
                    DiagnosticImpact::ProgressPreserved,
                    DiagnosticAction::Retry,
                    move || {
                        save_store.apply_translation_resources(
                            expected_raw_fingerprint,
                            &terminology_json,
                            &placeholder_json,
                            &invalidations,
                        )
                    },
                )
                .await?;
                summary.cleared_units = resource_outcome.committed;
                summary.conflicted_units += resource_outcome.conflicts.len();
            }
            ensure_generic_operation_running(&operation_cancellation)?;

            summary.reused_units = reuse_writes.len();
            if !reuse_writes.is_empty() {
                let commit_store = store.clone();
                let reuse_profile = profile_id.clone();
                let outcome = run_project_blocking(
                    "提交 Generic 去重复用译文",
                    DiagnosticStage::Translate,
                    DiagnosticImpact::ProgressPreserved,
                    DiagnosticAction::Retry,
                    move || {
                        commit_store.commit_translations_for_profile(
                            expected_raw_fingerprint,
                            &reuse_writes,
                            &reuse_profile,
                        )
                    },
                )
                .await?;
                add_commit_outcome(&mut summary, &outcome);
            }

            ensure_generic_operation_running(&operation_cancellation)?;

            if !tasks.is_empty() {
                let pem_roots =
                    load_additional_pem_roots(&operation_file_system, configuration.llm()).await?;
                let llm = OpenAiChatCompletionExecutor::new(
                    configuration.llm().with_pem_roots(pem_roots),
                )
                .map_err(|source| GenericCommandError::operation("启动 LLM 运行能力", source))?;
                *operation_llm_holder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(llm.clone());
                let metadata_client = Arc::clone(configuration.client());
                let metadata_cancellation = operation_cancellation.clone();
                let client_metadata = operation_cpu
                    .execute(move || {
                        ensure_generic_cpu_running(&metadata_cancellation)?;
                        let metadata = metadata_client.record_metadata();
                        ensure_generic_cpu_running(&metadata_cancellation)?;
                        Ok::<_, GenericPreparationError>(metadata)
                    })
                    .await
                    .map_err(|source| {
                        generic_cpu_execution_failure("调度 Generic LLM 记录信息", source)
                    })?
                    .map_err(|source| {
                        generic_preparation_failure("建立 Generic LLM 记录信息", source)
                    })?;
                let task_records = configure_generic_task_records(
                    command.record_translation_tasks(),
                    &operation_project_log,
                    &file_system_configuration,
                    client_metadata,
                    locale,
                    operation_cpu.clone(),
                    project.workspace_root(),
                );
                let task_result = execute_generic_tasks(GenericTaskExecution {
                    store: store.clone(),
                    expected_raw_fingerprint,
                    profile_id: profile_id.clone(),
                    tasks,
                    facts: Arc::new(facts),
                    placeholder_rules,
                    terminology,
                    language_module: source_language,
                    system_prompt: prompt.system_prompt,
                    response_mode: prompt.response_mode,
                    client: Arc::clone(configuration.client()),
                    llm: llm.clone(),
                    retry_delays: configuration
                        .profile()
                        .request()
                        .network_retry_delays()
                        .to_vec(),
                    max_retry_after: configuration.profile().request().max_network_retry_after(),
                    cpu: operation_cpu.clone(),
                    cancellation: operation_cancellation.clone(),
                    task_records: task_records.clone(),
                    project_log: generic_task_project_log(&operation_project_log),
                    progress: operation_progress.clone(),
                })
                .await;
                llm.shutdown().await;
                *operation_llm_holder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                task_records.finish().await;
                let task_summary = task_result?;
                merge_task_summary(&mut summary, task_summary);
            }

            ensure_generic_operation_running(&operation_cancellation)?;
            if should_remember_profile_separately(&summary) {
                let remember_store = store.clone();
                let remembered_profile = profile_id.clone();
                run_project_blocking(
                    "保存 Generic 最近 Profile",
                    DiagnosticStage::Translate,
                    DiagnosticImpact::ProgressPreserved,
                    DiagnosticAction::Retry,
                    move || remember_store.remember_profile(&remembered_profile),
                )
                .await?;
            }
            Ok(GenericCommandOutput::Translate {
                project: output_name,
                profile_id,
                summary,
            })
        };

        let driven = drive(operation, termination_signals, || {
            progress.safe_stopping();
            cancellation.request();
            cpu.cancel_waits();
            file_system.cancel_waits();
            if let Some(llm) = llm_holder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                llm.cancel_waits();
            }
        })
        .await;
        progress.finalizing();
        let mut shutdown_errors = Vec::new();
        if let Err(source) = cpu.shutdown() {
            shutdown_errors.push(GenericShutdownError::new("CPU executor", source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::new("filesystem", source));
        }
        record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
        GenericCommandRunReport::from_driven(
            driven,
            shutdown_errors,
            take_generic_project_log(&project_log),
        )
    }

    async fn run_write_back(
        self,
        command: ConfiguredGenericWriteBackCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let project_log = generic_project_log_slot();
        start_existing_generic_project_log(
            &project_log,
            command.common(),
            self.locale,
            &project_name,
            "write-back",
            DiagnosticStage::WriteBack,
            Arc::clone(&performance),
        );
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::from_driven(
                        Driven::Finished(Err(GenericCommandError::operation(
                            "启动文件运行能力",
                            source,
                        ))),
                        Vec::new(),
                        take_generic_project_log(&project_log),
                    );
                }
            };
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let error = GenericCommandError::operation("启动 CPU 运行能力", source);
                let mut shutdown_errors = Vec::new();
                if let Err(source) = file_system.shutdown().await {
                    shutdown_errors.push(GenericShutdownError::new("filesystem", source));
                }
                return GenericCommandRunReport::from_driven(
                    Driven::Finished(Err(error)),
                    shutdown_errors,
                    take_generic_project_log(&project_log),
                );
            }
        };
        let cancellation = CooperativeCancellation::default();
        let publication_gate = GenericWriteBackPublicationGate::default();
        let progress = generic_terminal_progress(self.progress_mode, self.locale);
        let operation_progress = progress.observer();
        let store = GenericProjectStore::for_workspace_with_cancellation(
            generic_workspace(command.common().projects_root(), &project_name),
            cancellation.clone(),
        );
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let operation_file_system = file_system.clone();
        let operation_cpu = cpu.clone();
        let operation_cancellation = cancellation.clone();
        let operation_publication_gate = publication_gate.clone();
        let output_name = project_name.clone();
        let operation_project_log = Arc::clone(&project_log);
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::PreparingWriteBack,
            ));
            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;

            let initial_store = store.clone();
            let (snapshot, live, current_resources) = run_project_blocking(
                "复查 Generic 写回输入",
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
                move || initial_store.load_current_translation_state(),
            )
            .await?;
            let project = snapshot.project().clone();
            let terminology = current_resources.terminology();
            let placeholder_rules = current_resources.placeholder_rules();

            let automatic_scan_cancellation = operation_cancellation.clone();
            let (snapshot, has_automatic_translation) = operation_cpu
                .execute(move || {
                    let mut has_automatic = false;
                    'files: for file in snapshot.files() {
                        ensure_generic_cpu_running(&automatic_scan_cancellation)?;
                        for group in file.groups() {
                            ensure_generic_cpu_running(&automatic_scan_cancellation)?;
                            for unit in group.units() {
                                ensure_generic_cpu_running(&automatic_scan_cancellation)?;
                                if unit.translation().is_some_and(|translation| {
                                    matches!(
                                        translation.origin(),
                                        crate::generic::TranslationOrigin::Automatic
                                    )
                                }) {
                                    has_automatic = true;
                                    break 'files;
                                }
                            }
                        }
                    }
                    Ok::<_, GenericPreparationError>((snapshot, has_automatic))
                })
                .await
                .map_err(|source| {
                    generic_cpu_execution_failure("调度 Generic 自动译文复查", source)
                })?
                .map_err(|source| generic_preparation_failure("复查 Generic 自动译文", source))?;
            let automatic_resources = if has_automatic_translation {
                match project.last_profile_id().map(str::to_owned) {
                    Some(profile_id) => {
                        let configuration =
                            command.resolve_translation(&profile_id).map_err(|source| {
                                GenericCommandError::operation("读取 WriteBack Profile", source)
                            })?;
                        if let Some(project_log) = operation_project_log
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_mut()
                        {
                            project_log.set_profile(&profile_id);
                        }
                        let source_language = configuration
                            .language_modules()
                            .resolve(project.language_pair().source())
                            .map_err(|source| {
                                GenericCommandError::operation("选择源语言模块", source)
                            })?;
                        let prompt = load_generic_prompt(
                            &operation_file_system,
                            &operation_cpu,
                            &configuration,
                            project.language_pair(),
                            &operation_cancellation,
                        )
                        .await?;
                        let fingerprint_client = Arc::clone(configuration.client());
                        let fingerprint_cancellation = operation_cancellation.clone();
                        Some(
                            operation_cpu
                                .execute(move || {
                                    ensure_generic_cpu_running(&fingerprint_cancellation)?;
                                    let client_semantics =
                                        fingerprint_client.semantic_fingerprint();
                                    ensure_generic_cpu_running(&fingerprint_cancellation)?;
                                    let language_module = source_language
                                        .semantic_fingerprint_with_cancellation(&mut || {
                                            ensure_generic_language_running(
                                                &fingerprint_cancellation,
                                            )
                                        })
                                        .map_err(|LanguageOperationCancelled| {
                                            GenericPreparationError::Cancelled
                                        })?;
                                    ensure_generic_cpu_running(&fingerprint_cancellation)?;
                                    Ok::<_, GenericPreparationError>(AutomaticStateResources {
                                        prompt: prompt.fingerprint,
                                        client_semantics,
                                        language_module,
                                        terminology_hits: empty_terminology_fingerprint(),
                                    })
                                })
                                .await
                                .map_err(|source| {
                                    generic_cpu_execution_failure(
                                        "调度 Generic 自动资源指纹",
                                        source,
                                    )
                                })?
                                .map_err(|source| {
                                    generic_preparation_failure("建立 Generic 自动资源指纹", source)
                                })?,
                        )
                    }
                    None => None,
                }
            } else {
                None
            };
            let current_snapshot = snapshot;
            let current_terms = Arc::clone(&terminology);
            let current_rules = placeholder_rules.clone();
            let current_cancellation = operation_cancellation.clone();
            let (current_snapshot, current_translations) = operation_cpu
                .execute(move || {
                    let current_translations = collect_generic_current_translations(
                        &current_snapshot,
                        current_terms.as_ref(),
                        &current_rules,
                        automatic_resources,
                        &current_cancellation,
                    )?;
                    Ok::<_, GenericPreparationError>((current_snapshot, current_translations))
                })
                .await
                .map_err(|source| {
                    generic_cpu_execution_failure("调度 Generic Current 复查", source)
                })?
                .map_err(|source| generic_preparation_failure("复查 Generic Current", source))?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let candidate_cancellation = operation_cancellation.clone();
            let (write_back_project, candidate) = operation_cpu
                .execute(move || {
                    let project = current_snapshot.project().clone();
                    build_write_back_candidate_with_cancellation(
                        &current_snapshot,
                        &live,
                        &current_translations,
                        &candidate_cancellation,
                    )
                    .map(|candidate| (project, candidate))
                })
                .await
                .map_err(|source| generic_cpu_execution_failure("建立 Generic 写回候选", source))?
                .map_err(generic_write_back_candidate_failure)?;
            publish_generic_write_back(
                directory_publisher,
                output_name,
                write_back_project,
                candidate,
                operation_cancellation,
                operation_publication_gate,
                move || {
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::PublishingWriteBack,
                    ));
                },
            )
            .await
        };

        let cancellation_publication_gate = publication_gate;
        let driven = drive_write_back(operation, termination_signals, || {
            if !cancellation_publication_gate.request_cancellation() {
                return false;
            }
            progress.safe_stopping();
            cancellation.request();
            cpu.cancel_waits();
            file_system.cancel_waits();
            true
        })
        .await;
        progress.finalizing();
        let mut shutdown_errors = Vec::new();
        if let Err(source) = cpu.shutdown() {
            shutdown_errors.push(GenericShutdownError::new("CPU executor", source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::new("filesystem", source));
        }
        record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
        GenericCommandRunReport::from_driven(
            driven,
            shutdown_errors,
            take_generic_project_log(&project_log),
        )
    }
}

#[derive(Debug)]
enum GenericLuaExecutionError {
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    Run(ProjectLuaRunError),
}

#[derive(Debug)]
struct GenericLuaPreflightError(ProjectLuaFailure);

impl fmt::Display for GenericLuaPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for GenericLuaPreflightError {}

impl GenericLuaExecutionError {
    fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Run(
                ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
                    | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
            )
        )
    }
}

impl fmt::Display for GenericLuaExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "打开项目数据库 {} 失败：{source}",
                    path.display()
                )
            }
            Self::Run(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericLuaExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Run(source) => Some(source),
        }
    }
}

fn project_lua_failure_diagnostic(
    source: &ProjectLuaFailure,
    stage: DiagnosticStage,
    detail: &str,
) -> SafeDiagnostic {
    let (code, reason, action, subject) = match source {
        ProjectLuaFailure::Compile(_) => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaCompilationFailed,
                detail,
            ),
            DiagnosticAction::FixInput,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
        ProjectLuaFailure::Cancelled
        | ProjectLuaFailure::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Cancelled) => {
            (
                DiagnosticCode::LuaExecution,
                DiagnosticReason::failure_with_detail(DiagnosticFailureKind::LockCancelled, detail),
                DiagnosticAction::Retry,
                DiagnosticSubject::operation("project_lua_transaction"),
            )
        }
        ProjectLuaFailure::Context(_) => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaContextCreationFailed,
                detail,
            ),
            DiagnosticAction::ReportBug,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
        ProjectLuaFailure::Script(_) => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaExecutionFailed,
                detail,
            ),
            DiagnosticAction::FixInput,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
        ProjectLuaFailure::DatabasePrerequisite(
            ProjectLuaDatabasePrerequisiteError::InvalidProjectState(state),
        ) => (
            DiagnosticCode::ProjectState,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::StateMismatch, state),
            DiagnosticAction::CheckProjectState,
            DiagnosticSubject::operation("validate_project_lua_database"),
        ),
        ProjectLuaFailure::DatabasePrerequisite(ProjectLuaDatabasePrerequisiteError::Sqlite(
            error,
        ))
        | ProjectLuaFailure::Database(error) => (
            DiagnosticCode::SqliteOperation,
            project_lua_sqlite_reason(error, DiagnosticFailureKind::LuaExecutionFailed),
            DiagnosticAction::CheckProjectState,
            DiagnosticSubject::operation(error.operation()),
        ),
        ProjectLuaFailure::Host {
            kind: "worker_spawn",
            operation,
            ..
        } => (
            DiagnosticCode::InternalOperation,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::WorkerSpawnFailed, detail),
            DiagnosticAction::ReportBug,
            DiagnosticSubject::operation(operation),
        ),
        ProjectLuaFailure::Host {
            operation: "translation.capture",
            ..
        } => (
            DiagnosticCode::ProjectState,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::StateMismatch, detail),
            DiagnosticAction::CheckProjectState,
            DiagnosticSubject::operation("translation.capture"),
        ),
        ProjectLuaFailure::Host { .. } => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::LuaHostCallFailed, detail),
            DiagnosticAction::FixInput,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
        ProjectLuaFailure::Validation(_) => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaFinalizationFailed,
                detail,
            ),
            DiagnosticAction::FixInput,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
        ProjectLuaFailure::Panicked => (
            DiagnosticCode::LuaExecution,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::WorkerPanicked, detail),
            DiagnosticAction::ReportBug,
            DiagnosticSubject::operation("project_lua_transaction"),
        ),
    };
    SafeDiagnostic::new(
        code,
        stage,
        subject,
        reason,
        DiagnosticImpact::Unchanged,
        action,
    )
}

fn project_lua_sqlite_reason(
    source: &ProjectLuaSqliteError,
    fallback: DiagnosticFailureKind,
) -> DiagnosticReason {
    match source.sqlite_codes() {
        Some((primary_code, extended_code)) => DiagnosticReason::Sqlite {
            primary_code,
            extended_code,
        },
        None => DiagnosticReason::failure(fallback),
    }
}

fn generic_lua_execution_diagnostic(source: &GenericLuaExecutionError) -> SafeDiagnostic {
    let detail = source.to_string();
    match source {
        GenericLuaExecutionError::Open { path, .. } => SafeDiagnostic::new(
            DiagnosticCode::LuaExecution,
            DiagnosticStage::Lua,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaDatabaseOpenFailed,
                &detail,
            ),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        ),
        GenericLuaExecutionError::Run(
            ProjectLuaRunError::RollbackOutcomeUnknown { .. }
            | ProjectLuaRunError::CommitOutcomeUnknown(_),
        ) => SafeDiagnostic::new(
            DiagnosticCode::LuaExecution,
            DiagnosticStage::Lua,
            DiagnosticSubject::operation("project_lua_transaction"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::LuaFinalizationFailed,
                &detail,
            ),
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::PreserveRecoveryArtifacts,
        )
        .with_recovery(RecoveryFact::transaction("outcome_unknown")),
        GenericLuaExecutionError::Run(ProjectLuaRunError::NotStarted(failure)) => {
            project_lua_failure_diagnostic(failure, DiagnosticStage::Lua, &detail)
        }
        GenericLuaExecutionError::Run(ProjectLuaRunError::RolledBack(failure)) => {
            project_lua_failure_diagnostic(failure, DiagnosticStage::Lua, &detail)
                .with_recovery(RecoveryFact::transaction("rolled_back"))
        }
    }
}

struct LoadedGenericPrompt {
    system_prompt: String,
    response_mode: TranslationResponseMode,
    fingerprint: Sha256Fingerprint,
}

#[derive(Debug)]
enum GenericPromptPreparationError {
    Cancelled,
    SystemResource(PromptResourceLoadError),
    ThinkingResource(PromptResourceLoadError),
    RulesResource(PromptResourceLoadError),
    ExampleResource(PromptResourceLoadError),
    SystemTemplate(PromptTemplateError),
    ThinkingTemplate(PromptTemplateError),
    RulesTemplate(PromptTemplateError),
    ExampleTemplate(PromptTemplateError),
}

async fn load_generic_prompt(
    file_system: &SystemFileSystem,
    cpu: &RayonCpuExecutor,
    configuration: &super::config::TranslateConfiguration,
    language_pair: &crate::language::LanguagePair,
    cancellation: &CooperativeCancellation,
) -> Result<LoadedGenericPrompt, GenericCommandError> {
    let response_mode =
        TranslationResponseMode::new(configuration.thinking_output(), configuration.source_echo());
    let prompt_paths =
        translation_prompt_resource_paths(configuration.prompt_root(), response_mode);
    let template = read_unparsed_prompt_resource(file_system, prompt_paths.system())
        .await
        .map_err(|source| generic_prompt_resource_failure("读取翻译 system Prompt", source))?;
    let thinking = if let Some(path) = prompt_paths.thinking() {
        Some(
            read_unparsed_prompt_resource(file_system, path)
                .await
                .map_err(|source| {
                    generic_prompt_resource_failure("读取翻译 thinking Prompt", source)
                })?,
        )
    } else {
        None
    };
    let rules = read_unparsed_prompt_resource(file_system, prompt_paths.rules())
        .await
        .map_err(|source| generic_prompt_resource_failure("读取翻译响应规则 Prompt", source))?;
    let example = read_unparsed_prompt_resource(file_system, prompt_paths.example())
        .await
        .map_err(|source| generic_prompt_resource_failure("读取翻译示例 Prompt", source))?;
    let language_pair = language_pair.clone();
    let prompt_cancellation = cancellation.clone();
    cpu.execute(move || {
        ensure_generic_prompt_preparation_running(&prompt_cancellation)?;
        let template = parse_prompt_resource_with_cancellation(template, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::SystemResource)?;
        let rendered_system =
            render_system_prompt_template_with_cancellation(&template, &language_pair, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::SystemTemplate)?;
        let thinking = if let Some(thinking) = thinking {
            let thinking = parse_prompt_resource_with_cancellation(thinking, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::ThinkingResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&thinking, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::ThinkingTemplate)?;
            Some(thinking)
        } else {
            None
        };
        let rules = parse_prompt_resource_with_cancellation(rules, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::RulesResource)?;
        ensure_no_prompt_template_variables_with_cancellation(&rules, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::RulesTemplate)?;

        let example = parse_prompt_resource_with_cancellation(example, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::ExampleResource)?;
        ensure_no_prompt_template_variables_with_cancellation(&example, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::ExampleTemplate)?;
        let system_prompt = assemble_translation_system_prompt_with_cancellation(
            rendered_system,
            thinking,
            rules,
            example,
            || ensure_generic_prompt_preparation_running(&prompt_cancellation),
        )?;

        let fingerprint =
            generic_prompt_fingerprint_with_cancellation(&system_prompt, response_mode, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?;
        Ok::<_, GenericPromptPreparationError>(LoadedGenericPrompt {
            system_prompt,
            response_mode,
            fingerprint,
        })
    })
    .await
    .map_err(|source| generic_cpu_execution_failure("调度 Generic Prompt 准备", source))?
    .map_err(|source| match source {
        GenericPromptPreparationError::Cancelled => GenericCommandError::Cancelled,
        GenericPromptPreparationError::SystemResource(source) => {
            generic_prompt_resource_failure("读取翻译 system Prompt", source)
        }
        GenericPromptPreparationError::ThinkingResource(source) => {
            generic_prompt_resource_failure("读取翻译 thinking Prompt", source)
        }
        GenericPromptPreparationError::RulesResource(source) => {
            generic_prompt_resource_failure("读取翻译响应规则 Prompt", source)
        }
        GenericPromptPreparationError::ExampleResource(source) => {
            generic_prompt_resource_failure("读取翻译示例 Prompt", source)
        }
        GenericPromptPreparationError::SystemTemplate(source) => {
            GenericCommandError::operation("渲染翻译 system Prompt", source)
        }
        GenericPromptPreparationError::ThinkingTemplate(source) => {
            GenericCommandError::operation("校验翻译 thinking Prompt", source)
        }
        GenericPromptPreparationError::RulesTemplate(source) => {
            GenericCommandError::operation("校验翻译响应规则 Prompt", source)
        }
        GenericPromptPreparationError::ExampleTemplate(source) => {
            GenericCommandError::operation("校验翻译示例 Prompt", source)
        }
    })
}

fn ensure_generic_prompt_preparation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPromptPreparationError> {
    if cancellation.is_requested() {
        Err(GenericPromptPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

fn generic_prompt_fingerprint_with_cancellation<E>(
    system_prompt: &str,
    response_mode: TranslationResponseMode,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size =
        std::num::NonZeroUsize::new(64 * 1024).expect("Prompt 指纹取消检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.translation.system-prompt");
    hasher
        .try_frame_chunks(1, system_prompt.as_bytes(), chunk_size, &mut ensure_running)?
        .frame(
            2,
            if response_mode.thinking() {
                b"thinking=true"
            } else {
                b"thinking=false"
            },
        )
        .frame(
            3,
            if response_mode.source_echo() {
                b"source-echo=true"
            } else {
                b"source-echo=false"
            },
        );
    ensure_running()?;
    Ok(hasher.finish())
}

#[derive(Clone)]
struct GenericValidationFact {
    kind: String,
    source_text: String,
    protected: GenericProtectedText,
    analysis: LanguageAnalysis,
}

struct PreparedGenericTranslation {
    plan: TranslationPlan,
    facts: GenericUnitMap<GenericValidationFact>,
}

struct PreparedGenericGroup {
    planning_units: Vec<PlanningUnit>,
    facts: Vec<(GenericUnitKey, GenericValidationFact)>,
}

#[derive(Debug)]
enum GenericPreparationError {
    Cancelled,
    Placeholder(crate::generic::GenericPlaceholderError),
    LanguageProjection(LanguageTextProjectionError),
    Planning(GenericPlanningError),
}

impl fmt::Display for GenericPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic CPU 工作已取消"),
            Self::Placeholder(source) => source.fmt(formatter),
            Self::LanguageProjection(source) => source.fmt(formatter),
            Self::Planning(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Placeholder(source) => Some(source),
            Self::LanguageProjection(source) => Some(source),
            Self::Planning(source) => Some(source),
        }
    }
}

impl GenericPreparationError {
    const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::Planning(source) if source.is_cancelled())
    }
}

#[derive(Clone, Copy, Debug)]
struct GenericOperationCancelled;

impl From<GenericOperationCancelled> for GenericCommandError {
    fn from(_: GenericOperationCancelled) -> Self {
        Self::Cancelled
    }
}

fn ensure_generic_operation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericOperationCancelled> {
    if cancellation.is_requested() {
        Err(GenericOperationCancelled)
    } else {
        Ok(())
    }
}

fn generic_cpu_execution_failure(
    stage: &'static str,
    source: CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> GenericCommandError {
    match source {
        CpuTaskExecutionError::Cancelled => GenericCommandError::Cancelled,
        source @ (CpuTaskExecutionError::Unavailable(_) | CpuTaskExecutionError::TaskPanicked) => {
            GenericCommandError::operation(stage, source)
        }
    }
}

fn generic_project_lease_failure(
    source: ProjectCommandLeaseError<Box<SystemFileSystemError>>,
) -> GenericCommandError {
    match &source {
        ProjectCommandLeaseError::Unavailable {
            source: operation, ..
        } if system_file_system_error_is_cancelled(operation.as_ref()) => {
            GenericCommandError::Cancelled
        }
        ProjectCommandLeaseError::Unavailable { .. } => {
            GenericCommandError::operation("取得项目租约", source)
        }
    }
}

fn system_file_system_error_is_cancelled(source: &SystemFileSystemError) -> bool {
    matches!(
        source,
        SystemFileSystemError::Cancelled { .. }
            | SystemFileSystemError::Windows(WindowsFsError::LockCancelled { .. })
    )
}

fn read_file_error_is_cancelled(source: &ReadFileError<SystemFileSystemError>) -> bool {
    matches!(
        source,
        ReadFileError::Io { source, .. } if system_file_system_error_is_cancelled(source)
    )
}

fn generic_read_file_failure(
    operation: &'static str,
    source: ReadFileError<SystemFileSystemError>,
    stage: DiagnosticStage,
    action: DiagnosticAction,
) -> GenericCommandError {
    if read_file_error_is_cancelled(&source) {
        GenericCommandError::Cancelled
    } else {
        let diagnostic = generic_read_file_diagnostic(&source, stage, action);
        GenericCommandError::diagnosed(operation, source, diagnostic)
    }
}

fn generic_prompt_resource_failure(
    operation: &'static str,
    source: PromptResourceLoadError,
) -> GenericCommandError {
    if matches!(
        &source,
        PromptResourceLoadError::Read(source) if read_file_error_is_cancelled(source)
    ) {
        GenericCommandError::Cancelled
    } else {
        let diagnostic = source.safe_diagnostic();
        GenericCommandError::diagnosed(operation, source, diagnostic)
    }
}

fn generic_translation_resource_failure(
    source: TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>,
) -> GenericCommandError {
    let cancelled = match &source {
        TranslationPlanningResourceReadingError::Cancelled => true,
        TranslationPlanningResourceReadingError::ReadTerminology { source, .. }
        | TranslationPlanningResourceReadingError::ReadPlaceholderRules { source, .. } => {
            read_file_error_is_cancelled(source)
        }
        TranslationPlanningResourceReadingError::ParseTerminologyCompute { source, .. }
        | TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
            source, ..
        } => matches!(source, CpuTaskExecutionError::Cancelled),
        TranslationPlanningResourceReadingError::InvalidTerminology { .. }
        | TranslationPlanningResourceReadingError::InvalidPlaceholderRules { .. } => false,
    };
    if cancelled {
        GenericCommandError::Cancelled
    } else if let Some((operation, worker_source)) = translation_resource_worker_start(&source) {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::operation(operation),
            "spawn_worker",
            worker_source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        );
        GenericCommandError::diagnosed("读取翻译资源", source, diagnostic)
    } else {
        GenericCommandError::operation("读取翻译资源", source)
    }
}

fn translation_resource_worker_start<F, C>(
    source: &TranslationPlanningResourceReadingError<F, C>,
) -> Option<(&'static str, &io::Error)> {
    match source {
        TranslationPlanningResourceReadingError::InvalidTerminology {
            source: TerminologyDefinitionError::StartWorker { operation, source },
            ..
        }
        | TranslationPlanningResourceReadingError::InvalidPlaceholderRules {
            source: PlaceholderDefinitionError::StartWorker { operation, source },
            ..
        } => Some((*operation, source)),
        _ => None,
    }
}

fn generic_preparation_failure(
    stage: &'static str,
    source: GenericPreparationError,
) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else if let GenericPreparationError::Placeholder(
        crate::generic::GenericPlaceholderError::Compilation(
            PlaceholderRuleCompilationError::StartWorker {
                operation,
                source: worker_source,
            },
        ),
    ) = &source
    {
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::operation("custom_placeholder_compile"),
            *operation,
            worker_source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        );
        GenericCommandError::diagnosed(stage, source, diagnostic)
    } else {
        GenericCommandError::operation(stage, source)
    }
}

fn generic_write_back_candidate_failure(source: GenericWriteBackError) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else {
        GenericCommandError::operation("建立 Generic 写回候选", source)
    }
}

fn prepare_generic_translation(
    snapshot: &GenericStoredSnapshot,
    terminology: Arc<CompiledTerminology>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    source_language: Arc<dyn LanguageModule>,
    base_resources: AutomaticStateResources,
    target_task_characters: std::num::NonZeroUsize,
    cancellation: &CooperativeCancellation,
) -> Result<PreparedGenericTranslation, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut groups = Vec::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for group in file.groups() {
            ensure_generic_cpu_running(cancellation)?;
            groups.push(group);
        }
    }
    let prepared_groups = groups
        .par_iter()
        .map(|group| {
            ensure_generic_cpu_running(cancellation)?;
            let service = GenericPlaceholderService::default();
            let mut prepared_units = Vec::with_capacity(group.units().len());
            for unit in group.units() {
                ensure_generic_cpu_running(cancellation)?;
                let protected = service
                    .protect_with_cancellation(
                        group.kind(),
                        unit.source_text(),
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .map_err(GenericPreparationError::Placeholder)?;
                let language_text = protected
                    .language_text_with_cancellation(|| ensure_generic_cpu_running(cancellation))?
                    .map_err(GenericPreparationError::LanguageProjection)?;
                let analysis = source_language
                    .analyze_source_with_cancellation(&language_text, &mut || {
                        ensure_generic_language_running(cancellation)
                    })
                    .map_err(|LanguageOperationCancelled| GenericPreparationError::Cancelled)?;
                ensure_generic_cpu_running(cancellation)?;
                prepared_units.push((unit, protected, language_text, analysis));
            }
            ensure_generic_cpu_running(cancellation)?;
            let term_indices = terminology.triggered_indices_with_cancellation(
                prepared_units
                    .iter()
                    .flat_map(|(_, _, language_text, _)| natural_segments(language_text)),
                || ensure_generic_cpu_running(cancellation),
            )?;
            let terminology_hits = terminology_hit_fingerprint_with_cancellation(
                terminology.as_ref(),
                &term_indices,
                || ensure_generic_cpu_running(cancellation),
            )?;
            let mut planning_units = Vec::with_capacity(prepared_units.len());
            let mut facts = Vec::with_capacity(prepared_units.len());
            for (unit, protected, language_text, analysis) in prepared_units {
                ensure_generic_cpu_running(cancellation)?;
                let resources = AutomaticStateResources {
                    terminology_hits,
                    ..base_resources
                };
                let mut planning = PlanningUnit::from_stored_with_cancellation(
                    snapshot.project(),
                    group,
                    unit,
                    &protected,
                    clone_generic_cpu_indices(&term_indices, cancellation)?,
                    generic_language_text_has_non_whitespace_natural_text(
                        &language_text,
                        cancellation,
                    )? && analysis.needs_translation(),
                    resources,
                    cancellation,
                )
                .map_err(GenericPreparationError::Planning)?;
                if let Some(current_translation) = planning.current_translation() {
                    match service.protect_with_cancellation(
                        group.kind(),
                        current_translation,
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )? {
                        Ok(current_protected)
                            if current_protected.binding_fingerprint()
                                == protected.binding_fingerprint() =>
                        {
                            planning.install_current_target_context(clone_generic_cpu_text(
                                current_protected.text(),
                                cancellation,
                            )?);
                        }
                        Ok(_) | Err(_) => {
                            planning.install_current_source_fallback(clone_generic_cpu_text(
                                protected.text(),
                                cancellation,
                            )?);
                        }
                    }
                }
                if planning.needs_candidate() {
                    facts.push((
                        clone_generic_unit_key(planning.key(), cancellation)?,
                        GenericValidationFact {
                            kind: clone_generic_cpu_text(group.kind(), cancellation)?,
                            source_text: clone_generic_cpu_text(unit.source_text(), cancellation)?,
                            protected,
                            analysis,
                        },
                    ));
                }
                planning_units.push(planning);
            }
            ensure_generic_cpu_running(cancellation)?;
            Ok::<_, GenericPreparationError>(PreparedGenericGroup {
                planning_units,
                facts,
            })
        })
        .collect::<Vec<_>>();

    let mut planning_units = Vec::new();
    let mut facts = GenericUnitMap::new();
    // 并行完成顺序不参与领域语义；按自然 Group 顺序处理结果，保证规划和错误稳定。
    for prepared_group in prepared_groups {
        ensure_generic_cpu_running(cancellation)?;
        let prepared_group = prepared_group?;
        for planning_unit in prepared_group.planning_units {
            ensure_generic_cpu_running(cancellation)?;
            planning_units.push(planning_unit);
        }
        for (key, fact) in prepared_group.facts {
            ensure_generic_cpu_running(cancellation)?;
            let previous = facts
                .insert_with_cancellation(key, fact, || ensure_generic_cpu_running(cancellation))?;
            debug_assert!(previous.is_none());
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    let plan = plan_translation_with_validator_and_cancellation(
        snapshot,
        &planning_units,
        target_task_characters,
        |key, candidate| {
            validate_generic_reuse_with_cancellation(
                key,
                candidate,
                &facts,
                placeholder_rules,
                source_language.as_ref(),
                cancellation,
            )
        },
        cancellation,
    )
    .map_err(GenericPreparationError::Planning)?;
    Ok(PreparedGenericTranslation { plan, facts })
}

fn collect_generic_current_translations(
    snapshot: &GenericStoredSnapshot,
    terminology: &CompiledTerminology,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    automatic_resources: Option<AutomaticStateResources>,
    cancellation: &CooperativeCancellation,
) -> Result<GenericUnitMap<String>, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut groups = Vec::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for group in file.groups() {
            ensure_generic_cpu_running(cancellation)?;
            groups.push(group);
        }
    }
    let prepared_groups = groups
        .par_iter()
        .map(|group| {
            ensure_generic_cpu_running(cancellation)?;
            let service = GenericPlaceholderService::default();
            let mut protected_units = Vec::with_capacity(group.units().len());
            for unit in group.units() {
                ensure_generic_cpu_running(cancellation)?;
                let protected = service
                    .protect_with_cancellation(
                        group.kind(),
                        unit.source_text(),
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .map_err(GenericPreparationError::Placeholder)?;
                let language_text = protected
                    .language_text_with_cancellation(|| ensure_generic_cpu_running(cancellation))?
                    .map_err(GenericPreparationError::LanguageProjection)?;
                ensure_generic_cpu_running(cancellation)?;
                protected_units.push((unit, protected, language_text));
            }
            ensure_generic_cpu_running(cancellation)?;
            let term_indices = match automatic_resources {
                Some(_) => Some(
                    terminology.triggered_indices_with_cancellation(
                        protected_units
                            .iter()
                            .flat_map(|(_, _, language_text)| natural_segments(language_text)),
                        || ensure_generic_cpu_running(cancellation),
                    )?,
                ),
                None => None,
            };
            let group_resources = match automatic_resources {
                Some(resources) => Some(AutomaticStateResources {
                    terminology_hits: terminology_hit_fingerprint_with_cancellation(
                        terminology,
                        term_indices.as_deref().unwrap_or_default(),
                        || ensure_generic_cpu_running(cancellation),
                    )?,
                    ..resources
                }),
                None => None,
            };
            let mut current = Vec::new();
            for (unit, protected, _) in protected_units {
                ensure_generic_cpu_running(cancellation)?;
                if let Some(translation) = current_translation_for_stored_with_cancellation(
                    snapshot.project(),
                    group,
                    unit,
                    protected.binding_fingerprint(),
                    group_resources,
                    cancellation,
                )
                .map_err(GenericPreparationError::Planning)?
                {
                    current.push((
                        GenericUnitKey::new(
                            clone_generic_cpu_text(group.id(), cancellation)?,
                            clone_generic_cpu_text(unit.id(), cancellation)?,
                        ),
                        translation,
                    ));
                }
            }
            ensure_generic_cpu_running(cancellation)?;
            Ok::<_, GenericPreparationError>(current)
        })
        .collect::<Vec<_>>();

    let mut current = GenericUnitMap::new();
    for prepared_group in prepared_groups {
        ensure_generic_cpu_running(cancellation)?;
        for (key, translation) in prepared_group? {
            ensure_generic_cpu_running(cancellation)?;
            let previous = current.insert_with_cancellation(key, translation, || {
                ensure_generic_cpu_running(cancellation)
            })?;
            debug_assert!(previous.is_none());
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(current)
}

fn ensure_generic_cpu_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPreparationError> {
    if cancellation.is_requested() {
        Err(GenericPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

fn ensure_generic_language_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), LanguageOperationCancelled> {
    if cancellation.is_requested() {
        Err(LanguageOperationCancelled)
    } else {
        Ok(())
    }
}

fn clone_generic_cpu_text(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericPreparationError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut output = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_generic_cpu_running(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(output)
}

fn clone_generic_cpu_indices(
    indices: &[usize],
    cancellation: &CooperativeCancellation,
) -> Result<Vec<usize>, GenericPreparationError> {
    const CANCELLATION_CHECK_ITEMS: usize = 1024;

    let mut output = Vec::with_capacity(indices.len());
    for chunk in indices.chunks(CANCELLATION_CHECK_ITEMS) {
        ensure_generic_cpu_running(cancellation)?;
        output.extend_from_slice(chunk);
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(output)
}

fn clone_generic_unit_key(
    key: &GenericUnitKey,
    cancellation: &CooperativeCancellation,
) -> Result<GenericUnitKey, GenericPreparationError> {
    Ok(GenericUnitKey::new(
        clone_generic_cpu_text(key.group_id(), cancellation)?,
        clone_generic_cpu_text(key.unit_id(), cancellation)?,
    ))
}

fn generic_cpu_text_equal(
    left: &str,
    right: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPreparationError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_generic_cpu_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CANCELLATION_CHECK_BYTES)
        .zip(right.as_bytes().chunks(CANCELLATION_CHECK_BYTES))
    {
        ensure_generic_cpu_running(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(true)
}

fn generic_language_text_has_non_whitespace_natural_text(
    text: &LanguageText,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPreparationError> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    for segment in text.segments() {
        ensure_generic_cpu_running(cancellation)?;
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        for (index, character) in text.chars().enumerate() {
            if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
                ensure_generic_cpu_running(cancellation)?;
            }
            if !character.is_whitespace() {
                return Ok(true);
            }
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(false)
}

fn natural_segments(language_text: &LanguageText) -> impl Iterator<Item = &str> {
    language_text
        .segments()
        .iter()
        .filter_map(|segment| match segment {
            LanguageTextSegment::NaturalText(text) => Some(text.as_str()),
            LanguageTextSegment::OpaqueBoundary => None,
        })
}

fn generic_task_response_diagnostic(
    task_index: usize,
    failure: DiagnosticFailureKind,
    detail: impl AsRef<str>,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::ModelRequest,
        DiagnosticStage::ModelRequest,
        DiagnosticSubject::operation(format!(
            "generic_translation_task_{}",
            generic_task_ordinal(task_index)
        )),
        DiagnosticReason::failure_with_detail(failure, detail),
        DiagnosticImpact::ProgressPreserved,
        DiagnosticAction::Retry,
    )
}

fn generic_response_problem_diagnostic(
    task_index: usize,
    problem: &ResponseProblem,
) -> SafeDiagnostic {
    let detail = match problem {
        ResponseProblem::InvalidId(id) => format!("problem=invalid_id; actual_id={id}"),
        ResponseProblem::UnexpectedId(id) => format!("problem=unexpected_id; output_id={id}"),
        ResponseProblem::DuplicateId(id) => format!("problem=duplicate_id; output_id={id}"),
        ResponseProblem::MissingId(id) => format!("problem=missing_id; output_id={id}"),
        ResponseProblem::InvalidValue { output_id, .. } => {
            format!("problem=invalid_value; output_id={output_id}")
        }
        ResponseProblem::InvalidTranslation { output_id, .. } => {
            format!("problem=invalid_translation; output_id={output_id}")
        }
        ResponseProblem::InvalidDestination { output_id, .. } => {
            format!("problem=invalid_destination_translation; output_id={output_id}")
        }
    };
    generic_task_response_diagnostic(
        task_index,
        DiagnosticFailureKind::InvalidResponseContract,
        detail,
    )
}

fn generic_response_parse_diagnostic(
    task_index: usize,
    error: TranslationTaskResponseParseError,
) -> SafeDiagnostic {
    let parse_kind = match error.kind() {
        TranslationTaskResponseParseErrorKind::Json(category) => {
            format!("json_{}", category.code())
        }
        kind => kind.code().to_owned(),
    };
    generic_task_response_diagnostic(
        task_index,
        DiagnosticFailureKind::ResponseParsingFailed,
        format!(
            "parse_kind={parse_kind}; line={}; column={}",
            error.line(),
            error.column()
        ),
    )
}

fn generic_unavailable_task_diagnostic(task_index: usize, reason: &'static str) -> SafeDiagnostic {
    generic_task_response_diagnostic(
        task_index,
        DiagnosticFailureKind::ExternalServiceUnavailable,
        format!("reason={reason}"),
    )
}

fn empty_terminology_fingerprint() -> Sha256Fingerprint {
    Sha256FramedHasher::new(b"att.generic.terminology-hits").finish()
}

#[cfg(test)]
fn terminology_hit_fingerprint(
    terminology: &CompiledTerminology,
    indices: &[usize],
) -> Sha256Fingerprint {
    terminology_hit_fingerprint_with_cancellation(terminology, indices, || {
        Ok::<_, std::convert::Infallible>(())
    })
    .unwrap_or_else(|never| match never {})
}

struct GenericTaskExecution {
    store: GenericProjectStore,
    expected_raw_fingerprint: Sha256Fingerprint,
    profile_id: String,
    tasks: Vec<PlannedTask>,
    facts: Arc<GenericUnitMap<GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_mode: TranslationResponseMode,
    client: Arc<crate::runtime::llm::OpenAiChatCompletionClient>,
    llm: OpenAiChatCompletionExecutor,
    retry_delays: Vec<Duration>,
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    task_records: ConfiguredTranslationTaskRecordSink,
    project_log: GenericTaskProjectLog,
    progress: TerminalProgressObserver<GenericProgressPhase>,
}

#[derive(Clone)]
struct GenericTaskProjectLog {
    logger: ProjectLogger,
    context: ProjectLogContext,
}

impl GenericTaskProjectLog {
    fn started(&self, task_index: usize, total_tasks: usize) {
        self.logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Debug,
            ProjectLogCode::TaskStarted,
            self.context.clone(),
            ProjectLogPayload::Task {
                ordinal: generic_task_ordinal(task_index),
                total: u64::try_from(total_tasks).unwrap_or(u64::MAX),
                outcome: None,
                attempts: None,
            },
        ));
    }

    fn finished(
        &self,
        task_index: usize,
        total_tasks: usize,
        attempts: usize,
        outcome: ProjectLogTaskOutcome,
        diagnostics: impl IntoIterator<Item = SafeDiagnostic>,
    ) {
        let ordinal = generic_task_ordinal(task_index);
        let total = u64::try_from(total_tasks).unwrap_or(u64::MAX);
        let attempts = u64::try_from(attempts).unwrap_or(u64::MAX);
        let finished = ProjectLogEvent::new(
            ProjectLogLevel::Debug,
            ProjectLogCode::TaskFinished,
            self.context.clone(),
            ProjectLogPayload::Task {
                ordinal,
                total,
                outcome: Some(outcome),
                attempts: Some(attempts),
            },
        );
        if outcome == ProjectLogTaskOutcome::Complete {
            self.logger.emit(finished);
        } else {
            self.logger.emit_reliable(finished);
        }
        for diagnostic in diagnostics {
            self.logger.emit_reliable(ProjectLogEvent::new(
                ProjectLogLevel::Warn,
                ProjectLogCode::TaskDiagnostic,
                self.context.clone(),
                ProjectLogPayload::TaskDiagnostic {
                    ordinal,
                    total,
                    attempts,
                    diagnostic,
                },
            ));
        }
    }

    fn partial_result(&self, summary: GenericTaskSummary) {
        if summary.partial_tasks == 0 && summary.unavailable_tasks == 0 {
            return;
        }
        self.logger.emit_reliable(ProjectLogEvent::new(
            ProjectLogLevel::Warn,
            ProjectLogCode::PartialResult,
            self.context.clone(),
            ProjectLogPayload::ResultSummary {
                complete: u64::try_from(summary.complete_tasks).unwrap_or(u64::MAX),
                partial: u64::try_from(summary.partial_tasks).unwrap_or(u64::MAX),
                unavailable: u64::try_from(summary.unavailable_tasks).unwrap_or(u64::MAX),
                manual_review: u64::try_from(summary.response_problems).unwrap_or(u64::MAX),
            },
        ));
    }
}

fn generic_task_ordinal(task_index: usize) -> u64 {
    u64::try_from(task_index)
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct GenericTaskSummary {
    complete_tasks: usize,
    partial_tasks: usize,
    unavailable_tasks: usize,
    accepted_units: usize,
    written_units: usize,
    conflicted_units: usize,
    response_problems: usize,
}

struct GenericTaskRecordDraft {
    total_tasks: usize,
    task_index: usize,
    messages: Vec<ChatMessage>,
    expected_outputs: usize,
    started_at: OffsetDateTime,
    duration: Duration,
    attempt_count: usize,
    attempts: Vec<crate::execution::llm_request::LlmRequestAttemptRecord>,
    response: Option<GenericTaskResponseRecord>,
}

struct GenericTaskRecordInFlight {
    total_tasks: usize,
    task_index: usize,
    messages: Vec<ChatMessage>,
    expected_outputs: usize,
    started_at: OffsetDateTime,
    started: Instant,
    attempt_count: usize,
    attempts: Vec<crate::execution::llm_request::LlmRequestAttemptRecord>,
}

impl GenericTaskRecordInFlight {
    fn finish(self, response: Option<GenericTaskResponseRecord>) -> GenericTaskRecordDraft {
        GenericTaskRecordDraft {
            total_tasks: self.total_tasks,
            task_index: self.task_index,
            messages: self.messages,
            expected_outputs: self.expected_outputs,
            started_at: self.started_at,
            duration: self.started.elapsed(),
            attempt_count: self.attempt_count,
            attempts: self.attempts,
            response,
        }
    }
}

impl GenericTaskRecordDraft {
    fn finish(self, state: GenericTaskRecordState) -> GenericTaskRecordDocument {
        GenericTaskRecordDocument::new(
            self.total_tasks,
            self.task_index,
            self.messages,
            self.expected_outputs,
            self.started_at,
            self.duration,
            self.attempt_count,
            self.attempts,
            self.response,
            state,
        )
    }
}

enum GenericPreparedTaskOutcome {
    Accepted {
        writes: Vec<TranslationWrite>,
        issues: Vec<GenericTaskRecordIssue>,
        diagnostics: Vec<SafeDiagnostic>,
        accepted_units: usize,
        response_problems: usize,
        response_complete: bool,
        accepted_outputs: usize,
    },
    Unavailable {
        reason: &'static str,
        diagnostic: SafeDiagnostic,
    },
    Failed(GenericCommandError),
    Cancelled,
}

struct GenericPreparedTask {
    task_index: usize,
    outcome: GenericPreparedTaskOutcome,
    record: Option<GenericTaskRecordDraft>,
    attempt_count: usize,
}

fn cancelled_generic_prepared_task(
    task_index: usize,
    record: Option<GenericTaskRecordInFlight>,
    attempt_count: usize,
) -> GenericPreparedTask {
    GenericPreparedTask {
        task_index,
        outcome: GenericPreparedTaskOutcome::Cancelled,
        record: record.map(|record| record.finish(None)),
        attempt_count,
    }
}

#[derive(Clone)]
struct GenericTaskRequestContext {
    total_tasks: usize,
    facts: Arc<GenericUnitMap<GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: Arc<String>,
    response_mode: TranslationResponseMode,
    client: Arc<crate::runtime::llm::OpenAiChatCompletionClient>,
    llm: OpenAiChatCompletionExecutor,
    retry_delays: Arc<Vec<Duration>>,
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
}

async fn execute_owned_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let render_terminology = Arc::clone(&context.terminology);
    let render_system_prompt = Arc::clone(&context.system_prompt);
    let render_cancellation = context.cancellation.clone();
    let (task, expected_outputs, system_prompt, user_message) = context
        .cpu
        .execute(move || {
            let user_message = render_generic_user_message_with_cancellation(
                &task,
                render_terminology.as_ref(),
                &render_cancellation,
            )
            .map_err(GenericPreparationError::Planning)?;
            let mut expected_outputs = 0_usize;
            for _ in task.expected_output_ids() {
                ensure_generic_cpu_running(&render_cancellation)?;
                expected_outputs += 1;
            }
            let system_prompt =
                clone_generic_cpu_text(render_system_prompt.as_str(), &render_cancellation)?;
            Ok::<_, GenericPreparationError>((task, expected_outputs, system_prompt, user_message))
        })
        .await
        .map_err(|source| generic_cpu_execution_failure("调度 Generic 模型消息", source))?
        .map_err(|source| {
            if source.is_cancelled() {
                GenericCommandError::Cancelled
            } else {
                generic_preparation_failure("建立 Generic 模型消息", source)
            }
        })?;
    execute_generic_task(
        context.total_tasks,
        task_index,
        task,
        expected_outputs,
        user_message,
        Arc::clone(&context.facts),
        context.placeholder_rules.clone(),
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
    )
    .await
}

async fn execute_generic_tasks(
    input: GenericTaskExecution,
) -> Result<GenericTaskSummary, GenericCommandError> {
    let GenericTaskExecution {
        store,
        expected_raw_fingerprint,
        profile_id,
        tasks,
        facts,
        placeholder_rules,
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
        progress,
    } = input;
    let total_tasks = tasks.len();
    let record_evidence = task_records.enabled();
    let concurrency = client.max_concurrent_requests().get();
    let request_context = GenericTaskRequestContext {
        total_tasks,
        facts,
        placeholder_rules,
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
    };
    let mut remaining = tasks.into_iter().enumerate();
    let mut tasks = FuturesOrdered::new();
    for _ in 0..concurrency {
        let Some((task_index, task)) = remaining.next() else {
            break;
        };
        project_log.started(task_index, total_tasks);
        tasks.push_back(execute_owned_generic_task(
            request_context.clone(),
            task_index,
            task,
        ));
    }

    let mut summary = GenericTaskSummary::default();
    let mut terminal_error = None;
    while let Some(prepared) = tasks.next().await {
        let GenericPreparedTask {
            task_index,
            outcome,
            record,
            attempt_count,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if terminal_error.is_none() {
                    cancellation.request();
                    terminal_error = Some(error);
                }
                continue;
            }
        };
        if terminal_error.is_some() {
            if let Some(record) = record {
                let state = match outcome {
                    GenericPreparedTaskOutcome::Cancelled => GenericTaskRecordState::cancelled(),
                    GenericPreparedTaskOutcome::Unavailable { reason, .. } => {
                        GenericTaskRecordState::unavailable(reason)
                    }
                    GenericPreparedTaskOutcome::Accepted { .. } => {
                        GenericTaskRecordState::not_committed_due_to_prior_failure()
                    }
                    GenericPreparedTaskOutcome::Failed(ref error) => {
                        GenericTaskRecordState::failed(error.safe_diagnostic())
                    }
                };
                task_records.submit(record.finish(state));
            }
            continue;
        }
        match outcome {
            GenericPreparedTaskOutcome::Accepted {
                writes,
                mut issues,
                diagnostics,
                accepted_units,
                response_problems,
                response_complete,
                accepted_outputs,
            } => {
                let commit = if writes.is_empty() {
                    Ok(CommitTranslationsOutcome {
                        committed: 0,
                        conflicts: Vec::new(),
                    })
                } else {
                    let store = store.clone();
                    let profile_id = profile_id.clone();
                    run_project_blocking(
                        "提交 Generic 模型译文",
                        DiagnosticStage::Translate,
                        DiagnosticImpact::ProgressPreserved,
                        DiagnosticAction::Retry,
                        move || {
                            store.commit_translations_for_profile(
                                expected_raw_fingerprint,
                                &writes,
                                &profile_id,
                            )
                        },
                    )
                    .await
                };
                let commit =
                    match commit {
                        Ok(commit) => commit,
                        Err(error) => {
                            project_log.finished(
                                task_index,
                                total_tasks,
                                attempt_count,
                                ProjectLogTaskOutcome::Failed,
                                [error.safe_diagnostic()],
                            );
                            if let Some(record) = record {
                                task_records.submit(record.finish(GenericTaskRecordState::failed(
                                    error.safe_diagnostic(),
                                )));
                            }
                            cancellation.request();
                            terminal_error = Some(error);
                            continue;
                        }
                    };
                if !commit.conflicts.is_empty() {
                    issues.push(GenericTaskRecordIssue::commit_conflicts(
                        commit.conflicts.len(),
                    ));
                }
                let mut diagnostics = diagnostics;
                if !commit.conflicts.is_empty() {
                    diagnostics.push(SafeDiagnostic::new(
                        DiagnosticCode::ProjectState,
                        DiagnosticStage::Translate,
                        DiagnosticSubject::operation(format!(
                            "generic_translation_task_{}",
                            generic_task_ordinal(task_index)
                        )),
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::StateMismatch,
                            format!("cas_conflicts={}", commit.conflicts.len()),
                        ),
                        DiagnosticImpact::ProgressPreserved,
                        DiagnosticAction::Retry,
                    ));
                }
                let complete = response_complete && commit.conflicts.is_empty();
                project_log.finished(
                    task_index,
                    total_tasks,
                    attempt_count,
                    if complete {
                        ProjectLogTaskOutcome::Complete
                    } else {
                        ProjectLogTaskOutcome::Partial
                    },
                    diagnostics,
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::committed(
                        response_complete,
                        accepted_outputs,
                        commit.committed,
                        issues,
                    )));
                }
                summary.accepted_units += accepted_units;
                summary.response_problems += response_problems;
                summary.written_units += commit.committed;
                summary.conflicted_units += commit.conflicts.len();
                if complete {
                    summary.complete_tasks += 1;
                } else {
                    summary.partial_tasks += 1;
                }
            }
            GenericPreparedTaskOutcome::Unavailable { reason, diagnostic } => {
                project_log.finished(
                    task_index,
                    total_tasks,
                    attempt_count,
                    ProjectLogTaskOutcome::Unavailable,
                    [diagnostic],
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::unavailable(reason)));
                }
                summary.unavailable_tasks += 1;
            }
            GenericPreparedTaskOutcome::Failed(error) => {
                let diagnostic = error.safe_diagnostic();
                project_log.finished(
                    task_index,
                    total_tasks,
                    attempt_count,
                    ProjectLogTaskOutcome::Failed,
                    [diagnostic.clone()],
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::failed(diagnostic)));
                }
                cancellation.request();
                terminal_error = Some(error);
            }
            GenericPreparedTaskOutcome::Cancelled => {
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
                u64::try_from(confirmed).unwrap_or(u64::MAX),
                u64::try_from(total_tasks).unwrap_or(u64::MAX),
            ));
        }
        if terminal_error.is_none()
            && let Some((task_index, task)) = remaining.next()
        {
            project_log.started(task_index, total_tasks);
            tasks.push_back(execute_owned_generic_task(
                request_context.clone(),
                task_index,
                task,
            ));
        }
    }
    match terminal_error {
        Some(error) => Err(error),
        None => {
            project_log.partial_result(summary);
            Ok(summary)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_generic_task(
    total_tasks: usize,
    task_index: usize,
    task: PlannedTask,
    expected_outputs: usize,
    user_message: String,
    facts: Arc<GenericUnitMap<GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_mode: TranslationResponseMode,
    client: &crate::runtime::llm::OpenAiChatCompletionClient,
    llm: &OpenAiChatCompletionExecutor,
    retry_delays: &[Duration],
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let messages = [
        ChatMessage::new(ChatMessageRole::System, system_prompt),
        ChatMessage::new(ChatMessageRole::User, user_message),
    ];
    let record_started = record_evidence.then(|| (OffsetDateTime::now_utc(), Instant::now()));
    let execution = execute_llm_request_with_retry(
        llm,
        client,
        &messages,
        LlmRequestRetryPolicy::new(retry_delays, max_retry_after),
        &TokioDelay,
        &cancellation,
        record_evidence,
    )
    .await;
    let record_messages =
        record_evidence.then(|| messages.into_iter().collect::<Vec<ChatMessage>>());
    let (outcome, evidence) = execution.into_parts();
    let (attempt_count, attempts) = evidence.into_parts();
    let record = record_started.map(|(started_at, started)| GenericTaskRecordInFlight {
        total_tasks,
        task_index,
        messages: record_messages.expect("启用记录时必须保留模型消息"),
        expected_outputs,
        started_at,
        started,
        attempt_count,
        attempts,
    });
    let response_cancellation = cancellation.clone();
    let processing = cpu
        .execute(move || {
            ensure_generic_response_processing_running(&response_cancellation)?;
            let mut response_record = None;
            let outcome = match outcome {
                LlmRequestExecutionOutcome::Response { response, .. } => {
                    let (content, finish_reason) = response.into_content_and_finish_reason();
                    ensure_generic_response_processing_running(&response_cancellation)?;
                    if matches!(finish_reason, LlmFinishReason::Stop) {
                        match parse_translation_response_with_cancellation(
                            &content,
                            response_mode,
                            || {
                                ensure_generic_response_processing_running(
                                    &response_cancellation,
                                )
                            },
                        )? {
                            Ok(parsed) => {
                                ensure_generic_response_processing_running(
                                    &response_cancellation,
                                )?;
                                let acceptance =
                                    accept_generic_response_with_cancellation(
                                        task,
                                        &parsed,
                                        facts.as_ref(),
                                        &placeholder_rules,
                                        language_module.as_ref(),
                                        &response_cancellation,
                                    )?;
                                let accepted_outputs = acceptance.accepted_output_count();
                                let (accepted, problems) = acceptance.into_parts();
                                let accepted_units = accepted.len();
                                let response_problems = problems.len();
                                let response_complete = problems.is_empty();
                                let mut issues = Vec::with_capacity(problems.len());
                                let mut diagnostics = Vec::with_capacity(problems.len());
                                for problem in &problems {
                                    ensure_generic_response_processing_running(
                                        &response_cancellation,
                                    )?;
                                    diagnostics.push(generic_response_problem_diagnostic(
                                        task_index,
                                        problem,
                                    ));
                                    issues.push(
                                        GenericTaskRecordIssue::from_response_problem_with_cancellation(
                                            problem,
                                            || {
                                                ensure_generic_response_processing_running(
                                                    &response_cancellation,
                                                )
                                            },
                                        )?,
                                    );
                                }
                                let mut writes = Vec::with_capacity(accepted.len());
                                for accepted in accepted {
                                    ensure_generic_response_processing_running(
                                        &response_cancellation,
                                    )?;
                                    writes.push(accepted.into_write());
                                }
                                if record_evidence {
                                    response_record = Some(
                                        GenericTaskResponseRecord::parsed_with_cancellation(
                                            content,
                                            parsed,
                                            || {
                                                ensure_generic_response_processing_running(
                                                    &response_cancellation,
                                                )
                                            },
                                        )?,
                                    );
                                }
                                GenericPreparedTaskOutcome::Accepted {
                                    writes,
                                    issues,
                                    diagnostics,
                                    accepted_units,
                                    response_problems,
                                    response_complete,
                                    accepted_outputs,
                                }
                            }
                            Err(error) => {
                                ensure_generic_response_processing_running(
                                    &response_cancellation,
                                )?;
                                let diagnostic =
                                    generic_response_parse_diagnostic(task_index, error);
                                if record_evidence {
                                    response_record =
                                        Some(GenericTaskResponseRecord::invalid(content, error));
                                }
                                GenericPreparedTaskOutcome::Unavailable {
                                    reason: "model_response_unusable",
                                    diagnostic,
                                }
                            }
                        }
                    } else {
                        let finish_reason = finish_reason.to_string();
                        if record_evidence {
                            response_record =
                                Some(GenericTaskResponseRecord::unprocessed(content));
                        }
                        GenericPreparedTaskOutcome::Unavailable {
                            reason: "non_stop_finish",
                            diagnostic: generic_task_response_diagnostic(
                                task_index,
                                DiagnosticFailureKind::InvalidResponseContract,
                                format!("finish_reason={finish_reason}"),
                            ),
                        }
                    }
                }
                LlmRequestExecutionOutcome::RetryAfterExceedsMaximum { diagnostic, .. } => {
                    GenericPreparedTaskOutcome::Unavailable {
                        reason: "retry_after_exceeds_maximum",
                        diagnostic,
                    }
                }
                LlmRequestExecutionOutcome::RetryBudgetExhausted { diagnostic, .. } => {
                    GenericPreparedTaskOutcome::Unavailable {
                        reason: "recoverable_request_exhausted",
                        diagnostic,
                    }
                }
                LlmRequestExecutionOutcome::Fatal {
                    cancelled,
                    diagnostic,
                    ..
                } => {
                    if cancelled {
                        GenericPreparedTaskOutcome::Cancelled
                    } else {
                        GenericPreparedTaskOutcome::Unavailable {
                            reason: "request_failed",
                            diagnostic: diagnostic.unwrap_or_else(|| {
                                generic_unavailable_task_diagnostic(task_index, "request_failed")
                            }),
                        }
                    }
                }
                LlmRequestExecutionOutcome::Cancelled { .. } => {
                    GenericPreparedTaskOutcome::Cancelled
                }
            };
            Ok::<_, GenericPlanningError>((outcome, response_record))
        })
        .await;
    let (outcome, response_record) = match processing {
        Err(CpuTaskExecutionError::Cancelled) => {
            return Ok(cancelled_generic_prepared_task(
                task_index,
                record,
                attempt_count,
            ));
        }
        Err(source) => {
            let error = generic_cpu_execution_failure("调度 Generic 响应验收", source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed(error),
                record: record.map(|record| record.finish(None)),
                attempt_count,
            });
        }
        Ok(Err(source)) if source.is_cancelled() => {
            return Ok(cancelled_generic_prepared_task(
                task_index,
                record,
                attempt_count,
            ));
        }
        Ok(Err(source)) => {
            let error = GenericCommandError::operation("验收 Generic 模型响应", source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed(error),
                record: record.map(|record| record.finish(None)),
                attempt_count,
            });
        }
        Ok(Ok(processed)) => processed,
    };
    Ok(GenericPreparedTask {
        task_index,
        outcome,
        record: record.map(|record| record.finish(response_record)),
        attempt_count,
    })
}

#[cfg(test)]
fn render_generic_user_message(task: &PlannedTask, terminology: &CompiledTerminology) -> String {
    render_generic_user_message_with_cancellation(
        task,
        terminology,
        &CooperativeCancellation::default(),
    )
    .expect("不取消的受信模型消息必须可以渲染")
}

fn render_generic_user_message_with_cancellation(
    task: &PlannedTask,
    terminology: &CompiledTerminology,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericPlanningError> {
    ensure_message_render_running(cancellation)?;
    let mut selected_terminology = Vec::with_capacity(task.terminology_indices().len());
    for index in task.terminology_indices() {
        ensure_message_render_running(cancellation)?;
        let entry = &terminology.entries()[*index];
        selected_terminology.push(TranslationUserTerminology::new(
            entry.term(),
            entry.translation(),
        ));
    }
    let mut groups = Vec::with_capacity(task.groups().len());
    for group in task.groups() {
        ensure_message_render_running(cancellation)?;
        let mut units = Vec::with_capacity(group.units().len());
        for unit in group.units() {
            ensure_message_render_running(cancellation)?;
            units.push(match unit.output_id() {
                Some(id) => TranslationUserUnit::translated(
                    id,
                    None,
                    TranslationReturnType::Free,
                    unit.text(),
                ),
                None => TranslationUserUnit::context(None, unit.text()),
            });
        }
        groups.push(TranslationUserGroup::new(group.kind(), units));
    }
    ensure_message_render_running(cancellation)?;
    render_translation_user_message(
        &TranslationUserMessage::new(selected_terminology, groups),
        cancellation,
    )
    .map_err(|_| GenericPlanningError::Cancelled)
}

fn ensure_message_render_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    if cancellation.is_requested() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn accept_generic_response_with(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    mut validator: impl FnMut(&GenericValidationFact, &str) -> Result<String, String>,
) -> TranslationAcceptance {
    accept_generic_response_with_validator_and_cancellation(
        task,
        parsed,
        facts,
        |fact, candidate| Ok(validator(fact, candidate)),
        &CooperativeCancellation::default(),
    )
    .expect("不取消的受信 Generic 响应必须完成验收")
}

fn accept_generic_response_with_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPlanningError> {
    accept_generic_response_with_validator_and_cancellation(
        task,
        parsed,
        facts,
        |fact, candidate| {
            validate_generic_candidate_fact_with_cancellation(
                fact,
                candidate,
                placeholder_rules,
                language_module,
                cancellation,
            )
        },
        cancellation,
    )
}

fn accept_generic_response_with_validator_and_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    mut validator: impl FnMut(
        &GenericValidationFact,
        &str,
    ) -> Result<Result<String, String>, GenericPlanningError>,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPlanningError> {
    let mut cache = HashMap::<TaskId, CancellableTextMap<&str, Result<String, String>>>::new();
    accept_parsed_response_with_cancellation(
        task,
        parsed,
        |output_id, key, candidate| {
            ensure_generic_response_processing_running(cancellation)?;
            let Some(fact) = facts.get_with_cancellation(key, || {
                ensure_generic_response_processing_running(cancellation)
            })?
            else {
                return Ok(Err("响应代表项不属于当前 Generic 计划".to_owned()));
            };
            let output_cache = cache
                .entry(output_id)
                .or_insert_with(|| CancellableTextMap::with_capacity(1));
            if let Some(cached) = output_cache.get_with_cancellation(fact.kind.as_str(), || {
                ensure_generic_response_processing_running(cancellation)
            })? {
                return clone_generic_validation_result(cached, cancellation);
            }

            // 一个 output_id 只对应一个全局去重族；同族的原文、保护后文本和实际
            // Placeholder 绑定相同。kind 仍会改变 scope，因此必须分别验收。
            let validated = validator(fact, candidate)?;
            let returned = clone_generic_validation_result(&validated, cancellation)?;
            let previous =
                output_cache.insert_with_cancellation(fact.kind.as_str(), validated, || {
                    ensure_generic_response_processing_running(cancellation)
                })?;
            debug_assert!(previous.is_none());
            Ok(returned)
        },
        || cancellation.is_requested(),
    )
}

#[cfg(test)]
fn validate_generic_candidate(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<String, String> {
    let fact = facts
        .get_with_cancellation(key, || Ok::<_, std::convert::Infallible>(()))
        .unwrap_or_else(|never| match never {})
        .ok_or_else(|| "响应代表项不属于当前 Generic 计划".to_owned())?;
    validate_generic_candidate_fact(fact, candidate, placeholder_rules, language_module)
}

fn validate_generic_reuse_with_cancellation(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<Result<ValidatedReuse, String>, GenericPlanningError> {
    ensure_generic_response_processing_running(cancellation)?;
    let Some(fact) = facts.get_with_cancellation(key, || {
        ensure_generic_response_processing_running(cancellation)
    })?
    else {
        return Ok(Err("响应代表项不属于当前 Generic 计划".to_owned()));
    };
    let final_translation = match validate_generic_candidate_fact_with_cancellation(
        fact,
        candidate,
        placeholder_rules,
        language_module,
        cancellation,
    )? {
        Ok(translation) => translation,
        Err(detail) => return Ok(Err(detail)),
    };
    let service = GenericPlaceholderService::default();
    let context = match service.protect_with_cancellation(
        &fact.kind,
        &final_translation,
        placeholder_rules,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(context) => context,
        Err(source) => return Ok(Err(source.to_string())),
    };
    let context_binding = context.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    let expected_binding = fact.protected.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    if context_binding != expected_binding {
        return Ok(Err(
            "复用译文的 Placeholder 绑定与目标 Generic Unit 不一致".to_owned()
        ));
    }
    let mut context_text = String::with_capacity(context.text().len());
    append_generic_response_text(&mut context_text, context.text(), cancellation)?;
    Ok(Ok(ValidatedReuse::new(final_translation, context_text)))
}

#[cfg(test)]
fn validate_generic_candidate_fact(
    fact: &GenericValidationFact,
    candidate: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<String, String> {
    let service = GenericPlaceholderService::default();
    let restored = service
        .restore(&fact.protected, candidate)
        .map_err(|source| source.to_string())?;
    let candidate_protected = service
        .protect(&fact.kind, &restored, placeholder_rules)
        .map_err(|source| source.to_string())?;
    let language_text = candidate_protected
        .language_text()
        .map_err(|source| source.to_string())?;
    if language_module
        .find_source_residual(&fact.analysis, &language_text)
        .map_err(|source| source.to_string())?
        .is_some()
    {
        return Err("译文仍含不允许保留的源语言文本".to_owned());
    }
    let repair = language_module
        .plan_translation_repair(&fact.analysis, &language_text)
        .map_err(|source| source.to_string())?;
    let repaired = language_text
        .apply_repair(&repair)
        .map_err(|source| source.to_string())?;
    let final_translation = rebuild_original_placeholders(&candidate_protected, &repaired)?;
    crate::generic::validate_translation_placeholders(
        &service,
        placeholder_rules,
        &fact.kind,
        &fact.source_text,
        &final_translation,
    )
    .map_err(|source| source.to_string())?;
    if placeholder_token::contains_reserved_prefix(&final_translation) {
        return Err("译文恢复后仍含 ATT 保留 token".to_owned());
    }
    Ok(final_translation)
}

fn validate_generic_candidate_fact_with_cancellation(
    fact: &GenericValidationFact,
    candidate: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<Result<String, String>, GenericPlanningError> {
    ensure_generic_response_processing_running(cancellation)?;
    let service = GenericPlaceholderService::default();
    let restored = match service.restore_with_cancellation(&fact.protected, candidate, || {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(restored) => restored,
        Err(source) => return Ok(Err(source.to_string())),
    };
    let candidate_protected =
        match service.protect_with_cancellation(&fact.kind, &restored, placeholder_rules, || {
            ensure_generic_response_processing_running(cancellation)
        })? {
            Ok(protected) => protected,
            Err(source) => return Ok(Err(source.to_string())),
        };
    let language_text = match candidate_protected.language_text_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(text) => text,
        Err(source) => return Ok(Err(source.to_string())),
    };
    let residual = match language_module.find_source_residual_with_cancellation(
        &fact.analysis,
        &language_text,
        &mut || ensure_generic_language_running(cancellation),
    ) {
        Ok(Ok(residual)) => residual,
        Ok(Err(source)) => return Ok(Err(source.to_string())),
        Err(LanguageOperationCancelled) => return Err(GenericPlanningError::Cancelled),
    };
    if residual.is_some() {
        return Ok(Err("译文仍含不允许保留的源语言文本".to_owned()));
    }
    let repair = match language_module.plan_translation_repair_with_cancellation(
        &fact.analysis,
        &language_text,
        &mut || ensure_generic_language_running(cancellation),
    ) {
        Ok(Ok(repair)) => repair,
        Ok(Err(source)) => return Ok(Err(source.to_string())),
        Err(LanguageOperationCancelled) => return Err(GenericPlanningError::Cancelled),
    };
    let repaired = match language_text.apply_repair_with_cancellation(&repair, || {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(repaired) => repaired,
        Err(source) => return Ok(Err(source.to_string())),
    };
    ensure_generic_response_processing_running(cancellation)?;
    let final_translation = match rebuild_original_placeholders_with_cancellation(
        &candidate_protected,
        &repaired,
        cancellation,
    )? {
        Ok(translation) => translation,
        Err(detail) => return Ok(Err(detail)),
    };
    match crate::generic::validate_translation_placeholders_with_cancellation(
        &service,
        placeholder_rules,
        &fact.kind,
        &fact.source_text,
        &final_translation,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(()) => {}
        Err(source) => return Ok(Err(source.to_string())),
    }
    if contains_reserved_prefix_with_cancellation(&final_translation, cancellation)? {
        return Ok(Err("译文恢复后仍含 ATT 保留 token".to_owned()));
    }
    ensure_generic_response_processing_running(cancellation)?;
    Ok(Ok(final_translation))
}

#[cfg(test)]
fn rebuild_original_placeholders(
    protected: &GenericProtectedText,
    repaired: &LanguageText,
) -> Result<String, String> {
    let mut output = String::new();
    let mut placeholders = protected.placeholders().iter();
    for segment in repaired.segments() {
        match segment {
            LanguageTextSegment::NaturalText(text) => output.push_str(text),
            LanguageTextSegment::OpaqueBoundary => {
                let placeholder = placeholders
                    .next()
                    .ok_or_else(|| "语言修复增加了 Placeholder 边界".to_owned())?;
                output.push_str(placeholder.original());
            }
        }
    }
    if placeholders.next().is_some() {
        return Err("语言修复删除了 Placeholder 边界".to_owned());
    }
    Ok(output)
}

fn rebuild_original_placeholders_with_cancellation(
    protected: &GenericProtectedText,
    repaired: &LanguageText,
    cancellation: &CooperativeCancellation,
) -> Result<Result<String, String>, GenericPlanningError> {
    ensure_generic_response_processing_running(cancellation)?;
    let mut output = String::new();
    let mut placeholders = protected.placeholders().iter();
    for segment in repaired.segments() {
        ensure_generic_response_processing_running(cancellation)?;
        match segment {
            LanguageTextSegment::NaturalText(text) => {
                append_generic_response_text(&mut output, text, cancellation)?;
            }
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return Ok(Err("语言修复增加了 Placeholder 边界".to_owned()));
                };
                append_generic_response_text(&mut output, placeholder.original(), cancellation)?;
            }
        }
    }
    ensure_generic_response_processing_running(cancellation)?;
    if placeholders.next().is_some() {
        return Ok(Err("语言修复删除了 Placeholder 边界".to_owned()));
    }
    Ok(Ok(output))
}

fn ensure_generic_response_processing_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    if cancellation.is_requested() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

fn append_generic_response_text(
    output: &mut String,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < text.len() {
        ensure_generic_response_processing_running(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_generic_response_processing_running(cancellation)
}

fn clone_generic_validation_result(
    result: &Result<String, String>,
    cancellation: &CooperativeCancellation,
) -> Result<Result<String, String>, GenericPlanningError> {
    let mut cloned = String::new();
    match result {
        Ok(value) => {
            append_generic_response_text(&mut cloned, value, cancellation)?;
            Ok(Ok(cloned))
        }
        Err(detail) => {
            append_generic_response_text(&mut cloned, detail, cancellation)?;
            Ok(Err(cloned))
        }
    }
}

fn contains_reserved_prefix_with_cancellation(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let prefix = placeholder_token::PREFIX.as_bytes();

    for (index, window) in text.as_bytes().windows(prefix.len()).enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_generic_response_processing_running(cancellation)?;
        }
        if window == prefix {
            return Ok(true);
        }
    }
    ensure_generic_response_processing_running(cancellation)?;
    Ok(false)
}

fn add_commit_outcome(
    summary: &mut GenericTranslationSummary,
    outcome: &CommitTranslationsOutcome,
) {
    summary.written_units += outcome.committed;
    summary.conflicted_units += outcome.conflicts.len();
}

#[cfg(test)]
fn should_apply_translation_resources(
    current_terminology_json: &str,
    current_placeholder_rules_json: &str,
    terminology_json: &str,
    placeholder_rules_json: &str,
    invalidation_count: usize,
) -> bool {
    invalidation_count != 0
        || current_terminology_json != terminology_json
        || current_placeholder_rules_json != placeholder_rules_json
}

fn should_remember_profile_separately(summary: &GenericTranslationSummary) -> bool {
    summary.written_units == 0
}

fn merge_task_summary(summary: &mut GenericTranslationSummary, tasks: GenericTaskSummary) {
    summary.complete_tasks += tasks.complete_tasks;
    summary.partial_tasks += tasks.partial_tasks;
    summary.unavailable_tasks += tasks.unavailable_tasks;
    summary.accepted_units += tasks.accepted_units;
    summary.written_units += tasks.written_units;
    summary.conflicted_units += tasks.conflicted_units;
    summary.response_problems += tasks.response_problems;
}

async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &super::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, GenericCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(|source| {
                generic_read_file_failure(
                    "读取附加 PEM",
                    source,
                    DiagnosticStage::CommandPreparation,
                    DiagnosticAction::FixConfiguration,
                )
            })?;
        roots.push(file.into_bytes());
    }
    Ok(roots)
}

#[derive(Clone, Copy, Debug, Default)]
struct TokioDelay;

impl AsyncDelay for TokioDelay {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

fn start_file_system(
    configuration: crate::runtime::filesystem::SystemFileSystemConfig,
    performance: Arc<RunPerformanceCounters>,
) -> Result<SystemFileSystem, SystemFileSystemBuildError> {
    SystemFileSystem::new_with_performance(configuration, performance)
}

type GenericProjectLogSlot = Arc<Mutex<Option<ActiveProjectLog>>>;

fn generic_project_log_slot() -> GenericProjectLogSlot {
    Arc::new(Mutex::new(None))
}

#[allow(clippy::too_many_arguments)]
fn start_existing_generic_project_log(
    slot: &GenericProjectLogSlot,
    common: &crate::application::config::CommonCommandConfiguration,
    locale: UiLocale,
    project: &ProjectName,
    command: &'static str,
    stage: DiagnosticStage,
    performance: Arc<RunPerformanceCounters>,
) {
    if !generic_workspace(common.projects_root(), project).is_dir() {
        return;
    }
    install_generic_project_log(
        slot,
        start_command_log(CommandLogStart {
            common,
            locale,
            engine: GENERIC_ENGINE_NAME,
            project: project.as_str(),
            command,
            stage,
            profile: None,
            performance,
            panic_boundary: None,
        }),
    );
}

fn install_generic_project_log(slot: &GenericProjectLogSlot, project_log: ActiveProjectLog) {
    let mut current = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        current.is_none(),
        "一条 Generic 命令只能建立一个项目日志会话"
    );
    *current = Some(project_log);
}

fn take_generic_project_log(slot: &GenericProjectLogSlot) -> Option<ActiveProjectLog> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn generic_task_project_log(slot: &GenericProjectLogSlot) -> GenericTaskProjectLog {
    let project_log = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project_log = project_log
        .as_ref()
        .expect("Generic Translate 必须在建立模型任务前建立项目日志");
    GenericTaskProjectLog {
        logger: project_log.logger().clone(),
        context: project_log.context().clone(),
    }
}

fn configure_generic_task_records(
    requested: bool,
    project_log: &GenericProjectLogSlot,
    file_system_configuration: &crate::runtime::filesystem::SystemFileSystemConfig,
    client: crate::llm::LlmClientRecordMetadata,
    locale: UiLocale,
    cpu: RayonCpuExecutor,
    project_workspace: &Path,
) -> ConfiguredTranslationTaskRecordSink {
    if !requested {
        return ConfiguredTranslationTaskRecordSink::disabled();
    }
    let (run_id, performance, logger) = {
        let project_log = project_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(project_log) = project_log.as_ref() else {
            return ConfiguredTranslationTaskRecordSink::disabled();
        };
        let Some(run_id) = project_log.run_id() else {
            return ConfiguredTranslationTaskRecordSink::disabled();
        };
        (
            run_id.to_owned(),
            Arc::clone(project_log.performance()),
            project_log.logger().clone(),
        )
    };
    match SystemFileSystem::new_with_performance(file_system_configuration.clone(), performance) {
        Ok(file_system) => ConfiguredTranslationTaskRecordSink::Markdown(Box::new(
            MarkdownTranslationTaskRecordSink::new(
                project_workspace.join("task-records").join(&run_id),
                run_id,
                client,
                locale,
                cpu,
                file_system,
                logger,
            ),
        )),
        Err(error) => {
            logger.record_task_record_failure(error.safe_diagnostic());
            ConfiguredTranslationTaskRecordSink::disabled()
        }
    }
}

fn generic_workspace(projects_root: &Path, project: &ProjectName) -> PathBuf {
    projects_root
        .join(GENERIC_ENGINE_NAME)
        .join(project.as_str())
}

async fn run_project_blocking<T>(
    stage: &'static str,
    diagnostic_stage: DiagnosticStage,
    impact: DiagnosticImpact,
    fallback_action: DiagnosticAction,
    operation: impl FnOnce() -> Result<T, GenericProjectError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| GenericCommandError::operation(stage, source))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) if source.is_cancelled() => Err(GenericCommandError::Cancelled),
        Err(source) => {
            let diagnostic =
                source.safe_diagnostic_source(diagnostic_stage, impact, fallback_action);
            Err(GenericCommandError::diagnosed(stage, source, diagnostic))
        }
    }
}

async fn run_scratch_blocking<T>(
    stage: &'static str,
    operation: impl FnOnce() -> Result<T, GenericScratchError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| GenericCommandError::operation(stage, source))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) => Err(generic_scratch_command_error(stage, source)),
    }
}

fn generic_scratch_command_error(
    stage: &'static str,
    source: GenericScratchError,
) -> GenericCommandError {
    if matches!(&source, GenericScratchError::Cancelled) {
        return GenericCommandError::Cancelled;
    }
    if matches!(
        &source,
        GenericScratchError::CleanupAfterFailure { operation, .. }
            if matches!(operation.as_ref(), GenericScratchError::Cancelled)
    ) {
        let GenericScratchError::CleanupAfterFailure { cleanup, .. } = source else {
            unreachable!("上方模式已经确认取消后的清理失败");
        };
        let recovery_paths = scratch_cleanup_recovery_path(&cleanup)
            .into_iter()
            .collect();
        return GenericCommandError::PublishDiscard {
            operation: Box::new(GenericCommandError::Cancelled),
            discard: cleanup.to_string(),
            recovery_paths,
        };
    }
    GenericCommandError::operation(stage, source)
}

fn scratch_cleanup_recovery_path(source: &GenericScratchError) -> Option<PathBuf> {
    match source {
        GenericScratchError::Io { path, .. } => Some(path.clone()),
        GenericScratchError::UnsafeCleanupTarget { scratch_root, .. } => Some(scratch_root.clone()),
        GenericScratchError::CleanupAfterFailure { cleanup, .. } => {
            scratch_cleanup_recovery_path(cleanup)
        }
        GenericScratchError::Cancelled
        | GenericScratchError::InvalidRelativePath(_)
        | GenericScratchError::InvalidMaterializedFile { .. }
        | GenericScratchError::TargetNotDirectory(_) => None,
    }
}

enum Driven<T> {
    Finished(T),
    Interrupted(T),
    CancellationWon(T),
    SignalFailed { source: io::Error, result: T },
}

async fn drive<T>(
    future: impl Future<Output = T>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
) -> Driven<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = termination_signals.recv() => match signal {
            Ok(()) => {
                cancel();
                Driven::Interrupted(future.await)
            }
            Err(source) => {
                cancel();
                Driven::SignalFailed {
                    source,
                    result: future.await,
                }
            }
        },
        result = &mut future => Driven::Finished(result),
    }
}

/// WriteBack 的取消只有在目录发布根接管候选前才能生效。
///
/// `cancel` 返回 `false` 表示发布边界已经先发生；此时继续等待业务 future，并按它的真实
/// 终态处理，而不是把已经开始的目录交换伪报为取消。
async fn drive_write_back<T>(
    future: impl Future<Output = T>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce() -> bool,
) -> Driven<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = termination_signals.recv() => match signal {
            Ok(()) => {
                let cancellation_started = cancel();
                let result = future.await;
                write_back_signal_result(cancellation_started, result)
            }
            Err(source) => {
                // 信号接收器本身失败时仍发起合作取消；最终分类由 SignalFailed 固定拥有。
                cancel();
                Driven::SignalFailed {
                    source,
                    result: future.await,
                }
            }
        },
        result = &mut future => Driven::Finished(result),
    }
}

fn write_back_signal_result<T>(cancellation_started: bool, result: T) -> Driven<T> {
    if cancellation_started {
        Driven::CancellationWon(result)
    } else {
        Driven::Finished(result)
    }
}

async fn drive_and_shutdown(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    file_system: SystemFileSystem,
    mut shutdown_errors: Vec<GenericShutdownError>,
    project_log: GenericProjectLogSlot,
    progress: GenericTerminalProgress,
) -> GenericCommandRunReport {
    let driven = drive(operation, termination_signals, || {
        progress.safe_stopping();
        cancel();
    })
    .await;
    progress.finalizing();
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::new("filesystem", source));
    }
    record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
    GenericCommandRunReport::from_driven(
        driven,
        shutdown_errors,
        take_generic_project_log(&project_log),
    )
}

impl GenericCommandRunReport {
    fn failed(error: GenericCommandError) -> Self {
        Self {
            result: GenericCommandRunResult::Failed(error),
            shutdown_errors: Vec::new(),
            pending_project_log: None,
        }
    }

    fn from_driven(
        driven: Driven<Result<GenericCommandOutput, GenericCommandError>>,
        shutdown_errors: Vec<GenericShutdownError>,
        project_log: Option<ActiveProjectLog>,
    ) -> Self {
        let result = match driven {
            // 信号到达后业务仍返回完整成功，说明事务或其他终态已经生效。
            Driven::Finished(Ok(output)) | Driven::Interrupted(Ok(output)) => {
                GenericCommandRunResult::Succeeded(output)
            }
            Driven::Finished(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            Driven::Interrupted(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            // WriteBack 的发布门确认取消先于发布取得终态；即使内部 future
            // 随后错误地返回成功，也不能把未发布候选报告为成功。
            Driven::CancellationWon(Ok(_)) => GenericCommandRunResult::Interrupted,
            Driven::CancellationWon(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            Driven::SignalFailed { source, result } => match result {
                Ok(_) => GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source,
                    operation: None,
                    state_applied: true,
                }),
                Err(error) => GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source,
                    operation: Some(Box::new(error)),
                    state_applied: false,
                }),
            },
        };
        let pending_project_log = project_log.map(|project_log| {
            let mut diagnostics = match &result {
                GenericCommandRunResult::Failed(error) => {
                    let mut diagnostics = vec![error.safe_diagnostic()];
                    diagnostics.extend(error.related_diagnostic());
                    diagnostics
                }
                GenericCommandRunResult::Succeeded(_) | GenericCommandRunResult::Interrupted => {
                    Vec::new()
                }
            };
            diagnostics.extend(
                shutdown_errors
                    .iter()
                    .map(GenericShutdownError::safe_diagnostic),
            );
            let outcome = if !shutdown_errors.is_empty() {
                ProjectLogRunOutcome::Failed
            } else {
                match &result {
                    GenericCommandRunResult::Succeeded(_) => ProjectLogRunOutcome::Succeeded,
                    GenericCommandRunResult::Interrupted => ProjectLogRunOutcome::Cancelled,
                    GenericCommandRunResult::Failed(error)
                        if matches!(
                            error.safe_diagnostic().impact,
                            DiagnosticImpact::RecoveryRequired
                        ) =>
                    {
                        ProjectLogRunOutcome::RecoveryRequired
                    }
                    GenericCommandRunResult::Failed(error)
                        if matches!(
                            error.safe_diagnostic().impact,
                            DiagnosticImpact::OutcomeUnknown
                        ) =>
                    {
                        ProjectLogRunOutcome::OutcomeUnknown
                    }
                    GenericCommandRunResult::Failed(_) => ProjectLogRunOutcome::Failed,
                }
            };
            PendingProjectLog::new(project_log, outcome, diagnostics)
        });
        Self {
            result,
            shutdown_errors,
            pending_project_log,
        }
    }
}

async fn publish_generic_write_back(
    publisher: SystemDirectoryPublisher,
    project_name: ProjectName,
    project: GenericProject,
    candidate: GenericWriteBackCandidate,
    cancellation: CooperativeCancellation,
    publication_gate: GenericWriteBackPublicationGate,
    publication_started: impl FnOnce() + Send,
) -> Result<GenericCommandOutput, GenericCommandError> {
    if cancellation.is_requested() {
        return Err(GenericCommandError::Cancelled);
    }
    let workspace_root = project.workspace_root().to_path_buf();
    let translated_units = candidate.translated_units();
    let retained_source_units = candidate.retained_source_units();
    let scratch_candidate = candidate;
    let scratch_workspace = workspace_root.clone();
    let materialize_cancellation = cancellation.clone();
    let scratch_root = run_scratch_blocking("建立 Generic 写回暂存来源", move || {
        materialize_write_back_source(
            &scratch_workspace,
            &scratch_candidate,
            &materialize_cancellation,
        )
    })
    .await?;
    if cancellation.is_requested() {
        return match cleanup_write_back_source(&workspace_root, &scratch_root) {
            Ok(()) => Err(GenericCommandError::Cancelled),
            Err(cleanup) => Err(GenericCommandError::PublishDiscard {
                operation: Box::new(GenericCommandError::Cancelled),
                discard: cleanup.to_string(),
                recovery_paths: vec![scratch_root],
            }),
        };
    }

    let target_root = project.write_back_root();
    let request = (|| {
        let publish_intent = publish_intent_for(&target_root).map_err(|source| {
            Box::new(GenericCommandError::operation(
                "检查 Generic 写回目录",
                source,
            ))
        })?;
        let mapping = DirectorySourceMapping::new(scratch_root.clone(), PathBuf::new()).map_err(
            |source| {
                Box::new(GenericCommandError::operation(
                    "建立 Generic 目录候选请求",
                    source,
                ))
            },
        )?;
        DirectoryStageRequest::new(
            target_root.clone(),
            publish_intent,
            vec![mapping],
            Vec::new(),
            Vec::new(),
        )
        .map_err(|source| {
            Box::new(GenericCommandError::operation(
                "建立 Generic 目录候选请求",
                source,
            ))
        })
    })()
    .map_err(|operation| {
        generic_scratch_handoff_failure(*operation, &workspace_root, &scratch_root)
    })?;

    let staged = match publisher.prepare(request).await {
        Ok(staged) => staged,
        Err(source) => {
            return Err(generic_prepare_failure(
                &cancellation,
                source,
                &workspace_root,
                &scratch_root,
            ));
        }
    };
    if cancellation.is_requested() {
        return discard_after_failure(&publisher, staged, GenericCommandError::Cancelled).await;
    }

    if let Err(source) = cleanup_write_back_source(&workspace_root, &scratch_root) {
        let diagnostic = generic_operation_diagnostic("清理 Generic 写回暂存来源")
            .with_recovery(RecoveryFact::path(&scratch_root));
        let operation =
            GenericCommandError::diagnosed("清理 Generic 写回暂存来源", source, diagnostic);
        return discard_after_failure(&publisher, staged, operation).await;
    }
    if cancellation.is_requested() {
        return discard_after_failure(&publisher, staged, GenericCommandError::Cancelled).await;
    }

    let recheck_cancellation = cancellation.clone();
    if let Err(operation) = run_project_blocking(
        "发布前复查 Generic 输入",
        DiagnosticStage::WriteBack,
        DiagnosticImpact::Unchanged,
        DiagnosticAction::FixInput,
        move || {
            ensure_input_fingerprints_current_with_cancellation(&project, &recheck_cancellation)
        },
    )
    .await
    {
        return discard_after_failure(&publisher, staged, operation).await;
    }

    if cancellation.is_requested() && publication_gate.request_cancellation() {
        return discard_after_failure(&publisher, staged, GenericCommandError::Cancelled).await;
    }
    if !begin_generic_write_back_publication(&publication_gate, publication_started) {
        return discard_after_failure(&publisher, staged, GenericCommandError::Cancelled).await;
    }
    publisher.publish(staged).await.map_err(|source| {
        let diagnostic = generic_publish_diagnostic(&source);
        GenericCommandError::diagnosed("发布 Generic 写回目录", source, diagnostic)
    })?;
    Ok(GenericCommandOutput::WriteBack {
        project: project_name,
        output_root: target_root,
        translated_units,
        retained_source_units,
    })
}

fn generic_prepare_error(
    cancellation: &CooperativeCancellation,
    source: DirectoryPrepareError<Box<SystemFileSystemError>>,
) -> GenericCommandError {
    if cancellation.is_requested() && directory_prepare_cancelled_without_cleanup(&source) {
        GenericCommandError::Cancelled
    } else {
        GenericCommandError::operation("准备 Generic 写回候选", source)
    }
}

fn generic_prepare_failure(
    cancellation: &CooperativeCancellation,
    source: DirectoryPrepareError<Box<SystemFileSystemError>>,
    workspace_root: &Path,
    scratch_root: &Path,
) -> GenericCommandError {
    let operation = generic_prepare_error(cancellation, source);
    generic_scratch_handoff_failure(operation, workspace_root, scratch_root)
}

fn generic_scratch_handoff_failure(
    operation: GenericCommandError,
    workspace_root: &Path,
    scratch_root: &Path,
) -> GenericCommandError {
    match cleanup_write_back_source(workspace_root, scratch_root) {
        Ok(()) => operation,
        Err(cleanup) => GenericCommandError::PublishDiscard {
            operation: Box::new(operation),
            discard: cleanup.to_string(),
            recovery_paths: vec![scratch_root.to_path_buf()],
        },
    }
}

fn directory_prepare_cancelled_without_cleanup(
    source: &DirectoryPrepareError<Box<SystemFileSystemError>>,
) -> bool {
    match source {
        DirectoryPrepareError::NotPrepared {
            source,
            cleanup_failure: None,
            ..
        } => matches!(
            source.as_ref(),
            SystemFileSystemError::Cancelled { .. }
                | SystemFileSystemError::Windows(WindowsFsError::LockCancelled { .. })
        ),
        DirectoryPrepareError::NotPrepared {
            cleanup_failure: Some(_),
            ..
        } => false,
    }
}

fn generic_publish_diagnostic(
    source: &DirectoryPublishError<Box<SystemFileSystemError>>,
) -> SafeDiagnostic {
    match source {
        DirectoryPublishError::TargetAlreadyExists {
            target_root,
            cleanup_failure,
        } => with_staging_cleanup(
            SafeDiagnostic::new(
                DiagnosticCode::WriteBackPublish,
                DiagnosticStage::Publication,
                DiagnosticSubject::path(target_root),
                DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            cleanup_failure.as_ref(),
        ),
        DirectoryPublishError::TargetMissing {
            target_root,
            cleanup_failure,
        } => with_staging_cleanup(
            SafeDiagnostic::new(
                DiagnosticCode::WriteBackPublish,
                DiagnosticStage::Publication,
                DiagnosticSubject::path(target_root),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            cleanup_failure.as_ref(),
        ),
        DirectoryPublishError::TargetNotDirectory {
            target_root,
            cleanup_failure,
        } => with_staging_cleanup(
            SafeDiagnostic::new(
                DiagnosticCode::WriteBackPublish,
                DiagnosticStage::Publication,
                DiagnosticSubject::path(target_root),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            cleanup_failure.as_ref(),
        ),
        DirectoryPublishError::NotAttempted {
            target_root,
            source,
            cleanup_failure,
        }
        | DirectoryPublishError::NotPublished {
            target_root,
            source,
            cleanup_failure,
        } => with_staging_cleanup(
            source
                .safe_diagnostic(
                    DiagnosticStage::Publication,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(RecoveryFact::path(target_root)),
            cleanup_failure.as_ref(),
        ),
        DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path,
            source,
        } => source
            .safe_diagnostic(
                DiagnosticStage::Publication,
                DiagnosticImpact::StateAppliedFinalizationFailed,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(RecoveryFact::path(target_root))
            .with_recovery(RecoveryFact::path(residual_path)),
        DirectoryPublishError::RecoveryRequired {
            target_root,
            recovery_artifacts,
            source,
        } => with_recovery_paths(
            source
                .safe_diagnostic(
                    DiagnosticStage::Publication,
                    DiagnosticImpact::RecoveryRequired,
                    DiagnosticAction::PreserveRecoveryArtifacts,
                )
                .with_recovery(RecoveryFact::path(target_root)),
            recovery_artifacts,
        ),
        DirectoryPublishError::OutcomeUnknown {
            target_root,
            recovery_artifacts,
            source,
        } => with_recovery_paths(
            source
                .safe_diagnostic(
                    DiagnosticStage::Publication,
                    DiagnosticImpact::OutcomeUnknown,
                    DiagnosticAction::PreserveRecoveryArtifacts,
                )
                .with_recovery(RecoveryFact::path(target_root)),
            recovery_artifacts,
        ),
    }
}

fn with_staging_cleanup(
    mut diagnostic: SafeDiagnostic,
    cleanup: Option<&StagingCleanupFailure<Box<SystemFileSystemError>>>,
) -> SafeDiagnostic {
    if let Some(cleanup) = cleanup {
        diagnostic = diagnostic
            .with_recovery(RecoveryFact::path(cleanup.residual_path()))
            .with_recovery(RecoveryFact::component("candidate_cleanup=failed"));
    }
    diagnostic
}

fn with_recovery_paths(mut diagnostic: SafeDiagnostic, paths: &[PathBuf]) -> SafeDiagnostic {
    for path in paths {
        diagnostic = diagnostic.with_recovery(RecoveryFact::path(path));
    }
    diagnostic
}

fn materialize_write_back_source(
    workspace_root: &Path,
    candidate: &GenericWriteBackCandidate,
    cancellation: &CooperativeCancellation,
) -> Result<PathBuf, GenericScratchError> {
    materialize_write_back_source_with(workspace_root, candidate, cancellation, |path, bytes| {
        write_file_with_cancellation(path, bytes, cancellation)
    })
}

fn materialize_write_back_source_with(
    workspace_root: &Path,
    candidate: &GenericWriteBackCandidate,
    cancellation: &CooperativeCancellation,
    mut write_file: impl FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<PathBuf, GenericScratchError> {
    if cancellation.is_requested() {
        return Err(GenericScratchError::Cancelled);
    }
    let scratch_root = workspace_root.join(format!(
        "{WRITE_BACK_SCRATCH_PREFIX}{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&scratch_root).map_err(|source| GenericScratchError::Io {
        operation: "建立暂存根",
        path: scratch_root.clone(),
        source,
    })?;

    for file in candidate.files() {
        ensure_materialization_not_cancelled(workspace_root, &scratch_root, cancellation)?;
        let relative = file.relative_path();
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::CurDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            let operation = GenericScratchError::InvalidRelativePath(relative.to_path_buf());
            return match cleanup_write_back_source(workspace_root, &scratch_root) {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        let target = scratch_root.join(relative);
        let parent = target.parent().expect("相对 JSONL 文件必须拥有暂存父目录");
        if let Err(source) = fs::create_dir_all(parent) {
            let operation = GenericScratchError::Io {
                operation: "建立暂存子目录",
                path: parent.to_path_buf(),
                source,
            };
            return match cleanup_write_back_source(workspace_root, &scratch_root) {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        ensure_materialization_not_cancelled(workspace_root, &scratch_root, cancellation)?;
        if let Err(source) = write_file(&target, file.bytes()) {
            let operation =
                if cancellation.is_requested() && source.kind() == io::ErrorKind::Interrupted {
                    GenericScratchError::Cancelled
                } else {
                    GenericScratchError::Io {
                        operation: "写入暂存 JSONL",
                        path: target,
                        source,
                    }
                };
            return match cleanup_write_back_source(workspace_root, &scratch_root) {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        ensure_materialization_not_cancelled(workspace_root, &scratch_root, cancellation)?;
        let materialized_bytes = match read_file_with_cancellation(&target, cancellation) {
            Ok(bytes) => bytes,
            Err(source) => {
                let operation =
                    if cancellation.is_requested() && source.kind() == io::ErrorKind::Interrupted {
                        GenericScratchError::Cancelled
                    } else {
                        GenericScratchError::Io {
                            operation: "读回暂存 JSONL",
                            path: target,
                            source,
                        }
                    };
                return match cleanup_write_back_source(workspace_root, &scratch_root) {
                    Ok(()) => Err(operation),
                    Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                        operation: Box::new(operation),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
        };
        ensure_materialization_not_cancelled(workspace_root, &scratch_root, cancellation)?;
        if let Err(source) = validate_materialized_write_back_file_with_cancellation(
            file,
            materialized_bytes,
            cancellation,
        ) {
            let operation = if source.is_cancelled() {
                GenericScratchError::Cancelled
            } else {
                GenericScratchError::InvalidMaterializedFile {
                    path: target,
                    source: Box::new(source),
                }
            };
            return match cleanup_write_back_source(workspace_root, &scratch_root) {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
    }
    ensure_materialization_not_cancelled(workspace_root, &scratch_root, cancellation)?;
    Ok(scratch_root)
}

fn write_file_with_cancellation(
    path: &Path,
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> io::Result<()> {
    const CHUNK_BYTES: usize = 64 * 1024;

    let mut file = fs::File::create(path)?;
    for chunk in bytes.chunks(CHUNK_BYTES) {
        if cancellation.is_requested() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        io::Write::write_all(&mut file, chunk)?;
    }
    if cancellation.is_requested() {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(())
    }
}

fn read_file_with_cancellation(
    path: &Path,
    cancellation: &CooperativeCancellation,
) -> io::Result<Vec<u8>> {
    const CHUNK_BYTES: usize = 64 * 1024;

    let mut file = fs::File::open(path)?;
    let capacity = file
        .metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or_default();
    let mut output = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; CHUNK_BYTES];
    loop {
        if cancellation.is_requested() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let read = loop {
            match io::Read::read(&mut file, &mut buffer) {
                Err(source)
                    if source.kind() == io::ErrorKind::Interrupted
                        && !cancellation.is_requested() =>
                {
                    continue;
                }
                result => break result?,
            }
        };
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    if cancellation.is_requested() {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(output)
    }
}

fn ensure_materialization_not_cancelled(
    workspace_root: &Path,
    scratch_root: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericScratchError> {
    if !cancellation.is_requested() {
        return Ok(());
    }
    let operation = GenericScratchError::Cancelled;
    match cleanup_write_back_source(workspace_root, scratch_root) {
        Ok(()) => Err(operation),
        Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
    }
}

fn cleanup_write_back_source(
    workspace_root: &Path,
    scratch_root: &Path,
) -> Result<(), GenericScratchError> {
    let valid_parent = scratch_root.parent() == Some(workspace_root);
    let valid_name = scratch_root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(WRITE_BACK_SCRATCH_PREFIX));
    if !valid_parent || !valid_name {
        return Err(GenericScratchError::UnsafeCleanupTarget {
            workspace_root: workspace_root.to_path_buf(),
            scratch_root: scratch_root.to_path_buf(),
        });
    }
    match fs::remove_dir_all(scratch_root) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GenericScratchError::Io {
            operation: "删除暂存来源",
            path: scratch_root.to_path_buf(),
            source,
        }),
    }
}

fn publish_intent_for(target_root: &Path) -> Result<DirectoryPublishIntent, GenericScratchError> {
    match fs::symlink_metadata(target_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            Ok(DirectoryPublishIntent::ReplaceExisting)
        }
        Ok(_) => Err(GenericScratchError::TargetNotDirectory(
            target_root.to_path_buf(),
        )),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(DirectoryPublishIntent::CreateNew)
        }
        Err(source) => Err(GenericScratchError::Io {
            operation: "读取目标 metadata",
            path: target_root.to_path_buf(),
            source,
        }),
    }
}

async fn discard_after_failure<P>(
    publisher: &P,
    staged: crate::storage::file_system::StagedDirectory<P::StagingState>,
    operation: GenericCommandError,
) -> Result<GenericCommandOutput, GenericCommandError>
where
    P: RecoverableDirectoryPublisher,
{
    match publisher.discard(staged).await {
        Ok(()) => Err(operation),
        Err(discard) => {
            let recovery_path = discard.staging_root().to_path_buf();
            Err(GenericCommandError::PublishDiscard {
                operation: Box::new(operation),
                discard: discard.to_string(),
                recovery_paths: vec![recovery_path],
            })
        }
    }
}

#[derive(Debug)]
enum GenericScratchError {
    Cancelled,
    InvalidRelativePath(PathBuf),
    InvalidMaterializedFile {
        path: PathBuf,
        source: Box<GenericWriteBackError>,
    },
    TargetNotDirectory(PathBuf),
    UnsafeCleanupTarget {
        workspace_root: PathBuf,
        scratch_root: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    CleanupAfterFailure {
        operation: Box<GenericScratchError>,
        cleanup: Box<GenericScratchError>,
    },
}

impl fmt::Display for GenericScratchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 写回暂存已取消"),
            Self::InvalidRelativePath(path) => {
                write!(
                    formatter,
                    "候选 JSONL 路径不是普通相对路径：{}",
                    path.display()
                )
            }
            Self::InvalidMaterializedFile { path, source } => {
                write!(
                    formatter,
                    "暂存 JSONL 未通过落盘复查：{}（{source}）",
                    path.display()
                )
            }
            Self::TargetNotDirectory(path) => {
                write!(formatter, "目标存在但不是普通目录：{}", path.display())
            }
            Self::UnsafeCleanupTarget {
                workspace_root,
                scratch_root,
            } => write!(
                formatter,
                "拒绝清理无法证明属于项目工作区的暂存目录：工作区 {}，目标 {}",
                workspace_root.display(),
                scratch_root.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} 失败：{source}", path.display()),
            Self::CleanupAfterFailure { operation, cleanup } => {
                write!(formatter, "{operation}；随后清理也失败：{cleanup}")
            }
        }
    }
}

impl Error for GenericScratchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidMaterializedFile { source, .. } => Some(source.as_ref()),
            Self::CleanupAfterFailure { operation, .. } => Some(operation.as_ref()),
            Self::Cancelled
            | Self::InvalidRelativePath(_)
            | Self::TargetNotDirectory(_)
            | Self::UnsafeCleanupTarget { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Barrier, Condvar, mpsc};

    use crate::generic::{
        GenericPlaceholderRuleDefinition, automatic_translation_state_fingerprint,
        manual_translation_state_fingerprint,
    };
    use crate::language::{
        JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageId,
        LanguageModule, QuotePair,
    };
    use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

    use super::*;

    fn fingerprint(byte: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([byte; 32])
    }

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    #[test]
    fn generic_prompt_fingerprint_includes_both_response_switches() {
        let modes = [
            TranslationResponseMode::new(false, false),
            TranslationResponseMode::new(true, false),
            TranslationResponseMode::new(false, true),
            TranslationResponseMode::new(true, true),
        ];
        let fingerprints = modes.map(|mode| {
            generic_prompt_fingerprint_with_cancellation("相同 Prompt 正文", mode, || {
                Ok::<_, ()>(())
            })
            .expect("未取消的 Prompt 指纹应建立")
        });
        for left in 0..fingerprints.len() {
            for right in left + 1..fingerprints.len() {
                assert_ne!(
                    fingerprints[left], fingerprints[right],
                    "不同响应开关组合必须产生不同自动译文语义指纹"
                );
            }
        }
    }

    #[tokio::test]
    async fn generic_panic_boundary_keeps_command_stage_and_workspace() {
        let workspace = PathBuf::from("projects/generic/panic-project");
        let report = catch_generic_command_panic(
            GenericCommandPanicContext {
                command: "translate",
                stage: DiagnosticStage::Translate,
                project_workspace: workspace.clone(),
            },
            async { panic!("不得读取的测试 panic payload") },
        )
        .await;

        let GenericCommandRunResult::Failed(GenericCommandError::Operation { diagnostic, .. }) =
            report.result
        else {
            panic!("Generic 命令 panic 必须成为命令级结构化失败");
        };
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert_eq!(diagnostic.subject, DiagnosticSubject::command("translate"));
        assert_eq!(diagnostic.impact, DiagnosticImpact::OutcomeUnknown);
        assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        assert_eq!(diagnostic.recovery, vec![RecoveryFact::path(workspace)]);
    }

    #[test]
    fn generic_partial_response_diagnostic_keeps_task_and_output_id() {
        let diagnostic =
            generic_response_problem_diagnostic(2, &ResponseProblem::MissingId(task_id(7)));

        assert_eq!(diagnostic.code, DiagnosticCode::ModelRequest);
        assert_eq!(diagnostic.stage, DiagnosticStage::ModelRequest);
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::operation("generic_translation_task_3")
        );
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidResponseContract,
                "problem=missing_id; output_id=7"
            )
        );
    }

    #[test]
    fn cancellation_after_request_keeps_task_record_draft() {
        let prepared = cancelled_generic_prepared_task(
            1,
            Some(GenericTaskRecordInFlight {
                total_tasks: 3,
                task_index: 1,
                messages: vec![ChatMessage::new(ChatMessageRole::User, "request")],
                expected_outputs: 2,
                started_at: OffsetDateTime::now_utc(),
                started: Instant::now(),
                attempt_count: 1,
                attempts: Vec::new(),
            }),
            1,
        );

        assert_eq!(prepared.task_index, 1);
        assert!(matches!(
            prepared.outcome,
            GenericPreparedTaskOutcome::Cancelled
        ));
        let record = prepared.record.expect("请求开始后的取消必须保留可提交记录");
        assert_eq!(record.total_tasks, 3);
        assert_eq!(record.task_index, 1);
        assert_eq!(record.attempt_count, 1);
        assert!(record.response.is_none());
    }

    struct BlockingLanguageModule {
        inner: JapaneseLanguageModule,
        started: Mutex<Option<mpsc::SyncSender<()>>>,
        release: Arc<(Mutex<bool>, Condvar)>,
        analysis_count: Arc<AtomicUsize>,
    }

    impl LanguageModule for BlockingLanguageModule {
        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            self.inner.semantic_fingerprint()
        }

        fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
            self.analysis_count.fetch_add(1, Ordering::AcqRel);
            if let Some(started) = self.started.lock().expect("开始信号锁不应中毒").take()
            {
                started.send(()).expect("测试线程必须等待语言分析开始");
            }
            let (released, release_signal) = self.release.as_ref();
            let released = released.lock().expect("释放信号锁不应中毒");
            drop(
                release_signal
                    .wait_while(released, |released| !*released)
                    .expect("释放信号锁不应中毒"),
            );
            self.inner.analyze_source(text)
        }

        fn find_source_residual(
            &self,
            analysis: &LanguageAnalysis,
            translation: &LanguageText,
        ) -> Result<Option<crate::language::LanguageResidual>, crate::language::LanguageModuleError>
        {
            self.inner.find_source_residual(analysis, translation)
        }

        fn plan_translation_repair(
            &self,
            analysis: &LanguageAnalysis,
            translation: &LanguageText,
        ) -> Result<crate::language::LanguageRepairPlan, crate::language::LanguageModuleError>
        {
            self.inner.plan_translation_repair(analysis, translation)
        }
    }

    #[test]
    fn received_signal_after_lua_commit_preserves_successful_terminal_state() {
        let mut connection = Connection::open_in_memory().expect("应该建立测试数据库");
        connection
            .execute_batch("CREATE TABLE committed(value INTEGER NOT NULL);")
            .expect("应该建立测试表");
        let transaction = connection.transaction().expect("应该开始事务");
        transaction
            .execute("INSERT INTO committed(value) VALUES (1)", [])
            .expect("脚本事务应该写入");
        transaction.commit().expect("脚本事务应该提交");

        let report = GenericCommandRunReport::from_driven(
            Driven::Interrupted(Ok(GenericCommandOutput::Lua {
                project: "signal-race".parse().expect("项目名应该有效"),
            })),
            Vec::new(),
            None,
        );

        assert!(matches!(
            report.result,
            GenericCommandRunResult::Succeeded(GenericCommandOutput::Lua { .. })
        ));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM committed", [], |row| row
                    .get::<_, i64>(0))
                .expect("应该读取已提交事务"),
            1,
            "信号晚到不能把已经提交的 Lua 事务伪报为取消"
        );
    }

    #[test]
    fn signal_receiver_failure_after_applied_state_reports_applied_impact() {
        let report = GenericCommandRunReport::from_driven(
            Driven::SignalFailed {
                source: io::Error::other("signal receiver failed"),
                result: Ok(GenericCommandOutput::Lua {
                    project: "signal-failure".parse().expect("项目名应该有效"),
                }),
            },
            Vec::new(),
            None,
        );

        let GenericCommandRunResult::Failed(error) = report.result else {
            panic!("信号接收失败仍应报告运行失败");
        };
        assert_eq!(
            error.safe_diagnostic().impact,
            DiagnosticImpact::StateAppliedFinalizationFailed
        );
    }

    #[test]
    fn requested_cancellation_stops_operation_before_first_side_effect() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let side_effect_observed = AtomicBool::new(false);

        let result = (|| {
            ensure_generic_operation_running(&cancellation).map_err(Box::new)?;
            side_effect_observed.store(true, Ordering::Release);
            Ok::<_, Box<GenericOperationCancelled>>(())
        })();

        let error =
            GenericCommandError::from(*result.expect_err("操作首次执行时必须观察已有取消请求"));
        assert!(!side_effect_observed.load(Ordering::Acquire));
        let report =
            GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));
    }

    #[test]
    fn running_cpu_preparation_cancellation_becomes_interrupted() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir(&source_root).expect("应该可建立输入目录");
        let group_count = rayon::current_num_threads().saturating_mul(4).max(8);
        let mut input = String::new();
        for index in 0..group_count {
            input.push_str(&format!(
                "{{\"id\":\"group-{index}\",\"kind\":\"dialogue\",\"units\":[\
                 {{\"id\":\"unit-{index}\",\"text\":\"こんにちは\"}}]}}\n"
            ));
        }
        fs::write(source_root.join("scene.jsonl"), input).expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "cpu-cancel".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则应该合法");
        let terminology = Arc::new(CompiledTerminology::empty());
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let analysis_count = Arc::new(AtomicUsize::new(0));
        let language_module: Arc<dyn LanguageModule> = Arc::new(BlockingLanguageModule {
            inner: JapaneseLanguageModule::new(
                JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                    .expect("日文残留策略应该合法"),
                None,
            ),
            started: Mutex::new(Some(started_sender)),
            release: Arc::clone(&release),
            analysis_count: Arc::clone(&analysis_count),
        });
        let cancellation = CooperativeCancellation::default();
        let operation_cancellation = cancellation.clone();
        let operation = std::thread::spawn(move || {
            prepare_generic_translation(
                &snapshot,
                terminology,
                &rules,
                language_module,
                AutomaticStateResources {
                    prompt: fingerprint(1),
                    client_semantics: fingerprint(2),
                    language_module: fingerprint(3),
                    terminology_hits: empty_terminology_fingerprint(),
                },
                NonZeroUsize::new(10_000).expect("常量应该非零"),
                &operation_cancellation,
            )
        });

        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("CPU 工作必须先进入语言分析");
        cancellation.request();
        let (released, release_signal) = release.as_ref();
        *released.lock().expect("释放信号锁不应中毒") = true;
        release_signal.notify_all();
        let source = operation
            .join()
            .expect("CPU 测试线程不应 panic")
            .err()
            .expect("运行中的 CPU 工作必须观察取消");
        assert!(source.is_cancelled());

        let error = generic_preparation_failure("建立 Generic 翻译计划", source);
        let report =
            GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));
        assert!(
            analysis_count.load(Ordering::Acquire) < group_count,
            "取消后不得继续遍历剩余 Group"
        );
    }

    #[test]
    fn cancelled_cpu_schedule_becomes_interrupted() {
        let source: CpuTaskExecutionError<CpuExecutorUnavailable> =
            CpuTaskExecutionError::Cancelled;
        let error = generic_cpu_execution_failure("调度 Generic 翻译计划", source);
        let report =
            GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);

        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));
    }

    #[test]
    fn cancelled_lease_file_and_resource_boundaries_become_interrupted() {
        fn assert_interrupted(error: GenericCommandError) {
            let report = GenericCommandRunReport::from_driven(
                Driven::Finished(Err(error)),
                Vec::new(),
                None,
            );
            assert!(matches!(
                report.result,
                GenericCommandRunResult::Interrupted
            ));
        }

        let cancelled_fs = || SystemFileSystemError::Cancelled {
            operation: "test_wait",
            path: PathBuf::from("cancelled"),
        };
        assert_interrupted(generic_project_lease_failure(
            ProjectCommandLeaseError::Unavailable {
                project: "lease-cancel".parse().expect("项目名应该合法"),
                source: Box::new(cancelled_fs()),
            },
        ));
        assert_interrupted(generic_read_file_failure(
            "读取 Lua 脚本",
            ReadFileError::Io {
                path: PathBuf::from("script.lua"),
                source: cancelled_fs(),
            },
            DiagnosticStage::CommandPreparation,
            DiagnosticAction::FixInput,
        ));
        assert_interrupted(generic_prompt_resource_failure(
            "读取翻译 system Prompt",
            PromptResourceLoadError::Read(ReadFileError::Io {
                path: PathBuf::from("system.md"),
                source: SystemFileSystemError::Windows(WindowsFsError::LockCancelled {
                    path: PathBuf::from("system.md"),
                }),
            }),
        ));
        assert_interrupted(generic_translation_resource_failure(
            TranslationPlanningResourceReadingError::ReadTerminology {
                path: PathBuf::from("terms.json"),
                source: ReadFileError::Io {
                    path: PathBuf::from("terms.json"),
                    source: cancelled_fs(),
                },
            },
        ));
        assert_interrupted(generic_translation_resource_failure(
            TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
                path: None,
                source: CpuTaskExecutionError::Cancelled,
            },
        ));
    }

    #[test]
    fn translation_resource_worker_start_is_an_internal_failure() {
        type ResourceError =
            TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>;

        let failures = [
            ResourceError::InvalidTerminology {
                path: Some(PathBuf::from("terms.toml")),
                source: TerminologyDefinitionError::StartWorker {
                    operation: "att-term-matcher",
                    source: io::Error::from_raw_os_error(8),
                },
            },
            ResourceError::InvalidPlaceholderRules {
                path: Some(PathBuf::from("placeholders.toml")),
                source: PlaceholderDefinitionError::StartWorker {
                    operation: "att-placeholder-toml",
                    source: io::Error::from_raw_os_error(8),
                },
            },
        ];

        for source in failures {
            let GenericCommandError::Operation { diagnostic, .. } =
                generic_translation_resource_failure(source)
            else {
                panic!("worker 启动失败必须保留为普通失败");
            };
            assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
            assert!(matches!(
                diagnostic.reason,
                DiagnosticReason::Io {
                    raw_os_code: Some(8),
                    ..
                }
            ));
            assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        }
    }

    #[test]
    fn lease_cleanup_failure_is_not_hidden_as_cancellation() {
        let error = generic_project_lease_failure(ProjectCommandLeaseError::Unavailable {
            project: "lease-cleanup".parse().expect("项目名应该合法"),
            source: Box::new(SystemFileSystemError::DirectChildRollbackFailed {
                path: PathBuf::from("lease"),
                operation: Box::new(SystemFileSystemError::Cancelled {
                    operation: "lease",
                    path: PathBuf::from("lease"),
                }),
                rollback: Box::new(SystemFileSystemError::Io {
                    operation: "rollback",
                    path: PathBuf::from("lease"),
                    source: io::Error::from_raw_os_error(5),
                }),
            }),
        });

        assert!(matches!(error, GenericCommandError::Operation { .. }));
    }

    #[tokio::test]
    async fn real_project_lease_wait_cancellation_becomes_interrupted() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let projects_root = temporary.path().join("projects");
        let owner_file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立租约所有者文件能力");
        let contender_file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立租约竞争者文件能力");
        let owner = ProjectCommandLeaseService::new(
            projects_root.clone(),
            GENERIC_ENGINE_NAME,
            owner_file_system.clone(),
        );
        let contender = ProjectCommandLeaseService::new(
            projects_root,
            GENERIC_ENGINE_NAME,
            contender_file_system.clone(),
        );
        let project: ProjectName = "lease-race".parse().expect("项目名应该合法");
        let held = owner
            .acquire(&project)
            .await
            .expect("所有者应该取得项目租约");
        let waiting_project = project.clone();
        let waiting = tokio::spawn(async move { contender.acquire(&waiting_project).await });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished(), "竞争者应该等待真实项目租约");

        contender_file_system.cancel_waits();
        let source = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("取消应该及时唤醒租约等待")
            .expect("租约竞争任务不应 panic")
            .err()
            .expect("取消后不得取得项目租约");
        let error = generic_project_lease_failure(source);
        let report =
            GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));

        drop(held);
        contender_file_system
            .shutdown()
            .await
            .expect("竞争者文件能力应该可终结");
        owner_file_system
            .shutdown()
            .await
            .expect("所有者文件能力应该可终结");
    }

    #[test]
    fn write_back_publication_gate_allows_exactly_one_terminal_decision() {
        let cancelled_first = GenericWriteBackPublicationGate::default();
        assert!(cancelled_first.request_cancellation());
        assert!(!cancelled_first.begin_publication());

        let published_first = GenericWriteBackPublicationGate::default();
        assert!(published_first.begin_publication());
        assert!(!published_first.request_cancellation());

        for _ in 0..32 {
            let gate = GenericWriteBackPublicationGate::default();
            let barrier = Arc::new(Barrier::new(3));
            let cancellation_gate = gate.clone();
            let cancellation_barrier = Arc::clone(&barrier);
            let cancellation = std::thread::spawn(move || {
                cancellation_barrier.wait();
                cancellation_gate.request_cancellation()
            });
            let publication_gate = gate;
            let publication_barrier = Arc::clone(&barrier);
            let publication = std::thread::spawn(move || {
                publication_barrier.wait();
                publication_gate.begin_publication()
            });
            barrier.wait();

            let cancellation_won = cancellation.join().expect("取消竞争线程不应 panic");
            let publication_won = publication.join().expect("发布竞争线程不应 panic");
            assert_ne!(
                cancellation_won, publication_won,
                "取消与发布必须恰好一个取得终态决定"
            );
        }
    }

    #[test]
    fn cancelled_publication_gate_does_not_report_or_enter_publishing() {
        let gate = GenericWriteBackPublicationGate::default();
        assert!(gate.request_cancellation());
        let observed = Arc::new(AtomicBool::new(false));
        let callback_observed = Arc::clone(&observed);

        let began_publication = begin_generic_write_back_publication(&gate, move || {
            callback_observed.store(true, Ordering::Release);
        });

        assert!(!began_publication);
        assert!(!observed.load(Ordering::Acquire));
    }

    #[test]
    fn write_back_signal_after_publication_keeps_the_real_terminal_result() {
        let output = GenericCommandOutput::Lua {
            project: "published-before-signal".parse().expect("项目名应该有效"),
        };
        let report = GenericCommandRunReport::from_driven(
            write_back_signal_result(false, Ok(output)),
            Vec::new(),
            None,
        );
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Succeeded(GenericCommandOutput::Lua { .. })
        ));

        let cancelled = GenericCommandRunReport::from_driven(
            write_back_signal_result(
                true,
                Ok(GenericCommandOutput::Lua {
                    project: "cancelled-before-publish".parse().expect("项目名应该有效"),
                }),
            ),
            Vec::new(),
            None,
        );
        assert!(matches!(
            cancelled.result,
            GenericCommandRunResult::Interrupted
        ));
    }

    #[test]
    fn cancelled_directory_prepare_maps_to_interrupted_without_cleanup_failure() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let error = generic_prepare_error(
            &cancellation,
            DirectoryPrepareError::NotPrepared {
                target_root: PathBuf::from("write_back"),
                source: Box::new(SystemFileSystemError::Cancelled {
                    operation: "prepare_directory_candidate",
                    path: PathBuf::from("write_back"),
                }),
                cleanup_failure: None,
            },
        );
        assert!(error.is_cancelled());

        let report =
            GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));
    }

    #[tokio::test]
    async fn real_publish_lock_cancellation_maps_to_interrupted_and_removes_scratch() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir(&source_root).expect("应该可建立候选来源");
        fs::write(source_root.join("source.jsonl"), b"source").expect("应该可建立候选文件");
        let target_root = temporary.path().join("write_back");
        let lock_directory = temporary.path().join("publish-locks");
        let publisher_config =
            crate::runtime::filesystem::DirectoryPublisherConfig::production(lock_directory)
                .expect("发布锁配置应该合法");
        let owner_file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立锁所有者文件能力");
        let contender_file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立锁竞争者文件能力");
        let owner = owner_file_system.directory_publisher(publisher_config.clone());
        let contender = contender_file_system.directory_publisher(publisher_config);
        let stage_request = || {
            let mapping = DirectorySourceMapping::new(source_root.clone(), PathBuf::new())
                .expect("候选来源映射应该合法");
            DirectoryStageRequest::new(
                target_root.clone(),
                DirectoryPublishIntent::CreateNew,
                vec![mapping],
                Vec::new(),
                Vec::new(),
            )
            .expect("候选请求应该合法")
        };
        let staged = owner
            .prepare(stage_request())
            .await
            .expect("所有者应该持有目标发布锁");

        let waiting = tokio::spawn({
            let contender = contender.clone();
            let request = stage_request();
            async move { contender.prepare(request).await }
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished(), "竞争者应该持续等待真实目标锁");

        let workspace_root = temporary.path().join("project");
        fs::create_dir(&workspace_root).expect("应该可建立项目工作区");
        let scratch_root = workspace_root.join(".generic-write-back-cancelled");
        fs::create_dir(&scratch_root).expect("应该可建立待清理 scratch");
        fs::write(scratch_root.join("residual"), b"candidate").expect("应该可建立 scratch 内容");
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        contender_file_system.cancel_waits();
        let prepare_result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("取消应该及时唤醒发布锁等待")
            .expect("竞争任务不应 panic");
        let prepare_error = match prepare_result {
            Ok(_) => panic!("取消后不得伪造候选已准备"),
            Err(error) => error,
        };
        assert!(directory_prepare_cancelled_without_cleanup(&prepare_error));

        let error =
            generic_prepare_failure(&cancellation, prepare_error, &workspace_root, &scratch_root);
        assert!(error.is_cancelled());
        assert!(!scratch_root.exists(), "受控取消后必须删除 scratch");
        let report =
            GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));

        owner.discard(staged).await.expect("应该可丢弃所有者候选");
        contender_file_system
            .shutdown()
            .await
            .expect("竞争者文件能力应该可终结");
        owner_file_system
            .shutdown()
            .await
            .expect("所有者文件能力应该可终结");
    }

    #[test]
    fn cancelled_directory_prepare_with_cleanup_failure_remains_a_failure() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let error = generic_prepare_error(
            &cancellation,
            DirectoryPrepareError::NotPrepared {
                target_root: PathBuf::from("write_back"),
                source: Box::new(SystemFileSystemError::Cancelled {
                    operation: "prepare_directory_candidate",
                    path: PathBuf::from("write_back"),
                }),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("candidate"),
                    Box::new(SystemFileSystemError::Cancelled {
                        operation: "cleanup_directory_candidate",
                        path: PathBuf::from("candidate"),
                    }),
                )),
            },
        );
        assert!(!error.is_cancelled());
        assert!(matches!(error, GenericCommandError::Operation { .. }));
    }

    #[test]
    fn cancelled_scratch_cleanup_failure_keeps_primary_and_recovery_path() {
        let scratch_root = PathBuf::from("project/.generic-write-back-test");
        let error = generic_scratch_command_error(
            "建立 Generic 写回暂存来源",
            GenericScratchError::CleanupAfterFailure {
                operation: Box::new(GenericScratchError::Cancelled),
                cleanup: Box::new(GenericScratchError::Io {
                    operation: "删除暂存来源",
                    path: scratch_root.clone(),
                    source: io::Error::from_raw_os_error(5),
                }),
            },
        );

        match error {
            GenericCommandError::PublishDiscard {
                operation,
                recovery_paths,
                ..
            } => {
                assert!(operation.is_cancelled());
                assert_eq!(recovery_paths, vec![scratch_root]);
            }
            other => panic!("取消后的清理失败必须保留双错误，实际为 {other}"),
        }
    }

    #[test]
    fn unchanged_translation_resources_without_invalidations_skip_persistence() {
        assert!(!should_apply_translation_resources(
            r#"[{"term":"魔王"}]"#,
            r#"[{"pattern":"\\\\N\\[\\d+\\]"}]"#,
            r#"[{"term":"魔王"}]"#,
            r#"[{"pattern":"\\\\N\\[\\d+\\]"}]"#,
            0,
        ));

        assert!(should_apply_translation_resources(
            r#"[{"term":"魔王"}]"#,
            "[]",
            r#"[{"term":"勇者"}]"#,
            "[]",
            0,
        ));
        assert!(should_apply_translation_resources(
            "[]",
            r#"[{"pattern":"old"}]"#,
            "[]",
            r#"[{"pattern":"new"}]"#,
            0,
        ));
        assert!(should_apply_translation_resources(
            "[]", "[]", "[]", "[]", 1,
        ));
    }

    #[test]
    fn profile_is_remembered_separately_only_without_committed_translation() {
        let only_conflicts = GenericTranslationSummary {
            conflicted_units: 3,
            ..GenericTranslationSummary::default()
        };
        assert!(
            should_remember_profile_separately(&only_conflicts),
            "没有译文成功写入时，即使存在冲突也仍要保存本轮 Profile"
        );

        let committed_with_conflicts = GenericTranslationSummary {
            written_units: 1,
            conflicted_units: 3,
            ..GenericTranslationSummary::default()
        };
        assert!(
            !should_remember_profile_separately(&committed_with_conflicts),
            "成功的译文提交已在同一事务保存 Profile，不应再启动独立写事务"
        );
    }

    #[test]
    fn generic_lua_runtime_diagnostic_keeps_transaction_state_and_concrete_cause() {
        let source = GenericLuaExecutionError::Run(ProjectLuaRunError::RolledBack(
            ProjectLuaFailure::Script("runtime-detail-sentinel".to_owned()),
        ));
        let expected_detail = source.to_string();

        let diagnostic = generic_lua_execution_diagnostic(&source);

        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::FailureWithDetail {
                failure: DiagnosticFailureKind::LuaExecutionFailed,
                detail: expected_detail,
            }
        );
        assert_eq!(
            diagnostic.recovery,
            vec![RecoveryFact::transaction("rolled_back")]
        );
    }

    #[test]
    fn project_lua_worker_start_is_internal_during_preflight_and_execution() {
        let source = ProjectLuaFailure::Host {
            domain: "isolated_worker",
            kind: "worker_spawn",
            operation: "compile_project_lua",
            message: "worker-start-detail-sentinel".to_owned(),
        };
        let detail = source.to_string();

        for stage in [DiagnosticStage::CommandPreparation, DiagnosticStage::Lua] {
            let diagnostic = project_lua_failure_diagnostic(&source, stage, &detail);

            assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
            assert_eq!(diagnostic.stage, stage);
            assert_eq!(
                diagnostic.subject,
                DiagnosticSubject::operation("compile_project_lua")
            );
            assert_eq!(
                diagnostic.reason,
                DiagnosticReason::FailureWithDetail {
                    failure: DiagnosticFailureKind::WorkerSpawnFailed,
                    detail: detail.clone(),
                }
            );
            assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        }
    }

    #[test]
    fn other_project_lua_host_failures_remain_user_input_failures() {
        let source = ProjectLuaFailure::Host {
            domain: "translation",
            kind: "invalid_translation",
            operation: "translation.set",
            message: "host-detail-sentinel".to_owned(),
        };
        let detail = source.to_string();

        let diagnostic = project_lua_failure_diagnostic(&source, DiagnosticStage::Lua, &detail);

        assert_eq!(diagnostic.code, DiagnosticCode::LuaExecution);
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::FailureWithDetail {
                failure: DiagnosticFailureKind::LuaHostCallFailed,
                detail,
            }
        );
        assert_eq!(diagnostic.action, DiagnosticAction::FixInput);
    }

    #[test]
    fn model_message_keeps_terms_and_uses_safe_current_or_source_context() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(source_root.join("nested")).expect("应该可建立输入目录");
        fs::write(
            source_root.join("nested/scene.jsonl"),
            concat!(
                r#"{"id":"secret-context-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-context","text":"魔王 {hero}"}]}"#,
                "\n",
                r#"{"id":"secret-invalid-current-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-invalid-current","text":"あ {rival}"}]}"#,
                "\n",
                r#"{"id":"secret-reuse-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-reuse","text":"あ {rival}"}]}"#,
                "\n",
                r#"{"id":"secret-output-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-output","text":"こんにちは"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "message-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let terminology = Arc::new(
            compile_terminology(vec![TerminologyEntry::new(
                "魔王",
                "魔王（Demon King）",
                vec!["魔王".to_owned()],
            )])
            .expect("术语应该可编译"),
        );
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let resources = AutomaticStateResources {
            prompt: fingerprint(2),
            client_semantics: fingerprint(3),
            language_module: fingerprint(4),
            terminology_hits: empty_terminology_fingerprint(),
        };
        let current_group = snapshot.files()[0].groups()[0].clone();
        let current_unit = current_group.units()[0].clone();
        let current_protected = GenericPlaceholderService::default()
            .protect(
                current_group.kind(),
                current_unit.source_text(),
                &placeholder_rules,
            )
            .expect("原文应该可保护");
        let current_state = automatic_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(current_group.id().to_owned(), current_unit.id().to_owned()),
            current_unit.source_text(),
            current_group.context_fingerprint(),
            current_protected.binding_fingerprint(),
            AutomaticStateResources {
                terminology_hits: terminology_hit_fingerprint(terminology.as_ref(), &[0]),
                ..resources
            },
        );
        let invalid_current_group = snapshot.files()[0].groups()[1].clone();
        let invalid_current_unit = invalid_current_group.units()[0].clone();
        let invalid_current_protected = GenericPlaceholderService::default()
            .protect(
                invalid_current_group.kind(),
                invalid_current_unit.source_text(),
                &placeholder_rules,
            )
            .expect("原文应该可保护");
        let invalid_current_state = automatic_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(
                invalid_current_group.id().to_owned(),
                invalid_current_unit.id().to_owned(),
            ),
            invalid_current_unit.source_text(),
            invalid_current_group.context_fingerprint(),
            invalid_current_protected.binding_fingerprint(),
            resources,
        );
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[
                    crate::generic::TranslationWrite {
                        group_id: current_group.id().to_owned(),
                        unit_id: current_unit.id().to_owned(),
                        expected_source_text: current_unit.source_text().to_owned(),
                        expected_group_context: current_group.context_fingerprint(),
                        translation: "已有上下文 {hero}".to_owned(),
                        origin: crate::generic::TranslationOrigin::Automatic,
                        state_fingerprint: current_state,
                        expected_translation: None,
                    },
                    crate::generic::TranslationWrite {
                        group_id: invalid_current_group.id().to_owned(),
                        unit_id: invalid_current_unit.id().to_owned(),
                        expected_source_text: invalid_current_unit.source_text().to_owned(),
                        expected_group_context: invalid_current_group.context_fingerprint(),
                        translation: "损坏的已有译文".to_owned(),
                        origin: crate::generic::TranslationOrigin::Automatic,
                        state_fingerprint: invalid_current_state,
                        expected_translation: None,
                    },
                ],
            )
            .expect("应该可保存测试译文");
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(
                NonZeroUsize::new(2).expect("测试阈值应该非零"),
                Vec::new(),
            )
            .expect("日文残留策略应该合法"),
            None,
        ));
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            language_module,
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");
        let message = render_generic_user_message(&prepared.plan.tasks()[0], terminology.as_ref());

        assert!(message.contains("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\""));
        assert_eq!(
            message
                .matches("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\"")
                .count(),
            1,
            "TaskBlock 命中的术语必须按文件顺序合并后只提供一次"
        );
        assert!(message.contains("\"kind\":\"dialogue\""));
        assert!(message.contains("\"text\":[\"已有上下文 "));
        assert!(message.contains("\"text\":[\"あ "));
        assert!(message.contains("\"id\":\"0\""));
        assert!(message.contains("\"id\":\"1\""));
        assert!(
            prepared.plan.reused().is_empty(),
            "只供阅读的源文回退不能成为重复 Unit 的复用译文"
        );
        assert!(!message.contains("损坏的已有译文"));
        assert!(!message.contains("{hero}"));
        assert!(!message.contains("{rival}"));
        for hidden_identity in [
            "secret-context-group",
            "secret-invalid-current-group",
            "secret-invalid-current",
            "secret-reuse-group",
            "secret-reuse",
            "secret-output-group",
            "secret-output",
            "secret-context",
            "nested/scene.jsonl",
        ] {
            assert!(
                !message.contains(hidden_identity),
                "模型输入不应泄漏稳定项目身份：{hidden_identity}"
            );
        }
    }

    #[test]
    fn repaired_reuse_is_reprotected_before_it_becomes_model_context() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"current","kind":"dialogue","units":[{"id":"unit","text":"「こんにちは {name}」"}]}"#,
                "\n",
                r#"{"id":"reuse","kind":"dialogue","units":[{"id":"unit","text":"「こんにちは {name}」"}]}"#,
                "\n",
                r#"{"id":"model","kind":"dialogue","units":[{"id":"unit","text":"さようなら"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "reuse-context-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let current_group = &snapshot.files()[0].groups()[0];
        let current_unit = &current_group.units()[0];
        let current_protected = GenericPlaceholderService::default()
            .protect(
                current_group.kind(),
                current_unit.source_text(),
                &placeholder_rules,
            )
            .expect("Current 原文应该可保护");
        let current_state = manual_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(current_group.id().to_owned(), current_unit.id().to_owned()),
            current_unit.source_text(),
            current_group.context_fingerprint(),
            current_protected.binding_fingerprint(),
        );
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[crate::generic::TranslationWrite {
                    group_id: current_group.id().to_owned(),
                    unit_id: current_unit.id().to_owned(),
                    expected_source_text: current_unit.source_text().to_owned(),
                    expected_group_context: current_group.context_fingerprint(),
                    translation: "“你好 {name}”".to_owned(),
                    origin: crate::generic::TranslationOrigin::Manual,
                    state_fingerprint: current_state,
                    expected_translation: None,
                }],
            )
            .expect("应该可保存 Current 译文");
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
            Some(
                JapaneseQuoteRepairPolicy::new(vec![QuotePair::new('“', '”')])
                    .expect("日文引号修复策略应该合法"),
            ),
        ));
        let terminology = Arc::new(CompiledTerminology::empty());
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            language_module,
            AutomaticStateResources {
                prompt: fingerprint(21),
                client_semantics: fingerprint(22),
                language_module: fingerprint(23),
                terminology_hits: empty_terminology_fingerprint(),
            },
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");

        assert_eq!(prepared.plan.reused().len(), 1);
        assert_eq!(prepared.plan.reused()[0].key().group_id(), "reuse");
        assert_eq!(
            prepared.plan.reused()[0].translation(),
            "「你好 {name}」",
            "提交值必须使用语言模块修复后的译文"
        );
        let task = &prepared.plan.tasks()[0];
        assert_eq!(task.groups().len(), 3);
        let current_context = task.groups()[0].units()[0].text();
        let reuse_context = task.groups()[1].units()[0].text();
        assert!(current_context.starts_with("“你好 ") && current_context.ends_with('”'));
        assert!(reuse_context.starts_with("「你好 ") && reuse_context.ends_with('」'));
        assert!(placeholder_token::contains_reserved_prefix(reuse_context));
        assert_eq!(task.groups()[1].units()[0].output_id(), None);
        assert_eq!(task.groups()[2].units()[0].output_id(), Some(task_id(0)));
        let message = render_generic_user_message(task, terminology.as_ref());
        assert!(message.contains("\"text\":[\"「你好 "));
        assert!(message.contains("\"id\":\"0\""));
        assert!(!message.contains("{name}"));
    }

    #[test]
    fn stable_source_packing_keeps_oversized_groups_and_renderer_keeps_full_content() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        let mut lines = Vec::new();
        for index in 0..18 {
            let units = if index == 1 {
                (0..12)
                    .map(|unit| {
                        serde_json::json!({
                            "id": format!("unit-{unit}"),
                            "text": format!("こんにちは \"{unit}\"\n魔王"),
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                let text = if index == 0 {
                    format!("{} 魔王", "こんにちは".repeat(80))
                } else {
                    format!("こんにちは \"{index}\"\n魔王")
                };
                vec![serde_json::json!({"id": "unit", "text": text})]
            };
            lines.push(
                serde_json::json!({
                    "id": format!("group-{index}"),
                    "kind": "dialogue\"kind",
                    "units": units,
                })
                .to_string(),
            );
        }
        fs::write(
            source_root.join("scene.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "size-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let terminology = Arc::new(
            compile_terminology(vec![TerminologyEntry::new(
                "魔王",
                "魔\"王 King",
                vec!["魔王".to_owned()],
            )])
            .expect("术语应该可编译"),
        );
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则应该合法");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
            None,
        ));
        let target = NonZeroUsize::new(260).expect("常量应该非零");
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            language_module,
            AutomaticStateResources {
                prompt: fingerprint(31),
                client_semantics: fingerprint(32),
                language_module: fingerprint(33),
                terminology_hits: empty_terminology_fingerprint(),
            },
            target,
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");

        assert!(prepared.plan.tasks().len() > 2);
        assert!(
            prepared
                .plan
                .tasks()
                .iter()
                .any(|task| task.groups().len() > 1),
            "除超大 Group 外，目标大小应允许多个 Group 同处一个 Task"
        );
        let rendered = prepared
            .plan
            .tasks()
            .iter()
            .map(|task| render_generic_user_message(task, terminology.as_ref()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("\\\""));
        assert!(rendered.contains("\",\"魔王\"]"));
        assert!(rendered.contains("\"id\":\"10\""));
    }

    #[test]
    fn write_back_current_keeps_manual_without_profile_and_omits_unprovable_automatic() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"manual","text":"手動"},"#,
                r#"{"id":"automatic","text":"自動"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "current-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        assert!(snapshot.project().last_profile_id().is_none());
        let group = &snapshot.files()[0].groups()[0];
        let rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则应该合法");
        let manual = &group.units()[0];
        let binding = GenericPlaceholderService::default()
            .protect(group.kind(), manual.source_text(), &rules)
            .expect("原文应该可保护")
            .binding_fingerprint();
        let manual_state = manual_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(group.id().to_owned(), manual.id().to_owned()),
            manual.source_text(),
            group.context_fingerprint(),
            binding,
        );
        let automatic = &group.units()[1];
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[
                    crate::generic::TranslationWrite {
                        group_id: group.id().to_owned(),
                        unit_id: manual.id().to_owned(),
                        expected_source_text: manual.source_text().to_owned(),
                        expected_group_context: group.context_fingerprint(),
                        translation: "人工译文".to_owned(),
                        origin: crate::generic::TranslationOrigin::Manual,
                        state_fingerprint: manual_state,
                        expected_translation: None,
                    },
                    crate::generic::TranslationWrite {
                        group_id: group.id().to_owned(),
                        unit_id: automatic.id().to_owned(),
                        expected_source_text: automatic.source_text().to_owned(),
                        expected_group_context: group.context_fingerprint(),
                        translation: "无法证明语义的自动译文".to_owned(),
                        origin: crate::generic::TranslationOrigin::Automatic,
                        state_fingerprint: fingerprint(70),
                        expected_translation: None,
                    },
                ],
            )
            .expect("应该可保存测试译文");
        let (stored, live) = store.ensure_input_current().expect("输入应该仍为 Current");
        let terminology = CompiledTerminology::empty();
        let current = collect_generic_current_translations(
            &stored,
            &terminology,
            &rules,
            None,
            &CooperativeCancellation::default(),
        )
        .expect("人工 Current 应可独立计算");
        let manual_key = GenericUnitKey::new("group".to_owned(), "manual".to_owned());
        assert_eq!(
            current
                .get_with_cancellation(&manual_key, || { Ok::<_, std::convert::Infallible>(()) })
                .unwrap_or_else(|never| match never {})
                .map(String::as_str),
            Some("人工译文")
        );
        let automatic_key = GenericUnitKey::new("group".to_owned(), "automatic".to_owned());
        assert!(
            !current
                .contains_with_cancellation(&automatic_key, || {
                    Ok::<_, std::convert::Infallible>(())
                })
                .unwrap_or_else(|never| match never {})
        );
        let candidate =
            build_write_back_candidate(&stored, &live, &current).expect("Partial 应允许写回");
        assert_eq!(candidate.translated_units(), 1);
        assert_eq!(candidate.retained_source_units(), 1);
    }

    #[test]
    fn materialization_rejects_changed_disk_bytes_and_removes_scratch() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"原文"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let workspace_root = temporary.path().join("project");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "materialize-test".parse().expect("项目名应该合法"),
            workspace_root: workspace_root.clone(),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let (stored, live) = store.ensure_input_current().expect("输入应该仍为 Current");
        let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
            .expect("应该可建立写回候选");

        let result = materialize_write_back_source_with(
            &workspace_root,
            &candidate,
            &CooperativeCancellation::default(),
            |path, bytes| {
                fs::write(path, bytes)?;
                fs::write(
                    path,
                    concat!(
                        r#"{"id":"group","kind":"dialogue","units":["#,
                        r#"{"id":"unit","text":"落盘后被改写"}]}"#,
                        "\n"
                    ),
                )
            },
        );

        assert!(matches!(
            result,
            Err(GenericScratchError::InvalidMaterializedFile { source, .. })
                if matches!(
                    source.as_ref(),
                    GenericWriteBackError::MaterializedMismatch {
                        bytes_changed: true,
                        structure_changed: true,
                        ..
                    }
                )
        ));
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(WRITE_BACK_SCRATCH_PREFIX)),
            "校验失败后不应残留 Generic 写回暂存目录"
        );

        let cancellation = CooperativeCancellation::default();
        let write_cancellation = cancellation.clone();
        let result = materialize_write_back_source_with(
            &workspace_root,
            &candidate,
            &cancellation,
            move |path, bytes| {
                fs::write(path, bytes)?;
                write_cancellation.request();
                Ok(())
            },
        );
        assert!(matches!(result, Err(GenericScratchError::Cancelled)));
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(WRITE_BACK_SCRATCH_PREFIX)),
            "取消后不应残留 Generic 写回暂存目录"
        );
    }

    #[tokio::test]
    async fn publish_recheck_rejects_source_changed_after_candidate_and_preserves_previous_output()
    {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"原文"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let workspace_root = temporary.path().join("project");
        let project_name: ProjectName = "publish-recheck".parse().expect("项目名应该合法");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: project_name.clone(),
            workspace_root: workspace_root.clone(),
            source_root: Some(source_root.clone()),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let (stored, live) = store.ensure_input_current().expect("首次复查应该通过");
        let project = stored.project().clone();
        let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
            .expect("应该可用首次复查快照建立候选");
        let output_root = project.write_back_root();
        fs::create_dir_all(&output_root).expect("应该可建立上一次输出");
        fs::write(output_root.join("previous.txt"), b"previous").expect("应该可写入上一次输出");

        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"候选后变化"}]}"#,
                "\n"
            ),
        )
        .expect("应该可在候选建立后修改外部输入");
        let file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立文件运行能力");
        let publisher = file_system.directory_publisher(
            crate::runtime::filesystem::DirectoryPublisherConfig::production(
                temporary.path().join("publish-locks"),
            )
            .expect("发布锁配置应该合法"),
        );

        let result = publish_generic_write_back(
            publisher,
            project_name,
            project,
            candidate,
            CooperativeCancellation::default(),
            GenericWriteBackPublicationGate::default(),
            || {},
        )
        .await;

        assert!(result.is_err(), "发布前输入变化必须拒绝发布");
        assert_eq!(
            fs::read(output_root.join("previous.txt")).expect("上一次输出应该仍可读取"),
            b"previous"
        );
        assert_eq!(
            fs::read_dir(&output_root)
                .expect("应该可列举上一次输出")
                .count(),
            1,
            "失败不得把候选内容混入上一次输出"
        );
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(WRITE_BACK_SCRATCH_PREFIX)),
            "发布前复查失败后不应残留 Generic 写回暂存目录"
        );
        file_system
            .shutdown()
            .await
            .expect("文件运行能力应该可终结");
    }

    #[tokio::test]
    async fn publish_setup_failure_removes_materialized_scratch() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"原文"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let workspace_root = temporary.path().join("project");
        let project_name: ProjectName = "publish-setup".parse().expect("项目名应该合法");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: project_name.clone(),
            workspace_root: workspace_root.clone(),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let (stored, live) = store.ensure_input_current().expect("输入复查应该通过");
        let project = stored.project().clone();
        let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
            .expect("应该可建立写回候选");
        let output_root = project.write_back_root();
        fs::write(&output_root, b"occupied").expect("应该可建立非目录发布目标");
        let file_system =
            SystemFileSystem::new(crate::runtime::filesystem::SystemFileSystemConfig::production())
                .expect("应该可建立文件运行能力");
        let publisher = file_system.directory_publisher(
            crate::runtime::filesystem::DirectoryPublisherConfig::production(
                temporary.path().join("publish-locks"),
            )
            .expect("发布锁配置应该合法"),
        );

        let result = publish_generic_write_back(
            publisher,
            project_name,
            project,
            candidate,
            CooperativeCancellation::default(),
            GenericWriteBackPublicationGate::default(),
            || {},
        )
        .await;

        assert!(result.is_err(), "非目录目标必须阻止发布");
        assert_eq!(
            fs::read(&output_root).expect("原目标文件应该保持"),
            b"occupied"
        );
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(WRITE_BACK_SCRATCH_PREFIX)),
            "发布请求建立失败后不应残留 Generic 写回暂存目录"
        );
        file_system
            .shutdown()
            .await
            .expect("文件运行能力应该可终结");
    }

    #[test]
    fn cross_kind_dedup_validates_reuse_and_model_output_for_each_target() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"source","kind":"dialogue","units":[{"id":"unit","text":"こんにちは"}]}"#,
                "\n",
                r#"{"id":"source-2","kind":"dialogue","units":[{"id":"unit","text":"こんにちは"}]}"#,
                "\n",
                r#"{"id":"target","kind":"name","units":[{"id":"unit","text":"こんにちは"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "cross-kind-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["name".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let terminology = Arc::new(CompiledTerminology::empty());
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
            None,
        ));
        let resources = AutomaticStateResources {
            prompt: fingerprint(81),
            client_semantics: fingerprint(82),
            language_module: fingerprint(83),
            terminology_hits: empty_terminology_fingerprint(),
        };

        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &rules,
            Arc::clone(&language_module),
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            &CooperativeCancellation::default(),
        )
        .expect("同文应该合并为一个模型输出");
        assert_eq!(prepared.plan.tasks().len(), 1);
        let task = &prepared.plan.tasks()[0];
        assert_eq!(
            task.expected_output_ids()
                .map(TaskId::get)
                .collect::<Vec<_>>(),
            [0]
        );
        let parsed = parse_translation_response(
            r#"{"0":["你好 {invented}"]}"#,
            TranslationResponseMode::new(false, false),
        )
        .expect("响应应该可解析");
        let mut validation_counts = HashMap::<String, usize>::new();
        let acceptance = accept_generic_response_with(
            task.clone(),
            &parsed,
            &prepared.facts,
            |fact, candidate| {
                *validation_counts.entry(fact.kind.clone()).or_default() += 1;
                validate_generic_candidate_fact(fact, candidate, &rules, language_module.as_ref())
            },
        );

        assert_eq!(acceptance.accepted().len(), 2, "同 kind 的合法目标都应保存");
        assert_eq!(
            validation_counts,
            HashMap::from([("dialogue".to_owned(), 1), ("name".to_owned(), 1)]),
            "同一 output_id 只需为每种 kind 完整验收一次"
        );
        assert_eq!(acceptance.accepted_output_count(), 1);
        assert!(acceptance.problems().iter().any(|problem| {
            matches!(
                problem,
                crate::generic::ResponseProblem::InvalidDestination {
                    output_id,
                    key,
                    ..
                } if *output_id == task_id(0)
                    && key.group_id() == "target"
                    && key.unit_id() == "unit"
            )
        }));

        let source_group = &snapshot.files()[0].groups()[0];
        let source_unit = &source_group.units()[0];
        let protected = GenericPlaceholderService::default()
            .protect(source_group.kind(), source_unit.source_text(), &rules)
            .expect("源文没有命中 Placeholder");
        assert!(protected.placeholders().is_empty());
        let state = manual_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(source_group.id().to_owned(), source_unit.id().to_owned()),
            source_unit.source_text(),
            source_group.context_fingerprint(),
            protected.binding_fingerprint(),
        );
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[crate::generic::TranslationWrite {
                    group_id: source_group.id().to_owned(),
                    unit_id: source_unit.id().to_owned(),
                    expected_source_text: source_unit.source_text().to_owned(),
                    expected_group_context: source_group.context_fingerprint(),
                    translation: "你好 {invented}".to_owned(),
                    origin: crate::generic::TranslationOrigin::Manual,
                    state_fingerprint: state,
                    expected_translation: None,
                }],
            )
            .expect("应该可保存 dialogue Current");
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let prepared = prepare_generic_translation(
            &snapshot,
            terminology,
            &rules,
            language_module,
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            &CooperativeCancellation::default(),
        )
        .expect("复用失败的目标应该改为请求模型");

        assert_eq!(prepared.plan.reused().len(), 1);
        assert_eq!(
            prepared.plan.reused()[0].key().group_id(),
            "source-2",
            "同 kind 目标仍应复用 Current"
        );
        assert_eq!(prepared.plan.tasks().len(), 1);
        assert_eq!(prepared.plan.tasks()[0].groups().len(), 3);
        assert_eq!(prepared.plan.tasks()[0].groups()[2].kind(), "name");
        assert_eq!(
            prepared.plan.tasks()[0]
                .expected_output_ids()
                .map(TaskId::get)
                .collect::<Vec<_>>(),
            [0],
            "复用失败的目标不能从同一次 Translate 消失"
        );
    }

    #[test]
    fn candidate_validation_restores_placeholders_and_allows_free_line_breaks() {
        let service = GenericPlaceholderService::default();
        let rules = service
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let protected = service
            .protect("dialogue", "こんにちは {name}", &rules)
            .expect("原文应该可保护");
        let token = protected.placeholders()[0].token().to_owned();
        let language_text = protected.language_text().expect("保护文本应该可投影");
        let language_module = JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
            None,
        );
        let key = GenericUnitKey::new("group".to_owned(), "unit".to_owned());
        let mut facts = GenericUnitMap::new();
        let previous = facts
            .insert_with_cancellation(
                key.clone(),
                GenericValidationFact {
                    kind: "dialogue".to_owned(),
                    source_text: "こんにちは {name}".to_owned(),
                    analysis: language_module.analyze_source(&language_text),
                    protected,
                },
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        assert!(previous.is_none());

        assert_eq!(
            validate_generic_candidate(
                &key,
                &format!("你好\n世界 {token}"),
                &facts,
                &rules,
                &language_module,
            )
            .expect("合法译文应该通过验收"),
            "你好\n世界 {name}"
        );
        assert!(
            validate_generic_candidate(&key, "你好", &facts, &rules, &language_module).is_err(),
            "丢失 Placeholder 的译文必须被拒绝"
        );
        assert!(
            validate_generic_candidate(
                &key,
                &format!("你好 {token} {{invented}}"),
                &facts,
                &rules,
                &language_module,
            )
            .is_err(),
            "新增原文不存在的 Placeholder 必须被拒绝"
        );
        assert!(
            validate_generic_candidate(
                &key,
                &format!("こんにちは {token}"),
                &facts,
                &rules,
                &language_module,
            )
            .is_err(),
            "仍含受限源语言文本的译文必须被拒绝"
        );
    }
}
