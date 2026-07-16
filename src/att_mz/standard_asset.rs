#![allow(dead_code, reason = "标准资产生产组合根尚未完整接线")]

//! MZ 五张标准资产表共享的存储语义。
//!
//! 这里只表达 Extract、Translate 与 WriteBack 共同依赖的表名、owner、
//! `text_body.unit_type` 以及它们与领域组类型的唯一映射。SQL 和各用例的
//! 行解码规则仍由相应的读写边界负责。

use std::num::NonZeroUsize;

use super::text::TextGroupKind;

/// 一个标准资产位置的提取所有者。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MzStandardAssetOwner {
    Builtin,
    Rules,
}

impl MzStandardAssetOwner {
    pub(crate) const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"builtin" => Some(Self::Builtin),
            b"rules" => Some(Self::Rules),
            _ => None,
        }
    }

    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Rules => "rules",
        }
    }
}

/// SQLite 中承载 MZ 标准资产的五张表。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzStandardAssetTable {
    Entry,
    SystemText,
    MapText,
    TextBody,
    PluginParam,
}

impl MzStandardAssetTable {
    pub(crate) const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"entry" => Some(Self::Entry),
            b"system_text" => Some(Self::SystemText),
            b"map_text" => Some(Self::MapText),
            b"text_body" => Some(Self::TextBody),
            b"plugin_param" => Some(Self::PluginParam),
            _ => None,
        }
    }

    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::SystemText => "system_text",
            Self::MapText => "map_text",
            Self::TextBody => "text_body",
            Self::PluginParam => "plugin_param",
        }
    }
}

/// `text_body` 中区分事件文本语义的受信单元类型。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzTextBodyUnit {
    Dialogue,
    Choices,
    ScrollingText,
    EventCommand,
}

impl MzTextBodyUnit {
    pub(crate) const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"dialogue" => Some(Self::Dialogue),
            b"choices" => Some(Self::Choices),
            b"scrolling_text" => Some(Self::ScrollingText),
            b"event_command" => Some(Self::EventCommand),
            _ => None,
        }
    }

    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Dialogue => "dialogue",
            Self::Choices => "choices",
            Self::ScrollingText => "scrolling_text",
            Self::EventCommand => "event_command",
        }
    }
}

/// 五张表与 `unit_type` 能够表达的八种合法存储语义。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzStandardAssetStorageKind {
    Entry,
    SystemText,
    MapText,
    Dialogue,
    Choices,
    ScrollingText,
    EventCommand,
    PluginParam,
}

impl MzStandardAssetStorageKind {
    /// 把表名与可选 `unit_type` 转换为唯一合法组合。
    pub(crate) const fn from_parts(
        table: MzStandardAssetTable,
        unit_type: Option<MzTextBodyUnit>,
    ) -> Option<Self> {
        match (table, unit_type) {
            (MzStandardAssetTable::Entry, None) => Some(Self::Entry),
            (MzStandardAssetTable::SystemText, None) => Some(Self::SystemText),
            (MzStandardAssetTable::MapText, None) => Some(Self::MapText),
            (MzStandardAssetTable::TextBody, Some(MzTextBodyUnit::Dialogue)) => {
                Some(Self::Dialogue)
            }
            (MzStandardAssetTable::TextBody, Some(MzTextBodyUnit::Choices)) => Some(Self::Choices),
            (MzStandardAssetTable::TextBody, Some(MzTextBodyUnit::ScrollingText)) => {
                Some(Self::ScrollingText)
            }
            (MzStandardAssetTable::TextBody, Some(MzTextBodyUnit::EventCommand)) => {
                Some(Self::EventCommand)
            }
            (MzStandardAssetTable::PluginParam, None) => Some(Self::PluginParam),
            _ => None,
        }
    }

    pub(crate) const fn for_group_kind(kind: TextGroupKind) -> Self {
        match kind {
            TextGroupKind::DatabaseEntry => Self::Entry,
            TextGroupKind::System => Self::SystemText,
            TextGroupKind::Map => Self::MapText,
            TextGroupKind::EventDialogue => Self::Dialogue,
            TextGroupKind::EventChoices => Self::Choices,
            TextGroupKind::EventScrollingText => Self::ScrollingText,
            TextGroupKind::EventCommand => Self::EventCommand,
            TextGroupKind::PluginParameter => Self::PluginParam,
        }
    }

