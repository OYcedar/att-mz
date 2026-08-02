//! Rules TOML 的严格解析边界。

use std::error::Error;
use std::fmt;

use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::text::DataFileName;

/// 已完成来源、路径和 PCRE2 ABI 校验的 Rules 定义。
#[derive(Clone, Debug)]
pub(super) struct RulesDefinition {
    rules: Vec<RuleDefinition>,
    canonical_json: String,
}

impl RulesDefinition {
    /// 只接受当前 Rules TOML 契约；根必须显式声明 `rule`。
    pub(super) fn parse(source: &str) -> Result<Self, RulesDefinitionError> {
        let raw: RawRulesDefinition =
            toml::from_str(source).map_err(RulesDefinitionError::InvalidToml)?;
        Self::from_raw_rules(raw.rule)
    }

    /// 从项目数据库保存的当前 canonical 语义重建已验证规则。
    pub(super) fn parse_canonical_json(source: &str) -> Result<Self, RulesDefinitionError> {
        let raw = serde_json::from_str::<Vec<RawRuleDefinition>>(source)
            .map_err(RulesDefinitionError::InvalidCanonicalJson)?;
        let definition = Self::from_raw_rules(raw)?;
        if definition.canonical_json != source {
            return Err(RulesDefinitionError::NonCanonicalJson);
        }
        Ok(definition)
    }

    fn from_raw_rules(raw: Vec<RawRuleDefinition>) -> Result<Self, RulesDefinitionError> {
        let canonical_json =
            serde_json::to_string(&raw).map_err(RulesDefinitionError::EncodeCanonicalJson)?;
        let rules = raw
            .into_iter()
            .enumerate()
            .map(|(index, rule)| RuleDefinition::try_from_raw(index + 1, rule))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            rules,
            canonical_json,
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub(super) fn rules(&self) -> &[RuleDefinition] {
        &self.rules
    }

    pub(super) fn into_rules(self) -> Vec<RuleDefinition> {
        self.rules
    }

    pub(super) fn canonical_json(&self) -> &str {
        &self.canonical_json
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRulesDefinition {
    rule: Vec<RawRuleDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawRuleDefinition {
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameter: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    decode_json: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// 一条规则的受信内部表达。
#[derive(Clone, Debug)]
pub(super) struct RuleDefinition {
    rule_number: usize,
    source: RuleSource,
    path: Option<CompiledPath>,
    decode_json: bool,
    pattern: Option<CompiledPattern>,
}

impl RuleDefinition {
    fn try_from_raw(
        rule_number: usize,
        raw: RawRuleDefinition,
    ) -> Result<Self, RulesDefinitionError> {
        let source = parse_rule_source(rule_number, raw.file, raw.plugin, raw.code, raw.parameter)?;

        let path = raw
            .path
            .map(|path| {
                CompiledPath::parse(&path).map_err(|reason| RulesDefinitionError::InvalidPath {
                    rule_number,
                    reason,
                })
            })
            .transpose()?;
        if source.requires_path() && path.is_none() {
            return Err(RulesDefinitionError::MissingPath {
                rule_number,
                source: source.kind_name(),
            });
        }

        let pattern = raw
            .pattern
            .map(|pattern| CompiledPattern::compile(rule_number, pattern))
            .transpose()?;

        Ok(Self {
            rule_number,
            source,
            path,
            decode_json: raw.decode_json,
            pattern,
        })
    }

    pub(super) fn rule_number(&self) -> usize {
        self.rule_number
    }

    pub(super) fn source(&self) -> &RuleSource {
        &self.source
    }

    pub(super) fn path(&self) -> Option<&CompiledPath> {
        self.path.as_ref()
    }

    pub(super) fn decode_json(&self) -> bool {
        self.decode_json
    }

    pub(super) fn pattern(&self) -> Option<&CompiledPattern> {
        self.pattern.as_ref()
    }
}

/// 一条规则唯一的数据来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RuleSource {
    File(FileRuleSource),
    Plugin(String),
    Command { code: i64, parameter: usize },
}

impl RuleSource {
    fn requires_path(&self) -> bool {
        matches!(self, Self::File(_) | Self::Plugin(_))
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Plugin(_) => "plugin",
            Self::Command { .. } => "code + parameter",
        }
    }
}

/// data 目录中的安全精确文件，或唯一受支持的地图通配符。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FileRuleSource {
    Exact(String),
    AllMaps,
}

fn parse_rule_source(
    rule_number: usize,
    file: Option<String>,
    plugin: Option<String>,
    code: Option<i64>,
    parameter: Option<usize>,
) -> Result<RuleSource, RulesDefinitionError> {
    let source_count =
        usize::from(file.is_some()) + usize::from(plugin.is_some()) + usize::from(code.is_some());
    if source_count == 0 {
        return Err(RulesDefinitionError::MissingSource { rule_number });
    }
    if source_count > 1 {
        return Err(RulesDefinitionError::ConflictingSources { rule_number });
    }

    if let Some(file) = file {
        if parameter.is_some() {
            return Err(RulesDefinitionError::ParameterWithoutCode { rule_number });
        }
        return Ok(RuleSource::File(parse_file_source(rule_number, file)?));
    }
    if let Some(plugin) = plugin {
        if parameter.is_some() {
            return Err(RulesDefinitionError::ParameterWithoutCode { rule_number });
        }
        if plugin.trim().is_empty() {
            return Err(RulesDefinitionError::EmptyField {
                rule_number,
                field: "plugin",
            });
        }
        return Ok(RuleSource::Plugin(plugin));
    }

    let code = code.expect("来源计数已经证明 code 存在");
    if code < 0 {
        return Err(RulesDefinitionError::InvalidCode { rule_number, code });
    }
    let parameter = parameter.ok_or(RulesDefinitionError::MissingParameter { rule_number })?;
    Ok(RuleSource::Command { code, parameter })
}

fn parse_file_source(
    rule_number: usize,
    file: String,
) -> Result<FileRuleSource, RulesDefinitionError> {
    if file == "Map*.json" {
        return Ok(FileRuleSource::AllMaps);
    }

    if DataFileName::parse(file.clone()).is_err() {
        return Err(RulesDefinitionError::InvalidFile { rule_number });
    }

    Ok(FileRuleSource::Exact(file))
}

/// 路径进入 JSON 值时允许的最小步骤集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PathSegment {
    Key(String),
    AnyIndex,
    Index(usize),
}

/// 已完成语法校验的窄路径。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompiledPath {
    source: String,
    segments: Vec<PathSegment>,
}

