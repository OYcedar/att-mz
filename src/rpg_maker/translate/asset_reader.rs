//! 从统一 RPG Maker 标准文本表建立一致翻译语料。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::TextFieldRole;
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
    StandardTranslationGroup, TranslationLeafIdentity, TranslationOwnerSnapshot,
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
    field_role,
    original_text,
    translation_context_json,
    translation,
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
        NULL AS field_role,
        NULL AS original_text,
        NULL AS translation_context_json,
        NULL AS translation,
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
        NULL
    FROM standard_translation_resource

    UNION ALL

    SELECT
        '3_leaf',
        leaf.owner,
        NULL,
        NULL,
        NULL,
        NULL,
        leaf.group_location,
        text_group.group_kind,
        leaf.field_role,
        leaf.original_text,
        leaf.translation_context_json,
        leaf.translation,
        leaf.translation_state
    FROM standard_text_leaf AS leaf
    JOIN standard_text_group AS text_group
      ON text_group.owner = leaf.owner
     AND text_group.group_location = leaf.group_location
)
ORDER BY row_kind, owner, resource_kind, group_location, field_role"#;

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
        let snapshot = split_snapshot_rows(rows)
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;
        let source_snapshot_fingerprint = decode_metadata(snapshot.metadata)
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;
        if source_snapshot_fingerprint != project.source_snapshot_fingerprint() {
            return Err(
                RpgMakerStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                    expected: project.source_snapshot_fingerprint(),
                    actual: source_snapshot_fingerprint,
                },
            );
        }

        let owner_states = decode_owner_states(snapshot.owners, source_snapshot_fingerprint)
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;
        if !owner_states.stale.is_empty() {
            return Err(
                RpgMakerStandardTranslationAssetReadingError::ExtractionOutOfDate {
                    owners: owner_states.stale,
                },
            );
        }
        let (terminology_json, placeholder_rules_json) = decode_resources(snapshot.resources)
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;

        let jobs = partition_rows(snapshot.leaves, self.config.leaves_per_decode_job().get());
        let decoded_batches = stream::iter(jobs.into_iter().map(|job| {
            let active_owners = owner_states.active.clone();
            async move {
                self.cpu
                    .execute(move || decode_rows(job, &active_owners))
                    .await
                    .map_err(RpgMakerStandardTranslationAssetReadingError::ScheduleDecode)?
                    .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)
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
            .map_err(RpgMakerStandardTranslationAssetReadingError::ScheduleAssembly)?
            .map_err(RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot)?;
        Ok(StandardTranslationCorpus::with_snapshot(
            groups,
            source_snapshot_fingerprint,
            owner_states.snapshots,
            terminology_json,
            placeholder_rules_json,
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
            Self::ScheduleDecode(source) | Self::ScheduleAssembly(source) => Some(source),
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
        role: TextFieldRole,
        kind: TextGroupKind,
    },
    BlankOriginalText,
    BlankTranslation,
    InvalidTranslationContext(serde_json::Error),
    TranslationContextMustBeObject,
    InvalidTranslationStatePair,
    InvalidTranslationStateLength {
        actual: usize,
    },
    DuplicateLogicalLeaf {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
        role: TextFieldRole,
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
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::InactiveOwner(owner) => write!(formatter, "文本叶引用未激活 owner：{owner}"),
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
            Self::InvalidRole(source) => write!(formatter, "字段角色无效：{source}"),
            Self::RoleDoesNotBelongToGroup { role, kind } => {
                write!(formatter, "字段角色 {role:?} 不属于文本组 {kind:?}")
            }
            Self::BlankOriginalText => formatter.write_str("标准文本原文仅包含空白"),
            Self::BlankTranslation => formatter.write_str("标准文本译文仅包含空白"),
            Self::InvalidTranslationContext(source) => {
                write!(formatter, "translation_context_json 无效：{source}")
            }
            Self::TranslationContextMustBeObject => {
                formatter.write_str("translation_context_json 必须是 JSON 对象")
            }
            Self::InvalidTranslationStatePair => {
                formatter.write_str("translation 与 translation_state 必须同时存在或同时为空")
            }
            Self::InvalidTranslationStateLength { actual } => write!(
                formatter,
                "translation_state 必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::DuplicateLogicalLeaf {
                owner,
                group_location,
                role,
            } => write!(
                formatter,
                "同一逻辑文本叶重复：{} / {group_location} / {role:?}",
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
            Self::InvalidTranslationContext(source) => Some(source),
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
    leaves: Vec<SqliteRow>,
}

fn split_snapshot_rows(
    rows: Vec<SqliteRow>,
) -> Result<SnapshotRows, InvalidStandardTranslationAssetSnapshot> {
    let mut snapshot = SnapshotRows {
        metadata: Vec::new(),
        owners: Vec::new(),
        resources: Vec::new(),
        leaves: Vec::new(),
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
            field_role,
            original_text,
            translation_context_json,
            translation,
            translation_state,
        ]: [SqliteValue; 13] = values.try_into().map_err(|_| {
            InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 13,
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
            "3_leaf" => snapshot.leaves.push(SqliteRow::new(vec![
                owner,
                group_location,
                group_kind,
                field_role,
                original_text,
                translation_context_json,
                translation,
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

fn partition_rows(rows: Vec<SqliteRow>, leaves_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut rows = rows.into_iter();
    std::iter::from_fn(|| {
        let job = rows.by_ref().take(leaves_per_job).collect::<Vec<_>>();
        (!job.is_empty()).then_some(job)
    })
    .collect()
}

#[derive(Debug)]
struct DecodedLeaf {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    role: TextFieldRole,
    original_text: String,
    translation_context_json: String,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
}

fn decode_rows(
    rows: Vec<SqliteRow>,
    active_owners: &BTreeSet<&'static str>,
) -> Result<Vec<DecodedLeaf>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|row| decode_leaf(row, active_owners))
        .collect()
}

fn decode_leaf(
    row: SqliteRow,
    active_owners: &BTreeSet<&'static str>,
) -> Result<DecodedLeaf, InvalidStandardTranslationAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 8 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 8,
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
    let role =
        RpgMakerProjectionCodec::decode_role(&required_text(next(&mut values), "field_role")?)
            .map_err(InvalidStandardTranslationAssetSnapshot::InvalidRole)?;
    validate_role(&role, kind)?;
    let original_text = required_text(next(&mut values), "original_text")?;
    if original_text.trim().is_empty() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankOriginalText);
    }
    let translation_context_json = required_text(next(&mut values), "translation_context_json")?;
    let context: serde_json::Value = serde_json::from_str(&translation_context_json)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationContext)?;
    if !context.is_object() {
        return Err(InvalidStandardTranslationAssetSnapshot::TranslationContextMustBeObject);
    }
    let translation = optional_text(next(&mut values), "translation")?;
    if translation
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslation);
    }
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
    Ok(DecodedLeaf {
        owner,
        kind,
        group_location,
        role,
        original_text,
        translation_context_json,
        translation,
        translation_state,
    })
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
    role: &TextFieldRole,
    kind: TextGroupKind,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    let valid = match role {
        TextFieldRole::DialogueSpeaker | TextFieldRole::DialogueBody { .. } => {
            kind == TextGroupKind::EventDialogue
        }
        TextFieldRole::ScrollingTextBody { .. } => kind == TextGroupKind::EventScrollingText,
        TextFieldRole::Scalar(_) => true,
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
    leaves: Vec<DecodedLeaf>,
) -> Result<Vec<StandardTranslationGroup>, InvalidStandardTranslationAssetSnapshot> {
    let mut seen = BTreeSet::new();
    let mut groups =
        BTreeMap::<(TextGroupKind, RpgMakerLocation), Vec<StandardTranslationAsset>>::new();
    for leaf in leaves {
        let key = (
            leaf.owner.storage_name(),
            leaf.group_location.clone(),
            leaf.role.clone(),
        );
        if !seen.insert(key) {
            return Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateLogicalLeaf {
                    owner: leaf.owner,
                    group_location: Box::new(leaf.group_location),
                    role: leaf.role,
                },
            );
        }
        let kind = leaf.kind;
        let group_location = leaf.group_location.clone();
        let identity = TranslationLeafIdentity::new(
            leaf.owner,
            kind,
            leaf.group_location,
            leaf.role,
            leaf.original_text,
            leaf.translation_context_json,
        );
        groups
            .entry((kind, group_location))
            .or_default()
            .push(StandardTranslationAsset::new(
                identity,
                leaf.translation,
                leaf.translation_state,
            ));
    }
    Ok(groups
        .into_iter()
        .map(|((kind, group_location), mut assets)| {
            assets.sort_by(|left, right| {
                left.identity()
                    .role()
                    .cmp(right.identity().role())
                    .then_with(|| {
                        owner_order(left.identity().owner())
                            .cmp(&owner_order(right.identity().owner()))
                    })
            });
            StandardTranslationGroup::new(kind, group_location, assets)
        })
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
    use crate::rpg_maker::model::TextFieldRole;
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
    async fn unified_tables_preserve_role_order_context_and_asset_baseline() {
        let group = dialogue_group();
        let speaker_role = RpgMakerProjectionCodec::encode_role(&TextFieldRole::DialogueSpeaker)
            .expect("角色应可编码");
        let body_role =
            RpgMakerProjectionCodec::encode_role(&TextFieldRole::DialogueBody { index: 0 })
                .expect("角色应可编码");
        let rows = snapshot_rows(vec![
            leaf_row(
                &group,
                "event_dialogue",
                &body_role,
                "同一句",
                r#"{"source_speaker":"甲"}"#,
            ),
            leaf_row(&group, "event_dialogue", &speaker_role, "甲", "{}"),
        ]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = RpgMakerStandardTranslationAssetReadingService::new(
            FakeQuery {
                calls: Arc::clone(&calls),
                rows: Arc::new(Mutex::new(Some(rows))),
            },
            InlineCpu,
            RpgMakerStandardAssetReadingConfig::new(non_zero(2), non_zero(1)),
        );

        let corpus = service.read(&project()).await.expect("统一表应可读取");

        assert_eq!(corpus.groups().len(), 1);
        let assets = corpus.groups()[0].assets();
        assert_eq!(assets[0].identity().role(), &TextFieldRole::DialogueSpeaker);
        assert_eq!(
            assets[1].identity().translation_context_json(),
            r#"{"source_speaker":"甲"}"#
        );
        let (_, baseline) = corpus.into_parts();
        assert_eq!(baseline.owner_snapshots().len(), 1);
        assert_eq!(
            baseline.owner_snapshots()[0].asset_snapshot_fingerprint(),
            AssetSnapshotFingerprint::from_bytes([0xb4; 32])
        );
        let calls = calls.lock().expect("查询锁");
        assert!(calls[0].1.statement().contains("standard_text_leaf"));
    }

    #[test]
    fn body_context_must_be_a_json_object() {
        let role = RpgMakerProjectionCodec::encode_role(&TextFieldRole::DialogueBody { index: 0 })
            .expect("角色应可编码");
        let error = decode_leaf(
            leaf_payload_row(&dialogue_group(), "event_dialogue", &role, "正文", "[]"),
            &BTreeSet::from(["builtin"]),
        )
        .expect_err("数组不能充当翻译上下文");
        assert!(matches!(
            error,
            InvalidStandardTranslationAssetSnapshot::TranslationContextMustBeObject
        ));
    }

    fn snapshot_rows(leaves: Vec<SqliteRow>) -> Vec<SqliteRow> {
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
        rows.extend(leaves);
        rows
    }

    fn leaf_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        original: &str,
        context: &str,
    ) -> SqliteRow {
        snapshot_row(
            "3_leaf",
            text("builtin"),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            [
                text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
                text(kind),
                text(role),
                text(original),
                text(context),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        )
    }

    fn leaf_payload_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        original: &str,
        context: &str,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            text("builtin"),
            text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
            text(kind),
            text(role),
            text(original),
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
        tail: [SqliteValue; 7],
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

    fn null_tail() -> [SqliteValue; 7] {
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
