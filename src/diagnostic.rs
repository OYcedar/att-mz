//! CLI 与项目日志共同消费的安全结构化诊断。
//!
//! 本模块刻意不提供从 `Display`、`Debug` 或错误来源链生成公开文本的入口。具体失败
//! 必须在仍持有类型化事实的边界选择公开代码、阶段、对象、原因、影响和恢复办法；原始
//! 错误只保留给进程内部的因果关系。

use std::error::Error;
use std::fmt;
use std::io;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::user_text::sanitize_user_text;

pub(crate) type BoxedError = Box<dyn Error + Send + Sync + 'static>;

/// 稳定、闭集的诊断代码。代码描述具体失败，不承担责任域分类职责。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum DiagnosticCode {
    #[serde(rename = "process.current_directory")]
    ProcessCurrentDirectory,
    #[serde(rename = "process.runtime_start")]
    ProcessRuntimeStart,
    #[serde(rename = "configuration.path")]
    ConfigurationPath,
    #[serde(rename = "configuration.open")]
    ConfigurationOpen,
    #[serde(rename = "configuration.read")]
    ConfigurationRead,
    #[serde(rename = "configuration.not_file")]
    ConfigurationNotFile,
    #[serde(rename = "configuration.invalid_utf8")]
    ConfigurationInvalidUtf8,
    #[serde(rename = "configuration.invalid_toml")]
    ConfigurationInvalidToml,
    #[serde(rename = "configuration.invalid_value")]
    ConfigurationInvalidValue,
    #[serde(rename = "configuration.profile_not_found")]
    ConfigurationProfileNotFound,
    #[serde(rename = "configuration.profile_conflict")]
    ConfigurationProfileConflict,
    #[serde(rename = "command.input")]
    CommandInput,
    #[serde(rename = "command.run_plan")]
    CommandRunPlan,
    #[serde(rename = "project.unavailable")]
    ProjectUnavailable,
    #[serde(rename = "project.state")]
    ProjectState,
    #[serde(rename = "prompt.unavailable")]
    PromptUnavailable,
    #[serde(rename = "language_module.unavailable")]
    LanguageModuleUnavailable,
    #[serde(rename = "model.request")]
    ModelRequest,
    #[serde(rename = "extract.builtin")]
    ExtractBuiltin,
    #[serde(rename = "extract.rules")]
    ExtractRules,
    #[serde(rename = "extract.document_read")]
    ExtractDocumentRead,
    #[serde(rename = "lua.execution")]
    LuaExecution,
    #[serde(rename = "lua.snapshot_store")]
    LuaSnapshotStore,
    #[serde(rename = "write_back.asset_read")]
    WriteBackAssetRead,
    #[serde(rename = "write_back.plan")]
    WriteBackPlan,
    #[serde(rename = "write_back.document_read")]
    WriteBackDocumentRead,
    #[serde(rename = "write_back.rewrite")]
    WriteBackRewrite,
    #[serde(rename = "write_back.candidate")]
    WriteBackCandidate,
    #[serde(rename = "write_back.validate")]
    WriteBackValidate,
    #[serde(rename = "write_back.publish")]
    WriteBackPublish,
    #[serde(rename = "write_back.discard")]
    WriteBackDiscard,
    #[serde(rename = "run_plan.save_failed")]
    RunPlanSaveFailed,
    #[serde(rename = "run_plan.outcome_unknown")]
    RunPlanOutcomeUnknown,
    #[serde(rename = "state.finalization_failed")]
    StateFinalizationFailed,
    #[serde(rename = "operation.outcome_unknown")]
    OperationOutcomeUnknown,
    #[serde(rename = "signal.registration")]
    SignalRegistration,
    #[serde(rename = "shutdown.component")]
    ShutdownComponent,
    #[serde(rename = "internal.operation")]
    InternalOperation,
    #[serde(rename = "filesystem.build")]
    FileSystemBuild,
    #[serde(rename = "filesystem.operation")]
    FileSystemOperation,
    #[serde(rename = "sqlite.operation")]
    SqliteOperation,
    #[serde(rename = "http.client_build")]
    HttpClientBuild,
    #[serde(rename = "log.start")]
    LogStart,
    #[serde(rename = "log.serialize")]
    LogSerialize,
    #[serde(rename = "log.write")]
    LogWrite,
    #[serde(rename = "log.flush")]
    LogFlush,
    #[serde(rename = "log.sync")]
    LogSync,
    #[serde(rename = "log.worker")]
    LogWorker,
}

impl DiagnosticCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessCurrentDirectory => "process.current_directory",
            Self::ProcessRuntimeStart => "process.runtime_start",
            Self::ConfigurationPath => "configuration.path",
            Self::ConfigurationOpen => "configuration.open",
            Self::ConfigurationRead => "configuration.read",
            Self::ConfigurationNotFile => "configuration.not_file",
            Self::ConfigurationInvalidUtf8 => "configuration.invalid_utf8",
            Self::ConfigurationInvalidToml => "configuration.invalid_toml",
            Self::ConfigurationInvalidValue => "configuration.invalid_value",
            Self::ConfigurationProfileNotFound => "configuration.profile_not_found",
            Self::ConfigurationProfileConflict => "configuration.profile_conflict",
            Self::CommandInput => "command.input",
            Self::CommandRunPlan => "command.run_plan",
            Self::ProjectUnavailable => "project.unavailable",
            Self::ProjectState => "project.state",
            Self::PromptUnavailable => "prompt.unavailable",
            Self::LanguageModuleUnavailable => "language_module.unavailable",
            Self::ModelRequest => "model.request",
            Self::ExtractBuiltin => "extract.builtin",
            Self::ExtractRules => "extract.rules",
            Self::ExtractDocumentRead => "extract.document_read",
            Self::LuaExecution => "lua.execution",
            Self::LuaSnapshotStore => "lua.snapshot_store",
            Self::WriteBackAssetRead => "write_back.asset_read",
            Self::WriteBackPlan => "write_back.plan",
            Self::WriteBackDocumentRead => "write_back.document_read",
            Self::WriteBackRewrite => "write_back.rewrite",
            Self::WriteBackCandidate => "write_back.candidate",
            Self::WriteBackValidate => "write_back.validate",
            Self::WriteBackPublish => "write_back.publish",
            Self::WriteBackDiscard => "write_back.discard",
            Self::RunPlanSaveFailed => "run_plan.save_failed",
            Self::RunPlanOutcomeUnknown => "run_plan.outcome_unknown",
            Self::StateFinalizationFailed => "state.finalization_failed",
            Self::OperationOutcomeUnknown => "operation.outcome_unknown",
            Self::SignalRegistration => "signal.registration",
            Self::ShutdownComponent => "shutdown.component",
            Self::InternalOperation => "internal.operation",
            Self::FileSystemBuild => "filesystem.build",
            Self::FileSystemOperation => "filesystem.operation",
            Self::SqliteOperation => "sqlite.operation",
            Self::HttpClientBuild => "http.client_build",
            Self::LogStart => "log.start",
            Self::LogSerialize => "log.serialize",
            Self::LogWrite => "log.write",
            Self::LogFlush => "log.flush",
            Self::LogSync => "log.sync",
            Self::LogWorker => "log.worker",
        }
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticStage {
    ProcessStartup,
    ProcessOutput,
    Configuration,
    CommandPreparation,
    ProjectOpening,
    Init,
    Extract,
    Translate,
    WriteBack,
    Lua,
    ModelRequest,
    RunPlanFinalization,
    Publication,
    Shutdown,
    Logging,
}

impl DiagnosticStage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessStartup => "process_startup",
            Self::ProcessOutput => "process_output",
            Self::Configuration => "configuration",
            Self::CommandPreparation => "command_preparation",
            Self::ProjectOpening => "project_opening",
            Self::Init => "init",
            Self::Extract => "extract",
            Self::Translate => "translate",
            Self::WriteBack => "write_back",
            Self::Lua => "lua",
            Self::ModelRequest => "model_request",
            Self::RunPlanFinalization => "run_plan_finalization",
            Self::Publication => "publication",
            Self::Shutdown => "shutdown",
            Self::Logging => "logging",
        }
    }
}

/// 可公开的失败对象。所有动态文本在构造时清理控制字符。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum DiagnosticSubject {
    Process,
    Command { name: String },
    Path { path: String },
    Field { field: String },
    Project { name: String },
    Profile { id: String },
    Component { name: String },
    Operation { name: String },
}

impl DiagnosticSubject {
    pub(crate) fn command(name: impl AsRef<str>) -> Self {
        Self::Command {
            name: sanitize_user_text(name.as_ref()),
        }
    }

    pub(crate) fn path(path: impl AsRef<Path>) -> Self {
        Self::Path {
            path: sanitize_user_text(&path.as_ref().to_string_lossy()),
        }
    }

    pub(crate) fn field(field: impl AsRef<str>) -> Self {
        Self::Field {
            field: sanitize_user_text(field.as_ref()),
        }
    }

    pub(crate) fn profile(id: impl AsRef<str>) -> Self {
        Self::Profile {
            id: sanitize_user_text(id.as_ref()),
        }
    }

    pub(crate) fn component(name: impl AsRef<str>) -> Self {
        Self::Component {
            name: sanitize_user_text(name.as_ref()),
        }
    }

    pub(crate) fn operation(name: impl AsRef<str>) -> Self {
        Self::Operation {
            name: sanitize_user_text(name.as_ref()),
        }
    }

