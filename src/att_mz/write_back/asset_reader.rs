//! 从五张 MZ 标准资产表建立不含术语数据的写回快照。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::project::OpenedProject;
use crate::att_mz::project_database::SourceSnapshotFingerprint;
use crate::att_mz::standard_asset::{
    MzStandardAssetLocationError, MzStandardAssetOwner, MzStandardAssetReadingConfig,
    MzStandardAssetStorageKind, MzStandardAssetTable, MzTextBodyUnit,
};
use crate::att_mz::text::{MzLocation, TextGroupKind};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardWriteBackAssetReader, StandardWriteBackFieldRole, StandardWriteBackGroup,
    StandardWriteBackLeaf, StandardWriteBackSnapshot, StandardWriteBackSnapshotError,
};

const READ_STANDARD_WRITE_BACK_SNAPSHOT: &str = r#"SELECT
    'owner_state' AS record_kind,
    NULL AS asset_table,
    NULL AS exact_location,
    owner,
    NULL AS group_location,
    NULL AS field_name,
    NULL AS unit_type,
    NULL AS original_text,
    NULL AS translation,
    source_snapshot_fingerprint
FROM standard_asset_owner_state

UNION ALL

SELECT
    'asset' AS record_kind,
    'entry' AS asset_table,
    exact_location,
    owner,
    group_location,
    field_name,
    NULL AS unit_type,
    original_text,
    translation,
    NULL AS source_snapshot_fingerprint
FROM entry

UNION ALL

SELECT
    'asset',
    'system_text',
    exact_location,
    owner,
    group_location,
    field_name,
    NULL,
    original_text,
    translation,
    NULL
FROM system_text

UNION ALL

SELECT
    'asset',
    'map_text',
    exact_location,
    owner,
    group_location,
    field_name,
    NULL,
    original_text,
    translation,
    NULL
FROM map_text

UNION ALL

SELECT
    'asset',
    'text_body',
    exact_location,
    owner,
    group_location,
    field_name,
    unit_type,
    original_text,
    translation,
    NULL
FROM text_body

UNION ALL

SELECT
    'asset',
    'plugin_param',
    exact_location,
    owner,
    group_location,
    field_name,
    NULL,
    original_text,
    translation,
    NULL
FROM plugin_param

ORDER BY record_kind DESC, owner, asset_table, exact_location"#;

/// 先验证 active owner 新鲜度，再用受控 CPU 解码建立 Standard 写回快照。
pub(crate) struct MzStandardWriteBackAssetReadingService<Q, C> {
    sqlite: Arc<Q>,
    cpu: Arc<C>,
    config: MzStandardAssetReadingConfig,
}

impl<Q, C> MzStandardWriteBackAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: MzStandardAssetReadingConfig) -> Self {
        Self {
            sqlite: Arc::new(sqlite),
            cpu: Arc::new(cpu),
            config,
        }
    }
}

impl<Q, C> StandardWriteBackAssetReader for MzStandardWriteBackAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzStandardWriteBackAssetReadingError<Q::Error, C::Error>;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<StandardWriteBackSnapshot, Self::Error>> + Send + use<Q, C>
    {
        let database_path = project.database_path().to_path_buf();
        let current_source = project.source_snapshot_fingerprint();
        let sqlite = Arc::clone(&self.sqlite);
        let cpu = Arc::clone(&self.cpu);
        let leaves_per_job = self.config.leaves_per_decode_job().get();
        let decode_concurrency = self.config.decode_concurrency().get();

        async move {
            let rows = sqlite
                .query_existing_database(
                    database_path.clone(),
                    SqliteQuery::new(READ_STANDARD_WRITE_BACK_SNAPSHOT, Vec::new()),
                )
                .await
                .map_err(|error| map_query_error(database_path, error))?;

            let prepared = cpu
                .execute(move || prepare_snapshot_rows(rows, current_source, leaves_per_job))
                .await
                .map_err(MzStandardWriteBackAssetReadingError::SchedulePartition)?
                .map_err(MzStandardWriteBackAssetReadingError::InvalidSnapshot)?;
            let PreparedSnapshotRows {
                stale_owners,
                active_owners,
                batches,
            } = prepared;
            if !stale_owners.is_empty() {
                return Err(MzStandardWriteBackAssetReadingError::ExtractionOutOfDate {
                    owners: stale_owners,
                });
            }
            if batches.is_empty() {
                return Ok(StandardWriteBackSnapshot::empty());
            }

            let decoded_batches = stream::iter(batches.into_iter().map(|batch| {
                let active_owners = active_owners.clone();
                let cpu = Arc::clone(&cpu);
                async move {
                    cpu.execute(move || decode_rows(batch, &active_owners))
                        .await
                        .map_err(MzStandardWriteBackAssetReadingError::ScheduleDecode)?
                        .map_err(MzStandardWriteBackAssetReadingError::InvalidSnapshot)
                }
            }))
            .buffered(decode_concurrency)
            .try_collect::<Vec<_>>()
            .await?;

            let decoded = decoded_batches.into_iter().flatten().collect::<Vec<_>>();
            cpu.execute(move || assemble_snapshot(decoded))
                .await
                .map_err(MzStandardWriteBackAssetReadingError::ScheduleAssembly)?
                .map_err(MzStandardWriteBackAssetReadingError::InvalidSnapshot)
        }
    }
}

/// 标准写回资产读取责任的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzStandardWriteBackAssetReadingError<Q, C> {
    DatabaseNotFound { database_path: PathBuf },
    Query { database_path: PathBuf, source: Q },
    ExtractionOutOfDate { owners: Vec<MzStandardAssetOwner> },
    SchedulePartition(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardWriteBackAssetSnapshot),
}

