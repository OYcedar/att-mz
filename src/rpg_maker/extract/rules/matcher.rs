//! 已受信 Rules 定义在冻结 RPG Maker 文档上的纯匹配与配方物化。
//!
//! 本模块只解释规则所声明的局部语义：来源、窄路径、嵌套 JSON 解码和 PCRE2
//! `text` 捕获。它不会在写回阶段保存或重新运行正则；匹配跨度会立即变成
//! `Literal` / `TextSlot` 配方。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, ScalarFieldKey, TextFieldRole, TextProjectionRecipe,
};
use crate::rpg_maker::text::{
    DataFileName, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
    TextGroupKind,
};

#[cfg(test)]
use super::definition::RulesDefinition;
use super::definition::{CompiledPath, FileRuleSource, PathSegment, RuleDefinition, RuleSource};

/// 一组已经由文档读取边界冻结的 Rules 输入。
#[derive(Clone, Debug, Default)]
pub(super) struct RulesMatchInput {
    files: BTreeMap<String, Value>,
    plugins: Vec<RulesPlugin>,
}

impl RulesMatchInput {
    pub(super) fn new(files: BTreeMap<String, Value>, mut plugins: Vec<RulesPlugin>) -> Self {
        plugins.sort_by_key(RulesPlugin::index);
        Self { files, plugins }
    }
}

/// `plugins.js` 中一条已经过外壳校验的插件记录。
#[derive(Clone, Debug)]
pub(super) struct RulesPlugin {
    index: usize,
    name: String,
    enabled: bool,
    parameters: Map<String, Value>,
}

impl RulesPlugin {
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

/// 最终字符串的物化替换配方。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatchedRuleTarget {
    rule_number: usize,
    kind: TextGroupKind,
    source: RulesMatchSource,
    group_steps: Vec<RulesValueStep>,
    steps: Vec<RulesValueStep>,
    expected_text: String,
    leaves: Vec<MatchedRuleLeaf>,
    parts: Vec<MatchedRulePart>,
}

impl MatchedRuleTarget {
    pub(super) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(super) fn leaves(&self) -> &[MatchedRuleLeaf] {
        &self.leaves
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
            RulesMatchSource::DataFile { file } => canonical_map_id(file).map_or_else(
                || {
                    DataFileName::parse(file.clone())
                        .map(RpgMakerSource::data_file)
                        .map_err(|source| RulesMatchError::InvalidTarget {
                            rule_number: self.rule_number,
                            message: source.to_string(),
                        })
                },
                |map_id| Ok(RpgMakerSource::map(map_id)),
            ),
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
                MatchedRulePart::TextSlot { leaf_index } => Ok(DirectTextPart::TextSlot {
                    role: self.role_for(*leaf_index),
                }),
            })
            .collect::<Result<Vec<_>, RulesMatchError>>()?;
        DirectTextRecipe::new(self.physical_location()?, self.expected_text.clone(), parts)
            .map(TextProjectionRecipe::Direct)
            .map_err(|source| RulesMatchError::InvalidMaterialization {
                rule_number: self.rule_number,
                message: source.to_string(),
            })
    }

    pub(super) fn role_for(&self, leaf_index: usize) -> TextFieldRole {
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
        key.push_str(&format!("text[{leaf_index}]"));
        ScalarFieldKey::new(key)
            .map(TextFieldRole::Scalar)
            .expect("生成的 Rules 标量角色键始终非空")
    }

    /// 使用叶子值重建最终字符串，供模型构造边界校验配方完整性。
    #[cfg(test)]
    pub(super) fn materialize(&self, values: &[String]) -> Result<String, RulesMatchError> {
        if values.len() != self.leaves.len() {
            return Err(RulesMatchError::InvalidMaterialization {
                rule_number: self.rule_number,
                message: format!(
                    "配方需要 {} 个文本值，但收到 {} 个",
                    self.leaves.len(),
                    values.len()
                ),
            });
        }
        Ok(materialize_parts(&self.parts, values).expect("配方构造已保证 TextSlot 引用存在的叶子"))
    }
}

/// 同一最终字符串中一个可独立翻译的逻辑叶。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatchedRuleLeaf {
    original_text: String,
}