    fn render_localized(&self, localizer: &UiLocalizer) -> String {
        let (kind, value) = match self {
            Self::Process => return "ATT".to_owned(),
            Self::Path { path } => return path.clone(),
            Self::Operation { name } => return name.clone(),
            Self::Command { name } => ("command", name.as_str()),
            Self::Field { field } => ("field", field.as_str()),
            Self::Project { name } => ("project", name.as_str()),
            Self::Profile { id } => ("profile", id.as_str()),
            Self::Component { name } => ("component", name.as_str()),
        };
        localizer.format(UiMessage::DiagnosticSubjectValue { kind, value })
    }

    fn sanitized(self) -> Self {
        match self {
            Self::Process => Self::Process,
            Self::Command { name } => Self::command(name),
            Self::Path { path } => Self::Path {
                path: sanitize_user_text(&path),
            },
            Self::Field { field } => Self::field(field),
            Self::Project { name } => Self::Project {
                name: sanitize_user_text(&name),
            },
            Self::Profile { id } => Self::profile(id),
            Self::Component { name } => Self::component(name),
            Self::Operation { name } => Self::operation(name),
        }
    }

    fn map_dynamic_text<F>(self, map: &mut F) -> Self
    where
        F: FnMut(&str) -> String,
    {
        let map_text = |value: String, map: &mut F| sanitize_user_text(&map(&value));
        match self {
            Self::Process => Self::Process,
            Self::Command { name } => Self::Command {
                name: map_text(name, map),
            },
            Self::Path { path } => Self::Path {
                path: map_text(path, map),
            },
            Self::Field { field } => Self::Field {
                field: map_text(field, map),
            },
            Self::Project { name } => Self::Project {
                name: map_text(name, map),
            },
            Self::Profile { id } => Self::Profile {
                id: map_text(id, map),
            },
            Self::Component { name } => Self::Component {
                name: map_text(name, map),
            },
            Self::Operation { name } => Self::Operation {
                name: map_text(name, map),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SafeIoKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionAborted,
    NotConnected,
    AddressInUse,
    AddressNotAvailable,
    NetworkDown,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StaleNetworkFileHandle,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    StorageFull,
    NotSeekable,
    QuotaExceeded,
    FileTooLarge,
    ResourceBusy,
    ExecutableFileBusy,
    Deadlock,
    CrossesDevices,
    TooManyLinks,
    InvalidFilename,
    ArgumentListTooLong,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
}

impl From<io::ErrorKind> for SafeIoKind {
    fn from(value: io::ErrorKind) -> Self {
        match value {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::AddrInUse => Self::AddressInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddressNotAvailable,
            io::ErrorKind::NetworkDown => Self::NetworkDown,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::NotADirectory => Self::NotADirectory,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            io::ErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::StorageFull => Self::StorageFull,
            io::ErrorKind::NotSeekable => Self::NotSeekable,
            io::ErrorKind::QuotaExceeded => Self::QuotaExceeded,
            io::ErrorKind::FileTooLarge => Self::FileTooLarge,
            io::ErrorKind::ResourceBusy => Self::ResourceBusy,
            io::ErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            io::ErrorKind::Deadlock => Self::Deadlock,
            io::ErrorKind::CrossesDevices => Self::CrossesDevices,
            io::ErrorKind::TooManyLinks => Self::TooManyLinks,
            io::ErrorKind::InvalidFilename => Self::InvalidFilename,
            io::ErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            _ => Self::Other,
        }
    }
}

impl SafeIoKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::ConnectionRefused => "connection_refused",
            Self::ConnectionReset => "connection_reset",
            Self::HostUnreachable => "host_unreachable",
            Self::NetworkUnreachable => "network_unreachable",
            Self::ConnectionAborted => "connection_aborted",
            Self::NotConnected => "not_connected",
            Self::AddressInUse => "address_in_use",
            Self::AddressNotAvailable => "address_not_available",
            Self::NetworkDown => "network_down",
            Self::BrokenPipe => "broken_pipe",
            Self::AlreadyExists => "already_exists",
            Self::WouldBlock => "would_block",
            Self::NotADirectory => "not_a_directory",
            Self::IsADirectory => "is_a_directory",
            Self::DirectoryNotEmpty => "directory_not_empty",
            Self::ReadOnlyFilesystem => "read_only_filesystem",
            Self::StaleNetworkFileHandle => "stale_network_file_handle",
            Self::InvalidInput => "invalid_input",
            Self::InvalidData => "invalid_data",
            Self::TimedOut => "timed_out",
            Self::WriteZero => "write_zero",
            Self::StorageFull => "storage_full",
            Self::NotSeekable => "not_seekable",
            Self::QuotaExceeded => "quota_exceeded",
            Self::FileTooLarge => "file_too_large",
            Self::ResourceBusy => "resource_busy",
            Self::ExecutableFileBusy => "executable_file_busy",
            Self::Deadlock => "deadlock",
            Self::CrossesDevices => "crosses_devices",
            Self::TooManyLinks => "too_many_links",
            Self::InvalidFilename => "invalid_filename",
            Self::ArgumentListTooLong => "argument_list_too_long",
            Self::Interrupted => "interrupted",
            Self::Unsupported => "unsupported",
            Self::UnexpectedEof => "unexpected_eof",
            Self::OutOfMemory => "out_of_memory",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticFailureKind {
    MissingRequiredValue,
    ExtractPlanRequired,
    ConflictingValues,
    InvalidSyntax,
    InvalidEncoding,
    InvalidValue,
    NotFound,
    Busy,
    StateMismatch,
    RequirementFailed,
    TransactionRolledBack,
    TransactionOutcomeUnknown,
    FinalizationFailed,
    RollbackFailed,
    ExternalServiceRejected,
    ExternalServiceUnavailable,
    ExecutorClosed,
    ConcurrentShutdown,
    ExecutorStatePoisoned,
    WorkerSpawnFailed,
    WorkerChannelClosed,
    WorkerPanicked,
    ReparsePointForbidden,
    NonLocalVolume,
    NonNtfsVolume,
    CaseSensitiveDirectory,
    LockCancelled,
    TargetAlreadyExists,
    FileIdentityChanged,
    InvalidPath,
    WrongPublisherInstance,
    JournalCorrupt,
    UnexpectedArtifact,
    InteractiveSessionAlreadyOpen,
    BackupIncomplete,
    RequestSerializationFailed,
    ResponseParsingFailed,
    InvalidResponseContract,
    TransportFailed,
    LuaDatabaseOpenFailed,
    LuaContextCreationFailed,
    LuaCompilationFailed,
    LuaExecutionFailed,
    LuaHostCallFailed,
    LuaFinalizationFailed,
    LuaUnclosedTransaction,
    LuaSnapshotStoreFailed,
    RulesDefinitionInvalid,
    RulesDocumentReadFailed,
    RulesNoNonBlankMatch,
    RulesInvalidTarget,
    RulesPatternMatchFailed,
    RulesZeroWidthMatch,
    RulesOverlappingCapture,
    RulesMissingTextCapture,
    RulesInvalidCaptureRange,
    RulesDuplicateTarget,
    RulesInvalidMaterialization,
    RulesSnapshotInvalid,
    RulesSnapshotStoreFailed,
    WriteBackExtractionOutOfDate,
    WriteBackAssetSnapshotInvalid,
    SourceDocumentInvalid,
    WriteBackMutationInvalid,
    WriteBackOutputPathInvalid,
    WriteBackOutputPathDuplicate,
    WriteBackCandidateProjectMismatch,
    WriteBackCandidateInvalid,
    WriteBackUnexpectedLuaOutcome,
    WriteBackNotPublished,
    WriteBackPublishedWithResiduals,
    WriteBackRecoveryRequired,
    InternalInvariant,
}

impl DiagnosticFailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingRequiredValue => "missing_required_value",
            Self::ExtractPlanRequired => "extract_plan_required",
            Self::ConflictingValues => "conflicting_values",
            Self::InvalidSyntax => "invalid_syntax",
            Self::InvalidEncoding => "invalid_encoding",
            Self::InvalidValue => "invalid_value",
            Self::NotFound => "not_found",
            Self::Busy => "busy",
            Self::StateMismatch => "state_mismatch",
            Self::RequirementFailed => "requirement_failed",
            Self::TransactionRolledBack => "transaction_rolled_back",
            Self::TransactionOutcomeUnknown => "transaction_outcome_unknown",
            Self::FinalizationFailed => "finalization_failed",
            Self::RollbackFailed => "rollback_failed",
            Self::ExternalServiceRejected => "external_service_rejected",
            Self::ExternalServiceUnavailable => "external_service_unavailable",
            Self::ExecutorClosed => "executor_closed",
            Self::ConcurrentShutdown => "concurrent_shutdown",
            Self::ExecutorStatePoisoned => "executor_state_poisoned",
            Self::WorkerSpawnFailed => "worker_spawn_failed",
            Self::WorkerChannelClosed => "worker_channel_closed",
            Self::WorkerPanicked => "worker_panicked",
            Self::ReparsePointForbidden => "reparse_point_forbidden",
            Self::NonLocalVolume => "non_local_volume",
            Self::NonNtfsVolume => "non_ntfs_volume",
            Self::CaseSensitiveDirectory => "case_sensitive_directory",
            Self::LockCancelled => "lock_cancelled",
            Self::TargetAlreadyExists => "target_already_exists",
            Self::FileIdentityChanged => "file_identity_changed",
            Self::InvalidPath => "invalid_path",
            Self::WrongPublisherInstance => "wrong_publisher_instance",
            Self::JournalCorrupt => "journal_corrupt",
            Self::UnexpectedArtifact => "unexpected_artifact",
            Self::InteractiveSessionAlreadyOpen => "interactive_session_already_open",
            Self::BackupIncomplete => "backup_incomplete",
            Self::RequestSerializationFailed => "request_serialization_failed",
            Self::ResponseParsingFailed => "response_parsing_failed",
            Self::InvalidResponseContract => "invalid_response_contract",
            Self::TransportFailed => "transport_failed",
            Self::LuaDatabaseOpenFailed => "lua_database_open_failed",
            Self::LuaContextCreationFailed => "lua_context_creation_failed",
            Self::LuaCompilationFailed => "lua_compilation_failed",
            Self::LuaExecutionFailed => "lua_execution_failed",
            Self::LuaHostCallFailed => "lua_host_call_failed",
            Self::LuaFinalizationFailed => "lua_finalization_failed",
            Self::LuaUnclosedTransaction => "lua_unclosed_transaction",
            Self::LuaSnapshotStoreFailed => "lua_snapshot_store_failed",
            Self::RulesDefinitionInvalid => "rules_definition_invalid",
            Self::RulesDocumentReadFailed => "rules_document_read_failed",
            Self::RulesNoNonBlankMatch => "rules_no_non_blank_match",
            Self::RulesInvalidTarget => "rules_invalid_target",
            Self::RulesPatternMatchFailed => "rules_pattern_match_failed",
            Self::RulesZeroWidthMatch => "rules_zero_width_match",
            Self::RulesOverlappingCapture => "rules_overlapping_capture",
            Self::RulesMissingTextCapture => "rules_missing_text_capture",
            Self::RulesInvalidCaptureRange => "rules_invalid_capture_range",
            Self::RulesDuplicateTarget => "rules_duplicate_target",
            Self::RulesInvalidMaterialization => "rules_invalid_materialization",
            Self::RulesSnapshotInvalid => "rules_snapshot_invalid",
            Self::RulesSnapshotStoreFailed => "rules_snapshot_store_failed",
            Self::WriteBackExtractionOutOfDate => "write_back_extraction_out_of_date",
            Self::WriteBackAssetSnapshotInvalid => "write_back_asset_snapshot_invalid",
            Self::SourceDocumentInvalid => "source_document_invalid",
            Self::WriteBackMutationInvalid => "write_back_mutation_invalid",
            Self::WriteBackOutputPathInvalid => "write_back_output_path_invalid",
            Self::WriteBackOutputPathDuplicate => "write_back_output_path_duplicate",
            Self::WriteBackCandidateProjectMismatch => "write_back_candidate_project_mismatch",
            Self::WriteBackCandidateInvalid => "write_back_candidate_invalid",
            Self::WriteBackUnexpectedLuaOutcome => "write_back_unexpected_lua_outcome",
            Self::WriteBackNotPublished => "write_back_not_published",
            Self::WriteBackPublishedWithResiduals => "write_back_published_with_residuals",
            Self::WriteBackRecoveryRequired => "write_back_recovery_required",
            Self::InternalInvariant => "internal_invariant",
        }
    }
}

