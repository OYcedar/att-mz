//! 写回前依据原文模板修复译文中能够唯一对应的符号。

use std::convert::Infallible;

use icu_properties::props::{
    BidiMirroringGlyph, BidiPairedBracketType, GeneralCategory, GeneralCategoryGroup,
    QuotationMark, WordBreak,
};
use icu_properties::{
    CodePointMapData, CodePointMapDataBorrowed, CodePointSetData, CodePointSetDataBorrowed,
};
use unicode_normalization::UnicodeNormalization;

use crate::language::{
    LanguageCharacterReplacement, LanguageRepairApplicationError, LanguageRepairPlan, LanguageText,
    LanguageTextSegment,
};

const CANCELLATION_CHECK_INTERVAL: usize = 256;
const MATRIX_INITIALIZATION_CHUNK: usize = 65_536;

/// 全局译文符号修复器。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TranslationSymbolRepairer;

/// 符号修复的正常结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationSymbolRepairOutcome {
    Unchanged,
    Repaired {
        plan: LanguageRepairPlan,
        replacement_count: usize,
    },
    Skipped {
        reason: TranslationSymbolRepairSkipReason,
    },
}

/// 当前 Unit 无法安全建立符号修复计划的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationSymbolRepairSkipReason {
    OpaqueBoundaryMismatch,
    SizeOverflow,
    ResourceExhausted,
    InvalidRepairPlan,
    RepairInvariantViolation,
    NonIdempotentRepair,
}

#[derive(Debug)]
enum RepairPlanningFailure<E> {
    Cancelled(E),
    Skipped(TranslationSymbolRepairSkipReason),
}

impl<E> From<TranslationSymbolRepairSkipReason> for RepairPlanningFailure<E> {
    fn from(reason: TranslationSymbolRepairSkipReason) -> Self {
        Self::Skipped(reason)
    }
}

fn ensure_repair_running<E>(
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), RepairPlanningFailure<E>> {
    ensure_running().map_err(RepairPlanningFailure::Cancelled)
}

