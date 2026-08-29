//! RPG Maker 固定提取与规则提取共用的语义文本快照。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, RpgMakerExtractionProblem, RpgMakerExtractionSemanticOrderKey,
    RpgMakerExtractionSemanticOrderProjectionViolation, RpgMakerExtractionSnapshotViolation,
    RpgMakerExtractionSource, RpgMakerIssue, StateEffect,
};
use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, LogicalTextLocation, MutationClaim, MutationClaimIndex,
    MutationClaimSet, ProjectionModelError, ScalarFieldKey, TextProjectionRecipe, TextUnitContent,
    TextUnitContentStructureError, TextUnitContentView, TextUnitRole, mutation_claims_for_group,
    validate_text_unit_content_structure,
};
use crate::rpg_maker::semantic_order::{
    RpgMakerSemanticOrderKey, RpgMakerSemanticOrderProjectionError,
};
pub(crate) use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};

/// 一个可独立继承、翻译、验收和写回的语义文本单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextUnit {
    role: TextUnitRole,
    projection_location: RpgMakerLocation,
    semantic_order_key: RpgMakerSemanticOrderKey,
    rule_number: Option<usize>,
    mutation_claim: MutationClaim,
    source_content: TextUnitContent,
}

impl ExtractedTextUnit {
    /// 为 Builtin 或规则的普通单值字段建立标量单元。
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

