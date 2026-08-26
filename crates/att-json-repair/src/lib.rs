//! 面向不可信模型输出的、保留顺序与重复字段的 JSON 修复器。
//!
//! 本 crate 只把近似 JSON 转换成严格 JSON 文本。它不解释业务 schema，也不会通过
//! map 覆盖重复字段。调用方应在修复后继续执行自己的结构和业务校验。
//! 行为调查固定参考 Python `json_repair` 的 `600ede6` 提交；本实现和测试均独立编写。

use std::borrow::Cow;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::ops::Range;

const CANCELLATION_INTERVAL: usize = 64 * 1024;

/// 控制允许采用的修复强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPolicy {
    /// 只执行由当前语法状态唯一确定的修复。
    Conservative,
    /// 允许选择第一个候选，并修复少量仍有合理默认解释的输入。
    BestEffort,
}

/// 一项已经执行的文本修复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    kind: RepairKind,
    original_range: Range<usize>,
    output_range: Range<usize>,
}

impl Repair {
    /// 返回稳定的修复类别。
    #[must_use]
    pub const fn kind(&self) -> RepairKind {
        self.kind
    }

    /// 返回修复涉及的原始输入字节范围。
    #[must_use]
    pub fn original_range(&self) -> Range<usize> {
        self.original_range.clone()
    }

    /// 返回修复产生或删除内容所在的输出字节范围。
    #[must_use]
    pub fn output_range(&self) -> Range<usize> {
        self.output_range.clone()
    }
}

/// 修复类别。每个 variant 的 [`RepairKind::code`] 是稳定的持久化名称。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairKind {
    RemovedByteOrderMark,
    RemovedMarkdownFence,
    RemovedSurroundingText,
    RemovedComment,
    NormalizedWhitespace,
    NormalizedQuote,
    EscapedInternalQuote,
    EscapedControlCharacter,
    EscapedInvalidEscape,
    QuotedBareKey,
    QuotedBareValue,
    InsertedColon,
    RemovedColon,
    InsertedComma,
    RemovedComma,
    InsertedClosingDelimiter,
    InsertedClosingQuote,
    NormalizedLiteral,
    NormalizedNumber,
}

impl RepairKind {
    /// 返回适合诊断和持久化的稳定 ASCII 名称。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RemovedByteOrderMark => "removed_byte_order_mark",
            Self::RemovedMarkdownFence => "removed_markdown_fence",
            Self::RemovedSurroundingText => "removed_surrounding_text",
            Self::RemovedComment => "removed_comment",
            Self::NormalizedWhitespace => "normalized_whitespace",
            Self::NormalizedQuote => "normalized_quote",
            Self::EscapedInternalQuote => "escaped_internal_quote",
            Self::EscapedControlCharacter => "escaped_control_character",
            Self::EscapedInvalidEscape => "escaped_invalid_escape",
            Self::QuotedBareKey => "quoted_bare_key",
            Self::QuotedBareValue => "quoted_bare_value",
            Self::InsertedColon => "inserted_colon",
            Self::RemovedColon => "removed_colon",
            Self::InsertedComma => "inserted_comma",
            Self::RemovedComma => "removed_comma",
            Self::InsertedClosingDelimiter => "inserted_closing_delimiter",
            Self::InsertedClosingQuote => "inserted_closing_quote",
            Self::NormalizedLiteral => "normalized_literal",
            Self::NormalizedNumber => "normalized_number",
        }
    }
}

impl fmt::Display for RepairKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// 修复后的严格 JSON 及其位置映射。
#[derive(Debug, Clone)]
pub struct RepairOutput<'a> {
    json: Cow<'a, str>,
    repairs: Vec<Repair>,
    source_map: SourceMap,
}

impl<'a> RepairOutput<'a> {
    /// 返回严格 JSON 文本。
    #[must_use]
    pub fn json(&self) -> &str {
        &self.json
    }

    /// 返回按执行顺序记录的修复。
    #[must_use]
    pub fn repairs(&self) -> &[Repair] {
        &self.repairs
    }

    /// 把输出字节边界映射回原始输入字节边界。
    ///
    /// `output_offset` 可以等于输出长度；超过输出长度时返回 `None`。
    #[must_use]
    pub fn original_offset(&self, output_offset: usize) -> Option<usize> {
        self.source_map.original_offset(output_offset)
    }

    /// 取得修复后的文本。完全无需修改时返回借用的原始输入。
    #[must_use]
    pub fn into_json(self) -> Cow<'a, str> {
        self.json
    }
}

/// 无法安全修复时的错误类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairErrorKind {
    NoJsonCandidate,
    MultipleJsonCandidates,
    MultipleRootValues,
    AmbiguousStringQuote,
    UnterminatedString,
    MissingValue,
    InvalidNumber,
    InvalidToken,
    UnexpectedClosingDelimiter,
}

impl RepairErrorKind {
    /// 返回适合诊断的稳定 ASCII 名称。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoJsonCandidate => "no_json_candidate",
            Self::MultipleJsonCandidates => "multiple_json_candidates",
            Self::MultipleRootValues => "multiple_root_values",
            Self::AmbiguousStringQuote => "ambiguous_string_quote",
            Self::UnterminatedString => "unterminated_string",
            Self::MissingValue => "missing_value",
            Self::InvalidNumber => "invalid_number",
            Self::InvalidToken => "invalid_token",
            Self::UnexpectedClosingDelimiter => "unexpected_closing_delimiter",
        }
    }
}

impl fmt::Display for RepairErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// 无法安全修复的输入错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairError {
    kind: RepairErrorKind,
    original_offset: usize,
}

impl RepairError {
    /// 返回稳定的错误类别。
    #[must_use]
    pub const fn kind(&self) -> RepairErrorKind {
        self.kind
    }

    /// 返回错误所在的原始输入字节位置。
    #[must_use]
    pub const fn original_offset(&self) -> usize {
        self.original_offset
    }

    fn new(kind: RepairErrorKind, original_offset: usize) -> Self {
        Self {
            kind,
            original_offset,
        }
    }
}

impl fmt::Display for RepairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "JSON repair failed with {} at byte {}",
            self.kind.code(),
            self.original_offset
        )
    }
}

impl Error for RepairError {}

/// 修复一段近似 JSON 文本。
pub fn repair(input: &str, policy: RepairPolicy) -> Result<RepairOutput<'_>, RepairError> {
    match repair_with_cancellation(input, policy, || Ok::<_, Infallible>(())) {
        Ok(result) => result,
        Err(error) => match error {},
    }
}

/// 修复一段近似 JSON 文本，并允许调用方周期性检查取消状态。
///
/// 外层 `Result` 只传播取消检查错误；内层 `Result` 表示 JSON 是否能够安全修复。
pub fn repair_with_cancellation<E>(
    input: &str,
    policy: RepairPolicy,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<RepairOutput<'_>, RepairError>, E> {
    let mut cancellation = Cancellation::new(ensure_running);
    cancellation.start()?;
    let candidate = match select_candidate(input, policy, &mut cancellation)? {
        Ok(candidate) => candidate,
        Err(error) => return Ok(Err(error)),
    };
    let parser = Parser::new(input, candidate, policy, &mut cancellation)?;
    parser.parse()
}

struct Cancellation<F> {
    ensure_running: F,
    processed: usize,
    next_check: usize,
}

impl<F> Cancellation<F> {
    fn new(ensure_running: F) -> Self {
        Self {
            ensure_running,
            processed: 0,
            next_check: CANCELLATION_INTERVAL,
        }
    }
}

