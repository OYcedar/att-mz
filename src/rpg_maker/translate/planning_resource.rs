//! 外部术语与占位符 TOML 的异步读取和 CPU 解析边界。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde::{Deserialize, Serialize};

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};

use super::placeholder::PlaceholderRuleDefinition;
use super::standard::TerminologyDependency;

/// Planner 一次运行实际读取到的全部外部资料。
pub(crate) struct TranslationPlanningResources {
    terminology: Arc<CompiledTerminology>,
    placeholder_rules: Vec<PlaceholderRuleDefinition>,
    terminology_json: String,
    placeholder_rules_json: String,
}

#[cfg(test)]
mod documentation_contract_tests {
    use crate::rpg_maker::documentation_test::{ClassifiedExampleKind, classified_toml_fences};
    use crate::rpg_maker::translate::placeholder::Pcre2PlaceholderService;

    use super::{compile_terminology, parse_placeholder_toml, parse_terminology_toml};

    const PLACEHOLDER_EXAMPLE: &[u8] =
        include_bytes!("../../../docs/rpg-maker/examples/placeholders.toml");
    const TERMINOLOGY_EXAMPLE: &[u8] =
        include_bytes!("../../../docs/rpg-maker/examples/terminology.toml");
    const RULES_GUIDE: &str = include_str!("../../../docs/rpg-maker/rules.md");
    const TERMINOLOGY_GUIDE: &str = include_str!("../../../docs/rpg-maker/terminology.md");

    #[test]
    fn documented_placeholder_rules_use_the_production_parser_and_compiler() {
        let definitions = parse_placeholder_toml(PLACEHOLDER_EXAMPLE)
            .expect("文档中的 Placeholder Rules 必须通过生产解析边界");
        assert!(!definitions.is_empty(), "完整示例必须至少声明一条规则");
        Pcre2PlaceholderService::new()
            .expect("Builtin Placeholder PCRE2 应可建立")
            .compile_custom(definitions)
            .expect("文档中的 Placeholder Rules 必须通过生产 PCRE2 编译边界");
    }

    #[test]
    fn documented_terminology_uses_the_production_parser_and_compiler() {
        let entries = parse_terminology_toml(TERMINOLOGY_EXAMPLE)
            .expect("文档中的 Terminology 必须通过生产解析边界");
        assert!(!entries.is_empty(), "完整示例必须至少声明一个术语");
        compile_terminology(entries).expect("文档中的 Terminology 必须通过生产编译边界");
    }

    #[test]
    fn classified_placeholder_fences_follow_the_production_contract() {
        let service = Pcre2PlaceholderService::new().expect("Builtin Placeholder PCRE2 应可建立");
        let mut valid = 0;
        let mut invalid = 0;
        for fence in classified_toml_fences(RULES_GUIDE) {
            let common_root = fence.section().starts_with("2.") && fence.subsection().is_none();
            let common_pcre2_example = fence
                .subsection()
                .is_some_and(|heading| heading.starts_with("2.1 "));
            let placeholder_section = fence.section().starts_with("6.");
            if (!common_root && !common_pcre2_example && !placeholder_section)
                || fence.kind() == ClassifiedExampleKind::Illustrative
            {
                continue;
            }
            let result = parse_placeholder_toml(fence.body().as_bytes())
                .map_err(|error| error.to_string())
                .and_then(|definitions| {
                    service
                        .compile_custom(definitions)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                });
            match fence.kind() {
                ClassifiedExampleKind::Valid => {
                    valid += 1;
                    result.unwrap_or_else(|error| {
                        panic!(
                            "rules.md:{} 的 Placeholder valid TOML 未通过生产边界：{error}",
                            fence.opening_line()
                        )
                    });
                }
                ClassifiedExampleKind::Invalid => {
                    invalid += 1;
                    assert!(
                        result.is_err(),
                        "rules.md:{} 的 Placeholder invalid TOML 被生产边界接受",
                        fence.opening_line()
                    );
                }
                ClassifiedExampleKind::Illustrative => unreachable!(),
            }
        }
        assert!(
            valid > 0 && invalid > 0,
            "共同根、2.1 PCRE2 与 Placeholder 章节必须覆盖生产样例"
        );
    }

