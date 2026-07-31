//! 把翻译阶段的 Placeholder 文本投影为引擎无关的语言视图。

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

use super::placeholder::AppliedPlaceholder;
use super::placeholder_token;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageText, LanguageTextSegment};

const PROJECTION_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

/// 从已保护文本建立语言模块可见的自然文本与不透明边界。
///
/// 占位符 token 及其原片段都不会进入语言视图；token 两侧始终由一个不透明边界
/// 分隔，不能因为隐藏内部协议而被重新拼接成另一段自然文本。
#[cfg(test)]
pub(crate) fn project_protected_text(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<LanguageText, LanguageTextProjectionError> {
    match project_protected_text_with_cancellation(protected_text, placeholders, || {
        Ok::<_, Infallible>(())
    }) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

pub(crate) fn project_protected_text_with_cancellation<E>(
    protected_text: &str,
    placeholders: &[AppliedPlaceholder],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<LanguageText, LanguageTextProjectionError>, E> {
    let bindings =
        match PlaceholderBindingIndex::new_with_cancellation(placeholders, &mut ensure_running)? {
            Ok(bindings) => bindings,
            Err(source) => return Ok(Err(source)),
        };
    let scanned = bindings.scan_with_cancellation(protected_text, &mut ensure_running)?;
    let projected = match bindings.project_with_cancellation(
        protected_text,
        &scanned,
        bindings.all_binding_indices(),
        &mut ensure_running,
    )? {
        Ok(projected) => projected,
        Err(source) => return Ok(Err(source)),
    };
    Ok(Ok(projected.language_text))
}

pub(crate) fn project_protected_text_from_shared_with_cancellation<E>(
    protected_text: &str,
    placeholders: Arc<Vec<AppliedPlaceholder>>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<LanguageText, LanguageTextProjectionError>, E> {
    let bindings = match PlaceholderBindingIndex::from_vec_shared_with_cancellation(
        placeholders,
        &mut ensure_running,
    )? {
        Ok(bindings) => bindings,
        Err(source) => return Ok(Err(source)),
    };
    let scanned = bindings.scan_with_cancellation(protected_text, &mut ensure_running)?;
    let projected = match bindings.project_with_cancellation(
        protected_text,
        &scanned,
        bindings.all_binding_indices(),
        &mut ensure_running,
    )? {
        Ok(projected) => projected,
        Err(source) => return Ok(Err(source)),
    };
    Ok(Ok(projected.language_text))
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
    bindings.rebuild(&projected, repaired_text)
}

/// 同一候选内可复用的占位符 binding 索引。
///
/// 正常 ATT token 由保留信封扫描直接定位；只有测试或内部不变量破坏产生的
/// 非信封 token 才需要额外的多模式匹配器。索引本身不限制 binding 总量。
#[derive(Clone)]
pub(crate) struct PlaceholderBindingIndex {
    placeholders: SharedPlaceholders,
    tokens: Vec<String>,
    token_fingerprint_to_indices: HashMap<Sha256Fingerprint, Vec<usize>>,
    binding_token_indices: Vec<usize>,
    token_binding_indices: Vec<Vec<usize>>,
    all_binding_indices: Vec<usize>,
    non_envelope_matcher: Option<AhoCorasick>,
    non_envelope_pattern_tokens: Vec<usize>,
    empty_token_index: Option<usize>,
    #[cfg(test)]
    scan_passes: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum SharedPlaceholders {
    Slice(Arc<[AppliedPlaceholder]>),
    Vec(Arc<Vec<AppliedPlaceholder>>),
}

impl SharedPlaceholders {
    fn as_slice(&self) -> &[AppliedPlaceholder] {
        match self {
            Self::Slice(placeholders) => placeholders,
            Self::Vec(placeholders) => placeholders.as_slice(),
        }
    }
}

impl std::ops::Deref for SharedPlaceholders {
    type Target = [AppliedPlaceholder];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl fmt::Debug for PlaceholderBindingIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaceholderBindingIndex")
            .field("placeholders", &self.placeholders.as_slice())
            .finish_non_exhaustive()
    }
}

impl PartialEq for PlaceholderBindingIndex {
    fn eq(&self, other: &Self) -> bool {
        self.placeholders.as_slice() == other.placeholders.as_slice()
    }
}

impl Eq for PlaceholderBindingIndex {}

impl PlaceholderBindingIndex {
    #[cfg(test)]
    pub(crate) fn new(
        placeholders: &[AppliedPlaceholder],
    ) -> Result<Self, LanguageTextProjectionError> {
        match Self::new_with_cancellation(placeholders, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn new_with_cancellation<E>(
        placeholders: &[AppliedPlaceholder],
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, LanguageTextProjectionError>, E> {
        let mut owned = Vec::with_capacity(placeholders.len());
        for placeholder in placeholders {
            ensure_running()?;
            owned.push(clone_applied_placeholder_with_cancellation(
                placeholder,
                &mut ensure_running,
            )?);
        }
        Self::from_vec_shared_with_cancellation(Arc::new(owned), ensure_running)
    }

    pub(crate) fn from_shared_with_cancellation<E>(
        placeholders: Arc<[AppliedPlaceholder]>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, LanguageTextProjectionError>, E> {
        Self::build_with_cancellation(SharedPlaceholders::Slice(placeholders), ensure_running)
    }

    pub(crate) fn from_vec_shared_with_cancellation<E>(
        placeholders: Arc<Vec<AppliedPlaceholder>>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, LanguageTextProjectionError>, E> {
        Self::build_with_cancellation(SharedPlaceholders::Vec(placeholders), ensure_running)
    }

    fn build_with_cancellation<E>(
        placeholders: SharedPlaceholders,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, LanguageTextProjectionError>, E> {
        let placeholder_slice = placeholders.as_slice();
        let mut tokens = Vec::<String>::new();
        let mut unique = HashMap::<Sha256Fingerprint, Vec<usize>>::new();
        for placeholder in placeholder_slice {
            ensure_running()?;
            let token = placeholder.token();
            let fingerprint =
                projection_text_fingerprint_with_cancellation(token, &mut ensure_running)?;
            let mut exists = false;
            if let Some(candidates) = unique.get(&fingerprint) {
                for candidate in candidates {
                    if projection_text_equal_with_cancellation(
                        &tokens[*candidate],
                        token,
                        &mut ensure_running,
                    )? {
                        exists = true;
                        break;
                    }
                }
            }
            if !exists {
                let token_index = tokens.len();
                tokens.push(clone_projection_text_with_cancellation(
                    token,
                    &mut ensure_running,
                )?);
                unique.entry(fingerprint).or_default().push(token_index);
            }
        }
        drop(unique);
        stable_sort_projection_strings_with_cancellation(&mut tokens, &mut ensure_running)?;

        let mut token_fingerprint_to_indices =
            HashMap::<Sha256Fingerprint, Vec<usize>>::with_capacity(tokens.len());
        for (token_index, token) in tokens.iter().enumerate() {
            ensure_running()?;
            let fingerprint =
                projection_text_fingerprint_with_cancellation(token, &mut ensure_running)?;
            token_fingerprint_to_indices
                .entry(fingerprint)
                .or_default()
                .push(token_index);
        }
        let mut token_binding_indices = Vec::with_capacity(tokens.len());
        for _ in 0..tokens.len() {
            ensure_running()?;
            token_binding_indices.push(Vec::new());
        }
        let mut binding_token_indices = Vec::with_capacity(placeholder_slice.len());
        let mut all_binding_indices = Vec::with_capacity(placeholder_slice.len());
        for (binding_index, placeholder) in placeholder_slice.iter().enumerate() {
            ensure_running()?;
            let token_index = find_token_index_with_cancellation(
                &tokens,
                &token_fingerprint_to_indices,
                placeholder.token(),
                &mut ensure_running,
            )?
            .expect("刚建立的 token 索引必须包含每个 binding");
            binding_token_indices.push(token_index);
            token_binding_indices[token_index].push(binding_index);
            all_binding_indices.push(binding_index);
        }
        let empty_token_index = tokens
            .first()
            .is_some_and(|token| token.is_empty())
            .then_some(0);

        let mut non_envelope_patterns = Vec::new();
        let mut non_envelope_pattern_tokens = Vec::new();
        for (token_index, token) in tokens.iter().enumerate() {
            ensure_running()?;
            if !token.is_empty()
                && !is_complete_token_envelope_with_cancellation(token, &mut ensure_running)?
            {
                non_envelope_patterns.push(token.as_str());
                non_envelope_pattern_tokens.push(token_index);
            }
        }
        let non_envelope_matcher = if non_envelope_patterns.is_empty() {
            None
        } else {
            // 生产保护路径只生成标准 ATT 信封；该 matcher 仅支持测试与内部不变量诊断。
            // Aho-Corasick 构建没有取消回调，因此只能在进入和返回时检查。
            ensure_running()?;
            Some(
                match AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .build(non_envelope_patterns)
                {
                    Ok(matcher) => matcher,
                    Err(_) => {
                        return Ok(Err(LanguageTextProjectionError::TokenIndexConstruction));
                    }
                },
            )
        };
        ensure_running()?;

        Ok(Ok(Self {
            placeholders,
            tokens,
            token_fingerprint_to_indices,
            binding_token_indices,
            token_binding_indices,
            all_binding_indices,
            non_envelope_matcher,
            non_envelope_pattern_tokens,
            empty_token_index,
            #[cfg(test)]
            scan_passes: Arc::new(AtomicUsize::new(0)),
        }))
    }

    pub(crate) fn all_binding_indices(&self) -> &[usize] {
        &self.all_binding_indices
    }

    #[cfg(test)]
    pub(crate) fn scan(&self, text: &str) -> PlaceholderTextScan {
        match self.scan_with_cancellation(text, || Ok::<_, Infallible>(())) {
            Ok(scanned) => scanned,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn scan_with_cancellation<E>(
        &self,
        text: &str,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<PlaceholderTextScan, E> {
        ensure_running()?;
        #[cfg(test)]
        self.scan_passes.fetch_add(1, Ordering::Relaxed);

        let mut matches = HashMap::<usize, TokenMatches>::new();
        let mut token_occurrences = Vec::new();
        let mut token_ranges = Vec::new();
        if let Some(matcher) = &self.non_envelope_matcher {
            let mut last_ends = Vec::with_capacity(self.non_envelope_pattern_tokens.len());
            for _ in 0..self.non_envelope_pattern_tokens.len() {
                ensure_running()?;
                last_ends.push(0_usize);
            }
            let mut iterator = matcher.find_overlapping_iter(text);
            loop {
                ensure_running()?;
                let Some(found) = iterator.next() else {
                    break;
                };
                let pattern_index = found.pattern().as_usize();
                if found.start() < last_ends[pattern_index] {
                    continue;
                }
                last_ends[pattern_index] = found.end();
                record_match(
                    &mut matches,
                    &mut token_occurrences,
                    self.non_envelope_pattern_tokens[pattern_index],
                    found.start(),
                    found.end(),
                );
                token_ranges.push((found.start(), found.end()));
            }
        }

        let mut envelopes = Vec::new();
        let mut cursor = 0usize;
        let envelope_scan = loop {
            let Some(start) = find_projection_substring_with_cancellation(
                text,
                cursor,
                placeholder_token::PREFIX,
                &mut ensure_running,
            )?
            else {
                break EnvelopeScan::Complete(envelopes);
            };
            let payload_start = start + placeholder_token::PREFIX.len();
            let Some(suffix_start) = find_projection_substring_with_cancellation(
                text,
                payload_start,
                placeholder_token::SUFFIX,
                &mut ensure_running,
            )?
            else {
                let fragment =
                    clone_projection_text_with_cancellation(&text[start..], &mut ensure_running)?;
                break EnvelopeScan::Unclosed(fragment);
            };
            let end = suffix_start + placeholder_token::SUFFIX.len();
            let token = &text[start..end];
            if let Some(token_index) = find_token_index_with_cancellation(
                &self.tokens,
                &self.token_fingerprint_to_indices,
                token,
                &mut ensure_running,
            )? {
                record_match(
                    &mut matches,
                    &mut token_occurrences,
                    token_index,
                    start,
                    end,
                );
                envelopes.push(ScannedEnvelope::Known(token_index));
            } else {
                envelopes.push(ScannedEnvelope::Unknown(
                    clone_projection_text_with_cancellation(token, &mut ensure_running)?,
                ));
            }
            token_ranges.push((start, end));
            cursor = end;
        };
        stable_sort_positioned_bindings_with_cancellation(
            &mut token_occurrences,
            &mut ensure_running,
        )?;
        sort_and_merge_token_ranges_with_cancellation(&mut token_ranges, &mut ensure_running)?;

        Ok(PlaceholderTextScan {
            empty_token_occurrences: if self.empty_token_index.is_some() {
                projection_character_count_with_cancellation(text, &mut ensure_running)?
                    .saturating_add(1)
            } else {
                0
            },
            matches,
            token_occurrences,
            envelope_scan,
            token_ranges,
        })
    }

    pub(crate) fn present_binding_indices_with_cancellation<E>(
        &self,
        scanned: &PlaceholderTextScan,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<usize>, E> {
        let mut present = Vec::new();
        if let Some(token_index) = self.empty_token_index
            && scanned.empty_token_occurrences != 0
        {
            ensure_running()?;
            for &binding_index in &self.token_binding_indices[token_index] {
                ensure_running()?;
                present.push(binding_index);
            }
        }
        for &token_index in scanned.matches.keys() {
            ensure_running()?;
            for &binding_index in &self.token_binding_indices[token_index] {
                ensure_running()?;
                present.push(binding_index);
            }
        }
        stable_sort_projection_usizes_with_cancellation(&mut present, &mut ensure_running)?;
        ensure_running()?;
        Ok(present)
    }

    pub(crate) fn all_binding_token_occurrences_with_cancellation<E>(
        &self,
        scans: &[PlaceholderTextScan],
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<usize>, E> {
        let occurrences =
            self.aggregate_token_occurrences_with_cancellation(scans, &mut ensure_running)?;
        let mut by_binding = Vec::with_capacity(self.binding_token_indices.len());
        for &token_index in &self.binding_token_indices {
            ensure_running()?;
            by_binding.push(occurrences.get(&token_index).copied().unwrap_or(0));
        }
        ensure_running()?;
        Ok(by_binding)
    }

    #[cfg(test)]
    pub(crate) fn validate_multiset(
        &self,
        scans: &[PlaceholderTextScan],
        binding_indices: &[usize],
    ) -> Result<(), PlaceholderMultisetError> {
        match self
            .validate_multiset_with_cancellation(scans, binding_indices, || Ok::<_, Infallible>(()))
        {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn validate_multiset_with_cancellation<E>(
        &self,
        scans: &[PlaceholderTextScan],
        binding_indices: &[usize],
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<(), PlaceholderMultisetError>, E> {
        let mut expected = BTreeMap::<usize, usize>::new();
        for &binding_index in binding_indices {
            ensure_running()?;
            *expected
                .entry(self.binding_token_indices[binding_index])
                .or_default() += 1;
        }
        let actual =
            self.aggregate_token_occurrences_with_cancellation(scans, &mut ensure_running)?;

        for (&token_index, &expected_count) in &expected {
            ensure_running()?;
            let actual_count = actual.get(&token_index).copied().unwrap_or(0);
            if actual_count != expected_count {
                return Ok(Err(PlaceholderMultisetError::Mismatch {
                    token: clone_projection_text_with_cancellation(
                        &self.tokens[token_index],
                        &mut ensure_running,
                    )?,
                }));
            }
        }

        for scan in scans {
            ensure_running()?;
            let envelopes = match &scan.envelope_scan {
                EnvelopeScan::Complete(envelopes) => envelopes,
                EnvelopeScan::Unclosed(fragment) => {
                    return Ok(Err(PlaceholderMultisetError::Unexpected {
                        token: clone_projection_text_with_cancellation(
                            fragment,
                            &mut ensure_running,
                        )?,
                    }));
                }
            };
            for envelope in envelopes {
                ensure_running()?;
                match envelope {
                    ScannedEnvelope::Known(token_index) if expected.contains_key(token_index) => {}
                    ScannedEnvelope::Known(token_index) => {
                        return Ok(Err(PlaceholderMultisetError::Unexpected {
                            token: clone_projection_text_with_cancellation(
                                &self.tokens[*token_index],
                                &mut ensure_running,
                            )?,
                        }));
                    }
                    ScannedEnvelope::Unknown(token) => {
                        return Ok(Err(PlaceholderMultisetError::Unexpected {
                            token: clone_projection_text_with_cancellation(
                                token,
                                &mut ensure_running,
                            )?,
                        }));
                    }
                }
            }
        }
        let mut expected_bindings = binding_indices.iter().copied();
        for scan in scans {
            for &(_, _, actual_token_index) in &scan.token_occurrences {
                ensure_running()?;
                let Some(expected_binding_index) = expected_bindings.next() else {
                    return Ok(Err(PlaceholderMultisetError::Unexpected {
                        token: clone_projection_text_with_cancellation(
                            &self.tokens[actual_token_index],
                            &mut ensure_running,
                        )?,
                    }));
                };
                let expected_token_index = self.binding_token_indices[expected_binding_index];
                if actual_token_index != expected_token_index {
                    return Ok(Err(PlaceholderMultisetError::OrderMismatch {
                        expected_token: clone_projection_text_with_cancellation(
                            &self.tokens[expected_token_index],
                            &mut ensure_running,
                        )?,
                        actual_token: clone_projection_text_with_cancellation(
                            &self.tokens[actual_token_index],
                            &mut ensure_running,
                        )?,
                    }));
                }
            }
        }
        if let Some(expected_binding_index) = expected_bindings.next() {
            let expected_token_index = self.binding_token_indices[expected_binding_index];
            return Ok(Err(PlaceholderMultisetError::Mismatch {
                token: clone_projection_text_with_cancellation(
                    &self.tokens[expected_token_index],
                    &mut ensure_running,
                )?,
            }));
        }
        ensure_running()?;
        Ok(Ok(()))
    }

    fn aggregate_token_occurrences_with_cancellation<E>(
        &self,
        scans: &[PlaceholderTextScan],
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<HashMap<usize, usize>, E> {
        let mut occurrences = HashMap::<usize, usize>::new();
        let mut empty_occurrences = 0_usize;
        for scan in scans {
            ensure_running()?;
            empty_occurrences = empty_occurrences.wrapping_add(scan.empty_token_occurrences);
            for (&token_index, matched) in &scan.matches {
                ensure_running()?;
                let total = occurrences.entry(token_index).or_default();
                *total = total.wrapping_add(matched.count);
            }
        }
        if let Some(token_index) = self.empty_token_index {
            ensure_running()?;
            occurrences.insert(token_index, empty_occurrences);
        }
        ensure_running()?;
        Ok(occurrences)
    }

    #[cfg(test)]
    pub(crate) fn project(
        &self,
        protected_text: &str,
        scanned: &PlaceholderTextScan,
        binding_indices: &[usize],
    ) -> Result<PlaceholderProjection, LanguageTextProjectionError> {
        match self.project_with_cancellation(protected_text, scanned, binding_indices, || {
            Ok::<_, Infallible>(())
        }) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn project_with_cancellation<E>(
        &self,
        protected_text: &str,
        scanned: &PlaceholderTextScan,
        binding_indices: &[usize],
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<PlaceholderProjection, LanguageTextProjectionError>, E> {
        let mut positioned = Vec::with_capacity(binding_indices.len());
        for &binding_index in binding_indices {
            ensure_running()?;
            let token = self.placeholders[binding_index].token();
            if token.is_empty() {
                return Ok(Err(LanguageTextProjectionError::EmptyToken));
            }
            let token_index = self.binding_token_indices[binding_index];
            let Some(matched) = scanned.matches.get(&token_index) else {
                return Ok(Err(LanguageTextProjectionError::MissingToken {
                    token: clone_projection_text_with_cancellation(token, &mut ensure_running)?,
                }));
            };
            if matched.count != 1 {
                return Ok(Err(LanguageTextProjectionError::RepeatedToken {
                    token: clone_projection_text_with_cancellation(token, &mut ensure_running)?,
                }));
            }
            positioned.push((matched.first_start, matched.first_end, binding_index));
        }
        stable_sort_positioned_bindings_with_cancellation(&mut positioned, &mut ensure_running)?;
        for (position, ((_, _, actual_binding_index), expected_binding_index)) in
            positioned.iter().zip(binding_indices).enumerate()
        {
            ensure_running()?;
            if actual_binding_index != expected_binding_index {
                return Ok(Err(LanguageTextProjectionError::ChangedTokenOrder {
                    position,
                    expected_token: clone_projection_text_with_cancellation(
                        self.placeholders[*expected_binding_index].token(),
                        &mut ensure_running,
                    )?,
                    actual_token: clone_projection_text_with_cancellation(
                        self.placeholders[*actual_binding_index].token(),
                        &mut ensure_running,
                    )?,
                }));
            }
        }

        let mut segments = Vec::with_capacity(positioned.len().saturating_mul(2) + 1);
        let mut ordered_binding_indices = Vec::with_capacity(positioned.len());
        let mut cursor = 0usize;
        for (start, end, binding_index) in positioned {
            ensure_running()?;
            let token = self.placeholders[binding_index].token();
            if start < cursor {
                return Ok(Err(LanguageTextProjectionError::OverlappingToken {
                    token: clone_projection_text_with_cancellation(token, &mut ensure_running)?,
                }));
            }
            if cursor < start {
                segments.push(LanguageTextSegment::NaturalText(
                    clone_projection_text_with_cancellation(
                        &protected_text[cursor..start],
                        &mut ensure_running,
                    )?,
                ));
            }
            segments.push(LanguageTextSegment::OpaqueBoundary);
            ordered_binding_indices.push(binding_index);
            cursor = end;
        }
        if cursor < protected_text.len() {
            segments.push(LanguageTextSegment::NaturalText(
                clone_projection_text_with_cancellation(
                    &protected_text[cursor..],
                    &mut ensure_running,
                )?,
            ));
        }

        let language_text = LanguageText::new_with_cancellation(segments, &mut ensure_running)?;
        Ok(Ok(PlaceholderProjection {
            language_text,
            ordered_binding_indices,
        }))
    }

    #[cfg(test)]
    pub(crate) fn rebuild_original(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
    ) -> Result<String, LanguageTextProjectionError> {
        self.rebuild(projected, repaired_text)
    }

    pub(crate) fn rebuild_original_with_cancellation<E>(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<String, LanguageTextProjectionError>, E> {
        self.rebuild_with_cancellation(projected, repaired_text, ensure_running)
    }

    #[cfg(test)]
    fn rebuild(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
    ) -> Result<String, LanguageTextProjectionError> {
        match self.rebuild_with_cancellation(projected, repaired_text, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    fn rebuild_with_cancellation<E>(
        &self,
        projected: &PlaceholderProjection,
        repaired_text: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<String, LanguageTextProjectionError>, E> {
        ensure_running()?;
        if projected.language_text.segments().len() != repaired_text.segments().len() {
            return Ok(Err(LanguageTextProjectionError::ChangedSegmentCount {
                expected: projected.language_text.segments().len(),
                actual: repaired_text.segments().len(),
            }));
        }

        let mut rebuilt_capacity = 0_usize;
        let mut ordered_bindings = projected.ordered_binding_indices.iter().copied();
        for (segment_index, (before, after)) in projected
            .language_text
            .segments()
            .iter()
            .zip(repaired_text.segments())
            .enumerate()
        {
            ensure_running()?;
            match (before, after) {
                (
                    LanguageTextSegment::NaturalText(_),
                    LanguageTextSegment::NaturalText(repaired),
                ) => {
                    rebuilt_capacity = rebuilt_capacity
                        .checked_add(repaired.len())
                        .expect("Placeholder 重建结果长度必须能由 usize 表示");
                }
                (LanguageTextSegment::OpaqueBoundary, LanguageTextSegment::OpaqueBoundary) => {
                    let Some(binding_index) = ordered_bindings.next() else {
                        return Ok(Err(LanguageTextProjectionError::MissingOrderedToken {
                            segment_index,
                        }));
                    };
                    let binding = &self.placeholders[binding_index];
                    let opaque = binding.original();
                    rebuilt_capacity = rebuilt_capacity
                        .checked_add(opaque.len())
                        .expect("Placeholder 重建结果长度必须能由 usize 表示");
                }
                _ => {
                    return Ok(Err(LanguageTextProjectionError::ChangedSegmentKind {
                        segment_index,
                    }));
                }
            }
        }
        if ordered_bindings.next().is_some() {
            return Ok(Err(LanguageTextProjectionError::UnusedOrderedToken));
        }

        let mut rebuilt = String::with_capacity(rebuilt_capacity);
        let mut ordered_bindings = projected.ordered_binding_indices.iter().copied();
        for (segment_index, (before, after)) in projected
            .language_text
            .segments()
            .iter()
            .zip(repaired_text.segments())
            .enumerate()
        {
            ensure_running()?;
            match (before, after) {
                (
                    LanguageTextSegment::NaturalText(_),
                    LanguageTextSegment::NaturalText(repaired),
                ) => append_projection_text_with_cancellation(
                    &mut rebuilt,
                    repaired,
                    &mut ensure_running,
                )?,
                (LanguageTextSegment::OpaqueBoundary, LanguageTextSegment::OpaqueBoundary) => {
                    let Some(binding_index) = ordered_bindings.next() else {
                        return Ok(Err(LanguageTextProjectionError::MissingOrderedToken {
                            segment_index,
                        }));
                    };
                    let binding = &self.placeholders[binding_index];
                    let opaque = binding.original();
                    append_projection_text_with_cancellation(
                        &mut rebuilt,
                        opaque,
                        &mut ensure_running,
                    )?;
                }
                _ => {
                    return Ok(Err(LanguageTextProjectionError::ChangedSegmentKind {
                        segment_index,
                    }));
                }
            }
        }
        if ordered_bindings.next().is_some() {
            return Ok(Err(LanguageTextProjectionError::UnusedOrderedToken));
        }
        ensure_running()?;
        Ok(Ok(rebuilt))
    }

    #[cfg(test)]
    pub(crate) fn scan_passes(&self) -> usize {
        self.scan_passes.load(Ordering::Relaxed)
    }
}

fn clone_applied_placeholder_with_cancellation<E>(
    placeholder: &AppliedPlaceholder,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<AppliedPlaceholder, E> {
    Ok(AppliedPlaceholder::new(
        clone_projection_text_with_cancellation(placeholder.token(), ensure_running)?,
        clone_projection_text_with_cancellation(placeholder.original(), ensure_running)?,
        placeholder.origin(),
        clone_projection_text_with_cancellation(placeholder.label(), ensure_running)?,
        clone_projection_text_with_cancellation(placeholder.scope(), ensure_running)?,
        placeholder.segment(),
    ))
}

fn clone_projection_text_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut cloned = String::with_capacity(text.len());
    append_projection_text_with_cancellation(&mut cloned, text, ensure_running)?;
    Ok(cloned)
}

fn append_projection_text_with_cancellation<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(PROJECTION_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
}

fn projection_text_fingerprint_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size =
        NonZeroUsize::new(PROJECTION_CANCELLATION_CHECK_BYTES).expect("检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.placeholder-projection-token");
    hasher.try_frame_chunks(1, text.as_bytes(), chunk_size, ensure_running)?;
    Ok(hasher.finish())
}

fn projection_text_equal_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    Ok(projection_text_cmp_with_cancellation(left, right, ensure_running)? == CmpOrdering::Equal)
}

fn projection_text_cmp_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<CmpOrdering, E> {
    for (left, right) in left
        .as_bytes()
        .chunks(PROJECTION_CANCELLATION_CHECK_BYTES)
        .zip(right.as_bytes().chunks(PROJECTION_CANCELLATION_CHECK_BYTES))
    {
        ensure_running()?;
        let ordering = left.cmp(right);
        if ordering != CmpOrdering::Equal {
            return Ok(ordering);
        }
    }
    ensure_running()?;
    Ok(left.len().cmp(&right.len()))
}

fn stable_sorted_order_with_cancellation<E, F, C>(
    length: usize,
    ensure_running: &mut F,
    mut compare: C,
) -> Result<Vec<usize>, E>
where
    F: FnMut() -> Result<(), E>,
    C: FnMut(usize, usize, &mut F) -> Result<CmpOrdering, E>,
{
    let mut order = Vec::with_capacity(length);
    let mut scratch = Vec::with_capacity(length);
    for index in 0..length {
        ensure_running()?;
        order.push(index);
        scratch.push(0_usize);
    }

    let mut width = 1_usize;
    while width < length {
        let run_width = width.saturating_mul(2);
        let mut run_start = 0_usize;
        while run_start < length {
            let middle = run_start.saturating_add(width).min(length);
            let run_end = run_start.saturating_add(run_width).min(length);
            let mut left = run_start;
            let mut right = middle;
            let mut output = run_start;
            while output < run_end {
                ensure_running()?;
                let take_left = right == run_end
                    || (left < middle
                        && compare(order[left], order[right], ensure_running)?
                            != CmpOrdering::Greater);
                scratch[output] = if take_left {
                    let index = order[left];
                    left += 1;
                    index
                } else {
                    let index = order[right];
                    right += 1;
                    index
                };
                output += 1;
            }
            run_start = run_end;
        }
        std::mem::swap(&mut order, &mut scratch);
        width = run_width;
    }
    ensure_running()?;
    Ok(order)
}

fn apply_projection_order_with_cancellation<T, E>(
    values: &mut [T],
    order: Vec<usize>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut target_position = Vec::with_capacity(order.len());
    for _ in 0..order.len() {
        ensure_running()?;
        target_position.push(0_usize);
    }
    for (new_position, original_position) in order.into_iter().enumerate() {
        ensure_running()?;
        target_position[original_position] = new_position;
    }
    for position in 0..values.len() {
        while target_position[position] != position {
            ensure_running()?;
            let destination = target_position[position];
            values.swap(position, destination);
            target_position.swap(position, destination);
        }
    }
    ensure_running()
}

fn stable_sort_projection_strings_with_cancellation<E>(
    values: &mut [String],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let order = stable_sorted_order_with_cancellation(
        values.len(),
        ensure_running,
        |left, right, ensure_running| {
            projection_text_cmp_with_cancellation(&values[left], &values[right], ensure_running)
        },
    )?;
    apply_projection_order_with_cancellation(values, order, ensure_running)
}

fn stable_sort_positioned_bindings_with_cancellation<E>(
    values: &mut [(usize, usize, usize)],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let order =
        stable_sorted_order_with_cancellation(values.len(), ensure_running, |left, right, _| {
            Ok(values[left].0.cmp(&values[right].0))
        })?;
    apply_projection_order_with_cancellation(values, order, ensure_running)
}

fn stable_sort_projection_usizes_with_cancellation<E>(
    values: &mut [usize],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let order =
        stable_sorted_order_with_cancellation(values.len(), ensure_running, |left, right, _| {
            Ok(values[left].cmp(&values[right]))
        })?;
    apply_projection_order_with_cancellation(values, order, ensure_running)
}

fn sort_and_merge_token_ranges_with_cancellation<E>(
    ranges: &mut Vec<(usize, usize)>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let order =
        stable_sorted_order_with_cancellation(ranges.len(), ensure_running, |left, right, _| {
            Ok(ranges[left].cmp(&ranges[right]))
        })?;
    apply_projection_order_with_cancellation(ranges, order, ensure_running)?;

    let mut output = 0_usize;
    for input in 0..ranges.len() {
        ensure_running()?;
        let current = ranges[input];
        if output != 0 && current.0 <= ranges[output - 1].1 {
            ranges[output - 1].1 = ranges[output - 1].1.max(current.1);
        } else {
            ranges[output] = current;
            output += 1;
        }
    }
    ranges.truncate(output);
    ensure_running()
}

fn find_token_index_with_cancellation<E>(
    tokens: &[String],
    lookup: &HashMap<Sha256Fingerprint, Vec<usize>>,
    token: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    let fingerprint = projection_text_fingerprint_with_cancellation(token, ensure_running)?;
    let Some(candidates) = lookup.get(&fingerprint) else {
        ensure_running()?;
        return Ok(None);
    };
    for candidate in candidates {
        if projection_text_equal_with_cancellation(&tokens[*candidate], token, ensure_running)? {
            return Ok(Some(*candidate));
        }
    }
    ensure_running()?;
    Ok(None)
}

fn find_projection_substring_with_cancellation<E>(
    text: &str,
    start: usize,
    needle: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    debug_assert!(text.is_char_boundary(start));
    debug_assert!(!needle.is_empty());
    if text.len().saturating_sub(start) < needle.len() {
        ensure_running()?;
        return Ok(None);
    }

    let overlap_bytes = needle.len().saturating_sub(1);
    let mut chunk_start = start;
    while chunk_start <= text.len() - needle.len() {
        ensure_running()?;
        let mut primary_end = chunk_start
            .saturating_add(PROJECTION_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while primary_end < text.len() && !text.is_char_boundary(primary_end) {
            primary_end -= 1;
        }
        let mut search_end = primary_end.saturating_add(overlap_bytes).min(text.len());
        while search_end < text.len() && !text.is_char_boundary(search_end) {
            search_end += 1;
        }
        if let Some(relative) = text[chunk_start..search_end].find(needle) {
            return Ok(Some(chunk_start + relative));
        }
        if primary_end == text.len() {
            break;
        }
        chunk_start = primary_end;
    }
    ensure_running()?;
    Ok(None)
}

fn projection_character_count_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    let mut count = 0_usize;
    for chunk in text.as_bytes().chunks(PROJECTION_CANCELLATION_CHECK_BYTES) {
        ensure_running()?;
        count = count.wrapping_add(
            chunk
                .iter()
                .filter(|byte| (**byte & 0b1100_0000) != 0b1000_0000)
                .count(),
        );
    }
    ensure_running()?;
    Ok(count)
}

fn is_complete_token_envelope_with_cancellation<E>(
    token: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let Some(payload) = token
        .strip_prefix(placeholder_token::PREFIX)
        .and_then(|token| token.strip_suffix(placeholder_token::SUFFIX))
    else {
        ensure_running()?;
        return Ok(false);
    };
    Ok(find_projection_substring_with_cancellation(
        payload,
        0,
        placeholder_token::SUFFIX,
        ensure_running,
    )?
    .is_none())
}

fn record_match(
    matches: &mut HashMap<usize, TokenMatches>,
    token_occurrences: &mut Vec<(usize, usize, usize)>,
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
    token_occurrences.push((start, end, token_index));
}

#[derive(Clone, Copy)]
struct TokenMatches {
    count: usize,
    first_start: usize,
    first_end: usize,
}

pub(crate) struct PlaceholderTextScan {
    empty_token_occurrences: usize,
    matches: HashMap<usize, TokenMatches>,
    token_occurrences: Vec<(usize, usize, usize)>,
    envelope_scan: EnvelopeScan,
    token_ranges: Vec<(usize, usize)>,
}

impl PlaceholderTextScan {
    pub(crate) fn token_ranges(&self) -> &[(usize, usize)] {
        &self.token_ranges
    }
}

enum EnvelopeScan {
    Complete(Vec<ScannedEnvelope>),
    Unclosed(String),
}

enum ScannedEnvelope {
    Known(usize),
    Unknown(String),
}

pub(crate) struct PlaceholderProjection {
    language_text: LanguageText,
    ordered_binding_indices: Vec<usize>,
}

impl PlaceholderProjection {
    pub(crate) fn language_text(&self) -> &LanguageText {
        &self.language_text
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlaceholderMultisetError {
    Mismatch {
        token: String,
    },
    Unexpected {
        token: String,
    },
    OrderMismatch {
        expected_token: String,
        actual_token: String,
    },
}

impl fmt::Display for PlaceholderMultisetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch { token } => {
                write!(formatter, "Placeholder token 数量不一致：{token:?}")
            }
            Self::Unexpected { token } => {
                write!(formatter, "出现未预期的 Placeholder token：{token:?}")
            }
            Self::OrderMismatch {
                expected_token,
                actual_token,
            } => write!(
                formatter,
                "Placeholder token 顺序不一致：预期 {expected_token:?}，实际 {actual_token:?}"
            ),
        }
    }
}

impl Error for PlaceholderMultisetError {}

/// 受信占位符绑定与受保护文本不再一致，无法安全建立语言视图。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LanguageTextProjectionError {
    TokenIndexConstruction,
    EmptyToken,
    MissingToken {
        token: String,
    },
    RepeatedToken {
        token: String,
    },
    OverlappingToken {
        token: String,
    },
    ChangedTokenOrder {
        position: usize,
        expected_token: String,
        actual_token: String,
    },
    ChangedSegmentCount {
        expected: usize,
        actual: usize,
    },
    ChangedSegmentKind {
        segment_index: usize,
    },
    MissingOrderedToken {
        segment_index: usize,
    },
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
            Self::ChangedTokenOrder {
                position,
                expected_token,
                actual_token,
            } => write!(
                formatter,
                "第 {position} 个占位符 token 顺序改变：预期 {expected_token:?}，实际 {actual_token:?}"
            ),
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
    use crate::translation::placeholder::{PlaceholderRuleOrigin, PlaceholderSegment};

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
    fn projection_rejects_reordered_tokens() {
        let first = applied("<first>");
        let second = applied("<second>");
        let translated = "前<second>中<first>后";

        assert!(matches!(
            project_protected_text(translated, &[first, second]),
            Err(LanguageTextProjectionError::ChangedTokenOrder { position: 0, .. })
        ));
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

    #[test]
    fn many_line_token_queries_scale_with_actual_matches() {
        const PLACEHOLDER_COUNT: usize = 2_048;

        let placeholders = (0..PLACEHOLDER_COUNT)
            .map(|index| applied(&format!("⟦ATT_TEST_WHOLE_{index:04}⟧")))
            .collect::<Vec<_>>();
        let bindings = PlaceholderBindingIndex::new(&placeholders).expect("token 索引应可建立");
        let scans = placeholders
            .iter()
            .map(|placeholder| bindings.scan(placeholder.token()))
            .collect::<Vec<_>>();

        let mut validation_polls = 0_usize;
        let validation = bindings
            .validate_multiset_with_cancellation(&scans, bindings.all_binding_indices(), || {
                validation_polls += 1;
                Ok::<_, Infallible>(())
            })
            .expect("测试没有请求取消");
        assert_eq!(validation, Ok(()));
        assert!(
            validation_polls < PLACEHOLDER_COUNT * 8,
            "多重集验收应一次聚合每行的实际匹配，polls={validation_polls}"
        );

        let mut presence_polls = 0_usize;
        for (binding_index, scan) in scans.iter().enumerate() {
            let present = bindings
                .present_binding_indices_with_cancellation(scan, || {
                    presence_polls += 1;
                    Ok::<_, Infallible>(())
                })
                .expect("测试没有请求取消");
            assert_eq!(present, [binding_index]);
        }
        assert!(
            presence_polls < PLACEHOLDER_COUNT * 16,
            "逐行索引应只遍历实际出现的 token，polls={presence_polls}"
        );
    }

    #[test]
    fn index_construction_can_cancel_while_cloning_a_long_original_fragment() {
        let placeholder = applied_with_original(
            "⟦ATT_TEST_WHOLE_0000⟧",
            &"x".repeat(PROJECTION_CANCELLATION_CHECK_BYTES * 4),
        );
        let mut polls = 0_usize;
        let result = PlaceholderBindingIndex::new_with_cancellation(
            std::slice::from_ref(&placeholder),
            || {
                polls += 1;
                if polls == 5 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 5);
    }

    #[test]
    fn envelope_scan_can_cancel_inside_a_long_candidate() {
        let placeholder = applied("⟦ATT_TEST_WHOLE_0000⟧");
        let bindings =
            PlaceholderBindingIndex::new(std::slice::from_ref(&placeholder)).expect("索引应有效");
        let candidate = "x".repeat(PROJECTION_CANCELLATION_CHECK_BYTES * 4);
        let mut polls = 0_usize;
        let result = bindings.scan_with_cancellation(&candidate, || {
            polls += 1;
            if polls == 3 { Err("cancelled") } else { Ok(()) }
        });

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 3);
    }

    #[test]
    fn restoration_can_cancel_between_long_original_fragment_copies() {
        let token = "⟦ATT_TEST_WHOLE_0000⟧";
        let placeholder =
            applied_with_original(token, &"x".repeat(PROJECTION_CANCELLATION_CHECK_BYTES * 4));
        let bindings =
            PlaceholderBindingIndex::new(std::slice::from_ref(&placeholder)).expect("索引应有效");
        let scanned = bindings.scan(token);
        let projected = bindings
            .project(token, &scanned, bindings.all_binding_indices())
            .expect("token 应可投影");
        let mut polls = 0_usize;
        let result = bindings.rebuild_original_with_cancellation(
            &projected,
            projected.language_text(),
            || {
                polls += 1;
                if polls == 5 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 5);
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
