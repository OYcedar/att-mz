//! 从 RPG Maker 标准文本资产规划并生成写回候选。

pub(crate) mod layout;

pub(crate) use layout::ConservativeRpgMakerWriteBackTextLayouter;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{StandardWriteBack, StandardWriteBackSummary};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueWriteRecipe, DirectTextPart, DirectTextRecipe, LogicalTextLocation,
    MutationTarget, TextProjectionRecipe, TextUnitContent, TextUnitRole,
};
use crate::rpg_maker::project::{MaxFullwidthChars, OpenedProject, RpgMakerWriteBackLayoutProfile};
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
};

/// 一个可独立拥有译文、验收并原子写回的语义文本单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackUnit {
    role: TextUnitRole,
    source_content: TextUnitContent,
    translation_content: Option<TextUnitContent>,
}

impl StandardWriteBackUnit {
    pub(crate) fn new(
        role: TextUnitRole,
        source_content: TextUnitContent,
        translation_content: Option<TextUnitContent>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        validate_content_shape(&role, &source_content)?;
        validate_content_lines(&role, &source_content, "原文")?;
        if source_content.is_blank() {
            return Err(StandardWriteBackSnapshotError::BlankSourceContent { role });
        }
        if let Some(translation) = &translation_content {
            validate_content_shape(&role, translation)?;
            validate_content_lines(&role, translation, "译文")?;
            if translation.is_blank() {
                return Err(StandardWriteBackSnapshotError::BlankTranslationContent { role });
            }
            if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
                let source_lines = source_content
                    .as_lines()
                    .expect("严格对齐角色的原文必须是行序列");
                let translated_lines = translation
                    .as_lines()
                    .expect("严格对齐角色的译文必须是行序列");
                if source_lines.len() != translated_lines.len() {
                    return Err(StandardWriteBackSnapshotError::AlignedLineCountMismatch {
                        role,
                        expected: source_lines.len(),
                        actual: translated_lines.len(),
                    });
                }
                for (line_index, (source, translated)) in
                    source_lines.iter().zip(translated_lines).enumerate()
                {
                    let source_is_blank = source.trim().is_empty();
                    if (source_is_blank && !translated.is_empty())
                        || (!source_is_blank && translated.trim().is_empty())
                    {
                        return Err(StandardWriteBackSnapshotError::AlignedBlankLineMismatch {
                            role,
                            line_index,
                        });
                    }
                }
            }
        }
        Ok(Self {
            role,
            source_content,
            translation_content,
        })
    }

    fn effective_content(&self) -> &TextUnitContent {
        self.translation_content
            .as_ref()
            .unwrap_or(&self.source_content)
    }
}

fn aligned_replacement_lines(unit: &StandardWriteBackUnit) -> Option<Vec<String>> {
    let translated = unit.translation_content.as_ref()?.as_lines()?;
    let source = unit
        .source_content
        .as_lines()
        .expect("严格对齐单元的原文必须是行序列");
    Some(
        source
            .iter()
            .zip(translated)
            .map(|(source, translated)| {
                if source.trim().is_empty() {
                    source.clone()
                } else {
                    translated.clone()
                }
            })
            .collect(),
    )
}

fn validate_content_shape(
    role: &TextUnitRole,
    content: &TextUnitContent,
) -> Result<(), StandardWriteBackSnapshotError> {
    if role.expects_lines() != matches!(content, TextUnitContent::Lines(_)) {
        return Err(StandardWriteBackSnapshotError::ContentShapeMismatch { role: role.clone() });
    }
    Ok(())
}

fn validate_content_lines(
    role: &TextUnitRole,
    content: &TextUnitContent,
    column: &'static str,
) -> Result<(), StandardWriteBackSnapshotError> {
    match content {
        TextUnitContent::Value(value) => {
            if value.contains('\0') {
                return Err(StandardWriteBackSnapshotError::InvalidContentLine {
                    role: role.clone(),
                    column,
                    line_index: 0,
                });
            }
            if matches!(role, TextUnitRole::DialogueSpeaker)
                && (value.contains('\r') || value.contains('\n'))
            {
                return Err(StandardWriteBackSnapshotError::InvalidContentLine {
                    role: role.clone(),
                    column,
                    line_index: 0,
                });
            }
        }
        TextUnitContent::Lines(lines) => {
            if lines.is_empty() {
                return Err(StandardWriteBackSnapshotError::EmptyLineContent {
                    role: role.clone(),
                    column,
                });
            }
            if let Some(line_index) = lines.iter().position(|line| {
                line.chars()
                    .any(|value| matches!(value, '\r' | '\n' | '\0'))
            }) {
                return Err(StandardWriteBackSnapshotError::InvalidContentLine {
                    role: role.clone(),
                    column,
                    line_index,
                });
            }
        }
    }
    Ok(())
}

/// 一组语义单元及其已经物化的物理写回配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<StandardWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    mutation_targets: Vec<MutationTarget>,
}

impl StandardWriteBackGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        mut units: Vec<StandardWriteBackUnit>,
        recipes: Vec<TextProjectionRecipe>,
        mut mutation_targets: Vec<MutationTarget>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        if recipes.is_empty() {
            return Err(StandardWriteBackSnapshotError::EmptyProjection {
                group_location: Box::new(group_location),
            });
        }
        for unit in &units {
            if !role_matches_kind(kind, &unit.role) {
                return Err(StandardWriteBackSnapshotError::InvalidRole {
                    kind,
                    role: unit.role.clone(),
                });
            }
        }
        units.sort_by(|left, right| left.role.cmp(&right.role));
        for pair in units.windows(2) {
            if pair[0].role == pair[1].role {
                return Err(StandardWriteBackSnapshotError::DuplicateRole {
                    group_location: Box::new(group_location),
                    role: pair[0].role.clone(),
                });
            }
        }

        let unit_roles = units
            .iter()
            .map(|unit| unit.role.clone())
            .collect::<BTreeSet<_>>();
        let recipe_roles = recipes
            .iter()
            .flat_map(TextProjectionRecipe::referenced_roles)
            .collect::<BTreeSet<_>>();
        if unit_roles != recipe_roles {
            return Err(StandardWriteBackSnapshotError::RecipeRoleMismatch {
                group_location: Box::new(group_location),
                units: unit_roles,
                recipes: recipe_roles,
            });
        }
        validate_line_references(&group_location, &units, &recipes)?;
        validate_projection_round_trip(&group_location, &units, &recipes)?;

        let mut expected_targets = recipes
            .iter()
            .flat_map(TextProjectionRecipe::mutation_targets)
            .collect::<Vec<_>>();
        expected_targets.sort();
        mutation_targets.sort();
        if expected_targets != mutation_targets {
            return Err(StandardWriteBackSnapshotError::RecipeTargetMismatch {
                group_location: Box::new(group_location),
            });
        }
        for pair in mutation_targets.windows(2) {
            if pair[0] == pair[1] {
                return Err(StandardWriteBackSnapshotError::DuplicateTarget {
                    target: Box::new(pair[0].clone()),
                });
            }
        }
        for target in &mutation_targets {
            let target_location = match target {
                MutationTarget::Value(location) => location,
                MutationTarget::DialogueBlock { header } => header,
            };
            if target_location.source() != group_location.source() {
                return Err(StandardWriteBackSnapshotError::MismatchedTargetSource {
                    group_location: Box::new(group_location),
                    target: Box::new(target.clone()),
                });
            }
        }

        match kind {
            TextGroupKind::EventDialogue => {
                if recipes.len() != 1 || !matches!(recipes[0], TextProjectionRecipe::Dialogue(_)) {
                    return Err(StandardWriteBackSnapshotError::InvalidDialogueProjection {
                        group_location: Box::new(group_location),
                    });
                }
                let TextProjectionRecipe::Dialogue(recipe) = &recipes[0] else {
                    unreachable!()
                };
                if recipe.group_location() != &group_location {
                    return Err(StandardWriteBackSnapshotError::MismatchedDialogueGroup {
                        group_location: Box::new(group_location),
                        recipe_location: Box::new(recipe.group_location().clone()),
                    });
                }
            }
            TextGroupKind::EventScrollingText => {
                if recipes
                    .iter()
                    .any(|recipe| !matches!(recipe, TextProjectionRecipe::Direct(_)))
                {
                    return Err(StandardWriteBackSnapshotError::InvalidScrollingProjection {
                        group_location: Box::new(group_location),
                    });
                }
                validate_scrolling_projection(&group_location, &units, &recipes)?;
            }
            TextGroupKind::EventChoices => {
                if recipes
                    .iter()
                    .any(|recipe| !matches!(recipe, TextProjectionRecipe::Direct(_)))
                {
                    return Err(StandardWriteBackSnapshotError::InvalidChoicesProjection {
                        group_location: Box::new(group_location),
                    });
                }
            }
            _ => {
                if recipes
                    .iter()
                    .any(|recipe| !matches!(recipe, TextProjectionRecipe::Direct(_)))
                {
                    return Err(StandardWriteBackSnapshotError::InvalidDirectProjection {
                        group_location: Box::new(group_location),
                    });
                }
            }
        }

        Ok(Self {
            kind,
            group_location,
            units,
            recipes,
            mutation_targets,
        })
    }

    fn into_parts(
        self,
    ) -> (
        TextGroupKind,
        RpgMakerLocation,
        Vec<StandardWriteBackUnit>,
        Vec<TextProjectionRecipe>,
    ) {
        (self.kind, self.group_location, self.units, self.recipes)
    }
}

