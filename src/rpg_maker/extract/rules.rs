//! 人类可直接书写的 Rules TOML，以及从定义到可逆标准文本快照的完整编排。

mod definition;
mod matcher;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::Utf8Error;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream;
use serde_json::Value;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::{DataFileName, StandardDataFile};
use crate::storage::file_system::{FileReader, ReadFileError};

use self::definition::{FileRuleSource, RuleSource, RulesDefinition, RulesDefinitionError};
use self::matcher::{
    MatchedRuleTarget, RulesMatchError, RulesMatchInput, RulesPlugin, match_rule,
    merge_rule_matches,
};
use super::document::{
    PluginConfiguration, RpgMakerDocumentId, RpgMakerDocumentSelection,
    RpgMakerProjectDocumentReader, RpgMakerProjectDocuments,
};
use super::model::{ExtractedTextField, ExtractedTextGroup, RulesSnapshot, SnapshotModelError};
use super::store::RulesSnapshotStore;

/// 使用调用方提供的当前 Rules TOML 完整替换 Rules 提取快照。
pub(crate) trait RulesExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace(
        &self,
        project: &OpenedProject,
        rules_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 读取、解析、匹配并原子提交一次 Rules 快照。
pub(crate) struct RulesExtractionService<F, D, S, C> {
    file_reader: F,
    document_reader: D,
    snapshot_store: S,
    cpu_executor: C,
    config: RulesExtractionConfig,
}

impl<F, D, S, C> RulesExtractionService<F, D, S, C> {
    pub(crate) fn new(
        file_reader: F,
        document_reader: D,
        snapshot_store: S,
        cpu_executor: C,
        config: RulesExtractionConfig,
    ) -> Self {
        Self {
            file_reader,
            document_reader,
            snapshot_store,
            cpu_executor,
            config,
        }
    }
}

/// Rules 按规则扫描冻结来源时允许并行占用的 CPU 工作单元数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RulesExtractionConfig {
    scan_concurrency: NonZeroUsize,
}

impl RulesExtractionConfig {
    pub(crate) const fn new(scan_concurrency: NonZeroUsize) -> Self {
        Self { scan_concurrency }
    }

    pub(crate) const fn scan_concurrency(self) -> NonZeroUsize {
        self.scan_concurrency
    }
}