/// 允许拥有具体底层错误类型的根把稳定事实交给领域包装错误。
///
/// 该契约没有 `Display`/`source` 回退；实现必须直接读取自身的类型化字段。领域包装层
/// 只补充业务阶段或对象，不能借此解析任意错误文本。
pub(crate) trait SafeDiagnosticSource {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic;

    fn into_failure_report(
        self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> FailureReport
    where
        Self: Error + Send + Sync + Sized + 'static,
    {
        let public = self.safe_diagnostic_source(stage, impact, fallback_action);
        FailureReport::new(ReportedFailure::new(public, self))
    }
}

impl<T> SafeDiagnosticSource for Box<T>
where
    T: SafeDiagnosticSource + ?Sized,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        self.as_ref()
            .safe_diagnostic_source(stage, impact, fallback_action)
    }
}

/// 配置值失败的稳定闭集。这里只保存校验规则和安全的数值事实，绝不保存配置正文。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfigurationValueRule {
    RuntimeConfigurationInvalid,
    UnsupportedPromptLocale,
    LanguagePolicyTermBlank,
    LanguagePolicyTermSurroundingWhitespace,
    LanguagePolicyTermDuplicate,
    QuoteRepairCandidatesEmpty,
    QuoteRepairDelimiterInvalid,
    QuoteRepairPairDuplicate,
    QuoteRepairDelimiterAmbiguous,
    LanguageIdBlank,
    LanguageIdSurroundingWhitespace,
    LanguageIdUsesUnderscore,
    LanguageIdInvalidSyntax,
    LanguageIdInvalidRegistryTag,
    LanguageIdCanonicalizationFailed,
    LanguageIdUndefinedPrimaryLanguage,
    LanguageIdDuplicate,
    LanguageCatalogEmpty,
    UrlInvalid,
    UrlCredentialsForbidden,
    UrlFragmentForbidden,
    UrlSchemeUnsupported,
    ApiKeyBlank,
    ApiKeySurroundingWhitespace,
    ApiKeyInvalidHeader,
    StrictJsonInvalid { line: u64, column: u64 },
    JsonObjectRequired,
    ReservedRequestField,
    ProxyMustBeFalseOrUrl,
    PemPathDuplicate,
    RuntimeMaximumExceeded { actual: u64, maximum: u64 },
    ValueSurroundingWhitespace,
    ValueBlank,
    PathBlank,
    PositiveRequired { actual: u64 },
    UsizeRangeExceeded { actual: u64 },
    U32RangeExceeded { actual: u64 },
    DuplicateProfileId,
    SelectedProfileInvalid,
    ReferencedClientNotFound,
}

impl ConfigurationValueRule {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::RuntimeConfigurationInvalid => "runtime_configuration_invalid",
            Self::UnsupportedPromptLocale => "unsupported_prompt_locale",
            Self::LanguagePolicyTermBlank => "language_policy_term_blank",
            Self::LanguagePolicyTermSurroundingWhitespace => {
                "language_policy_term_surrounding_whitespace"
            }
            Self::LanguagePolicyTermDuplicate => "language_policy_term_duplicate",
            Self::QuoteRepairCandidatesEmpty => "quote_repair_candidates_empty",
            Self::QuoteRepairDelimiterInvalid => "quote_repair_delimiter_invalid",
            Self::QuoteRepairPairDuplicate => "quote_repair_pair_duplicate",
            Self::QuoteRepairDelimiterAmbiguous => "quote_repair_delimiter_ambiguous",
            Self::LanguageIdBlank => "language_id_blank",
            Self::LanguageIdSurroundingWhitespace => "language_id_surrounding_whitespace",
            Self::LanguageIdUsesUnderscore => "language_id_uses_underscore",
            Self::LanguageIdInvalidSyntax => "language_id_invalid_syntax",
            Self::LanguageIdInvalidRegistryTag => "language_id_invalid_registry_tag",
            Self::LanguageIdCanonicalizationFailed => "language_id_canonicalization_failed",
            Self::LanguageIdUndefinedPrimaryLanguage => "language_id_undefined_primary_language",
            Self::LanguageIdDuplicate => "language_id_duplicate",
            Self::LanguageCatalogEmpty => "language_catalog_empty",
            Self::UrlInvalid => "url_invalid",
            Self::UrlCredentialsForbidden => "url_credentials_forbidden",
            Self::UrlFragmentForbidden => "url_fragment_forbidden",
            Self::UrlSchemeUnsupported => "url_scheme_unsupported",
            Self::ApiKeyBlank => "api_key_blank",
            Self::ApiKeySurroundingWhitespace => "api_key_surrounding_whitespace",
            Self::ApiKeyInvalidHeader => "api_key_invalid_header",
            Self::StrictJsonInvalid { .. } => "strict_json_invalid",
            Self::JsonObjectRequired => "json_object_required",
            Self::ReservedRequestField => "reserved_request_field",
            Self::ProxyMustBeFalseOrUrl => "proxy_must_be_false_or_url",
            Self::PemPathDuplicate => "pem_path_duplicate",
            Self::RuntimeMaximumExceeded { .. } => "runtime_maximum_exceeded",
            Self::ValueSurroundingWhitespace => "value_surrounding_whitespace",
            Self::ValueBlank => "value_blank",
            Self::PathBlank => "path_blank",
            Self::PositiveRequired { .. } => "positive_required",
            Self::UsizeRangeExceeded { .. } => "usize_range_exceeded",
            Self::U32RangeExceeded { .. } => "u32_range_exceeded",
            Self::DuplicateProfileId => "duplicate_profile_id",
            Self::SelectedProfileInvalid => "selected_profile_invalid",
            Self::ReferencedClientNotFound => "referenced_client_not_found",
        }
    }

    const fn fluent_facts(&self) -> (u64, u64, u64, u64) {
        match self {
            Self::StrictJsonInvalid { line, column } => (*line, *column, 0, 0),
            Self::RuntimeMaximumExceeded { actual, maximum } => (0, 0, *actual, *maximum),
            Self::PositiveRequired { actual }
            | Self::UsizeRangeExceeded { actual }
            | Self::U32RangeExceeded { actual } => (0, 0, *actual, 0),
            _ => (0, 0, 0, 0),
        }
    }

    pub(crate) fn render(&self) -> String {
        self.render_localized(&UiLocalizer::new(UiLocale::English))
    }

    fn render_localized(&self, localizer: &UiLocalizer) -> String {
        let (line, column, actual, maximum) = self.fluent_facts();
        localizer.format(UiMessage::DiagnosticConfigurationRuleValue {
            code: self.as_str(),
            line,
            column,
            actual,
            maximum,
        })
    }
}

