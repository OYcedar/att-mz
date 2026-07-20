//! 从 RPG Maker 标准文本三表建立写回快照。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256FramedHasher;
use crate::rpg_maker::dialogue::MvDialogueDefinitionError;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{MutationTarget, TextFieldRole, TextProjectionRecipe};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::standard_asset::{
    RpgMakerStandardAssetOwner, RpgMakerStandardAssetReadingConfig,
};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardWriteBackAssetReader, StandardWriteBackGroup, StandardWriteBackLeaf,
    StandardWriteBackSnapshot, StandardWriteBackSnapshotError,
};

const READ_STANDARD_WRITE_BACK_OWNER_STATES: &str = r#"SELECT
    owner,
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint
FROM standard_asset_owner_state
ORDER BY owner COLLATE BINARY"#;

const READ_STANDARD_WRITE_BACK_GROUPS: &str = r#"SELECT
    owner,
    group_location,
    group_kind,
    projection_recipe_json
FROM standard_text_group
ORDER BY owner COLLATE BINARY, group_location COLLATE BINARY"#;

const READ_STANDARD_WRITE_BACK_LEAVES: &str = r#"SELECT
    owner,
    group_location,
    field_role,
    original_text,
    translation_context_json,
    translation
FROM standard_text_leaf
ORDER BY owner COLLATE BINARY, group_location COLLATE BINARY, field_role COLLATE BINARY"#;

const READ_STANDARD_WRITE_BACK_TARGETS: &str = r#"SELECT
    owner,
    group_location,
    mutation_target
FROM standard_text_target
ORDER BY mutation_target COLLATE BINARY"#;

/// 先验证 active owner 与资产指纹，再用受控 CPU 解码建立写回快照。
pub(crate) struct RpgMakerStandardWriteBackAssetReadingService<Q, C> {
    sqlite: Arc<Q>,
    cpu: Arc<C>,
    config: RpgMakerStandardAssetReadingConfig,
}

impl<Q, C> RpgMakerStandardWriteBackAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: RpgMakerStandardAssetReadingConfig) -> Self {
        Self {
            sqlite: Arc::new(sqlite),
            cpu: Arc::new(cpu),
            config,
        }
    }
}

