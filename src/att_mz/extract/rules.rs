//! 人类可直接书写的极简 Rules JSON，以及从规则到标准文本快照的完整编排。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::Utf8Error;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream;
use serde::Deserializer;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::att_mz::project::OpenedProject;
use crate::att_mz::tag::simple_tag_spans;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{FileReader, ReadFileError};

use super::document::{
    MzDocumentId, MzDocumentSelection, MzProjectDocumentReader, MzProjectDocuments,
    PluginConfiguration, StandardDataFile,
};
use super::model::{
    ExtractedTextField, ExtractedTextGroup, MzLocation, MzLocationStep, MzSource, RulesSnapshot,
    SnapshotModelError, TextGroupKind,
};
use super::store::RulesSnapshotStore;

/// 使用调用方提供的规则完整替换 Rules 提取快照。
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

/// Rules 阶段由外部明确提供的 CPU 来源扫描上限。
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
    D: MzProjectDocumentReader,
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

        let snapshot = if definition.is_empty() {
            RulesSnapshot::empty()
        } else {
            let documents = self
                .document_reader
                .read(project, definition.document_selection())
                .await
                .map_err(|source| RulesExtractionError::ReadDocuments {
                    rules_path: rules_path.clone(),
                    source,
                })?;
            match build_rules_snapshot_parallel(
                &self.cpu_executor,
                self.config,
                definition,
                documents,
            )
            .await
            {
                Ok(snapshot) => snapshot,
                Err(ParallelRulesBuildError::MatchCompute(source)) => {
                    return Err(RulesExtractionError::MatchSourceCompute { rules_path, source });
                }
                Err(ParallelRulesBuildError::FinalizeCompute(source)) => {
                    return Err(RulesExtractionError::BuildSnapshotCompute { rules_path, source });
                }
                Err(ParallelRulesBuildError::Build(error)) => {
                    return Err(RulesExtractionError::from_build(rules_path, error));
                }
            }
        };

        self.snapshot_store
            .replace_rules(project, snapshot)
            .await
            .map_err(|source| RulesExtractionError::Persist { rules_path, source })
    }
}

/// Rules 提取在自身职责边界内产生的阶段错误。
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
    NoMatch {
        rules_path: PathBuf,
        locator: String,
    },
    InvalidTarget {
        rules_path: PathBuf,
        locator: String,
        message: String,
    },
    DuplicateTarget {
        rules_path: PathBuf,
        location: MzLocation,
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

impl<FE, DE, SE, CE> RulesExtractionError<FE, DE, SE, CE> {
    fn from_build(rules_path: PathBuf, error: BuildRulesSnapshotError) -> Self {
        match error {
            BuildRulesSnapshotError::NoMatch { locator } => Self::NoMatch {
                rules_path,
                locator,
            },
            BuildRulesSnapshotError::InvalidTarget { locator, message }
            | BuildRulesSnapshotError::Model { locator, message } => Self::InvalidTarget {
                rules_path,
                locator,
                message,
            },
            BuildRulesSnapshotError::DuplicateTarget { location } => Self::DuplicateTarget {
                rules_path,
                location,
            },
        }
    }
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
                    "读取 Rules JSON 失败 {}：{source}",
                    rules_path.display()
                )
            }
            Self::InvalidUtf8 { rules_path, source } => write!(
                formatter,
                "Rules JSON 不是有效 UTF-8 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidDefinition { rules_path, source } => write!(
                formatter,
                "Rules JSON 定义无效 {}：{source}",
                rules_path.display()
            ),
            Self::ParseDefinitionCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 定义 CPU 解析失败 {}：{source}",
                rules_path.display()
            ),
            Self::ReadDocuments { rules_path, source } => write!(
                formatter,
                "读取 Rules 所需 MZ 文档失败 {}：{source}",
                rules_path.display()
            ),
            Self::NoMatch {
                rules_path,
                locator,
            } => write!(
                formatter,
                "Rules 定位没有命中任何非空文本：{locator}（{}）",
                rules_path.display()
            ),
            Self::InvalidTarget {
                rules_path,
                locator,
                message,
            } => write!(
                formatter,
                "Rules 定位命中了无效目标：{locator}：{message}（{}）",
                rules_path.display()
            ),
            Self::DuplicateTarget {
                rules_path,
                location,
            } => write!(
                formatter,
                "Rules 多次命中同一文本位置：{location}（{}）",
                rules_path.display()
            ),
            Self::MatchSourceCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 来源 CPU 匹配失败 {}：{source}",
                rules_path.display()
            ),
            Self::BuildSnapshotCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 快照 CPU 汇总失败 {}：{source}",
                rules_path.display()
            ),
            Self::Persist { rules_path, source } => write!(
                formatter,
                "保存 Rules 快照失败 {}：{source}",
                rules_path.display()
            ),
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
            Self::Persist { source, .. } => Some(source),
            Self::NoMatch { .. } | Self::InvalidTarget { .. } | Self::DuplicateTarget { .. } => {
                None
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RulesDefinitionError {
    InvalidJson(String),
    RootMustBeObject,
    UnknownSection(String),
    ExpectedObject(String),
    ExpectedArray(String),
    ExpectedString(String),
    EmptyValue(String),
    DuplicateValue { context: String, value: String },
    UnsupportedSource { source: String, guidance: String },
    InvalidPath { path: String, reason: String },
    MissingEventLists,
    EventListWithoutConsumer(String),
}

impl fmt::Display for RulesDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(message) => write!(formatter, "JSON 无效：{message}"),
            Self::RootMustBeObject => formatter.write_str("Rules JSON 根值必须是对象"),
            Self::UnknownSection(section) => write!(formatter, "未知顶层字段：{section}"),
            Self::ExpectedObject(context) => write!(formatter, "{context} 必须是对象"),
            Self::ExpectedArray(context) => write!(formatter, "{context} 必须是数组"),
            Self::ExpectedString(context) => write!(formatter, "{context} 必须是字符串"),
            Self::EmptyValue(context) => write!(formatter, "{context} 不能为空"),
            Self::DuplicateValue { context, value } => {
                write!(formatter, "{context} 包含重复值：{value}")
            }
            Self::UnsupportedSource { source, guidance } => {
                write!(formatter, "不支持的数据来源 {source}；{guidance}")
            }
            Self::InvalidPath { path, reason } => {
                write!(formatter, "路径 {path} 无效：{reason}")
            }
            Self::MissingEventLists => {
                formatter.write_str("plugin_commands 必须通过 event_lists 声明事件列表路径")
            }
            Self::EventListWithoutConsumer(locator) => {
                write!(
                    formatter,
                    "事件列表路径没有 Comment 标签或插件命令消费者：{locator}"
                )
            }
        }
    }
}

impl Error for RulesDefinitionError {}

enum ParseRulesDefinitionError {
    InvalidUtf8(Utf8Error),
    InvalidDefinition(RulesDefinitionError),
}

