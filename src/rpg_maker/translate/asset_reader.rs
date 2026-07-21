//! 从统一 RPG Maker 标准文本表建立一致翻译语料。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    AssetSnapshotFingerprint, PLACEHOLDER_RULES_RESOURCE_KIND, SourceSnapshotFingerprint,
    TERMINOLOGY_RESOURCE_KIND,
};
use crate::rpg_maker::standard_asset::{
    RpgMakerStandardAssetOwner, RpgMakerStandardAssetReadingConfig,
};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardTranslationAsset, StandardTranslationAssetReader, StandardTranslationCorpus,
    StandardTranslationGroup, TranslationOwnerSnapshot, TranslationUnitIdentity,
};

const READ_TRANSLATION_SNAPSHOT: &str = r#"SELECT
    row_kind,
    owner,
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint,
    resource_kind,
    canonical_json,
    group_location,
    group_kind,
    group_order,
    unit_role,
    unit_order,
    source_content_json,
    source_context_json,
    translation_content_json,
    translation_state
FROM (
    SELECT
        '0_metadata' AS row_kind,
        NULL AS owner,
        source_snapshot_fingerprint,
        NULL AS asset_snapshot_fingerprint,
        NULL AS resource_kind,
        NULL AS canonical_json,
        NULL AS group_location,
        NULL AS group_kind,
        NULL AS group_order,
        NULL AS unit_role,
        NULL AS unit_order,
        NULL AS source_content_json,
        NULL AS source_context_json,
        NULL AS translation_content_json,
        NULL AS translation_state
    FROM metadata

    UNION ALL

    SELECT
        '1_owner',
        owner,
        source_snapshot_fingerprint,
        asset_snapshot_fingerprint,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL
    FROM standard_asset_owner_state

    UNION ALL

    SELECT
        '2_resource',
        NULL,
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
        NULL,
        NULL
    FROM standard_translation_resource

    UNION ALL

    SELECT
        '3_group',
        text_group.owner,
        NULL,
        NULL,
        NULL,
        NULL,
        text_group.group_location,
        text_group.group_kind,
        text_group.group_order,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL,
        NULL
    FROM standard_text_group AS text_group

    UNION ALL

    SELECT
        '4_unit',
        unit.owner,
        NULL,
        NULL,
        NULL,
        NULL,
        unit.group_location,
        text_group.group_kind,
        text_group.group_order,
        unit.unit_role,
        unit.unit_order,
        unit.source_content_json,
        unit.source_context_json,
        unit.translation_content_json,
        unit.translation_state
    FROM standard_text_unit AS unit
    JOIN standard_text_group AS text_group
      ON text_group.owner = unit.owner
     AND text_group.group_location = unit.group_location
)
ORDER BY
    row_kind,
    CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 WHEN 'lua' THEN 2 ELSE 3 END,
    resource_kind,
    group_order,
    unit_order"#;

/// 验证 owner 新鲜度、读取当前资源，并用受控 CPU 解码标准翻译语料。
pub(crate) struct RpgMakerStandardTranslationAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
    config: RpgMakerStandardAssetReadingConfig,
}

impl<Q, C> RpgMakerStandardTranslationAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: RpgMakerStandardAssetReadingConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<Q, C> StandardTranslationAssetReader for RpgMakerStandardTranslationAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerStandardTranslationAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
    ) -> Result<StandardTranslationCorpus, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_TRANSLATION_SNAPSHOT, Vec::new()),
            )
            .await
            .map_err(|error| map_query_error(database_path.clone(), error))?;
        let expected_source_snapshot = project.source_snapshot_fingerprint();
        let units_per_job = self.config.units_per_decode_job().get();
        let prepared = self
            .cpu
            .execute(move || prepare_snapshot(rows, expected_source_snapshot, units_per_job))
            .await
            .map_err(RpgMakerStandardTranslationAssetReadingError::SchedulePreparation)?
            .map_err(map_snapshot_preparation_error)?;
        let active_owners = Arc::new(prepared.active_owners);
        let decoded_groups = prepared.groups;
        let decoded_batches = self
            .cpu
            .execute_ordered_map(prepared.jobs, move |job| {
                decode_rows(job, active_owners.as_ref())
            })
            .await
            .map_err(RpgMakerStandardTranslationAssetReadingError::ScheduleDecode)?;
        let groups = self
            .cpu
            .execute(move || {
                let decoded = decoded_batches
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                assemble_corpus(decoded_groups, decoded)
            })
            .await
            .map_err(RpgMakerStandardTranslationAssetReadingError::ScheduleAssembly)?
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;
        Ok(StandardTranslationCorpus::with_snapshot(
            groups,
            prepared.source_snapshot_fingerprint,
            prepared.owner_snapshots,
            prepared.terminology_json,
            prepared.placeholder_rules_json,
        ))
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerStandardTranslationAssetReadingError<Q, C> {
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
        owners: Vec<RpgMakerStandardAssetOwner>,
    },
    SchedulePreparation(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardTranslationAssetSnapshot),
}

