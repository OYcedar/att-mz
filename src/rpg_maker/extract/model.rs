//! RPG Maker 固定提取、规则提取与 Lua 提取共用的逻辑文本快照。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, LogicalTextLocation, MutationTarget, ProjectionModelError,
    ScalarFieldKey, TextFieldRole, TextProjectionRecipe,
};
pub(crate) use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};

/// 一个可独立继承、翻译或清除译文的逻辑文本叶。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextField {
    field_name: String,
    role: TextFieldRole,
    physical_location: RpgMakerLocation,
    original_text: String,
}

impl ExtractedTextField {
    /// 为现有 Builtin/Lua 字段建立强角色；字段名只保留为诊断展示。
    pub(crate) fn new(
        field_name: impl Into<String>,
        physical_location: RpgMakerLocation,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        let field_name = field_name.into();
        if field_name.is_empty() {
            return Err(SnapshotModelError::EmptyFieldName {
                exact_location: Box::new(physical_location),
            });
        }
        let role = role_from_field_name(&field_name)?;
        Self::with_role(field_name, role, physical_location, original_text)
    }

    /// 为投影器已经确定角色的逻辑叶建立受信字段。
    pub(crate) fn projected(
        role: TextFieldRole,
        physical_location: RpgMakerLocation,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        let field_name = role_field_name(&role);
        Self::with_role(field_name, role, physical_location, original_text)
    }

    fn with_role(
        field_name: String,
        role: TextFieldRole,
        physical_location: RpgMakerLocation,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        if field_name.is_empty() {
            return Err(SnapshotModelError::EmptyFieldName {
                exact_location: Box::new(physical_location),
            });
        }
        let original_text = original_text.into();
        if original_text.trim().is_empty() {
            return Err(SnapshotModelError::BlankOriginal {
                exact_location: Box::new(physical_location),
            });
        }
        Ok(Self {
            field_name,
            role,
            physical_location,
            original_text,
        })
    }

    #[cfg(test)]
    pub(crate) fn field_name(&self) -> &str {
        &self.field_name
    }

    #[cfg(test)]
    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.physical_location
    }

    pub(crate) fn role(&self) -> &TextFieldRole {
        &self.role
    }

    pub(crate) fn logical_location(
        &self,
        group_location: &RpgMakerLocation,
    ) -> LogicalTextLocation {
        LogicalTextLocation::new(group_location.clone(), self.role.clone())
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }
}

/// 一个会作为最小翻译上下文共同送给翻译器的复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    fields: Vec<ExtractedTextField>,
    mutation_targets: Vec<MutationTarget>,
    recipes: Vec<TextProjectionRecipe>,
}

