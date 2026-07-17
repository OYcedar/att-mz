#![allow(dead_code, reason = "标准资产生产组合根尚未完整接线")]

//! MZ 五张标准资产表共享的存储语义。
//!
//! 这里只表达 Extract、Translate 与 WriteBack 共同依赖的表名、owner、
//! `text_body.unit_type`、领域组类型映射及结构化位置合法矩阵。SQL 和各用例的
//! 行解码规则仍由相应的读写边界负责。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

use super::text::{MzLocation, MzSource, StandardDataFile, TextGroupKind};

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

    /// 验证持久化表语义与结构化位置表达的是同一类标准资产。
    ///
    /// 字段名和 `Value` 的深层路径由提取规则与写回器各自负责；这里仅建立
    /// 五表存储种类、来源、位置变体及 Tag 容器之间的共享不变量。
    pub(crate) fn validate_locations(
        self,
        exact_location: &MzLocation,
        group_location: &MzLocation,
    ) -> Result<(), MzStandardAssetLocationError> {
        let (group_source, group_steps) = match group_location {
            MzLocation::Value { source, steps } => (source, steps),
            location => {
                return Err(MzStandardAssetLocationError::GroupLocationMustBeValue {
                    actual: location_kind(location),
                });
            }
        };

        let exact_source = exact_location.source();
        if exact_source != group_source {
            return Err(MzStandardAssetLocationError::DifferentSources {
                exact: exact_source.clone(),
                group: group_source.clone(),
            });
        }

        if !self.accepts_source(exact_source) {
            return Err(MzStandardAssetLocationError::SourceDoesNotMatchStorage {
                storage: self,
                source: exact_source.clone(),
            });
        }

        if !self.accepts_exact_location(exact_location) {
            return Err(
                MzStandardAssetLocationError::ExactLocationKindDoesNotMatchStorage {
                    storage: self,
                    actual: location_kind(exact_location),
                },
            );
        }

        match exact_location {
            MzLocation::NoteTag {
                container_steps, ..
            } if container_steps != group_steps => Err(
                MzStandardAssetLocationError::TagContainerDoesNotMatchGroup {
                    storage: self,
                    tag_kind: "NoteTag",
                },
            ),
            MzLocation::CommentTag { command_steps, .. } if command_steps != group_steps => Err(
                MzStandardAssetLocationError::TagContainerDoesNotMatchGroup {
                    storage: self,
                    tag_kind: "CommentTag",
                },
            ),
            _ => Ok(()),
        }
    }

    fn accepts_source(self, source: &MzSource) -> bool {
        match self {
            Self::Entry => match source {
                MzSource::Data(file) => *file != StandardDataFile::System,
                MzSource::Map(_) => true,
                MzSource::PluginParameter { .. } => false,
            },
            Self::SystemText => {
                matches!(source, MzSource::Data(StandardDataFile::System))
            }
            Self::MapText => matches!(source, MzSource::Map(_)),
            Self::Dialogue | Self::Choices | Self::ScrollingText => matches!(
                source,
                MzSource::Map(_)
                    | MzSource::Data(StandardDataFile::CommonEvents | StandardDataFile::Troops)
            ),
            Self::EventCommand => matches!(source, MzSource::Data(_) | MzSource::Map(_)),
            Self::PluginParam => matches!(source, MzSource::PluginParameter { .. }),
        }
    }

    fn accepts_exact_location(self, exact_location: &MzLocation) -> bool {
        match self {
            Self::Entry | Self::SystemText | Self::MapText => {
                matches!(
                    exact_location,
                    MzLocation::Value { .. } | MzLocation::NoteTag { .. }
                )
            }
            Self::Dialogue | Self::Choices | Self::ScrollingText | Self::PluginParam => {
                matches!(exact_location, MzLocation::Value { .. })
            }
            Self::EventCommand => matches!(
                exact_location,
                MzLocation::Value { .. } | MzLocation::CommentTag { .. }
            ),
        }
    }
}

/// 已能解码的位置与标准资产存储语义不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MzStandardAssetLocationError {
    GroupLocationMustBeValue {
        actual: &'static str,
    },
    DifferentSources {
        exact: MzSource,
        group: MzSource,
    },
    SourceDoesNotMatchStorage {
        storage: MzStandardAssetStorageKind,
        source: MzSource,
    },
    ExactLocationKindDoesNotMatchStorage {
        storage: MzStandardAssetStorageKind,
        actual: &'static str,
    },
    TagContainerDoesNotMatchGroup {
        storage: MzStandardAssetStorageKind,
        tag_kind: &'static str,
    },
}

