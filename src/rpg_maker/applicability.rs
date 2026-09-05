//! RPG Maker 完整 Group 来源语境与自动正文的适用性规则。

const RPG_MAKER_APPLICABILITY_DOMAIN: &[u8] = b"att.rpg-maker.applicability";
const RPG_MAKER_GROUP_SOURCE_CONTEXT_DOMAIN: &[u8] = b"att.rpg-maker.group-source-context";

/// 建立 RPG Maker 完整 Group 的稳定原文语境。
///
/// 调用方按自然顺序提供该 Group 的全部 Unit。这里只绑定 Group/Unit 的来源事实，
/// 不绑定译文、模型请求资源、Client、Prompt、术语或 Placeholder 配置。
pub(crate) fn rpg_maker_group_source_context<'a>(
    group_kind: &str,
    units: impl ExactSizeIterator<Item = (&'a str, &'a [u8], &'a str, &'a str)>,
) -> crate::fingerprint::Sha256Fingerprint {
    rpg_maker_group_source_context_with_cancellation(group_kind, units, || {
        Ok::<_, std::convert::Infallible>(())
    })
    .unwrap_or_else(|unreachable| match unreachable {})
}

pub(crate) fn rpg_maker_group_source_context_with_cancellation<'a, E>(
    group_kind: &str,
    units: impl ExactSizeIterator<Item = (&'a str, &'a [u8], &'a str, &'a str)>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<crate::fingerprint::Sha256Fingerprint, E> {
    let count = u64::try_from(units.len())
        .expect("当前平台的 RPG Maker Group Unit 数量必须可表示为 u64")
        .to_be_bytes();
    let chunk_size = std::num::NonZeroUsize::new(64 * 1024)
        .expect("RPG Maker Group 来源语境取消检查块大小必须非零");
    let mut hasher =
        crate::fingerprint::Sha256FramedHasher::new(RPG_MAKER_GROUP_SOURCE_CONTEXT_DOMAIN);
    hasher.frame(1, group_kind.as_bytes()).frame(3, &count);
    for (role, semantic_order_key, source_content_json, source_context_json) in units {
        ensure_running()?;
        hasher.try_frame_chunks(10, role.as_bytes(), chunk_size, &mut ensure_running)?;
        hasher.try_frame_chunks(11, semantic_order_key, chunk_size, &mut ensure_running)?;
        hasher.try_frame_chunks(
            12,
            source_content_json.as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.try_frame_chunks(
            13,
            source_context_json.as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

/// 建立 RPG Maker 自动正文与 Rejected 候选共用的当前适用性指纹。
///
/// 状态只绑定译文实际针对的稳定项目与来源事实；生成下一次请求所用的 Client、Profile、
/// Prompt、术语、语言检查阈值和 Placeholder 配置均不进入。Placeholder 继续由每个消费
/// 入口按当前规则独立执行强验收。
#[allow(clippy::too_many_arguments)]
pub(crate) fn rpg_maker_applicability(
    source_language: &str,
    target_language: &str,
    owner: &str,
    group_kind: &str,
    group_location: &str,
    unit_role: &str,
    recipe_shape: &str,
    source_content_json: &str,
    source_context_json: &str,
    group_source_context: crate::fingerprint::Sha256Fingerprint,
) -> crate::fingerprint::Sha256Fingerprint {
    rpg_maker_applicability_with_cancellation(
        source_language,
        target_language,
        owner,
        group_kind,
        group_location,
        unit_role,
        recipe_shape,
        source_content_json,
        source_context_json,
        group_source_context,
        || Ok::<_, std::convert::Infallible>(()),
    )
    .unwrap_or_else(|unreachable| match unreachable {})
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rpg_maker_applicability_with_cancellation<E>(
    source_language: &str,
    target_language: &str,
    owner: &str,
    group_kind: &str,
    group_location: &str,
    unit_role: &str,
    recipe_shape: &str,
    source_content_json: &str,
    source_context_json: &str,
    group_source_context: crate::fingerprint::Sha256Fingerprint,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<crate::fingerprint::Sha256Fingerprint, E> {
    let chunk_size =
        std::num::NonZeroUsize::new(64 * 1024).expect("RPG Maker 译文适用性取消检查块大小必须非零");
    let mut hasher = crate::fingerprint::Sha256FramedHasher::new(RPG_MAKER_APPLICABILITY_DOMAIN);
    for (tag, bytes) in [
        (1, source_language.as_bytes()),
        (2, target_language.as_bytes()),
        (3, owner.as_bytes()),
        (4, group_kind.as_bytes()),
        (5, group_location.as_bytes()),
        (6, unit_role.as_bytes()),
        (7, recipe_shape.as_bytes()),
        (8, source_content_json.as_bytes()),
        (9, source_context_json.as_bytes()),
        (10, group_source_context.as_bytes()),
    ] {
        hasher.try_frame_chunks(tag, bytes, chunk_size, &mut ensure_running)?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

#[cfg(test)]
pub(crate) fn unrelated_rpg_maker_applicability_for_test() -> crate::fingerprint::Sha256Fingerprint
{
    rpg_maker_applicability(
        "ja",
        "zh-Hans",
        "builtin",
        "database_entry",
        r#"["v",["d","Items.json"],[1]]"#,
        r#"{"f":"name"}"#,
        "[]",
        r#""另一条原文""#,
        "{}",
        crate::fingerprint::Sha256Fingerprint::from_bytes([0x5a; 32]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpg_maker_applicability_uses_exact_stable_facts() {
        let group = rpg_maker_group_source_context(
            "event_dialogue",
            [
                ("speaker", [1].as_slice(), r#""角色""#, "{}"),
                (
                    "body",
                    [2].as_slice(),
                    r#"["原文"]"#,
                    r#"{"source_speaker":"角色"}"#,
                ),
            ]
            .into_iter(),
        );
        let state = rpg_maker_applicability(
            "ja",
            "zh-Hans",
            "builtin",
            "event_dialogue",
            r#"{"source":{"kind":"map","map_id":1},"steps":[]}"#,
            "body",
            "[]",
            r#"["原文"]"#,
            r#"{"source_speaker":"角色"}"#,
            group,
        );
        assert_ne!(
            state,
            rpg_maker_applicability(
                "ja",
                "en",
                "builtin",
                "event_dialogue",
                r#"{"source":{"kind":"map","map_id":1},"steps":[]}"#,
                "body",
                "[]",
                r#"["原文"]"#,
                r#"{"source_speaker":"角色"}"#,
                group,
            )
        );
    }
}
