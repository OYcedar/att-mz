//! RPG Maker 固定提取、规则提取与 Lua 提取共用的语义文本快照。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, LogicalTextLocation, MutationClaim, MutationClaimSet,
    MutationResource, ProjectionModelError, ScalarFieldKey, TextProjectionRecipe, TextUnitContent,
    TextUnitRole, mutation_claims_for_group,
};
pub(crate) use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};

/// 一个可独立继承、翻译、验收和写回的语义文本单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextUnit {
    role: TextUnitRole,
    projection_location: RpgMakerLocation,
    mutation_claim: MutationClaim,
    source_content: TextUnitContent,
}

impl ExtractedTextUnit {
    /// 为 Builtin、规则或 Lua 的普通单值字段建立标量单元。
    pub(crate) fn new(
        field_name: impl Into<String>,
        projection_location: RpgMakerLocation,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        let role = ScalarFieldKey::new(field_name)
            .map(TextUnitRole::Scalar)
            .map_err(SnapshotModelError::Projection)?;
        Self::projected(
            role,
            projection_location,
            TextUnitContent::Value(original_text.into()),
        )
    }

    pub(crate) fn new_with_claim(
        field_name: impl Into<String>,
        projection_location: RpgMakerLocation,
        mutation_claim: MutationClaim,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        let role = ScalarFieldKey::new(field_name)
            .map(TextUnitRole::Scalar)
            .map_err(SnapshotModelError::Projection)?;
        Self::projected_with_claim(
            role,
            projection_location,
            mutation_claim,
            TextUnitContent::Value(original_text.into()),
        )
    }

    /// 为投影器已经确定角色和完整内容的语义单元建立受信快照。
    pub(crate) fn projected(
        role: TextUnitRole,
        projection_location: RpgMakerLocation,
        source_content: TextUnitContent,
    ) -> Result<Self, SnapshotModelError> {
        let mutation_claim = MutationClaim::for_location(projection_location.clone())
            .map_err(SnapshotModelError::Projection)?;
        Self::projected_with_claim(role, projection_location, mutation_claim, source_content)
    }

    pub(crate) fn projected_with_claim(
        role: TextUnitRole,
        projection_location: RpgMakerLocation,
        mutation_claim: MutationClaim,
        source_content: TextUnitContent,
    ) -> Result<Self, SnapshotModelError> {
        if mutation_claim.representative_location() != &projection_location {
            return Err(SnapshotModelError::Projection(
                ProjectionModelError::MutationClaimTargetMismatch,
            ));
        }
        let actual_lines = matches!(source_content, TextUnitContent::Lines(_));
        if role.expects_lines() != actual_lines {
            return Err(SnapshotModelError::ContentShapeMismatch {
                role,
                exact_location: Box::new(projection_location),
            });
        }
        if let TextUnitContent::Lines(lines) = &source_content
            && let Some(source_line_index) = lines.iter().position(|line| {
                line.chars()
                    .any(|character| matches!(character, '\r' | '\n' | '\0'))
            })
        {
            return Err(SnapshotModelError::InvalidSourceLine {
                source_line_index,
                exact_location: Box::new(projection_location),
            });
        }
        if source_content.is_blank() {
            return Err(SnapshotModelError::BlankSourceContent {
                exact_location: Box::new(projection_location),
            });
        }
        Ok(Self {
            role,
            projection_location,
            mutation_claim,
            source_content,
        })
    }

    pub(crate) fn role(&self) -> &TextUnitRole {
        &self.role
    }

    pub(crate) fn logical_location(
        &self,
        group_location: &RpgMakerLocation,
    ) -> LogicalTextLocation {
        LogicalTextLocation::new(group_location.clone(), self.role.clone())
    }

    pub(crate) fn source_content(&self) -> &TextUnitContent {
        &self.source_content
    }
}

/// 一个会作为最小翻译上下文共同送给翻译器的复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<ExtractedTextUnit>,
    mutation_claims: MutationClaimSet,
    recipes: Vec<TextProjectionRecipe>,
}

