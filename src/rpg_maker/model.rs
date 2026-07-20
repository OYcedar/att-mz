//! 语义文本单元身份、物理修改目标与物化写回配方。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::text::RpgMakerLocation;

/// 标量字段的稳定、由提取器生成的语义键。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ScalarFieldKey(String);

impl ScalarFieldKey {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, ProjectionModelError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProjectionModelError::EmptyScalarFieldKey);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 可独立翻译、验收、持久化和写回的语义角色。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum TextUnitRole {
    Scalar(ScalarFieldKey),
    DialogueSpeaker,
    DialogueBody,
    Choices,
    ScrollingText,
}

impl TextUnitRole {
    pub(crate) const fn expects_lines(&self) -> bool {
        matches!(
            self,
            Self::DialogueBody | Self::Choices | Self::ScrollingText
        )
    }
}

/// 一个语义单元的完整文本内容。
///
/// 无标签序列化使 SQLite 中的权威内容直接表现为 JSON string 或 string array，
/// 不把内部类型包装泄漏到持久化数据中。
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub(crate) enum TextUnitContent {
    Value(String),
    Lines(Vec<String>),
}

impl TextUnitContent {
    pub(crate) fn as_value(&self) -> Option<&str> {
        match self {
            Self::Value(value) => Some(value),
            Self::Lines(_) => None,
        }
    }

    pub(crate) fn as_lines(&self) -> Option<&[String]> {
        match self {
            Self::Value(_) => None,
            Self::Lines(lines) => Some(lines),
        }
    }

    pub(crate) fn is_blank(&self) -> bool {
        match self {
            Self::Value(value) => value.trim().is_empty(),
            Self::Lines(lines) => lines.iter().all(|line| line.trim().is_empty()),
        }
    }
}

/// 译文单元的权威身份，不等同于物理 JSON 地址。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LogicalTextLocation {
    group_location: RpgMakerLocation,
    role: TextUnitRole,
}

impl LogicalTextLocation {
    pub(crate) fn new(group_location: RpgMakerLocation, role: TextUnitRole) -> Self {
        Self {
            group_location,
            role,
        }
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn role(&self) -> &TextUnitRole {
        &self.role
    }
}

/// 写回冲突检测使用的物理修改目标。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MutationTarget {
    Value(RpgMakerLocation),
    DialogueBlock { header: RpgMakerLocation },
}

/// 一个已物化、无需重新运行外部规则的写回配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TextProjectionRecipe {
    Direct(DirectTextRecipe),
    Dialogue(DialogueWriteRecipe),
}

impl TextProjectionRecipe {
    pub(crate) fn mutation_targets(&self) -> Vec<MutationTarget> {
        match self {
            Self::Direct(recipe) => vec![MutationTarget::Value(recipe.target().clone())],
            Self::Dialogue(recipe) => {
                let mut targets = vec![MutationTarget::DialogueBlock {
                    header: recipe.group_location().clone(),
                }];
                if let Some(speaker) = recipe.direct_speaker() {
                    targets.push(MutationTarget::Value(speaker.physical_location().clone()));
                }
                targets.extend(
                    recipe
                        .lines()
                        .iter()
                        .map(|line| MutationTarget::Value(line.physical_location().clone())),
                );
                targets
            }
        }
    }

    pub(crate) fn referenced_roles(&self) -> BTreeSet<TextUnitRole> {
        match self {
            Self::Direct(recipe) => recipe
                .parts()
                .iter()
                .filter_map(|part| match part {
                    DirectTextPart::Literal(_) => None,
                    DirectTextPart::TextSlot { role } | DirectTextPart::LineSlot { role, .. } => {
                        Some(role.clone())
                    }
                })
                .collect(),
            Self::Dialogue(recipe) => {
                let mut roles = BTreeSet::new();
                if recipe.direct_speaker().is_some()
                    || recipe.lines().iter().any(|line| {
                        line.parts()
                            .iter()
                            .any(|part| matches!(part, DialogueLinePart::SpeakerSlot))
                    })
                {
                    roles.insert(TextUnitRole::DialogueSpeaker);
                }
                if recipe.lines().iter().any(|line| {
                    line.parts()
                        .iter()
                        .any(|part| matches!(part, DialogueLinePart::BodyLine { .. }))
                }) {
                    roles.insert(TextUnitRole::DialogueBody);
                }
                roles
            }
        }
    }

