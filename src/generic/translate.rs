//! Generic 的全局去重、文件内任务拆分与字符串响应验收。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::LanguagePair;
use crate::translation_protocol::ParsedTranslationResponse;
#[cfg(test)]
use crate::translation_protocol::{
    TranslationResponseEnvelope, TranslationTaskResponseParseError, parse_translation_response,
};

use super::placeholder::GenericProtectedText;
use super::project::{
    GenericProject, GenericStoredGroup, GenericStoredSnapshot, GenericStoredTranslation,
    GenericStoredUnit, TranslationClear, TranslationOrigin, TranslationWrite,
};

/// 一个 Generic Unit 的项目全局稳定位置。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct GenericUnitKey {
    group_id: String,
    unit_id: String,
}

impl GenericUnitKey {
    pub(crate) fn new(group_id: String, unit_id: String) -> Self {
        Self { group_id, unit_id }
    }

    pub(crate) fn group_id(&self) -> &str {
        &self.group_id
    }

    pub(crate) fn unit_id(&self) -> &str {
        &self.unit_id
    }
}

/// 公共语言、术语和 Placeholder 能力为一个 Unit 建立的规划事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlanningUnit {
    key: GenericUnitKey,
    protected_text: String,
    placeholder_binding_fingerprint: Sha256Fingerprint,
    terminology_indices: Vec<usize>,
    needs_translation: bool,
    current_translation: Option<String>,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
    invalidated_previous: Option<GenericStoredTranslation>,
}

/// 自动翻译状态中由公共翻译能力提供的实际资源身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticStateResources {
    pub(crate) prompt: Sha256Fingerprint,
    pub(crate) client_semantics: Sha256Fingerprint,
    pub(crate) language_module: Sha256Fingerprint,
    pub(crate) terminology_hits: Sha256Fingerprint,
}

impl PlanningUnit {
    #[cfg(test)]
    pub(crate) fn new(
        key: GenericUnitKey,
        protected_text: String,
        placeholder_binding_fingerprint: Sha256Fingerprint,
        needs_translation: bool,
        current_translation: Option<String>,
        expected_state_fingerprint: Sha256Fingerprint,
    ) -> Self {
        Self {
            key,
            protected_text,
            placeholder_binding_fingerprint,
            terminology_indices: Vec::new(),
            needs_translation,
            current_translation,
            expected_state_fingerprint,
            expected_previous: None,
            invalidated_previous: None,
        }
    }

    /// 用持久化记录和本次实际资源计算 Current，调用方不需要解释状态字段。
    pub(crate) fn from_stored(
        project: &GenericProject,
        group: &GenericStoredGroup,
        unit: &GenericStoredUnit,
        protected: &GenericProtectedText,
        terminology_indices: Vec<usize>,
        needs_translation: bool,
        resources: AutomaticStateResources,
    ) -> Self {
        let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
        let placeholder_binding_fingerprint = protected.binding_fingerprint();
        let automatic_state = automatic_translation_state_fingerprint(
            project.language_pair(),
            &key,
            unit.source_text(),
            group.context_fingerprint(),
            placeholder_binding_fingerprint,
            resources,
        );
        let current_translation = current_translation_for_stored(
            project,
            group,
            unit,
            placeholder_binding_fingerprint,
            Some(resources),
        );
        let invalidated_previous = current_translation
            .is_none()
            .then(|| unit.translation().cloned())
            .flatten();
        Self {
            key,
            protected_text: protected.text().to_owned(),
            placeholder_binding_fingerprint,
            terminology_indices,
            needs_translation,
            current_translation,
            expected_state_fingerprint: automatic_state,
            expected_previous: if invalidated_previous.is_none() {
                unit.translation().cloned()
            } else {
                None
            },
            invalidated_previous,
        }
    }

    pub(crate) fn key(&self) -> &GenericUnitKey {
        &self.key
    }

    pub(crate) fn needs_candidate(&self) -> bool {
        self.needs_translation && self.current_translation.is_none()
    }

    pub(crate) fn needs_planning(&self) -> bool {
        self.invalidated_previous.is_some() || self.needs_candidate()
    }

    #[cfg(test)]
    pub(crate) fn current_translation(&self) -> Option<&str> {
        self.current_translation.as_deref()
    }
}

/// 依据持久化来源类型和本次语义资源判断一个已有译文是否仍为 Current。
///
/// 人工状态不依赖自动资源；缺少自动资源时只把自动译文视为无法证明为 Current。
pub(crate) fn current_translation_for_stored(
    project: &GenericProject,
    group: &GenericStoredGroup,
    unit: &GenericStoredUnit,
    placeholder_binding_fingerprint: Sha256Fingerprint,
    automatic_resources: Option<AutomaticStateResources>,
) -> Option<String> {
    let translation = unit.translation()?;
    let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
    let expected = match translation.origin() {
        TranslationOrigin::Automatic => automatic_resources.map(|resources| {
            automatic_translation_state_fingerprint(
                project.language_pair(),
                &key,
                unit.source_text(),
                group.context_fingerprint(),
                placeholder_binding_fingerprint,
                resources,
            )
        }),
        TranslationOrigin::Manual => Some(manual_translation_state_fingerprint(
            project.language_pair(),
            &key,
            unit.source_text(),
            group.context_fingerprint(),
            placeholder_binding_fingerprint,
        )),
    };
    (expected == Some(translation.state_fingerprint()))
        .then(|| translation.translation().to_owned())
}

/// 当前 Translate 已确认失效、必须在模型请求前清除的旧译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedInvalidation {
    key: GenericUnitKey,
    expected_source_text: String,
    expected_group_context: Sha256Fingerprint,
    expected_translation: GenericStoredTranslation,
}

impl PlannedInvalidation {
    pub(crate) fn into_clear(self) -> TranslationClear {
        TranslationClear {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            expected_source_text: self.expected_source_text,
            expected_group_context: self.expected_group_context,
            expected_translation: self.expected_translation,
        }
    }
}

/// 一个无需请求模型即可传播的译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedReuse {
    key: GenericUnitKey,
    translation: String,
    expected_source_text: String,
    expected_group_context: Sha256Fingerprint,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
}

