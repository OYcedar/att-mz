//! RPG Maker 翻译语料的全局确定性去重。
//!
//! 本模块只拥有“一个翻译决策对应哪些具体位置”的领域规则。它不执行 I/O、
//! 不切分任务，也不持久化关系；调用方必须先按 RPG Maker 自然顺序提供候选项。

use std::collections::{HashMap, HashSet};

use super::pipeline::{
    AppliedPlaceholder, TranslationInvalidation, TranslationPropagationTarget, TranslationReuse,
    TranslationReuseSeed, TranslationReuseTarget, TranslationStateContext, TranslationUnitIdentity,
    TranslationVirtualReason,
};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
use crate::rpg_maker::text::{
    DataFileName, RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind,
};

/// 一个已经完成语言判定和占位符保护的可翻译语义单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationDeduplicationCandidate {
    identity: TranslationUnitIdentity,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    candidate_contract: Sha256Fingerprint,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
    state_context: TranslationStateContext,
    invalidated: bool,
}

impl TranslationDeduplicationCandidate {
    // 每项参数都是去重判断直接使用的候选事实；额外参数对象只会复制本结构。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        identity: TranslationUnitIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        candidate_contract: Sha256Fingerprint,
        translation: Option<TextUnitContent>,
        translation_state: Option<Sha256Fingerprint>,
        state_context: TranslationStateContext,
        invalidated: bool,
    ) -> Self {
        Self {
            identity,
            protected_text: protected_text.into(),
            applied_placeholders,
            candidate_contract,
            translation,
            translation_state,
            state_context,
            invalidated,
        }
    }
}

/// 去重后一个可翻译语义单元的任务责任。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationDeduplicationOutcome {
    Active {
        propagation_targets: Vec<TranslationPropagationTarget>,
    },
    Virtual {
        reason: TranslationVirtualReason,
    },
}

/// 全局去重建立的任务责任和译前数据库准备。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationDeduplicationResult {
    outcomes: Vec<TranslationDeduplicationOutcome>,
    invalidations: Vec<TranslationInvalidation>,
    reuses: Vec<TranslationReuse>,
}

impl TranslationDeduplicationResult {
    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<TranslationDeduplicationOutcome>,
        Vec<TranslationInvalidation>,
        Vec<TranslationReuse>,
    ) {
        (self.outcomes, self.invalidations, self.reuses)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DeduplicationKey {
    role: DeduplicationRole,
    source_content: TextUnitContent,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    candidate_contract: Sha256Fingerprint,
}

impl DeduplicationKey {
    fn from_candidate(candidate: &TranslationDeduplicationCandidate) -> Self {
        Self {
            role: DeduplicationRole::from_identity(&candidate.identity),
            source_content: candidate.identity.source_content().clone(),
            protected_text: candidate.protected_text.clone(),
            applied_placeholders: candidate.applied_placeholders.clone(),
            candidate_contract: candidate.candidate_contract,
        }
    }
}

/// 各语义角色已经确认的精确去重边界。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DeduplicationRole {
    Speaker,
    Body {
        source_context_json: String,
    },
    Choices {
        owner: RpgMakerAssetOwner,
        group_location: RpgMakerLocation,
    },
    ScrollingText,
    Scalar {
        kind: TextGroupKind,
        source_domain: ScalarSourceDomain,
        field: ScalarFieldKey,
    },
}

/// 标量的语义来源类别；物理实例序号不参与翻译复用身份。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ScalarSourceDomain {
    Data(StandardDataFile),
    DataFile(DataFileName),
    Map,
    PluginParameter {
        plugin_name: String,
        parameter_name: String,
    },
}

impl ScalarSourceDomain {
    fn from_source(source: &RpgMakerSource) -> Self {
        match source {
            RpgMakerSource::Data(file) => Self::Data(*file),
            RpgMakerSource::DataFile(file) => Self::DataFile(file.clone()),
            RpgMakerSource::Map(_) => Self::Map,
            RpgMakerSource::PluginParameter {
                plugin_name,
                parameter_name,
                ..
            } => Self::PluginParameter {
                plugin_name: plugin_name.clone(),
                parameter_name: parameter_name.clone(),
            },
        }
    }
}

