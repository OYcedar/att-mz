//! 把 MZ 翻译阶段的占位符文本投影为引擎无关的语言视图。

use std::error::Error;
use std::fmt;

use crate::language::{LanguageText, LanguageTextSegment};

use super::standard::AppliedPlaceholder;

/// 从已保护文本建立语言模块可见的自然文本与不透明边界。
///
/// 占位符 token 及其原片段都不会进入语言视图；token 两侧始终由一个不透明边界
/// 分隔，不能因为隐藏内部协议而被重新拼接成另一段自然文本。
pub(crate) fn project_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<LanguageText, LanguageTextProjectionError> {
    let (language_text, _) = project_with_ordered_tokens(protected_text, placeholders)?;
    Ok(language_text)
}

/// 把修复后的语言视图重建为仍含 token 的文本。
///
/// token 使用模型译文中的实际顺序，而不是规则声明顺序。语言模块只能
/// 修改自然文本字符；分段数量或自然/不透明类型发生变化都属于内部不变量破坏。
#[cfg(test)]
pub(crate) fn rebuild_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
    repaired_text: &LanguageText,
) -> Result<String, LanguageTextProjectionError> {
    rebuild_text(
        protected_text,
        placeholders,
        repaired_text,
        OpaqueRebuild::Token,
    )
}

/// 把修复后的自然文本与每个 token 精确绑定的原片段直接交错重建。
///
/// 直接按边界恢复可以避免连续全局 `replace` 让某个原片段中恰好出现的另一个
/// token 再次被替换。
pub(crate) fn restore_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
    repaired_text: &LanguageText,
) -> Result<String, LanguageTextProjectionError> {
    rebuild_text(
        protected_text,
        placeholders,
        repaired_text,
        OpaqueRebuild::Original,
    )
}

#[derive(Clone, Copy)]
enum OpaqueRebuild {
    #[cfg(test)]
    Token,
    Original,
}

fn rebuild_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
    repaired_text: &LanguageText,
    opaque_rebuild: OpaqueRebuild,
) -> Result<String, LanguageTextProjectionError> {
    let (projected, ordered_tokens) = project_with_ordered_tokens(protected_text, placeholders)?;
    if projected.segments().len() != repaired_text.segments().len() {
        return Err(LanguageTextProjectionError::ChangedSegmentCount {
            expected: projected.segments().len(),
            actual: repaired_text.segments().len(),
        });
    }

    let mut rebuilt = String::with_capacity(protected_text.len());
    let mut tokens = ordered_tokens.into_iter();
    for (segment_index, (before, after)) in projected
        .segments()
        .iter()
        .zip(repaired_text.segments())
        .enumerate()
    {
        match (before, after) {
            (LanguageTextSegment::NaturalText(_), LanguageTextSegment::NaturalText(repaired)) => {
                rebuilt.push_str(repaired)
            }
            (LanguageTextSegment::OpaqueBoundary, LanguageTextSegment::OpaqueBoundary) => {
                let Some(token) = tokens.next() else {
                    return Err(LanguageTextProjectionError::MissingOrderedToken { segment_index });
                };
                match opaque_rebuild {
                    #[cfg(test)]
                    OpaqueRebuild::Token => rebuilt.push_str(token),
                    OpaqueRebuild::Original => {
                        let Some(placeholder) =
                            placeholders.iter().find(|binding| binding.token() == token)
                        else {
                            return Err(LanguageTextProjectionError::MissingOriginalBinding {
                                token: token.to_owned(),
                            });
                        };
                        rebuilt.push_str(placeholder.original());
                    }
                }
            }
            _ => {
                return Err(LanguageTextProjectionError::ChangedSegmentKind { segment_index });
            }
        }
    }
    if tokens.next().is_some() {
        return Err(LanguageTextProjectionError::UnusedOrderedToken);
    }
    Ok(rebuilt)
}

fn project_with_ordered_tokens<'a>(
    protected_text: &str,
    placeholders: &'a [AppliedPlaceholder],
) -> Result<(LanguageText, Vec<&'a str>), LanguageTextProjectionError> {
    let mut positioned = Vec::with_capacity(placeholders.len());
    for placeholder in placeholders {
        let token = placeholder.token();
        if token.is_empty() {
            return Err(LanguageTextProjectionError::EmptyToken);
        }
        let mut occurrences = protected_text.match_indices(token);
        let Some((start, _)) = occurrences.next() else {
            return Err(LanguageTextProjectionError::MissingToken {
                token: token.to_owned(),
            });
        };
        if occurrences.next().is_some() {
            return Err(LanguageTextProjectionError::RepeatedToken {
                token: token.to_owned(),
            });
        }
        positioned.push((start, start + token.len(), token));
    }
    positioned.sort_unstable_by_key(|(start, _, _)| *start);

    let mut segments = Vec::with_capacity(positioned.len().saturating_mul(2) + 1);
    let mut ordered_tokens = Vec::with_capacity(positioned.len());
    let mut cursor = 0;
    for (start, end, token) in positioned {
        if start < cursor {
            return Err(LanguageTextProjectionError::OverlappingToken {
                token: token.to_owned(),
            });
        }
        if cursor < start {
            segments.push(LanguageTextSegment::NaturalText(
                protected_text[cursor..start].to_owned(),
            ));
        }
        segments.push(LanguageTextSegment::OpaqueBoundary);
        ordered_tokens.push(token);
        cursor = end;
    }
    if cursor < protected_text.len() {
        segments.push(LanguageTextSegment::NaturalText(
            protected_text[cursor..].to_owned(),
        ));
    }

    Ok((LanguageText::new(segments), ordered_tokens))
}

