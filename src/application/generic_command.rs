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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use rayon::prelude::*;
use rusqlite::Connection;
use time::OffsetDateTime;

use super::command::{
    ActiveProjectLog, CommandLogStart, PendingProjectLog, ProjectLogLuaPrintSink,
    TerminationSignals, start_command_log,
};
use super::config::{
    ConfiguredGenericCommand, ConfiguredGenericWriteBackCommand, ConfiguredProjectLuaCommand,
    ConfiguredTranslateCommand,
};
use super::translation_prompt::{
    SYSTEM_PROMPT_FILE_NAME, THINKING_PROMPT_FILE_NAME, ensure_no_prompt_template_variables,
    read_prompt_resource, render_system_prompt_template,
};
use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::CpuTaskExecutor;
use crate::execution::llm_request::{
    AsyncDelay, LlmRequestExecutionOutcome, LlmRequestRetryPolicy, execute_llm_request_with_retry,
};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::generic::{
    AutomaticStateResources, CommitTranslationsOutcome, ExtractOutcome,
    GenericCompiledPlaceholderRules, GenericInitRequest, GenericPlaceholderService,
    GenericPlanningError, GenericProject, GenericProjectStore, GenericProtectedText,
    GenericStoredSnapshot, GenericTaskRecordDocument, GenericTaskRecordIssue,
    GenericTaskRecordState, GenericTaskResponseRecord, GenericUnitKey, GenericWriteBackCandidate,
    GenericWriteBackError, PlannedGroup, PlannedTask, PlanningUnit, TranslationAcceptance,
    TranslationPlan, accept_parsed_response, build_write_back_candidate,
    current_translation_for_stored, ensure_input_fingerprints_current, plan_translation,
    split_tasks_by_rendered_size, validate_materialized_write_back_file,
};
use crate::i18n::UiLocale;
use crate::language::{LanguageAnalysis, LanguageModule, LanguageText, LanguageTextSegment};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity, LlmFinishReason,
};
use crate::progress::ProgressMode;
use crate::project_lease::{ProjectCommandLeaseProvider, ProjectCommandLeaseService};
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaFailure, ProjectLuaProgram, ProjectLuaProject,
    ProjectLuaRunError, ProjectLuaRunRequest, compile_project_lua_program,
    generic_project_lua_adapter, run_project_lua,
};
use crate::project_name::ProjectName;
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::{
    SystemDirectoryPublisher, SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemError,
};
use crate::runtime::llm::OpenAiChatCompletionExecutor;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLog, ProjectLogCode, ProjectLogEvent, ProjectLogLevel, ProjectLogPayload,
    ProjectLogRunOutcome,
};
use crate::storage::file_system::{
    DirectoryPublishError, DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    FileReader, RecoverableDirectoryPublisher, StagingCleanupFailure,
};
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::placeholder_token;
use crate::translation::planning_resource::{
    CompiledTerminology, TranslationPlanningResourceReader,
    TranslationPlanningResourceReadingService,
};
use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
};
use crate::translation_protocol::{
    ParsedTranslationResponse, TranslationResponseEnvelope, parse_translation_response,
};

