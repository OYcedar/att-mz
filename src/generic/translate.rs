//! Generic 的稳定 TaskBlock 投影、全局去重与字符串响应验收。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::execution::CooperativeCancellation;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::LanguagePair;
use crate::translation::planning_resource::CompiledTerminology;
use crate::translation::task_planning::{
    StableGroupCharacters, TaskId, TaskPlanningError, TaskPlanningGroupLayout, TaskPlanningLayout,
    TaskPlanningScopeLayout, UnitTaskResponsibility, assign_task_ids, pack_complete_task_blocks,
};
use crate::translation_protocol::ParsedTranslationResponse;
#[cfg(test)]
use crate::translation_protocol::{
    TranslationResponseEnvelope, TranslationTaskResponseParseError, parse_translation_response,
};

use super::identity::{
    FingerprintBucketMap, framed_identity_fingerprint_with_cancellation,
    identity_bytes_equal_with_cancellation,
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

/// 以可取消的复合身份查找 Generic Unit。
///
/// 标准 HashMap 只接收固定 SHA-256；同指纹候选仍逐字段分块精确比较，因此碰撞不会被
/// 当成同一 Unit。调用方的 `ensure_running` 同时覆盖指纹和碰撞比较。
pub(crate) struct GenericUnitMap<V> {
    inner: FingerprintBucketMap<GenericUnitKey, V>,
}

impl<V> GenericUnitMap<V> {
    pub(crate) fn new() -> Self {
        Self {
            inner: FingerprintBucketMap::new(),
        }
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: FingerprintBucketMap::with_capacity(capacity),
        }
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub(crate) fn insert_with_cancellation<E>(
        &mut self,
        key: GenericUnitKey,
        value: V,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<V>, E> {
        let fingerprint =
            generic_unit_key_fingerprint_with_cancellation(&key, &mut ensure_running)?;
        self.inner
            .insert_with(fingerprint, key, value, |left, right| {
                generic_unit_keys_equal_with_cancellation(left, right, &mut ensure_running)
            })
    }

    pub(crate) fn get_with_cancellation<E>(
        &self,
        key: &GenericUnitKey,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<&V>, E> {
        let fingerprint = generic_unit_key_fingerprint_with_cancellation(key, &mut ensure_running)?;
        self.inner.get_with(fingerprint, key, |left, right| {
            generic_unit_keys_equal_with_cancellation(left, right, &mut ensure_running)
        })
    }

    pub(crate) fn get_parts_with_cancellation<E>(
        &self,
        group_id: &str,
        unit_id: &str,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<&V>, E> {
        let fingerprint = generic_unit_parts_fingerprint_with_cancellation(
            group_id,
            unit_id,
            &mut ensure_running,
        )?;
        self.inner
            .get_with(fingerprint, &(group_id, unit_id), |stored, requested| {
                generic_unit_key_matches_parts_with_cancellation(
                    stored,
                    requested.0,
                    requested.1,
                    &mut ensure_running,
                )
            })
    }

    pub(crate) fn contains_with_cancellation<E>(
        &self,
        key: &GenericUnitKey,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<bool, E> {
        let fingerprint = generic_unit_key_fingerprint_with_cancellation(key, &mut ensure_running)?;
        self.inner.contains_with(fingerprint, key, |left, right| {
            generic_unit_keys_equal_with_cancellation(left, right, &mut ensure_running)
        })
    }

    pub(crate) fn remove_with_cancellation<E>(
        &mut self,
        key: &GenericUnitKey,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<V>, E> {
        let fingerprint = generic_unit_key_fingerprint_with_cancellation(key, &mut ensure_running)?;
        self.inner.remove_with(fingerprint, key, |left, right| {
            generic_unit_keys_equal_with_cancellation(left, right, &mut ensure_running)
        })
    }
}

fn generic_unit_key_fingerprint_with_cancellation<E>(
    key: &GenericUnitKey,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    generic_unit_parts_fingerprint_with_cancellation(key.group_id(), key.unit_id(), ensure_running)
}

fn generic_unit_parts_fingerprint_with_cancellation<E>(
    group_id: &str,
    unit_id: &str,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    framed_identity_fingerprint_with_cancellation(
        b"att.generic.unit-key-index",
        [(1, group_id.as_bytes()), (2, unit_id.as_bytes())],
        ensure_running,
    )
}

fn generic_unit_keys_equal_with_cancellation<E>(
    left: &GenericUnitKey,
    right: &GenericUnitKey,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    Ok(identity_bytes_equal_with_cancellation(
        left.group_id().as_bytes(),
        right.group_id().as_bytes(),
        &mut ensure_running,
    )? && identity_bytes_equal_with_cancellation(
        left.unit_id().as_bytes(),
        right.unit_id().as_bytes(),
        &mut ensure_running,
    )?)
}

fn generic_unit_key_matches_parts_with_cancellation<E>(
    stored: &GenericUnitKey,
    group_id: &str,
    unit_id: &str,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    Ok(identity_bytes_equal_with_cancellation(
        stored.group_id().as_bytes(),
        group_id.as_bytes(),
        &mut ensure_running,
    )? && identity_bytes_equal_with_cancellation(
        stored.unit_id().as_bytes(),
        unit_id.as_bytes(),
        &mut ensure_running,
    )?)
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
    current_context: Option<CurrentContext>,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
    invalidated_previous: Option<GenericStoredTranslation>,
}

/// Current Unit 在完整 TaskBlock 中提供的安全语境。
///
/// 无法把目标译文按本 Unit 的 Placeholder 绑定安全保护时，源文仍可供模型理解相邻文本，
/// 但它不是目标译文，不能成为全局复用的种子。
#[derive(Clone, Debug, Eq, PartialEq)]
enum CurrentContext {
    SafeTarget(String),
    ProtectedSourceFallback(String),
}

impl CurrentContext {
    fn text(&self) -> &str {
        match self {
            Self::SafeTarget(text) | Self::ProtectedSourceFallback(text) => text,
        }
    }

    fn reuse_candidate(&self) -> Option<&str> {
        match self {
            Self::SafeTarget(text) => Some(text),
            Self::ProtectedSourceFallback(_) => None,
        }
    }
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
        let current_context = current_translation
            .as_ref()
            .map(|text| CurrentContext::SafeTarget(text.clone()));
        Self {
            key,
            protected_text,
            placeholder_binding_fingerprint,
            terminology_indices: Vec::new(),
            needs_translation,
            current_translation,
            current_context,
            expected_state_fingerprint,
            expected_previous: None,
            invalidated_previous: None,
        }
    }

    /// 用持久化记录和本次实际资源计算 Current，调用方不需要解释状态字段。
    #[cfg(test)]
    pub(crate) fn from_stored(
        project: &GenericProject,
        group: &GenericStoredGroup,
        unit: &GenericStoredUnit,
        protected: &GenericProtectedText,
        terminology_indices: Vec<usize>,
        needs_translation: bool,
        resources: AutomaticStateResources,
    ) -> Self {
        Self::from_stored_with_cancellation(
            project,
            group,
            unit,
            protected,
            terminology_indices,
            needs_translation,
            resources,
            &CooperativeCancellation::default(),
        )
        .expect("不取消的受信 PlanningUnit 必须可以建立")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored_with_cancellation(
        project: &GenericProject,
        group: &GenericStoredGroup,
        unit: &GenericStoredUnit,
        protected: &GenericProtectedText,
        terminology_indices: Vec<usize>,
        needs_translation: bool,
        resources: AutomaticStateResources,
        cancellation: &CooperativeCancellation,
    ) -> Result<Self, GenericPlanningError> {
        ensure_translation_not_cancelled(cancellation)?;
        let key = GenericUnitKey::new(
            clone_translation_text(group.id(), cancellation)?,
            clone_translation_text(unit.id(), cancellation)?,
        );
        let placeholder_binding_fingerprint = protected.binding_fingerprint();
        let automatic_state = automatic_translation_state_fingerprint_with_cancellation(
            project.language_pair(),
            &key,
            unit.source_text(),
            group.context_fingerprint(),
            placeholder_binding_fingerprint,
            resources,
            cancellation,
        )?;
        let current_translation = current_translation_for_stored_with_cancellation(
            project,
            group,
            unit,
            placeholder_binding_fingerprint,
            Some(resources),
            cancellation,
        )?;
        let previous = unit
            .translation()
            .map(|translation| clone_stored_translation(translation, cancellation))
            .transpose()?;
        let (expected_previous, invalidated_previous) = if current_translation.is_some() {
            (previous, None)
        } else {
            (None, previous)
        };
        Ok(Self {
            key,
            protected_text: clone_translation_text(protected.text(), cancellation)?,
            placeholder_binding_fingerprint,
            terminology_indices,
            needs_translation,
            current_translation,
            current_context: None,
            expected_state_fingerprint: automatic_state,
            expected_previous,
            invalidated_previous,
        })
    }

    pub(crate) fn key(&self) -> &GenericUnitKey {
        &self.key
    }

    pub(crate) fn needs_candidate(&self) -> bool {
        self.needs_translation && self.current_translation.is_none()
    }

    pub(crate) fn current_translation(&self) -> Option<&str> {
        self.current_translation.as_deref()
    }

    /// 为已经确认 Current 的目标文本安装使用本 Unit Placeholder 绑定的安全表示。
    ///
    /// 状态判断仍使用持久化译文；进入 Task 规划前，调用方必须完成这一步，避免把原始
    /// Placeholder 内容直接发给模型。
    pub(crate) fn install_current_target_context(&mut self, context_text: String) {
        assert!(
            self.current_translation.is_some(),
            "只有 Current Unit 才能安装目标语境文本"
        );
        self.current_context = Some(CurrentContext::SafeTarget(context_text));
    }

    /// 目标译文无法安全投影时，安装只供模型理解相邻语义的保护后源文。
    pub(crate) fn install_current_source_fallback(&mut self, context_text: String) {
        assert!(
            self.current_translation.is_some(),
            "只有 Current Unit 才能安装源文回退语境"
        );
        self.current_context = Some(CurrentContext::ProtectedSourceFallback(context_text));
    }
}

/// 依据持久化来源类型和本次语义资源判断一个已有译文是否仍为 Current。
///
/// 人工状态不依赖自动资源；缺少自动资源时只把自动译文视为无法证明为 Current。
pub(crate) fn current_translation_for_stored_with_cancellation(
    project: &GenericProject,
    group: &GenericStoredGroup,
    unit: &GenericStoredUnit,
    placeholder_binding_fingerprint: Sha256Fingerprint,
    automatic_resources: Option<AutomaticStateResources>,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericPlanningError> {
    ensure_translation_not_cancelled(cancellation)?;
    let Some(translation) = unit.translation() else {
        return Ok(None);
    };
    let key = GenericUnitKey::new(
        clone_translation_text(group.id(), cancellation)?,
        clone_translation_text(unit.id(), cancellation)?,
    );
    let expected = match translation.origin() {
        TranslationOrigin::Automatic => automatic_resources
            .map(|resources| {
                automatic_translation_state_fingerprint_with_cancellation(
                    project.language_pair(),
                    &key,
                    unit.source_text(),
                    group.context_fingerprint(),
                    placeholder_binding_fingerprint,
                    resources,
                    cancellation,
                )
            })
            .transpose()?,
        TranslationOrigin::Manual => Some(manual_translation_state_fingerprint_with_cancellation(
            project.language_pair(),
            &key,
            unit.source_text(),
            group.context_fingerprint(),
            placeholder_binding_fingerprint,
            cancellation,
        )?),
    };
    if expected == Some(translation.state_fingerprint()) {
        Ok(Some(clone_translation_text(
            translation.translation(),
            cancellation,
        )?))
    } else {
        Ok(None)
    }
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

/// 一个目标 Unit 已经完整验收的复用结果。
///
/// `translation` 是提交到项目的最终译文；`context_text` 是使用该目标 Unit 的
/// Placeholder 绑定重新保护后的安全表示。二者必须由同一次验收产生，避免语言修复后
/// 模型看到的语境仍停留在修复前。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedReuse {
    translation: String,
    context_text: String,
}

impl ValidatedReuse {
    pub(crate) fn new(translation: String, context_text: String) -> Self {
        Self {
            translation,
            context_text,
        }
    }

    #[cfg(test)]
    fn same_text(text: String) -> Self {
        Self {
            context_text: text.clone(),
            translation: text,
        }
    }

    fn into_parts(self) -> (String, String) {
        (self.translation, self.context_text)
    }
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
    output_id: Option<TaskId>,
    text: String,
}

impl PlannedContextUnit {
    pub(crate) const fn output_id(&self) -> Option<TaskId> {
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
    units: Vec<PlannedContextUnit>,
}

impl PlannedGroup {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn units(&self) -> &[PlannedContextUnit] {
        &self.units
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
    terminology_indices: Vec<usize>,
    outputs: BTreeMap<TaskId, Vec<PlannedDestination>>,
}

impl PlannedTask {
    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn groups(&self) -> &[PlannedGroup] {
        &self.groups
    }

    pub(crate) fn terminology_indices(&self) -> &[usize] {
        &self.terminology_indices
    }

    pub(crate) fn expected_output_ids(&self) -> impl Iterator<Item = TaskId> + '_ {
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

/// Generic 稳定装箱、状态规划或 Extract 快照投影无法建立完整结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GenericPlanningError {
    Cancelled,
    TaskPlanning(TaskPlanningError),
    MissingCurrentContext(GenericUnitKey),
    Missing(GenericUnitKey),
    Unknown(GenericUnitKey),
    Duplicate(GenericUnitKey),
}

impl fmt::Display for GenericPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 翻译规划已取消"),
            Self::TaskPlanning(source) => source.fmt(formatter),
            Self::MissingCurrentContext(key) => write!(
                formatter,
                "Current Generic Unit 缺少安全目标语境：{}/{}",
                key.group_id, key.unit_id
            ),
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

impl Error for GenericPlanningError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskPlanning(source) => Some(source),
            Self::Cancelled
            | Self::MissingCurrentContext(_)
            | Self::Missing(_)
            | Self::Unknown(_)
            | Self::Duplicate(_) => None,
        }
    }
}

impl GenericPlanningError {
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::TaskPlanning(source) if source.is_cancelled())
    }
}

impl From<TaskPlanningError> for GenericPlanningError {
    fn from(source: TaskPlanningError) -> Self {
        Self::TaskPlanning(source)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DeduplicationKey {
    source_text: String,
    protected_text: String,
    placeholder_binding_fingerprint: Sha256Fingerprint,
}

fn deduplication_key_fingerprint_with_cancellation<E>(
    key: &DeduplicationKey,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    framed_identity_fingerprint_with_cancellation(
        b"att.generic.deduplication-key-index",
        [
            (1, key.source_text.as_bytes()),
            (2, key.protected_text.as_bytes()),
            (3, key.placeholder_binding_fingerprint.as_bytes()),
        ],
        ensure_running,
    )
}

fn deduplication_keys_equal_with_cancellation(
    left: &DeduplicationKey,
    right: &DeduplicationKey,
    is_cancelled: &impl Fn() -> bool,
) -> Result<bool, GenericPlanningError> {
    Ok(
        left.placeholder_binding_fingerprint == right.placeholder_binding_fingerprint
            && planning_text_equal_with_cancellation(
                &left.source_text,
                &right.source_text,
                is_cancelled,
            )?
            && planning_text_equal_with_cancellation(
                &left.protected_text,
                &right.protected_text,
                is_cancelled,
            )?,
    )
}

struct SnapshotUnitFacts<'a> {
    key: GenericUnitKey,
    source_text: &'a str,
    group_context: Sha256Fingerprint,
}

struct UnitFacts<'input, 'snapshot> {
    key: GenericUnitKey,
    input: &'input PlanningUnit,
    source_text: &'snapshot str,
    group_context: Sha256Fingerprint,
}

fn resolve_planning_inputs<'input>(
    natural_units: &[SnapshotUnitFacts<'_>],
    planning_units: &'input [PlanningUnit],
    is_cancelled: &impl Fn() -> bool,
) -> Result<Vec<&'input PlanningUnit>, GenericPlanningError> {
    let mut naturally_ordered = natural_units.len() == planning_units.len();
    if naturally_ordered {
        for (natural, supplied) in natural_units.iter().zip(planning_units) {
            ensure_planning_not_cancelled(is_cancelled)?;
            if !generic_unit_keys_equal_with_cancellation(&natural.key, &supplied.key, || {
                ensure_planning_not_cancelled(is_cancelled)
            })? {
                naturally_ordered = false;
                break;
            }
        }
    }
    if naturally_ordered {
        ensure_planning_not_cancelled(is_cancelled)?;
        return Ok(planning_units.iter().collect());
    }

    // 非生产顺序仍按完整身份严格验收，以保留 Duplicate、Missing 和 Unknown 的
    // 明确诊断。生产路径由 Extract 自然顺序直接建立，不承担这些重复哈希成本。
    let mut supplied = GenericUnitMap::with_capacity(planning_units.len());
    for unit in planning_units {
        ensure_planning_not_cancelled(is_cancelled)?;
        let key = clone_planning_key_with_cancellation(&unit.key, is_cancelled)?;
        if supplied
            .insert_with_cancellation(key, unit, || ensure_planning_not_cancelled(is_cancelled))?
            .is_some()
        {
            return Err(GenericPlanningError::Duplicate(
                clone_planning_key_with_cancellation(&unit.key, is_cancelled)?,
            ));
        }
    }

    let mut known = GenericUnitMap::with_capacity(natural_units.len());
    let mut resolved = Vec::with_capacity(natural_units.len());
    for natural in natural_units {
        ensure_planning_not_cancelled(is_cancelled)?;
        let Some(input) = supplied
            .get_with_cancellation(&natural.key, || ensure_planning_not_cancelled(is_cancelled))?
        else {
            return Err(GenericPlanningError::Missing(
                clone_planning_key_with_cancellation(&natural.key, is_cancelled)?,
            ));
        };
        resolved.push(*input);
        let previous = known.insert_with_cancellation(
            clone_planning_key_with_cancellation(&natural.key, is_cancelled)?,
            (),
            || ensure_planning_not_cancelled(is_cancelled),
        )?;
        debug_assert!(previous.is_none());
    }
    for unit in planning_units {
        ensure_planning_not_cancelled(is_cancelled)?;
        if !known
            .contains_with_cancellation(&unit.key, || ensure_planning_not_cancelled(is_cancelled))?
        {
            return Err(GenericPlanningError::Unknown(
                clone_planning_key_with_cancellation(&unit.key, is_cancelled)?,
            ));
        }
    }
    Ok(resolved)
}

struct Family {
    members: Vec<usize>,
}

/// 按完整 JSONL 层次稳定装箱，再按自然顺序计算全项目去重和块内临时 ID。
#[cfg(test)]
pub(crate) fn plan_translation(
    snapshot: &GenericStoredSnapshot,
    planning_units: &[PlanningUnit],
    reuse_validator: impl Fn(&GenericUnitKey, &str) -> Result<String, String>,
) -> Result<TranslationPlan, GenericPlanningError> {
    plan_translation_with_cancellation(
        snapshot,
        planning_units,
        NonZeroUsize::MAX,
        reuse_validator,
        &CooperativeCancellation::default(),
    )
}

/// 与 [`plan_translation`] 相同，但在全项目规划的自然边界响应调用方取消。
#[cfg(test)]
pub(crate) fn plan_translation_with_cancellation(
    snapshot: &GenericStoredSnapshot,
    planning_units: &[PlanningUnit],
    target_task_characters: NonZeroUsize,
    reuse_validator: impl Fn(&GenericUnitKey, &str) -> Result<String, String>,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationPlan, GenericPlanningError> {
    plan_translation_with_validator_and_cancellation(
        snapshot,
        planning_units,
        target_task_characters,
        |key, candidate| Ok(reuse_validator(key, candidate).map(ValidatedReuse::same_text)),
        cancellation,
    )
}

/// 与 [`plan_translation_with_cancellation`] 相同，并允许复用验收自身传播取消。
pub(crate) fn plan_translation_with_validator_and_cancellation(
    snapshot: &GenericStoredSnapshot,
    planning_units: &[PlanningUnit],
    target_task_characters: NonZeroUsize,
    reuse_validator: impl Fn(
        &GenericUnitKey,
        &str,
    ) -> Result<Result<ValidatedReuse, String>, GenericPlanningError>,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationPlan, GenericPlanningError> {
    let is_cancelled = || cancellation.is_requested();
    ensure_planning_not_cancelled(&is_cancelled)?;
    let (task_layout, scope_file_indices) = generic_task_planning_layout(snapshot, &is_cancelled)?;
    let complete_task_plan =
        pack_complete_task_blocks(&task_layout, target_task_characters, cancellation)?;
    let mut natural_units = Vec::with_capacity(task_layout.total_units());
    for file in snapshot.files() {
        ensure_planning_not_cancelled(&is_cancelled)?;
        for group in file.groups() {
            ensure_planning_not_cancelled(&is_cancelled)?;
            for unit in group.units() {
                ensure_planning_not_cancelled(&is_cancelled)?;
                let key = GenericUnitKey::new(
                    clone_planning_text_with_cancellation(group.id(), &is_cancelled)?,
                    clone_planning_text_with_cancellation(unit.id(), &is_cancelled)?,
                );
                natural_units.push(SnapshotUnitFacts {
                    key,
                    source_text: unit.source_text(),
                    group_context: group.context_fingerprint(),
                });
            }
        }
    }
    let resolved_inputs = resolve_planning_inputs(&natural_units, planning_units, &is_cancelled)?;
    let facts = natural_units
        .into_iter()
        .zip(resolved_inputs)
        .map(|(natural, input)| UnitFacts {
            key: natural.key,
            input,
            source_text: natural.source_text,
            group_context: natural.group_context,
        })
        .collect::<Vec<_>>();

    let mut invalidations = Vec::new();
    for fact in &facts {
        ensure_planning_not_cancelled(&is_cancelled)?;
        if let Some(expected_translation) = fact.input.invalidated_previous.as_ref() {
            invalidations.push(PlannedInvalidation {
                key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                expected_source_text: clone_planning_text_with_cancellation(
                    fact.source_text,
                    &is_cancelled,
                )?,
                expected_group_context: fact.group_context,
                expected_translation: clone_planning_stored_translation_with_cancellation(
                    expected_translation,
                    &is_cancelled,
                )?,
            });
        }
    }

    let mut family_indices = FingerprintBucketMap::<DeduplicationKey, usize>::new();
    let mut families: Vec<Family> = Vec::new();
    for (unit_index, fact) in facts.iter().enumerate() {
        ensure_planning_not_cancelled(&is_cancelled)?;
        let deduplication_key = DeduplicationKey {
            source_text: clone_planning_text_with_cancellation(fact.source_text, &is_cancelled)?,
            protected_text: clone_planning_text_with_cancellation(
                &fact.input.protected_text,
                &is_cancelled,
            )?,
            placeholder_binding_fingerprint: fact.input.placeholder_binding_fingerprint,
        };
        let fingerprint =
            deduplication_key_fingerprint_with_cancellation(&deduplication_key, || {
                ensure_planning_not_cancelled(&is_cancelled)
            })?;
        let family_index =
            match family_indices.get_with(fingerprint, &deduplication_key, |left, right| {
                deduplication_keys_equal_with_cancellation(left, right, &is_cancelled)
            })? {
                Some(index) => *index,
                None => {
                    let family_index = families.len();
                    let previous = family_indices.insert_with(
                        fingerprint,
                        deduplication_key,
                        family_index,
                        |left, right| {
                            deduplication_keys_equal_with_cancellation(left, right, &is_cancelled)
                        },
                    )?;
                    debug_assert!(previous.is_none());
                    families.push(Family {
                        members: Vec::new(),
                    });
                    family_index
                }
            };
        families[family_index].members.push(unit_index);
    }

    let mut reused = Vec::new();
    let mut representative_destinations = GenericUnitMap::new();
    let mut known_targets = GenericUnitMap::new();
    let mut responsibilities = vec![UnitTaskResponsibility::Context; facts.len()];
    for family in &families {
        ensure_planning_not_cancelled(&is_cancelled)?;
        let mut first_current = None::<&str>;
        let mut first_reuse_candidate = None::<&str>;
        let mut multiple_currents = false;
        for unit_index in &family.members {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let fact = &facts[*unit_index];
            let Some(current) = fact.input.current_translation.as_deref() else {
                continue;
            };
            let Some(context) = fact.input.current_context.as_ref() else {
                return Err(GenericPlanningError::MissingCurrentContext(
                    clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                ));
            };
            if first_reuse_candidate.is_none() {
                first_reuse_candidate = context.reuse_candidate();
            }
            match first_current {
                None => {
                    first_current = Some(current);
                }
                Some(first) => {
                    if !planning_text_equal_with_cancellation(first, current, &is_cancelled)? {
                        multiple_currents = true;
                        break;
                    }
                }
            }
        }
        for unit_index in &family.members {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let fact = &facts[*unit_index];
            if fact.input.current_translation.is_some() {
                let Some(context) = fact.input.current_context.as_ref() else {
                    return Err(GenericPlanningError::MissingCurrentContext(
                        clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                    ));
                };
                let previous = known_targets.insert_with_cancellation(
                    clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                    clone_planning_text_with_cancellation(context.text(), &is_cancelled)?,
                    || ensure_planning_not_cancelled(&is_cancelled),
                )?;
                debug_assert!(previous.is_none());
            }
        }

        let mut unresolved = Vec::new();
        for unit_index in &family.members {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let fact = &facts[*unit_index];
            if fact.input.needs_translation && fact.input.current_translation.is_none() {
                unresolved.push(*unit_index);
            }
        }
        if unresolved.is_empty() {
            continue;
        }

        if !multiple_currents {
            let Some(translation) = first_reuse_candidate else {
                let representative_index = unresolved[0];
                let representative = clone_planning_key_with_cancellation(
                    &facts[representative_index].key,
                    &is_cancelled,
                )?;
                let mut destinations = Vec::with_capacity(unresolved.len());
                for unit_index in &unresolved {
                    ensure_planning_not_cancelled(&is_cancelled)?;
                    let fact = &facts[*unit_index];
                    destinations.push(PlannedDestination {
                        key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                        expected_source_text: clone_planning_text_with_cancellation(
                            fact.source_text,
                            &is_cancelled,
                        )?,
                        expected_group_context: fact.group_context,
                        expected_state_fingerprint: fact.input.expected_state_fingerprint,
                        expected_previous: fact
                            .input
                            .expected_previous
                            .as_ref()
                            .map(|previous| {
                                clone_planning_stored_translation_with_cancellation(
                                    previous,
                                    &is_cancelled,
                                )
                            })
                            .transpose()?,
                    });
                }
                let previous = representative_destinations.insert_with_cancellation(
                    representative,
                    destinations,
                    || ensure_planning_not_cancelled(&is_cancelled),
                )?;
                debug_assert!(previous.is_none());
                responsibilities[representative_index] =
                    UnitTaskResponsibility::ModelRepresentative;
                continue;
            };
            let translation = clone_planning_text_with_cancellation(translation, &is_cancelled)?;
            let mut model_destinations = Vec::new();
            let mut model_representative_index = None;
            for unit_index in unresolved {
                ensure_planning_not_cancelled(&is_cancelled)?;
                let fact = &facts[unit_index];
                let destination = PlannedDestination {
                    key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                    expected_source_text: clone_planning_text_with_cancellation(
                        fact.source_text,
                        &is_cancelled,
                    )?,
                    expected_group_context: fact.group_context,
                    expected_state_fingerprint: fact.input.expected_state_fingerprint,
                    expected_previous: fact
                        .input
                        .expected_previous
                        .as_ref()
                        .map(|previous| {
                            clone_planning_stored_translation_with_cancellation(
                                previous,
                                &is_cancelled,
                            )
                        })
                        .transpose()?,
                };
                let validated = reuse_validator(&fact.key, &translation)?;
                ensure_planning_not_cancelled(&is_cancelled)?;
                match validated {
                    Ok(validated) => {
                        let (validated_translation, context_text) = validated.into_parts();
                        reused.push(PlannedReuse {
                            key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                            translation: clone_planning_text_with_cancellation(
                                &validated_translation,
                                &is_cancelled,
                            )?,
                            expected_source_text: destination.expected_source_text,
                            expected_group_context: destination.expected_group_context,
                            expected_state_fingerprint: destination.expected_state_fingerprint,
                            expected_previous: destination.expected_previous,
                        });
                        let previous = known_targets.insert_with_cancellation(
                            clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                            clone_planning_text_with_cancellation(&context_text, &is_cancelled)?,
                            || ensure_planning_not_cancelled(&is_cancelled),
                        )?;
                        debug_assert!(previous.is_none());
                    }
                    Err(_) => {
                        model_representative_index.get_or_insert(unit_index);
                        model_destinations.push(destination);
                    }
                }
            }
            if let Some(representative_index) = model_representative_index {
                let representative = clone_planning_key_with_cancellation(
                    &facts[representative_index].key,
                    &is_cancelled,
                )?;
                let previous = representative_destinations.insert_with_cancellation(
                    representative,
                    model_destinations,
                    || ensure_planning_not_cancelled(&is_cancelled),
                )?;
                debug_assert!(previous.is_none());
                responsibilities[representative_index] =
                    UnitTaskResponsibility::ModelRepresentative;
            }
            continue;
        }

        let representative_index = unresolved[0];
        let representative =
            clone_planning_key_with_cancellation(&facts[representative_index].key, &is_cancelled)?;
        let mut destinations = Vec::with_capacity(unresolved.len());
        for unit_index in &unresolved {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let fact = &facts[*unit_index];
            destinations.push(PlannedDestination {
                key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                expected_source_text: clone_planning_text_with_cancellation(
                    fact.source_text,
                    &is_cancelled,
                )?,
                expected_group_context: fact.group_context,
                expected_state_fingerprint: fact.input.expected_state_fingerprint,
                expected_previous: fact
                    .input
                    .expected_previous
                    .as_ref()
                    .map(|previous| {
                        clone_planning_stored_translation_with_cancellation(previous, &is_cancelled)
                    })
                    .transpose()?,
            });
        }
        let previous = representative_destinations.insert_with_cancellation(
            representative,
            destinations,
            || ensure_planning_not_cancelled(&is_cancelled),
        )?;
        debug_assert!(previous.is_none());
        responsibilities[representative_index] = UnitTaskResponsibility::ModelRepresentative;
    }

    debug_assert_eq!(responsibilities.len(), task_layout.total_units());
    let assigned_task_plan = assign_task_ids(complete_task_plan, &responsibilities, cancellation)?;

    let mut tasks = Vec::new();
    for block in assigned_task_plan.blocks_with_task_ids() {
        ensure_planning_not_cancelled(&is_cancelled)?;
        let layout = block.layout();
        let unit_range = layout.unit_range();
        debug_assert_eq!(unit_range.len(), block.unit_task_ids().len());
        let file = &snapshot.files()[scope_file_indices[layout.scope_index()]];
        let mut groups = Vec::with_capacity(layout.group_range().len());
        let mut terminology_indices = Vec::new();
        let mut outputs = BTreeMap::new();
        let mut block_unit_index = 0_usize;
        for group in &file.groups()[layout.group_range()] {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let mut units = Vec::with_capacity(group.units().len());
            for _unit in group.units() {
                ensure_planning_not_cancelled(&is_cancelled)?;
                let fact = &facts[unit_range.start + block_unit_index];
                let key = &fact.key;
                terminology_indices.extend(clone_planning_indices_with_cancellation(
                    &fact.input.terminology_indices,
                    &is_cancelled,
                )?);
                let text = match known_targets
                    .get_with_cancellation(key, || ensure_planning_not_cancelled(&is_cancelled))?
                {
                    Some(text) => clone_planning_text_with_cancellation(text, &is_cancelled)?,
                    None => clone_planning_text_with_cancellation(
                        &fact.input.protected_text,
                        &is_cancelled,
                    )?,
                };
                let output_id = block.unit_task_ids()[block_unit_index];
                block_unit_index += 1;
                if let Some(output_id) = output_id {
                    let destinations = representative_destinations
                        .remove_with_cancellation(key, || {
                            ensure_planning_not_cancelled(&is_cancelled)
                        })?
                        .expect("模型代表必须保留对应的传播目标");
                    let previous = outputs.insert(output_id, destinations);
                    debug_assert!(previous.is_none());
                    units.push(PlannedContextUnit {
                        output_id: Some(output_id),
                        text,
                    });
                } else {
                    units.push(PlannedContextUnit {
                        output_id: None,
                        text,
                    });
                }
            }
            groups.push(PlannedGroup {
                kind: clone_planning_text_with_cancellation(group.kind(), &is_cancelled)?,
                units,
            });
        }
        debug_assert_eq!(block_unit_index, block.unit_task_ids().len());
        terminology_indices.sort_unstable();
        terminology_indices.dedup();

        tasks.push(PlannedTask {
            relative_path: file.relative_path().to_path_buf(),
            groups,
            terminology_indices,
            outputs,
        });
    }
    ensure_planning_not_cancelled(&is_cancelled)?;
    debug_assert!(representative_destinations.is_empty());

    Ok(TranslationPlan {
        invalidations,
        reused,
        tasks,
    })
}

fn generic_task_planning_layout(
    snapshot: &GenericStoredSnapshot,
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> Result<(TaskPlanningLayout, Vec<usize>), GenericPlanningError> {
    let projected = snapshot
        .files()
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            ensure_planning_not_cancelled(is_cancelled)?;
            if file.groups().is_empty() {
                return Ok(None);
            }
            let mut groups = Vec::with_capacity(file.groups().len());
            for group in file.groups() {
                ensure_planning_not_cancelled(is_cancelled)?;
                groups.push(TaskPlanningGroupLayout::new(
                    group.units().len(),
                    stable_generic_group_characters(group, is_cancelled)?,
                )?);
            }
            Ok(Some((file_index, TaskPlanningScopeLayout::new(groups)?)))
        })
        .collect::<Vec<Result<_, GenericPlanningError>>>();

    let mut scopes = Vec::new();
    let mut scope_file_indices = Vec::new();
    for file in projected {
        ensure_planning_not_cancelled(is_cancelled)?;
        if let Some((file_index, scope)) = file? {
            scope_file_indices.push(file_index);
            scopes.push(scope);
        }
    }
    Ok((TaskPlanningLayout::new(scopes)?, scope_file_indices))
}

fn stable_generic_group_characters(
    group: &GenericStoredGroup,
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> Result<StableGroupCharacters, GenericPlanningError> {
    let mut following = "kind=".len();
    following = checked_stable_character_add(
        following,
        stable_json_string_characters(group.kind(), is_cancelled)?,
    )?;
    following = checked_stable_character_add(following, "\nunits:\n".len())?;
    for unit in group.units() {
        ensure_planning_not_cancelled(is_cancelled)?;
        following = checked_stable_character_add(following, "[-] ".len())?;
        following = checked_stable_character_add(
            following,
            stable_json_string_characters(unit.source_text(), is_cancelled)?,
        )?;
        following = checked_stable_character_add(following, 1)?;
    }
    following = checked_stable_character_add(following, 1)?;
    let first = checked_stable_character_add("Groups:\n".len(), following)?;
    Ok(StableGroupCharacters::new(first, following))
}

fn stable_json_string_characters(
    text: &str,
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> Result<usize, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut characters = 2_usize;
    for chunk in text.as_bytes().chunks(CANCELLATION_CHECK_BYTES) {
        ensure_planning_not_cancelled(is_cancelled)?;
        let mut chunk_characters = 0_usize;
        for byte in chunk {
            chunk_characters += match byte {
                b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
                0x00..=0x1f => 6,
                0x80..=0xbf => 0,
                _ => 1,
            };
        }
        characters = checked_stable_character_add(characters, chunk_characters)?;
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(characters)
}

fn checked_stable_character_add(left: usize, right: usize) -> Result<usize, GenericPlanningError> {
    left.checked_add(right)
        .ok_or(TaskPlanningError::CharacterCountOverflow.into())
}

fn ensure_planning_not_cancelled(
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), GenericPlanningError> {
    if is_cancelled() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

fn clone_planning_text_with_cancellation(
    text: &str,
    is_cancelled: &impl Fn() -> bool,
) -> Result<String, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut output = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_planning_not_cancelled(is_cancelled)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(output)
}

fn planning_text_equal_with_cancellation(
    left: &str,
    right: &str,
    is_cancelled: &impl Fn() -> bool,
) -> Result<bool, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_planning_not_cancelled(is_cancelled)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CANCELLATION_CHECK_BYTES)
        .zip(right.as_bytes().chunks(CANCELLATION_CHECK_BYTES))
    {
        ensure_planning_not_cancelled(is_cancelled)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(true)
}

fn clone_planning_key_with_cancellation(
    key: &GenericUnitKey,
    is_cancelled: &impl Fn() -> bool,
) -> Result<GenericUnitKey, GenericPlanningError> {
    Ok(GenericUnitKey::new(
        clone_planning_text_with_cancellation(key.group_id(), is_cancelled)?,
        clone_planning_text_with_cancellation(key.unit_id(), is_cancelled)?,
    ))
}

fn clone_planning_indices_with_cancellation(
    indices: &[usize],
    is_cancelled: &impl Fn() -> bool,
) -> Result<Vec<usize>, GenericPlanningError> {
    const CANCELLATION_CHECK_ITEMS: usize = 1024;

    let mut output = Vec::with_capacity(indices.len());
    for chunk in indices.chunks(CANCELLATION_CHECK_ITEMS) {
        ensure_planning_not_cancelled(is_cancelled)?;
        output.extend_from_slice(chunk);
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(output)
}

fn clone_planning_stored_translation_with_cancellation(
    translation: &GenericStoredTranslation,
    is_cancelled: &impl Fn() -> bool,
) -> Result<GenericStoredTranslation, GenericPlanningError> {
    Ok(GenericStoredTranslation {
        translation: clone_planning_text_with_cancellation(
            translation.translation(),
            is_cancelled,
        )?,
        origin: translation.origin(),
        state_fingerprint: translation.state_fingerprint(),
    })
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
    UnexpectedId(TaskId),
    DuplicateId(TaskId),
    MissingId(TaskId),
    NonStringValue(TaskId),
    InvalidTranslation {
        output_id: TaskId,
        detail: String,
    },
    InvalidDestination {
        output_id: TaskId,
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
    validator: impl FnMut(TaskId, &GenericUnitKey, &str) -> Result<String, String>,
) -> Result<TranslationAcceptance, GenericResponseError> {
    let parsed = parse_translation_response(assistant_response, response_envelope)
        .map_err(|source| GenericResponseError { source })?;
    Ok(accept_parsed_response(task.clone(), &parsed, validator))
}

/// 验收已经由公共协议解析器建立的 Generic 响应投影。
///
/// 记录任务时，调用方可以让解析投影同时进入旁路文档，避免再次解析模型正文。
#[cfg(test)]
pub(crate) fn accept_parsed_response(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    mut validator: impl FnMut(TaskId, &GenericUnitKey, &str) -> Result<String, String>,
) -> TranslationAcceptance {
    accept_parsed_response_with_cancellation(
        task,
        parsed,
        |output_id, key, candidate| Ok(validator(output_id, key, candidate)),
        || false,
    )
    .expect("不取消的受信响应验收必须完成")
}

pub(crate) fn accept_parsed_response_with_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    mut validator: impl FnMut(
        TaskId,
        &GenericUnitKey,
        &str,
    ) -> Result<Result<String, String>, GenericPlanningError>,
    is_cancelled: impl Fn() -> bool,
) -> Result<TranslationAcceptance, GenericPlanningError> {
    ensure_planning_not_cancelled(&is_cancelled)?;
    let entries = parsed.entries();
    let mut canonical_counts = HashMap::new();
    for entry in entries {
        ensure_planning_not_cancelled(&is_cancelled)?;
        if let Some(output_id) = entry.canonical_id() {
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
        ensure_planning_not_cancelled(&is_cancelled)?;
        let Some(output_id) = entry.canonical_id() else {
            problems.push(ResponseProblem::InvalidId(
                clone_planning_text_with_cancellation(entry.id(), &is_cancelled)?,
            ));
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
        let candidate = match entry.decode_value_with_cancellation::<String, _>(|| {
            ensure_planning_not_cancelled(&is_cancelled)
        })? {
            Ok(candidate) => candidate,
            Err(_) => {
                problems.push(ResponseProblem::NonStringValue(output_id));
                continue;
            }
        };
        if let Err(detail) = validate_candidate_text_with_cancellation(&candidate, &is_cancelled)? {
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
            ensure_planning_not_cancelled(&is_cancelled)?;
            let candidate = match validator(output_id, &destination.key, &candidate)? {
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
            ensure_planning_not_cancelled(&is_cancelled)?;
            if let Err(detail) =
                validate_candidate_text_with_cancellation(&candidate, &is_cancelled)?
            {
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
        ensure_planning_not_cancelled(&is_cancelled)?;
        if !observed.contains(output_id) {
            problems.push(ResponseProblem::MissingId(*output_id));
        }
    }
    ensure_planning_not_cancelled(&is_cancelled)?;
    Ok(TranslationAcceptance {
        accepted,
        problems,
        accepted_output_count,
    })
}

fn validate_candidate_text_with_cancellation(
    candidate: &str,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Result<(), &'static str>, GenericPlanningError> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    let mut has_non_whitespace = false;
    for (index, character) in candidate.chars().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_planning_not_cancelled(is_cancelled)?;
        }
        if character == '\r' {
            return Ok(Err("译文不能包含 CR（U+000D）"));
        }
        if character == '\0' {
            return Ok(Err("译文不能包含 NUL（U+0000）"));
        }
        has_non_whitespace |= !character.is_whitespace();
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    if has_non_whitespace {
        Ok(Ok(()))
    } else {
        Ok(Err("译文不能为空白"))
    }
}

/// 建立自动译文 Current 所需的完整语义状态。
#[cfg(test)]
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

fn automatic_translation_state_fingerprint_with_cancellation(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
    resources: AutomaticStateResources,
    cancellation: &CooperativeCancellation,
) -> Result<Sha256Fingerprint, GenericPlanningError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.translation-state.automatic");
    frame_unit_semantics_with_cancellation(
        &mut hasher,
        language_pair,
        key,
        source_text,
        group_context,
        placeholder_binding,
        cancellation,
    )?;
    hasher
        .frame(20, resources.prompt.as_bytes())
        .frame(21, resources.client_semantics.as_bytes())
        .frame(22, resources.language_module.as_bytes())
        .frame(23, resources.terminology_hits.as_bytes());
    ensure_translation_not_cancelled(cancellation)?;
    Ok(hasher.finish())
}

/// 建立人工译文状态；Prompt、Profile、Client 和术语变化不会使它失效。
#[cfg(test)]
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

pub(crate) fn manual_translation_state_fingerprint_with_cancellation(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
    cancellation: &CooperativeCancellation,
) -> Result<Sha256Fingerprint, GenericPlanningError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.translation-state.manual");
    frame_unit_semantics_with_cancellation(
        &mut hasher,
        language_pair,
        key,
        source_text,
        group_context,
        placeholder_binding,
        cancellation,
    )?;
    ensure_translation_not_cancelled(cancellation)?;
    Ok(hasher.finish())
}

/// 建立一个 Group 实际命中术语的稳定语义身份。
///
/// 调用方负责按自然顺序传入 `CompiledTerminology` 返回的命中索引。本函数由生产规划与
/// Project Lua 最终校验共同使用，确保两条路径不会各自定义状态哈希。
pub(crate) fn terminology_hit_fingerprint_with_cancellation<E>(
    terminology: &CompiledTerminology,
    indices: &[usize],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let chunk_size = NonZeroUsize::new(CANCELLATION_CHECK_BYTES).expect("取消检查块大小必须非零");

    ensure_running()?;
    let mut hasher = Sha256FramedHasher::new(b"att.generic.terminology-hits");
    for index in indices {
        ensure_running()?;
        let entry = &terminology.entries()[*index];
        hasher
            .try_frame_chunks(1, entry.term().as_bytes(), chunk_size, &mut ensure_running)?
            .try_frame_chunks(
                2,
                entry.translation().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

#[cfg(test)]
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

#[allow(clippy::too_many_arguments)]
fn frame_unit_semantics_with_cancellation(
    hasher: &mut Sha256FramedHasher,
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let chunk_size = NonZeroUsize::new(CANCELLATION_CHECK_BYTES).expect("取消检查块大小必须非零");

    for (tag, bytes) in [
        (1, language_pair.source().as_str().as_bytes()),
        (2, language_pair.target().as_str().as_bytes()),
        (3, key.group_id().as_bytes()),
        (4, key.unit_id().as_bytes()),
        (5, source_text.as_bytes()),
        (6, group_context.as_bytes()),
        (7, placeholder_binding.as_bytes()),
    ] {
        hasher.try_frame_chunks(tag, bytes, chunk_size, || {
            ensure_translation_not_cancelled(cancellation)
        })?;
    }
    ensure_translation_not_cancelled(cancellation)
}

fn ensure_translation_not_cancelled(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    if cancellation.is_requested() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

fn clone_translation_text(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut output = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_translation_not_cancelled(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_translation_not_cancelled(cancellation)?;
    Ok(output)
}

fn clone_stored_translation(
    translation: &GenericStoredTranslation,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredTranslation, GenericPlanningError> {
    Ok(GenericStoredTranslation {
        translation: clone_translation_text(translation.translation(), cancellation)?,
        origin: translation.origin(),
        state_fingerprint: translation.state_fingerprint(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
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

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value).expect("测试 Task ID 必须非零")
    }

    #[test]
    fn stable_json_character_count_matches_the_actual_generic_string_format() {
        for text in [
            "plain",
            "引号\"与反斜线\\",
            "换行\n制表\t退格\u{08}",
            "控制\u{01}字符",
        ] {
            assert_eq!(
                stable_json_string_characters(text, &|| false).expect("计数应成功"),
                serde_json::to_string(text)
                    .expect("字符串应可编码")
                    .chars()
                    .count()
            );
        }
    }

    #[test]
    fn planning_helpers_poll_long_text_and_index_copies() {
        fn cancels_on_third_poll(polls: &Cell<usize>) -> bool {
            let next = polls.get() + 1;
            polls.set(next);
            next >= 3
        }

        let text = "界".repeat(128 * 1024);
        let clone_polls = Cell::new(0);
        assert!(matches!(
            clone_planning_text_with_cancellation(&text, &|| {
                cancels_on_third_poll(&clone_polls)
            }),
            Err(GenericPlanningError::Cancelled)
        ));
        assert_eq!(clone_polls.get(), 3);

        let equality_polls = Cell::new(0);
        assert!(matches!(
            planning_text_equal_with_cancellation(&text, &text, &|| {
                cancels_on_third_poll(&equality_polls)
            }),
            Err(GenericPlanningError::Cancelled)
        ));
        assert_eq!(equality_polls.get(), 3);

        let indices = vec![0_usize; 4096];
        let index_polls = Cell::new(0);
        assert!(matches!(
            clone_planning_indices_with_cancellation(&indices, &|| {
                cancels_on_third_poll(&index_polls)
            }),
            Err(GenericPlanningError::Cancelled)
        ));
        assert_eq!(index_polls.get(), 3);
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
    fn stable_task_packing_uses_complete_file_groups_and_restarts_ids() {
        let snapshot = task_split_snapshot(&[10]);
        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning(&snapshot),
            NonZeroUsize::new(58).expect("常量应非零"),
            |_, candidate| Ok(candidate.to_owned()),
            &CooperativeCancellation::default(),
        )
        .expect("规划应成功");
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
            "第五个和第六个 Group 必须按完整原文稳定装入同一 TaskBlock"
        );
        for task in plan.tasks() {
            assert_eq!(
                task.expected_output_ids()
                    .map(TaskId::get)
                    .collect::<Vec<_>>(),
                [1, 2],
                "每个最终 Task 都应从 1 重新编号"
            );
        }
    }

    #[test]
    fn stable_task_packing_never_combines_different_files() {
        let snapshot = task_split_snapshot(&[3, 3]);
        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning(&snapshot),
            NonZeroUsize::new(58).expect("常量应非零"),
            |_, candidate| Ok(candidate.to_owned()),
            &CooperativeCancellation::default(),
        )
        .expect("规划应成功");
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
    fn stable_task_packing_keeps_an_oversized_group_alone() {
        let mut snapshot = task_split_snapshot(&[3]);
        snapshot.files[0].groups[1].units[0].source_text = "b".repeat(30);
        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning(&snapshot),
            NonZeroUsize::new(58).expect("常量应非零"),
            |_, candidate| Ok(candidate.to_owned()),
            &CooperativeCancellation::default(),
        )
        .expect("规划应成功");

        assert_eq!(
            plan.tasks()
                .iter()
                .map(|task| task.groups().len())
                .collect::<Vec<_>>(),
            [1, 1, 1]
        );
        assert_eq!(
            plan.tasks()[1].groups()[0].units()[0].text(),
            format!("<{}>", "b".repeat(30))
        );
    }

    #[test]
    fn translation_state_only_changes_ids_and_zero_id_block_filtering() {
        let snapshot = task_split_snapshot(&[6]);
        let planning_for = |model_groups: &[usize]| {
            snapshot.files()[0]
                .groups()
                .iter()
                .enumerate()
                .map(|(group_index, group)| {
                    let unit = &group.units()[0];
                    let model_representative = model_groups.contains(&group_index);
                    PlanningUnit::new(
                        GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned()),
                        format!("<{}>", unit.source_text()),
                        fingerprint(u8::try_from(group_index + 1).expect("测试索引应可表示")),
                        model_representative,
                        (!model_representative).then(|| format!("target-{group_index}")),
                        fingerprint(7),
                    )
                })
                .collect::<Vec<_>>()
        };
        let target = NonZeroUsize::new(58).expect("常量应非零");

        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning_for(&[0, 4]),
            target,
            |_, candidate| Ok(candidate.to_owned()),
            &CooperativeCancellation::default(),
        )
        .expect("规划应成功");
        assert_eq!(
            plan.tasks()
                .iter()
                .map(|task| {
                    task.groups()
                        .iter()
                        .map(|group| group.units()[0].text().to_owned())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
            [
                vec!["<a>".to_owned(), "target-1".to_owned()],
                vec!["<e>".to_owned(), "target-5".to_owned()],
            ],
            "中间零 ID 块只能被过滤，前后块不能合并或失去完整兄弟 Group"
        );
        for task in plan.tasks() {
            assert_eq!(
                task.expected_output_ids()
                    .map(TaskId::get)
                    .collect::<Vec<_>>(),
                [1]
            );
        }

        let all_current = plan_translation_with_cancellation(
            &snapshot,
            &planning_for(&[]),
            target,
            |_, candidate| Ok(candidate.to_owned()),
            &CooperativeCancellation::default(),
        )
        .expect("全部 Current 仍应完成稳定装箱和责任分配");
        assert!(all_current.tasks().is_empty());
    }

    #[test]
    fn current_text_without_safe_placeholder_projection_stops_task_materialization() {
        let snapshot = stored_snapshot();
        let mut planning_units = planning(&snapshot);
        let current = planning_units
            .iter_mut()
            .find(|unit| unit.current_translation.is_some())
            .expect("测试快照应含 Current Unit");
        current.current_context = None;

        let error = plan_translation(&snapshot, &planning_units, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect_err("缺少安全目标语境时不得渲染含原始 Placeholder 的 TaskBlock");
        assert!(matches!(
            error,
            GenericPlanningError::MissingCurrentContext(_)
        ));
    }

    #[test]
    fn missing_later_current_context_is_reported_after_different_currents_are_found() {
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
                translation: Some(GenericStoredTranslation {
                    translation: "译文丙".to_owned(),
                    origin: TranslationOrigin::Manual,
                    state_fingerprint: fingerprint(11),
                }),
            }],
        });
        let mut planning_units = planning(&snapshot);
        planning_units
            .iter_mut()
            .find(|unit| unit.key().group_id() == "g4")
            .expect("测试快照应含第三个 Current")
            .current_context = None;

        let error = plan_translation(&snapshot, &planning_units, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect_err("不同 Current 不能让后续安全语境缺失变成 panic");

        assert!(matches!(
            error,
            GenericPlanningError::MissingCurrentContext(key) if key.group_id() == "g4"
        ));
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
    fn planning_uses_natural_positions_but_still_validates_non_natural_input() {
        let snapshot = stored_snapshot();
        let natural = planning(&snapshot);
        let expected =
            plan_translation(&snapshot, &natural, |_, candidate| Ok(candidate.to_owned()))
                .expect("自然顺序规划应成功");

        let mut reordered = natural.clone();
        reordered.reverse();
        let actual = plan_translation(&snapshot, &reordered, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("非自然顺序仍应按身份恢复");
        assert_eq!(actual, expected);

        let mut duplicated = natural;
        duplicated[1] = duplicated[0].clone();
        assert!(matches!(
            plan_translation(&snapshot, &duplicated, |_, candidate| {
                Ok(candidate.to_owned())
            }),
            Err(GenericPlanningError::Duplicate(_))
        ));
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
    fn current_reuse_validator_can_propagate_cancellation() {
        let mut snapshot = stored_snapshot();
        snapshot.files[0].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "相同".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(9),
        });
        let planning_units = planning(&snapshot);

        let result = plan_translation_with_validator_and_cancellation(
            &snapshot,
            &planning_units,
            NonZeroUsize::MAX,
            |_, _| Err(GenericPlanningError::Cancelled),
            &CooperativeCancellation::default(),
        );

        assert!(matches!(result, Err(GenericPlanningError::Cancelled)));
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

        let plan = plan_translation_with_validator_and_cancellation(
            &snapshot,
            &planning(&snapshot),
            NonZeroUsize::MAX,
            |key, candidate| {
                if key.group_id() == "g3" {
                    Ok(Err("目标 kind 不接受该译文".to_owned()))
                } else {
                    Ok(Ok(ValidatedReuse::new(
                        format!("{candidate}-已验收"),
                        format!("<{candidate}-已验收>"),
                    )))
                }
            },
            &CooperativeCancellation::default(),
        )
        .expect("复用验收失败不应中止规划");

        assert_eq!(plan.reused().len(), 1);
        assert_eq!(plan.reused()[0].key().group_id(), "g4");
        assert_eq!(plan.reused()[0].translation(), "相同-已验收");
        let context_task = plan
            .tasks()
            .iter()
            .find(|task| task.relative_path() == Path::new("b.jsonl"))
            .expect("复用语境应与同文件的模型目标一起发送");
        assert_eq!(context_task.groups()[1].units()[0].output_id(), None);
        assert_eq!(
            context_task.groups()[1].units()[0].text(),
            "<相同-已验收>",
            "模型必须看到验收后的安全语境，不能继续使用验收前的 Current 投影"
        );
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
                .contains(&ResponseProblem::NonStringValue(task_id(2)))
        );
        assert!(
            acceptance
                .problems()
                .contains(&ResponseProblem::UnexpectedId(task_id(99)))
        );
    }

    #[test]
    fn deeply_nested_non_string_only_rejects_its_generic_id() {
        const DEPTH: usize = 10_000;

        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let deep_value = format!("{}0{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let response = format!(r#"{{"1":"同级合法译文","2":{deep_value}}}"#);

        let acceptance = accept_response(
            &plan.tasks()[0],
            &response,
            TranslationResponseEnvelope::JsonOnly,
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("任意深的值不应破坏有效外层 object");

        assert_eq!(
            acceptance.accepted().len(),
            2,
            "合法同级 ID 仍应传播到两个 Generic Unit"
        );
        assert_eq!(
            acceptance.problems(),
            [ResponseProblem::NonStringValue(task_id(2))]
        );
    }

    #[test]
    fn response_validation_polls_cancellation_inside_long_candidate_text() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let response = format!(r#"{{"1":"{}"}}"#, "文".repeat(128 * 1024));
        let parsed = parse_translation_response(&response, TranslationResponseEnvelope::JsonOnly)
            .expect("响应应可解析");
        let polls = Cell::new(0_usize);

        let result = accept_parsed_response_with_cancellation(
            plan.tasks()[0].clone(),
            &parsed,
            |_, _, _| -> Result<Result<String, String>, GenericPlanningError> {
                panic!("长候选正文扫描被取消后不应进入目标验收")
            },
            || {
                let next = polls.get() + 1;
                polls.set(next);
                next >= 7
            },
        );

        assert!(matches!(result, Err(GenericPlanningError::Cancelled)));
        assert_eq!(polls.get(), 7);
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
                    output_id: task_id(1),
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
                if output_id == task_id(1) {
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
                    output_id: task_id(1),
                    key: GenericUnitKey::new("g1".to_owned(), "u1".to_owned()),
                    detail: "该去重族的目标均拒绝译文".to_owned(),
                },
                ResponseProblem::InvalidDestination {
                    output_id: task_id(1),
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
                ResponseProblem::DuplicateId(task_id(1)),
                ResponseProblem::MissingId(task_id(2))
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
