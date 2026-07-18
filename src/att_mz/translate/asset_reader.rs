//! 从五张 MZ 标准资产表建立一致翻译语料。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::project::OpenedProject;
use crate::att_mz::project_database::{
    PLACEHOLDER_RULES_RESOURCE_KIND, SourceSnapshotFingerprint, TERMINOLOGY_RESOURCE_KIND,
};
use crate::att_mz::standard_asset::{
    MzStandardAssetLocationError, MzStandardAssetOwner, MzStandardAssetReadingConfig,
    MzStandardAssetStorageKind, MzStandardAssetTable, MzTextBodyUnit,
};
use crate::att_mz::text::{MzLocation, TextGroupKind};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardTranslationAsset, StandardTranslationAssetReader, StandardTranslationCorpus,
    StandardTranslationGroup, TranslationLeafIdentity,
};

const READ_TRANSLATION_SNAPSHOT: &str = r#"SELECT
    row_kind,
    owner,
    source_snapshot_fingerprint,
    resource_kind,
    canonical_json,
    asset_table,
    exact_location,
    group_location,
    field_name,
    unit_type,
    original_text,
    translation,
    translation_state
FROM (
    SELECT
        '0_metadata' AS row_kind,
        NULL AS owner,
        source_snapshot_fingerprint,
        NULL AS resource_kind,
        NULL AS canonical_json,
        NULL AS asset_table,
        NULL AS exact_location,
        NULL AS group_location,
        NULL AS field_name,
        NULL AS unit_type,
        NULL AS original_text,
        NULL AS translation,
        NULL AS translation_state
    FROM metadata

    UNION ALL

    SELECT
        '1_owner' AS row_kind,
        owner,
        source_snapshot_fingerprint,
        NULL AS resource_kind,
        NULL AS canonical_json,
        NULL AS asset_table,
        NULL AS exact_location,
        NULL AS group_location,
        NULL AS field_name,
        NULL AS unit_type,
        NULL AS original_text,
        NULL AS translation,
        NULL AS translation_state
    FROM standard_asset_owner_state

    UNION ALL

    SELECT
        '2_resource',
        NULL,
        NULL,
        resource_kind,
        canonical_json,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL
    FROM standard_translation_resource

    UNION ALL

    SELECT
        '3_asset',
        asset.owner,
        NULL,
        NULL,
        NULL,
        asset.asset_table,
        asset.exact_location,
        asset.group_location,
        asset.field_name,
        asset.unit_type,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM (
    SELECT
        'entry' AS asset_table,
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL AS unit_type,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM entry AS asset

    UNION ALL

    SELECT
        'system_text',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM system_text AS asset

    UNION ALL

    SELECT
        'map_text',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM map_text AS asset

    UNION ALL

    SELECT
        'text_body',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        asset.unit_type,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM text_body AS asset

    UNION ALL

    SELECT
        'plugin_param',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        asset.translation_state
    FROM plugin_param AS asset
    ) AS asset
)
ORDER BY row_kind, owner, resource_kind, asset_table, exact_location"#;

/// 验证 owner 新鲜度、读取当前资源，并用受控 CPU 解码标准翻译语料。
pub(crate) struct MzStandardTranslationAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
    config: MzStandardAssetReadingConfig,
}

impl<Q, C> MzStandardTranslationAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: MzStandardAssetReadingConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<Q, C> StandardTranslationAssetReader for MzStandardTranslationAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzStandardTranslationAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
    ) -> Result<StandardTranslationCorpus, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let snapshot_rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_TRANSLATION_SNAPSHOT, Vec::new()),
            )
            .await
            .map_err(|error| map_query_error(database_path.clone(), error))?;
        let snapshot_rows = split_snapshot_rows(snapshot_rows)
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)?;
        let source_snapshot_fingerprint = decode_metadata(snapshot_rows.metadata)
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)?;
        if source_snapshot_fingerprint != project.source_snapshot_fingerprint() {
            return Err(
                MzStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                    expected: project.source_snapshot_fingerprint(),
                    actual: source_snapshot_fingerprint,
                },
            );
        }
        let owner_states = decode_owner_states(snapshot_rows.owners, source_snapshot_fingerprint)
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)?;
        if !owner_states.stale.is_empty() {
            return Err(
                MzStandardTranslationAssetReadingError::ExtractionOutOfDate {
                    owners: owner_states.stale,
                },
            );
        }

        let (terminology_json, placeholder_rules_json) = decode_resources(snapshot_rows.resources)
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)?;

        if snapshot_rows.assets.is_empty() {
            return Ok(StandardTranslationCorpus::with_snapshot(
                Vec::new(),
                source_snapshot_fingerprint,
                owner_states.source_fingerprints,
                terminology_json,
                placeholder_rules_json,
            ));
        }

        let leaves_per_job = self.config.leaves_per_decode_job().get();
        let batches = self
            .cpu
            .execute(move || partition_rows(snapshot_rows.assets, leaves_per_job))
            .await
            .map_err(MzStandardTranslationAssetReadingError::SchedulePartition)?;

        let decoded_batches = stream::iter(batches.into_iter().map(|batch| {
            let active_owners = owner_states.active.clone();
            async move {
                self.cpu
                    .execute(move || decode_rows(batch, &active_owners))
                    .await
                    .map_err(MzStandardTranslationAssetReadingError::ScheduleDecode)?
                    .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)
            }
        }))
        .buffered(self.config.decode_concurrency().get())
        .try_collect::<Vec<_>>()
        .await?;

        let decoded = decoded_batches.into_iter().flatten().collect::<Vec<_>>();
        let groups = self
            .cpu
            .execute(move || assemble_corpus(decoded))
            .await
            .map_err(MzStandardTranslationAssetReadingError::ScheduleAssembly)?
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)?;
        Ok(StandardTranslationCorpus::with_snapshot(
            groups,
            source_snapshot_fingerprint,
            owner_states.source_fingerprints,
            terminology_json,
            placeholder_rules_json,
        ))
    }
}

