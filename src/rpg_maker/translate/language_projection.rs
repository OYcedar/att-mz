//! 把 RPG Maker 翻译阶段的占位符文本投影为引擎无关的语言视图。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

use crate::language::{LanguageText, LanguageTextSegment};
use crate::rpg_maker::placeholder_token;

use super::standard::AppliedPlaceholder;

/// 从已保护文本建立语言模块可见的自然文本与不透明边界。
///
/// 占位符 token 及其原片段都不会进入语言视图；token 两侧始终由一个不透明边界
/// 分隔，不能因为隐藏内部协议而被重新拼接成另一段自然文本。
pub(crate) fn project_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<LanguageText, LanguageTextProjectionError> {
    let bindings = PlaceholderBindingIndex::new(placeholders)?;
    let scanned = bindings.scan(protected_text);
    Ok(bindings
        .project(protected_text, &scanned, bindings.all_binding_indices())?
        .language_text)
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
    let bindings = PlaceholderBindingIndex::new(placeholders)?;
    let scanned = bindings.scan(protected_text);
    let projected = bindings.project(protected_text, &scanned, bindings.all_binding_indices())?;
    bindings.rebuild(&projected, repaired_text, OpaqueRebuild::Token)
}

/// 把修复后的自然文本与每个 token 精确绑定的原片段直接交错重建。
///
/// 直接按边界恢复可以避免连续全局 `replace` 让某个原片段中恰好出现的另一个
/// token 再次被替换。
#[cfg(test)]
pub(crate) fn restore_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
    repaired_text: &LanguageText,
) -> Result<String, LanguageTextProjectionError> {
    let bindings = PlaceholderBindingIndex::new(placeholders)?;
    let scanned = bindings.scan(protected_text);
    let projected = bindings.project(protected_text, &scanned, bindings.all_binding_indices())?;
    bindings.rebuild(&projected, repaired_text, OpaqueRebuild::Original)
}

/// 同一候选内可复用的占位符 binding 索引。
///
/// 正常 ATT token 由保留信封扫描直接定位；只有测试或内部不变量破坏产生的
/// 非信封 token 才需要额外的多模式匹配器。索引本身不限制 binding 总量。
#[derive(Clone)]
pub(super) struct PlaceholderBindingIndex {
    placeholders: Arc<[AppliedPlaceholder]>,
    tokens: Vec<String>,
    token_to_index: HashMap<String, usize>,
    binding_token_indices: Vec<usize>,
    all_binding_indices: Vec<usize>,
    non_envelope_matcher: Option<AhoCorasick>,
    non_envelope_pattern_tokens: Vec<usize>,
    has_empty_token: bool,
    #[cfg(test)]
    scan_passes: Arc<AtomicUsize>,
}

impl fmt::Debug for PlaceholderBindingIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaceholderBindingIndex")
            .field("placeholders", &self.placeholders)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PlaceholderBindingIndex {
    fn eq(&self, other: &Self) -> bool {
        self.placeholders == other.placeholders
    }
}

impl Eq for PlaceholderBindingIndex {}

impl PlaceholderBindingIndex {
    pub(super) fn new(
        placeholders: &[AppliedPlaceholder],
    ) -> Result<Self, LanguageTextProjectionError> {
        Self::from_shared(Arc::from(placeholders))
    }

    pub(super) fn from_shared(
        placeholders: Arc<[AppliedPlaceholder]>,
    ) -> Result<Self, LanguageTextProjectionError> {
        let tokens = placeholders
            .iter()
            .map(|placeholder| placeholder.token().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let token_to_index = tokens
            .iter()
            .enumerate()
            .map(|(index, token)| (token.clone(), index))
            .collect::<HashMap<_, _>>();
        let binding_token_indices = placeholders
            .iter()
            .map(|placeholder| token_to_index[placeholder.token()])
            .collect::<Vec<_>>();
        let all_binding_indices = (0..placeholders.len()).collect::<Vec<_>>();
        let has_empty_token = tokens.first().is_some_and(|token| token.is_empty());

        let mut non_envelope_patterns = Vec::new();
        let mut non_envelope_pattern_tokens = Vec::new();
        for (token_index, token) in tokens.iter().enumerate() {
            if !token.is_empty() && !is_complete_token_envelope(token) {
                non_envelope_patterns.push(token.as_str());
                non_envelope_pattern_tokens.push(token_index);
            }
        }
        let non_envelope_matcher = if non_envelope_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .build(non_envelope_patterns)
                    .map_err(|_| LanguageTextProjectionError::TokenIndexConstruction)?,
            )
        };