impl<Q, C> StandardWriteBackAssetReader for RpgMakerStandardWriteBackAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerStandardWriteBackAssetReadingError<Q::Error, C::Error>;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl std::future::Future<Output = Result<StandardWriteBackSnapshot, Self::Error>>
    + Send
    + use<Q, C> {
        let database_path = project.database_path().to_path_buf();
        let current_source = project.source_snapshot_fingerprint();
        let dialogue_definition = project.mv_dialogue_definition().clone();
        let sqlite = Arc::clone(&self.sqlite);
        let cpu = Arc::clone(&self.cpu);
        let records_per_job = self.config.leaves_per_decode_job().get();

        async move {
            let dialogue_definition_json =
                dialogue_definition.to_canonical_json().map_err(|source| {
                    RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot(
                        InvalidStandardWriteBackAssetSnapshot::InvalidDialogueDefinition(source),
                    )
                })?;
            let query_results = sqlite
                .query_existing_database_snapshot(
                    database_path.clone(),
                    vec![
                        SqliteQuery::new(READ_STANDARD_WRITE_BACK_OWNER_STATES, Vec::new()),
                        SqliteQuery::new(READ_STANDARD_WRITE_BACK_GROUPS, Vec::new()),
                        SqliteQuery::new(READ_STANDARD_WRITE_BACK_LEAVES, Vec::new()),
                        SqliteQuery::new(READ_STANDARD_WRITE_BACK_TARGETS, Vec::new()),
                    ],
                )
                .await
                .map_err(|error| map_query_error(database_path, error))?;
            let actual = query_results.len();
            let [owner_rows, group_rows, leaf_rows, target_rows] =
                query_results.try_into().map_err(|_| {
                    RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot(
                        InvalidStandardWriteBackAssetSnapshot::WrongQueryResultCount {
                            expected: 4,
                            actual,
                        },
                    )
                })?;

            let prepared = cpu
                .execute(move || {
                    prepare_rows(
                        SnapshotRows {
                            owners: owner_rows,
                            groups: group_rows,
                            leaves: leaf_rows,
                            targets: target_rows,
                        },
                        current_source,
                        records_per_job,
                    )
                })
                .await
                .map_err(RpgMakerStandardWriteBackAssetReadingError::SchedulePartition)?
                .map_err(RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot)?;
            if !prepared.stale_owners.is_empty() {
                return Err(
                    RpgMakerStandardWriteBackAssetReadingError::ExtractionOutOfDate {
                        owners: prepared.stale_owners,
                    },
                );
            }

            let decoded_batches = cpu
                .execute_ordered_map(prepared.batches, decode_batch)
                .await
                .map_err(RpgMakerStandardWriteBackAssetReadingError::ScheduleDecode)?;

            let owner_states = prepared.owner_states;
            cpu.execute(move || {
                let decoded = decoded_batches.into_iter().collect::<Result<Vec<_>, _>>()?;
                assemble_snapshot(
                    owner_states,
                    decoded.into_iter().flatten(),
                    &dialogue_definition_json,
                )
            })
            .await
            .map_err(RpgMakerStandardWriteBackAssetReadingError::ScheduleAssembly)?
            .map_err(RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot)
        }
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerStandardWriteBackAssetReadingError<Q, C> {
    DatabaseNotFound {
        database_path: PathBuf,
    },
    Query {
        database_path: PathBuf,
        source: Q,
    },
    ExtractionOutOfDate {
        owners: Vec<RpgMakerStandardAssetOwner>,
    },
    SchedulePartition(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardWriteBackAssetSnapshot),
}

impl<Q: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerStandardWriteBackAssetReadingError<Q, C>
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

impl<Q: Error + 'static, C: Error + 'static> Error
    for RpgMakerStandardWriteBackAssetReadingError<Q, C>
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
pub(crate) enum InvalidStandardWriteBackAssetSnapshot {
    WrongQueryResultCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    UnknownOwner(String),
    DuplicateOwner(String),
    InvalidFingerprintLength {
        owner: String,
        column: &'static str,
        actual: usize,
    },
    AssetWithoutOwner(String),
    UnknownGroupKind(String),
    DuplicateGroup {
        owner: String,
        group_location: String,
    },
    MissingGroup {
        owner: String,
        group_location: String,
    },
    AssetFingerprintMismatch {
        owner: String,
    },
    InvalidDialogueDefinition(MvDialogueDefinitionError),
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidProjection(RpgMakerProjectionCodecError),
    InvalidModel(StandardWriteBackSnapshotError),
}

impl fmt::Display for InvalidStandardWriteBackAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongQueryResultCount { expected, actual } => write!(
                formatter,
                "写回资产查询应返回 {expected} 组结果，实际为 {actual} 组"
            ),
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
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::DuplicateOwner(owner) => write!(formatter, "资产所有者状态重复：{owner}"),
            Self::InvalidFingerprintLength {
                owner,
                column,
                actual,
            } => write!(
                formatter,
                "资产所有者 {owner} 的 {column} 应为 32 字节，实际为 {actual} 字节"
            ),
            Self::AssetWithoutOwner(owner) => {
                write!(formatter, "资产没有 active owner state：{owner}")
            }
            Self::UnknownGroupKind(kind) => write!(formatter, "未知文本组类型：{kind}"),
            Self::DuplicateGroup {
                owner,
                group_location,
            } => write!(formatter, "资产组重复：{owner} / {group_location}"),
            Self::MissingGroup {
                owner,
                group_location,
            } => write!(
                formatter,
                "叶或目标没有对应资产组：{owner} / {group_location}"
            ),
            Self::AssetFingerprintMismatch { owner } => {
                write!(formatter, "资产所有者 {owner} 的快照指纹与三表内容不一致")
            }
            Self::InvalidDialogueDefinition(source) => {
                write!(formatter, "项目中的 MV 对话定义无法编码：{source}")
            }
            Self::InvalidLocation(source) => write!(formatter, "组位置无效：{source}"),
            Self::InvalidProjection(source) => write!(formatter, "文本投影无效：{source}"),
            Self::InvalidModel(source) => source.fmt(formatter),
        }
    }
}

impl Error for InvalidStandardWriteBackAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidProjection(source) => Some(source),
            Self::InvalidModel(source) => Some(source),
            Self::InvalidDialogueDefinition(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct OwnerState {
    owner: RpgMakerStandardAssetOwner,
    asset_fingerprint: AssetSnapshotFingerprint,
}

struct SnapshotRows {
    owners: Vec<SqliteRow>,
    groups: Vec<SqliteRow>,
    leaves: Vec<SqliteRow>,
    targets: Vec<SqliteRow>,
}

enum SnapshotAssetRow {
    Group(SqliteRow),
    Leaf(SqliteRow),
    Target(SqliteRow),
}

struct PreparedRows {
    stale_owners: Vec<RpgMakerStandardAssetOwner>,
    owner_states: BTreeMap<String, OwnerState>,
    batches: Vec<Vec<SnapshotAssetRow>>,
}

fn prepare_rows(
    rows: SnapshotRows,
    current_source: SourceSnapshotFingerprint,
    records_per_job: usize,
) -> Result<PreparedRows, InvalidStandardWriteBackAssetSnapshot> {
    assert!(records_per_job > 0, "写回资产解码批大小必须非零");
    let asset_row_count = rows
        .groups
        .len()
        .saturating_add(rows.leaves.len())
        .saturating_add(rows.targets.len());
    let initial_batch_capacity = records_per_job.min(asset_row_count);
    let mut owner_states = BTreeMap::new();
    let mut batches = Vec::new();
    let mut asset_rows = Vec::with_capacity(initial_batch_capacity);
    let mut stale_owners = Vec::new();
    for row in rows.owners {
        let values = row.into_values();
        let actual = values.len();
        let [
            owner,
            source_snapshot_fingerprint,
            asset_snapshot_fingerprint,
        ] = values.try_into().map_err(|_| {
            InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 3,
                actual,
            }
        })?;
        let owner_name = owned_text(owner, "owner")?;
        let owner = parse_owner(&owner_name)?;
        let source = owned_fingerprint(
            source_snapshot_fingerprint,
            &owner_name,
            "source_snapshot_fingerprint",
        )?;
        let asset = owned_fingerprint(
            asset_snapshot_fingerprint,
            &owner_name,
            "asset_snapshot_fingerprint",
        )?;
        if owner_states
            .insert(
                owner_name.clone(),
                OwnerState {
                    owner,
                    asset_fingerprint: AssetSnapshotFingerprint::from_bytes(asset),
                },
            )
            .is_some()
        {
            return Err(InvalidStandardWriteBackAssetSnapshot::DuplicateOwner(
                owner_name,
            ));
        }
        if SourceSnapshotFingerprint::from_bytes(source) != current_source {
            stale_owners.push(owner);
        }
    }
    stale_owners.sort_by_key(owner_order);

    let ordered_asset_rows = rows
        .groups
        .into_iter()
        .map(SnapshotAssetRow::Group)
        .chain(rows.leaves.into_iter().map(SnapshotAssetRow::Leaf))
        .chain(rows.targets.into_iter().map(SnapshotAssetRow::Target));
    for row in ordered_asset_rows {
        asset_rows.push(row);
        if asset_rows.len() == records_per_job {
            batches.push(std::mem::replace(
                &mut asset_rows,
                Vec::with_capacity(records_per_job),
            ));
        }
    }
    if !asset_rows.is_empty() {
        batches.push(asset_rows);
    }
    Ok(PreparedRows {
        stale_owners,
        owner_states,
        batches,
    })
}

enum DecodedRecord {
    Group {
        owner: String,
        group_location_raw: String,
        group_location: RpgMakerLocation,
        kind: TextGroupKind,
        group_kind_raw: String,
        recipes: Vec<TextProjectionRecipe>,
        recipes_raw: String,
    },
    Leaf {
        owner: String,
        group_location_raw: String,
        role: TextFieldRole,
        role_raw: String,
        original_text: String,
        translation_context_json: String,
        translation: Option<String>,
    },
    Target {
        owner: String,
        group_location_raw: String,
        target: MutationTarget,
        target_raw: String,
    },
}

