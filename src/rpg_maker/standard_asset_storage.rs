//! RPG Maker 标准资产表的共享存储投影与原始行解码。
//!
//! 本模块只拥有 SQLite 行如何还原为标准资产的共同事实。Translate 与 WriteBack
//! 仍分别拥有查询条件、阶段特有列、快照新鲜度、领域校验、组装和错误语义。

use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::sqlite::{SqliteRow, SqliteValue};

/// owner 状态查询的唯一列顺序。
pub(crate) const STANDARD_ASSET_OWNER_STATE_PROJECTION: &str =
    "owner,\n    source_snapshot_fingerprint,\n    asset_snapshot_fingerprint";

/// 标准文本组在各读取阶段共同消费的列顺序。
pub(crate) const STANDARD_TEXT_GROUP_CORE_PROJECTION: &str =
    "group_location,\n    group_kind,\n    group_order";

/// 标准文本单元位置列；Translate 会在其后插入阶段特有的组事实。
pub(crate) const STANDARD_TEXT_UNIT_LOCATION_PROJECTION: &str = "unit.group_location";

/// 标准文本单元在位置之后由各读取阶段共同消费的列顺序。
pub(crate) const STANDARD_TEXT_UNIT_CONTENT_PROJECTION: &str = "unit.unit_role,\n    \
     unit.unit_order,\n    \
     unit.source_content_json,\n    \
     unit.source_context_json,\n    \
     unit.translation_content_json";

/// 标准资产跨阶段唯一的 owner 自然顺序。
pub(crate) const STANDARD_ASSET_OWNER_ORDER: [RpgMakerStandardAssetOwner; 3] = [
    RpgMakerStandardAssetOwner::Builtin,
    RpgMakerStandardAssetOwner::Rules,
    RpgMakerStandardAssetOwner::Lua,
];

pub(crate) const fn standard_asset_owner_order(owner: RpgMakerStandardAssetOwner) -> usize {
    match owner {
        RpgMakerStandardAssetOwner::Builtin => 0,
        RpgMakerStandardAssetOwner::Rules => 1,
        RpgMakerStandardAssetOwner::Lua => 2,
    }
}

/// 构造与 [`STANDARD_ASSET_OWNER_ORDER`] 一致的 SQLite `CASE` 排序表达式。
pub(crate) fn standard_asset_owner_order_sql(column: &str) -> String {
    let mut expression = format!("CASE {column}");
    for owner in STANDARD_ASSET_OWNER_ORDER {
        expression.push_str(" WHEN '");
        expression.push_str(owner.storage_name());
        expression.push_str("' THEN ");
        expression.push(char::from(
            b'0' + u8::try_from(standard_asset_owner_order(owner))
                .expect("标准资产 owner 闭集顺序必须是单个十进制数字"),
        ));
    }
    expression.push_str(" END");
    expression
}

/// 一条由分区查询建立 owner 身份的标准资产行。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OwnerPartitionedSqliteRow {
    pub(crate) owner: RpgMakerStandardAssetOwner,
    pub(crate) row: SqliteRow,
}

/// 按唯一 owner 自然顺序归并三个窄查询分区。
pub(crate) fn merge_owner_partitions(
    partitions: [Vec<SqliteRow>; STANDARD_ASSET_OWNER_ORDER.len()],
) -> Vec<OwnerPartitionedSqliteRow> {
    let capacity = partitions.iter().map(Vec::len).sum();
    let mut merged = Vec::with_capacity(capacity);
    for (owner, partition) in STANDARD_ASSET_OWNER_ORDER.into_iter().zip(partitions) {
        merged.extend(
            partition
                .into_iter()
                .map(|row| OwnerPartitionedSqliteRow { owner, row }),
        );
    }
    merged
}

/// 把 owner 状态行按标准资产自然顺序排序；无法识别的行稳定排在闭集之后。
pub(crate) fn sort_owner_state_rows(rows: &mut [SqliteRow]) {
    rows.sort_by_key(|row| {
        row.values()
            .first()
            .and_then(|value| match value {
                SqliteValue::Text(owner) => {
                    RpgMakerStandardAssetOwner::from_storage_name(owner.as_str())
                }
                _ => None,
            })
            .map_or(STANDARD_ASSET_OWNER_ORDER.len(), standard_asset_owner_order)
    });
}

