//! Generic JSONL、项目状态与模型任务记录边界建立的封闭诊断问题。

use std::num::NonZeroUsize;

use serde::{Deserialize, Deserializer, Serialize, de};

use super::DiagnosticStage;
use super::issue::{
    GenericUnitLocator, IoFailure, PlaceholderCompilationProblem, PlaceholderIssue,
};
use super::model::DiagnosticResolution;
use super::safe_value::{SafeIdentifier, SafePath, SafeText};
use crate::json_diagnostic::JsonErrorCategory;
use crate::translation::candidate_validation::ProvenInvariantViolation;

/// Generic 问题发生时已经确定的命令阶段。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericDiagnosticStage {
    ProjectOpening,
    Init,
    Extract,
    Translate,
    WriteBack,
    TaskRecord,
}

impl GenericDiagnosticStage {
    const fn diagnostic_stage(self) -> DiagnosticStage {
        match self {
            Self::ProjectOpening => DiagnosticStage::ProjectOpening,
            Self::Init => DiagnosticStage::Init,
            Self::Extract => DiagnosticStage::Extract,
            Self::Translate => DiagnosticStage::Translate,
            Self::WriteBack => DiagnosticStage::WriteBack,
            Self::TaskRecord => DiagnosticStage::Translate,
        }
    }
}

/// Generic 模块执行的稳定操作标识；不得由展示文本反推。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericOperation {
    ScanInput,
    ParseJsonl,
    SerializeJsonl,
    OpenProject,
    InitializeProject,
    LoadSnapshot,
    ExtractInput,
    ResolveRunPlan,
    PrepareTranslation,
    CommitTranslations,
    BuildWriteBackCandidate,
    MaterializeWriteBack,
    RecheckInput,
    RecordTask,
}

impl GenericOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ScanInput => "scan_input",
            Self::ParseJsonl => "parse_jsonl",
            Self::SerializeJsonl => "serialize_jsonl",
            Self::OpenProject => "open_project",
            Self::InitializeProject => "initialize_project",
            Self::LoadSnapshot => "load_snapshot",
            Self::ExtractInput => "extract_input",
            Self::ResolveRunPlan => "resolve_run_plan",
            Self::PrepareTranslation => "prepare_translation",
            Self::CommitTranslations => "commit_translations",
            Self::BuildWriteBackCandidate => "build_write_back_candidate",
            Self::MaterializeWriteBack => "materialize_write_back",
            Self::RecheckInput => "recheck_input",
            Self::RecordTask => "record_task",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericTextViolation {
    CarriageReturn,
    Nul,
}

/// Generic 项目语言字段违反的闭集规则。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericLanguageViolation {
    Blank,
    SurroundingWhitespace,
    Underscore,
    InvalidSyntax,
    InvalidRegistryTag,
    CanonicalizationFailed,
    UndefinedPrimaryLanguage,
}

impl GenericLanguageViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::SurroundingWhitespace => "surrounding_whitespace",
            Self::Underscore => "underscore",
            Self::InvalidSyntax => "invalid_syntax",
            Self::InvalidRegistryTag => "invalid_registry_tag",
            Self::CanonicalizationFailed => "canonicalization_failed",
            Self::UndefinedPrimaryLanguage => "undefined_primary_language",
        }
    }
}

/// Generic 项目保存的翻译资源种类。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericResourceKind {
    Terminology,
    PlaceholderRules,
}

/// Generic 项目数据库违反的当前 schema 或状态不变量。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericProjectDatabaseProblem {
    UnexpectedCommittedUnit {
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    DuplicateCommittedUnit {
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    InvalidProjectName,
    MissingProjectRow,
    InvalidTextColumnUtf8 {
        operation: SafeIdentifier,
        column: usize,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    ManualTranslationStateFailure,
    SnapshotFileCount {
        stored: usize,
        extracted: usize,
    },
    SnapshotFileMismatch {
        relative_path: SafePath,
    },
    SnapshotGroupMismatch {
        relative_path: SafePath,
        group_id: Option<SafeIdentifier>,
    },
    SnapshotUnitMismatch {
        relative_path: SafePath,
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    GroupReferencesMissingFile {
        group_id: Option<SafeIdentifier>,
    },
    IncompleteTranslationState {
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    UnitReferencesMissingGroup {
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    SchemaMismatch {
        expected_count: usize,
        actual_count: usize,
        missing: Vec<SafeIdentifier>,
        definition_mismatches: Vec<SafeIdentifier>,
        unexpected: Vec<SafeIdentifier>,
    },
    TranslationResourceCount {
        expected: usize,
        actual: i64,
    },
    ForeignKeyViolation {
        table: SafeIdentifier,
    },
    QuickCheckFailed,
    UnextractedProjectHasAssets {
        count: i64,
    },
    OrdinalTooLarge {
        value: usize,
    },
    InvalidOrdinal {
        field: SafeIdentifier,
        value: i64,
    },
    InvalidFingerprintLength {
        field: SafeIdentifier,
        expected: usize,
        actual: usize,
    },
    InvalidUtf16Path {
        actual_bytes: usize,
    },
}

impl GenericProjectDatabaseProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::UnexpectedCommittedUnit { .. } => {
                "generic.project.database.unexpected_committed_unit"
            }
            Self::DuplicateCommittedUnit { .. } => {
                "generic.project.database.duplicate_committed_unit"
            }
            Self::InvalidProjectName => "generic.project.database.invalid_project_name",
            Self::MissingProjectRow => "generic.project.database.missing_project_row",
            Self::InvalidTextColumnUtf8 { .. } => {
                "generic.project.database.text_column_invalid_utf8"
            }
            Self::ManualTranslationStateFailure => {
                "generic.project.database.manual_translation_state"
            }
            Self::SnapshotFileCount { .. } => "generic.project.database.snapshot_file_count",
            Self::SnapshotFileMismatch { .. } => "generic.project.database.snapshot_file_mismatch",
            Self::SnapshotGroupMismatch { .. } => {
                "generic.project.database.snapshot_group_mismatch"
            }
            Self::SnapshotUnitMismatch { .. } => "generic.project.database.snapshot_unit_mismatch",
            Self::GroupReferencesMissingFile { .. } => {
                "generic.project.database.group_missing_file"
            }
            Self::IncompleteTranslationState { .. } => {
                "generic.project.database.incomplete_translation_state"
            }
            Self::UnitReferencesMissingGroup { .. } => {
                "generic.project.database.unit_missing_group"
            }
            Self::SchemaMismatch { .. } => "generic.project.database.schema_mismatch",
            Self::TranslationResourceCount { .. } => {
                "generic.project.database.translation_resource_count"
            }
            Self::ForeignKeyViolation { .. } => "generic.project.database.foreign_key_violation",
            Self::QuickCheckFailed => "generic.project.database.quick_check_failed",
            Self::UnextractedProjectHasAssets { .. } => {
                "generic.project.database.unextracted_assets"
            }
            Self::OrdinalTooLarge { .. } => "generic.project.database.ordinal_too_large",
            Self::InvalidOrdinal { .. } => "generic.project.database.invalid_ordinal",
            Self::InvalidFingerprintLength { .. } => {
                "generic.project.database.invalid_fingerprint_length"
            }
            Self::InvalidUtf16Path { .. } => "generic.project.database.invalid_utf16_path",
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::UnexpectedCommittedUnit {
                group_id, unit_id, ..
            }
            | Self::DuplicateCommittedUnit {
                group_id, unit_id, ..
            }
            | Self::IncompleteTranslationState {
                group_id, unit_id, ..
            }
            | Self::UnitReferencesMissingGroup {
                group_id, unit_id, ..
            }
            | Self::SnapshotUnitMismatch {
                group_id, unit_id, ..
            } => match (group_id, unit_id) {
                (Some(group_id), Some(unit_id)) => format!("{group_id}/{unit_id}"),
                _ => "generic_unit".to_owned(),
            },
            Self::SnapshotFileMismatch { relative_path }
            | Self::SnapshotGroupMismatch { relative_path, .. } => relative_path.to_string(),
            Self::GroupReferencesMissingFile {
                group_id: Some(group_id),
            } => group_id.to_string(),
            _ => "generic_project_database".to_owned(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let identifiers = |values: &[SafeIdentifier]| {
            values
                .iter()
                .map(SafeIdentifier::as_str)
                .collect::<Vec<_>>()
                .join(",")
        };
        let unit_facts = |group_id: &Option<SafeIdentifier>, unit_id: &Option<SafeIdentifier>| {
            let mut facts = Vec::new();
            if let Some(group_id) = group_id {
                facts.push(("group_id", group_id.to_string()));
            }
            if let Some(unit_id) = unit_id {
                facts.push(("unit_id", unit_id.to_string()));
            }
            facts
        };
        match self {
            Self::UnexpectedCommittedUnit {
                group_id, unit_id, ..
            }
            | Self::DuplicateCommittedUnit {
                group_id, unit_id, ..
            }
            | Self::IncompleteTranslationState {
                group_id, unit_id, ..
            }
            | Self::UnitReferencesMissingGroup {
                group_id, unit_id, ..
            } => unit_facts(group_id, unit_id),
            Self::InvalidTextColumnUtf8 {
                operation,
                column,
                valid_up_to,
                error_len,
            } => vec![
                ("operation", operation.to_string()),
                ("column", column.to_string()),
                ("valid_up_to", valid_up_to.to_string()),
                ("error_len", optional_number(*error_len)),
            ],
            Self::SnapshotFileCount { stored, extracted } => vec![
                ("stored_files", stored.to_string()),
                ("extracted_files", extracted.to_string()),
            ],
            Self::SnapshotFileMismatch { relative_path } => {
                vec![("relative_path", relative_path.to_string())]
            }
            Self::SnapshotGroupMismatch {
                relative_path,
                group_id,
            } => {
                let mut facts = vec![("relative_path", relative_path.to_string())];
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                facts
            }
            Self::SnapshotUnitMismatch {
                relative_path,
                group_id,
                unit_id,
            } => {
                let mut facts = vec![("relative_path", relative_path.to_string())];
                facts.extend(unit_facts(group_id, unit_id));
                facts
            }
            Self::GroupReferencesMissingFile { group_id } => group_id
                .as_ref()
                .map_or_else(Vec::new, |value| vec![("group_id", value.to_string())]),
            Self::SchemaMismatch {
                expected_count,
                actual_count,
                missing,
                definition_mismatches,
                unexpected,
            } => vec![
                ("expected_count", expected_count.to_string()),
                ("actual_count", actual_count.to_string()),
                ("missing", identifiers(missing)),
                ("definition_mismatches", identifiers(definition_mismatches)),
                ("unexpected", identifiers(unexpected)),
            ],
            Self::TranslationResourceCount { expected, actual } => vec![
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ],
            Self::ForeignKeyViolation { table } => vec![("table", table.to_string())],
            Self::UnextractedProjectHasAssets { count } => vec![("count", count.to_string())],
            Self::OrdinalTooLarge { value } => vec![("value", value.to_string())],
            Self::InvalidOrdinal { field, value } => {
                vec![("field", field.to_string()), ("value", value.to_string())]
            }
            Self::InvalidFingerprintLength {
                field,
                expected,
                actual,
            } => vec![
                ("field", field.to_string()),
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ],
            Self::InvalidUtf16Path { actual_bytes } => {
                vec![("actual_bytes", actual_bytes.to_string())]
            }
            Self::InvalidProjectName
            | Self::MissingProjectRow
            | Self::ManualTranslationStateFailure
            | Self::QuickCheckFailed => Vec::new(),
        }
    }
}

/// 写入 Generic 项目的译文违反了哪一项可公开契约。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericProjectTranslationProblem {
    Blank,
    CarriageReturn,
    Nul,
    InvalidPlaceholderSnapshot {
        category: GenericJsonErrorCategory,
        line: usize,
        column: usize,
    },
    PlaceholderCompilation {
        problem: PlaceholderCompilationProblem,
    },
    PlaceholderProtection {
        problem: PlaceholderIssue,
    },
    PlaceholderRestoreProjection {
        problem: GenericLanguageProjectionProblem,
    },
    PlaceholderRestoreMultiset {
        problem: GenericPlaceholderMultisetProblem,
    },
    PlaceholderBindingMismatch,
}

impl GenericProjectTranslationProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::Blank => "generic.project.translation.blank",
            Self::CarriageReturn => "generic.project.translation.carriage_return",
            Self::Nul => "generic.project.translation.nul",
            Self::InvalidPlaceholderSnapshot { .. } => {
                "generic.project.translation.placeholder_snapshot_invalid"
            }
            Self::PlaceholderCompilation { problem } => problem.code(),
            Self::PlaceholderProtection { problem } => problem.code(),
            Self::PlaceholderRestoreProjection { .. } => {
                "generic.project.translation.placeholder_restore_projection"
            }
            Self::PlaceholderRestoreMultiset { .. } => {
                "generic.project.translation.placeholder_restore_multiset"
            }
            Self::PlaceholderBindingMismatch => {
                "generic.project.translation.placeholder_binding_mismatch"
            }
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Blank | Self::CarriageReturn | Self::Nul => Vec::new(),
            Self::InvalidPlaceholderSnapshot {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("line", line.to_string()),
                ("column", column.to_string()),
            ],
            Self::PlaceholderCompilation { problem } => problem.facts(),
            Self::PlaceholderProtection { problem } => problem.facts(),
            Self::PlaceholderRestoreProjection { problem } => problem.facts(),
            Self::PlaceholderRestoreMultiset { .. } | Self::PlaceholderBindingMismatch => {
                Vec::new()
            }
        }
    }
}