fn decode_batch(
    rows: Vec<SnapshotAssetRow>,
) -> Result<Vec<DecodedRecord>, InvalidStandardWriteBackAssetSnapshot> {
    rows.into_iter()
        .map(|row| match row {
            SnapshotAssetRow::Group(row) => decode_group(row),
            SnapshotAssetRow::Leaf(row) => decode_leaf(row),
            SnapshotAssetRow::Target(row) => decode_target(row),
        })
        .collect()
}

fn decode_group(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [owner, group_location, group_kind, projection_recipe_json] = values.try_into().map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 4,
            actual,
        },
    )?;
    let owner = owned_text(owner, "owner")?;
    parse_owner(&owner)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let group_kind_raw = owned_text(group_kind, "group_kind")?;
    let recipes_raw = owned_text(projection_recipe_json, "projection_recipe_json")?;
    Ok(DecodedRecord::Group {
        owner,
        group_location: RpgMakerLocationCodec::decode(&group_location_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidLocation)?,
        group_location_raw,
        kind: parse_group_kind(&group_kind_raw)?,
        group_kind_raw,
        recipes: RpgMakerProjectionCodec::decode_recipes(&recipes_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
        recipes_raw,
    })
}

fn decode_leaf(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [
        owner,
        group_location,
        field_role,
        original_text,
        translation_context_json,
        translation,
    ] = values.try_into().map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 6,
            actual,
        },
    )?;
    let owner = owned_text(owner, "owner")?;
    parse_owner(&owner)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let role_raw = owned_text(field_role, "field_role")?;
    Ok(DecodedRecord::Leaf {
        owner,
        group_location_raw,
        role: RpgMakerProjectionCodec::decode_role(&role_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
        role_raw,
        original_text: owned_text(original_text, "original_text")?,
        translation_context_json: owned_text(translation_context_json, "translation_context_json")?,
        translation: optional_owned_text(translation, "translation")?,
    })
}

fn decode_target(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [owner, group_location, mutation_target] =
        values.try_into().map_err(
            |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 3,
                actual,
            },
        )?;
    let owner = owned_text(owner, "owner")?;
    parse_owner(&owner)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let target_raw = owned_text(mutation_target, "mutation_target")?;
    Ok(DecodedRecord::Target {
        owner,
        group_location_raw,
        target: RpgMakerProjectionCodec::decode_target(&target_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
        target_raw,
    })
}

struct GroupBuilder {
    kind: TextGroupKind,
    location: RpgMakerLocation,
    recipes: Vec<TextProjectionRecipe>,
    leaves: Vec<StandardWriteBackLeaf>,
    targets: Vec<MutationTarget>,
}

struct SnapshotFingerprintAccumulator {
    hasher: Sha256FramedHasher,
}

impl SnapshotFingerprintAccumulator {
    fn new(owner: RpgMakerStandardAssetOwner, dialogue_definition_json: &str) -> Self {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
        hasher.frame(1, owner.storage_name().as_bytes());
        if owner == RpgMakerStandardAssetOwner::Builtin {
            hasher
                .frame(14, b"project_definition")
                .frame(15, dialogue_definition_json.as_bytes());
        }
        Self { hasher }
    }

    fn group(&mut self, group_location: &str, group_kind: &str, recipes: &str) {
        self.hasher
            .frame(2, b"group")
            .frame(3, group_location.as_bytes())
            .frame(4, group_kind.as_bytes())
            .frame(5, recipes.as_bytes());
    }

    fn leaf(&mut self, group_location: &str, role: &str, original: &str, context: &str) {
        self.hasher
            .frame(6, b"leaf")
            .frame(7, group_location.as_bytes())
            .frame(8, role.as_bytes())
            .frame(9, original.as_bytes())
            .frame(10, context.as_bytes());
    }

    fn target(&mut self, target: &str, group_location: &str) {
        self.hasher
            .frame(11, b"target")
            .frame(12, target.as_bytes())
            .frame(13, group_location.as_bytes());
    }

    fn finish(self) -> AssetSnapshotFingerprint {
        AssetSnapshotFingerprint::from_bytes(self.hasher.finish().into_bytes())
    }
}