impl PlannedReuse {
    #[cfg(test)]
    pub(crate) fn key(&self) -> &GenericUnitKey {
        &self.key
    }

    #[cfg(test)]
    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }

    pub(crate) fn into_write(self) -> TranslationWrite {
        TranslationWrite {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            expected_source_text: self.expected_source_text,
            expected_group_context: self.expected_group_context,
            translation: self.translation,
            origin: TranslationOrigin::Automatic,
            state_fingerprint: self.expected_state_fingerprint,
            expected_translation: self.expected_previous,
        }
    }
}

/// Task 中一个只负责提供上下文或要求输出的 Unit。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedContextUnit {
    output_id: Option<u64>,
    text: String,
}

impl PlannedContextUnit {
    pub(crate) const fn output_id(&self) -> Option<u64> {
        self.output_id
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// Task 中不可拆开的完整 Group。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedGroup {
    kind: String,
    terminology_indices: Vec<usize>,
    units: Vec<PlannedContextUnit>,
}

impl PlannedGroup {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn units(&self) -> &[PlannedContextUnit] {
        &self.units
    }

    pub(crate) fn terminology_indices(&self) -> &[usize] {
        &self.terminology_indices
    }

    pub(crate) fn output_count(&self) -> usize {
        self.units
            .iter()
            .filter(|unit| unit.output_id.is_some())
            .count()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlannedDestination {
    key: GenericUnitKey,
    expected_source_text: String,
    expected_group_context: Sha256Fingerprint,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
}

/// 一个不能跨 JSONL 文件的模型任务。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedTask {
    relative_path: PathBuf,
    groups: Vec<PlannedGroup>,
    estimated_characters: usize,
    outputs: BTreeMap<u64, Vec<PlannedDestination>>,
}

impl PlannedTask {
    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn groups(&self) -> &[PlannedGroup] {
        &self.groups
    }

    #[cfg(test)]
    pub(crate) const fn estimated_characters(&self) -> usize {
        self.estimated_characters
    }

    pub(crate) fn expected_output_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.outputs.keys().copied()
    }
}

/// 一次 Generic Translate 的确定性规划结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationPlan {
    invalidations: Vec<PlannedInvalidation>,
    reused: Vec<PlannedReuse>,
    tasks: Vec<PlannedTask>,
}

impl TranslationPlan {
    pub(crate) const fn empty() -> Self {
        Self {
            invalidations: Vec::new(),
            reused: Vec::new(),
            tasks: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidations(&self) -> &[PlannedInvalidation] {
        &self.invalidations
    }

    #[cfg(test)]
    pub(crate) fn reused(&self) -> &[PlannedReuse] {
        &self.reused
    }

    #[cfg(test)]
    pub(crate) fn tasks(&self) -> &[PlannedTask] {
        &self.tasks
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<PlannedInvalidation>,
        Vec<PlannedReuse>,
        Vec<PlannedTask>,
    ) {
        (self.invalidations, self.reused, self.tasks)
    }
}

/// 规划输入与 Extract 快照不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GenericPlanningError {
    Missing(GenericUnitKey),
    Unknown(GenericUnitKey),
    Duplicate(GenericUnitKey),
}

impl fmt::Display for GenericPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(key) => write!(
                formatter,
                "缺少 Generic Unit 的规划事实：{}/{}",
                key.group_id, key.unit_id
            ),
            Self::Unknown(key) => write!(
                formatter,
                "规划事实引用了不存在的 Generic Unit：{}/{}",
                key.group_id, key.unit_id
            ),
            Self::Duplicate(key) => write!(
                formatter,
                "同一 Generic Unit 出现多份规划事实：{}/{}",
                key.group_id, key.unit_id
            ),
        }
    }
}

impl Error for GenericPlanningError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DeduplicationKey {
    source_text: String,
    protected_text: String,
    placeholder_binding_fingerprint: Sha256Fingerprint,
}

#[derive(Clone)]
struct UnitFacts<'a> {
    input: &'a PlanningUnit,
    source_text: &'a str,
    group_context: Sha256Fingerprint,
}

struct Family {
    members: Vec<GenericUnitKey>,
}

