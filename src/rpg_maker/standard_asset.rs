//! RPG Maker 标准文本资产的 owner、组语义与结构化位置约束。
//!
//! SQL 三表及各用例的行解码规则由相应的读写边界负责。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

use super::text::{RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind};

/// 一个标准资产位置的提取所有者。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RpgMakerStandardAssetOwner {
    Builtin,
    Rules,
    Lua,
}

impl RpgMakerStandardAssetOwner {
    pub(crate) const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"builtin" => Some(Self::Builtin),
            b"rules" => Some(Self::Rules),
            b"lua" => Some(Self::Lua),
            _ => None,
        }
    }

    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Rules => "rules",
            Self::Lua => "lua",
        }
    }
}

/// 验证文本组语义与结构化位置表达的是同一类标准资产。
///
/// 字段名和 `Value` 的深层路径由提取规则与写回器各自负责；这里仅建立
/// 文本组语义、来源、位置变体及 Tag 容器之间的共享不变量。
pub(crate) fn validate_standard_text_locations(
    kind: TextGroupKind,
    exact_location: &RpgMakerLocation,
    group_location: &RpgMakerLocation,
) -> Result<(), RpgMakerStandardTextLocationError> {
    let (group_source, group_steps) = match group_location {
        RpgMakerLocation::Value { source, steps } => (source, steps),
        location => {
            return Err(
                RpgMakerStandardTextLocationError::GroupLocationMustBeValue {
                    actual: location_kind(location),
                },
            );
        }
    };

    let exact_source = exact_location.source();
    if exact_source != group_source {
        return Err(RpgMakerStandardTextLocationError::DifferentSources {
            exact: exact_source.clone(),
            group: group_source.clone(),
        });
    }

    if !accepts_source(kind, exact_source) {
        return Err(RpgMakerStandardTextLocationError::SourceDoesNotMatchKind {
            kind,
            source: exact_source.clone(),
        });
    }

    if !accepts_exact_location(kind, exact_location) {
        return Err(
            RpgMakerStandardTextLocationError::ExactLocationKindDoesNotMatchGroup {
                kind,
                actual: location_kind(exact_location),
            },
        );
    }

    match exact_location {
        RpgMakerLocation::NoteTag {
            container_steps, ..
        } if container_steps != group_steps => Err(
            RpgMakerStandardTextLocationError::TagContainerDoesNotMatchGroup {
                kind,
                tag_kind: "NoteTag",
            },
        ),
        RpgMakerLocation::CommentTag { command_steps, .. } if command_steps != group_steps => Err(
            RpgMakerStandardTextLocationError::TagContainerDoesNotMatchGroup {
                kind,
                tag_kind: "CommentTag",
            },
        ),
        _ => Ok(()),
    }
}

fn accepts_source(kind: TextGroupKind, source: &RpgMakerSource) -> bool {
    match kind {
        TextGroupKind::DatabaseEntry => match source {
            RpgMakerSource::Data(file) => *file != StandardDataFile::System,
            RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_) => true,
            RpgMakerSource::PluginParameter { .. } => false,
        },
        TextGroupKind::System => {
            matches!(source, RpgMakerSource::Data(StandardDataFile::System))
        }
        TextGroupKind::Map => matches!(source, RpgMakerSource::Map(_)),
        TextGroupKind::EventDialogue
        | TextGroupKind::EventChoices
        | TextGroupKind::EventScrollingText => matches!(
            source,
            RpgMakerSource::Map(_)
                | RpgMakerSource::Data(StandardDataFile::CommonEvents | StandardDataFile::Troops)
        ),
        TextGroupKind::EventCommand => matches!(
            source,
            RpgMakerSource::Data(_) | RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_)
        ),
        TextGroupKind::PluginParameter => {
            matches!(source, RpgMakerSource::PluginParameter { .. })
        }
    }
}

