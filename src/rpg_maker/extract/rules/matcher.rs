//! 已受信 Rules 定义在冻结 RPG Maker 文档上的纯匹配与配方物化。
//!
//! 本模块只解释规则所声明的局部语义：来源、窄路径、嵌套 JSON 解码和 PCRE2
//! `text` 捕获。它不会在写回阶段保存或重新运行正则；匹配跨度会立即变成
//! `Literal` / `TextSlot` 配方。

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic,
};
use crate::json::{StackSafeJsonError, StackSafeJsonValue, from_str as parse_json};
use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, ProjectionModelError, ScalarFieldKey, TextProjectionRecipe,
    TextUnitRole,
};
use crate::rpg_maker::text::{
    DataFileName, DataFileNameError, MapId, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource,
    StandardDataFile, TextGroupKind,
};

#[cfg(test)]
use super::definition::RulesDefinition;
use super::definition::{
    CompiledPath, FileRuleSource, PathSegment, RuleDefinition, RuleSource, pcre2_error_detail,
};

/// 一组已经由文档读取边界冻结的 Rules 输入。
#[derive(Clone, Debug, Default)]
pub(super) struct RulesMatchInput {
    files: Vec<(String, Arc<StackSafeJsonValue>)>,
    plugins: Vec<RulesPlugin>,
}

impl RulesMatchInput {
    #[cfg(test)]
    pub(super) fn new(
        files: impl IntoIterator<Item = (String, Value)>,
        mut plugins: Vec<RulesPlugin>,
    ) -> Self {
        plugins.sort_by_key(RulesPlugin::index);
        Self {
            files: files
                .into_iter()
                .map(|(file, value)| (file, Arc::new(StackSafeJsonValue::new(value))))
                .collect(),
            plugins,
        }
    }

    pub(super) fn from_shared(
        files: impl IntoIterator<Item = (String, Arc<StackSafeJsonValue>)>,
        mut plugins: Vec<RulesPlugin>,
    ) -> Self {
        plugins.sort_by_key(RulesPlugin::index);
        Self {
            files: files.into_iter().collect(),
            plugins,
        }
    }
}

/// `plugins.js` 中一条已经过外壳校验的插件记录。
#[derive(Clone, Debug)]
pub(super) struct RulesPlugin {
    index: usize,
    name: String,
    enabled: bool,
    parameters: StackSafeJsonValue,
}

impl RulesPlugin {
    #[cfg(test)]
    pub(super) fn new(
        index: usize,
        name: impl Into<String>,
        enabled: bool,
        parameters: Map<String, Value>,
    ) -> Self {
        Self {
            index,
            name: name.into(),
            enabled,
            parameters: StackSafeJsonValue::new(Value::Object(parameters)),
        }
    }

    pub(super) fn from_stack_safe(
        index: usize,
        name: impl Into<String>,
        enabled: bool,
        parameters: StackSafeJsonValue,
    ) -> Self {
        debug_assert!(parameters.is_object());
        Self {
            index,
            name: name.into(),
            enabled,
            parameters,
        }
    }

    pub(super) const fn index(&self) -> usize {
        self.index
    }
}

/// Rules 物理根来源。路径中的 `DecodeJsonString` 记录可逆的嵌套编码边界。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RulesMatchSource {
    DataFile {
        file: String,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl fmt::Display for RulesMatchSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataFile { file } => write!(formatter, "data/{file}"),
            Self::PluginParameter {
                plugin_name,
                parameter_name,
                ..
            } => write!(formatter, "plugins.js[{plugin_name}][{parameter_name:?}]"),
        }
    }
}