    /// 为投影器已经确定角色和完整内容的语义单元建立受信快照。
    pub(crate) fn projected(
        role: TextUnitRole,
        projection_location: RpgMakerLocation,
        source_content: TextUnitContent,
    ) -> Result<Self, SnapshotModelError> {
        let mutation_claim = MutationClaim::for_location(projection_location.clone());
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
        if source_content.is_blank() {
            return Err(SnapshotModelError::BlankSourceContent {
                exact_location: Box::new(projection_location),
            });
        }
        let semantic_order_key =
            RpgMakerSemanticOrderKey::from_unit_location(&projection_location, &role);
        Ok(Self {
            role,
            projection_location,
            semantic_order_key,
            rule_number: None,
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

    pub(crate) fn projection_location(&self) -> &RpgMakerLocation {
        &self.projection_location
    }

    pub(crate) fn semantic_order_key(&self) -> &RpgMakerSemanticOrderKey {
        &self.semantic_order_key
    }

    pub(crate) fn set_semantic_order_key(&mut self, semantic_order_key: RpgMakerSemanticOrderKey) {
        self.semantic_order_key = semantic_order_key;
    }

    /// 保存产生 Rules Unit 的 TOML 自然序号；Builtin Unit 始终保持为空。
    pub(crate) fn set_rule_number(&mut self, rule_number: usize) {
        assert!(rule_number > 0, "Rules 自然序号必须从 1 开始");
        assert!(
            self.rule_number.is_none(),
            "Rules Unit 不能重复设置来源规则"
        );
        self.rule_number = Some(rule_number);
    }

    pub(crate) const fn rule_number(&self) -> Option<usize> {
        self.rule_number
    }
}

/// 一个会作为最小翻译上下文共同送给翻译器的复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextGroup {
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    semantic_order_key: RpgMakerSemanticOrderKey,
    units: Vec<ExtractedTextUnit>,
    mutation_claims: MutationClaimSet,
    recipes: Vec<TextProjectionRecipe>,
}

impl ExtractedTextGroup {
    /// 为一个单值单元对应一个物理目标的 Builtin 或规则组建立直接配方。
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
        for unit in &units {
            validate_text_unit_content_structure(
                kind,
                &unit.role,
                TextUnitContentView::from(&unit.source_content),
            )
            .map_err(|error| match error {
                TextUnitContentStructureError::KindRoleMismatch => {
                    SnapshotModelError::ContentShapeMismatch {
                        role: unit.role.clone(),
                        exact_location: Box::new(unit.projection_location.clone()),
                    }
                }
                TextUnitContentStructureError::ShapeMismatch => {
                    SnapshotModelError::ContentShapeMismatch {
                        role: unit.role.clone(),
                        exact_location: Box::new(unit.projection_location.clone()),
                    }
                }
                TextUnitContentStructureError::InvalidText { line_index } => {
                    SnapshotModelError::InvalidSourceLine {
                        source_line_index: line_index,
                        exact_location: Box::new(unit.projection_location.clone()),
                    }
                }
            })?;
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

        let semantic_order_key = RpgMakerSemanticOrderKey::from_group_location(&group_location);
        Ok(Self {
            kind,
            group_location,
            semantic_order_key,
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

    pub(crate) fn semantic_order_key(&self) -> &RpgMakerSemanticOrderKey {
        &self.semantic_order_key
    }

    pub(crate) fn set_semantic_order_key(&mut self, semantic_order_key: RpgMakerSemanticOrderKey) {
        self.semantic_order_key = semantic_order_key;
    }

    pub(crate) fn units_mut(&mut self) -> &mut [ExtractedTextUnit] {
        &mut self.units
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

/// 一个 RPG Maker 资产 owner 拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerAssetSnapshot(Vec<ExtractedTextGroup>);

impl RpgMakerAssetSnapshot {
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
pub(crate) struct BuiltinSnapshot(RpgMakerAssetSnapshot);

impl BuiltinSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        RpgMakerAssetSnapshot::new(groups).map(Self)
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
pub(crate) struct RulesSnapshot(RpgMakerAssetSnapshot);

impl RulesSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        RpgMakerAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(RpgMakerAssetSnapshot(Vec::new()))
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
    normalize_groups_with_rebuild_observer(groups, || {})
}

enum NormalizedGroupSlot {
    Complete(Option<ExtractedTextGroup>),
    Merged {
        group_location: RpgMakerLocation,
        semantic_order_key: RpgMakerSemanticOrderKey,
        kind: TextGroupKind,
        units: Vec<ExtractedTextUnit>,
        recipes: Vec<TextProjectionRecipe>,
    },
}

fn normalize_groups_with_rebuild_observer(
    input: Vec<ExtractedTextGroup>,
    mut observe_rebuild: impl FnMut(),
) -> Result<Vec<ExtractedTextGroup>, SnapshotModelError> {
    let mut merged = Vec::<NormalizedGroupSlot>::with_capacity(input.len());
    let mut indexes = HashMap::<RpgMakerLocation, usize>::new();
    for group in input {
        if let Some(index) = indexes.get(&group.group_location).copied() {
            let slot = &mut merged[index];
            match slot {
                NormalizedGroupSlot::Complete(existing) => {
                    let existing_kind = existing
                        .as_ref()
                        .expect("尚未归并的完整文本组必须存在")
                        .kind;
                    if existing_kind != group.kind {
                        return Err(SnapshotModelError::ConflictingGroupKind {
                            group_location: Box::new(group.group_location),
                            first: existing_kind,
                            second: group.kind,
                        });
                    }
                    let existing_semantic_order_key = &existing
                        .as_ref()
                        .expect("尚未归并的完整文本组必须存在")
                        .semantic_order_key;
                    if existing_semantic_order_key != &group.semantic_order_key {
                        return Err(SnapshotModelError::ConflictingSemanticOrderKey {
                            group_location: Box::new(group.group_location),
                            first: existing_semantic_order_key.clone(),
                            second: group.semantic_order_key,
                        });
                    }
                    let ExtractedTextGroup {
                        kind,
                        group_location,
                        semantic_order_key,
                        mut units,
                        mutation_claims: _,
                        mut recipes,
                    } = existing.take().expect("完整文本组只会在首次重复时拆解");
                    units.extend(group.units);
                    recipes.extend(group.recipes);
                    *slot = NormalizedGroupSlot::Merged {
                        group_location,
                        semantic_order_key,
                        kind,
                        units,
                        recipes,
                    };
                }
                NormalizedGroupSlot::Merged {
                    group_location,
                    semantic_order_key,
                    kind,
                    units,
                    recipes,
                } => {
                    if *kind != group.kind {
                        return Err(SnapshotModelError::ConflictingGroupKind {
                            group_location: Box::new(group.group_location),
                            first: *kind,
                            second: group.kind,
                        });
                    }
                    if *semantic_order_key != group.semantic_order_key {
                        return Err(SnapshotModelError::ConflictingSemanticOrderKey {
                            group_location: Box::new(group.group_location),
                            first: semantic_order_key.clone(),
                            second: group.semantic_order_key,
                        });
                    }
                    debug_assert_eq!(*group_location, group.group_location);
                    units.extend(group.units);
                    recipes.extend(group.recipes);
                }
            }
        } else {
            let index = merged.len();
            indexes.insert(group.group_location.clone(), index);
            merged.push(NormalizedGroupSlot::Complete(Some(group)));
        }
    }

    let mut groups = Vec::with_capacity(merged.len());
    let mut total_claim_locks = 0usize;
    for slot in merged {
        let group = match slot {
            NormalizedGroupSlot::Complete(group) => group.expect("未重复文本组必须保持完整"),
            NormalizedGroupSlot::Merged {
                group_location,
                semantic_order_key,
                kind,
                units,
                recipes,
            } => {
                observe_rebuild();
                let mut group =
                    ExtractedTextGroup::projected(kind, group_location, units, recipes)?;
                group.set_semantic_order_key(semantic_order_key);
                group
            }
        };
        total_claim_locks = total_claim_locks
            .checked_add(group.mutation_claims.locks().len())
            .expect("内存中的 Mutation Claim 锁总数必须可用 usize 表达");
        groups.push(group);
    }

    let mut claim_index = MutationClaimIndex::with_capacity(total_claim_locks);
    for group in &groups {
        // group_location 已在上面的唯一索引中归并；每个完整或重建 Group 又由
        // `ExtractedTextGroup::projected` 验证角色唯一，因此跨组逻辑位置不可能重复。
        claim_index
            .insert(&group.mutation_claims)
            .map_err(|conflict| SnapshotModelError::MutationClaimConflict {
                resource: Box::new(conflict.resource().clone()),
            })?;
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
    ConflictingGroupKind {
        group_location: Box<RpgMakerLocation>,
        first: TextGroupKind,
        second: TextGroupKind,
    },
    ConflictingSemanticOrderKey {
        group_location: Box<RpgMakerLocation>,
        first: RpgMakerSemanticOrderKey,
        second: RpgMakerSemanticOrderKey,
    },
    SemanticOrderProjection {
        exact_location: Box<RpgMakerLocation>,
        source: RpgMakerSemanticOrderProjectionError,
    },
    MutationClaimConflict {
        resource: Box<RpgMakerLocation>,
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

impl SnapshotModelError {
    /// 在 Extract 快照边界保留 owner、精确位置、角色与全部数值事实。
    pub(crate) fn diagnostic_report(&self, source: RpgMakerExtractionSource) -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::extraction(
                RpgMakerExtractionProblem::Snapshot {
                    source,
                    violation: self.diagnostic_violation(),
                },
            )),
        )
    }

    pub(crate) fn diagnostic_violation(&self) -> RpgMakerExtractionSnapshotViolation {
        match self {
            Self::BlankSourceContent { exact_location } => {
                RpgMakerExtractionSnapshotViolation::BlankSourceContent {
                    location: exact_location.diagnostic_location(),
                }
            }
            Self::ContentShapeMismatch {
                role,
                exact_location,
            } => RpgMakerExtractionSnapshotViolation::ContentShapeMismatch {
                role: role.diagnostic_role(),
                location: exact_location.diagnostic_location(),
            },
            Self::DirectGroupRequiresValue {
                role,
                exact_location,
            } => RpgMakerExtractionSnapshotViolation::DirectGroupRequiresValue {
                role: role.diagnostic_role(),
                location: exact_location.diagnostic_location(),
            },
            Self::InvalidSourceLine {
                source_line_index,
                exact_location,
            } => RpgMakerExtractionSnapshotViolation::InvalidSourceLine {
                source_line_index: *source_line_index,
                location: exact_location.diagnostic_location(),
            },
            Self::EmptyGroup { group_location } => {
                RpgMakerExtractionSnapshotViolation::EmptyGroup {
                    group_location: group_location.diagnostic_location(),
                }
            }
            Self::EmptyProjection { group_location } => {
                RpgMakerExtractionSnapshotViolation::EmptyProjection {
                    group_location: group_location.diagnostic_location(),
                }
            }
            Self::DuplicateLogicalLocation { logical_location } => {
                RpgMakerExtractionSnapshotViolation::DuplicateLogicalLocation {
                    group_location: logical_location.group_location().diagnostic_location(),
                    role: logical_location.role().diagnostic_role(),
                }
            }
            Self::ConflictingGroupKind {
                group_location,
                first,
                second,
            } => RpgMakerExtractionSnapshotViolation::ConflictingGroupKind {
                group_location: group_location.diagnostic_location(),
                first: first.diagnostic_group_kind(),
                second: second.diagnostic_group_kind(),
            },
            Self::ConflictingSemanticOrderKey {
                group_location,
                first,
                second,
            } => {
                let (first_path, first_fragment) = first.diagnostic_parts();
                let (second_path, second_fragment) = second.diagnostic_parts();
                RpgMakerExtractionSnapshotViolation::ConflictingSemanticOrderKey {
                    group_location: group_location.diagnostic_location(),
                    first: RpgMakerExtractionSemanticOrderKey::new(first_path, first_fragment),
                    second: RpgMakerExtractionSemanticOrderKey::new(second_path, second_fragment),
                }
            }
            Self::SemanticOrderProjection {
                exact_location,
                source,
            } => RpgMakerExtractionSnapshotViolation::SemanticOrderProjection {
                location: exact_location.diagnostic_location(),
                violation: semantic_order_projection_violation(*source),
            },
            Self::MutationClaimConflict { resource } => {
                RpgMakerExtractionSnapshotViolation::MutationClaimConflict {
                    resource: resource.diagnostic_location(),
                }
            }
            Self::RecipeRoleMismatch {
                group_location,
                units,
                referenced,
            } => RpgMakerExtractionSnapshotViolation::RecipeRoleMismatch {
                group_location: group_location.diagnostic_location(),
                units: units.iter().map(TextUnitRole::diagnostic_role).collect(),
                referenced: referenced
                    .iter()
                    .map(TextUnitRole::diagnostic_role)
                    .collect(),
            },
            Self::RecipeLineMismatch {
                group_location,
                role,
                expected,
                referenced,
            } => RpgMakerExtractionSnapshotViolation::RecipeLineMismatch {
                group_location: group_location.diagnostic_location(),
                role: role.diagnostic_role(),
                expected: expected.iter().copied().collect(),
                referenced: referenced.iter().copied().collect(),
            },
            Self::Projection(source) => RpgMakerExtractionSnapshotViolation::Projection {
                violation: source.diagnostic_violation(),
            },
        }
    }
}

fn semantic_order_projection_violation(
    source: RpgMakerSemanticOrderProjectionError,
) -> RpgMakerExtractionSemanticOrderProjectionViolation {
    match source {
        RpgMakerSemanticOrderProjectionError::MissingSourceDocument => {
            RpgMakerExtractionSemanticOrderProjectionViolation::MissingSourceDocument
        }
        RpgMakerSemanticOrderProjectionError::UnsupportedBuiltinPluginSource => {
            RpgMakerExtractionSemanticOrderProjectionViolation::UnsupportedBuiltinPluginSource
        }
        RpgMakerSemanticOrderProjectionError::ExpectedObject => {
            RpgMakerExtractionSemanticOrderProjectionViolation::ExpectedObject
        }
        RpgMakerSemanticOrderProjectionError::MissingObjectKey => {
            RpgMakerExtractionSemanticOrderProjectionViolation::MissingObjectKey
        }
        RpgMakerSemanticOrderProjectionError::ExpectedArray => {
            RpgMakerExtractionSemanticOrderProjectionViolation::ExpectedArray
        }
        RpgMakerSemanticOrderProjectionError::MissingArrayIndex => {
            RpgMakerExtractionSemanticOrderProjectionViolation::MissingArrayIndex
        }
        RpgMakerSemanticOrderProjectionError::ExpectedEncodedJsonString => {
            RpgMakerExtractionSemanticOrderProjectionViolation::ExpectedEncodedJsonString
        }
        RpgMakerSemanticOrderProjectionError::InvalidEncodedJson => {
            RpgMakerExtractionSemanticOrderProjectionViolation::InvalidEncodedJson
        }
        RpgMakerSemanticOrderProjectionError::MissingPhysicalOrdinal => {
            RpgMakerExtractionSemanticOrderProjectionViolation::MissingPhysicalOrdinal
        }
        RpgMakerSemanticOrderProjectionError::ExtraPhysicalOrdinal => {
            RpgMakerExtractionSemanticOrderProjectionViolation::ExtraPhysicalOrdinal
        }
        RpgMakerSemanticOrderProjectionError::ArrayOrdinalMismatch { index, ordinal } => {
            RpgMakerExtractionSemanticOrderProjectionViolation::ArrayOrdinalMismatch {
                index,
                ordinal,
            }
        }
        RpgMakerSemanticOrderProjectionError::OrdinalOverflow => {
            RpgMakerExtractionSemanticOrderProjectionViolation::OrdinalOverflow
        }
    }
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
            Self::ConflictingGroupKind {
                group_location,
                first,
                second,
            } => write!(
                formatter,
                "同一文本组位置 {group_location} 声明了不同类型：{first:?} 与 {second:?}"
            ),
            Self::ConflictingSemanticOrderKey {
                group_location,
                first,
                second,
            } => write!(
                formatter,
                "同一文本组位置 {group_location} 声明了不同语义顺序键：{first:?} 与 {second:?}"
            ),
            Self::SemanticOrderProjection {
                exact_location,
                source,
            } => write!(
                formatter,
                "无法从原始 JSON 物理位置建立语义顺序键：{exact_location}：{source}"
            ),
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
            Self::SemanticOrderProjection { source, .. } => Some(source),
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
    fn group_location_is_the_unique_identity_even_when_kinds_differ() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let unit = |field: &str, key: &str| {
            ExtractedTextUnit::new(
                field,
                RpgMakerLocation::value(
                    source.clone(),
                    vec![
                        RpgMakerLocationStep::index(1),
                        RpgMakerLocationStep::key(key),
                    ],
                ),
                "原文",
            )
            .expect("测试单元应合法")
        };
        let first = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            vec![unit("name", "name")],
        )
        .expect("首组应合法");
        let second = ExtractedTextGroup::new(
            TextGroupKind::System,
            group_location.clone(),
            vec![unit("description", "description")],
        )
        .expect("第二组应合法");

        assert_eq!(
            RulesSnapshot::new(vec![first, second]),
            Err(SnapshotModelError::ConflictingGroupKind {
                group_location: Box::new(group_location),
                first: TextGroupKind::DatabaseEntry,
                second: TextGroupKind::System,
            })
        );
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_large_snapshot_rebuilds_only_duplicate_locations() {
        const UNIQUE_GROUPS: usize = 20_000;

        let source = RpgMakerSource::data(StandardDataFile::Items);
        let make_group = |index: usize, field: &str| {
            let group_location =
                RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(index)]);
            let unit = ExtractedTextUnit::new(
                field,
                RpgMakerLocation::value(
                    source.clone(),
                    vec![
                        RpgMakerLocationStep::index(index),
                        RpgMakerLocationStep::key(field),
                    ],
                ),
                "文本",
            )
            .expect("测试单元应合法");
            ExtractedTextGroup::new(TextGroupKind::DatabaseEntry, group_location, vec![unit])
                .expect("测试组应合法")
        };

        let groups = (0..UNIQUE_GROUPS)
            .map(|index| make_group(index, "name"))
            .collect::<Vec<_>>();
        let first_units = groups[0].units.as_ptr();
        let first_recipes = groups[0].recipes.as_ptr();
        let mut rebuilds = 0;
        let normalized = normalize_groups_with_rebuild_observer(groups, || rebuilds += 1)
            .expect("大量唯一位置应可直接规范化");

        assert_eq!(normalized.len(), UNIQUE_GROUPS);
        assert_eq!(rebuilds, 0, "唯一位置不得再次运行完整投影构造");
        assert_eq!(normalized[0].units.as_ptr(), first_units);
        assert_eq!(normalized[0].recipes.as_ptr(), first_recipes);

        let mut rebuilds = 0;
        let normalized = normalize_groups_with_rebuild_observer(
            vec![
                make_group(UNIQUE_GROUPS, "name"),
                make_group(UNIQUE_GROUPS, "description"),
            ],
            || rebuilds += 1,
        )
        .expect("同位置的互补字段应归并");
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].units.len(), 2);
        assert_eq!(rebuilds, 1, "只有真实重复位置的桶需要重建一次");
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
    fn group_rejects_a_role_that_does_not_belong_to_its_kind() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let unit_location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let unit = ExtractedTextUnit::projected(
            TextUnitRole::DialogueSpeaker,
            unit_location,
            TextUnitContent::Value("角色".to_owned()),
        )
        .expect("完整单元必须等到所属组建立 kind/role 不变量");

        assert!(matches!(
            ExtractedTextGroup::new(TextGroupKind::DatabaseEntry, group_location, vec![unit],),
            Err(SnapshotModelError::ContentShapeMismatch {
                role: TextUnitRole::DialogueSpeaker,
                ..
            })
        ));
    }

    #[test]
    fn line_content_rejects_embedded_line_breaks_and_nul() {
        let source = RpgMakerSource::map(1);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(0)]);
        let target = RpgMakerLocation::value(source, vec![RpgMakerLocationStep::index(1)]);
        for invalid in ["一\n二", "一\r二", "一\0二"] {
            let unit = ExtractedTextUnit::projected(
                TextUnitRole::ScrollingText,
                target.clone(),
                TextUnitContent::Lines(vec![invalid.to_owned()]),
            )
            .expect("完整单元必须等到所属组建立内容结构不变量");
            let recipe = TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    target.clone(),
                    invalid,
                    vec![DirectTextPart::LineSlot {
                        role: TextUnitRole::ScrollingText,
                        source_line_index: 0,
                    }],
                )
                .expect("测试配方应合法"),
            );
            assert!(matches!(
                ExtractedTextGroup::projected(
                    TextGroupKind::EventScrollingText,
                    group_location.clone(),
                    vec![unit],
                    vec![recipe],
                ),
                Err(SnapshotModelError::InvalidSourceLine {
                    source_line_index: 0,
                    ..
                })
            ));
        }
    }
}
