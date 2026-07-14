//! 标准翻译语料的全局确定性去重。
//!
//! 本模块只拥有“一个翻译决策对应哪些具体位置”的领域规则。它不执行 I/O、
//! 不切分任务，也不持久化关系；调用方必须先按 MZ 自然顺序提供候选项。

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use super::standard::{
    AppliedPlaceholder, TerminologyDependency, TranslationInvalidation, TranslationLeafIdentity,
    TranslationReuse, TranslationReuseSeed, TranslationReuseTarget, TranslationVirtualReason,
};

/// 一个已经完成语言判定和占位符保护的可翻译叶子。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationDeduplicationCandidate {
    identity: TranslationLeafIdentity,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
    translation: Option<String>,
    terminology_dependencies: Vec<TerminologyDependency>,
    invalidated: bool,
}

impl TranslationDeduplicationCandidate {
    pub(crate) fn new(
        identity: TranslationLeafIdentity,
        protected_text: impl Into<String>,
        applied_placeholders: Vec<AppliedPlaceholder>,
        translation: Option<String>,
        terminology_dependencies: Vec<TerminologyDependency>,
        invalidated: bool,
    ) -> Self {
        Self {
            identity,
            protected_text: protected_text.into(),
            applied_placeholders,
            translation,
            terminology_dependencies,
            invalidated,
        }
    }
}

/// 去重后一个可翻译叶子的任务责任。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationDeduplicationOutcome {
    Active {
        propagation_targets: Vec<TranslationLeafIdentity>,
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
    original_text: String,
    protected_text: String,
    applied_placeholders: Vec<AppliedPlaceholder>,
}

