#![allow(dead_code, reason = "MZ 共享文本模型仍有直接依赖尚未生产接线")]

//! MZ 提取、翻译和写回共同使用的结构化文本身份。
//!
//! 这些类型只表达 MZ 领域事实。结构化位置是权威身份，`Display` 仅用于诊断，
//! 不能作为持久化格式重新解析。

use std::fmt;

/// RPG Maker MZ 在 `data/` 中定义的标准数据文件。
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

/// MZ 文本所在的物理来源。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzSource {
    Data(StandardDataFile),
    Map(u32),
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl MzSource {
    pub(crate) fn data(file: StandardDataFile) -> Self {
        Self::Data(file)
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

impl fmt::Display for MzSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(file) => write!(formatter, "data/{}", file.file_name()),
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
pub(crate) enum MzLocationStep {
    ObjectKey(String),
    ArrayIndex(usize),
    DecodeJsonString,
}

impl MzLocationStep {
    pub(crate) fn key(value: impl Into<String>) -> Self {
        Self::ObjectKey(value.into())
    }

    pub(crate) fn index(value: usize) -> Self {
        Self::ArrayIndex(value)
    }
}

/// 一个文本叶子的结构化权威地址。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzLocation {
    Value {
        source: MzSource,
        steps: Vec<MzLocationStep>,
    },
    NoteTag {
        source: MzSource,
        container_steps: Vec<MzLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: MzSource,
        command_steps: Vec<MzLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
}

impl MzLocation {
    pub(crate) fn value(source: MzSource, steps: Vec<MzLocationStep>) -> Self {
        Self::Value { source, steps }
    }

    pub(crate) fn note_tag(
        source: MzSource,
        container_steps: Vec<MzLocationStep>,
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
        source: MzSource,
        command_steps: Vec<MzLocationStep>,
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

    pub(crate) fn source(&self) -> &MzSource {
        match self {
            Self::Value { source, .. }
            | Self::NoteTag { source, .. }
            | Self::CommentTag { source, .. } => source,
        }
    }

    pub(crate) fn steps(&self) -> &[MzLocationStep] {
        match self {
            Self::Value { steps, .. } => steps,
            Self::NoteTag {
                container_steps, ..
            } => container_steps,
            Self::CommentTag { command_steps, .. } => command_steps,
        }
    }
}

impl fmt::Display for MzLocation {
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

/// 文本组所表达的 MZ 语义对象。
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
    source: &MzSource,
    steps: &[MzLocationStep],
) -> fmt::Result {
    write!(formatter, "{source}")?;
    for step in steps {
        match step {
            MzLocationStep::ObjectKey(key) => write_object_key(formatter, key)?,
            MzLocationStep::ArrayIndex(index) => write!(formatter, "[{index}]")?,
            MzLocationStep::DecodeJsonString => formatter.write_str("<json>")?,
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
    fn renders_diagnostic_paths_without_making_them_authoritative() {
        let item = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![
                MzLocationStep::index(10),
                MzLocationStep::key("description"),
            ],
        );
        let plugin = MzLocation::value(
            MzSource::plugin_parameter(3, "QuestMenu", "Categories"),
            vec![
                MzLocationStep::DecodeJsonString,
                MzLocationStep::index(2),
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("Name"),
            ],
        );

        assert_eq!(item.to_string(), "data/Items.json[10].description");
        assert_eq!(
            plugin.to_string(),
            "plugins.js[QuestMenu].Categories<json>[2]<json>.Name"
        );
    }
}