impl<Q, C> fmt::Display for MzStandardWriteBackAssetReadingError<Q, C>
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
                "无法从 {} 读取标准写回资产：{source}",
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
            Self::SchedulePartition(source) => {
                write!(formatter, "写回资产解码分批任务执行失败：{source}")
            }
            Self::ScheduleDecode(source) => {
                write!(formatter, "写回资产解码任务执行失败：{source}")
            }
            Self::ScheduleAssembly(source) => {
                write!(formatter, "写回资产快照组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot(source) => write!(formatter, "标准写回资产损坏：{source}"),
        }
    }
}

impl<Q, C> Error for MzStandardWriteBackAssetReadingError<Q, C>
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
            Self::DatabaseNotFound { .. } | Self::ExtractionOutOfDate { .. } => None,
        }
    }
}

#[derive(Debug)]
struct DecodedRow {
    kind: TextGroupKind,
    exact_location: MzLocation,
    group_location: MzLocation,
    role: StandardWriteBackFieldRole,
    original_text: String,
    translation: Option<String>,
}

/// 数据库内容违反五表 schema、角色编码或写回快照不变量的原因。
#[derive(Debug)]
pub(crate) enum InvalidStandardWriteBackAssetSnapshot {
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    UnknownAssetTable(String),
    UnknownSnapshotRecordKind(String),
    UnknownOwner(String),
    DuplicateOwner(String),
    InvalidOwnerFingerprintLength {
        owner: String,
        actual: usize,
    },
    AssetOwnerWithoutState(String),
    UnknownUnitType(String),
    UnexpectedUnitType {
        table: String,
    },
    InvalidFieldRole {
        kind: TextGroupKind,
        field_name: String,
    },
    InvalidLocation {
        column: &'static str,
        source: MzLocationCodecError,
    },
    InvalidStorageLocation(MzStandardAssetLocationError),
    InvalidModel(StandardWriteBackSnapshotError),
}

impl fmt::Display for InvalidStandardWriteBackAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongColumnCount { expected, actual } => {
                write!(
                    formatter,
                    "写回资产查询行应包含 {expected} 列，实际为 {actual} 列"
                )
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "列 {column} 应为 {expected}，实际为 {actual}"),
            Self::UnknownAssetTable(table) => write!(formatter, "未知标准资产表：{table}"),
            Self::UnknownSnapshotRecordKind(kind) => {
                write!(formatter, "未知写回快照记录类型：{kind}")
            }
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::DuplicateOwner(owner) => write!(formatter, "资产所有者状态重复：{owner}"),
            Self::InvalidOwnerFingerprintLength { owner, actual } => write!(
                formatter,
                "资产所有者 {owner} 的来源快照指纹应为 32 字节，实际为 {actual} 字节"
            ),
            Self::AssetOwnerWithoutState(owner) => {
                write!(formatter, "资产所有者没有 active owner state：{owner}")
            }
            Self::UnknownUnitType(unit_type) => {
                write!(formatter, "未知文本单元类型：{unit_type}")
            }
            Self::UnexpectedUnitType { table } => {
                write!(formatter, "资产表 {table} 的 unit_type 与表语义不一致")
            }
            Self::InvalidFieldRole { kind, field_name } => {
                write!(formatter, "组类型 {kind:?} 包含非规范字段角色 {field_name}")
            }
            Self::InvalidLocation { column, source } => {
                write!(formatter, "列 {column} 中的结构化位置无效：{source}")
            }
            Self::InvalidStorageLocation(source) => {
                write!(formatter, "结构化位置与标准资产存储语义不一致：{source}")
            }
            Self::InvalidModel(source) => source.fmt(formatter),
        }
    }
}

impl Error for InvalidStandardWriteBackAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation { source, .. } => Some(source),
            Self::InvalidStorageLocation(source) => Some(source),
            Self::InvalidModel(source) => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> MzStandardWriteBackAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            MzStandardWriteBackAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            MzStandardWriteBackAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

struct PreparedSnapshotRows {
    stale_owners: Vec<MzStandardAssetOwner>,
    active_owners: BTreeSet<&'static str>,
    batches: Vec<Vec<SqliteRow>>,
}

fn prepare_snapshot_rows(
    rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
    leaves_per_job: usize,
) -> Result<PreparedSnapshotRows, InvalidStandardWriteBackAssetSnapshot> {
    let (owner_rows, asset_rows) = split_snapshot_rows(rows)?;
    let (stale_owners, active_owners) = decode_owner_states(owner_rows, current)?;
    Ok(PreparedSnapshotRows {
        stale_owners,
        active_owners,
        batches: partition_rows(asset_rows, leaves_per_job),
    })
}

fn split_snapshot_rows(
    rows: Vec<SqliteRow>,
) -> Result<(Vec<SqliteRow>, Vec<SqliteRow>), InvalidStandardWriteBackAssetSnapshot> {
    let mut owner_rows = Vec::new();
    let mut asset_rows = Vec::new();
    for row in rows {
        let values = row.into_values();
        let actual = values.len();
        let [
            record_kind,
            asset_table,
            exact_location,
            owner,
            group_location,
            field_name,
            unit_type,
            original_text,
            translation,
            source_snapshot_fingerprint,
        ] = <[SqliteValue; 10]>::try_from(values).map_err(|_| {
            InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 10,
                actual,
            }
        })?;
        match required_text(record_kind, "record_kind")?.as_str() {
            "owner_state" => {
                required_null(asset_table, "asset_table")?;
                required_null(exact_location, "exact_location")?;
                required_null(group_location, "group_location")?;
                required_null(field_name, "field_name")?;
                required_null(unit_type, "unit_type")?;
                required_null(original_text, "original_text")?;
                required_null(translation, "translation")?;
                owner_rows.push(SqliteRow::new(vec![owner, source_snapshot_fingerprint]));
            }
            "asset" => {
                required_null(source_snapshot_fingerprint, "source_snapshot_fingerprint")?;
                asset_rows.push(SqliteRow::new(vec![
                    asset_table,
                    exact_location,
                    owner,
                    group_location,
                    field_name,
                    unit_type,
                    original_text,
                    translation,
                ]));
            }
            kind => {
                return Err(
                    InvalidStandardWriteBackAssetSnapshot::UnknownSnapshotRecordKind(
                        kind.to_owned(),
                    ),
                );
            }
        }
    }
    Ok((owner_rows, asset_rows))
}