/// Generic WriteBack 发现数据库快照、当前输入或候选往返结果不一致的具体位置。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericWriteBackSnapshotProblem {
    FileCount {
        stored: usize,
        input: usize,
    },
    FilePath {
        stored_path: SafePath,
        input_path: SafePath,
    },
    GroupCount {
        relative_path: SafePath,
        stored: usize,
        input: usize,
    },
    GroupShape {
        relative_path: SafePath,
        group_ordinal: usize,
        group_id: Option<SafeIdentifier>,
    },
    UnitShapeOrSource {
        relative_path: SafePath,
        group_ordinal: usize,
        unit_ordinal: usize,
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    RoundTripGroupCount {
        relative_path: SafePath,
        source: usize,
        candidate: usize,
    },
    RoundTripGroupShape {
        relative_path: SafePath,
        group_ordinal: usize,
        group_id: Option<SafeIdentifier>,
    },
    RoundTripUnitId {
        relative_path: SafePath,
        group_ordinal: usize,
        unit_ordinal: usize,
        group_id: Option<SafeIdentifier>,
        expected_unit_id: Option<SafeIdentifier>,
        actual_unit_id: Option<SafeIdentifier>,
    },
    RoundTripUnitText {
        relative_path: SafePath,
        group_ordinal: usize,
        unit_ordinal: usize,
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
}

/// Generic WriteBack 在单个 Unit 上处理的是原文还是当前译文。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericWriteBackTextSide {
    Source,
    Translation,
}

impl GenericWriteBackTextSide {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Translation => "translation",
        }
    }
}

/// Generic WriteBack 构造候选时发现的 Unit 级失败。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericWriteBackUnitProblem {
    CandidateViolation {
        problem: ProvenInvariantViolation,
    },
    PlaceholderProtection {
        side: GenericWriteBackTextSide,
        problem: PlaceholderIssue,
    },
    PlaceholderBindingMismatch {
        side: GenericWriteBackTextSide,
    },
    LanguageProjection {
        side: GenericWriteBackTextSide,
        problem: GenericLanguageProjectionProblem,
    },
    LayoutRestoration,
}

impl GenericWriteBackUnitProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::CandidateViolation { .. } => "generic.write_back.candidate.invalid",
            Self::PlaceholderProtection { problem, .. } => match problem {
                PlaceholderIssue::WorkerStart { .. } => {
                    "generic.write_back.placeholder.worker_start"
                }
                PlaceholderIssue::PatternMatch { .. } => {
                    "generic.write_back.placeholder.pattern_match"
                }
                PlaceholderIssue::EmptyMatch { .. } => "generic.write_back.placeholder.empty_match",
                PlaceholderIssue::MissingTextCapture { .. } => {
                    "generic.write_back.placeholder.missing_text_capture"
                }
                PlaceholderIssue::InvalidMatchRange { .. } => {
                    "generic.write_back.placeholder.invalid_match_range"
                }
                PlaceholderIssue::OverlappingMatches { .. } => {
                    "generic.write_back.placeholder.overlapping_matches"
                }
                PlaceholderIssue::CrossesLineBoundary { .. } => {
                    "generic.write_back.placeholder.crosses_line_boundary"
                }
                PlaceholderIssue::ReservedTokenNamespace { .. } => {
                    "generic.write_back.placeholder.reserved_token_namespace"
                }
            },
            Self::PlaceholderBindingMismatch { .. } => {
                "generic.write_back.placeholder.binding_mismatch"
            }
            Self::LanguageProjection { problem, .. } => match problem {
                GenericLanguageProjectionProblem::TokenIndexConstruction => {
                    "generic.write_back.language_projection.token_index_construction"
                }
                GenericLanguageProjectionProblem::EmptyToken => {
                    "generic.write_back.language_projection.empty_token"
                }
                GenericLanguageProjectionProblem::MissingToken => {
                    "generic.write_back.language_projection.missing_token"
                }
                GenericLanguageProjectionProblem::RepeatedToken => {
                    "generic.write_back.language_projection.repeated_token"
                }
                GenericLanguageProjectionProblem::OverlappingToken => {
                    "generic.write_back.language_projection.overlapping_token"
                }
                GenericLanguageProjectionProblem::ChangedTokenOrder { .. } => {
                    "generic.write_back.language_projection.changed_token_order"
                }
                GenericLanguageProjectionProblem::ChangedSegmentCount { .. } => {
                    "generic.write_back.language_projection.changed_segment_count"
                }
                GenericLanguageProjectionProblem::ChangedSegmentKind { .. } => {
                    "generic.write_back.language_projection.changed_segment_kind"
                }
                GenericLanguageProjectionProblem::MissingOrderedToken { .. } => {
                    "generic.write_back.language_projection.missing_ordered_token"
                }
                GenericLanguageProjectionProblem::UnusedOrderedToken => {
                    "generic.write_back.language_projection.unused_ordered_token"
                }
            },
            Self::LayoutRestoration => "generic.write_back.layout_restoration",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::CandidateViolation { .. } => DiagnosticResolution::FixInput,
            Self::PlaceholderProtection {
                problem: PlaceholderIssue::WorkerStart { .. },
                ..
            } => DiagnosticResolution::Retry,
            Self::PlaceholderProtection { .. } => DiagnosticResolution::FixPlaceholderRules,
            Self::PlaceholderBindingMismatch { .. } => DiagnosticResolution::FixInput,
            Self::LanguageProjection { .. } => DiagnosticResolution::ReportBug,
            Self::LayoutRestoration => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::CandidateViolation { .. } => "invalid_value",
            Self::PlaceholderProtection {
                problem: PlaceholderIssue::WorkerStart { .. },
                ..
            } => "worker_spawn_failed",
            Self::PlaceholderProtection { .. } | Self::PlaceholderBindingMismatch { .. } => {
                "invalid_value"
            }
            Self::LanguageProjection { .. } => "internal_invariant",
            Self::LayoutRestoration => "internal_invariant",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::CandidateViolation { problem } => vec![(
                "violation",
                serde_json::to_string(problem).expect("闭集候选违反项必须可以编码"),
            )],
            Self::PlaceholderProtection { side, problem } => {
                let mut facts = vec![("side", side.as_str().to_owned())];
                facts.extend(problem.facts());
                facts
            }
            Self::PlaceholderBindingMismatch { side } => {
                vec![("side", side.as_str().to_owned())]
            }
            Self::LanguageProjection { side, problem } => {
                let mut facts = vec![("side", side.as_str().to_owned())];
                facts.extend(problem.facts());
                facts
            }
            Self::LayoutRestoration => Vec::new(),
        }
    }
}

impl GenericWriteBackSnapshotProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::FileCount { .. } => "generic.write_back.snapshot.file_count",
            Self::FilePath { .. } => "generic.write_back.snapshot.file_path",
            Self::GroupCount { .. } => "generic.write_back.snapshot.group_count",
            Self::GroupShape { .. } => "generic.write_back.snapshot.group_shape",
            Self::UnitShapeOrSource { .. } => "generic.write_back.snapshot.unit_shape_or_source",
            Self::RoundTripGroupCount { .. } => {
                "generic.write_back.snapshot.round_trip_group_count"
            }
            Self::RoundTripGroupShape { .. } => {
                "generic.write_back.snapshot.round_trip_group_shape"
            }
            Self::RoundTripUnitId { .. } => "generic.write_back.snapshot.round_trip_unit_id",
            Self::RoundTripUnitText { .. } => "generic.write_back.snapshot.round_trip_unit_text",
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::FileCount { .. } => "generic_project_snapshot".to_owned(),
            Self::FilePath { input_path, .. }
            | Self::GroupCount {
                relative_path: input_path,
                ..
            }
            | Self::GroupShape {
                relative_path: input_path,
                ..
            }
            | Self::UnitShapeOrSource {
                relative_path: input_path,
                ..
            }
            | Self::RoundTripGroupCount {
                relative_path: input_path,
                ..
            }
            | Self::RoundTripGroupShape {
                relative_path: input_path,
                ..
            }
            | Self::RoundTripUnitId {
                relative_path: input_path,
                ..
            }
            | Self::RoundTripUnitText {
                relative_path: input_path,
                ..
            } => input_path.to_string(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::FileCount { stored, input } => vec![
                ("stored_files", stored.to_string()),
                ("input_files", input.to_string()),
            ],
            Self::FilePath {
                stored_path,
                input_path,
            } => vec![
                ("stored_path", stored_path.to_string()),
                ("input_path", input_path.to_string()),
            ],
            Self::GroupCount {
                relative_path,
                stored,
                input,
            } => vec![
                ("relative_path", relative_path.to_string()),
                ("stored_groups", stored.to_string()),
                ("input_groups", input.to_string()),
            ],
            Self::GroupShape {
                relative_path,
                group_ordinal,
                group_id,
            }
            | Self::RoundTripGroupShape {
                relative_path,
                group_ordinal,
                group_id,
            } => {
                let mut facts = vec![
                    ("relative_path", relative_path.to_string()),
                    ("group_ordinal", group_ordinal.to_string()),
                ];
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                facts
            }
            Self::UnitShapeOrSource {
                relative_path,
                group_ordinal,
                unit_ordinal,
                group_id,
                unit_id,
            }
            | Self::RoundTripUnitText {
                relative_path,
                group_ordinal,
                unit_ordinal,
                group_id,
                unit_id,
            } => unit_snapshot_facts(
                relative_path,
                *group_ordinal,
                *unit_ordinal,
                group_id.as_ref(),
                unit_id.as_ref(),
            ),
            Self::RoundTripGroupCount {
                relative_path,
                source,
                candidate,
            } => vec![
                ("relative_path", relative_path.to_string()),
                ("source_groups", source.to_string()),
                ("candidate_groups", candidate.to_string()),
            ],
            Self::RoundTripUnitId {
                relative_path,
                group_ordinal,
                unit_ordinal,
                group_id,
                expected_unit_id,
                actual_unit_id,
            } => {
                let mut facts = unit_snapshot_facts(
                    relative_path,
                    *group_ordinal,
                    *unit_ordinal,
                    group_id.as_ref(),
                    expected_unit_id.as_ref(),
                );
                if let Some(actual_unit_id) = actual_unit_id {
                    facts.push(("actual_unit_id", actual_unit_id.to_string()));
                }
                facts
            }
        }
    }
}

