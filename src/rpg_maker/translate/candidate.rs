//! Standard 人工候选的只读语义会话与批量验收。
//!
//! 本模块只拥有人工提交特有的状态选择与覆盖策略。Placeholder、语言验收、
//! state 和去重边界分别继续由现有 Standard 语义所有者提供。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::RpgMakerLocation;

use super::deduplication::{TranslationDeduplicationCandidate, translation_deduplication_families};
use super::executor::TranslationContentAcceptance;
#[cfg(test)]
use super::planner::prepare_corpus;
use super::planner::{
    CorpusPlanningError, PreparedScope, ScopePreprocessingError, expected_line_shape,
    translation_state_context,
};
use super::semantics::{
    PreparedTranslationStatus, PreparedTranslationText, ResolvedTranslationSemanticError,
    ResolvedTranslationSemantics,
};
#[cfg(test)]
use super::standard::StandardTranslationCorpus;
use super::standard::{
    ExpectedLineShape, TerminologyDependency, TranslationSnapshotBaseline, TranslationStateContext,
    TranslationTargetConstraints, TranslationUnitIdentity, TranslationUnitRejectionReason,
};

/// 当前人工候选会话中的稳定物理单元下标。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct StandardCandidateUnitIndex(usize);

impl StandardCandidateUnitIndex {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StandardCandidateUnitStatus {
    Current,
    Missing,
    Stale,
    NotApplicable,
    Unavailable,
}

/// Lua Host 可以安全投影的一个只读 Standard 物理单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardCandidateUnit {
    index: StandardCandidateUnitIndex,
    identity: TranslationUnitIdentity,
    translation: Option<TextUnitContent>,
    model_text: TextUnitContent,
    terms: Vec<TerminologyDependency>,
    line_shape: ExpectedLineShape,
    status: StandardCandidateUnitStatus,
    family_size: usize,
}

impl StandardCandidateUnit {
    pub(crate) const fn index(&self) -> StandardCandidateUnitIndex {
        self.index
    }

    pub(crate) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(crate) fn translation(&self) -> Option<&TextUnitContent> {
        self.translation.as_ref()
    }

    pub(crate) fn model_text(&self) -> &TextUnitContent {
        &self.model_text
    }

    pub(crate) fn terms(&self) -> &[TerminologyDependency] {
        &self.terms
    }

    pub(crate) const fn line_shape(&self) -> ExpectedLineShape {
        self.line_shape
    }

    pub(crate) const fn status(&self) -> StandardCandidateUnitStatus {
        self.status
    }

    pub(crate) const fn family_size(&self) -> usize {
        self.family_size
    }
}

struct StandardCandidateUnitState {
    view: StandardCandidateUnit,
    expected_translation_state: Option<Sha256Fingerprint>,
    prepared: Option<PreparedTranslationText>,
    state_context: Option<TranslationStateContext>,
    family_index: Option<usize>,
    unavailable_reason: Option<String>,
}

struct StandardCandidateSessionState {
    units: Vec<StandardCandidateUnitState>,
}

/// 一次 `open()` 冻结的完整 Standard 语义与数据库快照。
pub(crate) struct StandardCandidateSession {
    baseline: TranslationSnapshotBaseline,
    families: Vec<Vec<usize>>,
    state: Mutex<StandardCandidateSessionState>,
    acceptance_gate: tokio::sync::Mutex<()>,
}

/// 一个普通 Standard 语义 Scope 的独立人工候选预处理结果。
///
/// `open_candidate_session` 按普通 Planner 的相同 Scope 边界并行计算这些值，
/// 最终组装仍按输入顺序进行，因此并行度不会改变物理单元或去重族顺序。
pub(super) struct PreparedStandardCandidateScope {
    units: Vec<StandardCandidateUnitState>,
}