fn partition_rows(rows: Vec<SqliteRow>, leaves_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut rows = rows.into_iter();
    let mut batches = Vec::new();
    loop {
        let batch = rows.by_ref().take(leaves_per_job).collect::<Vec<_>>();
        if batch.is_empty() {
            return batches;
        }
        batches.push(batch);
    }
}

fn decode_owner_states(
    rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
) -> Result<
    (Vec<MzStandardAssetOwner>, BTreeSet<&'static str>),
    InvalidStandardWriteBackAssetSnapshot,
> {
    let mut active = BTreeSet::new();
    let mut stale = Vec::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 2,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let owner_name = required_text(next(&mut values), "owner")?;
        let owner = MzStandardAssetOwner::from_storage_name(&owner_name).ok_or_else(|| {
            InvalidStandardWriteBackAssetSnapshot::UnknownOwner(owner_name.clone())
        })?;
        if !active.insert(owner.storage_name()) {
            return Err(InvalidStandardWriteBackAssetSnapshot::DuplicateOwner(
                owner_name,
            ));
        }
        let fingerprint_value = next(&mut values);
        let SqliteValue::Blob(bytes) = fingerprint_value else {
            return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
                column: "source_snapshot_fingerprint",
                expected: "BLOB",
                actual: fingerprint_value.kind_name(),
            });
        };
        let fingerprint = SourceSnapshotFingerprint::from_slice(&bytes).map_err(|error| {
            InvalidStandardWriteBackAssetSnapshot::InvalidOwnerFingerprintLength {
                owner: owner.storage_name().to_owned(),
                actual: error.actual(),
            }
        })?;
        if fingerprint != current {
            stale.push(owner);
        }
    }
    stale.sort_by_key(|owner| match owner {
        MzStandardAssetOwner::Builtin => 0,
        MzStandardAssetOwner::Rules => 1,
        MzStandardAssetOwner::Lua => 2,
    });
    Ok((stale, active))
}

fn decode_rows(
    rows: Vec<SqliteRow>,
    active_owners: &BTreeSet<&'static str>,
) -> Result<Vec<DecodedRow>, InvalidStandardWriteBackAssetSnapshot> {
    rows.into_iter()
        .map(|row| decode_row(row, active_owners))
        .collect()
}

fn decode_row(
    row: SqliteRow,
    active_owners: &BTreeSet<&'static str>,
) -> Result<DecodedRow, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 8 {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 8,
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

    let table = MzStandardAssetTable::from_storage_name(&table_name).ok_or_else(|| {
        InvalidStandardWriteBackAssetSnapshot::UnknownAssetTable(table_name.clone())
    })?;
    let owner = MzStandardAssetOwner::from_storage_name(&owner)
        .ok_or(InvalidStandardWriteBackAssetSnapshot::UnknownOwner(owner))?;
    if !active_owners.contains(owner.storage_name()) {
        return Err(
            InvalidStandardWriteBackAssetSnapshot::AssetOwnerWithoutState(
                owner.storage_name().to_owned(),
            ),
        );
    }
    let unit_type = unit_type
        .as_deref()
        .map(|value| {
            MzTextBodyUnit::from_storage_name(value).ok_or_else(|| {
                InvalidStandardWriteBackAssetSnapshot::UnknownUnitType(value.to_owned())
            })
        })
        .transpose()?;
    let storage = MzStandardAssetStorageKind::from_parts(table, unit_type).ok_or_else(|| {
        InvalidStandardWriteBackAssetSnapshot::UnexpectedUnitType {
            table: table_name.clone(),
        }
    })?;
    let kind = storage.group_kind();
    let exact_location = MzLocationCodec::decode(&exact_location).map_err(|source| {
        InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
            column: "exact_location",
            source,
        }
    })?;
    let group_location = MzLocationCodec::decode(&group_location).map_err(|source| {
        InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
            column: "group_location",
            source,
        }
    })?;
    storage
        .validate_locations(&exact_location, &group_location)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidStorageLocation)?;
    let role = role_for(kind, field_name)?;

    Ok(DecodedRow {
        kind,
        exact_location,
        group_location,
        role,
        original_text,
        translation,
    })
}

fn role_for(
    kind: TextGroupKind,
    field_name: String,
) -> Result<StandardWriteBackFieldRole, InvalidStandardWriteBackAssetSnapshot> {
    match kind {
        TextGroupKind::EventDialogue if field_name == "speaker" => {
            Ok(StandardWriteBackFieldRole::dialogue_speaker())
        }
        TextGroupKind::EventDialogue => parse_body_index(&field_name)
            .map(StandardWriteBackFieldRole::dialogue_body)
            .ok_or(InvalidStandardWriteBackAssetSnapshot::InvalidFieldRole { kind, field_name }),
        TextGroupKind::EventScrollingText => parse_body_index(&field_name)
            .map(StandardWriteBackFieldRole::scrolling_text_body)
            .ok_or(InvalidStandardWriteBackAssetSnapshot::InvalidFieldRole { kind, field_name }),
        _ => Ok(StandardWriteBackFieldRole::scalar(field_name)),
    }
}

fn parse_body_index(field_name: &str) -> Option<usize> {
    let digits = field_name.strip_prefix("body[")?.strip_suffix(']')?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，写回资产查询行必须具有完整投影")
}