impl TranslationSymbolRepairer {
    #[allow(dead_code)]
    pub(crate) fn plan_repair(
        source: &LanguageText,
        translation: &LanguageText,
    ) -> TranslationSymbolRepairOutcome {
        match Self::plan_repair_with_cancellation(source, translation, || Ok::<_, Infallible>(())) {
            Ok(outcome) => outcome,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn plan_repair_with_cancellation<E>(
        source: &LanguageText,
        translation: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<TranslationSymbolRepairOutcome, E> {
        match Self::plan_repair_inner(source, translation, &mut ensure_running) {
            Ok(outcome) => Ok(outcome),
            Err(RepairPlanningFailure::Cancelled(source)) => Err(source),
            Err(RepairPlanningFailure::Skipped(reason)) => {
                Ok(TranslationSymbolRepairOutcome::Skipped { reason })
            }
        }
    }

    fn plan_repair_inner<E>(
        source: &LanguageText,
        translation: &LanguageText,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<TranslationSymbolRepairOutcome, RepairPlanningFailure<E>> {
        ensure_repair_running(ensure_running)?;
        let classifier = SymbolClassifier::new();
        let source_symbols = collect_symbols(source, &classifier, ensure_running)?;
        let translation_symbols = collect_symbols(translation, &classifier, ensure_running)?;
        if source_symbols.opaque_boundaries != translation_symbols.opaque_boundaries {
            return Err(TranslationSymbolRepairSkipReason::OpaqueBoundaryMismatch.into());
        }

        let first = build_plan(
            source_symbols.occurrences,
            translation_symbols.occurrences,
            ensure_running,
        )?;
        if first.plan.is_unchanged() {
            return Ok(TranslationSymbolRepairOutcome::Unchanged);
        }

        let repaired = apply_plan(translation, &first.plan, ensure_running)?;
        if !repair_preserves_text_shape(translation, &repaired, &classifier, ensure_running)? {
            return Err(TranslationSymbolRepairSkipReason::RepairInvariantViolation.into());
        }

        let repaired_symbols = collect_symbols(&repaired, &classifier, ensure_running)?;
        let second = build_plan(
            first.source_symbols,
            repaired_symbols.occurrences,
            ensure_running,
        )?;
        if !second.plan.is_unchanged() {
            return Err(TranslationSymbolRepairSkipReason::NonIdempotentRepair.into());
        }

        ensure_repair_running(ensure_running)?;
        Ok(TranslationSymbolRepairOutcome::Repaired {
            plan: first.plan,
            replacement_count: first.replacement_count,
        })
    }
}

fn apply_plan<E>(
    translation: &LanguageText,
    plan: &LanguageRepairPlan,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<LanguageText, RepairPlanningFailure<E>> {
    match translation.apply_repair_with_cancellation(plan, ensure_running) {
        Ok(Ok(repaired)) => Ok(repaired),
        Ok(Err(LanguageRepairApplicationError::ResourceExhausted)) => {
            Err(TranslationSymbolRepairSkipReason::ResourceExhausted.into())
        }
        Ok(Err(LanguageRepairApplicationError::SizeOverflow)) => {
            Err(TranslationSymbolRepairSkipReason::SizeOverflow.into())
        }
        Ok(Err(_)) => Err(TranslationSymbolRepairSkipReason::InvalidRepairPlan.into()),
        Err(source) => Err(RepairPlanningFailure::Cancelled(source)),
    }
}

fn repair_preserves_text_shape<E>(
    original: &LanguageText,
    repaired: &LanguageText,
    classifier: &SymbolClassifier,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    if original.segments().len() != repaired.segments().len() {
        return Ok(false);
    }
    for (original, repaired) in original.segments().iter().zip(repaired.segments()) {
        ensure_repair_running(ensure_running)?;
        match (original, repaired) {
            (LanguageTextSegment::OpaqueBoundary, LanguageTextSegment::OpaqueBoundary) => {}
            (
                LanguageTextSegment::NaturalText(original),
                LanguageTextSegment::NaturalText(repaired),
            ) => {
                let mut original = original.chars().peekable();
                let mut repaired = repaired.chars().peekable();
                let mut original_previous = None;
                let mut repaired_previous = None;
                let mut character_index = 0_usize;
                loop {
                    if character_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
                        ensure_repair_running(ensure_running)?;
                    }
                    match (original.next(), repaired.next()) {
                        (Some(before), Some(after)) => {
                            if before != after {
                                let original_next = original.peek().copied();
                                let repaired_next = repaired.peek().copied();
                                if classifier
                                    .classify(before, original_previous, original_next)
                                    .is_none()
                                    || classifier
                                        .classify(after, repaired_previous, repaired_next)
                                        .is_none()
                                {
                                    return Ok(false);
                                }
                            }
                            original_previous = Some(before);
                            repaired_previous = Some(after);
                            character_index = character_index
                                .checked_add(1)
                                .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
                        }
                        (None, None) => break,
                        _ => return Ok(false),
                    }
                }
            }
            _ => return Ok(false),
        };
    }
    ensure_repair_running(ensure_running)?;
    Ok(true)
}

struct CollectedSymbols {
    occurrences: Vec<SymbolOccurrence>,
    opaque_boundaries: usize,
}

fn collect_symbols<E>(
    text: &LanguageText,
    classifier: &SymbolClassifier,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<CollectedSymbols, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let mut occurrences = Vec::new();
    let mut region = 0_usize;
    for (segment_index, segment) in text.segments().iter().enumerate() {
        ensure_repair_running(ensure_running)?;
        match segment {
            LanguageTextSegment::OpaqueBoundary => {
                region = region
                    .checked_add(1)
                    .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
            }
            LanguageTextSegment::NaturalText(text) => {
                let mut characters = text.char_indices().peekable();
                let mut previous = None;
                let mut character_index = 0_usize;
                while let Some((byte_offset, character)) = characters.next() {
                    if character_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
                        ensure_repair_running(ensure_running)?;
                    }
                    let next = characters.peek().map(|(_, character)| *character);
                    if let Some(classification) = classifier.classify(character, previous, next) {
                        occurrences
                            .try_reserve(1)
                            .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
                        occurrences.push(SymbolOccurrence {
                            segment_index,
                            region,
                            byte_offset,
                            character,
                            family: classification.family,
                            structure: classification.structure,
                            structural_role: None,
                            structural_pair: None,
                        });
                    }
                    previous = Some(character);
                    character_index = character_index
                        .checked_add(1)
                        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
                }
            }
        }
    }
    assign_structural_topology(&mut occurrences, ensure_running)?;
    ensure_repair_running(ensure_running)?;
    Ok(CollectedSymbols {
        occurrences,
        opaque_boundaries: region,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SymbolOccurrence {
    segment_index: usize,
    region: usize,
    byte_offset: usize,
    character: char,
    family: SymbolFamily,
    structure: Option<StructuralDescriptor>,
    structural_role: Option<StructuralRole>,
    structural_pair: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SymbolClassification {
    family: SymbolFamily,
    structure: Option<StructuralDescriptor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SymbolFamily {
    Quote,
    Bracket,
    WordApostrophe,
    Comma,
    Period,
    Colon,
    Semicolon,
    QuestionMark,
    ExclamationMark,
    Ellipsis,
    Dash,
    ForwardSlash,
    Backslash,
    Exact(char),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StructuralDescriptor {
    kind: StructuralKind,
    raw_role: RawStructuralRole,
    bracket_pair: Option<BracketPair>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuralKind {
    Quote,
    Bracket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawStructuralRole {
    Open,
    Close,
    Flexible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuralRole {
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BracketPair {
    opening: char,
    closing: char,
}

struct SymbolClassifier {
    categories: CodePointMapDataBorrowed<'static, GeneralCategory>,
    quotation_marks: CodePointSetDataBorrowed<'static>,
    bidi_mirroring: CodePointMapDataBorrowed<'static, BidiMirroringGlyph>,
    word_breaks: CodePointMapDataBorrowed<'static, WordBreak>,
}

impl SymbolClassifier {
    fn new() -> Self {
        Self {
            categories: CodePointMapData::new(),
            quotation_marks: CodePointSetData::new::<QuotationMark>(),
            bidi_mirroring: CodePointMapData::new(),
            word_breaks: CodePointMapData::new(),
        }
    }

    fn classify(
        &self,
        character: char,
        previous: Option<char>,
        next: Option<char>,
    ) -> Option<SymbolClassification> {
        let category = self.categories.get(character);
        let compatibility_ascii = compatibility_ascii_punctuation(character);
        let normalized = compatibility_ascii.unwrap_or(character);
        let is_candidate = character.is_ascii_punctuation()
            || GeneralCategoryGroup::Punctuation.contains(category)
            || compatibility_ascii.is_some();
        if !is_candidate {
            return None;
        }

        if is_word_apostrophe(
            character,
            compatibility_ascii,
            previous,
            next,
            &self.word_breaks,
        ) {
            return Some(SymbolClassification {
                family: SymbolFamily::WordApostrophe,
                structure: None,
            });
        }

        let quotation_character = if self.quotation_marks.contains(character) {
            Some(character)
        } else if normalized != character && self.quotation_marks.contains(normalized) {
            Some(normalized)
        } else {
            None
        };
        if let Some(quotation_character) = quotation_character {
            return Some(SymbolClassification {
                family: SymbolFamily::Quote,
                structure: Some(StructuralDescriptor {
                    kind: StructuralKind::Quote,
                    raw_role: quote_role(self.categories.get(quotation_character)),
                    bracket_pair: None,
                }),
            });
        }

        if let Some((role, pair)) = self.classify_bracket(character, normalized) {
            return Some(SymbolClassification {
                family: SymbolFamily::Bracket,
                structure: Some(StructuralDescriptor {
                    kind: StructuralKind::Bracket,
                    raw_role: role,
                    bracket_pair: Some(pair),
                }),
            });
        }

        let family = match normalized {
            ',' => SymbolFamily::Comma,
            '.' => SymbolFamily::Period,
            ':' => SymbolFamily::Colon,
            ';' => SymbolFamily::Semicolon,
            '?' => SymbolFamily::QuestionMark,
            '!' => SymbolFamily::ExclamationMark,
            '-' => SymbolFamily::Dash,
            '/' => SymbolFamily::ForwardSlash,
            '\\' => SymbolFamily::Backslash,
            _ if is_comma(character) => SymbolFamily::Comma,
            _ if is_period(character) => SymbolFamily::Period,
            _ if is_colon(character) => SymbolFamily::Colon,
            _ if is_semicolon(character) => SymbolFamily::Semicolon,
            _ if is_question_mark(character) => SymbolFamily::QuestionMark,
            _ if is_exclamation_mark(character) => SymbolFamily::ExclamationMark,
            _ if is_ellipsis(character) => SymbolFamily::Ellipsis,
            _ if category == GeneralCategory::DashPunctuation => SymbolFamily::Dash,
            _ => SymbolFamily::Exact(normalized),
        };
        Some(SymbolClassification {
            family,
            structure: None,
        })
    }

    fn classify_bracket(
        &self,
        character: char,
        normalized: char,
    ) -> Option<(RawStructuralRole, BracketPair)> {
        for (candidate_index, bracket_character) in [character, normalized].into_iter().enumerate()
        {
            if candidate_index == 1 && normalized == character {
                continue;
            }
            let bidi = self.bidi_mirroring.get(bracket_character);
            let role = match bidi.paired_bracket_type {
                BidiPairedBracketType::Open => RawStructuralRole::Open,
                BidiPairedBracketType::Close => RawStructuralRole::Close,
                BidiPairedBracketType::None => continue,
                _ => continue,
            };
            let Some(paired) = bidi.mirroring_glyph else {
                continue;
            };
            let canonical_character =
                compatibility_ascii_punctuation(bracket_character).unwrap_or(bracket_character);
            let canonical_paired = compatibility_ascii_punctuation(paired).unwrap_or(paired);
            let pair = match role {
                RawStructuralRole::Open => BracketPair {
                    opening: canonical_character,
                    closing: canonical_paired,
                },
                RawStructuralRole::Close => BracketPair {
                    opening: canonical_paired,
                    closing: canonical_character,
                },
                RawStructuralRole::Flexible => unreachable!("双向括号只有开闭角色"),
            };
            return Some((role, pair));
        }
        None
    }
}

fn compatibility_ascii_punctuation(character: char) -> Option<char> {
    let mut encoded = [0_u8; 4];
    let mut normalized = character.encode_utf8(&mut encoded).nfkc();
    let first = normalized.next()?;
    if normalized.next().is_none() && first.is_ascii_punctuation() {
        Some(first)
    } else {
        None
    }
}

fn is_word_apostrophe(
    character: char,
    compatibility_ascii: Option<char>,
    previous: Option<char>,
    next: Option<char>,
    word_breaks: &CodePointMapDataBorrowed<'static, WordBreak>,
) -> bool {
    (compatibility_ascii == Some('\'') || matches!(character, '\'' | '\u{2019}' | '\u{02bc}'))
        && previous.is_some_and(|character| is_apostrophe_word_letter(word_breaks.get(character)))
        && next.is_some_and(|character| is_apostrophe_word_letter(word_breaks.get(character)))
}

fn is_apostrophe_word_letter(word_break: WordBreak) -> bool {
    matches!(word_break, WordBreak::ALetter | WordBreak::HebrewLetter)
}

fn quote_role(category: GeneralCategory) -> RawStructuralRole {
    match category {
        GeneralCategory::InitialPunctuation | GeneralCategory::OpenPunctuation => {
            RawStructuralRole::Open
        }
        GeneralCategory::FinalPunctuation | GeneralCategory::ClosePunctuation => {
            RawStructuralRole::Close
        }
        _ => RawStructuralRole::Flexible,
    }
}

fn is_comma(character: char) -> bool {
    matches!(
        character,
        '\u{055d}'
            | '\u{060c}'
            | '\u{07f8}'
            | '\u{1363}'
            | '\u{1802}'
            | '\u{1808}'
            | '\u{2e32}'
            | '\u{2e34}'
            | '\u{2e41}'
            | '\u{2e4c}'
            | '\u{3001}'
            | '\u{a4fe}'
            | '\u{a60d}'
            | '\u{a6f5}'
            | '\u{fe10}'
            | '\u{fe11}'
            | '\u{ff64}'
            | '\u{1144d}'
            | '\u{16e97}'
            | '\u{1da87}'
    )
}

fn is_period(character: char) -> bool {
    matches!(
        character,
        '\u{0589}'
            | '\u{06d4}'
            | '\u{0701}'
            | '\u{0702}'
            | '\u{1362}'
            | '\u{166e}'
            | '\u{1803}'
            | '\u{1809}'
            | '\u{2cf9}'
            | '\u{2cfe}'
            | '\u{2e3c}'
            | '\u{3002}'
            | '\u{a4ff}'
            | '\u{a60e}'
            | '\u{a6f3}'
            | '\u{fe12}'
            | '\u{ff61}'
            | '\u{16af5}'
            | '\u{16e98}'
            | '\u{1bc9f}'
            | '\u{1da88}'
    )
}

fn is_colon(character: char) -> bool {
    matches!(
        character,
        '\u{0703}'
            | '\u{0704}'
            | '\u{0705}'
            | '\u{0706}'
            | '\u{0707}'
            | '\u{0708}'
            | '\u{0709}'
            | '\u{1365}'
            | '\u{1366}'
            | '\u{1804}'
            | '\u{a6f4}'
            | '\u{fe13}'
            | '\u{fe55}'
            | '\u{12471}'
            | '\u{12472}'
            | '\u{1da8a}'
    )
}

fn is_semicolon(character: char) -> bool {
    matches!(
        character,
        '\u{061b}'
            | '\u{1364}'
            | '\u{204f}'
            | '\u{2e35}'
            | '\u{a6f6}'
            | '\u{fe14}'
            | '\u{fe54}'
            | '\u{1da89}'
    )
}

fn is_question_mark(character: char) -> bool {
    matches!(
        character,
        '\u{00bf}'
            | '\u{055e}'
            | '\u{061f}'
            | '\u{1367}'
            | '\u{1945}'
            | '\u{2cfa}'
            | '\u{2cfb}'
            | '\u{2e2e}'
            | '\u{2e54}'
            | '\u{a60f}'
            | '\u{a6f7}'
            | '\u{fe16}'
            | '\u{fe56}'
            | '\u{11143}'
            | '\u{1e95f}'
    )
}

fn is_exclamation_mark(character: char) -> bool {
    matches!(
        character,
        '\u{00a1}'
            | '\u{055c}'
            | '\u{07f9}'
            | '\u{1944}'
            | '\u{2e53}'
            | '\u{fe15}'
            | '\u{fe57}'
            | '\u{1e95e}'
    )
}

fn is_ellipsis(character: char) -> bool {
    matches!(character, '\u{1801}' | '\u{2026}' | '\u{fe19}')
}

#[derive(Clone, Copy)]
struct StructuralStackEntry {
    kind: StructuralKind,
    bracket_pair: Option<BracketPair>,
    symmetric_quote: Option<char>,
    occurrence_index: usize,
}

fn assign_structural_topology<E>(
    occurrences: &mut [SymbolOccurrence],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let mut stack = Vec::<StructuralStackEntry>::new();
    let mut next_pair_index = 0_usize;
    for occurrence_index in 0..occurrences.len() {
        if occurrence_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        let occurrence = occurrences[occurrence_index];
        let Some(structure) = occurrence.structure else {
            continue;
        };
        let role = match (structure.kind, structure.raw_role) {
            (StructuralKind::Quote, RawStructuralRole::Open) => StructuralRole::Open,
            (StructuralKind::Quote, RawStructuralRole::Close) => {
                if stack
                    .last()
                    .is_some_and(|entry| entry.kind == StructuralKind::Quote)
                {
                    let opened = stack.pop().expect("刚确认引号栈顶存在");
                    assign_structural_pair(
                        occurrences,
                        opened.occurrence_index,
                        occurrence_index,
                        &mut next_pair_index,
                    )?;
                    StructuralRole::Close
                } else {
                    StructuralRole::Close
                }
            }
            (StructuralKind::Quote, RawStructuralRole::Flexible) => {
                let closes_symmetric = stack.last().is_some_and(|entry| {
                    entry.kind == StructuralKind::Quote
                        && entry.symmetric_quote == Some(occurrence.character)
                });
                if closes_symmetric {
                    let opened = stack.pop().expect("刚确认对称引号栈顶存在");
                    assign_structural_pair(
                        occurrences,
                        opened.occurrence_index,
                        occurrence_index,
                        &mut next_pair_index,
                    )?;
                    StructuralRole::Close
                } else {
                    StructuralRole::Open
                }
            }
            (StructuralKind::Bracket, RawStructuralRole::Open) => StructuralRole::Open,
            (StructuralKind::Bracket, RawStructuralRole::Close) => {
                if stack.last().is_some_and(|entry| {
                    entry.kind == StructuralKind::Bracket
                        && entry.bracket_pair == structure.bracket_pair
                }) {
                    let opened = stack.pop().expect("刚确认括号栈顶存在");
                    assign_structural_pair(
                        occurrences,
                        opened.occurrence_index,
                        occurrence_index,
                        &mut next_pair_index,
                    )?;
                    StructuralRole::Close
                } else {
                    StructuralRole::Close
                }
            }
            (StructuralKind::Bracket, RawStructuralRole::Flexible) => continue,
        };
        occurrences[occurrence_index].structural_role = Some(role);
        if role == StructuralRole::Open {
            stack
                .try_reserve(1)
                .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
            stack.push(StructuralStackEntry {
                kind: structure.kind,
                bracket_pair: structure.bracket_pair,
                symmetric_quote: (structure.kind == StructuralKind::Quote
                    && structure.raw_role == RawStructuralRole::Flexible)
                    .then_some(occurrence.character),
                occurrence_index,
            });
        }
    }
    ensure_repair_running(ensure_running)?;
    Ok(())
}

fn assign_structural_pair(
    occurrences: &mut [SymbolOccurrence],
    open_index: usize,
    close_index: usize,
    next_pair_index: &mut usize,
) -> Result<(), TranslationSymbolRepairSkipReason> {
    let pair_index = *next_pair_index;
    *next_pair_index = next_pair_index
        .checked_add(1)
        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
    occurrences[open_index].structural_pair = Some(pair_index);
    occurrences[close_index].structural_pair = Some(pair_index);
    Ok(())
}

struct PlannedRepair {
    source_symbols: Vec<SymbolOccurrence>,
    plan: LanguageRepairPlan,
    replacement_count: usize,
}

fn build_plan<E>(
    source_symbols: Vec<SymbolOccurrence>,
    translation_symbols: Vec<SymbolOccurrence>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<PlannedRepair, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let forced = structurally_confirmed_matches(
        &source_symbols,
        &translation_symbols,
        forced_matches(&source_symbols, &translation_symbols, ensure_running)?,
        ensure_running,
    )?;
    let mut replacements = Vec::<LanguageCharacterReplacement>::new();
    replacements
        .try_reserve_exact(forced.len())
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    for (match_index, (source_index, translation_index)) in forced.into_iter().enumerate() {
        if match_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        let source = source_symbols[source_index];
        let translation = translation_symbols[translation_index];
        if source.character != translation.character {
            if replacements.last().is_some_and(|previous| {
                (previous.segment_index(), previous.byte_offset())
                    >= (translation.segment_index, translation.byte_offset)
            }) {
                return Err(TranslationSymbolRepairSkipReason::RepairInvariantViolation.into());
            }
            replacements.push(LanguageCharacterReplacement::new(
                translation.segment_index,
                translation.byte_offset,
                translation.character,
                source.character,
            ));
        }
    }
    ensure_repair_running(ensure_running)?;
    let replacement_count = replacements.len();
    Ok(PlannedRepair {
        source_symbols,
        plan: LanguageRepairPlan::replacing(replacements),
        replacement_count,
    })
}

fn symbols_are_compatible(source: &SymbolOccurrence, translation: &SymbolOccurrence) -> bool {
    if source.region != translation.region || source.family != translation.family {
        return false;
    }
    match source.family {
        SymbolFamily::Quote => source.structural_pair.is_some(),
        SymbolFamily::Bracket => source.structural_pair.is_some(),
        _ => true,
    }
}

fn source_topology_resolves_flexible_quote_pair(
    source_open: SymbolOccurrence,
    translation_open: SymbolOccurrence,
    translation_close: SymbolOccurrence,
) -> bool {
    if source_open.family != SymbolFamily::Quote
        || translation_open.region > translation_close.region
    {
        return false;
    }
    let (Some(open_structure), Some(close_structure)) =
        (translation_open.structure, translation_close.structure)
    else {
        return false;
    };
    open_structure.kind == StructuralKind::Quote
        && close_structure.kind == StructuralKind::Quote
        && matches!(
            open_structure.raw_role,
            RawStructuralRole::Open | RawStructuralRole::Flexible
        )
        && matches!(
            close_structure.raw_role,
            RawStructuralRole::Close | RawStructuralRole::Flexible
        )
        && (open_structure.raw_role == RawStructuralRole::Flexible
            || close_structure.raw_role == RawStructuralRole::Flexible)
}

#[derive(Clone, Copy, Debug, Default)]
struct StructuralPairEndpoints {
    open: Option<usize>,
    close: Option<usize>,
}

fn structurally_confirmed_matches<E>(
    source: &[SymbolOccurrence],
    translation: &[SymbolOccurrence],
    forced: Vec<(usize, usize)>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<(usize, usize)>, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let mut source_to_translation = Vec::new();
    source_to_translation
        .try_reserve_exact(source.len())
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    source_to_translation.resize(source.len(), None);
    for (match_index, &(source_index, translation_index)) in forced.iter().enumerate() {
        if match_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        source_to_translation[source_index] = Some(translation_index);
    }

    let pair_count = source
        .iter()
        .filter_map(|occurrence| occurrence.structural_pair)
        .max()
        .map_or(Ok(0), |maximum| {
            maximum
                .checked_add(1)
                .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)
        })?;
    let mut pairs = Vec::new();
    pairs
        .try_reserve_exact(pair_count)
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    pairs.resize(pair_count, StructuralPairEndpoints::default());
    for (source_index, occurrence) in source.iter().enumerate() {
        if source_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        let (Some(pair_index), Some(role)) =
            (occurrence.structural_pair, occurrence.structural_role)
        else {
            continue;
        };
        let endpoint = match role {
            StructuralRole::Open => &mut pairs[pair_index].open,
            StructuralRole::Close => &mut pairs[pair_index].close,
        };
        if endpoint.replace(source_index).is_some() {
            return Err(TranslationSymbolRepairSkipReason::RepairInvariantViolation.into());
        }
    }

    let pair_is_confirmed = |pair_index: usize| {
        let endpoints = pairs[pair_index];
        let (Some(source_open), Some(source_close)) = (endpoints.open, endpoints.close) else {
            return false;
        };
        let (Some(translation_open), Some(translation_close)) = (
            source_to_translation[source_open],
            source_to_translation[source_close],
        ) else {
            return false;
        };
        let existing_pair_is_confirmed = match (
            translation[translation_open].structural_pair,
            translation[translation_close].structural_pair,
        ) {
            (Some(open_pair), Some(close_pair)) => open_pair == close_pair,
            (None, None) => translation_open < translation_close,
            _ => false,
        };
        existing_pair_is_confirmed
            || source_topology_resolves_flexible_quote_pair(
                source[source_open],
                translation[translation_open],
                translation[translation_close],
            )
    };

    let mut confirmed = Vec::new();
    confirmed
        .try_reserve_exact(forced.len())
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    for (match_index, (source_index, translation_index)) in forced.into_iter().enumerate() {
        if match_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        let occurrence = source[source_index];
        if let Some(pair_index) = occurrence.structural_pair
            && !pair_is_confirmed(pair_index)
        {
            continue;
        }
        confirmed.push((source_index, translation_index));
    }
    ensure_repair_running(ensure_running)?;
    Ok(confirmed)
}

fn forced_matches<E>(
    source: &[SymbolOccurrence],
    translation: &[SymbolOccurrence],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<(usize, usize)>, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let rows = source
        .len()
        .checked_add(1)
        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
    let columns = translation
        .len()
        .checked_add(1)
        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
    let cells = rows
        .checked_mul(columns)
        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
    let mut prefix = zeroed_matrix(cells, ensure_running)?;
    let mut suffix = zeroed_matrix(cells, ensure_running)?;

    for source_index in 0..source.len() {
        ensure_repair_running(ensure_running)?;
        for translation_index in 0..translation.len() {
            if translation_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
                ensure_repair_running(ensure_running)?;
            }
            let above = prefix[matrix_index(source_index, translation_index + 1, columns)];
            let left = prefix[matrix_index(source_index + 1, translation_index, columns)];
            let mut best = above.max(left);
            if symbols_are_compatible(&source[source_index], &translation[translation_index]) {
                best = best.max(
                    prefix[matrix_index(source_index, translation_index, columns)]
                        .checked_add(1)
                        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?,
                );
            }
            prefix[matrix_index(source_index + 1, translation_index + 1, columns)] = best;
        }
    }

    for source_index in (0..source.len()).rev() {
        ensure_repair_running(ensure_running)?;
        for translation_index in (0..translation.len()).rev() {
            if translation_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
                ensure_repair_running(ensure_running)?;
            }
            let below = suffix[matrix_index(source_index + 1, translation_index, columns)];
            let right = suffix[matrix_index(source_index, translation_index + 1, columns)];
            let mut best = below.max(right);
            if symbols_are_compatible(&source[source_index], &translation[translation_index]) {
                best = best.max(
                    suffix[matrix_index(source_index + 1, translation_index + 1, columns)]
                        .checked_add(1)
                        .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?,
                );
            }
            suffix[matrix_index(source_index, translation_index, columns)] = best;
        }
    }

    let optimum = prefix[matrix_index(source.len(), translation.len(), columns)];
    let mut candidate_counts = zeroed_matrix(optimum, ensure_running)?;
    let mut unique_candidates = none_matrix(optimum, ensure_running)?;
    for source_index in 0..source.len() {
        ensure_repair_running(ensure_running)?;
        for translation_index in 0..translation.len() {
            if translation_index.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
                ensure_repair_running(ensure_running)?;
            }
            if !symbols_are_compatible(&source[source_index], &translation[translation_index]) {
                continue;
            }
            let prefix_length = prefix[matrix_index(source_index, translation_index, columns)];
            let suffix_length =
                suffix[matrix_index(source_index + 1, translation_index + 1, columns)];
            let through_edge = prefix_length
                .checked_add(1)
                .and_then(|length| length.checked_add(suffix_length))
                .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
            if through_edge != optimum {
                continue;
            }
            candidate_counts[prefix_length] = candidate_counts[prefix_length]
                .checked_add(1)
                .ok_or(TranslationSymbolRepairSkipReason::SizeOverflow)?;
            unique_candidates[prefix_length] = if candidate_counts[prefix_length] == 1 {
                Some((source_index, translation_index))
            } else {
                None
            };
        }
    }

    let mut forced = Vec::new();
    forced
        .try_reserve_exact(optimum)
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    for (rank, (count, candidate)) in candidate_counts
        .into_iter()
        .zip(unique_candidates)
        .enumerate()
    {
        if rank.is_multiple_of(CANCELLATION_CHECK_INTERVAL) {
            ensure_repair_running(ensure_running)?;
        }
        if count == 1 {
            let Some(candidate) = candidate else {
                return Err(TranslationSymbolRepairSkipReason::RepairInvariantViolation.into());
            };
            forced.push(candidate);
        }
    }
    ensure_repair_running(ensure_running)?;
    Ok(forced)
}

fn zeroed_matrix<E>(
    length: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<usize>, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let mut matrix = Vec::new();
    matrix
        .try_reserve_exact(length)
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    while matrix.len() < length {
        ensure_repair_running(ensure_running)?;
        let next = matrix
            .len()
            .checked_add(MATRIX_INITIALIZATION_CHUNK)
            .map_or(length, |next| next.min(length));
        matrix.resize(next, 0);
    }
    Ok(matrix)
}

fn none_matrix<E>(
    length: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<Option<(usize, usize)>>, RepairPlanningFailure<E>> {
    ensure_repair_running(ensure_running)?;
    let mut matrix = Vec::new();
    matrix
        .try_reserve_exact(length)
        .map_err(|_| TranslationSymbolRepairSkipReason::ResourceExhausted)?;
    while matrix.len() < length {
        ensure_repair_running(ensure_running)?;
        let next = matrix
            .len()
            .checked_add(MATRIX_INITIALIZATION_CHUNK)
            .map_or(length, |next| next.min(length));
        matrix.resize(next, None);
    }
    Ok(matrix)
}

fn matrix_index(row: usize, column: usize, columns: usize) -> usize {
    row * columns + column
}

#[cfg(test)]
mod tests {
    use super::*;

    fn natural(text: &str) -> LanguageText {
        LanguageText::natural(text)
    }

    fn repair(source: &LanguageText, translation: &LanguageText) -> LanguageText {
        match TranslationSymbolRepairer::plan_repair(source, translation) {
            TranslationSymbolRepairOutcome::Unchanged => translation.clone(),
            TranslationSymbolRepairOutcome::Repaired { plan, .. } => {
                translation.apply_repair(&plan).expect("修复计划必须有效")
            }
            TranslationSymbolRepairOutcome::Skipped { reason } => {
                panic!("测试输入不应跳过：{reason:?}")
            }
        }
    }

    #[test]
    fn restores_source_commas_without_copying_source_whitespace() {
        let repaired = repair(
            &natural("General, Misc, Audio, Toggle"),
            &natural("常规、杂项、声音、开关"),
        );

        assert_eq!(repaired, natural("常规,杂项,声音,开关"));
    }

    #[test]
    fn dense_matching_propagates_cancellation() {
        let source = natural(&",".repeat(1_024));
        let translation = natural(&"、".repeat(1_024));
        let mut checks = 0_usize;

        let result =
            TranslationSymbolRepairer::plan_repair_with_cancellation(&source, &translation, || {
                checks += 1;
                if checks >= 32 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });

        assert!(matches!(result, Err("cancelled")));
    }

    #[test]
    fn repairs_outer_quotes_but_keeps_incompatible_ellipsis() {
        let repaired = repair(
            &natural("\"Report to the Guardian...\""),
            &natural("“向守卫报告……”"),
        );

        assert_eq!(repaired, natural("\"向守卫报告……\""));
    }

    #[test]
    fn repairs_only_the_damaged_quote_endpoint() {
        assert_eq!(
            repair(&natural("「Hello」"), &natural("「你好“")),
            natural("「你好」")
        );
        assert_eq!(
            repair(&natural("「Hello」"), &natural("”你好」")),
            natural("「你好」")
        );
    }

    #[test]
    fn repairs_reversed_and_symmetric_quote_pairs() {
        assert_eq!(
            repair(&natural("「Hello」"), &natural("”你好“")),
            natural("「你好」")
        );
        assert_eq!(
            repair(&natural("\"Hello\""), &natural("“你好”")),
            natural("\"你好\"")
        );
    }

    #[test]
    fn source_topology_assigns_nested_roles_to_symmetric_quotes() {
        assert_eq!(
            repair(
                &natural("「これは『勇者』だ」"),
                &natural("\"This is \"the hero\".\""),
            ),
            natural("「This is 『the hero』.」")
        );
    }

    #[test]
    fn repairs_one_sided_and_reversed_bracket_pairs() {
        assert_eq!(
            repair(&natural("(Hello)"), &natural("(你好】")),
            natural("(你好)")
        );
        assert_eq!(
            repair(&natural("(Hello)"), &natural("）你好（")),
            natural("(你好)")
        );
    }

    #[test]
    fn preserves_nested_and_sibling_quote_topology() {
        assert_eq!(
            repair(
                &natural("「This is 『nested』.」「Again」"),
                &natural("“这是‘嵌套’。”“再次”"),
            ),
            natural("「这是『嵌套』.」「再次」")
        );
    }

    #[test]
    fn does_not_rewrite_sibling_quotes_into_nested_quotes() {
        assert_eq!(
            repair(
                &natural("「This is 『nested』」, now."),
                &natural("“这是”“同级”、现在。"),
            ),
            natural("“这是”“同级”,现在.")
        );
    }

    #[test]
    fn does_not_rewrite_nested_quotes_into_sibling_quotes() {
        assert_eq!(
            repair(
                &natural("「First」「Second」, now."),
                &natural("“第一‘第二’”、现在。"),
            ),
            natural("“第一‘第二’”,现在.")
        );
    }

    #[test]
    fn does_not_rewrite_nested_and_sibling_bracket_topology() {
        assert_eq!(
            repair(
                &natural("([Nested]), now."),
                &natural("（同级）（括号）、现在。"),
            ),
            natural("（同级）（括号）,现在.")
        );
        assert_eq!(
            repair(
                &natural("(First)(Second), now."),
                &natural("（外层【内层】）、现在。"),
            ),
            natural("（外层【内层】）,现在.")
        );
    }

    #[test]
    fn ambiguous_extra_quotes_do_not_block_a_unique_comma() {
        assert_eq!(
            repair(&natural("「A」, B"), &natural("“甲”“乙”、丙")),
            natural("“甲”“乙”,丙")
        );
    }

    #[test]
    fn missing_quote_endpoint_is_not_inserted() {
        assert_eq!(
            repair(&natural("「A」, B"), &natural("“甲、乙")),
            natural("“甲,乙")
        );
    }

    #[test]
    fn opaque_boundaries_are_matched_by_region_and_never_changed() {
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("Open ".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText(", now.".to_owned()),
        ]);
        let translation = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("打开 ".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("、现在。".to_owned()),
        ]);

        assert_eq!(
            repair(&source, &translation),
            LanguageText::new(vec![
                LanguageTextSegment::NaturalText("打开 ".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText(",现在.".to_owned()),
            ])
        );
    }

    #[test]
    fn quote_topology_can_cross_opaque_boundaries() {
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("「Hello ".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText(" world」".to_owned()),
        ]);
        let translation = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("“你好 ".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText(" 世界”".to_owned()),
        ]);

        assert_eq!(
            repair(&source, &translation),
            LanguageText::new(vec![
                LanguageTextSegment::NaturalText("「你好 ".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText(" 世界」".to_owned()),
            ])
        );
    }

    #[test]
    fn opaque_boundary_mismatch_skips_the_unit() {
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("A,".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("B".to_owned()),
        ]);
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&source, &natural("甲、乙")),
            TranslationSymbolRepairOutcome::Skipped {
                reason: TranslationSymbolRepairSkipReason::OpaqueBoundaryMismatch,
            }
        );
    }

    #[test]
    fn intraword_apostrophes_are_not_used_as_quote_topology() {
        assert_eq!(
            repair(&natural("Don't stop."), &natural("Don’t 停止。")),
            natural("Don't 停止.")
        );
        assert_eq!(
            repair(&natural("Don't stop."), &natural("Don＇t 停止。")),
            natural("Don't 停止.")
        );
    }

    #[test]
    fn cjk_adjacent_quotes_are_not_treated_as_word_apostrophes() {
        assert_eq!(
            repair(
                &natural("Don’t, won’t."),
                &natural("他说'你好、世界'然后走了。"),
            ),
            natural("他说'你好,世界'然后走了.")
        );
    }

    #[test]
    fn mixed_marks_repair_in_order_and_ellipsis_does_not_match_periods() {
        assert_eq!(
            repair(&natural("Ready?!"), &natural("准备？！")),
            natural("准备?!")
        );
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&natural("Wait..."), &natural("等等……")),
            TranslationSymbolRepairOutcome::Unchanged
        );
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&natural("Really?"), &natural("真的⁇")),
            TranslationSymbolRepairOutcome::Unchanged
        );
    }

    #[test]
    fn semantic_punctuation_families_are_not_limited_to_cjk_forms() {
        assert_eq!(
            repair(
                &natural("A,B.C:D;E?F!G…"),
                &natural("甲،乙۔丙᠄丁؛戊؟己߹庚᠁"),
            ),
            natural("甲,乙.丙:丁;戊?己!庚…")
        );
    }

    #[test]
    fn ambiguous_repeated_marks_do_not_block_an_independent_period() {
        assert_eq!(
            repair(&natural("A, B."), &natural("甲、乙、丙。")),
            natural("甲、乙、丙.")
        );
    }

    #[test]
    fn missing_and_added_symbols_are_preserved_while_independent_marks_repair() {
        assert_eq!(
            repair(&natural("A, B."), &natural("甲乙。")),
            natural("甲乙.")
        );
        assert_eq!(repair(&natural("A."), &natural("甲！。")), natural("甲！."));
    }

    #[test]
    fn repairs_compatible_fullwidth_ascii_symbols_and_keeps_slashes_distinct() {
        assert_eq!(
            repair(&natural("A+B=C/2\\X"), &natural("甲＋乙＝丙／2＼X")),
            natural("甲+乙=丙/2\\X")
        );
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&natural("A/B"), &natural("甲＼乙")),
            TranslationSymbolRepairOutcome::Unchanged
        );
    }

    #[test]
    fn compatibility_forms_use_normalized_quote_and_bracket_structure() {
        assert_eq!(repair(&natural("(A)"), &natural("︵甲︶")), natural("(甲)"));
        assert_eq!(
            repair(&natural("\"A\""), &natural("＂甲＂")),
            natural("\"甲\"")
        );
    }

    #[test]
    fn non_ascii_currency_math_and_emoji_are_outside_the_candidate_set() {
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&natural("A€B×C🙂"), &natural("甲$乙+丙!"),),
            TranslationSymbolRepairOutcome::Unchanged
        );
    }

    #[test]
    fn accepted_repairs_are_idempotent() {
        let source = natural("(A), [B]!");
        let translation = natural("（甲）、【乙】！");
        let repaired = repair(&source, &translation);
        assert_eq!(repaired, natural("(甲),[乙]!"));
        assert_eq!(
            TranslationSymbolRepairer::plan_repair(&source, &repaired),
            TranslationSymbolRepairOutcome::Unchanged
        );
    }

    #[test]
    fn dense_punctuation_sequence_repairs_without_an_artificial_capacity_limit() {
        const SYMBOLS: usize = 1_024;
        let source = natural(&",".repeat(SYMBOLS));
        let translation = natural(&"、".repeat(SYMBOLS));
        let repaired = repair(&source, &translation);

        assert_eq!(repaired, source);
    }

    #[test]
    fn forced_match_detection_agrees_with_exhaustive_short_lcs_enumeration() {
        let classifier = SymbolClassifier::new();
        let source_sequences = short_sequences(&[',', '.', '?'], 3);
        let translation_sequences = short_sequences(&['、', '。', '？'], 3);
        for source_text in &source_sequences {
            for translation_text in &translation_sequences {
                let mut ensure_running = || Ok::<_, Infallible>(());
                let source =
                    collect_symbols(&natural(source_text), &classifier, &mut ensure_running)
                        .expect("测试源文有效")
                        .occurrences;
                let translation =
                    collect_symbols(&natural(translation_text), &classifier, &mut ensure_running)
                        .expect("测试译文有效")
                        .occurrences;
                let actual = forced_matches(&source, &translation, &mut ensure_running)
                    .expect("短序列可分配");
                let alignments = enumerate_optimal_alignments(&source, &translation);
                let expected = intersection(&alignments);
                assert_eq!(
                    actual, expected,
                    "源符号 {source_text:?}，译文符号 {translation_text:?}"
                );
            }
        }
    }

    fn short_sequences(alphabet: &[char], maximum_length: usize) -> Vec<String> {
        fn append(
            sequences: &mut Vec<String>,
            current: &mut String,
            alphabet: &[char],
            remaining: usize,
        ) {
            sequences.push(current.clone());
            if remaining == 0 {
                return;
            }
            for character in alphabet {
                current.push(*character);
                append(sequences, current, alphabet, remaining - 1);
                current.pop();
            }
        }

        let mut sequences = Vec::new();
        append(&mut sequences, &mut String::new(), alphabet, maximum_length);
        sequences
    }

    fn enumerate_optimal_alignments(
        source: &[SymbolOccurrence],
        translation: &[SymbolOccurrence],
    ) -> Vec<Vec<(usize, usize)>> {
        fn visit(
            source: &[SymbolOccurrence],
            translation: &[SymbolOccurrence],
            source_index: usize,
            translation_index: usize,
            current: &mut Vec<(usize, usize)>,
            complete: &mut Vec<Vec<(usize, usize)>>,
        ) {
            if source_index == source.len() || translation_index == translation.len() {
                complete.push(current.clone());
                return;
            }
            visit(
                source,
                translation,
                source_index + 1,
                translation_index,
                current,
                complete,
            );
            visit(
                source,
                translation,
                source_index,
                translation_index + 1,
                current,
                complete,
            );
            if symbols_are_compatible(&source[source_index], &translation[translation_index]) {
                current.push((source_index, translation_index));
                visit(
                    source,
                    translation,
                    source_index + 1,
                    translation_index + 1,
                    current,
                    complete,
                );
                current.pop();
            }
        }

        let mut all = Vec::new();
        visit(source, translation, 0, 0, &mut Vec::new(), &mut all);
        let optimum = all.iter().map(Vec::len).max().unwrap_or(0);
        all.retain(|alignment| alignment.len() == optimum);
        all.sort();
        all.dedup();
        all
    }

    fn intersection(alignments: &[Vec<(usize, usize)>]) -> Vec<(usize, usize)> {
        let Some(first) = alignments.first() else {
            return Vec::new();
        };
        first
            .iter()
            .copied()
            .filter(|candidate| {
                alignments
                    .iter()
                    .all(|alignment| alignment.contains(candidate))
            })
            .collect()
    }
}