fn validate_line_references(
    group_location: &RpgMakerLocation,
    units: &[StandardWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), StandardWriteBackSnapshotError> {
    let mut referenced = BTreeMap::<TextUnitRole, BTreeMap<usize, usize>>::new();
    for (role, source_line_index) in recipes
        .iter()
        .flat_map(TextProjectionRecipe::referenced_lines)
    {
        *referenced
            .entry(role)
            .or_default()
            .entry(source_line_index)
            .or_default() += 1;
    }
    for unit in units {
        let actual = referenced.remove(&unit.role).unwrap_or_default();
        let Some(lines) = unit.source_content.as_lines() else {
            if !actual.is_empty() {
                return Err(StandardWriteBackSnapshotError::RecipeLineMismatch {
                    group_location: Box::new(group_location.clone()),
                    role: unit.role.clone(),
                });
            }
            continue;
        };
        let expected_uses = if matches!(unit.role, TextUnitRole::Choices) {
            2
        } else {
            1
        };
        if actual.len() != lines.len()
            || (0..lines.len()).any(|index| actual.get(&index) != Some(&expected_uses))
        {
            return Err(StandardWriteBackSnapshotError::RecipeLineMismatch {
                group_location: Box::new(group_location.clone()),
                role: unit.role.clone(),
            });
        }
    }
    debug_assert!(referenced.is_empty(), "角色集合已经在调用前验证一致");
    Ok(())
}

fn validate_projection_round_trip(
    group_location: &RpgMakerLocation,
    units: &[StandardWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), StandardWriteBackSnapshotError> {
    let units = units
        .iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    for recipe in recipes {
        match recipe {
            TextProjectionRecipe::Direct(recipe) => {
                let mut rebuilt = String::new();
                for part in recipe.parts() {
                    match part {
                        DirectTextPart::Literal(value) => rebuilt.push_str(value),
                        DirectTextPart::TextSlot { role } => rebuilt.push_str(
                            units
                                .get(role)
                                .and_then(|unit| unit.source_content.as_value())
                                .ok_or_else(|| {
                                    StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                        group_location: Box::new(group_location.clone()),
                                        target: Box::new(recipe.target().clone()),
                                    }
                                })?,
                        ),
                        DirectTextPart::LineSlot {
                            role,
                            source_line_index,
                        } => {
                            let line = units
                                .get(role)
                                .and_then(|unit| unit.source_content.as_lines())
                                .and_then(|lines| lines.get(*source_line_index))
                                .ok_or_else(|| {
                                    StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                        group_location: Box::new(group_location.clone()),
                                        target: Box::new(recipe.target().clone()),
                                    }
                                })?;
                            rebuilt.push_str(line);
                        }
                    }
                }
                if rebuilt != recipe.expected_raw() {
                    return Err(
                        StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                            group_location: Box::new(group_location.clone()),
                            target: Box::new(recipe.target().clone()),
                        },
                    );
                }
            }
            TextProjectionRecipe::Dialogue(recipe) => {
                let speaker = units
                    .get(&TextUnitRole::DialogueSpeaker)
                    .and_then(|unit| unit.source_content.as_value());
                if let Some(target) = recipe.direct_speaker()
                    && speaker != Some(target.expected_raw())
                {
                    return Err(
                        StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                            group_location: Box::new(group_location.clone()),
                            target: Box::new(target.physical_location().clone()),
                        },
                    );
                }
                for line in recipe.lines() {
                    let mut rebuilt = String::new();
                    for (part_index, part) in line.parts().iter().enumerate() {
                        match part {
                            DialogueLinePart::Literal(value) => rebuilt.push_str(value),
                            DialogueLinePart::SpeakerSlot => rebuilt
                                .push_str(speaker.expect(
                                    "调用前已经确认内嵌 SpeakerSlot 对应逻辑 Speaker 单元",
                                )),
                            DialogueLinePart::BodyLine { source_line_index } => {
                                if part_index + 1 != line.parts().len() {
                                    return Err(
                                        StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(line.physical_location().clone()),
                                        },
                                    );
                                }
                                rebuilt.push_str(
                                    units
                                        .get(&TextUnitRole::DialogueBody)
                                        .and_then(|unit| unit.source_content.as_lines())
                                        .and_then(|lines| lines.get(*source_line_index))
                                        .ok_or_else(|| StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(line.physical_location().clone()),
                                        })?,
                                );
                            }
                        }
                    }
                    if rebuilt != line.expected_raw() {
                        return Err(
                            StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                group_location: Box::new(group_location.clone()),
                                target: Box::new(line.physical_location().clone()),
                            },
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_scrolling_projection(
    group_location: &RpgMakerLocation,
    units: &[StandardWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), StandardWriteBackSnapshotError> {
    let lines = units
        .iter()
        .find(|unit| unit.role == TextUnitRole::ScrollingText)
        .and_then(|unit| unit.source_content.as_lines())
        .expect("受信滚动文本必须包含行序列单元");
    for (physical_index, recipe) in recipes.iter().enumerate() {
        let TextProjectionRecipe::Direct(recipe) = recipe else {
            unreachable!("调用前已经验证滚动文本只包含直接配方")
        };
        match recipe.parts() {
            [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::ScrollingText,
                    source_line_index,
                },
            ] if *source_line_index == physical_index
                && lines
                    .get(physical_index)
                    .is_some_and(|line| line == recipe.expected_raw()) => {}
            _ => {
                return Err(StandardWriteBackSnapshotError::InvalidScrollingRecipe {
                    group_location: Box::new(group_location.clone()),
                });
            }
        }
    }
    Ok(())
}

/// Reader 在同一个一致读视图中建立的完整 Standard 写回快照。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StandardWriteBackSnapshot {
    groups: Vec<StandardWriteBackGroup>,
}

impl StandardWriteBackSnapshot {
    pub(crate) fn new(
        mut groups: Vec<StandardWriteBackGroup>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        groups.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.group_location.cmp(&right.group_location))
        });

        let mut mutation_targets = BTreeSet::new();
        for group in &groups {
            for target in &group.mutation_targets {
                if !mutation_targets.insert(target.clone()) {
                    return Err(StandardWriteBackSnapshotError::DuplicateTarget {
                        target: Box::new(target.clone()),
                    });
                }
            }
        }
        Ok(Self { groups })
    }

    fn into_groups(self) -> Vec<StandardWriteBackGroup> {
        self.groups
    }
}

fn role_matches_kind(kind: TextGroupKind, role: &TextUnitRole) -> bool {
    match kind {
        TextGroupKind::EventDialogue => matches!(
            role,
            TextUnitRole::DialogueSpeaker | TextUnitRole::DialogueBody
        ),
        TextGroupKind::EventScrollingText => {
            matches!(role, TextUnitRole::ScrollingText)
        }
        TextGroupKind::EventChoices => matches!(role, TextUnitRole::Choices),
        _ => matches!(role, TextUnitRole::Scalar(_)),
    }
}

/// Reader 交回受信快照前必须排除的数据损坏。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackSnapshotError {
    BlankSourceContent {
        role: TextUnitRole,
    },
    BlankTranslationContent {
        role: TextUnitRole,
    },
    ContentShapeMismatch {
        role: TextUnitRole,
    },
    EmptyLineContent {
        role: TextUnitRole,
        column: &'static str,
    },
    InvalidContentLine {
        role: TextUnitRole,
        column: &'static str,
        line_index: usize,
    },
    AlignedLineCountMismatch {
        role: TextUnitRole,
        expected: usize,
        actual: usize,
    },
    AlignedBlankLineMismatch {
        role: TextUnitRole,
        line_index: usize,
    },
    EmptyProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidRole {
        kind: TextGroupKind,
        role: TextUnitRole,
    },
    DuplicateRole {
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
    RecipeRoleMismatch {
        group_location: Box<RpgMakerLocation>,
        units: BTreeSet<TextUnitRole>,
        recipes: BTreeSet<TextUnitRole>,
    },
    RecipeLineMismatch {
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
    RecipeTargetMismatch {
        group_location: Box<RpgMakerLocation>,
    },
    RecipeDoesNotRebuildOriginal {
        group_location: Box<RpgMakerLocation>,
        target: Box<RpgMakerLocation>,
    },
    DuplicateTarget {
        target: Box<MutationTarget>,
    },
    MismatchedTargetSource {
        group_location: Box<RpgMakerLocation>,
        target: Box<MutationTarget>,
    },
    InvalidDialogueProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidScrollingProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidScrollingRecipe {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidChoicesProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidDirectProjection {
        group_location: Box<RpgMakerLocation>,
    },
    MismatchedDialogueGroup {
        group_location: Box<RpgMakerLocation>,
        recipe_location: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for StandardWriteBackSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankSourceContent { role } => {
                write!(formatter, "写回资产原文仅包含空白：{role:?}")
            }
            Self::BlankTranslationContent { role } => {
                write!(formatter, "写回资产译文仅包含空白：{role:?}")
            }
            Self::ContentShapeMismatch { role } => {
                write!(formatter, "写回资产内容形状与角色不一致：{role:?}")
            }
            Self::EmptyLineContent { role, column } => {
                write!(formatter, "写回资产{column}行序列为空：{role:?}")
            }
            Self::InvalidContentLine {
                role,
                column,
                line_index,
            } => write!(
                formatter,
                "写回资产{column}第 {line_index} 行包含不允许的控制字符：{role:?}"
            ),
            Self::AlignedLineCountMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "严格对齐译文行数不一致：{role:?}，期待 {expected}，实际 {actual}"
            ),
            Self::AlignedBlankLineMismatch { role, line_index } => write!(
                formatter,
                "严格对齐译文第 {line_index} 行的空白状态与原文不一致：{role:?}"
            ),
            Self::EmptyProjection { group_location } => {
                write!(formatter, "写回资产组不包含投影配方：{group_location}")
            }
            Self::InvalidRole { kind, role } => {
                write!(formatter, "写回资产角色与组类型 {kind:?} 不一致：{role:?}")
            }
            Self::DuplicateRole {
                group_location,
                role,
            } => write!(
                formatter,
                "写回资产组 {group_location} 重复逻辑角色 {role:?}"
            ),
            Self::RecipeRoleMismatch {
                group_location,
                units,
                recipes,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的单元角色与配方角色不一致：{units:?} / {recipes:?}"
            ),
            Self::RecipeLineMismatch {
                group_location,
                role,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的行槽索引或引用次数无效：{role:?}"
            ),
            Self::RecipeTargetMismatch { group_location } => {
                write!(
                    formatter,
                    "写回资产组 {group_location} 的物理目标与配方不一致"
                )
            }
            Self::RecipeDoesNotRebuildOriginal {
                group_location,
                target,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的投影配方无法逐字重建冻结原文：{target}"
            ),
            Self::DuplicateTarget { target } => {
                write!(formatter, "写回快照包含重复物理目标：{target:?}")
            }
            Self::MismatchedTargetSource {
                group_location,
                target,
            } => write!(
                formatter,
                "写回资产组与物理目标不属于同一来源：{group_location} / {target:?}"
            ),
            Self::InvalidDialogueProjection { group_location } => write!(
                formatter,
                "对话组必须且只能包含一个对话配方：{group_location}"
            ),
            Self::InvalidScrollingProjection { group_location } => {
                write!(formatter, "滚动文本组只能包含直接配方：{group_location}")
            }
            Self::InvalidScrollingRecipe { group_location } => write!(
                formatter,
                "滚动文本组的语义行索引或直接配方无效：{group_location}"
            ),
            Self::InvalidChoicesProjection { group_location } => {
                write!(formatter, "选项组只能包含直接配方：{group_location}")
            }
            Self::InvalidDirectProjection { group_location } => {
                write!(formatter, "普通文本组只能包含直接配方：{group_location}")
            }
            Self::MismatchedDialogueGroup {
                group_location,
                recipe_location,
            } => write!(
                formatter,
                "对话组位置与配方位置不一致：{group_location} / {recipe_location}"
            ),
        }
    }
}

impl Error for StandardWriteBackSnapshotError {}

/// 在读取标准文本组三表前确认所有 active owner 仍属于当前冻结来源。
///
/// 实现不得读取或校验术语依赖；术语数据不是 WriteBack 的输入。
pub(crate) trait StandardWriteBackAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<StandardWriteBackSnapshot, Self::Error>> + Send + use<Self>;
}

