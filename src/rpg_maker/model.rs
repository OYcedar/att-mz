//! 语义文本单元身份、物理修改目标与物化写回配方。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::translation::candidate_validation::is_structural_blank;

use super::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind};
use crate::diagnostic::{RpgMakerDiagnosticRole, RpgMakerProjectionModelViolation};

/// 按 MV/MZ JavaScript number 语义判断 402.parameters[0] 是否为整数。
///
/// 引擎只把该值与实际选择的整数作严格相等比较；不相等的分支自然不会执行，因此 ATT
/// 不根据 102 的选项数量另造范围限制，也不需要把这个值转换成本机索引。
pub(crate) fn choice_branch_value_is_integer(value: Option<&serde_json::Value>) -> bool {
    let Some(number) = value.and_then(serde_json::Value::as_number) else {
        return false;
    };
    if number.as_i64().is_some() || number.as_u64().is_some() {
        return true;
    }
    number
        .as_f64()
        .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
}

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

    /// 只有选项和滚动文本的物理数组元素是必须分别保持的 Placeholder 槽位。
    /// 对话正文允许重新断行，因此按完整 Unit 验证 Placeholder。
    pub(crate) const fn preserves_placeholder_line_slots(&self) -> bool {
        matches!(self, Self::Choices | Self::ScrollingText)
    }

    /// 角色与文本组语义的唯一匹配规则。
    ///
    /// 事件专属组只接受对应专属角色,其余组只接受 Scalar。该不变量由提取协议
    /// 建立;Translate 与 WriteBack 的资产读取边界都消费这同一份定义,不得各自
    /// 另写宽严不同的版本。
    fn matches_kind(&self, kind: TextGroupKind) -> bool {
        match kind {
            TextGroupKind::EventDialogue => {
                matches!(self, Self::DialogueSpeaker | Self::DialogueBody)
            }
            TextGroupKind::EventChoices => matches!(self, Self::Choices),
            TextGroupKind::EventScrollingText => matches!(self, Self::ScrollingText),
            TextGroupKind::DatabaseEntry
            | TextGroupKind::System
            | TextGroupKind::Map
            | TextGroupKind::EventCommand
            | TextGroupKind::PluginParameter => matches!(self, Self::Scalar(_)),
        }
    }

    /// 将领域角色投影为公开诊断角色；标量字段键已经由 `ScalarFieldKey` 校验。
    pub(crate) fn diagnostic_role(&self) -> RpgMakerDiagnosticRole {
        match self {
            Self::Scalar(field) => RpgMakerDiagnosticRole::scalar(field.as_str()),
            Self::DialogueSpeaker => RpgMakerDiagnosticRole::DialogueSpeaker,
            Self::DialogueBody => RpgMakerDiagnosticRole::DialogueBody,
            Self::Choices => RpgMakerDiagnosticRole::Choices,
            Self::ScrollingText => RpgMakerDiagnosticRole::ScrollingText,
        }
    }
}

/// 一个语义单元的完整文本内容。
///
/// `Value` 是单个标量，内部 LF 属于值本身；`Lines` 的每个元素是独立语义槽，元素之间
/// 不存在可由 Placeholder 吞并的内容字符。
///
/// 无标签序列化使 SQLite 中的权威内容直接表现为 JSON string 或 string array，
/// 不把内部类型包装写入持久化数据。
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub(crate) enum TextUnitContent {
    Value(String),
    Lines(Vec<String>),
}

/// 在不复制正文的前提下交给领域不变量校验的文本内容视图。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextUnitContentView<'a> {
    Value(&'a str),
    Lines(&'a [String]),
}

impl<'a> From<&'a TextUnitContent> for TextUnitContentView<'a> {
    fn from(content: &'a TextUnitContent) -> Self {
        match content {
            TextUnitContent::Value(value) => Self::Value(value),
            TextUnitContent::Lines(lines) => Self::Lines(lines),
        }
    }
}

/// 组类型、角色、内容物理结构不一致，或内容含有不能进入语义单元的控制字符。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextUnitContentStructureError {
    KindRoleMismatch,
    ShapeMismatch,
    InvalidText { line_index: usize },
}

