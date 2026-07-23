//! RPG Maker 结构化位置的持久化编码。
//!
//! 数据库键使用紧凑、规范且可逆的 JSON，而不使用面向人类的 `Display` 文本。
//! 位置和修改资源是高基数索引键，编码必须避免在每个 B-tree 中重复字段名；
//! 读取、写入和日志 wire 仍共享同一份结构化位置语义。

use std::error::Error;
use std::fmt;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget, DirectTextPart,
    DirectTextRecipe, MutationClaim, MutationResource, ProjectionModelError, ScalarFieldKey,
    TextProjectionRecipe, TextUnitRole,
};
use crate::rpg_maker::text::{DataFileName, MapId};

/// 在数据库中无损保存 `RpgMakerLocation` 的规范编解码器。
pub(crate) struct RpgMakerLocationCodec;

impl RpgMakerLocationCodec {
    /// 把结构化位置编码为确定的紧凑单行 JSON。
    pub(crate) fn encode(
        location: &RpgMakerLocation,
    ) -> Result<String, RpgMakerLocationCodecError> {
        serde_json::to_string(&LocationRef::from(location))
            .map_err(RpgMakerLocationCodecError::Encode)
    }

    /// 把数据库中的权威位置解码回结构化类型。
    pub(crate) fn decode(value: &str) -> Result<RpgMakerLocation, RpgMakerLocationCodecError> {
        let stored = serde_json::from_str::<StoredLocation>(value)
            .map_err(RpgMakerLocationCodecError::Decode)?;
        let location = stored.try_into()?;
        let canonical = Self::encode(&location)?;
        if canonical != value {
            return Err(RpgMakerLocationCodecError::NonCanonical);
        }
        Ok(location)
    }
}

/// 逻辑文本身份、强角色、物理修改资源与投影配方的内部紧凑规范 JSON 编解码器。
pub(crate) struct RpgMakerProjectionCodec;

impl RpgMakerProjectionCodec {
    pub(crate) fn encode_role(role: &TextUnitRole) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&StoredRole::from(role)).map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_role(value: &str) -> Result<TextUnitRole, RpgMakerProjectionCodecError> {
        let role = serde_json::from_str::<StoredRole>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .try_into()?;
        let canonical = Self::encode_role(&role)?;
        if canonical != value {
            return Err(RpgMakerProjectionCodecError::NonCanonical);
        }
        Ok(role)
    }

    pub(crate) fn encode_mutation_resource(
        resource: &MutationResource,
    ) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&LocationRef::from(resource))
            .map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_mutation_resource(
        value: &str,
    ) -> Result<MutationResource, RpgMakerProjectionCodecError> {
        let resource = serde_json::from_str::<StoredLocation>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .try_into()?;
        let canonical = Self::encode_mutation_resource(&resource)?;
        if canonical != value {
            return Err(RpgMakerProjectionCodecError::NonCanonical);
        }
        Ok(resource)
    }

    pub(crate) fn encode_recipes(
        recipes: &[TextProjectionRecipe],
    ) -> Result<String, RpgMakerProjectionCodecError> {
        let stored = recipes.iter().map(StoredRecipe::from).collect::<Vec<_>>();
        serde_json::to_string(&stored).map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_recipes(
        value: &str,
    ) -> Result<Vec<TextProjectionRecipe>, RpgMakerProjectionCodecError> {
        let recipes = serde_json::from_str::<Vec<StoredRecipe>>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()?;
        let canonical = Self::encode_recipes(&recipes)?;
        if canonical != value {
            return Err(RpgMakerProjectionCodecError::NonCanonical);
        }
        Ok(recipes)
    }
}

/// RPG Maker 位置编解码失败。
#[derive(Debug)]
pub(crate) enum RpgMakerLocationCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    NonCanonical,
    InvalidDataFile(String),
    InvalidMapId(u32),
}

impl fmt::Display for RpgMakerLocationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 RPG Maker 位置：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 RPG Maker 位置：{source}"),
            Self::NonCanonical => write!(formatter, "RPG Maker 位置不是规范紧凑 JSON"),
            Self::InvalidDataFile(file_name) => {
                write!(formatter, "RPG Maker 位置引用了无效 data 文件：{file_name}")
            }
            Self::InvalidMapId(map_id) => {
                write!(formatter, "RPG Maker 位置引用了无效 map ID：{map_id}")
            }
        }
    }
}

impl Error for RpgMakerLocationCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) | Self::Decode(source) => Some(source),
            Self::NonCanonical | Self::InvalidDataFile(_) | Self::InvalidMapId(_) => None,
        }
    }
}

impl RpgMakerLocationCodecError {
    /// 只投影闭集编解码原因、JSON 坐标和规范 ID，不公开原始 JSON 或文件名正文。
    pub(crate) fn safe_diagnostic_detail(&self) -> String {
        match self {
            Self::Encode(source) => {
                format!(
                    "codec=location; operation=encode; {}",
                    codec_json_error_detail(source)
                )
            }
            Self::Decode(source) => {
                format!(
                    "codec=location; operation=decode; {}",
                    codec_json_error_detail(source)
                )
            }
            Self::NonCanonical => "codec=location; kind=non_canonical".to_owned(),
            Self::InvalidDataFile(_) => "codec=location; kind=invalid_data_file".to_owned(),
            Self::InvalidMapId(map_id) => {
                format!("codec=location; kind=invalid_map_id; map_id={map_id}")
            }
        }
    }
}

fn codec_json_error_detail(source: &serde_json::Error) -> String {
    let category = match source.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    format!(
        "json_category={category}; json_line={}; json_column={}",
        source.line(),
        source.column()
    )
}