#[derive(Debug)]
pub(crate) enum StandardAssetStorageRowError {
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    InvalidOrderValue {
        column: &'static str,
        actual: i64,
    },
    UnknownOwner(String),
    UnknownGroupKind(String),
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidRole(RpgMakerProjectionCodecError),
    InvalidSourceContent(serde_json::Error),
    InvalidTranslationContent(serde_json::Error),
}

/// 已验证列数、按共享投影顺序消费拥有型值的原始行解码器。
pub(crate) struct StandardAssetStorageRowDecoder {
    values: std::vec::IntoIter<SqliteValue>,
}

impl StandardAssetStorageRowDecoder {
    pub(crate) fn new(
        row: SqliteRow,
        expected_columns: usize,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let values = row.into_values();
        let actual = values.len();
        if actual != expected_columns {
            return Err(StandardAssetStorageRowError::WrongColumnCount {
                expected: expected_columns,
                actual,
            });
        }
        Ok(Self {
            values: values.into_iter(),
        })
    }

    pub(crate) fn required_text(
        &mut self,
        column: &'static str,
    ) -> Result<String, StandardAssetStorageRowError> {
        match self.next() {
            SqliteValue::Text(value) => Ok(value),
            actual => Err(StandardAssetStorageRowError::WrongColumnType {
                column,
                expected: "TEXT",
                actual: actual.kind_name(),
            }),
        }
    }

    pub(crate) fn optional_text(
        &mut self,
        column: &'static str,
    ) -> Result<Option<String>, StandardAssetStorageRowError> {
        match self.next() {
            SqliteValue::Null => Ok(None),
            SqliteValue::Text(value) => Ok(Some(value)),
            actual => Err(StandardAssetStorageRowError::WrongColumnType {
                column,
                expected: "TEXT 或 NULL",
                actual: actual.kind_name(),
            }),
        }
    }

    pub(crate) fn required_blob(
        &mut self,
        column: &'static str,
    ) -> Result<Vec<u8>, StandardAssetStorageRowError> {
        match self.next() {
            SqliteValue::Blob(value) => Ok(value),
            actual => Err(StandardAssetStorageRowError::WrongColumnType {
                column,
                expected: "BLOB",
                actual: actual.kind_name(),
            }),
        }
    }

    pub(crate) fn optional_blob(
        &mut self,
        column: &'static str,
    ) -> Result<Option<Vec<u8>>, StandardAssetStorageRowError> {
        match self.next() {
            SqliteValue::Null => Ok(None),
            SqliteValue::Blob(value) => Ok(Some(value)),
            actual => Err(StandardAssetStorageRowError::WrongColumnType {
                column,
                expected: "BLOB 或 NULL",
                actual: actual.kind_name(),
            }),
        }
    }

    pub(crate) fn non_negative_order(
        &mut self,
        column: &'static str,
    ) -> Result<usize, StandardAssetStorageRowError> {
        let value = match self.next() {
            SqliteValue::Integer(value) => value,
            actual => {
                return Err(StandardAssetStorageRowError::WrongColumnType {
                    column,
                    expected: "INTEGER",
                    actual: actual.kind_name(),
                });
            }
        };
        usize::try_from(value).map_err(|_| StandardAssetStorageRowError::InvalidOrderValue {
            column,
            actual: value,
        })
    }

    fn next(&mut self) -> SqliteValue {
        self.values
            .next()
            .expect("列数已验证，标准资产存储行必须具有完整投影")
    }
}

pub(crate) struct StandardAssetOwnerStateStorageRow {
    pub(crate) owner_name: String,
    pub(crate) owner: RpgMakerStandardAssetOwner,
    pub(crate) source_snapshot_fingerprint: Vec<u8>,
    pub(crate) asset_snapshot_fingerprint: Vec<u8>,
}

impl StandardAssetOwnerStateStorageRow {
    pub(crate) fn decode(row: SqliteRow) -> Result<Self, StandardAssetStorageRowError> {
        let mut row = StandardAssetStorageRowDecoder::new(row, 3)?;
        let owner_name = row.required_text("owner")?;
        let owner = RpgMakerStandardAssetOwner::from_storage_name(owner_name.as_str())
            .ok_or_else(|| StandardAssetStorageRowError::UnknownOwner(owner_name.clone()))?;
        Ok(Self {
            owner_name,
            owner,
            source_snapshot_fingerprint: row.required_blob("source_snapshot_fingerprint")?,
            asset_snapshot_fingerprint: row.required_blob("asset_snapshot_fingerprint")?,
        })
    }
}

