//! 可信 Lua 使用的 RPG Maker 只读文档与结构化位置能力。

use std::error::Error;
use std::fmt;

use crate::rpg_maker::lua::json::{
    HostValueBudget, LosslessJsonError, LosslessJsonValue, decode as decode_json,
};
use crate::rpg_maker::tag::simple_tag_spans;
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
};

use super::LuaSourcePath;

/// 一个由 Rust 从冻结来源建立的 RPG Maker 文档。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenedRpgMakerDocument {
    source: RpgMakerSource,
    root: LosslessJsonValue,
    budget: HostValueBudget,
}

impl OpenedRpgMakerDocument {
    pub(crate) fn open(
        source: RpgMakerSource,
        bytes: &[u8],
        budget: HostValueBudget,
    ) -> Result<Self, RpgMakerDocumentError> {
        if bytes.len() > budget.max_bytes().get() {
            return Err(RpgMakerDocumentError::InputTooLarge {
                actual: bytes.len(),
                maximum: budget.max_bytes().get(),
            });
        }
        let text = std::str::from_utf8(bytes).map_err(|_| RpgMakerDocumentError::InvalidUtf8)?;
        let root = match &source {
            RpgMakerSource::Data(_) | RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_) => {
                decode_json(text, budget).map_err(RpgMakerDocumentError::InvalidJson)?
            }
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => {
                let plugins = decode_plugins_envelope(text, budget)?;
                select_plugin_parameter(&plugins, *plugin_index, plugin_name, parameter_name)?
            }
        };
        Ok(Self {
            source,
            root,
            budget,
        })
    }

    pub(crate) fn source(&self) -> &RpgMakerSource {
        &self.source
    }

    pub(crate) fn value(
        &self,
        steps: &[RpgMakerLocationStep],
    ) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
        resolve_value(&self.root, steps, self.budget)
    }

    pub(crate) fn location(
        &self,
        steps: &[RpgMakerLocationStep],
    ) -> Result<RpgMakerLocation, RpgMakerDocumentError> {
        self.value(steps)?;
        Ok(RpgMakerLocation::value(self.source.clone(), steps.to_vec()))
    }

    pub(crate) fn text(
        &self,
        steps: &[RpgMakerLocationStep],
    ) -> Result<RpgMakerTextReference, RpgMakerDocumentError> {
        let value = self.value(steps)?;
        let LosslessJsonValue::String(original) = value else {
            return Err(RpgMakerDocumentError::ExpectedString);
        };
        Ok(RpgMakerTextReference::new(
            original,
            RpgMakerLocation::value(self.source.clone(), steps.to_vec()),
        ))
    }

    pub(crate) fn note_tag(
        &self,
        container_steps: &[RpgMakerLocationStep],
        tag_name: &str,
        occurrence: usize,
    ) -> Result<RpgMakerTextReference, RpgMakerDocumentError> {
        validate_tag_name(tag_name)?;
        let container = self.value(container_steps)?;
        let LosslessJsonValue::Object(fields) = container else {
            return Err(RpgMakerDocumentError::ExpectedObject);
        };
        let Some(LosslessJsonValue::String(note)) = object_get(&fields, "note") else {
            return Err(RpgMakerDocumentError::ExpectedNoteString);
        };
        let value = find_tag(note, tag_name, occurrence)?;
        Ok(RpgMakerTextReference::new(
            value.to_owned(),
            RpgMakerLocation::note_tag(
                self.source.clone(),
                container_steps.to_vec(),
                tag_name,
                occurrence,
            ),
        ))
    }

    pub(crate) fn comment_tag(
        &self,
        command_steps: &[RpgMakerLocationStep],
        tag_name: &str,
        occurrence: usize,
    ) -> Result<RpgMakerTextReference, RpgMakerDocumentError> {
        validate_tag_name(tag_name)?;
        let Some((last, list_steps)) = command_steps.split_last() else {
            return Err(RpgMakerDocumentError::ExpectedCommandPath);
        };
        let RpgMakerLocationStep::ArrayIndex(start_index) = last else {
            return Err(RpgMakerDocumentError::ExpectedCommandPath);
        };
        let list = self.value(list_steps)?;
        let LosslessJsonValue::Array(commands) = list else {
            return Err(RpgMakerDocumentError::ExpectedCommandList);
        };
        let mut lines = Vec::new();
        for (relative, command) in commands.iter().skip(*start_index).enumerate() {
            let LosslessJsonValue::Object(fields) = command else {
                if relative == 0 {
                    return Err(RpgMakerDocumentError::ExpectedCommandObject);
                }
                break;
            };
            let Some(code) = object_get(fields, "code").and_then(json_integer) else {
                if relative == 0 {
                    return Err(RpgMakerDocumentError::ExpectedCommandCode);
                }
                break;
            };
            let expected = if relative == 0 { 108 } else { 408 };
            if code != expected {
                if relative == 0 {
                    return Err(RpgMakerDocumentError::ExpectedCommentStart);
                }
                break;
            }
            let Some(LosslessJsonValue::Array(parameters)) = object_get(fields, "parameters")
            else {
                return Err(RpgMakerDocumentError::ExpectedCommandParameters);
            };
            let Some(LosslessJsonValue::String(line)) = parameters.first() else {
                return Err(RpgMakerDocumentError::ExpectedCommentLine);
            };
            lines.push(line.as_str());
        }
        let text = lines.join("\n");
        let value = find_tag(&text, tag_name, occurrence)?;
        Ok(RpgMakerTextReference::new(
            value.to_owned(),
            RpgMakerLocation::comment_tag(
                self.source.clone(),
                command_steps.to_vec(),
                tag_name,
                occurrence,
            ),
        ))
    }
}

