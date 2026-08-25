//! Generic JSONL 命令的生产装配。
//!
//! 本模块只编排 Generic 纵向流程。JSONL、动态 Extract、去重、译文状态和往返验证
//! 由 `generic` 领域负责；文件、CPU、LLM、租约和目录发布使用公共运行能力。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::FuturesOrdered;
use futures_util::{FutureExt, StreamExt};
use rayon::prelude::*;
use rusqlite::Connection;

use super::TranslationTerminalSummary;
use super::command::TerminationSignals;
use super::config::{
    ConfigurationLoadError, ConfiguredGenericCommand, ConfiguredGenericWriteBackCommand,
    ConfiguredManualCommand, ConfiguredProjectLuaCommand, ConfiguredTranslateCommand,
};
use super::project_log::{
    ActiveProjectLog, CommandLogStart, PendingProjectLog, ProjectLogHandle, ProjectLogLuaPrintSink,
    start_command_log,
};
use super::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError,
    assemble_translation_system_prompt_with_cancellation,
    ensure_no_prompt_template_variables_with_cancellation, parse_prompt_resource_with_cancellation,
    read_unparsed_prompt_resource, render_system_prompt_template_with_cancellation,
    translation_prompt_resource_paths,
};
use crate::diagnostic::{
    ByteRange, Diagnostic, DiagnosticReport, FileSystemDiagnosticContext,
    FileSystemDiagnosticStage, FileSystemIssue, FileSystemOperation, FileSystemPathViolation,
    FileSystemProblem, GenericDiagnosticStage, GenericIssue, GenericJsonErrorCategory,
    GenericLanguageProjectionProblem, GenericPlaceholderMultisetProblem, GenericProblem,
    GenericResponseDestinationProblem, GenericResponseReviewFinding,
    GenericTaskResponseJsonCategory, GenericTaskResponseProblem, GenericTaskUnavailableReason,
    GenericTranslationPreparationProblem, GenericUnitLocator as DiagnosticGenericUnitLocator,
    GenericWriteBackTextSide, GenericWriteBackUnitProblem, IoFailure, Pcre2Failure,
    Pcre2FailureKind, PlaceholderIssue,
    PlaceholderMatchRangeViolation as DiagnosticMatchRangeViolation,
    PlaceholderRuleOrigin as DiagnosticPlaceholderRuleOrigin,
    PlaceholderRuleSource as DiagnosticPlaceholderRuleSource,
    PlaceholderWorkerOperation as DiagnosticPlaceholderWorkerOperation, PublicationIssue,
    PublicationProblem, PublicationRequestViolation, PublicationStep, RelatedFailureRelation,
    ReportedFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation, SafeIdentifier, SafeIoKind,
    SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure, SqliteIssue,
    SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect, TranslationIssue,
    TranslationPlanningResourceOrigin, TranslationTaskPlanningProblem,
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
    AutomaticStateResources, CancellableTextMap, CommitTranslationResultsOutcome,
    CommitTranslationsOutcome, ExtractOutcome, GenericCompiledPlaceholderRules,
    GenericCurrentTranslation, GenericInitRequest, GenericPlaceholderService, GenericPlanningError,
    GenericPlanningUnitLocator, GenericProject, GenericProjectError, GenericProjectStore,
    GenericProtectedText, GenericStoredSnapshot, GenericTaskRecordDocument, GenericTaskRecordState,
    GenericUnitKey, GenericUnitMap, GenericWriteBackCandidate, GenericWriteBackError,
    GenericWriteBackTextOptions, PlannedTask, PlanningUnit, RejectedTranslationWrite,
    ResponseProblem, TranslationAcceptance, TranslationOrigin, TranslationPlan, TranslationReview,
    TranslationWrite, ValidatedReuse, accept_parsed_response_with_cancellation,
    build_write_back_candidate_with_cancellation, compile_generic_layout_rules,
    current_translation_for_stored_with_cancellation,
    ensure_input_fingerprints_current_with_cancellation,
    plan_translation_with_validator_and_cancellation,
    terminology_hit_fingerprint_with_cancellation,
    validate_materialized_write_back_file_with_cancellation,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::language::{
    LanguageAnalysis, LanguageId, LanguageModule, LanguageModuleCatalogError,
    LanguageOperationCancelled, LanguageText, LanguageTextSegment,
};
use crate::llm::ApiKeyRedactor;
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity, LlmFinishReason,
    LlmRequestFailure,
};
use crate::manual::{ManualCommandError, ManualCommandSummary, execute_generic_manual_command};
#[cfg(not(test))]
use crate::progress::ProgressObserver;
use crate::progress::{
    ProgressSnapshot, TerminalProgress, TerminalProgressFailures, TerminalProgressObserver,
};
use crate::project_lease::{
    ProjectCommandLeaseError, ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::project_lua::{
    ProjectLuaCancellation, ProjectLuaEngine, ProjectLuaFailure, ProjectLuaProgram,
    ProjectLuaProject, ProjectLuaRunError, ProjectLuaRunRequest,
    compile_project_lua_program_with_cancellation, generic_project_lua_adapter_for_name,
    run_project_lua,
};
use crate::project_name::ProjectName;
use crate::runtime::cpu::{
    CpuExecutorShutdownError, CpuExecutorStartError, CpuExecutorUnavailable, RayonCpuExecutor,
};
use crate::runtime::filesystem::{
    SystemDirectoryPublisher, SystemFileSystem, SystemFileSystemBuildError, SystemFileSystemError,
};
use crate::runtime::llm::OpenAiCompatibleExecutor;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticScope, GenericPublicationSummary as ProjectLogGenericPublicationSummary,
    GenericTranslationSummary as ProjectLogGenericTranslationSummary, PhaseStopOutcome,
    ProjectLogAmount, ProjectLogCommand, ProjectLogEngine, ProjectLogEvent, ProjectLogPhase,
    PublicationFinished, PublicationSummary, ResolvedRunPlan, RunPlanFinalization,
    RunPlanTransactionState, RunPlanValueSource, TaskFinishedOutcome, TaskPosition,
    TranslationEngineSummary, TranslationFinished, TranslationTaskCounters,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublicationDiagnosticSource,
    DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, FileReader, ReadFileError, RecoverableDirectoryPublisher,
};
use crate::translation::candidate_validation::{
    ProvenInvariantViolation, ReviewFinding, ValidatedCandidate,
    validate_reflowed_candidate_text_with_cancellation,
};
use crate::translation::layout_rules::{LayoutRuleSet, LayoutRulesError};
use crate::translation::placeholder::{
    PlaceholderMatchRangeViolation, PlaceholderPcre2ErrorKind, PlaceholderProtectionError,
    PlaceholderRestoreError, PlaceholderRuleOrigin, PlaceholderWorkerOperation,
};
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderMultisetError,
};
use crate::translation::placeholder_token;
use crate::translation::planning_resource::{
    CompiledTerminology, TranslationPlanningResourceReader,
    TranslationPlanningResourceReadingError, TranslationPlanningResourceReadingService,
};
#[cfg(test)]
use crate::translation::planning_resource::{
    PlaceholderDefinitionError, TerminologyDefinitionError,
};
use crate::translation::task_planning::{TaskId, TaskPlanningError};
use crate::translation::task_record::{
    ConfiguredTranslationTaskRecordSink, MarkdownTranslationTaskRecordSink,
    TaskRecordDiagnosticRecorder,
};
use crate::translation::user_message::{
    TranslationReturnType, TranslationUserGroup, TranslationUserMessage,
    TranslationUserTerminology, TranslationUserUnit, render_translation_user_message,
};
#[cfg(test)]
use crate::translation_protocol::parse_translation_response;
use crate::translation_protocol::{
    ParsedTranslationResponse, TranslationResponseMode, TranslationTaskResponseJsonErrorCategory,
    TranslationTaskResponseParseError, TranslationTaskResponseParseErrorKind,
    parse_translation_response_with_cancellation,
};

const GENERIC_ENGINE_NAME: &str = "generic";
const WRITE_BACK_SCRATCH_NAME: &str = ".write_back.tmp";
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
                .map(GenericShutdownError::terminal_progress),
        );
    }
}

fn generic_terminal_progress(locale: UiLocale) -> GenericTerminalProgress {
    let localizer = UiLocalizer::new(locale);
    let initializing = localizer.format(UiMessage::ProgressGenericInit);
    let extracting = localizer.format(UiMessage::ProgressGenericExtract);
    let planning = localizer.format(UiMessage::ProgressTranslatePlanning);
    let confirmed = localizer.format(UiMessage::ProgressTranslateConfirmed);
    let lua = localizer.format(UiMessage::ProgressProjectLua);
    let preparing_write_back = localizer.format(UiMessage::ProgressWriteBackPlanning);
    let publishing_write_back = localizer.format(UiMessage::ProgressWriteBackPublish);
    let no_progress_work = localizer.format(UiMessage::ProgressNoWork);
    let terminal = TerminalProgress::stderr(
        move |phase| match phase {
            GenericProgressPhase::Initializing => initializing.clone(),
            GenericProgressPhase::Extracting => extracting.clone(),
            GenericProgressPhase::PlanningTranslation => planning.clone(),
            GenericProgressPhase::ConfirmedTasks => confirmed.clone(),
            GenericProgressPhase::RunningLua => lua.clone(),
            GenericProgressPhase::PreparingWriteBack => preparing_write_back.clone(),
            GenericProgressPhase::PublishingWriteBack => publishing_write_back.clone(),
        },
        no_progress_work,
    );
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
    Manual {
        summary: ManualCommandSummary,
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
    pub(crate) started_tasks: usize,
    pub(crate) not_started_tasks: usize,
    pub(crate) complete_tasks: usize,
    pub(crate) partial_tasks: usize,
    pub(crate) unavailable_tasks: usize,
    pub(crate) planned_units: usize,
    pub(crate) remaining_units: usize,
    pub(crate) cleared_units: usize,
    pub(crate) reused_units: usize,
    pub(crate) accepted_units: usize,
    pub(crate) written_units: usize,
    pub(crate) conflicted_units: usize,
    pub(crate) response_problems: usize,
    pub(crate) recoverable_request_exhaustions: usize,
    pub(crate) request_admission_stopped: bool,
}

impl GenericTranslationSummary {
    /// Task 协议问题和 Unit 写入冲突都表示项目仍有未完成内容。
    pub(crate) const fn is_incomplete(self) -> bool {
        self.partial_tasks > 0
            || self.unavailable_tasks > 0
            || self.not_started_tasks > 0
            || self.remaining_units > 0
            || self.conflicted_units > 0
            || self.response_problems > 0
    }
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
    pub(crate) panic_log_path: Option<PathBuf>,
    pub(crate) selected_api_key_redactor: Option<Arc<ApiKeyRedactor>>,
    pub(crate) translation_summary: Option<TranslationTerminalSummary>,
}

/// 一个运行根在业务 future 完成后关闭失败。
#[derive(Debug)]
pub(crate) struct GenericShutdownError {
    component: &'static str,
    failure: ReportedFailure,
}

impl GenericShutdownError {
    fn new(
        component: &'static str,
        source: impl Error + Send + Sync + 'static,
        report: DiagnosticReport,
    ) -> Self {
        Self {
            component,
            failure: ReportedFailure::new(report, source),
        }
    }

    fn cpu(source: CpuExecutorShutdownError) -> Self {
        let report =
            DiagnosticReport::new(StateEffect::AppliedFinalizationFailed, source.diagnostic());
        Self::new("CPU executor", source, report)
    }

    fn file_system(source: SystemFileSystemError) -> Self {
        let report = source.shutdown_diagnostic_report();
        Self::new("filesystem", source, report)
    }

    fn terminal_progress(source: crate::progress::TerminalProgressFailure) -> Self {
        let report = source.diagnostic_report();
        Self::new("terminal progress", source, report)
    }

    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        self.failure.report().clone()
    }
}

impl fmt::Display for GenericShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} 关闭失败：{}", self.component, self.failure)
    }
}

impl Error for GenericShutdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure.source_error())
    }
}

/// 候选目录或 Generic scratch 清理的类型化失败。
#[derive(Debug)]
pub(crate) struct GenericDiscardFailure {
    failure: ReportedFailure,
}

impl GenericDiscardFailure {
    fn new(report: DiagnosticReport, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            failure: ReportedFailure::new(report, source),
        }
    }

    fn diagnostic_report(&self) -> DiagnosticReport {
        self.failure.report().clone()
    }

    #[cfg(test)]
    fn source_error(&self) -> &(dyn Error + 'static) {
        self.failure.source_error()
    }
}

impl fmt::Display for GenericDiscardFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl Error for GenericDiscardFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure.source_error())
    }
}

/// Generic 命令仍掌握完整阶段时建立的具体失败。
#[derive(Debug)]
pub(crate) enum GenericCommandError {
    Cancelled,
    Operation {
        failure: ReportedFailure,
    },
    Signal {
        source: io::Error,
        operation: Option<Box<GenericCommandError>>,
        state_applied: bool,
    },
    PublishDiscard {
        operation: Box<GenericCommandError>,
        discard: GenericDiscardFailure,
    },
}

impl GenericCommandError {
    fn reported(source: impl Error + Send + Sync + 'static, report: DiagnosticReport) -> Self {
        Self::Operation {
            failure: ReportedFailure::new(report, source),
        }
    }

    fn configuration(source: ConfigurationLoadError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::reported(source, report)
    }

    fn missing_profile_id() -> Self {
        Self::reported(
            MissingGenericProfileId,
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::Translate,
                    GenericProblem::MissingProfileId,
                )),
            ),
        )
    }

    fn language_module(source: LanguageModuleCatalogError, target_language: &LanguageId) -> Self {
        let LanguageModuleCatalogError::UnknownLanguageId {
            language_id,
            available_ids,
        } = &source;
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::LanguageModuleUnavailable {
                requested_language: SafeIdentifier::from_validated(language_id.as_str()),
                target_language: SafeIdentifier::from_validated(target_language.as_str()),
                available_languages: available_ids
                    .iter()
                    .map(|language| SafeIdentifier::from_validated(language.as_str()))
                    .collect(),
            }),
        );
        Self::reported(source, report)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    fn is_application_scope_panic(&self) -> bool {
        match self {
            Self::Operation { failure } => failure
                .source_error()
                .is::<GenericApplicationScopePanicked>(),
            Self::Signal {
                operation: Some(operation),
                ..
            }
            | Self::PublishDiscard { operation, .. } => operation.is_application_scope_panic(),
            Self::Cancelled
            | Self::Signal {
                operation: None, ..
            } => false,
        }
    }
}

impl fmt::Display for GenericCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 命令已取消"),
            Self::Operation { failure } => failure.fmt(formatter),
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
            Self::Operation { failure } => Some(failure.source_error()),
            Self::Signal { source, .. } => Some(source),
            Self::PublishDiscard { operation, .. } => Some(operation.as_ref()),
            Self::Cancelled => None,
        }
    }
}

impl GenericCommandError {
    pub(crate) fn manual_error(&self) -> Option<&ManualCommandError> {
        match self {
            Self::Operation { failure } => {
                failure.source_error().downcast_ref::<ManualCommandError>()
            }
            Self::Cancelled | Self::Signal { .. } | Self::PublishDiscard { .. } => None,
        }
    }
}

pub(crate) fn generic_command_error_report(error: &GenericCommandError) -> DiagnosticReport {
    match error {
        GenericCommandError::Cancelled => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericCommandError::Operation { failure } => failure.report().clone(),
        GenericCommandError::Signal {
            source,
            operation,
            state_applied,
        } => {
            let effect = if *state_applied {
                StateEffect::AppliedFinalizationFailed
            } else {
                operation
                    .as_ref()
                    .map_or(StateEffect::Unchanged, |operation| {
                        generic_command_error_report(operation).effect()
                    })
            };
            let mut report = DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TerminationSignals,
                    operation: RuntimeOperation::ReceiveTerminationSignal,
                    failure: IoFailure::from_error(source),
                }),
            );
            if let Some(operation) = operation {
                report = report.with_related(
                    RelatedFailureRelation::Finalization,
                    generic_command_error_report(operation),
                );
            }
            report
        }
        GenericCommandError::PublishDiscard { operation, discard } => {
            generic_command_error_report(operation)
                .with_related(RelatedFailureRelation::Discard, discard.diagnostic_report())
        }
    }
}

fn generic_read_file_report(
    source: &ReadFileError<SystemFileSystemError>,
    stage: FileSystemDiagnosticStage,
) -> DiagnosticReport {
    let context = FileSystemDiagnosticContext::new(stage, FileSystemOperation::Read);
    match source {
        ReadFileError::NotFound { path } => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::file_system(FileSystemIssue::new(
                context,
                FileSystemProblem::NotFound {
                    path: SafePath::new(path),
                },
            )),
        ),
        ReadFileError::NotFile { path } => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::file_system(FileSystemIssue::new(
                context,
                FileSystemProblem::NotFile {
                    path: SafePath::new(path),
                },
            )),
        ),
        ReadFileError::Io { source, .. } => {
            source.diagnostic_report(context, StateEffect::Unchanged)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MissingGenericProfileId;

impl fmt::Display for MissingGenericProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("首次 Generic Translate 必须显式提供 Profile ID")
    }
}

impl Error for MissingGenericProfileId {}

fn generic_blocking_join_failure(
    source: tokio::task::JoinError,
    effect: StateEffect,
) -> GenericCommandError {
    let issue = if source.is_panic() {
        RuntimeIssue::WorkerPanicked {
            component: RuntimeComponent::TokioRuntime,
            operation: RuntimeOperation::ExecuteTask,
        }
    } else {
        RuntimeIssue::ExecutorClosed {
            component: RuntimeComponent::TokioRuntime,
            operation: RuntimeOperation::ExecuteTask,
        }
    };
    GenericCommandError::reported(
        source,
        DiagnosticReport::new(effect, Diagnostic::runtime(issue)),
    )
}

#[derive(Clone, Debug)]
struct GenericCommandPanicContext {
    command: crate::diagnostic::RuntimeCommand,
    project_workspace: PathBuf,
    panic_log_path: Arc<Mutex<Option<PathBuf>>>,
    selected_api_key_redactor: Arc<Mutex<Option<Arc<ApiKeyRedactor>>>>,
}

impl GenericCommandPanicContext {
    fn new(command: crate::diagnostic::RuntimeCommand, project_workspace: PathBuf) -> Self {
        Self {
            command,
            project_workspace,
            panic_log_path: Arc::new(Mutex::new(None)),
            selected_api_key_redactor: Arc::new(Mutex::new(None)),
        }
    }

    fn observe_project_log(&self, project_log: &ActiveProjectLog) {
        let Some(path) = project_log.established_log_path().map(Path::to_path_buf) else {
            return;
        };
        *self
            .panic_log_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }

    fn observe_project_log_slot(&self, slot: &GenericProjectLogSlot) {
        if let Some(project_log) = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            self.observe_project_log(project_log);
        }
    }

    fn log_path(&self) -> Option<PathBuf> {
        self.panic_log_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn observe_selected_api_key_redactor(&self, redactor: Arc<ApiKeyRedactor>) {
        let mut selected = self
            .selected_api_key_redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = &*selected {
            assert!(
                Arc::ptr_eq(current, &redactor),
                "一次 Generic Translate 运行不能改选另一个 API key 替换器"
            );
        } else {
            *selected = Some(redactor);
        }
    }

    fn selected_api_key_redactor(&self) -> Option<Arc<ApiKeyRedactor>> {
        self.selected_api_key_redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Debug)]
struct GenericApplicationScopePanicked;

impl fmt::Display for GenericApplicationScopePanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("application scope panicked")
    }
}

impl Error for GenericApplicationScopePanicked {}

fn generic_command_panic_context(
    configured: &ConfiguredGenericCommand,
) -> GenericCommandPanicContext {
    let (command, projects_root, project_name, selected_api_key_redactor) = match configured {
        ConfiguredGenericCommand::Init { arguments, common } => (
            crate::diagnostic::RuntimeCommand::Init,
            common.projects_root(),
            arguments.project.name.as_str(),
            None,
        ),
        ConfiguredGenericCommand::Extract {
            project_name,
            common,
        } => (
            crate::diagnostic::RuntimeCommand::Extract,
            common.projects_root(),
            project_name.as_str(),
            None,
        ),
        ConfiguredGenericCommand::Translate(command) => {
            let redactor = command
                .resolved_profile_id()
                .map(|_| command.translation().client().api_key_redactor());
            (
                crate::diagnostic::RuntimeCommand::Translate,
                command.common().projects_root(),
                command.project_name().as_str(),
                redactor,
            )
        }
        ConfiguredGenericCommand::WriteBack(command) => (
            crate::diagnostic::RuntimeCommand::WriteBack,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Manual(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Translation(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Lua(command) => (
            crate::diagnostic::RuntimeCommand::Lua,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
    };
    let context = GenericCommandPanicContext::new(
        command,
        projects_root.join(GENERIC_ENGINE_NAME).join(project_name),
    );
    if let Some(redactor) = selected_api_key_redactor {
        context.observe_selected_api_key_redactor(redactor);
    }
    context
}

fn generic_command_panic_error(context: &GenericCommandPanicContext) -> GenericCommandError {
    let report = DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: crate::diagnostic::RuntimeEngine::Generic,
            command: context.command,
            project_workspace: SafePath::new(&context.project_workspace),
            log_path: context.log_path().map(SafePath::new),
        }),
    );
    GenericCommandError::reported(GenericApplicationScopePanicked, report)
}

fn generic_translate_panic_error(context: &GenericCommandPanicContext) -> GenericCommandError {
    let report = DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: crate::diagnostic::RuntimeEngine::Generic,
            command: context.command,
            project_workspace: SafePath::new(&context.project_workspace),
            log_path: context.log_path().map(SafePath::new),
        }),
    );
    GenericCommandError::reported(GenericApplicationScopePanicked, report)
}

async fn catch_generic_command_panic(
    context: GenericCommandPanicContext,
    future: impl Future<Output = GenericCommandRunReport>,
) -> GenericCommandRunReport {
    let mut report = match AssertUnwindSafe(future).catch_unwind().await {
        Ok(report) => report,
        Err(payload) => {
            // panic payload 可能包含模型正文、Lua、SQL 或用户文本；只丢弃，绝不读取。
            drop(payload);
            let panic_log_path = context.log_path();
            GenericCommandRunReport::panicked(generic_command_panic_error(&context), panic_log_path)
        }
    };
    report.selected_api_key_redactor = context.selected_api_key_redactor().or_else(|| {
        report
            .pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::selected_api_key_redactor)
    });
    report
}

/// Generic 的生产命令执行器。
pub(crate) struct ProductionGenericCommandRunner {
    locale: UiLocale,
    panic_context: Option<GenericCommandPanicContext>,
}

impl ProductionGenericCommandRunner {
    pub(crate) const fn new(locale: UiLocale) -> Self {
        Self {
            locale,
            panic_context: None,
        }
    }

    fn panic_context(&self) -> &GenericCommandPanicContext {
        self.panic_context
            .as_ref()
            .expect("Generic 命令进入生产执行前必须建立 panic 上下文")
    }