impl ExtractedTextGroup {
    /// 为一个字段对应一个物理目标的既有 Builtin/Lua 组建立直接配方。
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        mut fields: Vec<ExtractedTextField>,
    ) -> Result<Self, SnapshotModelError> {
        if fields.is_empty() {
            return Err(SnapshotModelError::EmptyGroup {
                group_location: Box::new(group_location),
            });
        }
        if kind == TextGroupKind::EventScrollingText {
            for field in &mut fields {
                if let TextFieldRole::DialogueBody { index } = field.role {
                    field.role = TextFieldRole::ScrollingTextBody { index };
                }
            }
        }
        let recipes = fields
            .iter()
            .map(|field| {
                DirectTextRecipe::new(
                    field.physical_location.clone(),
                    field.original_text.clone(),
                    vec![DirectTextPart::TextSlot {
                        role: field.role.clone(),
                    }],
                )
                .map(TextProjectionRecipe::Direct)
                .map_err(SnapshotModelError::Projection)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::projected(kind, group_location, fields, recipes)
    }

    /// 为同一物理目标可以包含多个逻辑槽的物化投影建立文本组。
    pub(crate) fn projected(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        mut fields: Vec<ExtractedTextField>,
        recipes: Vec<TextProjectionRecipe>,
    ) -> Result<Self, SnapshotModelError> {
        if recipes.is_empty() {
            return Err(SnapshotModelError::EmptyProjection {
                group_location: Box::new(group_location),
            });
        }
        fields.sort_by(|left, right| {
            left.role
                .cmp(&right.role)
                .then_with(|| left.physical_location.cmp(&right.physical_location))
        });

        let mut roles = BTreeSet::new();
        for field in &fields {
            if !roles.insert(field.role.clone()) {
                return Err(SnapshotModelError::DuplicateLogicalLocation {
                    logical_location: Box::new(field.logical_location(&group_location)),
                });
            }
        }

        let referenced_roles = recipes
            .iter()
            .flat_map(TextProjectionRecipe::referenced_roles)
            .collect::<BTreeSet<_>>();
        if roles != referenced_roles {
            return Err(SnapshotModelError::RecipeRoleMismatch {
                group_location: Box::new(group_location),
                leaves: roles,
                referenced: referenced_roles,
            });
        }

        let mut mutation_targets = recipes
            .iter()
            .flat_map(TextProjectionRecipe::mutation_targets)
            .collect::<Vec<_>>();
        mutation_targets.sort();
        for pair in mutation_targets.windows(2) {
            if pair[0] == pair[1] {
                return Err(SnapshotModelError::DuplicateMutationTarget {
                    target: Box::new(pair[0].clone()),
                });
            }
        }

        Ok(Self {
            kind,
            group_location,
            fields,
            mutation_targets,
            recipes,
        })
    }

    pub(crate) fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn fields(&self) -> &[ExtractedTextField] {
        &self.fields
    }

    pub(crate) fn mutation_targets(&self) -> &[MutationTarget] {
        &self.mutation_targets
    }

    pub(crate) fn recipes(&self) -> &[TextProjectionRecipe] {
        &self.recipes
    }
}

/// 一个标准资产 owner 拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardAssetSnapshot(Vec<ExtractedTextGroup>);

impl StandardAssetSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        normalize_groups(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        &self.0
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuiltinSnapshot(StandardAssetSnapshot);

impl BuiltinSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RulesSnapshot(StandardAssetSnapshot);

impl RulesSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(StandardAssetSnapshot(Vec::new()))
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LuaSnapshot(StandardAssetSnapshot);

impl LuaSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(StandardAssetSnapshot(Vec::new()))
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
    }
}

fn normalize_groups(
    groups: Vec<ExtractedTextGroup>,
) -> Result<Vec<ExtractedTextGroup>, SnapshotModelError> {
    let mut merged = BTreeMap::<
        (RpgMakerLocation, TextGroupKind),
        (Vec<ExtractedTextField>, Vec<TextProjectionRecipe>),
    >::new();
    for group in groups {
        let entry = merged
            .entry((group.group_location, group.kind))
            .or_default();
        entry.0.extend(group.fields);
        entry.1.extend(group.recipes);
    }

    let mut groups = Vec::with_capacity(merged.len());
    for ((group_location, kind), (fields, recipes)) in merged {
        groups.push(ExtractedTextGroup::projected(
            kind,
            group_location,
            fields,
            recipes,
        )?);
    }

    let mut logical_locations = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for group in &groups {
        for field in &group.fields {
            let logical_location = field.logical_location(&group.group_location);
            if !logical_locations.insert(logical_location.clone()) {
                return Err(SnapshotModelError::DuplicateLogicalLocation {
                    logical_location: Box::new(logical_location),
                });
            }
        }
        for target in &group.mutation_targets {
            if !targets.insert(target.clone()) {
                return Err(SnapshotModelError::DuplicateMutationTarget {
                    target: Box::new(target.clone()),
                });
            }
        }
    }
    Ok(groups)
}

fn role_from_field_name(field_name: &str) -> Result<TextFieldRole, SnapshotModelError> {
    if field_name == "speaker" {
        return Ok(TextFieldRole::DialogueSpeaker);
    }
    if let Some(index) = parse_indexed_field(field_name, "body") {
        return Ok(TextFieldRole::DialogueBody { index });
    }
    ScalarFieldKey::new(field_name)
        .map(TextFieldRole::Scalar)
        .map_err(SnapshotModelError::Projection)
}