/// 从来源根到最终字符串的一步；解码边界必须按相反顺序重新编码。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RulesValueStep {
    Key(String),
    Index(usize),
    DecodeJsonString,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JsonValueKind {
    Missing,
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl JsonValueKind {
    fn of(value: Option<&Value>) -> Self {
        match value {
            None => Self::Missing,
            Some(Value::Null) => Self::Null,
            Some(Value::Bool(_)) => Self::Boolean,
            Some(Value::Number(_)) => Self::Number,
            Some(Value::String(_)) => Self::String,
            Some(Value::Array(_)) => Self::Array,
            Some(Value::Object(_)) => Self::Object,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RulesDiagnosticSource {
    DataFile {
        file: String,
    },
    Plugin {
        plugin_index: usize,
        plugin_name: String,
    },
    Command {
        file: String,
        code: i64,
        parameter: usize,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RulesMatchContext {
    source: RulesDiagnosticSource,
    has_declared_path: bool,
}

impl RulesMatchContext {
    fn for_rule(rule: &RuleDefinition, source: RulesDiagnosticSource) -> Self {
        Self {
            source,
            has_declared_path: rule.path().is_some(),
        }
    }

    fn safe_detail(&self) -> String {
        let mut detail = match &self.source {
            RulesDiagnosticSource::DataFile { file } => {
                format!("source=data_file; file={}", json_string(file))
            }
            RulesDiagnosticSource::Plugin {
                plugin_index,
                plugin_name,
            } => format!(
                "source=plugin; plugin_index={plugin_index}; plugin={}",
                json_string(plugin_name)
            ),
            RulesDiagnosticSource::Command {
                file,
                code,
                parameter,
            } => format!(
                "source=command; file={}; code={code}; parameter={parameter}",
                json_string(file)
            ),
        };
        if self.has_declared_path {
            detail.push_str("; target=path");
        } else {
            detail.push_str("; target=command_parameter");
        }
        detail
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("字符串始终可以编码为 JSON")
}

/// 最终字符串的物化替换配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatchedRuleTarget {
    rule_number: usize,
    kind: TextGroupKind,
    source: RulesMatchSource,
    group_steps: Vec<RulesValueStep>,
    steps: Vec<RulesValueStep>,
    expected_text: String,
    units: Vec<MatchedRuleUnit>,
    parts: Vec<MatchedRulePart>,
    source_order: usize,
    physical_order: Vec<usize>,
}

impl MatchedRuleTarget {
    pub(super) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(super) fn units(&self) -> &[MatchedRuleUnit] {
        &self.units
    }

    pub(super) fn physical_location(&self) -> Result<RpgMakerLocation, RulesMatchError> {
        Ok(RpgMakerLocation::value(
            self.shared_source()?,
            self.steps.iter().map(shared_step).collect(),
        ))
    }

    pub(super) fn group_location(&self) -> Result<RpgMakerLocation, RulesMatchError> {
        Ok(RpgMakerLocation::value(
            self.shared_source()?,
            self.group_steps.iter().map(shared_step).collect(),
        ))
    }

    fn shared_source(&self) -> Result<RpgMakerSource, RulesMatchError> {
        match &self.source {
            RulesMatchSource::DataFile { file } => DataFileName::parse(file.clone())
                .map(RpgMakerSource::data_file)
                .map_err(|source| RulesMatchError::InvalidTarget {
                    rule_number: self.rule_number,
                    reason: RulesInvalidTargetReason::InvalidDataFileName {
                        file: file.clone(),
                        source,
                    },
                }),
            RulesMatchSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Ok(RpgMakerSource::plugin_parameter(
                *plugin_index,
                plugin_name.clone(),
                parameter_name.clone(),
            )),
        }
    }

    pub(super) fn projection_recipe(&self) -> Result<TextProjectionRecipe, RulesMatchError> {
        let parts = self
            .parts
            .iter()
            .map(|part| match part {
                MatchedRulePart::Literal(literal) => Ok(DirectTextPart::Literal(literal.clone())),
                MatchedRulePart::TextSlot { unit_index } => Ok(DirectTextPart::TextSlot {
                    role: self.role_for(*unit_index),
                }),
            })
            .collect::<Result<Vec<_>, RulesMatchError>>()?;
        DirectTextRecipe::new(self.physical_location()?, self.expected_text.clone(), parts)
            .map(TextProjectionRecipe::Direct)
            .map_err(|source| RulesMatchError::InvalidMaterialization {
                rule_number: self.rule_number,
                reason: RulesMaterializationReason::Projection {
                    at: self.steps.clone(),
                    source,
                },
            })
    }

    pub(super) fn role_for(&self, unit_index: usize) -> TextUnitRole {
        let relative_steps = self
            .steps
            .strip_prefix(self.group_steps.as_slice())
            .unwrap_or(&self.steps);
        let mut key = String::new();
        for step in relative_steps {
            match step {
                RulesValueStep::Key(value) => {
                    key.push('[');
                    key.push_str(
                        &serde_json::to_string(value).expect("JSON 对象键始终可以编码为字符串"),
                    );
                    key.push(']');
                }
                RulesValueStep::Index(index) => key.push_str(&format!("[{index}]")),
                RulesValueStep::DecodeJsonString => key.push_str("<json>"),
            }
        }
        if !key.is_empty() {
            key.push('.');
        }
        key.push_str(&format!("text[{unit_index}]"));
        ScalarFieldKey::new(key)
            .map(TextUnitRole::Scalar)
            .expect("生成的 Rules 标量角色键始终非空")
    }

    /// 使用单元值重建最终字符串，供模型构造边界校验配方完整性。
    #[cfg(test)]
    pub(super) fn materialize(&self, values: &[String]) -> Result<String, RulesMatchError> {
        if values.len() != self.units.len() {
            return Err(RulesMatchError::InvalidMaterialization {
                rule_number: self.rule_number,
                reason: RulesMaterializationReason::UnitCount {
                    at: self.steps.clone(),
                    expected: self.units.len(),
                    actual: values.len(),
                },
            });
        }
        Ok(materialize_parts(&self.parts, values).expect("配方构造已保证 TextSlot 引用存在的单元"))
    }
}

/// 同一最终字符串中一个可独立翻译的语义单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatchedRuleUnit {
    source_text: String,
}

impl MatchedRuleUnit {
    pub(super) fn source_text(&self) -> &str {
        &self.source_text
    }
}

/// 最终字符串中冻结外壳与可翻译槽的稳定顺序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MatchedRulePart {
    Literal(String),
    TextSlot { unit_index: usize },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TargetKey {
    source: RulesMatchSource,
    steps: Vec<RulesValueStep>,
}

/// 匹配全部规则；任何规则零命中或目标冲突都会放弃整个候选结果。
#[cfg(test)]
pub(super) fn match_rules(
    definition: &RulesDefinition,
    input: &RulesMatchInput,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let plan = build_source_match_plan(definition.rules().to_vec(), input.clone());
    let (rule_count, work_units) = plan.into_parts();
    finish_source_matches(
        rule_count,
        work_units
            .into_iter()
            .map(RulesSourceMatchWorkUnit::run)
            .collect(),
    )
}

/// 按物理来源建立 CPU 工作单元；事件列表在每个文件工作单元中只枚举一次，再按
/// command code 分发给所有适用规则。
pub(super) struct RulesSourceMatchPlan {
    rule_count: usize,
    work_units: Vec<RulesSourceMatchWorkUnit>,
}

impl RulesSourceMatchPlan {
    pub(super) fn into_parts(self) -> (usize, Vec<RulesSourceMatchWorkUnit>) {
        (self.rule_count, self.work_units)
    }
}

pub(super) enum RulesSourceMatchWorkUnit {
    File {
        rules: Arc<Vec<RuleDefinition>>,
        source_order: usize,
        file: String,
        root: Arc<StackSafeJsonValue>,
        file_rule_indexes: Vec<usize>,
        command_rule_indexes: Arc<Vec<usize>>,
    },
    Plugin {
        rules: Arc<Vec<RuleDefinition>>,
        source_order: usize,
        plugin: RulesPlugin,
        rule_indexes: Vec<usize>,
    },
}

pub(super) struct RulesSourceMatchResult {
    contributions: Vec<RuleMatchContribution>,
    #[cfg(test)]
    event_document_scans: usize,
    #[cfg(test)]
    shared_path_node_visits: usize,
}

struct RuleMatchContribution {
    rule_index: usize,
    outcome: Result<Vec<MatchedRuleTarget>, RulesMatchError>,
}

impl RulesSourceMatchWorkUnit {
    pub(super) fn run(self) -> RulesSourceMatchResult {
        match self {
            Self::File {
                rules,
                source_order,
                file,
                root,
                file_rule_indexes,
                command_rule_indexes,
            } => {
                let (mut contributions, shared_path_node_visits) = match_file_rules_on_source(
                    &rules,
                    &file_rule_indexes,
                    &file,
                    &root,
                    source_order,
                );
                let scans_event_document = !command_rule_indexes.is_empty();
                if scans_event_document {
                    contributions.extend(match_command_rules_on_file(
                        &rules,
                        &command_rule_indexes,
                        &file,
                        &root,
                        source_order,
                    ));
                }
                #[cfg(not(test))]
                let _ = shared_path_node_visits;
                RulesSourceMatchResult {
                    contributions,
                    #[cfg(test)]
                    event_document_scans: usize::from(scans_event_document),
                    #[cfg(test)]
                    shared_path_node_visits,
                }
            }
            Self::Plugin {
                rules,
                source_order,
                plugin,
                rule_indexes,
            } => {
                let (contributions, shared_path_node_visits) =
                    match_plugin_rules_on_source(&rules, &rule_indexes, &plugin, source_order);
                #[cfg(not(test))]
                let _ = shared_path_node_visits;
                RulesSourceMatchResult {
                    contributions,
                    #[cfg(test)]
                    event_document_scans: 0,
                    #[cfg(test)]
                    shared_path_node_visits,
                }
            }
        }
    }
}

pub(super) fn build_source_match_plan(
    rules: Vec<RuleDefinition>,
    input: RulesMatchInput,
) -> RulesSourceMatchPlan {
    let rule_count = rules.len();
    let rules = Arc::new(rules);
    let mut exact_file_rule_indexes = HashMap::<String, Vec<usize>>::new();
    let mut all_map_rule_indexes = Vec::new();
    let mut plugin_rule_indexes = HashMap::<String, Vec<usize>>::new();
    let mut command_rule_indexes = Vec::new();
    for (rule_index, rule) in rules.iter().enumerate() {
        match rule.source().clone() {
            RuleSource::File(FileRuleSource::Exact(file)) => {
                exact_file_rule_indexes
                    .entry(file)
                    .or_default()
                    .push(rule_index);
            }
            RuleSource::File(FileRuleSource::AllMaps) => {
                all_map_rule_indexes.push(rule_index);
            }
            RuleSource::Plugin(name) => plugin_rule_indexes
                .entry(name)
                .or_default()
                .push(rule_index),
            RuleSource::Command { .. } => command_rule_indexes.push(rule_index),
        }
    }
    let command_rule_indexes = Arc::new(command_rule_indexes);
    let no_command_rule_indexes = Arc::new(Vec::new());

    let RulesMatchInput { files, plugins } = input;
    let file_count = files.len();
    let mut work_units = Vec::with_capacity(file_count.saturating_add(plugins.len()));
    for (source_order, (file, root)) in files.into_iter().enumerate() {
        let mut applicable_file_rule_indexes =
            exact_file_rule_indexes.remove(&file).unwrap_or_default();
        if is_canonical_map_file(&file) {
            applicable_file_rule_indexes.extend(all_map_rule_indexes.iter().copied());
        }
        applicable_file_rule_indexes.sort_unstable();
        let applicable_command_rule_indexes = if is_event_document(&file) {
            Arc::clone(&command_rule_indexes)
        } else {
            Arc::clone(&no_command_rule_indexes)
        };
        if !applicable_file_rule_indexes.is_empty() || !applicable_command_rule_indexes.is_empty() {
            work_units.push(RulesSourceMatchWorkUnit::File {
                rules: Arc::clone(&rules),
                source_order,
                file,
                root,
                file_rule_indexes: applicable_file_rule_indexes,
                command_rule_indexes: applicable_command_rule_indexes,
            });
        }
    }

    let mut plugin_source_order = file_count;
    for plugin in plugins {
        let parameter_count = plugin
            .parameters
            .as_object()
            .expect("Rules 插件参数根始终是对象")
            .len();
        if let Some(rule_indexes) = plugin_rule_indexes.get(&plugin.name) {
            work_units.push(RulesSourceMatchWorkUnit::Plugin {
                rules: Arc::clone(&rules),
                source_order: plugin_source_order,
                plugin,
                rule_indexes: rule_indexes.clone(),
            });
        }
        plugin_source_order = plugin_source_order.saturating_add(parameter_count);
    }

    RulesSourceMatchPlan {
        rule_count,
        work_units,
    }
}

pub(super) fn finish_source_matches(
    rule_count: usize,
    results: Vec<RulesSourceMatchResult>,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut per_rule = (0..rule_count)
        .map(|_| (Vec::new(), None))
        .collect::<Vec<(Vec<MatchedRuleTarget>, Option<RulesMatchError>)>>();
    for result in results {
        for contribution in result.contributions {
            let (targets, error) = &mut per_rule[contribution.rule_index];
            if error.is_some() {
                continue;
            }
            match contribution.outcome {
                Ok(source_targets) => targets.extend(source_targets),
                Err(source) => *error = Some(source),
            }
        }
    }

    let mut matches = Vec::with_capacity(rule_count);
    for (rule_index, (targets, error)) in per_rule.into_iter().enumerate() {
        if let Some(error) = error {
            return Err(error);
        }
        if targets.is_empty() {
            return Err(RulesMatchError::NoNonBlankMatch {
                rule_number: rule_index + 1,
            });
        }
        matches.push(targets);
    }
    merge_ordered_rule_matches(matches)
}

fn merge_ordered_rule_matches(
    matches: Vec<Vec<MatchedRuleTarget>>,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut target_indexes = HashMap::<TargetKey, usize>::new();
    let mut targets = Vec::<MatchedRuleTarget>::new();
    for rule_targets in matches {
        for target in rule_targets {
            let key = TargetKey {
                source: target.source.clone(),
                steps: target.steps.clone(),
            };
            let second_rule = target.rule_number;
            if let Some(previous_index) = target_indexes.get(&key).copied() {
                let previous = &targets[previous_index];
                return Err(RulesMatchError::DuplicateTarget {
                    first_rule: previous.rule_number,
                    second_rule,
                    source: previous.source.clone(),
                    steps: previous.steps.clone(),
                });
            }
            target_indexes.insert(key, targets.len());
            targets.push(target);
        }
    }
    targets.sort_by(|left, right| {
        left.source_order
            .cmp(&right.source_order)
            .then_with(|| left.physical_order.cmp(&right.physical_order))
            .then_with(|| left.steps.cmp(&right.steps))
    });
    Ok(targets)
}

fn match_file_rules_on_source(
    rules: &[RuleDefinition],
    rule_indexes: &[usize],
    file: &str,
    root: &Value,
    source_order: usize,
) -> (Vec<RuleMatchContribution>, usize) {
    let (outcomes, node_visits) = match_shared_paths(rules, rule_indexes, root);
    let source = RulesMatchSource::DataFile {
        file: file.to_owned(),
    };
    let kind = if file == StandardDataFile::System.file_name() {
        TextGroupKind::System
    } else if is_canonical_map_file(file) {
        TextGroupKind::Map
    } else {
        TextGroupKind::DatabaseEntry
    };
    let contributions = rule_indexes
        .iter()
        .zip(outcomes)
        .map(|(rule_index, outcome)| {
            let rule = &rules[*rule_index];
            let context = RulesMatchContext::for_rule(
                rule,
                RulesDiagnosticSource::DataFile {
                    file: file.to_owned(),
                },
            );
            RuleMatchContribution {
                rule_index: rule.rule_number() - 1,
                outcome: outcome
                    .map_err(|source| source.with_context(context))
                    .map(|local| {
                        local
                            .into_iter()
                            .map(|terminal| MatchedRuleTarget {
                                kind,
                                source: source.clone(),
                                source_order,
                                physical_order: terminal.physical_order,
                                ..terminal.target
                            })
                            .collect()
                    }),
            }
        })
        .collect();
    (contributions, node_visits)
}

fn match_plugin_rules_on_source(
    rules: &[RuleDefinition],
    rule_indexes: &[usize],
    plugin: &RulesPlugin,
    source_order: usize,
) -> (Vec<RuleMatchContribution>, usize) {
    let (outcomes, node_visits) = if plugin.enabled {
        match_shared_paths(rules, rule_indexes, &plugin.parameters)
    } else {
        ((0..rule_indexes.len()).map(|_| Ok(Vec::new())).collect(), 0)
    };
    let contributions = rule_indexes
        .iter()
        .zip(outcomes)
        .map(|(rule_index, outcome)| {
            let rule = &rules[*rule_index];
            let context = RulesMatchContext::for_rule(
                rule,
                RulesDiagnosticSource::Plugin {
                    plugin_index: plugin.index,
                    plugin_name: plugin.name.clone(),
                },
            );
            RuleMatchContribution {
                rule_index: rule.rule_number() - 1,
                outcome: outcome
                    .and_then(|local| materialize_plugin_targets(rule, plugin, source_order, local))
                    .map_err(|source| source.with_context(context)),
            }
        })
        .collect();
    (contributions, node_visits)
}

fn materialize_plugin_targets(
    rule: &RuleDefinition,
    plugin: &RulesPlugin,
    source_order: usize,
    local: Vec<LocalTarget>,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut targets = Vec::with_capacity(local.len());
    for mut terminal in local {
        let Some((RulesValueStep::Key(parameter_name), tail)) = terminal.target.steps.split_first()
        else {
            return Err(RulesMatchError::InvalidTarget {
                rule_number: rule.rule_number(),
                reason: RulesInvalidTargetReason::PluginPathMissingParameter {
                    at: terminal.target.steps.clone(),
                },
            });
        };
        let parameter_name = parameter_name.clone();
        let steps = tail.to_vec();
        let group_steps = plugin_relative_steps(
            rule.rule_number(),
            &parameter_name,
            &terminal.target.group_steps,
        )?;
        let Some((parameter_index, physical_order)) = terminal.physical_order.split_first() else {
            unreachable!("插件参数路径的遍历顺序必然包含参数位置")
        };
        terminal.target.kind = TextGroupKind::PluginParameter;
        terminal.target.source = RulesMatchSource::PluginParameter {
            plugin_index: plugin.index,
            plugin_name: plugin.name.clone(),
            parameter_name,
        };
        terminal.target.group_steps = group_steps;
        terminal.target.steps = steps;
        terminal.target.source_order = source_order + *parameter_index;
        terminal.target.physical_order = physical_order.to_vec();
        targets.push(terminal.target);
    }
    Ok(targets)
}

/// 同一物理来源上全部 file/plugin 路径的共享前缀树。`rule_indexes` 保存的是本次
/// 来源局部结果槽，而不是业务规则编号；这样相同前缀只读取、解码和枚举一次。
#[derive(Default)]
struct SharedPathNode {
    rule_indexes: Vec<usize>,
    terminal_rule_indexes: Vec<usize>,
    key_children: HashMap<String, SharedPathNode>,
    index_children: HashMap<usize, SharedPathNode>,
    any_index_child: Option<Box<SharedPathNode>>,
}

impl SharedPathNode {
    fn insert(&mut self, segments: &[PathSegment], rule_index: usize) {
        let mut node = self;
        node.rule_indexes.push(rule_index);
        for segment in segments {
            node = match segment {
                PathSegment::Key(key) => node.key_children.entry(key.clone()).or_default(),
                PathSegment::Index(index) => node.index_children.entry(*index).or_default(),
                PathSegment::AnyIndex => node
                    .any_index_child
                    .get_or_insert_with(|| Box::new(Self::default())),
            };
            node.rule_indexes.push(rule_index);
        }
        node.terminal_rule_indexes.push(rule_index);
    }

    fn has_children(&self) -> bool {
        !self.key_children.is_empty()
            || !self.index_children.is_empty()
            || self.any_index_child.is_some()
    }
}

impl Drop for SharedPathNode {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.move_children_to(&mut pending);
        while let Some(mut node) = pending.pop() {
            node.move_children_to(&mut pending);
        }
    }
}

impl SharedPathNode {
    /// `SharedPathNode` 可以与规则路径一样深；先移走后代，避免默认析构递归整棵树。
    fn move_children_to(&mut self, pending: &mut Vec<Self>) {
        pending.extend(std::mem::take(&mut self.key_children).into_values());
        pending.extend(std::mem::take(&mut self.index_children).into_values());
        if let Some(child) = self.any_index_child.take() {
            pending.push(*child);
        }
    }
}

enum SharedPathWalkAction<'a> {
    Node {
        node: &'a SharedPathNode,
        value: *const Value,
    },
    Children {
        node: &'a SharedPathNode,
        value: *const Value,
    },
    ArrayIndex {
        node: &'a SharedPathNode,
        value: *const Value,
        index: usize,
    },
    EnterNode {
        node: &'a SharedPathNode,
        value: *const Value,
        step: RulesValueStep,
        physical_order: usize,
    },
    EnterChildren {
        node: &'a SharedPathNode,
        value: *const Value,
        decoded_values_len: usize,
    },
    Restore {
        steps_len: usize,
        physical_order_len: usize,
        decoded_values_len: Option<usize>,
    },
}

fn match_shared_paths(
    rules: &[RuleDefinition],
    rule_indexes: &[usize],
    root: &Value,
) -> (Vec<Result<Vec<LocalTarget>, RulesMatchError>>, usize) {
    let mut path_root = SharedPathNode::default();
    for (outcome_index, rule_index) in rule_indexes.iter().enumerate() {
        let path = rules[*rule_index]
            .path()
            .expect("file/plugin 规则已在解析边界保证存在 path");
        path_root.insert(path.segments(), outcome_index);
    }
    let mut matcher = SharedPathMatcher::new(rules, rule_indexes);
    matcher.walk_node(&path_root, root, &mut Vec::new(), &mut Vec::new());
    matcher.into_parts()
}

struct SharedPathMatcher<'a> {
    rules: &'a [RuleDefinition],
    rule_indexes: &'a [usize],
    outcomes: Vec<Result<Vec<LocalTarget>, RulesMatchError>>,
    node_visits: usize,
}

impl<'a> SharedPathMatcher<'a> {
    fn new(rules: &'a [RuleDefinition], rule_indexes: &'a [usize]) -> Self {
        Self {
            rules,
            rule_indexes,
            outcomes: (0..rule_indexes.len()).map(|_| Ok(Vec::new())).collect(),
            node_visits: 0,
        }
    }

    fn into_parts(self) -> (Vec<Result<Vec<LocalTarget>, RulesMatchError>>, usize) {
        (self.outcomes, self.node_visits)
    }

    fn walk_node(
        &mut self,
        node: &SharedPathNode,
        value: &Value,
        steps: &mut Vec<RulesValueStep>,
        physical_order: &mut Vec<usize>,
    ) {
        let initial_steps_len = steps.len();
        let initial_physical_order_len = physical_order.len();
        let mut decoded_values = Vec::<Pin<Box<StackSafeJsonValue>>>::new();
        let mut work = vec![SharedPathWalkAction::Node {
            node,
            value: value as *const Value,
        }];

        // 工作项中的指针只来自本次调用期间保持不变的来源树，或来自下面 arena 中
        // 地址稳定的 Box。解码子树的 Restore 工作项始终位于其全部后代之后。
        while let Some(action) = work.pop() {
            match action {
                SharedPathWalkAction::Node { node, value } => {
                    if !self.has_active_rule(&node.rule_indexes) {
                        continue;
                    }
                    self.node_visits = self.node_visits.saturating_add(1);

                    // SAFETY: `value` 满足上述工作栈不变量，且遍历期间不会修改 JSON 树。
                    let value = unsafe { &*value };
                    for outcome_index in &node.terminal_rule_indexes {
                        if self.outcomes[*outcome_index].is_err() {
                            continue;
                        }
                        let rule = &self.rules[self.rule_indexes[*outcome_index]];
                        let mut local = Vec::new();
                        match visit_terminal(
                            rule,
                            value,
                            steps.clone(),
                            physical_order,
                            &[],
                            true,
                            &mut local,
                        ) {
                            Ok(()) => self.outcomes[*outcome_index]
                                .as_mut()
                                .expect("已排除失败结果")
                                .extend(local),
                            Err(error) => self.outcomes[*outcome_index] = Err(error),
                        }
                    }

                    work.push(SharedPathWalkAction::Children {
                        node,
                        value: value as *const Value,
                    });
                }
                SharedPathWalkAction::Children { node, value } => {
                    self.walk_children(node, value, steps, &mut work, &mut decoded_values);
                }
                SharedPathWalkAction::ArrayIndex { node, value, index } => {
                    // SAFETY: `value` 指向当前 Children 工作项已经确认的稳定数组。
                    let array =
                        unsafe { (&*value).as_array().expect("已确认当前值是数组") };
                    let Some(child_value) = array.get(index) else {
                        continue;
                    };
                    if index + 1 < array.len() {
                        work.push(SharedPathWalkAction::ArrayIndex {
                            node,
                            value,
                            index: index + 1,
                        });
                    }
                    if !child_value.is_null()
                        && let Some(child) = node.any_index_child.as_deref()
                        && self.has_active_rule(&child.rule_indexes)
                    {
                        work.push(SharedPathWalkAction::EnterNode {
                            node: child,
                            value: child_value as *const Value,
                            step: RulesValueStep::Index(index),
                            physical_order: index,
                        });
                    }
                    if let Some(child) = node.index_children.get(&index)
                        && self.has_active_rule(&child.rule_indexes)
                    {
                        work.push(SharedPathWalkAction::EnterNode {
                            node: child,
                            value: child_value as *const Value,
                            step: RulesValueStep::Index(index),
                            physical_order: index,
                        });
                    }
                }
                SharedPathWalkAction::EnterNode {
                    node,
                    value,
                    step,
                    physical_order: next_physical_order,
                } => {
                    if !self.has_active_rule(&node.rule_indexes) {
                        continue;
                    }
                    let steps_len = steps.len();
                    let physical_order_len = physical_order.len();
                    steps.push(step);
                    physical_order.push(next_physical_order);
                    work.push(SharedPathWalkAction::Restore {
                        steps_len,
                        physical_order_len,
                        decoded_values_len: None,
                    });
                    work.push(SharedPathWalkAction::Node { node, value });
                }
                SharedPathWalkAction::EnterChildren {
                    node,
                    value,
                    decoded_values_len,
                } => {
                    let steps_len = steps.len();
                    let physical_order_len = physical_order.len();
                    steps.push(RulesValueStep::DecodeJsonString);
                    work.push(SharedPathWalkAction::Restore {
                        steps_len,
                        physical_order_len,
                        decoded_values_len: Some(decoded_values_len),
                    });
                    work.push(SharedPathWalkAction::Children { node, value });
                }
                SharedPathWalkAction::Restore {
                    steps_len,
                    physical_order_len,
                    decoded_values_len,
                } => {
                    steps.truncate(steps_len);
                    physical_order.truncate(physical_order_len);
                    if let Some(decoded_values_len) = decoded_values_len {
                        decoded_values.truncate(decoded_values_len);
                    }
                }
            }
        }

        steps.truncate(initial_steps_len);
        physical_order.truncate(initial_physical_order_len);
    }

    fn walk_children<'node>(
        &mut self,
        node: &'node SharedPathNode,
        value: *const Value,
        steps: &[RulesValueStep],
        work: &mut Vec<SharedPathWalkAction<'node>>,
        decoded_values: &mut Vec<Pin<Box<StackSafeJsonValue>>>,
    ) {
        if !node.has_children() || !self.has_active_child(node) {
            return;
        }

        // SAFETY: 调用方的显式工作栈保证来源树或 Box 解码树在工作项完成前存活。
        let value_ref = unsafe { &*value };
        if let Value::String(encoded) = value_ref {
            let decoded = match parse_json(encoded) {
                Ok(decoded) => decoded,
                Err(source) => {
                    let reason = RulesInvalidTargetReason::NestedJsonDecode {
                        phase: "path_traversal",
                        at: steps.to_vec(),
                        source: Arc::new(source),
                    };
                    self.fail_active_children(node, &reason);
                    return;
                }
            };
            let decoded_values_len = decoded_values.len();
            let decoded = Box::pin(decoded);
            let decoded_value = &**decoded as *const Value;
            decoded_values.push(decoded);
            self.node_visits = self.node_visits.saturating_add(1);
            work.push(SharedPathWalkAction::EnterChildren {
                node,
                value: decoded_value,
                decoded_values_len,
            });
            return;
        }

        match value_ref {
            Value::Object(object) => {
                for (index, child) in &node.index_children {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedArray {
                            at: steps.to_vec(),
                            index: Some(*index),
                            actual: JsonValueKind::Object,
                        },
                    );
                }
                if let Some(child) = node.any_index_child.as_deref() {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedArray {
                            at: steps.to_vec(),
                            index: None,
                            actual: JsonValueKind::Object,
                        },
                    );
                }
                for (key_order, (key, child_value)) in object.iter().enumerate().rev() {
                    let Some(child) = node.key_children.get(key) else {
                        continue;
                    };
                    if !self.has_active_rule(&child.rule_indexes) {
                        continue;
                    }
                    work.push(SharedPathWalkAction::EnterNode {
                        node: child,
                        value: child_value as *const Value,
                        step: RulesValueStep::Key(key.clone()),
                        physical_order: key_order,
                    });
                }
            }
            Value::Array(array) => {
                for (key, child) in &node.key_children {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedObject {
                            at: steps.to_vec(),
                            key: key.clone(),
                            actual: JsonValueKind::Array,
                        },
                    );
                }
                if !array.is_empty() {
                    work.push(SharedPathWalkAction::ArrayIndex {
                        node,
                        value,
                        index: 0,
                    });
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {
                let actual = JsonValueKind::of(Some(value_ref));
                for (key, child) in &node.key_children {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedObject {
                            at: steps.to_vec(),
                            key: key.clone(),
                            actual,
                        },
                    );
                }
                for (index, child) in &node.index_children {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedArray {
                            at: steps.to_vec(),
                            index: Some(*index),
                            actual,
                        },
                    );
                }
                if let Some(child) = node.any_index_child.as_deref() {
                    self.fail_with_reason(
                        &child.rule_indexes,
                        &RulesInvalidTargetReason::ExpectedArray {
                            at: steps.to_vec(),
                            index: None,
                            actual,
                        },
                    );
                }
            }
            Value::String(_) => unreachable!("字符串已经在路径继续前完成解码"),
        }
    }

    fn has_active_rule(&self, rule_indexes: &[usize]) -> bool {
        rule_indexes
            .iter()
            .any(|rule_index| self.outcomes[*rule_index].is_ok())
    }

    fn has_active_child(&self, node: &SharedPathNode) -> bool {
        node.key_children
            .values()
            .chain(node.index_children.values())
            .chain(node.any_index_child.as_deref())
            .any(|child| self.has_active_rule(&child.rule_indexes))
    }

    fn fail_active_children(&mut self, node: &SharedPathNode, reason: &RulesInvalidTargetReason) {
        for child in node.key_children.values() {
            self.fail_with_reason(&child.rule_indexes, reason);
        }
        for child in node.index_children.values() {
            self.fail_with_reason(&child.rule_indexes, reason);
        }
        if let Some(child) = node.any_index_child.as_deref() {
            self.fail_with_reason(&child.rule_indexes, reason);
        }
    }

    fn fail_with_reason(&mut self, rule_indexes: &[usize], reason: &RulesInvalidTargetReason) {
        for outcome_index in rule_indexes {
            if self.outcomes[*outcome_index].is_err() {
                continue;
            }
            let rule_number = self.rules[self.rule_indexes[*outcome_index]].rule_number();
            self.outcomes[*outcome_index] = Err(RulesMatchError::InvalidTarget {
                rule_number,
                reason: reason.clone(),
            });
        }
    }
}

