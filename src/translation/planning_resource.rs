//! 外部术语与占位符 TOML 的异步读取和 CPU 解析边界。

use std::collections::HashMap;
#[cfg(test)]
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use aho_corasick::{
    AhoCorasick, AhoCorasickBuilder, Anchored, MatchKind,
    automaton::Automaton,
    nfa::{contiguous, noncontiguous},
};
use serde::{Deserialize, Serialize};

use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::isolated::{IsolatedOperationError, run_isolated_operation};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};

use super::placeholder::PlaceholderRuleDefinition;

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
        include_bytes!("../../docs/rpg-maker/examples/placeholders.toml");
    const TERMINOLOGY_EXAMPLE: &[u8] =
        include_bytes!("../../docs/rpg-maker/examples/terminology.toml");
    const GENERIC_PLACEHOLDER_EXAMPLE: &[u8] =
        include_bytes!("../../docs/generic/examples/placeholders.toml");
    const GENERIC_TERMINOLOGY_EXAMPLE: &[u8] =
        include_bytes!("../../docs/generic/examples/terminology.toml");
    const RULES_GUIDE: &str = include_str!("../../docs/rpg-maker/rules.md");
    const TERMINOLOGY_GUIDE: &str = include_str!("../../docs/translation/terminology.md");

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
    fn documented_generic_resources_use_the_production_parsers_and_compilers() {
        let definitions = parse_placeholder_toml(GENERIC_PLACEHOLDER_EXAMPLE)
            .expect("Generic 文档中的 Placeholder Rules 必须通过公共生产解析边界");
        assert!(!definitions.is_empty(), "Generic 示例必须至少声明一条规则");
        crate::generic::GenericPlaceholderService::default()
            .compile(definitions)
            .expect("Generic 文档中的 Placeholder Rules 必须通过 Generic scope 编译边界");

        let entries = parse_terminology_toml(GENERIC_TERMINOLOGY_EXAMPLE)
            .expect("Generic 文档中的 Terminology 必须通过公共生产解析边界");
        assert!(!entries.is_empty(), "Generic 示例必须至少声明一个术语");
        compile_terminology(entries)
            .expect("Generic 文档中的 Terminology 必须通过公共生产编译边界");
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
}

/// 以 Aho-Corasick 一次扫描全部 trigger 的权威术语集合。
pub(crate) struct CompiledTerminology {
    entries: Vec<TerminologyEntry>,
    matcher: Option<TerminologyMatcher>,
    pattern_to_entry: Vec<usize>,
}

enum TerminologyMatcher {
    /// 常见短 trigger 使用优化后的高层 matcher，并以少量重叠的有界窗口搜索。
    Windowed(AhoCorasick),
    /// 超长 trigger 用具体 NFA 保留跨块状态，避免把整个 pattern 反复拼入搜索窗口。
    Streaming(Box<StreamingTerminologyMatcher>),
}

enum StreamingTerminologyMatcher {
    /// 紧凑表示可以直接索引同一状态上的全部重叠匹配。
    Contiguous(contiguous::NFA),
    /// 紧凑表示无法容纳时，保留不依赖其大小限制的基础 NFA。
    Noncontiguous(noncontiguous::NFA),
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
        self.triggered_indices(texts)
            .into_iter()
            .map(|index| &self.entries[index])
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn triggered_indices<'t>(
        &self,
        texts: impl IntoIterator<Item = &'t str>,
    ) -> Vec<usize> {
        match self.triggered_indices_with_cancellation(texts, || Ok::<_, Infallible>(())) {
            Ok(indices) => indices,
            Err(unreachable) => match unreachable {},
        }
    }

    /// 扫描任意数量和长度的原文，并在文本窗口、匹配项和结果收集之间轮询取消。
    pub(crate) fn triggered_indices_with_cancellation<'t, E>(
        &self,
        texts: impl IntoIterator<Item = &'t str>,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<usize>, E> {
        ensure_running()?;
        let Some(matcher) = &self.matcher else {
            return Ok(Vec::new());
        };
        let mut matched = vec![false; self.entries.len()];
        let mut matched_count = 0_usize;
        match matcher {
            TerminologyMatcher::Windowed(matcher) => scan_windowed_terminology(
                matcher,
                &self.pattern_to_entry,
                texts,
                &mut matched,
                &mut matched_count,
                &mut ensure_running,
            )?,
            TerminologyMatcher::Streaming(matcher) => match matcher.as_ref() {
                StreamingTerminologyMatcher::Contiguous(matcher) => scan_streaming_terminology(
                    matcher,
                    &self.pattern_to_entry,
                    texts,
                    &mut matched,
                    &mut matched_count,
                    &mut ensure_running,
                )?,
                StreamingTerminologyMatcher::Noncontiguous(matcher) => scan_streaming_terminology(
                    matcher,
                    &self.pattern_to_entry,
                    texts,
                    &mut matched,
                    &mut matched_count,
                    &mut ensure_running,
                )?,
            },
        }
        let mut indices = Vec::with_capacity(matched_count);
        for (index, matched) in matched.into_iter().enumerate() {
            ensure_running()?;
            if matched {
                indices.push(index);
            }
        }
        ensure_running()?;
        Ok(indices)
    }
}

/// 重叠窗口最多重复约 1/15 的输入；更长 trigger 改用真正流式的 NFA。
const TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES: usize = PLANNING_RESOURCE_CANCEL_CHECK_BYTES / 16;

