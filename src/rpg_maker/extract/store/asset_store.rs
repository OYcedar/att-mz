//! Builtin、Rules 与 Lua 标准文本资产的 SQLite 快照替换实现。

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
use crate::rpg_maker::model::TextFieldRole;
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
const DROP_STAGING_LEAF: &str = "DROP TABLE IF EXISTS temp.rpg_maker_staging_leaf";
const DROP_STAGING_TARGET: &str = "DROP TABLE IF EXISTS temp.rpg_maker_staging_target";
const DROP_PREVIOUS_LEAF: &str = "DROP TABLE IF EXISTS temp.rpg_maker_previous_leaf";

const CREATE_STAGING_GROUP: &str = r#"CREATE TEMP TABLE rpg_maker_staging_group (
    owner                  TEXT NOT NULL,
    group_location         TEXT NOT NULL,
    group_kind             TEXT NOT NULL,
    projection_recipe_json TEXT NOT NULL,
    PRIMARY KEY (owner, group_location)
)"#;

const CREATE_STAGING_LEAF: &str = r#"CREATE TEMP TABLE rpg_maker_staging_leaf (
    owner                    TEXT NOT NULL,
    group_location           TEXT NOT NULL,
    field_role               TEXT NOT NULL,
    original_text            TEXT NOT NULL,
    translation_context_json TEXT NOT NULL,
    translation              TEXT,
    translation_state        BLOB,
    PRIMARY KEY (owner, group_location, field_role)
)"#;

const CREATE_STAGING_TARGET: &str = r#"CREATE TEMP TABLE rpg_maker_staging_target (
    mutation_target TEXT NOT NULL PRIMARY KEY,
    owner            TEXT NOT NULL,
    group_location   TEXT NOT NULL
)"#;

const CREATE_PREVIOUS_LEAF: &str = r#"CREATE TEMP TABLE rpg_maker_previous_leaf AS
SELECT group_location,
       field_role,
       original_text,
       translation_context_json,
       translation,
       translation_state
FROM standard_text_leaf
WHERE owner = ?"#;

const INSERT_STAGING_GROUP: &str = r#"INSERT INTO rpg_maker_staging_group (
    owner, group_location, group_kind, projection_recipe_json
) VALUES (?, ?, ?, ?)"#;

const INSERT_STAGING_LEAF: &str = r#"INSERT INTO rpg_maker_staging_leaf (
    owner,
    group_location,
    field_role,
    original_text,
    translation_context_json
) VALUES (?, ?, ?, ?, ?)"#;

const INSERT_STAGING_TARGET: &str = r#"INSERT INTO rpg_maker_staging_target (
    mutation_target, owner, group_location
) VALUES (?, ?, ?)"#;

const FIND_MUTATION_TARGET_CONFLICT: &str = r#"SELECT staged.mutation_target
FROM rpg_maker_staging_target AS staged
JOIN standard_text_target AS current
  ON current.mutation_target = staged.mutation_target
WHERE current.owner <> staged.owner
LIMIT 1"#;

const INHERIT_TRANSLATIONS: &str = r#"UPDATE rpg_maker_staging_leaf
SET (translation, translation_state) = (
    SELECT previous.translation, previous.translation_state
    FROM rpg_maker_previous_leaf AS previous
    WHERE previous.group_location = rpg_maker_staging_leaf.group_location
      AND previous.field_role = rpg_maker_staging_leaf.field_role
      AND previous.original_text = rpg_maker_staging_leaf.original_text
      AND previous.translation_context_json = rpg_maker_staging_leaf.translation_context_json
    LIMIT 1
)"#;

const DELETE_OWNER_TARGETS: &str = "DELETE FROM standard_text_target WHERE owner = ?";
const DELETE_OWNER_LEAVES: &str = "DELETE FROM standard_text_leaf WHERE owner = ?";
const DELETE_OWNER_GROUPS: &str = "DELETE FROM standard_text_group WHERE owner = ?";

const UPSERT_OWNER_STATE: &str = r#"INSERT INTO standard_asset_owner_state (
    owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
) VALUES (?, ?, ?)
ON CONFLICT(owner) DO UPDATE SET
    source_snapshot_fingerprint = excluded.source_snapshot_fingerprint,
    asset_snapshot_fingerprint = excluded.asset_snapshot_fingerprint"#;

