//! 标准翻译语料的全局确定性去重。
//!
//! 本模块只拥有“一个翻译决策对应哪些具体位置”的领域规则。它不执行 I/O、
//! 不切分任务，也不持久化关系；调用方必须先按 RPG Maker 自然顺序提供候选项。

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use super::standard::{
    AppliedPlaceholder, TranslationInvalidation, TranslationLeafIdentity,
    TranslationPropagationTarget, TranslationReuse, TranslationReuseSeed, TranslationReuseTarget,
    TranslationStateContext, TranslationVirtualReason,
};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::model::TextFieldRole;

/// 一个已经完成语言判定和占位符保护的可翻译叶子。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationDeduplicationCandidate {
    identity: TranslationLeafIdentity,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
    state_context: TranslationStateContext,
    invalidated: bool,
}

impl TranslationDeduplicationCandidate {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        translation: Option<String>,
        translation_state: Option<Sha256Fingerprint>,
        state_context: TranslationStateContext,
        invalidated: bool,
    ) -> Self {
        Self {
            identity,
            protected_text: protected_text.into(),
            applied_placeholders,
            translation,
            translation_state,
            state_context,
            invalidated,
        }
    }
}

/// 去重后一个可翻译叶子的任务责任。
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
    original_text: String,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
}

impl DeduplicationKey {
    fn from_candidate(candidate: &TranslationDeduplicationCandidate) -> Self {
        Self {
            role: DeduplicationRole::from_identity(&candidate.identity),
            original_text: candidate.identity.original_text().to_owned(),
            protected_text: candidate.protected_text.clone(),
            applied_placeholders: candidate.applied_placeholders.clone(),
        }
    }
}

/// 只表达姓名投影已经证明需要的去重边界。
///
/// Speaker 可以跨组复用；Body 可以跨组复用，但必须具有相同的源 Speaker
/// 上下文。其余叶子沿用既有按文本语义去重的行为。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DeduplicationRole {
    Speaker,
    Body { translation_context_json: String },
    Other,
}

impl DeduplicationRole {
    fn from_identity(identity: &TranslationLeafIdentity) -> Self {
        match identity.role() {
            TextFieldRole::DialogueSpeaker => Self::Speaker,
            TextFieldRole::DialogueBody { .. } => Self::Body {
                translation_context_json: identity.translation_context_json().to_owned(),
            },
            TextFieldRole::Scalar(_) | TextFieldRole::ScrollingTextBody { .. } => Self::Other,
        }
    }
}

struct Family {
    original_text: String,
    member_indices: Vec<usize>,
}

/// 按调用方给出的稳定自然顺序建立全局去重族。
pub(crate) fn deduplicate_translation_candidates(
    candidates: Vec<TranslationDeduplicationCandidate>,
) -> Result<TranslationDeduplicationResult, TranslationDeduplicationError> {
    let mut family_index = HashMap::<DeduplicationKey, usize>::new();
    let mut families = Vec::<Family>::new();
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        let key = DeduplicationKey::from_candidate(candidate);
        let index = *family_index.entry(key).or_insert_with(|| {
            families.push(Family {
                original_text: candidate.identity.original_text().to_owned(),
                member_indices: Vec::new(),
            });
            families.len() - 1
        });
        families[index].member_indices.push(candidate_index);
    }

    let mut outcomes = vec![None; candidates.len()];
    let mut invalidations = Vec::new();
    let mut reuses = Vec::new();
    for family in families {
        plan_family(
            &family,
            &candidates,
            &mut outcomes,
            &mut invalidations,
            &mut reuses,
        )?;
    }

    Ok(TranslationDeduplicationResult {
        outcomes: outcomes
            .into_iter()
            .map(|outcome| outcome.expect("每个候选项必须恰好属于一个去重族"))
            .collect(),
        invalidations,
        reuses,
    })
}

