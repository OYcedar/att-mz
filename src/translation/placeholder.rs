//! 各翻译引擎共享的 PCRE2、token、绑定、语言投影与恢复机制。
//!
//! 本模块不解释游戏引擎、kind 枚举或内置控制符。调用方负责校验 scope，并把需要的
//! 内置 pattern 显式交给保护操作。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::LanguageText;

use super::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderBindingIndex, PlaceholderMultisetError,
};
use super::placeholder_token;

const CUSTOM_SEMANTIC_LABEL: &str = "CUSTOM";

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

/// 由引擎适配器提供的一条已编译内置规则。
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
    pub(crate) fn compile_custom(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
        mut valid_scope: impl FnMut(&str) -> bool,
    ) -> Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError> {
        let mut rules = Vec::with_capacity(definitions.len());
        for (index, definition) in definitions.into_iter().enumerate() {
            let rule_number = index + 1;
            let scopes = definition
                .scopes
                .map(|scopes| {
                    if scopes.is_empty() {
                        return Err(PlaceholderRuleCompilationError::EmptyScopes { rule_number });
                    }
                    let mut unique = HashSet::with_capacity(scopes.len());
                    for scope in &scopes {
                        if !valid_scope(scope) {
                            return Err(PlaceholderRuleCompilationError::UnknownScope {
                                rule_number,
                                scope: scope.clone(),
                            });
                        }
                        if !unique.insert(scope.clone()) {
                            return Err(PlaceholderRuleCompilationError::DuplicateScope {
                                rule_number,
                                scope: scope.clone(),
                            });
                        }
                    }
                    Ok(scopes)
                })
                .transpose()?;
            if definition.pattern.is_empty() {
                return Err(PlaceholderRuleCompilationError::EmptyPattern { rule_number });
            }
            let regex = compile_regex(&definition.pattern).map_err(|source| {
                PlaceholderRuleCompilationError::InvalidPattern {
                    rule_number,
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
                        rule_number,
                        captures: named_captures,
                    });
                }
            };
            rules.push(CompiledPlaceholderRule {
                scopes,
                regex,
                rule_number,
                has_text_capture,
            });
        }
        Ok(CompiledPlaceholderRules {
            rules: Arc::new(rules),
        })
    }

    /// 保护原文；scope 与 builtin 均由调用方提前确定。
    pub(crate) fn protect(
        &self,
        scope: &str,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        builtin: Option<&CompiledBuiltinPlaceholderRule>,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        if placeholder_token::contains_reserved_prefix(original) {
            return Err(PlaceholderProtectionError::ReservedTokenNamespace);
        }

        let mut matches = Vec::new();
        if let Some(builtin) = builtin {
            for matched in builtin.regex.find_iter(original.as_bytes()) {
                let matched = matched.map_err(PlaceholderProtectionError::Match)?;
                if matched.start() == matched.end() {
                    return Err(PlaceholderProtectionError::EmptyMatch {
                        label: builtin.semantic_label.to_owned(),
                    });
                }
                matches.push(ProtectionMatch {
                    protected: vec![SelectedSpan {
                        start: matched.start(),
                        end: matched.end(),
                        origin: PlaceholderRuleOrigin::BuiltIn,
                        semantic_label: builtin.semantic_label,
                        diagnostic_label: builtin.semantic_label.to_owned(),
                        rule_number: None,
                        scope,
                        segment: PlaceholderSegment::Whole,
                    }],
                });
            }
        }
        for rule in custom.rules.iter() {
            if rule
                .scopes
                .as_ref()
                .is_some_and(|scopes| !scopes.iter().any(|candidate| candidate == scope))
            {
                continue;
            }
            matches.extend(custom_matches(rule, original, scope)?);
        }

        let mut selected = matches
            .into_iter()
            .flat_map(|matched| matched.protected)
            .collect::<Vec<_>>();
        selected.sort_by_key(|span| (span.start, span.end));
        let mut max_end_span = None;
        for (index, current) in selected.iter().enumerate() {
            if let Some(previous_index) = max_end_span {
                let previous: &SelectedSpan<'_> = &selected[previous_index];
                if current.start < previous.end {
                    return Err(PlaceholderProtectionError::OverlappingMatches {
                        first: previous.diagnostic_label.clone(),
                        second: current.diagnostic_label.clone(),
                    });
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
            while line_separator_offsets
                .get(source_line_index)
                .is_some_and(|separator| *separator < span.start)
            {
                source_line_index += 1;
            }
            if line_separator_offsets
                .get(source_line_index)
                .is_some_and(|separator| *separator < span.end)
            {
                return Err(PlaceholderProtectionError::CrossesLineBoundary {
                    rule_number: span.rule_number,
                    source_line_index,
                });
            }
        }

        let mut protected = String::with_capacity(original.len());
        let mut placeholders = Vec::with_capacity(selected.len());
        let mut cursor = 0;
        for (index, span) in selected.into_iter().enumerate() {
            protected.push_str(&original[cursor..span.start]);
            let token = semantic_token(span.semantic_label, span.segment, index);
            protected.push_str(&token);
            placeholders.push(AppliedPlaceholder::new(
                token,
                &original[span.start..span.end],
                span.origin,
                span.semantic_label,
                span.scope,
                span.segment,
            ));
            cursor = span.end;
        }
        protected.push_str(&original[cursor..]);
        Ok(ProtectedText::new(protected, placeholders))
    }
}

fn compile_regex(pattern: &str) -> Result<Regex, pcre2::Error> {
    RegexBuilder::new()
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(pattern)
}

fn custom_matches<'a>(
    rule: &CompiledPlaceholderRule,
    original: &str,
    scope: &'a str,
) -> Result<Vec<ProtectionMatch<'a>>, PlaceholderProtectionError> {
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
            let capture =
                captures
                    .name("text")
                    .ok_or(PlaceholderProtectionError::MissingTextCapture {
                        rule_number: rule.rule_number,
                    })?;
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
                protected.push(SelectedSpan {
                    start: whole.start(),
                    end: capture.start(),
                    origin: PlaceholderRuleOrigin::Custom,
                    semantic_label: CUSTOM_SEMANTIC_LABEL,
                    diagnostic_label: diagnostic_label.clone(),
                    rule_number: Some(rule.rule_number),
                    scope,
                    segment: PlaceholderSegment::Begin,
                });
            }
            if capture.end() < whole.end() {
                protected.push(SelectedSpan {
                    start: capture.end(),
                    end: whole.end(),
                    origin: PlaceholderRuleOrigin::Custom,
                    semantic_label: CUSTOM_SEMANTIC_LABEL,
                    diagnostic_label: diagnostic_label.clone(),
                    rule_number: Some(rule.rule_number),
                    scope,
                    segment: PlaceholderSegment::End,
                });
            }
            protected
        } else {
            vec![SelectedSpan {
                start: whole.start(),
                end: whole.end(),
                origin: PlaceholderRuleOrigin::Custom,
                semantic_label: CUSTOM_SEMANTIC_LABEL,
                diagnostic_label: diagnostic_label.clone(),
                rule_number: Some(rule.rule_number),
                scope,
                segment: PlaceholderSegment::Whole,
            }]
        };
        result.push(ProtectionMatch { protected });
    }
    Ok(result)
}

