//! 语义文本单元身份、物理修改目标与物化写回配方。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind};

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

/// 一项物理修改对共享资源所需的访问方式。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MutationResourceAccess {
    Intent,
    Exclusive,
}

impl MutationResourceAccess {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Exclusive => "exclusive",
        }
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "intent" => Some(Self::Intent),
            "exclusive" => Some(Self::Exclusive),
            _ => None,
        }
    }

    pub(crate) const fn conflicts_with(self, other: Self) -> bool {
        !matches!((self, other), (Self::Intent, Self::Intent))
    }
}

/// Claim 展开后的规范物理资源。
///
/// Value 资源同时覆盖普通 JSON 值与逐层解码后的虚拟值；路径中的
/// `DecodeJsonString` 保留解码边界，因此同一原始字符串内的 decoded sibling
/// 只共享 Intent 前缀，而原始字符串与任一 descendant 会形成冲突。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MutationResource {
    Value {
        source: RpgMakerSource,
        steps: Vec<RpgMakerLocationStep>,
    },
    NoteTag {
        source: RpgMakerSource,
        container_steps: Vec<RpgMakerLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: RpgMakerSource,
        command_steps: Vec<RpgMakerLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
}

impl MutationResource {
    pub(crate) fn source(&self) -> &RpgMakerSource {
        match self {
            Self::Value { source, .. }
            | Self::NoteTag { source, .. }
            | Self::CommentTag { source, .. } => source,
        }
    }
}

/// 一个可持久化并用于跨组比较的资源锁。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MutationResourceLock {
    resource: MutationResource,
    access: MutationResourceAccess,
}

impl MutationResourceLock {
    pub(crate) fn new(resource: MutationResource, access: MutationResourceAccess) -> Self {
        Self { resource, access }
    }

    pub(crate) fn resource(&self) -> &MutationResource {
        &self.resource
    }

    pub(crate) const fn access(&self) -> MutationResourceAccess {
        self.access
    }
}

/// 一项原子物理修改声明。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MutationClaim {
    Value(RpgMakerLocation),
    NoteTag(RpgMakerLocation),
    CommentTag {
        location: RpgMakerLocation,
        backing_values: Vec<RpgMakerLocation>,
    },
    EventBlock {
        header: RpgMakerLocation,
        covered_values: Vec<RpgMakerLocation>,
    },
}

impl MutationClaim {
    pub(crate) fn for_location(location: RpgMakerLocation) -> Result<Self, ProjectionModelError> {
        match location {
            location @ RpgMakerLocation::Value { .. } => Ok(Self::Value(location)),
            location @ RpgMakerLocation::NoteTag { .. } => Ok(Self::NoteTag(location)),
            RpgMakerLocation::CommentTag { .. } => {
                Err(ProjectionModelError::CommentTagBackingRequired)
            }
        }
    }

    pub(crate) fn comment_tag(
        location: RpgMakerLocation,
        backing_values: Vec<RpgMakerLocation>,
    ) -> Result<Self, ProjectionModelError> {
        if !matches!(location, RpgMakerLocation::CommentTag { .. }) || backing_values.is_empty() {
            return Err(ProjectionModelError::InvalidCommentTagBacking);
        }
        let source = location.source();
        if backing_values.iter().any(|backing| {
            !matches!(backing, RpgMakerLocation::Value { .. }) || backing.source() != source
        }) {
            return Err(ProjectionModelError::InvalidCommentTagBacking);
        }
        Ok(Self::CommentTag {
            location,
            backing_values,
        })
    }

    pub(crate) fn event_block(
        header: RpgMakerLocation,
        covered_values: Vec<RpgMakerLocation>,
    ) -> Result<Self, ProjectionModelError> {
        let RpgMakerLocation::Value {
            source: header_source,
            ..
        } = &header
        else {
            return Err(ProjectionModelError::EventBlockHeaderMustBeValue);
        };
        if covered_values.is_empty() {
            return Err(ProjectionModelError::EventBlockCoverageRequired);
        }
        if covered_values.iter().any(|location| {
            !matches!(location, RpgMakerLocation::Value { .. })
                || location.source() != header_source
        }) {
            return Err(ProjectionModelError::InvalidEventBlockCoverage);
        }
        Ok(Self::EventBlock {
            header,
            covered_values,
        })
    }

