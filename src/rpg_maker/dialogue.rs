//! MV 第一条 `401` 的局部姓名投影定义与物化算法。

use std::error::Error;
use std::fmt;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::translation::candidate_validation::is_structural_blank;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, Pcre2Failure, Pcre2FailureKind, RpgMakerDiagnosticLocation,
    RpgMakerDiagnosticLocationStep, RpgMakerDiagnosticSource, RpgMakerDialogueDefinitionOrigin,
    RpgMakerDialogueDefinitionProblem, RpgMakerDialogueProjectionProblem, RpgMakerIssue,
    RpgMakerJsonFailureKind, RpgMakerProjectionFailureKind, RpgMakerTomlFailureKind,
    SafeIdentifier, StateEffect,
};
use crate::json_diagnostic::JsonErrorCategory;

use super::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, ProjectionModelError,
    TextUnitContent, TextUnitRole,
};
use super::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MvDialogueDefinition {
    rules: Vec<MvDialogueRule>,
}

impl MvDialogueDefinition {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub(crate) fn parse_toml(source: &str) -> Result<Self, MvDialogueDefinitionError> {
        if is_structural_blank(source) {
            return Err(MvDialogueDefinitionError::EmptyDocument);
        }
        if let Err(error) = toml::from_str::<toml::Value>(source) {
            return Err(MvDialogueDefinitionError::InvalidToml(Box::new(
                MvDialogueTomlError::new(source, MvDialogueTomlClassification::Syntax, error),
            )));
        }
        let external = toml::from_str::<ExternalDefinition>(source).map_err(|error| {
            MvDialogueDefinitionError::InvalidToml(Box::new(MvDialogueTomlError::new(
                source,
                MvDialogueTomlClassification::Data,
                error,
            )))
        })?;
        let rules = external
            .rule
            .ok_or(MvDialogueDefinitionError::MissingRuleArray)?;
        let definition = Self { rules };
        definition.validate()?;
        Ok(definition)
    }

    pub(crate) fn from_canonical_json(source: &str) -> Result<Self, MvDialogueDefinitionError> {
        let definition = serde_json::from_str::<Self>(source)
            .map_err(MvDialogueDefinitionError::InvalidCanonicalJson)?;
        definition.validate()?;
        Ok(definition)
    }

    pub(crate) fn to_canonical_json(&self) -> Result<String, MvDialogueDefinitionError> {
        serde_json::to_string(self).map_err(MvDialogueDefinitionError::EncodeCanonicalJson)
    }

    pub(crate) fn compile(&self) -> Result<MvDialogueProjector, MvDialogueDefinitionError> {
        let mut rules = Vec::with_capacity(self.rules.len());
        for (index, rule) in self.rules.iter().enumerate() {
            let rule_number = index + 1;
            let regex = RegexBuilder::new()
                .utf(true)
                .ucp(true)
                .jit_if_available(true)
                .build(&rule.pattern)
                .map_err(|source| MvDialogueDefinitionError::InvalidPattern {
                    rule_number,
                    source,
                })?;
            let captures = regex
                .capture_names()
                .iter()
                .filter_map(Option::as_deref)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if captures.as_slice() != ["speaker"] {
                return Err(MvDialogueDefinitionError::InvalidNamedCaptures {
                    rule_number,
                    captures,
                });
            }
            rules.push(CompiledMvDialogueRule {
                rule_number,
                regex,
                non_blank_speaker_matches: 0,
            });
        }
        Ok(MvDialogueProjector { rules })
    }

    #[cfg(test)]
    pub(crate) fn rules(&self) -> &[MvDialogueRule] {
        &self.rules
    }