impl<Q: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerStandardTranslationAssetReadingError<Q, C>
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
            Self::ProjectSnapshotChanged { expected, actual } => write!(
                formatter,
                "项目打开后 metadata 来源指纹发生变化（预期 {expected:?}，实际 {actual:?}）"
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
            Self::SchedulePreparation(source) => {
                write!(formatter, "资产快照准备任务执行失败：{source}")
            }
            Self::ScheduleDecode(source) => write!(formatter, "资产解码任务执行失败：{source}"),
            Self::ScheduleAssembly(source) => {
                write!(formatter, "资产语料组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot(source) => write!(formatter, "标准翻译资产损坏：{source}"),
        }
    }
}

impl<Q: Error + 'static, C: Error + 'static> Error
    for RpgMakerStandardTranslationAssetReadingError<Q, C>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::SchedulePreparation(source)
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
    InvalidOrderValue {
        column: &'static str,
        actual: i64,
    },
    UnknownOwner(String),
    InactiveOwner(String),
    DuplicateOwner(String),
    InvalidOwnerSourceFingerprintLength {
        owner: String,
        actual: usize,
    },
    InvalidOwnerAssetFingerprintLength {
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
    UnknownGroupKind(String),
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidRole(RpgMakerProjectionCodecError),
    RoleDoesNotBelongToGroup {
        role: TextUnitRole,
        kind: TextGroupKind,
    },
    InvalidSourceContent(serde_json::Error),
    InvalidTranslationContent(serde_json::Error),
    SourceContentShapeMismatch {
        role: TextUnitRole,
    },
    TranslationContentShapeMismatch {
        role: TextUnitRole,
    },
    BlankSourceContent,
    BlankTranslationContent,
    InvalidSourceLineText {
        index: usize,
    },
    InvalidTranslationLineText {
        index: usize,
    },
    AlignedLineCountMismatch {
        expected: usize,
        actual: usize,
    },
    AlignedBlankSlotMismatch {
        index: usize,
    },
    InvalidSourceContext(serde_json::Error),
    SourceContextMustBeObject,
    InvalidTranslationStatePair,
    InvalidTranslationStateLength {
        actual: usize,
    },
    DuplicateGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    MissingGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    EmptyGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InvalidGroupOrder {
        owner: RpgMakerStandardAssetOwner,
        expected: usize,
        actual: usize,
    },
    InconsistentGroupDefinition {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InvalidUnitOrder {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
        expected: usize,
        actual: usize,
    },
    DuplicateLogicalUnit {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
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
            Self::InvalidOrderValue { column, actual } => {
                write!(
                    formatter,
                    "列 {column} 必须是可表示的非负顺序，实际为 {actual}"
                )
            }
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::InactiveOwner(owner) => write!(formatter, "文本单元引用未激活 owner：{owner}"),
            Self::DuplicateOwner(owner) => write!(formatter, "资产 owner 状态重复：{owner}"),
            Self::InvalidOwnerSourceFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {owner} 的来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::InvalidOwnerAssetFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {owner} 的资产指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::InvalidMetadataRowCount { actual } => {
                write!(formatter, "metadata 必须恰好一行，实际为 {actual} 行")
            }
            Self::InvalidMetadataFingerprintLength { actual } => write!(
                formatter,
                "metadata 来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::MissingTranslationResource(kind) => write!(formatter, "缺少翻译资源 {kind}"),
            Self::DuplicateTranslationResource(kind) => write!(formatter, "翻译资源重复：{kind}"),
            Self::UnknownTranslationResource(kind) => write!(formatter, "未知翻译资源：{kind}"),
            Self::BlankTranslationResource(kind) => write!(formatter, "翻译资源 {kind} 为空"),
            Self::UnknownGroupKind(kind) => write!(formatter, "未知文本组类型：{kind}"),
            Self::InvalidLocation(source) => write!(formatter, "组位置无效：{source}"),
            Self::InvalidRole(source) => write!(formatter, "单元角色无效：{source}"),
            Self::RoleDoesNotBelongToGroup { role, kind } => {
                write!(formatter, "单元角色 {role:?} 不属于文本组 {kind:?}")
            }
            Self::InvalidSourceContent(source) => {
                write!(formatter, "source_content_json 无效：{source}")
            }
            Self::InvalidTranslationContent(source) => {
                write!(formatter, "translation_content_json 无效：{source}")
            }
            Self::SourceContentShapeMismatch { role } => {
                write!(formatter, "源内容形状不符合单元角色 {role:?}")
            }
            Self::TranslationContentShapeMismatch { role } => {
                write!(formatter, "译文内容形状不符合单元角色 {role:?}")
            }
            Self::BlankSourceContent => formatter.write_str("标准文本源内容仅包含空白"),
            Self::BlankTranslationContent => formatter.write_str("标准文本译文内容仅包含空白"),
            Self::InvalidSourceLineText { index } => {
                write!(formatter, "源内容第 {index} 行包含 CR、LF 或 NUL")
            }
            Self::InvalidTranslationLineText { index } => {
                write!(formatter, "译文内容第 {index} 行包含 CR、LF 或 NUL")
            }
            Self::AlignedLineCountMismatch { expected, actual } => write!(
                formatter,
                "严格对齐译文应包含 {expected} 行，实际为 {actual} 行"
            ),
            Self::AlignedBlankSlotMismatch { index } => {
                write!(formatter, "严格对齐译文第 {index} 行改变了源空槽状态")
            }
            Self::InvalidSourceContext(source) => {
                write!(formatter, "source_context_json 无效：{source}")
            }
            Self::SourceContextMustBeObject => {
                formatter.write_str("source_context_json 必须是 JSON 对象")
            }
            Self::InvalidTranslationStatePair => formatter
                .write_str("translation_content_json 与 translation_state 必须同时存在或同时为空"),
            Self::InvalidTranslationStateLength { actual } => write!(
                formatter,
                "translation_state 必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::DuplicateGroup {
                owner,
                group_location,
            } => write!(
                formatter,
                "资产组重复：{} / {group_location}",
                owner.storage_name()
            ),
            Self::MissingGroup {
                owner,
                group_location,
            } => write!(
                formatter,
                "文本单元没有对应资产组：{} / {group_location}",
                owner.storage_name()
            ),
            Self::EmptyGroup {
                owner,
                group_location,
            } => write!(
                formatter,
                "资产组不包含文本单元：{} / {group_location}",
                owner.storage_name()
            ),
            Self::InvalidGroupOrder {
                owner,
                expected,
                actual,
            } => write!(
                formatter,
                "owner {} 的 group_order 必须从 0 连续：期待 {expected}，实际 {actual}",
                owner.storage_name()
            ),
            Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => write!(
                formatter,
                "同一资产组的类型或 group_order 不一致：{} / {group_location}",
                owner.storage_name()
            ),
            Self::InvalidUnitOrder {
                owner,
                group_location,
                expected,
                actual,
            } => write!(
                formatter,
                "组 {} / {group_location} 的 unit_order 必须从 0 连续：期待 {expected}，实际 {actual}",
                owner.storage_name()
            ),
            Self::DuplicateLogicalUnit {
                owner,
                group_location,
                role,
            } => write!(
                formatter,
                "同一语义文本单元重复：{} / {group_location} / {role:?}",
                owner.storage_name()
            ),
        }
    }
}

impl Error for InvalidStandardTranslationAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidRole(source) => Some(source),
            Self::InvalidSourceContent(source)
            | Self::InvalidTranslationContent(source)
            | Self::InvalidSourceContext(source) => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> RpgMakerStandardTranslationAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerStandardTranslationAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerStandardTranslationAssetReadingError::Query {
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
    groups: Vec<SqliteRow>,
    units: Vec<SqliteRow>,
}

struct PreparedSnapshot {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    owner_snapshots: Vec<TranslationOwnerSnapshot>,
    active_owners: BTreeSet<&'static str>,
    terminology_json: String,
    placeholder_rules_json: String,
    groups: Vec<DecodedGroup>,
    jobs: Vec<Vec<SqliteRow>>,
}

enum SnapshotPreparationError {
    Invalid(InvalidStandardTranslationAssetSnapshot),
    ProjectSnapshotChanged {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    ExtractionOutOfDate {
        owners: Vec<RpgMakerStandardAssetOwner>,
    },
}

fn prepare_snapshot(
    rows: Vec<SqliteRow>,
    expected_source_snapshot: SourceSnapshotFingerprint,
    units_per_job: usize,
) -> Result<PreparedSnapshot, SnapshotPreparationError> {
    let snapshot = split_snapshot_rows(rows).map_err(SnapshotPreparationError::Invalid)?;
    let source_snapshot_fingerprint =
        decode_metadata(snapshot.metadata).map_err(SnapshotPreparationError::Invalid)?;
    if source_snapshot_fingerprint != expected_source_snapshot {
        return Err(SnapshotPreparationError::ProjectSnapshotChanged {
            expected: expected_source_snapshot,
            actual: source_snapshot_fingerprint,
        });
    }

    let owner_states = decode_owner_states(snapshot.owners, source_snapshot_fingerprint)
        .map_err(SnapshotPreparationError::Invalid)?;
    if !owner_states.stale.is_empty() {
        return Err(SnapshotPreparationError::ExtractionOutOfDate {
            owners: owner_states.stale,
        });
    }
    let (terminology_json, placeholder_rules_json) =
        decode_resources(snapshot.resources).map_err(SnapshotPreparationError::Invalid)?;
    let groups = decode_groups(snapshot.groups, &owner_states.active)
        .map_err(SnapshotPreparationError::Invalid)?;

    Ok(PreparedSnapshot {
        source_snapshot_fingerprint,
        owner_snapshots: owner_states.snapshots,
        active_owners: owner_states.active,
        terminology_json,
        placeholder_rules_json,
        groups,
        jobs: partition_rows(snapshot.units, units_per_job),
    })
}

fn map_snapshot_preparation_error<Q, C>(
    error: SnapshotPreparationError,
) -> RpgMakerStandardTranslationAssetReadingError<Q, C> {
    match error {
        SnapshotPreparationError::Invalid(source) => {
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot(source)
        }
        SnapshotPreparationError::ProjectSnapshotChanged { expected, actual } => {
            RpgMakerStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                expected,
                actual,
            }
        }
        SnapshotPreparationError::ExtractionOutOfDate { owners } => {
            RpgMakerStandardTranslationAssetReadingError::ExtractionOutOfDate { owners }
        }
    }
}

fn split_snapshot_rows(
    rows: Vec<SqliteRow>,
) -> Result<SnapshotRows, InvalidStandardTranslationAssetSnapshot> {
    let mut snapshot = SnapshotRows {
        metadata: Vec::new(),
        owners: Vec::new(),
        resources: Vec::new(),
        groups: Vec::new(),
        units: Vec::new(),
    };
    for row in rows {
        let values = row.into_values();
        let actual = values.len();
        let [
            row_kind,
            owner,
            source_fingerprint,
            asset_fingerprint,
            resource_kind,
            canonical_json,
            group_location,
            group_kind,
            group_order,
            unit_role,
            unit_order,
            source_content_json,
            source_context_json,
            translation_content_json,
            translation_state,
        ]: [SqliteValue; 15] = values.try_into().map_err(|_| {
            InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 15,
                actual,
            }
        })?;
        match required_text(row_kind, "row_kind")?.as_str() {
            "0_metadata" => snapshot
                .metadata
                .push(SqliteRow::new(vec![source_fingerprint])),
            "1_owner" => snapshot.owners.push(SqliteRow::new(vec![
                owner,
                source_fingerprint,
                asset_fingerprint,
            ])),
            "2_resource" => snapshot
                .resources
                .push(SqliteRow::new(vec![resource_kind, canonical_json])),
            "3_group" => snapshot.groups.push(SqliteRow::new(vec![
                owner,
                group_location,
                group_kind,
                group_order,
            ])),
            "4_unit" => snapshot.units.push(SqliteRow::new(vec![
                owner,
                group_location,
                group_kind,
                group_order,
                unit_role,
                unit_order,
                source_content_json,
                source_context_json,
                translation_content_json,
                translation_state,
            ])),
            unknown => {
                return Err(
                    InvalidStandardTranslationAssetSnapshot::UnknownSnapshotRowKind(
                        unknown.to_owned(),
                    ),
                );
            }
        }
    }
    Ok(snapshot)
}

