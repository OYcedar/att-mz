//! RPG Maker 资产 Claim 的完整逻辑投影与持久化冲突摘要。
//!
//! 完整逻辑 Claim 参与资产指纹与写回重建；SQLite 只持久化足以完成跨 owner
//! 冲突检查的摘要。每个 `(owner, resource_key)` 最多保留一行：Exclusive
//! 本来就只能出现一次，多个 Intent 则保留自然顺序最早的组作为确定性代表。

use std::fmt;

use rayon::prelude::*;

use super::model::MutationResourceAccess;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EncodedMutationClaim {
    pub(crate) resource_key: String,
    pub(crate) access: MutationResourceAccess,
    pub(crate) group_location: String,
    pub(crate) group_order: usize,
}

impl EncodedMutationClaim {
    pub(crate) fn new(
        resource_key: String,
        access: MutationResourceAccess,
        group_location: String,
        group_order: usize,
    ) -> Self {
        Self {
            resource_key,
            access,
            group_location,
            group_order,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MutationClaimSummaryError {
    MixedAccess { resource_key: String },
    MultipleExclusive { resource_key: String },
}

impl fmt::Display for MutationClaimSummaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MixedAccess { resource_key } => {
                write!(
                    formatter,
                    "同一 owner 的资源同时声明了 intent 与 exclusive：{resource_key}"
                )
            }
            Self::MultipleExclusive { resource_key } => {
                write!(
                    formatter,
                    "同一 owner 的资源存在多个 exclusive 声明：{resource_key}"
                )
            }
        }
    }
}

impl std::error::Error for MutationClaimSummaryError {}

/// 按资产指纹既有契约建立全序。
pub(crate) fn sort_logical_claims(claims: &mut [EncodedMutationClaim]) {
    claims.par_sort_unstable_by(|left, right| {
        left.resource_key
            .cmp(&right.resource_key)
            .then_with(|| left.access.cmp(&right.access))
            .then_with(|| left.group_location.cmp(&right.group_location))
    });
}

/// 从已经按 [`sort_logical_claims`] 排序的完整逻辑 Claim 建立冲突充分摘要。
pub(crate) fn collision_summary(
    logical_claims: &[EncodedMutationClaim],
) -> Result<Vec<EncodedMutationClaim>, MutationClaimSummaryError> {
    let mut summary = Vec::new();
    let mut start = 0;
    while start < logical_claims.len() {
        let resource_key = logical_claims[start].resource_key.as_str();
        let mut end = start + 1;
        while end < logical_claims.len()
            && logical_claims[end].resource_key.as_str() == resource_key
        {
            end += 1;
        }
        let claims = &logical_claims[start..end];
        let access = claims[0].access;
        if claims.iter().any(|claim| claim.access != access) {
            return Err(MutationClaimSummaryError::MixedAccess {
                resource_key: resource_key.to_owned(),
            });
        }
        let representative = match access {
            MutationResourceAccess::Intent => claims
                .iter()
                .min_by(|left, right| {
                    left.group_order
                        .cmp(&right.group_order)
                        .then_with(|| left.group_location.cmp(&right.group_location))
                })
                .expect("非空资源分组必须存在代表"),
            MutationResourceAccess::Exclusive => {
                if claims.len() != 1 {
                    return Err(MutationClaimSummaryError::MultipleExclusive {
                        resource_key: resource_key.to_owned(),
                    });
                }
                &claims[0]
            }
        };
        summary.push(representative.clone());
        start = end;
    }
    Ok(summary)
}

