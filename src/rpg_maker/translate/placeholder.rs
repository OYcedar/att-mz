//! RPG Maker 内置控制符与用户 PCRE2 规则的语义化保护。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::rpg_maker::placeholder_token;
use crate::rpg_maker::text::TextGroupKind;

use super::standard::{AppliedPlaceholder, PlaceholderRuleOrigin, PlaceholderSegment};

const BUILTIN_CONTROL_PATTERN: &str = r"\\(?:(?i:V|N|P|C|I|PX|PY|FS)\[\d+\]|(?i:G)|[\\{}$.|!><^])";

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
    builtin: Regex,
}

impl Pcre2PlaceholderService {
    pub(crate) fn new() -> Result<Self, Pcre2PlaceholderConstructionError> {
        Ok(Self {
            builtin: compile_regex(BUILTIN_CONTROL_PATTERN)
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
    pub(crate) fn protect(
        &self,
        kind: TextGroupKind,
        original: &str,
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        if placeholder_token::contains_reserved_prefix(original) {
            return Err(PlaceholderProtectionError::ReservedTokenNamespace);
        }

        let scope = PlaceholderScope::from_kind(kind);
        let mut matches = self.builtin_matches(original, scope)?;
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
        for (index, current) in matches.iter().enumerate() {
            for previous in &matches[..index] {
                if current.start < previous.end
                    && previous.start < current.end
                    && (current.origin == PlaceholderRuleOrigin::Custom
                        || previous.origin == PlaceholderRuleOrigin::Custom)
                {
                    return Err(PlaceholderProtectionError::OverlappingMatches {
                        first: previous.label.clone(),
                        second: current.label.clone(),
                    });
                }
            }
        }
        let mut selected = matches
            .into_iter()
            .flat_map(|matched| matched.protected)
            .collect::<Vec<_>>();
        selected.sort_by_key(|span| (span.start, span.end));

        let mut protected = String::with_capacity(original.len());
        let mut placeholders = Vec::with_capacity(selected.len());
        let mut cursor = 0;
        for (index, span) in selected.into_iter().enumerate() {
            protected.push_str(&original[cursor..span.start]);
            let token = semantic_token(&span.label, span.segment, index);
            protected.push_str(&token);
            placeholders.push(AppliedPlaceholder::new(
                token,
                &original[span.start..span.end],
                span.origin,
                span.label,
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
        original: &str,
        scope: PlaceholderScope,
    ) -> Result<Vec<ProtectionMatch>, PlaceholderProtectionError> {
        let mut result = Vec::new();
        for matched in self.builtin.find_iter(original.as_bytes()) {
            let matched = matched.map_err(PlaceholderProtectionError::Match)?;
            if matched.start() == matched.end() {
                return Err(PlaceholderProtectionError::EmptyMatch {
                    label: "RPG_MAKER_CONTROL".to_owned(),
                });
            }
            result.push(ProtectionMatch {
                start: matched.start(),
                end: matched.end(),
                origin: PlaceholderRuleOrigin::BuiltIn,
                label: "RPG_MAKER_CONTROL".to_owned(),
                protected: vec![SelectedSpan {
                    start: matched.start(),
                    end: matched.end(),
                    origin: PlaceholderRuleOrigin::BuiltIn,
                    label: "RPG_MAKER_CONTROL".to_owned(),
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
                label: custom_label(rule.rule_number),
            });
        }

        let label = custom_label(rule.rule_number);
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
                    label: label.clone(),
                    scope,
                    segment: PlaceholderSegment::Begin,
                });
            }
            if capture.end() < whole.end() {
                protected.push(SelectedSpan {
                    start: capture.end(),
                    end: whole.end(),
                    origin: PlaceholderRuleOrigin::Custom,
                    label: label.clone(),
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
                label: label.clone(),
                scope,
                segment: PlaceholderSegment::Whole,
            }]
        };
        result.push(ProtectionMatch {
            start: whole.start(),
            end: whole.end(),
            origin: PlaceholderRuleOrigin::Custom,
            label,
            protected,
        });
    }
    Ok(result)
}

fn valid_utf8_range(text: &str, start: usize, end: usize) -> bool {
    start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

fn custom_label(rule_number: usize) -> String {
    format!("CUSTOM_{rule_number:04}")
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PlaceholderScope {
    DatabaseEntry,
    System,
    Map,
    EventDialogue,
    EventChoices,
    EventScrollingText,
    EventCommand,
    PluginParameter,
}

impl PlaceholderScope {
    fn parse(value: &str, rule_number: usize) -> Result<Self, PlaceholderRuleCompilationError> {
        match value {
            "database_entry" => Ok(Self::DatabaseEntry),
            "system" => Ok(Self::System),
            "map" => Ok(Self::Map),
            "event_dialogue" => Ok(Self::EventDialogue),
            "event_choices" => Ok(Self::EventChoices),
            "event_scrolling_text" => Ok(Self::EventScrollingText),
            "event_command" => Ok(Self::EventCommand),
            "plugin_parameter" => Ok(Self::PluginParameter),
            _ => Err(PlaceholderRuleCompilationError::UnknownScope {
                rule_number,
                scope: value.to_owned(),
            }),
        }
    }

    const fn from_kind(kind: TextGroupKind) -> Self {
        match kind {
            TextGroupKind::DatabaseEntry => Self::DatabaseEntry,
            TextGroupKind::System => Self::System,
            TextGroupKind::Map => Self::Map,
            TextGroupKind::EventDialogue => Self::EventDialogue,
            TextGroupKind::EventChoices => Self::EventChoices,
            TextGroupKind::EventScrollingText => Self::EventScrollingText,
            TextGroupKind::EventCommand => Self::EventCommand,
            TextGroupKind::PluginParameter => Self::PluginParameter,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::DatabaseEntry => "database_entry",
            Self::System => "system",
            Self::Map => "map",
            Self::EventDialogue => "event_dialogue",
            Self::EventChoices => "event_choices",
            Self::EventScrollingText => "event_scrolling_text",
            Self::EventCommand => "event_command",
            Self::PluginParameter => "plugin_parameter",
        }
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
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    label: String,
    protected: Vec<SelectedSpan>,
}

struct SelectedSpan {
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    label: String,
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
    EmptyMatch { label: String },
    MissingTextCapture { rule_number: usize },
    InvalidMatchRange { rule_number: usize },
    OverlappingMatches { first: String, second: String },
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
            service.protect(TextGroupKind::EventDialogue, r"\C[2]勇者", &custom),
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
    fn original_text_cannot_enter_the_reserved_token_namespace() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");

        assert!(matches!(
            service.protect(
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
    fn omitted_scope_is_global_but_explicit_scope_is_local() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let global = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "TOKEN")])
            .expect("缺省 scopes 应表示全局");
        let (_, bindings) = service
            .protect(TextGroupKind::System, "TOKEN", &global)
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
            .protect(TextGroupKind::System, "TOKEN", &local)
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
            service.protect(TextGroupKind::Map, "abc", &overlapping),
            Err(PlaceholderProtectionError::OverlappingMatches { .. })
        ));

        let no_hit = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(None, "missing")])
            .expect("规则应可编译");
        let (text, bindings) = service
            .protect(TextGroupKind::Map, "source", &no_hit)
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
            service.protect(TextGroupKind::Map, "source", &custom),
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
                service.protect(TextGroupKind::Map, "莉", &custom),
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
            service.protect(TextGroupKind::Map, "AB", &custom),
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
                TextGroupKind::EventDialogue,
                r"播放 \SE[Bell] 后继续",
                &custom,
            )
            .expect("自定义控制符应该受到保护");
        let (_, placeholders) = protected.into_parts();

        assert_eq!(placeholders.len(), 1);
        assert_eq!(placeholders[0].origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(placeholders[0].label(), "CUSTOM_0001");
    }
}
