//! 从 RPG Maker 文本资产规划并生成写回候选。

pub(crate) mod layout;

pub(crate) use layout::ConservativeRpgMakerWriteBackTextLayouter;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{RpgMakerWriteBack, RpgMakerWriteBackSummary, WriteBackProgressPhase};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, PlaceholderRuleSource, RpgMakerComputeFailure, RpgMakerIssue,
    RpgMakerLogicalUnitLocator, RpgMakerManualLayoutRegion, RpgMakerUnitLocator,
    RpgMakerWriteBackChoicesPlanViolation, RpgMakerWriteBackDialoguePlanViolation,
    RpgMakerWriteBackMutationPlanViolation, RpgMakerWriteBackPlanningProblem, StateEffect,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::language::{LanguageText, LanguageTextSegment};
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueWriteRecipe, DirectTextPart, DirectTextRecipe, LogicalTextLocation,
    MutationClaim, MutationClaimIndex, MutationClaimSet, MutationResourceLock,
    TextProjectionRecipe, TextUnitContent, TextUnitContentStructureError, TextUnitContentView,
    TextUnitRole, mutation_claims_for_group, validate_text_unit_content_structure,
};
use crate::rpg_maker::project::{MaxFullwidthChars, OpenedProject, RpgMakerWriteBackLayoutProfile};
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
};
use crate::rpg_maker::translate::pipeline::{
    AppliedPlaceholder, TranslationPlaceholderProjectionFailure, TranslationPlanningFailureReason,
    placeholder_projection_diagnostic, placeholder_protection_diagnostic,
};
use crate::rpg_maker::translate::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, ProtectedText,
};
use crate::rpg_maker::translate::planner::{
    placeholder_projection_planning_failure, placeholder_protection_planning_failure,
};
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::translation::symbol_repair::{
    TranslationSymbolRepairOutcome, TranslationSymbolRepairer,
};

const MAX_PLANNING_PROGRESS_UPDATES: u64 = 1_024;

/// 同一数据库读快照中解析、编译完成的写回符号修复资源。
#[derive(Clone)]
pub(crate) struct RpgMakerWriteBackSymbolRepairContext {
    engine: RpgMakerEngine,
    placeholder_service: Pcre2PlaceholderService,
    placeholder_rules: CompiledPlaceholderRules,
    placeholder_rules_json: String,
}

impl RpgMakerWriteBackSymbolRepairContext {
    pub(crate) fn new(
        engine: RpgMakerEngine,
        placeholder_service: Pcre2PlaceholderService,
        placeholder_rules: CompiledPlaceholderRules,
        placeholder_rules_json: String,
    ) -> Self {
        Self {
            engine,
            placeholder_service,
            placeholder_rules,
            placeholder_rules_json,
        }
    }
}

impl fmt::Debug for RpgMakerWriteBackSymbolRepairContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerWriteBackSymbolRepairContext")
            .field("engine", &self.engine)
            .field("placeholder_rules", &self.placeholder_rules)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RpgMakerWriteBackSymbolRepairContext {
    fn eq(&self, other: &Self) -> bool {
        self.engine == other.engine && self.placeholder_rules_json == other.placeholder_rules_json
    }
}

impl Eq for RpgMakerWriteBackSymbolRepairContext {}

/// 一个可独立拥有译文、验收并原子写回的语义文本单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackUnit {
    role: TextUnitRole,
    source_content: TextUnitContent,
    translation_content: Option<TextUnitContent>,
    manual: bool,
}

impl RpgMakerWriteBackUnit {
    pub(crate) fn new(
        role: TextUnitRole,
        source_content: TextUnitContent,
        translation_content: Option<TextUnitContent>,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        Self::new_with_origin(role, source_content, translation_content, false)
    }

    pub(crate) fn new_manual(
        role: TextUnitRole,
        source_content: TextUnitContent,
        translation_content: TextUnitContent,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        Self::new_with_origin(role, source_content, Some(translation_content), true)
    }

    fn new_with_origin(
        role: TextUnitRole,
        source_content: TextUnitContent,
        translation_content: Option<TextUnitContent>,
        manual: bool,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        validate_content_presence(&role, &source_content, "原文")?;
        if source_content.is_blank() {
            return Err(RpgMakerWriteBackSnapshotError::BlankSourceContent { role: role.clone() });
        }
        if let Some(translation) = &translation_content {
            validate_content_presence(&role, translation, "译文")?;
            if !manual && translation.is_blank() {
                return Err(RpgMakerWriteBackSnapshotError::BlankTranslationContent {
                    role: role.clone(),
                });
            }
        }
        Ok(Self {
            role,
            source_content,
            translation_content,
            manual,
        })
    }