impl<F, E> Cancellation<F>
where
    F: FnMut() -> Result<(), E>,
{
    fn start(&mut self) -> Result<(), E> {
        (self.ensure_running)()
    }

    fn advance(&mut self, amount: usize) -> Result<(), E> {
        self.processed = self.processed.saturating_add(amount);
        while self.processed >= self.next_check {
            (self.ensure_running)()?;
            self.next_check = self.next_check.saturating_add(CANCELLATION_INTERVAL);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Candidate {
    range: Range<usize>,
    removals: Vec<(RepairKind, Range<usize>)>,
}

impl Candidate {
    fn logical_start(&self) -> usize {
        self.removals
            .iter()
            .map(|(_, range)| range.start)
            .min()
            .unwrap_or(self.range.start)
    }

    fn is_fenced(&self) -> bool {
        self.removals
            .iter()
            .any(|(kind, _)| *kind == RepairKind::RemovedMarkdownFence)
    }

    fn logical_end(&self) -> usize {
        self.removals
            .iter()
            .map(|(_, range)| range.end)
            .max()
            .unwrap_or(self.range.end)
    }
}

fn bytes_all_with_cancellation<F, E>(
    bytes: &[u8],
    cancellation: &mut Cancellation<F>,
    predicate: impl Fn(u8) -> bool,
) -> Result<bool, E>
where
    F: FnMut() -> Result<(), E>,
{
    for chunk in bytes.chunks(CANCELLATION_INTERVAL) {
        let matches = chunk.iter().copied().all(&predicate);
        cancellation.advance(chunk.len())?;
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bytes_any_with_cancellation<F, E>(
    bytes: &[u8],
    cancellation: &mut Cancellation<F>,
    predicate: impl Fn(u8) -> bool,
) -> Result<bool, E>
where
    F: FnMut() -> Result<(), E>,
{
    for chunk in bytes.chunks(CANCELLATION_INTERVAL) {
        let matches = chunk.iter().copied().any(&predicate);
        cancellation.advance(chunk.len())?;
        if matches {
            return Ok(true);
        }
    }
    Ok(false)
}

fn chars_all_whitespace_with_cancellation<F, E>(
    input: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<bool, E>
where
    F: FnMut() -> Result<(), E>,
{
    for character in input.chars() {
        cancellation.advance(character.len_utf8())?;
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn find_byte_with_cancellation<F, E>(
    input: &str,
    range: Range<usize>,
    needle: u8,
    cancellation: &mut Cancellation<F>,
) -> Result<Option<usize>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let bytes = input.as_bytes();
    let mut chunk_start = range.start;
    while chunk_start < range.end {
        let chunk_end = chunk_start
            .saturating_add(CANCELLATION_INTERVAL)
            .min(range.end);
        if let Some(offset) = bytes[chunk_start..chunk_end]
            .iter()
            .position(|byte| *byte == needle)
        {
            cancellation.advance(offset + 1)?;
            return Ok(Some(chunk_start + offset));
        }
        cancellation.advance(chunk_end - chunk_start)?;
        chunk_start = chunk_end;
    }
    Ok(None)
}

fn find_sequence_with_cancellation<F, E>(
    input: &str,
    range: Range<usize>,
    needle: &[u8],
    cancellation: &mut Cancellation<F>,
) -> Result<Option<usize>, E>
where
    F: FnMut() -> Result<(), E>,
{
    debug_assert!(!needle.is_empty());
    let bytes = input.as_bytes();
    let last_start = range.end.saturating_sub(needle.len());
    let mut position = range.start;
    let mut scanned = 0_usize;
    while position <= last_start && position + needle.len() <= range.end {
        if bytes.get(position..position + needle.len()) == Some(needle) {
            cancellation.advance(scanned + needle.len())?;
            return Ok(Some(position));
        }
        position += 1;
        scanned += 1;
        if scanned == CANCELLATION_INTERVAL {
            cancellation.advance(scanned)?;
            scanned = 0;
        }
    }
    cancellation.advance(scanned)?;
    Ok(None)
}

fn select_candidate<F, E>(
    input: &str,
    policy: RepairPolicy,
    cancellation: &mut Cancellation<F>,
) -> Result<Result<Candidate, RepairError>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let fenced = find_fenced_blocks(input, cancellation)?;
    let mut outside_structural = Vec::new();
    let mut outside_start = 0_usize;
    for candidate in &fenced {
        let fence_start = candidate.logical_start();
        if outside_start < fence_start {
            outside_structural.extend(find_structural_candidates(
                input,
                outside_start..fence_start,
                cancellation,
            )?);
        }
        outside_start = candidate.logical_end();
    }
    if outside_start < input.len() {
        outside_structural.extend(find_structural_candidates(
            input,
            outside_start..input.len(),
            cancellation,
        )?);
    }
    let mut fenced = fenced.into_iter().peekable();
    let mut outside_structural = outside_structural.into_iter().peekable();
    let mut candidates = Vec::new();
    while fenced.peek().is_some() || outside_structural.peek().is_some() {
        let take_fence = match (fenced.peek(), outside_structural.peek()) {
            (Some(fenced), Some(structural)) => fenced.logical_start() <= structural.start,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_fence {
            candidates.push(fenced.next().expect("peek confirmed fenced candidate"));
        } else {
            candidates.push(Candidate {
                range: outside_structural
                    .next()
                    .expect("peek confirmed structural candidate"),
                removals: Vec::new(),
            });
        }
    }
    if candidates.len() > 1 && policy == RepairPolicy::Conservative {
        return Ok(Err(RepairError::new(
            RepairErrorKind::MultipleJsonCandidates,
            candidates[1].logical_start(),
        )));
    }
    if let Some(mut candidate) = candidates.into_iter().next() {
        if candidate.is_fenced() {
            add_fenced_surrounding_removals(input, &mut candidate, cancellation)?;
            return Ok(Ok(candidate));
        }
        let range = candidate.range;
        let bom_len = input
            .starts_with('\u{feff}')
            .then_some('\u{feff}'.len_utf8());
        if let Some(bom_len) = bom_len
            && range.start >= bom_len
            && bytes_all_with_cancellation(
                &input.as_bytes()[bom_len..range.start],
                cancellation,
                is_json_whitespace,
            )?
            && bytes_all_with_cancellation(
                &input.as_bytes()[range.end..],
                cancellation,
                is_json_whitespace,
            )?
        {
            return Ok(Ok(Candidate {
                range: bom_len..input.len(),
                removals: vec![(RepairKind::RemovedByteOrderMark, 0..bom_len)],
            }));
        }
        let outside_is_only_whitespace =
            chars_all_whitespace_with_cancellation(&input[..range.start], cancellation)?
                && chars_all_whitespace_with_cancellation(&input[range.end..], cancellation)?;
        let mut candidate = if outside_is_only_whitespace {
            Candidate {
                range: 0..input.len(),
                removals: Vec::new(),
            }
        } else {
            Candidate {
                range,
                removals: Vec::new(),
            }
        };
        add_surrounding_removals(input, &mut candidate, cancellation)?;
        return Ok(Ok(candidate));
    }

    let trimmed = trim_json_like_whitespace_with_cancellation(input, cancellation)?;
    if input.starts_with('\u{feff}') {
        let bom_len = '\u{feff}'.len_utf8();
        let after_bom =
            trim_json_like_whitespace_with_cancellation(&input[bom_len..], cancellation)?;
        let start = bom_len + after_bom.start;
        if start < input.len() && looks_like_scalar_start(input[start..].chars().next()) {
            return Ok(Ok(Candidate {
                range: bom_len..input.len(),
                removals: vec![(RepairKind::RemovedByteOrderMark, 0..bom_len)],
            }));
        }
    }
    if trimmed.start < trimmed.end && looks_like_scalar_start(input[trimmed.start..].chars().next())
    {
        return Ok(Ok(Candidate {
            range: 0..input.len(),
            removals: Vec::new(),
        }));
    }

    Ok(Err(RepairError::new(
        RepairErrorKind::NoJsonCandidate,
        trimmed.start,
    )))
}

fn add_fenced_surrounding_removals<F, E>(
    input: &str,
    candidate: &mut Candidate,
    cancellation: &mut Cancellation<F>,
) -> Result<(), E>
where
    F: FnMut() -> Result<(), E>,
{
    let opening_start = candidate
        .removals
        .iter()
        .map(|(_, range)| range.start)
        .min()
        .unwrap_or(candidate.range.start);
    let closing_end = candidate
        .removals
        .iter()
        .map(|(_, range)| range.end)
        .max()
        .unwrap_or(candidate.range.end);
    if bytes_any_with_cancellation(&input.as_bytes()[..opening_start], cancellation, |byte| {
        !is_json_whitespace(byte)
    })? {
        candidate
            .removals
            .push((RepairKind::RemovedSurroundingText, 0..opening_start));
    }
    if bytes_any_with_cancellation(&input.as_bytes()[closing_end..], cancellation, |byte| {
        !is_json_whitespace(byte)
    })? {
        candidate
            .removals
            .push((RepairKind::RemovedSurroundingText, closing_end..input.len()));
    }
    candidate
        .removals
        .sort_by_key(|(_, range)| (range.start, range.end));
    Ok(())
}

fn add_surrounding_removals<F, E>(
    input: &str,
    candidate: &mut Candidate,
    cancellation: &mut Cancellation<F>,
) -> Result<(), E>
where
    F: FnMut() -> Result<(), E>,
{
    if bytes_any_with_cancellation(
        &input.as_bytes()[..candidate.range.start],
        cancellation,
        |byte| !is_json_whitespace(byte),
    )? {
        candidate
            .removals
            .push((RepairKind::RemovedSurroundingText, 0..candidate.range.start));
    }
    if bytes_any_with_cancellation(
        &input.as_bytes()[candidate.range.end..],
        cancellation,
        |byte| !is_json_whitespace(byte),
    )? {
        candidate.removals.push((
            RepairKind::RemovedSurroundingText,
            candidate.range.end..input.len(),
        ));
    }
    Ok(())
}

fn find_fenced_blocks<F, E>(
    input: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<Vec<Candidate>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut result = Vec::new();
    let mut line_start = 0;
    let mut open: Option<(u8, usize, Range<usize>, usize)> = None;

    while line_start < input.len() {
        let line_end =
            find_byte_with_cancellation(input, line_start..input.len(), b'\n', cancellation)?
                .map_or(input.len(), |newline| newline + 1);
        let mut content_end = line_end;
        while content_end > line_start && matches!(input.as_bytes()[content_end - 1], b'\r' | b'\n')
        {
            content_end -= 1;
        }
        let mut trimmed_start = line_start;
        while trimmed_start < content_end {
            let character = input[trimmed_start..content_end]
                .chars()
                .next()
                .expect("line position must be valid UTF-8");
            if !character.is_whitespace() {
                break;
            }
            cancellation.advance(character.len_utf8())?;
            trimmed_start += character.len_utf8();
        }
        let leading = trimmed_start - line_start;
        let marker = input.as_bytes().get(trimmed_start).copied();
        let mut marker_len = 0_usize;
        if let Some(marker) = marker {
            while input.as_bytes().get(trimmed_start + marker_len) == Some(&marker) {
                cancellation.advance(1)?;
                marker_len += 1;
            }
        }

        if let Some((open_marker, open_len, opening_range, content_start)) = &open {
            let closes_fence = marker == Some(*open_marker)
                && marker_len >= *open_len
                && chars_all_whitespace_with_cancellation(
                    &input[trimmed_start + marker_len..content_end],
                    cancellation,
                )?;
            if closes_fence {
                let closing_range = line_start + leading..line_end;
                result.push(Candidate {
                    range: *content_start..line_start,
                    removals: vec![
                        (RepairKind::RemovedMarkdownFence, opening_range.clone()),
                        (RepairKind::RemovedMarkdownFence, closing_range),
                    ],
                });
                open = None;
            }
        } else if matches!(marker, Some(b'`' | b'~')) && marker_len >= 3 {
            open = Some((
                marker.expect("matched marker"),
                marker_len,
                line_start + leading..line_end,
                line_end,
            ));
        }
        line_start = line_end;
    }

    Ok(result)
}

fn find_structural_candidates<F, E>(
    input: &str,
    range: Range<usize>,
    cancellation: &mut Cancellation<F>,
) -> Result<Vec<Range<usize>>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut result = Vec::new();
    let mut position = range.start;
    while position < range.end {
        let Some(character) = input[position..range.end].chars().next() else {
            break;
        };
        let width = character.len_utf8();
        if matches!(character, '{' | '[') {
            let end = scan_structural_candidate(input, position, range.end, cancellation)?;
            result.push(position..end);
            position = end.max(position + width);
        } else {
            cancellation.advance(width)?;
            position += width;
        }
    }
    Ok(result)
}

fn scan_structural_candidate<F, E>(
    input: &str,
    start: usize,
    end: usize,
    cancellation: &mut Cancellation<F>,
) -> Result<usize, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut stack = Vec::new();
    let mut position = start;
    let mut quote: Option<Quote> = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;

    while position < end {
        let character = input[position..].chars().next().expect("position in input");
        let width = character.len_utf8();
        let next = input[position + width..end].chars().next();
        cancellation.advance(width)?;

        if line_comment {
            line_comment = character != '\n';
            position += width;
            continue;
        }
        if block_comment {
            if character == '*' && next == Some('/') {
                cancellation.advance(1)?;
                position += 2;
                block_comment = false;
            } else {
                position += width;
            }
            continue;
        }
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if active_quote.closes(character) {
                quote = None;
            }
            position += width;
            continue;
        }
        if character == '/' && next == Some('/') {
            cancellation.advance(1)?;
            position += 2;
            line_comment = true;
            continue;
        }
        if character == '/' && next == Some('*') {
            cancellation.advance(1)?;
            position += 2;
            block_comment = true;
            continue;
        }
        if let Some(found_quote) = Quote::from_opener(character) {
            quote = Some(found_quote);
            position += width;
            continue;
        }
        match character {
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.last().copied() == Some(character) {
                    stack.pop();
                    position += width;
                    if stack.is_empty() {
                        return Ok(position);
                    }
                    continue;
                }
                return Ok(position + width);
            }
            _ => {}
        }
        position += width;
    }
    Ok(end)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Double,
    Single,
    SmartDouble,
    SmartSingle,
}

impl Quote {
    fn from_opener(character: char) -> Option<Self> {
        match character {
            '"' => Some(Self::Double),
            '\'' => Some(Self::Single),
            '“' | '”' => Some(Self::SmartDouble),
            '‘' | '’' => Some(Self::SmartSingle),
            _ => None,
        }
    }

    fn closes(self, character: char) -> bool {
        match self {
            Self::Double => character == '"',
            Self::Single => character == '\'',
            Self::SmartDouble => matches!(character, '“' | '”'),
            Self::SmartSingle => matches!(character, '‘' | '’'),
        }
    }

    fn is_standard(self) -> bool {
        self == Self::Double
    }

    const fn index(self) -> usize {
        match self {
            Self::Double => 0,
            Self::Single => 1,
            Self::SmartDouble => 2,
            Self::SmartSingle => 3,
        }
    }
}

#[derive(Debug)]
struct QuoteIndex {
    positions: [Vec<usize>; 4],
    cursors: [usize; 4],
}

impl QuoteIndex {
    fn new<F, E>(
        input: &str,
        range: Range<usize>,
        cancellation: &mut Cancellation<F>,
    ) -> Result<Self, E>
    where
        F: FnMut() -> Result<(), E>,
    {
        let mut positions = std::array::from_fn(|_| Vec::new());
        let mut position = range.start;
        let mut escaped = false;
        while position < range.end {
            let character = input[position..]
                .chars()
                .next()
                .expect("position must be within candidate");
            let width = character.len_utf8();
            cancellation.advance(width)?;
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if let Some(quote) = Quote::from_opener(character) {
                positions[quote.index()].push(position);
            }
            position += width;
        }
        Ok(Self {
            positions,
            cursors: [0; 4],
        })
    }

    fn at_or_after(&mut self, quote: Quote, minimum: usize) -> Option<usize> {
        let index = quote.index();
        let positions = &self.positions[index];
        let cursor = &mut self.cursors[index];
        while positions
            .get(*cursor)
            .is_some_and(|position| *position < minimum)
        {
            *cursor += 1;
        }
        positions.get(*cursor).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameState {
    ObjectKeyOrEnd,
    ObjectColon,
    ObjectValue,
    ObjectCommaOrEnd,
    ArrayValueOrEnd,
    ArrayCommaOrEnd,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    kind: FrameKind,
    state: FrameState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    Value,
    End,
}

struct Parser<'input, 'cancel, F> {
    input: &'input str,
    end: usize,
    position: usize,
    policy: RepairPolicy,
    root_state: RootState,
    frames: Vec<Frame>,
    quotes: QuoteIndex,
    builder: Builder<'input>,
    suffix_removals: Vec<(RepairKind, Range<usize>)>,
    cancellation: &'cancel mut Cancellation<F>,
}

impl<'input, 'cancel, F, E> Parser<'input, 'cancel, F>
where
    F: FnMut() -> Result<(), E>,
{
    fn new(
        input: &'input str,
        candidate: Candidate,
        policy: RepairPolicy,
        cancellation: &'cancel mut Cancellation<F>,
    ) -> Result<Self, E> {
        let quotes = QuoteIndex::new(input, candidate.range.clone(), cancellation)?;
        let mut builder = Builder::new(input, candidate.range.start);
        let mut suffix_removals = Vec::new();
        for (kind, range) in candidate.removals {
            if range.start >= candidate.range.end {
                suffix_removals.push((kind, range));
            } else {
                builder.record_deletion(kind, range);
            }
        }
        Ok(Self {
            input,
            end: candidate.range.end,
            position: candidate.range.start,
            policy,
            root_state: RootState::Value,
            frames: Vec::new(),
            quotes,
            builder,
            suffix_removals,
            cancellation,
        })
    }

    fn parse(mut self) -> Result<Result<RepairOutput<'input>, RepairError>, E> {
        loop {
            self.skip_trivia()?;
            if self.position >= self.end {
                if let Some(error) = self.finish_at_end() {
                    return Ok(Err(error));
                }
                let output_offset = self.builder.output.len();
                for (kind, range) in self.suffix_removals.drain(..) {
                    self.builder
                        .record_suffix_deletion_at(kind, range, output_offset);
                }
                return Ok(Ok(self.builder.finish()));
            }

            if self.frames.is_empty() {
                match self.root_state {
                    RootState::Value => {
                        if let Some(error) = self.consume_value()? {
                            return Ok(Err(error));
                        }
                    }
                    RootState::End => {
                        return Ok(Err(RepairError::new(
                            RepairErrorKind::MultipleRootValues,
                            self.position,
                        )));
                    }
                }
                continue;
            }

            let state = self.frames.last().expect("checked non-empty").state;
            let error = match state {
                FrameState::ObjectKeyOrEnd => self.object_key_or_end()?,
                FrameState::ObjectColon => self.object_colon()?,
                FrameState::ObjectValue => self.object_value()?,
                FrameState::ObjectCommaOrEnd => self.object_comma_or_end()?,
                FrameState::ArrayValueOrEnd => self.array_value_or_end()?,
                FrameState::ArrayCommaOrEnd => self.array_comma_or_end()?,
            };
            if let Some(error) = error {
                return Ok(Err(error));
            }
        }
    }

    fn skip_trivia(&mut self) -> Result<(), E> {
        loop {
            if self.position >= self.end {
                return Ok(());
            }
            let character = self.current_char();
            if character.is_ascii_whitespace() {
                let start = self.position;
                while self.position < self.end && self.current_char().is_ascii_whitespace() {
                    self.advance_current()?;
                }
                self.builder.copy(start..self.position);
                continue;
            }
            if character == '\u{feff}' {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedByteOrderMark, start..self.position);
                continue;
            }
            if character.is_whitespace() {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .replace(RepairKind::NormalizedWhitespace, start..self.position, " ");
                continue;
            }
            if self.starts_with("//") {
                let start = self.position;
                self.advance_bytes(2)?;
                while self.position < self.end && self.current_char() != '\n' {
                    self.advance_current()?;
                }
                self.builder
                    .delete(RepairKind::RemovedComment, start..self.position);
                continue;
            }
            if self.starts_with("/*") {
                let start = self.position;
                self.advance_bytes(2)?;
                if let Some(closing) = find_sequence_with_cancellation(
                    self.input,
                    self.position..self.end,
                    b"*/",
                    self.cancellation,
                )? {
                    self.position = closing + 2;
                } else {
                    self.position = self.end;
                }
                self.builder
                    .delete(RepairKind::RemovedComment, start..self.position);
                continue;
            }
            return Ok(());
        }
    }

    fn object_key_or_end(&mut self) -> Result<Option<RepairError>, E> {
        match self.current_char() {
            '}' => {
                self.copy_current()?;
                self.frames.pop();
                Ok(None)
            }
            ']' => self.close_missing_container(FrameKind::Object),
            ',' => {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedComma, start..self.position);
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedColon, start..self.position);
                Ok(None)
            }
            character if Quote::from_opener(character).is_some() => {
                if let Some(error) = self.consume_string(StringRole::Key)? {
                    return Ok(Some(error));
                }
                self.set_last_state(FrameState::ObjectColon);
                Ok(None)
            }
            _ => {
                if let Some(error) = self.consume_bare(true)? {
                    return Ok(Some(error));
                }
                self.set_last_state(FrameState::ObjectColon);
                Ok(None)
            }
        }
    }

    fn object_colon(&mut self) -> Result<Option<RepairError>, E> {
        if self.current_char() == ':' {
            self.copy_current()?;
            self.set_last_state(FrameState::ObjectValue);
            return Ok(None);
        }
        if matches!(self.current_char(), ',' | '}' | ']') {
            return Ok(Some(RepairError::new(
                RepairErrorKind::MissingValue,
                self.position,
            )));
        }
        self.builder
            .insert(RepairKind::InsertedColon, self.position, ":");
        self.set_last_state(FrameState::ObjectValue);
        Ok(None)
    }

    fn object_value(&mut self) -> Result<Option<RepairError>, E> {
        if self.current_char() == ':' {
            let start = self.position;
            self.advance_current()?;
            self.builder
                .delete(RepairKind::RemovedColon, start..self.position);
            return Ok(None);
        }
        if matches!(self.current_char(), ',' | '}' | ']') {
            return Ok(Some(RepairError::new(
                RepairErrorKind::MissingValue,
                self.position,
            )));
        }
        self.consume_value()
    }

    fn object_comma_or_end(&mut self) -> Result<Option<RepairError>, E> {
        match self.current_char() {
            '}' => {
                self.copy_current()?;
                self.frames.pop();
                Ok(None)
            }
            ']' => self.close_missing_container(FrameKind::Object),
            ',' => {
                let comma = self.position;
                let next = self.peek_significant_after(comma + 1)?;
                if next.is_none_or(|(_, character)| matches!(character, ',' | '}' | ']')) {
                    self.advance_current()?;
                    self.builder
                        .delete(RepairKind::RemovedComma, comma..self.position);
                } else {
                    self.copy_current()?;
                    self.set_last_state(FrameState::ObjectKeyOrEnd);
                }
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedColon, start..self.position);
                Ok(None)
            }
            _ => {
                self.builder
                    .insert(RepairKind::InsertedComma, self.position, ",");
                self.set_last_state(FrameState::ObjectKeyOrEnd);
                Ok(None)
            }
        }
    }

    fn array_value_or_end(&mut self) -> Result<Option<RepairError>, E> {
        match self.current_char() {
            ']' => {
                self.copy_current()?;
                self.frames.pop();
                Ok(None)
            }
            '}' => self.close_missing_container(FrameKind::Array),
            ',' => {
                if self.policy == RepairPolicy::Conservative {
                    return Ok(Some(RepairError::new(
                        RepairErrorKind::MissingValue,
                        self.position,
                    )));
                }
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedComma, start..self.position);
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedColon, start..self.position);
                Ok(None)
            }
            _ => self.consume_value(),
        }
    }

    fn array_comma_or_end(&mut self) -> Result<Option<RepairError>, E> {
        match self.current_char() {
            ']' => {
                self.copy_current()?;
                self.frames.pop();
                Ok(None)
            }
            '}' => self.close_missing_container(FrameKind::Array),
            ',' => {
                let comma = self.position;
                let next = self.peek_significant_after(comma + 1)?;
                if next.is_some_and(|(_, character)| character == ',')
                    && self.policy == RepairPolicy::Conservative
                {
                    return Ok(Some(RepairError::new(
                        RepairErrorKind::MissingValue,
                        next.expect("checked Some").0,
                    )));
                }
                if next.is_none_or(|(_, character)| matches!(character, ',' | '}' | ']')) {
                    self.advance_current()?;
                    self.builder
                        .delete(RepairKind::RemovedComma, comma..self.position);
                } else {
                    self.copy_current()?;
                    self.set_last_state(FrameState::ArrayValueOrEnd);
                }
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder
                    .delete(RepairKind::RemovedColon, start..self.position);
                Ok(None)
            }
            _ => {
                self.builder
                    .insert(RepairKind::InsertedComma, self.position, ",");
                self.set_last_state(FrameState::ArrayValueOrEnd);
                Ok(None)
            }
        }
    }

    fn consume_value(&mut self) -> Result<Option<RepairError>, E> {
        let character = self.current_char();
        match character {
            '{' => {
                self.mark_value_complete();
                self.copy_current()?;
                self.frames.push(Frame {
                    kind: FrameKind::Object,
                    state: FrameState::ObjectKeyOrEnd,
                });
                Ok(None)
            }
            '[' => {
                self.mark_value_complete();
                self.copy_current()?;
                self.frames.push(Frame {
                    kind: FrameKind::Array,
                    state: FrameState::ArrayValueOrEnd,
                });
                Ok(None)
            }
            '}' | ']' => Ok(Some(RepairError::new(
                RepairErrorKind::UnexpectedClosingDelimiter,
                self.position,
            ))),
            character if Quote::from_opener(character).is_some() => {
                if let Some(error) = self.consume_string(StringRole::Value)? {
                    return Ok(Some(error));
                }
                self.mark_value_complete();
                Ok(None)
            }
            _ => {
                if let Some(error) = self.consume_bare(false)? {
                    return Ok(Some(error));
                }
                self.mark_value_complete();
                Ok(None)
            }
        }
    }

    fn consume_string(&mut self, role: StringRole) -> Result<Option<RepairError>, E> {
        let start = self.position;
        let opener = self.current_char();
        let quote = Quote::from_opener(opener).expect("caller checked quote");
        let opener_end = start + opener.len_utf8();
        self.advance_current()?;
        if quote.is_standard() {
            self.builder.copy(start..opener_end);
        } else {
            self.builder
                .replace(RepairKind::NormalizedQuote, start..opener_end, "\"");
        }

        while self.position < self.end {
            let character_start = self.position;
            let character = self.current_char();
            let width = character.len_utf8();

            if self.policy == RepairPolicy::BestEffort
                && matches!(character, '}' | ']')
                && self.find_later_quote(quote, self.position).is_none()
            {
                self.builder
                    .insert(RepairKind::InsertedClosingQuote, self.position, "\"");
                return Ok(None);
            }

            if quote.closes(character) {
                let after = self.position + width;
                let (next_position, next, had_whitespace) = self.peek_after_string_quote(after)?;
                // `"a" "b"` 既可能是漏逗号的两个字符串，也可能是正文包含两个未转义
                // 引号的一个字符串。空白不能替调用方消除这种歧义。
                if self.policy == RepairPolicy::Conservative
                    && role == StringRole::Value
                    && had_whitespace
                    && next.is_some_and(|next| Quote::from_opener(next).is_some())
                {
                    return Ok(Some(RepairError::new(
                        RepairErrorKind::AmbiguousStringQuote,
                        character_start,
                    )));
                }
                if self.quote_can_close(role, next, had_whitespace) {
                    self.advance_current()?;
                    if quote.is_standard() {
                        self.builder.copy(character_start..self.position);
                    } else {
                        self.builder.replace(
                            RepairKind::NormalizedQuote,
                            character_start..self.position,
                            "\"",
                        );
                    }
                    return Ok(None);
                }

                let later_quote = self.find_later_quote(quote, next_position);
                if role == StringRole::Key && later_quote.is_none() {
                    self.advance_current()?;
                    if quote.is_standard() {
                        self.builder.copy(character_start..self.position);
                    } else {
                        self.builder.replace(
                            RepairKind::NormalizedQuote,
                            character_start..self.position,
                            "\"",
                        );
                    }
                    return Ok(None);
                }
                // 当前引号既可能是正文，也可能是边界并伴随其他语法缺失；后续引号不能
                // 反向证明唯一意图，Conservative 不替调用方选择其中一种解释。
                if self.policy == RepairPolicy::Conservative {
                    return Ok(Some(RepairError::new(
                        RepairErrorKind::AmbiguousStringQuote,
                        character_start,
                    )));
                }
                self.advance_current()?;
                if quote == Quote::Double || quote == Quote::SmartDouble {
                    self.builder.replace(
                        RepairKind::EscapedInternalQuote,
                        character_start..self.position,
                        "\\\"",
                    );
                } else {
                    self.builder.copy(character_start..self.position);
                }
                continue;
            }

            if character == '\\' {
                if let Some(error) = self.consume_escape(quote)? {
                    return Ok(Some(error));
                }
                continue;
            }
            if character <= '\u{001f}' {
                self.advance_current()?;
                let replacement = match character {
                    '\n' => "\\n".to_owned(),
                    '\r' => "\\r".to_owned(),
                    '\t' => "\\t".to_owned(),
                    '\u{0008}' => "\\b".to_owned(),
                    '\u{000c}' => "\\f".to_owned(),
                    _ => format!("\\u{:04x}", u32::from(character)),
                };
                self.builder.replace(
                    RepairKind::EscapedControlCharacter,
                    character_start..self.position,
                    &replacement,
                );
                continue;
            }
            if character == '"' && quote != Quote::Double {
                self.advance_current()?;
                self.builder.replace(
                    RepairKind::EscapedInternalQuote,
                    character_start..self.position,
                    "\\\"",
                );
                continue;
            }
            self.advance_current()?;
            self.builder.copy(character_start..self.position);
        }

        if self.policy == RepairPolicy::BestEffort {
            self.builder
                .insert(RepairKind::InsertedClosingQuote, self.position, "\"");
            Ok(None)
        } else {
            Ok(Some(RepairError::new(
                RepairErrorKind::UnterminatedString,
                start,
            )))
        }
    }

    fn consume_escape(&mut self, quote: Quote) -> Result<Option<RepairError>, E> {
        let start = self.position;
        self.advance_current()?;
        if self.position >= self.end {
            self.builder.replace(
                RepairKind::EscapedInvalidEscape,
                start..self.position,
                "\\\\",
            );
            return Ok(None);
        }
        let escaped = self.current_char();
        if matches!(escaped, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
            self.advance_current()?;
            self.builder.copy(start..self.position);
            return Ok(None);
        }
        if escaped == 'u' {
            let digits_start = self.position + 1;
            let digits_end = digits_start.saturating_add(4);
            let valid = digits_end <= self.end
                && self.input.as_bytes()[digits_start..digits_end]
                    .iter()
                    .all(u8::is_ascii_hexdigit);
            if valid {
                self.advance_bytes(5)?;
                self.builder.copy(start..self.position);
            } else {
                self.builder.replace(
                    RepairKind::EscapedInvalidEscape,
                    start..self.position,
                    "\\\\",
                );
            }
            return Ok(None);
        }
        self.advance_current()?;
        if escaped == '\'' && quote == Quote::Single {
            self.builder
                .replace(RepairKind::EscapedInvalidEscape, start..self.position, "'");
        } else {
            let replacement = format!("\\\\{escaped}");
            self.builder.replace(
                RepairKind::EscapedInvalidEscape,
                start..self.position,
                &replacement,
            );
        }
        Ok(None)
    }

    fn consume_bare(&mut self, key: bool) -> Result<Option<RepairError>, E> {
        let start = self.position;
        while self.position < self.end {
            let character = self.current_char();
            if character.is_whitespace()
                || matches!(
                    character,
                    ',' | ':' | '{' | '}' | '[' | ']' | '"' | '\'' | '“' | '”' | '‘' | '’'
                )
                || self.starts_with("//")
                || self.starts_with("/*")
            {
                break;
            }
            self.advance_current()?;
        }
        if self.position == start {
            return Ok(Some(RepairError::new(
                RepairErrorKind::InvalidToken,
                self.position,
            )));
        }
        let token = &self.input[start..self.position];
        if key {
            let replacement = quote_bare_with_cancellation(token, self.cancellation)?;
            self.builder.replace(
                RepairKind::QuotedBareKey,
                start..self.position,
                &replacement,
            );
            return Ok(None);
        }

        if matches!(token, "true" | "false" | "null") {
            self.builder.copy(start..self.position);
            return Ok(None);
        }
        if matches!(token, "True" | "False" | "None") && self.policy == RepairPolicy::BestEffort {
            let replacement = match token {
                "True" => "true",
                "False" => "false",
                "None" => "null",
                _ => unreachable!(),
            };
            self.builder.replace(
                RepairKind::NormalizedLiteral,
                start..self.position,
                replacement,
            );
            return Ok(None);
        }
        if looks_number_like(token) {
            if is_strict_json_number_with_cancellation(token, self.cancellation)? {
                self.builder.copy(start..self.position);
                return Ok(None);
            }
            if self.policy == RepairPolicy::BestEffort
                && let Some(number) = normalize_number_with_cancellation(token, self.cancellation)?
            {
                self.builder
                    .replace(RepairKind::NormalizedNumber, start..self.position, &number);
                return Ok(None);
            }
            return Ok(Some(RepairError::new(
                RepairErrorKind::InvalidNumber,
                start,
            )));
        }

        let replacement = quote_bare_with_cancellation(token, self.cancellation)?;
        self.builder.replace(
            RepairKind::QuotedBareValue,
            start..self.position,
            &replacement,
        );
        Ok(None)
    }

    fn quote_can_close(&self, role: StringRole, next: Option<char>, had_whitespace: bool) -> bool {
        match role {
            StringRole::Key => matches!(next, Some(':')),
            StringRole::Value => match next {
                None | Some(',' | '}' | ']') => true,
                Some('"' | '\'' | '“' | '”' | '‘' | '’' | '{' | '[') => had_whitespace,
                _ => false,
            },
        }
    }

    fn peek_after_string_quote(
        &mut self,
        position: usize,
    ) -> Result<(usize, Option<char>, bool), E> {
        if let Some((next_position, character)) = self.peek_significant_after(position)? {
            return Ok((next_position, Some(character), next_position > position));
        }
        Ok((self.end, None, self.end > position))
    }

    fn find_later_quote(&mut self, quote: Quote, position: usize) -> Option<usize> {
        self.quotes.at_or_after(quote, position)
    }

    fn close_missing_container(&mut self, expected: FrameKind) -> Result<Option<RepairError>, E> {
        let frame = self.frames.last().expect("container exists");
        if frame.kind != expected || !state_allows_close(frame.state) {
            return Ok(Some(RepairError::new(
                RepairErrorKind::UnexpectedClosingDelimiter,
                self.position,
            )));
        }
        let closing = match expected {
            FrameKind::Object => "}",
            FrameKind::Array => "]",
        };
        self.builder
            .insert(RepairKind::InsertedClosingDelimiter, self.position, closing);
        self.frames.pop();
        Ok(None)
    }

    fn finish_at_end(&mut self) -> Option<RepairError> {
        while let Some(frame) = self.frames.last().copied() {
            if !state_allows_close(frame.state) {
                return Some(RepairError::new(
                    RepairErrorKind::MissingValue,
                    self.position,
                ));
            }
            let closing = match frame.kind {
                FrameKind::Object => "}",
                FrameKind::Array => "]",
            };
            self.builder
                .insert(RepairKind::InsertedClosingDelimiter, self.position, closing);
            self.frames.pop();
        }
        if self.root_state == RootState::Value {
            return Some(RepairError::new(
                RepairErrorKind::MissingValue,
                self.position,
            ));
        }
        None
    }

    fn mark_value_complete(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.state = match frame.state {
                FrameState::ObjectValue => FrameState::ObjectCommaOrEnd,
                FrameState::ArrayValueOrEnd => FrameState::ArrayCommaOrEnd,
                state => state,
            };
        } else {
            self.root_state = RootState::End;
        }
    }

    fn set_last_state(&mut self, state: FrameState) {
        self.frames.last_mut().expect("container exists").state = state;
    }

    fn peek_significant_after(&mut self, mut position: usize) -> Result<Option<(usize, char)>, E> {
        while position < self.end {
            let character = self.input[position..]
                .chars()
                .next()
                .expect("valid position");
            let width = character.len_utf8();
            self.cancellation.advance(width)?;
            if character.is_whitespace() {
                position += width;
                continue;
            }
            if self.input[position..self.end].starts_with("//") {
                if let Some(newline) = find_byte_with_cancellation(
                    self.input,
                    position..self.end,
                    b'\n',
                    self.cancellation,
                )? {
                    position = newline + 1;
                } else {
                    return Ok(None);
                }
                continue;
            }
            if self.input[position..self.end].starts_with("/*") {
                if let Some(closing) = find_sequence_with_cancellation(
                    self.input,
                    position..self.end,
                    b"*/",
                    self.cancellation,
                )? {
                    position = closing + 2;
                } else {
                    return Ok(None);
                }
                continue;
            }
            return Ok(Some((position, character)));
        }
        Ok(None)
    }

    fn current_char(&self) -> char {
        self.input[self.position..]
            .chars()
            .next()
            .expect("position must be within candidate")
    }

    fn starts_with(&self, pattern: &str) -> bool {
        self.input[self.position..self.end].starts_with(pattern)
    }

    fn copy_current(&mut self) -> Result<(), E> {
        let start = self.position;
        self.advance_current()?;
        self.builder.copy(start..self.position);
        Ok(())
    }

    fn advance_current(&mut self) -> Result<(), E> {
        let width = self.current_char().len_utf8();
        self.advance_bytes(width)
    }

    fn advance_bytes(&mut self, amount: usize) -> Result<(), E> {
        self.cancellation.advance(amount)?;
        self.position += amount;
        Ok(())
    }
}

