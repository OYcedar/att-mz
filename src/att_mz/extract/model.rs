#![allow(dead_code, reason = "提取快照模型尚未接入生产存储适配器")]

//! MZ 固定提取与规则提取共用的复合文本快照模型。
//!
//! 一个快照以语义相关的文本组承载翻译上下文，同时让组内每个叶子拥有独立、
//! 可稳定比较的精确地址。地址是权威身份；其 `Display` 只服务于诊断，不参与解析。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use super::document::StandardDataFile;

/// MZ 文本所在的物理来源。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MzSource {
    /// `data/` 下的一个标准 MZ 数据文件。
    Data(StandardDataFile),
    /// 一个标准 `data/MapNNN.json` 文件。
    Map(u32),
    /// `js/plugins.js` 中某个插件的一个参数。
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
    /// 当前字符串被当作嵌套 JSON 解码，后续步骤作用于解码后的值。
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
    /// 标准数据库对象 `note` 中简单 `<Tag:value>` 的值。
    NoteTag {
        source: MzSource,
        container_steps: Vec<MzLocationStep>,
        tag_name: String,
        occurrence: usize,
    },
    /// 一个连续 `108 + 408` 注释块中的简单标签值。
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

/// 一个可独立继承或清除译文的文本叶子。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextField {
    field_name: String,
    exact_location: MzLocation,
    original_text: String,
}

impl ExtractedTextField {
    pub(crate) fn new(
        field_name: impl Into<String>,
        exact_location: MzLocation,
        original_text: impl Into<String>,
    ) -> Result<Self, SnapshotModelError> {
        let field_name = field_name.into();
        let original_text = original_text.into();
        if field_name.is_empty() {
            return Err(SnapshotModelError::EmptyFieldName { exact_location });
        }
        if original_text.trim().is_empty() {
            return Err(SnapshotModelError::BlankOriginal { exact_location });
        }
        Ok(Self {
            field_name,
            exact_location,
            original_text,
        })
    }

    pub(crate) fn field_name(&self) -> &str {
        &self.field_name
    }

    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }
}

/// 一个会作为最小翻译上下文共同送给翻译器的复合文本组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedTextGroup {
    kind: TextGroupKind,
    group_location: MzLocation,
    fields: Vec<ExtractedTextField>,
}

impl ExtractedTextGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: MzLocation,
        mut fields: Vec<ExtractedTextField>,
    ) -> Result<Self, SnapshotModelError> {
        if fields.is_empty() {
            return Err(SnapshotModelError::EmptyGroup { group_location });
        }
        fields.sort_by(|left, right| left.exact_location.cmp(&right.exact_location));
        for pair in fields.windows(2) {
            if pair[0].exact_location == pair[1].exact_location {
                return Err(SnapshotModelError::DuplicateLocation {
                    exact_location: pair[0].exact_location.clone(),
                });
            }
        }
        Ok(Self {
            kind,
            group_location,
            fields,
        })
    }

    pub(crate) fn kind(&self) -> TextGroupKind {
        self.kind
    }

    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

    pub(crate) fn fields(&self) -> &[ExtractedTextField] {
        &self.fields
    }
}

/// 内置提取拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuiltinSnapshot(Vec<ExtractedTextGroup>);

impl BuiltinSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        normalize_groups(groups).map(Self)
    }

    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        &self.0
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0
    }
}

/// Rules 提取拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RulesSnapshot(Vec<ExtractedTextGroup>);

impl RulesSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        normalize_groups(groups).map(Self)
    }

    pub(crate) fn empty() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        &self.0
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0
    }
}

fn normalize_groups(
    groups: Vec<ExtractedTextGroup>,
) -> Result<Vec<ExtractedTextGroup>, SnapshotModelError> {
    let mut merged = BTreeMap::<(MzLocation, TextGroupKind), Vec<ExtractedTextField>>::new();
    for group in groups {
        merged
            .entry((group.group_location, group.kind))
            .or_default()
            .extend(group.fields);
    }

    let mut groups = Vec::with_capacity(merged.len());
    for ((group_location, kind), fields) in merged {
        groups.push(ExtractedTextGroup::new(kind, group_location, fields)?);
    }

    let mut exact_locations = BTreeSet::new();
    for group in &groups {
        for field in &group.fields {
            if !exact_locations.insert(field.exact_location.clone()) {
                return Err(SnapshotModelError::DuplicateLocation {
                    exact_location: field.exact_location.clone(),
                });
            }
        }
    }
    Ok(groups)
}

/// 构造受信快照时发现的内部模型错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotModelError {
    EmptyFieldName { exact_location: MzLocation },
    BlankOriginal { exact_location: MzLocation },
    EmptyGroup { group_location: MzLocation },
    DuplicateLocation { exact_location: MzLocation },
}

impl fmt::Display for SnapshotModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFieldName { exact_location } => {
                write!(formatter, "文本字段名为空：{exact_location}")
            }
            Self::BlankOriginal { exact_location } => {
                write!(formatter, "纯空白原文不应进入快照：{exact_location}")
            }
            Self::EmptyGroup { group_location } => {
                write!(formatter, "复合文本组不包含任何文本：{group_location}")
            }
            Self::DuplicateLocation { exact_location } => {
                write!(formatter, "快照包含重复文本地址：{exact_location}")
            }
        }
    }
}

impl Error for SnapshotModelError {}

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
        let note = MzLocation::note_tag(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(10)],
            "Category",
            0,
        );

        assert_eq!(item.to_string(), "data/Items.json[10].description");
        assert_eq!(
            plugin.to_string(),
            "plugins.js[QuestMenu].Categories<json>[2]<json>.Name"
        );
        assert_eq!(note.to_string(), "data/Items.json[10].note#Category[0]");
    }

    #[test]
    fn snapshot_sorts_groups_and_rejects_duplicate_leaf_addresses() {
        let location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(1), MzLocationStep::key("name")],
        );
        let group_location = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(1)],
        );
        let field =
            ExtractedTextField::new("name", location.clone(), "宝剑").expect("非空字段应该合法");
        let group =
            ExtractedTextGroup::new(TextGroupKind::DatabaseEntry, group_location, vec![field])
                .expect("非空组应该合法");

        let error =
            BuiltinSnapshot::new(vec![group.clone(), group]).expect_err("同一具体地址只能出现一次");

        assert_eq!(
            error,
            SnapshotModelError::DuplicateLocation {
                exact_location: location
            }
        );
    }

    #[test]
    fn snapshot_merges_fields_that_belong_to_the_same_semantic_group() {
        let source = MzSource::data(StandardDataFile::Items);
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let name = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            vec![
                ExtractedTextField::new(
                    "name",
                    MzLocation::value(
                        source.clone(),
                        vec![MzLocationStep::index(1), MzLocationStep::key("name")],
                    ),
                    "宝剑",
                )
                .expect("名称字段应合法"),
            ],
        )
        .expect("名称组应合法");
        let description = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![
                ExtractedTextField::new(
                    "description",
                    MzLocation::value(
                        source,
                        vec![MzLocationStep::index(1), MzLocationStep::key("description")],
                    ),
                    "锋利的宝剑",
                )
                .expect("说明字段应合法"),
            ],
        )
        .expect("说明组应合法");

        let snapshot =
            RulesSnapshot::new(vec![name, description]).expect("同一对象的不同字段应该合并");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].fields().len(), 2);
    }
}