/// TOML 字段契约允许的值形态；该闭集只用于安全诊断，不包含配置正文。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfigurationTomlValueKind {
    String,
    Integer,
    Boolean,
    StringOrBoolean,
    StringArray,
    IntegerArray,
    StringPairArray,
    Table,
    TableArray,
}

impl ConfigurationTomlValueKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::StringOrBoolean => "string_or_boolean",
            Self::StringArray => "string_array",
            Self::IntegerArray => "integer_array",
            Self::StringPairArray => "string_pair_array",
            Self::Table => "table",
            Self::TableArray => "table_array",
        }
    }

    fn render_localized(self, localizer: &UiLocalizer) -> String {
        localizer.format(UiMessage::DiagnosticTomlExpectedKindValue {
            code: self.as_str(),
        })
    }
}

/// TOML 解析与字段契约失败的稳定闭集。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum ConfigurationTomlFailureKind {
    Syntax,
    MissingField,
    UnknownField,
    DuplicateField,
    TypeMismatch {
        expected: ConfigurationTomlValueKind,
    },
    InvalidValue,
}

impl ConfigurationTomlFailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::MissingField => "missing_field",
            Self::UnknownField => "unknown_field",
            Self::DuplicateField => "duplicate_field",
            Self::TypeMismatch { .. } => "type_mismatch",
            Self::InvalidValue => "invalid_value",
        }
    }

    fn render_localized(self, localizer: &UiLocalizer) -> String {
        let expected = match self {
            Self::TypeMismatch { expected } => expected.render_localized(localizer),
            _ => String::new(),
        };
        localizer.format(UiMessage::DiagnosticTomlFailureValue {
            code: self.as_str(),
            expected: &expected,
        })
    }
}

/// 可公开原因。自由文本只允许来自操作系统按稳定错误码重新生成的系统消息。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum DiagnosticReason {
    Io {
        operation: String,
        error_kind: SafeIoKind,
        raw_os_code: Option<i32>,
        system_message: Option<String>,
    },
    Failure {
        failure: DiagnosticFailureKind,
    },
    FailureWithDetail {
        failure: DiagnosticFailureKind,
        detail: String,
    },
    InvalidUtf8 {
        valid_up_to: u64,
        error_len: Option<u64>,
    },
    InvalidToml {
        line: Option<u64>,
        column: Option<u64>,
        resource: String,
        failure: ConfigurationTomlFailureKind,
    },
    InvalidConfigurationValue {
        rule: ConfigurationValueRule,
    },
    Http {
        status: Option<u16>,
        retry_after_seconds: Option<u64>,
        provider_code: Option<String>,
        provider_type: Option<String>,
    },
    Sqlite {
        primary_code: i32,
        extended_code: i32,
    },
    WindowsStatus {
        operation: String,
        status: i32,
    },
    Resource {
        resource: String,
        actual: u64,
        maximum: Option<u64>,
    },
}

impl DiagnosticReason {
    pub(crate) fn io(operation: impl AsRef<str>, source: &io::Error) -> Self {
        let raw_os_code = source.raw_os_error();
        let system_message = raw_os_code.map(|code| {
            // 只从稳定 OS code 重新生成系统消息；不读取任意、未结构化的 source 文本。
            sanitize_user_text(&io::Error::from_raw_os_error(code).to_string())
        });
        Self::Io {
            operation: sanitize_user_text(operation.as_ref()),
            error_kind: source.kind().into(),
            raw_os_code,
            system_message,
        }
    }

    pub(crate) const fn failure(failure: DiagnosticFailureKind) -> Self {
        Self::Failure { failure }
    }

