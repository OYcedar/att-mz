//! RPG Maker 内置控制符与用户 PCRE2 规则的语义化保护。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::placeholder_token;
use crate::rpg_maker::text::TextGroupKind;

use super::standard::{AppliedPlaceholder, PlaceholderRuleOrigin, PlaceholderSegment};

const MV_BUILTIN_CONTROL_PATTERN: &str = r"\\(?:[VvNnPpCcIi]\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const MZ_BUILTIN_CONTROL_PATTERN: &str =
    r"\\(?:(?:[VvNnPpCcIi]|[Pp][Xx]|[Pp][Yy]|[Ff][Ss])\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const BUILTIN_SEMANTIC_LABEL: &str = "RPG_MAKER_CONTROL";
const CUSTOM_SEMANTIC_LABEL: &str = "CUSTOM";

/// 外部 TOML 中一条自定义占位符规则的最小表达。
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

/// 已编译且可以在线程间共享的自定义规则集合。
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

impl CompiledPlaceholderRules {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }
}

#[derive(Clone)]
struct CompiledPlaceholderRule {
    scopes: Option<Vec<PlaceholderScope>>,
    regex: Regex,
    rule_number: usize,
    has_text_capture: bool,
}

/// PCRE2 只存在于这一个外部规则边界，内部仍使用 Rust 结构化绑定。
#[derive(Clone)]
pub(crate) struct Pcre2PlaceholderService {
    mv_builtin: Regex,
    mz_builtin: Regex,
}

impl Pcre2PlaceholderService {
    pub(crate) fn new() -> Result<Self, Pcre2PlaceholderConstructionError> {
        Ok(Self {
            mv_builtin: compile_regex(MV_BUILTIN_CONTROL_PATTERN)
                .map_err(Pcre2PlaceholderConstructionError)?,
            mz_builtin: compile_regex(MZ_BUILTIN_CONTROL_PATTERN)
                .map_err(Pcre2PlaceholderConstructionError)?,
        })
    }

    pub(crate) fn compile_custom(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
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
                    let scopes = scopes
                        .iter()
                        .map(|scope| PlaceholderScope::parse(scope, rule_number))
                        .collect::<Result<Vec<_>, _>>()?;
                    ensure_unique_scopes(&scopes, rule_number)?;
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

    /// 验证所有规则区间互不重叠后，生成可逆 Rust 绑定。
    #[cfg(test)]
    pub(crate) fn protect(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        self.protect_with_line_boundaries(engine, kind, original, &[], custom)
    }

    /// 保护原文，同时保证 `Lines` 拼接产生的槽分隔 LF 不进入任何不透明跨度。
    pub(crate) fn protect_with_line_boundaries(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        if placeholder_token::contains_reserved_prefix(original) {
            return Err(PlaceholderProtectionError::ReservedTokenNamespace);
        }

        let scope = PlaceholderScope::from_kind(kind);
        let mut matches = self.builtin_matches(engine, original, scope)?;
        for rule in custom.rules.iter() {
            if rule
                .scopes
                .as_ref()
                .is_some_and(|scopes| !scopes.contains(&scope))
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
                let previous: &SelectedSpan = &selected[previous_index];
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
                span.scope.name(),
                span.segment,
            ));
            cursor = span.end;
        }
        protected.push_str(&original[cursor..]);

        Ok(ProtectedText {
            text: protected,
            placeholders,
        })
    }

    fn builtin_matches(
        &self,
        engine: RpgMakerEngine,
        original: &str,
        scope: PlaceholderScope,
    ) -> Result<Vec<ProtectionMatch>, PlaceholderProtectionError> {
        let mut result = Vec::new();
        let regex = match engine {
            RpgMakerEngine::Mv => &self.mv_builtin,
            RpgMakerEngine::Mz => &self.mz_builtin,
        };
        for matched in regex.find_iter(original.as_bytes()) {
            let matched = matched.map_err(PlaceholderProtectionError::Match)?;
            if matched.start() == matched.end() {
                return Err(PlaceholderProtectionError::EmptyMatch {
                    label: BUILTIN_SEMANTIC_LABEL.to_owned(),
                });
            }
            result.push(ProtectionMatch {
                protected: vec![SelectedSpan {
                    start: matched.start(),
                    end: matched.end(),
                    origin: PlaceholderRuleOrigin::BuiltIn,
                    semantic_label: BUILTIN_SEMANTIC_LABEL,
                    diagnostic_label: BUILTIN_SEMANTIC_LABEL.to_owned(),
                    rule_number: None,
                    scope,
                    segment: PlaceholderSegment::Whole,
                }],
            });
        }
        Ok(result)
    }
}