pub(crate) struct StandardTextGroupStorageRow {
    pub(crate) group_location_raw: String,
    pub(crate) group_location: RpgMakerLocation,
    pub(crate) group_kind_raw: String,
    pub(crate) kind: TextGroupKind,
    pub(crate) group_order: usize,
}

impl StandardTextGroupStorageRow {
    pub(crate) fn decode(
        row: &mut StandardAssetStorageRowDecoder,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let group_location_raw = row.required_text("group_location")?;
        let group_location = RpgMakerLocationCodec::decode(group_location_raw.as_str())
            .map_err(StandardAssetStorageRowError::InvalidLocation)?;
        let group_kind_raw = row.required_text("group_kind")?;
        let kind = TextGroupKind::from_storage_name(group_kind_raw.as_str()).ok_or_else(|| {
            StandardAssetStorageRowError::UnknownGroupKind(group_kind_raw.clone())
        })?;
        let group_order = row.non_negative_order("group_order")?;
        Ok(Self {
            group_location_raw,
            group_location,
            group_kind_raw,
            kind,
            group_order,
        })
    }
}

pub(crate) struct StandardTextUnitLocationStorageRow {
    pub(crate) group_location_raw: String,
    pub(crate) group_location: RpgMakerLocation,
}

impl StandardTextUnitLocationStorageRow {
    pub(crate) fn decode(
        row: &mut StandardAssetStorageRowDecoder,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let group_location_raw = row.required_text("group_location")?;
        let group_location = RpgMakerLocationCodec::decode(group_location_raw.as_str())
            .map_err(StandardAssetStorageRowError::InvalidLocation)?;
        Ok(Self {
            group_location_raw,
            group_location,
        })
    }
}

pub(crate) struct StandardTextUnitIdentityStorageRow {
    pub(crate) location: StandardTextUnitLocationStorageRow,
    pub(crate) role_raw: String,
    pub(crate) role: TextUnitRole,
    pub(crate) unit_order: usize,
}

impl StandardTextUnitIdentityStorageRow {
    pub(crate) fn decode_after_location(
        row: &mut StandardAssetStorageRowDecoder,
        location: StandardTextUnitLocationStorageRow,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let role_raw = row.required_text("unit_role")?;
        let role = RpgMakerProjectionCodec::decode_role(role_raw.as_str())
            .map_err(StandardAssetStorageRowError::InvalidRole)?;
        let unit_order = row.non_negative_order("unit_order")?;
        Ok(Self {
            location,
            role_raw,
            role,
            unit_order,
        })
    }
}

pub(crate) struct StandardTextUnitStorageRow {
    pub(crate) group_location_raw: String,
    pub(crate) group_location: RpgMakerLocation,
    pub(crate) role_raw: String,
    pub(crate) role: TextUnitRole,
    pub(crate) unit_order: usize,
    pub(crate) source_content_json: String,
    pub(crate) source_content: TextUnitContent,
    pub(crate) source_context_json: String,
    pub(crate) translation_content_json: Option<String>,
}

impl StandardTextUnitStorageRow {
    pub(crate) fn decode(
        row: &mut StandardAssetStorageRowDecoder,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let location = StandardTextUnitLocationStorageRow::decode(row)?;
        let identity = StandardTextUnitIdentityStorageRow::decode_after_location(row, location)?;
        Self::decode_after_identity(row, identity)
    }

    pub(crate) fn decode_after_identity(
        row: &mut StandardAssetStorageRowDecoder,
        identity: StandardTextUnitIdentityStorageRow,
    ) -> Result<Self, StandardAssetStorageRowError> {
        let source_content_json = row.required_text("source_content_json")?;
        let source_content = serde_json::from_str(source_content_json.as_str())
            .map_err(StandardAssetStorageRowError::InvalidSourceContent)?;
        let source_context_json = row.required_text("source_context_json")?;
        let translation_content_json = row.optional_text("translation_content_json")?;
        let StandardTextUnitIdentityStorageRow {
            location:
                StandardTextUnitLocationStorageRow {
                    group_location_raw,
                    group_location,
                },
            role_raw,
            role,
            unit_order,
        } = identity;
        Ok(Self {
            group_location_raw,
            group_location,
            role_raw,
            role,
            unit_order,
            source_content_json,
            source_content,
            source_context_json,
            translation_content_json,
        })
    }