impl MatchedRuleLeaf {
    pub(super) fn original_text(&self) -> &str {
        &self.original_text
    }
}

/// 最终字符串中冻结外壳与可翻译槽的稳定顺序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MatchedRulePart {
    Literal(String),
    TextSlot { leaf_index: usize },
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
    let matches = definition
        .rules()
        .iter()
        .map(|rule| match_rule(rule, input))
        .collect::<Result<Vec<_>, _>>()?;
    merge_rule_matches(matches)
}

/// 匹配一条已受信规则，供服务按外部并发上限建立独立 CPU 工作单元。
pub(super) fn match_rule(
    rule: &RuleDefinition,
    input: &RulesMatchInput,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let targets = match rule.source() {
        RuleSource::File(file) => match_file_rule(rule, file, input)?,
        RuleSource::Plugin(plugin) => match_plugin_rule(rule, plugin, input)?,
        RuleSource::Command { code, parameter } => {
            match_command_rule(rule, *code, *parameter, input)?
        }
    };
    let leaf_count = targets
        .iter()
        .map(|target| target.leaves.len())
        .sum::<usize>();
    if leaf_count == 0 {
        return Err(RulesMatchError::NoNonBlankMatch {
            rule_number: rule.rule_number(),
        });
    }
    Ok(targets)
}

/// 汇总独立规则结果，并在提交前完成跨规则物理目标冲突检查。
pub(super) fn merge_rule_matches(
    matches: Vec<Vec<MatchedRuleTarget>>,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut targets = BTreeMap::<TargetKey, MatchedRuleTarget>::new();
    for rule_targets in matches {
        for target in rule_targets {
            let key = TargetKey {
                source: target.source.clone(),
                steps: target.steps.clone(),
            };
            let second_rule = target.rule_number;
            if let Some(previous) = targets.insert(key, target) {
                return Err(RulesMatchError::DuplicateTarget {
                    first_rule: previous.rule_number,
                    second_rule,
                    source: previous.source,
                    steps: previous.steps,
                });
            }
        }
    }
    Ok(targets.into_values().collect())
}

fn match_file_rule(
    rule: &RuleDefinition,
    source: &FileRuleSource,
    input: &RulesMatchInput,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let files = match source {
        FileRuleSource::Exact(file) => input
            .files
            .get(file)
            .map(|value| vec![(file.as_str(), value)])
            .unwrap_or_default(),
        FileRuleSource::AllMaps => input
            .files
            .iter()
            .filter(|(file, _)| is_canonical_map_file(file))
            .map(|(file, value)| (file.as_str(), value))
            .collect(),
    };

    let path = rule.path().expect("file 规则已在解析边界保证存在 path");
    let mut targets = Vec::new();
    for (file, root) in files {
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
        walk_rule_path(
            rule,
            root,
            path,
            RulePathTarget {
                source,
                kind,
                base_steps: Vec::new(),
                default_group_steps: Vec::new(),
                group_by_terminal_parent: true,
            },
            &mut targets,
        )?;
    }
    Ok(targets)
}

fn match_plugin_rule(
    rule: &RuleDefinition,
    plugin_name: &str,
    input: &RulesMatchInput,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let path = rule.path().expect("plugin 规则已在解析边界保证存在 path");
    let mut targets = Vec::new();
    for plugin in input
        .plugins
        .iter()
        .filter(|plugin| plugin.enabled && plugin.name == plugin_name)
    {
        let parameters = Value::Object(plugin.parameters.clone());
        let local = collect_local_targets(
            rule,
            &parameters,
            path.segments(),
            Vec::new(),
            Vec::new(),
            true,
        )?;
        for terminal in local {
            let Some((RulesValueStep::Key(parameter_name), tail)) = terminal.steps.split_first()
            else {
                return Err(RulesMatchError::InvalidTarget {
                    rule_number: rule.rule_number(),
                    message: "插件路径必须先选择一个参数名".to_owned(),
                });
            };
            let group_steps = plugin_relative_steps(
                rule.rule_number(),
                parameter_name,
                &terminal.target.group_steps,
            )?;
            targets.push(MatchedRuleTarget {
                kind: TextGroupKind::PluginParameter,
                source: RulesMatchSource::PluginParameter {
                    plugin_index: plugin.index,
                    plugin_name: plugin.name.clone(),
                    parameter_name: parameter_name.clone(),
                },
                group_steps,
                steps: tail.to_vec(),
                ..terminal.target
            });
        }
    }
    Ok(targets)
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
            message: "插件复合文本组没有从参数名开始".to_owned(),
        });
    };
    if group_parameter != parameter_name {
        return Err(RulesMatchError::InvalidTarget {
            rule_number,
            message: "插件复合文本组跨越了两个参数".to_owned(),
        });
    }
    Ok(tail.to_vec())
}