    pub(crate) fn referenced_lines(&self) -> Vec<(TextUnitRole, usize)> {
        match self {
            Self::Direct(recipe) => recipe
                .parts()
                .iter()
                .filter_map(|part| match part {
                    DirectTextPart::LineSlot {
                        role,
                        source_line_index,
                    } => Some((role.clone(), *source_line_index)),
                    DirectTextPart::Literal(_) | DirectTextPart::TextSlot { .. } => None,
                })
                .collect(),
            Self::Dialogue(recipe) => recipe
                .lines()
                .iter()
                .flat_map(DialogueLineRecipe::parts)
                .filter_map(|part| match part {
                    DialogueLinePart::BodyLine { source_line_index } => {
                        Some((TextUnitRole::DialogueBody, *source_line_index))
                    }
                    DialogueLinePart::Literal(_) | DialogueLinePart::SpeakerSlot => None,
                })
                .collect(),
        }
    }
}

/// 整字段、局部正则文本或语义行的可逆配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectTextRecipe {
    target: RpgMakerLocation,
    expected_raw: String,
    parts: Vec<DirectTextPart>,
}

impl DirectTextRecipe {
    pub(crate) fn new(
        target: RpgMakerLocation,
        expected_raw: impl Into<String>,
        parts: Vec<DirectTextPart>,
    ) -> Result<Self, ProjectionModelError> {
        let expected_raw = expected_raw.into();
        let has_text_slot = parts.iter().any(|part| {
            matches!(
                part,
                DirectTextPart::TextSlot { .. } | DirectTextPart::LineSlot { .. }
            )
        });
        let is_frozen_literal =
            matches!(parts.as_slice(), [DirectTextPart::Literal(value)] if value == &expected_raw);
        if parts.is_empty() || (!has_text_slot && !is_frozen_literal) {
            return Err(ProjectionModelError::RecipeHasNoTextSlot);
        }

        let mut slots = BTreeSet::new();
        for part in &parts {
            let slot = match part {
                DirectTextPart::Literal(_) => continue,
                DirectTextPart::TextSlot { role } => (role.clone(), None),
                DirectTextPart::LineSlot {
                    role,
                    source_line_index,
                } => (role.clone(), Some(*source_line_index)),
            };
            if !slots.insert(slot.clone()) {
                return Err(ProjectionModelError::DuplicateProjectionSlot {
                    role: slot.0,
                    source_line_index: slot.1,
                });
            }
        }

        Ok(Self {
            target,
            expected_raw,
            parts,
        })
    }

    pub(crate) fn target(&self) -> &RpgMakerLocation {
        &self.target
    }

    pub(crate) fn expected_raw(&self) -> &str {
        &self.expected_raw
    }