fn state_allows_close(state: FrameState) -> bool {
    matches!(
        state,
        FrameState::ObjectKeyOrEnd
            | FrameState::ObjectCommaOrEnd
            | FrameState::ArrayValueOrEnd
            | FrameState::ArrayCommaOrEnd
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringRole {
    Key,
    Value,
}

struct Builder<'a> {
    input: &'a str,
    output: String,
    repairs: Vec<Repair>,
    source_map: SourceMap,
}

impl<'a> Builder<'a> {
    fn new(input: &'a str, original_start: usize) -> Self {
        Self {
            input,
            output: String::new(),
            repairs: Vec::new(),
            source_map: SourceMap::new(original_start),
        }
    }

    fn copy(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.source_map.push_copy(
            self.output.len(),
            self.output.len() + range.len(),
            range.clone(),
        );
        self.output.push_str(&self.input[range]);
    }

    fn replace(&mut self, kind: RepairKind, original: Range<usize>, replacement: &str) {
        let output_start = self.output.len();
        if replacement.is_empty() {
            self.source_map.push_boundary(output_start, original.end);
        } else {
            self.source_map.push_replacement(
                output_start,
                output_start + replacement.len(),
                original.clone(),
            );
        }
        self.output.push_str(replacement);
        self.repairs.push(Repair {
            kind,
            original_range: original,
            output_range: output_start..self.output.len(),
        });
    }

    fn insert(&mut self, kind: RepairKind, original_offset: usize, text: &str) {
        self.replace(kind, original_offset..original_offset, text);
    }

    fn delete(&mut self, kind: RepairKind, original: Range<usize>) {
        self.replace(kind, original, "");
    }

    fn record_deletion(&mut self, kind: RepairKind, original: Range<usize>) {
        self.record_deletion_at(kind, original, 0);
    }

    fn record_deletion_at(
        &mut self,
        kind: RepairKind,
        original: Range<usize>,
        output_offset: usize,
    ) {
        self.repairs.push(Repair {
            kind,
            original_range: original,
            output_range: output_offset..output_offset,
        });
    }

    fn record_suffix_deletion_at(
        &mut self,
        kind: RepairKind,
        original: Range<usize>,
        output_offset: usize,
    ) {
        self.source_map.push_boundary(output_offset, original.end);
        self.record_deletion_at(kind, original, output_offset);
    }

    fn finish(self) -> RepairOutput<'a> {
        let no_changes = self.repairs.is_empty();
        RepairOutput {
            json: if no_changes {
                Cow::Borrowed(self.input)
            } else {
                Cow::Owned(self.output)
            },
            repairs: self.repairs,
            source_map: self.source_map,
        }
    }
}