    fn effective_content(&self) -> &TextUnitContent {
        self.translation_content
            .as_ref()
            .unwrap_or(&self.source_content)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SymbolRepairStatistics {
    attempted_units: usize,
    repaired_units: usize,
    skipped_units: usize,
    replacements: usize,
}

enum RepairedText {
    Unchanged,
    Repaired {
        text: String,
        replacements: usize,
    },
    RepairedLines {
        lines: Vec<String>,
        replacements: usize,
    },
    Skipped,
}

#[derive(Clone, Copy, Debug, Default)]
struct SymbolRepairAllocation {
    #[cfg(test)]
    fail_reservations: bool,
}

impl SymbolRepairAllocation {
    fn try_reserve_string(self, output: &mut String, additional: usize) -> bool {
        #[cfg(test)]
        if self.fail_reservations && additional != 0 {
            return false;
        }
        output.try_reserve_exact(additional).is_ok()
    }

    fn try_reserve_vec<T>(self, output: &mut Vec<T>, additional: usize) -> bool {
        #[cfg(test)]
        if self.fail_reservations && additional != 0 {
            return false;
        }
        output.try_reserve_exact(additional).is_ok()
    }

    #[cfg(test)]
    const fn failing() -> Self {
        Self {
            fail_reservations: true,
        }
    }
}

#[cfg(test)]
fn repair_unit_translation_symbols(
    unit: &mut RpgMakerWriteBackUnit,
    kind: TextGroupKind,
    context: &RpgMakerWriteBackSymbolRepairContext,
) -> Result<SymbolRepairStatistics, TranslationPlanningFailureReason> {
    match repair_unit_translation_symbols_with_cancellation(
        unit,
        kind,
        context,
        &CooperativeCancellation::default(),
    )? {
        OperationCompletion::Completed(statistics) => Ok(statistics),
        OperationCompletion::Cancelled => unreachable!("测试取消状态必须保持运行"),
    }
}

fn repair_unit_translation_symbols_with_cancellation(
    unit: &mut RpgMakerWriteBackUnit,
    kind: TextGroupKind,
    context: &RpgMakerWriteBackSymbolRepairContext,
    cancellation: &CooperativeCancellation,
) -> Result<OperationCompletion<SymbolRepairStatistics>, TranslationPlanningFailureReason> {
    repair_unit_translation_symbols_with_allocation(
        unit,
        kind,
        context,
        cancellation,
        SymbolRepairAllocation::default(),
    )
}

fn repair_unit_translation_symbols_with_allocation(
    unit: &mut RpgMakerWriteBackUnit,
    kind: TextGroupKind,
    context: &RpgMakerWriteBackSymbolRepairContext,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> Result<OperationCompletion<SymbolRepairStatistics>, TranslationPlanningFailureReason> {
    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }
    if unit.manual {
        return Ok(OperationCompletion::Completed(
            SymbolRepairStatistics::default(),
        ));
    }
    let Some(translation) = unit.translation_content.as_ref() else {
        return Ok(OperationCompletion::Completed(
            SymbolRepairStatistics::default(),
        ));
    };
    let mut statistics = SymbolRepairStatistics {
        attempted_units: 1,
        ..SymbolRepairStatistics::default()
    };

    let repaired = match (&unit.source_content, translation) {
        (TextUnitContent::Value(source), TextUnitContent::Value(translation)) => {
            match repair_text_symbols_with_cancellation(
                source,
                translation,
                kind,
                context,
                false,
                cancellation,
                allocation,
            )? {
                OperationCompletion::Completed(repaired) => repaired,
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            }
        }
        (TextUnitContent::Lines(source), TextUnitContent::Lines(translation))
            if matches!(
                unit.role,
                TextUnitRole::Choices | TextUnitRole::ScrollingText
            ) =>
        {
            match repair_aligned_line_symbols_with_cancellation(
                source,
                translation,
                kind,
                context,
                cancellation,
                allocation,
            )? {
                OperationCompletion::Completed(repaired) => repaired,
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            }
        }
        (TextUnitContent::Lines(source), TextUnitContent::Lines(translation)) => {
            let source = match join_symbol_repair_lines(source, cancellation, allocation) {
                OperationCompletion::Completed(Some(text)) => text,
                OperationCompletion::Completed(None) => {
                    return Ok(OperationCompletion::Completed(skipped_symbol_repair(
                        statistics,
                    )));
                }
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            };
            let translation = match join_symbol_repair_lines(translation, cancellation, allocation)
            {
                OperationCompletion::Completed(Some(text)) => text,
                OperationCompletion::Completed(None) => {
                    return Ok(OperationCompletion::Completed(skipped_symbol_repair(
                        statistics,
                    )));
                }
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            };
            match repair_text_symbols_with_cancellation(
                &source,
                &translation,
                kind,
                context,
                true,
                cancellation,
                allocation,
            )? {
                OperationCompletion::Completed(repaired) => repaired,
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            }
        }
        _ => unreachable!("受信写回单元的原文与译文结构必须一致"),
    };

    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }
    Ok(OperationCompletion::Completed(match repaired {
        RepairedText::Unchanged => statistics,
        RepairedText::Skipped => skipped_symbol_repair(statistics),
        RepairedText::Repaired { text, replacements } => {
            let replacement = match &unit.translation_content {
                Some(TextUnitContent::Value(_)) => TextUnitContent::Value(text),
                Some(TextUnitContent::Lines(lines)) => {
                    let repaired_lines = match split_symbol_repair_lines(
                        &text,
                        lines.len(),
                        cancellation,
                        allocation,
                    ) {
                        OperationCompletion::Completed(Some(lines)) => lines,
                        OperationCompletion::Completed(None) => {
                            return Ok(OperationCompletion::Completed(skipped_symbol_repair(
                                statistics,
                            )));
                        }
                        OperationCompletion::Cancelled => {
                            return Ok(OperationCompletion::Cancelled);
                        }
                    };
                    TextUnitContent::Lines(repaired_lines)
                }
                None => unreachable!("已确认写回单元存在译文"),
            };
            unit.translation_content = Some(replacement);
            statistics.repaired_units = 1;
            statistics.replacements = replacements;
            statistics
        }
        RepairedText::RepairedLines {
            lines,
            replacements,
        } => {
            debug_assert!(matches!(
                unit.translation_content,
                Some(TextUnitContent::Lines(_))
            ));
            unit.translation_content = Some(TextUnitContent::Lines(lines));
            statistics.repaired_units = 1;
            statistics.replacements = replacements;
            statistics
        }
    }))
}

fn skipped_symbol_repair(mut statistics: SymbolRepairStatistics) -> SymbolRepairStatistics {
    statistics.skipped_units = 1;
    statistics
}

fn repair_text_symbols_with_cancellation(
    source: &str,
    translation: &str,
    kind: TextGroupKind,
    context: &RpgMakerWriteBackSymbolRepairContext,
    has_line_slot_boundaries: bool,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> Result<OperationCompletion<RepairedText>, TranslationPlanningFailureReason> {
    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }
    let source_line_boundaries = if has_line_slot_boundaries {
        match line_separator_offsets(source, cancellation, allocation) {
            OperationCompletion::Completed(Some(offsets)) => offsets,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        }
    } else {
        Vec::new()
    };
    let translation_line_boundaries = if has_line_slot_boundaries {
        match line_separator_offsets(translation, cancellation, allocation) {
            OperationCompletion::Completed(Some(offsets)) => offsets,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        }
    } else {
        Vec::new()
    };
    let source = match context
        .placeholder_service
        .protect_with_line_boundaries_with_cancellation(
            context.engine,
            kind,
            source,
            &source_line_boundaries,
            &context.placeholder_rules,
            || ensure_symbol_repair_running(cancellation),
        ) {
        Ok(Ok(source)) => source,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProtection {
                failure: placeholder_protection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let translation_view = match context
        .placeholder_service
        .protect_with_line_boundaries_with_cancellation(
            context.engine,
            kind,
            translation,
            &translation_line_boundaries,
            &context.placeholder_rules,
            || ensure_symbol_repair_running(cancellation),
        ) {
        Ok(Ok(translation)) => translation,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProtection {
                failure: placeholder_protection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    validate_placeholder_bindings(source.placeholders(), translation_view.placeholders())
        .map_err(|failure| TranslationPlanningFailureReason::PlaceholderProjection { failure })?;
    let source = match source
        .language_text_with_cancellation(|| ensure_symbol_repair_running(cancellation))
    {
        Ok(Ok(source)) => source,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProjection {
                failure: placeholder_projection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let translation_text = match translation_view
        .language_text_with_cancellation(|| ensure_symbol_repair_running(cancellation))
    {
        Ok(Ok(translation)) => translation,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProjection {
                failure: placeholder_projection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let (plan, replacements) = match TranslationSymbolRepairer::plan_repair_with_cancellation(
        &source,
        &translation_text,
        || ensure_symbol_repair_running(cancellation),
    ) {
        Ok(TranslationSymbolRepairOutcome::Unchanged) => {
            return Ok(OperationCompletion::Completed(RepairedText::Unchanged));
        }
        Ok(TranslationSymbolRepairOutcome::Repaired {
            plan,
            replacement_count,
        }) => (plan, replacement_count),
        Ok(TranslationSymbolRepairOutcome::Skipped { .. }) => {
            return Ok(OperationCompletion::Completed(RepairedText::Skipped));
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let repaired = match translation_text
        .apply_repair_with_cancellation(&plan, || ensure_symbol_repair_running(cancellation))
    {
        Ok(Ok(repaired)) => repaired,
        Ok(Err(_)) => {
            return Ok(OperationCompletion::Completed(RepairedText::Skipped));
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let text =
        match rebuild_protected_translation(&translation_view, &repaired, cancellation, allocation)
        {
            OperationCompletion::Completed(Some(text)) => text,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        };
    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }
    Ok(OperationCompletion::Completed(RepairedText::Repaired {
        text,
        replacements,
    }))
}

fn repair_aligned_line_symbols_with_cancellation(
    source_lines: &[String],
    translation_lines: &[String],
    kind: TextGroupKind,
    context: &RpgMakerWriteBackSymbolRepairContext,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> Result<OperationCompletion<RepairedText>, TranslationPlanningFailureReason> {
    let source_text = match join_symbol_repair_lines(source_lines, cancellation, allocation) {
        OperationCompletion::Completed(Some(text)) => text,
        OperationCompletion::Completed(None) => {
            return Ok(OperationCompletion::Completed(RepairedText::Skipped));
        }
        OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
    };
    let translation_text =
        match join_symbol_repair_lines(translation_lines, cancellation, allocation) {
            OperationCompletion::Completed(Some(text)) => text,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        };
    let source_boundaries = match line_separator_offsets(&source_text, cancellation, allocation) {
        OperationCompletion::Completed(Some(offsets)) => offsets,
        OperationCompletion::Completed(None) => {
            return Ok(OperationCompletion::Completed(RepairedText::Skipped));
        }
        OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
    };
    let translation_boundaries =
        match line_separator_offsets(&translation_text, cancellation, allocation) {
            OperationCompletion::Completed(Some(offsets)) => offsets,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        };
    let source = match context
        .placeholder_service
        .protect_with_line_boundaries_with_cancellation(
            context.engine,
            kind,
            &source_text,
            &source_boundaries,
            &context.placeholder_rules,
            || ensure_symbol_repair_running(cancellation),
        ) {
        Ok(Ok(source)) => source,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProtection {
                failure: placeholder_protection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    let translation = match context
        .placeholder_service
        .protect_with_line_boundaries_with_cancellation(
            context.engine,
            kind,
            &translation_text,
            &translation_boundaries,
            &context.placeholder_rules,
            || ensure_symbol_repair_running(cancellation),
        ) {
        Ok(Ok(translation)) => translation,
        Ok(Err(source)) => {
            return Err(TranslationPlanningFailureReason::PlaceholderProtection {
                failure: placeholder_protection_planning_failure(source),
            });
        }
        Err(()) => return Ok(OperationCompletion::Cancelled),
    };
    validate_placeholder_bindings(source.placeholders(), translation.placeholders())
        .map_err(|failure| TranslationPlanningFailureReason::PlaceholderProjection { failure })?;
    let mut repaired_lines =
        match clone_symbol_repair_lines(translation_lines, cancellation, allocation) {
            OperationCompletion::Completed(Some(lines)) => lines,
            OperationCompletion::Completed(None) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        };
    let mut replacements = 0_usize;
    for ((source_line, translation_line), repaired_line) in source_lines
        .iter()
        .zip(translation_lines)
        .zip(&mut repaired_lines)
    {
        if cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        match repair_text_symbols_with_cancellation(
            source_line,
            translation_line,
            kind,
            context,
            false,
            cancellation,
            allocation,
        )? {
            OperationCompletion::Completed(RepairedText::Unchanged) => {}
            OperationCompletion::Completed(RepairedText::Repaired {
                text,
                replacements: line_replacements,
            }) => {
                *repaired_line = text;
                let Some(total) = replacements.checked_add(line_replacements) else {
                    return Ok(OperationCompletion::Completed(RepairedText::Skipped));
                };
                replacements = total;
            }
            OperationCompletion::Completed(RepairedText::Skipped) => {
                return Ok(OperationCompletion::Completed(RepairedText::Skipped));
            }
            OperationCompletion::Completed(RepairedText::RepairedLines { .. }) => {
                unreachable!("单行符号修复不能返回行序列")
            }
            OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
        }
    }
    if replacements == 0 {
        Ok(OperationCompletion::Completed(RepairedText::Unchanged))
    } else {
        Ok(OperationCompletion::Completed(
            RepairedText::RepairedLines {
                lines: repaired_lines,
                replacements,
            },
        ))
    }
}

fn ensure_symbol_repair_running(cancellation: &CooperativeCancellation) -> Result<(), ()> {
    if cancellation.is_requested() {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_placeholder_bindings(
    expected: &[AppliedPlaceholder],
    actual: &[AppliedPlaceholder],
) -> Result<(), TranslationPlaceholderProjectionFailure> {
    if expected.len() != actual.len() {
        return Err(
            TranslationPlaceholderProjectionFailure::ChangedSegmentCount {
                expected: expected.len(),
                actual: actual.len(),
            },
        );
    }
    for (segment_index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
        if expected == actual {
            continue;
        }
        if expected.segment() != actual.segment() {
            return Err(
                TranslationPlaceholderProjectionFailure::ChangedSegmentKind { segment_index },
            );
        }
        if expected.token() != actual.token() {
            return Err(TranslationPlaceholderProjectionFailure::ChangedTokenOrder {
                position: segment_index,
                expected_token: expected.token().to_owned(),
                actual_token: actual.token().to_owned(),
            });
        }
        return Err(TranslationPlaceholderProjectionFailure::MissingOrderedToken { segment_index });
    }
    Ok(())
}

fn clone_symbol_repair_lines(
    lines: &[String],
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> OperationCompletion<Option<Vec<String>>> {
    if cancellation.is_requested() {
        return OperationCompletion::Cancelled;
    }
    let mut cloned = Vec::new();
    if !allocation.try_reserve_vec(&mut cloned, lines.len()) {
        return OperationCompletion::Completed(None);
    }
    for line in lines {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        let mut cloned_line = String::new();
        if !allocation.try_reserve_string(&mut cloned_line, line.len()) {
            return OperationCompletion::Completed(None);
        }
        cloned_line.push_str(line);
        cloned.push(cloned_line);
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(cloned))
    }
}

fn join_symbol_repair_lines(
    lines: &[String],
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> OperationCompletion<Option<String>> {
    if cancellation.is_requested() {
        return OperationCompletion::Cancelled;
    }
    let total = match checked_joined_symbol_repair_length(
        lines.iter().map(String::len),
        lines.len().saturating_sub(1),
        cancellation,
    ) {
        OperationCompletion::Completed(Some(total)) => total,
        OperationCompletion::Completed(None) => return OperationCompletion::Completed(None),
        OperationCompletion::Cancelled => return OperationCompletion::Cancelled,
    };
    let mut joined = String::new();
    if !allocation.try_reserve_string(&mut joined, total) {
        return OperationCompletion::Completed(None);
    }
    for (index, line) in lines.iter().enumerate() {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        if index != 0 {
            joined.push('\n');
        }
        joined.push_str(line);
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(joined))
    }
}

fn checked_joined_symbol_repair_length(
    lengths: impl IntoIterator<Item = usize>,
    separator_bytes: usize,
    cancellation: &CooperativeCancellation,
) -> OperationCompletion<Option<usize>> {
    let mut total = separator_bytes;
    for length in lengths {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        let Some(next) = total.checked_add(length) else {
            return OperationCompletion::Completed(None);
        };
        total = next;
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(total))
    }
}

fn split_symbol_repair_lines(
    text: &str,
    expected_lines: usize,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> OperationCompletion<Option<Vec<String>>> {
    if cancellation.is_requested() {
        return OperationCompletion::Cancelled;
    }
    let mut lines = Vec::new();
    if !allocation.try_reserve_vec(&mut lines, expected_lines) {
        return OperationCompletion::Completed(None);
    }
    for line in text.split('\n') {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        if lines.len() == expected_lines {
            return OperationCompletion::Completed(None);
        }
        let mut owned = String::new();
        if !allocation.try_reserve_string(&mut owned, line.len()) {
            return OperationCompletion::Completed(None);
        }
        owned.push_str(line);
        lines.push(owned);
    }
    if lines.len() != expected_lines {
        return OperationCompletion::Completed(None);
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(lines))
    }
}

fn line_separator_offsets(
    text: &str,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> OperationCompletion<Option<Vec<usize>>> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut count = 0_usize;
    for chunk in text.as_bytes().chunks(CANCELLATION_CHECK_BYTES) {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        let chunk_count = chunk.iter().filter(|byte| **byte == b'\n').count();
        let Some(next) = count.checked_add(chunk_count) else {
            return OperationCompletion::Completed(None);
        };
        count = next;
    }
    let mut offsets = Vec::new();
    if !allocation.try_reserve_vec(&mut offsets, count) {
        return OperationCompletion::Completed(None);
    }
    for (offset, _) in text.match_indices('\n') {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        offsets.push(offset);
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(offsets))
    }
}

fn rebuild_protected_translation(
    protected: &ProtectedText,
    repaired: &LanguageText,
    cancellation: &CooperativeCancellation,
    allocation: SymbolRepairAllocation,
) -> OperationCompletion<Option<String>> {
    let mut placeholders = protected.placeholders().iter();
    let mut total = 0_usize;
    for segment in repaired.segments() {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        let text = match segment {
            LanguageTextSegment::NaturalText(text) => text.as_str(),
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return OperationCompletion::Completed(None);
                };
                placeholder.original()
            }
        };
        let Some(next) = total.checked_add(text.len()) else {
            return OperationCompletion::Completed(None);
        };
        total = next;
    }
    if placeholders.next().is_some() {
        return OperationCompletion::Completed(None);
    }

    let mut output = String::new();
    if !allocation.try_reserve_string(&mut output, total) {
        return OperationCompletion::Completed(None);
    }
    let mut placeholders = protected.placeholders().iter();
    for segment in repaired.segments() {
        if cancellation.is_requested() {
            return OperationCompletion::Cancelled;
        }
        match segment {
            LanguageTextSegment::NaturalText(text) => output.push_str(text),
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return OperationCompletion::Completed(None);
                };
                output.push_str(placeholder.original());
            }
        }
    }
    if placeholders.next().is_some() {
        return OperationCompletion::Completed(None);
    }
    if cancellation.is_requested() {
        OperationCompletion::Cancelled
    } else {
        OperationCompletion::Completed(Some(output))
    }
}

fn aligned_replacement_lines(unit: &RpgMakerWriteBackUnit) -> Option<Vec<String>> {
    let translated = unit.translation_content.as_ref()?.as_lines()?;
    let source = unit
        .source_content
        .as_lines()
        .expect("严格对齐单元的原文必须是行序列");
    Some(
        source
            .iter()
            .zip(translated)
            .map(|(source, translated)| {
                if source.trim().is_empty() {
                    source.clone()
                } else {
                    translated.clone()
                }
            })
            .collect(),
    )
}

fn validate_content_presence(
    role: &TextUnitRole,
    content: &TextUnitContent,
    column: &'static str,
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    if matches!(content, TextUnitContent::Lines(lines) if lines.is_empty()) {
        Err(RpgMakerWriteBackSnapshotError::EmptyLineContent {
            role: role.clone(),
            column,
        })
    } else {
        Ok(())
    }
}

fn validate_content_structure(
    kind: TextGroupKind,
    role: &TextUnitRole,
    content: &TextUnitContent,
    column: &'static str,
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    validate_text_unit_content_structure(kind, role, TextUnitContentView::from(content)).map_err(
        |error| match error {
            TextUnitContentStructureError::KindRoleMismatch => {
                RpgMakerWriteBackSnapshotError::InvalidRole {
                    kind,
                    role: role.clone(),
                }
            }
            TextUnitContentStructureError::ShapeMismatch => {
                RpgMakerWriteBackSnapshotError::ContentShapeMismatch { role: role.clone() }
            }
            TextUnitContentStructureError::InvalidText { line_index } => {
                RpgMakerWriteBackSnapshotError::InvalidContentLine {
                    role: role.clone(),
                    column,
                    line_index,
                }
            }
        },
    )
}

fn validate_aligned_content(
    unit: &RpgMakerWriteBackUnit,
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    if !matches!(
        unit.role,
        TextUnitRole::Choices | TextUnitRole::ScrollingText
    ) {
        return Ok(());
    }
    let Some(translation) = &unit.translation_content else {
        return Ok(());
    };
    let source_lines = unit
        .source_content
        .as_lines()
        .expect("严格对齐角色的原文结构已由唯一校验器验证");
    let translated_lines = translation
        .as_lines()
        .expect("严格对齐角色的译文结构已由唯一校验器验证");
    if source_lines.len() != translated_lines.len() {
        return Err(RpgMakerWriteBackSnapshotError::AlignedLineCountMismatch {
            role: unit.role.clone(),
            expected: source_lines.len(),
            actual: translated_lines.len(),
        });
    }
    for (line_index, (source, translated)) in source_lines.iter().zip(translated_lines).enumerate()
    {
        let source_is_blank = source.trim().is_empty();
        if (source_is_blank && !translated.is_empty())
            || (!unit.manual && !source_is_blank && translated.trim().is_empty())
        {
            return Err(RpgMakerWriteBackSnapshotError::AlignedBlankLineMismatch {
                role: unit.role.clone(),
                line_index,
            });
        }
    }
    Ok(())
}

/// 一组语义单元及其已经物化的物理写回配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackGroup {
    owner: RpgMakerAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<RpgMakerWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    mutation_claims: MutationClaimSet,
}

impl RpgMakerWriteBackGroup {
    #[cfg(test)]
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<RpgMakerWriteBackUnit>,
        recipes: Vec<TextProjectionRecipe>,
        mutation_locks: Vec<MutationResourceLock>,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        Self::build(
            RpgMakerAssetOwner::Builtin,
            kind,
            group_location,
            units,
            recipes,
            Some(mutation_locks),
        )
    }

    /// 直接复用配方重建出的唯一 Claim 集合，避免读取边界先深拷贝全部 locks，
    /// 随后又从同一配方重建并排序一次。
    pub(crate) fn from_recipes(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<RpgMakerWriteBackUnit>,
        recipes: Vec<TextProjectionRecipe>,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        Self::build(owner, kind, group_location, units, recipes, None)
    }

    fn build(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        units: Vec<RpgMakerWriteBackUnit>,
        recipes: Vec<TextProjectionRecipe>,
        mutation_locks: Option<Vec<MutationResourceLock>>,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        if recipes.is_empty() {
            return Err(RpgMakerWriteBackSnapshotError::EmptyProjection {
                group_location: Box::new(group_location),
            });
        }
        for unit in &units {
            validate_content_structure(kind, &unit.role, &unit.source_content, "原文")?;
            if let Some(translation) = &unit.translation_content {
                validate_content_structure(kind, &unit.role, translation, "译文")?;
            }
            validate_aligned_content(unit)?;
        }
        let mut seen_roles = BTreeSet::new();
        for unit in &units {
            if !seen_roles.insert(unit.role.clone()) {
                return Err(RpgMakerWriteBackSnapshotError::DuplicateRole {
                    group_location: Box::new(group_location),
                    role: unit.role.clone(),
                });
            }
        }

        let unit_roles = units
            .iter()
            .map(|unit| unit.role.clone())
            .collect::<BTreeSet<_>>();
        let recipe_roles = recipes
            .iter()
            .flat_map(TextProjectionRecipe::referenced_roles)
            .collect::<BTreeSet<_>>();
        if unit_roles != recipe_roles {
            return Err(RpgMakerWriteBackSnapshotError::RecipeRoleMismatch {
                group_location: Box::new(group_location),
                units: unit_roles,
                recipes: recipe_roles,
            });
        }
        validate_line_references(&group_location, &units, &recipes)?;
        validate_projection_round_trip(&group_location, &units, &recipes)?;

        let expected_claims =
            mutation_claims_for_group(kind, &group_location, &recipes).map_err(|conflict| {
                RpgMakerWriteBackSnapshotError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                }
            })?;
        let mutation_claims = mutation_locks
            .map(MutationClaimSet::from_locks)
            .transpose()
            .map_err(
                |conflict| RpgMakerWriteBackSnapshotError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                },
            )?;
        if mutation_claims
            .as_ref()
            .is_some_and(|mutation_claims| expected_claims.locks() != mutation_claims.locks())
        {
            return Err(RpgMakerWriteBackSnapshotError::RecipeClaimMismatch {
                group_location: Box::new(group_location),
            });
        }
        let validated_claims = mutation_claims.as_ref().unwrap_or(&expected_claims);
        if let Some(lock) = validated_claims
            .locks()
            .iter()
            .find(|lock| lock.resource().source() != group_location.source())
        {
            return Err(
                RpgMakerWriteBackSnapshotError::MismatchedClaimResourceSource {
                    group_location: Box::new(group_location),
                    resource: Box::new(lock.resource().clone()),
                },
            );
        }
        for claim in expected_claims.claims() {
            let claim_location = claim.representative_location();
            if claim_location.source() != group_location.source() {
                return Err(RpgMakerWriteBackSnapshotError::MismatchedClaimSource {
                    group_location: Box::new(group_location),
                    claim: Box::new(claim.clone()),
                });
            }
        }
        let mutation_claims = mutation_claims.unwrap_or(expected_claims);

        match kind {
            TextGroupKind::EventDialogue => {
                if recipes.len() != 1 || !matches!(recipes[0], TextProjectionRecipe::Dialogue(_)) {
                    return Err(RpgMakerWriteBackSnapshotError::InvalidDialogueProjection {
                        group_location: Box::new(group_location),
                    });
                }
                let TextProjectionRecipe::Dialogue(recipe) = &recipes[0] else {
                    unreachable!()
                };
                if recipe.group_location() != &group_location {
                    return Err(RpgMakerWriteBackSnapshotError::MismatchedDialogueGroup {
                        group_location: Box::new(group_location),
                        recipe_location: Box::new(recipe.group_location().clone()),
                    });
                }
            }
            TextGroupKind::EventScrollingText => {
                if recipes
                    .iter()
                    .any(|recipe| !matches!(recipe, TextProjectionRecipe::Direct(_)))
                {
                    return Err(RpgMakerWriteBackSnapshotError::InvalidScrollingProjection {
                        group_location: Box::new(group_location),
                    });
                }
                validate_scrolling_projection(&group_location, &units, &recipes)?;
            }
            TextGroupKind::EventChoices => {
                if recipes.iter().any(|recipe| {
                    !matches!(
                        recipe,
                        TextProjectionRecipe::Direct(_) | TextProjectionRecipe::Claim(_)
                    )
                }) || recipes
                    .iter()
                    .filter(|recipe| matches!(recipe, TextProjectionRecipe::Claim(_)))
                    .count()
                    != 1
                {
                    return Err(RpgMakerWriteBackSnapshotError::InvalidChoicesProjection {
                        group_location: Box::new(group_location),
                    });
                }
            }
            _ => {
                if recipes
                    .iter()
                    .any(|recipe| !matches!(recipe, TextProjectionRecipe::Direct(_)))
                {
                    return Err(RpgMakerWriteBackSnapshotError::InvalidDirectProjection {
                        group_location: Box::new(group_location),
                    });
                }
            }
        }

        Ok(Self {
            owner,
            kind,
            group_location,
            units,
            recipes,
            mutation_claims,
        })
    }

    fn into_parts(
        self,
    ) -> (
        RpgMakerAssetOwner,
        TextGroupKind,
        RpgMakerLocation,
        Vec<RpgMakerWriteBackUnit>,
        Vec<TextProjectionRecipe>,
        MutationClaimSet,
    ) {
        (
            self.owner,
            self.kind,
            self.group_location,
            self.units,
            self.recipes,
            self.mutation_claims,
        )
    }

    pub(crate) fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }
}

fn validate_line_references(
    group_location: &RpgMakerLocation,
    units: &[RpgMakerWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    let mut referenced = BTreeMap::<TextUnitRole, BTreeMap<usize, usize>>::new();
    for (role, source_line_index) in recipes
        .iter()
        .flat_map(TextProjectionRecipe::referenced_lines)
    {
        *referenced
            .entry(role)
            .or_default()
            .entry(source_line_index)
            .or_default() += 1;
    }
    for unit in units {
        let actual = referenced.remove(&unit.role).unwrap_or_default();
        let Some(lines) = unit.source_content.as_lines() else {
            if !actual.is_empty() {
                return Err(RpgMakerWriteBackSnapshotError::RecipeLineMismatch {
                    group_location: Box::new(group_location.clone()),
                    role: unit.role.clone(),
                });
            }
            continue;
        };
        let expected_uses = if matches!(unit.role, TextUnitRole::Choices) {
            2
        } else {
            1
        };
        if actual.len() != lines.len()
            || (0..lines.len()).any(|index| actual.get(&index) != Some(&expected_uses))
        {
            return Err(RpgMakerWriteBackSnapshotError::RecipeLineMismatch {
                group_location: Box::new(group_location.clone()),
                role: unit.role.clone(),
            });
        }
    }
    debug_assert!(referenced.is_empty(), "角色集合已经在调用前验证一致");
    Ok(())
}

/// 以借用的冻结原文字节和固定大小游标逐段校验，不物化完整重建文本。
struct ExpectedRawCursor<'a> {
    expected_raw: &'a [u8],
    offset: Option<usize>,
    #[cfg(test)]
    visited_segments: usize,
    #[cfg(test)]
    visited_segment_bytes: usize,
}

impl<'a> ExpectedRawCursor<'a> {
    fn new(expected_raw: &'a str) -> Self {
        Self {
            expected_raw: expected_raw.as_bytes(),
            offset: Some(0),
            #[cfg(test)]
            visited_segments: 0,
            #[cfg(test)]
            visited_segment_bytes: 0,
        }
    }

    fn consume(&mut self, segment: &str) {
        #[cfg(test)]
        {
            self.visited_segments += 1;
            self.visited_segment_bytes += segment.len();
        }

        let Some(offset) = self.offset else {
            return;
        };
        let segment = segment.as_bytes();
        if self.expected_raw[offset..].starts_with(segment) {
            self.offset = Some(offset + segment.len());
        } else {
            self.offset = None;
        }
    }

    fn is_complete(&self) -> bool {
        self.offset == Some(self.expected_raw.len())
    }

    #[cfg(test)]
    fn work(&self) -> (usize, usize) {
        (self.visited_segments, self.visited_segment_bytes)
    }
}

fn validate_projection_round_trip(
    group_location: &RpgMakerLocation,
    units: &[RpgMakerWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    let units = units
        .iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    for recipe in recipes {
        match recipe {
            TextProjectionRecipe::Direct(recipe) => {
                let mut expected = ExpectedRawCursor::new(recipe.expected_raw());
                for part in recipe.parts() {
                    let segment = match part {
                        DirectTextPart::Literal(value) => value.as_str(),
                        DirectTextPart::TextSlot { role } => units
                            .get(role)
                            .and_then(|unit| unit.source_content.as_value())
                            .ok_or_else(|| {
                                RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                    group_location: Box::new(group_location.clone()),
                                    target: Box::new(recipe.target().clone()),
                                }
                            })?,
                        DirectTextPart::LineSlot {
                            role,
                            source_line_index,
                        } => units
                            .get(role)
                            .and_then(|unit| unit.source_content.as_lines())
                            .and_then(|lines| lines.get(*source_line_index))
                            .map(String::as_str)
                            .ok_or_else(|| {
                                RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                    group_location: Box::new(group_location.clone()),
                                    target: Box::new(recipe.target().clone()),
                                }
                            })?,
                    };
                    expected.consume(segment);
                }
                if !expected.is_complete() {
                    return Err(
                        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                            group_location: Box::new(group_location.clone()),
                            target: Box::new(recipe.target().clone()),
                        },
                    );
                }
            }
            TextProjectionRecipe::Dialogue(recipe) => {
                let speaker = units
                    .get(&TextUnitRole::DialogueSpeaker)
                    .and_then(|unit| unit.source_content.as_value());
                if let Some(target) = recipe.direct_speaker()
                    && speaker != Some(target.expected_raw())
                {
                    return Err(
                        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                            group_location: Box::new(group_location.clone()),
                            target: Box::new(target.physical_location().clone()),
                        },
                    );
                }
                for line in recipe.lines() {
                    let mut expected = ExpectedRawCursor::new(line.expected_raw());
                    for (part_index, part) in line.parts().iter().enumerate() {
                        let segment = match part {
                            DialogueLinePart::Literal(value) => value.as_str(),
                            DialogueLinePart::SpeakerSlot => speaker
                                .expect("调用前已经确认内嵌 SpeakerSlot 对应逻辑 Speaker 单元"),
                            DialogueLinePart::BodyLine { source_line_index } => {
                                if part_index + 1 != line.parts().len() {
                                    return Err(
                                        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(line.physical_location().clone()),
                                        },
                                    );
                                }
                                units
                                    .get(&TextUnitRole::DialogueBody)
                                    .and_then(|unit| unit.source_content.as_lines())
                                    .and_then(|lines| lines.get(*source_line_index))
                                    .map(String::as_str)
                                    .ok_or_else(|| RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(line.physical_location().clone()),
                                        })?
                            }
                        };
                        expected.consume(segment);
                    }
                    if !expected.is_complete() {
                        return Err(
                            RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                group_location: Box::new(group_location.clone()),
                                target: Box::new(line.physical_location().clone()),
                            },
                        );
                    }
                }
            }
            TextProjectionRecipe::Claim(_) => {}
        }
    }
    Ok(())
}

fn validate_scrolling_projection(
    group_location: &RpgMakerLocation,
    units: &[RpgMakerWriteBackUnit],
    recipes: &[TextProjectionRecipe],
) -> Result<(), RpgMakerWriteBackSnapshotError> {
    let lines = units
        .iter()
        .find(|unit| unit.role == TextUnitRole::ScrollingText)
        .and_then(|unit| unit.source_content.as_lines())
        .expect("受信滚动文本必须包含行序列单元");
    for (physical_index, recipe) in recipes.iter().enumerate() {
        let TextProjectionRecipe::Direct(recipe) = recipe else {
            unreachable!("调用前已经验证滚动文本只包含直接配方")
        };
        match recipe.parts() {
            [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::ScrollingText,
                    source_line_index,
                },
            ] if *source_line_index == physical_index
                && lines
                    .get(physical_index)
                    .is_some_and(|line| line == recipe.expected_raw()) => {}
            _ => {
                return Err(RpgMakerWriteBackSnapshotError::InvalidScrollingRecipe {
                    group_location: Box::new(group_location.clone()),
                });
            }
        }
    }
    Ok(())
}