    pub(crate) async fn run(
        mut self,
        command: ConfiguredGenericCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let panic_context = generic_command_panic_context(&command);
        self.panic_context = Some(panic_context.clone());
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
                        return GenericCommandRunReport::failed(generic_file_system_build_failure(
                            source,
                        ));
                    }
                };
                let project_log = generic_project_log_slot();
                let cancellation = CooperativeCancellation::default();
                let progress = generic_terminal_progress(self.locale);
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
                let operation_panic_context = self.panic_context().clone();
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
                    let database_path = request.workspace_root.join("project.db");
                    let init_cancellation = operation_cancellation.clone();
                    let (_, project) = run_project_blocking(
                        GenericDiagnosticStage::Init,
                        StateEffect::Unchanged,
                        database_path,
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
                            engine: ProjectLogEngine::Generic,
                            project: project.project_name().as_str(),
                            command: ProjectLogCommand::Init,
                            performance,
                            selected_api_key_redactor: None,
                        }),
                    );
                    operation_panic_context.observe_project_log_slot(&operation_project_log);
                    if let Some(handle) = generic_project_log_handle(&operation_project_log) {
                        handle.emit(ProjectLogEvent::RunPlanResolved {
                            plan: ResolvedRunPlan::init(
                                RunPlanValueSource::Explicit,
                                project.source_root(),
                            ),
                        });
                        handle.emit(ProjectLogEvent::RunPlanFinalized {
                            database: crate::diagnostic::SafePath::new(project.database_path()),
                            result: RunPlanFinalization::Saved {
                                transaction: RunPlanTransactionState::Committed,
                                run_continues: false,
                            },
                        });
                    }
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
                let extract_project_log = generic_extract_project_log_state();
                start_existing_generic_project_log(
                    &project_log,
                    &common,
                    self.locale,
                    &project_name,
                    ProjectLogCommand::Extract,
                    Arc::clone(&performance),
                );
                self.panic_context().observe_project_log_slot(&project_log);
                let file_system = match start_file_system(
                    common.filesystem().clone(),
                    Arc::clone(&performance),
                ) {
                    Ok(file_system) => file_system,
                    Err(source) => {
                        return GenericCommandRunReport::from_driven(
                            Driven::Finished(Err(generic_file_system_build_failure(source))),
                            Vec::new(),
                            take_generic_project_log(&project_log),
                        );
                    }
                };
                let cancellation = CooperativeCancellation::default();
                let progress = generic_terminal_progress(self.locale);
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
                let operation_project_log = Arc::clone(&project_log);
                let operation_extract_project_log = Arc::clone(&extract_project_log);
                let operation = async move {
                    ensure_generic_operation_running(&operation_cancellation)?;
                    start_generic_extract_project_log(
                        &operation_project_log,
                        &operation_extract_project_log,
                    );
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::Extracting,
                    ));
                    let _lease = lease_provider
                        .acquire(&project_name)
                        .await
                        .map_err(generic_project_lease_failure)?;
                    ensure_generic_operation_running(&operation_cancellation)?;
                    let database_path = store.database_path().to_path_buf();
                    let open_store = store.clone();
                    let project = run_project_blocking(
                        GenericDiagnosticStage::ProjectOpening,
                        StateEffect::Unchanged,
                        database_path.clone(),
                        move || open_store.open(),
                    )
                    .await?;
                    resolve_generic_extract_run_plan(
                        &operation_project_log,
                        &operation_extract_project_log,
                        project.database_path(),
                    );
                    let outcome = run_project_blocking(
                        GenericDiagnosticStage::Extract,
                        StateEffect::ProgressPreserved,
                        database_path,
                        move || store.extract(),
                    )
                    .await?;
                    Ok(GenericCommandOutput::Extract {
                        project: output_name,
                        outcome,
                    })
                };
                drive_extract_and_shutdown(
                    operation,
                    termination_signals,
                    move || {
                        cancellation.request();
                        cancellation_file_system.cancel_waits();
                    },
                    file_system,
                    Vec::new(),
                    project_log,
                    extract_project_log,
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
            ConfiguredGenericCommand::Manual(command) => {
                self.run_manual(command, termination_signals).await
            }
            ConfiguredGenericCommand::Translation(command) => {
                self.run_manual(command, termination_signals).await
            }
            ConfiguredGenericCommand::Lua(command) => {
                self.run_lua(command, termination_signals).await
            }
        }
    }

    async fn run_manual(
        self,
        command: ConfiguredManualCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let file_system =
            match start_file_system(command.common().filesystem().clone(), performance) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::failed(generic_file_system_build_failure(
                        source,
                    ));
                }
            };
        let cancellation = CooperativeCancellation::default();
        let progress = generic_terminal_progress(self.locale);
        let project_log = generic_project_log_slot();
        let project = command.project_name().clone();
        let database_path =
            generic_workspace(command.common().projects_root(), &project).join("project.db");
        let operation = command.operation();
        let file = command.file().to_path_buf();
        let export_selection = command.export_selection().cloned();
        let language_modules = command.language_modules().cloned();
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            GENERIC_ENGINE_NAME,
            file_system.clone(),
        );
        let operation_project = project.clone();
        let operation_cancellation = cancellation.clone();
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            let _lease = lease_provider
                .acquire(&operation_project)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let blocking_cancellation = operation_cancellation.clone();
            let summary = tokio::task::spawn_blocking(move || {
                execute_generic_manual_command(
                    &database_path,
                    operation,
                    &file,
                    export_selection.as_ref(),
                    language_modules.as_ref(),
                    &blocking_cancellation,
                )
            })
            .await
            .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?
            .map_err(generic_manual_failure)?;
            Ok(GenericCommandOutput::Manual { summary })
        };
        let cancellation_file_system = file_system.clone();
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

    async fn run_lua(
        self,
        command: ConfiguredProjectLuaCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let language_modules = command.language_modules().clone();
        let project_log = generic_project_log_slot();
        install_generic_project_log(
            &project_log,
            start_command_log(CommandLogStart {
                common: command.common(),
                locale: self.locale,
                engine: ProjectLogEngine::Generic,
                project: project_name.as_str(),
                command: ProjectLogCommand::Lua,
                performance: Arc::clone(&performance),
                selected_api_key_redactor: None,
            }),
        );
        self.panic_context().observe_project_log_slot(&project_log);
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::from_driven(
                        Driven::Finished(Err(generic_file_system_build_failure(source))),
                        Vec::new(),
                        take_generic_project_log(&project_log),
                    );
                }
            };
        let cancellation = CooperativeCancellation::default();
        let lua_cancellation = ProjectLuaCancellation::default();
        let progress = generic_terminal_progress(self.locale);
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
        let print_sink = {
            let project_log = project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            project_log.as_ref().map(|project_log| {
                Arc::new(ProjectLogLuaPrintSink::from_active(project_log))
                    as Arc<dyn crate::project_lua::ProjectLuaPrintSink>
            })
        };
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            let script = operation_file_system
                .read_file(script_path.clone())
                .await
                .map_err(|source| {
                    generic_read_file_failure(source, FileSystemDiagnosticStage::CommandPreparation)
                })?;
            let identity = script.resolved_path().to_string_lossy().into_owned();
            let source = script.into_bytes();
            let preflight_database_path = store.database_path().to_path_buf();
            let preflight_cancellation = operation_lua_cancellation.clone();
            let preparation = tokio::task::spawn_blocking(move || {
                let program = ProjectLuaProgram::new(identity, source, arguments);
                compile_project_lua_program_with_cancellation(&program, &preflight_cancellation)?;
                Ok::<_, ProjectLuaFailure>(program)
            })
            .await;
            let preparation = preparation
                .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?;
            let program = match preparation {
                Ok(prepared) => prepared,
                Err(ProjectLuaFailure::Cancelled) => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let report = source.preflight_diagnostic_report(&preflight_database_path);
                    let source = GenericLuaPreflightError(source);
                    return Err(GenericCommandError::reported(source, report));
                }
            };
            ensure_generic_operation_running(&operation_cancellation)?;

            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let database_path = store.database_path().to_path_buf();
            let lua_project_name = output_name.as_str().to_owned();
            let lua_adapter =
                generic_project_lua_adapter_for_name(lua_project_name.clone(), language_modules);
            let request = ProjectLuaRunRequest::new(
                ProjectLuaProject::new(lua_project_name, ProjectLuaEngine::Generic),
                program,
                lua_adapter,
            )
            .with_cancellation(operation_lua_cancellation);
            let request = match print_sink {
                Some(print_sink) => request.with_print_sink(print_sink),
                None => request,
            };
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::RunningLua,
            ));
            let diagnostic_database_path = database_path.clone();
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
            let execution = execution.map_err(|source| {
                generic_blocking_join_failure(source, StateEffect::OutcomeUnknown)
            })?;
            match execution {
                Ok(_) => {}
                Err(source) if source.is_cancelled() => return Err(GenericCommandError::Cancelled),
                Err(source) => {
                    let report = source.diagnostic_report(&diagnostic_database_path);
                    return Err(GenericCommandError::reported(source, report));
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
        let translate_project_log = generic_translate_project_log_state();
        start_existing_generic_project_log(
            &project_log,
            command.common(),
            self.locale,
            &project_name,
            ProjectLogCommand::Translate,
            Arc::clone(&performance),
        );
        if let Some(redactor) = self.panic_context().selected_api_key_redactor() {
            select_generic_project_log_api_key_redactor(&project_log, redactor);
        }
        self.panic_context().observe_project_log_slot(&project_log);
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration.clone(), Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    let driven = Driven::Finished(Err(generic_file_system_build_failure(source)));
                    let terminal_occurrence = finish_generic_translate_project_log(
                        &project_log,
                        &translate_project_log,
                        &driven,
                    );
                    return GenericCommandRunReport::from_driven_with_terminal_occurrence(
                        driven,
                        Vec::new(),
                        take_generic_project_log(&project_log),
                        terminal_occurrence,
                    );
                }
            };
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let error = generic_cpu_start_failure(source);
                let mut shutdown_errors = Vec::new();
                if let Err(source) = file_system.shutdown().await {
                    shutdown_errors.push(GenericShutdownError::file_system(source));
                }
                let driven = Driven::Finished(Err(error));
                let terminal_occurrence = finish_generic_translate_project_log(
                    &project_log,
                    &translate_project_log,
                    &driven,
                );
                return GenericCommandRunReport::from_driven_with_terminal_occurrence(
                    driven,
                    shutdown_errors,
                    take_generic_project_log(&project_log),
                    terminal_occurrence,
                );
            }
        };
        let cancellation = CooperativeCancellation::default();
        let progress = generic_terminal_progress(self.locale);
        let operation_progress = progress.observer();
        let llm_holder = Arc::new(Mutex::new(None::<OpenAiCompatibleExecutor>));
        let task_record_holder = Arc::new(Mutex::new(None::<ConfiguredTranslationTaskRecordSink>));
        let operation_panic_context = self.panic_context().clone();
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
        let operation_task_record_holder = Arc::clone(&task_record_holder);
        let output_name = project_name.clone();
        let locale = self.locale;
        let operation_project_log = Arc::clone(&project_log);
        let operation_translate_project_log = Arc::clone(&translate_project_log);
        let selection_panic_context = operation_panic_context.clone();
        let operation = async move {
            ensure_generic_operation_running(&operation_cancellation)?;
            start_generic_translate_phase(
                &operation_project_log,
                &operation_translate_project_log,
                ProjectLogPhase::Planning,
                ProjectLogAmount::Indeterminate,
            );
            operation_progress.observe(ProgressSnapshot::indeterminate(
                GenericProgressPhase::PlanningTranslation,
            ));
            let _lease = lease_provider
                .acquire(&project_name)
                .await
                .map_err(generic_project_lease_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;

            let database_path = store.database_path().to_path_buf();
            let initial_store = store.clone();
            let (snapshot, _live, current_resources) = run_project_blocking(
                GenericDiagnosticStage::Translate,
                StateEffect::ProgressPreserved,
                database_path.clone(),
                move || initial_store.load_current_translation_state(),
            )
            .await?;
            let project = snapshot.project().clone();
            let profile_source = if command.resolved_profile_id().is_some() {
                RunPlanValueSource::Explicit
            } else {
                RunPlanValueSource::ProjectState
            };
            let profile_id = command
                .resolved_profile_id()
                .map(str::to_owned)
                .or_else(|| project.last_profile_id().map(str::to_owned))
                .ok_or_else(GenericCommandError::missing_profile_id)?;
            let command = command
                .resolve_profile(&profile_id)
                .map_err(GenericCommandError::configuration)?;
            let configuration = command.translation();
            let selected_api_key_redactor = configuration.client().api_key_redactor();
            selection_panic_context
                .observe_selected_api_key_redactor(Arc::clone(&selected_api_key_redactor));
            select_generic_project_log_api_key_redactor(
                &operation_project_log,
                selected_api_key_redactor,
            );
            let source_language = configuration
                .language_modules()
                .resolve(project.language_pair().source())
                .map_err(|source| {
                    GenericCommandError::language_module(source, project.language_pair().target())
                })?;

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
                    .map_err(generic_cpu_execution_failure)?
                    .map_err(generic_preparation_failure)?;
            let terminology_path = command.terminology_path().map(Path::to_path_buf);
            let placeholder_rules_path = command.placeholder_rules_path().map(Path::to_path_buf);
            resolve_generic_translate_run_plan(
                &operation_project_log,
                &operation_translate_project_log,
                project.database_path(),
                profile_source,
                &profile_id,
                terminology_path.as_deref(),
                placeholder_rules_path.as_deref(),
            );
            let placeholder_rule_source = placeholder_rules_path
                .as_ref()
                .map_or(GenericPlaceholderRuleSource::ProjectSnapshot, |path| {
                    GenericPlaceholderRuleSource::ExternalFile(path.clone())
                });
            let (terminology, _placeholder_rules, terminology_json, placeholder_json) =
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
                    let placeholder_compile_source = placeholder_rule_source.clone();
                    let placeholder_rules = operation_cpu
                        .execute(move || {
                            GenericPlaceholderService::default()
                                .compile_resource_with_cancellation(
                                    placeholder_definitions,
                                    || {
                                        ensure_generic_cpu_running(
                                            &placeholder_compile_cancellation,
                                        )
                                    },
                                )?
                                .map_err(|source| GenericPreparationError::Placeholder {
                                    rule_source: placeholder_compile_source,
                                    source,
                                })
                        })
                        .await
                        .map_err(generic_cpu_execution_failure)?
                        .map_err(generic_preparation_failure)?;
                    (
                        terminology,
                        placeholder_rules,
                        terminology_json,
                        placeholder_json,
                    )
                };

            let valid_placeholder_ids = snapshot.natural_unit_ids();
            let strict_placeholder_json = placeholder_json.clone();
            let strict_placeholder_source = placeholder_rule_source.clone();
            let strict_placeholder_cancellation = operation_cancellation.clone();
            let placeholder_rules = operation_cpu
                .execute(move || {
                    let service = GenericPlaceholderService::default();
                    let definitions = service
                        .parse_canonical_json_with_cancellation(&strict_placeholder_json, || {
                            ensure_generic_cpu_running(&strict_placeholder_cancellation)
                        })?
                        .map_err(|source| GenericPreparationError::Placeholder {
                            rule_source: strict_placeholder_source.clone(),
                            source,
                        })?;
                    service
                        .compile_for_ids_with_cancellation(
                            definitions,
                            &valid_placeholder_ids,
                            || ensure_generic_cpu_running(&strict_placeholder_cancellation),
                        )?
                        .map_err(|source| GenericPreparationError::Placeholder {
                            rule_source: strict_placeholder_source,
                            source,
                        })
                })
                .await
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_preparation_failure)?;

            let expected_raw_fingerprint = snapshot
                .project()
                .extracted_raw_fingerprint()
                .expect("load_current_translation_state 已确认存在 Extract 指纹");
            let planning_snapshot = snapshot;
            let planning_terms = Arc::clone(&terminology);
            let planning_rules = placeholder_rules.clone();
            let planning_rule_source = placeholder_rule_source.clone();
            let planning_language = Arc::clone(&source_language);
            let planning_prompt = prompt.fingerprint;
            let planning_client = Arc::clone(configuration.client());
            let planning_cancellation = operation_cancellation.clone();
            let target_characters = configuration
                .profile()
                .target_task_user_message_characters();
            let retry_rejected = command.retry_rejected();
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
                        &planning_rule_source,
                        planning_language,
                        AutomaticStateResources {
                            prompt: planning_prompt,
                            client_semantics: planning_client_fingerprint,
                            language_module: planning_language_fingerprint,
                            terminology_hits: empty_terminology_fingerprint(),
                        },
                        target_characters,
                        retry_rejected,
                        &planning_cancellation,
                    )
                })
                .await
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_preparation_failure)?;

            let PreparedGenericTranslation { plan, facts } = prepared;
            let (invalidations, reused, tasks, skipped_rejected) = plan.into_parts();
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
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_preparation_failure)?;
            let planned_units = tasks
                .iter()
                .map(PlannedTask::unit_count)
                .sum::<usize>()
                .saturating_add(skipped_rejected);
            let mut summary = GenericTranslationSummary {
                total_tasks: tasks.len(),
                planned_units,
                remaining_units: planned_units,
                ..GenericTranslationSummary::default()
            };
            set_generic_translate_summary(&operation_translate_project_log, summary);
            let task_project_log = install_generic_translate_task_log(
                &operation_project_log,
                &operation_translate_project_log,
                tasks.len(),
            );
            complete_generic_translate_phase(
                &operation_project_log,
                &operation_translate_project_log,
                ProjectLogPhase::Planning,
                ProjectLogAmount::Determinate {
                    completed: generic_count(tasks.len()),
                    total: generic_count(tasks.len()),
                },
            );
            if tasks.is_empty() {
                operation_progress.observe(ProgressSnapshot::determinate(
                    GenericProgressPhase::ConfirmedTasks,
                    0,
                    0,
                ));
            } else {
                start_generic_translate_phase(
                    &operation_project_log,
                    &operation_translate_project_log,
                    ProjectLogPhase::ConfirmedTasks,
                    ProjectLogAmount::Determinate {
                        completed: 0,
                        total: generic_count(tasks.len()),
                    },
                );
                operation_progress.observe(ProgressSnapshot::determinate(
                    GenericProgressPhase::ConfirmedTasks,
                    0,
                    generic_count(tasks.len()),
                ));
            }
            ensure_generic_operation_running(&operation_cancellation)?;
            if apply_translation_resources {
                let save_store = store.clone();
                let resource_outcome = run_project_blocking(
                    GenericDiagnosticStage::Translate,
                    StateEffect::ProgressPreserved,
                    database_path.clone(),
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
                mark_generic_translate_run_plan_saved(&operation_translate_project_log);
                summary.cleared_units = resource_outcome.committed;
                summary.conflicted_units += resource_outcome.conflicts.len();
                set_generic_translate_summary(&operation_translate_project_log, summary);
            }
            ensure_generic_operation_running(&operation_cancellation)?;

            summary.reused_units = reuse_writes.len();
            set_generic_translate_summary(&operation_translate_project_log, summary);
            if !reuse_writes.is_empty() {
                let commit_store = store.clone();
                let reuse_profile = profile_id.clone();
                let outcome = run_project_blocking(
                    GenericDiagnosticStage::Translate,
                    StateEffect::ProgressPreserved,
                    database_path.clone(),
                    move || {
                        commit_store.commit_translations_for_profile(
                            expected_raw_fingerprint,
                            &reuse_writes,
                            &reuse_profile,
                        )
                    },
                )
                .await?;
                if outcome.committed > 0 {
                    mark_generic_translate_run_plan_saved(&operation_translate_project_log);
                }
                add_commit_outcome(&mut summary, &outcome);
                set_generic_translate_summary(&operation_translate_project_log, summary);
            }

            ensure_generic_operation_running(&operation_cancellation)?;

            if !tasks.is_empty() {
                let pem_roots =
                    load_additional_pem_roots(&operation_file_system, configuration.llm()).await?;
                let llm =
                    OpenAiCompatibleExecutor::new(configuration.llm().with_pem_roots(pem_roots))
                        .map_err(|source| {
                            let report = DiagnosticReport::new(
                                StateEffect::ProgressPreserved,
                                source.diagnostic(),
                            );
                            GenericCommandError::reported(source, report)
                        })?;
                *operation_llm_holder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(llm.clone());
                let task_records = configure_generic_task_records(
                    command.record_translation_tasks(),
                    &operation_project_log,
                    &file_system_configuration,
                    configuration.client().api_key_redactor(),
                    locale,
                    operation_cpu.clone(),
                    project.workspace_root(),
                );
                *operation_task_record_holder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(task_records.clone());
                let task_result = execute_generic_tasks(GenericTaskExecution {
                    store: store.clone(),
                    expected_raw_fingerprint,
                    profile_id: profile_id.clone(),
                    tasks,
                    facts: Arc::new(facts),
                    placeholder_rules,
                    placeholder_rule_source,
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
                    project_log: task_project_log,
                    translate_project_log: Arc::clone(&operation_translate_project_log),
                    progress: operation_progress.clone(),
                })
                .await;
                let task_summary = task_result?;
                merge_task_summary(&mut summary, task_summary);
                set_generic_translate_summary(&operation_translate_project_log, summary);
                complete_generic_translate_phase(
                    &operation_project_log,
                    &operation_translate_project_log,
                    ProjectLogPhase::ConfirmedTasks,
                    ProjectLogAmount::Determinate {
                        completed: generic_count(summary.started_tasks),
                        total: generic_count(summary.total_tasks),
                    },
                );
            }

            ensure_generic_operation_running(&operation_cancellation)?;
            if should_remember_profile_separately(&summary) {
                let remember_store = store.clone();
                let remembered_profile = profile_id.clone();
                run_project_blocking(
                    GenericDiagnosticStage::Translate,
                    StateEffect::ProgressPreserved,
                    database_path,
                    move || remember_store.remember_profile(&remembered_profile),
                )
                .await?;
                mark_generic_translate_run_plan_saved(&operation_translate_project_log);
            }
            Ok(GenericCommandOutput::Translate {
                project: output_name,
                profile_id,
                summary,
            })
        };

        let cancellation_project_log = Arc::clone(&project_log);
        let driven = drive_generic_translate_with_panic_boundary(
            operation,
            termination_signals,
            || {
                emit_generic_cancellation_requested(&cancellation_project_log);
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
            },
            operation_panic_context,
        )
        .await;
        if let Some(error) = generic_translate_driven_error(&driven)
            && error.is_application_scope_panic()
        {
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
            if let Some(tasks) = translate_project_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tasks
                .clone()
            {
                tasks.fail_in_flight_after_panic(generic_command_error_report(error));
            }
        }
        let llm = llm_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(llm) = llm {
            llm.shutdown().await;
        }
        let task_records = task_record_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task_records) = task_records {
            task_records.finish().await;
        }
        progress.finalizing();
        let mut shutdown_errors = Vec::new();
        if let Err(source) = cpu.shutdown() {
            shutdown_errors.push(GenericShutdownError::cpu(source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::file_system(source));
        }
        record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
        let terminal_occurrence =
            finish_generic_translate_project_log(&project_log, &translate_project_log, &driven);
        GenericCommandRunReport::from_driven_with_terminal_occurrence(
            driven,
            shutdown_errors,
            take_generic_project_log(&project_log),
            terminal_occurrence,
        )
        .with_translation_summary(generic_terminal_translation_summary(&translate_project_log))
    }

    async fn run_write_back(
        self,
        command: ConfiguredGenericWriteBackCommand,
        termination_signals: &mut TerminationSignals,
    ) -> GenericCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let project_name = command.project_name().clone();
        let project_log = generic_project_log_slot();
        let publication_occurrence = generic_terminal_occurrence_slot();
        start_existing_generic_project_log(
            &project_log,
            command.common(),
            self.locale,
            &project_name,
            ProjectLogCommand::WriteBack,
            Arc::clone(&performance),
        );
        self.panic_context().observe_project_log_slot(&project_log);
        let file_system_configuration = command.common().filesystem().clone();
        let file_system =
            match start_file_system(file_system_configuration, Arc::clone(&performance)) {
                Ok(file_system) => file_system,
                Err(source) => {
                    return GenericCommandRunReport::from_driven(
                        Driven::Finished(Err(generic_file_system_build_failure(source))),
                        Vec::new(),
                        take_generic_project_log(&project_log),
                    );
                }
            };
        let cpu = match RayonCpuExecutor::start(command.cpu()) {
            Ok(cpu) => cpu,
            Err(source) => {
                let error = generic_cpu_start_failure(source);
                let mut shutdown_errors = Vec::new();
                if let Err(source) = file_system.shutdown().await {
                    shutdown_errors.push(GenericShutdownError::file_system(source));
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
        let progress = generic_terminal_progress(self.locale);
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
        let operation_project_log = generic_project_log_handle(&project_log);
        let operation_publication_occurrence = Arc::clone(&publication_occurrence);
        let repair_punctuation = command.write_back().repair_punctuation();
        let complete_continuation_whitespace =
            command.write_back().complete_continuation_whitespace();
        let layout_rules_path = command.layout_rules_path().map(Path::to_path_buf);
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

            let database_path = store.database_path().to_path_buf();
            let initial_store = store.clone();
            let (snapshot, live, current_resources) = run_project_blocking(
                GenericDiagnosticStage::WriteBack,
                StateEffect::Unchanged,
                database_path,
                move || initial_store.load_current_translation_state(),
            )
            .await?;
            let (layout_rules, external_layout_rules_path) =
                if let Some(requested_path) = layout_rules_path {
                    let file = operation_file_system
                        .read_file(requested_path)
                        .await
                        .map_err(|source| {
                            generic_read_file_failure(source, FileSystemDiagnosticStage::WriteBack)
                        })?;
                    let resolved_path = file.resolved_path().to_path_buf();
                    let bytes = file.into_bytes();
                    let parse_path = resolved_path.clone();
                    let rules = operation_cpu
                        .execute(move || LayoutRuleSet::parse_toml(&bytes))
                        .await
                        .map_err(generic_cpu_execution_failure)?
                        .map_err(|source| {
                            generic_layout_rules_failure(source, Some(parse_path), false)
                        })?;
                    (rules, Some(resolved_path))
                } else {
                    let layout_store = store.clone();
                    let database_path = store.database_path().to_path_buf();
                    let rules = run_project_blocking(
                        GenericDiagnosticStage::WriteBack,
                        StateEffect::Unchanged,
                        database_path,
                        move || layout_store.load_write_back_layout_rules(),
                    )
                    .await?;
                    (rules, None)
                };
            let compile_rules = layout_rules.clone();
            let compile_path = external_layout_rules_path.clone();
            let (live, compiled_layout_rules) = operation_cpu
                .execute(move || {
                    compile_generic_layout_rules(&live, &compile_rules)
                        .map(|compiled| (live, compiled))
                })
                .await
                .map_err(generic_cpu_execution_failure)?
                .map_err(|source| {
                    generic_layout_rules_failure(
                        source,
                        compile_path,
                        external_layout_rules_path.is_none(),
                    )
                })?;
            if external_layout_rules_path.is_some() {
                let expected_raw_fingerprint = live.raw_fingerprint();
                let save_store = store.clone();
                let database_path = store.database_path().to_path_buf();
                run_project_blocking(
                    GenericDiagnosticStage::WriteBack,
                    StateEffect::Unchanged,
                    database_path,
                    move || {
                        save_store.replace_write_back_layout_rules(
                            expected_raw_fingerprint,
                            &layout_rules,
                        )
                    },
                )
                .await?;
            }
            let project = snapshot.project().clone();
            let terminology = current_resources.terminology();
            let valid_placeholder_ids = snapshot.natural_unit_ids();
            let placeholder_json = current_resources.placeholder_rules_json().to_owned();
            let placeholder_compile_cancellation = operation_cancellation.clone();
            let placeholder_rules = operation_cpu
                .execute(move || {
                    let service = GenericPlaceholderService::default();
                    let definitions = service
                        .parse_canonical_json_with_cancellation(&placeholder_json, || {
                            ensure_generic_cpu_running(&placeholder_compile_cancellation)
                        })?
                        .map_err(|source| GenericPreparationError::Placeholder {
                            rule_source: GenericPlaceholderRuleSource::ProjectSnapshot,
                            source,
                        })?;
                    service
                        .compile_for_ids_with_cancellation(
                            definitions,
                            &valid_placeholder_ids,
                            || ensure_generic_cpu_running(&placeholder_compile_cancellation),
                        )?
                        .map_err(|source| GenericPreparationError::Placeholder {
                            rule_source: GenericPlaceholderRuleSource::ProjectSnapshot,
                            source,
                        })
                })
                .await
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_write_back_preparation_failure)?;

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
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_write_back_preparation_failure)?;
            let automatic_resources = if has_automatic_translation {
                match project.last_profile_id().map(str::to_owned) {
                    Some(profile_id) => {
                        let configuration = command
                            .resolve_translation(&profile_id)
                            .map_err(GenericCommandError::configuration)?;
                        let source_language = configuration
                            .language_modules()
                            .resolve(project.language_pair().source())
                            .map_err(|source| {
                                GenericCommandError::language_module(
                                    source,
                                    project.language_pair().target(),
                                )
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
                                .map_err(generic_cpu_execution_failure)?
                                .map_err(generic_write_back_preparation_failure)?,
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
            let write_back_rules = placeholder_rules;
            let write_back_layout_rules = compiled_layout_rules;
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
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_write_back_preparation_failure)?;
            ensure_generic_operation_running(&operation_cancellation)?;
            let candidate_cancellation = operation_cancellation.clone();
            let (write_back_project, candidate) = operation_cpu
                .execute(move || {
                    let project = current_snapshot.project().clone();
                    build_write_back_candidate_with_cancellation(
                        &current_snapshot,
                        &live,
                        &current_translations,
                        &write_back_rules,
                        &write_back_layout_rules,
                        GenericWriteBackTextOptions::new(
                            repair_punctuation,
                            complete_continuation_whitespace,
                        ),
                        &candidate_cancellation,
                    )
                    .map(|candidate| (project, candidate))
                })
                .await
                .map_err(generic_cpu_execution_failure)?
                .map_err(generic_write_back_candidate_failure)?;
            publish_generic_write_back(
                directory_publisher,
                output_name,
                write_back_project,
                candidate,
                operation_cancellation,
                operation_publication_gate,
                operation_project_log,
                operation_publication_occurrence,
                move || {
                    operation_progress.observe(ProgressSnapshot::indeterminate(
                        GenericProgressPhase::PublishingWriteBack,
                    ));
                },
            )
            .await
        };

        let cancellation_publication_gate = publication_gate;
        let cancellation_project_log = Arc::clone(&project_log);
        let driven = drive_write_back(operation, termination_signals, || {
            if !cancellation_publication_gate.request_cancellation() {
                return false;
            }
            emit_generic_cancellation_requested(&cancellation_project_log);
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
            shutdown_errors.push(GenericShutdownError::cpu(source));
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown_errors.push(GenericShutdownError::file_system(source));
        }
        record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
        let terminal_occurrence = *publication_occurrence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        GenericCommandRunReport::from_driven_with_terminal_occurrence(
            driven,
            shutdown_errors,
            take_generic_project_log(&project_log),
            terminal_occurrence,
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
                    | ProjectLuaRunError::Failed(ProjectLuaFailure::Cancelled)
                    | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
            )
        )
    }

    fn diagnostic_report(&self, database_path: &Path) -> DiagnosticReport {
        match self {
            Self::Open { path, source } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::sqlite(SqliteIssue::new(
                    SqliteDiagnosticContext::new(
                        SqliteDiagnosticStage::Lua,
                        SqliteOperation::Open,
                        SqliteTransactionState::NotStarted,
                    ),
                    SqliteProblem::Driver {
                        database: SafePath::new(path),
                        query_id: None,
                        query_ordinal: None,
                        failure: SqliteDriverFailure::from_error(source),
                    },
                )),
            ),
            Self::Run(source) => source.diagnostic_report(database_path),
        }
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
        .map_err(generic_prompt_resource_failure)?;
    let thinking = if let Some(path) = prompt_paths.thinking() {
        Some(
            read_unparsed_prompt_resource(file_system, path)
                .await
                .map_err(generic_prompt_resource_failure)?,
        )
    } else {
        None
    };
    let rules = read_unparsed_prompt_resource(file_system, prompt_paths.rules())
        .await
        .map_err(generic_prompt_resource_failure)?;
    let example = read_unparsed_prompt_resource(file_system, prompt_paths.example())
        .await
        .map_err(generic_prompt_resource_failure)?;
    let system_path = prompt_paths.system().to_path_buf();
    let thinking_path = prompt_paths.thinking().map(Path::to_path_buf);
    let rules_path = prompt_paths.rules().to_path_buf();
    let example_path = prompt_paths.example().to_path_buf();
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
    .map_err(generic_cpu_execution_failure)?
    .map_err(|source| match source {
        GenericPromptPreparationError::Cancelled => GenericCommandError::Cancelled,
        GenericPromptPreparationError::SystemResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::ThinkingResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::RulesResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::ExampleResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::SystemTemplate(source) => {
            generic_prompt_template_failure(&system_path, source)
        }
        GenericPromptPreparationError::ThinkingTemplate(source) => generic_prompt_template_failure(
            thinking_path
                .as_deref()
                .expect("thinking 模板失败必须对应已选择的 thinking 资源"),
            source,
        ),
        GenericPromptPreparationError::RulesTemplate(source) => {
            generic_prompt_template_failure(&rules_path, source)
        }
        GenericPromptPreparationError::ExampleTemplate(source) => {
            generic_prompt_template_failure(&example_path, source)
        }
    })
}

fn generic_prompt_template_failure(
    path: &Path,
    source: PromptTemplateError,
) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic(path));
    GenericCommandError::reported(source, report)
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
    locator: GenericUnitLocator,
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
    Placeholder {
        rule_source: GenericPlaceholderRuleSource,
        source: crate::generic::GenericPlaceholderError,
    },
    PlaceholderProtection {
        rule_source: GenericPlaceholderRuleSource,
        locator: GenericUnitLocator,
        source: PlaceholderProtectionError,
    },
    LanguageProjection {
        locator: GenericUnitLocator,
        source: LanguageTextProjectionError,
    },
    Planning(GenericPlanningError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GenericPlaceholderRuleSource {
    ExternalFile(PathBuf),
    ProjectSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GenericUnitLocator {
    relative_path: PathBuf,
    group_id: String,
    unit_id: String,
    role: String,
    line: usize,
    unit: usize,
}

impl GenericUnitLocator {
    fn readable_id(&self) -> String {
        crate::generic::readable_generic_unit_id(&self.relative_path, self.line, self.unit)
    }
}

impl fmt::Display for GenericPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic CPU 工作已取消"),
            Self::Placeholder { source, .. } => source.fmt(formatter),
            Self::PlaceholderProtection { source, .. } => source.fmt(formatter),
            Self::LanguageProjection { source, .. } => source.fmt(formatter),
            Self::Planning(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Placeholder { source, .. } => Some(source),
            Self::PlaceholderProtection { source, .. } => Some(source),
            Self::LanguageProjection { source, .. } => Some(source),
            Self::Planning(source) => Some(source),
        }
    }
}

impl From<GenericPlanningError> for GenericPreparationError {
    fn from(source: GenericPlanningError) -> Self {
        Self::Planning(source)
    }
}

fn generic_placeholder_protection_failure(
    source: crate::generic::GenericPlaceholderError,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> GenericPreparationError {
    match source {
        crate::generic::GenericPlaceholderError::Protection(source) => {
            GenericPreparationError::PlaceholderProtection {
                rule_source: rule_source.clone(),
                locator: locator.clone(),
                source,
            }
        }
        source => GenericPreparationError::Placeholder {
            rule_source: rule_source.clone(),
            source,
        },
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
    source: CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> GenericCommandError {
    match source {
        CpuTaskExecutionError::Cancelled => GenericCommandError::Cancelled,
        source @ (CpuTaskExecutionError::Unavailable(_) | CpuTaskExecutionError::TaskPanicked) => {
            let report = DiagnosticReport::new(StateEffect::ProgressPreserved, source.diagnostic());
            GenericCommandError::reported(source, report)
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
            let report = source.diagnostic_report_at(FileSystemDiagnosticStage::Project);
            GenericCommandError::reported(source, report)
        }
    }
}

fn generic_manual_failure(source: ManualCommandError) -> GenericCommandError {
    if source.is_cancelled() {
        return GenericCommandError::Cancelled;
    }
    let report = source.diagnostic_report();
    GenericCommandError::reported(source, report)
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
    source: ReadFileError<SystemFileSystemError>,
    stage: FileSystemDiagnosticStage,
) -> GenericCommandError {
    if read_file_error_is_cancelled(&source) {
        GenericCommandError::Cancelled
    } else {
        let report = generic_read_file_report(&source, stage);
        GenericCommandError::reported(source, report)
    }
}

fn generic_prompt_resource_failure(source: PromptResourceLoadError) -> GenericCommandError {
    if matches!(
        &source,
        PromptResourceLoadError::Read(source) if read_file_error_is_cancelled(source)
    ) {
        GenericCommandError::Cancelled
    } else {
        let report = source.diagnostic_report();
        GenericCommandError::reported(source, report)
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
    } else {
        let report = generic_translation_resource_report(&source);
        GenericCommandError::reported(source, report)
    }
}

fn generic_translation_resource_report(
    source: &TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>,
) -> DiagnosticReport {
    source.diagnostic_report()
}

fn generic_preparation_failure(source: GenericPreparationError) -> GenericCommandError {
    generic_preparation_failure_at(GenericDiagnosticStage::Translate, source)
}

fn generic_write_back_preparation_failure(source: GenericPreparationError) -> GenericCommandError {
    generic_preparation_failure_at(GenericDiagnosticStage::WriteBack, source)
}

#[derive(Debug)]
struct GenericLayoutRulesFailure {
    path: Option<PathBuf>,
    project_snapshot: bool,
    source: LayoutRulesError,
}

impl fmt::Display for GenericLayoutRulesFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(
                formatter,
                "排版规则无效 {}：{}",
                path.display(),
                self.source
            ),
            None => write!(formatter, "项目保存的排版规则无效：{}", self.source),
        }
    }
}

impl Error for GenericLayoutRulesFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

fn generic_layout_rules_failure(
    source: LayoutRulesError,
    path: Option<PathBuf>,
    project_snapshot: bool,
) -> GenericCommandError {
    let failure = GenericLayoutRulesFailure {
        path,
        project_snapshot,
        source,
    };
    let report = DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::WriteBack,
            GenericProblem::WriteBackLayoutRules {
                path: failure.path.as_ref().map(SafePath::new),
                rule_number: failure.source.rule_number(),
                project_snapshot: failure.project_snapshot,
            },
        )),
    );
    GenericCommandError::reported(failure, report)
}