#[derive(Debug, Clone)]
struct SourceMap {
    initial_original: usize,
    output_len: usize,
    segments: Vec<MapSegment>,
}

impl SourceMap {
    fn new(initial_original: usize) -> Self {
        Self {
            initial_original,
            output_len: 0,
            segments: Vec::new(),
        }
    }

    fn push_copy(&mut self, output_start: usize, output_end: usize, original: Range<usize>) {
        if let Some(previous) = self.segments.last_mut()
            && previous.kind == MapSegmentKind::Copy
            && previous.output_end == output_start
            && previous.original_end == original.start
        {
            previous.output_end = output_end;
            previous.original_end = original.end;
            self.output_len = output_end;
            return;
        }
        self.segments.push(MapSegment {
            output_start,
            output_end,
            original_start: original.start,
            original_end: original.end,
            kind: MapSegmentKind::Copy,
        });
        self.output_len = output_end;
    }

    fn push_replacement(&mut self, output_start: usize, output_end: usize, original: Range<usize>) {
        self.segments.push(MapSegment {
            output_start,
            output_end,
            original_start: original.start,
            original_end: original.end,
            kind: MapSegmentKind::Replacement,
        });
        self.output_len = output_end;
    }

    fn push_boundary(&mut self, output_offset: usize, original_offset: usize) {
        self.segments.push(MapSegment {
            output_start: output_offset,
            output_end: output_offset,
            original_start: original_offset,
            original_end: original_offset,
            kind: MapSegmentKind::Boundary,
        });
        self.output_len = output_offset;
    }