fn assemble_snapshot(
    owner_states: BTreeMap<String, OwnerState>,
    records: impl IntoIterator<Item = DecodedRecord>,
    dialogue_definition_json: &str,
) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
    let mut groups = BTreeMap::<(String, String), GroupBuilder>::new();
    let mut leaves = Vec::new();
    let mut targets = Vec::new();
    let mut fingerprint_accumulators = owner_states
        .iter()
        .map(|(owner_name, state)| {
            (
                owner_name.clone(),
                SnapshotFingerprintAccumulator::new(state.owner, dialogue_definition_json),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for record in records {
        let owner = match &record {
            DecodedRecord::Group { owner, .. }
            | DecodedRecord::Leaf { owner, .. }
            | DecodedRecord::Target { owner, .. } => owner,
        };
        if !owner_states.contains_key(owner) {
            return Err(InvalidStandardWriteBackAssetSnapshot::AssetWithoutOwner(
                owner.clone(),
            ));
        }
        match record {
            DecodedRecord::Group {
                owner,
                group_location_raw,
                group_location,
                kind,
                group_kind_raw,
                recipes,
                recipes_raw,
            } => {
                fingerprint_accumulators
                    .get_mut(&owner)
                    .expect("active owner 已在循环入口确认")
                    .group(&group_location_raw, &group_kind_raw, &recipes_raw);
                let key = (owner.clone(), group_location_raw.clone());
                if groups
                    .insert(
                        key,
                        GroupBuilder {
                            kind,
                            location: group_location,
                            recipes,
                            leaves: Vec::new(),
                            targets: Vec::new(),
                        },
                    )
                    .is_some()
                {
                    return Err(InvalidStandardWriteBackAssetSnapshot::DuplicateGroup {
                        owner,
                        group_location: group_location_raw,
                    });
                }
            }
            DecodedRecord::Leaf {
                owner,
                group_location_raw,
                role,
                role_raw,
                original_text,
                translation_context_json,
                translation,
            } => {
                fingerprint_accumulators
                    .get_mut(&owner)
                    .expect("active owner 已在循环入口确认")
                    .leaf(
                        &group_location_raw,
                        &role_raw,
                        &original_text,
                        &translation_context_json,
                    );
                leaves.push((
                    owner,
                    group_location_raw,
                    StandardWriteBackLeaf::new(role, original_text, translation)
                        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?,
                ));
            }
            DecodedRecord::Target {
                owner,
                group_location_raw,
                target,
                target_raw,
            } => {
                fingerprint_accumulators
                    .get_mut(&owner)
                    .expect("active owner 已在循环入口确认")
                    .target(&target_raw, &group_location_raw);
                targets.push((owner, group_location_raw, target));
            }
        }
    }

    for (owner, group_location, leaf) in leaves {
        groups
            .get_mut(&(owner.clone(), group_location.clone()))
            .ok_or(InvalidStandardWriteBackAssetSnapshot::MissingGroup {
                owner,
                group_location,
            })?
            .leaves
            .push(leaf);
    }
    for (owner, group_location, target) in targets {
        groups
            .get_mut(&(owner.clone(), group_location.clone()))
            .ok_or(InvalidStandardWriteBackAssetSnapshot::MissingGroup {
                owner,
                group_location,
            })?
            .targets
            .push(target);
    }

    for (owner_name, state) in &owner_states {
        let actual = fingerprint_accumulators
            .remove(owner_name)
            .expect("每个 active owner 都应建立指纹累加器")
            .finish();
        if actual != state.asset_fingerprint {
            return Err(
                InvalidStandardWriteBackAssetSnapshot::AssetFingerprintMismatch {
                    owner: owner_name.clone(),
                },
            );
        }
    }

    let groups = groups
        .into_values()
        .map(|group| {
            StandardWriteBackGroup::new(
                group.kind,
                group.location,
                group.leaves,
                group.recipes,
                group.targets,
            )
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
        })
        .collect::<Result<Vec<_>, _>>()?;
    StandardWriteBackSnapshot::new(groups)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
}

#[cfg(test)]
#[derive(Default)]
struct FingerprintRows {
    groups: Vec<(String, String, String)>,
    leaves: Vec<(String, String, String, String)>,
    targets: Vec<(String, String)>,
}

#[cfg(test)]
fn snapshot_fingerprint(
    owner: RpgMakerStandardAssetOwner,
    mut rows: FingerprintRows,
    dialogue_definition_json: &str,
) -> AssetSnapshotFingerprint {
    rows.groups.sort();
    rows.leaves.sort();
    rows.targets.sort();
    let mut accumulator = SnapshotFingerprintAccumulator::new(owner, dialogue_definition_json);
    for (group_location, group_kind, recipes) in rows.groups {
        accumulator.group(&group_location, &group_kind, &recipes);
    }
    for (group_location, role, original, context) in rows.leaves {
        accumulator.leaf(&group_location, &role, &original, &context);
    }
    for (target, group_location) in rows.targets {
        accumulator.target(&target, &group_location);
    }
    accumulator.finish()
}

fn parse_owner(
    value: &str,
) -> Result<RpgMakerStandardAssetOwner, InvalidStandardWriteBackAssetSnapshot> {
    RpgMakerStandardAssetOwner::from_storage_name(value)
        .ok_or_else(|| InvalidStandardWriteBackAssetSnapshot::UnknownOwner(value.to_owned()))
}

fn parse_group_kind(value: &str) -> Result<TextGroupKind, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        "database_entry" => Ok(TextGroupKind::DatabaseEntry),
        "system" => Ok(TextGroupKind::System),
        "map" => Ok(TextGroupKind::Map),
        "event_dialogue" => Ok(TextGroupKind::EventDialogue),
        "event_choices" => Ok(TextGroupKind::EventChoices),
        "event_scrolling_text" => Ok(TextGroupKind::EventScrollingText),
        "event_command" => Ok(TextGroupKind::EventCommand),
        "plugin_parameter" => Ok(TextGroupKind::PluginParameter),
        other => Err(InvalidStandardWriteBackAssetSnapshot::UnknownGroupKind(
            other.to_owned(),
        )),
    }
}