/// 标准翻译资产读取职责产生的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzStandardTranslationAssetReadingError<Q, C> {
    DatabaseNotFound {
        database_path: PathBuf,
    },
    Query {
        database_path: PathBuf,
        source: Q,
    },
    ProjectSnapshotChanged {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    ExtractionOutOfDate {
        owners: Vec<MzStandardAssetOwner>,
    },
    SchedulePartition(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardTranslationAssetSnapshot),
}

impl<Q, C> fmt::Display for MzStandardTranslationAssetReadingError<Q, C>
where
    Q: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::Query {
                database_path,
                source,
            } => write!(
                formatter,
                "无法从 {} 读取标准翻译资产：{source}",
                database_path.display()
            ),
            Self::ExtractionOutOfDate { owners } => write!(
                formatter,
                "标准资产提取已过期：{}",
                owners
                    .iter()
                    .map(|owner| owner.storage_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::ProjectSnapshotChanged { expected, actual } => write!(
                formatter,
                "项目打开后 metadata 来源指纹发生变化（预期 {expected:?}，实际 {actual:?}）"
            ),
            Self::SchedulePartition(source) => {
                write!(formatter, "资产解码分批任务执行失败：{source}")
            }
            Self::ScheduleDecode(source) => write!(formatter, "资产解码任务执行失败：{source}"),
            Self::ScheduleAssembly(source) => {
                write!(formatter, "资产语料组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot(source) => write!(formatter, "标准翻译资产损坏：{source}"),
        }
    }
}

impl<Q, C> Error for MzStandardTranslationAssetReadingError<Q, C>
where
    Q: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::SchedulePartition(source)
            | Self::ScheduleDecode(source)
            | Self::ScheduleAssembly(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::DatabaseNotFound { .. }
            | Self::ProjectSnapshotChanged { .. }
            | Self::ExtractionOutOfDate { .. } => None,
        }
    }
}

#[derive(Debug)]
struct DecodedRow {
    owner: MzStandardAssetOwner,
    storage: MzStandardAssetStorageKind,
    kind: TextGroupKind,
    exact_location: MzLocation,
    group_location: MzLocation,
    field_name: String,
    original_text: String,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
}

#[derive(Debug)]
struct LeafAccumulator {
    owner: MzStandardAssetOwner,
    storage: MzStandardAssetStorageKind,
    kind: TextGroupKind,
    group_location: MzLocation,
    field_name: String,
    original_text: String,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
}

impl LeafAccumulator {
    fn from_row(row: DecodedRow) -> Self {
        Self {
            owner: row.owner,
            storage: row.storage,
            kind: row.kind,
            group_location: row.group_location,
            field_name: row.field_name,
            original_text: row.original_text,
            translation: row.translation,
            translation_state: row.translation_state,
        }
    }

    fn accepts(&self, row: &DecodedRow) -> bool {
        self.owner == row.owner
            && self.storage == row.storage
            && self.kind == row.kind
            && self.group_location == row.group_location
            && self.field_name == row.field_name
            && self.original_text == row.original_text
            && self.translation == row.translation
            && self.translation_state == row.translation_state
    }
}