enum StoredLocation {
    Value {
        source: StoredSource,
        steps: Vec<StoredStep>,
    },
    NoteTag {
        source: StoredSource,
        container_steps: Vec<StoredStep>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: StoredSource,
        command_steps: Vec<StoredStep>,
        tag_name: String,
        occurrence: usize,
    },
}

impl StoredLocation {
    fn from_compact_value(value: Value) -> Result<Self, String> {
        let fields = expect_array(value, "位置")?;
        let tag = fields
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| "位置缺少字符串种类标记".to_owned())?
            .to_owned();
        match tag.as_str() {
            "v" => {
                let [_, source, steps] = exact_fields(fields, "值位置")?;
                Ok(Self::Value {
                    source: parse_source(source)?,
                    steps: parse_steps(steps)?,
                })
            }
            "n" => {
                let [_, source, steps, tag_name, occurrence] =
                    exact_fields(fields, "备注标签位置")?;
                Ok(Self::NoteTag {
                    source: parse_source(source)?,
                    container_steps: parse_steps(steps)?,
                    tag_name: expect_string(tag_name, "备注标签名")?,
                    occurrence: expect_usize(occurrence, "备注标签序号")?,
                })
            }
            "c" => {
                let [_, source, steps, tag_name, occurrence] =
                    exact_fields(fields, "注释标签位置")?;
                Ok(Self::CommentTag {
                    source: parse_source(source)?,
                    command_steps: parse_steps(steps)?,
                    tag_name: expect_string(tag_name, "注释标签名")?,
                    occurrence: expect_usize(occurrence, "注释标签序号")?,
                })
            }
            _ => Err(format!("位置包含未知种类标记：{tag}")),
        }
    }
}

impl Serialize for StoredLocation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Value { source, steps } => {
                let mut sequence = serializer.serialize_seq(Some(3))?;
                sequence.serialize_element("v")?;
                sequence.serialize_element(source)?;
                sequence.serialize_element(steps)?;
                sequence.end()
            }
            Self::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => {
                let mut sequence = serializer.serialize_seq(Some(5))?;
                sequence.serialize_element("n")?;
                sequence.serialize_element(source)?;
                sequence.serialize_element(container_steps)?;
                sequence.serialize_element(tag_name)?;
                sequence.serialize_element(occurrence)?;
                sequence.end()
            }
            Self::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => {
                let mut sequence = serializer.serialize_seq(Some(5))?;
                sequence.serialize_element("c")?;
                sequence.serialize_element(source)?;
                sequence.serialize_element(command_steps)?;
                sequence.serialize_element(tag_name)?;
                sequence.serialize_element(occurrence)?;
                sequence.end()
            }
        }
    }
}

enum LocationRef<'a> {
    Value {
        source: &'a RpgMakerSource,
        steps: &'a [RpgMakerLocationStep],
    },
    NoteTag {
        source: &'a RpgMakerSource,
        container_steps: &'a [RpgMakerLocationStep],
        tag_name: &'a str,
        occurrence: usize,
    },
    CommentTag {
        source: &'a RpgMakerSource,
        command_steps: &'a [RpgMakerLocationStep],
        tag_name: &'a str,
        occurrence: usize,
    },
}

impl<'a> From<&'a RpgMakerLocation> for LocationRef<'a> {
    fn from(location: &'a RpgMakerLocation) -> Self {
        match location {
            RpgMakerLocation::Value { source, steps } => Self::Value { source, steps },
            RpgMakerLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence: *occurrence,
            },
            RpgMakerLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence: *occurrence,
            },
        }
    }
}

impl<'a> From<&'a MutationResource> for LocationRef<'a> {
    fn from(resource: &'a MutationResource) -> Self {
        match resource {
            MutationResource::Value { source, steps } => Self::Value { source, steps },
            MutationResource::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence: *occurrence,
            },
            MutationResource::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence: *occurrence,
            },
        }
    }
}

impl Serialize for LocationRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Value { source, steps } => {
                let mut sequence = serializer.serialize_seq(Some(3))?;
                sequence.serialize_element("v")?;
                sequence.serialize_element(&SourceRef(source))?;
                sequence.serialize_element(&StepsRef(steps))?;
                sequence.end()
            }
            Self::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => {
                let mut sequence = serializer.serialize_seq(Some(5))?;
                sequence.serialize_element("n")?;
                sequence.serialize_element(&SourceRef(source))?;
                sequence.serialize_element(&StepsRef(container_steps))?;
                sequence.serialize_element(tag_name)?;
                sequence.serialize_element(occurrence)?;
                sequence.end()
            }
            Self::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => {
                let mut sequence = serializer.serialize_seq(Some(5))?;
                sequence.serialize_element("c")?;
                sequence.serialize_element(&SourceRef(source))?;
                sequence.serialize_element(&StepsRef(command_steps))?;
                sequence.serialize_element(tag_name)?;
                sequence.serialize_element(occurrence)?;
                sequence.end()
            }
        }
    }
}

struct SourceRef<'a>(&'a RpgMakerSource);

impl Serialize for SourceRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            RpgMakerSource::Data(file) => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element("d")?;
                sequence.serialize_element(file.file_name())?;
                sequence.end()
            }
            RpgMakerSource::DataFile(file) => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element("d")?;
                sequence.serialize_element(file.as_str())?;
                sequence.end()
            }
            RpgMakerSource::Map(map_id) => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element("m")?;
                sequence.serialize_element(&map_id.get())?;
                sequence.end()
            }
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => {
                let mut sequence = serializer.serialize_seq(Some(4))?;
                sequence.serialize_element("p")?;
                sequence.serialize_element(plugin_index)?;
                sequence.serialize_element(plugin_name)?;
                sequence.serialize_element(parameter_name)?;
                sequence.end()
            }
        }
    }
}

