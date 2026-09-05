//! Generic 的稳定 TaskBlock 投影、全局去重与字符串响应验收。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use crate::diagnostic::{
    GenericJsonErrorCategory, GenericPlaceholderMultisetProblem, GenericResponseDestinationProblem,
    GenericResponseTextProblem, GenericResponseValueProblem, GenericTaskResponseProblem,
    GenericUnitLocator as DiagnosticGenericUnitLocator,
};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::Sha256Fingerprint;
use crate::language::{
    LanguageAnalysis, LanguageModule, LanguageOperationCancelled, LanguagePair, LanguageText,
    LanguageTextSegment,
};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::{
    ProvenInvariantViolation, ReviewFinding, ValidatedCandidate,
    validate_reflowed_candidate_text_with_cancellation,
};
use crate::translation::placeholder::{PlaceholderProtectionError, PlaceholderRestoreError};
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderMultisetError,
};
use crate::translation::placeholder_token;
use crate::translation::planning_resource::CompiledTerminology;
use crate::translation::task_planning::{
    StableGroupCharacters, TaskId, TaskPlanningError, TaskPlanningGroupLayout, TaskPlanningLayout,
    TaskPlanningScopeLayout, UnitTaskResponsibility, assign_task_ids, pack_complete_task_blocks,
};
use crate::translation_protocol::{
    DecodedJsonStringArray, DecodedSourceEchoFieldsError, DecodedSourceEchoValue,
    DecodedTranslationAssistantValue, ParsedTranslationResponse,
};
#[cfg(test)]
use crate::translation_protocol::{
    TranslationResponseMode, TranslationTaskResponseParseError, parse_translation_response,
};

use super::identity::{
    CancellableTextMap, FingerprintBucketMap, framed_identity_fingerprint_with_cancellation,
    identity_bytes_equal_with_cancellation,
};
use super::placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
    GenericProtectedText,
};
use super::project::{
    GenericProject, GenericStoredGroup, GenericStoredSnapshot, GenericStoredTranslation,
    GenericStoredUnit, RejectedTranslationWrite, TranslationClear, TranslationWrite,
};
use super::write_back::GenericCurrentTranslation;

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

/// 规划阶段保留的完整自然位置；稳定身份仍只由 [`GenericUnitKey`] 决定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericPlanningUnitLocator {
    relative_path: PathBuf,
    group_id: String,
    unit_id: String,
    role: String,
    line: Option<usize>,
    unit: Option<usize>,
}

impl GenericPlanningUnitLocator {
    pub(crate) fn new(
        relative_path: impl AsRef<Path>,
        group_id: impl Into<String>,
        unit_id: impl Into<String>,
        role: impl Into<String>,
    ) -> Self {
        Self {
            relative_path: relative_path.as_ref().to_path_buf(),
            group_id: group_id.into(),
            unit_id: unit_id.into(),
            role: role.into(),
            line: None,
            unit: None,
        }
    }

    pub(crate) fn with_natural_position(mut self, line: usize, unit: usize) -> Self {
        self.line = Some(line);
        self.unit = Some(unit);
        self
    }

    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn group_id(&self) -> &str {
        &self.group_id
    }

    pub(crate) fn unit_id(&self) -> &str {
        &self.unit_id
    }

    pub(crate) fn role(&self) -> &str {
        &self.role
    }

    pub(crate) const fn natural_position(&self) -> Option<(usize, usize)> {
        match (self.line, self.unit) {
            (Some(line), Some(unit)) => Some((line, unit)),
            _ => None,
        }
    }
}

/// Generic 面向人的自然 Unit ID。所有导出、Placeholder 精确目标和 Rejected 状态共用
/// 这一表达，不能从数据库随机身份反推。
pub(crate) fn readable_generic_unit_id(relative_path: &Path, line: usize, unit: usize) -> String {
    let readable_path = relative_path.to_string_lossy().replace('\\', "/");
    format!("{readable_path}:line{line}:unit{unit}:text")
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
    locator: GenericPlanningUnitLocator,
    protected_text: String,
    placeholder_binding_fingerprint: Sha256Fingerprint,
    terminology_indices: Vec<usize>,
    needs_translation: bool,
    current_translation: Option<String>,
    current_context: Option<CurrentContext>,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
    invalidated_previous: Option<GenericStoredTranslation>,
    invalidation_violation: Option<ProvenInvariantViolation>,
    current_rejected: bool,
    retry_rejected: bool,
}

/// Current Unit 在完整 TaskBlock 中提供的安全目标语境。
#[derive(Clone, Debug, Eq, PartialEq)]
enum CurrentContext {
    SafeTarget(String),
}

impl CurrentContext {
    fn text(&self) -> &str {
        match self {
            Self::SafeTarget(text) => text,
        }
    }

    fn reuse_candidate(&self) -> Option<&str> {
        match self {
            Self::SafeTarget(text) => Some(text),
        }
    }
}

/// 测试中从受信持久化记录建立 PlanningUnit 所需的全部事实。
///
/// 生产路径使用带取消检查的独立参数版本；这个测试夹具把同一组事实作为一个值传入，避免
/// 测试辅助函数重新形成难以维护的长参数列表。
#[cfg(test)]
struct StoredPlanningUnitInput<'a> {
    relative_path: &'a Path,
    project: &'a GenericProject,
    group: &'a GenericStoredGroup,
    unit: &'a GenericStoredUnit,
    protected: &'a GenericProtectedText,
    terminology_indices: Vec<usize>,
    needs_translation: bool,
    retry_rejected: bool,
}

impl PlanningUnit {
    #[cfg(test)]
    pub(crate) fn new(
        key: GenericUnitKey,
        locator: GenericPlanningUnitLocator,
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
            locator,
            protected_text,
            placeholder_binding_fingerprint,
            terminology_indices: Vec::new(),
            needs_translation,
            current_translation,
            current_context,
            expected_state_fingerprint,
            expected_previous: None,
            invalidated_previous: None,
            invalidation_violation: None,
            current_rejected: false,
            retry_rejected: false,
        }
    }

    /// 用持久化记录和本次实际资源计算 Current，调用方不需要解释状态字段。
    #[cfg(test)]
    fn from_stored(input: StoredPlanningUnitInput<'_>) -> Self {
        Self::from_stored_with_cancellation(
            input.relative_path,
            input.project,
            input.group,
            input.unit,
            input.protected,
            input.terminology_indices,
            input.needs_translation,
            input.retry_rejected,
            &CooperativeCancellation::default(),
        )
        .expect("不取消的受信 PlanningUnit 必须可以建立")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored_with_cancellation(
        relative_path: &Path,
        project: &GenericProject,
        group: &GenericStoredGroup,
        unit: &GenericStoredUnit,
        protected: &GenericProtectedText,
        terminology_indices: Vec<usize>,
        needs_translation: bool,
        retry_rejected: bool,
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
            cancellation,
        )?;
        let current_translation = current_translation_for_expected_applicability_with_cancellation(
            unit,
            automatic_state,
            cancellation,
        )?;
        let previous = unit
            .translation()
            .map(|translation| clone_stored_translation(translation, cancellation))
            .transpose()?;
        let source_lines = unit.source_text().split('\n').collect::<Vec<_>>();
        let current_rejected = current_translation.is_none()
            && unit.rejected().is_some_and(|rejected| {
                rejected.source.len() == source_lines.len()
                    && rejected
                        .source
                        .iter()
                        .zip(&source_lines)
                        .all(|(stored, current)| stored == current)
                    && rejected.group_context == group.context_fingerprint()
                    && rejected.planning_state == automatic_state
            });
        // 状态变化只决定本轮是否需要新候选。旧正文继续作为 CAS 的预期值保留，直到
        // 替代候选通过验收并原子写入；请求失败、取消或额度不足不能先销毁它。
        let expected_previous = previous;
        let invalidated_previous = None;
        Ok(Self {
            locator: GenericPlanningUnitLocator::new(
                relative_path,
                clone_translation_text(group.id(), cancellation)?,
                clone_translation_text(unit.id(), cancellation)?,
                clone_translation_text(group.kind(), cancellation)?,
            )
            .with_natural_position(group.ordinal() + 1, unit.ordinal() + 1),
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
            invalidation_violation: None,
            current_rejected,
            retry_rejected,
        })
    }

    pub(crate) fn key(&self) -> &GenericUnitKey {
        &self.key
    }

    pub(crate) fn locator(&self) -> &GenericPlanningUnitLocator {
        &self.locator
    }

    pub(crate) fn needs_candidate(&self) -> bool {
        self.needs_translation
            && self.current_translation.is_none()
            && (!self.current_rejected || self.retry_rejected)
    }

    #[cfg(test)]
    pub(crate) const fn is_skipped_rejected(&self) -> bool {
        self.needs_translation
            && self.current_translation.is_none()
            && self.current_rejected
            && !self.retry_rejected
    }

    pub(crate) const fn is_current_rejected(&self) -> bool {
        self.current_rejected
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

    /// 把当前候选正文原样交给 Rejected，并使本轮默认不再请求同一 Unit。
    pub(crate) fn reject_invalid_current(&mut self, violation: ProvenInvariantViolation) {
        assert!(
            self.current_translation.is_some(),
            "只有 Current Unit 才能因强不变量失效"
        );
        self.current_translation = None;
        self.current_context = None;
        self.invalidated_previous = self.expected_previous.take();
        self.invalidation_violation = Some(violation);
        self.current_rejected = true;
    }
}

/// 依据持久化来源类型和本次语义资源判断一个已有译文是否仍为 Current。
///
/// 人工状态不依赖自动状态；自动正文只绑定决定实际适用性的项目事实，不绑定 Client、
/// Prompt、Profile、模型参数或语言检查阈值等未来请求策略。
pub(crate) fn current_translation_for_stored_with_cancellation(
    project: &GenericProject,
    group: &GenericStoredGroup,
    unit: &GenericStoredUnit,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericPlanningError> {
    let key = GenericUnitKey::new(
        clone_translation_text(group.id(), cancellation)?,
        clone_translation_text(unit.id(), cancellation)?,
    );
    let expected = automatic_translation_state_fingerprint_with_cancellation(
        project.language_pair(),
        &key,
        unit.source_text(),
        group.context_fingerprint(),
        cancellation,
    )?;
    current_translation_for_expected_applicability_with_cancellation(unit, expected, cancellation)
}

fn current_translation_for_expected_applicability_with_cancellation(
    unit: &GenericStoredUnit,
    expected_automatic_applicability: Sha256Fingerprint,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericPlanningError> {
    ensure_translation_not_cancelled(cancellation)?;
    let Some(translation) = unit.translation() else {
        return Ok(None);
    };
    if translation.origin() == TranslationOrigin::Manual {
        return Ok(Some(clone_translation_text(
            translation.translation(),
            cancellation,
        )?));
    }
    // Placeholder 不属于正文适用性 digest，随后仍会按当前规则独立执行强验收。
    if translation.state_fingerprint() == expected_automatic_applicability {
        Ok(Some(clone_translation_text(
            translation.translation(),
            cancellation,
        )?))
    } else {
        Ok(None)
    }
}

/// 当前 Translate 证明违反强不变量、必须转入 Rejected 的旧译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedInvalidation {
    key: GenericUnitKey,
    readable_id: String,
    expected_source_text: String,
    expected_group_context: Sha256Fingerprint,
    expected_translation: GenericStoredTranslation,
    violation: ProvenInvariantViolation,
    rejection_planning_state: Sha256Fingerprint,
}

impl PlannedInvalidation {
    pub(crate) fn into_clear(self) -> TranslationClear {
        TranslationClear {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            readable_id: self.readable_id,
            expected_source_text: self.expected_source_text,
            expected_group_context: self.expected_group_context,
            expected_translation: self.expected_translation,
            violation: self.violation,
            rejection_planning_state: self.rejection_planning_state,
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
    was_current_rejected: bool,
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
            state_fingerprint: self.expected_state_fingerprint,
            expected_translation: self.expected_previous,
            was_current_rejected: self.was_current_rejected,
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
    locator: GenericPlanningUnitLocator,
    expected_source_text: String,
    expected_source: Vec<String>,
    expected_group_context: Sha256Fingerprint,
    expected_state_fingerprint: Sha256Fingerprint,
    expected_previous: Option<GenericStoredTranslation>,
    source_language: String,
    target_language: String,
    was_current_rejected: bool,
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

    /// 本任务的全部实际 Generic Unit 数；同一模型 ID 的传播目标各自计入。
    pub(crate) fn unit_count(&self) -> usize {
        self.outputs.values().map(Vec::len).sum()
    }

    pub(crate) fn expected_output_count(&self) -> usize {
        self.outputs.len()
    }

    #[cfg(test)]
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
    planned_units: usize,
    initial_rejected_units: usize,
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
        usize,
        usize,
    ) {
        (
            self.invalidations,
            self.reused,
            self.tasks,
            self.planned_units,
            self.initial_rejected_units,
        )
    }
}

/// Generic 稳定装箱、状态规划或 Extract 快照投影无法建立完整结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GenericPlanningError {
    Cancelled,
    TaskPlanning(TaskPlanningError),
    MissingCurrentContext(GenericPlanningUnitLocator),
    Missing(GenericPlanningUnitLocator),
    Unknown(GenericPlanningUnitLocator),
    Duplicate(GenericPlanningUnitLocator),
}

impl fmt::Display for GenericPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 翻译规划已取消"),
            Self::TaskPlanning(source) => source.fmt(formatter),
            Self::MissingCurrentContext(locator) => write!(
                formatter,
                "Current Generic Unit 缺少安全目标语境：{}/{}",
                locator.group_id, locator.unit_id
            ),
            Self::Missing(locator) => write!(
                formatter,
                "缺少 Generic Unit 的规划事实：{}/{}",
                locator.group_id, locator.unit_id
            ),
            Self::Unknown(locator) => write!(
                formatter,
                "规划事实引用了不存在的 Generic Unit：{}/{}",
                locator.group_id, locator.unit_id
            ),
            Self::Duplicate(locator) => write!(
                formatter,
                "同一 Generic Unit 出现多份规划事实：{}/{}",
                locator.group_id, locator.unit_id
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

/// Generic Translate 准备与候选验收共享的完整 Unit 事实。
#[derive(Clone)]
pub(crate) struct GenericValidationFact {
    locator: GenericUnitLocator,
    kind: String,
    protected: GenericProtectedText,
    analysis: LanguageAnalysis,
}

/// 一次 Generic Translate 的领域准备结果。
pub(crate) struct PreparedGenericTranslation {
    plan: TranslationPlan,
    facts: GenericUnitMap<GenericValidationFact>,
}

impl PreparedGenericTranslation {
    #[cfg(test)]
    pub(crate) fn plan(&self) -> &TranslationPlan {
        &self.plan
    }

    #[cfg(test)]
    pub(crate) fn facts(&self) -> &GenericUnitMap<GenericValidationFact> {
        &self.facts
    }

    pub(crate) fn into_parts(self) -> (TranslationPlan, GenericUnitMap<GenericValidationFact>) {
        (self.plan, self.facts)
    }
}

struct PreparedGenericGroup {
    planning_units: Vec<PlanningUnit>,
    facts: Vec<(GenericUnitKey, GenericValidationFact)>,
}

/// Generic Placeholder 规则的当前权威来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GenericPlaceholderRuleSource {
    ExternalFile(PathBuf),
    ProjectSnapshot,
}

/// Generic Unit 的完整自然位置，用于领域错误与面向人诊断的稳定交接。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericUnitLocator {
    relative_path: PathBuf,
    group_id: String,
    unit_id: String,
    role: String,
    line: usize,
    unit: usize,
}