/// Reader 在同一个一致读视图中建立的完整 RPG Maker 写回快照。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackSnapshot {
    groups: Vec<RpgMakerWriteBackGroup>,
    symbol_repair: Option<RpgMakerWriteBackSymbolRepairContext>,
}

impl RpgMakerWriteBackSnapshot {
    pub(crate) fn new(
        groups: Vec<RpgMakerWriteBackGroup>,
    ) -> Result<Self, RpgMakerWriteBackSnapshotError> {
        let claim_count = groups
            .iter()
            .map(|group| group.mutation_claims.locks().len())
            .sum();
        let mut claim_index = MutationClaimIndex::with_capacity(claim_count);
        for group in &groups {
            claim_index
                .insert(&group.mutation_claims)
                .map_err(
                    |conflict| RpgMakerWriteBackSnapshotError::MutationClaimConflict {
                        resource: Box::new(conflict.resource().clone()),
                    },
                )?;
        }
        Ok(Self {
            groups,
            symbol_repair: None,
        })
    }

    pub(crate) fn with_symbol_repair(
        mut self,
        context: RpgMakerWriteBackSymbolRepairContext,
    ) -> Self {
        self.symbol_repair = Some(context);
        self
    }

    fn into_parts(
        self,
    ) -> (
        Vec<RpgMakerWriteBackGroup>,
        Option<RpgMakerWriteBackSymbolRepairContext>,
    ) {
        (self.groups, self.symbol_repair)
    }
}

/// Reader 交回受信快照前必须排除的数据损坏。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackSnapshotError {
    BlankSourceContent {
        role: TextUnitRole,
    },
    BlankTranslationContent {
        role: TextUnitRole,
    },
    ContentShapeMismatch {
        role: TextUnitRole,
    },
    EmptyLineContent {
        role: TextUnitRole,
        column: &'static str,
    },
    InvalidContentLine {
        role: TextUnitRole,
        column: &'static str,
        line_index: usize,
    },
    AlignedLineCountMismatch {
        role: TextUnitRole,
        expected: usize,
        actual: usize,
    },
    AlignedBlankLineMismatch {
        role: TextUnitRole,
        line_index: usize,
    },
    EmptyProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidRole {
        kind: TextGroupKind,
        role: TextUnitRole,
    },
    DuplicateRole {
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
    RecipeRoleMismatch {
        group_location: Box<RpgMakerLocation>,
        units: BTreeSet<TextUnitRole>,
        recipes: BTreeSet<TextUnitRole>,
    },
    RecipeLineMismatch {
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
    RecipeClaimMismatch {
        group_location: Box<RpgMakerLocation>,
    },
    RecipeDoesNotRebuildOriginal {
        group_location: Box<RpgMakerLocation>,
        target: Box<RpgMakerLocation>,
    },
    MutationClaimConflict {
        resource: Box<RpgMakerLocation>,
    },
    MismatchedClaimSource {
        group_location: Box<RpgMakerLocation>,
        claim: Box<MutationClaim>,
    },
    MismatchedClaimResourceSource {
        group_location: Box<RpgMakerLocation>,
        resource: Box<RpgMakerLocation>,
    },
    InvalidDialogueProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidScrollingProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidScrollingRecipe {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidChoicesProjection {
        group_location: Box<RpgMakerLocation>,
    },
    InvalidDirectProjection {
        group_location: Box<RpgMakerLocation>,
    },
    MismatchedDialogueGroup {
        group_location: Box<RpgMakerLocation>,
        recipe_location: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for RpgMakerWriteBackSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankSourceContent { role } => {
                write!(formatter, "写回资产原文仅包含空白：{role:?}")
            }
            Self::BlankTranslationContent { role } => {
                write!(formatter, "写回资产译文仅包含空白：{role:?}")
            }
            Self::ContentShapeMismatch { role } => {
                write!(formatter, "写回资产内容形状与角色不一致：{role:?}")
            }
            Self::EmptyLineContent { role, column } => {
                write!(formatter, "写回资产{column}行序列为空：{role:?}")
            }
            Self::InvalidContentLine {
                role,
                column,
                line_index,
            } => write!(
                formatter,
                "写回资产{column}第 {line_index} 行包含不允许的控制字符：{role:?}"
            ),
            Self::AlignedLineCountMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "严格对齐译文行数不一致：{role:?}，期待 {expected}，实际 {actual}"
            ),
            Self::AlignedBlankLineMismatch { role, line_index } => write!(
                formatter,
                "严格对齐译文第 {line_index} 行的空白状态与原文不一致：{role:?}"
            ),
            Self::EmptyProjection { group_location } => {
                write!(formatter, "写回资产组不包含投影配方：{group_location}")
            }
            Self::InvalidRole { kind, role } => {
                write!(formatter, "写回资产角色与组类型 {kind:?} 不一致：{role:?}")
            }
            Self::DuplicateRole {
                group_location,
                role,
            } => write!(
                formatter,
                "写回资产组 {group_location} 重复逻辑角色 {role:?}"
            ),
            Self::RecipeRoleMismatch {
                group_location,
                units,
                recipes,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的单元角色与配方角色不一致：{units:?} / {recipes:?}"
            ),
            Self::RecipeLineMismatch {
                group_location,
                role,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的行槽索引或引用次数无效：{role:?}"
            ),
            Self::RecipeClaimMismatch { group_location } => {
                write!(
                    formatter,
                    "写回资产组 {group_location} 的物理修改声明没有覆盖配方"
                )
            }
            Self::RecipeDoesNotRebuildOriginal {
                group_location,
                target,
            } => write!(
                formatter,
                "写回资产组 {group_location} 的投影配方无法逐字重建冻结原文：{target}"
            ),
            Self::MutationClaimConflict { resource } => {
                write!(formatter, "写回快照包含冲突的物理修改声明：{resource:?}")
            }
            Self::MismatchedClaimSource {
                group_location,
                claim,
            } => write!(
                formatter,
                "写回资产组与物理修改声明不属于同一来源：{group_location} / {claim:?}"
            ),
            Self::MismatchedClaimResourceSource {
                group_location,
                resource,
            } => write!(
                formatter,
                "写回资产组与物理修改资源不属于同一来源：{group_location} / {resource:?}"
            ),
            Self::InvalidDialogueProjection { group_location } => write!(
                formatter,
                "对话组必须且只能包含一个对话配方：{group_location}"
            ),
            Self::InvalidScrollingProjection { group_location } => {
                write!(formatter, "滚动文本组只能包含直接配方：{group_location}")
            }
            Self::InvalidScrollingRecipe { group_location } => write!(
                formatter,
                "滚动文本组的语义行索引或直接配方无效：{group_location}"
            ),
            Self::InvalidChoicesProjection { group_location } => {
                write!(formatter, "选项组只能包含直接配方：{group_location}")
            }
            Self::InvalidDirectProjection { group_location } => {
                write!(formatter, "普通文本组只能包含直接配方：{group_location}")
            }
            Self::MismatchedDialogueGroup {
                group_location,
                recipe_location,
            } => write!(
                formatter,
                "对话组位置与配方位置不一致：{group_location} / {recipe_location}"
            ),
        }
    }
}

impl Error for RpgMakerWriteBackSnapshotError {}

/// 在读取 RPG Maker 文本资产表前确认所有 active owner 仍属于当前冻结来源。
///
/// 实现不得读取或校验术语依赖；术语数据不是 WriteBack 的输入。
pub(crate) trait RpgMakerWriteBackAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<RpgMakerWriteBackSnapshot, Self::Error>> + Send + use<Self>;
}

/// 当前允许自动布局的 RPG Maker 显示区域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutRegion {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

#[cfg(test)]
impl RpgMakerWriteBackLayoutRegion {
    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::DialogueBody => "dialogue_body",
            Self::ScrollingText => "scrolling_text",
            Self::HelpDescription => "help_description",
        }
    }
}

/// 共享布局内核中的一个原文/当前文本对。
///
/// `replacement == None` 表示该项仍使用冻结原文：它参与跨项括号与缩进状态观察，
/// 但布局结果不得修改它。调用方可以用 `Some` 提供已经确定的当前文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerLayoutTextPair {
    original_text: String,
    replacement: Option<String>,
}

impl RpgMakerLayoutTextPair {
    pub(crate) fn new(original_text: String, replacement: Option<String>) -> Self {
        Self {
            original_text,
            replacement,
        }
    }

    pub(crate) fn replacement(&self) -> Option<&str> {
        self.replacement.as_deref()
    }

    fn effective_text(&self) -> &str {
        self.replacement.as_deref().unwrap_or(&self.original_text)
    }
}

/// 一个布局段当前写回内容的权威来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutCandidate {
    /// 数据库没有译文，必须保持冻结原命令或原字段不变。
    FrozenOriginal,
    /// 数据库明确存在译文，允许布局器调整显示行。
    DatabaseTranslation(String),
}

/// 布局请求中一个仍与数据库语义单元保持对应关系的内容段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLayoutSegment {
    logical_location: Option<LogicalTextLocation>,
    exact_location: RpgMakerLocation,
    original_text: String,
    candidate: RpgMakerWriteBackLayoutCandidate,
}

impl RpgMakerWriteBackLayoutSegment {
    fn from_unit_at(
        group_location: &RpgMakerLocation,
        unit: &RpgMakerWriteBackUnit,
        exact_location: RpgMakerLocation,
    ) -> Self {
        let candidate = unit.translation_content.as_ref().map_or(
            RpgMakerWriteBackLayoutCandidate::FrozenOriginal,
            |content| RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(content_text(content)),
        );
        Self {
            logical_location: Some(LogicalTextLocation::new(
                group_location.clone(),
                unit.role.clone(),
            )),
            exact_location,
            original_text: content_text(&unit.source_content),
            candidate,
        }
    }

    fn from_line_at(
        group_location: &RpgMakerLocation,
        role: TextUnitRole,
        exact_location: RpgMakerLocation,
        original_text: String,
        translation: Option<String>,
    ) -> Self {
        Self {
            logical_location: Some(LogicalTextLocation::new(group_location.clone(), role)),
            exact_location,
            original_text,
            candidate: translation.map_or(
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation,
            ),
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn candidate(&self) -> &RpgMakerWriteBackLayoutCandidate {
        &self.candidate
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }
}

fn content_text(content: &TextUnitContent) -> String {
    match content {
        TextUnitContent::Value(value) => value.clone(),
        TextUnitContent::Lines(lines) => lines.join("\n"),
    }
}

/// RPG Maker 为一个完整布局单元建立的显式请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLayoutRequest {
    region: RpgMakerWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
    segments: Vec<RpgMakerWriteBackLayoutSegment>,
}

impl RpgMakerWriteBackLayoutRequest {
    pub(crate) fn new(
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
        segments: Vec<RpgMakerWriteBackLayoutSegment>,
    ) -> Self {
        debug_assert!(!segments.is_empty(), "布局单元必须包含至少一个文本段");
        debug_assert!(
            segments.iter().any(|segment| matches!(
                segment.candidate,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_)
            )),
            "没有数据库译文的单元不应请求布局"
        );
        Self {
            region,
            max_fullwidth_chars,
            segments,
        }
    }

    pub(crate) const fn max_fullwidth_chars(&self) -> MaxFullwidthChars {
        self.max_fullwidth_chars
    }

    pub(crate) fn segments(&self) -> &[RpgMakerWriteBackLayoutSegment] {
        &self.segments
    }
}

/// 布局器产生的一条最终显示行及其所属译文语义行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLaidOutLine {
    text: String,
    source_semantic_line_index: usize,
}

impl RpgMakerWriteBackLaidOutLine {
    pub(crate) fn new(text: String, source_semantic_line_index: usize) -> Self {
        Self {
            text,
            source_semantic_line_index,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn source_semantic_line_index(&self) -> usize {
        self.source_semantic_line_index
    }
}

/// 布局器为一个数据库译文单元产生的最终显示行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackLaidOutSegment {
    exact_location: RpgMakerLocation,
    lines: Vec<RpgMakerWriteBackLaidOutLine>,
}

impl RpgMakerWriteBackLaidOutSegment {
    pub(crate) fn new(
        exact_location: RpgMakerLocation,
        lines: Vec<RpgMakerWriteBackLaidOutLine>,
    ) -> Result<Self, RpgMakerWriteBackAppliedLayoutError> {
        if lines.is_empty() {
            return Err(RpgMakerWriteBackAppliedLayoutError::EmptyReplacement {
                exact_location: Box::new(exact_location),
            });
        }
        if let Some(line_index) = lines.iter().position(|line| line.text.contains('\n')) {
            return Err(RpgMakerWriteBackAppliedLayoutError::EmbeddedLineBreak {
                exact_location: Box::new(exact_location),
                line_index,
            });
        }
        Ok(Self {
            exact_location,
            lines,
        })
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[RpgMakerWriteBackLaidOutLine] {
        &self.lines
    }
}

/// 一次已经通过请求对应性校验的布局成功结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackAppliedLayout {
    segments: Vec<RpgMakerWriteBackLaidOutSegment>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

impl RpgMakerWriteBackAppliedLayout {
    pub(crate) fn new(
        request: &RpgMakerWriteBackLayoutRequest,
        segments: Vec<RpgMakerWriteBackLaidOutSegment>,
        inserted_line_breaks: usize,
        inserted_fullwidth_indents: usize,
    ) -> Result<Self, RpgMakerWriteBackAppliedLayoutError> {
        let mut replacements = BTreeMap::new();
        for segment in segments {
            let location = segment.exact_location.clone();
            if replacements.insert(location.clone(), segment).is_some() {
                return Err(RpgMakerWriteBackAppliedLayoutError::DuplicateReplacement {
                    exact_location: Box::new(location),
                });
            }
        }

        let mut ordered = Vec::new();
        for request_segment in &request.segments {
            match request_segment.candidate {
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal => {
                    if replacements.contains_key(&request_segment.exact_location) {
                        return Err(RpgMakerWriteBackAppliedLayoutError::ChangesFrozenOriginal {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    }
                }
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_) => {
                    let Some(segment) = replacements.remove(&request_segment.exact_location) else {
                        return Err(RpgMakerWriteBackAppliedLayoutError::MissingReplacement {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    };
                    ordered.push(segment);
                }
            }
        }
        if let Some((exact_location, _)) = replacements.into_iter().next() {
            return Err(RpgMakerWriteBackAppliedLayoutError::UnexpectedReplacement {
                exact_location: Box::new(exact_location),
            });
        }

        Ok(Self {
            segments: ordered,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        })
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[RpgMakerWriteBackLaidOutSegment] {
        &self.segments
    }

    #[cfg(test)]
    pub(crate) const fn inserted_line_breaks(&self) -> usize {
        self.inserted_line_breaks
    }

    #[cfg(test)]
    pub(crate) const fn inserted_fullwidth_indents(&self) -> usize {
        self.inserted_fullwidth_indents
    }

    fn into_parts(self) -> (Vec<RpgMakerWriteBackLaidOutSegment>, usize, usize) {
        (
            self.segments,
            self.inserted_line_breaks,
            self.inserted_fullwidth_indents,
        )
    }
}

/// 布局器在构造 Applied 结果时违反请求边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackAppliedLayoutError {
    EmptyReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    EmbeddedLineBreak {
        exact_location: Box<RpgMakerLocation>,
        line_index: usize,
    },
    DuplicateReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    ChangesFrozenOriginal {
        exact_location: Box<RpgMakerLocation>,
    },
    MissingReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    UnexpectedReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for RpgMakerWriteBackAppliedLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyReplacement { exact_location } => {
                write!(formatter, "布局结果没有提供任何显示行：{exact_location}")
            }
            Self::EmbeddedLineBreak {
                exact_location,
                line_index,
            } => write!(
                formatter,
                "布局结果第 {line_index} 个显示行仍包含真实换行：{exact_location}"
            ),
            Self::DuplicateReplacement { exact_location } => {
                write!(formatter, "布局结果重复返回位置：{exact_location}")
            }
            Self::ChangesFrozenOriginal { exact_location } => {
                write!(formatter, "布局结果试图修改缺译原文：{exact_location}")
            }
            Self::MissingReplacement { exact_location } => {
                write!(formatter, "布局结果缺少数据库译文位置：{exact_location}")
            }
            Self::UnexpectedReplacement { exact_location } => {
                write!(formatter, "布局结果包含请求外位置：{exact_location}")
            }
        }
    }
}

impl Error for RpgMakerWriteBackAppliedLayoutError {}

/// 保守布局的正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackLayoutOutcome {
    Applied(RpgMakerWriteBackAppliedLayout),
    /// 无法保证阅读质量；调用方必须撤销整个单元的自动布局。
    Manual,
}

/// 对一个完整 RPG Maker 显示单元执行保守布局。
///
/// 本能力是同步纯业务计算，并且必须遵守以下交接约束：
///
/// - 请求已经显式给出区域与该区域的行宽，不得自行读取或选择整个布局 Profile；
/// - 数据库译文已有的真实换行是必须保留的硬边界，只对其中过宽的语义行新增自动换行；
/// - 段边界就是数据库语义单元的来源边界：可以跨段观察括号和缩进状态，但不得把字符
///   移动到其他段，也不得返回对 `FrozenOriginal` 的修改；
/// - 必须先决定自动换行，再为符合规则的续行补全角空格；
/// - `inserted_line_breaks` 与 `inserted_fullwidth_indents` 只统计本次自动新增内容，
///   不包含数据库硬换行、原 401/405 边界或原文已有空格。
///
/// 控制符不明确、没有安全断点或无法完整遵守上述规则时，必须对整个请求返回
/// `Manual`，不得升级为技术错误或强制切断文本。
pub(crate) trait RpgMakerWriteBackTextLayouter: Send + Sync {
    fn layout(&self, request: &RpgMakerWriteBackLayoutRequest) -> RpgMakerWriteBackLayoutOutcome;
}

/// 一次已经按物化直接配方完成渲染的单值替换。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetTextMutation {
    exact_location: RpgMakerLocation,
    mutation_claims: MutationClaimSet,
    expected_original: String,
    replacement: String,
}

impl SetTextMutation {
    #[cfg(test)]
    pub(crate) fn for_test(
        exact_location: RpgMakerLocation,
        expected_original: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        let mutation_claim = MutationClaim::for_location(exact_location.clone());
        Self::for_test_with_claim(
            exact_location,
            mutation_claim,
            expected_original,
            replacement,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_claim(
        exact_location: RpgMakerLocation,
        mutation_claim: MutationClaim,
        expected_original: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        assert_eq!(
            mutation_claim.representative_location(),
            &exact_location,
            "测试 SetText Claim 必须描述同一位置"
        );
        Self {
            exact_location,
            mutation_claims: MutationClaimSet::new(vec![mutation_claim])
                .expect("单一测试 Claim 不应自冲突"),
            expected_original: expected_original.into(),
            replacement: replacement.into(),
        }
    }

    fn from_recipe(recipe: &DirectTextRecipe, replacement: String) -> Self {
        Self {
            exact_location: recipe.target().clone(),
            mutation_claims: MutationClaimSet::new(vec![recipe.mutation_claim().clone()])
                .expect("受信直接配方的单一 Claim 不应自冲突"),
            expected_original: recipe.expected_raw().to_owned(),
            replacement,
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn replacement(&self) -> &str {
        &self.replacement
    }

    fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }
}

/// 一个 `101 + 401*` 对话块的唯一原子修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceDialogueMutation {
    recipe: DialogueWriteRecipe,
    mutation_claims: MutationClaimSet,
    source_speaker: Option<String>,
    speaker: Option<String>,
    body_lines: Option<Vec<RpgMakerWriteBackLaidOutLine>>,
}

impl ReplaceDialogueMutation {
    #[cfg(test)]
    pub(crate) fn new(
        recipe: DialogueWriteRecipe,
        source_speaker: Option<String>,
        speaker: Option<String>,
        body_lines: Option<Vec<RpgMakerWriteBackLaidOutLine>>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        let mutation_claims = mutation_claims_for_group(
            TextGroupKind::EventDialogue,
            recipe.group_location(),
            &[TextProjectionRecipe::Dialogue(recipe.clone())],
        )
        .map_err(
            |conflict| RpgMakerWriteBackMutationPlanError::MutationClaimConflict {
                resource: Box::new(conflict.resource().clone()),
            },
        )?;
        Self::new_with_claims(recipe, mutation_claims, source_speaker, speaker, body_lines)
    }

    fn new_with_claims(
        recipe: DialogueWriteRecipe,
        mutation_claims: MutationClaimSet,
        source_speaker: Option<String>,
        speaker: Option<String>,
        body_lines: Option<Vec<RpgMakerWriteBackLaidOutLine>>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        let referenced = TextProjectionRecipe::Dialogue(recipe.clone()).referenced_roles();
        let expects_speaker = referenced.contains(&TextUnitRole::DialogueSpeaker);
        if expects_speaker != speaker.is_some() || expects_speaker != source_speaker.is_some() {
            return Err(RpgMakerWriteBackMutationPlanError::InvalidDialogue {
                group_location: Box::new(recipe.group_location().clone()),
                violation: WriteBackDialoguePlanViolation::SpeakerSlotMismatch,
            });
        }
        let expects_body = referenced.contains(&TextUnitRole::DialogueBody);
        if !expects_body && body_lines.is_some() {
            return Err(RpgMakerWriteBackMutationPlanError::InvalidDialogue {
                group_location: Box::new(recipe.group_location().clone()),
                violation: WriteBackDialoguePlanViolation::UnexpectedBodyTranslation,
            });
        }
        if let Some(lines) = &body_lines {
            if lines.is_empty() {
                return Err(RpgMakerWriteBackMutationPlanError::InvalidDialogue {
                    group_location: Box::new(recipe.group_location().clone()),
                    violation: WriteBackDialoguePlanViolation::EmptyBodyLines,
                });
            }
            let semantic_indexes = lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::source_semantic_line_index)
                .collect::<Vec<_>>();
            if semantic_indexes.first() != Some(&0)
                || semantic_indexes.windows(2).any(|pair| pair[0] > pair[1])
                || semantic_indexes
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .iter()
                    .copied()
                    .enumerate()
                    .any(|(expected, actual)| expected != actual)
            {
                return Err(RpgMakerWriteBackMutationPlanError::InvalidDialogue {
                    group_location: Box::new(recipe.group_location().clone()),
                    violation: WriteBackDialoguePlanViolation::NonContiguousBodySemanticIndexes,
                });
            }
        }
        Ok(Self {
            recipe,
            mutation_claims,
            source_speaker,
            speaker,
            body_lines,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        self.recipe.group_location()
    }

    pub(crate) fn recipe(&self) -> &DialogueWriteRecipe {
        &self.recipe
    }

    pub(crate) fn speaker(&self) -> Option<&str> {
        self.speaker.as_deref()
    }

    pub(crate) fn source_speaker(&self) -> Option<&str> {
        self.source_speaker.as_deref()
    }

    pub(crate) fn body_lines(&self) -> Option<&[RpgMakerWriteBackLaidOutLine]> {
        self.body_lines.as_deref()
    }

    fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }
}

/// 一个选项头及其同层分支标签的唯一原子修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceChoicesMutation {
    group_location: RpgMakerLocation,
    recipes: Vec<DirectTextRecipe>,
    mutation_claims: MutationClaimSet,
    source_lines: Vec<String>,
    replacement_lines: Vec<String>,
}

impl ReplaceChoicesMutation {
    #[cfg(test)]
    pub(crate) fn new(
        group_location: RpgMakerLocation,
        recipes: Vec<DirectTextRecipe>,
        source_lines: Vec<String>,
        replacement_lines: Vec<String>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        let projections = recipes
            .iter()
            .cloned()
            .map(TextProjectionRecipe::Direct)
            .collect::<Vec<_>>();
        let mutation_claims =
            mutation_claims_for_group(TextGroupKind::EventChoices, &group_location, &projections)
                .map_err(
                |conflict| RpgMakerWriteBackMutationPlanError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                },
            )?;
        Self::new_with_claims(
            group_location,
            recipes,
            mutation_claims,
            source_lines,
            replacement_lines,
        )
    }

