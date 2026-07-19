//! MV 第一条 `401` 的局部姓名投影定义与物化算法。

use std::error::Error;
use std::fmt;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, ProjectionModelError, TextFieldRole,
};
use super::text::RpgMakerLocation;

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
        if source.trim().is_empty() {
            return Err(MvDialogueDefinitionError::EmptyDocument);
        }
        let external = toml::from_str::<ExternalDefinition>(source)
            .map_err(MvDialogueDefinitionError::InvalidToml)?;
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
        let mut leaves = Vec::new();
        let mut line_recipes = Vec::with_capacity(lines.len());
        let mut body_index = 0;

        for (line_index, line) in lines.into_iter().enumerate() {
            if line_index == 0 {
                let projection = self.project_first_line(&line)?;
                if let Some(value) = projection.speaker {
                    leaves.push(ProjectedDialogueLeaf::new(
                        TextFieldRole::DialogueSpeaker,
                        line.physical_location.clone(),
                        value,
                    ));
                }
                let mut parts = projection.prefix_parts;
                if !projection.body.trim().is_empty() {
                    leaves.push(ProjectedDialogueLeaf::new(
                        TextFieldRole::DialogueBody { index: body_index },
                        line.physical_location.clone(),
                        projection.body,
                    ));
                    parts.push(DialogueLinePart::BodySlot { index: body_index });
                    body_index += 1;
                } else if !projection.body.is_empty() {
                    push_literal(&mut parts, &projection.body);
                } else if parts.is_empty() {
                    parts.push(DialogueLinePart::Literal(line.expected_raw.clone()));
                }
                line_recipes.push(
                    DialogueLineRecipe::new(line.physical_location, line.expected_raw, parts)
                        .map_err(|source| {
                            MvDialogueProjectionError::InvalidRecipe(Box::new(source))
                        })?,
                );
                continue;
            }

            let parts = if line.expected_raw.trim().is_empty() {
                vec![DialogueLinePart::Literal(line.expected_raw.clone())]
            } else {
                leaves.push(ProjectedDialogueLeaf::new(
                    TextFieldRole::DialogueBody { index: body_index },
                    line.physical_location.clone(),
                    line.expected_raw.clone(),
                ));
                let parts = vec![DialogueLinePart::BodySlot { index: body_index }];
                body_index += 1;
                parts
            };
            line_recipes.push(
                DialogueLineRecipe::new(line.physical_location, line.expected_raw, parts)
                    .map_err(|source| MvDialogueProjectionError::InvalidRecipe(Box::new(source)))?,
            );
        }

        let recipe = DialogueWriteRecipe::new(group_location, None, line_recipes)
            .map_err(|source| MvDialogueProjectionError::InvalidRecipe(Box::new(source)))?;
        Ok(ProjectedDialogue { leaves, recipe })
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
            let rule_matches = collect_matches(rule, &line.expected_raw)?;
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
            .filter(|speaker| !speaker.trim().is_empty())
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
            if matched.speaker.trim().is_empty() {
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
) -> Result<Vec<SpeakerMatch>, MvDialogueProjectionError> {
    let mut matches = Vec::new();
    for captures in rule.regex.captures_iter(text.as_bytes()) {
        let captures = captures.map_err(|source| MvDialogueProjectionError::Match {
            rule_number: rule.rule_number,
            source: Box::new(source),
        })?;
        let whole = captures
            .get(0)
            .expect("成功的 PCRE2 captures 必须包含完整匹配");
        if whole.start() == whole.end() {
            return Err(MvDialogueProjectionError::ZeroWidthMatch {
                rule_number: rule.rule_number,
            });
        }
        let speaker =
            captures
                .name("speaker")
                .ok_or(MvDialogueProjectionError::MissingSpeakerCapture {
                    rule_number: rule.rule_number,
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
            });
        }
        let speaker_text = text[speaker.start()..speaker.end()].to_owned();
        if !speaker_text.trim().is_empty() {
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
    leaves: Vec<ProjectedDialogueLeaf>,
    recipe: DialogueWriteRecipe,
}

impl ProjectedDialogue {
    #[cfg(test)]
    pub(crate) fn speaker(&self) -> Option<&str> {
        self.leaves.iter().find_map(|leaf| {
            (leaf.role == TextFieldRole::DialogueSpeaker).then_some(leaf.original_text.as_str())
        })
    }

    pub(crate) fn into_parts(self) -> (Vec<ProjectedDialogueLeaf>, DialogueWriteRecipe) {
        (self.leaves, self.recipe)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedDialogueLeaf {
    role: TextFieldRole,
    physical_location: RpgMakerLocation,
    original_text: String,
}

impl ProjectedDialogueLeaf {
    fn new(
        role: TextFieldRole,
        physical_location: RpgMakerLocation,
        original_text: String,
    ) -> Self {
        Self {
            role,
            physical_location,
            original_text,
        }
    }

    pub(crate) fn into_parts(self) -> (TextFieldRole, RpgMakerLocation, String) {
        (self.role, self.physical_location, self.original_text)
    }
}

#[derive(Debug)]
pub(crate) enum MvDialogueDefinitionError {
    EmptyDocument,
    MissingRuleArray,
    InvalidToml(toml::de::Error),
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
            Self::InvalidCanonicalJson(source) => {
                write!(formatter, "项目中的 MV 对话定义无效：{source}")
            }
            Self::EncodeCanonicalJson(source) => {
                write!(formatter, "无法保存 MV 对话定义：{source}")
            }
            Self::EmptyPattern { rule_number } => {
                write!(formatter, "MV 对话规则 {rule_number} 的 pattern 为空")
            }
            Self::InvalidPattern {
                rule_number,
                source,
            } => {
                write!(
                    formatter,
                    "MV 对话规则 {rule_number} 的 PCRE2 无效：{source}"
                )
            }
            Self::InvalidNamedCaptures {
                rule_number,
                captures,
            } => write!(
                formatter,
                "MV 对话规则 {rule_number} 必须且只能包含 speaker 命名捕获，实际为 {captures:?}"
            ),
        }
    }
}

impl Error for MvDialogueDefinitionError {}

#[derive(Debug)]
pub(crate) enum MvDialogueProjectionError {
    Match {
        rule_number: usize,
        source: Box<pcre2::Error>,
    },
    ZeroWidthMatch {
        rule_number: usize,
    },
    MissingSpeakerCapture {
        rule_number: usize,
    },
    InvalidSpeakerCaptureRange {
        rule_number: usize,
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
                source,
            } => {
                write!(formatter, "MV 对话规则 {rule_number} 匹配失败：{source}")
            }
            Self::ZeroWidthMatch { rule_number } => {
                write!(formatter, "MV 对话规则 {rule_number} 产生零宽匹配")
            }
            Self::MissingSpeakerCapture { rule_number } => {
                write!(
                    formatter,
                    "MV 对话规则 {rule_number} 的 speaker 捕获未参与匹配"
                )
            }
            Self::InvalidSpeakerCaptureRange { rule_number } => write!(
                formatter,
                "MV 对话规则 {rule_number} 的 speaker 捕获必须位于完整匹配内并对齐 UTF-8 字符边界"
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
            Self::InvalidRecipe(source) => write!(formatter, "MV 对话物化配方无效：{source}"),
        }
    }
}

impl Error for MvDialogueProjectionError {}

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
        let (leaves, recipe) = projected.into_parts();
        assert_eq!(leaves.len(), 3);
        assert_eq!(recipe.lines().len(), 2);
        assert_eq!(
            recipe.lines()[0].parts(),
            &[
                DialogueLinePart::Literal("\\C[18]\\n<".to_owned()),
                DialogueLinePart::SpeakerSlot,
                DialogueLinePart::Literal(":>\\n<".to_owned()),
                DialogueLinePart::SpeakerSlot,
                DialogueLinePart::Literal(":>".to_owned()),
                DialogueLinePart::BodySlot { index: 0 },
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
                .all(|part| !matches!(part, DialogueLinePart::BodySlot { .. }))
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
            Err(MvDialogueProjectionError::InvalidSpeakerCaptureRange { rule_number: 1 })
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
            Err(MvDialogueProjectionError::InvalidSpeakerCaptureRange { rule_number: 1 })
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
}