/// 当前允许自动布局的 RPG Maker 显示区域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutRegion {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

/// 共享布局内核中的一个原文/当前文本对。
///
/// `replacement == None` 表示该项仍使用冻结原文：它参与跨项括号与缩进状态观察，
/// 但布局结果不得修改它。Lua 对自己提供的当前文本使用 `Some`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerLayoutTextPair {
    original_text: String,
    replacement: Option<String>,
}

impl RpgMakerLayoutTextPair {
    pub(crate) fn new(original_text: String, replacement: Option<String>) -> Self {
        Self {
            original_text,
            replacement,
        }
    }

    pub(crate) fn replacement(&self) -> Option<&str> {
        self.replacement.as_deref()
    }

    fn effective_text(&self) -> &str {
        self.replacement.as_deref().unwrap_or(&self.original_text)
    }
}

/// 共享布局内核成功后与输入逐项对齐的文本及新增内容计数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerAppliedTextLayout {
    texts: Vec<String>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

impl RpgMakerAppliedTextLayout {
    pub(super) fn new(
        texts: Vec<String>,
        inserted_line_breaks: usize,
        inserted_fullwidth_indents: usize,
    ) -> Self {
        Self {
            texts,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<String>, usize, usize) {
        (
            self.texts,
            self.inserted_line_breaks,
            self.inserted_fullwidth_indents,
        )
    }

    #[cfg(test)]
    pub(crate) fn texts(&self) -> &[String] {
        &self.texts
    }

    #[cfg(test)]
    pub(crate) const fn inserted_line_breaks(&self) -> usize {
        self.inserted_line_breaks
    }

    #[cfg(test)]
    pub(crate) const fn inserted_fullwidth_indents(&self) -> usize {
        self.inserted_fullwidth_indents
    }
}

/// 共享纯布局内核的正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerTextLayoutOutcome {
    Applied(RpgMakerAppliedTextLayout),
    /// 无法保证阅读质量；文本仍与输入逐项对齐，且不含程序新增内容。
    Manual(RpgMakerAppliedTextLayout),
}

/// 一个布局段当前写回内容的权威来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutCandidate {
    /// 数据库没有译文，必须保持冻结原命令或原字段不变。
    FrozenOriginal,
    /// 数据库明确存在译文，允许布局器调整显示行。
    DatabaseTranslation(String),
}

/// 布局请求中一个仍与数据库语义单元保持对应关系的内容段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLayoutSegment {
    logical_location: Option<LogicalTextLocation>,
    exact_location: RpgMakerLocation,
    original_text: String,
    candidate: RpgMakerWriteBackLayoutCandidate,
}

impl RpgMakerWriteBackLayoutSegment {
    fn from_unit_at(
        group_location: &RpgMakerLocation,
        unit: &StandardWriteBackUnit,
        exact_location: RpgMakerLocation,
    ) -> Self {
        let candidate = unit.translation_content.as_ref().map_or(
            RpgMakerWriteBackLayoutCandidate::FrozenOriginal,
            |content| RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(content_text(content)),
        );
        Self {
            logical_location: Some(LogicalTextLocation::new(
                group_location.clone(),
                unit.role.clone(),
            )),
            exact_location,
            original_text: content_text(&unit.source_content),
            candidate,
        }
    }

    fn from_line_at(
        group_location: &RpgMakerLocation,
        role: TextUnitRole,
        exact_location: RpgMakerLocation,
        original_text: String,
        translation: Option<String>,
    ) -> Self {
        Self {
            logical_location: Some(LogicalTextLocation::new(group_location.clone(), role)),
            exact_location,
            original_text,
            candidate: translation.map_or(
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation,
            ),
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn candidate(&self) -> &RpgMakerWriteBackLayoutCandidate {
        &self.candidate
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }
}

fn content_text(content: &TextUnitContent) -> String {
    match content {
        TextUnitContent::Value(value) => value.clone(),
        TextUnitContent::Lines(lines) => lines.join("\n"),
    }
}

/// Standard 为一个完整布局单元建立的显式请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLayoutRequest {
    region: RpgMakerWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
    segments: Vec<RpgMakerWriteBackLayoutSegment>,
}

impl RpgMakerWriteBackLayoutRequest {
    pub(crate) fn new(
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
        segments: Vec<RpgMakerWriteBackLayoutSegment>,
    ) -> Self {
        debug_assert!(!segments.is_empty(), "布局单元必须包含至少一个文本段");
        debug_assert!(
            segments.iter().any(|segment| matches!(
                segment.candidate,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_)
            )),
            "没有数据库译文的单元不应请求布局"
        );
        Self {
            region,
            max_fullwidth_chars,
            segments,
        }
    }

    pub(crate) const fn max_fullwidth_chars(&self) -> MaxFullwidthChars {
        self.max_fullwidth_chars
    }

    pub(crate) fn segments(&self) -> &[RpgMakerWriteBackLayoutSegment] {
        &self.segments
    }
}

/// 布局器产生的一条最终显示行及其所属译文语义行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLaidOutLine {
    text: String,
    source_semantic_line_index: usize,
}

impl RpgMakerWriteBackLaidOutLine {
    pub(crate) fn new(text: String, source_semantic_line_index: usize) -> Self {
        Self {
            text,
            source_semantic_line_index,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn source_semantic_line_index(&self) -> usize {
        self.source_semantic_line_index
    }
}

/// 布局器为一个数据库译文单元产生的最终显示行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLaidOutSegment {
    exact_location: RpgMakerLocation,
    lines: Vec<RpgMakerWriteBackLaidOutLine>,
}

impl RpgMakerWriteBackLaidOutSegment {
    pub(crate) fn new(
        exact_location: RpgMakerLocation,
        lines: Vec<RpgMakerWriteBackLaidOutLine>,
    ) -> Result<Self, RpgMakerWriteBackAppliedLayoutError> {
        if lines.is_empty() {
            return Err(RpgMakerWriteBackAppliedLayoutError::EmptyReplacement {
                exact_location: Box::new(exact_location),
            });
        }
        if let Some(line_index) = lines.iter().position(|line| line.text.contains('\n')) {
            return Err(RpgMakerWriteBackAppliedLayoutError::EmbeddedLineBreak {
                exact_location: Box::new(exact_location),
                line_index,
            });
        }
        Ok(Self {
            exact_location,
            lines,
        })
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[RpgMakerWriteBackLaidOutLine] {
        &self.lines
    }
}

/// 一次已经通过请求对应性校验的布局成功结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackAppliedLayout {
    segments: Vec<RpgMakerWriteBackLaidOutSegment>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

impl RpgMakerWriteBackAppliedLayout {
    pub(crate) fn new(
        request: &RpgMakerWriteBackLayoutRequest,
        segments: Vec<RpgMakerWriteBackLaidOutSegment>,
        inserted_line_breaks: usize,
        inserted_fullwidth_indents: usize,
    ) -> Result<Self, RpgMakerWriteBackAppliedLayoutError> {
        let mut replacements = BTreeMap::new();
        for segment in segments {
            let location = segment.exact_location.clone();
            if replacements.insert(location.clone(), segment).is_some() {
                return Err(RpgMakerWriteBackAppliedLayoutError::DuplicateReplacement {
                    exact_location: Box::new(location),
                });
            }
        }

        let mut ordered = Vec::new();
        for request_segment in &request.segments {
            match request_segment.candidate {
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal => {
                    if replacements.contains_key(&request_segment.exact_location) {
                        return Err(RpgMakerWriteBackAppliedLayoutError::ChangesFrozenOriginal {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    }
                }
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_) => {
                    let Some(segment) = replacements.remove(&request_segment.exact_location) else {
                        return Err(RpgMakerWriteBackAppliedLayoutError::MissingReplacement {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    };
                    ordered.push(segment);
                }
            }
        }
        if let Some((exact_location, _)) = replacements.into_iter().next() {
            return Err(RpgMakerWriteBackAppliedLayoutError::UnexpectedReplacement {
                exact_location: Box::new(exact_location),
            });
        }

        Ok(Self {
            segments: ordered,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        })
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[RpgMakerWriteBackLaidOutSegment] {
        &self.segments
    }

    #[cfg(test)]
    pub(crate) const fn inserted_line_breaks(&self) -> usize {
        self.inserted_line_breaks
    }

    #[cfg(test)]
    pub(crate) const fn inserted_fullwidth_indents(&self) -> usize {
        self.inserted_fullwidth_indents
    }

    fn into_parts(self) -> (Vec<RpgMakerWriteBackLaidOutSegment>, usize, usize) {
        (
            self.segments,
            self.inserted_line_breaks,
            self.inserted_fullwidth_indents,
        )
    }
}

/// 布局器在构造 Applied 结果时违反请求边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackAppliedLayoutError {
    EmptyReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    EmbeddedLineBreak {
        exact_location: Box<RpgMakerLocation>,
        line_index: usize,
    },
    DuplicateReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    ChangesFrozenOriginal {
        exact_location: Box<RpgMakerLocation>,
    },
    MissingReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    UnexpectedReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for RpgMakerWriteBackAppliedLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyReplacement { exact_location } => {
                write!(formatter, "布局结果没有提供任何显示行：{exact_location}")
            }
            Self::EmbeddedLineBreak {
                exact_location,
                line_index,
            } => write!(
                formatter,
                "布局结果第 {line_index} 个显示行仍包含真实换行：{exact_location}"
            ),
            Self::DuplicateReplacement { exact_location } => {
                write!(formatter, "布局结果重复返回位置：{exact_location}")
            }
            Self::ChangesFrozenOriginal { exact_location } => {
                write!(formatter, "布局结果试图修改缺译原文：{exact_location}")
            }
            Self::MissingReplacement { exact_location } => {
                write!(formatter, "布局结果缺少数据库译文位置：{exact_location}")
            }
            Self::UnexpectedReplacement { exact_location } => {
                write!(formatter, "布局结果包含请求外位置：{exact_location}")
            }
        }
    }
}

impl Error for RpgMakerWriteBackAppliedLayoutError {}

/// 保守布局的正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutOutcome {
    Applied(RpgMakerWriteBackAppliedLayout),
    /// 无法保证阅读质量；调用方必须撤销整个单元的自动布局。
    Manual,
}

/// 对一个完整 RPG Maker 显示单元执行保守布局。
///
/// 本能力是同步纯业务计算，并且必须遵守以下交接约束：
///
/// - 请求已经显式给出区域与该区域的行宽，不得自行读取或选择整个布局 Profile；
/// - 数据库译文已有的真实换行是必须保留的硬边界，只对其中过宽的语义行新增自动换行；
/// - 段边界就是数据库语义单元的来源边界：可以跨段观察括号和缩进状态，但不得把字符
///   移动到其他段，也不得返回对 `FrozenOriginal` 的修改；
/// - 必须先决定自动换行，再为符合规则的续行补全角空格；
/// - `inserted_line_breaks` 与 `inserted_fullwidth_indents` 只统计本次自动新增内容，
///   不包含数据库硬换行、原 401/405 边界或原文已有空格。
///
/// 控制符不明确、没有安全断点或无法完整遵守上述规则时，必须对整个请求返回
/// `Manual`，不得升级为技术错误或强制切断文本。
pub(crate) trait RpgMakerWriteBackTextLayouter: Send + Sync {
    fn layout(&self, request: &RpgMakerWriteBackLayoutRequest) -> RpgMakerWriteBackLayoutOutcome;
}

/// 一次已经按物化直接配方完成渲染的单值替换。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetTextMutation {
    exact_location: RpgMakerLocation,
    expected_original: String,
    replacement: String,
}

impl SetTextMutation {
    #[cfg(test)]
    pub(crate) fn for_test(
        exact_location: RpgMakerLocation,
        expected_original: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            replacement: replacement.into(),
        }
    }