fn match_command_rule(
    rule: &RuleDefinition,
    code: i64,
    parameter: usize,
    input: &RulesMatchInput,
) -> Result<Vec<MatchedRuleTarget>, RulesMatchError> {
    let mut targets = Vec::new();
    for (file, root) in &input.files {
        if !is_event_document(file) {
            continue;
        }
        for (list_steps, list) in event_lists(file, root) {
            for (command_index, command) in list.iter().enumerate() {
                let Some(command) = command.as_object() else {
                    continue;
                };
                if command.get("code").and_then(Value::as_i64) != Some(code) {
                    continue;
                }
                let parameters = command
                    .get("parameters")
                    .and_then(Value::as_array)
                    .ok_or_else(|| RulesMatchError::InvalidTarget {
                        rule_number: rule.rule_number(),
                        message: format!("data/{file} 的 code {code} 命令 parameters 不是数组"),
                    })?;
                let selected =
                    parameters
                        .get(parameter)
                        .ok_or_else(|| RulesMatchError::InvalidTarget {
                            rule_number: rule.rule_number(),
                            message: format!(
                                "data/{file} 的 code {code} 命令没有 parameters[{parameter}]"
                            ),
                        })?;
                let mut command_steps = list_steps.clone();
                command_steps.push(RulesValueStep::Index(command_index));
                let mut target_steps = command_steps.clone();
                target_steps.push(RulesValueStep::Key("parameters".to_owned()));
                target_steps.push(RulesValueStep::Index(parameter));
                let source = RulesMatchSource::DataFile { file: file.clone() };
                if let Some(path) = rule.path() {
                    walk_rule_path(
                        rule,
                        selected,
                        path,
                        RulePathTarget {
                            source,
                            kind: TextGroupKind::EventCommand,
                            base_steps: target_steps,
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
                        selected,
                        target_steps,
                        &command_steps,
                        false,
                        &mut local,
                    )?;
                    targets.extend(local.into_iter().map(|terminal| MatchedRuleTarget {
                        kind: TextGroupKind::EventCommand,
                        source: source.clone(),
                        ..terminal.target
                    }));
                }
            }
        }
    }
    Ok(targets)
}

struct RulePathTarget {
    source: RulesMatchSource,
    kind: TextGroupKind,
    base_steps: Vec<RulesValueStep>,
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
        target.default_group_steps,
        target.group_by_terminal_parent,
    )?;
    output.extend(local.into_iter().map(|terminal| MatchedRuleTarget {
        kind: target.kind,
        source: target.source.clone(),
        ..terminal.target
    }));
    Ok(())
}

struct LocalTarget {
    steps: Vec<RulesValueStep>,
    target: MatchedRuleTarget,
}

fn collect_local_targets(
    rule: &RuleDefinition,
    root: &Value,
    segments: &[PathSegment],
    mut base_steps: Vec<RulesValueStep>,
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
    walker.walk(root, 0, &mut base_steps)?;
    Ok(walker.output)
}

struct PathWalker<'a> {
    rule: &'a RuleDefinition,
    segments: &'a [PathSegment],
    default_group_steps: &'a [RulesValueStep],
    group_by_terminal_parent: bool,
    output: Vec<LocalTarget>,
}