    fn validate(&self) -> Result<(), MvDialogueDefinitionError> {
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.pattern.is_empty() {
                return Err(MvDialogueDefinitionError::EmptyPattern {
                    rule_number: index + 1,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MvDialogueRule {
    pattern: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalDefinition {
    rule: Option<Vec<MvDialogueRule>>,
}

pub(crate) struct MvDialogueProjector {
    rules: Vec<CompiledMvDialogueRule>,
}

impl MvDialogueProjector {
    /// 为一个独立扫描分片建立零计数投影器，同时复用已经编译的 PCRE2。
    pub(crate) fn fork_for_scan(&self) -> Self {
        Self {
            rules: self
                .rules
                .iter()
                .map(|rule| CompiledMvDialogueRule {
                    rule_number: rule.rule_number,
                    regex: rule.regex.clone(),
                    non_blank_speaker_matches: 0,
                })
                .collect(),
        }
    }

    /// 合并同一定义在独立扫描分片中得到的非空 Speaker 命中数。
    pub(crate) fn merge_scan(&mut self, other: Self) {
        assert_eq!(
            self.rules.len(),
            other.rules.len(),
            "只能合并由同一个 MV 对话定义派生的扫描投影器"
        );
        for (current, scanned) in self.rules.iter_mut().zip(other.rules) {
            assert_eq!(
                current.rule_number, scanned.rule_number,
                "只能合并由同一个 MV 对话定义派生的扫描投影器"
            );
            current.non_blank_speaker_matches += scanned.non_blank_speaker_matches;
        }
    }

    pub(crate) fn project(
        &mut self,
        group_location: RpgMakerLocation,
        lines: Vec<DialoguePhysicalLine>,
    ) -> Result<ProjectedDialogue, MvDialogueProjectionError> {
        let mut units = Vec::new();
        let mut pending_lines = Vec::with_capacity(lines.len());
        let mut lines = lines.into_iter();

        if let Some(first_line) = lines.next() {
            let projection = self.project_first_line(&first_line)?;
            if let Some(value) = projection.speaker {
                units.push(ProjectedDialogueUnit::new(
                    TextUnitRole::DialogueSpeaker,
                    first_line.physical_location.clone(),
                    TextUnitContent::Value(value),
                ));
            }

            let mut prefix_parts = projection.prefix_parts;
            let body = if is_structural_blank(&projection.body) && !prefix_parts.is_empty() {
                push_literal(&mut prefix_parts, &projection.body);
                None
            } else {
                Some(projection.body)
            };
            pending_lines.push(PendingDialogueLine {
                physical_location: first_line.physical_location,
                expected_raw: first_line.expected_raw,
                prefix_parts,
                body,
            });
        }
        pending_lines.extend(lines.map(|line| PendingDialogueLine {
            physical_location: line.physical_location,
            expected_raw: line.expected_raw.clone(),
            prefix_parts: Vec::new(),
            body: Some(line.expected_raw),
        }));

        let has_body = pending_lines.iter().any(|line| {
            line.body
                .as_deref()
                .is_some_and(|body| !is_structural_blank(body))
        });
        let mut body_lines = Vec::new();
        let mut body_projection_location = None;
        let mut line_recipes = Vec::with_capacity(pending_lines.len());
        for pending in pending_lines {
            let mut parts = pending.prefix_parts;
            if let Some(body) = pending.body {
                if has_body {
                    body_projection_location
                        .get_or_insert_with(|| pending.physical_location.clone());
                    let source_line_index = body_lines.len();
                    body_lines.push(body);
                    parts.push(DialogueLinePart::BodyLine { source_line_index });
                } else {
                    push_literal(&mut parts, &body);
                }
            }
            if parts.is_empty() {
                parts.push(DialogueLinePart::Literal(pending.expected_raw.clone()));
            }
            line_recipes.push(
                DialogueLineRecipe::new(pending.physical_location, pending.expected_raw, parts)
                    .map_err(|source| MvDialogueProjectionError::InvalidRecipe(Box::new(source)))?,
            );
        }
        if has_body {
            units.push(ProjectedDialogueUnit::new(
                TextUnitRole::DialogueBody,
                body_projection_location.expect("非空正文必须有首个物理来源"),
                TextUnitContent::Lines(body_lines),
            ));
        }

        let recipe = DialogueWriteRecipe::new(group_location, None, line_recipes)
            .map_err(|source| MvDialogueProjectionError::InvalidRecipe(Box::new(source)))?;
        Ok(ProjectedDialogue { units, recipe })
    }

    pub(crate) fn finish(self) -> Result<(), MvDialogueProjectionError> {
        if let Some(rule) = self
            .rules
            .into_iter()
            .find(|rule| rule.non_blank_speaker_matches == 0)
        {
            return Err(MvDialogueProjectionError::RuleCapturedNoSpeaker {
                rule_number: rule.rule_number,
            });
        }
        Ok(())
    }

    fn project_first_line(
        &mut self,
        line: &DialoguePhysicalLine,
    ) -> Result<FirstLineProjection, MvDialogueProjectionError> {
        let mut owner = None;
        let mut matches = Vec::new();
        for rule in &mut self.rules {
            let rule_matches = collect_matches(rule, &line.expected_raw, &line.physical_location)?;
            if rule_matches.is_empty() {
                continue;
            }
            if let Some(previous) = owner {
                return Err(MvDialogueProjectionError::MultipleRulesOwnField {
                    location: Box::new(line.physical_location.clone()),
                    first_rule: previous,
                    second_rule: rule.rule_number,
                });
            }
            owner = Some(rule.rule_number);
            matches = rule_matches;
        }

        if matches.is_empty() {
            return Ok(FirstLineProjection {
                speaker: None,
                prefix_parts: Vec::new(),
                body: line.expected_raw.clone(),
            });
        }

        let non_blank_speakers = matches
            .iter()
            .map(|matched| matched.speaker.as_str())
            .filter(|speaker| !is_structural_blank(speaker))
            .collect::<Vec<_>>();
        let speaker = non_blank_speakers.first().map(|value| (*value).to_owned());
        if non_blank_speakers
            .iter()
            .any(|candidate| Some(*candidate) != speaker.as_deref())
        {
            return Err(MvDialogueProjectionError::DifferentSpeakers {
                location: Box::new(line.physical_location.clone()),
            });
        }

        let mut parts = Vec::new();
        let mut cursor = 0;
        for matched in &matches {
            push_literal(
                &mut parts,
                &line.expected_raw[cursor..matched.speaker_start],
            );
            if is_structural_blank(&matched.speaker) {
                push_literal(
                    &mut parts,
                    &line.expected_raw[matched.speaker_start..matched.speaker_end],
                );
            } else {
                parts.push(DialogueLinePart::SpeakerSlot);
            }
            push_literal(
                &mut parts,
                &line.expected_raw[matched.speaker_end..matched.whole_end],
            );
            cursor = matched.whole_end;
        }
        let body = line.expected_raw[cursor..].to_owned();
        Ok(FirstLineProjection {
            speaker,
            prefix_parts: parts,
            body,
        })
    }
}

fn collect_matches(
    rule: &mut CompiledMvDialogueRule,
    text: &str,
    location: &RpgMakerLocation,
) -> Result<Vec<SpeakerMatch>, MvDialogueProjectionError> {
    let mut matches = Vec::new();
    for captures in rule.regex.captures_iter(text.as_bytes()) {
        let captures = captures.map_err(|source| MvDialogueProjectionError::Match {
            rule_number: rule.rule_number,
            location: Box::new(location.clone()),
            source: Box::new(source),
        })?;
        let whole = captures
            .get(0)
            .expect("成功的 PCRE2 captures 必须包含完整匹配");
        if whole.start() == whole.end() {
            return Err(MvDialogueProjectionError::ZeroWidthMatch {
                rule_number: rule.rule_number,
                location: Box::new(location.clone()),
            });
        }
        let speaker =
            captures
                .name("speaker")
                .ok_or(MvDialogueProjectionError::MissingSpeakerCapture {
                    rule_number: rule.rule_number,
                    location: Box::new(location.clone()),
                })?;
        if whole.start() > speaker.start()
            || speaker.end() > whole.end()
            || !text.is_char_boundary(whole.start())
            || !text.is_char_boundary(whole.end())
            || !text.is_char_boundary(speaker.start())
            || !text.is_char_boundary(speaker.end())
        {
            return Err(MvDialogueProjectionError::InvalidSpeakerCaptureRange {
                rule_number: rule.rule_number,
                location: Box::new(location.clone()),
            });
        }
        let speaker_text = text[speaker.start()..speaker.end()].to_owned();
        if !is_structural_blank(&speaker_text) {
            rule.non_blank_speaker_matches += 1;
        }
        matches.push(SpeakerMatch {
            whole_end: whole.end(),
            speaker_start: speaker.start(),
            speaker_end: speaker.end(),
            speaker: speaker_text,
        });
    }
    Ok(matches)
}

fn push_literal(parts: &mut Vec<DialogueLinePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(DialogueLinePart::Literal(current)) => current.push_str(value),
        _ => parts.push(DialogueLinePart::Literal(value.to_owned())),
    }
}

struct CompiledMvDialogueRule {
    rule_number: usize,
    regex: Regex,
    non_blank_speaker_matches: usize,
}

struct SpeakerMatch {
    whole_end: usize,
    speaker_start: usize,
    speaker_end: usize,
    speaker: String,
}

struct FirstLineProjection {
    speaker: Option<String>,
    prefix_parts: Vec<DialogueLinePart>,
    body: String,
}

struct PendingDialogueLine {
    physical_location: RpgMakerLocation,
    expected_raw: String,
    prefix_parts: Vec<DialogueLinePart>,
    body: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DialoguePhysicalLine {
    physical_location: RpgMakerLocation,
    expected_raw: String,
}

impl DialoguePhysicalLine {
    pub(crate) fn new(
        physical_location: RpgMakerLocation,
        expected_raw: impl Into<String>,
    ) -> Self {
        Self {
            physical_location,
            expected_raw: expected_raw.into(),
        }
    }

    pub(crate) fn physical_location(&self) -> &RpgMakerLocation {
        &self.physical_location
    }

    pub(crate) fn expected_raw(&self) -> &str {
        &self.expected_raw
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedDialogue {
    units: Vec<ProjectedDialogueUnit>,
    recipe: DialogueWriteRecipe,
}

impl ProjectedDialogue {
    #[cfg(test)]
    pub(crate) fn speaker(&self) -> Option<&str> {
        self.units.iter().find_map(|unit| {
            (unit.role == TextUnitRole::DialogueSpeaker)
                .then(|| unit.source_content.as_value())
                .flatten()
        })
    }

    pub(crate) fn into_parts(self) -> (Vec<ProjectedDialogueUnit>, DialogueWriteRecipe) {
        (self.units, self.recipe)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedDialogueUnit {
    role: TextUnitRole,
    projection_location: RpgMakerLocation,
    source_content: TextUnitContent,
}

impl ProjectedDialogueUnit {
    fn new(
        role: TextUnitRole,
        projection_location: RpgMakerLocation,
        source_content: TextUnitContent,
    ) -> Self {
        Self {
            role,
            projection_location,
            source_content,
        }
    }

    pub(crate) fn into_parts(self) -> (TextUnitRole, RpgMakerLocation, TextUnitContent) {
        (self.role, self.projection_location, self.source_content)
    }
}

#[derive(Debug)]
pub(crate) struct MvDialogueTomlError {
    source: toml::de::Error,
    classification: MvDialogueTomlClassification,
    line: Option<u64>,
    column: Option<u64>,
}

impl MvDialogueTomlError {
    fn new(
        document: &str,
        classification: MvDialogueTomlClassification,
        source: toml::de::Error,
    ) -> Self {
        let (line, column) = source
            .span()
            .map(|span| source_line_column(document, span.start))
            .map_or((None, None), |(line, column)| (Some(line), Some(column)));
        Self {
            source,
            classification,
            line,
            column,
        }
    }
}

impl fmt::Display for MvDialogueTomlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                write!(
                    formatter,
                    "TOML {}失败（第 {line} 行、第 {column} 列）",
                    self.classification.as_str()
                )
            }
            _ => write!(formatter, "TOML {}失败", self.classification.as_str()),
        }
    }
}

impl Error for MvDialogueTomlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Copy, Debug)]
enum MvDialogueTomlClassification {
    Syntax,
    Data,
}

impl MvDialogueTomlClassification {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Data => "data",
        }
    }
}

#[derive(Debug)]
pub(crate) enum MvDialogueDefinitionError {
    EmptyDocument,
    MissingRuleArray,
    InvalidToml(Box<MvDialogueTomlError>),
    InvalidCanonicalJson(serde_json::Error),
    EncodeCanonicalJson(serde_json::Error),
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        source: pcre2::Error,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        captures: Vec<String>,
    },
}

impl fmt::Display for MvDialogueDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDocument => formatter.write_str("MV 对话定义不能为空或仅包含注释"),
            Self::MissingRuleArray => formatter.write_str("MV 对话定义必须显式包含 rule 数组"),
            Self::InvalidToml(source) => write!(formatter, "MV 对话 TOML 无效：{source}"),
            Self::InvalidCanonicalJson(source) => write!(
                formatter,
                "项目中的 MV 对话 JSON 定义无效（{}，第 {} 行、第 {} 列）",
                json_error_classification(source),
                source.line(),
                source.column()
            ),
            Self::EncodeCanonicalJson(source) => write!(
                formatter,
                "无法保存 MV 对话 JSON 定义（{}，第 {} 行、第 {} 列）",
                json_error_classification(source),
                source.line(),
                source.column()
            ),
            Self::EmptyPattern { rule_number } => {
                write!(formatter, "MV 对话规则 {rule_number} 的 pattern 为空")
            }
            Self::InvalidPattern {
                rule_number,
                source,
            } => write!(
                formatter,
                "MV 对话规则 {rule_number} 的 PCRE2 无效（kind={}，code={}，offset={:?}）",
                pcre2_error_kind(source),
                source.code(),
                source.offset()
            ),
            Self::InvalidNamedCaptures {
                rule_number,
                captures,
            } => write!(
                formatter,
                "MV 对话规则 {rule_number} 的命名捕获无效（{}）",
                named_capture_detail(*rule_number, captures)
            ),
        }
    }
}