impl fmt::Display for MzStandardAssetLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GroupLocationMustBeValue { actual } => {
                write!(formatter, "组位置必须是 Value，实际为 {actual}")
            }
            Self::DifferentSources { exact, group } => {
                write!(
                    formatter,
                    "精确位置来源 {exact} 与组位置来源 {group} 不一致"
                )
            }
            Self::SourceDoesNotMatchStorage { storage, source } => {
                write!(formatter, "存储种类 {storage:?} 不接受来源 {source}")
            }
            Self::ExactLocationKindDoesNotMatchStorage { storage, actual } => {
                write!(formatter, "存储种类 {storage:?} 不接受 {actual} 精确位置")
            }
            Self::TagContainerDoesNotMatchGroup { storage, tag_kind } => write!(
                formatter,
                "存储种类 {storage:?} 的 {tag_kind} 容器路径与组位置路径不一致"
            ),
        }
    }
}

impl Error for MzStandardAssetLocationError {}

fn location_kind(location: &MzLocation) -> &'static str {
    match location {
        MzLocation::Value { .. } => "Value",
        MzLocation::NoteTag { .. } => "NoteTag",
        MzLocation::CommentTag { .. } => "CommentTag",
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
    use crate::att_mz::text::MzLocationStep;

    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum ExactShape {
        Value,
        NoteTag,
        CommentTag,
    }

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

    #[test]
    fn storage_location_matrix_accepts_every_supported_source_and_exact_shape() {
        let mut cases = Vec::new();

        for file in StandardDataFile::ALL
            .into_iter()
            .filter(|file| *file != StandardDataFile::System)
        {
            for shape in [ExactShape::Value, ExactShape::NoteTag] {
                cases.push((
                    MzStandardAssetStorageKind::Entry,
                    MzSource::data(file),
                    shape,
                ));
            }
        }
        for shape in [ExactShape::Value, ExactShape::NoteTag] {
            cases.push((MzStandardAssetStorageKind::Entry, MzSource::map(7), shape));
            cases.push((
                MzStandardAssetStorageKind::SystemText,
                MzSource::data(StandardDataFile::System),
                shape,
            ));
            cases.push((MzStandardAssetStorageKind::MapText, MzSource::map(8), shape));
        }

        for storage in [
            MzStandardAssetStorageKind::Dialogue,
            MzStandardAssetStorageKind::Choices,
            MzStandardAssetStorageKind::ScrollingText,
        ] {
            for source in [
                MzSource::map(9),
                MzSource::data(StandardDataFile::CommonEvents),
                MzSource::data(StandardDataFile::Troops),
            ] {
                cases.push((storage, source, ExactShape::Value));
            }
        }

        for file in StandardDataFile::ALL {
            for shape in [ExactShape::Value, ExactShape::CommentTag] {
                cases.push((
                    MzStandardAssetStorageKind::EventCommand,
                    MzSource::data(file),
                    shape,
                ));
            }
        }
        for shape in [ExactShape::Value, ExactShape::CommentTag] {
            cases.push((
                MzStandardAssetStorageKind::EventCommand,
                MzSource::map(10),
                shape,
            ));
        }
        cases.push((
            MzStandardAssetStorageKind::PluginParam,
            MzSource::plugin_parameter(2, "Demo", "Config"),
            ExactShape::Value,
        ));

        for (storage, source, shape) in cases {
            let (exact, group) = locations(source.clone(), shape);
            assert_eq!(
                storage.validate_locations(&exact, &group),
                Ok(()),
                "{storage:?} 应接受 {source:?} 的 {shape:?}"
            );
        }
    }

    #[test]
    fn rules_entry_on_map_and_independent_value_paths_remain_valid() {
        let source = MzSource::map(12);
        let group = MzLocation::value(
            source.clone(),
            vec![MzLocationStep::key("events"), MzLocationStep::index(4)],
        );
        let exact = MzLocation::value(
            source,
            vec![
                MzLocationStep::key("custom_rules_path"),
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("arbitrary_field"),
            ],
        );

        assert_eq!(
            MzStandardAssetStorageKind::Entry.validate_locations(&exact, &group),
            Ok(())
        );
    }

    #[test]
    fn every_storage_kind_rejects_incompatible_sources() {
        let cases = [
            (
                MzStandardAssetStorageKind::Entry,
                MzSource::data(StandardDataFile::System),
            ),
            (
                MzStandardAssetStorageKind::SystemText,
                MzSource::data(StandardDataFile::Items),
            ),
            (
                MzStandardAssetStorageKind::MapText,
                MzSource::data(StandardDataFile::Items),
            ),
            (
                MzStandardAssetStorageKind::Dialogue,
                MzSource::data(StandardDataFile::Items),
            ),
            (
                MzStandardAssetStorageKind::Choices,
                MzSource::data(StandardDataFile::System),
            ),
            (
                MzStandardAssetStorageKind::ScrollingText,
                MzSource::plugin_parameter(1, "Demo", "Text"),
            ),
            (
                MzStandardAssetStorageKind::EventCommand,
                MzSource::plugin_parameter(1, "Demo", "Text"),
            ),
            (MzStandardAssetStorageKind::PluginParam, MzSource::map(1)),
        ];

        for (storage, source) in cases {
            let (exact, group) = locations(source.clone(), ExactShape::Value);
            assert!(matches!(
                storage.validate_locations(&exact, &group),
                Err(MzStandardAssetLocationError::SourceDoesNotMatchStorage {
                    storage: actual_storage,
                    source: actual_source,
                }) if actual_storage == storage && actual_source == source
            ));
        }
    }

    #[test]
    fn every_storage_kind_rejects_unsupported_exact_location_shapes() {
        let cases = [
            (
                MzStandardAssetStorageKind::Entry,
                MzSource::data(StandardDataFile::Items),
                ExactShape::CommentTag,
            ),
            (
                MzStandardAssetStorageKind::SystemText,
                MzSource::data(StandardDataFile::System),
                ExactShape::CommentTag,
            ),
            (
                MzStandardAssetStorageKind::MapText,
                MzSource::map(1),
                ExactShape::CommentTag,
            ),
            (
                MzStandardAssetStorageKind::Dialogue,
                MzSource::map(1),
                ExactShape::NoteTag,
            ),
            (
                MzStandardAssetStorageKind::Choices,
                MzSource::data(StandardDataFile::CommonEvents),
                ExactShape::CommentTag,
            ),
            (
                MzStandardAssetStorageKind::ScrollingText,
                MzSource::data(StandardDataFile::Troops),
                ExactShape::NoteTag,
            ),
            (
                MzStandardAssetStorageKind::EventCommand,
                MzSource::map(1),
                ExactShape::NoteTag,
            ),
            (
                MzStandardAssetStorageKind::PluginParam,
                MzSource::plugin_parameter(1, "Demo", "Text"),
                ExactShape::CommentTag,
            ),
        ];

        for (storage, source, shape) in cases {
            let (exact, group) = locations(source, shape);
            assert!(matches!(
                storage.validate_locations(&exact, &group),
                Err(
                    MzStandardAssetLocationError::ExactLocationKindDoesNotMatchStorage {
                        storage: actual_storage,
                        ..
                    }
                ) if actual_storage == storage
            ));
        }
    }

    #[test]
    fn group_must_be_value_and_both_locations_must_share_the_full_source() {
        let source = MzSource::data(StandardDataFile::Items);
        let exact = MzLocation::value(source.clone(), vec![MzLocationStep::key("name")]);
        let non_value_group = MzLocation::note_tag(source, Vec::new(), "Tag", 0);
        assert!(matches!(
            MzStandardAssetStorageKind::Entry.validate_locations(&exact, &non_value_group),
            Err(MzStandardAssetLocationError::GroupLocationMustBeValue { actual: "NoteTag" })
        ));

        let exact = MzLocation::value(MzSource::map(1), vec![MzLocationStep::key("name")]);
        let group = MzLocation::value(MzSource::map(2), Vec::new());
        assert!(matches!(
            MzStandardAssetStorageKind::Entry.validate_locations(&exact, &group),
            Err(MzStandardAssetLocationError::DifferentSources { exact, group })
                if exact == MzSource::map(1) && group == MzSource::map(2)
        ));
    }

    #[test]
    fn tag_container_steps_must_equal_group_value_steps() {
        let item_source = MzSource::data(StandardDataFile::Items);
        let item_group = MzLocation::value(item_source.clone(), vec![MzLocationStep::index(1)]);
        let note = MzLocation::note_tag(
            item_source,
            vec![MzLocationStep::index(2)],
            "DisplayName",
            0,
        );
        assert!(matches!(
            MzStandardAssetStorageKind::Entry.validate_locations(&note, &item_group),
            Err(
                MzStandardAssetLocationError::TagContainerDoesNotMatchGroup {
                    tag_kind: "NoteTag",
                    ..
                }
            )
        ));

        let map_source = MzSource::map(3);
        let command_group = MzLocation::value(
            map_source.clone(),
            vec![MzLocationStep::key("list"), MzLocationStep::index(4)],
        );
        let comment = MzLocation::comment_tag(
            map_source,
            vec![MzLocationStep::key("list"), MzLocationStep::index(5)],
            "Name",
            0,
        );
        assert!(matches!(
            MzStandardAssetStorageKind::EventCommand.validate_locations(&comment, &command_group),
            Err(
                MzStandardAssetLocationError::TagContainerDoesNotMatchGroup {
                    tag_kind: "CommentTag",
                    ..
                }
            )
        ));
    }

    fn locations(source: MzSource, shape: ExactShape) -> (MzLocation, MzLocation) {
        let group_steps = vec![MzLocationStep::key("rules_group"), MzLocationStep::index(3)];
        let group = MzLocation::value(source.clone(), group_steps.clone());
        let exact = match shape {
            ExactShape::Value => MzLocation::value(
                source,
                vec![
                    MzLocationStep::key("custom_rules_path"),
                    MzLocationStep::DecodeJsonString,
                    MzLocationStep::key("arbitrary_field"),
                ],
            ),
            ExactShape::NoteTag => MzLocation::note_tag(source, group_steps, "DisplayName", 0),
            ExactShape::CommentTag => {
                MzLocation::comment_tag(source, group_steps, "DisplayName", 0)
            }
        };
        (exact, group)
    }
}