impl ExtractedTextGroup {
    /// 为一个单值单元对应一个物理目标的 Builtin、规则或 Lua 组建立直接配方。
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<ExtractedTextUnit>,
    ) -> Result<Self, SnapshotModelError> {
        if units.is_empty() {
            return Err(SnapshotModelError::EmptyGroup {
                group_location: Box::new(group_location),
            });
        }
        let recipes = units
            .iter()
            .map(|unit| {
                let source = unit.source_content.as_value().ok_or_else(|| {
                    SnapshotModelError::DirectGroupRequiresValue {
                        role: unit.role.clone(),
                        exact_location: Box::new(unit.projection_location.clone()),
                    }
                })?;
                DirectTextRecipe::new_with_claim(
                    unit.projection_location.clone(),
                    unit.mutation_claim.clone(),
                    source,
                    vec![DirectTextPart::TextSlot {
                        role: unit.role.clone(),
                    }],
                )
                .map(TextProjectionRecipe::Direct)
                .map_err(SnapshotModelError::Projection)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::projected(kind, group_location, units, recipes)
    }

    /// 为一个语义单元可以投影到多个物理目标的组建立完整快照。
    pub(crate) fn projected(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<ExtractedTextUnit>,
        recipes: Vec<TextProjectionRecipe>,
    ) -> Result<Self, SnapshotModelError> {
        if units.is_empty() {
            return Err(SnapshotModelError::EmptyGroup {
                group_location: Box::new(group_location),
            });
        }
        if recipes.is_empty() {
            return Err(SnapshotModelError::EmptyProjection {
                group_location: Box::new(group_location),
            });
        }
        let mut roles = BTreeSet::new();
        for unit in &units {
            if !roles.insert(unit.role.clone()) {
                return Err(SnapshotModelError::DuplicateLogicalLocation {
                    logical_location: Box::new(unit.logical_location(&group_location)),
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
                units: roles,
                referenced: referenced_roles,
            });
        }
        validate_line_references(&group_location, &units, &recipes)?;

        let mutation_claims =
            mutation_claims_for_group(kind, &group_location, &recipes).map_err(|conflict| {
                SnapshotModelError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                }
            })?;

        Ok(Self {
            kind,
            group_location,
            units,
            mutation_claims,
            recipes,
        })
    }

    pub(crate) fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn units(&self) -> &[ExtractedTextUnit] {
        &self.units
    }

    pub(crate) fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }

    pub(crate) fn recipes(&self) -> &[TextProjectionRecipe] {
        &self.recipes
    }
}

