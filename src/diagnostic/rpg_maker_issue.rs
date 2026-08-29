//! RPG Maker Translate 的封闭公开诊断模型。
//!
//! 本模块只保存 wire 类型及其派生行为。RPG Maker 领域类型到这些类型的映射由
//! Translate 模块在仍掌握语义事实的边界完成。

use serde::{Deserialize, Serialize};

use crate::json_diagnostic::JsonErrorCategory;

use super::{
    ByteRange, DiagnosticResolution, DiagnosticStage, FileSystemOrdinalKeyPhase, IoFailure,
    Pcre2Failure, PlaceholderIssue, PlaceholderRuleOrigin, PlaceholderRuleSource, SafeIdentifier,
    SafePath, SafeText, SqliteTransactionState,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticStage {
    CommandPreparation,
    ProjectOpening,
    Init,
    RunPlanFinalization,
    DialogueDefinitionInput,
    DialogueDefinitionProjectSnapshot,
    RulesDefinitionInput,
    RulesDefinitionProjectSnapshot,
    ExtractBuiltin,
    ExtractDocument,
    ExtractRules,
    ExtractStore,
    TranslatePlanning,
    TranslateExecution,
    TranslateCommit,
    TaskRecord,
    WriteBackDocument,
}

impl RpgMakerDiagnosticStage {
    pub(crate) const fn diagnostic_stage(self) -> DiagnosticStage {
        match self {
            Self::CommandPreparation => DiagnosticStage::CommandPreparation,
            Self::ProjectOpening => DiagnosticStage::ProjectOpening,
            Self::Init => DiagnosticStage::Init,
            Self::RunPlanFinalization => DiagnosticStage::RunPlanFinalization,
            Self::RulesDefinitionInput | Self::DialogueDefinitionInput => {
                DiagnosticStage::CommandPreparation
            }
            Self::RulesDefinitionProjectSnapshot
            | Self::DialogueDefinitionProjectSnapshot
            | Self::ExtractBuiltin
            | Self::ExtractDocument
            | Self::ExtractRules
            | Self::ExtractStore => DiagnosticStage::Extract,
            Self::TranslatePlanning
            | Self::TranslateExecution
            | Self::TranslateCommit
            | Self::TaskRecord => DiagnosticStage::Translate,
            Self::WriteBackDocument => DiagnosticStage::WriteBack,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticOwner {
    Builtin,
    Rules,
}

impl RpgMakerDiagnosticOwner {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Rules => "rules",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticGroupKind {
    DatabaseEntry,
    System,
    Map,
    EventDialogue,
    EventChoices,
    EventScrollingText,
    EventCommand,
    PluginParameter,
}

impl RpgMakerDiagnosticGroupKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DatabaseEntry => "database_entry",
            Self::System => "system",
            Self::Map => "map",
            Self::EventDialogue => "event_dialogue",
            Self::EventChoices => "event_choices",
            Self::EventScrollingText => "event_scrolling_text",
            Self::EventCommand => "event_command",
            Self::PluginParameter => "plugin_parameter",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticSource {
    Data {
        file: SafeIdentifier,
    },
    DataFile {
        file: SafeIdentifier,
    },
    Map {
        map_id: u32,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: SafeText,
        parameter_name: SafeText,
    },
}

impl RpgMakerDiagnosticSource {
    pub(crate) fn data(file: impl AsRef<str>) -> Self {
        Self::Data {
            file: SafeIdentifier::from_validated(file),
        }
    }

    pub(crate) fn data_file(file: impl AsRef<str>) -> Self {
        Self::DataFile {
            file: SafeIdentifier::from_validated(file),
        }
    }

    pub(crate) const fn map(map_id: u32) -> Self {
        Self::Map { map_id }
    }

    pub(crate) fn plugin_parameter(
        plugin_index: usize,
        plugin_name: impl AsRef<str>,
        parameter_name: impl AsRef<str>,
    ) -> Self {
        Self::PluginParameter {
            plugin_index,
            plugin_name: SafeText::new(plugin_name),
            parameter_name: SafeText::new(parameter_name),
        }
    }

    fn fact_value(&self) -> String {
        match self {
            Self::Data { file } => format!("data:{file}"),
            Self::DataFile { file } => format!("data_file:{file}"),
            Self::Map { map_id } => format!("map:{map_id}"),
            Self::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => format!("plugin_parameter:{plugin_index}:{plugin_name}:{parameter_name}"),
        }
    }

    fn natural_location(&self) -> String {
        match self {
            Self::Data { file } | Self::DataFile { file } => file.to_string(),
            Self::Map { map_id } => format!("Map{map_id:03}.json"),
            Self::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => format!(
                "plugins.js:plugin[{plugin_index}]({plugin_name}):parameter[{parameter_name}]"
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticLocationStep {
    ObjectKey { key: SafeText },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl RpgMakerDiagnosticLocationStep {
    pub(crate) fn object_key(key: impl AsRef<str>) -> Self {
        Self::ObjectKey {
            key: SafeText::new(key),
        }
    }

    pub(crate) const fn array_index(index: usize) -> Self {
        Self::ArrayIndex { index }
    }

    pub(crate) const fn decode_json_string() -> Self {
        Self::DecodeJsonString
    }

    fn fact_value(&self) -> String {
        match self {
            Self::ObjectKey { key } => format!("key:{key}"),
            Self::ArrayIndex { index } => format!("index:{index}"),
            Self::DecodeJsonString => "decode_json_string".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerDiagnosticLocation {
    source: RpgMakerDiagnosticSource,
    steps: Vec<RpgMakerDiagnosticLocationStep>,
}

impl RpgMakerDiagnosticLocation {
    pub(crate) fn new(
        source: RpgMakerDiagnosticSource,
        steps: Vec<RpgMakerDiagnosticLocationStep>,
    ) -> Self {
        Self { source, steps }
    }

    fn source_fact(&self) -> String {
        self.source.fact_value()
    }

    fn steps_fact(&self) -> String {
        if self.steps.is_empty() {
            return "root".to_owned();
        }
        self.steps
            .iter()
            .map(RpgMakerDiagnosticLocationStep::fact_value)
            .collect::<Vec<_>>()
            .join("/")
    }

    fn natural_location(&self) -> String {
        let mut location = self.source.natural_location();
        for step in &self.steps {
            match step {
                RpgMakerDiagnosticLocationStep::ObjectKey { key } => {
                    location.push(':');
                    location.push_str(&key.to_string());
                }
                RpgMakerDiagnosticLocationStep::ArrayIndex { index } => {
                    location.push('[');
                    location.push_str(&index.to_string());
                    location.push(']');
                }
                RpgMakerDiagnosticLocationStep::DecodeJsonString => {
                    location.push_str(":decoded_json");
                }
            }
        }
        location
    }

    fn natural_game_location(&self) -> String {
        let location = self.natural_location();
        match &self.source {
            RpgMakerDiagnosticSource::Data { .. }
            | RpgMakerDiagnosticSource::DataFile { .. }
            | RpgMakerDiagnosticSource::Map { .. } => format!("data/{location}"),
            RpgMakerDiagnosticSource::PluginParameter { .. } => location,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticRole {
    Scalar {
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<SafeIdentifier>,
    },
    DialogueSpeaker,
    DialogueBody,
    Choices,
    ScrollingText,
}

impl RpgMakerDiagnosticRole {
    pub(crate) fn scalar(field: impl AsRef<str>) -> Self {
        Self::Scalar {
            field: SafeIdentifier::new(field).ok(),
        }
    }

    fn fact_value(&self) -> String {
        match self {
            Self::Scalar { field: Some(field) } => format!("scalar:{field}"),
            Self::Scalar { field: None } => "scalar".to_owned(),
            Self::DialogueSpeaker => "dialogue_speaker".to_owned(),
            Self::DialogueBody => "dialogue_body".to_owned(),
            Self::Choices => "choices".to_owned(),
            Self::ScrollingText => "scrolling_text".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerUnitLocator {
    owner: RpgMakerDiagnosticOwner,
    group_kind: RpgMakerDiagnosticGroupKind,
    group_location: RpgMakerDiagnosticLocation,
    role: RpgMakerDiagnosticRole,
}

impl RpgMakerUnitLocator {
    pub(crate) const fn new(
        owner: RpgMakerDiagnosticOwner,
        group_kind: RpgMakerDiagnosticGroupKind,
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    ) -> Self {
        Self {
            owner,
            group_kind,
            group_location,
            role,
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("owner", self.owner.as_str().to_owned()),
            ("group_kind", self.group_kind.as_str().to_owned()),
            ("source", self.group_location.source_fact()),
            ("location_steps", self.group_location.steps_fact()),
            ("role", self.role.fact_value()),
        ]
    }

    fn placeholder_subject(
        &self,
        rule_source: &PlaceholderRuleSource,
        rules: Option<String>,
    ) -> String {
        let mut details = vec![
            format!("group={}", self.group_kind.as_str()),
            format!("role={}", self.role.fact_value()),
            format!(
                "Placeholder={}",
                match rule_source {
                    PlaceholderRuleSource::ExternalFile { path } => path.to_string(),
                    PlaceholderRuleSource::ProjectSnapshot => "project snapshot".to_owned(),
                }
            ),
        ];
        if let Some(rules) = rules {
            details.push(format!("rules={rules}"));
        }
        format!(
            "{} ({})",
            self.group_location.natural_location(),
            details.join("; ")
        )
    }
}

fn placeholder_rule_label(
    origin: Option<PlaceholderRuleOrigin>,
    rule_number: Option<usize>,
) -> Option<String> {
    match (origin, rule_number) {
        (Some(PlaceholderRuleOrigin::Builtin), _) => Some("builtin".to_owned()),
        (Some(PlaceholderRuleOrigin::Custom), Some(rule_number)) => {
            Some(format!("custom rule {rule_number}"))
        }
        (Some(PlaceholderRuleOrigin::Custom), None) => Some("custom rule".to_owned()),
        (None, Some(rule_number)) => Some(format!("rule {rule_number}")),
        (None, None) => None,
    }
}

fn placeholder_problem_rules(problem: &PlaceholderIssue) -> Option<String> {
    let rules: Vec<String> = match problem {
        PlaceholderIssue::PatternMatch {
            rule_origin,
            rule_number,
            ..
        } => placeholder_rule_label(*rule_origin, *rule_number)
            .into_iter()
            .collect(),
        PlaceholderIssue::EmptyMatch {
            rule_origin,
            rule_number,
            ..
        }
        | PlaceholderIssue::CrossesLineBoundary {
            rule_origin,
            rule_number,
            ..
        } => placeholder_rule_label(Some(*rule_origin), *rule_number)
            .into_iter()
            .collect(),
        PlaceholderIssue::MissingTextCapture { rule_number, .. }
        | PlaceholderIssue::InvalidMatchRange { rule_number, .. } => {
            vec![format!("custom rule {rule_number}")]
        }
        PlaceholderIssue::OverlappingMatches {
            first_origin,
            first_rule_number,
            second_origin,
            second_rule_number,
            ..
        } => [
            placeholder_rule_label(Some(*first_origin), *first_rule_number),
            placeholder_rule_label(Some(*second_origin), *second_rule_number),
        ]
        .into_iter()
        .flatten()
        .collect(),
        PlaceholderIssue::WorkerStart { .. } | PlaceholderIssue::ReservedTokenNamespace { .. } => {
            Vec::new()
        }
    };
    (!rules.is_empty()).then(|| rules.join(" + "))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerPlaceholderProjectionProblem {
    TokenIndexConstruction,
    EmptyToken,
    MissingToken {
        token: SafeIdentifier,
    },
    RepeatedToken {
        token: SafeIdentifier,
    },
    OverlappingToken {
        token: SafeIdentifier,
    },
    ChangedTokenOrder {
        position: usize,
        expected_token: SafeIdentifier,
        actual_token: SafeIdentifier,
    },
    ChangedSegmentCount {
        expected: usize,
        actual: usize,
    },
    ChangedSegmentKind {
        segment_index: usize,
    },
    MissingOrderedToken {
        segment_index: usize,
    },
    UnusedOrderedToken,
    SourceBindingMismatch,
}

impl RpgMakerPlaceholderProjectionProblem {
    pub(crate) fn missing_token(token: impl AsRef<str>) -> Self {
        Self::MissingToken {
            token: SafeIdentifier::from_validated(token),
        }
    }

    pub(crate) fn repeated_token(token: impl AsRef<str>) -> Self {
        Self::RepeatedToken {
            token: SafeIdentifier::from_validated(token),
        }
    }

    pub(crate) fn overlapping_token(token: impl AsRef<str>) -> Self {
        Self::OverlappingToken {
            token: SafeIdentifier::from_validated(token),
        }
    }

    pub(crate) fn changed_token_order(
        position: usize,
        expected_token: impl AsRef<str>,
        actual_token: impl AsRef<str>,
    ) -> Self {
        Self::ChangedTokenOrder {
            position,
            expected_token: SafeIdentifier::from_validated(expected_token),
            actual_token: SafeIdentifier::from_validated(actual_token),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::TokenIndexConstruction => {
                vec![("projection_failure", "token_index_construction".to_owned())]
            }
            Self::EmptyToken => vec![("projection_failure", "empty_token".to_owned())],
            Self::MissingToken { token } => vec![
                ("projection_failure", "missing_token".to_owned()),
                ("token", token.to_string()),
            ],
            Self::RepeatedToken { token } => vec![
                ("projection_failure", "repeated_token".to_owned()),
                ("token", token.to_string()),
            ],
            Self::OverlappingToken { token } => vec![
                ("projection_failure", "overlapping_token".to_owned()),
                ("token", token.to_string()),
            ],
            Self::ChangedTokenOrder {
                position,
                expected_token,
                actual_token,
            } => vec![
                ("projection_failure", "changed_token_order".to_owned()),
                ("position", position.to_string()),
                ("expected_token", expected_token.to_string()),
                ("actual_token", actual_token.to_string()),
            ],
            Self::ChangedSegmentCount { expected, actual } => vec![
                ("projection_failure", "changed_segment_count".to_owned()),
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ],
            Self::ChangedSegmentKind { segment_index } => vec![
                ("projection_failure", "changed_segment_kind".to_owned()),
                ("segment_index", segment_index.to_string()),
            ],
            Self::MissingOrderedToken { segment_index } => vec![
                ("projection_failure", "missing_ordered_token".to_owned()),
                ("segment_index", segment_index.to_string()),
            ],
            Self::UnusedOrderedToken => {
                vec![("projection_failure", "unused_ordered_token".to_owned())]
            }
            Self::SourceBindingMismatch => {
                vec![("projection_failure", "source_binding_mismatch".to_owned())]
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesDefinitionOrigin {
    ExternalToml,
    ProjectSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerJsonFailureKind {
    Io,
    Syntax,
    Data,
    Eof,
    DuplicateObjectKey,
}

impl RpgMakerJsonFailureKind {
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

impl From<JsonErrorCategory> for RpgMakerJsonFailureKind {
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesSourceKind {
    File,
    Plugin,
    Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesPathFailure {
    Empty,
    UnsupportedJsonPath {
        byte_offset: usize,
    },
    UnexpectedDot {
        byte_offset: usize,
    },
    DotBeforeBracket {
        byte_offset: usize,
    },
    MissingDot {
        byte_offset: usize,
    },
    InvalidBareKey {
        byte_offset: usize,
    },
    TrailingDot {
        byte_offset: usize,
    },
    UnclosedBracket {
        byte_offset: usize,
    },
    MissingQuotedKey {
        byte_offset: usize,
    },
    InvalidQuotedKey {
        byte_offset: usize,
        json_category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    QuotedKeyMissingClose {
        byte_offset: usize,
    },
    InvalidBracket {
        byte_offset: usize,
    },
    IndexOutOfRange {
        byte_offset: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesDefinitionProblem {
    InvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidToml {
        byte_range: Option<ByteRange>,
    },
    InvalidCanonicalJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    EncodeCanonicalJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    NonCanonicalJson,
    MissingSource {
        rule_number: usize,
    },
    ConflictingSources {
        rule_number: usize,
    },
    ParameterWithoutCode {
        rule_number: usize,
    },
    MissingParameter {
        rule_number: usize,
    },
    InvalidCode {
        rule_number: usize,
        code: i64,
    },
    MissingPath {
        rule_number: usize,
        source: RpgMakerRulesSourceKind,
    },
    EmptyField {
        rule_number: usize,
        field: SafeIdentifier,
    },
    InvalidFile {
        rule_number: usize,
    },
    InvalidPath {
        rule_number: usize,
        failure: RpgMakerRulesPathFailure,
    },
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        failure: Pcre2Failure,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        actual_count: usize,
    },
}

impl RpgMakerRulesDefinitionProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidUtf8 { .. } => "rpg_maker.rules.definition.invalid_utf8",
            Self::InvalidToml { .. } => "rpg_maker.rules.definition.invalid_toml",
            Self::InvalidCanonicalJson { .. } => {
                "rpg_maker.rules.definition.invalid_canonical_json"
            }
            Self::EncodeCanonicalJson { .. } => "rpg_maker.rules.definition.encode_canonical_json",
            Self::NonCanonicalJson => "rpg_maker.rules.definition.non_canonical_json",
            Self::MissingSource { .. } => "rpg_maker.rules.definition.missing_source",
            Self::ConflictingSources { .. } => "rpg_maker.rules.definition.conflicting_sources",
            Self::ParameterWithoutCode { .. } => {
                "rpg_maker.rules.definition.parameter_without_code"
            }
            Self::MissingParameter { .. } => "rpg_maker.rules.definition.missing_parameter",
            Self::InvalidCode { .. } => "rpg_maker.rules.definition.invalid_code",
            Self::MissingPath { .. } => "rpg_maker.rules.definition.missing_path",
            Self::EmptyField { .. } => "rpg_maker.rules.definition.empty_field",
            Self::InvalidFile { .. } => "rpg_maker.rules.definition.invalid_file",
            Self::InvalidPath { .. } => "rpg_maker.rules.definition.invalid_path",
            Self::EmptyPattern { .. } => "rpg_maker.rules.definition.empty_pattern",
            Self::InvalidPattern { .. } => "rpg_maker.rules.definition.invalid_pattern",
            Self::InvalidNamedCaptures { .. } => {
                "rpg_maker.rules.definition.invalid_named_captures"
            }
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::InvalidToml { .. }
            | Self::InvalidCanonicalJson { .. }
            | Self::NonCanonicalJson
            | Self::InvalidPattern { .. } => "invalid_syntax",
            Self::EncodeCanonicalJson { .. } => "internal_invariant",
            Self::MissingSource { .. }
            | Self::MissingParameter { .. }
            | Self::MissingPath { .. }
            | Self::EmptyField { .. }
            | Self::EmptyPattern { .. } => "missing_required_value",
            Self::ConflictingSources { .. } => "conflicting_values",
            Self::ParameterWithoutCode { .. }
            | Self::InvalidCode { .. }
            | Self::InvalidFile { .. }
            | Self::InvalidPath { .. }
            | Self::InvalidNamedCaptures { .. } => "invalid_value",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesValueStep {
    Key { key: SafeText },
    Index { index: usize },
    DecodeJsonString,
}

impl RpgMakerRulesValueStep {
    pub(crate) fn key(value: impl AsRef<str>) -> Self {
        Self::Key {
            key: SafeText::new(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerJsonValueKind {
    Missing,
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesMatchSource {
    DataFile {
        file: SafeText,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: SafeText,
        parameter_name: SafeText,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesDiagnosticSource {
    DataFile {
        file: SafeText,
    },
    Plugin {
        plugin_index: usize,
        plugin_name: SafeText,
    },
    Command {
        file: SafeText,
        code: i64,
        parameter: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerRulesMatchContext {
    source: RpgMakerRulesDiagnosticSource,
    has_declared_path: bool,
}

impl RpgMakerRulesMatchContext {
    pub(crate) const fn new(
        source: RpgMakerRulesDiagnosticSource,
        has_declared_path: bool,
    ) -> Self {
        Self {
            source,
            has_declared_path,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesCommandNonStringType {
    Null,
    Boolean,
    Number,
    Array,
    Object,
}

impl RpgMakerRulesCommandNonStringType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerRulesCommandNonStringFact {
    rule_number: usize,
    source_file: SafeText,
    command_code: i64,
    parameter: usize,
    actual_type: RpgMakerRulesCommandNonStringType,
    skipped_count: u64,
}

impl RpgMakerRulesCommandNonStringFact {
    pub(crate) fn new(
        rule_number: usize,
        source_file: impl AsRef<str>,
        command_code: i64,
        parameter: usize,
        actual_type: RpgMakerRulesCommandNonStringType,
        skipped_count: u64,
    ) -> Self {
        Self {
            rule_number,
            source_file: SafeText::new(source_file),
            command_code,
            parameter,
            actual_type,
            skipped_count,
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("rule_number", self.rule_number.to_string()),
            ("source_file", self.source_file.to_string()),
            ("command_code", self.command_code.to_string()),
            ("parameter", self.parameter.to_string()),
            ("actual_type", self.actual_type.as_str().to_owned()),
            ("skipped_count", self.skipped_count.to_string()),
        ]
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerLogicalUnitLocator {
    group_location: RpgMakerDiagnosticLocation,
    role: RpgMakerDiagnosticRole,
}

impl RpgMakerLogicalUnitLocator {
    pub(crate) const fn new(
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    ) -> Self {
        Self {
            group_location,
            role,
        }
    }

    fn natural_id(&self) -> String {
        format!(
            "{}:{}",
            self.group_location.natural_game_location(),
            self.role.fact_value()
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesInvalidTarget {
    InvalidDataFileName {
        file: SafeText,
    },
    PluginFieldType {
        plugin_index: usize,
        plugin_name: SafeText,
        field: SafeIdentifier,
        expected: SafeIdentifier,
        actual: RpgMakerJsonValueKind,
    },
    PluginPathMissingParameter {
        at: Vec<RpgMakerRulesValueStep>,
    },
    PluginGroupMissingParameter {
        at: Vec<RpgMakerRulesValueStep>,
    },
    PluginGroupCrossesParameters {
        expected_parameter: SafeText,
        actual_parameter: SafeText,
        at: Vec<RpgMakerRulesValueStep>,
    },
    NestedJsonDecode {
        phase: SafeIdentifier,
        at: Vec<RpgMakerRulesValueStep>,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    ExpectedObject {
        at: Vec<RpgMakerRulesValueStep>,
        key: SafeText,
        actual: RpgMakerJsonValueKind,
    },
    ExpectedArray {
        at: Vec<RpgMakerRulesValueStep>,
        index: Option<usize>,
        actual: RpgMakerJsonValueKind,
    },
    CommandParametersType {
        file: SafeText,
        code: i64,
        parameter: usize,
        at: Vec<RpgMakerRulesValueStep>,
        actual: RpgMakerJsonValueKind,
    },
    CommandParameterMissing {
        file: SafeText,
        code: i64,
        parameter: usize,
        available: usize,
        at: Vec<RpgMakerRulesValueStep>,
    },
    DecodeJsonTargetType {
        at: Vec<RpgMakerRulesValueStep>,
        actual: RpgMakerJsonValueKind,
    },
    FinalTargetType {
        at: Vec<RpgMakerRulesValueStep>,
        actual: RpgMakerJsonValueKind,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectionFailureKind {
    EmptyScalarFieldKey,
    EventBlockCoverageRequired,
    InvalidEventBlockCoverage,
    MutationClaimTargetMismatch,
    RecipeHasNoTextSlot,
    DuplicateProjectionSlot,
    MultipleBodyLinesInPhysicalLine,
    DuplicateDialogueBodyLine,
    NonContiguousDialogueBodyLines,
    MixedDirectAndInlineSpeaker,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesMaterializationFailure {
    Projection {
        at: Vec<RpgMakerRulesValueStep>,
        failure: RpgMakerProjectionFailureKind,
    },
    UnitCount {
        at: Vec<RpgMakerRulesValueStep>,
        expected: usize,
        actual: usize,
    },
    RoundTripMismatch {
        at: Vec<RpgMakerRulesValueStep>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRulesMatchProblem {
    NoNonBlankMatch {
        rule_number: usize,
        skipped_non_strings: Vec<RpgMakerRulesCommandNonStringFact>,
    },
    InvalidTarget {
        rule_number: usize,
        reason: RpgMakerRulesInvalidTarget,
    },
    PatternMatch {
        rule_number: usize,
        at: Vec<RpgMakerRulesValueStep>,
        failure: Pcre2Failure,
    },
    ZeroWidthMatch {
        rule_number: usize,
        at: Vec<RpgMakerRulesValueStep>,
        match_range: ByteRange,
    },
    OverlappingMatch {
        rule_number: usize,
        at: Vec<RpgMakerRulesValueStep>,
        previous_end: usize,
        match_range: ByteRange,
    },
    MissingTextCapture {
        rule_number: usize,
        at: Vec<RpgMakerRulesValueStep>,
    },
    InvalidCaptureRange {
        rule_number: usize,
        at: Vec<RpgMakerRulesValueStep>,
        text_bytes: usize,
        whole_start: usize,
        whole_end: usize,
        capture_start: usize,
        capture_end: usize,
    },
    DuplicateTarget {
        first_rule: usize,
        second_rule: usize,
        source: RpgMakerRulesMatchSource,
        steps: Vec<RpgMakerRulesValueStep>,
    },
    InvalidMaterialization {
        rule_number: usize,
        reason: RpgMakerRulesMaterializationFailure,
    },
}

impl RpgMakerRulesMatchProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::NoNonBlankMatch { .. } => "rpg_maker.rules.no_non_blank_match",
            Self::InvalidTarget { .. } => "rpg_maker.rules.invalid_target",
            Self::PatternMatch { .. } => "rpg_maker.rules.pattern_match_failed",
            Self::ZeroWidthMatch { .. } => "rpg_maker.rules.zero_width_match",
            Self::OverlappingMatch { .. } => "rpg_maker.rules.overlapping_capture",
            Self::MissingTextCapture { .. } => "rpg_maker.rules.missing_text_capture",
            Self::InvalidCaptureRange { .. } => "rpg_maker.rules.invalid_capture_range",
            Self::DuplicateTarget { .. } => "rpg_maker.rules.duplicate_target",
            Self::InvalidMaterialization { .. } => "rpg_maker.rules.invalid_materialization",
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::NoNonBlankMatch { .. } | Self::MissingTextCapture { .. } => {
                "missing_required_value"
            }
            Self::ZeroWidthMatch { .. } => "empty_text_capture",
            Self::DuplicateTarget { .. } => "conflicting_values",
            Self::InvalidMaterialization { .. } => "internal_invariant",
            Self::InvalidTarget { .. }
            | Self::PatternMatch { .. }
            | Self::OverlappingMatch { .. }
            | Self::InvalidCaptureRange { .. } => "invalid_value",
        }
    }
}

fn rules_match_subject(
    rules_path: &SafePath,
    context: Option<&RpgMakerRulesMatchContext>,
    problem: &RpgMakerRulesMatchProblem,
) -> String {
    let rule = rules_match_rule_label(problem);
    match rules_match_target(context, problem) {
        Some(target) => format!("{rules_path}:{rule} -> {target}"),
        None => format!("{rules_path}:{rule}"),
    }
}

fn rules_match_rule_label(problem: &RpgMakerRulesMatchProblem) -> String {
    match problem {
        RpgMakerRulesMatchProblem::DuplicateTarget {
            first_rule,
            second_rule,
            ..
        } => format!("Rules[{first_rule},{second_rule}]"),
        RpgMakerRulesMatchProblem::NoNonBlankMatch { rule_number, .. }
        | RpgMakerRulesMatchProblem::InvalidTarget { rule_number, .. }
        | RpgMakerRulesMatchProblem::PatternMatch { rule_number, .. }
        | RpgMakerRulesMatchProblem::ZeroWidthMatch { rule_number, .. }
        | RpgMakerRulesMatchProblem::OverlappingMatch { rule_number, .. }
        | RpgMakerRulesMatchProblem::MissingTextCapture { rule_number, .. }
        | RpgMakerRulesMatchProblem::InvalidCaptureRange { rule_number, .. }
        | RpgMakerRulesMatchProblem::InvalidMaterialization { rule_number, .. } => {
            format!("Rules[{rule_number}]")
        }
    }
}

fn rules_match_target(
    context: Option<&RpgMakerRulesMatchContext>,
    problem: &RpgMakerRulesMatchProblem,
) -> Option<String> {
    let mut target = match problem {
        RpgMakerRulesMatchProblem::DuplicateTarget { source, .. } => {
            rules_match_source_target(source)
        }
        RpgMakerRulesMatchProblem::InvalidTarget {
            reason:
                RpgMakerRulesInvalidTarget::PluginFieldType {
                    plugin_index,
                    plugin_name,
                    field,
                    ..
                },
            ..
        } if context.is_none() => format!(
            "plugins.js:plugin[{plugin_index}]({plugin_name})[{}]",
            serde_json::to_string(&field.to_string()).expect("安全字段名始终可以编码为 JSON")
        ),
        _ => rules_match_context_target(context?),
    };
    if let Some(steps) = rules_match_steps(problem) {
        append_rules_match_steps(&mut target, steps);
    }
    Some(target)
}

fn rules_match_context_target(context: &RpgMakerRulesMatchContext) -> String {
    match &context.source {
        RpgMakerRulesDiagnosticSource::DataFile { file } => file.to_string(),
        RpgMakerRulesDiagnosticSource::Plugin {
            plugin_index,
            plugin_name,
        } => format!("plugins.js:plugin[{plugin_index}]({plugin_name})"),
        RpgMakerRulesDiagnosticSource::Command {
            file,
            code,
            parameter,
        } => format!("{file}:command[{code}]:parameter[{parameter}]"),
    }
}

fn rules_match_source_target(source: &RpgMakerRulesMatchSource) -> String {
    match source {
        RpgMakerRulesMatchSource::DataFile { file } => file.to_string(),
        RpgMakerRulesMatchSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => {
            format!("plugins.js:plugin[{plugin_index}]({plugin_name}):parameter[{parameter_name}]")
        }
    }
}

fn rules_match_steps(problem: &RpgMakerRulesMatchProblem) -> Option<&[RpgMakerRulesValueStep]> {
    match problem {
        RpgMakerRulesMatchProblem::NoNonBlankMatch { .. } => None,
        RpgMakerRulesMatchProblem::InvalidTarget { reason, .. } => {
            rules_invalid_target_steps(reason)
        }
        RpgMakerRulesMatchProblem::PatternMatch { at, .. }
        | RpgMakerRulesMatchProblem::ZeroWidthMatch { at, .. }
        | RpgMakerRulesMatchProblem::OverlappingMatch { at, .. }
        | RpgMakerRulesMatchProblem::MissingTextCapture { at, .. }
        | RpgMakerRulesMatchProblem::InvalidCaptureRange { at, .. }
        | RpgMakerRulesMatchProblem::DuplicateTarget { steps: at, .. } => Some(at),
        RpgMakerRulesMatchProblem::InvalidMaterialization { reason, .. } => Some(match reason {
            RpgMakerRulesMaterializationFailure::Projection { at, .. }
            | RpgMakerRulesMaterializationFailure::UnitCount { at, .. }
            | RpgMakerRulesMaterializationFailure::RoundTripMismatch { at } => at,
        }),
    }
}

fn rules_invalid_target_steps(
    reason: &RpgMakerRulesInvalidTarget,
) -> Option<&[RpgMakerRulesValueStep]> {
    match reason {
        RpgMakerRulesInvalidTarget::InvalidDataFileName { .. }
        | RpgMakerRulesInvalidTarget::PluginFieldType { .. } => None,
        RpgMakerRulesInvalidTarget::PluginPathMissingParameter { at }
        | RpgMakerRulesInvalidTarget::PluginGroupMissingParameter { at }
        | RpgMakerRulesInvalidTarget::PluginGroupCrossesParameters { at, .. }
        | RpgMakerRulesInvalidTarget::NestedJsonDecode { at, .. }
        | RpgMakerRulesInvalidTarget::ExpectedObject { at, .. }
        | RpgMakerRulesInvalidTarget::ExpectedArray { at, .. }
        | RpgMakerRulesInvalidTarget::CommandParametersType { at, .. }
        | RpgMakerRulesInvalidTarget::CommandParameterMissing { at, .. }
        | RpgMakerRulesInvalidTarget::DecodeJsonTargetType { at, .. }
        | RpgMakerRulesInvalidTarget::FinalTargetType { at, .. } => Some(at),
    }
}

fn append_rules_match_steps(target: &mut String, steps: &[RpgMakerRulesValueStep]) {
    target.push('$');
    for step in steps {
        match step {
            RpgMakerRulesValueStep::Key { key } => {
                target.push('[');
                target.push_str(
                    &serde_json::to_string(&key.to_string())
                        .expect("安全 Rules 路径键始终可以编码为 JSON"),
                );
                target.push(']');
            }
            RpgMakerRulesValueStep::Index { index } => {
                target.push('[');
                target.push_str(&index.to_string());
                target.push(']');
            }
            RpgMakerRulesValueStep::DecodeJsonString => {}
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerBuiltinDocumentProblem {
    MissingDocument,
    ExpectedObject,
    ExpectedArray,
    ExpectedString,
    MissingValue,
    EventCodeMustBeInteger,
    EventParametersMissing,
    EventIndentMustBeInteger,
    ContinuationWithoutStart { command_code: i64 },
    ChoiceIndexInvalid,
    ChoiceEndMissing,
}

impl RpgMakerBuiltinDocumentProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::MissingDocument => "rpg_maker.builtin.document_missing",
            Self::ExpectedObject => "rpg_maker.builtin.expected_object",
            Self::ExpectedArray => "rpg_maker.builtin.expected_array",
            Self::ExpectedString => "rpg_maker.builtin.expected_string",
            Self::MissingValue => "rpg_maker.builtin.value_missing",
            Self::EventCodeMustBeInteger => "rpg_maker.builtin.event_code_not_integer",
            Self::EventParametersMissing => "rpg_maker.builtin.event_parameters_missing",
            Self::EventIndentMustBeInteger => "rpg_maker.builtin.event_indent_not_integer",
            Self::ContinuationWithoutStart { .. } => "rpg_maker.builtin.continuation_without_start",
            Self::ChoiceIndexInvalid => "rpg_maker.builtin.choice_index_invalid",
            Self::ChoiceEndMissing => "rpg_maker.builtin.choice_end_missing",
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::MissingDocument => "not_found",
            Self::MissingValue
            | Self::EventParametersMissing
            | Self::ContinuationWithoutStart { .. }
            | Self::ChoiceEndMissing => "missing_required_value",
            Self::ExpectedObject
            | Self::ExpectedArray
            | Self::ExpectedString
            | Self::EventCodeMustBeInteger
            | Self::EventIndentMustBeInteger
            | Self::ChoiceIndexInvalid => "invalid_value",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDialogueDefinitionOrigin {
    ExternalToml,
    ProjectSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerTomlFailureKind {
    Syntax,
    InvalidValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDialogueDefinitionProblem {
    InvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    EmptyDocument,
    MissingRuleArray,
    InvalidToml {
        failure: RpgMakerTomlFailureKind,
        line: Option<u64>,
        column: Option<u64>,
    },
    InvalidCanonicalJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    EncodeCanonicalJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        failure: Pcre2Failure,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        safe_actual_captures: Vec<SafeIdentifier>,
        actual_count: usize,
        hidden_count: usize,
    },
}

impl RpgMakerDialogueDefinitionProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidUtf8 { .. } => "rpg_maker.dialogue.definition.invalid_utf8",
            Self::EmptyDocument => "rpg_maker.dialogue.definition.empty",
            Self::MissingRuleArray => "rpg_maker.dialogue.definition.rule_array_missing",
            Self::InvalidToml { .. } => "rpg_maker.dialogue.definition.invalid_toml",
            Self::InvalidCanonicalJson { .. } => {
                "rpg_maker.dialogue.definition.invalid_canonical_json"
            }
            Self::EncodeCanonicalJson { .. } => {
                "rpg_maker.dialogue.definition.encode_canonical_json"
            }
            Self::EmptyPattern { .. } => "rpg_maker.dialogue.definition.empty_pattern",
            Self::InvalidPattern { .. } => "rpg_maker.dialogue.definition.invalid_pattern",
            Self::InvalidNamedCaptures { .. } => {
                "rpg_maker.dialogue.definition.invalid_named_captures"
            }
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::EmptyDocument | Self::MissingRuleArray | Self::EmptyPattern { .. } => {
                "missing_required_value"
            }
            Self::InvalidToml { .. }
            | Self::InvalidCanonicalJson { .. }
            | Self::InvalidPattern { .. } => "invalid_syntax",
            Self::EncodeCanonicalJson { .. } => "internal_invariant",
            Self::InvalidNamedCaptures { .. } => "invalid_value",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => vec![
                ("valid_up_to", valid_up_to.to_string()),
                (
                    "error_len",
                    error_len.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                ),
            ],
            Self::InvalidToml { line, column, .. } => vec![
                (
                    "line",
                    line.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                ),
                (
                    "column",
                    column.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                ),
            ],
            Self::InvalidCanonicalJson { line, column, .. }
            | Self::EncodeCanonicalJson { line, column, .. } => {
                vec![("line", line.to_string()), ("column", column.to_string())]
            }
            Self::EmptyPattern { rule_number }
            | Self::InvalidPattern { rule_number, .. }
            | Self::InvalidNamedCaptures { rule_number, .. } => {
                vec![("rule_number", rule_number.to_string())]
            }
            Self::EmptyDocument | Self::MissingRuleArray => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDialogueProjectionProblem {
    Match {
        rule_number: usize,
        location: RpgMakerDiagnosticLocation,
        failure: Pcre2Failure,
    },
    ZeroWidthMatch {
        rule_number: usize,
        location: RpgMakerDiagnosticLocation,
    },
    MissingSpeakerCapture {
        rule_number: usize,
        location: RpgMakerDiagnosticLocation,
    },
    InvalidSpeakerCaptureRange {
        rule_number: usize,
        location: RpgMakerDiagnosticLocation,
    },
    MultipleRulesOwnField {
        location: RpgMakerDiagnosticLocation,
        first_rule: usize,
        second_rule: usize,
    },
    DifferentSpeakers {
        location: RpgMakerDiagnosticLocation,
    },
    RuleCapturedNoSpeaker {
        rule_number: usize,
    },
    InvalidRecipe {
        failure: RpgMakerProjectionFailureKind,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerBackendCause {
    diagnostic: Box<super::Diagnostic>,
}

impl RpgMakerBackendCause {
    pub(crate) fn new(diagnostic: super::Diagnostic) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDocumentConsumer {
    Builtin,
    Rules,
    WriteBack,
}

impl RpgMakerDocumentConsumer {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Rules => "rules",
            Self::WriteBack => "write_back",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerDocumentOperation {
    ListData,
    ListJs,
    ResolveFileName,
    Read,
    ScheduleParse,
    Parse,
}

impl RpgMakerDocumentOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ListData => "list_data",
            Self::ListJs => "list_js",
            Self::ResolveFileName => "resolve_file_name",
            Self::Read => "read",
            Self::ScheduleParse => "schedule_parse",
            Self::Parse => "parse",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerPluginsEnvelopeFailure {
    Declaration,
    Prefix,
    Assignment,
    Terminator,
    RootType,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDocumentProblem {
    NotFound {
        path: SafePath,
    },
    NotDirectory {
        path: SafePath,
    },
    NotFile {
        path: SafePath,
    },
    FileNameCaseMismatch {
        requested: SafePath,
        actual: SafePath,
    },
    FileNameTooLarge {
        path: SafePath,
        observed: u64,
        maximum: u64,
    },
    Backend {
        path: SafePath,
        cause: RpgMakerBackendCause,
    },
    InvalidUtf8 {
        path: SafePath,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidJson {
        path: SafePath,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    InvalidPluginsEnvelope {
        path: SafePath,
        failure: RpgMakerPluginsEnvelopeFailure,
    },
    InvalidPluginRecord {
        path: SafePath,
        index: usize,
    },
}

impl RpgMakerDocumentProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "rpg_maker.document.not_found",
            Self::NotDirectory { .. } => "rpg_maker.document.not_directory",
            Self::NotFile { .. } => "rpg_maker.document.not_file",
            Self::FileNameCaseMismatch { .. } => "rpg_maker.document.file_name_case_mismatch",
            Self::FileNameTooLarge { .. } => "rpg_maker.document.file_name_too_large",
            Self::Backend { .. } => "rpg_maker.document.backend_failed",
            Self::InvalidUtf8 { .. } => "rpg_maker.document.invalid_utf8",
            Self::InvalidJson { .. } => "rpg_maker.document.invalid_json",
            Self::InvalidPluginsEnvelope { .. } => "rpg_maker.document.invalid_plugins_envelope",
            Self::InvalidPluginRecord { .. } => "rpg_maker.document.invalid_plugin_record",
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::NotDirectory { .. }
            | Self::NotFile { .. }
            | Self::FileNameCaseMismatch { .. }
            | Self::InvalidPluginsEnvelope { .. }
            | Self::InvalidPluginRecord { .. } => "invalid_path",
            Self::FileNameTooLarge { .. } => "resource_limit",
            Self::Backend { .. } => "external_service_unavailable",
            Self::InvalidUtf8 { .. } => "invalid_encoding",
            Self::InvalidJson { .. } => "invalid_syntax",
        }
    }
}

/// 译后响应处理仍掌握的最小定位事实。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResponseProcessingScope {
    Task {
        task_index: usize,
    },
    Unit {
        task_index: usize,
        unit: RpgMakerUnitLocator,
    },
}

impl RpgMakerResponseProcessingScope {
    pub(crate) const fn task(task_index: usize) -> Self {
        Self::Task { task_index }
    }

    pub(crate) const fn unit(task_index: usize, unit: RpgMakerUnitLocator) -> Self {
        Self::Unit { task_index, unit }
    }

    const fn task_index(&self) -> usize {
        match self {
            Self::Task { task_index } | Self::Unit { task_index, .. } => *task_index,
        }
    }

    fn unit_locator(&self) -> Option<&RpgMakerUnitLocator> {
        match self {
            Self::Task { .. } => None,
            Self::Unit { unit, .. } => Some(unit),
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::Task { task_index } => format!("translation_task_{task_index}"),
            Self::Unit { task_index, unit } => format!(
                "translation_task_{task_index}:{}:{}",
                unit.owner.as_str(),
                unit.group_kind.as_str()
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerLanguageModuleKind {
    Japanese,
    English,
}

/// 模型 HTTP 协议返回的最终结束原因。
///
/// 已知协议值使用封闭变体；供应商扩展值只作为清理后的事实保存，不能充当诊断 code。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerModelFinishReason {
    Stop,
    Length,
    ContentFilter,
    ProviderSpecific { value: SafeText },
}

impl RpgMakerModelFinishReason {
    pub(crate) fn provider_specific(value: impl AsRef<str>) -> Self {
        Self::ProviderSpecific {
            value: SafeText::new(value),
        }
    }

    pub(crate) fn non_stop(&self) -> Option<RpgMakerModelNonStopFinishReason> {
        match self {
            Self::Stop => None,
            Self::Length => Some(RpgMakerModelNonStopFinishReason::Length),
            Self::ContentFilter => Some(RpgMakerModelNonStopFinishReason::ContentFilter),
            Self::ProviderSpecific { value } => {
                Some(RpgMakerModelNonStopFinishReason::ProviderSpecific {
                    value: value.clone(),
                })
            }
        }
    }
}

/// 只允许非 stop 值进入协议诊断，避免构造与诊断名称相矛盾的状态。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerModelNonStopFinishReason {
    Length,
    ContentFilter,
    ProviderSpecific { value: SafeText },
}

impl RpgMakerModelNonStopFinishReason {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Length => "length",
            Self::ContentFilter => "content_filter",
            Self::ProviderSpecific { value } => value.as_str(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResponseLanguageProjectionProblem {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResponseInvariantProblem {
    ResponseAttemptZero,
    ExpectedOutputsEmpty,
    /// 有序翻译流水线返回了不属于当前计划位置的任务结果。
    TaskResultIndexMismatch {
        expected_task_index: usize,
        actual_task_index: usize,
    },
    /// 有序翻译流水线在当前计划位置返回结果前结束。
    TaskResultSequenceIncomplete {
        expected_task_index: usize,
    },
    LanguagePairMismatch {
        task_source: SafeIdentifier,
        task_target: SafeIdentifier,
        resolved_source: SafeIdentifier,
        resolved_target: SafeIdentifier,
    },
    RepairSegmentRangeMissing {
        line_index: usize,
        start: usize,
        end: usize,
        actual: usize,
    },
    RepairLineBoundaryMissing {
        line_index: usize,
        segment_index: usize,
        actual: usize,
    },
    RepairUnassignedSegments {
        consumed: usize,
        actual: usize,
    },
    ReservedTokenAfterRestore,
}

/// 模型响应 JSON 在共享协议边界已经确认的类别。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum RpgMakerTaskResponseJsonCategory {
    Io,
    Syntax,
    Shape,
    UnexpectedEof,
}

impl RpgMakerTaskResponseJsonCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Shape => "shape",
            Self::UnexpectedEof => "unexpected_eof",
        }
    }
}

/// 单个模型输出值不满足当前翻译协议时的封闭原因。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTaskResponseValueProblem {
    TranslationNotArray,
    TranslationNonStringItem { item: usize },
    SourceEchoNotObject,
    SourceEchoMissingSource,
    SourceEchoMissingTranslation,
    SourceEchoDuplicateSource,
    SourceEchoDuplicateTranslation,
    SourceEchoUnexpectedField,
    SourceNotArray,
    SourceNonStringItem { item: usize },
}

impl RpgMakerTaskResponseValueProblem {
    const fn code(self) -> &'static str {
        match self {
            Self::TranslationNotArray => {
                "rpg_maker.translation.response.value.translation_not_array"
            }
            Self::TranslationNonStringItem { .. } => {
                "rpg_maker.translation.response.value.translation_non_string_item"
            }
            Self::SourceEchoNotObject => {
                "rpg_maker.translation.response.value.source_echo_not_object"
            }
            Self::SourceEchoMissingSource => {
                "rpg_maker.translation.response.value.source_echo_missing_source"
            }
            Self::SourceEchoMissingTranslation => {
                "rpg_maker.translation.response.value.source_echo_missing_translation"
            }
            Self::SourceEchoDuplicateSource => {
                "rpg_maker.translation.response.value.source_echo_duplicate_source"
            }
            Self::SourceEchoDuplicateTranslation => {
                "rpg_maker.translation.response.value.source_echo_duplicate_translation"
            }
            Self::SourceEchoUnexpectedField => {
                "rpg_maker.translation.response.value.source_echo_unexpected_field"
            }
            Self::SourceNotArray => "rpg_maker.translation.response.value.source_not_array",
            Self::SourceNonStringItem { .. } => {
                "rpg_maker.translation.response.value.source_non_string_item"
            }
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

/// 一个已经定位到 RPG Maker Unit 的模型输出被验收层拒绝的原因。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTaskResponseUnitProblem {
    Missing,
    Duplicate,
    InvalidValue {
        problem: RpgMakerTaskResponseValueProblem,
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
    ContainsByteOrderMark,
    PlaceholderMismatch,
    UnexpectedPlaceholderToken,
    PlaceholderNormalizationAmbiguous,
}

/// 合法候选已经保存、但需要后续质量审核的非阻塞事实。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerTaskResponseReviewProblem {
    SourceResidual,
}

impl RpgMakerTaskResponseReviewProblem {
    const fn code(self) -> &'static str {
        match self {
            Self::SourceResidual => "rpg_maker.translation.review.unit.source_residual",
        }
    }

    const fn code_suffix(self) -> &'static str {
        match self {
            Self::SourceResidual => "source_residual",
        }
    }
}

impl RpgMakerTaskResponseUnitProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::Missing => "rpg_maker.translation.response.unit.missing",
            Self::Duplicate => "rpg_maker.translation.response.unit.duplicate",
            Self::InvalidValue { problem } => problem.code(),
            Self::LineCountMismatch { .. } => {
                "rpg_maker.translation.response.unit.line_count_mismatch"
            }
            Self::InvalidLineText { .. } => "rpg_maker.translation.response.unit.invalid_line_text",
            Self::BlankLineMismatch { .. } => {
                "rpg_maker.translation.response.unit.blank_line_mismatch"
            }
            Self::BlankTranslation => "rpg_maker.translation.response.unit.blank_translation",
            Self::ContainsByteOrderMark => {
                "rpg_maker.translation.response.unit.contains_byte_order_mark"
            }
            Self::PlaceholderMismatch => "rpg_maker.translation.response.unit.placeholder_mismatch",
            Self::UnexpectedPlaceholderToken => {
                "rpg_maker.translation.response.unit.unexpected_placeholder_token"
            }
            Self::PlaceholderNormalizationAmbiguous => {
                "rpg_maker.translation.response.unit.placeholder_normalization_ambiguous"
            }
        }
    }

    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Duplicate => "duplicate",
            Self::InvalidValue { problem } => problem.code_suffix(),
            Self::LineCountMismatch { .. } => "line_count_mismatch",
            Self::InvalidLineText { .. } => "invalid_line_text",
            Self::BlankLineMismatch { .. } => "blank_line_mismatch",
            Self::BlankTranslation => "blank_translation",
            Self::ContainsByteOrderMark => "contains_byte_order_mark",
            Self::PlaceholderMismatch => "placeholder_mismatch",
            Self::UnexpectedPlaceholderToken => "unexpected_placeholder_token",
            Self::PlaceholderNormalizationAmbiguous => "placeholder_normalization_ambiguous",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidValue { problem } => {
                let mut facts = vec![("value_problem", problem.code_suffix().to_owned())];
                facts.extend(problem.facts());
                facts
            }
            Self::LineCountMismatch { expected, actual } => vec![
                ("expected_line_count", expected.to_string()),
                ("actual_line_count", actual.to_string()),
            ],
            Self::InvalidLineText { line_index } | Self::BlankLineMismatch { line_index, .. } => {
                let mut facts = vec![("line_index", line_index.to_string())];
                if let Self::BlankLineMismatch { expected_blank, .. } = self {
                    facts.push(("expected_blank", expected_blank.to_string()));
                }
                facts
            }
            Self::Missing
            | Self::Duplicate
            | Self::BlankTranslation
            | Self::ContainsByteOrderMark
            | Self::PlaceholderMismatch
            | Self::UnexpectedPlaceholderToken
            | Self::PlaceholderNormalizationAmbiguous => Vec::new(),
        }
    }
}

/// 一次 RPG Maker TaskBlock 模型响应的可公开、可重试问题。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTaskResponseProblem {
    InvalidJson {
        category: RpgMakerTaskResponseJsonCategory,
        line: usize,
        column: usize,
    },
    ThinkingEmpty {
        line: usize,
        column: usize,
    },
    NonStopFinish {
        reason: RpgMakerModelNonStopFinishReason,
    },
    InvalidId {
        item_index: usize,
    },
    UnknownId {
        item_index: usize,
        output_id: usize,
    },
    UnitRejected {
        output_id: usize,
        problem: RpgMakerTaskResponseUnitProblem,
    },
    UnitReview {
        output_id: usize,
        finding: RpgMakerTaskResponseReviewProblem,
    },
    ModelResponseUnusable,
    AllOutputsRejected,
    MissingPlannedOutput {
        output_id: usize,
    },
}

impl RpgMakerTaskResponseProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson { .. } => "rpg_maker.translation.response.invalid_json",
            Self::ThinkingEmpty { .. } => "rpg_maker.translation.response.thinking_empty",
            Self::NonStopFinish { .. } => "rpg_maker.translation.response.non_stop_finish",
            Self::InvalidId { .. } => "rpg_maker.translation.response.invalid_id",
            Self::UnknownId { .. } => "rpg_maker.translation.response.unknown_id",
            Self::UnitRejected { problem, .. } => problem.code(),
            Self::UnitReview { finding, .. } => finding.code(),
            Self::ModelResponseUnusable => "rpg_maker.translation.response.model_response_unusable",
            Self::AllOutputsRejected => "rpg_maker.translation.response.all_outputs_rejected",
            Self::MissingPlannedOutput { .. } => {
                "rpg_maker.translation.response.missing_planned_output"
            }
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidJson { .. } => "response_parsing_failed",
            Self::NonStopFinish { .. } | Self::UnitReview { .. } => "needs_review",
            Self::ModelResponseUnusable | Self::AllOutputsRejected => "invalid_response_contract",
            Self::ThinkingEmpty { .. }
            | Self::InvalidId { .. }
            | Self::UnknownId { .. }
            | Self::UnitRejected { .. }
            | Self::MissingPlannedOutput { .. } => "invalid_response_contract",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
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
            Self::NonStopFinish { reason } => {
                vec![("finish_reason", reason.as_str().to_owned())]
            }
            Self::InvalidId { item_index } => vec![("item_index", item_index.to_string())],
            Self::UnknownId {
                item_index,
                output_id,
            } => vec![
                ("item_index", item_index.to_string()),
                ("output_id", output_id.to_string()),
            ],
            Self::UnitRejected { output_id, problem } => {
                let mut facts = vec![
                    ("output_id", output_id.to_string()),
                    ("rejection", problem.code_suffix().to_owned()),
                ];
                facts.extend(problem.facts());
                facts
            }
            Self::UnitReview { output_id, finding } => vec![
                ("output_id", output_id.to_string()),
                ("review", finding.code_suffix().to_owned()),
            ],
            Self::MissingPlannedOutput { output_id } => {
                vec![("output_id", output_id.to_string())]
            }
            Self::ModelResponseUnusable | Self::AllOutputsRejected => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResponseProcessingProblem {
    Cancelled,
    Compute {
        cause: RpgMakerBackendCause,
    },
    LanguageModuleMismatch {
        expected: RpgMakerLanguageModuleKind,
        actual: RpgMakerLanguageModuleKind,
    },
    LanguageProjection {
        problem: RpgMakerResponseLanguageProjectionProblem,
    },
    PlaceholderProtection {
        problem: PlaceholderIssue,
    },
    InternalInvariant {
        problem: RpgMakerResponseInvariantProblem,
    },
}

impl RpgMakerResponseProcessingProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "rpg_maker.translation.response.cancelled",
            Self::Compute { .. } => "rpg_maker.translation.response.compute_failed",
            Self::LanguageModuleMismatch { .. } => {
                "rpg_maker.translation.response.language_module_mismatch"
            }
            Self::LanguageProjection { problem } => match problem {
                RpgMakerResponseLanguageProjectionProblem::TokenIndexConstruction => {
                    "rpg_maker.translation.response.token_index_construction"
                }
                RpgMakerResponseLanguageProjectionProblem::EmptyToken => {
                    "rpg_maker.translation.response.empty_token"
                }
                RpgMakerResponseLanguageProjectionProblem::MissingToken => {
                    "rpg_maker.translation.response.missing_token"
                }
                RpgMakerResponseLanguageProjectionProblem::RepeatedToken => {
                    "rpg_maker.translation.response.repeated_token"
                }
                RpgMakerResponseLanguageProjectionProblem::OverlappingToken => {
                    "rpg_maker.translation.response.overlapping_token"
                }
                RpgMakerResponseLanguageProjectionProblem::ChangedTokenOrder { .. } => {
                    "rpg_maker.translation.response.changed_token_order"
                }
                RpgMakerResponseLanguageProjectionProblem::ChangedSegmentCount { .. } => {
                    "rpg_maker.translation.response.changed_segment_count"
                }
                RpgMakerResponseLanguageProjectionProblem::ChangedSegmentKind { .. } => {
                    "rpg_maker.translation.response.changed_segment_kind"
                }
                RpgMakerResponseLanguageProjectionProblem::MissingOrderedToken { .. } => {
                    "rpg_maker.translation.response.missing_ordered_token"
                }
                RpgMakerResponseLanguageProjectionProblem::UnusedOrderedToken => {
                    "rpg_maker.translation.response.unused_ordered_token"
                }
            },
            Self::PlaceholderProtection { problem } => problem.code(),
            Self::InternalInvariant { problem } => match problem {
                RpgMakerResponseInvariantProblem::ResponseAttemptZero => {
                    "rpg_maker.translation.response.attempt_zero"
                }
                RpgMakerResponseInvariantProblem::ExpectedOutputsEmpty => {
                    "rpg_maker.translation.response.expected_outputs_empty"
                }
                RpgMakerResponseInvariantProblem::TaskResultIndexMismatch { .. } => {
                    "rpg_maker.translation.response.task_result_index_mismatch"
                }
                RpgMakerResponseInvariantProblem::TaskResultSequenceIncomplete { .. } => {
                    "rpg_maker.translation.response.task_result_sequence_incomplete"
                }
                RpgMakerResponseInvariantProblem::LanguagePairMismatch { .. } => {
                    "rpg_maker.translation.response.language_pair_mismatch"
                }
                RpgMakerResponseInvariantProblem::RepairSegmentRangeMissing { .. } => {
                    "rpg_maker.translation.response.repair_segment_range_missing"
                }
                RpgMakerResponseInvariantProblem::RepairLineBoundaryMissing { .. } => {
                    "rpg_maker.translation.response.repair_line_boundary_missing"
                }
                RpgMakerResponseInvariantProblem::RepairUnassignedSegments { .. } => {
                    "rpg_maker.translation.response.repair_unassigned_segments"
                }
                RpgMakerResponseInvariantProblem::ReservedTokenAfterRestore => {
                    "rpg_maker.translation.response.reserved_token_after_restore"
                }
            },
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Compute { .. } => "external_service_unavailable",
            Self::LanguageModuleMismatch { .. }
            | Self::LanguageProjection { .. }
            | Self::InternalInvariant { .. } => "internal_invariant",
            Self::PlaceholderProtection { problem } => problem.summary_code(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Cancelled => Vec::new(),
            Self::Compute { cause } => vec![("backend_code", cause.diagnostic.code().to_owned())],
            Self::LanguageModuleMismatch { expected, actual } => vec![
                (
                    "expected_module",
                    language_module_kind_name(*expected).to_owned(),
                ),
                (
                    "actual_module",
                    language_module_kind_name(*actual).to_owned(),
                ),
            ],
            Self::LanguageProjection { problem } => response_language_projection_facts(problem),
            Self::PlaceholderProtection { problem } => problem.facts(),
            Self::InternalInvariant { problem } => response_invariant_facts(problem),
        }
    }
}

const fn language_module_kind_name(kind: RpgMakerLanguageModuleKind) -> &'static str {
    match kind {
        RpgMakerLanguageModuleKind::Japanese => "japanese",
        RpgMakerLanguageModuleKind::English => "english",
    }
}

fn response_language_projection_facts(
    problem: &RpgMakerResponseLanguageProjectionProblem,
) -> Vec<(&'static str, String)> {
    match problem {
        RpgMakerResponseLanguageProjectionProblem::TokenIndexConstruction => {
            vec![("projection_failure", "token_index_construction".to_owned())]
        }
        RpgMakerResponseLanguageProjectionProblem::EmptyToken => {
            vec![("projection_failure", "empty_token".to_owned())]
        }
        RpgMakerResponseLanguageProjectionProblem::MissingToken => {
            vec![("projection_failure", "missing_token".to_owned())]
        }
        RpgMakerResponseLanguageProjectionProblem::RepeatedToken => {
            vec![("projection_failure", "repeated_token".to_owned())]
        }
        RpgMakerResponseLanguageProjectionProblem::OverlappingToken => {
            vec![("projection_failure", "overlapping_token".to_owned())]
        }
        RpgMakerResponseLanguageProjectionProblem::ChangedTokenOrder { position } => vec![
            ("projection_failure", "changed_token_order".to_owned()),
            ("position", position.to_string()),
        ],
        RpgMakerResponseLanguageProjectionProblem::ChangedSegmentCount { expected, actual } => {
            vec![
                ("projection_failure", "changed_segment_count".to_owned()),
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ]
        }
        RpgMakerResponseLanguageProjectionProblem::ChangedSegmentKind { segment_index } => vec![
            ("projection_failure", "changed_segment_kind".to_owned()),
            ("segment_index", segment_index.to_string()),
        ],
        RpgMakerResponseLanguageProjectionProblem::MissingOrderedToken { segment_index } => vec![
            ("projection_failure", "missing_ordered_token".to_owned()),
            ("segment_index", segment_index.to_string()),
        ],
        RpgMakerResponseLanguageProjectionProblem::UnusedOrderedToken => {
            vec![("projection_failure", "unused_ordered_token".to_owned())]
        }
    }
}

fn response_invariant_facts(
    problem: &RpgMakerResponseInvariantProblem,
) -> Vec<(&'static str, String)> {
    match problem {
        RpgMakerResponseInvariantProblem::ResponseAttemptZero => {
            vec![("attempt", "0".to_owned())]
        }
        RpgMakerResponseInvariantProblem::ExpectedOutputsEmpty => {
            vec![("expected_output_count", "0".to_owned())]
        }
        RpgMakerResponseInvariantProblem::TaskResultIndexMismatch {
            expected_task_index,
            actual_task_index,
        } => vec![
            ("expected_task_index", expected_task_index.to_string()),
            ("actual_task_index", actual_task_index.to_string()),
        ],
        RpgMakerResponseInvariantProblem::TaskResultSequenceIncomplete {
            expected_task_index,
        } => vec![("expected_task_index", expected_task_index.to_string())],
        RpgMakerResponseInvariantProblem::LanguagePairMismatch {
            task_source,
            task_target,
            resolved_source,
            resolved_target,
        } => vec![
            ("task_source", task_source.to_string()),
            ("task_target", task_target.to_string()),
            ("resolved_source", resolved_source.to_string()),
            ("resolved_target", resolved_target.to_string()),
        ],
        RpgMakerResponseInvariantProblem::RepairSegmentRangeMissing {
            line_index,
            start,
            end,
            actual,
        } => vec![
            ("line_index", line_index.to_string()),
            ("start", start.to_string()),
            ("end", end.to_string()),
            ("actual", actual.to_string()),
        ],
        RpgMakerResponseInvariantProblem::RepairLineBoundaryMissing {
            line_index,
            segment_index,
            actual,
        } => vec![
            ("line_index", line_index.to_string()),
            ("segment_index", segment_index.to_string()),
            ("actual", actual.to_string()),
        ],
        RpgMakerResponseInvariantProblem::RepairUnassignedSegments { consumed, actual } => vec![
            ("consumed", consumed.to_string()),
            ("actual", actual.to_string()),
        ],
        RpgMakerResponseInvariantProblem::ReservedTokenAfterRestore => Vec::new(),
    }
}

impl RpgMakerDialogueProjectionProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::Match { .. } => "rpg_maker.dialogue.pattern_match_failed",
            Self::ZeroWidthMatch { .. } => "rpg_maker.dialogue.zero_width_match",
            Self::MissingSpeakerCapture { .. } => "rpg_maker.dialogue.speaker_capture_missing",
            Self::InvalidSpeakerCaptureRange { .. } => {
                "rpg_maker.dialogue.speaker_capture_range_invalid"
            }
            Self::MultipleRulesOwnField { .. } => "rpg_maker.dialogue.multiple_rules_own_field",
            Self::DifferentSpeakers { .. } => "rpg_maker.dialogue.different_speakers",
            Self::RuleCapturedNoSpeaker { .. } => "rpg_maker.dialogue.no_speaker_captured",
            Self::InvalidRecipe { .. } => "rpg_maker.dialogue.invalid_recipe",
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::MissingSpeakerCapture { .. } | Self::RuleCapturedNoSpeaker { .. } => {
                "missing_required_value"
            }
            Self::MultipleRulesOwnField { .. } | Self::DifferentSpeakers { .. } => {
                "conflicting_values"
            }
            Self::InvalidRecipe { .. } => "internal_invariant",
            Self::Match { .. }
            | Self::ZeroWidthMatch { .. }
            | Self::InvalidSpeakerCaptureRange { .. } => "invalid_value",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerEngineKind {
    Mv,
    Mz,
}

impl RpgMakerEngineKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mv => "mv",
            Self::Mz => "mz",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerInitialSetting {
    SourceLanguage,
    TargetLanguage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRunPlanValueViolation {
    EmptyPath,
    RelativePath,
    PathContainsNul,
    InvalidWindowsPathEncoding,
    EmptyExtractOwners,
    InvalidRulesJson {
        line: u64,
        column: u64,
        category: SafeIdentifier,
    },
    RulesJsonNotArray,
    RulesJsonEncodingFailed {
        line: u64,
        column: u64,
        category: SafeIdentifier,
    },
    InvalidRulesSemantics,
    NonCanonicalRulesJson,
    EmptyRulesDefinition,
    EmptyProfileId,
    ProfileIdOuterWhitespace,
    UnsafeProfileId,
}

impl RpgMakerRunPlanValueViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::EmptyPath => "empty_path",
            Self::RelativePath => "relative_path",
            Self::PathContainsNul => "path_contains_nul",
            Self::InvalidWindowsPathEncoding => "invalid_windows_path_encoding",
            Self::EmptyExtractOwners => "empty_extract_owners",
            Self::InvalidRulesJson { .. } => "invalid_rules_json",
            Self::RulesJsonNotArray => "rules_json_not_array",
            Self::RulesJsonEncodingFailed { .. } => "rules_json_encoding_failed",
            Self::InvalidRulesSemantics => "invalid_rules_semantics",
            Self::NonCanonicalRulesJson => "noncanonical_rules_json",
            Self::EmptyRulesDefinition => "empty_rules_definition",
            Self::EmptyProfileId => "empty_profile_id",
            Self::ProfileIdOuterWhitespace => "profile_id_outer_whitespace",
            Self::UnsafeProfileId => "unsafe_profile_id",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerRunPlanExpectedSqliteType {
    BlobOrNull,
    Integer,
    TextOrNull,
}

impl RpgMakerRunPlanExpectedSqliteType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BlobOrNull => "blob_or_null",
            Self::Integer => "integer",
            Self::TextOrNull => "text_or_null",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerRunPlanSqliteType {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl RpgMakerRunPlanSqliteType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Integer => "integer",
            Self::Real => "real",
            Self::Text => "text",
            Self::Blob => "blob",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerRunPlanSnapshotViolation {
    UnexpectedResultSets {
        expected: u64,
        actual: u64,
    },
    UnexpectedRows {
        expected: u64,
        actual: u64,
    },
    UnexpectedColumns {
        expected: u64,
        actual: u64,
    },
    WrongValueType {
        field: SafeIdentifier,
        expected: RpgMakerRunPlanExpectedSqliteType,
        actual: RpgMakerRunPlanSqliteType,
    },
    InvalidBoolean {
        field: SafeIdentifier,
        actual: i64,
    },
    RulesDefinitionMismatch {
        rules_enabled: bool,
        definition_present: bool,
    },
}

impl RpgMakerRunPlanSnapshotViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::UnexpectedResultSets { .. } => "unexpected_result_sets",
            Self::UnexpectedRows { .. } => "unexpected_rows",
            Self::UnexpectedColumns { .. } => "unexpected_columns",
            Self::WrongValueType { .. } => "wrong_value_type",
            Self::InvalidBoolean { .. } => "invalid_boolean",
            Self::RulesDefinitionMismatch { .. } => "rules_definition_mismatch",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("violation", self.as_str().to_owned())];
        match self {
            Self::UnexpectedResultSets { expected, actual }
            | Self::UnexpectedRows { expected, actual }
            | Self::UnexpectedColumns { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::WrongValueType {
                field,
                expected,
                actual,
            } => {
                facts.push(("field", field.to_string()));
                facts.push(("expected_type", expected.as_str().to_owned()));
                facts.push(("actual_type", actual.as_str().to_owned()));
            }
            Self::InvalidBoolean { field, actual } => {
                facts.push(("field", field.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::RulesDefinitionMismatch {
                rules_enabled,
                definition_present,
            } => {
                facts.push(("rules_enabled", rules_enabled.to_string()));
                facts.push(("definition_present", definition_present.to_string()));
            }
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerLanguageIdViolation {
    Blank,
    SurroundingWhitespace,
    UnderscoreSeparator,
    InvalidRfc5646Syntax,
    InvalidIanaRegistryTag,
    CanonicalizationFailed,
    UndefinedPrimaryLanguage,
}

impl RpgMakerLanguageIdViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::SurroundingWhitespace => "surrounding_whitespace",
            Self::UnderscoreSeparator => "underscore_separator",
            Self::InvalidRfc5646Syntax => "invalid_rfc5646_syntax",
            Self::InvalidIanaRegistryTag => "invalid_iana_registry_tag",
            Self::CanonicalizationFailed => "canonicalization_failed",
            Self::UndefinedPrimaryLanguage => "undefined_primary_language",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectDefinitionStage {
    Decode,
    Compile,
    Encode,
}

impl RpgMakerProjectDefinitionStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Compile => "compile",
            Self::Encode => "encode",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectDefinitionViolation {
    EmptyDocument,
    MissingRuleArray,
    InvalidToml {
        byte_start: Option<u64>,
        byte_end: Option<u64>,
    },
    InvalidJson {
        category: SafeIdentifier,
        line: u64,
        column: u64,
    },
    EncodeJson {
        category: SafeIdentifier,
        line: u64,
        column: u64,
    },
    EmptyPattern {
        rule_number: u64,
    },
    InvalidPattern {
        rule_number: u64,
        category: SafeIdentifier,
        backend_code: i32,
        offset: Option<u64>,
    },
    InvalidNamedCaptures {
        rule_number: u64,
        actual_count: u64,
    },
}

impl RpgMakerProjectDefinitionViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::EmptyDocument => "empty_document",
            Self::MissingRuleArray => "missing_rule_array",
            Self::InvalidToml { .. } => "invalid_toml",
            Self::InvalidJson { .. } => "invalid_json",
            Self::EncodeJson { .. } => "encode_json",
            Self::EmptyPattern { .. } => "empty_pattern",
            Self::InvalidPattern { .. } => "invalid_pattern",
            Self::InvalidNamedCaptures { .. } => "invalid_named_captures",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("definition_failure", self.as_str().to_owned())];
        match self {
            Self::InvalidToml {
                byte_start,
                byte_end,
            } => {
                if let Some(byte_start) = byte_start {
                    facts.push(("byte_start", byte_start.to_string()));
                }
                if let Some(byte_end) = byte_end {
                    facts.push(("byte_end", byte_end.to_string()));
                }
            }
            Self::InvalidJson {
                category,
                line,
                column,
            }
            | Self::EncodeJson {
                category,
                line,
                column,
            } => {
                facts.push(("json_category", category.to_string()));
                facts.push(("line", line.to_string()));
                facts.push(("column", column.to_string()));
            }
            Self::EmptyPattern { rule_number } => {
                facts.push(("rule_number", rule_number.to_string()));
            }
            Self::InvalidPattern {
                rule_number,
                category,
                backend_code,
                offset,
            } => {
                facts.push(("rule_number", rule_number.to_string()));
                facts.push(("pcre2_category", category.to_string()));
                facts.push(("backend_code", backend_code.to_string()));
                if let Some(offset) = offset {
                    facts.push(("offset", offset.to_string()));
                }
            }
            Self::InvalidNamedCaptures {
                rule_number,
                actual_count,
            } => {
                facts.push(("rule_number", rule_number.to_string()));
                facts.push(("actual_count", actual_count.to_string()));
            }
            Self::EmptyDocument | Self::MissingRuleArray => {}
        }
        facts
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectMetadataViolation {
    MissingRow,
    MultipleRows,
    WrongColumnCount {
        expected: u64,
        actual: u64,
    },
    WrongColumnType {
        column: SafeIdentifier,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    InvalidProjectName,
    NameMismatch {
        requested: SafeText,
        stored: SafeText,
    },
    InvalidLanguage {
        column: SafeIdentifier,
        violation: RpgMakerLanguageIdViolation,
    },
    NonCanonicalLanguage {
        column: SafeIdentifier,
        stored: SafeText,
        canonical: SafeText,
    },
    InvalidSourceSnapshotFingerprintLength {
        expected: u64,
        actual: u64,
    },
    InvalidDialogueDefinition {
        stage: RpgMakerProjectDefinitionStage,
        violation: RpgMakerProjectDefinitionViolation,
    },
}

impl RpgMakerProjectMetadataViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::MissingRow => "rpg_maker.project.metadata.missing_row",
            Self::MultipleRows => "rpg_maker.project.metadata.multiple_rows",
            Self::WrongColumnCount { .. } => "rpg_maker.project.metadata.wrong_column_count",
            Self::WrongColumnType { .. } => "rpg_maker.project.metadata.wrong_column_type",
            Self::InvalidProjectName => "rpg_maker.project.metadata.invalid_project_name",
            Self::NameMismatch { .. } => "rpg_maker.project.metadata.name_mismatch",
            Self::InvalidLanguage { .. } => "rpg_maker.project.metadata.invalid_language",
            Self::NonCanonicalLanguage { .. } => "rpg_maker.project.metadata.noncanonical_language",
            Self::InvalidSourceSnapshotFingerprintLength { .. } => {
                "rpg_maker.project.metadata.invalid_source_snapshot_fingerprint_length"
            }
            Self::InvalidDialogueDefinition { .. } => {
                "rpg_maker.project.metadata.invalid_dialogue_definition"
            }
        }
    }

    const fn as_str(&self) -> &'static str {
        match self {
            Self::MissingRow => "missing_row",
            Self::MultipleRows => "multiple_rows",
            Self::WrongColumnCount { .. } => "wrong_column_count",
            Self::WrongColumnType { .. } => "wrong_column_type",
            Self::InvalidProjectName => "invalid_project_name",
            Self::NameMismatch { .. } => "name_mismatch",
            Self::InvalidLanguage { .. } => "invalid_language",
            Self::NonCanonicalLanguage { .. } => "noncanonical_language",
            Self::InvalidSourceSnapshotFingerprintLength { .. } => {
                "invalid_source_snapshot_fingerprint_length"
            }
            Self::InvalidDialogueDefinition { .. } => "invalid_dialogue_definition",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("metadata_violation", self.as_str().to_owned())];
        match self {
            Self::WrongColumnCount { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => {
                facts.push(("column", column.to_string()));
                facts.push(("expected_type", expected.to_string()));
                facts.push(("actual_type", actual.to_string()));
            }
            Self::NameMismatch { requested, stored } => {
                facts.push(("requested", requested.to_string()));
                facts.push(("stored", stored.to_string()));
            }
            Self::InvalidLanguage { column, violation } => {
                facts.push(("column", column.to_string()));
                facts.push(("language_failure", violation.as_str().to_owned()));
            }
            Self::NonCanonicalLanguage {
                column,
                stored,
                canonical,
            } => {
                facts.push(("column", column.to_string()));
                facts.push(("stored", stored.to_string()));
                facts.push(("canonical", canonical.to_string()));
            }
            Self::InvalidSourceSnapshotFingerprintLength { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::InvalidDialogueDefinition { stage, violation } => {
                facts.push(("definition", "mv_dialogue_rules".to_owned()));
                facts.push(("definition_stage", stage.as_str().to_owned()));
                facts.extend(violation.facts());
            }
            Self::MissingRow | Self::MultipleRows | Self::InvalidProjectName => {}
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerStorageCodecOperation {
    Encode,
    Decode,
}

impl RpgMakerStorageCodecOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Encode => "encode",
            Self::Decode => "decode",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerLocationCodecFailure {
    Json {
        operation: RpgMakerStorageCodecOperation,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    NonCanonical,
    InvalidDataFile,
    InvalidMapId {
        map_id: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectionModelViolation {
    EmptyScalarFieldKey,
    EventBlockCoverageRequired,
    InvalidEventBlockCoverage,
    MutationClaimTargetMismatch,
    RecipeHasNoTextSlot,
    DuplicateProjectionSlot {
        role: RpgMakerDiagnosticRole,
        source_line_index: Option<usize>,
    },
    MultipleBodyLinesInPhysicalLine,
    DuplicateDialogueBodyLine {
        source_line_index: usize,
    },
    NonContiguousDialogueBodyLines {
        expected: usize,
        actual: usize,
    },
    MixedDirectAndInlineSpeaker,
}

impl RpgMakerProjectionModelViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::EmptyScalarFieldKey => "empty_scalar_field_key",
            Self::EventBlockCoverageRequired => "event_block_coverage_required",
            Self::InvalidEventBlockCoverage => "invalid_event_block_coverage",
            Self::MutationClaimTargetMismatch => "mutation_claim_target_mismatch",
            Self::RecipeHasNoTextSlot => "recipe_has_no_text_slot",
            Self::DuplicateProjectionSlot { .. } => "duplicate_projection_slot",
            Self::MultipleBodyLinesInPhysicalLine => "multiple_body_lines_in_physical_line",
            Self::DuplicateDialogueBodyLine { .. } => "duplicate_dialogue_body_line",
            Self::NonContiguousDialogueBodyLines { .. } => "noncontiguous_dialogue_body_lines",
            Self::MixedDirectAndInlineSpeaker => "mixed_direct_and_inline_speaker",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectionCodecFailure {
    Json {
        operation: RpgMakerStorageCodecOperation,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    NonCanonical,
    Location {
        failure: Box<RpgMakerLocationCodecFailure>,
    },
    Projection {
        violation: RpgMakerProjectionModelViolation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResultStorePlanViolation {
    LocationCodec {
        failure: RpgMakerLocationCodecFailure,
    },
    ProjectionCodec {
        failure: RpgMakerProjectionCodecFailure,
    },
    ContentJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    EmptyTaskResult,
    EmptyReuseTargets,
    BlankTranslation,
    InconsistentTranslationState,
    MismatchedReuseSourceContent,
    MismatchedReuseSourceContext,
    MismatchedPropagationSourceContent,
    MismatchedPropagationSourceContext,
    DuplicateUnit,
    InvalidCommitDecisionSequence,
    MissingCommitDecisionUnit,
}

impl RpgMakerResultStorePlanViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::LocationCodec { .. } => "location_codec",
            Self::ProjectionCodec { .. } => "projection_codec",
            Self::ContentJson { .. } => "content_json",
            Self::EmptyTaskResult => "empty_task_result",
            Self::EmptyReuseTargets => "empty_reuse_targets",
            Self::BlankTranslation => "blank_translation",
            Self::InconsistentTranslationState => "inconsistent_translation_state",
            Self::MismatchedReuseSourceContent => "mismatched_reuse_source_content",
            Self::MismatchedReuseSourceContext => "mismatched_reuse_source_context",
            Self::MismatchedPropagationSourceContent => "mismatched_propagation_source_content",
            Self::MismatchedPropagationSourceContext => "mismatched_propagation_source_context",
            Self::DuplicateUnit => "duplicate_unit",
            Self::InvalidCommitDecisionSequence => "invalid_commit_decision_sequence",
            Self::MissingCommitDecisionUnit => "missing_commit_decision_unit",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerResultStoreProblem {
    InvalidPlan {
        violation: RpgMakerResultStorePlanViolation,
    },
    DatabaseNotFound {
        path: SafePath,
    },
    StalePlan {
        path: SafePath,
    },
    SessionDatabaseChanged {
        opened_path: SafePath,
        requested_path: SafePath,
    },
    SessionFinalized {
        path: SafePath,
    },
    FinalizationRolledBackTransaction {
        path: SafePath,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerDiagnosticScope {
    StandardDatabase { file: SafeIdentifier },
    DataFile { path: SafePath },
    System,
    Map { map_id: u64 },
    CommonEvent { event_id: usize },
    Troop { troop_id: usize },
    Plugin { plugin_index: usize },
}

impl RpgMakerDiagnosticScope {
    fn fact_value(&self) -> String {
        match self {
            Self::StandardDatabase { file } => format!("data/{file}"),
            Self::DataFile { path } => format!("data/{path}"),
            Self::System => "data/System.json".to_owned(),
            Self::Map { map_id } => format!("map:{map_id}"),
            Self::CommonEvent { event_id } => format!("common_event:{event_id}"),
            Self::Troop { troop_id } => format!("troop:{troop_id}"),
            Self::Plugin { plugin_index } => format!("plugin_index:{plugin_index}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerPlaceholderMultisetViolation {
    Mismatch,
    Unexpected,
    OrderMismatch,
}

impl RpgMakerPlaceholderMultisetViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mismatch => "mismatch",
            Self::Unexpected => "unexpected",
            Self::OrderMismatch => "order_mismatch",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerOutputContractViolation {
    PropagationContextCountMismatch {
        target_count: usize,
        context_count: usize,
    },
    PlaceholderIndexInvalid {
        failure: RpgMakerPlaceholderProjectionProblem,
    },
    ProtectedPlaceholderMultisetMismatch {
        violation: RpgMakerPlaceholderMultisetViolation,
    },
    ProtectedPlaceholderCrossesLineBoundary {
        placeholder_index: usize,
    },
    ProtectedLineCountMismatch {
        expected: usize,
        actual: usize,
    },
    ScalarAlignedCountInvalid {
        actual: usize,
    },
    LinesAlignedCountMismatch {
        expected: usize,
        actual: usize,
    },
}

impl RpgMakerOutputContractViolation {
    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::PropagationContextCountMismatch { .. } => "propagation_context_count_mismatch",
            Self::PlaceholderIndexInvalid { .. } => "placeholder_index_invalid",
            Self::ProtectedPlaceholderMultisetMismatch { .. } => {
                "protected_placeholder_multiset_mismatch"
            }
            Self::ProtectedPlaceholderCrossesLineBoundary { .. } => {
                "protected_placeholder_crosses_line_boundary"
            }
            Self::ProtectedLineCountMismatch { .. } => "protected_line_count_mismatch",
            Self::ScalarAlignedCountInvalid { .. } => "scalar_aligned_count_invalid",
            Self::LinesAlignedCountMismatch { .. } => "lines_aligned_count_mismatch",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::PropagationContextCountMismatch {
                target_count,
                context_count,
            } => vec![
                ("target_count", target_count.to_string()),
                ("context_count", context_count.to_string()),
            ],
            Self::PlaceholderIndexInvalid { failure } => failure.facts(),
            Self::ProtectedPlaceholderMultisetMismatch { violation } => {
                vec![("multiset_violation", violation.as_str().to_owned())]
            }
            Self::ProtectedPlaceholderCrossesLineBoundary { placeholder_index } => {
                vec![("placeholder_index", placeholder_index.to_string())]
            }
            Self::ProtectedLineCountMismatch { expected, actual }
            | Self::LinesAlignedCountMismatch { expected, actual } => vec![
                ("expected", expected.to_string()),
                ("actual", actual.to_string()),
            ],
            Self::ScalarAlignedCountInvalid { actual } => {
                vec![("expected", "1".to_owned()), ("actual", actual.to_string())]
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTranslationPlanningProblem {
    LanguagePairMismatch {
        project_source: Option<SafeIdentifier>,
        project_target: Option<SafeIdentifier>,
        resolved_source: Option<SafeIdentifier>,
        resolved_target: Option<SafeIdentifier>,
    },
    EmptySemanticScope {
        scope: RpgMakerDiagnosticScope,
    },
    EmptyGroup {
        scope: RpgMakerDiagnosticScope,
        group_kind: RpgMakerDiagnosticGroupKind,
    },
    ScopeLocationCodec {
        scope: RpgMakerDiagnosticScope,
        failure: RpgMakerLocationCodecFailure,
    },
    ScopeProjectionCodec {
        scope: RpgMakerDiagnosticScope,
        failure: RpgMakerProjectionCodecFailure,
    },
    ScopeSemanticOrderLengthOverflow {
        scope: RpgMakerDiagnosticScope,
    },
    OutputContract {
        task_id: usize,
        unit: RpgMakerUnitLocator,
        violation: RpgMakerOutputContractViolation,
    },
}

impl RpgMakerTranslationPlanningProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::LanguagePairMismatch { .. } => {
                "rpg_maker.translate.planning.language_pair_mismatch"
            }
            Self::EmptySemanticScope { .. } => "rpg_maker.translate.planning.empty_semantic_scope",
            Self::EmptyGroup { .. } => "rpg_maker.translate.planning.empty_group",
            Self::ScopeLocationCodec { .. } => "rpg_maker.translate.planning.scope_location_codec",
            Self::ScopeProjectionCodec { .. } => {
                "rpg_maker.translate.planning.scope_projection_codec"
            }
            Self::ScopeSemanticOrderLengthOverflow { .. } => {
                "rpg_maker.translate.planning.scope_semantic_order_length_overflow"
            }
            Self::OutputContract { violation, .. } => match violation {
                RpgMakerOutputContractViolation::PropagationContextCountMismatch { .. } => {
                    "rpg_maker.translate.output_contract.propagation_context_count_mismatch"
                }
                RpgMakerOutputContractViolation::PlaceholderIndexInvalid { .. } => {
                    "rpg_maker.translate.output_contract.placeholder_index_invalid"
                }
                RpgMakerOutputContractViolation::ProtectedPlaceholderMultisetMismatch {
                    ..
                } => "rpg_maker.translate.output_contract.protected_placeholder_multiset_mismatch",
                RpgMakerOutputContractViolation::ProtectedPlaceholderCrossesLineBoundary {
                    ..
                } => {
                    "rpg_maker.translate.output_contract.protected_placeholder_crosses_line_boundary"
                }
                RpgMakerOutputContractViolation::ProtectedLineCountMismatch { .. } => {
                    "rpg_maker.translate.output_contract.protected_line_count_mismatch"
                }
                RpgMakerOutputContractViolation::ScalarAlignedCountInvalid { .. } => {
                    "rpg_maker.translate.output_contract.scalar_aligned_count_invalid"
                }
                RpgMakerOutputContractViolation::LinesAlignedCountMismatch { .. } => {
                    "rpg_maker.translate.output_contract.lines_aligned_count_mismatch"
                }
            },
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::LanguagePairMismatch { .. } => DiagnosticResolution::FixConfiguration,
            Self::EmptySemanticScope { .. } | Self::EmptyGroup { .. } => {
                DiagnosticResolution::CheckProjectState
            }
            Self::ScopeLocationCodec { .. }
            | Self::ScopeProjectionCodec { .. }
            | Self::ScopeSemanticOrderLengthOverflow { .. }
            | Self::OutputContract { .. } => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::LanguagePairMismatch { .. } => "conflicting_values",
            Self::EmptySemanticScope { .. } | Self::EmptyGroup { .. } => "state_mismatch",
            _ => "internal_invariant",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::LanguagePairMismatch {
                project_source,
                project_target,
                resolved_source,
                resolved_target,
            } => [
                ("project_source", project_source),
                ("project_target", project_target),
                ("resolved_source", resolved_source),
                ("resolved_target", resolved_target),
            ]
            .into_iter()
            .filter_map(|(name, value)| value.as_ref().map(|value| (name, value.to_string())))
            .collect(),
            Self::EmptySemanticScope { scope } => {
                vec![("scope", scope.fact_value())]
            }
            Self::EmptyGroup { scope, group_kind } => vec![
                ("scope", scope.fact_value()),
                ("group_kind", group_kind.as_str().to_owned()),
            ],
            Self::ScopeLocationCodec { scope, failure } => {
                let mut facts = vec![("scope", scope.fact_value())];
                append_location_codec_facts(&mut facts, failure);
                facts
            }
            Self::ScopeProjectionCodec { scope, failure } => {
                let mut facts = vec![("scope", scope.fact_value())];
                append_projection_codec_facts(&mut facts, failure);
                facts
            }
            Self::ScopeSemanticOrderLengthOverflow { scope } => {
                vec![("scope", scope.fact_value())]
            }
            Self::OutputContract {
                task_id, violation, ..
            } => {
                let mut facts = vec![
                    ("task_id", task_id.to_string()),
                    ("contract_violation", violation.code_suffix().to_owned()),
                ];
                facts.extend(violation.facts());
                facts
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerSemanticOrderKeyViolation {
    Truncated,
    UnknownMarker { actual: u8 },
    TrailingBytes,
}

impl RpgMakerSemanticOrderKeyViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::UnknownMarker { .. } => "unknown_marker",
            Self::TrailingBytes => "trailing_bytes",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerTranslationResourceKind {
    Terminology,
    PlaceholderRules,
}

impl RpgMakerTranslationResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Terminology => "terminology",
            Self::PlaceholderRules => "placeholder_rules",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerSemanticOrderLevel {
    Group,
    Unit,
}

impl RpgMakerSemanticOrderLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Unit => "unit",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTranslationSnapshotViolation {
    WrongQueryResultSetCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: SafeIdentifier,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    InvalidSemanticOrderKey {
        column: SafeIdentifier,
        violation: RpgMakerSemanticOrderKeyViolation,
    },
    UnknownOwner,
    InactiveOwner {
        owner: RpgMakerDiagnosticOwner,
    },
    DuplicateOwner {
        owner: RpgMakerDiagnosticOwner,
    },
    InvalidOwnerSourceFingerprintLength {
        owner: RpgMakerDiagnosticOwner,
        actual: usize,
    },
    InvalidOwnerAssetFingerprintLength {
        owner: RpgMakerDiagnosticOwner,
        actual: usize,
    },
    InvalidMetadataRowCount {
        actual: usize,
    },
    InvalidMetadataFingerprintLength {
        actual: usize,
    },
    MissingTranslationResource {
        resource_kind: RpgMakerTranslationResourceKind,
    },
    DuplicateTranslationResource {
        resource_kind: RpgMakerTranslationResourceKind,
    },
    UnknownTranslationResource,
    BlankTranslationResource {
        resource_kind: RpgMakerTranslationResourceKind,
    },
    UnknownGroupKind,
    InvalidLocation {
        failure: RpgMakerLocationCodecFailure,
    },
    InvalidSemanticScope {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidRole {
        failure: RpgMakerProjectionCodecFailure,
    },
    RoleDoesNotBelongToGroup {
        role: RpgMakerDiagnosticRole,
        group_kind: RpgMakerDiagnosticGroupKind,
    },
    InvalidSourceContent {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    InvalidTranslationContent {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    SourceContentShapeMismatch {
        role: RpgMakerDiagnosticRole,
    },
    TranslationContentShapeMismatch {
        role: RpgMakerDiagnosticRole,
    },
    BlankSourceContent,
    BlankTranslationContent,
    InvalidSourceLineText {
        index: usize,
    },
    InvalidTranslationLineText {
        index: usize,
    },
    AlignedLineCountMismatch {
        expected: usize,
        actual: usize,
    },
    AlignedBlankSlotMismatch {
        index: usize,
    },
    InvalidSourceContext {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    SourceContextMustBeObject,
    InvalidTranslationStatePair,
    InvalidTranslationStateLength {
        actual: usize,
    },
    DuplicateGroup {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    MissingGroup {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    EmptyGroup {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    InconsistentGroupDefinition {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    DuplicateSemanticOrderKey {
        level: RpgMakerSemanticOrderLevel,
    },
    DuplicateLogicalUnit {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerTranslationAssetProblem {
    ProjectSnapshotChanged {
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    ExtractionOutOfDate {
        owners: Vec<RpgMakerDiagnosticOwner>,
    },
    InvalidSnapshot {
        violation: RpgMakerTranslationSnapshotViolation,
    },
    Compute {
        operation: RpgMakerTranslationAssetComputeOperation,
        failure: RpgMakerComputeFailure,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerTranslationAssetComputeOperation {
    PrepareSnapshot,
    DecodeUnits,
    AssembleCorpus,
}

impl RpgMakerTranslationAssetComputeOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PrepareSnapshot => "prepare_snapshot",
            Self::DecodeUnits => "decode_units",
            Self::AssembleCorpus => "assemble_corpus",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerComputeFailure {
    Cancelled,
    ExecutorClosed,
    StatePoisoned,
    WorkerPanicked,
}

impl RpgMakerComputeFailure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::ExecutorClosed => "executor_closed",
            Self::StatePoisoned => "state_poisoned",
            Self::WorkerPanicked => "worker_panicked",
        }
    }
}

impl RpgMakerTranslationSnapshotViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::WrongQueryResultSetCount { .. } => {
                "rpg_maker.translate.asset_snapshot.wrong_query_result_set_count"
            }
            Self::WrongColumnCount { .. } => {
                "rpg_maker.translate.asset_snapshot.wrong_column_count"
            }
            Self::WrongColumnType { .. } => "rpg_maker.translate.asset_snapshot.wrong_column_type",
            Self::InvalidSemanticOrderKey { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_semantic_order_key"
            }
            Self::UnknownOwner => "rpg_maker.translate.asset_snapshot.unknown_owner",
            Self::InactiveOwner { .. } => "rpg_maker.translate.asset_snapshot.inactive_owner",
            Self::DuplicateOwner { .. } => "rpg_maker.translate.asset_snapshot.duplicate_owner",
            Self::InvalidOwnerSourceFingerprintLength { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_owner_source_fingerprint_length"
            }
            Self::InvalidOwnerAssetFingerprintLength { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_owner_asset_fingerprint_length"
            }
            Self::InvalidMetadataRowCount { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_metadata_row_count"
            }
            Self::InvalidMetadataFingerprintLength { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_metadata_fingerprint_length"
            }
            Self::MissingTranslationResource { .. } => {
                "rpg_maker.translate.asset_snapshot.missing_translation_resource"
            }
            Self::DuplicateTranslationResource { .. } => {
                "rpg_maker.translate.asset_snapshot.duplicate_translation_resource"
            }
            Self::UnknownTranslationResource => {
                "rpg_maker.translate.asset_snapshot.unknown_translation_resource"
            }
            Self::BlankTranslationResource { .. } => {
                "rpg_maker.translate.asset_snapshot.blank_translation_resource"
            }
            Self::UnknownGroupKind => "rpg_maker.translate.asset_snapshot.unknown_group_kind",
            Self::InvalidLocation { .. } => "rpg_maker.translate.asset_snapshot.invalid_location",
            Self::InvalidSemanticScope { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_semantic_scope"
            }
            Self::InvalidRole { .. } => "rpg_maker.translate.asset_snapshot.invalid_role",
            Self::RoleDoesNotBelongToGroup { .. } => {
                "rpg_maker.translate.asset_snapshot.role_group_mismatch"
            }
            Self::InvalidSourceContent { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_source_content"
            }
            Self::InvalidTranslationContent { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_translation_content"
            }
            Self::SourceContentShapeMismatch { .. } => {
                "rpg_maker.translate.asset_snapshot.source_content_shape_mismatch"
            }
            Self::TranslationContentShapeMismatch { .. } => {
                "rpg_maker.translate.asset_snapshot.translation_content_shape_mismatch"
            }
            Self::BlankSourceContent => "rpg_maker.translate.asset_snapshot.blank_source_content",
            Self::BlankTranslationContent => {
                "rpg_maker.translate.asset_snapshot.blank_translation_content"
            }
            Self::InvalidSourceLineText { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_source_line_text"
            }
            Self::InvalidTranslationLineText { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_translation_line_text"
            }
            Self::AlignedLineCountMismatch { .. } => {
                "rpg_maker.translate.asset_snapshot.aligned_line_count_mismatch"
            }
            Self::AlignedBlankSlotMismatch { .. } => {
                "rpg_maker.translate.asset_snapshot.aligned_blank_slot_mismatch"
            }
            Self::InvalidSourceContext { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_source_context"
            }
            Self::SourceContextMustBeObject => {
                "rpg_maker.translate.asset_snapshot.source_context_not_object"
            }
            Self::InvalidTranslationStatePair => {
                "rpg_maker.translate.asset_snapshot.invalid_translation_state_pair"
            }
            Self::InvalidTranslationStateLength { .. } => {
                "rpg_maker.translate.asset_snapshot.invalid_translation_state_length"
            }
            Self::DuplicateGroup { .. } => "rpg_maker.translate.asset_snapshot.duplicate_group",
            Self::MissingGroup { .. } => "rpg_maker.translate.asset_snapshot.missing_group",
            Self::EmptyGroup { .. } => "rpg_maker.translate.asset_snapshot.empty_group",
            Self::InconsistentGroupDefinition { .. } => {
                "rpg_maker.translate.asset_snapshot.inconsistent_group_definition"
            }
            Self::DuplicateSemanticOrderKey { .. } => {
                "rpg_maker.translate.asset_snapshot.duplicate_semantic_order_key"
            }
            Self::DuplicateLogicalUnit { .. } => {
                "rpg_maker.translate.asset_snapshot.duplicate_logical_unit"
            }
        }
    }

    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::WrongQueryResultSetCount { .. } => "wrong_query_result_set_count",
            Self::WrongColumnCount { .. } => "wrong_column_count",
            Self::WrongColumnType { .. } => "wrong_column_type",
            Self::InvalidSemanticOrderKey { .. } => "invalid_semantic_order_key",
            Self::UnknownOwner => "unknown_owner",
            Self::InactiveOwner { .. } => "inactive_owner",
            Self::DuplicateOwner { .. } => "duplicate_owner",
            Self::InvalidOwnerSourceFingerprintLength { .. } => {
                "invalid_owner_source_fingerprint_length"
            }
            Self::InvalidOwnerAssetFingerprintLength { .. } => {
                "invalid_owner_asset_fingerprint_length"
            }
            Self::InvalidMetadataRowCount { .. } => "invalid_metadata_row_count",
            Self::InvalidMetadataFingerprintLength { .. } => "invalid_metadata_fingerprint_length",
            Self::MissingTranslationResource { .. } => "missing_translation_resource",
            Self::DuplicateTranslationResource { .. } => "duplicate_translation_resource",
            Self::UnknownTranslationResource => "unknown_translation_resource",
            Self::BlankTranslationResource { .. } => "blank_translation_resource",
            Self::UnknownGroupKind => "unknown_group_kind",
            Self::InvalidLocation { .. } => "invalid_location",
            Self::InvalidSemanticScope { .. } => "invalid_semantic_scope",
            Self::InvalidRole { .. } => "invalid_role",
            Self::RoleDoesNotBelongToGroup { .. } => "role_group_mismatch",
            Self::InvalidSourceContent { .. } => "invalid_source_content",
            Self::InvalidTranslationContent { .. } => "invalid_translation_content",
            Self::SourceContentShapeMismatch { .. } => "source_content_shape_mismatch",
            Self::TranslationContentShapeMismatch { .. } => "translation_content_shape_mismatch",
            Self::BlankSourceContent => "blank_source_content",
            Self::BlankTranslationContent => "blank_translation_content",
            Self::InvalidSourceLineText { .. } => "invalid_source_line_text",
            Self::InvalidTranslationLineText { .. } => "invalid_translation_line_text",
            Self::AlignedLineCountMismatch { .. } => "aligned_line_count_mismatch",
            Self::AlignedBlankSlotMismatch { .. } => "aligned_blank_slot_mismatch",
            Self::InvalidSourceContext { .. } => "invalid_source_context",
            Self::SourceContextMustBeObject => "source_context_not_object",
            Self::InvalidTranslationStatePair => "invalid_translation_state_pair",
            Self::InvalidTranslationStateLength { .. } => "invalid_translation_state_length",
            Self::DuplicateGroup { .. } => "duplicate_group",
            Self::MissingGroup { .. } => "missing_group",
            Self::EmptyGroup { .. } => "empty_group",
            Self::InconsistentGroupDefinition { .. } => "inconsistent_group_definition",
            Self::DuplicateSemanticOrderKey { .. } => "duplicate_semantic_order_key",
            Self::DuplicateLogicalUnit { .. } => "duplicate_logical_unit",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("violation", self.code_suffix().to_owned())];
        match self {
            Self::WrongQueryResultSetCount { expected, actual }
            | Self::WrongColumnCount { expected, actual }
            | Self::AlignedLineCountMismatch { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => {
                facts.push(("column", column.to_string()));
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::InvalidSemanticOrderKey { column, violation } => {
                facts.push(("column", column.to_string()));
                facts.push(("semantic_order_failure", violation.as_str().to_owned()));
                if let RpgMakerSemanticOrderKeyViolation::UnknownMarker { actual } = violation {
                    facts.push(("actual_marker", actual.to_string()));
                }
            }
            Self::InactiveOwner { owner } | Self::DuplicateOwner { owner } => {
                facts.push(("owner", owner.as_str().to_owned()));
            }
            Self::InvalidOwnerSourceFingerprintLength { owner, actual }
            | Self::InvalidOwnerAssetFingerprintLength { owner, actual } => {
                facts.push(("owner", owner.as_str().to_owned()));
                facts.push(("expected_bytes", "32".to_owned()));
                facts.push(("actual_bytes", actual.to_string()));
            }
            Self::InvalidMetadataRowCount { actual } => {
                facts.push(("expected", "1".to_owned()));
                facts.push(("actual", actual.to_string()));
            }
            Self::InvalidMetadataFingerprintLength { actual }
            | Self::InvalidTranslationStateLength { actual } => {
                facts.push(("expected_bytes", "32".to_owned()));
                facts.push(("actual_bytes", actual.to_string()));
            }
            Self::MissingTranslationResource { resource_kind } => {
                facts.push(("resource_kind", resource_kind.as_str().to_owned()))
            }
            Self::DuplicateTranslationResource { resource_kind }
            | Self::BlankTranslationResource { resource_kind } => {
                facts.push(("resource_kind", resource_kind.as_str().to_owned()))
            }
            Self::InvalidSemanticScope { group_location } => {
                facts.push(("group_source", group_location.source_fact()));
                facts.push(("group_location_steps", group_location.steps_fact()));
            }
            Self::InvalidLocation { failure } => append_location_codec_facts(&mut facts, failure),
            Self::InvalidRole { failure } => append_projection_codec_facts(&mut facts, failure),
            Self::RoleDoesNotBelongToGroup { role, group_kind } => {
                facts.push(("role", role.fact_value()));
                facts.push(("group_kind", group_kind.as_str().to_owned()));
            }
            Self::InvalidSourceContent {
                category,
                line,
                column,
            }
            | Self::InvalidTranslationContent {
                category,
                line,
                column,
            }
            | Self::InvalidSourceContext {
                category,
                line,
                column,
            } => append_json_facts(&mut facts, *category, *line, *column),
            Self::SourceContentShapeMismatch { role }
            | Self::TranslationContentShapeMismatch { role } => {
                facts.push(("role", role.fact_value()))
            }
            Self::InvalidSourceLineText { index }
            | Self::InvalidTranslationLineText { index }
            | Self::AlignedBlankSlotMismatch { index } => {
                facts.push(("line_index", index.to_string()))
            }
            Self::DuplicateGroup {
                owner,
                group_location,
            }
            | Self::MissingGroup {
                owner,
                group_location,
            }
            | Self::EmptyGroup {
                owner,
                group_location,
            }
            | Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => append_asset_group_facts(&mut facts, *owner, group_location),
            Self::DuplicateSemanticOrderKey { level } => {
                facts.push(("level", level.as_str().to_owned()));
            }
            Self::DuplicateLogicalUnit {
                owner,
                group_location,
                role,
            } => {
                append_asset_group_facts(&mut facts, *owner, group_location);
                facts.push(("role", role.fact_value()));
            }
            Self::UnknownOwner
            | Self::UnknownTranslationResource
            | Self::UnknownGroupKind
            | Self::BlankSourceContent
            | Self::BlankTranslationContent
            | Self::SourceContextMustBeObject
            | Self::InvalidTranslationStatePair => {}
        }
        facts
    }
}

fn append_asset_group_facts(
    facts: &mut Vec<(&'static str, String)>,
    owner: RpgMakerDiagnosticOwner,
    location: &RpgMakerDiagnosticLocation,
) {
    facts.push(("owner", owner.as_str().to_owned()));
    facts.push(("group_source", location.source_fact()));
    facts.push(("group_location_steps", location.steps_fact()));
}

impl RpgMakerTranslationAssetProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::ProjectSnapshotChanged { .. } => {
                "rpg_maker.translate.asset_snapshot.project_changed"
            }
            Self::ExtractionOutOfDate { .. } => {
                "rpg_maker.translate.asset_snapshot.extraction_out_of_date"
            }
            Self::InvalidSnapshot { violation } => violation.code(),
            Self::Compute { operation, failure } => match (operation, failure) {
                (
                    RpgMakerTranslationAssetComputeOperation::PrepareSnapshot,
                    RpgMakerComputeFailure::Cancelled,
                ) => "rpg_maker.translate.asset_snapshot.prepare_snapshot.cancelled",
                (
                    RpgMakerTranslationAssetComputeOperation::PrepareSnapshot,
                    RpgMakerComputeFailure::ExecutorClosed,
                ) => "rpg_maker.translate.asset_snapshot.prepare_snapshot.executor_closed",
                (
                    RpgMakerTranslationAssetComputeOperation::PrepareSnapshot,
                    RpgMakerComputeFailure::StatePoisoned,
                ) => "rpg_maker.translate.asset_snapshot.prepare_snapshot.state_poisoned",
                (
                    RpgMakerTranslationAssetComputeOperation::PrepareSnapshot,
                    RpgMakerComputeFailure::WorkerPanicked,
                ) => "rpg_maker.translate.asset_snapshot.prepare_snapshot.worker_panicked",
                (
                    RpgMakerTranslationAssetComputeOperation::DecodeUnits,
                    RpgMakerComputeFailure::Cancelled,
                ) => "rpg_maker.translate.asset_snapshot.decode_units.cancelled",
                (
                    RpgMakerTranslationAssetComputeOperation::DecodeUnits,
                    RpgMakerComputeFailure::ExecutorClosed,
                ) => "rpg_maker.translate.asset_snapshot.decode_units.executor_closed",
                (
                    RpgMakerTranslationAssetComputeOperation::DecodeUnits,
                    RpgMakerComputeFailure::StatePoisoned,
                ) => "rpg_maker.translate.asset_snapshot.decode_units.state_poisoned",
                (
                    RpgMakerTranslationAssetComputeOperation::DecodeUnits,
                    RpgMakerComputeFailure::WorkerPanicked,
                ) => "rpg_maker.translate.asset_snapshot.decode_units.worker_panicked",
                (
                    RpgMakerTranslationAssetComputeOperation::AssembleCorpus,
                    RpgMakerComputeFailure::Cancelled,
                ) => "rpg_maker.translate.asset_snapshot.assemble_corpus.cancelled",
                (
                    RpgMakerTranslationAssetComputeOperation::AssembleCorpus,
                    RpgMakerComputeFailure::ExecutorClosed,
                ) => "rpg_maker.translate.asset_snapshot.assemble_corpus.executor_closed",
                (
                    RpgMakerTranslationAssetComputeOperation::AssembleCorpus,
                    RpgMakerComputeFailure::StatePoisoned,
                ) => "rpg_maker.translate.asset_snapshot.assemble_corpus.state_poisoned",
                (
                    RpgMakerTranslationAssetComputeOperation::AssembleCorpus,
                    RpgMakerComputeFailure::WorkerPanicked,
                ) => "rpg_maker.translate.asset_snapshot.assemble_corpus.worker_panicked",
            },
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled | RpgMakerComputeFailure::ExecutorClosed,
                ..
            } => DiagnosticResolution::Retry,
            Self::Compute { .. } => DiagnosticResolution::ReportBug,
            _ => DiagnosticResolution::CheckProjectState,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::ProjectSnapshotChanged { .. } => "source_snapshot_mismatch",
            Self::ExtractionOutOfDate { .. } => "extraction_out_of_date",
            Self::InvalidSnapshot { .. } => "invalid_value",
            Self::Compute { failure, .. } => match failure {
                RpgMakerComputeFailure::Cancelled => "cancelled",
                RpgMakerComputeFailure::ExecutorClosed => "unavailable",
                RpgMakerComputeFailure::StatePoisoned | RpgMakerComputeFailure::WorkerPanicked => {
                    "internal_invariant"
                }
            },
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::ProjectSnapshotChanged { expected, actual } => vec![
                ("expected_fingerprint", expected.to_string()),
                ("actual_fingerprint", actual.to_string()),
            ],
            Self::ExtractionOutOfDate { owners } => vec![(
                "owners",
                owners
                    .iter()
                    .map(|owner| owner.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )],
            Self::InvalidSnapshot { violation } => violation.facts(),
            Self::Compute { operation, failure } => vec![
                ("operation", operation.as_str().to_owned()),
                ("compute_failure", failure.as_str().to_owned()),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerMutationAccess {
    Intent,
    Exclusive,
}

impl RpgMakerMutationAccess {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Exclusive => "exclusive",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerClaimSummaryMismatchKind {
    DuplicateResource,
    MissingRow,
    UnexpectedRow,
    Resource,
    Access,
    Representative,
}

impl RpgMakerClaimSummaryMismatchKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateResource => "duplicate_resource",
            Self::MissingRow => "missing_row",
            Self::UnexpectedRow => "unexpected_row",
            Self::Resource => "resource_mismatch",
            Self::Access => "access_mismatch",
            Self::Representative => "representative_mismatch",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerClaimSummaryMismatchDetails {
    pub(crate) expected_group: Option<RpgMakerDiagnosticLocation>,
    pub(crate) actual_group: Option<RpgMakerDiagnosticLocation>,
    pub(crate) expected_resource: Option<RpgMakerDiagnosticLocation>,
    pub(crate) actual_resource: Option<RpgMakerDiagnosticLocation>,
    pub(crate) expected_access: Option<RpgMakerMutationAccess>,
    pub(crate) actual_access: Option<RpgMakerMutationAccess>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackModelViolation {
    BlankSourceContent {
        role: RpgMakerDiagnosticRole,
    },
    BlankTranslationContent {
        role: RpgMakerDiagnosticRole,
    },
    ContentShapeMismatch {
        role: RpgMakerDiagnosticRole,
    },
    EmptyLineContent {
        role: RpgMakerDiagnosticRole,
        column: SafeIdentifier,
    },
    InvalidContentLine {
        role: RpgMakerDiagnosticRole,
        column: SafeIdentifier,
        line_index: usize,
    },
    AlignedLineCountMismatch {
        role: RpgMakerDiagnosticRole,
        expected: usize,
        actual: usize,
    },
    AlignedBlankLineMismatch {
        unit: RpgMakerLogicalUnitLocator,
        line_index: usize,
    },
    EmptyProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidRole {
        group_kind: RpgMakerDiagnosticGroupKind,
        role: RpgMakerDiagnosticRole,
    },
    DuplicateRole {
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    },
    RecipeRoleMismatch {
        group_location: RpgMakerDiagnosticLocation,
        units: Vec<RpgMakerDiagnosticRole>,
        recipes: Vec<RpgMakerDiagnosticRole>,
    },
    RecipeLineMismatch {
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    },
    RecipeClaimMismatch {
        group_location: RpgMakerDiagnosticLocation,
    },
    RecipeDoesNotRebuildOriginal {
        group_location: RpgMakerDiagnosticLocation,
        target: RpgMakerDiagnosticLocation,
    },
    MutationClaimConflict {
        resource: RpgMakerDiagnosticLocation,
    },
    MismatchedClaimSource {
        group_location: RpgMakerDiagnosticLocation,
    },
    MismatchedClaimResourceSource {
        group_location: RpgMakerDiagnosticLocation,
        resource: RpgMakerDiagnosticLocation,
    },
    InvalidDialogueProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidScrollingProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidScrollingRecipe {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidChoicesProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidDirectProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    MismatchedDialogueGroup {
        group_location: RpgMakerDiagnosticLocation,
        recipe_location: RpgMakerDiagnosticLocation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackAssetSnapshotViolation {
    WrongQueryResultSetCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: SafeIdentifier,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    PlaceholderRuleRowCount {
        expected: usize,
        actual: usize,
    },
    BlankPlaceholderRules,
    LayoutRuleRowCount {
        expected: usize,
        actual: usize,
    },
    BlankLayoutRules,
    InvalidSemanticOrderKey {
        column: SafeIdentifier,
        violation: RpgMakerSemanticOrderKeyViolation,
    },
    UnknownOwner,
    DuplicateOwner {
        owner: RpgMakerDiagnosticOwner,
    },
    InvalidFingerprintLength {
        owner: RpgMakerDiagnosticOwner,
        column: SafeIdentifier,
        expected: usize,
        actual: usize,
    },
    AssetWithoutOwner {
        owner: RpgMakerDiagnosticOwner,
    },
    UnknownGroupKind,
    DuplicateGroup {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    MissingGroup {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    DuplicateSemanticOrderKey {
        owner: RpgMakerDiagnosticOwner,
        level: RpgMakerSemanticOrderLevel,
    },
    UnknownMutationAccess,
    NonCanonicalMutationResource {
        owner: RpgMakerDiagnosticOwner,
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidClaimSummary {
        owner: RpgMakerDiagnosticOwner,
        row_index: usize,
        mismatch: RpgMakerClaimSummaryMismatchKind,
        expected_rows: usize,
        actual_rows: usize,
        details: RpgMakerClaimSummaryMismatchDetails,
    },
    AssetFingerprintMismatch {
        owner: RpgMakerDiagnosticOwner,
    },
    InvalidDialogueDefinition {
        problem: RpgMakerDialogueDefinitionProblem,
    },
    InvalidLocation {
        failure: RpgMakerLocationCodecFailure,
    },
    InvalidProjection {
        failure: RpgMakerProjectionCodecFailure,
    },
    InvalidUnitContent {
        column: SafeIdentifier,
        category: RpgMakerJsonFailureKind,
        line: usize,
        json_column: usize,
    },
    InvalidModel {
        violation: RpgMakerWriteBackModelViolation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackAssetComputeOperation {
    #[serde(rename = "prepare_snapshot")]
    Prepare,
    #[serde(rename = "decode_snapshot")]
    Decode,
    #[serde(rename = "assemble_snapshot")]
    Assemble,
}

impl RpgMakerWriteBackAssetComputeOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare_snapshot",
            Self::Decode => "decode_snapshot",
            Self::Assemble => "assemble_snapshot",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackAssetProblem {
    DatabaseNotFound,
    ExtractionOutOfDate {
        owners: Vec<RpgMakerDiagnosticOwner>,
    },
    InvalidSnapshot {
        violation: RpgMakerWriteBackAssetSnapshotViolation,
    },
    InvalidLayoutRules {
        path: Option<SafePath>,
        rule_number: Option<usize>,
        project_snapshot: bool,
    },
    LayoutRulesStateChanged,
    Compute {
        operation: RpgMakerWriteBackAssetComputeOperation,
        failure: RpgMakerComputeFailure,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackDialoguePlanViolation {
    SpeakerSlotMismatch,
    UnexpectedBodyTranslation,
    EmptyBodyLines,
    BodySourceMapMismatch,
}

impl RpgMakerWriteBackDialoguePlanViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SpeakerSlotMismatch => "speaker_slot_mismatch",
            Self::UnexpectedBodyTranslation => "unexpected_body_translation",
            Self::EmptyBodyLines => "empty_body_lines",
            Self::BodySourceMapMismatch => "body_source_map_mismatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackChoicesPlanViolation {
    EmptyOrMismatchedLineCount,
    BlankSlotChanged,
    InvalidRecipeShape,
    RecipeSourceMismatch,
    IncompleteCommandCoverage,
}

impl RpgMakerWriteBackChoicesPlanViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyOrMismatchedLineCount => "empty_or_mismatched_line_count",
            Self::BlankSlotChanged => "blank_slot_changed",
            Self::InvalidRecipeShape => "invalid_recipe_shape",
            Self::RecipeSourceMismatch => "recipe_source_mismatch",
            Self::IncompleteCommandCoverage => "incomplete_command_coverage",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackMutationPlanViolation {
    EmptyEventBody {
        group_location: RpgMakerDiagnosticLocation,
    },
    EmptyEventReplacement {
        exact_location: RpgMakerDiagnosticLocation,
    },
    InvalidDialogue {
        group_location: RpgMakerDiagnosticLocation,
        violation: RpgMakerWriteBackDialoguePlanViolation,
    },
    InvalidChoices {
        group_location: RpgMakerDiagnosticLocation,
        violation: RpgMakerWriteBackChoicesPlanViolation,
    },
    DuplicateLocation {
        exact_location: RpgMakerDiagnosticLocation,
    },
    MutationClaimConflict {
        resource: RpgMakerDiagnosticLocation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackPlanningProblem {
    Compute {
        failure: RpgMakerComputeFailure,
    },
    InvalidPlan {
        violation: RpgMakerWriteBackMutationPlanViolation,
    },
}

impl RpgMakerWriteBackMutationPlanViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::EmptyEventBody { .. } => "rpg_maker.write_back.plan.empty_event_body",
            Self::EmptyEventReplacement { .. } => {
                "rpg_maker.write_back.plan.empty_event_replacement"
            }
            Self::InvalidDialogue { .. } => "rpg_maker.write_back.plan.invalid_dialogue",
            Self::InvalidChoices { .. } => "rpg_maker.write_back.plan.invalid_choices",
            Self::DuplicateLocation { .. } => "rpg_maker.write_back.plan.duplicate_location",
            Self::MutationClaimConflict { .. } => {
                "rpg_maker.write_back.plan.mutation_claim_conflict"
            }
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::EmptyEventBody { group_location } => location_facts(group_location),
            Self::EmptyEventReplacement { exact_location }
            | Self::DuplicateLocation { exact_location } => location_facts(exact_location),
            Self::InvalidDialogue {
                group_location,
                violation,
            } => {
                let mut facts = location_facts(group_location);
                facts.push(("dialogue_violation", violation.as_str().to_owned()));
                facts
            }
            Self::InvalidChoices {
                group_location,
                violation,
            } => {
                let mut facts = location_facts(group_location);
                facts.push(("choices_violation", violation.as_str().to_owned()));
                facts
            }
            Self::MutationClaimConflict { resource } => location_facts(resource),
        }
    }
}

impl RpgMakerWriteBackPlanningProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled,
            } => "rpg_maker.write_back.plan.cancelled",
            Self::Compute {
                failure: RpgMakerComputeFailure::ExecutorClosed,
            } => "rpg_maker.write_back.plan.executor_closed",
            Self::Compute {
                failure: RpgMakerComputeFailure::StatePoisoned,
            } => "rpg_maker.write_back.plan.state_poisoned",
            Self::Compute {
                failure: RpgMakerComputeFailure::WorkerPanicked,
            } => "rpg_maker.write_back.plan.worker_panicked",
            Self::InvalidPlan { violation } => violation.code(),
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled | RpgMakerComputeFailure::ExecutorClosed,
            } => DiagnosticResolution::Retry,
            Self::Compute { .. } | Self::InvalidPlan { .. } => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled,
            } => "cancelled",
            Self::Compute {
                failure: RpgMakerComputeFailure::ExecutorClosed,
            } => "unavailable",
            Self::Compute { .. } | Self::InvalidPlan { .. } => "internal_invariant",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Compute { failure } => {
                vec![("compute_failure", failure.as_str().to_owned())]
            }
            Self::InvalidPlan { violation } => violation.facts(),
        }
    }
}

fn location_facts(location: &RpgMakerDiagnosticLocation) -> Vec<(&'static str, String)> {
    vec![
        ("source", location.source_fact()),
        ("location_steps", location.steps_fact()),
    ]
}

impl RpgMakerWriteBackModelViolation {
    const fn code_suffix(&self) -> &'static str {
        match self {
            Self::BlankSourceContent { .. } => "blank_source_content",
            Self::BlankTranslationContent { .. } => "blank_translation_content",
            Self::ContentShapeMismatch { .. } => "content_shape_mismatch",
            Self::EmptyLineContent { .. } => "empty_line_content",
            Self::InvalidContentLine { .. } => "invalid_content_line",
            Self::AlignedLineCountMismatch { .. } => "aligned_line_count_mismatch",
            Self::AlignedBlankLineMismatch { .. } => "aligned_blank_line_mismatch",
            Self::EmptyProjection { .. } => "empty_projection",
            Self::InvalidRole { .. } => "invalid_role",
            Self::DuplicateRole { .. } => "duplicate_role",
            Self::RecipeRoleMismatch { .. } => "recipe_role_mismatch",
            Self::RecipeLineMismatch { .. } => "recipe_line_mismatch",
            Self::RecipeClaimMismatch { .. } => "recipe_claim_mismatch",
            Self::RecipeDoesNotRebuildOriginal { .. } => "recipe_does_not_rebuild_original",
            Self::MutationClaimConflict { .. } => "mutation_claim_conflict",
            Self::MismatchedClaimSource { .. } => "mismatched_claim_source",
            Self::MismatchedClaimResourceSource { .. } => "mismatched_claim_resource_source",
            Self::InvalidDialogueProjection { .. } => "invalid_dialogue_projection",
            Self::InvalidScrollingProjection { .. } => "invalid_scrolling_projection",
            Self::InvalidScrollingRecipe { .. } => "invalid_scrolling_recipe",
            Self::InvalidChoicesProjection { .. } => "invalid_choices_projection",
            Self::InvalidDirectProjection { .. } => "invalid_direct_projection",
            Self::MismatchedDialogueGroup { .. } => "mismatched_dialogue_group",
        }
    }
}

impl RpgMakerWriteBackAssetSnapshotViolation {
    fn code(&self) -> &'static str {
        let suffix = match self {
            Self::WrongQueryResultSetCount { .. } => "wrong_query_result_set_count",
            Self::WrongColumnCount { .. } => "wrong_column_count",
            Self::WrongColumnType { .. } => "wrong_column_type",
            Self::PlaceholderRuleRowCount { .. } => "placeholder_rule_row_count",
            Self::BlankPlaceholderRules => "blank_placeholder_rules",
            Self::LayoutRuleRowCount { .. } => "layout_rule_row_count",
            Self::BlankLayoutRules => "blank_layout_rules",
            Self::InvalidSemanticOrderKey { .. } => "invalid_semantic_order_key",
            Self::UnknownOwner => "unknown_owner",
            Self::DuplicateOwner { .. } => "duplicate_owner",
            Self::InvalidFingerprintLength { .. } => "invalid_fingerprint_length",
            Self::AssetWithoutOwner { .. } => "asset_without_owner",
            Self::UnknownGroupKind => "unknown_group_kind",
            Self::DuplicateGroup { .. } => "duplicate_group",
            Self::MissingGroup { .. } => "missing_group",
            Self::DuplicateSemanticOrderKey { .. } => "duplicate_semantic_order_key",
            Self::UnknownMutationAccess => "unknown_mutation_access",
            Self::NonCanonicalMutationResource { .. } => "non_canonical_mutation_resource",
            Self::InvalidClaimSummary { .. } => "invalid_claim_summary",
            Self::AssetFingerprintMismatch { .. } => "asset_fingerprint_mismatch",
            Self::InvalidDialogueDefinition { .. } => "invalid_dialogue_definition",
            Self::InvalidLocation { .. } => "invalid_location",
            Self::InvalidProjection { .. } => "invalid_projection",
            Self::InvalidUnitContent { .. } => "invalid_unit_content",
            Self::InvalidModel { violation } => violation.code_suffix(),
        };
        match suffix {
            "wrong_query_result_set_count" => {
                "rpg_maker.write_back.asset_snapshot.wrong_query_result_set_count"
            }
            "wrong_column_count" => "rpg_maker.write_back.asset_snapshot.wrong_column_count",
            "wrong_column_type" => "rpg_maker.write_back.asset_snapshot.wrong_column_type",
            "placeholder_rule_row_count" => {
                "rpg_maker.write_back.asset_snapshot.placeholder_rule_row_count"
            }
            "blank_placeholder_rules" => {
                "rpg_maker.write_back.asset_snapshot.blank_placeholder_rules"
            }
            "layout_rule_row_count" => "rpg_maker.write_back.asset_snapshot.layout_rule_row_count",
            "blank_layout_rules" => "rpg_maker.write_back.asset_snapshot.blank_layout_rules",
            "invalid_semantic_order_key" => {
                "rpg_maker.write_back.asset_snapshot.invalid_semantic_order_key"
            }
            "unknown_owner" => "rpg_maker.write_back.asset_snapshot.unknown_owner",
            "duplicate_owner" => "rpg_maker.write_back.asset_snapshot.duplicate_owner",
            "invalid_fingerprint_length" => {
                "rpg_maker.write_back.asset_snapshot.invalid_fingerprint_length"
            }
            "asset_without_owner" => "rpg_maker.write_back.asset_snapshot.asset_without_owner",
            "unknown_group_kind" => "rpg_maker.write_back.asset_snapshot.unknown_group_kind",
            "duplicate_group" => "rpg_maker.write_back.asset_snapshot.duplicate_group",
            "missing_group" => "rpg_maker.write_back.asset_snapshot.missing_group",
            "duplicate_semantic_order_key" => {
                "rpg_maker.write_back.asset_snapshot.duplicate_semantic_order_key"
            }
            "unknown_mutation_access" => {
                "rpg_maker.write_back.asset_snapshot.unknown_mutation_access"
            }
            "non_canonical_mutation_resource" => {
                "rpg_maker.write_back.asset_snapshot.non_canonical_mutation_resource"
            }
            "invalid_claim_summary" => "rpg_maker.write_back.asset_snapshot.invalid_claim_summary",
            "asset_fingerprint_mismatch" => {
                "rpg_maker.write_back.asset_snapshot.asset_fingerprint_mismatch"
            }
            "invalid_dialogue_definition" => {
                "rpg_maker.write_back.asset_snapshot.invalid_dialogue_definition"
            }
            "invalid_location" => "rpg_maker.write_back.asset_snapshot.invalid_location",
            "invalid_projection" => "rpg_maker.write_back.asset_snapshot.invalid_projection",
            "invalid_unit_content" => "rpg_maker.write_back.asset_snapshot.invalid_unit_content",
            "blank_source_content" => "rpg_maker.write_back.asset_snapshot.blank_source_content",
            "blank_translation_content" => {
                "rpg_maker.write_back.asset_snapshot.blank_translation_content"
            }
            "content_shape_mismatch" => {
                "rpg_maker.write_back.asset_snapshot.content_shape_mismatch"
            }
            "empty_line_content" => "rpg_maker.write_back.asset_snapshot.empty_line_content",
            "invalid_content_line" => "rpg_maker.write_back.asset_snapshot.invalid_content_line",
            "aligned_line_count_mismatch" => {
                "rpg_maker.write_back.asset_snapshot.aligned_line_count_mismatch"
            }
            "aligned_blank_line_mismatch" => {
                "rpg_maker.write_back.asset_snapshot.aligned_blank_line_mismatch"
            }
            "empty_projection" => "rpg_maker.write_back.asset_snapshot.empty_projection",
            "invalid_role" => "rpg_maker.write_back.asset_snapshot.invalid_role",
            "duplicate_role" => "rpg_maker.write_back.asset_snapshot.duplicate_role",
            "recipe_role_mismatch" => "rpg_maker.write_back.asset_snapshot.recipe_role_mismatch",
            "recipe_line_mismatch" => "rpg_maker.write_back.asset_snapshot.recipe_line_mismatch",
            "recipe_claim_mismatch" => "rpg_maker.write_back.asset_snapshot.recipe_claim_mismatch",
            "recipe_does_not_rebuild_original" => {
                "rpg_maker.write_back.asset_snapshot.recipe_does_not_rebuild_original"
            }
            "mutation_claim_conflict" => {
                "rpg_maker.write_back.asset_snapshot.mutation_claim_conflict"
            }
            "mismatched_claim_source" => {
                "rpg_maker.write_back.asset_snapshot.mismatched_claim_source"
            }
            "mismatched_claim_resource_source" => {
                "rpg_maker.write_back.asset_snapshot.mismatched_claim_resource_source"
            }
            "invalid_dialogue_projection" => {
                "rpg_maker.write_back.asset_snapshot.invalid_dialogue_projection"
            }
            "invalid_scrolling_projection" => {
                "rpg_maker.write_back.asset_snapshot.invalid_scrolling_projection"
            }
            "invalid_scrolling_recipe" => {
                "rpg_maker.write_back.asset_snapshot.invalid_scrolling_recipe"
            }
            "invalid_choices_projection" => {
                "rpg_maker.write_back.asset_snapshot.invalid_choices_projection"
            }
            "invalid_direct_projection" => {
                "rpg_maker.write_back.asset_snapshot.invalid_direct_projection"
            }
            "mismatched_dialogue_group" => {
                "rpg_maker.write_back.asset_snapshot.mismatched_dialogue_group"
            }
            _ => unreachable!("写回资产快照违反项必须拥有稳定代码"),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("violation", self.code().to_owned())];
        match self {
            Self::WrongQueryResultSetCount { expected, actual }
            | Self::WrongColumnCount { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => {
                facts.push(("column", column.to_string()));
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::PlaceholderRuleRowCount { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::LayoutRuleRowCount { expected, actual } => {
                facts.push(("expected", expected.to_string()));
                facts.push(("actual", actual.to_string()));
            }
            Self::InvalidSemanticOrderKey { column, violation } => {
                facts.push(("column", column.to_string()));
                facts.push(("semantic_order_failure", violation.as_str().to_owned()));
            }
            Self::DuplicateOwner { owner }
            | Self::AssetWithoutOwner { owner }
            | Self::AssetFingerprintMismatch { owner } => {
                facts.push(("owner", owner.as_str().to_owned()));
            }
            Self::InvalidFingerprintLength {
                owner,
                column,
                expected,
                actual,
            } => {
                facts.push(("owner", owner.as_str().to_owned()));
                facts.push(("column", column.to_string()));
                facts.push(("expected_bytes", expected.to_string()));
                facts.push(("actual_bytes", actual.to_string()));
            }
            Self::DuplicateGroup {
                owner,
                group_location,
            }
            | Self::MissingGroup {
                owner,
                group_location,
            }
            | Self::NonCanonicalMutationResource {
                owner,
                group_location,
            } => {
                append_asset_group_facts(&mut facts, *owner, group_location);
            }
            Self::DuplicateSemanticOrderKey { owner, level } => {
                facts.push(("owner", owner.as_str().to_owned()));
                facts.push(("level", level.as_str().to_owned()));
            }
            Self::InvalidClaimSummary {
                owner,
                row_index,
                mismatch,
                expected_rows,
                actual_rows,
                details,
            } => {
                facts.push(("owner", owner.as_str().to_owned()));
                facts.push(("row_index", row_index.to_string()));
                facts.push(("summary_mismatch", mismatch.as_str().to_owned()));
                facts.push(("expected_rows", expected_rows.to_string()));
                facts.push(("actual_rows", actual_rows.to_string()));
                if let Some(access) = details.expected_access {
                    facts.push(("expected_access", access.as_str().to_owned()));
                }
                if let Some(access) = details.actual_access {
                    facts.push(("actual_access", access.as_str().to_owned()));
                }
            }
            Self::InvalidDialogueDefinition { problem } => facts.extend(problem.facts()),
            Self::InvalidLocation { failure } => append_location_codec_facts(&mut facts, failure),
            Self::InvalidProjection { failure } => {
                append_projection_codec_facts(&mut facts, failure)
            }
            Self::InvalidUnitContent {
                column,
                category,
                line,
                json_column,
            } => {
                facts.push(("column", column.to_string()));
                append_json_facts(&mut facts, *category, *line, *json_column);
            }
            Self::InvalidModel { violation } => {
                facts.push(("model_violation", violation.code_suffix().to_owned()));
            }
            Self::UnknownOwner
            | Self::UnknownGroupKind
            | Self::UnknownMutationAccess
            | Self::BlankPlaceholderRules
            | Self::BlankLayoutRules => {}
        }
        facts
    }
}

impl RpgMakerWriteBackAssetProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::DatabaseNotFound => "rpg_maker.write_back.asset_snapshot.database_not_found",
            Self::ExtractionOutOfDate { .. } => {
                "rpg_maker.write_back.asset_snapshot.extraction_out_of_date"
            }
            Self::InvalidSnapshot { violation } => violation.code(),
            Self::InvalidLayoutRules { .. } => "rpg_maker.write_back.layout_rules.invalid",
            Self::LayoutRulesStateChanged => "rpg_maker.write_back.layout_rules.state_changed",
            Self::Compute {
                operation: RpgMakerWriteBackAssetComputeOperation::Prepare,
                ..
            } => "rpg_maker.write_back.asset_snapshot.prepare_snapshot_failed",
            Self::Compute {
                operation: RpgMakerWriteBackAssetComputeOperation::Decode,
                ..
            } => "rpg_maker.write_back.asset_snapshot.decode_snapshot_failed",
            Self::Compute {
                operation: RpgMakerWriteBackAssetComputeOperation::Assemble,
                ..
            } => "rpg_maker.write_back.asset_snapshot.assemble_snapshot_failed",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::DatabaseNotFound
            | Self::ExtractionOutOfDate { .. }
            | Self::InvalidSnapshot { .. } => DiagnosticResolution::CheckProjectState,
            Self::InvalidLayoutRules {
                project_snapshot: true,
                ..
            }
            | Self::LayoutRulesStateChanged => DiagnosticResolution::CheckProjectState,
            Self::InvalidLayoutRules {
                project_snapshot: false,
                ..
            } => DiagnosticResolution::FixInput,
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled | RpgMakerComputeFailure::ExecutorClosed,
                ..
            } => DiagnosticResolution::Retry,
            Self::Compute { .. } => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::DatabaseNotFound => "not_found",
            Self::ExtractionOutOfDate { .. } => "extraction_out_of_date",
            Self::InvalidSnapshot { .. } => "invalid_value",
            Self::InvalidLayoutRules {
                project_snapshot: true,
                ..
            }
            | Self::LayoutRulesStateChanged => "state_mismatch",
            Self::InvalidLayoutRules {
                project_snapshot: false,
                ..
            } => "invalid_value",
            Self::Compute {
                failure: RpgMakerComputeFailure::Cancelled,
                ..
            } => "cancelled",
            Self::Compute {
                failure: RpgMakerComputeFailure::ExecutorClosed,
                ..
            } => "unavailable",
            Self::Compute { .. } => "internal_invariant",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::DatabaseNotFound => Vec::new(),
            Self::ExtractionOutOfDate { owners } => vec![(
                "owners",
                owners
                    .iter()
                    .map(|owner| owner.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )],
            Self::InvalidSnapshot { violation } => violation.facts(),
            Self::InvalidLayoutRules {
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
            Self::LayoutRulesStateChanged => Vec::new(),
            Self::Compute { operation, failure } => vec![
                ("operation", operation.as_str().to_owned()),
                ("compute_failure", failure.as_str().to_owned()),
            ],
        }
    }
}

/// Extract 问题的真实产生者。Rules 来源必须同时保留当前诊断文件。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionSource {
    Builtin,
    Rules { rules_path: SafePath },
}

impl RpgMakerExtractionSource {
    pub(crate) const fn builtin() -> Self {
        Self::Builtin
    }

    pub(crate) fn rules(rules_path: impl AsRef<std::path::Path>) -> Self {
        Self::Rules {
            rules_path: SafePath::new(rules_path),
        }
    }

    const fn owner(&self) -> RpgMakerDiagnosticOwner {
        match self {
            Self::Builtin => RpgMakerDiagnosticOwner::Builtin,
            Self::Rules { .. } => RpgMakerDiagnosticOwner::Rules,
        }
    }

    const fn stage(&self) -> RpgMakerDiagnosticStage {
        match self {
            Self::Builtin => RpgMakerDiagnosticStage::ExtractBuiltin,
            Self::Rules { .. } => RpgMakerDiagnosticStage::ExtractRules,
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::Builtin => "builtin".to_owned(),
            Self::Rules { rules_path } => rules_path.to_string(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("owner", self.owner().as_str().to_owned())];
        if let Self::Rules { rules_path } = self {
            facts.push(("rules_path", rules_path.to_string()));
        }
        facts
    }
}

/// 语义顺序键的全部可观测内容，不使用 Debug 正文作协议。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerExtractionSemanticOrderKey {
    physical_path: Vec<u64>,
    fragment: u64,
}

impl RpgMakerExtractionSemanticOrderKey {
    pub(crate) const fn new(physical_path: Vec<u64>, fragment: u64) -> Self {
        Self {
            physical_path,
            fragment,
        }
    }

    fn fact_value(&self) -> String {
        let path = self
            .physical_path
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("/");
        format!("{path}#{fragment}", fragment = self.fragment)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionSemanticOrderProjectionViolation {
    MissingSourceDocument,
    UnsupportedBuiltinPluginSource,
    ExpectedObject,
    MissingObjectKey,
    ExpectedArray,
    MissingArrayIndex,
    ExpectedEncodedJsonString,
    InvalidEncodedJson,
    MissingPhysicalOrdinal,
    ExtraPhysicalOrdinal,
    ArrayOrdinalMismatch { index: usize, ordinal: usize },
    OrdinalOverflow,
}

impl RpgMakerExtractionSemanticOrderProjectionViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingSourceDocument => "missing_source_document",
            Self::UnsupportedBuiltinPluginSource => "unsupported_builtin_plugin_source",
            Self::ExpectedObject => "expected_object",
            Self::MissingObjectKey => "missing_object_key",
            Self::ExpectedArray => "expected_array",
            Self::MissingArrayIndex => "missing_array_index",
            Self::ExpectedEncodedJsonString => "expected_encoded_json_string",
            Self::InvalidEncodedJson => "invalid_encoded_json",
            Self::MissingPhysicalOrdinal => "missing_physical_ordinal",
            Self::ExtraPhysicalOrdinal => "extra_physical_ordinal",
            Self::ArrayOrdinalMismatch { .. } => "array_ordinal_mismatch",
            Self::OrdinalOverflow => "ordinal_overflow",
        }
    }
}

/// Extract 快照模型在尚持有精确位置和角色时建立的封闭违反项。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionSnapshotViolation {
    BlankSourceContent {
        location: RpgMakerDiagnosticLocation,
    },
    ContentShapeMismatch {
        role: RpgMakerDiagnosticRole,
        location: RpgMakerDiagnosticLocation,
    },
    DirectGroupRequiresValue {
        role: RpgMakerDiagnosticRole,
        location: RpgMakerDiagnosticLocation,
    },
    InvalidSourceLine {
        source_line_index: usize,
        location: RpgMakerDiagnosticLocation,
    },
    EmptyGroup {
        group_location: RpgMakerDiagnosticLocation,
    },
    EmptyProjection {
        group_location: RpgMakerDiagnosticLocation,
    },
    DuplicateLogicalLocation {
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
    },
    ConflictingGroupKind {
        group_location: RpgMakerDiagnosticLocation,
        first: RpgMakerDiagnosticGroupKind,
        second: RpgMakerDiagnosticGroupKind,
    },
    ConflictingSemanticOrderKey {
        group_location: RpgMakerDiagnosticLocation,
        first: RpgMakerExtractionSemanticOrderKey,
        second: RpgMakerExtractionSemanticOrderKey,
    },
    SemanticOrderProjection {
        location: RpgMakerDiagnosticLocation,
        violation: RpgMakerExtractionSemanticOrderProjectionViolation,
    },
    MutationClaimConflict {
        resource: RpgMakerDiagnosticLocation,
    },
    RecipeRoleMismatch {
        group_location: RpgMakerDiagnosticLocation,
        units: Vec<RpgMakerDiagnosticRole>,
        referenced: Vec<RpgMakerDiagnosticRole>,
    },
    RecipeLineMismatch {
        group_location: RpgMakerDiagnosticLocation,
        role: RpgMakerDiagnosticRole,
        expected: Vec<usize>,
        referenced: Vec<usize>,
    },
    Projection {
        violation: RpgMakerProjectionModelViolation,
    },
}

impl RpgMakerExtractionSnapshotViolation {
    fn code(&self) -> &'static str {
        match self {
            Self::BlankSourceContent { .. } => "rpg_maker.extract.snapshot.blank_source_content",
            Self::ContentShapeMismatch { .. } => {
                "rpg_maker.extract.snapshot.content_shape_mismatch"
            }
            Self::DirectGroupRequiresValue { .. } => {
                "rpg_maker.extract.snapshot.direct_group_requires_value"
            }
            Self::InvalidSourceLine { .. } => "rpg_maker.extract.snapshot.invalid_source_line",
            Self::EmptyGroup { .. } => "rpg_maker.extract.snapshot.empty_group",
            Self::EmptyProjection { .. } => "rpg_maker.extract.snapshot.empty_projection",
            Self::DuplicateLogicalLocation { .. } => {
                "rpg_maker.extract.snapshot.duplicate_logical_location"
            }
            Self::ConflictingGroupKind { .. } => {
                "rpg_maker.extract.snapshot.conflicting_group_kind"
            }
            Self::ConflictingSemanticOrderKey { .. } => {
                "rpg_maker.extract.snapshot.conflicting_semantic_order_key"
            }
            Self::SemanticOrderProjection { .. } => {
                "rpg_maker.extract.snapshot.semantic_order_projection"
            }
            Self::MutationClaimConflict { .. } => {
                "rpg_maker.extract.snapshot.mutation_claim_conflict"
            }
            Self::RecipeRoleMismatch { .. } => "rpg_maker.extract.snapshot.recipe_role_mismatch",
            Self::RecipeLineMismatch { .. } => "rpg_maker.extract.snapshot.recipe_line_mismatch",
            Self::Projection { .. } => "rpg_maker.extract.snapshot.invalid_projection",
        }
    }

    fn subject(&self) -> Option<String> {
        let location = match self {
            Self::BlankSourceContent { location }
            | Self::ContentShapeMismatch { location, .. }
            | Self::DirectGroupRequiresValue { location, .. }
            | Self::InvalidSourceLine { location, .. }
            | Self::SemanticOrderProjection { location, .. } => location,
            Self::EmptyGroup { group_location }
            | Self::EmptyProjection { group_location }
            | Self::DuplicateLogicalLocation { group_location, .. }
            | Self::ConflictingGroupKind { group_location, .. }
            | Self::ConflictingSemanticOrderKey { group_location, .. }
            | Self::RecipeRoleMismatch { group_location, .. }
            | Self::RecipeLineMismatch { group_location, .. } => group_location,
            Self::MutationClaimConflict { resource } => resource,
            Self::Projection { .. } => return None,
        };
        Some(format!(
            "{}:{}",
            location.source_fact(),
            location.steps_fact()
        ))
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("snapshot_violation", self.code().to_owned())];
        let append_location =
            |facts: &mut Vec<(&'static str, String)>, location: &RpgMakerDiagnosticLocation| {
                facts.push(("source", location.source_fact()));
                facts.push(("location", location.steps_fact()));
            };
        match self {
            Self::BlankSourceContent { location } => append_location(&mut facts, location),
            Self::ContentShapeMismatch { role, location }
            | Self::DirectGroupRequiresValue { role, location } => {
                append_location(&mut facts, location);
                facts.push(("role", role.fact_value()));
            }
            Self::InvalidSourceLine {
                source_line_index,
                location,
            } => {
                append_location(&mut facts, location);
                facts.push(("source_line_index", source_line_index.to_string()));
            }
            Self::EmptyGroup { group_location } | Self::EmptyProjection { group_location } => {
                append_location(&mut facts, group_location);
            }
            Self::DuplicateLogicalLocation {
                group_location,
                role,
            } => {
                append_location(&mut facts, group_location);
                facts.push(("role", role.fact_value()));
            }
            Self::ConflictingGroupKind {
                group_location,
                first,
                second,
            } => {
                append_location(&mut facts, group_location);
                facts.push(("first_group_kind", first.as_str().to_owned()));
                facts.push(("second_group_kind", second.as_str().to_owned()));
            }
            Self::ConflictingSemanticOrderKey {
                group_location,
                first,
                second,
            } => {
                append_location(&mut facts, group_location);
                facts.push(("first_semantic_order_key", first.fact_value()));
                facts.push(("second_semantic_order_key", second.fact_value()));
            }
            Self::SemanticOrderProjection {
                location,
                violation,
            } => {
                append_location(&mut facts, location);
                facts.push(("semantic_order_violation", violation.as_str().to_owned()));
                if let RpgMakerExtractionSemanticOrderProjectionViolation::ArrayOrdinalMismatch {
                    index,
                    ordinal,
                } = violation
                {
                    facts.push(("array_index", index.to_string()));
                    facts.push(("physical_ordinal", ordinal.to_string()));
                }
            }
            Self::MutationClaimConflict { resource } => append_location(&mut facts, resource),
            Self::RecipeRoleMismatch {
                group_location,
                units,
                referenced,
            } => {
                append_location(&mut facts, group_location);
                facts.push((
                    "unit_roles",
                    units
                        .iter()
                        .map(RpgMakerDiagnosticRole::fact_value)
                        .collect::<Vec<_>>()
                        .join(","),
                ));
                facts.push((
                    "referenced_roles",
                    referenced
                        .iter()
                        .map(RpgMakerDiagnosticRole::fact_value)
                        .collect::<Vec<_>>()
                        .join(","),
                ));
            }
            Self::RecipeLineMismatch {
                group_location,
                role,
                expected,
                referenced,
            } => {
                append_location(&mut facts, group_location);
                facts.push(("role", role.fact_value()));
                facts.push(("expected_lines", join_usize(expected)));
                facts.push(("referenced_lines", join_usize(referenced)));
            }
            Self::Projection { violation } => {
                facts.push(("projection_violation", violation.as_str().to_owned()));
            }
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackMutationViolation {
    PluginParameterAsDocument,
    RequestedDocumentMissing,
    CrossDocumentMutation,
    DuplicateStructuralTarget,
    DecodeBoundaryInEventContainer,
    EventListNotArray,
    OverlappingFrozenRanges,
    DecodeTargetNotString,
    PluginIndexMissing,
    PluginIndexMismatch,
    PluginNameMismatch,
    PluginParametersNotObject,
    PluginParameterMissing,
    TargetNotString,
    ExpectedOriginalMismatch,
    ObjectPathMissingOrWrongType,
    ArrayPathMissingOrWrongType,
    UnexpectedDecodeBoundary,
    MissingCommandArrayIndex,
    CommandPathNotArrayIndex,
    ChoiceStartOutOfBounds,
    ChoiceStartNot102,
    FrozenChoicesMismatch,
    InvalidChoiceIndex,
    MissingChoiceEnd,
    ChoiceRecipeTargetMismatch,
    EventBodyStartOutOfBounds,
    EventBodyCodeMismatch,
    EventBodyTooLong,
    IncompleteEventBodyCoverage,
    FrozenBodyCodeMismatch,
    FrozenBodyMismatch,
    DialogueRecipeLocationMismatch,
    DialogueStartOutOfBounds,
    DialogueStartNot101,
    DialogueRecipeTooLong,
    IncompleteDialogueCoverage,
    FrozenSpeakerMismatch,
    FrozenDialogueCodeMismatch,
    FrozenDialogueBodyMismatch,
    StructureAfterBody,
    TranslationWithoutBodyRecipe,
    FrozenBodyMissingSpeakerShell,
    BodyLineNotLast,
    MissingEmbeddedSpeaker,
    DialogueParameterOutsideBlock,
    EventBodySegmentOutsideBlock,
    StructuralObjectFieldMissing,
    StructuralArrayIndexOutOfBounds,
    DecodeBoundaryInStructuralPath,
    CommandCodeMissingOrInvalid,
    CommandIndentMissingOrInvalid,
    CommandParametersMissing,
    CommandParameterNotArray,
    CommandCodeOutsideTextBlock,
    CommandTextMissing,
    CommandParameterNotString,
    CommandParameterMissing,
    CommandTemplateNotObject,
    CommandTemplateParametersMissing,
    CommandTemplateTextMissing,
}

impl RpgMakerWriteBackMutationViolation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PluginParameterAsDocument => "plugin_parameter_as_document",
            Self::RequestedDocumentMissing => "requested_document_missing",
            Self::CrossDocumentMutation => "cross_document_mutation",
            Self::DuplicateStructuralTarget => "duplicate_structural_target",
            Self::DecodeBoundaryInEventContainer => "decode_boundary_in_event_container",
            Self::EventListNotArray => "event_list_not_array",
            Self::OverlappingFrozenRanges => "overlapping_frozen_ranges",
            Self::DecodeTargetNotString => "decode_target_not_string",
            Self::PluginIndexMissing => "plugin_index_missing",
            Self::PluginIndexMismatch => "plugin_index_mismatch",
            Self::PluginNameMismatch => "plugin_name_mismatch",
            Self::PluginParametersNotObject => "plugin_parameters_not_object",
            Self::PluginParameterMissing => "plugin_parameter_missing",
            Self::TargetNotString => "target_not_string",
            Self::ExpectedOriginalMismatch => "expected_original_mismatch",
            Self::ObjectPathMissingOrWrongType => "object_path_missing_or_wrong_type",
            Self::ArrayPathMissingOrWrongType => "array_path_missing_or_wrong_type",
            Self::UnexpectedDecodeBoundary => "unexpected_decode_boundary",
            Self::MissingCommandArrayIndex => "missing_command_array_index",
            Self::CommandPathNotArrayIndex => "command_path_not_array_index",
            Self::ChoiceStartOutOfBounds => "choice_start_out_of_bounds",
            Self::ChoiceStartNot102 => "choice_start_not_102",
            Self::FrozenChoicesMismatch => "frozen_choices_mismatch",
            Self::InvalidChoiceIndex => "invalid_choice_index",
            Self::MissingChoiceEnd => "missing_choice_end",
            Self::ChoiceRecipeTargetMismatch => "choice_recipe_target_mismatch",
            Self::EventBodyStartOutOfBounds => "event_body_start_out_of_bounds",
            Self::EventBodyCodeMismatch => "event_body_code_mismatch",
            Self::EventBodyTooLong => "event_body_too_long",
            Self::IncompleteEventBodyCoverage => "incomplete_event_body_coverage",
            Self::FrozenBodyCodeMismatch => "frozen_body_code_mismatch",
            Self::FrozenBodyMismatch => "frozen_body_mismatch",
            Self::DialogueRecipeLocationMismatch => "dialogue_recipe_location_mismatch",
            Self::DialogueStartOutOfBounds => "dialogue_start_out_of_bounds",
            Self::DialogueStartNot101 => "dialogue_start_not_101",
            Self::DialogueRecipeTooLong => "dialogue_recipe_too_long",
            Self::IncompleteDialogueCoverage => "incomplete_dialogue_coverage",
            Self::FrozenSpeakerMismatch => "frozen_speaker_mismatch",
            Self::FrozenDialogueCodeMismatch => "frozen_dialogue_code_mismatch",
            Self::FrozenDialogueBodyMismatch => "frozen_dialogue_body_mismatch",
            Self::StructureAfterBody => "structure_after_body",
            Self::TranslationWithoutBodyRecipe => "translation_without_body_recipe",
            Self::FrozenBodyMissingSpeakerShell => "frozen_body_missing_speaker_shell",
            Self::BodyLineNotLast => "body_line_not_last",
            Self::MissingEmbeddedSpeaker => "missing_embedded_speaker",
            Self::DialogueParameterOutsideBlock => "dialogue_parameter_outside_block",
            Self::EventBodySegmentOutsideBlock => "event_body_segment_outside_block",
            Self::StructuralObjectFieldMissing => "structural_object_field_missing",
            Self::StructuralArrayIndexOutOfBounds => "structural_array_index_out_of_bounds",
            Self::DecodeBoundaryInStructuralPath => "decode_boundary_in_structural_path",
            Self::CommandCodeMissingOrInvalid => "command_code_missing_or_invalid",
            Self::CommandIndentMissingOrInvalid => "command_indent_missing_or_invalid",
            Self::CommandParametersMissing => "command_parameters_missing",
            Self::CommandParameterNotArray => "command_parameter_not_array",
            Self::CommandCodeOutsideTextBlock => "command_code_outside_text_block",
            Self::CommandTextMissing => "command_text_missing",
            Self::CommandParameterNotString => "command_parameter_not_string",
            Self::CommandParameterMissing => "command_parameter_missing",
            Self::CommandTemplateNotObject => "command_template_not_object",
            Self::CommandTemplateParametersMissing => "command_template_parameters_missing",
            Self::CommandTemplateTextMissing => "command_template_text_missing",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerWriteBackDocumentRewriteProblem {
    InvalidMutation {
        location: RpgMakerDiagnosticLocation,
        violation: RpgMakerWriteBackMutationViolation,
    },
    DecodeNestedJson {
        location: RpgMakerDiagnosticLocation,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    EncodeNestedJson {
        location: RpgMakerDiagnosticLocation,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    SerializeDocument {
        path: SafePath,
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    InvalidOutputPath {
        path: SafePath,
    },
    DuplicateOutputPath {
        path: SafePath,
    },
    OrdinalCaseKeyInputTooLarge {
        path: SafePath,
        observed: u64,
        maximum: u64,
    },
    OrdinalCaseKeyIo {
        path: SafePath,
        phase: FileSystemOrdinalKeyPhase,
        failure: IoFailure,
    },
    MissingChangedDocument {
        path: SafePath,
    },
    PluginOrderMismatch {
        expected_index: usize,
        stored_index: usize,
    },
    RewriteCompute {
        failure: RpgMakerComputeFailure,
    },
}

impl RpgMakerWriteBackDocumentRewriteProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidMutation { .. } => "rpg_maker.write_back.rewrite.invalid_mutation",
            Self::DecodeNestedJson { .. } => "rpg_maker.write_back.rewrite.decode_nested_json",
            Self::EncodeNestedJson { .. } => "rpg_maker.write_back.rewrite.encode_nested_json",
            Self::SerializeDocument { .. } => "rpg_maker.write_back.rewrite.serialize_document",
            Self::InvalidOutputPath { .. } => "rpg_maker.write_back.rewrite.invalid_output_path",
            Self::DuplicateOutputPath { .. } => {
                "rpg_maker.write_back.rewrite.duplicate_output_path"
            }
            Self::OrdinalCaseKeyInputTooLarge { .. } => {
                "rpg_maker.write_back.rewrite.ordinal_case_key_input_too_large"
            }
            Self::OrdinalCaseKeyIo { .. } => "rpg_maker.write_back.rewrite.ordinal_case_key_io",
            Self::MissingChangedDocument { .. } => {
                "rpg_maker.write_back.rewrite.missing_changed_document"
            }
            Self::PluginOrderMismatch { .. } => {
                "rpg_maker.write_back.rewrite.plugin_order_mismatch"
            }
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::Cancelled,
            } => "rpg_maker.write_back.rewrite.cancelled",
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::ExecutorClosed,
            } => "rpg_maker.write_back.rewrite.executor_closed",
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::StatePoisoned,
            } => "rpg_maker.write_back.rewrite.state_poisoned",
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::WorkerPanicked,
            } => "rpg_maker.write_back.rewrite.worker_panicked",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::InvalidMutation { .. }
            | Self::DecodeNestedJson { .. }
            | Self::PluginOrderMismatch { .. } => DiagnosticResolution::CheckProjectState,
            Self::OrdinalCaseKeyIo { .. }
            | Self::RewriteCompute {
                failure: RpgMakerComputeFailure::Cancelled | RpgMakerComputeFailure::ExecutorClosed,
            } => DiagnosticResolution::Retry,
            Self::EncodeNestedJson { .. }
            | Self::SerializeDocument { .. }
            | Self::InvalidOutputPath { .. }
            | Self::DuplicateOutputPath { .. }
            | Self::OrdinalCaseKeyInputTooLarge { .. }
            | Self::MissingChangedDocument { .. }
            | Self::RewriteCompute { .. } => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidMutation { .. } | Self::DecodeNestedJson { .. } => "state_mismatch",
            Self::OrdinalCaseKeyIo { .. } => "external_service_unavailable",
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::Cancelled,
            } => "cancelled",
            Self::RewriteCompute {
                failure: RpgMakerComputeFailure::ExecutorClosed,
            } => "unavailable",
            Self::OrdinalCaseKeyInputTooLarge { .. } => "resource_limit",
            Self::PluginOrderMismatch { .. } => "state_mismatch",
            _ => "internal_invariant",
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::InvalidMutation { location, .. }
            | Self::DecodeNestedJson { location, .. }
            | Self::EncodeNestedJson { location, .. } => {
                format!("{}:{}", location.source_fact(), location.steps_fact())
            }
            Self::SerializeDocument { path, .. }
            | Self::InvalidOutputPath { path }
            | Self::DuplicateOutputPath { path }
            | Self::OrdinalCaseKeyInputTooLarge { path, .. }
            | Self::OrdinalCaseKeyIo { path, .. }
            | Self::MissingChangedDocument { path } => path.to_string(),
            Self::PluginOrderMismatch { .. } => "plugins.js".to_owned(),
            Self::RewriteCompute { .. } => "rpg_maker_write_back_rewrite".to_owned(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidMutation {
                location,
                violation,
            } => vec![
                ("source", location.source_fact()),
                ("location_steps", location.steps_fact()),
                ("mutation_violation", violation.as_str().to_owned()),
            ],
            Self::DecodeNestedJson {
                location,
                category,
                line,
                column,
            }
            | Self::EncodeNestedJson {
                location,
                category,
                line,
                column,
            } => {
                let mut facts = location_facts(location);
                append_json_facts(&mut facts, *category, *line, *column);
                facts
            }
            Self::SerializeDocument {
                path,
                category,
                line,
                column,
            } => {
                let mut facts = vec![("path", path.to_string())];
                append_json_facts(&mut facts, *category, *line, *column);
                facts
            }
            Self::InvalidOutputPath { path }
            | Self::DuplicateOutputPath { path }
            | Self::MissingChangedDocument { path } => vec![("path", path.to_string())],
            Self::OrdinalCaseKeyInputTooLarge {
                path,
                observed,
                maximum,
            } => vec![
                ("path", path.to_string()),
                ("observed", observed.to_string()),
                ("maximum", maximum.to_string()),
            ],
            Self::OrdinalCaseKeyIo {
                path,
                phase,
                failure,
            } => {
                let mut facts = vec![
                    ("path", path.to_string()),
                    ("phase", phase.as_str().to_owned()),
                    ("io_kind", failure.kind.as_str().to_owned()),
                ];
                if let Some(code) = failure.raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
                facts
            }
            Self::PluginOrderMismatch {
                expected_index,
                stored_index,
            } => vec![
                ("expected_index", expected_index.to_string()),
                ("stored_index", stored_index.to_string()),
            ],
            Self::RewriteCompute { failure } => {
                vec![("compute_failure", failure.as_str().to_owned())]
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionComputeOperation {
    BuiltinBuildSnapshot,
    RulesMatchSource,
    RulesBuildSnapshot,
}

impl RpgMakerExtractionComputeOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltinBuildSnapshot => "builtin_build_snapshot",
            Self::RulesMatchSource => "rules_match_source",
            Self::RulesBuildSnapshot => "rules_build_snapshot",
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::BuiltinBuildSnapshot => "rpg_maker.extract.builtin.build_snapshot_failed",
            Self::RulesMatchSource => "rpg_maker.extract.rules.match_source_failed",
            Self::RulesBuildSnapshot => "rpg_maker.extract.rules.build_snapshot_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionStoreOperation {
    ScheduleEncoding,
    EncodeSnapshot,
    EncodeProjectDefinition,
    ReadOwnerState,
    ReadSnapshot,
    ReadProjectDefinition,
    DecideClaimIndexMaintenance,
    DecideUnitIndexMaintenance,
    CommitSnapshot,
}

impl RpgMakerExtractionStoreOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ScheduleEncoding => "schedule_encoding",
            Self::EncodeSnapshot => "encode_snapshot",
            Self::EncodeProjectDefinition => "encode_project_definition",
            Self::ReadOwnerState => "read_owner_state",
            Self::ReadSnapshot => "read_snapshot",
            Self::ReadProjectDefinition => "read_project_definition",
            Self::DecideClaimIndexMaintenance => "decide_claim_index_maintenance",
            Self::DecideUnitIndexMaintenance => "decide_unit_index_maintenance",
            Self::CommitSnapshot => "commit_snapshot",
        }
    }

    const fn backend_code(self) -> &'static str {
        match self {
            Self::ScheduleEncoding => "rpg_maker.extract.store.schedule_encoding_failed",
            Self::EncodeSnapshot => "rpg_maker.extract.store.encode_snapshot_failed",
            Self::EncodeProjectDefinition => {
                "rpg_maker.extract.store.encode_project_definition_failed"
            }
            Self::ReadOwnerState => "rpg_maker.extract.store.read_owner_state_failed",
            Self::ReadSnapshot => "rpg_maker.extract.store.read_snapshot_failed",
            Self::ReadProjectDefinition => "rpg_maker.extract.store.read_project_definition_failed",
            Self::DecideClaimIndexMaintenance => {
                "rpg_maker.extract.store.decide_claim_index_maintenance_failed"
            }
            Self::DecideUnitIndexMaintenance => {
                "rpg_maker.extract.store.decide_unit_index_maintenance_failed"
            }
            Self::CommitSnapshot => "rpg_maker.extract.store.commit_snapshot_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionClaimSummaryViolation {
    MixedAccess,
    MultipleExclusive,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionSnapshotEncodingViolation {
    InvalidLocation {
        failure: RpgMakerLocationCodecFailure,
    },
    InvalidProjection {
        failure: RpgMakerProjectionCodecFailure,
    },
    InvalidSourceContentJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    InvalidSourceContextJson {
        category: RpgMakerJsonFailureKind,
        line: usize,
        column: usize,
    },
    DuplicateGroupLocation {
        group_location: RpgMakerDiagnosticLocation,
    },
    InvalidClaimSummary {
        resource: RpgMakerDiagnosticLocation,
        violation: RpgMakerExtractionClaimSummaryViolation,
    },
}

impl RpgMakerExtractionSnapshotEncodingViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidLocation { .. } => "rpg_maker.extract.store.snapshot.invalid_location",
            Self::InvalidProjection { .. } => "rpg_maker.extract.store.snapshot.invalid_projection",
            Self::InvalidSourceContentJson { .. } => {
                "rpg_maker.extract.store.snapshot.invalid_source_content_json"
            }
            Self::InvalidSourceContextJson { .. } => {
                "rpg_maker.extract.store.snapshot.invalid_source_context_json"
            }
            Self::DuplicateGroupLocation { .. } => {
                "rpg_maker.extract.store.snapshot.duplicate_group_location"
            }
            Self::InvalidClaimSummary { .. } => {
                "rpg_maker.extract.store.snapshot.invalid_claim_summary"
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionStoredDefinitionViolation {
    Missing,
    Multiple,
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: SafeIdentifier,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    Invalid {
        problem: RpgMakerDialogueDefinitionProblem,
    },
    NonCanonical,
}

impl RpgMakerExtractionStoredDefinitionViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::Missing => "rpg_maker.extract.store.project_definition_missing",
            Self::Multiple => "rpg_maker.extract.store.project_definition_multiple",
            Self::WrongColumnCount { .. } => {
                "rpg_maker.extract.store.project_definition_wrong_column_count"
            }
            Self::WrongColumnType { .. } => {
                "rpg_maker.extract.store.project_definition_wrong_column_type"
            }
            Self::Invalid { .. } => "rpg_maker.extract.store.project_definition_invalid",
            Self::NonCanonical => "rpg_maker.extract.store.project_definition_non_canonical",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionIndexDecisionViolation {
    RowCount {
        maximum: usize,
        actual: usize,
    },
    ColumnCount {
        expected: usize,
        actual: usize,
    },
    Value {
        expected_integer: i64,
        actual_kind: SafeIdentifier,
        actual_integer: Option<i64>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionConflictRowViolation {
    UnexpectedQueryId {
        actual: SafeText,
    },
    ColumnCount {
        expected: usize,
        actual: usize,
    },
    ColumnType {
        column: SafeIdentifier,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    UnknownOwner {
        column: SafeIdentifier,
    },
    UnknownAccess {
        column: SafeIdentifier,
    },
    InvalidGroupLocation {
        column: SafeIdentifier,
        failure: RpgMakerLocationCodecFailure,
    },
    NonCanonicalGroupLocation {
        column: SafeIdentifier,
    },
    InvalidResource {
        failure: RpgMakerProjectionCodecFailure,
    },
    NonCanonicalResource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerExtractionMutationConflict {
    resource: RpgMakerDiagnosticLocation,
    incoming_owner: RpgMakerDiagnosticOwner,
    incoming_group: RpgMakerDiagnosticLocation,
    incoming_access: RpgMakerMutationAccess,
    current_owner: RpgMakerDiagnosticOwner,
    current_group: RpgMakerDiagnosticLocation,
    current_access: RpgMakerMutationAccess,
}

impl RpgMakerExtractionMutationConflict {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        resource: RpgMakerDiagnosticLocation,
        incoming_owner: RpgMakerDiagnosticOwner,
        incoming_group: RpgMakerDiagnosticLocation,
        incoming_access: RpgMakerMutationAccess,
        current_owner: RpgMakerDiagnosticOwner,
        current_group: RpgMakerDiagnosticLocation,
        current_access: RpgMakerMutationAccess,
    ) -> Self {
        Self {
            resource,
            incoming_owner,
            incoming_group,
            incoming_access,
            current_owner,
            current_group,
            current_access,
        }
    }

    fn subject(&self) -> String {
        format!(
            "{}:{}",
            self.resource.source_fact(),
            self.resource.steps_fact()
        )
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("resource_source", self.resource.source_fact()),
            ("resource_location", self.resource.steps_fact()),
            ("incoming_owner", self.incoming_owner.as_str().to_owned()),
            ("incoming_group", self.incoming_group.steps_fact()),
            ("incoming_access", self.incoming_access.as_str().to_owned()),
            ("current_owner", self.current_owner.as_str().to_owned()),
            ("current_group", self.current_group.steps_fact()),
            ("current_access", self.current_access.as_str().to_owned()),
        ]
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionStoreProblem {
    Backend {
        cause: RpgMakerBackendCause,
    },
    DatabaseNotFound,
    SnapshotEncoding {
        violation: RpgMakerExtractionSnapshotEncodingViolation,
    },
    ProjectDefinitionEncoding {
        problem: RpgMakerDialogueDefinitionProblem,
    },
    UnexpectedQueryResultSetCount {
        expected: usize,
        actual: usize,
    },
    InvalidStoredProjectDefinition {
        violation: RpgMakerExtractionStoredDefinitionViolation,
    },
    InvalidClaimIndexDecision {
        violation: RpgMakerExtractionIndexDecisionViolation,
    },
    InvalidUnitIndexDecision {
        violation: RpgMakerExtractionIndexDecisionViolation,
    },
    MutationClaimConflict {
        conflict: RpgMakerExtractionMutationConflict,
        transaction: SqliteTransactionState,
    },
    ConcurrentModification {
        transaction: SqliteTransactionState,
    },
    InvalidConflictRow {
        violation: RpgMakerExtractionConflictRowViolation,
        transaction: SqliteTransactionState,
    },
    MutationClaimConflictOutcomeUnknown {
        conflict: RpgMakerExtractionMutationConflict,
        cause: RpgMakerBackendCause,
    },
    InvalidConflictRowOutcomeUnknown {
        violation: RpgMakerExtractionConflictRowViolation,
        cause: RpgMakerBackendCause,
    },
    NotCommitted {
        cause: RpgMakerBackendCause,
        transaction: SqliteTransactionState,
    },
    OutcomeUnknown {
        cause: RpgMakerBackendCause,
        transaction: SqliteTransactionState,
    },
}

impl RpgMakerExtractionStoreProblem {
    fn code(&self, operation: RpgMakerExtractionStoreOperation) -> &'static str {
        match self {
            Self::Backend { .. } => operation.backend_code(),
            Self::DatabaseNotFound => "rpg_maker.extract.store.database_not_found",
            Self::SnapshotEncoding { violation } => violation.code(),
            Self::ProjectDefinitionEncoding { .. } => {
                "rpg_maker.extract.store.project_definition_encoding_failed"
            }
            Self::UnexpectedQueryResultSetCount { .. } => {
                "rpg_maker.extract.store.unexpected_query_result_set_count"
            }
            Self::InvalidStoredProjectDefinition { violation } => violation.code(),
            Self::InvalidClaimIndexDecision { .. } => {
                "rpg_maker.extract.store.invalid_claim_index_decision"
            }
            Self::InvalidUnitIndexDecision { .. } => {
                "rpg_maker.extract.store.invalid_unit_index_decision"
            }
            Self::MutationClaimConflict { .. } => "rpg_maker.extract.store.mutation_claim_conflict",
            Self::ConcurrentModification { .. } => {
                "rpg_maker.extract.store.concurrent_modification"
            }
            Self::InvalidConflictRow { .. } => "rpg_maker.extract.store.invalid_conflict_row",
            Self::MutationClaimConflictOutcomeUnknown { .. } => {
                "rpg_maker.extract.store.mutation_claim_conflict_outcome_unknown"
            }
            Self::InvalidConflictRowOutcomeUnknown { .. } => {
                "rpg_maker.extract.store.invalid_conflict_row_outcome_unknown"
            }
            Self::NotCommitted { .. } => "rpg_maker.extract.store.not_committed",
            Self::OutcomeUnknown { .. } => "rpg_maker.extract.store.outcome_unknown",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Backend { cause } => cause.diagnostic.resolution(),
            Self::DatabaseNotFound
            | Self::InvalidStoredProjectDefinition { .. }
            | Self::MutationClaimConflict { .. }
            | Self::InvalidConflictRow { .. }
            | Self::NotCommitted { .. } => DiagnosticResolution::CheckProjectState,
            Self::ConcurrentModification { .. } => DiagnosticResolution::Retry,
            Self::SnapshotEncoding { .. }
            | Self::ProjectDefinitionEncoding { .. }
            | Self::UnexpectedQueryResultSetCount { .. }
            | Self::InvalidClaimIndexDecision { .. }
            | Self::InvalidUnitIndexDecision { .. } => DiagnosticResolution::ReportBug,
            Self::MutationClaimConflictOutcomeUnknown { .. }
            | Self::InvalidConflictRowOutcomeUnknown { .. }
            | Self::OutcomeUnknown { .. } => DiagnosticResolution::PreserveRecoveryArtifacts,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::Backend { .. } => "external_service_unavailable",
            Self::DatabaseNotFound => "not_found",
            Self::SnapshotEncoding { .. }
            | Self::ProjectDefinitionEncoding { .. }
            | Self::UnexpectedQueryResultSetCount { .. }
            | Self::InvalidClaimIndexDecision { .. }
            | Self::InvalidUnitIndexDecision { .. } => "internal_invariant",
            Self::InvalidStoredProjectDefinition { .. } | Self::InvalidConflictRow { .. } => {
                "invalid_value"
            }
            Self::MutationClaimConflict { .. } => "conflicting_values",
            Self::ConcurrentModification { .. } => "concurrent_modification",
            Self::MutationClaimConflictOutcomeUnknown { .. }
            | Self::InvalidConflictRowOutcomeUnknown { .. }
            | Self::OutcomeUnknown { .. } => "transaction_outcome_unknown",
            Self::NotCommitted { .. } => "transaction_rolled_back",
        }
    }

    fn subject(&self, database_path: &SafePath) -> String {
        match self {
            Self::MutationClaimConflict { conflict, .. }
            | Self::MutationClaimConflictOutcomeUnknown { conflict, .. } => conflict.subject(),
            _ => database_path.to_string(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Backend { cause } => vec![("backend_code", cause.diagnostic.code().to_owned())],
            Self::DatabaseNotFound => Vec::new(),
            Self::SnapshotEncoding { violation } => {
                vec![("encoding_violation", violation.code().to_owned())]
            }
            Self::ProjectDefinitionEncoding { problem } => {
                let mut facts = vec![("definition_kind", "mv_dialogue_rules".to_owned())];
                facts.extend(problem.facts());
                facts
            }
            Self::UnexpectedQueryResultSetCount { expected, actual } => vec![
                ("expected_query_result_sets", expected.to_string()),
                ("actual_query_result_sets", actual.to_string()),
            ],
            Self::InvalidStoredProjectDefinition { violation } => {
                vec![("stored_definition_violation", violation.code().to_owned())]
            }
            Self::InvalidClaimIndexDecision { violation }
            | Self::InvalidUnitIndexDecision { violation } => match violation {
                RpgMakerExtractionIndexDecisionViolation::RowCount { maximum, actual } => {
                    vec![
                        ("maximum_rows", maximum.to_string()),
                        ("actual_rows", actual.to_string()),
                    ]
                }
                RpgMakerExtractionIndexDecisionViolation::ColumnCount { expected, actual } => {
                    vec![
                        ("expected_columns", expected.to_string()),
                        ("actual_columns", actual.to_string()),
                    ]
                }
                RpgMakerExtractionIndexDecisionViolation::Value {
                    expected_integer,
                    actual_kind,
                    actual_integer,
                } => vec![
                    ("expected_integer", expected_integer.to_string()),
                    ("actual_kind", actual_kind.to_string()),
                    (
                        "actual_integer",
                        actual_integer.map_or_else(|| "none".to_owned(), |value| value.to_string()),
                    ),
                ],
            },
            Self::MutationClaimConflict {
                conflict,
                transaction,
            } => {
                let mut facts = conflict.facts();
                facts.push((
                    "transaction",
                    sqlite_transaction_state_name(*transaction).to_owned(),
                ));
                facts
            }
            Self::ConcurrentModification { transaction } => vec![(
                "transaction",
                sqlite_transaction_state_name(*transaction).to_owned(),
            )],
            Self::InvalidConflictRow {
                violation: _,
                transaction,
            } => vec![(
                "transaction",
                sqlite_transaction_state_name(*transaction).to_owned(),
            )],
            Self::MutationClaimConflictOutcomeUnknown { conflict, cause } => {
                let mut facts = conflict.facts();
                facts.push(("transaction", "outcome_unknown".to_owned()));
                facts.push(("backend_code", cause.diagnostic.code().to_owned()));
                facts
            }
            Self::InvalidConflictRowOutcomeUnknown {
                violation: _,
                cause,
            } => vec![
                ("transaction", "outcome_unknown".to_owned()),
                ("backend_code", cause.diagnostic.code().to_owned()),
            ],
            Self::NotCommitted { cause, transaction }
            | Self::OutcomeUnknown { cause, transaction } => vec![
                (
                    "transaction",
                    sqlite_transaction_state_name(*transaction).to_owned(),
                ),
                ("backend_code", cause.diagnostic.code().to_owned()),
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerExtractionProblem {
    Snapshot {
        source: RpgMakerExtractionSource,
        violation: RpgMakerExtractionSnapshotViolation,
    },
    Compute {
        source: RpgMakerExtractionSource,
        operation: RpgMakerExtractionComputeOperation,
        cause: RpgMakerBackendCause,
    },
    Store {
        owner: RpgMakerDiagnosticOwner,
        database_path: SafePath,
        operation: RpgMakerExtractionStoreOperation,
        problem: RpgMakerExtractionStoreProblem,
    },
}

impl RpgMakerExtractionProblem {
    const fn stage(&self) -> RpgMakerDiagnosticStage {
        match self {
            Self::Snapshot { source, .. } | Self::Compute { source, .. } => source.stage(),
            Self::Store { .. } => RpgMakerDiagnosticStage::ExtractStore,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Snapshot { violation, .. } => violation.code(),
            Self::Compute { operation, .. } => operation.code(),
            Self::Store {
                operation, problem, ..
            } => problem.code(*operation),
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::Snapshot { .. } => DiagnosticResolution::ReportBug,
            Self::Compute { cause, .. } => cause.diagnostic.resolution(),
            Self::Store { problem, .. } => problem.resolution(),
        }
    }

    fn summary_code(&self) -> &'static str {
        match self {
            Self::Snapshot { .. } => "internal_invariant",
            Self::Compute { cause, .. } => cause.diagnostic.issue().summary_code(),
            Self::Store { problem, .. } => problem.summary_code(),
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::Snapshot { source, violation } => {
                violation.subject().unwrap_or_else(|| source.subject())
            }
            Self::Compute { source, .. } => source.subject(),
            Self::Store {
                database_path,
                problem,
                ..
            } => problem.subject(database_path),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Snapshot { source, violation } => {
                let mut facts = source.facts();
                facts.extend(violation.facts());
                facts
            }
            Self::Compute {
                source,
                operation,
                cause,
            } => {
                let mut facts = source.facts();
                facts.push(("operation", operation.as_str().to_owned()));
                facts.push(("backend_code", cause.diagnostic.code().to_owned()));
                facts
            }
            Self::Store {
                owner,
                database_path,
                operation,
                problem,
            } => {
                let mut facts = vec![
                    ("owner", owner.as_str().to_owned()),
                    ("database_path", database_path.to_string()),
                    ("operation", operation.as_str().to_owned()),
                ];
                facts.extend(problem.facts());
                facts
            }
        }
    }
}

fn join_usize(values: &[usize]) -> String {
    values
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

const fn sqlite_transaction_state_name(state: SqliteTransactionState) -> &'static str {
    match state {
        SqliteTransactionState::NotStarted => "not_started",
        SqliteTransactionState::Active => "active",
        SqliteTransactionState::Committed => "committed",
        SqliteTransactionState::RolledBack => "rolled_back",
        SqliteTransactionState::FinalizationFailed => "finalization_failed",
        SqliteTransactionState::OutcomeUnknown => "outcome_unknown",
    }
}

impl RpgMakerResultStoreProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlan { .. } => "rpg_maker.translate.result_store.invalid_plan",
            Self::DatabaseNotFound { .. } => "rpg_maker.translate.result_store.database_not_found",
            Self::StalePlan { .. } => "rpg_maker.translate.result_store.stale_plan",
            Self::SessionDatabaseChanged { .. } => {
                "rpg_maker.translate.result_store.session_database_changed"
            }
            Self::SessionFinalized { .. } => "rpg_maker.translate.result_store.session_finalized",
            Self::FinalizationRolledBackTransaction { .. } => {
                "rpg_maker.translate.result_store.finalization_rolled_back"
            }
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::DatabaseNotFound { .. } => DiagnosticResolution::CheckProjectState,
            Self::StalePlan { .. } => DiagnosticResolution::Retry,
            Self::InvalidPlan { .. }
            | Self::SessionDatabaseChanged { .. }
            | Self::SessionFinalized { .. }
            | Self::FinalizationRolledBackTransaction { .. } => DiagnosticResolution::ReportBug,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidPlan { .. }
            | Self::SessionDatabaseChanged { .. }
            | Self::SessionFinalized { .. } => "internal_invariant",
            Self::DatabaseNotFound { .. } => "not_found",
            Self::StalePlan { .. } => "concurrent_modification",
            Self::FinalizationRolledBackTransaction { .. } => "finalization_failed",
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::InvalidPlan { .. } => "translation_result_store".to_owned(),
            Self::DatabaseNotFound { path }
            | Self::StalePlan { path }
            | Self::SessionFinalized { path }
            | Self::FinalizationRolledBackTransaction { path } => path.to_string(),
            Self::SessionDatabaseChanged { requested_path, .. } => requested_path.to_string(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidPlan { violation } => {
                let mut facts = vec![("violation", violation.as_str().to_owned())];
                match violation {
                    RpgMakerResultStorePlanViolation::LocationCodec { failure } => {
                        append_location_codec_facts(&mut facts, failure)
                    }
                    RpgMakerResultStorePlanViolation::ProjectionCodec { failure } => {
                        append_projection_codec_facts(&mut facts, failure)
                    }
                    RpgMakerResultStorePlanViolation::ContentJson {
                        category,
                        line,
                        column,
                    } => append_json_facts(&mut facts, *category, *line, *column),
                    _ => {}
                }
                facts
            }
            Self::DatabaseNotFound { path }
            | Self::StalePlan { path }
            | Self::SessionFinalized { path }
            | Self::FinalizationRolledBackTransaction { path } => vec![("path", path.to_string())],
            Self::SessionDatabaseChanged {
                opened_path,
                requested_path,
            } => vec![
                ("opened_path", opened_path.to_string()),
                ("requested_path", requested_path.to_string()),
            ],
        }
    }
}

fn append_json_facts(
    facts: &mut Vec<(&'static str, String)>,
    category: RpgMakerJsonFailureKind,
    line: usize,
    column: usize,
) {
    facts.push(("json_category", category.as_str().to_owned()));
    facts.push(("line", line.to_string()));
    facts.push(("column", column.to_string()));
}

fn append_location_codec_facts(
    facts: &mut Vec<(&'static str, String)>,
    failure: &RpgMakerLocationCodecFailure,
) {
    match failure {
        RpgMakerLocationCodecFailure::Json {
            operation,
            category,
            line,
            column,
        } => {
            facts.push(("codec_operation", operation.as_str().to_owned()));
            append_json_facts(facts, *category, *line, *column);
        }
        RpgMakerLocationCodecFailure::NonCanonical => {
            facts.push(("codec_failure", "non_canonical".to_owned()));
        }
        RpgMakerLocationCodecFailure::InvalidDataFile => {
            facts.push(("codec_failure", "invalid_data_file".to_owned()));
        }
        RpgMakerLocationCodecFailure::InvalidMapId { map_id } => {
            facts.push(("codec_failure", "invalid_map_id".to_owned()));
            facts.push(("map_id", map_id.to_string()));
        }
    }
}

fn append_projection_codec_facts(
    facts: &mut Vec<(&'static str, String)>,
    failure: &RpgMakerProjectionCodecFailure,
) {
    match failure {
        RpgMakerProjectionCodecFailure::Json {
            operation,
            category,
            line,
            column,
        } => {
            facts.push(("codec_operation", operation.as_str().to_owned()));
            append_json_facts(facts, *category, *line, *column);
        }
        RpgMakerProjectionCodecFailure::NonCanonical => {
            facts.push(("codec_failure", "non_canonical".to_owned()));
        }
        RpgMakerProjectionCodecFailure::Location { failure } => {
            facts.push(("codec_failure", "invalid_location".to_owned()));
            append_location_codec_facts(facts, failure);
        }
        RpgMakerProjectionCodecFailure::Projection { violation } => {
            facts.push(("codec_failure", "invalid_projection".to_owned()));
            facts.push(("projection_violation", violation.as_str().to_owned()));
            match violation {
                RpgMakerProjectionModelViolation::DuplicateProjectionSlot {
                    role,
                    source_line_index,
                } => {
                    facts.push(("role", role.fact_value()));
                    if let Some(source_line_index) = source_line_index {
                        facts.push(("source_line_index", source_line_index.to_string()));
                    }
                }
                RpgMakerProjectionModelViolation::DuplicateDialogueBodyLine {
                    source_line_index,
                } => facts.push(("source_line_index", source_line_index.to_string())),
                RpgMakerProjectionModelViolation::NonContiguousDialogueBodyLines {
                    expected,
                    actual,
                } => {
                    facts.push(("expected", expected.to_string()));
                    facts.push(("actual", actual.to_string()));
                }
                _ => {}
            }
        }
    }
}

impl RpgMakerInitialSetting {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SourceLanguage => "source_language",
            Self::TargetLanguage => "target_language",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProjectProblem {
    InvalidGameLayout {
        game_root: SafePath,
        engine: RpgMakerEngineKind,
        data_relative: SafePath,
        js_relative: SafePath,
        core_script: SafeIdentifier,
    },
    MissingInitialSettings {
        settings: Vec<RpgMakerInitialSetting>,
    },
    DatabaseNotFound {
        path: SafePath,
    },
    DatabaseAlreadyExists {
        path: SafePath,
    },
    InvalidDatabase {
        path: SafePath,
    },
    InvalidMetadata {
        path: SafePath,
        violation: RpgMakerProjectMetadataViolation,
    },
    ConcurrentModification {
        path: SafePath,
    },
    SourceSnapshotMismatch {
        persisted: SafeIdentifier,
        observed: SafeIdentifier,
    },
    RunPlanRequired,
    SavedProfileUnavailable {
        profile_id: SafeIdentifier,
    },
    InvalidRunPlan {
        path: SafePath,
        violation: RpgMakerRunPlanSnapshotViolation,
    },
    InvalidRunPlanValue {
        field: SafeIdentifier,
        violation: RpgMakerRunPlanValueViolation,
    },
    RunPlanPersistenceInvariant {
        path: SafePath,
        query_id: Option<SafeText>,
        selected_columns: Option<u64>,
    },
}

impl RpgMakerProjectProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidGameLayout { .. } => "rpg_maker.init.invalid_game_layout",
            Self::MissingInitialSettings { .. } => "rpg_maker.init.missing_initial_settings",
            Self::DatabaseNotFound { .. } => "rpg_maker.project.database_not_found",
            Self::DatabaseAlreadyExists { .. } => "rpg_maker.project.database_already_exists",
            Self::InvalidDatabase { .. } => "rpg_maker.project.invalid_database",
            Self::InvalidMetadata { violation, .. } => violation.code(),
            Self::ConcurrentModification { .. } => "rpg_maker.project.concurrent_modification",
            Self::SourceSnapshotMismatch { .. } => "rpg_maker.project.source_snapshot_mismatch",
            Self::RunPlanRequired => "rpg_maker.run_plan.required",
            Self::SavedProfileUnavailable { .. } => "rpg_maker.run_plan.profile_unavailable",
            Self::InvalidRunPlan { .. } => "rpg_maker.run_plan.invalid",
            Self::InvalidRunPlanValue { .. } => "rpg_maker.run_plan.invalid_value",
            Self::RunPlanPersistenceInvariant { .. } => "rpg_maker.run_plan.persistence_invariant",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::InvalidGameLayout { .. } | Self::MissingInitialSettings { .. } => {
                DiagnosticResolution::FixInput
            }
            Self::DatabaseAlreadyExists { .. } | Self::ConcurrentModification { .. } => {
                DiagnosticResolution::ResolveContention
            }
            Self::RunPlanRequired | Self::SavedProfileUnavailable { .. } => {
                DiagnosticResolution::FixConfiguration
            }
            Self::DatabaseNotFound { .. }
            | Self::InvalidDatabase { .. }
            | Self::InvalidMetadata { .. }
            | Self::SourceSnapshotMismatch { .. }
            | Self::InvalidRunPlan { .. } => DiagnosticResolution::CheckProjectState,
            Self::RunPlanPersistenceInvariant { .. } => DiagnosticResolution::ReportBug,
            Self::InvalidRunPlanValue {
                violation: RpgMakerRunPlanValueViolation::UnsafeProfileId,
                ..
            } => DiagnosticResolution::CheckProjectState,
            Self::InvalidRunPlanValue { .. } => DiagnosticResolution::FixInput,
        }
    }

    const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidGameLayout { .. }
            | Self::InvalidDatabase { .. }
            | Self::InvalidMetadata { .. }
            | Self::InvalidRunPlan { .. }
            | Self::InvalidRunPlanValue { .. } => "invalid_value",
            Self::MissingInitialSettings { .. } | Self::RunPlanRequired => "missing_required_value",
            Self::DatabaseNotFound { .. } => "not_found",
            Self::DatabaseAlreadyExists { .. } => "target_already_exists",
            Self::ConcurrentModification { .. } => "concurrent_modification",
            Self::SourceSnapshotMismatch { .. } => "source_snapshot_mismatch",
            Self::SavedProfileUnavailable { .. } => "profile_not_found",
            Self::RunPlanPersistenceInvariant { .. } => "internal_invariant",
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::InvalidGameLayout { game_root, .. } => game_root.to_string(),
            Self::MissingInitialSettings { .. } | Self::RunPlanRequired => {
                "rpg_maker_project".to_owned()
            }
            Self::DatabaseNotFound { path }
            | Self::DatabaseAlreadyExists { path }
            | Self::InvalidDatabase { path }
            | Self::InvalidMetadata { path, .. }
            | Self::ConcurrentModification { path }
            | Self::InvalidRunPlan { path, .. }
            | Self::RunPlanPersistenceInvariant { path, .. } => path.to_string(),
            Self::SourceSnapshotMismatch { .. } => "source_snapshot".to_owned(),
            Self::SavedProfileUnavailable { profile_id } => profile_id.to_string(),
            Self::InvalidRunPlanValue { field, .. } => field.to_string(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidGameLayout {
                game_root,
                engine,
                data_relative,
                js_relative,
                core_script,
            } => vec![
                ("game_root", game_root.to_string()),
                ("engine", engine.as_str().to_owned()),
                ("data_relative", data_relative.to_string()),
                ("js_relative", js_relative.to_string()),
                ("core_script", core_script.to_string()),
            ],
            Self::MissingInitialSettings { settings } => vec![(
                "settings",
                settings
                    .iter()
                    .map(|setting| setting.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )],
            Self::DatabaseNotFound { path }
            | Self::DatabaseAlreadyExists { path }
            | Self::InvalidDatabase { path }
            | Self::ConcurrentModification { path } => vec![("path", path.to_string())],
            Self::InvalidMetadata { path, violation } => {
                let mut facts = vec![("path", path.to_string())];
                facts.extend(violation.facts());
                facts
            }
            Self::InvalidRunPlan { path, violation } => {
                let mut facts = vec![("path", path.to_string())];
                facts.extend(violation.facts());
                facts
            }
            Self::RunPlanPersistenceInvariant {
                path,
                query_id,
                selected_columns,
            } => {
                let mut facts = vec![("path", path.to_string())];
                if let Some(query_id) = query_id {
                    facts.push(("query_id", query_id.to_string()));
                }
                if let Some(selected_columns) = selected_columns {
                    facts.push(("selected_columns", selected_columns.to_string()));
                }
                facts
            }
            Self::SavedProfileUnavailable { profile_id } => {
                vec![("profile_id", profile_id.to_string())]
            }
            Self::InvalidRunPlanValue { field, violation } => {
                let mut facts = vec![
                    ("field", field.to_string()),
                    ("violation", violation.as_str().to_owned()),
                ];
                match violation {
                    RpgMakerRunPlanValueViolation::InvalidRulesJson {
                        line,
                        column,
                        category,
                    }
                    | RpgMakerRunPlanValueViolation::RulesJsonEncodingFailed {
                        line,
                        column,
                        category,
                    } => {
                        facts.push(("line", line.to_string()));
                        facts.push(("column", column.to_string()));
                        facts.push(("json_category", category.to_string()));
                    }
                    _ => {}
                }
                facts
            }
            Self::SourceSnapshotMismatch {
                persisted,
                observed,
            } => vec![
                ("persisted_fingerprint", persisted.to_string()),
                ("observed_fingerprint", observed.to_string()),
            ],
            Self::RunPlanRequired => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RpgMakerProblem {
    Project {
        problem: RpgMakerProjectProblem,
    },
    RulesDefinition {
        origin: RpgMakerRulesDefinitionOrigin,
        rules_path: SafePath,
        problem: RpgMakerRulesDefinitionProblem,
    },
    RulesMatch {
        rules_path: SafePath,
        context: Option<RpgMakerRulesMatchContext>,
        problem: RpgMakerRulesMatchProblem,
    },
    RulesCommandNonString {
        fact: RpgMakerRulesCommandNonStringFact,
    },
    RulesOwnerDisabled {
        rules_path: SafePath,
    },
    BuiltinDocument {
        location: RpgMakerDiagnosticLocation,
        problem: RpgMakerBuiltinDocumentProblem,
    },
    DialogueDefinition {
        origin: RpgMakerDialogueDefinitionOrigin,
        path: Option<SafePath>,
        problem: RpgMakerDialogueDefinitionProblem,
    },
    DialogueProjection {
        problem: RpgMakerDialogueProjectionProblem,
    },
    Document {
        consumer: RpgMakerDocumentConsumer,
        operation: RpgMakerDocumentOperation,
        problem: RpgMakerDocumentProblem,
    },
    Extraction {
        problem: RpgMakerExtractionProblem,
    },
    PlaceholderPlanning {
        rule_source: PlaceholderRuleSource,
        unit: RpgMakerUnitLocator,
        problem: PlaceholderIssue,
    },
    PlaceholderProjection {
        rule_source: PlaceholderRuleSource,
        unit: RpgMakerUnitLocator,
        problem: RpgMakerPlaceholderProjectionProblem,
    },
    ResponseProcessing {
        scope: RpgMakerResponseProcessingScope,
        problem: RpgMakerResponseProcessingProblem,
    },
    /// 模型响应协议在仍掌握 Task 或 Unit 定位时被拒绝。
    TaskResponse {
        scope: RpgMakerResponseProcessingScope,
        problem: RpgMakerTaskResponseProblem,
    },
    ResultStore {
        problem: RpgMakerResultStoreProblem,
    },
    TranslationAsset {
        database_path: SafePath,
        problem: RpgMakerTranslationAssetProblem,
    },
    WriteBackAsset {
        database_path: SafePath,
        problem: RpgMakerWriteBackAssetProblem,
    },
    WriteBackPlanning {
        problem: RpgMakerWriteBackPlanningProblem,
    },
    WriteBackDocumentRewrite {
        problem: RpgMakerWriteBackDocumentRewriteProblem,
    },
    TranslationPlanning {
        problem: RpgMakerTranslationPlanningProblem,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpgMakerIssue {
    stage: RpgMakerDiagnosticStage,
    problem: RpgMakerProblem,
}

impl RpgMakerIssue {
    pub(crate) const fn project(
        stage: RpgMakerDiagnosticStage,
        problem: RpgMakerProjectProblem,
    ) -> Self {
        Self {
            stage,
            problem: RpgMakerProblem::Project { problem },
        }
    }

    pub(crate) fn rules_definition(
        origin: RpgMakerRulesDefinitionOrigin,
        rules_path: impl AsRef<std::path::Path>,
        problem: RpgMakerRulesDefinitionProblem,
    ) -> Self {
        Self {
            stage: match origin {
                RpgMakerRulesDefinitionOrigin::ExternalToml => {
                    RpgMakerDiagnosticStage::RulesDefinitionInput
                }
                RpgMakerRulesDefinitionOrigin::ProjectSnapshot => {
                    RpgMakerDiagnosticStage::RulesDefinitionProjectSnapshot
                }
            },
            problem: RpgMakerProblem::RulesDefinition {
                origin,
                rules_path: SafePath::new(rules_path),
                problem,
            },
        }
    }

    pub(crate) fn rules_match(
        rules_path: impl AsRef<std::path::Path>,
        context: Option<RpgMakerRulesMatchContext>,
        problem: RpgMakerRulesMatchProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::ExtractRules,
            problem: RpgMakerProblem::RulesMatch {
                rules_path: SafePath::new(rules_path),
                context,
                problem,
            },
        }
    }

    pub(crate) const fn rules_command_non_string(fact: RpgMakerRulesCommandNonStringFact) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::ExtractRules,
            problem: RpgMakerProblem::RulesCommandNonString { fact },
        }
    }

    pub(crate) fn rules_owner_disabled(rules_path: impl AsRef<std::path::Path>) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::RulesDefinitionInput,
            problem: RpgMakerProblem::RulesOwnerDisabled {
                rules_path: SafePath::new(rules_path),
            },
        }
    }

    pub(crate) const fn builtin_document(
        location: RpgMakerDiagnosticLocation,
        problem: RpgMakerBuiltinDocumentProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::ExtractBuiltin,
            problem: RpgMakerProblem::BuiltinDocument { location, problem },
        }
    }

    pub(crate) fn external_dialogue_definition(
        path: impl AsRef<std::path::Path>,
        problem: RpgMakerDialogueDefinitionProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::DialogueDefinitionInput,
            problem: RpgMakerProblem::DialogueDefinition {
                origin: RpgMakerDialogueDefinitionOrigin::ExternalToml,
                path: Some(SafePath::new(path)),
                problem,
            },
        }
    }

    pub(crate) const fn project_dialogue_definition(
        problem: RpgMakerDialogueDefinitionProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::DialogueDefinitionProjectSnapshot,
            problem: RpgMakerProblem::DialogueDefinition {
                origin: RpgMakerDialogueDefinitionOrigin::ProjectSnapshot,
                path: None,
                problem,
            },
        }
    }

    pub(crate) const fn dialogue_projection(problem: RpgMakerDialogueProjectionProblem) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::ExtractBuiltin,
            problem: RpgMakerProblem::DialogueProjection { problem },
        }
    }

    pub(crate) const fn document(
        consumer: RpgMakerDocumentConsumer,
        operation: RpgMakerDocumentOperation,
        problem: RpgMakerDocumentProblem,
    ) -> Self {
        Self {
            stage: match consumer {
                RpgMakerDocumentConsumer::Builtin | RpgMakerDocumentConsumer::Rules => {
                    RpgMakerDiagnosticStage::ExtractDocument
                }
                RpgMakerDocumentConsumer::WriteBack => RpgMakerDiagnosticStage::WriteBackDocument,
            },
            problem: RpgMakerProblem::Document {
                consumer,
                operation,
                problem,
            },
        }
    }

    pub(crate) fn extraction(problem: RpgMakerExtractionProblem) -> Self {
        Self {
            stage: problem.stage(),
            problem: RpgMakerProblem::Extraction { problem },
        }
    }

    pub(crate) const fn placeholder_planning(
        rule_source: PlaceholderRuleSource,
        unit: RpgMakerUnitLocator,
        problem: PlaceholderIssue,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslatePlanning,
            problem: RpgMakerProblem::PlaceholderPlanning {
                rule_source,
                unit,
                problem,
            },
        }
    }

    pub(crate) const fn placeholder_projection(
        rule_source: PlaceholderRuleSource,
        unit: RpgMakerUnitLocator,
        problem: RpgMakerPlaceholderProjectionProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslatePlanning,
            problem: RpgMakerProblem::PlaceholderProjection {
                rule_source,
                unit,
                problem,
            },
        }
    }

    pub(crate) const fn write_back_placeholder_planning(
        rule_source: PlaceholderRuleSource,
        unit: RpgMakerUnitLocator,
        problem: PlaceholderIssue,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::WriteBackDocument,
            problem: RpgMakerProblem::PlaceholderPlanning {
                rule_source,
                unit,
                problem,
            },
        }
    }

    pub(crate) const fn write_back_placeholder_projection(
        unit: RpgMakerUnitLocator,
        problem: RpgMakerPlaceholderProjectionProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::WriteBackDocument,
            problem: RpgMakerProblem::PlaceholderProjection {
                rule_source: PlaceholderRuleSource::ProjectSnapshot,
                unit,
                problem,
            },
        }
    }

    pub(crate) const fn response_processing(
        scope: RpgMakerResponseProcessingScope,
        problem: RpgMakerResponseProcessingProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslateExecution,
            problem: RpgMakerProblem::ResponseProcessing { scope, problem },
        }
    }

    pub(crate) const fn task_response(
        scope: RpgMakerResponseProcessingScope,
        problem: RpgMakerTaskResponseProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslateExecution,
            problem: RpgMakerProblem::TaskResponse { scope, problem },
        }
    }

    pub(crate) const fn result_store(problem: RpgMakerResultStoreProblem) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslateCommit,
            problem: RpgMakerProblem::ResultStore { problem },
        }
    }

    pub(crate) fn translation_asset(
        database_path: impl AsRef<std::path::Path>,
        problem: RpgMakerTranslationAssetProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslatePlanning,
            problem: RpgMakerProblem::TranslationAsset {
                database_path: SafePath::new(database_path),
                problem,
            },
        }
    }

    pub(crate) fn write_back_asset(
        database_path: impl AsRef<std::path::Path>,
        problem: RpgMakerWriteBackAssetProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::WriteBackDocument,
            problem: RpgMakerProblem::WriteBackAsset {
                database_path: SafePath::new(database_path),
                problem,
            },
        }
    }

    pub(crate) const fn write_back_planning(problem: RpgMakerWriteBackPlanningProblem) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::WriteBackDocument,
            problem: RpgMakerProblem::WriteBackPlanning { problem },
        }
    }

    pub(crate) const fn write_back_document_rewrite(
        problem: RpgMakerWriteBackDocumentRewriteProblem,
    ) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::WriteBackDocument,
            problem: RpgMakerProblem::WriteBackDocumentRewrite { problem },
        }
    }

    pub(crate) const fn translation_planning(problem: RpgMakerTranslationPlanningProblem) -> Self {
        Self {
            stage: RpgMakerDiagnosticStage::TranslatePlanning,
            problem: RpgMakerProblem::TranslationPlanning { problem },
        }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        self.stage.diagnostic_stage()
    }

    pub(crate) fn code(&self) -> &'static str {
        match &self.problem {
            RpgMakerProblem::Project { problem } => problem.code(),
            RpgMakerProblem::RulesDefinition { problem, .. } => problem.code(),
            RpgMakerProblem::RulesMatch { problem, .. } => problem.code(),
            RpgMakerProblem::RulesCommandNonString { .. } => {
                "rpg_maker.extract.rules.command_non_string_skipped"
            }
            RpgMakerProblem::RulesOwnerDisabled { .. } => "rpg_maker.extract.rules.owner_disabled",
            RpgMakerProblem::BuiltinDocument { problem, .. } => problem.code(),
            RpgMakerProblem::DialogueDefinition { problem, .. } => problem.code(),
            RpgMakerProblem::DialogueProjection { problem } => problem.code(),
            RpgMakerProblem::Document { problem, .. } => problem.code(),
            RpgMakerProblem::Extraction { problem } => problem.code(),
            RpgMakerProblem::PlaceholderPlanning { problem, .. } => problem.code(),
            RpgMakerProblem::PlaceholderProjection { problem, .. } => match problem {
                RpgMakerPlaceholderProjectionProblem::TokenIndexConstruction => {
                    "translation.placeholder.token_index_construction"
                }
                RpgMakerPlaceholderProjectionProblem::EmptyToken => {
                    "translation.placeholder.empty_token"
                }
                RpgMakerPlaceholderProjectionProblem::MissingToken { .. } => {
                    "translation.placeholder.missing_token"
                }
                RpgMakerPlaceholderProjectionProblem::RepeatedToken { .. } => {
                    "translation.placeholder.repeated_token"
                }
                RpgMakerPlaceholderProjectionProblem::OverlappingToken { .. } => {
                    "translation.placeholder.overlapping_token"
                }
                RpgMakerPlaceholderProjectionProblem::ChangedTokenOrder { .. } => {
                    "translation.placeholder.changed_token_order"
                }
                RpgMakerPlaceholderProjectionProblem::ChangedSegmentCount { .. } => {
                    "translation.placeholder.changed_segment_count"
                }
                RpgMakerPlaceholderProjectionProblem::ChangedSegmentKind { .. } => {
                    "translation.placeholder.changed_segment_kind"
                }
                RpgMakerPlaceholderProjectionProblem::MissingOrderedToken { .. } => {
                    "translation.placeholder.missing_ordered_token"
                }
                RpgMakerPlaceholderProjectionProblem::UnusedOrderedToken => {
                    "translation.placeholder.unused_ordered_token"
                }
                RpgMakerPlaceholderProjectionProblem::SourceBindingMismatch => {
                    "translation.placeholder.source_binding_mismatch"
                }
            },
            RpgMakerProblem::ResponseProcessing { problem, .. } => problem.code(),
            RpgMakerProblem::TaskResponse { problem, .. } => problem.code(),
            RpgMakerProblem::ResultStore { problem } => problem.code(),
            RpgMakerProblem::TranslationAsset { problem, .. } => problem.code(),
            RpgMakerProblem::WriteBackAsset { problem, .. } => problem.code(),
            RpgMakerProblem::WriteBackPlanning { problem } => problem.code(),
            RpgMakerProblem::WriteBackDocumentRewrite { problem } => problem.code(),
            RpgMakerProblem::TranslationPlanning { problem } => problem.code(),
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match &self.problem {
            RpgMakerProblem::Project { problem } => problem.resolution(),
            RpgMakerProblem::RulesDefinition {
                origin, problem, ..
            } => match problem {
                RpgMakerRulesDefinitionProblem::EncodeCanonicalJson { .. } => {
                    DiagnosticResolution::ReportBug
                }
                _ => match origin {
                    RpgMakerRulesDefinitionOrigin::ExternalToml => DiagnosticResolution::FixInput,
                    RpgMakerRulesDefinitionOrigin::ProjectSnapshot => {
                        DiagnosticResolution::CheckProjectState
                    }
                },
            },
            RpgMakerProblem::RulesMatch { problem, .. } => match problem {
                RpgMakerRulesMatchProblem::InvalidMaterialization { .. } => {
                    DiagnosticResolution::ReportBug
                }
                _ => DiagnosticResolution::FixInput,
            },
            RpgMakerProblem::RulesCommandNonString { .. } => DiagnosticResolution::FixInput,
            RpgMakerProblem::RulesOwnerDisabled { .. } => DiagnosticResolution::ReviewDisabledRules,
            RpgMakerProblem::BuiltinDocument { .. } => DiagnosticResolution::FixInput,
            RpgMakerProblem::DialogueDefinition {
                origin, problem, ..
            } => match problem {
                RpgMakerDialogueDefinitionProblem::EncodeCanonicalJson { .. } => {
                    DiagnosticResolution::ReportBug
                }
                _ => match origin {
                    RpgMakerDialogueDefinitionOrigin::ExternalToml => {
                        DiagnosticResolution::FixInput
                    }
                    RpgMakerDialogueDefinitionOrigin::ProjectSnapshot => {
                        DiagnosticResolution::CheckProjectState
                    }
                },
            },
            RpgMakerProblem::DialogueProjection { problem } => match problem {
                RpgMakerDialogueProjectionProblem::InvalidRecipe { .. } => {
                    DiagnosticResolution::ReportBug
                }
                _ => DiagnosticResolution::FixInput,
            },
            RpgMakerProblem::Document { problem, .. } => match problem {
                RpgMakerDocumentProblem::Backend { .. } => DiagnosticResolution::Retry,
                _ => DiagnosticResolution::FixInput,
            },
            RpgMakerProblem::Extraction { problem } => problem.resolution(),
            RpgMakerProblem::PlaceholderPlanning { .. } => {
                DiagnosticResolution::FixPlaceholderRules
            }
            RpgMakerProblem::PlaceholderProjection { .. } => {
                if matches!(self.stage, RpgMakerDiagnosticStage::WriteBackDocument) {
                    DiagnosticResolution::FixInput
                } else {
                    DiagnosticResolution::ReportBug
                }
            }
            RpgMakerProblem::ResponseProcessing { problem, .. } => match problem {
                RpgMakerResponseProcessingProblem::Cancelled
                | RpgMakerResponseProcessingProblem::Compute { .. } => DiagnosticResolution::Retry,
                RpgMakerResponseProcessingProblem::PlaceholderProtection { .. } => {
                    DiagnosticResolution::FixPlaceholderRules
                }
                RpgMakerResponseProcessingProblem::LanguageModuleMismatch { .. }
                | RpgMakerResponseProcessingProblem::LanguageProjection { .. }
                | RpgMakerResponseProcessingProblem::InternalInvariant { .. } => {
                    DiagnosticResolution::ReportBug
                }
            },
            RpgMakerProblem::TaskResponse { .. } => DiagnosticResolution::Retry,
            RpgMakerProblem::ResultStore { problem } => problem.resolution(),
            RpgMakerProblem::TranslationAsset { problem, .. } => problem.resolution(),
            RpgMakerProblem::WriteBackAsset { problem, .. } => problem.resolution(),
            RpgMakerProblem::WriteBackPlanning { problem } => problem.resolution(),
            RpgMakerProblem::WriteBackDocumentRewrite { problem } => problem.resolution(),
            RpgMakerProblem::TranslationPlanning { problem } => problem.resolution(),
        }
    }

    pub(crate) fn summary_code(&self) -> &'static str {
        match &self.problem {
            RpgMakerProblem::Project { problem } => problem.summary_code(),
            RpgMakerProblem::RulesDefinition { problem, .. } => problem.summary_code(),
            RpgMakerProblem::RulesMatch { problem, .. } => problem.summary_code(),
            RpgMakerProblem::RulesCommandNonString { .. } => "invalid_value",
            RpgMakerProblem::RulesOwnerDisabled { .. } => "rules_owner_disabled",
            RpgMakerProblem::BuiltinDocument { problem, .. } => problem.summary_code(),
            RpgMakerProblem::DialogueDefinition { problem, .. } => problem.summary_code(),
            RpgMakerProblem::DialogueProjection { problem } => problem.summary_code(),
            RpgMakerProblem::Document { problem, .. } => problem.summary_code(),
            RpgMakerProblem::Extraction { problem } => problem.summary_code(),
            RpgMakerProblem::PlaceholderPlanning { problem, .. } => problem.summary_code(),
            RpgMakerProblem::PlaceholderProjection { .. } => "placeholder_projection_failed",
            RpgMakerProblem::ResponseProcessing { problem, .. } => problem.summary_code(),
            RpgMakerProblem::TaskResponse { problem, .. } => problem.summary_code(),
            RpgMakerProblem::ResultStore { problem } => problem.summary_code(),
            RpgMakerProblem::TranslationAsset { problem, .. } => problem.summary_code(),
            RpgMakerProblem::WriteBackAsset { problem, .. } => problem.summary_code(),
            RpgMakerProblem::WriteBackPlanning { problem } => problem.summary_code(),
            RpgMakerProblem::WriteBackDocumentRewrite { problem } => problem.summary_code(),
            RpgMakerProblem::TranslationPlanning { problem } => problem.summary_code(),
        }
    }

    pub(crate) fn subject(&self) -> String {
        match &self.problem {
            RpgMakerProblem::Project { problem } => problem.subject(),
            RpgMakerProblem::RulesDefinition { rules_path, .. } => rules_path.to_string(),
            RpgMakerProblem::RulesMatch {
                rules_path,
                context,
                problem,
            } => rules_match_subject(rules_path, context.as_ref(), problem),
            RpgMakerProblem::RulesCommandNonString { fact } => format!(
                "{} (Rules rule {}; command code={}; parameter={}; type={}; skipped={})",
                fact.source_file,
                fact.rule_number,
                fact.command_code,
                fact.parameter,
                fact.actual_type.as_str(),
                fact.skipped_count,
            ),
            RpgMakerProblem::RulesOwnerDisabled { rules_path } => rules_path.to_string(),
            RpgMakerProblem::BuiltinDocument { location, .. } => {
                format!("{}:{}", location.source_fact(), location.steps_fact())
            }
            RpgMakerProblem::DialogueDefinition { path, .. } => path
                .as_ref()
                .map_or_else(|| "mv_dialogue_definition".to_owned(), ToString::to_string),
            RpgMakerProblem::DialogueProjection { problem } => match problem {
                RpgMakerDialogueProjectionProblem::Match { location, .. }
                | RpgMakerDialogueProjectionProblem::ZeroWidthMatch { location, .. }
                | RpgMakerDialogueProjectionProblem::MissingSpeakerCapture { location, .. }
                | RpgMakerDialogueProjectionProblem::InvalidSpeakerCaptureRange {
                    location, ..
                }
                | RpgMakerDialogueProjectionProblem::MultipleRulesOwnField { location, .. }
                | RpgMakerDialogueProjectionProblem::DifferentSpeakers { location } => {
                    format!("{}:{}", location.source_fact(), location.steps_fact())
                }
                RpgMakerDialogueProjectionProblem::RuleCapturedNoSpeaker { rule_number } => {
                    format!("mv_dialogue.rule[{rule_number}]")
                }
                RpgMakerDialogueProjectionProblem::InvalidRecipe { .. } => {
                    "mv_dialogue_recipe".to_owned()
                }
            },
            RpgMakerProblem::Document { problem, .. } => match problem {
                RpgMakerDocumentProblem::NotFound { path }
                | RpgMakerDocumentProblem::NotDirectory { path }
                | RpgMakerDocumentProblem::NotFile { path }
                | RpgMakerDocumentProblem::FileNameTooLarge { path, .. }
                | RpgMakerDocumentProblem::Backend { path, .. }
                | RpgMakerDocumentProblem::InvalidUtf8 { path, .. }
                | RpgMakerDocumentProblem::InvalidJson { path, .. }
                | RpgMakerDocumentProblem::InvalidPluginsEnvelope { path, .. }
                | RpgMakerDocumentProblem::InvalidPluginRecord { path, .. } => path.to_string(),
                RpgMakerDocumentProblem::FileNameCaseMismatch { requested, .. } => {
                    requested.to_string()
                }
            },
            RpgMakerProblem::Extraction { problem } => problem.subject(),
            RpgMakerProblem::PlaceholderPlanning {
                rule_source,
                unit,
                problem,
            } => unit.placeholder_subject(rule_source, placeholder_problem_rules(problem)),
            RpgMakerProblem::PlaceholderProjection {
                rule_source, unit, ..
            } => unit.placeholder_subject(rule_source, None),
            RpgMakerProblem::ResponseProcessing { scope, .. }
            | RpgMakerProblem::TaskResponse { scope, .. } => scope.subject(),
            RpgMakerProblem::ResultStore { problem } => problem.subject(),
            RpgMakerProblem::TranslationAsset { database_path, .. } => database_path.to_string(),
            RpgMakerProblem::WriteBackAsset {
                problem:
                    RpgMakerWriteBackAssetProblem::InvalidSnapshot {
                        violation:
                            RpgMakerWriteBackAssetSnapshotViolation::InvalidModel {
                                violation:
                                    RpgMakerWriteBackModelViolation::AlignedBlankLineMismatch {
                                        unit,
                                        ..
                                    },
                            },
                    },
                ..
            } => unit.natural_id(),
            RpgMakerProblem::WriteBackAsset { database_path, .. } => database_path.to_string(),
            RpgMakerProblem::WriteBackPlanning { .. } => "rpg_maker_write_back_plan".to_owned(),
            RpgMakerProblem::WriteBackDocumentRewrite { problem } => problem.subject(),
            RpgMakerProblem::TranslationPlanning { problem } => match problem {
                RpgMakerTranslationPlanningProblem::OutputContract { unit, .. } => {
                    format!("{}:{}", unit.owner.as_str(), unit.group_kind.as_str())
                }
                _ => "rpg_maker_translation_planning".to_owned(),
            },
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = match &self.problem {
            RpgMakerProblem::Project { problem } => problem.facts(),
            RpgMakerProblem::RulesDefinition {
                origin,
                rules_path,
                problem,
            } => vec![
                (
                    "origin",
                    match origin {
                        RpgMakerRulesDefinitionOrigin::ExternalToml => "external_toml",
                        RpgMakerRulesDefinitionOrigin::ProjectSnapshot => "project_snapshot",
                    }
                    .to_owned(),
                ),
                ("rules_path", rules_path.to_string()),
                ("rules_problem", problem.code().to_owned()),
            ],
            RpgMakerProblem::RulesMatch {
                rules_path,
                context,
                problem,
            } => vec![
                ("rules_path", rules_path.to_string()),
                ("rules_problem", problem.code().to_owned()),
                (
                    "rules_context",
                    if context.is_some() { "present" } else { "none" }.to_owned(),
                ),
            ],
            RpgMakerProblem::RulesCommandNonString { fact } => fact.facts(),
            RpgMakerProblem::RulesOwnerDisabled { rules_path } => {
                vec![("rules_path", rules_path.to_string())]
            }
            RpgMakerProblem::BuiltinDocument { location, problem } => vec![
                ("source", location.source_fact()),
                ("location_steps", location.steps_fact()),
                ("builtin_problem", problem.code().to_owned()),
            ],
            RpgMakerProblem::DialogueDefinition {
                origin,
                path,
                problem,
            } => vec![
                (
                    "origin",
                    match origin {
                        RpgMakerDialogueDefinitionOrigin::ExternalToml => "external_toml",
                        RpgMakerDialogueDefinitionOrigin::ProjectSnapshot => "project_snapshot",
                    }
                    .to_owned(),
                ),
                (
                    "path",
                    path.as_ref()
                        .map_or_else(|| "project_snapshot".to_owned(), ToString::to_string),
                ),
                ("dialogue_problem", problem.code().to_owned()),
            ],
            RpgMakerProblem::DialogueProjection { problem } => {
                vec![("dialogue_problem", problem.code().to_owned())]
            }
            RpgMakerProblem::Document {
                consumer,
                operation,
                problem,
            } => vec![
                ("consumer", consumer.as_str().to_owned()),
                ("operation", operation.as_str().to_owned()),
                ("document_problem", problem.code().to_owned()),
            ],
            RpgMakerProblem::Extraction { problem } => problem.facts(),
            RpgMakerProblem::PlaceholderPlanning {
                rule_source,
                problem,
                ..
            } => {
                let mut facts = vec![(
                    "rule_source",
                    match rule_source {
                        PlaceholderRuleSource::ExternalFile { path } => path.to_string(),
                        PlaceholderRuleSource::ProjectSnapshot => "project_snapshot".to_owned(),
                    },
                )];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::PlaceholderProjection {
                rule_source,
                problem,
                ..
            } => {
                let mut facts = vec![(
                    "rule_source",
                    match rule_source {
                        PlaceholderRuleSource::ExternalFile { path } => path.to_string(),
                        PlaceholderRuleSource::ProjectSnapshot => "project_snapshot".to_owned(),
                    },
                )];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::ResponseProcessing { scope, problem } => {
                let mut facts = vec![("task_index", scope.task_index().to_string())];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::TaskResponse { scope, problem } => {
                let mut facts = vec![("task_index", scope.task_index().to_string())];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::ResultStore { problem } => problem.facts(),
            RpgMakerProblem::TranslationAsset {
                database_path,
                problem,
            } => {
                let mut facts = vec![("database_path", database_path.to_string())];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::WriteBackAsset {
                database_path,
                problem,
            } => {
                let mut facts = vec![("database_path", database_path.to_string())];
                facts.extend(problem.facts());
                facts
            }
            RpgMakerProblem::WriteBackPlanning { problem } => problem.facts(),
            RpgMakerProblem::WriteBackDocumentRewrite { problem } => problem.facts(),
            RpgMakerProblem::TranslationPlanning { problem } => problem.facts(),
        };
        if let RpgMakerProblem::DialogueDefinition { problem, .. } = &self.problem {
            facts.extend(problem.facts());
        }
        if let Some(unit) = self.unit() {
            facts.extend(unit.facts());
        }
        facts
    }

    fn unit(&self) -> Option<&RpgMakerUnitLocator> {
        match &self.problem {
            RpgMakerProblem::Project { .. }
            | RpgMakerProblem::RulesDefinition { .. }
            | RpgMakerProblem::RulesMatch { .. }
            | RpgMakerProblem::RulesCommandNonString { .. }
            | RpgMakerProblem::RulesOwnerDisabled { .. }
            | RpgMakerProblem::BuiltinDocument { .. }
            | RpgMakerProblem::DialogueDefinition { .. }
            | RpgMakerProblem::DialogueProjection { .. }
            | RpgMakerProblem::Document { .. }
            | RpgMakerProblem::Extraction { .. } => None,
            RpgMakerProblem::ResultStore { .. } => None,
            RpgMakerProblem::TranslationAsset { .. } => None,
            RpgMakerProblem::WriteBackAsset { .. } => None,
            RpgMakerProblem::WriteBackPlanning { .. } => None,
            RpgMakerProblem::WriteBackDocumentRewrite { .. } => None,
            RpgMakerProblem::TranslationPlanning { problem } => match problem {
                RpgMakerTranslationPlanningProblem::OutputContract { unit, .. } => Some(unit),
                _ => None,
            },
            RpgMakerProblem::PlaceholderPlanning { unit, .. }
            | RpgMakerProblem::PlaceholderProjection { unit, .. } => Some(unit),
            RpgMakerProblem::ResponseProcessing { scope, .. }
            | RpgMakerProblem::TaskResponse { scope, .. } => scope.unit_locator(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{
        ByteRange, Diagnostic, DiagnosticReport, PlaceholderIssue, StateEffect,
        render_diagnostic_fields,
    };
    use crate::i18n::{UiLocale, UiLocalizer};

    #[test]
    fn missing_text_capture_wire_keeps_complete_rpg_maker_locator() {
        let unit = RpgMakerUnitLocator::new(
            RpgMakerDiagnosticOwner::Builtin,
            RpgMakerDiagnosticGroupKind::DatabaseEntry,
            RpgMakerDiagnosticLocation::new(
                RpgMakerDiagnosticSource::data("Actors.json"),
                vec![
                    RpgMakerDiagnosticLocationStep::array_index(3),
                    RpgMakerDiagnosticLocationStep::object_key("name"),
                ],
            ),
            RpgMakerDiagnosticRole::scalar("name"),
        );
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::placeholder_planning(
                PlaceholderRuleSource::ProjectSnapshot,
                unit,
                PlaceholderIssue::MissingTextCapture {
                    rule_number: 7,
                    match_range: ByteRange::new(4, 12).expect("有效范围"),
                },
            )),
        );

        let value = serde_json::to_value(report).expect("诊断必须可序列化");
        assert_eq!(
            value["primary"]["code"],
            "translation.placeholder.missing_text_capture"
        );
        assert_eq!(value["primary"]["stage"], "translate");
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["unit"]["group_location"]["steps"][1]["key"],
            "name"
        );
        assert_eq!(value["primary"]["resolution"], "fix_placeholder_rules");

        let issue = RpgMakerIssue::placeholder_planning(
            PlaceholderRuleSource::ProjectSnapshot,
            RpgMakerUnitLocator::new(
                RpgMakerDiagnosticOwner::Builtin,
                RpgMakerDiagnosticGroupKind::DatabaseEntry,
                RpgMakerDiagnosticLocation::new(
                    RpgMakerDiagnosticSource::data("Actors.json"),
                    vec![RpgMakerDiagnosticLocationStep::object_key("name")],
                ),
                RpgMakerDiagnosticRole::scalar("name"),
            ),
            PlaceholderIssue::MissingTextCapture {
                rule_number: 7,
                match_range: ByteRange::new(4, 12).expect("有效范围"),
            },
        );
        let facts = issue.facts();
        assert!(facts.contains(&("source", "data:Actors.json".to_owned())));
        assert!(facts.contains(&("location_steps", "key:name".to_owned())));
        assert!(facts.contains(&("role", "scalar:name".to_owned())));
        let subject = issue.subject();
        for expected in [
            "Actors.json:name",
            "role=scalar:name",
            "Placeholder=project snapshot",
            "custom rule 7",
        ] {
            assert!(
                subject.contains(expected),
                "object 缺少 {expected:?}：{subject}"
            );
        }
        assert!(
            !subject.contains("4..12"),
            "object 不得显示字节范围：{subject}"
        );
    }

    #[test]
    fn overlapping_placeholder_subject_names_unit_source_file_and_both_natural_rules() {
        let issue = RpgMakerIssue::placeholder_planning(
            PlaceholderRuleSource::external_file("C:/input/placeholders.toml"),
            RpgMakerUnitLocator::new(
                RpgMakerDiagnosticOwner::Builtin,
                RpgMakerDiagnosticGroupKind::DatabaseEntry,
                RpgMakerDiagnosticLocation::new(
                    RpgMakerDiagnosticSource::data("Items.json"),
                    vec![
                        RpgMakerDiagnosticLocationStep::array_index(2),
                        RpgMakerDiagnosticLocationStep::object_key("description"),
                    ],
                ),
                RpgMakerDiagnosticRole::scalar("description"),
            ),
            PlaceholderIssue::OverlappingMatches {
                first_origin: PlaceholderRuleOrigin::Builtin,
                first_rule_number: None,
                first_range: ByteRange::new(7, 14).expect("有效范围"),
                second_origin: PlaceholderRuleOrigin::Custom,
                second_rule_number: Some(1),
                second_range: ByteRange::new(7, 14).expect("有效范围"),
            },
        );

        let subject = issue.subject();
        for expected in [
            "Items.json[2]:description",
            "role=scalar:description",
            "C:/input/placeholders.toml",
            "builtin",
            "custom rule 1",
        ] {
            assert!(
                subject.contains(expected),
                "object 缺少 {expected:?}：{subject}"
            );
        }
        for forbidden in ["7..14", "first_range", "second_range"] {
            assert!(
                !subject.contains(forbidden),
                "object 不得显示内部范围 {forbidden:?}：{subject}"
            );
        }
    }

    #[test]
    fn planning_projection_is_an_internal_projection_failure() {
        let issue = RpgMakerIssue::placeholder_projection(
            PlaceholderRuleSource::ProjectSnapshot,
            RpgMakerUnitLocator::new(
                RpgMakerDiagnosticOwner::Rules,
                RpgMakerDiagnosticGroupKind::EventDialogue,
                RpgMakerDiagnosticLocation::new(
                    RpgMakerDiagnosticSource::map(2),
                    vec![RpgMakerDiagnosticLocationStep::array_index(7)],
                ),
                RpgMakerDiagnosticRole::DialogueBody,
            ),
            RpgMakerPlaceholderProjectionProblem::ChangedSegmentCount {
                expected: 2,
                actual: 1,
            },
        );

        assert_eq!(issue.resolution(), DiagnosticResolution::ReportBug);
        assert!(issue.facts().contains(&("expected", "2".to_owned())));
        assert!(issue.facts().contains(&("actual", "1".to_owned())));
        let subject = issue.subject();
        for expected in [
            "Map002.json[7]",
            "role=dialogue_body",
            "Placeholder=project snapshot",
        ] {
            assert!(
                subject.contains(expected),
                "object 缺少 {expected:?}：{subject}"
            );
        }
    }

    #[test]
    fn task_response_blank_line_mismatch_keeps_unit_locator_and_leaf_code() {
        let unit = RpgMakerUnitLocator::new(
            RpgMakerDiagnosticOwner::Builtin,
            RpgMakerDiagnosticGroupKind::EventChoices,
            RpgMakerDiagnosticLocation::new(
                RpgMakerDiagnosticSource::data("CommonEvents.json"),
                vec![
                    RpgMakerDiagnosticLocationStep::array_index(66),
                    RpgMakerDiagnosticLocationStep::object_key("list"),
                    RpgMakerDiagnosticLocationStep::array_index(3),
                ],
            ),
            RpgMakerDiagnosticRole::Choices,
        );
        let report = DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::rpg_maker(RpgMakerIssue::task_response(
                RpgMakerResponseProcessingScope::unit(41, unit),
                RpgMakerTaskResponseProblem::UnitRejected {
                    output_id: 1,
                    problem: RpgMakerTaskResponseUnitProblem::BlankLineMismatch {
                        line_index: 2,
                        expected_blank: true,
                    },
                },
            )),
        );

        let value = serde_json::to_value(report).expect("诊断必须可序列化");
        assert_eq!(
            value["primary"]["code"],
            "rpg_maker.translation.response.unit.blank_line_mismatch"
        );
        assert_eq!(value["primary"]["stage"], "translate");
        assert_eq!(value["primary"]["resolution"], "retry");
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["scope"]["unit"]["group_location"]["source"]
                ["file"],
            "CommonEvents.json"
        );
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["problem"]["problem"]["expected_blank"],
            true
        );
    }

    #[test]
    fn scalar_role_drops_untrusted_field_without_panicking_or_leaking() {
        let sentinel = "api-key-sentinel\n\0";
        let role = RpgMakerDiagnosticRole::scalar(sentinel);

        assert_eq!(role.fact_value(), "scalar");
        let wire = serde_json::to_string(&role).expect("角色必须可序列化");
        assert!(!wire.contains("api-key-sentinel"));
        assert_eq!(wire, r#"{"kind":"scalar"}"#);
    }

    #[test]
    fn unknown_snapshot_values_are_omitted_and_leaf_code_remains_specific() {
        let sentinel = "Authorization: Bearer api-key-sentinel";
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::translation_asset(
                "project.sqlite3",
                RpgMakerTranslationAssetProblem::InvalidSnapshot {
                    violation: RpgMakerTranslationSnapshotViolation::UnknownTranslationResource,
                },
            )),
        );

        let wire = serde_json::to_string(&report).expect("诊断必须可序列化");
        assert!(!wire.contains(sentinel));
        assert!(!wire.contains("api-key-sentinel"));
        let value: serde_json::Value = serde_json::from_str(&wire).expect("诊断必须是 JSON");
        assert_eq!(
            value["primary"]["code"],
            "rpg_maker.translate.asset_snapshot.unknown_translation_resource"
        );
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["problem"]["violation"]["kind"],
            "unknown_translation_resource"
        );
    }

    #[test]
    fn write_back_mutation_failure_uses_a_closed_violation_and_exact_location() {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::write_back_document_rewrite(
                RpgMakerWriteBackDocumentRewriteProblem::InvalidMutation {
                    location: RpgMakerDiagnosticLocation::new(
                        RpgMakerDiagnosticSource::map(12),
                        vec![
                            RpgMakerDiagnosticLocationStep::array_index(3),
                            RpgMakerDiagnosticLocationStep::object_key("list"),
                        ],
                    ),
                    violation: RpgMakerWriteBackMutationViolation::EventListNotArray,
                },
            )),
        );

        let value = serde_json::to_value(report).expect("写回诊断必须可序列化");
        assert_eq!(
            value["primary"]["code"],
            "rpg_maker.write_back.rewrite.invalid_mutation"
        );
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["problem"]["violation"],
            "event_list_not_array"
        );
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["problem"]["location"]["source"]["map_id"],
            12
        );
    }

    #[test]
    fn write_back_aligned_blank_line_mismatch_renders_logical_unit_as_object() {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::write_back_asset(
                "project.db",
                RpgMakerWriteBackAssetProblem::InvalidSnapshot {
                    violation: RpgMakerWriteBackAssetSnapshotViolation::InvalidModel {
                        violation: RpgMakerWriteBackModelViolation::AlignedBlankLineMismatch {
                            unit: RpgMakerLogicalUnitLocator::new(
                                RpgMakerDiagnosticLocation::new(
                                    RpgMakerDiagnosticSource::data("CommonEvents.json"),
                                    vec![
                                        RpgMakerDiagnosticLocationStep::array_index(66),
                                        RpgMakerDiagnosticLocationStep::object_key("list"),
                                        RpgMakerDiagnosticLocationStep::array_index(3),
                                    ],
                                ),
                                RpgMakerDiagnosticRole::Choices,
                            ),
                            line_index: 0,
                        },
                    },
                },
            )),
        );

        let rendered =
            render_diagnostic_fields(&report, &UiLocalizer::new(UiLocale::SimplifiedChinese));

        assert_eq!(
            rendered.object,
            "data/CommonEvents.json[66]:list[3]:choices"
        );
        assert_ne!(rendered.object, "project.db");
    }

    #[test]
    fn provider_specific_finish_reason_is_sanitized_and_non_stop_is_closed() {
        let reason = RpgMakerModelFinishReason::provider_specific("custom\r\n\u{202e}finish");

        assert_eq!(
            serde_json::to_value(&reason).expect("结束原因必须可序列化"),
            serde_json::json!({
                "kind": "provider_specific",
                "value": "custom finish"
            })
        );
        assert_eq!(
            serde_json::to_value(reason.non_stop().expect("供应商扩展值不是 stop"))
                .expect("非 stop 结束原因必须可序列化"),
            serde_json::json!({
                "kind": "provider_specific",
                "value": "custom finish"
            })
        );
        assert!(RpgMakerModelFinishReason::Stop.non_stop().is_none());
    }
}