/// 按自然顺序计算全项目去重，并为每个文件形成一个待精确拆分的 Task 草案。
///
/// 此处不能按原文长度提前拆分，否则后续按最终 user message 大小重新拆分时，旧边界
/// 会留下无法与后续 Group 合并的小 Task。文件边界仍在这里建立，最终拆分只在文件内进行。
pub(crate) fn plan_translation(
    snapshot: &GenericStoredSnapshot,
    planning_units: &[PlanningUnit],
    reuse_validator: impl Fn(&GenericUnitKey, &str) -> Result<String, String>,
) -> Result<TranslationPlan, GenericPlanningError> {
    let mut supplied = HashMap::with_capacity(planning_units.len());
    for unit in planning_units {
        if supplied.insert(unit.key.clone(), unit).is_some() {
            return Err(GenericPlanningError::Duplicate(unit.key.clone()));
        }
    }

    let mut facts = HashMap::with_capacity(snapshot.unit_count());
    let mut natural_keys = Vec::with_capacity(snapshot.unit_count());
    for file in snapshot.files() {
        for group in file.groups() {
            for unit in group.units() {
                let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
                let input = supplied
                    .get(&key)
                    .copied()
                    .ok_or_else(|| GenericPlanningError::Missing(key.clone()))?;
                facts.insert(
                    key.clone(),
                    UnitFacts {
                        input,
                        source_text: unit.source_text(),
                        group_context: group.context_fingerprint(),
                    },
                );
                natural_keys.push(key);
            }
        }
    }
    if let Some(unknown) = supplied.keys().find(|key| !facts.contains_key(*key)) {
        return Err(GenericPlanningError::Unknown(unknown.clone()));
    }

    let invalidations = natural_keys
        .iter()
        .filter_map(|key| {
            let fact = &facts[key];
            fact.input
                .invalidated_previous
                .clone()
                .map(|expected_translation| PlannedInvalidation {
                    key: key.clone(),
                    expected_source_text: fact.source_text.to_owned(),
                    expected_group_context: fact.group_context,
                    expected_translation,
                })
        })
        .collect();

    let mut family_indices: HashMap<DeduplicationKey, usize> = HashMap::new();
    let mut families: Vec<Family> = Vec::new();
    for key in &natural_keys {
        let fact = &facts[key];
        let deduplication_key = DeduplicationKey {
            source_text: fact.source_text.to_owned(),
            protected_text: fact.input.protected_text.clone(),
            placeholder_binding_fingerprint: fact.input.placeholder_binding_fingerprint,
        };
        let family_index = *family_indices.entry(deduplication_key).or_insert_with(|| {
            families.push(Family {
                members: Vec::new(),
            });
            families.len() - 1
        });
        families[family_index].members.push(key.clone());
    }

    let mut reused = Vec::new();
    let mut representative_destinations = HashMap::new();
    let mut known_targets: HashMap<GenericUnitKey, String> = HashMap::new();
    for family in &families {
        let mut first_current = None::<&str>;
        let mut multiple_currents = false;
        for key in &family.members {
            let Some(current) = facts[key].input.current_translation.as_deref() else {
                continue;
            };
            match first_current {
                None => first_current = Some(current),
                Some(first) if first != current => {
                    multiple_currents = true;
                    break;
                }
                Some(_) => {}
            }
        }
        for key in &family.members {
            if let Some(current) = &facts[key].input.current_translation {
                known_targets.insert(key.clone(), current.clone());
            }
        }

        let unresolved = family
            .members
            .iter()
            .filter(|key| {
                let fact = &facts[*key];
                fact.input.needs_translation && fact.input.current_translation.is_none()
            })
            .cloned()
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            continue;
        }

        if !multiple_currents {
            let Some(translation) = first_current.map(str::to_owned) else {
                let representative = unresolved[0].clone();
                let destinations = unresolved
                    .iter()
                    .map(|key| {
                        let fact = &facts[key];
                        PlannedDestination {
                            key: key.clone(),
                            expected_source_text: fact.source_text.to_owned(),
                            expected_group_context: fact.group_context,
                            expected_state_fingerprint: fact.input.expected_state_fingerprint,
                            expected_previous: fact.input.expected_previous.clone(),
                        }
                    })
                    .collect();
                representative_destinations.insert(representative, destinations);
                continue;
            };
            let mut model_destinations = Vec::new();
            for key in unresolved {
                let fact = &facts[&key];
                let destination = PlannedDestination {
                    key: key.clone(),
                    expected_source_text: fact.source_text.to_owned(),
                    expected_group_context: fact.group_context,
                    expected_state_fingerprint: fact.input.expected_state_fingerprint,
                    expected_previous: fact.input.expected_previous.clone(),
                };
                match reuse_validator(&key, &translation) {
                    Ok(validated_translation) => {
                        reused.push(PlannedReuse {
                            key: key.clone(),
                            translation: validated_translation.clone(),
                            expected_source_text: destination.expected_source_text,
                            expected_group_context: destination.expected_group_context,
                            expected_state_fingerprint: destination.expected_state_fingerprint,
                            expected_previous: destination.expected_previous,
                        });
                        known_targets.insert(key, validated_translation);
                    }
                    Err(_) => model_destinations.push(destination),
                }
            }
            if let Some(representative) = model_destinations
                .first()
                .map(|destination| destination.key.clone())
            {
                representative_destinations.insert(representative, model_destinations);
            }
            continue;
        }

        let representative = unresolved[0].clone();
        let destinations = unresolved
            .iter()
            .map(|key| {
                let fact = &facts[key];
                PlannedDestination {
                    key: key.clone(),
                    expected_source_text: fact.source_text.to_owned(),
                    expected_group_context: fact.group_context,
                    expected_state_fingerprint: fact.input.expected_state_fingerprint,
                    expected_previous: fact.input.expected_previous.clone(),
                }
            })
            .collect();
        representative_destinations.insert(representative, destinations);
    }

    let mut tasks = Vec::new();
    for file in snapshot.files() {
        let mut drafts = Vec::new();
        for group in file.groups() {
            let group_representatives = group
                .units()
                .iter()
                .filter_map(|unit| {
                    let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
                    representative_destinations
                        .contains_key(&key)
                        .then_some(key)
                })
                .collect::<HashSet<_>>();
            if group_representatives.is_empty() {
                continue;
            }

            let units = group
                .units()
                .iter()
                .map(|unit| {
                    let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
                    let fact = &facts[&key];
                    let text = known_targets
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| fact.input.protected_text.clone());
                    DraftUnit {
                        representative: group_representatives.contains(&key).then_some(key),
                        text,
                    }
                })
                .collect::<Vec<_>>();
            let estimated_characters = group.kind().chars().count()
                + units
                    .iter()
                    .map(|unit| unit.text.chars().count())
                    .sum::<usize>();
            drafts.push(GroupDraft {
                kind: group.kind().to_owned(),
                terminology_indices: group
                    .units()
                    .first()
                    .map(|unit| {
                        let key = GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned());
                        facts[&key].input.terminology_indices.clone()
                    })
                    .unwrap_or_default(),
                units,
                estimated_characters,
            });
        }

        if !drafts.is_empty() {
            let estimated_characters = drafts.iter().fold(0_usize, |characters, draft| {
                characters.saturating_add(draft.estimated_characters)
            });
            tasks.push(finalize_task(
                file.relative_path().to_path_buf(),
                drafts,
                estimated_characters,
                &mut representative_destinations,
            ));
        }
    }
    debug_assert!(representative_destinations.is_empty());

    Ok(TranslationPlan {
        invalidations,
        reused,
        tasks,
    })
}

