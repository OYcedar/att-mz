//! RPG Maker 提取结果的语义范围与自然顺序。
//!
//! 顺序键只表达源数据中的物理位置。它不参与文本身份，也不读取译文状态、任务编号或
//! 翻译历史。SQLite 直接按规范 BLOB 的字典序排序，结果必须与 Rust 类型的 `Ord` 一致。

use std::error::Error;
use std::fmt;
use std::pin::Pin;

use serde_json::Value;

use crate::json::{StackSafeJsonValue, from_str as parse_json};

use crate::rpg_maker::model::TextUnitRole;
use crate::rpg_maker::text::{
    DataFileName, MapId, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
};

/// 允许若干有序 Group 共同进入一个 TaskBlock 的最大 RPG Maker 语义范围。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerSemanticScopeKey {
    StandardDatabase(StandardDataFile),
    DataFile(DataFileName),
    System,
    Map(MapId),
    CommonEvent(usize),
    Troop(usize),
    Plugin {
        plugin_index: usize,
        plugin_name: String,
    },
}

impl RpgMakerSemanticScopeKey {
    /// 从已经通过提取边界校验的 Group 位置确定语义范围。
    pub(crate) fn from_group_location(
        location: &RpgMakerLocation,
    ) -> Result<Self, RpgMakerSemanticScopeError> {
        match location.source() {
            RpgMakerSource::Data(StandardDataFile::System) => Ok(Self::System),
            RpgMakerSource::Data(StandardDataFile::CommonEvents) => first_array_index(location)
                .map(Self::CommonEvent)
                .ok_or_else(|| RpgMakerSemanticScopeError::MissingArrayIndex {
                    location: Box::new(location.clone()),
                }),
            RpgMakerSource::Data(StandardDataFile::Troops) => first_array_index(location)
                .map(Self::Troop)
                .ok_or_else(|| RpgMakerSemanticScopeError::MissingArrayIndex {
                    location: Box::new(location.clone()),
                }),
            RpgMakerSource::Data(file) => Ok(Self::StandardDatabase(*file)),
            RpgMakerSource::DataFile(file) => Ok(Self::DataFile(file.clone())),
            RpgMakerSource::Map(map_id) => Ok(Self::Map(*map_id)),
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                ..
            } => Ok(Self::Plugin {
                plugin_index: *plugin_index,
                plugin_name: plugin_name.clone(),
            }),
        }
    }
}

impl fmt::Display for RpgMakerSemanticScopeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StandardDatabase(file) => write!(formatter, "data/{}", file.file_name()),
            Self::DataFile(file) => write!(formatter, "data/{file}"),
            Self::System => formatter.write_str("data/System.json"),
            Self::Map(map_id) => write!(formatter, "Map{:03}", map_id.get()),
            Self::CommonEvent(event_id) => write!(formatter, "CommonEvent[{event_id}]"),
            Self::Troop(troop_id) => write!(formatter, "Troop[{troop_id}]"),
            Self::Plugin { plugin_name, .. } => write!(formatter, "Plugin[{plugin_name}]"),
        }
    }
}

fn first_array_index(location: &RpgMakerLocation) -> Option<usize> {
    location.steps().iter().find_map(|step| match step {
        RpgMakerLocationStep::ArrayIndex(index) => Some(*index),
        RpgMakerLocationStep::ObjectKey(_) | RpgMakerLocationStep::DecodeJsonString => None,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerSemanticScopeError {
    MissingArrayIndex { location: Box<RpgMakerLocation> },
}

impl RpgMakerSemanticScopeError {
    pub(crate) fn location(&self) -> &RpgMakerLocation {
        match self {
            Self::MissingArrayIndex { location } => location,
        }
    }
}

impl fmt::Display for RpgMakerSemanticScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingArrayIndex { location } => write!(
                formatter,
                "RPG Maker 语义范围要求 Group 位置包含数组下标：{location}"
            ),
        }
    }
}