/// 消费已经排序的完整逻辑 Claim，并把每个资源的代表直接移入持久化摘要。
///
/// Extract 完成完整指纹后不再需要逐条逻辑 Claim；消费式压缩避免为数十万条代表
/// 再克隆 resource key 与 group location。代表和冲突选择与 [`collision_summary`]
/// 完全相同。
pub(crate) fn collision_summary_owned(
    logical_claims: Vec<EncodedMutationClaim>,
) -> Result<Vec<EncodedMutationClaim>, MutationClaimSummaryError> {
    let mut claims = logical_claims.into_iter().peekable();
    let mut summary = Vec::new();
    while let Some(mut representative) = claims.next() {
        let access = representative.access;
        while claims
            .peek()
            .is_some_and(|claim| claim.resource_key == representative.resource_key)
        {
            let candidate = claims.next().expect("peek 已确认同资源 Claim 存在");
            if candidate.access != access {
                return Err(MutationClaimSummaryError::MixedAccess {
                    resource_key: representative.resource_key.clone(),
                });
            }
            if access == MutationResourceAccess::Exclusive {
                return Err(MutationClaimSummaryError::MultipleExclusive {
                    resource_key: representative.resource_key.clone(),
                });
            }
            if candidate
                .group_order
                .cmp(&representative.group_order)
                .then_with(|| candidate.group_location.cmp(&representative.group_location))
                .is_lt()
            {
                representative = candidate;
            }
        }
        summary.push(representative);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(
        resource_key: &str,
        access: MutationResourceAccess,
        group_location: &str,
        group_order: usize,
    ) -> EncodedMutationClaim {
        EncodedMutationClaim::new(
            resource_key.to_owned(),
            access,
            group_location.to_owned(),
            group_order,
        )
    }

    #[test]
    fn summary_folds_intents_to_the_earliest_natural_group() {
        let mut claims = vec![
            claim("resource", MutationResourceAccess::Intent, "group-a", 8),
            claim("resource", MutationResourceAccess::Intent, "group-z", 2),
            claim("other", MutationResourceAccess::Exclusive, "group-x", 5),
        ];
        sort_logical_claims(&mut claims);

        let summary = collision_summary(&claims).expect("合法 Claim 应可建立摘要");

        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].resource_key, "other");
        assert_eq!(summary[1].resource_key, "resource");
        assert_eq!(summary[1].group_location, "group-z");
        assert_eq!(summary[1].group_order, 2);
    }

    #[test]
    fn summary_rejects_mixed_access_and_duplicate_exclusive() {
        let mut mixed = vec![
            claim("resource", MutationResourceAccess::Intent, "group-a", 0),
            claim("resource", MutationResourceAccess::Exclusive, "group-b", 1),
        ];
        sort_logical_claims(&mut mixed);
        assert!(matches!(
            collision_summary(&mixed),
            Err(MutationClaimSummaryError::MixedAccess { .. })
        ));

        let mut duplicate = vec![
            claim("resource", MutationResourceAccess::Exclusive, "group-a", 0),
            claim("resource", MutationResourceAccess::Exclusive, "group-b", 1),
        ];
        sort_logical_claims(&mut duplicate);
        assert!(matches!(
            collision_summary(&duplicate),
            Err(MutationClaimSummaryError::MultipleExclusive { .. })
        ));
    }

    #[test]
    fn consuming_summary_matches_borrowed_summary_without_cloning_representatives() {
        let cases = [
            ("empty", Vec::new()),
            (
                "multiple resources",
                vec![
                    claim("resource", MutationResourceAccess::Intent, "group-a", 8),
                    claim("resource", MutationResourceAccess::Intent, "group-z", 2),
                    claim("other", MutationResourceAccess::Exclusive, "group-x", 5),
                ],
            ),
            (
                "duplicate exclusive",
                vec![
                    claim("resource", MutationResourceAccess::Exclusive, "group-a", 0),
                    claim("resource", MutationResourceAccess::Exclusive, "group-b", 1),
                ],
            ),
            (
                "group location tie-break",
                vec![
                    claim("resource", MutationResourceAccess::Intent, "group-z", 4),
                    claim("resource", MutationResourceAccess::Intent, "group-a", 4),
                ],
            ),
            (
                "mixed access",
                vec![
                    claim("resource", MutationResourceAccess::Intent, "group-a", 0),
                    claim("resource", MutationResourceAccess::Exclusive, "group-b", 1),
                ],
            ),
        ];
        for (name, mut claims) in cases {
            sort_logical_claims(&mut claims);
            assert_eq!(
                collision_summary_owned(claims.clone()),
                collision_summary(&claims),
                "消费式摘要必须在 {name} 场景保持借用式语义"
            );
        }

        let mut tied = vec![
            claim("resource", MutationResourceAccess::Intent, "group-z", 4),
            claim("resource", MutationResourceAccess::Intent, "group-a", 4),
        ];
        sort_logical_claims(&mut tied);
        let summary = collision_summary_owned(tied).expect("并列 Intent 应可消费式压缩");
        assert_eq!(summary[0].group_location, "group-a");
    }
}
