//! 外部术语与占位符 JSON 的异步读取和 CPU 解析边界。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
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
    let (path, bytes) = file.map_or_else(
        || (None, current_json.into_bytes()),
        |file| (Some(file.resolved_path().to_owned()), file.into_bytes()),
    );
    let error_path = path.clone();
    let parsed = cpu
        .execute(move || {
            let canonical =
                canonicalize_json(&bytes).map_err(TerminologyDefinitionError::InvalidJson)?;
            let terminology = parse_terminology(canonical.as_bytes())?;
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
    let (path, bytes) = file.map_or_else(
        || (None, current_json.into_bytes()),
        |file| (Some(file.resolved_path().to_owned()), file.into_bytes()),
    );
    let error_path = path.clone();
    let parsed = cpu
        .execute(move || {
            let canonical = canonicalize_json(&bytes)?;
            let definitions = serde_json::from_str::<Vec<PlaceholderRuleDefinition>>(&canonical)?;
            Ok::<_, serde_json::Error>((definitions, canonical))
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

fn canonicalize_json(bytes: &[u8]) -> Result<String, serde_json::Error> {
    fn sort(value: Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(values.into_iter().map(sort).collect()),
            Value::Object(values) => {
                let mut sorted = values.into_iter().collect::<Vec<_>>();
                sorted.sort_by(|left, right| left.0.cmp(&right.0));
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key, sort(value)))
                        .collect(),
                )
            }
            value => value,
        }
    }

    let mut duplicate_check = serde_json::Deserializer::from_slice(bytes);
    DuplicateKeyCheckedValue::deserialize(&mut duplicate_check)?;
    duplicate_check.end()?;

    let value = serde_json::from_slice::<Value>(bytes)?;
    serde_json::to_string(&sort(value))
}

struct DuplicateKeyCheckedValue;

impl<'de> Deserialize<'de> for DuplicateKeyCheckedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyCheckedVisitor)
    }
}

struct DuplicateKeyCheckedVisitor;

impl<'de> Visitor<'de> for DuplicateKeyCheckedVisitor {
    type Value = DuplicateKeyCheckedValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("完整且对象键唯一的 JSON 值")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_borrowed_str<E>(self, _value: &'de str) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        DuplicateKeyCheckedValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element::<DuplicateKeyCheckedValue>()?
            .is_some()
        {}
        Ok(DuplicateKeyCheckedValue)
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("JSON 对象键重复：{key:?}")));
            }
            object.next_value::<DuplicateKeyCheckedValue>()?;
        }
        Ok(DuplicateKeyCheckedValue)
    }
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

    #[tokio::test]
    async fn terminology_duplicate_key_is_rejected_before_canonicalization() {
        let error = parse_terminology_resource::<FakeError, _>(
            &ImmediateCpu,
            None,
            r#"[{"term":"A","term":"B","translation":"甲","triggers":["A"]}]"#.to_owned(),
        )
        .await
        .expect_err("术语对象重复键必须拒绝");

        assert!(matches!(
            &error,
            TranslationPlanningResourceReadingError::InvalidTerminology {
                source: TerminologyDefinitionError::InvalidJson(_),
                ..
            }
        ));
        assert!(format!("{error}").contains("对象键重复"));
    }

    #[tokio::test]
    async fn placeholder_duplicate_key_is_rejected_before_canonicalization() {
        let error = parse_placeholder_resource::<FakeError, _>(
            &ImmediateCpu,
            None,
            r#"[{"scopes":["event_dialogue"],"pattern":"x","pattern":"y","label":"X"}]"#.to_owned(),
        )
        .await
        .expect_err("占位符对象重复键必须拒绝");

        assert!(matches!(
            &error,
            TranslationPlanningResourceReadingError::InvalidPlaceholderRules { .. }
        ));
        assert!(format!("{error}").contains("对象键重复"));
    }

    #[test]
    fn canonicalization_rejects_duplicate_keys_at_arbitrary_depth() {
        let error = canonicalize_json(br#"{"outer":[{"nested":1,"nested":2}]}"#)
            .expect_err("任意深度的重复对象键都必须拒绝");

        assert!(error.to_string().contains("对象键重复"));
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
                "[]".to_owned(),
                "[]".to_owned(),
            )
            .await
            .expect("两份外部资料应该并发读取并分别解析");

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(resources.terminology().entries().len(), 1);
        assert_eq!(resources.placeholder_rules.len(), 1);
        assert!(resources.terminology_json.contains("魔法剣"));
        assert!(resources.placeholder_rules_json.contains("SOUND_EFFECT"));
    }
}