    #[test]
    fn classified_terminology_fences_follow_the_production_contract() {
        let mut valid = 0;
        let mut invalid = 0;
        for fence in classified_toml_fences(TERMINOLOGY_GUIDE) {
            if fence.kind() == ClassifiedExampleKind::Illustrative {
                continue;
            }
            let result = parse_terminology_toml(fence.body().as_bytes())
                .and_then(|entries| compile_terminology(entries).map(|_| ()));
            match fence.kind() {
                ClassifiedExampleKind::Valid => {
                    valid += 1;
                    result.unwrap_or_else(|error| {
                        panic!(
                            "terminology.md:{} 的 valid TOML 未通过生产边界：{error}",
                            fence.opening_line()
                        )
                    });
                }
                ClassifiedExampleKind::Invalid => {
                    invalid += 1;
                    assert!(
                        result.is_err(),
                        "terminology.md:{} 的 invalid TOML 被生产边界接受",
                        fence.opening_line()
                    );
                }
                ClassifiedExampleKind::Illustrative => unreachable!(),
            }
        }
        assert!(
            valid > 0 && invalid > 0,
            "Terminology 文档必须覆盖生产正反样例"
        );
    }
}

impl TranslationPlanningResources {
    pub(crate) fn new(
        terminology: CompiledTerminology,
        placeholder_rules: Vec<PlaceholderRuleDefinition>,
        terminology_json: String,
        placeholder_rules_json: String,
    ) -> Self {
        Self {
            terminology: Arc::new(terminology),
            placeholder_rules,
            terminology_json,
            placeholder_rules_json,
        }
    }

    #[cfg(test)]
    pub(crate) fn terminology(&self) -> &Arc<CompiledTerminology> {
        &self.terminology
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<CompiledTerminology>,
        Vec<PlaceholderRuleDefinition>,
        String,
        String,
    ) {
        (
            self.terminology,
            self.placeholder_rules,
            self.terminology_json,
            self.placeholder_rules_json,
        )
    }
}

/// 一条已经通过外部 TOML 边界校验的术语。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminologyEntry {
    term: String,
    translation: String,
    triggers: Vec<String>,
}

impl TerminologyEntry {
    #[cfg(test)]
    pub(crate) fn new(
        term: impl Into<String>,
        translation: impl Into<String>,
        triggers: Vec<String>,
    ) -> Self {
        Self {
            term: term.into(),
            translation: translation.into(),
            triggers,
        }
    }

    pub(crate) fn term(&self) -> &str {
        &self.term
    }

    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }

    pub(crate) fn dependency(&self) -> TerminologyDependency {
        TerminologyDependency::new(self.term.clone(), self.translation.clone())
    }
}

/// 以 Aho-Corasick 一次扫描全部 trigger 的权威术语集合。
pub(crate) struct CompiledTerminology {
    entries: Vec<TerminologyEntry>,
    matcher: Option<AhoCorasick>,
    pattern_to_entry: Vec<usize>,
}

impl CompiledTerminology {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
            matcher: None,
            pattern_to_entry: Vec::new(),
        }
    }

    pub(crate) fn entries(&self) -> &[TerminologyEntry] {
        &self.entries
    }

    /// 返回由任意给定原文触发的术语，顺序稳定为术语文件顺序。
    #[cfg(test)]
    pub(crate) fn triggered_by<'a>(
        &'a self,
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Vec<&'a TerminologyEntry> {
        let Some(matcher) = &self.matcher else {
            return Vec::new();
        };
        let mut matched = vec![false; self.entries.len()];
        for text in texts {
            for found in matcher.find_overlapping_iter(text) {
                matched[self.pattern_to_entry[found.pattern().as_usize()]] = true;
            }
        }
        self.entries
            .iter()
            .zip(matched)
            .filter_map(|(entry, matched)| matched.then_some(entry))
            .collect()
    }

    pub(crate) fn triggered_indices<'t>(
        &self,
        texts: impl IntoIterator<Item = &'t str>,
    ) -> Vec<usize> {
        let Some(matcher) = &self.matcher else {
            return Vec::new();
        };
        let mut matched = vec![false; self.entries.len()];
        for text in texts {
            for found in matcher.find_overlapping_iter(text) {
                matched[self.pattern_to_entry[found.pattern().as_usize()]] = true;
            }
        }
        matched
            .into_iter()
            .enumerate()
            .filter_map(|(index, matched)| matched.then_some(index))
            .collect()
    }
}

