#![allow(dead_code, reason = "占位符服务等待 Planner 生产装配")]

//! MZ 内置控制符与用户 PCRE2 规则的语义化保护。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::att_mz::text::TextGroupKind;

use super::standard::{AppliedPlaceholder, PlaceholderRuleOrigin, PlaceholderSegment};

const BUILTIN_CONTROL_PATTERN: &str = r"\\(?:(?i:V|N|P|C|I|PX|PY|FS)\[\d+\]|(?i:G)|[{}$.|!><^])";

/// 外部 JSON 中一条自定义占位符规则的最小表达。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceholderRuleDefinition {
    scopes: Vec<String>,
    pattern: String,
    label: String,
    translate: Option<String>,
}

impl PlaceholderRuleDefinition {
    pub(crate) fn new(
        scopes: Vec<String>,
        pattern: impl Into<String>,
        label: impl Into<String>,
        translate: Option<String>,
    ) -> Self {
        Self {
            scopes,
            pattern: pattern.into(),
            label: label.into(),
            translate,
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
    pub(crate) fn empty() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }
}

#[derive(Clone)]
struct CompiledPlaceholderRule {
    scopes: Vec<PlaceholderScope>,
    regex: Regex,
    label: String,
    translate_capture: Option<String>,
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
            if definition.scopes.is_empty() {
                return Err(PlaceholderRuleCompilationError::EmptyScopes { rule_number });
            }
            let scopes = definition
                .scopes
                .iter()
                .map(|scope| PlaceholderScope::parse(scope, rule_number))
                .collect::<Result<Vec<_>, _>>()?;
            ensure_unique_scopes(&scopes, rule_number)?;
            validate_untrimmed_non_empty("pattern", &definition.pattern, rule_number)?;
            validate_label(&definition.label, rule_number)?;
            if let Some(capture) = &definition.translate {
                validate_untrimmed_non_empty("translate", capture, rule_number)?;
            }

            let regex = compile_regex(&definition.pattern).map_err(|source| {
                PlaceholderRuleCompilationError::InvalidPattern {
                    rule_number,
                    source,
                }
            })?;
            if let Some(capture) = &definition.translate {
                let matches = regex
                    .capture_names()
                    .iter()
                    .filter(|name| name.as_deref() == Some(capture.as_str()))
                    .count();
                if matches != 1 {
                    return Err(
                        PlaceholderRuleCompilationError::UnknownOrAmbiguousTranslateCapture {
                            rule_number,
                            capture: capture.clone(),
                        },
                    );
                }
            }

            rules.push(CompiledPlaceholderRule {
                scopes,
                regex,
                label: definition.label,
                translate_capture: definition.translate,
            });
        }

        Ok(CompiledPlaceholderRules {
            rules: Arc::new(rules),
        })
    }

    /// 按 Builtin 优先、Custom 文件顺序确定保护区间，并生成可逆 Rust 绑定。
    pub(crate) fn protect(
        &self,
        kind: TextGroupKind,
        original: &str,
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        let scope = PlaceholderScope::from_kind(kind);
        let mut candidates = self.builtin_candidates(original, scope)?;
        for (rule_index, rule) in custom.rules.iter().enumerate() {
            if !rule.scopes.contains(&PlaceholderScope::All) && !rule.scopes.contains(&scope) {
                continue;
            }
            candidates.extend(custom_candidates(rule, rule_index, original, scope)?);
        }

        candidates.sort_by_key(|candidate| {
            (
                candidate.priority,
                candidate.start,
                candidate.end,
                candidate.segment.rank(),
            )
        });
        let mut occupied = Vec::<(usize, usize)>::new();
        let mut selected = Vec::<SelectedSpan>::new();
        for candidate in candidates {
            for (start, end) in subtract_occupied(candidate.start, candidate.end, &occupied) {
                insert_interval(&mut occupied, (start, end));
                selected.push(SelectedSpan {
                    start,
                    end,
                    origin: candidate.origin,
                    label: candidate.label.clone(),
                    scope: candidate.scope,
                    segment: candidate.segment,
                });
            }
        }
        ensure_suspicious_controls_are_protected(original, &occupied)?;
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

    fn builtin_candidates(
        &self,
        original: &str,
        scope: PlaceholderScope,
    ) -> Result<Vec<ProtectionCandidate>, PlaceholderProtectionError> {
        let mut result = Vec::new();
        for matched in self.builtin.find_iter(original.as_bytes()) {
            let matched = matched.map_err(PlaceholderProtectionError::Match)?;
            if matched.start() == matched.end() {
                return Err(PlaceholderProtectionError::EmptyMatch {
                    label: "RMMZ_CONTROL".to_owned(),
                });
            }
            result.push(ProtectionCandidate {
                start: matched.start(),
                end: matched.end(),
                priority: 0,
                origin: PlaceholderRuleOrigin::BuiltIn,
                label: "RMMZ_CONTROL".to_owned(),
                scope,
                segment: PlaceholderSegment::Whole,
            });
        }
        Ok(result)
    }
}