    pub(crate) fn representative_location(&self) -> &RpgMakerLocation {
        match self {
            Self::Value(location) | Self::NoteTag(location) => location,
            Self::CommentTag { location, .. } => location,
            Self::EventBlock { header, .. } => header,
        }
    }

    fn locks(&self) -> Vec<MutationResourceLock> {
        let mut locks = Vec::new();
        match self {
            Self::Value(location) => append_location_locks(location, &mut locks),
            Self::NoteTag(location) => append_note_tag_locks(location, &mut locks),
            Self::CommentTag {
                location,
                backing_values,
            } => append_comment_tag_locks(location, backing_values, &mut locks),
            Self::EventBlock { covered_values, .. } => {
                for location in covered_values {
                    append_location_locks(location, &mut locks);
                }
            }
        }
        locks
    }
}

/// 一项文本组的 Claim 经过规范化后的资源锁集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationClaimSet {
    claims: Vec<MutationClaim>,
    locks: Vec<MutationResourceLock>,
}

impl MutationClaimSet {
    pub(crate) fn new(claims: Vec<MutationClaim>) -> Result<Self, MutationConflict> {
        let mut locks = BTreeMap::<MutationResource, MutationResourceAccess>::new();
        for claim in &claims {
            let mut claim_locks = BTreeMap::<MutationResource, MutationResourceAccess>::new();
            for lock in claim.locks() {
                claim_locks
                    .entry(lock.resource)
                    .and_modify(|access| {
                        if lock.access == MutationResourceAccess::Exclusive {
                            *access = MutationResourceAccess::Exclusive;
                        }
                    })
                    .or_insert(lock.access);
            }
            for (resource, access) in claim_locks {
                if let Some(existing) = locks.get(&resource)
                    && existing.conflicts_with(access)
                {
                    return Err(MutationConflict { resource });
                }
                locks.entry(resource).or_insert(access);
            }
        }
        Ok(Self {
            claims,
            locks: locks
                .into_iter()
                .map(|(resource, access)| MutationResourceLock::new(resource, access))
                .collect(),
        })
    }

    pub(crate) fn claims(&self) -> &[MutationClaim] {
        &self.claims
    }

    /// 从数据库中已经展开的规范锁恢复集合；重复资源表示持久化损坏。
    pub(crate) fn from_locks(
        mut locks: Vec<MutationResourceLock>,
    ) -> Result<Self, MutationConflict> {
        locks.sort_by(|left, right| left.resource.cmp(&right.resource));
        for pair in locks.windows(2) {
            if pair[0].resource == pair[1].resource {
                return Err(MutationConflict {
                    resource: pair[0].resource.clone(),
                });
            }
        }
        Ok(Self {
            claims: Vec::new(),
            locks,
        })
    }

    pub(crate) fn locks(&self) -> &[MutationResourceLock] {
        &self.locks
    }

