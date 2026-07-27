//! 可信 Lua 使用的 RPG Maker 只读文档与结构化位置能力。

use std::error::Error;
use std::fmt;

use crate::rpg_maker::lua::json::{LosslessJsonError, LosslessJsonValue, decode as decode_json};
use crate::rpg_maker::model::MutationClaim;
use crate::rpg_maker::plugin_document::{
    PluginsEnvelopeFailure, parse_plugins_envelope, validate_plugins_root_is_array,
};
use crate::rpg_maker::text::{
    DataFileName, DataFileNameError, MapId, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource,
    StandardDataFile,
};

use super::LuaSourcePath;

/// 一个由 Rust 从冻结来源建立的 RPG Maker 文档。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenedRpgMakerDocument {
    source: RpgMakerSource,
    root: LosslessJsonValue,
}

impl OpenedRpgMakerDocument {
    pub(crate) fn open(
        source: RpgMakerSource,
        bytes: &[u8],
    ) -> Result<Self, RpgMakerDocumentError> {
        let text = std::str::from_utf8(bytes).map_err(|_| RpgMakerDocumentError::InvalidUtf8)?;
        let root = match &source {
            RpgMakerSource::Data(_) | RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_) => {
                decode_json(text).map_err(RpgMakerDocumentError::InvalidJson)?
            }
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => {
                let plugins = decode_plugins_envelope(text)?;
                select_plugin_parameter(&plugins, *plugin_index, plugin_name, parameter_name)?
            }
        };
        Ok(Self { source, root })
    }

    pub(crate) fn source(&self) -> &RpgMakerSource {
        &self.source
    }

    pub(crate) fn value(
        &self,
        steps: &[RpgMakerLocationStep],
    ) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
        resolve_value(&self.root, steps)
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
        let LosslessJsonValue::String(original) = &value else {
            return Err(RpgMakerDocumentError::ExpectedString);
        };
        Ok(RpgMakerTextReference::new(
            original.clone(),
            RpgMakerLocation::value(self.source.clone(), steps.to_vec()),
        ))
    }
}

/// 由 Rust 建立且同时携带冻结原文和结构化身份的文本引用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTextReference {
    original: String,
    location: RpgMakerLocation,
    mutation_claim: MutationClaim,
}

impl RpgMakerTextReference {
    fn new(original: String, location: RpgMakerLocation) -> Self {
        let mutation_claim = MutationClaim::for_location(location.clone());
        Self {
            original,
            location,
            mutation_claim,
        }
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) fn location(&self) -> &RpgMakerLocation {
        &self.location
    }

    pub(crate) fn mutation_claim(&self) -> &MutationClaim {
        &self.mutation_claim
    }
}