impl CompiledPath {
    fn parse(path: &str) -> Result<Self, InvalidPathReason> {
        if path.is_empty() {
            return Err(InvalidPathReason::Empty);
        }
        if path.starts_with('$') {
            return Err(InvalidPathReason::UnsupportedJsonPath { offset: 0 });
        }

        let bytes = path.as_bytes();
        let mut cursor = 0;
        let mut segments = Vec::new();
        let mut expect_segment = true;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'.' => {
                    if expect_segment {
                        return Err(InvalidPathReason::UnexpectedDot { offset: cursor });
                    }
                    cursor += 1;
                    expect_segment = true;
                }
                b'[' => {
                    if cursor > 0 && bytes[cursor - 1] == b'.' {
                        return Err(InvalidPathReason::DotBeforeBracket { offset: cursor - 1 });
                    }
                    let (segment, next) = parse_bracket_segment(path, cursor)?;
                    segments.push(segment);
                    cursor = next;
                    expect_segment = false;
                }
                _ => {
                    if !expect_segment {
                        return Err(InvalidPathReason::MissingDot { offset: cursor });
                    }
                    let start = cursor;
                    while cursor < bytes.len() && !matches!(bytes[cursor], b'.' | b'[' | b']') {
                        cursor += 1;
                    }
                    let key = &path[start..cursor];
                    if key.is_empty()
                        || !key
                            .chars()
                            .all(|character| character == '_' || character.is_ascii_alphanumeric())
                        || key
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_ascii_digit())
                    {
                        return Err(InvalidPathReason::InvalidBareKey { offset: start });
                    }
                    segments.push(PathSegment::Key(key.to_owned()));
                    expect_segment = false;
                }
            }
        }
        if expect_segment || segments.is_empty() {
            return Err(InvalidPathReason::TrailingDot {
                offset: path.len().saturating_sub(1),
            });
        }

        Ok(Self {
            source: path.to_owned(),
            segments,
        })
    }

    #[cfg(test)]
    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) fn segments(&self) -> &[PathSegment] {
        &self.segments
    }
}