fn compile_regex(pattern: &str) -> Result<Regex, pcre2::Error> {
    RegexBuilder::new()
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(pattern)
}

fn custom_matches(
    rule: &CompiledPlaceholderRule,
    original: &str,
    scope: PlaceholderScope,
) -> Result<Vec<ProtectionMatch>, PlaceholderProtectionError> {
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PlaceholderScope(TextGroupKind);

impl PlaceholderScope {
    fn parse(value: &str, rule_number: usize) -> Result<Self, PlaceholderRuleCompilationError> {
        TextGroupKind::from_storage_name(value)
            .map(Self)
            .ok_or_else(|| PlaceholderRuleCompilationError::UnknownScope {
                rule_number,
                scope: value.to_owned(),
            })
    }

    const fn from_kind(kind: TextGroupKind) -> Self {
        Self(kind)
    }

    const fn name(self) -> &'static str {
        self.0.storage_name()
    }
}

fn ensure_unique_scopes(
    scopes: &[PlaceholderScope],
    rule_number: usize,
) -> Result<(), PlaceholderRuleCompilationError> {
    for (index, scope) in scopes.iter().enumerate() {
        if scopes[..index].contains(scope) {
            return Err(PlaceholderRuleCompilationError::DuplicateScope {
                rule_number,
                scope: scope.name().to_owned(),
            });
        }
    }
    Ok(())
}

struct ProtectionMatch {
    protected: Vec<SelectedSpan>,
}

struct SelectedSpan {
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    semantic_label: &'static str,
    diagnostic_label: String,
    rule_number: Option<usize>,
    scope: PlaceholderScope,
    segment: PlaceholderSegment,
}

fn semantic_token(label: &str, segment: PlaceholderSegment, index: usize) -> String {
    let segment = match segment {
        PlaceholderSegment::Whole => "WHOLE",
        PlaceholderSegment::Begin => "BEGIN",
        PlaceholderSegment::End => "END",
    };
    placeholder_token::envelope(&format!("{label}_{segment}_{index:04}"))
}

/// 一次保护的完整可逆结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtectedText {
    text: String,
    placeholders: Vec<AppliedPlaceholder>,
}

impl ProtectedText {
    pub(crate) fn into_parts(self) -> (String, Vec<AppliedPlaceholder>) {
        (self.text, self.placeholders)
    }
}

#[derive(Debug)]
pub(crate) struct Pcre2PlaceholderConstructionError(pcre2::Error);

impl Pcre2PlaceholderConstructionError {
    /// 只公开 PCRE2 的稳定分类、数值代码和偏移；内置 pattern 与底层错误文本不进入投影。
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

    #[cfg(test)]
    pub(crate) fn for_test(pattern: &str) -> Self {
        match compile_regex(pattern) {
            Ok(_) => panic!("测试 pattern 必须触发 PCRE2 编译失败"),
            Err(source) => Self(source),
        }
    }
}