fn unit_snapshot_facts(
    relative_path: &SafePath,
    group_ordinal: usize,
    unit_ordinal: usize,
    group_id: Option<&SafeIdentifier>,
    unit_id: Option<&SafeIdentifier>,
) -> Vec<(&'static str, String)> {
    let mut facts = vec![
        ("relative_path", relative_path.to_string()),
        ("group_ordinal", group_ordinal.to_string()),
        ("unit_ordinal", unit_ordinal.to_string()),
    ];
    if let Some(group_id) = group_id {
        facts.push(("group_id", group_id.to_string()));
    }
    if let Some(unit_id) = unit_id {
        facts.push(("unit_id", unit_id.to_string()));
    }
    facts
}

impl GenericResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Terminology => "terminology",
            Self::PlaceholderRules => "placeholder_rules",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericTaskResponseJsonCategory {
    Io,
    Syntax,
    Shape,
    UnexpectedEof,
}

impl GenericTaskResponseJsonCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Shape => "shape",
            Self::UnexpectedEof => "unexpected_eof",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericResponseValueProblem {
    TranslationNotArray,
    TranslationNonStringItem { item: NonZeroUsize },
    SourceEchoNotObject,
    SourceEchoMissingSource,
    SourceEchoMissingTranslation,
    SourceEchoDuplicateSource,
    SourceEchoDuplicateTranslation,
    SourceEchoUnexpectedField,
    SourceNotArray,
    SourceNonStringItem { item: NonZeroUsize },
}

impl GenericResponseValueProblem {
    const fn summary_code(self) -> &'static str {
        match self {
            Self::TranslationNotArray => "response_translation_not_array",
            Self::TranslationNonStringItem { .. } => "response_translation_item_not_string",
            Self::SourceEchoNotObject
            | Self::SourceEchoMissingSource
            | Self::SourceEchoMissingTranslation
            | Self::SourceEchoDuplicateSource
            | Self::SourceEchoDuplicateTranslation
            | Self::SourceEchoUnexpectedField
            | Self::SourceNotArray => "response_echo_shape_invalid",
            Self::SourceNonStringItem { .. } => "response_echo_source_item_not_string",
        }
    }

    const fn code_suffix(self) -> &'static str {
        match self {
            Self::TranslationNotArray => "translation_not_array",
            Self::TranslationNonStringItem { .. } => "translation_non_string_item",
            Self::SourceEchoNotObject => "source_echo_not_object",
            Self::SourceEchoMissingSource => "source_echo_missing_source",
            Self::SourceEchoMissingTranslation => "source_echo_missing_translation",
            Self::SourceEchoDuplicateSource => "source_echo_duplicate_source",
            Self::SourceEchoDuplicateTranslation => "source_echo_duplicate_translation",
            Self::SourceEchoUnexpectedField => "source_echo_unexpected_field",
            Self::SourceNotArray => "source_not_array",
            Self::SourceNonStringItem { .. } => "source_non_string_item",
        }
    }

    fn facts(self) -> Vec<(&'static str, String)> {
        match self {
            Self::TranslationNonStringItem { item } | Self::SourceNonStringItem { item } => {
                vec![("item", item.to_string())]
            }
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericResponseTextProblem {
    Blank,
    CarriageReturn,
    LineFeed,
    Nul,
    ByteOrderMark,
}

/// 已保存合法结果、但需要后续质量审核的非阻塞事实。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericResponseReviewFinding {
    SourceResidual,
    NonStopFinish,
}

impl GenericResponseReviewFinding {
    const fn code_suffix(self) -> &'static str {
        match self {
            Self::SourceResidual => "source_residual",
            Self::NonStopFinish => "non_stop_finish",
        }
    }
}