fn parse_bracket_segment(
    path: &str,
    start: usize,
) -> Result<(PathSegment, usize), InvalidPathReason> {
    let bytes = path.as_bytes();
    let mut cursor = start + 1;
    if cursor >= bytes.len() {
        return Err(InvalidPathReason::UnclosedBracket { offset: start });
    }
    if bytes[cursor] == b']' {
        return Ok((PathSegment::AnyIndex, cursor + 1));
    }
    if bytes[cursor] == b'"' {
        let string_start = cursor;
        let mut strings =
            serde_json::Deserializer::from_str(&path[string_start..]).into_iter::<String>();
        let key = strings
            .next()
            .ok_or(InvalidPathReason::MissingQuotedKey {
                offset: string_start,
            })?
            .map_err(|error| InvalidPathReason::InvalidQuotedKey {
                offset: string_start,
                json_category: JsonErrorCategory::from(&error),
                line: error.line(),
                column: error.column(),
            })?;
        cursor = string_start + strings.byte_offset();
        if bytes.get(cursor) != Some(&b']') {
            return Err(InvalidPathReason::QuotedKeyMissingClose { offset: cursor });
        }
        return Ok((PathSegment::Key(key), cursor + 1));
    }

    let digits_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    if cursor == digits_start || cursor >= bytes.len() || bytes[cursor] != b']' {
        return Err(InvalidPathReason::InvalidBracket { offset: start });
    }
    let index = path[digits_start..cursor].parse::<usize>().map_err(|_| {
        InvalidPathReason::IndexOutOfRange {
            offset: digits_start,
        }
    })?;
    Ok((PathSegment::Index(index), cursor + 1))
}

/// 路径语法错误只保存稳定的语法类别与字节位置，不保存解析器自由文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidPathReason {
    Empty,
    UnsupportedJsonPath {
        offset: usize,
    },
    UnexpectedDot {
        offset: usize,
    },
    DotBeforeBracket {
        offset: usize,
    },
    MissingDot {
        offset: usize,
    },
    InvalidBareKey {
        offset: usize,
    },
    TrailingDot {
        offset: usize,
    },
    UnclosedBracket {
        offset: usize,
    },
    MissingQuotedKey {
        offset: usize,
    },
    InvalidQuotedKey {
        offset: usize,
        json_category: JsonErrorCategory,
        line: usize,
        column: usize,
    },
    QuotedKeyMissingClose {
        offset: usize,
    },
    InvalidBracket {
        offset: usize,
    },
    IndexOutOfRange {
        offset: usize,
    },
}

impl fmt::Display for InvalidPathReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("路径不能为空"),
            Self::UnsupportedJsonPath { offset } => {
                write!(formatter, "路径第 {offset} 个字节处不支持 JSONPath 语法")
            }
            Self::UnexpectedDot { offset } => {
                write!(formatter, "路径第 {offset} 个字节处出现了意外的点号")
            }
            Self::DotBeforeBracket { offset } => {
                write!(
                    formatter,
                    "路径第 {offset} 个字节处的点号后不能直接跟方括号"
                )
            }
            Self::MissingDot { offset } => {
                write!(formatter, "路径第 {offset} 个字节处缺少分隔点号")
            }
            Self::InvalidBareKey { offset } => {
                write!(formatter, "路径第 {offset} 个字节处的裸键无效")
            }
            Self::TrailingDot { offset } => {
                write!(formatter, "路径第 {offset} 个字节处以点号结束")
            }
            Self::UnclosedBracket { offset } => {
                write!(formatter, "路径第 {offset} 个字节处的方括号没有闭合")
            }
            Self::MissingQuotedKey { offset } => {
                write!(formatter, "路径第 {offset} 个字节处缺少带引号的键")
            }
            Self::InvalidQuotedKey {
                offset,
                json_category,
                line,
                column,
            } => write!(
                formatter,
                "路径第 {offset} 个字节处的带引号键不是有效 JSON 字符串（{}，第 {line} 行第 {column} 列）",
                json_error_category_name(*json_category)
            ),
            Self::QuotedKeyMissingClose { offset } => {
                write!(formatter, "路径第 {offset} 个字节处的带引号键没有闭合")
            }
            Self::InvalidBracket { offset } => {
                write!(formatter, "路径第 {offset} 个字节处的方括号内容无效")
            }
            Self::IndexOutOfRange { offset } => {
                write!(formatter, "路径第 {offset} 个字节处的数组索引超出范围")
            }
        }
    }
}