impl<F, D, S, C> RulesExtraction for RulesExtractionService<F, D, S, C>
where
    F: FileReader,
    D: RpgMakerProjectDocumentReader,
    S: RulesSnapshotStore,
    C: CpuTaskExecutor,
{
    type Error = RulesExtractionError<F::Error, D::Error, S::Error, C::Error>;

    async fn replace(
        &self,
        project: &OpenedProject,
        rules_path: PathBuf,
    ) -> Result<(), Self::Error> {
        let file = self
            .file_reader
            .read_file(rules_path.clone())
            .await
            .map_err(|source| RulesExtractionError::ReadRules {
                rules_path: rules_path.clone(),
                source,
            })?;
        let definition = self
            .cpu_executor
            .execute(move || parse_rules_definition(file.into_bytes()))
            .await
            .map_err(|source| RulesExtractionError::ParseDefinitionCompute {
                rules_path: rules_path.clone(),
                source,
            })?
            .map_err(|error| match error {
                ParseRulesDefinitionError::InvalidUtf8(source) => {
                    RulesExtractionError::InvalidUtf8 {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
                ParseRulesDefinitionError::InvalidDefinition(source) => {
                    RulesExtractionError::InvalidDefinition {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
            })?;

        if definition.is_empty() {
            self.snapshot_store
                .deactivate_rules(project)
                .await
                .map_err(|source| RulesExtractionError::Persist { rules_path, source })?;
            return Ok(());
        }

        let selection = document_selection(&definition);
        let documents = self
            .document_reader
            .read(project, selection)
            .await
            .map_err(|source| RulesExtractionError::ReadDocuments {
                rules_path: rules_path.clone(),
                source,
            })?;
        let input = build_match_input(&definition, documents).map_err(|source| {
            RulesExtractionError::InvalidTarget {
                rules_path: rules_path.clone(),
                source,
            }
        })?;

        let matches = self
            .match_rules_parallel(definition, input)
            .await
            .map_err(|error| match error {
                ParallelRulesBuildError::MatchCompute(source) => {
                    RulesExtractionError::MatchSourceCompute {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
                ParallelRulesBuildError::FinalizeCompute(source) => {
                    RulesExtractionError::BuildSnapshotCompute {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
                ParallelRulesBuildError::Match(source) => RulesExtractionError::InvalidTarget {
                    rules_path: rules_path.clone(),
                    source,
                },
                ParallelRulesBuildError::Snapshot(source) => {
                    RulesExtractionError::InvalidSnapshot {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
            })?;

        self.snapshot_store
            .replace_rules(project, matches)
            .await
            .map_err(|source| RulesExtractionError::Persist { rules_path, source })
    }
}

impl<F, D, S, C> RulesExtractionService<F, D, S, C>
where
    C: CpuTaskExecutor,
{
    async fn match_rules_parallel(
        &self,
        definition: RulesDefinition,
        input: RulesMatchInput,
    ) -> Result<RulesSnapshot, ParallelRulesBuildError<C::Error>> {
        let input = Arc::new(input);
        let concurrency = self.config.scan_concurrency().get();
        let work = stream::iter(definition.into_rules().into_iter().map(|rule| {
            let input = Arc::clone(&input);
            async move {
                self.cpu_executor
                    .execute(move || match_rule(&rule, &input))
                    .await
                    .map_err(ParallelRulesBuildError::MatchCompute)?
                    .map_err(ParallelRulesBuildError::Match)
            }
        }))
        .buffered(concurrency);
        futures_util::pin_mut!(work);

        let mut completed = Vec::new();
        while let Some(result) = work.next().await {
            completed.push(result?);
        }

        self.cpu_executor
            .execute(move || {
                let targets =
                    merge_rule_matches(completed).map_err(ParallelRulesBuildError::Match)?;
                snapshot_from_targets(targets).map_err(ParallelRulesBuildError::Snapshot)
            })
            .await
            .map_err(ParallelRulesBuildError::FinalizeCompute)?
    }
}

fn parse_rules_definition(bytes: Vec<u8>) -> Result<RulesDefinition, ParseRulesDefinitionError> {
    let text = String::from_utf8(bytes)
        .map_err(|source| ParseRulesDefinitionError::InvalidUtf8(source.utf8_error()))?;
    RulesDefinition::parse(&text).map_err(ParseRulesDefinitionError::InvalidDefinition)
}

fn document_selection(definition: &RulesDefinition) -> RpgMakerDocumentSelection {
    let mut selection = RpgMakerDocumentSelection::empty();
    for rule in definition.rules() {
        match rule.source() {
            RuleSource::File(FileRuleSource::AllMaps) => selection.request_all_maps(),
            RuleSource::File(FileRuleSource::Exact(file)) => {
                select_exact_data_file(&mut selection, file)
            }
            RuleSource::Plugin(_) => selection.request_plugins(),
            RuleSource::Command { .. } => {
                selection.insert_standard_file(StandardDataFile::CommonEvents);
                selection.insert_standard_file(StandardDataFile::Troops);
                selection.request_all_maps();
            }
        }
    }
    selection
}

fn select_exact_data_file(selection: &mut RpgMakerDocumentSelection, file: &str) {
    if let Some(standard) = StandardDataFile::from_file_name(file) {
        selection.insert_standard_file(standard);
    } else if let Some(map_id) = canonical_map_id(file) {
        selection.insert_map(map_id);
    } else {
        let file = DataFileName::parse(file.to_owned())
            .expect("Rules 定义解析已经校验安全的精确 JSON 基名");
        selection.insert_data_file(file);
    }
}

fn build_match_input(
    definition: &RulesDefinition,
    documents: RpgMakerProjectDocuments,
) -> Result<RulesMatchInput, RulesMatchError> {
    let plugin_rules = definition
        .rules()
        .iter()
        .filter_map(|rule| match rule.source() {
            RuleSource::Plugin(name) => Some((name.clone(), rule.rule_number())),
            RuleSource::File(_) | RuleSource::Command { .. } => None,
        })
        .fold(
            BTreeMap::<String, usize>::new(),
            |mut rules, (name, number)| {
                rules.entry(name).or_insert(number);
                rules
            },
        );
    let (documents, plugins) = documents.into_parts();
    let files = documents
        .into_iter()
        .map(|(id, value)| (document_file_name(id), value))
        .collect();
    let plugins = plugins
        .into_iter()
        .map(|plugin| plugin_for_rules(plugin, &plugin_rules))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(RulesMatchInput::new(files, plugins))
}

fn document_file_name(id: RpgMakerDocumentId) -> String {
    match id {
        RpgMakerDocumentId::Data(file) => file.file_name().to_owned(),
        RpgMakerDocumentId::DataFile(file) => file.as_str().to_owned(),
        RpgMakerDocumentId::Map(map_id) => format!("Map{map_id:03}.json"),
    }
}

fn plugin_for_rules(
    plugin: PluginConfiguration,
    rules: &BTreeMap<String, usize>,
) -> Result<Option<RulesPlugin>, RulesMatchError> {
    let (index, fields) = plugin.into_parts();
    let Some(name) = fields.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(rule_number) = rules.get(name).copied() else {
        return Ok(None);
    };
    let enabled = fields
        .get("status")
        .and_then(Value::as_bool)
        .ok_or_else(|| RulesMatchError::InvalidTarget {
            rule_number,
            message: format!("插件 {name:?} 的 status 必须是布尔值"),
        })?;
    let parameters = if enabled {
        fields
            .get("parameters")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| RulesMatchError::InvalidTarget {
                rule_number,
                message: format!("插件 {name:?} 的 parameters 必须是对象"),
            })?
    } else {
        Default::default()
    };
    Ok(Some(RulesPlugin::new(index, name, enabled, parameters)))
}

fn snapshot_from_targets(
    targets: Vec<MatchedRuleTarget>,
) -> Result<RulesSnapshot, SnapshotModelError> {
    let mut groups = Vec::with_capacity(targets.len());
    for target in targets {
        let physical_location = target
            .physical_location()
            .expect("匹配器只会产生已通过 Rules 定义校验的来源");
        let fields = target
            .leaves()
            .iter()
            .enumerate()
            .map(|(leaf_index, leaf)| {
                ExtractedTextField::projected(
                    target.role_for(leaf_index),
                    physical_location.clone(),
                    leaf.original_text(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let recipe = target
            .projection_recipe()
            .expect("匹配器已经校验物化配方可以逐字重建最终字符串");
        groups.push(ExtractedTextGroup::projected(
            target.kind(),
            target
                .group_location()
                .expect("匹配器只会产生已通过 Rules 定义校验的来源"),
            fields,
            vec![recipe],
        )?);
    }
    RulesSnapshot::new(groups)
}

fn canonical_map_id(file: &str) -> Option<u32> {
    let digits = file.strip_prefix("Map")?.strip_suffix(".json")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse::<u32>().ok()?;
    (format!("Map{id:03}.json") == file).then_some(id)
}

#[derive(Debug)]
enum ParseRulesDefinitionError {
    InvalidUtf8(Utf8Error),
    InvalidDefinition(RulesDefinitionError),
}

enum ParallelRulesBuildError<CE> {
    MatchCompute(CpuTaskExecutionError<CE>),
    FinalizeCompute(CpuTaskExecutionError<CE>),
    Match(RulesMatchError),
    Snapshot(SnapshotModelError),
}

/// Rules 提取在自身职责边界产生的阶段错误。
#[derive(Debug)]
pub(crate) enum RulesExtractionError<FE, DE, SE, CE> {
    ReadRules {
        rules_path: PathBuf,
        source: ReadFileError<FE>,
    },
    InvalidUtf8 {
        rules_path: PathBuf,
        source: Utf8Error,
    },
    InvalidDefinition {
        rules_path: PathBuf,
        source: RulesDefinitionError,
    },
    ParseDefinitionCompute {
        rules_path: PathBuf,
        source: CpuTaskExecutionError<CE>,
    },
    ReadDocuments {
        rules_path: PathBuf,
        source: DE,
    },
    InvalidTarget {
        rules_path: PathBuf,
        source: RulesMatchError,
    },
    InvalidSnapshot {
        rules_path: PathBuf,
        source: SnapshotModelError,
    },
    MatchSourceCompute {
        rules_path: PathBuf,
        source: CpuTaskExecutionError<CE>,
    },
    BuildSnapshotCompute {
        rules_path: PathBuf,
        source: CpuTaskExecutionError<CE>,
    },
    Persist {
        rules_path: PathBuf,
        source: SE,
    },
}

impl<FE, DE, SE, CE> fmt::Display for RulesExtractionError<FE, DE, SE, CE>
where
    FE: Error,
    DE: Error,
    SE: Error,
    CE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadRules { rules_path, source } => {
                write!(
                    formatter,
                    "读取 Rules TOML 失败 {}：{source}",
                    rules_path.display()
                )
            }
            Self::InvalidUtf8 { rules_path, source } => write!(
                formatter,
                "Rules TOML 不是有效 UTF-8 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidDefinition { rules_path, source } => write!(
                formatter,
                "Rules TOML 定义无效 {}：{source}",
                rules_path.display()
            ),
            Self::ParseDefinitionCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules TOML 解析失败 {}：{source}",
                rules_path.display()
            ),
            Self::ReadDocuments { rules_path, source } => write!(
                formatter,
                "读取 Rules 所需 RPG Maker 文档失败 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidTarget { rules_path, source } => write!(
                formatter,
                "Rules 匹配失败 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidSnapshot { rules_path, source } => write!(
                formatter,
                "Rules 快照无效 {}：{source}",
                rules_path.display()
            ),
            Self::MatchSourceCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 来源匹配失败 {}：{source}",
                rules_path.display()
            ),
            Self::BuildSnapshotCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 快照汇总失败 {}：{source}",
                rules_path.display()
            ),
            Self::Persist { rules_path, source } => {
                write!(
                    formatter,
                    "保存 Rules 快照失败 {}：{source}",
                    rules_path.display()
                )
            }
        }
    }
}

impl<FE, DE, SE, CE> Error for RulesExtractionError<FE, DE, SE, CE>
where
    FE: Error + 'static,
    DE: Error + 'static,
    SE: Error + 'static,
    CE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadRules { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidDefinition { source, .. } => Some(source),
            Self::ParseDefinitionCompute { source, .. }
            | Self::MatchSourceCompute { source, .. }
            | Self::BuildSnapshotCompute { source, .. } => Some(source),
            Self::ReadDocuments { source, .. } => Some(source),
            Self::InvalidTarget { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            Self::Persist { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{DirectTextPart, TextProjectionRecipe};
    use crate::storage::file_system::ReadFile;

    #[test]
    fn selection_requests_only_declared_sources_and_builtin_event_documents() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Disciplines.json"
path = '[].Name'

[[rule]]
file = "Map042.json"
path = 'displayName'

[[rule]]
plugin = "Quest"
path = 'Title'

[[rule]]
code = 356
parameter = 0
pattern = '\A(?<text>.+)\z'
"#,
        )
        .expect("规则应合法");

        let selection = document_selection(&definition);

        assert!(selection.includes_all_maps());
        assert!(selection.includes_plugins());
        assert!(
            selection
                .standard_files()
                .contains(&StandardDataFile::CommonEvents)
        );
        assert!(
            selection
                .standard_files()
                .contains(&StandardDataFile::Troops)
        );
        assert!(selection.map_ids().contains(&42));
        assert!(
            selection
                .data_files()
                .iter()
                .any(|file| file.as_str() == "Disciplines.json")
        );
    }

    #[test]
    fn matched_regex_slots_become_one_direct_recipe_and_multiple_logical_leaves() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].note'
pattern = '<x>(?<text>.*?)</x>'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Items.json".to_owned(),
                json!([null, {"note":"<x>甲</x><x>乙</x>"}]),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("规则应命中");

        let snapshot = snapshot_from_targets(targets).expect("投影应形成快照");

        let groups = snapshot.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].fields().len(), 2);
        assert_eq!(groups[0].mutation_targets().len(), 1);
        let TextProjectionRecipe::Direct(recipe) = &groups[0].recipes()[0] else {
            panic!("Rules 局部文本必须生成直接配方")
        };
        assert_eq!(
            recipe
                .parts()
                .iter()
                .filter(|part| matches!(part, DirectTextPart::TextSlot { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn rules_for_fields_of_one_array_entry_form_one_logical_group() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'

[[rule]]
file = "Items.json"
path = '[].description'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Items.json".to_owned(),
                json!([null, {"name":"药草", "description":"恢复少量生命"}]),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("两条规则都应命中");

        let snapshot = snapshot_from_targets(targets).expect("同一数据库条目应合并为复合文本组");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].fields().len(), 2);
        assert_eq!(snapshot.groups()[0].mutation_targets().len(), 2);
        assert_eq!(snapshot.groups()[0].recipes().len(), 2);
    }

    #[test]
    fn terminal_parent_prevents_unrelated_nested_objects_from_sharing_a_group() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'menu.title'

[[rule]]
file = "Custom.json"
path = 'quest.body'

[[rule]]
file = "Custom.json"
path = 'entries[0].left.Name'

[[rule]]
file = "Custom.json"
path = 'entries[0].right.Name'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Custom.json".to_owned(),
                json!({
                    "menu": {"title": "菜单"},
                    "quest": {"body": "任务正文"},
                    "entries": [{
                        "left": {"Name": "左"},
                        "right": {"Name": "右"}
                    }]
                }),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("四条规则都应命中");

        let snapshot = snapshot_from_targets(targets).expect("终点父容器应形成稳定组边界");

        assert_eq!(snapshot.groups().len(), 4);
        assert!(
            snapshot
                .groups()
                .iter()
                .all(|group| group.fields().len() == 1)
        );
    }

    #[test]
    fn zero_byte_and_comment_only_are_invalid_but_explicit_empty_deactivates() {
        for bytes in [Vec::new(), b"# comment only\n".to_vec()] {
            assert!(matches!(
                parse_rules_definition(bytes),
                Err(ParseRulesDefinitionError::InvalidDefinition(_))
            ));
        }
        assert!(
            parse_rules_definition(b"rule = []".to_vec())
                .expect("显式空集合应合法")
                .is_empty()
        );
    }

    #[test]
    fn config_keeps_explicit_scan_limit() {
        let concurrency = NonZeroUsize::new(4).expect("并发上限应非零");
        assert_eq!(
            RulesExtractionConfig::new(concurrency).scan_concurrency(),
            concurrency
        );
    }

    #[tokio::test]
    async fn failed_candidate_never_replaces_or_deactivates_the_previous_snapshot() {
        let state = Arc::new(StoreState::default());
        let service = test_service(
            br#"
[[rule]]
file = "Items.json"
path = '[].name'
"#
            .to_vec(),
            RpgMakerProjectDocuments::new(
                BTreeMap::from([(
                    RpgMakerDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"name":"   "}]),
                )]),
                Vec::new(),
            ),
            Arc::clone(&state),
        );

        let error = service
            .replace(&project(), PathBuf::from("rules.toml"))
            .await
            .expect_err("零个非空翻译叶必须放弃整个替换");

        assert!(matches!(
            error,
            RulesExtractionError::InvalidTarget {
                source: RulesMatchError::NoNonBlankMatch { rule_number: 1 },
                ..
            }
        ));
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicit_empty_deactivates_without_reading_project_documents() {
        let state = Arc::new(StoreState::default());
        let document_reads = Arc::new(AtomicUsize::new(0));
        let service = RulesExtractionService::new(
            FakeFileReader {
                bytes: b"rule = []".to_vec(),
            },
            FakeDocumentReader {
                documents: RpgMakerProjectDocuments::empty(),
                reads: Arc::clone(&document_reads),
            },
            FakeStore {
                state: Arc::clone(&state),
            },
            InlineCpu,
            RulesExtractionConfig::new(NonZeroUsize::new(2).expect("并发数应非零")),
        );

        service
            .replace(&project(), PathBuf::from("rules.toml"))
            .await
            .expect("显式空集合应停用 Rules owner");

        assert_eq!(document_reads.load(Ordering::SeqCst), 0);
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 1);
    }

    fn test_service(
        bytes: Vec<u8>,
        documents: RpgMakerProjectDocuments,
        state: Arc<StoreState>,
    ) -> RulesExtractionService<FakeFileReader, FakeDocumentReader, FakeStore, InlineCpu> {
        RulesExtractionService::new(
            FakeFileReader { bytes },
            FakeDocumentReader {
                documents,
                reads: Arc::new(AtomicUsize::new(0)),
            },
            FakeStore { state },
            InlineCpu,
            RulesExtractionConfig::new(NonZeroUsize::new(2).expect("并发数应非零")),
        )
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "rules-test".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/att/projects/rules-test"),
            PathBuf::from("C:/att/projects/rules-test/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试错误")
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeFileReader {
        bytes: Vec<u8>,
    }

    impl FileReader for FakeFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            Ok(ReadFile::new(path, self.bytes.clone()))
        }
    }

    #[derive(Clone)]
    struct FakeDocumentReader {
        documents: RpgMakerProjectDocuments,
        reads: Arc<AtomicUsize>,
    }

    impl RpgMakerProjectDocumentReader for FakeDocumentReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            _selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.documents.clone())
        }
    }

    #[derive(Default)]
    struct StoreState {
        replacements: AtomicUsize,
        deactivations: AtomicUsize,
    }

    #[derive(Clone)]
    struct FakeStore {
        state: Arc<StoreState>,
    }

    impl RulesSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_rules(
            &self,
            _project: &OpenedProject,
            _snapshot: RulesSnapshot,
        ) -> Result<(), Self::Error> {
            self.state.replacements.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn deactivate_rules(&self, _project: &OpenedProject) -> Result<(), Self::Error> {
            self.state.deactivations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct InlineCpu;

    impl CpuTaskExecutor for InlineCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }
}