fn scan_windowed_terminology<'t, E>(
    matcher: &AhoCorasick,
    pattern_to_entry: &[usize],
    texts: impl IntoIterator<Item = &'t str>,
    matched: &mut [bool],
    matched_count: &mut usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let maximum_pattern_bytes = matcher.max_pattern_len();
    debug_assert!(
        maximum_pattern_bytes <= TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES,
        "只有短 trigger 才能进入重叠窗口扫描"
    );
    let overlap = maximum_pattern_bytes.saturating_sub(1);
    let advance = PLANNING_RESOURCE_CANCEL_CHECK_BYTES
        .checked_sub(overlap)
        .expect("短 trigger 的重叠必须小于取消检查窗口");

    for text in texts {
        ensure_running()?;
        let bytes = text.as_bytes();
        let mut start = 0_usize;
        while start < bytes.len() {
            ensure_running()?;
            let end = start
                .saturating_add(PLANNING_RESOURCE_CANCEL_CHECK_BYTES)
                .min(bytes.len());
            for found in matcher.find_overlapping_iter(&bytes[start..end]) {
                ensure_running()?;
                record_terminology_match(
                    found.pattern().as_usize(),
                    pattern_to_entry,
                    matched,
                    matched_count,
                );
            }
            if end == bytes.len() {
                break;
            }
            start = start.saturating_add(advance);
        }
    }
    ensure_running()
}

fn scan_streaming_terminology<'t, E, A: Automaton>(
    matcher: &A,
    pattern_to_entry: &[usize],
    texts: impl IntoIterator<Item = &'t str>,
    matched: &mut [bool],
    matched_count: &mut usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    for text in texts {
        ensure_running()?;
        let mut state = matcher
            .start_state(Anchored::No)
            .expect("术语 NFA 必须支持非锚定搜索");
        'text: for chunk in text.as_bytes().chunks(PLANNING_RESOURCE_CANCEL_CHECK_BYTES) {
            ensure_running()?;
            for &byte in chunk {
                state = matcher.next_state(Anchored::No, state, byte);
                if !matcher.is_special(state) {
                    continue;
                }
                let dead = matcher.is_dead(state);
                debug_assert!(!dead, "Standard 非锚定术语 NFA 不应进入 dead state");
                if dead {
                    break 'text;
                }
                if !matcher.is_match(state) {
                    continue;
                }
                for match_index in 0..matcher.match_len(state) {
                    ensure_running()?;
                    record_terminology_match(
                        matcher.match_pattern(state, match_index).as_usize(),
                        pattern_to_entry,
                        matched,
                        matched_count,
                    );
                }
            }
        }
    }
    ensure_running()
}

fn record_terminology_match(
    pattern_index: usize,
    pattern_to_entry: &[usize],
    matched: &mut [bool],
    matched_count: &mut usize,
) {
    let entry_index = pattern_to_entry[pattern_index];
    if !matched[entry_index] {
        matched[entry_index] = true;
        *matched_count += 1;
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
    cancellation: CooperativeCancellation,
}

impl<F, C> TranslationPlanningResourceReadingService<F, C> {
    pub(crate) fn new(file_reader: F, cpu: C) -> Self {
        Self {
            file_reader,
            cpu,
            cancellation: CooperativeCancellation::default(),
        }
    }

    pub(crate) fn with_cancellation(mut self, cancellation: CooperativeCancellation) -> Self {
        self.cancellation = cancellation;
        self
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
        if self.cancellation.is_requested() {
            return Err(TranslationPlanningResourceReadingError::Cancelled);
        }
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
        if self.cancellation.is_requested() {
            return Err(TranslationPlanningResourceReadingError::Cancelled);
        }

        let terminology_parse = parse_terminology_resource::<F::Error, C>(
            &self.cpu,
            terminology_file,
            current_terminology_json,
            self.cancellation.clone(),
        );
        let placeholder_parse = parse_placeholder_resource::<F::Error, C>(
            &self.cpu,
            placeholder_file,
            current_placeholder_rules_json,
            self.cancellation.clone(),
        );
        let (terminology, placeholder_rules) =
            futures_util::join!(terminology_parse, placeholder_parse);
        let (terminology, terminology_json) = terminology?;
        let (placeholder_rules, placeholder_rules_json) = placeholder_rules?;
        if self.cancellation.is_requested() {
            return Err(TranslationPlanningResourceReadingError::Cancelled);
        }

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
    cancellation: CooperativeCancellation,
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
            let is_cancelled = || cancellation.is_requested();
            ensure_planning_resource_running(&is_cancelled)
                .map_err(|()| TerminologyDefinitionError::Cancelled)?;
            let entries = match input {
                PlanningResourceInput::External(bytes) => {
                    parse_terminology_toml_with_cancellation(bytes, &is_cancelled)?
                }
                PlanningResourceInput::Snapshot(json) => {
                    parse_terminology_snapshot_with_cancellation(&json, &is_cancelled)?
                }
            };
            let canonical = encode_terminology_snapshot_with_cancellation(&entries, &is_cancelled)?;
            let terminology = compile_terminology_with_cancellation(entries, &is_cancelled)?;
            Ok::<_, TerminologyDefinitionError>((terminology, canonical))
        })
        .await
        .map_err(
            |source| TranslationPlanningResourceReadingError::ParseTerminologyCompute {
                path: error_path.clone(),
                source,
            },
        )?;
    match parsed {
        Err(TerminologyDefinitionError::Cancelled) => {
            Err(TranslationPlanningResourceReadingError::Cancelled)
        }
        Err(source) => {
            Err(TranslationPlanningResourceReadingError::InvalidTerminology { path, source })
        }
        Ok(result) => Ok(result),
    }
}

async fn parse_placeholder_resource<F, C: CpuTaskExecutor>(
    cpu: &C,
    file: Option<ReadFile>,
    current_json: String,
    cancellation: CooperativeCancellation,
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
            let is_cancelled = || cancellation.is_requested();
            ensure_planning_resource_running(&is_cancelled)
                .map_err(|()| PlaceholderDefinitionError::Cancelled)?;
            let definitions = match input {
                PlanningResourceInput::External(bytes) => {
                    parse_placeholder_toml_with_cancellation(bytes, &is_cancelled)?
                }
                PlanningResourceInput::Snapshot(json) => {
                    parse_placeholder_snapshot_with_cancellation(&json, &is_cancelled)?
                }
            };
            let canonical =
                encode_placeholder_snapshot_with_cancellation(&definitions, &is_cancelled)?;
            Ok::<_, PlaceholderDefinitionError>((definitions, canonical))
        })
        .await
        .map_err(|source| {
            TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
                path: error_path.clone(),
                source,
            }
        })?;
    match parsed {
        Err(PlaceholderDefinitionError::Cancelled) => {
            Err(TranslationPlanningResourceReadingError::Cancelled)
        }
        Err(source) => {
            Err(TranslationPlanningResourceReadingError::InvalidPlaceholderRules { path, source })
        }
        Ok(result) => Ok(result),
    }
}