    pub(crate) fn conflict_with(&self, other: &Self) -> Option<MutationConflict> {
        let mut left = self.locks.iter().peekable();
        let mut right = other.locks.iter().peekable();
        while let (Some(left_lock), Some(right_lock)) = (left.peek(), right.peek()) {
            match left_lock.resource.cmp(&right_lock.resource) {
                std::cmp::Ordering::Less => {
                    left.next();
                }
                std::cmp::Ordering::Greater => {
                    right.next();
                }
                std::cmp::Ordering::Equal => {
                    if left_lock.access.conflicts_with(right_lock.access) {
                        return Some(MutationConflict {
                            resource: left_lock.resource.clone(),
                        });
                    }
                    left.next();
                    right.next();
                }
            }
        }
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationConflict {
    resource: MutationResource,
}

impl MutationConflict {
    pub(crate) fn resource(&self) -> &MutationResource {
        &self.resource
    }
}

fn append_location_locks(location: &RpgMakerLocation, locks: &mut Vec<MutationResourceLock>) {
    match location {
        RpgMakerLocation::Value { source, steps } => {
            for prefix_length in 0..steps.len() {
                locks.push(MutationResourceLock::new(
                    MutationResource::Value {
                        source: source.clone(),
                        steps: steps[..prefix_length].to_vec(),
                    },
                    MutationResourceAccess::Intent,
                ));
            }
            locks.push(MutationResourceLock::new(
                MutationResource::Value {
                    source: source.clone(),
                    steps: steps.clone(),
                },
                MutationResourceAccess::Exclusive,
            ));
        }
        RpgMakerLocation::NoteTag { .. } => append_note_tag_locks(location, locks),
        RpgMakerLocation::CommentTag { .. } => {
            debug_assert!(false, "CommentTag 必须由携带完整 backing 的 Claim 展开")
        }
    }
}

fn append_note_tag_locks(location: &RpgMakerLocation, locks: &mut Vec<MutationResourceLock>) {
    let RpgMakerLocation::NoteTag {
        source,
        container_steps,
        tag_name,
        occurrence,
    } = location
    else {
        debug_assert!(false, "NoteTag Claim 必须携带 NoteTag 位置");
        return;
    };
    let mut note_steps = container_steps.clone();
    note_steps.push(RpgMakerLocationStep::key("note"));
    for prefix_length in 0..note_steps.len() {
        locks.push(MutationResourceLock::new(
            MutationResource::Value {
                source: source.clone(),
                steps: note_steps[..prefix_length].to_vec(),
            },
            MutationResourceAccess::Intent,
        ));
    }
    locks.push(MutationResourceLock::new(
        MutationResource::Value {
            source: source.clone(),
            steps: note_steps,
        },
        MutationResourceAccess::Intent,
    ));
    locks.push(MutationResourceLock::new(
        MutationResource::NoteTag {
            source: source.clone(),
            container_steps: container_steps.clone(),
            tag_name: tag_name.clone(),
            occurrence: *occurrence,
        },
        MutationResourceAccess::Exclusive,
    ));
}

fn append_comment_tag_locks(
    location: &RpgMakerLocation,
    backing_values: &[RpgMakerLocation],
    locks: &mut Vec<MutationResourceLock>,
) {
    let RpgMakerLocation::CommentTag {
        source,
        command_steps,
        tag_name,
        occurrence,
    } = location
    else {
        debug_assert!(false, "CommentTag Claim 必须携带 CommentTag 位置");
        return;
    };
    for backing in backing_values {
        let RpgMakerLocation::Value {
            source: backing_source,
            steps,
        } = backing
        else {
            debug_assert!(false, "CommentTag backing 必须是 Value");
            continue;
        };
        for prefix_length in 0..steps.len() {
            locks.push(MutationResourceLock::new(
                MutationResource::Value {
                    source: backing_source.clone(),
                    steps: steps[..prefix_length].to_vec(),
                },
                MutationResourceAccess::Intent,
            ));
        }
        locks.push(MutationResourceLock::new(
            MutationResource::Value {
                source: backing_source.clone(),
                steps: steps.clone(),
            },
            MutationResourceAccess::Intent,
        ));
    }
    locks.push(MutationResourceLock::new(
        MutationResource::CommentTag {
            source: source.clone(),
            command_steps: command_steps.clone(),
            tag_name: tag_name.clone(),
            occurrence: *occurrence,
        },
        MutationResourceAccess::Exclusive,
    ));
}

fn event_command_steps(
    location: &RpgMakerLocation,
) -> Option<(&RpgMakerSource, Vec<RpgMakerLocationStep>)> {
    let RpgMakerLocation::Value { source, steps } = location else {
        return None;
    };
    let parameter_key = steps.iter().position(
        |step| matches!(step, RpgMakerLocationStep::ObjectKey(key) if key == "parameters"),
    );
    let search_end = parameter_key.unwrap_or(steps.len());
    let command_index = steps[..search_end]
        .iter()
        .rposition(|step| matches!(step, RpgMakerLocationStep::ArrayIndex(_)))?;
    Some((source, steps[..=command_index].to_vec()))
}

/// 一个已物化、无需重新运行外部规则的写回配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TextProjectionRecipe {
    Direct(DirectTextRecipe),
    Dialogue(DialogueWriteRecipe),
    /// 不直接重建文本，只把冻结事件结构覆盖物化进同一配方快照。
    Claim(MutationClaim),
}

impl TextProjectionRecipe {
    fn direct_mutation_claims(&self) -> Vec<MutationClaim> {
        match self {
            Self::Direct(recipe) => vec![recipe.mutation_claim().clone()],
            Self::Dialogue(recipe) => {
                let mut claims = Vec::new();
                if let Some(speaker) = recipe.direct_speaker() {
                    claims.push(
                        MutationClaim::for_location(speaker.physical_location().clone())
                            .expect("对话直接 Speaker 已验证为 Value 位置"),
                    );
                }
                claims.extend(recipe.lines().iter().map(|line| {
                    MutationClaim::for_location(line.physical_location().clone())
                        .expect("对话正文行已验证为 Value 位置")
                }));
                claims
            }
            Self::Claim(claim) => vec![claim.clone()],
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
            Self::Claim(_) => BTreeSet::new(),
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
            Self::Claim(_) => Vec::new(),
        }
    }
}

/// 从组语义与物化配方建立唯一的物理修改声明集合。
pub(crate) fn mutation_claims_for_group(
    kind: TextGroupKind,
    group_location: &RpgMakerLocation,
    recipes: &[TextProjectionRecipe],
) -> Result<MutationClaimSet, MutationConflict> {
    let direct_claims = recipes
        .iter()
        .flat_map(TextProjectionRecipe::direct_mutation_claims)
        .collect::<Vec<_>>();

    if matches!(
        kind,
        TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
    ) {
        let explicit_event_claims = direct_claims
            .iter()
            .filter(|claim| matches!(claim, MutationClaim::EventBlock { .. }))
            .cloned()
            .collect::<Vec<_>>();
        if !explicit_event_claims.is_empty() {
            let event_claims = MutationClaimSet::new(explicit_event_claims)?;
            for claim in direct_claims
                .iter()
                .filter(|claim| !matches!(claim, MutationClaim::EventBlock { .. }))
            {
                for required in claim.locks() {
                    if !event_claims.locks().iter().any(|actual| {
                        actual.resource() == required.resource()
                            && (actual.access() == required.access()
                                || actual.access() == MutationResourceAccess::Exclusive)
                    }) {
                        return Err(MutationConflict {
                            resource: required.resource,
                        });
                    }
                }
            }
            return Ok(event_claims);
        }
    }
    // 先以每个配方目标为原子项验证，排除同一组内的 raw/decoded、重复标签等冲突。
    MutationClaimSet::new(direct_claims.clone())?;

    if matches!(
        kind,
        TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
    ) {
        let mut covered_values = vec![group_location.clone()];
        covered_values.extend(direct_claims.into_iter().map(|claim| {
            let location = claim.representative_location();
            event_command_steps(location).map_or_else(
                || location.clone(),
                |(source, steps)| RpgMakerLocation::value(source.clone(), steps),
            )
        }));
        let event_claim = MutationClaim::event_block(group_location.clone(), covered_values)
            .expect("自动事件块 Claim 必须由同一来源的非空 Value 地址组成");
        MutationClaimSet::new(vec![event_claim])
    } else {
        MutationClaimSet::new(direct_claims)
    }
}

/// 整字段、局部正则文本或语义行的可逆配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectTextRecipe {
    target: RpgMakerLocation,
    mutation_claim: MutationClaim,
    expected_raw: String,
    parts: Vec<DirectTextPart>,
}

