use super::models::{TextPlanRules, TranslationItem};
use crate::native_core::controls::{
    iter_indexed_standard_spans, iter_literal_escape_spans, iter_no_param_standard_spans,
    iter_symbol_standard_spans, iter_terms_percent_spans,
};
use crate::native_core::text_layout::{
    count_unprotected_width_chars, is_byte_in_spans, normalize_byte_spans,
    normalize_wrapping_continuation_indents, raw_control_candidate_byte_spans,
};

pub(super) fn prepared_lines(
    item: &TranslationItem,
    rules: &TextPlanRules,
) -> Result<Vec<String>, String> {
    if item.translation_lines.is_empty() {
        return Err(format!(
            "译文行为空，不能写进游戏文件: {}",
            item.location_path
        ));
    }
    let lines: Vec<String> = item
        .translation_lines
        .iter()
        .map(|line| line.trim().to_string())
        .collect();
    Ok(normalize_translated_wrapping_punctuation(
        &item.original_lines,
        &lines,
        rules,
    ))
}

pub(super) fn prepared_single_text(
    item: &TranslationItem,
    rules: &TextPlanRules,
) -> Result<String, String> {
    prepared_lines(item, rules)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("译文行为空，不能写进游戏文件: {}", item.location_path))
}

pub(super) fn prepared_long_lines(
    item: &TranslationItem,
    rules: &TextPlanRules,
) -> Result<Vec<String>, String> {
    let mut lines = split_overwide_lines(prepared_lines(item, rules)?, rules);
    while lines.last().is_some_and(|line| line.is_empty()) {
        let _ = lines.pop();
    }
    if lines.is_empty() {
        return Err(format!(
            "长文本译文行为空，不能写进游戏文件: {}",
            item.location_path
        ));
    }
    Ok(lines)
}

pub(super) fn split_overwide_lines(lines: Vec<String>, rules: &TextPlanRules) -> Vec<String> {
    let mut split_lines = Vec::new();
    for line in lines {
        if line.is_empty() {
            split_lines.push(line);
            continue;
        }
        split_lines.extend(split_single_overwide_line(&line, rules));
    }
    normalize_wrapping_continuation_indents(
        split_lines,
        &rules.preserve_wrapping_punctuation_pairs,
        |line| protected_control_byte_spans(line, rules),
    )
}

pub(super) fn split_single_overwide_line(line: &str, rules: &TextPlanRules) -> Vec<String> {
    let mut result = Vec::new();
    let mut pending_line = line.to_string();
    while count_line_width_chars(&pending_line, rules) > rules.long_text_line_width_limit {
        let Some(split_position) = find_hard_split_position(&pending_line, rules) else {
            break;
        };
        if split_position == 0 || split_position >= pending_line.len() {
            break;
        }
        let head = pending_line[..split_position].trim_end().to_string();
        let tail = pending_line[split_position..].trim_start().to_string();
        if head.is_empty() || tail.is_empty() {
            break;
        }
        result.push(head);
        pending_line = tail;
    }
    result.push(pending_line);
    result
}

pub(super) fn find_hard_split_position(text: &str, rules: &TextPlanRules) -> Option<usize> {
    let mut line_width_count = 0usize;
    let protected_spans = protected_control_byte_spans(text, rules);
    for (index, character) in text.char_indices() {
        if is_byte_in_spans(index, &protected_spans) {
            continue;
        }
        if !rules.is_line_width_counted_char(character) {
            continue;
        }
        line_width_count += 1;
        if line_width_count < rules.long_text_line_width_limit {
            continue;
        }
        let mut position = index + character.len_utf8();
        while position < text.len() {
            let Some(next_character) = text[position..].chars().next() else {
                break;
            };
            if !rules
                .line_split_punctuations
                .iter()
                .any(|punctuation| punctuation == &next_character.to_string())
            {
                break;
            }
            position += next_character.len_utf8();
        }
        return Some(position);
    }
    None
}

pub(super) fn count_line_width_chars(text: &str, rules: &TextPlanRules) -> usize {
    let protected_spans = protected_control_byte_spans(text, rules);
    count_unprotected_width_chars(text, &protected_spans, |character| {
        rules.is_line_width_counted_char(character)
    })
}