fn required_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        actual => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        actual => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn required_null(
    value: SqliteValue,
    column: &'static str,
) -> Result<(), InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(()),
        actual => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn assemble_snapshot(
    rows: Vec<DecodedRow>,
) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
    // owner 只拥有提取快照与新鲜度，不改变真实 MZ 文档中的组身份。不同 owner 对同一
    // 业务对象贡献互不冲突的叶时必须合并，才能形成一次完整且确定性的文档改写。
    let mut groups = BTreeMap::<(TextGroupKind, MzLocation), Vec<StandardWriteBackLeaf>>::new();
    for row in rows {
        let leaf = StandardWriteBackLeaf::new(
            row.role,
            row.exact_location,
            row.original_text,
            row.translation,
        )
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?;
        groups
            .entry((row.kind, row.group_location))
            .or_default()
            .push(leaf);
    }

    let groups = groups
        .into_iter()
        .map(|((kind, group_location), leaves)| {
            StandardWriteBackGroup::new(kind, group_location, leaves)
                .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
        })
        .collect::<Result<Vec<_>, _>>()?;
    StandardWriteBackSnapshot::new(groups)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
    use crate::att_mz::project::{MaxFullwidthChars, MzWriteBackLayoutProfile};
    use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile};

    use super::*;

    type QueryResponse = Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeError>>;
    type SharedQueryResponse = Arc<Mutex<Option<QueryResponse>>>;

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
                .take()
                .expect("一次快照读取只应提交一条 SQLite statement");
            async move { response }
        }
    }

    #[derive(Clone)]
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        fail_at: Option<usize>,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_at == Some(call) {
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

    #[tokio::test]
    async fn one_snapshot_statement_builds_all_storage_kinds_without_terminology() {
        let dialogue_group = command_location(2, 10);
        let scrolling_group = command_location(4, 20);
        let rows = vec![
            replace_column(
                row(
                    "plugin_param",
                    &plugin_location(),
                    &plugin_group_location(),
                    "Name",
                    None,
                    "插件",
                    Some("Plugin"),
                ),
                2,
                SqliteValue::Text("rules".to_owned()),
            ),
            row(
                "text_body",
                &command_parameter_location(5, 31, 0),
                &command_location(5, 30),
                "parameter[0]",
                Some("event_command"),
                "事件原文",
                Some("Event"),
            ),
            row(
                "text_body",
                &command_parameter_location(4, 21, 0),
                &scrolling_group,
                "body[0]",
                Some("scrolling_text"),
                "滚动原文",
                Some("Scroll"),
            ),
            row(
                "text_body",
                &command_parameter_location(3, 11, 0),
                &command_location(3, 10),
                "choice[0]",
                Some("choices"),
                "选项原文",
                None,
            ),
            row(
                "text_body",
                &command_parameter_location(2, 11, 0),
                &dialogue_group,
                "body[0]",
                Some("dialogue"),
                "对话原文",
                Some("Dialogue"),
            ),
            row(
                "text_body",
                &command_parameter_location(2, 10, 4),
                &dialogue_group,
                "speaker",
                Some("dialogue"),
                "角色",
                Some("Actor"),
            ),
            scalar_row(
                "map_text",
                MzSource::map(1),
                "displayName",
                "地图",
                Some("Map"),
            ),
            scalar_row(
                "system_text",
                MzSource::data(StandardDataFile::System),
                "gameTitle",
                "标题",
                Some("Title"),
            ),
            scalar_row(
                "entry",
                MzSource::data(StandardDataFile::Items),
                "name",
                "药水",
                Some("Potion"),
            ),
        ];
        let harness = Harness::new(Ok(rows));

        let snapshot = harness
            .service(3, 2, None)
            .read(&project())
            .await
            .expect("写回快照应读取成功");

        let calls = harness.query_calls.lock().expect("查询调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(calls[0].1.statement(), READ_STANDARD_WRITE_BACK_SNAPSHOT);
        assert!(calls[0].1.parameters().is_empty());
        assert_eq!(calls[0].1.statement().matches("UNION ALL").count(), 5);
        assert!(
            calls[0]
                .1
                .statement()
                .contains("'owner_state' AS record_kind")
        );
        assert!(
            calls[0]
                .1
                .statement()
                .contains("owner, asset_table, exact_location")
        );
        assert!(!calls[0].1.statement().contains("terminology"));
        assert_eq!(snapshot.groups().len(), 8);
        assert_eq!(
            snapshot
                .groups()
                .iter()
                .map(StandardWriteBackGroup::kind)
                .collect::<Vec<_>>(),
            vec![
                TextGroupKind::DatabaseEntry,
                TextGroupKind::System,
                TextGroupKind::Map,
                TextGroupKind::EventDialogue,
                TextGroupKind::EventChoices,
                TextGroupKind::EventScrollingText,
                TextGroupKind::EventCommand,
                TextGroupKind::PluginParameter,
            ]
        );
        let dialogue = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventDialogue)
            .expect("对话组应存在");
        assert!(matches!(
            dialogue.leaves()[0].role(),
            StandardWriteBackFieldRole::DialogueSpeaker
        ));
        assert!(matches!(
            dialogue.leaves()[1].role(),
            StandardWriteBackFieldRole::DialogueBody { index: 0 }
        ));
        assert_eq!(
            snapshot
                .groups()
                .iter()
                .flat_map(StandardWriteBackGroup::leaves)
                .filter(|leaf| leaf.translation().is_none())
                .count(),
            1
        );
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_query_returns_empty_snapshot_without_cpu_work() {
        let harness = Harness::new(Ok(Vec::new()));

        let snapshot = harness
            .service(1, 1, None)
            .read(&project())
            .await
            .expect("空写回快照合法");

        assert!(snapshot.groups().is_empty());
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_active_owners_stop_before_the_five_asset_tables_are_read() {
        let harness = Harness::with_response(Ok(combined_snapshot_rows(
            owner_state_rows(&[
                (MzStandardAssetOwner::Lua, 0x11),
                (MzStandardAssetOwner::Rules, 0x22),
                (MzStandardAssetOwner::Builtin, 0xa5),
            ]),
            vec![scalar_row(
                "entry",
                MzSource::data(StandardDataFile::Items),
                "name",
                "剑",
                Some("Sword"),
            )],
        )));

        let error = harness
            .service(1, 1, None)
            .read(&project())
            .await
            .expect_err("任一 active owner 过期都必须停止写回读取");

        assert!(matches!(
            error,
            MzStandardWriteBackAssetReadingError::ExtractionOutOfDate { owners }
                if owners == vec![MzStandardAssetOwner::Rules, MzStandardAssetOwner::Lua]
        ));
        let calls = harness.query_calls.lock().expect("查询记录锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1.statement(), READ_STANDARD_WRITE_BACK_SNAPSHOT);
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_asset_row_without_active_owner_state_is_database_corruption() {
        let mut orphan = scalar_row(
            "entry",
            MzSource::data(StandardDataFile::Items),
            "name",
            "剑",
            Some("Sword"),
        )
        .into_values();
        orphan[2] = SqliteValue::Text("rules".to_owned());
        let harness = Harness::with_response(Ok(combined_snapshot_rows(
            owner_state_rows(&[(MzStandardAssetOwner::Builtin, 0xa5)]),
            vec![SqliteRow::new(orphan)],
        )));

        let error = harness
            .service(1, 1, None)
            .read(&project())
            .await
            .expect_err("没有 owner state 的资产行必须视为数据库损坏");

        assert!(matches!(
            error,
            MzStandardWriteBackAssetReadingError::InvalidSnapshot(
                InvalidStandardWriteBackAssetSnapshot::AssetOwnerWithoutState(owner)
            ) if owner == "rules"
        ));
    }

    #[test]
    fn owner_state_rows_require_unique_known_owners_and_exact_fingerprints() {
        assert!(matches!(
            decode_owner_states(
                owner_state_rows(&[
                    (MzStandardAssetOwner::Builtin, 0xa5),
                    (MzStandardAssetOwner::Builtin, 0xa5),
                ]),
                project().source_snapshot_fingerprint(),
            ),
            Err(InvalidStandardWriteBackAssetSnapshot::DuplicateOwner(owner))
                if owner == "builtin"
        ));
        assert!(matches!(
            decode_owner_states(
                vec![SqliteRow::new(vec![
                    SqliteValue::Text("lua".to_owned()),
                    SqliteValue::Blob(vec![0; 31]),
                ])],
                project().source_snapshot_fingerprint(),
            ),
            Err(
                InvalidStandardWriteBackAssetSnapshot::InvalidOwnerFingerprintLength {
                    owner,
                    actual: 31,
                }
            ) if owner == "lua"
        ));
    }

    #[tokio::test]
    async fn rows_from_different_owners_merge_by_their_real_mz_group() {
        let source = MzSource::data(StandardDataFile::Items);
        let group = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let first = row(
            "entry",
            &MzLocation::value(
                source.clone(),
                vec![MzLocationStep::index(1), MzLocationStep::key("name")],
            ),
            &group,
            "name",
            None,
            "剑",
            Some("Sword"),
        );
        let mut second = row(
            "entry",
            &MzLocation::value(
                source,
                vec![MzLocationStep::index(1), MzLocationStep::key("description")],
            ),
            &group,
            "description",
            None,
            "说明",
            Some("Description"),
        )
        .into_values();
        second[2] = SqliteValue::Text("rules".to_owned());
        let harness = Harness::new(Ok(vec![first, SqliteRow::new(second)]));

        let snapshot = harness
            .service(1, 10, None)
            .read(&project())
            .await
            .expect("不同 owner 对同一 MZ 对象的互补叶应合并");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].leaves().len(), 2);
        assert_eq!(
            snapshot.groups()[0]
                .leaves()
                .iter()
                .map(|leaf| leaf.original_text())
                .collect::<Vec<_>>(),
            ["说明", "剑"]
        );
    }

    #[tokio::test]
    async fn rows_from_different_owners_still_reject_the_same_exact_location() {
        let source = MzSource::data(StandardDataFile::Items);
        let exact = MzLocation::value(
            source.clone(),
            vec![MzLocationStep::index(1), MzLocationStep::key("name")],
        );
        let group = MzLocation::value(source, vec![MzLocationStep::index(1)]);
        let first = row("entry", &exact, &group, "name", None, "剑", Some("Sword"));
        let second = replace_column(
            row("entry", &exact, &group, "name", None, "剑", Some("Blade")),
            2,
            SqliteValue::Text("rules".to_owned()),
        );
        let harness = Harness::new(Ok(vec![first, second]));

        let error = harness
            .service(1, 10, None)
            .read(&project())
            .await
            .expect_err("不同 owner 不得覆盖同一权威 exact 位置");

        assert!(matches!(
            error,
            MzStandardWriteBackAssetReadingError::InvalidSnapshot(
                InvalidStandardWriteBackAssetSnapshot::InvalidModel(
                    StandardWriteBackSnapshotError::DuplicateLocation { exact_location }
                )
            ) if *exact_location == exact
        ));
    }

    #[tokio::test]
    async fn repeated_note_tag_names_with_distinct_occurrences_are_complementary_leaves() {
        let source = MzSource::data(StandardDataFile::Items);
        let container_steps = vec![MzLocationStep::index(1)];
        let group = MzLocation::value(source.clone(), container_steps.clone());
        let first = row(
            "entry",
            &MzLocation::note_tag(source.clone(), container_steps.clone(), "Help", 0),
            &group,
            "Help",
            None,
            "第一段",
            Some("First"),
        );
        let second = replace_column(
            row(
                "entry",
                &MzLocation::note_tag(source, container_steps, "Help", 1),
                &group,
                "Help",
                None,
                "第二段",
                Some("Second"),
            ),
            2,
            SqliteValue::Text("rules".to_owned()),
        );
        let harness = Harness::new(Ok(vec![first, second]));

        let snapshot = harness
            .service(1, 10, None)
            .read(&project())
            .await
            .expect("同标签的不同 occurrence 是两个合法权威位置");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].leaves().len(), 2);
    }

    #[test]
    fn real_sqlite_union_merges_complementary_owner_leaves() {
        let connection = rusqlite::Connection::open_in_memory().expect("内存 SQLite 应可打开");
        connection
            .execute_batch(
                r#"
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT NOT NULL,
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE entry (
                    exact_location TEXT NOT NULL,
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT
                );
                CREATE TABLE system_text AS SELECT * FROM entry WHERE 0;
                CREATE TABLE map_text AS SELECT * FROM entry WHERE 0;
                CREATE TABLE text_body (
                    exact_location TEXT NOT NULL,
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    unit_type TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT
                );
                CREATE TABLE plugin_param AS SELECT * FROM entry WHERE 0;
                "#,
            )
            .expect("写回查询的当前表形状应可建立");

        let fingerprint = project().source_snapshot_fingerprint().as_bytes().to_vec();
        for owner in ["builtin", "rules"] {
            connection
                .execute(
                    "INSERT INTO standard_asset_owner_state VALUES (?1, ?2)",
                    rusqlite::params![owner, &fingerprint],
                )
                .expect("owner state 应可写入");
        }

        let source = MzSource::data(StandardDataFile::Items);
        let group = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let group = MzLocationCodec::encode(&group).expect("测试组位置应可编码");
        for (owner, field, original, translation) in [
            ("builtin", "name", "剑", "Sword"),
            ("rules", "description", "说明", "Description"),
        ] {
            let exact = MzLocation::value(
                source.clone(),
                vec![MzLocationStep::index(1), MzLocationStep::key(field)],
            );
            let exact = MzLocationCodec::encode(&exact).expect("测试 exact 位置应可编码");
            connection
                .execute(
                    "INSERT INTO entry VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![exact, owner, &group, field, original, translation],
                )
                .expect("标准资产行应可写入");
        }

        let mut statement = connection
            .prepare(READ_STANDARD_WRITE_BACK_SNAPSHOT)
            .expect("生产写回快照 SQL 应可在真实 SQLite 执行");
        let column_count = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let values = (0..column_count)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(|value| match value {
                        rusqlite::types::Value::Null => SqliteValue::Null,
                        rusqlite::types::Value::Integer(value) => SqliteValue::Integer(value),
                        rusqlite::types::Value::Real(value) => SqliteValue::Real(value),
                        rusqlite::types::Value::Text(value) => SqliteValue::Text(value),
                        rusqlite::types::Value::Blob(value) => SqliteValue::Blob(value),
                    })
                    .collect();
                Ok(SqliteRow::new(values))
            })
            .expect("生产查询应返回快照行")
            .collect::<Result<Vec<_>, _>>()
            .expect("真实 SQLite 快照行应可读取");

        let PreparedSnapshotRows {
            stale_owners,
            active_owners,
            batches,
        } = prepare_snapshot_rows(rows, project().source_snapshot_fingerprint(), 10)
            .expect("真实 SQLite 快照应可准备");
        assert!(stale_owners.is_empty());
        let decoded = batches
            .into_iter()
            .map(|batch| decode_rows(batch, &active_owners))
            .collect::<Result<Vec<_>, _>>()
            .expect("真实 SQLite 快照应可解码")
            .into_iter()
            .flatten()
            .collect();
        let snapshot = assemble_snapshot(decoded).expect("互补 owner 叶应合成单一 MZ 组");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].leaves().len(), 2);
    }

    #[test]
    fn event_roles_accept_only_canonical_database_field_names() {
        assert_eq!(
            role_for(TextGroupKind::EventDialogue, "speaker".to_owned()).expect("speaker 应合法"),
            StandardWriteBackFieldRole::dialogue_speaker()
        );
        assert_eq!(
            role_for(TextGroupKind::EventDialogue, "body[0]".to_owned()).expect("body[0] 应合法"),
            StandardWriteBackFieldRole::dialogue_body(0)
        );
        assert_eq!(
            role_for(TextGroupKind::EventScrollingText, "body[12]".to_owned())
                .expect("body[12] 应合法"),
            StandardWriteBackFieldRole::scrolling_text_body(12)
        );
        for field_name in ["body[]", "body[01]", "body[-1]", "body[1]tail", "speaker "] {
            assert!(
                role_for(TextGroupKind::EventDialogue, field_name.to_owned()).is_err(),
                "{field_name} 不应成为对话角色"
            );
        }
        assert!(role_for(TextGroupKind::EventScrollingText, "speaker".to_owned()).is_err());
    }

    #[test]
    fn malformed_rows_are_rejected_at_the_database_boundary() {
        let location = data_location(StandardDataFile::Items, 1, "name");
        let group = data_group_location(StandardDataFile::Items, 1);
        let valid = row(
            "entry",
            &location,
            &group,
            "name",
            None,
            "剑",
            Some("Sword"),
        );
        let cases = [
            (
                SqliteRow::new(vec![SqliteValue::Text("entry".to_owned())]),
                "column-count",
            ),
            (
                replace_column(valid.clone(), 0, SqliteValue::Text("terms".to_owned())),
                "table",
            ),
            (
                replace_column(valid.clone(), 2, SqliteValue::Text("unknown".to_owned())),
                "owner",
            ),
            (
                replace_column(valid.clone(), 5, SqliteValue::Text("dialogue".to_owned())),
                "unit",
            ),
            (replace_column(valid, 6, SqliteValue::Integer(1)), "type"),
        ];

        for (row, label) in cases {
            assert!(
                decode_row(row, &active_owners()).is_err(),
                "{label} 损坏应被拒绝"
            );
        }
    }

    #[test]
    fn invalid_locations_and_unknown_text_body_units_are_rejected() {
        let group = command_location(1, 10);
        let body = command_parameter_location(1, 11, 0);
        let valid = row(
            "text_body",
            &body,
            &group,
            "body[0]",
            Some("dialogue"),
            "原文",
            Some("译文"),
        );

        assert!(matches!(
            decode_row(
                replace_column(valid.clone(), 1, SqliteValue::Text("not-json".to_owned())),
                &active_owners(),
            ),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
                column: "exact_location",
                ..
            })
        ));
        assert!(matches!(
            decode_row(
                replace_column(valid, 5, SqliteValue::Text("dialog".to_owned())),
                &active_owners(),
            ),
            Err(InvalidStandardWriteBackAssetSnapshot::UnknownUnitType(unit)) if unit == "dialog"
        ));
    }

    #[test]
    fn decoded_locations_incompatible_with_the_asset_table_are_rejected() {
        let group = data_group_location(StandardDataFile::Items, 1);
        let exact = data_location(StandardDataFile::Items, 1, "Name");

        let error = decode_row(
            row(
                "plugin_param",
                &exact,
                &group,
                "Name",
                None,
                "插件参数",
                Some("Plugin parameter"),
            ),
            &active_owners(),
        )
        .expect_err("PluginParam 不应接受 Data 来源");

        assert!(matches!(
            error,
            InvalidStandardWriteBackAssetSnapshot::InvalidStorageLocation(
                MzStandardAssetLocationError::SourceDoesNotMatchStorage {
                    storage: MzStandardAssetStorageKind::PluginParam,
                    source: MzSource::Data(StandardDataFile::Items),
                }
            )
        ));
    }

    #[test]
    fn rules_entry_on_map_with_custom_field_and_path_is_accepted() {
        let source = MzSource::map(18);
        let group = MzLocation::value(
            source.clone(),
            vec![MzLocationStep::key("rules_group"), MzLocationStep::index(6)],
        );
        let exact = MzLocation::value(
            source,
            vec![
                MzLocationStep::key("custom_rules_path"),
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("actual_name"),
            ],
        );
        let mut values = row(
            "entry",
            &exact,
            &group,
            "custom_field_name",
            None,
            "原文",
            Some("Translation"),
        )
        .into_values();
        values[2] = SqliteValue::Text("rules".to_owned());

        let decoded = decode_row(SqliteRow::new(values), &active_owners())
            .expect("合法 Rules Entry→Map 应被接受");

        assert_eq!(decoded.kind, TextGroupKind::DatabaseEntry);
        assert_eq!(decoded.exact_location, exact);
        assert_eq!(decoded.group_location, group);
        assert_eq!(
            decoded.role,
            StandardWriteBackFieldRole::scalar("custom_field_name")
        );
    }

    #[tokio::test]
    async fn snapshot_model_rejects_non_contiguous_body_indices() {
        let group = command_location(1, 10);
        let rows = vec![
            row(
                "text_body",
                &command_parameter_location(1, 11, 0),
                &group,
                "body[0]",
                Some("dialogue"),
                "第一行",
                Some("First"),
            ),
            row(
                "text_body",
                &command_parameter_location(1, 12, 0),
                &group,
                "body[2]",
                Some("dialogue"),
                "第三行",
                Some("Third"),
            ),
        ];
        let harness = Harness::new(Ok(rows));

        let error = harness
            .service(1, 10, None)
            .read(&project())
            .await
            .expect_err("不连续正文应被拒绝");

        assert!(matches!(
            error,
            MzStandardWriteBackAssetReadingError::InvalidSnapshot(
                InvalidStandardWriteBackAssetSnapshot::InvalidModel(
                    StandardWriteBackSnapshotError::NonContiguousBodyIndex {
                        expected: 1,
                        actual: 2,
                        ..
                    }
                )
            )
        ));
    }

    #[tokio::test]
    async fn query_terminal_states_keep_database_path_and_source() {
        for response in [
            Err(QueryExistingDatabaseError::NotFound),
            Err(QueryExistingDatabaseError::QueryFailed(FakeError("read"))),
        ] {
            let harness = Harness::with_response(response);
            let error = harness
                .service(1, 1, None)
                .read(&project())
                .await
                .expect_err("查询错误应传播");
            match error {
                MzStandardWriteBackAssetReadingError::DatabaseNotFound { database_path } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                }
                MzStandardWriteBackAssetReadingError::Query {
                    database_path,
                    source,
                } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("read"));
                }
                other => panic!("未预期的写回资产读取错误：{other}"),
            }
        }
    }

    #[tokio::test]
    async fn cpu_failures_report_the_exact_processing_stage() {
        for (fail_at, expected_stage) in [(1, "partition"), (2, "decode"), (3, "assembly")] {
            let harness = Harness::new(Ok(vec![scalar_row(
                "entry",
                MzSource::data(StandardDataFile::Items),
                "name",
                "剑",
                Some("Sword"),
            )]));
            let error = harness
                .service(1, 10, Some(fail_at))
                .read(&project())
                .await
                .expect_err("CPU 失败应传播");
            let actual_stage = match error {
                MzStandardWriteBackAssetReadingError::SchedulePartition(_) => "partition",
                MzStandardWriteBackAssetReadingError::ScheduleDecode(_) => "decode",
                MzStandardWriteBackAssetReadingError::ScheduleAssembly(_) => "assembly",
                other => panic!("未预期的 CPU 阶段错误：{other}"),
            };
            assert_eq!(actual_stage, expected_stage);
        }
    }

    #[test]
    fn reading_future_is_send() {
        let harness = Harness::new(Ok(Vec::new()));
        let service = harness.service(1, 1, None);
        let project = project();

        assert_send(service.read(&project));
    }

    fn assert_send(_: impl Send) {}

    struct Harness {
        query_calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        response: SharedQueryResponse,
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(asset_response: QueryResponse) -> Self {
            let response = asset_response
                .map(|asset_rows| combined_snapshot_rows(fresh_owner_rows(), asset_rows));
            Self::with_response(response)
        }

        fn with_response(response: QueryResponse) -> Self {
            Self {
                query_calls: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(Some(response))),
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn service(
            &self,
            decode_concurrency: usize,
            leaves_per_job: usize,
            fail_at: Option<usize>,
        ) -> MzStandardWriteBackAssetReadingService<RecordingQuery, RecordingCpu> {
            MzStandardWriteBackAssetReadingService::new(
                RecordingQuery {
                    calls: Arc::clone(&self.query_calls),
                    response: Arc::clone(&self.response),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                    fail_at,
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
        unit_type: Option<&str>,
        original_text: &str,
        translation: Option<&str>,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text(table.to_owned()),
            SqliteValue::Text(
                MzLocationCodec::encode(exact_location).expect("测试精确位置应可编码"),
            ),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(group_location).expect("测试组位置应可编码")),
            SqliteValue::Text(field_name.to_owned()),
            unit_type.map_or(SqliteValue::Null, |value| {
                SqliteValue::Text(value.to_owned())
            }),
            SqliteValue::Text(original_text.to_owned()),
            translation.map_or(SqliteValue::Null, |value| {
                SqliteValue::Text(value.to_owned())
            }),
        ])
    }

    fn scalar_row(
        table: &str,
        source: MzSource,
        field_name: &str,
        original_text: &str,
        translation: Option<&str>,
    ) -> SqliteRow {
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let exact_location = MzLocation::value(
            source,
            vec![MzLocationStep::index(1), MzLocationStep::key(field_name)],
        );
        row(
            table,
            &exact_location,
            &group_location,
            field_name,
            None,
            original_text,
            translation,
        )
    }

    fn data_group_location(file: StandardDataFile, index: usize) -> MzLocation {
        MzLocation::value(MzSource::data(file), vec![MzLocationStep::index(index)])
    }

    fn data_location(file: StandardDataFile, index: usize, field_name: &str) -> MzLocation {
        MzLocation::value(
            MzSource::data(file),
            vec![
                MzLocationStep::index(index),
                MzLocationStep::key(field_name),
            ],
        )
    }

    fn command_location(map_id: u32, command_index: usize) -> MzLocation {
        MzLocation::value(
            MzSource::map(map_id),
            vec![
                MzLocationStep::key("events"),
                MzLocationStep::index(1),
                MzLocationStep::key("pages"),
                MzLocationStep::index(0),
                MzLocationStep::key("list"),
                MzLocationStep::index(command_index),
            ],
        )
    }

    fn command_parameter_location(
        map_id: u32,
        command_index: usize,
        parameter_index: usize,
    ) -> MzLocation {
        let MzLocation::Value { source, mut steps } = command_location(map_id, command_index)
        else {
            unreachable!("命令位置始终是 Value")
        };
        steps.push(MzLocationStep::key("parameters"));
        steps.push(MzLocationStep::index(parameter_index));
        MzLocation::value(source, steps)
    }

    fn plugin_group_location() -> MzLocation {
        MzLocation::value(MzSource::plugin_parameter(0, "Demo", "Config"), Vec::new())
    }

    fn plugin_location() -> MzLocation {
        MzLocation::value(
            MzSource::plugin_parameter(0, "Demo", "Config"),
            vec![
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("Name"),
            ],
        )
    }

    fn replace_column(row: SqliteRow, index: usize, value: SqliteValue) -> SqliteRow {
        let mut values = row.into_values();
        values[index] = value;
        SqliteRow::new(values)
    }

    fn fresh_owner_rows() -> Vec<SqliteRow> {
        owner_state_rows(&[
            (MzStandardAssetOwner::Builtin, 0xa5),
            (MzStandardAssetOwner::Rules, 0xa5),
            (MzStandardAssetOwner::Lua, 0xa5),
        ])
    }

    fn combined_snapshot_rows(
        owner_rows: Vec<SqliteRow>,
        asset_rows: Vec<SqliteRow>,
    ) -> Vec<SqliteRow> {
        let mut rows = owner_rows
            .into_iter()
            .map(|row| {
                let [owner, fingerprint] = <[SqliteValue; 2]>::try_from(row.into_values())
                    .expect("测试 owner state 应恰好两列");
                SqliteRow::new(vec![
                    SqliteValue::Text("owner_state".to_owned()),
                    SqliteValue::Null,
                    SqliteValue::Null,
                    owner,
                    SqliteValue::Null,
                    SqliteValue::Null,
                    SqliteValue::Null,
                    SqliteValue::Null,
                    SqliteValue::Null,
                    fingerprint,
                ])
            })
            .collect::<Vec<_>>();
        rows.extend(asset_rows.into_iter().map(|row| {
            let [
                asset_table,
                exact_location,
                owner,
                group_location,
                field_name,
                unit_type,
                original_text,
                translation,
            ] = <[SqliteValue; 8]>::try_from(row.into_values()).expect("测试资产行应恰好八列");
            SqliteRow::new(vec![
                SqliteValue::Text("asset".to_owned()),
                asset_table,
                exact_location,
                owner,
                group_location,
                field_name,
                unit_type,
                original_text,
                translation,
                SqliteValue::Null,
            ])
        }));
        rows
    }

    fn owner_state_rows(owners: &[(MzStandardAssetOwner, u8)]) -> Vec<SqliteRow> {
        owners
            .iter()
            .map(|(owner, fingerprint_byte)| {
                SqliteRow::new(vec![
                    SqliteValue::Text(owner.storage_name().to_owned()),
                    SqliteValue::Blob(vec![*fingerprint_byte; 32]),
                ])
            })
            .collect()
    }

    fn active_owners() -> BTreeSet<&'static str> {
        [
            MzStandardAssetOwner::Builtin.storage_name(),
            MzStandardAssetOwner::Rules.storage_name(),
            MzStandardAssetOwner::Lua.storage_name(),
        ]
        .into_iter()
        .collect()
    }

    fn project() -> OpenedProject {
        let width = MaxFullwidthChars::new(20).expect("测试行宽应合法");
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名称应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            MzWriteBackLayoutProfile::new(width, width, width),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