/// 一个 RPG Maker 来源在冻结项目根中的物理文件。
pub(crate) fn source_path(source: &RpgMakerSource) -> LuaSourcePath {
    let path = match source {
        RpgMakerSource::Data(file) => format!("data/{}", file.file_name()),
        RpgMakerSource::DataFile(file) => format!("data/{file}"),
        RpgMakerSource::Map(map_id) => format!("data/{}", map_id.file_name()),
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

/// Lua `ctx.rpg_maker.data_file` 能建立的精确安全 Data 文件来源。
///
/// 标准文件和规范 Map 复用同一物理身份分类；`Map000.json`、`Map01.json` 等
/// 非规范名称仍是调用方明确选择的自定义 Data 文件。
pub(crate) fn data_file_source(file_name: &str) -> Result<RpgMakerSource, RpgMakerDocumentError> {
    DataFileName::parse(file_name.to_owned())
        .map(RpgMakerSource::data_file)
        .map_err(RpgMakerDocumentError::InvalidDataFileName)
}

/// Lua `ctx.rpg_maker.map` 能建立的正整数地图来源。
pub(crate) fn map_source(map_id: i64) -> Result<RpgMakerSource, RpgMakerDocumentError> {
    let map_id = u32::try_from(map_id).map_err(|_| RpgMakerDocumentError::InvalidMapId)?;
    MapId::new(map_id)
        .map(RpgMakerSource::map_id)
        .map_err(|_| RpgMakerDocumentError::InvalidMapId)
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
            (ResolvedValue::Owned(mut value), RpgMakerLocationStep::ObjectKey(key)) => {
                if !matches!(&value, LosslessJsonValue::Object(_)) {
                    return Err(RpgMakerDocumentError::ExpectedObject);
                }
                ResolvedValue::Owned(
                    value
                        .take_object_value(key)
                        .ok_or(RpgMakerDocumentError::MissingObjectKey)?,
                )
            }
            (
                ResolvedValue::Borrowed(LosslessJsonValue::Array(values)),
                RpgMakerLocationStep::ArrayIndex(index),
            ) => ResolvedValue::Borrowed(
                values
                    .get(*index)
                    .ok_or(RpgMakerDocumentError::MissingArrayIndex)?,
            ),
            (ResolvedValue::Owned(mut value), RpgMakerLocationStep::ArrayIndex(index)) => {
                if !matches!(&value, LosslessJsonValue::Array(_)) {
                    return Err(RpgMakerDocumentError::ExpectedArray);
                }
                ResolvedValue::Owned(
                    value
                        .take_array_value(*index)
                        .ok_or(RpgMakerDocumentError::MissingArrayIndex)?,
                )
            }
            (
                ResolvedValue::Borrowed(LosslessJsonValue::String(source)),
                RpgMakerLocationStep::DecodeJsonString,
            ) => ResolvedValue::Owned(
                decode_json(source).map_err(RpgMakerDocumentError::InvalidNestedJson)?,
            ),
            (ResolvedValue::Owned(value), RpgMakerLocationStep::DecodeJsonString) => {
                let LosslessJsonValue::String(source) = &value else {
                    return Err(RpgMakerDocumentError::ExpectedEncodedJsonString);
                };
                ResolvedValue::Owned(
                    decode_json(source).map_err(RpgMakerDocumentError::InvalidNestedJson)?,
                )
            }
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

fn decode_plugins_envelope(text: &str) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
    let envelope = parse_plugins_envelope(text)
        .map_err(|failure| RpgMakerDocumentError::InvalidPluginsEnvelope { failure })?;
    let value = decode_json(envelope.json()).map_err(RpgMakerDocumentError::InvalidJson)?;
    validate_plugins_root_is_array(matches!(value, LosslessJsonValue::Array(_)))
        .map_err(|failure| RpgMakerDocumentError::InvalidPluginsEnvelope { failure })?;
    Ok(value)
}

fn select_plugin_parameter(
    plugins: &LosslessJsonValue,
    plugin_index: usize,
    plugin_name: &str,
    parameter_name: &str,
) -> Result<LosslessJsonValue, RpgMakerDocumentError> {
    let LosslessJsonValue::Array(plugins) = plugins else {
        return Err(RpgMakerDocumentError::InvalidPluginsEnvelope {
            failure: PluginsEnvelopeFailure::RootType,
        });
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

/// RPG Maker 文档来源、结构路径或目标值无效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerDocumentError {
    UnknownStandardDataFile,
    InvalidDataFileName(DataFileNameError),
    InvalidMapId,
    InvalidPluginParameterSource,
    InvalidUtf8,
    InvalidJson(LosslessJsonError),
    InvalidNestedJson(LosslessJsonError),
    InvalidPluginsEnvelope { failure: PluginsEnvelopeFailure },
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
}

impl RpgMakerDocumentError {
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::UnknownStandardDataFile | Self::InvalidDataFileName(_) | Self::InvalidMapId => {
                "invalid_source"
            }
            Self::InvalidPluginParameterSource
            | Self::PluginIndexMissing
            | Self::PluginNameMissing
            | Self::PluginNameMismatch
            | Self::PluginParametersMissing
            | Self::PluginParameterMissing => "invalid_plugin_parameter_source",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidJson(_) | Self::InvalidNestedJson(_) => "invalid_json",
            Self::InvalidPluginsEnvelope { .. } => "invalid_plugins_envelope",
            Self::ExpectedObject
            | Self::ExpectedArray
            | Self::MissingObjectKey
            | Self::MissingArrayIndex
            | Self::ExpectedEncodedJsonString
            | Self::ExpectedString => "invalid_location",
        }
    }
}

impl fmt::Display for RpgMakerDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStandardDataFile => formatter.write_str("不是标准 RPG Maker Data 文件名"),
            Self::InvalidDataFileName(error) => error.fmt(formatter),
            Self::InvalidMapId => formatter.write_str("RPG Maker map ID 必须是正 u32 整数"),
            Self::InvalidPluginParameterSource => {
                formatter.write_str("RPG Maker 插件参数来源的索引或名称无效")
            }
            Self::InvalidUtf8 => formatter.write_str("RPG Maker 文档不是有效 UTF-8"),
            Self::InvalidJson(error) => write!(formatter, "RPG Maker 文档不是有效 JSON：{error}"),
            Self::InvalidNestedJson(error) => {
                write!(formatter, "RPG Maker JSON 字符串不是有效 JSON：{error}")
            }
            Self::InvalidPluginsEnvelope { failure } => write!(
                formatter,
                "文档不是 RPG Maker 生成的 plugins.js 格式：{failure}"
            ),
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
        }
    }
}

