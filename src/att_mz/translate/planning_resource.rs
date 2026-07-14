#![allow(dead_code, reason = "规划资料读取服务等待 Planner 生产装配")]

//! 外部术语与占位符 JSON 的异步读取和 CPU 解析边界。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde::Deserialize;

use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};

use super::placeholder::PlaceholderRuleDefinition;
use super::standard::TerminologyDependency;

/// Planner 一次运行实际读取到的全部外部资料。
pub(crate) struct TranslationPlanningResources {
    terminology: Option<Arc<CompiledTerminology>>,
    placeholder_rules: Vec<PlaceholderRuleDefinition>,
}

impl TranslationPlanningResources {
    pub(crate) fn new(
        terminology: Option<CompiledTerminology>,
        placeholder_rules: Vec<PlaceholderRuleDefinition>,
    ) -> Self {
        Self {
            terminology: terminology.map(Arc::new),
            placeholder_rules,
        }
    }

    /// `None` 表示本次没有提供权威术语表；`Some(empty)` 表示权威空集合。
    pub(crate) fn terminology(&self) -> Option<&Arc<CompiledTerminology>> {
        self.terminology.as_ref()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Option<Arc<CompiledTerminology>>,
        Vec<PlaceholderRuleDefinition>,
    ) {
        (self.terminology, self.placeholder_rules)
    }
}

/// 一条已经通过外部 JSON 边界校验的术语。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminologyEntry {
    term: String,
    translation: String,
    triggers: Vec<String>,
}

impl TerminologyEntry {
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
    entry_by_term: BTreeMap<String, usize>,
    matcher: Option<AhoCorasick>,
    pattern_to_entry: Vec<usize>,
}

impl CompiledTerminology {
    pub(crate) fn entries(&self) -> &[TerminologyEntry] {
        &self.entries
    }

    pub(crate) fn entry_at(&self, index: usize) -> &TerminologyEntry {
        &self.entries[index]
    }

    pub(crate) fn entry(&self, term: &str) -> Option<&TerminologyEntry> {
        self.entry_by_term
            .get(term)
            .map(|index| &self.entries[*index])
    }

    /// 返回由任意给定原文触发的术语，顺序稳定为术语文件顺序。
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
    ) -> impl Future<Output = Result<TranslationPlanningResources, Self::Error>> + Send;
}

/// 使用异步文件根与 CPU 根读取两份可选 JSON。
pub(crate) struct JsonTranslationPlanningResourceReadingService<F, C> {
    file_reader: F,
    cpu: C,
}

impl<F, C> JsonTranslationPlanningResourceReadingService<F, C> {
    pub(crate) fn new(file_reader: F, cpu: C) -> Self {
        Self { file_reader, cpu }
    }
}