#[cfg(test)]
fn match_file_rule_on_source_reference(
    rule: &RuleDefinition,
    file: &str,
    root: &Value,
    source_order: usize,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let path = rule.path().expect("file 规则已在解析边界保证存在 path");
    let source = RulesMatchSource::DataFile {
        file: file.to_owned(),
    };
    let kind = if file == StandardDataFile::System.file_name() {
        TextGroupKind::System
    } else if is_canonical_map_file(file) {
        TextGroupKind::Map
    } else {
        TextGroupKind::DatabaseEntry
    };
    let mut targets = Vec::new();
    walk_rule_path(
        rule,
        root,
        path,
        RulePathTarget {
            source,
            kind,
            source_order,
            base_steps: Vec::new(),
            base_physical_order: Vec::new(),
            default_group_steps: Vec::new(),
            group_by_terminal_parent: true,
        },
        &mut targets,
    )?;
    Ok(targets)
}

#[cfg(test)]
fn match_plugin_rule_on_source_reference(
    rule: &RuleDefinition,
    plugin: &RulesPlugin,
    source_order: usize,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    if !plugin.enabled {
        return Ok(Vec::new());
    }
    let path = rule.path().expect("plugin 规则已在解析边界保证存在 path");
    let local = collect_local_targets(
        rule,
        &plugin.parameters,
        path.segments(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        true,
    )?;
    materialize_plugin_targets(rule, plugin, source_order, local)
}

fn plugin_relative_steps(
    rule_number: usize,
    parameter_name: &str,
    group_steps: &[RulesValueStep],
) -> Result<Vec<RulesValueStep>, RulesMatchError> {
    if group_steps.is_empty() {
        return Ok(Vec::new());
    }
    let Some((RulesValueStep::Key(group_parameter), tail)) = group_steps.split_first() else {
        return Err(RulesMatchError::InvalidTarget {
            rule_number,
            reason: RulesInvalidTargetReason::PluginGroupMissingParameter {
                at: group_steps.to_vec(),
            },
        });
    };
    if group_parameter != parameter_name {
        return Err(RulesMatchError::InvalidTarget {
            rule_number,
            reason: RulesInvalidTargetReason::PluginGroupCrossesParameters {
                expected_parameter: parameter_name.to_owned(),
                actual_parameter: group_parameter.clone(),
                at: group_steps.to_vec(),
            },
        });
    }
    Ok(tail.to_vec())
}

fn match_command_rules_on_file(
    rules: &[RuleDefinition],
    rule_indexes: &[usize],
    file: &str,
    root: &Value,
    source_order: usize,
) -> Vec<RuleMatchContribution> {
    let mut by_code = HashMap::<i64, Vec<usize>>::new();
    for (outcome_index, rule_index) in rule_indexes.iter().enumerate() {
        let rule = &rules[*rule_index];
        let RuleSource::Command { code, .. } = rule.source() else {
            unreachable!("command 工作单元只包含 command 规则");
        };
        by_code.entry(*code).or_default().push(outcome_index);
    }
    let mut outcomes = (0..rule_indexes.len())
        .map(|_| Ok(Vec::<MatchedRuleTarget>::new()))
        .collect::<Vec<Result<Vec<_>, RulesMatchError>>>();

    for event_list in event_lists(file, root) {
        for (command_index, command) in event_list.commands.iter().enumerate() {
            let Some(command) = command.as_object() else {
                continue;
            };
            let Some(code) = command.get("code").and_then(Value::as_i64) else {
                continue;
            };
            let Some(outcome_indexes) = by_code.get(&code) else {
                continue;
            };
            let parameters_value = command.get("parameters");
            let parameters = parameters_value.and_then(Value::as_array);
            let parameters_order = command.keys().position(|key| key == "parameters");
            for outcome_index in outcome_indexes {
                if outcomes[*outcome_index].is_err() {
                    continue;
                }
                let rule = &rules[rule_indexes[*outcome_index]];
                let RuleSource::Command { parameter, .. } = rule.source() else {
                    unreachable!("command 工作单元只包含 command 规则");
                };
                let mut parameters_path = event_list.steps.clone();
                parameters_path.push(RulesValueStep::Index(command_index));
                parameters_path.push(RulesValueStep::Key("parameters".to_owned()));
                let context = RulesMatchContext::for_rule(
                    rule,
                    RulesDiagnosticSource::Command {
                        file: file.to_owned(),
                        code,
                        parameter: *parameter,
                    },
                );
                let result = parameters
                    .ok_or_else(|| RulesMatchError::InvalidTarget {
                        rule_number: rule.rule_number(),
                        reason: RulesInvalidTargetReason::CommandParametersType {
                            file: file.to_owned(),
                            code,
                            parameter: *parameter,
                            at: parameters_path.clone(),
                            actual: JsonValueKind::of(parameters_value),
                        },
                    })
                    .and_then(|parameters| {
                        parameters
                            .get(*parameter)
                            .ok_or_else(|| RulesMatchError::InvalidTarget {
                                rule_number: rule.rule_number(),
                                reason: RulesInvalidTargetReason::CommandParameterMissing {
                                    file: file.to_owned(),
                                    code,
                                    parameter: *parameter,
                                    available: parameters.len(),
                                    at: parameters_path.clone(),
                                },
                            })
                    })
                    .and_then(|selected| {
                        match_command_rule_at(
                            rule,
                            CommandRuleMatch {
                                file,
                                source_order,
                                list_steps: &event_list.steps,
                                list_physical_order: &event_list.physical_order,
                                command_index,
                                parameters_order: parameters_order
                                    .expect("已取得的 parameters 数组必然拥有物理位置"),
                                parameter: *parameter,
                                selected,
                            },
                        )
                    })
                    .map_err(|source| source.with_context(context));
                match result {
                    Ok(targets) => outcomes[*outcome_index]
                        .as_mut()
                        .expect("已排除失败结果")
                        .extend(targets),
                    Err(error) => outcomes[*outcome_index] = Err(error),
                }
            }
        }
    }

    rule_indexes
        .iter()
        .map(|rule_index| &rules[*rule_index])
        .zip(outcomes)
        .map(|(rule, outcome)| RuleMatchContribution {
            rule_index: rule.rule_number() - 1,
            outcome,
        })
        .collect()
}

struct CommandRuleMatch<'a> {
    file: &'a str,
    source_order: usize,
    list_steps: &'a [RulesValueStep],
    list_physical_order: &'a [usize],
    command_index: usize,
    parameters_order: usize,
    parameter: usize,
    selected: &'a Value,
}