impl fmt::Debug for CompiledTerminology {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledTerminology")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

/// Planner 从外部路径取得受信规划资料的直接契约。
pub(crate) trait TranslationPlanningResourceReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        terminology_path: Option<PathBuf>,
        placeholder_rules_path: Option<PathBuf>,
        current_terminology_json: String,
        current_placeholder_rules_json: String,
    ) -> impl Future<Output = Result<TranslationPlanningResources, Self::Error>> + Send;
}

/// 使用异步文件根与 CPU 根读取两份可选 TOML。
pub(crate) struct TranslationPlanningResourceReadingService<F, C> {
    file_reader: F,
    cpu: C,
}

impl<F, C> TranslationPlanningResourceReadingService<F, C> {
    pub(crate) fn new(file_reader: F, cpu: C) -> Self {
        Self { file_reader, cpu }
    }
}

impl<F, C> TranslationPlanningResourceReader for TranslationPlanningResourceReadingService<F, C>
where
    F: FileReader,
    C: CpuTaskExecutor,
{
    type Error = TranslationPlanningResourceReadingError<F::Error, C::Error>;

    async fn read(
        &self,
        terminology_path: Option<PathBuf>,
        placeholder_rules_path: Option<PathBuf>,
        current_terminology_json: String,
        current_placeholder_rules_json: String,
    ) -> Result<TranslationPlanningResources, Self::Error> {
        let terminology_request_path = terminology_path.clone();
        let placeholder_request_path = placeholder_rules_path.clone();
        let (terminology_file, placeholder_file) = futures_util::join!(
            read_optional(&self.file_reader, terminology_path),
            read_optional(&self.file_reader, placeholder_rules_path),
        );
        let terminology_file = terminology_file.map_err(|source| {
            TranslationPlanningResourceReadingError::ReadTerminology {
                path: terminology_request_path.expect("读取错误只可能来自 Some 路径"),
                source,
            }
        })?;
        let placeholder_file = placeholder_file.map_err(|source| {
            TranslationPlanningResourceReadingError::ReadPlaceholderRules {
                path: placeholder_request_path.expect("读取错误只可能来自 Some 路径"),
                source,
            }
        })?;

        let terminology_parse = parse_terminology_resource::<F::Error, C>(
            &self.cpu,
            terminology_file,
            current_terminology_json,
        );
        let placeholder_parse = parse_placeholder_resource::<F::Error, C>(
            &self.cpu,
            placeholder_file,
            current_placeholder_rules_json,
        );
        let (terminology, placeholder_rules) =
            futures_util::join!(terminology_parse, placeholder_parse);
        let (terminology, terminology_json) = terminology?;
        let (placeholder_rules, placeholder_rules_json) = placeholder_rules?;

        Ok(TranslationPlanningResources::new(
            terminology,
            placeholder_rules,
            terminology_json,
            placeholder_rules_json,
        ))
    }
}

async fn read_optional<F: FileReader>(
    reader: &F,
    path: Option<PathBuf>,
) -> Result<Option<ReadFile>, ReadFileError<F::Error>> {
    match path {
        Some(path) => reader.read_file(path).await.map(Some),
        None => Ok(None),
    }
}

