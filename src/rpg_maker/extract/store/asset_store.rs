//! Builtin、Rules 与 Lua 标准文本资产的 SQLite 快照替换实现。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use serde::Serialize;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256FramedHasher;
use crate::rpg_maker::dialogue::{MvDialogueDefinition, MvDialogueDefinitionError};
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{MutationResourceAccess, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    AssetSnapshotFingerprint, MV_DIALOGUE_RULES_DEFINITION_KIND,
};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::TextGroupKind;
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteBatch, SqliteCommand, SqliteQuery,
    SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};

use super::super::model::{BuiltinSnapshot, ExtractedTextGroup, LuaSnapshot, RulesSnapshot};
use super::{
    BuiltinProjectDefinitionUpdate, BuiltinSnapshotStore, LuaSnapshotStore, RulesSnapshotStore,
};

const DROP_STAGING_GROUP: &str = "DROP TABLE IF EXISTS temp.rpg_maker_staging_group";
const DROP_STAGING_UNIT: &str = "DROP TABLE IF EXISTS temp.rpg_maker_staging_unit";
const DROP_STAGING_CLAIM: &str = "DROP TABLE IF EXISTS temp.rpg_maker_staging_claim";
const DROP_PREVIOUS_UNIT: &str = "DROP TABLE IF EXISTS temp.rpg_maker_previous_unit";

const CREATE_STAGING_GROUP: &str = r#"CREATE TEMP TABLE rpg_maker_staging_group (
    owner                  TEXT NOT NULL,
    group_location         TEXT NOT NULL,
    group_order            INTEGER NOT NULL,
    group_kind             TEXT NOT NULL,
    projection_recipe_json TEXT NOT NULL,
    PRIMARY KEY (owner, group_location)
)"#;

const CREATE_STAGING_UNIT: &str = r#"CREATE TEMP TABLE rpg_maker_staging_unit (
    owner                    TEXT NOT NULL,
    group_location           TEXT NOT NULL,
    unit_role                TEXT NOT NULL,
    unit_order               INTEGER NOT NULL,
    source_content_json      TEXT NOT NULL,
    source_context_json      TEXT NOT NULL,
    translation_content_json TEXT,
    translation_state        BLOB,
    PRIMARY KEY (owner, group_location, unit_role)
)"#;

const CREATE_STAGING_CLAIM: &str = r#"CREATE TEMP TABLE rpg_maker_staging_claim (
    owner          TEXT NOT NULL,
    group_location TEXT NOT NULL,
    resource_key   TEXT NOT NULL,
    access         TEXT NOT NULL,
    PRIMARY KEY (owner, group_location, resource_key)
)"#;

const CREATE_PREVIOUS_UNIT: &str = r#"CREATE TEMP TABLE rpg_maker_previous_unit AS
SELECT group_location,
       unit_role,
       source_content_json,
       source_context_json,
       translation_content_json,
       translation_state
FROM standard_text_unit
WHERE owner = ?"#;

const INSERT_STAGING_GROUP: &str = r#"INSERT INTO rpg_maker_staging_group (
    owner, group_location, group_order, group_kind, projection_recipe_json
) VALUES (?, ?, ?, ?, ?)"#;

const INSERT_STAGING_UNIT: &str = r#"INSERT INTO rpg_maker_staging_unit (
    owner,
    group_location,
    unit_role,
    unit_order,
    source_content_json,
    source_context_json
) VALUES (?, ?, ?, ?, ?, ?)"#;

const INSERT_STAGING_CLAIM: &str = r#"INSERT INTO rpg_maker_staging_claim (
    owner, group_location, resource_key, access
) VALUES (?, ?, ?, ?)"#;

const FIND_MUTATION_CLAIM_CONFLICT: &str = r#"SELECT staged.resource_key
FROM rpg_maker_staging_claim AS staged
JOIN standard_mutation_claim AS current
  ON current.resource_key = staged.resource_key
WHERE current.owner <> staged.owner
  AND (current.access = 'exclusive' OR staged.access = 'exclusive')
LIMIT 1"#;

const INHERIT_TRANSLATIONS: &str = r#"UPDATE rpg_maker_staging_unit
SET (translation_content_json, translation_state) = (
    SELECT previous.translation_content_json, previous.translation_state
    FROM rpg_maker_previous_unit AS previous
    WHERE previous.group_location = rpg_maker_staging_unit.group_location
      AND previous.unit_role = rpg_maker_staging_unit.unit_role
      AND previous.source_content_json = rpg_maker_staging_unit.source_content_json
      AND previous.source_context_json = rpg_maker_staging_unit.source_context_json
    LIMIT 1
)"#;

const DELETE_OWNER_CLAIMS: &str = "DELETE FROM standard_mutation_claim WHERE owner = ?";
const DELETE_OWNER_UNITS: &str = "DELETE FROM standard_text_unit WHERE owner = ?";
const DELETE_OWNER_GROUPS: &str = "DELETE FROM standard_text_group WHERE owner = ?";

const UPSERT_OWNER_STATE: &str = r#"INSERT INTO standard_asset_owner_state (
    owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
) VALUES (?, ?, ?)
ON CONFLICT(owner) DO UPDATE SET
    source_snapshot_fingerprint = excluded.source_snapshot_fingerprint,
    asset_snapshot_fingerprint = excluded.asset_snapshot_fingerprint"#;

const INSERT_GROUPS: &str = r#"INSERT INTO standard_text_group (
    owner, group_location, group_order, group_kind, projection_recipe_json
)
SELECT owner, group_location, group_order, group_kind, projection_recipe_json
FROM rpg_maker_staging_group
ORDER BY group_order"#;

const INSERT_UNITS: &str = r#"INSERT INTO standard_text_unit (
    owner,
    group_location,
    unit_role,
    unit_order,
    source_content_json,
    source_context_json,
    translation_content_json,
    translation_state
)
SELECT unit.owner,
       unit.group_location,
       unit.unit_role,
       unit.unit_order,
       unit.source_content_json,
       unit.source_context_json,
       unit.translation_content_json,
       unit.translation_state
FROM rpg_maker_staging_unit AS unit
JOIN rpg_maker_staging_group AS text_group
  ON text_group.owner = unit.owner
 AND text_group.group_location = unit.group_location
ORDER BY text_group.group_order, unit.unit_order"#;

const INSERT_CLAIMS: &str = r#"INSERT INTO standard_mutation_claim (
    owner, group_location, resource_key, access
)
SELECT owner, group_location, resource_key, access
FROM rpg_maker_staging_claim
ORDER BY resource_key, access, owner, group_location"#;

const DEACTIVATE_OWNER: &str = "DELETE FROM standard_asset_owner_state WHERE owner = ?";

const READ_PROJECT_DEFINITION: &str = r#"SELECT canonical_json
FROM standard_project_definition
WHERE definition_kind = ?"#;

const UPDATE_PROJECT_DEFINITION: &str = r#"UPDATE standard_project_definition
SET canonical_json = ?
WHERE definition_kind = ?"#;

const READ_OWNER_STATE: &str = r#"SELECT
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint
FROM standard_asset_owner_state
WHERE owner = ?"#;

const READ_OWNER_GROUPS: &str = r#"SELECT
    group_location,
    group_order,
    group_kind,
    projection_recipe_json
FROM standard_text_group
WHERE owner = ?
ORDER BY group_order"#;

const READ_OWNER_UNITS: &str = r#"SELECT
    unit.group_location,
    unit.unit_role,
    unit.unit_order,
    unit.source_content_json,
    unit.source_context_json
FROM standard_text_group AS text_group
CROSS JOIN standard_text_unit AS unit
  ON unit.owner = text_group.owner
 AND unit.group_location = text_group.group_location
WHERE text_group.owner = ?
ORDER BY text_group.group_order, unit.unit_order"#;

const READ_OWNER_CLAIMS: &str = r#"SELECT
    resource_key,
    access,
    group_location
FROM standard_mutation_claim INDEXED BY standard_mutation_claim_owner_resource_idx
WHERE owner = ?
ORDER BY owner, resource_key, access, group_location"#;

/// 标准提取资产编码阶段的必填资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerExtractionAssetStoreConfig {
    groups_per_encode_job: NonZeroUsize,
}

impl RpgMakerExtractionAssetStoreConfig {
    pub(crate) const fn new(groups_per_encode_job: NonZeroUsize) -> Self {
        Self {
            groups_per_encode_job,
        }
    }

    #[cfg(test)]
    pub(crate) const fn groups_per_encode_job(self) -> NonZeroUsize {
        self.groups_per_encode_job
    }
}

/// 使用纯 CPU 编码与单个 SQLite 事务替换 RPG Maker 标准资产。
pub(crate) struct RpgMakerExtractionAssetStore<S, C> {
    sqlite: S,
    cpu: C,
    config: RpgMakerExtractionAssetStoreConfig,
}

impl<S, C> RpgMakerExtractionAssetStore<S, C> {
    pub(crate) fn new(sqlite: S, cpu: C, config: RpgMakerExtractionAssetStoreConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<S, C> RpgMakerExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    async fn replace(
        &self,
        project: &OpenedProject,
        owner: RpgMakerStandardAssetOwner,
        groups: Vec<ExtractedTextGroup>,
        project_definition_update: Option<BuiltinProjectDefinitionUpdate>,
    ) -> Result<(), RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>>
    {
        let groups_per_encode_job = self.config.groups_per_encode_job.get();
        let ordered_groups = groups.into_iter().enumerate().collect::<Vec<_>>();
        let batches = self
            .cpu
            .execute(move || split_groups(ordered_groups, groups_per_encode_job))
            .await
            .map_err(RpgMakerExtractionAssetStoreError::ScheduleEncoding)?;
        let encoded_batches = self
            .cpu
            .execute_ordered_map(batches, encode_batch)
            .await
            .map_err(RpgMakerExtractionAssetStoreError::ScheduleEncoding)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(RpgMakerExtractionAssetStoreError::EncodeSnapshot)?;

        let database_path = project.database_path().to_path_buf();
        let current_owner_state = self.read_owner_state(database_path.clone(), owner).await?;
        let project_definition = match project_definition_update {
            None => None,
            Some(update) => {
                let current = self.read_project_definition(database_path.clone()).await?;
                let replacement = match update {
                    BuiltinProjectDefinitionUpdate::Reuse => None,
                    BuiltinProjectDefinitionUpdate::Replace(definition) => Some(
                        definition
                            .to_canonical_json()
                            .map_err(RpgMakerExtractionAssetStoreError::EncodeProjectDefinition)?,
                    ),
                };
                Some(ResolvedProjectDefinition {
                    current_canonical_json: current,
                    replacement,
                })
            }
        };
        let source_snapshot_fingerprint = *project.source_snapshot_fingerprint().as_bytes();
        let (encoded, project_definition) = self
            .cpu
            .execute(move || {
                let encoded = EncodedSnapshot::merge(
                    owner,
                    encoded_batches,
                    project_definition
                        .as_ref()
                        .map(ResolvedProjectDefinition::canonical_json),
                )?;
                Ok::<_, EncodeAssetSnapshotError>((encoded, project_definition))
            })
            .await
            .map_err(RpgMakerExtractionAssetStoreError::ScheduleEncoding)?
            .map_err(RpgMakerExtractionAssetStoreError::EncodeSnapshot)?;

        let encoded = if owner_state_matches(
            current_owner_state,
            &source_snapshot_fingerprint,
            encoded.fingerprint.as_bytes(),
        ) {
            let current = self
                .read_stored_snapshot_rows(database_path.clone(), owner)
                .await?;
            let definition_is_current = project_definition
                .as_ref()
                .is_none_or(ResolvedProjectDefinition::is_current);
            let (encoded_snapshot, snapshot_is_current) = self
                .cpu
                .execute(move || {
                    let snapshot_is_current = encoded
                        .matches_rows(current, &source_snapshot_fingerprint)
                        && definition_is_current;
                    (encoded, snapshot_is_current)
                })
                .await
                .map_err(RpgMakerExtractionAssetStoreError::ScheduleEncoding)?;
            if snapshot_is_current {
                return Ok(());
            }
            encoded_snapshot
        } else {
            encoded
        };
        let replacement = project_definition.and_then(|definition| definition.replacement);
        let plan = self
            .cpu
            .execute(move || {
                build_transaction_plan(owner, source_snapshot_fingerprint, encoded, replacement)
            })
            .await
            .map_err(RpgMakerExtractionAssetStoreError::ScheduleEncoding)?;
        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_persist_error(database_path, error))?;
        Ok(())
    }