impl Error for MvDialogueDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidToml(source) => Some(source.as_ref()),
            Self::InvalidCanonicalJson(source) | Self::EncodeCanonicalJson(source) => Some(source),
            Self::InvalidPattern { source, .. } => Some(source),
            Self::EmptyDocument
            | Self::MissingRuleArray
            | Self::EmptyPattern { .. }
            | Self::InvalidNamedCaptures { .. } => None,
        }
    }
}

impl MvDialogueDefinitionError {
    pub(crate) fn diagnostic_report(
        &self,
        origin: RpgMakerDialogueDefinitionOrigin,
    ) -> DiagnosticReport {
        assert_eq!(
            origin,
            RpgMakerDialogueDefinitionOrigin::ProjectSnapshot,
            "外部 MV Dialogue TOML 诊断必须使用 external_diagnostic_report 保留路径"
        );
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::project_dialogue_definition(
                self.diagnostic_problem(),
            )),
        )
    }

    pub(crate) fn external_diagnostic_report(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::external_dialogue_definition(
                path,
                self.diagnostic_problem(),
            )),
        )
    }

    pub(crate) fn diagnostic_problem(&self) -> RpgMakerDialogueDefinitionProblem {
        match self {
            Self::EmptyDocument => RpgMakerDialogueDefinitionProblem::EmptyDocument,
            Self::MissingRuleArray => RpgMakerDialogueDefinitionProblem::MissingRuleArray,
            Self::InvalidToml(source) => RpgMakerDialogueDefinitionProblem::InvalidToml {
                failure: match source.classification {
                    MvDialogueTomlClassification::Syntax => RpgMakerTomlFailureKind::Syntax,
                    MvDialogueTomlClassification::Data => RpgMakerTomlFailureKind::InvalidValue,
                },
                line: source.line,
                column: source.column,
            },
            Self::InvalidCanonicalJson(source) => {
                RpgMakerDialogueDefinitionProblem::InvalidCanonicalJson {
                    category: dialogue_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::EncodeCanonicalJson(source) => {
                RpgMakerDialogueDefinitionProblem::EncodeCanonicalJson {
                    category: dialogue_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::EmptyPattern { rule_number } => RpgMakerDialogueDefinitionProblem::EmptyPattern {
                rule_number: *rule_number,
            },
            Self::InvalidPattern {
                rule_number,
                source,
            } => RpgMakerDialogueDefinitionProblem::InvalidPattern {
                rule_number: *rule_number,
                failure: dialogue_pcre2_failure(source),
            },
            Self::InvalidNamedCaptures {
                rule_number,
                captures,
            } => {
                let safe_actual_captures = captures
                    .iter()
                    .filter(|capture| is_safe_capture_name(capture))
                    .map(SafeIdentifier::from_validated)
                    .collect::<Vec<_>>();
                RpgMakerDialogueDefinitionProblem::InvalidNamedCaptures {
                    rule_number: *rule_number,
                    hidden_count: captures.len().saturating_sub(safe_actual_captures.len()),
                    actual_count: captures.len(),
                    safe_actual_captures,
                }
            }
        }
    }
}

pub(crate) fn external_invalid_utf8_diagnostic_report(
    path: impl AsRef<std::path::Path>,
    source: &std::str::Utf8Error,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::external_dialogue_definition(
            path,
            RpgMakerDialogueDefinitionProblem::InvalidUtf8 {
                valid_up_to: source.valid_up_to(),
                error_len: source.error_len(),
            },
        )),
    )
}

#[derive(Debug)]
pub(crate) enum MvDialogueProjectionError {
    Match {
        rule_number: usize,
        location: Box<RpgMakerLocation>,
        source: Box<pcre2::Error>,
    },
    ZeroWidthMatch {
        rule_number: usize,
        location: Box<RpgMakerLocation>,
    },
    MissingSpeakerCapture {
        rule_number: usize,
        location: Box<RpgMakerLocation>,
    },
    InvalidSpeakerCaptureRange {
        rule_number: usize,
        location: Box<RpgMakerLocation>,
    },
    MultipleRulesOwnField {
        location: Box<RpgMakerLocation>,
        first_rule: usize,
        second_rule: usize,
    },
    DifferentSpeakers {
        location: Box<RpgMakerLocation>,
    },
    RuleCapturedNoSpeaker {
        rule_number: usize,
    },
    InvalidRecipe(Box<ProjectionModelError>),
}

impl fmt::Display for MvDialogueProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Match {
                rule_number,
                location,
                source,
            } => write!(
                formatter,
                "{location} 的 MV 对话规则 {rule_number} 匹配失败（kind={}，code={}，offset={:?}）",
                pcre2_error_kind(source),
                source.code(),
                source.offset()
            ),
            Self::ZeroWidthMatch {
                rule_number,
                location,
            } => {
                write!(
                    formatter,
                    "{location} 的 MV 对话规则 {rule_number} 产生零宽匹配"
                )
            }
            Self::MissingSpeakerCapture {
                rule_number,
                location,
            } => {
                write!(
                    formatter,
                    "{location} 的 MV 对话规则 {rule_number} 的 speaker 捕获未参与匹配"
                )
            }
            Self::InvalidSpeakerCaptureRange {
                rule_number,
                location,
            } => write!(
                formatter,
                "{location} 的 MV 对话规则 {rule_number} 的 speaker 捕获必须位于完整匹配内并对齐 UTF-8 字符边界"
            ),
            Self::MultipleRulesOwnField {
                location,
                first_rule,
                second_rule,
            } => write!(
                formatter,
                "{location} 同时被 MV 对话规则 {first_rule} 与 {second_rule} 拥有"
            ),
            Self::DifferentSpeakers { location } => {
                write!(formatter, "{location} 的多个姓名标记包含不同非空 Speaker")
            }
            Self::RuleCapturedNoSpeaker { rule_number } => write!(
                formatter,
                "MV 对话规则 {rule_number} 没有捕获任何非空 Speaker"
            ),
            Self::InvalidRecipe(source) => write!(
                formatter,
                "MV 对话物化配方无效（{}）",
                projection_model_detail(source)
            ),
        }
    }
}