/// 由 Rust 建立且同时携带冻结原文和结构化身份的文本引用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTextReference {
    original: String,
    location: RpgMakerLocation,
}

impl RpgMakerTextReference {
    fn new(original: String, location: RpgMakerLocation) -> Self {
        Self { original, location }
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) fn location(&self) -> &RpgMakerLocation {
        &self.location
    }
}

/// 一个 RPG Maker 来源在冻结项目根中的物理文件。
pub(crate) fn source_path(source: &RpgMakerSource) -> LuaSourcePath {
    let path = match source {
        RpgMakerSource::Data(file) => format!("data/{}", file.file_name()),
        RpgMakerSource::DataFile(file) => format!("data/{file}"),
        RpgMakerSource::Map(map_id) => format!("data/Map{map_id:03}.json"),
        RpgMakerSource::PluginParameter { .. } => "js/plugins.js".to_owned(),
    };
    LuaSourcePath::parse(&path).expect("固定 RPG Maker 来源路径必须满足 Lua 来源边界")
}

/// Lua `ctx.rpg_maker.data` 能建立的标准来源。
pub(crate) fn data_source(file_name: &str) -> Result<RpgMakerSource, RpgMakerDocumentError> {
    StandardDataFile::from_file_name(file_name)
        .map(RpgMakerSource::data)
        .ok_or(RpgMakerDocumentError::UnknownStandardDataFile)
}

/// Lua `ctx.rpg_maker.map` 能建立的正整数地图来源。
pub(crate) fn map_source(map_id: i64) -> Result<RpgMakerSource, RpgMakerDocumentError> {
    let map_id = u32::try_from(map_id).map_err(|_| RpgMakerDocumentError::InvalidMapId)?;
    if map_id == 0 {
        return Err(RpgMakerDocumentError::InvalidMapId);
    }
    Ok(RpgMakerSource::map(map_id))
}

/// Lua `ctx.rpg_maker.plugin_parameter` 能建立的插件参数来源。
pub(crate) fn plugin_parameter_source(
    plugin_index: i64,
    plugin_name: &str,
    parameter_name: &str,
) -> Result<RpgMakerSource, RpgMakerDocumentError> {
    let plugin_index = usize::try_from(plugin_index)
        .map_err(|_| RpgMakerDocumentError::InvalidPluginParameterSource)?;
    if plugin_name.is_empty()
        || plugin_name.trim() != plugin_name
        || parameter_name.is_empty()
        || parameter_name.trim() != parameter_name
    {
        return Err(RpgMakerDocumentError::InvalidPluginParameterSource);
    }
    Ok(RpgMakerSource::plugin_parameter(
        plugin_index,
        plugin_name,
        parameter_name,
    ))
}