/// 按最终模型 user message 的实际字符数重新拆分已有 TaskBlock。
///
/// 初步规划已经保证每个文件只有一个 Task 草案且 Group 不拆分；这里按最终大小在
/// 文件内连续贪心拆分，并为每个新 Task 重新编号临时输出 ID。单个 Group 超过目标时
/// 仍独占任务。
pub(crate) fn split_tasks_by_rendered_size(
    plan: TranslationPlan,
    target_task_characters: NonZeroUsize,
    fixed_characters: usize,
    measure_group: impl Fn(&PlannedGroup, u64) -> usize,
) -> TranslationPlan {
    let TranslationPlan {
        invalidations,
        reused,
        tasks,
    } = plan;
    let mut split_tasks = Vec::new();
    for task in tasks {
        let PlannedTask {
            relative_path,
            groups,
            mut outputs,
            ..
        } = task;
        let mut current = Vec::new();
        let mut current_characters = fixed_characters;
        let mut next_output_id = 1_u64;
        for group in groups {
            let mut group_characters = measure_group(&group, next_output_id);
            if !current.is_empty()
                && current_characters.saturating_add(group_characters)
                    > target_task_characters.get()
            {
                split_tasks.push(rebuild_task(
                    relative_path.clone(),
                    std::mem::take(&mut current),
                    &mut outputs,
                    current_characters,
                ));
                current_characters = fixed_characters;
                next_output_id = 1;
                group_characters = measure_group(&group, next_output_id);
            }
            next_output_id = next_output_id
                .saturating_add(u64::try_from(group.output_count()).unwrap_or(u64::MAX));
            current_characters = current_characters.saturating_add(group_characters);
            current.push(group);
        }
        if !current.is_empty() {
            split_tasks.push(rebuild_task(
                relative_path,
                current,
                &mut outputs,
                current_characters,
            ));
        }
        debug_assert!(outputs.is_empty());
    }
    TranslationPlan {
        invalidations,
        reused,
        tasks: split_tasks,
    }
}

fn rebuild_task(
    relative_path: PathBuf,
    groups: Vec<PlannedGroup>,
    previous_outputs: &mut BTreeMap<u64, Vec<PlannedDestination>>,
    estimated_characters: usize,
) -> PlannedTask {
    let mut outputs = BTreeMap::new();
    let mut next_output_id = 1_u64;
    let groups = groups
        .into_iter()
        .map(|group| PlannedGroup {
            kind: group.kind,
            terminology_indices: group.terminology_indices,
            units: group
                .units
                .into_iter()
                .map(|unit| {
                    let output_id = unit.output_id.map(|previous_id| {
                        let output_id = next_output_id;
                        next_output_id += 1;
                        let destinations = previous_outputs
                            .remove(&previous_id)
                            .expect("Task 输出必须保留对应的传播目标");
                        outputs.insert(output_id, destinations);
                        output_id
                    });
                    PlannedContextUnit {
                        output_id,
                        text: unit.text,
                    }
                })
                .collect(),
        })
        .collect();
    PlannedTask {
        relative_path,
        groups,
        estimated_characters,
        outputs,
    }
}

struct DraftUnit {
    representative: Option<GenericUnitKey>,
    text: String,
}

struct GroupDraft {
    kind: String,
    terminology_indices: Vec<usize>,
    units: Vec<DraftUnit>,
    estimated_characters: usize,
}

fn finalize_task(
    relative_path: PathBuf,
    drafts: Vec<GroupDraft>,
    estimated_characters: usize,
    representative_destinations: &mut HashMap<GenericUnitKey, Vec<PlannedDestination>>,
) -> PlannedTask {
    let mut outputs = BTreeMap::new();
    let mut next_output_id = 1_u64;
    let groups = drafts
        .into_iter()
        .map(|draft| {
            let units = draft
                .units
                .into_iter()
                .map(|unit| {
                    let output_id = unit.representative.map(|representative| {
                        let id = next_output_id;
                        next_output_id += 1;
                        outputs.insert(
                            id,
                            representative_destinations
                                .remove(&representative)
                                .expect("模型代表必须保留对应的传播目标"),
                        );
                        id
                    });
                    PlannedContextUnit {
                        output_id,
                        text: unit.text,
                    }
                })
                .collect();
            PlannedGroup {
                kind: draft.kind,
                terminology_indices: draft.terminology_indices,
                units,
            }
        })
        .collect();
    PlannedTask {
        relative_path,
        groups,
        estimated_characters,
        outputs,
    }
}

/// 一个通过全部验收、需要写入具体 Unit 的模型结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedTranslation {
    key: GenericUnitKey,
    translation: String,
    expected_source_text: String,
    expected_group_context: Sha256Fingerprint,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
}

impl AcceptedTranslation {
    pub(crate) fn into_write(self) -> TranslationWrite {
        TranslationWrite {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            expected_source_text: self.expected_source_text,
            expected_group_context: self.expected_group_context,
            translation: self.translation,
            origin: TranslationOrigin::Automatic,
            state_fingerprint: self.expected_state_fingerprint,
            expected_translation: self.expected_previous,
        }
    }
}

/// 可解析响应中单个 ID 的问题；其他合法 ID 仍可保存。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResponseProblem {
    InvalidId(String),
    UnexpectedId(u64),
    DuplicateId(u64),
    MissingId(u64),
    NonStringValue(u64),
    InvalidTranslation {
        output_id: u64,
        detail: String,
    },
    InvalidDestination {
        output_id: u64,
        key: GenericUnitKey,
        detail: String,
    },
}

/// 一次响应的部分验收结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationAcceptance {
    accepted: Vec<AcceptedTranslation>,
    problems: Vec<ResponseProblem>,
    accepted_output_count: usize,
}

impl TranslationAcceptance {
    #[cfg(test)]
    pub(crate) fn accepted(&self) -> &[AcceptedTranslation] {
        &self.accepted
    }

    #[cfg(test)]
    pub(crate) fn problems(&self) -> &[ResponseProblem] {
        &self.problems
    }

    /// 返回至少有一个目标 Unit 通过验收的模型输出数量。
    pub(crate) const fn accepted_output_count(&self) -> usize {
        self.accepted_output_count
    }

    pub(crate) fn into_parts(self) -> (Vec<AcceptedTranslation>, Vec<ResponseProblem>) {
        (self.accepted, self.problems)
    }
}

/// 响应整体不是可验收的严格 JSON object。
#[derive(Debug)]
#[cfg(test)]
pub(crate) struct GenericResponseError {
    source: TranslationTaskResponseParseError,
}

#[cfg(test)]
impl fmt::Display for GenericResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Generic 模型响应不符合翻译响应协议：{}",
            self.source.business_message()
        )
    }
}