    async fn deactivate(
        &self,
        project: &OpenedProject,
        owner: RpgMakerStandardAssetOwner,
    ) -> Result<(), RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>>
    {
        let database_path = project.database_path().to_path_buf();
        if self
            .read_owner_state(database_path.clone(), owner)
            .await?
            .is_empty()
        {
            return Ok(());
        }

        let plan = SqliteTransactionPlan::new(vec![execute(
            DEACTIVATE_OWNER,
            vec![text(owner.storage_name())],
        )]);
        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_persist_error(database_path, error))?;
        Ok(())
    }

    async fn read_owner_state(
        &self,
        database_path: PathBuf,
        owner: RpgMakerStandardAssetOwner,
    ) -> Result<
        Vec<SqliteRow>,
        RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>,
    > {
        self.sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_OWNER_STATE, vec![text(owner.storage_name())]),
            )
            .await
            .map_err(|error| map_query_error(database_path, error))
    }

    async fn read_stored_snapshot_rows(
        &self,
        database_path: PathBuf,
        owner: RpgMakerStandardAssetOwner,
    ) -> Result<
        StoredSnapshotRows,
        RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>,
    > {
        let query_results = self
            .sqlite
            .query_existing_database_snapshot(
                database_path.clone(),
                vec![
                    SqliteQuery::new(READ_OWNER_STATE, vec![text(owner.storage_name())]),
                    SqliteQuery::new(READ_OWNER_GROUPS, vec![text(owner.storage_name())]),
                    SqliteQuery::new(READ_OWNER_UNITS, vec![text(owner.storage_name())]),
                    SqliteQuery::new(READ_OWNER_CLAIMS, vec![text(owner.storage_name())]),
                ],
            )
            .await
            .map_err(|error| map_query_error(database_path, error))?;
        let actual = query_results.len();
        let [owner_state, groups, units, claims] = query_results.try_into().map_err(|_| {
            RpgMakerExtractionAssetStoreError::UnexpectedSnapshotQueryResultCount {
                expected: 4,
                actual,
            }
        })?;
        Ok(StoredSnapshotRows {
            owner_state,
            groups,
            units,
            claims,
        })
    }

    async fn read_project_definition(
        &self,
        database_path: PathBuf,
    ) -> Result<
        String,
        RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>,
    > {
        let rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(
                    READ_PROJECT_DEFINITION,
                    vec![text(MV_DIALOGUE_RULES_DEFINITION_KIND)],
                ),
            )
            .await
            .map_err(|error| map_project_definition_query_error(database_path.clone(), error))?;
        decode_project_definition(rows).map_err(|source| {
            RpgMakerExtractionAssetStoreError::InvalidProjectDefinition {
                database_path,
                source,
            }
        })
    }
}

impl<S, C> BuiltinSnapshotStore for RpgMakerExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_builtin(
        &self,
        project: &OpenedProject,
        snapshot: BuiltinSnapshot,
        project_definition_update: BuiltinProjectDefinitionUpdate,
    ) -> Result<(), Self::Error> {
        self.replace(
            project,
            RpgMakerStandardAssetOwner::Builtin,
            snapshot.into_groups(),
            Some(project_definition_update),
        )
        .await
    }
}

impl<S, C> RulesSnapshotStore for RpgMakerExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_rules(
        &self,
        project: &OpenedProject,
        snapshot: RulesSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(
            project,
            RpgMakerStandardAssetOwner::Rules,
            snapshot.into_groups(),
            None,
        )
        .await
    }

    async fn deactivate_rules(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        self.deactivate(project, RpgMakerStandardAssetOwner::Rules)
            .await
    }
}

impl<S, C> LuaSnapshotStore for RpgMakerExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_lua(
        &self,
        project: &OpenedProject,
        snapshot: LuaSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(
            project,
            RpgMakerStandardAssetOwner::Lua,
            snapshot.into_groups(),
            None,
        )
        .await
    }

    async fn deactivate_lua(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        self.deactivate(project, RpgMakerStandardAssetOwner::Lua)
            .await
    }
}

/// 标准提取快照替换的阶段化错误。
#[derive(Debug)]
pub(crate) enum RpgMakerExtractionAssetStoreError<C, S> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    EncodeSnapshot(EncodeAssetSnapshotError),
    EncodeProjectDefinition(MvDialogueDefinitionError),
    DatabaseNotFound {
        database_path: PathBuf,
    },
    ReadCurrentState {
        database_path: PathBuf,
        source: S,
    },
    UnexpectedSnapshotQueryResultCount {
        expected: usize,
        actual: usize,
    },
    ReadProjectDefinition {
        database_path: PathBuf,
        source: S,
    },
    InvalidProjectDefinition {
        database_path: PathBuf,
        source: StoredProjectDefinitionError,
    },
    MutationClaimConflict {
        database_path: PathBuf,
    },
    NotCommitted {
        database_path: PathBuf,
        source: S,
    },
    OutcomeUnknown {
        database_path: PathBuf,
        source: S,
    },
}

impl<C, S> fmt::Display for RpgMakerExtractionAssetStoreError<C, S>
where
    C: fmt::Display,
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleEncoding(source) => write!(formatter, "资产编码任务执行失败：{source}"),
            Self::EncodeSnapshot(source) => write!(formatter, "资产快照编码失败：{source}"),
            Self::EncodeProjectDefinition(source) => {
                write!(formatter, "MV 对话定义编码失败：{source}")
            }
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::ReadCurrentState {
                database_path,
                source,
            } => write!(
                formatter,
                "无法读取当前 owner 快照 {}：{source}",
                database_path.display()
            ),
            Self::UnexpectedSnapshotQueryResultCount { expected, actual } => write!(
                formatter,
                "资产快照查询应返回 {expected} 组结果，实际为 {actual} 组"
            ),
            Self::ReadProjectDefinition {
                database_path,
                source,
            } => write!(
                formatter,
                "无法读取当前 MV 对话定义 {}：{source}",
                database_path.display()
            ),
            Self::InvalidProjectDefinition {
                database_path,
                source,
            } => write!(
                formatter,
                "当前 MV 对话定义无效 {}：{source}",
                database_path.display()
            ),
            Self::MutationClaimConflict { database_path } => write!(
                formatter,
                "标准资产 owner 的物理修改声明发生冲突：{}",
                database_path.display()
            ),
            Self::NotCommitted {
                database_path,
                source,
            } => write!(
                formatter,
                "资产快照未写入 {} 且旧快照保持不变：{source}",
                database_path.display()
            ),
            Self::OutcomeUnknown {
                database_path,
                source,
            } => write!(
                formatter,
                "无法确认资产快照是否已写入 {}：{source}",
                database_path.display()
            ),
        }
    }
}

impl<C, S> Error for RpgMakerExtractionAssetStoreError<C, S>
where
    C: Error + 'static,
    S: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleEncoding(source) => Some(source),
            Self::EncodeSnapshot(source) => Some(source),
            Self::EncodeProjectDefinition(source) => Some(source),
            Self::ReadCurrentState { source, .. } | Self::ReadProjectDefinition { source, .. } => {
                Some(source)
            }
            Self::InvalidProjectDefinition { source, .. } => Some(source),
            Self::NotCommitted { source, .. } | Self::OutcomeUnknown { source, .. } => Some(source),
            Self::DatabaseNotFound { .. }
            | Self::UnexpectedSnapshotQueryResultCount { .. }
            | Self::MutationClaimConflict { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum StoredProjectDefinitionError {
    Missing,
    Multiple,
    WrongColumnCount { actual: usize },
    WrongColumnType { actual: &'static str },
    Invalid(MvDialogueDefinitionError),
    NonCanonical,
}

impl fmt::Display for StoredProjectDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("缺少定义记录"),
            Self::Multiple => formatter.write_str("定义记录不唯一"),
            Self::WrongColumnCount { actual } => {
                write!(formatter, "定义查询应返回一列，实际为 {actual} 列")
            }
            Self::WrongColumnType { actual } => {
                write!(formatter, "canonical_json 应为 TEXT，实际为 {actual}")
            }
            Self::Invalid(source) => {
                write!(formatter, "canonical_json 无法恢复为受信定义：{source}")
            }
            Self::NonCanonical => formatter.write_str("canonical_json 不是当前规范编码"),
        }
    }
}

impl Error for StoredProjectDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(source) => Some(source),
            Self::Missing
            | Self::Multiple
            | Self::WrongColumnCount { .. }
            | Self::WrongColumnType { .. }
            | Self::NonCanonical => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum EncodeAssetSnapshotError {
    Location(RpgMakerLocationCodecError),
    Projection(RpgMakerProjectionCodecError),
    SourceContent(serde_json::Error),
    SourceContext(serde_json::Error),
    DuplicateGroupLocation { group_location: String },
}

