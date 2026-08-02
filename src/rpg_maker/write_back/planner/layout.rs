//! RPG Maker 写回文本的保守显示布局。

use std::collections::BTreeSet;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    RpgMakerLayoutTextPair, RpgMakerWriteBackAppliedLayout, RpgMakerWriteBackLaidOutLine,
    RpgMakerWriteBackLaidOutSegment, RpgMakerWriteBackLayoutCandidate,
    RpgMakerWriteBackLayoutOutcome, RpgMakerWriteBackLayoutRequest, RpgMakerWriteBackTextLayouter,
};
use crate::rpg_maker::project::MaxFullwidthChars;
use crate::translation::placeholder_token;

const FULLWIDTH_INDENT: &str = "　";
const CONTINUATION_INDENT_CELLS: u64 = 2;
const MAX_TAIL_CELLS: u64 = 8;

/// 只在能够完整证明显示结果安全时修改文本的 RPG Maker 布局器。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ConservativeRpgMakerWriteBackTextLayouter;

impl RpgMakerWriteBackTextLayouter for ConservativeRpgMakerWriteBackTextLayouter {
    fn layout(&self, request: &RpgMakerWriteBackLayoutRequest) -> RpgMakerWriteBackLayoutOutcome {
        layout_request(request).unwrap_or(RpgMakerWriteBackLayoutOutcome::Manual)
    }
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
            width_cells: u64::try_from(UnicodeWidthStr::width_cjk(grapheme))
                .expect("当前目标平台的字素宽度必须能用 u64 表达"),
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
    let mut observation = WrapSearchObservation::default();
    let index = WrapLineIndex::new(tokens, &mut observation);
    let ranges = find_wrapped_ranges(
        &index,
        &candidates,
        max_cells,
        min_tail_cells,
        &mut observation,
    )?;
    Some(
        ranges
            .into_iter()
            .map(|(start, end)| join_tokens(&tokens[start..end]))
            .collect(),
    )
}

#[derive(Clone, Copy, Debug)]
enum WrapDecision {
    Final,
    Break(BreakCandidate),
}

/// 为一条显示行预计算可重复使用的宽度与可见字符索引。
///
/// 宽度前缀使用 `u128`，最后再饱和收窄到 `u64`，因此与原先对任意子区间逐项
/// `saturating_add` 的结果一致，同时避免搜索每个断点时重新扫描同一批 token。
struct WrapLineIndex<'tokens, 'text> {
    tokens: &'tokens [DisplayToken<'text>],
    width_prefix: Vec<u128>,
    next_visible: Vec<usize>,
    previous_visible: Vec<usize>,
}

impl<'tokens, 'text> WrapLineIndex<'tokens, 'text> {
    fn new(
        tokens: &'tokens [DisplayToken<'text>],
        observation: &mut WrapSearchObservation,
    ) -> Self {
        let mut width_prefix: Vec<u128> = Vec::with_capacity(tokens.len() + 1);
        width_prefix.push(0);
        for token in tokens {
            observation.observe_prefix_token();
            let accumulated = width_prefix
                .last()
                .copied()
                .expect("宽度前缀必须包含零起点")
                .checked_add(u128::from(token.width_cells))
                .expect("单行 token 总宽度不可能超过 u128");
            width_prefix.push(accumulated);
        }

        let sentinel = tokens.len();
        let mut next_visible = vec![sentinel; tokens.len() + 1];
        for index in (0..tokens.len()).rev() {
            next_visible[index] = if tokens[index].kind == DisplayTokenKind::Visible {
                index
            } else {
                next_visible[index + 1]
            };
        }

        let mut previous_visible = vec![sentinel; tokens.len() + 1];
        for end in 1..=tokens.len() {
            previous_visible[end] = if tokens[end - 1].kind == DisplayTokenKind::Visible {
                end - 1
            } else {
                previous_visible[end - 1]
            };
        }

        Self {
            tokens,
            width_prefix,
            next_visible,
            previous_visible,
        }
    }

    fn range_width(
        &self,
        start: usize,
        end: usize,
        observation: &mut WrapSearchObservation,
    ) -> u64 {
        observation.observe_width_query();
        let width = self.width_prefix[end] - self.width_prefix[start];
        u64::try_from(width).expect("当前目标平台的布局宽度必须能用 u64 表达")
    }

    fn valid_output_range(
        &self,
        start: usize,
        end: usize,
        observation: &mut WrapSearchObservation,
    ) -> bool {
        observation.observe_range_validation();
        let first = self.next_visible[start];
        if first >= end {
            return false;
        }
        let last = self.previous_visible[end];
        debug_assert!(last >= first && last < end);
        !is_line_start_prohibited(self.tokens[first].text)
            && !is_pair_opener(self.tokens[last].text)
    }
}