fn owned_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: value.kind_name(),
        }),
    }
}

fn optional_owned_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        value => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: value.kind_name(),
        }),
    }
}

fn owned_fingerprint(
    value: SqliteValue,
    owner: &str,
    column: &'static str,
) -> Result<[u8; 32], InvalidStandardWriteBackAssetSnapshot> {
    let SqliteValue::Blob(bytes) = value else {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "BLOB",
            actual: value.kind_name(),
        });
    };
    let actual = bytes.len();
    bytes.try_into().map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::InvalidFingerprintLength {
            owner: owner.to_owned(),
            column,
            actual,
        },
    )
}

fn owner_order(owner: &RpgMakerStandardAssetOwner) -> u8 {
    match owner {
        RpgMakerStandardAssetOwner::Builtin => 0,
        RpgMakerStandardAssetOwner::Rules => 1,
        RpgMakerStandardAssetOwner::Lua => 2,
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> RpgMakerStandardWriteBackAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerStandardWriteBackAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerStandardWriteBackAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::model::ScalarFieldKey;

    fn owner_row(source: [u8; 32], asset: [u8; 32]) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Blob(source.to_vec()),
            SqliteValue::Blob(asset.to_vec()),
        ])
    }

    fn snapshot_rows(owners: Vec<SqliteRow>) -> SnapshotRows {
        SnapshotRows {
            owners,
            groups: Vec::new(),
            leaves: Vec::new(),
            targets: Vec::new(),
        }
    }

    #[test]
    fn snapshot_queries_follow_primary_key_order_without_temporary_sorting() {
        let connection = rusqlite::Connection::open_in_memory().expect("应可建立内存数据库");
        connection
            .execute_batch(
                r#"
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE standard_text_group (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location)
                );
                CREATE TABLE standard_text_leaf (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_role TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation_context_json TEXT NOT NULL,
                    translation TEXT,
                    PRIMARY KEY (owner, group_location, field_role)
                );
                CREATE TABLE standard_text_target (
                    mutation_target TEXT NOT NULL PRIMARY KEY,
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL
                );

                INSERT INTO standard_asset_owner_state VALUES ('rules', zeroblob(32), zeroblob(32));
                INSERT INTO standard_asset_owner_state VALUES ('builtin', zeroblob(32), zeroblob(32));
                INSERT INTO standard_text_group VALUES ('builtin', 'group-b', 'map', '[]');
                INSERT INTO standard_text_group VALUES ('builtin', 'group-a', 'map', '[]');
                INSERT INTO standard_text_leaf VALUES ('builtin', 'group-b', 'role-z', 'z', '{}', NULL);
                INSERT INTO standard_text_leaf VALUES ('builtin', 'group-a', 'role-y', 'y', '{}', NULL);
                INSERT INTO standard_text_target VALUES ('target-z', 'builtin', 'group-a');
                INSERT INTO standard_text_target VALUES ('target-a', 'builtin', 'group-z');
                "#,
            )
            .expect("测试快照表与行应可建立");

        for query in [
            READ_STANDARD_WRITE_BACK_OWNER_STATES,
            READ_STANDARD_WRITE_BACK_GROUPS,
            READ_STANDARD_WRITE_BACK_LEAVES,
            READ_STANDARD_WRITE_BACK_TARGETS,
        ] {
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .expect("快照查询计划应可建立")
                .query_map([], |row| row.get::<_, String>(3))
                .expect("快照查询计划应可读取")
                .collect::<Result<Vec<_>, _>>()
                .expect("快照查询计划行应可读取");
            assert!(
                plan.iter().all(|detail| !detail.contains("TEMP B-TREE")),
                "快照查询不应产生临时排序：{plan:?}"
            );
        }

        let owners = connection
            .prepare(READ_STANDARD_WRITE_BACK_OWNER_STATES)
            .expect("owner 查询应可建立")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("owner 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner 行应可读取");
        let groups = connection
            .prepare(READ_STANDARD_WRITE_BACK_GROUPS)
            .expect("group 查询应可建立")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("group 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("group 行应可读取");
        let leaves = connection
            .prepare(READ_STANDARD_WRITE_BACK_LEAVES)
            .expect("leaf 查询应可建立")
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .expect("leaf 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("leaf 行应可读取");
        let targets = connection
            .prepare(READ_STANDARD_WRITE_BACK_TARGETS)
            .expect("target 查询应可建立")
            .query_map([], |row| {
                Ok((row.get::<_, String>(2)?, row.get::<_, String>(1)?))
            })
            .expect("target 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("target 行应可读取");

        assert_eq!(owners, ["builtin", "rules"]);
        assert_eq!(groups, ["group-a", "group-b"]);
        assert_eq!(
            leaves,
            [
                ("group-a".to_owned(), "role-y".to_owned()),
                ("group-b".to_owned(), "role-z".to_owned()),
            ]
        );
        assert_eq!(
            targets,
            [
                ("target-a".to_owned(), "group-z".to_owned()),
                ("target-z".to_owned(), "group-a".to_owned()),
            ]
        );
    }

    #[test]
    fn prepare_rows_moves_asset_rows_into_natural_order_batches() {
        let mut pointers = Vec::new();
        let mut make_row = |label: &str, columns: usize| {
            let owner = label.to_owned();
            pointers.push(owner.as_ptr());
            let mut values = Vec::with_capacity(columns);
            values.push(SqliteValue::Text(owner));
            values.resize(columns, SqliteValue::Null);
            SqliteRow::new(values)
        };
        let rows = SnapshotRows {
            owners: Vec::new(),
            groups: vec![make_row("group-0", 4), make_row("group-1", 4)],
            leaves: vec![make_row("leaf-0", 6), make_row("leaf-1", 6)],
            targets: vec![make_row("target-0", 3)],
        };

        let prepared = prepare_rows(rows, SourceSnapshotFingerprint::from_bytes([1; 32]), 2)
            .expect("非 owner 行只应按所有权分批");

        assert_eq!(
            prepared.batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert_eq!(
            prepared
                .batches
                .iter()
                .flatten()
                .map(|row| match row {
                    SnapshotAssetRow::Group(row)
                    | SnapshotAssetRow::Leaf(row)
                    | SnapshotAssetRow::Target(row) => match &row.values()[0] {
                        SqliteValue::Text(value) => value.as_ptr(),
                        value => panic!("owner 应为 TEXT，实际为 {}", value.kind_name()),
                    },
                })
                .collect::<Vec<_>>(),
            pointers
        );
    }

    #[test]
    fn decode_record_moves_owned_text_out_of_sqlite_values() {
        let owner = "builtin".to_owned();
        let owner_pointer = owner.as_ptr();
        let group_location = RpgMakerLocationCodec::encode(&RpgMakerLocation::value(
            crate::rpg_maker::text::RpgMakerSource::map(1),
            vec![crate::rpg_maker::text::RpgMakerLocationStep::key("name")],
        ))
        .expect("测试位置应可编码");
        let group_location_pointer = group_location.as_ptr();
        let role = RpgMakerProjectionCodec::encode_role(&TextFieldRole::Scalar(
            ScalarFieldKey::new("name").expect("测试字段键应合法"),
        ))
        .expect("测试角色应可编码");
        let role_pointer = role.as_ptr();
        let original = "原文".to_owned();
        let original_pointer = original.as_ptr();
        let context = "{}".to_owned();
        let context_pointer = context.as_ptr();
        let translation = "译文".to_owned();
        let translation_pointer = translation.as_ptr();
        let row = SqliteRow::new(vec![
            SqliteValue::Text(owner),
            SqliteValue::Text(group_location),
            SqliteValue::Text(role),
            SqliteValue::Text(original),
            SqliteValue::Text(context),
            SqliteValue::Text(translation),
        ]);

        let DecodedRecord::Leaf {
            owner,
            group_location_raw,
            role_raw,
            original_text,
            translation_context_json,
            translation: Some(translation),
            ..
        } = decode_leaf(row).expect("测试叶行应可解码")
        else {
            panic!("测试行应解码为 leaf")
        };

        assert_eq!(owner.as_ptr(), owner_pointer);
        assert_eq!(group_location_raw.as_ptr(), group_location_pointer);
        assert_eq!(role_raw.as_ptr(), role_pointer);
        assert_eq!(original_text.as_ptr(), original_pointer);
        assert_eq!(translation_context_json.as_ptr(), context_pointer);
        assert_eq!(translation.as_ptr(), translation_pointer);
    }

    #[test]
    fn stale_source_and_asset_fingerprint_corruption_are_distinct_failures() {
        const DIALOGUE_DEFINITION: &str = "{\"rules\":[]}";
        let stale = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], [2; 32])]),
            SourceSnapshotFingerprint::from_bytes([9; 32]),
            16,
        )
        .expect("owner 行应可解码");
        assert_eq!(stale.stale_owners, [RpgMakerStandardAssetOwner::Builtin]);

        let prepared = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], [2; 32])]),
            SourceSnapshotFingerprint::from_bytes([1; 32]),
            16,
        )
        .expect("owner 行应可解码");
        assert!(matches!(
            assemble_snapshot(
                prepared.owner_states,
                Vec::new(),
                DIALOGUE_DEFINITION,
            ),
            Err(InvalidStandardWriteBackAssetSnapshot::AssetFingerprintMismatch {
                owner
            }) if owner == "builtin"
        ));

        let valid_fingerprint = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            FingerprintRows::default(),
            DIALOGUE_DEFINITION,
        );
        let prepared = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], *valid_fingerprint.as_bytes())]),
            SourceSnapshotFingerprint::from_bytes([1; 32]),
            16,
        )
        .expect("owner 行应可解码");
        assemble_snapshot(prepared.owner_states, Vec::new(), DIALOGUE_DEFINITION)
            .expect("Builtin 指纹应包含活动 MV 对话定义");
    }

    #[test]
    fn damaged_projection_recipe_fails_at_the_database_boundary() {
        let location = RpgMakerLocation::value(
            crate::rpg_maker::text::RpgMakerSource::map(1),
            vec![crate::rpg_maker::text::RpgMakerLocationStep::key("list")],
        );
        let row = SqliteRow::new(vec![
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(RpgMakerLocationCodec::encode(&location).expect("位置应可编码")),
            SqliteValue::Text("event_dialogue".to_owned()),
            SqliteValue::Text("{not-json".to_owned()),
        ]);
        assert!(matches!(
            decode_group(row),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection(_))
        ));
    }
}