fn accepts_exact_location(kind: TextGroupKind, exact_location: &RpgMakerLocation) -> bool {
    match kind {
        TextGroupKind::DatabaseEntry | TextGroupKind::System | TextGroupKind::Map => matches!(
            exact_location,
            RpgMakerLocation::Value { .. } | RpgMakerLocation::NoteTag { .. }
        ),
        TextGroupKind::EventDialogue
        | TextGroupKind::EventChoices
        | TextGroupKind::EventScrollingText
        | TextGroupKind::PluginParameter => {
            matches!(exact_location, RpgMakerLocation::Value { .. })
        }
        TextGroupKind::EventCommand => matches!(
            exact_location,
            RpgMakerLocation::Value { .. } | RpgMakerLocation::CommentTag { .. }
        ),
    }
}

/// 已能解码的位置与标准资产存储语义不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerStandardTextLocationError {
    GroupLocationMustBeValue {
        actual: &'static str,
    },
    DifferentSources {
        exact: RpgMakerSource,
        group: RpgMakerSource,
    },
    SourceDoesNotMatchKind {
        kind: TextGroupKind,
        source: RpgMakerSource,
    },
    ExactLocationKindDoesNotMatchGroup {
        kind: TextGroupKind,
        actual: &'static str,
    },
    TagContainerDoesNotMatchGroup {
        kind: TextGroupKind,
        tag_kind: &'static str,
    },
}

impl fmt::Display for RpgMakerStandardTextLocationError {
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
            Self::SourceDoesNotMatchKind { kind, source } => {
                write!(formatter, "文本组 {kind:?} 不接受来源 {source}")
            }
            Self::ExactLocationKindDoesNotMatchGroup { kind, actual } => {
                write!(formatter, "文本组 {kind:?} 不接受 {actual} 精确位置")
            }
            Self::TagContainerDoesNotMatchGroup { kind, tag_kind } => write!(
                formatter,
                "文本组 {kind:?} 的 {tag_kind} 容器路径与组位置路径不一致"
            ),
        }
    }
}

impl Error for RpgMakerStandardTextLocationError {}

fn location_kind(location: &RpgMakerLocation) -> &'static str {
    match location {
        RpgMakerLocation::Value { .. } => "Value",
        RpgMakerLocation::NoteTag { .. } => "NoteTag",
        RpgMakerLocation::CommentTag { .. } => "CommentTag",
    }
}

/// 标准资产读取时每个 CPU 解码作业的逻辑叶上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerStandardAssetReadingConfig {
    leaves_per_decode_job: NonZeroUsize,
}

impl RpgMakerStandardAssetReadingConfig {
    pub(crate) const fn new(leaves_per_decode_job: NonZeroUsize) -> Self {
        Self {
            leaves_per_decode_job,
        }
    }

    pub(crate) const fn leaves_per_decode_job(self) -> NonZeroUsize {
        self.leaves_per_decode_job
    }
}

#[cfg(test)]
mod tests {
    use crate::rpg_maker::text::RpgMakerLocationStep;

    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum ExactShape {
        Value,
        NoteTag,
        CommentTag,
    }

    #[test]
    fn storage_names_accept_only_schema_values() {
        assert_eq!(
            RpgMakerStandardAssetOwner::from_storage_name("builtin"),
            Some(RpgMakerStandardAssetOwner::Builtin)
        );
        assert_eq!(
            RpgMakerStandardAssetOwner::from_storage_name("rules"),
            Some(RpgMakerStandardAssetOwner::Rules)
        );
        assert_eq!(
            RpgMakerStandardAssetOwner::from_storage_name("lua"),
            Some(RpgMakerStandardAssetOwner::Lua)
        );
        assert_eq!(
            RpgMakerStandardAssetOwner::from_storage_name("Builtin"),
            None
        );
        assert_eq!(RpgMakerStandardAssetOwner::from_storage_name("Lua"), None);
    }