fn decode_metadata(
    rows: Vec<SqliteRow>,
) -> Result<SourceSnapshotFingerprint, InvalidStandardTranslationAssetSnapshot> {
    if rows.len() != 1 {
        return Err(
            InvalidStandardTranslationAssetSnapshot::InvalidMetadataRowCount { actual: rows.len() },
        );
    }
    let mut values = rows.into_iter().next().expect("已确认有一行").into_values();
    if values.len() != 1 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 1,
            actual: values.len(),
        });
    }
    let value = values.pop().expect("已确认有一列");
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
    stale: Vec<RpgMakerStandardAssetOwner>,
    active: BTreeSet<&'static str>,
    snapshots: Vec<TranslationOwnerSnapshot>,
}

fn decode_owner_states(
    rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
) -> Result<DecodedOwnerStates, InvalidStandardTranslationAssetSnapshot> {
    let mut active = BTreeSet::new();
    let mut stale = Vec::new();
    let mut snapshots = Vec::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 3 {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 3,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let owner_name = required_text(next(&mut values), "owner")?;
        let owner =
            RpgMakerStandardAssetOwner::from_storage_name(&owner_name).ok_or_else(|| {
                InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner_name.clone())
            })?;
        if !active.insert(owner.storage_name()) {
            return Err(InvalidStandardTranslationAssetSnapshot::DuplicateOwner(
                owner_name,
            ));
        }
        let source_bytes = required_blob(next(&mut values), "source_snapshot_fingerprint")?;
        let source = SourceSnapshotFingerprint::from_slice(&source_bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidOwnerSourceFingerprintLength {
                owner: owner.storage_name().to_owned(),
                actual: error.actual(),
            }
        })?;
        let asset_bytes = required_blob(next(&mut values), "asset_snapshot_fingerprint")?;
        let asset = AssetSnapshotFingerprint::from_slice(&asset_bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidOwnerAssetFingerprintLength {
                owner: owner.storage_name().to_owned(),
                actual: error.actual(),
            }
        })?;
        if source != current {
            stale.push(owner);
        }
        snapshots.push(TranslationOwnerSnapshot::new(owner, source, asset));
    }
    stale.sort_by_key(|owner| owner_order(*owner));
    snapshots.sort_by_key(|snapshot| owner_order(snapshot.owner()));
    Ok(DecodedOwnerStates {
        stale,
        active,
        snapshots,
    })
}