async fn parse_terminology_resource<F, C: CpuTaskExecutor>(
    cpu: &C,
    file: Option<ReadFile>,
    current_json: String,
) -> Result<(CompiledTerminology, String), TranslationPlanningResourceReadingError<F, C::Error>> {
    let (path, input) = file.map_or_else(
        || (None, PlanningResourceInput::Snapshot(current_json)),
        |file| {
            (
                Some(file.resolved_path().to_owned()),
                PlanningResourceInput::External(file.into_bytes()),
            )
        },
    );
    let error_path = path.clone();
    let parsed = cpu
        .execute(move || {
            let entries = match input {
                PlanningResourceInput::External(bytes) => parse_terminology_toml(&bytes)?,
                PlanningResourceInput::Snapshot(json) => parse_terminology_snapshot(&json)?,
            };
            let canonical = serde_json::to_string(&entries)
                .map_err(TerminologyDefinitionError::EncodeSnapshot)?;
            let terminology = compile_terminology(entries)?;
            Ok::<_, TerminologyDefinitionError>((terminology, canonical))
        })
        .await
        .map_err(
            |source| TranslationPlanningResourceReadingError::ParseTerminologyCompute {
                path: error_path.clone(),
                source,
            },
        )?;
    parsed.map_err(
        |source| TranslationPlanningResourceReadingError::InvalidTerminology { path, source },
    )
}

async fn parse_placeholder_resource<F, C: CpuTaskExecutor>(
    cpu: &C,
    file: Option<ReadFile>,
    current_json: String,
) -> Result<
    (Vec<PlaceholderRuleDefinition>, String),
    TranslationPlanningResourceReadingError<F, C::Error>,
> {
    let (path, input) = file.map_or_else(
        || (None, PlanningResourceInput::Snapshot(current_json)),
        |file| {
            (
                Some(file.resolved_path().to_owned()),
                PlanningResourceInput::External(file.into_bytes()),
            )
        },
    );
    let error_path = path.clone();
    let parsed = cpu
        .execute(move || {
            let definitions = match input {
                PlanningResourceInput::External(bytes) => parse_placeholder_toml(&bytes)?,
                PlanningResourceInput::Snapshot(json) => serde_json::from_str(&json)
                    .map_err(PlaceholderDefinitionError::InvalidSnapshot)?,
            };
            let canonical = serde_json::to_string(&definitions)
                .map_err(PlaceholderDefinitionError::EncodeSnapshot)?;
            Ok::<_, PlaceholderDefinitionError>((definitions, canonical))
        })
        .await
        .map_err(|source| {
            TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
                path: error_path.clone(),
                source,
            }
        })?;
    parsed.map_err(
        |source| TranslationPlanningResourceReadingError::InvalidPlaceholderRules { path, source },
    )
}