        Ok(Self {
            placeholders,
            tokens,
            token_to_index,
            binding_token_indices,
            all_binding_indices,
            non_envelope_matcher,
            non_envelope_pattern_tokens,
            has_empty_token,
            #[cfg(test)]
            scan_passes: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(super) fn all_binding_indices(&self) -> &[usize] {
        &self.all_binding_indices
    }

    pub(super) fn scan(&self, text: &str) -> PlaceholderTextScan {
        #[cfg(test)]
        self.scan_passes.fetch_add(1, Ordering::Relaxed);

        let mut matches = HashMap::<usize, TokenMatches>::new();
        if let Some(matcher) = &self.non_envelope_matcher {
            let mut last_ends = vec![0usize; self.non_envelope_pattern_tokens.len()];
            for found in matcher.find_overlapping_iter(text) {
                let pattern_index = found.pattern().as_usize();
                if found.start() < last_ends[pattern_index] {
                    continue;
                }
                last_ends[pattern_index] = found.end();
                record_match(
                    &mut matches,
                    self.non_envelope_pattern_tokens[pattern_index],
                    found.start(),
                    found.end(),
                );
            }
        }

        let mut envelopes = Vec::new();
        let mut cursor = 0usize;
        let envelope_scan = loop {
            let Some(relative_start) = text[cursor..].find(placeholder_token::PREFIX) else {
                break EnvelopeScan::Complete(envelopes);
            };
            let start = cursor + relative_start;
            let payload_start = start + placeholder_token::PREFIX.len();
            let Some(relative_end) = text[payload_start..].find(placeholder_token::SUFFIX) else {
                let fragment = placeholder_token::scan_envelopes(text)
                    .expect_err("已定位未闭合保留前缀")
                    .into_fragment();
                break EnvelopeScan::Unclosed(fragment);
            };
            let end = payload_start + relative_end + placeholder_token::SUFFIX.len();
            let token = &text[start..end];
            if let Some(&token_index) = self.token_to_index.get(token) {
                record_match(&mut matches, token_index, start, end);
                envelopes.push(ScannedEnvelope::Known(token_index));
            } else {
                envelopes.push(ScannedEnvelope::Unknown(token.to_owned()));
            }
            cursor = end;
        };

        PlaceholderTextScan {
            source_bytes: text.len(),
            empty_token_occurrences: if self.has_empty_token {
                text.chars().count().saturating_add(1)
            } else {
                0
            },
            matches,
            envelope_scan,
        }
    }

    pub(super) fn present_binding_indices(&self, scanned: &PlaceholderTextScan) -> Vec<usize> {
        self.all_binding_indices
            .iter()
            .copied()
            .filter(|&binding_index| {
                let token = self.placeholders[binding_index].token();
                token.is_empty()
                    || scanned
                        .matches
                        .contains_key(&self.binding_token_indices[binding_index])
            })
            .collect()
    }

    pub(super) fn token_occurrences(
        &self,
        scans: &[PlaceholderTextScan],
        binding_index: usize,
    ) -> usize {
        let token_index = self.binding_token_indices[binding_index];
        if self.tokens[token_index].is_empty() {
            return scans.iter().map(|scan| scan.empty_token_occurrences).sum();
        }
        scans
            .iter()
            .map(|scan| {
                scan.matches
                    .get(&token_index)
                    .map_or(0, |matched| matched.count)
            })
            .sum()
    }

    pub(super) fn validate_multiset(
        &self,
        scans: &[PlaceholderTextScan],
        binding_indices: &[usize],
    ) -> Result<(), PlaceholderMultisetError> {
        let mut expected = BTreeMap::<usize, usize>::new();
        for &binding_index in binding_indices {
            *expected
                .entry(self.binding_token_indices[binding_index])
                .or_default() += 1;
        }

        for (&token_index, &expected_count) in &expected {
            let actual_count: usize = if self.tokens[token_index].is_empty() {
                scans.iter().map(|scan| scan.empty_token_occurrences).sum()
            } else {
                scans
                    .iter()
                    .map(|scan| {
                        scan.matches
                            .get(&token_index)
                            .map_or(0, |matched| matched.count)
                    })
                    .sum()
            };
            if actual_count != expected_count {
                return Err(PlaceholderMultisetError::Mismatch {
                    token: self.tokens[token_index].clone(),
                });
            }
        }

        for scan in scans {
            let envelopes = match &scan.envelope_scan {
                EnvelopeScan::Complete(envelopes) => envelopes,
                EnvelopeScan::Unclosed(fragment) => {
                    return Err(PlaceholderMultisetError::Unexpected {
                        token: fragment.clone(),
                    });
                }
            };
            for envelope in envelopes {
                match envelope {
                    ScannedEnvelope::Known(token_index) if expected.contains_key(token_index) => {}
                    ScannedEnvelope::Known(token_index) => {
                        return Err(PlaceholderMultisetError::Unexpected {
                            token: self.tokens[*token_index].clone(),
                        });
                    }
                    ScannedEnvelope::Unknown(token) => {
                        return Err(PlaceholderMultisetError::Unexpected {
                            token: token.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn project(
        &self,
        protected_text: &str,
        scanned: &PlaceholderTextScan,
        binding_indices: &[usize],
    ) -> Result<PlaceholderProjection, LanguageTextProjectionError> {
        let mut positioned = Vec::with_capacity(binding_indices.len());
        for &binding_index in binding_indices {
            let token = self.placeholders[binding_index].token();
            if token.is_empty() {
                return Err(LanguageTextProjectionError::EmptyToken);
            }
            let token_index = self.binding_token_indices[binding_index];
            let Some(matched) = scanned.matches.get(&token_index) else {
                return Err(LanguageTextProjectionError::MissingToken {
                    token: token.to_owned(),
                });
            };
            if matched.count != 1 {
                return Err(LanguageTextProjectionError::RepeatedToken {
                    token: token.to_owned(),
                });
            }
            positioned.push((matched.first_start, matched.first_end, binding_index));
        }
        positioned.sort_unstable_by_key(|(start, _, _)| *start);

        let mut segments = Vec::with_capacity(positioned.len().saturating_mul(2) + 1);
        let mut ordered_binding_indices = Vec::with_capacity(positioned.len());
        let mut cursor = 0usize;
        for (start, end, binding_index) in positioned {
            let token = self.placeholders[binding_index].token();
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
            ordered_binding_indices.push(binding_index);
            cursor = end;
        }
        if cursor < protected_text.len() {
            segments.push(LanguageTextSegment::NaturalText(
                protected_text[cursor..].to_owned(),
            ));
        }

        Ok(PlaceholderProjection {
            language_text: LanguageText::new(segments),
            ordered_binding_indices,
            source_bytes: scanned.source_bytes,
        })
    }

    pub(super) fn rebuild_original(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
    ) -> Result<String, LanguageTextProjectionError> {
        self.rebuild(projected, repaired_text, OpaqueRebuild::Original)
    }

    fn rebuild(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
        opaque_rebuild: OpaqueRebuild,
    ) -> Result<String, LanguageTextProjectionError> {
        if projected.language_text.segments().len() != repaired_text.segments().len() {
            return Err(LanguageTextProjectionError::ChangedSegmentCount {
                expected: projected.language_text.segments().len(),
                actual: repaired_text.segments().len(),
            });
        }

        let mut rebuilt = String::with_capacity(projected.source_bytes);
        let mut ordered_bindings = projected.ordered_binding_indices.iter().copied();
        for (segment_index, (before, after)) in projected
            .language_text
            .segments()
            .iter()
            .zip(repaired_text.segments())
            .enumerate()
        {
            match (before, after) {
                (
                    LanguageTextSegment::NaturalText(_),
                    LanguageTextSegment::NaturalText(repaired),
                ) => rebuilt.push_str(repaired),
                (LanguageTextSegment::OpaqueBoundary, LanguageTextSegment::OpaqueBoundary) => {
                    let Some(binding_index) = ordered_bindings.next() else {
                        return Err(LanguageTextProjectionError::MissingOrderedToken {
                            segment_index,
                        });
                    };
                    let binding = &self.placeholders[binding_index];
                    match opaque_rebuild {
                        #[cfg(test)]
                        OpaqueRebuild::Token => rebuilt.push_str(binding.token()),
                        OpaqueRebuild::Original => rebuilt.push_str(binding.original()),
                    }
                }
                _ => {
                    return Err(LanguageTextProjectionError::ChangedSegmentKind { segment_index });
                }
            }
        }
        if ordered_bindings.next().is_some() {
            return Err(LanguageTextProjectionError::UnusedOrderedToken);
        }
        Ok(rebuilt)
    }

    #[cfg(test)]
    pub(super) fn scan_passes(&self) -> usize {
        self.scan_passes.load(Ordering::Relaxed)
    }
}

fn is_complete_token_envelope(token: &str) -> bool {
    let Some(payload) = token
        .strip_prefix(placeholder_token::PREFIX)
        .and_then(|token| token.strip_suffix(placeholder_token::SUFFIX))
    else {
        return false;
    };
    !payload.contains(placeholder_token::SUFFIX)
}

fn record_match(
    matches: &mut HashMap<usize, TokenMatches>,
    token_index: usize,
    start: usize,
    end: usize,
) {
    let matched = matches.entry(token_index).or_insert(TokenMatches {
        count: 0,
        first_start: start,
        first_end: end,
    });
    matched.count += 1;
}

#[derive(Clone, Copy)]
struct TokenMatches {
    count: usize,
    first_start: usize,
    first_end: usize,
}

pub(super) struct PlaceholderTextScan {
    source_bytes: usize,
    empty_token_occurrences: usize,
    matches: HashMap<usize, TokenMatches>,
    envelope_scan: EnvelopeScan,
}

enum EnvelopeScan {
    Complete(Vec<ScannedEnvelope>),
    Unclosed(String),
}

enum ScannedEnvelope {
    Known(usize),
    Unknown(String),
}

pub(super) struct PlaceholderProjection {
    language_text: LanguageText,
    ordered_binding_indices: Vec<usize>,
    source_bytes: usize,
}

impl PlaceholderProjection {
    pub(super) fn language_text(&self) -> &LanguageText {
        &self.language_text
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PlaceholderMultisetError {
    Mismatch { token: String },
    Unexpected { token: String },
}

#[derive(Clone, Copy)]
enum OpaqueRebuild {
    #[cfg(test)]
    Token,
    Original,
}

/// 受信占位符绑定与受保护文本不再一致，无法安全建立语言视图。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LanguageTextProjectionError {
    TokenIndexConstruction,
    EmptyToken,
    MissingToken { token: String },
    RepeatedToken { token: String },
    OverlappingToken { token: String },
    ChangedSegmentCount { expected: usize, actual: usize },
    ChangedSegmentKind { segment_index: usize },
    MissingOrderedToken { segment_index: usize },
    UnusedOrderedToken,
}

impl fmt::Display for LanguageTextProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenIndexConstruction => {
                formatter.write_str("无法为占位符 token 建立多模式索引")
            }
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
            Self::UnusedOrderedToken => formatter.write_str("重建完成后仍有未使用的 token"),
        }
    }
}

impl Error for LanguageTextProjectionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::translate::standard::{PlaceholderRuleOrigin, PlaceholderSegment};

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

    #[test]
    fn indexed_projection_matches_the_reference_semantics() {
        let placeholders = (0..128)
            .map(|index| applied(&format!("⟦ATT_TEST_WHOLE_{index:04}⟧")))
            .collect::<Vec<_>>();
        let text = placeholders
            .iter()
            .rev()
            .enumerate()
            .map(|(index, placeholder)| format!("自然{index}{}", placeholder.token()))
            .collect::<String>();

        assert_eq!(
            project_protected_text(&text, &placeholders),
            reference_project(&text, &placeholders)
        );
        for candidate in [
            text.replacen(placeholders[0].token(), "", 1),
            format!("{text}{}", placeholders[0].token()),
        ] {
            assert_eq!(
                project_protected_text(&candidate, &placeholders),
                reference_project(&candidate, &placeholders)
            );
        }
    }

    #[test]
    fn validation_projection_and_restoration_share_one_large_token_scan() {
        let placeholders = (0..8_192)
            .map(|index| {
                applied_with_original(
                    &format!("⟦ATT_TEST_WHOLE_{index:04}⟧"),
                    &format!("<原片段{index}>"),
                )
            })
            .collect::<Vec<_>>();
        let text = placeholders
            .iter()
            .enumerate()
            .map(|(index, placeholder)| format!("字{index}{}", placeholder.token()))
            .collect::<String>();
        let bindings = PlaceholderBindingIndex::new(&placeholders).expect("token 索引应可建立");
        let scanned = bindings.scan(&text);

        bindings
            .validate_multiset(
                std::slice::from_ref(&scanned),
                bindings.all_binding_indices(),
            )
            .expect("token 集合应一致");
        let projected = bindings
            .project(&text, &scanned, bindings.all_binding_indices())
            .expect("token 应可投影");
        let restored = bindings
            .rebuild_original(&projected, projected.language_text())
            .expect("原片段应可直接恢复");

        assert_eq!(bindings.scan_passes(), 1);
        assert!(!placeholder_token::contains_reserved_prefix(&restored));
        assert!(restored.contains("<原片段8191>"));
    }

    fn reference_project(
        protected_text: &str,
        placeholders: &[AppliedPlaceholder],
    ) -> Result<LanguageText, LanguageTextProjectionError> {
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
        let mut cursor = 0usize;
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
            cursor = end;
        }
        if cursor < protected_text.len() {
            segments.push(LanguageTextSegment::NaturalText(
                protected_text[cursor..].to_owned(),
            ));
        }
        Ok(LanguageText::new(segments))
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