pub(super) fn prepare_candidate_scope(
    semantics: Arc<ResolvedTranslationSemantics>,
    scope: PreparedScope,
) -> Result<PreparedStandardCandidateScope, StandardCandidateSessionBuildError> {
    let mut units = Vec::new();
    for group in scope.into_groups() {
        let (group_kind, assets) = group.into_parts();
        for asset in assets {
            let (identity, translation, translation_state) = asset.into_parts();
            let line_shape = expected_line_shape(&identity);
            match semantics.prepare_content(group_kind, identity.source_content()) {
                Ok(prepared) => {
                    let state_context = translation_state_context(
                        semantics.global_fingerprint(),
                        &identity,
                        prepared.model_text(),
                        prepared.placeholders(),
                        prepared.terms(),
                    )
                    .map_err(StandardCandidateSessionBuildError::StateContext)?;
                    let current = translation.as_ref().is_some_and(|translation| {
                        translation_state == Some(state_context.finish(translation))
                    });
                    let status = match prepared.status() {
                        PreparedTranslationStatus::Active if current => {
                            StandardCandidateUnitStatus::Current
                        }
                        PreparedTranslationStatus::Active
                            if translation.is_some() || translation_state.is_some() =>
                        {
                            StandardCandidateUnitStatus::Stale
                        }
                        PreparedTranslationStatus::Active => StandardCandidateUnitStatus::Missing,
                        PreparedTranslationStatus::NonSourceLanguage
                        | PreparedTranslationStatus::FullyProtected => {
                            StandardCandidateUnitStatus::NotApplicable
                        }
                    };
                    let model_text =
                        content_with_model_text(identity.source_content(), prepared.model_text());
                    let active = prepared.status() == PreparedTranslationStatus::Active;
                    units.push(StandardCandidateUnitState {
                        view: StandardCandidateUnit {
                            index: StandardCandidateUnitIndex::new(0),
                            identity,
                            translation,
                            model_text,
                            terms: prepared.terms().to_vec(),
                            line_shape,
                            status,
                            family_size: 1,
                        },
                        expected_translation_state: translation_state,
                        prepared: active.then_some(prepared),
                        state_context: active.then_some(state_context),
                        family_index: None,
                        unavailable_reason: None,
                    });
                }
                Err(source) => {
                    units.push(StandardCandidateUnitState {
                        view: StandardCandidateUnit {
                            index: StandardCandidateUnitIndex::new(0),
                            model_text: identity.source_content().clone(),
                            terms: Vec::new(),
                            line_shape,
                            status: StandardCandidateUnitStatus::Unavailable,
                            identity,
                            translation,
                            family_size: 1,
                        },
                        expected_translation_state: translation_state,
                        prepared: None,
                        state_context: None,
                        family_index: None,
                        unavailable_reason: Some(source.safe_detail()),
                    });
                }
            }
        }
    }
    Ok(PreparedStandardCandidateScope { units })
}