fn parse_indexed_field(value: &str, prefix: &str) -> Option<usize> {
    value
        .strip_prefix(prefix)?
        .strip_prefix('[')?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn role_field_name(role: &TextFieldRole) -> String {
    match role {
        TextFieldRole::Scalar(key) => key.as_str().to_owned(),
        TextFieldRole::DialogueSpeaker => "speaker".to_owned(),
        TextFieldRole::DialogueBody { index } | TextFieldRole::ScrollingTextBody { index } => {
            format!("body[{index}]")
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotModelError {
    EmptyFieldName {
        exact_location: Box<RpgMakerLocation>,
    },
    BlankOriginal {
        exact_location: Box<RpgMakerLocation>,
    },
    EmptyGroup {
        group_location: Box<RpgMakerLocation>,
    },
    EmptyProjection {
        group_location: Box<RpgMakerLocation>,
    },
    DuplicateLogicalLocation {
        logical_location: Box<LogicalTextLocation>,
    },
    DuplicateMutationTarget {
        target: Box<MutationTarget>,
    },
    RecipeRoleMismatch {
        group_location: Box<RpgMakerLocation>,
        leaves: BTreeSet<TextFieldRole>,
        referenced: BTreeSet<TextFieldRole>,
    },
    Projection(ProjectionModelError),
}

impl fmt::Display for SnapshotModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFieldName { exact_location } => {
                write!(formatter, "文本字段名为空：{exact_location}")
            }
            Self::BlankOriginal { exact_location } => {
                write!(formatter, "纯空白原文不应进入逻辑叶：{exact_location}")
            }
            Self::EmptyGroup { group_location } => {
                write!(formatter, "复合文本组不包含任何文本：{group_location}")
            }
            Self::EmptyProjection { group_location } => {
                write!(formatter, "复合文本组没有物化写回配方：{group_location}")
            }
            Self::DuplicateLogicalLocation { logical_location } => {
                write!(formatter, "快照包含重复逻辑文本地址：{logical_location:?}")
            }
            Self::DuplicateMutationTarget { target } => {
                write!(formatter, "快照包含重复物理修改目标：{target:?}")
            }
            Self::RecipeRoleMismatch {
                group_location,
                leaves,
                referenced,
            } => write!(
                formatter,
                "组 {group_location} 的逻辑叶与写回配方引用不一致：leaves={leaves:?}, referenced={referenced:?}"
            ),
            Self::Projection(source) => write!(formatter, "文本投影无效：{source}"),
        }
    }
}

impl Error for SnapshotModelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::model::{DirectTextPart, DirectTextRecipe};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};

    #[test]
    fn snapshot_uses_roles_for_order_and_rejects_duplicate_physical_targets() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("text"),
            ],
        );
        let field =
            ExtractedTextField::new("speaker", location.clone(), "角色").expect("非空字段应该合法");
        let first = ExtractedTextGroup::new(
            TextGroupKind::EventDialogue,
            group_location.clone(),
            vec![field.clone()],
        )
        .expect("组应合法");
        let second =
            ExtractedTextGroup::new(TextGroupKind::EventDialogue, group_location, vec![field])
                .expect("单个组应合法");

        assert!(matches!(
            BuiltinSnapshot::new(vec![first, second]),
            Err(SnapshotModelError::DuplicateLogicalLocation { .. })
                | Err(SnapshotModelError::DuplicateMutationTarget { .. })
        ));
    }

    #[test]
    fn one_physical_string_can_project_multiple_scalar_leaves() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let target = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("note"),
            ],
        );
        let left_role = TextFieldRole::Scalar(ScalarFieldKey::new("match[0]").expect("键应合法"));
        let right_role = TextFieldRole::Scalar(ScalarFieldKey::new("match[1]").expect("键应合法"));
        let fields = vec![
            ExtractedTextField::projected(left_role.clone(), target.clone(), "甲")
                .expect("叶应合法"),
            ExtractedTextField::projected(right_role.clone(), target.clone(), "乙")
                .expect("叶应合法"),
        ];
        let recipe = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                target,
                "<x>甲</x><x>乙</x>",
                vec![
                    DirectTextPart::Literal("<x>".to_owned()),
                    DirectTextPart::TextSlot { role: left_role },
                    DirectTextPart::Literal("</x><x>".to_owned()),
                    DirectTextPart::TextSlot { role: right_role },
                    DirectTextPart::Literal("</x>".to_owned()),
                ],
            )
            .expect("多槽配方应合法"),
        );

        let group = ExtractedTextGroup::projected(
            TextGroupKind::DatabaseEntry,
            group_location,
            fields,
            vec![recipe],
        )
        .expect("同址多逻辑叶应合法");

        assert_eq!(group.fields().len(), 2);
        assert_eq!(group.mutation_targets().len(), 1);
    }
}