#[cfg(test)]
impl Error for GenericResponseError {}

/// 验收 Generic 的 `{"1":"译文"}` 响应。
///
/// `validator` 负责 Placeholder 恢复、语言残留检查和可选安全修复。它返回最终应
/// 保存的译文，或者只影响当前 ID 的具体原因。
#[cfg(test)]
pub(crate) fn accept_response(
    task: &PlannedTask,
    assistant_response: &str,
    response_envelope: TranslationResponseEnvelope,
    validator: impl FnMut(u64, &GenericUnitKey, &str) -> Result<String, String>,
) -> Result<TranslationAcceptance, GenericResponseError> {
    let parsed = parse_translation_response(assistant_response, response_envelope)
        .map_err(|source| GenericResponseError { source })?;
    Ok(accept_parsed_response(task.clone(), &parsed, validator))
}

/// 验收已经由公共协议解析器建立的 Generic 响应投影。
///
/// 记录任务时，调用方可以让解析投影同时进入旁路文档，避免再次解析模型正文。
pub(crate) fn accept_parsed_response(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    mut validator: impl FnMut(u64, &GenericUnitKey, &str) -> Result<String, String>,
) -> TranslationAcceptance {
    let entries = parsed.entries();
    let mut canonical_counts = HashMap::new();
    for entry in entries {
        if let Some(output_id) = entry.canonical_id().and_then(|id| u64::try_from(id).ok()) {
            *canonical_counts.entry(output_id).or_insert(0usize) += 1;
        }
    }

    let mut accepted = Vec::new();
    let mut problems = Vec::new();
    let mut accepted_output_count = 0;
    let mut observed = HashSet::new();
    let mut reported_duplicates = HashSet::new();
    let mut outputs = task.outputs;
    for entry in entries {
        let Some(output_id) = entry.canonical_id().and_then(|id| u64::try_from(id).ok()) else {
            problems.push(ResponseProblem::InvalidId(entry.id().to_owned()));
            continue;
        };
        if !outputs.contains_key(&output_id) {
            problems.push(ResponseProblem::UnexpectedId(output_id));
            continue;
        }
        observed.insert(output_id);
        if canonical_counts
            .get(&output_id)
            .copied()
            .unwrap_or_default()
            > 1
        {
            if reported_duplicates.insert(output_id) {
                problems.push(ResponseProblem::DuplicateId(output_id));
            }
            continue;
        }
        let Value::String(candidate) = entry.value() else {
            problems.push(ResponseProblem::NonStringValue(output_id));
            continue;
        };
        if let Err(detail) = validate_candidate_text(candidate) {
            problems.push(ResponseProblem::InvalidTranslation {
                output_id,
                detail: detail.to_owned(),
            });
            continue;
        }
        let destinations = std::mem::take(
            outputs
                .get_mut(&output_id)
                .expect("已确认的模型输出必须仍属于当前 Generic 任务"),
        );
        let mut output_accepted = false;
        for destination in destinations {
            let candidate = match validator(output_id, &destination.key, candidate) {
                Ok(candidate) => candidate,
                Err(detail) => {
                    problems.push(ResponseProblem::InvalidDestination {
                        output_id,
                        key: destination.key,
                        detail,
                    });
                    continue;
                }
            };
            if let Err(detail) = validate_candidate_text(&candidate) {
                problems.push(ResponseProblem::InvalidDestination {
                    output_id,
                    key: destination.key,
                    detail: detail.to_owned(),
                });
                continue;
            }
            accepted.push(AcceptedTranslation {
                key: destination.key,
                translation: candidate,
                expected_source_text: destination.expected_source_text,
                expected_group_context: destination.expected_group_context,
                expected_state_fingerprint: destination.expected_state_fingerprint,
                expected_previous: destination.expected_previous,
            });
            output_accepted = true;
        }
        if output_accepted {
            accepted_output_count += 1;
        }
    }
    for output_id in outputs.keys() {
        if !observed.contains(output_id) {
            problems.push(ResponseProblem::MissingId(*output_id));
        }
    }
    TranslationAcceptance {
        accepted,
        problems,
        accepted_output_count,
    }
}

fn validate_candidate_text(candidate: &str) -> Result<(), &'static str> {
    if candidate.chars().all(char::is_whitespace) {
        return Err("译文不能为空白");
    }
    if candidate.contains('\r') {
        return Err("译文不能包含 CR（U+000D）");
    }
    if candidate.contains('\0') {
        return Err("译文不能包含 NUL（U+0000）");
    }
    Ok(())
}

/// 建立自动译文 Current 所需的完整语义状态。
pub(crate) fn automatic_translation_state_fingerprint(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
    resources: AutomaticStateResources,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.translation-state.automatic");
    frame_unit_semantics(
        &mut hasher,
        language_pair,
        key,
        source_text,
        group_context,
        placeholder_binding,
    );
    hasher
        .frame(20, resources.prompt.as_bytes())
        .frame(21, resources.client_semantics.as_bytes())
        .frame(22, resources.language_module.as_bytes())
        .frame(23, resources.terminology_hits.as_bytes());
    hasher.finish()
}

/// 建立人工译文状态；Prompt、Profile、Client 和术语变化不会使它失效。
pub(crate) fn manual_translation_state_fingerprint(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.translation-state.manual");
    frame_unit_semantics(
        &mut hasher,
        language_pair,
        key,
        source_text,
        group_context,
        placeholder_binding,
    );
    hasher.finish()
}