/// 数据库内容违反标准资产 schema 或跨行一致性时的明确原因。
#[derive(Debug)]
pub(crate) enum InvalidStandardTranslationAssetSnapshot {
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    UnknownSnapshotRowKind(String),
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    UnknownAssetTable(String),
    UnknownOwner(String),
    InactiveOwner(String),
    UnknownUnitType(String),
    UnexpectedUnitType {
        table: String,
    },
    BlankFieldName,
    BlankOriginalText,
    BlankTranslation,
    InvalidTranslationStatePair,
    InvalidTranslationStateLength {
        actual: usize,
    },
    DuplicateOwner(String),
    InvalidOwnerFingerprintLength {
        owner: String,
        actual: usize,
    },
    InvalidMetadataRowCount {
        actual: usize,
    },
    InvalidMetadataFingerprintLength {
        actual: usize,
    },
    MissingTranslationResource(&'static str),
    DuplicateTranslationResource(String),
    UnknownTranslationResource(String),
    BlankTranslationResource(String),
    InvalidLocation {
        column: &'static str,
        source: MzLocationCodecError,
    },
    InvalidStorageLocation(MzStandardAssetLocationError),
    ContradictoryAssetRows {
        exact_location: Box<MzLocation>,
    },
}

impl fmt::Display for InvalidStandardTranslationAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongColumnCount { expected, actual } => {
                write!(formatter, "查询行应包含 {expected} 列，实际为 {actual} 列")
            }
            Self::UnknownSnapshotRowKind(kind) => write!(formatter, "未知翻译快照行种类：{kind}"),
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "列 {column} 应为 {expected}，实际为 {actual}"),
            Self::UnknownAssetTable(table) => write!(formatter, "未知标准资产表：{table}"),
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::InactiveOwner(owner) => write!(formatter, "资产引用未激活 owner：{owner}"),
            Self::UnknownUnitType(unit_type) => write!(formatter, "未知文本单元类型：{unit_type}"),
            Self::UnexpectedUnitType { table } => {
                write!(formatter, "资产表 {table} 的 unit_type 与表语义不一致")
            }
            Self::BlankFieldName => formatter.write_str("标准资产字段名为空"),
            Self::BlankOriginalText => formatter.write_str("标准资产原文仅包含空白"),
            Self::BlankTranslation => formatter.write_str("标准资产译文仅包含空白"),
            Self::InvalidTranslationStatePair => {
                formatter.write_str("translation 与 translation_state 必须同时存在或同时为空")
            }
            Self::InvalidTranslationStateLength { actual } => {
                write!(
                    formatter,
                    "translation_state 必须是 32 字节 BLOB，实际为 {actual} 字节"
                )
            }
            Self::DuplicateOwner(owner) => write!(formatter, "资产 owner 状态重复：{owner}"),
            Self::InvalidOwnerFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {owner} 的来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::InvalidMetadataRowCount { actual } => {
                write!(formatter, "metadata 必须恰好一行，实际为 {actual} 行")
            }
            Self::InvalidMetadataFingerprintLength { actual } => write!(
                formatter,
                "metadata 来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::MissingTranslationResource(kind) => {
                write!(formatter, "缺少翻译资源 {kind}")
            }
            Self::DuplicateTranslationResource(kind) => {
                write!(formatter, "翻译资源重复：{kind}")
            }
            Self::UnknownTranslationResource(kind) => {
                write!(formatter, "未知翻译资源：{kind}")
            }
            Self::BlankTranslationResource(kind) => {
                write!(formatter, "翻译资源 {kind} 为空")
            }
            Self::InvalidLocation { column, source } => {
                write!(formatter, "列 {column} 中的结构化位置无效：{source}")
            }
            Self::InvalidStorageLocation(source) => {
                write!(formatter, "结构化位置与标准资产存储语义不一致：{source}")
            }
            Self::ContradictoryAssetRows { exact_location } => {
                write!(formatter, "同一资产位置存在矛盾行：{exact_location}")
            }
        }
    }
}

impl Error for InvalidStandardTranslationAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation { source, .. } => Some(source),
            Self::InvalidStorageLocation(source) => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> MzStandardTranslationAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            MzStandardTranslationAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            MzStandardTranslationAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

struct SnapshotRows {
    metadata: Vec<SqliteRow>,
    owners: Vec<SqliteRow>,
    resources: Vec<SqliteRow>,
    assets: Vec<SqliteRow>,
}

fn split_snapshot_rows(
    rows: Vec<SqliteRow>,
) -> Result<SnapshotRows, InvalidStandardTranslationAssetSnapshot> {
    let mut metadata = Vec::new();
    let mut owners = Vec::new();
    let mut resources = Vec::new();
    let mut assets = Vec::new();
    for row in rows {
        let values = row.into_values();
        let actual = values.len();
        let [
            row_kind,
            owner,
            source_fingerprint,
            resource_kind,
            canonical_json,
            asset_table,
            exact_location,
            group_location,
            field_name,
            unit_type,
            original_text,
            translation,
            translation_state,
        ]: [SqliteValue; 13] = values.try_into().map_err(|_| {
            InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 13,
                actual,
            }
        })?;
        let row_kind = required_text(row_kind, "row_kind")?;
        match row_kind.as_str() {
            "0_metadata" => metadata.push(SqliteRow::new(vec![source_fingerprint])),
            "1_owner" => owners.push(SqliteRow::new(vec![owner, source_fingerprint])),
            "2_resource" => resources.push(SqliteRow::new(vec![resource_kind, canonical_json])),
            "3_asset" => assets.push(SqliteRow::new(vec![
                asset_table,
                exact_location,
                owner,
                group_location,
                field_name,
                unit_type,
                original_text,
                translation,
                translation_state,
            ])),
            _ => {
                return Err(
                    InvalidStandardTranslationAssetSnapshot::UnknownSnapshotRowKind(row_kind),
                );
            }
        }
    }
    Ok(SnapshotRows {
        metadata,
        owners,
        resources,
        assets,
    })
}