const fn owner_order(owner: RpgMakerStandardAssetOwner) -> usize {
    match owner {
        RpgMakerStandardAssetOwner::Builtin => 0,
        RpgMakerStandardAssetOwner::Rules => 1,
        RpgMakerStandardAssetOwner::Lua => 2,
    }
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

fn partition_rows(rows: Vec<SqliteRow>, units_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut rows = rows.into_iter();
    std::iter::from_fn(|| {
        let job = rows.by_ref().take(units_per_job).collect::<Vec<_>>();
        (!job.is_empty()).then_some(job)
    })
    .collect()
}

#[derive(Debug)]
struct DecodedGroup {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    group_order: usize,
}

fn decode_groups(
    rows: Vec<SqliteRow>,
    active_owners: &BTreeSet<&'static str>,
) -> Result<Vec<DecodedGroup>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|row| {
            let values = row.into_values();
            if values.len() != 4 {
                return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                    expected: 4,
                    actual: values.len(),
                });
            }
            let mut values = values.into_iter();
            let owner_name = required_text(next(&mut values), "owner")?;
            let owner = RpgMakerStandardAssetOwner::from_storage_name(&owner_name).ok_or(
                InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner_name),
            )?;
            if !active_owners.contains(owner.storage_name()) {
                return Err(InvalidStandardTranslationAssetSnapshot::InactiveOwner(
                    owner.storage_name().to_owned(),
                ));
            }
            let group_location =
                RpgMakerLocationCodec::decode(&required_text(next(&mut values), "group_location")?)
                    .map_err(InvalidStandardTranslationAssetSnapshot::InvalidLocation)?;
            let kind = decode_group_kind(&required_text(next(&mut values), "group_kind")?)?;
            let group_order = required_non_negative_order(next(&mut values), "group_order")?;
            Ok(DecodedGroup {
                owner,
                kind,
                group_location,
                group_order,
            })
        })
        .collect()
}