/// 已编译且满足单一 `text` 命名捕获 ABI 的 PCRE2。
#[derive(Clone)]
pub(super) struct CompiledPattern {
    source: String,
    regex: Regex,
}

impl CompiledPattern {
    fn compile(rule_number: usize, source: String) -> Result<Self, RulesDefinitionError> {
        if source.is_empty() {
            return Err(RulesDefinitionError::EmptyPattern { rule_number });
        }
        let regex = RegexBuilder::new()
            .utf(true)
            .ucp(true)
            .jit_if_available(true)
            .build(&source)
            .map_err(|error| RulesDefinitionError::InvalidPattern {
                rule_number,
                source: error,
            })?;
        let captures = regex
            .capture_names()
            .iter()
            .filter_map(Option::as_deref)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if captures.as_slice() != ["text"].as_slice() {
            return Err(RulesDefinitionError::InvalidNamedCaptures {
                rule_number,
                actual_count: captures.len(),
            });
        }

        Ok(Self { source, regex })
    }

    #[cfg(test)]
    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) fn regex(&self) -> &Regex {
        &self.regex
    }
}

impl fmt::Debug for CompiledPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledPattern")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Rules TOML 在外部边界产生的结构化错误。
#[derive(Debug)]
pub(crate) enum RulesDefinitionError {
    InvalidToml(toml::de::Error),
    InvalidCanonicalJson(serde_json::Error),
    EncodeCanonicalJson(serde_json::Error),
    NonCanonicalJson,
    MissingSource {
        rule_number: usize,
    },
    ConflictingSources {
        rule_number: usize,
    },
    ParameterWithoutCode {
        rule_number: usize,
    },
    MissingParameter {
        rule_number: usize,
    },
    InvalidCode {
        rule_number: usize,
        code: i64,
    },
    MissingPath {
        rule_number: usize,
        source: &'static str,
    },
    EmptyField {
        rule_number: usize,
        field: &'static str,
    },
    InvalidFile {
        rule_number: usize,
    },
    InvalidPath {
        rule_number: usize,
        reason: InvalidPathReason,
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
        actual_count: usize,
    },
}

pub(super) fn write_pcre2_error(
    formatter: &mut fmt::Formatter<'_>,
    source: &pcre2::Error,
) -> fmt::Result {
    let kind = match source.kind() {
        pcre2::ErrorKind::Compile => "编译",
        pcre2::ErrorKind::JIT => "JIT",
        pcre2::ErrorKind::Match => "匹配",
        pcre2::ErrorKind::Info => "信息查询",
        pcre2::ErrorKind::Option => "选项",
        _ => "未知",
    };
    write!(formatter, "PCRE2 {kind}错误（代码 {}", source.code())?;
    if let Some(offset) = source.offset() {
        write!(formatter, "，偏移 {offset}")?;
    }
    formatter.write_str("）")
}

fn json_error_category_name(category: JsonErrorCategory) -> &'static str {
    match category {
        JsonErrorCategory::Io => "I/O 错误",
        JsonErrorCategory::Syntax => "语法错误",
        JsonErrorCategory::Data => "数据错误",
        JsonErrorCategory::Eof => "意外结束",
        JsonErrorCategory::DuplicateObjectKey => "重复对象键",
    }
}

fn rule_source_name(source: &str) -> &str {
    match source {
        "file" => "文件",
        "plugin" => "插件",
        "command" => "事件命令",
        _ => source,
    }
}