    fn new_with_claims(
        group_location: RpgMakerLocation,
        recipes: Vec<DirectTextRecipe>,
        mutation_claims: MutationClaimSet,
        source_lines: Vec<String>,
        replacement_lines: Vec<String>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        if source_lines.is_empty() || source_lines.len() != replacement_lines.len() {
            return Err(RpgMakerWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                violation: WriteBackChoicesPlanViolation::EmptyOrMismatchedLineCount,
            });
        }
        if source_lines
            .iter()
            .zip(&replacement_lines)
            .any(|(source, replacement)| source.trim().is_empty() && source != replacement)
        {
            return Err(RpgMakerWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                violation: WriteBackChoicesPlanViolation::BlankSlotChanged,
            });
        }
        let mut references = BTreeMap::<usize, usize>::new();
        for recipe in &recipes {
            let [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index,
                },
            ] = recipe.parts()
            else {
                return Err(RpgMakerWriteBackMutationPlanError::InvalidChoices {
                    group_location: Box::new(group_location),
                    violation: WriteBackChoicesPlanViolation::InvalidRecipeShape,
                });
            };
            if source_lines.get(*source_line_index).map(String::as_str)
                != Some(recipe.expected_raw())
            {
                return Err(RpgMakerWriteBackMutationPlanError::InvalidChoices {
                    group_location: Box::new(group_location),
                    violation: WriteBackChoicesPlanViolation::RecipeSourceMismatch,
                });
            }
            *references.entry(*source_line_index).or_default() += 1;
        }
        if references.len() != source_lines.len()
            || (0..source_lines.len()).any(|index| references.get(&index) != Some(&2))
        {
            return Err(RpgMakerWriteBackMutationPlanError::InvalidChoices {
                group_location: Box::new(group_location),
                violation: WriteBackChoicesPlanViolation::IncompleteCommandCoverage,
            });
        }
        Ok(Self {
            group_location,
            recipes,
            mutation_claims,
            source_lines,
            replacement_lines,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn recipes(&self) -> &[DirectTextRecipe] {
        &self.recipes
    }

    pub(crate) fn source_lines(&self) -> &[String] {
        &self.source_lines
    }

    pub(crate) fn replacement_lines(&self) -> &[String] {
        &self.replacement_lines
    }

    fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }
}

/// 一条原始 401/405 正文在块级重建计划中的对应项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventBodyMutationSegment {
    exact_location: RpgMakerLocation,
    expected_original: String,
    replacement_lines: Vec<String>,
}

impl EventBodyMutationSegment {
    #[cfg(test)]
    pub(crate) fn replace_for_test(
        exact_location: RpgMakerLocation,
        expected_original: impl Into<String>,
        lines: Vec<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            replacement_lines: lines,
        }
    }

    fn replace(
        exact_location: RpgMakerLocation,
        expected_original: String,
        lines: Vec<String>,
    ) -> Self {
        debug_assert!(!lines.is_empty(), "译文语义行必须至少产生一个原生正文行");
        Self {
            exact_location,
            expected_original,
            replacement_lines: lines,
        }
    }

    pub(crate) fn exact_location(&self) -> &RpgMakerLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn replacement_lines(&self) -> &[String] {
        &self.replacement_lines
    }
}

/// 一个完整滚动文本正文块的重建计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceEventBodyMutation {
    group_location: RpgMakerLocation,
    segments: Vec<EventBodyMutationSegment>,
    mutation_claims: MutationClaimSet,
}

impl ReplaceEventBodyMutation {
    #[cfg(test)]
    pub(crate) fn new(
        group_location: RpgMakerLocation,
        segments: Vec<EventBodyMutationSegment>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        if segments.is_empty() {
            return Err(RpgMakerWriteBackMutationPlanError::EmptyEventBody {
                group_location: Box::new(group_location),
            });
        }
        let covered_values = segments
            .iter()
            .map(|segment| segment.exact_location.clone())
            .collect();
        let event_claim = MutationClaim::event_block(group_location.clone(), covered_values)
            .expect("测试滚动文本 Claim 必须由同一来源的 Value 地址组成");
        let mutation_claims = MutationClaimSet::new(vec![event_claim]).map_err(|conflict| {
            RpgMakerWriteBackMutationPlanError::MutationClaimConflict {
                resource: Box::new(conflict.resource().clone()),
            }
        })?;
        Self::new_with_claims(group_location, segments, mutation_claims)
    }

    fn new_with_claims(
        group_location: RpgMakerLocation,
        segments: Vec<EventBodyMutationSegment>,
        mutation_claims: MutationClaimSet,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        if segments.is_empty() {
            return Err(RpgMakerWriteBackMutationPlanError::EmptyEventBody {
                group_location: Box::new(group_location),
            });
        }
        let mut exact_locations = BTreeSet::new();
        for segment in &segments {
            if !exact_locations.insert(segment.exact_location.clone()) {
                return Err(RpgMakerWriteBackMutationPlanError::DuplicateLocation {
                    exact_location: Box::new(segment.exact_location.clone()),
                });
            }
            if segment.replacement_lines.is_empty() {
                return Err(RpgMakerWriteBackMutationPlanError::EmptyEventReplacement {
                    exact_location: Box::new(segment.exact_location.clone()),
                });
            }
        }
        Ok(Self {
            group_location,
            segments,
            mutation_claims,
        })
    }

    pub(crate) fn group_location(&self) -> &RpgMakerLocation {
        &self.group_location
    }

    pub(crate) fn segments(&self) -> &[EventBodyMutationSegment] {
        &self.segments
    }

    fn mutation_claims(&self) -> &MutationClaimSet {
        &self.mutation_claims
    }
}

/// RPG Maker 交给 RPG Maker 文档改写器的一项领域修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackMutation {
    SetText(SetTextMutation),
    ReplaceDialogue(ReplaceDialogueMutation),
    ReplaceChoices(ReplaceChoicesMutation),
    ReplaceEventBody(ReplaceEventBodyMutation),
}

/// 已经排除位置冲突的一轮完整文档修改计划。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackMutationPlan {
    mutations: Vec<RpgMakerWriteBackMutation>,
}

impl RpgMakerWriteBackMutationPlan {
    pub(crate) fn new(
        mutations: Vec<RpgMakerWriteBackMutation>,
    ) -> Result<Self, RpgMakerWriteBackMutationPlanError> {
        let mut claim_index = MutationClaimIndex::default();
        for mutation in &mutations {
            let claims = match mutation {
                RpgMakerWriteBackMutation::SetText(mutation) => mutation.mutation_claims(),
                RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => mutation.mutation_claims(),
                RpgMakerWriteBackMutation::ReplaceChoices(mutation) => mutation.mutation_claims(),
                RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => mutation.mutation_claims(),
            };
            claim_index.insert(claims).map_err(|conflict| {
                RpgMakerWriteBackMutationPlanError::MutationClaimConflict {
                    resource: Box::new(conflict.resource().clone()),
                }
            })?;
        }
        Ok(Self { mutations })
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn mutations(&self) -> &[RpgMakerWriteBackMutation] {
        &self.mutations
    }

    pub(crate) fn into_mutations(self) -> Vec<RpgMakerWriteBackMutation> {
        self.mutations
    }
}

/// Mutation 计划构造时发现的内部冲突。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackDialoguePlanViolation {
    SpeakerSlotMismatch,
    UnexpectedBodyTranslation,
    EmptyBodyLines,
    NonContiguousBodySemanticIndexes,
}

impl fmt::Display for WriteBackDialoguePlanViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SpeakerSlotMismatch => "Speaker 槽与最终 Speaker 不一致",
            Self::UnexpectedBodyTranslation => "没有 Body 槽的对话不能提供 Body 译文",
            Self::EmptyBodyLines => "Body 没有产生显示行",
            Self::NonContiguousBodySemanticIndexes => "Body 显示行的语义来源索引不连续",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackChoicesPlanViolation {
    EmptyOrMismatchedLineCount,
    BlankSlotChanged,
    InvalidRecipeShape,
    RecipeSourceMismatch,
    IncompleteCommandCoverage,
}

impl fmt::Display for WriteBackChoicesPlanViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyOrMismatchedLineCount => "选项原文与译文必须是等长非空行序列",
            Self::BlankSlotChanged => "选项空白槽必须逐字保持冻结原文",
            Self::InvalidRecipeShape => "选项配方必须只包含一个 Choices 行槽",
            Self::RecipeSourceMismatch => "选项配方与冻结原文不一致",
            Self::IncompleteCommandCoverage => "每个选项必须同时对应 102 列表项和同层 402 标签",
        })
    }
}

/// WriteBack 使用当前项目 Placeholder 资源重新验收 Current 时发现的确定失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerWriteBackPlaceholderValidationError {
    unit: RpgMakerUnitLocator,
    reason: TranslationPlanningFailureReason,
}

impl RpgMakerWriteBackPlaceholderValidationError {
    fn new(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
        reason: TranslationPlanningFailureReason,
    ) -> Self {
        Self {
            unit: RpgMakerUnitLocator::new(
                owner.diagnostic_owner(),
                kind.diagnostic_group_kind(),
                group_location.diagnostic_location(),
                role.diagnostic_role(),
            ),
            reason,
        }
    }

    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let issue = match &self.reason {
            TranslationPlanningFailureReason::PlaceholderProtection { failure } => {
                RpgMakerIssue::write_back_placeholder_planning(
                    PlaceholderRuleSource::ProjectSnapshot,
                    self.unit.clone(),
                    placeholder_protection_diagnostic(failure),
                )
            }
            TranslationPlanningFailureReason::PlaceholderProjection { failure } => {
                RpgMakerIssue::write_back_placeholder_projection(
                    self.unit.clone(),
                    placeholder_projection_diagnostic(failure),
                )
            }
        };
        DiagnosticReport::new(StateEffect::Unchanged, Diagnostic::rpg_maker(issue))
    }
}

impl fmt::Display for RpgMakerWriteBackPlaceholderValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TranslationPlanningFailureReason::PlaceholderProtection { .. } => {
                formatter.write_str("Current 无法按项目 Placeholder 规则建立保护")
            }
            TranslationPlanningFailureReason::PlaceholderProjection { .. } => {
                formatter.write_str("Current 的 Placeholder 绑定与冻结原文不一致")
            }
        }
    }
}

impl Error for RpgMakerWriteBackPlaceholderValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerWriteBackMutationPlanError {
    EmptyEventBody {
        group_location: Box<RpgMakerLocation>,
    },
    EmptyEventReplacement {
        exact_location: Box<RpgMakerLocation>,
    },
    InvalidDialogue {
        group_location: Box<RpgMakerLocation>,
        violation: WriteBackDialoguePlanViolation,
    },
    InvalidChoices {
        group_location: Box<RpgMakerLocation>,
        violation: WriteBackChoicesPlanViolation,
    },
    DuplicateLocation {
        exact_location: Box<RpgMakerLocation>,
    },
    MutationClaimConflict {
        resource: Box<RpgMakerLocation>,
    },
}

impl fmt::Display for RpgMakerWriteBackMutationPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEventBody { group_location } => {
                write!(formatter, "事件正文修改不包含原始段：{group_location}")
            }
            Self::EmptyEventReplacement { exact_location } => {
                write!(formatter, "事件正文译文没有产生显示行：{exact_location}")
            }
            Self::InvalidDialogue {
                group_location,
                violation,
            } => write!(formatter, "对话修改 {group_location} 无效：{violation}"),
            Self::InvalidChoices {
                group_location,
                violation,
            } => write!(formatter, "选项修改 {group_location} 无效：{violation}"),
            Self::DuplicateLocation { exact_location } => {
                write!(formatter, "Mutation 计划重复修改物理地址：{exact_location}")
            }
            Self::MutationClaimConflict { resource } => {
                write!(formatter, "Mutation 计划的物理修改声明冲突：{resource:?}")
            }
        }
    }
}

impl Error for RpgMakerWriteBackMutationPlanError {}

impl RpgMakerWriteBackMutationPlanError {
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::write_back_planning(
                RpgMakerWriteBackPlanningProblem::InvalidPlan {
                    violation: self.diagnostic_violation(),
                },
            )),
        )
    }

    fn diagnostic_violation(&self) -> RpgMakerWriteBackMutationPlanViolation {
        match self {
            Self::EmptyEventBody { group_location } => {
                RpgMakerWriteBackMutationPlanViolation::EmptyEventBody {
                    group_location: group_location.diagnostic_location(),
                }
            }
            Self::EmptyEventReplacement { exact_location } => {
                RpgMakerWriteBackMutationPlanViolation::EmptyEventReplacement {
                    exact_location: exact_location.diagnostic_location(),
                }
            }
            Self::InvalidDialogue {
                group_location,
                violation,
            } => RpgMakerWriteBackMutationPlanViolation::InvalidDialogue {
                group_location: group_location.diagnostic_location(),
                violation: match violation {
                    WriteBackDialoguePlanViolation::SpeakerSlotMismatch => {
                        RpgMakerWriteBackDialoguePlanViolation::SpeakerSlotMismatch
                    }
                    WriteBackDialoguePlanViolation::UnexpectedBodyTranslation => {
                        RpgMakerWriteBackDialoguePlanViolation::UnexpectedBodyTranslation
                    }
                    WriteBackDialoguePlanViolation::EmptyBodyLines => {
                        RpgMakerWriteBackDialoguePlanViolation::EmptyBodyLines
                    }
                    WriteBackDialoguePlanViolation::NonContiguousBodySemanticIndexes => {
                        RpgMakerWriteBackDialoguePlanViolation::NonContiguousBodySemanticIndexes
                    }
                },
            },
            Self::InvalidChoices {
                group_location,
                violation,
            } => RpgMakerWriteBackMutationPlanViolation::InvalidChoices {
                group_location: group_location.diagnostic_location(),
                violation: match violation {
                    WriteBackChoicesPlanViolation::EmptyOrMismatchedLineCount => {
                        RpgMakerWriteBackChoicesPlanViolation::EmptyOrMismatchedLineCount
                    }
                    WriteBackChoicesPlanViolation::BlankSlotChanged => {
                        RpgMakerWriteBackChoicesPlanViolation::BlankSlotChanged
                    }
                    WriteBackChoicesPlanViolation::InvalidRecipeShape => {
                        RpgMakerWriteBackChoicesPlanViolation::InvalidRecipeShape
                    }
                    WriteBackChoicesPlanViolation::RecipeSourceMismatch => {
                        RpgMakerWriteBackChoicesPlanViolation::RecipeSourceMismatch
                    }
                    WriteBackChoicesPlanViolation::IncompleteCommandCoverage => {
                        RpgMakerWriteBackChoicesPlanViolation::IncompleteCommandCoverage
                    }
                },
            },
            Self::DuplicateLocation { exact_location } => {
                RpgMakerWriteBackMutationPlanViolation::DuplicateLocation {
                    exact_location: exact_location.diagnostic_location(),
                }
            }
            Self::MutationClaimConflict { resource } => {
                RpgMakerWriteBackMutationPlanViolation::MutationClaimConflict {
                    resource: resource.diagnostic_location(),
                }
            }
        }
    }
}

