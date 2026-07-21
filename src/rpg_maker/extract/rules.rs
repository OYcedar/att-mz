//! 人类可直接书写的 Rules TOML，以及从定义到可逆标准文本快照的完整编排。

mod definition;
mod matcher;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::str::Utf8Error;
use std::sync::Arc;

use serde_json::Value;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::{DataFileName, StandardDataFile};

use self::definition::{FileRuleSource, RuleSource, RulesDefinition, RulesDefinitionError};
use self::matcher::{
    MatchedRuleTarget, RulesMatchError, RulesMatchInput, RulesPlugin, match_rule,
    merge_rule_matches,
};
use super::document::{
    DocumentReadProgress, PluginConfiguration, RpgMakerDocumentId, RpgMakerDocumentSelection,
    RpgMakerProjectDocumentReader, RpgMakerProjectDocuments,
};
use super::model::{ExtractedTextGroup, ExtractedTextUnit, RulesSnapshot, SnapshotModelError};
use super::store::RulesSnapshotStore;
use super::{ExtractProgress, ExtractProgressPhase};

/// 使用调用方提供的当前 Rules TOML 完整替换 Rules 提取快照。
pub(crate) trait RulesExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace(
        &self,
        project: &OpenedProject,
        program: RulesProgram,
        progress: ExtractProgress,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 已读取、验证并规范编码的 Extract Rules 程序。
///
/// 显式文件和项目状态复用都在进入业务服务前建立该值；复用路径因此不会重新读取
/// 原 TOML 文件。`diagnostic_path` 只用于人类诊断，不参与规则语义。
#[derive(Clone, Debug)]
pub(crate) struct RulesProgram {
    diagnostic_path: PathBuf,
    definition: RulesDefinition,
}

impl RulesProgram {
    pub(crate) fn from_toml(
        diagnostic_path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, RulesProgramError> {
        let text = String::from_utf8(bytes)
            .map_err(|source| RulesProgramError::InvalidUtf8(source.utf8_error()))?;
        let definition =
            RulesDefinition::parse(&text).map_err(RulesProgramError::InvalidDefinition)?;
        Ok(Self {
            diagnostic_path,
            definition,
        })
    }

    pub(crate) fn from_canonical_json(
        diagnostic_path: PathBuf,
        canonical_json: &str,
    ) -> Result<Self, RulesProgramError> {
        let definition = RulesDefinition::parse_canonical_json(canonical_json)
            .map_err(RulesProgramError::InvalidDefinition)?;
        Ok(Self {
            diagnostic_path,
            definition,
        })
    }

    pub(crate) fn diagnostic_path(&self) -> &std::path::Path {
        &self.diagnostic_path
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.definition.is_empty()
    }

    pub(crate) fn canonical_json(&self) -> &str {
        self.definition.canonical_json()
    }
}

/// Rules 程序在用户输入/项目状态边界无法建立。
#[derive(Debug)]
pub(crate) enum RulesProgramError {
    InvalidUtf8(Utf8Error),
    InvalidDefinition(RulesDefinitionError),
}

impl fmt::Display for RulesProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(source) => write!(formatter, "Rules 定义不是 UTF-8：{source}"),
            Self::InvalidDefinition(source) => source.fmt(formatter),
        }
    }
}

impl Error for RulesProgramError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(source) => Some(source),
            Self::InvalidDefinition(source) => Some(source),
        }
    }
}

/// 在项目数据库边界确认 canonical Rules 仍满足当前来源、路径与 PCRE2 语义。
pub(crate) fn validate_rules_canonical_json(source: &str) -> Result<(), RulesProgramError> {
    RulesDefinition::parse_canonical_json(source)
        .map(|_| ())
        .map_err(RulesProgramError::InvalidDefinition)
}

/// 读取、解析、匹配并原子提交一次 Rules 快照。
pub(crate) struct RulesExtractionService<D, S, C> {
    document_reader: D,
    snapshot_store: S,
    cpu_executor: C,
}

impl<D, S, C> RulesExtractionService<D, S, C> {
    pub(crate) fn new(document_reader: D, snapshot_store: S, cpu_executor: C) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
        }
    }
}