impl<F, C> TranslationPlanningResourceReader for JsonTranslationPlanningResourceReadingService<F, C>
where
    F: FileReader,
    C: CpuTaskExecutor,
{
    type Error = TranslationPlanningResourceReadingError<F::Error, C::Error>;

    async fn read(
        &self,
        terminology_path: Option<PathBuf>,
        placeholder_rules_path: Option<PathBuf>,
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

        let terminology_parse =
            parse_terminology_optional::<F::Error, C>(&self.cpu, terminology_file);
        let placeholder_parse =
            parse_placeholder_optional::<F::Error, C>(&self.cpu, placeholder_file);
        let (terminology, placeholder_rules) =
            futures_util::join!(terminology_parse, placeholder_parse);

        Ok(TranslationPlanningResources::new(
            terminology?,
            placeholder_rules?,
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

async fn parse_terminology_optional<F, C: CpuTaskExecutor>(
    cpu: &C,
    file: Option<ReadFile>,
) -> Result<Option<CompiledTerminology>, TranslationPlanningResourceReadingError<F, C::Error>> {
    let Some(file) = file else {
        return Ok(None);
    };
    let path = file.resolved_path().to_owned();
    let bytes = file.into_bytes();
    let parsed = cpu
        .execute(move || parse_terminology(&bytes))
        .await
        .map_err(
            |source| TranslationPlanningResourceReadingError::ParseTerminologyCompute {
                path: path.clone(),
                source,
            },
        )?;
    parsed.map(Some).map_err(
        |source| TranslationPlanningResourceReadingError::InvalidTerminology { path, source },
    )
}

async fn parse_placeholder_optional<F, C: CpuTaskExecutor>(
    cpu: &C,
    file: Option<ReadFile>,
) -> Result<Vec<PlaceholderRuleDefinition>, TranslationPlanningResourceReadingError<F, C::Error>> {
    let Some(file) = file else {
        return Ok(Vec::new());
    };
    let path = file.resolved_path().to_owned();
    let bytes = file.into_bytes();
    let parsed = cpu
        .execute(move || serde_json::from_slice::<Vec<PlaceholderRuleDefinition>>(&bytes))
        .await
        .map_err(|source| {
            TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
                path: path.clone(),
                source,
            }
        })?;
    parsed.map_err(
        |source| TranslationPlanningResourceReadingError::InvalidPlaceholderRules { path, source },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTerminologyEntry {
    term: String,
    translation: String,
    triggers: Vec<String>,
}

fn parse_terminology(bytes: &[u8]) -> Result<CompiledTerminology, TerminologyDefinitionError> {
    let raw: Vec<RawTerminologyEntry> =
        serde_json::from_slice(bytes).map_err(TerminologyDefinitionError::InvalidJson)?;
    let mut entries = Vec::with_capacity(raw.len());
    let mut entry_by_term = BTreeMap::new();
    let mut all_triggers = BTreeSet::new();
    let mut patterns = Vec::new();
    let mut pattern_to_entry = Vec::new();

    for (index, raw_entry) in raw.into_iter().enumerate() {
        let entry_number = index + 1;
        validate_term_string("term", &raw_entry.term, entry_number)?;
        validate_term_string("translation", &raw_entry.translation, entry_number)?;
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
            validate_term_string("trigger", trigger, entry_number)?;
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
        entry_by_term,
        matcher,
        pattern_to_entry,
    })
}

fn validate_term_string(
    field: &'static str,
    value: &str,
    entry_number: usize,
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
        path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    InvalidTerminology {
        path: PathBuf,
        source: TerminologyDefinitionError,
    },
    ParsePlaceholderRulesCompute {
        path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    InvalidPlaceholderRules {
        path: PathBuf,
        source: serde_json::Error,
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
                write!(formatter, "无法调度术语解析 {}：{source}", path.display())
            }
            Self::InvalidTerminology { path, source } => {
                write!(formatter, "术语文件无效 {}：{source}", path.display())
            }
            Self::ParsePlaceholderRulesCompute { path, source } => write!(
                formatter,
                "无法调度占位符规则解析 {}：{source}",
                path.display()
            ),
            Self::InvalidPlaceholderRules { path, source } => write!(
                formatter,
                "占位符规则 JSON 无效 {}：{source}",
                path.display()
            ),
        }
    }
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

impl<F, C> TranslationPlanningResourceReadingError<F, C> {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::ReadTerminology { path, .. }
            | Self::ReadPlaceholderRules { path, .. }
            | Self::ParseTerminologyCompute { path, .. }
            | Self::InvalidTerminology { path, .. }
            | Self::ParsePlaceholderRulesCompute { path, .. }
            | Self::InvalidPlaceholderRules { path, .. } => path,
        }
    }
}

#[derive(Debug)]
pub(crate) enum TerminologyDefinitionError {
    InvalidJson(serde_json::Error),
    BlankField {
        entry_number: usize,
        field: &'static str,
    },
    SurroundingWhitespace {
        entry_number: usize,
        field: &'static str,
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
            Self::InvalidJson(source) => write!(formatter, "JSON 解析失败：{source}"),
            Self::BlankField {
                entry_number,
                field,
            } => write!(formatter, "术语 {entry_number} 的 {field} 不能为空白"),
            Self::SurroundingWhitespace {
                entry_number,
                field,
            } => write!(formatter, "术语 {entry_number} 的 {field} 含首尾空白"),
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
            Self::InvalidJson(source) => Some(source),
            Self::CompileMatcher(source) => Some(source),
            _ => None,
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
            let bytes = if path.ends_with("terms.json") {
                r#"[{"term":"魔法剣","translation":"魔法剑","triggers":["魔法剣"]}]"#
                    .as_bytes()
                    .to_vec()
            } else {
                br#"[{"scopes":["event_dialogue"],"pattern":"\\\\SE\\[[^]]+\\]","label":"SOUND_EFFECT"}]"#.to_vec()
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ReadFile::new(path, bytes))
        }
    }

    #[test]
    fn terminology_keeps_overlap_and_file_order() {
        let compiled = parse_terminology(
            r#"[
                {"term":"魔法剣","translation":"魔法剑","triggers":["魔法剣","魔剣"]},
                {"term":"剣","translation":"剑","triggers":["剣"]}
            ]"#
            .as_bytes(),
        )
        .expect("术语应该有效");

        let matched = compiled.triggered_by(["伝説の魔法剣"]);
        assert_eq!(
            matched.iter().map(|entry| entry.term()).collect::<Vec<_>>(),
            vec!["魔法剣", "剣"]
        );
    }

    #[test]
    fn authoritative_empty_terminology_is_distinct_from_missing_file() {
        let compiled = parse_terminology(b"[]").expect("空数组是权威空术语表");
        assert!(compiled.entries().is_empty());
        assert!(compiled.triggered_by(["任何文本"]).is_empty());
    }

    #[test]
    fn duplicate_trigger_and_unknown_fields_fail() {
        let duplicate = parse_terminology(
            r#"[
                {"term":"A","translation":"甲","triggers":["x"]},
                {"term":"B","translation":"乙","triggers":["x"]}
            ]"#
            .as_bytes(),
        )
        .expect_err("触发词必须全局唯一");
        assert!(matches!(
            duplicate,
            TerminologyDefinitionError::DuplicateTrigger { .. }
        ));

        assert!(matches!(
            parse_terminology(
                r#"[{"term":"A","translation":"甲","triggers":["A"],"id":1}]"#.as_bytes()
            ),
            Err(TerminologyDefinitionError::InvalidJson(_))
        ));
    }

    #[test]
    fn ten_thousand_literal_triggers_share_one_compiled_matcher() {
        let definitions = (0..10_000)
            .map(|index| {
                serde_json::json!({
                    "term": format!("term-{index:05}"),
                    "translation": format!("译词-{index:05}"),
                    "triggers": [format!("trigger-{index:05}")],
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&definitions).expect("大型术语 fixture 应可编码");

        let compiled = parse_terminology(&bytes).expect("一万条字面 trigger 应可一次编译");
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
        let service = JsonTranslationPlanningResourceReadingService::new(
            YieldingFileReader {
                active: Arc::clone(&active),
                max_active: Arc::clone(&max_active),
            },
            ImmediateCpu,
        );

        let resources = service
            .read(
                Some(PathBuf::from("C:/input/terms.json")),
                Some(PathBuf::from("C:/input/placeholders.json")),
            )
            .await
            .expect("两份外部资料应该并发读取并分别解析");

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            resources
                .terminology()
                .expect("应保留权威术语表")
                .entries()
                .len(),
            1
        );
        assert_eq!(resources.placeholder_rules.len(), 1);
    }
}