    fn from_recipe(recipe: &DirectTextRecipe, replacement: String) -> Self {
        Self {
            exact_location: recipe.target().clone(),
            expected_original: recipe.expected_raw().to_owned(),
            replacement,
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn replacement(&self) -> &str {
        &self.replacement
    }
}

/// 一个 `101 + 401*` 对话块的唯一原子修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceDialogueMutation {
    recipe: DialogueWriteRecipe,
    source_speaker: Option<String>,
    speaker: Option<String>,
    body_lines: Option<Vec<RpgMakerWriteBackLaidOutLine>>,
}

impl ReplaceDialogueMutation {
    pub(crate) fn new(
        recipe: DialogueWriteRecipe,
        source_speaker: Option<String>,
        speaker: Option<String>,
        body_lines: Option<Vec<RpgMakerWriteBackLaidOutLine>>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        let referenced = TextProjectionRecipe::Dialogue(recipe.clone()).referenced_roles();
        let expects_speaker = referenced.contains(&TextUnitRole::DialogueSpeaker);
        if expects_speaker != speaker.is_some() || expects_speaker != source_speaker.is_some() {
            return Err(StandardWriteBackMutationPlanError::InvalidDialogue {
                group_location: Box::new(recipe.group_location().clone()),
                message: "Speaker 槽与最终 Speaker 不一致",
            });
        }
        let expects_body = referenced.contains(&TextUnitRole::DialogueBody);
        if !expects_body && body_lines.is_some() {
            return Err(StandardWriteBackMutationPlanError::InvalidDialogue {
                group_location: Box::new(recipe.group_location().clone()),
                message: "没有 Body 槽的对话不能提供 Body 译文",
            });
        }
        if let Some(lines) = &body_lines {
            if lines.is_empty() {
                return Err(StandardWriteBackMutationPlanError::InvalidDialogue {
                    group_location: Box::new(recipe.group_location().clone()),
                    message: "Body 没有产生显示行",
                });
            }
            let semantic_indexes = lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::source_semantic_line_index)
                .collect::<Vec<_>>();
            if semantic_indexes.first() != Some(&0)
                || semantic_indexes.windows(2).any(|pair| pair[0] > pair[1])
                || semantic_indexes
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .iter()
                    .copied()
                    .enumerate()
                    .any(|(expected, actual)| expected != actual)
            {
                return Err(StandardWriteBackMutationPlanError::InvalidDialogue {
                    group_location: Box::new(recipe.group_location().clone()),
                    message: "Body 显示行的语义来源索引不连续",
                });
            }
        }
        Ok(Self {
            recipe,
            source_speaker,
            speaker,
            body_lines,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        self.recipe.group_location()
    }

    pub(crate) fn recipe(&self) -> &DialogueWriteRecipe {
        &self.recipe
    }

    pub(crate) fn speaker(&self) -> Option<&str> {
        self.speaker.as_deref()
    }

    pub(crate) fn source_speaker(&self) -> Option<&str> {
        self.source_speaker.as_deref()
    }

    pub(crate) fn body_lines(&self) -> Option<&[RpgMakerWriteBackLaidOutLine]> {
        self.body_lines.as_deref()
    }

    fn mutation_targets(&self) -> Vec<MutationTarget> {
        TextProjectionRecipe::Dialogue(self.recipe.clone()).mutation_targets()
    }
}

/// 一个选项头及其同层分支标签的唯一原子修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceChoicesMutation {
    group_location: RpgMakerLocation,
    recipes: Vec<DirectTextRecipe>,
    source_lines: Vec<String>,
    replacement_lines: Vec<String>,
}

impl ReplaceChoicesMutation {
    pub(crate) fn new(
        group_location: RpgMakerLocation,
        recipes: Vec<DirectTextRecipe>,
        source_lines: Vec<String>,
        replacement_lines: Vec<String>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        if source_lines.is_empty() || source_lines.len() != replacement_lines.len() {
            return Err(StandardWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                message: "选项原文与译文必须是等长非空行序列",
            });
        }
        if source_lines
            .iter()
            .zip(&replacement_lines)
            .any(|(source, replacement)| source.trim().is_empty() && source != replacement)
        {
            return Err(StandardWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                message: "选项空白槽必须逐字保持冻结原文",
            });
        }
        let mut references = BTreeMap::<usize, usize>::new();
        for recipe in &recipes {
            let [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index,
                },
            ] = recipe.parts()
            else {
                return Err(StandardWriteBackMutationPlanError::InvalidChoices {
                    group_location: Box::new(group_location),
                    message: "选项配方必须只包含一个 Choices 行槽",
                });
            };
            if source_lines.get(*source_line_index).map(String::as_str)
                != Some(recipe.expected_raw())
            {
                return Err(StandardWriteBackMutationPlanError::InvalidChoices {
                    group_location: Box::new(group_location),
                    message: "选项配方与冻结原文不一致",
                });
            }
            *references.entry(*source_line_index).or_default() += 1;
        }
        if references.len() != source_lines.len()
            || (0..source_lines.len()).any(|index| references.get(&index) != Some(&2))
        {
            return Err(StandardWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                message: "每个选项必须同时对应 102 列表项和同层 402 标签",
            });
        }
        Ok(Self {
            group_location,
            recipes,
            source_lines,
            replacement_lines,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn recipes(&self) -> &[DirectTextRecipe] {
        &self.recipes
    }

    pub(crate) fn source_lines(&self) -> &[String] {
        &self.source_lines
    }

    pub(crate) fn replacement_lines(&self) -> &[String] {
        &self.replacement_lines
    }

    fn mutation_targets(&self) -> impl Iterator<Item = MutationTarget> + '_ {
        self.recipes
            .iter()
            .map(|recipe| MutationTarget::Value(recipe.target().clone()))
    }
}

/// 一条原始 401/405 正文在块级重建计划中的对应项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventBodyMutationSegment {
    exact_location: RpgMakerLocation,
    expected_original: String,
    replacement_lines: Vec<String>,
}

impl EventBodyMutationSegment {
    #[cfg(test)]
    pub(crate) fn replace_for_test(
        exact_location: RpgMakerLocation,
        expected_original: impl Into<String>,
        lines: Vec<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            replacement_lines: lines,
        }
    }

    fn replace(
        exact_location: RpgMakerLocation,
        expected_original: String,
        lines: Vec<String>,
    ) -> Self {
        debug_assert!(!lines.is_empty(), "译文语义行必须至少产生一个原生正文行");
        Self {
            exact_location,
            expected_original,
            replacement_lines: lines,
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn replacement_lines(&self) -> &[String] {
        &self.replacement_lines
    }
}

/// 一个完整滚动文本正文块的重建计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceEventBodyMutation {
    group_location: RpgMakerLocation,
    segments: Vec<EventBodyMutationSegment>,
}

impl ReplaceEventBodyMutation {
    pub(crate) fn new(
        group_location: RpgMakerLocation,
        segments: Vec<EventBodyMutationSegment>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        if segments.is_empty() {
            return Err(StandardWriteBackMutationPlanError::EmptyEventBody {
                group_location: Box::new(group_location),
            });
        }
        let mut exact_locations = BTreeSet::new();
        for segment in &segments {
            if !exact_locations.insert(segment.exact_location.clone()) {
                return Err(StandardWriteBackMutationPlanError::DuplicateTarget {
                    target: Box::new(MutationTarget::Value(segment.exact_location.clone())),
                });
            }
            if segment.replacement_lines.is_empty() {
                return Err(StandardWriteBackMutationPlanError::EmptyEventReplacement {
                    exact_location: Box::new(segment.exact_location.clone()),
                });
            }
        }
        Ok(Self {
            group_location,
            segments,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn segments(&self) -> &[EventBodyMutationSegment] {
        &self.segments
    }
}

/// Standard 交给 RPG Maker 文档改写器的一项领域修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackMutation {
    SetText(SetTextMutation),
    ReplaceDialogue(ReplaceDialogueMutation),
    ReplaceChoices(ReplaceChoicesMutation),
    ReplaceEventBody(ReplaceEventBodyMutation),
}

/// 已经排除位置冲突的一轮完整文档修改计划。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StandardWriteBackMutationPlan {
    mutations: Vec<StandardWriteBackMutation>,
}

impl StandardWriteBackMutationPlan {
    pub(crate) fn new(
        mutations: Vec<StandardWriteBackMutation>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        let mut targets = BTreeSet::new();
        let mut event_groups = BTreeSet::new();
        for mutation in &mutations {
            match mutation {
                StandardWriteBackMutation::SetText(mutation) => {
                    insert_plan_target(
                        &mut targets,
                        MutationTarget::Value(mutation.exact_location.clone()),
                    )?;
                }
                StandardWriteBackMutation::ReplaceDialogue(mutation) => {
                    if !event_groups.insert(mutation.group_location().clone()) {
                        return Err(StandardWriteBackMutationPlanError::DuplicateEventBody {
                            group_location: Box::new(mutation.group_location().clone()),
                        });
                    }
                    for target in mutation.mutation_targets() {
                        insert_plan_target(&mut targets, target)?;
                    }
                }
                StandardWriteBackMutation::ReplaceChoices(mutation) => {
                    for target in mutation.mutation_targets() {
                        insert_plan_target(&mut targets, target)?;
                    }
                }
                StandardWriteBackMutation::ReplaceEventBody(mutation) => {
                    if !event_groups.insert(mutation.group_location.clone()) {
                        return Err(StandardWriteBackMutationPlanError::DuplicateEventBody {
                            group_location: Box::new(mutation.group_location.clone()),
                        });
                    }
                    insert_plan_target(
                        &mut targets,
                        MutationTarget::Value(mutation.group_location.clone()),
                    )?;
                    for segment in &mutation.segments {
                        insert_plan_target(
                            &mut targets,
                            MutationTarget::Value(segment.exact_location.clone()),
                        )?;
                    }
                }
            }
        }
        Ok(Self { mutations })
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn mutations(&self) -> &[StandardWriteBackMutation] {
        &self.mutations
    }

    pub(crate) fn into_mutations(self) -> Vec<StandardWriteBackMutation> {
        self.mutations
    }
}

fn insert_plan_target(
    targets: &mut BTreeSet<MutationTarget>,
    target: MutationTarget,
) -> Result<(), StandardWriteBackMutationPlanError> {
    if !targets.insert(target.clone()) {
        return Err(StandardWriteBackMutationPlanError::DuplicateTarget {
            target: Box::new(target),
        });
    }
    Ok(())
}

/// Mutation 计划构造时发现的内部冲突。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackMutationPlanError {
    EmptyEventBody {
        group_location: Box<RpgMakerLocation>,
    },
    EmptyEventReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    InvalidDialogue {
        group_location: Box<RpgMakerLocation>,
        message: &'static str,
    },
    InvalidChoices {
        group_location: Box<RpgMakerLocation>,
        message: &'static str,
    },
    DuplicateTarget {
        target: Box<MutationTarget>,
    },
    DuplicateEventBody {
        group_location: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for StandardWriteBackMutationPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEventBody { group_location } => {
                write!(formatter, "事件正文修改不包含原始段：{group_location}")
            }
            Self::EmptyEventReplacement { exact_location } => {
                write!(formatter, "事件正文译文没有产生显示行：{exact_location}")
            }
            Self::InvalidDialogue {
                group_location,
                message,
            } => write!(formatter, "对话修改 {group_location} 无效：{message}"),
            Self::InvalidChoices {
                group_location,
                message,
            } => write!(formatter, "选项修改 {group_location} 无效：{message}"),
            Self::DuplicateTarget { target } => {
                write!(formatter, "Mutation 计划重复修改物理目标：{target:?}")
            }
            Self::DuplicateEventBody { group_location } => {
                write!(formatter, "Mutation 计划重复修改事件正文：{group_location}")
            }
        }
    }
}