impl DeduplicationRole {
    fn from_identity(identity: &TranslationUnitIdentity) -> Self {
        match identity.role() {
            TextUnitRole::DialogueSpeaker => Self::Speaker,
            TextUnitRole::DialogueBody => Self::Body {
                source_context_json: identity.source_context_json().to_owned(),
            },
            TextUnitRole::Choices => Self::Choices {
                owner: identity.owner(),
                group_location: identity.group_location().clone(),
            },
            TextUnitRole::ScrollingText => Self::ScrollingText,
            TextUnitRole::Scalar(field) => Self::Scalar {
                kind: identity.kind(),
                source_domain: ScalarSourceDomain::from_source(identity.group_location().source()),
                field: field.clone(),
            },
        }
    }
}

struct Family {
    member_indices: Vec<usize>,
}

/// 按 RPG Maker 去重键建立自然顺序稳定的成员族。
pub(crate) fn translation_deduplication_families(
    candidates: &[TranslationDeduplicationCandidate],
) -> Vec<Vec<usize>> {
    let mut family_index = HashMap::<DeduplicationKey, usize>::with_capacity(candidates.len());
    let mut families = Vec::<Vec<usize>>::with_capacity(candidates.len());
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        let key = DeduplicationKey::from_candidate(candidate);
        let index = *family_index.entry(key).or_insert_with(|| {
            families.push(Vec::new());
            families.len() - 1
        });
        families[index].push(candidate_index);
    }
    families
}

/// 按调用方给出的稳定自然顺序建立全局去重族。
pub(crate) fn deduplicate_translation_candidates(
    candidates: Vec<TranslationDeduplicationCandidate>,
) -> TranslationDeduplicationResult {
    let families = translation_deduplication_families(&candidates)
        .into_iter()
        .map(|member_indices| Family { member_indices })
        .collect::<Vec<_>>();

    let mut outcomes = vec![None; candidates.len()];
    let invalidations = Vec::new();
    let mut reuses = Vec::new();
    for family in families {
        plan_family(&family, &candidates, &mut outcomes, &mut reuses);
    }

    TranslationDeduplicationResult {
        outcomes: outcomes
            .into_iter()
            .map(|outcome| outcome.expect("每个候选项必须恰好属于一个去重族"))
            .collect(),
        invalidations,
        reuses,
    }
}

fn plan_family(
    family: &Family,
    candidates: &[TranslationDeduplicationCandidate],
    outcomes: &mut [Option<TranslationDeduplicationOutcome>],
    reuses: &mut Vec<TranslationReuse>,
) {
    let mut current_indices = Vec::<usize>::new();
    let mut distinct_current_translations = HashSet::<&TextUnitContent>::new();
    let mut first_current_index = None;
    let mut unresolved_indices = Vec::<usize>::new();
    for &index in &family.member_indices {
        let candidate = &candidates[index];
        if !candidate.invalidated
            && let Some(translation) = candidate.translation.as_ref()
        {
            current_indices.push(index);
            if distinct_current_translations.insert(translation) {
                first_current_index.get_or_insert(index);
            }
        } else {
            unresolved_indices.push(index);
        }
    }

    for &index in &current_indices {
        outcomes[index] = Some(TranslationDeduplicationOutcome::Virtual {
            reason: TranslationVirtualReason::ExistingTranslation,
        });
    }

    if distinct_current_translations.len() == 1 {
        let seed_index = first_current_index.expect("存在 Current 译文时必须记录其自然顺序首项");
        plan_reuse_family(family, candidates, seed_index, outcomes, reuses);
    } else if !unresolved_indices.is_empty() {
        plan_active_members(&unresolved_indices, candidates, outcomes);
    }
}

fn plan_reuse_family(
    family: &Family,
    candidates: &[TranslationDeduplicationCandidate],
    seed_index: usize,
    outcomes: &mut [Option<TranslationDeduplicationOutcome>],
    reuses: &mut Vec<TranslationReuse>,
) {
    let seed = &candidates[seed_index];
    let seed_translation = seed
        .translation
        .as_ref()
        .expect("只有具有有效译文的成员才能成为复用种子");
    let mut targets = Vec::new();

    for &index in &family.member_indices {
        let candidate = &candidates[index];
        if !candidate.invalidated && candidate.translation.as_ref() == Some(seed_translation) {
            outcomes[index] = Some(TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation,
            });
            continue;
        }

        targets.push(TranslationReuseTarget::new(
            candidate.identity.clone(),
            candidate.translation.clone(),
            candidate.translation_state,
            candidate.state_context.finish(seed_translation),
        ));
        outcomes[index] = Some(TranslationDeduplicationOutcome::Virtual {
            reason: TranslationVirtualReason::Reused {
                seed: Box::new(seed.identity.clone()),
                translation: seed_translation.clone(),
            },
        });
    }

    if !targets.is_empty() {
        reuses.push(TranslationReuse::new(
            TranslationReuseSeed::new(
                seed.identity.clone(),
                seed_translation.clone(),
                seed.translation_state
                    .expect("当前译文必须同时具有 translation_state"),
            ),
            targets,
        ));
    }
}

