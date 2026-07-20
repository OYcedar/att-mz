//! RPG Maker 写回文本的保守显示布局。

use std::collections::{BTreeMap, BTreeSet};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    RpgMakerAppliedTextLayout, RpgMakerLayoutTextPair, RpgMakerTextLayoutOutcome,
    RpgMakerWriteBackAppliedLayout, RpgMakerWriteBackLaidOutLine, RpgMakerWriteBackLaidOutSegment,
    RpgMakerWriteBackLayoutCandidate, RpgMakerWriteBackLayoutOutcome,
    RpgMakerWriteBackLayoutRegion, RpgMakerWriteBackLayoutRequest, RpgMakerWriteBackTextLayouter,
};
use crate::rpg_maker::placeholder_token;
use crate::rpg_maker::project::{MaxFullwidthChars, RpgMakerWriteBackLayoutProfile};

const FULLWIDTH_INDENT: &str = "　";
const MAX_TAIL_CELLS: u64 = 8;

/// 只在能够完整证明显示结果安全时修改文本的 RPG Maker 布局器。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ConservativeRpgMakerWriteBackTextLayouter;

impl RpgMakerWriteBackTextLayouter for ConservativeRpgMakerWriteBackTextLayouter {
    fn layout(&self, request: &RpgMakerWriteBackLayoutRequest) -> RpgMakerWriteBackLayoutOutcome {
        layout_request(request).unwrap_or(RpgMakerWriteBackLayoutOutcome::Manual)
    }
}

/// 使用调用方项目的实际行宽，对逐项对应的文本执行共享纯布局。
pub(crate) fn layout(
    region: RpgMakerWriteBackLayoutRegion,
    pairs: &[RpgMakerLayoutTextPair],
    profile: &RpgMakerWriteBackLayoutProfile,
) -> RpgMakerTextLayoutOutcome {
    let max_fullwidth_chars = match region {
        RpgMakerWriteBackLayoutRegion::DialogueBody => profile.dialogue_body(),
        RpgMakerWriteBackLayoutRegion::ScrollingText => profile.scrolling_text(),
        RpgMakerWriteBackLayoutRegion::HelpDescription => profile.help_description(),
    };
    // 三种显示区域都使用自身的显式宽度做安全兜底换行；找不到安全断点时转人工。
    layout_pairs_result(pairs, max_fullwidth_chars)
}

fn layout_request(
    request: &RpgMakerWriteBackLayoutRequest,
) -> Option<RpgMakerWriteBackLayoutOutcome> {
    let pairs = request
        .segments()
        .iter()
        .map(|segment| {
            let replacement = match segment.candidate() {
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal => None,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(translation) => {
                    Some(translation.clone())
                }
            };
            RpgMakerLayoutTextPair::new(segment.original_text().to_owned(), replacement)
        })
        .collect::<Vec<_>>();
    let Some(applied) = layout_pairs(&pairs, request.max_fullwidth_chars()) else {
        return Some(RpgMakerWriteBackLayoutOutcome::Manual);
    };
    let CoreAppliedLayout {
        lines_by_pair,
        inserted_line_breaks,
        inserted_fullwidth_indents,
    } = applied;
    let laid_out_segments = request
        .segments()
        .iter()
        .zip(lines_by_pair)
        .filter_map(|(segment, lines)| {
            matches!(
                segment.candidate(),
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_)
            )
            .then(|| {
                RpgMakerWriteBackLaidOutSegment::new(
                    segment.exact_location().clone(),
                    lines
                        .into_iter()
                        .map(|line| {
                            RpgMakerWriteBackLaidOutLine::new(
                                line.text,
                                line.source_semantic_line_index,
                            )
                        })
                        .collect(),
                )
                .expect("受信布局过程必须为每个数据库译文保留至少一个无换行显示行")
            })
        })
        .collect();

    Some(RpgMakerWriteBackLayoutOutcome::Applied(
        RpgMakerWriteBackAppliedLayout::new(
            request,
            laid_out_segments,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        )
        .expect("受信布局过程必须逐一返回请求中的数据库译文单元"),
    ))
}