impl Error for StandardWriteBackMutationPlanError {}

/// 把领域 Mutation 应用到冻结 RPG Maker 文档并产生一个待发布候选。
///
/// 实现必须从 `OpenedProject::source_root()` 下的冻结文档读取权威结构，并在修改前用
/// `expected_original` 核对每个目标仍与快照一致。每项 Mutation 必须恰好应用一次；
/// 目标缺失、重复或原文不匹配都是技术错误。`ReplaceDialogue` 必须同时核对并替换
/// Speaker 与完整 `101 + 401*` 块；`ReplaceEventBody` 仅用于 `105 + 405*` 滚动正文。
/// 本能力只产生候选，不发布文件，也不把领域计划泄漏成 JSON 或字节覆盖集合。
pub(crate) trait RpgMakerWriteBackDocumentRewriter: Send + Sync {
    type RewrittenDocuments: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn rewrite(
        &self,
        project: &OpenedProject,
        plan: StandardWriteBackMutationPlan,
    ) -> impl Future<Output = Result<Self::RewrittenDocuments, Self::Error>> + Send;
}

/// 一项需要人工调整布局、但没有阻止写回的结构化诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualLayoutDiagnostic {
    locations: Vec<LogicalTextLocation>,
    region: RpgMakerWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
}

impl ManualLayoutDiagnostic {
    fn from_request(request: &RpgMakerWriteBackLayoutRequest) -> Self {
        let mut seen = BTreeSet::new();
        let locations = request
            .segments
            .iter()
            .filter_map(|segment| match segment.candidate {
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal => None,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_) => {
                    let location = segment
                        .logical_location
                        .clone()
                        .expect("数据库译文布局段必须属于逻辑单元");
                    seen.insert(location.clone()).then_some(location)
                }
            })
            .collect();
        Self::new(locations, request.region, request.max_fullwidth_chars)
    }

    fn new(
        locations: Vec<LogicalTextLocation>,
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
    ) -> Self {
        debug_assert!(
            !locations.is_empty(),
            "人工布局诊断必须关联至少一个逻辑单元"
        );
        Self {
            locations,
            region,
            max_fullwidth_chars,
        }
    }

    pub(crate) fn locations(&self) -> &[LogicalTextLocation] {
        &self.locations
    }

    pub(crate) const fn region(&self) -> RpgMakerWriteBackLayoutRegion {
        self.region
    }

    pub(crate) const fn max_fullwidth_chars(&self) -> MaxFullwidthChars {
        self.max_fullwidth_chars
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        locations: Vec<LogicalTextLocation>,
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
    ) -> Self {
        Self::new(locations, region, max_fullwidth_chars)
    }
}

/// 一次完整 WriteBack 成功发布后写入持久日志的运行事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WriteBackRunLog {
    layout_profile: RpgMakerWriteBackLayoutProfile,
    output_root: PathBuf,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
    lua_executed: bool,
}

impl WriteBackRunLog {
    pub(crate) fn new(
        project: &OpenedProject,
        layout_profile: RpgMakerWriteBackLayoutProfile,
        summary: StandardWriteBackSummary,
        manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
        lua_executed: bool,
    ) -> Self {
        assert_eq!(
            summary.manual_layout_units,
            manual_layout_diagnostics.len(),
            "人工布局计数必须由结构化诊断唯一建立"
        );
        Self {
            layout_profile,
            output_root: project.write_back_root().to_path_buf(),
            summary,
            manual_layout_diagnostics,
            lua_executed,
        }
    }

    pub(crate) const fn layout_profile(&self) -> RpgMakerWriteBackLayoutProfile {
        self.layout_profile
    }

    pub(crate) fn output_root(&self) -> &Path {
        &self.output_root
    }

    pub(crate) const fn summary(&self) -> StandardWriteBackSummary {
        self.summary
    }

    pub(crate) fn manual_layout_diagnostics(&self) -> &[ManualLayoutDiagnostic] {
        &self.manual_layout_diagnostics
    }

    pub(crate) const fn lua_executed(&self) -> bool {
        self.lua_executed
    }
}

/// Standard 阶段生成的文件候选和全部业务事实。
pub(crate) struct StandardWriteBackPreparation<D> {
    documents: D,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

impl<D> fmt::Debug for StandardWriteBackPreparation<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StandardWriteBackPreparation")
            .field("summary", &self.summary)
            .field("manual_layout_diagnostics", &self.manual_layout_diagnostics)
            .field("documents", &"<owned documents>")
            .finish()
    }
}

impl<D> StandardWriteBackPreparation<D> {
    pub(crate) fn new(
        documents: D,
        summary: StandardWriteBackSummary,
        manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
    ) -> Self {
        assert_eq!(
            summary.manual_layout_units,
            manual_layout_diagnostics.len(),
            "人工布局计数必须由结构化诊断唯一建立"
        );
        Self {
            documents,
            summary,
            manual_layout_diagnostics,
        }
    }

    pub(crate) fn into_parts(self) -> (D, StandardWriteBackSummary, Vec<ManualLayoutDiagnostic>) {
        (self.documents, self.summary, self.manual_layout_diagnostics)
    }
}

/// 使用资产读取、布局和文档改写能力准备 Standard 写回候选。
pub(crate) struct StandardWriteBackService<R, L, D, C> {
    asset_reader: R,
    text_layouter: Arc<L>,
    document_rewriter: D,
    cpu: Arc<C>,
    cancellation: CooperativeCancellation,
}

impl<R, L, D, C> StandardWriteBackService<R, L, D, C> {
    pub(crate) fn new(
        asset_reader: R,
        text_layouter: L,
        document_rewriter: D,
        cpu: C,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            asset_reader,
            text_layouter: Arc::new(text_layouter),
            document_rewriter,
            cpu: Arc::new(cpu),
            cancellation,
        }
    }
}

impl<R, L, D, C> StandardWriteBack for StandardWriteBackService<R, L, D, C>
where
    R: StandardWriteBackAssetReader,
    L: RpgMakerWriteBackTextLayouter + 'static,
    D: RpgMakerWriteBackDocumentRewriter,
    C: CpuTaskExecutor,
{
    type Documents = D::RewrittenDocuments;
    type Error = StandardWriteBackServiceError<R::Error, D::Error, C::Error>;

    async fn prepare(
        &self,
        project: &OpenedProject,
        layout_profile: &RpgMakerWriteBackLayoutProfile,
    ) -> Result<OperationCompletion<StandardWriteBackPreparation<Self::Documents>>, Self::Error>
    {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let snapshot = self
            .asset_reader
            .read(project)
            .await
            .map_err(StandardWriteBackServiceError::ReadAssets)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let groups = snapshot.into_groups();
        let profile = *layout_profile;
        let layouter = Arc::clone(&self.text_layouter);
        let planned_groups = self
            .cpu
            .execute_ordered_map(groups, move |group| {
                plan_standard_write_back_group(group, &profile, layouter.as_ref())
            })
            .await
            .map_err(StandardWriteBackServiceError::SchedulePlanning)?;
        let planned = self
            .cpu
            .execute(move || assemble_planned_standard_write_back(planned_groups))
            .await
            .map_err(StandardWriteBackServiceError::SchedulePlanning)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let rewritten = self
            .document_rewriter
            .rewrite(project, planned.mutation_plan)
            .await
            .map_err(StandardWriteBackServiceError::RewriteDocuments)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        Ok(OperationCompletion::Completed(
            StandardWriteBackPreparation::new(
                rewritten,
                planned.summary,
                planned.manual_layout_diagnostics,
            ),
        ))
    }
}

