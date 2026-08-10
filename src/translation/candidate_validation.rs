//! Manual、自动 Translate 与 Lua 共用的候选译文硬不变量。

use serde::{Deserialize, Serialize};

/// 候选译文必须保持的文本形状。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateTextShape {
    /// 允许模型重新分行，但整个候选必须包含实际文本。
    Free,
    /// 逐槽对应原文；槽数和空槽位置都属于写回结构。
    Fixed,
}

/// 判断文本是否只包含结构空白。
///
/// U+000C 在 RPG Maker 消息窗口中是已经确认的换页控制符，不能因为 Rust 把它归入
/// Unicode whitespace 就把包含它的槽误判成空槽。其他 Unicode 空白保持现行语义。
pub(crate) fn is_structural_blank(text: &str) -> bool {
    text.chars()
        .all(|character| character != '\u{000c}' && character.is_whitespace())
}

/// 可以只凭当前原文、候选正文和确定结构证明的违反项。
///
/// 语言残留、布局宽度与措辞质量不在这里：它们需要阈值、运行时界面或人工判断，
/// 因而只能进入 Review，不能阻止合法候选保存。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProvenInvariantViolation {
    LineCountMismatch { expected: usize, actual: usize },
    InvalidLineText { line_index: usize },
    ContainsByteOrderMark { line_index: usize },
    BlankTranslation,
    FixedBlankSlotChanged { line_index: usize },
    FixedNonBlankSlotEmptied { line_index: usize },
    PlaceholderMismatch,
    UnexpectedPlaceholderToken,
    PlaceholderBoundaryChanged,
    ReservedPlaceholderToken,
    InvalidCandidateShape,
}

/// 需要后续质量审核、但没有破坏可证明不变量的事实。
///
/// Review 只能附着在有效候选或完整可解析响应上；它不实现 `Error`，也不能转换为
/// `ProvenInvariantViolation`，从类型边界上阻止调用方把启发式质量判断升级为拒绝。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewFinding {
    SourceResidual,
    NonStopFinish,
}

/// 已通过全部硬不变量的值及其非阻塞 Review。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedCandidate<T> {
    value: T,
    reviews: Vec<ReviewFinding>,
}

impl<T> ValidatedCandidate<T> {
    pub(crate) fn clean(value: T) -> Self {
        Self {
            value,
            reviews: Vec::new(),
        }
    }

    pub(crate) fn with_review(value: T, finding: ReviewFinding) -> Self {
        Self {
            value,
            reviews: vec![finding],
        }
    }

    pub(crate) fn into_parts(self) -> (T, Vec<ReviewFinding>) {
        (self.value, self.reviews)
    }

    pub(crate) fn value(&self) -> &T {
        &self.value
    }

    pub(crate) fn reviews(&self) -> &[ReviewFinding] {
        &self.reviews
    }
}

/// 验证所有候选入口都必须遵守的文本不变量。
pub(crate) fn validate_candidate_text(
    source: &[String],
    candidate: &[String],
    shape: CandidateTextShape,
) -> Result<(), ProvenInvariantViolation> {
    validate_candidate_text_with_cancellation(source, candidate, shape, || {
        Ok::<_, std::convert::Infallible>(())
    })
    .unwrap_or_else(|never| match never {})
}