fn generic_preparation_failure_at(
    stage: GenericDiagnosticStage,
    source: GenericPreparationError,
) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else {
        let report = generic_preparation_report_at(&source, stage);
        GenericCommandError::reported(source, report)
    }
}

fn generic_preparation_report_at(
    source: &GenericPreparationError,
    stage: GenericDiagnosticStage,
) -> DiagnosticReport {
    if let Some(report) = generic_placeholder_protection_report(source, stage) {
        return report;
    }
    let preparation = |unit, problem| {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::TranslationPreparation { unit, problem },
            )),
        )
    };
    match source {
        GenericPreparationError::Cancelled => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericPreparationError::Placeholder {
            rule_source,
            source,
        } => match source {
            crate::generic::GenericPlaceholderError::InvalidResourceSnapshot(source) => {
                preparation(
                    None,
                    GenericTranslationPreparationProblem::InvalidPlaceholderSnapshot {
                        category: GenericJsonErrorCategory::from(
                            crate::json_diagnostic::JsonErrorCategory::from(source),
                        ),
                        line: source.line(),
                        column: source.column(),
                    },
                )
            }
            crate::generic::GenericPlaceholderError::Compilation(source) => {
                let origin = match rule_source {
                    GenericPlaceholderRuleSource::ExternalFile(path) => {
                        TranslationPlanningResourceOrigin::external(path)
                    }
                    GenericPlaceholderRuleSource::ProjectSnapshot => {
                        TranslationPlanningResourceOrigin::ProjectSnapshot
                    }
                };
                DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                        origin,
                        problem: source.diagnostic_problem(),
                    }),
                )
            }
            crate::generic::GenericPlaceholderError::Protection(_) => preparation(
                None,
                GenericTranslationPreparationProblem::UnexpectedUnlocatedPlaceholderProtection,
            ),
            crate::generic::GenericPlaceholderError::Restore(source) => match source {
                PlaceholderRestoreError::Projection(source) => preparation(
                    None,
                    GenericTranslationPreparationProblem::PlaceholderRestoreProjection {
                        problem: generic_language_projection_problem(source),
                    },
                ),
                PlaceholderRestoreError::Multiset(source) => preparation(
                    None,
                    GenericTranslationPreparationProblem::PlaceholderRestoreMultiset {
                        problem: generic_placeholder_multiset_problem(source),
                    },
                ),
            },
            crate::generic::GenericPlaceholderError::ManualTranslationMismatch => preparation(
                None,
                GenericTranslationPreparationProblem::ManualTranslationPlaceholderMismatch,
            ),
        },
        GenericPreparationError::PlaceholderProtection { .. } => {
            unreachable!("带定位的 Placeholder 失败已在函数入口处理")
        }
        GenericPreparationError::LanguageProjection { locator, source }
            if stage == GenericDiagnosticStage::WriteBack =>
        {
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: diagnostic_generic_unit_locator(locator),
                        problem: GenericWriteBackUnitProblem::LanguageProjection {
                            side: GenericWriteBackTextSide::Source,
                            problem: generic_language_projection_problem(source),
                        },
                    },
                )),
            )
        }
        GenericPreparationError::LanguageProjection { locator, source } => preparation(
            Some(diagnostic_generic_unit_locator(locator)),
            GenericTranslationPreparationProblem::LanguageProjection {
                problem: generic_language_projection_problem(source),
            },
        ),
        GenericPreparationError::Planning(source) => generic_planning_report(source),
    }
}

fn diagnostic_generic_unit_locator(locator: &GenericUnitLocator) -> DiagnosticGenericUnitLocator {
    DiagnosticGenericUnitLocator::new(
        &locator.relative_path,
        &locator.group_id,
        &locator.unit_id,
        Some(&locator.role),
    )
    .with_natural_position(locator.line, locator.unit)
}

const fn generic_language_projection_problem(
    source: &LanguageTextProjectionError,
) -> GenericLanguageProjectionProblem {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            GenericLanguageProjectionProblem::TokenIndexConstruction
        }
        LanguageTextProjectionError::EmptyToken => GenericLanguageProjectionProblem::EmptyToken,
        LanguageTextProjectionError::MissingToken { .. } => {
            GenericLanguageProjectionProblem::MissingToken
        }
        LanguageTextProjectionError::RepeatedToken { .. } => {
            GenericLanguageProjectionProblem::RepeatedToken
        }
        LanguageTextProjectionError::OverlappingToken { .. } => {
            GenericLanguageProjectionProblem::OverlappingToken
        }
        LanguageTextProjectionError::ChangedTokenOrder { position, .. } => {
            GenericLanguageProjectionProblem::ChangedTokenOrder {
                position: *position,
            }
        }
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            GenericLanguageProjectionProblem::ChangedSegmentCount {
                expected: *expected,
                actual: *actual,
            }
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            GenericLanguageProjectionProblem::ChangedSegmentKind {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            GenericLanguageProjectionProblem::MissingOrderedToken {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            GenericLanguageProjectionProblem::UnusedOrderedToken
        }
    }
}

const fn generic_placeholder_multiset_problem(
    source: &PlaceholderMultisetError,
) -> GenericPlaceholderMultisetProblem {
    match source {
        PlaceholderMultisetError::Mismatch { .. } => GenericPlaceholderMultisetProblem::Mismatch,
        PlaceholderMultisetError::Unexpected { .. } => {
            GenericPlaceholderMultisetProblem::Unexpected
        }
        PlaceholderMultisetError::OrderMismatch { .. } => {
            GenericPlaceholderMultisetProblem::OrderMismatch
        }
        PlaceholderMultisetError::WrapperTopologyChanged { .. } => {
            GenericPlaceholderMultisetProblem::OrderMismatch
        }
    }
}

fn generic_response_restore_problem(
    source: &PlaceholderRestoreError,
) -> GenericResponseDestinationProblem {
    match source {
        PlaceholderRestoreError::Projection(source) => {
            GenericResponseDestinationProblem::PlaceholderRestoreProjection {
                problem: generic_language_projection_problem(source),
            }
        }
        PlaceholderRestoreError::Multiset(source) => {
            GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                problem: generic_placeholder_multiset_problem(source),
            }
        }
    }
}

fn generic_response_placeholder_problem(
    source: &crate::generic::GenericPlaceholderError,
) -> GenericResponseDestinationProblem {
    match source {
        crate::generic::GenericPlaceholderError::InvalidResourceSnapshot(source) => {
            GenericResponseDestinationProblem::InvalidPlaceholderSnapshot {
                category: GenericJsonErrorCategory::from(
                    crate::json_diagnostic::JsonErrorCategory::from(source),
                ),
                line: source.line(),
                column: source.column(),
            }
        }
        crate::generic::GenericPlaceholderError::Compilation(source) => {
            GenericResponseDestinationProblem::PlaceholderCompilation {
                problem: source.diagnostic_problem(),
            }
        }
        crate::generic::GenericPlaceholderError::Protection(source) => {
            GenericResponseDestinationProblem::PlaceholderProtection {
                problem: source.diagnostic_issue(),
            }
        }
        crate::generic::GenericPlaceholderError::Restore(source) => {
            generic_response_restore_problem(source)
        }
        crate::generic::GenericPlaceholderError::ManualTranslationMismatch => {
            GenericResponseDestinationProblem::PlaceholderBindingMismatch
        }
    }
}

fn generic_candidate_placeholder_problem(
    source: crate::generic::GenericPlaceholderError,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> Result<GenericResponseDestinationProblem, GenericPreparationError> {
    match source {
        crate::generic::GenericPlaceholderError::Protection(
            source @ (PlaceholderProtectionError::StartWorker { .. }
            | PlaceholderProtectionError::Match { .. }),
        ) => Err(GenericPreparationError::PlaceholderProtection {
            rule_source: rule_source.clone(),
            locator: locator.clone(),
            source,
        }),
        source => Ok(generic_response_placeholder_problem(&source)),
    }
}

fn generic_current_translation_protection_result(
    protected: Result<GenericProtectedText, crate::generic::GenericPlaceholderError>,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> Result<Option<GenericProtectedText>, GenericPreparationError> {
    match protected {
        Ok(protected) => Ok(Some(protected)),
        Err(source) => match generic_candidate_placeholder_problem(source, rule_source, locator) {
            Ok(_) => Ok(None),
            Err(source) => Err(source),
        },
    }
}

fn generic_planning_report(source: &GenericPlanningError) -> DiagnosticReport {
    match source {
        GenericPlanningError::Cancelled => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::TaskPlanning {
                problem: TranslationTaskPlanningProblem::Cancelled,
            }),
        ),
        GenericPlanningError::TaskPlanning(source) => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::TaskPlanning {
                problem: generic_task_planning_problem(source),
            }),
        ),
        GenericPlanningError::MissingCurrentContext(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::MissingCurrentContext {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Missing(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::MissingPlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Unknown(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::UnknownPlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Duplicate(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::DuplicatePlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
    }
}

fn generic_planning_fact_report(
    locator: &GenericPlanningUnitLocator,
    problem: GenericTranslationPreparationProblem,
) -> DiagnosticReport {
    let unit = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    );
    let unit = match locator.natural_position() {
        Some((line, unit_ordinal)) => unit.with_natural_position(line, unit_ordinal),
        None => unit,
    };
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TranslationPreparation {
                unit: Some(unit),
                problem,
            },
        )),
    )
}

const fn generic_task_planning_problem(
    source: &TaskPlanningError,
) -> TranslationTaskPlanningProblem {
    match source {
        TaskPlanningError::Cancelled => TranslationTaskPlanningProblem::Cancelled,
        TaskPlanningError::EmptyScope => TranslationTaskPlanningProblem::EmptyScope,
        TaskPlanningError::EmptyGroup => TranslationTaskPlanningProblem::EmptyGroup,
        TaskPlanningError::UnitCountOverflow => TranslationTaskPlanningProblem::UnitCountOverflow,
        TaskPlanningError::CharacterCountOverflow => {
            TranslationTaskPlanningProblem::CharacterCountOverflow
        }
        TaskPlanningError::ResponsibilityCountMismatch { expected, actual } => {
            TranslationTaskPlanningProblem::ResponsibilityCountMismatch {
                expected: *expected,
                actual: *actual,
            }
        }
        TaskPlanningError::TaskIdOverflow => TranslationTaskPlanningProblem::TaskIdOverflow,
    }
}

fn generic_placeholder_protection_report(
    source: &GenericPreparationError,
    stage: GenericDiagnosticStage,
) -> Option<DiagnosticReport> {
    let GenericPreparationError::PlaceholderProtection {
        rule_source,
        locator,
        source,
    } = source
    else {
        return None;
    };
    let rule_source = match rule_source {
        GenericPlaceholderRuleSource::ExternalFile(path) => {
            DiagnosticPlaceholderRuleSource::external_file(path)
        }
        GenericPlaceholderRuleSource::ProjectSnapshot => {
            DiagnosticPlaceholderRuleSource::ProjectSnapshot
        }
    };
    let unit = DiagnosticGenericUnitLocator::new(
        &locator.relative_path,
        &locator.group_id,
        &locator.unit_id,
        Some(&locator.role),
    )
    .with_natural_position(locator.line, locator.unit);
    let problem = placeholder_protection_issue(source);
    if stage == GenericDiagnosticStage::WriteBack {
        return Some(DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackUnit {
                    unit,
                    problem: GenericWriteBackUnitProblem::PlaceholderProtection {
                        side: GenericWriteBackTextSide::Source,
                        problem,
                    },
                },
            )),
        ));
    }
    Some(DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::translation(TranslationIssue::Placeholder {
            rule_source,
            unit,
            problem,
        }),
    ))
}

fn placeholder_protection_issue(source: &PlaceholderProtectionError) -> PlaceholderIssue {
    match source {
        PlaceholderProtectionError::StartWorker { operation, source } => {
            PlaceholderIssue::WorkerStart {
                operation: diagnostic_placeholder_worker_operation(*operation),
                io_kind: SafeIoKind::from(source.kind()),
                raw_os_code: source.raw_os_error(),
            }
        }
        PlaceholderProtectionError::Match { rule, source } => PlaceholderIssue::PatternMatch {
            rule_origin: Some(diagnostic_placeholder_rule_origin(rule.origin())),
            rule_number: rule.rule_number(),
            pcre2: Pcre2Failure {
                kind: diagnostic_pcre2_failure_kind(source.kind()),
                code: source.code(),
                offset: source.offset(),
            },
        },
        PlaceholderProtectionError::EmptyMatch { matched } => PlaceholderIssue::EmptyMatch {
            rule_origin: diagnostic_placeholder_rule_origin(matched.rule().origin()),
            rule_number: matched.rule().rule_number(),
            match_range: known_placeholder_range(matched.start_byte(), matched.end_byte()),
        },
        PlaceholderProtectionError::MissingTextCapture {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
        } => PlaceholderIssue::MissingTextCapture {
            rule_number: *rule_number,
            match_range: known_placeholder_range(*whole_match_start_byte, *whole_match_end_byte),
        },
        PlaceholderProtectionError::InvalidMatchRange {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
            capture_start_byte,
            capture_end_byte,
            violation,
        } => PlaceholderIssue::InvalidMatchRange {
            rule_number: *rule_number,
            whole_match_start_byte: *whole_match_start_byte,
            whole_match_end_byte: *whole_match_end_byte,
            capture_start_byte: *capture_start_byte,
            capture_end_byte: *capture_end_byte,
            violation: diagnostic_match_range_violation(*violation),
        },
        PlaceholderProtectionError::OverlappingMatches { first, second } => {
            PlaceholderIssue::OverlappingMatches {
                first_origin: diagnostic_placeholder_rule_origin(first.rule().origin()),
                first_rule_number: first.rule().rule_number(),
                first_range: known_placeholder_range(first.start_byte(), first.end_byte()),
                second_origin: diagnostic_placeholder_rule_origin(second.rule().origin()),
                second_rule_number: second.rule().rule_number(),
                second_range: known_placeholder_range(second.start_byte(), second.end_byte()),
            }
        }
        PlaceholderProtectionError::CrossesLineBoundary {
            matched,
            source_line_index,
        } => PlaceholderIssue::CrossesLineBoundary {
            rule_origin: diagnostic_placeholder_rule_origin(matched.rule().origin()),
            rule_number: matched.rule().rule_number(),
            source_line_index: *source_line_index,
        },
        PlaceholderProtectionError::ReservedTokenNamespace {
            start_byte,
            end_byte,
        } => PlaceholderIssue::ReservedTokenNamespace {
            range: known_placeholder_range(*start_byte, *end_byte),
        },
    }
}

fn known_placeholder_range(start: usize, end: usize) -> ByteRange {
    ByteRange::new(start, end).expect("Placeholder 叶子错误必须保持已确认的正向匹配范围")
}

const fn diagnostic_placeholder_rule_origin(
    origin: PlaceholderRuleOrigin,
) -> DiagnosticPlaceholderRuleOrigin {
    match origin {
        PlaceholderRuleOrigin::BuiltIn => DiagnosticPlaceholderRuleOrigin::Builtin,
        PlaceholderRuleOrigin::Custom => DiagnosticPlaceholderRuleOrigin::Custom,
    }
}