struct PlannedStandardWriteBack {
    mutation_plan: StandardWriteBackMutationPlan,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

struct PlannedStandardWriteBackGroup {
    mutations: Vec<StandardWriteBackMutation>,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

struct GroupPlanningOutputs<'a> {
    mutations: &'a mut Vec<StandardWriteBackMutation>,
    summary: &'a mut StandardWriteBackSummary,
    manual_layout_diagnostics: &'a mut Vec<ManualLayoutDiagnostic>,
}

#[cfg(test)]
fn plan_standard_write_back(
    snapshot: StandardWriteBackSnapshot,
    profile: &RpgMakerWriteBackLayoutProfile,
    layouter: &impl RpgMakerWriteBackTextLayouter,
) -> PlannedStandardWriteBack {
    let groups = snapshot
        .into_groups()
        .into_iter()
        .map(|group| plan_standard_write_back_group(group, profile, layouter))
        .collect();
    assemble_planned_standard_write_back(groups)
}

fn plan_standard_write_back_group(
    group: StandardWriteBackGroup,
    profile: &RpgMakerWriteBackLayoutProfile,
    layouter: &impl RpgMakerWriteBackTextLayouter,
) -> PlannedStandardWriteBackGroup {
    let mut mutations = Vec::new();
    let mut summary = StandardWriteBackSummary::default();
    let mut manual_layout_diagnostics = Vec::new();

    {
        let mut outputs = GroupPlanningOutputs {
            mutations: &mut mutations,
            summary: &mut summary,
            manual_layout_diagnostics: &mut manual_layout_diagnostics,
        };
        for unit in &group.units {
            if unit.translation_content.is_some() {
                outputs.summary.translated_units += 1;
            } else {
                outputs.summary.original_units += 1;
            }
        }

        let (kind, group_location, units, recipes) = group.into_parts();
        match kind {
            TextGroupKind::EventDialogue => plan_dialogue_group(
                group_location,
                units,
                recipes,
                profile.dialogue_body(),
                layouter,
                &mut outputs,
            ),
            TextGroupKind::EventScrollingText => plan_scrolling_group(
                group_location,
                units,
                recipes,
                profile.scrolling_text(),
                layouter,
                &mut outputs,
            ),
            TextGroupKind::EventChoices => {
                plan_choices_group(group_location, units, recipes, &mut outputs)
            }
            _ => plan_scalar_group(
                kind,
                group_location,
                units,
                recipes,
                profile.help_description(),
                layouter,
                &mut outputs,
            ),
        }
        outputs.summary.manual_layout_units = outputs.manual_layout_diagnostics.len();
    }

    PlannedStandardWriteBackGroup {
        mutations,
        summary,
        manual_layout_diagnostics,
    }
}

fn assemble_planned_standard_write_back(
    groups: Vec<PlannedStandardWriteBackGroup>,
) -> PlannedStandardWriteBack {
    let mut mutations = Vec::new();
    let mut summary = StandardWriteBackSummary::default();
    let mut manual_layout_diagnostics = Vec::new();
    for group in groups {
        mutations.extend(group.mutations);
        merge_standard_write_back_summary(&mut summary, group.summary);
        manual_layout_diagnostics.extend(group.manual_layout_diagnostics);
    }
    summary.manual_layout_units = manual_layout_diagnostics.len();

    let mutation_plan = StandardWriteBackMutationPlan::new(mutations)
        .expect("受信快照和布局结果不应产生冲突 Mutation");
    PlannedStandardWriteBack {
        mutation_plan,
        summary,
        manual_layout_diagnostics,
    }
}

fn merge_standard_write_back_summary(
    total: &mut StandardWriteBackSummary,
    group: StandardWriteBackSummary,
) {
    total.translated_units += group.translated_units;
    total.original_units += group.original_units;
    total.auto_wrapped_units += group.auto_wrapped_units;
    total.inserted_line_breaks += group.inserted_line_breaks;
    total.inserted_fullwidth_indents += group.inserted_fullwidth_indents;
}

fn plan_dialogue_group(
    group_location: RpgMakerLocation,
    units: Vec<StandardWriteBackUnit>,
    mut recipes: Vec<TextProjectionRecipe>,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    if !units.iter().any(|unit| unit.translation_content.is_some()) {
        return;
    }
    let TextProjectionRecipe::Dialogue(recipe) = recipes.pop().expect("受信对话组必须包含唯一配方")
    else {
        unreachable!("受信对话组必须包含 Dialogue 配方")
    };
    debug_assert!(recipes.is_empty());

    let units = units
        .into_iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    let speaker_unit = units.get(&TextUnitRole::DialogueSpeaker);
    let source_speaker = speaker_unit.map(|unit| {
        unit.source_content
            .as_value()
            .expect("受信 Speaker 原文必须是单值")
            .to_owned()
    });
    let speaker = speaker_unit.map(|unit| {
        unit.effective_content()
            .as_value()
            .expect("受信 Speaker 必须是单值")
            .to_owned()
    });
    let body = units.get(&TextUnitRole::DialogueBody);
    let body_lines = if body.is_some_and(|unit| unit.translation_content.is_some()) {
        let body = body.expect("已经确认 Body 存在");
        let exact_location =
            dialogue_body_location(&recipe).expect("受信对话正文配方必须引用至少一个 BodyLine");
        let request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            max_fullwidth_chars,
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &group_location,
                body,
                exact_location.clone(),
            )],
        );
        Some(
            layout_replacements(&request, layouter, outputs)
                .remove(&exact_location)
                .expect("布局结果必须覆盖对话正文语义单元"),
        )
    } else {
        None
    };

    let mutation = ReplaceDialogueMutation::new(recipe, source_speaker, speaker, body_lines)
        .expect("受信对话资产必须建立合法原子 Mutation");
    outputs
        .mutations
        .push(StandardWriteBackMutation::ReplaceDialogue(mutation));
}

fn plan_scrolling_group(
    group_location: RpgMakerLocation,
    units: Vec<StandardWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let [unit] = units.as_slice() else {
        unreachable!("受信滚动文本组必须只包含一个语义单元")
    };
    let Some(replacement_lines) = aligned_replacement_lines(unit) else {
        return;
    };
    let source_lines = unit
        .source_content
        .as_lines()
        .expect("受信滚动文本原文必须是行序列");
    let entries = recipes
        .iter()
        .map(|recipe| {
            let TextProjectionRecipe::Direct(recipe) = recipe else {
                unreachable!("受信滚动文本组只包含直接配方")
            };
            let [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::ScrollingText,
                    source_line_index,
                },
            ] = recipe.parts()
            else {
                unreachable!("受信滚动文本配方必须只包含一个行槽")
            };
            (recipe, *source_line_index)
        })
        .collect::<Vec<_>>();

    let request = RpgMakerWriteBackLayoutRequest::new(
        RpgMakerWriteBackLayoutRegion::ScrollingText,
        max_fullwidth_chars,
        entries
            .iter()
            .map(|(recipe, source_line_index)| {
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &group_location,
                    TextUnitRole::ScrollingText,
                    recipe.target().clone(),
                    source_lines[*source_line_index].clone(),
                    Some(replacement_lines[*source_line_index].clone()),
                )
            })
            .collect(),
    );
    let replacements = layout_replacements(&request, layouter, outputs);
    let segments = entries
        .into_iter()
        .map(|(recipe, _)| {
            let lines = replacements
                .get(recipe.target())
                .expect("受信布局结果必须覆盖每个滚动文本语义行")
                .iter()
                .map(|line| line.text().to_owned())
                .collect();
            EventBodyMutationSegment::replace(
                recipe.target().clone(),
                recipe.expected_raw().to_owned(),
                lines,
            )
        })
        .collect();
    let mutation = ReplaceEventBodyMutation::new(group_location, segments)
        .expect("受信滚动正文应建立合法块级 Mutation");
    outputs
        .mutations
        .push(StandardWriteBackMutation::ReplaceEventBody(mutation));
}

fn plan_choices_group(
    group_location: RpgMakerLocation,
    units: Vec<StandardWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let [unit] = units.as_slice() else {
        unreachable!("受信选项组必须只包含一个语义单元")
    };
    let Some(replacement_lines) = aligned_replacement_lines(unit) else {
        return;
    };
    let source_lines = unit
        .source_content
        .as_lines()
        .expect("受信选项原文必须是行序列");
    let recipes = recipes
        .into_iter()
        .map(|recipe| match recipe {
            TextProjectionRecipe::Direct(recipe) => recipe,
            TextProjectionRecipe::Dialogue(_) => unreachable!("受信选项组只包含直接配方"),
        })
        .collect();
    let mutation = ReplaceChoicesMutation::new(
        group_location,
        recipes,
        source_lines.to_vec(),
        replacement_lines,
    )
    .expect("受信选项资产必须建立合法原子 Mutation");
    outputs
        .mutations
        .push(StandardWriteBackMutation::ReplaceChoices(mutation));
}

fn plan_scalar_group(
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<StandardWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    help_max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let units = units
        .into_iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    for recipe in recipes {
        let TextProjectionRecipe::Direct(recipe) = recipe else {
            unreachable!("受信普通文本组只包含直接配方")
        };
        let roles = recipe
            .parts()
            .iter()
            .filter_map(|part| match part {
                DirectTextPart::Literal(_) => None,
                DirectTextPart::TextSlot { role } | DirectTextPart::LineSlot { role, .. } => {
                    Some(role)
                }
            })
            .collect::<Vec<_>>();
        if !roles.iter().any(|role| {
            units
                .get(*role)
                .is_some_and(|unit| unit.translation_content.is_some())
        }) {
            continue;
        }

        let mut overrides = BTreeMap::new();
        if roles.len() == 1 {
            let role = roles[0];
            let unit = units.get(role).expect("受信配方角色必须存在语义单元");
            if unit.translation_content.is_some()
                && is_canonical_help_description(kind, unit, &recipe)
            {
                let request = RpgMakerWriteBackLayoutRequest::new(
                    RpgMakerWriteBackLayoutRegion::HelpDescription,
                    help_max_fullwidth_chars,
                    vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                        &group_location,
                        unit,
                        recipe.target().clone(),
                    )],
                );
                let replacement = layout_replacements(&request, layouter, outputs)
                    .remove(recipe.target())
                    .expect("帮助说明布局必须返回唯一译文单元")
                    .iter()
                    .map(RpgMakerWriteBackLaidOutLine::text)
                    .collect::<Vec<_>>()
                    .join("\n");
                overrides.insert(role.clone(), replacement);
            }
        }

        let replacement = render_direct_recipe(&recipe, &units, &overrides);
        outputs.mutations.push(StandardWriteBackMutation::SetText(
            SetTextMutation::from_recipe(&recipe, replacement),
        ));
    }
}

fn layout_replacements(
    request: &RpgMakerWriteBackLayoutRequest,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) -> BTreeMap<RpgMakerLocation, Vec<RpgMakerWriteBackLaidOutLine>> {
    match layouter.layout(request) {
        RpgMakerWriteBackLayoutOutcome::Applied(applied) => {
            let (segments, inserted_line_breaks, inserted_fullwidth_indents) = applied.into_parts();
            record_applied_layout(
                outputs.summary,
                inserted_line_breaks,
                inserted_fullwidth_indents,
            );
            segments
                .into_iter()
                .map(|segment| (segment.exact_location, segment.lines))
                .collect()
        }
        RpgMakerWriteBackLayoutOutcome::Manual => {
            outputs
                .manual_layout_diagnostics
                .push(ManualLayoutDiagnostic::from_request(request));
            request
                .segments
                .iter()
                .filter_map(|segment| match &segment.candidate {
                    RpgMakerWriteBackLayoutCandidate::FrozenOriginal => None,
                    RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(translation) => Some((
                        segment.exact_location.clone(),
                        split_hard_lines(translation)
                            .into_iter()
                            .enumerate()
                            .map(|(source_semantic_line_index, text)| {
                                RpgMakerWriteBackLaidOutLine::new(text, source_semantic_line_index)
                            })
                            .collect(),
                    )),
                })
                .collect()
        }
    }
}