    pub(crate) fn parts(&self) -> &[DirectTextPart] {
        &self.parts
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectTextPart {
    Literal(String),
    TextSlot {
        role: TextUnitRole,
    },
    LineSlot {
        role: TextUnitRole,
        source_line_index: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DialogueWriteRecipe {
    group_location: RpgMakerLocation,
    direct_speaker: Option<DirectSpeakerTarget>,
    lines: Vec<DialogueLineRecipe>,
}

impl DialogueWriteRecipe {
    pub(crate) fn new(
        group_location: RpgMakerLocation,
        direct_speaker: Option<DirectSpeakerTarget>,
        lines: Vec<DialogueLineRecipe>,
    ) -> Result<Self, ProjectionModelError> {
        let mut body_line_indexes = BTreeSet::new();
        let mut has_speaker_slot = false;
        for line in &lines {
            for part in line.parts() {
                match part {
                    DialogueLinePart::SpeakerSlot => has_speaker_slot = true,
                    DialogueLinePart::BodyLine { source_line_index }
                        if !body_line_indexes.insert(*source_line_index) =>
                    {
                        return Err(ProjectionModelError::DuplicateDialogueBodyLine {
                            source_line_index: *source_line_index,
                        });
                    }
                    DialogueLinePart::Literal(_) | DialogueLinePart::BodyLine { .. } => {}
                }
            }
        }
        if direct_speaker.is_some() && has_speaker_slot {
            return Err(ProjectionModelError::MixedDirectAndInlineSpeaker);
        }
        for (expected, actual) in body_line_indexes.iter().copied().enumerate() {
            if expected != actual {
                return Err(ProjectionModelError::NonContiguousDialogueBodyLines {
                    expected,
                    actual,
                });
            }
        }
        Ok(Self {
            group_location,
            direct_speaker,
            lines,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn direct_speaker(&self) -> Option<&DirectSpeakerTarget> {
        self.direct_speaker.as_ref()
    }

    pub(crate) fn lines(&self) -> &[DialogueLineRecipe] {
        &self.lines
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectSpeakerTarget {
    physical_location: RpgMakerLocation,
    expected_raw: String,
}

impl DirectSpeakerTarget {
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
pub(crate) struct DialogueLineRecipe {
    physical_location: RpgMakerLocation,
    expected_raw: String,
    parts: Vec<DialogueLinePart>,
}

impl DialogueLineRecipe {
    pub(crate) fn new(
        physical_location: RpgMakerLocation,
        expected_raw: impl Into<String>,
        parts: Vec<DialogueLinePart>,
    ) -> Result<Self, ProjectionModelError> {
        let body_lines = parts
            .iter()
            .filter(|part| matches!(part, DialogueLinePart::BodyLine { .. }))
            .count();
        if body_lines > 1 {
            return Err(ProjectionModelError::MultipleBodyLinesInPhysicalLine);
        }
        Ok(Self {
            physical_location,
            expected_raw: expected_raw.into(),
            parts,
        })
    }

    pub(crate) fn physical_location(&self) -> &RpgMakerLocation {
        &self.physical_location
    }

    pub(crate) fn expected_raw(&self) -> &str {
        &self.expected_raw
    }

    pub(crate) fn parts(&self) -> &[DialogueLinePart] {
        &self.parts
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DialogueLinePart {
    Literal(String),
    SpeakerSlot,
    BodyLine { source_line_index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectionModelError {
    EmptyScalarFieldKey,
    RecipeHasNoTextSlot,
    DuplicateProjectionSlot {
        role: TextUnitRole,
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

impl fmt::Display for ProjectionModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScalarFieldKey => formatter.write_str("标量字段键不能为空"),
            Self::RecipeHasNoTextSlot => {
                formatter.write_str("直接文本配方既没有文本槽，也不是完整冻结字面量")
            }
            Self::DuplicateProjectionSlot {
                role,
                source_line_index,
            } => write!(
                formatter,
                "直接文本配方重复引用槽 {role:?}/{source_line_index:?}"
            ),
            Self::MultipleBodyLinesInPhysicalLine => {
                formatter.write_str("一条对话物理行最多引用一个正文源行")
            }
            Self::DuplicateDialogueBodyLine { source_line_index } => {
                write!(formatter, "对话配方重复引用正文源行 {source_line_index}")
            }
            Self::NonContiguousDialogueBodyLines { expected, actual } => write!(
                formatter,
                "对话正文源行索引不连续：期待 {expected}，实际 {actual}"
            ),
            Self::MixedDirectAndInlineSpeaker => {
                formatter.write_str("同一对话不能同时使用直接 Speaker 和内嵌 SpeakerSlot")
            }
        }
    }
}

impl Error for ProjectionModelError {}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn dialogue_recipe_keeps_blank_lines_as_body_source_slots() {
        let recipe = DialogueWriteRecipe::new(
            location(0),
            None,
            vec![
                DialogueLineRecipe::new(
                    location(1),
                    "正文",
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 0,
                    }],
                )
                .expect("正文行应合法"),
                DialogueLineRecipe::new(
                    location(2),
                    "",
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 1,
                    }],
                )
                .expect("空白正文行应保留"),
            ],
        )
        .expect("含空白正文行的配方应合法");

        assert_eq!(recipe.lines().len(), 2);
    }

    #[test]
    fn dialogue_recipe_rejects_duplicate_or_gapped_source_line_indexes() {
        let duplicate = vec![
            DialogueLineRecipe::new(
                location(1),
                "一",
                vec![DialogueLinePart::BodyLine {
                    source_line_index: 0,
                }],
            )
            .expect("单行应合法"),
            DialogueLineRecipe::new(
                location(2),
                "二",
                vec![DialogueLinePart::BodyLine {
                    source_line_index: 0,
                }],
            )
            .expect("单行应合法"),
        ];
        assert!(matches!(
            DialogueWriteRecipe::new(location(0), None, duplicate),
            Err(ProjectionModelError::DuplicateDialogueBodyLine {
                source_line_index: 0
            })
        ));

        let gap = vec![
            DialogueLineRecipe::new(
                location(1),
                "一",
                vec![DialogueLinePart::BodyLine {
                    source_line_index: 1,
                }],
            )
            .expect("单行应合法"),
        ];
        assert!(matches!(
            DialogueWriteRecipe::new(location(0), None, gap),
            Err(ProjectionModelError::NonContiguousDialogueBodyLines {
                expected: 0,
                actual: 1
            })
        ));
    }

    #[test]
    fn content_serializes_as_plain_json_value_or_array() {
        assert_eq!(
            serde_json::to_string(&TextUnitContent::Value("姓名".to_owned()))
                .expect("单值应可序列化"),
            r#""姓名""#
        );
        assert_eq!(
            serde_json::to_string(&TextUnitContent::Lines(vec![
                "第一行".to_owned(),
                String::new(),
            ]))
            .expect("行集合应可序列化"),
            r#"["第一行",""]"#
        );
    }
}