#[derive(Debug)]
struct DecodedUnit {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    group_order: usize,
    role: TextUnitRole,
    unit_order: usize,
    source_content: TextUnitContent,
    source_context_json: String,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
}

fn decode_rows(
    rows: Vec<SqliteRow>,
    active_owners: &BTreeSet<&'static str>,
) -> Result<Vec<DecodedUnit>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|row| decode_unit(row, active_owners))
        .collect()
}

fn decode_unit(
    row: SqliteRow,
    active_owners: &BTreeSet<&'static str>,
) -> Result<DecodedUnit, InvalidStandardTranslationAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 10 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 10,
            actual: values.len(),
        });
    }
    let mut values = values.into_iter();
    let owner_name = required_text(next(&mut values), "owner")?;
    let owner = RpgMakerStandardAssetOwner::from_storage_name(&owner_name).ok_or(
        InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner_name),
    )?;
    if !active_owners.contains(owner.storage_name()) {
        return Err(InvalidStandardTranslationAssetSnapshot::InactiveOwner(
            owner.storage_name().to_owned(),
        ));
    }
    let group_location =
        RpgMakerLocationCodec::decode(&required_text(next(&mut values), "group_location")?)
            .map_err(InvalidStandardTranslationAssetSnapshot::InvalidLocation)?;
    let kind = decode_group_kind(&required_text(next(&mut values), "group_kind")?)?;
    let group_order = required_non_negative_order(next(&mut values), "group_order")?;
    let role =
        RpgMakerProjectionCodec::decode_role(&required_text(next(&mut values), "unit_role")?)
            .map_err(InvalidStandardTranslationAssetSnapshot::InvalidRole)?;
    let unit_order = required_non_negative_order(next(&mut values), "unit_order")?;
    validate_role(&role, kind)?;
    let source_content_json = required_text(next(&mut values), "source_content_json")?;
    let source_content: TextUnitContent = serde_json::from_str(&source_content_json)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidSourceContent)?;
    if source_content.is_blank() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankSourceContent);
    }
    if role.expects_lines() != source_content.as_lines().is_some() {
        return Err(
            InvalidStandardTranslationAssetSnapshot::SourceContentShapeMismatch {
                role: role.clone(),
            },
        );
    }
    let source_context_json = required_text(next(&mut values), "source_context_json")?;
    let context: serde_json::Value = serde_json::from_str(&source_context_json)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidSourceContext)?;
    if !context.is_object() {
        return Err(InvalidStandardTranslationAssetSnapshot::SourceContextMustBeObject);
    }
    let translation_content_json = optional_text(next(&mut values), "translation_content_json")?;
    let translation = translation_content_json
        .map(|translation| {
            serde_json::from_str::<TextUnitContent>(&translation)
                .map_err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationContent)
        })
        .transpose()?;
    if translation.as_ref().is_some_and(TextUnitContent::is_blank) {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslationContent);
    }
    if translation
        .as_ref()
        .is_some_and(|translation| translation.as_lines().is_some() != role.expects_lines())
    {
        return Err(
            InvalidStandardTranslationAssetSnapshot::TranslationContentShapeMismatch {
                role: role.clone(),
            },
        );
    }
    validate_persisted_content(&role, &source_content, translation.as_ref())?;
    let translation_state = optional_blob(next(&mut values), "translation_state")?;
    let translation_state = match (translation.as_ref(), translation_state) {
        (None, None) => None,
        (Some(_), Some(bytes)) => Some(Sha256Fingerprint::from_slice(&bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidTranslationStateLength {
                actual: error.actual(),
            }
        })?),
        _ => return Err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationStatePair),
    };
    Ok(DecodedUnit {
        owner,
        kind,
        group_location,
        group_order,
        role,
        unit_order,
        source_content,
        source_context_json,
        translation,
        translation_state,
    })
}