fn dialogue_body_location(recipe: &DialogueWriteRecipe) -> Option<RpgMakerLocation> {
    recipe.lines().iter().find_map(|line| {
        line.parts()
            .iter()
            .any(|part| matches!(part, DialogueLinePart::BodyLine { .. }))
            .then(|| line.physical_location().clone())
    })
}

fn render_direct_recipe(
    recipe: &DirectTextRecipe,
    units: &BTreeMap<TextUnitRole, StandardWriteBackUnit>,
    overrides: &BTreeMap<TextUnitRole, String>,
) -> String {
    let mut rendered = String::new();
    for part in recipe.parts() {
        match part {
            DirectTextPart::Literal(value) => rendered.push_str(value),
            DirectTextPart::TextSlot { role } => {
                if let Some(value) = overrides.get(role) {
                    rendered.push_str(value);
                } else {
                    rendered.push_str(
                        units
                            .get(role)
                            .expect("受信直接配方角色必须存在语义单元")
                            .effective_content()
                            .as_value()
                            .expect("TextSlot 必须引用单值内容"),
                    );
                }
            }
            DirectTextPart::LineSlot {
                role,
                source_line_index,
            } => rendered.push_str(
                units
                    .get(role)
                    .expect("受信直接配方角色必须存在语义单元")
                    .effective_content()
                    .as_lines()
                    .and_then(|lines| lines.get(*source_line_index))
                    .expect("LineSlot 必须引用存在的语义行"),
            ),
        }
    }
    rendered
}

fn record_applied_layout(
    summary: &mut StandardWriteBackSummary,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
) {
    if inserted_line_breaks > 0 {
        summary.auto_wrapped_units += 1;
    }
    summary.inserted_line_breaks += inserted_line_breaks;
    summary.inserted_fullwidth_indents += inserted_fullwidth_indents;
}

fn split_hard_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

fn is_canonical_help_description(
    kind: TextGroupKind,
    unit: &StandardWriteBackUnit,
    recipe: &DirectTextRecipe,
) -> bool {
    if kind != TextGroupKind::DatabaseEntry {
        return false;
    }
    if !matches!(
        &unit.role,
        TextUnitRole::Scalar(field_name) if field_name.as_str() == "description"
    ) {
        return false;
    }
    let RpgMakerLocation::Value { source, steps } = recipe.target() else {
        return false;
    };
    let RpgMakerSource::Data(file) = source else {
        return false;
    };
    if !matches!(
        file,
        StandardDataFile::Skills
            | StandardDataFile::Items
            | StandardDataFile::Weapons
            | StandardDataFile::Armors
    ) {
        return false;
    }
    matches!(
        steps.as_slice(),
        [RpgMakerLocationStep::ArrayIndex(_), RpgMakerLocationStep::ObjectKey(field_name)]
            if field_name == "description"
    )
}

/// Standard 在资产读取和文档改写边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum StandardWriteBackServiceError<R, D, C> {
    ReadAssets(R),
    SchedulePlanning(CpuTaskExecutionError<C>),
    RewriteDocuments(D),
}

impl<R, D, C> fmt::Display for StandardWriteBackServiceError<R, D, C>
where
    R: fmt::Display,
    D: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadAssets(source) => write!(formatter, "读取 Standard 写回资产失败：{source}"),
            Self::SchedulePlanning(source) => {
                write!(formatter, "调度 Standard 写回规划失败：{source}")
            }
            Self::RewriteDocuments(source) => {
                write!(formatter, "改写 RPG Maker 文档失败：{source}")
            }
        }
    }
}