fn layout_pairs_result(
    pairs: &[RpgMakerLayoutTextPair],
    max_fullwidth_chars: MaxFullwidthChars,
) -> RpgMakerTextLayoutOutcome {
    layout_pairs(pairs, max_fullwidth_chars).map_or_else(
        || {
            RpgMakerTextLayoutOutcome::Manual(RpgMakerAppliedTextLayout::new(
                pairs
                    .iter()
                    .map(|pair| pair.effective_text().to_owned())
                    .collect(),
                0,
                0,
            ))
        },
        |applied| {
            let CoreAppliedLayout {
                lines_by_pair,
                inserted_line_breaks,
                inserted_fullwidth_indents,
            } = applied;
            RpgMakerTextLayoutOutcome::Applied(RpgMakerAppliedTextLayout::new(
                lines_by_pair
                    .into_iter()
                    .map(|lines| {
                        lines
                            .into_iter()
                            .map(|line| line.text)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect(),
                inserted_line_breaks,
                inserted_fullwidth_indents,
            ))
        },
    )
}

struct CoreAppliedLayout {
    lines_by_pair: Vec<Vec<WorkingLine>>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

fn layout_pairs(
    pairs: &[RpgMakerLayoutTextPair],
    max_fullwidth_chars: MaxFullwidthChars,
) -> Option<CoreAppliedLayout> {
    let max_cells = u64::from(max_fullwidth_chars.get()) * 2;
    let mut working_segments = Vec::with_capacity(pairs.len());
    let mut inserted_line_breaks = 0usize;

    for pair in pairs {
        let translated = pair.replacement().is_some();
        let mut lines = Vec::new();
        for (source_semantic_line_index, hard_line) in pair.effective_text().split('\n').enumerate()
        {
            let tokens = scan_line(hard_line)?;
            if !translated {
                lines.push(WorkingLine::semantic(
                    hard_line.to_owned(),
                    source_semantic_line_index,
                ));
                continue;
            }

            if line_width(&tokens) <= max_cells {
                lines.push(WorkingLine::semantic(
                    hard_line.to_owned(),
                    source_semantic_line_index,
                ));
                continue;
            }
            let wrapped = wrap_line(&tokens, max_cells)?;
            inserted_line_breaks =
                inserted_line_breaks.checked_add(wrapped.len().saturating_sub(1))?;
            lines.extend(
                wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(index, text)| WorkingLine {
                        text,
                        automatically_generated_continuation: index > 0,
                        source_semantic_line_index,
                    }),
            );
        }
        working_segments.push(WorkingSegment { translated, lines });
    }

    let inserted_fullwidth_indents = apply_continuation_indents(&mut working_segments, max_cells)?;
    let lines_by_pair = working_segments
        .into_iter()
        .map(|segment| segment.lines)
        .collect();
    Some(CoreAppliedLayout {
        lines_by_pair,
        inserted_line_breaks,
        inserted_fullwidth_indents,
    })
}

struct WorkingSegment {
    translated: bool,
    lines: Vec<WorkingLine>,
}

struct WorkingLine {
    text: String,
    automatically_generated_continuation: bool,
    /// 模型语义硬行的序号；自动续行始终继承母行序号。
    source_semantic_line_index: usize,
}

impl WorkingLine {
    fn semantic(text: String, source_semantic_line_index: usize) -> Self {
        Self {
            text,
            automatically_generated_continuation: false,
            source_semantic_line_index,
        }
    }
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

fn scan_line(line: &str) -> Option<Vec<DisplayToken<'_>>> {
    if placeholder_token::contains_reserved_prefix(line) {
        return None;
    }

    let mut tokens = Vec::new();
    let mut offset = 0usize;
    while offset < line.len() {
        let remaining = &line[offset..];
        if remaining.starts_with('\\') {
            let control_length = control_sequence_length(remaining)?;
            tokens.push(DisplayToken {
                text: &remaining[..control_length],
                kind: DisplayTokenKind::Control,
                width_cells: 0,
            });
            offset += control_length;
            continue;
        }

        let grapheme = remaining
            .graphemes(true)
            .next()
            .expect("非空 UTF-8 后缀必须包含一个 grapheme");
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
            width_cells: UnicodeWidthStr::width_cjk(grapheme) as u64,
        });
        offset += grapheme.len();
    }
    Some(tokens)
}