impl fmt::Display for Pcre2PlaceholderConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "无法编译内置 RPG Maker 控制符规格：{}", self.0)
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
            Self::UnknownScope { rule_number, scope } => write!(
                formatter,
                "占位符规则 {rule_number} 使用未知作用域 {scope:?}"
            ),
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
            Self::MissingTextCapture { rule_number } => write!(
                formatter,
                "占位符规则 {rule_number} 的 text 命名组未参与匹配"
            ),
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
                    "占位符规则 {rule_number} 的不透明保护跨度跨越第 {} 个 Lines 元素之后的槽边界",
                    source_line_index + 1
                ),
                None => write!(
                    formatter,
                    "内置占位符的不透明保护跨度跨越第 {} 个 Lines 元素之后的槽边界",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_and_custom_overlap_is_rejected() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["event_dialogue".to_owned()]),
                r"\\C\[[^]]+\]勇者",
            )])
            .expect("自定义规则应该有效");

        assert!(matches!(
            service.protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                r"\C[2]勇者",
                &custom,
            ),
            Err(PlaceholderProtectionError::OverlappingMatches { .. })
        ));
    }

    #[test]
    fn builtin_controls_cover_literal_backslashes_and_adjacent_boundaries() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let cases: &[(&str, &[&str])] = &[
            (r"\\", &[r"\\"]),
            (r"\\\\", &[r"\\", r"\\"]),
            (r"\\C[2]", &[r"\\"]),
            (r"\\\C[2]", &[r"\\", r"\C[2]"]),
        ];

        for &(original, expected) in cases {
            let (_, bindings) = service
                .protect(
                    RpgMakerEngine::Mz,
                    TextGroupKind::EventDialogue,
                    original,
                    &CompiledPlaceholderRules::empty(),
                )
                .expect("内置控制符应该受到保护")
                .into_parts();
            let actual = bindings
                .iter()
                .map(AppliedPlaceholder::original)
                .collect::<Vec<_>>();

            assert_eq!(actual, expected, "边界样本 {original:?}");
            assert!(bindings.iter().all(|binding| {
                binding.origin() == PlaceholderRuleOrigin::BuiltIn
                    && binding.segment() == PlaceholderSegment::Whole
            }));
        }
    }

    #[test]
    fn builtin_parameter_and_single_character_controls_remain_protected() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        for control in [
            r"\V[1]", r"\N[2]", r"\P[3]", r"\C[4]", r"\I[5]", r"\PX[6]", r"\PY[7]", r"\FS[8]",
            r"\G", r"\{", r"\}", r"\$", r"\.", r"\|", r"\!", r"\>", r"\<", r"\^",
        ] {
            let (_, bindings) = service
                .protect(
                    RpgMakerEngine::Mz,
                    TextGroupKind::EventDialogue,
                    control,
                    &CompiledPlaceholderRules::empty(),
                )
                .expect("既有内置控制符应该受到保护")
                .into_parts();

            assert_eq!(bindings.len(), 1, "控制符 {control:?}");
            assert_eq!(bindings[0].original(), control);
        }
    }

    #[test]
    fn builtin_controls_are_engine_specific_and_strictly_ascii() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let empty = CompiledPlaceholderRules::empty();

        for control in [r"\PX[6]", r"\pY[7]", r"\Fs[8]"] {
            let (_, mv) = service
                .protect(
                    RpgMakerEngine::Mv,
                    TextGroupKind::EventDialogue,
                    control,
                    &empty,
                )
                .expect("MV 不支持的控制符应保留为自然文本")
                .into_parts();
            let (_, mz) = service
                .protect(
                    RpgMakerEngine::Mz,
                    TextGroupKind::EventDialogue,
                    control,
                    &empty,
                )
                .expect("MZ 控制符应可保护")
                .into_parts();
            assert!(mv.is_empty(), "MV 不应内建保护 {control:?}");
            assert_eq!(mz.len(), 1, "MZ 应内建保护 {control:?}");
        }

        for invalid in [r"\V[１]", r"\Fſ[8]"] {
            let (text, bindings) = service
                .protect(
                    RpgMakerEngine::Mz,
                    TextGroupKind::EventDialogue,
                    invalid,
                    &empty,
                )
                .expect("非 ASCII 形式只是不命中")
                .into_parts();
            assert_eq!(text, invalid);
            assert!(bindings.is_empty());
        }
    }

    #[test]
    fn original_text_cannot_enter_the_reserved_token_namespace() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");

        assert!(matches!(
            service.protect(
                RpgMakerEngine::Mz,
                TextGroupKind::DatabaseEntry,
                "自然文本⟦ATT_FAKE",
                &CompiledPlaceholderRules::empty(),
            ),
            Err(PlaceholderProtectionError::ReservedTokenNamespace)
        ));
    }

    #[test]
    fn structured_rule_protects_shell_and_keeps_named_capture_translatable() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["plugin_parameter".to_owned()]),
                r"<name>(?<text>.*?)</name>",
            )])
            .expect("结构化规则应该有效");

        let (text, bindings) = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::PluginParameter,
                "<name>魔法剣</name>",
                &custom,
            )
            .expect("结构化保护应该成功")
            .into_parts();

        assert!(text.contains("魔法剣"));
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].segment(), PlaceholderSegment::Begin);
        assert_eq!(bindings[0].original(), "<name>");
        assert_eq!(bindings[1].segment(), PlaceholderSegment::End);
        assert_eq!(bindings[1].original(), "</name>");
    }

    #[test]
    fn structured_wrapper_allows_builtin_controls_inside_its_natural_text_capture() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                None,
                r"<name>(?<text>.*?)</name>",
            )])
            .expect("结构化规则应该有效");

        let (text, bindings) = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::PluginParameter,
                r"<name>\C[2]勇者</name>",
                &custom,
            )
            .expect("只有实际保护跨度互不重叠时应允许组合规则")
            .into_parts();

        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].original(), "<name>");
        assert_eq!(bindings[1].original(), r"\C[2]");
        assert_eq!(bindings[2].original(), "</name>");
        assert!(text.contains("勇者"));
    }

    #[test]
    fn unmatched_rule_insertion_does_not_change_selected_tokens_or_bindings() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let original = "前<TOKEN>後";
        let first = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "<TOKEN>")])
            .expect("命中规则应该有效");
        let shifted = service
            .compile_custom(vec![
                PlaceholderRuleDefinition::new(None, "DOES_NOT_MATCH"),
                PlaceholderRuleDefinition::new(None, "<TOKEN>"),
            ])
            .expect("插入的不命中规则应该有效");

        let first = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::DatabaseEntry,
                original,
                &first,
            )
            .expect("首份规则应该保护成功");
        let shifted = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::DatabaseEntry,
                original,
                &shifted,
            )
            .expect("规则编号变化不应改变有效保护结果");

        assert_eq!(first, shifted);
        let (text, bindings) = shifted.into_parts();
        assert_eq!(text, "前⟦ATT_CUSTOM_WHOLE_0000⟧後");
        assert_eq!(bindings[0].label(), CUSTOM_SEMANTIC_LABEL);
    }

    #[test]
    fn omitted_scope_is_global_but_explicit_scope_is_local() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let global = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "TOKEN")])
            .expect("缺省 scopes 应表示全局");
        let (_, bindings) = service
            .protect(RpgMakerEngine::Mz, TextGroupKind::System, "TOKEN", &global)
            .expect("全局规则应适用于 System")
            .into_parts();
        assert_eq!(bindings.len(), 1);

        let local = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["event_dialogue".to_owned()]),
                "TOKEN",
            )])
            .expect("显式作用域应该有效");
        let (text, bindings) = service
            .protect(RpgMakerEngine::Mz, TextGroupKind::System, "TOKEN", &local)
            .expect("未命中作用域不是错误")
            .into_parts();
        assert_eq!(text, "TOKEN");
        assert!(bindings.is_empty());
    }

    #[test]
    fn all_scope_and_empty_explicit_scopes_are_invalid() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        assert!(matches!(
            service.compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["all".to_owned()]),
                "x",
            )]),
            Err(PlaceholderRuleCompilationError::UnknownScope { .. })
        ));
        assert!(matches!(
            service.compile_custom(vec![PlaceholderRuleDefinition::new(Some(Vec::new()), "x")]),
            Err(PlaceholderRuleCompilationError::EmptyScopes { .. })
        ));
    }

    #[test]
    fn custom_overlap_fails_while_zero_hits_are_legal() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let overlapping = service
            .compile_custom(vec![
                PlaceholderRuleDefinition::new(None, "abc"),
                PlaceholderRuleDefinition::new(None, "bc"),
            ])
            .expect("两条规则均可编译");
        assert!(matches!(
            service.protect(RpgMakerEngine::Mz, TextGroupKind::Map, "abc", &overlapping),
            Err(PlaceholderProtectionError::OverlappingMatches { .. })
        ));

        let no_hit = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "missing")])
            .expect("规则应可编译");
        let (text, bindings) = service
            .protect(RpgMakerEngine::Mz, TextGroupKind::Map, "source", &no_hit)
            .expect("零命中合法")
            .into_parts();
        assert_eq!(text, "source");
        assert!(bindings.is_empty());
    }

    #[test]
    fn zero_width_match_fails_when_encountered() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, r"\A")])
            .expect("零宽正则可编译");
        assert!(matches!(
            service.protect(RpgMakerEngine::Mz, TextGroupKind::Map, "source", &custom),
            Err(PlaceholderProtectionError::EmptyMatch { .. })
        ));
    }

    #[test]
    fn custom_match_ranges_must_align_with_utf8_boundaries() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        for pattern in [r"\C", r"(?<text>\C)"] {
            let custom = service
                .compile_custom(vec![PlaceholderRuleDefinition::new(None, pattern)])
                .expect("PCRE2 允许按单个字节匹配");

            assert!(matches!(
                service.protect(RpgMakerEngine::Mz, TextGroupKind::Map, "莉", &custom),
                Err(PlaceholderProtectionError::InvalidMatchRange { rule_number: 1 })
            ));
        }
    }

    #[test]
    fn custom_text_capture_must_be_contained_in_the_complete_match() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                None,
                r"A(?=(?<text>B))",
            )])
            .expect("PCRE2 允许 lookahead 捕获超出完整匹配");

        assert!(matches!(
            service.protect(RpgMakerEngine::Mz, TextGroupKind::Map, "AB", &custom),
            Err(PlaceholderProtectionError::InvalidMatchRange { rule_number: 1 })
        ));
    }

    #[test]
    fn invalid_capture_and_unknown_fields_are_rejected() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let error = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "(?<other>.+)")])
            .expect_err("除 text 外的命名捕获组应该失败");
        assert!(matches!(
            error,
            PlaceholderRuleCompilationError::InvalidNamedCaptures { .. }
        ));

        assert!(
            toml::from_str::<PlaceholderRuleDefinition>("pattern = 'x'\nextra = true").is_err()
        );
    }

    #[test]
    fn unmatched_backslash_sequences_remain_natural_text() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");

        for original in [
            r"播放 \SE[Bell] 后继续",
            r"C:\Users\Player\save.json",
            r"正则表达式 ^\w+\s+$",
        ] {
            let (protected, placeholders) = service
                .protect(
                    RpgMakerEngine::Mz,
                    TextGroupKind::EventDialogue,
                    original,
                    &CompiledPlaceholderRules::empty(),
                )
                .expect("未命中精确规则的反斜杠文本应该保持为自然文本")
                .into_parts();

            assert_eq!(protected, original);
            assert!(placeholders.is_empty());
        }
    }

    #[test]
    fn mixed_text_only_protects_exact_builtin_controls() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let original = r"路径 C:\Users\Player；播放 \SE[Bell]；颜色 \C[2]红；正则 ^\w+$";

        let (protected, placeholders) = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                original,
                &CompiledPlaceholderRules::empty(),
            )
            .expect("混合文本应该只保护精确命中的内置控制符")
            .into_parts();

        assert_eq!(placeholders.len(), 1);
        assert_eq!(placeholders[0].origin(), PlaceholderRuleOrigin::BuiltIn);
        assert_eq!(placeholders[0].original(), r"\C[2]");
        assert!(protected.contains(r"C:\Users\Player"));
        assert!(protected.contains(r"\SE[Bell]"));
        assert!(protected.contains(r"^\w+$"));
        assert!(!protected.contains(r"\C[2]"));
    }

    #[test]
    fn custom_rule_can_protect_an_external_control_sequence() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");

        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["event_dialogue".to_owned()]),
                r"\\SE\[[^]]+\]",
            )])
            .expect("用户规则应该接管插件控制符");
        let protected = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                r"播放 \SE[Bell] 后继续",
                &custom,
            )
            .expect("自定义控制符应该受到保护");
        let (_, placeholders) = protected.into_parts();

        assert_eq!(placeholders.len(), 1);
        assert_eq!(placeholders[0].origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(placeholders[0].label(), CUSTOM_SEMANTIC_LABEL);
    }
}