    pub(crate) const fn is_wait_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Failure {
                failure: DiagnosticFailureKind::LockCancelled
            } | Self::FailureWithDetail {
                failure: DiagnosticFailureKind::LockCancelled,
                ..
            }
        )
    }

    /// 追加已经由类型化根判定可公开的机制详情。
    ///
    /// 调用方只能传入有界、类型化的机制事实，不得复制任意业务载荷或错误链文本。
    pub(crate) fn failure_with_detail(
        failure: DiagnosticFailureKind,
        detail: impl AsRef<str>,
    ) -> Self {
        Self::FailureWithDetail {
            failure,
            detail: sanitize_user_text(detail.as_ref()),
        }
    }

    fn sanitized(self) -> Self {
        match self {
            Self::Io {
                operation,
                error_kind,
                raw_os_code,
                system_message,
            } => Self::Io {
                operation: sanitize_user_text(&operation),
                error_kind,
                raw_os_code,
                system_message: system_message.map(|message| sanitize_user_text(&message)),
            },
            Self::InvalidToml {
                line,
                column,
                resource,
                failure,
            } => Self::InvalidToml {
                line,
                column,
                resource: sanitize_user_text(&resource),
                failure,
            },
            Self::Http {
                status,
                retry_after_seconds,
                provider_code,
                provider_type,
            } => Self::Http {
                status,
                retry_after_seconds,
                provider_code: provider_code.and_then(safe_provider_identifier),
                provider_type: provider_type.and_then(safe_provider_identifier),
            },
            Self::WindowsStatus { operation, status } => Self::WindowsStatus {
                operation: sanitize_user_text(&operation),
                status,
            },
            Self::Resource {
                resource,
                actual,
                maximum,
            } => Self::Resource {
                resource: sanitize_user_text(&resource),
                actual,
                maximum,
            },
            Self::FailureWithDetail { failure, detail } => Self::FailureWithDetail {
                failure,
                detail: sanitize_user_text(&detail),
            },
            reason => reason,
        }
    }

    fn map_dynamic_text<F>(self, map: &mut F) -> Self
    where
        F: FnMut(&str) -> String,
    {
        let map_text = |value: String, map: &mut F| sanitize_user_text(&map(&value));
        match self {
            Self::Io {
                operation,
                error_kind,
                raw_os_code,
                system_message,
            } => Self::Io {
                operation: map_text(operation, map),
                error_kind,
                raw_os_code,
                system_message: system_message.map(|message| map_text(message, map)),
            },
            Self::Failure { failure } => Self::Failure { failure },
            Self::FailureWithDetail { failure, detail } => Self::FailureWithDetail {
                failure,
                detail: map_text(detail, map),
            },
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            },
            Self::InvalidToml {
                line,
                column,
                resource,
                failure,
            } => Self::InvalidToml {
                line,
                column,
                resource: map_text(resource, map),
                failure,
            },
            Self::InvalidConfigurationValue { rule } => Self::InvalidConfigurationValue { rule },
            Self::Http {
                status,
                retry_after_seconds,
                provider_code,
                provider_type,
            } => Self::Http {
                status,
                retry_after_seconds,
                provider_code: provider_code.map(|value| map_text(value, map)),
                provider_type: provider_type.map(|value| map_text(value, map)),
            },
            Self::Sqlite {
                primary_code,
                extended_code,
            } => Self::Sqlite {
                primary_code,
                extended_code,
            },
            Self::WindowsStatus { operation, status } => Self::WindowsStatus {
                operation: map_text(operation, map),
                status,
            },
            Self::Resource {
                resource,
                actual,
                maximum,
            } => Self::Resource {
                resource: map_text(resource, map),
                actual,
                maximum,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn render(&self) -> String {
        self.render_localized(&UiLocalizer::new(UiLocale::English))
    }

    pub(crate) fn render_localized(&self, localizer: &UiLocalizer) -> String {
        match self {
            Self::Io {
                operation,
                error_kind,
                raw_os_code,
                system_message,
            } => {
                let kind = localizer.format(UiMessage::DiagnosticIoKindValue {
                    code: error_kind.as_str(),
                });
                match (raw_os_code, system_message) {
                    (Some(os_code), Some(system_message)) => {
                        let os_code = os_code.to_string();
                        localizer.format(UiMessage::DiagnosticIoReasonWithOsCodeAndSystemMessage {
                            operation,
                            kind: &kind,
                            os_code: &os_code,
                            system_message,
                        })
                    }
                    (Some(os_code), None) => {
                        let os_code = os_code.to_string();
                        localizer.format(UiMessage::DiagnosticIoReasonWithOsCode {
                            operation,
                            kind: &kind,
                            os_code: &os_code,
                        })
                    }
                    (None, Some(system_message)) => {
                        localizer.format(UiMessage::DiagnosticIoReasonWithSystemMessage {
                            operation,
                            kind: &kind,
                            system_message,
                        })
                    }
                    (None, None) => localizer.format(UiMessage::DiagnosticIoReason {
                        operation,
                        kind: &kind,
                    }),
                }
            }
            Self::Failure { failure } => localizer.format(UiMessage::DiagnosticFailureValue {
                code: failure.as_str(),
            }),
            Self::FailureWithDetail { failure, detail } => {
                let summary = localizer.format(UiMessage::DiagnosticFailureValue {
                    code: failure.as_str(),
                });
                localizer.format(UiMessage::DiagnosticFailureWithDetail {
                    failure: &summary,
                    detail,
                })
            }
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => match error_len {
                Some(error_len) => localizer.format(UiMessage::DiagnosticInvalidUtf8 {
                    valid_up_to: *valid_up_to,
                    error_len: *error_len,
                }),
                None => localizer.format(UiMessage::DiagnosticIncompleteUtf8 {
                    valid_up_to: *valid_up_to,
                }),
            },
            Self::InvalidToml {
                line,
                column,
                resource,
                failure,
            } => {
                let failure = failure.render_localized(localizer);
                match (line, column) {
                    (Some(line), Some(column)) => {
                        localizer.format(UiMessage::DiagnosticInvalidTomlAt {
                            line: *line,
                            column: *column,
                            resource,
                            failure: &failure,
                        })
                    }
                    _ => localizer.format(UiMessage::DiagnosticInvalidToml {
                        resource,
                        failure: &failure,
                    }),
                }
            }
            Self::InvalidConfigurationValue { rule } => rule.render_localized(localizer),
            Self::Http {
                status,
                retry_after_seconds,
                provider_code,
                provider_type,
            } => {
                let mut facts = Vec::new();
                if let Some(status) = status {
                    facts.push(localizer.format(UiMessage::DiagnosticHttpStatus {
                        status: u64::from(*status),
                    }));
                }
                if let Some(seconds) = retry_after_seconds {
                    facts.push(
                        localizer.format(UiMessage::DiagnosticHttpRetryAfter { seconds: *seconds }),
                    );
                }
                if let Some(code) = provider_code {
                    facts.push(localizer.format(UiMessage::DiagnosticHttpProviderCode { code }));
                }
                if let Some(kind) = provider_type {
                    facts.push(localizer.format(UiMessage::DiagnosticHttpProviderType { kind }));
                }
                if facts.is_empty() {
                    localizer.format(UiMessage::DiagnosticHttpNoDetails)
                } else {
                    facts.join(&localizer.format(UiMessage::DiagnosticHttpFactSeparator))
                }
            }
            Self::Sqlite {
                primary_code,
                extended_code,
            } => {
                let primary_code = primary_code.to_string();
                let extended_code = extended_code.to_string();
                localizer.format(UiMessage::DiagnosticSqlite {
                    primary_code: &primary_code,
                    extended_code: &extended_code,
                })
            }
            Self::WindowsStatus { operation, status } => {
                let status = format!("{status:#010x}");
                localizer.format(UiMessage::DiagnosticWindowsStatus {
                    operation,
                    status: &status,
                })
            }
            Self::Resource {
                resource,
                actual,
                maximum,
            } => match maximum {
                Some(maximum) => localizer.format(UiMessage::DiagnosticResourceWithMaximum {
                    resource,
                    actual: *actual,
                    maximum: *maximum,
                }),
                None => localizer.format(UiMessage::DiagnosticResource {
                    resource,
                    actual: *actual,
                }),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticImpact {
    Unchanged,
    ProgressPreserved,
    ResultAppliedPlanNotSaved,
    StateAppliedFinalizationFailed,
    RecoveryRequired,
    OutcomeUnknown,
}

impl DiagnosticImpact {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::ProgressPreserved => "valid_progress_preserved",
            Self::ResultAppliedPlanNotSaved => "result_applied_but_run_plan_not_saved",
            Self::StateAppliedFinalizationFailed => "state_applied_but_finalization_failed",
            Self::RecoveryRequired => "recovery_required",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticAction {
    FixConfiguration,
    FixInput,
    CheckPathAndPermissions,
    CheckProjectState,
    RetryAfterResolvingContention,
    CheckModelService,
    PreserveRecoveryArtifacts,
    Retry,
    ReportBug,
}

impl DiagnosticAction {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FixConfiguration => "fix_configuration",
            Self::FixInput => "fix_input",
            Self::CheckPathAndPermissions => "check_path_and_permissions",
            Self::CheckProjectState => "check_project_state",
            Self::RetryAfterResolvingContention => "retry_after_resolving_contention",
            Self::CheckModelService => "check_model_service",
            Self::PreserveRecoveryArtifacts => "preserve_recovery_artifacts",
            Self::Retry => "retry",
            Self::ReportBug => "report_bug",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RecoveryFact {
    Path { path: String },
    Component { name: String },
    Transaction { state: String },
}

impl RecoveryFact {
    pub(crate) fn path(path: impl AsRef<Path>) -> Self {
        Self::Path {
            path: sanitize_user_text(&path.as_ref().to_string_lossy()),
        }
    }

    pub(crate) fn component(name: impl AsRef<str>) -> Self {
        Self::Component {
            name: sanitize_user_text(name.as_ref()),
        }
    }

    pub(crate) fn transaction(state: impl AsRef<str>) -> Self {
        Self::Transaction {
            state: sanitize_user_text(state.as_ref()),
        }
    }

    fn render_localized(&self, localizer: &UiLocalizer) -> String {
        match self {
            Self::Path { path } => path.clone(),
            Self::Component { name } => localizer.format(UiMessage::DiagnosticRecoveryValue {
                kind: "component",
                value: name,
            }),
            Self::Transaction { state } => localizer.format(UiMessage::DiagnosticRecoveryValue {
                kind: "transaction",
                value: state,
            }),
        }
    }

    fn sanitized(self) -> Self {
        match self {
            Self::Path { path } => Self::Path {
                path: sanitize_user_text(&path),
            },
            Self::Component { name } => Self::component(name),
            Self::Transaction { state } => Self::transaction(state),
        }
    }

    fn map_dynamic_text<F>(self, map: &mut F) -> Self
    where
        F: FnMut(&str) -> String,
    {
        let map_text = |value: String, map: &mut F| sanitize_user_text(&map(&value));
        match self {
            Self::Path { path } => Self::Path {
                path: map_text(path, map),
            },
            Self::Component { name } => Self::Component {
                name: map_text(name, map),
            },
            Self::Transaction { state } => Self::Transaction {
                state: map_text(state, map),
            },
        }
    }
}

/// CLI 与 JSONL 共享的唯一公开诊断事实。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SafeDiagnostic {
    pub(crate) code: DiagnosticCode,
    pub(crate) stage: DiagnosticStage,
    pub(crate) subject: DiagnosticSubject,
    pub(crate) reason: DiagnosticReason,
    pub(crate) impact: DiagnosticImpact,
    pub(crate) action: DiagnosticAction,
    pub(crate) recovery: Vec<RecoveryFact>,
}

impl SafeDiagnostic {
    pub(crate) fn new(
        code: DiagnosticCode,
        stage: DiagnosticStage,
        subject: DiagnosticSubject,
        reason: DiagnosticReason,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> Self {
        Self {
            code,
            stage,
            subject: subject.sanitized(),
            reason: reason.sanitized(),
            impact,
            action,
            recovery: Vec::new(),
        }
    }

    pub(crate) fn io(
        code: DiagnosticCode,
        stage: DiagnosticStage,
        subject: DiagnosticSubject,
        operation: impl AsRef<str>,
        source: &io::Error,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> Self {
        Self::new(
            code,
            stage,
            subject,
            DiagnosticReason::io(operation, source),
            impact,
            action,
        )
    }

    pub(crate) fn with_recovery(mut self, fact: RecoveryFact) -> Self {
        self.recovery.push(fact.sanitized());
        self
    }

    /// 只改写公开诊断中的动态文本，同时保留 code、stage、终态和其他结构化事实。
    ///
    /// 映射结果重新清理控制字符；调用方无需把诊断序列化后再猜测其字段语义。
    pub(crate) fn map_dynamic_text<F>(mut self, mut map: F) -> Self
    where
        F: FnMut(&str) -> String,
    {
        self.subject = self.subject.map_dynamic_text(&mut map);
        self.reason = self.reason.map_dynamic_text(&mut map);
        self.recovery = self
            .recovery
            .into_iter()
            .map(|fact| fact.map_dynamic_text(&mut map))
            .collect();
        self
    }

    /// 重建公开不变量；日志边界也调用它，防止未来反序列化或直接枚举构造绕过清理。
    pub(crate) fn sanitized(mut self) -> Self {
        self.subject = self.subject.sanitized();
        self.reason = self.reason.sanitized();
        self.recovery = self
            .recovery
            .into_iter()
            .map(RecoveryFact::sanitized)
            .collect();
        self
    }
}

fn safe_provider_identifier(value: String) -> Option<String> {
    let value = sanitize_user_text(&value);
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }))
    .then_some(value)
}

/// 内部因果与公开投影的绑定。`source` 不参与序列化或渲染。
pub(crate) struct ReportedFailure {
    public: SafeDiagnostic,
    source: BoxedError,
}

impl ReportedFailure {
    pub(crate) fn new(public: SafeDiagnostic, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            public,
            source: Box::new(source),
        }
    }

    pub(crate) const fn public(&self) -> &SafeDiagnostic {
        &self.public
    }

    pub(crate) fn source_error(&self) -> &(dyn Error + 'static) {
        self.source.as_ref()
    }
}

impl fmt::Debug for ReportedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReportedFailure")
            .field("public", &self.public)
            .field("source", &"<omitted typed source>")
            .finish()
    }
}

impl fmt::Display for ReportedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.public.code.fmt(formatter)
    }
}

impl Error for ReportedFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub(crate) struct FailureReport {
    pub(crate) primary: ReportedFailure,
    pub(crate) related: Vec<ReportedFailure>,
}

impl FailureReport {
    pub(crate) fn new(primary: ReportedFailure) -> Self {
        Self {
            primary,
            related: Vec::new(),
        }
    }

    pub(crate) fn with_related(mut self, related: ReportedFailure) -> Self {
        self.related.push(related);
        self
    }

    pub(crate) fn with_related_report(mut self, related: Self) -> Self {
        self.related.push(related.primary);
        self.related.extend(related.related);
        self
    }

    pub(crate) fn with_primary_recovery(mut self, recovery: RecoveryFact) -> Self {
        self.primary.public.recovery.push(recovery.sanitized());
        self
    }