const fn diagnostic_placeholder_worker_operation(
    operation: PlaceholderWorkerOperation,
) -> DiagnosticPlaceholderWorkerOperation {
    match operation {
        PlaceholderWorkerOperation::CompileCustomRules => {
            DiagnosticPlaceholderWorkerOperation::CompileCustomRules
        }
        PlaceholderWorkerOperation::MatchText => DiagnosticPlaceholderWorkerOperation::MatchText,
    }
}

const fn diagnostic_pcre2_failure_kind(kind: PlaceholderPcre2ErrorKind) -> Pcre2FailureKind {
    match kind {
        PlaceholderPcre2ErrorKind::Compile => Pcre2FailureKind::Compile,
        PlaceholderPcre2ErrorKind::Jit => Pcre2FailureKind::Jit,
        PlaceholderPcre2ErrorKind::Match => Pcre2FailureKind::Match,
        PlaceholderPcre2ErrorKind::Info => Pcre2FailureKind::Info,
        PlaceholderPcre2ErrorKind::Option => Pcre2FailureKind::Option,
        PlaceholderPcre2ErrorKind::Unrecognized => Pcre2FailureKind::Unrecognized,
    }
}

const fn diagnostic_match_range_violation(
    violation: PlaceholderMatchRangeViolation,
) -> DiagnosticMatchRangeViolation {
    match violation {
        PlaceholderMatchRangeViolation::WholeStartAfterEnd => {
            DiagnosticMatchRangeViolation::WholeStartAfterEnd
        }
        PlaceholderMatchRangeViolation::WholeEndBeyondText => {
            DiagnosticMatchRangeViolation::WholeEndBeyondText
        }
        PlaceholderMatchRangeViolation::WholeStartNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::WholeStartNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::WholeEndNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::WholeEndNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureStartAfterEnd => {
            DiagnosticMatchRangeViolation::CaptureStartAfterEnd
        }
        PlaceholderMatchRangeViolation::CaptureEndBeyondText => {
            DiagnosticMatchRangeViolation::CaptureEndBeyondText
        }
        PlaceholderMatchRangeViolation::CaptureStartNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::CaptureStartNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureEndNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::CaptureEndNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureStartsBeforeWhole => {
            DiagnosticMatchRangeViolation::CaptureStartsBeforeWhole
        }
        PlaceholderMatchRangeViolation::CaptureEndsAfterWhole => {
            DiagnosticMatchRangeViolation::CaptureEndsAfterWhole
        }
    }
}

fn generic_write_back_candidate_failure(source: GenericWriteBackError) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else {
        let report = source.diagnostic_report(StateEffect::Unchanged);
        GenericCommandError::reported(source, report)
    }
}

// 这是翻译规划边界：每项参数都有独立的所有权和取消语义，合并为可变上下文会掩盖它们。
#[allow(clippy::too_many_arguments)]
fn prepare_generic_translation(
    snapshot: &GenericStoredSnapshot,
    terminology: Arc<CompiledTerminology>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    source_language: Arc<dyn LanguageModule>,
    base_resources: AutomaticStateResources,
    target_task_characters: std::num::NonZeroUsize,
    retry_rejected: bool,
    cancellation: &CooperativeCancellation,
) -> Result<PreparedGenericTranslation, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut groups = Vec::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for (group_ordinal, group) in file.groups().iter().enumerate() {
            ensure_generic_cpu_running(cancellation)?;
            groups.push((file.relative_path(), group_ordinal, group));
        }
    }
    let prepared_groups = groups
        .par_iter()
        .map(|(relative_path, group_ordinal, group)| {
            ensure_generic_cpu_running(cancellation)?;
            let service = GenericPlaceholderService::default();
            let mut prepared_units = Vec::with_capacity(group.units().len());
            for (unit_ordinal, unit) in group.units().iter().enumerate() {
                ensure_generic_cpu_running(cancellation)?;
                let locator = GenericUnitLocator {
                    relative_path: relative_path.to_path_buf(),
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    role: group.kind().to_owned(),
                    line: group_ordinal + 1,
                    unit: unit_ordinal + 1,
                };
                let target_id = locator.readable_id();
                let protected = service
                    .protect_target_with_cancellation(
                        &target_id,
                        group.kind(),
                        unit.source_text(),
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .map_err(|source| {
                        generic_placeholder_protection_failure(
                            source,
                            placeholder_rule_source,
                            &locator,
                        )
                    })?;
                let language_text = protected
                    .language_text_with_cancellation(|| ensure_generic_cpu_running(cancellation))?
                    .map_err(|source| GenericPreparationError::LanguageProjection {
                        locator: locator.clone(),
                        source,
                    })?;
                let analysis = source_language
                    .analyze_source_with_cancellation(&language_text, &mut || {
                        ensure_generic_language_running(cancellation)
                    })
                    .map_err(|LanguageOperationCancelled| GenericPreparationError::Cancelled)?;
                ensure_generic_cpu_running(cancellation)?;
                prepared_units.push((unit, locator, protected, language_text, analysis));
            }
            ensure_generic_cpu_running(cancellation)?;
            let term_indices = terminology.triggered_indices_with_cancellation(
                prepared_units
                    .iter()
                    .flat_map(|(_, _, _, language_text, _)| natural_segments(language_text)),
                || ensure_generic_cpu_running(cancellation),
            )?;
            let terminology_hits = terminology_hit_fingerprint_with_cancellation(
                terminology.as_ref(),
                &term_indices,
                || ensure_generic_cpu_running(cancellation),
            )?;
            let mut planning_units = Vec::with_capacity(prepared_units.len());
            let mut facts = Vec::with_capacity(prepared_units.len());
            for (unit, locator, protected, language_text, analysis) in prepared_units {
                ensure_generic_cpu_running(cancellation)?;
                let resources = AutomaticStateResources {
                    terminology_hits,
                    ..base_resources
                };
                let mut planning = PlanningUnit::from_stored_with_cancellation(
                    relative_path,
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
                    retry_rejected,
                    cancellation,
                )
                .map_err(GenericPreparationError::Planning)?;
                if let Some(current_translation) = planning.current_translation() {
                    let text_violation = validate_reflowed_candidate_text_with_cancellation(
                        current_translation,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .err();
                    let current_protected = if text_violation.is_none() {
                        generic_current_translation_protection_result(
                            service.protect_target_with_cancellation(
                                &locator.readable_id(),
                                group.kind(),
                                current_translation,
                                placeholder_rules,
                                || ensure_generic_cpu_running(cancellation),
                            )?,
                            placeholder_rule_source,
                            &locator,
                        )?
                    } else {
                        None
                    };
                    if let Some(current_protected) = current_protected
                        && current_protected.binding_fingerprint()
                            == protected.binding_fingerprint()
                    {
                        planning.install_current_target_context(clone_generic_cpu_text(
                            current_protected.text(),
                            cancellation,
                        )?);
                    } else {
                        planning.reject_invalid_current(
                            text_violation.unwrap_or(ProvenInvariantViolation::PlaceholderMismatch),
                        );
                    }
                }
                if planning.needs_candidate() {
                    facts.push((
                        clone_generic_unit_key(planning.key(), cancellation)?,
                        GenericValidationFact {
                            locator,
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
                placeholder_rule_source,
                source_language.as_ref(),
                cancellation,
            )
        },
        cancellation,
    )?;
    Ok(PreparedGenericTranslation { plan, facts })
}

fn collect_generic_current_translations(
    snapshot: &GenericStoredSnapshot,
    terminology: &CompiledTerminology,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    automatic_resources: Option<AutomaticStateResources>,
    cancellation: &CooperativeCancellation,
) -> Result<GenericUnitMap<GenericCurrentTranslation>, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut groups = Vec::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for (group_ordinal, group) in file.groups().iter().enumerate() {
            ensure_generic_cpu_running(cancellation)?;
            groups.push((file.relative_path(), group_ordinal, group));
        }
    }
    let prepared_groups = groups
        .par_iter()
        .map(|(relative_path, group_ordinal, group)| {
            ensure_generic_cpu_running(cancellation)?;
            let has_automatic = group.units().iter().any(|unit| {
                unit.translation()
                    .is_some_and(|translation| translation.origin() == TranslationOrigin::Automatic)
            });
            if !has_automatic {
                let mut current = Vec::new();
                for unit in group.units() {
                    if let Some(translation) = unit
                        .translation()
                        .filter(|translation| translation.origin() == TranslationOrigin::Manual)
                    {
                        current.push((
                            GenericUnitKey::new(
                                clone_generic_cpu_text(group.id(), cancellation)?,
                                clone_generic_cpu_text(unit.id(), cancellation)?,
                            ),
                            GenericCurrentTranslation::new(
                                clone_generic_cpu_text(translation.translation(), cancellation)?,
                                true,
                            ),
                        ));
                    }
                }
                return Ok::<_, GenericPreparationError>(current);
            }
            let service = GenericPlaceholderService::default();
            let mut protected_units = Vec::with_capacity(group.units().len());
            for (unit_ordinal, unit) in group.units().iter().enumerate() {
                ensure_generic_cpu_running(cancellation)?;
                let locator = GenericUnitLocator {
                    relative_path: relative_path.to_path_buf(),
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    role: group.kind().to_owned(),
                    line: group_ordinal + 1,
                    unit: unit_ordinal + 1,
                };
                let target_id = locator.readable_id();
                let protected = service
                    .protect_target_with_cancellation(
                        &target_id,
                        group.kind(),
                        unit.source_text(),
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .map_err(|source| {
                        generic_placeholder_protection_failure(
                            source,
                            &GenericPlaceholderRuleSource::ProjectSnapshot,
                            &locator,
                        )
                    })?;
                let language_text = protected
                    .language_text_with_cancellation(|| ensure_generic_cpu_running(cancellation))?
                    .map_err(|source| GenericPreparationError::LanguageProjection {
                        locator,
                        source,
                    })?;
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
                        GenericCurrentTranslation::new(
                            translation,
                            unit.translation()
                                .is_some_and(|stored| stored.origin() == TranslationOrigin::Manual),
                        ),
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
    total_tasks: usize,
    problem: GenericTaskResponseProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: generic_task_ordinal(task_index),
                total_tasks: generic_count(total_tasks),
                problem,
            },
        )),
    )
}

fn generic_response_problem_diagnostic(
    task_index: usize,
    total_tasks: usize,
    problem: &ResponseProblem,
) -> DiagnosticReport {
    generic_task_response_diagnostic(task_index, total_tasks, problem.clone())
}

fn generic_response_review_diagnostic(
    task_index: usize,
    total_tasks: usize,
    review: &TranslationReview,
) -> DiagnosticReport {
    let locator = review.locator();
    let destination = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    );
    let destination = match locator.natural_position() {
        Some((line, unit)) => destination.with_natural_position(line, unit),
        None => destination,
    };
    let finding = match review.finding() {
        ReviewFinding::SourceResidual => GenericResponseReviewFinding::SourceResidual,
        ReviewFinding::NonStopFinish => GenericResponseReviewFinding::NonStopFinish,
    };
    generic_task_response_diagnostic(
        task_index,
        total_tasks,
        GenericTaskResponseProblem::DestinationReview {
            output_id: u64::try_from(review.output_id().get())
                .expect("当前平台 usize 必须能够无损表示为 u64"),
            destination,
            finding,
        },
    )
}

fn generic_response_parse_diagnostic(
    task_index: usize,
    total_tasks: usize,
    error: TranslationTaskResponseParseError,
) -> DiagnosticReport {
    let problem = match error.kind() {
        TranslationTaskResponseParseErrorKind::Json(category)
        | TranslationTaskResponseParseErrorKind::JsonRepair { category, .. } => {
            GenericTaskResponseProblem::InvalidJson {
                category: match category {
                    TranslationTaskResponseJsonErrorCategory::Io => {
                        GenericTaskResponseJsonCategory::Io
                    }
                    TranslationTaskResponseJsonErrorCategory::Syntax => {
                        GenericTaskResponseJsonCategory::Syntax
                    }
                    TranslationTaskResponseJsonErrorCategory::Shape => {
                        GenericTaskResponseJsonCategory::Shape
                    }
                    TranslationTaskResponseJsonErrorCategory::UnexpectedEof => {
                        GenericTaskResponseJsonCategory::UnexpectedEof
                    }
                },
                line: error.line(),
                column: error.column(),
            }
        }
        TranslationTaskResponseParseErrorKind::ThinkingEmpty => {
            GenericTaskResponseProblem::ThinkingEmpty {
                line: error.line(),
                column: error.column(),
            }
        }
    };
    generic_task_response_diagnostic(task_index, total_tasks, problem)
}

fn generic_unavailable_task_diagnostic(
    task_index: usize,
    total_tasks: usize,
    reason: GenericTaskUnavailableReason,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskUnavailable {
                task_ordinal: generic_task_ordinal(task_index),
                total_tasks: generic_count(total_tasks),
                reason,
            },
        )),
    )
}

fn generic_task_execution_error_report(
    error: &GenericCommandError,
    task_index: usize,
    total_tasks: usize,
) -> DiagnosticReport {
    let report = generic_command_error_report(error);
    if matches!(report.primary().code(), "generic.translation.failed") {
        generic_unavailable_task_diagnostic(
            task_index,
            total_tasks,
            GenericTaskUnavailableReason::RequestFailed,
        )
    } else {
        report.with_effect(StateEffect::ProgressPreserved)
    }
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
    placeholder_rule_source: GenericPlaceholderRuleSource,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_mode: TranslationResponseMode,
    client: Arc<crate::runtime::llm::OpenAiCompatibleClient>,
    llm: OpenAiCompatibleExecutor,
    retry_delays: Vec<Duration>,
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    task_records: ConfiguredTranslationTaskRecordSink,
    project_log: GenericTaskProjectLog,
    translate_project_log: GenericTranslateProjectLogStateRef,
    progress: TerminalProgressObserver<GenericProgressPhase>,
}

#[derive(Clone)]
struct GenericTaskProjectLog {
    handle: ProjectLogHandle,
    state: Arc<Mutex<GenericTaskLogState>>,
}

#[derive(Clone, Copy)]
enum GenericTaskTerminal {
    Complete,
    Partial,
    Unavailable,
    Failed,
    NotCommittedAfterEarlierFailure,
    Cancelled,
}

#[derive(Default)]
struct GenericTaskLogState {
    planned: u64,
    started: u64,
    complete: u64,
    partial: u64,
    unavailable: u64,
    failed: u64,
    cancelled: u64,
    in_flight: HashSet<usize>,
    failure_occurrence: Option<(
        crate::runtime::project_log::DiagnosticOccurrenceId,
        StateEffect,
    )>,
}

impl GenericTaskProjectLog {
    fn new(handle: ProjectLogHandle, total_tasks: usize) -> Self {
        Self {
            handle,
            state: Arc::new(Mutex::new(GenericTaskLogState {
                planned: generic_count(total_tasks),
                ..GenericTaskLogState::default()
            })),
        }
    }

    fn position(&self, task_index: usize) -> TaskPosition {
        let total = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .planned;
        TaskPosition::new(generic_task_ordinal(task_index), total)
            .expect("Generic task index 必须位于已确认的计划范围内")
    }

    fn started(&self, task_index: usize) {
        let task = self.position(task_index);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.started = state
                .started
                .checked_add(1)
                .expect("Generic task started 计数不得溢出");
            assert!(
                state.in_flight.insert(task_index),
                "同一 Generic task 不得重复开始"
            );
        }
        self.handle.emit(ProjectLogEvent::TaskStarted { task });
    }

    fn finished(
        &self,
        task_index: usize,
        attempts: usize,
        terminal: GenericTaskTerminal,
        diagnostics: impl IntoIterator<Item = DiagnosticReport>,
    ) {
        let task = self.position(task_index);
        let earlier_failure = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                state.in_flight.remove(&task_index),
                "Generic task 终态必须对应已经开始且尚未结束的任务"
            );
            let counter = match terminal {
                GenericTaskTerminal::Complete => &mut state.complete,
                GenericTaskTerminal::Partial => &mut state.partial,
                GenericTaskTerminal::Unavailable => &mut state.unavailable,
                GenericTaskTerminal::Failed
                | GenericTaskTerminal::NotCommittedAfterEarlierFailure => &mut state.failed,
                GenericTaskTerminal::Cancelled => &mut state.cancelled,
            };
            *counter = counter
                .checked_add(1)
                .expect("Generic task 终态计数不得溢出");
            match terminal {
                GenericTaskTerminal::NotCommittedAfterEarlierFailure => {
                    state.failure_occurrence.map(|(occurrence, _)| occurrence)
                }
                _ => None,
            }
        };
        let mut occurrence = None;
        for diagnostic in diagnostics {
            let effect = diagnostic.effect();
            let id = self
                .handle
                .record_diagnostic(DiagnosticScope::TranslationTask, diagnostic);
            if occurrence.is_none() {
                occurrence = id;
            }
            if matches!(terminal, GenericTaskTerminal::Failed)
                && let Some(id) = id
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.failure_occurrence.is_none() {
                    state.failure_occurrence = Some((id, effect));
                }
            }
        }
        let outcome = match terminal {
            GenericTaskTerminal::Complete => TaskFinishedOutcome::Complete,
            GenericTaskTerminal::Partial => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Partial { diagnostic }
            }
            GenericTaskTerminal::Unavailable => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Unavailable { diagnostic }
            }
            GenericTaskTerminal::Failed => {
                let Some(diagnostic) = occurrence else {
                    return;
                };
                TaskFinishedOutcome::Failed { diagnostic }
            }
            GenericTaskTerminal::NotCommittedAfterEarlierFailure => {
                let Some(diagnostic) = earlier_failure else {
                    return;
                };
                TaskFinishedOutcome::NotCommittedAfterEarlierFailure { diagnostic }
            }
            GenericTaskTerminal::Cancelled => TaskFinishedOutcome::Cancelled,
        };
        self.handle.emit(ProjectLogEvent::TaskFinished {
            task,
            attempts: generic_count(attempts),
            outcome,
        });
    }

    fn counters(&self) -> TranslationTaskCounters {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(counters) = self.handle.translation_task_counters(state.planned) {
            return counters;
        }
        // logger 已不可用时不会再有可持久化的 JSONL；保留本地快照仅供进程终态分类，
        // 不把它当作已写入的任务事实。
        let not_started = state
            .planned
            .checked_sub(state.started)
            .expect("Generic 已开始任务数不得超过计划数");
        TranslationTaskCounters::new(
            state.planned,
            state.started,
            state.complete,
            state.partial,
            state.unavailable,
            state.failed,
            state.cancelled,
            not_started,
        )
        .expect("Generic task 日志计数必须满足状态机恒等式")
    }

    fn fail_in_flight_after_panic(&self, report: DiagnosticReport) {
        let mut in_flight = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .in_flight
            .iter()
            .copied()
            .collect::<Vec<_>>();
        in_flight.sort_unstable();
        for task_index in in_flight {
            self.finished(task_index, 0, GenericTaskTerminal::Failed, [report.clone()]);
        }
    }

    fn failure_occurrence(
        &self,
    ) -> Option<(
        crate::runtime::project_log::DiagnosticOccurrenceId,
        StateEffect,
    )> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure_occurrence
    }
}

fn generic_task_ordinal(task_index: usize) -> u64 {
    generic_count(task_index)
        .checked_add(1)
        .expect("Generic task ordinal 不得溢出")
}

fn generic_count(value: usize) -> u64 {
    u64::try_from(value).expect("当前平台 usize 必须能够无损表示为 u64")
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct GenericTaskSummary {
    started_tasks: usize,
    not_started_tasks: usize,
    complete_tasks: usize,
    partial_tasks: usize,
    unavailable_tasks: usize,
    accepted_units: usize,
    written_units: usize,
    conflicted_units: usize,
    response_problems: usize,
    recoverable_request_exhaustions: usize,
    request_admission_stopped: bool,
}

struct GenericTaskRecordDraft {
    task_index: usize,
    requested_outputs: usize,
    user_message: String,
    raw_assistant: Option<String>,
}

struct GenericTaskRecordInFlight {
    task_index: usize,
    requested_outputs: usize,
    user_message: String,
}

impl GenericTaskRecordInFlight {
    fn finish(self, raw_assistant: Option<String>) -> GenericTaskRecordDraft {
        GenericTaskRecordDraft {
            task_index: self.task_index,
            requested_outputs: self.requested_outputs,
            user_message: self.user_message,
            raw_assistant,
        }
    }
}

impl GenericTaskRecordDraft {
    fn finish(self, state: GenericTaskRecordState) -> GenericTaskRecordDocument {
        GenericTaskRecordDocument::new(
            self.task_index,
            self.requested_outputs,
            self.user_message,
            self.raw_assistant,
            state,
        )
    }
}

enum GenericPreparedTaskOutcome {
    Accepted {
        writes: Vec<TranslationWrite>,
        rejections: Vec<RejectedTranslationWrite>,
        diagnostics: Vec<DiagnosticReport>,
        accepted_units: usize,
        response_problems: usize,
        response_complete: bool,
        accepted_output_ids: Vec<usize>,
    },
    Unavailable {
        diagnostic: DiagnosticReport,
        request_exhausted: bool,
        stop_admission: bool,
    },
    Failed {
        error: GenericCommandError,
        preserve_admitted_results: bool,
    },
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
    placeholder_rule_source: GenericPlaceholderRuleSource,
    terminology: Arc<CompiledTerminology>,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: Arc<String>,
    response_mode: TranslationResponseMode,
    client: Arc<crate::runtime::llm::OpenAiCompatibleClient>,
    llm: OpenAiCompatibleExecutor,
    retry_delays: Arc<Vec<Duration>>,
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
    admission_stopped: Arc<AtomicBool>,
}

async fn execute_owned_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let render_terminology = Arc::clone(&context.terminology);
    let render_system_prompt = Arc::clone(&context.system_prompt);
    let render_cancellation = context.cancellation.clone();
    let (task, system_prompt, user_message) = context
        .cpu
        .execute(move || {
            let user_message = render_generic_user_message_with_cancellation(
                &task,
                render_terminology.as_ref(),
                &render_cancellation,
            )
            .map_err(GenericPreparationError::Planning)?;
            let system_prompt =
                clone_generic_cpu_text(render_system_prompt.as_str(), &render_cancellation)?;
            Ok::<_, GenericPreparationError>((task, system_prompt, user_message))
        })
        .await
        .map_err(generic_cpu_execution_failure)?
        .map_err(|source| {
            if source.is_cancelled() {
                GenericCommandError::Cancelled
            } else {
                generic_preparation_failure(source)
            }
        })?;
    execute_generic_task(
        context.total_tasks,
        task_index,
        task,
        user_message,
        Arc::clone(&context.facts),
        context.placeholder_rules.clone(),
        context.placeholder_rule_source.clone(),
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
        Arc::clone(&context.admission_stopped),
    )
    .await
}