fn resolve_value(
    root: &LosslessJsonValue,
    steps: &[RpgMakerLocationStep],
    budget: HostValueBudget,
) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
    let mut current = ResolvedValue::Borrowed(root);
    for step in steps {
        current = match (current, step) {
            (
                ResolvedValue::Borrowed(LosslessJsonValue::Object(entries)),
                RpgMakerLocationStep::ObjectKey(key),
            ) => ResolvedValue::Borrowed(
                object_get(entries, key).ok_or(RpgMakerDocumentError::MissingObjectKey)?,
            ),
            (
                ResolvedValue::Owned(LosslessJsonValue::Object(entries)),
                RpgMakerLocationStep::ObjectKey(key),
            ) => ResolvedValue::Owned(
                entries
                    .into_iter()
                    .find_map(|(candidate, value)| (candidate == *key).then_some(value))
                    .ok_or(RpgMakerDocumentError::MissingObjectKey)?,
            ),
            (
                ResolvedValue::Borrowed(LosslessJsonValue::Array(values)),
                RpgMakerLocationStep::ArrayIndex(index),
            ) => ResolvedValue::Borrowed(
                values
                    .get(*index)
                    .ok_or(RpgMakerDocumentError::MissingArrayIndex)?,
            ),
            (
                ResolvedValue::Owned(LosslessJsonValue::Array(values)),
                RpgMakerLocationStep::ArrayIndex(index),
            ) => ResolvedValue::Owned(
                values
                    .into_iter()
                    .nth(*index)
                    .ok_or(RpgMakerDocumentError::MissingArrayIndex)?,
            ),
            (
                ResolvedValue::Borrowed(LosslessJsonValue::String(source)),
                RpgMakerLocationStep::DecodeJsonString,
            ) => ResolvedValue::Owned(
                decode_json(source, budget).map_err(RpgMakerDocumentError::InvalidNestedJson)?,
            ),
            (
                ResolvedValue::Owned(LosslessJsonValue::String(source)),
                RpgMakerLocationStep::DecodeJsonString,
            ) => ResolvedValue::Owned(
                decode_json(&source, budget).map_err(RpgMakerDocumentError::InvalidNestedJson)?,
            ),
            (_, RpgMakerLocationStep::DecodeJsonString) => {
                return Err(RpgMakerDocumentError::ExpectedEncodedJsonString);
            }
            (_, RpgMakerLocationStep::ObjectKey(_)) => {
                return Err(RpgMakerDocumentError::ExpectedObject);
            }
            (_, RpgMakerLocationStep::ArrayIndex(_)) => {
                return Err(RpgMakerDocumentError::ExpectedArray);
            }
        };
    }
    Ok(current.into_owned())
}

enum ResolvedValue<'a> {
    Borrowed(&'a LosslessJsonValue),
    Owned(LosslessJsonValue),
}

impl ResolvedValue<'_> {
    fn into_owned(self) -> LosslessJsonValue {
        match self {
            Self::Borrowed(value) => value.clone(),
            Self::Owned(value) => value,
        }
    }
}

fn decode_plugins_envelope(
    text: &str,
    budget: HostValueBudget,
) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
    let Some((prefix, assignment)) = text.split_once("var $plugins") else {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    };
    if !prefix
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with("//"))
    {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    }
    let Some(json_with_terminator) = assignment.trim_start().strip_prefix('=') else {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    };
    let Some(json) = json_with_terminator.trim().strip_suffix(';') else {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    };
    let value = decode_json(json.trim(), budget).map_err(RpgMakerDocumentError::InvalidJson)?;
    if !matches!(value, LosslessJsonValue::Array(_)) {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    }
    Ok(value)
}

fn select_plugin_parameter(
    plugins: &LosslessJsonValue,
    plugin_index: usize,
    plugin_name: &str,
    parameter_name: &str,
) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
    let LosslessJsonValue::Array(plugins) = plugins else {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope);
    };
    let Some(LosslessJsonValue::Object(plugin)) = plugins.get(plugin_index) else {
        return Err(RpgMakerDocumentError::PluginIndexMissing);
    };
    let Some(LosslessJsonValue::String(actual_name)) = object_get(plugin, "name") else {
        return Err(RpgMakerDocumentError::PluginNameMissing);
    };
    if actual_name != plugin_name {
        return Err(RpgMakerDocumentError::PluginNameMismatch);
    }
    let Some(LosslessJsonValue::Object(parameters)) = object_get(plugin, "parameters") else {
        return Err(RpgMakerDocumentError::PluginParametersMissing);
    };
    let Some(LosslessJsonValue::String(value)) = object_get(parameters, parameter_name) else {
        return Err(RpgMakerDocumentError::PluginParameterMissing);
    };
    Ok(LosslessJsonValue::String(value.clone()))
}

fn object_get<'a>(
    entries: &'a [(String, LosslessJsonValue)],
    key: &str,
) -> Option<&'a LosslessJsonValue> {
    entries
        .iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(value))
}