impl fmt::Display for RulesDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToml(source) => {
                formatter.write_str("Rules TOML 格式或结构无效")?;
                if let Some(span) = source.span() {
                    write!(formatter, "（字节范围 {}..{}）", span.start, span.end)?;
                }
                Ok(())
            }
            Self::InvalidCanonicalJson(source) => write!(
                formatter,
                "保存的 Rules canonical JSON 无效（{}，第 {} 行第 {} 列）",
                json_error_category_name(JsonErrorCategory::from(source)),
                source.line(),
                source.column()
            ),
            Self::EncodeCanonicalJson(source) => write!(
                formatter,
                "Rules canonical JSON 编码失败（{}，第 {} 行第 {} 列）",
                json_error_category_name(JsonErrorCategory::from(source)),
                source.line(),
                source.column()
            ),
            Self::NonCanonicalJson => {
                formatter.write_str("保存的 Rules canonical JSON 不符合当前规范编码")
            }
            Self::MissingSource { rule_number } => {
                write!(formatter, "Rules 规则 {rule_number} 没有声明来源")
            }
            Self::ConflictingSources { rule_number } => write!(
                formatter,
                "Rules 规则 {rule_number} 同时声明了互斥的文件、插件或事件命令来源"
            ),
            Self::ParameterWithoutCode { rule_number } => write!(
                formatter,
                "Rules 规则 {rule_number} 声明了 parameter，但没有声明 code"
            ),
            Self::MissingParameter { rule_number } => write!(
                formatter,
                "Rules 规则 {rule_number} 声明了 code，但没有声明 parameter"
            ),
            Self::InvalidCode { rule_number, code } => write!(
                formatter,
                "Rules 规则 {rule_number} 的 code 为 {code}，必须是非负整数"
            ),
            Self::MissingPath {
                rule_number,
                source,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 的{}来源缺少 path",
                rule_source_name(source)
            ),
            Self::EmptyField { rule_number, field } => {
                write!(
                    formatter,
                    "Rules 规则 {rule_number} 的字段 {field:?} 不能为空"
                )
            }
            Self::InvalidFile { rule_number, .. } => write!(
                formatter,
                "Rules 规则 {rule_number} 的 file 必须是安全的精确 .json 基名或 Map*.json"
            ),
            Self::InvalidPath {
                rule_number,
                reason,
                ..
            } => write!(formatter, "Rules 规则 {rule_number} 的 path 无效：{reason}"),
            Self::EmptyPattern { rule_number } => {
                write!(formatter, "Rules 规则 {rule_number} 的 pattern 不能为空")
            }
            Self::InvalidPattern {
                rule_number,
                source,
                ..
            } => {
                write!(formatter, "Rules 规则 {rule_number} 的 pattern 无法编译：")?;
                write_pcre2_error(formatter, source)
            }
            Self::InvalidNamedCaptures {
                rule_number,
                actual_count,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 的 pattern 必须且只能有一个名为 text 的命名捕获，实际有 {actual_count} 个"
            ),
        }
    }
}

