//! 各翻译引擎共享的 PCRE2、token、绑定、语言投影与恢复机制。
//!
//! 本模块不解释游戏引擎、kind 枚举或内置控制符。调用方负责校验 scope，并把需要的
//! 内置 pattern 显式交给保护操作。

use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::isolated::{IsolatedOperationError, run_isolated_operation};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::LanguageText;

use super::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderBindingIndex, PlaceholderMultisetError,
};
use super::placeholder_token;

const CUSTOM_SEMANTIC_LABEL: &str = "CUSTOM";
const PLACEHOLDER_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

/// 外部 TOML 中一条自定义 Placeholder 规则的最小表达。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceholderRuleDefinition {
    #[serde(skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
    pattern: String,
}

impl PlaceholderRuleDefinition {
    #[cfg(test)]
    pub(crate) fn new(scopes: Option<Vec<String>>, pattern: impl Into<String>) -> Self {
        Self {
            scopes,
            pattern: pattern.into(),
        }
    }
}

/// 已编译且可以在线程间共享的自定义规则。
#[derive(Clone)]
pub(crate) struct CompiledPlaceholderRules {
    rules: Arc<Vec<CompiledPlaceholderRule>>,
}

impl fmt::Debug for CompiledPlaceholderRules {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledPlaceholderRules")
            .field("rule_count", &self.rules.len())
            .finish()
    }
}

#[derive(Clone)]
struct CompiledPlaceholderRule {
    scopes: Option<Vec<String>>,
    regex: Regex,
    rule_number: usize,
    has_text_capture: bool,
}

struct ValidatedPlaceholderRule {
    scopes: Option<Vec<String>>,
    pattern: String,
    rule_number: usize,
}

/// 由引擎适配器提供的一条已编译内置规则。
#[derive(Clone)]
pub(crate) struct CompiledBuiltinPlaceholderRule {
    regex: Regex,
    semantic_label: &'static str,
}

/// 无状态的公共 Placeholder 算法入口。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlaceholderService;

impl PlaceholderService {
    /// 编译调用方的内置 pattern；公共层不决定哪些引擎拥有内置规则。
    pub(crate) fn compile_builtin(
        &self,
        pattern: &str,
        semantic_label: &'static str,
    ) -> Result<CompiledBuiltinPlaceholderRule, Pcre2PlaceholderConstructionError> {
        Ok(CompiledBuiltinPlaceholderRule {
            regex: compile_regex(pattern).map_err(Pcre2PlaceholderConstructionError)?,
            semantic_label,
        })
    }