impl GenericResponseTextProblem {
    const fn summary_code(self) -> &'static str {
        match self {
            Self::Blank => "response_translation_blank",
            Self::CarriageReturn | Self::LineFeed | Self::Nul | Self::ByteOrderMark => {
                "response_translation_text_invalid"
            }
        }
    }

    const fn code_suffix(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::CarriageReturn => "carriage_return",
            Self::LineFeed => "line_feed",
            Self::Nul => "nul",
            Self::ByteOrderMark => "byte_order_mark",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericRepairApplicationProblem {
    InvalidNaturalSegment {
        segment_index: usize,
    },
    DuplicatePosition {
        segment_index: usize,
        byte_offset: usize,
    },
    InvalidCharacterBoundary {
        segment_index: usize,
        byte_offset: usize,
    },
    MissingCharacter {
        segment_index: usize,
        byte_offset: usize,
    },
    UnexpectedCharacter {
        segment_index: usize,
        byte_offset: usize,
    },
}

impl GenericRepairApplicationProblem {
    fn facts(self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidNaturalSegment { segment_index } => {
                vec![("segment_index", segment_index.to_string())]
            }
            Self::DuplicatePosition {
                segment_index,
                byte_offset,
            }
            | Self::InvalidCharacterBoundary {
                segment_index,
                byte_offset,
            }
            | Self::MissingCharacter {
                segment_index,
                byte_offset,
            }
            | Self::UnexpectedCharacter {
                segment_index,
                byte_offset,
            } => vec![
                ("segment_index", segment_index.to_string()),
                ("byte_offset", byte_offset.to_string()),
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericResponseDestinationProblem {
    MissingPlanningFact,
    InvalidPlaceholderSnapshot {
        category: GenericJsonErrorCategory,
        line: usize,
        column: usize,
    },
    PlaceholderCompilation {
        problem: PlaceholderCompilationProblem,
    },
    PlaceholderProtection {
        problem: PlaceholderIssue,
    },
    PlaceholderRestoreProjection {
        problem: GenericLanguageProjectionProblem,
    },
    PlaceholderRestoreMultiset {
        problem: GenericPlaceholderMultisetProblem,
    },
    PlaceholderBindingMismatch,
    LanguageProjection {
        problem: GenericLanguageProjectionProblem,
    },
    LanguageAnalysisMismatch,
    RepairPlanningMismatch,
    RepairApplication {
        problem: GenericRepairApplicationProblem,
    },
    PlaceholderBoundaryAdded,
    PlaceholderBoundaryRemoved,
    ReservedToken,
    InvalidTranslation {
        problem: GenericResponseTextProblem,
    },
}

impl GenericResponseDestinationProblem {
    const fn summary_code(&self) -> &'static str {
        match self {
            Self::MissingPlanningFact => "internal_invariant",
            Self::InvalidPlaceholderSnapshot { .. } => "response_placeholder_snapshot_invalid",
            Self::PlaceholderCompilation { problem } => match problem {
                PlaceholderCompilationProblem::WorkerStart { .. } => "worker_spawn_failed",
                _ => "invalid_value",
            },
            Self::PlaceholderProtection { problem } => problem.summary_code(),
            Self::PlaceholderRestoreProjection { problem }
            | Self::LanguageProjection { problem } => problem.response_summary_code(),
            Self::PlaceholderRestoreMultiset { problem } => match problem {
                GenericPlaceholderMultisetProblem::Mismatch => {
                    "response_placeholder_identity_or_count_mismatch"
                }
                GenericPlaceholderMultisetProblem::Unexpected => "response_placeholder_unexpected",
                GenericPlaceholderMultisetProblem::OrderMismatch => {
                    "response_placeholder_order_mismatch"
                }
                GenericPlaceholderMultisetProblem::WrapperTopologyChanged => {
                    "response_placeholder_boundary_mismatch"
                }
            },
            Self::PlaceholderBindingMismatch => "response_placeholder_binding_mismatch",
            Self::LanguageAnalysisMismatch => "internal_invariant",
            Self::RepairPlanningMismatch => "internal_invariant",
            Self::RepairApplication { .. } => "internal_invariant",
            Self::PlaceholderBoundaryAdded | Self::PlaceholderBoundaryRemoved => {
                "response_placeholder_boundary_mismatch"
            }
            Self::ReservedToken => "response_placeholder_reserved_token",
            Self::InvalidTranslation { problem } => problem.summary_code(),
        }
    }

    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::MissingPlanningFact => "missing_planning_fact",
            Self::InvalidPlaceholderSnapshot { .. } => "invalid_placeholder_snapshot",
            Self::PlaceholderCompilation { .. } => "placeholder_compilation",
            Self::PlaceholderProtection { .. } => "placeholder_protection",
            Self::PlaceholderRestoreProjection { .. } => "placeholder_restore_projection",
            Self::PlaceholderRestoreMultiset { .. } => "placeholder_restore_multiset",
            Self::PlaceholderBindingMismatch => "placeholder_binding_mismatch",
            Self::LanguageProjection { .. } => "language_projection",
            Self::LanguageAnalysisMismatch => "language_analysis_mismatch",
            Self::RepairPlanningMismatch => "repair_planning_mismatch",
            Self::RepairApplication { .. } => "repair_application",
            Self::PlaceholderBoundaryAdded => "placeholder_boundary_added",
            Self::PlaceholderBoundaryRemoved => "placeholder_boundary_removed",
            Self::ReservedToken => "reserved_token",
            Self::InvalidTranslation { problem } => problem.code_suffix(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidPlaceholderSnapshot {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("line", line.to_string()),
                ("column", column.to_string()),
            ],
            Self::PlaceholderCompilation { problem } => problem.facts(),
            Self::PlaceholderProtection { problem } => problem.facts(),
            Self::PlaceholderRestoreProjection { problem }
            | Self::LanguageProjection { problem } => problem.facts(),
            Self::RepairApplication { problem } => problem.facts(),
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericTaskResponseProblem {
    InvalidId {
        item_index: usize,
    },
    UnexpectedId {
        output_id: u64,
    },
    DuplicateId {
        output_id: u64,
    },
    MissingId {
        output_id: u64,
    },
    InvalidValue {
        output_id: u64,
        problem: GenericResponseValueProblem,
    },
    InvalidTranslation {
        output_id: u64,
        problem: GenericResponseTextProblem,
    },
    InvalidDestination {
        output_id: u64,
        destination: GenericUnitLocator,
        problem: GenericResponseDestinationProblem,
    },
    InvalidJson {
        category: GenericTaskResponseJsonCategory,
        line: NonZeroUsize,
        column: NonZeroUsize,
    },
    ThinkingEmpty {
        line: NonZeroUsize,
        column: NonZeroUsize,
    },
    ResponseReview {
        finding: GenericResponseReviewFinding,
    },
    DestinationReview {
        output_id: u64,
        destination: GenericUnitLocator,
        finding: GenericResponseReviewFinding,
    },
    CommitConflict {
        count: u64,
    },
}

impl GenericTaskResponseProblem {
    const fn output_id(&self) -> Option<u64> {
        match self {
            Self::UnexpectedId { output_id }
            | Self::DuplicateId { output_id }
            | Self::MissingId { output_id }
            | Self::InvalidValue { output_id, .. }
            | Self::InvalidTranslation { output_id, .. }
            | Self::InvalidDestination { output_id, .. }
            | Self::DestinationReview { output_id, .. } => Some(*output_id),
            Self::InvalidId { .. }
            | Self::InvalidJson { .. }
            | Self::ThinkingEmpty { .. }
            | Self::ResponseReview { .. }
            | Self::CommitConflict { .. } => None,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "generic.translation.response.invalid_id",
            Self::UnexpectedId { .. } => "generic.translation.response.unexpected_id",
            Self::DuplicateId { .. } => "generic.translation.response.duplicate_id",
            Self::MissingId { .. } => "generic.translation.response.missing_id",
            Self::InvalidValue { problem, .. } => match problem {
                GenericResponseValueProblem::TranslationNotArray => {
                    "generic.translation.response.value.translation_not_array"
                }
                GenericResponseValueProblem::TranslationNonStringItem { .. } => {
                    "generic.translation.response.value.translation_non_string_item"
                }
                GenericResponseValueProblem::SourceEchoNotObject => {
                    "generic.translation.response.value.source_echo_not_object"
                }
                GenericResponseValueProblem::SourceEchoMissingSource => {
                    "generic.translation.response.value.source_echo_missing_source"
                }
                GenericResponseValueProblem::SourceEchoMissingTranslation => {
                    "generic.translation.response.value.source_echo_missing_translation"
                }
                GenericResponseValueProblem::SourceEchoDuplicateSource => {
                    "generic.translation.response.value.source_echo_duplicate_source"
                }
                GenericResponseValueProblem::SourceEchoDuplicateTranslation => {
                    "generic.translation.response.value.source_echo_duplicate_translation"
                }
                GenericResponseValueProblem::SourceEchoUnexpectedField => {
                    "generic.translation.response.value.source_echo_unexpected_field"
                }
                GenericResponseValueProblem::SourceNotArray => {
                    "generic.translation.response.value.source_not_array"
                }
                GenericResponseValueProblem::SourceNonStringItem { .. } => {
                    "generic.translation.response.value.source_non_string_item"
                }
            },
            Self::InvalidTranslation { problem, .. } => match problem {
                GenericResponseTextProblem::Blank => {
                    "generic.translation.response.translation.blank"
                }
                GenericResponseTextProblem::CarriageReturn => {
                    "generic.translation.response.translation.carriage_return"
                }
                GenericResponseTextProblem::LineFeed => {
                    "generic.translation.response.translation.line_feed"
                }
                GenericResponseTextProblem::Nul => "generic.translation.response.translation.nul",
                GenericResponseTextProblem::ByteOrderMark => {
                    "generic.translation.response.translation.byte_order_mark"
                }
            },
            Self::InvalidDestination { problem, .. } => match problem {
                GenericResponseDestinationProblem::MissingPlanningFact => {
                    "generic.translation.response.destination.missing_planning_fact"
                }
                GenericResponseDestinationProblem::InvalidPlaceholderSnapshot { .. } => {
                    "generic.translation.response.destination.invalid_placeholder_snapshot"
                }
                GenericResponseDestinationProblem::PlaceholderCompilation { .. } => {
                    "generic.translation.response.destination.placeholder_compilation"
                }
                GenericResponseDestinationProblem::PlaceholderProtection { .. } => {
                    "generic.translation.response.destination.placeholder_protection"
                }
                GenericResponseDestinationProblem::PlaceholderRestoreProjection { .. } => {
                    "generic.translation.response.destination.placeholder_restore_projection"
                }
                GenericResponseDestinationProblem::PlaceholderRestoreMultiset { .. } => {
                    "generic.translation.response.destination.placeholder_restore_multiset"
                }
                GenericResponseDestinationProblem::PlaceholderBindingMismatch => {
                    "generic.translation.response.destination.placeholder_binding_mismatch"
                }
                GenericResponseDestinationProblem::LanguageProjection { .. } => {
                    "generic.translation.response.destination.language_projection"
                }
                GenericResponseDestinationProblem::LanguageAnalysisMismatch => {
                    "generic.translation.response.destination.language_analysis_mismatch"
                }
                GenericResponseDestinationProblem::RepairPlanningMismatch => {
                    "generic.translation.response.destination.repair_planning_mismatch"
                }
                GenericResponseDestinationProblem::RepairApplication { .. } => {
                    "generic.translation.response.destination.repair_application"
                }
                GenericResponseDestinationProblem::PlaceholderBoundaryAdded => {
                    "generic.translation.response.destination.placeholder_boundary_added"
                }
                GenericResponseDestinationProblem::PlaceholderBoundaryRemoved => {
                    "generic.translation.response.destination.placeholder_boundary_removed"
                }
                GenericResponseDestinationProblem::ReservedToken => {
                    "generic.translation.response.destination.reserved_token"
                }
                GenericResponseDestinationProblem::InvalidTranslation { problem } => {
                    match problem {
                        GenericResponseTextProblem::Blank => {
                            "generic.translation.response.destination.blank"
                        }
                        GenericResponseTextProblem::CarriageReturn => {
                            "generic.translation.response.destination.carriage_return"
                        }
                        GenericResponseTextProblem::LineFeed => {
                            "generic.translation.response.destination.line_feed"
                        }
                        GenericResponseTextProblem::Nul => {
                            "generic.translation.response.destination.nul"
                        }
                        GenericResponseTextProblem::ByteOrderMark => {
                            "generic.translation.response.destination.byte_order_mark"
                        }
                    }
                }
            },
            Self::InvalidJson {
                category: GenericTaskResponseJsonCategory::Shape,
                ..
            } => "generic.translation.response.invalid_shape",
            Self::InvalidJson { .. } => "generic.translation.response.invalid_json",
            Self::ThinkingEmpty { .. } => "generic.translation.response.thinking_empty",
            Self::ResponseReview { finding } => match finding {
                GenericResponseReviewFinding::NonStopFinish => {
                    "generic.translation.review.non_stop_finish"
                }
                GenericResponseReviewFinding::SourceResidual => {
                    "generic.translation.review.source_residual"
                }
            },
            Self::DestinationReview { finding, .. } => match finding {
                GenericResponseReviewFinding::SourceResidual => {
                    "generic.translation.review.destination.source_residual"
                }
                GenericResponseReviewFinding::NonStopFinish => {
                    "generic.translation.review.destination.non_stop_finish"
                }
            },
            Self::CommitConflict { .. } => "generic.translation.commit_conflict",
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidJson {
                category: GenericTaskResponseJsonCategory::Shape,
                ..
            } => "response_shape_invalid",
            Self::InvalidJson { .. } => "response_json_invalid",
            Self::CommitConflict { .. } => "state_mismatch",
            Self::ResponseReview { finding } | Self::DestinationReview { finding, .. } => {
                match finding {
                    GenericResponseReviewFinding::SourceResidual => "response_source_residual",
                    GenericResponseReviewFinding::NonStopFinish => {
                        "response_finish_requires_review"
                    }
                }
            }
            Self::InvalidId { .. } => "response_id_invalid",
            Self::UnexpectedId { .. } => "response_id_unexpected",
            Self::DuplicateId { .. } => "response_id_duplicate",
            Self::MissingId { .. } => "response_id_missing",
            Self::InvalidValue { problem, .. } => problem.summary_code(),
            Self::InvalidTranslation { problem, .. } => problem.summary_code(),
            Self::InvalidDestination { problem, .. } => problem.summary_code(),
            Self::ThinkingEmpty { .. } => "response_thinking_empty",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidId { item_index } => vec![("item_index", item_index.to_string())],
            Self::ResponseReview { finding } => {
                vec![("review", finding.code_suffix().to_owned())]
            }
            Self::DestinationReview {
                output_id,
                destination,
                finding,
            } => {
                let mut facts = vec![
                    ("output_id", output_id.to_string()),
                    ("relative_path", destination.relative_path.to_string()),
                    ("review", finding.code_suffix().to_owned()),
                ];
                if let Some(group_id) = &destination.group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                if let Some(unit_id) = &destination.unit_id {
                    facts.push(("unit_id", unit_id.to_string()));
                }
                if let Some(role) = &destination.role {
                    facts.push(("role", role.to_string()));
                }
                if let Some(line) = destination.line {
                    facts.push(("line", line.to_string()));
                }
                if let Some(unit) = destination.unit {
                    facts.push(("unit", unit.to_string()));
                }
                facts
            }
            Self::UnexpectedId { output_id }
            | Self::DuplicateId { output_id }
            | Self::MissingId { output_id } => {
                vec![("output_id", output_id.to_string())]
            }
            Self::InvalidValue { output_id, problem } => {
                let mut facts = vec![("output_id", output_id.to_string())];
                facts.push(("value_problem", problem.code_suffix().to_owned()));
                facts.extend(problem.facts());
                facts
            }
            Self::InvalidTranslation { output_id, problem } => vec![
                ("output_id", output_id.to_string()),
                ("translation_problem", problem.code_suffix().to_owned()),
            ],
            Self::InvalidDestination {
                output_id,
                destination,
                problem,
            } => {
                let mut facts = vec![
                    ("output_id", output_id.to_string()),
                    ("relative_path", destination.relative_path.to_string()),
                    ("destination_problem", problem.code_suffix().to_owned()),
                ];
                if let Some(group_id) = &destination.group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                if let Some(unit_id) = &destination.unit_id {
                    facts.push(("unit_id", unit_id.to_string()));
                }
                if let Some(role) = &destination.role {
                    facts.push(("role", role.to_string()));
                }
                facts.extend(problem.facts());
                facts
            }
            Self::InvalidJson {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("line", line.to_string()),
                ("column", column.to_string()),
            ],
            Self::ThinkingEmpty { line, column } => {
                vec![("line", line.to_string()), ("column", column.to_string())]
            }
            Self::CommitConflict { count } => vec![("conflicted_units", count.to_string())],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericTaskUnavailableReason {
    RetryAfterExceedsMaximum,
    RecoverableRequestExhausted,
    RequestFailed,
}

impl GenericTaskUnavailableReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RetryAfterExceedsMaximum => "retry_after_exceeds_maximum",
            Self::RecoverableRequestExhausted => "recoverable_request_exhausted",
            Self::RequestFailed => "request_failed",
        }
    }
}

/// JSON 后端的稳定错误类别；不携带后端 Display 文本。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericJsonErrorCategory {
    Io,
    Syntax,
    Data,
    Eof,
    DuplicateObjectKey,
}

impl GenericJsonErrorCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "eof",
            Self::DuplicateObjectKey => "duplicate_object_key",
        }
    }
}

impl From<JsonErrorCategory> for GenericJsonErrorCategory {
    fn from(value: JsonErrorCategory) -> Self {
        match value {
            JsonErrorCategory::Io => Self::Io,
            JsonErrorCategory::Syntax => Self::Syntax,
            JsonErrorCategory::Data => Self::Data,
            JsonErrorCategory::Eof => Self::Eof,
            JsonErrorCategory::DuplicateObjectKey => Self::DuplicateObjectKey,
        }
    }
}

/// Generic JSONL 的物理位置。`line` 是一基行号，ordinal 是零基自然顺序。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericJsonlLocation {
    pub(crate) path: SafePath,
    pub(crate) line: NonZeroUsize,
    pub(crate) group_id: Option<SafeIdentifier>,
    pub(crate) unit_id: Option<SafeIdentifier>,
    pub(crate) group_ordinal: Option<usize>,
    pub(crate) unit_ordinal: Option<usize>,
}

