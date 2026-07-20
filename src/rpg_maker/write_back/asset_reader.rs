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

const READ_STANDARD_WRITE_BACK_SNAPSHOT: &str = r#"SELECT
    'owner' AS record_kind,
    owner,
    NULL AS group_location,
    NULL AS group_kind,
    NULL AS projection_recipe_json,
    NULL AS field_role,
    NULL AS original_text,
    NULL AS translation_context_json,
    NULL AS translation,
    NULL AS mutation_target,
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint
FROM standard_asset_owner_state

UNION ALL

SELECT
    'group', owner, group_location, group_kind, projection_recipe_json,
    NULL, NULL, NULL, NULL, NULL, NULL, NULL
FROM standard_text_group

UNION ALL

SELECT
    'leaf', owner, group_location, NULL, NULL,
    field_role, original_text, translation_context_json, translation,
    NULL, NULL, NULL
FROM standard_text_leaf

UNION ALL

SELECT
    'target', owner, group_location, NULL, NULL,
    NULL, NULL, NULL, NULL, mutation_target, NULL, NULL
FROM standard_text_target

ORDER BY owner, record_kind, group_location, field_role, mutation_target"#;

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
            let rows = sqlite
                .query_existing_database(
                    database_path.clone(),
                    SqliteQuery::new(READ_STANDARD_WRITE_BACK_SNAPSHOT, Vec::new()),
                )
                .await
                .map_err(|error| map_query_error(database_path, error))?;

            let prepared = cpu
                .execute(move || prepare_rows(rows, current_source, records_per_job))
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
                let decoded = decoded_batches
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                assemble_snapshot(owner_states, decoded, &dialogue_definition_json)
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
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    UnknownRecordKind(String),
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
            Self::UnknownRecordKind(kind) => write!(formatter, "未知写回记录类型：{kind}"),
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

struct PreparedRows {
    stale_owners: Vec<RpgMakerStandardAssetOwner>,
    owner_states: BTreeMap<String, OwnerState>,
    batches: Vec<Vec<SqliteRow>>,
}

fn prepare_rows(
    rows: Vec<SqliteRow>,
    current_source: SourceSnapshotFingerprint,
    records_per_job: usize,
) -> Result<PreparedRows, InvalidStandardWriteBackAssetSnapshot> {
    let mut owner_states = BTreeMap::new();
    let mut asset_rows = Vec::new();
    let mut stale_owners = Vec::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 12 {
            return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 12,
                actual: values.len(),
            });
        }
        let kind = required_text(&values[0], "record_kind")?;
        if kind != "owner" {
            asset_rows.push(SqliteRow::new(values));
            continue;
        }
        for (column, value) in [
            ("group_location", &values[2]),
            ("group_kind", &values[3]),
            ("projection_recipe_json", &values[4]),
            ("field_role", &values[5]),
            ("original_text", &values[6]),
            ("translation_context_json", &values[7]),
            ("translation", &values[8]),
            ("mutation_target", &values[9]),
        ] {
            required_null(value, column)?;
        }
        let owner_name = required_text(&values[1], "owner")?;
        let owner = parse_owner(owner_name)?;
        let source = fingerprint(&values[10], owner_name, "source_snapshot_fingerprint")?;
        let asset = fingerprint(&values[11], owner_name, "asset_snapshot_fingerprint")?;
        if owner_states
            .insert(
                owner_name.to_owned(),
                OwnerState {
                    owner,
                    asset_fingerprint: AssetSnapshotFingerprint::from_bytes(asset),
                },
            )
            .is_some()
        {
            return Err(InvalidStandardWriteBackAssetSnapshot::DuplicateOwner(
                owner_name.to_owned(),
            ));
        }
        if SourceSnapshotFingerprint::from_bytes(source) != current_source {
            stale_owners.push(owner);
        }
    }
    stale_owners.sort_by_key(owner_order);
    let batches = asset_rows
        .chunks(records_per_job)
        .map(<[SqliteRow]>::to_vec)
        .collect();
    Ok(PreparedRows {
        stale_owners,
        owner_states,
        batches,
    })
}