impl Error for RulesDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidToml(source) => Some(source),
            Self::InvalidCanonicalJson(source) | Self::EncodeCanonicalJson(source) => Some(source),
            Self::InvalidPattern { source, .. } => Some(source),
            Self::NonCanonicalJson
            | Self::MissingSource { .. }
            | Self::ConflictingSources { .. }
            | Self::ParameterWithoutCode { .. }
            | Self::MissingParameter { .. }
            | Self::InvalidCode { .. }
            | Self::MissingPath { .. }
            | Self::EmptyField { .. }
            | Self::InvalidFile { .. }
            | Self::InvalidPath { .. }
            | Self::EmptyPattern { .. }
            | Self::InvalidNamedCaptures { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_literal_string_preserves_pcre2_backslashes() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
code = 356
parameter = 0
pattern = '(?i)\AGabText\s+(?<text>.+)\z'
"#,
        )
        .expect("TOML literal string 应该原样承载 PCRE2");

        let rule = &definition.rules()[0];
        assert_eq!(rule.rule_number(), 1);
        assert_eq!(
            rule.source(),
            &RuleSource::Command {
                code: 356,
                parameter: 0
            }
        );
        assert_eq!(
            rule.pattern().expect("应该有 pattern").source(),
            r"(?i)\AGabText\s+(?<text>.+)\z"
        );
    }

    #[test]
    fn root_requires_an_explicit_rule_collection() {
        let empty = RulesDefinition::parse("rule = []").expect("显式空集合应该合法");
        assert!(empty.is_empty());
        assert!(empty.rules().is_empty());
        assert!(empty.into_rules().is_empty());

        for source in ["", "# only a comment\n"] {
            assert!(matches!(
                RulesDefinition::parse(source),
                Err(RulesDefinitionError::InvalidToml(_))
            ));
        }
    }

    #[test]
    fn definition_error_display_is_human_text_not_detail_protocol() {
        let error = RulesDefinition::parse(
            r#"
[[rule]]
file = "Actors.json"
path = ''
"#,
        )
        .expect_err("空路径必须失败");

        let rendered = error.to_string();
        assert_eq!(rendered, "Rules 规则 1 的 path 无效：路径不能为空");
        assert!(!rendered.contains("rule="));
        assert!(!rendered.contains("path_error="));
    }

    #[test]
    fn toml_rejects_unknown_and_duplicate_fields() {
        for source in [
            "rule = []\nversion = 1",
            r#"
[[rule]]
file = "Actors.json"
path = '[].name'
label = "actor"
"#,
            r#"
[[rule]]
file = "Actors.json"
file = "Items.json"
path = '[].name'
"#,
        ] {
            assert!(matches!(
                RulesDefinition::parse(source),
                Err(RulesDefinitionError::InvalidToml(_))
            ));
        }
    }

    #[test]
    fn source_selection_is_mutually_exclusive_and_complete() {
        let missing = "[[rule]]\npath = 'name'";
        assert!(matches!(
            RulesDefinition::parse(missing),
            Err(RulesDefinitionError::MissingSource { rule_number: 1 })
        ));

        let conflicting = "[[rule]]\nfile = 'Actors.json'\nplugin = 'Quest'\npath = 'name'";
        assert!(matches!(
            RulesDefinition::parse(conflicting),
            Err(RulesDefinitionError::ConflictingSources { rule_number: 1 })
        ));

        for source in [
            "[[rule]]\nfile = 'Actors.json'\nparameter = 0\npath = 'name'",
            "[[rule]]\nplugin = 'Quest'\nparameter = 0\npath = 'name'",
        ] {
            assert!(matches!(
                RulesDefinition::parse(source),
                Err(RulesDefinitionError::ParameterWithoutCode { rule_number: 1 })
            ));
        }

        assert!(matches!(
            RulesDefinition::parse("[[rule]]\ncode = 401"),
            Err(RulesDefinitionError::MissingParameter { rule_number: 1 })
        ));
        assert!(matches!(
            RulesDefinition::parse("[[rule]]\ncode = -1\nparameter = 0"),
            Err(RulesDefinitionError::InvalidCode {
                rule_number: 1,
                code: -1
            })
        ));
    }

    #[test]
    fn file_and_plugin_require_paths_but_command_path_is_optional() {
        for source in [
            "[[rule]]\nfile = 'Actors.json'",
            "[[rule]]\nplugin = 'Quest'",
        ] {
            assert!(matches!(
                RulesDefinition::parse(source),
                Err(RulesDefinitionError::MissingPath { rule_number: 1, .. })
            ));
        }

        let direct = RulesDefinition::parse("[[rule]]\ncode = 401\nparameter = 0")
            .expect("命令参数可以直接作为最终叶");
        assert!(direct.rules()[0].path().is_none());
        assert!(!direct.rules()[0].decode_json());
    }

    #[test]
    fn file_accepts_safe_exact_json_names_and_only_the_map_wildcard() {
        for (file, expected) in [
            (
                "Disciplines.json",
                FileRuleSource::Exact("Disciplines.json".to_owned()),
            ),
            (
                "Map001.json",
                FileRuleSource::Exact("Map001.json".to_owned()),
            ),
            ("Map*.json", FileRuleSource::AllMaps),
            (
                "自定义.json",
                FileRuleSource::Exact("自定义.json".to_owned()),
            ),
        ] {
            let source = format!("[[rule]]\nfile = {file:?}\npath = 'name'");
            let definition = RulesDefinition::parse(&source).expect("安全 JSON 基名应该合法");
            assert_eq!(definition.rules()[0].source(), &RuleSource::File(expected));
        }

        for file in [
            "",
            "Actors.JSON",
            "../Actors.json",
            "data/Actors.json",
            r"data\Actors.json",
            "*.json",
            "Map??.json",
            "CON.json",
            "NUL.metadata.json",
            "COM1.any.json",
            "Actors.json.",
        ] {
            let source = format!("[[rule]]\nfile = {file:?}\npath = 'name'");
            assert!(matches!(
                RulesDefinition::parse(&source),
                Err(RulesDefinitionError::InvalidFile { rule_number: 1, .. })
            ));
        }
    }

    #[test]
    fn path_supports_only_keys_indices_expansion_and_exact_quoted_keys() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Disciplines.json"