impl fmt::Display for EncodeAssetSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => write!(formatter, "位置编码失败：{source}"),
            Self::Projection(source) => write!(formatter, "文本投影编码失败：{source}"),
            Self::SourceContent(source) => write!(formatter, "源内容编码失败：{source}"),
            Self::SourceContext(source) => write!(formatter, "源上下文编码失败：{source}"),
            Self::DuplicateGroupLocation { group_location } => {
                write!(
                    formatter,
                    "多个文本组使用了同一逻辑组位置：{group_location}"
                )
            }
        }
    }
}

impl Error for EncodeAssetSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::SourceContent(source) | Self::SourceContext(source) => Some(source),
            Self::DuplicateGroupLocation { .. } => None,
        }
    }
}

#[derive(Default)]
struct EncodedBatch {
    groups: Vec<EncodedGroup>,
    units: Vec<EncodedUnit>,
    claims: Vec<EncodedClaim>,
}

struct ResolvedProjectDefinition {
    current_canonical_json: String,
    replacement: Option<String>,
}

impl ResolvedProjectDefinition {
    fn canonical_json(&self) -> &str {
        self.replacement
            .as_deref()
            .unwrap_or(&self.current_canonical_json)
    }

    fn is_current(&self) -> bool {
        self.replacement
            .as_deref()
            .is_none_or(|replacement| replacement == self.current_canonical_json.as_str())
    }
}

struct EncodedSnapshot {
    #[cfg(test)]
    owner: RpgMakerStandardAssetOwner,
    groups: Vec<EncodedGroup>,
    units: Vec<EncodedUnit>,
    claims: Vec<EncodedClaim>,
    fingerprint: AssetSnapshotFingerprint,
}

#[cfg_attr(test, derive(Clone, Debug, Default, PartialEq))]
struct StoredSnapshotRows {
    owner_state: Vec<SqliteRow>,
    groups: Vec<SqliteRow>,
    units: Vec<SqliteRow>,
    claims: Vec<SqliteRow>,
}

impl EncodedSnapshot {
    fn merge(
        owner: RpgMakerStandardAssetOwner,
        batches: Vec<EncodedBatch>,
        project_definition_json: Option<&str>,
    ) -> Result<Self, EncodeAssetSnapshotError> {
        let mut groups = Vec::new();
        let mut units = Vec::new();
        let mut claims = Vec::new();
        for batch in batches {
            groups.extend(batch.groups);
            units.extend(batch.units);
            claims.extend(batch.claims);
        }
        claims.sort_by(|left, right| {
            left.resource_key
                .cmp(&right.resource_key)
                .then_with(|| left.access.cmp(&right.access))
                .then_with(|| left.group_location.cmp(&right.group_location))
        });

        let mut group_locations = BTreeSet::new();
        if let Some(duplicate) = groups
            .iter()
            .find(|group| !group_locations.insert(group.group_location.as_str()))
        {
            return Err(EncodeAssetSnapshotError::DuplicateGroupLocation {
                group_location: duplicate.group_location.clone(),
            });
        }

        let fingerprint =
            asset_snapshot_fingerprint(owner, project_definition_json, &groups, &units, &claims);
        Ok(Self {
            #[cfg(test)]
            owner,
            groups,
            units,
            claims,
            fingerprint,
        })
    }

    fn matches_rows(
        &self,
        rows: StoredSnapshotRows,
        source_snapshot_fingerprint: &[u8; 32],
    ) -> bool {
        let StoredSnapshotRows {
            owner_state,
            groups,
            units,
            claims,
        } = rows;
        if !owner_state_matches(
            owner_state,
            source_snapshot_fingerprint,
            self.fingerprint.as_bytes(),
        ) {
            return false;
        }
        let mut rows = groups.into_iter();
        for group in &self.groups {
            if !stored_group_row_matches(rows.next(), group) {
                return false;
            }
        }
        if rows.next().is_some() {
            return false;
        }

        let mut rows = units.into_iter();
        for unit in &self.units {
            if !stored_unit_row_matches(rows.next(), unit) {
                return false;
            }
        }
        if rows.next().is_some() {
            return false;
        }

        let mut rows = claims.into_iter();
        for claim in &self.claims {
            if !stored_claim_row_matches(rows.next(), claim) {
                return false;
            }
        }
        rows.next().is_none()
    }
}

fn owner_state_matches(
    rows: Vec<SqliteRow>,
    source_snapshot_fingerprint: &[u8; 32],
    asset_snapshot_fingerprint: &[u8; 32],
) -> bool {
    let mut rows = rows.into_iter();
    let Some(row) = rows.next() else {
        return false;
    };
    if rows.next().is_some() {
        return false;
    }
    let values = row.values();
    matches!(values, [SqliteValue::Blob(source), SqliteValue::Blob(asset)]
        if source.as_slice() == source_snapshot_fingerprint
            && asset.as_slice() == asset_snapshot_fingerprint)
}

fn stored_group_row_matches(row: Option<SqliteRow>, expected: &EncodedGroup) -> bool {
    let Some(row) = row else {
        return false;
    };
    let Ok(expected_order) = i64::try_from(expected.group_order) else {
        return false;
    };
    matches!(
        row.values(),
        [
            SqliteValue::Text(group_location),
            SqliteValue::Integer(group_order),
            SqliteValue::Text(group_kind),
            SqliteValue::Text(projection_recipe_json),
        ] if group_location == &expected.group_location
            && *group_order == expected_order
            && group_kind == expected.group_kind
            && projection_recipe_json == &expected.projection_recipe_json
    )
}

fn stored_unit_row_matches(row: Option<SqliteRow>, expected: &EncodedUnit) -> bool {
    let Some(row) = row else {
        return false;
    };
    let Ok(expected_order) = i64::try_from(expected.unit_order) else {
        return false;
    };
    matches!(
        row.values(),
        [
            SqliteValue::Text(group_location),
            SqliteValue::Text(unit_role),
            SqliteValue::Integer(unit_order),
            SqliteValue::Text(source_content_json),
            SqliteValue::Text(source_context_json),
        ] if group_location == &expected.group_location
            && unit_role == &expected.unit_role
            && *unit_order == expected_order
            && source_content_json == &expected.source_content_json
            && source_context_json == &expected.source_context_json
    )
}

fn stored_claim_row_matches(row: Option<SqliteRow>, expected: &EncodedClaim) -> bool {
    let Some(row) = row else {
        return false;
    };
    matches!(
        row.values(),
        [
            SqliteValue::Text(resource_key),
            SqliteValue::Text(access),
            SqliteValue::Text(group_location),
        ]
            if resource_key == &expected.resource_key
                && access == expected.access.storage_name()
                && group_location == &expected.group_location
    )
}

struct EncodedGroup {
    group_location: String,
    group_order: usize,
    group_kind: &'static str,
    projection_recipe_json: String,
}

struct EncodedUnit {
    group_location: String,
    unit_role: String,
    unit_order: usize,
    source_content_json: String,
    source_context_json: String,
}

struct EncodedClaim {
    resource_key: String,
    access: MutationResourceAccess,
    group_location: String,
}

#[derive(Serialize)]
struct DialogueBodySourceContext<'a> {
    source_speaker: &'a str,
}

fn split_groups(
    groups: Vec<(usize, ExtractedTextGroup)>,
    groups_per_job: usize,
) -> Vec<Vec<(usize, ExtractedTextGroup)>> {
    let mut groups = groups.into_iter();
    let mut batches = Vec::new();
    loop {
        let batch = groups.by_ref().take(groups_per_job).collect::<Vec<_>>();
        if batch.is_empty() {
            return batches;
        }
        batches.push(batch);
    }
}

fn encode_batch(
    groups: Vec<(usize, ExtractedTextGroup)>,
) -> Result<EncodedBatch, EncodeAssetSnapshotError> {
    let mut encoded = EncodedBatch::default();
    for (group_order, group) in groups {
        let group_location = RpgMakerLocationCodec::encode(group.group_location())
            .map_err(EncodeAssetSnapshotError::Location)?;
        let source_speaker = group
            .units()
            .iter()
            .find(|unit| unit.role() == &TextUnitRole::DialogueSpeaker)
            .and_then(|unit| unit.source_content().as_value());
        let dialogue_context = source_speaker
            .map(|source_speaker| {
                serde_json::to_string(&DialogueBodySourceContext { source_speaker })
                    .map_err(EncodeAssetSnapshotError::SourceContext)
            })
            .transpose()?;

        for (unit_order, unit) in group.units().iter().enumerate() {
            let source_context_json = if matches!(unit.role(), TextUnitRole::DialogueBody) {
                dialogue_context.as_deref().unwrap_or("{}")
            } else {
                "{}"
            };
            encoded.units.push(EncodedUnit {
                group_location: group_location.clone(),
                unit_role: RpgMakerProjectionCodec::encode_role(unit.role())
                    .map_err(EncodeAssetSnapshotError::Projection)?,
                unit_order,
                source_content_json: serde_json::to_string(unit.source_content())
                    .map_err(EncodeAssetSnapshotError::SourceContent)?,
                source_context_json: source_context_json.to_owned(),
            });
        }

        for lock in group.mutation_claims().locks() {
            encoded.claims.push(EncodedClaim {
                resource_key: RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                    .map_err(EncodeAssetSnapshotError::Projection)?,
                access: lock.access(),
                group_location: group_location.clone(),
            });
        }

        encoded.groups.push(EncodedGroup {
            group_location,
            group_order,
            group_kind: group_kind_name(group.kind()),
            projection_recipe_json: RpgMakerProjectionCodec::encode_recipes(group.recipes())
                .map_err(EncodeAssetSnapshotError::Projection)?,
        });
    }
    Ok(encoded)
}

const fn group_kind_name(kind: TextGroupKind) -> &'static str {
    match kind {
        TextGroupKind::DatabaseEntry => "database_entry",
        TextGroupKind::System => "system",
        TextGroupKind::Map => "map",
        TextGroupKind::EventDialogue => "event_dialogue",
        TextGroupKind::EventChoices => "event_choices",
        TextGroupKind::EventScrollingText => "event_scrolling_text",
        TextGroupKind::EventCommand => "event_command",
        TextGroupKind::PluginParameter => "plugin_parameter",
    }
}