impl DirectTextRecipe {
    pub(crate) fn new(
        target: RpgMakerLocation,
        expected_raw: impl Into<String>,
        parts: Vec<DirectTextPart>,
    ) -> Result<Self, ProjectionModelError> {
        let mutation_claim = MutationClaim::for_location(target.clone())?;
        Self::new_with_claim(target, mutation_claim, expected_raw, parts)
    }

    pub(crate) fn new_with_claim(
        target: RpgMakerLocation,
        mutation_claim: MutationClaim,
        expected_raw: impl Into<String>,
        parts: Vec<DirectTextPart>,
    ) -> Result<Self, ProjectionModelError> {
        if mutation_claim.representative_location() != &target {
            return Err(ProjectionModelError::MutationClaimTargetMismatch);
        }
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
            mutation_claim,
            expected_raw,
            parts,
        })
    }

    pub(crate) fn target(&self) -> &RpgMakerLocation {
        &self.target
    }

    pub(crate) fn mutation_claim(&self) -> &MutationClaim {
        &self.mutation_claim
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
        if !matches!(group_location, RpgMakerLocation::Value { .. })
            || direct_speaker.as_ref().is_some_and(|target| {
                !matches!(target.physical_location(), RpgMakerLocation::Value { .. })
            })
            || lines
                .iter()
                .any(|line| !matches!(line.physical_location(), RpgMakerLocation::Value { .. }))
        {
            return Err(ProjectionModelError::InvalidDialoguePhysicalLocation);
        }
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
    CommentTagBackingRequired,
    InvalidCommentTagBacking,
    EventBlockHeaderMustBeValue,
    EventBlockCoverageRequired,
    InvalidEventBlockCoverage,
    MutationClaimTargetMismatch,
    InvalidDialoguePhysicalLocation,
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
            Self::CommentTagBackingRequired => {
                formatter.write_str("CommentTag 投影必须声明完整 108/408 backing 值")
            }
            Self::InvalidCommentTagBacking => {
                formatter.write_str("CommentTag backing 必须是同一来源中的非空 Value 地址集合")
            }
            Self::EventBlockHeaderMustBeValue => {
                formatter.write_str("事件块 header 必须是 Value 地址")
            }
            Self::EventBlockCoverageRequired => {
                formatter.write_str("事件块必须声明至少一个真实修改 Value")
            }
            Self::InvalidEventBlockCoverage => {
                formatter.write_str("事件块 coverage 必须全部是与 header 同来源的 Value 地址")
            }
            Self::MutationClaimTargetMismatch => {
                formatter.write_str("直接文本配方的 Claim 与写回目标不一致")
            }
            Self::InvalidDialoguePhysicalLocation => {
                formatter.write_str("对话头、Speaker 与正文行必须都是 Value 地址")
            }
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
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};

    fn location(index: usize) -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(index),
            ],
        )
    }

    fn value_location(steps: Vec<RpgMakerLocationStep>) -> RpgMakerLocation {
        RpgMakerLocation::value(RpgMakerSource::data(StandardDataFile::Items), steps)
    }

    fn claim_set(claims: Vec<MutationClaim>) -> MutationClaimSet {
        MutationClaimSet::new(claims).expect("测试 Claim 应互不冲突")
    }

    #[test]
    fn value_claims_conflict_with_decoded_descendants_but_not_decoded_siblings() {
        let raw = value_location(vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("payload"),
        ]);
        let decoded_left = value_location(vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("payload"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::key("left"),
        ]);
        let decoded_right = value_location(vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("payload"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::key("right"),
        ]);

        assert!(
            MutationClaimSet::new(vec![
                MutationClaim::for_location(raw.clone()).expect("raw Value 应合法"),
                MutationClaim::for_location(decoded_left.clone()).expect("decoded Value 应合法"),
            ])
            .is_err()
        );
        assert!(
            claim_set(vec![
                MutationClaim::for_location(raw).expect("raw Value 应合法")
            ])
            .conflict_with(&claim_set(vec![
                MutationClaim::for_location(decoded_left.clone()).expect("decoded Value 应合法")
            ]))
            .is_some()
        );
        assert!(
            MutationClaimSet::new(vec![
                MutationClaim::for_location(decoded_left).expect("decoded left 应合法"),
                MutationClaim::for_location(decoded_right).expect("decoded right 应合法"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn note_tag_claims_conflict_with_raw_note_but_distinct_occurrences_coexist() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let container = vec![RpgMakerLocationStep::index(1)];
        let raw_note = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("note"),
            ],
        );
        let first = RpgMakerLocation::note_tag(source.clone(), container.clone(), "Tag", 0);
        let second = RpgMakerLocation::note_tag(source, container, "Tag", 1);

        assert!(
            MutationClaimSet::new(vec![
                MutationClaim::for_location(raw_note).expect("raw note 应合法"),
                MutationClaim::for_location(first.clone()).expect("NoteTag 应合法"),
            ])
            .is_err()
        );
        assert!(
            MutationClaimSet::new(vec![
                MutationClaim::for_location(first).expect("首个 NoteTag 应合法"),
                MutationClaim::for_location(second).expect("第二个 NoteTag 应合法"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn comment_tag_claims_cover_every_108_408_backing_and_allow_distinct_occurrences() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let command_steps = vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("list"),
            RpgMakerLocationStep::index(4),
        ];
        let backing = [4, 5]
            .into_iter()
            .map(|command_index| {
                RpgMakerLocation::value(
                    source.clone(),
                    vec![
                        RpgMakerLocationStep::index(1),
                        RpgMakerLocationStep::key("list"),
                        RpgMakerLocationStep::index(command_index),
                        RpgMakerLocationStep::key("parameters"),
                        RpgMakerLocationStep::index(0),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let first = MutationClaim::comment_tag(
            RpgMakerLocation::comment_tag(source.clone(), command_steps.clone(), "Tag", 0),
            backing.clone(),
        )
        .expect("首个 CommentTag 应合法");
        let second = MutationClaim::comment_tag(
            RpgMakerLocation::comment_tag(source, command_steps, "Tag", 1),
            backing.clone(),
        )
        .expect("第二个 CommentTag 应合法");

        for (index, raw_comment) in backing.iter().enumerate() {
            assert!(
                MutationClaimSet::new(vec![
                    MutationClaim::for_location(raw_comment.clone()).expect("108/408 raw 值应合法"),
                    first.clone(),
                ])
                .is_err(),
                "CommentTag 必须与第 {index} 个真实 108/408 backing 冲突"
            );
        }
        assert!(MutationClaimSet::new(vec![first, second]).is_ok());
    }

    #[test]
    fn event_block_claim_conflicts_with_covered_descendants_but_not_other_commands() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let command = |index| {
            RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(index),
                ],
            )
        };
        let descendant = |index| {
            RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(index),
                    RpgMakerLocationStep::key("parameters"),
                    RpgMakerLocationStep::index(0),
                ],
            )
        };
        let dialogue = claim_set(vec![
            MutationClaim::event_block(command(1), vec![command(2)])
                .expect("同来源非空事件块 Claim 应合法"),
        ]);
        let header_descendant = claim_set(vec![
            MutationClaim::for_location(descendant(1)).expect("header descendant 应合法"),
        ]);
        let covered_descendant = claim_set(vec![
            MutationClaim::for_location(descendant(2)).expect("正文 descendant 应合法"),
        ]);
        let other_command = claim_set(vec![
            MutationClaim::for_location(descendant(3)).expect("其他命令 descendant 应合法"),
        ]);

        assert!(dialogue.conflict_with(&header_descendant).is_none());
        assert!(dialogue.conflict_with(&covered_descendant).is_some());
        assert!(dialogue.conflict_with(&other_command).is_none());
    }

    #[test]
    fn event_block_claim_rejects_invalid_header_empty_non_value_and_cross_source_coverage() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let header = RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let covered = RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(2)]);
        let non_value = RpgMakerLocation::note_tag(
            source.clone(),
            vec![RpgMakerLocationStep::index(2)],
            "Tag",
            0,
        );
        let cross_source = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );

        assert!(MutationClaim::event_block(header.clone(), vec![covered]).is_ok());
        assert!(matches!(
            MutationClaim::event_block(non_value.clone(), vec![header.clone()]),
            Err(ProjectionModelError::EventBlockHeaderMustBeValue)
        ));
        assert!(matches!(
            MutationClaim::event_block(header.clone(), Vec::new()),
            Err(ProjectionModelError::EventBlockCoverageRequired)
        ));
        assert!(matches!(
            MutationClaim::event_block(header.clone(), vec![non_value]),
            Err(ProjectionModelError::InvalidEventBlockCoverage)
        ));
        assert!(matches!(
            MutationClaim::event_block(header, vec![cross_source]),
            Err(ProjectionModelError::InvalidEventBlockCoverage)
        ));
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