#[derive(Default)]
struct WrapSearchObservation {
    #[cfg(test)]
    prefix_tokens: usize,
    #[cfg(test)]
    states: usize,
    #[cfg(test)]
    width_queries: usize,
    #[cfg(test)]
    candidate_checks: usize,
    #[cfg(test)]
    range_validations: usize,
}

impl WrapSearchObservation {
    #[inline]
    fn observe_prefix_token(&mut self) {
        #[cfg(test)]
        {
            self.prefix_tokens += 1;
        }
    }

    #[inline]
    fn observe_state(&mut self) {
        #[cfg(test)]
        {
            self.states += 1;
        }
    }

    #[inline]
    fn observe_width_query(&mut self) {
        #[cfg(test)]
        {
            self.width_queries += 1;
        }
    }

    #[inline]
    fn observe_candidate_check(&mut self) {
        #[cfg(test)]
        {
            self.candidate_checks += 1;
        }
    }

    #[inline]
    fn observe_range_validation(&mut self) {
        #[cfg(test)]
        {
            self.range_validations += 1;
        }
    }
}

fn find_wrapped_ranges(
    index: &WrapLineIndex<'_, '_>,
    candidates: &[BreakCandidate],
    max_cells: u64,
    min_tail_cells: u64,
    observation: &mut WrapSearchObservation,
) -> Option<Vec<(usize, usize)>> {
    let token_count = index.tokens.len();
    let mut starts = Vec::with_capacity(candidates.len() + 1);
    starts.push(0);
    starts.extend(candidates.iter().map(|candidate| candidate.tail_start));
    starts.sort_unstable();
    starts.dedup();

    // 每个断点只会前进，因此从后向前计算即可替代递归和整条结果链克隆。
    let mut decisions = vec![None; token_count + 1];
    for &start in starts.iter().rev() {
        observation.observe_state();
        let line_max_cells = if start == 0 {
            max_cells
        } else {
            max_cells.saturating_sub(CONTINUATION_INDENT_CELLS)
        };
        let remaining_width = index.range_width(start, token_count, observation);
        if remaining_width <= line_max_cells {
            if remaining_width >= min_tail_cells
                && index.valid_output_range(start, token_count, observation)
            {
                decisions[start] = Some(WrapDecision::Final);
            }
            continue;
        }

        let first_after_start = candidates.partition_point(|candidate| candidate.head_end <= start);
        let fits_end = first_after_start
            + candidates[first_after_start..].partition_point(|candidate| {
                index.range_width(start, candidate.head_end, observation) <= line_max_cells
            });
        let minimum_width_end = line_max_cells.saturating_mul(45);
        let viable_start = first_after_start
            + candidates[first_after_start..fits_end].partition_point(|candidate| {
                index
                    .range_width(start, candidate.head_end, observation)
                    .saturating_mul(100)
                    < minimum_width_end
            });

        // 原实现按「宽度降序、head_end 降序」稳定排序。宽度随 head_end 单调，
        // 因此倒序访问 head_end 分组、组内仍按原始 tail_start 顺序访问即可完全复现。
        let mut group_end = fits_end;
        'candidate_groups: while group_end > viable_start {
            let head_end = candidates[group_end - 1].head_end;
            let group_start = viable_start
                + candidates[viable_start..group_end]
                    .partition_point(|candidate| candidate.head_end < head_end);
            for &candidate in &candidates[group_start..group_end] {
                observation.observe_candidate_check();
                if candidate.tail_start <= start
                    || !index.valid_output_range(start, candidate.head_end, observation)
                    || decisions[candidate.tail_start].is_none()
                {
                    continue;
                }
                decisions[start] = Some(WrapDecision::Break(candidate));
                break 'candidate_groups;
            }
            group_end = group_start;
        }
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    loop {
        match decisions.get(start).copied().flatten()? {
            WrapDecision::Final => {
                ranges.push((start, token_count));
                return Some(ranges);
            }
            WrapDecision::Break(candidate) => {
                ranges.push((start, candidate.head_end));
                start = candidate.tail_start;
            }
        }
    }
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
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::project::MaxFullwidthChars;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
    };
    use crate::rpg_maker::write_back::planner::{
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

    fn reference_wrap_line(tokens: &[DisplayToken<'_>], max_cells: u64) -> Option<Vec<String>> {
        let candidates = collect_break_candidates(tokens);
        let min_tail_cells = MAX_TAIL_CELLS.min(max_cells.div_ceil(4));
        let mut memo = std::collections::BTreeMap::new();
        let ranges = reference_find_wrapped_ranges(
            tokens,
            &candidates,
            0,
            max_cells,
            min_tail_cells,
            &mut memo,
        )?;
        Some(
            ranges
                .into_iter()
                .map(|(start, end)| join_tokens(&tokens[start..end]))
                .collect(),
        )
    }

    fn reference_find_wrapped_ranges(
        tokens: &[DisplayToken<'_>],
        candidates: &[BreakCandidate],
        start: usize,
        max_cells: u64,
        min_tail_cells: u64,
        memo: &mut std::collections::BTreeMap<usize, Option<Vec<(usize, usize)>>>,
    ) -> Option<Vec<(usize, usize)>> {
        if let Some(cached) = memo.get(&start) {
            return cached.clone();
        }

        let remaining_width = line_width(&tokens[start..]);
        let line_max_cells = if start == 0 {
            max_cells
        } else {
            max_cells.saturating_sub(CONTINUATION_INDENT_CELLS)
        };
        if remaining_width <= line_max_cells {
            let result = (remaining_width >= min_tail_cells
                && reference_valid_output_range(tokens, start, tokens.len()))
            .then_some(vec![(start, tokens.len())]);
            memo.insert(start, result.clone());
            return result;
        }

        let mut viable = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.head_end > start && candidate.tail_start > start)
            .filter_map(|candidate| {
                let width = line_width(&tokens[start..candidate.head_end]);
                (width <= line_max_cells
                    && width.saturating_mul(100) >= line_max_cells.saturating_mul(45)
                    && reference_valid_output_range(tokens, start, candidate.head_end))
                .then_some((candidate, width))
            })
            .collect::<Vec<_>>();
        viable.sort_by(|(left, left_width), (right, right_width)| {
            right_width
                .cmp(left_width)
                .then_with(|| right.head_end.cmp(&left.head_end))
        });

        for (candidate, _) in viable {
            let Some(mut tail) = reference_find_wrapped_ranges(
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

    fn reference_valid_output_range(tokens: &[DisplayToken<'_>], start: usize, end: usize) -> bool {
        let mut significant = tokens[start..end]
            .iter()
            .filter(|token| token.kind == DisplayTokenKind::Visible);
        let Some(first) = significant.next() else {
            return false;
        };
        let last = significant.next_back().unwrap_or(first);
        !is_line_start_prohibited(first.text) && !is_pair_opener(last.text)
    }

    fn observed_wrap_line(
        tokens: &[DisplayToken<'_>],
        max_cells: u64,
    ) -> (Option<Vec<String>>, WrapSearchObservation) {
        let candidates = collect_break_candidates(tokens);
        let min_tail_cells = MAX_TAIL_CELLS.min(max_cells.div_ceil(4));
        let mut observation = WrapSearchObservation::default();
        let index = WrapLineIndex::new(tokens, &mut observation);
        let wrapped = find_wrapped_ranges(
            &index,
            &candidates,
            max_cells,
            min_tail_cells,
            &mut observation,
        )
        .map(|ranges| {
            ranges
                .into_iter()
                .map(|(start, end)| join_tokens(&tokens[start..end]))
                .collect()
        });
        (wrapped, observation)
    }

    #[test]
    fn help_request_wraps_a_single_line_source_at_the_explicit_help_width() {
        let request = request(4, vec![segment(1, "单行原文", Some("甲乙，丙丁。"))]);

        let applied = applied(&request);

        assert_eq!(line_texts(&applied.segments()[0]), ["甲乙，", "丙丁。"][..]);
        assert_eq!(source_semantic_line_indexes(&applied.segments()[0]), [0, 0]);
        assert_eq!(applied.inserted_line_breaks(), 1);
        assert_eq!(applied.inserted_fullwidth_indents(), 0);
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
    fn indexed_wrap_search_matches_the_reference_selection_semantics() {
        const FRAGMENTS: &[&str] = &[
            "甲", "乙", "A", "，", "。", " ", "  ", "(", ")", "「", "」", "（", "）", "…",
            r"\C[1]", "\u{a0}",
        ];
        let mut state = 0x7a31_49d2_u32;
        for case_index in 0..64usize {
            let fragment_count = 8 + case_index % 25;
            let mut line = String::new();
            for _ in 0..fragment_count {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                line.push_str(FRAGMENTS[(state as usize) % FRAGMENTS.len()]);
            }
            let tokens = scan_line(&line).expect("固定片段生成的文本必须可扫描");
            for max_cells in 1..=24 {
                assert_eq!(
                    wrap_line(&tokens, max_cells),
                    reference_wrap_line(&tokens, max_cells),
                    "索引搜索必须匹配独立参考，case={case_index}, max_cells={max_cells}, line={line:?}"
                );
            }
        }
    }

    #[test]
    fn wrap_search_precomputes_width_once_and_has_at_most_quadratic_candidate_work() {
        let mut line = "甲乙，".repeat(512);
        line.push_str("丙丁。");
        let tokens = scan_line(&line).expect("复杂度样本必须可扫描");
        let candidates = collect_break_candidates(&tokens);

        let (wrapped, observation) = observed_wrap_line(&tokens, 12);

        assert!(wrapped.is_some(), "复杂度样本必须存在安全换行方案");
        assert_eq!(
            observation.prefix_tokens,
            tokens.len(),
            "token 宽度只能在线性前缀构建时读取一次"
        );
        assert!(
            observation.states <= candidates.len() + 1,
            "搜索状态只能来自起点和断点 tail_start"
        );
        assert!(
            observation.candidate_checks <= observation.states.saturating_mul(candidates.len()),
            "每个状态不得重复检查同一个断点候选"
        );
        assert!(
            observation.width_queries
                <= observation
                    .states
                    .saturating_mul(1 + 2 * usize::BITS as usize),
            "宽度查找应通过两次二分保持在每状态 O(log candidate)"
        );
        assert!(
            observation.range_validations
                <= observation
                    .candidate_checks
                    .saturating_add(observation.states),
            "可见字符边界只能按候选或最终尾段做 O(1) 查询"
        );
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
            4,
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
        let request = request(3, vec![segment(1, "原文", Some("abcd  efgh"))]);

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
    fn reserves_two_cells_for_every_automatic_continuation() {
        let tokens = scan_line("abcd efgh").expect("测试文本应可扫描");

        assert_eq!(
            wrap_line(&tokens, 5),
            None,
            "续行正文即使单独不超宽，也必须为可能插入的全角缩进预留两个 cell"
        );
        assert_eq!(
            wrap_line(&tokens, 6),
            Some(vec!["abcd".to_owned(), "efgh".to_owned()])
        );
    }

    #[test]
    fn ascii_parentheses_share_pairing_and_line_boundary_rules() {
        let layout_request = request(4, vec![segment(1, "原文", Some("(甲乙，丙丁)"))]);
        let applied = applied(&layout_request);

        assert_eq!(
            line_texts(&applied.segments()[0]),
            ["(甲乙，", "　丙丁)"][..]
        );
        assert_eq!(applied.inserted_fullwidth_indents(), 1);
        assert_manual(&request(3, vec![segment(1, "原文", Some("abcd( efgh"))]));
        assert_manual(&request(4, vec![segment(1, "原文", Some("abcd )efgh"))]));
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