impl DeduplicationKey {
    fn from_candidate(candidate: &TranslationDeduplicationCandidate) -> Self {
        Self {
            original_text: candidate.identity.original_text().to_owned(),
            protected_text: candidate.protected_text.clone(),
            applied_placeholders: candidate.applied_placeholders.clone(),
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
            candidate.terminology_dependencies.clone(),
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
                seed.terminology_dependencies.clone(),
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
        .map(|&index| candidates[index].identity.clone())
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
                candidate.terminology_dependencies.clone(),
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
                        conflict.identity.exact_location(),
                        conflict.translation
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl Error for TranslationDeduplicationError {}

#[cfg(test)]
mod tests {
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };

    use super::*;
    use crate::att_mz::translate::standard::{PlaceholderRuleOrigin, PlaceholderSegment};

    fn identity(index: usize, original: &str) -> TranslationLeafIdentity {
        let group_location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(index)],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(index), MzLocationStep::key("name")],
            ),
            original,
        )
    }

    fn candidate(
        index: usize,
        original: &str,
        protected_text: &str,
        placeholders: Vec<AppliedPlaceholder>,
        translation: Option<&str>,
        dependencies: Vec<TerminologyDependency>,
        invalidated: bool,
    ) -> TranslationDeduplicationCandidate {
        TranslationDeduplicationCandidate::new(
            identity(index, original),
            protected_text,
            placeholders,
            translation.map(str::to_owned),
            dependencies,
            invalidated,
        )
    }

    fn placeholder(scope: &str) -> AppliedPlaceholder {
        AppliedPlaceholder::new(
            "<att:actor-name:0>",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            scope,
            PlaceholderSegment::Whole,
        )
    }

    #[test]
    fn first_pending_member_owns_one_output_and_all_later_locations() {
        let first = identity(1, "保存しますか？");
        let second = identity(2, "保存しますか？");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存しますか？",
                "保存しますか？",
                Vec::new(),
                None,
                Vec::new(),
                false,
            ),
            candidate(
                2,
                "保存しますか？",
                "保存しますか？",
                Vec::new(),
                None,
                Vec::new(),
                false,
            ),
        ])
        .expect("相同原文应建立唯一代表");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert_eq!(invalidations, Vec::new());
        assert_eq!(reuses, Vec::new());
        assert_eq!(
            outcomes[0],
            TranslationDeduplicationOutcome::Active {
                propagation_targets: vec![second]
            }
        );
        assert_eq!(
            outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Duplicate {
                    leader: Box::new(first)
                }
            }
        );
    }

    #[test]
    fn valid_translation_becomes_a_preparation_reuse_without_llm_owner() {
        let dependency = TerminologyDependency::new("保存", "Save");
        let seed = identity(1, "保存");
        let target = identity(2, "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some("Save"),
                vec![dependency.clone()],
                false,
            ),
            candidate(2, "保存", "保存", Vec::new(), None, Vec::new(), false),
        ])
        .expect("唯一有效译文应直接复用");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &seed);
        assert_eq!(reuses[0].seed().expected_translation(), "Save");
        assert_eq!(
            reuses[0].seed().expected_terminology_dependencies(),
            &[dependency]
        );
        assert_eq!(reuses[0].targets()[0].identity(), &target);
        assert_eq!(reuses[0].targets()[0].expected_translation(), None);
        assert!(matches!(
            &outcomes[1],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed: actual }
            } if actual.as_ref() == &seed
        ));
    }

    #[test]
    fn earliest_equal_translation_seed_supplies_the_reused_dependencies() {
        let earliest_dependency = TerminologyDependency::new("保存", "保存");
        let later_dependency = TerminologyDependency::new("記録", "保存");
        let earliest_seed = identity(1, "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some("保存"),
                vec![earliest_dependency.clone()],
                false,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some("保存"),
                vec![later_dependency],
                false,
            ),
            candidate(3, "保存", "保存", Vec::new(), None, Vec::new(), false),
        ])
        .expect("相同有效译文不构成冲突");
        let (_, _, reuses) = result.into_parts();

        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &earliest_seed);
        assert_eq!(
            reuses[0].seed().expected_terminology_dependencies(),
            &[earliest_dependency]
        );
        assert_eq!(reuses[0].targets().len(), 1);
        assert_eq!(reuses[0].targets()[0].identity(), &identity(3, "保存"));
    }

    #[test]
    fn stale_translation_is_a_reuse_target_when_another_valid_seed_exists() {
        let stale_dependency = TerminologyDependency::new("保存", "旧译名");
        let valid_dependency = TerminologyDependency::new("保存", "保存");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some("旧译文"),
                vec![stale_dependency.clone()],
                true,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some("保存"),
                vec![valid_dependency.clone()],
                false,
            ),
        ])
        .expect("失效译文不能成为种子，但可以由其他有效种子覆盖");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(invalidations.is_empty());
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].seed().identity(), &identity(2, "保存"));
        assert_eq!(
            reuses[0].seed().expected_terminology_dependencies(),
            &[valid_dependency]
        );
        assert_eq!(
            reuses[0].targets()[0].expected_translation(),
            Some("旧译文")
        );
        assert_eq!(
            reuses[0].targets()[0].expected_terminology_dependencies(),
            &[stale_dependency]
        );
        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Virtual {
                reason: TranslationVirtualReason::Reused { seed }
            } if seed.as_ref() == &identity(2, "保存")
        ));
    }

    #[test]
    fn conflicting_valid_translations_fail_before_a_plan_exists() {
        let error = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some("Save"),
                Vec::new(),
                false,
            ),
            candidate(
                2,
                "保存",
                "保存",
                Vec::new(),
                Some("Store"),
                Vec::new(),
                false,
            ),
        ])
        .expect_err("同族有效译文冲突必须显式失败");

        assert!(matches!(
            error,
            TranslationDeduplicationError::ConflictingReusableTranslations {
                conflicts,
                ..
            } if conflicts.len() == 2
        ));
    }

    #[test]
    fn different_placeholder_contracts_form_independent_families() {
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "\\N[1]",
                "<att:actor-name:0>",
                vec![placeholder("database_entry")],
                None,
                Vec::new(),
                false,
            ),
            candidate(
                2,
                "\\N[1]",
                "<att:actor-name:0>",
                vec![placeholder("event_dialogue")],
                None,
                Vec::new(),
                false,
            ),
        ])
        .expect("不同保护契约不应互相冲突");
        let (outcomes, _, _) = result.into_parts();

        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            TranslationDeduplicationOutcome::Active {
                propagation_targets
            } if propagation_targets.is_empty()
        )));
    }

    #[test]
    fn textual_near_matches_are_not_deduplicated() {
        let result = deduplicate_translation_candidates(vec![
            candidate(1, "Save", "Save", Vec::new(), None, Vec::new(), false),
            candidate(2, "save", "save", Vec::new(), None, Vec::new(), false),
            candidate(3, "Save ", "Save ", Vec::new(), None, Vec::new(), false),
            candidate(4, "é", "é", Vec::new(), None, Vec::new(), false),
            candidate(
                5,
                "e\u{301}",
                "e\u{301}",
                Vec::new(),
                None,
                Vec::new(),
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
    fn stale_translations_cannot_seed_reuse_and_are_invalidated() {
        let old_dependency = TerminologyDependency::new("保存", "旧译名");
        let result = deduplicate_translation_candidates(vec![
            candidate(
                1,
                "保存",
                "保存",
                Vec::new(),
                Some("旧译文"),
                vec![old_dependency.clone()],
                true,
            ),
            candidate(2, "保存", "保存", Vec::new(), None, Vec::new(), false),
        ])
        .expect("失效译文应按待翻译原文处理");
        let (outcomes, invalidations, reuses) = result.into_parts();

        assert!(matches!(
            &outcomes[0],
            TranslationDeduplicationOutcome::Active { propagation_targets }
                if propagation_targets == &[identity(2, "保存")]
        ));
        assert_eq!(invalidations.len(), 1);
        assert_eq!(invalidations[0].expected_translation(), "旧译文");
        assert_eq!(
            invalidations[0].expected_terminology_dependencies(),
            &[old_dependency]
        );
        assert!(reuses.is_empty());
    }
}
