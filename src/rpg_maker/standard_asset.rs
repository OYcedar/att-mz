//! RPG Maker 标准文本资产的 owner、组语义、结构化位置约束与快照指纹 framing。
//!
//! SQL 三表及各用例的行解码规则由相应的读写边界负责。

use std::error::Error;
use std::fmt;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};

use super::text::{RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind};

/// `att.rpg_maker.standard_text_snapshot` 域指纹的唯一 framing 定义。
///
/// 写入方(Extract 资产存储)与校验方(WriteBack 资产读取)必须对同一份持久化
/// 内容产生逐字节一致的指纹,tag 布局因此只允许存在这一份实现。project_definition
/// 帧按"提供即掺入"编码:写入方传当前替换值,校验方按 owner 是否拥有项目级
/// 对话定义决定传 `Some`/`None`,两侧对同一 owner 的判断结果必须一致。
pub(crate) struct StandardTextSnapshotFingerprintBuilder {
    hasher: Sha256FramedHasher,
}

impl StandardTextSnapshotFingerprintBuilder {
    pub(crate) fn new(
        owner: RpgMakerStandardAssetOwner,
        project_definition_json: Option<&str>,
    ) -> Self {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
        hasher.frame(1, owner.storage_name().as_bytes());
        if let Some(project_definition_json) = project_definition_json {
            hasher
                .frame(14, b"project_definition")
                .frame(15, project_definition_json.as_bytes());
        }
        Self { hasher }
    }

    /// 按持久化自然顺序写入一个文本组;`group_order` 为组在 owner 分区内的序号。
    pub(crate) fn group(
        &mut self,
        group_location: &str,
        group_order: usize,
        group_kind: &str,
        projection_recipes_json: &str,
    ) {
        let group_order = u64::try_from(group_order).expect("group_order 必须可编码为 u64");
        self.hasher
            .frame(2, b"group")
            .frame(3, group_location.as_bytes())
            .frame(16, &group_order.to_le_bytes())
            .frame(4, group_kind.as_bytes())
            .frame(5, projection_recipes_json.as_bytes());
    }

    /// 按持久化自然顺序写入一个文本单元;`unit_order` 为单元在组内的序号。
    pub(crate) fn unit(
        &mut self,
        group_location: &str,
        unit_role: &str,
        unit_order: usize,
        source_content_json: &str,
        source_context_json: &str,
    ) {
        let unit_order = u64::try_from(unit_order).expect("unit_order 必须可编码为 u64");
        self.hasher
            .frame(6, b"unit")
            .frame(7, group_location.as_bytes())
            .frame(8, unit_role.as_bytes())
            .frame(17, &unit_order.to_le_bytes())
            .frame(9, source_content_json.as_bytes())
            .frame(10, source_context_json.as_bytes());
    }

    /// 按持久化自然顺序写入一个 Mutation Claim 摘要行。
    pub(crate) fn claim(&mut self, resource_key: &str, access: &str, group_location: &str) {
        self.hasher
            .frame(11, b"claim")
            .frame(12, resource_key.as_bytes())
            .frame(18, access.as_bytes())
            .frame(13, group_location.as_bytes());
    }

    pub(crate) fn finish(self) -> Sha256Fingerprint {
        self.hasher.finish()
    }
}

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
/// 文本组语义与来源之间的共享不变量。
pub(crate) fn validate_standard_text_locations(
    kind: TextGroupKind,
    exact_location: &RpgMakerLocation,
    group_location: &RpgMakerLocation,
) -> Result<(), RpgMakerStandardTextLocationError> {
    let group_source = group_location.source();
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

    Ok(())
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

/// 已能解码的位置与标准资产存储语义不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerStandardTextLocationError {
    DifferentSources {
        exact: RpgMakerSource,
        group: RpgMakerSource,
    },
    SourceDoesNotMatchKind {
        kind: TextGroupKind,
        source: RpgMakerSource,
    },
}

impl fmt::Display for RpgMakerStandardTextLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentSources { exact, group } => {
                write!(
                    formatter,
                    "精确位置来源 {exact} 与组位置来源 {group} 不一致"
                )
            }
            Self::SourceDoesNotMatchKind { kind, source } => {
                write!(formatter, "文本组 {kind:?} 不接受来源 {source}")
            }
        }
    }
}

impl Error for RpgMakerStandardTextLocationError {}

#[cfg(test)]
mod tests {
    use crate::rpg_maker::text::RpgMakerLocationStep;

    use super::*;

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
    fn text_group_location_matrix_accepts_every_supported_source() {
        let mut cases = Vec::new();

        for file in StandardDataFile::ALL
            .into_iter()
            .filter(|file| *file != StandardDataFile::System)
        {
            cases.push((TextGroupKind::DatabaseEntry, RpgMakerSource::data(file)));
        }
        cases.push((TextGroupKind::DatabaseEntry, RpgMakerSource::map(7)));
        cases.push((
            TextGroupKind::System,
            RpgMakerSource::data(StandardDataFile::System),
        ));
        cases.push((TextGroupKind::Map, RpgMakerSource::map(8)));

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
                cases.push((kind, source));
            }
        }

        for file in StandardDataFile::ALL {
            cases.push((TextGroupKind::EventCommand, RpgMakerSource::data(file)));
        }
        cases.push((TextGroupKind::EventCommand, RpgMakerSource::map(10)));
        cases.push((
            TextGroupKind::PluginParameter,
            RpgMakerSource::plugin_parameter(2, "Demo", "Config"),
        ));

        for (kind, source) in cases {
            let (exact, group) = locations(source.clone());
            assert_eq!(
                validate_standard_text_locations(kind, &exact, &group),
                Ok(()),
                "{kind:?} 应接受 {source:?}"
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
            let (exact, group) = locations(source.clone());
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
    fn both_locations_must_share_the_full_source() {
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

    fn locations(source: RpgMakerSource) -> (RpgMakerLocation, RpgMakerLocation) {
        let group = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::key("rules_group"),
                RpgMakerLocationStep::index(3),
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
        (exact, group)
    }
}