fn asset_snapshot_fingerprint(
    owner: RpgMakerStandardAssetOwner,
    project_definition_json: Option<&str>,
    groups: &[EncodedGroup],
    units: &[EncodedUnit],
    claims: &[EncodedClaim],
) -> AssetSnapshotFingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
    hasher.frame(1, owner.storage_name().as_bytes());
    if let Some(project_definition_json) = project_definition_json {
        hasher
            .frame(14, b"project_definition")
            .frame(15, project_definition_json.as_bytes());
    }
    for group in groups {
        let group_order =
            u64::try_from(group.group_order).expect("内存中的 group_order 必须可编码为 u64");
        hasher
            .frame(2, b"group")
            .frame(3, group.group_location.as_bytes())
            .frame(16, &group_order.to_le_bytes())
            .frame(4, group.group_kind.as_bytes())
            .frame(5, group.projection_recipe_json.as_bytes());
    }
    for unit in units {
        let unit_order =
            u64::try_from(unit.unit_order).expect("内存中的 unit_order 必须可编码为 u64");
        hasher
            .frame(6, b"unit")
            .frame(7, unit.group_location.as_bytes())
            .frame(8, unit.unit_role.as_bytes())
            .frame(17, &unit_order.to_le_bytes())
            .frame(9, unit.source_content_json.as_bytes())
            .frame(10, unit.source_context_json.as_bytes());
    }
    for claim in claims {
        hasher
            .frame(11, b"claim")
            .frame(12, claim.resource_key.as_bytes())
            .frame(18, claim.access.storage_name().as_bytes())
            .frame(13, claim.group_location.as_bytes());
    }
    AssetSnapshotFingerprint::from_bytes(hasher.finish().into_bytes())
}

fn build_transaction_plan(
    owner: RpgMakerStandardAssetOwner,
    source_snapshot_fingerprint: [u8; 32],
    snapshot: EncodedSnapshot,
    project_definition_replacement: Option<String>,
) -> SqliteTransactionPlan {
    let EncodedSnapshot {
        groups,
        units,
        claims,
        fingerprint,
        ..
    } = snapshot;
    let mut steps = Vec::new();
    for statement in [
        DROP_STAGING_GROUP,
        DROP_STAGING_UNIT,
        DROP_STAGING_CLAIM,
        DROP_PREVIOUS_UNIT,
        CREATE_STAGING_GROUP,
        CREATE_STAGING_UNIT,
        CREATE_STAGING_CLAIM,
    ] {
        steps.push(execute(statement, Vec::new()));
    }
    steps.push(execute(
        CREATE_PREVIOUS_UNIT,
        vec![text(owner.storage_name())],
    ));

    if !groups.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING_GROUP,
            groups
                .into_iter()
                .map(|group| {
                    vec![
                        text(owner.storage_name()),
                        text(group.group_location),
                        SqliteValue::Integer(
                            i64::try_from(group.group_order)
                                .expect("内存中的 group_order 必须可写入 SQLite INTEGER"),
                        ),
                        text(group.group_kind),
                        text(group.projection_recipe_json),
                    ]
                })
                .collect(),
        )));
    }
    if !units.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING_UNIT,
            units
                .into_iter()
                .map(|unit| {
                    vec![
                        text(owner.storage_name()),
                        text(unit.group_location),
                        text(unit.unit_role),
                        SqliteValue::Integer(
                            i64::try_from(unit.unit_order)
                                .expect("内存中的 unit_order 必须可写入 SQLite INTEGER"),
                        ),
                        text(unit.source_content_json),
                        text(unit.source_context_json),
                    ]
                })
                .collect(),
        )));
    }
    if !claims.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING_CLAIM,
            claims
                .into_iter()
                .map(|claim| {
                    vec![
                        text(owner.storage_name()),
                        text(claim.group_location),
                        text(claim.resource_key),
                        text(claim.access.storage_name()),
                    ]
                })
                .collect(),
        )));
    }

    steps.push(SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        FIND_MUTATION_CLAIM_CONFLICT,
        Vec::new(),
    )));
    if let Some(canonical_json) = project_definition_replacement {
        steps.push(execute(
            UPDATE_PROJECT_DEFINITION,
            vec![
                text(canonical_json),
                text(MV_DIALOGUE_RULES_DEFINITION_KIND),
            ],
        ));
    }
    steps.push(execute(INHERIT_TRANSLATIONS, Vec::new()));
    for statement in [DELETE_OWNER_CLAIMS, DELETE_OWNER_UNITS, DELETE_OWNER_GROUPS] {
        steps.push(execute(statement, vec![text(owner.storage_name())]));
    }
    steps.push(execute(
        UPSERT_OWNER_STATE,
        vec![
            text(owner.storage_name()),
            SqliteValue::Blob(Vec::from(source_snapshot_fingerprint)),
            SqliteValue::Blob(fingerprint.as_bytes().to_vec()),
        ],
    ));
    for statement in [INSERT_GROUPS, INSERT_UNITS, INSERT_CLAIMS] {
        steps.push(execute(statement, Vec::new()));
    }
    for statement in [
        DROP_STAGING_GROUP,
        DROP_STAGING_UNIT,
        DROP_STAGING_CLAIM,
        DROP_PREVIOUS_UNIT,
    ] {
        steps.push(execute(statement, Vec::new()));
    }
    SqliteTransactionPlan::new(steps)
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn execute(statement: &str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn decode_project_definition(rows: Vec<SqliteRow>) -> Result<String, StoredProjectDefinitionError> {
    let mut rows = rows.into_iter();
    let row = rows.next().ok_or(StoredProjectDefinitionError::Missing)?;
    if rows.next().is_some() {
        return Err(StoredProjectDefinitionError::Multiple);
    }
    let values = row.into_values();
    if values.len() != 1 {
        return Err(StoredProjectDefinitionError::WrongColumnCount {
            actual: values.len(),
        });
    }
    let value = values.into_iter().next().expect("已确认定义行恰好有一列");
    let canonical_json = match value {
        SqliteValue::Text(value) => value,
        value => {
            return Err(StoredProjectDefinitionError::WrongColumnType {
                actual: value.kind_name(),
            });
        }
    };
    let definition = MvDialogueDefinition::from_canonical_json(&canonical_json)
        .map_err(StoredProjectDefinitionError::Invalid)?;
    let normalized = definition
        .to_canonical_json()
        .map_err(StoredProjectDefinitionError::Invalid)?;
    if normalized != canonical_json {
        return Err(StoredProjectDefinitionError::NonCanonical);
    }
    Ok(canonical_json)
}

fn map_persist_error<C, S>(
    database_path: PathBuf,
    error: ExecuteTransactionError<S>,
) -> RpgMakerExtractionAssetStoreError<C, S> {
    match error {
        ExecuteTransactionError::NotFound => {
            RpgMakerExtractionAssetStoreError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed => {
            RpgMakerExtractionAssetStoreError::MutationClaimConflict { database_path }
        }
        ExecuteTransactionError::NotCommitted(source) => {
            RpgMakerExtractionAssetStoreError::NotCommitted {
                database_path,
                source,
            }
        }
        ExecuteTransactionError::OutcomeUnknown(source) => {
            RpgMakerExtractionAssetStoreError::OutcomeUnknown {
                database_path,
                source,
            }
        }
    }
}

fn map_query_error<C, S>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<S>,
) -> RpgMakerExtractionAssetStoreError<C, S> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerExtractionAssetStoreError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerExtractionAssetStoreError::ReadCurrentState {
                database_path,
                source,
            }
        }
    }
}