fn control_sequence_length(input: &str) -> Option<usize> {
    debug_assert!(input.starts_with('\\'));
    let after_slash = input.get(1..)?;
    let first = after_slash.chars().next()?;
    if matches!(
        first,
        '{' | '}' | '\\' | '$' | '.' | '|' | '!' | '>' | '<' | '^'
    ) {
        return Some(1 + first.len_utf8());
    }

    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut command_length = 0usize;
    for character in after_slash.chars() {
        if character.is_ascii_alphabetic() {
            command_length += character.len_utf8();
        } else {
            break;
        }
    }
    for character in after_slash[command_length..].chars() {
        if character.is_ascii_digit() {
            command_length += character.len_utf8();
        } else {
            break;
        }
    }

    let after_command = &after_slash[command_length..];
    if let Some(parameter) = after_command.strip_prefix('[') {
        let closing_offset = parameter.find(']')?;
        let contents = &parameter[..closing_offset];
        if contents.chars().any(char::is_control) {
            return None;
        }
        return Some(1 + command_length + 1 + closing_offset + 1);
    }

    if command_length == 1 && first.eq_ignore_ascii_case(&'g') {
        let boundary = after_command.chars().next();
        if boundary.is_none_or(|character| !character.is_ascii_alphanumeric() && character != '[') {
            return Some(2);
        }
    }
    None
}