    pub(crate) fn with_primary_impact(mut self, impact: DiagnosticImpact) -> Self {
        self.primary.public.impact = impact;
        self
    }

    pub(crate) fn public_diagnostics(&self) -> impl Iterator<Item = &SafeDiagnostic> {
        std::iter::once(self.primary.public())
            .chain(self.related.iter().map(ReportedFailure::public))
    }
}

/// 按固定字段顺序呈现一条公开诊断；不会访问 `ReportedFailure::source`。
pub(crate) fn render_safe_diagnostic(
    diagnostic: &SafeDiagnostic,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
) -> io::Result<()> {
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticTitle {
            code: diagnostic.code.as_str(),
        })
    )?;
    writeln!(output, "{}", {
        let stage = localizer.format(UiMessage::DiagnosticStageValue {
            code: diagnostic.stage.as_str(),
        });
        localizer.format(UiMessage::DiagnosticStage { stage: &stage })
    })?;
    let subject = diagnostic.subject.render_localized(localizer);
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticSubject { subject: &subject })
    )?;
    let reason = diagnostic.reason.render_localized(localizer);
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticReason { reason: &reason })
    )?;
    writeln!(output, "{}", {
        let impact = localizer.format(UiMessage::DiagnosticImpactValue {
            code: diagnostic.impact.as_str(),
        });
        localizer.format(UiMessage::DiagnosticImpact { impact: &impact })
    })?;
    writeln!(output, "{}", {
        let action = localizer.format(UiMessage::DiagnosticActionValue {
            code: diagnostic.action.as_str(),
        });
        localizer.format(UiMessage::DiagnosticAction { action: &action })
    })?;
    for recovery in &diagnostic.recovery {
        let recovery = recovery.render_localized(localizer);
        writeln!(
            output,
            "{}",
            localizer.format(UiMessage::DiagnosticRecovery {
                recovery: &recovery,
            })
        )?;
    }
    Ok(())
}

pub(crate) fn render_failure_report(
    report: &FailureReport,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
) -> io::Result<()> {
    render_safe_diagnostic(report.primary.public(), localizer, output)?;
    for (index, related) in report.related.iter().enumerate() {
        writeln!(
            output,
            "{}",
            localizer.format(UiMessage::DiagnosticRelated {
                index: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
            })
        )?;
        render_safe_diagnostic(related.public(), localizer, output)?;
    }
    Ok(())
}

impl fmt::Display for FailureReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.primary.fmt(formatter)
    }
}