const INSERT_GROUPS: &str = r#"INSERT INTO standard_text_group (
    owner, group_location, group_kind, projection_recipe_json
)
SELECT owner, group_location, group_kind, projection_recipe_json
FROM rpg_maker_staging_group
ORDER BY group_location"#;

const INSERT_LEAVES: &str = r#"INSERT INTO standard_text_leaf (
    owner,
    group_location,
    field_role,
    original_text,
    translation_context_json,
    translation,
    translation_state
)
SELECT owner,
       group_location,
       field_role,
       original_text,
       translation_context_json,
       translation,
       translation_state
FROM rpg_maker_staging_leaf
ORDER BY group_location, field_role"#;

const INSERT_TARGETS: &str = r#"INSERT INTO standard_text_target (
    mutation_target, owner, group_location
)
SELECT mutation_target, owner, group_location
FROM rpg_maker_staging_target
ORDER BY mutation_target"#;

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
    group_kind,
    projection_recipe_json
FROM standard_text_group
WHERE owner = ?
ORDER BY group_location"#;

const READ_OWNER_LEAVES: &str = r#"SELECT
    group_location,
    field_role,
    original_text,
    translation_context_json
FROM standard_text_leaf
WHERE owner = ?
ORDER BY group_location, field_role"#;

const READ_OWNER_TARGETS: &str = r#"SELECT
    mutation_target,
    group_location
FROM standard_text_target INDEXED BY sqlite_autoindex_standard_text_target_1
WHERE owner = ?
ORDER BY mutation_target"#;

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
        let batches = self
            .cpu
            .execute(move || split_groups(groups, groups_per_encode_job))
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
                    SqliteQuery::new(READ_OWNER_LEAVES, vec![text(owner.storage_name())]),
                    SqliteQuery::new(READ_OWNER_TARGETS, vec![text(owner.storage_name())]),
                ],
            )
            .await
            .map_err(|error| map_query_error(database_path, error))?;
        let actual = query_results.len();
        let [owner_state, groups, leaves, targets] = query_results.try_into().map_err(|_| {
            RpgMakerExtractionAssetStoreError::UnexpectedSnapshotQueryResultCount {
                expected: 4,
                actual,
            }
        })?;
        Ok(StoredSnapshotRows {
            owner_state,
            groups,
            leaves,
            targets,
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
    MutationTargetConflict {
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
            Self::MutationTargetConflict { database_path } => write!(
                formatter,
                "标准资产 owner 拥有了同一物理修改目标：{}",
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
            | Self::MutationTargetConflict { .. } => None,
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
    TranslationContext(serde_json::Error),
    DuplicateGroupLocation { group_location: String },
}

impl fmt::Display for EncodeAssetSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => write!(formatter, "位置编码失败：{source}"),
            Self::Projection(source) => write!(formatter, "文本投影编码失败：{source}"),
            Self::TranslationContext(source) => write!(formatter, "翻译上下文编码失败：{source}"),
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
            Self::TranslationContext(source) => Some(source),
            Self::DuplicateGroupLocation { .. } => None,
        }
    }
}

#[derive(Default)]
struct EncodedBatch {
    groups: Vec<EncodedGroup>,
    leaves: Vec<EncodedLeaf>,
    targets: Vec<EncodedTarget>,
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
    leaves: Vec<EncodedLeaf>,
    targets: Vec<EncodedTarget>,
    fingerprint: AssetSnapshotFingerprint,
}

#[cfg_attr(test, derive(Clone, Debug, Default, PartialEq))]
struct StoredSnapshotRows {
    owner_state: Vec<SqliteRow>,
    groups: Vec<SqliteRow>,
    leaves: Vec<SqliteRow>,
    targets: Vec<SqliteRow>,
}