    #[test]
    fn text_group_location_matrix_accepts_every_supported_source_and_exact_shape() {
        let mut cases = Vec::new();

        for file in StandardDataFile::ALL
            .into_iter()
            .filter(|file| *file != StandardDataFile::System)
        {
            for shape in [ExactShape::Value, ExactShape::NoteTag] {
                cases.push((
                    TextGroupKind::DatabaseEntry,
                    RpgMakerSource::data(file),
                    shape,
                ));
            }
        }
        for shape in [ExactShape::Value, ExactShape::NoteTag] {
            cases.push((TextGroupKind::DatabaseEntry, RpgMakerSource::map(7), shape));
            cases.push((
                TextGroupKind::System,
                RpgMakerSource::data(StandardDataFile::System),
                shape,
            ));
            cases.push((TextGroupKind::Map, RpgMakerSource::map(8), shape));
        }

        for kind in [
            TextGroupKind::EventDialogue,
            TextGroupKind::EventChoices,
            TextGroupKind::EventScrollingText,
        ] {
            for source in [
                RpgMakerSource::map(9),
                RpgMakerSource::data(StandardDataFile::CommonEvents),
                RpgMakerSource::data(StandardDataFile::Troops),
            ] {
                cases.push((kind, source, ExactShape::Value));
            }
        }

        for file in StandardDataFile::ALL {
            for shape in [ExactShape::Value, ExactShape::CommentTag] {
                cases.push((
                    TextGroupKind::EventCommand,
                    RpgMakerSource::data(file),
                    shape,
                ));
            }
        }
        for shape in [ExactShape::Value, ExactShape::CommentTag] {
            cases.push((TextGroupKind::EventCommand, RpgMakerSource::map(10), shape));
        }
        cases.push((
            TextGroupKind::PluginParameter,
            RpgMakerSource::plugin_parameter(2, "Demo", "Config"),
            ExactShape::Value,
        ));

        for (kind, source, shape) in cases {
            let (exact, group) = locations(source.clone(), shape);
            assert_eq!(
                validate_standard_text_locations(kind, &exact, &group),
                Ok(()),
                "{kind:?} 应接受 {source:?} 的 {shape:?}"
            );
        }
    }