fn decode_metadata(
    rows: Vec<SqliteRow>,
) -> Result<SourceSnapshotFingerprint, InvalidStandardTranslationAssetSnapshot> {
    if rows.len() != 1 {
        return Err(
            InvalidStandardTranslationAssetSnapshot::InvalidMetadataRowCount { actual: rows.len() },
        );
    }
    let mut values = rows
        .into_iter()
        .next()
        .expect("已确认恰好一行")
        .into_values();
    if values.len() != 1 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 1,
            actual: values.len(),
        });
    }
    let value = values.pop().expect("已确认恰好一列");
    let SqliteValue::Blob(bytes) = value else {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column: "metadata.source_snapshot_fingerprint",
            expected: "BLOB",
            actual: value.kind_name(),
        });
    };
    SourceSnapshotFingerprint::from_slice(&bytes).map_err(|error| {
        InvalidStandardTranslationAssetSnapshot::InvalidMetadataFingerprintLength {
            actual: error.actual(),
        }
    })
}

struct DecodedOwnerStates {
    stale: Vec<MzStandardAssetOwner>,
    active: BTreeSet<&'static str>,
    source_fingerprints: Vec<(MzStandardAssetOwner, SourceSnapshotFingerprint)>,
}

fn decode_owner_states(
    rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
) -> Result<DecodedOwnerStates, InvalidStandardTranslationAssetSnapshot> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stale = Vec::new();
    let mut owner_source_fingerprints = Vec::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 2,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let owner_name = required_text(next(&mut values), "owner")?;
        let owner = MzStandardAssetOwner::from_storage_name(&owner_name).ok_or_else(|| {
            InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner_name.clone())
        })?;
        if !seen.insert(owner.storage_name()) {
            return Err(InvalidStandardTranslationAssetSnapshot::DuplicateOwner(
                owner_name,
            ));
        }
        let fingerprint_value = next(&mut values);
        let SqliteValue::Blob(bytes) = fingerprint_value else {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
                column: "source_snapshot_fingerprint",
                expected: "BLOB",
                actual: fingerprint_value.kind_name(),
            });
        };
        let fingerprint = SourceSnapshotFingerprint::from_slice(&bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidOwnerFingerprintLength {
                owner: owner.storage_name().to_owned(),
                actual: error.actual(),
            }
        })?;
        if fingerprint != current {
            stale.push(owner);
        }
        owner_source_fingerprints.push((owner, fingerprint));
    }
    stale.sort_by_key(|owner| match owner {
        MzStandardAssetOwner::Builtin => 0,
        MzStandardAssetOwner::Rules => 1,
        MzStandardAssetOwner::Lua => 2,
    });
    owner_source_fingerprints.sort_by_key(|(owner, _)| match owner {
        MzStandardAssetOwner::Builtin => 0,
        MzStandardAssetOwner::Rules => 1,
        MzStandardAssetOwner::Lua => 2,
    });
    Ok(DecodedOwnerStates {
        stale,
        active: seen,
        source_fingerprints: owner_source_fingerprints,
    })
}

fn decode_resources(
    rows: Vec<SqliteRow>,
) -> Result<(String, String), InvalidStandardTranslationAssetSnapshot> {
    let mut resources = BTreeMap::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 2,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let kind = required_text(next(&mut values), "resource_kind")?;
        if kind != TERMINOLOGY_RESOURCE_KIND && kind != PLACEHOLDER_RULES_RESOURCE_KIND {
            return Err(InvalidStandardTranslationAssetSnapshot::UnknownTranslationResource(kind));
        }
        let canonical_json = required_text(next(&mut values), "canonical_json")?;
        if canonical_json.is_empty() {
            return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslationResource(kind));
        }
        if resources.insert(kind.clone(), canonical_json).is_some() {
            return Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateTranslationResource(kind),
            );
        }
    }
    let terminology = resources.remove(TERMINOLOGY_RESOURCE_KIND).ok_or(
        InvalidStandardTranslationAssetSnapshot::MissingTranslationResource(
            TERMINOLOGY_RESOURCE_KIND,
        ),
    )?;
    let placeholders = resources.remove(PLACEHOLDER_RULES_RESOURCE_KIND).ok_or(
        InvalidStandardTranslationAssetSnapshot::MissingTranslationResource(
            PLACEHOLDER_RULES_RESOURCE_KIND,
        ),
    )?;
    Ok((terminology, placeholders))
}