    pub(crate) fn decode_translation_content(
        &self,
    ) -> Result<Option<TextUnitContent>, StandardAssetStorageRowError> {
        self.translation_content_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StandardAssetStorageRowError::InvalidTranslationContent)
    }
}

#[cfg(test)]
mod tests {
    use crate::rpg_maker::model::ScalarFieldKey;
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource};

    use super::*;

    #[test]
    fn owner_partition_merge_uses_the_single_natural_order() {
        let row = |value: &str| SqliteRow::new(vec![SqliteValue::Text(value.to_owned())]);
        let merged =
            merge_owner_partitions([vec![row("builtin")], vec![row("rules")], vec![row("lua")]]);

        for (expected, owner) in STANDARD_ASSET_OWNER_ORDER.into_iter().enumerate() {
            assert_eq!(standard_asset_owner_order(owner), expected);
        }
        assert_eq!(
            standard_asset_owner_order_sql("owner"),
            "CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 WHEN 'lua' THEN 2 END"
        );
        assert_eq!(
            merged
                .iter()
                .map(|row| row.owner)
                .collect::<Vec<RpgMakerStandardAssetOwner>>(),
            STANDARD_ASSET_OWNER_ORDER
        );
        assert_eq!(
            merged
                .into_iter()
                .map(|row| match row.row.into_values().pop() {
                    Some(SqliteValue::Text(value)) => value,
                    _ => panic!("测试行应包含 TEXT"),
                })
                .collect::<Vec<_>>(),
            ["builtin", "rules", "lua"]
        );
    }

    #[test]
    fn shared_unit_decoder_preserves_raw_text_allocations_and_typed_values() {
        let location_raw = RpgMakerLocationCodec::encode(&RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![RpgMakerLocationStep::key("name")],
        ))
        .expect("测试位置应编码");
        let role_raw = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
            ScalarFieldKey::new("name").expect("测试字段键应合法"),
        ))
        .expect("测试角色应编码");
        let source_content_json = r#""原文""#.to_owned();
        let source_context_json = "{}".to_owned();
        let location_pointer = location_raw.as_ptr();
        let role_pointer = role_raw.as_ptr();
        let source_pointer = source_content_json.as_ptr();
        let mut decoder = StandardAssetStorageRowDecoder::new(
            SqliteRow::new(vec![
                SqliteValue::Text(location_raw),
                SqliteValue::Text(role_raw),
                SqliteValue::Integer(0),
                SqliteValue::Text(source_content_json),
                SqliteValue::Text(source_context_json),
                SqliteValue::Text(r#""译文""#.to_owned()),
            ]),
            6,
        )
        .expect("共享单元投影列数应合法");

        let decoded = StandardTextUnitStorageRow::decode(&mut decoder).expect("共享单元行应解码");

        assert_eq!(decoded.group_location_raw.as_ptr(), location_pointer);
        assert_eq!(decoded.role_raw.as_ptr(), role_pointer);
        assert_eq!(decoded.source_content_json.as_ptr(), source_pointer);
        assert_eq!(decoded.source_content.as_value(), Some("原文"));
        assert_eq!(
            decoded
                .decode_translation_content()
                .expect("译文 JSON 应解码")
                .as_ref()
                .and_then(TextUnitContent::as_value),
            Some("译文")
        );
    }

    #[test]
    fn shared_decoder_reports_exact_column_shape_and_type() {
        assert!(matches!(
            StandardAssetStorageRowDecoder::new(SqliteRow::new(Vec::new()), 1),
            Err(StandardAssetStorageRowError::WrongColumnCount {
                expected: 1,
                actual: 0
            })
        ));
        let mut decoder =
            StandardAssetStorageRowDecoder::new(SqliteRow::new(vec![SqliteValue::Null]), 1)
                .expect("测试列数应合法");
        assert!(matches!(
            decoder.required_text("owner"),
            Err(StandardAssetStorageRowError::WrongColumnType {
                column: "owner",
                expected: "TEXT",
                actual: "NULL"
            })
        ));
    }
}