enum PlanningResourceInput {
    External(Vec<u8>),
    Snapshot(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminologyToml {
    term: Vec<ExternalTerminologyEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalTerminologyEntry {
    term: String,
    translation: String,
    triggers: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceholderToml {
    rule: Vec<PlaceholderRuleDefinition>,
}

fn parse_terminology_toml(
    bytes: &[u8],
) -> Result<Vec<TerminologyEntry>, TerminologyDefinitionError> {
    let source = std::str::from_utf8(bytes).map_err(TerminologyDefinitionError::InvalidUtf8)?;
    let definition: TerminologyToml =
        toml::from_str(source).map_err(TerminologyDefinitionError::InvalidToml)?;
    Ok(definition
        .term
        .into_iter()
        .map(|entry| {
            let triggers = entry.triggers.unwrap_or_else(|| vec![entry.term.clone()]);
            TerminologyEntry {
                term: entry.term,
                translation: entry.translation,
                triggers,
            }
        })
        .collect())
}

fn parse_terminology_snapshot(
    json: &str,
) -> Result<Vec<TerminologyEntry>, TerminologyDefinitionError> {
    serde_json::from_str(json).map_err(TerminologyDefinitionError::InvalidSnapshot)
}

fn parse_placeholder_toml(
    bytes: &[u8],
) -> Result<Vec<PlaceholderRuleDefinition>, PlaceholderDefinitionError> {
    let source = std::str::from_utf8(bytes).map_err(PlaceholderDefinitionError::InvalidUtf8)?;
    let definition: PlaceholderToml =
        toml::from_str(source).map_err(PlaceholderDefinitionError::InvalidToml)?;
    Ok(definition.rule)
}

pub(super) fn compile_terminology(
    raw: Vec<TerminologyEntry>,
) -> Result<CompiledTerminology, TerminologyDefinitionError> {
    let mut entries = Vec::with_capacity(raw.len());
    let mut entry_by_term = BTreeMap::new();
    let mut all_triggers = BTreeSet::new();
    let mut patterns = Vec::new();
    let mut pattern_to_entry = Vec::new();

    for (index, raw_entry) in raw.into_iter().enumerate() {
        let entry_number = index + 1;
        validate_term_string("term", &raw_entry.term, entry_number, false)?;
        validate_term_string("translation", &raw_entry.translation, entry_number, false)?;
        if raw_entry.triggers.is_empty() {
            return Err(TerminologyDefinitionError::EmptyTriggers { entry_number });
        }
        if entry_by_term
            .insert(raw_entry.term.clone(), index)
            .is_some()
        {
            return Err(TerminologyDefinitionError::DuplicateTerm {
                term: raw_entry.term,
            });
        }
        let mut local_triggers = BTreeSet::new();
        for trigger in &raw_entry.triggers {
            validate_term_string("trigger", trigger, entry_number, true)?;
            if !local_triggers.insert(trigger.clone()) || !all_triggers.insert(trigger.clone()) {
                return Err(TerminologyDefinitionError::DuplicateTrigger {
                    trigger: trigger.clone(),
                });
            }
            patterns.push(trigger.clone());
            pattern_to_entry.push(index);
        }
        entries.push(TerminologyEntry {
            term: raw_entry.term,
            translation: raw_entry.translation,
            triggers: raw_entry.triggers,
        });
    }

    let matcher = if patterns.is_empty() {
        None
    } else {
        Some(
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .build(patterns)
                .map_err(TerminologyDefinitionError::CompileMatcher)?,
        )
    };

    Ok(CompiledTerminology {
        entries,
        matcher,
        pattern_to_entry,
    })
}

fn validate_term_string(
    field: &'static str,
    value: &str,
    entry_number: usize,
    allow_line_feed: bool,
) -> Result<(), TerminologyDefinitionError> {
    if value.trim().is_empty() {
        return Err(TerminologyDefinitionError::BlankField {
            entry_number,
            field,
        });
    }
    if value.trim() != value {
        return Err(TerminologyDefinitionError::SurroundingWhitespace {
            entry_number,
            field,
        });
    }
    if let Some(character) = value
        .chars()
        .find(|character| character.is_control() && (!allow_line_feed || *character != '\n'))
    {
        return Err(TerminologyDefinitionError::ControlCharacter {
            entry_number,
            field,
            character,
        });
    }
    Ok(())
}

/// 外部规划资料在读取或 CPU 解析阶段的错误。
#[derive(Debug)]
pub(crate) enum TranslationPlanningResourceReadingError<F, C> {
    ReadTerminology {
        path: PathBuf,
        source: ReadFileError<F>,
    },
    ReadPlaceholderRules {
        path: PathBuf,
        source: ReadFileError<F>,
    },
    ParseTerminologyCompute {
        path: Option<PathBuf>,
        source: CpuTaskExecutionError<C>,
    },
    InvalidTerminology {
        path: Option<PathBuf>,
        source: TerminologyDefinitionError,
    },
    ParsePlaceholderRulesCompute {
        path: Option<PathBuf>,
        source: CpuTaskExecutionError<C>,
    },
    InvalidPlaceholderRules {
        path: Option<PathBuf>,
        source: PlaceholderDefinitionError,
    },
}

impl<F, C> fmt::Display for TranslationPlanningResourceReadingError<F, C>
where
    F: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadTerminology { path, source } => {
                write!(formatter, "无法读取术语文件 {}：{source}", path.display())
            }
            Self::ReadPlaceholderRules { path, source } => write!(
                formatter,
                "无法读取占位符规则文件 {}：{source}",
                path.display()
            ),
            Self::ParseTerminologyCompute { path, source } => {
                write!(
                    formatter,
                    "无法调度术语解析 {}：{source}",
                    resource_label(path)
                )
            }
            Self::InvalidTerminology { path, source } => {
                write!(formatter, "术语资源无效 {}：{source}", resource_label(path))
            }
            Self::ParsePlaceholderRulesCompute { path, source } => write!(
                formatter,
                "无法调度占位符规则解析 {}：{source}",
                resource_label(path)
            ),
            Self::InvalidPlaceholderRules { path, source } => write!(
                formatter,
                "占位符规则资源无效 {}：{source}",
                resource_label(path)
            ),
        }
    }
}

fn resource_label(path: &Option<PathBuf>) -> String {
    path.as_ref().map_or_else(
        || "（项目当前快照）".to_owned(),
        |path| path.display().to_string(),
    )
}

impl<F, C> Error for TranslationPlanningResourceReadingError<F, C>
where
    F: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadTerminology { source, .. } => Some(source),
            Self::ReadPlaceholderRules { source, .. } => Some(source),
            Self::ParseTerminologyCompute { source, .. } => Some(source),
            Self::InvalidTerminology { source, .. } => Some(source),
            Self::ParsePlaceholderRulesCompute { source, .. } => Some(source),
            Self::InvalidPlaceholderRules { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum TerminologyDefinitionError {
    InvalidUtf8(std::str::Utf8Error),
    InvalidToml(toml::de::Error),
    InvalidSnapshot(serde_json::Error),
    EncodeSnapshot(serde_json::Error),
    BlankField {
        entry_number: usize,
        field: &'static str,
    },
    SurroundingWhitespace {
        entry_number: usize,
        field: &'static str,
    },
    ControlCharacter {
        entry_number: usize,
        field: &'static str,
        character: char,
    },
    EmptyTriggers {
        entry_number: usize,
    },
    DuplicateTerm {
        term: String,
    },
    DuplicateTrigger {
        trigger: String,
    },
    CompileMatcher(aho_corasick::BuildError),
}

impl fmt::Display for TerminologyDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(source) => write!(formatter, "TOML 不是 UTF-8：{source}"),
            Self::InvalidToml(source) => write!(formatter, "TOML 解析失败：{source}"),
            Self::InvalidSnapshot(source) => {
                write!(formatter, "项目内部术语快照无效：{source}")
            }
            Self::EncodeSnapshot(source) => {
                write!(formatter, "无法编码项目内部术语快照：{source}")
            }
            Self::BlankField {
                entry_number,
                field,
            } => write!(formatter, "术语 {entry_number} 的 {field} 不能为空白"),
            Self::SurroundingWhitespace {
                entry_number,
                field,
            } => write!(formatter, "术语 {entry_number} 的 {field} 含首尾空白"),
            Self::ControlCharacter {
                entry_number,
                field,
                character,
            } => write!(
                formatter,
                "术语 {entry_number} 的 {field} 含不允许的控制字符 U+{:04X}",
                u32::from(*character)
            ),
            Self::EmptyTriggers { entry_number } => {
                write!(formatter, "术语 {entry_number} 的 triggers 为空")
            }
            Self::DuplicateTerm { term } => write!(formatter, "术语重复：{term:?}"),
            Self::DuplicateTrigger { trigger } => write!(formatter, "触发词重复：{trigger:?}"),
            Self::CompileMatcher(source) => write!(formatter, "无法编译术语匹配器：{source}"),
        }
    }
}