impl EncodedSnapshot {
    fn merge(
        owner: RpgMakerStandardAssetOwner,
        batches: Vec<EncodedBatch>,
        project_definition_json: Option<&str>,
    ) -> Result<Self, EncodeAssetSnapshotError> {
        let mut groups = Vec::new();
        let mut leaves = Vec::new();
        let mut targets = Vec::new();
        for batch in batches {
            groups.extend(batch.groups);
            leaves.extend(batch.leaves);
            targets.extend(batch.targets);
        }
        groups.sort_by(|left, right| left.group_location.cmp(&right.group_location));
        leaves.sort_by(|left, right| {
            left.group_location
                .cmp(&right.group_location)
                .then_with(|| left.field_role.cmp(&right.field_role))
        });
        targets.sort_by(|left, right| left.mutation_target.cmp(&right.mutation_target));

        if let Some(duplicate) = groups
            .windows(2)
            .find(|pair| pair[0].group_location == pair[1].group_location)
        {
            return Err(EncodeAssetSnapshotError::DuplicateGroupLocation {
                group_location: duplicate[0].group_location.clone(),
            });
        }

        let fingerprint =
            asset_snapshot_fingerprint(owner, project_definition_json, &groups, &leaves, &targets);
        Ok(Self {
            #[cfg(test)]
            owner,
            groups,
            leaves,
            targets,
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
            leaves,
            targets,
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

        let mut rows = leaves.into_iter();
        for leaf in &self.leaves {
            if !stored_leaf_row_matches(rows.next(), leaf) {
                return false;
            }
        }
        if rows.next().is_some() {
            return false;
        }

        let mut rows = targets.into_iter();
        for target in &self.targets {
            if !stored_target_row_matches(rows.next(), target) {
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
    matches!(
        row.values(),
        [
            SqliteValue::Text(group_location),
            SqliteValue::Text(group_kind),
            SqliteValue::Text(projection_recipe_json),
        ] if group_location == &expected.group_location
            && group_kind == expected.group_kind
            && projection_recipe_json == &expected.projection_recipe_json
    )
}

fn stored_leaf_row_matches(row: Option<SqliteRow>, expected: &EncodedLeaf) -> bool {
    let Some(row) = row else {
        return false;
    };
    matches!(
        row.values(),
        [
            SqliteValue::Text(group_location),
            SqliteValue::Text(field_role),
            SqliteValue::Text(original_text),
            SqliteValue::Text(translation_context_json),
        ] if group_location == &expected.group_location
            && field_role == &expected.field_role
            && original_text == &expected.original_text
            && translation_context_json == &expected.translation_context_json
    )
}

fn stored_target_row_matches(row: Option<SqliteRow>, expected: &EncodedTarget) -> bool {
    let Some(row) = row else {
        return false;
    };
    matches!(
        row.values(),
        [SqliteValue::Text(mutation_target), SqliteValue::Text(group_location)]
            if mutation_target == &expected.mutation_target
                && group_location == &expected.group_location
    )
}

struct EncodedGroup {
    group_location: String,
    group_kind: &'static str,
    projection_recipe_json: String,
}

struct EncodedLeaf {
    group_location: String,
    field_role: String,
    original_text: String,
    translation_context_json: String,
}

struct EncodedTarget {
    mutation_target: String,
    group_location: String,
}

#[derive(Serialize)]
struct DialogueBodyTranslationContext<'a> {
    source_speaker: &'a str,
}

fn split_groups(
    groups: Vec<ExtractedTextGroup>,
    groups_per_job: usize,
) -> Vec<Vec<ExtractedTextGroup>> {
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

fn encode_batch(groups: Vec<ExtractedTextGroup>) -> Result<EncodedBatch, EncodeAssetSnapshotError> {
    let mut encoded = EncodedBatch::default();
    for group in groups {
        let group_location = RpgMakerLocationCodec::encode(group.group_location())
            .map_err(EncodeAssetSnapshotError::Location)?;
        let source_speaker = group
            .fields()
            .iter()
            .find(|field| field.role() == &TextFieldRole::DialogueSpeaker)
            .map(|field| field.original_text());
        let dialogue_context = source_speaker
            .map(|source_speaker| {
                serde_json::to_string(&DialogueBodyTranslationContext { source_speaker })
                    .map_err(EncodeAssetSnapshotError::TranslationContext)
            })
            .transpose()?;

        for field in group.fields() {
            let translation_context_json =
                if matches!(field.role(), TextFieldRole::DialogueBody { .. }) {
                    dialogue_context.as_deref().unwrap_or("{}")
                } else {
                    "{}"
                };
            encoded.leaves.push(EncodedLeaf {
                group_location: group_location.clone(),
                field_role: RpgMakerProjectionCodec::encode_role(field.role())
                    .map_err(EncodeAssetSnapshotError::Projection)?,
                original_text: field.original_text().to_owned(),
                translation_context_json: translation_context_json.to_owned(),
            });
        }

        for target in group.mutation_targets() {
            encoded.targets.push(EncodedTarget {
                mutation_target: RpgMakerProjectionCodec::encode_target(target)
                    .map_err(EncodeAssetSnapshotError::Projection)?,
                group_location: group_location.clone(),
            });
        }

        encoded.groups.push(EncodedGroup {
            group_location,
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
    leaves: &[EncodedLeaf],
    targets: &[EncodedTarget],
) -> AssetSnapshotFingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
    hasher.frame(1, owner.storage_name().as_bytes());
    if let Some(project_definition_json) = project_definition_json {
        hasher
            .frame(14, b"project_definition")
            .frame(15, project_definition_json.as_bytes());
    }
    for group in groups {
        hasher
            .frame(2, b"group")
            .frame(3, group.group_location.as_bytes())
            .frame(4, group.group_kind.as_bytes())
            .frame(5, group.projection_recipe_json.as_bytes());
    }
    for leaf in leaves {
        hasher
            .frame(6, b"leaf")
            .frame(7, leaf.group_location.as_bytes())
            .frame(8, leaf.field_role.as_bytes())
            .frame(9, leaf.original_text.as_bytes())
            .frame(10, leaf.translation_context_json.as_bytes());
    }
    for target in targets {
        hasher
            .frame(11, b"target")
            .frame(12, target.mutation_target.as_bytes())
            .frame(13, target.group_location.as_bytes());
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
        leaves,
        targets,
        fingerprint,
        ..
    } = snapshot;
    let mut steps = Vec::new();
    for statement in [
        DROP_STAGING_GROUP,
        DROP_STAGING_LEAF,
        DROP_STAGING_TARGET,
        DROP_PREVIOUS_LEAF,
        CREATE_STAGING_GROUP,
        CREATE_STAGING_LEAF,
        CREATE_STAGING_TARGET,
    ] {
        steps.push(execute(statement, Vec::new()));
    }
    steps.push(execute(
        CREATE_PREVIOUS_LEAF,
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
                        text(group.group_kind),
                        text(group.projection_recipe_json),
                    ]
                })
                .collect(),
        )));
    }
    if !leaves.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING_LEAF,
            leaves
                .into_iter()
                .map(|leaf| {
                    vec![
                        text(owner.storage_name()),
                        text(leaf.group_location),
                        text(leaf.field_role),
                        text(leaf.original_text),
                        text(leaf.translation_context_json),
                    ]
                })
                .collect(),
        )));
    }
    if !targets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING_TARGET,
            targets
                .into_iter()
                .map(|target| {
                    vec![
                        text(target.mutation_target),
                        text(owner.storage_name()),
                        text(target.group_location),
                    ]
                })
                .collect(),
        )));
    }

    steps.push(SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        FIND_MUTATION_TARGET_CONFLICT,
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
    for statement in [
        DELETE_OWNER_TARGETS,
        DELETE_OWNER_LEAVES,
        DELETE_OWNER_GROUPS,
    ] {
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
    for statement in [INSERT_GROUPS, INSERT_LEAVES, INSERT_TARGETS] {
        steps.push(execute(statement, Vec::new()));
    }
    for statement in [
        DROP_STAGING_GROUP,
        DROP_STAGING_LEAF,
        DROP_STAGING_TARGET,
        DROP_PREVIOUS_LEAF,
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
            RpgMakerExtractionAssetStoreError::MutationTargetConflict { database_path }
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
        ExtractedTextField, RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource,
    };
    use crate::rpg_maker::model::{
        DirectTextPart, DirectTextRecipe, ScalarFieldKey, TextProjectionRecipe,
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
                READ_OWNER_LEAVES => Ok(snapshot.leaves.clone()),
                READ_OWNER_TARGETS => Ok(snapshot.targets.clone()),
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
        let encoded = encode_batch(vec![group]).expect("对话快照应可编码");

        assert_eq!(encoded.groups.len(), 1);
        assert_eq!(encoded.leaves.len(), 2);
        assert_eq!(encoded.targets.len(), 2);
        assert_eq!(encoded.groups[0].group_kind, "event_dialogue");
        RpgMakerProjectionCodec::decode_recipes(&encoded.groups[0].projection_recipe_json)
            .expect("配方必须是可逆的内部 canonical JSON");
        for target in &encoded.targets {
            RpgMakerProjectionCodec::decode_target(&target.mutation_target)
                .expect("修改目标必须是可逆的内部 canonical JSON");
        }
        for leaf in &encoded.leaves {
            RpgMakerProjectionCodec::decode_role(&leaf.field_role)
                .expect("角色必须是可逆的内部 canonical JSON");
        }

        let body = encoded
            .leaves
            .iter()
            .find(|leaf| {
                RpgMakerProjectionCodec::decode_role(&leaf.field_role)
                    .is_ok_and(|role| role == TextFieldRole::DialogueBody { index: 0 })
            })
            .expect("应存在正文叶");
        assert_eq!(
            body.translation_context_json,
            r#"{"source_speaker":"角色"}"#
        );
        let speaker = encoded
            .leaves
            .iter()
            .find(|leaf| {
                RpgMakerProjectionCodec::decode_role(&leaf.field_role)
                    .is_ok_and(|role| role == TextFieldRole::DialogueSpeaker)
            })
            .expect("应存在 Speaker 叶");
        assert_eq!(speaker.translation_context_json, "{}");
    }

    #[test]
    fn asset_fingerprint_covers_owner_groups_leaves_context_recipes_and_targets() {
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
            vec![encode_batch(vec![projected_group("<a>", "</a>")]).expect("测试快照应可编码")],
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
        assert!(INHERIT_TRANSLATIONS.contains("previous.field_role"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.original_text"));
        assert!(INHERIT_TRANSLATIONS.contains("previous.translation_context_json"));
        assert!(!INHERIT_TRANSLATIONS.contains("projection_recipe_json"));
        assert!(!INHERIT_TRANSLATIONS.contains("mutation_target"));
    }

    #[test]
    fn recipe_shell_change_inherits_translation_in_a_real_transaction() {
        let owner = RpgMakerStandardAssetOwner::Builtin;
        let old = EncodedSnapshot::merge(
            owner,
            vec![encode_batch(vec![projected_group("<a>", "</a>")]).expect("旧快照应可编码")],
            None,
        )
        .expect("旧快照应可合并");
        let new = EncodedSnapshot::merge(
            owner,
            vec![encode_batch(vec![projected_group("<b>", "</b>")]).expect("新快照应可编码")],
            None,
        )
        .expect("新快照应可合并");
        assert_ne!(old.fingerprint, new.fingerprint);

        let mut connection = Connection::open_in_memory().expect("应创建内存数据库");
        create_current_schema(&connection);
        seed_snapshot(&connection, &old, "译文", &[0x44; 32]);
        execute_plan(
            &mut connection,
            build_transaction_plan(owner, [0xa5; 32], new, None),
        )
        .expect("配方外壳变化应完成替换");

        let (translation, state): (String, Vec<u8>) = connection
            .query_row(
                "SELECT translation, translation_state FROM standard_text_leaf",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("继承后的叶应存在");
        assert_eq!(translation, "译文");
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
        assert!(statements.contains("standard_text_leaf"));
        assert!(statements.contains("standard_text_target"));
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
            vec![encode_batch(vec![group.clone()]).expect("快照应可编码")],
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
                READ_OWNER_LEAVES.to_owned(),
                READ_OWNER_TARGETS.to_owned(),
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
            vec![encode_batch(vec![group.clone()]).expect("快照应可编码")],
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
                READ_OWNER_LEAVES.to_owned(),
                READ_OWNER_TARGETS.to_owned(),
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
            vec![encode_batch(vec![group.clone()]).expect("快照应可编码")],
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
                READ_OWNER_LEAVES.to_owned(),
                READ_OWNER_TARGETS.to_owned(),
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
            RpgMakerExtractionAssetStoreError::MutationTargetConflict { .. }
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
        assert_eq!(requirement.statement(), FIND_MUTATION_TARGET_CONFLICT);
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
                encode_batch(vec![
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
                encode_batch(vec![
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

        for column in 0..3 {
            let mut damaged = current.clone();
            let mut values = damaged.groups[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.groups[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }
        for column in 0..4 {
            let mut damaged = current.clone();
            let mut values = damaged.leaves[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.leaves[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }
        for column in 0..2 {
            let mut damaged = current.clone();
            let mut values = damaged.targets[0].values().to_vec();
            values[column] = SqliteValue::Null;
            damaged.targets[0] = SqliteRow::new(values);
            assert!(!snapshot.matches_rows(damaged, &[0xa5; 32]));
        }

        for table in 0..3 {
            let mut missing = current.clone();
            match table {
                0 => {
                    missing.groups.pop();
                }
                1 => {
                    missing.leaves.pop();
                }
                2 => {
                    missing.targets.pop();
                }
                _ => unreachable!("测试表编号固定为 0..3"),
            }
            assert!(!snapshot.matches_rows(missing, &[0xa5; 32]));

            let mut extra = current.clone();
            match table {
                0 => extra.groups.push(current.groups[0].clone()),
                1 => extra.leaves.push(current.leaves[0].clone()),
                2 => extra.targets.push(current.targets[0].clone()),
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
            READ_OWNER_LEAVES,
            READ_OWNER_TARGETS,
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
                details.iter().any(|detail| detail.contains("USING INDEX")),
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
            vec![encode_batch(vec![group]).expect("测试快照应可编码")],
            None,
        )
        .expect("测试快照应可合并")
        .fingerprint
    }

    fn scalar_group(index: usize, field_name: &str, original_text: &str) -> ExtractedTextGroup {
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
                ExtractedTextField::new(field_name, physical_location, original_text)
                    .expect("标量字段应合法"),
            ],
        )
        .expect("标量组应合法")
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
        let role = TextFieldRole::Scalar(ScalarFieldKey::new("match[0]").expect("角色应合法"));
        let field = ExtractedTextField::projected(role.clone(), target.clone(), "原文")
            .expect("投影叶应合法");
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
            vec![field],
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
        ExtractedTextGroup::new(
            TextGroupKind::EventDialogue,
            group_location,
            vec![
                ExtractedTextField::new("speaker", speaker_location, speaker)
                    .expect("Speaker 应合法"),
                ExtractedTextField::new("body[0]", body_location, body).expect("Body 应合法"),
            ],
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
                    text(group.group_kind),
                    text(group.projection_recipe_json.clone()),
                ])
            })
            .collect();
        let leaves = snapshot
            .leaves
            .iter()
            .map(|leaf| {
                SqliteRow::new(vec![
                    text(leaf.group_location.clone()),
                    text(leaf.field_role.clone()),
                    text(leaf.original_text.clone()),
                    text(leaf.translation_context_json.clone()),
                ])
            })
            .collect();
        let targets = snapshot
            .targets
            .iter()
            .map(|target| {
                SqliteRow::new(vec![
                    text(target.mutation_target.clone()),
                    text(target.group_location.clone()),
                ])
            })
            .collect();
        StoredSnapshotRows {
            owner_state: owner_state_rows(&[0xa5; 32], snapshot.fingerprint.as_bytes()),
            groups,
            leaves,
            targets,
        }
    }

    fn read_snapshot_rows(
        connection: &Connection,
        owner: RpgMakerStandardAssetOwner,
    ) -> StoredSnapshotRows {
        StoredSnapshotRows {
            owner_state: read_rows(connection, READ_OWNER_STATE, owner, 2),
            groups: read_rows(connection, READ_OWNER_GROUPS, owner, 3),
            leaves: read_rows(connection, READ_OWNER_LEAVES, owner, 4),
            targets: read_rows(connection, READ_OWNER_TARGETS, owner, 2),
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
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE standard_text_leaf (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_role TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation_context_json TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, group_location, field_role),
                    FOREIGN KEY (owner, group_location)
                        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE
                );
                CREATE TABLE standard_text_target (
                    mutation_target TEXT PRIMARY KEY,
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    FOREIGN KEY (owner, group_location)
                        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE
                );
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
                    "INSERT INTO standard_text_group VALUES (?1, ?2, ?3, ?4)",
                    (
                        snapshot.owner.storage_name(),
                        &group.group_location,
                        group.group_kind,
                        &group.projection_recipe_json,
                    ),
                )
                .expect("组应写入");
        }
        for leaf in &snapshot.leaves {
            connection
                .execute(
                    "INSERT INTO standard_text_leaf VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    (
                        snapshot.owner.storage_name(),
                        &leaf.group_location,
                        &leaf.field_role,
                        &leaf.original_text,
                        &leaf.translation_context_json,
                        translation,
                        translation_state.to_vec(),
                    ),
                )
                .expect("叶应写入");
        }
        for target in &snapshot.targets {
            connection
                .execute(
                    "INSERT INTO standard_text_target VALUES (?1, ?2, ?3)",
                    (
                        &target.mutation_target,
                        snapshot.owner.storage_name(),
                        &target.group_location,
                    ),
                )
                .expect("目标应写入");
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