    fn original_offset(&self, output_offset: usize) -> Option<usize> {
        if output_offset > self.output_len {
            return None;
        }
        let after = self
            .segments
            .partition_point(|segment| segment.output_start <= output_offset);
        let Some(segment) = after
            .checked_sub(1)
            .and_then(|index| self.segments.get(index))
        else {
            return Some(self.initial_original);
        };
        Some(match segment.kind {
            MapSegmentKind::Copy => {
                if output_offset >= segment.output_end {
                    segment.original_end
                } else {
                    segment.original_start + (output_offset - segment.output_start)
                }
            }
            MapSegmentKind::Replacement => {
                if output_offset >= segment.output_end {
                    segment.original_end
                } else {
                    segment.original_start
                }
            }
            MapSegmentKind::Boundary => segment.original_end,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapSegmentKind {
    Copy,
    Replacement,
    Boundary,
}

#[derive(Debug, Clone)]
struct MapSegment {
    output_start: usize,
    output_end: usize,
    original_start: usize,
    original_end: usize,
    kind: MapSegmentKind,
}

fn trim_json_like_whitespace_with_cancellation<F, E>(
    input: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<Range<usize>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut start = 0_usize;
    while start < input.len() {
        let character = input[start..]
            .chars()
            .next()
            .expect("trim start must be within UTF-8 input");
        if !character.is_whitespace() {
            break;
        }
        cancellation.advance(character.len_utf8())?;
        start += character.len_utf8();
    }

    let mut end = input.len();
    while end > start {
        let character = input[..end]
            .chars()
            .next_back()
            .expect("trim end must be within UTF-8 input");
        if !character.is_whitespace() {
            break;
        }
        cancellation.advance(character.len_utf8())?;
        end -= character.len_utf8();
    }
    Ok(start..end)
}

fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn looks_like_scalar_start(character: Option<char>) -> bool {
    matches!(
        character,
        Some('"' | '\'' | '“' | '”' | '‘' | '’' | '-' | '+' | '.' | '0'..='9')
    ) || character.is_some_and(|character| character.is_alphabetic())
}

fn quote_bare_with_cancellation<F, E>(
    token: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<String, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');
    for character in token.chars() {
        cancellation.advance(character.len_utf8())?;
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{0008}' => quoted.push_str("\\b"),
            '\u{000c}' => quoted.push_str("\\f"),
            character if character <= '\u{001f}' => {
                use fmt::Write as _;
                write!(&mut quoted, "\\u{:04x}", u32::from(character))
                    .expect("writing to String cannot fail");
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

fn looks_number_like(token: &str) -> bool {
    token
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b'+' | b'-' | b'.' | b'0'..=b'9'))
}

fn is_strict_json_number_with_cancellation<F, E>(
    token: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<bool, E>
where
    F: FnMut() -> Result<(), E>,
{
    let bytes = token.as_bytes();
    let mut position = 0;
    if bytes.get(position) == Some(&b'-') {
        position += 1;
        cancellation.advance(1)?;
    }
    match bytes.get(position) {
        Some(b'0') => {
            position += 1;
            cancellation.advance(1)?;
        }
        Some(b'1'..=b'9') => {
            position += 1;
            cancellation.advance(1)?;
            while bytes.get(position).is_some_and(u8::is_ascii_digit) {
                position += 1;
                cancellation.advance(1)?;
            }
        }
        _ => return Ok(false),
    }
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        cancellation.advance(1)?;
        let fraction_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
            cancellation.advance(1)?;
        }
        if position == fraction_start {
            return Ok(false);
        }
    }
    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        cancellation.advance(1)?;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
            cancellation.advance(1)?;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
            cancellation.advance(1)?;
        }
        if position == exponent_start {
            return Ok(false);
        }
    }
    Ok(position == bytes.len())
}

fn normalize_number_with_cancellation<F, E>(
    token: &str,
    cancellation: &mut Cancellation<F>,
) -> Result<Option<String>, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut value = token;
    let mut sign = "";
    if let Some(rest) = value.strip_prefix('+') {
        value = rest;
    } else if let Some(rest) = value.strip_prefix('-') {
        sign = "-";
        value = rest;
    }
    if value.is_empty() {
        return Ok(None);
    }
    let lower_exponent = find_byte_with_cancellation(value, 0..value.len(), b'e', cancellation)?;
    let upper_exponent = find_byte_with_cancellation(value, 0..value.len(), b'E', cancellation)?;
    let exponent_at = match (lower_exponent, upper_exponent) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    };
    if let Some(at) = exponent_at
        && bytes_any_with_cancellation(&value.as_bytes()[at + 1..], cancellation, |byte| {
            matches!(byte, b'e' | b'E')
        })?
    {
        return Ok(None);
    }
    let (mantissa, exponent) =
        exponent_at.map_or((value, None), |at| (&value[..at], Some(&value[at + 1..])));
    let exponent = if let Some(exponent) = exponent {
        let (sign, digits) = exponent
            .strip_prefix(['+', '-'])
            .map_or(("", exponent), |digits| (&exponent[..1], digits));
        if digits.is_empty()
            || !bytes_all_with_cancellation(digits.as_bytes(), cancellation, |byte| {
                byte.is_ascii_digit()
            })?
        {
            return Ok(None);
        }
        Some((sign, digits))
    } else {
        None
    };