#[derive(Clone)]
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
    rows: Vec<SqliteRow>,
) -> Result<Vec<DecodedRecord>, InvalidStandardWriteBackAssetSnapshot> {
    rows.into_iter().map(decode_record).collect()
}

fn decode_record(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 12 {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 12,
            actual: values.len(),
        });
    }
    required_null(&values[10], "source_snapshot_fingerprint")?;
    required_null(&values[11], "asset_snapshot_fingerprint")?;
    let kind = required_text(&values[0], "record_kind")?;
    let owner = required_text(&values[1], "owner")?.to_owned();
    parse_owner(&owner)?;
    let group_location_raw = required_text(&values[2], "group_location")?.to_owned();
    match kind {
        "group" => {
            let group_kind_raw = required_text(&values[3], "group_kind")?.to_owned();
            let recipes_raw = required_text(&values[4], "projection_recipe_json")?.to_owned();
            for (column, value) in [
                ("field_role", &values[5]),
                ("original_text", &values[6]),
                ("translation_context_json", &values[7]),
                ("translation", &values[8]),
                ("mutation_target", &values[9]),
            ] {
                required_null(value, column)?;
            }
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
        "leaf" => {
            required_null(&values[3], "group_kind")?;
            required_null(&values[4], "projection_recipe_json")?;
            required_null(&values[9], "mutation_target")?;
            let role_raw = required_text(&values[5], "field_role")?.to_owned();
            Ok(DecodedRecord::Leaf {
                owner,
                group_location_raw,
                role: RpgMakerProjectionCodec::decode_role(&role_raw)
                    .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
                role_raw,
                original_text: required_text(&values[6], "original_text")?.to_owned(),
                translation_context_json: required_text(&values[7], "translation_context_json")?
                    .to_owned(),
                translation: optional_text(&values[8], "translation")?,
            })
        }
        "target" => {
            for (column, value) in [
                ("group_kind", &values[3]),
                ("projection_recipe_json", &values[4]),
                ("field_role", &values[5]),
                ("original_text", &values[6]),
                ("translation_context_json", &values[7]),
                ("translation", &values[8]),
            ] {
                required_null(value, column)?;
            }
            let target_raw = required_text(&values[9], "mutation_target")?.to_owned();
            Ok(DecodedRecord::Target {
                owner,
                group_location_raw,
                target: RpgMakerProjectionCodec::decode_target(&target_raw)
                    .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
                target_raw,
            })
        }
        other => Err(InvalidStandardWriteBackAssetSnapshot::UnknownRecordKind(
            other.to_owned(),
        )),
    }
}

struct GroupBuilder {
    kind: TextGroupKind,
    location: RpgMakerLocation,
    recipes: Vec<TextProjectionRecipe>,
    leaves: Vec<StandardWriteBackLeaf>,
    targets: Vec<MutationTarget>,
}

#[derive(Default)]
struct FingerprintRows {
    groups: Vec<(String, String, String)>,
    leaves: Vec<(String, String, String, String)>,
    targets: Vec<(String, String)>,
}