fn json_integer(value: &LosslessJsonValue) -> Option<i64> {
    let LosslessJsonValue::Number(value) = value else {
        return None;
    };
    value.parse().ok()
}

fn validate_tag_name(tag_name: &str) -> Result<(), RpgMakerDocumentError> {
    if tag_name.is_empty()
        || tag_name
            .chars()
            .any(|character| matches!(character, '<' | '>' | ':'))
    {
        Err(RpgMakerDocumentError::InvalidTagName)
    } else {
        Ok(())
    }
}

fn find_tag<'a>(
    text: &'a str,
    tag_name: &str,
    occurrence: usize,
) -> Result<&'a str, RpgMakerDocumentError> {
    simple_tag_spans(text)
        .into_iter()
        .find(|tag| tag.name() == tag_name && tag.occurrence() == occurrence)
        .map(|tag| tag.value())
        .ok_or(RpgMakerDocumentError::TagNotFound)
}

/// RPG Maker 文档来源、结构路径或目标值无效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerDocumentError {
    UnknownStandardDataFile,
    InvalidMapId,
    InvalidPluginParameterSource,
    InputTooLarge { actual: usize, maximum: usize },
    InvalidUtf8,
    InvalidJson(LosslessJsonError),
    InvalidNestedJson(LosslessJsonError),
    InvalidPluginsEnvelope,
    PluginIndexMissing,
    PluginNameMissing,
    PluginNameMismatch,
    PluginParametersMissing,
    PluginParameterMissing,
    ExpectedObject,
    ExpectedArray,
    MissingObjectKey,
    MissingArrayIndex,
    ExpectedEncodedJsonString,
    ExpectedString,
    ExpectedNoteString,
    ExpectedCommandPath,
    ExpectedCommandList,
    ExpectedCommandObject,
    ExpectedCommandCode,
    ExpectedCommentStart,
    ExpectedCommandParameters,
    ExpectedCommentLine,
    InvalidTagName,
    TagNotFound,
}

impl RpgMakerDocumentError {
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::UnknownStandardDataFile | Self::InvalidMapId => "invalid_source",
            Self::InvalidPluginParameterSource
            | Self::PluginIndexMissing
            | Self::PluginNameMissing
            | Self::PluginNameMismatch
            | Self::PluginParametersMissing
            | Self::PluginParameterMissing => "invalid_plugin_parameter_source",
            Self::InputTooLarge { .. } => "resource_limit",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidJson(_) | Self::InvalidNestedJson(_) => "invalid_json",
            Self::InvalidPluginsEnvelope => "invalid_plugins_envelope",
            Self::ExpectedObject
            | Self::ExpectedArray
            | Self::MissingObjectKey
            | Self::MissingArrayIndex
            | Self::ExpectedEncodedJsonString
            | Self::ExpectedString
            | Self::ExpectedNoteString
            | Self::ExpectedCommandPath
            | Self::ExpectedCommandList
            | Self::ExpectedCommandObject
            | Self::ExpectedCommandCode
            | Self::ExpectedCommentStart
            | Self::ExpectedCommandParameters
            | Self::ExpectedCommentLine => "invalid_location",
            Self::InvalidTagName => "invalid_tag",
            Self::TagNotFound => "tag_not_found",
        }
    }
}