enum PlanningResourceInput {
    External(Vec<u8>),
    Snapshot(String),
}

const PLANNING_RESOURCE_CANCEL_CHECK_BYTES: usize = 64 * 1024;

pub(crate) trait PlanningResourceCancellation {
    fn is_cancelled(&self) -> bool;
}

impl<F> PlanningResourceCancellation for F
where
    F: Fn() -> bool,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

#[cfg(test)]
fn never_cancelled() -> bool {
    false
}

fn ensure_planning_resource_running(
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<(), ()> {
    if cancellation.is_cancelled() {
        Err(())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
enum CancellableUtf8Error {
    Cancelled,
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
    Invalid(std::str::Utf8Error),
}

fn terminology_isolated_operation_error(
    source: IsolatedOperationError<()>,
) -> TerminologyDefinitionError {
    match source {
        IsolatedOperationError::Cancelled(_) => TerminologyDefinitionError::Cancelled,
        IsolatedOperationError::Start { operation, source } => {
            TerminologyDefinitionError::StartWorker { operation, source }
        }
    }
}

fn placeholder_isolated_operation_error(
    source: IsolatedOperationError<()>,
) -> PlaceholderDefinitionError {
    match source {
        IsolatedOperationError::Cancelled(_) => PlaceholderDefinitionError::Cancelled,
        IsolatedOperationError::Start { operation, source } => {
            PlaceholderDefinitionError::StartWorker { operation, source }
        }
    }
}

/// 分块确认完整 UTF-8 后复用原 `Vec` 的分配建立 `String`。
fn into_utf8_string_with_cancellation(
    bytes: Vec<u8>,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<String, CancellableUtf8Error> {
    let mut start = 0_usize;
    while start < bytes.len() {
        ensure_planning_resource_running(cancellation)
            .map_err(|()| CancellableUtf8Error::Cancelled)?;
        let end = start
            .saturating_add(PLANNING_RESOURCE_CANCEL_CHECK_BYTES)
            .min(bytes.len());
        match std::str::from_utf8(&bytes[start..end]) {
            Ok(_) => start = end,
            Err(source) => {
                let valid_end = start.saturating_add(source.valid_up_to());
                match source.error_len() {
                    Some(_) => {
                        ensure_planning_resource_running(cancellation)
                            .map_err(|()| CancellableUtf8Error::Cancelled)?;
                        return invalid_utf8_error_with_cancellation(bytes, cancellation);
                    }
                    None if end == bytes.len() => {
                        ensure_planning_resource_running(cancellation)
                            .map_err(|()| CancellableUtf8Error::Cancelled)?;
                        return invalid_utf8_error_with_cancellation(bytes, cancellation);
                    }
                    None => {
                        debug_assert!(valid_end > start, "不完整 UTF-8 序列只能位于非空块的末尾");
                        start = valid_end;
                    }
                }
            }
        }
    }
    ensure_planning_resource_running(cancellation).map_err(|()| CancellableUtf8Error::Cancelled)?;
    // SAFETY: 上面的循环覆盖整份字节串。跨块的不完整码点会从该码点起始位置在下一块
    // 重新校验；只有最后一块也完整时循环才会成功结束。
    Ok(unsafe { String::from_utf8_unchecked(bytes) })
}

fn invalid_utf8_error_with_cancellation(
    bytes: Vec<u8>,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<String, CancellableUtf8Error> {
    match run_isolated_operation(
        "att-resource-utf8",
        move || std::str::from_utf8(&bytes).expect_err("分块 UTF-8 校验已经确认输入无效"),
        || ensure_planning_resource_running(cancellation),
    ) {
        Ok(source) => Err(CancellableUtf8Error::Invalid(source)),
        Err(IsolatedOperationError::Cancelled(_)) => Err(CancellableUtf8Error::Cancelled),
        Err(IsolatedOperationError::Start { operation, source }) => {
            Err(CancellableUtf8Error::StartWorker { operation, source })
        }
    }
}

struct CancellableSliceReader<'a, C: ?Sized> {
    source: &'a [u8],
    offset: usize,
    cancellation: &'a C,
}

impl<'a, C: PlanningResourceCancellation + ?Sized> CancellableSliceReader<'a, C> {
    fn new(source: &'a [u8], cancellation: &'a C) -> Self {
        Self {
            source,
            offset: 0,
            cancellation,
        }
    }
}

impl<C: PlanningResourceCancellation + ?Sized> Read for CancellableSliceReader<'_, C> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            // `Read` 调用方会自动重试 `Interrupted`；取消必须返回不可重试错误，
            // 否则 serde_json 会在同一个读取点永久循环。
            return Err(io::Error::other("翻译资源解析已取消"));
        }
        if self.offset == self.source.len() || buffer.is_empty() {
            return Ok(0);
        }
        let amount = buffer
            .len()
            .min(PLANNING_RESOURCE_CANCEL_CHECK_BYTES)
            .min(self.source.len() - self.offset);
        buffer[..amount].copy_from_slice(&self.source[self.offset..self.offset + amount]);
        self.offset += amount;
        Ok(amount)
    }
}

struct CancellableVecWriter<'a, C: ?Sized> {
    output: &'a mut Vec<u8>,
    cancellation: &'a C,
}

impl<C: PlanningResourceCancellation + ?Sized> Write for CancellableVecWriter<'_, C> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            // `write_all` 会自动重试 `Interrupted`，因此取消使用不可重试错误。
            return Err(io::Error::other("翻译资源编码已取消"));
        }
        let amount = bytes.len().min(PLANNING_RESOURCE_CANCEL_CHECK_BYTES);
        self.output.extend_from_slice(&bytes[..amount]);
        Ok(amount)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            Err(io::Error::other("翻译资源编码已取消"))
        } else {
            Ok(())
        }
    }
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

