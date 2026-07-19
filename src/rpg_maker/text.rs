//! RPG Maker 提取、翻译和写回共同使用的结构化物理位置。
//!
//! 结构化位置是权威身份，`Display` 只用于诊断，不作为持久化协议解析。

use std::error::Error;
use std::fmt;

/// RPG Maker `data/` 中由 Builtin 理解语义的标准数据文件。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum StandardDataFile {
    Actors,
    Animations,
    Armors,
    Classes,
    CommonEvents,
    Enemies,
    Items,
    MapInfos,
    Skills,
    States,
    System,
    Tilesets,
    Troops,
    Weapons,
}

impl StandardDataFile {
    pub(crate) const ALL: [Self; 14] = [
        Self::Actors,
        Self::Animations,
        Self::Armors,
        Self::Classes,
        Self::CommonEvents,
        Self::Enemies,
        Self::Items,
        Self::MapInfos,
        Self::Skills,
        Self::States,
        Self::System,
        Self::Tilesets,
        Self::Troops,
        Self::Weapons,
    ];

    pub(crate) const fn file_name(self) -> &'static str {
        match self {
            Self::Actors => "Actors.json",
            Self::Animations => "Animations.json",
            Self::Armors => "Armors.json",
            Self::Classes => "Classes.json",
            Self::CommonEvents => "CommonEvents.json",
            Self::Enemies => "Enemies.json",
            Self::Items => "Items.json",
            Self::MapInfos => "MapInfos.json",
            Self::Skills => "Skills.json",
            Self::States => "States.json",
            Self::System => "System.json",
            Self::Tilesets => "Tilesets.json",
            Self::Troops => "Troops.json",
            Self::Weapons => "Weapons.json",
        }
    }

    pub(crate) fn from_file_name(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.file_name() == value)
    }
}

/// Rules 可以精确选择的安全 JSON 基名。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DataFileName(String);

impl DataFileName {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, DataFileNameError> {
        let value = value.into();
        let has_invalid_character = value
            .chars()
            .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character));
        if value.is_empty()
            || !value.ends_with(".json")
            || value == ".json"
            || has_invalid_character
            || is_reserved_windows_file_name(&value)
        {
            return Err(DataFileNameError { value });
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_reserved_windows_file_name(value: &str) -> bool {
    let stem = value
        .strip_suffix(".json")
        .expect("调用方已经校验 .json 后缀");
    let device_name = stem
        .split('.')
        .next()
        .expect("split 始终产生至少一个元素")
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(
        device_name.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || device_name
        .strip_prefix("COM")
        .or_else(|| device_name.strip_prefix("LPT"))
        .is_some_and(|number| number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9'))
}

impl fmt::Display for DataFileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DataFileNameError {
    value: String,
}

impl fmt::Display for DataFileNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "data 文件必须是安全的精确 *.json 基名：{:?}",
            self.value
        )
    }
}

impl Error for DataFileNameError {}

/// RPG Maker 文本所在的物理来源。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerSource {
    Data(StandardDataFile),
    DataFile(DataFileName),
    Map(u32),
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl RpgMakerSource {
    pub(crate) fn data(file: StandardDataFile) -> Self {
        Self::Data(file)
    }

    pub(crate) fn data_file(file: DataFileName) -> Self {
        StandardDataFile::from_file_name(file.as_str()).map_or(Self::DataFile(file), Self::Data)
    }

    pub(crate) fn map(map_id: u32) -> Self {
        Self::Map(map_id)
    }

    pub(crate) fn plugin_parameter(
        plugin_index: usize,
        plugin_name: impl Into<String>,
        parameter_name: impl Into<String>,
    ) -> Self {
        Self::PluginParameter {
            plugin_index,
            plugin_name: plugin_name.into(),
            parameter_name: parameter_name.into(),
        }
    }
}

impl fmt::Display for RpgMakerSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(file) => write!(formatter, "data/{}", file.file_name()),
            Self::DataFile(file) => write!(formatter, "data/{file}"),
            Self::Map(map_id) => write!(formatter, "data/Map{map_id:03}.json"),
            Self::PluginParameter {
                plugin_name,
                parameter_name,
                ..
            } => {
                write!(formatter, "plugins.js[{plugin_name}]")?;
                write_object_key(formatter, parameter_name)
            }
        }
    }
}