fn validate_line_references(
    group_location: &RpgMakerLocation,
    units: &[ExtractedTextUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), SnapshotModelError> {
    let mut referenced = BTreeMap::<TextUnitRole, BTreeSet<usize>>::new();
    for (role, source_line_index) in recipes
        .iter()
        .flat_map(TextProjectionRecipe::referenced_lines)
    {
        referenced
            .entry(role)
            .or_default()
            .insert(source_line_index);
    }

    for unit in units {
        let actual = referenced.remove(unit.role()).unwrap_or_default();
        let expected = match unit.source_content() {
            TextUnitContent::Value(_) => BTreeSet::new(),
            TextUnitContent::Lines(lines) => (0..lines.len()).collect(),
        };
        if actual != expected {
            return Err(SnapshotModelError::RecipeLineMismatch {
                group_location: Box::new(group_location.clone()),
                role: unit.role().clone(),
                expected,
                referenced: actual,
            });
        }
    }
    debug_assert!(referenced.is_empty(), "角色集合已经在调用前验证一致");
    Ok(())
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
    let mut merged = Vec::<(
        (RpgMakerLocation, TextGroupKind),
        (Vec<ExtractedTextUnit>, Vec<TextProjectionRecipe>),
    )>::new();
    let mut indexes = BTreeMap::<(RpgMakerLocation, TextGroupKind), usize>::new();
    for group in groups {
        let key = (group.group_location, group.kind);
        let index = if let Some(index) = indexes.get(&key).copied() {
            index
        } else {
            let index = merged.len();
            indexes.insert(key.clone(), index);
            merged.push((key, (Vec::new(), Vec::new())));
            index
        };
        let entry = &mut merged[index].1;
        entry.0.extend(group.units);
        entry.1.extend(group.recipes);
    }

    let mut groups = Vec::with_capacity(merged.len());
    for ((group_location, kind), (units, recipes)) in merged {
        groups.push(ExtractedTextGroup::projected(
            kind,
            group_location,
            units,
            recipes,
        )?);
    }

    let mut logical_locations = BTreeSet::new();
    let mut claim_sets = Vec::<&MutationClaimSet>::new();
    for group in &groups {
        for unit in &group.units {
            let logical_location = unit.logical_location(&group.group_location);
            if !logical_locations.insert(logical_location.clone()) {
                return Err(SnapshotModelError::DuplicateLogicalLocation {
                    logical_location: Box::new(logical_location),
                });
            }
        }
        for existing in &claim_sets {
            if let Some(conflict) = existing.conflict_with(&group.mutation_claims) {
                return Err(SnapshotModelError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                });
            }
        }
        claim_sets.push(&group.mutation_claims);
    }
    Ok(groups)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotModelError {
    BlankSourceContent {
        exact_location: Box<RpgMakerLocation>,
    },
    ContentShapeMismatch {
        role: TextUnitRole,
        exact_location: Box<RpgMakerLocation>,
    },
    DirectGroupRequiresValue {
        role: TextUnitRole,
        exact_location: Box<RpgMakerLocation>,
    },
    InvalidSourceLine {
        source_line_index: usize,
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
    MutationClaimConflict {
        resource: Box<MutationResource>,
    },
    RecipeRoleMismatch {
        group_location: Box<RpgMakerLocation>,
        units: BTreeSet<TextUnitRole>,
        referenced: BTreeSet<TextUnitRole>,
    },
    RecipeLineMismatch {
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
        expected: BTreeSet<usize>,
        referenced: BTreeSet<usize>,
    },
    Projection(ProjectionModelError),
}

impl fmt::Display for SnapshotModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankSourceContent { exact_location } => {
                write!(
                    formatter,
                    "纯空白原文不应进入语义文本单元：{exact_location}"
                )
            }
            Self::ContentShapeMismatch {
                role,
                exact_location,
            } => write!(
                formatter,
                "语义角色 {role:?} 与原文内容形状不一致：{exact_location}"
            ),
            Self::DirectGroupRequiresValue {
                role,
                exact_location,
            } => write!(
                formatter,
                "直接文本组中的 {role:?} 必须是单值内容：{exact_location}"
            ),
            Self::InvalidSourceLine {
                source_line_index,
                exact_location,
            } => write!(
                formatter,
                "语义行 {source_line_index} 含有 CR、LF 或 NUL：{exact_location}"
            ),
            Self::EmptyGroup { group_location } => {
                write!(
                    formatter,
                    "复合文本组不包含任何语义文本单元：{group_location}"
                )
            }
            Self::EmptyProjection { group_location } => {
                write!(formatter, "复合文本组没有物化写回配方：{group_location}")
            }
            Self::DuplicateLogicalLocation { logical_location } => {
                write!(formatter, "快照包含重复逻辑文本地址：{logical_location:?}")
            }
            Self::MutationClaimConflict { resource } => {
                write!(formatter, "快照包含冲突的物理修改声明：{resource:?}")
            }
            Self::RecipeRoleMismatch {
                group_location,
                units,
                referenced,
            } => write!(
                formatter,
                "组 {group_location} 的语义单元与写回配方引用不一致：units={units:?}, referenced={referenced:?}"
            ),
            Self::RecipeLineMismatch {
                group_location,
                role,
                expected,
                referenced,
            } => write!(
                formatter,
                "组 {group_location} 的 {role:?} 行引用不完整：expected={expected:?}, referenced={referenced:?}"
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
    fn snapshot_preserves_declared_order_and_rejects_duplicate_physical_claims() {
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
        let unit = ExtractedTextUnit::projected(
            TextUnitRole::DialogueSpeaker,
            location.clone(),
            TextUnitContent::Value("角色".to_owned()),
        )
        .expect("非空单元应该合法");
        let first = ExtractedTextGroup::new(
            TextGroupKind::EventDialogue,
            group_location.clone(),
            vec![unit.clone()],
        )
        .expect("组应合法");
        let second =
            ExtractedTextGroup::new(TextGroupKind::EventDialogue, group_location, vec![unit])
                .expect("单个组应合法");

        assert!(matches!(
            BuiltinSnapshot::new(vec![first, second]),
            Err(SnapshotModelError::DuplicateLogicalLocation { .. })
                | Err(SnapshotModelError::MutationClaimConflict { .. })
        ));
    }

    #[test]
    fn raw_and_decoded_claims_fail_within_group_and_owner_while_siblings_coexist() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let raw = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("payload"),
            ],
        );
        let decoded = |key: &str| {
            RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key("payload"),
                    RpgMakerLocationStep::DecodeJsonString,
                    RpgMakerLocationStep::key(key),
                ],
            )
        };
        let unit = |field: &str, location: RpgMakerLocation| {
            ExtractedTextUnit::new(field, location, "原文").expect("测试单元应合法")
        };
        let group_location = |index| {
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(index)])
        };

        assert!(matches!(
            ExtractedTextGroup::new(
                TextGroupKind::DatabaseEntry,
                group_location(10),
                vec![unit("raw", raw.clone()), unit("decoded", decoded("left"))],
            ),
            Err(SnapshotModelError::MutationClaimConflict { .. })
        ));

        let raw_group = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location(11),
            vec![unit("raw", raw)],
        )
        .expect("raw 组本身应合法");
        let decoded_left_group = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location(12),
            vec![unit("decoded", decoded("left"))],
        )
        .expect("decoded 组本身应合法");
        assert!(matches!(
            RulesSnapshot::new(vec![raw_group, decoded_left_group]),
            Err(SnapshotModelError::MutationClaimConflict { .. })
        ));

        let decoded_left_group = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location(13),
            vec![unit("left", decoded("left"))],
        )
        .expect("decoded left 组应合法");
        let decoded_right_group = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location(14),
            vec![unit("right", decoded("right"))],
        )
        .expect("decoded right 组应合法");
        RulesSnapshot::new(vec![decoded_left_group, decoded_right_group])
            .expect("decoded siblings 只能共享 Intent，必须允许同 owner 共存");
    }

    #[test]
    fn one_physical_string_can_project_multiple_scalar_units() {
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
        let left_role = TextUnitRole::Scalar(ScalarFieldKey::new("match[0]").expect("键应合法"));
        let right_role = TextUnitRole::Scalar(ScalarFieldKey::new("match[1]").expect("键应合法"));
        let units = vec![
            ExtractedTextUnit::projected(
                left_role.clone(),
                target.clone(),
                TextUnitContent::Value("甲".to_owned()),
            )
            .expect("单元应合法"),
            ExtractedTextUnit::projected(
                right_role.clone(),
                target.clone(),
                TextUnitContent::Value("乙".to_owned()),
            )
            .expect("单元应合法"),
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
            units,
            vec![recipe],
        )
        .expect("同址多语义单元应合法");

        assert_eq!(group.units().len(), 2);
        assert_eq!(group.mutation_claims().claims().len(), 1);
    }

    #[test]
    fn line_content_requires_every_source_index_in_the_recipe() {
        let source = RpgMakerSource::map(1);
        let group_location = RpgMakerLocation::value(source.clone(), Vec::new());
        let target = RpgMakerLocation::value(source, vec![RpgMakerLocationStep::index(1)]);
        let unit = ExtractedTextUnit::projected(
            TextUnitRole::Choices,
            group_location.clone(),
            TextUnitContent::Lines(vec!["是".to_owned(), "否".to_owned()]),
        )
        .expect("选项单元应合法");
        let recipe = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                target,
                "是",
                vec![DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index: 0,
                }],
            )
            .expect("首个选项配方应合法"),
        );

        assert!(matches!(
            ExtractedTextGroup::projected(
                TextGroupKind::EventChoices,
                group_location,
                vec![unit],
                vec![recipe]
            ),
            Err(SnapshotModelError::RecipeLineMismatch { .. })
        ));
    }

    #[test]
    fn line_content_rejects_embedded_line_breaks_and_nul() {
        let location = RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new());
        for invalid in ["一\n二", "一\r二", "一\0二"] {
            assert!(matches!(
                ExtractedTextUnit::projected(
                    TextUnitRole::DialogueBody,
                    location.clone(),
                    TextUnitContent::Lines(vec![invalid.to_owned()]),
                ),
                Err(SnapshotModelError::InvalidSourceLine {
                    source_line_index: 0,
                    ..
                })
            ));
        }
    }
}