pub(super) fn protected_control_byte_spans(
    text: &str,
    rules: &TextPlanRules,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    spans.extend(
        iter_indexed_standard_spans(text)
            .into_iter()
            .map(|span| (span.start, span.end)),
    );
    spans.extend(
        iter_no_param_standard_spans(text)
            .into_iter()
            .map(|span| (span.start, span.end)),
    );
    spans.extend(
        iter_symbol_standard_spans(text)
            .into_iter()
            .map(|span| (span.start, span.end)),
    );
    spans.extend(
        iter_terms_percent_spans(text)
            .into_iter()
            .map(|span| (span.start, span.end)),
    );
    spans.extend(
        iter_literal_escape_spans(text)
            .into_iter()
            .map(|span| (span.start, span.end)),
    );
    spans.extend(raw_control_candidate_byte_spans(text));
    spans.extend(
        rules
            .protected_macro_pattern
            .find_iter(text)
            .map(|matched| (matched.start(), matched.end())),
    );
    normalize_byte_spans(spans)
}

#[derive(Clone)]
struct WrappingBoundary {
    line_index: usize,
    char_index: usize,
    character: String,
}

struct WrappingSpan {
    left: WrappingBoundary,
    right: WrappingBoundary,
    pair: (String, String),
}

pub(super) fn normalize_translated_wrapping_punctuation(
    original_lines: &[String],
    translation_lines: &[String],
    rules: &TextPlanRules,
) -> Vec<String> {
    if rules.preserve_wrapping_punctuation_pairs.is_empty() {
        return translation_lines.to_vec();
    }
    let source_spans = collect_source_wrapping_spans(original_lines, rules);
    if source_spans.is_empty() {
        return translation_lines.to_vec();
    }
    let translated_spans = collect_translated_wrapping_spans(translation_lines, rules);
    if translated_spans.is_empty() {
        return translation_lines.to_vec();
    }
    let mut normalized_lines = translation_lines.to_vec();
    for (source_span, translated_span) in source_spans.iter().zip(translated_spans.iter()) {
        let replace_left = translated_span.left.character != source_span.pair.0;
        let replace_right = translated_span.right.character != source_span.pair.1;
        if translated_span.left.line_index == translated_span.right.line_index
            && translated_span.left.char_index < translated_span.right.char_index
        {
            if replace_right {
                normalized_lines[translated_span.right.line_index] = replace_char_at(
                    &normalized_lines[translated_span.right.line_index],
                    translated_span.right.char_index,
                    &source_span.pair.1,
                );
            }
            if replace_left {
                normalized_lines[translated_span.left.line_index] = replace_char_at(
                    &normalized_lines[translated_span.left.line_index],
                    translated_span.left.char_index,
                    &source_span.pair.0,
                );
            }
        } else {
            if replace_left {
                normalized_lines[translated_span.left.line_index] = replace_char_at(
                    &normalized_lines[translated_span.left.line_index],
                    translated_span.left.char_index,
                    &source_span.pair.0,
                );
            }
            if replace_right {
                normalized_lines[translated_span.right.line_index] = replace_char_at(
                    &normalized_lines[translated_span.right.line_index],
                    translated_span.right.char_index,
                    &source_span.pair.1,
                );
            }
        }
    }
    normalized_lines
}

fn collect_source_wrapping_spans(lines: &[String], rules: &TextPlanRules) -> Vec<WrappingSpan> {
    collect_wrapping_spans(
        lines,
        &rules.preserve_wrapping_punctuation_pairs,
        rules,
        true,
    )
}

fn collect_translated_wrapping_spans(lines: &[String], rules: &TextPlanRules) -> Vec<WrappingSpan> {
    let mut pairs = rules.preserve_wrapping_punctuation_pairs.clone();
    for pair in [
        ("“", "”"),
        ("‘", "’"),
        ("\"", "\""),
        ("'", "'"),
        ("＂", "＂"),
        ("「", "」"),
        ("『", "』"),
        ("《", "》"),
        ("〈", "〉"),
        ("（", "）"),
        ("(", ")"),
    ] {
        let pair_value = (pair.0.to_string(), pair.1.to_string());
        if !pairs.contains(&pair_value) {
            pairs.push(pair_value);
        }
    }
    collect_wrapping_spans(lines, &pairs, rules, false)
}