/// 把领域 Mutation 应用到冻结 RPG Maker 文档并产生一个待发布候选。
///
/// 实现必须从 `OpenedProject::source_root()` 下的冻结文档读取权威结构，并在修改前用
/// `expected_original` 核对每个目标仍与快照一致。每项 Mutation 必须恰好应用一次；
/// 目标缺失、重复或原文不匹配都是技术错误。`ReplaceDialogue` 必须同时核对并替换
/// Speaker 与完整 `101 + 401*` 块；`ReplaceEventBody` 仅用于 `105 + 405*` 滚动正文。
/// 本能力只产生候选，不发布文件，也不把领域计划改写成 JSON 或字节覆盖集合。
pub(crate) trait RpgMakerWriteBackDocumentRewriter: Send + Sync {
    type RewrittenDocuments: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn rewrite(
        &self,
        project: &OpenedProject,
        plan: RpgMakerWriteBackMutationPlan,
    ) -> impl Future<Output = Result<Self::RewrittenDocuments, Self::Error>> + Send;
}

/// 一项需要人工调整布局、但没有阻止写回的结构化诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualLayoutDiagnostic {
    locations: Vec<LogicalTextLocation>,
    region: RpgMakerWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
}

impl ManualLayoutDiagnostic {
    fn from_request(request: &RpgMakerWriteBackLayoutRequest) -> Self {
        let mut seen = BTreeSet::new();
        let locations = request
            .segments
            .iter()
            .filter_map(|segment| match segment.candidate {
                RpgMakerWriteBackLayoutCandidate::FrozenOriginal => None,
                RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(_) => {
                    let location = segment
                        .logical_location
                        .clone()
                        .expect("数据库译文布局段必须属于逻辑单元");
                    seen.insert(location.clone()).then_some(location)
                }
            })
            .collect();
        Self::new(locations, request.region, request.max_fullwidth_chars)
    }

    fn new(
        locations: Vec<LogicalTextLocation>,
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
    ) -> Self {
        assert!(
            !locations.is_empty(),
            "人工布局诊断必须关联至少一个逻辑单元"
        );
        Self {
            locations,
            region,
            max_fullwidth_chars,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        locations: Vec<LogicalTextLocation>,
        region: RpgMakerWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
    ) -> Self {
        Self::new(locations, region, max_fullwidth_chars)
    }

    #[cfg(test)]
    pub(crate) fn locations(&self) -> &[LogicalTextLocation] {
        &self.locations
    }

    #[cfg(test)]
    pub(crate) const fn region_name(&self) -> &'static str {
        self.region.diagnostic_name()
    }

    #[cfg(test)]
    pub(crate) fn max_fullwidth_chars(&self) -> u32 {
        self.max_fullwidth_chars.get()
    }

    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let locations = self
            .locations
            .iter()
            .map(|location| {
                RpgMakerLogicalUnitLocator::new(
                    location.group_location().diagnostic_location(),
                    location.role().diagnostic_role(),
                )
            })
            .collect();
        let region = match self.region {
            RpgMakerWriteBackLayoutRegion::DialogueBody => RpgMakerManualLayoutRegion::DialogueBody,
            RpgMakerWriteBackLayoutRegion::ScrollingText => {
                RpgMakerManualLayoutRegion::ScrollingText
            }
            RpgMakerWriteBackLayoutRegion::HelpDescription => {
                RpgMakerManualLayoutRegion::HelpDescription
            }
        };
        DiagnosticReport::new(
            StateEffect::Applied,
            Diagnostic::rpg_maker(RpgMakerIssue::manual_layout_required(
                locations,
                region,
                self.max_fullwidth_chars.get(),
            )),
        )
    }
}

/// RPG Maker 阶段生成的文件候选和全部业务事实。
pub(crate) struct RpgMakerWriteBackPreparation<D> {
    documents: D,
    summary: RpgMakerWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

impl<D> fmt::Debug for RpgMakerWriteBackPreparation<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerWriteBackPreparation")
            .field("summary", &self.summary)
            .field("manual_layout_diagnostics", &self.manual_layout_diagnostics)
            .field("documents", &"<owned documents>")
            .finish()
    }
}

impl<D> RpgMakerWriteBackPreparation<D> {
    pub(crate) fn new(
        documents: D,
        summary: RpgMakerWriteBackSummary,
        manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
    ) -> Self {
        assert_eq!(
            summary.manual_layout_units,
            manual_layout_diagnostics.len(),
            "人工布局计数必须由结构化诊断唯一建立"
        );
        Self {
            documents,
            summary,
            manual_layout_diagnostics,
        }
    }

    pub(crate) fn into_parts(self) -> (D, RpgMakerWriteBackSummary, Vec<ManualLayoutDiagnostic>) {
        (self.documents, self.summary, self.manual_layout_diagnostics)
    }
}

/// 使用资产读取、布局和文档改写能力准备 RPG Maker 写回候选。
pub(crate) struct RpgMakerWriteBackService<R, L, D, C> {
    asset_reader: R,
    text_layouter: Arc<L>,
    document_rewriter: D,
    cpu: Arc<C>,
    cancellation: CooperativeCancellation,
    progress: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
}

impl<R, L, D, C> RpgMakerWriteBackService<R, L, D, C> {
    pub(crate) fn new(
        asset_reader: R,
        text_layouter: L,
        document_rewriter: D,
        cpu: C,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            asset_reader,
            text_layouter: Arc::new(text_layouter),
            document_rewriter,
            cpu: Arc::new(cpu),
            cancellation,
            progress: Arc::new(NoopProgressObserver),
        }
    }

    /// 为 RPG Maker WriteBack 绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<WriteBackProgressPhase> + 'static,
    {
        self.progress = Arc::new(progress);
        self
    }
}

impl<R, L, D, C> RpgMakerWriteBack for RpgMakerWriteBackService<R, L, D, C>
where
    R: RpgMakerWriteBackAssetReader,
    L: RpgMakerWriteBackTextLayouter + 'static,
    D: RpgMakerWriteBackDocumentRewriter,
    C: CpuTaskExecutor,
{
    type Documents = D::RewrittenDocuments;
    type Error = RpgMakerWriteBackServiceError<R::Error, D::Error, C::Error>;

    async fn prepare(
        &self,
        project: &OpenedProject,
        layout_profile: &RpgMakerWriteBackLayoutProfile,
    ) -> Result<OperationCompletion<RpgMakerWriteBackPreparation<Self::Documents>>, Self::Error>
    {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.progress.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::ReadingAssets,
            0,
            1,
        ));
        let snapshot = self
            .asset_reader
            .read(project)
            .await
            .map_err(RpgMakerWriteBackServiceError::ReadAssets)?;
        self.progress.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::ReadingAssets,
            1,
            1,
        ));
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let (groups, symbol_repair) = snapshot.into_parts();
        let total_groups = u64::try_from(groups.len()).expect("写回组数量必须可表示为 u64");
        self.progress.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::PlanningTranslations,
            0,
            total_groups,
        ));
        let profile = *layout_profile;
        let planning_progress = Arc::new(PlanningProgress::new(
            Arc::clone(&self.progress),
            total_groups,
        ));
        let groups = groups
            .into_iter()
            .map(|group| ProgressTrackedPlanningJob {
                group,
                profile,
                layouter: Arc::clone(&self.text_layouter),
                symbol_repair: symbol_repair.clone(),
                cancellation: self.cancellation.clone(),
                progress: Arc::clone(&planning_progress),
            })
            .collect();
        let planned_groups = self
            .cpu
            .execute_ordered_map(groups, plan_rpg_maker_write_back_group_with_progress)
            .await
            .map_err(RpgMakerWriteBackServiceError::SchedulePlanning)?;
        let mut completed_groups = Vec::with_capacity(planned_groups.len());
        for planned in planned_groups {
            match planned.map_err(RpgMakerWriteBackServiceError::InvalidPlaceholder)? {
                OperationCompletion::Completed(planned) => completed_groups.push(planned),
                OperationCompletion::Cancelled => return Ok(OperationCompletion::Cancelled),
            }
        }
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let planned = self
            .cpu
            .execute(move || assemble_planned_rpg_maker_write_back(completed_groups))
            .await
            .map_err(RpgMakerWriteBackServiceError::SchedulePlanning)?
            .map_err(RpgMakerWriteBackServiceError::InvalidPlan)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.progress.observe(ProgressSnapshot::indeterminate(
            WriteBackProgressPhase::RewritingDocuments,
        ));
        let rewritten = self
            .document_rewriter
            .rewrite(project, planned.mutation_plan)
            .await
            .map_err(RpgMakerWriteBackServiceError::RewriteDocuments)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        Ok(OperationCompletion::Completed(
            RpgMakerWriteBackPreparation::new(
                rewritten,
                planned.summary,
                planned.manual_layout_diagnostics,
            ),
        ))
    }
}

struct PlannedRpgMakerWriteBack {
    mutation_plan: RpgMakerWriteBackMutationPlan,
    summary: RpgMakerWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

struct PlannedRpgMakerWriteBackGroup {
    mutations: Vec<RpgMakerWriteBackMutation>,
    summary: RpgMakerWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

struct ProgressTrackedPlanningJob<L> {
    group: RpgMakerWriteBackGroup,
    profile: RpgMakerWriteBackLayoutProfile,
    layouter: Arc<L>,
    symbol_repair: Option<RpgMakerWriteBackSymbolRepairContext>,
    cancellation: CooperativeCancellation,
    progress: Arc<PlanningProgress>,
}

struct PlanningProgress {
    observer: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
    completed: AtomicU64,
    last_reported: Mutex<u64>,
    total: u64,
    report_stride: u64,
}

impl PlanningProgress {
    fn new(observer: Arc<dyn ProgressObserver<WriteBackProgressPhase>>, total: u64) -> Self {
        Self {
            observer,
            completed: AtomicU64::new(0),
            last_reported: Mutex::new(0),
            total,
            report_stride: total.div_ceil(MAX_PLANNING_PROGRESS_UPDATES).max(1),
        }
    }

    fn complete(&self) {
        let completed = self
            .completed
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .expect("写回已规划组数量必须可表示为 u64");
        if completed < self.total && !completed.is_multiple_of(self.report_stride) {
            return;
        }

        let mut last_reported = self
            .last_reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observed = self.completed.load(Ordering::Acquire).min(self.total);
        if observed <= *last_reported {
            return;
        }
        *last_reported = observed;
        self.observer.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::PlanningTranslations,
            observed,
            self.total,
        ));
    }
}

fn plan_rpg_maker_write_back_group_with_progress<L>(
    job: ProgressTrackedPlanningJob<L>,
) -> Result<
    OperationCompletion<PlannedRpgMakerWriteBackGroup>,
    RpgMakerWriteBackPlaceholderValidationError,
>
where
    L: RpgMakerWriteBackTextLayouter,
{
    let planned = plan_rpg_maker_write_back_group(
        job.group,
        &job.profile,
        job.layouter.as_ref(),
        job.symbol_repair.as_ref(),
        &job.cancellation,
    );
    if matches!(planned, Ok(OperationCompletion::Completed(_))) {
        job.progress.complete();
    }
    planned
}

struct GroupPlanningOutputs<'a> {
    mutations: &'a mut Vec<RpgMakerWriteBackMutation>,
    summary: &'a mut RpgMakerWriteBackSummary,
    manual_layout_diagnostics: &'a mut Vec<ManualLayoutDiagnostic>,
}

#[cfg(test)]
fn plan_rpg_maker_write_back(
    snapshot: RpgMakerWriteBackSnapshot,
    profile: &RpgMakerWriteBackLayoutProfile,
    layouter: &impl RpgMakerWriteBackTextLayouter,
) -> PlannedRpgMakerWriteBack {
    let (groups, symbol_repair) = snapshot.into_parts();
    let groups = groups
        .into_iter()
        .map(|group| {
            match plan_rpg_maker_write_back_group(
                group,
                profile,
                layouter,
                symbol_repair.as_ref(),
                &CooperativeCancellation::default(),
            )
            .expect("测试写回快照的 Current Placeholder 必须有效")
            {
                OperationCompletion::Completed(planned) => planned,
                OperationCompletion::Cancelled => unreachable!("测试写回规划必须保持运行"),
            }
        })
        .collect();
    assemble_planned_rpg_maker_write_back(groups).expect("测试快照应产生有效 Mutation 计划")
}

fn plan_rpg_maker_write_back_group(
    group: RpgMakerWriteBackGroup,
    profile: &RpgMakerWriteBackLayoutProfile,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    symbol_repair: Option<&RpgMakerWriteBackSymbolRepairContext>,
    cancellation: &CooperativeCancellation,
) -> Result<
    OperationCompletion<PlannedRpgMakerWriteBackGroup>,
    RpgMakerWriteBackPlaceholderValidationError,
> {
    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }
    let mut mutations = Vec::new();
    let mut summary = RpgMakerWriteBackSummary::default();
    let mut manual_layout_diagnostics = Vec::new();

    {
        let mut outputs = GroupPlanningOutputs {
            mutations: &mut mutations,
            summary: &mut summary,
            manual_layout_diagnostics: &mut manual_layout_diagnostics,
        };
        for unit in &group.units {
            if cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            if unit.translation_content.is_some() {
                outputs.summary.translated_units += 1;
            } else {
                outputs.summary.original_units += 1;
            }
        }

        let (owner, kind, group_location, mut units, recipes, mutation_claims) = group.into_parts();
        if let Some(symbol_repair) = symbol_repair {
            for unit in &mut units {
                let repaired = repair_unit_translation_symbols_with_cancellation(
                    unit,
                    kind,
                    symbol_repair,
                    cancellation,
                )
                .map_err(|reason| {
                    RpgMakerWriteBackPlaceholderValidationError::new(
                        owner,
                        kind,
                        group_location.clone(),
                        unit.role.clone(),
                        reason,
                    )
                })?;
                let OperationCompletion::Completed(repaired) = repaired else {
                    return Ok(OperationCompletion::Cancelled);
                };
                outputs.summary.symbol_repair_attempted_units += repaired.attempted_units;
                outputs.summary.symbol_repair_repaired_units += repaired.repaired_units;
                outputs.summary.symbol_repair_skipped_units += repaired.skipped_units;
                outputs.summary.symbol_repair_replacements += repaired.replacements;
            }
        }
        if cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        match kind {
            TextGroupKind::EventDialogue => plan_dialogue_group(
                group_location,
                units,
                recipes,
                mutation_claims,
                profile.dialogue_body(),
                layouter,
                &mut outputs,
            ),
            TextGroupKind::EventScrollingText => plan_scrolling_group(
                group_location,
                units,
                recipes,
                mutation_claims,
                profile.scrolling_text(),
                layouter,
                &mut outputs,
            ),
            TextGroupKind::EventChoices => plan_choices_group(
                group_location,
                units,
                recipes,
                mutation_claims,
                &mut outputs,
            ),
            _ => plan_scalar_group(
                kind,
                group_location,
                units,
                recipes,
                profile.help_description(),
                layouter,
                &mut outputs,
            ),
        }
        if cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        outputs.summary.manual_layout_units = outputs.manual_layout_diagnostics.len();
    }

    Ok(OperationCompletion::Completed(
        PlannedRpgMakerWriteBackGroup {
            mutations,
            summary,
            manual_layout_diagnostics,
        },
    ))
}

fn assemble_planned_rpg_maker_write_back(
    groups: Vec<PlannedRpgMakerWriteBackGroup>,
) -> Result<PlannedRpgMakerWriteBack, RpgMakerWriteBackMutationPlanError> {
    let mut mutations = Vec::new();
    let mut summary = RpgMakerWriteBackSummary::default();
    let mut manual_layout_diagnostics = Vec::new();
    for group in groups {
        mutations.extend(group.mutations);
        merge_rpg_maker_write_back_summary(&mut summary, group.summary);
        manual_layout_diagnostics.extend(group.manual_layout_diagnostics);
    }
    summary.manual_layout_units = manual_layout_diagnostics.len();

    let mutation_plan = RpgMakerWriteBackMutationPlan::new(mutations)?;
    Ok(PlannedRpgMakerWriteBack {
        mutation_plan,
        summary,
        manual_layout_diagnostics,
    })
}

fn merge_rpg_maker_write_back_summary(
    total: &mut RpgMakerWriteBackSummary,
    group: RpgMakerWriteBackSummary,
) {
    total.translated_units += group.translated_units;
    total.original_units += group.original_units;
    total.auto_wrapped_units += group.auto_wrapped_units;
    total.inserted_line_breaks += group.inserted_line_breaks;
    total.inserted_fullwidth_indents += group.inserted_fullwidth_indents;
    total.symbol_repair_attempted_units += group.symbol_repair_attempted_units;
    total.symbol_repair_repaired_units += group.symbol_repair_repaired_units;
    total.symbol_repair_skipped_units += group.symbol_repair_skipped_units;
    total.symbol_repair_replacements += group.symbol_repair_replacements;
}

fn plan_dialogue_group(
    group_location: RpgMakerLocation,
    units: Vec<RpgMakerWriteBackUnit>,
    mut recipes: Vec<TextProjectionRecipe>,
    mutation_claims: MutationClaimSet,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    if !units.iter().any(|unit| unit.translation_content.is_some()) {
        return;
    }
    let TextProjectionRecipe::Dialogue(recipe) = recipes.pop().expect("受信对话组必须包含唯一配方")
    else {
        unreachable!("受信对话组必须包含 Dialogue 配方")
    };
    debug_assert!(recipes.is_empty());

    let units = units
        .into_iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    let speaker_unit = units.get(&TextUnitRole::DialogueSpeaker);
    let source_speaker = speaker_unit.map(|unit| {
        unit.source_content
            .as_value()
            .expect("受信 Speaker 原文必须是单值")
            .to_owned()
    });
    let speaker = speaker_unit.map(|unit| {
        unit.effective_content()
            .as_value()
            .expect("受信 Speaker 必须是单值")
            .to_owned()
    });
    let body = units.get(&TextUnitRole::DialogueBody);
    let body_lines = if body.is_some_and(|unit| unit.translation_content.is_some()) {
        let body = body.expect("已经确认 Body 存在");
        let exact_location =
            dialogue_body_location(&recipe).expect("受信对话正文配方必须引用至少一个 BodyLine");
        let request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            max_fullwidth_chars,
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &group_location,
                body,
                exact_location.clone(),
            )],
        );
        Some(
            layout_replacements(&request, layouter, outputs)
                .remove(&exact_location)
                .expect("布局结果必须覆盖对话正文语义单元"),
        )
    } else {
        None
    };

    let mutation = ReplaceDialogueMutation::new_with_claims(
        recipe,
        mutation_claims,
        source_speaker,
        speaker,
        body_lines,
    )
    .expect("受信对话资产必须建立合法原子 Mutation");
    outputs
        .mutations
        .push(RpgMakerWriteBackMutation::ReplaceDialogue(mutation));
}

fn plan_scrolling_group(
    group_location: RpgMakerLocation,
    units: Vec<RpgMakerWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    mutation_claims: MutationClaimSet,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let [unit] = units.as_slice() else {
        unreachable!("受信滚动文本组必须只包含一个语义单元")
    };
    let Some(replacement_lines) = aligned_replacement_lines(unit) else {
        return;
    };
    let source_lines = unit
        .source_content
        .as_lines()
        .expect("受信滚动文本原文必须是行序列");
    let entries = recipes
        .iter()
        .map(|recipe| {
            let TextProjectionRecipe::Direct(recipe) = recipe else {
                unreachable!("受信滚动文本组只包含直接配方")
            };
            let [
                DirectTextPart::LineSlot {
                    role: TextUnitRole::ScrollingText,
                    source_line_index,
                },
            ] = recipe.parts()
            else {
                unreachable!("受信滚动文本配方必须只包含一个行槽")
            };
            (recipe, *source_line_index)
        })
        .collect::<Vec<_>>();

    let request = RpgMakerWriteBackLayoutRequest::new(
        RpgMakerWriteBackLayoutRegion::ScrollingText,
        max_fullwidth_chars,
        entries
            .iter()
            .map(|(recipe, source_line_index)| {
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &group_location,
                    TextUnitRole::ScrollingText,
                    recipe.target().clone(),
                    source_lines[*source_line_index].clone(),
                    Some(replacement_lines[*source_line_index].clone()),
                )
            })
            .collect(),
    );
    let replacements = layout_replacements(&request, layouter, outputs);
    let segments = entries
        .into_iter()
        .map(|(recipe, _)| {
            let lines = replacements
                .get(recipe.target())
                .expect("受信布局结果必须覆盖每个滚动文本语义行")
                .iter()
                .map(|line| line.text().to_owned())
                .collect();
            EventBodyMutationSegment::replace(
                recipe.target().clone(),
                recipe.expected_raw().to_owned(),
                lines,
            )
        })
        .collect();
    let mutation =
        ReplaceEventBodyMutation::new_with_claims(group_location, segments, mutation_claims)
            .expect("受信滚动正文应建立合法块级 Mutation");
    outputs
        .mutations
        .push(RpgMakerWriteBackMutation::ReplaceEventBody(mutation));
}