fn valid_utf8_range(text: &str, start: usize, end: usize) -> bool {
    start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

fn custom_diagnostic_label(rule_number: usize) -> String {
    format!("CUSTOM_{rule_number:04}")
}

struct ProtectionMatch<'a> {
    protected: Vec<SelectedSpan<'a>>,
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
    placeholders: Vec<AppliedPlaceholder>,
    binding_fingerprint: Sha256Fingerprint,
}

impl ProtectedText {
    fn new(text: String, placeholders: Vec<AppliedPlaceholder>) -> Self {
        let binding_fingerprint = placeholder_binding_fingerprint(&placeholders);
        Self {
            text,
            placeholders,
            binding_fingerprint,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn placeholders(&self) -> &[AppliedPlaceholder] {
        &self.placeholders
    }

    pub(crate) const fn binding_fingerprint(&self) -> Sha256Fingerprint {
        self.binding_fingerprint
    }

    pub(crate) fn language_text(&self) -> Result<LanguageText, LanguageTextProjectionError> {
        super::placeholder_projection::project_protected_text(&self.text, &self.placeholders)
    }

    /// 验证 token 多重集后，按候选实际 token 顺序直接交错恢复原片段。
    pub(crate) fn restore(&self, candidate: &str) -> Result<String, PlaceholderRestoreError> {
        let bindings = PlaceholderBindingIndex::new(&self.placeholders)?;
        let scanned = bindings.scan(candidate);
        bindings.validate_multiset(
            std::slice::from_ref(&scanned),
            bindings.all_binding_indices(),
        )?;
        let projected = bindings.project(candidate, &scanned, bindings.all_binding_indices())?;
        Ok(bindings.rebuild_original(&projected, projected.language_text())?)
    }

    pub(crate) fn into_parts(self) -> (String, Vec<AppliedPlaceholder>) {
        (self.text, self.placeholders)
    }
}

fn placeholder_binding_fingerprint(bindings: &[AppliedPlaceholder]) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.placeholder-bindings");
    for binding in bindings {
        hasher
            .frame(1, binding.token().as_bytes())
            .frame(2, binding.original().as_bytes())
            .frame(3, binding.label().as_bytes())
            .frame(4, binding.scope().as_bytes())
            .frame(5, binding.segment().name().as_bytes());
    }
    hasher.finish()
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
            Self::InvalidPattern { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PlaceholderProtectionError {
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
            Self::Match(source) => Some(source),
            _ => None,
        }
    }
}