impl Error for RpgMakerSemanticScopeError {}

/// 一个源数据物理节点及其节点内捕获槽的全序键。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RpgMakerSemanticOrderKey {
    physical_path: Vec<u64>,
    fragment: u64,
}

impl RpgMakerSemanticOrderKey {
    pub(crate) fn new(physical_path: Vec<u64>, fragment: u64) -> Self {
        Self {
            physical_path,
            fragment,
        }
    }

    /// 为封闭诊断保留顺序键的完整结构，不经过 Debug 文本或存储编码。
    pub(crate) fn diagnostic_parts(&self) -> (Vec<u64>, u64) {
        (self.physical_path.clone(), self.fragment)
    }

    #[cfg(test)]
    pub(crate) fn physical_path(&self) -> &[u64] {
        &self.physical_path
    }

    #[cfg(test)]
    pub(crate) const fn fragment(&self) -> u64 {
        self.fragment
    }

    /// 从提取器已经确定的结构化物理 Group 位置建立跨 owner 一致的顺序键。
    pub(crate) fn from_group_location(location: &RpgMakerLocation) -> Self {
        Self::new(physical_location_path(location), 0)
    }

    /// 从 Unit 的精确物理位置和节点内语义角色建立跨 owner 一致的顺序键。
    pub(crate) fn from_unit_location(location: &RpgMakerLocation, role: &TextUnitRole) -> Self {
        let mut physical_path = physical_location_path(location);
        let fragment = match role {
            TextUnitRole::Scalar(field) => {
                physical_path.push(0x20);
                push_string_segments(&mut physical_path, field.as_str());
                1
            }
            TextUnitRole::DialogueSpeaker => 2,
            TextUnitRole::DialogueBody => 3,
            TextUnitRole::Choices => 4,
            TextUnitRole::ScrollingText => 5,
        };
        Self::new(physical_path, fragment)
    }

    /// 按已解析 JSON 中的真实物理顺序建立顺序键。
    ///
    /// 对象步骤记录字段的插入序号，数组步骤记录数组下标。字段名只是定位
    /// 依据，不参与比较顺序。
    pub(crate) fn from_json_location(
        location: &RpgMakerLocation,
        root: &Value,
        fragment: u64,
    ) -> Result<Self, RpgMakerSemanticOrderProjectionError> {
        let physical_ordinals = json_physical_ordinals(root, location.steps())?;
        Self::from_physical_ordinals(location, &physical_ordinals, fragment, None)
    }

    /// 使用 Extract 遍历时已记录的对象/数组物理序号建立顺序键。
    ///
    /// Rules 匹配器已在遍历源树时保留这些序号，因此不必为了顺序再读一遍文档。
    /// 插件参数的序号位于参数内部路径之前，保证同一插件内按 `parameters`
    /// 对象的插入顺序排列。
    pub(crate) fn from_physical_ordinals(
        location: &RpgMakerLocation,
        physical_ordinals: &[usize],
        fragment: u64,
        plugin_parameter_order: Option<usize>,
    ) -> Result<Self, RpgMakerSemanticOrderProjectionError> {
        let mut path = physical_source_path(location.source(), plugin_parameter_order)?;
        let mut ordinals = physical_ordinals.iter().copied();
        for step in location.steps() {
            match step {
                RpgMakerLocationStep::ObjectKey(_) => {
                    let ordinal = ordinals
                        .next()
                        .ok_or(RpgMakerSemanticOrderProjectionError::MissingPhysicalOrdinal)?;
                    path.extend([0x30, usize_to_u64(ordinal)?]);
                }
                RpgMakerLocationStep::ArrayIndex(index) => {
                    let ordinal = ordinals
                        .next()
                        .ok_or(RpgMakerSemanticOrderProjectionError::MissingPhysicalOrdinal)?;
                    if ordinal != *index {
                        return Err(RpgMakerSemanticOrderProjectionError::ArrayOrdinalMismatch {
                            index: *index,
                            ordinal,
                        });
                    }
                    path.extend([0x31, usize_to_u64(*index)?]);
                }
                RpgMakerLocationStep::DecodeJsonString => path.push(0x32),
            }
        }
        if ordinals.next().is_some() {
            return Err(RpgMakerSemanticOrderProjectionError::ExtraPhysicalOrdinal);
        }
        Ok(Self::new(path, fragment))
    }