fn parse_rules_definition(bytes: Vec<u8>) -> Result<RulesDefinition, ParseRulesDefinitionError> {
    let text = String::from_utf8(bytes)
        .map_err(|source| ParseRulesDefinitionError::InvalidUtf8(source.utf8_error()))?;
    RulesDefinition::parse(&text).map_err(ParseRulesDefinitionError::InvalidDefinition)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuleDescriptor {
    label: String,
    field_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TagRule {
    id: usize,
    tag_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathRule {
    id: usize,
    path: CompiledPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TaggedPathRule {
    label: String,
    path: CompiledPath,
    tags: Vec<TagRule>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum DocumentRuleSource {
    Data(StandardDataFile),
    AllMaps,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RulesDefinition {
    descriptors: Vec<RuleDescriptor>,
    notes: BTreeMap<DocumentRuleSource, Vec<TaggedPathRule>>,
    event_lists: BTreeMap<DocumentRuleSource, Vec<TaggedPathRule>>,
    plugin_parameters: BTreeMap<String, Vec<PathRule>>,
    plugin_commands: BTreeMap<String, BTreeMap<String, Vec<PathRule>>>,
    standard_fields: BTreeMap<DocumentRuleSource, Vec<PathRule>>,
}

impl RulesDefinition {
    fn parse(text: &str) -> Result<Self, RulesDefinitionError> {
        let value = parse_strict_json(text)?;
        let root = value
            .as_object()
            .ok_or(RulesDefinitionError::RootMustBeObject)?;
        for section in root.keys() {
            if ![
                "notes",
                "event_lists",
                "plugin_parameters",
                "plugin_commands",
                "standard_fields",
            ]
            .contains(&section.as_str())
            {
                return Err(RulesDefinitionError::UnknownSection(section.clone()));
            }
        }

        let mut definition = Self::default();
        definition.notes = definition.parse_tagged_path_section(root, "notes", false)?;
        definition.event_lists = definition.parse_tagged_path_section(root, "event_lists", true)?;
        definition.plugin_parameters = definition.parse_plugin_parameter_section(root)?;
        definition.plugin_commands = definition.parse_plugin_command_section(root)?;
        definition.standard_fields = definition.parse_path_section(root, "standard_fields")?;
        definition.validate_event_list_consumers()?;
        Ok(definition)
    }

    fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    fn document_selection(&self) -> MzDocumentSelection {
        let mut selection = MzDocumentSelection::empty();
        for source in self
            .notes
            .keys()
            .chain(self.event_lists.keys())
            .chain(self.standard_fields.keys())
        {
            match source {
                DocumentRuleSource::Data(file) => selection.insert_standard_file(*file),
                DocumentRuleSource::AllMaps => selection.request_all_maps(),
            }
        }
        if !self.plugin_parameters.is_empty() {
            selection.request_plugins();
        }
        selection
    }

    fn add_descriptor(&mut self, label: String, field_name: String) -> usize {
        let id = self.descriptors.len();
        self.descriptors.push(RuleDescriptor { label, field_name });
        id
    }

    fn parse_tagged_path_section(
        &mut self,
        root: &Map<String, Value>,
        section: &str,
        allow_empty_tags: bool,
    ) -> Result<BTreeMap<DocumentRuleSource, Vec<TaggedPathRule>>, RulesDefinitionError> {
        let Some(value) = root.get(section) else {
            return Ok(BTreeMap::new());
        };
        let object = expect_nonempty_rule_object(value, section)?;
        let mut result = BTreeMap::new();
        for (source_name, paths_value) in object {
            let source = parse_document_rule_source(source_name)?;
            let context = object_member_context(section, source_name);
            let paths = expect_nonempty_rule_object(paths_value, &context)?;
            let mut rules = Vec::with_capacity(paths.len());
            let mut source_tags = BTreeMap::<String, TagRule>::new();
            for (raw_path, tags_value) in paths {
                ensure_non_blank(raw_path, &format!("{context} 的路径"))?;
                let path_context = object_member_context(&context, raw_path);
                let path = CompiledPath::parse(raw_path)
                    .map_err(|error| qualify_path_error(error, &path_context))?;
                if section == "notes"
                    && !matches!(path.segments.last(), Some(PathSegment::Key(key)) if key == "note")
                {
                    return Err(RulesDefinitionError::InvalidPath {
                        path: path_context,
                        reason: "Note 路径必须以 note 字段结束".to_owned(),
                    });
                }
                let tags = parse_unique_strings(tags_value, &path_context)?;
                if tags.is_empty() && !allow_empty_tags {
                    return Err(RulesDefinitionError::EmptyValue(path_context));
                }
                let mut tag_rules = Vec::with_capacity(tags.len());
                for tag_name in tags {
                    let tag_rule = if let Some(rule) = source_tags.get(&tag_name) {
                        rule.clone()
                    } else {
                        let id = self.add_descriptor(
                            format!(
                                "{context} 中的标签 {}",
                                serde_json::to_string(&tag_name)
                                    .expect("Rust 字符串必须能编码为 JSON 字符串")
                            ),
                            format!(
                                "{}#{tag_name}",
                                if section == "notes" {
                                    "note"
                                } else {
                                    "comment"
                                }
                            ),
                        );
                        let rule = TagRule {
                            id,
                            tag_name: tag_name.clone(),
                        };
                        source_tags.insert(tag_name, rule.clone());
                        rule
                    };
                    tag_rules.push(tag_rule);
                }
                rules.push(TaggedPathRule {
                    label: path_context,
                    path,
                    tags: tag_rules,
                });
            }
            result.insert(source, rules);
        }
        Ok(result)
    }

    fn validate_event_list_consumers(&self) -> Result<(), RulesDefinitionError> {
        if !self.plugin_commands.is_empty() && self.event_lists.is_empty() {
            return Err(RulesDefinitionError::MissingEventLists);
        }
        if self.plugin_commands.is_empty() {
            for rules in self.event_lists.values() {
                if let Some(rule) = rules.iter().find(|rule| rule.tags.is_empty()) {
                    return Err(RulesDefinitionError::EventListWithoutConsumer(
                        rule.label.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_path_section(
        &mut self,
        root: &Map<String, Value>,
        section: &str,
    ) -> Result<BTreeMap<DocumentRuleSource, Vec<PathRule>>, RulesDefinitionError> {
        let Some(value) = root.get(section) else {
            return Ok(BTreeMap::new());
        };
        let object = expect_nonempty_rule_object(value, section)?;
        let mut result = BTreeMap::new();
        for (source_name, values) in object {
            let source = parse_document_rule_source(source_name)?;
            let paths = parse_unique_strings(values, &format!("{section}.{source_name}"))?;
            if paths.is_empty() {
                return Err(RulesDefinitionError::EmptyValue(format!(
                    "{section}.{source_name}"
                )));
            }
            let mut rules = Vec::with_capacity(paths.len());
            for raw_path in paths {
                let path = CompiledPath::parse(&raw_path)?;
                let id = self.add_descriptor(
                    format!("{section}.{source_name}.{raw_path}"),
                    path.field_name(&raw_path),
                );
                rules.push(PathRule { id, path });
            }
            result.insert(source, rules);
        }
        Ok(result)
    }

    fn parse_plugin_parameter_section(
        &mut self,
        root: &Map<String, Value>,
    ) -> Result<BTreeMap<String, Vec<PathRule>>, RulesDefinitionError> {
        let Some(value) = root.get("plugin_parameters") else {
            return Ok(BTreeMap::new());
        };
        let object = expect_nonempty_rule_object(value, "plugin_parameters")?;
        let mut result = BTreeMap::new();
        for (plugin_name, values) in object {
            ensure_non_blank(plugin_name, "plugin_parameters 的插件名")?;
            let paths = parse_unique_strings(values, &format!("plugin_parameters.{plugin_name}"))?;
            if paths.is_empty() {
                return Err(RulesDefinitionError::EmptyValue(format!(
                    "plugin_parameters.{plugin_name}"
                )));
            }
            let mut rules = Vec::with_capacity(paths.len());
            for raw_path in paths {
                let path = CompiledPath::parse(&raw_path)?;
                if !matches!(path.segments.first(), Some(PathSegment::Key(_))) {
                    return Err(RulesDefinitionError::InvalidPath {
                        path: raw_path,
                        reason: "插件参数路径必须先指定参数名".to_owned(),
                    });
                }
                let id = self.add_descriptor(
                    format!("plugin_parameters.{plugin_name}.{raw_path}"),
                    path.field_name(&raw_path),
                );
                rules.push(PathRule { id, path });
            }
            result.insert(plugin_name.clone(), rules);
        }
        Ok(result)
    }

    fn parse_plugin_command_section(
        &mut self,
        root: &Map<String, Value>,
    ) -> Result<BTreeMap<String, BTreeMap<String, Vec<PathRule>>>, RulesDefinitionError> {
        let Some(value) = root.get("plugin_commands") else {
            return Ok(BTreeMap::new());
        };
        let plugins = expect_nonempty_rule_object(value, "plugin_commands")?;
        let mut result = BTreeMap::new();
        for (plugin_name, commands_value) in plugins {
            ensure_non_blank(plugin_name, "plugin_commands 的插件名")?;
            let commands = expect_nonempty_rule_object(
                commands_value,
                &format!("plugin_commands.{plugin_name}"),
            )?;
            let mut parsed_commands = BTreeMap::new();
            for (command_name, values) in commands {
                ensure_non_blank(command_name, "plugin_commands 的命令名")?;
                let paths = parse_unique_strings(
                    values,
                    &format!("plugin_commands.{plugin_name}.{command_name}"),
                )?;
                if paths.is_empty() {
                    return Err(RulesDefinitionError::EmptyValue(format!(
                        "plugin_commands.{plugin_name}.{command_name}"
                    )));
                }
                let mut rules = Vec::with_capacity(paths.len());
                for raw_path in paths {
                    let path = CompiledPath::parse(&raw_path)?;
                    if !matches!(path.segments.first(), Some(PathSegment::Key(_))) {
                        return Err(RulesDefinitionError::InvalidPath {
                            path: raw_path,
                            reason: "插件命令路径必须先指定参数名".to_owned(),
                        });
                    }
                    let id = self.add_descriptor(
                        format!("plugin_commands.{plugin_name}.{command_name}.{raw_path}"),
                        path.field_name(&raw_path),
                    );
                    rules.push(PathRule { id, path });
                }
                parsed_commands.insert(command_name.clone(), rules);
            }
            result.insert(plugin_name.clone(), parsed_commands);
        }
        Ok(result)
    }
}

fn expect_rule_object<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a Map<String, Value>, RulesDefinitionError> {
    value
        .as_object()
        .ok_or_else(|| RulesDefinitionError::ExpectedObject(context.to_owned()))
}

fn expect_nonempty_rule_object<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a Map<String, Value>, RulesDefinitionError> {
    let object = expect_rule_object(value, context)?;
    if object.is_empty() {
        return Err(RulesDefinitionError::EmptyValue(context.to_owned()));
    }
    Ok(object)
}

fn object_member_context(parent: &str, key: &str) -> String {
    let key = serde_json::to_string(key).expect("Rust 字符串必须能编码为 JSON 字符串");
    format!("{parent}[{key}]")
}

fn qualify_path_error(error: RulesDefinitionError, context: &str) -> RulesDefinitionError {
    match error {
        RulesDefinitionError::InvalidPath { reason, .. } => RulesDefinitionError::InvalidPath {
            path: context.to_owned(),
            reason,
        },
        error => error,
    }
}

fn parse_unique_strings(value: &Value, context: &str) -> Result<Vec<String>, RulesDefinitionError> {
    let array = value
        .as_array()
        .ok_or_else(|| RulesDefinitionError::ExpectedArray(context.to_owned()))?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        let string = value
            .as_str()
            .ok_or_else(|| RulesDefinitionError::ExpectedString(format!("{context}[{index}]")))?;
        ensure_non_blank(string, &format!("{context}[{index}]"))?;
        if !seen.insert(string.to_owned()) {
            return Err(RulesDefinitionError::DuplicateValue {
                context: context.to_owned(),
                value: string.to_owned(),
            });
        }
        result.push(string.to_owned());
    }
    Ok(result)
}

fn ensure_non_blank(value: &str, context: &str) -> Result<(), RulesDefinitionError> {
    if value.trim().is_empty() {
        Err(RulesDefinitionError::EmptyValue(context.to_owned()))
    } else {
        Ok(())
    }
}

fn parse_document_rule_source(value: &str) -> Result<DocumentRuleSource, RulesDefinitionError> {
    ensure_non_blank(value, "数据来源")?;
    if value == "Map*.json" {
        return Ok(DocumentRuleSource::AllMaps);
    }
    if let Some(file) = StandardDataFile::from_file_name(value) {
        return Ok(DocumentRuleSource::Data(file));
    }
    let exact_map = value
        .strip_prefix("Map")
        .and_then(|value| value.strip_suffix(".json"))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()));
    Err(RulesDefinitionError::UnsupportedSource {
        source: value.to_owned(),
        guidance: if exact_map {
            "地图规则请使用 Map*.json".to_owned()
        } else {
            "非标准 data/*.json 必须交给 Lua".to_owned()
        },
    })
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PathSegment {
    Key(String),
    AnyIndex,
    Index(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompiledPath {
    segments: Vec<PathSegment>,
}

impl CompiledPath {
    fn parse(path: &str) -> Result<Self, RulesDefinitionError> {
        if path.is_empty() {
            return Err(RulesDefinitionError::EmptyValue("路径".to_owned()));
        }
        if path.starts_with('$') {
            return Err(invalid_path(path, "不支持 $ 或通用 JSONPath"));
        }

        let bytes = path.as_bytes();
        let mut cursor = 0;
        let mut segments = Vec::new();
        let mut expect_segment = true;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'.' => {
                    if expect_segment {
                        return Err(invalid_path(path, "点号两侧必须是字段名"));
                    }
                    cursor += 1;
                    expect_segment = true;
                }
                b'[' => {
                    let (segment, next) = parse_bracket_segment(path, cursor)?;
                    segments.push(segment);
                    cursor = next;
                    expect_segment = false;
                }
                _ => {
                    if !expect_segment {
                        return Err(invalid_path(path, "字段之间需要点号"));
                    }
                    let start = cursor;
                    while cursor < bytes.len()
                        && bytes[cursor] != b'.'
                        && bytes[cursor] != b'['
                        && bytes[cursor] != b']'
                    {
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
                        return Err(invalid_path(
                            path,
                            "普通字段名只支持字母、数字和下划线；其他键请使用 [\"...\"]",
                        ));
                    }
                    segments.push(PathSegment::Key(key.to_owned()));
                    expect_segment = false;
                }
            }
        }
        if expect_segment || segments.is_empty() {
            return Err(invalid_path(path, "路径不能以点号结束"));
        }
        Ok(Self { segments })
    }

    fn field_name(&self, fallback: &str) -> String {
        self.segments
            .iter()
            .rev()
            .find_map(|segment| match segment {
                PathSegment::Key(key) => Some(key.clone()),
                PathSegment::AnyIndex | PathSegment::Index(_) => None,
            })
            .unwrap_or_else(|| fallback.to_owned())
    }
}

fn parse_bracket_segment(
    path: &str,
    start: usize,
) -> Result<(PathSegment, usize), RulesDefinitionError> {
    let bytes = path.as_bytes();
    let mut cursor = start + 1;
    if cursor >= bytes.len() {
        return Err(invalid_path(path, "方括号没有闭合"));
    }
    if bytes[cursor] == b']' {
        return Ok((PathSegment::AnyIndex, cursor + 1));
    }
    if bytes[cursor] == b'"' {
        let string_start = cursor;
        cursor += 1;
        let mut escaped = false;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                cursor += 1;
                break;
            }
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b']' {
            return Err(invalid_path(path, "带引号的键没有以 ] 结束"));
        }
        let encoded = &path[string_start..cursor];
        let key: String = serde_json::from_str(encoded)
            .map_err(|error| invalid_path(path, &format!("键字符串无效：{error}")))?;
        if key.is_empty() {
            return Err(invalid_path(path, "字段名不能为空"));
        }
        return Ok((PathSegment::Key(key), cursor + 1));
    }

    let digits_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    if cursor == digits_start || cursor >= bytes.len() || bytes[cursor] != b']' {
        return Err(invalid_path(path, "方括号只支持 []、[数字] 或 [\"键\"]"));
    }
    let index = path[digits_start..cursor]
        .parse::<usize>()
        .map_err(|_| invalid_path(path, "数组下标超出范围"))?;
    Ok((PathSegment::Index(index), cursor + 1))
}

fn invalid_path(path: &str, reason: &str) -> RulesDefinitionError {
    RulesDefinitionError::InvalidPath {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn parse_strict_json(text: &str) -> Result<Value, RulesDefinitionError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = StrictValueSeed
        .deserialize(&mut deserializer)
        .map_err(|error| RulesDefinitionError::InvalidJson(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| RulesDefinitionError::InvalidJson(error.to_string()))?;
    Ok(value)
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("任意合法 JSON 值")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON 数字不是有限值"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!("对象键重复：{key}")));
            }
            values.insert(key, map.next_value_seed(StrictValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

#[derive(Default)]
struct PathTrie {
    terminals: Vec<usize>,
    children: BTreeMap<PathSegment, PathTrie>,
}

impl PathTrie {
    fn from_rules<'a>(rules: impl IntoIterator<Item = &'a PathRule>) -> Self {
        let mut trie = Self::default();
        for rule in rules {
            trie.insert(&rule.path.segments, rule.id);
        }
        trie
    }

    fn from_tagged_paths(rules: &[TaggedPathRule]) -> Self {
        let mut trie = Self::default();
        for (index, rule) in rules.iter().enumerate() {
            trie.insert(&rule.path.segments, index);
        }
        trie
    }

    fn insert(&mut self, segments: &[PathSegment], id: usize) {
        let Some((head, tail)) = segments.split_first() else {
            self.terminals.push(id);
            return;
        };
        self.children
            .entry(head.clone())
            .or_default()
            .insert(tail, id);
    }

    fn first_rule_id(&self) -> Option<usize> {
        self.terminals
            .first()
            .copied()
            .or_else(|| self.children.values().find_map(PathTrie::first_rule_id))
    }
}

#[derive(Debug)]
enum BuildRulesSnapshotError {
    NoMatch { locator: String },
    InvalidTarget { locator: String, message: String },
    DuplicateTarget { location: MzLocation },
    Model { locator: String, message: String },
}

#[derive(Default)]
struct MatchCounts(BTreeMap<usize, usize>);

impl MatchCounts {
    fn increment(&mut self, id: usize) {
        *self.0.entry(id).or_default() += 1;
    }

    fn merge(&mut self, other: Self) {
        for (id, count) in other.0 {
            *self.0.entry(id).or_default() += count;
        }
    }

    fn count(&self, id: usize) -> usize {
        self.0.get(&id).copied().unwrap_or(0)
    }
}

struct GroupCollector {
    groups: BTreeMap<(MzLocation, TextGroupKind), Vec<ExtractedTextField>>,
    locations: BTreeSet<MzLocation>,
}

impl GroupCollector {
    fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
            locations: BTreeSet::new(),
        }
    }

    fn add(
        &mut self,
        kind: TextGroupKind,
        group_location: MzLocation,
        field_name: &str,
        exact_location: MzLocation,
        original_text: &str,
        locator: &str,
    ) -> Result<(), BuildRulesSnapshotError> {
        if !self.locations.insert(exact_location.clone()) {
            return Err(BuildRulesSnapshotError::DuplicateTarget {
                location: exact_location,
            });
        }
        let field = ExtractedTextField::new(field_name, exact_location, original_text).map_err(
            |source| BuildRulesSnapshotError::Model {
                locator: locator.to_owned(),
                message: source.to_string(),
            },
        )?;
        self.groups
            .entry((group_location, kind))
            .or_default()
            .push(field);
        Ok(())
    }

    fn into_groups(self) -> Result<Vec<ExtractedTextGroup>, BuildRulesSnapshotError> {
        let mut groups = Vec::with_capacity(self.groups.len());
        for ((group_location, kind), fields) in self.groups {
            groups.push(
                ExtractedTextGroup::new(kind, group_location, fields).map_err(|source| {
                    BuildRulesSnapshotError::Model {
                        locator: "Rules 快照".to_owned(),
                        message: source.to_string(),
                    }
                })?,
            );
        }
        Ok(groups)
    }

    #[cfg(test)]
    fn finish(self) -> Result<RulesSnapshot, BuildRulesSnapshotError> {
        let groups = self.into_groups()?;
        rules_snapshot_from_groups(groups)
    }
}

fn rules_snapshot_from_groups(
    groups: Vec<ExtractedTextGroup>,
) -> Result<RulesSnapshot, BuildRulesSnapshotError> {
    RulesSnapshot::new(groups).map_err(|source| match source {
        SnapshotModelError::DuplicateLocation { exact_location } => {
            BuildRulesSnapshotError::DuplicateTarget {
                location: exact_location,
            }
        }
        source => BuildRulesSnapshotError::Model {
            locator: "Rules 快照".to_owned(),
            message: source.to_string(),
        },
    })
}

#[cfg(test)]
fn build_rules_snapshot(
    definition: &RulesDefinition,
    documents: &MzProjectDocuments,
) -> Result<RulesSnapshot, BuildRulesSnapshotError> {
    let mut matches = MatchCounts::default();
    let mut collector = GroupCollector::new();

    extract_standard_fields(definition, documents, &mut matches, &mut collector)?;
    extract_notes(definition, documents, &mut matches, &mut collector)?;
    extract_plugin_parameters(
        definition,
        documents.plugins(),
        &mut matches,
        &mut collector,
    )?;
    extract_event_rules(definition, documents, &mut matches, &mut collector)?;

    if let Some(id) = (0..definition.descriptors.len()).find(|id| matches.count(*id) == 0) {
        return Err(BuildRulesSnapshotError::NoMatch {
            locator: definition.descriptors[id].label.clone(),
        });
    }
    collector.finish()
}

enum ParallelRulesBuildError<CE> {
    MatchCompute(CpuTaskExecutionError<CE>),
    FinalizeCompute(CpuTaskExecutionError<CE>),
    Build(BuildRulesSnapshotError),
}

struct PreparedRules {
    standard_tries: BTreeMap<DocumentRuleSource, Arc<PathTrie>>,
    note_tries: BTreeMap<DocumentRuleSource, Arc<PathTrie>>,
    event_list_tries: BTreeMap<DocumentRuleSource, Arc<PathTrie>>,
    plugin_parameter_tries: BTreeMap<String, BTreeMap<String, Arc<PathTrie>>>,
    plugin_command_tries: Arc<PluginCommandTries>,
}

impl PreparedRules {
    fn new(definition: &RulesDefinition) -> Result<Self, BuildRulesSnapshotError> {
        let standard_tries = definition
            .standard_fields
            .iter()
            .map(|(source, rules)| (*source, Arc::new(PathTrie::from_rules(rules))))
            .collect();
        let note_tries = definition
            .notes
            .iter()
            .map(|(source, rules)| (*source, Arc::new(PathTrie::from_tagged_paths(rules))))
            .collect();
        let event_list_tries = definition
            .event_lists
            .iter()
            .map(|(source, rules)| (*source, Arc::new(PathTrie::from_tagged_paths(rules))))
            .collect();

        let mut plugin_parameter_tries = BTreeMap::new();
        for (plugin_name, rules) in &definition.plugin_parameters {
            let mut parameter_tries = BTreeMap::<String, PathTrie>::new();
            for rule in rules {
                let Some((PathSegment::Key(parameter_name), tail)) =
                    rule.path.segments.split_first()
                else {
                    return invalid_target(definition, rule.id, "插件参数路径没有参数名");
                };
                parameter_tries
                    .entry(parameter_name.clone())
                    .or_default()
                    .insert(tail, rule.id);
            }
            plugin_parameter_tries.insert(
                plugin_name.clone(),
                parameter_tries
                    .into_iter()
                    .map(|(name, trie)| (name, Arc::new(trie)))
                    .collect(),
            );
        }

        Ok(Self {
            standard_tries,
            note_tries,
            event_list_tries,
            plugin_parameter_tries,
            plugin_command_tries: Arc::new(compile_plugin_command_tries(definition)),
        })
    }
}

enum RulesWorkUnit {
    Document {
        document_id: MzDocumentId,
        document: Arc<Value>,
    },
    PluginParameters(Arc<PluginConfiguration>),
}

struct RulesWorkResult {
    matches: MatchCounts,
    groups: Vec<ExtractedTextGroup>,
}

impl RulesWorkUnit {
    fn run(
        self,
        definition: &RulesDefinition,
        prepared: &PreparedRules,
    ) -> Result<RulesWorkResult, BuildRulesSnapshotError> {
        let mut matches = MatchCounts::default();
        let mut collector = GroupCollector::new();

        match self {
            Self::Document {
                document_id,
                document,
            } => {
                let rule_source = document_rule_source(document_id);
                let source = document_source(document_id);
                if let Some(trie) = prepared.standard_tries.get(&rule_source) {
                    let kind = standard_group_kind(&source, &[]);
                    match_path_trie(
                        &document,
                        trie,
                        &source,
                        &mut Vec::new(),
                        Vec::new(),
                        None,
                        kind,
                        false,
                        definition,
                        &mut matches,
                        &mut collector,
                    )?;
                }
                if let (Some(rules), Some(trie)) = (
                    definition.notes.get(&rule_source),
                    prepared.note_tries.get(&rule_source),
                ) {
                    match_note_paths(
                        &document,
                        trie,
                        &source,
                        rules,
                        definition,
                        &mut matches,
                        &mut collector,
                    )?;
                }
                if let (Some(rules), Some(trie)) = (
                    definition.event_lists.get(&rule_source),
                    prepared.event_list_tries.get(&rule_source),
                ) {
                    match_event_list_paths(
                        &document,
                        trie,
                        &source,
                        rules,
                        &prepared.plugin_command_tries,
                        definition,
                        &mut matches,
                        &mut collector,
                    )?;
                }
            }
            Self::PluginParameters(plugin) => extract_plugin_parameter_record(
                definition,
                prepared,
                &plugin,
                &mut matches,
                &mut collector,
            )?,
        }

        Ok(RulesWorkResult {
            matches,
            groups: collector.into_groups()?,
        })
    }
}

async fn build_rules_snapshot_parallel<C>(
    cpu_executor: &C,
    config: RulesExtractionConfig,
    definition: RulesDefinition,
    documents: MzProjectDocuments,
) -> Result<RulesSnapshot, ParallelRulesBuildError<C::Error>>
where
    C: CpuTaskExecutor,
{
    let definition = Arc::new(definition);
    let prepared_definition = Arc::clone(&definition);
    let prepared = cpu_executor
        .execute(move || PreparedRules::new(&prepared_definition))
        .await
        .map_err(ParallelRulesBuildError::MatchCompute)?
        .map_err(ParallelRulesBuildError::Build)?;
    let prepared = Arc::new(prepared);

    let work_units =
        rules_work_units(&definition, documents).map_err(ParallelRulesBuildError::Build)?;
    let results = stream::iter(work_units.into_iter().map(|work_unit| {
        let definition = Arc::clone(&definition);
        let prepared = Arc::clone(&prepared);
        cpu_executor.execute(move || work_unit.run(&definition, &prepared))
    }))
    .buffered(config.scan_concurrency().get())
    .collect::<Vec<_>>()
    .await;

    let mut completed = Vec::with_capacity(results.len());
    for result in results {
        completed.push(
            result
                .map_err(ParallelRulesBuildError::MatchCompute)?
                .map_err(ParallelRulesBuildError::Build)?,
        );
    }

    let definition = Arc::clone(&definition);
    cpu_executor
        .execute(move || finalize_parallel_rules(&definition, completed))
        .await
        .map_err(ParallelRulesBuildError::FinalizeCompute)?
        .map_err(ParallelRulesBuildError::Build)
}

fn rules_work_units(
    definition: &RulesDefinition,
    documents: MzProjectDocuments,
) -> Result<Vec<RulesWorkUnit>, BuildRulesSnapshotError> {
    let (documents, plugins) = documents.into_parts();
    let documents = documents
        .into_iter()
        .map(|(id, value)| (id, Arc::new(value)))
        .collect::<BTreeMap<_, _>>();
    validate_required_rules_documents(definition, &documents)?;

    let mut work_units = Vec::new();
    for (document_id, document) in &documents {
        let rule_source = document_rule_source(*document_id);
        if definition.standard_fields.contains_key(&rule_source)
            || definition.notes.contains_key(&rule_source)
            || definition.event_lists.contains_key(&rule_source)
        {
            work_units.push(RulesWorkUnit::Document {
                document_id: *document_id,
                document: Arc::clone(document),
            });
        }
    }

    for plugin in plugins {
        let plugin = Arc::new(plugin);
        let matching_name = plugin
            .fields()
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| definition.plugin_parameters.contains_key(name));
        if matching_name {
            work_units.push(RulesWorkUnit::PluginParameters(plugin));
        }
    }
    Ok(work_units)
}

fn validate_required_rules_documents(
    definition: &RulesDefinition,
    documents: &BTreeMap<MzDocumentId, Arc<Value>>,
) -> Result<(), BuildRulesSnapshotError> {
    for (source, rules) in &definition.standard_fields {
        if let DocumentRuleSource::Data(file) = source
            && !documents.contains_key(&MzDocumentId::Data(*file))
        {
            return invalid_target(
                definition,
                first_path_rule_id(rules),
                &format!("读取器没有返回已请求的 {}", file.file_name()),
            );
        }
    }
    for (source, rules) in definition.notes.iter().chain(&definition.event_lists) {
        if let DocumentRuleSource::Data(file) = source
            && !documents.contains_key(&MzDocumentId::Data(*file))
        {
            return Err(invalid_target_at(
                &rules[0].label,
                &format!("读取器没有返回已请求的 {}", file.file_name()),
            ));
        }
    }
    Ok(())
}

const fn document_rule_source(id: MzDocumentId) -> DocumentRuleSource {
    match id {
        MzDocumentId::Data(file) => DocumentRuleSource::Data(file),
        MzDocumentId::Map(_) => DocumentRuleSource::AllMaps,
    }
}

fn document_source(id: MzDocumentId) -> MzSource {
    match id {
        MzDocumentId::Data(file) => MzSource::data(file),
        MzDocumentId::Map(map_id) => MzSource::map(map_id),
    }
}

fn finalize_parallel_rules(
    definition: &RulesDefinition,
    results: Vec<RulesWorkResult>,
) -> Result<RulesSnapshot, BuildRulesSnapshotError> {
    let mut matches = MatchCounts::default();
    let mut groups = Vec::new();
    for result in results {
        matches.merge(result.matches);
        groups.extend(result.groups);
    }
    if let Some(id) = (0..definition.descriptors.len()).find(|id| matches.count(*id) == 0) {
        return Err(BuildRulesSnapshotError::NoMatch {
            locator: definition.descriptors[id].label.clone(),
        });
    }
    rules_snapshot_from_groups(groups)
}

#[cfg(test)]
fn extract_standard_fields(
    definition: &RulesDefinition,
    documents: &MzProjectDocuments,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    for (rule_source, rules) in &definition.standard_fields {
        let trie = PathTrie::from_rules(rules);
        for (source, document) in documents_for_rule_source(
            *rule_source,
            documents,
            first_path_rule_id(rules),
            definition,
        )? {
            let kind = standard_group_kind(&source, &[]);
            let mut steps = Vec::new();
            match_path_trie(
                document,
                &trie,
                &source,
                &mut steps,
                Vec::new(),
                None,
                kind,
                false,
                definition,
                matches,
                collector,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn extract_notes(
    definition: &RulesDefinition,
    documents: &MzProjectDocuments,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    for (rule_source, rules) in &definition.notes {
        let trie = PathTrie::from_tagged_paths(rules);
        for (source, document) in
            documents_for_rule_source_at(*rule_source, documents, &rules[0].label)?
        {
            match_note_paths(
                document, &trie, &source, rules, definition, matches, collector,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn extract_plugin_parameters(
    definition: &RulesDefinition,
    plugins: &[PluginConfiguration],
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    for (plugin_name, rules) in &definition.plugin_parameters {
        let mut parameter_tries = BTreeMap::<String, PathTrie>::new();
        for rule in rules {
            let Some((PathSegment::Key(parameter_name), tail)) = rule.path.segments.split_first()
            else {
                return invalid_target(definition, rule.id, "插件参数路径没有参数名");
            };
            parameter_tries
                .entry(parameter_name.clone())
                .or_default()
                .insert(tail, rule.id);
        }

        for plugin in plugins {
            let record = plugin.fields();
            let Some(record_name) = record.get("name").and_then(Value::as_str) else {
                continue;
            };
            if record_name != plugin_name {
                continue;
            }

            let rule_id = first_path_rule_id(rules);
            let enabled = record
                .get("status")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    invalid_target_value(definition, rule_id, "目标插件记录的 status 必须是布尔值")
                })?;
            if !enabled {
                continue;
            }
            let parameters = record
                .get("parameters")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    invalid_target_value(
                        definition,
                        rule_id,
                        "目标插件记录的 parameters 必须是对象",
                    )
                })?;

            for (parameter_name, trie) in &parameter_tries {
                let Some(raw_value) = parameters.get(parameter_name) else {
                    continue;
                };
                let Some(raw_value) = raw_value.as_str() else {
                    return invalid_target(
                        definition,
                        trie.first_rule_id().unwrap_or(rule_id),
                        "目标插件参数必须是字符串",
                    );
                };
                let source =
                    MzSource::plugin_parameter(plugin.index(), record_name, parameter_name);
                let mut steps = Vec::new();
                match_path_trie(
                    &Value::String(raw_value.to_owned()),
                    trie,
                    &source,
                    &mut steps,
                    Vec::new(),
                    None,
                    TextGroupKind::PluginParameter,
                    true,
                    definition,
                    matches,
                    collector,
                )?;
            }
        }
    }
    Ok(())
}

fn extract_plugin_parameter_record(
    definition: &RulesDefinition,
    prepared: &PreparedRules,
    plugin: &PluginConfiguration,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let record = plugin.fields();
    let Some(record_name) = record.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(parameter_tries) = prepared.plugin_parameter_tries.get(record_name) else {
        return Ok(());
    };
    let rules = definition
        .plugin_parameters
        .get(record_name)
        .expect("已编译插件参数必须仍有原始规则");
    let rule_id = first_path_rule_id(rules);
    let enabled = record
        .get("status")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            invalid_target_value(definition, rule_id, "目标插件记录的 status 必须是布尔值")
        })?;
    if !enabled {
        return Ok(());
    }
    let parameters = record
        .get("parameters")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            invalid_target_value(definition, rule_id, "目标插件记录的 parameters 必须是对象")
        })?;

    for (parameter_name, trie) in parameter_tries {
        let Some(raw_value) = parameters.get(parameter_name) else {
            continue;
        };
        let Some(raw_value) = raw_value.as_str() else {
            return invalid_target(
                definition,
                trie.first_rule_id().unwrap_or(rule_id),
                "目标插件参数必须是字符串",
            );
        };
        let source = MzSource::plugin_parameter(plugin.index(), record_name, parameter_name);
        match_path_trie(
            &Value::String(raw_value.to_owned()),
            trie,
            &source,
            &mut Vec::new(),
            Vec::new(),
            None,
            TextGroupKind::PluginParameter,
            true,
            definition,
            matches,
            collector,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn match_path_trie(
    value: &Value,
    trie: &PathTrie,
    source: &MzSource,
    steps: &mut Vec<MzLocationStep>,
    default_group_steps: Vec<MzLocationStep>,
    active_group_steps: Option<Vec<MzLocationStep>>,
    default_kind: TextGroupKind,
    decode_strings: bool,
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let context = |id: usize| {
        definition
            .descriptors
            .get(id)
            .map_or_else(|| "Rules 路径".to_owned(), |rule| rule.label.clone())
    };
    let mut visit_terminal =
        |value: &Value,
         terminals: &[usize],
         terminal_steps: &[MzLocationStep],
         terminal_group_steps: Option<&[MzLocationStep]>| {
            let Some(text) = value.as_str() else {
                return Err(BuildRulesSnapshotError::InvalidTarget {
                    locator: context(terminals[0]),
                    message: "最终目标必须是字符串".to_owned(),
                });
            };
            if !text.trim().is_empty() {
                for id in terminals {
                    let descriptor = &definition.descriptors[*id];
                    let exact_location = MzLocation::value(source.clone(), terminal_steps.to_vec());
                    let group_steps = terminal_group_steps
                        .map_or_else(|| default_group_steps.clone(), <[_]>::to_vec);
                    collector.add(
                        default_kind,
                        MzLocation::value(source.clone(), group_steps),
                        &descriptor.field_name,
                        exact_location,
                        text,
                        &descriptor.label,
                    )?;
                    matches.increment(*id);
                }
            }
            Ok(())
        };

    walk_path_trie(
        value,
        trie,
        steps,
        active_group_steps,
        decode_strings,
        &context,
        &mut visit_terminal,
    )
}

fn walk_path_trie(
    value: &Value,
    trie: &PathTrie,
    steps: &mut Vec<MzLocationStep>,
    active_group_steps: Option<Vec<MzLocationStep>>,
    decode_strings: bool,
    context: &impl Fn(usize) -> String,
    visit_terminal: &mut impl FnMut(
        &Value,
        &[usize],
        &[MzLocationStep],
        Option<&[MzLocationStep]>,
    ) -> Result<(), BuildRulesSnapshotError>,
) -> Result<(), BuildRulesSnapshotError> {
    if !trie.terminals.is_empty() {
        visit_terminal(value, &trie.terminals, steps, active_group_steps.as_deref())?;
    }

    if trie.children.is_empty() {
        return Ok(());
    }

    if let Value::String(raw_json) = value {
        if !decode_strings {
            return Err(path_walk_error(
                trie,
                context,
                "路径仍需深入，但当前值是字符串",
            ));
        }
        let decoded: Value = serde_json::from_str(raw_json).map_err(|source| {
            BuildRulesSnapshotError::InvalidTarget {
                locator: path_walk_context(trie, context),
                message: format!("嵌套 JSON 字符串无法解码：{source}"),
            }
        })?;
        steps.push(MzLocationStep::DecodeJsonString);
        let result = walk_path_children(
            &decoded,
            trie,
            steps,
            active_group_steps,
            decode_strings,
            context,
            visit_terminal,
        );
        steps.pop();
        return result;
    }

    walk_path_children(
        value,
        trie,
        steps,
        active_group_steps,
        decode_strings,
        context,
        visit_terminal,
    )
}

fn walk_path_children(
    value: &Value,
    trie: &PathTrie,
    steps: &mut Vec<MzLocationStep>,
    active_group_steps: Option<Vec<MzLocationStep>>,
    decode_strings: bool,
    context: &impl Fn(usize) -> String,
    visit_terminal: &mut impl FnMut(
        &Value,
        &[usize],
        &[MzLocationStep],
        Option<&[MzLocationStep]>,
    ) -> Result<(), BuildRulesSnapshotError>,
) -> Result<(), BuildRulesSnapshotError> {
    // 对象节点通常承载成千上万个同级字段规则。由实际字段反查前缀树，避免在 `[]`
    // 展开的每个稀疏对象上重新枚举全部规则。
    if trie
        .children
        .keys()
        .all(|segment| matches!(segment, PathSegment::Key(_)))
    {
        let object = value
            .as_object()
            .ok_or_else(|| path_walk_error(trie, context, "路径需要对象字段，但当前值不是对象"))?;

        for (key, child_value) in object {
            let Some(child) = trie.children.get(&PathSegment::Key(key.clone())) else {
                continue;
            };
            steps.push(MzLocationStep::key(key));
            let result = walk_path_trie(
                child_value,
                child,
                steps,
                active_group_steps.clone(),
                decode_strings,
                context,
                visit_terminal,
            );
            steps.pop();
            result?;
        }
        return Ok(());
    }

    for (segment, child) in &trie.children {
        match segment {
            PathSegment::Key(key) => {
                let object = value.as_object().ok_or_else(|| {
                    path_walk_error(child, context, "路径需要对象字段，但当前值不是对象")
                })?;
                let Some(child_value) = object.get(key) else {
                    continue;
                };
                steps.push(MzLocationStep::key(key));
                let result = walk_path_trie(
                    child_value,
                    child,
                    steps,
                    active_group_steps.clone(),
                    decode_strings,
                    context,
                    visit_terminal,
                );
                steps.pop();
                result?;
            }
            PathSegment::Index(index) => {
                let array = value.as_array().ok_or_else(|| {
                    path_walk_error(child, context, "路径需要数组下标，但当前值不是数组")
                })?;
                let Some(child_value) = array.get(*index) else {
                    continue;
                };
                steps.push(MzLocationStep::index(*index));
                let group_steps = Some(steps.clone());
                let result = walk_path_trie(
                    child_value,
                    child,
                    steps,
                    group_steps,
                    decode_strings,
                    context,
                    visit_terminal,
                );
                steps.pop();
                result?;
            }
            PathSegment::AnyIndex => {
                let array = value.as_array().ok_or_else(|| {
                    path_walk_error(child, context, "路径使用 []，但当前值不是数组")
                })?;
                for (index, child_value) in array.iter().enumerate() {
                    if child_value.is_null() {
                        continue;
                    }
                    steps.push(MzLocationStep::index(index));
                    let group_steps = Some(steps.clone());
                    let result = walk_path_trie(
                        child_value,
                        child,
                        steps,
                        group_steps,
                        decode_strings,
                        context,
                        visit_terminal,
                    );
                    steps.pop();
                    result?;
                }
            }
        }
    }
    Ok(())
}

fn path_walk_context(trie: &PathTrie, context: &impl Fn(usize) -> String) -> String {
    trie.first_rule_id()
        .map_or_else(|| "Rules 路径".to_owned(), context)
}

fn path_walk_error(
    trie: &PathTrie,
    context: &impl Fn(usize) -> String,
    message: &str,
) -> BuildRulesSnapshotError {
    BuildRulesSnapshotError::InvalidTarget {
        locator: path_walk_context(trie, context),
        message: message.to_owned(),
    }
}

fn standard_group_kind(source: &MzSource, group_steps: &[MzLocationStep]) -> TextGroupKind {
    match source {
        MzSource::Data(StandardDataFile::System) => TextGroupKind::System,
        MzSource::Map(_) if group_steps.is_empty() => TextGroupKind::Map,
        MzSource::PluginParameter { .. } => TextGroupKind::PluginParameter,
        MzSource::Data(_) | MzSource::Map(_) => TextGroupKind::DatabaseEntry,
    }
}

fn first_path_rule_id(rules: &[PathRule]) -> usize {
    rules.first().map_or(0, |rule| rule.id)
}

#[cfg(test)]
fn documents_for_rule_source<'a>(
    source: DocumentRuleSource,
    documents: &'a MzProjectDocuments,
    rule_id: usize,
    definition: &RulesDefinition,
) -> Result<Vec<(MzSource, &'a Value)>, BuildRulesSnapshotError> {
    let locator = definition
        .descriptors
        .get(rule_id)
        .map_or("Rules 定位", |rule| rule.label.as_str());
    documents_for_rule_source_at(source, documents, locator)
}

#[cfg(test)]
fn documents_for_rule_source_at<'a>(
    source: DocumentRuleSource,
    documents: &'a MzProjectDocuments,
    locator: &str,
) -> Result<Vec<(MzSource, &'a Value)>, BuildRulesSnapshotError> {
    match source {
        DocumentRuleSource::Data(file) => documents
            .document(MzDocumentId::Data(file))
            .map(|document| vec![(MzSource::data(file), document)])
            .ok_or_else(|| {
                invalid_target_at(
                    locator,
                    &format!("读取器没有返回已请求的 {}", file.file_name()),
                )
            }),
        DocumentRuleSource::AllMaps => Ok(documents
            .documents()
            .iter()
            .filter_map(|(id, document)| match id {
                MzDocumentId::Map(map_id) => Some((MzSource::map(*map_id), document)),
                MzDocumentId::Data(_) => None,
            })
            .collect()),
    }
}

fn invalid_target<T>(
    definition: &RulesDefinition,
    rule_id: usize,
    message: &str,
) -> Result<T, BuildRulesSnapshotError> {
    Err(invalid_target_value(definition, rule_id, message))
}

fn invalid_target_value(
    definition: &RulesDefinition,
    rule_id: usize,
    message: &str,
) -> BuildRulesSnapshotError {
    BuildRulesSnapshotError::InvalidTarget {
        locator: definition
            .descriptors
            .get(rule_id)
            .map_or_else(|| "Rules 定位".to_owned(), |rule| rule.label.clone()),
        message: message.to_owned(),
    }
}

fn invalid_target_at(locator: &str, message: &str) -> BuildRulesSnapshotError {
    BuildRulesSnapshotError::InvalidTarget {
        locator: locator.to_owned(),
        message: message.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn match_note_paths(
    value: &Value,
    trie: &PathTrie,
    source: &MzSource,
    rules: &[TaggedPathRule],
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let context = |index: usize| {
        rules
            .get(index)
            .map_or_else(|| "Rules Note 路径".to_owned(), |rule| rule.label.clone())
    };
    let mut visit_terminal =
        |note: &Value,
         terminals: &[usize],
         terminal_steps: &[MzLocationStep],
         _group_steps: Option<&[MzLocationStep]>| {
            let Some(text) = note.as_str() else {
                return Err(BuildRulesSnapshotError::InvalidTarget {
                    locator: context(terminals[0]),
                    message: "Note 路径终点必须是字符串".to_owned(),
                });
            };
            let container_steps = terminal_steps
                .split_last()
                .map_or_else(Vec::new, |(_, steps)| steps.to_vec());

            for rule_index in terminals {
                let rule = &rules[*rule_index];
                let selected = rule
                    .tags
                    .iter()
                    .map(|rule| (rule.tag_name.as_str(), rule))
                    .collect::<BTreeMap<_, _>>();
                for tag in simple_tag_spans(text) {
                    let Some(tag_rule) = selected.get(tag.name()) else {
                        continue;
                    };
                    if tag.value().trim().is_empty() {
                        continue;
                    }
                    let exact_location = MzLocation::note_tag(
                        source.clone(),
                        container_steps.clone(),
                        tag.name(),
                        tag.occurrence(),
                    );
                    let group_location = MzLocation::value(source.clone(), container_steps.clone());
                    let descriptor = &definition.descriptors[tag_rule.id];
                    collector.add(
                        standard_group_kind(source, &container_steps),
                        group_location,
                        &descriptor.field_name,
                        exact_location,
                        tag.value(),
                        &descriptor.label,
                    )?;
                    matches.increment(tag_rule.id);
                }
            }
            Ok(())
        };

    walk_path_trie(
        value,
        trie,
        &mut Vec::new(),
        None,
        false,
        &context,
        &mut visit_terminal,
    )
}

#[cfg(test)]
fn extract_event_rules(
    definition: &RulesDefinition,
    documents: &MzProjectDocuments,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    if definition.event_lists.is_empty() {
        return Ok(());
    }

    let command_tries = compile_plugin_command_tries(definition);
    for (rule_source, rules) in &definition.event_lists {
        let trie = PathTrie::from_tagged_paths(rules);
        for (source, document) in
            documents_for_rule_source_at(*rule_source, documents, &rules[0].label)?
        {
            match_event_list_paths(
                document,
                &trie,
                &source,
                rules,
                &command_tries,
                definition,
                matches,
                collector,
            )?;
        }
    }
    Ok(())
}

type PluginCommandTries = BTreeMap<String, BTreeMap<String, PathTrie>>;

fn compile_plugin_command_tries(definition: &RulesDefinition) -> PluginCommandTries {
    definition
        .plugin_commands
        .iter()
        .map(|(plugin, commands)| {
            (
                plugin.clone(),
                commands
                    .iter()
                    .map(|(command, rules)| (command.clone(), PathTrie::from_rules(rules)))
                    .collect(),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn match_event_list_paths(
    value: &Value,
    trie: &PathTrie,
    source: &MzSource,
    rules: &[TaggedPathRule],
    command_tries: &PluginCommandTries,
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let context = |index: usize| {
        rules.get(index).map_or_else(
            || "Rules 事件列表路径".to_owned(),
            |rule| rule.label.clone(),
        )
    };
    let mut visit_terminal =
        |list: &Value,
         terminals: &[usize],
         terminal_steps: &[MzLocationStep],
         _group_steps: Option<&[MzLocationStep]>| {
            let Some(list) = list.as_array() else {
                return Err(BuildRulesSnapshotError::InvalidTarget {
                    locator: context(terminals[0]),
                    message: "事件列表路径终点必须是数组".to_owned(),
                });
            };
            for rule_index in terminals {
                process_event_list(
                    list,
                    source,
                    terminal_steps,
                    &rules[*rule_index].label,
                    &rules[*rule_index].tags,
                    command_tries,
                    definition,
                    matches,
                    collector,
                )?;
            }
            Ok(())
        };

    walk_path_trie(
        value,
        trie,
        &mut Vec::new(),
        None,
        false,
        &context,
        &mut visit_terminal,
    )
}

#[allow(clippy::too_many_arguments)]
fn process_event_list(
    list: &[Value],
    source: &MzSource,
    list_steps: &[MzLocationStep],
    list_locator: &str,
    comment_rules: &[TagRule],
    command_tries: &PluginCommandTries,
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    for (command_index, command) in list.iter().enumerate() {
        let command = command
            .as_object()
            .ok_or_else(|| invalid_target_at(list_locator, "事件指令必须是对象"))?;
        let code = command
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid_target_at(list_locator, "事件指令 code 必须是整数"))?;
        let parameters = command
            .get("parameters")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_target_at(list_locator, "事件指令 parameters 必须是数组"))?;

        if code == 108 && !comment_rules.is_empty() {
            extract_comment_block(
                list,
                command_index,
                source,
                list_steps,
                comment_rules,
                definition,
                matches,
                collector,
            )?;
        }
        if code == 357 && !command_tries.is_empty() {
            extract_plugin_command(
                parameters,
                command_index,
                source,
                list_steps,
                list_locator,
                command_tries,
                definition,
                matches,
                collector,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extract_comment_block(
    list: &[Value],
    start_index: usize,
    source: &MzSource,
    list_steps: &[MzLocationStep],
    rules: &[TagRule],
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let mut lines = Vec::new();
    let mut index = start_index;
    while index < list.len() {
        let Some(command) = list[index].as_object() else {
            break;
        };
        let Some(code) = command.get("code").and_then(Value::as_i64) else {
            break;
        };
        if (index == start_index && code != 108) || (index > start_index && code != 408) {
            break;
        }
        let line = command
            .get("parameters")
            .and_then(Value::as_array)
            .and_then(|parameters| parameters.first())
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_target_value(definition, rules[0].id, "108/408 注释正文必须是字符串")
            })?;
        lines.push(line);
        index += 1;
    }
    let text = lines.join("\n");
    let selected = rules
        .iter()
        .map(|rule| (rule.tag_name.as_str(), rule))
        .collect::<BTreeMap<_, _>>();
    let mut command_steps = list_steps.to_vec();
    command_steps.push(MzLocationStep::index(start_index));
    for tag in simple_tag_spans(&text) {
        let Some(rule) = selected.get(tag.name()) else {
            continue;
        };
        if tag.value().trim().is_empty() {
            continue;
        }
        let descriptor = &definition.descriptors[rule.id];
        collector.add(
            TextGroupKind::EventCommand,
            MzLocation::value(source.clone(), command_steps.clone()),
            &descriptor.field_name,
            MzLocation::comment_tag(
                source.clone(),
                command_steps.clone(),
                tag.name(),
                tag.occurrence(),
            ),
            tag.value(),
            &descriptor.label,
        )?;
        matches.increment(rule.id);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extract_plugin_command(
    parameters: &[Value],
    command_index: usize,
    source: &MzSource,
    list_steps: &[MzLocationStep],
    list_locator: &str,
    command_tries: &PluginCommandTries,
    definition: &RulesDefinition,
    matches: &mut MatchCounts,
    collector: &mut GroupCollector,
) -> Result<(), BuildRulesSnapshotError> {
    let Some(plugin_name) = parameters.first().and_then(Value::as_str) else {
        return Err(invalid_target_at(list_locator, "357 插件名必须是字符串"));
    };
    let Some(command_name) = parameters.get(1).and_then(Value::as_str) else {
        return Err(invalid_target_at(list_locator, "357 命令名必须是字符串"));
    };
    let Some(trie) = command_tries
        .get(plugin_name)
        .and_then(|commands| commands.get(command_name))
    else {
        return Ok(());
    };
    let args = parameters.get(3).ok_or_else(|| {
        invalid_target_value(
            definition,
            trie.first_rule_id().unwrap_or(0),
            "357 缺少命令参数对象",
        )
    })?;
    if !args.is_object() {
        return invalid_target(
            definition,
            trie.first_rule_id().unwrap_or(0),
            "357 命令参数必须是对象",
        );
    }
    let mut command_steps = list_steps.to_vec();
    command_steps.push(MzLocationStep::index(command_index));
    let mut value_steps = command_steps.clone();
    value_steps.extend([MzLocationStep::key("parameters"), MzLocationStep::index(3)]);
    match_path_trie(
        args,
        trie,
        source,
        &mut value_steps,
        command_steps,
        None,
        TextGroupKind::EventCommand,
        true,
        definition,
        matches,
        collector,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::storage::file_system::ReadFile;

    #[test]
    fn accepts_only_the_five_small_sections_and_rejects_duplicate_json_keys() {
        let definition = RulesDefinition::parse("{}").expect("空对象应该表示空 Rules");
        assert!(definition.is_empty());

        for section in [
            "notes",
            "event_lists",
            "plugin_parameters",
            "plugin_commands",
            "standard_fields",
        ] {
            let text = format!(r#"{{"{section}":{{}}}}"#);
            assert!(matches!(
                RulesDefinition::parse(&text),
                Err(RulesDefinitionError::EmptyValue(context)) if context == section
            ));
        }

        assert!(matches!(
            RulesDefinition::parse(r#"{"metadata": {}}"#),
            Err(RulesDefinitionError::UnknownSection(section)) if section == "metadata"
        ));
        let duplicate = RulesDefinition::parse(
            r#"{"notes": {}, "notes": {"Items.json": {"[].note": ["Category"]}}}"#,
        )
        .expect_err("重复对象键必须失败");
        assert!(duplicate.to_string().contains("对象键重复"));
        assert!(matches!(
            RulesDefinition::parse(
                r#"{"notes":{"Items.json":{"[].note":["Category","Category"]}}}"#
            ),
            Err(RulesDefinitionError::DuplicateValue { .. })
        ));
        assert!(matches!(
            RulesDefinition::parse(r#"{"notes":{"Items.json":{"[].note":[]}}}"#),
            Err(RulesDefinitionError::EmptyValue(_))
        ));
        assert!(matches!(
            RulesDefinition::parse(r#"{"notes":{"Items.json":{"[].name":["Tag"]}}}"#),
            Err(RulesDefinitionError::InvalidPath { .. })
        ));
        assert!(matches!(
            RulesDefinition::parse(r#"{"plugin_commands":{"Quest":{"Show":["Text"]}}}"#),
            Err(RulesDefinitionError::MissingEventLists)
        ));
    }

    #[test]
    fn path_language_is_small_but_expressive() {
        for path in ["A.B", "A[].B", "A[3].B", "[\"key.with.dot\"]", "[].field"] {
            CompiledPath::parse(path)
                .unwrap_or_else(|error| panic!("路径 {path} 应该合法：{error}"));
        }
        for path in ["$.A", "A..B", "A[?(@.x)]", "A[*]", "A."] {
            assert!(
                CompiledPath::parse(path).is_err(),
                "路径 {path} 不应进入 Rules"
            );
        }

        let nonstandard =
            RulesDefinition::parse(r#"{"standard_fields":{"QuestData.json":["[].title"]}}"#)
                .expect_err("非标准 data 文件必须交给 Lua");
        assert!(nonstandard.to_string().contains("Lua"));
        let exact_map =
            RulesDefinition::parse(r#"{"standard_fields":{"Map001.json":["displayName"]}}"#)
                .expect_err("地图来源只接受 Map*.json");
        assert!(exact_map.to_string().contains("Map*.json"));
    }

    #[test]
    fn compact_contract_rejects_entries_without_extraction_intent() {
        for text in [
            r#"{"notes":{"Items.json":{}}}"#,
            r#"{"standard_fields":{"Items.json":[]}}"#,
            r#"{"plugin_parameters":{"QuestMenu":[]}}"#,
            r#"{"plugin_commands":{"QuestBook":{}}}"#,
            r#"{"plugin_commands":{"QuestBook":{"ShowQuest":[]}}}"#,
            r#"{"event_lists":{"Items.json":{"[].list":[]}}}"#,
        ] {
            assert!(
                RulesDefinition::parse(text).is_err(),
                "无提取意图的声明必须失败：{text}"
            );
        }
    }

    #[test]
    fn explicit_paths_ignore_unrelated_note_and_list_keys() {
        let definition = RulesDefinition::parse(
            r#"{
                "notes": {
                    "Items.json": {
                        "[].missing.note": ["Category"],
                        "[].note": ["Category"]
                    }
                },
                "event_lists": {
                    "Items.json": {
                        "[].events[].list": ["QuestDescription"]
                    }
                }
            }"#,
        )
        .expect("紧凑的 Note 与事件路径应该合法");
        assert_eq!(
            definition.descriptors.len(),
            2,
            "同一来源的同名标签必须跨路径共享一次匹配事实"
        );
        let documents = items_documents(json!([null, {
            "note": "<Category:武器>",
            "custom": {"note": 42},
            "list": 42,
            "events": [{"list": [
                {"code": 108, "parameters": ["<QuestDescription:任务说明>"]},
                {"code": 0, "parameters": []}
            ]}]
        }]));

        let snapshot =
            build_rules_snapshot(&definition, &documents).expect("只应解释显式声明的路径");
        let originals = snapshot
            .groups()
            .iter()
            .flat_map(|group| group.fields())
            .map(|field| field.original_text())
            .collect::<BTreeSet<_>>();
        assert_eq!(originals, BTreeSet::from(["任务说明", "武器"]));
    }

    #[test]
    fn map_root_note_path_derives_the_root_container() {
        let definition = RulesDefinition::parse(r#"{"notes":{"Map*.json":{"note":["MapTag"]}}}"#)
            .expect("地图根 Note 路径应该合法");
        let documents = MzProjectDocuments::new(
            [(MzDocumentId::Map(1), json!({"note": "<MapTag:地图备注>"}))]
                .into_iter()
                .collect(),
            Vec::new(),
        );

        let snapshot =
            build_rules_snapshot(&definition, &documents).expect("地图根 Note 应该生成空容器路径");
        let group = &snapshot.groups()[0];
        assert_eq!(group.kind(), TextGroupKind::Map);
        assert_eq!(group.group_location().to_string(), "data/Map001.json");
        assert!(matches!(
            group.fields()[0].exact_location(),
            MzLocation::NoteTag {
                container_steps,
                ..
            } if container_steps.is_empty()
        ));
        assert_eq!(
            group.fields()[0].exact_location().to_string(),
            "data/Map001.json.note#MapTag[0]"
        );
    }

    #[test]
    fn declared_structural_type_mismatches_fail_at_the_exact_path() {
        for (text, document, expected_path) in [
            (
                r#"{"notes":{"Items.json":{"[].note":["Tag"]}}}"#,
                json!([null, {"note": 42}]),
                "[].note",
            ),
            (
                r#"{"event_lists":{"Items.json":{"[].list":["Tag"]}}}"#,
                json!([null, {"list": 42}]),
                "[].list",
            ),
            (
                r#"{"event_lists":{"Items.json":{"[].events[].list":["Tag"]}}}"#,
                json!([null, {"events": {}}]),
                "[].events[].list",
            ),
        ] {
            let definition = RulesDefinition::parse(text).expect("结构错误应发生在数据匹配阶段");
            let error = build_rules_snapshot(&definition, &items_documents(document))
                .expect_err("显式路径的结构类型冲突必须失败");
            assert!(matches!(
                error,
                BuildRulesSnapshotError::InvalidTarget { locator, .. }
                    if locator.contains(expected_path)
            ));
        }
    }

    #[test]
    fn plugin_commands_use_only_declared_event_sources_and_allow_optional_routes() {
        let definition = RulesDefinition::parse(
            r#"{
                "event_lists": {
                    "Items.json": {
                        "[].missing[].list": [],
                        "[].list": ["QuestDescription"]
                    }
                },
                "plugin_commands": {
                    "QuestBook": {"ShowQuest": ["Text"]}
                }
            }"#,
        )
        .expect("插件命令可以使用任意显式标准数据来源");
        let selection = definition.document_selection();
        assert_eq!(
            selection.standard_files(),
            &BTreeSet::from([StandardDataFile::Items])
        );
        assert!(!selection.includes_all_maps());

        let documents = MzProjectDocuments::new(
            [
                (
                    MzDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"list": [
                        {"code": 108, "parameters": ["<QuestDescription:任务说明>"]},
                        {"code": 357, "parameters": [
                            "QuestBook", "ShowQuest", "显示任务", {"Text": "正文"}
                        ]},
                        {"code": 0, "parameters": []}
                    ]}]),
                ),
                (MzDocumentId::Map(1), json!({"list": 42})),
            ]
            .into_iter()
            .collect(),
            Vec::new(),
        );

        let snapshot = build_rules_snapshot(&definition, &documents)
            .expect("未声明的地图和未命中的可选路径都不应影响插件命令");
        let fields = snapshot
            .groups()
            .iter()
            .flat_map(|group| group.fields())
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields
                .iter()
                .map(|field| field.original_text())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["任务说明", "正文"])
        );
        assert!(fields.iter().any(|field| {
            field.original_text() == "正文"
                && field
                    .exact_location()
                    .to_string()
                    .starts_with("data/Items.json[1].list[1]")
        }));
    }

    #[test]
    fn all_five_sections_form_compound_groups_and_structured_addresses() {
        let definition = full_definition();
        let snapshot =
            build_rules_snapshot(&definition, &complete_documents()).expect("所有规则都应该命中");

        let item_group = snapshot
            .groups()
            .iter()
            .find(|group| group.group_location().to_string() == "data/Items.json[1]")
            .expect("道具扩展字段和 Note 应该归入同一个对象组");
        assert_eq!(item_group.kind(), TextGroupKind::DatabaseEntry);
        assert_eq!(item_group.fields().len(), 3);
        assert!(item_group.fields().iter().any(|field| {
            field.field_name() == "customDescription" && field.original_text() == "  锋利的剑  "
        }));
        assert!(item_group.fields().iter().any(|field| {
            field.exact_location().to_string() == "data/Items.json[1].note#Category[0]"
        }));

        assert!(
            snapshot
                .groups()
                .iter()
                .flat_map(|group| group.fields())
                .any(|field| {
                    field
                        .exact_location()
                        .to_string()
                        .contains("plugins.js[QuestMenu].Outer<json>[0]<json>.Name")
                        && field.original_text() == "主线"
                })
        );
        assert!(
            snapshot
                .groups()
                .iter()
                .flat_map(|group| group.fields())
                .any(|field| {
                    field
                        .exact_location()
                        .to_string()
                        .contains("parameters[3].Entries<json>[0].Title")
                        && field.original_text() == "第一章"
                })
        );
        assert!(
            snapshot
                .groups()
                .iter()
                .flat_map(|group| group.fields())
                .any(|field| {
                    field
                        .exact_location()
                        .to_string()
                        .contains("#comment:QuestDescription[0]")
                        && field.original_text() == "任务\n说明"
                })
        );
    }

    #[test]
    fn zero_match_wrong_type_and_duplicate_leaf_abort_before_a_snapshot_exists() {
        let documents = items_documents(json!([null, {"customDescription": 42}]));

        let no_match =
            RulesDefinition::parse(r#"{"standard_fields":{"Items.json":["[].missing"]}}"#)
                .expect("规则格式应该合法");
        assert!(matches!(
            build_rules_snapshot(&no_match, &documents),
            Err(BuildRulesSnapshotError::NoMatch { .. })
        ));

        let wrong_type = RulesDefinition::parse(
            r#"{"standard_fields":{"Items.json":["[].customDescription"]}}"#,
        )
        .expect("规则格式应该合法");
        assert!(matches!(
            build_rules_snapshot(&wrong_type, &documents),
            Err(BuildRulesSnapshotError::InvalidTarget { .. })
        ));

        let duplicate = RulesDefinition::parse(
            r#"{"standard_fields":{"Items.json":[
                "[].customDescription",
                "[][\"customDescription\"]"
            ]}}"#,
        )
        .expect("两种写法分别合法");
        let string_documents = items_documents(json!([null, {"customDescription": "说明"}]));
        assert!(matches!(
            build_rules_snapshot(&duplicate, &string_documents),
            Err(BuildRulesSnapshotError::DuplicateTarget { .. })
        ));
    }

    #[test]
    fn ten_thousand_paths_share_one_source_trie_across_a_large_sparse_array() {
        const COUNT: usize = 10_000;
        const SPARSE_ROWS: usize = 1_024;
        let mut fields = Map::new();
        let mut paths = Vec::with_capacity(COUNT);
        for index in 0..COUNT {
            let name = format!("field{index:05}");
            fields.insert(name.clone(), Value::String(format!("文本 {index}")));
            paths.push(Value::String(format!("[].{name}")));
        }
        let definition_text = json!({
            "standard_fields": {"Items.json": paths}
        })
        .to_string();
        let definition =
            RulesDefinition::parse(&definition_text).expect("万级极简路径应该可以直接解析");
        let mut rows = Vec::with_capacity(SPARSE_ROWS + 2);
        rows.push(Value::Null);
        rows.push(Value::Object(fields));
        for index in 0..SPARSE_ROWS {
            let mut row = Map::new();
            row.insert(
                format!("field{:05}", index % COUNT),
                Value::String(format!("稀疏文本 {index}")),
            );
            rows.push(Value::Object(row));
        }
        let documents = items_documents(Value::Array(rows));

        let snapshot = build_rules_snapshot(&definition, &documents)
            .expect("万级路径和大数组应该通过共享前缀树完成匹配");

        assert_eq!(snapshot.groups().len(), SPARSE_ROWS + 1);
        assert_eq!(
            snapshot
                .groups()
                .iter()
                .map(|group| group.fields().len())
                .sum::<usize>(),
            COUNT + SPARSE_ROWS
        );
    }

    #[tokio::test]
    async fn empty_definition_clears_rules_without_reading_mz_documents() {
        let harness = Harness::new(b"{}".to_vec(), MzProjectDocuments::empty());

        harness
            .service()
            .replace(&project(), PathBuf::from("empty.json"))
            .await
            .expect("空规则应该提交空快照");

        assert_eq!(harness.document_calls.load(Ordering::SeqCst), 0);
        let snapshots = harness.snapshots.lock().expect("快照锁不应中毒");
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots[0].groups().is_empty());
    }

    #[tokio::test]
    async fn service_reads_once_requests_exact_documents_and_persists_once() {
        let text = full_definition_text().as_bytes().to_vec();
        let harness = Harness::new(text, complete_documents());

        harness
            .service()
            .replace(&project(), PathBuf::from("rules.json"))
            .await
            .expect("完整规则应该成功");

        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.document_calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.snapshots.lock().expect("快照锁不应中毒").len(), 1);
        let selections = harness.selections.lock().expect("选择锁不应中毒");
        assert_eq!(selections.len(), 1);
        assert!(selections[0].includes_all_maps());
        assert!(selections[0].includes_plugins());
        assert!(
            selections[0]
                .standard_files()
                .contains(&StandardDataFile::Items)
        );
        assert!(
            selections[0]
                .standard_files()
                .contains(&StandardDataFile::CommonEvents)
        );
        assert!(
            selections[0]
                .standard_files()
                .contains(&StandardDataFile::Troops)
        );
    }

    #[tokio::test]
    async fn invalid_utf8_and_persist_failure_keep_their_stages_paths_and_sources() {
        let invalid = Harness::new(vec![0xff], MzProjectDocuments::empty());
        let error = invalid
            .service()
            .replace(&project(), PathBuf::from("invalid.json"))
            .await
            .expect_err("非法 UTF-8 必须失败");
        assert!(matches!(
            error,
            RulesExtractionError::InvalidUtf8 { rules_path, .. }
                if rules_path.as_path() == std::path::Path::new("invalid.json")
        ));
        assert_eq!(invalid.document_calls.load(Ordering::SeqCst), 0);
        assert!(invalid.snapshots.lock().expect("快照锁不应中毒").is_empty());

        let mut failing = Harness::new(b"{}".to_vec(), MzProjectDocuments::empty());
        failing.store_failure = true;
        let error = failing
            .service()
            .replace(&project(), PathBuf::from("persist.json"))
            .await
            .expect_err("Store 失败必须传播");
        assert!(matches!(
            &error,
            RulesExtractionError::Persist {
                rules_path,
                source: FakeError("persist")
            } if rules_path == &PathBuf::from("persist.json")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError("persist"))
        );
    }

    #[tokio::test]
    async fn dependency_and_no_match_failures_never_submit_a_partial_snapshot() {
        let mut read_failure = Harness::new(b"{}".to_vec(), MzProjectDocuments::empty());
        read_failure.file_failure = true;
        let error = read_failure
            .service()
            .replace(&project(), PathBuf::from("missing.json"))
            .await
            .expect_err("文件读取失败必须直接返回");
        assert!(matches!(
            error,
            RulesExtractionError::ReadRules { rules_path, .. }
                if rules_path.as_path() == std::path::Path::new("missing.json")
        ));
        assert_eq!(read_failure.document_calls.load(Ordering::SeqCst), 0);
        assert!(
            read_failure
                .snapshots
                .lock()
                .expect("快照锁不应中毒")
                .is_empty()
        );

        let mut document_failure = Harness::new(
            br#"{"standard_fields":{"Items.json":["[].name"]}}"#.to_vec(),
            items_documents(json!([null, {"name": "宝剑"}])),
        );
        document_failure.document_failure = true;
        let error = document_failure
            .service()
            .replace(&project(), PathBuf::from("rules.json"))
            .await
            .expect_err("MZ 文档读取失败必须直接返回");
        assert!(matches!(
            error,
            RulesExtractionError::ReadDocuments {
                source: FakeError("documents"),
                ..
            }
        ));
        assert!(
            document_failure
                .snapshots
                .lock()
                .expect("快照锁不应中毒")
                .is_empty()
        );

        let no_match = Harness::new(
            br#"{"standard_fields":{"Items.json":["[].missing"]}}"#.to_vec(),
            items_documents(json!([null, {"name": "宝剑"}])),
        );
        let error = no_match
            .service()
            .replace(&project(), PathBuf::from("rules.json"))
            .await
            .expect_err("零命中必须保留旧快照");
        assert!(matches!(error, RulesExtractionError::NoMatch { .. }));
        assert!(
            no_match
                .snapshots
                .lock()
                .expect("快照锁不应中毒")
                .is_empty()
        );
    }

    #[test]
    fn replacement_future_is_send() {
        fn assert_send(_: impl Send) {}

        let harness = Harness::new(b"{}".to_vec(), MzProjectDocuments::empty());
        let service = harness.service();
        let project = project();
        assert_send(service.replace(&project, PathBuf::from("rules.json")));
    }

    fn full_definition() -> RulesDefinition {
        RulesDefinition::parse(full_definition_text()).expect("完整规则定义应该合法")
    }

    fn full_definition_text() -> &'static str {
        r#"{
            "notes": {"Items.json": {"[].note": ["Category"]}},
            "event_lists": {
                "Map*.json": {
                    "events[].pages[].list": ["QuestDescription"]
                },
                "CommonEvents.json": {"[].list": []},
                "Troops.json": {"[].pages[].list": []}
            },
            "plugin_parameters": {
                "QuestMenu": ["WindowTitle", "Outer[].Name"]
            },
            "plugin_commands": {
                "QuestBook": {
                    "ShowQuest": ["Entries[].Title", "Entries[].Body"]
                }
            },
            "standard_fields": {
                "Items.json": ["[].customShortName", "[].customDescription"]
            }
        }"#
    }

    fn complete_documents() -> MzProjectDocuments {
        let mut documents = BTreeMap::new();
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Items),
            json!([null, {
                "name": "宝剑",
                "note": "<Category:武器>",
                "customShortName": "剑",
                "customDescription": "  锋利的剑  "
            }]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 357, "parameters": [
                    "QuestBook",
                    "ShowQuest",
                    "显示任务",
                    {"Entries": "[{\"Title\":\"第一章\",\"Body\":\"正文\"}]"}
                ]},
                {"code": 0, "parameters": []}
            ]}]),
        );
        documents.insert(MzDocumentId::Data(StandardDataFile::Troops), json!([null]));
        documents.insert(
            MzDocumentId::Map(1),
            json!({"events": [null, {"pages": [{"list": [
                {"code": 108, "parameters": ["<QuestDescription:任务"]},
                {"code": 408, "parameters": ["说明>"]},
                {"code": 0, "parameters": []}
            ]}]}]}),
        );
        let mut parameters = BTreeMap::new();
        parameters.insert("WindowTitle".to_owned(), "任务列表".to_owned());
        parameters.insert("Outer".to_owned(), r#"["{\"Name\":\"主线\"}"]"#.to_owned());
        MzProjectDocuments::new(
            documents,
            vec![PluginConfiguration::new(
                2,
                json!({
                    "name": "QuestMenu",
                    "status": true,
                    "description": "未知于 Rules 的字段仍由读取模型保留",
                    "parameters": parameters
                })
                .as_object()
                .expect("插件 fixture 必须是对象")
                .clone(),
            )],
        )
    }

    fn items_documents(items: Value) -> MzProjectDocuments {
        let mut documents = BTreeMap::new();
        documents.insert(MzDocumentId::Data(StandardDataFile::Items), items);
        MzProjectDocuments::new(documents, Vec::new())
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
    struct FakeCpu;

    impl CpuTaskExecutor for FakeCpu {
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
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone, Copy)]
    enum InjectedCpuFailure {
        Unavailable,
        TaskPanicked,
    }

    #[derive(Clone)]
    struct FailingCpu {
        calls: Arc<AtomicUsize>,
        fail_at: usize,
        failure: InjectedCpuFailure,
    }

    impl CpuTaskExecutor for FailingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.fail_at {
                return Err(match self.failure {
                    InjectedCpuFailure::Unavailable => {
                        CpuTaskExecutionError::Unavailable(FakeError("cpu unavailable"))
                    }
                    InjectedCpuFailure::TaskPanicked => CpuTaskExecutionError::TaskPanicked,
                });
            }
            Ok(task())
        }
    }

    #[test]
    fn config_keeps_the_explicit_scan_limit() {
        let config = RulesExtractionConfig::new(NonZeroUsize::new(4).expect("测试并发数必须非零"));

        assert_eq!(config.scan_concurrency().get(), 4);
    }

    #[tokio::test]
    async fn parallel_matching_obeys_the_stage_limit_and_keeps_serial_results() {
        let definition = full_definition();
        let expected =
            build_rules_snapshot(&definition, &complete_documents()).expect("串行规格实现应该成功");
        let calls = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let cpu = RecordingCpu {
            calls: Arc::clone(&calls),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::clone(&max_active),
        };

        let actual = build_rules_snapshot_parallel(
            &cpu,
            RulesExtractionConfig::new(NonZeroUsize::new(4).expect("测试并发数必须非零")),
            definition,
            complete_documents(),
        )
        .await;
        let actual = match actual {
            Ok(snapshot) => snapshot,
            Err(_) => panic!("并行 Rules 匹配应该成功"),
        };

        assert_eq!(actual, expected);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            7,
            "一次规则准备、五个真实来源任务和一次稳定汇总"
        );
        assert_eq!(max_active.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn duplicate_leaf_across_parallel_sources_keeps_duplicate_target_semantics() {
        let definition = RulesDefinition::parse(
            r#"{
                "standard_fields": {
                    "Map*.json": ["events[1].pages[0].list[0].parameters[3].Text"]
                },
                "event_lists": {
                    "Map*.json": {"events[].pages[].list": []}
                },
                "plugin_commands": {
                    "QuestBook": {"ShowQuest": ["Text"]}
                }
            }"#,
        )
        .expect("两种定位各自都应该合法");
        let documents = MzProjectDocuments::new(
            [
                (
                    MzDocumentId::Data(StandardDataFile::CommonEvents),
                    json!([null]),
                ),
                (MzDocumentId::Data(StandardDataFile::Troops), json!([null])),
                (
                    MzDocumentId::Map(1),
                    json!({"events": [null, {"pages": [{"list": [{
                        "code": 357,
                        "parameters": ["QuestBook", "ShowQuest", "显示任务", {"Text": "同一文本"}]
                    }, {"code": 0, "parameters": []}]}]}]}),
                ),
            ]
            .into_iter()
            .collect(),
            Vec::new(),
        );

        let error = build_rules_snapshot_parallel(
            &FakeCpu,
            RulesExtractionConfig::new(NonZeroUsize::new(2).expect("测试并发数必须非零")),
            definition,
            documents,
        )
        .await;

        assert!(matches!(
            error,
            Err(ParallelRulesBuildError::Build(
                BuildRulesSnapshotError::DuplicateTarget { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn cpu_failures_keep_parse_match_and_finalize_stages_distinct() {
        for (fail_at, failure, expected_stage) in [
            (1, InjectedCpuFailure::TaskPanicked, "parse"),
            (3, InjectedCpuFailure::Unavailable, "match"),
            (8, InjectedCpuFailure::TaskPanicked, "finalize"),
        ] {
            let harness = Harness::new(
                full_definition_text().as_bytes().to_vec(),
                complete_documents(),
            );
            let service = harness.service_with_cpu(FailingCpu {
                calls: Arc::new(AtomicUsize::new(0)),
                fail_at,
                failure,
            });

            let error = service
                .replace(&project(), PathBuf::from("rules.json"))
                .await
                .expect_err("注入的 CPU 失败必须传播");

            match (expected_stage, error) {
                (
                    "parse",
                    RulesExtractionError::ParseDefinitionCompute {
                        source: CpuTaskExecutionError::TaskPanicked,
                        ..
                    },
                ) => {}
                (
                    "match",
                    RulesExtractionError::MatchSourceCompute {
                        source: CpuTaskExecutionError::Unavailable(FakeError("cpu unavailable")),
                        ..
                    },
                ) => {}
                (
                    "finalize",
                    RulesExtractionError::BuildSnapshotCompute {
                        source: CpuTaskExecutionError::TaskPanicked,
                        ..
                    },
                ) => {}
                (expected, actual) => panic!("期望 {expected} CPU 阶段，实际为 {actual}"),
            }
            assert!(harness.snapshots.lock().expect("快照锁不应中毒").is_empty());
        }
    }

    #[derive(Clone)]
    struct FakeFileReader {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl FileReader for FakeFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(ReadFileError::Io {
                    path,
                    source: FakeError("read"),
                })
            } else {
                Ok(ReadFile::new(path, self.bytes.clone()))
            }
        }
    }

    #[derive(Clone)]
    struct FakeDocumentReader {
        documents: MzProjectDocuments,
        calls: Arc<AtomicUsize>,
        selections: Arc<Mutex<Vec<MzDocumentSelection>>>,
        fail: bool,
    }

    impl MzProjectDocumentReader for FakeDocumentReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            selection: MzDocumentSelection,
        ) -> Result<MzProjectDocuments, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.selections
                .lock()
                .expect("选择锁不应中毒")
                .push(selection);
            if self.fail {
                Err(FakeError("documents"))
            } else {
                Ok(self.documents.clone())
            }
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        snapshots: Arc<Mutex<Vec<RulesSnapshot>>>,
        fail: bool,
    }

    impl RulesSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_rules(
            &self,
            _project: &OpenedProject,
            snapshot: RulesSnapshot,
        ) -> Result<(), Self::Error> {
            self.snapshots
                .lock()
                .expect("快照锁不应中毒")
                .push(snapshot);
            if self.fail {
                Err(FakeError("persist"))
            } else {
                Ok(())
            }
        }
    }

    struct Harness {
        bytes: Vec<u8>,
        documents: MzProjectDocuments,
        file_calls: Arc<AtomicUsize>,
        document_calls: Arc<AtomicUsize>,
        selections: Arc<Mutex<Vec<MzDocumentSelection>>>,
        snapshots: Arc<Mutex<Vec<RulesSnapshot>>>,
        store_failure: bool,
        file_failure: bool,
        document_failure: bool,
    }

    impl Harness {
        fn new(bytes: Vec<u8>, documents: MzProjectDocuments) -> Self {
            Self {
                bytes,
                documents,
                file_calls: Arc::new(AtomicUsize::new(0)),
                document_calls: Arc::new(AtomicUsize::new(0)),
                selections: Arc::new(Mutex::new(Vec::new())),
                snapshots: Arc::new(Mutex::new(Vec::new())),
                store_failure: false,
                file_failure: false,
                document_failure: false,
            }
        }

        fn service(
            &self,
        ) -> RulesExtractionService<FakeFileReader, FakeDocumentReader, FakeStore, FakeCpu>
        {
            self.service_with_cpu(FakeCpu)
        }

        fn service_with_cpu<C>(
            &self,
            cpu: C,
        ) -> RulesExtractionService<FakeFileReader, FakeDocumentReader, FakeStore, C> {
            RulesExtractionService::new(
                FakeFileReader {
                    bytes: self.bytes.clone(),
                    calls: Arc::clone(&self.file_calls),
                    fail: self.file_failure,
                },
                FakeDocumentReader {
                    documents: self.documents.clone(),
                    calls: Arc::clone(&self.document_calls),
                    selections: Arc::clone(&self.selections),
                    fail: self.document_failure,
                },
                FakeStore {
                    snapshots: Arc::clone(&self.snapshots),
                    fail: self.store_failure,
                },
                cpu,
                RulesExtractionConfig::new(NonZeroUsize::new(4).expect("测试并发数必须非零")),
            )
        }
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应该合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }
}