impl GenericUnitLocator {
    pub(crate) fn new(
        relative_path: impl Into<PathBuf>,
        group_id: impl Into<String>,
        unit_id: impl Into<String>,
        role: impl Into<String>,
        line: usize,
        unit: usize,
    ) -> Self {
        Self {
            relative_path: relative_path.into(),
            group_id: group_id.into(),
            unit_id: unit_id.into(),
            role: role.into(),
            line,
            unit,
        }
    }

    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn group_id(&self) -> &str {
        &self.group_id
    }

    pub(crate) fn unit_id(&self) -> &str {
        &self.unit_id
    }

    pub(crate) fn role(&self) -> &str {
        &self.role
    }

    pub(crate) const fn natural_position(&self) -> (usize, usize) {
        (self.line, self.unit)
    }

    fn readable_id(&self) -> String {
        readable_generic_unit_id(&self.relative_path, self.line, self.unit)
    }
}

/// Generic Translate 准备阶段无法建立可验收领域状态。
#[derive(Debug)]
pub(crate) enum GenericPreparationError {
    Cancelled,
    Placeholder {
        rule_source: GenericPlaceholderRuleSource,
        source: GenericPlaceholderError,
    },
    PlaceholderProtection {
        rule_source: GenericPlaceholderRuleSource,
        locator: GenericUnitLocator,
        source: PlaceholderProtectionError,
    },
    LanguageProjection {
        locator: GenericUnitLocator,
        source: LanguageTextProjectionError,
    },
    Planning(GenericPlanningError),
}

impl fmt::Display for GenericPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic CPU 工作已取消"),
            Self::Placeholder { source, .. } => source.fmt(formatter),
            Self::PlaceholderProtection { source, .. } => source.fmt(formatter),
            Self::LanguageProjection { source, .. } => source.fmt(formatter),
            Self::Planning(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Placeholder { source, .. } => Some(source),
            Self::PlaceholderProtection { source, .. } => Some(source),
            Self::LanguageProjection { source, .. } => Some(source),
            Self::Planning(source) => Some(source),
        }
    }
}

impl From<GenericPlanningError> for GenericPreparationError {
    fn from(source: GenericPlanningError) -> Self {
        Self::Planning(source)
    }
}

impl GenericPreparationError {
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::Planning(source) if source.is_cancelled())
    }
}