impl PathWalker<'_> {
    fn walk(
        &mut self,
        value: &Value,
        segment_index: usize,
        steps: &mut Vec<RulesValueStep>,
    ) -> Result<(), RulesMatchError> {
        if segment_index == self.segments.len() {
            return visit_terminal(
                self.rule,
                value,
                steps.clone(),
                self.default_group_steps,
                self.group_by_terminal_parent,
                &mut self.output,
            );
        }

        if let Value::String(encoded) = value {
            let decoded = serde_json::from_str::<Value>(encoded).map_err(|source| {
                RulesMatchError::InvalidTarget {
                    rule_number: self.rule.rule_number(),
                    message: format!("路径继续深入时无法解码嵌套 JSON：{source}"),
                }
            })?;
            steps.push(RulesValueStep::DecodeJsonString);
            let result = self.walk(&decoded, segment_index, steps);
            steps.pop();
            return result;
        }

        match &self.segments[segment_index] {
            PathSegment::Key(key) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| RulesMatchError::InvalidTarget {
                        rule_number: self.rule.rule_number(),
                        message: format!("路径需要对象字段 {key:?}，但当前值不是对象"),
                    })?;
                if let Some(child) = object.get(key) {
                    steps.push(RulesValueStep::Key(key.clone()));
                    let result = self.walk(child, segment_index + 1, steps);
                    steps.pop();
                    result?;
                }
            }
            PathSegment::Index(index) => {
                let array = value
                    .as_array()
                    .ok_or_else(|| RulesMatchError::InvalidTarget {
                        rule_number: self.rule.rule_number(),
                        message: format!("路径需要数组下标 [{index}]，但当前值不是数组"),
                    })?;
                if let Some(child) = array.get(*index) {
                    steps.push(RulesValueStep::Index(*index));
                    let result = self.walk(child, segment_index + 1, steps);
                    steps.pop();
                    result?;
                }
            }
            PathSegment::AnyIndex => {
                let array = value
                    .as_array()
                    .ok_or_else(|| RulesMatchError::InvalidTarget {
                        rule_number: self.rule.rule_number(),
                        message: "路径使用 []，但当前值不是数组".to_owned(),
                    })?;
                for (index, child) in array.iter().enumerate() {
                    if child.is_null() {
                        continue;
                    }
                    steps.push(RulesValueStep::Index(index));
                    let result = self.walk(child, segment_index + 1, steps);
                    steps.pop();
                    result?;
                }
            }
        }
        Ok(())
    }
}

fn visit_terminal(
    rule: &RuleDefinition,
    value: &Value,
    mut steps: Vec<RulesValueStep>,
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
                message: "decode_json 的最终目标不是字符串".to_owned(),
            })?;
        final_value = serde_json::from_str::<Value>(encoded).map_err(|source| {
            RulesMatchError::InvalidTarget {
                rule_number: rule.rule_number(),
                message: format!("decode_json 无法解码最终 JSON 字符串：{source}"),
            }
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
            message: "最终目标必须是字符串".to_owned(),
        })?;
    let group_steps = if group_by_terminal_parent {
        terminal_group_steps(&steps, default_group_steps)
    } else {
        default_group_steps.to_vec()
    };
    if let Some(target) = materialize_target(rule, text, group_steps, steps.clone())? {
        output.push(LocalTarget { steps, target });
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
    let (leaves, parts) = if let Some(pattern) = rule.pattern() {
        let mut captures = Vec::new();
        for result in pattern.regex().captures_iter(text.as_bytes()) {
            let result = result.map_err(|source| RulesMatchError::PatternMatch {
                rule_number: rule.rule_number(),
                source,
            })?;
            let whole = result.get(0).expect("PCRE2 成功 captures 必须包含完整匹配");
            if whole.start() == whole.end() {
                return Err(RulesMatchError::ZeroWidthMatch {
                    rule_number: rule.rule_number(),
                });
            }
            let capture = result
                .name("text")
                .ok_or(RulesMatchError::MissingTextCapture {
                    rule_number: rule.rule_number(),
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
                });
            }
            captures.push((capture.start(), capture.end()));
        }
        materialize_captures(rule.rule_number(), text, captures)?
    } else if text.trim().is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (
            vec![MatchedRuleLeaf {
                original_text: text.to_owned(),
            }],
            vec![MatchedRulePart::TextSlot { leaf_index: 0 }],
        )
    };
    if leaves.is_empty() {
        return Ok(None);
    }
    let originals = leaves
        .iter()
        .map(|leaf| leaf.original_text.as_str())
        .collect::<Vec<_>>();
    if materialize_parts(&parts, &originals).as_deref() != Some(text) {
        return Err(RulesMatchError::InvalidMaterialization {
            rule_number: rule.rule_number(),
            message: "Literal/TextSlot 配方无法逐字重建来源字符串".to_owned(),
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
        leaves,
        parts,
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
) -> Result<(Vec<MatchedRuleLeaf>, Vec<MatchedRulePart>), RulesMatchError> {
    let mut leaves = Vec::new();
    let mut parts = Vec::new();
    let mut cursor = 0;
    let mut previous_capture_end = 0;
    for (start, end) in captures {
        if start == end {
            return Err(RulesMatchError::ZeroWidthMatch { rule_number });
        }
        if start < previous_capture_end {
            return Err(RulesMatchError::OverlappingMatch { rule_number });
        }
        previous_capture_end = end;
        if text[start..end].trim().is_empty() {
            continue;
        }
        if cursor < start {
            push_literal(&mut parts, &text[cursor..start]);
        }
        let leaf_index = leaves.len();
        leaves.push(MatchedRuleLeaf {
            original_text: text[start..end].to_owned(),
        });
        parts.push(MatchedRulePart::TextSlot { leaf_index });
        cursor = end;
    }
    if cursor < text.len() {
        push_literal(&mut parts, &text[cursor..]);
    }
    Ok((leaves, parts))
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
            MatchedRulePart::TextSlot { leaf_index } => {
                output.push_str(values.get(*leaf_index)?.as_ref());
            }
        }
    }
    Some(output)
}