fn map_project_definition_query_error<C, S>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<S>,
) -> RpgMakerExtractionAssetStoreError<C, S> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerExtractionAssetStoreError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerExtractionAssetStoreError::ReadProjectDefinition {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rusqlite::types::Value as RusqliteValue;
    use rusqlite::{Connection, params_from_iter};

    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::extract::model::{
        ExtractedTextUnit, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource,
    };
    use crate::rpg_maker::model::{
        DirectTextPart, DirectTextRecipe, ScalarFieldKey, TextProjectionRecipe, TextUnitContent,
        TextUnitRole,
    };
    use crate::rpg_maker::project::test_layout_profile;
    use crate::rpg_maker::text::StandardDataFile;

    use super::*;

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
            let result = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(result)
        }

        async fn execute_ordered_map<I, T, F>(
            &self,
            inputs: Vec<I>,
            operation: F,
        ) -> Result<Vec<T>, CpuTaskExecutionError<Self::Error>>
        where
            I: Send + 'static,
            T: Send + 'static,
            F: Fn(I) -> T + Send + Sync + 'static,
        {
            if self.fail {
                return Err(CpuTaskExecutionError::Unavailable(FakeError("cpu")));
            }
            self.calls.fetch_add(inputs.len(), Ordering::SeqCst);
            Ok(inputs.into_iter().map(operation).collect())
        }
    }

    #[derive(Clone)]
    struct RecordingSqlite {
        plans: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        owner_state: Arc<Mutex<Vec<SqliteRow>>>,
        deep_owner_state_override: Arc<Mutex<Option<Vec<SqliteRow>>>>,
        snapshot_rows: Arc<Mutex<StoredSnapshotRows>>,
        project_definition: Arc<Mutex<String>>,
        response: Arc<Mutex<Option<SqliteResponse>>>,
        queries: Arc<Mutex<Vec<String>>>,
    }

    impl SqliteQueryExecutor for RecordingSqlite {
        type Error = FakeError;

        async fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
            assert_eq!(path, PathBuf::from("C:/projects/demo/project.db"));
            self.queries
                .lock()
                .expect("查询记录锁不应中毒")
                .push(query.statement().to_owned());
            if query.statement() == READ_PROJECT_DEFINITION {
                assert_eq!(
                    query.parameters(),
                    &[text(MV_DIALOGUE_RULES_DEFINITION_KIND)]
                );
                return Ok(vec![SqliteRow::new(vec![text(
                    self.project_definition
                        .lock()
                        .expect("项目定义锁不应中毒")
                        .clone(),
                )])]);
            }
            if query.statement() == READ_OWNER_STATE {
                assert!(matches!(
                    query.parameters(),
                    [SqliteValue::Text(owner)]
                        if matches!(owner.as_str(), "builtin" | "rules" | "lua")
                ));
                return Ok(self
                    .owner_state
                    .lock()
                    .expect("owner state 锁不应中毒")
                    .clone());
            }
            assert!(matches!(
                query.parameters(),
                [SqliteValue::Text(owner)]
                    if matches!(owner.as_str(), "builtin" | "rules" | "lua")
            ));
            let snapshot = self.snapshot_rows.lock().expect("当前快照锁不应中毒");
            match query.statement() {
                READ_OWNER_GROUPS => Ok(snapshot.groups.clone()),
                READ_OWNER_UNITS => Ok(snapshot.units.clone()),
                READ_OWNER_CLAIMS => Ok(snapshot.claims.clone()),
                statement => panic!("收到未预期的查询：{statement}"),
            }
        }

        async fn query_existing_database_snapshot(
            &self,
            path: PathBuf,
            queries: Vec<SqliteQuery>,
        ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
            let mut results = Vec::with_capacity(queries.len());
            for query in queries {
                if query.statement() == READ_OWNER_STATE
                    && let Some(rows) = self
                        .deep_owner_state_override
                        .lock()
                        .expect("深读 owner state 覆盖锁不应中毒")
                        .clone()
                {
                    assert_eq!(path, PathBuf::from("C:/projects/demo/project.db"));
                    self.queries
                        .lock()
                        .expect("查询记录锁不应中毒")
                        .push(query.statement().to_owned());
                    results.push(rows);
                    continue;
                }
                results.push(self.query_existing_database(path.clone(), query).await?);
            }
            Ok(results)
        }
    }

    impl SqliteTransactionExecutor for RecordingSqlite {
        type Error = FakeError;

        async fn execute_transaction(
            &self,
            path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> Result<(), ExecuteTransactionError<Self::Error>> {
            self.plans
                .lock()
                .expect("事务记录锁不应中毒")
                .push((path, plan));
            match self.response.lock().expect("响应锁不应中毒").take() {
                None => Ok(()),
                Some(SqliteResponse::NotFound) => Err(ExecuteTransactionError::NotFound),
                Some(SqliteResponse::Conflict) => Err(ExecuteTransactionError::RequirementFailed),
                Some(SqliteResponse::NotCommitted) => {
                    Err(ExecuteTransactionError::NotCommitted(FakeError("write")))
                }
                Some(SqliteResponse::OutcomeUnknown) => {
                    Err(ExecuteTransactionError::OutcomeUnknown(FakeError("commit")))
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum SqliteResponse {
        NotFound,
        Conflict,
        NotCommitted,
        OutcomeUnknown,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    struct Harness {
        sqlite: RecordingSqlite,
        cpu: RecordingCpu,
    }

    impl Harness {
        fn new(response: Option<SqliteResponse>) -> Self {
            Self {
                sqlite: RecordingSqlite {
                    plans: Arc::new(Mutex::new(Vec::new())),
                    owner_state: Arc::new(Mutex::new(Vec::new())),
                    deep_owner_state_override: Arc::new(Mutex::new(None)),
                    snapshot_rows: Arc::new(Mutex::new(StoredSnapshotRows::default())),
                    project_definition: Arc::new(Mutex::new(
                        MvDialogueDefinition::empty()
                            .to_canonical_json()
                            .expect("空定义应可编码"),
                    )),
                    response: Arc::new(Mutex::new(response)),
                    queries: Arc::new(Mutex::new(Vec::new())),
                },
                cpu: RecordingCpu {
                    calls: Arc::new(AtomicUsize::new(0)),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::new(AtomicUsize::new(0)),
                    fail: false,
                },
            }
        }

        fn service(
            &self,
            groups_per_job: usize,
        ) -> RpgMakerExtractionAssetStore<RecordingSqlite, RecordingCpu> {
            RpgMakerExtractionAssetStore::new(
                self.sqlite.clone(),
                self.cpu.clone(),
                RpgMakerExtractionAssetStoreConfig::new(non_zero(groups_per_job)),
            )
        }
    }

    #[test]
    fn config_exposes_the_explicit_batch_size() {
        let config = RpgMakerExtractionAssetStoreConfig::new(non_zero(20));

        assert_eq!(config.groups_per_encode_job().get(), 20);
    }

    #[test]
    fn every_group_kind_maps_to_the_single_group_table_contract() {
        let cases = [
            (TextGroupKind::DatabaseEntry, "database_entry"),
            (TextGroupKind::System, "system"),
            (TextGroupKind::Map, "map"),
            (TextGroupKind::EventDialogue, "event_dialogue"),
            (TextGroupKind::EventChoices, "event_choices"),
            (TextGroupKind::EventScrollingText, "event_scrolling_text"),
            (TextGroupKind::EventCommand, "event_command"),
            (TextGroupKind::PluginParameter, "plugin_parameter"),
        ];

        for (kind, expected) in cases {
            assert_eq!(group_kind_name(kind), expected);
        }
    }

    #[test]
    fn encoding_uses_projection_codecs_and_source_speaker_context() {
        let group = dialogue_group("角色", "第一句");
        let encoded = encode_test_batch(vec![group]).expect("对话快照应可编码");

        assert_eq!(encoded.groups.len(), 1);
        assert_eq!(encoded.units.len(), 2);
        assert!(!encoded.claims.is_empty());
        assert_eq!(encoded.groups[0].group_kind, "event_dialogue");
        RpgMakerProjectionCodec::decode_recipes(&encoded.groups[0].projection_recipe_json)
            .expect("配方必须是可逆的内部 canonical JSON");
        for claim in &encoded.claims {
            RpgMakerProjectionCodec::decode_mutation_resource(&claim.resource_key)
                .expect("修改资源必须是可逆的内部 canonical JSON");
        }
        for unit in &encoded.units {
            RpgMakerProjectionCodec::decode_role(&unit.unit_role)
                .expect("角色必须是可逆的内部 canonical JSON");
        }

        let body = encoded
            .units
            .iter()
            .find(|unit| {
                RpgMakerProjectionCodec::decode_role(&unit.unit_role)
                    .is_ok_and(|role| role == TextUnitRole::DialogueBody)
            })
            .expect("应存在正文单元");
        assert_eq!(body.source_content_json, r#"["第一句"]"#);
        assert_eq!(body.source_context_json, r#"{"source_speaker":"角色"}"#);
        let speaker = encoded
            .units
            .iter()
            .find(|unit| {
                RpgMakerProjectionCodec::decode_role(&unit.unit_role)
                    .is_ok_and(|role| role == TextUnitRole::DialogueSpeaker)
            })
            .expect("应存在 Speaker 单元");
        assert_eq!(speaker.source_content_json, r#""角色""#);
        assert_eq!(speaker.source_context_json, "{}");
    }

    #[test]
    fn asset_fingerprint_covers_owner_groups_units_context_recipes_and_claims() {
        let base = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            projected_group("<a>", "</a>"),
        );
        let different_owner = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Rules,
            projected_group("<a>", "</a>"),
        );
        let different_recipe = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            projected_group("<b>", "</b>"),
        );
        let different_text = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            scalar_group(1, "name", "另一段原文"),
        );
        let different_target = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            scalar_group(2, "name", "原文"),
        );
        let with_context = snapshot_fingerprint(
            RpgMakerStandardAssetOwner::Builtin,
            dialogue_group("角色", "原文"),
        );
        let with_project_definition = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Builtin,
            vec![
                encode_test_batch(vec![projected_group("<a>", "</a>")]).expect("测试快照应可编码"),
            ],
            Some(r#"{"rules":[]}"#),
        )
        .expect("带项目定义的快照应可合并")
        .fingerprint;

        assert_ne!(base, different_owner);
        assert_ne!(base, different_recipe);
        assert_ne!(base, different_text);
        assert_ne!(base, different_target);
        assert_ne!(base, with_context);
        assert_ne!(base, with_project_definition);
    }

    #[test]
    fn asset_fingerprint_tracks_group_and_unit_order_without_changing_inheritance_identity() {
        let forward_groups = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Rules,
            vec![
                encode_test_batch(vec![
                    scalar_group(1, "name", "一"),
                    scalar_group(2, "name", "二"),
                ])
                .expect("正序组应可编码"),
            ],
            None,
        )
        .expect("正序组快照应可合并");
        let reverse_groups = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Rules,
            vec![
                encode_test_batch(vec![
                    scalar_group(2, "name", "二"),
                    scalar_group(1, "name", "一"),
                ])
                .expect("逆序组应可编码"),
            ],
            None,
        )
        .expect("逆序组快照应可合并");
        let forward_units = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Lua,
            vec![encode_test_batch(vec![two_field_group(false)]).expect("正序单元应可编码")],
            None,
        )
        .expect("正序单元快照应可合并");
        let reverse_units = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Lua,
            vec![encode_test_batch(vec![two_field_group(true)]).expect("逆序单元应可编码")],
            None,
        )
        .expect("逆序单元快照应可合并");

        assert_ne!(forward_groups.fingerprint, reverse_groups.fingerprint);
        assert_ne!(forward_units.fingerprint, reverse_units.fingerprint);
        assert!(INHERIT_TRANSLATIONS.contains("previous.group_location"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.unit_role"));
        assert!(!INHERIT_TRANSLATIONS.contains("group_order"));
        assert!(!INHERIT_TRANSLATIONS.contains("unit_order"));
    }

    #[test]
    fn owner_state_shortcut_requires_both_exact_fingerprints() {
        assert!(owner_state_matches(
            owner_state_rows(&[0xa5; 32], &[0xb4; 32]),
            &[0xa5; 32],
            &[0xb4; 32],
        ));
        assert!(!owner_state_matches(
            owner_state_rows(&[0x33; 32], &[0xb4; 32]),
            &[0xa5; 32],
            &[0xb4; 32],
        ));
        assert!(!owner_state_matches(
            owner_state_rows(&[0xa5; 32], &[0x44; 32]),
            &[0xa5; 32],
            &[0xb4; 32],
        ));
        assert!(!owner_state_matches(
            vec![SqliteRow::new(vec![text("not-a-blob"), text("bad")])],
            &[0xa5; 32],
            &[0xb4; 32],
        ));
    }

    #[test]
    fn translation_inheritance_uses_logical_identity_text_and_context_only() {
        assert!(INHERIT_TRANSLATIONS.contains("previous.group_location"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.unit_role"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.source_content_json"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.source_context_json"));
        assert!(!INHERIT_TRANSLATIONS.contains("projection_recipe_json"));
        assert!(!INHERIT_TRANSLATIONS.contains("mutation_target"));
    }

    #[test]
    fn recipe_shell_change_inherits_translation_in_a_real_transaction() {
        let owner = RpgMakerStandardAssetOwner::Builtin;
        let old = EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![projected_group("<a>", "</a>")]).expect("旧快照应可编码")],
            None,
        )
        .expect("旧快照应可合并");
        let new = EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![projected_group("<b>", "</b>")]).expect("新快照应可编码")],
            None,
        )
        .expect("新快照应可合并");
        assert_ne!(old.fingerprint, new.fingerprint);

        let mut connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);
        seed_snapshot(&connection, &old, r#""译文""#, &[0x44; 32]);
        execute_plan(
            &mut connection,
            build_transaction_plan(owner, [0xa5; 32], new, None),
        )
        .expect("配方外壳变化应完成替换");

        let (translation, state): (String, Vec<u8>) = connection
            .query_row(
                "SELECT translation_content_json, translation_state FROM standard_text_unit",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("继承后的单元应存在");
        assert_eq!(translation, r#""译文""#);
        assert_eq!(state, vec![0x44; 32]);
        let recipe: String = connection
            .query_row(
                "SELECT projection_recipe_json FROM standard_text_group",
                [],
                |row| row.get(0),
            )
            .expect("新配方应存在");
        assert!(recipe.contains("<b>"));
        assert!(!recipe.contains("<a>"));
    }

    #[test]
    fn cross_owner_claim_conflict_rolls_back_and_keeps_the_previous_snapshot() {
        let builtin = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Builtin,
            vec![
                encode_test_batch(vec![scalar_group(1, "name", "Builtin 原文")])
                    .expect("Builtin 快照应可编码"),
            ],
            None,
        )
        .expect("Builtin 快照应可合并");
        let previous_rules = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Rules,
            vec![
                encode_test_batch(vec![scalar_group(2, "name", "旧 Rules 原文")])
                    .expect("旧 Rules 快照应可编码"),
            ],
            None,
        )
        .expect("旧 Rules 快照应可合并");
        let conflicting_rules = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Rules,
            vec![
                encode_test_batch(vec![scalar_group(1, "name", "新 Rules 原文")])
                    .expect("冲突 Rules 快照应可编码"),
            ],
            None,
        )
        .expect("冲突 Rules 快照应可合并");

        let mut connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);
        seed_snapshot(&connection, &builtin, r#""Builtin 译文""#, &[0x11; 32]);
        seed_snapshot(
            &connection,
            &previous_rules,
            r#""旧 Rules 译文""#,
            &[0x22; 32],
        );
        let previous_rows = read_snapshot_rows(&connection, RpgMakerStandardAssetOwner::Rules);

        let error = execute_plan(
            &mut connection,
            build_transaction_plan(
                RpgMakerStandardAssetOwner::Rules,
                [0xa5; 32],
                conflicting_rules,
                None,
            ),
        )
        .expect_err("跨 owner 的 Exclusive Claim 冲突必须终止整个事务");

        assert_eq!(error, "requirement failed");
        assert_eq!(
            read_snapshot_rows(&connection, RpgMakerStandardAssetOwner::Rules),
            previous_rows,
            "冲突替换不得删除、部分覆盖或污染旧 Rules 快照"
        );
        assert_eq!(
            read_snapshot_rows(&connection, RpgMakerStandardAssetOwner::Builtin),
            snapshot_rows(&builtin),
            "冲突替换不得改变另一个 owner 的权威快照"
        );
    }

    #[tokio::test]
    async fn replacement_keeps_all_snapshot_planning_in_cpu_jobs_and_uses_three_asset_tables() {
        let harness = Harness::new(None);
        let groups = (0..8)
            .map(|index| scalar_group(index, "name", &format!("文本 {index}")))
            .collect::<Vec<_>>();

        harness
            .service(1)
            .replace_builtin(
                &project(),
                BuiltinSnapshot::new(groups).expect("快照应合法"),
                BuiltinProjectDefinitionUpdate::Reuse,
            )
            .await
            .expect("快照应完成替换");

        assert_eq!(harness.cpu.calls.load(Ordering::SeqCst), 11);
        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [
                READ_OWNER_STATE.to_owned(),
                READ_PROJECT_DEFINITION.to_owned()
            ]
        );
        let plans = harness.sqlite.plans.lock().expect("事务记录锁不应中毒");
        assert_eq!(plans.len(), 1);
        let statements = plan_statements(&plans[0].1).join("\n");
        assert!(statements.contains("standard_text_group"));
        assert!(statements.contains("standard_text_unit"));
        assert!(statements.contains("standard_mutation_claim"));
    }

    #[tokio::test]
    async fn builtin_definition_replacement_and_snapshot_share_one_transaction() {
        let harness = Harness::new(None);
        let definition = MvDialogueDefinition::parse_toml(
            r#"
                [[rule]]
                pattern = '(?<speaker>.+)'
            "#,
        )
        .expect("测试定义应合法");
        let expected_json = definition.to_canonical_json().expect("定义应可编码");

        harness
            .service(1)
            .replace_builtin(
                &project(),
                BuiltinSnapshot::new(vec![scalar_group(1, "name", "原文")]).expect("快照应合法"),
                BuiltinProjectDefinitionUpdate::Replace(definition),
            )
            .await
            .expect("定义与快照应原子替换");

        let plans = harness.sqlite.plans.lock().expect("事务记录锁不应中毒");
        assert_eq!(plans.len(), 1, "定义与资产不得拆成两个事务");
        let steps = plans[0].1.steps();
        let definition_update = steps
            .iter()
            .position(|step| {
                matches!(step, SqliteTransactionStep::Execute(command) if command.statement() == UPDATE_PROJECT_DEFINITION)
            })
            .expect("事务必须包含项目定义更新");
        let owner_update = steps
            .iter()
            .position(|step| {
                matches!(step, SqliteTransactionStep::Execute(command) if command.statement() == UPSERT_OWNER_STATE)
            })
            .expect("事务必须包含 owner state 更新");
        assert!(definition_update < owner_update);
        let command = match &steps[definition_update] {
            SqliteTransactionStep::Execute(command) => command,
            _ => unreachable!("已确认是定义更新命令"),
        };
        assert_eq!(
            command.parameters(),
            &[
                text(expected_json.clone()),
                text(MV_DIALOGUE_RULES_DEFINITION_KIND)
            ]
        );
        let owner_command = steps
            .iter()
            .find_map(|step| match step {
                SqliteTransactionStep::Execute(command)
                    if command.statement() == UPSERT_OWNER_STATE =>
                {
                    Some(command)
                }
                _ => None,
            })
            .expect("owner state 更新应存在");
        assert!(
            matches!(&owner_command.parameters()[2], SqliteValue::Blob(value) if value.len() == 32)
        );
        let plan = plans[0].1.clone();
        drop(plans);

        let mut connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);
        execute_plan(&mut connection, plan).expect("定义与资产事务应可整体提交");
        let persisted: String = connection
            .query_row(
                "SELECT canonical_json FROM standard_project_definition WHERE definition_kind = 'mv_dialogue_rules'",
                [],
                |row| row.get(0),
            )
            .expect("提交后定义应存在");
        assert_eq!(persisted, expected_json);
    }

    #[test]
    fn stored_project_definition_requires_one_canonical_current_row() {
        let canonical = MvDialogueDefinition::empty()
            .to_canonical_json()
            .expect("空定义应可编码");
        assert_eq!(
            decode_project_definition(vec![SqliteRow::new(vec![text(canonical.clone())])])
                .expect("规范定义应恢复"),
            canonical
        );
        assert!(matches!(
            decode_project_definition(Vec::new()),
            Err(StoredProjectDefinitionError::Missing)
        ));
        assert!(matches!(
            decode_project_definition(vec![SqliteRow::new(vec![SqliteValue::Blob(Vec::new())])]),
            Err(StoredProjectDefinitionError::WrongColumnType { actual: "BLOB" })
        ));
        assert!(matches!(
            decode_project_definition(vec![SqliteRow::new(vec![text("{ \"rules\": [] }")])]),
            Err(StoredProjectDefinitionError::NonCanonical)
        ));
    }

    #[tokio::test]
    async fn identical_snapshot_skips_the_write_transaction() {
        let harness = Harness::new(None);
        let owner = RpgMakerStandardAssetOwner::Builtin;
        let group = scalar_group(1, "name", "原文");
        let encoded = EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![group.clone()]).expect("快照应可编码")],
            Some(r#"{"rules":[]}"#),
        )
        .expect("快照应可合并");
        let source_snapshot_fingerprint = project().source_snapshot_fingerprint();
        *harness
            .sqlite
            .owner_state
            .lock()
            .expect("owner state 锁不应中毒") = owner_state_rows(
            source_snapshot_fingerprint.as_bytes(),
            encoded.fingerprint.as_bytes(),
        );
        *harness
            .sqlite
            .snapshot_rows
            .lock()
            .expect("当前快照锁不应中毒") = snapshot_rows(&encoded);

        harness
            .service(1)
            .replace_builtin(
                &project(),
                BuiltinSnapshot::new(vec![group]).expect("快照应合法"),
                BuiltinProjectDefinitionUpdate::Reuse,
            )
            .await
            .expect("相同快照应正常收敛");

        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [
                READ_OWNER_STATE.to_owned(),
                READ_PROJECT_DEFINITION.to_owned(),
                READ_OWNER_STATE.to_owned(),
                READ_OWNER_GROUPS.to_owned(),
                READ_OWNER_UNITS.to_owned(),
                READ_OWNER_CLAIMS.to_owned(),
            ]
        );
        assert!(
            harness
                .sqlite
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deep_snapshot_rechecks_owner_state_in_the_same_read_view() {
        let harness = Harness::new(None);
        let owner = RpgMakerStandardAssetOwner::Rules;
        let group = scalar_group(1, "name", "原文");
        let encoded = EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![group.clone()]).expect("快照应可编码")],
            None,
        )
        .expect("快照应可合并");
        let source_snapshot_fingerprint = project().source_snapshot_fingerprint();
        *harness
            .sqlite
            .owner_state
            .lock()
            .expect("owner state 锁不应中毒") = owner_state_rows(
            source_snapshot_fingerprint.as_bytes(),
            encoded.fingerprint.as_bytes(),
        );
        *harness
            .sqlite
            .snapshot_rows
            .lock()
            .expect("当前快照锁不应中毒") = snapshot_rows(&encoded);
        *harness
            .sqlite
            .deep_owner_state_override
            .lock()
            .expect("深读 owner state 覆盖锁不应中毒") = Some(owner_state_rows(
            source_snapshot_fingerprint.as_bytes(),
            &[0x44; 32],
        ));

        harness
            .service(1)
            .replace_rules(
                &project(),
                RulesSnapshot::new(vec![group]).expect("快照应合法"),
            )
            .await
            .expect("深读视图中的 owner 变化应触发权威替换");

        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [
                READ_OWNER_STATE.to_owned(),
                READ_OWNER_STATE.to_owned(),
                READ_OWNER_GROUPS.to_owned(),
                READ_OWNER_UNITS.to_owned(),
                READ_OWNER_CLAIMS.to_owned(),
            ]
        );
        assert_eq!(
            harness
                .sqlite
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .len(),
            1,
            "深读 owner state 与期望不一致时不得错误早退"
        );
    }

    #[tokio::test]
    async fn matching_fingerprints_deep_read_and_repair_a_damaged_snapshot() {
        let harness = Harness::new(None);
        let owner = RpgMakerStandardAssetOwner::Rules;
        let group = scalar_group(1, "name", "原文");
        let encoded = EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![group.clone()]).expect("快照应可编码")],
            None,
        )
        .expect("快照应可合并");
        let source_snapshot_fingerprint = project().source_snapshot_fingerprint();
        *harness
            .sqlite
            .owner_state
            .lock()
            .expect("owner state 锁不应中毒") = owner_state_rows(
            source_snapshot_fingerprint.as_bytes(),
            encoded.fingerprint.as_bytes(),
        );
        harness
            .service(1)
            .replace_rules(
                &project(),
                RulesSnapshot::new(vec![group]).expect("快照应合法"),
            )
            .await
            .expect("损坏快照应通过权威替换修复");

        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [
                READ_OWNER_STATE.to_owned(),
                READ_OWNER_STATE.to_owned(),
                READ_OWNER_GROUPS.to_owned(),
                READ_OWNER_UNITS.to_owned(),
                READ_OWNER_CLAIMS.to_owned(),
            ]
        );
        assert_eq!(
            harness
                .sqlite
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn changed_fingerprint_skips_the_deep_snapshot_read() {
        let harness = Harness::new(None);
        *harness
            .sqlite
            .owner_state
            .lock()
            .expect("owner state 锁不应中毒") = owner_state_rows(&[0xa5; 32], &[0xb4; 32]);

        harness
            .service(1)
            .replace_rules(
                &project(),
                RulesSnapshot::new(vec![scalar_group(1, "name", "原文")]).expect("快照应合法"),
            )
            .await
            .expect("指纹变化应直接执行权威替换");

        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [READ_OWNER_STATE.to_owned()]
        );
        assert_eq!(
            harness
                .sqlite
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn active_empty_snapshot_and_deactivated_owner_are_distinct() {
        let harness = Harness::new(None);

        harness
            .service(1)
            .replace_rules(&project(), RulesSnapshot::empty())
            .await
            .expect("active 空快照应写入 owner state");
        {
            let plans = harness.sqlite.plans.lock().expect("事务记录锁不应中毒");
            let upsert = plans[0]
                .1
                .steps()
                .iter()
                .find_map(|step| match step {
                    SqliteTransactionStep::Execute(command)
                        if command.statement() == UPSERT_OWNER_STATE =>
                    {
                        Some(command)
                    }
                    _ => None,
                })
                .expect("active 空快照必须写入 owner state");
            assert_eq!(upsert.parameters()[0], text("rules"));
            assert!(
                matches!(&upsert.parameters()[2], SqliteValue::Blob(value) if value.len() == 32)
            );
        }

        *harness
            .sqlite
            .owner_state
            .lock()
            .expect("owner state 锁不应中毒") = owner_state_rows(&[0xa5; 32], &[0xb4; 32]);
        harness
            .service(1)
            .deactivate_rules(&project())
            .await
            .expect("停用应删除 owner state");
        let plans = harness.sqlite.plans.lock().expect("事务记录锁不应中毒");
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plan_statements(&plans[1].1),
            vec![DEACTIVATE_OWNER.to_owned()]
        );
        assert_eq!(
            *harness.sqlite.queries.lock().expect("查询记录锁不应中毒"),
            [READ_OWNER_STATE.to_owned(), READ_OWNER_STATE.to_owned()]
        );
    }

    #[tokio::test]
    async fn global_target_requirement_maps_to_a_specific_conflict_error() {
        let harness = Harness::new(Some(SqliteResponse::Conflict));
        let error = harness
            .service(1)
            .replace_rules(
                &project(),
                RulesSnapshot::new(vec![scalar_group(1, "name", "原文")]).expect("快照应合法"),
            )
            .await
            .expect_err("物理目标冲突必须失败");

        assert!(matches!(
            error,
            RpgMakerExtractionAssetStoreError::MutationClaimConflict { .. }
        ));
        let plans = harness.sqlite.plans.lock().expect("事务记录锁不应中毒");
        let requirement = plans[0]
            .1
            .steps()
            .iter()
            .find_map(|step| match step {
                SqliteTransactionStep::RequireNoRows(query) => Some(query),
                _ => None,
            })
            .expect("事务必须显式检查全局修改目标冲突");
        assert_eq!(requirement.statement(), FIND_MUTATION_CLAIM_CONFLICT);
        assert!(
            requirement
                .statement()
                .contains("current.owner <> staged.owner")
        );
    }

    #[tokio::test]
    async fn terminal_transaction_errors_keep_their_distinct_meaning() {
        for response in [
            SqliteResponse::NotFound,
            SqliteResponse::NotCommitted,
            SqliteResponse::OutcomeUnknown,
        ] {
            let harness = Harness::new(Some(response));
            let error = harness
                .service(1)
                .replace_lua(
                    &project(),
                    LuaSnapshot::new(vec![scalar_group(1, "name", "原文")]).expect("快照应合法"),
                )
                .await
                .expect_err("预设的事务终态必须返回");
            assert!(matches!(
                (response, error),
                (
                    SqliteResponse::NotFound,
                    RpgMakerExtractionAssetStoreError::DatabaseNotFound { .. },
                ) | (
                    SqliteResponse::NotCommitted,
                    RpgMakerExtractionAssetStoreError::NotCommitted { .. },
                ) | (
                    SqliteResponse::OutcomeUnknown,
                    RpgMakerExtractionAssetStoreError::OutcomeUnknown { .. },
                )
            ));
        }
    }

    #[test]
    fn consuming_transaction_plan_preserves_snapshot_values_and_natural_order() {
        let owner = RpgMakerStandardAssetOwner::Rules;
        let snapshot = EncodedSnapshot::merge(
            owner,
            vec![
                encode_test_batch(vec![
                    scalar_group(2, "description", "说明"),
                    scalar_group(1, "name", "名称"),
                ])
                .expect("测试快照应可编码"),
            ],
            None,
        )
        .expect("测试快照应可合并");
        let expected = snapshot_rows(&snapshot);

        let mut connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);
        execute_plan(
            &mut connection,
            build_transaction_plan(owner, [0xa5; 32], snapshot, None),
        )
        .expect("消费式事务计划应完整写入快照");

        assert_eq!(read_snapshot_rows(&connection, owner), expected);
    }

    #[test]
    fn narrow_snapshot_rows_require_exact_table_contents_and_types() {
        let snapshot = EncodedSnapshot::merge(
            RpgMakerStandardAssetOwner::Rules,
            vec![
                encode_test_batch(vec![
                    scalar_group(2, "description", "说明"),
                    scalar_group(1, "name", "名称"),
                ])
                .expect("测试快照应可编码"),
            ],
            None,
        )
        .expect("测试快照应可合并");
        let current = snapshot_rows(&snapshot);
        assert!(snapshot.matches_rows(current.clone(), &[0xa5; 32]));

        for column in 0..4 {
            let mut damaged = current.clone();
            let mut values = damaged.groups[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.groups[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }
        for column in 0..5 {
            let mut damaged = current.clone();
            let mut values = damaged.units[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.units[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }
        for column in 0..3 {
            let mut damaged = current.clone();
            let mut values = damaged.claims[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.claims[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }

        for table in 0..3 {
            let mut missing = current.clone();
            match table {
                0 => {
                    missing.groups.pop();
                }
                1 => {
                    missing.units.pop();
                }
                2 => {
                    missing.claims.pop();
                }
                _ => unreachable!("测试表编号固定为 0..3"),
            }
            assert!(!snapshot.matches_rows(missing, &[0xa5; 32]));

            let mut extra = current.clone();
            match table {
                0 => extra.groups.push(current.groups[0].clone()),
                1 => extra.units.push(current.units[0].clone()),
                2 => extra.claims.push(current.claims[0].clone()),
                _ => unreachable!("测试表编号固定为 0..3"),
            }
            assert!(!snapshot.matches_rows(extra, &[0xa5; 32]));
        }
    }

    #[test]
    fn narrow_snapshot_queries_use_authoritative_indexes_without_temp_sorting() {
        let connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);

        for query in [
            READ_OWNER_STATE,
            READ_OWNER_GROUPS,
            READ_OWNER_UNITS,
            READ_OWNER_CLAIMS,
        ] {
            let explain = format!("EXPLAIN QUERY PLAN {query}");
            let mut statement = connection.prepare(&explain).expect("查询计划应可准备");
            let details = statement
                .query_map([RpgMakerStandardAssetOwner::Rules.storage_name()], |row| {
                    row.get::<_, String>(3)
                })
                .expect("查询计划应可读取")
                .collect::<Result<Vec<_>, _>>()
                .expect("查询计划行应可解码");

            assert!(
                details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
                "深快照查询不得创建临时排序树：{details:?}"
            );
            assert!(
                details.iter().any(|detail| {
                    detail.contains("USING INDEX") || detail.contains("USING COVERING INDEX")
                }),
                "深快照查询必须按现有权威索引读取：{details:?}"
            );
        }
    }

    fn snapshot_fingerprint(
        owner: RpgMakerStandardAssetOwner,
        group: ExtractedTextGroup,
    ) -> AssetSnapshotFingerprint {
        EncodedSnapshot::merge(
            owner,
            vec![encode_test_batch(vec![group]).expect("测试快照应可编码")],
            None,
        )
        .expect("测试快照应可合并")
        .fingerprint
    }

    fn scalar_group(index: usize, field_name: &str, source_text: &str) -> ExtractedTextGroup {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(index)]);
        let physical_location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(index),
                RpgMakerLocationStep::key(field_name),
            ],
        );
        ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![
                ExtractedTextUnit::new(field_name, physical_location, source_text)
                    .expect("标量字段应合法"),
            ],
        )
        .expect("标量组应合法")
    }

    fn two_field_group(reverse: bool) -> ExtractedTextGroup {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let unit = |field_name: &str, source_text: &str| {
            ExtractedTextUnit::new(
                field_name,
                RpgMakerLocation::value(
                    source.clone(),
                    vec![
                        RpgMakerLocationStep::index(1),
                        RpgMakerLocationStep::key(field_name),
                    ],
                ),
                source_text,
            )
            .expect("测试字段应合法")
        };
        let mut units = vec![unit("zeta", "先"), unit("alpha", "后")];
        if reverse {
            units.reverse();
        }
        ExtractedTextGroup::new(TextGroupKind::DatabaseEntry, group_location, units)
            .expect("双字段组应合法")
    }

    fn projected_group(prefix: &str, suffix: &str) -> ExtractedTextGroup {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let target = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("note"),
            ],
        );
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("match[0]").expect("角色应合法"));
        let unit = ExtractedTextUnit::projected(
            role.clone(),
            target.clone(),
            TextUnitContent::Value("原文".to_owned()),
        )
        .expect("投影单元应合法");
        let recipe = TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                target,
                format!("{prefix}原文{suffix}"),
                vec![
                    DirectTextPart::Literal(prefix.to_owned()),
                    DirectTextPart::TextSlot { role },
                    DirectTextPart::Literal(suffix.to_owned()),
                ],
            )
            .expect("局部文本配方应合法"),
        );
        ExtractedTextGroup::projected(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![unit],
            vec![recipe],
        )
        .expect("投影组应合法")
    }

    fn dialogue_group(speaker: &str, body: &str) -> ExtractedTextGroup {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let group_location = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let speaker_location = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(4),
            ],
        );
        let body_location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let speaker_unit = ExtractedTextUnit::projected(
            TextUnitRole::DialogueSpeaker,
            speaker_location.clone(),
            TextUnitContent::Value(speaker.to_owned()),
        )
        .expect("Speaker 单元应合法");
        let body_unit = ExtractedTextUnit::projected(
            TextUnitRole::DialogueBody,
            body_location.clone(),
            TextUnitContent::Lines(vec![body.to_owned()]),
        )
        .expect("正文单元应合法");
        let speaker_recipe = DirectTextRecipe::new(
            speaker_location,
            speaker,
            vec![DirectTextPart::TextSlot {
                role: TextUnitRole::DialogueSpeaker,
            }],
        )
        .map(TextProjectionRecipe::Direct)
        .expect("Speaker 配方应合法");
        let body_recipe = DirectTextRecipe::new(
            body_location,
            body,
            vec![DirectTextPart::LineSlot {
                role: TextUnitRole::DialogueBody,
                source_line_index: 0,
            }],
        )
        .map(TextProjectionRecipe::Direct)
        .expect("正文配方应合法");
        ExtractedTextGroup::projected(
            TextGroupKind::EventDialogue,
            group_location,
            vec![speaker_unit, body_unit],
            vec![speaker_recipe, body_recipe],
        )
        .expect("对话组应合法")
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("测试项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        )
    }

    fn encode_test_batch(
        groups: Vec<ExtractedTextGroup>,
    ) -> Result<EncodedBatch, EncodeAssetSnapshotError> {
        encode_batch(groups.into_iter().enumerate().collect())
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试值应非零")
    }

    fn owner_state_rows(
        source_snapshot_fingerprint: &[u8; 32],
        asset_snapshot_fingerprint: &[u8; 32],
    ) -> Vec<SqliteRow> {
        vec![SqliteRow::new(vec![
            SqliteValue::Blob(source_snapshot_fingerprint.to_vec()),
            SqliteValue::Blob(asset_snapshot_fingerprint.to_vec()),
        ])]
    }

    fn snapshot_rows(snapshot: &EncodedSnapshot) -> StoredSnapshotRows {
        let groups = snapshot
            .groups
            .iter()
            .map(|group| {
                SqliteRow::new(vec![
                    text(group.group_location.clone()),
                    SqliteValue::Integer(
                        i64::try_from(group.group_order).expect("测试顺序应可编码"),
                    ),
                    text(group.group_kind),
                    text(group.projection_recipe_json.clone()),
                ])
            })
            .collect();
        let units = snapshot
            .units
            .iter()
            .map(|unit| {
                SqliteRow::new(vec![
                    text(unit.group_location.clone()),
                    text(unit.unit_role.clone()),
                    SqliteValue::Integer(i64::try_from(unit.unit_order).expect("测试顺序应可编码")),
                    text(unit.source_content_json.clone()),
                    text(unit.source_context_json.clone()),
                ])
            })
            .collect();
        let claims = snapshot
            .claims
            .iter()
            .map(|claim| {
                SqliteRow::new(vec![
                    text(claim.resource_key.clone()),
                    text(claim.access.storage_name()),
                    text(claim.group_location.clone()),
                ])
            })
            .collect();
        StoredSnapshotRows {
            owner_state: owner_state_rows(&[0xa5; 32], snapshot.fingerprint.as_bytes()),
            groups,
            units,
            claims,
        }
    }

    fn read_snapshot_rows(
        connection: &Connection,
        owner: RpgMakerStandardAssetOwner,
    ) -> StoredSnapshotRows {
        StoredSnapshotRows {
            owner_state: read_rows(connection, READ_OWNER_STATE, owner, 2),
            groups: read_rows(connection, READ_OWNER_GROUPS, owner, 4),
            units: read_rows(connection, READ_OWNER_UNITS, owner, 5),
            claims: read_rows(connection, READ_OWNER_CLAIMS, owner, 3),
        }
    }

    fn read_rows(
        connection: &Connection,
        query: &str,
        owner: RpgMakerStandardAssetOwner,
        column_count: usize,
    ) -> Vec<SqliteRow> {
        let mut statement = connection.prepare(query).expect("快照查询应可准备");
        let mut rows = statement
            .query([owner.storage_name()])
            .expect("快照查询应可执行");
        let mut snapshot = Vec::new();
        while let Some(row) = rows.next().expect("快照行应可读取") {
            let values = (0..column_count)
                .map(|column| {
                    row.get::<_, RusqliteValue>(column)
                        .map(sqlite_value_from_rusqlite)
                        .expect("快照列应可读取")
                })
                .collect();
            snapshot.push(SqliteRow::new(values));
        }
        snapshot
    }

    fn sqlite_value_from_rusqlite(value: RusqliteValue) -> SqliteValue {
        match value {
            RusqliteValue::Null => SqliteValue::Null,
            RusqliteValue::Integer(value) => SqliteValue::Integer(value),
            RusqliteValue::Real(value) => SqliteValue::Real(value),
            RusqliteValue::Text(value) => SqliteValue::Text(value),
            RusqliteValue::Blob(value) => SqliteValue::Blob(value),
        }
    }

    fn plan_statements(plan: &SqliteTransactionPlan) -> Vec<String> {
        plan.steps()
            .iter()
            .map(|step| match step {
                SqliteTransactionStep::Execute(command) => command.statement().to_owned(),
                SqliteTransactionStep::ExecuteMany(batch)
                | SqliteTransactionStep::ExecuteManyExactlyOne(batch)
                | SqliteTransactionStep::RequireNoRowsMany(batch) => batch.statement().to_owned(),
                SqliteTransactionStep::RequireNoRows(query) => query.statement().to_owned(),
            })
            .collect()
    }

    fn create_current_schema(connection: &Connection) {
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE standard_project_definition (
                    definition_kind TEXT PRIMARY KEY,
                    canonical_json TEXT NOT NULL
                );
                INSERT INTO standard_project_definition VALUES (
                    'mv_dialogue_rules',
                    '{"rules":[]}'
                );
                CREATE TABLE standard_text_group (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    group_order INTEGER NOT NULL CHECK (group_order >= 0),
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location),
                    UNIQUE (owner, group_order),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE standard_text_unit (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    unit_role TEXT NOT NULL,
                    unit_order INTEGER NOT NULL CHECK (unit_order >= 0),
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, group_location, unit_role),
                    UNIQUE (owner, group_location, unit_order),
                    FOREIGN KEY (owner, group_location)
                        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE
                );
                CREATE TABLE standard_mutation_claim (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    resource_key TEXT NOT NULL,
                    access TEXT NOT NULL CHECK (access IN ('intent', 'exclusive')),
                    PRIMARY KEY (owner, group_location, resource_key),
                    FOREIGN KEY (owner, group_location)
                        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE
                );
                CREATE INDEX standard_mutation_claim_resource_idx
                    ON standard_mutation_claim(resource_key, access, owner, group_location);
                CREATE INDEX standard_mutation_claim_owner_resource_idx
                    ON standard_mutation_claim(owner, resource_key, access, group_location);
                "#,
            )
            .expect("当前测试 schema 应创建成功");
    }

    fn seed_snapshot(
        connection: &Connection,
        snapshot: &EncodedSnapshot,
        translation: &str,
        translation_state: &[u8; 32],
    ) {
        connection
            .execute(
                "INSERT INTO standard_asset_owner_state VALUES (?1, ?2, ?3)",
                (
                    snapshot.owner.storage_name(),
                    vec![0xa5; 32],
                    snapshot.fingerprint.as_bytes().to_vec(),
                ),
            )
            .expect("owner state 应写入");
        for group in &snapshot.groups {
            connection
                .execute(
                    "INSERT INTO standard_text_group VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        snapshot.owner.storage_name(),
                        &group.group_location,
                        i64::try_from(group.group_order).expect("测试顺序应可编码"),
                        group.group_kind,
                        &group.projection_recipe_json,
                    ),
                )
                .expect("组应写入");
        }
        for unit in &snapshot.units {
            connection
                .execute(
                    "INSERT INTO standard_text_unit VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        snapshot.owner.storage_name(),
                        &unit.group_location,
                        &unit.unit_role,
                        i64::try_from(unit.unit_order).expect("测试顺序应可编码"),
                        &unit.source_content_json,
                        &unit.source_context_json,
                        translation,
                        translation_state.to_vec(),
                    ],
                )
                .expect("单元应写入");
        }
        for claim in &snapshot.claims {
            connection
                .execute(
                    "INSERT INTO standard_mutation_claim VALUES (?1, ?2, ?3, ?4)",
                    (
                        snapshot.owner.storage_name(),
                        &claim.group_location,
                        &claim.resource_key,
                        claim.access.storage_name(),
                    ),
                )
                .expect("Claim 应写入");
        }
    }

    fn execute_plan(
        connection: &mut Connection,
        plan: SqliteTransactionPlan,
    ) -> Result<(), String> {
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        for step in plan.steps() {
            match step {
                SqliteTransactionStep::Execute(command) => {
                    transaction
                        .execute(
                            command.statement(),
                            params_from_iter(command.parameters().iter().map(to_rusqlite_value)),
                        )
                        .map_err(|error| error.to_string())?;
                }
                SqliteTransactionStep::ExecuteMany(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .map_err(|error| error.to_string())?;
                    for parameters in batch.parameter_sets() {
                        statement
                            .execute(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .map_err(|error| error.to_string())?;
                    }
                }
                SqliteTransactionStep::ExecuteManyExactlyOne(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .map_err(|error| error.to_string())?;
                    for parameters in batch.parameter_sets() {
                        let affected = statement
                            .execute(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .map_err(|error| error.to_string())?;
                        if affected != 1 {
                            return Err(format!(
                                "exactly-one requirement failed: affected {affected} rows"
                            ));
                        }
                    }
                }
                SqliteTransactionStep::RequireNoRows(query) => {
                    let mut statement = transaction
                        .prepare(query.statement())
                        .map_err(|error| error.to_string())?;
                    let mut rows = statement
                        .query(params_from_iter(
                            query.parameters().iter().map(to_rusqlite_value),
                        ))
                        .map_err(|error| error.to_string())?;
                    if rows.next().map_err(|error| error.to_string())?.is_some() {
                        return Err("requirement failed".to_owned());
                    }
                }
                SqliteTransactionStep::RequireNoRowsMany(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .map_err(|error| error.to_string())?;
                    for parameters in batch.parameter_sets() {
                        let mut rows = statement
                            .query(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .map_err(|error| error.to_string())?;
                        if rows.next().map_err(|error| error.to_string())?.is_some() {
                            return Err("requirement failed".to_owned());
                        }
                    }
                }
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    fn to_rusqlite_value(value: &SqliteValue) -> RusqliteValue {
        match value {
            SqliteValue::Null => RusqliteValue::Null,
            SqliteValue::Integer(value) => RusqliteValue::Integer(*value),
            SqliteValue::Real(value) => RusqliteValue::Real(*value),
            SqliteValue::Text(value) => RusqliteValue::Text(value.clone()),
            SqliteValue::Blob(value) => RusqliteValue::Blob(value.clone()),
        }
    }
}
