//! 文本布局共享辅助。
//!
//! 本模块只处理与具体配置无关的布局事实：受保护片段不贡献可见宽度、
//! 续行缩进插入到行首控制符之后，以及基于受保护范围的可见文本提取。

use crate::native_core::controls::iter_raw_control_sequence_candidates;

pub(crate) const WRAPPING_CONTINUATION_INDENT: &str = "　";

pub(crate) fn raw_control_candidate_byte_spans(text: &str) -> Vec<(usize, usize)> {
    iter_raw_control_sequence_candidates(text)
        .into_iter()
        .map(|candidate| (candidate.start, candidate.end))
        .collect()
}

pub(crate) fn normalize_byte_spans<I>(spans: I) -> Vec<(usize, usize)>
where
    I: IntoIterator<Item = (usize, usize)>,
{
    let mut sorted_spans: Vec<(usize, usize)> = spans
        .into_iter()
        .filter(|(start, end)| start < end)
        .collect();
    sorted_spans.sort_unstable();

    let mut normalized: Vec<(usize, usize)> = Vec::new();
    for (start, end) in sorted_spans {
        let Some(last) = normalized.last_mut() else {
            normalized.push((start, end));
            continue;
        };
        if start <= last.1 {
            last.1 = last.1.max(end);
            continue;
        }
        normalized.push((start, end));
    }
    normalized
}

pub(crate) fn is_byte_in_spans(byte_index: usize, spans: &[(usize, usize)]) -> bool {
    spans
        .iter()
        .any(|(start, end)| *start <= byte_index && byte_index < *end)
}

pub(crate) fn count_unprotected_width_chars<F>(
    text: &str,
    protected_spans: &[(usize, usize)],
    mut is_counted: F,
) -> usize
where
    F: FnMut(char) -> bool,
{
    text.char_indices()
        .filter(|(byte_index, character)| {
            !is_byte_in_spans(*byte_index, protected_spans) && is_counted(*character)
        })
        .count()
}

pub(crate) fn strip_byte_spans(text: &str, protected_spans: &[(usize, usize)]) -> String {
    if protected_spans.is_empty() {
        return text.to_string();
    }
    let normalized_spans = normalize_byte_spans(protected_spans.iter().copied());
    let mut output = String::new();
    let mut last_end = 0usize;
    for (start, end) in normalized_spans {
        if start > last_end {
            output.push_str(&text[last_end..start]);
        }
        last_end = end;
    }
    output.push_str(&text[last_end..]);
    output
}

pub(crate) fn prepend_after_leading_protected_spans(
    line: &str,
    prefix: &str,
    protected_spans: &[(usize, usize)],
) -> String {
    if prefix.is_empty() || line.is_empty() || line.starts_with(prefix) {
        return line.to_string();
    }

    let normalized_spans = normalize_byte_spans(protected_spans.iter().copied());
    let mut insert_at = 0usize;
    while let Some((_start, end)) = normalized_spans
        .iter()
        .find(|(start, end)| *start == insert_at && *end > insert_at)
    {
        insert_at = *end;
    }

    if insert_at >= line.len() || line[insert_at..].starts_with(prefix) {
        return line.to_string();
    }
    let Some(first_visible_char) = line[insert_at..].chars().next() else {
        return line.to_string();
    };
    if first_visible_char.is_whitespace() {
        return line.to_string();
    }

    format!("{}{}{}", &line[..insert_at], prefix, &line[insert_at..])
}

pub(crate) fn normalize_wrapping_continuation_indents<F>(
    lines: Vec<String>,
    wrapping_pairs: &[(String, String)],
    mut protected_spans_for_line: F,
) -> Vec<String>
where
    F: FnMut(&str) -> Vec<(usize, usize)>,
{
    if wrapping_pairs.is_empty() {
        return lines;
    }

    let mut normalized_lines = Vec::with_capacity(lines.len());
    let mut active_wrapping_stack: Vec<(String, String)> = Vec::new();
    for line in lines {
        let protected_spans = protected_spans_for_line(&line);
        let normalized_line = if active_wrapping_stack.is_empty() {
            line.clone()
        } else {
            prepend_after_leading_protected_spans(
                &line,
                WRAPPING_CONTINUATION_INDENT,
                &protected_spans,
            )
        };
        update_wrapping_stack_from_line(
            &mut active_wrapping_stack,
            &strip_byte_spans(&line, &protected_spans),
            wrapping_pairs,
        );
        normalized_lines.push(normalized_line);
    }
    normalized_lines
}

fn update_wrapping_stack_from_line(
    active_wrapping_stack: &mut Vec<(String, String)>,
    visible_line: &str,
    wrapping_pairs: &[(String, String)],
) {
    let mut visible_characters = visible_line
        .chars()
        .filter(|character| !character.is_whitespace());
    let Some(first_character) = visible_characters.next() else {
        return;
    };
    if active_wrapping_stack.is_empty() {
        let first = first_character.to_string();
        let Some(pair) = wrapping_pairs.iter().find(|(left, _right)| left == &first) else {
            return;
        };
        active_wrapping_stack.push(pair.clone());
        update_wrapping_stack_from_characters(
            active_wrapping_stack,
            visible_characters,
            wrapping_pairs,
        );
        return;
    }

    update_wrapping_stack_from_characters(
        active_wrapping_stack,
        std::iter::once(first_character).chain(visible_characters),
        wrapping_pairs,
    );
}

fn update_wrapping_stack_from_characters<I>(
    active_wrapping_stack: &mut Vec<(String, String)>,
    visible_characters: I,
    wrapping_pairs: &[(String, String)],
) where
    I: IntoIterator<Item = char>,
{
    for character in visible_characters {
        let current = character.to_string();
        if active_wrapping_stack
            .last()
            .is_some_and(|(_left, right)| right == &current)
        {
            let _ = active_wrapping_stack.pop();
            continue;
        }
        if let Some(pair) = wrapping_pairs
            .iter()
            .find(|(left, _right)| left == &current)
        {
            active_wrapping_stack.push(pair.clone());
        }
    }
}