const GENERIC_ENGINE_NAME: &str = "generic";
const GENERIC_PROMPT_DIRECTORY_NAME: &str = "generic";
const WRITE_BACK_SCRATCH_PREFIX: &str = ".generic-write-back-";

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
            Self::Signal { source, operation } => SafeDiagnostic::io(
                DiagnosticCode::SignalRegistration,
                DiagnosticStage::Shutdown,
                DiagnosticSubject::component("Windows control signal"),
                "receive_signal",
                source,
                operation
                    .as_ref()
                    .map_or(DiagnosticImpact::Unchanged, |error| {
                        error.safe_diagnostic().impact
                    }),
                DiagnosticAction::Retry,
            ),
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
            Self::Signal { source, operation } => {
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
        "同步 Generic JSONL" => (
            DiagnosticCode::ExtractDocumentRead,
            DiagnosticStage::Extract,
            DiagnosticFailureKind::SourceDocumentInvalid,
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
        "选择 Generic Prompt 语言"
        | "读取 Generic system Prompt"
        | "渲染 Generic system Prompt"
        | "读取 Generic thinking Prompt"
        | "校验 Generic thinking Prompt" => (
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

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MessageError {}

/// Generic 的生产命令执行器。
pub(crate) struct ProductionGenericCommandRunner {
    locale: UiLocale,
    _progress_mode: ProgressMode,
}

impl ProductionGenericCommandRunner {
    pub(crate) const fn new(locale: UiLocale, progress_mode: ProgressMode) -> Self {
        Self {
            locale,
            _progress_mode: progress_mode,
        }
    }

    pub(crate) async fn run(
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
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(|source| GenericCommandError::operation("取得项目租约", source))?;
                    if operation_cancellation.is_requested() {
                        return Err(GenericCommandError::Cancelled);
                    }
                    let request = GenericInitRequest {
                        project_name,
                        workspace_root,
                        source_root: arguments.path,
                        source_language: arguments.source_language,
                        target_language: arguments.target_language,
                    };
                    let (_, project) = run_blocking("初始化 Generic 项目", move || {
                        GenericProjectStore::initialize(request)
                    })
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
                )
                .await
            }
            ConfiguredGenericCommand::Extract {
                project_name,
                common,
            } => {
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
                let store = GenericProjectStore::for_workspace(generic_workspace(
                    common.projects_root(),
                    &project_name,
                ));
                let lease_provider = ProjectCommandLeaseService::new(
                    common.projects_root().to_path_buf(),
                    GENERIC_ENGINE_NAME,
                    file_system.clone(),
                );
                let output_name = project_name.clone();
                let operation_cancellation = cancellation.clone();
                let cancellation_file_system = file_system.clone();
                let operation_project_log = Arc::clone(&project_log);
                let locale = self.locale;
                let operation = async move {
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(|source| GenericCommandError::operation("取得项目租约", source))?;
                    if operation_cancellation.is_requested() {
                        return Err(GenericCommandError::Cancelled);
                    }
                    let open_store = store.clone();
                    run_blocking("打开 Generic 项目", move || open_store.open()).await?;
                    install_generic_project_log(
                        &operation_project_log,
                        start_command_log(CommandLogStart {
                            common: &common,
                            locale,
                            engine: GENERIC_ENGINE_NAME,
                            project: project_name.as_str(),
                            command: "extract",
                            stage: DiagnosticStage::Extract,
                            profile: None,
                            performance,
                            panic_boundary: None,
                        }),
                    );
                    let outcome =
                        run_blocking("同步 Generic JSONL", move || store.extract()).await?;
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
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
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
        let lua_cancellation = ProjectLuaCancellation::default();
        let project_name = command.project_name().clone();
        let script_path = command.script().script_path().to_path_buf();
        let arguments = command.arguments().to_vec();
        let store = GenericProjectStore::for_workspace(generic_workspace(
            command.common().projects_root(),
            &project_name,
        ));
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
        let common = command.common();
        let operation_project_log = Arc::clone(&project_log);
        let locale = self.locale;
        let operation = async move {
            let script = operation_file_system
                .read_file(script_path.clone())
                .await
                .map_err(|source| GenericCommandError::operation("读取 Lua 脚本", source))?;
            let identity = script.resolved_path().to_string_lossy().into_owned();
            let source = script.into_bytes();
            let mut fingerprint = Sha256FramedHasher::new(b"att.project-lua.program-identity");
            fingerprint.frame(1, source.as_slice());
            let fingerprint = fingerprint.finish();
            let program = ProjectLuaProgram::new(identity.clone(), source, arguments);
            let project_log = start_command_log(CommandLogStart {
                common,
                locale,
                engine: GENERIC_ENGINE_NAME,
                project: project_name.as_str(),
                command: "lua",
                stage: DiagnosticStage::Lua,
                profile: None,
                performance,
                panic_boundary: None,
            });
            project_log.logger().emit(ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::LuaScript,
                project_log.context().clone(),
                ProjectLogPayload::LuaScript {
                    identity: identity.clone(),
                    fingerprint: fingerprint.hex(),
                },
            ));
            let print_sink = Arc::new(ProjectLogLuaPrintSink::from_active(&project_log));
            install_generic_project_log(&operation_project_log, project_log);
            compile_project_lua_program(&program).map_err(|source| {
                let source = GenericLuaPreflightError(source);
                let detail = source.to_string();
                let diagnostic = project_lua_failure_diagnostic(
                    &source.0,
                    DiagnosticStage::CommandPreparation,
                    &detail,
                );
                GenericCommandError::diagnosed("编译 Lua 脚本", source, diagnostic)
            })?;
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }

            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(|source| GenericCommandError::operation("取得项目租约", source))?;
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }
            let project = run_blocking("打开 Generic 项目", move || store.open()).await?;
            let database_path = project.database_path().to_path_buf();
            let lua_project_name = output_name.as_str().to_owned();
            let lua_adapter = generic_project_lua_adapter(project);
            let request = ProjectLuaRunRequest::new(
                ProjectLuaProject::new(lua_project_name, GENERIC_ENGINE_NAME),
                program,
                lua_adapter,
            )
            .with_cancellation(operation_lua_cancellation)
            .with_print_sink(print_sink);
            let report = tokio::task::spawn_blocking(move || {
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
            .await
            .map_err(|source| GenericCommandError::operation("执行 Generic Lua", source))?;
            let report = match report {
                Ok(report) => report,
                Err(source) if source.is_cancelled() => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let diagnostic = generic_lua_execution_diagnostic(&source);
                    return Err(GenericCommandError::diagnosed(
                        "执行 Generic Lua",
                        source,
                        diagnostic,
                    ));
                }
            };
            if let Some(project_log) = operation_project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                project_log.logger().emit(ProjectLogEvent::new(
                    ProjectLogLevel::Info,
                    ProjectLogCode::LuaSummary,
                    project_log.context().clone(),
                    ProjectLogPayload::LuaSummary {
                        database_calls: report.database_calls(),
                        changed_rows: report.changed_rows(),
                        translation_calls: report.translation_calls(),
                        printed_lines: report.printed_lines(),
                    },
                ));
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
        )
        .await
    }

    async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration.clone(), Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::failed(GenericCommandError::operation(
                        "启动文件运行能力",
                        source,
                    ));
                }
            };
        let project_log = generic_project_log_slot();
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let mut report = GenericCommandRunReport::failed(GenericCommandError::operation(
                    "启动 CPU 运行能力",
                    source,
                ));
                if let Err(source) = file_system.shutdown().await {
                    report
                        .shutdown_errors
                        .push(GenericShutdownError::new("filesystem", source));
                }
                return report;
            }
        };
        let cancellation = CooperativeCancellation::default();
        let llm_holder = Arc::new(Mutex::new(None::<OpenAiChatCompletionExecutor>));
        let project_name = command.project_name().clone();
        let store = GenericProjectStore::for_workspace(generic_workspace(
            command.common().projects_root(),
            &project_name,
        ));
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
            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(|source| GenericCommandError::operation("取得项目租约", source))?;
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }

            let initial_store = store.clone();
            let (snapshot, _live, current_resources) =
                run_blocking("复查 Generic 输入", move || {
                    initial_store.load_current_translation_state()
                })
                .await?;
            let project = snapshot.project().clone();
            install_generic_project_log(
                &operation_project_log,
                start_command_log(CommandLogStart {
                    common: command.common(),
                    locale,
                    engine: GENERIC_ENGINE_NAME,
                    project: project_name.as_str(),
                    command: "translate",
                    stage: DiagnosticStage::Translate,
                    profile: None,
                    performance,
                    panic_boundary: None,
                }),
            );
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
                configuration,
                project.language_pair(),
            )
            .await?;
            let resource_reader = TranslationPlanningResourceReadingService::new(
                operation_file_system.clone(),
                operation_cpu.clone(),
            );
            let resources = resource_reader
                .read(
                    command.terminology_path().map(Path::to_path_buf),
                    command.placeholder_rules_path().map(Path::to_path_buf),
                    current_resources.terminology_json().to_owned(),
                    current_resources.placeholder_rules_json().to_owned(),
                )
                .await
                .map_err(|source| GenericCommandError::operation("读取翻译资源", source))?;
            let (terminology, placeholder_definitions, terminology_json, placeholder_json) =
                resources.into_parts();
            let placeholder_rules = GenericPlaceholderService::default()
                .compile(placeholder_definitions)
                .map_err(|source| {
                    GenericCommandError::operation("编译 Generic Placeholder", source)
                })?;

            let expected_raw_fingerprint = snapshot
                .project()
                .extracted_raw_fingerprint()
                .expect("load_current_translation_state 已确认存在 Extract 指纹");
            let planning_snapshot = snapshot;
            let planning_terms = Arc::clone(&terminology);
            let planning_rules = placeholder_rules.clone();
            let planning_language = Arc::clone(&source_language);
            let planning_prompt = prompt.fingerprint;
            let planning_client = configuration.client().semantic_fingerprint();
            let planning_language_fingerprint = source_language.semantic_fingerprint();
            let target_characters = configuration
                .profile()
                .target_task_user_message_characters();
            let prepared = operation_cpu
                .execute(move || {
                    prepare_generic_translation(
                        &planning_snapshot,
                        planning_terms,
                        &planning_rules,
                        planning_language,
                        AutomaticStateResources {
                            prompt: planning_prompt,
                            client_semantics: planning_client,
                            language_module: planning_language_fingerprint,
                            terminology_hits: empty_terminology_fingerprint(),
                        },
                        target_characters,
                    )
                })
                .await
                .map_err(|source| GenericCommandError::operation("调度 Generic 翻译计划", source))?
                .map_err(|source| {
                    GenericCommandError::operation("建立 Generic 翻译计划", source)
                })?;

            let PreparedGenericTranslation { plan, facts } = prepared;
            let (invalidations, reused, tasks) = plan.into_parts();
            let mut summary = GenericTranslationSummary {
                total_tasks: tasks.len(),
                ..GenericTranslationSummary::default()
            };
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }
            let invalidations = invalidations
                .into_iter()
                .map(|invalidation| invalidation.into_clear())
                .collect::<Vec<_>>();
            if should_apply_translation_resources(
                current_resources.terminology_json(),
                current_resources.placeholder_rules_json(),
                &terminology_json,
                &placeholder_json,
                invalidations.len(),
            ) {
                let save_store = store.clone();
                let resource_outcome = run_blocking(
                    "保存 Generic 翻译资源并清除失效译文",
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
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }

            let reuse_writes = reused
                .into_iter()
                .map(|reuse| reuse.into_write())
                .collect::<Vec<_>>();
            summary.reused_units = reuse_writes.len();
            if !reuse_writes.is_empty() {
                let commit_store = store.clone();
                let reuse_profile = profile_id.clone();
                let outcome = run_blocking("提交 Generic 去重复用译文", move || {
                    commit_store.commit_translations_for_profile(
                        expected_raw_fingerprint,
                        &reuse_writes,
                        &reuse_profile,
                    )
                })
                .await?;
                add_commit_outcome(&mut summary, &outcome);
            }

            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }

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
                let task_records = configure_generic_task_records(
                    command.record_translation_tasks(),
                    &operation_project_log,
                    &file_system_configuration,
                    configuration.client().record_metadata(),
                    locale,
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
                    response_envelope: prompt.response_envelope,
                    client: Arc::clone(configuration.client()),
                    llm: llm.clone(),
                    retry_delays: configuration
                        .profile()
                        .request()
                        .network_retry_delays()
                        .to_vec(),
                    max_retry_after: configuration.profile().request().max_network_retry_after(),
                    cancellation: operation_cancellation.clone(),
                    task_records: task_records.clone(),
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

            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }
            if should_remember_profile_separately(&summary) {
                let remember_store = store.clone();
                let remembered_profile = profile_id.clone();
                run_blocking("保存 Generic 最近 Profile", move || {
                    remember_store.remember_profile(&remembered_profile)
                })
                .await?;
            }
            Ok(GenericCommandOutput::Translate {
                project: output_name,
                profile_id,
                summary,
            })
        };

        let driven = drive(operation, termination_signals, || {
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
        let mut shutdown_errors = Vec::new();
        if let Err(source) = cpu.shutdown() {
            shutdown_errors.push(GenericShutdownError::new("CPU executor", source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::new("filesystem", source));
        }
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
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::failed(GenericCommandError::operation(
                        "启动文件运行能力",
                        source,
                    ));
                }
            };
        let project_log = generic_project_log_slot();
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let mut report = GenericCommandRunReport::failed(GenericCommandError::operation(
                    "启动 CPU 运行能力",
                    source,
                ));
                if let Err(source) = file_system.shutdown().await {
                    report
                        .shutdown_errors
                        .push(GenericShutdownError::new("filesystem", source));
                }
                return report;
            }
        };
        let cancellation = CooperativeCancellation::default();
        let project_name = command.project_name().clone();
        let store = GenericProjectStore::for_workspace(generic_workspace(
            command.common().projects_root(),
            &project_name,
        ));
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let operation_file_system = file_system.clone();
        let operation_cpu = cpu.clone();
        let operation_cancellation = cancellation.clone();
        let output_name = project_name.clone();
        let locale = self.locale;
        let operation_project_log = Arc::clone(&project_log);
        let operation = async move {
            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(|source| GenericCommandError::operation("取得项目租约", source))?;
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }

            let initial_store = store.clone();
            let (snapshot, live, current_resources) =
                run_blocking("复查 Generic 写回输入", move || {
                    initial_store.load_current_translation_state()
                })
                .await?;
            let project = snapshot.project().clone();
            install_generic_project_log(
                &operation_project_log,
                start_command_log(CommandLogStart {
                    common: command.common(),
                    locale,
                    engine: GENERIC_ENGINE_NAME,
                    project: project_name.as_str(),
                    command: "write-back",
                    stage: DiagnosticStage::WriteBack,
                    profile: None,
                    performance,
                    panic_boundary: None,
                }),
            );
            let resource_reader = TranslationPlanningResourceReadingService::new(
                operation_file_system.clone(),
                operation_cpu.clone(),
            );
            let resources = resource_reader
                .read(
                    None,
                    None,
                    current_resources.terminology_json().to_owned(),
                    current_resources.placeholder_rules_json().to_owned(),
                )
                .await
                .map_err(|source| GenericCommandError::operation("读取翻译资源", source))?;
            let (terminology, placeholder_definitions, _, _) = resources.into_parts();
            let placeholder_rules = GenericPlaceholderService::default()
                .compile(placeholder_definitions)
                .map_err(|source| {
                    GenericCommandError::operation("编译 Generic Placeholder", source)
                })?;

            let has_automatic_translation = snapshot
                .files()
                .iter()
                .flat_map(|file| file.groups())
                .flat_map(|group| group.units())
                .filter_map(|unit| unit.translation())
                .any(|translation| {
                    matches!(
                        translation.origin(),
                        crate::generic::TranslationOrigin::Automatic
                    )
                });
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
                            &configuration,
                            project.language_pair(),
                        )
                        .await?;
                        Some(AutomaticStateResources {
                            prompt: prompt.fingerprint,
                            client_semantics: configuration.client().semantic_fingerprint(),
                            language_module: source_language.semantic_fingerprint(),
                            terminology_hits: empty_terminology_fingerprint(),
                        })
                    }
                    None => None,
                }
            } else {
                None
            };
            let current_snapshot = snapshot;
            let current_terms = Arc::clone(&terminology);
            let current_rules = placeholder_rules.clone();
            let (current_snapshot, current_translations) = operation_cpu
                .execute(move || {
                    let current_translations = collect_generic_current_translations(
                        &current_snapshot,
                        current_terms.as_ref(),
                        &current_rules,
                        automatic_resources,
                    )?;
                    Ok::<_, GenericPreparationError>((current_snapshot, current_translations))
                })
                .await
                .map_err(|source| {
                    GenericCommandError::operation("调度 Generic Current 复查", source)
                })?
                .map_err(|source| GenericCommandError::operation("复查 Generic Current", source))?;
            if operation_cancellation.is_requested() {
                return Err(GenericCommandError::Cancelled);
            }
            let (write_back_project, candidate) = operation_cpu
                .execute(move || {
                    let project = current_snapshot.project().clone();
                    build_write_back_candidate(&current_snapshot, &live, &current_translations)
                        .map(|candidate| (project, candidate))
                })
                .await
                .map_err(|source| GenericCommandError::operation("建立 Generic 写回候选", source))?
                .map_err(|source| {
                    GenericCommandError::operation("建立 Generic 写回候选", source)
                })?;
            publish_generic_write_back(
                directory_publisher,
                output_name,
                write_back_project,
                candidate,
            )
            .await
        };

        let driven = drive(operation, termination_signals, || {
            cancellation.request();
            cpu.cancel_waits();
            file_system.cancel_waits();
        })
        .await;
        let mut shutdown_errors = Vec::new();
        if let Err(source) = cpu.shutdown() {
            shutdown_errors.push(GenericShutdownError::new("CPU executor", source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::new("filesystem", source));
        }
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
    let (failure, action) = match source {
        ProjectLuaFailure::Compile(_) => (
            DiagnosticFailureKind::LuaCompilationFailed,
            DiagnosticAction::FixInput,
        ),
        ProjectLuaFailure::Cancelled => (
            DiagnosticFailureKind::LockCancelled,
            DiagnosticAction::Retry,
        ),
        ProjectLuaFailure::Context(_) => (
            DiagnosticFailureKind::LuaContextCreationFailed,
            DiagnosticAction::ReportBug,
        ),
        ProjectLuaFailure::Script(_) => (
            DiagnosticFailureKind::LuaExecutionFailed,
            DiagnosticAction::FixInput,
        ),
        ProjectLuaFailure::Host { .. } => (
            DiagnosticFailureKind::LuaHostCallFailed,
            DiagnosticAction::FixInput,
        ),
        ProjectLuaFailure::Database(_) => (
            DiagnosticFailureKind::LuaExecutionFailed,
            DiagnosticAction::CheckProjectState,
        ),
        ProjectLuaFailure::Validation(_) => (
            DiagnosticFailureKind::LuaFinalizationFailed,
            DiagnosticAction::FixInput,
        ),
        ProjectLuaFailure::Panicked => (
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticAction::ReportBug,
        ),
    };
    SafeDiagnostic::new(
        DiagnosticCode::LuaExecution,
        stage,
        DiagnosticSubject::operation("project_lua_transaction"),
        DiagnosticReason::failure_with_detail(failure, detail),
        DiagnosticImpact::Unchanged,
        action,
    )
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
    response_envelope: TranslationResponseEnvelope,
    fingerprint: Sha256Fingerprint,
}