    /// 编码为可直接由 SQLite 按 BINARY 规则排序的规范 BLOB。
    pub(crate) fn encode(&self) -> Result<Vec<u8>, RpgMakerSemanticOrderKeyEncodeError> {
        let segment_bytes = self
            .physical_path
            .len()
            .checked_mul(9)
            .ok_or(RpgMakerSemanticOrderKeyEncodeError::LengthOverflow)?;
        let capacity = segment_bytes
            .checked_add(9)
            .ok_or(RpgMakerSemanticOrderKeyEncodeError::LengthOverflow)?;
        let mut encoded = Vec::with_capacity(capacity);
        for segment in &self.physical_path {
            encoded.push(0x01);
            encoded.extend_from_slice(&segment.to_be_bytes());
        }
        encoded.push(0x00);
        encoded.extend_from_slice(&self.fragment.to_be_bytes());
        Ok(encoded)
    }

    /// 严格解码规范 BLOB；任何截断、未知标记或终止符后的尾部字节都不是当前格式。
    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, RpgMakerSemanticOrderKeyDecodeError> {
        if encoded.len() < 9 {
            return Err(RpgMakerSemanticOrderKeyDecodeError::Truncated);
        }
        let mut physical_path = Vec::with_capacity(encoded.len() / 9);
        let mut offset = 0usize;
        loop {
            let remaining = encoded.len() - offset;
            if remaining < 9 {
                return Err(RpgMakerSemanticOrderKeyDecodeError::Truncated);
            }
            let marker = encoded[offset];
            let value = u64::from_be_bytes(
                encoded[offset + 1..offset + 9]
                    .try_into()
                    .expect("长度已经验证为完整 u64"),
            );
            offset += 9;
            match marker {
                0x01 => physical_path.push(value),
                0x00 if offset == encoded.len() => {
                    return Ok(Self::new(physical_path, value));
                }
                0x00 => return Err(RpgMakerSemanticOrderKeyDecodeError::TrailingBytes),
                actual => {
                    return Err(RpgMakerSemanticOrderKeyDecodeError::UnknownMarker { actual });
                }
            }
        }
    }
}

fn physical_source_path(
    source: &RpgMakerSource,
    plugin_parameter_order: Option<usize>,
) -> Result<Vec<u64>, RpgMakerSemanticOrderProjectionError> {
    let mut path = Vec::new();
    match source {
        RpgMakerSource::Data(file) => {
            path.push(0x10);
            path.push(
                StandardDataFile::ALL
                    .iter()
                    .position(|candidate| candidate == file)
                    .map(usize_to_u64)
                    .expect("标准数据文件必须属于封闭集合")?,
            );
        }
        RpgMakerSource::DataFile(file) => {
            path.push(0x11);
            push_string_segments(&mut path, file.as_str());
        }
        RpgMakerSource::Map(map_id) => path.extend([0x12, u64::from(map_id.get())]),
        RpgMakerSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => {
            path.extend([0x13, usize_to_u64(*plugin_index)?]);
            if let Some(parameter_order) = plugin_parameter_order {
                path.extend([0x14, usize_to_u64(parameter_order)?]);
            } else {
                // 只为没有原始参数对象的测试构造器保留可确定的后备键。
                // 生产 Rules Extract 始终传入参数插入序号。
                path.push(0x15);
                push_string_segments(&mut path, plugin_name);
                push_string_segments(&mut path, parameter_name);
            }
        }
    }
    Ok(path)
}