async fn execute_indexed_generic_task(
    context: GenericTaskRequestContext,
    task_index: usize,
    task: PlannedTask,
) -> (usize, Result<GenericPreparedTask, GenericCommandError>) {
    let result = execute_owned_generic_task(context, task_index, task).await;
    (task_index, result)
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
        placeholder_rule_source,
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
        translate_project_log,
        progress,
    } = input;
    let total_tasks = tasks.len();
    let record_evidence = task_records.enabled();
    let concurrency = client.max_concurrent_requests().get();
    let request_context = GenericTaskRequestContext {
        total_tasks,
        facts,
        placeholder_rules,
        placeholder_rule_source,
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
        admission_stopped: Arc::new(AtomicBool::new(false)),
    };
    let mut remaining = tasks.into_iter().enumerate();
    let mut tasks = FuturesOrdered::new();
    for _ in 0..concurrency {
        let Some((task_index, task)) = remaining.next() else {
            break;
        };
        project_log.started(task_index);
        tasks.push_back(execute_indexed_generic_task(
            request_context.clone(),
            task_index,
            task,
        ));
    }

    let mut summary = GenericTaskSummary::default();
    let mut terminal_error = None;
    let mut preserve_admitted_results_after_error = false;
    let mut admission_stopped = false;
    while let Some((scheduled_task_index, prepared)) = tasks.next().await {
        let GenericPreparedTask {
            task_index,
            outcome,
            record,
            attempt_count,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let report =
                    generic_task_execution_error_report(&error, scheduled_task_index, total_tasks);
                project_log.finished(
                    scheduled_task_index,
                    0,
                    if error.is_cancelled() {
                        GenericTaskTerminal::Cancelled
                    } else {
                        GenericTaskTerminal::Failed
                    },
                    (!error.is_cancelled()).then_some(report),
                );
                if terminal_error.is_none() {
                    cancellation.request();
                    terminal_error = Some(error);
                }
                continue;
            }
        };
        if terminal_error.is_some()
            && !(preserve_admitted_results_after_error
                && matches!(&outcome, GenericPreparedTaskOutcome::Accepted { .. }))
        {
            let prior_was_cancelled = terminal_error
                .as_ref()
                .is_some_and(GenericCommandError::is_cancelled);
            match &outcome {
                GenericPreparedTaskOutcome::Cancelled => project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Cancelled,
                    std::iter::empty(),
                ),
                GenericPreparedTaskOutcome::Unavailable { diagnostic, .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Unavailable,
                    [diagnostic.clone()],
                ),
                GenericPreparedTaskOutcome::Accepted { .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    if prior_was_cancelled {
                        GenericTaskTerminal::Cancelled
                    } else {
                        GenericTaskTerminal::NotCommittedAfterEarlierFailure
                    },
                    std::iter::empty(),
                ),
                GenericPreparedTaskOutcome::Failed { error, .. } => project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Failed,
                    [generic_task_execution_error_report(
                        error,
                        task_index,
                        total_tasks,
                    )],
                ),
            }
            if let Some(record) = record {
                let state = match outcome {
                    GenericPreparedTaskOutcome::Cancelled => GenericTaskRecordState::cancelled(),
                    GenericPreparedTaskOutcome::Unavailable { diagnostic, .. } => {
                        GenericTaskRecordState::unavailable(diagnostic)
                    }
                    GenericPreparedTaskOutcome::Accepted {
                        accepted_output_ids,
                        diagnostics,
                        ..
                    } => {
                        if prior_was_cancelled {
                            GenericTaskRecordState::cancelled_after_acceptance(
                                accepted_output_ids,
                                diagnostics,
                            )
                        } else {
                            GenericTaskRecordState::not_committed_due_to_prior_failure(
                                accepted_output_ids,
                                diagnostics,
                            )
                        }
                    }
                    GenericPreparedTaskOutcome::Failed { ref error, .. } => {
                        GenericTaskRecordState::failed(generic_task_execution_error_report(
                            error,
                            task_index,
                            total_tasks,
                        ))
                    }
                };
                task_records.submit(record.finish(state));
            }
            continue;
        }
        match outcome {
            GenericPreparedTaskOutcome::Accepted {
                writes,
                rejections,
                mut diagnostics,
                accepted_units,
                response_problems,
                response_complete,
                accepted_output_ids,
            } => {
                let commit = if writes.is_empty() && rejections.is_empty() {
                    Ok(CommitTranslationResultsOutcome {
                        committed: 0,
                        rejected: 0,
                        conflicts: Vec::new(),
                    })
                } else {
                    let database_path = store.database_path().to_path_buf();
                    let store = store.clone();
                    let profile_id = profile_id.clone();
                    run_project_blocking(
                        GenericDiagnosticStage::Translate,
                        StateEffect::ProgressPreserved,
                        database_path,
                        move || {
                            store.commit_translation_results_for_profile(
                                expected_raw_fingerprint,
                                &writes,
                                &rejections,
                                &profile_id,
                            )
                        },
                    )
                    .await
                };
                let commit = match commit {
                    Ok(commit) => commit,
                    Err(error) => {
                        let report =
                            generic_task_execution_error_report(&error, task_index, total_tasks);
                        project_log.finished(
                            task_index,
                            attempt_count,
                            GenericTaskTerminal::Failed,
                            [report.clone()],
                        );
                        if let Some(record) = record {
                            diagnostics.push(report.clone());
                            task_records.submit(record.finish(
                                GenericTaskRecordState::failed_after_acceptance(
                                    accepted_output_ids,
                                    diagnostics,
                                ),
                            ));
                        }
                        cancellation.request();
                        preserve_admitted_results_after_error = false;
                        terminal_error.get_or_insert(error);
                        continue;
                    }
                };
                if commit.committed > 0 || commit.rejected > 0 {
                    mark_generic_translate_run_plan_saved(&translate_project_log);
                }
                let complete = response_complete && commit.conflicts.is_empty();
                summary.accepted_units += accepted_units;
                summary.response_problems += response_problems;
                summary.written_units += commit.committed;
                summary.conflicted_units += commit.conflicts.len();
                if complete {
                    summary.complete_tasks += 1;
                } else {
                    summary.partial_tasks += 1;
                }
                update_generic_translate_summary(&translate_project_log, |stored| {
                    stored.accepted_units += accepted_units;
                    stored.response_problems += response_problems;
                    stored.written_units += commit.committed;
                    stored.remaining_units = stored
                        .remaining_units
                        .checked_sub(commit.committed)
                        .expect("Generic 已写入模型 Unit 不得超过计划 Unit");
                    stored.conflicted_units += commit.conflicts.len();
                    if complete {
                        stored.complete_tasks += 1;
                    } else {
                        stored.partial_tasks += 1;
                    }
                });
                if !commit.conflicts.is_empty() {
                    diagnostics.push(generic_task_response_diagnostic(
                        task_index,
                        total_tasks,
                        GenericTaskResponseProblem::CommitConflict {
                            count: generic_count(commit.conflicts.len()),
                        },
                    ));
                }
                let task_record_diagnostics = diagnostics.clone();
                project_log.finished(
                    task_index,
                    attempt_count,
                    if complete {
                        GenericTaskTerminal::Complete
                    } else {
                        GenericTaskTerminal::Partial
                    },
                    diagnostics,
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::committed(
                        response_complete,
                        accepted_output_ids,
                        commit.committed,
                        task_record_diagnostics,
                    )));
                }
            }
            GenericPreparedTaskOutcome::Unavailable {
                diagnostic,
                request_exhausted,
                stop_admission,
            } => {
                project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Unavailable,
                    [diagnostic.clone()],
                );
                if let Some(record) = record {
                    task_records
                        .submit(record.finish(GenericTaskRecordState::unavailable(diagnostic)));
                }
                summary.unavailable_tasks += 1;
                summary.recoverable_request_exhaustions += usize::from(request_exhausted);
                summary.request_admission_stopped |= stop_admission;
                admission_stopped |= stop_admission;
                update_generic_translate_summary(&translate_project_log, |stored| {
                    stored.unavailable_tasks += 1;
                    stored.recoverable_request_exhaustions += usize::from(request_exhausted);
                    stored.request_admission_stopped |= stop_admission;
                });
            }
            GenericPreparedTaskOutcome::Failed {
                error,
                preserve_admitted_results,
            } => {
                let diagnostic =
                    generic_task_execution_error_report(&error, task_index, total_tasks);
                project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Failed,
                    [diagnostic.clone()],
                );
                if let Some(record) = record {
                    task_records.submit(record.finish(GenericTaskRecordState::failed(diagnostic)));
                }
                if !preserve_admitted_results {
                    cancellation.request();
                } else {
                    summary.request_admission_stopped = true;
                    admission_stopped = true;
                    update_generic_translate_summary(&translate_project_log, |stored| {
                        stored.request_admission_stopped = true;
                    });
                }
                preserve_admitted_results_after_error = preserve_admitted_results;
                terminal_error = Some(error);
            }
            GenericPreparedTaskOutcome::Cancelled => {
                project_log.finished(
                    task_index,
                    attempt_count,
                    GenericTaskTerminal::Cancelled,
                    std::iter::empty(),
                );
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
                generic_count(confirmed),
                generic_count(total_tasks),
            ));
        }
        if terminal_error.is_none()
            && !admission_stopped
            && !request_context.admission_stopped.load(Ordering::Acquire)
            && let Some((task_index, task)) = remaining.next()
        {
            project_log.started(task_index);
            tasks.push_back(execute_indexed_generic_task(
                request_context.clone(),
                task_index,
                task,
            ));
        }
    }
    match terminal_error {
        Some(error) => Err(error),
        None => {
            summary.started_tasks =
                summary.complete_tasks + summary.partial_tasks + summary.unavailable_tasks;
            summary.not_started_tasks = total_tasks.saturating_sub(summary.started_tasks);
            Ok(summary)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_generic_task(
    total_tasks: usize,
    task_index: usize,
    task: PlannedTask,
    user_message: String,
    facts: Arc<GenericUnitMap<GenericValidationFact>>,
    placeholder_rules: GenericCompiledPlaceholderRules,
    placeholder_rule_source: GenericPlaceholderRuleSource,
    language_module: Arc<dyn LanguageModule>,
    system_prompt: String,
    response_mode: TranslationResponseMode,
    client: &crate::runtime::llm::OpenAiCompatibleClient,
    llm: &OpenAiCompatibleExecutor,
    retry_delays: &[Duration],
    max_retry_after: Duration,
    cpu: RayonCpuExecutor,
    cancellation: CooperativeCancellation,
    record_evidence: bool,
    admission_stopped: Arc<AtomicBool>,
) -> Result<GenericPreparedTask, GenericCommandError> {
    let requested_outputs = task.expected_output_count();
    let recorded_user_message = record_evidence.then(|| user_message.clone());
    let messages = [
        ChatMessage::new(ChatMessageRole::System, system_prompt),
        ChatMessage::new(ChatMessageRole::User, user_message),
    ];
    let execution = execute_llm_request_with_retry(
        llm,
        client,
        &messages,
        LlmRequestRetryPolicy::new(retry_delays, max_retry_after),
        &TokioDelay,
        &cancellation,
    )
    .await;
    let (outcome, evidence) = execution.into_parts();
    let stops_admission = match &outcome {
        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum { service_status, .. }
        | LlmRequestExecutionOutcome::RetryBudgetExhausted { service_status, .. } => {
            service_status.stops_admission_after_unavailable()
        }
        LlmRequestExecutionOutcome::Fatal { source, .. } => source.service_status().is_permanent(),
        LlmRequestExecutionOutcome::AdmissionStopped { .. } => true,
        LlmRequestExecutionOutcome::Response { .. }
        | LlmRequestExecutionOutcome::Cancelled { .. } => false,
    };
    if stops_admission {
        admission_stopped.store(true, Ordering::Release);
    }
    let attempt_count = evidence.attempt_count();
    let record = recorded_user_message.map(|user_message| GenericTaskRecordInFlight {
        task_index,
        requested_outputs,
        user_message,
    });
    let response_cancellation = cancellation.clone();
    let processing = cpu
        .execute(move || {
            ensure_generic_response_processing_running(&response_cancellation)?;
            let mut response_record = None;
            let outcome = match outcome {
                LlmRequestExecutionOutcome::Response { response, .. } => {
                    let (content, finish_reason) = response.into_content_and_finish_reason();
                    if record_evidence {
                        response_record = Some(content.clone());
                    }
                    ensure_generic_response_processing_running(&response_cancellation)?;
                    let finish_review =
                        (!matches!(finish_reason, LlmFinishReason::Stop)).then(|| {
                            generic_task_response_diagnostic(
                                task_index,
                                total_tasks,
                                GenericTaskResponseProblem::ResponseReview {
                                    finding: GenericResponseReviewFinding::NonStopFinish,
                                },
                            )
                        });
                    match parse_translation_response_with_cancellation(
                        &content,
                        response_mode,
                        || ensure_generic_response_processing_running(&response_cancellation),
                    )? {
                        Ok(parsed) => {
                            ensure_generic_response_processing_running(&response_cancellation)?;
                            let acceptance = accept_generic_response_with_cancellation(
                                task,
                                &parsed,
                                facts.as_ref(),
                                &placeholder_rules,
                                &placeholder_rule_source,
                                language_module.as_ref(),
                                &response_cancellation,
                            )?;
                            let accepted_output_ids = acceptance
                                .accepted_output_ids()
                                .iter()
                                .map(|id| id.get())
                                .collect();
                            let (accepted, rejected, problems, reviews) = acceptance.into_parts();
                            let accepted_units = accepted.len();
                            let response_problems = problems.len();
                            let response_complete = problems.is_empty();
                            let mut diagnostics = Vec::with_capacity(
                                problems.len()
                                    + reviews.len()
                                    + usize::from(finish_review.is_some()),
                            );
                            if let Some(finish_review) = finish_review {
                                diagnostics.push(finish_review);
                            }
                            for problem in &problems {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                diagnostics.push(generic_response_problem_diagnostic(
                                    task_index,
                                    total_tasks,
                                    problem,
                                ));
                            }
                            for review in &reviews {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                diagnostics.push(generic_response_review_diagnostic(
                                    task_index,
                                    total_tasks,
                                    review,
                                ));
                            }
                            let mut writes = Vec::with_capacity(accepted.len());
                            for accepted in accepted {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                writes.push(accepted.into_write());
                            }
                            let mut rejections = Vec::with_capacity(rejected.len());
                            for rejected in rejected {
                                ensure_generic_response_processing_running(&response_cancellation)?;
                                rejections.push(rejected.into_write());
                            }
                            GenericPreparedTaskOutcome::Accepted {
                                writes,
                                rejections,
                                diagnostics,
                                accepted_units,
                                response_problems,
                                response_complete,
                                accepted_output_ids,
                            }
                        }
                        Err(error) => {
                            ensure_generic_response_processing_running(&response_cancellation)?;
                            let diagnostic =
                                generic_response_parse_diagnostic(task_index, total_tasks, error);
                            GenericPreparedTaskOutcome::Unavailable {
                                diagnostic,
                                request_exhausted: false,
                                stop_admission: false,
                            }
                        }
                    }
                }
                LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
                    diagnostic,
                    service_status,
                    ..
                } => GenericPreparedTaskOutcome::Unavailable {
                    diagnostic,
                    request_exhausted: true,
                    stop_admission: service_status.stops_admission_after_unavailable(),
                },
                LlmRequestExecutionOutcome::RetryBudgetExhausted {
                    diagnostic,
                    service_status,
                    ..
                } => GenericPreparedTaskOutcome::Unavailable {
                    diagnostic,
                    request_exhausted: true,
                    stop_admission: service_status.stops_admission_after_unavailable(),
                },
                LlmRequestExecutionOutcome::Fatal {
                    source, diagnostic, ..
                } => {
                    let preserve_admitted_results = source.service_status().is_permanent();
                    GenericPreparedTaskOutcome::Failed {
                        error: GenericCommandError::reported(source, diagnostic),
                        preserve_admitted_results,
                    }
                }
                LlmRequestExecutionOutcome::AdmissionStopped { diagnostic, .. } => {
                    GenericPreparedTaskOutcome::Unavailable {
                        diagnostic,
                        request_exhausted: false,
                        stop_admission: true,
                    }
                }
                LlmRequestExecutionOutcome::Cancelled { .. } => {
                    GenericPreparedTaskOutcome::Cancelled
                }
            };
            Ok::<_, GenericPreparationError>((outcome, response_record))
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
            let error = generic_cpu_execution_failure(source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed {
                    error,
                    preserve_admitted_results: false,
                },
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
            let error = generic_preparation_failure(source);
            return Ok(GenericPreparedTask {
                task_index,
                outcome: GenericPreparedTaskOutcome::Failed {
                    error,
                    preserve_admitted_results: false,
                },
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
    mut validator: impl FnMut(
        &GenericValidationFact,
        &str,
    ) -> Result<String, GenericResponseDestinationProblem>,
) -> TranslationAcceptance {
    accept_generic_response_with_validator_and_cancellation(
        task,
        parsed,
        facts,
        |fact, candidate| {
            Ok::<_, GenericPreparationError>(
                validator(fact, candidate).map(ValidatedCandidate::clean),
            )
        },
        &CooperativeCancellation::default(),
    )
    .expect("不取消的受信 Generic 响应必须完成验收")
}

fn accept_generic_response_with_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPreparationError> {
    accept_generic_response_with_validator_and_cancellation(
        task,
        parsed,
        facts,
        |fact, candidate| {
            validate_generic_candidate_fact_with_cancellation(
                fact,
                candidate,
                placeholder_rules,
                placeholder_rule_source,
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
    ) -> Result<
        Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
        GenericPreparationError,
    >,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPreparationError> {
    let mut cache = HashMap::<
        TaskId,
        CancellableTextMap<
            &str,
            Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
        >,
    >::new();
    let mut reviews = Vec::new();
    let mut acceptance =
        accept_parsed_response_with_cancellation(
            task,
            parsed,
            |output_id,
             key,
             candidate|
             -> Result<
                Result<String, GenericResponseDestinationProblem>,
                GenericPreparationError,
            > {
                ensure_generic_response_processing_running(cancellation)?;
                let Some(fact) = facts.get_with_cancellation(key, || {
                    ensure_generic_response_processing_running(cancellation)
                })?
                else {
                    return Ok(Err(GenericResponseDestinationProblem::MissingPlanningFact));
                };
                let output_cache = cache
                    .entry(output_id)
                    .or_insert_with(|| CancellableTextMap::with_capacity(1));
                let validated = if let Some(cached) = output_cache
                    .get_with_cancellation(fact.kind.as_str(), || {
                        ensure_generic_response_processing_running(cancellation)
                    })? {
                    clone_generic_validation_result(cached, cancellation)?
                } else {
                    // 一个 output_id 只对应一个全局去重族；同族的原文、保护后文本和实际
                    // Placeholder 绑定相同。kind 仍会改变 scope，因此必须分别验收。
                    let validated = validator(fact, candidate)?;
                    let returned = clone_generic_validation_result(&validated, cancellation)?;
                    let previous = output_cache.insert_with_cancellation(
                        fact.kind.as_str(),
                        validated,
                        || ensure_generic_response_processing_running(cancellation),
                    )?;
                    debug_assert!(previous.is_none());
                    returned
                };
                match validated {
                    Ok(validated) => {
                        let (translation, findings) = validated.into_parts();
                        for finding in findings {
                            reviews.push(TranslationReview::new(
                                output_id,
                                GenericPlanningUnitLocator::new(
                                    &fact.locator.relative_path,
                                    fact.locator.group_id.clone(),
                                    fact.locator.unit_id.clone(),
                                    fact.locator.role.clone(),
                                )
                                .with_natural_position(fact.locator.line, fact.locator.unit),
                                finding,
                            ));
                        }
                        Ok(Ok(translation))
                    }
                    Err(problem) => Ok(Err(problem)),
                }
            },
            || cancellation.is_requested(),
        )?;
    acceptance.append_reviews(&mut reviews);
    Ok(acceptance)
}

#[cfg(test)]
fn validate_generic_candidate(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<ValidatedCandidate<String>, GenericResponseDestinationProblem> {
    let fact = facts
        .get_with_cancellation(key, || Ok::<_, std::convert::Infallible>(()))
        .unwrap_or_else(|never| match never {})
        .ok_or(GenericResponseDestinationProblem::MissingPlanningFact)?;
    validate_generic_candidate_fact(fact, candidate, placeholder_rules, language_module)
}

fn validate_generic_reuse_with_cancellation(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<Result<ValidatedReuse, GenericResponseDestinationProblem>, GenericPreparationError> {
    ensure_generic_response_processing_running(cancellation)?;
    let Some(fact) = facts.get_with_cancellation(key, || {
        ensure_generic_response_processing_running(cancellation)
    })?
    else {
        return Ok(Err(GenericResponseDestinationProblem::MissingPlanningFact));
    };
    let final_translation = match validate_generic_candidate_fact_with_cancellation(
        fact,
        candidate,
        placeholder_rules,
        placeholder_rule_source,
        language_module,
        cancellation,
    )? {
        Ok(translation) => translation.into_parts().0,
        Err(problem) => return Ok(Err(problem)),
    };
    let service = GenericPlaceholderService::default();
    let target_id = fact.locator.readable_id();
    let context = match service.protect_target_with_cancellation(
        &target_id,
        &fact.kind,
        &final_translation,
        placeholder_rules,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(context) => context,
        Err(source) => {
            return Ok(Err(generic_candidate_placeholder_problem(
                source,
                placeholder_rule_source,
                &fact.locator,
            )?));
        }
    };
    let context_binding = context.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    let expected_binding = fact.protected.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    if context_binding != expected_binding {
        return Ok(Err(
            GenericResponseDestinationProblem::PlaceholderBindingMismatch,
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
) -> Result<ValidatedCandidate<String>, GenericResponseDestinationProblem> {
    validate_generic_candidate_fact_with_cancellation(
        fact,
        candidate,
        placeholder_rules,
        &GenericPlaceholderRuleSource::ProjectSnapshot,
        language_module,
        &CooperativeCancellation::default(),
    )
    .expect("不取消的候选验收必须完成")
}

fn validate_generic_candidate_fact_with_cancellation(
    fact: &GenericValidationFact,
    candidate: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<
    Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    GenericPreparationError,
> {
    ensure_generic_response_processing_running(cancellation)?;
    let service = GenericPlaceholderService::default();
    let restored = match service.restore_with_cancellation(&fact.protected, candidate, || {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(restored) => restored,
        Err(source) => return Ok(Err(generic_response_placeholder_problem(&source))),
    };
    let target_id = fact.locator.readable_id();
    let candidate_protected = match service.protect_target_with_cancellation(
        &target_id,
        &fact.kind,
        &restored,
        placeholder_rules,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(protected) => protected,
        Err(source) => {
            return Ok(Err(generic_candidate_placeholder_problem(
                source,
                placeholder_rule_source,
                &fact.locator,
            )?));
        }
    };
    let language_text = match candidate_protected.language_text_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(text) => text,
        Err(source) => {
            return Ok(Err(GenericResponseDestinationProblem::LanguageProjection {
                problem: generic_language_projection_problem(&source),
            }));
        }
    };
    let residual = match language_module.find_source_residual_with_cancellation(
        &fact.analysis,
        &language_text,
        &mut || ensure_generic_language_running(cancellation),
    ) {
        Ok(Ok(residual)) => residual,
        Ok(Err(_)) => {
            return Ok(Err(
                GenericResponseDestinationProblem::LanguageAnalysisMismatch,
            ));
        }
        Err(LanguageOperationCancelled) => {
            return Err(GenericPlanningError::Cancelled.into());
        }
    };
    let review = residual.is_some().then_some(ReviewFinding::SourceResidual);
    ensure_generic_response_processing_running(cancellation)?;
    let final_translation = match rebuild_original_placeholders_with_cancellation(
        &candidate_protected,
        &language_text,
        cancellation,
    )? {
        Ok(translation) => translation,
        Err(problem) => return Ok(Err(problem)),
    };
    match crate::generic::validate_translation_placeholders_with_cancellation(
        &service,
        placeholder_rules,
        &target_id,
        &fact.kind,
        &fact.source_text,
        &final_translation,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(()) => {}
        Err(source) => {
            return Ok(Err(generic_candidate_placeholder_problem(
                source,
                placeholder_rule_source,
                &fact.locator,
            )?));
        }
    }
    if contains_reserved_prefix_with_cancellation(&final_translation, cancellation)? {
        return Ok(Err(GenericResponseDestinationProblem::ReservedToken));
    }
    ensure_generic_response_processing_running(cancellation)?;
    Ok(Ok(match review {
        Some(finding) => ValidatedCandidate::with_review(final_translation, finding),
        None => ValidatedCandidate::clean(final_translation),
    }))
}

fn rebuild_original_placeholders_with_cancellation(
    protected: &GenericProtectedText,
    repaired: &LanguageText,
    cancellation: &CooperativeCancellation,
) -> Result<Result<String, GenericResponseDestinationProblem>, GenericPlanningError> {
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
                    return Ok(Err(
                        GenericResponseDestinationProblem::PlaceholderBoundaryAdded,
                    ));
                };
                append_generic_response_text(&mut output, placeholder.original(), cancellation)?;
            }
        }
    }
    ensure_generic_response_processing_running(cancellation)?;
    if placeholders.next().is_some() {
        return Ok(Err(
            GenericResponseDestinationProblem::PlaceholderBoundaryRemoved,
        ));
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
    result: &Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    cancellation: &CooperativeCancellation,
) -> Result<
    Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    GenericPlanningError,
> {
    let mut cloned = String::new();
    match result {
        Ok(value) => {
            append_generic_response_text(&mut cloned, value.value(), cancellation)?;
            let cloned = match value.reviews() {
                [] => ValidatedCandidate::clean(cloned),
                [finding] => ValidatedCandidate::with_review(cloned, finding.clone()),
                _ => unreachable!("当前候选验收每个目标最多产生一个 Review"),
            };
            Ok(Ok(cloned))
        }
        Err(problem) => {
            ensure_generic_response_processing_running(cancellation)?;
            Ok(Err(problem.clone()))
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
    summary.started_tasks += tasks.started_tasks;
    summary.not_started_tasks += tasks.not_started_tasks;
    summary.complete_tasks += tasks.complete_tasks;
    summary.partial_tasks += tasks.partial_tasks;
    summary.unavailable_tasks += tasks.unavailable_tasks;
    summary.accepted_units += tasks.accepted_units;
    summary.written_units += tasks.written_units;
    summary.conflicted_units += tasks.conflicted_units;
    summary.response_problems += tasks.response_problems;
    summary.remaining_units = summary
        .remaining_units
        .checked_sub(tasks.written_units)
        .expect("Generic 已写入模型 Unit 不得超过计划 Unit");
    summary.recoverable_request_exhaustions += tasks.recoverable_request_exhaustions;
    summary.request_admission_stopped |= tasks.request_admission_stopped;
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
                generic_read_file_failure(source, FileSystemDiagnosticStage::CommandPreparation)
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

fn generic_file_system_build_failure(source: SystemFileSystemBuildError) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
    GenericCommandError::reported(source, report)
}

fn generic_cpu_start_failure(source: CpuExecutorStartError) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
    GenericCommandError::reported(source, report)
}

type GenericProjectLogSlot = Arc<Mutex<Option<ActiveProjectLog>>>;

#[derive(Default)]
struct GenericTranslateProjectLogState {
    database_path: Option<PathBuf>,
    run_plan_resolved: bool,
    run_plan_finalized: bool,
    translation_finished: bool,
    run_plan_saved: bool,
    summary: Option<GenericTranslationSummary>,
    tasks: Option<GenericTaskProjectLog>,
    active_phase: Option<ProjectLogPhase>,
}

#[derive(Default)]
struct GenericExtractProjectLogState {
    database_path: Option<PathBuf>,
    run_plan_resolved: bool,
    run_plan_finalized: bool,
    phase_started: bool,
}

type GenericExtractProjectLogStateRef = Arc<Mutex<GenericExtractProjectLogState>>;

type GenericTranslateProjectLogStateRef = Arc<Mutex<GenericTranslateProjectLogState>>;
#[derive(Clone, Copy)]
struct GenericTerminalOccurrence {
    diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
    outcome: GenericTerminalRunOutcome,
}

#[derive(Clone, Copy)]
enum GenericTerminalRunOutcome {
    FromEffect(StateEffect),
    RecoveryRequired,
}

impl GenericTerminalOccurrence {
    const fn from_effect(
        diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
        effect: StateEffect,
    ) -> Self {
        Self {
            diagnostic,
            outcome: GenericTerminalRunOutcome::FromEffect(effect),
        }
    }

    const fn recovery_required(
        diagnostic: crate::runtime::project_log::DiagnosticOccurrenceId,
    ) -> Self {
        Self {
            diagnostic,
            outcome: GenericTerminalRunOutcome::RecoveryRequired,
        }
    }

    fn into_pending(self, project_log: ActiveProjectLog) -> PendingProjectLog {
        match self.outcome {
            GenericTerminalRunOutcome::FromEffect(effect) => {
                project_log.pending_failure_with_occurrence(effect, self.diagnostic)
            }
            GenericTerminalRunOutcome::RecoveryRequired => {
                project_log.pending_recovery_required_with_occurrence(self.diagnostic)
            }
        }
    }
}
type GenericTerminalOccurrenceSlot = Arc<Mutex<Option<GenericTerminalOccurrence>>>;

fn generic_translate_project_log_state() -> GenericTranslateProjectLogStateRef {
    Arc::new(Mutex::new(GenericTranslateProjectLogState::default()))
}

fn generic_terminal_occurrence_slot() -> GenericTerminalOccurrenceSlot {
    Arc::new(Mutex::new(None))
}

fn generic_extract_project_log_state() -> GenericExtractProjectLogStateRef {
    Arc::new(Mutex::new(GenericExtractProjectLogState::default()))
}

fn generic_project_log_slot() -> GenericProjectLogSlot {
    Arc::new(Mutex::new(None))
}

fn start_existing_generic_project_log(
    slot: &GenericProjectLogSlot,
    common: &crate::application::config::CommonCommandConfiguration,
    locale: UiLocale,
    project: &ProjectName,
    command: ProjectLogCommand,
    performance: Arc<RunPerformanceCounters>,
) {
    let workspace = generic_workspace(common.projects_root(), project);
    // Path::is_dir 会把权限、I/O 和“同名普通文件”都压成 false，随后项目打开错误
    // 便失去本次运行的 JSONL。仅在明确不存在项目时不建立日志；其余情况让标准日志
    // 建立路径记录可观察的失败。
    if matches!(std::fs::metadata(&workspace), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return;
    }
    install_generic_project_log(
        slot,
        start_command_log(CommandLogStart {
            common,
            locale,
            engine: ProjectLogEngine::Generic,
            project: project.as_str(),
            command,
            performance,
            selected_api_key_redactor: None,
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

fn generic_project_log_handle(slot: &GenericProjectLogSlot) -> Option<ProjectLogHandle> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|project_log| project_log.handle().clone())
}

fn select_generic_project_log_api_key_redactor(
    slot: &GenericProjectLogSlot,
    redactor: Arc<ApiKeyRedactor>,
) {
    if let Some(project_log) = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        project_log.select_api_key_redactor(redactor);
    }
}

/// 所有 Generic 命令经过同一条合作取消路径。此时不把尚未确认的工作伪造为完成量；
/// logger 负责压缩重复信号，因此每次运行最多持久化一条 run.cancel_requested。
fn emit_generic_cancellation_requested(project_log: &GenericProjectLogSlot) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::CancellationRequested {
            confirmed: 0,
            total: None,
        });
    }
}

fn start_generic_extract_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_started(
            ProjectLogPhase::ScanSource,
            ProjectLogAmount::Indeterminate,
        ));
    }
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .phase_started = true;
}

fn resolve_generic_extract_run_plan(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
    database_path: &Path,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::RunPlanResolved {
            plan: ResolvedRunPlan::generic_extract(RunPlanValueSource::ProductDefault),
        });
    }
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.database_path = Some(database_path.to_path_buf());
    state.run_plan_resolved = true;
}

fn finish_generic_extract_success(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
) {
    let Some(handle) = generic_project_log_handle(project_log) else {
        return;
    };
    let (database_path, complete_phase, finalize) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let database_path = state.database_path.clone();
        let complete_phase = std::mem::take(&mut state.phase_started);
        let finalize = state.run_plan_resolved && !state.run_plan_finalized;
        state.run_plan_finalized |= finalize;
        (database_path, complete_phase, finalize)
    };
    if complete_phase {
        handle.emit(ProjectLogEvent::phase_completed(
            ProjectLogPhase::ScanSource,
            ProjectLogAmount::Indeterminate,
        ));
    }
    if finalize {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("Generic Extract run plan 必须保存数据库路径"),
            ),
            result: RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            },
        });
    }
}

fn finish_generic_extract_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericExtractProjectLogStateRef,
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTerminalOccurrence> {
    if matches!(
        driven,
        Driven::Finished(Ok(GenericCommandOutput::Extract { .. }))
            | Driven::Interrupted(Ok(GenericCommandOutput::Extract { .. }))
    ) {
        finish_generic_extract_success(project_log, state);
        return None;
    }
    let handle = generic_project_log_handle(project_log)?;
    let cancelled = match driven {
        Driven::CancellationWon(_) => true,
        Driven::Finished(Err(error)) | Driven::Interrupted(Err(error)) => error.is_cancelled(),
        Driven::Finished(Ok(_)) | Driven::Interrupted(Ok(_)) | Driven::SignalFailed { .. } => false,
    };
    let report = match driven {
        Driven::Finished(Err(error))
        | Driven::Interrupted(Err(error))
        | Driven::CancellationWon(Err(error)) => generic_command_error_report(error),
        Driven::SignalFailed { source, .. } => DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::TerminationSignals,
                operation: RuntimeOperation::ReceiveTerminationSignal,
                failure: IoFailure::from_error(source),
            }),
        ),
        Driven::CancellationWon(Ok(_)) => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        Driven::Finished(Ok(_)) | Driven::Interrupted(Ok(_)) => return None,
    };
    let (database_path, resolved, finalized, phase_started) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let values = (
            state.database_path.clone(),
            state.run_plan_resolved,
            state.run_plan_finalized,
            std::mem::take(&mut state.phase_started),
        );
        state.run_plan_finalized |= state.run_plan_resolved;
        values
    };
    let effect = report.effect();
    // 同一失败若还要表达 run_plan.finalized，必须以 RunPlan occurrence 引用；
    // phase.stopped 不要求专属 scope，因此可安全复用这个主诊断。
    let diagnostic = handle.record_diagnostic(
        if resolved {
            DiagnosticScope::RunPlan
        } else {
            DiagnosticScope::Extract
        },
        report,
    )?;
    if phase_started {
        handle.emit(ProjectLogEvent::phase_stopped(
            ProjectLogPhase::ScanSource,
            if cancelled {
                PhaseStopOutcome::Cancelled
            } else {
                PhaseStopOutcome::Failed { diagnostic }
            },
        ));
    }
    if resolved && !finalized {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("Generic Extract run plan 必须保存数据库路径"),
            ),
            result: if effect == StateEffect::OutcomeUnknown {
                RunPlanFinalization::OutcomeUnknown {
                    transaction: RunPlanTransactionState::OutcomeUnknown,
                    run_continues: false,
                    diagnostic,
                }
            } else {
                RunPlanFinalization::NotSaved {
                    transaction: RunPlanTransactionState::RolledBack,
                    run_continues: false,
                    diagnostic,
                }
            },
        });
    }
    Some(GenericTerminalOccurrence::from_effect(diagnostic, effect))
}