/// 在 JSON 结构中从来源根节点走到具体值的一步。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerLocationStep {
    ObjectKey(String),
    ArrayIndex(usize),
    DecodeJsonString,
}

impl RpgMakerLocationStep {
    pub(crate) fn key(value: impl Into<String>) -> Self {
        Self::ObjectKey(value.into())
    }

    pub(crate) fn index(value: usize) -> Self {
        Self::ArrayIndex(value)
    }
}

/// 一个物理值或局部文本容器的结构化权威地址。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerLocation {
    Value {
        source: RpgMakerSource,
        steps: Vec<RpgMakerLocationStep>,
    },
    NoteTag {
        source: RpgMakerSource,
        container_steps: Vec<RpgMakerLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: RpgMakerSource,
        command_steps: Vec<RpgMakerLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
}

impl RpgMakerLocation {
    pub(crate) fn value(source: RpgMakerSource, steps: Vec<RpgMakerLocationStep>) -> Self {
        Self::Value { source, steps }
    }

    pub(crate) fn note_tag(
        source: RpgMakerSource,
        container_steps: Vec<RpgMakerLocationStep>,
        tag_name: impl Into<String>,
        occurrence: usize,
    ) -> Self {
        Self::NoteTag {
            source,
            container_steps,
            tag_name: tag_name.into(),
            occurrence,
        }
    }

    pub(crate) fn comment_tag(
        source: RpgMakerSource,
        command_steps: Vec<RpgMakerLocationStep>,
        tag_name: impl Into<String>,
        occurrence: usize,
    ) -> Self {
        Self::CommentTag {
            source,
            command_steps,
            tag_name: tag_name.into(),
            occurrence,
        }
    }

    pub(crate) fn source(&self) -> &RpgMakerSource {
        match self {
            Self::Value { source, .. }
            | Self::NoteTag { source, .. }
            | Self::CommentTag { source, .. } => source,
        }
    }

    pub(crate) fn steps(&self) -> &[RpgMakerLocationStep] {
        match self {
            Self::Value { steps, .. } => steps,
            Self::NoteTag {
                container_steps, ..
            } => container_steps,
            Self::CommentTag { command_steps, .. } => command_steps,
        }
    }
}

impl fmt::Display for RpgMakerLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value { source, steps } => write_path(formatter, source, steps),
            Self::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => {
                write_path(formatter, source, container_steps)?;
                write!(formatter, ".note#{tag_name}[{occurrence}]")
            }
            Self::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => {
                write_path(formatter, source, command_steps)?;
                write!(formatter, "#comment:{tag_name}[{occurrence}]")
            }
        }
    }
}

/// 文本组所表达的 RPG Maker 语义对象。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum TextGroupKind {
    DatabaseEntry,
    System,
    Map,
    EventDialogue,
    EventChoices,
    EventScrollingText,
    EventCommand,
    PluginParameter,
}

fn write_path(
    formatter: &mut fmt::Formatter<'_>,
    source: &RpgMakerSource,
    steps: &[RpgMakerLocationStep],
) -> fmt::Result {
    write!(formatter, "{source}")?;
    for step in steps {
        match step {
            RpgMakerLocationStep::ObjectKey(key) => write_object_key(formatter, key)?,
            RpgMakerLocationStep::ArrayIndex(index) => write!(formatter, "[{index}]")?,
            RpgMakerLocationStep::DecodeJsonString => formatter.write_str("<json>")?,
        }
    }
    Ok(())
}

fn write_object_key(formatter: &mut fmt::Formatter<'_>, key: &str) -> fmt::Result {
    if is_simple_key(key) {
        write!(formatter, ".{key}")
    } else {
        write!(formatter, "[{:?}]", key)
    }
}

fn is_simple_key(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_non_standard_json_basename() {
        let file = DataFileName::parse("Disciplines.json").expect("精确 JSON 基名应合法");
        let location = RpgMakerLocation::value(
            RpgMakerSource::data_file(file),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("Name"),
            ],
        );
        assert_eq!(location.to_string(), "data/Disciplines.json[1].Name");
    }

    #[test]
    fn rejects_paths_and_non_json_file_names() {
        for value in [
            "",
            "../Actors.json",
            "folder/Actors.json",
            "Actors.JSON",
            "*.json",
            "NUL.json",
            "NUL.metadata.json",
            "COM1.any.json",
        ] {
            assert!(DataFileName::parse(value).is_err(), "{value:?} 应拒绝");
        }
    }
}