impl Error for RpgMakerDocumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDataFileName(error) => Some(error),
            Self::InvalidJson(error) | Self::InvalidNestedJson(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_zero_based_paths_nested_json_and_text_identity() {
        let document = OpenedRpgMakerDocument::open(
            RpgMakerSource::data(StandardDataFile::Items),
            r#"[null,{"note":"<Help:说明>","nested":"[{\"name\":\"值\"}]"}]"#.as_bytes(),
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
    }

    #[test]
    fn selects_plugin_parameter_by_index_name_and_parameter() {
        let source = RpgMakerSource::plugin_parameter(1, "Quest", "Entries");
        let document = OpenedRpgMakerDocument::open(
            source,
            r#"// generated
var $plugins = [{"name":"Other","parameters":{}},{"name":"Quest","parameters":{"Entries":"[{\"Title\":\"任务\"}]"}}];"#.as_bytes(),
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

    #[test]
    fn plugin_open_reports_the_shared_envelope_failure_fact() {
        let source = || RpgMakerSource::plugin_parameter(0, "Quest", "Entries");
        for (text, expected) in [
            ("const plugins = [];", PluginsEnvelopeFailure::Declaration),
            (
                "/* unsupported */\nvar $plugins = [];",
                PluginsEnvelopeFailure::Prefix,
            ),
            ("var $plugins [];", PluginsEnvelopeFailure::Assignment),
            ("var $plugins = []", PluginsEnvelopeFailure::Terminator),
            ("var $plugins = {};", PluginsEnvelopeFailure::RootType),
        ] {
            assert_eq!(
                OpenedRpgMakerDocument::open(source(), text.as_bytes()),
                Err(RpgMakerDocumentError::InvalidPluginsEnvelope { failure: expected })
            );
        }
    }

    #[test]
    fn opens_and_resolves_deep_documents() {
        const DEPTH: usize = 10_000;
        let mut source = "[".repeat(DEPTH);
        source.push_str(r#"{"text":"值"}"#);
        source.push_str(&"]".repeat(DEPTH));
        let document = OpenedRpgMakerDocument::open(
            RpgMakerSource::data(StandardDataFile::Items),
            source.as_bytes(),
        )
        .unwrap();
        let mut steps = vec![RpgMakerLocationStep::index(0); DEPTH];
        steps.push(RpgMakerLocationStep::key("text"));
        assert_eq!(document.text(&steps).unwrap().original(), "值");
    }

    #[test]
    fn data_file_source_reuses_standard_and_map_identity() {
        assert_eq!(
            data_file_source("Actors.json").unwrap(),
            RpgMakerSource::Data(StandardDataFile::Actors)
        );
        assert_eq!(
            data_file_source("Map001.json").unwrap(),
            RpgMakerSource::Map(MapId::new(1).unwrap())
        );
        for file_name in ["Map000.json", "Map01.json", "Map0001.json"] {
            assert!(matches!(
                data_file_source(file_name).unwrap(),
                RpgMakerSource::DataFile(file) if file.as_str() == file_name
            ));
        }
        assert!(data_file_source("../Actors.json").is_err());
    }

    #[test]
    fn map_source_accepts_only_positive_u32_ids() {
        assert_eq!(
            map_source(1).unwrap(),
            RpgMakerSource::Map(MapId::new(1).unwrap())
        );
        for invalid in [-1, 0, i64::from(u32::MAX) + 1] {
            assert_eq!(
                map_source(invalid),
                Err(RpgMakerDocumentError::InvalidMapId)
            );
        }
    }
}
