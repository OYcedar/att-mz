//! RPG Maker 结构化位置的持久化编码。
//!
//! 数据库键使用规范 JSON，而不使用面向人类的 `Display` 文本。
//! 读取、写入和日志 wire 共享同一份结构化位置语义。

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget, DirectTextPart,
    DirectTextRecipe, MutationTarget, ProjectionModelError, ScalarFieldKey, TextFieldRole,
    TextProjectionRecipe,
};
use crate::rpg_maker::text::DataFileName;

/// 在数据库中无损保存 `RpgMakerLocation` 的规范编解码器。
pub(crate) struct RpgMakerLocationCodec;

impl RpgMakerLocationCodec {
    /// 把结构化位置编码为确定的单行 JSON。
    pub(crate) fn encode(
        location: &RpgMakerLocation,
    ) -> Result<String, RpgMakerLocationCodecError> {
        let stored = StoredLocation::from(location);
        serde_json::to_string(&stored).map_err(RpgMakerLocationCodecError::Encode)
    }

    /// 把数据库中的权威位置解码回结构化类型。
    pub(crate) fn decode(value: &str) -> Result<RpgMakerLocation, RpgMakerLocationCodecError> {
        let stored = serde_json::from_str::<StoredLocation>(value)
            .map_err(RpgMakerLocationCodecError::Decode)?;
        stored.try_into()
    }
}

/// 逻辑文本身份、强角色、物理目标和投影配方的内部规范 JSON 编解码器。
pub(crate) struct RpgMakerProjectionCodec;

impl RpgMakerProjectionCodec {
    pub(crate) fn encode_role(
        role: &TextFieldRole,
    ) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&StoredRole::from(role)).map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_role(value: &str) -> Result<TextFieldRole, RpgMakerProjectionCodecError> {
        serde_json::from_str::<StoredRole>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .try_into()
    }

    pub(crate) fn encode_target(
        target: &MutationTarget,
    ) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&StoredMutationTarget::from(target))
            .map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_target(
        value: &str,
    ) -> Result<MutationTarget, RpgMakerProjectionCodecError> {
        serde_json::from_str::<StoredMutationTarget>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .try_into()
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
        serde_json::from_str::<Vec<StoredRecipe>>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }
}

/// RPG Maker 位置编解码失败。
#[derive(Debug)]
pub(crate) enum RpgMakerLocationCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    InvalidDataFile(String),
}

impl fmt::Display for RpgMakerLocationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 RPG Maker 位置：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 RPG Maker 位置：{source}"),
            Self::InvalidDataFile(file_name) => {
                write!(formatter, "RPG Maker 位置引用了无效 data 文件：{file_name}")
            }
        }
    }
}

impl Error for RpgMakerLocationCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) | Self::Decode(source) => Some(source),
            Self::InvalidDataFile(_) => None,
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