fn start_generic_translate_phase(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    phase: ProjectLogPhase,
    amount: ProjectLogAmount,
) {
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_started(phase, amount));
    }
    let previous = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .replace(phase);
    assert!(previous.is_none(), "开始新阶段前必须显式结束上一阶段");
}

fn complete_generic_translate_phase(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    phase: ProjectLogPhase,
    amount: ProjectLogAmount,
) {
    let active = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .take();
    assert_eq!(active, Some(phase), "只能完成当前活动阶段");
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::phase_completed(phase, amount));
    }
}

fn generic_task_project_log(
    slot: &GenericProjectLogSlot,
    total_tasks: usize,
) -> GenericTaskProjectLog {
    let project_log = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project_log = project_log
        .as_ref()
        .expect("Generic Translate 必须在建立模型任务前建立项目日志");
    GenericTaskProjectLog::new(project_log.handle().clone(), total_tasks)
}

fn resolve_generic_translate_run_plan(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    database_path: &Path,
    source: RunPlanValueSource,
    profile_id: &str,
    terminology_path: Option<&Path>,
    placeholder_rules_path: Option<&Path>,
) {
    let plan =
        ResolvedRunPlan::translate(source, profile_id, terminology_path, placeholder_rules_path)
            .expect("已解析的 Generic Profile ID 必须可用于项目日志");
    if let Some(handle) = generic_project_log_handle(project_log) {
        handle.emit(ProjectLogEvent::RunPlanResolved { plan });
    }
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.database_path = Some(database_path.to_path_buf());
    state.run_plan_resolved = true;
}

fn install_generic_translate_task_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    total_tasks: usize,
) -> GenericTaskProjectLog {
    let tasks = generic_task_project_log(project_log, total_tasks);
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tasks = Some(tasks.clone());
    tasks
}

fn mark_generic_translate_run_plan_saved(state: &GenericTranslateProjectLogStateRef) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_plan_saved = true;
}

fn set_generic_translate_summary(
    state: &GenericTranslateProjectLogStateRef,
    summary: GenericTranslationSummary,
) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summary = Some(summary);
}

fn update_generic_translate_summary(
    state: &GenericTranslateProjectLogStateRef,
    update: impl FnOnce(&mut GenericTranslationSummary),
) {
    if let Some(summary) = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summary
        .as_mut()
    {
        update(summary);
    }
}

fn project_log_generic_translation_summary(
    summary: GenericTranslationSummary,
) -> ProjectLogGenericTranslationSummary {
    ProjectLogGenericTranslationSummary {
        planned_units: generic_count(summary.planned_units),
        remaining_units: generic_count(summary.remaining_units),
        cleared_units: generic_count(summary.cleared_units),
        reused_units: generic_count(summary.reused_units),
        accepted_units: generic_count(summary.accepted_units),
        written_units: generic_count(summary.written_units),
        conflicted_units: generic_count(summary.conflicted_units),
        response_problems: generic_count(summary.response_problems),
        recoverable_request_exhaustions: generic_count(summary.recoverable_request_exhaustions),
        request_admission_stopped: summary.request_admission_stopped,
    }
}

fn generic_terminal_translation_summary(
    state: &GenericTranslateProjectLogStateRef,
) -> Option<TranslationTerminalSummary> {
    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let summary = state.summary?;
    let tasks = state.tasks.as_ref().map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须有效"),
        GenericTaskProjectLog::counters,
    );
    Some(TranslationTerminalSummary {
        tasks,
        engine: TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary)),
    })
}

fn finish_generic_translate_success(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    summary: GenericTranslationSummary,
) {
    let Some(handle) = generic_project_log_handle(project_log) else {
        return;
    };
    let (database_path, tasks, should_finalize) = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let database_path = state.database_path.clone();
        let tasks = state.tasks.clone();
        let should_finalize = state.run_plan_resolved && !state.run_plan_finalized;
        state.run_plan_finalized |= should_finalize;
        state.translation_finished = true;
        (database_path, tasks, should_finalize)
    };
    if should_finalize {
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(
                database_path
                    .as_ref()
                    .expect("已解析的 Generic run plan 必须保存数据库路径"),
            ),
            result: RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            },
        });
    }
    let tasks = tasks.map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须满足恒等式"),
        |tasks| tasks.counters(),
    );
    let engine_summary =
        TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary));
    let result = if summary.is_incomplete() {
        TranslationFinished::Incomplete {
            tasks,
            summary: engine_summary,
        }
    } else if tasks.planned == 0 {
        TranslationFinished::NoWork {
            tasks,
            summary: engine_summary,
        }
    } else {
        TranslationFinished::Complete {
            tasks,
            summary: engine_summary,
        }
    };
    handle.emit(ProjectLogEvent::TranslationFinished { result });
}

fn finish_generic_translate_project_log(
    project_log: &GenericProjectLogSlot,
    state: &GenericTranslateProjectLogStateRef,
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTerminalOccurrence> {
    if let Some(summary) = generic_translate_success_summary(driven) {
        finish_generic_translate_success(project_log, state, summary);
        return None;
    }
    let handle = generic_project_log_handle(project_log)?;
    let cancelled = generic_translate_was_cancelled(driven);
    let error = generic_translate_driven_error(driven);
    let (database_path, resolved, finalized, run_plan_saved, summary, tasks, already_finished) = {
        let state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.database_path.clone(),
            state.run_plan_resolved,
            state.run_plan_finalized,
            state.run_plan_saved,
            state.summary,
            state.tasks.clone(),
            state.translation_finished,
        )
    };
    if already_finished {
        return None;
    }

    let task_occurrence = tasks
        .as_ref()
        .and_then(GenericTaskProjectLog::failure_occurrence);
    let report = match (error, driven) {
        (Some(error), _) => generic_command_error_report(error),
        (
            None,
            Driven::SignalFailed {
                source,
                result: Ok(_),
            },
        ) => DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::TerminationSignals,
                operation: RuntimeOperation::ReceiveTerminationSignal,
                failure: IoFailure::from_error(source),
            }),
        ),
        (None, _) => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
    };
    let report_effect = report.effect();
    let occurrence = task_occurrence.or_else(|| {
        handle
            .record_diagnostic(
                if resolved
                    && tasks
                        .as_ref()
                        .is_none_or(|tasks| tasks.counters().started == 0)
                {
                    DiagnosticScope::RunPlan
                } else {
                    DiagnosticScope::Run
                },
                report.clone(),
            )
            .map(|id| (id, report_effect))
    });

    let active_phase = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_phase
        .take();
    if let Some(phase) = active_phase {
        let outcome = if cancelled {
            PhaseStopOutcome::Cancelled
        } else if let Some((diagnostic, _)) = occurrence {
            PhaseStopOutcome::Failed { diagnostic }
        } else {
            return None;
        };
        handle.emit(ProjectLogEvent::phase_stopped(phase, outcome));
    }

    let plan_occurrence = if resolved && !finalized && !run_plan_saved {
        let no_task_started = tasks
            .as_ref()
            .is_none_or(|tasks| tasks.counters().started == 0);
        if no_task_started {
            occurrence
        } else {
            // task.finished 和 run_plan.finalized 各自拥有不同 scope；不能把同一个
            // TranslationTask occurrence 偷渡给运行方案终态。
            handle
                .record_diagnostic(DiagnosticScope::RunPlan, report.clone())
                .map(|id| (id, report_effect))
        }
    } else {
        occurrence
    };
    if resolved
        && !finalized
        && let (Some(database_path), Some((diagnostic, effect))) =
            (database_path.as_ref(), plan_occurrence)
    {
        let result = if run_plan_saved {
            RunPlanFinalization::Saved {
                transaction: RunPlanTransactionState::Committed,
                run_continues: false,
            }
        } else if effect == StateEffect::OutcomeUnknown {
            RunPlanFinalization::OutcomeUnknown {
                transaction: RunPlanTransactionState::OutcomeUnknown,
                run_continues: false,
                diagnostic,
            }
        } else {
            RunPlanFinalization::NotSaved {
                transaction: RunPlanTransactionState::NotStarted,
                run_continues: false,
                diagnostic,
            }
        };
        handle.emit(ProjectLogEvent::RunPlanFinalized {
            database: crate::diagnostic::SafePath::new(database_path),
            result,
        });
    }

    let counters = tasks.as_ref().map_or_else(
        || TranslationTaskCounters::new(0, 0, 0, 0, 0, 0, 0, 0).expect("零任务汇总必须满足恒等式"),
        GenericTaskProjectLog::counters,
    );
    let result = if cancelled {
        TranslationFinished::Cancelled {
            tasks: counters,
            summary: summary.map(|summary| {
                TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary))
            }),
        }
    } else if let Some((diagnostic, _)) = occurrence {
        TranslationFinished::Failed {
            tasks: counters,
            summary: summary.map(|summary| {
                TranslationEngineSummary::Generic(project_log_generic_translation_summary(summary))
            }),
            diagnostic,
        }
    } else {
        // logger 无法登记诊断时，公共句柄已经记录了独立的日志契约故障；此时不再
        // 构造一个引用未知 occurrence 的翻译终态。
        return None;
    };
    handle.emit(ProjectLogEvent::TranslationFinished { result });
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.run_plan_finalized |= resolved;
    state.translation_finished = true;
    occurrence
        .map(|(diagnostic, effect)| GenericTerminalOccurrence::from_effect(diagnostic, effect))
}

fn generic_translate_success_summary(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<GenericTranslationSummary> {
    match driven {
        Driven::Finished(Ok(GenericCommandOutput::Translate { summary, .. }))
        | Driven::Interrupted(Ok(GenericCommandOutput::Translate { summary, .. })) => {
            Some(*summary)
        }
        Driven::Finished(Ok(_))
        | Driven::Interrupted(Ok(_))
        | Driven::CancellationWon(_)
        | Driven::Finished(Err(_))
        | Driven::Interrupted(Err(_))
        | Driven::SignalFailed { .. } => None,
    }
}

fn generic_translate_driven_error(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> Option<&GenericCommandError> {
    match driven {
        Driven::Finished(Err(error))
        | Driven::Interrupted(Err(error))
        | Driven::CancellationWon(Err(error)) => Some(error),
        Driven::SignalFailed {
            result: Err(error), ..
        } => Some(error),
        Driven::Finished(Ok(_))
        | Driven::Interrupted(Ok(_))
        | Driven::CancellationWon(Ok(_))
        | Driven::SignalFailed { result: Ok(_), .. } => None,
    }
}

fn generic_translate_was_cancelled(
    driven: &Driven<Result<GenericCommandOutput, GenericCommandError>>,
) -> bool {
    match driven {
        Driven::CancellationWon(_) => true,
        Driven::Finished(Err(error)) | Driven::Interrupted(Err(error)) => error.is_cancelled(),
        Driven::Interrupted(Ok(_)) | Driven::Finished(Ok(_)) | Driven::SignalFailed { .. } => false,
    }
}

fn configure_generic_task_records(
    requested: bool,
    project_log: &GenericProjectLogSlot,
    file_system_configuration: &crate::runtime::filesystem::SystemFileSystemConfig,
    redactor: Arc<crate::llm::ApiKeyRedactor>,
    locale: UiLocale,
    cpu: RayonCpuExecutor,
    project_workspace: &Path,
) -> ConfiguredTranslationTaskRecordSink {
    if !requested {
        return ConfiguredTranslationTaskRecordSink::disabled();
    }
    let prepared = {
        let project_log = project_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(project_log) = project_log.as_ref() else {
            return ConfiguredTranslationTaskRecordSink::disabled();
        };
        project_log.run_id().map_or_else(
            || {
                Err((
                    project_log.handle().clone(),
                    project_log.run_id_failure().cloned(),
                ))
            },
            |run_id| {
                Ok((
                    run_id.to_owned(),
                    Arc::clone(project_log.performance()),
                    project_log.handle().clone(),
                ))
            },
        )
    };
    let (run_id, performance, project_log_handle) = match prepared {
        Ok(prepared) => prepared,
        Err((project_log_handle, Some(report))) => {
            project_log_handle.record_task_record_diagnostic(report);
            return ConfiguredTranslationTaskRecordSink::disabled();
        }
        Err((_project_log_handle, None)) => {
            return ConfiguredTranslationTaskRecordSink::disabled();
        }
    };
    match SystemFileSystem::new_with_performance(file_system_configuration.clone(), performance) {
        Ok(file_system) => ConfiguredTranslationTaskRecordSink::Markdown(Box::new(
            MarkdownTranslationTaskRecordSink::new(
                project_workspace.join("task-records").join(&run_id),
                redactor,
                locale,
                cpu,
                file_system,
                project_log_handle.clone(),
            ),
        )),
        Err(error) => {
            project_log_handle.record_task_record_diagnostic(DiagnosticReport::new(
                StateEffect::Unchanged,
                error.diagnostic(),
            ));
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
    diagnostic_stage: GenericDiagnosticStage,
    effect: StateEffect,
    database_path: PathBuf,
    operation: impl FnOnce() -> Result<T, GenericProjectError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| generic_blocking_join_failure(source, effect))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) if source.is_cancelled() => Err(GenericCommandError::Cancelled),
        Err(source) => {
            let report = source.diagnostic_report(diagnostic_stage, &database_path, effect);
            Err(GenericCommandError::reported(source, report))
        }
    }
}

async fn run_scratch_blocking<T>(
    operation: impl FnOnce() -> Result<T, GenericScratchError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) => Err(generic_scratch_command_error(source)),
    }
}

fn generic_scratch_command_error(source: GenericScratchError) -> GenericCommandError {
    if let GenericScratchError::CleanupAfterFailure { operation, cleanup } = source {
        let operation = if matches!(operation.as_ref(), GenericScratchError::Cancelled) {
            GenericCommandError::Cancelled
        } else {
            let report = generic_scratch_report(
                operation.as_ref(),
                FileSystemDiagnosticStage::WriteBack,
                StateEffect::Unchanged,
            );
            GenericCommandError::reported(*operation, report)
        };
        let discard = generic_scratch_discard_failure(*cleanup);
        return GenericCommandError::PublishDiscard {
            operation: Box::new(operation),
            discard,
        };
    }
    if matches!(source, GenericScratchError::Cancelled) {
        GenericCommandError::Cancelled
    } else {
        let report = generic_scratch_report(
            &source,
            FileSystemDiagnosticStage::WriteBack,
            StateEffect::Unchanged,
        );
        GenericCommandError::reported(source, report)
    }
}

fn generic_scratch_discard_failure(source: GenericScratchError) -> GenericDiscardFailure {
    let report = generic_scratch_report(
        &source,
        FileSystemDiagnosticStage::Publication,
        StateEffect::RecoveryRequired,
    );
    GenericDiscardFailure::new(report, source)
}

fn generic_scratch_report(
    source: &GenericScratchError,
    stage: FileSystemDiagnosticStage,
    effect: StateEffect,
) -> DiagnosticReport {
    let file_system = |operation, problem| {
        DiagnosticReport::new(
            effect,
            Diagnostic::file_system(FileSystemIssue::new(
                FileSystemDiagnosticContext::new(stage, operation),
                problem,
            )),
        )
    };
    match source {
        GenericScratchError::Io {
            operation,
            path,
            source,
        } => file_system(
            *operation,
            FileSystemProblem::Io {
                path: SafePath::new(path),
                failure: IoFailure::from_error(source),
            },
        ),
        GenericScratchError::UnsafeCleanupTarget {
            workspace_root,
            scratch_root,
        } => file_system(
            FileSystemOperation::Remove,
            FileSystemProblem::OutsideScope {
                root: SafePath::new(workspace_root),
                path: SafePath::new(scratch_root),
            },
        ),
        GenericScratchError::CleanupAfterFailure { cleanup, .. } => {
            generic_scratch_report(cleanup, stage, effect)
        }
        GenericScratchError::Cancelled => DiagnosticReport::new(
            effect,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::FileSystemExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericScratchError::InvalidRelativePath(path) => file_system(
            FileSystemOperation::ResolveDirectory,
            FileSystemProblem::InvalidPath {
                path: SafePath::new(path),
                violation: FileSystemPathViolation::OutsideScope,
            },
        ),
        GenericScratchError::TargetNotDirectory(path) => file_system(
            FileSystemOperation::Metadata,
            FileSystemProblem::NotDirectory {
                path: SafePath::new(path),
            },
        ),
        GenericScratchError::InvalidMaterializedFile { source, .. } => {
            source.diagnostic_report(effect)
        }
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

async fn drive_generic_translate_with_panic_boundary(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    panic_context: GenericCommandPanicContext,
) -> Driven<Result<GenericCommandOutput, GenericCommandError>> {
    match AssertUnwindSafe(drive(operation, termination_signals, cancel))
        .catch_unwind()
        .await
    {
        Ok(driven) => driven,
        Err(payload) => {
            // 业务正文可能进入 panic payload；引擎边界只保留安全、类型化诊断。
            drop(payload);
            Driven::Finished(Err(generic_translate_panic_error(&panic_context)))
        }
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
    let cancellation_project_log = Arc::clone(&project_log);
    let driven = drive(operation, termination_signals, || {
        emit_generic_cancellation_requested(&cancellation_project_log);
        progress.safe_stopping();
        cancel();
    })
    .await;
    progress.finalizing();
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::file_system(source));
    }
    record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
    GenericCommandRunReport::from_driven(
        driven,
        shutdown_errors,
        take_generic_project_log(&project_log),
    )
}

// 进程驱动器显式持有信号、日志和终止依赖，避免由无类型上下文重新解释生命周期。
#[allow(clippy::too_many_arguments)]
async fn drive_extract_and_shutdown(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    file_system: SystemFileSystem,
    mut shutdown_errors: Vec<GenericShutdownError>,
    project_log: GenericProjectLogSlot,
    extract_project_log: GenericExtractProjectLogStateRef,
    progress: GenericTerminalProgress,
) -> GenericCommandRunReport {
    let cancellation_project_log = Arc::clone(&project_log);
    let driven = drive(operation, termination_signals, || {
        emit_generic_cancellation_requested(&cancellation_project_log);
        progress.safe_stopping();
        cancel();
    })
    .await;
    progress.finalizing();
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::file_system(source));
    }
    record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
    let terminal_occurrence =
        finish_generic_extract_project_log(&project_log, &extract_project_log, &driven);
    GenericCommandRunReport::from_driven_with_terminal_occurrence(
        driven,
        shutdown_errors,
        take_generic_project_log(&project_log),
        terminal_occurrence,
    )
}

