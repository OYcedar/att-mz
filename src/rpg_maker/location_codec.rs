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
    DirectTextRecipe, MutationClaim, MutationResource, ProjectionModelError, ScalarFieldKey,
    TextProjectionRecipe, TextUnitRole,
};
use crate::rpg_maker::text::{DataFileName, MapId};

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

/// 逻辑文本身份、强角色、物理修改资源与投影配方的内部规范 JSON 编解码器。
pub(crate) struct RpgMakerProjectionCodec;

impl RpgMakerProjectionCodec {
    pub(crate) fn encode_role(role: &TextUnitRole) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&StoredRole::from(role)).map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_role(value: &str) -> Result<TextUnitRole, RpgMakerProjectionCodecError> {
        serde_json::from_str::<StoredRole>(value)
            .map_err(RpgMakerProjectionCodecError::Decode)?
            .try_into()
    }

    pub(crate) fn encode_mutation_resource(
        resource: &MutationResource,
    ) -> Result<String, RpgMakerProjectionCodecError> {
        serde_json::to_string(&StoredMutationResource::from(resource))
            .map_err(RpgMakerProjectionCodecError::Encode)
    }

    pub(crate) fn decode_mutation_resource(
        value: &str,
    ) -> Result<MutationResource, RpgMakerProjectionCodecError> {
        serde_json::from_str::<StoredMutationResource>(value)
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
    InvalidMapId(u32),
}

impl fmt::Display for RpgMakerLocationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(source) => write!(formatter, "无法编码 RPG Maker 位置：{source}"),
            Self::Decode(source) => write!(formatter, "无法解码 RPG Maker 位置：{source}"),
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
            Self::InvalidDataFile(_) | Self::InvalidMapId(_) => None,
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
    DialogueBody,
    Choices,
    ScrollingText,
}