fn match_command_rule_at(
    rule: &RuleDefinition,
    command: CommandRuleMatch<'_>,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut command_steps = command.list_steps.to_vec();
    command_steps.push(RulesValueStep::Index(command.command_index));
    let mut target_steps = command_steps.clone();
    target_steps.push(RulesValueStep::Key("parameters".to_owned()));
    target_steps.push(RulesValueStep::Index(command.parameter));
    let mut target_physical_order = command.list_physical_order.to_vec();
    target_physical_order.push(command.command_index);
    target_physical_order.push(command.parameters_order);
    target_physical_order.push(command.parameter);
    let source = RulesMatchSource::DataFile {
        file: command.file.to_owned(),
    };
    let mut targets = Vec::new();
    if let Some(path) = rule.path() {
        walk_rule_path(
            rule,
            command.selected,
            path,
            RulePathTarget {
                source,
                kind: TextGroupKind::EventCommand,
                source_order: command.source_order,
                base_steps: target_steps,
                base_physical_order: target_physical_order,
                default_group_steps: command_steps,
                group_by_terminal_parent: path.segments().iter().any(|segment| {
                    matches!(segment, PathSegment::AnyIndex | PathSegment::Index(_))
                }),
            },
            &mut targets,
        )?;
    } else {
        let mut local = Vec::new();
        visit_terminal(
            rule,
            command.selected,
            target_steps,
            &target_physical_order,
            &command_steps,
            false,
            &mut local,
        )?;
        targets.extend(local.into_iter().map(|terminal| MatchedRuleTarget {
            kind: TextGroupKind::EventCommand,
            source: source.clone(),
            source_order: command.source_order,
            physical_order: terminal.physical_order,
            ..terminal.target
        }));
    }
    Ok(targets)
}

struct RulePathTarget {
    source: RulesMatchSource,
    kind: TextGroupKind,
    source_order: usize,
    base_steps: Vec<RulesValueStep>,
    base_physical_order: Vec<usize>,
    default_group_steps: Vec<RulesValueStep>,
    group_by_terminal_parent: bool,
}

fn walk_rule_path(
    rule: &RuleDefinition,
    root: &Value,
    path: &CompiledPath,
    target: RulePathTarget,
    output: &mut Vec<MatchedRuleTarget>,
) -> Result<(), RulesMatchError> {
    let local = collect_local_targets(
        rule,
        root,
        path.segments(),
        target.base_steps,
        target.base_physical_order,
        target.default_group_steps,
        target.group_by_terminal_parent,
    )?;
    output.extend(local.into_iter().map(|terminal| MatchedRuleTarget {
        kind: target.kind,
        source: target.source.clone(),
        source_order: target.source_order,
        physical_order: terminal.physical_order,
        ..terminal.target
    }));
    Ok(())
}

struct LocalTarget {
    physical_order: Vec<usize>,
    target: MatchedRuleTarget,
}

fn collect_local_targets(
    rule: &RuleDefinition,
    root: &Value,
    segments: &[PathSegment],
    mut base_steps: Vec<RulesValueStep>,
    mut base_physical_order: Vec<usize>,
    default_group_steps: Vec<RulesValueStep>,
    group_by_terminal_parent: bool,
) -> Result<Vec<LocalTarget>, RulesMatchError> {
    let mut walker = PathWalker {
        rule,
        segments,
        default_group_steps: &default_group_steps,
        group_by_terminal_parent,
        output: Vec::new(),
    };
    walker.walk(root, 0, &mut base_steps, &mut base_physical_order)?;
    Ok(walker.output)
}

struct PathWalker<'a> {
    rule: &'a RuleDefinition,
    segments: &'a [PathSegment],
    default_group_steps: &'a [RulesValueStep],
    group_by_terminal_parent: bool,
    output: Vec<LocalTarget>,
}

enum PathWalkAction {
    Value {
        value: *const Value,
        segment_index: usize,
    },
    ArrayIndex {
        value: *const Value,
        segment_index: usize,
        index: usize,
    },
    Enter {
        value: *const Value,
        segment_index: usize,
        step: RulesValueStep,
        physical_order: Option<usize>,
        decoded_values_len: Option<usize>,
    },
    Restore {
        steps_len: usize,
        physical_order_len: usize,
        decoded_values_len: Option<usize>,
    },
}

impl PathWalker<'_> {
    fn walk(
        &mut self,
        value: &Value,
        segment_index: usize,
        steps: &mut Vec<RulesValueStep>,
        physical_order: &mut Vec<usize>,
    ) -> Result<(), RulesMatchError> {
        let initial_steps_len = steps.len();
        let initial_physical_order_len = physical_order.len();
        let mut decoded_values = Vec::<Pin<Box<StackSafeJsonValue>>>::new();
        let mut work = vec![PathWalkAction::Value {
            value: value as *const Value,
            segment_index,
        }];

        // 与共享路径匹配相同，工作项只引用整个遍历期间不变的来源树，或引用地址
        // 稳定且由对应 Restore 工作项保活的 Box 解码树。
        let result = (|| -> Result<(), RulesMatchError> {
            while let Some(action) = work.pop() {
                match action {
                    PathWalkAction::Value {
                        value,
                        segment_index,
                    } => {
                        // SAFETY: `value` 满足上述显式工作栈的来源或 Box 生命周期不变量。
                        let value_ref = unsafe { &*value };
                        if segment_index == self.segments.len() {
                            visit_terminal(
                                self.rule,
                                value_ref,
                                steps.clone(),
                                physical_order,
                                self.default_group_steps,
                                self.group_by_terminal_parent,
                                &mut self.output,
                            )?;
                            continue;
                        }

                        if let Value::String(encoded) = value_ref {
                            let decoded = parse_json(encoded).map_err(|source| {
                                RulesMatchError::InvalidTarget {
                                    rule_number: self.rule.rule_number(),
                                    reason: RulesInvalidTargetReason::NestedJsonDecode {
                                        phase: "path_traversal",
                                        at: steps.clone(),
                                        source: Arc::new(source),
                                    },
                                }
                            })?;
                            let decoded_values_len = decoded_values.len();
                            let decoded = Box::pin(decoded);
                            let decoded_value = &**decoded as *const Value;
                            decoded_values.push(decoded);
                            work.push(PathWalkAction::Enter {
                                value: decoded_value,
                                segment_index,
                                step: RulesValueStep::DecodeJsonString,
                                physical_order: None,
                                decoded_values_len: Some(decoded_values_len),
                            });
                            continue;
                        }

                        match &self.segments[segment_index] {
                            PathSegment::Key(key) => {
                                let object = value_ref.as_object().ok_or_else(|| {
                                    RulesMatchError::InvalidTarget {
                                        rule_number: self.rule.rule_number(),
                                        reason: RulesInvalidTargetReason::ExpectedObject {
                                            at: steps.clone(),
                                            key: key.clone(),
                                            actual: JsonValueKind::of(Some(value_ref)),
                                        },
                                    }
                                })?;
                                if let Some(child) = object.get(key) {
                                    let key_order = object
                                        .keys()
                                        .position(|candidate| candidate == key)
                                        .expect("已取得的对象字段必然拥有物理位置");
                                    work.push(PathWalkAction::Enter {
                                        value: child as *const Value,
                                        segment_index: segment_index + 1,
                                        step: RulesValueStep::Key(key.clone()),
                                        physical_order: Some(key_order),
                                        decoded_values_len: None,
                                    });
                                }
                            }
                            PathSegment::Index(index) => {
                                let array = value_ref.as_array().ok_or_else(|| {
                                    RulesMatchError::InvalidTarget {
                                        rule_number: self.rule.rule_number(),
                                        reason: RulesInvalidTargetReason::ExpectedArray {
                                            at: steps.clone(),
                                            index: Some(*index),
                                            actual: JsonValueKind::of(Some(value_ref)),
                                        },
                                    }
                                })?;
                                if let Some(child) = array.get(*index) {
                                    work.push(PathWalkAction::Enter {
                                        value: child as *const Value,
                                        segment_index: segment_index + 1,
                                        step: RulesValueStep::Index(*index),
                                        physical_order: Some(*index),
                                        decoded_values_len: None,
                                    });
                                }
                            }
                            PathSegment::AnyIndex => {
                                let array = value_ref.as_array().ok_or_else(|| {
                                    RulesMatchError::InvalidTarget {
                                        rule_number: self.rule.rule_number(),
                                        reason: RulesInvalidTargetReason::ExpectedArray {
                                            at: steps.clone(),
                                            index: None,
                                            actual: JsonValueKind::of(Some(value_ref)),
                                        },
                                    }
                                })?;
                                if !array.is_empty() {
                                    work.push(PathWalkAction::ArrayIndex {
                                        value,
                                        segment_index: segment_index + 1,
                                        index: 0,
                                    });
                                }
                            }
                        }
                    }
                    PathWalkAction::ArrayIndex {
                        value,
                        segment_index,
                        mut index,
                    } => {
                        // SAFETY: `value` 指向 Value 工作项已经确认且仍被保活的数组。
                        let array =
                            unsafe { (&*value).as_array().expect("已确认当前值是数组") };
                        while array.get(index).is_some_and(Value::is_null) {
                            index += 1;
                        }
                        let Some(child) = array.get(index) else {
                            continue;
                        };
                        if index + 1 < array.len() {
                            work.push(PathWalkAction::ArrayIndex {
                                value,
                                segment_index,
                                index: index + 1,
                            });
                        }
                        work.push(PathWalkAction::Enter {
                            value: child as *const Value,
                            segment_index,
                            step: RulesValueStep::Index(index),
                            physical_order: Some(index),
                            decoded_values_len: None,
                        });
                    }
                    PathWalkAction::Enter {
                        value,
                        segment_index,
                        step,
                        physical_order: next_physical_order,
                        decoded_values_len,
                    } => {
                        let steps_len = steps.len();
                        let physical_order_len = physical_order.len();
                        steps.push(step);
                        if let Some(next_physical_order) = next_physical_order {
                            physical_order.push(next_physical_order);
                        }
                        work.push(PathWalkAction::Restore {
                            steps_len,
                            physical_order_len,
                            decoded_values_len,
                        });
                        work.push(PathWalkAction::Value {
                            value,
                            segment_index,
                        });
                    }
                    PathWalkAction::Restore {
                        steps_len,
                        physical_order_len,
                        decoded_values_len,
                    } => {
                        steps.truncate(steps_len);
                        physical_order.truncate(physical_order_len);
                        if let Some(decoded_values_len) = decoded_values_len {
                            decoded_values.truncate(decoded_values_len);
                        }
                    }
                }
            }
            Ok(())
        })();

        // 失败可以跳过尚未执行的 Restore；对调用方仍必须恢复传入的基础路径。
        steps.truncate(initial_steps_len);
        physical_order.truncate(initial_physical_order_len);
        result
    }
}