/// 带协作取消检查的同一硬不变量入口。
pub(crate) fn validate_candidate_text_with_cancellation<E>(
    source: &[String],
    candidate: &[String],
    shape: CandidateTextShape,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<(), ProvenInvariantViolation>, E> {
    ensure_running()?;
    if matches!(shape, CandidateTextShape::Fixed) && candidate.len() != source.len() {
        return Ok(Err(ProvenInvariantViolation::LineCountMismatch {
            expected: source.len(),
            actual: candidate.len(),
        }));
    }

    for (line_index, line) in candidate.iter().enumerate() {
        ensure_running()?;
        if line
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            return Ok(Err(ProvenInvariantViolation::InvalidLineText {
                line_index,
            }));
        }
        if line.contains('\u{feff}') {
            return Ok(Err(ProvenInvariantViolation::ContainsByteOrderMark {
                line_index,
            }));
        }
    }

    if matches!(shape, CandidateTextShape::Fixed) {
        for (line_index, (source, translation)) in source.iter().zip(candidate).enumerate() {
            ensure_running()?;
            if is_structural_blank(source) {
                if !translation.is_empty() {
                    return Ok(Err(ProvenInvariantViolation::FixedBlankSlotChanged {
                        line_index,
                    }));
                }
            } else if is_structural_blank(translation) {
                return Ok(Err(ProvenInvariantViolation::FixedNonBlankSlotEmptied {
                    line_index,
                }));
            }
        }
    }

    ensure_running()?;
    if candidate.iter().all(|line| is_structural_blank(line)) {
        return Ok(Err(ProvenInvariantViolation::BlankTranslation));
    }

    ensure_running()?;
    Ok(Ok(()))
}

/// 验证已经由字符串数组按换行符连接的自由重排候选。
pub(crate) fn validate_reflowed_candidate_text_with_cancellation<E>(
    candidate: &str,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<(), ProvenInvariantViolation>, E> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    let mut line_index = 0_usize;
    let mut has_non_whitespace = false;
    for (character_index, character) in candidate.chars().enumerate() {
        if character_index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        match character {
            '\n' => line_index += 1,
            '\r' | '\0' => {
                return Ok(Err(ProvenInvariantViolation::InvalidLineText {
                    line_index,
                }));
            }
            '\u{feff}' => {
                return Ok(Err(ProvenInvariantViolation::ContainsByteOrderMark {
                    line_index,
                }));
            }
            _ => has_non_whitespace |= character == '\u{000c}' || !character.is_whitespace(),
        }
    }
    ensure_running()?;
    if has_non_whitespace {
        Ok(Ok(()))
    } else {
        Ok(Err(ProvenInvariantViolation::BlankTranslation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn fixed_shape_preserves_blank_slots_and_nonblank_slots() {
        let source = lines(&["Accept quest", "", "Cancel"]);
        assert_eq!(
            validate_candidate_text(
                &source,
                &lines(&["接受任务", "", "取消"]),
                CandidateTextShape::Fixed,
            ),
            Ok(())
        );
        assert_eq!(
            validate_candidate_text(
                &source,
                &lines(&["接受任务", "多余文字", "取消"]),
                CandidateTextShape::Fixed,
            ),
            Err(ProvenInvariantViolation::FixedBlankSlotChanged { line_index: 1 })
        );
        assert_eq!(
            validate_candidate_text(
                &source,
                &lines(&["", "", "取消"]),
                CandidateTextShape::Fixed,
            ),
            Err(ProvenInvariantViolation::FixedNonBlankSlotEmptied { line_index: 0 })
        );
    }

    #[test]
    fn an_explicit_whitespace_candidate_is_not_an_unfilled_manual_slot() {
        assert_eq!(
            validate_candidate_text(
                &lines(&["General"]),
                &lines(&["  "]),
                CandidateTextShape::Free,
            ),
            Err(ProvenInvariantViolation::BlankTranslation)
        );
    }

    #[test]
    fn page_break_is_not_structural_blank() {
        assert_eq!(
            validate_candidate_text(
                &lines(&["\u{000c}"]),
                &lines(&["\u{000c}"]),
                CandidateTextShape::Fixed,
            ),
            Ok(())
        );
        assert_eq!(
            validate_candidate_text(
                &lines(&["message"]),
                &lines(&["\u{000c}"]),
                CandidateTextShape::Free,
            ),
            Ok(())
        );
        assert_eq!(
            validate_reflowed_candidate_text_with_cancellation("\u{000c}", || {
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap(),
            Ok(())
        );
    }
}