fn is_event_document(file: &str) -> bool {
    is_canonical_map_file(file) || matches!(file, "CommonEvents.json" | "Troops.json")
}

fn is_canonical_map_file(file: &str) -> bool {
    canonical_map_id(file).is_some()
}

fn canonical_map_id(file: &str) -> Option<u32> {
    let digits = file
        .strip_prefix("Map")
        .and_then(|value| value.strip_suffix(".json"))?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<u32>()
        .ok()
        .filter(|id| format!("Map{id:03}.json") == file)
}

fn shared_step(step: &RulesValueStep) -> RpgMakerLocationStep {
    match step {
        RulesValueStep::Key(key) => RpgMakerLocationStep::key(key),
        RulesValueStep::Index(index) => RpgMakerLocationStep::index(*index),
        RulesValueStep::DecodeJsonString => RpgMakerLocationStep::DecodeJsonString,
    }
}

fn event_lists<'a>(file: &str, root: &'a Value) -> Vec<(Vec<RulesValueStep>, &'a [Value])> {
    let mut lists = Vec::new();
    if is_canonical_map_file(file) {
        let Some(events) = root.get("events").and_then(Value::as_array) else {
            return lists;
        };
        for (event_index, event) in events.iter().enumerate() {
            let Some(pages) = event.get("pages").and_then(Value::as_array) else {
                continue;
            };
            for (page_index, page) in pages.iter().enumerate() {
                let Some(list) = page.get("list").and_then(Value::as_array) else {
                    continue;
                };
                lists.push((
                    vec![
                        RulesValueStep::Key("events".to_owned()),
                        RulesValueStep::Index(event_index),
                        RulesValueStep::Key("pages".to_owned()),
                        RulesValueStep::Index(page_index),
                        RulesValueStep::Key("list".to_owned()),
                    ],
                    list.as_slice(),
                ));
            }
        }
    } else if file == "CommonEvents.json" {
        let Some(events) = root.as_array() else {
            return lists;
        };
        for (event_index, event) in events.iter().enumerate() {
            let Some(list) = event.get("list").and_then(Value::as_array) else {
                continue;
            };
            lists.push((
                vec![
                    RulesValueStep::Index(event_index),
                    RulesValueStep::Key("list".to_owned()),
                ],
                list.as_slice(),
            ));
        }
    } else if file == "Troops.json" {
        let Some(troops) = root.as_array() else {
            return lists;
        };
        for (troop_index, troop) in troops.iter().enumerate() {
            let Some(pages) = troop.get("pages").and_then(Value::as_array) else {
                continue;
            };
            for (page_index, page) in pages.iter().enumerate() {
                let Some(list) = page.get("list").and_then(Value::as_array) else {
                    continue;
                };
                lists.push((
                    vec![
                        RulesValueStep::Index(troop_index),
                        RulesValueStep::Key("pages".to_owned()),
                        RulesValueStep::Index(page_index),
                        RulesValueStep::Key("list".to_owned()),
                    ],
                    list.as_slice(),
                ));
            }
        }
    }
    lists
}