struct StepsRef<'a>(&'a [RpgMakerLocationStep]);

impl Serialize for StepsRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for step in self.0 {
            sequence.serialize_element(&StepRef(step))?;
        }
        sequence.end()
    }
}

struct StepRef<'a>(&'a RpgMakerLocationStep);

impl Serialize for StepRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            RpgMakerLocationStep::ObjectKey(key) => serializer.serialize_str(key),
            RpgMakerLocationStep::ArrayIndex(index) => index.serialize(serializer),
            RpgMakerLocationStep::DecodeJsonString => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for StoredLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_compact_value(Value::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

fn exact_fields<const N: usize>(fields: Vec<Value>, subject: &str) -> Result<[Value; N], String> {
    let actual = fields.len();
    fields
        .try_into()
        .map_err(|_| format!("{subject}字段数无效：期待 {N}，实际 {actual}"))
}

fn expect_array(value: Value, subject: &str) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(values) => Ok(values),
        _ => Err(format!("{subject}必须是数组")),
    }
}

fn expect_string(value: Value, subject: &str) -> Result<String, String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(format!("{subject}必须是字符串")),
    }
}

fn expect_usize(value: Value, subject: &str) -> Result<usize, String> {
    let value = match value {
        Value::Number(value) => value
            .as_u64()
            .ok_or_else(|| format!("{subject}必须是非负整数"))?,
        _ => return Err(format!("{subject}必须是非负整数")),
    };
    usize::try_from(value).map_err(|_| format!("{subject}超出平台索引范围"))
}

fn parse_source(value: Value) -> Result<StoredSource, String> {
    let fields = expect_array(value, "位置来源")?;
    let tag = fields
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| "位置来源缺少字符串种类标记".to_owned())?
        .to_owned();
    match tag.as_str() {
        "d" => {
            let [_, file] = exact_fields(fields, "data 文件来源")?;
            Ok(StoredSource::Data {
                file: expect_string(file, "data 文件名")?,
            })
        }
        "m" => {
            let [_, map_id] = exact_fields(fields, "地图来源")?;
            let map_id = expect_usize(map_id, "地图 ID")?;
            Ok(StoredSource::Map {
                map_id: u32::try_from(map_id).map_err(|_| "地图 ID 超出 u32 范围".to_owned())?,
            })
        }
        "p" => {
            let [_, plugin_index, plugin_name, parameter_name] =
                exact_fields(fields, "插件参数来源")?;
            Ok(StoredSource::PluginParameter {
                plugin_index: expect_usize(plugin_index, "插件序号")?,
                plugin_name: expect_string(plugin_name, "插件名")?,
                parameter_name: expect_string(parameter_name, "参数名")?,
            })
        }
        _ => Err(format!("位置来源包含未知种类标记：{tag}")),
    }
}

fn parse_steps(value: Value) -> Result<Vec<StoredStep>, String> {
    expect_array(value, "位置步骤")?
        .into_iter()
        .map(|value| match value {
            Value::String(key) => Ok(StoredStep::ObjectKey { key }),
            Value::Number(index) => index
                .as_u64()
                .ok_or_else(|| "数组下标必须是非负整数".to_owned())
                .and_then(|index| {
                    usize::try_from(index).map_err(|_| "数组下标超出平台索引范围".to_owned())
                })
                .map(|index| StoredStep::ArrayIndex { index }),
            Value::Null => Ok(StoredStep::DecodeJsonString),
            _ => Err("位置步骤只能是字符串键、非负数组下标或 null 解码标记".to_owned()),
        })
        .collect()
}

impl From<&RpgMakerLocation> for StoredLocation {
    fn from(location: &RpgMakerLocation) -> Self {
        match location {
            RpgMakerLocation::Value { source, steps } => Self::Value {
                source: source.into(),
                steps: steps.iter().map(Into::into).collect(),
            },
            RpgMakerLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source: source.into(),
                container_steps: container_steps.iter().map(Into::into).collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
            RpgMakerLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source: source.into(),
                command_steps: command_steps.iter().map(Into::into).collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
        }
    }
}

impl TryFrom<StoredLocation> for RpgMakerLocation {
    type Error = RpgMakerLocationCodecError;

    fn try_from(location: StoredLocation) -> Result<Self, Self::Error> {
        match location {
            StoredLocation::Value { source, steps } => Ok(Self::value(
                source.try_into()?,
                steps.into_iter().map(Into::into).collect(),
            )),
            StoredLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Ok(Self::note_tag(
                source.try_into()?,
                container_steps.into_iter().map(Into::into).collect(),
                tag_name,
                occurrence,
            )),
            StoredLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Ok(Self::comment_tag(
                source.try_into()?,
                command_steps.into_iter().map(Into::into).collect(),
                tag_name,
                occurrence,
            )),
        }
    }
}

enum StoredSource {
    Data {
        file: String,
    },
    Map {
        map_id: u32,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl Serialize for StoredSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Data { file } => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element("d")?;
                sequence.serialize_element(file)?;
                sequence.end()
            }
            Self::Map { map_id } => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element("m")?;
                sequence.serialize_element(map_id)?;
                sequence.end()
            }
            Self::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => {
                let mut sequence = serializer.serialize_seq(Some(4))?;
                sequence.serialize_element("p")?;
                sequence.serialize_element(plugin_index)?;
                sequence.serialize_element(plugin_name)?;
                sequence.serialize_element(parameter_name)?;
                sequence.end()
            }
        }
    }
}