fn usize_to_u64(value: usize) -> Result<u64, RpgMakerSemanticOrderProjectionError> {
    u64::try_from(value).map_err(|_| RpgMakerSemanticOrderProjectionError::OrdinalOverflow)
}

fn json_physical_ordinals(
    root: &Value,
    steps: &[RpgMakerLocationStep],
) -> Result<Vec<usize>, RpgMakerSemanticOrderProjectionError> {
    let mut physical_ordinals = Vec::with_capacity(steps.len());
    let mut decoded_values = Vec::<Pin<Box<StackSafeJsonValue>>>::new();
    let mut current = root as *const Value;
    for step in steps {
        // SAFETY: `current` 只指向本函数期间不变的 `root` 子树，或下面
        // Pin<Box<_>> 保活的解码子树。遍历期间不修改任何一棵树。
        let value = unsafe { &*current };
        match step {
            RpgMakerLocationStep::ObjectKey(key) => {
                let object = value
                    .as_object()
                    .ok_or(RpgMakerSemanticOrderProjectionError::ExpectedObject)?;
                let ordinal = object
                    .keys()
                    .position(|candidate| candidate == key)
                    .ok_or(RpgMakerSemanticOrderProjectionError::MissingObjectKey)?;
                let child = object.get(key).expect("已按同一对象的键集确认字段存在");
                physical_ordinals.push(ordinal);
                current = child as *const Value;
            }
            RpgMakerLocationStep::ArrayIndex(index) => {
                let array = value
                    .as_array()
                    .ok_or(RpgMakerSemanticOrderProjectionError::ExpectedArray)?;
                let child = array
                    .get(*index)
                    .ok_or(RpgMakerSemanticOrderProjectionError::MissingArrayIndex)?;
                physical_ordinals.push(*index);
                current = child as *const Value;
            }
            RpgMakerLocationStep::DecodeJsonString => {
                let encoded = value
                    .as_str()
                    .ok_or(RpgMakerSemanticOrderProjectionError::ExpectedEncodedJsonString)?;
                let decoded = parse_json(encoded)
                    .map_err(|_| RpgMakerSemanticOrderProjectionError::InvalidEncodedJson)?;
                let decoded = Box::pin(decoded);
                current = &**decoded as *const Value;
                decoded_values.push(decoded);
            }
        }
    }
    Ok(physical_ordinals)
}

fn physical_location_path(location: &RpgMakerLocation) -> Vec<u64> {
    let mut path = Vec::new();
    match location.source() {
        RpgMakerSource::Data(file) => {
            path.push(0x10);
            path.push(
                u64::try_from(
                    StandardDataFile::ALL
                        .iter()
                        .position(|candidate| candidate == file)
                        .expect("标准数据文件必须属于封闭集合"),
                )
                .expect("当前 Rust 目标的 usize 必须可无损表达为 u64"),
            );
        }
        RpgMakerSource::DataFile(file) => {
            path.push(0x11);
            push_string_segments(&mut path, file.as_str());
        }
        RpgMakerSource::Map(map_id) => {
            path.extend([0x12, u64::from(map_id.get())]);
        }
        RpgMakerSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => {
            path.extend([
                0x13,
                u64::try_from(*plugin_index).expect("当前 Rust 目标的 usize 必须可无损表达为 u64"),
            ]);
            push_string_segments(&mut path, plugin_name);
            push_string_segments(&mut path, parameter_name);
        }
    }
    for step in location.steps() {
        match step {
            RpgMakerLocationStep::ObjectKey(key) => {
                path.push(0x30);
                push_string_segments(&mut path, key);
            }
            RpgMakerLocationStep::ArrayIndex(index) => path.extend([
                0x31,
                u64::try_from(*index).expect("当前 Rust 目标的 usize 必须可无损表达为 u64"),
            ]),
            RpgMakerLocationStep::DecodeJsonString => path.push(0x32),
        }
    }
    path
}