fn plan_family(
    family: &Family,
    candidates: &[TranslationDeduplicationCandidate],
    outcomes: &mut [Option<TranslationDeduplicationOutcome>],
    invalidations: &mut Vec<TranslationInvalidation>,
    reuses: &mut Vec<TranslationReuse>,
) -> Result<(), TranslationDeduplicationError> {
    let mut valid_translations = BTreeMap::<&str, Vec<usize>>::new();
    for &index in &family.member_indices {
        let candidate = &candidates[index];
        if !candidate.invalidated
            && let Some(translation) = candidate.translation.as_deref()
        {
            valid_translations
                .entry(translation)
                .or_default()
                .push(index);
        }
    }

    if valid_translations.len() > 1 {
        let conflicts = family
            .member_indices
            .iter()
            .filter_map(|&index| {
                let candidate = &candidates[index];
                (!candidate.invalidated).then(|| {
                    candidate.translation.as_ref().map(|translation| {
                        ConflictingReusableTranslation::new(
                            candidate.identity.clone(),
                            translation.clone(),
                        )
                    })
                })?
            })
            .collect();
        return Err(
            TranslationDeduplicationError::ConflictingReusableTranslations {
                original_text: family.original_text.clone(),
                conflicts,
            },
        );
    }

    if let Some(seed_index) = valid_translations
        .values()
        .next()
        .and_then(|indices| indices.first())
        .copied()
    {
        plan_reuse_family(family, candidates, seed_index, outcomes, reuses);
    } else {
        plan_active_family(family, candidates, outcomes, invalidations);
    }
    Ok(())
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
        .as_deref()
        .expect("只有具有有效译文的成员才能成为复用种子");
    let mut targets = Vec::new();

    for &index in &family.member_indices {
        let candidate = &candidates[index];
        if !candidate.invalidated && candidate.translation.as_deref() == Some(seed_translation) {
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
            },
        });
    }

    if !targets.is_empty() {
        reuses.push(TranslationReuse::new(
            TranslationReuseSeed::new(
                seed.identity.clone(),
                seed_translation,
                seed.translation_state
                    .expect("当前译文必须同时具有 translation_state"),
            ),
            targets,
        ));
    }
}

fn plan_active_family(
    family: &Family,
    candidates: &[TranslationDeduplicationCandidate],
    outcomes: &mut [Option<TranslationDeduplicationOutcome>],
    invalidations: &mut Vec<TranslationInvalidation>,
) {
    let leader_index = family.member_indices[0];
    let leader = &candidates[leader_index];
    let propagation_targets = family.member_indices[1..]
        .iter()
        .map(|&index| {
            TranslationPropagationTarget::new(
                candidates[index].identity.clone(),
                candidates[index].state_context,
            )
        })
        .collect();
    outcomes[leader_index] = Some(TranslationDeduplicationOutcome::Active {
        propagation_targets,
    });

    for &index in &family.member_indices[1..] {
        outcomes[index] = Some(TranslationDeduplicationOutcome::Virtual {
            reason: TranslationVirtualReason::Duplicate {
                leader: Box::new(leader.identity.clone()),
            },
        });
    }

    for &index in &family.member_indices {
        let candidate = &candidates[index];
        if candidate.invalidated {
            invalidations.push(TranslationInvalidation::new(
                candidate.identity.clone(),
                candidate
                    .translation
                    .as_deref()
                    .expect("只有已有译文的候选项才可能失效"),
                candidate
                    .translation_state
                    .expect("已有译文必须同时具有 translation_state"),
            ));
        }
    }
}

/// 同一可安全复用族内的一条冲突译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConflictingReusableTranslation {
    identity: TranslationLeafIdentity,
    translation: String,
}

impl ConflictingReusableTranslation {
    fn new(identity: TranslationLeafIdentity, translation: String) -> Self {
        Self {
            identity,
            translation,
        }
    }
}

/// 当前一致语料无法建立唯一复用译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationDeduplicationError {
    ConflictingReusableTranslations {
        original_text: String,
        conflicts: Vec<ConflictingReusableTranslation>,
    },
}