impl StandardCandidateSession {
    #[cfg(test)]
    pub(crate) fn from_corpus(
        semantics: Arc<ResolvedTranslationSemantics>,
        corpus: StandardTranslationCorpus,
    ) -> Result<Self, StandardCandidateSessionBuildError> {
        let (groups, baseline) = corpus.into_parts();
        let prepared_scopes = prepare_corpus(groups)
            .map_err(StandardCandidateSessionBuildError::Corpus)?
            .into_scopes()
            .into_iter()
            .map(|scope| prepare_candidate_scope(Arc::clone(&semantics), scope))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::from_prepared_scopes(baseline, prepared_scopes))
    }

    pub(super) fn from_prepared_scopes(
        baseline: TranslationSnapshotBaseline,
        prepared_scopes: Vec<PreparedStandardCandidateScope>,
    ) -> Self {
        let mut units = Vec::new();
        let mut deduplication_candidates = Vec::new();
        let mut candidate_unit_indices = Vec::new();

        for prepared_scope in prepared_scopes {
            for mut unit in prepared_scope.units {
                let index = StandardCandidateUnitIndex::new(units.len());
                unit.view.index = index;
                if let (Some(prepared), Some(state_context)) =
                    (unit.prepared.as_ref(), unit.state_context)
                {
                    deduplication_candidates.push(TranslationDeduplicationCandidate::new(
                        unit.view.identity.clone(),
                        prepared.model_text(),
                        prepared.placeholders().to_vec(),
                        unit.view.translation.clone(),
                        unit.expected_translation_state,
                        state_context,
                        unit.view.translation.is_some()
                            && unit.view.status != StandardCandidateUnitStatus::Current,
                    ));
                    candidate_unit_indices.push(index.get());
                }
                units.push(unit);
            }
        }

        let candidate_families = translation_deduplication_families(&deduplication_candidates);
        let mut families = Vec::with_capacity(candidate_families.len());
        for candidate_family in candidate_families {
            let family_index = families.len();
            let members = candidate_family
                .into_iter()
                .map(|candidate_index| candidate_unit_indices[candidate_index])
                .collect::<Vec<_>>();
            let family_size = members.len();
            for &unit_index in &members {
                units[unit_index].family_index = Some(family_index);
                units[unit_index].view.family_size = family_size;
            }
            families.push(members);
        }

        Self {
            baseline,
            families,
            state: Mutex::new(StandardCandidateSessionState { units }),
            acceptance_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn units(&self) -> Vec<StandardCandidateUnit> {
        self.state
            .lock()
            .expect("Standard 人工候选会话互斥锁不应中毒")
            .units
            .iter()
            .map(|unit| unit.view.clone())
            .collect()
    }

    pub(crate) fn get(
        &self,
        owner: RpgMakerStandardAssetOwner,
        group_location: &RpgMakerLocation,
        role: &TextUnitRole,
    ) -> Option<StandardCandidateUnit> {
        self.state
            .lock()
            .expect("Standard 人工候选会话互斥锁不应中毒")
            .units
            .iter()
            .find(|unit| {
                let identity = unit.view.identity();
                identity.owner() == owner
                    && identity.group_location() == group_location
                    && identity.role() == role
            })
            .map(|unit| unit.view.clone())
    }

    pub(crate) async fn lock_acceptance(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.acceptance_gate.lock().await
    }

    pub(super) fn prepare_acceptance(
        &self,
        requests: Vec<StandardCandidateRequest>,
    ) -> Result<PreparedStandardCandidateAcceptance, StandardCandidatePreparationError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StandardCandidatePreparationError::SessionPoisoned)?;
        let mut results = vec![None; requests.len()];
        let mut requests_by_family = BTreeMap::<usize, Vec<usize>>::new();

        for (request_index, request) in requests.iter().enumerate() {
            let unit = state.units.get(request.unit_index.get()).ok_or(
                StandardCandidatePreparationError::InvalidUnitIndex {
                    index: request.unit_index.get(),
                    unit_count: state.units.len(),
                },
            )?;
            match unit.view.status {
                StandardCandidateUnitStatus::NotApplicable => {
                    results[request_index] = Some(StandardCandidateAcceptance::Rejected {
                        reason: StandardCandidateRejectionReason::NotApplicable,
                    });
                }
                StandardCandidateUnitStatus::Unavailable => {
                    results[request_index] = Some(StandardCandidateAcceptance::Rejected {
                        reason: StandardCandidateRejectionReason::Unavailable {
                            detail: unit
                                .unavailable_reason
                                .clone()
                                .unwrap_or_else(|| "standard_unit_unavailable".to_owned()),
                        },
                    });
                }
                StandardCandidateUnitStatus::Current
                | StandardCandidateUnitStatus::Missing
                | StandardCandidateUnitStatus::Stale => {
                    let family_index = unit.family_index.ok_or(
                        StandardCandidatePreparationError::ActiveUnitMissingFamily {
                            index: request.unit_index.get(),
                        },
                    )?;
                    requests_by_family
                        .entry(family_index)
                        .or_default()
                        .push(request_index);
                }
            }
        }

        let mut commits = Vec::new();
        for (family_index, request_indices) in requests_by_family {
            let first = &requests[request_indices[0]];
            if request_indices.iter().skip(1).any(|&request_index| {
                let request = &requests[request_index];
                request.candidate != first.candidate
                    || request.replace_current != first.replace_current
            }) {
                for request_index in request_indices {
                    results[request_index] = Some(StandardCandidateAcceptance::Rejected {
                        reason: StandardCandidateRejectionReason::ConflictingCandidate,
                    });
                }
                continue;
            }

            let members = self
                .families
                .get(family_index)
                .ok_or(StandardCandidatePreparationError::InvalidFamilyIndex { family_index })?;
            let target_constraints = TranslationTargetConstraints::from_identities(
                members
                    .iter()
                    .map(|&member_index| state.units[member_index].view.identity()),
            );
            let unit = &state.units[first.unit_index.get()];
            let prepared = unit.prepared.as_ref().ok_or(
                StandardCandidatePreparationError::ActiveUnitMissingPreparedSemantics {
                    index: first.unit_index.get(),
                },
            )?;
            let accepted = prepared
                .accept_content(
                    unit.view.identity(),
                    target_constraints,
                    unit.view.line_shape(),
                    first.candidate.clone(),
                )
                .map_err(StandardCandidatePreparationError::Semantic)?;
            let translation = match accepted {
                TranslationContentAcceptance::Accepted(translation) => translation,
                TranslationContentAcceptance::Rejected(reason) => {
                    for request_index in request_indices {
                        results[request_index] = Some(StandardCandidateAcceptance::Rejected {
                            reason: StandardCandidateRejectionReason::Candidate(reason.clone()),
                        });
                    }
                    continue;
                }
            };
            let changes_current = members.iter().any(|&member_index| {
                let member = &state.units[member_index];
                member.view.status == StandardCandidateUnitStatus::Current
                    && member.view.translation.as_ref() != Some(&translation)
            });
            if changes_current && !first.replace_current {
                for request_index in request_indices {
                    results[request_index] = Some(StandardCandidateAcceptance::Rejected {
                        reason: StandardCandidateRejectionReason::CurrentReplacementRequired,
                    });
                }
                continue;
            }

            let mut changed_locations = 0usize;
            let mut writes = Vec::with_capacity(members.len());
            for &member_index in members {
                let member = &state.units[member_index];
                let state_context = member.state_context.ok_or(
                    StandardCandidatePreparationError::ActiveUnitMissingStateContext {
                        index: member_index,
                    },
                )?;
                let replacement_state = state_context.finish(&translation);
                if member.view.translation.as_ref() != Some(&translation)
                    || member.expected_translation_state != Some(replacement_state)
                {
                    changed_locations += 1;
                }
                writes.push(StandardCandidateWrite {
                    unit_index: StandardCandidateUnitIndex::new(member_index),
                    identity: member.view.identity.clone(),
                    expected_translation: member.view.translation.clone(),
                    expected_translation_state: member.expected_translation_state,
                    replacement_translation: translation.clone(),
                    replacement_translation_state: replacement_state,
                });
            }
            for request_index in request_indices {
                results[request_index] = Some(StandardCandidateAcceptance::Accepted {
                    translation: translation.clone(),
                    changed_locations,
                });
            }
            commits.push(StandardCandidateFamilyCommit { writes });
        }

        Ok(PreparedStandardCandidateAcceptance {
            baseline: self.baseline.clone(),
            results: results
                .into_iter()
                .map(|result| result.expect("每项人工候选必须得到一个正常结果"))
                .collect(),
            commits,
        })
    }

    pub(super) fn apply_committed(
        &self,
        prepared: &PreparedStandardCandidateAcceptance,
    ) -> Result<(), StandardCandidatePreparationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StandardCandidatePreparationError::SessionPoisoned)?;
        let unit_count = state.units.len();
        for commit in &prepared.commits {
            for write in &commit.writes {
                let unit = state.units.get_mut(write.unit_index.get()).ok_or(
                    StandardCandidatePreparationError::InvalidUnitIndex {
                        index: write.unit_index.get(),
                        unit_count,
                    },
                )?;
                unit.view.translation = Some(write.replacement_translation.clone());
                unit.expected_translation_state = Some(write.replacement_translation_state);
                unit.view.status = StandardCandidateUnitStatus::Current;
            }
        }
        Ok(())
    }
}