fn plan_active_members(
    member_indices: &[usize],
    candidates: &[TranslationDeduplicationCandidate],
    outcomes: &mut [Option<TranslationDeduplicationOutcome>],
) {
    let leader_index = member_indices[0];
    let leader = &candidates[leader_index];
    let propagation_targets = member_indices
        .iter()
        .copied()
        .filter(|&index| index != leader_index)
        .map(|index| {
            TranslationPropagationTarget::with_previous(
                candidates[index].identity.clone(),
                candidates[index].state_context,
                candidates[index].translation.clone(),
                candidates[index].translation_state,
            )
        })
        .collect();
    outcomes[leader_index] = Some(TranslationDeduplicationOutcome::Active {
        propagation_targets,
    });

    for &index in member_indices {
        if index == leader_index {
            continue;
        }
        outcomes[index] = Some(TranslationDeduplicationOutcome::Virtual {
            reason: TranslationVirtualReason::Duplicate {
                leader: Box::new(leader.identity.clone()),
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::text::{
        DataFileName, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
        TextGroupKind,
    };

    use super::*;
    use crate::rpg_maker::translate::pipeline::{PlaceholderRuleOrigin, PlaceholderSegment};

    fn fingerprint(marker: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([marker; 32])
    }

    fn state_context(marker: u8) -> TranslationStateContext {
        TranslationStateContext::new(fingerprint(marker))
    }

    fn value(text: &str) -> TextUnitContent {
        TextUnitContent::Value(text.to_owned())
    }

    fn lines(values: &[&str]) -> TextUnitContent {
        TextUnitContent::Lines(values.iter().map(|value| (*value).to_owned()).collect())
    }

    fn identity(
        kind: TextGroupKind,
        source: RpgMakerSource,
        group_index: usize,
        role: TextUnitRole,
        source_content: TextUnitContent,
        source_context_json: &str,
    ) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            kind,
            RpgMakerLocation::value(source, vec![RpgMakerLocationStep::index(group_index)]),
            role,
            source_content,
            source_context_json,
        )
    }

    fn scalar_identity(
        file: StandardDataFile,
        group_index: usize,
        field: &str,
        source_text: &str,
    ) -> TranslationUnitIdentity {
        identity(
            TextGroupKind::DatabaseEntry,
            RpgMakerSource::data(file),
            group_index,
            TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("字段键应合法")),
            value(source_text),
            "{}",
        )
    }

    fn dialogue_identity(
        group_index: usize,
        role: TextUnitRole,
        source_content: TextUnitContent,
        source_context_json: &str,
    ) -> TranslationUnitIdentity {
        identity(
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(1),
            group_index,
            role,
            source_content,
            source_context_json,
        )
    }

    struct StoredTranslation {
        content: TextUnitContent,
        state: Sha256Fingerprint,
    }

    impl StoredTranslation {
        fn new(content: TextUnitContent, state: Sha256Fingerprint) -> Self {
            Self { content, state }
        }
    }

    fn candidate(
        identity: TranslationUnitIdentity,
        protected_text: &str,
        placeholders: Vec<AppliedPlaceholder>,
        stored_translation: Option<StoredTranslation>,
        state_context: TranslationStateContext,
        invalidated: bool,
    ) -> TranslationDeduplicationCandidate {
        let (translation, translation_state) = stored_translation
            .map(|stored| (Some(stored.content), Some(stored.state)))
            .unwrap_or_default();
        TranslationDeduplicationCandidate::new(
            identity,
            protected_text,
            placeholders,
            fingerprint(0),
            translation,
            translation_state,
            state_context,
            invalidated,
        )
    }

    fn placeholder(scope: &str) -> AppliedPlaceholder {
        AppliedPlaceholder::new(
            "⟦ATT_00000000_00000000⟧",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            scope,
            PlaceholderSegment::Whole,
        )
    }

    #[test]
    fn family_without_current_seed_uses_first_unit_and_propagates_in_natural_order() {
        let original = "<Help:保存しますか？>";
        let first = scalar_identity(StandardDataFile::Items, 1, "name", original);
        let second = scalar_identity(StandardDataFile::Items, 2, "name", original);
        let third = scalar_identity(StandardDataFile::Items, 3, "name", original);
        let second_context = state_context(2);
        let third_context = state_context(3);
        let result = deduplicate_translation_candidates(vec![
            candidate(
                first.clone(),
                original,
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                second.clone(),
                original,
                Vec::new(),
                None,
                second_context,
                false,
            ),
            candidate(
                third.clone(),
                original,
                Vec::new(),
                None,
                third_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert_eq!(
            outcomes[0],
            TranslationDeduplicationOutcome::Active {
                propagation_targets: vec![
                    TranslationPropagationTarget::new(second, second_context),
                    TranslationPropagationTarget::new(third, third_context),
                ],
            }
        );
        assert_eq!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate {
                    leader: Box::new(first.clone())
                }
            }
        );
        assert_eq!(
            outcomes[2],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate {
                    leader: Box::new(first)
                }
            }
        );
    }

    #[test]
    fn zero_match_candidate_contract_prevents_unsafe_propagation() {
        let first = scalar_identity(StandardDataFile::Items, 1, "name", "same source");
        let second = scalar_identity(StandardDataFile::Items, 2, "name", "same source");
        let first = candidate(
            first,
            "same source",
            Vec::new(),
            None,
            state_context(1),
            false,
        );
        let mut second = candidate(
            second,
            "same source",
            Vec::new(),
            None,
            state_context(2),
            false,
        );
        second.candidate_contract = fingerprint(1);

        let (outcomes, invalidations, reuses) =
            deduplicate_translation_candidates(vec![first, second]).into_parts();
        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn different_complete_angle_bracket_values_never_share_a_family() {
        let first = scalar_identity(StandardDataFile::Items, 1, "note", "<Help:甲>");
        let second = scalar_identity(StandardDataFile::Items, 2, "note", "<Help:乙>");

        let result = deduplicate_translation_candidates(vec![
            candidate(
                first,
                "<Help:甲>",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                second,
                "<Help:乙>",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn current_state_translation_becomes_a_reuse_seed() {
        let seed_context = state_context(1);
        let target_context = state_context(2);
        let translation = value("<Help:Save>");
        let seed_state = seed_context.finish(&translation);
        let seed = scalar_identity(StandardDataFile::Items, 1, "name", "保存");
        let target = scalar_identity(StandardDataFile::Items, 2, "name", "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                seed.clone(),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(translation.clone(), seed_state)),
                seed_context,
                false,
            ),
            candidate(
                target.clone(),
                "保存",
                Vec::new(),
                None,
                target_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &seed);
        assert_eq!(reuses[0].seed().expected_translation(), &translation);
        assert_eq!(reuses[0].seed().expected_translation_state(), seed_state);
        assert_eq!(reuses[0].targets()[0].identity(), &target);
        assert_eq!(reuses[0].targets()[0].expected_translation(), None);
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            target_context.finish(&translation)
        );
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert!(matches!(
            &outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed: actual, .. }
            } if actual.as_ref() == &seed
        ));
    }

    #[test]
    fn earliest_equal_current_translation_is_the_deterministic_seed() {
        let earliest_context = state_context(1);
        let later_context = state_context(2);
        let target_context = state_context(3);
        let translation = value("保存");
        let earliest_seed = scalar_identity(StandardDataFile::Items, 1, "name", "保存");
        let later_seed = scalar_identity(StandardDataFile::Items, 2, "name", "保存");
        let target = scalar_identity(StandardDataFile::Items, 3, "name", "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                earliest_seed.clone(),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    translation.clone(),
                    earliest_context.finish(&translation),
                )),
                earliest_context,
                false,
            ),
            candidate(
                later_seed,
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    translation.clone(),
                    later_context.finish(&translation),
                )),
                later_context,
                false,
            ),
            candidate(
                target.clone(),
                "保存",
                Vec::new(),
                None,
                target_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &earliest_seed);
        assert_eq!(reuses[0].targets().len(), 1);
        assert_eq!(reuses[0].targets()[0].identity(), &target);
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            target_context.finish(&translation)
        );
        assert!(matches!(
            outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert!(matches!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert!(matches!(
            &outcomes[2],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed, .. }
            } if seed.as_ref() == &earliest_seed
        ));
    }

    #[test]
    fn current_seed_atomically_overwrites_a_stale_target() {
        let stale_context = state_context(1);
        let current_context = state_context(2);
        let stale_translation = value("旧译文");
        let current_translation = value("保存");
        let stale_state = fingerprint(91);
        let current_state = current_context.finish(&current_translation);
        let stale_target = scalar_identity(StandardDataFile::Items, 1, "name", "保存");
        let current_seed = scalar_identity(StandardDataFile::Items, 2, "name", "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                stale_target.clone(),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    stale_translation.clone(),
                    stale_state,
                )),
                stale_context,
                true,
            ),
            candidate(
                current_seed.clone(),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    current_translation.clone(),
                    current_state,
                )),
                current_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &current_seed);
        assert_eq!(reuses[0].seed().expected_translation_state(), current_state);
        assert_eq!(reuses[0].targets().len(), 1);
        assert_eq!(reuses[0].targets()[0].identity(), &stale_target);
        assert_eq!(
            reuses[0].targets()[0].expected_translation(),
            Some(&stale_translation)
        );
        assert_eq!(
            reuses[0].targets()[0].expected_translation_state(),
            Some(stale_state)
        );
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            stale_context.finish(&current_translation)
        );
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed, .. }
            } if seed.as_ref() == &current_seed
        ));
        assert!(matches!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
    }

    #[test]
    fn different_current_translations_coexist_without_reuse_or_model_work() {
        let first_context = state_context(1);
        let second_context = state_context(2);
        let first_translation = value("Save");
        let second_translation = value("Store");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar_identity(StandardDataFile::Items, 1, "name", "保存"),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    first_translation.clone(),
                    first_context.finish(&first_translation),
                )),
                first_context,
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Items, 2, "name", "保存"),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    second_translation.clone(),
                    second_context.finish(&second_translation),
                )),
                second_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        )));
    }

    #[test]
    fn many_different_current_translations_coexist_without_quadratic_comparison() {
        const CURRENT_COUNT: usize = 4_096;

        let candidates = (0..CURRENT_COUNT)
            .map(|index| {
                let context = state_context((index % 251 + 1) as u8);
                let translation = value(&format!("译文-{index}"));
                candidate(
                    scalar_identity(StandardDataFile::Items, index, "name", "私"),
                    "私",
                    Vec::new(),
                    Some(StoredTranslation::new(
                        translation.clone(),
                        context.finish(&translation),
                    )),
                    context,
                    false,
                )
            })
            .collect();
        let result = deduplicate_translation_candidates(candidates);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert_eq!(outcomes.len(), CURRENT_COUNT);
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        )));
    }

    #[test]
    fn different_current_translations_leave_missing_members_for_a_new_decision() {
        let first_context = state_context(1);
        let second_context = state_context(2);
        let missing_context = state_context(3);
        let other_missing_context = state_context(4);
        let first_translation = value("我");
        let second_translation = value("小女子");
        let missing = scalar_identity(StandardDataFile::Items, 3, "name", "私");
        let other_missing = scalar_identity(StandardDataFile::Items, 4, "name", "私");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar_identity(StandardDataFile::Items, 1, "name", "私"),
                "私",
                Vec::new(),
                Some(StoredTranslation::new(
                    first_translation.clone(),
                    first_context.finish(&first_translation),
                )),
                first_context,
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Items, 2, "name", "私"),
                "私",
                Vec::new(),
                Some(StoredTranslation::new(
                    second_translation.clone(),
                    second_context.finish(&second_translation),
                )),
                second_context,
                false,
            ),
            candidate(
                missing.clone(),
                "私",
                Vec::new(),
                None,
                missing_context,
                false,
            ),
            candidate(
                other_missing.clone(),
                "私",
                Vec::new(),
                None,
                other_missing_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(matches!(
            outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert!(matches!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation
            }
        ));
        assert_eq!(
            outcomes[2],
            TranslationDeduplicationOutcome::Active {
                propagation_targets: vec![TranslationPropagationTarget::new(
                    other_missing,
                    other_missing_context,
                )],
            }
        );
        assert_eq!(
            outcomes[3],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate {
                    leader: Box::new(missing),
                },
            }
        );
    }

    #[test]
    fn placeholder_contracts_are_part_of_the_exact_family_identity() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar_identity(StandardDataFile::Items, 1, "name", "\\N[1]"),
                "⟦ATT_00000000_00000000⟧",
                vec![placeholder("database_entry")],
                None,
                state_context(1),
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Items, 2, "name", "\\N[1]"),
                "⟦ATT_00000000_00000000⟧",
                vec![placeholder("event_dialogue")],
                None,
                state_context(2),
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn complete_dialogue_sequence_and_source_speaker_define_the_family() {
        let same = lines(&["同", "一句"]);
        let result = deduplicate_translation_candidates(vec![
            candidate(
                dialogue_identity(
                    1,
                    TextUnitRole::DialogueBody,
                    same.clone(),
                    r#"{"source_speaker":"甲"}"#,
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                dialogue_identity(
                    2,
                    TextUnitRole::DialogueBody,
                    same,
                    r#"{"source_speaker":"甲"}"#,
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
            candidate(
                dialogue_identity(
                    3,
                    TextUnitRole::DialogueBody,
                    lines(&["同", "一句"]),
                    r#"{"source_speaker":"乙"}"#,
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
            candidate(
                dialogue_identity(
                    4,
                    TextUnitRole::DialogueBody,
                    lines(&["同一句"]),
                    r#"{"source_speaker":"甲"}"#,
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(4),
                false,
            ),
            candidate(
                dialogue_identity(
                    5,
                    TextUnitRole::DialogueBody,
                    lines(&["同", "", "一句"]),
                    r#"{"source_speaker":"甲"}"#,
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(5),
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.len() == 1
                    && propagation_targets[0].identity().group_location()
                        == &RpgMakerLocation::value(
                            RpgMakerSource::map(1),
                            vec![RpgMakerLocationStep::index(2)],
                        )
        ));
        assert!(outcomes[2..].iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn choices_never_deduplicate_across_groups() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                identity(
                    TextGroupKind::EventChoices,
                    RpgMakerSource::map(1),
                    1,
                    TextUnitRole::Choices,
                    lines(&["はい", "いいえ"]),
                    "{}",
                ),
                "はい\nいいえ",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                identity(
                    TextGroupKind::EventChoices,
                    RpgMakerSource::map(1),
                    2,
                    TextUnitRole::Choices,
                    lines(&["はい", "いいえ"]),
                    "{}",
                ),
                "はい\nいいえ",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn scrolling_text_deduplicates_only_the_exact_ordered_sequence() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                identity(
                    TextGroupKind::EventScrollingText,
                    RpgMakerSource::map(1),
                    1,
                    TextUnitRole::ScrollingText,
                    lines(&["制作", "", "甲"]),
                    "{}",
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                identity(
                    TextGroupKind::EventScrollingText,
                    RpgMakerSource::map(2),
                    2,
                    TextUnitRole::ScrollingText,
                    lines(&["制作", "", "甲"]),
                    "{}",
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
            candidate(
                identity(
                    TextGroupKind::EventScrollingText,
                    RpgMakerSource::map(3),
                    3,
                    TextUnitRole::ScrollingText,
                    lines(&["制作", "甲", ""]),
                    "{}",
                ),
                "相同保护结果",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[TranslationPropagationTarget::new(
                    identity(
                        TextGroupKind::EventScrollingText,
                        RpgMakerSource::map(2),
                        2,
                        TextUnitRole::ScrollingText,
                        lines(&["制作", "", "甲"]),
                        "{}",
                    ),
                    state_context(2),
                )]
        ));
        assert!(matches!(
            &outcomes[2],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.is_empty()
        ));
    }

    #[test]
    fn scalar_semantic_domain_and_field_are_part_of_the_family() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar_identity(StandardDataFile::Items, 1, "name", "共通"),
                "共通",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Items, 2, "name", "共通"),
                "共通",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Skills, 1, "name", "共通"),
                "共通",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
            candidate(
                scalar_identity(StandardDataFile::Items, 3, "description", "共通"),
                "共通",
                Vec::new(),
                None,
                state_context(4),
                false,
            ),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.len() == 1
        ));
        assert!(outcomes[2..].iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn scalar_semantic_domain_ignores_physical_map_and_plugin_indexes() {
        let scalar = |kind, source, index, field: &str| {
            identity(
                kind,
                source,
                index,
                TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("字段键应合法")),
                value("共通"),
                "{}",
            )
        };
        let map_target = scalar(TextGroupKind::Map, RpgMakerSource::map(9), 2, "displayName");
        let plugin_target = scalar(
            TextGroupKind::PluginParameter,
            RpgMakerSource::plugin_parameter(8, "MenuCore", "title"),
            5,
            "text",
        );
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar(TextGroupKind::Map, RpgMakerSource::map(1), 1, "displayName"),
                "共通",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                map_target.clone(),
                "共通",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::EventCommand,
                    RpgMakerSource::map(2),
                    3,
                    "displayName",
                ),
                "共通",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::PluginParameter,
                    RpgMakerSource::plugin_parameter(1, "MenuCore", "title"),
                    4,
                    "text",
                ),
                "共通",
                Vec::new(),
                None,
                state_context(4),
                false,
            ),
            candidate(
                plugin_target.clone(),
                "共通",
                Vec::new(),
                None,
                state_context(5),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::PluginParameter,
                    RpgMakerSource::plugin_parameter(9, "MenuCore", "subtitle"),
                    6,
                    "text",
                ),
                "共通",
                Vec::new(),
                None,
                state_context(6),
                false,
            ),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[TranslationPropagationTarget::new(
                    map_target,
                    state_context(2),
                )]
        ));
        assert!(matches!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate { .. }
            }
        ));
        assert!(matches!(
            &outcomes[2],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.is_empty()
        ));
        assert!(matches!(
            &outcomes[3],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[TranslationPropagationTarget::new(
                    plugin_target,
                    state_context(5),
                )]
        ));
        assert!(matches!(
            outcomes[4],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate { .. }
            }
        ));
        assert!(matches!(
            &outcomes[5],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.is_empty()
        ));
    }

    #[test]
    fn scalar_semantic_domain_preserves_custom_file_and_plugin_names() {
        let scalar = |kind, source, index| {
            identity(
                kind,
                source,
                index,
                TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
                value("共通"),
                "{}",
            )
        };
        let result = deduplicate_translation_candidates(vec![
            candidate(
                scalar(
                    TextGroupKind::DatabaseEntry,
                    RpgMakerSource::data_file(
                        DataFileName::parse("Quests.json").expect("自定义文件名应合法"),
                    ),
                    1,
                ),
                "共通",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::DatabaseEntry,
                    RpgMakerSource::data_file(
                        DataFileName::parse("Bestiary.json").expect("自定义文件名应合法"),
                    ),
                    2,
                ),
                "共通",
                Vec::new(),
                None,
                state_context(2),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::PluginParameter,
                    RpgMakerSource::plugin_parameter(1, "MenuCore", "title"),
                    3,
                ),
                "共通",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
            candidate(
                scalar(
                    TextGroupKind::PluginParameter,
                    RpgMakerSource::plugin_parameter(2, "BattleCore", "title"),
                    4,
                ),
                "共通",
                Vec::new(),
                None,
                state_context(4),
                false,
            ),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn speaker_values_deduplicate_globally() {
        let first = dialogue_identity(1, TextUnitRole::DialogueSpeaker, value("アリス"), "{}");
        let second = identity(
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(9),
            2,
            TextUnitRole::DialogueSpeaker,
            value("アリス"),
            "{}",
        );
        let result = deduplicate_translation_candidates(vec![
            candidate(first, "アリス", Vec::new(), None, state_context(1), false),
            candidate(second, "アリス", Vec::new(), None, state_context(2), false),
        ]);
        let (outcomes, _, _) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.len() == 1
        ));
    }

    #[test]
    fn stale_translation_without_current_seed_is_retained_for_atomic_replacement() {
        let stale_context = state_context(1);
        let pending_context = state_context(2);
        let stale_translation = value("旧译文");
        let stale_state = fingerprint(81);
        let first = scalar_identity(StandardDataFile::Items, 1, "name", "保存");
        let second = scalar_identity(StandardDataFile::Items, 2, "name", "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                first.clone(),
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    stale_translation.clone(),
                    stale_state,
                )),
                stale_context,
                true,
            ),
            candidate(
                second.clone(),
                "保存",
                Vec::new(),
                None,
                pending_context,
                false,
            ),
        ]);
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[TranslationPropagationTarget::new(
                    second,
                    pending_context,
                )]
        ));
        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
    }
}
