//! MZ 固定提取与规则提取共用的复合文本快照模型。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

pub(crate) use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, TextGroupKind};

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

/// 一个标准资产 owner 拥有的完整当前快照。
///
/// 三种提取入口共同依赖这里的排序、同组字段合并与精确地址唯一性，owner 包装只负责
/// 在类型上表达本次提交属于谁，不各自实现快照规则。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardAssetSnapshot(Vec<ExtractedTextGroup>);

impl StandardAssetSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        normalize_groups(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        &self.0
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0
    }
}

/// 内置提取拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuiltinSnapshot(StandardAssetSnapshot);

impl BuiltinSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
    }
}

/// Rules 提取拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RulesSnapshot(StandardAssetSnapshot);

impl RulesSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(StandardAssetSnapshot(Vec::new()))
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
    }
}

/// Lua 提取拥有的完整当前快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LuaSnapshot(StandardAssetSnapshot);

impl LuaSnapshot {
    pub(crate) fn new(groups: Vec<ExtractedTextGroup>) -> Result<Self, SnapshotModelError> {
        StandardAssetSnapshot::new(groups).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(StandardAssetSnapshot(Vec::new()))
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[ExtractedTextGroup] {
        self.0.groups()
    }

    pub(crate) fn into_groups(self) -> Vec<ExtractedTextGroup> {
        self.0.into_groups()
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
    use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile};

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