impl fmt::Display for RpgMakerDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStandardDataFile => formatter.write_str("不是标准 RPG Maker Data 文件名"),
            Self::InvalidMapId => formatter.write_str("RPG Maker map ID 必须是正 u32 整数"),
            Self::InvalidPluginParameterSource => {
                formatter.write_str("RPG Maker 插件参数来源的索引或名称无效")
            }
            Self::InputTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "RPG Maker 文档为 {actual} 字节，超过上限 {maximum}"
                )
            }
            Self::InvalidUtf8 => formatter.write_str("RPG Maker 文档不是有效 UTF-8"),
            Self::InvalidJson(error) => write!(formatter, "RPG Maker 文档不是有效 JSON：{error}"),
            Self::InvalidNestedJson(error) => {
                write!(formatter, "RPG Maker JSON 字符串不是有效 JSON：{error}")
            }
            Self::InvalidPluginsEnvelope => {
                formatter.write_str("文档不是 RPG Maker 生成的 plugins.js 格式")
            }
            Self::PluginIndexMissing => formatter.write_str("plugins.js 中不存在指定插件索引"),
            Self::PluginNameMissing => formatter.write_str("plugins.js 插件记录缺少字符串 name"),
            Self::PluginNameMismatch => formatter.write_str("plugins.js 指定索引的插件名称不匹配"),
            Self::PluginParametersMissing => {
                formatter.write_str("plugins.js 插件记录缺少 parameters 对象")
            }
            Self::PluginParameterMissing => {
                formatter.write_str("plugins.js 中不存在指定字符串参数")
            }
            Self::ExpectedObject => formatter.write_str("RPG Maker 路径当前值不是 JSON object"),
            Self::ExpectedArray => formatter.write_str("RPG Maker 路径当前值不是 JSON array"),
            Self::MissingObjectKey => formatter.write_str("RPG Maker 路径的 object key 不存在"),
            Self::MissingArrayIndex => formatter.write_str("RPG Maker 路径的 array index 不存在"),
            Self::ExpectedEncodedJsonString => {
                formatter.write_str("DecodeJsonString 当前值不是 JSON string")
            }
            Self::ExpectedString => formatter.write_str("RPG Maker 文本位置不是 JSON string"),
            Self::ExpectedNoteString => formatter.write_str("RPG Maker Note 容器缺少字符串 note"),
            Self::ExpectedCommandPath => {
                formatter.write_str("RPG Maker 注释路径必须终止于事件指令数组下标")
            }
            Self::ExpectedCommandList => {
                formatter.write_str("RPG Maker 指令父位置不是事件指令数组")
            }
            Self::ExpectedCommandObject => formatter.write_str("RPG Maker 事件指令不是对象"),
            Self::ExpectedCommandCode => formatter.write_str("RPG Maker 事件指令 code 不是整数"),
            Self::ExpectedCommentStart => {
                formatter.write_str("RPG Maker 注释起始指令 code 不是 108")
            }
            Self::ExpectedCommandParameters => {
                formatter.write_str("RPG Maker 注释指令 parameters 不是数组")
            }
            Self::ExpectedCommentLine => {
                formatter.write_str("RPG Maker 108/408 注释正文不是字符串")
            }
            Self::InvalidTagName => formatter.write_str("RPG Maker 标签名无效"),
            Self::TagNotFound => formatter.write_str("RPG Maker 文本中不存在指定标签 occurrence"),
        }
    }
}

impl Error for RpgMakerDocumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidJson(error) | Self::InvalidNestedJson(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    fn budget() -> HostValueBudget {
        HostValueBudget::new(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(256).unwrap(),
            NonZeroUsize::new(16).unwrap(),
        )
    }

    #[test]
    fn resolves_zero_based_paths_nested_json_and_text_identity() {
        let document = OpenedRpgMakerDocument::open(
            RpgMakerSource::data(StandardDataFile::Items),
            r#"[null,{"note":"<Help:说明>","nested":"[{\"name\":\"值\"}]"}]"#.as_bytes(),
            budget(),
        )
        .unwrap();
        let steps = vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("nested"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::index(0),
            RpgMakerLocationStep::key("name"),
        ];
        let text = document.text(&steps).unwrap();
        assert_eq!(text.original(), "值");
        assert_eq!(
            text.location(),
            &RpgMakerLocation::value(document.source().clone(), steps)
        );
        assert_eq!(
            document
                .note_tag(&[RpgMakerLocationStep::index(1)], "Help", 0)
                .unwrap()
                .original(),
            "说明"
        );
    }

    #[test]
    fn resolves_contiguous_108_408_comment_tags() {
        let document = OpenedRpgMakerDocument::open(
            RpgMakerSource::map(1),
            r#"{"list":[{"code":108,"parameters":["<Quest:第一"]},{"code":408,"parameters":["行>"]},{"code":0,"parameters":[]}] }"#.as_bytes(),
            budget(),
        )
        .unwrap();
        let reference = document
            .comment_tag(
                &[
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(0),
                ],
                "Quest",
                0,
            )
            .unwrap();
        assert_eq!(reference.original(), "第一\n行");
    }

    #[test]
    fn selects_plugin_parameter_by_index_name_and_parameter() {
        let source = RpgMakerSource::plugin_parameter(1, "Quest", "Entries");
        let document = OpenedRpgMakerDocument::open(
            source,
            r#"// generated
var $plugins = [{"name":"Other","parameters":{}},{"name":"Quest","parameters":{"Entries":"[{\"Title\":\"任务\"}]"}}];"#.as_bytes(),
            budget(),
        )
        .unwrap();
        let text = document
            .text(&[
                RpgMakerLocationStep::DecodeJsonString,
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("Title"),
            ])
            .unwrap();
        assert_eq!(text.original(), "任务");
    }
}