fn generic_placeholder_protection_failure(
    source: GenericPlaceholderError,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> GenericPreparationError {
    match source {
        GenericPlaceholderError::Protection(source) => {
            GenericPreparationError::PlaceholderProtection {
                rule_source: rule_source.clone(),
                locator: locator.clone(),
                source,
            }
        }
        source => GenericPreparationError::Placeholder {
            rule_source: rule_source.clone(),
            source,
        },
    }
}

// 这是翻译规划边界：每项参数都有独立的所有权和取消语义，合并为可变上下文会掩盖它们。
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_generic_translation(
    snapshot: &GenericStoredSnapshot,
    terminology: Arc<CompiledTerminology>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    source_language: Arc<dyn LanguageModule>,
    target_task_characters: NonZeroUsize,
    retry_rejected: bool,
    cancellation: &CooperativeCancellation,
) -> Result<PreparedGenericTranslation, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut groups = Vec::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for (group_ordinal, group) in file.groups().iter().enumerate() {
            ensure_generic_cpu_running(cancellation)?;
            groups.push((file.relative_path(), group_ordinal, group));
        }
    }
    let prepared_groups = groups
        .par_iter()
        .map(|(relative_path, group_ordinal, group)| {
            ensure_generic_cpu_running(cancellation)?;
            let service = GenericPlaceholderService::default();
            let mut prepared_units = Vec::with_capacity(group.units().len());
            for (unit_ordinal, unit) in group.units().iter().enumerate() {
                ensure_generic_cpu_running(cancellation)?;
                let locator = GenericUnitLocator::new(
                    relative_path.to_path_buf(),
                    group.id(),
                    unit.id(),
                    group.kind(),
                    group_ordinal + 1,
                    unit_ordinal + 1,
                );
                let target_id = locator.readable_id();
                let protected = service
                    .protect_target_with_cancellation(
                        &target_id,
                        group.kind(),
                        unit.source_text(),
                        placeholder_rules,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .map_err(|source| {
                        generic_placeholder_protection_failure(
                            source,
                            placeholder_rule_source,
                            &locator,
                        )
                    })?;
                let language_text = protected
                    .language_text_with_cancellation(|| ensure_generic_cpu_running(cancellation))?
                    .map_err(|source| GenericPreparationError::LanguageProjection {
                        locator: locator.clone(),
                        source,
                    })?;
                let analysis = source_language
                    .analyze_source_with_cancellation(&language_text, &mut || {
                        ensure_generic_language_running(cancellation)
                    })
                    .map_err(|LanguageOperationCancelled| GenericPreparationError::Cancelled)?;
                ensure_generic_cpu_running(cancellation)?;
                prepared_units.push((unit, locator, protected, language_text, analysis));
            }
            ensure_generic_cpu_running(cancellation)?;
            let term_indices = terminology.triggered_indices_with_cancellation(
                prepared_units
                    .iter()
                    .flat_map(|(_, _, _, language_text, _)| natural_segments(language_text)),
                || ensure_generic_cpu_running(cancellation),
            )?;
            let mut planning_units = Vec::with_capacity(prepared_units.len());
            let mut facts = Vec::with_capacity(prepared_units.len());
            for (unit, locator, protected, language_text, analysis) in prepared_units {
                ensure_generic_cpu_running(cancellation)?;
                let mut planning = PlanningUnit::from_stored_with_cancellation(
                    relative_path,
                    snapshot.project(),
                    group,
                    unit,
                    &protected,
                    clone_generic_cpu_indices(&term_indices, cancellation)?,
                    generic_language_text_has_non_whitespace_natural_text(
                        &language_text,
                        cancellation,
                    )? && analysis.needs_translation(),
                    retry_rejected,
                    cancellation,
                )
                .map_err(GenericPreparationError::Planning)?;
                if let Some(current_translation) = planning.current_translation() {
                    let text_violation = validate_reflowed_candidate_text_with_cancellation(
                        current_translation,
                        || ensure_generic_cpu_running(cancellation),
                    )?
                    .err();
                    let current_protected = if text_violation.is_none() {
                        generic_current_translation_protection_result(
                            service
                                .bind_target_candidate_with_cancellation(
                                    &protected,
                                    &locator.readable_id(),
                                    group.kind(),
                                    current_translation,
                                    placeholder_rules,
                                    || ensure_generic_cpu_running(cancellation),
                                )?
                                .map_err(Into::into),
                            placeholder_rule_source,
                            &locator,
                        )?
                    } else {
                        None
                    };
                    if let Some(current_protected) = current_protected
                        && current_protected.binding_fingerprint()
                            == protected.binding_fingerprint()
                    {
                        planning.install_current_target_context(clone_generic_cpu_text(
                            current_protected.text(),
                            cancellation,
                        )?);
                    } else {
                        planning.reject_invalid_current(
                            text_violation.unwrap_or(ProvenInvariantViolation::PlaceholderMismatch),
                        );
                    }
                }
                if planning.needs_candidate() {
                    facts.push((
                        clone_generic_unit_key(planning.key(), cancellation)?,
                        GenericValidationFact {
                            locator,
                            kind: clone_generic_cpu_text(group.kind(), cancellation)?,
                            protected,
                            analysis,
                        },
                    ));
                }
                planning_units.push(planning);
            }
            ensure_generic_cpu_running(cancellation)?;
            Ok::<_, GenericPreparationError>(PreparedGenericGroup {
                planning_units,
                facts,
            })
        })
        .collect::<Vec<_>>();

    let mut planning_units = Vec::new();
    let mut facts = GenericUnitMap::new();
    // 并行完成顺序不参与领域语义；按自然 Group 顺序处理结果，保证规划和错误稳定。
    for prepared_group in prepared_groups {
        ensure_generic_cpu_running(cancellation)?;
        let prepared_group = prepared_group?;
        for planning_unit in prepared_group.planning_units {
            ensure_generic_cpu_running(cancellation)?;
            planning_units.push(planning_unit);
        }
        for (key, fact) in prepared_group.facts {
            ensure_generic_cpu_running(cancellation)?;
            let previous = facts
                .insert_with_cancellation(key, fact, || ensure_generic_cpu_running(cancellation))?;
            debug_assert!(previous.is_none());
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    let plan = plan_translation_with_validator_and_cancellation(
        snapshot,
        &planning_units,
        target_task_characters,
        |key, candidate| {
            validate_generic_reuse_with_cancellation(
                key,
                candidate,
                &facts,
                placeholder_rules,
                placeholder_rule_source,
                source_language.as_ref(),
                cancellation,
            )
        },
        cancellation,
    )?;
    Ok(PreparedGenericTranslation { plan, facts })
}

pub(crate) fn collect_generic_current_translations(
    snapshot: &GenericStoredSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<GenericUnitMap<GenericCurrentTranslation>, GenericPreparationError> {
    ensure_generic_cpu_running(cancellation)?;
    let mut current = GenericUnitMap::new();
    for file in snapshot.files() {
        ensure_generic_cpu_running(cancellation)?;
        for group in file.groups() {
            ensure_generic_cpu_running(cancellation)?;
            for unit in group.units() {
                ensure_generic_cpu_running(cancellation)?;
                if let Some(translation) = current_translation_for_stored_with_cancellation(
                    snapshot.project(),
                    group,
                    unit,
                    cancellation,
                )
                .map_err(GenericPreparationError::Planning)?
                {
                    let key = GenericUnitKey::new(
                        clone_generic_cpu_text(group.id(), cancellation)?,
                        clone_generic_cpu_text(unit.id(), cancellation)?,
                    );
                    let translation = GenericCurrentTranslation::new(
                        translation,
                        unit.translation()
                            .is_some_and(|stored| stored.origin() == TranslationOrigin::Manual),
                    );
                    let previous = current.insert_with_cancellation(key, translation, || {
                        ensure_generic_cpu_running(cancellation)
                    })?;
                    debug_assert!(previous.is_none());
                }
            }
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(current)
}

pub(crate) fn ensure_generic_cpu_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPreparationError> {
    if cancellation.is_requested() {
        Err(GenericPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

fn ensure_generic_language_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), LanguageOperationCancelled> {
    if cancellation.is_requested() {
        Err(LanguageOperationCancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn clone_generic_cpu_text(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericPreparationError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut output = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_generic_cpu_running(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(output)
}

fn clone_generic_cpu_indices(
    indices: &[usize],
    cancellation: &CooperativeCancellation,
) -> Result<Vec<usize>, GenericPreparationError> {
    const CANCELLATION_CHECK_ITEMS: usize = 1024;

    let mut output = Vec::with_capacity(indices.len());
    for chunk in indices.chunks(CANCELLATION_CHECK_ITEMS) {
        ensure_generic_cpu_running(cancellation)?;
        output.extend_from_slice(chunk);
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(output)
}

fn clone_generic_unit_key(
    key: &GenericUnitKey,
    cancellation: &CooperativeCancellation,
) -> Result<GenericUnitKey, GenericPreparationError> {
    Ok(GenericUnitKey::new(
        clone_generic_cpu_text(key.group_id(), cancellation)?,
        clone_generic_cpu_text(key.unit_id(), cancellation)?,
    ))
}

pub(crate) fn generic_cpu_text_equal(
    left: &str,
    right: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPreparationError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_generic_cpu_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CANCELLATION_CHECK_BYTES)
        .zip(right.as_bytes().chunks(CANCELLATION_CHECK_BYTES))
    {
        ensure_generic_cpu_running(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(true)
}

fn generic_language_text_has_non_whitespace_natural_text(
    text: &LanguageText,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPreparationError> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    for segment in text.segments() {
        ensure_generic_cpu_running(cancellation)?;
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        for (index, character) in text.chars().enumerate() {
            if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
                ensure_generic_cpu_running(cancellation)?;
            }
            if !character.is_whitespace() {
                return Ok(true);
            }
        }
    }
    ensure_generic_cpu_running(cancellation)?;
    Ok(false)
}

fn natural_segments(language_text: &LanguageText) -> impl Iterator<Item = &str> {
    language_text
        .segments()
        .iter()
        .filter_map(|segment| match segment {
            LanguageTextSegment::NaturalText(text) => Some(text.as_str()),
            LanguageTextSegment::OpaqueBoundary => None,
        })
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
    locator: GenericPlanningUnitLocator,
    source_text: &'a str,
    group_context: Sha256Fingerprint,
}

struct UnitFacts<'input, 'snapshot> {
    key: GenericUnitKey,
    locator: GenericPlanningUnitLocator,
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
            return Err(GenericPlanningError::Duplicate(unit.locator().clone()));
        }
    }

    let mut known = GenericUnitMap::with_capacity(natural_units.len());
    let mut resolved = Vec::with_capacity(natural_units.len());
    for natural in natural_units {
        ensure_planning_not_cancelled(is_cancelled)?;
        let Some(input) = supplied
            .get_with_cancellation(&natural.key, || ensure_planning_not_cancelled(is_cancelled))?
        else {
            return Err(GenericPlanningError::Missing(natural.locator.clone()));
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
            return Err(GenericPlanningError::Unknown(unit.locator().clone()));
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
    reuse_validator: impl Fn(&GenericUnitKey, &str) -> Result<String, GenericResponseDestinationProblem>,
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
    reuse_validator: impl Fn(&GenericUnitKey, &str) -> Result<String, GenericResponseDestinationProblem>,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationPlan, GenericPlanningError> {
    plan_translation_with_validator_and_cancellation(
        snapshot,
        planning_units,
        target_task_characters,
        |key, candidate| {
            Ok::<_, GenericPlanningError>(
                reuse_validator(key, candidate).map(ValidatedReuse::same_text),
            )
        },
        cancellation,
    )
}

/// 与 [`plan_translation_with_cancellation`] 相同，并允许复用验收自身传播取消。
pub(crate) fn plan_translation_with_validator_and_cancellation<E>(
    snapshot: &GenericStoredSnapshot,
    planning_units: &[PlanningUnit],
    target_task_characters: NonZeroUsize,
    reuse_validator: impl Fn(
        &GenericUnitKey,
        &str,
    )
        -> Result<Result<ValidatedReuse, GenericResponseDestinationProblem>, E>,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationPlan, E>
where
    E: From<GenericPlanningError>,
{
    let is_cancelled = || cancellation.is_requested();
    ensure_planning_not_cancelled(&is_cancelled)?;
    let (task_layout, scope_file_indices) = generic_task_planning_layout(snapshot, &is_cancelled)?;
    let complete_task_plan =
        pack_complete_task_blocks(&task_layout, target_task_characters, cancellation)
            .map_err(GenericPlanningError::from)?;
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
                    locator: GenericPlanningUnitLocator::new(
                        file.relative_path(),
                        clone_planning_text_with_cancellation(group.id(), &is_cancelled)?,
                        clone_planning_text_with_cancellation(unit.id(), &is_cancelled)?,
                        clone_planning_text_with_cancellation(group.kind(), &is_cancelled)?,
                    )
                    .with_natural_position(group.ordinal() + 1, unit.ordinal() + 1),
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
            locator: natural.locator,
            input,
            source_text: natural.source_text,
            group_context: natural.group_context,
        })
        .collect::<Vec<_>>();

    let mut invalidations = Vec::new();
    for fact in &facts {
        ensure_planning_not_cancelled(&is_cancelled)?;
        if let Some(expected_translation) = fact.input.invalidated_previous.as_ref() {
            let (line, unit) = fact
                .locator
                .natural_position()
                .expect("数据库自然快照中的 Generic Unit 必须有自然位置");
            invalidations.push(PlannedInvalidation {
                key: clone_planning_key_with_cancellation(&fact.key, &is_cancelled)?,
                readable_id: readable_generic_unit_id(fact.locator.relative_path(), line, unit),
                expected_source_text: clone_planning_text_with_cancellation(
                    fact.source_text,
                    &is_cancelled,
                )?,
                expected_group_context: fact.group_context,
                expected_translation: clone_planning_stored_translation_with_cancellation(
                    expected_translation,
                    &is_cancelled,
                )?,
                violation: fact
                    .input
                    .invalidation_violation
                    .clone()
                    .expect("只有已经证明强不变量违反的正文才能进入失效计划"),
                rejection_planning_state: fact.input.expected_state_fingerprint,
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
                return Err(
                    GenericPlanningError::MissingCurrentContext(fact.locator.clone()).into(),
                );
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
                    return Err(
                        GenericPlanningError::MissingCurrentContext(fact.locator.clone()).into(),
                    );
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
            if fact.input.needs_candidate() {
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
                        locator: fact.locator.clone(),
                        expected_source_text: clone_planning_text_with_cancellation(
                            fact.source_text,
                            &is_cancelled,
                        )?,
                        expected_source: clone_planning_source_lines_with_cancellation(
                            fact.source_text,
                            &is_cancelled,
                        )?,
                        expected_group_context: fact.group_context,
                        expected_state_fingerprint: fact.input.expected_state_fingerprint,
                        target_language: snapshot
                            .project()
                            .language_pair()
                            .target()
                            .as_str()
                            .to_owned(),
                        source_language: snapshot
                            .project()
                            .language_pair()
                            .source()
                            .as_str()
                            .to_owned(),
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
                        was_current_rejected: fact.input.is_current_rejected(),
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
                    locator: fact.locator.clone(),
                    expected_source_text: clone_planning_text_with_cancellation(
                        fact.source_text,
                        &is_cancelled,
                    )?,
                    expected_source: clone_planning_source_lines_with_cancellation(
                        fact.source_text,
                        &is_cancelled,
                    )?,
                    expected_group_context: fact.group_context,
                    expected_state_fingerprint: fact.input.expected_state_fingerprint,
                    target_language: snapshot
                        .project()
                        .language_pair()
                        .target()
                        .as_str()
                        .to_owned(),
                    source_language: snapshot
                        .project()
                        .language_pair()
                        .source()
                        .as_str()
                        .to_owned(),
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
                    was_current_rejected: fact.input.is_current_rejected(),
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
                            was_current_rejected: destination.was_current_rejected,
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
                locator: fact.locator.clone(),
                expected_source_text: clone_planning_text_with_cancellation(
                    fact.source_text,
                    &is_cancelled,
                )?,
                expected_source: clone_planning_source_lines_with_cancellation(
                    fact.source_text,
                    &is_cancelled,
                )?,
                expected_group_context: fact.group_context,
                expected_state_fingerprint: fact.input.expected_state_fingerprint,
                target_language: snapshot
                    .project()
                    .language_pair()
                    .target()
                    .as_str()
                    .to_owned(),
                source_language: snapshot
                    .project()
                    .language_pair()
                    .source()
                    .as_str()
                    .to_owned(),
                expected_previous: fact
                    .input
                    .expected_previous
                    .as_ref()
                    .map(|previous| {
                        clone_planning_stored_translation_with_cancellation(previous, &is_cancelled)
                    })
                    .transpose()?,
                was_current_rejected: fact.input.is_current_rejected(),
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
    let assigned_task_plan = assign_task_ids(complete_task_plan, &responsibilities, cancellation)
        .map_err(GenericPlanningError::from)?;

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

    let rejected_units_after_preparation = facts
        .iter()
        .filter(|fact| fact.input.is_current_rejected())
        .count();
    let rejected_units_in_tasks = tasks
        .iter()
        .flat_map(|task| task.outputs.values())
        .flatten()
        .filter(|destination| destination.was_current_rejected)
        .count();
    let planned_units = tasks
        .iter()
        .map(PlannedTask::unit_count)
        .sum::<usize>()
        .checked_add(
            rejected_units_after_preparation
                .checked_sub(rejected_units_in_tasks)
                .expect("Task 中的 Rejected Unit 必须来自本次规划事实"),
        )
        .expect("Generic 计划 Unit 数不得溢出");
    let initial_rejected_units = rejected_units_after_preparation
        .checked_sub(invalidations.len())
        .expect("新失效 Unit 必须由非 Rejected 状态转入 Rejected");

    Ok(TranslationPlan {
        invalidations,
        reused,
        tasks,
        planned_units,
        initial_rejected_units,
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
    let mut group_characters = "{\"kind\":".len();
    group_characters = checked_stable_character_add(
        group_characters,
        stable_json_string_characters(group.kind(), is_cancelled)?,
    )?;
    group_characters = checked_stable_character_add(group_characters, ",\"units\":[".len())?;
    for (unit_index, unit) in group.units().iter().enumerate() {
        ensure_planning_not_cancelled(is_cancelled)?;
        if unit_index != 0 {
            group_characters = checked_stable_character_add(group_characters, 1)?;
        }
        group_characters = checked_stable_character_add(group_characters, "{\"text\":[".len())?;
        for (line_index, line) in unit.source_text().split('\n').enumerate() {
            ensure_planning_not_cancelled(is_cancelled)?;
            if line_index != 0 {
                group_characters = checked_stable_character_add(group_characters, 1)?;
            }
            group_characters = checked_stable_character_add(
                group_characters,
                stable_json_string_characters(line, is_cancelled)?,
            )?;
        }
        group_characters = checked_stable_character_add(group_characters, "]}".len())?;
    }
    group_characters = checked_stable_character_add(group_characters, "]}".len())?;
    let first = checked_stable_character_add(
        checked_stable_character_add("{\"groups\":[".len(), group_characters)?,
        "]}".len(),
    )?;
    let following = checked_stable_character_add(1, group_characters)?;
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

fn clone_planning_source_lines_with_cancellation(
    text: &str,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Vec<String>, GenericPlanningError> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        ensure_planning_not_cancelled(is_cancelled)?;
        lines.push(clone_planning_text_with_cancellation(line, is_cancelled)?);
    }
    Ok(lines)
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
    was_current_rejected: bool,
}

/// 一个已绑定到当前规划事实、只因硬不变量而未能入库的候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedTranslation {
    key: GenericUnitKey,
    locator: GenericPlanningUnitLocator,
    origin: TranslationOrigin,
    candidate_json: String,
    translation: Option<Vec<String>>,
    expected_source_text: String,
    source: Vec<String>,
    expected_group_context: Sha256Fingerprint,
    violation: ProvenInvariantViolation,
    planning_state: Sha256Fingerprint,
    source_language: String,
    target_language: String,
    expected_previous: Option<GenericStoredTranslation>,
    was_current_rejected: bool,
}

impl RejectedTranslation {
    pub(crate) fn into_write(self) -> RejectedTranslationWrite {
        let readable_path = self
            .locator
            .relative_path()
            .to_string_lossy()
            .replace('\\', "/");
        let expected_manual_applicability = crate::manual::generic_manual_applicability(
            self.key.group_id(),
            self.key.unit_id(),
            &readable_path,
            self.locator.role(),
            &self.source_language,
            &self.target_language,
            &self.source,
        );
        let readable_id = self.locator.natural_position().map_or_else(
            || {
                format!(
                    "{readable_path}:{}:{}:text",
                    self.key.group_id(),
                    self.key.unit_id()
                )
            },
            |(line, unit)| readable_generic_unit_id(self.locator.relative_path(), line, unit),
        );
        RejectedTranslationWrite {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            readable_id,
            origin: self.origin,
            expected_source_text: self.expected_source_text,
            source: self.source,
            expected_group_context: self.expected_group_context,
            expected_manual_applicability,
            candidate_json: self.candidate_json,
            translation: self.translation,
            violation: self.violation,
            planning_state: self.planning_state,
            expected_translation: self.expected_previous,
            was_current_rejected: self.was_current_rejected,
        }
    }
}

impl AcceptedTranslation {
    pub(crate) fn into_write(self) -> TranslationWrite {
        TranslationWrite {
            group_id: self.key.group_id,
            unit_id: self.key.unit_id,
            expected_source_text: self.expected_source_text,
            expected_group_context: self.expected_group_context,
            translation: self.translation,
            state_fingerprint: self.expected_state_fingerprint,
            expected_translation: self.expected_previous,
            was_current_rejected: self.was_current_rejected,
        }
    }
}

/// 可解析响应中单个 ID 的安全问题；日志与任务记录直接复用这一封闭类型。
pub(crate) type ResponseProblem = GenericTaskResponseProblem;

/// 一个有效 Generic 目标译文附带的非阻塞质量审核事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationReview {
    output_id: TaskId,
    locator: GenericPlanningUnitLocator,
    finding: ReviewFinding,
}

impl TranslationReview {
    pub(crate) fn new(
        output_id: TaskId,
        locator: GenericPlanningUnitLocator,
        finding: ReviewFinding,
    ) -> Self {
        Self {
            output_id,
            locator,
            finding,
        }
    }

    pub(crate) const fn output_id(&self) -> TaskId {
        self.output_id
    }

    pub(crate) fn locator(&self) -> &GenericPlanningUnitLocator {
        &self.locator
    }

    pub(crate) fn finding(&self) -> &ReviewFinding {
        &self.finding
    }
}

/// 一次响应的部分验收结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationAcceptance {
    accepted: Vec<AcceptedTranslation>,
    rejected: Vec<RejectedTranslation>,
    problems: Vec<ResponseProblem>,
    reviews: Vec<TranslationReview>,
    accepted_output_ids: Vec<TaskId>,
}

impl TranslationAcceptance {
    #[cfg(test)]
    pub(crate) fn accepted(&self) -> &[AcceptedTranslation] {
        &self.accepted
    }

    #[cfg(test)]
    pub(crate) fn rejected(&self) -> &[RejectedTranslation] {
        &self.rejected
    }

    #[cfg(test)]
    pub(crate) fn problems(&self) -> &[ResponseProblem] {
        &self.problems
    }

    /// 返回至少有一个目标 Unit 通过验收的模型输出数量。
    #[cfg(test)]
    pub(crate) fn accepted_output_count(&self) -> usize {
        self.accepted_output_ids.len()
    }

    pub(crate) fn accepted_output_ids(&self) -> &[TaskId] {
        &self.accepted_output_ids
    }

    pub(crate) fn append_reviews(&mut self, reviews: &mut Vec<TranslationReview>) {
        self.reviews.append(reviews);
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<AcceptedTranslation>,
        Vec<RejectedTranslation>,
        Vec<ResponseProblem>,
        Vec<TranslationReview>,
    ) {
        (self.accepted, self.rejected, self.problems, self.reviews)
    }
}

pub(crate) const fn generic_language_projection_problem(
    source: &LanguageTextProjectionError,
) -> crate::diagnostic::GenericLanguageProjectionProblem {
    use crate::diagnostic::GenericLanguageProjectionProblem;

    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            GenericLanguageProjectionProblem::TokenIndexConstruction
        }
        LanguageTextProjectionError::EmptyToken => GenericLanguageProjectionProblem::EmptyToken,
        LanguageTextProjectionError::MissingToken { .. } => {
            GenericLanguageProjectionProblem::MissingToken
        }
        LanguageTextProjectionError::RepeatedToken { .. } => {
            GenericLanguageProjectionProblem::RepeatedToken
        }
        LanguageTextProjectionError::OverlappingToken { .. } => {
            GenericLanguageProjectionProblem::OverlappingToken
        }
        LanguageTextProjectionError::ChangedTokenOrder { position, .. } => {
            GenericLanguageProjectionProblem::ChangedTokenOrder {
                position: *position,
            }
        }
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            GenericLanguageProjectionProblem::ChangedSegmentCount {
                expected: *expected,
                actual: *actual,
            }
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            GenericLanguageProjectionProblem::ChangedSegmentKind {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            GenericLanguageProjectionProblem::MissingOrderedToken {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            GenericLanguageProjectionProblem::UnusedOrderedToken
        }
    }
}

pub(crate) const fn generic_placeholder_multiset_problem(
    source: &PlaceholderMultisetError,
) -> GenericPlaceholderMultisetProblem {
    match source {
        PlaceholderMultisetError::Mismatch { .. } => GenericPlaceholderMultisetProblem::Mismatch,
        PlaceholderMultisetError::Unexpected { .. } => {
            GenericPlaceholderMultisetProblem::Unexpected
        }
        PlaceholderMultisetError::OrderMismatch { .. } => {
            GenericPlaceholderMultisetProblem::OrderMismatch
        }
        PlaceholderMultisetError::WrapperTopologyChanged { .. } => {
            GenericPlaceholderMultisetProblem::WrapperTopologyChanged
        }
    }
}

fn generic_response_restore_problem(
    source: &PlaceholderRestoreError,
) -> GenericResponseDestinationProblem {
    match source {
        PlaceholderRestoreError::Projection(source) => {
            GenericResponseDestinationProblem::PlaceholderRestoreProjection {
                problem: generic_language_projection_problem(source),
            }
        }
        PlaceholderRestoreError::Multiset(source) => {
            GenericResponseDestinationProblem::PlaceholderRestoreMultiset {
                problem: generic_placeholder_multiset_problem(source),
            }
        }
    }
}

fn generic_response_placeholder_problem(
    source: &GenericPlaceholderError,
) -> GenericResponseDestinationProblem {
    match source {
        GenericPlaceholderError::InvalidResourceSnapshot(source) => {
            GenericResponseDestinationProblem::InvalidPlaceholderSnapshot {
                category: GenericJsonErrorCategory::from(
                    crate::json_diagnostic::JsonErrorCategory::from(source),
                ),
                line: source.line(),
                column: source.column(),
            }
        }
        GenericPlaceholderError::Compilation(source) => {
            GenericResponseDestinationProblem::PlaceholderCompilation {
                problem: source.diagnostic_problem(),
            }
        }
        GenericPlaceholderError::Protection(source) => {
            GenericResponseDestinationProblem::PlaceholderProtection {
                problem: source.diagnostic_issue(),
            }
        }
        GenericPlaceholderError::Restore(source) => generic_response_restore_problem(source),
        GenericPlaceholderError::ManualTranslationMismatch => {
            GenericResponseDestinationProblem::PlaceholderBindingMismatch
        }
    }
}

fn generic_candidate_placeholder_problem(
    source: GenericPlaceholderError,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> Result<GenericResponseDestinationProblem, GenericPreparationError> {
    match source {
        GenericPlaceholderError::Protection(
            source @ (PlaceholderProtectionError::StartWorker { .. }
            | PlaceholderProtectionError::Match { .. }),
        ) => Err(GenericPreparationError::PlaceholderProtection {
            rule_source: rule_source.clone(),
            locator: locator.clone(),
            source,
        }),
        source => Ok(generic_response_placeholder_problem(&source)),
    }
}

fn generic_current_translation_protection_result(
    protected: Result<GenericProtectedText, GenericPlaceholderError>,
    rule_source: &GenericPlaceholderRuleSource,
    locator: &GenericUnitLocator,
) -> Result<Option<GenericProtectedText>, GenericPreparationError> {
    match protected {
        Ok(protected) => Ok(Some(protected)),
        Err(source) => match generic_candidate_placeholder_problem(source, rule_source, locator) {
            Ok(_) => Ok(None),
            Err(source) => Err(source),
        },
    }
}

pub(crate) fn accept_generic_response_with_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPreparationError> {
    accept_generic_response_with_validator_and_cancellation(
        task,
        parsed,
        facts,
        |fact, candidate| {
            validate_generic_candidate_fact_with_cancellation(
                fact,
                candidate,
                placeholder_rules,
                placeholder_rule_source,
                language_module,
                cancellation,
            )
        },
        cancellation,
    )
}

fn accept_generic_response_with_validator_and_cancellation(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    facts: &GenericUnitMap<GenericValidationFact>,
    mut validator: impl FnMut(
        &GenericValidationFact,
        &str,
    ) -> Result<
        Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
        GenericPreparationError,
    >,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationAcceptance, GenericPreparationError> {
    let mut cache = HashMap::<
        TaskId,
        CancellableTextMap<
            &str,
            Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
        >,
    >::new();
    let mut reviews = Vec::new();
    let mut acceptance =
        accept_parsed_response_with_cancellation(
            task,
            parsed,
            |output_id,
             key,
             candidate|
             -> Result<
                Result<String, GenericResponseDestinationProblem>,
                GenericPreparationError,
            > {
                ensure_generic_response_processing_running(cancellation)?;
                let Some(fact) = facts.get_with_cancellation(key, || {
                    ensure_generic_response_processing_running(cancellation)
                })?
                else {
                    return Ok(Err(GenericResponseDestinationProblem::MissingPlanningFact));
                };
                let output_cache = cache
                    .entry(output_id)
                    .or_insert_with(|| CancellableTextMap::with_capacity(1));
                let validated = if let Some(cached) = output_cache
                    .get_with_cancellation(fact.kind.as_str(), || {
                        ensure_generic_response_processing_running(cancellation)
                    })? {
                    clone_generic_validation_result(cached, cancellation)?
                } else {
                    // 一个 output_id 只对应一个全局去重族；同族的原文、保护后文本和实际
                    // Placeholder 绑定相同。kind 仍会改变 scope，因此必须分别验收。
                    let validated = validator(fact, candidate)?;
                    let returned = clone_generic_validation_result(&validated, cancellation)?;
                    let previous = output_cache.insert_with_cancellation(
                        fact.kind.as_str(),
                        validated,
                        || ensure_generic_response_processing_running(cancellation),
                    )?;
                    debug_assert!(previous.is_none());
                    returned
                };
                match validated {
                    Ok(validated) => {
                        let (translation, findings) = validated.into_parts();
                        for finding in findings {
                            reviews.push(TranslationReview::new(
                                output_id,
                                GenericPlanningUnitLocator::new(
                                    fact.locator.relative_path(),
                                    fact.locator.group_id(),
                                    fact.locator.unit_id(),
                                    fact.locator.role(),
                                )
                                .with_natural_position(
                                    fact.locator.natural_position().0,
                                    fact.locator.natural_position().1,
                                ),
                                finding,
                            ));
                        }
                        Ok(Ok(translation))
                    }
                    Err(problem) => Ok(Err(problem)),
                }
            },
            || cancellation.is_requested(),
        )?;
    acceptance.append_reviews(&mut reviews);
    Ok(acceptance)
}

#[cfg(test)]
fn validate_generic_candidate(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<ValidatedCandidate<String>, GenericResponseDestinationProblem> {
    let fact = facts
        .get_with_cancellation(key, || Ok::<_, std::convert::Infallible>(()))
        .unwrap_or_else(|never| match never {})
        .ok_or(GenericResponseDestinationProblem::MissingPlanningFact)?;
    validate_generic_candidate_fact(fact, candidate, placeholder_rules, language_module)
}

fn validate_generic_reuse_with_cancellation(
    key: &GenericUnitKey,
    candidate: &str,
    facts: &GenericUnitMap<GenericValidationFact>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<Result<ValidatedReuse, GenericResponseDestinationProblem>, GenericPreparationError> {
    ensure_generic_response_processing_running(cancellation)?;
    let Some(fact) = facts.get_with_cancellation(key, || {
        ensure_generic_response_processing_running(cancellation)
    })?
    else {
        return Ok(Err(GenericResponseDestinationProblem::MissingPlanningFact));
    };
    let final_translation = match validate_generic_candidate_fact_with_cancellation(
        fact,
        candidate,
        placeholder_rules,
        placeholder_rule_source,
        language_module,
        cancellation,
    )? {
        Ok(translation) => translation.into_parts().0,
        Err(problem) => return Ok(Err(problem)),
    };
    let service = GenericPlaceholderService::default();
    let target_id = fact.locator.readable_id();
    let context = match service.bind_target_candidate_with_cancellation(
        &fact.protected,
        &target_id,
        &fact.kind,
        &final_translation,
        placeholder_rules,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(context) => context,
        Err(source) => {
            return Ok(Err(generic_candidate_placeholder_problem(
                source.into(),
                placeholder_rule_source,
                &fact.locator,
            )?));
        }
    };
    let context_binding = context.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    let expected_binding = fact.protected.binding_fingerprint_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })?;
    if context_binding != expected_binding {
        return Ok(Err(
            GenericResponseDestinationProblem::PlaceholderBindingMismatch,
        ));
    }
    let mut context_text = String::with_capacity(context.text().len());
    append_generic_response_text(&mut context_text, context.text(), cancellation)?;
    Ok(Ok(ValidatedReuse::new(final_translation, context_text)))
}

#[cfg(test)]
fn validate_generic_candidate_fact(
    fact: &GenericValidationFact,
    candidate: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    language_module: &dyn LanguageModule,
) -> Result<ValidatedCandidate<String>, GenericResponseDestinationProblem> {
    validate_generic_candidate_fact_with_cancellation(
        fact,
        candidate,
        placeholder_rules,
        &GenericPlaceholderRuleSource::ProjectSnapshot,
        language_module,
        &CooperativeCancellation::default(),
    )
    .expect("不取消的候选验收必须完成")
}

fn validate_generic_candidate_fact_with_cancellation(
    fact: &GenericValidationFact,
    candidate: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    placeholder_rule_source: &GenericPlaceholderRuleSource,
    language_module: &dyn LanguageModule,
    cancellation: &CooperativeCancellation,
) -> Result<
    Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    GenericPreparationError,
> {
    ensure_generic_response_processing_running(cancellation)?;
    let service = GenericPlaceholderService::default();
    let restored = match service.restore_with_cancellation(&fact.protected, candidate, || {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(restored) => restored,
        Err(source) => return Ok(Err(generic_response_placeholder_problem(&source))),
    };
    let target_id = fact.locator.readable_id();
    let candidate_protected = match service.bind_target_candidate_with_cancellation(
        &fact.protected,
        &target_id,
        &fact.kind,
        &restored,
        placeholder_rules,
        || ensure_generic_response_processing_running(cancellation),
    )? {
        Ok(protected) => protected,
        Err(source) => {
            return Ok(Err(generic_candidate_placeholder_problem(
                source.into(),
                placeholder_rule_source,
                &fact.locator,
            )?));
        }
    };
    let language_text = match candidate_protected.language_text_with_cancellation(|| {
        ensure_generic_response_processing_running(cancellation)
    })? {
        Ok(text) => text,
        Err(source) => {
            return Ok(Err(GenericResponseDestinationProblem::LanguageProjection {
                problem: generic_language_projection_problem(&source),
            }));
        }
    };
    let residual = match language_module.find_source_residual_with_cancellation(
        &fact.analysis,
        &language_text,
        &mut || ensure_generic_language_running(cancellation),
    ) {
        Ok(Ok(residual)) => residual,
        Ok(Err(_)) => {
            return Ok(Err(
                GenericResponseDestinationProblem::LanguageAnalysisMismatch,
            ));
        }
        Err(LanguageOperationCancelled) => return Err(GenericPlanningError::Cancelled.into()),
    };
    let review = residual.is_some().then_some(ReviewFinding::SourceResidual);
    ensure_generic_response_processing_running(cancellation)?;
    let final_translation = match rebuild_original_placeholders_with_cancellation(
        &candidate_protected,
        &language_text,
        cancellation,
    )? {
        Ok(translation) => translation,
        Err(problem) => return Ok(Err(problem)),
    };
    if contains_reserved_prefix_with_cancellation(&final_translation, cancellation)? {
        return Ok(Err(GenericResponseDestinationProblem::ReservedToken));
    }
    ensure_generic_response_processing_running(cancellation)?;
    Ok(Ok(match review {
        Some(finding) => ValidatedCandidate::with_review(final_translation, finding),
        None => ValidatedCandidate::clean(final_translation),
    }))
}

fn rebuild_original_placeholders_with_cancellation(
    protected: &GenericProtectedText,
    repaired: &LanguageText,
    cancellation: &CooperativeCancellation,
) -> Result<Result<String, GenericResponseDestinationProblem>, GenericPlanningError> {
    ensure_generic_response_processing_running(cancellation)?;
    let mut output = String::new();
    let mut placeholders = protected.placeholders().iter();
    for segment in repaired.segments() {
        ensure_generic_response_processing_running(cancellation)?;
        match segment {
            LanguageTextSegment::NaturalText(text) => {
                append_generic_response_text(&mut output, text, cancellation)?;
            }
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return Ok(Err(
                        GenericResponseDestinationProblem::PlaceholderBoundaryAdded,
                    ));
                };
                append_generic_response_text(&mut output, placeholder.original(), cancellation)?;
            }
        }
    }
    ensure_generic_response_processing_running(cancellation)?;
    if placeholders.next().is_some() {
        return Ok(Err(
            GenericResponseDestinationProblem::PlaceholderBoundaryRemoved,
        ));
    }
    Ok(Ok(output))
}

pub(crate) fn ensure_generic_response_processing_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    if cancellation.is_requested() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

fn append_generic_response_text(
    output: &mut String,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < text.len() {
        ensure_generic_response_processing_running(cancellation)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_generic_response_processing_running(cancellation)
}

fn clone_generic_validation_result(
    result: &Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    cancellation: &CooperativeCancellation,
) -> Result<
    Result<ValidatedCandidate<String>, GenericResponseDestinationProblem>,
    GenericPlanningError,
> {
    let mut cloned = String::new();
    match result {
        Ok(value) => {
            append_generic_response_text(&mut cloned, value.value(), cancellation)?;
            let cloned = match value.reviews() {
                [] => ValidatedCandidate::clean(cloned),
                [finding] => ValidatedCandidate::with_review(cloned, finding.clone()),
                _ => unreachable!("当前候选验收每个目标最多产生一个 Review"),
            };
            Ok(Ok(cloned))
        }
        Err(problem) => {
            ensure_generic_response_processing_running(cancellation)?;
            Ok(Err(problem.clone()))
        }
    }
}

fn contains_reserved_prefix_with_cancellation(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let prefix = placeholder_token::PREFIX.as_bytes();

    for (index, window) in text.as_bytes().windows(prefix.len()).enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_generic_response_processing_running(cancellation)?;
        }
        if window == prefix {
            return Ok(true);
        }
    }
    ensure_generic_response_processing_running(cancellation)?;
    Ok(false)
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
            "Generic 模型响应不符合翻译响应协议：{}，第 {} 行、第 {} 列",
            self.source.kind().code(),
            self.source.line(),
            self.source.column(),
        )
    }
}

#[cfg(test)]
impl Error for GenericResponseError {}

/// 验收 Generic 的逐 ID 字符串数组响应。
///
/// `validator` 负责 Placeholder 恢复、语言残留检查和可选安全修复。它返回最终应
/// 保存的译文，或者只影响当前 ID 的具体原因。
#[cfg(test)]
pub(crate) fn accept_response(
    task: &PlannedTask,
    assistant_response: &str,
    response_mode: TranslationResponseMode,
    validator: impl FnMut(
        TaskId,
        &GenericUnitKey,
        &str,
    ) -> Result<String, GenericResponseDestinationProblem>,
) -> Result<TranslationAcceptance, GenericResponseError> {
    let parsed = parse_translation_response(assistant_response, response_mode)
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
    mut validator: impl FnMut(
        TaskId,
        &GenericUnitKey,
        &str,
    ) -> Result<String, GenericResponseDestinationProblem>,
) -> TranslationAcceptance {
    accept_parsed_response_with_cancellation(
        task,
        parsed,
        |output_id, key, candidate| {
            Ok::<_, GenericPlanningError>(validator(output_id, key, candidate))
        },
        || false,
    )
    .expect("不取消的受信响应验收必须完成")
}

pub(crate) fn accept_parsed_response_with_cancellation<E>(
    task: PlannedTask,
    parsed: &ParsedTranslationResponse,
    mut validator: impl FnMut(
        TaskId,
        &GenericUnitKey,
        &str,
    ) -> Result<Result<String, GenericResponseDestinationProblem>, E>,
    is_cancelled: impl Fn() -> bool,
) -> Result<TranslationAcceptance, E>
where
    E: From<GenericPlanningError>,
{
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
    let mut rejected = Vec::new();
    let mut problems = Vec::new();
    let mut accepted_output_ids = Vec::new();
    let mut observed = HashSet::new();
    let mut reported_duplicates = HashSet::new();
    let mut outputs = task.outputs;
    for (item_index, entry) in entries.iter().enumerate() {
        ensure_planning_not_cancelled(&is_cancelled)?;
        let Some(output_id) = entry.canonical_id() else {
            problems.push(ResponseProblem::InvalidId { item_index });
            continue;
        };
        if !outputs.contains_key(&output_id) {
            problems.push(ResponseProblem::UnexpectedId {
                output_id: response_output_id(output_id),
            });
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
                problems.push(ResponseProblem::DuplicateId {
                    output_id: response_output_id(output_id),
                });
            }
            continue;
        }
        let decoded = entry.decode_translation_value_with_cancellation(|| {
            ensure_planning_not_cancelled(&is_cancelled)
        })?;
        let candidate = match generic_translation_candidate(decoded, &is_cancelled)? {
            Ok(candidate) => candidate,
            Err(problem) => {
                let destinations = std::mem::take(
                    outputs
                        .get_mut(&output_id)
                        .expect("已确认的模型输出必须仍属于当前 Generic 任务"),
                );
                let candidate_json =
                    clone_planning_text_with_cancellation(entry.raw_value().get(), &is_cancelled)?;
                for destination in destinations {
                    rejected.push(rejected_generic_destination(
                        destination,
                        candidate_json.clone(),
                        None,
                        ProvenInvariantViolation::InvalidCandidateShape,
                    ));
                }
                problems.push(ResponseProblem::InvalidValue {
                    output_id: response_output_id(output_id),
                    problem,
                });
                continue;
            }
        };
        let destinations = std::mem::take(
            outputs
                .get_mut(&output_id)
                .expect("已确认的模型输出必须仍属于当前 Generic 任务"),
        );
        let response_line_problem =
            validate_response_lines_with_cancellation(&candidate.lines, &is_cancelled)?.err();
        let candidate_problem = match response_line_problem {
            Some(problem) => Some(problem),
            None => {
                validate_candidate_text_with_cancellation(&candidate.text, &is_cancelled)?.err()
            }
        };
        if let Some(problem) = candidate_problem {
            let violation = generic_text_problem_violation(problem, &candidate.lines);
            for destination in destinations {
                rejected.push(rejected_generic_destination(
                    destination,
                    candidate.candidate_json.clone(),
                    Some(candidate.lines.clone()),
                    violation.clone(),
                ));
            }
            problems.push(ResponseProblem::InvalidTranslation {
                output_id: response_output_id(output_id),
                problem,
            });
            continue;
        }
        let mut output_accepted = false;
        for destination in destinations {
            ensure_planning_not_cancelled(&is_cancelled)?;
            let validated_text = match validator(output_id, &destination.key, &candidate.text)? {
                Ok(candidate) => candidate,
                Err(problem) => {
                    if let Some(violation) = generic_destination_violation(&problem) {
                        rejected.push(rejected_generic_destination(
                            destination.clone(),
                            candidate.candidate_json.clone(),
                            Some(candidate.lines.clone()),
                            violation,
                        ));
                    }
                    problems.push(ResponseProblem::InvalidDestination {
                        output_id: response_output_id(output_id),
                        destination: diagnostic_response_locator(&destination.locator),
                        problem,
                    });
                    continue;
                }
            };
            ensure_planning_not_cancelled(&is_cancelled)?;
            if let Err(problem) =
                validate_candidate_text_with_cancellation(&validated_text, &is_cancelled)?
            {
                let rejected_lines = validated_text
                    .split('\n')
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                rejected.push(rejected_generic_destination(
                    destination.clone(),
                    serde_json::to_string(&rejected_lines)
                        .expect("Generic 候选字符串数组必须可以编码"),
                    Some(rejected_lines.clone()),
                    generic_text_problem_violation(problem, &rejected_lines),
                ));
                problems.push(ResponseProblem::InvalidDestination {
                    output_id: response_output_id(output_id),
                    destination: diagnostic_response_locator(&destination.locator),
                    problem: GenericResponseDestinationProblem::InvalidTranslation { problem },
                });
                continue;
            }
            accepted.push(AcceptedTranslation {
                key: destination.key,
                translation: validated_text,
                expected_source_text: destination.expected_source_text,
                expected_group_context: destination.expected_group_context,
                expected_state_fingerprint: destination.expected_state_fingerprint,
                expected_previous: destination.expected_previous,
                was_current_rejected: destination.was_current_rejected,
            });
            output_accepted = true;
        }
        if output_accepted {
            accepted_output_ids.push(output_id);
        }
    }
    for output_id in outputs.keys() {
        ensure_planning_not_cancelled(&is_cancelled)?;
        if !observed.contains(output_id) {
            problems.push(ResponseProblem::MissingId {
                output_id: response_output_id(*output_id),
            });
        }
    }
    ensure_planning_not_cancelled(&is_cancelled)?;
    Ok(TranslationAcceptance {
        accepted,
        rejected,
        problems,
        reviews: Vec::new(),
        accepted_output_ids,
    })
}

fn response_output_id(output_id: TaskId) -> u64 {
    u64::try_from(output_id.get()).expect("当前平台 usize 必须能够无损表示为 u64")
}

fn diagnostic_response_locator(
    locator: &GenericPlanningUnitLocator,
) -> DiagnosticGenericUnitLocator {
    let diagnostic = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    );
    match locator.natural_position() {
        Some((line, unit)) => diagnostic.with_natural_position(line, unit),
        None => diagnostic,
    }
}

fn rejected_generic_destination(
    destination: PlannedDestination,
    candidate_json: String,
    translation: Option<Vec<String>>,
    violation: ProvenInvariantViolation,
) -> RejectedTranslation {
    RejectedTranslation {
        key: destination.key,
        locator: destination.locator,
        origin: TranslationOrigin::Automatic,
        candidate_json,
        translation,
        expected_source_text: destination.expected_source_text,
        source: destination.expected_source,
        expected_group_context: destination.expected_group_context,
        violation,
        planning_state: destination.expected_state_fingerprint,
        source_language: destination.source_language,
        target_language: destination.target_language,
        expected_previous: destination.expected_previous,
        was_current_rejected: destination.was_current_rejected,
    }
}

fn generic_text_problem_violation(
    problem: GenericResponseTextProblem,
    translation: &[String],
) -> ProvenInvariantViolation {
    match problem {
        GenericResponseTextProblem::Blank => ProvenInvariantViolation::BlankTranslation,
        GenericResponseTextProblem::CarriageReturn => ProvenInvariantViolation::InvalidLineText {
            line_index: translation
                .iter()
                .position(|line| line.contains('\r'))
                .unwrap_or_default(),
        },
        GenericResponseTextProblem::LineFeed => ProvenInvariantViolation::InvalidLineText {
            line_index: translation
                .iter()
                .position(|line| line.contains('\n'))
                .unwrap_or_default(),
        },
        GenericResponseTextProblem::Nul => ProvenInvariantViolation::InvalidLineText {
            line_index: translation
                .iter()
                .position(|line| line.contains('\0'))
                .unwrap_or_default(),
        },
        GenericResponseTextProblem::ByteOrderMark => {
            ProvenInvariantViolation::ContainsByteOrderMark {
                line_index: translation
                    .iter()
                    .position(|line| line.contains('\u{feff}'))
                    .unwrap_or_default(),
            }
        }
    }
}

fn generic_destination_violation(
    problem: &GenericResponseDestinationProblem,
) -> Option<ProvenInvariantViolation> {
    match problem {
        GenericResponseDestinationProblem::PlaceholderRestoreMultiset { problem } => {
            Some(match problem {
                GenericPlaceholderMultisetProblem::Unexpected => {
                    ProvenInvariantViolation::UnexpectedPlaceholderToken
                }
                GenericPlaceholderMultisetProblem::Mismatch
                | GenericPlaceholderMultisetProblem::OrderMismatch => {
                    ProvenInvariantViolation::PlaceholderMismatch
                }
                GenericPlaceholderMultisetProblem::WrapperTopologyChanged => {
                    ProvenInvariantViolation::PlaceholderBoundaryChanged
                }
            })
        }
        GenericResponseDestinationProblem::PlaceholderProtection { .. }
        | GenericResponseDestinationProblem::PlaceholderBindingMismatch => {
            Some(ProvenInvariantViolation::PlaceholderMismatch)
        }
        GenericResponseDestinationProblem::PlaceholderRestoreProjection { .. }
        | GenericResponseDestinationProblem::LanguageProjection { .. }
        | GenericResponseDestinationProblem::PlaceholderBoundaryAdded
        | GenericResponseDestinationProblem::PlaceholderBoundaryRemoved => {
            Some(ProvenInvariantViolation::PlaceholderBoundaryChanged)
        }
        GenericResponseDestinationProblem::ReservedToken => {
            Some(ProvenInvariantViolation::ReservedPlaceholderToken)
        }
        GenericResponseDestinationProblem::InvalidTranslation { problem } => {
            Some(generic_text_problem_violation(*problem, &[]))
        }
        GenericResponseDestinationProblem::MissingPlanningFact
        | GenericResponseDestinationProblem::InvalidPlaceholderSnapshot { .. }
        | GenericResponseDestinationProblem::PlaceholderCompilation { .. }
        | GenericResponseDestinationProblem::LanguageAnalysisMismatch
        | GenericResponseDestinationProblem::RepairPlanningMismatch
        | GenericResponseDestinationProblem::RepairApplication { .. } => None,
    }
}

struct GenericTranslationCandidate {
    lines: Vec<String>,
    text: String,
    candidate_json: String,
}

fn generic_translation_candidate(
    decoded: DecodedTranslationAssistantValue,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Result<GenericTranslationCandidate, GenericResponseValueProblem>, GenericPlanningError>
{
    let translation = match decoded {
        DecodedTranslationAssistantValue::Translation(translation) => translation,
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
            source,
            translation,
        }) => {
            if let Err(problem) = validate_response_string_array_shape(source, true) {
                return Ok(Err(problem));
            }
            translation
        }
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::NotObject) => {
            return Ok(Err(GenericResponseValueProblem::SourceEchoNotObject));
        }
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
            error,
        )) => return Ok(Err(source_echo_fields_problem(error))),
    };

    let lines = match translation {
        DecodedJsonStringArray::Strings(lines) => lines,
        invalid => {
            return Ok(Err(response_string_array_shape_problem(invalid, false)));
        }
    };
    let text = join_translation_lines_with_cancellation(&lines, is_cancelled)?;
    let candidate_json = serde_json::to_string(&lines).expect("Generic 候选字符串数组必须可以编码");
    Ok(Ok(GenericTranslationCandidate {
        lines,
        text,
        candidate_json,
    }))
}

fn validate_response_string_array_shape(
    value: DecodedJsonStringArray,
    source_field: bool,
) -> Result<(), GenericResponseValueProblem> {
    match value {
        DecodedJsonStringArray::Strings(_) => Ok(()),
        invalid => Err(response_string_array_shape_problem(invalid, source_field)),
    }
}

fn response_string_array_shape_problem(
    value: DecodedJsonStringArray,
    source_field: bool,
) -> GenericResponseValueProblem {
    match (source_field, value) {
        (true, DecodedJsonStringArray::NotArray) => GenericResponseValueProblem::SourceNotArray,
        (true, DecodedJsonStringArray::NonStringItem { item }) => {
            GenericResponseValueProblem::SourceNonStringItem { item }
        }
        (false, DecodedJsonStringArray::NotArray) => {
            GenericResponseValueProblem::TranslationNotArray
        }
        (false, DecodedJsonStringArray::NonStringItem { item }) => {
            GenericResponseValueProblem::TranslationNonStringItem { item }
        }
        (_, DecodedJsonStringArray::Strings(_)) => {
            unreachable!("字符串数组不应进入形状错误分支")
        }
    }
}

fn source_echo_fields_problem(error: DecodedSourceEchoFieldsError) -> GenericResponseValueProblem {
    match error {
        DecodedSourceEchoFieldsError::MissingSource => {
            GenericResponseValueProblem::SourceEchoMissingSource
        }
        DecodedSourceEchoFieldsError::MissingTranslation => {
            GenericResponseValueProblem::SourceEchoMissingTranslation
        }
        DecodedSourceEchoFieldsError::DuplicateSource => {
            GenericResponseValueProblem::SourceEchoDuplicateSource
        }
        DecodedSourceEchoFieldsError::DuplicateTranslation => {
            GenericResponseValueProblem::SourceEchoDuplicateTranslation
        }
        DecodedSourceEchoFieldsError::UnexpectedField { .. } => {
            GenericResponseValueProblem::SourceEchoUnexpectedField
        }
    }
}

fn join_translation_lines_with_cancellation(
    lines: &[String],
    is_cancelled: &impl Fn() -> bool,
) -> Result<String, GenericPlanningError> {
    let capacity = lines.iter().try_fold(0_usize, |capacity, line| {
        capacity
            .checked_add(line.len())
            .and_then(|capacity| capacity.checked_add(1))
            .ok_or(TaskPlanningError::CharacterCountOverflow)
    })?;
    let mut translation = String::with_capacity(capacity.saturating_sub(1));
    for (index, line) in lines.iter().enumerate() {
        ensure_planning_not_cancelled(is_cancelled)?;
        if index != 0 {
            translation.push('\n');
        }
        translation.push_str(&clone_planning_text_with_cancellation(line, is_cancelled)?);
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(translation)
}

fn validate_response_lines_with_cancellation(
    lines: &[String],
    is_cancelled: &impl Fn() -> bool,
) -> Result<Result<(), GenericResponseTextProblem>, GenericPlanningError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    // 数组项本身就是模型协议的分行边界；必须在 ATT 插入项间 LF 之前校验每项。
    for line in lines {
        let mut next_check = 0_usize;
        for (offset, character) in line.char_indices() {
            if offset >= next_check {
                ensure_planning_not_cancelled(is_cancelled)?;
                next_check = offset.saturating_add(CANCELLATION_CHECK_BYTES);
            }
            let problem = match character {
                '\r' => Some(GenericResponseTextProblem::CarriageReturn),
                '\n' => Some(GenericResponseTextProblem::LineFeed),
                '\0' => Some(GenericResponseTextProblem::Nul),
                _ => None,
            };
            if let Some(problem) = problem {
                return Ok(Err(problem));
            }
        }
    }
    ensure_planning_not_cancelled(is_cancelled)?;
    Ok(Ok(()))
}

fn validate_candidate_text_with_cancellation(
    candidate: &str,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Result<(), GenericResponseTextProblem>, GenericPlanningError> {
    Ok(
        validate_reflowed_candidate_text_with_cancellation(candidate, || {
            ensure_planning_not_cancelled(is_cancelled)
        })?
        .map_err(|violation| match violation {
            ProvenInvariantViolation::BlankTranslation => GenericResponseTextProblem::Blank,
            ProvenInvariantViolation::InvalidLineText { .. } => {
                if candidate.contains('\r') {
                    GenericResponseTextProblem::CarriageReturn
                } else {
                    GenericResponseTextProblem::Nul
                }
            }
            ProvenInvariantViolation::ContainsByteOrderMark { .. } => {
                GenericResponseTextProblem::ByteOrderMark
            }
            ProvenInvariantViolation::LineCountMismatch { .. }
            | ProvenInvariantViolation::FixedBlankSlotChanged { .. }
            | ProvenInvariantViolation::FixedNonBlankSlotEmptied { .. }
            | ProvenInvariantViolation::PlaceholderMismatch
            | ProvenInvariantViolation::UnexpectedPlaceholderToken
            | ProvenInvariantViolation::PlaceholderBoundaryChanged
            | ProvenInvariantViolation::ReservedPlaceholderToken
            | ProvenInvariantViolation::InvalidCandidateShape => {
                unreachable!("自由文本校验不会产生固定槽或 Placeholder 违反项")
            }
        }),
    )
}

/// 建立自动译文 Current 所需的完整语义状态。
#[cfg(test)]
pub(crate) fn automatic_translation_state_fingerprint(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
) -> Sha256Fingerprint {
    crate::generic::applicability::generic_automatic_applicability(
        language_pair.source().as_str(),
        language_pair.target().as_str(),
        key.group_id(),
        key.unit_id(),
        source_text,
        group_context,
    )
}

fn automatic_translation_state_fingerprint_with_cancellation(
    language_pair: &LanguagePair,
    key: &GenericUnitKey,
    source_text: &str,
    group_context: Sha256Fingerprint,
    cancellation: &CooperativeCancellation,
) -> Result<Sha256Fingerprint, GenericPlanningError> {
    crate::generic::applicability::generic_automatic_applicability_with_cancellation(
        language_pair.source().as_str(),
        language_pair.target().as_str(),
        key.group_id(),
        key.unit_id(),
        source_text,
        group_context,
        || ensure_translation_not_cancelled(cancellation),
    )
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
    use std::io;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use crate::diagnostic::{
        Diagnostic, DiagnosticReport, GenericDiagnosticStage, GenericIssue, StateEffect,
        render_diagnostic_fields,
    };
    use crate::generic::GenericPlaceholderRuleDefinition;
    use crate::i18n::{UiLocale, UiLocalizer};
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguagePair,
    };
    use crate::project_name::ProjectName;
    use crate::translation::placeholder::PlaceholderWorkerOperation;

    use super::*;
    use crate::generic::project::{
        GenericProject, GenericStoredFile, GenericStoredGroup, GenericStoredRejectedTranslation,
        GenericStoredSnapshot, GenericStoredTranslation, GenericStoredUnit,
    };

    fn fingerprint(value: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([value; 32])
    }

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    fn unit_locator() -> GenericUnitLocator {
        GenericUnitLocator::new("story.jsonl", "story", "line", "dialogue", 1, 1)
    }

    #[test]
    fn placeholder_multiset_diagnostics_preserve_every_failure_kind() {
        for (source, expected) in [
            (
                PlaceholderMultisetError::Mismatch {
                    token: "secret-missing-token".to_owned(),
                },
                GenericPlaceholderMultisetProblem::Mismatch,
            ),
            (
                PlaceholderMultisetError::Unexpected {
                    token: "secret-unexpected-token".to_owned(),
                },
                GenericPlaceholderMultisetProblem::Unexpected,
            ),
            (
                PlaceholderMultisetError::OrderMismatch {
                    expected_token: "secret-expected-token".to_owned(),
                    actual_token: "secret-actual-token".to_owned(),
                },
                GenericPlaceholderMultisetProblem::OrderMismatch,
            ),
            (
                PlaceholderMultisetError::WrapperTopologyChanged {
                    token: "secret-wrapper-token".to_owned(),
                },
                GenericPlaceholderMultisetProblem::WrapperTopologyChanged,
            ),
        ] {
            assert_eq!(generic_placeholder_multiset_problem(&source), expected);
        }
    }

    fn japanese_language_module() -> JapaneseLanguageModule {
        JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
        )
    }

    #[test]
    fn technical_placeholder_failures_leave_candidate_and_current_fallback_paths() {
        let worker_failure = || {
            GenericPlaceholderError::Protection(PlaceholderProtectionError::StartWorker {
                operation: PlaceholderWorkerOperation::MatchText,
                source: io::Error::other("worker unavailable"),
            })
        };
        let candidate = generic_candidate_placeholder_problem(
            worker_failure(),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &unit_locator(),
        )
        .expect_err("worker 启动失败必须离开普通候选不合格分支");
        assert!(matches!(
            candidate,
            GenericPreparationError::PlaceholderProtection {
                source: PlaceholderProtectionError::StartWorker { .. },
                ..
            }
        ));

        let current = generic_current_translation_protection_result(
            Err(worker_failure()),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &unit_locator(),
        )
        .expect_err("已有译文的 worker 启动失败必须终止规划");
        assert!(matches!(
            current,
            GenericPreparationError::PlaceholderProtection {
                source: PlaceholderProtectionError::StartWorker { .. },
                ..
            }
        ));

        assert_eq!(
            generic_candidate_placeholder_problem(
                GenericPlaceholderError::ManualTranslationMismatch,
                &GenericPlaceholderRuleSource::ProjectSnapshot,
                &unit_locator(),
            )
            .expect("候选 Placeholder 绑定不匹配仍应成为逐目标问题"),
            GenericResponseDestinationProblem::PlaceholderBindingMismatch
        );
        let fallback = generic_current_translation_protection_result(
            Err(GenericPlaceholderError::Protection(
                PlaceholderProtectionError::ReservedTokenNamespace {
                    start_byte: 0,
                    end_byte: 8,
                },
            )),
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            &unit_locator(),
        )
        .expect("已有译文的数据不合格仍应使用保护后原文");
        assert!(fallback.is_none());
    }

    #[test]
    fn candidate_validation_restores_placeholders_and_allows_free_line_breaks() {
        let service = GenericPlaceholderService::default();
        let rules = service
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let protected = service
            .protect("dialogue", "こんにちは {name}", &rules)
            .expect("原文应该可保护");
        let token = protected.placeholders()[0].token().to_owned();
        let language_text = protected.language_text().expect("保护文本应该可投影");
        let language_module = japanese_language_module();
        let key = GenericUnitKey::new("group".to_owned(), "unit".to_owned());
        let mut facts = GenericUnitMap::new();
        let previous = facts
            .insert_with_cancellation(
                key.clone(),
                GenericValidationFact {
                    locator: GenericUnitLocator::new(
                        "scene.jsonl",
                        "group",
                        "unit",
                        "dialogue",
                        1,
                        1,
                    ),
                    kind: "dialogue".to_owned(),
                    analysis: language_module.analyze_source(&language_text),
                    protected,
                },
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        assert!(previous.is_none());

        assert_eq!(
            validate_generic_candidate(
                &key,
                &format!("你好\n世界 {token}"),
                &facts,
                &rules,
                &language_module,
            )
            .expect("合法译文应该通过验收")
            .into_parts()
            .0,
            "你好\n世界 {name}"
        );
        assert!(
            validate_generic_candidate(&key, "你好", &facts, &rules, &language_module).is_err(),
            "丢失 Placeholder 的译文必须被拒绝"
        );
        assert!(
            validate_generic_candidate(
                &key,
                &format!("你好 {token} {{invented}}"),
                &facts,
                &rules,
                &language_module,
            )
            .is_err(),
            "新增原文不存在的 Placeholder 必须被拒绝"
        );
        let residual = validate_generic_candidate(
            &key,
            &format!("こんにちは {token}"),
            &facts,
            &rules,
            &language_module,
        )
        .expect("源语言残留只进入 Review，不应丢弃合法候选");
        assert_eq!(residual.value(), "こんにちは {name}");
        assert_eq!(residual.reviews(), &[ReviewFinding::SourceResidual]);
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
                            rejected: None,
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

    fn cross_kind_snapshot(current: Option<&str>) -> GenericStoredSnapshot {
        let groups = [
            ("source", "dialogue", current),
            ("source-2", "dialogue", None),
            ("target", "name", None),
        ]
        .into_iter()
        .enumerate()
        .map(
            |(ordinal, (group_id, kind, translation))| GenericStoredGroup {
                id: group_id.to_owned(),
                ordinal,
                kind: kind.to_owned(),
                context_fingerprint: fingerprint(40 + ordinal as u8),
                units: vec![GenericStoredUnit {
                    id: "unit".to_owned(),
                    ordinal: 0,
                    source_text: "こんにちは".to_owned(),
                    translation: translation.map(|translation| GenericStoredTranslation {
                        translation: translation.to_owned(),
                        origin: TranslationOrigin::Manual,
                        state_fingerprint: fingerprint(90),
                    }),
                    rejected: None,
                }],
            },
        )
        .collect();
        GenericStoredSnapshot {
            project: stored_snapshot().project,
            files: vec![GenericStoredFile {
                relative_path: PathBuf::from("scene.jsonl"),
                ordinal: 0,
                groups,
            }],
        }
    }

    #[test]
    fn current_translation_keeps_source_bound_placeholders_after_context_translation() {
        let rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                None,
                r"(?<=名前: )[A-Za-z0-9-]+",
            )])
            .unwrap();
        for origin in [TranslationOrigin::Manual, TranslationOrigin::Automatic] {
            let mut snapshot = cross_kind_snapshot(None);
            snapshot.files[0].groups.truncate(1);
            let group = &mut snapshot.files[0].groups[0];
            group.units[0].source_text = "名前: abc-123".to_owned();
            group.units[0].translation = Some(GenericStoredTranslation {
                translation: "名称：abc-123".to_owned(),
                origin,
                state_fingerprint: automatic_translation_state_fingerprint(
                    snapshot.project.language_pair(),
                    &GenericUnitKey::new(group.id.clone(), group.units[0].id.clone()),
                    &group.units[0].source_text,
                    group.context_fingerprint,
                ),
            });
            group.units.push(GenericStoredUnit {
                id: "pending".to_owned(),
                ordinal: 1,
                source_text: "こんにちは".to_owned(),
                translation: None,
                rejected: None,
            });
            let prepared = prepare_generic_translation(
                &snapshot,
                Arc::new(CompiledTerminology::empty()),
                &rules,
                &GenericPlaceholderRuleSource::ProjectSnapshot,
                Arc::new(japanese_language_module()),
                NonZeroUsize::new(10_000).unwrap(),
                false,
                &CooperativeCancellation::default(),
            )
            .unwrap();
            assert!(prepared.plan().invalidations().is_empty());
            let task = &prepared.plan().tasks()[0];
            assert_eq!(task.unit_count(), 1);
            let current_context = &task.groups()[0].units()[0];
            assert_eq!(current_context.output_id(), None);
            assert!(current_context.text().starts_with("名称："));
            assert!(!current_context.text().contains("abc-123"));
        }
    }

    #[test]
    fn cross_kind_dedup_validates_reuse_and_model_output_for_each_kind() {
        let rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["name".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let terminology = Arc::new(CompiledTerminology::empty());
        let language_module: Arc<dyn LanguageModule> = Arc::new(japanese_language_module());
        let snapshot = cross_kind_snapshot(None);
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            Arc::clone(&language_module),
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        )
        .expect("同文应该合并为一个模型输出");
        assert_eq!(prepared.plan().tasks().len(), 1);
        let parsed = parse_translation_response(
            r#"{"0":["你好 {invented}"]}"#,
            TranslationResponseMode::new(false, false),
        )
        .expect("响应应该可解析");
        let acceptance = accept_generic_response_with_cancellation(
            prepared.plan().tasks()[0].clone(),
            &parsed,
            prepared.facts(),
            &rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module.as_ref(),
            &CooperativeCancellation::default(),
        )
        .expect("响应验收不应取消");
        assert_eq!(acceptance.accepted().len(), 2, "同 kind 的合法目标都应保存");
        assert_eq!(acceptance.accepted_output_count(), 1);
        assert!(acceptance.problems().iter().any(|problem| matches!(
            problem,
            ResponseProblem::InvalidDestination { destination, .. }
                if destination.group_id.as_ref().is_some_and(|id| id.to_string() == "target")
        )));

        let snapshot = cross_kind_snapshot(Some("你好 {invented}"));
        let prepared = prepare_generic_translation(
            &snapshot,
            terminology,
            &rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        )
        .expect("复用失败的目标应该改为请求模型");
        assert_eq!(prepared.plan().reused().len(), 1);
        assert_eq!(prepared.plan().reused()[0].key().group_id(), "source-2");
        assert_eq!(prepared.plan().tasks().len(), 1);
        assert_eq!(prepared.plan().tasks()[0].groups()[2].kind(), "name");
    }

    fn planning(snapshot: &GenericStoredSnapshot) -> Vec<PlanningUnit> {
        snapshot
            .files()
            .iter()
            .flat_map(|file| {
                file.groups().iter().flat_map(move |group| {
                    group.units().iter().map(move |unit| {
                        PlanningUnit::new(
                            GenericUnitKey::new(group.id().to_owned(), unit.id().to_owned()),
                            GenericPlanningUnitLocator::new(
                                file.relative_path(),
                                group.id().to_owned(),
                                unit.id().to_owned(),
                                group.kind().to_owned(),
                            ),
                            format!("<{}>", unit.source_text()),
                            fingerprint(if unit.source_text() == "同文" { 1 } else { 2 }),
                            true,
                            unit.translation()
                                .map(|translation| translation.translation().to_owned()),
                            fingerprint(7),
                        )
                    })
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
                                rejected: None,
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

    fn two_short_groups_stable_json_target() -> NonZeroUsize {
        let stable_projection = r#"{"groups":[{"kind":"k","units":[{"text":["a"]}]},{"kind":"k","units":[{"text":["b"]}]}]}"#;
        NonZeroUsize::new(stable_projection.chars().count()).expect("稳定 JSON 投影非空")
    }

    #[test]
    fn stable_task_packing_uses_complete_file_groups_and_restarts_ids() {
        let snapshot = task_split_snapshot(&[10]);
        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning(&snapshot),
            two_short_groups_stable_json_target(),
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
                [0, 1],
                "每个最终 Task 都应从 0 重新编号"
            );
        }
    }

    #[test]
    fn stable_task_packing_never_combines_different_files() {
        let snapshot = task_split_snapshot(&[3, 3]);
        let plan = plan_translation_with_cancellation(
            &snapshot,
            &planning(&snapshot),
            two_short_groups_stable_json_target(),
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
                        GenericPlanningUnitLocator::new(
                            snapshot.files()[0].relative_path(),
                            group.id().to_owned(),
                            unit.id().to_owned(),
                            group.kind().to_owned(),
                        ),
                        format!("<{}>", unit.source_text()),
                        fingerprint(u8::try_from(group_index + 1).expect("测试索引应可表示")),
                        model_representative,
                        (!model_representative).then(|| format!("target-{group_index}")),
                        fingerprint(7),
                    )
                })
                .collect::<Vec<_>>()
        };
        let target = two_short_groups_stable_json_target();

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
                [0]
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
                rejected: None,
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
    fn retry_rejected_resolved_by_reuse_remains_in_planned_and_rejected_baselines() {
        let mut snapshot = stored_snapshot();
        snapshot.files[0].groups[0].units[0].translation = Some(GenericStoredTranslation {
            translation: "相同".to_owned(),
            origin: TranslationOrigin::Manual,
            state_fingerprint: fingerprint(9),
        });
        let mut planning_units = planning(&snapshot);
        let rejected = planning_units
            .iter_mut()
            .find(|unit| unit.key().group_id() == "g3")
            .expect("测试快照应含跨文件同文目标");
        rejected.current_rejected = true;
        rejected.retry_rejected = true;

        let plan = plan_translation(&snapshot, &planning_units, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();

        assert!(
            plan.reused()
                .iter()
                .any(|reuse| reuse.key().group_id() == "g3" && reuse.was_current_rejected)
        );
        assert_eq!(plan.initial_rejected_units, 1);
        assert_eq!(plan.planned_units, 2, "独立模型 Unit 与复用修复各计一次");
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
                rejected: None,
            }],
        });

        let plan = plan_translation_with_validator_and_cancellation(
            &snapshot,
            &planning(&snapshot),
            NonZeroUsize::MAX,
            |key, candidate| {
                if key.group_id() == "g3" {
                    Ok::<_, GenericPlanningError>(Err(
                        GenericResponseDestinationProblem::PlaceholderBindingMismatch,
                    ))
                } else {
                    Ok::<_, GenericPlanningError>(Ok(ValidatedReuse::new(
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
                rejected: None,
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
    fn current_requires_exact_applicability_not_future_request_policy() {
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
        let automatic_state = automatic_translation_state_fingerprint(
            project.language_pair(),
            &key,
            original.source_text(),
            group.context_fingerprint(),
        );
        let automatic = GenericStoredUnit {
            translation: Some(GenericStoredTranslation {
                translation: "直接 SQL 修改后的正文".to_owned(),
                origin: TranslationOrigin::Automatic,
                state_fingerprint: automatic_state,
            }),
            ..original.clone()
        };
        let current = PlanningUnit::from_stored(StoredPlanningUnitInput {
            relative_path: Path::new("a.jsonl"),
            project,
            group,
            unit: &automatic,
            protected: &protected,
            terminology_indices: Vec::new(),
            needs_translation: true,
            retry_rejected: false,
        });
        assert_eq!(
            current.current_translation(),
            Some("直接 SQL 修改后的正文"),
            "目标译文本身不属于语义状态，直接 SQL 修改正文后仍应为 Current"
        );

        let mut changed_context = group.clone();
        changed_context.context_fingerprint = fingerprint(92);
        let stale = PlanningUnit::from_stored(StoredPlanningUnitInput {
            relative_path: Path::new("a.jsonl"),
            project,
            group: &changed_context,
            unit: &automatic,
            protected: &protected,
            terminology_indices: Vec::new(),
            needs_translation: true,
            retry_rejected: false,
        });
        assert_eq!(
            stale.current_translation(),
            None,
            "持久状态必须精确匹配当前正文适用事实"
        );

        let manual = GenericStoredUnit {
            translation: Some(GenericStoredTranslation {
                translation: "人工修订".to_owned(),
                origin: TranslationOrigin::Manual,
                state_fingerprint: fingerprint(27),
            }),
            ..original.clone()
        };
        let current = PlanningUnit::from_stored(StoredPlanningUnitInput {
            relative_path: Path::new("a.jsonl"),
            project,
            group,
            unit: &manual,
            protected: &protected,
            terminology_indices: Vec::new(),
            needs_translation: true,
            retry_rejected: false,
        });
        assert_eq!(current.current_translation(), Some("人工修订"));
    }

    #[test]
    fn current_rejected_is_skipped_by_default_and_retry_adds_it_to_pending() {
        let mut snapshot = stored_snapshot();
        snapshot.files.truncate(1);
        snapshot.files[0].groups.truncate(1);
        snapshot.files[0].groups[0].units.truncate(1);
        let project = snapshot.project().clone();
        let group = snapshot.files[0].groups[0].clone();
        let original = group.units[0].clone();
        let service = super::super::placeholder::GenericPlaceholderService::default();
        let rules = service.compile(Vec::new()).unwrap();
        let protected = service
            .protect(group.kind(), original.source_text(), &rules)
            .unwrap();
        let key = GenericUnitKey::new(group.id().to_owned(), original.id().to_owned());
        let state = automatic_translation_state_fingerprint(
            project.language_pair(),
            &key,
            original.source_text(),
            group.context_fingerprint(),
        );
        snapshot.files[0].groups[0].units[0].rejected = Some(GenericStoredRejectedTranslation {
            readable_id: "a.jsonl:line1:unit1:text".to_owned(),
            origin: TranslationOrigin::Automatic,
            source: vec![original.source_text().to_owned()],
            candidate_json: "true".to_owned(),
            translation: None,
            group_context: group.context_fingerprint(),
            violation: ProvenInvariantViolation::InvalidCandidateShape,
            planning_state: state,
        });

        let make_planning = |retry_rejected| {
            PlanningUnit::from_stored(StoredPlanningUnitInput {
                relative_path: Path::new("a.jsonl"),
                project: &project,
                group: &group,
                unit: &snapshot.files[0].groups[0].units[0],
                protected: &protected,
                terminology_indices: Vec::new(),
                needs_translation: true,
                retry_rejected,
            })
        };
        let default = make_planning(false);
        assert!(!default.needs_candidate());
        assert!(default.is_skipped_rejected());
        let plan = plan_translation(&snapshot, &[default], |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        assert!(plan.tasks().is_empty());
        assert_eq!(plan.planned_units, 1);
        assert_eq!(plan.initial_rejected_units, 1);

        let retried = make_planning(true);
        assert!(retried.needs_candidate());
        let plan = plan_translation(&snapshot, &[retried], |_, candidate| {
            Ok(candidate.to_owned())
        })
        .unwrap();
        assert_eq!(
            plan.tasks()
                .iter()
                .map(PlannedTask::unit_count)
                .sum::<usize>(),
            1
        );
        assert_eq!(plan.planned_units, 1);
        assert_eq!(plan.initial_rejected_units, 1);
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
            r#"{"0":["译文","第二行"],"1":3,"99":["额外"]}"#,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("object 可解析");

        assert_eq!(acceptance.accepted().len(), 2, "同文族传播到两个 Unit");
        assert_eq!(acceptance.rejected().len(), 1);
        assert_eq!(acceptance.rejected()[0].candidate_json, "3");
        assert_eq!(
            acceptance.rejected()[0].violation,
            ProvenInvariantViolation::InvalidCandidateShape
        );
        assert!(acceptance.problems().iter().any(|problem| matches!(
            problem,
            ResponseProblem::InvalidValue { output_id, .. } if *output_id == 1
        )));
        assert!(
            acceptance
                .problems()
                .contains(&ResponseProblem::UnexpectedId { output_id: 99 })
        );
    }

    #[test]
    fn acceptance_diagnostics_distinguish_multiple_invalid_id_items() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let acceptance = accept_response(
            &plan.tasks()[0],
            r#"{"0":["同文译文"],"bad":["甲"],"1":["独立译文"],"01":["乙"]}"#,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("包含非法 ID 的根对象仍应逐项验收");

        let invalid = acceptance
            .problems()
            .iter()
            .filter_map(|problem| match problem {
                ResponseProblem::InvalidId { item_index } => Some((*item_index, problem.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            invalid.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [1, 3]
        );

        let reasons = invalid
            .into_iter()
            .map(|(_, problem)| {
                let issue = GenericIssue::project(
                    GenericDiagnosticStage::Translate,
                    crate::diagnostic::GenericProblem::TaskResponse {
                        task_ordinal: 1,
                        total_tasks: 1,
                        problem,
                    },
                );
                render_diagnostic_fields(
                    &DiagnosticReport::new(
                        StateEffect::ProgressPreserved,
                        Diagnostic::generic(issue),
                    ),
                    &UiLocalizer::new(UiLocale::SimplifiedChinese),
                )
                .reason
                .replace(['\u{2068}', '\u{2069}'], "")
            })
            .collect::<Vec<_>>();
        assert!(reasons[0].contains("响应第 2 项"));
        assert!(reasons[1].contains("响应第 4 项"));
    }

    #[test]
    fn response_array_items_reject_embedded_line_delimiters_but_allow_multiple_items() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let task = &plan.tasks()[0];

        let multiple_items = accept_response(
            task,
            r#"{"0":["甲","乙"],"1":["合法"]}"#,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("数组项之间的协议分行应合法");
        assert_eq!(multiple_items.accepted().len(), 3);
        assert_eq!(
            multiple_items
                .accepted()
                .iter()
                .filter(|translation| translation.translation == "甲\n乙")
                .count(),
            2,
            "合法数组项应只在验收后由 ATT 使用 LF 连接"
        );

        for (invalid, expected) in [
            ("甲\r乙", GenericResponseTextProblem::CarriageReturn),
            ("甲\n乙", GenericResponseTextProblem::LineFeed),
            ("甲\0乙", GenericResponseTextProblem::Nul),
        ] {
            let response = serde_json::json!({"0": [invalid], "1": ["合法"]}).to_string();
            let acceptance = accept_response(
                task,
                &response,
                TranslationResponseMode::new(false, false),
                |_, _, candidate| Ok(candidate.to_owned()),
            )
            .expect("合法 JSON 中的逐项文本错误应只拒绝对应 ID");

            assert_eq!(acceptance.accepted().len(), 1, "合法同级 ID 仍应保存");
            assert_eq!(acceptance.rejected().len(), 2, "同文族的两个目标都应拒绝");
            assert!(
                acceptance
                    .problems()
                    .contains(&ResponseProblem::InvalidTranslation {
                        output_id: 0,
                        problem: expected,
                    })
            );
            assert!(acceptance.rejected().iter().all(|rejected| matches!(
                &rejected.violation,
                ProvenInvariantViolation::InvalidLineText { line_index: 0 }
            )));
        }
    }

    #[test]
    fn source_echo_content_is_ignored_but_its_shape_is_checked_per_generic_id() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let task = &plan.tasks()[0];
        let accepted = accept_response(
            task,
            r#"{"0":{"source":["与请求不同"],"translation":["同文译文"]},"1":{"source":["也不比较"],"translation":["独立译文"]}}"#,
            TranslationResponseMode::new(false, true),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("原文回显内容不同仍应按 ID 验收");
        assert_eq!(accepted.accepted().len(), 3);
        assert!(accepted.problems().is_empty());

        let partial = accept_response(
            task,
            r#"{"0":{"source":true,"translation":["同文译文"]},"1":{"source":["任意"],"translation":["独立译文"]}}"#,
            TranslationResponseMode::new(false, true),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("逐 ID 的 source 形状错误不能使整份响应失效");
        assert_eq!(partial.accepted().len(), 1);
        assert!(matches!(
            partial.problems(),
            [ResponseProblem::InvalidValue {
                output_id: 0,
                problem: GenericResponseValueProblem::SourceNotArray,
            }]
        ));
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_deeply_nested_non_string_only_rejects_its_generic_id() {
        const DEPTH: usize = 10_000;

        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let deep_value = format!("{}0{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let response = format!(r#"{{"0":["同级合法译文"],"1":{deep_value}}}"#);

        let acceptance = accept_response(
            &plan.tasks()[0],
            &response,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("任意深的值不应破坏有效外层 object");

        assert_eq!(
            acceptance.accepted().len(),
            2,
            "合法同级 ID 仍应传播到两个 Generic Unit"
        );
        assert!(matches!(
            acceptance.problems(),
            [ResponseProblem::InvalidValue { output_id, .. }] if *output_id == 1
        ));
    }

    #[test]
    fn response_validation_polls_cancellation_inside_long_candidate_text() {
        let snapshot = stored_snapshot();
        let plan = plan_translation(&snapshot, &planning(&snapshot), |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("规划应成功");
        let response = format!(r#"{{"0":["{}"]}}"#, "文".repeat(128 * 1024));
        let parsed =
            parse_translation_response(&response, TranslationResponseMode::new(false, false))
                .expect("响应应可解析");
        let polls = Cell::new(0_usize);

        let result =
            accept_parsed_response_with_cancellation(
                plan.tasks()[0].clone(),
                &parsed,
                |_,
                 _,
                 _|
                 -> Result<
                    Result<String, GenericResponseDestinationProblem>,
                    GenericPlanningError,
                > {
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
            r#"{"0":["同文译文"],"1":["独立译文"]}"#,
            TranslationResponseMode::new(false, false),
            |_, key, candidate| {
                if key.group_id() == "g3" {
                    Err(GenericResponseDestinationProblem::PlaceholderBindingMismatch)
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
                    output_id: 0,
                    destination: DiagnosticGenericUnitLocator::new(
                        "b.jsonl",
                        "g3",
                        "u1",
                        Some("dialogue"),
                    )
                    .with_natural_position(1, 1),
                    problem: GenericResponseDestinationProblem::PlaceholderBindingMismatch,
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
            r#"{"0":["同文译文"],"1":["独立译文"]}"#,
            TranslationResponseMode::new(false, false),
            |output_id, _, candidate| {
                if output_id == task_id(0) {
                    Err(GenericResponseDestinationProblem::PlaceholderBindingMismatch)
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
                    output_id: 0,
                    destination: DiagnosticGenericUnitLocator::new(
                        "a.jsonl",
                        "g1",
                        "u1",
                        Some("dialogue"),
                    )
                    .with_natural_position(1, 1),
                    problem: GenericResponseDestinationProblem::PlaceholderBindingMismatch,
                },
                ResponseProblem::InvalidDestination {
                    output_id: 0,
                    destination: DiagnosticGenericUnitLocator::new(
                        "b.jsonl",
                        "g3",
                        "u1",
                        Some("dialogue"),
                    )
                    .with_natural_position(1, 1),
                    problem: GenericResponseDestinationProblem::PlaceholderBindingMismatch,
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
            r#"{"0":["甲"],"0":["乙"]}"#,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("公共协议应保留重复项，交给逐 ID 验收");
        assert!(acceptance.accepted().is_empty());
        assert!(
            acceptance.rejected().is_empty(),
            "重复或缺失 ID 无法唯一绑定当前 Unit，不得替换已有 Rejected"
        );
        assert_eq!(
            acceptance.problems(),
            [
                ResponseProblem::DuplicateId { output_id: 0 },
                ResponseProblem::MissingId { output_id: 1 }
            ]
        );
    }

    #[test]
    fn stale_translation_is_retained_until_a_replacement_commits() {
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
        stale_unit.current_context = None;
        stale_unit.expected_previous = Some(stale.clone());
        stale_unit.invalidated_previous = None;

        let plan = plan_translation(&snapshot, &planning_units, |_, candidate| {
            Ok(candidate.to_owned())
        })
        .expect("失效译文应该可规划");

        assert!(plan.invalidations().is_empty());
        let destination = plan
            .tasks()
            .iter()
            .flat_map(|task| task.outputs.values())
            .flatten()
            .find(|destination| {
                destination.key.group_id() == "g1" && destination.key.unit_id() == "u2"
            })
            .expect("失效 Unit 应重新参与模型任务");
        assert_eq!(destination.expected_previous.as_ref(), Some(&stale));

        let (task, output_id) = plan
            .tasks()
            .iter()
            .find_map(|task| {
                task.outputs.iter().find_map(|(output_id, destinations)| {
                    destinations
                        .iter()
                        .any(|destination| {
                            destination.key.group_id() == "g1" && destination.key.unit_id() == "u2"
                        })
                        .then_some((task, *output_id))
                })
            })
            .expect("失效 Unit 应有模型输出 ID");
        let response = format!(r#"{{"{}":3}}"#, output_id.get());
        let acceptance = accept_response(
            task,
            &response,
            TranslationResponseMode::new(false, false),
            |_, _, candidate| Ok(candidate.to_owned()),
        )
        .expect("硬拒绝响应应该可以逐 ID 验收");
        let rejected = acceptance
            .rejected()
            .iter()
            .find(|rejected| rejected.key.group_id() == "g1" && rejected.key.unit_id() == "u2")
            .expect("失效 Unit 的硬拒绝候选必须保留 CAS 旧值")
            .clone()
            .into_write();
        assert_eq!(rejected.expected_translation, Some(stale));
    }
}