fn line_width(tokens: &[DisplayToken<'_>]) -> u64 {
    tokens
        .iter()
        .fold(0u64, |width, token| width.saturating_add(token.width_cells))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BreakCandidate {
    head_end: usize,
    tail_start: usize,
}

fn wrap_line(tokens: &[DisplayToken<'_>], max_cells: u64) -> Option<Vec<String>> {
    let candidates = collect_break_candidates(tokens);
    let min_tail_cells = MAX_TAIL_CELLS.min(max_cells.div_ceil(4));
    let mut memo = BTreeMap::new();
    let ranges = find_wrapped_ranges(tokens, &candidates, 0, max_cells, min_tail_cells, &mut memo)?;
    Some(
        ranges
            .into_iter()
            .map(|(start, end)| join_tokens(&tokens[start..end]))
            .collect(),
    )
}

fn find_wrapped_ranges(
    tokens: &[DisplayToken<'_>],
    candidates: &[BreakCandidate],
    start: usize,
    max_cells: u64,
    min_tail_cells: u64,
    memo: &mut BTreeMap<usize, Option<Vec<(usize, usize)>>>,
) -> Option<Vec<(usize, usize)>> {
    if let Some(cached) = memo.get(&start) {
        return cached.clone();
    }

    let remaining_width = range_width(tokens, start, tokens.len());
    if remaining_width <= max_cells {
        let result = (remaining_width >= min_tail_cells
            && valid_output_range(tokens, start, tokens.len()))
        .then_some(vec![(start, tokens.len())]);
        memo.insert(start, result.clone());
        return result;
    }

    let mut viable = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.head_end > start && candidate.tail_start > start)
        .filter_map(|candidate| {
            let width = range_width(tokens, start, candidate.head_end);
            (width <= max_cells
                && width.saturating_mul(100) >= max_cells.saturating_mul(45)
                && valid_output_range(tokens, start, candidate.head_end))
            .then_some((candidate, width))
        })
        .collect::<Vec<_>>();
    viable.sort_by(|(left, left_width), (right, right_width)| {
        right_width
            .cmp(left_width)
            .then_with(|| right.head_end.cmp(&left.head_end))
    });

    for (candidate, _) in viable {
        let Some(mut tail) = find_wrapped_ranges(
            tokens,
            candidates,
            candidate.tail_start,
            max_cells,
            min_tail_cells,
            memo,
        ) else {
            continue;
        };
        let mut ranges = Vec::with_capacity(tail.len() + 1);
        ranges.push((start, candidate.head_end));
        ranges.append(&mut tail);
        memo.insert(start, Some(ranges.clone()));
        return Some(ranges);
    }

    memo.insert(start, None);
    None
}

fn collect_break_candidates(tokens: &[DisplayToken<'_>]) -> Vec<BreakCandidate> {
    let mut candidates = BTreeSet::new();
    let mut index = 0usize;
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
    if !matches!(text, "," | "." | "!" | "?" | ";" | ":") {
        return false;
    }
    tokens
        .get(index + 1)
        .is_none_or(|next| next.kind == DisplayTokenKind::Whitespace || is_pair_closer(next.text))
}

fn valid_output_range(tokens: &[DisplayToken<'_>], start: usize, end: usize) -> bool {
    let mut significant = tokens[start..end]
        .iter()
        .filter(|token| token.kind == DisplayTokenKind::Visible);
    let Some(first) = significant.next() else {
        return false;
    };
    let last = significant.next_back().unwrap_or(first);
    !is_line_start_prohibited(first.text) && !is_pair_opener(last.text)
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
        "」" | "』" | "”" | "）" | "】" | "》" | "〉" | "〕" | "］" | "｝"
    )
}

fn range_width(tokens: &[DisplayToken<'_>], start: usize, end: usize) -> u64 {
    line_width(&tokens[start..end])
}

fn join_tokens(tokens: &[DisplayToken<'_>]) -> String {
    let capacity = tokens.iter().map(|token| token.text.len()).sum();
    let mut output = String::with_capacity(capacity);
    for token in tokens {
        output.push_str(token.text);
    }
    output
}

fn apply_continuation_indents(segments: &mut [WorkingSegment], max_cells: u64) -> Option<usize> {
    let mut wrapping_stack = Vec::new();
    let mut inserted = 0usize;

    for segment in segments {
        for line in &mut segment.lines {
            let insert_at = if segment.translated
                && line.automatically_generated_continuation
                && !wrapping_stack.is_empty()
            {
                let tokens = scan_line(&line.text)?;
                continuation_indent_position(&tokens)
            } else {
                None
            };
            if let Some(insert_at) = insert_at {
                line.text.insert_str(insert_at, FULLWIDTH_INDENT);
                inserted = inserted.checked_add(1)?;
            }

            let tokens = scan_line(&line.text)?;
            if segment.translated && line_width(&tokens) > max_cells {
                return None;
            }
            update_wrapping_stack(&tokens, &mut wrapping_stack);
        }
    }
    Some(inserted)
}

fn continuation_indent_position(tokens: &[DisplayToken<'_>]) -> Option<usize> {
    let mut insert_at = 0usize;
    let mut first_non_control = 0usize;
    while first_non_control < tokens.len()
        && tokens[first_non_control].kind == DisplayTokenKind::Control
    {
        insert_at += tokens[first_non_control].text.len();
        first_non_control += 1;
    }
    let first = tokens.get(first_non_control)?;
    matches!(first.kind, DisplayTokenKind::Visible).then_some(insert_at)
}

fn update_wrapping_stack(tokens: &[DisplayToken<'_>], wrapping_stack: &mut Vec<&'static str>) {
    let mut visible = tokens
        .iter()
        .filter(|token| token.kind == DisplayTokenKind::Visible);
    if wrapping_stack.is_empty() {
        let Some(first) = visible.next() else {
            return;
        };
        let Some(closer) = pair_closer(first.text) else {
            return;
        };
        wrapping_stack.push(closer);
    }

    for token in visible {
        if wrapping_stack
            .last()
            .is_some_and(|closer| *closer == token.text)
        {
            let _ = wrapping_stack.pop();
            if wrapping_stack.is_empty() {
                return;
            }
            continue;
        }
        if let Some(closer) = pair_closer(token.text) {
            wrapping_stack.push(closer);
        }
    }
}

fn pair_closer(opener: &str) -> Option<&'static str> {
    match opener {
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
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::project::{MaxFullwidthChars, RpgMakerWriteBackLayoutProfile};
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
    };
    use crate::rpg_maker::write_back::standard::{
        RpgMakerWriteBackLayoutRegion, RpgMakerWriteBackLayoutSegment,
    };

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试行宽应为正整数")
    }

    fn location(index: usize) -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![
                RpgMakerLocationStep::index(index),
                RpgMakerLocationStep::key("description"),
            ],
        )
    }

    fn segment(
        index: usize,
        original: &str,
        translation: Option<&str>,
    ) -> RpgMakerWriteBackLayoutSegment {
        RpgMakerWriteBackLayoutSegment::from_line_at(
            &location(999),
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键应合法")),
            location(index),
            original.to_owned(),
            translation.map(str::to_owned),
        )
    }

    fn request(
        max_fullwidth_chars: u32,
        segments: Vec<RpgMakerWriteBackLayoutSegment>,
    ) -> RpgMakerWriteBackLayoutRequest {
        RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::HelpDescription,
            width(max_fullwidth_chars),
            segments,
        )
    }

    fn applied(request: &RpgMakerWriteBackLayoutRequest) -> RpgMakerWriteBackAppliedLayout {
        let RpgMakerWriteBackLayoutOutcome::Applied(applied) =
            ConservativeRpgMakerWriteBackTextLayouter.layout(request)
        else {
            panic!("测试请求应安全布局")
        };
        applied
    }

    fn line_texts(segment: &RpgMakerWriteBackLaidOutSegment) -> Vec<&str> {
        segment.lines().iter().map(|line| line.text()).collect()
    }

    fn source_semantic_line_indexes(segment: &RpgMakerWriteBackLaidOutSegment) -> Vec<usize> {
        segment
            .lines()
            .iter()
            .map(|line| line.source_semantic_line_index())
            .collect()
    }

    fn assert_manual(request: &RpgMakerWriteBackLayoutRequest) {
        assert_eq!(
            ConservativeRpgMakerWriteBackTextLayouter.layout(request),
            RpgMakerWriteBackLayoutOutcome::Manual
        );
    }

    fn profile(dialogue: u32, scrolling: u32, help: u32) -> RpgMakerWriteBackLayoutProfile {
        RpgMakerWriteBackLayoutProfile::new(width(dialogue), width(scrolling), width(help))
    }

    #[test]
    fn shared_layout_uses_the_selected_region_from_the_actual_profile() {
        let pairs = vec![RpgMakerLayoutTextPair::new(
            "原文".to_owned(),
            Some("甲乙丙".to_owned()),
        )];
        let profile = profile(3, 2, 2);

        let RpgMakerTextLayoutOutcome::Applied(dialogue) = layout(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            &pairs,
            &profile,
        ) else {
            panic!("对话实际宽度足以容纳文本")
        };
        assert_eq!(dialogue.texts(), ["甲乙丙"]);
        for outcome in [
            layout(
                RpgMakerWriteBackLayoutRegion::ScrollingText,
                &pairs,
                &profile,
            ),
            layout(
                RpgMakerWriteBackLayoutRegion::HelpDescription,
                &pairs,
                &profile,
            ),
        ] {
            let RpgMakerTextLayoutOutcome::Manual(manual) = outcome else {
                panic!("宽度不足且不能安全换行时应转人工")
            };
            assert_eq!(manual.texts(), ["甲乙丙"]);
            assert_eq!(manual.inserted_line_breaks(), 0);
            assert_eq!(manual.inserted_fullwidth_indents(), 0);
        }
    }

    #[test]
    fn single_line_help_source_uses_help_width_for_safe_auto_wrap() {
        let pairs = vec![RpgMakerLayoutTextPair::new(
            "单行原文".to_owned(),
            Some("甲乙，丙丁。".to_owned()),
        )];

        let RpgMakerTextLayoutOutcome::Applied(applied) = layout(
            RpgMakerWriteBackLayoutRegion::HelpDescription,
            &pairs,
            &profile(20, 20, 3),
        ) else {
            panic!("单行帮助说明应按帮助宽度安全换行")
        };

        assert_eq!(applied.texts(), ["甲乙，\n丙丁。"]);
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn help_request_wraps_a_single_line_source_at_the_explicit_help_width() {
        let request = request(3, vec![segment(1, "单行原文", Some("甲乙，丙丁。"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["甲乙，", "丙丁。"][..]);
        assert_eq!(source_semantic_line_indexes(&applied.segments()[0]), [0, 0]);
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn shared_layout_returns_aligned_texts_and_inserted_counts() {
        let pairs = vec![
            RpgMakerLayoutTextPair::new("原一".to_owned(), Some("「甲乙，丙丁」".to_owned())),
            RpgMakerLayoutTextPair::new("冻结原文".to_owned(), None),
        ];

        let RpgMakerTextLayoutOutcome::Applied(applied) = layout(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            &pairs,
            &profile(4, 4, 4),
        ) else {
            panic!("共享布局请求应可安全处理")
        };

        assert_eq!(applied.texts(), ["「甲乙，\n　丙丁」", "冻结原文"]);
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 1);
        let (texts, line_breaks, indents) = applied.into_parts();
        assert_eq!(texts.len(), pairs.len());
        assert_eq!((line_breaks, indents), (1, 1));
    }

    #[test]
    fn shared_manual_layout_returns_current_texts_in_input_order() {
        let pairs = vec![
            RpgMakerLayoutTextPair::new("原文一".to_owned(), Some("甲乙丙".to_owned())),
            RpgMakerLayoutTextPair::new("原文二".to_owned(), None),
        ];

        let RpgMakerTextLayoutOutcome::Manual(manual) = layout(
            RpgMakerWriteBackLayoutRegion::HelpDescription,
            &pairs,
            &profile(2, 2, 2),
        ) else {
            panic!("无法安全布局的帮助文本应转人工")
        };

        assert_eq!(manual.texts(), ["甲乙丙", "原文二"]);
        assert_eq!(manual.inserted_line_breaks(), 0);
        assert_eq!(manual.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn counts_cjk_ascii_combining_emoji_and_ambiguous_width_in_half_cells() {
        let tokens = scan_line("甲A e\u{301}👨‍👩‍👧‍👦·").expect("文本应可扫描");

        assert_eq!(line_width(&tokens), 9);
    }

    #[test]
    fn canonical_and_plugin_controls_are_zero_width() {
        let tokens = scan_line(r"\C[1]甲\.\G\SE[11-nb]乙").expect("控制符应可扫描");

        assert_eq!(line_width(&tokens), 4);
    }

    #[test]
    fn uncertain_controls_and_residual_att_tokens_make_the_whole_unit_manual() {
        for text in [r"甲\broken乙", r"甲\C[1乙", "甲\\", "甲⟦ATT_X_0001⟧乙"] {
            assert_manual(&request(20, vec![segment(1, "原文", Some(text))]));
        }
    }

    #[test]
    fn cr_tab_and_other_control_characters_make_the_whole_unit_manual() {
        for text in ["甲\r乙", "甲\t乙", "甲\u{7}乙", "甲\u{2028}乙"] {
            assert_manual(&request(20, vec![segment(1, "原文", Some(text))]));
        }
    }

    #[test]
    fn non_breaking_spaces_are_not_used_as_word_boundaries() {
        assert_manual(&request(2, vec![segment(1, "原文", Some("ab\u{a0}cd"))]));
    }

    #[test]
    fn wraps_at_chinese_punctuation_without_moving_text_between_segments() {
        let request = request(
            3,
            vec![
                segment(1, "原一", Some("甲乙，丙丁。")),
                segment(2, "原二", Some("戊己，庚辛。")),
            ],
        );

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["甲乙，", "丙丁。"][..]);
        assert_eq!(line_texts(&applied.segments()[1]), ["戊己，", "庚辛。"][..]);
        assert_eq!(applied.inserted_line_breaks(), 2);
    }

    #[test]
    fn replaces_a_horizontal_whitespace_boundary_with_a_line_break() {
        let request = request(2, vec![segment(1, "原文", Some("abcd  efgh"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["abcd", "efgh"][..]);
        assert_eq!(applied.inserted_line_breaks(), 1);
    }

    #[test]
    fn refuses_a_whitespace_break_that_places_ascii_punctuation_at_line_start() {
        assert_manual(&request(2, vec![segment(1, "原文", Some("abcd ,efg"))]));
    }

    #[test]
    fn refuses_to_hard_split_an_unsplittable_run() {
        assert_manual(&request(3, vec![segment(1, "原文", Some("甲乙丙丁"))]));
    }

    #[test]
    fn rejects_a_break_before_the_forty_five_percent_readability_threshold() {
        assert_manual(&request(
            10,
            vec![segment(1, "原文", Some("甲甲甲，乙乙乙乙乙乙乙乙"))],
        ));
    }

    #[test]
    fn rejects_a_tiny_final_tail() {
        assert_manual(&request(
            6,
            vec![segment(1, "原文", Some("甲乙丙丁戊，乙"))],
        ));
    }

    #[test]
    fn keeps_closing_pair_with_preceding_punctuation_line() {
        let request = request(4, vec![segment(1, "原文", Some("甲乙，」丙丁"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["甲乙，」", "丙丁"][..]);
    }

    #[test]
    fn refuses_a_break_that_would_leave_an_opening_pair_at_line_end() {
        assert_manual(&request(3, vec![segment(1, "原文", Some("甲乙（ 丙丁丁"))]));
    }

    #[test]
    fn preserves_database_hard_lines_without_counting_them_as_inserted() {
        let request = request(2, vec![segment(1, "原文", Some("甲乙\n\n丙丁"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["甲乙", "", "丙丁"][..]);
        assert_eq!(applied.inserted_line_breaks(), 0);
    }

    #[test]
    fn inserted_line_breaks_count_only_automatic_wraps_after_semantic_lines() {
        let request = request(4, vec![segment(1, "原文", Some("甲乙\n「甲乙，丙丁」"))]);

        let applied = applied(&request);

        assert_eq!(
            line_texts(&applied.segments()[0]),
            ["甲乙", "「甲乙，", "　丙丁」"][..]
        );
        assert_eq!(
            source_semantic_line_indexes(&applied.segments()[0]),
            [0, 1, 1]
        );
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 1);
    }

    #[test]
    fn overwidth_line_without_a_safe_break_remains_manual() {
        assert_manual(&request(2, vec![segment(1, "原文", Some("甲乙丙"))]));
    }

    #[test]
    fn wraps_before_adding_fullwidth_continuation_indent() {
        let request = request(4, vec![segment(1, "原文", Some("「甲乙，丙丁」"))]);

        let applied = applied(&request);

        assert_eq!(
            line_texts(&applied.segments()[0]),
            ["「甲乙，", "　丙丁」"][..]
        );
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 1);
    }

    #[test]
    fn preserves_semantic_line_after_leading_controls_byte_for_byte() {
        let request = request(3, vec![segment(1, "原文", Some("「甲\n\\SE[2]乙」"))]);

        let applied = applied(&request);

        assert_eq!(
            line_texts(&applied.segments()[0]),
            ["「甲", r"\SE[2]乙」"][..]
        );
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn does_not_duplicate_existing_half_or_fullwidth_whitespace() {
        for continuation in [" 乙」", "　乙」", "\u{a0}乙」"] {
            let text = format!("「甲\n{continuation}");
            let request = request(3, vec![segment(1, "原文", Some(&text))]);

            let applied = applied(&request);

            assert_eq!(applied.segments()[0].lines()[1].text(), continuation);
            assert_eq!(applied.inserted_fullwidth_indents(), 0);
        }
    }

    #[test]
    fn line_mid_opening_pair_does_not_start_continuation_indent() {
        let request = request(5, vec![segment(1, "原文", Some("他说「甲\n乙」"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["他说「甲", "乙」"][..]);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn semantic_lines_observe_cross_segment_state_without_inserting_indents() {
        let request = request(
            5,
            vec![
                segment(1, "原一", Some("「甲")),
                segment(2, "缺译", None),
                segment(3, "原三", Some("【乙\n丙】」")),
            ],
        );

        let applied = applied(&request);

        assert_eq!(applied.segments().len(), 2);
        assert_eq!(line_texts(&applied.segments()[0]), ["「甲"][..]);
        assert_eq!(line_texts(&applied.segments()[1]), ["【乙", "丙】」"][..]);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn frozen_original_can_close_wrapping_state_without_being_modified() {
        let request = request(
            4,
            vec![
                segment(1, "原一", Some("「甲")),
                segment(2, "缺译」", None),
                segment(3, "原三", Some("乙")),
            ],
        );

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[1]), ["乙"][..]);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }

    #[test]
    fn malformed_control_in_frozen_original_makes_state_observation_manual() {
        assert_manual(&request(
            20,
            vec![segment(1, "原一", Some("译文")), segment(2, "缺译\\", None)],
        ));
    }

    #[test]
    fn semantic_hard_line_is_not_indented_or_rejected_by_width() {
        let request = request(3, vec![segment(1, "原文", Some("「甲\n乙丙」"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["「甲", "乙丙」"][..]);
        assert_eq!(applied.inserted_line_breaks(), 0);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
    }
}
