//! MZ 结构化位置的持久化编码。
//!
//! 数据库键使用规范 JSON，而不使用面向人类的 `Display` 文本。
//! 读取、写入和日志 wire 共享同一份结构化位置语义。

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile};

/// 在数据库中无损保存 `MzLocation` 的规范编解码器。
pub(crate) struct MzLocationCodec;

impl MzLocationCodec {
    /// 把结构化位置编码为确定的单行 JSON。
    pub(crate) fn encode(location: &MzLocation) -> Result<String, MzLocationCodecError> {
        let stored = StoredLocation::from(location);
        serde_json::to_string(&stored).map_err(MzLocationCodecError::Encode)
    }

    /// 把数据库中的权威位置解码回结构化类型。
    pub(crate) fn decode(value: &str) -> Result<MzLocation, MzLocationCodecError> {
        let stored =
            serde_json::from_str::<StoredLocation>(value).map_err(MzLocationCodecError::Decode)?;
        stored.try_into()
    }
}

/// MZ 位置编解码失败。
#[derive(Debug)]
pub(crate) enum MzLocationCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    UnknownStandardDataFile(String),
}

impl fmt::Display for MzLocationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 MZ 位置：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 MZ 位置：{source}"),
            Self::UnknownStandardDataFile(file_name) => {
                write!(formatter, "MZ 位置引用了未知标准文件：{file_name}")
            }
        }
    }
}

impl Error for MzLocationCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) | Self::Decode(source) => Some(source),
            Self::UnknownStandardDataFile(_) => None,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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

impl From<&MzLocation> for StoredLocation {
    fn from(location: &MzLocation) -> Self {
        match location {
            MzLocation::Value { source, steps } => Self::Value {
                source: source.into(),
                steps: steps.iter().map(Into::into).collect(),
            },
            MzLocation::NoteTag {
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
            MzLocation::CommentTag {
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

impl TryFrom<StoredLocation> for MzLocation {
    type Error = MzLocationCodecError;

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

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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

impl From<&MzSource> for StoredSource {
    fn from(source: &MzSource) -> Self {
        match source {
            MzSource::Data(file) => Self::Data {
                file: file.file_name().to_owned(),
            },
            MzSource::Map(map_id) => Self::Map { map_id: *map_id },
            MzSource::PluginParameter {
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

impl TryFrom<StoredSource> for MzSource {
    type Error = MzLocationCodecError;

    fn try_from(source: StoredSource) -> Result<Self, Self::Error> {
        match source {
            StoredSource::Data { file } => StandardDataFile::from_file_name(&file)
                .map(Self::data)
                .ok_or(MzLocationCodecError::UnknownStandardDataFile(file)),
            StoredSource::Map { map_id } => Ok(Self::map(map_id)),
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

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredStep {
    ObjectKey { key: String },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl From<&MzLocationStep> for StoredStep {
    fn from(step: &MzLocationStep) -> Self {
        match step {
            MzLocationStep::ObjectKey(key) => Self::ObjectKey { key: key.clone() },
            MzLocationStep::ArrayIndex(index) => Self::ArrayIndex { index: *index },
            MzLocationStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

impl From<StoredStep> for MzLocationStep {
    fn from(step: StoredStep) -> Self {
        match step {
            StoredStep::ObjectKey { key } => Self::key(key),
            StoredStep::ArrayIndex { index } => Self::index(index),
            StoredStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_location_has_deterministic_json_and_round_trips() {
        let location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(10), MzLocationStep::key("name")],
        );

        let encoded = MzLocationCodec::encode(&location).expect("位置应该可编码");

        assert_eq!(
            encoded,
            r#"{"kind":"value","source":{"kind":"data","file":"Items.json"},"steps":[{"kind":"array_index","index":10},{"kind":"object_key","key":"name"}]}"#
        );
        assert_eq!(
            MzLocationCodec::decode(&encoded).expect("位置应该可解码"),
            location
        );
    }

    #[test]
    fn every_location_variant_and_decoding_boundary_round_trips() {
        let locations = [
            MzLocation::note_tag(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(2)],
                "Category",
                1,
            ),
            MzLocation::comment_tag(
                MzSource::map(3),
                vec![
                    MzLocationStep::key("events"),
                    MzLocationStep::index(1),
                    MzLocationStep::DecodeJsonString,
                ],
                "Quest",
                0,
            ),
            MzLocation::value(
                MzSource::plugin_parameter(4, "QuestMenu", "Categories"),
                vec![
                    MzLocationStep::DecodeJsonString,
                    MzLocationStep::index(2),
                    MzLocationStep::key("Name"),
                ],
            ),
        ];

        for location in locations {
            let encoded = MzLocationCodec::encode(&location).expect("位置应该可编码");
            assert_eq!(
                MzLocationCodec::decode(&encoded).expect("位置应该可解码"),
                location
            );
        }
    }

    #[test]
    fn display_text_is_not_accepted_as_authoritative_storage() {
        let location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(10), MzLocationStep::key("name")],
        );

        assert!(MzLocationCodec::decode(&location.to_string()).is_err());
    }

    #[test]
    fn rejects_unknown_standard_data_file() {
        let encoded =
            r#"{"kind":"value","source":{"kind":"data","file":"Custom.json"},"steps":[]}"#;

        assert!(matches!(
            MzLocationCodec::decode(encoded),
            Err(MzLocationCodecError::UnknownStandardDataFile(file)) if file == "Custom.json"
        ));
    }
}