fn visit_terminal(
    rule: &RuleDefinition,
    value: &Value,
    mut steps: Vec<RulesValueStep>,
    physical_order: &[usize],
    default_group_steps: &[RulesValueStep],
    group_by_terminal_parent: bool,
    output: &mut Vec<LocalTarget>,
) -> Result<(), RulesMatchError> {
    let final_value;
    let value = if rule.decode_json() {
        let encoded = value
            .as_str()
            .ok_or_else(|| RulesMatchError::InvalidTarget {
                rule_number: rule.rule_number(),
                reason: RulesInvalidTargetReason::DecodeJsonTargetType {
                    at: steps.clone(),
                    actual: JsonValueKind::of(Some(value)),
                },
            })?;
        final_value = parse_json(encoded).map_err(|source| RulesMatchError::InvalidTarget {
            rule_number: rule.rule_number(),
            reason: RulesInvalidTargetReason::NestedJsonDecode {
                phase: "decode_json_target",
                at: steps.clone(),
                source: Arc::new(source),
            },
        })?;
        steps.push(RulesValueStep::DecodeJsonString);
        &final_value
    } else {
        value
    };
    let text = value
        .as_str()
        .ok_or_else(|| RulesMatchError::InvalidTarget {
            rule_number: rule.rule_number(),
            reason: RulesInvalidTargetReason::FinalTargetType {
                at: steps.clone(),
                actual: JsonValueKind::of(Some(value)),
            },
        })?;
    let group_steps = if group_by_terminal_parent {
        terminal_group_steps(&steps, default_group_steps)
    } else {
        default_group_steps.to_vec()
    };
    if let Some(target) = materialize_target(rule, text, group_steps, steps.clone())? {
        output.push(LocalTarget {
            physical_order: physical_order.to_vec(),
            target,
        });
    }
    Ok(())
}

fn terminal_group_steps(
    target_steps: &[RulesValueStep],
    default_group_steps: &[RulesValueStep],
) -> Vec<RulesValueStep> {
    let Some(last_value_step) = target_steps
        .iter()
        .rposition(|step| !matches!(step, RulesValueStep::DecodeJsonString))
    else {
        return default_group_steps.to_vec();
    };
    match target_steps[last_value_step] {
        RulesValueStep::Key(_) => target_steps[..last_value_step].to_vec(),
        RulesValueStep::Index(_) => target_steps.to_vec(),
        RulesValueStep::DecodeJsonString => unreachable!("已排除 JSON 解码步骤"),
    }
}

fn materialize_target(
    rule: &RuleDefinition,
    text: &str,
    group_steps: Vec<RulesValueStep>,
    steps: Vec<RulesValueStep>,
) -> Result<Option<MatchedRuleTarget>, RulesMatchError> {
    let (units, parts) = if let Some(pattern) = rule.pattern() {
        let mut captures = Vec::new();
        for result in pattern.regex().captures_iter(text.as_bytes()) {
            let result = result.map_err(|source| RulesMatchError::PatternMatch {
                rule_number: rule.rule_number(),
                at: steps.clone(),
                source,
            })?;
            let whole = result.get(0).expect("PCRE2 成功 captures 必须包含完整匹配");
            if whole.start() == whole.end() {
                return Err(RulesMatchError::ZeroWidthMatch {
                    rule_number: rule.rule_number(),
                    at: steps.clone(),
                    start: whole.start(),
                    end: whole.end(),
                });
            }
            let capture = result
                .name("text")
                .ok_or(RulesMatchError::MissingTextCapture {
                    rule_number: rule.rule_number(),
                    at: steps.clone(),
                })?;
            if !valid_capture_range(
                text,
                whole.start(),
                whole.end(),
                capture.start(),
                capture.end(),
            ) {
                return Err(RulesMatchError::InvalidCaptureRange {
                    rule_number: rule.rule_number(),
                    at: steps.clone(),
                    text_bytes: text.len(),
                    whole_start: whole.start(),
                    whole_end: whole.end(),
                    capture_start: capture.start(),
                    capture_end: capture.end(),
                });
            }
            captures.push((capture.start(), capture.end()));
        }
        materialize_captures(rule.rule_number(), text, captures, &steps)?
    } else if text.trim().is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (
            vec![MatchedRuleUnit {
                source_text: text.to_owned(),
            }],
            vec![MatchedRulePart::TextSlot { unit_index: 0 }],
        )
    };
    if units.is_empty() {
        return Ok(None);
    }
    let originals = units
        .iter()
        .map(|unit| unit.source_text.as_str())
        .collect::<Vec<_>>();
    if materialize_parts(&parts, &originals).as_deref() != Some(text) {
        return Err(RulesMatchError::InvalidMaterialization {
            rule_number: rule.rule_number(),
            reason: RulesMaterializationReason::RoundTripMismatch { at: steps.clone() },
        });
    }
    Ok(Some(MatchedRuleTarget {
        rule_number: rule.rule_number(),
        kind: TextGroupKind::DatabaseEntry,
        source: RulesMatchSource::DataFile {
            file: String::new(),
        },
        group_steps,
        steps,
        expected_text: text.to_owned(),
        units,
        parts,
        source_order: 0,
        physical_order: Vec::new(),
    }))
}

fn valid_capture_range(
    text: &str,
    whole_start: usize,
    whole_end: usize,
    capture_start: usize,
    capture_end: usize,
) -> bool {
    whole_start <= whole_end
        && whole_end <= text.len()
        && capture_start <= capture_end
        && capture_end <= text.len()
        && whole_start <= capture_start
        && capture_end <= whole_end
        && text.is_char_boundary(whole_start)
        && text.is_char_boundary(whole_end)
        && text.is_char_boundary(capture_start)
        && text.is_char_boundary(capture_end)
}

fn materialize_captures(
    rule_number: usize,
    text: &str,
    captures: Vec<(usize, usize)>,
    at: &[RulesValueStep],
) -> Result<(Vec<MatchedRuleUnit>, Vec<MatchedRulePart>), RulesMatchError> {
    let mut units = Vec::new();
    let mut parts = Vec::new();
    let mut cursor = 0;
    let mut previous_capture_end = 0;
    for (start, end) in captures {
        if start == end {
            return Err(RulesMatchError::ZeroWidthMatch {
                rule_number,
                at: at.to_vec(),
                start,
                end,
            });
        }
        if start < previous_capture_end {
            return Err(RulesMatchError::OverlappingMatch {
                rule_number,
                at: at.to_vec(),
                previous_end: previous_capture_end,
                start,
                end,
            });
        }
        previous_capture_end = end;
        if text[start..end].trim().is_empty() {
            continue;
        }
        if cursor < start {
            push_literal(&mut parts, &text[cursor..start]);
        }
        let unit_index = units.len();
        units.push(MatchedRuleUnit {
            source_text: text[start..end].to_owned(),
        });
        parts.push(MatchedRulePart::TextSlot { unit_index });
        cursor = end;
    }
    if cursor < text.len() {
        push_literal(&mut parts, &text[cursor..]);
    }
    Ok((units, parts))
}

fn push_literal(parts: &mut Vec<MatchedRulePart>, literal: &str) {
    if literal.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(MatchedRulePart::Literal(previous)) => previous.push_str(literal),
        _ => parts.push(MatchedRulePart::Literal(literal.to_owned())),
    }
}

fn materialize_parts<T>(parts: &[MatchedRulePart], values: &[T]) -> Option<String>
where
    T: AsRef<str>,
{
    let mut output = String::new();
    for part in parts {
        match part {
            MatchedRulePart::Literal(literal) => output.push_str(literal),
            MatchedRulePart::TextSlot { unit_index } => {
                output.push_str(values.get(*unit_index)?.as_ref());
            }
        }
    }
    Some(output)
}

fn is_event_document(file: &str) -> bool {
    is_canonical_map_file(file) || matches!(file, "CommonEvents.json" | "Troops.json")
}

fn is_canonical_map_file(file: &str) -> bool {
    MapId::from_canonical_file_name(file).is_some()
}

fn shared_step(step: &RulesValueStep) -> RpgMakerLocationStep {
    match step {
        RulesValueStep::Key(key) => RpgMakerLocationStep::key(key),
        RulesValueStep::Index(index) => RpgMakerLocationStep::index(*index),
        RulesValueStep::DecodeJsonString => RpgMakerLocationStep::DecodeJsonString,
    }
}

struct EventCommandList<'a> {
    steps: Vec<RulesValueStep>,
    physical_order: Vec<usize>,
    commands: &'a [Value],
}

fn event_lists<'a>(file: &str, root: &'a Value) -> Vec<EventCommandList<'a>> {
    let mut lists = Vec::new();
    if is_canonical_map_file(file) {
        let Some(root) = root.as_object() else {
            return lists;
        };
        let Some(events) = root.get("events").and_then(Value::as_array) else {
            return lists;
        };
        let events_order = object_key_order(root, "events");
        for (event_index, event) in events.iter().enumerate() {
            let Some(event) = event.as_object() else {
                continue;
            };
            let Some(pages) = event.get("pages").and_then(Value::as_array) else {
                continue;
            };
            let pages_order = object_key_order(event, "pages");
            for (page_index, page) in pages.iter().enumerate() {
                let Some(page) = page.as_object() else {
                    continue;
                };
                let Some(list) = page.get("list").and_then(Value::as_array) else {
                    continue;
                };
                let list_order = object_key_order(page, "list");
                lists.push(EventCommandList {
                    steps: vec![
                        RulesValueStep::Key("events".to_owned()),
                        RulesValueStep::Index(event_index),
                        RulesValueStep::Key("pages".to_owned()),
                        RulesValueStep::Index(page_index),
                        RulesValueStep::Key("list".to_owned()),
                    ],
                    physical_order: vec![
                        events_order,
                        event_index,
                        pages_order,
                        page_index,
                        list_order,
                    ],
                    commands: list,
                });
            }
        }
    } else if file == "CommonEvents.json" {
        let Some(events) = root.as_array() else {
            return lists;
        };
        for (event_index, event) in events.iter().enumerate() {
            let Some(event) = event.as_object() else {
                continue;
            };
            let Some(list) = event.get("list").and_then(Value::as_array) else {
                continue;
            };
            let list_order = object_key_order(event, "list");
            lists.push(EventCommandList {
                steps: vec![
                    RulesValueStep::Index(event_index),
                    RulesValueStep::Key("list".to_owned()),
                ],
                physical_order: vec![event_index, list_order],
                commands: list,
            });
        }
    } else if file == "Troops.json" {
        let Some(troops) = root.as_array() else {
            return lists;
        };
        for (troop_index, troop) in troops.iter().enumerate() {
            let Some(troop) = troop.as_object() else {
                continue;
            };
            let Some(pages) = troop.get("pages").and_then(Value::as_array) else {
                continue;
            };
            let pages_order = object_key_order(troop, "pages");
            for (page_index, page) in pages.iter().enumerate() {
                let Some(page) = page.as_object() else {
                    continue;
                };
                let Some(list) = page.get("list").and_then(Value::as_array) else {
                    continue;
                };
                let list_order = object_key_order(page, "list");
                lists.push(EventCommandList {
                    steps: vec![
                        RulesValueStep::Index(troop_index),
                        RulesValueStep::Key("pages".to_owned()),
                        RulesValueStep::Index(page_index),
                        RulesValueStep::Key("list".to_owned()),
                    ],
                    physical_order: vec![troop_index, pages_order, page_index, list_order],
                    commands: list,
                });
            }
        }
    }
    lists
}

fn object_key_order(object: &Map<String, Value>, key: &str) -> usize {
    object
        .keys()
        .position(|candidate| candidate == key)
        .expect("已取得的对象字段必然拥有物理位置")
}