impl From<&RpgMakerSource> for StoredSource {
    fn from(source: &RpgMakerSource) -> Self {
        match source {
            RpgMakerSource::Data(file) => Self::Data {
                file: file.file_name().to_owned(),
            },
            RpgMakerSource::DataFile(file) => Self::Data {
                file: file.as_str().to_owned(),
            },
            RpgMakerSource::Map(map_id) => Self::Map { map_id: *map_id },
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredRole {
    Scalar { key: String },
    DialogueSpeaker,
    DialogueBody { index: usize },
    ScrollingTextBody { index: usize },
}

impl From<&TextFieldRole> for StoredRole {
    fn from(role: &TextFieldRole) -> Self {
        match role {
            TextFieldRole::Scalar(key) => Self::Scalar {
                key: key.as_str().to_owned(),
            },
            TextFieldRole::DialogueSpeaker => Self::DialogueSpeaker,
            TextFieldRole::DialogueBody { index } => Self::DialogueBody { index: *index },
            TextFieldRole::ScrollingTextBody { index } => Self::ScrollingTextBody { index: *index },
        }
    }
}

impl TryFrom<StoredRole> for TextFieldRole {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(role: StoredRole) -> Result<Self, Self::Error> {
        match role {
            StoredRole::Scalar { key } => ScalarFieldKey::new(key)
                .map(Self::Scalar)
                .map_err(RpgMakerProjectionCodecError::Projection),
            StoredRole::DialogueSpeaker => Ok(Self::DialogueSpeaker),
            StoredRole::DialogueBody { index } => Ok(Self::DialogueBody { index }),
            StoredRole::ScrollingTextBody { index } => Ok(Self::ScrollingTextBody { index }),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredMutationTarget {
    Value { location: StoredLocation },
    DialogueBlock { header: StoredLocation },
}

impl From<&MutationTarget> for StoredMutationTarget {
    fn from(target: &MutationTarget) -> Self {
        match target {
            MutationTarget::Value(location) => Self::Value {
                location: StoredLocation::from(location),
            },
            MutationTarget::DialogueBlock { header } => Self::DialogueBlock {
                header: StoredLocation::from(header),
            },
        }
    }
}

impl TryFrom<StoredMutationTarget> for MutationTarget {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(target: StoredMutationTarget) -> Result<Self, Self::Error> {
        match target {
            StoredMutationTarget::Value { location } => Ok(Self::Value(
                location
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
            )),
            StoredMutationTarget::DialogueBlock { header } => Ok(Self::DialogueBlock {
                header: header
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredRecipe {
    Direct {
        target: StoredLocation,
        expected_raw: String,
        parts: Vec<StoredDirectTextPart>,
    },
    Dialogue {
        group_location: StoredLocation,
        direct_speaker: Option<StoredDirectSpeakerTarget>,
        lines: Vec<StoredDialogueLineRecipe>,
    },
}

impl From<&TextProjectionRecipe> for StoredRecipe {
    fn from(recipe: &TextProjectionRecipe) -> Self {
        match recipe {
            TextProjectionRecipe::Direct(recipe) => Self::Direct {
                target: StoredLocation::from(recipe.target()),
                expected_raw: recipe.expected_raw().to_owned(),
                parts: recipe.parts().iter().map(Into::into).collect(),
            },
            TextProjectionRecipe::Dialogue(recipe) => Self::Dialogue {
                group_location: StoredLocation::from(recipe.group_location()),
                direct_speaker: recipe.direct_speaker().map(Into::into),
                lines: recipe.lines().iter().map(Into::into).collect(),
            },
        }
    }
}

impl TryFrom<StoredRecipe> for TextProjectionRecipe {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(recipe: StoredRecipe) -> Result<Self, Self::Error> {
        match recipe {
            StoredRecipe::Direct {
                target,
                expected_raw,
                parts,
            } => DirectTextRecipe::new(
                target
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
                expected_raw,
                parts
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .map(TextProjectionRecipe::Direct)
            .map_err(RpgMakerProjectionCodecError::Projection),
            StoredRecipe::Dialogue {
                group_location,
                direct_speaker,
                lines,
            } => DialogueWriteRecipe::new(
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
            .map_err(RpgMakerProjectionCodecError::Projection),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredDirectTextPart {
    Literal { value: String },
    TextSlot { role: StoredRole },
}

impl From<&DirectTextPart> for StoredDirectTextPart {
    fn from(part: &DirectTextPart) -> Self {
        match part {
            DirectTextPart::Literal(value) => Self::Literal {
                value: value.clone(),
            },
            DirectTextPart::TextSlot { role } => Self::TextSlot {
                role: StoredRole::from(role),
            },
        }
    }
}

impl TryFrom<StoredDirectTextPart> for DirectTextPart {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(part: StoredDirectTextPart) -> Result<Self, Self::Error> {
        match part {
            StoredDirectTextPart::Literal { value } => Ok(Self::Literal(value)),
            StoredDirectTextPart::TextSlot { role } => Ok(Self::TextSlot {
                role: role.try_into()?,
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDirectSpeakerTarget {
    physical_location: StoredLocation,
    expected_raw: String,
}

impl From<&DirectSpeakerTarget> for StoredDirectSpeakerTarget {
    fn from(target: &DirectSpeakerTarget) -> Self {
        Self {
            physical_location: StoredLocation::from(target.physical_location()),
            expected_raw: target.expected_raw().to_owned(),
        }
    }
}

impl TryFrom<StoredDirectSpeakerTarget> for DirectSpeakerTarget {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(target: StoredDirectSpeakerTarget) -> Result<Self, Self::Error> {
        Ok(Self::new(
            target
                .physical_location
                .try_into()
                .map_err(RpgMakerProjectionCodecError::Location)?,
            target.expected_raw,
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDialogueLineRecipe {
    physical_location: StoredLocation,
    expected_raw: String,
    parts: Vec<StoredDialogueLinePart>,
}

impl From<&DialogueLineRecipe> for StoredDialogueLineRecipe {
    fn from(line: &DialogueLineRecipe) -> Self {
        Self {
            physical_location: StoredLocation::from(line.physical_location()),
            expected_raw: line.expected_raw().to_owned(),
            parts: line.parts().iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<StoredDialogueLineRecipe> for DialogueLineRecipe {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(line: StoredDialogueLineRecipe) -> Result<Self, Self::Error> {
        DialogueLineRecipe::new(
            line.physical_location
                .try_into()
                .map_err(RpgMakerProjectionCodecError::Location)?,
            line.expected_raw,
            line.parts
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(RpgMakerProjectionCodecError::Projection)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredDialogueLinePart {
    Literal { value: String },
    SpeakerSlot,
    BodySlot { index: usize },
}

impl From<&DialogueLinePart> for StoredDialogueLinePart {
    fn from(part: &DialogueLinePart) -> Self {
        match part {
            DialogueLinePart::Literal(value) => Self::Literal {
                value: value.clone(),
            },
            DialogueLinePart::SpeakerSlot => Self::SpeakerSlot,
            DialogueLinePart::BodySlot { index } => Self::BodySlot { index: *index },
        }
    }
}

impl TryFrom<StoredDialogueLinePart> for DialogueLinePart {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(part: StoredDialogueLinePart) -> Result<Self, Self::Error> {
        match part {
            StoredDialogueLinePart::Literal { value } => Ok(Self::Literal(value)),
            StoredDialogueLinePart::SpeakerSlot => Ok(Self::SpeakerSlot),
            StoredDialogueLinePart::BodySlot { index } => Ok(Self::BodySlot { index }),
        }
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerProjectionCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    Location(RpgMakerLocationCodecError),
    Projection(ProjectionModelError),
}

impl fmt::Display for RpgMakerProjectionCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 RPG Maker 文本投影：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 RPG Maker 文本投影：{source}"),
            Self::Location(source) => write!(formatter, "文本投影位置无效：{source}"),
            Self::Projection(source) => write!(formatter, "文本投影配方无效：{source}"),
        }
    }
}

impl Error for RpgMakerProjectionCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) | Self::Decode(source) => Some(source),
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
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

        assert_eq!(
            encoded,
            r#"{"kind":"value","source":{"kind":"data","file":"Items.json"},"steps":[{"kind":"array_index","index":10},{"kind":"object_key","key":"name"}]}"#
        );
        assert_eq!(
            RpgMakerLocationCodec::decode(&encoded).expect("位置应该可解码"),
            location
        );
    }

    #[test]
    fn every_location_variant_and_decoding_boundary_round_trips() {
        let locations = [
            RpgMakerLocation::note_tag(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![RpgMakerLocationStep::index(2)],
                "Category",
                1,
            ),
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
            RpgMakerLocation::value(
                RpgMakerSource::plugin_parameter(4, "QuestMenu", "Categories"),
                vec![
                    RpgMakerLocationStep::DecodeJsonString,
                    RpgMakerLocationStep::index(2),
                    RpgMakerLocationStep::key("Name"),
                ],
            ),
        ];

        for location in locations {
            let encoded = RpgMakerLocationCodec::encode(&location).expect("位置应该可编码");
            assert_eq!(
                RpgMakerLocationCodec::decode(&encoded).expect("位置应该可解码"),
                location
            );
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
        let encoded =
            r#"{"kind":"value","source":{"kind":"data","file":"Custom.json"},"steps":[]}"#;

        let decoded = RpgMakerLocationCodec::decode(encoded).expect("非标准 JSON 基名应可持久化");
        assert_eq!(decoded.to_string(), "data/Custom.json");
    }
}