impl GenericCommandRunReport {
    fn failed(error: GenericCommandError) -> Self {
        Self {
            result: GenericCommandRunResult::Failed(error),
            shutdown_errors: Vec::new(),
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    fn panicked(error: GenericCommandError, panic_log_path: Option<PathBuf>) -> Self {
        Self {
            result: GenericCommandRunResult::Failed(error),
            shutdown_errors: Vec::new(),
            pending_project_log: None,
            panic_log_path,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    fn from_driven(
        driven: Driven<Result<GenericCommandOutput, GenericCommandError>>,
        shutdown_errors: Vec<GenericShutdownError>,
        project_log: Option<ActiveProjectLog>,
    ) -> Self {
        Self::from_driven_with_terminal_occurrence(driven, shutdown_errors, project_log, None)
    }

    fn from_driven_with_terminal_occurrence(
        driven: Driven<Result<GenericCommandOutput, GenericCommandError>>,
        shutdown_errors: Vec<GenericShutdownError>,
        project_log: Option<ActiveProjectLog>,
        terminal_occurrence: Option<GenericTerminalOccurrence>,
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
            if shutdown_errors.is_empty() {
                return match &result {
                    GenericCommandRunResult::Succeeded(_) => project_log.pending_succeeded(),
                    GenericCommandRunResult::Interrupted => project_log.pending_cancelled(),
                    GenericCommandRunResult::Failed(error) => match terminal_occurrence {
                        Some(occurrence) => occurrence.into_pending(project_log),
                        None => project_log.pending_failure(generic_command_error_report(error)),
                    },
                };
            }

            let mut reports = shutdown_errors
                .iter()
                .map(GenericShutdownError::diagnostic_report);
            let first_shutdown = reports
                .next()
                .expect("非空 shutdown_errors 必须产生至少一份报告");
            let mut report = match &result {
                GenericCommandRunResult::Failed(error) => generic_command_error_report(error)
                    .with_related(RelatedFailureRelation::Shutdown, first_shutdown),
                GenericCommandRunResult::Succeeded(_) | GenericCommandRunResult::Interrupted => {
                    first_shutdown
                }
            };
            for related in reports {
                report = report.with_related(RelatedFailureRelation::Shutdown, related);
            }
            project_log.pending_failure(report)
        });
        Self {
            result,
            shutdown_errors,
            pending_project_log,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    fn with_translation_summary(mut self, summary: Option<TranslationTerminalSummary>) -> Self {
        self.translation_summary = summary;
        self
    }
}

// 发布编排必须独立持有候选、门闩和日志终态，保持副作用顺序可审计。
#[allow(clippy::too_many_arguments)]
async fn publish_generic_write_back(
    publisher: SystemDirectoryPublisher,
    project_name: ProjectName,
    project: GenericProject,
    candidate: GenericWriteBackCandidate,
    cancellation: CooperativeCancellation,
    publication_gate: GenericWriteBackPublicationGate,
    project_log: Option<ProjectLogHandle>,
    publication_occurrence: GenericTerminalOccurrenceSlot,
    publication_started: impl FnOnce() + Send,
) -> Result<GenericCommandOutput, GenericCommandError> {
    if cancellation.is_requested() {
        return Err(GenericCommandError::Cancelled);
    }
    let workspace_root = project.workspace_root().to_path_buf();
    let translated_units = candidate.translated_units();
    let retained_source_units = candidate.retained_source_units();
    let files = candidate.files().len();
    let scratch_candidate = candidate;
    let scratch_workspace = workspace_root.clone();
    let materialize_cancellation = cancellation.clone();
    let scratch_root = run_scratch_blocking(move || {
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
            Err(cleanup) => {
                let discard = generic_scratch_discard_failure(cleanup);
                Err(GenericCommandError::PublishDiscard {
                    operation: Box::new(GenericCommandError::Cancelled),
                    discard,
                })
            }
        };
    }

    let target_root = project.write_back_root();
    let request = (|| {
        let publish_intent = publish_intent_for(&target_root)
            .map_err(|source| Box::new(generic_scratch_command_error(source)))?;
        let mapping = DirectorySourceMapping::new(scratch_root.clone(), PathBuf::new()).map_err(
            |source| Box::new(generic_publication_request_failure(&target_root, source)),
        )?;
        DirectoryStageRequest::new(
            target_root.clone(),
            publish_intent,
            vec![mapping],
            Vec::new(),
            Vec::new(),
        )
        .map_err(|source| Box::new(generic_publication_request_failure(&target_root, source)))
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
        let report = generic_scratch_report(
            &source,
            FileSystemDiagnosticStage::Publication,
            StateEffect::RecoveryRequired,
        );
        let operation = GenericCommandError::reported(source, report);
        return discard_after_failure(&publisher, staged, operation).await;
    }
    if cancellation.is_requested() {
        return discard_after_failure(&publisher, staged, GenericCommandError::Cancelled).await;
    }

    let recheck_cancellation = cancellation.clone();
    let database_path = project.database_path().to_path_buf();
    if let Err(operation) = run_project_blocking(
        GenericDiagnosticStage::WriteBack,
        StateEffect::Unchanged,
        database_path,
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
    if let Some(project_log) = &project_log {
        project_log.emit(ProjectLogEvent::publication_started(&target_root));
    }
    if let Err(source) = publisher.publish(staged).await {
        let report = source.diagnostic_report();
        if let Some(project_log) = &project_log
            && let Some(diagnostic) =
                project_log.record_diagnostic(DiagnosticScope::Publication, report.clone())
        {
            let occurrence = match report.effect() {
                StateEffect::AppliedFinalizationFailed | StateEffect::RecoveryRequired => {
                    GenericTerminalOccurrence::recovery_required(diagnostic)
                }
                StateEffect::Unchanged
                | StateEffect::ProgressPreserved
                | StateEffect::Applied
                | StateEffect::AppliedRunPlanNotSaved
                | StateEffect::OutcomeUnknown => {
                    GenericTerminalOccurrence::from_effect(diagnostic, report.effect())
                }
            };
            *publication_occurrence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(occurrence);
            let result = match report.effect() {
                StateEffect::RecoveryRequired | StateEffect::AppliedFinalizationFailed => {
                    PublicationFinished::RecoveryRequired { diagnostic }
                }
                StateEffect::OutcomeUnknown => PublicationFinished::OutcomeUnknown { diagnostic },
                StateEffect::Unchanged
                | StateEffect::ProgressPreserved
                | StateEffect::Applied
                | StateEffect::AppliedRunPlanNotSaved => {
                    PublicationFinished::NotPublished { diagnostic }
                }
            };
            project_log.emit(ProjectLogEvent::PublicationFinished { result });
        }
        return Err(GenericCommandError::reported(source, report));
    }
    if let Some(project_log) = &project_log {
        project_log.emit(ProjectLogEvent::PublicationFinished {
            result: PublicationFinished::Published {
                summary: PublicationSummary::Generic(ProjectLogGenericPublicationSummary {
                    files: generic_count(files),
                    translated_units: generic_count(translated_units),
                    retained_source_units: generic_count(retained_source_units),
                }),
            },
        });
    }
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
        let report = source.diagnostic_report();
        GenericCommandError::reported(source, report)
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
        Err(cleanup) => {
            let discard = generic_scratch_discard_failure(cleanup);
            GenericCommandError::PublishDiscard {
                operation: Box::new(operation),
                discard,
            }
        }
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

fn generic_publication_request_failure(
    output_root: &Path,
    source: DirectoryStageRequestError,
) -> GenericCommandError {
    let violation = match &source {
        DirectoryStageRequestError::EmptyTargetRoot => PublicationRequestViolation::EmptyTargetRoot,
        DirectoryStageRequestError::EmptySourceDirectory => {
            PublicationRequestViolation::EmptySourceDirectory
        }
        DirectoryStageRequestError::EmptySourceMappings => {
            PublicationRequestViolation::EmptySourceMappings
        }
        DirectoryStageRequestError::InvalidRelativePath { path } => {
            PublicationRequestViolation::InvalidRelativePath {
                path: SafePath::new(path),
            }
        }
        DirectoryStageRequestError::OverlappingSourceTargets { first, second } => {
            PublicationRequestViolation::OverlappingSourceTargets {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlappingOverlays { first, second } => {
            PublicationRequestViolation::OverlappingOverlays {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlappingEmptyDirectories { first, second } => {
            PublicationRequestViolation::OverlappingEmptyDirectories {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlayOutsideSourceMappings { relative_file } => {
            PublicationRequestViolation::OverlayOutsideSourceMappings {
                relative_file: SafePath::new(relative_file),
            }
        }
        DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
            empty_directory,
            source_target,
        } => PublicationRequestViolation::EmptyDirectoryOverlapsSourceTarget {
            empty_directory: SafePath::new(empty_directory),
            source_target: SafePath::new(source_target),
        },
        DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
            empty_directory,
            overlay,
        } => PublicationRequestViolation::EmptyDirectoryOverlapsOverlay {
            empty_directory: SafePath::new(empty_directory),
            overlay: SafePath::new(overlay),
        },
    };
    let report = DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::publication(PublicationIssue::new(
            PublicationStep::PrepareCandidate,
            PublicationProblem::InvalidRequest {
                output_root: SafePath::new(output_root),
                violation,
            },
        )),
    );
    GenericCommandError::reported(source, report)
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
    let scratch_root = workspace_root.join(WRITE_BACK_SCRATCH_NAME);
    fs::create_dir(&scratch_root).map_err(|source| GenericScratchError::Io {
        operation: FileSystemOperation::Create,
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
                operation: FileSystemOperation::Create,
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
                        operation: FileSystemOperation::Write,
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
                            operation: FileSystemOperation::Read,
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
        .is_some_and(|name| name == WRITE_BACK_SCRATCH_NAME);
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
            operation: FileSystemOperation::Remove,
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
            operation: FileSystemOperation::Metadata,
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
    P::Error: DirectoryPublicationDiagnosticSource,
{
    match publisher.discard(staged).await {
        Ok(()) => Err(operation),
        Err(discard) => {
            let discard = generic_directory_discard_failure(discard);
            Err(GenericCommandError::PublishDiscard {
                operation: Box::new(operation),
                discard,
            })
        }
    }
}

fn generic_directory_discard_failure<E>(source: DirectoryDiscardError<E>) -> GenericDiscardFailure
where
    E: Error + Send + Sync + DirectoryPublicationDiagnosticSource + 'static,
{
    let report = source.diagnostic_report();
    GenericDiscardFailure::new(report, source)
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
        operation: FileSystemOperation,
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
            } => write!(
                formatter,
                "{} {} 失败：{source}",
                operation.as_str(),
                path.display()
            ),
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

    use crate::diagnostic::DiagnosticStage;
    use crate::generic::{
        GenericPlaceholderRuleDefinition, automatic_translation_state_fingerprint,
    };
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule,
    };
    use crate::storage::file_system::{DirectoryPublishError, StagingCleanupFailure};
    use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

    use super::*;

    fn fingerprint(byte: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([byte; 32])
    }

    fn manual_read_failure() -> ManualCommandError {
        ManualCommandError::Document(crate::manual::ManualDocumentError::Read {
            path: PathBuf::from("C:/project/manual.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "测试读取失败"),
        })
    }

    #[test]
    fn only_direct_generic_manual_failure_uses_detailed_manual_renderer() {
        let direct = generic_manual_failure(manual_read_failure());
        assert!(direct.manual_error().is_some());

        let signal = GenericCommandError::Signal {
            source: io::Error::other("测试信号失败"),
            operation: Some(Box::new(generic_manual_failure(manual_read_failure()))),
            state_applied: false,
        };
        assert!(
            signal.manual_error().is_none(),
            "Signal 外层的类型化主错误和 related 不能被递归 Manual 呈现替换"
        );
        assert_eq!(generic_command_error_report(&signal).related().len(), 1);

        let discard_report = DiagnosticReport::new(
            StateEffect::RecoveryRequired,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::Shutdown,
            }),
        );
        let discard =
            GenericDiscardFailure::new(discard_report, io::Error::other("测试候选清理失败"));
        let publish_discard = GenericCommandError::PublishDiscard {
            operation: Box::new(generic_manual_failure(manual_read_failure())),
            discard,
        };
        assert!(
            publish_discard.manual_error().is_none(),
            "Discard 外层必须保留类型化相关报告"
        );
        assert_eq!(
            generic_command_error_report(&publish_discard)
                .related()
                .len(),
            1
        );
    }

    struct TestManualTranslation<'a> {
        id: &'a str,
        relative_path: &'a str,
        group_id: &'a str,
        unit_id: &'a str,
        kind: &'a str,
        source: &'a str,
        translation: &'a str,
    }

    fn apply_test_manual_translation(
        store: &GenericProjectStore,
        entry: TestManualTranslation<'_>,
    ) {
        let source = entry
            .source
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let translation = entry
            .translation
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let write = crate::manual::ValidatedManualTranslation {
            id: entry.id.to_owned(),
            kind: crate::manual::ManualTranslationType::Free,
            source: source.clone(),
            translation,
            locator: crate::manual::ManualTranslationLocator::Generic {
                group_id: entry.group_id.to_owned(),
                unit_id: entry.unit_id.to_owned(),
            },
            applicability: crate::manual::generic_manual_applicability(
                entry.group_id,
                entry.unit_id,
                entry.relative_path,
                entry.kind,
                &source,
            ),
        };
        let connection = Connection::open(store.database_path()).expect("应该可打开测试项目数据库");
        crate::manual::apply_generic_manual_translations(&connection, &[write])
            .expect("应该可保存独立人工译文");
    }

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    fn compact_json(message: &str) -> String {
        let json = message
            .strip_prefix("```json\n")
            .and_then(|value| value.strip_suffix("\n```"))
            .expect("模型 user message 必须是单一 JSON 围栏");
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(json)
                .expect("模型 user message 必须是有效 JSON"),
        )
        .expect("模型 user message 应该可以重新序列化")
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
        let context = GenericCommandPanicContext::new(
            crate::diagnostic::RuntimeCommand::Translate,
            workspace.clone(),
        );
        let redactor = Arc::new(ApiKeyRedactor::new(secrecy::SecretString::from(
            "selected-secret",
        )));
        context.observe_selected_api_key_redactor(Arc::clone(&redactor));
        let report = catch_generic_command_panic(context, async {
            panic!("不得读取的测试 panic payload")
        })
        .await;

        assert!(
            report
                .selected_api_key_redactor
                .as_ref()
                .is_some_and(|selected| Arc::ptr_eq(selected, &redactor))
        );

        let GenericCommandRunResult::Failed(GenericCommandError::Operation { failure }) =
            report.result
        else {
            panic!("Generic 命令 panic 必须成为命令级结构化失败");
        };
        let diagnostic = failure.report();
        assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
        assert_eq!(diagnostic.primary().stage(), DiagnosticStage::Translate);
        assert_eq!(diagnostic.primary().code(), "runtime.command_panicked");
        let wire = serde_json::to_string(diagnostic).expect("panic 诊断必须可序列化");
        assert!(wire.contains("\"command\":\"translate\""));
        assert!(wire.contains(&workspace.to_string_lossy().to_string()));
        assert!(!wire.contains("不得读取的测试 panic payload"));
    }

    #[tokio::test]
    async fn generic_command_panic_after_log_start_preserves_established_log_path() {
        let temporary = tempfile::tempdir().expect("应建立 Generic panic 日志测试目录");
        let common =
            crate::application::config::CommonCommandConfiguration::for_test(temporary.path());
        let project = "panic-log";
        let workspace = temporary.path().join(GENERIC_ENGINE_NAME).join(project);
        fs::create_dir_all(&workspace).expect("应建立 Generic 测试项目目录");
        let active = start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::SimplifiedChinese,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Extract,
            performance: Arc::new(RunPerformanceCounters::default()),
            selected_api_key_redactor: None,
        });
        let expected = active
            .established_log_path()
            .expect("测试项目日志 runtime 应建立")
            .to_path_buf();
        let context =
            GenericCommandPanicContext::new(crate::diagnostic::RuntimeCommand::Extract, workspace);
        context.observe_project_log(&active);

        let report = catch_generic_command_panic(context, async {
            panic!("测试项目日志建立后的 Generic 命令 panic")
        })
        .await;

        assert_eq!(report.panic_log_path.as_deref(), Some(expected.as_path()));
        let GenericCommandRunResult::Failed(error) = report.result else {
            panic!("Generic 命令 panic 必须报告失败");
        };
        let diagnostic = generic_command_error_report(&error);
        let crate::diagnostic::DiagnosticIssue::Runtime(RuntimeIssue::CommandPanicked {
            log_path: Some(log_path),
            ..
        }) = diagnostic.primary().issue()
        else {
            panic!("Generic 命令 panic 的类型化诊断必须保留已建立日志路径");
        };
        assert_eq!(log_path.as_str(), expected.to_string_lossy().as_ref());
        let _ = active.pending_cancelled().finish();
    }

    #[tokio::test]
    async fn generic_translate_panic_boundary_keeps_engine_finalization_path() {
        let workspace = PathBuf::from("projects/generic/translate-panic");
        let mut termination_signals = TerminationSignals::new();
        let driven = drive_generic_translate_with_panic_boundary(
            async {
                panic!("不得读取的 Translate panic payload");
                #[allow(unreachable_code)]
                Ok(GenericCommandOutput::Lua {
                    project: "unreachable".parse().expect("项目名应合法"),
                })
            },
            &mut termination_signals,
            || {},
            GenericCommandPanicContext::new(
                crate::diagnostic::RuntimeCommand::Translate,
                workspace.clone(),
            ),
        )
        .await;

        let Driven::Finished(Err(error)) = driven else {
            panic!("Translate 内部 panic 必须回到引擎自己的失败收尾路径");
        };
        assert!(error.is_application_scope_panic());
        let diagnostic = generic_command_error_report(&error);
        assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
        assert_eq!(diagnostic.primary().code(), "runtime.command_panicked");
        let wire = serde_json::to_string(&diagnostic).expect("panic 诊断必须可序列化");
        assert!(wire.contains(&workspace.to_string_lossy().to_string()));
        assert!(!wire.contains("不得读取的 Translate panic payload"));
    }

    #[test]
    fn generic_translate_panic_finalizes_started_tasks_and_project_log_once() {
        let temporary = tempfile::tempdir().expect("应建立 Translate panic 日志目录");
        let common =
            crate::application::config::CommonCommandConfiguration::for_test(temporary.path());
        let project = "translate-panic-log";
        let workspace = temporary.path().join("generic").join(project);
        fs::create_dir_all(&workspace).expect("应建立 Generic 项目工作区");
        let project_log = generic_project_log_slot();
        install_generic_project_log(
            &project_log,
            start_command_log(CommandLogStart {
                common: &common,
                locale: UiLocale::English,
                engine: ProjectLogEngine::Generic,
                project,
                command: ProjectLogCommand::Translate,
                performance: Arc::new(RunPerformanceCounters::default()),
                selected_api_key_redactor: None,
            }),
        );
        let state = generic_translate_project_log_state();
        start_generic_translate_phase(
            &project_log,
            &state,
            ProjectLogPhase::Planning,
            ProjectLogAmount::Indeterminate,
        );
        resolve_generic_translate_run_plan(
            &project_log,
            &state,
            &workspace.join("project.db"),
            RunPlanValueSource::Explicit,
            "local",
            None,
            None,
        );
        let tasks = install_generic_translate_task_log(&project_log, &state, 2);
        set_generic_translate_summary(
            &state,
            GenericTranslationSummary {
                total_tasks: 2,
                written_units: 1,
                ..GenericTranslationSummary::default()
            },
        );
        mark_generic_translate_run_plan_saved(&state);
        complete_generic_translate_phase(
            &project_log,
            &state,
            ProjectLogPhase::Planning,
            ProjectLogAmount::Determinate {
                completed: 2,
                total: 2,
            },
        );
        start_generic_translate_phase(
            &project_log,
            &state,
            ProjectLogPhase::ConfirmedTasks,
            ProjectLogAmount::Determinate {
                completed: 0,
                total: 2,
            },
        );
        tasks.started(0);
        tasks.started(1);

        let panic_context = GenericCommandPanicContext::new(
            crate::diagnostic::RuntimeCommand::Translate,
            workspace.clone(),
        );
        let driven = Driven::Finished(Err(generic_translate_panic_error(&panic_context)));
        let error = generic_translate_driven_error(&driven).expect("panic 必须形成失败");
        tasks.fail_in_flight_after_panic(generic_command_error_report(error));
        let occurrence = finish_generic_translate_project_log(&project_log, &state, &driven)
            .expect("panic 失败必须形成项目日志诊断");
        let active = take_generic_project_log(&project_log).expect("项目日志必须仍由引擎持有");
        let _ = occurrence.into_pending(active).finish();

        let mut logs = fs::read_dir(workspace.join("logs"))
            .expect("日志目录必须可读取")
            .map(|entry| entry.expect("日志条目必须可读取").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect::<Vec<_>>();
        logs.sort();
        assert_eq!(logs.len(), 1, "panic 运行必须只有一份项目日志");
        let records = fs::read_to_string(&logs[0])
            .expect("panic 项目日志必须可读取")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志必须是 JSONL"))
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "task.started")
                .count(),
            2
        );
        let finished = records
            .iter()
            .filter(|record| record["event"] == "task.finished")
            .collect::<Vec<_>>();
        assert_eq!(finished.len(), 2, "每个已开始任务都必须获得 panic 终态");
        assert!(
            finished
                .iter()
                .all(|record| record["payload"]["outcome"]["kind"] == "failed")
        );
        for event in [
            "phase.stopped",
            "run_plan.finalized",
            "translation.finished",
            "run.finished",
        ] {
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record["event"] == event)
                    .count(),
                1,
                "panic 必须恰好写一条 {event}"
            );
        }
        let run_plan = records
            .iter()
            .find(|record| record["event"] == "run_plan.finalized")
            .expect("run plan 必须有终态");
        assert_eq!(run_plan["payload"]["result"]["kind"], "saved");
        let translation = records
            .iter()
            .find(|record| record["event"] == "translation.finished")
            .expect("Translate 必须有终态");
        assert_eq!(translation["payload"]["result"]["kind"], "failed");
        assert_eq!(
            translation["payload"]["result"]["tasks"],
            serde_json::json!({
                "planned": 2,
                "started": 2,
                "complete": 0,
                "partial": 0,
                "unavailable": 0,
                "failed": 2,
                "cancelled": 0,
                "not_started": 0,
            })
        );
        assert_eq!(
            translation["payload"]["result"]["summary"]["summary"]["written_units"],
            1
        );
        assert_eq!(
            records.last().expect("panic 日志不得为空")["payload"]["result"]["kind"],
            "failed",
            "run.finished 必须是最后的 Failed 终态"
        );
    }

    #[test]
    fn generic_candidate_worker_failure_is_not_a_partial_response_problem() {
        let locator = GenericUnitLocator {
            relative_path: PathBuf::from("story.jsonl"),
            group_id: "story".to_owned(),
            unit_id: "line".to_owned(),
            role: "dialogue".to_owned(),
            line: 1,
            unit: 1,
        };
        let error = generic_candidate_placeholder_problem(
            crate::generic::GenericPlaceholderError::Protection(
                PlaceholderProtectionError::StartWorker {
                    operation: PlaceholderWorkerOperation::MatchText,
                    source: io::Error::other("worker unavailable"),
                },
            ),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &locator,
        )
        .expect_err("worker 启动失败必须离开普通候选不合格分支");

        assert!(matches!(
            &error,
            GenericPreparationError::PlaceholderProtection {
                source: PlaceholderProtectionError::StartWorker { .. },
                ..
            }
        ));
        let report = generic_preparation_report_at(&error, GenericDiagnosticStage::Translate);
        assert_eq!(
            report.primary().code(),
            "translation.placeholder.worker_start"
        );

        let ordinary = generic_candidate_placeholder_problem(
            crate::generic::GenericPlaceholderError::ManualTranslationMismatch,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &locator,
        )
        .expect("候选 Placeholder 绑定不匹配仍应成为逐目标问题");
        assert_eq!(
            ordinary,
            GenericResponseDestinationProblem::PlaceholderBindingMismatch
        );
    }

    #[test]
    fn generic_current_translation_technical_placeholder_failure_is_not_source_fallback() {
        let locator = GenericUnitLocator {
            relative_path: PathBuf::from("story.jsonl"),
            group_id: "story".to_owned(),
            unit_id: "line".to_owned(),
            role: "dialogue".to_owned(),
            line: 1,
            unit: 1,
        };
        let error = generic_current_translation_protection_result(
            Err(crate::generic::GenericPlaceholderError::Protection(
                PlaceholderProtectionError::StartWorker {
                    operation: PlaceholderWorkerOperation::MatchText,
                    source: io::Error::other("worker unavailable"),
                },
            )),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &locator,
        )
        .expect_err("已有译文的 worker 启动失败必须终止规划");
        assert!(matches!(
            &error,
            GenericPreparationError::PlaceholderProtection {
                source: PlaceholderProtectionError::StartWorker { .. },
                ..
            }
        ));
        assert_eq!(
            generic_preparation_report_at(&error, GenericDiagnosticStage::Translate)
                .primary()
                .code(),
            "translation.placeholder.worker_start"
        );

        let ordinary = generic_current_translation_protection_result(
            Err(crate::generic::GenericPlaceholderError::Protection(
                PlaceholderProtectionError::ReservedTokenNamespace {
                    start_byte: 0,
                    end_byte: 8,
                },
            )),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &locator,
        )
        .expect("已有译文的数据不合格仍应使用保护后原文");
        assert!(ordinary.is_none());
    }

    #[test]
    fn generic_translate_log_state_keeps_saved_progress_summary() {
        let state = generic_translate_project_log_state();
        let initial = GenericTranslationSummary {
            total_tasks: 2,
            cleared_units: 1,
            ..GenericTranslationSummary::default()
        };
        set_generic_translate_summary(&state, initial);
        mark_generic_translate_run_plan_saved(&state);
        update_generic_translate_summary(&state, |summary| {
            summary.accepted_units += 1;
            summary.written_units += 1;
            summary.complete_tasks += 1;
        });

        let state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.run_plan_saved);
        assert_eq!(
            state.summary,
            Some(GenericTranslationSummary {
                total_tasks: 2,
                complete_tasks: 1,
                cleared_units: 1,
                accepted_units: 1,
                written_units: 1,
                ..GenericTranslationSummary::default()
            })
        );
    }

    #[test]
    fn generic_partial_response_diagnostic_keeps_task_and_output_id() {
        let diagnostic =
            generic_response_problem_diagnostic(2, 3, &ResponseProblem::MissingId { output_id: 7 });

        assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
        assert_eq!(
            diagnostic.primary().code(),
            "generic.translation.response.missing_id"
        );
        let wire = serde_json::to_value(&diagnostic).expect("响应诊断必须可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["task_ordinal"],
            3
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["total_tasks"],
            3
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["problem"]["output_id"],
            7
        );
    }

    #[test]
    fn generic_planning_fact_diagnostic_keeps_full_unit_locator() {
        let locator = GenericPlanningUnitLocator::new(
            "a.jsonl",
            "group-a".to_owned(),
            "unit-a".to_owned(),
            "dialogue".to_owned(),
        );
        let report = generic_planning_report(&GenericPlanningError::Missing(locator));
        let wire = serde_json::to_value(report).expect("规划诊断必须可序列化");

        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["unit"],
            serde_json::json!({
                "relative_path": "a.jsonl",
                "group_id": "group-a",
                "unit_id": "unit-a",
                "role": "dialogue",
                "line": null,
                "unit": null,
            })
        );
    }

    #[test]
    fn cancellation_after_request_keeps_task_record_draft() {
        let prepared = cancelled_generic_prepared_task(
            1,
            Some(GenericTaskRecordInFlight {
                task_index: 1,
                requested_outputs: 1,
                user_message: "request".to_owned(),
            }),
            1,
        );

        assert_eq!(prepared.task_index, 1);
        assert!(matches!(
            prepared.outcome,
            GenericPreparedTaskOutcome::Cancelled
        ));
        let record = prepared.record.expect("请求开始后的取消必须保留可提交记录");
        assert_eq!(record.task_index, 1);
        assert_eq!(record.requested_outputs, 1);
        assert_eq!(record.user_message, "request");
        assert!(record.raw_assistant.is_none());
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
            generic_command_error_report(&error).effect(),
            StateEffect::AppliedFinalizationFailed
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
                &GenericPlaceholderRuleSource::ProjectSnapshot,
                language_module,
                AutomaticStateResources {
                    prompt: fingerprint(1),
                    client_semantics: fingerprint(2),
                    language_module: fingerprint(3),
                    terminology_hits: empty_terminology_fingerprint(),
                },
                NonZeroUsize::new(10_000).expect("常量应该非零"),
                false,
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

        let error = generic_preparation_failure(source);
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
    fn generic_placeholder_failure_keeps_rule_source_unit_locator_and_match_range() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("a.jsonl"),
            concat!(
                r#"{"id":"group-a","kind":"dialogue","units":["#,
                r#"{"id":"safe","text":"こんにちは"},"#,
                r#"{"id":"unit-a","text":"甲触发缺组乙"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入首个 Generic 输入");
        fs::write(
            source_root.join("b.jsonl"),
            concat!(
                r#"{"id":"group-b","kind":"dialogue","units":["#,
                r#"{"id":"unit-b","text":"丙触发缺组丁"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入第二个 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "placeholder-locator".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"(?:(?<text>保留)|触发缺组)",
            )])
            .expect("Placeholder 规则应该可编译");
        let external_rules = temporary.path().join("rules/placeholders.toml");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
        ));