fn ensure_suspicious_controls_are_protected(
    original: &str,
    occupied: &[(usize, usize)],
) -> Result<(), PlaceholderProtectionError> {
    let mut characters = original.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if character != '\\' {
            continue;
        }
        let Some((_, next)) = characters.peek().copied() else {
            continue;
        };
        let suspicious = next.is_ascii_alphabetic() || "{}$.|!><^".contains(next);
        let protected = occupied
            .iter()
            .any(|&(start, end)| start <= offset && offset < end);
        if suspicious && !protected {
            return Err(PlaceholderProtectionError::UnprotectedControl {
                fragment: format!("\\{next}"),
            });
        }
    }
    Ok(())
}

fn compile_regex(pattern: &str) -> Result<Regex, pcre2::Error> {
    RegexBuilder::new()
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(pattern)
}

fn custom_candidates(
    rule: &CompiledPlaceholderRule,
    rule_index: usize,
    original: &str,
    scope: PlaceholderScope,
) -> Result<Vec<ProtectionCandidate>, PlaceholderProtectionError> {
    let mut result = Vec::new();
    for captures in rule.regex.captures_iter(original.as_bytes()) {
        let captures = captures.map_err(PlaceholderProtectionError::Match)?;
        let whole = captures
            .get(0)
            .expect("PCRE2 成功 captures 必须包含整个匹配");
        if whole.start() == whole.end() {
            return Err(PlaceholderProtectionError::EmptyMatch {
                label: rule.label.clone(),
            });
        }

        match &rule.translate_capture {
            None => result.push(ProtectionCandidate {
                start: whole.start(),
                end: whole.end(),
                priority: rule_index + 1,
                origin: PlaceholderRuleOrigin::Custom,
                label: rule.label.clone(),
                scope,
                segment: PlaceholderSegment::Whole,
            }),
            Some(capture_name) => {
                let capture = captures.name(capture_name).ok_or_else(|| {
                    PlaceholderProtectionError::MissingTranslateCapture {
                        label: rule.label.clone(),
                        capture: capture_name.clone(),
                    }
                })?;
                if whole.start() < capture.start() {
                    result.push(ProtectionCandidate {
                        start: whole.start(),
                        end: capture.start(),
                        priority: rule_index + 1,
                        origin: PlaceholderRuleOrigin::Custom,
                        label: rule.label.clone(),
                        scope,
                        segment: PlaceholderSegment::Begin,
                    });
                }
                if capture.end() < whole.end() {
                    result.push(ProtectionCandidate {
                        start: capture.end(),
                        end: whole.end(),
                        priority: rule_index + 1,
                        origin: PlaceholderRuleOrigin::Custom,
                        label: rule.label.clone(),
                        scope,
                        segment: PlaceholderSegment::End,
                    });
                }
            }
        }
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PlaceholderScope {
    All,
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
            "all" => Ok(Self::All),
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
            Self::All => "all",
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

fn validate_untrimmed_non_empty(
    field: &'static str,
    value: &str,
    rule_number: usize,
) -> Result<(), PlaceholderRuleCompilationError> {
    if value.trim().is_empty() {
        return Err(PlaceholderRuleCompilationError::BlankField { rule_number, field });
    }
    if value.trim() != value {
        return Err(PlaceholderRuleCompilationError::SurroundingWhitespace { rule_number, field });
    }
    Ok(())
}

fn validate_label(label: &str, rule_number: usize) -> Result<(), PlaceholderRuleCompilationError> {
    validate_untrimmed_non_empty("label", label, rule_number)?;
    let mut characters = label.chars();
    let valid = characters
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
        && characters.all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        });
    if !valid {
        return Err(PlaceholderRuleCompilationError::InvalidLabel {
            rule_number,
            label: label.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone)]
struct ProtectionCandidate {
    start: usize,
    end: usize,
    priority: usize,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: PlaceholderScope,
    segment: PlaceholderSegment,
}

struct SelectedSpan {
    start: usize,
    end: usize,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: PlaceholderScope,
    segment: PlaceholderSegment,
}

impl PlaceholderSegment {
    const fn rank(self) -> u8 {
        match self {
            Self::Whole => 0,
            Self::Begin => 1,
            Self::End => 2,
        }
    }
}

fn subtract_occupied(start: usize, end: usize, occupied: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut fragments = Vec::new();
    let mut cursor = start;
    for &(occupied_start, occupied_end) in occupied {
        if occupied_end <= cursor || occupied_start >= end {
            continue;
        }
        if cursor < occupied_start {
            fragments.push((cursor, occupied_start.min(end)));
        }
        cursor = cursor.max(occupied_end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        fragments.push((cursor, end));
    }
    fragments
}

fn insert_interval(intervals: &mut Vec<(usize, usize)>, interval: (usize, usize)) {
    let index = intervals
        .binary_search_by_key(&interval.0, |candidate| candidate.0)
        .unwrap_or_else(|index| index);
    intervals.insert(index, interval);
}

fn semantic_token(label: &str, segment: PlaceholderSegment, index: usize) -> String {
    let segment = match segment {
        PlaceholderSegment::Whole => "WHOLE",
        PlaceholderSegment::Begin => "BEGIN",
        PlaceholderSegment::End => "END",
    };
    format!("⟦ATT_{label}_{segment}_{index:04}⟧")
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
        write!(formatter, "无法编译内置 RMMZ 控制符规格：{}", self.0)
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
    BlankField {
        rule_number: usize,
        field: &'static str,
    },
    SurroundingWhitespace {
        rule_number: usize,
        field: &'static str,
    },
    InvalidLabel {
        rule_number: usize,
        label: String,
    },
    InvalidPattern {
        rule_number: usize,
        source: pcre2::Error,
    },
    UnknownOrAmbiguousTranslateCapture {
        rule_number: usize,
        capture: String,
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
            Self::BlankField { rule_number, field } => {
                write!(formatter, "占位符规则 {rule_number} 的 {field} 不能为空白")
            }
            Self::SurroundingWhitespace { rule_number, field } => {
                write!(formatter, "占位符规则 {rule_number} 的 {field} 含首尾空白")
            }
            Self::InvalidLabel { rule_number, label } => write!(
                formatter,
                "占位符规则 {rule_number} 的 label 不是大写 ASCII 标识：{label:?}"
            ),
            Self::InvalidPattern {
                rule_number,
                source,
            } => write!(
                formatter,
                "占位符规则 {rule_number} 的 PCRE2 pattern 无效：{source}"
            ),
            Self::UnknownOrAmbiguousTranslateCapture {
                rule_number,
                capture,
            } => write!(
                formatter,
                "占位符规则 {rule_number} 的 translate 必须精确指向一个命名捕获组：{capture:?}"
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
    MissingTranslateCapture { label: String, capture: String },
    UnprotectedControl { fragment: String },
}

impl fmt::Display for PlaceholderProtectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Match(source) => write!(formatter, "PCRE2 匹配失败：{source}"),
            Self::EmptyMatch { label } => write!(formatter, "占位符规则 {label} 产生空匹配"),
            Self::MissingTranslateCapture { label, capture } => write!(
                formatter,
                "占位符规则 {label} 的命名组 {capture:?} 未参与匹配"
            ),
            Self::UnprotectedControl { fragment } => write!(
                formatter,
                "发现未被规则保护的疑似 RMMZ 控制符：{fragment:?}"
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
    fn builtin_controls_have_highest_priority() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                vec!["event_dialogue".to_owned()],
                r"\\C\[[^]]+\]勇者",
                "CUSTOM",
                None,
            )])
            .expect("自定义规则应该有效");

        let protected = service
            .protect(TextGroupKind::EventDialogue, r"\C[2]勇者", &custom)
            .expect("重叠规则应该按优先级保护");
        let (text, bindings) = protected.into_parts();

        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].origin(), PlaceholderRuleOrigin::BuiltIn);
        assert_eq!(bindings[0].original(), r"\C[2]");
        assert_eq!(bindings[1].origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(bindings[1].original(), "勇者");
        assert!(!text.contains(r"\C[2]"));
    }

    #[test]
    fn structured_rule_protects_shell_and_keeps_named_capture_translatable() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                vec!["plugin_parameter".to_owned()],
                r"<name>(?<text>.*?)</name>",
                "NAME_TAG",
                Some("text".to_owned()),
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
    fn invalid_capture_and_unknown_fields_are_rejected() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");
        let error = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                vec!["all".to_owned()],
                "(?<text>.+)",
                "TEXT",
                Some("missing".to_owned()),
            )])
            .expect_err("不存在的命名捕获组应该失败");
        assert!(matches!(
            error,
            PlaceholderRuleCompilationError::UnknownOrAmbiguousTranslateCapture { .. }
        ));

        assert!(
            serde_json::from_str::<Vec<PlaceholderRuleDefinition>>(
                r#"[{"scopes":["all"],"pattern":"x","label":"X","extra":true}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_control_requires_an_explicit_custom_rule() {
        let service = Pcre2PlaceholderService::new().expect("内置规则应该有效");

        assert!(matches!(
            service.protect(
                TextGroupKind::EventDialogue,
                r"播放 \SE[Bell] 后继续",
                &CompiledPlaceholderRules::empty(),
            ),
            Err(PlaceholderProtectionError::UnprotectedControl { fragment })
                if fragment == r"\S"
        ));

        let custom = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                vec!["event_dialogue".to_owned()],
                r"\\SE\[[^]]+\]",
                "SOUND_EFFECT",
                None,
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
        assert_eq!(placeholders[0].label(), "SOUND_EFFECT");
    }
}
