//! 逻辑文本身份、物理修改目标与物化写回配方。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

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

/// 翻译叶在语义组中的强类型角色。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum TextFieldRole {
    Scalar(ScalarFieldKey),
    DialogueSpeaker,
    DialogueBody { index: usize },
    ScrollingTextBody { index: usize },
}

/// 译文叶的权威身份，不等同于物理 JSON 地址。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LogicalTextLocation {
    group_location: RpgMakerLocation,
    role: TextFieldRole,
}

impl LogicalTextLocation {
    pub(crate) fn new(group_location: RpgMakerLocation, role: TextFieldRole) -> Self {
        Self {
            group_location,
            role,
        }
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn role(&self) -> &TextFieldRole {
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

    pub(crate) fn referenced_roles(&self) -> BTreeSet<TextFieldRole> {
        match self {
            Self::Direct(recipe) => recipe
                .parts()
                .iter()
                .filter_map(|part| match part {
                    DirectTextPart::Literal(_) => None,
                    DirectTextPart::TextSlot { role } => Some(role.clone()),
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
                    roles.insert(TextFieldRole::DialogueSpeaker);
                }
                for line in recipe.lines() {
                    for part in line.parts() {
                        if let DialogueLinePart::BodySlot { index } = part {
                            roles.insert(TextFieldRole::DialogueBody { index: *index });
                        }
                    }
                }
                roles
            }
        }
    }
}

/// 整字段或局部正则文本的可逆配方。
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
        let has_text_slot = parts
            .iter()
            .any(|part| matches!(part, DirectTextPart::TextSlot { .. }));
        let is_frozen_literal =
            matches!(parts.as_slice(), [DirectTextPart::Literal(value)] if value == &expected_raw);
        if parts.is_empty() || (!has_text_slot && !is_frozen_literal) {
            return Err(ProjectionModelError::RecipeHasNoTextSlot);
        }
        let mut roles = BTreeSet::new();
        for part in &parts {
            if let DirectTextPart::TextSlot { role } = part
                && !roles.insert(role.clone())
            {
                return Err(ProjectionModelError::DuplicateRole { role: role.clone() });
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
    TextSlot { role: TextFieldRole },
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
        let mut body_indexes = BTreeSet::new();
        let mut has_speaker_slot = false;
        for line in &lines {
            for part in line.parts() {
                match part {
                    DialogueLinePart::SpeakerSlot => has_speaker_slot = true,
                    DialogueLinePart::BodySlot { index } if !body_indexes.insert(*index) => {
                        return Err(ProjectionModelError::DuplicateDialogueBody { index: *index });
                    }
                    DialogueLinePart::Literal(_) | DialogueLinePart::BodySlot { .. } => {}
                }
            }
        }
        if direct_speaker.is_some() && has_speaker_slot {
            return Err(ProjectionModelError::MixedDirectAndInlineSpeaker);
        }
        for (expected, actual) in body_indexes.iter().copied().enumerate() {
            if expected != actual {
                return Err(ProjectionModelError::NonContiguousDialogueBody { expected, actual });
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
        let body_slots = parts
            .iter()
            .filter(|part| matches!(part, DialogueLinePart::BodySlot { .. }))
            .count();
        if body_slots > 1 {
            return Err(ProjectionModelError::MultipleBodySlotsInLine);
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
    BodySlot { index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectionModelError {
    EmptyScalarFieldKey,
    RecipeHasNoTextSlot,
    DuplicateRole { role: TextFieldRole },
    MultipleBodySlotsInLine,
    DuplicateDialogueBody { index: usize },
    NonContiguousDialogueBody { expected: usize, actual: usize },
    MixedDirectAndInlineSpeaker,
}

impl fmt::Display for ProjectionModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScalarFieldKey => formatter.write_str("标量字段键不能为空"),
            Self::RecipeHasNoTextSlot => {
                formatter.write_str("直接文本配方既没有文本槽，也不是完整冻结字面量")
            }
            Self::DuplicateRole { role } => write!(formatter, "直接文本配方重复引用角色 {role:?}"),
            Self::MultipleBodySlotsInLine => formatter.write_str("一条对话物理行最多引用一个 Body"),
            Self::DuplicateDialogueBody { index } => {
                write!(formatter, "对话配方重复引用 Body({index})")
            }
            Self::NonContiguousDialogueBody { expected, actual } => write!(
                formatter,
                "对话 Body 索引不连续：期待 {expected}，实际 {actual}"
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
    fn dialogue_recipe_keeps_blank_physical_lines_without_creating_body_leaves() {
        let recipe = DialogueWriteRecipe::new(
            location(0),
            None,
            vec![
                DialogueLineRecipe::new(
                    location(1),
                    "正文",
                    vec![DialogueLinePart::BodySlot { index: 0 }],
                )
                .expect("正文行应合法"),
                DialogueLineRecipe::new(
                    location(2),
                    "",
                    vec![DialogueLinePart::Literal(String::new())],
                )
                .expect("空白物理行应保留"),
            ],
        )
        .expect("含空白物理行的配方应合法");

        assert_eq!(recipe.lines().len(), 2);
    }

    #[test]
    fn dialogue_recipe_rejects_duplicate_or_gapped_body_indexes() {
        let duplicate = vec![
            DialogueLineRecipe::new(
                location(1),
                "一",
                vec![DialogueLinePart::BodySlot { index: 0 }],
            )
            .expect("单行应合法"),
            DialogueLineRecipe::new(
                location(2),
                "二",
                vec![DialogueLinePart::BodySlot { index: 0 }],
            )
            .expect("单行应合法"),
        ];
        assert!(matches!(
            DialogueWriteRecipe::new(location(0), None, duplicate),
            Err(ProjectionModelError::DuplicateDialogueBody { index: 0 })
        ));

        let gap = vec![
            DialogueLineRecipe::new(
                location(1),
                "一",
                vec![DialogueLinePart::BodySlot { index: 1 }],
            )
            .expect("单行应合法"),
        ];
        assert!(matches!(
            DialogueWriteRecipe::new(location(0), None, gap),
            Err(ProjectionModelError::NonContiguousDialogueBody {
                expected: 0,
                actual: 1
            })
        ));
    }
}