async fn load_generic_prompt(
    file_system: &SystemFileSystem,
    configuration: &super::config::TranslateConfiguration,
    language_pair: &crate::language::LanguagePair,
) -> Result<LoadedGenericPrompt, GenericCommandError> {
    let prompt_locale = configuration
        .prompt_locale()
        .resolve(language_pair.target())
        .map_err(|source| GenericCommandError::operation("选择 Generic Prompt 语言", source))?;
    let prompt_directory = configuration
        .prompt_root()
        .join(GENERIC_PROMPT_DIRECTORY_NAME)
        .join(prompt_locale.as_str());
    let template =
        read_prompt_resource(file_system, &prompt_directory.join(SYSTEM_PROMPT_FILE_NAME))
            .await
            .map_err(|source| {
                GenericCommandError::operation("读取 Generic system Prompt", source)
            })?;
    let mut system_prompt = render_system_prompt_template(&template, language_pair)
        .map_err(|source| GenericCommandError::operation("渲染 Generic system Prompt", source))?;
    let response_envelope = if configuration.thinking_output() {
        let thinking = read_prompt_resource(
            file_system,
            &prompt_directory.join(THINKING_PROMPT_FILE_NAME),
        )
        .await
        .map_err(|source| GenericCommandError::operation("读取 Generic thinking Prompt", source))?;
        ensure_no_prompt_template_variables(&thinking).map_err(|source| {
            GenericCommandError::operation("校验 Generic thinking Prompt", source)
        })?;
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&thinking);
        TranslationResponseEnvelope::ThinkingThenJson
    } else {
        TranslationResponseEnvelope::JsonOnly
    };
    let mut hasher = Sha256FramedHasher::new(b"att.generic.system-prompt");
    hasher.frame(1, system_prompt.as_bytes()).frame(
        2,
        match response_envelope {
            TranslationResponseEnvelope::JsonOnly => b"json-only",
            TranslationResponseEnvelope::ThinkingThenJson => b"thinking-then-json",
        },
    );
    Ok(LoadedGenericPrompt {
        system_prompt,
        response_envelope,
        fingerprint: hasher.finish(),
    })
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
    facts: HashMap<GenericUnitKey, GenericValidationFact>,
}