    pub(crate) const fn group_kind(self) -> TextGroupKind {
        match self {
            Self::Entry => TextGroupKind::DatabaseEntry,
            Self::SystemText => TextGroupKind::System,
            Self::MapText => TextGroupKind::Map,
            Self::Dialogue => TextGroupKind::EventDialogue,
            Self::Choices => TextGroupKind::EventChoices,
            Self::ScrollingText => TextGroupKind::EventScrollingText,
            Self::EventCommand => TextGroupKind::EventCommand,
            Self::PluginParam => TextGroupKind::PluginParameter,
        }
    }

    pub(crate) const fn table(self) -> MzStandardAssetTable {
        match self {
            Self::Entry => MzStandardAssetTable::Entry,
            Self::SystemText => MzStandardAssetTable::SystemText,
            Self::MapText => MzStandardAssetTable::MapText,
            Self::Dialogue | Self::Choices | Self::ScrollingText | Self::EventCommand => {
                MzStandardAssetTable::TextBody
            }
            Self::PluginParam => MzStandardAssetTable::PluginParam,
        }
    }

    pub(crate) const fn unit_type(self) -> Option<MzTextBodyUnit> {
        match self {
            Self::Dialogue => Some(MzTextBodyUnit::Dialogue),
            Self::Choices => Some(MzTextBodyUnit::Choices),
            Self::ScrollingText => Some(MzTextBodyUnit::ScrollingText),
            Self::EventCommand => Some(MzTextBodyUnit::EventCommand),
            Self::Entry | Self::SystemText | Self::MapText | Self::PluginParam => None,
        }
    }
}

/// 标准资产读取和 CPU 解码阶段的全部必填资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MzStandardAssetReadingConfig {
    decode_concurrency: NonZeroUsize,
    leaves_per_decode_job: NonZeroUsize,
}

impl MzStandardAssetReadingConfig {
    pub(crate) const fn new(
        decode_concurrency: NonZeroUsize,
        leaves_per_decode_job: NonZeroUsize,
    ) -> Self {
        Self {
            decode_concurrency,
            leaves_per_decode_job,
        }
    }

    pub(crate) const fn decode_concurrency(self) -> NonZeroUsize {
        self.decode_concurrency
    }

    pub(crate) const fn leaves_per_decode_job(self) -> NonZeroUsize {
        self.leaves_per_decode_job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_kind_round_trips_through_one_storage_shape() {
        for kind in [
            TextGroupKind::DatabaseEntry,
            TextGroupKind::System,
            TextGroupKind::Map,
            TextGroupKind::EventDialogue,
            TextGroupKind::EventChoices,
            TextGroupKind::EventScrollingText,
            TextGroupKind::EventCommand,
            TextGroupKind::PluginParameter,
        ] {
            let storage = MzStandardAssetStorageKind::for_group_kind(kind);
            assert_eq!(storage.group_kind(), kind);
            assert_eq!(
                MzStandardAssetStorageKind::from_parts(storage.table(), storage.unit_type()),
                Some(storage)
            );
        }
    }

    #[test]
    fn storage_names_accept_only_schema_values() {
        assert_eq!(
            MzStandardAssetOwner::from_storage_name("builtin"),
            Some(MzStandardAssetOwner::Builtin)
        );
        assert_eq!(
            MzStandardAssetTable::from_storage_name("text_body"),
            Some(MzStandardAssetTable::TextBody)
        );
        assert_eq!(
            MzTextBodyUnit::from_storage_name("scrolling_text"),
            Some(MzTextBodyUnit::ScrollingText)
        );
        assert_eq!(MzStandardAssetOwner::from_storage_name("Builtin"), None);
        assert_eq!(MzStandardAssetTable::from_storage_name("terminology"), None);
        assert_eq!(MzTextBodyUnit::from_storage_name("dialog"), None);
    }
}
