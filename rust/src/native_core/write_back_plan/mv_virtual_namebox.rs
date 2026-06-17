//! MV 虚拟名字框写回辅助：译文正文说话人前缀校验。
//!
//! 名字框说话人/正文的解析与 render_parts 构建由索引阶段（scope_index）唯一完成，
//! 写回阶段只消费当前文本事实的 render_parts，不再重新解析 401 指令。

pub(super) fn ensure_mv_translation_body_is_clean(
    source_speaker: &str,
    translated_speaker: &str,
    translation_lines: &[String],
    location_path: &str,
) -> Result<(), String> {
    let Some(first_line) = translation_lines.first() else {
        return Ok(());
    };
    let first_line = first_line.trim();
    let forbidden_prefixes = [
        format!("{source_speaker}:"),
        format!("{source_speaker}："),
        format!("{source_speaker}「"),
        format!("{source_speaker}（"),
        format!("{translated_speaker}:"),
        format!("{translated_speaker}："),
        format!("{translated_speaker}「"),
        format!("{translated_speaker}（"),
    ];
    if forbidden_prefixes
        .iter()
        .any(|prefix| first_line.starts_with(prefix))
    {
        return Err(format!(
            "MV 译文正文仍包含说话人前缀，请先执行 reset-translations --all 后重新翻译: {location_path}"
        ));
    }
    Ok(())
}
