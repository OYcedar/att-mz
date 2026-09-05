//! Generic 自动译文的稳定内容适用性规则。

const GENERIC_AUTOMATIC_APPLICABILITY_DOMAIN: &[u8] = b"att.generic.automatic-applicability";

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

#[cfg(test)]
mod tests {
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
}