type RawLeafKey = (Option<SqliteValue>, Option<SqliteValue>);

fn partition_rows(rows: Vec<SqliteRow>, leaves_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut leaves_in_batch = 0usize;
    let mut previous_key: Option<RawLeafKey> = None;

    for row in rows {
        let values = row.values();
        let key = (values.first().cloned(), values.get(1).cloned());
        let is_new_leaf = previous_key.as_ref() != Some(&key);
        if is_new_leaf && leaves_in_batch == leaves_per_job {
            batches.push(std::mem::take(&mut batch));
            leaves_in_batch = 0;
        }
        if is_new_leaf {
            leaves_in_batch += 1;
            previous_key = Some(key);
        }
        batch.push(row);
    }

    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn decode_rows(
    rows: Vec<SqliteRow>,
    active_owners: &BTreeSet<&'static str>,
) -> Result<Vec<DecodedRow>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|row| decode_row(row, active_owners))
        .collect()
}

fn decode_row(
    row: SqliteRow,
    active_owners: &BTreeSet<&'static str>,
) -> Result<DecodedRow, InvalidStandardTranslationAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 9 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 9,
            actual: values.len(),
        });
    }
    let mut values = values.into_iter();
    let table_name = required_text(next(&mut values), "asset_table")?;
    let exact_location = required_text(next(&mut values), "exact_location")?;
    let owner = required_text(next(&mut values), "owner")?;
    let group_location = required_text(next(&mut values), "group_location")?;
    let field_name = required_text(next(&mut values), "field_name")?;
    let unit_type = optional_text(next(&mut values), "unit_type")?;
    let original_text = required_text(next(&mut values), "original_text")?;
    let translation = optional_text(next(&mut values), "translation")?;
    let translation_state = optional_blob(next(&mut values), "translation_state")?;

    let table = MzStandardAssetTable::from_storage_name(&table_name).ok_or_else(|| {
        InvalidStandardTranslationAssetSnapshot::UnknownAssetTable(table_name.clone())
    })?;
    let owner = MzStandardAssetOwner::from_storage_name(&owner)
        .ok_or(InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner))?;
    if !active_owners.contains(owner.storage_name()) {
        return Err(InvalidStandardTranslationAssetSnapshot::InactiveOwner(
            owner.storage_name().to_owned(),
        ));
    }
    let unit = unit_type
        .as_deref()
        .map(|value| {
            MzTextBodyUnit::from_storage_name(value).ok_or_else(|| {
                InvalidStandardTranslationAssetSnapshot::UnknownUnitType(value.to_owned())
            })
        })
        .transpose()?;
    let storage = MzStandardAssetStorageKind::from_parts(table, unit).ok_or_else(|| {
        InvalidStandardTranslationAssetSnapshot::UnexpectedUnitType {
            table: table_name.clone(),
        }
    })?;
    let kind = storage.group_kind();
    let exact_location = MzLocationCodec::decode(&exact_location).map_err(|source| {
        InvalidStandardTranslationAssetSnapshot::InvalidLocation {
            column: "exact_location",
            source,
        }
    })?;
    let group_location = MzLocationCodec::decode(&group_location).map_err(|source| {
        InvalidStandardTranslationAssetSnapshot::InvalidLocation {
            column: "group_location",
            source,
        }
    })?;
    storage
        .validate_locations(&exact_location, &group_location)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidStorageLocation)?;

    if field_name.is_empty() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankFieldName);
    }
    if original_text.trim().is_empty() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankOriginalText);
    }
    if translation
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslation);
    }

    let translation_state = match (translation.as_ref(), translation_state) {
        (None, None) => None,
        (Some(_), Some(bytes)) => Some(Sha256Fingerprint::from_slice(&bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidTranslationStateLength {
                actual: error.actual(),
            }
        })?),
        _ => return Err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationStatePair),
    };

    Ok(DecodedRow {
        owner,
        storage,
        kind,
        exact_location,
        group_location,
        field_name,
        original_text,
        translation,
        translation_state,
    })
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，标准资产查询行必须具有完整投影")
}

