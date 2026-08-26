//! WriteBack 共用的保守文本断行与续行缩进。

use std::collections::BTreeSet;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::translation::placeholder_token;

const FULLWIDTH_INDENT: &str = "　";
const CONTINUATION_INDENT_CELLS: u64 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaidOutTextLine {
    text: String,
    source_line_index: usize,
}

impl LaidOutTextLine {
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn source_line_index(&self) -> usize {
        self.source_line_index
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaidOutText {
    lines: Vec<LaidOutTextLine>,
}

impl LaidOutText {
    pub(crate) fn lines(&self) -> &[LaidOutTextLine] {
        &self.lines
    }

    pub(crate) fn joined_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 只增加安全 LF 和可证明需要的 U+3000。返回 `None` 表示整段保持排版前文本。
pub(crate) fn layout_text(
    text: &str,
    max_fullwidth_chars: Option<u32>,
    complete_continuation_whitespace: bool,
) -> Option<LaidOutText> {
    let max_cells = max_fullwidth_chars.map(|width| u64::from(width) * 2);
    let mut lines = Vec::new();
    let mut wrapping_stack = Vec::new();

    for (source_line_index, hard_line) in text.split('\n').enumerate() {
        let tokens = scan_line(hard_line)?;
        let semantic_indent_required = complete_continuation_whitespace
            && !wrapping_stack.is_empty()
            && continuation_indent_position(&tokens).is_some();
        let first_indent_cells = if semantic_indent_required {
            CONTINUATION_INDENT_CELLS
        } else {
            0
        };
        let wrapped = match max_cells {
            Some(max_cells)
                if line_width(&tokens) > max_cells.saturating_sub(first_indent_cells) =>
            {
                wrap_line(
                    &tokens,
                    max_cells,
                    first_indent_cells,
                    complete_continuation_whitespace,
                )?
            }
            _ => vec![hard_line.to_owned()],
        };
        for (index, text) in wrapped.into_iter().enumerate() {
            let line_tokens = scan_line(&text)?;
            update_wrapping_stack(&line_tokens, &mut wrapping_stack);
            lines.push(WorkingLine {
                text,
                automatically_generated_continuation: index > 0,
                semantic_indent_required: index == 0 && semantic_indent_required,
                source_line_index,
            });
        }
    }

    if complete_continuation_whitespace {
        apply_continuation_indents(&mut lines, max_cells)?;
    }
    Some(LaidOutText {
        lines: lines
            .into_iter()
            .map(|line| LaidOutTextLine {
                text: line.text,
                source_line_index: line.source_line_index,
            })
            .collect(),
    })
}

struct WorkingLine {
    text: String,
    automatically_generated_continuation: bool,
    semantic_indent_required: bool,
    source_line_index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisplayTokenKind {
    Control,
    Whitespace,
    NonBreakingWhitespace,
    Visible,
}

#[derive(Clone, Copy, Debug)]
struct DisplayToken<'a> {
    text: &'a str,
    kind: DisplayTokenKind,
    width_cells: u64,
}

// 排版层只信任上游语义所有者建立的 ATT token；其他字符（包括反斜杠形式）都按可见文本计量。
fn scan_line(line: &str) -> Option<Vec<DisplayToken<'_>>> {
    let mut tokens = Vec::new();
    let mut offset = 0_usize;
    while offset < line.len() {
        let remaining = &line[offset..];
        if let Some(payload) = remaining.strip_prefix(placeholder_token::PREFIX) {
            let end = payload.find(placeholder_token::SUFFIX)?
                + placeholder_token::PREFIX.len()
                + placeholder_token::SUFFIX.len();
            tokens.push(DisplayToken {
                text: &remaining[..end],
                kind: DisplayTokenKind::Control,
                width_cells: 0,
            });
            offset += end;
            continue;
        }
        let grapheme = remaining.graphemes(true).next()?;
        if grapheme
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            return None;
        }
        let kind = if grapheme.chars().all(char::is_whitespace) {
            if grapheme
                .chars()
                .any(|character| matches!(character, '\u{00a0}' | '\u{202f}'))
            {
                DisplayTokenKind::NonBreakingWhitespace
            } else {
                DisplayTokenKind::Whitespace
            }
        } else {
            DisplayTokenKind::Visible
        };
        tokens.push(DisplayToken {
            text: grapheme,
            kind,
            width_cells: u64::try_from(UnicodeWidthStr::width_cjk(grapheme)).ok()?,
        });
        offset += grapheme.len();
    }
    Some(tokens)
}

fn line_width(tokens: &[DisplayToken<'_>]) -> u64 {
    tokens.iter().fold(0_u64, |width, token| {
        width.saturating_add(token.width_cells)
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BreakCandidate {
    head_end: usize,
    tail_start: usize,
}

fn wrap_line(
    tokens: &[DisplayToken<'_>],
    max_cells: u64,
    first_indent_cells: u64,
    reserve_continuation_indent: bool,
) -> Option<Vec<String>> {
    let candidates = collect_break_candidates(tokens);
    let mut starts = Vec::with_capacity(candidates.len() + 1);
    starts.push(0);
    starts.extend(candidates.iter().map(|candidate| candidate.tail_start));
    starts.sort_unstable();
    starts.dedup();

    let mut decisions = vec![None; tokens.len() + 1];
    for &start in starts.iter().rev() {
        let indent_cells = if start == 0 {
            first_indent_cells
        } else if reserve_continuation_indent {
            CONTINUATION_INDENT_CELLS
        } else {
            0
        };
        let limit = max_cells.saturating_sub(indent_cells);
        if token_range_width(tokens, start, tokens.len()) <= limit
            && valid_output_range(tokens, start, tokens.len())
        {
            decisions[start] = Some(None);
            continue;
        }
        for candidate in candidates.iter().rev() {
            if candidate.head_end <= start
                || token_range_width(tokens, start, candidate.head_end) > limit
                || !valid_output_range(tokens, start, candidate.head_end)
                || decisions[candidate.tail_start].is_none()
            {
                continue;
            }
            decisions[start] = Some(Some(*candidate));
            break;
        }
    }

    let mut output = Vec::new();
    let mut start = 0_usize;
    loop {
        match decisions.get(start).copied().flatten()? {
            None => {
                output.push(join_tokens(&tokens[start..]));
                break;
            }
            Some(candidate) => {
                output.push(join_tokens(&tokens[start..candidate.head_end]));
                start = candidate.tail_start;
            }
        }
    }
    Some(output)
}

fn token_range_width(tokens: &[DisplayToken<'_>], start: usize, end: usize) -> u64 {
    line_width(&tokens[start..end])
}

fn valid_output_range(tokens: &[DisplayToken<'_>], start: usize, end: usize) -> bool {
    let visible = tokens[start..end]
        .iter()
        .filter(|token| token.kind == DisplayTokenKind::Visible)
        .collect::<Vec<_>>();
    let (Some(first), Some(last)) = (visible.first(), visible.last()) else {
        return false;
    };
    !is_line_start_prohibited(first.text) && !is_pair_opener(last.text)
}

fn collect_break_candidates(tokens: &[DisplayToken<'_>]) -> Vec<BreakCandidate> {
    let mut candidates = BTreeSet::new();
    let mut index = 0_usize;
    while index < tokens.len() {
        if tokens[index].kind == DisplayTokenKind::Whitespace {
            let end = whitespace_run_end(tokens, index);
            if index > 0 && end < tokens.len() {
                candidates.insert(BreakCandidate {
                    head_end: index,
                    tail_start: end,
                });
            }
            index = end;
            continue;
        }
        if is_break_punctuation(tokens, index) {
            let mut head_end = index + 1;
            while head_end < tokens.len() && is_pair_closer(tokens[head_end].text) {
                head_end += 1;
            }
            let tail_start = whitespace_run_end(tokens, head_end);
            if tail_start < tokens.len() {
                candidates.insert(BreakCandidate {
                    head_end,
                    tail_start,
                });
            }
        }
        index += 1;
    }
    candidates.into_iter().collect()
}

fn whitespace_run_end(tokens: &[DisplayToken<'_>], start: usize) -> usize {
    let mut end = start;
    while end < tokens.len() && tokens[end].kind == DisplayTokenKind::Whitespace {
        end += 1;
    }
    end
}

fn is_break_punctuation(tokens: &[DisplayToken<'_>], index: usize) -> bool {
    let text = tokens[index].text;
    if matches!(text, "，" | "。" | "！" | "？" | "；" | "：" | "、" | "…") {
        return true;
    }
    matches!(text, "," | "." | "!" | "?" | ";" | ":")
        && tokens.get(index + 1).is_none_or(|next| {
            next.kind == DisplayTokenKind::Whitespace || is_pair_closer(next.text)
        })
}

fn is_line_start_prohibited(text: &str) -> bool {
    is_pair_closer(text)
        || matches!(
            text,
            "，" | "。"
                | "！"
                | "？"
                | "；"
                | "："
                | "、"
                | "…"
                | ","
                | "."
                | "!"
                | "?"
                | ";"
                | ":"
        )
}

fn is_pair_opener(text: &str) -> bool {
    pair_closer(text).is_some()
}

fn is_pair_closer(text: &str) -> bool {
    matches!(
        text,
        ")" | "」" | "』" | "”" | "）" | "】" | "》" | "〉" | "〕" | "］" | "｝"
    )
}

fn join_tokens(tokens: &[DisplayToken<'_>]) -> String {
    let mut output = String::with_capacity(tokens.iter().map(|token| token.text.len()).sum());
    for token in tokens {
        output.push_str(token.text);
    }
    output
}

fn apply_continuation_indents(lines: &mut [WorkingLine], max_cells: Option<u64>) -> Option<()> {
    let mut wrapping_stack = Vec::new();
    for line in lines {
        let insert_at = if (line.automatically_generated_continuation
            || line.semantic_indent_required)
            && !wrapping_stack.is_empty()
        {
            continuation_indent_position(&scan_line(&line.text)?)
        } else {
            None
        };
        if let Some(insert_at) = insert_at {
            line.text.insert_str(insert_at, FULLWIDTH_INDENT);
        }
        let tokens = scan_line(&line.text)?;
        if max_cells.is_some_and(|maximum| line_width(&tokens) > maximum) {
            return None;
        }
        update_wrapping_stack(&tokens, &mut wrapping_stack);
    }
    Some(())
}

fn continuation_indent_position(tokens: &[DisplayToken<'_>]) -> Option<usize> {
    let mut insert_at = 0_usize;
    let mut first_non_control = 0_usize;
    while first_non_control < tokens.len()
        && tokens[first_non_control].kind == DisplayTokenKind::Control
    {
        insert_at += tokens[first_non_control].text.len();
        first_non_control += 1;
    }
    tokens
        .get(first_non_control)
        .is_some_and(|token| token.kind == DisplayTokenKind::Visible)
        .then_some(insert_at)
}

fn update_wrapping_stack(tokens: &[DisplayToken<'_>], stack: &mut Vec<&'static str>) {
    for token in tokens
        .iter()
        .filter(|token| token.kind == DisplayTokenKind::Visible)
    {
        if stack.last().is_some_and(|closer| *closer == token.text) {
            let _ = stack.pop();
            continue;
        }
        if let Some(closer) = pair_closer(token.text) {
            stack.push(closer);
        }
    }
}

fn pair_closer(opener: &str) -> Option<&'static str> {
    match opener {
        "(" => Some(")"),
        "「" => Some("」"),
        "『" => Some("』"),
        "“" => Some("”"),
        "（" => Some("）"),
        "【" => Some("】"),
        "《" => Some("》"),
        "〈" => Some("〉"),
        "〔" => Some("〕"),
        "［" => Some("］"),
        "｛" => Some("｝"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_punctuation_and_indents_inside_quote() {
        let laid_out = layout_text("「甲乙，丙丁」", Some(4), true).expect("标点断点应可安全排版");
        assert_eq!(
            laid_out
                .lines()
                .iter()
                .map(LaidOutTextLine::text)
                .collect::<Vec<_>>(),
            ["「甲乙，", "　丙丁」"]
        );
    }

    #[test]
    fn independent_whitespace_completion_handles_protected_controls() {
        let laid_out = layout_text("「甲\n⟦ATT_CONTROL⟧乙」", None, true)
            .expect("语义所有者建立的控制符 token 后应可补空白");
        assert_eq!(laid_out.joined_text(), "「甲\n⟦ATT_CONTROL⟧　乙」");
    }

    #[test]
    fn unknown_backslash_forms_remain_visible_text() {
        for text in ["「甲\n\\SE[2]乙」", "「甲\n\\N<姓名>乙」"] {
            let laid_out = layout_text(text, None, true).expect("普通可见文本应可排版");
            assert_eq!(
                laid_out.joined_text(),
                text.replacen('\n', "\n　", 1),
                "没有实际消费者建立 token 的反斜杠形式不能被当作零宽控制符"
            );
        }
    }

    #[test]
    fn existing_leading_whitespace_is_not_duplicated() {
        for whitespace in [" ", "　", "\u{00a0}"] {
            let text = format!("「甲\n{whitespace}乙」");
            let laid_out = layout_text(&text, None, true).expect("既有空白应保持");
            assert_eq!(laid_out.joined_text(), text);
        }
    }

    #[test]
    fn tracks_a_new_pair_after_closing_an_old_pair_on_the_same_line() {
        let laid_out = layout_text("「甲」又「乙\n丙」", None, true)
            .expect("同一行重新打开的引号应延续到下一硬行");
        assert_eq!(laid_out.joined_text(), "「甲」又「乙\n　丙」");
    }
}