path = '[][0]["exact.key"].Name'
decode_json = true
"#,
        )
        .expect("窄路径的四种步骤应该可以组合");
        let rule = &definition.rules()[0];
        assert!(rule.decode_json());
        let path = rule.path().expect("文件来源必须有路径");
        assert_eq!(path.source(), r#"[][0]["exact.key"].Name"#);
        assert_eq!(
            path.segments(),
            [
                PathSegment::AnyIndex,
                PathSegment::Index(0),
                PathSegment::Key("exact.key".to_owned()),
                PathSegment::Key("Name".to_owned()),
            ]
        );

        for path in [
            "",
            "$[0].name",
            "name..value",
            "name.",
            "name.[0]",
            "name value",
            "[abc]",
            "0name",
            "name]",
        ] {
            let source = format!("[[rule]]\nfile = 'Actors.json'\npath = {path:?}");
            assert!(matches!(
                RulesDefinition::parse(&source),
                Err(RulesDefinitionError::InvalidPath { rule_number: 1, .. })
            ));
        }
    }

    #[test]
    fn quoted_path_keys_follow_json_string_grammar_and_may_be_empty() {
        for (path, expected) in [
            (r#"[""]"#, vec![PathSegment::Key(String::new())]),
            (r#"["中文"]"#, vec![PathSegment::Key("中文".to_owned())]),
            (r#"["\u4E2D"]"#, vec![PathSegment::Key("中".to_owned())]),
            (
                r#"["\uD83D\uDE00"]"#,
                vec![PathSegment::Key("😀".to_owned())],
            ),
            (
                r#"["quote\"slash\\"]"#,
                vec![PathSegment::Key("quote\"slash\\".to_owned())],
            ),
            (r#"["]"]"#, vec![PathSegment::Key("]".to_owned())]),
            (
                r#"[""].next[1]["尾"]"#,
                vec![
                    PathSegment::Key(String::new()),
                    PathSegment::Key("next".to_owned()),
                    PathSegment::Index(1),
                    PathSegment::Key("尾".to_owned()),
                ],
            ),
        ] {
            let compiled = CompiledPath::parse(path).expect("合法 JSON 字符串应当成为精确键");
            assert_eq!(compiled.source(), path);
            assert_eq!(compiled.segments(), expected);
        }
    }

    #[test]
    fn quoted_path_keys_reject_invalid_json_or_non_adjacent_closing_bracket() {
        for path in [
            "",
            r#"["unterminated"#,
            r#"["\x41"]"#,
            r#"["\uD83D"]"#,
            r#"["\uDE00"]"#,
            "[\"line\nbreak\"]",
            r#"["missing""#,
            r#"[ "value"]"#,
            r#"["value" ]"#,
            r#"["value"0]"#,
            r#"["value"]extra"#,
        ] {
            assert!(
                CompiledPath::parse(path).is_err(),
                "路径应当被拒绝：{path:?}"
            );
        }
    }

    #[test]
    fn pattern_requires_exactly_one_text_named_capture() {
        let valid = RulesDefinition::parse(
            r#"
[[rule]]
code = 401
parameter = 0
pattern = '(prefix)?(?<text>.+)'
"#,
        )
        .expect("未命名捕获不影响 text ABI");
        let pattern = valid.rules()[0].pattern().expect("应该有 pattern");
        assert!(pattern.regex().is_match(b"prefixbody").expect("匹配应成功"));

        assert!(matches!(
            RulesDefinition::parse("[[rule]]\ncode = 401\nparameter = 0\npattern = ''"),
            Err(RulesDefinitionError::EmptyPattern { rule_number: 1 })
        ));

        for pattern in [
            " ",
            ".+",
            "(?<other>.+)",
            "(?<text>.+)(?<other>.*)",
            "(?J)(?<text>a)(?<text>b)",
        ] {
            let source = format!("[[rule]]\ncode = 401\nparameter = 0\npattern = {pattern:?}");
            assert!(matches!(
                RulesDefinition::parse(&source),
                Err(RulesDefinitionError::InvalidNamedCaptures { rule_number: 1, .. })
            ));
        }

        assert!(matches!(
            RulesDefinition::parse("[[rule]]\ncode = 401\nparameter = 0\npattern = '(?<text>'"),
            Err(RulesDefinitionError::InvalidPattern { rule_number: 1, .. })
        ));
    }

    #[test]
    fn rule_numbers_follow_source_order_for_diagnostics_only() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Actors.json"
path = '[].name'

[[rule]]
plugin = "YEP_QuestJournal"
path = '["Quest 1"].Title'

[[rule]]
code = 357
parameter = 3
path = 'dText'
"#,
        )
        .expect("三种来源应该共用同一个 rule 数组");

        assert_eq!(
            definition
                .rules()
                .iter()
                .map(RuleDefinition::rule_number)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
}