fn content_with_model_text(source: &TextUnitContent, model_text: &str) -> TextUnitContent {
    match source {
        TextUnitContent::Value(_) => TextUnitContent::Value(model_text.to_owned()),
        TextUnitContent::Lines(_) => {
            TextUnitContent::Lines(model_text.split('\n').map(str::to_owned).collect())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardCandidateRequest {
    unit_index: StandardCandidateUnitIndex,
    candidate: TextUnitContent,
    replace_current: bool,
}

impl StandardCandidateRequest {
    pub(crate) fn new(
        unit_index: StandardCandidateUnitIndex,
        candidate: TextUnitContent,
        replace_current: bool,
    ) -> Self {
        Self {
            unit_index,
            candidate,
            replace_current,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardCandidateAcceptance {
    Accepted {
        translation: TextUnitContent,
        changed_locations: usize,
    },
    Rejected {
        reason: StandardCandidateRejectionReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardCandidateRejectionReason {
    NotApplicable,
    Unavailable { detail: String },
    ConflictingCandidate,
    CurrentReplacementRequired,
    Candidate(TranslationUnitRejectionReason),
}

#[derive(Clone)]
pub(super) struct PreparedStandardCandidateAcceptance {
    baseline: TranslationSnapshotBaseline,
    results: Vec<StandardCandidateAcceptance>,
    commits: Vec<StandardCandidateFamilyCommit>,
}

impl PreparedStandardCandidateAcceptance {
    pub(super) fn baseline(&self) -> &TranslationSnapshotBaseline {
        &self.baseline
    }

    pub(super) fn results(&self) -> &[StandardCandidateAcceptance] {
        &self.results
    }

    pub(super) fn commits(&self) -> &[StandardCandidateFamilyCommit] {
        &self.commits
    }
}

#[derive(Clone)]
pub(super) struct StandardCandidateFamilyCommit {
    writes: Vec<StandardCandidateWrite>,
}

impl StandardCandidateFamilyCommit {
    pub(super) fn writes(&self) -> &[StandardCandidateWrite] {
        &self.writes
    }
}

#[derive(Clone)]
pub(super) struct StandardCandidateWrite {
    unit_index: StandardCandidateUnitIndex,
    identity: TranslationUnitIdentity,
    expected_translation: Option<TextUnitContent>,
    expected_translation_state: Option<Sha256Fingerprint>,
    replacement_translation: TextUnitContent,
    replacement_translation_state: Sha256Fingerprint,
}

impl StandardCandidateWrite {
    pub(super) fn identity(&self) -> &TranslationUnitIdentity {
        &self.identity
    }

    pub(super) fn expected_translation(&self) -> Option<&TextUnitContent> {
        self.expected_translation.as_ref()
    }

    pub(super) const fn expected_translation_state(&self) -> Option<Sha256Fingerprint> {
        self.expected_translation_state
    }

    pub(super) fn replacement_translation(&self) -> &TextUnitContent {
        &self.replacement_translation
    }

    pub(super) const fn replacement_translation_state(&self) -> Sha256Fingerprint {
        self.replacement_translation_state
    }
}

#[derive(Debug)]
pub(crate) enum StandardCandidateSessionBuildError {
    Corpus(CorpusPlanningError),
    StateContext(ScopePreprocessingError),
}

impl StandardCandidateSessionBuildError {
    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::Corpus(source) => {
                format!("reason=invalid_corpus; detail={source}")
            }
            Self::StateContext(source) => format!(
                "reason=state_context; {}",
                super::planner::scope_preprocessing_detail(source)
            ),
        }
    }
}

impl fmt::Display for StandardCandidateSessionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corpus(source) => {
                write!(formatter, "Standard 人工候选语料无效：{source}")
            }
            Self::StateContext(source) => {
                write!(
                    formatter,
                    "无法建立 Standard 人工候选 state 上下文：{source}"
                )
            }
        }
    }
}

impl Error for StandardCandidateSessionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Corpus(source) => Some(source),
            Self::StateContext(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum StandardCandidatePreparationError {
    InvalidUnitIndex { index: usize, unit_count: usize },
    InvalidFamilyIndex { family_index: usize },
    ActiveUnitMissingFamily { index: usize },
    ActiveUnitMissingPreparedSemantics { index: usize },
    ActiveUnitMissingStateContext { index: usize },
    SessionPoisoned,
    Semantic(ResolvedTranslationSemanticError),
}

impl StandardCandidatePreparationError {
    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::InvalidUnitIndex { index, unit_count } => {
                format!("reason=invalid_unit_index; index={index}; unit_count={unit_count}")
            }
            Self::InvalidFamilyIndex { family_index } => {
                format!("reason=invalid_family_index; family={family_index}")
            }
            Self::ActiveUnitMissingFamily { index } => {
                format!("reason=active_unit_missing_family; index={index}")
            }
            Self::ActiveUnitMissingPreparedSemantics { index } => {
                format!("reason=active_unit_missing_prepared_semantics; index={index}")
            }
            Self::ActiveUnitMissingStateContext { index } => {
                format!("reason=active_unit_missing_state_context; index={index}")
            }
            Self::SessionPoisoned => "reason=session_poisoned".to_owned(),
            Self::Semantic(source) => {
                format!("reason=semantic_failure; {}", source.safe_detail())
            }
        }
    }
}

impl fmt::Display for StandardCandidatePreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUnitIndex { index, unit_count } => write!(
                formatter,
                "Standard 人工候选句柄无效：index={index}, unit_count={unit_count}"
            ),
            Self::InvalidFamilyIndex { family_index } => {
                write!(
                    formatter,
                    "Standard 人工候选去重族无效：family={family_index}"
                )
            }
            Self::ActiveUnitMissingFamily { index } => {
                write!(formatter, "活跃 Standard 单元 {index} 没有去重族")
            }
            Self::ActiveUnitMissingPreparedSemantics { index } => {
                write!(formatter, "活跃 Standard 单元 {index} 没有验收语义")
            }
            Self::ActiveUnitMissingStateContext { index } => {
                write!(formatter, "活跃 Standard 单元 {index} 没有 state 上下文")
            }
            Self::SessionPoisoned => formatter.write_str("Standard 人工候选会话状态锁已中毒"),
            Self::Semantic(source) => write!(formatter, "Standard 人工候选验收失败：{source}"),
        }
    }
}