fn collect_wrapping_spans(
    lines: &[String],
    pair_definitions: &[(String, String)],
    rules: &TextPlanRules,
    allow_mismatched_right: bool,
) -> Vec<WrappingSpan> {
    let visible_chars = collect_visible_chars(lines, rules);
    let mut spans = Vec::new();
    let mut stack: Vec<(WrappingBoundary, (String, String))> = Vec::new();
    for boundary in visible_chars {
        let left_pair = pair_definitions
            .iter()
            .find(|(left, _right)| left == &boundary.character)
            .cloned();
        let is_right = pair_definitions
            .iter()
            .any(|(_left, right)| right == &boundary.character);
        if is_right && !stack.is_empty() {
            let Some((left_boundary, expected_pair)) = stack.pop() else {
                continue;
            };
            if expected_pair.1 == boundary.character || allow_mismatched_right {
                let pair = if expected_pair.1 == boundary.character {
                    expected_pair
                } else {
                    (left_boundary.character.clone(), boundary.character.clone())
                };
                spans.push(WrappingSpan {
                    left: left_boundary,
                    right: boundary,
                    pair,
                });
            } else {
                stack.push((left_boundary, expected_pair));
            }
            continue;
        }
        if let Some(pair) = left_pair {
            stack.push((boundary, pair));
        }
    }
    spans
}

fn collect_visible_chars(lines: &[String], rules: &TextPlanRules) -> Vec<WrappingBoundary> {
    let mut boundaries = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let protected_spans = protected_control_byte_spans(line, rules);
        for (char_index, (byte_index, character)) in line.char_indices().enumerate() {
            if is_byte_in_spans(byte_index, &protected_spans) {
                continue;
            }
            if character.is_whitespace() {
                continue;
            }
            boundaries.push(WrappingBoundary {
                line_index,
                char_index,
                character: character.to_string(),
            });
        }
    }
    boundaries
}

pub(super) fn replace_char_at(text: &str, char_index: usize, replacement: &str) -> String {
    let Some(byte_index) = text.char_indices().nth(char_index).map(|(index, _)| index) else {
        return text.to_string();
    };
    let next_index = text[byte_index..]
        .chars()
        .next()
        .map(|character| byte_index + character.len_utf8())
        .unwrap_or(byte_index);
    format!(
        "{}{}{}",
        &text[..byte_index],
        replacement,
        &text[next_index..],
    )
}

#[cfg(test)]
mod tests {
    use super::super::models::{SettingPayload, TextPlanRules};
    use super::{count_line_width_chars, split_overwide_lines};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct LayoutContract {
        line_width_cases: Vec<LineWidthCase>,
        split_cases: Vec<SplitCase>,
    }

    #[derive(Deserialize)]
    struct LineWidthCase {
        text: String,
        expected_width: usize,
    }

    #[derive(Deserialize)]
    struct SplitCase {
        #[serde(default)]
        line_width_limit: Option<usize>,
        lines: Vec<String>,
        expected: Vec<String>,
    }

    fn load_layout_contract() -> LayoutContract {
        serde_json::from_str(include_str!("../../../../tests/layout_contract_cases.json"))
            .expect("共享布局契约用例必须是有效 JSON")
    }

    fn contract_rules(long_text_line_width_limit: usize) -> TextPlanRules {
        TextPlanRules::from_payload(&SettingPayload {
            quality_text_rules: None,
            replacement_font_path: None,
            source_font_names: None,
            allowed_translation_paths: None,
            long_text_line_width_limit: Some(long_text_line_width_limit),
            line_width_count_pattern: Some(r"\S".to_string()),
            line_split_punctuations: Some(vec![
                "，".to_string(),
                "。".to_string(),
                "、".to_string(),
                "；".to_string(),
                "：".to_string(),
                "！".to_string(),
                "？".to_string(),
                "…".to_string(),
                "）".to_string(),
                "」".to_string(),
                "』".to_string(),
            ]),
            preserve_wrapping_punctuation_pairs: Some(vec![
                ("「".to_string(), "」".to_string()),
                ("『".to_string(), "』".to_string()),
                ("（".to_string(), "）".to_string()),
            ]),
            plan_content_output_dir: None,
        })
        .expect("共享布局契约文本规则应可编译")
    }

    #[test]
    fn shared_layout_contract_counts_only_visible_width() {
        let rules = contract_rules(999);
        for case in load_layout_contract().line_width_cases {
            assert_eq!(
                count_line_width_chars(&case.text, &rules),
                case.expected_width
            );
        }
    }

    #[test]
    fn shared_layout_contract_applies_wrapping_continuation_indent() {
        for case in load_layout_contract().split_cases {
            let rules = contract_rules(case.line_width_limit.unwrap_or(999));
            assert_eq!(split_overwide_lines(case.lines, &rules), case.expected);
        }
    }
}