impl<D, S, C> RulesExtraction for RulesExtractionService<D, S, C>
where
    D: RpgMakerProjectDocumentReader,
    S: RulesSnapshotStore,
    C: CpuTaskExecutor,
{
    type Error = RulesExtractionError<D::Error, S::Error, C::Error>;

    async fn replace(
        &self,
        project: &OpenedProject,
        program: RulesProgram,
        progress: ExtractProgress,
    ) -> Result<(), Self::Error> {
        let rules_path = program.diagnostic_path().to_path_buf();
        let definition = program.definition;

        if definition.is_empty() {
            progress.indeterminate(ExtractProgressPhase::RulesCommit);
            self.snapshot_store
                .deactivate_rules(project)
                .await
                .map_err(|source| RulesExtractionError::Persist { rules_path, source })?;
            return Ok(());
        }

        let selection = document_selection(&definition);
        let documents = self
            .document_reader
            .read_with_progress(
                project,
                selection,
                DocumentReadProgress::new({
                    let progress = progress.clone();
                    move |completed, total| {
                        progress.determinate(
                            ExtractProgressPhase::RulesDocuments,
                            completed,
                            total,
                        );
                    }
                }),
            )
            .await
            .map_err(|source| RulesExtractionError::ReadDocuments {
                rules_path: rules_path.clone(),
                source,
            })?;
        let (definition, input) = self
            .cpu_executor
            .execute(move || {
                let input = build_match_input(&definition, documents)?;
                Ok::<_, RulesMatchError>((definition, input))
            })
            .await
            .map_err(|source| RulesExtractionError::MatchSourceCompute {
                rules_path: rules_path.clone(),
                source,
            })?
            .map_err(|source| RulesExtractionError::InvalidTarget {
                rules_path: rules_path.clone(),
                source,
            })?;

        let matches = self
            .match_rules_parallel(definition, input, progress.clone())
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

        progress.indeterminate(ExtractProgressPhase::RulesCommit);
        self.snapshot_store
            .replace_rules(project, matches)
            .await
            .map_err(|source| RulesExtractionError::Persist { rules_path, source })
    }
}

impl<D, S, C> RulesExtractionService<D, S, C>
where
    C: CpuTaskExecutor,
{
    async fn match_rules_parallel(
        &self,
        definition: RulesDefinition,
        input: RulesMatchInput,
        progress: ExtractProgress,
    ) -> Result<RulesSnapshot, ParallelRulesBuildError<C::Error>> {
        let input = Arc::new(input);
        let merge_input = Arc::clone(&input);
        let rules = definition.into_rules();
        let total = u64::try_from(rules.len()).expect("Rules 规则数必须能用 u64 表达");
        progress.determinate(ExtractProgressPhase::RulesMatches, 0, total);
        let completed = self
            .cpu_executor
            .execute_ordered_map_observed(rules, move |rule| match_rule(&rule, &input), {
                let progress = progress.clone();
                move |completed| {
                    progress.determinate(ExtractProgressPhase::RulesMatches, completed, total);
                }
            })
            .await
            .map_err(ParallelRulesBuildError::MatchCompute)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(ParallelRulesBuildError::Match)?;

        self.cpu_executor
            .execute(move || {
                let targets = merge_rule_matches(completed, &merge_input)
                    .map_err(ParallelRulesBuildError::Match)?;
                snapshot_from_targets(targets).map_err(ParallelRulesBuildError::Snapshot)
            })
            .await
            .map_err(ParallelRulesBuildError::FinalizeCompute)?
    }
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
    let file =
        DataFileName::parse(file.to_owned()).expect("Rules 定义解析已经校验安全的精确 JSON 基名");
    selection.insert_data_file(file);
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
        .collect::<Vec<_>>();
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
        RpgMakerDocumentId::Map(map_id) => map_id.file_name(),
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
        let units = target
            .units()
            .iter()
            .enumerate()
            .map(|(unit_index, unit)| {
                ExtractedTextUnit::projected(
                    target.role_for(unit_index),
                    physical_location.clone(),
                    TextUnitContent::Value(unit.source_text().to_owned()),
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
            units,
            vec![recipe],
        )?);
    }
    RulesSnapshot::new(groups)
}

enum ParallelRulesBuildError<CE> {
    MatchCompute(CpuTaskExecutionError<CE>),
    FinalizeCompute(CpuTaskExecutionError<CE>),
    Match(RulesMatchError),
    Snapshot(SnapshotModelError),
}