impl fmt::Display for TranslationDeduplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingReusableTranslations {
                original_text,
                conflicts,
            } => {
                write!(
                    formatter,
                    "原文 {original_text:?} 存在 {} 个互相冲突的有效译文：",
                    conflicts.len()
                )?;
                for (index, conflict) in conflicts.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("；")?;
                    }
                    write!(
                        formatter,
                        "{} => {:?}",
                        format_logical_location(&conflict.identity),
                        conflict.translation
                    )?;
                }
                Ok(())
            }
        }
    }
}

fn format_logical_location(identity: &TranslationLeafIdentity) -> String {
    format!("{} / {:?}", identity.group_location(), identity.role())
}

impl Error for TranslationDeduplicationError {}

#[cfg(test)]
mod tests {
    use crate::rpg_maker::model::{ScalarFieldKey, TextFieldRole};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };

    use super::*;
    use crate::rpg_maker::translate::standard::{PlaceholderRuleOrigin, PlaceholderSegment};

    fn fingerprint(marker: u8) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([marker; 32])
    }

    fn state_context(marker: u8) -> TranslationStateContext {
        TranslationStateContext::new(fingerprint(marker))
    }

    fn identity(index: usize, original: &str) -> TranslationLeafIdentity {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(index)],
        );
        TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location,
            TextFieldRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            original,
            "{}",
        )
    }

    struct StoredTranslation<'a> {
        text: &'a str,
        state: Sha256Fingerprint,
    }

    impl<'a> StoredTranslation<'a> {
        const fn new(text: &'a str, state: Sha256Fingerprint) -> Self {
            Self { text, state }
        }
    }

    fn candidate(
        index: usize,
        original: &str,
        protected_text: &str,
        placeholders: Vec<AppliedPlaceholder>,
        stored_translation: Option<StoredTranslation<'_>>,
        state_context: TranslationStateContext,
        invalidated: bool,
    ) -> TranslationDeduplicationCandidate {
        let (translation, translation_state) = stored_translation
            .map(|stored| (Some(stored.text.to_owned()), Some(stored.state)))
            .unwrap_or_default();
        TranslationDeduplicationCandidate::new(
            identity(index, original),
            protected_text,
            placeholders,
            translation,
            translation_state,
            state_context,
            invalidated,
        )
    }

    fn dialogue_candidate(
        group_index: usize,
        role: TextFieldRole,
        original: &str,
        translation_context_json: &str,
        context_marker: u8,
    ) -> TranslationDeduplicationCandidate {
        let identity = TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(
                RpgMakerSource::map(1),
                vec![
                    RpgMakerLocationStep::key("events"),
                    RpgMakerLocationStep::index(group_index),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(0),
                ],
            ),
            role,
            original,
            translation_context_json,
        );
        TranslationDeduplicationCandidate::new(
            identity,
            original,
            Vec::new(),
            None,
            None,
            state_context(context_marker),
            false,
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
    fn family_without_current_seed_uses_first_member_and_propagates_in_natural_order() {
        let first = identity(1, "保存しますか？");
        let second = identity(2, "保存しますか？");
        let third = identity(3, "保存しますか？");
        let second_context = state_context(2);
        let third_context = state_context(3);
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存しますか？",
                "保存しますか？",
                Vec::new(),
                None,
                state_context(1),
                false,
            ),
            candidate(
                2,
                "保存しますか？",
                "保存しますか？",
                Vec::new(),
                None,
                second_context,
                false,
            ),
            candidate(
                3,
                "保存しますか？",
                "保存しますか？",
                Vec::new(),
                None,
                third_context,
                false,
            ),
        ])
        .expect("没有当前译文时应建立唯一模型责任");
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
    fn current_state_translation_becomes_a_reuse_seed() {
        let seed_context = state_context(1);
        let target_context = state_context(2);
        let seed_state = seed_context.finish("Save");
        let seed = identity(1, "保存");
        let target = identity(2, "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("Save", seed_state)),
                seed_context,
                false,
            ),
            candidate(2, "保存", "保存", Vec::new(), None, target_context, false),
        ])
        .expect("当前译文应直接复用");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &seed);
        assert_eq!(reuses[0].seed().expected_translation(), "Save");
        assert_eq!(reuses[0].seed().expected_translation_state(), seed_state);
        assert_eq!(reuses[0].targets()[0].identity(), &target);
        assert_eq!(reuses[0].targets()[0].expected_translation(), None);
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            target_context.finish("Save")
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
                reason: TranslationVirtualReason::Reused { seed: actual }
            } if actual.as_ref() == &seed
        ));
    }

    #[test]
    fn earliest_equal_current_translation_is_the_deterministic_seed() {
        let earliest_context = state_context(1);
        let later_context = state_context(2);
        let target_context = state_context(3);
        let earliest_seed = identity(1, "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    "保存",
                    earliest_context.finish("保存"),
                )),
                earliest_context,
                false,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("保存", later_context.finish("保存"))),
                later_context,
                false,
            ),
            candidate(3, "保存", "保存", Vec::new(), None, target_context, false),
        ])
        .expect("相同当前译文不构成冲突");
        let (_, _, reuses) = result.into_parts();

        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &earliest_seed);
        assert_eq!(reuses[0].targets().len(), 1);
        assert_eq!(reuses[0].targets()[0].identity(), &identity(3, "保存"));
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            target_context.finish("保存")
        );
    }

    #[test]
    fn stale_translation_is_overwritten_when_a_current_seed_exists() {
        let stale_context = state_context(1);
        let current_context = state_context(2);
        let stale_state = fingerprint(91);
        let current_state = current_context.finish("保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("旧译文", stale_state)),
                stale_context,
                true,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("保存", current_state)),
                current_context,
                false,
            ),
        ])
        .expect("失效译文不能成为种子，但可以由当前种子覆盖");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &identity(2, "保存"));
        assert_eq!(reuses[0].seed().expected_translation_state(), current_state);
        assert_eq!(
            reuses[0].targets()[0].expected_translation(),
            Some("旧译文")
        );
        assert_eq!(
            reuses[0].targets()[0].expected_translation_state(),
            Some(stale_state)
        );
        assert_eq!(
            reuses[0].targets()[0].replacement_translation_state(),
            stale_context.finish("保存")
        );
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed }
            } if seed.as_ref() == &identity(2, "保存")
        ));
    }

    #[test]
    fn conflicting_current_translations_fail_before_a_plan_exists() {
        let first_context = state_context(1);
        let second_context = state_context(2);
        let error = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("Save", first_context.finish("Save"))),
                first_context,
                false,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new(
                    "Store",
                    second_context.finish("Store"),
                )),
                second_context,
                false,
            ),
        ])
        .expect_err("同族当前译文冲突必须显式失败");

        assert!(matches!(
            error,
            TranslationDeduplicationError::ConflictingReusableTranslations {
                conflicts,
                ..
            } if conflicts.len() == 2
        ));
    }

    #[test]
    fn placeholder_contracts_are_part_of_the_exact_family_identity() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "\\N[1]",
                "⟦ATT_00000000_00000000⟧",
                vec![placeholder("database_entry")],
                None,
                state_context(1),
                false,
            ),
            candidate(
                2,
                "\\N[1]",
                "⟦ATT_00000000_00000000⟧",
                vec![placeholder("event_dialogue")],
                None,
                state_context(2),
                false,
            ),
        ])
        .expect("不同精确占位符契约必须形成独立去重族");
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
    fn dialogue_roles_and_source_speaker_context_define_deduplication_families() {
        let result = deduplicate_translation_candidates(vec![
            dialogue_candidate(1, TextFieldRole::DialogueSpeaker, "同一句", "{}", 1),
            dialogue_candidate(
                1,
                TextFieldRole::DialogueBody { index: 0 },
                "同一句",
                r#"{"source_speaker":"甲"}"#,
                2,
            ),
            dialogue_candidate(
                2,
                TextFieldRole::DialogueBody { index: 0 },
                "同一句",
                r#"{"source_speaker":"甲"}"#,
                3,
            ),
            dialogue_candidate(
                3,
                TextFieldRole::DialogueBody { index: 0 },
                "同一句",
                r#"{"source_speaker":"乙"}"#,
                4,
            ),
            dialogue_candidate(4, TextFieldRole::DialogueSpeaker, "同一句", "{}", 5),
        ])
        .expect("姓名和正文应按强角色及源姓名上下文去重");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.len() == 1
                    && propagation_targets[0].identity().role()
                        == &TextFieldRole::DialogueSpeaker
        ));
        assert!(matches!(
            &outcomes[1],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets.len() == 1
                    && propagation_targets[0].identity().group_location()
                        == &RpgMakerLocation::value(
                            RpgMakerSource::map(1),
                            vec![
                                RpgMakerLocationStep::key("events"),
                                RpgMakerLocationStep::index(2),
                                RpgMakerLocationStep::key("list"),
                                RpgMakerLocationStep::index(0),
                            ],
                        )
        ));
        assert!(matches!(
            outcomes[3],
            TranslationDeduplicationOutcome::Active {
                ref propagation_targets
            } if propagation_targets.is_empty()
        ));
    }

    #[test]
    fn textual_near_matches_are_not_deduplicated() {
        let result = deduplicate_translation_candidates(vec![
            candidate(1, "Save", "Save", Vec::new(), None, state_context(1), false),
            candidate(2, "save", "save", Vec::new(), None, state_context(2), false),
            candidate(
                3,
                "Save ",
                "Save ",
                Vec::new(),
                None,
                state_context(3),
                false,
            ),
            candidate(4, "é", "é", Vec::new(), None, state_context(4), false),
            candidate(
                5,
                "e\u{301}",
                "e\u{301}",
                Vec::new(),
                None,
                state_context(5),
                false,
            ),
        ])
        .expect("大小写、空白和 Unicode 表示差异必须保留为不同原文");
        let (outcomes, _, _) = result.into_parts();

        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn stale_translation_without_current_seed_is_invalidated() {
        let stale_context = state_context(1);
        let pending_context = state_context(2);
        let stale_state = fingerprint(81);
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some(StoredTranslation::new("旧译文", stale_state)),
                stale_context,
                true,
            ),
            candidate(2, "保存", "保存", Vec::new(), None, pending_context, false),
        ])
        .expect("失效译文应按待翻译原文处理");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[TranslationPropagationTarget::new(
                    identity(2, "保存"),
                    pending_context,
                )]
        ));
        assert_eq!(invalidations.len(), 1);
        assert_eq!(invalidations[0].identity(), &identity(1, "保存"));
        assert_eq!(invalidations[0].expected_translation(), "旧译文");
        assert_eq!(invalidations[0].expected_translation_state(), stale_state);
        assert!(reuses.is_empty());
    }

    #[test]
    fn repeated_planning_preserves_interleaved_family_order() {
        let candidates = vec![
            candidate(1, "保存", "保存", Vec::new(), None, state_context(1), false),
            candidate(2, "終了", "終了", Vec::new(), None, state_context(2), false),
            candidate(3, "保存", "保存", Vec::new(), None, state_context(3), false),
            candidate(4, "終了", "終了", Vec::new(), None, state_context(4), false),
        ];

        let first =
            deduplicate_translation_candidates(candidates.clone()).expect("稳定输入应能完成去重");
        let second = deduplicate_translation_candidates(candidates).expect("重复规划应能完成去重");

        assert_eq!(first, second);
        let (outcomes, invalidations, reuses) = first.into_parts();
        assert!(invalidations.is_empty());
        assert!(reuses.is_empty());
        assert_eq!(
            outcomes[0],
            TranslationDeduplicationOutcome::Active {
                propagation_targets: vec![TranslationPropagationTarget::new(
                    identity(3, "保存"),
                    state_context(3),
                )],
            }
        );
        assert_eq!(
            outcomes[1],
            TranslationDeduplicationOutcome::Active {
                propagation_targets: vec![TranslationPropagationTarget::new(
                    identity(4, "終了"),
                    state_context(4),
                )],
            }
        );
    }
}