impl From<&TextUnitRole> for StoredRole {
    fn from(role: &TextUnitRole) -> Self {
        match role {
            TextUnitRole::Scalar(key) => Self::Scalar {
                key: key.as_str().to_owned(),
            },
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
            StoredRole::Scalar { key } => ScalarFieldKey::new(key)
                .map(Self::Scalar)
                .map_err(RpgMakerProjectionCodecError::Projection),
            StoredRole::DialogueSpeaker => Ok(Self::DialogueSpeaker),
            StoredRole::DialogueBody => Ok(Self::DialogueBody),
            StoredRole::Choices => Ok(Self::Choices),
            StoredRole::ScrollingText => Ok(Self::ScrollingText),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredMutationResource {
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

impl From<&MutationResource> for StoredMutationResource {
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

impl TryFrom<StoredMutationResource> for MutationResource {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(resource: StoredMutationResource) -> Result<Self, Self::Error> {
        match resource {
            StoredMutationResource::Value { source, steps } => Ok(Self::Value {
                source: source
                    .try_into()
                    .map_err(RpgMakerProjectionCodecError::Location)?,
                steps: steps.into_iter().map(Into::into).collect(),
            }),
            StoredMutationResource::NoteTag {
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
            StoredMutationResource::CommentTag {
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredRecipe {
    Direct {
        target: StoredLocation,
        mutation_claim: StoredMutationClaim,
        expected_raw: String,
        parts: Vec<StoredDirectTextPart>,
    },
    Dialogue {
        group_location: StoredLocation,
        direct_speaker: Option<StoredDirectSpeakerTarget>,
        lines: Vec<StoredDialogueLineRecipe>,
    },
    Claim {
        mutation_claim: StoredMutationClaim,
    },
}

impl From<&TextProjectionRecipe> for StoredRecipe {
    fn from(recipe: &TextProjectionRecipe) -> Self {
        match recipe {
            TextProjectionRecipe::Direct(recipe) => Self::Direct {
                target: StoredLocation::from(recipe.target()),
                mutation_claim: StoredMutationClaim::from(recipe.mutation_claim()),
                expected_raw: recipe.expected_raw().to_owned(),
                parts: recipe.parts().iter().map(Into::into).collect(),
            },
            TextProjectionRecipe::Dialogue(recipe) => Self::Dialogue {
                group_location: StoredLocation::from(recipe.group_location()),
                direct_speaker: recipe.direct_speaker().map(Into::into),
                lines: recipe.lines().iter().map(Into::into).collect(),
            },
            TextProjectionRecipe::Claim(claim) => Self::Claim {
                mutation_claim: StoredMutationClaim::from(claim),
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
                mutation_claim,
                expected_raw,
                parts,
            } => DirectTextRecipe::new_with_claim(
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
            StoredRecipe::Claim { mutation_claim } => {
                mutation_claim.try_into().map(TextProjectionRecipe::Claim)
            }
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredMutationClaim {
    Value {
        location: StoredLocation,
    },
    NoteTag {
        location: StoredLocation,
    },
    CommentTag {
        location: StoredLocation,
        backing_values: Vec<StoredLocation>,
    },
    EventBlock {
        header: StoredLocation,
        covered_values: Vec<StoredLocation>,
    },
}

impl From<&MutationClaim> for StoredMutationClaim {
    fn from(claim: &MutationClaim) -> Self {
        match claim {
            MutationClaim::Value(location) => Self::Value {
                location: location.into(),
            },
            MutationClaim::NoteTag(location) => Self::NoteTag {
                location: location.into(),
            },
            MutationClaim::CommentTag {
                location,
                backing_values,
            } => Self::CommentTag {
                location: location.into(),
                backing_values: backing_values.iter().map(Into::into).collect(),
            },
            MutationClaim::EventBlock {
                header,
                covered_values,
            } => Self::EventBlock {
                header: header.into(),
                covered_values: covered_values.iter().map(Into::into).collect(),
            },
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
            StoredMutationClaim::Value { location } => {
                let location = decode(location)?;
                if !matches!(location, RpgMakerLocation::Value { .. }) {
                    return Err(RpgMakerProjectionCodecError::MutationClaimKindMismatch {
                        expected: "value",
                        actual: location_kind(&location),
                    });
                }
                Ok(MutationClaim::Value(location))
            }
            StoredMutationClaim::NoteTag { location } => {
                let location = decode(location)?;
                if !matches!(location, RpgMakerLocation::NoteTag { .. }) {
                    return Err(RpgMakerProjectionCodecError::MutationClaimKindMismatch {
                        expected: "note_tag",
                        actual: location_kind(&location),
                    });
                }
                Ok(MutationClaim::NoteTag(location))
            }
            StoredMutationClaim::CommentTag {
                location,
                backing_values,
            } => MutationClaim::comment_tag(
                decode(location)?,
                backing_values
                    .into_iter()
                    .map(decode)
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .map_err(RpgMakerProjectionCodecError::Projection),
            StoredMutationClaim::EventBlock {
                header,
                covered_values,
            } => MutationClaim::event_block(
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredDirectTextPart {
    Literal {
        value: String,
    },
    TextSlot {
        role: StoredRole,
    },
    LineSlot {
        role: StoredRole,
        source_line_index: usize,
    },
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
            DirectTextPart::LineSlot {
                role,
                source_line_index,
            } => Self::LineSlot {
                role: StoredRole::from(role),
                source_line_index: *source_line_index,
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
            StoredDirectTextPart::LineSlot {
                role,
                source_line_index,
            } => Ok(Self::LineSlot {
                role: role.try_into()?,
                source_line_index,
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
    BodyLine { source_line_index: usize },
}

impl From<&DialogueLinePart> for StoredDialogueLinePart {
    fn from(part: &DialogueLinePart) -> Self {
        match part {
            DialogueLinePart::Literal(value) => Self::Literal {
                value: value.clone(),
            },
            DialogueLinePart::SpeakerSlot => Self::SpeakerSlot,
            DialogueLinePart::BodyLine { source_line_index } => Self::BodyLine {
                source_line_index: *source_line_index,
            },
        }
    }
}

impl TryFrom<StoredDialogueLinePart> for DialogueLinePart {
    type Error = RpgMakerProjectionCodecError;

    fn try_from(part: StoredDialogueLinePart) -> Result<Self, Self::Error> {
        match part {
            StoredDialogueLinePart::Literal { value } => Ok(Self::Literal(value)),
            StoredDialogueLinePart::SpeakerSlot => Ok(Self::SpeakerSlot),
            StoredDialogueLinePart::BodyLine { source_line_index } => {
                Ok(Self::BodyLine { source_line_index })
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerProjectionCodecError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
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
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::MutationClaimKindMismatch { .. } => None,
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
    fn stored_mutation_claim_kind_must_match_its_location_variant() {
        let note = RpgMakerLocation::note_tag(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
            "Help",
            0,
        );

        let error = MutationClaim::try_from(StoredMutationClaim::Value {
            location: StoredLocation::from(&note),
        })
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
        let stored = |coverage: Vec<&RpgMakerLocation>| StoredMutationClaim::EventBlock {
            header: StoredLocation::from(&header),
            covered_values: coverage.into_iter().map(StoredLocation::from).collect(),
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
        let encoded =
            r#"{"kind":"value","source":{"kind":"data","file":"Custom.json"},"steps":[]}"#;

        let decoded = RpgMakerLocationCodec::decode(encoded).expect("非标准 JSON 基名应可持久化");
        assert_eq!(decoded.to_string(), "data/Custom.json");
    }

    #[test]
    fn persisted_map_identity_rejects_zero() {
        let encoded = r#"{"kind":"value","source":{"kind":"map","map_id":0},"steps":[]}"#;

        assert!(matches!(
            RpgMakerLocationCodec::decode(encoded),
            Err(RpgMakerLocationCodecError::InvalidMapId(0))
        ));
    }

    #[test]
    fn semantic_unit_roles_have_no_physical_line_identity() {
        let cases = [
            (
                TextUnitRole::DialogueSpeaker,
                r#"{"kind":"dialogue_speaker"}"#,
            ),
            (TextUnitRole::DialogueBody, r#"{"kind":"dialogue_body"}"#),
            (TextUnitRole::Choices, r#"{"kind":"choices"}"#),
            (TextUnitRole::ScrollingText, r#"{"kind":"scrolling_text"}"#),
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

        assert!(encoded.contains(r#""kind":"line_slot""#));
        assert!(encoded.contains(r#""source_line_index":1"#));
        assert_eq!(
            RpgMakerProjectionCodec::decode_recipes(&encoded).expect("配方应可解码"),
            [recipe]
        );
    }
}