struct PreparedGenericGroup {
    planning_units: Vec<PlanningUnit>,
    facts: Vec<(GenericUnitKey, GenericValidationFact)>,
}

#[derive(Debug)]
enum GenericPreparationError {
    Placeholder(crate::generic::GenericPlaceholderError),
    LanguageProjection(LanguageTextProjectionError),
    Planning(GenericPlanningError),
}

impl fmt::Display for GenericPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placeholder(source) => source.fmt(formatter),
            Self::LanguageProjection(source) => source.fmt(formatter),
            Self::Planning(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Placeholder(source) => Some(source),
            Self::LanguageProjection(source) => Some(source),
            Self::Planning(source) => Some(source),
        }
    }
}

fn prepare_generic_translation(
    snapshot: &GenericStoredSnapshot,
    terminology: Arc<CompiledTerminology>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    source_language: Arc<dyn LanguageModule>,
    base_resources: AutomaticStateResources,
    target_task_characters: std::num::NonZeroUsize,
) -> Result<PreparedGenericTranslation, GenericPreparationError> {
    let groups = snapshot
        .files()
        .iter()
        .flat_map(|file| file.groups())
        .collect::<Vec<_>>();
    let prepared_groups = groups
        .par_iter()
        .map(|group| {
            let service = GenericPlaceholderService::default();
            let mut prepared_units = Vec::with_capacity(group.units().len());
            for unit in group.units() {
                let protected = service
                    .protect(group.kind(), unit.source_text(), placeholder_rules)
                    .map_err(GenericPreparationError::Placeholder)?;
                let language_text = protected
                    .language_text()
                    .map_err(GenericPreparationError::LanguageProjection)?;
                let analysis = source_language.analyze_source(&language_text);
                prepared_units.push((unit, protected, language_text, analysis));
            }
            let term_indices = terminology.triggered_indices(
                prepared_units
                    .iter()
                    .flat_map(|(_, _, language_text, _)| natural_segments(language_text)),
            );
            let terminology_hits = terminology_hit_fingerprint(terminology.as_ref(), &term_indices);
            let mut planning_units = Vec::with_capacity(prepared_units.len());
            let mut facts = Vec::with_capacity(prepared_units.len());
            for (unit, protected, language_text, analysis) in prepared_units {
                let resources = AutomaticStateResources {
                    terminology_hits,
                    ..base_resources
                };
                let planning = PlanningUnit::from_stored(
                    snapshot.project(),
                    group,
                    unit,
                    &protected,
                    term_indices.clone(),
                    language_text.has_non_whitespace_natural_text() && analysis.needs_translation(),
                    resources,
                );
                if planning.needs_candidate() {
                    facts.push((
                        planning.key().clone(),
                        GenericValidationFact {
                            kind: group.kind().to_owned(),
                            source_text: unit.source_text().to_owned(),
                            protected,
                            analysis,
                        },
                    ));
                }
                planning_units.push(planning);
            }
            Ok::<_, GenericPreparationError>(PreparedGenericGroup {
                planning_units,
                facts,
            })
        })
        .collect::<Vec<_>>();

    let mut planning_units = Vec::with_capacity(snapshot.unit_count());
    let mut facts = HashMap::with_capacity(snapshot.unit_count());
    // 并行完成顺序不参与领域语义；按自然 Group 顺序处理结果，保证规划和错误稳定。
    for prepared_group in prepared_groups {
        let prepared_group = prepared_group?;
        planning_units.extend(prepared_group.planning_units);
        for (key, fact) in prepared_group.facts {
            facts.insert(key, fact);
        }
    }
    if planning_units.iter().all(|unit| !unit.needs_planning()) {
        return Ok(PreparedGenericTranslation {
            plan: TranslationPlan::empty(),
            facts,
        });
    }
    let plan = plan_translation(snapshot, &planning_units, |key, candidate| {
        validate_generic_candidate(
            key,
            candidate,
            &facts,
            placeholder_rules,
            source_language.as_ref(),
        )
    })
    .map_err(GenericPreparationError::Planning)?;
    let plan = split_tasks_by_rendered_size(
        plan,
        target_task_characters,
        "Groups:\n".chars().count(),
        |group, first_output_id| {
            measure_generic_group_message(group, terminology.as_ref(), first_output_id)
        },
    );
    Ok(PreparedGenericTranslation { plan, facts })
}