impl From<&RpgMakerSource> for StoredSource {
    fn from(source: &RpgMakerSource) -> Self {
        match source {
            RpgMakerSource::Data(file) => Self::Data {
                file: file.file_name().to_owned(),
            },
            RpgMakerSource::DataFile(file) => Self::Data {
                file: file.as_str().to_owned(),
            },
            RpgMakerSource::Map(map_id) => Self::Map {
                map_id: map_id.get(),
            },
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Self::PluginParameter {
                plugin_index: *plugin_index,
                plugin_name: plugin_name.clone(),
                parameter_name: parameter_name.clone(),
            },
        }
    }
}

impl TryFrom<StoredSource> for RpgMakerSource {
    type Error = RpgMakerLocationCodecError;

    fn try_from(source: StoredSource) -> Result<Self, Self::Error> {
        match source {
            StoredSource::Data { file } => DataFileName::parse(file.clone())
                .map(Self::data_file)
                .map_err(|_| RpgMakerLocationCodecError::InvalidDataFile(file)),
            StoredSource::Map { map_id } => MapId::new(map_id)
                .map(Self::map_id)
                .map_err(|_| RpgMakerLocationCodecError::InvalidMapId(map_id)),
            StoredSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Ok(Self::plugin_parameter(
                plugin_index,
                plugin_name,
                parameter_name,
            )),
        }
    }
}

enum StoredStep {
    ObjectKey { key: String },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl Serialize for StoredStep {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::ObjectKey { key } => serializer.serialize_str(key),
            Self::ArrayIndex { index } => index.serialize(serializer),
            Self::DecodeJsonString => serializer.serialize_none(),
        }
    }
}