impl Error for MvDialogueProjectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Match { source, .. } => Some(source.as_ref()),
            Self::InvalidRecipe(source) => Some(source.as_ref()),
            Self::ZeroWidthMatch { .. }
            | Self::MissingSpeakerCapture { .. }
            | Self::InvalidSpeakerCaptureRange { .. }
            | Self::MultipleRulesOwnField { .. }
            | Self::DifferentSpeakers { .. }
            | Self::RuleCapturedNoSpeaker { .. } => None,
        }
    }
}

impl MvDialogueProjectionError {
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::dialogue_projection(
                self.diagnostic_problem(),
            )),
        )
    }

    pub(crate) fn diagnostic_problem(&self) -> RpgMakerDialogueProjectionProblem {
        match self {
            Self::Match {
                rule_number,
                location,
                source,
            } => RpgMakerDialogueProjectionProblem::Match {
                rule_number: *rule_number,
                location: dialogue_diagnostic_location(location),
                failure: dialogue_pcre2_failure(source),
            },
            Self::ZeroWidthMatch {
                rule_number,
                location,
            } => RpgMakerDialogueProjectionProblem::ZeroWidthMatch {
                rule_number: *rule_number,
                location: dialogue_diagnostic_location(location),
            },
            Self::MissingSpeakerCapture {
                rule_number,
                location,
            } => RpgMakerDialogueProjectionProblem::MissingSpeakerCapture {
                rule_number: *rule_number,
                location: dialogue_diagnostic_location(location),
            },
            Self::InvalidSpeakerCaptureRange {
                rule_number,
                location,
            } => RpgMakerDialogueProjectionProblem::InvalidSpeakerCaptureRange {
                rule_number: *rule_number,
                location: dialogue_diagnostic_location(location),
            },
            Self::MultipleRulesOwnField {
                location,
                first_rule,
                second_rule,
            } => RpgMakerDialogueProjectionProblem::MultipleRulesOwnField {
                location: dialogue_diagnostic_location(location),
                first_rule: *first_rule,
                second_rule: *second_rule,
            },
            Self::DifferentSpeakers { location } => {
                RpgMakerDialogueProjectionProblem::DifferentSpeakers {
                    location: dialogue_diagnostic_location(location),
                }
            }
            Self::RuleCapturedNoSpeaker { rule_number } => {
                RpgMakerDialogueProjectionProblem::RuleCapturedNoSpeaker {
                    rule_number: *rule_number,
                }
            }
            Self::InvalidRecipe(source) => RpgMakerDialogueProjectionProblem::InvalidRecipe {
                failure: dialogue_projection_failure(source),
            },
        }
    }
}