fn required_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_blob(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<Vec<u8>>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Blob(value) => Ok(Some(value)),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "BLOB 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn assemble_corpus(
    rows: Vec<DecodedRow>,
) -> Result<Vec<StandardTranslationGroup>, InvalidStandardTranslationAssetSnapshot> {
    let mut leaves = BTreeMap::<MzLocation, LeafAccumulator>::new();
    for row in rows {
        let exact_location = row.exact_location.clone();
        match leaves.entry(row.exact_location.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(LeafAccumulator::from_row(row));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if !entry.get().accepts(&row) {
                    return Err(
                        InvalidStandardTranslationAssetSnapshot::ContradictoryAssetRows {
                            exact_location: Box::new(exact_location),
                        },
                    );
                }
                return Err(
                    InvalidStandardTranslationAssetSnapshot::ContradictoryAssetRows {
                        exact_location: Box::new(exact_location),
                    },
                );
            }
        }
    }

    let mut groups = BTreeMap::<(TextGroupKind, MzLocation), Vec<StandardTranslationAsset>>::new();
    for (exact_location, leaf) in leaves {
        let identity = TranslationLeafIdentity::new(
            leaf.owner,
            leaf.kind,
            leaf.field_name,
            leaf.group_location.clone(),
            exact_location,
            leaf.original_text,
        );
        groups
            .entry((leaf.kind, leaf.group_location))
            .or_default()
            .push(StandardTranslationAsset::new(
                identity,
                leaf.translation,
                leaf.translation_state,
            ));
    }

    Ok(groups
        .into_iter()
        .map(|((kind, group_location), assets)| {
            StandardTranslationGroup::new(kind, group_location, assets)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
    use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile};

    use super::*;

    type QueryResponse = Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeError>>;
    type SharedQueryResponse = Arc<Mutex<VecDeque<QueryResponse>>>;

    #[derive(Clone)]
    struct RecordingQuery {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        response: SharedQueryResponse,
    }

    impl SqliteQueryExecutor for RecordingQuery {
        type Error = FakeError;

        fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
        {
            self.calls
                .lock()
                .expect("查询调用锁不应中毒")
                .push((path, query));
            let response = self
                .response
                .lock()
                .expect("查询响应锁不应中毒")
                .pop_front()
                .expect("测试应为每次查询提供响应");
            async move { response }
        }
    }

    #[derive(Clone)]
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        fail: bool,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(CpuTaskExecutionError::Unavailable(FakeError("cpu")));
            }
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[test]
    fn config_preserves_explicit_non_zero_limits() {
        let config = MzStandardAssetReadingConfig::new(non_zero(3), non_zero(12));

        assert_eq!(config.decode_concurrency().get(), 3);
        assert_eq!(config.leaves_per_decode_job().get(), 12);
    }

    #[test]
    fn decoded_locations_incompatible_with_the_asset_table_are_rejected() {
        let group = location(vec![MzLocationStep::index(1)]);
        let exact = location(vec![
            MzLocationStep::index(1),
            MzLocationStep::key("gameTitle"),
        ]);

        let error = decode_row(
            row(
                "system_text",
                &exact,
                &group,
                "gameTitle",
                [
                    SqliteValue::Text("标题".to_owned()),
                    SqliteValue::Null,
                    SqliteValue::Null,
                ],
            ),
            &active_owners(["builtin"]),
        )
        .expect_err("SystemText 不应接受 Items 来源");

        assert!(matches!(
            error,
            InvalidStandardTranslationAssetSnapshot::InvalidStorageLocation(
                MzStandardAssetLocationError::SourceDoesNotMatchStorage {
                    storage: MzStandardAssetStorageKind::SystemText,
                    source: MzSource::Data(StandardDataFile::Items),
                }
            )
        ));
    }

    #[test]
    fn rules_entry_on_map_with_custom_field_and_path_is_accepted() {
        let source = MzSource::map(15);
        let group = MzLocation::value(
            source.clone(),
            vec![MzLocationStep::key("rules_group"), MzLocationStep::index(2)],
        );
        let exact = MzLocation::value(
            source,
            vec![
                MzLocationStep::key("unrelated_rules_path"),
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("actual_name"),
            ],
        );
        let mut values = row(
            "entry",
            &exact,
            &group,
            "custom_field_name",
            [
                SqliteValue::Text("原文".to_owned()),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        )
        .into_values();
        values[2] = SqliteValue::Text("rules".to_owned());

        let decoded = decode_row(SqliteRow::new(values), &active_owners(["rules"]))
            .expect("合法 Rules Entry→Map 应被接受");

        assert_eq!(decoded.storage, MzStandardAssetStorageKind::Entry);
        assert_eq!(decoded.field_name, "custom_field_name");
        assert_eq!(decoded.exact_location, exact);
        assert_eq!(decoded.group_location, group);
    }

    #[tokio::test]
    async fn owner_resources_and_union_assets_form_one_current_corpus() {
        let item_group = location(vec![MzLocationStep::index(10)]);
        let name = location(vec![MzLocationStep::index(10), MzLocationStep::key("name")]);
        let description = location(vec![
            MzLocationStep::index(10),
            MzLocationStep::key("description"),
        ]);
        let rows = vec![
            row(
                "entry",
                &name,
                &item_group,
                "name",
                [
                    SqliteValue::Text("宝剑".to_owned()),
                    SqliteValue::Text("Sword".to_owned()),
                    SqliteValue::Blob(vec![0x11; 32]),
                ],
            ),
            row(
                "entry",
                &description,
                &item_group,
                "description",
                [
                    SqliteValue::Text("锋利的宝剑".to_owned()),
                    SqliteValue::Null,
                    SqliteValue::Null,
                ],
            ),
        ];
        let harness = Harness::with_responses([Ok(snapshot_rows(
            owner_rows("builtin", 0xa5),
            resource_rows(),
            rows,
        ))]);
        let service = harness.service(2, 1);

        let corpus = service.read(&project()).await.expect("资产读取应该成功");

        let calls = harness.query_calls.lock().expect("查询调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(calls[0].1.statement(), READ_TRANSLATION_SNAPSHOT);
        assert!(calls[0].1.statement().matches("UNION ALL").count() == 7);
        assert_eq!(corpus.groups().len(), 1);
        assert_eq!(corpus.groups()[0].assets().len(), 2);
        let translated_name = corpus.groups()[0]
            .assets()
            .iter()
            .find(|asset| asset.identity().field_name() == "name")
            .expect("name 叶子应存在");
        assert_eq!(
            translated_name.translation_state(),
            Some(Sha256Fingerprint::from_bytes([0x11; 32]))
        );
        assert_eq!(corpus.terminology_json(), "[]");
        assert_eq!(corpus.placeholder_rules_json(), "[]");
        let (_, baseline) = corpus.into_parts();
        assert_eq!(
            baseline.source_snapshot_fingerprint(),
            SourceSnapshotFingerprint::from_bytes([0xa5; 32])
        );
        assert_eq!(
            baseline.owner_source_fingerprints(),
            [(
                MzStandardAssetOwner::Builtin,
                SourceSnapshotFingerprint::from_bytes([0xa5; 32])
            )]
        );
    }

    #[tokio::test]
    async fn rows_from_different_owners_merge_by_their_real_mz_group() {
        let item_group = location(vec![MzLocationStep::index(11)]);
        let name = location(vec![MzLocationStep::index(11), MzLocationStep::key("name")]);
        let description = location(vec![
            MzLocationStep::index(11),
            MzLocationStep::key("description"),
        ]);
        let builtin_name = row(
            "entry",
            &name,
            &item_group,
            "name",
            [
                SqliteValue::Text("宝剑".to_owned()),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        );
        let mut rules_description = row(
            "entry",
            &description,
            &item_group,
            "description",
            [
                SqliteValue::Text("锋利的宝剑".to_owned()),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        )
        .into_values();
        rules_description[2] = SqliteValue::Text("rules".to_owned());
        let owners = [owner_rows("builtin", 0xa5), owner_rows("rules", 0xa5)]
            .into_iter()
            .flatten()
            .collect();
        let harness = Harness::with_responses([Ok(snapshot_rows(
            owners,
            resource_rows(),
            vec![builtin_name, SqliteRow::new(rules_description)],
        ))]);

        let corpus = harness
            .service(1, 10)
            .read(&project())
            .await
            .expect("不同 owner 对同一 MZ 对象的互补叶应合并");

        assert_eq!(corpus.groups().len(), 1);
        assert_eq!(corpus.groups()[0].assets().len(), 2);
        assert!(corpus.groups()[0].assets().iter().any(|asset| {
            asset.identity().owner() == MzStandardAssetOwner::Builtin
                && asset.identity().exact_location() == &name
        }));
        assert!(corpus.groups()[0].assets().iter().any(|asset| {
            asset.identity().owner() == MzStandardAssetOwner::Rules
                && asset.identity().exact_location() == &description
        }));
    }

    #[tokio::test]
    async fn stale_owner_blocks_before_resources_or_assets_are_read() {
        let harness = Harness::with_responses([Ok(snapshot_rows(
            owner_rows("builtin", 0x44),
            resource_rows(),
            Vec::new(),
        ))]);

        let error = harness
            .service(1, 1)
            .read(&project())
            .await
            .expect_err("过期 owner 必须阻止下游");

        assert!(matches!(
            error,
            MzStandardTranslationAssetReadingError::ExtractionOutOfDate { owners }
                if owners == vec![MzStandardAssetOwner::Builtin]
        ));
        assert_eq!(harness.query_calls.lock().expect("查询锁").len(), 1);
    }

    #[tokio::test]
    async fn metadata_change_after_project_open_is_reported_as_concurrent_state_change() {
        let mut rows = snapshot_rows(owner_rows("builtin", 0xa5), resource_rows(), Vec::new());
        rows[0] = metadata_snapshot_row(0xb4);
        let harness = Harness::with_responses([Ok(rows)]);

        let error = harness
            .service(1, 1)
            .read(&project())
            .await
            .expect_err("metadata 改变必须阻止继续规划");

        assert!(matches!(
            error,
            MzStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                expected,
                actual,
            } if expected == SourceSnapshotFingerprint::from_bytes([0xa5; 32])
                && actual == SourceSnapshotFingerprint::from_bytes([0xb4; 32])
        ));
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn query_terminal_states_keep_database_path_and_source() {
        for response in [
            Err(QueryExistingDatabaseError::NotFound),
            Err(QueryExistingDatabaseError::QueryFailed(FakeError("read"))),
        ] {
            let harness = Harness::new(response);
            let error = harness
                .service(1, 1)
                .read(&project())
                .await
                .expect_err("查询错误应该传播");
            match error {
                MzStandardTranslationAssetReadingError::DatabaseNotFound { database_path } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                }
                MzStandardTranslationAssetReadingError::Query {
                    database_path,
                    source,
                } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("read"));
                }
                other => panic!("未预期的读取错误：{other}"),
            }
        }
    }

    struct Harness {
        query_calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        response: SharedQueryResponse,
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(response: QueryResponse) -> Self {
            Self::with_responses([response])
        }

        fn with_responses(responses: impl IntoIterator<Item = QueryResponse>) -> Self {
            Self {
                query_calls: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(responses.into_iter().collect())),
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn service(
            &self,
            decode_concurrency: usize,
            leaves_per_job: usize,
        ) -> MzStandardTranslationAssetReadingService<RecordingQuery, RecordingCpu> {
            MzStandardTranslationAssetReadingService::new(
                RecordingQuery {
                    calls: Arc::clone(&self.query_calls),
                    response: Arc::clone(&self.response),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                    fail: false,
                },
                MzStandardAssetReadingConfig::new(
                    non_zero(decode_concurrency),
                    non_zero(leaves_per_job),
                ),
            )
        }
    }

    fn row(
        table: &str,
        exact_location: &MzLocation,
        group_location: &MzLocation,
        field_name: &str,
        payload: [SqliteValue; 3],
    ) -> SqliteRow {
        let [original_text, translation, translation_state] = payload;
        SqliteRow::new(vec![
            SqliteValue::Text(table.to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(exact_location).expect("位置应可编码")),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(group_location).expect("位置应可编码")),
            SqliteValue::Text(field_name.to_owned()),
            SqliteValue::Null,
            original_text,
            translation,
            translation_state,
        ])
    }

    fn active_owners<const N: usize>(owners: [&'static str; N]) -> BTreeSet<&'static str> {
        owners.into_iter().collect()
    }

    fn owner_rows(owner: &str, byte: u8) -> Vec<SqliteRow> {
        vec![SqliteRow::new(vec![
            SqliteValue::Text(owner.to_owned()),
            SqliteValue::Blob(vec![byte; 32]),
        ])]
    }

    fn resource_rows() -> Vec<SqliteRow> {
        vec![
            SqliteRow::new(vec![
                SqliteValue::Text(TERMINOLOGY_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]),
            SqliteRow::new(vec![
                SqliteValue::Text(PLACEHOLDER_RULES_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]),
        ]
    }

    fn snapshot_rows(
        owner_rows: Vec<SqliteRow>,
        resource_rows: Vec<SqliteRow>,
        asset_rows: Vec<SqliteRow>,
    ) -> Vec<SqliteRow> {
        let mut rows = vec![metadata_snapshot_row(0xa5)];
        for owner in owner_rows {
            let mut values = owner.into_values().into_iter();
            rows.push(SqliteRow::new(vec![
                SqliteValue::Text("1_owner".to_owned()),
                values.next().expect("owner 行应有名称"),
                values.next().expect("owner 行应有指纹"),
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
            ]));
        }
        for resource in resource_rows {
            let mut values = resource.into_values().into_iter();
            rows.push(SqliteRow::new(vec![
                SqliteValue::Text("2_resource".to_owned()),
                SqliteValue::Null,
                SqliteValue::Null,
                values.next().expect("资源行应有种类"),
                values.next().expect("资源行应有 JSON"),
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
            ]));
        }
        for asset in asset_rows {
            let values = asset.into_values();
            let [
                table,
                exact,
                owner,
                group,
                field,
                unit,
                original,
                translation,
                state,
            ]: [SqliteValue; 9] = values.try_into().expect("资产测试行应为 9 列");
            rows.push(SqliteRow::new(vec![
                SqliteValue::Text("3_asset".to_owned()),
                owner,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                table,
                exact,
                group,
                field,
                unit,
                original,
                translation,
                state,
            ]));
        }
        rows
    }

    fn metadata_snapshot_row(byte: u8) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text("0_metadata".to_owned()),
            SqliteValue::Null,
            SqliteValue::Blob(vec![byte; 32]),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
        ])
    }

    fn location(steps: Vec<MzLocationStep>) -> MzLocation {
        MzLocation::value(MzSource::data(StandardDataFile::Items), steps)
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名称应该有效"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