fn plan_choices_group(
    group_location: RpgMakerLocation,
    units: Vec<RpgMakerWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    mutation_claims: MutationClaimSet,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let [unit] = units.as_slice() else {
        unreachable!("受信选项组必须只包含一个语义单元")
    };
    let Some(replacement_lines) = aligned_replacement_lines(unit) else {
        return;
    };
    let source_lines = unit
        .source_content
        .as_lines()
        .expect("受信选项原文必须是行序列");
    let recipes = recipes
        .into_iter()
        .filter_map(|recipe| match recipe {
            TextProjectionRecipe::Direct(recipe) => Some(recipe),
            TextProjectionRecipe::Dialogue(_) => unreachable!("受信选项组只包含直接配方"),
            TextProjectionRecipe::Claim(_) => None,
        })
        .collect();
    let mutation = ReplaceChoicesMutation::new_with_claims(
        group_location,
        recipes,
        mutation_claims,
        source_lines.to_vec(),
        replacement_lines,
    )
    .expect("受信选项资产必须建立合法原子 Mutation");
    outputs
        .mutations
        .push(RpgMakerWriteBackMutation::ReplaceChoices(mutation));
}

fn plan_scalar_group(
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    units: Vec<RpgMakerWriteBackUnit>,
    recipes: Vec<TextProjectionRecipe>,
    help_max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) {
    let units = units
        .into_iter()
        .map(|unit| (unit.role.clone(), unit))
        .collect::<BTreeMap<_, _>>();
    for recipe in recipes {
        let TextProjectionRecipe::Direct(recipe) = recipe else {
            unreachable!("受信普通文本组只包含直接配方")
        };
        let roles = recipe
            .parts()
            .iter()
            .filter_map(|part| match part {
                DirectTextPart::Literal(_) => None,
                DirectTextPart::TextSlot { role } | DirectTextPart::LineSlot { role, .. } => {
                    Some(role)
                }
            })
            .collect::<Vec<_>>();
        if !roles.iter().any(|role| {
            units
                .get(*role)
                .is_some_and(|unit| unit.translation_content.is_some())
        }) {
            continue;
        }

        let mut overrides = BTreeMap::new();
        if roles.len() == 1 {
            let role = roles[0];
            let unit = units.get(role).expect("受信配方角色必须存在语义单元");
            if unit.translation_content.is_some()
                && is_canonical_help_description(kind, unit, &recipe)
            {
                let request = RpgMakerWriteBackLayoutRequest::new(
                    RpgMakerWriteBackLayoutRegion::HelpDescription,
                    help_max_fullwidth_chars,
                    vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                        &group_location,
                        unit,
                        recipe.target().clone(),
                    )],
                );
                let replacement = layout_replacements(&request, layouter, outputs)
                    .remove(recipe.target())
                    .expect("帮助说明布局必须返回唯一译文单元")
                    .iter()
                    .map(RpgMakerWriteBackLaidOutLine::text)
                    .collect::<Vec<_>>()
                    .join("\n");
                overrides.insert(role.clone(), replacement);
            }
        }

        let replacement = render_direct_recipe(&recipe, &units, &overrides);
        outputs.mutations.push(RpgMakerWriteBackMutation::SetText(
            SetTextMutation::from_recipe(&recipe, replacement),
        ));
    }
}

fn layout_replacements(
    request: &RpgMakerWriteBackLayoutRequest,
    layouter: &impl RpgMakerWriteBackTextLayouter,
    outputs: &mut GroupPlanningOutputs<'_>,
) -> BTreeMap<RpgMakerLocation, Vec<RpgMakerWriteBackLaidOutLine>> {
    match layouter.layout(request) {
        RpgMakerWriteBackLayoutOutcome::Applied(applied) => {
            let (segments, inserted_line_breaks, inserted_fullwidth_indents) = applied.into_parts();
            record_applied_layout(
                outputs.summary,
                inserted_line_breaks,
                inserted_fullwidth_indents,
            );
            segments
                .into_iter()
                .map(|segment| (segment.exact_location, segment.lines))
                .collect()
        }
        RpgMakerWriteBackLayoutOutcome::Manual => {
            outputs
                .manual_layout_diagnostics
                .push(ManualLayoutDiagnostic::from_request(request));
            request
                .segments
                .iter()
                .filter_map(|segment| match &segment.candidate {
                    RpgMakerWriteBackLayoutCandidate::FrozenOriginal => None,
                    RpgMakerWriteBackLayoutCandidate::DatabaseTranslation(translation) => Some((
                        segment.exact_location.clone(),
                        split_hard_lines(translation)
                            .into_iter()
                            .enumerate()
                            .map(|(source_semantic_line_index, text)| {
                                RpgMakerWriteBackLaidOutLine::new(text, source_semantic_line_index)
                            })
                            .collect(),
                    )),
                })
                .collect()
        }
    }
}

fn dialogue_body_location(recipe: &DialogueWriteRecipe) -> Option<RpgMakerLocation> {
    recipe.lines().iter().find_map(|line| {
        line.parts()
            .iter()
            .any(|part| matches!(part, DialogueLinePart::BodyLine { .. }))
            .then(|| line.physical_location().clone())
    })
}

fn render_direct_recipe(
    recipe: &DirectTextRecipe,
    units: &BTreeMap<TextUnitRole, RpgMakerWriteBackUnit>,
    overrides: &BTreeMap<TextUnitRole, String>,
) -> String {
    let mut rendered = String::new();
    for part in recipe.parts() {
        match part {
            DirectTextPart::Literal(value) => rendered.push_str(value),
            DirectTextPart::TextSlot { role } => {
                if let Some(value) = overrides.get(role) {
                    rendered.push_str(value);
                } else {
                    rendered.push_str(
                        units
                            .get(role)
                            .expect("受信直接配方角色必须存在语义单元")
                            .effective_content()
                            .as_value()
                            .expect("TextSlot 必须引用单值内容"),
                    );
                }
            }
            DirectTextPart::LineSlot {
                role,
                source_line_index,
            } => rendered.push_str(
                units
                    .get(role)
                    .expect("受信直接配方角色必须存在语义单元")
                    .effective_content()
                    .as_lines()
                    .and_then(|lines| lines.get(*source_line_index))
                    .expect("LineSlot 必须引用存在的语义行"),
            ),
        }
    }
    rendered
}

fn record_applied_layout(
    summary: &mut RpgMakerWriteBackSummary,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
) {
    if inserted_line_breaks > 0 {
        summary.auto_wrapped_units += 1;
    }
    summary.inserted_line_breaks += inserted_line_breaks;
    summary.inserted_fullwidth_indents += inserted_fullwidth_indents;
}

fn split_hard_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

fn is_canonical_help_description(
    kind: TextGroupKind,
    unit: &RpgMakerWriteBackUnit,
    recipe: &DirectTextRecipe,
) -> bool {
    if kind != TextGroupKind::DatabaseEntry {
        return false;
    }
    if !matches!(
        &unit.role,
        TextUnitRole::Scalar(field_name) if field_name.as_str() == "description"
    ) {
        return false;
    }
    let RpgMakerSource::Data(file) = recipe.target().source() else {
        return false;
    };
    if !matches!(
        file,
        StandardDataFile::Skills
            | StandardDataFile::Items
            | StandardDataFile::Weapons
            | StandardDataFile::Armors
    ) {
        return false;
    }
    matches!(
        recipe.target().steps(),
        [RpgMakerLocationStep::ArrayIndex(_), RpgMakerLocationStep::ObjectKey(field_name)]
            if field_name == "description"
    )
}

/// RPG Maker 在资产读取和文档改写边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackServiceError<R, D, C> {
    ReadAssets(R),
    SchedulePlanning(CpuTaskExecutionError<C>),
    InvalidPlaceholder(RpgMakerWriteBackPlaceholderValidationError),
    InvalidPlan(RpgMakerWriteBackMutationPlanError),
    RewriteDocuments(D),
}

impl<R, D, C> fmt::Display for RpgMakerWriteBackServiceError<R, D, C>
where
    R: fmt::Display,
    D: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadAssets(source) => write!(formatter, "读取 RPG Maker 写回资产失败：{source}"),
            Self::SchedulePlanning(source) => {
                write!(formatter, "调度 RPG Maker 写回规划失败：{source}")
            }
            Self::InvalidPlaceholder(source) => {
                write!(formatter, "RPG Maker 写回前 Placeholder 验收失败：{source}")
            }
            Self::InvalidPlan(source) => write!(formatter, "RPG Maker 写回规划无效：{source}"),
            Self::RewriteDocuments(source) => {
                write!(formatter, "改写 RPG Maker 文档失败：{source}")
            }
        }
    }
}

impl<R, D, C> Error for RpgMakerWriteBackServiceError<R, D, C>
where
    R: Error + 'static,
    D: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadAssets(source) => Some(source),
            Self::SchedulePlanning(source) => Some(source),
            Self::InvalidPlaceholder(source) => Some(source),
            Self::InvalidPlan(source) => Some(source),
            Self::RewriteDocuments(source) => Some(source),
        }
    }
}