impl Error for TerminologyDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(source) => Some(source),
            Self::InvalidToml(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::EncodeSnapshot(source) => Some(source),
            Self::CompileMatcher(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PlaceholderDefinitionError {
    InvalidUtf8(std::str::Utf8Error),
    InvalidToml(toml::de::Error),
    InvalidSnapshot(serde_json::Error),
    EncodeSnapshot(serde_json::Error),
}

impl fmt::Display for PlaceholderDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(source) => write!(formatter, "TOML 不是 UTF-8：{source}"),
            Self::InvalidToml(source) => write!(formatter, "TOML 解析失败：{source}"),
            Self::InvalidSnapshot(source) => {
                write!(formatter, "项目内部占位符快照无效：{source}")
            }
            Self::EncodeSnapshot(source) => {
                write!(formatter, "无法编码项目内部占位符快照：{source}")
            }
        }
    }
}

impl Error for PlaceholderDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(source) => Some(source),
            Self::InvalidToml(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::EncodeSnapshot(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake failure")
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
    struct ImmediateCpu;

    impl CpuTaskExecutor for ImmediateCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[derive(Clone)]
    struct YieldingFileReader {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl FileReader for YieldingFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let bytes = if path.ends_with("terms.toml") {
                r#"
                    [[term]]
                    term = "魔法剣"
                    translation = "魔法剑"
                "#
                .as_bytes()
                .to_vec()
            } else {
                br#"
                    [[rule]]
                    scopes = ["event_dialogue"]
                    pattern = '\\SE\[[^]]+\]'
                "#
                .to_vec()
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ReadFile::new(path, bytes))
        }
    }

    #[test]
    fn terminology_keeps_overlap_and_file_order() {
        let entries = parse_terminology_toml(
            r#"
                [[term]]
                term = "魔法剣"
                translation = "魔法剑"
                triggers = ["魔法剣", "魔剣"]

                [[term]]
                term = "剣"
                translation = "剑"
                triggers = ["剣"]
            "#
            .as_bytes(),
        )
        .expect("TOML 术语应该有效");
        let compiled = compile_terminology(entries).expect("术语应该有效");

        let matched = compiled.triggered_by(["伝説の魔法剣"]);
        assert_eq!(
            matched.iter().map(|entry| entry.term()).collect::<Vec<_>>(),
            vec!["魔法剣", "剣"]
        );
    }

    #[test]
    fn omitted_triggers_default_to_term_and_explicit_empty_is_invalid() {
        let entries = parse_terminology_toml(
            r#"
                [[term]]
                term = "魔法剣"
                translation = "魔法剑"
            "#
            .as_bytes(),
        )
        .expect("缺省 triggers 应该有效");
        assert_eq!(entries[0].triggers, ["魔法剣"]);

        let entries = parse_terminology_toml(
            r#"
                [[term]]
                term = "魔法剣"
                translation = "魔法剑"
                triggers = []
            "#
            .as_bytes(),
        )
        .expect("TOML 结构应可解析");
        assert!(matches!(
            compile_terminology(entries),
            Err(TerminologyDefinitionError::EmptyTriggers { .. })
        ));
    }

    #[test]
    fn authoritative_empty_terminology_is_explicit() {
        let entries = parse_terminology_toml(b"term = []").expect("显式空集合应该有效");
        let compiled = compile_terminology(entries).expect("空数组是权威空术语表");
        assert!(compiled.entries().is_empty());
        assert!(compiled.triggered_by(["任何文本"]).is_empty());

        for invalid in ["", "# only comments"] {
            assert!(
                parse_terminology_toml(invalid.as_bytes()).is_err(),
                "未显式写 term 的文件必须失败"
            );
        }
    }

    #[test]
    fn duplicate_trigger_and_unknown_fields_fail() {
        let entries = parse_terminology_toml(
            r#"
                [[term]]
                term = "A"
                translation = "甲"
                triggers = ["x"]
                [[term]]
                term = "B"
                translation = "乙"
                triggers = ["x"]
            "#
            .as_bytes(),
        )
        .expect("TOML 结构应可解析");
        let duplicate = compile_terminology(entries).expect_err("触发词必须全局唯一");
        assert!(matches!(
            duplicate,
            TerminologyDefinitionError::DuplicateTrigger { .. }
        ));

        let duplicate_term = compile_terminology(vec![
            TerminologyEntry {
                term: "A".to_owned(),
                translation: "甲".to_owned(),
                triggers: vec!["x".to_owned()],
            },
            TerminologyEntry {
                term: "A".to_owned(),
                translation: "乙".to_owned(),
                triggers: vec!["y".to_owned()],
            },
        ])
        .expect_err("术语必须唯一");
        assert!(matches!(
            duplicate_term,
            TerminologyDefinitionError::DuplicateTerm { .. }
        ));

        assert!(matches!(
            compile_terminology(vec![TerminologyEntry {
                term: " A".to_owned(),
                translation: "甲".to_owned(),
                triggers: vec!["A".to_owned()],
            }]),
            Err(TerminologyDefinitionError::SurroundingWhitespace { .. })
        ));

        assert!(matches!(
            parse_terminology_toml(
                r#"
                    [[term]]
                    term = "A"
                    translation = "甲"
                    id = 1
                "#
                .as_bytes()
            ),
            Err(TerminologyDefinitionError::InvalidToml(_))
        ));
    }

    #[test]
    fn terminology_control_character_contract_distinguishes_values_from_triggers() {
        for (field, value) in [("term", "A\nB"), ("translation", "甲\t乙")] {
            let definition = TerminologyEntry {
                term: if field == "term" { value } else { "A" }.to_owned(),
                translation: if field == "translation" { value } else { "甲" }.to_owned(),
                triggers: vec!["A".to_owned()],
            };
            assert!(matches!(
                compile_terminology(vec![definition]),
                Err(TerminologyDefinitionError::ControlCharacter {
                    field: actual,
                    ..
                }) if actual == field
            ));
        }

        let with_line_feed = compile_terminology(vec![TerminologyEntry {
            term: "A".to_owned(),
            translation: "甲".to_owned(),
            triggers: vec!["前\n後".to_owned()],
        }])
        .expect("trigger 应允许内部 LF");
        assert_eq!(with_line_feed.triggered_by(["前\n後"])[0].term(), "A");

        for invalid in ["A\rB", "A\0B", "A\u{0085}B"] {
            assert!(matches!(
                compile_terminology(vec![TerminologyEntry {
                    term: "A".to_owned(),
                    translation: "甲".to_owned(),
                    triggers: vec![invalid.to_owned()],
                }]),
                Err(TerminologyDefinitionError::ControlCharacter {
                    field: "trigger",
                    ..
                })
            ));
        }
    }

    #[test]
    fn placeholder_toml_requires_explicit_root_and_rejects_unknown_or_duplicate_fields() {
        assert!(parse_placeholder_toml(b"rule = []").is_ok());
        for invalid in [
            "",
            "# only comments",
            "[[rule]]\npattern = 'x'\nextra = true",
            "[[rule]]\npattern = 'x'\npattern = 'y'",
        ] {
            assert!(parse_placeholder_toml(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn ten_thousand_literal_triggers_share_one_compiled_matcher() {
        let definitions = (0..10_000)
            .map(|index| TerminologyEntry {
                term: format!("term-{index:05}"),
                translation: format!("译词-{index:05}"),
                triggers: vec![format!("trigger-{index:05}")],
            })
            .collect::<Vec<_>>();

        let compiled = compile_terminology(definitions).expect("一万条字面 trigger 应可一次编译");
        let matched = compiled.triggered_by(["前文 trigger-09999 中段 trigger-00007 后文"]);

        assert_eq!(compiled.entries().len(), 10_000);
        assert_eq!(
            matched.iter().map(|entry| entry.term()).collect::<Vec<_>>(),
            ["term-00007", "term-09999"]
        );
    }

    #[tokio::test]
    async fn two_optional_files_are_read_concurrently_before_cpu_parsing() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let service = TranslationPlanningResourceReadingService::new(
            YieldingFileReader {
                active: Arc::clone(&active),
                max_active: Arc::clone(&max_active),
            },
            ImmediateCpu,
        );

        let resources = service
            .read(
                Some(PathBuf::from("C:/input/terms.toml")),
                Some(PathBuf::from("C:/input/placeholders.toml")),
                "[]".to_owned(),
                "[]".to_owned(),
            )
            .await
            .expect("两份外部资料应该并发读取并分别解析");

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(resources.terminology().entries().len(), 1);
        assert_eq!(resources.placeholder_rules.len(), 1);
        assert_eq!(
            resources.terminology_json,
            r#"[{"term":"魔法剣","translation":"魔法剑","triggers":["魔法剣"]}]"#
        );
        assert_eq!(
            resources.placeholder_rules_json,
            r#"[{"scopes":["event_dialogue"],"pattern":"\\\\SE\\[[^]]+\\]"}]"#
        );
    }
}