fn frame_unit_semantics(
    hasher: &mut Sha256FramedHasher,
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
) {
    hasher
        .frame(1, language_pair.source().as_str().as_bytes())
        .frame(2, language_pair.target().as_str().as_bytes())
        .frame(3, key.group_id().as_bytes())
        .frame(4, key.unit_id().as_bytes())
        .frame(5, source_text.as_bytes())
        .frame(6, group_context.as_bytes())
        .frame(7, placeholder_binding.as_bytes());
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use crate::language::{LanguageId, LanguagePair};
    use crate::project_name::ProjectName;

    use super::*;
    use crate::generic::project::{
        GenericProject, GenericStoredFile, GenericStoredGroup, GenericStoredSnapshot,
        GenericStoredTranslation, GenericStoredUnit,
    };

    fn fingerprint(value: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([value; 32])
    }

    fn stored_snapshot() -> GenericStoredSnapshot {
        let make_group =
            |id: &str, ordinal: usize, units: &[(&str, &str, Option<&str>)]| GenericStoredGroup {
                id: id.to_owned(),
                ordinal,
                kind: "dialogue".to_owned(),
                context_fingerprint: fingerprint(ordinal as u8 + 10),
                units: units
                    .iter()
                    .enumerate()
                    .map(
                        |(unit_ordinal, (id, source, translation))| GenericStoredUnit {
                            id: (*id).to_owned(),
                            ordinal: unit_ordinal,
                            source_text: (*source).to_owned(),
                            translation: translation.map(|translation| GenericStoredTranslation {
                                translation: translation.to_owned(),
                                origin: TranslationOrigin::Automatic,
                                state_fingerprint: fingerprint(90),
                            }),
                        },
                    )
                    .collect(),
            };
        GenericStoredSnapshot {
            project: GenericProject {
                project_name: "game".parse::<ProjectName>().unwrap(),
                workspace_root: PathBuf::from("workspace"),
                database_path: PathBuf::from("workspace/project.db"),
                source_root: PathBuf::from("source"),
                language_pair: LanguagePair::new(
                    LanguageId::parse("ja").unwrap(),
                    LanguageId::parse("zh-Hans").unwrap(),
                ),
                extracted_raw_fingerprint: Some(fingerprint(1)),
                extracted_asset_fingerprint: Some(fingerprint(2)),
                last_profile_id: None,
            },
            files: vec![
                GenericStoredFile {
                    relative_path: PathBuf::from("a.jsonl"),
                    ordinal: 0,
                    groups: vec![
                        make_group(
                            "g1",
                            0,
                            &[("u1", "同文", None), ("u2", "已有", Some("当前"))],
                        ),
                        make_group("g2", 1, &[("u1", "独立", None)]),
                    ],
                },
                GenericStoredFile {
                    relative_path: PathBuf::from("b.jsonl"),
                    ordinal: 1,
                    groups: vec![make_group("g3", 0, &[("u1", "同文", None)])],
                },
            ],
        }
    }

    fn planning(snapshot: &GenericStoredSnapshot) -> Vec<PlanningUnit> {
        snapshot
            .files()
            .iter()
            .flat_map(|file| file.groups())
            .flat_map(|group| {
                group.units().iter().map(|unit| {
                    PlanningUnit::new(
                        GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned()),
                        format!("<{}>", unit.source_text()),
                        fingerprint(if unit.source_text() == "同文" { 1 } else { 2 }),
                        true,
                        unit.translation()
                            .map(|translation| translation.translation().to_owned()),
                        fingerprint(7),
                    )
                })
            })
            .collect()
    }

    fn task_split_snapshot(file_group_counts: &[usize]) -> GenericStoredSnapshot {
        let mut next_group = 0_usize;
        let files = file_group_counts
            .iter()
            .enumerate()
            .map(|(file_ordinal, group_count)| {
                let groups = (0..*group_count)
                    .map(|group_ordinal| {
                        let index = next_group;
                        next_group += 1;
                        GenericStoredGroup {
                            id: format!("split-group-{index}"),
                            ordinal: group_ordinal,
                            kind: "k".to_owned(),
                            context_fingerprint: fingerprint(
                                u8::try_from(index + 40).expect("测试 Group 数量应可表示为 u8"),
                            ),
                            units: vec![GenericStoredUnit {
                                id: "unit".to_owned(),
                                ordinal: 0,
                                source_text: char::from(
                                    b'a' + u8::try_from(index)
                                        .expect("测试 Group 数量应可表示为 u8"),
                                )
                                .to_string(),
                                translation: None,
                            }],
                        }
                    })
                    .collect();
                GenericStoredFile {
                    relative_path: PathBuf::from(format!("{file_ordinal}.jsonl")),
                    ordinal: file_ordinal,
                    groups,
                }
            })
            .collect();
        GenericStoredSnapshot {
            project: stored_snapshot().project,
            files,
        }
    }

    #[test]
    fn exact_task_split_fills_across_the_removed_rough_boundary() {
        let snapshot = task_split_snapshot(&[10]);
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");

        assert_eq!(plan.tasks().len(), 1, "每个文件应只形成一个待拆分草案");
        assert_eq!(plan.tasks()[0].groups().len(), 10);
        assert_eq!(
            plan.tasks()[0].estimated_characters(),
            40,
            "每个 Group 的旧粗略估算为四个字符，旧逻辑会在第五个 Group 后截断"
        );

        let plan = split_tasks_by_rendered_size(
            plan,
            NonZeroUsize::new(20).expect("常量应非零"),
            0,
            |_, _| 9,
        );
        assert_eq!(
            plan.tasks()
                .iter()
                .map(|task| task.groups().len())
                .collect::<Vec<_>>(),
            [2, 2, 2, 2, 2]
        );
        assert_eq!(
            plan.tasks()[2]
                .groups()
                .iter()
                .map(|group| group.units()[0].text())
                .collect::<Vec<_>>(),
            ["<e>", "<f>"],
            "第五个和第六个 Group 必须越过旧粗拆边界进入同一最终 Task"
        );
        for task in plan.tasks() {
            assert_eq!(
                task.expected_output_ids().collect::<Vec<_>>(),
                [1, 2],
                "每个最终 Task 都应从 1 重新编号"
            );
        }
    }

    #[test]
    fn exact_task_split_never_combines_different_files() {
        let snapshot = task_split_snapshot(&[3, 3]);
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        assert_eq!(plan.tasks().len(), 2, "每个有输出的文件应形成独立草案");

        let plan = split_tasks_by_rendered_size(
            plan,
            NonZeroUsize::new(20).expect("常量应非零"),
            0,
            |_, _| 9,
        );
        assert_eq!(
            plan.tasks()
                .iter()
                .map(|task| (task.relative_path().to_path_buf(), task.groups().len()))
                .collect::<Vec<_>>(),
            [
                (PathBuf::from("0.jsonl"), 2),
                (PathBuf::from("0.jsonl"), 1),
                (PathBuf::from("1.jsonl"), 2),
                (PathBuf::from("1.jsonl"), 1),
            ]
        );
    }

    #[test]
    fn exact_task_split_keeps_an_oversized_group_alone() {
        let snapshot = task_split_snapshot(&[3]);
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let plan = split_tasks_by_rendered_size(
            plan,
            NonZeroUsize::new(20).expect("常量应非零"),
            0,
            |group, _| {
                if group.units()[0].text() == "<b>" {
                    25
                } else {
                    9
                }
            },
        );

        assert_eq!(
            plan.tasks()
                .iter()
                .map(|task| task.groups().len())
                .collect::<Vec<_>>(),
            [1, 1, 1]
        );
        assert_eq!(plan.tasks()[1].estimated_characters(), 25);
        assert_eq!(plan.tasks()[1].groups()[0].units()[0].text(), "<b>");
    }

    #[test]
    fn global_dedup_chooses_one_representative_without_crossing_file_tasks() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");

        assert_eq!(plan.tasks().len(), 1);
        assert_eq!(plan.tasks()[0].relative_path(), Path::new("a.jsonl"));
        assert_eq!(plan.tasks()[0].expected_output_ids().count(), 2);
        assert!(plan.reused().is_empty());
    }

    #[test]
    fn a_single_current_translation_propagates_without_a_model_task() {
        let mut snapshot = stored_snapshot();
        snapshot.files[0].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "相同".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(9),
        });
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();

        assert_eq!(plan.reused().len(), 1);
        assert_eq!(plan.reused()[0].key().group_id(), "g3");
        assert_eq!(plan.reused()[0].translation(), "相同");
    }

    #[test]
    fn current_reuse_validates_each_target_and_sends_failed_targets_to_the_model() {
        let mut snapshot = stored_snapshot();
        snapshot.files[0].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "相同".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(9),
        });
        snapshot.files[1].groups.push(GenericStoredGroup {
            id: "g4".to_owned(),
            ordinal: 1,
            kind: "name".to_owned(),
            context_fingerprint: fingerprint(14),
            units: vec![GenericStoredUnit {
                id: "u1".to_owned(),
                ordinal: 0,
                source_text: "同文".to_owned(),
                translation: None,
            }],
        });

        let plan = plan_translation(&snapshot, &planning(&snapshot), |key, candidate| {
            if key.group_id() == "g3" {
                Err("目标 kind 不接受该译文".to_owned())
            } else {
                Ok(format!("{candidate}-已验收"))
            }
        })
        .expect("复用验收失败不应中止规划");

        assert_eq!(plan.reused().len(), 1);
        assert_eq!(plan.reused()[0].key().group_id(), "g4");
        assert_eq!(plan.reused()[0].translation(), "相同-已验收");
        let model_destinations = plan
            .tasks()
            .iter()
            .flat_map(|task| task.outputs.values())
            .flatten()
            .filter(|destination| destination.expected_source_text == "同文")
            .map(|destination| destination.key.group_id())
            .collect::<Vec<_>>();
        assert_eq!(model_destinations, ["g3"]);
    }

    #[test]
    fn multiple_current_translations_coexist_and_only_untranslated_members_request_model() {
        let mut snapshot = stored_snapshot();
        snapshot.files[0].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "译文甲".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(9),
        });
        snapshot.files[1].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "译文乙".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(10),
        });
        snapshot.files[1].groups.push(GenericStoredGroup {
            id: "g4".to_owned(),
            ordinal: 1,
            kind: "dialogue".to_owned(),
            context_fingerprint: fingerprint(14),
            units: vec![GenericStoredUnit {
                id: "u1".to_owned(),
                ordinal: 0,
                source_text: "同文".to_owned(),
                translation: None,
            }],
        });

        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("多个不同 Current 不应构成冲突");

        assert!(plan.reused().is_empty());
        let destinations = plan
            .tasks()
            .iter()
            .flat_map(|task| task.outputs.values())
            .flatten()
            .filter(|destination| destination.expected_source_text == "同文")
            .collect::<Vec<_>>();
        assert_eq!(destinations.len(), 1);
        assert_eq!(destinations[0].key.group_id(), "g4");
        assert_eq!(destinations[0].key.unit_id(), "u1");
    }

    #[test]
    fn automatic_current_tracks_semantics_while_manual_current_ignores_prompt_and_client() {
        let snapshot = stored_snapshot();
        let project = snapshot.project();
        let group = &snapshot.files()[0].groups()[0];
        let original = &group.units()[0];
        let key = GenericUnitKey::new(group.id().to_owned(), original.id().to_owned());
        let placeholder_service = super::super::placeholder::GenericPlaceholderService::default();
        let placeholder_rules = placeholder_service
            .compile(Vec::new())
            .expect("空 Placeholder 规则应能编译");
        let protected = placeholder_service
            .protect(group.kind(), original.source_text(), &placeholder_rules)
            .expect("无 Placeholder 的正文应能保护");
        let binding = protected.binding_fingerprint();
        let resources = AutomaticStateResources {
            prompt: fingerprint(21),
            client_semantics: fingerprint(22),
            language_module: fingerprint(23),
            terminology_hits: fingerprint(24),
        };
        let automatic_state = automatic_translation_state_fingerprint(
            project.language_pair(),
            &key,
            original.source_text(),
            group.context_fingerprint(),
            binding,
            resources,
        );
        let automatic = GenericStoredUnit {
            translation: Some(GenericStoredTranslation {
                translation: "直接 SQL 修改后的正文".to_owned(),
                origin: TranslationOrigin::Automatic,
                state_fingerprint: automatic_state,
            }),
            ..original.clone()
        };
        let current = PlanningUnit::from_stored(
            project,
            group,
            &automatic,
            &protected,
            Vec::new(),
            true,
            resources,
        );
        assert_eq!(
            current.current_translation(),
            Some("直接 SQL 修改后的正文"),
            "目标译文本身不属于语义状态，直接 SQL 修改正文后仍应为 Current"
        );

        let changed_resources = AutomaticStateResources {
            prompt: fingerprint(25),
            client_semantics: fingerprint(26),
            ..resources
        };
        let stale = PlanningUnit::from_stored(
            project,
            group,
            &automatic,
            &protected,
            Vec::new(),
            true,
            changed_resources,
        );
        assert!(stale.current_translation().is_none());

        let manual_state = manual_translation_state_fingerprint(
            project.language_pair(),
            &key,
            original.source_text(),
            group.context_fingerprint(),
            binding,
        );
        let manual = GenericStoredUnit {
            translation: Some(GenericStoredTranslation {
                translation: "人工修订".to_owned(),
                origin: TranslationOrigin::Manual,
                state_fingerprint: manual_state,
            }),
            ..original.clone()
        };
        let current = PlanningUnit::from_stored(
            project,
            group,
            &manual,
            &protected,
            Vec::new(),
            true,
            changed_resources,
        );
        assert_eq!(current.current_translation(), Some("人工修订"));
    }

    #[test]
    fn response_keeps_valid_ids_when_other_ids_are_invalid() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        let task = &plan.tasks()[0];
        let acceptance = accept_response(
            task,
            r#"{"1":"译文\n第二行","2":3,"99":"额外"}"#,
            TranslationResponseEnvelope::JsonOnly,
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("object 可解析");

        assert_eq!(acceptance.accepted().len(), 2, "同文族传播到两个 Unit");
        assert!(
            acceptance
                .problems()
                .contains(&ResponseProblem::NonStringValue(2))
        );
        assert!(
            acceptance
                .problems()
                .contains(&ResponseProblem::UnexpectedId(99))
        );
    }

    #[test]
    fn response_validates_each_destination_without_rejecting_valid_family_members() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        let task = &plan.tasks()[0];
        let acceptance = accept_response(
            task,
            r#"{"1":"同文译文","2":"独立译文"}"#,
            TranslationResponseEnvelope::JsonOnly,
            |_, key, candidate| {
                if key.group_id() == "g3" {
                    Err("目标 kind 不接受该译文".to_owned())
                } else {
                    Ok(candidate.to_owned())
                }
            },
        )
        .expect("object 可解析");

        assert!(acceptance.accepted().iter().any(|accepted| {
            accepted.key.group_id() == "g1" && accepted.translation == "同文译文"
        }));
        assert!(
            !acceptance
                .accepted()
                .iter()
                .any(|accepted| accepted.key.group_id() == "g3")
        );
        assert!(
            acceptance
                .problems()
                .contains(&ResponseProblem::InvalidDestination {
                    output_id: 1,
                    key: GenericUnitKey::new("g3".to_owned(), "u1".to_owned()),
                    detail: "目标 kind 不接受该译文".to_owned(),
                })
        );
        assert_eq!(
            acceptance.accepted_output_count(),
            2,
            "部分传播成功的 output_id 仍算已接受一个模型输出"
        );
    }

    #[test]
    fn response_does_not_count_an_output_when_all_destinations_fail() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        let acceptance = accept_response(
            &plan.tasks()[0],
            r#"{"1":"同文译文","2":"独立译文"}"#,
            TranslationResponseEnvelope::JsonOnly,
            |output_id, _, candidate| {
                if output_id == 1 {
                    Err("该去重族的目标均拒绝译文".to_owned())
                } else {
                    Ok(candidate.to_owned())
                }
            },
        )
        .expect("object 可解析");

        assert_eq!(acceptance.accepted_output_count(), 1);
        assert_eq!(acceptance.accepted().len(), 1);
        assert_eq!(acceptance.accepted()[0].key.group_id(), "g2");
        assert_eq!(
            acceptance.problems(),
            [
                ResponseProblem::InvalidDestination {
                    output_id: 1,
                    key: GenericUnitKey::new("g1".to_owned(), "u1".to_owned()),
                    detail: "该去重族的目标均拒绝译文".to_owned(),
                },
                ResponseProblem::InvalidDestination {
                    output_id: 1,
                    key: GenericUnitKey::new("g3".to_owned(), "u1".to_owned()),
                    detail: "该去重族的目标均拒绝译文".to_owned(),
                },
            ],
            "消费 destinations 后仍应保持原有目标顺序"
        );
    }

    #[test]
    fn duplicate_response_keys_only_reject_the_ambiguous_id() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        let acceptance = accept_response(
            &plan.tasks()[0],
            r#"{"1":"甲","1":"乙"}"#,
            TranslationResponseEnvelope::JsonOnly,
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("公共协议应保留重复项，交给逐 ID 验收");
        assert!(acceptance.accepted().is_empty());
        assert_eq!(
            acceptance.problems(),
            [
                ResponseProblem::DuplicateId(1),
                ResponseProblem::MissingId(2)
            ]
        );
    }

    #[test]
    fn stale_translation_is_cleared_before_new_writes_expect_an_empty_slot() {
        let snapshot = stored_snapshot();
        let mut planning_units = planning(&snapshot);
        let stale = snapshot.files()[0].groups()[0].units()[1]
            .translation()
            .expect("测试 Unit 应有旧译文")
            .clone();
        let stale_unit = planning_units
            .iter_mut()
            .find(|unit| unit.key().group_id() == "g1" && unit.key().unit_id() == "u2")
            .expect("应该找到测试 Unit");
        stale_unit.current_translation = None;
        stale_unit.expected_previous = None;
        stale_unit.invalidated_previous = Some(stale.clone());

        let plan = plan_translation(&snapshot, &planning_units, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("失效译文应该可规划");

        assert_eq!(plan.invalidations().len(), 1);
        let clear = plan.invalidations()[0].clone().into_clear();
        assert_eq!(clear.group_id, "g1");
        assert_eq!(clear.unit_id, "u2");
        assert_eq!(clear.expected_translation, stale);
        let destination = plan
            .tasks()
            .iter()
            .flat_map(|task| task.outputs.values())
            .flatten()
            .find(|destination| {
                destination.key.group_id() == "g1" && destination.key.unit_id() == "u2"
            })
            .expect("失效 Unit 应重新参与模型任务");
        assert!(
            destination.expected_previous.is_none(),
            "清除已在模型请求前提交，新写入必须 CAS 比较空译文槽"
        );
    }
}