fn push_string_segments(path: &mut Vec<u64>, value: &str) {
    path.extend(value.as_bytes().iter().map(|byte| u64::from(*byte) + 1));
    path.push(0);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerSemanticOrderProjectionError {
    MissingSourceDocument,
    UnsupportedBuiltinPluginSource,
    ExpectedObject,
    MissingObjectKey,
    ExpectedArray,
    MissingArrayIndex,
    ExpectedEncodedJsonString,
    InvalidEncodedJson,
    MissingPhysicalOrdinal,
    ExtraPhysicalOrdinal,
    ArrayOrdinalMismatch { index: usize, ordinal: usize },
    OrdinalOverflow,
}

impl fmt::Display for RpgMakerSemanticOrderProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceDocument => formatter.write_str("语义顺序路径的源文档不存在"),
            Self::UnsupportedBuiltinPluginSource => {
                formatter.write_str("Builtin 语义顺序不接受插件参数来源")
            }
            Self::ExpectedObject => formatter.write_str("语义顺序路径要求 JSON 对象"),
            Self::MissingObjectKey => formatter.write_str("语义顺序路径的对象字段不存在"),
            Self::ExpectedArray => formatter.write_str("语义顺序路径要求 JSON 数组"),
            Self::MissingArrayIndex => formatter.write_str("语义顺序路径的数组下标不存在"),
            Self::ExpectedEncodedJsonString => {
                formatter.write_str("语义顺序路径要求包含 JSON 的字符串")
            }
            Self::InvalidEncodedJson => formatter.write_str("语义顺序路径中的嵌套 JSON 无效"),
            Self::MissingPhysicalOrdinal => formatter.write_str("语义顺序路径缺少物理序号"),
            Self::ExtraPhysicalOrdinal => formatter.write_str("语义顺序路径包含多余物理序号"),
            Self::ArrayOrdinalMismatch { index, ordinal } => write!(
                formatter,
                "数组下标 {index} 与遍历记录的物理序号 {ordinal} 不一致"
            ),
            Self::OrdinalOverflow => formatter.write_str("语义顺序的物理序号无法表示为 u64"),
        }
    }
}

impl Error for RpgMakerSemanticOrderProjectionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerSemanticOrderKeyEncodeError {
    LengthOverflow,
}

impl fmt::Display for RpgMakerSemanticOrderKeyEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker 语义顺序键编码长度溢出")
    }
}

impl Error for RpgMakerSemanticOrderKeyEncodeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerSemanticOrderKeyDecodeError {
    Truncated,
    UnknownMarker { actual: u8 },
    TrailingBytes,
}

impl RpgMakerSemanticOrderKeyDecodeError {
    pub(crate) const fn diagnostic_violation(
        self,
    ) -> crate::diagnostic::RpgMakerSemanticOrderKeyViolation {
        match self {
            Self::Truncated => crate::diagnostic::RpgMakerSemanticOrderKeyViolation::Truncated,
            Self::UnknownMarker { actual } => {
                crate::diagnostic::RpgMakerSemanticOrderKeyViolation::UnknownMarker { actual }
            }
            Self::TrailingBytes => {
                crate::diagnostic::RpgMakerSemanticOrderKeyViolation::TrailingBytes
            }
        }
    }
}

impl fmt::Display for RpgMakerSemanticOrderKeyDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("RPG Maker 语义顺序键不是完整的 9 字节段"),
            Self::UnknownMarker { actual } => {
                write!(
                    formatter,
                    "RPG Maker 语义顺序键包含未知段标记 0x{actual:02x}"
                )
            }
            Self::TrailingBytes => {
                formatter.write_str("RPG Maker 语义顺序键在终止段之后包含尾部字节")
            }
        }
    }
}

