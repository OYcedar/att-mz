//! CLI 与项目日志共同消费的安全结构化诊断。
//!
//! 本模块刻意不提供从 `Display`、`Debug` 或错误来源链生成公开文本的入口。具体失败
//! 必须在仍持有类型化事实的边界选择公开代码、阶段、对象、原因、影响和恢复办法；原始
//! 错误只保留给进程内部的因果关系。

use std::error::Error;
use std::io;

use serde::{Deserialize, Serialize};

use crate::i18n::{UiLocale, UiLocalizer, UiMessage};

mod generic_issue;
mod issue;
mod lua_issue;
mod model;
mod observability_issue;
mod publication_issue;
mod rpg_maker_issue;
mod runtime_issue;
mod safe_value;

pub(crate) use generic_issue::{
    GenericDiagnosticStage, GenericIssue, GenericJsonErrorCategory, GenericJsonlLocation,
    GenericLanguageProjectionProblem, GenericLanguageViolation, GenericPlaceholderMultisetProblem,
    GenericProblem, GenericProjectDatabaseProblem, GenericProjectTranslationProblem,
    GenericResourceKind, GenericResponseDestinationProblem, GenericResponseTextProblem,
    GenericResponseValueProblem, GenericTaskResponseJsonCategory, GenericTaskResponseProblem,
    GenericTaskUnavailableReason, GenericTextViolation, GenericTranslationPreparationProblem,
    GenericWriteBackSnapshotProblem, GenericWriteBackTextSide, GenericWriteBackUnitProblem,
};
pub(crate) use issue::{
    ConfigurationIssue, DiagnosticIssue, GenericUnitLocator, IoFailure, Pcre2Failure,
    Pcre2FailureKind, PlaceholderCompilationProblem, PlaceholderIssue,
    PlaceholderMatchRangeViolation, PlaceholderRuleOrigin, PlaceholderRuleSource,
    PlaceholderWorkerOperation, PromptProblem, PromptTemplateViolation, TerminologyField,
    TranslationIssue, TranslationJsonFailureKind, TranslationPlanningResourceKind,
    TranslationPlanningResourceOrigin, TranslationPlanningResourceProblem,
    TranslationPlanningWorkerOperation, TranslationTaskPlanningProblem,
};
pub(crate) use lua_issue::{
    LuaCompilationProblem, LuaCompilerCategory, LuaContextProblem, LuaEngine, LuaIssue,
    LuaOperation, LuaProblem, LuaScriptProblem, LuaValueViolation,
};
pub(crate) use model::{
    Diagnostic, DiagnosticReport, DiagnosticResolution, RelatedFailureRelation, ReportedFailure,
    StateEffect, render_diagnostic_fields, render_diagnostic_report, render_state_effect_impact,
};
pub(crate) use observability_issue::{
    ObservabilityComponent, ObservabilityContractViolation, ObservabilityEventCode,
    ObservabilityFailureCount, ObservabilityIssue, ObservabilityPathFailure,
    ObservabilityProjectLogPhase, ObservabilityRenderTarget, ObservabilityWriteFailure,
};
pub(crate) use publication_issue::{
    PublicationBackendCause, PublicationCandidateBindingProblem,
    PublicationCandidateInspectionProblem, PublicationIssue, PublicationProblem,
    PublicationRequestViolation, PublicationStep,
};
pub(crate) use rpg_maker_issue::{
    RpgMakerBackendCause, RpgMakerBuiltinDocumentProblem, RpgMakerClaimSummaryMismatchDetails,
    RpgMakerClaimSummaryMismatchKind, RpgMakerComputeFailure, RpgMakerDiagnosticGroupKind,
    RpgMakerDiagnosticLocation, RpgMakerDiagnosticLocationStep, RpgMakerDiagnosticOwner,
    RpgMakerDiagnosticRole, RpgMakerDiagnosticScope, RpgMakerDiagnosticSource,
    RpgMakerDiagnosticStage, RpgMakerDialogueDefinitionOrigin, RpgMakerDialogueDefinitionProblem,
    RpgMakerDialogueProjectionProblem, RpgMakerDocumentConsumer, RpgMakerDocumentOperation,
    RpgMakerDocumentProblem, RpgMakerEngineKind, RpgMakerExtractionClaimSummaryViolation,
    RpgMakerExtractionComputeOperation, RpgMakerExtractionConflictRowViolation,
    RpgMakerExtractionIndexDecisionViolation, RpgMakerExtractionMutationConflict,
    RpgMakerExtractionProblem, RpgMakerExtractionSemanticOrderKey,
    RpgMakerExtractionSemanticOrderProjectionViolation,
    RpgMakerExtractionSnapshotEncodingViolation, RpgMakerExtractionSnapshotViolation,
    RpgMakerExtractionSource, RpgMakerExtractionStoreOperation, RpgMakerExtractionStoreProblem,
    RpgMakerExtractionStoredDefinitionViolation, RpgMakerInitialSetting, RpgMakerIssue,
    RpgMakerJsonFailureKind, RpgMakerJsonValueKind, RpgMakerLanguageIdViolation,
    RpgMakerLanguageModuleKind, RpgMakerLocationCodecFailure, RpgMakerLogicalUnitLocator,
    RpgMakerManualLayoutRegion, RpgMakerModelFinishReason, RpgMakerModelNonStopFinishReason,
    RpgMakerMutationAccess, RpgMakerOutputContractViolation, RpgMakerPlaceholderMultisetViolation,
    RpgMakerPlaceholderProjectionProblem, RpgMakerPluginsEnvelopeFailure,
    RpgMakerProjectDefinitionStage, RpgMakerProjectDefinitionViolation,
    RpgMakerProjectMetadataViolation, RpgMakerProjectProblem, RpgMakerProjectionCodecFailure,
    RpgMakerProjectionFailureKind, RpgMakerProjectionModelViolation,
    RpgMakerResponseInvariantProblem, RpgMakerResponseLanguageProjectionProblem,
    RpgMakerResponseProcessingProblem, RpgMakerResponseProcessingScope,
    RpgMakerResultStorePlanViolation, RpgMakerResultStoreProblem,
    RpgMakerRulesCommandNonStringFact, RpgMakerRulesCommandNonStringType,
    RpgMakerRulesDefinitionOrigin, RpgMakerRulesDefinitionProblem, RpgMakerRulesDiagnosticSource,
    RpgMakerRulesInvalidTarget, RpgMakerRulesMatchContext, RpgMakerRulesMatchProblem,
    RpgMakerRulesMatchSource, RpgMakerRulesMaterializationFailure, RpgMakerRulesPathFailure,
    RpgMakerRulesSourceKind, RpgMakerRulesValueStep, RpgMakerRunPlanExpectedSqliteType,
    RpgMakerRunPlanSnapshotViolation, RpgMakerRunPlanSqliteType, RpgMakerRunPlanValueViolation,
    RpgMakerSemanticOrderKeyViolation, RpgMakerSemanticOrderLevel, RpgMakerStorageCodecOperation,
    RpgMakerTaskResponseJsonCategory, RpgMakerTaskResponseProblem, RpgMakerTaskResponseUnitProblem,
    RpgMakerTaskResponseValueProblem, RpgMakerTomlFailureKind,
    RpgMakerTranslationAssetComputeOperation, RpgMakerTranslationAssetProblem,
    RpgMakerTranslationPlanningProblem, RpgMakerTranslationResourceKind,
    RpgMakerTranslationSnapshotViolation, RpgMakerUnitLocator,
    RpgMakerWriteBackAssetComputeOperation, RpgMakerWriteBackAssetProblem,
    RpgMakerWriteBackAssetSnapshotViolation, RpgMakerWriteBackChoicesPlanViolation,
    RpgMakerWriteBackDialoguePlanViolation, RpgMakerWriteBackDocumentRewriteProblem,
    RpgMakerWriteBackModelViolation, RpgMakerWriteBackMutationPlanViolation,
    RpgMakerWriteBackMutationViolation, RpgMakerWriteBackPlanningProblem,
};
pub(crate) use runtime_issue::{
    FileSystemDiagnosticContext, FileSystemDiagnosticStage, FileSystemIssue,
    FileSystemJournalViolation, FileSystemOperation, FileSystemOrdinalKeyPhase,
    FileSystemPathViolation, FileSystemProblem, FileSystemRecoveryViolation, HttpEndpoint,
    HttpEnvelopeViolation, HttpIssue, HttpJsonCategory, HttpResponseReadFailure, HttpScheme,
    HttpTransportKind, HttpTransportPhase, RuntimeBoundaryOperation, RuntimeCommand,
    RuntimeComponent, RuntimeEngine, RuntimeIssue, RuntimeOperation, RuntimePanicBoundary,
    SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure, SqliteDriverKind,
    SqliteIssue, SqliteOperation, SqliteProblem, SqliteTransactionState,
    TranslationTaskCounterInvariant,
};
pub(crate) use safe_value::{
    ByteRange, InvalidSafeIdentifier, SafeIdentifier, SafePath, SafeText, public_path,
};