/// Rules 匹配阶段的输入或目标错误。
#[derive(Debug)]
pub(crate) enum RulesMatchError {
    NoNonBlankMatch {
        rule_number: usize,
    },
    InvalidTarget {
        rule_number: usize,
        message: String,
    },
    PatternMatch {
        rule_number: usize,
        source: pcre2::Error,
    },
    ZeroWidthMatch {
        rule_number: usize,
    },
    OverlappingMatch {
        rule_number: usize,
    },
    MissingTextCapture {
        rule_number: usize,
    },
    InvalidCaptureRange {
        rule_number: usize,
    },
    DuplicateTarget {
        first_rule: usize,
        second_rule: usize,
        source: RulesMatchSource,
        steps: Vec<RulesValueStep>,
    },
    InvalidMaterialization {
        rule_number: usize,
        message: String,
    },
}

impl fmt::Display for RulesMatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNonBlankMatch { rule_number } => {
                write!(formatter, "Rules 规则 {rule_number} 没有产生任何非空翻译叶")
            }
            Self::InvalidTarget {
                rule_number,
                message,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 命中了无效目标：{message}"
            ),
            Self::PatternMatch {
                rule_number,
                source,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 执行 PCRE2 失败：{source}"
            ),
            Self::ZeroWidthMatch { rule_number } => {
                write!(formatter, "Rules 规则 {rule_number} 产生了零宽匹配")
            }
            Self::OverlappingMatch { rule_number } => {
                write!(formatter, "Rules 规则 {rule_number} 产生了重叠的 text 捕获")
            }
            Self::MissingTextCapture { rule_number } => write!(
                formatter,
                "Rules 规则 {rule_number} 的 text 捕获在一次匹配中没有参与"
            ),
            Self::InvalidCaptureRange { rule_number } => write!(
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
                message,
            } => write!(
                formatter,
                "Rules 规则 {rule_number} 的物化配方无效：{message}"
            ),
        }
    }
}

impl Error for RulesMatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PatternMatch { source, .. } => Some(source),
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
        assert_eq!(targets[0].leaves.len(), 2);
        assert_eq!(targets[0].leaves[0].original_text, "第一段");
        assert_eq!(targets[0].leaves[1].original_text, "第二段");
        let originals = targets[0]
            .leaves
            .iter()
            .map(|leaf| leaf.original_text.clone())
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
        assert_eq!(targets[0].leaves[0].original_text, "手柄未连接");
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
                .map(|target| target.leaves[0].original_text.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["命令正文", "文件正文"])
        );
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
            .flat_map(|target| target.leaves.iter())
            .map(|leaf| leaf.original_text.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(originals, BTreeSet::from(["你好", "说明"]));
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
    fn every_nonempty_rule_must_produce_a_nonblank_leaf() {
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

        let targets = match_rules(&definition, &input).expect("非空捕获应产生翻译叶");

        assert_eq!(targets[0].leaves.len(), 1);
        assert_eq!(targets[0].leaves[0].original_text, "甲");
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

        assert!(matches!(
            match_rules(&zero, &input),
            Err(RulesMatchError::ZeroWidthMatch { .. })
        ));
        assert!(matches!(
            match_rules(&optional, &input),
            Err(RulesMatchError::MissingTextCapture { .. })
        ));
        assert!(matches!(
            match_rules(&zero_capture, &input),
            Err(RulesMatchError::ZeroWidthMatch { .. })
        ));
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

        assert!(matches!(
            match_rules(&definition, &input),
            Err(RulesMatchError::InvalidCaptureRange { .. })
        ));
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

        assert!(matches!(
            match_rules(&definition, &input),
            Err(RulesMatchError::InvalidCaptureRange { .. })
        ));
    }

    fn input<const N: usize>(files: [(&str, Value); N]) -> RulesMatchInput {
        RulesMatchInput::new(
            files
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            Vec::new(),
        )
    }
}