/// 验证一个完整语义单元内容的唯一结构规则。
///
/// 组类型必须接受该语义角色；`Scalar` 的单值可以包含 LF；Dialogue speaker 的单值
/// 不能包含 CR/LF；任何单值都不能包含 NUL。行序列中的每个槽都不能包含 CR、LF 或
/// NUL。纯空白、空行序列、对齐数量和空槽对应关系属于各业务边界的额外规则，不在这里
/// 判断。
pub(crate) fn validate_text_unit_content_structure(
    kind: TextGroupKind,
    role: &TextUnitRole,
    content: TextUnitContentView<'_>,
) -> Result<(), TextUnitContentStructureError> {
    if !role.matches_kind(kind) {
        return Err(TextUnitContentStructureError::KindRoleMismatch);
    }
    match content {
        TextUnitContentView::Value(_) if role.expects_lines() => {
            Err(TextUnitContentStructureError::ShapeMismatch)
        }
        TextUnitContentView::Lines(_) if !role.expects_lines() => {
            Err(TextUnitContentStructureError::ShapeMismatch)
        }
        TextUnitContentView::Value(value) => {
            if value.contains('\0')
                || (matches!(role, TextUnitRole::DialogueSpeaker)
                    && (value.contains('\r') || value.contains('\n')))
            {
                Err(TextUnitContentStructureError::InvalidText { line_index: 0 })
            } else {
                Ok(())
            }
        }
        TextUnitContentView::Lines(lines) => validate_text_unit_lines(lines),
    }
}

/// 验证物理行槽的唯一控制字符规则。
///
/// 模型响应尚未重建为 `TextUnitContent` 时也需要消费同一规则，因此这一窄入口与
/// 完整内容校验并列暴露，避免 Executor 复制字符扫描逻辑。
pub(crate) fn validate_text_unit_lines(
    lines: &[String],
) -> Result<(), TextUnitContentStructureError> {
    if let Some(line_index) = lines.iter().position(|line| {
        line.chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
    }) {
        Err(TextUnitContentStructureError::InvalidText { line_index })
    } else {
        Ok(())
    }
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
            Self::Value(value) => is_structural_blank(value),
            Self::Lines(lines) => lines.iter().all(|line| is_structural_blank(line)),
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

/// 一个可持久化并用于跨组比较的资源锁。
///
/// 每个资源都是一个物理 JSON 值。路径中的 `DecodeJsonString` 保留解码边界，
/// 因此同一原始字符串内的 decoded sibling 只共享 Intent 前缀，而原始字符串与任一
/// descendant 会形成冲突。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MutationResourceLock {
    resource: RpgMakerLocation,
    access: MutationResourceAccess,
}

impl MutationResourceLock {
    pub(crate) fn new(resource: RpgMakerLocation, access: MutationResourceAccess) -> Self {
        Self { resource, access }
    }

    pub(crate) fn resource(&self) -> &RpgMakerLocation {
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
    EventBlock {
        header: RpgMakerLocation,
        covered_values: Vec<RpgMakerLocation>,
    },
}

impl MutationClaim {
    pub(crate) fn for_location(location: RpgMakerLocation) -> Self {
        Self::Value(location)
    }

    pub(crate) fn event_block(
        header: RpgMakerLocation,
        covered_values: Vec<RpgMakerLocation>,
    ) -> Result<Self, ProjectionModelError> {
        let header_source = header.source();
        if covered_values.is_empty() {
            return Err(ProjectionModelError::EventBlockCoverageRequired);
        }
        if covered_values
            .iter()
            .any(|location| location.source() != header_source)
        {
            return Err(ProjectionModelError::InvalidEventBlockCoverage);
        }
        Ok(Self::EventBlock {
            header,
            covered_values,
        })
    }

    pub(crate) fn representative_location(&self) -> &RpgMakerLocation {
        match self {
            Self::Value(location) => location,
            Self::EventBlock { header, .. } => header,
        }
    }

