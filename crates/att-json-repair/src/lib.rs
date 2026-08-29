//! 面向不可信模型输出的、保留顺序与重复字段的 JSON 修复器。
//!
//! 本 crate 只把近似 JSON 转换成严格 JSON 文本。它不解释业务 schema，也不会通过
//! map 覆盖重复字段。调用方应在修复后继续执行自己的结构和业务校验。
//! 行为调查固定参考 Python `json_repair` 的 `600ede6` 提交；本实现和测试均独立编写。

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::ops::Range;

const CANCELLATION_INTERVAL: usize = 64 * 1024;

/// 修复后的严格 JSON 及其位置映射。
#[derive(Debug, Clone)]
pub struct RepairOutput<'a> {
    json: Cow<'a, str>,
    source_map: SourceMap,
}

impl<'a> RepairOutput<'a> {
    /// 返回严格 JSON 文本。
    #[must_use]
    pub fn json(&self) -> &str {
        &self.json
    }

    /// 把输出字节边界映射回原始输入字节边界。
    ///
    /// `output_offset` 可以等于输出长度；超过输出长度时返回 `None`。
    #[must_use]
    pub fn original_offset(&self, output_offset: usize) -> Option<usize> {
        self.source_map.original_offset(output_offset)
    }
}

/// 无法安全修复时的内部错误类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RepairErrorKind {
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
    const fn code(self) -> &'static str {
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
    #[cfg(test)]
    const fn kind(&self) -> RepairErrorKind {
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

/// 修复一段近似 JSON 文本，并允许调用方周期性检查取消状态。
///
/// 外层 `Result` 只传播取消检查错误；内层 `Result` 表示 JSON 是否能够安全修复。
pub fn repair_with_cancellation<E>(
    input: &str,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<RepairOutput<'_>, RepairError>, E> {
    let mut cancellation = Cancellation::new(ensure_running);
    cancellation.start()?;
    let candidate = match select_candidate(input, &mut cancellation)? {
        Ok(candidate) => candidate,
        Err(error) => return Ok(Err(error)),
    };
    let parser = Parser::new(input, candidate, &mut cancellation)?;
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
    removals: Vec<Range<usize>>,
    fenced: bool,
}

impl Candidate {
    fn logical_start(&self) -> usize {
        self.removals
            .iter()
            .map(|range| range.start)
            .min()
            .unwrap_or(self.range.start)
    }

    fn is_fenced(&self) -> bool {
        self.fenced
    }

    fn logical_end(&self) -> usize {
        self.removals
            .iter()
            .map(|range| range.end)
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
                fenced: false,
            });
        }
    }
    if candidates.len() > 1 {
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
                removals: std::iter::once(0..bom_len).collect(),
                fenced: false,
            }));
        }
        let outside_is_only_whitespace =
            chars_all_whitespace_with_cancellation(&input[..range.start], cancellation)?
                && chars_all_whitespace_with_cancellation(&input[range.end..], cancellation)?;
        let mut candidate = if outside_is_only_whitespace {
            Candidate {
                range: 0..input.len(),
                removals: Vec::new(),
                fenced: false,
            }
        } else {
            Candidate {
                range,
                removals: Vec::new(),
                fenced: false,
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
                removals: std::iter::once(0..bom_len).collect(),
                fenced: false,
            }));
        }
    }
    if trimmed.start < trimmed.end && looks_like_scalar_start(input[trimmed.start..].chars().next())
    {
        return Ok(Ok(Candidate {
            range: 0..input.len(),
            removals: Vec::new(),
            fenced: false,
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
        .map(|range| range.start)
        .min()
        .unwrap_or(candidate.range.start);
    let closing_end = candidate
        .removals
        .iter()
        .map(|range| range.end)
        .max()
        .unwrap_or(candidate.range.end);
    if bytes_any_with_cancellation(&input.as_bytes()[..opening_start], cancellation, |byte| {
        !is_json_whitespace(byte)
    })? {
        candidate.removals.push(0..opening_start);
    }
    if bytes_any_with_cancellation(&input.as_bytes()[closing_end..], cancellation, |byte| {
        !is_json_whitespace(byte)
    })? {
        candidate.removals.push(closing_end..input.len());
    }
    candidate
        .removals
        .sort_by_key(|range| (range.start, range.end));
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
        candidate.removals.push(0..candidate.range.start);
    }
    if bytes_any_with_cancellation(
        &input.as_bytes()[candidate.range.end..],
        cancellation,
        |byte| !is_json_whitespace(byte),
    )? {
        candidate.removals.push(candidate.range.end..input.len());
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
                    removals: vec![opening_range.clone(), closing_range],
                    fenced: true,
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
    root_state: RootState,
    frames: Vec<Frame>,
    quotes: QuoteIndex,
    builder: Builder<'input>,
    suffix_removals: Vec<Range<usize>>,
    cancellation: &'cancel mut Cancellation<F>,
}

impl<'input, 'cancel, F, E> Parser<'input, 'cancel, F>
where
    F: FnMut() -> Result<(), E>,
{
    fn new(
        input: &'input str,
        candidate: Candidate,
        cancellation: &'cancel mut Cancellation<F>,
    ) -> Result<Self, E> {
        let quotes = QuoteIndex::new(input, candidate.range.clone(), cancellation)?;
        let mut builder = Builder::new(input, candidate.range.start);
        let mut suffix_removals = Vec::new();
        for range in candidate.removals {
            if range.start >= candidate.range.end {
                suffix_removals.push(range);
            } else {
                builder.mark_changed();
            }
        }
        Ok(Self {
            input,
            end: candidate.range.end,
            position: candidate.range.start,
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
                for range in self.suffix_removals.drain(..) {
                    self.builder.record_suffix_deletion_at(range, output_offset);
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
                self.builder.delete(start..self.position);
                continue;
            }
            if character.is_whitespace() {
                let start = self.position;
                self.advance_current()?;
                self.builder.replace(start..self.position, " ");
                continue;
            }
            if self.starts_with("//") {
                let start = self.position;
                self.advance_bytes(2)?;
                while self.position < self.end && self.current_char() != '\n' {
                    self.advance_current()?;
                }
                self.builder.delete(start..self.position);
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
                self.builder.delete(start..self.position);
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
                self.builder.delete(start..self.position);
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder.delete(start..self.position);
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
        self.builder.insert(self.position, ":");
        self.set_last_state(FrameState::ObjectValue);
        Ok(None)
    }

    fn object_value(&mut self) -> Result<Option<RepairError>, E> {
        if self.current_char() == ':' {
            let start = self.position;
            self.advance_current()?;
            self.builder.delete(start..self.position);
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
                    self.builder.delete(comma..self.position);
                } else {
                    self.copy_current()?;
                    self.set_last_state(FrameState::ObjectKeyOrEnd);
                }
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder.delete(start..self.position);
                Ok(None)
            }
            _ => {
                self.builder.insert(self.position, ",");
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
            ',' => Ok(Some(RepairError::new(
                RepairErrorKind::MissingValue,
                self.position,
            ))),
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder.delete(start..self.position);
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
                if next.is_some_and(|(_, character)| character == ',') {
                    return Ok(Some(RepairError::new(
                        RepairErrorKind::MissingValue,
                        next.expect("checked Some").0,
                    )));
                }
                if next.is_none_or(|(_, character)| matches!(character, ',' | '}' | ']')) {
                    self.advance_current()?;
                    self.builder.delete(comma..self.position);
                } else {
                    self.copy_current()?;
                    self.set_last_state(FrameState::ArrayValueOrEnd);
                }
                Ok(None)
            }
            ':' => {
                let start = self.position;
                self.advance_current()?;
                self.builder.delete(start..self.position);
                Ok(None)
            }
            _ => {
                self.builder.insert(self.position, ",");
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
            self.builder.replace(start..opener_end, "\"");
        }

        while self.position < self.end {
            let character_start = self.position;
            let character = self.current_char();
            let width = character.len_utf8();

            if quote.closes(character) {
                let after = self.position + width;
                let (next_position, next, had_whitespace) = self.peek_after_string_quote(after)?;
                // `"a" "b"` 既可能是漏逗号的两个字符串，也可能是正文包含两个未转义
                // 引号的一个字符串。空白不能替调用方消除这种歧义。
                if role == StringRole::Value
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
                        self.builder.replace(character_start..self.position, "\"");
                    }
                    return Ok(None);
                }

                let later_quote = self.find_later_quote(quote, next_position);
                if role == StringRole::Key && later_quote.is_none() {
                    self.advance_current()?;
                    if quote.is_standard() {
                        self.builder.copy(character_start..self.position);
                    } else {
                        self.builder.replace(character_start..self.position, "\"");
                    }
                    return Ok(None);
                }
                // 当前引号既可能是正文，也可能是边界并伴随其他语法缺失；后续引号不能
                // 反向证明唯一意图，修复器不替调用方选择其中一种解释。
                return Ok(Some(RepairError::new(
                    RepairErrorKind::AmbiguousStringQuote,
                    character_start,
                )));
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
                self.builder
                    .replace(character_start..self.position, &replacement);
                continue;
            }
            if character == '"' && quote != Quote::Double {
                self.advance_current()?;
                self.builder.replace(character_start..self.position, "\\\"");
                continue;
            }
            self.advance_current()?;
            self.builder.copy(character_start..self.position);
        }

        Ok(Some(RepairError::new(
            RepairErrorKind::UnterminatedString,
            start,
        )))
    }

    fn consume_escape(&mut self, quote: Quote) -> Result<Option<RepairError>, E> {
        let start = self.position;
        self.advance_current()?;
        if self.position >= self.end {
            self.builder.replace(start..self.position, "\\\\");
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
                self.builder.replace(start..self.position, "\\\\");
            }
            return Ok(None);
        }
        self.advance_current()?;
        if escaped == '\'' && quote == Quote::Single {
            self.builder.replace(start..self.position, "'");
        } else {
            let replacement = format!("\\\\{escaped}");
            self.builder.replace(start..self.position, &replacement);
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
            self.builder.replace(start..self.position, &replacement);
            return Ok(None);
        }

        if matches!(token, "true" | "false" | "null") {
            self.builder.copy(start..self.position);
            return Ok(None);
        }
        if looks_number_like(token) {
            if is_strict_json_number_with_cancellation(token, self.cancellation)? {
                self.builder.copy(start..self.position);
                return Ok(None);
            }
            return Ok(Some(RepairError::new(
                RepairErrorKind::InvalidNumber,
                start,
            )));
        }

        let replacement = quote_bare_with_cancellation(token, self.cancellation)?;
        self.builder.replace(start..self.position, &replacement);
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
        self.builder.insert(self.position, closing);
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
            self.builder.insert(self.position, closing);
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
    changed: bool,
    source_map: SourceMap,
}

impl<'a> Builder<'a> {
    fn new(input: &'a str, original_start: usize) -> Self {
        Self {
            input,
            output: String::new(),
            changed: false,
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

    fn replace(&mut self, original: Range<usize>, replacement: &str) {
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
        self.changed = true;
    }

    fn insert(&mut self, original_offset: usize, text: &str) {
        self.replace(original_offset..original_offset, text);
    }

    fn delete(&mut self, original: Range<usize>) {
        self.replace(original, "");
    }

    fn mark_changed(&mut self) {
        self.changed = true;
    }

    fn record_suffix_deletion_at(&mut self, original: Range<usize>, output_offset: usize) {
        self.source_map.push_boundary(output_offset, original.end);
        self.changed = true;
    }

    fn finish(self) -> RepairOutput<'a> {
        RepairOutput {
            json: if self.changed {
                Cow::Owned(self.output)
            } else {
                Cow::Borrowed(self.input)
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::convert::Infallible;

    fn repair_result(input: &str) -> Result<RepairOutput<'_>, RepairError> {
        match repair_with_cancellation(input, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(error) => match error {},
        }
    }

    fn repaired(input: &str) -> RepairOutput<'_> {
        repair_result(input).expect("input should be repairable")
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
        assert_valid_json(&output);
    }

    #[test]
    fn removes_bom_without_discarding_json_whitespace() {
        let input = "\u{feff} \r\n{\"a\":1}\n";
        let output = repaired(input);
        assert_eq!(output.json(), " \r\n{\"a\":1}\n");
        assert_valid_json(&output);
    }

    #[test]
    fn rejects_multiple_candidates() {
        let input = "first {\"a\":1} second {\"b\":2}";
        let error = repair_result(input).expect_err("must reject ambiguity");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);

        let fenced_and_plain = "```json\n{\"a\":1}\n```\n{\"b\":2}";
        let error =
            repair_result(fenced_and_plain).expect_err("围栏外的第二个结构候选也必须形成歧义");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);
    }

    #[test]
    fn fenced_candidate_closes_at_the_fence_without_hiding_later_json() {
        let input = "```json\n{\"a\":1\n```";
        let output = repaired(input);
        assert_eq!(output.json(), "{\"a\":1\n}");
        assert_valid_json(&output);

        let with_later_json = "```json\n{\"a\":1\n```\n{\"b\":2}";
        let error = repair_result(with_later_json)
            .expect_err("围栏外的完整候选不能被围栏内的缺失闭合符吞掉");
        assert_eq!(error.kind(), RepairErrorKind::MultipleJsonCandidates);

        let trailing_same_line = "```json\n{\"a\":1}\n``` {\"b\":2}";
        let error = repair_result(trailing_same_line)
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
    fn rejects_ambiguous_missing_array_values_and_adjacent_strings() {
        for input in ["[1,,2]", "[,1]", "[\"a\"\"b\"]", "[\"a\" \"b\"]"] {
            let error =
                repair_result(input).expect_err("修复器不应替调用方选择缺值或相邻字符串的解释");
            assert!(matches!(
                error.kind(),
                RepairErrorKind::MissingValue | RepairErrorKind::AmbiguousStringQuote
            ));
        }
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
    fn rejects_ambiguous_internal_double_quotes() {
        for input in ["{\"text\":\"type: \"free\"\"}", "{\"text\":\"a \"b\" c\"}"] {
            let error = repair_result(input)
                .expect_err("内部双引号既可能是字符串边界也可能是正文，不得猜测");
            assert_eq!(error.kind(), RepairErrorKind::AmbiguousStringQuote);
        }
    }

    #[test]
    fn inserts_missing_closing_delimiters_without_values() {
        let output = repaired("{\"a\":[1,2");
        assert_eq!(output.json(), "{\"a\":[1,2]}");
        assert_valid_json(&output);

        let error = repair_result("{\"a\":").expect_err("value is absent");
        assert_eq!(error.kind(), RepairErrorKind::MissingValue);
    }

    #[test]
    fn rejects_unterminated_string() {
        let input = "{\"a\":\"text";
        let error = repair_result(input).expect_err("must reject");
        assert_eq!(error.kind(), RepairErrorKind::UnterminatedString);
    }

    #[test]
    fn rejects_invalid_number() {
        let error = repair_result("[+1]").expect_err("must reject number");
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

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_source_map_compresses_contiguous_copies() {
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
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_deeply_nested_input_uses_explicit_stack() {
        let depth = 20_000;
        let input = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let output = repaired(&input);
        assert_eq!(output.json(), input);
        assert!(matches!(output.json, Cow::Borrowed(_)));
    }

    #[test]
    fn long_repair_can_be_cancelled() {
        let input = format!("[\"{}\"]", "x".repeat(CANCELLATION_INTERVAL * 2));
        let mut checks = 0;
        let result = repair_with_cancellation(&input, || {
            checks += 1;
            if checks > 1 { Err("cancelled") } else { Ok(()) }
        });
        assert!(matches!(result, Err("cancelled")));
        assert!(checks > 1);
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