fn validate_persisted_content(
    role: &TextUnitRole,
    source: &TextUnitContent,
    translation: Option<&TextUnitContent>,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    if let Some(lines) = source.as_lines() {
        if let Some(index) = lines.iter().position(|line| contains_line_separator(line)) {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidSourceLineText { index });
        }
    } else if matches!(role, TextUnitRole::DialogueSpeaker)
        && source.as_value().is_some_and(contains_line_separator)
    {
        return Err(InvalidStandardTranslationAssetSnapshot::InvalidSourceLineText { index: 0 });
    }

    let Some(translation) = translation else {
        return Ok(());
    };
    if let Some(lines) = translation.as_lines() {
        if let Some(index) = lines.iter().position(|line| contains_line_separator(line)) {
            return Err(
                InvalidStandardTranslationAssetSnapshot::InvalidTranslationLineText { index },
            );
        }
    } else if matches!(role, TextUnitRole::DialogueSpeaker)
        && translation.as_value().is_some_and(contains_line_separator)
    {
        return Err(
            InvalidStandardTranslationAssetSnapshot::InvalidTranslationLineText { index: 0 },
        );
    }

    if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
        let source_lines = source.as_lines().expect("严格对齐角色的源内容形状已验证");
        let translation_lines = translation
            .as_lines()
            .expect("严格对齐角色的译文内容形状已验证");
        if source_lines.len() != translation_lines.len() {
            return Err(
                InvalidStandardTranslationAssetSnapshot::AlignedLineCountMismatch {
                    expected: source_lines.len(),
                    actual: translation_lines.len(),
                },
            );
        }
        if let Some(index) =
            source_lines
                .iter()
                .zip(translation_lines)
                .position(|(source, translation)| {
                    source.trim().is_empty() != translation.trim().is_empty()
                })
        {
            return Err(
                InvalidStandardTranslationAssetSnapshot::AlignedBlankSlotMismatch { index },
            );
        }
    }
    Ok(())
}

fn contains_line_separator(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
}

fn decode_group_kind(
    value: &str,
) -> Result<TextGroupKind, InvalidStandardTranslationAssetSnapshot> {
    match value {
        "database_entry" => Ok(TextGroupKind::DatabaseEntry),
        "system" => Ok(TextGroupKind::System),
        "map" => Ok(TextGroupKind::Map),
        "event_dialogue" => Ok(TextGroupKind::EventDialogue),
        "event_choices" => Ok(TextGroupKind::EventChoices),
        "event_scrolling_text" => Ok(TextGroupKind::EventScrollingText),
        "event_command" => Ok(TextGroupKind::EventCommand),
        "plugin_parameter" => Ok(TextGroupKind::PluginParameter),
        unknown => Err(InvalidStandardTranslationAssetSnapshot::UnknownGroupKind(
            unknown.to_owned(),
        )),
    }
}

fn validate_role(
    role: &TextUnitRole,
    kind: TextGroupKind,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    let valid = match role {
        TextUnitRole::DialogueSpeaker | TextUnitRole::DialogueBody => {
            kind == TextGroupKind::EventDialogue
        }
        TextUnitRole::Choices => kind == TextGroupKind::EventChoices,
        TextUnitRole::ScrollingText => kind == TextGroupKind::EventScrollingText,
        TextUnitRole::Scalar(_) => true,
    };
    if valid {
        Ok(())
    } else {
        Err(
            InvalidStandardTranslationAssetSnapshot::RoleDoesNotBelongToGroup {
                role: role.clone(),
                kind,
            },
        )
    }
}

fn assemble_corpus(
    group_rows: Vec<DecodedGroup>,
    units: Vec<DecodedUnit>,
) -> Result<Vec<StandardTranslationGroup>, InvalidStandardTranslationAssetSnapshot> {
    struct GroupBuilder {
        owner: RpgMakerStandardAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        group_order: usize,
        assets: Vec<StandardTranslationAsset>,
    }

    let mut next_group_orders = BTreeMap::<&'static str, usize>::new();
    let mut group_indexes = BTreeMap::<(&'static str, RpgMakerLocation), usize>::new();
    let mut groups = Vec::<GroupBuilder>::new();
    for group in group_rows {
        let owner_name = group.owner.storage_name();
        let expected = next_group_orders.entry(owner_name).or_default();
        if group.group_order != *expected {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidGroupOrder {
                owner: group.owner,
                expected: *expected,
                actual: group.group_order,
            });
        }
        *expected += 1;
        let key = (owner_name, group.group_location.clone());
        if group_indexes.contains_key(&key) {
            return Err(InvalidStandardTranslationAssetSnapshot::DuplicateGroup {
                owner: group.owner,
                group_location: Box::new(group.group_location),
            });
        }
        let index = groups.len();
        group_indexes.insert(key, index);
        groups.push(GroupBuilder {
            owner: group.owner,
            kind: group.kind,
            group_location: group.group_location,
            group_order: group.group_order,
            assets: Vec::new(),
        });
    }

    let mut seen = BTreeSet::new();
    for unit in units {
        let owner_name = unit.owner.storage_name();
        let group_index = group_indexes
            .get(&(owner_name, unit.group_location.clone()))
            .copied()
            .ok_or_else(|| InvalidStandardTranslationAssetSnapshot::MissingGroup {
                owner: unit.owner,
                group_location: Box::new(unit.group_location.clone()),
            })?;
        let group = &mut groups[group_index];
        if group.kind != unit.kind || group.group_order != unit.group_order {
            return Err(
                InvalidStandardTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                },
            );
        }
        let expected_unit_order = group.assets.len();
        if unit.unit_order != expected_unit_order {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidUnitOrder {
                owner: unit.owner,
                group_location: Box::new(unit.group_location),
                expected: expected_unit_order,
                actual: unit.unit_order,
            });
        }
        let key = (owner_name, unit.group_location.clone(), unit.role.clone());
        if !seen.insert(key) {
            return Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateLogicalUnit {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                    role: unit.role,
                },
            );
        }
        let identity = TranslationUnitIdentity::new(
            unit.owner,
            unit.kind,
            unit.group_location,
            unit.role,
            unit.source_content,
            unit.source_context_json,
        );
        group.assets.push(StandardTranslationAsset::new(
            identity,
            unit.translation,
            unit.translation_state,
        ));
    }

    if let Some(group) = groups.iter().find(|group| group.assets.is_empty()) {
        return Err(InvalidStandardTranslationAssetSnapshot::EmptyGroup {
            owner: group.owner,
            group_location: Box::new(group.group_location.clone()),
        });
    }
    Ok(groups
        .into_iter()
        .map(|group| StandardTranslationGroup::new(group.kind, group.group_location, group.assets))
        .collect())
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，标准文本查询行必须具有完整投影")
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