        for expected_rule_source in [
            GenericPlaceholderRuleSource::ExternalFile(external_rules),
            GenericPlaceholderRuleSource::ProjectSnapshot,
        ] {
            let error = match prepare_generic_translation(
                &snapshot,
                Arc::new(CompiledTerminology::empty()),
                &rules,
                &expected_rule_source,
                Arc::clone(&language_module),
                AutomaticStateResources {
                    prompt: fingerprint(91),
                    client_semantics: fingerprint(92),
                    language_module: fingerprint(93),
                    terminology_hits: empty_terminology_fingerprint(),
                },
                NonZeroUsize::new(10_000).expect("常量应该非零"),
                false,
                &CooperativeCancellation::default(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("最早的缺少 text 捕获应该阻止规划"),
            };

            let report =
                generic_placeholder_protection_report(&error, GenericDiagnosticStage::Translate)
                    .expect("Placeholder 叶子失败必须直接投影为结构化报告");
            let wire = serde_json::to_value(&report).expect("诊断报告必须可序列化");
            assert_eq!(
                wire["primary"]["code"],
                "translation.placeholder.missing_text_capture"
            );
            assert_eq!(wire["primary"]["stage"], "translate");
            assert_eq!(wire["primary"]["resolution"], "fix_placeholder_rules");
            assert_eq!(wire["effect"], "unchanged");
            assert_eq!(
                wire["primary"]["issue"]["details"]["unit"]["relative_path"],
                "a.jsonl"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["unit"]["group_id"],
                "group-a"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["unit"]["unit_id"],
                "unit-a"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["unit"]["role"],
                "dialogue"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["rule_number"],
                1
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["match_range"],
                serde_json::json!({
                    "start": "甲".len(),
                    "end": "甲触发缺组".len(),
                })
            );
            if expected_rule_source == GenericPlaceholderRuleSource::ProjectSnapshot {
                let report = generic_placeholder_protection_report(
                    &error,
                    GenericDiagnosticStage::WriteBack,
                )
                .expect("WriteBack Placeholder 失败必须保留 Unit 定位");
                let wire = serde_json::to_value(report).expect("诊断报告必须可序列化");
                assert_eq!(
                    wire["primary"]["code"],
                    "generic.write_back.placeholder.missing_text_capture"
                );
                assert_eq!(wire["primary"]["stage"], "write_back");
                assert_eq!(wire["primary"]["resolution"], "fix_placeholder_rules");
                assert_eq!(
                    wire["primary"]["issue"]["details"]["operation"],
                    "build_write_back_candidate"
                );
                assert_eq!(
                    wire["primary"]["issue"]["details"]["problem"]["unit"]["role"],
                    "dialogue"
                );
                assert_eq!(
                    wire["primary"]["issue"]["details"]["problem"]["problem"]["side"],
                    "source"
                );
            }

            match error {
                GenericPreparationError::PlaceholderProtection {
                    rule_source,
                    locator,
                    source:
                        PlaceholderProtectionError::MissingTextCapture {
                            rule_number,
                            whole_match_start_byte,
                            whole_match_end_byte,
                        },
                } => {
                    assert_eq!(rule_source, expected_rule_source);
                    assert_eq!(locator.relative_path, Path::new("a.jsonl"));
                    assert_eq!(locator.group_id, "group-a");
                    assert_eq!(locator.unit_id, "unit-a");
                    assert_eq!(locator.role, "dialogue");
                    assert_eq!(rule_number, 1);
                    assert_eq!(whole_match_start_byte, "甲".len());
                    assert_eq!(whole_match_end_byte, "甲触发缺组".len());
                }
                other => panic!("应返回完整 Generic Placeholder 定位，实际为 {other:?}"),
            }
        }
    }

    #[test]
    fn cancelled_cpu_schedule_becomes_interrupted() {
        let source: CpuTaskExecutionError<CpuExecutorUnavailable> =
            CpuTaskExecutionError::Cancelled;
        let error = generic_cpu_execution_failure(source);
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
            ReadFileError::Io {
                path: PathBuf::from("script.lua"),
                source: cancelled_fs(),
            },
            FileSystemDiagnosticStage::CommandPreparation,
        ));
        assert_interrupted(generic_prompt_resource_failure(
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
            (
                ResourceError::InvalidTerminology {
                    path: Some(PathBuf::from("terms.toml")),
                    source: TerminologyDefinitionError::StartWorker {
                        operation: "att-term-matcher",
                        source: io::Error::from_raw_os_error(8),
                    },
                },
                "translation.terminology.worker_start",
            ),
            (
                ResourceError::InvalidPlaceholderRules {
                    path: Some(PathBuf::from("placeholders.toml")),
                    source: PlaceholderDefinitionError::StartWorker {
                        operation: "att-placeholder-toml",
                        source: io::Error::from_raw_os_error(8),
                    },
                },
                "translation.placeholder_definition.worker_start",
            ),
        ];

        for (source, expected_code) in failures {
            let GenericCommandError::Operation { failure } =
                generic_translation_resource_failure(source)
            else {
                panic!("worker 启动失败必须保留为普通失败");
            };
            let diagnostic = failure.report();
            assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
            assert_eq!(diagnostic.primary().code(), expected_code);
            let wire = serde_json::to_string(diagnostic).expect("worker 诊断必须可序列化");
            assert!(wire.contains("\"raw_os_code\":8"));
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
        let scratch_root = workspace_root.join(WRITE_BACK_SCRATCH_NAME);
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
    fn directory_prepare_preserves_recovery_and_unknown_effects() {
        for (source, expected_effect) in [
            (
                SystemFileSystemError::JournalCorrupt {
                    path: PathBuf::from(".directory-publish/write_back/journal"),
                    violation: crate::diagnostic::FileSystemJournalViolation::NotRegularFile,
                },
                StateEffect::RecoveryRequired,
            ),
            (
                SystemFileSystemError::OutcomeUnknown {
                    target_root: PathBuf::from("write_back"),
                    artifacts: vec![PathBuf::from(".directory-publish/write_back/journal")],
                    violation: crate::diagnostic::FileSystemRecoveryViolation::ObservationFailed,
                },
                StateEffect::OutcomeUnknown,
            ),
        ] {
            let error = generic_prepare_error(
                &CooperativeCancellation::default(),
                DirectoryPrepareError::NotPrepared {
                    target_root: PathBuf::from("write_back"),
                    source: Box::new(source),
                    cleanup_failure: None,
                },
            );
            assert_eq!(
                generic_command_error_report(&error).effect(),
                expected_effect
            );
        }
    }

    #[test]
    fn directory_publish_cleanup_is_a_related_recovery_failure() {
        let source = DirectoryPublishError::TargetAlreadyExists {
            target_root: PathBuf::from("write_back"),
            cleanup_failure: Some(StagingCleanupFailure::new(
                PathBuf::from(".directory-publish/write_back/stage"),
                Box::new(SystemFileSystemError::Closed),
            )),
        };
        let report = source.diagnostic_report();
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Cleanup
        );
        assert_eq!(
            report.related()[0].report().effect(),
            StateEffect::RecoveryRequired
        );

        let error = GenericCommandError::reported(source, report);
        assert_eq!(
            generic_command_error_report(&error).effect(),
            StateEffect::RecoveryRequired
        );
    }

    #[test]
    fn directory_request_failure_keeps_output_root_and_invalid_path() {
        let output_root = PathBuf::from("project/write_back");
        let invalid_path = PathBuf::from("../outside");
        let error = generic_publication_request_failure(
            &output_root,
            DirectoryStageRequestError::InvalidRelativePath {
                path: invalid_path.clone(),
            },
        );
        let report = generic_command_error_report(&error);

        assert_eq!(report.effect(), StateEffect::Unchanged);
        assert_eq!(
            report.primary().code(),
            "publication.request.invalid_relative_path"
        );
        let wire = serde_json::to_string(&report).expect("发布请求诊断必须可序列化");
        assert!(wire.contains(&output_root.to_string_lossy().to_string()));
        assert!(wire.contains(&invalid_path.to_string_lossy().to_string()));
    }

    #[test]
    fn cancelled_scratch_cleanup_failure_keeps_primary_and_recovery_path() {
        let scratch_root = PathBuf::from("project/.write_back.tmp");
        let error = generic_scratch_command_error(GenericScratchError::CleanupAfterFailure {
            operation: Box::new(GenericScratchError::Cancelled),
            cleanup: Box::new(GenericScratchError::Io {
                operation: FileSystemOperation::Remove,
                path: scratch_root.clone(),
                source: io::Error::from_raw_os_error(5),
            }),
        });

        let report = generic_command_error_report(&error);
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.primary().code(), "runtime.cancelled");
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Discard
        );
        assert_eq!(
            report.related()[0].report().effect(),
            StateEffect::RecoveryRequired
        );
        let wire = serde_json::to_string(&report).expect("scratch 诊断必须可序列化");
        assert!(wire.contains(&scratch_root.to_string_lossy().to_string()));

        match error {
            GenericCommandError::PublishDiscard { operation, .. } => {
                assert!(operation.is_cancelled());
            }
            other => panic!("取消后的清理失败必须保留双错误，实际为 {other}"),
        }
    }

    #[test]
    fn failed_scratch_materialization_cleanup_keeps_primary_and_recovery_path() {
        let scratch_root = PathBuf::from("project/.write_back.tmp");
        let source_file = scratch_root.join("dialogue.jsonl");
        let error = generic_scratch_command_error(GenericScratchError::CleanupAfterFailure {
            operation: Box::new(GenericScratchError::Io {
                operation: FileSystemOperation::Write,
                path: source_file,
                source: io::Error::from_raw_os_error(5),
            }),
            cleanup: Box::new(GenericScratchError::Io {
                operation: FileSystemOperation::Remove,
                path: scratch_root.clone(),
                source: io::Error::from_raw_os_error(5),
            }),
        });

        let report = generic_command_error_report(&error);
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.primary().code(), "filesystem.io");
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Discard
        );
        let related = report.related()[0].report();
        assert_eq!(related.effect(), StateEffect::RecoveryRequired);
        assert_eq!(related.primary().code(), "filesystem.io");
        let wire = serde_json::to_string(&report).expect("scratch 诊断必须可序列化");
        assert!(wire.contains("\"operation\":\"write\""));
        assert!(wire.contains("\"operation\":\"remove\""));
        assert!(wire.contains("\"raw_os_code\":5"));
        assert!(wire.contains(&scratch_root.to_string_lossy().to_string()));

        let primary_source = Error::source(&error).expect("双重失败必须以主业务错误为 source");
        assert!(
            primary_source
                .downcast_ref::<GenericCommandError>()
                .is_some()
        );

        match &error {
            GenericCommandError::PublishDiscard { operation, discard } => {
                assert!(!operation.is_cancelled());
                assert!(matches!(
                    operation.as_ref(),
                    GenericCommandError::Operation { .. }
                ));
                let cleanup = discard
                    .source_error()
                    .downcast_ref::<GenericScratchError>()
                    .expect("应保留真实 scratch 清理错误");
                let io_source = Error::source(cleanup)
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .expect("scratch 清理错误链应保留 io::Error");
                assert_eq!(io_source.raw_os_error(), Some(5));
            }
            other => panic!("暂存建立与清理双重失败必须保留主错误和恢复路径，实际为 {other}"),
        }

        assert_eq!(
            generic_command_error_report(&error).effect(),
            StateEffect::RecoveryRequired
        );
    }

    #[test]
    fn directory_discard_preserves_typed_io_failure_as_related_diagnostic() {
        let stage_root = PathBuf::from("project/.directory-publish/write_back/stage");
        let source = DirectoryDiscardError::new(
            stage_root.clone(),
            Box::new(SystemFileSystemError::Io {
                operation: "discard_directory_candidate",
                path: stage_root.clone(),
                source: io::Error::from_raw_os_error(32),
            }),
        );
        let discard = generic_directory_discard_failure(source);
        let operation_report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackSourceChanged,
            )),
        );
        let error = GenericCommandError::PublishDiscard {
            operation: Box::new(GenericCommandError::reported(
                io::Error::other("input changed"),
                operation_report,
            )),
            discard,
        };

        assert!(matches!(
            Error::source(&error),
            Some(source) if source.downcast_ref::<GenericCommandError>().is_some()
        ));
        let report = generic_command_error_report(&error);
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Discard
        );
        let related = report.related()[0].report();
        assert_eq!(related.effect(), StateEffect::RecoveryRequired);
        assert_eq!(related.primary().code(), "publication.discard_failed");
        let wire = serde_json::to_string(&report).expect("丢弃诊断必须可序列化");
        assert!(wire.contains("\"operation\":\"remove\""));
        assert!(!wire.contains("discard_directory_candidate"));
        assert!(wire.contains("\"raw_os_code\":32"));
        assert!(wire.contains(&stage_root.to_string_lossy().to_string()));

        let GenericCommandError::PublishDiscard { discard, .. } = &error else {
            unreachable!("上方已建立 PublishDiscard")
        };
        let discard_source = discard
            .source_error()
            .downcast_ref::<DirectoryDiscardError<Box<SystemFileSystemError>>>()
            .expect("应保留真实目录丢弃错误");
        let SystemFileSystemError::Io { source, .. } = discard_source.source().as_ref() else {
            panic!("目录丢弃错误应保留文件系统 I/O 原因")
        };
        assert_eq!(source.raw_os_error(), Some(32));

        assert_eq!(
            generic_command_error_report(&error).effect(),
            StateEffect::RecoveryRequired
        );
    }

    #[test]
    fn unchanged_translation_resources_without_invalidations_skip_persistence() {
        assert!(!should_apply_translation_resources(
            r#"[{"term":"魔王"}]"#,
            r#"[{"kind":"rule","definition":{"order":"preserve","pattern":"\\\\N\\[\\d+\\]"}}]"#,
            r#"[{"term":"魔王"}]"#,
            r#"[{"kind":"rule","definition":{"order":"preserve","pattern":"\\\\N\\[\\d+\\]"}}]"#,
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
            r#"[{"kind":"rule","definition":{"order":"preserve","pattern":"old"}}]"#,
            "[]",
            r#"[{"kind":"rule","definition":{"order":"preserve","pattern":"new"}}]"#,
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
    fn zero_requests_with_only_current_rejected_units_is_incomplete() {
        let summary = GenericTranslationSummary {
            total_tasks: 0,
            started_tasks: 0,
            planned_units: 1,
            remaining_units: 1,
            ..GenericTranslationSummary::default()
        };

        assert!(summary.is_incomplete());
        assert_eq!(
            summary.total_tasks, 0,
            "默认重跑不得为 current Rejected 发请求"
        );
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
                        state_fingerprint: current_state,
                        expected_translation: None,
                    },
                    crate::generic::TranslationWrite {
                        group_id: invalid_current_group.id().to_owned(),
                        unit_id: invalid_current_unit.id().to_owned(),
                        expected_source_text: invalid_current_unit.source_text().to_owned(),
                        expected_group_context: invalid_current_group.context_fingerprint(),
                        translation: "损坏的已有译文".to_owned(),
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
        ));
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");
        let message = render_generic_user_message(&prepared.plan.tasks()[0], terminology.as_ref());
        let wire = compact_json(&message);

        assert!(wire.contains("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\""));
        assert_eq!(
            wire.matches("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\"")
                .count(),
            1,
            "TaskBlock 命中的术语必须按文件顺序合并后只提供一次"
        );
        assert!(wire.contains("\"kind\":\"dialogue\""));
        assert!(wire.contains("\"text\":[\"已有上下文 "));
        assert!(wire.contains("\"text\":[\"あ "));
        assert!(wire.contains("\"id\":\"0\""));
        assert!(wire.contains("\"id\":\"1\""));
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
    fn reused_translation_keeps_quote_style_when_reprotected_for_model_context() {
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
        apply_test_manual_translation(
            &store,
            TestManualTranslation {
                id: "scene.jsonl:line1:unit1:text",
                relative_path: "scene.jsonl",
                group_id: current_group.id(),
                unit_id: current_unit.id(),
                kind: current_group.kind(),
                source: current_unit.source_text(),
                translation: "“你好 {name}”",
            },
        );
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
        ));
        let terminology = Arc::new(CompiledTerminology::empty());
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            AutomaticStateResources {
                prompt: fingerprint(21),
                client_semantics: fingerprint(22),
                language_module: fingerprint(23),
                terminology_hits: empty_terminology_fingerprint(),
            },
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");

        assert_eq!(prepared.plan.reused().len(), 1);
        assert_eq!(prepared.plan.reused()[0].key().group_id(), "reuse");
        assert_eq!(
            prepared.plan.reused()[0].translation(),
            "“你好 {name}”",
            "Translate 验收不得改写合格译文的引号风格"
        );
        let task = &prepared.plan.tasks()[0];
        assert_eq!(task.groups().len(), 3);
        let current_context = task.groups()[0].units()[0].text();
        let reuse_context = task.groups()[1].units()[0].text();
        assert!(current_context.starts_with("“你好 ") && current_context.ends_with('”'));
        assert!(reuse_context.starts_with("“你好 ") && reuse_context.ends_with('”'));
        assert!(placeholder_token::contains_reserved_prefix(reuse_context));
        assert_eq!(task.groups()[1].units()[0].output_id(), None);
        assert_eq!(task.groups()[2].units()[0].output_id(), Some(task_id(0)));
        let message = render_generic_user_message(task, terminology.as_ref());
        let wire = compact_json(&message);
        assert!(wire.contains("“你好 "));
        assert!(wire.contains("\"id\":\"0\""));
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
        ));
        let target = NonZeroUsize::new(260).expect("常量应该非零");
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            AutomaticStateResources {
                prompt: fingerprint(31),
                client_semantics: fingerprint(32),
                language_module: fingerprint(33),
                terminology_hits: empty_terminology_fingerprint(),
            },
            target,
            false,
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
            .map(|message| compact_json(&message))
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
        apply_test_manual_translation(
            &store,
            TestManualTranslation {
                id: "scene.jsonl:line1:unit1:text",
                relative_path: "scene.jsonl",
                group_id: group.id(),
                unit_id: manual.id(),
                kind: group.kind(),
                source: manual.source_text(),
                translation: "人工译文",
            },
        );
        let automatic = &group.units()[1];
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[crate::generic::TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: automatic.id().to_owned(),
                    expected_source_text: automatic.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "无法证明语义的自动译文".to_owned(),
                    state_fingerprint: fingerprint(70),
                    expected_translation: None,
                }],
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
                .map(GenericCurrentTranslation::text),
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
    fn write_back_rejects_manual_translation_when_placeholder_rules_change() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"Open [A]."}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "current-placeholder-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\[[^]]+\]",
            )])
            .expect("Placeholder 规则应该合法");
        apply_test_manual_translation(
            &store,
            TestManualTranslation {
                id: "scene.jsonl:line1:unit1:text",
                relative_path: "scene.jsonl",
                group_id: group.id(),
                unit_id: unit.id(),
                kind: group.kind(),
                source: unit.source_text(),
                translation: "打开 [B]。",
            },
        );
        let (stored, live) = store
            .ensure_input_current()
            .expect("应该可重新读取当前 Generic 输入");
        let current = collect_generic_current_translations(
            &stored,
            &CompiledTerminology::empty(),
            &rules,
            None,
            &CooperativeCancellation::default(),
        )
        .expect("应该可读取数据库中的人工译文");
        let key = GenericUnitKey::new("group".to_owned(), "unit".to_owned());
        current
            .get_with_cancellation(&key, || Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
            .expect("人工译文应该仍为 Current");
        let layout_rules = compile_generic_layout_rules(
            &live,
            &LayoutRuleSet::from_canonical_json("[]").expect("空排版规则必须有效"),
        )
        .expect("空排版规则必须适用于任意 Generic 输入");
        let error = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &current,
            &rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(true, true),
            &CooperativeCancellation::default(),
        );
        assert!(
            matches!(
                error,
                Err(GenericWriteBackError::PlaceholderBindingMismatch { .. })
            ),
            "WriteBack 必须使用与 Translate 相同的强校验，不能因译文来源是 Manual 而跳过"
        );
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
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
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
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
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
            None,
            generic_terminal_occurrence_slot(),
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
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
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
            None,
            generic_terminal_occurrence_slot(),
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
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
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
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            Arc::clone(&language_module),
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
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
                    .map(|validated| validated.into_parts().0)
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
                    destination,
                    ..
                } if *output_id == 0
                    && destination.group_id.as_ref().is_some_and(|id| id.to_string() == "target")
                    && destination.unit_id.as_ref().is_some_and(|id| id.to_string() == "unit")
            )
        }));

        let source_group = &snapshot.files()[0].groups()[0];
        let source_unit = &source_group.units()[0];
        let protected = GenericPlaceholderService::default()
            .protect(source_group.kind(), source_unit.source_text(), &rules)
            .expect("源文没有命中 Placeholder");
        assert!(protected.placeholders().is_empty());
        apply_test_manual_translation(
            &store,
            TestManualTranslation {
                id: "scene.jsonl:line1:unit1:text",
                relative_path: "scene.jsonl",
                group_id: source_group.id(),
                unit_id: source_unit.id(),
                kind: source_group.kind(),
                source: source_unit.source_text(),
                translation: "你好 {invented}",
            },
        );
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let prepared = prepare_generic_translation(
            &snapshot,
            terminology,
            &rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            resources,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
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
        );
        let key = GenericUnitKey::new("group".to_owned(), "unit".to_owned());
        let mut facts = GenericUnitMap::new();
        let previous = facts
            .insert_with_cancellation(
                key.clone(),
                GenericValidationFact {
                    locator: GenericUnitLocator {
                        relative_path: PathBuf::from("scene.jsonl"),
                        group_id: "group".to_owned(),
                        unit_id: "unit".to_owned(),
                        role: "dialogue".to_owned(),
                        line: 1,
                        unit: 1,
                    },
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
            .expect("合法译文应该通过验收")
            .into_parts()
            .0,
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
        let residual = validate_generic_candidate(
            &key,
            &format!("こんにちは {token}"),
            &facts,
            &rules,
            &language_module,
        )
        .expect("源语言残留只进入 Review，不应丢弃合法候选");
        assert_eq!(residual.value(), "こんにちは {name}");
        assert_eq!(residual.reviews(), &[ReviewFinding::SourceResidual]);
    }
}