    #[test]
    fn rules_entry_on_map_and_independent_value_paths_remain_valid() {
        let source = RpgMakerSource::map(12);
        let group = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(4),
            ],
        );
        let exact = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::key("custom_rules_path"),
                RpgMakerLocationStep::DecodeJsonString,
                RpgMakerLocationStep::key("arbitrary_field"),
            ],
        );

        assert_eq!(
            validate_standard_text_locations(TextGroupKind::DatabaseEntry, &exact, &group),
            Ok(())
        );
    }

    #[test]
    fn every_text_group_kind_rejects_incompatible_sources() {
        let cases = [
            (
                TextGroupKind::DatabaseEntry,
                RpgMakerSource::data(StandardDataFile::System),
            ),
            (
                TextGroupKind::System,
                RpgMakerSource::data(StandardDataFile::Items),
            ),
            (
                TextGroupKind::Map,
                RpgMakerSource::data(StandardDataFile::Items),
            ),
            (
                TextGroupKind::EventDialogue,
                RpgMakerSource::data(StandardDataFile::Items),
            ),
            (
                TextGroupKind::EventChoices,
                RpgMakerSource::data(StandardDataFile::System),
            ),
            (
                TextGroupKind::EventScrollingText,
                RpgMakerSource::plugin_parameter(1, "Demo", "Text"),
            ),
            (
                TextGroupKind::EventCommand,
                RpgMakerSource::plugin_parameter(1, "Demo", "Text"),
            ),
            (TextGroupKind::PluginParameter, RpgMakerSource::map(1)),
        ];

        for (kind, source) in cases {
            let (exact, group) = locations(source.clone(), ExactShape::Value);
            assert!(matches!(
                validate_standard_text_locations(kind, &exact, &group),
                Err(RpgMakerStandardTextLocationError::SourceDoesNotMatchKind {
                    kind: actual_kind,
                    source: actual_source,
                }) if actual_kind == kind && actual_source == source
            ));
        }
    }

    #[test]
    fn every_text_group_kind_rejects_unsupported_exact_location_shapes() {
        let cases = [
            (
                TextGroupKind::DatabaseEntry,
                RpgMakerSource::data(StandardDataFile::Items),
                ExactShape::CommentTag,
            ),
            (
                TextGroupKind::System,
                RpgMakerSource::data(StandardDataFile::System),
                ExactShape::CommentTag,
            ),
            (
                TextGroupKind::Map,
                RpgMakerSource::map(1),
                ExactShape::CommentTag,
            ),
            (
                TextGroupKind::EventDialogue,
                RpgMakerSource::map(1),
                ExactShape::NoteTag,
            ),
            (
                TextGroupKind::EventChoices,
                RpgMakerSource::data(StandardDataFile::CommonEvents),
                ExactShape::CommentTag,
            ),
            (
                TextGroupKind::EventScrollingText,
                RpgMakerSource::data(StandardDataFile::Troops),
                ExactShape::NoteTag,
            ),
            (
                TextGroupKind::EventCommand,
                RpgMakerSource::map(1),
                ExactShape::NoteTag,
            ),
            (
                TextGroupKind::PluginParameter,
                RpgMakerSource::plugin_parameter(1, "Demo", "Text"),
                ExactShape::CommentTag,
            ),
        ];

        for (kind, source, shape) in cases {
            let (exact, group) = locations(source, shape);
            assert!(matches!(
                validate_standard_text_locations(kind, &exact, &group),
                Err(
                    RpgMakerStandardTextLocationError::ExactLocationKindDoesNotMatchGroup {
                        kind: actual_kind,
                        ..
                    }
                ) if actual_kind == kind
            ));
        }
    }

    #[test]
    fn group_must_be_value_and_both_locations_must_share_the_full_source() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let exact =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key("name")]);
        let non_value_group = RpgMakerLocation::note_tag(source, Vec::new(), "Tag", 0);
        assert!(matches!(
            validate_standard_text_locations(
                TextGroupKind::DatabaseEntry,
                &exact,
                &non_value_group
            ),
            Err(RpgMakerStandardTextLocationError::GroupLocationMustBeValue { actual: "NoteTag" })
        ));

        let exact = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![RpgMakerLocationStep::key("name")],
        );
        let group = RpgMakerLocation::value(RpgMakerSource::map(2), Vec::new());
        assert!(matches!(
            validate_standard_text_locations(TextGroupKind::DatabaseEntry, &exact, &group),
            Err(RpgMakerStandardTextLocationError::DifferentSources { exact, group })
                if exact == RpgMakerSource::map(1) && group == RpgMakerSource::map(2)
        ));
    }

    #[test]
    fn tag_container_steps_must_equal_group_value_steps() {
        let item_source = RpgMakerSource::data(StandardDataFile::Items);
        let item_group =
            RpgMakerLocation::value(item_source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let note = RpgMakerLocation::note_tag(
            item_source,
            vec![RpgMakerLocationStep::index(2)],
            "DisplayName",
            0,
        );
        assert!(matches!(
            validate_standard_text_locations(TextGroupKind::DatabaseEntry, &note, &item_group),
            Err(
                RpgMakerStandardTextLocationError::TagContainerDoesNotMatchGroup {
                    tag_kind: "NoteTag",
                    ..
                }
            )
        ));

        let map_source = RpgMakerSource::map(3);
        let command_group = RpgMakerLocation::value(
            map_source.clone(),
            vec![
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(4),
            ],
        );
        let comment = RpgMakerLocation::comment_tag(
            map_source,
            vec![
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(5),
            ],
            "Name",
            0,
        );
        assert!(matches!(
            validate_standard_text_locations(TextGroupKind::EventCommand, &comment, &command_group),
            Err(
                RpgMakerStandardTextLocationError::TagContainerDoesNotMatchGroup {
                    tag_kind: "CommentTag",
                    ..
                }
            )
        ));
    }

    fn locations(
        source: RpgMakerSource,
        shape: ExactShape,
    ) -> (RpgMakerLocation, RpgMakerLocation) {
        let group_steps = vec![
            RpgMakerLocationStep::key("rules_group"),
            RpgMakerLocationStep::index(3),
        ];
        let group = RpgMakerLocation::value(source.clone(), group_steps.clone());
        let exact = match shape {
            ExactShape::Value => RpgMakerLocation::value(
                source,
                vec![
                    RpgMakerLocationStep::key("custom_rules_path"),
                    RpgMakerLocationStep::DecodeJsonString,
                    RpgMakerLocationStep::key("arbitrary_field"),
                ],
            ),
            ExactShape::NoteTag => {
                RpgMakerLocation::note_tag(source, group_steps, "DisplayName", 0)
            }
            ExactShape::CommentTag => {
                RpgMakerLocation::comment_tag(source, group_steps, "DisplayName", 0)
            }
        };
        (exact, group)
    }
}