fn collect_generic_current_translations(
    snapshot: &GenericStoredSnapshot,
    terminology: &CompiledTerminology,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    automatic_resources: Option<AutomaticStateResources>,
) -> Result<HashMap<(String, String), String>, GenericPreparationError> {
    let groups = snapshot
        .files()
        .iter()
        .flat_map(|file| file.groups())
        .collect::<Vec<_>>();
    let prepared_groups = groups
        .par_iter()
        .map(|group| {
            let service = GenericPlaceholderService::default();
            let mut protected_units = Vec::with_capacity(group.units().len());
            for unit in group.units() {
                let protected = service
                    .protect(group.kind(), unit.source_text(), placeholder_rules)
                    .map_err(GenericPreparationError::Placeholder)?;
                let language_text = protected
                    .language_text()
                    .map_err(GenericPreparationError::LanguageProjection)?;
                protected_units.push((unit, protected, language_text));
            }
            let term_indices = automatic_resources.map(|_| {
                terminology.triggered_indices(
                    protected_units
                        .iter()
                        .flat_map(|(_, _, language_text)| natural_segments(language_text)),
                )
            });
            let group_resources = automatic_resources.map(|resources| AutomaticStateResources {
                terminology_hits: terminology_hit_fingerprint(
                    terminology,
                    term_indices.as_deref().unwrap_or_default(),
                ),
                ..resources
            });
            let mut current = Vec::new();
            for (unit, protected, _) in protected_units {
                if let Some(translation) = current_translation_for_stored(
                    snapshot.project(),
                    group,
                    unit,
                    protected.binding_fingerprint(),
                    group_resources,
                ) {
                    current.push(((group.id().to_owned(), unit.id().to_owned()), translation));
                }
            }
            Ok::<_, GenericPreparationError>(current)
        })
        .collect::<Vec<_>>();

    let mut current = HashMap::with_capacity(snapshot.unit_count());
    for prepared_group in prepared_groups {
        for (key, translation) in prepared_group? {
            current.insert(key, translation);
        }
    }
    Ok(current)
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

fn empty_terminology_fingerprint() -> Sha256Fingerprint {
    Sha256FramedHasher::new(b"att.generic.terminology-hits").finish()
}

fn terminology_hit_fingerprint(
    terminology: &CompiledTerminology,
    indices: &[usize],
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.terminology-hits");
    for index in indices {
        let entry = &terminology.entries()[*index];
        hasher
            .frame(1, entry.term().as_bytes())
            .frame(2, entry.translation().as_bytes());
    }
    hasher.finish()
}

struct GenericTaskExecution {
    store: GenericProjectStore,
    expected_raw_fingerprint: Sha256Fingerprint,
    profile_id: String,
    tasks: Vec<PlannedTask>,
    facts: Arc<HashMap<GenericUnitKey, GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_envelope: TranslationResponseEnvelope,
    client: Arc<crate::runtime::llm::OpenAiChatCompletionClient>,
    llm: OpenAiChatCompletionExecutor,
    retry_delays: Vec<Duration>,
    max_retry_after: Duration,
    cancellation: CooperativeCancellation,
    task_records: ConfiguredTranslationTaskRecordSink,
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
        acceptance: TranslationAcceptance,
        accepted_outputs: usize,
    },
    Unavailable {
        reason: &'static str,
    },
    Cancelled,
}

struct GenericPreparedTask {
    outcome: GenericPreparedTaskOutcome,
    record: Option<GenericTaskRecordDraft>,
}

#[derive(Clone)]
struct GenericTaskRequestContext {
    total_tasks: usize,
    facts: Arc<HashMap<GenericUnitKey, GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_envelope: TranslationResponseEnvelope,
    client: Arc<crate::runtime::llm::OpenAiChatCompletionClient>,
    llm: OpenAiChatCompletionExecutor,
    retry_delays: Vec<Duration>,
    max_retry_after: Duration,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
}