/// Rules 匹配阶段的输入或目标错误。
#[derive(Debug)]
pub(crate) enum RulesMatchError {
    Context {
        context: RulesMatchContext,
        source: Box<RulesMatchError>,
    },
    NoNonBlankMatch {
        rule_number: usize,
    },
    InvalidTarget {
        rule_number: usize,
        reason: RulesInvalidTargetReason,
    },
    PatternMatch {
        rule_number: usize,
        at: Vec<RulesValueStep>,
        source: pcre2::Error,
    },
    ZeroWidthMatch {
        rule_number: usize,
        at: Vec<RulesValueStep>,
        start: usize,
        end: usize,
    },
    OverlappingMatch {
        rule_number: usize,
        at: Vec<RulesValueStep>,
        previous_end: usize,
        start: usize,
        end: usize,
    },
    MissingTextCapture {
        rule_number: usize,
        at: Vec<RulesValueStep>,
    },
    InvalidCaptureRange {
        rule_number: usize,
        at: Vec<RulesValueStep>,
        text_bytes: usize,
        whole_start: usize,
        whole_end: usize,
        capture_start: usize,
        capture_end: usize,
    },
    DuplicateTarget {
        first_rule: usize,
        second_rule: usize,
        source: RulesMatchSource,
        steps: Vec<RulesValueStep>,
    },
    InvalidMaterialization {
        rule_number: usize,
        reason: RulesMaterializationReason,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum RulesInvalidTargetReason {
    InvalidDataFileName {
        file: String,
        source: DataFileNameError,
    },
    PluginFieldType {
        plugin_index: usize,
        plugin_name: String,
        field: &'static str,
        expected: &'static str,
        actual: JsonValueKind,
    },
    PluginPathMissingParameter {
        at: Vec<RulesValueStep>,
    },
    PluginGroupMissingParameter {
        at: Vec<RulesValueStep>,
    },
    PluginGroupCrossesParameters {
        expected_parameter: String,
        actual_parameter: String,
        at: Vec<RulesValueStep>,
    },
    NestedJsonDecode {
        phase: &'static str,
        at: Vec<RulesValueStep>,
        source: Arc<StackSafeJsonError>,
    },
    ExpectedObject {
        at: Vec<RulesValueStep>,
        key: String,
        actual: JsonValueKind,
    },
    ExpectedArray {
        at: Vec<RulesValueStep>,
        index: Option<usize>,
        actual: JsonValueKind,
    },
    CommandParametersType {
        file: String,
        code: i64,
        parameter: usize,
        at: Vec<RulesValueStep>,
        actual: JsonValueKind,
    },
    CommandParameterMissing {
        file: String,
        code: i64,
        parameter: usize,
        available: usize,
        at: Vec<RulesValueStep>,
    },
    DecodeJsonTargetType {
        at: Vec<RulesValueStep>,
        actual: JsonValueKind,
    },
    FinalTargetType {
        at: Vec<RulesValueStep>,
        actual: JsonValueKind,
    },
}

impl RulesInvalidTargetReason {
    fn safe_detail(&self) -> String {
        match self {
            Self::InvalidDataFileName { file, .. } => format!(
                "target=data_file; file={}; error=unsafe_data_file_name",
                json_string(file)
            ),
            Self::PluginFieldType {
                plugin_index,
                plugin_name,
                field,
                expected,
                actual,
            } => format!(
                "source=plugin; plugin_index={plugin_index}; plugin={}; target={field}; expected={expected}; actual={}",
                json_string(plugin_name),
                actual.as_str()
            ),
            Self::PluginPathMissingParameter { at } => format!(
                "source=plugin; target=path; actual_path={}; error=first_step_must_select_parameter",
                render_value_steps(at)
            ),
            Self::PluginGroupMissingParameter { at } => format!(
                "source=plugin; target=group_path; actual_path={}; error=first_step_must_select_parameter",
                render_value_steps(at)
            ),
            Self::PluginGroupCrossesParameters {
                expected_parameter,
                actual_parameter,
                at,
            } => format!(
                "source=plugin; target=group_path; actual_path={}; expected_parameter={}; actual_parameter={}; error=crosses_plugin_parameters",
                render_value_steps(at),
                json_string(expected_parameter),
                json_string(actual_parameter)
            ),
            Self::NestedJsonDecode { phase, at, source } => format!(
                "target={phase}; actual_path={}; error=invalid_nested_json; json_category={}; json_line={}; json_column={}",
                render_value_steps(at),
                json_error_classification(source),
                source.line(),
                source.column()
            ),
            Self::ExpectedObject { at, key, actual } => format!(
                "target=path; actual_path={}; step=key; key={}; expected=object; actual={}",
                render_value_steps(at),
                json_string(key),
                actual.as_str()
            ),
            Self::ExpectedArray { at, index, actual } => {
                let selector = index.map_or_else(|| "[]".to_owned(), |value| format!("[{value}]"));
                format!(
                    "target=path; actual_path={}; step={selector}; expected=array; actual={}",
                    render_value_steps(at),
                    actual.as_str()
                )
            }
            Self::CommandParametersType {
                file,
                code,
                parameter,
                at,
                actual,
            } => format!(
                "source=command; file={}; code={code}; parameter={parameter}; target=parameters; actual_path={}; expected=array; actual={}",
                json_string(file),
                render_value_steps(at),
                actual.as_str()
            ),
            Self::CommandParameterMissing {
                file,
                code,
                parameter,
                available,
                at,
            } => format!(
                "source=command; file={}; code={code}; target=parameters[{parameter}]; actual_path={}; available_parameters={available}; error=missing_parameter",
                json_string(file),
                render_value_steps(at)
            ),
            Self::DecodeJsonTargetType { at, actual } => format!(
                "target=decode_json; actual_path={}; expected=string; actual={}",
                render_value_steps(at),
                actual.as_str()
            ),
            Self::FinalTargetType { at, actual } => format!(
                "target=text; actual_path={}; expected=string; actual={}",
                render_value_steps(at),
                actual.as_str()
            ),
        }
    }

    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDataFileName { source, .. } => Some(source),
            Self::NestedJsonDecode { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RulesMaterializationReason {
    Projection {
        at: Vec<RulesValueStep>,
        source: ProjectionModelError,
    },
    #[cfg(test)]
    UnitCount {
        at: Vec<RulesValueStep>,
        expected: usize,
        actual: usize,
    },
    RoundTripMismatch {
        at: Vec<RulesValueStep>,
    },
}

impl RulesMaterializationReason {
    fn safe_detail(&self) -> String {
        match self {
            Self::Projection { at, source } => format!(
                "target=projection_recipe; actual_path={}; projection_error={}",
                render_value_steps(at),
                projection_error_code(source),
            ),
            #[cfg(test)]
            Self::UnitCount {
                at,
                expected,
                actual,
            } => format!(
                "target=projection_recipe; actual_path={}; expected_units={expected}; actual_units={actual}",
                render_value_steps(at)
            ),
            Self::RoundTripMismatch { at } => format!(
                "target=projection_recipe; actual_path={}; error=round_trip_mismatch",
                render_value_steps(at)
            ),
        }
    }
}

fn projection_error_code(source: &ProjectionModelError) -> &'static str {
    match source {
        ProjectionModelError::EmptyScalarFieldKey => "empty_scalar_field_key",
        ProjectionModelError::EventBlockCoverageRequired => "event_block_coverage_required",
        ProjectionModelError::InvalidEventBlockCoverage => "invalid_event_block_coverage",
        ProjectionModelError::MutationClaimTargetMismatch => "mutation_claim_target_mismatch",
        ProjectionModelError::RecipeHasNoTextSlot => "recipe_has_no_text_slot",
        ProjectionModelError::DuplicateProjectionSlot { .. } => "duplicate_projection_slot",
        ProjectionModelError::MultipleBodyLinesInPhysicalLine => {
            "multiple_body_lines_in_physical_line"
        }
        ProjectionModelError::DuplicateDialogueBodyLine { .. } => "duplicate_dialogue_body_line",
        ProjectionModelError::NonContiguousDialogueBodyLines { .. } => {
            "non_contiguous_dialogue_body_lines"
        }
        ProjectionModelError::MixedDirectAndInlineSpeaker => "mixed_direct_and_inline_speaker",
    }
}

fn json_error_classification(source: &StackSafeJsonError) -> &'static str {
    source.diagnostic_category().storage_name()
}

impl RulesMatchError {
    pub(super) fn invalid_plugin_field(
        rule_number: usize,
        plugin_index: usize,
        plugin_name: String,
        field: &'static str,
        expected: &'static str,
        actual: Option<&Value>,
    ) -> Self {
        Self::InvalidTarget {
            rule_number,
            reason: RulesInvalidTargetReason::PluginFieldType {
                plugin_index,
                plugin_name,
                field,
                expected,
                actual: JsonValueKind::of(actual),
            },
        }
    }

    fn with_context(self, context: RulesMatchContext) -> Self {
        if matches!(self, Self::Context { .. }) {
            self
        } else {
            Self::Context {
                context,
                source: Box::new(self),
            }
        }
    }

    fn core_and_context(&self) -> (&Self, Option<&RulesMatchContext>) {
        match self {
            Self::Context { context, source } => (source, Some(context)),
            _ => (self, None),
        }
    }

    fn safe_projection(&self) -> (DiagnosticFailureKind, String) {
        match self {
            Self::Context { source, .. } => source.safe_projection(),
            Self::NoNonBlankMatch { rule_number } => (
                DiagnosticFailureKind::RulesNoNonBlankMatch,
                format!("rule={rule_number}; error=no_non_blank_match"),
            ),
            Self::InvalidTarget {
                rule_number,
                reason,
            } => (
                DiagnosticFailureKind::RulesInvalidTarget,
                format!("rule={rule_number}; {}", reason.safe_detail()),
            ),
            Self::PatternMatch {
                rule_number,
                at,
                source,
            } => (
                DiagnosticFailureKind::RulesPatternMatchFailed,
                format!(
                    "rule={rule_number}; actual_path={}; {}",
                    render_value_steps(at),
                    pcre2_error_detail(source)
                ),
            ),
            Self::ZeroWidthMatch {
                rule_number,
                at,
                start,
                end,
            } => (
                DiagnosticFailureKind::RulesZeroWidthMatch,
                format!(
                    "rule={rule_number}; actual_path={}; match_start={start}; match_end={end}; error=zero_width_match",
                    render_value_steps(at)
                ),
            ),
            Self::OverlappingMatch {
                rule_number,
                at,
                previous_end,
                start,
                end,
            } => (
                DiagnosticFailureKind::RulesOverlappingCapture,
                format!(
                    "rule={rule_number}; actual_path={}; capture=text; previous_end={previous_end}; capture_start={start}; capture_end={end}; error=overlap",
                    render_value_steps(at)
                ),
            ),
            Self::MissingTextCapture { rule_number, at } => (
                DiagnosticFailureKind::RulesMissingTextCapture,
                format!(
                    "rule={rule_number}; actual_path={}; capture=text; error=not_participating",
                    render_value_steps(at)
                ),
            ),
            Self::InvalidCaptureRange {
                rule_number,
                at,
                text_bytes,
                whole_start,
                whole_end,
                capture_start,
                capture_end,
            } => (
                DiagnosticFailureKind::RulesInvalidCaptureRange,
                format!(
                    "rule={rule_number}; actual_path={}; text_bytes={text_bytes}; match_start={whole_start}; match_end={whole_end}; capture_start={capture_start}; capture_end={capture_end}; error=invalid_utf8_or_match_range",
                    render_value_steps(at)
                ),
            ),
            Self::DuplicateTarget {
                first_rule,
                second_rule,
                source,
                steps,
            } => (
                DiagnosticFailureKind::RulesDuplicateTarget,
                format!(
                    "first_rule={first_rule}; second_rule={second_rule}; {}; target_path={}",
                    render_match_source(source),
                    render_value_steps(steps)
                ),
            ),
            Self::InvalidMaterialization {
                rule_number,
                reason,
            } => (
                DiagnosticFailureKind::RulesInvalidMaterialization,
                format!("rule={rule_number}; {}", reason.safe_detail()),
            ),
        }
    }

    /// 公开 Rules 路径、规则、来源、目标结构与稳定底层代码；正文和值留在 source。
    pub(super) fn safe_diagnostic(&self, rules_path: &Path) -> SafeDiagnostic {
        let (failure, mut detail) = self.safe_projection();
        let (_, context) = self.core_and_context();
        if let Some(context) = context {
            detail.push_str("; ");
            detail.push_str(&context.safe_detail());
        }
        SafeDiagnostic::new(
            DiagnosticCode::ExtractRules,
            DiagnosticStage::Extract,
            DiagnosticSubject::path(rules_path),
            DiagnosticReason::failure_with_detail(failure, detail),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        )
    }
}

fn render_value_steps(steps: &[RulesValueStep]) -> String {
    let mut result = "$".to_owned();
    for step in steps {
        match step {
            RulesValueStep::Key(key) => result.push_str(&format!("[{}]", json_string(key))),
            RulesValueStep::Index(index) => result.push_str(&format!("[{index}]")),
            RulesValueStep::DecodeJsonString => result.push_str("<decode_json>"),
        }
    }
    json_string(&result)
}

fn render_match_source(source: &RulesMatchSource) -> String {
    match source {
        RulesMatchSource::DataFile { file } => {
            format!("source=data_file; file={}", json_string(file))
        }
        RulesMatchSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => format!(
            "source=plugin_parameter; plugin_index={plugin_index}; plugin={}; parameter={}",
            json_string(plugin_name),
            json_string(parameter_name)
        ),
    }
}

impl fmt::Display for RulesMatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context { context, source } => {
                write!(formatter, "{source}; {}", context.safe_detail())
            }
            Self::NoNonBlankMatch { rule_number } => {
                write!(
                    formatter,
                    "Rules 规则 {rule_number} 没有产生任何非空语义单元"
                )
            }
            Self::InvalidTarget {
                rule_number,
                reason,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 命中了无效目标：{}",
                reason.safe_detail()
            ),
            Self::PatternMatch {
                rule_number,
                source,
                ..
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 执行 PCRE2 失败：{}",
                pcre2_error_detail(source)
            ),
            Self::ZeroWidthMatch { rule_number, .. } => {
                write!(formatter, "Rules 规则 {rule_number} 产生了零宽匹配")
            }
            Self::OverlappingMatch { rule_number, .. } => {
                write!(formatter, "Rules 规则 {rule_number} 产生了重叠的 text 捕获")
            }
            Self::MissingTextCapture { rule_number, .. } => write!(
                formatter,
                "Rules 规则 {rule_number} 的 text 捕获在一次匹配中没有参与"
            ),
            Self::InvalidCaptureRange { rule_number, .. } => write!(
                formatter,
                "Rules 规则 {rule_number} 的完整匹配与 text 捕获必须位于原文 UTF-8 字符边界内，且 text 捕获必须包含在完整匹配中"
            ),
            Self::DuplicateTarget {
                first_rule,
                second_rule,
                source,
                steps,
            } => write!(
                formatter,
                "Rules 规则 {first_rule} 与 {second_rule} 重复拥有同一物理目标：{source}{steps:?}"
            ),
            Self::InvalidMaterialization {
                rule_number,
                reason,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 的物化配方无效：{}",
                reason.safe_detail()
            ),
        }
    }
}

