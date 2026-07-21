//! RPG Maker 提取、翻译和写回共同使用的结构化物理位置。
//!
//! 结构化位置是权威身份，`Display` 只用于诊断，不作为持久化协议解析。

use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;

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
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
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

/// RPG Maker 规范 Map 文件能够表达的正整数地图身份。
///
/// 文件名只在这个类型中规范化：小于三位时补零，三位及以上保持十进制原样；
/// `Map000.json` 与带冗余前导零的名称都不是 Map 身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MapId(NonZeroU32);

impl MapId {
    pub(crate) fn new(value: u32) -> Result<Self, MapIdError> {
        NonZeroU32::new(value).map(Self).ok_or(MapIdError { value })
    }

    pub(crate) const fn get(self) -> u32 {
        self.0.get()
    }

    pub(crate) fn from_canonical_file_name(file_name: &str) -> Option<Self> {
        let digits = file_name.strip_prefix("Map")?.strip_suffix(".json")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let id = Self::new(digits.parse().ok()?).ok()?;
        (id.file_name() == file_name).then_some(id)
    }

    pub(crate) fn file_name(self) -> String {
        format!("Map{:03}.json", self.get())
    }
}

impl fmt::Display for MapId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MapIdError {
    value: u32,
}

impl fmt::Display for MapIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "RPG Maker map ID 必须是正 u32 整数，实际为 {}",
            self.value
        )
    }
}

impl Error for MapIdError {}

/// RPG Maker 文本所在的物理来源。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerSource {
    Data(StandardDataFile),
    DataFile(DataFileName),
    Map(MapId),
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
        if let Some(standard) = StandardDataFile::from_file_name(file.as_str()) {
            Self::Data(standard)
        } else if let Some(map_id) = MapId::from_canonical_file_name(file.as_str()) {
            Self::map_id(map_id)
        } else {
            Self::DataFile(file)
        }
    }

    #[cfg(test)]
    pub(crate) fn map(map_id: u32) -> Self {
        Self::map_id(MapId::new(map_id).expect("RPG Maker map 来源必须使用正整数身份"))
    }

    pub(crate) fn map_id(map_id: MapId) -> Self {
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
            Self::Map(map_id) => write!(formatter, "data/{}", map_id.file_name()),
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
            "COM¹.json",
            "com².metadata.json",
            "LPT³.json",
        ] {
            assert!(DataFileName::parse(value).is_err(), "{value:?} 应拒绝");
        }
    }

    #[test]
    fn canonical_map_names_require_positive_ids_without_redundant_zeroes() {
        for (file_name, expected) in [
            ("Map001.json", 1),
            ("Map042.json", 42),
            ("Map999.json", 999),
            ("Map1000.json", 1000),
        ] {
            let map_id = MapId::from_canonical_file_name(file_name).expect("规范 Map 名应识别");
            assert_eq!(map_id.get(), expected);
            assert_eq!(map_id.file_name(), file_name);
        }

        for file_name in [
            "Map000.json",
            "Map00.json",
            "Map01.json",
            "Map0001.json",
            "Map.json",
            "map001.json",
            "MapInfos.json",
        ] {
            assert_eq!(
                MapId::from_canonical_file_name(file_name),
                None,
                "{file_name}"
            );
        }
        assert!(MapId::new(0).is_err());
    }

    #[test]
    fn data_file_classification_keeps_noncanonical_map_names_custom() {
        assert_eq!(
            RpgMakerSource::data_file(DataFileName::parse("Actors.json").unwrap()),
            RpgMakerSource::Data(StandardDataFile::Actors)
        );
        assert_eq!(
            RpgMakerSource::data_file(DataFileName::parse("Map001.json").unwrap()),
            RpgMakerSource::Map(MapId::new(1).unwrap())
        );
        for file_name in ["Map000.json", "Map01.json", "Map0001.json"] {
            assert!(matches!(
                RpgMakerSource::data_file(DataFileName::parse(file_name).unwrap()),
                RpgMakerSource::DataFile(file) if file.as_str() == file_name
            ));
        }
    }
}