async fn execute_owned_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> Result<GenericPreparedTask, GenericCommandError> {
    execute_generic_task(
        context.total_tasks,
        task_index,
        task,
        context.facts.as_ref(),
        &context.placeholder_rules,
        context.terminology.as_ref(),
        context.language_module.as_ref(),
        &context.system_prompt,
        context.response_envelope,
        context.client.as_ref(),
        &context.llm,
        &context.retry_delays,
        context.max_retry_after,
        &context.cancellation,
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
        response_envelope,
        client,
        llm,
        retry_delays,
        max_retry_after,
        cancellation,
        task_records,
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
        system_prompt,
        response_envelope,
        client,
        llm,
        retry_delays,
        max_retry_after,
        cancellation: cancellation.clone(),
        record_evidence,
    };
    let mut remaining = tasks.into_iter().enumerate();
    let mut tasks = FuturesOrdered::new();
    for _ in 0..concurrency {
        let Some((task_index, task)) = remaining.next() else {
            break;
        };
        tasks.push_back(execute_owned_generic_task(
            request_context.clone(),
            task_index,
            task,
        ));
    }

    let mut summary = GenericTaskSummary::default();
    let mut terminal_error = None;
    while let Some(prepared) = tasks.next().await {
        let GenericPreparedTask { outcome, record } = match prepared {
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
                    GenericPreparedTaskOutcome::Unavailable { reason } => {
                        GenericTaskRecordState::unavailable(reason)
                    }
                    GenericPreparedTaskOutcome::Accepted { .. } => {
                        GenericTaskRecordState::not_committed_due_to_prior_failure()
                    }
                };
                task_records.submit(record.finish(state));
            }
            continue;
        }
        match outcome {
            GenericPreparedTaskOutcome::Accepted {
                acceptance,
                accepted_outputs,
            } => {
                let (accepted, problems) = acceptance.into_parts();
                let accepted_units = accepted.len();
                let response_problems = problems.len();
                let response_complete = problems.is_empty();
                let mut issues = problems
                    .iter()
                    .map(GenericTaskRecordIssue::from_response_problem)
                    .collect::<Vec<_>>();
                let writes = accepted
                    .into_iter()
                    .map(|accepted| accepted.into_write())
                    .collect::<Vec<_>>();
                let commit = if writes.is_empty() {
                    Ok(CommitTranslationsOutcome {
                        committed: 0,
                        conflicts: Vec::new(),
                    })
                } else {
                    let store = store.clone();
                    let profile_id = profile_id.clone();
                    run_blocking("提交 Generic 模型译文", move || {
                        store.commit_translations_for_profile(
                            expected_raw_fingerprint,
                            &writes,
                            &profile_id,
                        )
                    })
                    .await
                };
                let commit =
                    match commit {
                        Ok(commit) => commit,
                        Err(error) => {
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
                if response_complete && commit.conflicts.is_empty() {
                    summary.complete_tasks += 1;
                } else {
                    summary.partial_tasks += 1;
                }
            }
            GenericPreparedTaskOutcome::Unavailable { reason } => {
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::unavailable(reason)));
                }
                summary.unavailable_tasks += 1;
            }
            GenericPreparedTaskOutcome::Cancelled => {
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::cancelled()));
                }
                cancellation.request();
                terminal_error = Some(GenericCommandError::Cancelled);
            }
        }
        if terminal_error.is_none()
            && let Some((task_index, task)) = remaining.next()
        {
            tasks.push_back(execute_owned_generic_task(
                request_context.clone(),
                task_index,
                task,
            ));
        }
    }
    match terminal_error {
        Some(error) => Err(error),
        None => Ok(summary),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_generic_task(
    total_tasks: usize,
    task_index: usize,
    task: PlannedTask,
    facts: &HashMap<GenericUnitKey, GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    terminology: &CompiledTerminology,
    language_module: &dyn LanguageModule,
    system_prompt: &str,
    response_envelope: TranslationResponseEnvelope,
    client: &crate::runtime::llm::OpenAiChatCompletionClient,
    llm: &OpenAiChatCompletionExecutor,
    retry_delays: &[Duration],
    max_retry_after: Duration,
    cancellation: &CooperativeCancellation,
    record_evidence: bool,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let user_message = render_generic_user_message(&task, terminology);
    let messages = [
        ChatMessage::new(ChatMessageRole::System, system_prompt),
        ChatMessage::new(ChatMessageRole::User, user_message),
    ];
    let record_started = record_evidence.then(|| (OffsetDateTime::now_utc(), Instant::now()));
    let record_messages = record_evidence.then(|| messages.to_vec());
    let expected_outputs = task.expected_output_ids().count();
    let execution = execute_llm_request_with_retry(
        llm,
        client,
        &messages,
        LlmRequestRetryPolicy::new(retry_delays, max_retry_after),
        &TokioDelay,
        cancellation,
        record_evidence,
    )
    .await;
    let (outcome, evidence) = execution.into_parts();
    let (attempt_count, attempts) = evidence.into_parts();
    let mut response_record = None;
    let outcome = match outcome {
        LlmRequestExecutionOutcome::Response { response, .. }
            if matches!(response.finish_reason(), LlmFinishReason::Stop) =>
        {
            match parse_translation_response(response.content(), response_envelope) {
                Ok(parsed) => {
                    let acceptance = accept_generic_response(
                        task,
                        &parsed,
                        facts,
                        placeholder_rules,
                        language_module,
                    );
                    let accepted_outputs = acceptance.accepted_output_count();
                    if record_evidence {
                        response_record = Some(GenericTaskResponseRecord::parsed(parsed));
                    }
                    GenericPreparedTaskOutcome::Accepted {
                        acceptance,
                        accepted_outputs,
                    }
                }
                Err(error) => {
                    if record_evidence {
                        response_record = Some(GenericTaskResponseRecord::invalid(
                            response.content().to_owned(),
                            error,
                        ));
                    }
                    GenericPreparedTaskOutcome::Unavailable {
                        reason: "model_response_unusable",
                    }
                }
            }
        }
        LlmRequestExecutionOutcome::Response { response, .. } => {
            if record_evidence {
                response_record = Some(GenericTaskResponseRecord::unprocessed(
                    response.content().to_owned(),
                ));
            }
            GenericPreparedTaskOutcome::Unavailable {
                reason: "non_stop_finish",
            }
        }
        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum { .. } => {
            GenericPreparedTaskOutcome::Unavailable {
                reason: "retry_after_exceeds_maximum",
            }
        }
        LlmRequestExecutionOutcome::RetryBudgetExhausted { .. } => {
            GenericPreparedTaskOutcome::Unavailable {
                reason: "recoverable_request_exhausted",
            }
        }
        LlmRequestExecutionOutcome::Fatal { cancelled, .. } => {
            if cancelled {
                GenericPreparedTaskOutcome::Cancelled
            } else {
                GenericPreparedTaskOutcome::Unavailable {
                    reason: "request_failed",
                }
            }
        }
        LlmRequestExecutionOutcome::Cancelled { .. } => GenericPreparedTaskOutcome::Cancelled,
    };
    let record = record_started.map(|(started_at, started)| GenericTaskRecordDraft {
        total_tasks,
        task_index,
        messages: record_messages.expect("启用记录时必须保留模型消息"),
        expected_outputs,
        started_at,
        duration: started.elapsed(),
        attempt_count,
        attempts,
        response: response_record,
    });
    Ok(GenericPreparedTask { outcome, record })
}

fn render_generic_user_message(task: &PlannedTask, terminology: &CompiledTerminology) -> String {
    let mut output = String::new();
    output.push_str("Groups:\n");
    for group in task.groups() {
        append_generic_group_message(&mut output, group, terminology, None);
        output.push('\n');
    }
    output
}

fn measure_generic_group_message(
    group: &PlannedGroup,
    terminology: &CompiledTerminology,
    first_output_id: u64,
) -> usize {
    let mut output = String::new();
    let mut next_output_id = first_output_id;
    append_generic_group_message(&mut output, group, terminology, Some(&mut next_output_id));
    output.push('\n');
    output.chars().count()
}

fn append_generic_group_message(
    output: &mut String,
    group: &PlannedGroup,
    terminology: &CompiledTerminology,
    mut next_output_id: Option<&mut u64>,
) {
    output.push_str("kind=");
    output.push_str(&serde_json::to_string(group.kind()).expect("受信 UTF-8 kind 必须可编码"));
    output.push('\n');
    if !group.terminology_indices().is_empty() {
        output.push_str("terminology:\n");
        for index in group.terminology_indices() {
            let entry = &terminology.entries()[*index];
            output
                .push_str(&serde_json::to_string(entry.term()).expect("受信 UTF-8 术语必须可编码"));
            output.push_str(" => ");
            output.push_str(
                &serde_json::to_string(entry.translation()).expect("受信 UTF-8 译词必须可编码"),
            );
            output.push('\n');
        }
    }
    output.push_str("units:\n");
    for unit in group.units() {
        let rendered_output_id = match (unit.output_id(), next_output_id.as_deref_mut()) {
            (Some(_), Some(next)) => {
                let current = *next;
                *next = next.saturating_add(1);
                Some(current)
            }
            (output_id, None) => output_id,
            (None, Some(_)) => None,
        };
        match rendered_output_id {
            Some(output_id) => {
                output.push('[');
                output.push_str(&output_id.to_string());
                output.push_str("] ");
            }
            None => output.push_str("[-] "),
        }
        output.push_str(&serde_json::to_string(unit.text()).expect("受信 UTF-8 text 必须可编码"));
        output.push('\n');
    }
}

fn accept_generic_response(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &HashMap<GenericUnitKey, GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> TranslationAcceptance {
    accept_generic_response_with(task, parsed, facts, |fact, candidate| {
        validate_generic_candidate_fact(fact, candidate, placeholder_rules, language_module)
    })
}

fn accept_generic_response_with(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &HashMap<GenericUnitKey, GenericValidationFact>,
    mut validator: impl FnMut(&GenericValidationFact, &str) -> Result<String, String>,
) -> TranslationAcceptance {
    let mut cache = HashMap::<u64, HashMap<String, Result<String, String>>>::new();
    accept_parsed_response(task, parsed, |output_id, key, candidate| {
        let Some(fact) = facts.get(key) else {
            return Err("响应代表项不属于当前 Generic 计划".to_owned());
        };
        let output_cache = cache.entry(output_id).or_default();
        if let Some(cached) = output_cache.get(&fact.kind) {
            return cached.clone();
        }

        // 一个 output_id 只对应一个全局去重族；同族的原文、保护后文本和实际
        // Placeholder 绑定相同。kind 仍会改变 scope，因此必须分别验收。
        let validated = validator(fact, candidate);
        output_cache.insert(fact.kind.clone(), validated.clone());
        validated
    })
}

fn validate_generic_candidate(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &HashMap<GenericUnitKey, GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<String, String> {
    let fact = facts
        .get(key)
        .ok_or_else(|| "响应代表项不属于当前 Generic 计划".to_owned())?;
    validate_generic_candidate_fact(fact, candidate, placeholder_rules, language_module)
}

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

fn add_commit_outcome(
    summary: &mut GenericTranslationSummary,
    outcome: &CommitTranslationsOutcome,
) {
    summary.written_units += outcome.committed;
    summary.conflicted_units += outcome.conflicts.len();
}

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
            .map_err(|source| GenericCommandError::operation("读取附加 PEM", source))?;
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

fn configure_generic_task_records(
    requested: bool,
    project_log: &GenericProjectLogSlot,
    file_system_configuration: &crate::runtime::filesystem::SystemFileSystemConfig,
    client: crate::llm::LlmClientRecordMetadata,
    locale: UiLocale,
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

async fn run_blocking<T, E>(
    stage: &'static str,
    operation: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
    E: Error + Send + Sync + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| GenericCommandError::operation(stage, source))?
        .map_err(|source| GenericCommandError::operation(stage, source))
}

enum Driven<T> {
    Finished(T),
    Interrupted(T),
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

async fn drive_and_shutdown(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    file_system: SystemFileSystem,
    mut shutdown_errors: Vec<GenericShutdownError>,
    project_log: GenericProjectLogSlot,
) -> GenericCommandRunReport {
    let driven = drive(operation, termination_signals, cancel).await;
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::new("filesystem", source));
    }
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
            Driven::SignalFailed { source, result } => {
                GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source,
                    operation: result.err().map(Box::new),
                })
            }
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
) -> Result<GenericCommandOutput, GenericCommandError> {
    let workspace_root = project.workspace_root().to_path_buf();
    let translated_units = candidate.translated_units();
    let retained_source_units = candidate.retained_source_units();
    let scratch_candidate = candidate;
    let scratch_workspace = workspace_root.clone();
    let scratch_root = run_blocking("建立 Generic 写回暂存来源", move || {
        materialize_write_back_source(&scratch_workspace, &scratch_candidate)
    })
    .await?;

    let target_root = project.write_back_root();
    let publish_intent = publish_intent_for(&target_root)
        .map_err(|source| GenericCommandError::operation("检查 Generic 写回目录", source))?;
    let mapping = DirectorySourceMapping::new(scratch_root.clone(), PathBuf::new())
        .map_err(|source| GenericCommandError::operation("建立 Generic 目录候选请求", source))?;
    let request = DirectoryStageRequest::new(
        target_root.clone(),
        publish_intent,
        vec![mapping],
        Vec::new(),
        Vec::new(),
    )
    .map_err(|source| GenericCommandError::operation("建立 Generic 目录候选请求", source))?;

    let staged = match publisher.prepare(request).await {
        Ok(staged) => staged,
        Err(source) => {
            let cleanup = cleanup_write_back_source(&workspace_root, &scratch_root);
            return match cleanup {
                Ok(()) => Err(GenericCommandError::operation(
                    "准备 Generic 写回候选",
                    source,
                )),
                Err(cleanup) => Err(GenericCommandError::PublishDiscard {
                    operation: Box::new(GenericCommandError::operation(
                        "准备 Generic 写回候选",
                        source,
                    )),
                    discard: cleanup.to_string(),
                    recovery_paths: vec![scratch_root.clone()],
                }),
            };
        }
    };

    if let Err(source) = cleanup_write_back_source(&workspace_root, &scratch_root) {
        let diagnostic = generic_operation_diagnostic("清理 Generic 写回暂存来源")
            .with_recovery(RecoveryFact::path(&scratch_root));
        let operation =
            GenericCommandError::diagnosed("清理 Generic 写回暂存来源", source, diagnostic);
        return discard_after_failure(&publisher, staged, operation).await;
    }

    if let Err(operation) = run_blocking("发布前复查 Generic 输入", move || {
        ensure_input_fingerprints_current(&project)
    })
    .await
    {
        return discard_after_failure(&publisher, staged, operation).await;
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
) -> Result<PathBuf, GenericScratchError> {
    materialize_write_back_source_with(workspace_root, candidate, |path, bytes| {
        fs::write(path, bytes)
    })
}

fn materialize_write_back_source_with(
    workspace_root: &Path,
    candidate: &GenericWriteBackCandidate,
    mut write_file: impl FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<PathBuf, GenericScratchError> {
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
        if let Err(source) = write_file(&target, file.bytes()) {
            let operation = GenericScratchError::Io {
                operation: "写入暂存 JSONL",
                path: target,
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
        let materialized_bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(source) => {
                let operation = GenericScratchError::Io {
                    operation: "读回暂存 JSONL",
                    path: target,
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
        };
        if let Err(source) = validate_materialized_write_back_file(file, materialized_bytes) {
            let operation = GenericScratchError::InvalidMaterializedFile {
                path: target,
                source: Box::new(source),
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
    Ok(scratch_root)
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
            Self::InvalidRelativePath(_)
            | Self::TargetNotDirectory(_)
            | Self::UnsafeCleanupTarget { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::generic::{
        GenericPlaceholderRuleDefinition, automatic_translation_state_fingerprint,
        manual_translation_state_fingerprint,
    };
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule,
    };
    use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

    use super::*;

    fn fingerprint(byte: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([byte; 32])
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
    fn model_message_contains_complete_group_context_without_stable_project_identity() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(source_root.join("nested")).expect("应该可建立输入目录");
        fs::write(
            source_root.join("nested/scene.jsonl"),
            concat!(
                r#"{"id":"secret-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-output","text":"こんにちは"},"#,
                r#"{"id":"secret-context","text":"魔王"}]}"#,
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
            .compile(Vec::new())
            .expect("空 Placeholder 规则应该合法");
        let resources = AutomaticStateResources {
            prompt: fingerprint(2),
            client_semantics: fingerprint(3),
            language_module: fingerprint(4),
            terminology_hits: empty_terminology_fingerprint(),
        };
        let group = snapshot.files()[0].groups()[0].clone();
        let unit = group.units()[0].clone();
        let protected = GenericPlaceholderService::default()
            .protect(group.kind(), unit.source_text(), &placeholder_rules)
            .expect("原文应该可保护");
        let state = automatic_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned()),
            unit.source_text(),
            group.context_fingerprint(),
            protected.binding_fingerprint(),
            AutomaticStateResources {
                terminology_hits: terminology_hit_fingerprint(terminology.as_ref(), &[0]),
                ..resources
            },
        );
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[crate::generic::TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "已有上下文".to_owned(),
                    origin: crate::generic::TranslationOrigin::Automatic,
                    state_fingerprint: state,
                    expected_translation: None,
                }],
            )
            .expect("应该可保存测试译文");
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
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
        )
        .expect("翻译任务应该可规划");
        let message = render_generic_user_message(&prepared.plan.tasks()[0], terminology.as_ref());

        assert!(message.contains("\"魔王\" => \"魔王（Demon King）\""));
        assert!(message.contains("kind=\"dialogue\""));
        assert!(message.contains("[-] \"已有上下文\""));
        assert!(message.contains("[1] \"魔王\""));
        for hidden_identity in [
            "secret-group",
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
    fn rendered_task_size_includes_terms_escaping_ids_and_keeps_oversized_group_whole() {
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
            .map(|task| {
                let message = render_generic_user_message(task, terminology.as_ref());
                let characters = message.chars().count();
                assert_eq!(task.estimated_characters(), characters);
                assert!(
                    characters <= target.get() || task.groups().len() == 1,
                    "超过目标字符数的 Task 只能包含一个不可拆 Group"
                );
                message
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("\\\""));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("[10]"));
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
        let current = collect_generic_current_translations(&stored, &terminology, &rules, None)
            .expect("人工 Current 应可独立计算");
        assert_eq!(
            current
                .get(&("group".to_owned(), "manual".to_owned()))
                .map(String::as_str),
            Some("人工译文")
        );
        assert!(!current.contains_key(&("group".to_owned(), "automatic".to_owned())));
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
        let candidate = build_write_back_candidate(&stored, &live, &HashMap::new())
            .expect("应该可建立写回候选");

        let result =
            materialize_write_back_source_with(&workspace_root, &candidate, |path, bytes| {
                fs::write(path, bytes)?;
                fs::write(
                    path,
                    concat!(
                        r#"{"id":"group","kind":"dialogue","units":["#,
                        r#"{"id":"unit","text":"落盘后被改写"}]}"#,
                        "\n"
                    ),
                )
            });

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
        let candidate = build_write_back_candidate(&stored, &live, &HashMap::new())
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

        let result = publish_generic_write_back(publisher, project_name, project, candidate).await;

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
        )
        .expect("同文应该合并为一个模型输出");
        assert_eq!(prepared.plan.tasks().len(), 1);
        let task = &prepared.plan.tasks()[0];
        assert_eq!(task.expected_output_ids().collect::<Vec<_>>(), [1]);
        let parsed = parse_translation_response(
            r#"{"1":"你好 {invented}"}"#,
            TranslationResponseEnvelope::JsonOnly,
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
                    output_id: 1,
                    key,
                    ..
                } if key.group_id() == "target" && key.unit_id() == "unit"
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
        )
        .expect("复用失败的目标应该改为请求模型");

        assert_eq!(prepared.plan.reused().len(), 1);
        assert_eq!(
            prepared.plan.reused()[0].key().group_id(),
            "source-2",
            "同 kind 目标仍应复用 Current"
        );
        assert_eq!(prepared.plan.tasks().len(), 1);
        assert_eq!(prepared.plan.tasks()[0].groups()[0].kind(), "name");
        assert_eq!(
            prepared.plan.tasks()[0]
                .expected_output_ids()
                .collect::<Vec<_>>(),
            [1],
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
        let facts = HashMap::from([(
            key.clone(),
            GenericValidationFact {
                kind: "dialogue".to_owned(),
                source_text: "こんにちは {name}".to_owned(),
                analysis: language_module.analyze_source(&language_text),
                protected,
            },
        )]);

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