impl Error for RulesMatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context { source, .. } => Some(source),
            Self::InvalidTarget { reason, .. } => reason.source(),
            Self::PatternMatch { source, .. } => Some(source),
            Self::InvalidMaterialization {
                reason: RulesMaterializationReason::Projection { source, .. },
                ..
            } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn file_path_expands_arrays_and_materializes_multiple_regex_matches() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Classes.json"
path = '[].note'
pattern = '(?ms)<DESC:(?<text>.*?)>'
"#,
        )
        .expect("规则应合法");
        let input = input([(
            "Classes.json",
            json!([null, {"note":"<DESC:第一段><DESC:第二段>"}]),
        )]);

        let targets = match_rules(&definition, &input).expect("两段文本都应物化");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].units.len(), 2);
        assert_eq!(targets[0].units[0].source_text, "第一段");
        assert_eq!(targets[0].units[1].source_text, "第二段");
        let originals = targets[0]
            .units
            .iter()
            .map(|unit| unit.source_text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            targets[0].materialize(&originals).expect("配方应可重建"),
            targets[0].expected_text
        );
    }

    #[test]
    fn path_automatically_decodes_nested_json_and_decode_json_decodes_final_scalar() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
plugin = "Input"
path = 'Settings.Message'
decode_json = true
"#,
        )
        .expect("规则应合法");
        let plugins = vec![RulesPlugin::new(
            3,
            "Input",
            true,
            Map::from_iter([(
                "Settings".to_owned(),
                Value::String(r#"{"Message":"\"手柄未连接\""}"#.to_owned()),
            )]),
        )];
        let input = RulesMatchInput::new(BTreeMap::new(), plugins);

        let targets = match_rules(&definition, &input).expect("嵌套值应匹配");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].units[0].source_text, "手柄未连接");
        assert_eq!(
            targets[0].steps,
            vec![
                RulesValueStep::DecodeJsonString,
                RulesValueStep::Key("Message".to_owned()),
                RulesValueStep::DecodeJsonString,
            ]
        );
    }

    #[test]
    fn file_and_command_paths_also_decode_nested_json_strings() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = '[].Payload.Text'

[[rule]]
code = 357
parameter = 3
path = 'dText'
"#,
        )
        .expect("文件与命令规则应合法");
        let input = input([
            (
                "Custom.json",
                json!([{"Payload": r#"{"Text":"文件正文"}"#}]),
            ),
            (
                "CommonEvents.json",
                json!([null, {"list":[{
                    "code":357,
                    "parameters":["P", "C", "M", r#"{"dText":"命令正文"}"#]
                }]}]),
            ),
        ]);

        let targets = match_rules(&definition, &input).expect("两种来源都应逐层解码");

        assert_eq!(targets.len(), 2);
        assert!(
            targets
                .iter()
                .all(|target| target.steps.contains(&RulesValueStep::DecodeJsonString))
        );
        assert_eq!(
            targets
                .iter()
                .map(|target| target.units[0].source_text.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["命令正文", "文件正文"])
        );
    }

    #[test]
    fn nested_json_diagnostic_keeps_source_path_and_parser_location_without_value() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'Payload.Message'
"#,
        )
        .expect("规则应合法");
        let input = input([(
            "Custom.json",
            json!({"Payload":"{\"Message\":SOURCE_JSON_VALUE_SENTINEL}"}),
        )]);

        let error = match_rules(&definition, &input).expect_err("嵌套 JSON 必须失败");
        let diagnostic = error.safe_diagnostic(Path::new("rules/main.toml"));
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert!(serialized.contains("rules/main.toml"));
        assert!(serialized.contains("rule=1"));
        assert!(serialized.contains("source=data_file"));
        assert!(serialized.contains("Custom.json"));
        assert!(serialized.contains("target=path"));
        assert!(serialized.contains("actual_path="));
        assert!(serialized.contains("json_category=syntax"));
        assert!(serialized.contains("json_line=1"));
        assert!(serialized.contains("json_column="));
        assert!(!serialized.contains("SOURCE_JSON_VALUE_SENTINEL"));
    }

    #[test]
    fn arbitrary_command_code_and_optional_structured_path_are_supported() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
code = 356
parameter = 0
pattern = '\AGabText\s+(?<text>.+)\z'

[[rule]]
code = 357
parameter = 3
path = 'dText'
"#,
        )
        .expect("命令规则应合法");
        let common_events = json!([
            null,
            {"list":[
                {"code":356,"parameters":["GabText 你好"]},
                {"code":357,"parameters":["P","C","M",{"dText":"说明"}]}
            ]}
        ]);
        let input = input([("CommonEvents.json", common_events)]);

        let targets = match_rules(&definition, &input).expect("任意 code 应匹配");

        let originals = targets
            .iter()
            .flat_map(|target| target.units.iter())
            .map(|unit| unit.source_text.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(originals, BTreeSet::from(["你好", "说明"]));
    }

    #[test]
    fn command_source_skips_non_objects_and_non_integer_codes() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
code = 401
parameter = 0
"#,
        )
        .expect("命令规则应合法");
        let input = input([(
            "CommonEvents.json",
            json!([
                null,
                {
                    "list": [
                        "不是 command 对象",
                        {"code": "401", "parameters": ["不得误匹配字符串 code"]},
                        {"code": 401.5, "parameters": ["不得误匹配小数 code"]},
                        {"code": 401, "parameters": ["有效正文"]}
                    ]
                }
            ]),
        )]);

        let targets = match_rules(&definition, &input)
            .expect("非对象或非整数 code 的 command 来源应跳过而不是导致匹配失败");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].units().len(), 1);
        assert_eq!(targets[0].units()[0].source_text(), "有效正文");
    }

    #[test]
    fn exact_nonstandard_file_and_all_maps_are_real_sources() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Disciplines.json"
path = '[].Name'

[[rule]]
file = "Map*.json"
path = 'displayName'
"#,
        )
        .expect("任意安全文件应合法");
        let input = input([
            ("Disciplines.json", json!([{"Name":"Baking"}])),
            ("Map001.json", json!({"displayName":"第一张地图"})),
            ("Map1.json", json!({"displayName":"非规范名字"})),
        ]);

        let targets = match_rules(&definition, &input).expect("两个来源都应匹配");

        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn all_maps_keep_numeric_physical_source_order_past_three_digits() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Map*.json"
path = 'displayName'
"#,
        )
        .expect("AllMaps 规则应合法");
        let input = input([
            ("Map999.json", json!({"displayName":"第 999 张"})),
            ("Map1000.json", json!({"displayName":"第 1000 张"})),
        ]);

        let targets = match_rules(&definition, &input).expect("两张地图都应命中");

        assert_eq!(
            targets
                .iter()
                .map(|target| target.units[0].source_text.as_str())
                .collect::<Vec<_>>(),
            ["第 999 张", "第 1000 张"],
            "Map1000 不得因文件名词法顺序排到 Map999 前"
        );
    }

    #[test]
    fn merged_rules_follow_object_insertion_order_instead_of_rule_or_key_order() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'a'

[[rule]]
file = "Custom.json"
path = 'z'
"#,
        )
        .expect("两条精确字段规则应合法");
        let input = input([("Custom.json", json!({"z":"物理第一", "a":"物理第二"}))]);

        let targets = match_rules(&definition, &input).expect("两条规则都应命中");

        assert_eq!(
            targets
                .iter()
                .map(|target| target.units[0].source_text.as_str())
                .collect::<Vec<_>>(),
            ["物理第一", "物理第二"],
            "规则编号与键名词法序都不得覆盖来源结构顺序"
        );
    }

    #[test]
    fn rule_without_pattern_keeps_angle_bracket_text_as_one_complete_unit() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].note'
"#,
        )
        .expect("完整 Value 规则应合法");
        let input = input([("Items.json", json!([null, {"note":"<Help:炎之剑的说明>"}]))]);

        let targets = match_rules(&definition, &input).expect("完整 Value 应形成一个目标");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].units.len(), 1);
        assert_eq!(targets[0].units[0].source_text, "<Help:炎之剑的说明>");
        assert_eq!(
            targets[0].parts,
            [MatchedRulePart::TextSlot { unit_index: 0 }]
        );
        assert_eq!(
            targets[0]
                .materialize(&["<Help:烈焰之剑的说明>>".to_owned()])
                .expect("完整 Value 配方应可写回"),
            "<Help:烈焰之剑的说明>>"
        );
    }

    #[test]
    fn explicit_help_capture_makes_only_the_body_a_unit_and_keeps_the_shell_literal() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].note'
pattern = '\A<Help:(?<text>.*?)>\z'
"#,
        )
        .expect("显式正文捕获规则应合法");
        let input = input([("Items.json", json!([null, {"note":"<Help:炎之剑的说明>"}]))]);

        let targets = match_rules(&definition, &input).expect("显式捕获应形成一个目标");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].units.len(), 1);
        assert_eq!(targets[0].units[0].source_text, "炎之剑的说明");
        assert_eq!(
            targets[0].parts,
            [
                MatchedRulePart::Literal("<Help:".to_owned()),
                MatchedRulePart::TextSlot { unit_index: 0 },
                MatchedRulePart::Literal(">".to_owned()),
            ]
        );
        assert_eq!(
            targets[0]
                .materialize(&["烈焰之剑的说明>追加".to_owned()])
                .expect("外壳与正文配方应可写回"),
            "<Help:烈焰之剑的说明>追加>"
        );
    }

    #[test]
    fn one_string_keeps_twelve_capture_units_in_byte_position_order() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'text'
pattern = '<x>(?<text>.*?)</x>'
"#,
        )
        .expect("多次捕获规则应合法");
        let expected = (0..12)
            .map(|index| format!("值{index}"))
            .collect::<Vec<_>>();
        let source = expected
            .iter()
            .map(|value| format!("<x>{value}</x>"))
            .collect::<String>();
        let input = input([("Custom.json", json!({"text":source}))]);

        let targets = match_rules(&definition, &input).expect("十二次捕获都应命中");

        assert_eq!(
            targets[0]
                .units
                .iter()
                .map(|unit| unit.source_text.clone())
                .collect::<Vec<_>>(),
            expected,
            "第 10 次捕获不得因字符串化编号排到第 2 次前"
        );
    }

    #[test]
    fn every_nonempty_rule_must_produce_a_nonblank_unit() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
"#,
        )
        .expect("规则应合法");
        let input = input([("Items.json", json!([null, {"name":"   "}]))]);

        assert!(matches!(
            match_rules(&definition, &input),
            Err(RulesMatchError::NoNonBlankMatch { rule_number: 1 })
        ));
    }

    #[test]
    fn duplicate_physical_target_across_rules_fails_without_priority() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'

[[rule]]
file = "Items.json"
path = '[].name'
pattern = '\A(?<text>.+)\z'
"#,
        )
        .expect("各规则单独都合法");
        let input = input([("Items.json", json!([null, {"name":"药草"}]))]);

        assert!(matches!(
            match_rules(&definition, &input),
            Err(RulesMatchError::DuplicateTarget {
                first_rule: 1,
                second_rule: 2,
                ..
            })
        ));
    }

    #[test]
    fn blank_capture_is_frozen_as_literal_while_nonblank_capture_remains_translatable() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].note'
pattern = '<x>(?<text>.*?)</x>'
"#,
        )
        .expect("规则应合法");
        let input = input([("Items.json", json!([null, {"note":"<x> </x><x>甲</x>"}]))]);

        let targets = match_rules(&definition, &input).expect("非空捕获应产生语义单元");

        assert_eq!(targets[0].units.len(), 1);
        assert_eq!(targets[0].units[0].source_text, "甲");
        assert_eq!(
            targets[0]
                .materialize(&["乙".to_owned()])
                .expect("配方应可写回"),
            "<x> </x><x>乙</x>"
        );
    }

    #[test]
    fn zero_width_whole_or_capture_and_nonparticipating_capture_are_rejected() {
        let zero = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
pattern = '(?<text>(?=药))'
"#,
        )
        .expect("捕获 ABI 合法但运行时是零宽");
        let optional = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
pattern = '(?:药|(?<text>草))'
"#,
        )
        .expect("捕获 ABI 合法但可能不参与");
        let zero_capture = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
pattern = '(?<text>(?=药)).'
"#,
        )
        .expect("完整匹配非零宽但 text 捕获为零宽");
        let input = input([("Items.json", json!([null, {"name":"药草"}]))]);

        assert_contextual_data_file_error(match_rules(&zero, &input), "Items.json", |source| {
            matches!(source, RulesMatchError::ZeroWidthMatch { .. })
        });
        assert_contextual_data_file_error(match_rules(&optional, &input), "Items.json", |source| {
            matches!(source, RulesMatchError::MissingTextCapture { .. })
        });
        assert_contextual_data_file_error(
            match_rules(&zero_capture, &input),
            "Items.json",
            |source| matches!(source, RulesMatchError::ZeroWidthMatch { .. }),
        );
    }

    #[test]
    fn text_capture_must_be_contained_in_the_complete_match() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