fn assemble_snapshot(
    owner_states: BTreeMap<String, OwnerState>,
    records: Vec<DecodedRecord>,
    dialogue_definition_json: &str,
) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
    let mut groups = BTreeMap::<(String, String), GroupBuilder>::new();
    let mut leaves = Vec::new();
    let mut targets = Vec::new();
    let mut fingerprint_rows = BTreeMap::<String, FingerprintRows>::new();

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
                fingerprint_rows.entry(owner).or_default().groups.push((
                    group_location_raw,
                    group_kind_raw,
                    recipes_raw,
                ));
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
                fingerprint_rows
                    .entry(owner.clone())
                    .or_default()
                    .leaves
                    .push((
                        group_location_raw.clone(),
                        role_raw,
                        original_text.clone(),
                        translation_context_json,
                    ));
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
                fingerprint_rows
                    .entry(owner.clone())
                    .or_default()
                    .targets
                    .push((target_raw, group_location_raw.clone()));
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
        let rows = fingerprint_rows.remove(owner_name).unwrap_or_default();
        if snapshot_fingerprint(state.owner, rows, dialogue_definition_json)
            != state.asset_fingerprint
        {
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

fn snapshot_fingerprint(
    owner: RpgMakerStandardAssetOwner,
    mut rows: FingerprintRows,
    dialogue_definition_json: &str,
) -> AssetSnapshotFingerprint {
    rows.groups.sort();
    rows.leaves.sort();
    rows.targets.sort();
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
    hasher.frame(1, owner.storage_name().as_bytes());
    if owner == RpgMakerStandardAssetOwner::Builtin {
        hasher
            .frame(14, b"project_definition")
            .frame(15, dialogue_definition_json.as_bytes());
    }
    for (group_location, group_kind, recipes) in rows.groups {
        hasher
            .frame(2, b"group")
            .frame(3, group_location.as_bytes())
            .frame(4, group_kind.as_bytes())
            .frame(5, recipes.as_bytes());
    }
    for (group_location, role, original, context) in rows.leaves {
        hasher
            .frame(6, b"leaf")
            .frame(7, group_location.as_bytes())
            .frame(8, role.as_bytes())
            .frame(9, original.as_bytes())
            .frame(10, context.as_bytes());
    }
    for (target, group_location) in rows.targets {
        hasher
            .frame(11, b"target")
            .frame(12, target.as_bytes())
            .frame(13, group_location.as_bytes());
    }
    AssetSnapshotFingerprint::from_bytes(hasher.finish().into_bytes())
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

fn required_text<'a>(
    value: &'a SqliteValue,
    column: &'static str,
) -> Result<&'a str, InvalidStandardWriteBackAssetSnapshot> {
    let SqliteValue::Text(value) = value else {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: value.kind_name(),
        });
    };
    Ok(value)
}

fn optional_text(
    value: &SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value.clone())),
        value => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: value.kind_name(),
        }),
    }
}

fn required_null(
    value: &SqliteValue,
    column: &'static str,
) -> Result<(), InvalidStandardWriteBackAssetSnapshot> {
    if matches!(value, SqliteValue::Null) {
        Ok(())
    } else {
        Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "NULL",
            actual: value.kind_name(),
        })
    }
}

fn fingerprint(
    value: &SqliteValue,
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
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        InvalidStandardWriteBackAssetSnapshot::InvalidFingerprintLength {
            owner: owner.to_owned(),
            column,
            actual: bytes.len(),
        }
    })
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

    fn owner_row(source: [u8; 32], asset: [u8; 32]) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text("owner".to_owned()),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Blob(source.to_vec()),
            SqliteValue::Blob(asset.to_vec()),
        ])
    }

    #[test]
    fn stale_source_and_asset_fingerprint_corruption_are_distinct_failures() {
        const DIALOGUE_DEFINITION: &str = "{\"rules\":[]}";
        let stale = prepare_rows(
            vec![owner_row([1; 32], [2; 32])],
            SourceSnapshotFingerprint::from_bytes([9; 32]),
            16,
        )
        .expect("owner 行应可解码");
        assert_eq!(stale.stale_owners, [RpgMakerStandardAssetOwner::Builtin]);

        let prepared = prepare_rows(
            vec![owner_row([1; 32], [2; 32])],
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
            vec![owner_row([1; 32], *valid_fingerprint.as_bytes())],
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
            SqliteValue::Text("group".to_owned()),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(RpgMakerLocationCodec::encode(&location).expect("位置应可编码")),
            SqliteValue::Text("event_dialogue".to_owned()),
            SqliteValue::Text("{not-json".to_owned()),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
        ]);
        assert!(matches!(
            decode_record(row),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection(_))
        ));
    }
}