fn source_line_column(source: &str, byte_offset: usize) -> (u64, u64) {
    let mut byte_offset = byte_offset.min(source.len());
    while !source.is_char_boundary(byte_offset) {
        byte_offset -= 1;
    }
    let prefix = &source[..byte_offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, current_line)| current_line)
        .chars()
        .count()
        + 1;
    (
        u64::try_from(line).expect("当前目标平台的 JSON 行号必须能用 u64 表达"),
        u64::try_from(column).expect("当前目标平台的 JSON 列号必须能用 u64 表达"),
    )
}

fn json_error_classification(source: &serde_json::Error) -> &'static str {
    JsonErrorCategory::from(source).storage_name()
}

fn dialogue_json_failure(source: &serde_json::Error) -> RpgMakerJsonFailureKind {
    match JsonErrorCategory::from(source) {
        JsonErrorCategory::Io => RpgMakerJsonFailureKind::Io,
        JsonErrorCategory::Syntax => RpgMakerJsonFailureKind::Syntax,
        JsonErrorCategory::Data => RpgMakerJsonFailureKind::Data,
        JsonErrorCategory::Eof => RpgMakerJsonFailureKind::Eof,
        JsonErrorCategory::DuplicateObjectKey => RpgMakerJsonFailureKind::DuplicateObjectKey,
    }
}

fn dialogue_pcre2_failure(source: &pcre2::Error) -> Pcre2Failure {
    Pcre2Failure {
        kind: match source.kind() {
            pcre2::ErrorKind::Compile => Pcre2FailureKind::Compile,
            pcre2::ErrorKind::JIT => Pcre2FailureKind::Jit,
            pcre2::ErrorKind::Match => Pcre2FailureKind::Match,
            pcre2::ErrorKind::Info => Pcre2FailureKind::Info,
            pcre2::ErrorKind::Option => Pcre2FailureKind::Option,
            _ => Pcre2FailureKind::Unrecognized,
        },
        code: source.code(),
        offset: source.offset(),
    }
}

fn dialogue_diagnostic_location(location: &RpgMakerLocation) -> RpgMakerDiagnosticLocation {
    RpgMakerDiagnosticLocation::new(
        match location.source() {
            RpgMakerSource::Data(file) => RpgMakerDiagnosticSource::data(file.file_name()),
            RpgMakerSource::DataFile(file) => RpgMakerDiagnosticSource::data_file(file.as_str()),
            RpgMakerSource::Map(map_id) => RpgMakerDiagnosticSource::map(map_id.get()),
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => RpgMakerDiagnosticSource::plugin_parameter(
                *plugin_index,
                plugin_name,
                parameter_name,
            ),
        },
        location
            .steps()
            .iter()
            .map(|step| match step {
                RpgMakerLocationStep::ObjectKey(key) => {
                    RpgMakerDiagnosticLocationStep::object_key(key)
                }
                RpgMakerLocationStep::ArrayIndex(index) => {
                    RpgMakerDiagnosticLocationStep::array_index(*index)
                }
                RpgMakerLocationStep::DecodeJsonString => {
                    RpgMakerDiagnosticLocationStep::decode_json_string()
                }
            })
            .collect(),
    )
}