impl GenericJsonlLocation {
    pub(crate) fn line(path: impl AsRef<std::path::Path>, line: NonZeroUsize) -> Self {
        Self {
            path: SafePath::new(path),
            line,
            group_id: None,
            unit_id: None,
            group_ordinal: None,
            unit_ordinal: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_group(
        mut self,
        group_id: impl AsRef<str>,
        group_ordinal: Option<usize>,
    ) -> Self {
        self.group_id = SafeIdentifier::new(group_id).ok();
        self.group_ordinal = group_ordinal;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_unit(
        mut self,
        unit_id: impl AsRef<str>,
        unit_ordinal: Option<usize>,
    ) -> Self {
        self.unit_id = SafeIdentifier::new(unit_id).ok();
        self.unit_ordinal = unit_ordinal;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericJsonlFieldProblem {
    UnknownField {
        field: SafeText,
    },
    MissingField {
        field: SafeText,
    },
    TypeMismatch {
        field: SafeText,
        expected: GenericJsonlValueKind,
    },
}

impl GenericJsonlFieldProblem {
    pub(crate) fn field(&self) -> &SafeText {
        match self {
            Self::UnknownField { field }
            | Self::MissingField { field }
            | Self::TypeMismatch { field, .. } => field,
        }
    }

    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::UnknownField { .. } => "unknown_field",
            Self::MissingField { .. } => "missing_field",
            Self::TypeMismatch { .. } => "type_mismatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericJsonlValueKind {
    String,
    Array,
    Object,
}

impl GenericJsonlValueKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericProblem {
    Cancelled,
    ProjectCancelled,
    InvalidUtf8 {
        path: SafePath,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    BlankJsonlLine {
        location: GenericJsonlLocation,
    },
    InvalidJson {
        location: GenericJsonlLocation,
        json_line: usize,
        json_column: usize,
        category: GenericJsonErrorCategory,
        field_problem: Option<GenericJsonlFieldProblem>,
    },
    BlankField {
        location: Option<GenericJsonlLocation>,
        field: SafeIdentifier,
    },
    InvalidText {
        location: Option<GenericJsonlLocation>,
        violation: GenericTextViolation,
    },
    EmptyUnits {
        location: Option<GenericJsonlLocation>,
        group_id: Option<SafeIdentifier>,
    },
    DuplicateUnitId {
        location: Option<GenericJsonlLocation>,
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
        first_ordinal: usize,
        second_ordinal: usize,
    },
    DuplicateGroupId {
        group_id: Option<SafeIdentifier>,
        first: GenericJsonlLocation,
        second: GenericJsonlLocation,
    },
    SerializeJson {
        category: GenericJsonErrorCategory,
        line: usize,
        column: usize,
    },
    ProjectIdentityMismatch {
        expected: SafeIdentifier,
        observed: SafeIdentifier,
    },
    SourceWriteBackOverlap {
        source_root: SafePath,
        write_back_root: SafePath,
    },
    MissingInitialField {
        field: SafeIdentifier,
    },
    SameSourceAndTargetLanguage {
        language: SafeIdentifier,
    },
    InvalidProjectDatabase {
        problem: GenericProjectDatabaseProblem,
    },
    InvalidLanguage {
        violation: GenericLanguageViolation,
    },
    InvalidTranslation {
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
        problem: GenericProjectTranslationProblem,
    },
    InvalidResource {
        resource: GenericResourceKind,
        path: Option<SafePath>,
    },
    NonCanonicalResourceSnapshot {
        resource: GenericResourceKind,
    },
    UnexpectedResourceState {
        resource: GenericResourceKind,
    },
    WorkerStart {
        operation: SafeIdentifier,
        failure: IoFailure,
    },
    WriteBackSourceChanged,
    WriteBackSnapshotMismatch {
        problem: GenericWriteBackSnapshotProblem,
    },
    WriteBackUnit {
        unit: GenericUnitLocator,
        problem: GenericWriteBackUnitProblem,
    },
    WriteBackMaterializedMismatch {
        path: SafePath,
        bytes_changed: bool,
        structure_changed: bool,
    },
    WriteBackLayoutRules {
        path: Option<SafePath>,
        rule_number: Option<usize>,
        project_snapshot: bool,
    },
    InputChangedDuringExtract,
    ExtractRequired,
    TranslationSnapshotChanged,
    DuplicateTranslationWrite {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    DuplicateTranslationClear {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    MissingProfileId,
    BlankProfileId,
    UnitNotFound {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    TaskResponse {
        task_ordinal: u64,
        total_tasks: u64,
        problem: GenericTaskResponseProblem,
    },
    TaskUnavailable {
        task_ordinal: u64,
        total_tasks: u64,
        reason: GenericTaskUnavailableReason,
    },
    TranslationPreparation {
        unit: Option<GenericUnitLocator>,
        problem: GenericTranslationPreparationProblem,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericTranslationPreparationProblem {
    InvalidPlaceholderSnapshot {
        category: GenericJsonErrorCategory,
        line: usize,
        column: usize,
    },
    PlaceholderRestoreProjection {
        problem: GenericLanguageProjectionProblem,
    },
    PlaceholderRestoreMultiset {
        problem: GenericPlaceholderMultisetProblem,
    },
    ManualTranslationPlaceholderMismatch,
    UnexpectedUnlocatedPlaceholderProtection,
    LanguageProjection {
        problem: GenericLanguageProjectionProblem,
    },
    MissingCurrentContext {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    MissingPlanningFact {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    UnknownPlanningFact {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
    DuplicatePlanningFact {
        group_id: SafeIdentifier,
        unit_id: SafeIdentifier,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum GenericLanguageProjectionProblem {
    TokenIndexConstruction,
    EmptyToken,
    MissingToken,
    RepeatedToken,
    OverlappingToken,
    ChangedTokenOrder { position: usize },
    ChangedSegmentCount { expected: usize, actual: usize },
    ChangedSegmentKind { segment_index: usize },
    MissingOrderedToken { segment_index: usize },
    UnusedOrderedToken,
}

impl GenericLanguageProjectionProblem {
    const fn response_summary_code(self) -> &'static str {
        match self {
            Self::TokenIndexConstruction
            | Self::EmptyToken
            | Self::RepeatedToken
            | Self::OverlappingToken => "response_control_token_invalid",
            Self::MissingToken => "response_placeholder_missing",
            Self::ChangedTokenOrder { .. } => "response_placeholder_order_mismatch",
            Self::ChangedSegmentCount { .. } => "response_text_segment_count_mismatch",
            Self::ChangedSegmentKind { .. } => "response_text_segment_shape_mismatch",
            Self::MissingOrderedToken { .. } => "response_placeholder_missing",
            Self::UnusedOrderedToken => "response_placeholder_unexpected",
        }
    }

    fn facts(self) -> Vec<(&'static str, String)> {
        match self {
            Self::ChangedTokenOrder { position } => vec![("position", position.to_string())],
            Self::ChangedSegmentCount { expected, actual } => vec![
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ],
            Self::ChangedSegmentKind { segment_index }
            | Self::MissingOrderedToken { segment_index } => {
                vec![("segment_index", segment_index.to_string())]
            }
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericPlaceholderMultisetProblem {
    Mismatch,
    Unexpected,
    OrderMismatch,
    WrapperTopologyChanged,
}

impl GenericTranslationPreparationProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlaceholderSnapshot { .. } => {
                "generic.translation.placeholder_snapshot.invalid_json"
            }
            Self::PlaceholderRestoreProjection { problem } => match problem {
                GenericLanguageProjectionProblem::TokenIndexConstruction => {
                    "generic.translation.placeholder_restore.token_index_construction"
                }
                GenericLanguageProjectionProblem::EmptyToken => {
                    "generic.translation.placeholder_restore.empty_token"
                }
                GenericLanguageProjectionProblem::MissingToken => {
                    "generic.translation.placeholder_restore.missing_token"
                }
                GenericLanguageProjectionProblem::RepeatedToken => {
                    "generic.translation.placeholder_restore.repeated_token"
                }
                GenericLanguageProjectionProblem::OverlappingToken => {
                    "generic.translation.placeholder_restore.overlapping_token"
                }
                GenericLanguageProjectionProblem::ChangedTokenOrder { .. } => {
                    "generic.translation.placeholder_restore.changed_token_order"
                }
                GenericLanguageProjectionProblem::ChangedSegmentCount { .. } => {
                    "generic.translation.placeholder_restore.changed_segment_count"
                }
                GenericLanguageProjectionProblem::ChangedSegmentKind { .. } => {
                    "generic.translation.placeholder_restore.changed_segment_kind"
                }
                GenericLanguageProjectionProblem::MissingOrderedToken { .. } => {
                    "generic.translation.placeholder_restore.missing_ordered_token"
                }
                GenericLanguageProjectionProblem::UnusedOrderedToken => {
                    "generic.translation.placeholder_restore.unused_ordered_token"
                }
            },
            Self::PlaceholderRestoreMultiset { problem } => match problem {
                GenericPlaceholderMultisetProblem::Mismatch => {
                    "generic.translation.placeholder_restore.multiset_mismatch"
                }
                GenericPlaceholderMultisetProblem::Unexpected => {
                    "generic.translation.placeholder_restore.unexpected_token"
                }
                GenericPlaceholderMultisetProblem::OrderMismatch => {
                    "generic.translation.placeholder_restore.order_mismatch"
                }
                GenericPlaceholderMultisetProblem::WrapperTopologyChanged => {
                    "generic.translation.placeholder_restore.wrapper_topology_changed"
                }
            },
            Self::ManualTranslationPlaceholderMismatch => {
                "generic.translation.manual_placeholder_mismatch"
            }
            Self::UnexpectedUnlocatedPlaceholderProtection => {
                "generic.translation.placeholder.unlocated_failure"
            }
            Self::LanguageProjection { problem } => match problem {
                GenericLanguageProjectionProblem::TokenIndexConstruction => {
                    "generic.translation.language_projection.token_index_construction"
                }
                GenericLanguageProjectionProblem::EmptyToken => {
                    "generic.translation.language_projection.empty_token"
                }
                GenericLanguageProjectionProblem::MissingToken => {
                    "generic.translation.language_projection.missing_token"
                }
                GenericLanguageProjectionProblem::RepeatedToken => {
                    "generic.translation.language_projection.repeated_token"
                }
                GenericLanguageProjectionProblem::OverlappingToken => {
                    "generic.translation.language_projection.overlapping_token"
                }
                GenericLanguageProjectionProblem::ChangedTokenOrder { .. } => {
                    "generic.translation.language_projection.changed_token_order"
                }
                GenericLanguageProjectionProblem::ChangedSegmentCount { .. } => {
                    "generic.translation.language_projection.changed_segment_count"
                }
                GenericLanguageProjectionProblem::ChangedSegmentKind { .. } => {
                    "generic.translation.language_projection.changed_segment_kind"
                }
                GenericLanguageProjectionProblem::MissingOrderedToken { .. } => {
                    "generic.translation.language_projection.missing_ordered_token"
                }
                GenericLanguageProjectionProblem::UnusedOrderedToken => {
                    "generic.translation.language_projection.unused_ordered_token"
                }
            },
            Self::MissingCurrentContext { .. } => {
                "generic.translation.planning.missing_current_context"
            }
            Self::MissingPlanningFact { .. } => "generic.translation.planning.missing_fact",
            Self::UnknownPlanningFact { .. } => "generic.translation.planning.unknown_fact",
            Self::DuplicatePlanningFact { .. } => "generic.translation.planning.duplicate_fact",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidPlaceholderSnapshot {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("line", line.to_string()),
                ("column", column.to_string()),
            ],
            Self::PlaceholderRestoreProjection { problem }
            | Self::LanguageProjection { problem } => problem.facts(),
            Self::MissingCurrentContext { group_id, unit_id }
            | Self::MissingPlanningFact { group_id, unit_id }
            | Self::UnknownPlanningFact { group_id, unit_id }
            | Self::DuplicatePlanningFact { group_id, unit_id } => vec![
                ("group_id", group_id.to_string()),
                ("unit_id", unit_id.to_string()),
            ],
            Self::PlaceholderRestoreMultiset { .. }
            | Self::ManualTranslationPlaceholderMismatch
            | Self::UnexpectedUnlocatedPlaceholderProtection => Vec::new(),
        }
    }
}

impl GenericProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "generic.jsonl.cancelled",
            Self::ProjectCancelled => "generic.project.cancelled",
            Self::InvalidUtf8 { .. } => "generic.jsonl.invalid_utf8",
            Self::BlankJsonlLine { .. } => "generic.jsonl.blank_line",
            Self::InvalidJson { .. } => "generic.jsonl.invalid_json",
            Self::BlankField { .. } => "generic.jsonl.blank_field",
            Self::InvalidText { .. } => "generic.jsonl.invalid_text",
            Self::EmptyUnits { .. } => "generic.jsonl.empty_units",
            Self::DuplicateUnitId { .. } => "generic.jsonl.duplicate_unit_id",
            Self::DuplicateGroupId { .. } => "generic.jsonl.duplicate_group_id",
            Self::SerializeJson { .. } => "generic.jsonl.serialize",
            Self::ProjectIdentityMismatch { .. } => "generic.project.identity_mismatch",
            Self::SourceWriteBackOverlap { .. } => "generic.project.path_overlap",
            Self::MissingInitialField { .. } => "generic.project.missing_initial_field",
            Self::SameSourceAndTargetLanguage { .. } => "generic.project.language_conflict",
            Self::InvalidProjectDatabase { problem } => problem.code(),
            Self::InvalidLanguage { .. } => "generic.project.invalid_language",
            Self::InvalidTranslation { problem, .. } => problem.code(),
            Self::InvalidResource { .. } => "generic.project.invalid_resource",
            Self::NonCanonicalResourceSnapshot { .. } => {
                "generic.project.resource.noncanonical_snapshot"
            }
            Self::UnexpectedResourceState { .. } => "generic.project.resource.unexpected_state",
            Self::WorkerStart { .. } => "generic.project.worker_start",
            Self::WriteBackSourceChanged => "generic.write_back.source_changed",
            Self::WriteBackSnapshotMismatch { problem } => problem.code(),
            Self::WriteBackUnit { problem, .. } => problem.code(),
            Self::WriteBackMaterializedMismatch { .. } => {
                "generic.write_back.materialized_mismatch"
            }
            Self::WriteBackLayoutRules { .. } => "generic.write_back.layout_rules.invalid",
            Self::InputChangedDuringExtract => "generic.project.input_changed_during_extract",
            Self::ExtractRequired => "generic.project.extract_required",
            Self::TranslationSnapshotChanged => "generic.project.translation_snapshot_changed",
            Self::DuplicateTranslationWrite { .. } => "generic.project.duplicate_translation_write",
            Self::DuplicateTranslationClear { .. } => "generic.project.duplicate_translation_clear",
            Self::MissingProfileId => "generic.project.missing_profile_id",
            Self::BlankProfileId => "generic.project.blank_profile_id",
            Self::UnitNotFound { .. } => "generic.project.unit_not_found",
            Self::TaskResponse { problem, .. } => problem.code(),
            Self::TaskUnavailable { .. } => "generic.translation.task_unavailable",
            Self::TranslationPreparation { problem, .. } => problem.code(),
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Cancelled | Self::ProjectCancelled => DiagnosticResolution::Retry,
            Self::InputChangedDuringExtract | Self::TranslationSnapshotChanged => {
                DiagnosticResolution::Retry
            }
            Self::ExtractRequired | Self::ProjectIdentityMismatch { .. } => {
                DiagnosticResolution::CheckProjectState
            }
            Self::InvalidProjectDatabase { .. } => DiagnosticResolution::CheckProjectState,
            Self::InvalidTranslation {
                problem:
                    GenericProjectTranslationProblem::PlaceholderCompilation {
                        problem: PlaceholderCompilationProblem::WorkerStart { .. },
                    }
                    | GenericProjectTranslationProblem::PlaceholderProtection {
                        problem: PlaceholderIssue::WorkerStart { .. },
                    },
                ..
            } => DiagnosticResolution::Retry,
            Self::SourceWriteBackOverlap { .. }
            | Self::MissingInitialField { .. }
            | Self::InvalidUtf8 { .. }
            | Self::BlankJsonlLine { .. }
            | Self::InvalidJson { .. }
            | Self::BlankField { .. }
            | Self::InvalidText { .. }
            | Self::EmptyUnits { .. }
            | Self::DuplicateUnitId { .. }
            | Self::DuplicateGroupId { .. }
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InvalidLanguage { .. }
            | Self::InvalidResource { .. }
            | Self::NonCanonicalResourceSnapshot { .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::UnitNotFound { .. } => DiagnosticResolution::FixInput,
            Self::InvalidTranslation { .. } => DiagnosticResolution::FixInput,
            Self::UnexpectedResourceState { .. } => DiagnosticResolution::ReportBug,
            Self::MissingProfileId | Self::BlankProfileId => DiagnosticResolution::FixConfiguration,
            Self::SerializeJson { .. } => DiagnosticResolution::ReportBug,
            Self::TaskResponse { problem, .. } => match problem {
                GenericTaskResponseProblem::ResponseReview { .. }
                | GenericTaskResponseProblem::DestinationReview { .. } => {
                    DiagnosticResolution::ReviewTranslation
                }
                _ => DiagnosticResolution::Retry,
            },
            Self::TaskUnavailable { .. } => DiagnosticResolution::Retry,
            Self::TranslationPreparation { .. } => DiagnosticResolution::ReportBug,
            Self::WorkerStart { .. } => DiagnosticResolution::ReportBug,
            Self::WriteBackSourceChanged => DiagnosticResolution::FixInput,
            Self::WriteBackUnit { problem, .. } => problem.resolution(),
            Self::WriteBackSnapshotMismatch { .. } | Self::WriteBackMaterializedMismatch { .. } => {
                DiagnosticResolution::CheckProjectState
            }
            Self::WriteBackLayoutRules {
                project_snapshot: true,
                ..
            } => DiagnosticResolution::CheckProjectState,
            Self::WriteBackLayoutRules {
                project_snapshot: false,
                ..
            } => DiagnosticResolution::FixInput,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::Cancelled | Self::ProjectCancelled => "lock_cancelled",
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::BlankJsonlLine { .. }
            | Self::BlankField { .. }
            | Self::EmptyUnits { .. }
            | Self::MissingInitialField { .. }
            | Self::MissingProfileId
            | Self::BlankProfileId => "missing_required_value",
            Self::InvalidJson { .. } | Self::SerializeJson { .. } => "invalid_syntax",
            Self::DuplicateUnitId { .. }
            | Self::DuplicateGroupId { .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. } => "duplicate_identifier",
            Self::ProjectIdentityMismatch { .. }
            | Self::InputChangedDuringExtract
            | Self::TranslationSnapshotChanged
            | Self::InvalidProjectDatabase { .. } => "state_mismatch",
            Self::SourceWriteBackOverlap { .. } | Self::SameSourceAndTargetLanguage { .. } => {
                "conflicting_values"
            }
            Self::ExtractRequired => "generic_extract_required",
            Self::InvalidTranslation {
                problem:
                    GenericProjectTranslationProblem::PlaceholderCompilation {
                        problem: PlaceholderCompilationProblem::WorkerStart { .. },
                    }
                    | GenericProjectTranslationProblem::PlaceholderProtection {
                        problem: PlaceholderIssue::WorkerStart { .. },
                    },
                ..
            } => "worker_spawn_failed",
            Self::InvalidLanguage { .. } | Self::InvalidResource { .. } => "invalid_value",
            Self::InvalidTranslation { .. } => "invalid_value",
            Self::NonCanonicalResourceSnapshot { .. } => "state_mismatch",
            Self::UnexpectedResourceState { .. } => "internal_invariant",
            Self::WorkerStart { .. } => "worker_spawn_failed",
            Self::WriteBackSourceChanged | Self::WriteBackSnapshotMismatch { .. } => {
                "state_mismatch"
            }
            Self::WriteBackUnit { problem, .. } => problem.summary_code(),
            Self::WriteBackMaterializedMismatch { .. } => "write_back_candidate_invalid",
            Self::WriteBackLayoutRules {
                project_snapshot: true,
                ..
            } => "state_mismatch",
            Self::WriteBackLayoutRules {
                project_snapshot: false,
                ..
            } => "invalid_value",
            Self::UnitNotFound { .. } => "not_found",
            Self::InvalidText { .. } => "invalid_value",
            Self::TaskResponse { problem, .. } => problem.summary_code(),
            Self::TaskUnavailable { .. } => "external_service_unavailable",
            Self::TranslationPreparation { problem, .. } => match problem {
                GenericTranslationPreparationProblem::InvalidPlaceholderSnapshot { .. }
                | GenericTranslationPreparationProblem::ManualTranslationPlaceholderMismatch => {
                    "invalid_value"
                }
                GenericTranslationPreparationProblem::PlaceholderRestoreProjection { .. }
                | GenericTranslationPreparationProblem::PlaceholderRestoreMultiset { .. }
                | GenericTranslationPreparationProblem::LanguageProjection { .. }
                | GenericTranslationPreparationProblem::MissingCurrentContext { .. }
                | GenericTranslationPreparationProblem::MissingPlanningFact { .. }
                | GenericTranslationPreparationProblem::UnknownPlanningFact { .. }
                | GenericTranslationPreparationProblem::DuplicatePlanningFact { .. }
                | GenericTranslationPreparationProblem::UnexpectedUnlocatedPlaceholderProtection => {
                    "internal_invariant"
                }
            },
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::Cancelled => "generic_jsonl".to_owned(),
            Self::ProjectCancelled => "generic_project".to_owned(),
            Self::InvalidUtf8 { path, .. } => path.to_string(),
            Self::BlankJsonlLine { location } | Self::InvalidJson { location, .. } => {
                format!(
                    "{}:line{}",
                    readable_generic_path(&location.path),
                    location.line
                )
            }
            Self::BlankField { location, field } => location
                .as_ref()
                .map_or_else(|| field.to_string(), |location| location.path.to_string()),
            Self::InvalidText { location, .. } => location.as_ref().map_or_else(
                || "unit.text".to_owned(),
                |location| location.path.to_string(),
            ),
            Self::EmptyUnits { location, group_id } => location.as_ref().map_or_else(
                || {
                    group_id
                        .as_ref()
                        .map_or_else(|| "units".to_owned(), ToString::to_string)
                },
                |location| location.path.to_string(),
            ),
            Self::DuplicateUnitId {
                location,
                group_id,
                unit_id,
                ..
            } => location.as_ref().map_or_else(
                || match (group_id, unit_id) {
                    (Some(group_id), Some(unit_id)) => format!("{group_id}/{unit_id}"),
                    _ => "unit.id".to_owned(),
                },
                |location| location.path.to_string(),
            ),
            Self::DuplicateGroupId {
                group_id: Some(group_id),
                ..
            } => group_id.to_string(),
            Self::DuplicateGroupId { group_id: None, .. } => "group.id".to_owned(),
            Self::SerializeJson { .. } => "generic_jsonl".to_owned(),
            Self::ProjectIdentityMismatch { observed, .. } => observed.to_string(),
            Self::SourceWriteBackOverlap { source_root, .. } => source_root.to_string(),
            Self::MissingInitialField { field } => field.to_string(),
            Self::SameSourceAndTargetLanguage { language } => language.to_string(),
            Self::InvalidProjectDatabase { problem } => problem.subject(),
            Self::InvalidLanguage { .. } => "generic_language".to_owned(),
            Self::InvalidTranslation {
                group_id, unit_id, ..
            } => match (group_id, unit_id) {
                (Some(group_id), Some(unit_id)) => format!("{group_id}/{unit_id}"),
                _ => "generic_translation".to_owned(),
            },
            Self::InvalidResource { resource, path } => path
                .as_ref()
                .map_or_else(|| resource.as_str().to_owned(), ToString::to_string),
            Self::NonCanonicalResourceSnapshot { resource }
            | Self::UnexpectedResourceState { resource } => resource.as_str().to_owned(),
            Self::WorkerStart { operation, .. } => operation.to_string(),
            Self::WriteBackSourceChanged => "generic_input".to_owned(),
            Self::WriteBackSnapshotMismatch { problem } => problem.subject(),
            Self::WriteBackUnit { unit, .. } => generic_unit_subject(unit),
            Self::WriteBackMaterializedMismatch { path, .. } => path.to_string(),
            Self::WriteBackLayoutRules { path, .. } => path
                .as_ref()
                .map_or_else(|| "write_back_layout_rules".to_owned(), ToString::to_string),
            Self::InputChangedDuringExtract => "generic_input".to_owned(),
            Self::ExtractRequired => "generic_extract".to_owned(),
            Self::TranslationSnapshotChanged => "generic_translation_snapshot".to_owned(),
            Self::DuplicateTranslationWrite { group_id, unit_id }
            | Self::DuplicateTranslationClear { group_id, unit_id }
            | Self::UnitNotFound { group_id, unit_id } => format!("{group_id}/{unit_id}"),
            Self::MissingProfileId | Self::BlankProfileId => "profile_id".to_owned(),
            Self::TaskResponse {
                task_ordinal,
                problem,
                ..
            } => {
                let mut subject = match problem {
                    GenericTaskResponseProblem::InvalidDestination { destination, .. }
                    | GenericTaskResponseProblem::DestinationReview { destination, .. } => {
                        format!(
                            "{} (generic_translation_task_{task_ordinal})",
                            destination.readable_id()
                        )
                    }
                    _ => format!("generic_translation_task_{task_ordinal}"),
                };
                if let Some(output_id) = problem.output_id() {
                    subject.push_str(&format!("; output_id={output_id}"));
                }
                subject
            }
            Self::TaskUnavailable { task_ordinal, .. } => {
                format!("generic_translation_task_{task_ordinal}")
            }
            Self::TranslationPreparation { unit, .. } => unit.as_ref().map_or_else(
                || "generic_translation_preparation".to_owned(),
                |unit| {
                    let mut subject = unit.relative_path.to_string();
                    if let Some(group_id) = &unit.group_id {
                        subject.push(':');
                        subject.push_str(group_id.as_str());
                    }
                    if let Some(unit_id) = &unit.unit_id {
                        subject.push(':');
                        subject.push_str(unit_id.as_str());
                    }
                    subject
                },
            ),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Cancelled | Self::ProjectCancelled => Vec::new(),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => vec![
                ("path", path.to_string()),
                ("valid_up_to", valid_up_to.to_string()),
                ("error_len", optional_number(*error_len)),
            ],
            Self::BlankJsonlLine { location } => location_facts(location),
            Self::InvalidJson {
                location,
                json_line,
                json_column,
                category,
                field_problem,
            } => {
                let mut facts = location_facts(location);
                facts.extend([
                    ("json_line", json_line.to_string()),
                    ("json_column", json_column.to_string()),
                    ("json_category", category.as_str().to_owned()),
                ]);
                if let Some(problem) = field_problem {
                    facts.push(("field", problem.field().to_string()));
                    facts.push(("field_failure", problem.as_str().to_owned()));
                    if let GenericJsonlFieldProblem::TypeMismatch { expected, .. } = problem {
                        facts.push(("expected", expected.as_str().to_owned()));
                    }
                }
                facts
            }
            Self::BlankField { location, field } => {
                let mut facts = location.as_ref().map_or_else(Vec::new, location_facts);
                facts.push(("field", field.to_string()));
                facts
            }
            Self::InvalidText {
                location,
                violation,
            } => {
                let mut facts = location.as_ref().map_or_else(Vec::new, location_facts);
                facts.push((
                    "violation",
                    match violation {
                        GenericTextViolation::CarriageReturn => "carriage_return",
                        GenericTextViolation::Nul => "nul",
                    }
                    .to_owned(),
                ));
                facts
            }
            Self::EmptyUnits { location, group_id } => {
                let mut facts = location.as_ref().map_or_else(Vec::new, location_facts);
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                facts
            }
            Self::DuplicateUnitId {
                location,
                group_id,
                unit_id,
                first_ordinal,
                second_ordinal,
            } => {
                let mut facts = location.as_ref().map_or_else(Vec::new, location_facts);
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                if let Some(unit_id) = unit_id {
                    facts.push(("unit_id", unit_id.to_string()));
                }
                facts.extend([
                    ("first_ordinal", first_ordinal.to_string()),
                    ("second_ordinal", second_ordinal.to_string()),
                ]);
                facts
            }
            Self::DuplicateGroupId {
                group_id,
                first,
                second,
            } => {
                let mut facts = Vec::new();
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                facts.extend([
                    ("first_path", first.path.to_string()),
                    ("first_line", first.line.to_string()),
                    ("second_path", second.path.to_string()),
                    ("second_line", second.line.to_string()),
                ]);
                facts
            }
            Self::SerializeJson {
                category,
                line,
                column,
            } => vec![
                ("json_category", category.as_str().to_owned()),
                ("json_line", line.to_string()),
                ("json_column", column.to_string()),
            ],
            Self::ProjectIdentityMismatch { expected, observed } => vec![
                ("expected_project", expected.to_string()),
                ("observed_project", observed.to_string()),
            ],
            Self::SourceWriteBackOverlap {
                source_root,
                write_back_root,
            } => vec![
                ("source_root", source_root.to_string()),
                ("write_back_root", write_back_root.to_string()),
            ],
            Self::MissingInitialField { field } => vec![("field", field.to_string())],
            Self::SameSourceAndTargetLanguage { language } => {
                vec![("language", language.to_string())]
            }
            Self::InvalidProjectDatabase { problem } => problem.facts(),
            Self::InvalidTranslation {
                group_id,
                unit_id,
                problem,
            } => {
                let mut facts = Vec::new();
                if let Some(group_id) = group_id {
                    facts.push(("group_id", group_id.to_string()));
                }
                if let Some(unit_id) = unit_id {
                    facts.push(("unit_id", unit_id.to_string()));
                }
                facts.extend(problem.facts());
                facts
            }
            Self::InvalidLanguage { violation } => {
                vec![("violation", violation.as_str().to_owned())]
            }
            Self::InvalidResource { resource, path } => {
                let mut facts = vec![("resource", resource.as_str().to_owned())];
                if let Some(path) = path {
                    facts.push(("path", path.to_string()));
                }
                facts
            }
            Self::NonCanonicalResourceSnapshot { resource }
            | Self::UnexpectedResourceState { resource } => {
                vec![("resource", resource.as_str().to_owned())]
            }
            Self::WorkerStart { operation, failure } => {
                let mut facts = vec![("worker_operation", operation.to_string())];
                facts.extend(failure.facts());
                facts
            }
            Self::WriteBackSourceChanged => Vec::new(),
            Self::WriteBackSnapshotMismatch { problem } => problem.facts(),
            Self::WriteBackUnit { unit, problem } => {
                let mut facts = generic_unit_facts(unit);
                facts.extend(problem.facts());
                facts
            }
            Self::WriteBackMaterializedMismatch {
                path,
                bytes_changed,
                structure_changed,
            } => vec![
                ("path", path.to_string()),
                ("bytes_changed", bytes_changed.to_string()),
                ("structure_changed", structure_changed.to_string()),
            ],
            Self::WriteBackLayoutRules {
                path,
                rule_number,
                project_snapshot,
            } => {
                let mut facts = vec![("project_snapshot", project_snapshot.to_string())];
                if let Some(path) = path {
                    facts.push(("path", path.to_string()));
                }
                if let Some(rule_number) = rule_number {
                    facts.push(("rule_number", rule_number.to_string()));
                }
                facts
            }
            Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::MissingProfileId
            | Self::BlankProfileId => Vec::new(),
            Self::DuplicateTranslationWrite { group_id, unit_id }
            | Self::DuplicateTranslationClear { group_id, unit_id }
            | Self::UnitNotFound { group_id, unit_id } => vec![
                ("group_id", group_id.to_string()),
                ("unit_id", unit_id.to_string()),
            ],
            Self::TaskResponse {
                task_ordinal,
                total_tasks,
                problem,
            } => {
                let mut facts = vec![
                    ("task_ordinal", task_ordinal.to_string()),
                    ("total_tasks", total_tasks.to_string()),
                ];
                facts.extend(problem.facts());
                facts
            }
            Self::TaskUnavailable {
                task_ordinal,
                total_tasks,
                reason,
            } => vec![
                ("task_ordinal", task_ordinal.to_string()),
                ("total_tasks", total_tasks.to_string()),
                ("reason", reason.as_str().to_owned()),
            ],
            Self::TranslationPreparation { unit, problem } => {
                let mut facts = Vec::new();
                if let Some(unit) = unit {
                    facts.push(("relative_path", unit.relative_path.to_string()));
                    if let Some(group_id) = &unit.group_id {
                        facts.push(("group_id", group_id.to_string()));
                    }
                    if let Some(unit_id) = &unit.unit_id {
                        facts.push(("unit_id", unit_id.to_string()));
                    }
                    if let Some(role) = &unit.role {
                        facts.push(("role", role.to_string()));
                    }
                }
                facts.extend(problem.facts());
                facts
            }
        }
    }
}

/// Generic family 的外层 context 保证 stage 与 operation 都是封闭事实。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericIssue {
    stage: GenericDiagnosticStage,
    operation: GenericOperation,
    problem: GenericProblem,
}

impl GenericIssue {
    pub(crate) const fn problem(&self) -> &GenericProblem {
        &self.problem
    }

    const fn new(
        stage: GenericDiagnosticStage,
        operation: GenericOperation,
        problem: GenericProblem,
    ) -> Self {
        Self {
            stage,
            operation,
            problem,
        }
    }

    pub(crate) fn jsonl(stage: GenericDiagnosticStage, problem: GenericProblem) -> Self {
        let operation = expected_operation(&problem);
        assert!(
            matches!(
                operation,
                GenericOperation::ParseJsonl | GenericOperation::SerializeJsonl
            ),
            "Generic JSONL 诊断不能承载项目状态问题"
        );
        Self::new(stage, operation, problem)
    }

    pub(crate) fn project(stage: GenericDiagnosticStage, problem: GenericProblem) -> Self {
        let operation = expected_operation(&problem);
        assert!(
            !matches!(
                operation,
                GenericOperation::ParseJsonl | GenericOperation::SerializeJsonl
            ),
            "Generic 项目诊断不能承载 JSONL 格式问题"
        );
        Self::new(stage, operation, problem)
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        self.stage.diagnostic_stage()
    }

    pub(crate) fn code(&self) -> &'static str {
        self.problem.code()
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        self.problem.resolution()
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        self.problem.summary_code()
    }

    pub(crate) fn subject(&self) -> String {
        self.problem.subject()
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = self.problem.facts();
        facts.insert(0, ("operation", self.operation.as_str().to_owned()));
        facts
    }
}

impl<'de> Deserialize<'de> for GenericIssue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            stage: GenericDiagnosticStage,
            operation: GenericOperation,
            problem: GenericProblem,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.operation != expected_operation(&wire.problem) {
            return Err(de::Error::custom("Generic operation 与 problem 不一致"));
        }
        Ok(Self::new(wire.stage, wire.operation, wire.problem))
    }
}

const fn expected_operation(problem: &GenericProblem) -> GenericOperation {
    match problem {
        GenericProblem::SerializeJson { .. } => GenericOperation::SerializeJsonl,
        GenericProblem::Cancelled
        | GenericProblem::InvalidUtf8 { .. }
        | GenericProblem::BlankJsonlLine { .. }
        | GenericProblem::InvalidJson { .. }
        | GenericProblem::BlankField { .. }
        | GenericProblem::InvalidText { .. }
        | GenericProblem::EmptyUnits { .. }
        | GenericProblem::DuplicateUnitId { .. }
        | GenericProblem::DuplicateGroupId { .. } => GenericOperation::ParseJsonl,
        GenericProblem::ProjectCancelled | GenericProblem::ProjectIdentityMismatch { .. } => {
            GenericOperation::OpenProject
        }
        GenericProblem::SourceWriteBackOverlap { .. }
        | GenericProblem::MissingInitialField { .. }
        | GenericProblem::SameSourceAndTargetLanguage { .. }
        | GenericProblem::InvalidLanguage { .. } => GenericOperation::InitializeProject,
        GenericProblem::InvalidProjectDatabase { .. } => GenericOperation::OpenProject,
        GenericProblem::InputChangedDuringExtract => GenericOperation::ExtractInput,
        GenericProblem::ExtractRequired | GenericProblem::TranslationSnapshotChanged => {
            GenericOperation::LoadSnapshot
        }
        GenericProblem::DuplicateTranslationWrite { .. }
        | GenericProblem::DuplicateTranslationClear { .. }
        | GenericProblem::UnitNotFound { .. }
        | GenericProblem::InvalidTranslation { .. } => GenericOperation::CommitTranslations,
        GenericProblem::InvalidResource { .. }
        | GenericProblem::NonCanonicalResourceSnapshot { .. }
        | GenericProblem::UnexpectedResourceState { .. }
        | GenericProblem::WorkerStart { .. } => GenericOperation::PrepareTranslation,
        GenericProblem::WriteBackSourceChanged => GenericOperation::RecheckInput,
        GenericProblem::WriteBackSnapshotMismatch { .. } => {
            GenericOperation::BuildWriteBackCandidate
        }
        GenericProblem::WriteBackUnit { .. } => GenericOperation::BuildWriteBackCandidate,
        GenericProblem::WriteBackLayoutRules { .. } => GenericOperation::BuildWriteBackCandidate,
        GenericProblem::WriteBackMaterializedMismatch { .. } => {
            GenericOperation::MaterializeWriteBack
        }
        GenericProblem::MissingProfileId | GenericProblem::BlankProfileId => {
            GenericOperation::ResolveRunPlan
        }
        GenericProblem::TaskResponse { .. } | GenericProblem::TaskUnavailable { .. } => {
            GenericOperation::RecordTask
        }
        GenericProblem::TranslationPreparation { .. } => GenericOperation::PrepareTranslation,
    }
}

fn generic_unit_subject(unit: &GenericUnitLocator) -> String {
    unit.readable_id()
}

fn readable_generic_path(path: &SafePath) -> String {
    path.to_string().replace('\\', "/")
}

fn generic_unit_facts(unit: &GenericUnitLocator) -> Vec<(&'static str, String)> {
    let mut facts = vec![("relative_path", unit.relative_path.to_string())];
    if let Some(group_id) = &unit.group_id {
        facts.push(("group_id", group_id.to_string()));
    }
    if let Some(unit_id) = &unit.unit_id {
        facts.push(("unit_id", unit_id.to_string()));
    }
    if let Some(role) = &unit.role {
        facts.push(("role", role.to_string()));
    }
    facts
}

fn location_facts(location: &GenericJsonlLocation) -> Vec<(&'static str, String)> {
    let mut facts = vec![
        ("path", location.path.to_string()),
        ("line", location.line.to_string()),
    ];
    if let Some(group_id) = &location.group_id {
        facts.push(("group_id", group_id.to_string()));
    }
    if let Some(unit_id) = &location.unit_id {
        facts.push(("unit_id", unit_id.to_string()));
    }
    if let Some(group_ordinal) = location.group_ordinal {
        facts.push(("group_ordinal", group_ordinal.to_string()));
    }
    if let Some(unit_ordinal) = location.unit_ordinal {
        facts.push(("unit_ordinal", unit_ordinal.to_string()));
    }
    facts
}

fn optional_number(value: Option<usize>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{
        Diagnostic, DiagnosticReport, DiagnosticResolution, StateEffect, render_diagnostic_fields,
    };
    use crate::i18n::{UiLocale, UiLocalizer};

    #[test]
    fn jsonl_location_rejects_zero_line() {
        let value = serde_json::json!({
            "path": "scene.jsonl",
            "line": 0,
            "group_id": null,
            "unit_id": null,
            "group_ordinal": null,
            "unit_ordinal": null,
        });
        assert!(serde_json::from_value::<GenericJsonlLocation>(value).is_err());
    }

    #[test]
    fn jsonl_problem_derives_operation_code_and_resolution() {
        let location = GenericJsonlLocation::line(
            "scene.jsonl",
            NonZeroUsize::new(3).expect("一基行号必须非零"),
        )
        .with_group("group", Some(2))
        .with_unit("unit", Some(1));
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::jsonl(
                GenericDiagnosticStage::Extract,
                GenericProblem::InvalidJson {
                    location,
                    json_line: 1,
                    json_column: 7,
                    category: GenericJsonErrorCategory::Syntax,
                    field_problem: None,
                },
            )),
        );
        let wire = serde_json::to_value(report).expect("Generic 诊断必须可序列化");
        assert_eq!(wire["primary"]["code"], "generic.jsonl.invalid_json");
        assert_eq!(wire["primary"]["stage"], "extract");
        assert_eq!(wire["effect"], "unchanged");
        assert_eq!(
            wire["primary"]["issue"]["details"]["operation"],
            "parse_jsonl"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["location"]["line"],
            3
        );
    }

    #[test]
    #[should_panic(expected = "Generic 项目诊断不能承载 JSONL 格式问题")]
    fn project_constructor_rejects_jsonl_problem() {
        let _ = GenericIssue::project(
            GenericDiagnosticStage::ProjectOpening,
            GenericProblem::InvalidUtf8 {
                path: SafePath::new("scene.jsonl"),
                valid_up_to: 4,
                error_len: Some(1),
            },
        );
    }

    #[test]
    fn response_shape_problem_is_reported_as_contract_failure() {
        let issue = GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: 1,
                total_tasks: 1,
                problem: GenericTaskResponseProblem::InvalidJson {
                    category: GenericTaskResponseJsonCategory::Shape,
                    line: NonZeroUsize::MIN,
                    column: NonZeroUsize::MIN,
                },
            },
        );
        assert_eq!(issue.code(), "generic.translation.response.invalid_shape");
        let fields = render_diagnostic_fields(
            &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
        );
        let reason = fields.reason.replace(['\u{2068}', '\u{2069}'], "");
        assert!(reason.contains("根结构或响应结构"));
        assert!(reason.contains("第 1 行，第 1 列"));
    }

    #[test]
    fn response_json_syntax_problem_keeps_its_line_and_column() {
        let issue = GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: 1,
                total_tasks: 1,
                problem: GenericTaskResponseProblem::InvalidJson {
                    category: GenericTaskResponseJsonCategory::Syntax,
                    line: NonZeroUsize::new(4).expect("测试行号必须非零"),
                    column: NonZeroUsize::new(7).expect("测试列号必须非零"),
                },
            },
        );
        let reason = render_diagnostic_fields(
            &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
        )
        .reason
        .replace(['\u{2068}', '\u{2069}'], "");

        assert!(reason.contains("不是有效 JSON"));
        assert!(reason.contains("第 4 行，第 7 列"));
    }

    #[test]
    fn empty_think_keeps_internal_root_offset_without_presenting_it_as_field_position() {
        let issue = GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: 1,
                total_tasks: 1,
                problem: GenericTaskResponseProblem::ThinkingEmpty {
                    line: NonZeroUsize::new(4).expect("测试行号必须非零"),
                    column: NonZeroUsize::new(7).expect("测试列号必须非零"),
                },
            },
        );
        assert!(issue.facts().contains(&("line", "4".to_owned())));
        assert!(issue.facts().contains(&("column", "7".to_owned())));
        let reason = render_diagnostic_fields(
            &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
        )
        .reason
        .replace(['\u{2068}', '\u{2069}'], "");

        assert!(reason.contains("think 字段为空或仅含空白"));
        assert!(!reason.contains("第 4 行"));
        assert!(!reason.contains("第 7 列"));
    }

    #[test]
    fn response_leaf_problems_render_direct_reasons() {
        let destination = GenericUnitLocator::new(
            "story.jsonl",
            "internal-group",
            "internal-unit",
            Some("internal-role"),
        )
        .with_natural_position(2, 3);
        let cases = vec![
            (
                GenericTaskResponseProblem::InvalidId { item_index: 0 },
                "响应第 1 项",
            ),
            (
                GenericTaskResponseProblem::UnexpectedId { output_id: 9 },
                "未要求的 output ID",
            ),
            (
                GenericTaskResponseProblem::DuplicateId { output_id: 9 },
                "重复返回了同一个 output ID",
            ),
            (
                GenericTaskResponseProblem::MissingId { output_id: 9 },
                "缺少本任务要求的 output ID",
            ),
            (
                GenericTaskResponseProblem::InvalidValue {
                    output_id: 9,
                    problem: GenericResponseValueProblem::TranslationNotArray,
                },
                "translation 必须是字符串数组",
            ),
            (
                GenericTaskResponseProblem::InvalidValue {
                    output_id: 9,
                    problem: GenericResponseValueProblem::TranslationNonStringItem {
                        item: NonZeroUsize::new(2).expect("测试项号必须非零"),
                    },
                },
                "数组第 2 项",
            ),
            (
                GenericTaskResponseProblem::InvalidValue {
                    output_id: 9,
                    problem: GenericResponseValueProblem::SourceEchoMissingSource,
                },
                "source/translation 结构",
            ),
            (
                GenericTaskResponseProblem::InvalidTranslation {
                    output_id: 9,
                    problem: GenericResponseTextProblem::LineFeed,
                },
                "包含不允许的换行",
            ),
            (
                GenericTaskResponseProblem::InvalidDestination {
                    output_id: 9,
                    destination: destination.clone(),
                    problem: GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                        problem: GenericPlaceholderMultisetProblem::Mismatch,
                    },
                },
                "Placeholder 的身份或数量",
            ),
            (
                GenericTaskResponseProblem::InvalidDestination {
                    output_id: 9,
                    destination: destination.clone(),
                    problem: GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                        problem: GenericPlaceholderMultisetProblem::Unexpected,
                    },
                },
                "计划外的控制 token",
            ),
            (
                GenericTaskResponseProblem::InvalidDestination {
                    output_id: 9,
                    destination,
                    problem: GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                        problem: GenericPlaceholderMultisetProblem::OrderMismatch,
                    },
                },
                "控制 token 的顺序",
            ),
            (
                GenericTaskResponseProblem::InvalidDestination {
                    output_id: 9,
                    destination: GenericUnitLocator::new(
                        "story.jsonl",
                        "internal-group",
                        "internal-unit",
                        Some("internal-role"),
                    )
                    .with_natural_position(2, 3),
                    problem: GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                        problem: GenericPlaceholderMultisetProblem::WrapperTopologyChanged,
                    },
                },
                "Placeholder 边界",
            ),
        ];

        for (problem, expected) in cases {
            let issue = GenericIssue::project(
                GenericDiagnosticStage::Translate,
                GenericProblem::TaskResponse {
                    task_ordinal: 1,
                    total_tasks: 1,
                    problem,
                },
            );
            let reason = render_diagnostic_fields(
                &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
                &UiLocalizer::new(UiLocale::SimplifiedChinese),
            )
            .reason
            .replace(['\u{2068}', '\u{2069}'], "");
            assert!(
                reason.contains(expected),
                "诊断原因 {reason:?} 缺少 {expected:?}"
            );
            assert!(!reason.contains("响应契约"), "叶子诊断不得退化为总括文案");
        }
    }

    #[test]
    fn response_review_uses_review_action_without_fallback() {
        let issue = GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: 1,
                total_tasks: 1,
                problem: GenericTaskResponseProblem::ResponseReview {
                    finding: GenericResponseReviewFinding::SourceResidual,
                },
            },
        );
        assert_eq!(issue.resolution(), DiagnosticResolution::ReviewTranslation);
        let fields = render_diagnostic_fields(
            &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
        );
        assert!(fields.reason.contains("仍含源语言文本"));
        assert!(fields.help.contains("Manual"));
        assert!(!fields.reason.contains("__ATT_FALLBACK__"));
    }

    #[test]
    fn response_problem_subject_names_the_temporary_output_id() {
        let issue = GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: 1,
                total_tasks: 1,
                problem: GenericTaskResponseProblem::MissingId { output_id: 7 },
            },
        );

        assert_eq!(issue.subject(), "generic_translation_task_1; output_id=7");
    }

    #[test]
    fn destination_response_subject_names_the_natural_unit_without_internal_ids() {
        let destination = GenericUnitLocator::new(
            "story.jsonl",
            "internal-group",
            "internal-unit",
            Some("internal-role"),
        )
        .with_natural_position(3, 2);
        let problems = [
            GenericTaskResponseProblem::InvalidDestination {
                output_id: 7,
                destination: destination.clone(),
                problem: GenericResponseDestinationProblem::MissingPlanningFact,
            },
            GenericTaskResponseProblem::DestinationReview {
                output_id: 7,
                destination,
                finding: GenericResponseReviewFinding::SourceResidual,
            },
        ];

        for problem in problems {
            let issue = GenericIssue::project(
                GenericDiagnosticStage::Translate,
                GenericProblem::TaskResponse {
                    task_ordinal: 1,
                    total_tasks: 1,
                    problem,
                },
            );
            let fields = render_diagnostic_fields(
                &DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::generic(issue)),
                &UiLocalizer::new(UiLocale::SimplifiedChinese),
            );
            assert_eq!(
                fields.object,
                "story.jsonl:line3:unit2:text (generic_translation_task_1); output_id=7"
            );
            for internal in ["internal-group", "internal-unit", "internal-role"] {
                assert!(!fields.object.contains(internal));
            }
        }
    }
}