impl Error for FailureReport {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_projection_uses_stable_os_fact_without_wrapped_source_text() {
        let wrapped = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "UNSTRUCTURED_SOURCE_SENTINEL",
        );
        let diagnostic = SafeDiagnostic::io(
            DiagnosticCode::ConfigurationRead,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path("settings.toml"),
            "read",
            &wrapped,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains("UNSTRUCTURED_SOURCE_SENTINEL"));
        assert!(serialized.contains("permission_denied"));
    }

    #[test]
    fn process_output_stage_has_a_stable_wire_value() {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::StateFinalizationFailed,
            DiagnosticStage::ProcessOutput,
            DiagnosticSubject::operation("write_stdout"),
            DiagnosticReason::failure(DiagnosticFailureKind::FinalizationFailed),
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::Retry,
        );

        let serialized = serde_json::to_value(diagnostic).expect("诊断应可序列化");
        assert_eq!(serialized["stage"], "process_output");
    }

    #[test]
    fn debug_and_display_use_only_the_stable_public_projection() {
        let reported = ReportedFailure::new(
            SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::Extract,
                DiagnosticSubject::operation("normalize claims"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            ),
            io::Error::other("UNSTRUCTURED_SOURCE_SENTINEL"),
        );
        assert!(!format!("{reported}").contains("UNSTRUCTURED_SOURCE_SENTINEL"));
        assert!(!format!("{reported:?}").contains("UNSTRUCTURED_SOURCE_SENTINEL"));
    }

    #[test]
    fn directly_constructed_public_fields_are_sanitized_at_diagnostic_boundary() {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::Component {
                name: "provider\r\nINJECTED_SUBJECT".to_owned(),
            },
            DiagnosticReason::Http {
                status: Some(429),
                retry_after_seconds: Some(7),
                provider_code: Some("rate_limit\r\nINJECTED_CODE".to_owned()),
                provider_type: Some("rate-limit".to_owned()),
            },
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckModelService,
        )
        .with_recovery(RecoveryFact::Component {
            name: "task\r\nINJECTED_RECOVERY".to_owned(),
        });

        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains("INJECTED_CODE"));
        assert!(!serialized.contains("\\r"));
        assert!(!serialized.contains("\\n"));
        assert!(serialized.contains("provider INJECTED_SUBJECT"));
        assert!(serialized.contains("task INJECTED_RECOVERY"));
        assert!(serialized.contains("rate-limit"));
        assert!(serialized.contains("\"provider_code\":null"));
    }

    #[test]
    fn dynamic_text_mapping_preserves_diagnostic_structure_and_each_text_field() {
        const API_KEY: &str = "actual-api-key";
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::path(format!("C:/projects/{API_KEY}/task.md")),
            DiagnosticReason::Http {
                status: Some(429),
                retry_after_seconds: Some(7),
                provider_code: Some(format!("before-{API_KEY}-after")),
                provider_type: Some(format!("type-{API_KEY}")),
            },
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckModelService,
        )
        .with_recovery(RecoveryFact::path(format!(
            "C:/projects/{API_KEY}/temporary.md"
        )))
        .map_dynamic_text(|value| value.replace(API_KEY, "[REDACTED API KEY]"));

        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains(API_KEY));
        assert_eq!(serialized.matches("[REDACTED API KEY]").count(), 4);
        assert!(serialized.contains("\"code\":\"model.request\""));
        assert!(serialized.contains("\"stage\":\"model_request\""));
        assert!(serialized.contains("\"status\":429"));
        assert!(serialized.contains("\"impact\":\"progress_preserved\""));
    }

    #[test]
    fn renderer_uses_the_stable_projection_and_sanitizes_control_characters() {
        let report = FailureReport::new(ReportedFailure::new(
            SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::WriteBack,
                DiagnosticSubject::path("C:\\game\r\nforged"),
                DiagnosticReason::failure(DiagnosticFailureKind::RequirementFailed),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            io::Error::other("API_KEY=SECRET_SENTINEL"),
        ));
        let mut rendered = Vec::new();
        render_failure_report(
            &report,
            &UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese),
            &mut rendered,
        )
        .expect("诊断应可渲染");
        let rendered = String::from_utf8(rendered).expect("渲染文本应是 UTF-8");

        assert!(!rendered.contains("SECRET_SENTINEL"));
        assert!(!rendered.contains("\r"));
        assert!(rendered.contains("filesystem.operation"));
        assert!(rendered.contains("C:\\game forged"));
    }

    #[test]
    fn simplified_chinese_renderer_localizes_structured_labels_without_english_flattening() {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ShutdownComponent,
            DiagnosticStage::Shutdown,
            DiagnosticSubject::component("SQLite"),
            DiagnosticReason::failure(DiagnosticFailureKind::RollbackFailed),
            DiagnosticImpact::RecoveryRequired,
            DiagnosticAction::PreserveRecoveryArtifacts,
        )
        .with_recovery(RecoveryFact::transaction("rolled_back"));
        let mut rendered = Vec::new();
        render_safe_diagnostic(
            &diagnostic,
            &UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese),
            &mut rendered,
        )
        .expect("诊断应可渲染");
        let rendered = String::from_utf8(rendered).expect("渲染文本应是 UTF-8");
        let plain = rendered.replace(['\u{2068}', '\u{2069}'], "");
        let mut lines = plain.lines();

        // 错误码是跨语言稳定的公开身份，不应为了本地化而翻译或隐藏。
        assert_eq!(lines.next(), Some("错误 [shutdown.component]"));
        let localized_fields = lines.collect::<Vec<_>>().join("\n");

        assert!(localized_fields.contains("位置：组件 SQLite"));
        assert!(localized_fields.contains("阶段：关闭"));
        assert!(localized_fields.contains("主操作失败，并且回滚也失败"));
        assert!(localized_fields.contains("恢复位置：事务 rolled_back"));
        for english_label in [
            "component SQLite",
            "transaction rolled_back",
            "shutdown",
            "recovery is required",
            "preserve recovery",
        ] {
            assert!(
                !localized_fields.contains(english_label),
                "残留英文标签：{english_label}"
            );
        }
    }

    #[test]
    fn simplified_chinese_renderer_explains_typed_protocol_codes_without_debug_syntax() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let http = DiagnosticReason::Http {
            status: Some(429),
            retry_after_seconds: Some(17),
            provider_code: Some("rate_limit".to_owned()),
            provider_type: Some("quota".to_owned()),
        }
        .render_localized(&localizer);
        let sqlite = DiagnosticReason::Sqlite {
            primary_code: 5,
            extended_code: 517,
        }
        .render_localized(&localizer);
        let resource = DiagnosticReason::Resource {
            resource: "请求字符数".to_owned(),
            actual: 25_000,
            maximum: Some(24_000),
        }
        .render_localized(&localizer);
        let http_plain = http.replace(['\u{2068}', '\u{2069}'], "");
        let sqlite_plain = sqlite.replace(['\u{2068}', '\u{2069}'], "");
        let resource_plain = resource.replace(['\u{2068}', '\u{2069}'], "");

        assert_eq!(
            http_plain,
            "HTTP 状态码 429；Retry-After 17 秒；供应商错误码 rate_limit；供应商错误类型 quota"
        );
        assert_eq!(sqlite_plain, "SQLite 主错误码 5，扩展错误码 517");
        assert_eq!(resource_plain, "请求字符数：实际值 25000，上限 24000");
        assert!(!http.contains("Some("));
    }

    #[test]
    fn api_key_configuration_rules_have_specific_wire_and_ui_names() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let cases = [
            (
                ConfigurationValueRule::ApiKeyBlank,
                "api_key_blank",
                "API key 不能为空",
            ),
            (
                ConfigurationValueRule::ApiKeySurroundingWhitespace,
                "api_key_surrounding_whitespace",
                "API key 不能带首尾空白",
            ),
            (
                ConfigurationValueRule::ApiKeyInvalidHeader,
                "api_key_invalid_header",
                "API key 不是有效 HTTP Header 值",
            ),
        ];

        for (rule, wire_name, localized) in cases {
            assert_eq!(rule.as_str(), wire_name);
            let rendered =
                DiagnosticReason::InvalidConfigurationValue { rule }.render_localized(&localizer);
            assert_eq!(rendered.replace(['\u{2068}', '\u{2069}'], ""), localized);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_structured_diagnostic_value_is_localized_without_fallback_or_debug_syntax() {
        let codes = [
            DiagnosticCode::ProcessCurrentDirectory,
            DiagnosticCode::ProcessRuntimeStart,
            DiagnosticCode::ConfigurationPath,
            DiagnosticCode::ConfigurationOpen,
            DiagnosticCode::ConfigurationRead,
            DiagnosticCode::ConfigurationNotFile,
            DiagnosticCode::ConfigurationInvalidUtf8,
            DiagnosticCode::ConfigurationInvalidToml,
            DiagnosticCode::ConfigurationInvalidValue,
            DiagnosticCode::ConfigurationProfileNotFound,
            DiagnosticCode::ConfigurationProfileConflict,
            DiagnosticCode::CommandInput,
            DiagnosticCode::CommandRunPlan,
            DiagnosticCode::ProjectUnavailable,
            DiagnosticCode::ProjectState,
            DiagnosticCode::PromptUnavailable,
            DiagnosticCode::LanguageModuleUnavailable,
            DiagnosticCode::ModelRequest,
            DiagnosticCode::ExtractBuiltin,
            DiagnosticCode::ExtractRules,
            DiagnosticCode::ExtractDocumentRead,
            DiagnosticCode::LuaExecution,
            DiagnosticCode::LuaSnapshotStore,
            DiagnosticCode::WriteBackAssetRead,
            DiagnosticCode::WriteBackPlan,
            DiagnosticCode::WriteBackDocumentRead,
            DiagnosticCode::WriteBackRewrite,
            DiagnosticCode::WriteBackCandidate,
            DiagnosticCode::WriteBackValidate,
            DiagnosticCode::WriteBackPublish,
            DiagnosticCode::WriteBackDiscard,
            DiagnosticCode::RunPlanSaveFailed,
            DiagnosticCode::RunPlanOutcomeUnknown,
            DiagnosticCode::StateFinalizationFailed,
            DiagnosticCode::OperationOutcomeUnknown,
            DiagnosticCode::SignalRegistration,
            DiagnosticCode::ShutdownComponent,
            DiagnosticCode::InternalOperation,
            DiagnosticCode::FileSystemBuild,
            DiagnosticCode::FileSystemOperation,
            DiagnosticCode::SqliteOperation,
            DiagnosticCode::HttpClientBuild,
            DiagnosticCode::LogStart,
            DiagnosticCode::LogSerialize,
            DiagnosticCode::LogWrite,
            DiagnosticCode::LogFlush,
            DiagnosticCode::LogSync,
            DiagnosticCode::LogWorker,
        ];
        let stages = [
            DiagnosticStage::ProcessStartup,
            DiagnosticStage::ProcessOutput,
            DiagnosticStage::Configuration,
            DiagnosticStage::CommandPreparation,
            DiagnosticStage::ProjectOpening,
            DiagnosticStage::Init,
            DiagnosticStage::Extract,
            DiagnosticStage::Translate,
            DiagnosticStage::WriteBack,
            DiagnosticStage::Lua,
            DiagnosticStage::ModelRequest,
            DiagnosticStage::RunPlanFinalization,
            DiagnosticStage::Publication,
            DiagnosticStage::Shutdown,
            DiagnosticStage::Logging,
        ];
        let impacts = [
            DiagnosticImpact::Unchanged,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticImpact::ResultAppliedPlanNotSaved,
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticImpact::RecoveryRequired,
            DiagnosticImpact::OutcomeUnknown,
        ];
        let actions = [
            DiagnosticAction::FixConfiguration,
            DiagnosticAction::FixInput,
            DiagnosticAction::CheckPathAndPermissions,
            DiagnosticAction::CheckProjectState,
            DiagnosticAction::RetryAfterResolvingContention,
            DiagnosticAction::CheckModelService,
            DiagnosticAction::PreserveRecoveryArtifacts,
            DiagnosticAction::Retry,
            DiagnosticAction::ReportBug,
        ];
        let io_kinds = [
            SafeIoKind::NotFound,
            SafeIoKind::PermissionDenied,
            SafeIoKind::ConnectionRefused,
            SafeIoKind::ConnectionReset,
            SafeIoKind::HostUnreachable,
            SafeIoKind::NetworkUnreachable,
            SafeIoKind::ConnectionAborted,
            SafeIoKind::NotConnected,
            SafeIoKind::AddressInUse,
            SafeIoKind::AddressNotAvailable,
            SafeIoKind::NetworkDown,
            SafeIoKind::BrokenPipe,
            SafeIoKind::AlreadyExists,
            SafeIoKind::WouldBlock,
            SafeIoKind::NotADirectory,
            SafeIoKind::IsADirectory,
            SafeIoKind::DirectoryNotEmpty,
            SafeIoKind::ReadOnlyFilesystem,
            SafeIoKind::StaleNetworkFileHandle,
            SafeIoKind::InvalidInput,
            SafeIoKind::InvalidData,
            SafeIoKind::TimedOut,
            SafeIoKind::WriteZero,
            SafeIoKind::StorageFull,
            SafeIoKind::NotSeekable,
            SafeIoKind::QuotaExceeded,
            SafeIoKind::FileTooLarge,
            SafeIoKind::ResourceBusy,
            SafeIoKind::ExecutableFileBusy,
            SafeIoKind::Deadlock,
            SafeIoKind::CrossesDevices,
            SafeIoKind::TooManyLinks,
            SafeIoKind::InvalidFilename,
            SafeIoKind::ArgumentListTooLong,
            SafeIoKind::Interrupted,
            SafeIoKind::Unsupported,
            SafeIoKind::UnexpectedEof,
            SafeIoKind::OutOfMemory,
            SafeIoKind::Other,
        ];
        let failures = [
            DiagnosticFailureKind::MissingRequiredValue,
            DiagnosticFailureKind::ExtractPlanRequired,
            DiagnosticFailureKind::ConflictingValues,
            DiagnosticFailureKind::InvalidSyntax,
            DiagnosticFailureKind::InvalidEncoding,
            DiagnosticFailureKind::InvalidValue,
            DiagnosticFailureKind::NotFound,
            DiagnosticFailureKind::Busy,
            DiagnosticFailureKind::StateMismatch,
            DiagnosticFailureKind::RequirementFailed,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticFailureKind::TransactionOutcomeUnknown,
            DiagnosticFailureKind::FinalizationFailed,
            DiagnosticFailureKind::RollbackFailed,
            DiagnosticFailureKind::ExternalServiceRejected,
            DiagnosticFailureKind::ExternalServiceUnavailable,
            DiagnosticFailureKind::ExecutorClosed,
            DiagnosticFailureKind::ConcurrentShutdown,
            DiagnosticFailureKind::ExecutorStatePoisoned,
            DiagnosticFailureKind::WorkerSpawnFailed,
            DiagnosticFailureKind::WorkerChannelClosed,
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticFailureKind::ReparsePointForbidden,
            DiagnosticFailureKind::NonLocalVolume,
            DiagnosticFailureKind::NonNtfsVolume,
            DiagnosticFailureKind::CaseSensitiveDirectory,
            DiagnosticFailureKind::LockCancelled,
            DiagnosticFailureKind::TargetAlreadyExists,
            DiagnosticFailureKind::FileIdentityChanged,
            DiagnosticFailureKind::InvalidPath,
            DiagnosticFailureKind::WrongPublisherInstance,
            DiagnosticFailureKind::JournalCorrupt,
            DiagnosticFailureKind::UnexpectedArtifact,
            DiagnosticFailureKind::InteractiveSessionAlreadyOpen,
            DiagnosticFailureKind::BackupIncomplete,
            DiagnosticFailureKind::RequestSerializationFailed,
            DiagnosticFailureKind::ResponseParsingFailed,
            DiagnosticFailureKind::InvalidResponseContract,
            DiagnosticFailureKind::TransportFailed,
            DiagnosticFailureKind::LuaDatabaseOpenFailed,
            DiagnosticFailureKind::LuaContextCreationFailed,
            DiagnosticFailureKind::LuaCompilationFailed,
            DiagnosticFailureKind::LuaExecutionFailed,
            DiagnosticFailureKind::LuaHostCallFailed,
            DiagnosticFailureKind::LuaFinalizationFailed,
            DiagnosticFailureKind::LuaUnclosedTransaction,
            DiagnosticFailureKind::LuaSnapshotStoreFailed,
            DiagnosticFailureKind::RulesDefinitionInvalid,
            DiagnosticFailureKind::RulesDocumentReadFailed,
            DiagnosticFailureKind::RulesNoNonBlankMatch,
            DiagnosticFailureKind::RulesInvalidTarget,
            DiagnosticFailureKind::RulesPatternMatchFailed,
            DiagnosticFailureKind::RulesZeroWidthMatch,
            DiagnosticFailureKind::RulesOverlappingCapture,
            DiagnosticFailureKind::RulesMissingTextCapture,
            DiagnosticFailureKind::RulesInvalidCaptureRange,
            DiagnosticFailureKind::RulesDuplicateTarget,
            DiagnosticFailureKind::RulesInvalidMaterialization,
            DiagnosticFailureKind::RulesSnapshotInvalid,
            DiagnosticFailureKind::RulesSnapshotStoreFailed,
            DiagnosticFailureKind::WriteBackExtractionOutOfDate,
            DiagnosticFailureKind::WriteBackAssetSnapshotInvalid,
            DiagnosticFailureKind::SourceDocumentInvalid,
            DiagnosticFailureKind::WriteBackMutationInvalid,
            DiagnosticFailureKind::WriteBackOutputPathInvalid,
            DiagnosticFailureKind::WriteBackOutputPathDuplicate,
            DiagnosticFailureKind::WriteBackCandidateProjectMismatch,
            DiagnosticFailureKind::WriteBackCandidateInvalid,
            DiagnosticFailureKind::WriteBackUnexpectedLuaOutcome,
            DiagnosticFailureKind::WriteBackNotPublished,
            DiagnosticFailureKind::WriteBackPublishedWithResiduals,
            DiagnosticFailureKind::WriteBackRecoveryRequired,
            DiagnosticFailureKind::InternalInvariant,
        ];
        let toml_value_kinds = [
            ConfigurationTomlValueKind::String,
            ConfigurationTomlValueKind::Integer,
            ConfigurationTomlValueKind::Boolean,
            ConfigurationTomlValueKind::StringOrBoolean,
            ConfigurationTomlValueKind::StringArray,
            ConfigurationTomlValueKind::IntegerArray,
            ConfigurationTomlValueKind::StringPairArray,
            ConfigurationTomlValueKind::Table,
            ConfigurationTomlValueKind::TableArray,
        ];
        let toml_failures = [
            ConfigurationTomlFailureKind::Syntax,
            ConfigurationTomlFailureKind::MissingField,
            ConfigurationTomlFailureKind::UnknownField,
            ConfigurationTomlFailureKind::DuplicateField,
            ConfigurationTomlFailureKind::TypeMismatch {
                expected: ConfigurationTomlValueKind::String,
            },
            ConfigurationTomlFailureKind::InvalidValue,
        ];

        for locale in UiLocale::ALL {
            let localizer = UiLocalizer::new(locale);
            for code in codes {
                assert_localized_value(
                    locale,
                    code.as_str(),
                    localizer.format(UiMessage::DiagnosticTitle {
                        code: code.as_str(),
                    }),
                );
            }
            for stage in stages {
                assert_localized_value(
                    locale,
                    stage.as_str(),
                    localizer.format(UiMessage::DiagnosticStageValue {
                        code: stage.as_str(),
                    }),
                );
            }
            for impact in impacts {
                assert_localized_value(
                    locale,
                    impact.as_str(),
                    localizer.format(UiMessage::DiagnosticImpactValue {
                        code: impact.as_str(),
                    }),
                );
            }
            for action in actions {
                assert_localized_value(
                    locale,
                    action.as_str(),
                    localizer.format(UiMessage::DiagnosticActionValue {
                        code: action.as_str(),
                    }),
                );
            }
            for io_kind in io_kinds {
                assert_localized_value(
                    locale,
                    io_kind.as_str(),
                    localizer.format(UiMessage::DiagnosticIoKindValue {
                        code: io_kind.as_str(),
                    }),
                );
            }
            for failure in failures {
                assert_localized_value(
                    locale,
                    failure.as_str(),
                    DiagnosticReason::failure(failure).render_localized(&localizer),
                );
            }
            for rule in all_configuration_value_rules() {
                assert_localized_value(
                    locale,
                    rule.as_str(),
                    DiagnosticReason::InvalidConfigurationValue { rule }
                        .render_localized(&localizer),
                );
            }
            for kind in toml_value_kinds {
                assert_localized_value(locale, kind.as_str(), kind.render_localized(&localizer));
            }
            for failure in toml_failures {
                assert_localized_value(
                    locale,
                    failure.as_str(),
                    failure.render_localized(&localizer),
                );
            }

            let structured_reasons = [
                DiagnosticReason::Io {
                    operation: "read".to_owned(),
                    error_kind: SafeIoKind::NotFound,
                    raw_os_code: Some(2),
                    system_message: Some("system message".to_owned()),
                },
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: 7,
                    error_len: None,
                },
                DiagnosticReason::InvalidToml {
                    line: Some(3),
                    column: Some(5),
                    resource: "configuration".to_owned(),
                    failure: ConfigurationTomlFailureKind::TypeMismatch {
                        expected: ConfigurationTomlValueKind::StringArray,
                    },
                },
                DiagnosticReason::Http {
                    status: Some(429),
                    retry_after_seconds: Some(17),
                    provider_code: Some("rate_limit".to_owned()),
                    provider_type: Some("quota".to_owned()),
                },
                DiagnosticReason::Http {
                    status: None,
                    retry_after_seconds: None,
                    provider_code: None,
                    provider_type: None,
                },
                DiagnosticReason::Sqlite {
                    primary_code: 5,
                    extended_code: 517,
                },
                DiagnosticReason::WindowsStatus {
                    operation: "rename".to_owned(),
                    status: -1_073_741_823,
                },
                DiagnosticReason::Resource {
                    resource: "request bytes".to_owned(),
                    actual: 25_000,
                    maximum: Some(24_000),
                },
            ];
            for (index, reason) in structured_reasons.into_iter().enumerate() {
                assert_localized_value(
                    locale,
                    &format!("structured_reason_{index}"),
                    reason.render_localized(&localizer),
                );
            }
        }
    }

    fn assert_localized_value(locale: UiLocale, code: &str, rendered: String) {
        let rendered = rendered.replace(['\u{2068}', '\u{2069}'], "");
        assert!(
            !rendered.contains("__ATT_FALLBACK__"),
            "{locale} 的 {code} 命中了 fallback：{rendered}"
        );
        for debug_marker in ["Some(", "None", "\\\"", "\"rate_limit\"", "\"quota\""] {
            assert!(
                !rendered.contains(debug_marker),
                "{locale} 的 {code} 泄漏 Debug 语法 {debug_marker}：{rendered}"
            );
        }
    }

    fn all_configuration_value_rules() -> Vec<ConfigurationValueRule> {
        vec![
            ConfigurationValueRule::RuntimeConfigurationInvalid,
            ConfigurationValueRule::UnsupportedPromptLocale,
            ConfigurationValueRule::LanguagePolicyTermBlank,
            ConfigurationValueRule::LanguagePolicyTermSurroundingWhitespace,
            ConfigurationValueRule::LanguagePolicyTermDuplicate,
            ConfigurationValueRule::QuoteRepairCandidatesEmpty,
            ConfigurationValueRule::QuoteRepairDelimiterInvalid,
            ConfigurationValueRule::QuoteRepairPairDuplicate,
            ConfigurationValueRule::QuoteRepairDelimiterAmbiguous,
            ConfigurationValueRule::LanguageIdBlank,
            ConfigurationValueRule::LanguageIdSurroundingWhitespace,
            ConfigurationValueRule::LanguageIdUsesUnderscore,
            ConfigurationValueRule::LanguageIdInvalidSyntax,
            ConfigurationValueRule::LanguageIdInvalidRegistryTag,
            ConfigurationValueRule::LanguageIdCanonicalizationFailed,
            ConfigurationValueRule::LanguageIdUndefinedPrimaryLanguage,
            ConfigurationValueRule::LanguageIdDuplicate,
            ConfigurationValueRule::LanguageCatalogEmpty,
            ConfigurationValueRule::UrlInvalid,
            ConfigurationValueRule::UrlCredentialsForbidden,
            ConfigurationValueRule::UrlFragmentForbidden,
            ConfigurationValueRule::UrlSchemeUnsupported,
            ConfigurationValueRule::ApiKeyBlank,
            ConfigurationValueRule::ApiKeySurroundingWhitespace,
            ConfigurationValueRule::ApiKeyInvalidHeader,
            ConfigurationValueRule::StrictJsonInvalid { line: 3, column: 5 },
            ConfigurationValueRule::JsonObjectRequired,
            ConfigurationValueRule::ReservedRequestField,
            ConfigurationValueRule::ProxyMustBeFalseOrUrl,
            ConfigurationValueRule::PemPathDuplicate,
            ConfigurationValueRule::RuntimeMaximumExceeded {
                actual: 25,
                maximum: 24,
            },
            ConfigurationValueRule::ValueSurroundingWhitespace,
            ConfigurationValueRule::ValueBlank,
            ConfigurationValueRule::PathBlank,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
            ConfigurationValueRule::UsizeRangeExceeded { actual: u64::MAX },
            ConfigurationValueRule::U32RangeExceeded { actual: u64::MAX },
            ConfigurationValueRule::DuplicateProfileId,
            ConfigurationValueRule::SelectedProfileInvalid,
            ConfigurationValueRule::ReferencedClientNotFound,
        ]
    }
}