    /// 编译自定义规则，并让引擎适配器决定哪些 scope 名称有效。
    #[cfg(test)]
    pub(crate) fn compile_custom(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
        valid_scope: impl FnMut(&str) -> bool,
    ) -> Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError> {
        match self
            .compile_custom_with_cancellation(definitions, valid_scope, || Ok::<_, Infallible>(()))
        {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    /// 编译自定义规则，并在规则与 scope 之间轮询调用方。
    ///
    /// 所有 scope 和空 pattern 校验完成后，把整批已校验规则交给一个隔离 worker。
    /// PCRE2 没有取消回调；调用方取消时不等待当前纯计算结束，且每次调用最多遗留一个
    /// 已经运行的有限 worker。
    pub(crate) fn compile_custom_with_cancellation<E>(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
        mut valid_scope: impl FnMut(&str) -> bool,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError>, E> {
        ensure_running()?;
        let mut validated = Vec::with_capacity(definitions.len());
        for (index, definition) in definitions.into_iter().enumerate() {
            ensure_running()?;
            let rule_number = index + 1;
            match validate_placeholder_rule_with_cancellation(
                definition,
                rule_number,
                &mut valid_scope,
                &mut ensure_running,
            )? {
                Ok(rule) => validated.push(Ok(rule)),
                Err(source) if validated.is_empty() => return Ok(Err(source)),
                Err(source) => {
                    // 先前规则的 PCRE2 错误必须继续优先于当前规则的字段错误。
                    // 把首个字段错误作为终止项交给同一 worker，保持原有规则顺序。
                    validated.push(Err(source));
                    break;
                }
            }
        }
        if validated.is_empty() {
            ensure_running()?;
            return Ok(Ok(CompiledPlaceholderRules {
                rules: Arc::new(Vec::new()),
            }));
        }
        match run_isolated_operation(
            "att-placeholder-regex",
            move || compile_validated_placeholder_rules(validated),
            ensure_running,
        ) {
            Ok(result) => Ok(result),
            Err(IsolatedOperationError::Cancelled(cancellation)) => Err(cancellation),
            Err(IsolatedOperationError::Start { operation, source }) => {
                Ok(Err(PlaceholderRuleCompilationError::StartWorker {
                    operation,
                    source,
                }))
            }
        }
    }

    /// 保护原文；scope 与 builtin 均由调用方提前确定。
    #[cfg(test)]
    pub(crate) fn protect(
        &self,
        scope: &str,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        builtin: Option<&CompiledBuiltinPlaceholderRule>,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        match self.protect_with_cancellation(
            scope,
            original,
            line_separator_offsets,
            custom,
            builtin,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    /// 保护原文，并在公共层执行的扫描、复制和绑定哈希之间轮询调用方。
    ///
    /// 用户自定义 PCRE2 以及长文本上的内置 PCRE2 匹配没有取消回调，因此每次保护最多
    /// 把一个纯匹配批次交给隔离 worker。内置规则来自程序固定的线性 pattern；不超过
    /// 一个取消检查窗口时继续内联，避免空规则和常见短文本为每项启动线程。
    pub(crate) fn protect_with_cancellation<E>(
        &self,
        scope: &str,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        builtin: Option<&CompiledBuiltinPlaceholderRule>,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<ProtectedText, PlaceholderProtectionError>, E> {
        ensure_running()?;
        if contains_reserved_prefix_with_cancellation(original, &mut ensure_running)? {
            return Ok(Err(PlaceholderProtectionError::ReservedTokenNamespace));
        }

        let mut has_applicable_custom_rule = false;
        for rule in custom.rules.iter() {
            ensure_running()?;
            if rule_applies_to_scope_with_cancellation(
                rule.scopes.as_deref(),
                scope,
                &mut ensure_running,
            )? {
                has_applicable_custom_rule = true;
                break;
            }
        }

        let isolate_matching = should_isolate_placeholder_matching(
            original.len(),
            builtin.is_some(),
            has_applicable_custom_rule,
        );
        let owned_matches = if isolate_matching {
            let original = clone_placeholder_text_with_cancellation(original, &mut ensure_running)?;
            let worker_scope = if has_applicable_custom_rule {
                clone_placeholder_text_with_cancellation(scope, &mut ensure_running)?
            } else {
                String::new()
            };
            let builtin = builtin.cloned();
            let custom_rules = Arc::clone(&custom.rules);
            match run_isolated_operation(
                "att-placeholder-match",
                move || {
                    collect_placeholder_matches(
                        builtin.as_ref(),
                        custom_rules.as_slice(),
                        has_applicable_custom_rule,
                        &original,
                        &worker_scope,
                    )
                },
                &mut ensure_running,
            ) {
                Ok(Ok(matches)) => matches,
                Ok(Err(source)) => return Ok(Err(source)),
                Err(IsolatedOperationError::Cancelled(cancellation)) => {
                    return Err(cancellation);
                }
                Err(IsolatedOperationError::Start { operation, source }) => {
                    return Ok(Err(PlaceholderProtectionError::StartWorker {
                        operation,
                        source,
                    }));
                }
            }
        } else {
            match builtin {
                Some(builtin) => match collect_builtin_matches(builtin, original) {
                    Ok(matches) => matches,
                    Err(source) => return Ok(Err(source)),
                },
                None => Vec::new(),
            }
        };

        let mut selected = Vec::with_capacity(owned_matches.len());
        for span in owned_matches {
            ensure_running()?;
            selected.push(span.with_scope(scope));
        }

        stable_sort_selected_spans_with_cancellation(&mut selected, &mut ensure_running)?;
        let mut max_end_span = None;
        for (index, current) in selected.iter().enumerate() {
            ensure_running()?;
            if let Some(previous_index) = max_end_span {
                let previous: &SelectedSpan<'_> = &selected[previous_index];
                if current.start < previous.end {
                    return Ok(Err(PlaceholderProtectionError::OverlappingMatches {
                        first: clone_placeholder_text_with_cancellation(
                            &previous.diagnostic_label,
                            &mut ensure_running,
                        )?,
                        second: clone_placeholder_text_with_cancellation(
                            &current.diagnostic_label,
                            &mut ensure_running,
                        )?,
                    }));
                }
                if current.end > previous.end {
                    max_end_span = Some(index);
                }
            } else {
                max_end_span = Some(index);
            }
        }
        let mut source_line_index = 0;
        for span in &selected {
            ensure_running()?;
            while line_separator_offsets
                .get(source_line_index)
                .is_some_and(|separator| *separator < span.start)
            {
                ensure_running()?;
                source_line_index += 1;
            }
            if line_separator_offsets
                .get(source_line_index)
                .is_some_and(|separator| *separator < span.end)
            {
                return Ok(Err(PlaceholderProtectionError::CrossesLineBoundary {
                    rule_number: span.rule_number,
                    source_line_index,
                }));
            }
        }

        let mut tokens = Vec::with_capacity(selected.len());
        let mut protected_capacity = original.len();
        for (index, span) in selected.iter().enumerate() {
            ensure_running()?;
            let token = semantic_token(span.semantic_label, span.segment, index);
            protected_capacity = protected_capacity
                .checked_sub(span.end - span.start)
                .and_then(|capacity| capacity.checked_add(token.len()))
                .expect("Placeholder 保护结果长度必须能由 usize 表示");
            tokens.push(token);
        }
        let mut protected = String::with_capacity(protected_capacity);
        let mut placeholders = Vec::with_capacity(selected.len());
        let mut cursor = 0;
        for (span, token) in selected.into_iter().zip(tokens) {
            append_placeholder_text_with_cancellation(
                &mut protected,
                &original[cursor..span.start],
                &mut ensure_running,
            )?;
            protected.push_str(&token);
            placeholders.push(AppliedPlaceholder::new(
                token,
                clone_placeholder_text_with_cancellation(
                    &original[span.start..span.end],
                    &mut ensure_running,
                )?,
                span.origin,
                clone_placeholder_text_with_cancellation(span.semantic_label, &mut ensure_running)?,
                clone_placeholder_text_with_cancellation(span.scope, &mut ensure_running)?,
                span.segment,
            ));
            cursor = span.end;
        }
        append_placeholder_text_with_cancellation(
            &mut protected,
            &original[cursor..],
            &mut ensure_running,
        )?;
        Ok(Ok(ProtectedText::new_with_cancellation(
            protected,
            placeholders,
            &mut ensure_running,
        )?))
    }
}

fn compile_validated_placeholder_rules(
    validated: Vec<Result<ValidatedPlaceholderRule, PlaceholderRuleCompilationError>>,
) -> Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError> {
    let mut rules = Vec::with_capacity(validated.len());
    for definition in validated {
        let definition = definition?;
        let regex = compile_regex(&definition.pattern).map_err(|source| {
            PlaceholderRuleCompilationError::InvalidPattern {
                rule_number: definition.rule_number,
                source,
            }
        })?;
        let named_captures = regex
            .capture_names()
            .iter()
            .filter_map(Option::as_deref)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let has_text_capture = match named_captures.as_slice() {
            [] => false,
            [name] if name == "text" => true,
            _ => {
                return Err(PlaceholderRuleCompilationError::InvalidNamedCaptures {
                    rule_number: definition.rule_number,
                    captures: named_captures,
                });
            }
        };
        rules.push(CompiledPlaceholderRule {
            scopes: definition.scopes,
            regex,
            rule_number: definition.rule_number,
            has_text_capture,
        });
    }
    Ok(CompiledPlaceholderRules {
        rules: Arc::new(rules),
    })
}

fn validate_placeholder_rule_with_cancellation<E>(
    definition: PlaceholderRuleDefinition,
    rule_number: usize,
    valid_scope: &mut impl FnMut(&str) -> bool,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<ValidatedPlaceholderRule, PlaceholderRuleCompilationError>, E> {
    let scopes = match definition.scopes {
        Some(scopes) => {
            if scopes.is_empty() {
                return Ok(Err(PlaceholderRuleCompilationError::EmptyScopes {
                    rule_number,
                }));
            }
            let mut unique = HashMap::<Sha256Fingerprint, Vec<usize>>::with_capacity(scopes.len());
            for (scope_index, scope) in scopes.iter().enumerate() {
                ensure_running()?;
                if !valid_scope(scope) {
                    return Ok(Err(PlaceholderRuleCompilationError::UnknownScope {
                        rule_number,
                        scope: clone_placeholder_text_with_cancellation(scope, ensure_running)?,
                    }));
                }
                let fingerprint =
                    placeholder_text_fingerprint_with_cancellation(scope, ensure_running)?;
                let duplicate = if let Some(candidates) = unique.get(&fingerprint) {
                    let mut duplicate = false;
                    for candidate_index in candidates {
                        if placeholder_text_equal_with_cancellation(
                            &scopes[*candidate_index],
                            scope,
                            ensure_running,
                        )? {
                            duplicate = true;
                            break;
                        }
                    }
                    duplicate
                } else {
                    false
                };
                if duplicate {
                    return Ok(Err(PlaceholderRuleCompilationError::DuplicateScope {
                        rule_number,
                        scope: clone_placeholder_text_with_cancellation(scope, ensure_running)?,
                    }));
                }
                unique.entry(fingerprint).or_default().push(scope_index);
            }
            drop(unique);
            Some(scopes)
        }
        None => None,
    };
    if definition.pattern.is_empty() {
        return Ok(Err(PlaceholderRuleCompilationError::EmptyPattern {
            rule_number,
        }));
    }
    Ok(Ok(ValidatedPlaceholderRule {
        scopes,
        pattern: definition.pattern,
        rule_number,
    }))
}

fn clone_placeholder_text_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut output = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(PLACEHOLDER_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()?;
    Ok(output)
}

fn append_placeholder_text_with_cancellation<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(PLACEHOLDER_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
}

fn contains_reserved_prefix_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let overlap_bytes = placeholder_token::PREFIX.len().saturating_sub(1);
    if text.len() < placeholder_token::PREFIX.len() {
        ensure_running()?;
        return Ok(false);
    }

    let mut start = 0_usize;
    while start <= text.len() - placeholder_token::PREFIX.len() {
        ensure_running()?;
        let mut primary_end = start
            .saturating_add(PLACEHOLDER_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while primary_end < text.len() && !text.is_char_boundary(primary_end) {
            primary_end -= 1;
        }
        let mut search_end = primary_end.saturating_add(overlap_bytes).min(text.len());
        while search_end < text.len() && !text.is_char_boundary(search_end) {
            search_end += 1;
        }
        if placeholder_token::contains_reserved_prefix(&text[start..search_end]) {
            return Ok(true);
        }
        if primary_end == text.len() {
            break;
        }
        start = primary_end;
    }
    ensure_running()?;
    Ok(false)
}

fn rule_applies_to_scope_with_cancellation<E>(
    scopes: Option<&[String]>,
    scope: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let Some(scopes) = scopes else {
        ensure_running()?;
        return Ok(true);
    };
    for candidate in scopes {
        if placeholder_text_equal_with_cancellation(candidate, scope, ensure_running)? {
            return Ok(true);
        }
    }
    ensure_running()?;
    Ok(false)
}

fn placeholder_text_equal_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    if left.len() != right.len() {
        ensure_running()?;
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(PLACEHOLDER_CANCELLATION_CHECK_BYTES)
        .zip(
            right
                .as_bytes()
                .chunks(PLACEHOLDER_CANCELLATION_CHECK_BYTES),
        )
    {
        ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

fn placeholder_text_fingerprint_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size =
        NonZeroUsize::new(PLACEHOLDER_CANCELLATION_CHECK_BYTES).expect("检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.placeholder-text");
    hasher.try_frame_chunks(1, text.as_bytes(), chunk_size, ensure_running)?;
    Ok(hasher.finish())
}

fn compile_regex(pattern: &str) -> Result<Regex, pcre2::Error> {
    RegexBuilder::new()
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(pattern)
}

fn should_isolate_placeholder_matching(
    original_bytes: usize,
    has_builtin: bool,
    has_applicable_custom_rule: bool,
) -> bool {
    has_applicable_custom_rule
        || (has_builtin && original_bytes > PLACEHOLDER_CANCELLATION_CHECK_BYTES)
}

fn collect_placeholder_matches(
    builtin: Option<&CompiledBuiltinPlaceholderRule>,
    custom_rules: &[CompiledPlaceholderRule],
    include_custom: bool,
    original: &str,
    scope: &str,
) -> Result<Vec<OwnedSelectedSpan>, PlaceholderProtectionError> {
    let mut selected = match builtin {
        Some(builtin) => collect_builtin_matches(builtin, original)?,
        None => Vec::new(),
    };
    if include_custom {
        for rule in custom_rules {
            if rule_applies_to_scope(rule.scopes.as_deref(), scope) {
                selected.extend(collect_custom_matches(rule, original)?);
            }
        }
    }
    Ok(selected)
}

fn collect_builtin_matches(
    builtin: &CompiledBuiltinPlaceholderRule,
    original: &str,
) -> Result<Vec<OwnedSelectedSpan>, PlaceholderProtectionError> {
    let mut selected = Vec::new();
    for matched in builtin.regex.find_iter(original.as_bytes()) {
        let matched = matched.map_err(PlaceholderProtectionError::Match)?;
        if matched.start() == matched.end() {
            return Err(PlaceholderProtectionError::EmptyMatch {
                label: builtin.semantic_label.to_owned(),
            });
        }
        selected.push(OwnedSelectedSpan {
            start: matched.start(),
            end: matched.end(),
            origin: PlaceholderRuleOrigin::BuiltIn,
            semantic_label: builtin.semantic_label,
            diagnostic_label: builtin.semantic_label.to_owned(),
            rule_number: None,
            segment: PlaceholderSegment::Whole,
        });
    }
    Ok(selected)
}

fn rule_applies_to_scope(scopes: Option<&[String]>, scope: &str) -> bool {
    scopes.is_none_or(|scopes| scopes.iter().any(|candidate| candidate == scope))
}

fn collect_custom_matches(
    rule: &CompiledPlaceholderRule,
    original: &str,
) -> Result<Vec<OwnedSelectedSpan>, PlaceholderProtectionError> {
    let mut result = Vec::new();
    for captures in rule.regex.captures_iter(original.as_bytes()) {
        let captures = captures.map_err(PlaceholderProtectionError::Match)?;
        let whole = captures
            .get(0)
            .expect("PCRE2 成功 captures 必须包含整个匹配");
        if !valid_utf8_range(original, whole.start(), whole.end()) {
            return Err(PlaceholderProtectionError::InvalidMatchRange {
                rule_number: rule.rule_number,
            });
        }
        if whole.start() == whole.end() {
            return Err(PlaceholderProtectionError::EmptyMatch {
                label: custom_diagnostic_label(rule.rule_number),
            });
        }
        let diagnostic_label = custom_diagnostic_label(rule.rule_number);
        let protected = if rule.has_text_capture {
            let capture = match captures.name("text") {
                Some(capture) => capture,
                None => {
                    return Err(PlaceholderProtectionError::MissingTextCapture {
                        rule_number: rule.rule_number,
                    });
                }
            };
            if !valid_utf8_range(original, capture.start(), capture.end())
                || capture.start() < whole.start()
                || capture.end() > whole.end()
            {
                return Err(PlaceholderProtectionError::InvalidMatchRange {
                    rule_number: rule.rule_number,
                });
            }
            let mut protected = Vec::with_capacity(2);
            if whole.start() < capture.start() {
                protected.push(OwnedSelectedSpan {
                    start: whole.start(),
                    end: capture.start(),
                    origin: PlaceholderRuleOrigin::Custom,
                    semantic_label: CUSTOM_SEMANTIC_LABEL,
                    diagnostic_label: diagnostic_label.clone(),
                    rule_number: Some(rule.rule_number),
                    segment: PlaceholderSegment::Begin,
                });
            }
            if capture.end() < whole.end() {
                protected.push(OwnedSelectedSpan {
                    start: capture.end(),
                    end: whole.end(),
                    origin: PlaceholderRuleOrigin::Custom,
                    semantic_label: CUSTOM_SEMANTIC_LABEL,
                    diagnostic_label: diagnostic_label.clone(),
                    rule_number: Some(rule.rule_number),
                    segment: PlaceholderSegment::End,
                });
            }
            protected
        } else {
            vec![OwnedSelectedSpan {
                start: whole.start(),
                end: whole.end(),
                origin: PlaceholderRuleOrigin::Custom,
                semantic_label: CUSTOM_SEMANTIC_LABEL,
                diagnostic_label: diagnostic_label.clone(),
                rule_number: Some(rule.rule_number),
                segment: PlaceholderSegment::Whole,
            }]
        };
        result.extend(protected);
    }
    Ok(result)
}

fn valid_utf8_range(text: &str, start: usize, end: usize) -> bool {
    start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

fn custom_diagnostic_label(rule_number: usize) -> String {
    format!("CUSTOM_{rule_number:04}")
}

struct OwnedSelectedSpan {
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    semantic_label: &'static str,
    diagnostic_label: String,
    rule_number: Option<usize>,
    segment: PlaceholderSegment,
}

impl OwnedSelectedSpan {
    fn with_scope(self, scope: &str) -> SelectedSpan<'_> {
        SelectedSpan {
            start: self.start,
            end: self.end,
            origin: self.origin,
            semantic_label: self.semantic_label,
            diagnostic_label: self.diagnostic_label,
            rule_number: self.rule_number,
            scope,
            segment: self.segment,
        }
    }
}

struct SelectedSpan<'a> {
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    semantic_label: &'static str,
    diagnostic_label: String,
    rule_number: Option<usize>,
    scope: &'a str,
    segment: PlaceholderSegment,
}

fn stable_sort_selected_spans_with_cancellation<E>(
    selected: &mut [SelectedSpan<'_>],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let length = selected.len();
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
                    || (left < middle && {
                        let left_span = &selected[order[left]];
                        let right_span = &selected[order[right]];
                        (left_span.start, left_span.end) <= (right_span.start, right_span.end)
                    });
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

    let mut target_position = Vec::with_capacity(length);
    for _ in 0..length {
        ensure_running()?;
        target_position.push(0_usize);
    }
    for (new_position, original_position) in order.into_iter().enumerate() {
        ensure_running()?;
        target_position[original_position] = new_position;
    }
    drop(scratch);

    for position in 0..length {
        while target_position[position] != position {
            ensure_running()?;
            let destination = target_position[position];
            selected.swap(position, destination);
            target_position.swap(position, destination);
        }
    }
    ensure_running()
}

fn semantic_token(label: &str, segment: PlaceholderSegment, index: usize) -> String {
    placeholder_token::envelope(&format!("{label}_{}_{index:04}", segment.name()))
}

/// Placeholder 来自引擎内置保护还是用户规则。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PlaceholderRuleOrigin {
    BuiltIn,
    Custom,
}

/// Placeholder 对应完整匹配，或 `text` 捕获两侧的外壳。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PlaceholderSegment {
    Whole,
    Begin,
    End,
}

impl PlaceholderSegment {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Whole => "WHOLE",
            Self::Begin => "BEGIN",
            Self::End => "END",
        }
    }
}

/// Planner 建立的一条可逆 Placeholder 绑定。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AppliedPlaceholder {
    token: String,
    original: String,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: String,
    segment: PlaceholderSegment,
}

impl AppliedPlaceholder {
    pub(crate) fn new(
        token: impl Into<String>,
        original: impl Into<String>,
        origin: PlaceholderRuleOrigin,
        label: impl Into<String>,
        scope: impl Into<String>,
        segment: PlaceholderSegment,
    ) -> Self {
        Self {
            token: token.into(),
            original: original.into(),
            origin,
            label: label.into(),
            scope: scope.into(),
            segment,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) const fn origin(&self) -> PlaceholderRuleOrigin {
        self.origin
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    pub(crate) const fn segment(&self) -> PlaceholderSegment {
        self.segment
    }
}

/// 一次保护的完整可逆结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtectedText {
    text: String,
    placeholders: Arc<Vec<AppliedPlaceholder>>,
    binding_fingerprint: Sha256Fingerprint,
}

impl ProtectedText {
    fn new_with_cancellation<E>(
        text: String,
        placeholders: Vec<AppliedPlaceholder>,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let binding_fingerprint =
            placeholder_binding_fingerprint_with_cancellation(&placeholders, ensure_running)?;
        Ok(Self {
            text,
            placeholders: Arc::new(placeholders),
            binding_fingerprint,
        })
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn placeholders(&self) -> &[AppliedPlaceholder] {
        self.placeholders.as_slice()
    }

    pub(crate) fn binding_fingerprint(&self) -> Sha256Fingerprint {
        match self.binding_fingerprint_with_cancellation(|| Ok::<_, Infallible>(())) {
            Ok(fingerprint) => fingerprint,
            Err(unreachable) => match unreachable {},
        }
    }

    /// 指纹在保护阶段已经按块建立；读取只需确认调用方此刻仍允许继续。
    pub(crate) fn binding_fingerprint_with_cancellation<E>(
        &self,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Sha256Fingerprint, E> {
        ensure_running()?;
        Ok(self.binding_fingerprint)
    }

    #[cfg(test)]
    pub(crate) fn language_text(&self) -> Result<LanguageText, LanguageTextProjectionError> {
        match self.language_text_with_cancellation(|| Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn language_text_with_cancellation<E>(
        &self,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<LanguageText, LanguageTextProjectionError>, E> {
        super::placeholder_projection::project_protected_text_from_shared_with_cancellation(
            &self.text,
            Arc::clone(&self.placeholders),
            ensure_running,
        )
    }

    /// 验证 token 数量与原顺序后，按绑定直接交错恢复原片段。
    #[cfg(test)]
    pub(crate) fn restore(&self, candidate: &str) -> Result<String, PlaceholderRestoreError> {
        match self.restore_with_cancellation(candidate, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn restore_with_cancellation<E>(
        &self,
        candidate: &str,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<String, PlaceholderRestoreError>, E> {
        let bindings = match PlaceholderBindingIndex::from_vec_shared_with_cancellation(
            Arc::clone(&self.placeholders),
            &mut ensure_running,
        )? {
            Ok(bindings) => bindings,
            Err(source) => return Ok(Err(PlaceholderRestoreError::Projection(source))),
        };
        let scanned = bindings.scan_with_cancellation(candidate, &mut ensure_running)?;
        if let Err(source) = bindings.validate_multiset_with_cancellation(
            std::slice::from_ref(&scanned),
            bindings.all_binding_indices(),
            &mut ensure_running,
        )? {
            return Ok(Err(PlaceholderRestoreError::Multiset(source)));
        }
        let projected = match bindings.project_with_cancellation(
            candidate,
            &scanned,
            bindings.all_binding_indices(),
            &mut ensure_running,
        )? {
            Ok(projected) => projected,
            Err(source) => return Ok(Err(PlaceholderRestoreError::Projection(source))),
        };
        match bindings.rebuild_original_with_cancellation(
            &projected,
            projected.language_text(),
            ensure_running,
        )? {
            Ok(restored) => Ok(Ok(restored)),
            Err(source) => Ok(Err(PlaceholderRestoreError::Projection(source))),
        }
    }

    pub(crate) fn into_parts(self) -> (String, Vec<AppliedPlaceholder>) {
        let placeholders =
            Arc::try_unwrap(self.placeholders).unwrap_or_else(|shared| shared.as_ref().clone());
        (self.text, placeholders)
    }
}

fn placeholder_binding_fingerprint_with_cancellation<E>(
    bindings: &[AppliedPlaceholder],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size =
        NonZeroUsize::new(PLACEHOLDER_CANCELLATION_CHECK_BYTES).expect("检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.placeholder-bindings");
    for binding in bindings {
        ensure_running()?;
        hasher
            .try_frame_chunks(
                1,
                binding.token().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?
            .try_frame_chunks(
                2,
                binding.original().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?
            .try_frame_chunks(
                3,
                binding.label().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?
            .try_frame_chunks(
                4,
                binding.scope().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?
            .try_frame_chunks(
                5,
                binding.segment().name().as_bytes(),
                chunk_size,
                &mut ensure_running,
            )?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

/// 候选 Placeholder 验收或恢复失败。
#[derive(Debug)]
pub(crate) enum PlaceholderRestoreError {
    Projection(LanguageTextProjectionError),
    Multiset(PlaceholderMultisetError),
}

impl fmt::Display for PlaceholderRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projection(source) => write!(formatter, "Placeholder 投影失败：{source}"),
            Self::Multiset(source) => write!(formatter, "Placeholder token 不一致：{source}"),
        }
    }
}

impl Error for PlaceholderRestoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source),
            Self::Multiset(source) => Some(source),
        }
    }
}

impl From<LanguageTextProjectionError> for PlaceholderRestoreError {
    fn from(source: LanguageTextProjectionError) -> Self {
        Self::Projection(source)
    }
}

impl From<PlaceholderMultisetError> for PlaceholderRestoreError {
    fn from(source: PlaceholderMultisetError) -> Self {
        Self::Multiset(source)
    }
}

#[derive(Debug)]
pub(crate) struct Pcre2PlaceholderConstructionError(pcre2::Error);

impl Pcre2PlaceholderConstructionError {
    pub(crate) fn safe_diagnostic(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            stage,
            DiagnosticSubject::operation("builtin_placeholder_compile"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                format!(
                    "engine=pcre2; kind={}; code={}; offset={}",
                    pcre2_error_kind(&self.0),
                    self.0.code(),
                    optional_offset(self.0.offset())
                ),
            ),
            impact,
            DiagnosticAction::ReportBug,
        )
    }
}

impl fmt::Display for Pcre2PlaceholderConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "无法编译内置 Placeholder 规格：{}", self.0)
    }
}

impl Error for Pcre2PlaceholderConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

impl SafeDiagnosticSource for Pcre2PlaceholderConstructionError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        self.safe_diagnostic(stage, impact)
    }
}

fn pcre2_error_kind(source: &pcre2::Error) -> &'static str {
    match source.kind() {
        pcre2::ErrorKind::Compile => "compile",
        pcre2::ErrorKind::JIT => "jit",
        pcre2::ErrorKind::Match => "match",
        pcre2::ErrorKind::Info => "info",
        pcre2::ErrorKind::Option => "option",
        _ => "unknown",
    }
}

fn optional_offset(offset: Option<usize>) -> String {
    offset.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

#[derive(Debug)]
pub(crate) enum PlaceholderRuleCompilationError {
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
    EmptyScopes {
        rule_number: usize,
    },
    UnknownScope {
        rule_number: usize,
        scope: String,
    },
    DuplicateScope {
        rule_number: usize,
        scope: String,
    },
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        source: pcre2::Error,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        captures: Vec<String>,
    },
}

impl fmt::Display for PlaceholderRuleCompilationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartWorker { operation, source } => {
                write!(
                    formatter,
                    "无法启动自定义 Placeholder 编译 worker {operation}：{source}"
                )
            }
            Self::EmptyScopes { rule_number } => {
                write!(formatter, "占位符规则 {rule_number} 的 scopes 为空")
            }
            Self::UnknownScope { rule_number, scope } => {
                write!(
                    formatter,
                    "占位符规则 {rule_number} 使用未知作用域 {scope:?}"
                )
            }
            Self::DuplicateScope { rule_number, scope } => {
                write!(formatter, "占位符规则 {rule_number} 重复作用域 {scope:?}")
            }
            Self::EmptyPattern { rule_number } => {
                write!(formatter, "占位符规则 {rule_number} 的 pattern 不能为空")
            }
            Self::InvalidPattern {
                rule_number,
                source,
            } => write!(
                formatter,
                "占位符规则 {rule_number} 的 PCRE2 pattern 无效：{source}"
            ),
            Self::InvalidNamedCaptures {
                rule_number,
                captures,
            } => write!(
                formatter,
                "占位符规则 {rule_number} 只允许唯一的 text 命名捕获组，实际为 {captures:?}"
            ),
        }
    }
}

impl Error for PlaceholderRuleCompilationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StartWorker { source, .. } => Some(source),
            Self::InvalidPattern { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PlaceholderProtectionError {
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
    Match(pcre2::Error),
    EmptyMatch {
        label: String,
    },
    MissingTextCapture {
        rule_number: usize,
    },
    InvalidMatchRange {
        rule_number: usize,
    },
    OverlappingMatches {
        first: String,
        second: String,
    },
    CrossesLineBoundary {
        rule_number: Option<usize>,
        source_line_index: usize,
    },
    ReservedTokenNamespace,
}

impl fmt::Display for PlaceholderProtectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartWorker { operation, source } => {
                write!(
                    formatter,
                    "无法启动 Placeholder 匹配 worker {operation}：{source}"
                )
            }
            Self::Match(source) => write!(formatter, "PCRE2 匹配失败：{source}"),
            Self::EmptyMatch { label } => write!(formatter, "占位符规则 {label} 产生空匹配"),
            Self::MissingTextCapture { rule_number } => {
                write!(
                    formatter,
                    "占位符规则 {rule_number} 的 text 命名组未参与匹配"
                )
            }
            Self::InvalidMatchRange { rule_number } => write!(
                formatter,
                "占位符规则 {rule_number} 的完整匹配与 text 捕获必须位于原文 UTF-8 字符边界内，且 text 捕获必须包含在完整匹配中"
            ),
            Self::OverlappingMatches { first, second } => {
                write!(formatter, "占位符匹配区间重叠：{first} 与 {second}")
            }
            Self::CrossesLineBoundary {
                rule_number,
                source_line_index,
            } => match rule_number {
                Some(rule_number) => write!(
                    formatter,
                    "占位符规则 {rule_number} 的不透明保护跨度跨越第 {} 个文本单元边界",
                    source_line_index + 1
                ),
                None => write!(
                    formatter,
                    "内置占位符的不透明保护跨度跨越第 {} 个文本单元边界",
                    source_line_index + 1
                ),
            },
            Self::ReservedTokenNamespace => write!(
                formatter,
                "原文包含保留的 ATT token 前缀 {:?}",
                placeholder_token::PREFIX
            ),
        }
    }
}