pub(crate) type BoxedError = Box<dyn Error + Send + Sync + 'static>;

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
    Runtime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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

/// 配置值失败的稳定闭集。这里只保存校验规则和安全的数值事实，绝不保存配置正文。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum ConfigurationValueRule {
    LanguagePolicyTermBlank,
    LanguagePolicyTermSurroundingWhitespace,
    LanguagePolicyTermDuplicate,
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
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::LanguagePolicyTermBlank => "language_policy_term_blank",
            Self::LanguagePolicyTermSurroundingWhitespace => {
                "language_policy_term_surrounding_whitespace"
            }
            Self::LanguagePolicyTermDuplicate => "language_policy_term_duplicate",
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

    /// 将规则本身携带的数值事实提供给结构化诊断呈现层。
    pub(crate) fn numeric_facts(&self) -> Vec<(&'static str, u64)> {
        match self {
            Self::StrictJsonInvalid { line, column } => {
                vec![("line", *line), ("column", *column)]
            }
            Self::RuntimeMaximumExceeded { actual, maximum } => {
                vec![("actual", *actual), ("maximum", *maximum)]
            }
            Self::PositiveRequired { actual }
            | Self::UsizeRangeExceeded { actual }
            | Self::U32RangeExceeded { actual } => vec![("actual", *actual)],
            _ => Vec::new(),
        }
    }

    pub(crate) fn render(&self) -> String {
        self.render_localized(&UiLocalizer::new(UiLocale::English))
    }

    pub(crate) fn render_localized(&self, localizer: &UiLocalizer) -> String {
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
    Table,
    TableArray,
}

impl ConfigurationTomlValueKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::StringOrBoolean => "string_or_boolean",
            Self::StringArray => "string_array",
            Self::IntegerArray => "integer_array",
            Self::Table => "table",
            Self::TableArray => "table_array",
        }
    }
}

/// TOML 解析与字段契约失败的稳定闭集。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
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
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::MissingField => "missing_field",
            Self::UnknownField => "unknown_field",
            Self::DuplicateField => "duplicate_field",
            Self::TypeMismatch { .. } => "type_mismatch",
            Self::InvalidValue => "invalid_value",
        }
    }
}

#[cfg(test)]
mod wire_contract_tests {
    use super::{ConfigurationTomlFailureKind, ConfigurationValueRule};

    #[test]
    fn configuration_wire_rejects_unknown_nested_fields() {
        assert!(
            serde_json::from_value::<ConfigurationValueRule>(serde_json::json!({
                "strict_json_invalid": {
                    "line": 1,
                    "column": 2,
                    "unexpected": true
                }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ConfigurationTomlFailureKind>(serde_json::json!({
                "kind": "type_mismatch",
                "expected": "string",
                "unexpected": true
            }))
            .is_err()
        );
    }
}