/// 受信占位符绑定与受保护文本不再一致，无法安全建立语言视图。
#[derive(Debug)]
pub(crate) enum LanguageTextProjectionError {
    EmptyToken,
    MissingToken { token: String },
    RepeatedToken { token: String },
    OverlappingToken { token: String },
    ChangedSegmentCount { expected: usize, actual: usize },
    ChangedSegmentKind { segment_index: usize },
    MissingOrderedToken { segment_index: usize },
    MissingOriginalBinding { token: String },
    UnusedOrderedToken,
}

impl fmt::Display for LanguageTextProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyToken => formatter.write_str("占位符 token 为空"),
            Self::MissingToken { token } => {
                write!(formatter, "受保护文本缺少占位符 token {token:?}")
            }
            Self::RepeatedToken { token } => {
                write!(formatter, "受保护文本重复占位符 token {token:?}")
            }
            Self::OverlappingToken { token } => {
                write!(formatter, "占位符 token {token:?} 与其他 token 重叠")
            }
            Self::ChangedSegmentCount { expected, actual } => write!(
                formatter,
                "语言修复改变了分段数量：预期 {expected}，实际 {actual}"
            ),
            Self::ChangedSegmentKind { segment_index } => {
                write!(formatter, "语言修复改变了第 {segment_index} 个分段的类型")
            }
            Self::MissingOrderedToken { segment_index } => {
                write!(formatter, "第 {segment_index} 个不透明边界没有对应 token")
            }
            Self::MissingOriginalBinding { token } => {
                write!(formatter, "占位符 token {token:?} 没有对应原片段")
            }
            Self::UnusedOrderedToken => formatter.write_str("重建完成后仍有未使用的 token"),
        }
    }
}

impl Error for LanguageTextProjectionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att_mz::translate::standard::{PlaceholderRuleOrigin, PlaceholderSegment};

    #[test]
    fn placeholder_sides_remain_separate_natural_segments() {
        let placeholder = applied("<token>");
        let projected = project_protected_text("前半<token>后半", &[placeholder])
            .expect("完整绑定应该可以投影");

        assert_eq!(
            projected.segments(),
            [
                LanguageTextSegment::NaturalText("前半".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText("后半".to_owned()),
            ]
        );
    }

    #[test]
    fn fully_protected_text_has_no_natural_content() {
        let projected =
            project_protected_text("<token>", &[applied("<token>")]).expect("整段保护应该可以投影");

        assert_eq!(projected.segments(), [LanguageTextSegment::OpaqueBoundary]);
        assert!(!projected.has_non_whitespace_natural_text());
    }

    #[test]
    fn inconsistent_bindings_fail_instead_of_exposing_protocol_text() {
        assert!(matches!(
            project_protected_text("自然文本", &[applied("<missing>")]),
            Err(LanguageTextProjectionError::MissingToken { .. })
        ));
        assert!(matches!(
            project_protected_text("<same><same>", &[applied("<same>")]),
            Err(LanguageTextProjectionError::RepeatedToken { .. })
        ));
    }

    #[test]
    fn rebuild_keeps_the_tokens_actual_translated_order() {
        let first = applied("<first>");
        let second = applied("<second>");
        let translated = "前<second>中<first>后";
        let projected = project_protected_text(translated, &[first.clone(), second.clone()])
            .expect("模型可以在语义需要时重排 token");

        assert_eq!(
            rebuild_protected_text(translated, &[first, second], &projected)
                .expect("应使用译文中的 token 顺序"),
            translated
        );
    }

    #[test]
    fn direct_restoration_does_not_replace_token_like_text_inside_original_fragments() {
        let first = applied_with_original("<first>", "<second>");
        let second = applied_with_original("<second>", "原片段二");
        let translated = "前<first>中<second>后";
        let projected = project_protected_text(translated, &[first.clone(), second.clone()])
            .expect("模型译文中的 token 应该可以投影");

        assert_eq!(
            restore_protected_text(translated, &[first, second], &projected)
                .expect("每个边界应直接恢复自己的原片段"),
            "前<second>中原片段二后"
        );
    }

    fn applied(token: &str) -> AppliedPlaceholder {
        applied_with_original(token, "原保护片段")
    }

    fn applied_with_original(token: &str, original: &str) -> AppliedPlaceholder {
        AppliedPlaceholder::new(
            token,
            original,
            PlaceholderRuleOrigin::Custom,
            "TEST",
            "all",
            PlaceholderSegment::Whole,
        )
    }
}