pattern = '(?=(?<text>...))..'
"#,
        )
        .expect("PCRE2 可以让捕获超出完整匹配");
        let input = input([("Items.json", json!([null, {"name":"药草水晶球"}]))]);

        assert_contextual_data_file_error(
            match_rules(&definition, &input),
            "Items.json",
            |source| matches!(source, RulesMatchError::InvalidCaptureRange { .. }),
        );
    }

    #[test]
    fn text_capture_must_align_with_utf8_boundaries() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'
pattern = '(?<text>\C)'
"#,
        )
        .expect("PCRE2 允许按单个字节捕获");
        let input = input([("Items.json", json!([null, {"name":"药"}]))]);

        assert_contextual_data_file_error(
            match_rules(&definition, &input),
            "Items.json",
            |source| matches!(source, RulesMatchError::InvalidCaptureRange { .. }),
        );
    }

    #[test]
    fn source_plan_scans_each_event_document_once_for_multiple_command_rules() {
        let source = r#"
            [[rule]]
            code = 401
            parameter = 0

            [[rule]]
            code = 402
            parameter = 0
        "#;
        let input = input([(
            "CommonEvents.json",
            json!([
                null,
                {
                    "list": [
                        {"code": 401, "parameters": ["第一段"]},
                        {"code": 402, "parameters": ["第二段"]}
                    ]
                }
            ]),
        )]);
        let definition = RulesDefinition::parse(source).expect("规则应合法");
        let plan = build_source_match_plan(definition.into_rules(), input);
        let (rule_count, work_units) = plan.into_parts();
        assert_eq!(work_units.len(), 1, "同一事件文件应形成一个来源工作单元");
        let results = work_units
            .into_iter()
            .map(RulesSourceMatchWorkUnit::run)
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .map(|result| result.event_document_scans)
                .sum::<usize>(),
            1,
            "多条 command 规则不得重复枚举同一事件来源"
        );
        let actual = finish_source_matches(rule_count, results).expect("来源驱动匹配应成功");

        assert_eq!(actual.len(), 2);
        assert_eq!(actual[0].units()[0].source_text(), "第一段");
        assert_eq!(actual[1].units()[0].source_text(), "第二段");
    }

    #[test]
    fn shared_file_and_plugin_paths_match_the_rule_driven_reference_matrix() {
        let file_success = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'records[].name'

[[rule]]
file = "Custom.json"
path = 'records[].payload.text'

[[rule]]
file = "Custom.json"
path = 'records[1].note'
pattern = '<x>(?<text>.*?)</x>'

[[rule]]
file = "Custom.json"
path = 'meta.title'
decode_json = true
"#,
        )
        .expect("文件矩阵规则应合法");
        let file_input = input([(
            "Custom.json",
            json!({
                "meta": {"title": "\"总标题\""},
                "records": [
                    {"name":"第一项", "payload": r#"{"text":"第一段"}"#, "note":"忽略"},
                    {"name":"第二项", "payload": r#"{"text":"第二段"}"#, "note":"<x>注释</x>"}
                ]
            }),
        )]);
        assert_shared_matches_rule_driven_reference(&file_success, &file_input);

        let plugin_success = RulesDefinition::parse(
            r#"
[[rule]]
plugin = "Shared"
path = 'Config.Label'

[[rule]]
plugin = "Shared"
path = 'Config.Items[].name'

[[rule]]
plugin = "Shared"
path = 'Encoded'
decode_json = true
"#,
        )
        .expect("插件矩阵规则应合法");
        let plugin_input = RulesMatchInput::new(
            BTreeMap::new(),
            vec![RulesPlugin::new(
                7,
                "Shared",
                true,
                Map::from_iter([
                    (
                        "Config".to_owned(),
                        Value::String(
                            r#"{"Label":"设置", "Items":[{"name":"甲"},{"name":"乙"}]}"#.to_owned(),
                        ),
                    ),
                    (
                        "Encoded".to_owned(),
                        Value::String("\"插件正文\"".to_owned()),
                    ),
                ]),
            )],
        );
        assert_shared_matches_rule_driven_reference(&plugin_success, &plugin_input);

        let first_error = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'missing.text'

[[rule]]
file = "Custom.json"
path = 'scalar.text'
"#,
        )
        .expect("首错矩阵规则应合法");
        let first_error_input = input([("Custom.json", json!({"scalar": 1}))]);
        assert_shared_matches_rule_driven_reference(&first_error, &first_error_input);

        let duplicate = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'text'

[[rule]]
file = "Custom.json"
path = 'text'
pattern = '\A(?<text>.+)\z'
"#,
        )
        .expect("重复目标矩阵规则应合法");
        let duplicate_input = input([("Custom.json", json!({"text":"重复正文"}))]);
        assert_shared_matches_rule_driven_reference(&duplicate, &duplicate_input);

        let nested_error = RulesDefinition::parse(
            r#"
[[rule]]
plugin = "Broken"
path = 'Config.Label'

[[rule]]
plugin = "Broken"
path = 'Config.Description'
"#,
        )
        .expect("嵌套错误矩阵规则应合法");
        let nested_error_input = RulesMatchInput::new(
            BTreeMap::new(),
            vec![RulesPlugin::new(
                1,
                "Broken",
                true,
                Map::from_iter([("Config".to_owned(), Value::String("{broken".to_owned()))]),
            )],
        );
        assert_shared_matches_rule_driven_reference(&nested_error, &nested_error_input);
    }

    #[test]
    fn shared_path_node_visits_do_not_scale_with_identical_file_or_plugin_rules() {
        let file_input = input([(
            "Custom.json",
            json!({
                "rows": (0..64)
                    .map(|index| json!({"payload": format!(r#"{{"text":"值{index}"}}"#)}))
                    .collect::<Vec<_>>()
            }),
        )]);
        let one_file_rule = r#"
[[rule]]
file = "Custom.json"
path = 'rows[].payload.text'
"#;
        let many_file_rules = one_file_rule.repeat(128);
        assert_eq!(
            shared_path_node_visits(one_file_rule, file_input.clone()),
            shared_path_node_visits(&many_file_rules, file_input),
            "同一路径规则增多不得重新扫描文件 JSON 来源",
        );

        let plugin = RulesPlugin::new(
            2,
            "Shared",
            true,
            Map::from_iter([(
                "Config".to_owned(),
                Value::String(
                    serde_json::to_string(&json!({
                        "rows": (0..64)
                            .map(|index| json!({"text": format!("插件值{index}")}))
                            .collect::<Vec<_>>()
                    }))
                    .expect("测试 JSON 应可编码"),
                ),
            )]),
        );
        let plugin_input = RulesMatchInput::new(BTreeMap::new(), vec![plugin]);
        let one_plugin_rule = r#"
[[rule]]
plugin = "Shared"
path = 'Config.rows[].text'
"#;
        let many_plugin_rules = one_plugin_rule.repeat(128);
        assert_eq!(
            shared_path_node_visits(one_plugin_rule, plugin_input.clone()),
            shared_path_node_visits(&many_plugin_rules, plugin_input),
            "同一路径规则增多不得重新扫描插件参数来源",
        );
    }

    #[test]
    fn production_matchers_support_rule_paths_deeper_than_the_native_call_stack() {
        const DEPTH: usize = 32_768;

        let deep_path = (0..DEPTH)
            .map(|index| format!("k{index}"))
            .collect::<Vec<_>>()
            .join(".");
        let definition_source = format!(
            r#"
[[rule]]
file = "CommonEvents.json"
path = '[1].list[0].parameters[0].{deep_path}.file_text'

[[rule]]
code = 999
parameter = 0
path = '{deep_path}.command_text'
"#,
        );
        let definition = RulesDefinition::parse(&definition_source).expect("深路径规则应合法");

        let terminal = Value::Object(Map::from_iter([
            (
                "file_text".to_owned(),
                Value::String("文件深路径正文".to_owned()),
            ),
            (
                "command_text".to_owned(),
                Value::String("命令深路径正文".to_owned()),
            ),
        ]));
        let selected = deeply_nested_rule_value(DEPTH, terminal);
        let command = Value::Object(Map::from_iter([
            ("code".to_owned(), Value::from(999)),
            ("parameters".to_owned(), Value::Array(vec![selected])),
        ]));
        let common_event = Value::Object(Map::from_iter([(
            "list".to_owned(),
            Value::Array(vec![command]),
        )]));
        let input = input([(
            "CommonEvents.json",
            Value::Array(vec![Value::Null, common_event]),
        )]);

        let targets =
            match_rules(&definition, &input).expect("两套生产匹配路径都不应依赖调用栈深度");

        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.units[0].source_text.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["命令深路径正文", "文件深路径正文"]),
        );
        assert!(
            targets.iter().all(|target| target.steps.len() >= DEPTH),
            "共享 file matcher 与 command PathWalker 都必须实际走完整条深路径",
        );
    }

    fn assert_shared_matches_rule_driven_reference(
        definition: &RulesDefinition,
        input: &RulesMatchInput,
    ) {
        let shared = match_rules(definition, input);
        let reference = match_rules_rule_driven_reference(definition, input);
        match (shared, reference) {
            (Ok(shared), Ok(reference)) => assert_eq!(shared, reference),
            (Err(shared), Err(reference)) => assert_eq!(
                shared.safe_diagnostic(Path::new("Rules.toml")),
                reference.safe_diagnostic(Path::new("Rules.toml")),
                "共享路径匹配与逐规则参考实现必须保留相同的安全结构化诊断",
            ),
            (shared, reference) => panic!(
                "共享路径匹配与逐规则参考实现结果分歧：shared={shared:?}, reference={reference:?}"
            ),
        }
    }

    fn assert_contextual_data_file_error(
        result: Result<Vec<MatchedRuleTarget>, RulesMatchError>,
        expected_file: &str,
        matches_core: impl FnOnce(&RulesMatchError) -> bool,
    ) {
        match result {
            Err(RulesMatchError::Context {
                context:
                    RulesMatchContext {
                        source: RulesDiagnosticSource::DataFile { file },
                        has_declared_path: true,
                    },
                source,
            }) if file == expected_file && matches_core(&source) => {}
            result => panic!(
                "应返回带文件和 path 阶段的具体 Rules 错误：file={expected_file:?}; result={result:?}"
            ),
        }
    }

    fn match_rules_rule_driven_reference(
        definition: &RulesDefinition,
        input: &RulesMatchInput,
    ) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
        let mut matches = Vec::with_capacity(definition.rules().len());
        for rule in definition.rules() {
            let mut targets = Vec::new();
            match rule.source() {
                RuleSource::File(file_source) => {
                    for (source_order, (file, root)) in input.files.iter().enumerate() {
                        let applies = match file_source {
                            FileRuleSource::Exact(expected) => file == expected,
                            FileRuleSource::AllMaps => is_canonical_map_file(file),
                        };
                        if applies {
                            let context = RulesMatchContext::for_rule(
                                rule,
                                RulesDiagnosticSource::DataFile { file: file.clone() },
                            );
                            targets.extend(
                                match_file_rule_on_source_reference(rule, file, root, source_order)
                                    .map_err(|source| source.with_context(context))?,
                            );
                        }
                    }
                }
                RuleSource::Plugin(plugin_name) => {
                    let mut source_order = input.files.len();
                    for plugin in &input.plugins {
                        if &plugin.name == plugin_name {
                            let context = RulesMatchContext::for_rule(
                                rule,
                                RulesDiagnosticSource::Plugin {
                                    plugin_index: plugin.index,
                                    plugin_name: plugin.name.clone(),
                                },
                            );
                            targets.extend(
                                match_plugin_rule_on_source_reference(rule, plugin, source_order)
                                    .map_err(|source| source.with_context(context))?,
                            );
                        }
                        source_order = source_order.saturating_add(
                            plugin
                                .parameters
                                .as_object()
                                .expect("Rules 插件参数根始终是对象")
                                .len(),
                        );
                    }
                }
                RuleSource::Command { .. } => {
                    unreachable!("file/plugin 等价矩阵不包含 command 规则")
                }
            }
            if targets.is_empty() {
                return Err(RulesMatchError::NoNonBlankMatch {
                    rule_number: rule.rule_number(),
                });
            }
            matches.push(targets);
        }
        merge_ordered_rule_matches(matches)
    }

    fn shared_path_node_visits(source: &str, input: RulesMatchInput) -> usize {
        let definition = RulesDefinition::parse(source).expect("计数规则应合法");
        let plan = build_source_match_plan(definition.into_rules(), input);
        let (_, work_units) = plan.into_parts();
        work_units
            .into_iter()
            .map(RulesSourceMatchWorkUnit::run)
            .map(|result| result.shared_path_node_visits)
            .sum()
    }

    fn input<const N: usize>(files: [(&str, Value); N]) -> RulesMatchInput {
        RulesMatchInput::new(
            files
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect::<Vec<_>>(),
            Vec::new(),
        )
    }

    fn deeply_nested_rule_value(depth: usize, terminal: Value) -> Value {
        (0..depth).rev().fold(terminal, |value, index| {
            Value::Object(Map::from_iter([(format!("k{index}"), value)]))
        })
    }
}