/// Rules 提取在自身职责边界产生的阶段错误。
#[derive(Debug)]
pub(crate) enum RulesExtractionError<DE, SE, CE> {
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

impl<DE, SE, CE> fmt::Display for RulesExtractionError<DE, SE, CE>
where
    DE: Error,
    SE: Error,
    CE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl<DE, SE, CE> Error for RulesExtractionError<DE, SE, CE>
where
    DE: Error + 'static,
    SE: Error + 'static,
    CE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MatchSourceCompute { source, .. } | Self::BuildSnapshotCompute { source, .. } => {
                Some(source)
            }
            Self::ReadDocuments { source, .. } => Some(source),
            Self::InvalidTarget { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            Self::Persist { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Map, json};

    use super::*;
    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::progress::{ProgressObserver, ProgressSnapshot};
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{
        DirectTextPart, DirectTextRecipe, TextProjectionRecipe, TextUnitRole,
    };
    use crate::rpg_maker::text::MapId;

    #[derive(Clone, Default)]
    struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<ExtractProgressPhase>>>>);

    impl ProgressObserver<ExtractProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<ExtractProgressPhase>) {
            self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
        }
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<ExtractProgressPhase>> {
            self.0.lock().expect("进度记录锁不应中毒").clone()
        }
    }

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
file = "Map000.json"
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
        assert!(selection.map_ids().contains(&MapId::new(42).unwrap()));
        assert!(
            selection
                .data_files()
                .iter()
                .any(|file| file.as_str() == "Disciplines.json")
        );
        assert!(
            selection
                .data_files()
                .iter()
                .any(|file| file.as_str() == "Map000.json")
        );
    }

    #[test]
    fn rules_targets_follow_canonical_cross_source_order() {
        assert_eq!(
            StandardDataFile::ALL.map(StandardDataFile::file_name),
            [
                "Actors.json",
                "Animations.json",
                "Armors.json",
                "Classes.json",
                "CommonEvents.json",
                "Enemies.json",
                "Items.json",
                "MapInfos.json",
                "Skills.json",
                "States.json",
                "System.json",
                "Tilesets.json",
                "Troops.json",
                "Weapons.json",
            ],
            "标准 DataFile 的固定顺序是 Rules canonical 来源顺序的一部分"
        );
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
plugin = "Quest"
path = 'alpha'

[[rule]]
file = "Map010.json"
path = 'displayName'

[[rule]]
file = "Zulu.json"
path = 'text'

[[rule]]
file = "Items.json"
path = '[].name'

[[rule]]
plugin = "Earlier"
path = 'only'

[[rule]]
file = "Map002.json"
path = 'displayName'

[[rule]]
file = "AlphaCustom.json"
path = 'text'

[[rule]]
file = "Actors.json"
path = '[].name'

[[rule]]
plugin = "Quest"
path = 'zeta'
"#,
        )
        .expect("跨来源排序规则应合法");

        let mut quest_parameters = Map::new();
        quest_parameters.insert("zeta".to_owned(), json!("插件后声明字母"));
        quest_parameters.insert("alpha".to_owned(), json!("插件先声明字母"));
        let mut quest = Map::new();
        quest.insert("name".to_owned(), json!("Quest"));
        quest.insert("status".to_owned(), json!(true));
        quest.insert("parameters".to_owned(), Value::Object(quest_parameters));

        let mut earlier = Map::new();
        earlier.insert("name".to_owned(), json!("Earlier"));
        earlier.insert("status".to_owned(), json!(true));
        earlier.insert("parameters".to_owned(), json!({"only":"较早插件"}));

        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Map(MapId::new(10).unwrap()),
                    json!({"displayName":"地图十"}),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("Zulu.json").expect("测试文件名应合法"),
                    ),
                    json!({"text":"自定义后"}),
                ),
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"name":"物品"}]),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("AlphaCustom.json").expect("测试文件名应合法"),
                    ),
                    json!({"text":"自定义前"}),
                ),
                (
                    RpgMakerDocumentId::Map(MapId::new(2).unwrap()),
                    json!({"displayName":"地图二"}),
                ),
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Actors),
                    json!([null, {"name":"角色"}]),
                ),
            ]),
            vec![
                PluginConfiguration::new(7, quest),
                PluginConfiguration::new(2, earlier),
            ],
        );
        let input = build_match_input(&definition, documents).expect("冻结来源应建立匹配输入");

        let targets = matcher::match_rules(&definition, &input).expect("全部规则都应命中");
        assert_eq!(
            targets
                .iter()
                .flat_map(MatchedRuleTarget::units)
                .map(|unit| unit.source_text())
                .collect::<Vec<_>>(),
            [
                "角色",
                "物品",
                "自定义前",
                "自定义后",
                "地图二",
                "地图十",
                "较早插件",
                "插件后声明字母",
                "插件先声明字母",
            ],
            "规则编号、调用方插入顺序和 OS 枚举都不得改变 canonical 来源顺序"
        );
    }

    #[test]
    fn documented_extract_rules_match_frozen_sources_and_round_trip_materialized_recipes() {
        const EXAMPLE: &str = include_str!("../../../docs/rpg-maker/examples/extract-rules.toml");

        fn rebuild(recipe: &DirectTextRecipe, values: &BTreeMap<TextUnitRole, String>) -> String {
            let mut output = String::new();
            for part in recipe.parts() {
                match part {
                    DirectTextPart::Literal(literal) => output.push_str(literal),
                    DirectTextPart::TextSlot { role } => output.push_str(
                        values
                            .get(role)
                            .expect("文档示例每个 recipe slot 都应有对应单元"),
                    ),
                    DirectTextPart::LineSlot { .. } => {
                        panic!("Extract Rules 的 Scalar recipe 不应产生 LineSlot")
                    }
                }
            }
            output
        }

        let definition = RulesDefinition::parse(EXAMPLE)
            .expect("完整 Extract Rules 示例必须通过生产解析与 PCRE2 编译边界");

        let plugin_entry = json!({"title":"插件标题"}).to_string();
        let encoded_plugin_entries = json!([plugin_entry]).to_string();
        let mut plugin_parameters = Map::new();
        plugin_parameters.insert("entries".to_owned(), Value::String(encoded_plugin_entries));
        let mut plugin = Map::new();
        plugin.insert("name".to_owned(), json!("QuestWindow"));
        plugin.insert("status".to_owned(), json!(true));
        plugin.insert("parameters".to_owned(), Value::Object(plugin_parameters));

        let encoded_final_title = serde_json::to_string("终点标题").unwrap();
        let encoded_title_object = json!({"title":encoded_final_title}).to_string();
        let encoded_payload_object = json!({"payload":encoded_title_object}).to_string();
        let encoded_empty_key_root = json!({"":encoded_payload_object}).to_string();
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                    json!([
                        null,
                        {
                            "list": [
                                {"code":356,"parameters":["DisplayNotice 出航命令"]},
                                {"code":357,"parameters":["QuestWindow","Show","",encoded_empty_key_root]}
                            ]
                        }
                    ]),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("QuestEntries.json").expect("示例自定义文件名应合法"),
                    ),
                    json!([{"title":"委托标题"}]),
                ),
            ]),
            vec![PluginConfiguration::new(4, plugin)],
        );
        let input = build_match_input(&definition, documents).expect("冻结来源应建立匹配输入");
        let targets = matcher::match_rules(&definition, &input)
            .expect("完整示例的四条规则都应命中代表性冻结来源");

        assert_eq!(targets.len(), 4);
        assert!(targets.iter().all(|target| target.units().len() == 1));
        assert_eq!(
            targets
                .iter()
                .map(|target| target.units()[0].source_text())
                .collect::<Vec<_>>(),
            ["出航命令", "终点标题", "委托标题", "插件标题"],
            "插件与 357 来源必须经过生产路径的逐层 JSON 解码"
        );

        let mut original_round_trips = Vec::new();
        let mut translated_round_trips = Vec::new();
        for (target_index, target) in targets.iter().enumerate() {
            let TextProjectionRecipe::Direct(recipe) = target
                .projection_recipe()
                .expect("匹配目标必须物化为 Direct recipe")
            else {
                panic!("Extract Rules 只应物化 Direct recipe")
            };
            let originals = target
                .units()
                .iter()
                .enumerate()
                .map(|(unit_index, unit)| {
                    (target.role_for(unit_index), unit.source_text().to_owned())
                })
                .collect::<BTreeMap<_, _>>();
            let translations = target
                .units()
                .iter()
                .enumerate()
                .map(|(unit_index, _)| {
                    (
                        target.role_for(unit_index),
                        format!("译文{}", target_index + 1),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            let original = rebuild(&recipe, &originals);
            assert_eq!(
                original,
                recipe.expected_raw(),
                "未翻译 recipe 必须逐字 round-trip"
            );
            original_round_trips.push(original);
            translated_round_trips.push(rebuild(&recipe, &translations));
        }

        assert_eq!(
            original_round_trips,
            ["DisplayNotice 出航命令", "终点标题", "委托标题", "插件标题",]
        );
        assert_eq!(
            translated_round_trips,
            ["DisplayNotice 译文1", "译文2", "译文3", "译文4",],
            "翻译后 recipe 必须只替换槽位并精确保留冻结外壳"
        );
    }

    #[test]
    fn matched_regex_slots_become_one_direct_recipe_and_multiple_logical_units() {
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
        assert_eq!(groups[0].units().len(), 2);
        assert_eq!(groups[0].mutation_claims().claims().len(), 1);
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
        assert_eq!(snapshot.groups()[0].units().len(), 2);
        assert_eq!(snapshot.groups()[0].mutation_claims().claims().len(), 2);
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
                .all(|group| group.units().len() == 1)
        );
    }

    #[test]
    fn zero_byte_and_comment_only_are_invalid_but_explicit_empty_deactivates() {
        for bytes in [Vec::new(), b"# comment only\n".to_vec()] {
            assert!(matches!(
                RulesProgram::from_toml(PathBuf::from("rules.toml"), bytes),
                Err(RulesProgramError::InvalidDefinition(_))
            ));
        }
        assert!(
            RulesProgram::from_toml(PathBuf::from("rules.toml"), b"rule = []".to_vec(),)
                .expect("显式空集合应合法")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_candidate_never_replaces_or_deactivates_the_previous_snapshot() {
        let state = Arc::new(StoreState::default());
        let (service, program) = test_service(
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
        let progress = RecordingProgress::default();

        let error = service
            .replace(&project(), program, ExtractProgress::new(progress.clone()))
            .await
            .expect_err("零个非空语义单元必须放弃整个替换");

        assert!(matches!(
            error,
            RulesExtractionError::InvalidTarget {
                source: RulesMatchError::NoNonBlankMatch { rule_number: 1 },
                ..
            }
        ));
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 0);
        assert_eq!(
            progress.snapshots(),
            [
                ProgressSnapshot::determinate(ExtractProgressPhase::RulesMatches, 0, 1),
                ProgressSnapshot::determinate(ExtractProgressPhase::RulesMatches, 1, 1),
            ]
        );
    }

    #[tokio::test]
    async fn explicit_empty_deactivates_without_reading_project_documents() {
        let state = Arc::new(StoreState::default());
        let document_reads = Arc::new(AtomicUsize::new(0));
        let service = RulesExtractionService::new(
            FakeDocumentReader {
                documents: RpgMakerProjectDocuments::empty(),
                reads: Arc::clone(&document_reads),
            },
            FakeStore {
                state: Arc::clone(&state),
            },
            InlineCpu,
        );
        let progress = RecordingProgress::default();

        service
            .replace(
                &project(),
                RulesProgram::from_toml(PathBuf::from("rules.toml"), b"rule = []".to_vec())
                    .expect("显式空定义应可建立"),
                ExtractProgress::new(progress.clone()),
            )
            .await
            .expect("显式空集合应停用 Rules owner");

        assert_eq!(document_reads.load(Ordering::SeqCst), 0);
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 1);
        assert_eq!(
            progress.snapshots(),
            [ProgressSnapshot::indeterminate(
                ExtractProgressPhase::RulesCommit
            )]
        );
    }

    fn test_service(
        bytes: Vec<u8>,
        documents: RpgMakerProjectDocuments,
        state: Arc<StoreState>,
    ) -> (
        RulesExtractionService<FakeDocumentReader, FakeStore, InlineCpu>,
        RulesProgram,
    ) {
        let program = RulesProgram::from_toml(PathBuf::from("rules.toml"), bytes)
            .expect("测试 Rules 应通过输入边界");
        let service = RulesExtractionService::new(
            FakeDocumentReader {
                documents,
                reads: Arc::new(AtomicUsize::new(0)),
            },
            FakeStore { state },
            InlineCpu,
        );
        (service, program)
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