fn dialogue_projection_failure(source: &ProjectionModelError) -> RpgMakerProjectionFailureKind {
    match source {
        ProjectionModelError::EmptyScalarFieldKey => {
            RpgMakerProjectionFailureKind::EmptyScalarFieldKey
        }
        ProjectionModelError::EventBlockCoverageRequired => {
            RpgMakerProjectionFailureKind::EventBlockCoverageRequired
        }
        ProjectionModelError::InvalidEventBlockCoverage => {
            RpgMakerProjectionFailureKind::InvalidEventBlockCoverage
        }
        ProjectionModelError::MutationClaimTargetMismatch => {
            RpgMakerProjectionFailureKind::MutationClaimTargetMismatch
        }
        ProjectionModelError::RecipeHasNoTextSlot => {
            RpgMakerProjectionFailureKind::RecipeHasNoTextSlot
        }
        ProjectionModelError::DuplicateProjectionSlot { .. } => {
            RpgMakerProjectionFailureKind::DuplicateProjectionSlot
        }
        ProjectionModelError::MultipleBodyLinesInPhysicalLine => {
            RpgMakerProjectionFailureKind::MultipleBodyLinesInPhysicalLine
        }
        ProjectionModelError::DuplicateDialogueBodyLine { .. } => {
            RpgMakerProjectionFailureKind::DuplicateDialogueBodyLine
        }
        ProjectionModelError::NonContiguousDialogueBodyLines { .. } => {
            RpgMakerProjectionFailureKind::NonContiguousDialogueBodyLines
        }
        ProjectionModelError::MixedDirectAndInlineSpeaker => {
            RpgMakerProjectionFailureKind::MixedDirectAndInlineSpeaker
        }
    }
}

fn pcre2_error_kind(source: &pcre2::Error) -> &'static str {
    match source.kind() {
        pcre2::ErrorKind::Compile => "compile",
        pcre2::ErrorKind::JIT => "jit",
        pcre2::ErrorKind::Match => "match",
        pcre2::ErrorKind::Info => "info",
        pcre2::ErrorKind::Option => "option",
        _ => "unknown",
    }
}

fn named_capture_detail(rule_number: usize, captures: &[String]) -> String {
    let safe_captures = captures
        .iter()
        .filter(|capture| is_safe_capture_name(capture))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let hidden_count = captures.len().saturating_sub(safe_captures.len());
    format!(
        "rule_number={rule_number}; required_captures=[speaker]; safe_actual_captures=[{}]; actual_capture_count={}; hidden_capture_count={hidden_count}",
        safe_captures.join(","),
        captures.len()
    )
}

fn is_safe_capture_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// 将投影模型的闭集变体转换成不含正文的稳定结构说明。
pub(crate) fn projection_model_detail(source: &ProjectionModelError) -> String {
    match source {
        ProjectionModelError::EmptyScalarFieldKey => "structure=empty_scalar_field_key".to_owned(),
        ProjectionModelError::EventBlockCoverageRequired => {
            "structure=event_block_coverage_required".to_owned()
        }
        ProjectionModelError::InvalidEventBlockCoverage => {
            "structure=invalid_event_block_coverage".to_owned()
        }
        ProjectionModelError::MutationClaimTargetMismatch => {
            "structure=mutation_claim_target_mismatch".to_owned()
        }
        ProjectionModelError::RecipeHasNoTextSlot => "structure=recipe_has_no_text_slot".to_owned(),
        ProjectionModelError::DuplicateProjectionSlot {
            role,
            source_line_index,
        } => format!(
            "structure=duplicate_projection_slot; role={}; source_line_index={}",
            text_unit_role_name(role),
            source_line_index.map_or_else(|| "none".to_owned(), |value| value.to_string())
        ),
        ProjectionModelError::MultipleBodyLinesInPhysicalLine => {
            "structure=multiple_body_lines_in_physical_line".to_owned()
        }
        ProjectionModelError::DuplicateDialogueBodyLine { source_line_index } => {
            format!("structure=duplicate_dialogue_body_line; source_line_index={source_line_index}")
        }
        ProjectionModelError::NonContiguousDialogueBodyLines { expected, actual } => format!(
            "structure=non_contiguous_dialogue_body_lines; expected={expected}; actual={actual}"
        ),
        ProjectionModelError::MixedDirectAndInlineSpeaker => {
            "structure=mixed_direct_and_inline_speaker".to_owned()
        }
    }
}