fn required_blob(
    value: SqliteValue,
    column: &'static str,
) -> Result<Vec<u8>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Blob(value) => Ok(value),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "BLOB",
            actual: actual.kind_name(),
        }),
    }
}

fn required_non_negative_order(
    value: SqliteValue,
    column: &'static str,
) -> Result<usize, InvalidStandardTranslationAssetSnapshot> {
    let SqliteValue::Integer(value) = value else {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "INTEGER",
            actual: value.kind_name(),
        });
    };
    usize::try_from(value).map_err(
        |_| InvalidStandardTranslationAssetSnapshot::InvalidOrderValue {
            column,
            actual: value,
        },
    )
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource};

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeQuery {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        rows: Arc<Mutex<Option<Vec<SqliteRow>>>>,
    }

    impl SqliteQueryExecutor for FakeQuery {
        type Error = FakeError;

        fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
        {
            self.calls.lock().expect("查询锁").push((path, query));
            let rows = self.rows.lock().expect("响应锁").take().expect("单次响应");
            async move { Ok(rows) }
        }

        async fn query_existing_database_snapshot(
            &self,
            path: PathBuf,
            queries: Vec<SqliteQuery>,
        ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
            let mut results = Vec::with_capacity(queries.len());
            for query in queries {
                results.push(self.query_existing_database(path.clone(), query).await?);
            }
            Ok(results)
        }
    }

    #[derive(Clone, Copy)]
    struct InlineCpu;

    impl CpuTaskExecutor for InlineCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[tokio::test]
    async fn unified_tables_preserve_persisted_unit_order_context_and_asset_baseline() {
        let group = dialogue_group();
        let speaker_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueSpeaker)
            .expect("角色应可编码");
        let body_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let rows = snapshot_rows(
            &group,
            vec![
                unit_row(
                    &group,
                    "event_dialogue",
                    &body_role,
                    0,
                    r#"["同一句"]"#,
                    r#"{"source_speaker":"甲"}"#,
                ),
                unit_row(&group, "event_dialogue", &speaker_role, 1, r#""甲""#, "{}"),
            ],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = RpgMakerStandardTranslationAssetReadingService::new(
            FakeQuery {
                calls: Arc::clone(&calls),
                rows: Arc::new(Mutex::new(Some(rows))),
            },
            InlineCpu,
            RpgMakerStandardAssetReadingConfig::new(non_zero(1)),
        );

        let corpus = service.read(&project()).await.expect("统一表应可读取");

        assert_eq!(corpus.groups().len(), 1);
        let assets = corpus.groups()[0].assets();
        assert_eq!(assets[0].identity().role(), &TextUnitRole::DialogueBody);
        assert_eq!(
            assets[0].identity().source_context_json(),
            r#"{"source_speaker":"甲"}"#
        );
        assert_eq!(
            assets[0].identity().source_content(),
            &TextUnitContent::Lines(vec!["同一句".to_owned()])
        );
        let (_, baseline) = corpus.into_parts();
        assert_eq!(baseline.owner_snapshots().len(), 1);
        assert_eq!(
            baseline.owner_snapshots()[0].asset_snapshot_fingerprint(),
            AssetSnapshotFingerprint::from_bytes([0xb4; 32])
        );
        let calls = calls.lock().expect("查询锁");
        assert!(calls[0].1.statement().contains("standard_text_unit"));
    }

    #[test]
    fn corpus_keeps_builtin_rules_lua_owner_order_and_independent_same_location_groups() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let owners = [
            RpgMakerStandardAssetOwner::Builtin,
            RpgMakerStandardAssetOwner::Rules,
            RpgMakerStandardAssetOwner::Lua,
        ];
        let group_rows = owners
            .into_iter()
            .map(|owner| DecodedGroup {
                owner,
                kind: TextGroupKind::DatabaseEntry,
                group_location: group_location.clone(),
                group_order: 0,
            })
            .collect::<Vec<_>>();
        let units = owners
            .into_iter()
            .map(|owner| DecodedUnit {
                owner,
                kind: TextGroupKind::DatabaseEntry,
                group_location: group_location.clone(),
                group_order: 0,
                role: TextUnitRole::Scalar(
                    ScalarFieldKey::new(format!("{}_name", owner.storage_name()))
                        .expect("测试角色应合法"),
                ),
                unit_order: 0,
                source_content: TextUnitContent::Value(owner.storage_name().to_owned()),
                source_context_json: "{}".to_owned(),
                translation: None,
                translation_state: None,
            })
            .collect::<Vec<_>>();

        let groups = assemble_corpus(group_rows, units).expect("三 owner 语料应能组装");

        assert_eq!(groups.len(), 3, "同一逻辑位置不得跨 owner 合并");
        assert_eq!(
            groups
                .iter()
                .map(|group| group.assets()[0].identity().owner())
                .collect::<Vec<_>>(),
            owners,
            "Standard 总顺序必须固定为 Builtin、Rules、Lua"
        );
        assert!(READ_TRANSLATION_SNAPSHOT.contains("WHEN 'builtin' THEN 0"));
        assert!(READ_TRANSLATION_SNAPSHOT.contains("WHEN 'rules' THEN 1"));
        assert!(READ_TRANSLATION_SNAPSHOT.contains("WHEN 'lua' THEN 2"));
    }

    #[test]
    fn body_context_must_be_a_json_object() {
        let role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let error = decode_unit(
            unit_payload_row(
                &dialogue_group(),
                "event_dialogue",
                &role,
                r#"["正文"]"#,
                "[]",
            ),
            &BTreeSet::from(["builtin"]),
        )
        .expect_err("数组不能充当源上下文");
        assert!(matches!(
            error,
            InvalidStandardTranslationAssetSnapshot::SourceContextMustBeObject
        ));
    }

    #[test]
    fn persisted_content_must_match_the_semantic_unit_shape() {
        let body_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let mut values = unit_payload_row(
            &dialogue_group(),
            "event_dialogue",
            &body_role,
            r#"["正文"]"#,
            "{}",
        )
        .into_values();
        values[8] = text(r#""错误形状""#);
        values[9] = SqliteValue::Blob(vec![0x44; 32]);

        let error = decode_unit(SqliteRow::new(values), &BTreeSet::from(["builtin"]))
            .expect_err("正文译文必须保持 Lines 形状");
        assert!(matches!(
            error,
            InvalidStandardTranslationAssetSnapshot::TranslationContentShapeMismatch {
                role: TextUnitRole::DialogueBody
            }
        ));
    }

    fn snapshot_rows(group: &RpgMakerLocation, units: Vec<SqliteRow>) -> Vec<SqliteRow> {
        let mut rows = vec![
            snapshot_row(
                "0_metadata",
                SqliteValue::Null,
                SqliteValue::Blob(vec![0xa5; 32]),
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                null_tail(),
            ),
            snapshot_row(
                "1_owner",
                text("builtin"),
                SqliteValue::Blob(vec![0xa5; 32]),
                SqliteValue::Blob(vec![0xb4; 32]),
                SqliteValue::Null,
                SqliteValue::Null,
                null_tail(),
            ),
            snapshot_row(
                "2_resource",
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                text(TERMINOLOGY_RESOURCE_KIND),
                text("[]"),
                null_tail(),
            ),
            snapshot_row(
                "2_resource",
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                text(PLACEHOLDER_RULES_RESOURCE_KIND),
                text("[]"),
                null_tail(),
            ),
        ];
        rows.push(snapshot_row(
            "3_group",
            text("builtin"),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            [
                text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
                text("event_dialogue"),
                SqliteValue::Integer(0),
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        ));
        rows.extend(units);
        rows
    }

    fn unit_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        unit_order: i64,
        source_content_json: &str,
        context: &str,
    ) -> SqliteRow {
        snapshot_row(
            "4_unit",
            text("builtin"),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            [
                text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
                text(kind),
                SqliteValue::Integer(0),
                text(role),
                SqliteValue::Integer(unit_order),
                text(source_content_json),
                text(context),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        )
    }

    fn unit_payload_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        source_content_json: &str,
        context: &str,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            text("builtin"),
            text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
            text(kind),
            SqliteValue::Integer(0),
            text(role),
            SqliteValue::Integer(0),
            text(source_content_json),
            text(context),
            SqliteValue::Null,
            SqliteValue::Null,
        ])
    }

    fn snapshot_row(
        kind: &str,
        owner: SqliteValue,
        source: SqliteValue,
        asset: SqliteValue,
        resource_kind: SqliteValue,
        canonical_json: SqliteValue,
        tail: [SqliteValue; 9],
    ) -> SqliteRow {
        let mut values = vec![
            text(kind),
            owner,
            source,
            asset,
            resource_kind,
            canonical_json,
        ];
        values.extend(tail);
        SqliteRow::new(values)
    }

    fn null_tail() -> [SqliteValue; 9] {
        std::array::from_fn(|_| SqliteValue::Null)
    }

    fn dialogue_group() -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        )
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }

    fn text(value: impl Into<String>) -> SqliteValue {
        SqliteValue::Text(value.into())
    }
}