    let decimal_at = find_byte_with_cancellation(mantissa, 0..mantissa.len(), b'.', cancellation)?;
    if let Some(at) = decimal_at
        && bytes_any_with_cancellation(&mantissa.as_bytes()[at + 1..], cancellation, |byte| {
            byte == b'.'
        })?
    {
        return Ok(None);
    }
    let (integer, fraction) = decimal_at.map_or((mantissa, None), |at| {
        (&mantissa[..at], Some(&mantissa[at + 1..]))
    });
    if !bytes_all_with_cancellation(integer.as_bytes(), cancellation, |byte| {
        byte.is_ascii_digit()
    })? {
        return Ok(None);
    }
    if let Some(fraction) = fraction
        && !bytes_all_with_cancellation(fraction.as_bytes(), cancellation, |byte| {
            byte.is_ascii_digit()
        })?
    {
        return Ok(None);
    }
    if integer.is_empty() && fraction.is_none_or(str::is_empty) {
        return Ok(None);
    }

    let mut leading_zeroes = 0_usize;
    while integer.as_bytes().get(leading_zeroes) == Some(&b'0') {
        cancellation.advance(1)?;
        leading_zeroes += 1;
    }
    let integer = &integer[leading_zeroes..];
    let integer = if integer.is_empty() { "0" } else { integer };
    let mut normalized = match fraction {
        Some("") => format!("{integer}.0"),
        Some(fraction) => format!("{integer}.{fraction}"),
        None => integer.to_owned(),
    };
    if let Some((exponent_sign, digits)) = exponent {
        normalized.push('e');
        normalized.push_str(exponent_sign);
        normalized.push_str(digits);
    }
    if sign == "-" && normalized != "0" {
        normalized.insert(0, '-');
    }
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn repaired(input: &str) -> RepairOutput<'_> {
        repair(input, RepairPolicy::Conservative).expect("input should be repairable")
    }

    fn assert_valid_json(output: &RepairOutput<'_>) {
        serde_json::from_str::<Value>(output.json()).expect("repair must emit strict JSON");
    }

    #[test]
    fn valid_json_is_borrowed_and_unchanged() {
        let input = " {\"a\":1,\"a\":2} \n";
        let output = repaired(input);
        assert!(matches!(output.json, Cow::Borrowed(_)));
        assert_eq!(output.json(), input);
        assert!(output.repairs().is_empty());
        assert_eq!(output.original_offset(input.len()), Some(input.len()));
    }

    #[test]
    fn preserves_duplicate_keys_and_order() {
        let output = repaired("{a:1,a:2,b:3}");
        assert_eq!(output.json(), "{\"a\":1,\"a\":2,\"b\":3}");
        assert_eq!(output.json().match_indices("\"a\"").count(), 2);
        assert_valid_json(&output);
    }

    #[test]
    fn removes_single_markdown_fence_and_surrounding_text() {
        let input = "answer:\n```json\n{\"a\":1}\n```\ndone";
        let output = repaired(input);
        assert_eq!(output.json(), "{\"a\":1}\n");
        assert_eq!(
            output
                .repairs()
                .iter()
                .filter(|repair| repair.kind() == RepairKind::RemovedMarkdownFence)
                .count(),
            2
        );
        assert!(
            output
                .repairs()
                .iter()
                .any(|repair| repair.kind() == RepairKind::RemovedSurroundingText)
        );
        assert_valid_json(&output);
    }

    #[test]
    fn records_fence_deletions_at_their_output_boundaries() {
        let input = "before\n```json\n{\"a\":1}\n```\nafter";
        let output = repaired(input);
        let fences = output
            .repairs()
            .iter()
            .filter(|repair| repair.kind() == RepairKind::RemovedMarkdownFence)
            .collect::<Vec<_>>();
        assert_eq!(fences.len(), 2);
        assert_eq!(fences[0].output_range(), 0..0);
        assert_eq!(
            fences[1].output_range(),
            output.json().len()..output.json().len()
        );
        assert!(!output.repairs().iter().any(|left| {
            output.repairs().iter().any(|right| {
                !std::ptr::eq(left, right)
                    && left.original_range().start < right.original_range().end
                    && right.original_range().start < left.original_range().end
            })
        }));
    }

    #[test]
    fn removes_bom_without_discarding_json_whitespace() {
        let input = "\u{feff} \r\n{\"a\":1}\n";
        let output = repaired(input);
        assert_eq!(output.json(), " \r\n{\"a\":1}\n");
        assert_eq!(output.repairs().len(), 1);
        assert_eq!(output.repairs()[0].kind(), RepairKind::RemovedByteOrderMark);
        assert_valid_json(&output);
    }

    #[test]
    fn conservative_rejects_multiple_candidates_and_best_effort_takes_first() {
        let input = "first {\"a\":1} second {\"b\":2}";
        let error = repair(input, RepairPolicy::Conservative).expect_err("must reject ambiguity");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);

        let output = repair(input, RepairPolicy::BestEffort).expect("best effort takes first");
        assert_eq!(output.json(), "{\"a\":1}");
        assert_valid_json(&output);

        let fenced_and_plain = "```json\n{\"a\":1}\n```\n{\"b\":2}";
        let error = repair(fenced_and_plain, RepairPolicy::Conservative)
            .expect_err("围栏外的第二个结构候选也必须形成歧义");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);

        let output = repair(fenced_and_plain, RepairPolicy::BestEffort)
            .expect("BestEffort 应选择文本顺序中的第一个候选");
        assert_eq!(output.json(), "{\"a\":1}\n");
        assert_valid_json(&output);
    }

    #[test]
    fn fenced_candidate_closes_at_the_fence_without_hiding_later_json() {
        let input = "```json\n{\"a\":1\n```";
        let output = repaired(input);
        assert_eq!(output.json(), "{\"a\":1\n}");
        assert_valid_json(&output);

        let with_later_json = "```json\n{\"a\":1\n```\n{\"b\":2}";
        let error = repair(with_later_json, RepairPolicy::Conservative)
            .expect_err("围栏外的完整候选不能被围栏内的缺失闭合符吞掉");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);

        let trailing_same_line = "```json\n{\"a\":1}\n``` {\"b\":2}";
        let error = repair(trailing_same_line, RepairPolicy::Conservative)
            .expect_err("closing fence 同行的 JSON 不能随围栏一起删除");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);
    }

    #[test]
    fn repairs_comments_quotes_bare_tokens_and_punctuation() {
        let input = "{/* note */ foo 'bar', list:[one,two,], tail:true}";
        let output = repaired(input);
        assert_eq!(
            output.json(),
            "{ \"foo\" :\"bar\", \"list\":[\"one\",\"two\"], \"tail\":true}"
        );
        assert_valid_json(&output);
    }

    #[test]
    fn repairs_missing_and_extra_colons_and_commas() {
        let output = repaired("{a::1 b 2,,c:3:}");
        assert_eq!(output.json(), "{\"a\":1 ,\"b\" :2,\"c\":3}");
        assert_valid_json(&output);
    }

    #[test]
    fn repairs_separator_runs_and_missing_separator_without_whitespace() {
        let object = repaired("{\"a\":1,,}");
        assert_eq!(object.json(), "{\"a\":1}");
        assert_valid_json(&object);

        let array = repair("[1,,,]", RepairPolicy::BestEffort)
            .expect("BestEffort 应删除存在缺值歧义的逗号串");
        assert_eq!(array.json(), "[1]");
        assert_valid_json(&array);

        let adjacent = repaired("{\"a\":1\"b\":2}");
        assert_eq!(adjacent.json(), "{\"a\":1,\"b\":2}");
        assert_valid_json(&adjacent);

        let mismatched_object_close = repaired("[{\"a\":1,]");
        assert_eq!(mismatched_object_close.json(), "[{\"a\":1}]");
        assert_valid_json(&mismatched_object_close);

        let mismatched_array_close = repaired("{\"a\":[1,}");
        assert_eq!(mismatched_array_close.json(), "{\"a\":[1]}");
        assert_valid_json(&mismatched_array_close);
    }

    #[test]
    fn conservative_rejects_ambiguous_missing_array_values_and_adjacent_strings() {
        for input in ["[1,,2]", "[,1]", "[\"a\"\"b\"]", "[\"a\" \"b\"]"] {
            let error = repair(input, RepairPolicy::Conservative)
                .expect_err("Conservative 不应替调用方选择缺值或相邻字符串的解释");
            assert!(matches!(
                error.kind(),
                RepairErrorKind::MissingValue | RepairErrorKind::AmbiguousStringQuote
            ));
        }

        let output = repair("[1,,2]", RepairPolicy::BestEffort)
            .expect("BestEffort 可以把重复逗号解释为多余分隔符");
        assert_eq!(output.json(), "[1,2]");
        assert_valid_json(&output);
    }

    #[test]
    fn repairs_missing_colon_after_a_quoted_key() {
        let output = repaired("{\"a\" 1}");
        assert_eq!(output.json(), "{\"a\" :1}");
        assert_valid_json(&output);
    }

    #[test]
    fn repairs_smart_quotes_control_characters_and_invalid_escapes() {
        let output = repaired("{“a”:“line\n\\q”}");
        assert_eq!(output.json(), "{\"a\":\"line\\n\\\\q\"}");
        assert_valid_json(&output);

        let unicode_after_invalid_escape = repaired("{\"a\":\"\\ué中\"}");
        assert_eq!(unicode_after_invalid_escape.json(), "{\"a\":\"\\\\ué中\"}");
        assert_valid_json(&unicode_after_invalid_escape);
    }

    #[test]
    fn recognizes_a_string_terminator_before_a_comment() {
        let output = repaired("{\"a\":\"x\" /* note */, \"b\":1}");
        assert_eq!(output.json(), "{\"a\":\"x\" , \"b\":1}");
        assert_valid_json(&output);
    }

    #[test]
    fn conservative_rejects_ambiguous_internal_double_quotes() {
        for input in ["{\"text\":\"type: \"free\"\"}", "{\"text\":\"a \"b\" c\"}"] {
            let error = repair(input, RepairPolicy::Conservative)
                .expect_err("内部双引号既可能是字符串边界也可能是正文，不得猜测");
            assert_eq!(error.kind(), RepairErrorKind::AmbiguousStringQuote);
        }
    }

    #[test]
    fn inserts_missing_closing_delimiters_without_values() {
        let output = repaired("{\"a\":[1,2");
        assert_eq!(output.json(), "{\"a\":[1,2]}");
        assert_valid_json(&output);

        let error = repair("{\"a\":", RepairPolicy::Conservative).expect_err("value is absent");
        assert_eq!(error.kind(), RepairErrorKind::MissingValue);
    }

    #[test]
    fn conservative_rejects_unterminated_string_and_best_effort_closes_it() {
        let input = "{\"a\":\"text";
        let error = repair(input, RepairPolicy::Conservative).expect_err("must reject");
        assert_eq!(error.kind(), RepairErrorKind::UnterminatedString);

        let output = repair(input, RepairPolicy::BestEffort).expect("best effort closes string");
        assert_eq!(output.json(), "{\"a\":\"text\"}");
        assert_valid_json(&output);

        let before_delimiter = repair("{\"a\":\"text}", RepairPolicy::BestEffort)
            .expect("best effort closes before a container delimiter");
        assert_eq!(before_delimiter.json(), "{\"a\":\"text\"}");
        assert_valid_json(&before_delimiter);
    }

    #[test]
    fn best_effort_normalizes_literals_and_simple_numbers() {
        let output = repair(
            "[True,False,None,+1,.5,1.,007,+01.20e+03]",
            RepairPolicy::BestEffort,
        )
        .expect("best effort should normalize values");
        assert_eq!(output.json(), "[true,false,null,1,0.5,1.0,7,1.20e+03]");
        assert_valid_json(&output);

        let error = repair("[+1]", RepairPolicy::Conservative).expect_err("must reject number");
        assert_eq!(error.kind(), RepairErrorKind::InvalidNumber);
    }

    #[test]
    fn maps_replacements_and_insertions_to_original_offsets() {
        let input = "前缀 {a:1}";
        let output = repaired(input);
        assert_eq!(output.json(), "{\"a\":1}");
        assert_eq!(output.original_offset(0), Some("前缀 ".len()));
        assert_eq!(
            output.original_offset(output.json().len()),
            Some(input.len())
        );
        assert_eq!(output.original_offset(output.json().len() + 1), None);
    }

    #[test]
    fn maps_a_deleted_suffix_to_the_end_of_the_original_input() {
        let input = "1/* note */";
        let output = repaired(input);
        assert_eq!(output.json(), "1");
        assert_eq!(
            output.original_offset(output.json().len()),
            Some(input.len())
        );

        let fenced = "```json\n{\"a\":1}\n```\nafter";
        let output = repaired(fenced);
        assert_eq!(
            output.original_offset(output.json().len()),
            Some(fenced.len())
        );
    }

    #[test]
    fn source_map_compresses_contiguous_copies() {
        let input = format!("[\"{}\"]", "内容".repeat(100_000));
        let output = repaired(&input);
        assert_eq!(output.source_map.segments.len(), 1);
        assert_eq!(output.original_offset(input.len()), Some(input.len()));
    }

    #[test]
    fn handles_crlf_and_utf8_ranges_in_bytes() {
        let input = "说明\r\n```json\r\n{名字:'值'}\r\n```\r\n";
        let output = repaired(input);
        assert_eq!(output.json(), "{\"名字\":\"值\"}\r\n");
        assert_valid_json(&output);
        for repair in output.repairs() {
            assert!(repair.original_range().end <= input.len());
            assert!(repair.output_range().end <= output.json().len());
        }
    }

    #[test]
    fn deeply_nested_input_uses_explicit_stack() {
        let depth = 20_000;
        let input = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let output = repaired(&input);
        assert_eq!(output.json(), input);
        assert!(output.repairs().is_empty());
    }

    #[test]
    fn cancellation_is_checked_at_start_and_each_64_kib() {
        let input = format!("[\"{}\"]", "x".repeat(CANCELLATION_INTERVAL * 2));
        let mut checks = 0;
        let result = repair_with_cancellation(&input, RepairPolicy::Conservative, || {
            checks += 1;
            if checks == 3 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err("cancelled")));
        assert_eq!(checks, 3);
    }

    #[test]
    fn dense_internal_quotes_and_trailing_commas_have_linear_scan_budgets() {
        let quote_count = 100_000;
        let mut quoted = String::from("{\"text\":\"");
        for _ in 0..quote_count {
            quoted.push_str("x\"");
        }
        quoted.push_str("x\"}");

        let mut quote_checks = 0_usize;
        let quoted_output = repair_with_cancellation(
            &quoted,
            RepairPolicy::BestEffort,
            || -> Result<(), Infallible> {
                quote_checks += 1;
                Ok(())
            },
        )
        .expect("取消检查不会失败")
        .expect("BestEffort 的密集内部引号选择仍应保持线性");
        assert_valid_json(&quoted_output);
        let quote_budget = quoted.len().div_ceil(CANCELLATION_INTERVAL) * 8 + 16;
        assert!(
            quote_checks <= quote_budget,
            "内部引号扫描次数 {quote_checks} 超过线性预算 {quote_budget}"
        );

        let commas = format!("[0{}]", ",".repeat(CANCELLATION_INTERVAL * 2));
        let mut comma_checks = 0_usize;
        let comma_output = repair_with_cancellation(
            &commas,
            RepairPolicy::BestEffort,
            || -> Result<(), Infallible> {
                comma_checks += 1;
                Ok(())
            },
        )
        .expect("取消检查不会失败")
        .expect("密集尾逗号应可修复");
        assert_eq!(comma_output.json(), "[0]");
        assert_valid_json(&comma_output);
        let comma_budget = commas.len().div_ceil(CANCELLATION_INTERVAL) * 8 + 16;
        assert!(
            comma_checks <= comma_budget,
            "尾逗号扫描次数 {comma_checks} 超过线性预算 {comma_budget}"
        );
    }

    #[test]
    fn deterministic_malformed_inputs_never_emit_invalid_json() {
        let cases = [
            "{a:1}",
            "[1 2 3]",
            "{a:'x',}",
            "/*x*/{a:true}",
            "{a:[1,2}",
            "{a:\"x\\q\"}",
            "{a:bare}",
            "{a:1,,b:2}",
        ];
        for input in cases {
            let output = repaired(input);
            assert_valid_json(&output);
        }
    }
}