fn text_unit_role_name(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::Scalar(_) => "scalar",
        TextUnitRole::DialogueSpeaker => "dialogue_speaker",
        TextUnitRole::DialogueBody => "dialogue_body",
        TextUnitRole::Choices => "choices",
        TextUnitRole::ScrollingText => "scrolling_text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::model::DialogueLinePart;
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource};

    fn location(index: usize) -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(index),
            ],
        )
    }

    #[test]
    fn strict_toml_requires_explicit_rule_array_and_only_pattern() {
        assert_eq!(
            MvDialogueDefinition::parse_toml("rule = []")
                .expect("显式空定义应合法")
                .rules(),
            []
        );
        for source in [
            "",
            "# comment",
            "other = []",
            "[[rule]]\npattern='x'\ncode=401",
        ] {
            assert!(
                MvDialogueDefinition::parse_toml(source).is_err(),
                "{source:?} 应拒绝"
            );
        }
    }

    #[test]
    fn projects_inline_repeated_marker_and_body_reversibly() {
        let definition = MvDialogueDefinition::parse_toml(
            r#"[[rule]]
pattern = '(?i)\\n<(?<speaker>[^>]*?)(?::)?>'
"#,
        )
        .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");
        let projected = projector
            .project(
                location(0),
                vec![
                    DialoguePhysicalLine::new(location(1), "\\C[18]\\n<莉莉:>\\n<莉莉:>你好"),
                    DialoguePhysicalLine::new(location(2), "第二行"),
                ],
            )
            .expect("inline 姓名应投影");
        projector.finish().expect("规则已捕获非空姓名");

        assert_eq!(projected.speaker(), Some("莉莉"));
        let (units, recipe) = projected.into_parts();
        assert_eq!(units.len(), 2);
        let body = units
            .iter()
            .find(|unit| unit.role == TextUnitRole::DialogueBody)
            .expect("应形成一个完整正文单元");
        assert_eq!(
            body.source_content.as_lines(),
            Some(["你好".to_owned(), "第二行".to_owned()].as_slice())
        );
        assert_eq!(recipe.lines().len(), 2);
        assert_eq!(
            recipe.lines()[0].parts(),
            &[
                DialogueLinePart::Literal("\\C[18]\\n<".to_owned()),
                DialogueLinePart::SpeakerSlot,
                DialogueLinePart::Literal(":>\\n<".to_owned()),
                DialogueLinePart::SpeakerSlot,
                DialogueLinePart::Literal(":>".to_owned()),
                DialogueLinePart::BodyLine {
                    source_line_index: 0,
                },
            ]
        );
    }

    #[test]
    fn exact_first_line_speaker_has_no_body_slot() {
        let definition = MvDialogueDefinition::parse_toml(
            "[[rule]]\npattern = '\\A(?<speaker>バニー淫魔)\\z'\n",
        )
        .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");
        let projected = projector
            .project(
                location(0),
                vec![
                    DialoguePhysicalLine::new(location(1), "バニー淫魔"),
                    DialoguePhysicalLine::new(location(2), "「台词」"),
                ],
            )
            .expect("首行姓名应投影");
        projector.finish().expect("规则已命中");

        let (_, recipe) = projected.into_parts();
        assert!(
            recipe.lines()[0]
                .parts()
                .iter()
                .all(|part| !matches!(part, DialogueLinePart::BodyLine { .. }))
        );
    }

    #[test]
    fn marker_trailing_whitespace_is_frozen_in_the_recipe() {
        let definition = MvDialogueDefinition::parse_toml(
            r#"[[rule]]
pattern = '(?i)\\n<(?<speaker>[^>]*?)(?::)?>'
"#,
        )
        .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");
        let projected = projector
            .project(
                location(0),
                vec![
                    DialoguePhysicalLine::new(location(1), "\\n<莉莉:>   "),
                    DialoguePhysicalLine::new(location(2), "第二行"),
                ],
            )
            .expect("姓名后的纯空白后缀应原样冻结");
        projector.finish().expect("规则已捕获非空姓名");

        let (_, recipe) = projected.into_parts();
        assert_eq!(
            recipe.lines()[0].parts(),
            &[
                DialogueLinePart::Literal("\\n<".to_owned()),
                DialogueLinePart::SpeakerSlot,
                DialogueLinePart::Literal(":>   ".to_owned()),
            ]
        );
        assert_eq!(recipe.lines()[0].expected_raw(), "\\n<莉莉:>   ");
    }

    #[test]
    fn speaker_capture_must_stay_inside_the_complete_match() {
        let definition =
            MvDialogueDefinition::parse_toml("[[rule]]\npattern = 'A(?=(?<speaker>B))'\n")
                .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");

        assert!(matches!(
            projector.project(
                location(0),
                vec![DialoguePhysicalLine::new(location(1), "AB")],
            ),
            Err(MvDialogueProjectionError::InvalidSpeakerCaptureRange { rule_number: 1, .. })
        ));
    }

    #[test]
    fn speaker_capture_must_align_with_utf8_boundaries() {
        let definition =
            MvDialogueDefinition::parse_toml("[[rule]]\npattern = '(?<speaker>\\C)'\n")
                .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");

        assert!(matches!(
            projector.project(
                location(0),
                vec![DialoguePhysicalLine::new(location(1), "莉")],
            ),
            Err(MvDialogueProjectionError::InvalidSpeakerCaptureRange { rule_number: 1, .. })
        ));
    }

    #[test]
    fn malformed_near_match_stays_ordinary_body() {
        let definition = MvDialogueDefinition::parse_toml(
            r#"[[rule]]
pattern = '\\n<(?<speaker>[^>]+)>'
"#,
        )
        .expect("定义应合法");
        let mut projector = definition.compile().expect("PCRE2 应合法");
        let projected = projector
            .project(
                location(0),
                vec![DialoguePhysicalLine::new(location(1), "\\n<缺少结尾正文")],
            )
            .expect("畸形近似值不应猜测修复");
        assert_eq!(projected.speaker(), None);
        assert!(matches!(
            projector.finish(),
            Err(MvDialogueProjectionError::RuleCapturedNoSpeaker { rule_number: 1 })
        ));
    }

    #[test]
    fn definition_diagnostics_publish_typed_positions_without_document_or_pattern_text() {
        const TOML_BODY: &str = "TOML_DOCUMENT_BODY_SENTINEL";
        let toml_error =
            MvDialogueDefinition::parse_toml(&format!("[[rule]\npattern = '{TOML_BODY}'\n"))
                .expect_err("缺少右方括号的 TOML 应拒绝");
        let RpgMakerDialogueDefinitionProblem::InvalidToml {
            failure,
            line,
            column,
        } = toml_error.diagnostic_problem()
        else {
            panic!("TOML 失败应提供分类与行列")
        };
        assert_eq!(failure, RpgMakerTomlFailureKind::Syntax);
        assert_eq!(line, Some(1));
        assert!(column.is_some());
        let wire =
            serde_json::to_value(toml_error.external_diagnostic_report("config/mv-dialogue.toml"))
                .expect("MV 对话定义诊断必须可序列化");
        assert_eq!(
            wire["primary"]["code"],
            "rpg_maker.dialogue.definition.invalid_toml"
        );
        assert_eq!(wire["primary"]["stage"], "command_preparation");
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["problem"]["failure"],
            "syntax"
        );
        assert!(!wire.to_string().contains(TOML_BODY));

        const TOML_DATA_VALUE: &str = "TOML_DATA_VALUE_SENTINEL";
        let toml_data_error =
            MvDialogueDefinition::parse_toml(&format!("other = '{TOML_DATA_VALUE}'\n"))
                .expect_err("未知 TOML 字段应作为 data 分类拒绝");
        let RpgMakerDialogueDefinitionProblem::InvalidToml {
            line,
            column,
            failure,
        } = toml_data_error.diagnostic_problem()
        else {
            panic!("TOML data 失败应提供分类与行列")
        };
        assert!(line.is_some());
        assert!(column.is_some());
        assert_eq!(failure, RpgMakerTomlFailureKind::InvalidValue);
        assert!(
            !serde_json::to_string(
                &toml_data_error.external_diagnostic_report("config/mv-dialogue.toml")
            )
            .expect("安全 TOML data 报告应可序列化")
            .contains(TOML_DATA_VALUE)
        );

        const JSON_VALUE: &str = "JSON_VALUE_SENTINEL";
        let json_error = MvDialogueDefinition::from_canonical_json(&format!(
            "{{\n\"rules\": \"{JSON_VALUE}\"\n}}"
        ))
        .expect_err("rules 类型错误的 JSON 应拒绝");
        let RpgMakerDialogueDefinitionProblem::InvalidCanonicalJson {
            category,
            line,
            column,
        } = json_error.diagnostic_problem()
        else {
            panic!("JSON 失败应提供闭集分类与行列")
        };
        assert_eq!(category, RpgMakerJsonFailureKind::Data);
        assert_eq!(line, 2);
        assert!(column > 0);
        assert!(
            !serde_json::to_string(
                &json_error.diagnostic_report(RpgMakerDialogueDefinitionOrigin::ProjectSnapshot)
            )
            .expect("安全 JSON 报告应可序列化")
            .contains(JSON_VALUE)
        );

        const PATTERN_BODY: &str = "PATTERN_BODY_SENTINEL";
        let definition = MvDialogueDefinition::parse_toml(&format!(
            "[[rule]]\npattern = '(?<speaker>{PATTERN_BODY}'\n"
        ))
        .expect("TOML 本身应合法");
        let pattern_error = match definition.compile() {
            Err(error) => error,
            Ok(_) => panic!("未闭合 PCRE2 应拒绝"),
        };
        let RpgMakerDialogueDefinitionProblem::InvalidPattern { failure, .. } =
            pattern_error.diagnostic_problem()
        else {
            panic!("PCRE2 编译失败应提供稳定 code/offset")
        };
        assert_ne!(failure.code, 0);
        assert!(failure.offset.is_some());
        assert!(
            !serde_json::to_string(&pattern_error.external_diagnostic_report("dialogue.toml"))
                .expect("PCRE2 报告应可序列化")
                .contains(PATTERN_BODY)
        );
    }

    #[test]
    fn named_capture_diagnostic_only_exposes_safe_identifiers() {
        const CONTROL_CAPTURE: &str = "CAPTURE_WITH_CONTROL\nVALUE";
        let error = MvDialogueDefinitionError::InvalidNamedCaptures {
            rule_number: 7,
            captures: vec![
                "speaker".to_owned(),
                "safe_capture_2".to_owned(),
                CONTROL_CAPTURE.to_owned(),
            ],
        };
        let RpgMakerDialogueDefinitionProblem::InvalidNamedCaptures {
            safe_actual_captures,
            actual_count,
            hidden_count,
            ..
        } = error.diagnostic_problem()
        else {
            panic!("命名捕获错误应提供安全名称清单")
        };
        assert_eq!(actual_count, 3);
        assert_eq!(hidden_count, 1);
        assert_eq!(
            safe_actual_captures
                .iter()
                .map(SafeIdentifier::as_str)
                .collect::<Vec<_>>(),
            ["speaker", "safe_capture_2"]
        );
        assert!(
            !serde_json::to_string(&error.external_diagnostic_report("dialogue.toml"))
                .expect("命名捕获报告应可序列化")
                .contains(CONTROL_CAPTURE)
        );
    }

    #[test]
    fn projection_diagnostic_keeps_location_rule_conflict_and_structure_reason() {
        let exact_location = location(9);
        let conflict = MvDialogueProjectionError::MultipleRulesOwnField {
            location: Box::new(exact_location.clone()),
            first_rule: 2,
            second_rule: 5,
        };
        let RpgMakerDialogueProjectionProblem::MultipleRulesOwnField {
            location,
            first_rule,
            second_rule,
        } = conflict.diagnostic_problem()
        else {
            panic!("规则冲突应提供具体 ownership 事实")
        };
        assert_eq!(first_rule, 2);
        assert_eq!(second_rule, 5);
        assert_eq!(location, dialogue_diagnostic_location(&exact_location));
        let wire = serde_json::to_value(conflict.diagnostic_report())
            .expect("MV 对话投影诊断必须可序列化");
        assert_eq!(
            wire["primary"]["code"],
            "rpg_maker.dialogue.multiple_rules_own_field"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["problem"]["location"]["steps"][1]["index"],
            9
        );

        let invalid_recipe = MvDialogueProjectionError::InvalidRecipe(Box::new(
            ProjectionModelError::NonContiguousDialogueBodyLines {
                expected: 3,
                actual: 5,
            },
        ));
        let RpgMakerDialogueProjectionProblem::InvalidRecipe { failure } =
            invalid_recipe.diagnostic_problem()
        else {
            panic!("配方错误应保留具体结构变体")
        };
        assert_eq!(
            failure,
            RpgMakerProjectionFailureKind::NonContiguousDialogueBodyLines
        );
    }
}