impl<R, D, C> Error for StandardWriteBackServiceError<R, D, C>
where
    R: Error + 'static,
    D: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadAssets(source) => Some(source),
            Self::SchedulePlanning(source) => Some(source),
            Self::RewriteDocuments(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::model::{
        DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget,
        ScalarFieldKey,
    };
    use crate::rpg_maker::project::MaxFullwidthChars;

    fn location(command_index: usize, parameter_index: Option<usize>) -> RpgMakerLocation {
        let mut steps = vec![
            RpgMakerLocationStep::key("list"),
            RpgMakerLocationStep::index(command_index),
        ];
        if let Some(parameter_index) = parameter_index {
            steps.extend([
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(parameter_index),
            ]);
        }
        RpgMakerLocation::value(RpgMakerSource::map(1), steps)
    }

    fn profile() -> RpgMakerWriteBackLayoutProfile {
        let width = MaxFullwidthChars::new(40).expect("测试行宽应合法");
        RpgMakerWriteBackLayoutProfile::new(width, width, width)
    }

    fn dialogue_snapshot(
        speaker_translation: Option<&str>,
        body_translation: Option<&str>,
    ) -> StandardWriteBackSnapshot {
        let header = location(0, None);
        let line = location(1, Some(0));
        let recipe = DialogueWriteRecipe::new(
            header.clone(),
            None,
            vec![
                DialogueLineRecipe::new(
                    line,
                    "\\n<Alice>Hello",
                    vec![
                        DialogueLinePart::Literal("\\n<".to_owned()),
                        DialogueLinePart::SpeakerSlot,
                        DialogueLinePart::Literal(">".to_owned()),
                        DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        },
                    ],
                )
                .expect("测试对话行应合法"),
            ],
        )
        .expect("测试对话配方应合法");
        let projection = TextProjectionRecipe::Dialogue(recipe);
        let group = StandardWriteBackGroup::new(
            TextGroupKind::EventDialogue,
            header,
            vec![
                StandardWriteBackUnit::new(
                    TextUnitRole::DialogueSpeaker,
                    TextUnitContent::Value("Alice".to_owned()),
                    speaker_translation
                        .map(|translation| TextUnitContent::Value(translation.to_owned())),
                )
                .expect("测试 Speaker 应合法"),
                StandardWriteBackUnit::new(
                    TextUnitRole::DialogueBody,
                    TextUnitContent::Lines(vec!["Hello".to_owned()]),
                    body_translation
                        .map(|translation| TextUnitContent::Lines(split_hard_lines(translation))),
                )
                .expect("测试 Body 应合法"),
            ],
            vec![projection.clone()],
            projection.mutation_targets(),
        )
        .expect("测试对话组应合法");
        StandardWriteBackSnapshot::new(vec![group]).expect("测试快照应合法")
    }

    fn dialogue_mutation(
        speaker_translation: Option<&str>,
        body_translation: Option<&str>,
    ) -> Option<ReplaceDialogueMutation> {
        let planned = plan_standard_write_back(
            dialogue_snapshot(speaker_translation, body_translation),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        match planned.mutation_plan.mutations().first() {
            None => None,
            Some(StandardWriteBackMutation::ReplaceDialogue(mutation)) => Some(mutation.clone()),
            Some(other) => panic!("对话必须生成唯一块级 Mutation，实际为 {other:?}"),
        }
    }

    #[test]
    fn manual_layout_diagnostic_identifies_affected_logical_units() {
        let scalar_group = location(8, None);
        let scalar_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键应合法"));
        let scalar_unit = StandardWriteBackUnit::new(
            scalar_role.clone(),
            TextUnitContent::Value("原说明".to_owned()),
            Some(TextUnitContent::Value("很长的译文".to_owned())),
        )
        .expect("测试标量单元应合法");
        let scalar_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::HelpDescription,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &scalar_group,
                &scalar_unit,
                location(8, Some(0)),
            )],
        );
        let scalar_diagnostic = ManualLayoutDiagnostic::from_request(&scalar_request);
        assert_eq!(
            scalar_diagnostic.locations(),
            &[LogicalTextLocation::new(scalar_group, scalar_role)]
        );

        let dialogue_group = location(10, None);
        let body_role = TextUnitRole::DialogueBody;
        let body = StandardWriteBackUnit::new(
            body_role.clone(),
            TextUnitContent::Lines(vec!["原文一".to_owned(), "原文二".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "译文一".to_owned(),
                "译文二".to_owned(),
            ])),
        )
        .expect("对话正文单元应合法");
        let dialogue_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &dialogue_group,
                &body,
                location(11, Some(0)),
            )],
        );
        let dialogue_diagnostic = ManualLayoutDiagnostic::from_request(&dialogue_request);
        assert_eq!(
            dialogue_diagnostic.locations(),
            [LogicalTextLocation::new(dialogue_group, body_role)]
        );

        let scrolling_group = location(20, None);
        let scrolling_role = TextUnitRole::ScrollingText;
        let scrolling_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::ScrollingText,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &scrolling_group,
                    scrolling_role.clone(),
                    location(21, Some(0)),
                    "原文一".to_owned(),
                    Some("译文一".to_owned()),
                ),
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &scrolling_group,
                    scrolling_role.clone(),
                    location(22, Some(0)),
                    "原文二".to_owned(),
                    Some("译文二".to_owned()),
                ),
            ],
        );
        let scrolling_diagnostic = ManualLayoutDiagnostic::from_request(&scrolling_request);
        assert_eq!(
            scrolling_diagnostic.locations(),
            [LogicalTextLocation::new(scrolling_group, scrolling_role)]
        );
    }

    #[test]
    fn dialogue_none_speaker_only_body_only_and_both_use_one_atomic_mutation() {
        assert!(dialogue_mutation(None, None).is_none());

        let speaker_only = dialogue_mutation(Some("爱丽丝"), None).expect("Speaker 译文应触发写回");
        assert_eq!(speaker_only.speaker(), Some("爱丽丝"));
        assert_eq!(speaker_only.body_lines(), None);

        let body_only = dialogue_mutation(None, Some("你好")).expect("Body 译文应触发写回");
        assert_eq!(body_only.speaker(), Some("Alice"));
        assert_eq!(
            body_only.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["你好"])
        );

        let both = dialogue_mutation(Some("爱丽丝"), Some("你好")).expect("两类译文应触发写回");
        assert_eq!(both.speaker(), Some("爱丽丝"));
        assert_eq!(
            both.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["你好"])
        );
    }

    #[test]
    fn dialogue_body_hard_line_breaks_preserve_semantic_line_provenance() {
        let mutation =
            dialogue_mutation(None, Some("第一行\n第二行")).expect("Body 译文应触发写回");
        assert_eq!(
            mutation.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["第一行", "第二行"])
        );
        assert_eq!(
            mutation.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::source_semantic_line_index)
                .collect::<Vec<_>>()),
            Some(vec![0, 1])
        );
    }

    #[test]
    fn scrolling_recipe_keeps_blank_slots_inside_the_atomic_unit() {
        let group_location = location(0, None);
        let role = TextUnitRole::ScrollingText;
        let recipes = vec![
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(1, Some(0)),
                    "第一行",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 0,
                    }],
                )
                .expect("首行配方应合法"),
            ),
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(2, Some(0)),
                    "   ",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 1,
                    }],
                )
                .expect("冻结空白配方应合法"),
            ),
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(3, Some(0)),
                    "第三行",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 2,
                    }],
                )
                .expect("末行配方应合法"),
            ),
        ];
        let targets = recipes
            .iter()
            .flat_map(TextProjectionRecipe::mutation_targets)
            .collect();
        let group = StandardWriteBackGroup::new(
            TextGroupKind::EventScrollingText,
            group_location,
            vec![
                StandardWriteBackUnit::new(
                    role,
                    TextUnitContent::Lines(vec![
                        "第一行".to_owned(),
                        "   ".to_owned(),
                        "第三行".to_owned(),
                    ]),
                    Some(TextUnitContent::Lines(vec![
                        "译文".to_owned(),
                        String::new(),
                        "第三行".to_owned(),
                    ])),
                )
                .expect("滚动文本单元应合法"),
            ],
            recipes,
            targets,
        )
        .expect("包含空白物理行的滚动组应合法");

        let planned = plan_standard_write_back(
            StandardWriteBackSnapshot::new(vec![group]).expect("滚动快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [StandardWriteBackMutation::ReplaceEventBody(mutation)] =
            planned.mutation_plan.mutations()
        else {
            panic!("滚动组应产生唯一块级 Mutation")
        };
        assert_eq!(mutation.segments().len(), 3);
        assert_eq!(mutation.segments()[0].replacement_lines(), &["译文"]);
        assert_eq!(mutation.segments()[1].replacement_lines(), &["   "]);
        assert_eq!(mutation.segments()[1].expected_original(), "   ");
        assert_eq!(mutation.segments()[2].replacement_lines(), &["第三行"]);
    }

    #[test]
    fn choices_are_planned_as_one_strictly_aligned_atomic_mutation() {
        let group_location = location(20, None);
        let source_lines = vec!["はい".to_owned(), "いいえ".to_owned()];
        let translated_lines = vec!["是".to_owned(), "否".to_owned()];
        let recipes = [
            (location(20, Some(0)), 0),
            (location(20, Some(1)), 1),
            (location(21, Some(1)), 0),
            (location(22, Some(1)), 1),
        ]
        .into_iter()
        .map(|(target, source_line_index)| {
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    target,
                    source_lines[source_line_index].clone(),
                    vec![DirectTextPart::LineSlot {
                        role: TextUnitRole::Choices,
                        source_line_index,
                    }],
                )
                .expect("选项配方应合法"),
            )
        })
        .collect::<Vec<_>>();
        let targets = recipes
            .iter()
            .flat_map(TextProjectionRecipe::mutation_targets)
            .collect();
        let group = StandardWriteBackGroup::new(
            TextGroupKind::EventChoices,
            group_location,
            vec![
                StandardWriteBackUnit::new(
                    TextUnitRole::Choices,
                    TextUnitContent::Lines(source_lines.clone()),
                    Some(TextUnitContent::Lines(translated_lines.clone())),
                )
                .expect("选项单元应合法"),
            ],
            recipes,
            targets,
        )
        .expect("选项组应合法");

        let planned = plan_standard_write_back(
            StandardWriteBackSnapshot::new(vec![group]).expect("选项快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [StandardWriteBackMutation::ReplaceChoices(mutation)] =
            planned.mutation_plan.mutations()
        else {
            panic!("选项组应产生唯一原子 Mutation")
        };
        assert_eq!(mutation.source_lines(), source_lines);
        assert_eq!(mutation.replacement_lines(), translated_lines);
    }

    #[test]
    fn aligned_units_reject_line_count_and_blank_slot_changes() {
        assert!(matches!(
            StandardWriteBackUnit::new(
                TextUnitRole::ScrollingText,
                TextUnitContent::Lines(vec!["甲".to_owned(), "乙".to_owned()]),
                Some(TextUnitContent::Lines(vec!["译文".to_owned()])),
            ),
            Err(StandardWriteBackSnapshotError::AlignedLineCountMismatch { .. })
        ));
        assert!(matches!(
            StandardWriteBackUnit::new(
                TextUnitRole::Choices,
                TextUnitContent::Lines(vec!["是".to_owned(), "   ".to_owned()]),
                Some(TextUnitContent::Lines(vec![
                    "はい".to_owned(),
                    "填充".to_owned()
                ])),
            ),
            Err(StandardWriteBackSnapshotError::AlignedBlankLineMismatch { line_index: 1, .. })
        ));
    }

    #[test]
    fn direct_recipe_renders_literals_and_all_logical_slots_once() {
        let group_location = location(3, None);
        let target = location(3, Some(0));
        let left = TextUnitRole::Scalar(ScalarFieldKey::new("left").expect("键应合法"));
        let right = TextUnitRole::Scalar(ScalarFieldKey::new("right").expect("键应合法"));
        let recipe = DirectTextRecipe::new(
            target,
            "<x>甲</x><x>乙</x>",
            vec![
                DirectTextPart::Literal("<x>".to_owned()),
                DirectTextPart::TextSlot { role: left.clone() },
                DirectTextPart::Literal("</x><x>".to_owned()),
                DirectTextPart::TextSlot {
                    role: right.clone(),
                },
                DirectTextPart::Literal("</x>".to_owned()),
            ],
        )
        .expect("直接配方应合法");
        let projection = TextProjectionRecipe::Direct(recipe);
        let group = StandardWriteBackGroup::new(
            TextGroupKind::EventCommand,
            group_location,
            vec![
                StandardWriteBackUnit::new(
                    left,
                    TextUnitContent::Value("甲".to_owned()),
                    Some(TextUnitContent::Value("一".to_owned())),
                )
                .expect("左单元应合法"),
                StandardWriteBackUnit::new(right, TextUnitContent::Value("乙".to_owned()), None)
                    .expect("右单元应合法"),
            ],
            vec![projection.clone()],
            projection.mutation_targets(),
        )
        .expect("直接组应合法");
        let planned = plan_standard_write_back(
            StandardWriteBackSnapshot::new(vec![group]).expect("快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [StandardWriteBackMutation::SetText(mutation)] = planned.mutation_plan.mutations()
        else {
            panic!("直接组应产生唯一 SetText")
        };
        assert_eq!(mutation.replacement(), "<x>一</x><x>乙</x>");
    }

    #[test]
    fn snapshot_rejects_recipe_target_corruption_and_cross_group_conflicts() {
        let group_location = location(4, None);
        let target = location(4, Some(0));
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("键应合法"));
        let recipe = DirectTextRecipe::new(
            target.clone(),
            "原文",
            vec![DirectTextPart::TextSlot { role: role.clone() }],
        )
        .expect("配方应合法");
        let projection = TextProjectionRecipe::Direct(recipe);
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventCommand,
                group_location.clone(),
                vec![
                    StandardWriteBackUnit::new(
                        role.clone(),
                        TextUnitContent::Value("原文".to_owned()),
                        None,
                    )
                    .expect("单元应合法")
                ],
                vec![projection.clone()],
                vec![MutationTarget::Value(location(9, Some(0)))],
            ),
            Err(StandardWriteBackSnapshotError::RecipeTargetMismatch { .. })
        ));

        let make_group = |field: &str| {
            let unit_role = TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("键应合法"));
            let direct = DirectTextRecipe::new(
                target.clone(),
                "原文",
                vec![DirectTextPart::TextSlot {
                    role: unit_role.clone(),
                }],
            )
            .expect("配方应合法");
            let projection = TextProjectionRecipe::Direct(direct);
            StandardWriteBackGroup::new(
                TextGroupKind::EventCommand,
                group_location.clone(),
                vec![
                    StandardWriteBackUnit::new(
                        unit_role,
                        TextUnitContent::Value("原文".to_owned()),
                        None,
                    )
                    .expect("单元应合法"),
                ],
                vec![projection.clone()],
                projection.mutation_targets(),
            )
            .expect("单组应合法")
        };
        assert!(matches!(
            StandardWriteBackSnapshot::new(vec![make_group("first"), make_group("second")]),
            Err(StandardWriteBackSnapshotError::DuplicateTarget { .. })
        ));
    }

    #[test]
    fn snapshot_rejects_recipe_that_cannot_rebuild_frozen_original() {
        let direct_group = location(20, None);
        let direct_target = location(20, Some(0));
        let direct_role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("键应合法"));
        let direct = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                direct_target,
                "[Alice]",
                vec![
                    DirectTextPart::Literal("<".to_owned()),
                    DirectTextPart::TextSlot {
                        role: direct_role.clone(),
                    },
                    DirectTextPart::Literal(">".to_owned()),
                ],
            )
            .expect("形状合法但不能还原原文的直接配方应可进入快照边界"),
        );
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventCommand,
                direct_group,
                vec![
                    StandardWriteBackUnit::new(
                        direct_role,
                        TextUnitContent::Value("Alice".to_owned()),
                        None,
                    )
                    .expect("单元应合法")
                ],
                vec![direct.clone()],
                direct.mutation_targets(),
            ),
            Err(StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));

        let dialogue_group = location(30, None);
        let direct_speaker = DirectSpeakerTarget::new(location(30, Some(4)), "Alice");
        let dialogue = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                dialogue_group.clone(),
                Some(direct_speaker),
                vec![
                    DialogueLineRecipe::new(
                        location(31, Some(0)),
                        "Hello",
                        vec![DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        }],
                    )
                    .expect("正文配方应合法"),
                ],
            )
            .expect("对话配方形状应合法"),
        );
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                dialogue_group,
                vec![
                    StandardWriteBackUnit::new(
                        TextUnitRole::DialogueSpeaker,
                        TextUnitContent::Value("Bob".to_owned()),
                        None,
                    )
                    .expect("Speaker 单元应合法"),
                    StandardWriteBackUnit::new(
                        TextUnitRole::DialogueBody,
                        TextUnitContent::Lines(vec!["Hello".to_owned()]),
                        None,
                    )
                    .expect("Body 单元应合法"),
                ],
                vec![dialogue.clone()],
                dialogue.mutation_targets(),
            ),
            Err(StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));

        let trailing_group = location(40, None);
        let trailing = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                trailing_group.clone(),
                None,
                vec![
                    DialogueLineRecipe::new(
                        location(41, Some(0)),
                        "Hello",
                        vec![
                            DialogueLinePart::BodyLine {
                                source_line_index: 0,
                            },
                            DialogueLinePart::Literal(String::new()),
                        ],
                    )
                    .expect("模型边界允许由快照边界拒绝 Body 后缀"),
                ],
            )
            .expect("对话配方形状应合法"),
        );
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                trailing_group,
                vec![
                    StandardWriteBackUnit::new(
                        TextUnitRole::DialogueBody,
                        TextUnitContent::Lines(vec!["Hello".to_owned()]),
                        None,
                    )
                    .expect("Body 单元应合法")
                ],
                vec![trailing.clone()],
                trailing.mutation_targets(),
            ),
            Err(StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));
    }
}
