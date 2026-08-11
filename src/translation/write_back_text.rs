//! WriteBack 共享的正文修复能力。

use crate::language::{LanguageText, LanguageTextSegment};
use crate::translation::placeholder::ProtectedText;
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::symbol_repair::{
    TranslationSymbolRepairOutcome, TranslationSymbolRepairer,
};

/// 标点修复后的正文。无法完整证明修复安全时保留原译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PunctuationRepairOutcome {
    Unchanged,
    Repaired(String),
    Skipped,
}

/// Placeholder 投影失败属于内部不变量破坏，不能伪装成无需修复。
#[derive(Debug)]
pub(crate) enum PunctuationRepairError {
    SourceProjection(LanguageTextProjectionError),
    TranslationProjection(LanguageTextProjectionError),
}

/// 只修复 Placeholder 之外的自然文本，并原样恢复译文实际携带的 Placeholder。
pub(crate) fn repair_punctuation_with_cancellation<E>(
    source: &ProtectedText,
    translation: &ProtectedText,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<PunctuationRepairOutcome, PunctuationRepairError>, E> {
    let source_text = match source.language_text_with_cancellation(&mut ensure_running)? {
        Ok(text) => text,
        Err(source) => return Ok(Err(PunctuationRepairError::SourceProjection(source))),
    };
    let translation_text = match translation.language_text_with_cancellation(&mut ensure_running)? {
        Ok(text) => text,
        Err(source) => {
            return Ok(Err(PunctuationRepairError::TranslationProjection(source)));
        }
    };

    let plan = match TranslationSymbolRepairer::plan_repair_with_cancellation(
        &source_text,
        &translation_text,
        &mut ensure_running,
    )? {
        TranslationSymbolRepairOutcome::Unchanged => {
            return Ok(Ok(PunctuationRepairOutcome::Unchanged));
        }
        TranslationSymbolRepairOutcome::Repaired { plan, .. } => plan,
        TranslationSymbolRepairOutcome::Skipped { .. } => {
            return Ok(Ok(PunctuationRepairOutcome::Skipped));
        }
    };
    let repaired =
        match translation_text.apply_repair_with_cancellation(&plan, &mut ensure_running)? {
            Ok(text) => text,
            Err(_) => return Ok(Ok(PunctuationRepairOutcome::Skipped)),
        };
    let Some(text) = rebuild_translation(&repaired, translation, &mut ensure_running)? else {
        return Ok(Ok(PunctuationRepairOutcome::Skipped));
    };
    Ok(Ok(PunctuationRepairOutcome::Repaired(text)))
}

fn rebuild_translation<E>(
    repaired: &LanguageText,
    protected: &ProtectedText,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<String>, E> {
    let mut placeholders = protected.placeholders().iter();
    let mut output_length = 0_usize;
    for segment in repaired.segments() {
        ensure_running()?;
        let text = match segment {
            LanguageTextSegment::NaturalText(text) => text.as_str(),
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return Ok(None);
                };
                placeholder.original()
            }
        };
        let Some(next) = output_length.checked_add(text.len()) else {
            return Ok(None);
        };
        output_length = next;
    }
    if placeholders.next().is_some() {
        return Ok(None);
    }

    let mut output = String::new();
    if output.try_reserve_exact(output_length).is_err() {
        return Ok(None);
    }
    let mut placeholders = protected.placeholders().iter();
    for segment in repaired.segments() {
        ensure_running()?;
        match segment {
            LanguageTextSegment::NaturalText(text) => output.push_str(text),
            LanguageTextSegment::OpaqueBoundary => {
                let Some(placeholder) = placeholders.next() else {
                    return Ok(None);
                };
                output.push_str(placeholder.original());
            }
        }
    }
    ensure_running()?;
    if placeholders.next().is_some() {
        return Ok(None);
    }
    Ok(Some(output))
}