impl From<&RpgMakerLocationStep> for StoredStep {
    fn from(step: &RpgMakerLocationStep) -> Self {
        match step {
            RpgMakerLocationStep::ObjectKey(key) => Self::ObjectKey { key: key.clone() },
            RpgMakerLocationStep::ArrayIndex(index) => Self::ArrayIndex { index: *index },
            RpgMakerLocationStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

impl From<StoredStep> for RpgMakerLocationStep {
    fn from(step: StoredStep) -> Self {
        match step {
            StoredStep::ObjectKey { key } => Self::key(key),
            StoredStep::ArrayIndex { index } => Self::index(index),
            StoredStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

#[derive(Deserialize, Serialize)]
enum StoredRole {
    #[serde(rename = "f")]
    Scalar(String),
    #[serde(rename = "p")]
    DialogueSpeaker,
    #[serde(rename = "b")]
    DialogueBody,
    #[serde(rename = "c")]
    Choices,
    #[serde(rename = "r")]
    ScrollingText,
}

impl From<&TextUnitRole> for StoredRole {
    fn from(role: &TextUnitRole) -> Self {
        match role {
            TextUnitRole::Scalar(key) => Self::Scalar(key.as_str().to_owned()),
            TextUnitRole::DialogueSpeaker => Self::DialogueSpeaker,
            TextUnitRole::DialogueBody => Self::DialogueBody,
            TextUnitRole::Choices => Self::Choices,
            TextUnitRole::ScrollingText => Self::ScrollingText,
        }
    }
}

impl TryFrom<StoredRole> for TextUnitRole {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(role: StoredRole) -> Result<Self, Self::Error> {
        match role {
            StoredRole::Scalar(key) => ScalarFieldKey::new(key)
                .map(Self::Scalar)
                .map_err(RpgMakerProjectionCodecError::Projection),
            StoredRole::DialogueSpeaker => Ok(Self::DialogueSpeaker),
            StoredRole::DialogueBody => Ok(Self::DialogueBody),
            StoredRole::Choices => Ok(Self::Choices),
            StoredRole::ScrollingText => Ok(Self::ScrollingText),
        }
    }
}

impl From<&MutationResource> for StoredLocation {
    fn from(resource: &MutationResource) -> Self {
        match resource {
            MutationResource::Value { source, steps } => Self::Value {
                source: source.into(),
                steps: steps.iter().map(Into::into).collect(),
            },
            MutationResource::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source: source.into(),
                container_steps: container_steps.iter().map(Into::into).collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
            MutationResource::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source: source.into(),
                command_steps: command_steps.iter().map(Into::into).collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
        }
    }
}

impl TryFrom<StoredLocation> for MutationResource {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(resource: StoredLocation) -> Result<Self, Self::Error> {
        match resource {
            StoredLocation::Value { source, steps } => Ok(Self::Value {
                source: source
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
                steps: steps.into_iter().map(Into::into).collect(),
            }),
            StoredLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Ok(Self::NoteTag {
                source: source
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
                container_steps: container_steps.into_iter().map(Into::into).collect(),
                tag_name,
                occurrence,
            }),
            StoredLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Ok(Self::CommentTag {
                source: source
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
                command_steps: command_steps.into_iter().map(Into::into).collect(),
                tag_name,
                occurrence,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
enum StoredRecipe {
    #[serde(rename = "d")]
    Direct(
        StoredLocation,
        StoredMutationClaim,
        String,
        Vec<StoredDirectTextPart>,
    ),
    #[serde(rename = "l")]
    Dialogue(
        StoredLocation,
        Option<StoredDirectSpeakerTarget>,
        Vec<StoredDialogueLineRecipe>,
    ),
    #[serde(rename = "c")]
    Claim(StoredMutationClaim),
}

impl From<&TextProjectionRecipe> for StoredRecipe {
    fn from(recipe: &TextProjectionRecipe) -> Self {
        match recipe {
            TextProjectionRecipe::Direct(recipe) => Self::Direct(
                StoredLocation::from(recipe.target()),
                StoredMutationClaim::from(recipe.mutation_claim()),
                recipe.expected_raw().to_owned(),
                recipe.parts().iter().map(Into::into).collect(),
            ),
            TextProjectionRecipe::Dialogue(recipe) => Self::Dialogue(
                StoredLocation::from(recipe.group_location()),
                recipe.direct_speaker().map(Into::into),
                recipe.lines().iter().map(Into::into).collect(),
            ),
            TextProjectionRecipe::Claim(claim) => Self::Claim(StoredMutationClaim::from(claim)),
        }
    }
}

impl TryFrom<StoredRecipe> for TextProjectionRecipe {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(recipe: StoredRecipe) -> Result<Self, Self::Error> {
        match recipe {
            StoredRecipe::Direct(target, mutation_claim, expected_raw, parts) => {
                DirectTextRecipe::new_with_claim(
                    target
                        .try_into()
                        .map_err(RpgMakerProjectionCodecError::Location)?,
                    mutation_claim.try_into()?,
                    expected_raw,
                    parts
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .map(TextProjectionRecipe::Direct)
                .map_err(RpgMakerProjectionCodecError::Projection)
            }
            StoredRecipe::Dialogue(group_location, direct_speaker, lines) => {
                DialogueWriteRecipe::new(
                    group_location
                        .try_into()
                        .map_err(RpgMakerProjectionCodecError::Location)?,
                    direct_speaker.map(TryInto::try_into).transpose()?,
                    lines
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .map(TextProjectionRecipe::Dialogue)
                .map_err(RpgMakerProjectionCodecError::Projection)
            }
            StoredRecipe::Claim(mutation_claim) => {
                mutation_claim.try_into().map(TextProjectionRecipe::Claim)
            }
        }
    }
}

#[derive(Deserialize, Serialize)]
enum StoredMutationClaim {
    #[serde(rename = "v")]
    Value(StoredLocation),
    #[serde(rename = "n")]
    NoteTag(StoredLocation),
    #[serde(rename = "c")]
    CommentTag(StoredLocation, Vec<StoredLocation>),
    #[serde(rename = "e")]
    EventBlock(StoredLocation, Vec<StoredLocation>),
}

impl From<&MutationClaim> for StoredMutationClaim {
    fn from(claim: &MutationClaim) -> Self {
        match claim {
            MutationClaim::Value(location) => Self::Value(location.into()),
            MutationClaim::NoteTag(location) => Self::NoteTag(location.into()),
            MutationClaim::CommentTag {
                location,
                backing_values,
            } => Self::CommentTag(
                location.into(),
                backing_values.iter().map(Into::into).collect(),
            ),
            MutationClaim::EventBlock {
                header,
                covered_values,
            } => Self::EventBlock(
                header.into(),
                covered_values.iter().map(Into::into).collect(),
            ),
        }
    }
}

impl TryFrom<StoredMutationClaim> for MutationClaim {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(claim: StoredMutationClaim) -> Result<Self, Self::Error> {
        let decode = |location: StoredLocation| {
            location
                .try_into()
                .map_err(RpgMakerProjectionCodecError::Location)
        };
        match claim {
            StoredMutationClaim::Value(location) => {
                let location = decode(location)?;
                if !matches!(location, RpgMakerLocation::Value { .. }) {
                    return Err(RpgMakerProjectionCodecError::MutationClaimKindMismatch {
                        expected: "value",
                        actual: location_kind(&location),
                    });
                }
                Ok(MutationClaim::Value(location))
            }
            StoredMutationClaim::NoteTag(location) => {
                let location = decode(location)?;
                if !matches!(location, RpgMakerLocation::NoteTag { .. }) {
                    return Err(RpgMakerProjectionCodecError::MutationClaimKindMismatch {
                        expected: "note_tag",
                        actual: location_kind(&location),
                    });
                }
                Ok(MutationClaim::NoteTag(location))
            }
            StoredMutationClaim::CommentTag(location, backing_values) => {
                MutationClaim::comment_tag(
                    decode(location)?,
                    backing_values
                        .into_iter()
                        .map(decode)
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .map_err(RpgMakerProjectionCodecError::Projection)
            }
            StoredMutationClaim::EventBlock(header, covered_values) => MutationClaim::event_block(
                decode(header)?,
                covered_values
                    .into_iter()
                    .map(decode)
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .map_err(RpgMakerProjectionCodecError::Projection),
        }
    }
}

#[derive(Deserialize, Serialize)]
enum StoredDirectTextPart {
    #[serde(rename = "l")]
    Literal(String),
    #[serde(rename = "t")]
    TextSlot(StoredRole),
    #[serde(rename = "s")]
    LineSlot(StoredRole, usize),
}

impl From<&DirectTextPart> for StoredDirectTextPart {
    fn from(part: &DirectTextPart) -> Self {
        match part {
            DirectTextPart::Literal(value) => Self::Literal(value.clone()),
            DirectTextPart::TextSlot { role } => Self::TextSlot(StoredRole::from(role)),
            DirectTextPart::LineSlot {
                role,
                source_line_index,
            } => Self::LineSlot(StoredRole::from(role), *source_line_index),
        }
    }
}

impl TryFrom<StoredDirectTextPart> for DirectTextPart {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(part: StoredDirectTextPart) -> Result<Self, Self::Error> {
        match part {
            StoredDirectTextPart::Literal(value) => Ok(Self::Literal(value)),
            StoredDirectTextPart::TextSlot(role) => Ok(Self::TextSlot {
                role: role.try_into()?,
            }),
            StoredDirectTextPart::LineSlot(role, source_line_index) => Ok(Self::LineSlot {
                role: role.try_into()?,
                source_line_index,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredDirectSpeakerTarget(StoredLocation, String);

impl From<&DirectSpeakerTarget> for StoredDirectSpeakerTarget {
    fn from(target: &DirectSpeakerTarget) -> Self {
        Self(
            StoredLocation::from(target.physical_location()),
            target.expected_raw().to_owned(),
        )
    }
}

impl TryFrom<StoredDirectSpeakerTarget> for DirectSpeakerTarget {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(target: StoredDirectSpeakerTarget) -> Result<Self, Self::Error> {
        Ok(Self::new(
            target
                .0
                .try_into()
                .map_err(RpgMakerProjectionCodecError::Location)?,
            target.1,
        ))
    }
}

#[derive(Deserialize, Serialize)]
struct StoredDialogueLineRecipe(StoredLocation, String, Vec<StoredDialogueLinePart>);

impl From<&DialogueLineRecipe> for StoredDialogueLineRecipe {
    fn from(line: &DialogueLineRecipe) -> Self {
        Self(
            StoredLocation::from(line.physical_location()),
            line.expected_raw().to_owned(),
            line.parts().iter().map(Into::into).collect(),
        )
    }
}

impl TryFrom<StoredDialogueLineRecipe> for DialogueLineRecipe {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(line: StoredDialogueLineRecipe) -> Result<Self, Self::Error> {
        DialogueLineRecipe::new(
            line.0
                .try_into()
                .map_err(RpgMakerProjectionCodecError::Location)?,
            line.1,
            line.2
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(RpgMakerProjectionCodecError::Projection)
    }
}

#[derive(Deserialize, Serialize)]
enum StoredDialogueLinePart {
    #[serde(rename = "l")]
    Literal(String),
    #[serde(rename = "s")]
    SpeakerSlot,
    #[serde(rename = "b")]
    BodyLine(usize),
}

impl From<&DialogueLinePart> for StoredDialogueLinePart {
    fn from(part: &DialogueLinePart) -> Self {
        match part {
            DialogueLinePart::Literal(value) => Self::Literal(value.clone()),
            DialogueLinePart::SpeakerSlot => Self::SpeakerSlot,
            DialogueLinePart::BodyLine { source_line_index } => Self::BodyLine(*source_line_index),
        }
    }
}

impl TryFrom<StoredDialogueLinePart> for DialogueLinePart {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(part: StoredDialogueLinePart) -> Result<Self, Self::Error> {
        match part {
            StoredDialogueLinePart::Literal(value) => Ok(Self::Literal(value)),
            StoredDialogueLinePart::SpeakerSlot => Ok(Self::SpeakerSlot),
            StoredDialogueLinePart::BodyLine(source_line_index) => {
                Ok(Self::BodyLine { source_line_index })
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerProjectionCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    NonCanonical,
    Location(RpgMakerLocationCodecError),
    Projection(ProjectionModelError),
    MutationClaimKindMismatch {
        expected: &'static str,
        actual: &'static str,
    },
}

fn location_kind(location: &RpgMakerLocation) -> &'static str {
    match location {
        RpgMakerLocation::Value { .. } => "value",
        RpgMakerLocation::NoteTag { .. } => "note_tag",
        RpgMakerLocation::CommentTag { .. } => "comment_tag",
    }
}

impl fmt::Display for RpgMakerProjectionCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 RPG Maker 文本投影：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 RPG Maker 文本投影：{source}"),
            Self::NonCanonical => write!(formatter, "RPG Maker 文本投影不是规范紧凑 JSON"),
            Self::Location(source) => write!(formatter, "文本投影位置无效：{source}"),
            Self::Projection(source) => write!(formatter, "文本投影配方无效：{source}"),
            Self::MutationClaimKindMismatch { expected, actual } => write!(
                formatter,
                "文本投影 Claim 种类与位置不一致：期待 {expected}，实际为 {actual}"
            ),
        }
    }
}

impl Error for RpgMakerProjectionCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) | Self::Decode(source) => Some(source),
            Self::NonCanonical => None,
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::MutationClaimKindMismatch { .. } => None,
        }
    }
}

impl RpgMakerProjectionCodecError {
    /// 只投影闭集投影模型、嵌套位置和 JSON 坐标，不公开原始 JSON 或文本槽正文。
    pub(crate) fn safe_diagnostic_detail(&self) -> String {
        match self {
            Self::Encode(source) => {
                format!(
                    "codec=projection; operation=encode; {}",
                    codec_json_error_detail(source)
                )
            }
            Self::Decode(source) => {
                format!(
                    "codec=projection; operation=decode; {}",
                    codec_json_error_detail(source)
                )
            }
            Self::NonCanonical => "codec=projection; kind=non_canonical".to_owned(),
            Self::Location(source) => format!(
                "codec=projection; kind=invalid_location; {}",
                source.safe_diagnostic_detail()
            ),
            Self::Projection(source) => format!(
                "codec=projection; kind=invalid_projection; {}",
                crate::rpg_maker::dialogue::projection_model_detail(source)
            ),
            Self::MutationClaimKindMismatch { expected, actual } => format!(
                "codec=projection; kind=mutation_claim_kind_mismatch; expected={expected}; actual={actual}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::text::StandardDataFile;

    #[test]
    fn value_location_has_deterministic_json_and_round_trips() {
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![
                RpgMakerLocationStep::index(10),
                RpgMakerLocationStep::key("name"),
            ],
        );

        let encoded = RpgMakerLocationCodec::encode(&location).expect("位置应该可编码");

        assert_eq!(encoded, r#"["v",["d","Items.json"],[10,"name"]]"#);
        assert_eq!(
            RpgMakerLocationCodec::decode(&encoded).expect("位置应该可解码"),
            location
        );
    }

    #[test]
    fn every_location_variant_and_decoding_boundary_round_trips() {
        let locations = [
            (
                RpgMakerLocation::note_tag(
                    RpgMakerSource::data(StandardDataFile::Items),
                    vec![RpgMakerLocationStep::index(2)],
                    "Category",
                    1,
                ),
                r#"["n",["d","Items.json"],[2],"Category",1]"#,
            ),
            (
                RpgMakerLocation::comment_tag(
                    RpgMakerSource::map(3),
                    vec![
                        RpgMakerLocationStep::key("events"),
                        RpgMakerLocationStep::index(1),
                        RpgMakerLocationStep::DecodeJsonString,
                    ],
                    "Quest",
                    0,
                ),
                r#"["c",["m",3],["events",1,null],"Quest",0]"#,
            ),
            (
                RpgMakerLocation::value(
                    RpgMakerSource::plugin_parameter(4, "QuestMenu", "Categories"),
                    vec![
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::index(2),
                        RpgMakerLocationStep::key("Name"),
                    ],
                ),
                r#"["v",["p",4,"QuestMenu","Categories"],[null,2,"Name"]]"#,
            ),
        ];

        for (location, expected) in locations {
            let encoded = RpgMakerLocationCodec::encode(&location).expect("位置应该可编码");
            assert_eq!(encoded, expected);
            assert_eq!(
                serde_json::to_string(&StoredLocation::from(&location))
                    .expect("嵌入配方的位置应该可编码"),
                expected
            );
            assert_eq!(
                RpgMakerLocationCodec::decode(&encoded).expect("位置应该可解码"),
                location
            );
        }
    }

    #[test]
    fn mutation_resource_uses_the_same_exact_compact_location_bytes() {
        let resource = MutationResource::CommentTag {
            source: RpgMakerSource::plugin_parameter(7, "QuestMenu", "Help"),
            command_steps: vec![
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(12),
                RpgMakerLocationStep::DecodeJsonString,
            ],
            tag_name: "Hint".to_owned(),
            occurrence: 2,
        };

        let encoded =
            RpgMakerProjectionCodec::encode_mutation_resource(&resource).expect("资源应该可编码");

        assert_eq!(
            encoded,
            r#"["c",["p",7,"QuestMenu","Help"],["list",12,null],"Hint",2]"#
        );
        assert_eq!(
            RpgMakerProjectionCodec::decode_mutation_resource(&encoded).expect("资源应该可解码"),
            resource
        );
    }

    #[test]
    fn long_location_path_round_trips_without_an_encoding_limit() {
        let steps = (0..16_384)
            .map(|index| match index % 3 {
                0 => RpgMakerLocationStep::key(format!("key-{index}")),
                1 => RpgMakerLocationStep::index(index),
                _ => RpgMakerLocationStep::DecodeJsonString,
            })
            .collect();
        let location =
            RpgMakerLocation::value(RpgMakerSource::data(StandardDataFile::Items), steps);

        let encoded = RpgMakerLocationCodec::encode(&location).expect("长路径应该可编码");

        assert!(encoded.starts_with(r#"["v",["d","Items.json"],["key-0",1,null,"key-3""#));
        assert!(encoded.ends_with(r#""key-16383"]]"#));
        assert_eq!(
            RpgMakerLocationCodec::decode(&encoded).expect("长路径应该可解码"),
            location
        );
    }

    #[test]
    fn every_persisted_decoder_rejects_whitespace_and_unicode_escape_aliases() {
        for value in [
            r#" ["v",["d","Items.json"],[]]"#,
            r#"["\u0076",["d","Items.json"],[]]"#,
        ] {
            assert!(matches!(
                RpgMakerLocationCodec::decode(value),
                Err(RpgMakerLocationCodecError::NonCanonical)
            ));
        }

        for value in [r#" "p""#, r#""\u0070""#] {
            assert!(matches!(
                RpgMakerProjectionCodec::decode_role(value),
                Err(RpgMakerProjectionCodecError::NonCanonical)
            ));
        }

        for value in [r#" ["v",["m",1],[3]]"#, r#"["\u0076",["m",1],[3]]"#] {
            assert!(matches!(
                RpgMakerProjectionCodec::decode_mutation_resource(value),
                Err(RpgMakerProjectionCodecError::NonCanonical)
            ));
        }

        for value in [
            r#" [{"c":{"v":["v",["m",1],[3]]}}]"#,
            r#"[{"\u0063":{"v":["v",["m",1],[3]]}}]"#,
        ] {
            assert!(matches!(
                RpgMakerProjectionCodec::decode_recipes(value),
                Err(RpgMakerProjectionCodecError::NonCanonical)
            ));
        }
    }

    #[test]
    fn non_canonical_errors_do_not_include_the_rejected_input() {
        let rejected = r#"["\u0076",["d","do-not-echo.json"],[]]"#;

        let error = RpgMakerLocationCodec::decode(rejected).expect_err("转义别名必须被拒绝");

        assert_eq!(error.to_string(), "RPG Maker 位置不是规范紧凑 JSON");
        assert!(!error.to_string().contains("do-not-echo"));
    }

    #[test]
    fn numeric_boundaries_round_trip_in_the_canonical_wire_format() {
        let maximum_map = RpgMakerLocation::value(RpgMakerSource::map(u32::MAX), Vec::new());
        let encoded_map =
            RpgMakerLocationCodec::encode(&maximum_map).expect("u32 最大地图 ID 应可编码");
        assert_eq!(encoded_map, r#"["v",["m",4294967295],[]]"#);
        assert_eq!(
            RpgMakerLocationCodec::decode(&encoded_map).expect("u32 最大地图 ID 应可解码"),
            maximum_map
        );

        let maximum_index = usize::MAX;
        let maximum_usize = RpgMakerLocation::note_tag(
            RpgMakerSource::plugin_parameter(maximum_index, "Plugin", "Parameter"),
            vec![RpgMakerLocationStep::index(maximum_index)],
            "Tag",
            maximum_index,
        );
        let encoded_usize =
            RpgMakerLocationCodec::encode(&maximum_usize).expect("平台 usize 上界应可编码");
        assert_eq!(
            encoded_usize,
            format!(
                r#"["n",["p",{maximum_index},"Plugin","Parameter"],[{maximum_index}],"Tag",{maximum_index}]"#
            )
        );
        assert_eq!(
            RpgMakerLocationCodec::decode(&encoded_usize).expect("平台 usize 上界应可解码"),
            maximum_usize
        );
    }

    #[test]
    fn stored_mutation_claim_kind_must_match_its_location_variant() {
        let note = RpgMakerLocation::note_tag(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
            "Help",
            0,
        );

        let error =
            MutationClaim::try_from(StoredMutationClaim::Value(StoredLocation::from(&note)))
                .expect_err("Value Claim 不得夹带 NoteTag 位置");

        assert!(matches!(
            error,
            RpgMakerProjectionCodecError::MutationClaimKindMismatch {
                expected: "value",
                actual: "note_tag"
            }
        ));
    }

    #[test]
    fn stored_event_block_claim_reuses_model_validation() {
        let header = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::CommonEvents),
            vec![RpgMakerLocationStep::index(1)],
        );
        let covered = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::CommonEvents),
            vec![RpgMakerLocationStep::index(2)],
        );
        let note = RpgMakerLocation::note_tag(
            RpgMakerSource::data(StandardDataFile::CommonEvents),
            vec![RpgMakerLocationStep::index(2)],
            "Tag",
            0,
        );
        let cross_source = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let stored = |coverage: Vec<&RpgMakerLocation>| {
            StoredMutationClaim::EventBlock(
                StoredLocation::from(&header),
                coverage.into_iter().map(StoredLocation::from).collect(),
            )
        };

        assert!(MutationClaim::try_from(stored(vec![&covered])).is_ok());
        assert!(matches!(
            MutationClaim::try_from(stored(Vec::new())),
            Err(RpgMakerProjectionCodecError::Projection(
                ProjectionModelError::EventBlockCoverageRequired
            ))
        ));
        for invalid in [&note, &cross_source] {
            assert!(matches!(
                MutationClaim::try_from(stored(vec![invalid])),
                Err(RpgMakerProjectionCodecError::Projection(
                    ProjectionModelError::InvalidEventBlockCoverage
                ))
            ));
        }
    }

    #[test]
    fn display_text_is_not_accepted_as_authoritative_storage() {
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![
                RpgMakerLocationStep::index(10),
                RpgMakerLocationStep::key("name"),
            ],
        );

        assert!(RpgMakerLocationCodec::decode(&location.to_string()).is_err());
    }

    #[test]
    fn accepts_safe_non_standard_data_file() {
        let encoded = r#"["v",["d","Custom.json"],[]]"#;

        let decoded = RpgMakerLocationCodec::decode(encoded).expect("非标准 JSON 基名应可持久化");
        assert_eq!(decoded.to_string(), "data/Custom.json");
    }

    #[test]
    fn persisted_map_identity_rejects_zero() {
        let encoded = r#"["v",["m",0],[]]"#;

        assert!(matches!(
            RpgMakerLocationCodec::decode(encoded),
            Err(RpgMakerLocationCodecError::InvalidMapId(0))
        ));
    }

    #[test]
    fn semantic_unit_roles_have_no_physical_line_identity() {
        let cases = [
            (TextUnitRole::DialogueSpeaker, r#""p""#),
            (TextUnitRole::DialogueBody, r#""b""#),
            (TextUnitRole::Choices, r#""c""#),
            (TextUnitRole::ScrollingText, r#""r""#),
        ];

        for (role, expected) in cases {
            let encoded = RpgMakerProjectionCodec::encode_role(&role).expect("角色应可编码");
            assert_eq!(encoded, expected);
            assert_eq!(
                RpgMakerProjectionCodec::decode_role(&encoded).expect("角色应可解码"),
                role
            );
        }
    }

    #[test]
    fn line_slots_round_trip_only_inside_projection_recipes() {
        let target =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(3)]);
        let recipe = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                target,
                "第二项",
                vec![DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index: 1,
                }],
            )
            .expect("行槽配方应合法"),
        );
        let encoded = RpgMakerProjectionCodec::encode_recipes(std::slice::from_ref(&recipe))
            .expect("配方应可编码");

        assert_eq!(
            encoded,
            r#"[{"d":[["v",["m",1],[3]],{"v":["v",["m",1],[3]]},"第二项",[{"s":["c",1]}]]}]"#
        );
        assert_eq!(
            RpgMakerProjectionCodec::decode_recipes(&encoded).expect("配方应可解码"),
            [recipe]
        );
    }
}