impl Error for StandardCandidatePreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Semantic(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::standard::{StandardTranslationAsset, StandardTranslationGroup};
    use super::*;
    use crate::rpg_maker::model::ScalarFieldKey;
    use crate::rpg_maker::text::{
        RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };

    fn identity(index: usize, source: TextUnitContent) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![RpgMakerLocationStep::index(index)],
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键应合法")),
            source,
            "{}",
        )
    }

    fn tag_identity(index: usize, source: TextUnitContent) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::note_tag(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![RpgMakerLocationStep::index(index)],
                "Help",
                0,
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键应合法")),
            source,
            "{}",
        )
    }

    fn corpus_with_duplicate_family(
        semantics: &ResolvedTranslationSemantics,
        current_translation: Option<&str>,
    ) -> StandardTranslationCorpus {
        let source = TextUnitContent::Value("魔法剣".to_owned());
        let first_identity = identity(1, source.clone());
        let first_state = current_translation.map(|translation| {
            let prepared = semantics
                .prepare_content(TextGroupKind::DatabaseEntry, &source)
                .expect("测试原文应可准备");
            translation_state_context(
                semantics.global_fingerprint(),
                &first_identity,
                prepared.model_text(),
                prepared.placeholders(),
                prepared.terms(),
            )
            .expect("测试 state 上下文应可建立")
            .finish(&TextUnitContent::Value(translation.to_owned()))
        });
        let first_location = first_identity.group_location().clone();
        let second_identity = identity(2, source);
        let second_location = second_identity.group_location().clone();
        StandardTranslationCorpus::new(vec![
            StandardTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                first_location,
                vec![StandardTranslationAsset::new(
                    first_identity,
                    current_translation
                        .map(|translation| TextUnitContent::Value(translation.to_owned())),
                    first_state,
                )],
            ),
            StandardTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                second_location,
                vec![StandardTranslationAsset::new(second_identity, None, None)],
            ),
        ])
    }

    fn corpus_with_conflicting_current_family(
        semantics: &ResolvedTranslationSemantics,
    ) -> StandardTranslationCorpus {
        let source = TextUnitContent::Value("魔法剣".to_owned());
        let assets = [(1, "人工译文"), (2, "另一译文")]
            .into_iter()
            .map(|(index, translation)| {
                let identity = identity(index, source.clone());
                let prepared = semantics
                    .prepare_content(TextGroupKind::DatabaseEntry, &source)
                    .expect("测试原文应可准备");
                let state = translation_state_context(
                    semantics.global_fingerprint(),
                    &identity,
                    prepared.model_text(),
                    prepared.placeholders(),
                    prepared.terms(),
                )
                .expect("测试 state 上下文应可建立")
                .finish(&TextUnitContent::Value(translation.to_owned()));
                let location = identity.group_location().clone();
                StandardTranslationGroup::new(
                    TextGroupKind::DatabaseEntry,
                    location,
                    vec![StandardTranslationAsset::new(
                        identity,
                        Some(TextUnitContent::Value(translation.to_owned())),
                        Some(state),
                    )],
                )
            })
            .collect();
        StandardTranslationCorpus::new(assets)
    }

    fn corpus_with_tag_family_member() -> StandardTranslationCorpus {
        let source = TextUnitContent::Value("魔法剣".to_owned());
        let ordinary = identity(1, source.clone());
        let tag = tag_identity(2, source);
        StandardTranslationCorpus::new(vec![
            StandardTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                ordinary.group_location().clone(),
                vec![StandardTranslationAsset::new(ordinary, None, None)],
            ),
            StandardTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                tag.group_location().clone(),
                vec![StandardTranslationAsset::new(tag, None, None)],
            ),
        ])
    }

    #[test]
    fn session_exposes_physical_units_and_shared_exact_family() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_duplicate_family(semantics.as_ref(), Some("人工译文")),
        )
        .expect("人工候选会话应可建立");

        let units = session.units();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].status(), StandardCandidateUnitStatus::Current);
        assert_eq!(units[1].status(), StandardCandidateUnitStatus::Missing);
        assert_eq!(units[0].family_size(), 2);
        assert_eq!(units[1].family_size(), 2);
        assert_eq!(
            units[0].model_text(),
            &TextUnitContent::Value("魔法剣".to_owned())
        );
    }

    #[test]
    fn session_uses_the_same_validated_physical_order_as_standard_planning() {
        let groups = [2, 1, 3]
            .into_iter()
            .map(|index| {
                let identity =
                    identity(index, TextUnitContent::Value(format!("テスト原文{index}")));
                StandardTranslationGroup::new(
                    identity.kind(),
                    identity.group_location().clone(),
                    vec![StandardTranslationAsset::new(identity, None, None)],
                )
            })
            .collect::<Vec<_>>();
        let ordinary_order = prepare_corpus(groups.clone())
            .expect("普通 Standard 语料应可准备")
            .into_scopes()
            .into_iter()
            .flat_map(|scope| scope.into_groups())
            .flat_map(|group| group.into_parts().1)
            .map(|asset| asset.into_parts().0)
            .collect::<Vec<_>>();

        let session = StandardCandidateSession::from_corpus(
            Arc::new(ResolvedTranslationSemantics::for_test()),
            StandardTranslationCorpus::new(groups),
        )
        .expect("人工会话应复用普通 Standard 语料顺序");
        let candidate_order = session
            .units()
            .into_iter()
            .map(|unit| unit.identity().clone())
            .collect::<Vec<_>>();

        assert_eq!(candidate_order, ordinary_order);
    }

    #[test]
    fn session_rejects_common_event_and_troop_locations_without_semantic_index() {
        for source_file in [StandardDataFile::CommonEvents, StandardDataFile::Troops] {
            let location = RpgMakerLocation::value(RpgMakerSource::data(source_file), Vec::new());
            let identity = TranslationUnitIdentity::new(
                RpgMakerStandardAssetOwner::Builtin,
                TextGroupKind::EventDialogue,
                location.clone(),
                TextUnitRole::DialogueBody,
                TextUnitContent::Lines(vec!["テスト".to_owned()]),
                r#"{"source_speaker":null}"#,
            );
            let error = StandardCandidateSession::from_corpus(
                Arc::new(ResolvedTranslationSemantics::for_test()),
                StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
                    TextGroupKind::EventDialogue,
                    location,
                    vec![StandardTranslationAsset::new(identity, None, None)],
                )]),
            )
            .err()
            .expect("缺少对象索引的 CommonEvent/Troop 不得绕过普通 Standard 校验");

            assert!(matches!(
                error,
                StandardCandidateSessionBuildError::Corpus(
                    CorpusPlanningError::MissingSemanticIndex { .. }
                )
            ));
        }
    }

    #[test]
    fn identical_current_candidate_fills_missing_family_member_without_replace() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_duplicate_family(semantics.as_ref(), Some("人工译文")),
        )
        .expect("人工候选会话应可建立");
        let prepared = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(1),
                TextUnitContent::Value("人工译文".to_owned()),
                false,
            )])
            .expect("候选应可验收");

        assert_eq!(
            prepared.results(),
            &[StandardCandidateAcceptance::Accepted {
                translation: TextUnitContent::Value("人工译文".to_owned()),
                changed_locations: 1,
            }]
        );
        assert_eq!(prepared.commits().len(), 1);
        assert_eq!(prepared.commits()[0].writes().len(), 2);
        session
            .apply_committed(&prepared)
            .expect("提交后内存状态应可同步");
        assert!(
            session
                .units()
                .iter()
                .all(|unit| unit.status() == StandardCandidateUnitStatus::Current)
        );
    }

    #[test]
    fn changing_current_family_requires_explicit_replace() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_duplicate_family(semantics.as_ref(), Some("人工译文")),
        )
        .expect("人工候选会话应可建立");
        let prepared = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(1),
                TextUnitContent::Value("另一译文".to_owned()),
                false,
            )])
            .expect("覆盖策略属于正常验收结果");

        assert_eq!(
            prepared.results(),
            &[StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::CurrentReplacementRequired,
            }]
        );
        assert!(prepared.commits().is_empty());

        let replacement = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(1),
                TextUnitContent::Value("另一译文".to_owned()),
                true,
            )])
            .expect("显式覆盖应形成原子族提交");
        assert_eq!(
            replacement.results(),
            &[StandardCandidateAcceptance::Accepted {
                translation: TextUnitContent::Value("另一译文".to_owned()),
                changed_locations: 2,
            }]
        );
        assert_eq!(replacement.commits()[0].writes().len(), 2);
    }

    #[test]
    fn conflicting_requests_for_one_family_are_all_normal_rejections() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_duplicate_family(semantics.as_ref(), None),
        )
        .expect("人工候选会话应可建立");
        let prepared = session
            .prepare_acceptance(vec![
                StandardCandidateRequest::new(
                    StandardCandidateUnitIndex::new(0),
                    TextUnitContent::Value("人工译文".to_owned()),
                    false,
                ),
                StandardCandidateRequest::new(
                    StandardCandidateUnitIndex::new(1),
                    TextUnitContent::Value("另一译文".to_owned()),
                    false,
                ),
            ])
            .expect("同族冲突属于正常验收结果");

        assert_eq!(
            prepared.results(),
            &[
                StandardCandidateAcceptance::Rejected {
                    reason: StandardCandidateRejectionReason::ConflictingCandidate,
                },
                StandardCandidateAcceptance::Rejected {
                    reason: StandardCandidateRejectionReason::ConflictingCandidate,
                },
            ]
        );
        assert!(prepared.commits().is_empty());
    }

    #[test]
    fn manual_candidate_applies_tag_constraint_from_every_family_member() {
        let session = StandardCandidateSession::from_corpus(
            Arc::new(ResolvedTranslationSemantics::for_test()),
            corpus_with_tag_family_member(),
        )
        .expect("普通位置与标签位置应形成同一人工候选族");

        let prepared = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value("魔法剑>强化".to_owned()),
                false,
            )])
            .expect("标签闭合符属于正常候选拒绝");

        assert_eq!(
            prepared.results(),
            &[StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::Candidate(
                    TranslationUnitRejectionReason::TagValueContainsClosingDelimiter {
                        line_index: 0
                    }
                ),
            }]
        );
        assert!(prepared.commits().is_empty());
    }

    #[test]
    fn explicit_replace_repairs_a_family_with_conflicting_current_translations() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_conflicting_current_family(semantics.as_ref()),
        )
        .expect("冲突 Current 不应阻止人工会话建立");

        let without_replace = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value("统一译文".to_owned()),
                false,
            )])
            .expect("覆盖策略属于正常结果");
        assert!(matches!(
            &without_replace.results()[0],
            StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::CurrentReplacementRequired
            }
        ));

        let replacement = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value("统一译文".to_owned()),
                true,
            )])
            .expect("显式覆盖应修复冲突 Current");
        assert_eq!(
            replacement.results(),
            &[StandardCandidateAcceptance::Accepted {
                translation: TextUnitContent::Value("统一译文".to_owned()),
                changed_locations: 2,
            }]
        );
        assert_eq!(replacement.commits()[0].writes().len(), 2);
    }

    #[test]
    fn structured_candidate_keeps_lines_boundary_and_reflow_policy() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let source = TextUnitContent::Lines(vec!["こんにちは".to_owned(), "世界".to_owned()]);
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::CommonEvents),
                vec![RpgMakerLocationStep::index(1)],
            ),
            TextUnitRole::DialogueBody,
            source,
            r#"{"source_speaker":null}"#,
        );
        let location = identity.group_location().clone();
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
                TextGroupKind::EventDialogue,
                location,
                vec![StandardTranslationAsset::new(identity, None, None)],
            )]),
        )
        .expect("对话 Lines 会话应可建立");

        let wrong_shape = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value("人工译文".to_owned()),
                false,
            )])
            .expect("形状错误属于正常拒绝");
        assert!(matches!(
            &wrong_shape.results()[0],
            StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::Candidate(
                    TranslationUnitRejectionReason::InvalidShape { .. }
                )
            }
        ));

        let reflow = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Lines(vec!["人工译文".to_owned()]),
                false,
            )])
            .expect("对话正文应允许改变物理行数");
        assert!(matches!(
            &reflow.results()[0],
            StandardCandidateAcceptance::Accepted {
                translation: TextUnitContent::Lines(lines),
                changed_locations: 1,
            } if lines == &["人工译文".to_owned()]
        ));
    }

    #[test]
    fn value_source_rejects_lines_candidate_without_guessing_from_content() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            corpus_with_duplicate_family(semantics.as_ref(), None),
        )
        .expect("Value 会话应可建立");
        let prepared = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Lines(vec!["人工译文".to_owned()]),
                false,
            )])
            .expect("形状错误属于正常拒绝");
        assert!(matches!(
            &prepared.results()[0],
            StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::Candidate(
                    TranslationUnitRejectionReason::InvalidShape { .. }
                )
            }
        ));
    }

    #[test]
    fn aligned_choices_and_scrolling_text_preserve_count_and_blank_slots() {
        for (kind, role) in [
            (TextGroupKind::EventChoices, TextUnitRole::Choices),
            (
                TextGroupKind::EventScrollingText,
                TextUnitRole::ScrollingText,
            ),
        ] {
            let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
            let identity = TranslationUnitIdentity::new(
                RpgMakerStandardAssetOwner::Builtin,
                kind,
                RpgMakerLocation::value(
                    RpgMakerSource::data(StandardDataFile::CommonEvents),
                    vec![RpgMakerLocationStep::index(1)],
                ),
                role,
                TextUnitContent::Lines(vec![
                    "選択一".to_owned(),
                    String::new(),
                    "選択三".to_owned(),
                ]),
                "{}",
            );
            let location = identity.group_location().clone();
            let session = StandardCandidateSession::from_corpus(
                Arc::clone(&semantics),
                StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
                    kind,
                    location,
                    vec![StandardTranslationAsset::new(identity, None, None)],
                )]),
            )
            .expect("严格对齐会话应可建立");

            let wrong_count = session
                .prepare_acceptance(vec![StandardCandidateRequest::new(
                    StandardCandidateUnitIndex::new(0),
                    TextUnitContent::Lines(vec!["译文一".to_owned(), "译文三".to_owned()]),
                    false,
                )])
                .expect("行数不符属于正常拒绝");
            assert!(matches!(
                &wrong_count.results()[0],
                StandardCandidateAcceptance::Rejected {
                    reason: StandardCandidateRejectionReason::Candidate(
                        TranslationUnitRejectionReason::LineCountMismatch {
                            expected: 3,
                            actual: 2
                        }
                    )
                }
            ));

            let wrong_blank = session
                .prepare_acceptance(vec![StandardCandidateRequest::new(
                    StandardCandidateUnitIndex::new(0),
                    TextUnitContent::Lines(vec![
                        "译文一".to_owned(),
                        "不应填充".to_owned(),
                        "译文三".to_owned(),
                    ]),
                    false,
                )])
                .expect("空槽不符属于正常拒绝");
            assert!(matches!(
                &wrong_blank.results()[0],
                StandardCandidateAcceptance::Rejected {
                    reason: StandardCandidateRejectionReason::Candidate(
                        TranslationUnitRejectionReason::BlankLineMismatch {
                            line_index: 1,
                            expected_blank: true
                        }
                    )
                }
            ));

            let accepted = session
                .prepare_acceptance(vec![StandardCandidateRequest::new(
                    StandardCandidateUnitIndex::new(0),
                    TextUnitContent::Lines(vec![
                        "译文一".to_owned(),
                        String::new(),
                        "译文三".to_owned(),
                    ]),
                    false,
                )])
                .expect("对齐候选应可验收");
            assert!(matches!(
                &accepted.results()[0],
                StandardCandidateAcceptance::Accepted {
                    changed_locations: 1,
                    ..
                }
            ));
        }
    }

    #[test]
    fn manual_candidate_uses_shared_placeholder_and_language_validation() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let source = TextUnitContent::Value(r"\C[2]テスト".to_owned());
        let identity = identity(1, source);
        let location = identity.group_location().clone();
        let session = StandardCandidateSession::from_corpus(
            Arc::clone(&semantics),
            StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                location,
                vec![StandardTranslationAsset::new(identity, None, None)],
            )]),
        )
        .expect("Placeholder 会话应可建立");

        let missing_placeholder = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value("人工译文".to_owned()),
                false,
            )])
            .expect("Placeholder 不符属于正常拒绝");
        assert!(matches!(
            &missing_placeholder.results()[0],
            StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::Candidate(
                    TranslationUnitRejectionReason::PlaceholderMismatch { .. }
                )
            }
        ));

        let source_residual = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value(r"\C[2]テスト".to_owned()),
                false,
            )])
            .expect("源语残留属于正常拒绝");
        assert!(matches!(
            &source_residual.results()[0],
            StandardCandidateAcceptance::Rejected {
                reason: StandardCandidateRejectionReason::Candidate(
                    TranslationUnitRejectionReason::SourceResidual { .. }
                )
            }
        ));

        let accepted = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                TextUnitContent::Value(r"\C[2]人工译文".to_owned()),
                false,
            )])
            .expect("原控制符应按共享规则正规化并恢复");
        assert_eq!(
            accepted.results(),
            &[StandardCandidateAcceptance::Accepted {
                translation: TextUnitContent::Value(r"\C[2]人工译文".to_owned()),
                changed_locations: 1,
            }]
        );
    }
}
