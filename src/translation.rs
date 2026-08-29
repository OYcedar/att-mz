//! 多个翻译引擎共享且语义一致的翻译能力。

/// 一条译文进入当前项目的受管入口。
///
/// 该闭集同时用于 Current 与 Rejected 状态；把 Current 候选转入 Rejected 时必须原样
/// 保留，导出方不能再从剩余表结构猜测来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationOrigin {
    Automatic,
    Manual,
}

impl TranslationOrigin {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "automatic" => Some(Self::Automatic),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

const GENERIC_AUTOMATIC_APPLICABILITY_DOMAIN: &[u8] = b"att.generic.automatic-applicability";
const RPG_MAKER_APPLICABILITY_DOMAIN: &[u8] = b"att.rpg-maker.applicability";
const RPG_MAKER_GROUP_SOURCE_CONTEXT_DOMAIN: &[u8] = b"att.rpg-maker.group-source-context";

/// 建立 Generic 自动译文正文的当前适用性指纹。
///
/// 指纹只绑定已经产出正文所针对的事实，不绑定 Client、Profile、Prompt、术语、语言检查阈值或
/// Placeholder 配置等未来请求选择。Placeholder 仍由各消费入口按当前规则独立执行强验收。
pub(crate) fn generic_automatic_applicability(
    source_language: &str,
    target_language: &str,
    group_id: &str,
    unit_id: &str,
    source_text: &str,
    group_context: crate::fingerprint::Sha256Fingerprint,
) -> crate::fingerprint::Sha256Fingerprint {
    generic_automatic_applicability_with_cancellation(
        source_language,
        target_language,
        group_id,
        unit_id,
        source_text,
        group_context,
        || Ok::<_, std::convert::Infallible>(()),
    )
    .unwrap_or_else(|unreachable| match unreachable {})
}

pub(crate) fn generic_automatic_applicability_with_cancellation<E>(
    source_language: &str,
    target_language: &str,
    group_id: &str,
    unit_id: &str,
    source_text: &str,
    group_context: crate::fingerprint::Sha256Fingerprint,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<crate::fingerprint::Sha256Fingerprint, E> {
    let chunk_size = std::num::NonZeroUsize::new(64 * 1024)
        .expect("Generic 自动译文适用性取消检查块大小必须非零");
    let mut hasher =
        crate::fingerprint::Sha256FramedHasher::new(GENERIC_AUTOMATIC_APPLICABILITY_DOMAIN);
    for (tag, bytes) in [
        (1, source_language.as_bytes()),
        (2, target_language.as_bytes()),
        (3, group_id.as_bytes()),
        (4, unit_id.as_bytes()),
        (5, source_text.as_bytes()),
        (6, group_context.as_bytes()),
    ] {
        hasher.try_frame_chunks(tag, bytes, chunk_size, &mut ensure_running)?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

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
mod applicability_tests {
    use super::*;

    fn context(value: u8) -> crate::fingerprint::Sha256Fingerprint {
        crate::fingerprint::Sha256Fingerprint::from_bytes([value; 32])
    }

    #[test]
    fn generic_applicability_compares_exact_content_facts() {
        let state =
            generic_automatic_applicability("ja", "zh-Hans", "group", "unit", "source", context(1));
        assert_ne!(
            state,
            generic_automatic_applicability("ja", "en", "group", "unit", "source", context(1),)
        );
        assert_ne!(
            state,
            generic_automatic_applicability("ja", "zh-Hans", "group", "unit", "source", context(2),)
        );
    }

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

pub(crate) mod candidate_validation;
pub(crate) mod layout_rules;
pub(crate) mod placeholder;
pub(crate) mod placeholder_projection;
pub(crate) mod placeholder_token;
pub(crate) mod planning_resource;
pub(crate) mod profile;
pub(crate) mod symbol_repair;
pub(crate) mod task_planning;
pub(crate) mod task_record;
pub(crate) mod text_layout;
pub(crate) mod user_message;
pub(crate) mod write_back_text;