impl Error for RpgMakerSemanticOrderKeyDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Map, json};

    use crate::rpg_maker::text::StandardDataFile;

    #[test]
    fn semantic_order_key_round_trips_and_rejects_noncanonical_blobs() {
        let key = RpgMakerSemanticOrderKey::new(vec![0, 1, u64::MAX], 42);
        let encoded = key.encode().expect("有效顺序键应可编码");
        assert_eq!(
            RpgMakerSemanticOrderKey::decode(&encoded).expect("规范 BLOB 应可解码"),
            key
        );
        assert_eq!(
            RpgMakerSemanticOrderKey::decode(&encoded[..encoded.len() - 1]),
            Err(RpgMakerSemanticOrderKeyDecodeError::Truncated)
        );
        let mut unknown = encoded.clone();
        unknown[0] = 0x02;
        assert_eq!(
            RpgMakerSemanticOrderKey::decode(&unknown),
            Err(RpgMakerSemanticOrderKeyDecodeError::UnknownMarker { actual: 0x02 })
        );
        let mut trailing = RpgMakerSemanticOrderKey::new(Vec::new(), 0)
            .encode()
            .expect("有效顺序键应可编码");
        trailing.extend_from_slice(&[0; 9]);
        assert_eq!(
            RpgMakerSemanticOrderKey::decode(&trailing),
            Err(RpgMakerSemanticOrderKeyDecodeError::TrailingBytes)
        );
    }

    #[test]
    fn encoded_binary_order_is_exactly_rust_order() {
        let mut keys = vec![
            RpgMakerSemanticOrderKey::new(vec![1], 8),
            RpgMakerSemanticOrderKey::new(Vec::new(), u64::MAX),
            RpgMakerSemanticOrderKey::new(vec![1, 0], 0),
            RpgMakerSemanticOrderKey::new(vec![0, u64::MAX], 1),
            RpgMakerSemanticOrderKey::new(vec![1], 7),
            RpgMakerSemanticOrderKey::new(vec![u64::MAX], 0),
        ];
        let mut encoded = keys
            .iter()
            .map(|key| key.encode().expect("有效顺序键应可编码"))
            .collect::<Vec<_>>();
        keys.sort();
        encoded.sort();
        let decoded = encoded
            .iter()
            .map(|blob| RpgMakerSemanticOrderKey::decode(blob).expect("规范 BLOB 应可解码"))
            .collect::<Vec<_>>();
        assert_eq!(decoded, keys);
    }

    #[test]
    fn json_object_insertion_order_controls_semantic_order() {
        let source = RpgMakerSource::data(StandardDataFile::System);
        let location = |field: &str| {
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key(field)])
        };
        let root = |first: &str, second: &str| {
            let mut object = Map::new();
            object.insert(first.to_owned(), json!(first));
            object.insert(second.to_owned(), json!(second));
            Value::Object(object)
        };

        let alpha_then_beta = root("alpha", "beta");
        let beta_then_alpha = root("beta", "alpha");
        let alpha_first =
            RpgMakerSemanticOrderKey::from_json_location(&location("alpha"), &alpha_then_beta, 0)
                .expect("存在的字段应可建立顺序键");
        let beta_second =
            RpgMakerSemanticOrderKey::from_json_location(&location("beta"), &alpha_then_beta, 0)
                .expect("存在的字段应可建立顺序键");
        let beta_first =
            RpgMakerSemanticOrderKey::from_json_location(&location("beta"), &beta_then_alpha, 0)
                .expect("存在的字段应可建立顺序键");
        let alpha_second =
            RpgMakerSemanticOrderKey::from_json_location(&location("alpha"), &beta_then_alpha, 0)
                .expect("存在的字段应可建立顺序键");

        assert!(alpha_first < beta_second);
        assert!(beta_first < alpha_second);
        assert_eq!(alpha_first.physical_path(), beta_first.physical_path());
        assert_eq!(beta_second.physical_path(), alpha_second.physical_path());
    }
}
