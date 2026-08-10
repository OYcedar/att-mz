//! 各语义所有者建立的封闭诊断问题。

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use super::model::DiagnosticResolution;
use super::safe_value::{ByteRange, SafeIdentifier, SafePath, SafeText};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IoFailure {
    pub(crate) kind: super::SafeIoKind,
    pub(crate) raw_os_code: Option<i32>,
    pub(crate) system_message: Option<SafeText>,
}

impl IoFailure {
    pub(crate) fn from_error(source: &std::io::Error) -> Self {
        let raw_os_code = source.raw_os_error();
        Self {
            kind: source.kind().into(),
            raw_os_code,
            system_message: raw_os_code
                .map(|code| SafeText::new(std::io::Error::from_raw_os_error(code).to_string())),
        }
    }

    pub(crate) fn from_parts(kind: super::SafeIoKind, raw_os_code: Option<i32>) -> Self {
        Self {
            kind,
            raw_os_code,
            system_message: raw_os_code
                .map(|code| SafeText::new(std::io::Error::from_raw_os_error(code).to_string())),
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self.kind {
            super::SafeIoKind::NotFound => "not_found",
            super::SafeIoKind::AlreadyExists => "already_exists",
            _ => "operation_failed",
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![
            ("io_kind", self.kind.as_str().to_owned()),
            (
                "raw_os_code",
                self.raw_os_code
                    .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            ),
        ];
        if let Some(message) = &self.system_message {
            facts.push(("system_message", message.to_string()));
        }
        facts
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ConfigurationIssue {
    Open {
        path: SafePath,
        failure: IoFailure,
    },
    NotFile {
        path: SafePath,
    },
    Read {
        path: SafePath,
        failure: IoFailure,
    },
    InvalidUtf8 {
        path: SafePath,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidToml {
        path: SafePath,
        line: Option<usize>,
        column: Option<usize>,
        resource: SafeText,
        failure: super::ConfigurationTomlFailureKind,
    },
    InvalidValue {
        path: Option<SafePath>,
        field: SafeIdentifier,
        rule: super::ConfigurationValueRule,
    },
    TranslationProfileNotFound {
        path: SafePath,
        profile_id: SafeIdentifier,
    },
    ProfileSelectionConflict {
        path: SafePath,
        explicit_profile: SafeIdentifier,
        requested_profile: SafeIdentifier,
    },
}

impl ConfigurationIssue {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Open { .. } => "configuration.open",
            Self::NotFile { .. } => "configuration.not_file",
            Self::Read { .. } => "configuration.read",
            Self::InvalidUtf8 { .. } => "configuration.invalid_utf8",
            Self::InvalidToml { .. } => "configuration.invalid_toml",
            Self::InvalidValue { .. } => "configuration.invalid_value",
            Self::TranslationProfileNotFound { .. } => "configuration.profile_not_found",
            Self::ProfileSelectionConflict { .. } => "configuration.profile_conflict",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Open { .. } | Self::Read { .. } => DiagnosticResolution::CheckPathAndPermissions,
            Self::NotFile { .. }
            | Self::InvalidUtf8 { .. }
            | Self::InvalidToml { .. }
            | Self::InvalidValue { .. }
            | Self::TranslationProfileNotFound { .. }
            | Self::ProfileSelectionConflict { .. } => DiagnosticResolution::FixConfiguration,
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::Open { failure, .. } | Self::Read { failure, .. } => failure.summary_code(),
            Self::NotFile { .. } => "invalid_path",
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::InvalidToml { .. } => "invalid_syntax",
            Self::InvalidValue { .. } => "invalid_value",
            Self::TranslationProfileNotFound { .. } => "not_found",
            Self::ProfileSelectionConflict { .. } => "conflicting_values",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::Open { path, .. }
            | Self::NotFile { path }
            | Self::Read { path, .. }
            | Self::InvalidUtf8 { path, .. }
            | Self::InvalidToml { path, .. }
            | Self::TranslationProfileNotFound { path, .. }
            | Self::ProfileSelectionConflict { path, .. } => path.to_string(),
            Self::InvalidValue { field, .. } => field.to_string(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Open { path, failure } | Self::Read { path, failure } => {
                let mut facts = vec![("path", path.to_string())];
                facts.extend(failure.facts());
                facts
            }
            Self::NotFile { path } => vec![
                ("path", path.to_string()),
                ("expected", "file".to_owned()),
                ("actual", "not_file".to_owned()),
            ],
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => vec![
                ("path", path.to_string()),
                ("valid_up_to", valid_up_to.to_string()),
                ("error_len", optional_number(*error_len)),
            ],
            Self::InvalidToml {
                path,
                line,
                column,
                resource,
                failure,
            } => {
                let mut facts = vec![
                    ("path", path.to_string()),
                    ("line", optional_number(*line)),
                    ("column", optional_number(*column)),
                    ("resource", resource.to_string()),
                    ("toml_failure", failure.as_str().to_owned()),
                ];
                if let super::ConfigurationTomlFailureKind::TypeMismatch { expected } = failure {
                    facts.push(("expected", expected.as_str().to_owned()));
                }
                facts
            }
            Self::InvalidValue { path, field, rule } => {
                let mut facts = vec![
                    ("field", field.to_string()),
                    ("rule", rule.as_str().to_owned()),
                ];
                if let Some(path) = path {
                    facts.push(("path", path.to_string()));
                }
                facts.extend(
                    rule.numeric_facts()
                        .into_iter()
                        .map(|(name, value)| (name, value.to_string())),
                );
                facts
            }
            Self::TranslationProfileNotFound { path, profile_id } => vec![
                ("path", path.to_string()),
                ("profile_id", profile_id.to_string()),
            ],
            Self::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => vec![
                ("path", path.to_string()),
                ("explicit_profile", explicit_profile.to_string()),
                ("requested_profile", requested_profile.to_string()),
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericUnitLocator {
    pub(crate) relative_path: SafePath,
    pub(crate) group_id: Option<SafeIdentifier>,
    pub(crate) unit_id: Option<SafeIdentifier>,
    pub(crate) role: Option<SafeIdentifier>,
    pub(crate) line: Option<NonZeroUsize>,
    pub(crate) unit: Option<NonZeroUsize>,
}

impl GenericUnitLocator {
    pub(crate) fn new(
        relative_path: impl AsRef<std::path::Path>,
        group_id: impl AsRef<str>,
        unit_id: impl AsRef<str>,
        role: Option<&str>,
    ) -> Self {
        Self {
            relative_path: SafePath::new(relative_path),
            group_id: SafeIdentifier::new(group_id).ok(),
            unit_id: SafeIdentifier::new(unit_id).ok(),
            role: role.and_then(|value| SafeIdentifier::new(value).ok()),
            line: None,
            unit: None,
        }
    }

    pub(crate) fn with_natural_position(mut self, line: usize, unit: usize) -> Self {
        self.line = NonZeroUsize::new(line);
        self.unit = NonZeroUsize::new(unit);
        self
    }

    pub(crate) fn readable_id(&self) -> String {
        let path = self.relative_path.to_string().replace('\\', "/");
        match (self.line, self.unit) {
            (Some(line), Some(unit)) => format!("{path}:line{line}:unit{unit}:text"),
            _ => path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PlaceholderRuleSource {
    ExternalFile { path: SafePath },
    ProjectSnapshot,
}

impl PlaceholderRuleSource {
    pub(crate) fn external_file(path: impl AsRef<std::path::Path>) -> Self {
        Self::ExternalFile {
            path: SafePath::new(path),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlaceholderRuleOrigin {
    Builtin,
    Custom,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlaceholderWorkerOperation {
    CompileCustomRules,
    MatchText,
}

impl PlaceholderWorkerOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CompileCustomRules => "compile_custom_rules",
            Self::MatchText => "match_text",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Pcre2FailureKind {
    Compile,
    Jit,
    Match,
    Info,
    Option,
    Unrecognized,
}

impl Pcre2FailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Jit => "jit",
            Self::Match => "match",
            Self::Info => "info",
            Self::Option => "option",
            Self::Unrecognized => "unrecognized",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pcre2Failure {
    pub(crate) kind: Pcre2FailureKind,
    pub(crate) code: i32,
    pub(crate) offset: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlaceholderMatchRangeViolation {
    WholeStartAfterEnd,
    WholeEndBeyondText,
    WholeStartNotUtf8Boundary,
    WholeEndNotUtf8Boundary,
    CaptureStartAfterEnd,
    CaptureEndBeyondText,
    CaptureStartNotUtf8Boundary,
    CaptureEndNotUtf8Boundary,
    CaptureStartsBeforeWhole,
    CaptureEndsAfterWhole,
}

impl PlaceholderMatchRangeViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::WholeStartAfterEnd => "whole_start_after_end",
            Self::WholeEndBeyondText => "whole_end_beyond_text",
            Self::WholeStartNotUtf8Boundary => "whole_start_not_utf8_boundary",
            Self::WholeEndNotUtf8Boundary => "whole_end_not_utf8_boundary",
            Self::CaptureStartAfterEnd => "capture_start_after_end",
            Self::CaptureEndBeyondText => "capture_end_beyond_text",
            Self::CaptureStartNotUtf8Boundary => "capture_start_not_utf8_boundary",
            Self::CaptureEndNotUtf8Boundary => "capture_end_not_utf8_boundary",
            Self::CaptureStartsBeforeWhole => "capture_starts_before_whole",
            Self::CaptureEndsAfterWhole => "capture_ends_after_whole",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PlaceholderIssue {
    WorkerStart {
        operation: PlaceholderWorkerOperation,
        io_kind: super::SafeIoKind,
        raw_os_code: Option<i32>,
    },
    PatternMatch {
        rule_origin: Option<PlaceholderRuleOrigin>,
        rule_number: Option<usize>,
        pcre2: Pcre2Failure,
    },
    EmptyMatch {
        rule_origin: PlaceholderRuleOrigin,
        rule_number: Option<usize>,
        match_range: ByteRange,
    },
    MissingTextCapture {
        rule_number: usize,
        match_range: ByteRange,
    },
    InvalidMatchRange {
        rule_number: usize,
        whole_match_start_byte: usize,
        whole_match_end_byte: usize,
        capture_start_byte: Option<usize>,
        capture_end_byte: Option<usize>,
        violation: PlaceholderMatchRangeViolation,
    },
    OverlappingMatches {
        first_origin: PlaceholderRuleOrigin,
        first_rule_number: Option<usize>,
        first_range: ByteRange,
        second_origin: PlaceholderRuleOrigin,
        second_rule_number: Option<usize>,
        second_range: ByteRange,
    },
    CrossesLineBoundary {
        rule_origin: PlaceholderRuleOrigin,
        rule_number: Option<usize>,
        source_line_index: usize,
    },
    ReservedTokenNamespace {
        range: ByteRange,
    },
}

impl PlaceholderIssue {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::WorkerStart { .. } => "translation.placeholder.worker_start",
            Self::PatternMatch { .. } => "translation.placeholder.pattern_match",
            Self::EmptyMatch { .. } => "translation.placeholder.empty_match",
            Self::MissingTextCapture { .. } => "translation.placeholder.missing_text_capture",
            Self::InvalidMatchRange { .. } => "translation.placeholder.invalid_match_range",
            Self::OverlappingMatches { .. } => "translation.placeholder.overlapping_matches",
            Self::CrossesLineBoundary { .. } => "translation.placeholder.crosses_line_boundary",
            Self::ReservedTokenNamespace { .. } => {
                "translation.placeholder.reserved_token_namespace"
            }
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::WorkerStart { .. } => "worker_spawn_failed",
            Self::PatternMatch { .. } => "rules_pattern_match_failed",
            Self::EmptyMatch { .. } => "rules_zero_width_match",
            Self::MissingTextCapture { .. } => "rules_missing_text_capture",
            Self::InvalidMatchRange { .. } => "rules_invalid_capture_range",
            Self::OverlappingMatches { .. } => "rules_overlapping_capture",
            Self::CrossesLineBoundary { .. } => "invalid_value",
            Self::ReservedTokenNamespace { .. } => "invalid_value",
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::WorkerStart {
                operation,
                io_kind,
                raw_os_code,
            } => vec![
                ("operation", operation.as_str().to_owned()),
                ("io_kind", io_kind.as_str().to_owned()),
                (
                    "raw_os_code",
                    raw_os_code.map_or_else(|| "none".to_owned(), |value| value.to_string()),
                ),
            ],
            Self::PatternMatch {
                rule_origin,
                rule_number,
                pcre2,
            } => vec![
                ("rule_origin", optional_origin(*rule_origin)),
                ("rule_number", optional_number(*rule_number)),
                ("pcre2_kind", pcre2.kind.as_str().to_owned()),
                ("pcre2_code", pcre2.code.to_string()),
                ("pcre2_offset", optional_number(pcre2.offset)),
            ],
            Self::EmptyMatch {
                rule_origin,
                rule_number,
                match_range,
            } => vec![
                ("rule_origin", origin(*rule_origin).to_owned()),
                ("rule_number", optional_number(*rule_number)),
                ("match_range", match_range.to_string()),
            ],
            Self::MissingTextCapture {
                rule_number,
                match_range,
            } => vec![
                ("rule_number", rule_number.to_string()),
                ("match_range", match_range.to_string()),
            ],
            Self::InvalidMatchRange {
                rule_number,
                whole_match_start_byte,
                whole_match_end_byte,
                capture_start_byte,
                capture_end_byte,
                violation,
            } => vec![
                ("rule_number", rule_number.to_string()),
                ("whole_match_start_byte", whole_match_start_byte.to_string()),
                ("whole_match_end_byte", whole_match_end_byte.to_string()),
                ("capture_start_byte", optional_number(*capture_start_byte)),
                ("capture_end_byte", optional_number(*capture_end_byte)),
                ("violation", violation.as_str().to_owned()),
            ],
            Self::OverlappingMatches {
                first_origin,
                first_rule_number,
                first_range,
                second_origin,
                second_rule_number,
                second_range,
            } => vec![
                ("first_origin", origin(*first_origin).to_owned()),
                ("first_rule_number", optional_number(*first_rule_number)),
                ("first_range", first_range.to_string()),
                ("second_origin", origin(*second_origin).to_owned()),
                ("second_rule_number", optional_number(*second_rule_number)),
                ("second_range", second_range.to_string()),
            ],
            Self::CrossesLineBoundary {
                rule_origin,
                rule_number,
                source_line_index,
            } => vec![
                ("rule_origin", origin(*rule_origin).to_owned()),
                ("rule_number", optional_number(*rule_number)),
                ("source_line_index", source_line_index.to_string()),
            ],
            Self::ReservedTokenNamespace { range } => {
                vec![("match_range", range.to_string())]
            }
        }
    }
}

fn origin(value: PlaceholderRuleOrigin) -> &'static str {
    match value {
        PlaceholderRuleOrigin::Builtin => "builtin",
        PlaceholderRuleOrigin::Custom => "custom",
    }
}

fn optional_origin(value: Option<PlaceholderRuleOrigin>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| origin(value).to_owned())
}

fn optional_number(value: Option<usize>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromptTemplateViolation {
    InvalidSyntax,
    UnknownVariable,
    MissingSourceLanguage,
    MissingTargetLanguage,
    VariablesNotAllowed,
}

impl PromptTemplateViolation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSyntax => "invalid_syntax",
            Self::UnknownVariable => "unknown_variable",
            Self::MissingSourceLanguage => "missing_source_language",
            Self::MissingTargetLanguage => "missing_target_language",
            Self::VariablesNotAllowed => "variables_not_allowed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PromptProblem {
    NotFound,
    NotFile,
    ResolvedFileNameMismatch {
        resolved_path: SafePath,
    },
    InvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    Empty,
    InvalidTemplate {
        violation: PromptTemplateViolation,
    },
}

impl PromptProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::NotFound | Self::NotFile => "translation.prompt.unavailable",
            Self::ResolvedFileNameMismatch { .. } => "translation.prompt.identity_changed",
            Self::InvalidUtf8 { .. } => "translation.prompt.invalid_utf8",
            Self::Empty => "translation.prompt.empty",
            Self::InvalidTemplate { .. } => "translation.prompt.invalid_template",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::ResolvedFileNameMismatch { .. } => DiagnosticResolution::CheckPathAndPermissions,
            Self::NotFound
            | Self::NotFile
            | Self::InvalidUtf8 { .. }
            | Self::Empty
            | Self::InvalidTemplate { .. } => DiagnosticResolution::FixConfiguration,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotFile => "invalid_path",
            Self::ResolvedFileNameMismatch { .. } => "file_identity_changed",
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::Empty => "missing_required_value",
            Self::InvalidTemplate { .. } => "invalid_value",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::NotFound => Vec::new(),
            Self::NotFile => vec![
                ("expected", "file".to_owned()),
                ("actual", "not_file".to_owned()),
            ],
            Self::ResolvedFileNameMismatch { resolved_path } => {
                vec![("resolved_path", resolved_path.to_string())]
            }
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => vec![
                ("valid_up_to", valid_up_to.to_string()),
                ("error_len", optional_number(*error_len)),
            ],
            Self::Empty => vec![("content", "blank".to_owned())],
            Self::InvalidTemplate { violation } => {
                vec![("violation", violation.as_str().to_owned())]
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranslationPlanningResourceKind {
    Terminology,
    PlaceholderRules,
}

impl TranslationPlanningResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Terminology => "terminology",
            Self::PlaceholderRules => "placeholder_rules",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranslationPlanningResourceOrigin {
    ExternalFile { path: SafePath },
    ProjectSnapshot,
}

impl TranslationPlanningResourceOrigin {
    pub(crate) fn external(path: impl AsRef<std::path::Path>) -> Self {
        Self::ExternalFile {
            path: SafePath::new(path),
        }
    }

    fn fact_value(&self) -> String {
        match self {
            Self::ExternalFile { path } => path.to_string(),
            Self::ProjectSnapshot => "project_snapshot".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranslationPlanningWorkerOperation {
    Utf8Validation,
    ParseTerminology,
    ParsePlaceholderRules,
    CompileTerminologyMatcher,
    Unknown,
}

impl TranslationPlanningWorkerOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8Validation => "utf8_validation",
            Self::ParseTerminology => "parse_terminology",
            Self::ParsePlaceholderRules => "parse_placeholder_rules",
            Self::CompileTerminologyMatcher => "compile_terminology_matcher",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminologyField {
    Term,
    Translation,
    Trigger,
    Unknown,
}

impl TerminologyField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Translation => "translation",
            Self::Trigger => "trigger",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranslationJsonFailureKind {
    Io,
    Syntax,
    Data,
    Eof,
}

impl TranslationJsonFailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "eof",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranslationPlanningResourceProblem {
    Cancelled,
    WorkerStart {
        operation: TranslationPlanningWorkerOperation,
        failure: IoFailure,
    },
    InvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidToml {
        span_start: Option<usize>,
        span_end: Option<usize>,
    },
    InvalidSnapshotJson {
        category: TranslationJsonFailureKind,
        line: usize,
        column: usize,
    },
    SnapshotEncodingJson {
        category: TranslationJsonFailureKind,
        line: usize,
        column: usize,
    },
    BlankField {
        entry_number: usize,
        field: TerminologyField,
    },
    SurroundingWhitespace {
        entry_number: usize,
        field: TerminologyField,
    },
    ControlCharacter {
        entry_number: usize,
        field: TerminologyField,
        code_point: u32,
    },
    EmptyTriggers {
        entry_number: usize,
    },
    DuplicateTerm,
    DuplicateTrigger,
    MatcherConstruction,
}

impl TranslationPlanningResourceProblem {
    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::WorkerStart { .. } => "worker_start",
            Self::InvalidUtf8 { .. } => "invalid_utf8",
            Self::InvalidToml { .. } => "invalid_toml",
            Self::InvalidSnapshotJson { .. } => "invalid_snapshot_json",
            Self::SnapshotEncodingJson { .. } => "snapshot_encoding_json",
            Self::BlankField { .. } => "blank_field",
            Self::SurroundingWhitespace { .. } => "surrounding_whitespace",
            Self::ControlCharacter { .. } => "control_character",
            Self::EmptyTriggers { .. } => "empty_triggers",
            Self::DuplicateTerm => "duplicate_term",
            Self::DuplicateTrigger => "duplicate_trigger",
            Self::MatcherConstruction => "matcher_construction",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Cancelled
            | Self::DuplicateTerm
            | Self::DuplicateTrigger
            | Self::MatcherConstruction => Vec::new(),
            Self::WorkerStart { operation, failure } => {
                let mut facts = vec![("operation", operation.as_str().to_owned())];
                facts.extend(failure.facts());
                facts
            }
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => vec![
                ("valid_up_to", valid_up_to.to_string()),
                ("error_len", optional_number(*error_len)),
            ],
            Self::InvalidToml {
                span_start,
                span_end,
            } => vec![
                ("span_start", optional_number(*span_start)),
                ("span_end", optional_number(*span_end)),
            ],
            Self::InvalidSnapshotJson {
                category,
                line,
                column,
            }
            | Self::SnapshotEncodingJson {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("line", line.to_string()),
                ("column", column.to_string()),
            ],
            Self::BlankField {
                entry_number,
                field,
            }
            | Self::SurroundingWhitespace {
                entry_number,
                field,
            } => vec![
                ("entry_number", entry_number.to_string()),
                ("field", field.as_str().to_owned()),
            ],
            Self::ControlCharacter {
                entry_number,
                field,
                code_point,
            } => vec![
                ("entry_number", entry_number.to_string()),
                ("field", field.as_str().to_owned()),
                ("code_point", format!("U+{code_point:04X}")),
            ],
            Self::EmptyTriggers { entry_number } => {
                vec![("entry_number", entry_number.to_string())]
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PlaceholderCompilationProblem {
    WorkerStart {
        operation: PlaceholderWorkerOperation,
        failure: IoFailure,
    },
    EmptyScopes {
        rule_number: usize,
    },
    UnknownScope {
        rule_number: usize,
    },
    DuplicateScope {
        rule_number: usize,
    },
    EmptyIds {
        rule_number: usize,
    },
    InvalidId {
        rule_number: usize,
    },
    UnknownId {
        rule_number: usize,
    },
    DuplicateId {
        rule_number: usize,
    },
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        pcre2: Pcre2Failure,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        actual_count: usize,
    },
    ReorderedWrapper {
        rule_number: usize,
    },
}

impl PlaceholderCompilationProblem {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::WorkerStart { .. } => "translation.placeholder.compilation.worker_start",
            Self::EmptyScopes { .. } => "translation.placeholder.compilation.empty_scopes",
            Self::UnknownScope { .. } => "translation.placeholder.compilation.unknown_scope",
            Self::DuplicateScope { .. } => "translation.placeholder.compilation.duplicate_scope",
            Self::EmptyIds { .. } => "translation.placeholder.compilation.empty_ids",
            Self::InvalidId { .. } => "translation.placeholder.compilation.invalid_id",
            Self::UnknownId { .. } => "translation.placeholder.compilation.unknown_id",
            Self::DuplicateId { .. } => "translation.placeholder.compilation.duplicate_id",
            Self::EmptyPattern { .. } => "translation.placeholder.compilation.empty_pattern",
            Self::InvalidPattern { .. } => "translation.placeholder.compilation.invalid_pattern",
            Self::InvalidNamedCaptures { .. } => {
                "translation.placeholder.compilation.invalid_named_captures"
            }
            Self::ReorderedWrapper { .. } => {
                "translation.placeholder.compilation.reordered_wrapper"
            }
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::WorkerStart { operation, failure } => {
                let mut facts = vec![("operation", operation.as_str().to_owned())];
                facts.extend(failure.facts());
                facts
            }
            Self::EmptyScopes { rule_number }
            | Self::UnknownScope { rule_number }
            | Self::DuplicateScope { rule_number }
            | Self::EmptyIds { rule_number }
            | Self::InvalidId { rule_number }
            | Self::UnknownId { rule_number }
            | Self::DuplicateId { rule_number }
            | Self::EmptyPattern { rule_number }
            | Self::ReorderedWrapper { rule_number } => {
                vec![("rule_number", rule_number.to_string())]
            }
            Self::InvalidPattern { rule_number, pcre2 } => vec![
                ("rule_number", rule_number.to_string()),
                ("pcre2_kind", pcre2.kind.as_str().to_owned()),
                ("pcre2_code", pcre2.code.to_string()),
                ("pcre2_offset", optional_number(pcre2.offset)),
            ],
            Self::InvalidNamedCaptures {
                rule_number,
                actual_count,
            } => vec![
                ("rule_number", rule_number.to_string()),
                ("actual_count", actual_count.to_string()),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranslationTaskPlanningProblem {
    Cancelled,
    EmptyScope,
    EmptyGroup,
    UnitCountOverflow,
    CharacterCountOverflow,
    ResponsibilityCountMismatch { expected: usize, actual: usize },
    TaskIdOverflow,
}

impl TranslationTaskPlanningProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "translation.task_planning.cancelled",
            Self::EmptyScope => "translation.task_planning.empty_scope",
            Self::EmptyGroup => "translation.task_planning.empty_group",
            Self::UnitCountOverflow => "translation.task_planning.unit_count_overflow",
            Self::CharacterCountOverflow => "translation.task_planning.character_count_overflow",
            Self::ResponsibilityCountMismatch { .. } => {
                "translation.task_planning.responsibility_count_mismatch"
            }
            Self::TaskIdOverflow => "translation.task_planning.task_id_overflow",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranslationIssue {
    Placeholder {
        rule_source: PlaceholderRuleSource,
        unit: GenericUnitLocator,
        problem: PlaceholderIssue,
    },
    Prompt {
        path: SafePath,
        problem: PromptProblem,
    },
    BuiltinPlaceholderCompile {
        pcre2: Pcre2Failure,
    },
    LanguageModuleUnavailable {
        requested_language: SafeIdentifier,
        target_language: SafeIdentifier,
        available_languages: Vec<SafeIdentifier>,
    },
    PlanningResource {
        resource: TranslationPlanningResourceKind,
        origin: TranslationPlanningResourceOrigin,
        problem: TranslationPlanningResourceProblem,
    },
    PlaceholderCompilation {
        origin: TranslationPlanningResourceOrigin,
        problem: PlaceholderCompilationProblem,
    },
    TaskPlanning {
        problem: TranslationTaskPlanningProblem,
    },
}

impl TranslationIssue {
    pub(crate) const fn stage(&self) -> super::DiagnosticStage {
        match self {
            Self::Placeholder { .. } => super::DiagnosticStage::Translate,
            Self::Prompt { .. }
            | Self::BuiltinPlaceholderCompile { .. }
            | Self::LanguageModuleUnavailable { .. } => super::DiagnosticStage::CommandPreparation,
            Self::PlanningResource { .. }
            | Self::PlaceholderCompilation { .. }
            | Self::TaskPlanning { .. } => super::DiagnosticStage::Translate,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Placeholder { problem, .. } => problem.code(),
            Self::Prompt { problem, .. } => problem.code(),
            Self::BuiltinPlaceholderCompile { .. } => "translation.placeholder.builtin_compile",
            Self::LanguageModuleUnavailable { .. } => "translation.language_module.unavailable",
            Self::PlanningResource {
                resource, problem, ..
            } => translation_planning_resource_code(*resource, problem),
            Self::PlaceholderCompilation { problem, .. } => problem.code(),
            Self::TaskPlanning { problem } => problem.code(),
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Placeholder { .. } => DiagnosticResolution::FixPlaceholderRules,
            Self::Prompt { problem, .. } => problem.resolution(),
            Self::BuiltinPlaceholderCompile { .. } => DiagnosticResolution::ReportBug,
            Self::LanguageModuleUnavailable { .. } => DiagnosticResolution::FixConfiguration,
            Self::PlanningResource {
                origin, problem, ..
            } => match problem {
                TranslationPlanningResourceProblem::Cancelled
                | TranslationPlanningResourceProblem::WorkerStart { .. } => {
                    DiagnosticResolution::Retry
                }
                TranslationPlanningResourceProblem::InvalidSnapshotJson { .. } => match origin {
                    TranslationPlanningResourceOrigin::ExternalFile { .. } => {
                        DiagnosticResolution::FixInput
                    }
                    TranslationPlanningResourceOrigin::ProjectSnapshot => {
                        DiagnosticResolution::CheckProjectState
                    }
                },
                TranslationPlanningResourceProblem::SnapshotEncodingJson { .. }
                | TranslationPlanningResourceProblem::MatcherConstruction => {
                    DiagnosticResolution::ReportBug
                }
                _ => DiagnosticResolution::FixInput,
            },
            Self::PlaceholderCompilation { problem, .. } => match problem {
                PlaceholderCompilationProblem::WorkerStart { .. } => DiagnosticResolution::Retry,
                _ => DiagnosticResolution::FixPlaceholderRules,
            },
            Self::TaskPlanning { problem } => match problem {
                TranslationTaskPlanningProblem::Cancelled => DiagnosticResolution::Retry,
                _ => DiagnosticResolution::ReportBug,
            },
        }
    }

    pub(crate) fn summary_code(&self) -> &'static str {
        match self {
            Self::Placeholder { problem, .. } => problem.summary_code(),
            Self::Prompt { problem, .. } => problem.summary_code(),
            Self::BuiltinPlaceholderCompile { .. } => "internal_invariant",
            Self::LanguageModuleUnavailable { .. } => "not_found",
            Self::PlanningResource { problem, .. } => match problem {
                TranslationPlanningResourceProblem::Cancelled => "cancelled",
                TranslationPlanningResourceProblem::WorkerStart { .. } => "worker_spawn_failed",
                TranslationPlanningResourceProblem::InvalidUtf8 { .. } => "invalid_encoding",
                TranslationPlanningResourceProblem::InvalidToml { .. } => "invalid_syntax",
                TranslationPlanningResourceProblem::InvalidSnapshotJson { .. } => "invalid_value",
                TranslationPlanningResourceProblem::SnapshotEncodingJson { .. }
                | TranslationPlanningResourceProblem::MatcherConstruction => "internal_invariant",
                _ => "invalid_value",
            },
            Self::PlaceholderCompilation { problem, .. } => match problem {
                PlaceholderCompilationProblem::WorkerStart { .. } => "worker_spawn_failed",
                PlaceholderCompilationProblem::InvalidPattern { .. } => "invalid_syntax",
                _ => "invalid_value",
            },
            Self::TaskPlanning { problem } => match problem {
                TranslationTaskPlanningProblem::Cancelled => "cancelled",
                _ => "internal_invariant",
            },
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::Placeholder { unit, .. } => unit.readable_id(),
            Self::Prompt { path, .. } => path.to_string(),
            Self::BuiltinPlaceholderCompile { .. } => "builtin_placeholder_rules".to_owned(),
            Self::LanguageModuleUnavailable {
                requested_language, ..
            } => requested_language.to_string(),
            Self::PlanningResource {
                resource, origin, ..
            } => match origin {
                TranslationPlanningResourceOrigin::ExternalFile { path } => path.to_string(),
                TranslationPlanningResourceOrigin::ProjectSnapshot => resource.as_str().to_owned(),
            },
            Self::PlaceholderCompilation { origin, .. } => match origin {
                TranslationPlanningResourceOrigin::ExternalFile { path } => path.to_string(),
                TranslationPlanningResourceOrigin::ProjectSnapshot => {
                    "placeholder_rules".to_owned()
                }
            },
            Self::TaskPlanning { .. } => "task_block_planning".to_owned(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Placeholder {
                rule_source,
                unit,
                problem,
            } => {
                let mut facts = vec![
                    (
                        "rule_source",
                        match rule_source {
                            PlaceholderRuleSource::ExternalFile { path } => path.to_string(),
                            PlaceholderRuleSource::ProjectSnapshot => "project_snapshot".to_owned(),
                        },
                    ),
                    ("relative_path", unit.relative_path.to_string()),
                ];
                if let Some(group_id) = &unit.group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                if let Some(unit_id) = &unit.unit_id {
                    facts.push(("unit_id", unit_id.to_string()));
                }
                if let Some(role) = &unit.role {
                    facts.push(("role", role.to_string()));
                }
                facts.extend(problem.facts());
                facts
            }
            Self::Prompt { path, problem } => {
                let mut facts = vec![("path", path.to_string())];
                facts.extend(problem.facts());
                facts
            }
            Self::BuiltinPlaceholderCompile { pcre2 } => vec![
                ("pcre2_kind", pcre2.kind.as_str().to_owned()),
                ("pcre2_code", pcre2.code.to_string()),
                ("pcre2_offset", optional_number(pcre2.offset)),
            ],
            Self::LanguageModuleUnavailable {
                requested_language,
                target_language,
                available_languages,
            } => vec![
                ("requested_language", requested_language.to_string()),
                ("target_language", target_language.to_string()),
                (
                    "available_languages",
                    if available_languages.is_empty() {
                        "none".to_owned()
                    } else {
                        available_languages
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    },
                ),
            ],
            Self::PlanningResource {
                resource,
                origin,
                problem,
            } => {
                let mut facts = vec![
                    ("resource", resource.as_str().to_owned()),
                    ("resource_origin", origin.fact_value()),
                    ("problem", problem.code_suffix().to_owned()),
                ];
                facts.extend(problem.facts());
                facts
            }
            Self::PlaceholderCompilation { origin, problem } => {
                let mut facts = vec![("rule_source", origin.fact_value())];
                facts.extend(problem.facts());
                facts
            }
            Self::TaskPlanning { problem } => match problem {
                TranslationTaskPlanningProblem::ResponsibilityCountMismatch {
                    expected,
                    actual,
                } => vec![
                    ("expected", expected.to_string()),
                    ("actual", actual.to_string()),
                ],
                _ => Vec::new(),
            },
        }
    }
}

fn translation_planning_resource_code(
    resource: TranslationPlanningResourceKind,
    problem: &TranslationPlanningResourceProblem,
) -> &'static str {
    match (resource, problem) {
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::Cancelled,
        ) => "translation.terminology.cancelled",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::WorkerStart { .. },
        ) => "translation.terminology.worker_start",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::InvalidUtf8 { .. },
        ) => "translation.terminology.invalid_utf8",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::InvalidToml { .. },
        ) => "translation.terminology.invalid_toml",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::InvalidSnapshotJson { .. },
        ) => "translation.terminology.invalid_snapshot_json",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::SnapshotEncodingJson { .. },
        ) => "translation.terminology.snapshot_encoding_json",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::BlankField { .. },
        ) => "translation.terminology.blank_field",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::SurroundingWhitespace { .. },
        ) => "translation.terminology.surrounding_whitespace",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::ControlCharacter { .. },
        ) => "translation.terminology.control_character",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::EmptyTriggers { .. },
        ) => "translation.terminology.empty_triggers",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::DuplicateTerm,
        ) => "translation.terminology.duplicate_term",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::DuplicateTrigger,
        ) => "translation.terminology.duplicate_trigger",
        (
            TranslationPlanningResourceKind::Terminology,
            TranslationPlanningResourceProblem::MatcherConstruction,
        ) => "translation.terminology.matcher_construction",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::Cancelled,
        ) => "translation.placeholder_definition.cancelled",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::WorkerStart { .. },
        ) => "translation.placeholder_definition.worker_start",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::InvalidUtf8 { .. },
        ) => "translation.placeholder_definition.invalid_utf8",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::InvalidToml { .. },
        ) => "translation.placeholder_definition.invalid_toml",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::InvalidSnapshotJson { .. },
        ) => "translation.placeholder_definition.invalid_snapshot_json",
        (
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::SnapshotEncodingJson { .. },
        ) => "translation.placeholder_definition.snapshot_encoding_json",
        (TranslationPlanningResourceKind::PlaceholderRules, _) => {
            "translation.placeholder_definition.invalid_contract"
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    deny_unknown_fields,
    tag = "family",
    content = "details",
    rename_all = "snake_case"
)]
pub(crate) enum DiagnosticIssue {
    Configuration(ConfigurationIssue),
    Translation(TranslationIssue),
    Generic(super::GenericIssue),
    Lua(super::LuaIssue),
    RpgMaker(super::RpgMakerIssue),
    Publication(super::PublicationIssue),
    Runtime(super::RuntimeIssue),
    FileSystem(super::FileSystemIssue),
    Sqlite(super::SqliteIssue),
    Http(super::HttpIssue),
    Observability(super::ObservabilityIssue),
}

impl DiagnosticIssue {
    pub(crate) const fn stage(&self) -> super::DiagnosticStage {
        match self {
            Self::Configuration(_) => super::DiagnosticStage::Configuration,
            Self::Translation(issue) => issue.stage(),
            Self::Generic(issue) => issue.stage(),
            Self::Lua(issue) => issue.stage(),
            Self::RpgMaker(issue) => issue.stage(),
            Self::Publication(issue) => issue.stage(),
            Self::Runtime(issue) => issue.stage(),
            Self::FileSystem(issue) => issue.stage(),
            Self::Sqlite(issue) => issue.stage(),
            Self::Http(issue) => issue.stage(),
            Self::Observability(issue) => issue.stage(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Configuration(issue) => issue.code(),
            Self::Translation(issue) => issue.code(),
            Self::Generic(issue) => issue.code(),
            Self::Lua(issue) => issue.code(),
            Self::RpgMaker(issue) => issue.code(),
            Self::Publication(issue) => issue.code(),
            Self::Runtime(issue) => issue.code(),
            Self::FileSystem(issue) => issue.code(),
            Self::Sqlite(issue) => issue.code(),
            Self::Http(issue) => issue.code(),
            Self::Observability(issue) => issue.code(),
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Configuration(issue) => issue.resolution(),
            Self::Translation(issue) => issue.resolution(),
            Self::Generic(issue) => issue.resolution(),
            Self::Lua(issue) => issue.resolution(),
            Self::RpgMaker(issue) => issue.resolution(),
            Self::Publication(issue) => issue.resolution(),
            Self::Runtime(issue) => issue.resolution(),
            Self::FileSystem(issue) => issue.resolution(),
            Self::Sqlite(issue) => issue.resolution(),
            Self::Http(issue) => issue.resolution(),
            Self::Observability(issue) => issue.resolution(),
        }
    }

    pub(crate) fn summary_code(&self) -> &'static str {
        match self {
            Self::Configuration(issue) => issue.summary_code(),
            Self::Translation(issue) => issue.summary_code(),
            Self::Generic(issue) => issue.summary_code(),
            Self::Lua(issue) => issue.summary_code(),
            Self::RpgMaker(issue) => issue.summary_code(),
            Self::Publication(issue) => issue.summary_code(),
            Self::Runtime(issue) => issue.summary_code(),
            Self::FileSystem(issue) => issue.summary_code(),
            Self::Sqlite(issue) => issue.summary_code(),
            Self::Http(issue) => issue.summary_code(),
            Self::Observability(issue) => issue.summary_code(),
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::Configuration(issue) => issue.subject(),
            Self::Translation(issue) => issue.subject(),
            Self::Generic(issue) => issue.subject(),
            Self::Lua(issue) => issue.subject(),
            Self::RpgMaker(issue) => issue.subject(),
            Self::Publication(issue) => issue.subject(),
            Self::Runtime(issue) => issue.subject(),
            Self::FileSystem(issue) => issue.subject(),
            Self::Sqlite(issue) => issue.subject(),
            Self::Http(issue) => issue.subject(),
            Self::Observability(issue) => issue.subject(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Configuration(issue) => issue.facts(),
            Self::Translation(issue) => issue.facts(),
            Self::Generic(issue) => issue.facts(),
            Self::Lua(issue) => issue.facts(),
            Self::RpgMaker(issue) => issue.facts(),
            Self::Publication(issue) => issue.facts(),
            Self::Runtime(issue) => issue.facts(),
            Self::FileSystem(issue) => issue.facts(),
            Self::Sqlite(issue) => issue.facts(),
            Self::Http(issue) => issue.facts(),
            Self::Observability(issue) => issue.facts(),
        }
    }
}

impl From<TranslationIssue> for DiagnosticIssue {
    fn from(value: TranslationIssue) -> Self {
        Self::Translation(value)
    }
}

impl From<ConfigurationIssue> for DiagnosticIssue {
    fn from(value: ConfigurationIssue) -> Self {
        Self::Configuration(value)
    }
}

impl From<super::GenericIssue> for DiagnosticIssue {
    fn from(value: super::GenericIssue) -> Self {
        Self::Generic(value)
    }
}

impl From<super::LuaIssue> for DiagnosticIssue {
    fn from(value: super::LuaIssue) -> Self {
        Self::Lua(value)
    }
}

impl From<super::RpgMakerIssue> for DiagnosticIssue {
    fn from(value: super::RpgMakerIssue) -> Self {
        Self::RpgMaker(value)
    }
}

impl From<super::PublicationIssue> for DiagnosticIssue {
    fn from(value: super::PublicationIssue) -> Self {
        Self::Publication(value)
    }
}

impl From<super::RuntimeIssue> for DiagnosticIssue {
    fn from(value: super::RuntimeIssue) -> Self {
        Self::Runtime(value)
    }
}

impl From<super::FileSystemIssue> for DiagnosticIssue {
    fn from(value: super::FileSystemIssue) -> Self {
        Self::FileSystem(value)
    }
}

impl From<super::SqliteIssue> for DiagnosticIssue {
    fn from(value: super::SqliteIssue) -> Self {
        Self::Sqlite(value)
    }
}

impl From<super::HttpIssue> for DiagnosticIssue {
    fn from(value: super::HttpIssue) -> Self {
        Self::Http(value)
    }
}

impl From<super::ObservabilityIssue> for DiagnosticIssue {
    fn from(value: super::ObservabilityIssue) -> Self {
        Self::Observability(value)
    }
}