pub(crate) fn write_back_planning_compute_report(
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> DiagnosticReport {
    let failure = match source {
        CpuTaskExecutionError::Cancelled => RpgMakerComputeFailure::Cancelled,
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown) => {
            RpgMakerComputeFailure::ExecutorClosed
        }
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned) => {
            RpgMakerComputeFailure::StatePoisoned
        }
        CpuTaskExecutionError::TaskPanicked => RpgMakerComputeFailure::WorkerPanicked,
    };
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::write_back_planning(
            RpgMakerWriteBackPlanningProblem::Compute { failure },
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressAmount;
    use crate::rpg_maker::model::{
        DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget,
        ScalarFieldKey,
    };
    use crate::rpg_maker::project::MaxFullwidthChars;
    use crate::rpg_maker::translate::placeholder::PlaceholderRuleDefinition;

    #[derive(Clone, Default)]
    struct RecordingPlanningProgress(Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>);

    impl ProgressObserver<WriteBackProgressPhase> for RecordingPlanningProgress {
        fn observe(&self, snapshot: ProgressSnapshot<WriteBackProgressPhase>) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(snapshot);
        }
    }

    fn symbol_repair_context(
        definitions: Vec<PlaceholderRuleDefinition>,
    ) -> RpgMakerWriteBackSymbolRepairContext {
        let placeholder_rules_json =
            serde_json::to_string(&definitions).expect("测试 Placeholder 规则应可编码");
        let placeholder_service =
            Pcre2PlaceholderService::new().expect("内建 Placeholder 应可编译");
        let placeholder_rules = placeholder_service
            .compile_custom(definitions)
            .expect("测试自定义 Placeholder 应可编译");
        RpgMakerWriteBackSymbolRepairContext::new(
            RpgMakerEngine::Mz,
            placeholder_service,
            placeholder_rules,
            placeholder_rules_json,
        )
    }

    #[test]
    fn symbol_repair_fixes_categories_and_one_sided_quotes() {
        let context = symbol_repair_context(Vec::new());
        let mut categories = RpgMakerWriteBackUnit::new(
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("General, Misc, Audio, Toggle".to_owned()),
            Some(TextUnitContent::Value("常规、杂项、声音、开关".to_owned())),
        )
        .expect("测试单值单元应有效");
        let categories_statistics = repair_unit_translation_symbols(
            &mut categories,
            TextGroupKind::DatabaseEntry,
            &context,
        )
        .expect("Categories Current 的 Placeholder 应有效");

        assert_eq!(
            categories.translation_content,
            Some(TextUnitContent::Value("常规,杂项,声音,开关".to_owned()))
        );
        assert_eq!(
            categories_statistics,
            SymbolRepairStatistics {
                attempted_units: 1,
                repaired_units: 1,
                skipped_units: 0,
                replacements: 3,
            }
        );

        let mut quote = RpgMakerWriteBackUnit::new(
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("「Open」".to_owned()),
            Some(TextUnitContent::Value("“打开」".to_owned())),
        )
        .expect("测试引号单元应有效");
        let quote_statistics =
            repair_unit_translation_symbols(&mut quote, TextGroupKind::DatabaseEntry, &context)
                .expect("引号 Current 的 Placeholder 应有效");

        assert_eq!(
            quote.translation_content,
            Some(TextUnitContent::Value("「打开」".to_owned()))
        );
        assert_eq!(quote_statistics.replacements, 1);
    }

    #[test]
    fn symbol_repair_uses_choice_items_and_joined_line_units_at_their_required_boundaries() {
        let context = symbol_repair_context(Vec::new());
        let mut choices = RpgMakerWriteBackUnit::new(
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["A, B".to_owned(), "C.".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "甲、乙".to_owned(),
                "丙。".to_owned(),
            ])),
        )
        .expect("测试选项单元应有效");
        let choices_statistics =
            repair_unit_translation_symbols(&mut choices, TextGroupKind::EventChoices, &context)
                .expect("选项 Current 的 Placeholder 应有效");
        assert_eq!(
            choices.translation_content,
            Some(TextUnitContent::Lines(vec![
                "甲,乙".to_owned(),
                "丙.".to_owned(),
            ]))
        );
        assert_eq!(choices_statistics.attempted_units, 1);
        assert_eq!(choices_statistics.repaired_units, 1);
        assert_eq!(choices_statistics.replacements, 2);

        let mut dialogue = RpgMakerWriteBackUnit::new(
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["A,".to_owned(), "B.".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "甲、".to_owned(),
                "乙。".to_owned(),
            ])),
        )
        .expect("测试对话单元应有效");
        let dialogue_statistics =
            repair_unit_translation_symbols(&mut dialogue, TextGroupKind::EventDialogue, &context)
                .expect("对话 Current 的 Placeholder 应有效");
        assert_eq!(
            dialogue.translation_content,
            Some(TextUnitContent::Lines(vec![
                "甲,".to_owned(),
                "乙.".to_owned()
            ]))
        );
        assert_eq!(dialogue_statistics.replacements, 2);

        let mut scrolling = RpgMakerWriteBackUnit::new(
            TextUnitRole::ScrollingText,
            TextUnitContent::Lines(vec!["A,".to_owned(), "B".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "甲".to_owned(),
                "乙、".to_owned(),
            ])),
        )
        .expect("测试滚动文本单元应有效");
        let scrolling_statistics = repair_unit_translation_symbols(
            &mut scrolling,
            TextGroupKind::EventScrollingText,
            &context,
        )
        .expect("滚动文本 Current 的 Placeholder 应有效");
        assert_eq!(
            scrolling.translation_content,
            Some(TextUnitContent::Lines(vec![
                "甲".to_owned(),
                "乙、".to_owned(),
            ]))
        );
        assert_eq!(scrolling_statistics.replacements, 0);
    }

    #[test]
    fn symbol_repair_validates_cross_line_choice_placeholders_before_item_repair() {
        let context = symbol_repair_context(vec![PlaceholderRuleDefinition::new(
            Some(vec!["event_choices".to_owned()]),
            r"(?s)<msg>(?<text>.*?)</msg>",
        )]);
        let mut choices = RpgMakerWriteBackUnit::new(
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["A,".to_owned(), "B.".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "<msg>甲、".to_owned(),
                "乙。</msg>".to_owned(),
            ])),
        )
        .expect("测试选项单元应有效");

        let failure =
            repair_unit_translation_symbols(&mut choices, TextGroupKind::EventChoices, &context)
                .expect_err("译文新增跨槽 Placeholder 必须明确失败");

        assert!(matches!(
            failure,
            TranslationPlanningFailureReason::PlaceholderProjection {
                failure: TranslationPlaceholderProjectionFailure::ChangedSegmentCount {
                    expected: 0,
                    actual: 2,
                }
            }
        ));
    }

    #[test]
    fn symbol_repair_rejects_a_placeholder_moved_to_another_choice_slot() {
        let context = symbol_repair_context(Vec::new());
        let mut choices = RpgMakerWriteBackUnit::new(
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["\\V[1] A,".to_owned(), "B.".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "甲，".to_owned(),
                "\\V[1]乙。".to_owned(),
            ])),
        )
        .expect("测试选项单元应有效");

        let failure =
            repair_unit_translation_symbols(&mut choices, TextGroupKind::EventChoices, &context)
                .expect_err("Placeholder 不得移动到另一个选项槽位");

        assert!(matches!(
            failure,
            TranslationPlanningFailureReason::PlaceholderProjection {
                failure: TranslationPlaceholderProjectionFailure::ChangedSegmentCount {
                    expected: 1,
                    actual: 0,
                }
            }
        ));
    }

    #[test]
    fn symbol_repair_preserves_rpg_controls_custom_wrappers_and_recipe_literals() {
        let context = symbol_repair_context(vec![PlaceholderRuleDefinition::new(
            Some(vec!["event_dialogue".to_owned()]),
            r"<msg>(?<text>.*?)</msg>",
        )]);
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::DialogueBody,
            TextUnitContent::Value(r"\N[1] <msg>Open \C[2], now.</msg> \n<Hero>".to_owned()),
            Some(TextUnitContent::Value(
                r"\N[1] <msg>打开 \C[2]、现在。</msg> \n<Hero>".to_owned(),
            )),
        )
        .expect("测试控制符单元应有效");
        let statistics =
            repair_unit_translation_symbols(&mut unit, TextGroupKind::EventDialogue, &context)
                .expect("控制符 Current 的 Placeholder 应有效");
        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Value(
                r"\N[1] <msg>打开 \C[2],现在.</msg> \n<Hero>".to_owned(),
            ))
        );
        assert_eq!(statistics.replacements, 2);

        let multiline_context = symbol_repair_context(vec![PlaceholderRuleDefinition::new(
            Some(vec!["event_dialogue".to_owned()]),
            r"(?s)<raw>.*?</raw>",
        )]);
        let mut multiline_value = RpgMakerWriteBackUnit::new(
            TextUnitRole::DialogueBody,
            TextUnitContent::Value("A, <raw>fixed\nbytes</raw>, B.".to_owned()),
            Some(TextUnitContent::Value(
                "甲、<raw>fixed\nbytes</raw>、乙。".to_owned(),
            )),
        )
        .expect("跨 LF 的 Value Placeholder 单元应有效");
        let multiline_statistics = repair_unit_translation_symbols(
            &mut multiline_value,
            TextGroupKind::EventDialogue,
            &multiline_context,
        )
        .expect("跨 LF Current 的 Placeholder 应有效");
        assert_eq!(
            multiline_value.translation_content,
            Some(TextUnitContent::Value(
                "甲,<raw>fixed\nbytes</raw>,乙.".to_owned(),
            ))
        );
        assert_eq!(multiline_statistics.replacements, 3);

        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"));
        let mut literal_unit = RpgMakerWriteBackUnit::new(
            role.clone(),
            TextUnitContent::Value("General, Misc".to_owned()),
            Some(TextUnitContent::Value("常规、杂项".to_owned())),
        )
        .expect("测试 Literal 单元应有效");
        repair_unit_translation_symbols(
            &mut literal_unit,
            TextGroupKind::DatabaseEntry,
            &symbol_repair_context(Vec::new()),
        )
        .expect("Literal 测试 Current 的 Placeholder 应有效");
        let units = BTreeMap::from([(role.clone(), literal_unit)]);
        let recipe = DirectTextRecipe::new(
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key("name"),
                ],
            ),
            "【General, Misc】、固定",
            vec![
                DirectTextPart::Literal("【".to_owned()),
                DirectTextPart::TextSlot { role },
                DirectTextPart::Literal("】、固定".to_owned()),
            ],
        )
        .expect("测试直接配方应有效");

        assert_eq!(
            render_direct_recipe(&recipe, &units, &BTreeMap::new()),
            "【常规,杂项】、固定"
        );
    }

    #[test]
    fn symbol_repair_rejects_a_current_with_new_placeholder_bindings() {
        let context = symbol_repair_context(vec![PlaceholderRuleDefinition::new(
            Some(vec!["database_entry".to_owned()]),
            r"\[[^]]+\]",
        )]);
        let original_translation = "[常规、杂项]".to_owned();
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("General, Misc".to_owned()),
            Some(TextUnitContent::Value(original_translation.clone())),
        )
        .expect("测试 Current 单元应有效");

        let failure =
            repair_unit_translation_symbols(&mut unit, TextGroupKind::DatabaseEntry, &context)
                .expect_err("新增 Placeholder 的 Current 必须明确失败");

        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Value(original_translation))
        );
        assert!(matches!(
            failure,
            TranslationPlanningFailureReason::PlaceholderProjection {
                failure: TranslationPlaceholderProjectionFailure::ChangedSegmentCount {
                    expected: 0,
                    actual: 1,
                }
            }
        ));
    }

    #[test]
    fn symbol_repair_skips_value_when_rebuilt_text_cannot_be_allocated() {
        let context = symbol_repair_context(Vec::new());
        let original_translation = "甲、乙".to_owned();
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("A, B".to_owned()),
            Some(TextUnitContent::Value(original_translation.clone())),
        )
        .expect("测试单值单元应有效");

        let completion = repair_unit_translation_symbols_with_allocation(
            &mut unit,
            TextGroupKind::DatabaseEntry,
            &context,
            &CooperativeCancellation::default(),
            SymbolRepairAllocation::failing(),
        )
        .expect("分配失败应作为内部跳过处理");

        assert_eq!(
            completion,
            OperationCompletion::Completed(SymbolRepairStatistics {
                attempted_units: 1,
                repaired_units: 0,
                skipped_units: 1,
                replacements: 0,
            })
        );
        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Value(original_translation))
        );
    }

    #[test]
    fn symbol_repair_skips_choices_when_line_cloning_cannot_be_allocated() {
        let context = symbol_repair_context(Vec::new());
        let original_translation = vec!["甲、乙".to_owned(), "丙。".to_owned()];
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["A, B".to_owned(), "C.".to_owned()]),
            Some(TextUnitContent::Lines(original_translation.clone())),
        )
        .expect("测试选项单元应有效");

        let completion = repair_unit_translation_symbols_with_allocation(
            &mut unit,
            TextGroupKind::EventChoices,
            &context,
            &CooperativeCancellation::default(),
            SymbolRepairAllocation::failing(),
        )
        .expect("分配失败应作为内部跳过处理");

        assert_eq!(
            completion,
            OperationCompletion::Completed(SymbolRepairStatistics {
                attempted_units: 1,
                repaired_units: 0,
                skipped_units: 1,
                replacements: 0,
            })
        );
        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Lines(original_translation))
        );
    }

    #[test]
    fn symbol_repair_skips_joined_lines_when_join_cannot_be_allocated() {
        let context = symbol_repair_context(Vec::new());
        let original_translation = vec!["甲、".to_owned(), "乙。".to_owned()];
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["A,".to_owned(), "B.".to_owned()]),
            Some(TextUnitContent::Lines(original_translation.clone())),
        )
        .expect("测试对话单元应有效");

        let completion = repair_unit_translation_symbols_with_allocation(
            &mut unit,
            TextGroupKind::EventDialogue,
            &context,
            &CooperativeCancellation::default(),
            SymbolRepairAllocation::failing(),
        )
        .expect("分配失败应作为内部跳过处理");

        assert_eq!(
            completion,
            OperationCompletion::Completed(SymbolRepairStatistics {
                attempted_units: 1,
                repaired_units: 0,
                skipped_units: 1,
                replacements: 0,
            })
        );
        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Lines(original_translation))
        );
    }

    #[test]
    fn symbol_repair_reports_requested_cancellation_before_allocation_failure() {
        let context = symbol_repair_context(Vec::new());
        let original_translation = "甲、乙".to_owned();
        let mut unit = RpgMakerWriteBackUnit::new(
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("A, B".to_owned()),
            Some(TextUnitContent::Value(original_translation.clone())),
        )
        .expect("测试单值单元应有效");
        let cancellation = CooperativeCancellation::default();
        cancellation.request();

        let completion = repair_unit_translation_symbols_with_allocation(
            &mut unit,
            TextGroupKind::DatabaseEntry,
            &context,
            &cancellation,
            SymbolRepairAllocation::failing(),
        )
        .expect("取消应作为正常终态返回");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(
            unit.translation_content,
            Some(TextUnitContent::Value(original_translation))
        );
    }

    #[test]
    fn symbol_repair_joined_length_rejects_overflow() {
        assert_eq!(
            checked_joined_symbol_repair_length(
                [usize::MAX, 1],
                0,
                &CooperativeCancellation::default(),
            ),
            OperationCompletion::Completed(None)
        );
    }

    #[test]
    fn large_parallel_planning_coalesces_progress_and_keeps_the_exact_final_count() {
        const TOTAL: u64 = 217_000;
        const WORKERS: usize = 8;

        let observer = RecordingPlanningProgress::default();
        let progress = Arc::new(PlanningProgress::new(Arc::new(observer.clone()), TOTAL));
        let next = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                let progress = Arc::clone(&progress);
                let next = &next;
                scope.spawn(move || {
                    loop {
                        if next.fetch_add(1, Ordering::Relaxed) >= TOTAL {
                            break;
                        }
                        progress.complete();
                    }
                });
            }
        });

        let snapshots = observer
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            snapshots.len() <= MAX_PLANNING_PROGRESS_UPDATES as usize,
            "大量 Group 不得退化为逐组锁和逐组进度事件：{}",
            snapshots.len()
        );
        let counts = snapshots
            .iter()
            .map(|snapshot| match snapshot.amount {
                ProgressAmount::Determinate { completed, total }
                    if snapshot.phase == WriteBackProgressPhase::PlanningTranslations =>
                {
                    assert_eq!(total, TOTAL);
                    completed
                }
                _ => panic!("规划计数只能发布确定型 PlanningTranslations 快照"),
            })
            .collect::<Vec<_>>();
        assert!(counts.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(counts.last(), Some(&TOTAL));
    }

    fn location(command_index: usize, parameter_index: Option<usize>) -> RpgMakerLocation {
        let mut steps = vec![
            RpgMakerLocationStep::key("list"),
            RpgMakerLocationStep::index(command_index),
        ];
        if let Some(parameter_index) = parameter_index {
            steps.extend([
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(parameter_index),
            ]);
        }
        RpgMakerLocation::value(RpgMakerSource::map(1), steps)
    }

    fn profile() -> RpgMakerWriteBackLayoutProfile {
        let width = MaxFullwidthChars::new(40).expect("测试行宽应合法");
        RpgMakerWriteBackLayoutProfile::new(width, width, width)
    }

    fn recipe_locks(
        kind: TextGroupKind,
        group_location: &RpgMakerLocation,
        recipes: &[TextProjectionRecipe],
    ) -> Vec<MutationResourceLock> {
        mutation_claims_for_group(kind, group_location, recipes)
            .expect("测试配方应形成无冲突 Claim")
            .locks()
            .to_vec()
    }

    /// 测试专用的旧式完整字符串重建，用来锁定游标实现的现行语义。
    fn reference_validate_projection_round_trip(
        group_location: &RpgMakerLocation,
        units: &[RpgMakerWriteBackUnit],
        recipes: &[TextProjectionRecipe],
    ) -> Result<(), RpgMakerWriteBackSnapshotError> {
        let units = units
            .iter()
            .map(|unit| (unit.role.clone(), unit))
            .collect::<BTreeMap<_, _>>();
        for recipe in recipes {
            match recipe {
                TextProjectionRecipe::Direct(recipe) => {
                    let mut rebuilt = String::new();
                    for part in recipe.parts() {
                        match part {
                            DirectTextPart::Literal(value) => rebuilt.push_str(value),
                            DirectTextPart::TextSlot { role } => rebuilt.push_str(
                                units
                                    .get(role)
                                    .and_then(|unit| unit.source_content.as_value())
                                    .ok_or_else(|| {
                                        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(recipe.target().clone()),
                                        }
                                    })?,
                            ),
                            DirectTextPart::LineSlot {
                                role,
                                source_line_index,
                            } => {
                                let line = units
                                    .get(role)
                                    .and_then(|unit| unit.source_content.as_lines())
                                    .and_then(|lines| lines.get(*source_line_index))
                                    .ok_or_else(|| {
                                        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                            group_location: Box::new(group_location.clone()),
                                            target: Box::new(recipe.target().clone()),
                                        }
                                    })?;
                                rebuilt.push_str(line);
                            }
                        }
                    }
                    if rebuilt != recipe.expected_raw() {
                        return Err(
                            RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                group_location: Box::new(group_location.clone()),
                                target: Box::new(recipe.target().clone()),
                            },
                        );
                    }
                }
                TextProjectionRecipe::Dialogue(recipe) => {
                    let speaker = units
                        .get(&TextUnitRole::DialogueSpeaker)
                        .and_then(|unit| unit.source_content.as_value());
                    if let Some(target) = recipe.direct_speaker()
                        && speaker != Some(target.expected_raw())
                    {
                        return Err(
                            RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                group_location: Box::new(group_location.clone()),
                                target: Box::new(target.physical_location().clone()),
                            },
                        );
                    }
                    for line in recipe.lines() {
                        let mut rebuilt = String::new();
                        for (part_index, part) in line.parts().iter().enumerate() {
                            match part {
                                DialogueLinePart::Literal(value) => rebuilt.push_str(value),
                                DialogueLinePart::SpeakerSlot => rebuilt.push_str(speaker.expect(
                                    "调用前已经确认内嵌 SpeakerSlot 对应逻辑 Speaker 单元",
                                )),
                                DialogueLinePart::BodyLine { source_line_index } => {
                                    if part_index + 1 != line.parts().len() {
                                        return Err(
                                            RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                                group_location: Box::new(group_location.clone()),
                                                target: Box::new(line.physical_location().clone()),
                                            },
                                        );
                                    }
                                    rebuilt.push_str(
                                        units
                                            .get(&TextUnitRole::DialogueBody)
                                            .and_then(|unit| unit.source_content.as_lines())
                                            .and_then(|lines| lines.get(*source_line_index))
                                            .ok_or_else(|| RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                                group_location: Box::new(group_location.clone()),
                                                target: Box::new(line.physical_location().clone()),
                                            })?,
                                    );
                                }
                            }
                        }
                        if rebuilt != line.expected_raw() {
                            return Err(
                                RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                                    group_location: Box::new(group_location.clone()),
                                    target: Box::new(line.physical_location().clone()),
                                },
                            );
                        }
                    }
                }
                TextProjectionRecipe::Claim(_) => {}
            }
        }
        Ok(())
    }

    #[test]
    fn projection_round_trip_cursor_matches_the_rebuilding_reference() {
        let group_location = location(100, None);
        let scalar_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("unicode_scalar").expect("测试标量键应合法"));
        let line_role = TextUnitRole::Choices;
        let units = vec![
            RpgMakerWriteBackUnit::new(
                scalar_role.clone(),
                TextUnitContent::Value("甲🙂".to_owned()),
                None,
            )
            .expect("测试标量单元应合法"),
            RpgMakerWriteBackUnit::new(
                line_role.clone(),
                TextUnitContent::Lines(vec!["第一行".to_owned(), "第二行🌏".to_owned()]),
                None,
            )
            .expect("测试行单元应合法"),
            RpgMakerWriteBackUnit::new(
                TextUnitRole::DialogueSpeaker,
                TextUnitContent::Value("爱丽丝🙂".to_owned()),
                None,
            )
            .expect("测试 Speaker 单元应合法"),
            RpgMakerWriteBackUnit::new(
                TextUnitRole::DialogueBody,
                TextUnitContent::Lines(vec!["你好🌏".to_owned(), "再见".to_owned()]),
                None,
            )
            .expect("测试 Body 单元应合法"),
        ];

        let direct = |target: RpgMakerLocation, expected_raw: &str, line_index: usize| {
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    target,
                    expected_raw,
                    vec![
                        DirectTextPart::Literal("前缀【".to_owned()),
                        DirectTextPart::TextSlot {
                            role: scalar_role.clone(),
                        },
                        DirectTextPart::Literal("】中段".to_owned()),
                        DirectTextPart::LineSlot {
                            role: line_role.clone(),
                            source_line_index: line_index,
                        },
                        DirectTextPart::Literal("尾🙂".to_owned()),
                    ],
                )
                .expect("测试直接配方形状应合法"),
            )
        };
        let valid_direct = direct(location(101, Some(0)), "前缀【甲🙂】中段第二行🌏尾🙂", 1);
        let first_bad_target = location(102, Some(0));
        let mismatched_direct = direct(first_bad_target.clone(), "前缀【甲🙃】中段第二行🌏尾🙂", 1);
        let missing_line_direct = direct(location(103, Some(0)), "任意冻结原文", 9);

        let valid_dialogue = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                group_location.clone(),
                None,
                vec![
                    DialogueLineRecipe::new(
                        location(104, Some(0)),
                        "\\n<爱丽丝🙂>你好🌏",
                        vec![
                            DialogueLinePart::Literal("\\n<".to_owned()),
                            DialogueLinePart::SpeakerSlot,
                            DialogueLinePart::Literal(">".to_owned()),
                            DialogueLinePart::BodyLine {
                                source_line_index: 0,
                            },
                        ],
                    )
                    .expect("测试首行对话配方应合法"),
                    DialogueLineRecipe::new(
                        location(105, Some(0)),
                        "再见",
                        vec![DialogueLinePart::BodyLine {
                            source_line_index: 1,
                        }],
                    )
                    .expect("测试次行对话配方应合法"),
                ],
            )
            .expect("测试对话配方应合法"),
        );
        let bad_body_position_target = location(106, Some(0));
        let bad_body_position = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                group_location.clone(),
                None,
                vec![
                    DialogueLineRecipe::new(
                        bad_body_position_target,
                        "你好🌏!",
                        vec![
                            DialogueLinePart::BodyLine {
                                source_line_index: 0,
                            },
                            DialogueLinePart::Literal("!".to_owned()),
                        ],
                    )
                    .expect("测试模型允许快照边界拒绝 Body 后缀"),
                ],
            )
            .expect("测试对话配方形状应合法"),
        );
        let direct_speaker_target = location(107, Some(4));
        let bad_direct_speaker = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                group_location.clone(),
                Some(DirectSpeakerTarget::new(
                    direct_speaker_target,
                    "错误 Speaker",
                )),
                vec![
                    DialogueLineRecipe::new(
                        location(108, Some(0)),
                        "错误正文",
                        vec![DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        }],
                    )
                    .expect("测试直接 Speaker 对话行应合法"),
                ],
            )
            .expect("测试直接 Speaker 对话配方应合法"),
        );

        let cases = [
            vec![valid_direct.clone()],
            vec![mismatched_direct.clone()],
            vec![missing_line_direct],
            vec![valid_dialogue],
            vec![bad_body_position],
            vec![bad_direct_speaker],
            vec![
                valid_direct,
                mismatched_direct,
                direct(location(109, Some(0)), "同样错误", 1),
            ],
        ];
        for (case_index, recipes) in cases.iter().enumerate() {
            assert_eq!(
                validate_projection_round_trip(&group_location, &units, recipes),
                reference_validate_projection_round_trip(&group_location, &units, recipes),
                "第 {case_index} 个 Direct/Dialogue 等价性样本不一致"
            );
        }

        let first_error = validate_projection_round_trip(
            &group_location,
            &units,
            cases.last().expect("必须包含首错顺序样本"),
        );
        assert_eq!(
            first_error,
            Err(
                RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                    group_location: Box::new(group_location),
                    target: Box::new(first_bad_target),
                }
            )
        );
    }

    #[test]
    fn large_expected_raw_cursor_has_linear_borrowed_segment_work() {
        const SEGMENTS: usize = 262_144;
        const SEGMENT: &str = "片段🙂世界🌏";

        let expected_raw = SEGMENT.repeat(SEGMENTS);
        let mut cursor = ExpectedRawCursor::new(&expected_raw);
        for _ in 0..SEGMENTS {
            cursor.consume(SEGMENT);
        }

        assert!(cursor.is_complete());
        assert_eq!(cursor.work(), (SEGMENTS, expected_raw.len()));
        assert!(
            std::mem::size_of_val(&cursor) <= 6 * std::mem::size_of::<usize>(),
            "校验游标只能保留借用、偏移和固定工作计数，不能按冻结原文大小持有重建缓冲"
        );
    }

    fn dialogue_snapshot(
        speaker_translation: Option<&str>,
        body_translation: Option<&str>,
    ) -> RpgMakerWriteBackSnapshot {
        let header = location(0, None);
        let line = location(1, Some(0));
        let recipe = DialogueWriteRecipe::new(
            header.clone(),
            None,
            vec![
                DialogueLineRecipe::new(
                    line,
                    "\\n<Alice>Hello",
                    vec![
                        DialogueLinePart::Literal("\\n<".to_owned()),
                        DialogueLinePart::SpeakerSlot,
                        DialogueLinePart::Literal(">".to_owned()),
                        DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        },
                    ],
                )
                .expect("测试对话行应合法"),
            ],
        )
        .expect("测试对话配方应合法");
        let projection = TextProjectionRecipe::Dialogue(recipe);
        let mutation_locks = recipe_locks(
            TextGroupKind::EventDialogue,
            &header,
            std::slice::from_ref(&projection),
        );
        let group = RpgMakerWriteBackGroup::new(
            TextGroupKind::EventDialogue,
            header,
            vec![
                RpgMakerWriteBackUnit::new(
                    TextUnitRole::DialogueSpeaker,
                    TextUnitContent::Value("Alice".to_owned()),
                    speaker_translation
                        .map(|translation| TextUnitContent::Value(translation.to_owned())),
                )
                .expect("测试 Speaker 应合法"),
                RpgMakerWriteBackUnit::new(
                    TextUnitRole::DialogueBody,
                    TextUnitContent::Lines(vec!["Hello".to_owned()]),
                    body_translation
                        .map(|translation| TextUnitContent::Lines(split_hard_lines(translation))),
                )
                .expect("测试 Body 应合法"),
            ],
            vec![projection],
            mutation_locks,
        )
        .expect("测试对话组应合法");
        RpgMakerWriteBackSnapshot::new(vec![group]).expect("测试快照应合法")
    }

    fn dialogue_mutation(
        speaker_translation: Option<&str>,
        body_translation: Option<&str>,
    ) -> Option<ReplaceDialogueMutation> {
        let planned = plan_rpg_maker_write_back(
            dialogue_snapshot(speaker_translation, body_translation),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        match planned.mutation_plan.mutations().first() {
            None => None,
            Some(RpgMakerWriteBackMutation::ReplaceDialogue(mutation)) => Some(mutation.clone()),
            Some(other) => panic!("对话必须生成唯一块级 Mutation，实际为 {other:?}"),
        }
    }

    #[test]
    fn manual_layout_diagnostic_identifies_affected_logical_units() {
        let scalar_group = location(8, None);
        let scalar_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键应合法"));
        let scalar_unit = RpgMakerWriteBackUnit::new(
            scalar_role.clone(),
            TextUnitContent::Value("原说明".to_owned()),
            Some(TextUnitContent::Value("很长的译文".to_owned())),
        )
        .expect("测试标量单元应合法");
        let scalar_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::HelpDescription,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &scalar_group,
                &scalar_unit,
                location(8, Some(0)),
            )],
        );
        let scalar_diagnostic = ManualLayoutDiagnostic::from_request(&scalar_request);
        assert_eq!(
            scalar_diagnostic.locations(),
            &[LogicalTextLocation::new(scalar_group, scalar_role)]
        );

        let dialogue_group = location(10, None);
        let body_role = TextUnitRole::DialogueBody;
        let body = RpgMakerWriteBackUnit::new(
            body_role.clone(),
            TextUnitContent::Lines(vec!["原文一".to_owned(), "原文二".to_owned()]),
            Some(TextUnitContent::Lines(vec![
                "译文一".to_owned(),
                "译文二".to_owned(),
            ])),
        )
        .expect("对话正文单元应合法");
        let dialogue_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::DialogueBody,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![RpgMakerWriteBackLayoutSegment::from_unit_at(
                &dialogue_group,
                &body,
                location(11, Some(0)),
            )],
        );
        let dialogue_diagnostic = ManualLayoutDiagnostic::from_request(&dialogue_request);
        assert_eq!(
            dialogue_diagnostic.locations(),
            [LogicalTextLocation::new(dialogue_group, body_role)]
        );

        let scrolling_group = location(20, None);
        let scrolling_role = TextUnitRole::ScrollingText;
        let scrolling_request = RpgMakerWriteBackLayoutRequest::new(
            RpgMakerWriteBackLayoutRegion::ScrollingText,
            MaxFullwidthChars::new(2).expect("测试行宽应合法"),
            vec![
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &scrolling_group,
                    scrolling_role.clone(),
                    location(21, Some(0)),
                    "原文一".to_owned(),
                    Some("译文一".to_owned()),
                ),
                RpgMakerWriteBackLayoutSegment::from_line_at(
                    &scrolling_group,
                    scrolling_role.clone(),
                    location(22, Some(0)),
                    "原文二".to_owned(),
                    Some("译文二".to_owned()),
                ),
            ],
        );
        let scrolling_diagnostic = ManualLayoutDiagnostic::from_request(&scrolling_request);
        assert_eq!(
            scrolling_diagnostic.locations(),
            [LogicalTextLocation::new(scrolling_group, scrolling_role)]
        );
    }

    #[test]
    fn dialogue_none_speaker_only_body_only_and_both_use_one_atomic_mutation() {
        assert!(dialogue_mutation(None, None).is_none());

        let speaker_only = dialogue_mutation(Some("爱丽丝"), None).expect("Speaker 译文应触发写回");
        assert_eq!(speaker_only.speaker(), Some("爱丽丝"));
        assert_eq!(speaker_only.body_lines(), None);

        let body_only = dialogue_mutation(None, Some("你好")).expect("Body 译文应触发写回");
        assert_eq!(body_only.speaker(), Some("Alice"));
        assert_eq!(
            body_only.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["你好"])
        );

        let both = dialogue_mutation(Some("爱丽丝"), Some("你好")).expect("两类译文应触发写回");
        assert_eq!(both.speaker(), Some("爱丽丝"));
        assert_eq!(
            both.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["你好"])
        );
    }

    #[test]
    fn dialogue_body_hard_line_breaks_preserve_semantic_line_provenance() {
        let mutation =
            dialogue_mutation(None, Some("第一行\n第二行")).expect("Body 译文应触发写回");
        assert_eq!(
            mutation.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::text)
                .collect::<Vec<_>>()),
            Some(vec!["第一行", "第二行"])
        );
        assert_eq!(
            mutation.body_lines().map(|lines| lines
                .iter()
                .map(RpgMakerWriteBackLaidOutLine::source_semantic_line_index)
                .collect::<Vec<_>>()),
            Some(vec![0, 1])
        );
    }

    #[test]
    fn scrolling_recipe_keeps_blank_slots_inside_the_atomic_unit() {
        let group_location = location(0, None);
        let role = TextUnitRole::ScrollingText;
        let recipes = vec![
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(1, Some(0)),
                    "第一行",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 0,
                    }],
                )
                .expect("首行配方应合法"),
            ),
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(2, Some(0)),
                    "   ",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 1,
                    }],
                )
                .expect("冻结空白配方应合法"),
            ),
            TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    location(3, Some(0)),
                    "第三行",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 2,
                    }],
                )
                .expect("末行配方应合法"),
            ),
        ];
        let mutation_locks =
            recipe_locks(TextGroupKind::EventScrollingText, &group_location, &recipes);
        let group = RpgMakerWriteBackGroup::new(
            TextGroupKind::EventScrollingText,
            group_location,
            vec![
                RpgMakerWriteBackUnit::new(
                    role,
                    TextUnitContent::Lines(vec![
                        "第一行".to_owned(),
                        "   ".to_owned(),
                        "第三行".to_owned(),
                    ]),
                    Some(TextUnitContent::Lines(vec![
                        "译文".to_owned(),
                        String::new(),
                        "第三行".to_owned(),
                    ])),
                )
                .expect("滚动文本单元应合法"),
            ],
            recipes,
            mutation_locks,
        )
        .expect("包含空白物理行的滚动组应合法");

        let planned = plan_rpg_maker_write_back(
            RpgMakerWriteBackSnapshot::new(vec![group]).expect("滚动快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [RpgMakerWriteBackMutation::ReplaceEventBody(mutation)] =
            planned.mutation_plan.mutations()
        else {
            panic!("滚动组应产生唯一块级 Mutation")
        };
        assert_eq!(mutation.segments().len(), 3);
        assert_eq!(mutation.segments()[0].replacement_lines(), &["译文"]);
        assert_eq!(mutation.segments()[1].replacement_lines(), &["   "]);
        assert_eq!(mutation.segments()[1].expected_original(), "   ");
        assert_eq!(mutation.segments()[2].replacement_lines(), &["第三行"]);
    }

    #[test]
    fn choices_are_planned_as_one_strictly_aligned_atomic_mutation() {
        let group_location = location(20, None);
        let source_lines = vec!["はい".to_owned(), "いいえ".to_owned()];
        let translated_lines = vec!["是".to_owned(), "否".to_owned()];
        let physical_targets = [
            (location(20, Some(0)), 0),
            (location(20, Some(1)), 1),
            (location(21, Some(1)), 0),
            (location(22, Some(1)), 1),
        ];
        let mut recipes = physical_targets
            .clone()
            .into_iter()
            .map(|(target, source_line_index)| {
                TextProjectionRecipe::Direct(
                    DirectTextRecipe::new(
                        target,
                        source_lines[source_line_index].clone(),
                        vec![DirectTextPart::LineSlot {
                            role: TextUnitRole::Choices,
                            source_line_index,
                        }],
                    )
                    .expect("选项配方应合法"),
                )
            })
            .collect::<Vec<_>>();
        let mut covered_values = physical_targets
            .into_iter()
            .map(|(target, _)| target)
            .collect::<Vec<_>>();
        covered_values.extend([location(21, None), location(22, None), location(23, None)]);
        recipes.push(TextProjectionRecipe::Claim(
            MutationClaim::event_block(group_location.clone(), covered_values)
                .expect("选项测试 EventBlock Claim 应合法"),
        ));
        let mutation_locks = recipe_locks(TextGroupKind::EventChoices, &group_location, &recipes);
        let group = RpgMakerWriteBackGroup::new(
            TextGroupKind::EventChoices,
            group_location,
            vec![
                RpgMakerWriteBackUnit::new(
                    TextUnitRole::Choices,
                    TextUnitContent::Lines(source_lines.clone()),
                    Some(TextUnitContent::Lines(translated_lines.clone())),
                )
                .expect("选项单元应合法"),
            ],
            recipes,
            mutation_locks,
        )
        .expect("选项组应合法");

        let planned = plan_rpg_maker_write_back(
            RpgMakerWriteBackSnapshot::new(vec![group]).expect("选项快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [RpgMakerWriteBackMutation::ReplaceChoices(mutation)] =
            planned.mutation_plan.mutations()
        else {
            panic!("选项组应产生唯一原子 Mutation")
        };
        assert_eq!(mutation.source_lines(), source_lines);
        assert_eq!(mutation.replacement_lines(), translated_lines);
    }

    #[test]
    fn aligned_units_reject_line_count_and_blank_slot_changes() {
        let invalid_group = |kind: TextGroupKind,
                             role: TextUnitRole,
                             source: TextUnitContent,
                             translation: TextUnitContent| {
            let group_location = location(40, None);
            let target = location(41, Some(0));
            let recipe = TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    target,
                    "原文",
                    vec![DirectTextPart::LineSlot {
                        role: role.clone(),
                        source_line_index: 0,
                    }],
                )
                .expect("测试配方应合法"),
            );
            let mutation_locks = recipe_locks(kind, &group_location, std::slice::from_ref(&recipe));
            RpgMakerWriteBackGroup::new(
                kind,
                group_location,
                vec![
                    RpgMakerWriteBackUnit::new(role, source, Some(translation))
                        .expect("非空内容应先建立待验证单元"),
                ],
                vec![recipe],
                mutation_locks,
            )
        };

        assert!(matches!(
            invalid_group(
                TextGroupKind::EventScrollingText,
                TextUnitRole::ScrollingText,
                TextUnitContent::Lines(vec!["甲".to_owned(), "乙".to_owned()]),
                TextUnitContent::Lines(vec!["译文".to_owned()]),
            ),
            Err(RpgMakerWriteBackSnapshotError::AlignedLineCountMismatch { .. })
        ));
        assert!(matches!(
            invalid_group(
                TextGroupKind::EventChoices,
                TextUnitRole::Choices,
                TextUnitContent::Lines(vec!["是".to_owned(), "   ".to_owned()]),
                TextUnitContent::Lines(vec!["はい".to_owned(), "填充".to_owned()]),
            ),
            Err(RpgMakerWriteBackSnapshotError::AlignedBlankLineMismatch { line_index: 1, .. })
        ));

        let group_location = location(43, None);
        let targets = [location(44, Some(0)), location(45, Some(0))];
        let mut recipes = targets
            .iter()
            .cloned()
            .map(|target| {
                TextProjectionRecipe::Direct(
                    DirectTextRecipe::new(
                        target,
                        "erase me",
                        vec![DirectTextPart::LineSlot {
                            role: TextUnitRole::Choices,
                            source_line_index: 0,
                        }],
                    )
                    .expect("测试配方应合法"),
                )
            })
            .collect::<Vec<_>>();
        let claim = TextProjectionRecipe::Claim(
            MutationClaim::event_block(group_location.clone(), targets.into_iter().collect())
                .expect("测试选项 Claim 应合法"),
        );
        recipes.push(claim);
        let mutation_locks = recipe_locks(TextGroupKind::EventChoices, &group_location, &recipes);
        let manual_group = RpgMakerWriteBackGroup::new(
            TextGroupKind::EventChoices,
            group_location,
            vec![
                RpgMakerWriteBackUnit::new_manual(
                    TextUnitRole::Choices,
                    TextUnitContent::Lines(vec!["erase me".to_owned()]),
                    TextUnitContent::Lines(vec![String::new()]),
                )
                .expect("人工译文明确填写的空字符串应能建立单元"),
            ],
            recipes,
            mutation_locks,
        );
        assert!(
            manual_group.is_ok(),
            "人工译文应能把非空源槽替换为空字符串：{manual_group:?}"
        );
    }

    #[test]
    fn write_back_group_rejects_kind_role_mismatch_through_the_shared_validator() {
        let group_location = location(42, None);
        let target = location(42, Some(0));
        let recipe = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                target,
                "Alice",
                vec![DirectTextPart::TextSlot {
                    role: TextUnitRole::DialogueSpeaker,
                }],
            )
            .expect("测试配方应合法"),
        );
        let mutation_locks = recipe_locks(
            TextGroupKind::DatabaseEntry,
            &group_location,
            std::slice::from_ref(&recipe),
        );

        assert!(matches!(
            RpgMakerWriteBackGroup::new(
                TextGroupKind::DatabaseEntry,
                group_location,
                vec![
                    RpgMakerWriteBackUnit::new(
                        TextUnitRole::DialogueSpeaker,
                        TextUnitContent::Value("Alice".to_owned()),
                        None,
                    )
                    .expect("非空内容应先建立待验证单元"),
                ],
                vec![recipe],
                mutation_locks,
            ),
            Err(RpgMakerWriteBackSnapshotError::InvalidRole {
                kind: TextGroupKind::DatabaseEntry,
                role: TextUnitRole::DialogueSpeaker,
            })
        ));
    }

    #[test]
    fn direct_recipe_renders_literals_and_all_logical_slots_once() {
        let group_location = location(3, None);
        let target = location(3, Some(0));
        let left = TextUnitRole::Scalar(ScalarFieldKey::new("left").expect("键应合法"));
        let right = TextUnitRole::Scalar(ScalarFieldKey::new("right").expect("键应合法"));
        let recipe = DirectTextRecipe::new(
            target,
            "<x>甲</x><x>乙</x>",
            vec![
                DirectTextPart::Literal("<x>".to_owned()),
                DirectTextPart::TextSlot { role: left.clone() },
                DirectTextPart::Literal("</x><x>".to_owned()),
                DirectTextPart::TextSlot {
                    role: right.clone(),
                },
                DirectTextPart::Literal("</x>".to_owned()),
            ],
        )
        .expect("直接配方应合法");
        let projection = TextProjectionRecipe::Direct(recipe);
        let mutation_locks = recipe_locks(
            TextGroupKind::EventCommand,
            &group_location,
            std::slice::from_ref(&projection),
        );
        let group = RpgMakerWriteBackGroup::new(
            TextGroupKind::EventCommand,
            group_location,
            vec![
                RpgMakerWriteBackUnit::new(
                    left,
                    TextUnitContent::Value("甲".to_owned()),
                    Some(TextUnitContent::Value("一".to_owned())),
                )
                .expect("左单元应合法"),
                RpgMakerWriteBackUnit::new(right, TextUnitContent::Value("乙".to_owned()), None)
                    .expect("右单元应合法"),
            ],
            vec![projection],
            mutation_locks,
        )
        .expect("直接组应合法");
        let planned = plan_rpg_maker_write_back(
            RpgMakerWriteBackSnapshot::new(vec![group]).expect("快照应合法"),
            &profile(),
            &ConservativeRpgMakerWriteBackTextLayouter,
        );
        let [RpgMakerWriteBackMutation::SetText(mutation)] = planned.mutation_plan.mutations()
        else {
            panic!("直接组应产生唯一 SetText")
        };
        assert_eq!(mutation.replacement(), "<x>一</x><x>乙</x>");
    }

    #[test]
    fn snapshot_rejects_recipe_target_corruption_and_cross_group_conflicts() {
        let group_location = location(4, None);
        let target = location(4, Some(0));
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("键应合法"));
        let recipe = DirectTextRecipe::new(
            target.clone(),
            "原文",
            vec![DirectTextPart::TextSlot { role: role.clone() }],
        )
        .expect("配方应合法");
        let projection = TextProjectionRecipe::Direct(recipe);
        let wrong_locks =
            MutationClaimSet::new(vec![MutationClaim::for_location(location(9, Some(0)))])
                .expect("测试 Claim 应无内部冲突")
                .locks()
                .to_vec();
        assert!(matches!(
            RpgMakerWriteBackGroup::new(
                TextGroupKind::EventCommand,
                group_location.clone(),
                vec![
                    RpgMakerWriteBackUnit::new(
                        role.clone(),
                        TextUnitContent::Value("原文".to_owned()),
                        None,
                    )
                    .expect("单元应合法")
                ],
                vec![projection.clone()],
                wrong_locks,
            ),
            Err(RpgMakerWriteBackSnapshotError::RecipeClaimMismatch { .. })
        ));

        let make_group = |field: &str| {
            let unit_role = TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("键应合法"));
            let direct = DirectTextRecipe::new(
                target.clone(),
                "原文",
                vec![DirectTextPart::TextSlot {
                    role: unit_role.clone(),
                }],
            )
            .expect("配方应合法");
            let projection = TextProjectionRecipe::Direct(direct);
            let mutation_locks = recipe_locks(
                TextGroupKind::EventCommand,
                &group_location,
                std::slice::from_ref(&projection),
            );
            RpgMakerWriteBackGroup::new(
                TextGroupKind::EventCommand,
                group_location.clone(),
                vec![
                    RpgMakerWriteBackUnit::new(
                        unit_role,
                        TextUnitContent::Value("原文".to_owned()),
                        None,
                    )
                    .expect("单元应合法"),
                ],
                vec![projection],
                mutation_locks,
            )
            .expect("单组应合法")
        };
        assert!(matches!(
            RpgMakerWriteBackSnapshot::new(vec![make_group("first"), make_group("second")]),
            Err(RpgMakerWriteBackSnapshotError::MutationClaimConflict { .. })
        ));
    }

    #[test]
    fn snapshot_rejects_recipe_that_cannot_rebuild_frozen_original() {
        let direct_group = location(20, None);
        let direct_target = location(20, Some(0));
        let direct_role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("键应合法"));
        let direct = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                direct_target,
                "[Alice]",
                vec![
                    DirectTextPart::Literal("<".to_owned()),
                    DirectTextPart::TextSlot {
                        role: direct_role.clone(),
                    },
                    DirectTextPart::Literal(">".to_owned()),
                ],
            )
            .expect("形状合法但不能还原原文的直接配方应可进入快照边界"),
        );
        let direct_locks = recipe_locks(
            TextGroupKind::EventCommand,
            &direct_group,
            std::slice::from_ref(&direct),
        );
        assert!(matches!(
            RpgMakerWriteBackGroup::new(
                TextGroupKind::EventCommand,
                direct_group,
                vec![
                    RpgMakerWriteBackUnit::new(
                        direct_role,
                        TextUnitContent::Value("Alice".to_owned()),
                        None,
                    )
                    .expect("单元应合法")
                ],
                vec![direct.clone()],
                direct_locks,
            ),
            Err(RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));

        let dialogue_group = location(30, None);
        let direct_speaker = DirectSpeakerTarget::new(location(30, Some(4)), "Alice");
        let dialogue = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                dialogue_group.clone(),
                Some(direct_speaker),
                vec![
                    DialogueLineRecipe::new(
                        location(31, Some(0)),
                        "Hello",
                        vec![DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        }],
                    )
                    .expect("正文配方应合法"),
                ],
            )
            .expect("对话配方形状应合法"),
        );
        let dialogue_locks = recipe_locks(
            TextGroupKind::EventDialogue,
            &dialogue_group,
            std::slice::from_ref(&dialogue),
        );
        assert!(matches!(
            RpgMakerWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                dialogue_group,
                vec![
                    RpgMakerWriteBackUnit::new(
                        TextUnitRole::DialogueSpeaker,
                        TextUnitContent::Value("Bob".to_owned()),
                        None,
                    )
                    .expect("Speaker 单元应合法"),
                    RpgMakerWriteBackUnit::new(
                        TextUnitRole::DialogueBody,
                        TextUnitContent::Lines(vec!["Hello".to_owned()]),
                        None,
                    )
                    .expect("Body 单元应合法"),
                ],
                vec![dialogue.clone()],
                dialogue_locks,
            ),
            Err(RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));

        let trailing_group = location(40, None);
        let trailing = TextProjectionRecipe::Dialogue(
            DialogueWriteRecipe::new(
                trailing_group.clone(),
                None,
                vec![
                    DialogueLineRecipe::new(
                        location(41, Some(0)),
                        "Hello",
                        vec![
                            DialogueLinePart::BodyLine {
                                source_line_index: 0,
                            },
                            DialogueLinePart::Literal(String::new()),
                        ],
                    )
                    .expect("模型边界允许由快照边界拒绝 Body 后缀"),
                ],
            )
            .expect("对话配方形状应合法"),
        );
        let trailing_locks = recipe_locks(
            TextGroupKind::EventDialogue,
            &trailing_group,
            std::slice::from_ref(&trailing),
        );
        assert!(matches!(
            RpgMakerWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                trailing_group,
                vec![
                    RpgMakerWriteBackUnit::new(
                        TextUnitRole::DialogueBody,
                        TextUnitContent::Lines(vec!["Hello".to_owned()]),
                        None,
                    )
                    .expect("Body 单元应合法")
                ],
                vec![trailing.clone()],
                trailing_locks,
            ),
            Err(RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal { .. })
        ));
    }
}