#[cfg(test)]
fn parse_terminology_toml(
    bytes: &[u8],
) -> Result<Vec<TerminologyEntry>, TerminologyDefinitionError> {
    parse_terminology_toml_with_cancellation(bytes.to_vec(), &never_cancelled)
}

fn parse_terminology_toml_with_cancellation(
    bytes: Vec<u8>,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<Vec<TerminologyEntry>, TerminologyDefinitionError> {
    let source =
        into_utf8_string_with_cancellation(bytes, cancellation).map_err(|source| match source {
            CancellableUtf8Error::Cancelled => TerminologyDefinitionError::Cancelled,
            CancellableUtf8Error::StartWorker { operation, source } => {
                TerminologyDefinitionError::StartWorker { operation, source }
            }
            CancellableUtf8Error::Invalid(source) => {
                TerminologyDefinitionError::InvalidUtf8(source)
            }
        })?;
    let definition = run_isolated_operation(
        "att-term-toml",
        move || toml::from_str::<TerminologyToml>(&source),
        || ensure_planning_resource_running(cancellation),
    )
    .map_err(terminology_isolated_operation_error)?
    .map_err(TerminologyDefinitionError::InvalidToml)?;
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    let mut entries = Vec::<TerminologyEntry>::with_capacity(definition.term.len());
    for entry in definition.term {
        ensure_planning_resource_running(cancellation)
            .map_err(|()| TerminologyDefinitionError::Cancelled)?;
        let triggers = match entry.triggers {
            Some(triggers) => triggers,
            None => vec![
                clone_planning_resource_string(&entry.term, cancellation)
                    .map_err(|()| TerminologyDefinitionError::Cancelled)?,
            ],
        };
        entries.push(TerminologyEntry {
            term: entry.term,
            translation: entry.translation,
            triggers,
        });
    }
    Ok(entries)
}

fn parse_terminology_snapshot_with_cancellation(
    json: &str,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<Vec<TerminologyEntry>, TerminologyDefinitionError> {
    let source = CancellableSliceReader::new(json.as_bytes(), cancellation);
    let mut reader = BufReader::with_capacity(PLANNING_RESOURCE_CANCEL_CHECK_BYTES, source);
    let result = serde_json::from_reader(&mut reader);
    if cancellation.is_cancelled() {
        Err(TerminologyDefinitionError::Cancelled)
    } else {
        result.map_err(TerminologyDefinitionError::InvalidSnapshot)
    }
}

#[cfg(test)]
fn parse_placeholder_toml(
    bytes: &[u8],
) -> Result<Vec<PlaceholderRuleDefinition>, PlaceholderDefinitionError> {
    parse_placeholder_toml_with_cancellation(bytes.to_vec(), &never_cancelled)
}

fn parse_placeholder_toml_with_cancellation(
    bytes: Vec<u8>,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<Vec<PlaceholderRuleDefinition>, PlaceholderDefinitionError> {
    let source =
        into_utf8_string_with_cancellation(bytes, cancellation).map_err(|source| match source {
            CancellableUtf8Error::Cancelled => PlaceholderDefinitionError::Cancelled,
            CancellableUtf8Error::StartWorker { operation, source } => {
                PlaceholderDefinitionError::StartWorker { operation, source }
            }
            CancellableUtf8Error::Invalid(source) => {
                PlaceholderDefinitionError::InvalidUtf8(source)
            }
        })?;
    let definition = run_isolated_operation(
        "att-placeholder-toml",
        move || toml::from_str::<PlaceholderToml>(&source),
        || ensure_planning_resource_running(cancellation),
    )
    .map_err(placeholder_isolated_operation_error)?
    .map_err(PlaceholderDefinitionError::InvalidToml)?;
    ensure_planning_resource_running(cancellation)
        .map_err(|()| PlaceholderDefinitionError::Cancelled)?;
    Ok(definition.rule)
}

fn parse_placeholder_snapshot_with_cancellation(
    json: &str,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<Vec<PlaceholderRuleDefinition>, PlaceholderDefinitionError> {
    let source = CancellableSliceReader::new(json.as_bytes(), cancellation);
    let mut reader = BufReader::with_capacity(PLANNING_RESOURCE_CANCEL_CHECK_BYTES, source);
    let result = serde_json::from_reader(&mut reader);
    if cancellation.is_cancelled() {
        Err(PlaceholderDefinitionError::Cancelled)
    } else {
        result.map_err(PlaceholderDefinitionError::InvalidSnapshot)
    }
}

fn encode_terminology_snapshot_with_cancellation(
    entries: &[TerminologyEntry],
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<String, TerminologyDefinitionError> {
    let mut output = Vec::new();
    let result = {
        let mut writer = CancellableVecWriter {
            output: &mut output,
            cancellation,
        };
        serde_json::to_writer(&mut writer, entries)
    };
    if cancellation.is_cancelled() {
        return Err(TerminologyDefinitionError::Cancelled);
    }
    result.map_err(TerminologyDefinitionError::EncodeSnapshot)?;
    String::from_utf8(output).map_err(|source| {
        TerminologyDefinitionError::EncodeSnapshot(serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            source,
        )))
    })
}

fn encode_placeholder_snapshot_with_cancellation(
    definitions: &[PlaceholderRuleDefinition],
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<String, PlaceholderDefinitionError> {
    let mut output = Vec::new();
    let result = {
        let mut writer = CancellableVecWriter {
            output: &mut output,
            cancellation,
        };
        serde_json::to_writer(&mut writer, definitions)
    };
    if cancellation.is_cancelled() {
        return Err(PlaceholderDefinitionError::Cancelled);
    }
    result.map_err(PlaceholderDefinitionError::EncodeSnapshot)?;
    String::from_utf8(output).map_err(|source| {
        PlaceholderDefinitionError::EncodeSnapshot(serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            source,
        )))
    })
}

fn clone_planning_resource_string(
    source: &str,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<String, ()> {
    let mut output = String::with_capacity(source.len());
    let mut start = 0;
    while start < source.len() {
        ensure_planning_resource_running(cancellation)?;
        let mut end = (start + PLANNING_RESOURCE_CANCEL_CHECK_BYTES).min(source.len());
        while end < source.len() && !source.is_char_boundary(end) {
            end += 1;
        }
        output.push_str(&source[start..end]);
        start = end;
    }
    ensure_planning_resource_running(cancellation)?;
    Ok(output)
}

#[cfg(test)]
pub(crate) fn compile_terminology(
    raw: Vec<TerminologyEntry>,
) -> Result<CompiledTerminology, TerminologyDefinitionError> {
    compile_terminology_with_cancellation(raw, &never_cancelled)
}

pub(crate) fn compile_terminology_with_cancellation(
    raw: Vec<TerminologyEntry>,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<CompiledTerminology, TerminologyDefinitionError> {
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    let mut entries = Vec::<TerminologyEntry>::with_capacity(raw.len());
    let mut entry_by_term = HashMap::<Sha256Fingerprint, Vec<usize>>::new();
    let mut trigger_by_text = HashMap::<Sha256Fingerprint, Vec<(usize, usize)>>::new();
    let mut pattern_to_entry = Vec::new();
    let mut maximum_trigger_bytes = 0_usize;

    for raw_entry in raw {
        ensure_planning_resource_running(cancellation)
            .map_err(|()| TerminologyDefinitionError::Cancelled)?;
        let entry_index = entries.len();
        let entry_number = entry_index + 1;
        validate_term_string_with_cancellation(
            "term",
            &raw_entry.term,
            entry_number,
            false,
            cancellation,
        )?;
        validate_term_string_with_cancellation(
            "translation",
            &raw_entry.translation,
            entry_number,
            false,
            cancellation,
        )?;
        if raw_entry.triggers.is_empty() {
            return Err(TerminologyDefinitionError::EmptyTriggers { entry_number });
        }

        let term_fingerprint = terminology_text_fingerprint(
            b"att.terminology.term-identity",
            &raw_entry.term,
            cancellation,
        )?;
        if let Some(candidates) = entry_by_term.get(&term_fingerprint) {
            for &candidate_index in candidates {
                if terminology_text_eq_with_cancellation(
                    &entries[candidate_index].term,
                    &raw_entry.term,
                    cancellation,
                )? {
                    return Err(TerminologyDefinitionError::DuplicateTerm {
                        term: raw_entry.term,
                    });
                }
            }
        }

        entries.push(raw_entry);
        entry_by_term
            .entry(term_fingerprint)
            .or_default()
            .push(entry_index);

        for trigger_index in 0..entries[entry_index].triggers.len() {
            ensure_planning_resource_running(cancellation)
                .map_err(|()| TerminologyDefinitionError::Cancelled)?;
            let trigger = &entries[entry_index].triggers[trigger_index];
            validate_term_string_with_cancellation(
                "trigger",
                trigger,
                entry_number,
                true,
                cancellation,
            )?;
            maximum_trigger_bytes = maximum_trigger_bytes.max(trigger.len());

            let trigger_fingerprint = terminology_text_fingerprint(
                b"att.terminology.trigger-identity",
                trigger,
                cancellation,
            )?;
            let mut duplicate = false;
            if let Some(candidates) = trigger_by_text.get(&trigger_fingerprint) {
                for &(candidate_entry, candidate_trigger) in candidates {
                    if terminology_text_eq_with_cancellation(
                        &entries[candidate_entry].triggers[candidate_trigger],
                        trigger,
                        cancellation,
                    )? {
                        duplicate = true;
                        break;
                    }
                }
            }
            if duplicate {
                return Err(TerminologyDefinitionError::DuplicateTrigger {
                    trigger: clone_planning_resource_string(trigger, cancellation)
                        .map_err(|()| TerminologyDefinitionError::Cancelled)?,
                });
            }
            trigger_by_text
                .entry(trigger_fingerprint)
                .or_default()
                .push((entry_index, trigger_index));
            pattern_to_entry.push(entry_index);
        }
    }

    drop(entry_by_term);
    drop(trigger_by_text);
    let (entries, matcher) = if pattern_to_entry.is_empty() {
        (entries, None)
    } else {
        ensure_planning_resource_running(cancellation)
            .map_err(|()| TerminologyDefinitionError::Cancelled)?;
        let (entries, matcher) = run_isolated_operation(
            "att-term-matcher",
            move || build_terminology_matcher(entries, maximum_trigger_bytes),
            || ensure_planning_resource_running(cancellation),
        )
        .map_err(terminology_isolated_operation_error)?
        .map_err(TerminologyDefinitionError::CompileMatcher)?;
        (entries, Some(matcher))
    };

    Ok(CompiledTerminology {
        entries,
        matcher,
        pattern_to_entry,
    })
}

fn build_terminology_matcher(
    entries: Vec<TerminologyEntry>,
    maximum_trigger_bytes: usize,
) -> Result<(Vec<TerminologyEntry>, TerminologyMatcher), aho_corasick::BuildError> {
    let matcher = if maximum_trigger_bytes <= TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES {
        TerminologyMatcher::Windowed(
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .build(
                    entries
                        .iter()
                        .flat_map(|entry| entry.triggers.iter().map(String::as_bytes)),
                )?,
        )
    } else {
        let mut builder = noncontiguous::NFA::builder();
        builder.match_kind(MatchKind::Standard).prefilter(false);
        let noncontiguous = builder.build(
            entries
                .iter()
                .flat_map(|entry| entry.triggers.iter().map(String::as_bytes)),
        )?;
        let streaming = match contiguous::NFA::builder().build_from_noncontiguous(&noncontiguous) {
            Ok(contiguous) => StreamingTerminologyMatcher::Contiguous(contiguous),
            Err(_) => StreamingTerminologyMatcher::Noncontiguous(noncontiguous),
        };
        TerminologyMatcher::Streaming(Box::new(streaming))
    };
    Ok((entries, matcher))
}

fn terminology_text_fingerprint(
    domain: &[u8],
    text: &str,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<Sha256Fingerprint, TerminologyDefinitionError> {
    let chunk_size = NonZeroUsize::new(PLANNING_RESOURCE_CANCEL_CHECK_BYTES)
        .expect("术语指纹取消检查块大小必须非零");
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    let mut hasher = Sha256FramedHasher::new(domain);
    hasher
        .try_frame_chunks(1, text.as_bytes(), chunk_size, || {
            ensure_planning_resource_running(cancellation)
        })
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    Ok(hasher.finish())
}

fn terminology_text_eq_with_cancellation(
    left: &str,
    right: &str,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<bool, TerminologyDefinitionError> {
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(PLANNING_RESOURCE_CANCEL_CHECK_BYTES)
        .zip(
            right
                .as_bytes()
                .chunks(PLANNING_RESOURCE_CANCEL_CHECK_BYTES),
        )
    {
        ensure_planning_resource_running(cancellation)
            .map_err(|()| TerminologyDefinitionError::Cancelled)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    Ok(true)
}

fn validate_term_string_with_cancellation(
    field: &'static str,
    value: &str,
    entry_number: usize,
    allow_line_feed: bool,
    cancellation: &(impl PlanningResourceCancellation + ?Sized),
) -> Result<(), TerminologyDefinitionError> {
    let mut has_non_whitespace = false;
    let mut first_is_whitespace = None;
    let mut last_is_whitespace = false;
    let mut first_disallowed_control = None;
    let mut next_check = 0;
    for (offset, character) in value.char_indices() {
        if offset >= next_check {
            ensure_planning_resource_running(cancellation)
                .map_err(|()| TerminologyDefinitionError::Cancelled)?;
            next_check = offset.saturating_add(PLANNING_RESOURCE_CANCEL_CHECK_BYTES);
        }
        let whitespace = character.is_whitespace();
        first_is_whitespace.get_or_insert(whitespace);
        last_is_whitespace = whitespace;
        has_non_whitespace |= !whitespace;
        if first_disallowed_control.is_none()
            && character.is_control()
            && (!allow_line_feed || character != '\n')
        {
            first_disallowed_control = Some(character);
        }
    }
    ensure_planning_resource_running(cancellation)
        .map_err(|()| TerminologyDefinitionError::Cancelled)?;
    if !has_non_whitespace {
        return Err(TerminologyDefinitionError::BlankField {
            entry_number,
            field,
        });
    }
    if first_is_whitespace.unwrap_or(false) || last_is_whitespace {
        return Err(TerminologyDefinitionError::SurroundingWhitespace {
            entry_number,
            field,
        });
    }
    if let Some(character) = first_disallowed_control {
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
    Cancelled,
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
            Self::Cancelled => formatter.write_str("翻译资源读取已取消"),
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
            Self::Cancelled => None,
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
    Cancelled,
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
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
            Self::Cancelled => formatter.write_str("术语处理已取消"),
            Self::StartWorker { operation, source } => {
                write!(formatter, "无法启动术语处理 worker {operation}：{source}")
            }
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
            Self::Cancelled => None,
            Self::StartWorker { source, .. } => Some(source),
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
    Cancelled,
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
    InvalidUtf8(std::str::Utf8Error),
    InvalidToml(toml::de::Error),
    InvalidSnapshot(serde_json::Error),
    EncodeSnapshot(serde_json::Error),
}

impl fmt::Display for PlaceholderDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("占位符规则处理已取消"),
            Self::StartWorker { operation, source } => {
                write!(
                    formatter,
                    "无法启动占位符规则处理 worker {operation}：{source}"
                )
            }
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
            Self::Cancelled => None,
            Self::StartWorker { source, .. } => Some(source),
            Self::InvalidUtf8(source) => Some(source),
            Self::InvalidToml(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::EncodeSnapshot(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::runtime::cpu::{CpuExecutorConfig, RayonCpuExecutor};
    use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemConfig};

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

    struct CancelAtPoll {
        polls: AtomicUsize,
        cancel_at: usize,
    }

    impl CancelAtPoll {
        fn new(cancel_at: usize) -> Self {
            Self {
                polls: AtomicUsize::new(0),
                cancel_at,
            }
        }
    }

    impl CancelAtPoll {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) >= self.cancel_at
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
    fn terminology_compilation_stops_between_entries() {
        let definitions = (0..1_000)
            .map(|index| TerminologyEntry {
                term: format!("term-{index}"),
                translation: format!("translation-{index}"),
                triggers: vec![format!("trigger-{index}")],
            })
            .collect();
        let cancellation = CancelAtPoll::new(40);

        assert!(matches!(
            compile_terminology_with_cancellation(definitions, &|| cancellation.is_cancelled()),
            Err(TerminologyDefinitionError::Cancelled)
        ));
        assert!(
            cancellation.polls.load(Ordering::SeqCst) < 200,
            "取消后不得继续扫描全部术语"
        );
    }

    #[test]
    fn terminology_identity_fingerprint_stops_between_long_text_chunks() {
        let text = "x".repeat(PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 4);
        let cancellation = CancelAtPoll::new(2);

        assert!(matches!(
            terminology_text_fingerprint(b"att.terminology.test-identity", &text, &|| cancellation
                .is_cancelled(),),
            Err(TerminologyDefinitionError::Cancelled)
        ));
        assert_eq!(cancellation.polls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn terminology_collision_comparison_stops_between_long_common_prefix_chunks() {
        let prefix = "x".repeat(PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 4);
        let left = format!("{prefix}a");
        let right = format!("{prefix}b");
        let cancellation = CancelAtPoll::new(2);

        assert!(matches!(
            terminology_text_eq_with_cancellation(&left, &right, &|| cancellation.is_cancelled(),),
            Err(TerminologyDefinitionError::Cancelled)
        ));
        assert_eq!(cancellation.polls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn long_common_prefix_terms_and_triggers_keep_file_order_semantics() {
        let prefix = "x".repeat(PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 2 + 17);
        let first_trigger = format!("{prefix}-first-trigger");
        let second_trigger = format!("{prefix}-second-trigger");
        let compiled = compile_terminology(vec![
            TerminologyEntry {
                term: format!("{prefix}-first-term"),
                translation: "甲".to_owned(),
                triggers: vec![first_trigger.clone()],
            },
            TerminologyEntry {
                term: format!("{prefix}-second-term"),
                translation: "乙".to_owned(),
                triggers: vec![second_trigger.clone()],
            },
        ])
        .expect("超长共同前缀不应改变术语身份或 Aho pattern 顺序");
        let text = format!("{second_trigger}\n{first_trigger}");

        assert_eq!(
            compiled
                .triggered_by([text.as_str()])
                .into_iter()
                .map(TerminologyEntry::translation)
                .collect::<Vec<_>>(),
            ["甲", "乙"]
        );
    }

    #[test]
    fn cancellable_utf8_validation_preserves_cross_chunk_codepoint_and_allocation() {
        let mut bytes = vec![b'x'; PLANNING_RESOURCE_CANCEL_CHECK_BYTES - 1];
        bytes.extend_from_slice("译".as_bytes());
        let original_pointer = bytes.as_ptr();

        let source = into_utf8_string_with_cancellation(bytes, &never_cancelled)
            .expect("跨分块的 UTF-8 码点应有效");

        assert_eq!(source.as_ptr(), original_pointer);
        assert!(source.ends_with('译'));
    }

    #[test]
    fn cancellable_utf8_validation_reports_global_invalid_offset() {
        let invalid_offset = PLANNING_RESOURCE_CANCEL_CHECK_BYTES + 17;
        let mut bytes = vec![b'x'; PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 2];
        bytes[invalid_offset] = 0xff;

        match into_utf8_string_with_cancellation(bytes, &never_cancelled) {
            Err(CancellableUtf8Error::Invalid(source)) => {
                assert_eq!(source.valid_up_to(), invalid_offset);
            }
            Err(CancellableUtf8Error::Cancelled) => panic!("未请求取消"),
            Err(CancellableUtf8Error::StartWorker { operation, source }) => {
                panic!("不应无法启动 UTF-8 诊断 worker {operation}：{source}");
            }
            Ok(_) => panic!("无效 UTF-8 不应通过校验"),
        }
    }

    #[test]
    fn terminology_snapshot_parser_stops_while_reading_json() {
        let json = format!(
            r#"[{{"term":"{}","translation":"译文","triggers":["触发"]}}]"#,
            "x".repeat(PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 4)
        );
        let cancellation = CancelAtPoll::new(2);

        assert!(matches!(
            parse_terminology_snapshot_with_cancellation(&json, &|| cancellation.is_cancelled()),
            Err(TerminologyDefinitionError::Cancelled)
        ));
    }

    #[test]
    fn terminology_snapshot_encoder_stops_while_writing_json() {
        let entries = vec![TerminologyEntry {
            term: "x".repeat(PLANNING_RESOURCE_CANCEL_CHECK_BYTES * 4),
            translation: "译文".to_owned(),
            triggers: vec!["触发".to_owned()],
        }];
        let cancellation = CancelAtPoll::new(3);

        assert!(matches!(
            encode_terminology_snapshot_with_cancellation(&entries, &|| cancellation
                .is_cancelled()),
            Err(TerminologyDefinitionError::Cancelled)
        ));
    }

    #[test]
    fn cancellable_terminology_scan_preserves_cross_window_matches() {
        const WINDOW_BYTES: usize = 64 * 1024;
        let compiled = compile_terminology(vec![TerminologyEntry {
            term: "boundary".to_owned(),
            translation: "边界".to_owned(),
            triggers: vec!["ABCD".to_owned()],
        }])
        .expect("术语应该有效");
        let mut text = "x".repeat(WINDOW_BYTES - 2);
        text.push_str("ABCD");

        let indices = compiled
            .triggered_indices_with_cancellation([text.as_str()], || Ok::<_, ()>(()))
            .expect("未取消的扫描应该完成");

        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn cancellable_terminology_scan_reports_all_nested_and_overlapping_patterns() {
        let compiled = compile_terminology(vec![
            TerminologyEntry {
                term: "one".to_owned(),
                translation: "一".to_owned(),
                triggers: vec!["a".to_owned()],
            },
            TerminologyEntry {
                term: "two".to_owned(),
                translation: "二".to_owned(),
                triggers: vec!["aa".to_owned()],
            },
            TerminologyEntry {
                term: "three".to_owned(),
                translation: "三".to_owned(),
                triggers: vec!["aaa".to_owned()],
            },
        ])
        .expect("嵌套 trigger 应可编译");

        let indices = compiled
            .triggered_indices_with_cancellation(["aaaa"], || Ok::<_, ()>(()))
            .expect("未取消的重叠扫描应完成");

        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn cancellable_terminology_streaming_scan_reports_all_nested_patterns() {
        let longest = "a".repeat(TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES + 1);
        let middle = "a".repeat(TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES);
        let shortest = "a".repeat(TERMINOLOGY_WINDOWED_MAX_PATTERN_BYTES - 1);
        let compiled = compile_terminology(vec![
            TerminologyEntry {
                term: "longest".to_owned(),
                translation: "最长".to_owned(),
                triggers: vec![longest.clone()],
            },
            TerminologyEntry {
                term: "middle".to_owned(),
                translation: "中间".to_owned(),
                triggers: vec![middle],
            },
            TerminologyEntry {
                term: "shortest".to_owned(),
                translation: "最短".to_owned(),
                triggers: vec![shortest],
            },
        ])
        .expect("嵌套的长 trigger 应可编译");
        assert!(matches!(
            compiled.matcher.as_ref(),
            Some(TerminologyMatcher::Streaming(_))
        ));
        let text = format!("{longest}a");

        let indices = compiled
            .triggered_indices_with_cancellation([text.as_str()], || Ok::<_, ()>(()))
            .expect("流式扫描必须枚举同一状态上的全部嵌套匹配");

        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn cancellable_terminology_scan_streams_multiblock_trigger_and_cancels_by_chunk() {
        const WINDOW_BYTES: usize = 64 * 1024;
        const LONG_PATTERN_BYTES: usize = WINDOW_BYTES * 2 + 17;
        let trigger = "n".repeat(LONG_PATTERN_BYTES);
        let compiled = compile_terminology(vec![TerminologyEntry {
            term: "long-boundary".to_owned(),
            translation: "长边界".to_owned(),
            triggers: vec![trigger.clone()],
        }])
        .expect("长术语应该有效");
        assert!(matches!(
            compiled.matcher.as_ref(),
            Some(TerminologyMatcher::Streaming(_))
        ));
        let mut text = "x".repeat(WINDOW_BYTES - 11);
        text.push_str(&trigger);
        let mut polls = 0_usize;

        let indices = compiled
            .triggered_indices_with_cancellation([text.as_str()], || {
                polls += 1;
                Ok::<_, ()>(())
            })
            .expect("未取消的长术语扫描应该完成");

        assert_eq!(indices, vec![0]);
        let input_chunks = text.len().div_ceil(WINDOW_BYTES);
        assert!(
            polls >= input_chunks,
            "每处理至多一个 64 KiB 输入块必须轮询一次，实际 chunks={input_chunks}, polls={polls}"
        );
        assert!(
            polls <= input_chunks + 6,
            "单个长 trigger 应线性流式扫描，不能按 pattern 长度反复窗口扫描，实际 chunks={input_chunks}, polls={polls}"
        );

        let missing = "x".repeat(WINDOW_BYTES * 8);
        let mut cancellation_polls = 0_usize;
        let cancelled = compiled.triggered_indices_with_cancellation([missing.as_str()], || {
            cancellation_polls += 1;
            if cancellation_polls >= 5 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert_eq!(cancelled, Err("cancelled"));
        assert_eq!(cancellation_polls, 5);
    }

    #[test]
    fn cancellable_terminology_scan_stops_between_text_windows() {
        const WINDOW_BYTES: usize = 64 * 1024;
        let compiled = compile_terminology(vec![TerminologyEntry {
            term: "missing".to_owned(),
            translation: "缺失".to_owned(),
            triggers: vec!["needle".to_owned()],
        }])
        .expect("术语应该有效");
        let text = "x".repeat(WINDOW_BYTES * 4);
        let mut polls = 0_usize;

        let result = compiled.triggered_indices_with_cancellation([text.as_str()], || {
            polls += 1;
            if polls >= 4 { Err(()) } else { Ok(()) }
        });

        assert_eq!(result, Err(()));
        assert_eq!(polls, 4);
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

        let compile_started = Instant::now();
        let compiled = compile_terminology(definitions).expect("一万条字面 trigger 应可一次编译");
        let compile_elapsed = compile_started.elapsed();
        let scan_started = Instant::now();
        let matched = compiled.triggered_by(["前文 trigger-09999 中段 trigger-00007 后文"]);
        let scan_elapsed = scan_started.elapsed();

        assert_eq!(compiled.entries().len(), 10_000);
        assert_eq!(
            matched.iter().map(|entry| entry.term()).collect::<Vec<_>>(),
            ["term-00007", "term-09999"]
        );
        eprintln!(
            "typical terminology: compile={compile_elapsed:?}, scan={scan_elapsed:?}, patterns=10000"
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

    #[tokio::test]
    async fn terminology_larger_than_nine_mibibytes_crosses_production_read_and_prepare() {
        const TRANSLATION_BYTES: usize = 9 * 1024 * 1024 + 1;

        let directory = tempfile::tempdir().expect("应建立临时目录");
        let path = directory.path().join("large-terminology.toml");
        let mut file = File::create(&path).expect("应建立大术语文件");
        file.write_all(b"[[term]]\nterm = \"large-term\"\ntranslation = \"")
            .expect("应写入术语前缀");
        let chunk = vec![b'x'; 1024 * 1024];
        let mut remaining = TRANSLATION_BYTES;
        while remaining != 0 {
            let count = remaining.min(chunk.len());
            file.write_all(&chunk[..count]).expect("应写入术语正文");
            remaining -= count;
        }
        file.write_all(b"\"\n").expect("应写入术语后缀");
        file.sync_all().expect("应完整落盘测试输入");
        drop(file);
        assert!(std::fs::metadata(&path).expect("应读取文件元数据").len() > 9 * 1024 * 1024);

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("生产文件系统应启动");
        let cpu = RayonCpuExecutor::start(CpuExecutorConfig::production())
            .expect("生产 CPU 执行器应启动");
        let service =
            TranslationPlanningResourceReadingService::new(file_system.clone(), cpu.clone());
        let resources = service
            .read(Some(path), None, "[]".to_owned(), "[]".to_owned())
            .await
            .expect("9 MiB 以上术语应通过生产读取、TOML 解析、规范编码和索引编译");

        assert_eq!(resources.terminology().entries().len(), 1);
        assert_eq!(
            resources.terminology().entries()[0].translation().len(),
            TRANSLATION_BYTES
        );
        assert!(resources.terminology_json.len() > 9 * 1024 * 1024);

        let canonical_snapshot = resources.terminology_json.clone();
        drop(resources);
        let restored = service
            .read(None, None, canonical_snapshot, "[]".to_owned())
            .await
            .expect("9 MiB 以上 canonical 术语应能从项目持久状态重新准备");
        assert_eq!(restored.terminology().entries().len(), 1);
        assert_eq!(
            restored.terminology().entries()[0].translation().len(),
            TRANSLATION_BYTES
        );
        assert!(restored.terminology_json.len() > 9 * 1024 * 1024);

        drop(service);
        file_system.shutdown().await.expect("文件系统应关闭");
        cpu.shutdown().expect("CPU 执行器应关闭");
    }
}