    fn locks(&self) -> Vec<MutationResourceLock> {
        let mut locks = Vec::new();
        match self {
            Self::Value(location) => append_location_locks(location, &mut locks),
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
        let mut locks = BTreeMap::<RpgMakerLocation, MutationResourceAccess>::new();
        for claim in &claims {
            let mut claim_locks = BTreeMap::<RpgMakerLocation, MutationResourceAccess>::new();
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

    #[cfg(test)]
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

/// 按文本组声明顺序增量验证 Mutation Claim 的 owner 级索引。
///
/// 索引只遍历当前组的规范锁，并直接查找此前最早占用同一资源的组，避免把每个新组
/// 与全部旧组两两比较。冲突选择仍与原有顺序一致：优先最早的旧组；同一旧组内优先
/// 规范资源顺序最小的冲突。
#[derive(Default)]
pub(crate) struct MutationClaimIndex<'a> {
    // Claim 集合在 owner 级校验结束前始终存活；索引只借用规范资源，避免为大型
    // 游戏的每个唯一 Value/事件块深拷贝 source、steps 和路径字符串。只有真的
    // 发现冲突时才克隆最终要进入错误值的那一个资源。
    resources: HashMap<&'a RpgMakerLocation, IndexedMutationResource>,
    next_group_index: usize,
    #[cfg(test)]
    resource_lookups: usize,
}

#[derive(Clone, Copy)]
struct IndexedMutationResource {
    first_group: usize,
    first_exclusive_group: Option<usize>,
}

impl<'a> MutationClaimIndex<'a> {
    pub(crate) fn with_capacity(resource_capacity: usize) -> Self {
        Self {
            resources: HashMap::with_capacity(resource_capacity),
            next_group_index: 0,
            #[cfg(test)]
            resource_lookups: 0,
        }
    }

    pub(crate) fn insert(&mut self, claims: &'a MutationClaimSet) -> Result<(), MutationConflict> {
        let group_index = self.next_group_index;
        let next_group_index = self
            .next_group_index
            .checked_add(1)
            .expect("内存中的文本组数量必须可用 usize 表达");
        let mut inserted_resources = Vec::new();
        let mut selected = None::<(usize, &'a RpgMakerLocation)>;
        for lock in claims.locks() {
            #[cfg(test)]
            {
                self.resource_lookups += 1;
            }
            match self.resources.entry(lock.resource()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    inserted_resources.push(lock.resource());
                    entry.insert(IndexedMutationResource {
                        first_group: group_index,
                        first_exclusive_group: (lock.access() == MutationResourceAccess::Exclusive)
                            .then_some(group_index),
                    });
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    let indexed = entry.get();
                    let conflicting_group = match lock.access() {
                        MutationResourceAccess::Intent => indexed.first_exclusive_group,
                        MutationResourceAccess::Exclusive => Some(indexed.first_group),
                    };
                    if let Some(conflicting_group) = conflicting_group {
                        let candidate = (conflicting_group, lock.resource());
                        if selected.is_none_or(|current| candidate < current) {
                            selected = Some(candidate);
                        }
                    }
                }
            }
        }
        if let Some((_, resource)) = selected {
            // `insert` 的错误路径同样保持原子；调用方即使选择继续使用索引，也不会
            // 看见当前失败 Group 在此前空闲资源上留下的部分占用。
            for resource in inserted_resources {
                self.resources.remove(resource);
            }
            return Err(MutationConflict {
                resource: resource.clone(),
            });
        }
        self.next_group_index = next_group_index;
        Ok(())
    }

    #[cfg(test)]
    fn resource_lookups(&self) -> usize {
        self.resource_lookups
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationConflict {
    resource: RpgMakerLocation,
}

impl MutationConflict {
    pub(crate) fn resource(&self) -> &RpgMakerLocation {
        &self.resource
    }
}

fn append_location_locks(location: &RpgMakerLocation, locks: &mut Vec<MutationResourceLock>) {
    for prefix_length in 0..location.steps().len() {
        locks.push(MutationResourceLock::new(
            RpgMakerLocation::value(
                location.source().clone(),
                location.steps()[..prefix_length].to_vec(),
            ),
            MutationResourceAccess::Intent,
        ));
    }
    locks.push(MutationResourceLock::new(
        location.clone(),
        MutationResourceAccess::Exclusive,
    ));
}

fn event_command_steps(
    location: &RpgMakerLocation,
) -> Option<(&RpgMakerSource, Vec<RpgMakerLocationStep>)> {
    let source = location.source();
    let steps = location.steps();
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
                    claims.push(MutationClaim::for_location(
                        speaker.physical_location().clone(),
                    ));
                }
                claims.extend(
                    recipe
                        .lines()
                        .iter()
                        .map(|line| MutationClaim::for_location(line.physical_location().clone())),
                );
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
                    let actual = event_claims
                        .locks()
                        .binary_search_by(|actual| actual.resource().cmp(required.resource()))
                        .ok()
                        .map(|index| &event_claims.locks()[index]);
                    if !actual.is_some_and(|actual| {
                        actual.access() == required.access()
                            || actual.access() == MutationResourceAccess::Exclusive
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
    // 先以每个配方目标为原子项验证，排除同一组内的 raw/decoded 与重复 Value 冲突。
    let direct_claims = MutationClaimSet::new(direct_claims)?;

    if matches!(
        kind,
        TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
    ) {
        let mut covered_values = vec![group_location.clone()];
        covered_values.extend(direct_claims.claims().iter().map(|claim| {
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
        Ok(direct_claims)
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
        let mutation_claim = MutationClaim::for_location(target.clone());
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
    EventBlockCoverageRequired,
    InvalidEventBlockCoverage,
    MutationClaimTargetMismatch,
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

impl ProjectionModelError {
    /// 保留投影模型的封闭原因和数值位置，不把 `Display` 当成诊断协议。
    pub(crate) fn diagnostic_violation(&self) -> RpgMakerProjectionModelViolation {
        match self {
            Self::EmptyScalarFieldKey => RpgMakerProjectionModelViolation::EmptyScalarFieldKey,
            Self::EventBlockCoverageRequired => {
                RpgMakerProjectionModelViolation::EventBlockCoverageRequired
            }
            Self::InvalidEventBlockCoverage => {
                RpgMakerProjectionModelViolation::InvalidEventBlockCoverage
            }
            Self::MutationClaimTargetMismatch => {
                RpgMakerProjectionModelViolation::MutationClaimTargetMismatch
            }
            Self::RecipeHasNoTextSlot => RpgMakerProjectionModelViolation::RecipeHasNoTextSlot,
            Self::DuplicateProjectionSlot {
                role,
                source_line_index,
            } => RpgMakerProjectionModelViolation::DuplicateProjectionSlot {
                role: role.diagnostic_role(),
                source_line_index: *source_line_index,
            },
            Self::MultipleBodyLinesInPhysicalLine => {
                RpgMakerProjectionModelViolation::MultipleBodyLinesInPhysicalLine
            }
            Self::DuplicateDialogueBodyLine { source_line_index } => {
                RpgMakerProjectionModelViolation::DuplicateDialogueBodyLine {
                    source_line_index: *source_line_index,
                }
            }
            Self::NonContiguousDialogueBodyLines { expected, actual } => {
                RpgMakerProjectionModelViolation::NonContiguousDialogueBodyLines {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::MixedDirectAndInlineSpeaker => {
                RpgMakerProjectionModelViolation::MixedDirectAndInlineSpeaker
            }
        }
    }
}

impl fmt::Display for ProjectionModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScalarFieldKey => formatter.write_str("标量字段键不能为空"),
            Self::EventBlockCoverageRequired => {
                formatter.write_str("事件块必须声明至少一个真实修改 Value")
            }
            Self::InvalidEventBlockCoverage => {
                formatter.write_str("事件块 coverage 必须全部与 header 来自同一来源")
            }
            Self::MutationClaimTargetMismatch => {
                formatter.write_str("直接文本配方的 Claim 与写回目标不一致")
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
    fn claim_index_preserves_earliest_group_conflict_order() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let location = |key: &str| {
            RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key(key),
                ],
            )
        };
        let first = claim_set(vec![MutationClaim::Value(location("z"))]);
        let second = claim_set(vec![MutationClaim::Value(location("a"))]);
        let current = claim_set(vec![
            MutationClaim::Value(location("a")),
            MutationClaim::Value(location("z")),
        ]);

        let expected = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("z"),
            ],
        );
        for mut index in [
            MutationClaimIndex::default(),
            MutationClaimIndex::with_capacity(
                first.locks().len() + second.locks().len() + current.locks().len(),
            ),
        ] {
            index.insert(&first).expect("首组应可登记");
            index.insert(&second).expect("第二组应可登记");
            let conflict = index.insert(&current).expect_err("当前组应同时冲突");

            assert_eq!(
                conflict.resource(),
                &expected,
                "预分配与否都必须先报告最早旧组的冲突，而不是全局字典序最小资源"
            );
        }
    }

    #[test]
    fn claim_index_work_grows_with_claim_locks_instead_of_group_pairs() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_count = 2_000;
        let claims = (0..group_count)
            .map(|group_index| {
                claim_set(vec![MutationClaim::Value(RpgMakerLocation::value(
                    source.clone(),
                    vec![
                        RpgMakerLocationStep::index(group_index),
                        RpgMakerLocationStep::key("name"),
                    ],
                ))])
            })
            .collect::<Vec<_>>();
        let expected_lookups = claims.iter().map(|claims| claims.locks().len()).sum();
        let mut index = MutationClaimIndex::with_capacity(expected_lookups);

        for claims in &claims {
            index.insert(claims).expect("不同数组元素不得冲突");
        }

        assert_eq!(index.resource_lookups(), expected_lookups);
        assert_eq!(expected_lookups, group_count * 3);
    }

    #[test]
    fn failed_claim_index_insert_rolls_back_all_new_resources_and_preserves_sequence() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let resource = |key: &str| {
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key(key)])
        };
        let existing = MutationClaimSet::from_locks(vec![MutationResourceLock::new(
            resource("b"),
            MutationResourceAccess::Exclusive,
        )])
        .expect("现存 Claim 应合法");
        let failing = MutationClaimSet::from_locks(vec![
            MutationResourceLock::new(resource("a"), MutationResourceAccess::Intent),
            MutationResourceLock::new(resource("b"), MutationResourceAccess::Intent),
            MutationResourceLock::new(resource("c"), MutationResourceAccess::Intent),
        ])
        .expect("当前组自身应合法");
        let after_failure_left = MutationClaimSet::from_locks(vec![MutationResourceLock::new(
            resource("a"),
            MutationResourceAccess::Exclusive,
        )])
        .expect("冲突点前的后续 Claim 应合法");
        let after_failure_right = MutationClaimSet::from_locks(vec![MutationResourceLock::new(
            resource("c"),
            MutationResourceAccess::Exclusive,
        )])
        .expect("冲突点后的后续 Claim 应合法");
        let mut index = MutationClaimIndex::default();

        index.insert(&existing).expect("首组应可登记");
        assert_eq!(index.next_group_index, 1);
        index.insert(&failing).expect_err("第二组必须冲突");
        assert_eq!(index.next_group_index, 1, "失败插入不得消耗文本组声明序号");
        index
            .insert(&after_failure_left)
            .expect("失败组在冲突点前登记的空闲资源必须回滚");
        index
            .insert(&after_failure_right)
            .expect("失败组在冲突点后登记的空闲资源也必须回滚");
        assert_eq!(index.next_group_index, 3);
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
                MutationClaim::for_location(raw.clone()),
                MutationClaim::for_location(decoded_left.clone()),
            ])
            .is_err()
        );
        assert!(
            claim_set(vec![MutationClaim::for_location(raw)])
                .conflict_with(&claim_set(vec![MutationClaim::for_location(
                    decoded_left.clone(),
                )]))
                .is_some()
        );
        assert!(
            MutationClaimSet::new(vec![
                MutationClaim::for_location(decoded_left),
                MutationClaim::for_location(decoded_right),
            ])
            .is_ok()
        );
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
        let header_descendant = claim_set(vec![MutationClaim::for_location(descendant(1))]);
        let covered_descendant = claim_set(vec![MutationClaim::for_location(descendant(2))]);
        let other_command = claim_set(vec![MutationClaim::for_location(descendant(3))]);

        assert!(dialogue.conflict_with(&header_descendant).is_none());
        assert!(dialogue.conflict_with(&covered_descendant).is_some());
        assert!(dialogue.conflict_with(&other_command).is_none());
    }

    #[test]
    fn event_block_claim_rejects_empty_and_cross_source_coverage() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let header = RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let covered = RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(2)]);
        let cross_source = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );

        assert!(MutationClaim::event_block(header.clone(), vec![covered]).is_ok());
        assert!(matches!(
            MutationClaim::event_block(header.clone(), Vec::new()),
            Err(ProjectionModelError::EventBlockCoverageRequired)
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

    #[test]
    fn content_structure_validation_rejects_kind_role_mismatches() {
        let scalar = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"));
        let speaker = TextUnitRole::DialogueSpeaker;
        let body = TextUnitRole::DialogueBody;
        let choices = TextUnitRole::Choices;
        let scrolling = TextUnitRole::ScrollingText;

        for kind in [
            TextGroupKind::DatabaseEntry,
            TextGroupKind::System,
            TextGroupKind::Map,
            TextGroupKind::EventCommand,
            TextGroupKind::PluginParameter,
        ] {
            assert_eq!(
                validate_text_unit_content_structure(
                    kind,
                    &scalar,
                    TextUnitContentView::Value("值")
                ),
                Ok(())
            );
        }
        for (kind, role, content) in [
            (
                TextGroupKind::EventDialogue,
                &speaker,
                TextUnitContentView::Value("姓名"),
            ),
            (
                TextGroupKind::EventDialogue,
                &body,
                TextUnitContentView::Lines(&[]),
            ),
            (
                TextGroupKind::EventChoices,
                &choices,
                TextUnitContentView::Lines(&[]),
            ),
            (
                TextGroupKind::EventScrollingText,
                &scrolling,
                TextUnitContentView::Lines(&[]),
            ),
        ] {
            assert_eq!(
                validate_text_unit_content_structure(kind, role, content),
                Ok(())
            );
        }
        for (kind, role, content) in [
            (
                TextGroupKind::EventDialogue,
                &scalar,
                TextUnitContentView::Value("值"),
            ),
            (
                TextGroupKind::DatabaseEntry,
                &speaker,
                TextUnitContentView::Value("姓名"),
            ),
            (
                TextGroupKind::EventChoices,
                &body,
                TextUnitContentView::Lines(&[]),
            ),
            (
                TextGroupKind::EventScrollingText,
                &choices,
                TextUnitContentView::Lines(&[]),
            ),
        ] {
            assert_eq!(
                validate_text_unit_content_structure(kind, role, content),
                Err(TextUnitContentStructureError::KindRoleMismatch)
            );
        }
    }

    #[test]
    fn content_structure_validation_preserves_the_value_and_line_control_contract() {
        let scalar = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"));
        let speaker = TextUnitRole::DialogueSpeaker;
        let body = TextUnitRole::DialogueBody;

        assert_eq!(
            validate_text_unit_content_structure(
                TextGroupKind::DatabaseEntry,
                &scalar,
                TextUnitContentView::Value("第一行\r第二行\n第三行")
            ),
            Ok(())
        );
        for invalid in ["值\0", "\0值"] {
            assert_eq!(
                validate_text_unit_content_structure(
                    TextGroupKind::DatabaseEntry,
                    &scalar,
                    TextUnitContentView::Value(invalid)
                ),
                Err(TextUnitContentStructureError::InvalidText { line_index: 0 })
            );
        }
        for invalid in ["姓名\r别名", "姓名\n别名", "姓名\0别名"] {
            assert_eq!(
                validate_text_unit_content_structure(
                    TextGroupKind::EventDialogue,
                    &speaker,
                    TextUnitContentView::Value(invalid)
                ),
                Err(TextUnitContentStructureError::InvalidText { line_index: 0 })
            );
        }
        for invalid in ["第二\r行", "第二\n行", "第二\0行"] {
            assert_eq!(
                validate_text_unit_content_structure(
                    TextGroupKind::EventDialogue,
                    &body,
                    TextUnitContentView::Lines(&["第一行".to_owned(), invalid.to_owned()])
                ),
                Err(TextUnitContentStructureError::InvalidText { line_index: 1 })
            );
        }
        assert_eq!(
            validate_text_unit_content_structure(
                TextGroupKind::EventDialogue,
                &body,
                TextUnitContentView::Value("错误形状")
            ),
            Err(TextUnitContentStructureError::ShapeMismatch)
        );
        assert_eq!(
            validate_text_unit_content_structure(
                TextGroupKind::DatabaseEntry,
                &scalar,
                TextUnitContentView::Lines(&["错误形状".to_owned()])
            ),
            Err(TextUnitContentStructureError::ShapeMismatch)
        );
    }
}