impl Error for PlaceholderProtectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StartWorker { source, .. } => Some(source),
            Self::Match(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_rules() -> CompiledPlaceholderRules {
        CompiledPlaceholderRules {
            rules: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn cancellable_protection_preserves_existing_result_and_hash_protocol() {
        let service = PlaceholderService;
        let custom = service
            .compile_custom(
                vec![PlaceholderRuleDefinition::new(None, r"\{[^}]+\}")],
                |_| true,
            )
            .expect("规则应可编译");
        let expected = service
            .protect("dialogue", "你好 {name}", &[], &custom, None)
            .expect("普通保护应成功");
        let mut polls = 0_usize;
        let actual = service
            .protect_with_cancellation("dialogue", "你好 {name}", &[], &custom, None, || {
                polls += 1;
                Ok::<_, Infallible>(())
            })
            .expect("检查不会取消")
            .expect("可取消保护应成功");

        assert_eq!(actual, expected);
        assert!(polls > 1);

        let mut one_shot = Sha256FramedHasher::new(b"att.placeholder-bindings");
        for binding in actual.placeholders() {
            one_shot
                .frame(1, binding.token().as_bytes())
                .frame(2, binding.original().as_bytes())
                .frame(3, binding.label().as_bytes())
                .frame(4, binding.scope().as_bytes())
                .frame(5, binding.segment().name().as_bytes());
        }
        assert_eq!(actual.binding_fingerprint(), one_shot.finish());
    }

    #[test]
    fn matching_isolation_keeps_empty_and_short_builtin_paths_inline() {
        assert!(!should_isolate_placeholder_matching(0, false, false));
        assert!(!should_isolate_placeholder_matching(
            PLACEHOLDER_CANCELLATION_CHECK_BYTES,
            true,
            false
        ));
        assert!(should_isolate_placeholder_matching(1, false, true));
        assert!(should_isolate_placeholder_matching(
            PLACEHOLDER_CANCELLATION_CHECK_BYTES + 1,
            true,
            false
        ));
    }

    #[test]
    fn custom_match_batch_propagates_cancellation_at_the_isolated_worker_boundary() {
        let custom = PlaceholderService
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "a")], |_| true)
            .expect("规则应可编译");
        let mut polls = 0_usize;

        let result = PlaceholderService.protect_with_cancellation(
            "dialogue",
            "a",
            &[],
            &custom,
            None,
            || {
                polls += 1;
                // 前九次检查覆盖入口、保留前缀、scope、输入复制和 worker 启动前检查。
                // 第十次发生在 worker 已启动或已经返回的边界，隔离执行器在此原样交还取消。
                if polls == 10 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        );

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 10);
    }

    #[test]
    fn match_worker_start_failure_keeps_operation_and_os_error() {
        let error = PlaceholderProtectionError::StartWorker {
            operation: "att-placeholder-match",
            source: io::Error::from_raw_os_error(8),
        };

        assert!(error.to_string().contains("att-placeholder-match"));
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .and_then(io::Error::raw_os_error),
            Some(8)
        );
    }

    #[test]
    fn long_unmatched_text_can_cancel_between_output_copies() {
        let original = "x".repeat(PLACEHOLDER_CANCELLATION_CHECK_BYTES * 4);
        let mut polls = 0_usize;
        let result = PlaceholderService.protect_with_cancellation(
            "dialogue",
            &original,
            &[],
            &empty_rules(),
            None,
            || {
                polls += 1;
                if polls == 9 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 9);
    }

    #[test]
    fn long_matched_text_can_cancel_while_cloning_the_binding() {
        let service = PlaceholderService;
        let custom = service
            .compile_custom(
                vec![PlaceholderRuleDefinition::new(None, r"(?s).+")],
                |_| true,
            )
            .expect("规则应可编译");
        let original = "x".repeat(PLACEHOLDER_CANCELLATION_CHECK_BYTES * 4);
        let mut polls = 0_usize;
        let result =
            service.protect_with_cancellation("dialogue", &original, &[], &custom, None, || {
                polls += 1;
                if polls == 23 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 23);
    }

    #[test]
    fn binding_hash_can_cancel_inside_each_unbounded_field() {
        fn assert_field_cancels(
            token: String,
            original: String,
            label: String,
            scope: String,
            cancel_at: usize,
        ) {
            let bindings = [AppliedPlaceholder::new(
                token,
                original,
                PlaceholderRuleOrigin::Custom,
                label,
                scope,
                PlaceholderSegment::Whole,
            )];
            let mut polls = 0_usize;
            let result = placeholder_binding_fingerprint_with_cancellation(&bindings, || {
                polls += 1;
                if polls == cancel_at {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });

            assert!(matches!(result, Err("cancelled")));
            assert_eq!(polls, cancel_at);
        }

        let long = || "x".repeat(PLACEHOLDER_CANCELLATION_CHECK_BYTES * 4);
        assert_field_cancels(long(), String::new(), String::new(), String::new(), 4);
        assert_field_cancels(String::new(), long(), String::new(), String::new(), 4);
        assert_field_cancels(String::new(), String::new(), long(), String::new(), 4);
        assert_field_cancels(String::new(), String::new(), String::new(), long(), 4);
    }

    #[test]
    fn cancellable_reserved_prefix_scan_finds_a_chunk_boundary_match() {
        let prefix_start = PLACEHOLDER_CANCELLATION_CHECK_BYTES - 2;
        let mut original = "x".repeat(prefix_start);
        original.push_str(placeholder_token::PREFIX);
        original.push_str("tail");

        assert!(
            contains_reserved_prefix_with_cancellation(&original, &mut || {
                Ok::<_, Infallible>(())
            })
            .expect("检查不会取消")
        );
    }

    #[test]
    fn cancellable_span_sort_is_stable_and_can_stop_during_merge() {
        fn span(start: usize, diagnostic_label: String) -> SelectedSpan<'static> {
            SelectedSpan {
                start,
                end: start + 1,
                origin: PlaceholderRuleOrigin::Custom,
                semantic_label: CUSTOM_SEMANTIC_LABEL,
                diagnostic_label,
                rule_number: Some(1),
                scope: "dialogue",
                segment: PlaceholderSegment::Whole,
            }
        }

        let mut equal = vec![span(1, "first".to_owned()), span(1, "second".to_owned())];
        stable_sort_selected_spans_with_cancellation(&mut equal, &mut || Ok::<_, Infallible>(()))
            .expect("检查不会取消");
        assert_eq!(equal[0].diagnostic_label, "first");
        assert_eq!(equal[1].diagnostic_label, "second");

        let length = 4_096_usize;
        let mut descending = (0..length)
            .rev()
            .map(|start| span(start, String::new()))
            .collect::<Vec<_>>();
        let mut polls = 0_usize;
        let result = stable_sort_selected_spans_with_cancellation(&mut descending, &mut || {
            polls += 1;
            if polls == length + 10 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, length + 10);
    }
}
