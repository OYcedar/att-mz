//! Builtin 与 Rules 标准提取资产的 SQLite 快照替换实现。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::project::OpenedProject;
use crate::att_mz::standard_asset::{
    MzStandardAssetOwner, MzStandardAssetStorageKind, MzStandardAssetTable,
};
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteBatch, SqliteCommand, SqliteQuery,
    SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};

use super::super::model::{BuiltinSnapshot, ExtractedTextGroup, LuaSnapshot, RulesSnapshot};
use super::{BuiltinSnapshotStore, LuaSnapshotStore, RulesSnapshotStore};
use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};

const DROP_STAGING_TABLE: &str = "DROP TABLE IF EXISTS temp.att_mz_extraction_staging";
const DROP_PREVIOUS_TABLE: &str = "DROP TABLE IF EXISTS temp.att_mz_extraction_previous";

const CREATE_STAGING_TABLE: &str = r#"CREATE TEMP TABLE att_mz_extraction_staging (
    target_table   TEXT NOT NULL,
    exact_location TEXT NOT NULL,
    owner          TEXT NOT NULL,
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    unit_type      TEXT,
    translation    TEXT,
    translation_state BLOB,
    PRIMARY KEY (target_table, exact_location)
)"#;

const CREATE_PREVIOUS_TABLE: &str = r#"CREATE TEMP TABLE att_mz_extraction_previous AS
SELECT 'entry' AS target_table, exact_location, owner, field_name, original_text,
       NULL AS unit_type, translation, translation_state FROM entry WHERE owner = ?
UNION ALL
SELECT 'system_text', exact_location, owner, field_name, original_text,
       NULL, translation, translation_state FROM system_text WHERE owner = ?
UNION ALL
SELECT 'map_text', exact_location, owner, field_name, original_text,
       NULL, translation, translation_state FROM map_text WHERE owner = ?
UNION ALL
SELECT 'text_body', exact_location, owner, field_name, original_text,
       unit_type, translation, translation_state FROM text_body WHERE owner = ?
UNION ALL
SELECT 'plugin_param', exact_location, owner, field_name, original_text,
       NULL, translation, translation_state FROM plugin_param WHERE owner = ?"#;

const INSERT_STAGING: &str = r#"INSERT INTO att_mz_extraction_staging (
    target_table,
    exact_location,
    owner,
    group_location,
    field_name,
    original_text,
    unit_type
) VALUES (?, ?, ?, ?, ?, ?, ?)"#;

const FIND_OWNER_CONFLICT: &str = r#"SELECT 1
FROM att_mz_extraction_staging AS staged
JOIN (
    SELECT owner, exact_location FROM entry
    UNION ALL SELECT owner, exact_location FROM system_text
    UNION ALL SELECT owner, exact_location FROM map_text
    UNION ALL SELECT owner, exact_location FROM text_body
    UNION ALL SELECT owner, exact_location FROM plugin_param
) AS previous ON previous.exact_location = staged.exact_location
JOIN standard_asset_owner_state AS state ON state.owner = previous.owner
JOIN metadata ON metadata.source_snapshot_fingerprint = state.source_snapshot_fingerprint
WHERE previous.owner <> staged.owner
LIMIT 1"#;

const INHERIT_TRANSLATIONS: &str = r#"UPDATE att_mz_extraction_staging
SET (translation, translation_state) = (
    SELECT previous.translation, previous.translation_state
    FROM att_mz_extraction_previous AS previous
    WHERE previous.target_table = att_mz_extraction_staging.target_table
      AND previous.exact_location = att_mz_extraction_staging.exact_location
      AND previous.owner = att_mz_extraction_staging.owner
      AND previous.field_name = att_mz_extraction_staging.field_name
      AND previous.original_text = att_mz_extraction_staging.original_text
      AND previous.unit_type IS att_mz_extraction_staging.unit_type
    LIMIT 1
)"#;

const DELETE_OWNER_FROM_TABLES: [&str; 5] = [
    "DELETE FROM entry WHERE owner = ?",
    "DELETE FROM system_text WHERE owner = ?",
    "DELETE FROM map_text WHERE owner = ?",
    "DELETE FROM text_body WHERE owner = ?",
    "DELETE FROM plugin_param WHERE owner = ?",
];

const UPSERT_OWNER_STATE: &str = r#"INSERT INTO standard_asset_owner_state (
    owner, source_snapshot_fingerprint
) VALUES (?, ?)
ON CONFLICT(owner) DO UPDATE SET
    source_snapshot_fingerprint = excluded.source_snapshot_fingerprint"#;

const DEACTIVATE_OWNER: &str = "DELETE FROM standard_asset_owner_state WHERE owner = ?";

const READ_OWNER_SNAPSHOT: &str = r#"SELECT
    'owner', '', '', '', '', '', NULL, source_snapshot_fingerprint
FROM standard_asset_owner_state
WHERE owner = ?
UNION ALL
SELECT 'asset', 'entry', exact_location, group_location, field_name, original_text, NULL, NULL
FROM entry WHERE owner = ?
UNION ALL
SELECT 'asset', 'system_text', exact_location, group_location, field_name, original_text, NULL, NULL
FROM system_text WHERE owner = ?
UNION ALL
SELECT 'asset', 'map_text', exact_location, group_location, field_name, original_text, NULL, NULL
FROM map_text WHERE owner = ?
UNION ALL
SELECT 'asset', 'text_body', exact_location, group_location, field_name, original_text, unit_type, NULL
FROM text_body WHERE owner = ?
UNION ALL
SELECT 'asset', 'plugin_param', exact_location, group_location, field_name, original_text, NULL, NULL
FROM plugin_param WHERE owner = ?
ORDER BY 1 DESC, 2, 3"#;

const INSERT_ENTRY: &str = r#"INSERT INTO entry (
    owner, exact_location, group_location, field_name, original_text, translation, translation_state
)
SELECT owner, exact_location, group_location, field_name, original_text, translation, translation_state
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_SYSTEM_TEXT: &str = r#"INSERT INTO system_text (
    owner, exact_location, group_location, field_name, original_text, translation, translation_state
)
SELECT owner, exact_location, group_location, field_name, original_text, translation, translation_state
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_MAP_TEXT: &str = r#"INSERT INTO map_text (
    owner, exact_location, group_location, field_name, original_text, translation, translation_state
)
SELECT owner, exact_location, group_location, field_name, original_text, translation, translation_state
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_TEXT_BODY: &str = r#"INSERT INTO text_body (
    owner, exact_location, group_location, field_name, unit_type, original_text, translation, translation_state
)
SELECT owner, exact_location, group_location, field_name, unit_type, original_text, translation, translation_state
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_PLUGIN_PARAM: &str = r#"INSERT INTO plugin_param (
    owner, exact_location, group_location, field_name, original_text, translation, translation_state
)
SELECT owner, exact_location, group_location, field_name, original_text, translation, translation_state
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

/// 标准提取资产编码阶段的必填资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MzExtractionAssetStoreConfig {
    encode_concurrency: NonZeroUsize,
    groups_per_encode_job: NonZeroUsize,
}

impl MzExtractionAssetStoreConfig {
    pub(crate) const fn new(
        encode_concurrency: NonZeroUsize,
        groups_per_encode_job: NonZeroUsize,
    ) -> Self {
        Self {
            encode_concurrency,
            groups_per_encode_job,
        }
    }

    #[cfg(test)]
    pub(crate) const fn encode_concurrency(self) -> NonZeroUsize {
        self.encode_concurrency
    }

    #[cfg(test)]
    pub(crate) const fn groups_per_encode_job(self) -> NonZeroUsize {
        self.groups_per_encode_job
    }
}

/// 使用纯 CPU 编码与单个 SQLite 事务替换 MZ 标准资产。
pub(crate) struct MzExtractionAssetStore<S, C> {
    sqlite: S,
    cpu: C,
    config: MzExtractionAssetStoreConfig,
}

impl<S, C> MzExtractionAssetStore<S, C> {
    pub(crate) fn new(sqlite: S, cpu: C, config: MzExtractionAssetStoreConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<S, C> MzExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    async fn replace(
        &self,
        project: &OpenedProject,
        owner: MzStandardAssetOwner,
        groups: Vec<ExtractedTextGroup>,
    ) -> Result<(), MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>> {
        let batches = split_groups(groups, self.config.groups_per_encode_job.get());
        let parameter_batches = stream::iter(batches.into_iter().map(|batch| async move {
            self.cpu
                .execute(move || encode_batch(batch, owner))
                .await
                .map_err(MzExtractionAssetStoreError::ScheduleEncoding)?
                .map_err(MzExtractionAssetStoreError::EncodeLocation)
        }))
        .buffered(self.config.encode_concurrency.get())
        .try_collect::<Vec<_>>()
        .await?;

        let mut parameter_sets = parameter_batches.into_iter().flatten().collect::<Vec<_>>();
        sort_parameter_sets(&mut parameter_sets);
        let database_path = project.database_path().to_path_buf();
        let current = self
            .read_owner_snapshot(database_path.clone(), owner)
            .await?;
        let desired = desired_snapshot_rows(
            project.source_snapshot_fingerprint().as_bytes(),
            &parameter_sets,
        );
        if current == desired {
            return Ok(());
        }

        let plan = build_transaction_plan(
            owner,
            project.source_snapshot_fingerprint().as_bytes(),
            parameter_sets,
        );

        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_persist_error(database_path, error))?;
        Ok(())
    }

    async fn deactivate(
        &self,
        project: &OpenedProject,
        owner: MzStandardAssetOwner,
    ) -> Result<(), MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>> {
        let database_path = project.database_path().to_path_buf();
        if self
            .read_owner_snapshot(database_path.clone(), owner)
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

    async fn read_owner_snapshot(
        &self,
        database_path: PathBuf,
        owner: MzStandardAssetOwner,
    ) -> Result<
        Vec<SqliteRow>,
        MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>,
    > {
        let owner = text(owner.storage_name());
        self.sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_OWNER_SNAPSHOT, vec![owner; 6]),
            )
            .await
            .map_err(|error| map_query_error(database_path, error))
    }
}

impl<S, C> BuiltinSnapshotStore for MzExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_builtin(
        &self,
        project: &OpenedProject,
        snapshot: BuiltinSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(
            project,
            MzStandardAssetOwner::Builtin,
            snapshot.into_groups(),
        )
        .await
    }
}

impl<S, C> RulesSnapshotStore for MzExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_rules(
        &self,
        project: &OpenedProject,
        snapshot: RulesSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(project, MzStandardAssetOwner::Rules, snapshot.into_groups())
            .await
    }

    async fn deactivate_rules(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        self.deactivate(project, MzStandardAssetOwner::Rules).await
    }
}

impl<S, C> LuaSnapshotStore for MzExtractionAssetStore<S, C>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
    C: CpuTaskExecutor,
{
    type Error = MzExtractionAssetStoreError<C::Error, <S as SqliteQueryExecutor>::Error>;

    async fn replace_lua(
        &self,
        project: &OpenedProject,
        snapshot: LuaSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(project, MzStandardAssetOwner::Lua, snapshot.into_groups())
            .await
    }

    async fn deactivate_lua(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        self.deactivate(project, MzStandardAssetOwner::Lua).await
    }
}

/// 标准提取快照替换的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzExtractionAssetStoreError<C, S> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    EncodeLocation(MzLocationCodecError),
    DatabaseNotFound { database_path: PathBuf },
    ReadCurrentState { database_path: PathBuf, source: S },
    OwnershipConflict { database_path: PathBuf },
    NotCommitted { database_path: PathBuf, source: S },
    OutcomeUnknown { database_path: PathBuf, source: S },
}

impl<C, S> fmt::Display for MzExtractionAssetStoreError<C, S>
where
    C: fmt::Display,
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleEncoding(source) => write!(formatter, "资产编码任务执行失败：{source}"),
            Self::EncodeLocation(source) => write!(formatter, "资产位置编码失败：{source}"),
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
            Self::OwnershipConflict { database_path } => write!(
                formatter,
                "当前来源下的新鲜标准资产 owner 拥有了同一文本位置：{}",
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

impl<C, S> Error for MzExtractionAssetStoreError<C, S>
where
    C: Error + 'static,
    S: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleEncoding(source) => Some(source),
            Self::EncodeLocation(source) => Some(source),
            Self::ReadCurrentState { source, .. } => Some(source),
            Self::NotCommitted { source, .. } | Self::OutcomeUnknown { source, .. } => Some(source),
            Self::DatabaseNotFound { .. } | Self::OwnershipConflict { .. } => None,
        }
    }
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

fn encode_batch(
    groups: Vec<ExtractedTextGroup>,
    owner: MzStandardAssetOwner,
) -> Result<Vec<Vec<SqliteValue>>, MzLocationCodecError> {
    let capacity = groups.iter().map(|group| group.fields().len()).sum();
    let mut parameter_sets = Vec::with_capacity(capacity);

    for group in groups {
        let storage = MzStandardAssetStorageKind::for_group_kind(group.kind());
        let group_location = MzLocationCodec::encode(group.group_location())?;
        for field in group.fields() {
            parameter_sets.push(vec![
                text(storage.table().storage_name()),
                text(MzLocationCodec::encode(field.exact_location())?),
                text(owner.storage_name()),
                text(group_location.clone()),
                text(field.field_name()),
                text(field.original_text()),
                storage
                    .unit_type()
                    .map_or(SqliteValue::Null, |unit| text(unit.storage_name())),
            ]);
        }
    }

    Ok(parameter_sets)
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn sqlite_text(value: &SqliteValue) -> &str {
    let SqliteValue::Text(value) = value else {
        unreachable!("内部编码的表名与位置必须是 TEXT")
    };
    value
}

fn encoded_asset_table(table: &str) -> MzStandardAssetTable {
    MzStandardAssetTable::from_storage_name(table).expect("内部编码只能产生当前五张标准资产表")
}

fn sort_parameter_sets(parameter_sets: &mut [Vec<SqliteValue>]) {
    parameter_sets.sort_by(|left, right| {
        encoded_asset_table(sqlite_text(&left[0]))
            .cmp(&encoded_asset_table(sqlite_text(&right[0])))
            .then_with(|| sqlite_text(&left[1]).cmp(sqlite_text(&right[1])))
    });
}

fn desired_snapshot_rows(
    fingerprint: &[u8; 32],
    parameter_sets: &[Vec<SqliteValue>],
) -> Vec<SqliteRow> {
    let mut rows = Vec::with_capacity(parameter_sets.len() + 1);
    rows.push(SqliteRow::new(vec![
        text("owner"),
        text(""),
        text(""),
        text(""),
        text(""),
        text(""),
        SqliteValue::Null,
        SqliteValue::Blob(fingerprint.to_vec()),
    ]));
    rows.extend(parameter_sets.iter().map(|parameters| {
        SqliteRow::new(vec![
            text("asset"),
            parameters[0].clone(),
            parameters[1].clone(),
            parameters[3].clone(),
            parameters[4].clone(),
            parameters[5].clone(),
            parameters[6].clone(),
            SqliteValue::Null,
        ])
    }));
    rows
}

fn build_transaction_plan(
    owner: MzStandardAssetOwner,
    source_snapshot_fingerprint: &[u8; 32],
    parameter_sets: Vec<Vec<SqliteValue>>,
) -> SqliteTransactionPlan {
    let mut steps = Vec::with_capacity(20);
    for statement in [
        DROP_STAGING_TABLE,
        DROP_PREVIOUS_TABLE,
        CREATE_STAGING_TABLE,
    ] {
        steps.push(execute(statement, Vec::new()));
    }
    steps.push(execute(
        CREATE_PREVIOUS_TABLE,
        vec![text(owner.storage_name()); 5],
    ));

    if !parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING,
            parameter_sets,
        )));
    }

    steps.push(SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        FIND_OWNER_CONFLICT,
        Vec::new(),
    )));
    steps.push(execute(INHERIT_TRANSLATIONS, Vec::new()));

    for statement in DELETE_OWNER_FROM_TABLES {
        steps.push(execute(statement, vec![text(owner.storage_name())]));
    }
    steps.push(execute(
        UPSERT_OWNER_STATE,
        vec![
            text(owner.storage_name()),
            SqliteValue::Blob(source_snapshot_fingerprint.to_vec()),
        ],
    ));

    for (statement, table) in [
        (INSERT_ENTRY, MzStandardAssetTable::Entry),
        (INSERT_SYSTEM_TEXT, MzStandardAssetTable::SystemText),
        (INSERT_MAP_TEXT, MzStandardAssetTable::MapText),
        (INSERT_TEXT_BODY, MzStandardAssetTable::TextBody),
        (INSERT_PLUGIN_PARAM, MzStandardAssetTable::PluginParam),
    ] {
        steps.push(execute(statement, vec![text(table.storage_name())]));
    }

    steps.push(execute(DROP_STAGING_TABLE, Vec::new()));
    steps.push(execute(DROP_PREVIOUS_TABLE, Vec::new()));
    SqliteTransactionPlan::new(steps)
}

fn execute(statement: &str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn map_persist_error<C, S>(
    database_path: PathBuf,
    error: ExecuteTransactionError<S>,
) -> MzExtractionAssetStoreError<C, S> {
    match error {
        ExecuteTransactionError::NotFound => {
            MzExtractionAssetStoreError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed => {
            MzExtractionAssetStoreError::OwnershipConflict { database_path }
        }
        ExecuteTransactionError::NotCommitted(source) => {
            MzExtractionAssetStoreError::NotCommitted {
                database_path,
                source,
            }
        }
        ExecuteTransactionError::OutcomeUnknown(source) => {
            MzExtractionAssetStoreError::OutcomeUnknown {
                database_path,
                source,
            }
        }
    }
}

fn map_query_error<C, S>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<S>,
) -> MzExtractionAssetStoreError<C, S> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            MzExtractionAssetStoreError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            MzExtractionAssetStoreError::ReadCurrentState {
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

    use crate::att_mz::ProjectName;
    use crate::att_mz::extract::document::StandardDataFile;
    use crate::att_mz::extract::model::{ExtractedTextField, MzLocation, MzLocationStep, MzSource};
    use crate::att_mz::text::TextGroupKind;
    use rusqlite::types::Value as RusqliteValue;
    use rusqlite::{Connection, params_from_iter};

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
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone)]
    struct RecordingSqlite {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        response: Arc<Mutex<Option<SqliteResponse>>>,
        query_calls: Arc<AtomicUsize>,
        current_snapshot: Arc<Mutex<Vec<SqliteRow>>>,
    }

    impl SqliteTransactionExecutor for RecordingSqlite {
        type Error = FakeError;

        async fn execute_transaction(
            &self,
            path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> Result<(), ExecuteTransactionError<Self::Error>> {
            self.calls
                .lock()
                .expect("SQLite 调用锁不应中毒")
                .push((path, plan));
            match self.response.lock().expect("SQLite 响应锁不应中毒").take() {
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

    impl SqliteQueryExecutor for RecordingSqlite {
        type Error = FakeError;

        async fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
            assert_eq!(path, PathBuf::from("C:/projects/demo/project.db"));
            assert_eq!(query.statement(), READ_OWNER_SNAPSHOT);
            assert_eq!(query.parameters().len(), 6);
            assert!(query.parameters().windows(2).all(|pair| pair[0] == pair[1]));
            self.query_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .current_snapshot
                .lock()
                .expect("当前快照锁不应中毒")
                .clone())
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

    #[test]
    fn config_exposes_only_explicit_non_zero_values() {
        let config = MzExtractionAssetStoreConfig::new(non_zero(3), non_zero(20));

        assert_eq!(config.encode_concurrency().get(), 3);
        assert_eq!(config.groups_per_encode_job().get(), 20);
    }

    #[tokio::test]
    async fn maps_all_group_kinds_to_five_domain_tables_with_bounded_cpu_jobs() {
        let harness = Harness::new(None);
        let service = harness.service(2, 1);

        service
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("快照应该成功编码");

        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 8);
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 2);
        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("C:/projects/demo/project.db"));

        let parameter_sets = staging_parameter_sets(&calls[0].1);
        assert_eq!(parameter_sets.len(), 8);
        let table_and_unit = parameter_sets
            .iter()
            .map(|parameters| (&parameters[0], &parameters[6]))
            .collect::<Vec<_>>();
        assert_eq!(
            table_and_unit,
            vec![
                (&text("entry"), &SqliteValue::Null),
                (&text("system_text"), &SqliteValue::Null),
                (&text("map_text"), &SqliteValue::Null),
                (&text("text_body"), &text("dialogue")),
                (&text("text_body"), &text("choices")),
                (&text("text_body"), &text("scrolling_text")),
                (&text("text_body"), &text("event_command")),
                (&text("plugin_param"), &SqliteValue::Null),
            ]
        );
        assert!(
            parameter_sets
                .iter()
                .all(|parameters| parameters[2] == text("builtin"))
        );
    }

    #[tokio::test]
    async fn compound_fields_share_group_location_but_keep_independent_exact_locations() {
        let harness = Harness::new(None);
        let service = harness.service(1, 10);
        let source = MzSource::data(StandardDataFile::Items);
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(10)]);
        let group = ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![
                field(source.clone(), 10, "description", "锋利的宝剑"),
                field(source, 10, "name", "宝剑"),
            ],
        )
        .expect("复合字段应该合法");

        service
            .replace_rules(
                &project(),
                RulesSnapshot::new(vec![group]).expect("快照应该合法"),
            )
            .await
            .expect("快照应该成功编码");

        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        let rows = staging_parameter_sets(&calls[0].1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][3], rows[1][3]);
        assert_ne!(rows[0][1], rows[1][1]);
        assert!(rows.iter().all(|row| row[2] == text("rules")));
    }

    #[tokio::test]
    async fn ownership_check_precedes_all_owner_deletes() {
        let harness = Harness::new(None);
        let service = harness.service(1, 10);

        service
            .replace_rules(&project(), RulesSnapshot::empty())
            .await
            .expect("空 Rules 快照应该清除旧叶子");

        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 0);
        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        let steps = calls[0].1.steps();
        assert!(
            !steps
                .iter()
                .any(|step| matches!(step, SqliteTransactionStep::ExecuteMany(_)))
        );
        let check_index = steps
            .iter()
            .position(|step| matches!(step, SqliteTransactionStep::RequireNoRows(_)))
            .expect("事务必须检查所有权冲突");
        let delete_index = steps
            .iter()
            .position(|step| {
                matches!(step, SqliteTransactionStep::Execute(command) if command.statement().starts_with("DELETE FROM"))
            })
            .expect("空快照仍必须删除当前 owner 的旧叶子");
        assert!(check_index < delete_index);
        for command in steps.iter().filter_map(|step| match step {
            SqliteTransactionStep::Execute(command)
                if command.statement().starts_with("DELETE FROM") =>
            {
                Some(command)
            }
            _ => None,
        }) {
            assert_eq!(command.parameters(), &[text("rules")]);
        }
    }

    #[tokio::test]
    async fn store_uses_init_schema_and_inherits_translation_state_by_exact_leaf_semantics() {
        let harness = Harness::new(None);

        harness
            .service(1, 1)
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("快照应该成功编码");

        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        let steps = calls[0].1.steps();
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement() == INHERIT_TRANSLATIONS
                        && command.statement().contains("translation_state")
                        && command.statement().contains("previous.target_table")
                        && command.statement().contains("previous.exact_location")
                        && command.statement().contains("previous.field_name")
                        && command.statement().contains("previous.original_text")
                        && command.statement().contains("previous.unit_type IS")
            )
        }));
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement() == UPSERT_OWNER_STATE
                        && command.parameters()[0] == text("builtin")
                        && matches!(&command.parameters()[1], SqliteValue::Blob(bytes) if bytes.len() == 32)
            )
        }));
        assert!(
            !INHERIT_TRANSLATIONS.contains("group_location"),
            "group 变化不得扩大逐叶译文失效范围"
        );
        assert!(FIND_OWNER_CONFLICT.contains("standard_asset_owner_state"));
        assert!(
            FIND_OWNER_CONFLICT.contains(
                "metadata.source_snapshot_fingerprint = state.source_snapshot_fingerprint"
            )
        );
    }

    #[tokio::test]
    async fn identical_owner_snapshot_returns_unchanged_without_a_write_transaction() {
        let harness = Harness::new(None);
        let project = project();
        let mut parameters = encode_batch(all_kind_groups(), MzStandardAssetOwner::Builtin)
            .expect("测试快照位置应该可编码");
        sort_parameter_sets(&mut parameters);
        *harness.current_snapshot.lock().expect("当前快照锁不应中毒") = desired_snapshot_rows(
            project.source_snapshot_fingerprint().as_bytes(),
            &parameters,
        );

        harness
            .service(3, 2)
            .replace_builtin(&project, BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("完全相同的 owner 快照应该正常收敛");

        assert_eq!(harness.sqlite_query_calls.load(Ordering::SeqCst), 1);
        assert!(
            harness
                .sqlite_calls
                .lock()
                .expect("SQLite 调用锁不应中毒")
                .is_empty(),
            "完全相同的快照不得删除重插或更新 owner state"
        );
    }

    #[tokio::test]
    async fn active_empty_rules_snapshot_and_deactivated_rules_are_distinct_states() {
        let harness = Harness::new(None);
        let project = project();

        harness
            .service(1, 1)
            .replace_rules(&project, RulesSnapshot::empty())
            .await
            .expect("非空定义生成的空快照应可激活 Rules owner");
        {
            let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
            assert_eq!(calls.len(), 1);
            assert!(calls[0].1.steps().iter().any(|step| {
                matches!(
                    step,
                    SqliteTransactionStep::Execute(command)
                        if command.statement() == UPSERT_OWNER_STATE
                            && command.parameters()[0] == text("rules")
                )
            }));
        }

        harness
            .sqlite_calls
            .lock()
            .expect("SQLite 调用锁不应中毒")
            .clear();
        *harness.current_snapshot.lock().expect("当前快照锁不应中毒") =
            desired_snapshot_rows(project.source_snapshot_fingerprint().as_bytes(), &[]);
        harness
            .service(1, 1)
            .replace_rules(&project, RulesSnapshot::empty())
            .await
            .expect("active 空快照重复提交应可收敛");
        assert!(
            harness
                .sqlite_calls
                .lock()
                .expect("SQLite 调用锁不应中毒")
                .is_empty()
        );

        harness
            .service(1, 1)
            .deactivate_rules(&project)
            .await
            .expect("停用 active 空快照应该删除 owner state");
        {
            let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].1.steps().len(), 1);
            let SqliteTransactionStep::Execute(command) = &calls[0].1.steps()[0] else {
                panic!("停用 Rules 应该只删除 owner state")
            };
            assert_eq!(command.statement(), DEACTIVATE_OWNER);
            assert_eq!(command.parameters(), &[text("rules")]);
        }
        harness
            .sqlite_calls
            .lock()
            .expect("SQLite 调用锁不应中毒")
            .clear();
        harness
            .current_snapshot
            .lock()
            .expect("当前快照锁不应中毒")
            .clear();
        harness
            .service(1, 1)
            .deactivate_rules(&project)
            .await
            .expect("已停用 Rules 重复收敛应成功");
        assert!(
            harness
                .sqlite_calls
                .lock()
                .expect("SQLite 调用锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn lua_store_uses_lua_owner_for_active_empty_and_deactivation() {
        let harness = Harness::new(None);
        let project = project();

        harness
            .service(1, 1)
            .replace_lua(&project, LuaSnapshot::empty())
            .await
            .expect("replace_standard({}) 应建立 active Lua owner");
        {
            let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
            assert!(calls[0].1.steps().iter().any(|step| {
                matches!(
                    step,
                    SqliteTransactionStep::Execute(command)
                        if command.statement() == UPSERT_OWNER_STATE
                            && command.parameters()[0] == text("lua")
                )
            }));
        }

        harness
            .sqlite_calls
            .lock()
            .expect("SQLite 调用锁不应中毒")
            .clear();
        *harness.current_snapshot.lock().expect("当前快照锁不应中毒") =
            desired_snapshot_rows(project.source_snapshot_fingerprint().as_bytes(), &[]);
        harness
            .service(1, 1)
            .deactivate_lua(&project)
            .await
            .expect("clear_standard 应停用 Lua owner");
        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        let SqliteTransactionStep::Execute(command) = &calls[0].1.steps()[0] else {
            panic!("停用 Lua owner 应只删除 owner state")
        };
        assert_eq!(command.statement(), DEACTIVATE_OWNER);
        assert_eq!(command.parameters(), &[text("lua")]);
    }

    #[test]
    fn real_sqlite_plan_inherits_only_exact_leaf_semantics_and_ignores_stale_owner_conflicts() {
        let mut connection = Connection::open_in_memory().expect("应该可打开内存 SQLite");
        create_current_asset_schema(&connection);
        let current_fingerprint = vec![0xa5; 32];
        let stale_fingerprint = vec![0xb4; 32];
        connection
            .execute("INSERT INTO metadata VALUES (?1)", [&current_fingerprint])
            .expect("应该可写入项目来源指纹");
        connection
            .execute(
                "INSERT INTO standard_asset_owner_state VALUES ('builtin', ?1)",
                [&current_fingerprint],
            )
            .expect("应该可建立 Builtin owner state");

        let mut desired = encode_batch(
            vec![single_entry_group("name", "宝剑")],
            MzStandardAssetOwner::Builtin,
        )
        .expect("测试叶子应该可编码")
        .remove(0);
        let exact_location = sqlite_text(&desired[1]).to_owned();
        connection
            .execute(
                "INSERT INTO entry (owner, exact_location, group_location, field_name, original_text, translation, translation_state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "builtin",
                    exact_location,
                    "old group",
                    "name",
                    "宝剑",
                    "Sword",
                    vec![0x11_u8; 32],
                ],
            )
            .expect("应该可写入已翻译叶子");

        desired[3] = text("new group");
        execute_test_plan(
            &mut connection,
            build_transaction_plan(
                MzStandardAssetOwner::Builtin,
                &[0xa5; 32],
                vec![desired.clone()],
            ),
        )
        .expect("group 变化不应阻止快照替换");
        let inherited = connection
            .query_row(
                "SELECT group_location, translation, translation_state FROM entry WHERE owner = 'builtin' AND exact_location = ?1",
                [&exact_location],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .expect("替换后叶子应该存在");
        assert_eq!(inherited.0, "new group");
        assert_eq!(inherited.1.as_deref(), Some("Sword"));
        assert_eq!(inherited.2, Some(vec![0x11; 32]));

        connection
            .execute(
                "INSERT INTO standard_asset_owner_state VALUES ('rules', ?1)",
                [&stale_fingerprint],
            )
            .expect("应该可建立过期 Rules owner state");
        connection
            .execute(
                "INSERT INTO entry (owner, exact_location, group_location, field_name, original_text) VALUES ('rules', ?1, 'rules group', 'name', '宝剑')",
                [&exact_location],
            )
            .expect("过期 owner 应可暂时保留重叠叶子");
        execute_test_plan(
            &mut connection,
            build_transaction_plan(
                MzStandardAssetOwner::Builtin,
                &[0xa5; 32],
                vec![desired.clone()],
            ),
        )
        .expect("过期 owner 不得阻止当前 owner 刷新");

        connection
            .execute(
                "UPDATE standard_asset_owner_state SET source_snapshot_fingerprint = ?1 WHERE owner = 'rules'",
                [&current_fingerprint],
            )
            .expect("应该可把 Rules owner 标记为新鲜");
        assert!(
            execute_test_plan(
                &mut connection,
                build_transaction_plan(
                    MzStandardAssetOwner::Builtin,
                    &[0xa5; 32],
                    vec![desired.clone()],
                ),
            )
            .is_err(),
            "新鲜其他 owner 的精确地址冲突必须回滚整个替换"
        );

        desired[4] = text("renamed");
        connection
            .execute(
                "DELETE FROM standard_asset_owner_state WHERE owner = 'rules'",
                [],
            )
            .expect("应该可停用 Rules owner");
        execute_test_plan(
            &mut connection,
            build_transaction_plan(MzStandardAssetOwner::Builtin, &[0xa5; 32], vec![desired]),
        )
        .expect("字段身份变化后应可提交新叶子");
        let invalidated = connection
            .query_row(
                "SELECT translation, translation_state FROM entry WHERE owner = 'builtin' AND exact_location = ?1",
                [&exact_location],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .expect("字段身份变化后叶子应该存在");
        assert_eq!(invalidated, (None, None));
    }

    #[tokio::test]
    async fn different_parallelism_and_batching_produce_the_same_transaction_plan() {
        let serial = Harness::new(None);
        let parallel = Harness::new(None);

        serial
            .service(1, 1)
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("串行编码应该成功");
        parallel
            .service(4, 3)
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("并行编码应该成功");

        let serial_calls = serial.sqlite_calls.lock().expect("串行 SQLite 锁不应中毒");
        let parallel_calls = parallel
            .sqlite_calls
            .lock()
            .expect("并行 SQLite 锁不应中毒");
        assert_eq!(serial_calls[0].1, parallel_calls[0].1);
    }

    #[tokio::test]
    async fn cpu_failure_stops_before_sqlite_side_effects_and_keeps_source() {
        let harness = Harness::new(None);
        let mut service = harness.service(1, 1);
        service.cpu.fail = true;

        let error = service
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect_err("CPU 执行失败应阻止事务");

        assert!(matches!(
            error,
            MzExtractionAssetStoreError::ScheduleEncoding(CpuTaskExecutionError::Unavailable(
                FakeError("cpu")
            ))
        ));
        assert!(error.source().is_some());
        assert!(
            harness
                .sqlite_calls
                .lock()
                .expect("SQLite 调用锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn maps_each_sqlite_terminal_state_without_losing_database_path_or_source() {
        for (response, expected) in [
            (SqliteResponse::NotFound, "not_found"),
            (SqliteResponse::Conflict, "conflict"),
            (SqliteResponse::NotCommitted, "not_committed"),
            (SqliteResponse::OutcomeUnknown, "outcome_unknown"),
        ] {
            let harness = Harness::new(Some(response));
            let error = harness
                .service(1, 1)
                .replace_rules(&project(), RulesSnapshot::empty())
                .await
                .expect_err("SQLite 终态应该传播");

            match (expected, error) {
                ("not_found", MzExtractionAssetStoreError::DatabaseNotFound { database_path }) => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"))
                }
                ("conflict", MzExtractionAssetStoreError::OwnershipConflict { database_path }) => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"))
                }
                (
                    "not_committed",
                    MzExtractionAssetStoreError::NotCommitted {
                        database_path,
                        source,
                    },
                ) => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("write"));
                }
                (
                    "outcome_unknown",
                    MzExtractionAssetStoreError::OutcomeUnknown {
                        database_path,
                        source,
                    },
                ) => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("commit"));
                }
                (expected, actual) => panic!("期望 {expected}，实际为 {actual}"),
            }
        }
    }

    #[test]
    fn replacement_future_is_send() {
        let harness = Harness::new(None);
        let service = harness.service(1, 1);
        let project = project();

        assert_send(service.replace_rules(&project, RulesSnapshot::empty()));
    }

    fn staging_parameter_sets(plan: &SqliteTransactionPlan) -> &[Vec<SqliteValue>] {
        plan.steps()
            .iter()
            .find_map(|step| match step {
                SqliteTransactionStep::ExecuteMany(batch)
                    if batch.statement() == INSERT_STAGING =>
                {
                    Some(batch.parameter_sets())
                }
                _ => None,
            })
            .expect("非空快照必须批量写入 staging")
    }

    fn all_kind_groups() -> Vec<ExtractedTextGroup> {
        [
            (
                TextGroupKind::DatabaseEntry,
                MzSource::data(StandardDataFile::Items),
            ),
            (
                TextGroupKind::System,
                MzSource::data(StandardDataFile::System),
            ),
            (TextGroupKind::Map, MzSource::map(1)),
            (TextGroupKind::EventDialogue, MzSource::map(2)),
            (TextGroupKind::EventChoices, MzSource::map(3)),
            (TextGroupKind::EventScrollingText, MzSource::map(4)),
            (TextGroupKind::EventCommand, MzSource::map(5)),
            (
                TextGroupKind::PluginParameter,
                MzSource::plugin_parameter(0, "QuestMenu", "Title"),
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, source))| {
            let group_location =
                MzLocation::value(source.clone(), vec![MzLocationStep::index(index)]);
            let exact_location = MzLocation::value(
                source,
                vec![MzLocationStep::index(index), MzLocationStep::key("text")],
            );
            ExtractedTextGroup::new(
                kind,
                group_location,
                vec![
                    ExtractedTextField::new("text", exact_location, format!("文本 {index}"))
                        .expect("字段应该合法"),
                ],
            )
            .expect("文本组应该合法")
        })
        .collect()
    }

    fn field(
        source: MzSource,
        entry_index: usize,
        field_name: &str,
        original_text: &str,
    ) -> ExtractedTextField {
        ExtractedTextField::new(
            field_name,
            MzLocation::value(
                source,
                vec![
                    MzLocationStep::index(entry_index),
                    MzLocationStep::key(field_name),
                ],
            ),
            original_text,
        )
        .expect("字段应该合法")
    }

    fn single_entry_group(field_name: &str, original_text: &str) -> ExtractedTextGroup {
        let source = MzSource::data(StandardDataFile::Items);
        ExtractedTextGroup::new(
            TextGroupKind::DatabaseEntry,
            MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]),
            vec![field(source, 1, field_name, original_text)],
        )
        .expect("单叶子测试组应该合法")
    }

    fn create_current_asset_schema(connection: &Connection) {
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;
                CREATE TABLE metadata (
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE entry (
                    owner TEXT NOT NULL,
                    exact_location TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, exact_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE system_text (
                    owner TEXT NOT NULL,
                    exact_location TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, exact_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE map_text (
                    owner TEXT NOT NULL,
                    exact_location TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, exact_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE text_body (
                    owner TEXT NOT NULL,
                    exact_location TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    unit_type TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, exact_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE plugin_param (
                    owner TEXT NOT NULL,
                    exact_location TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    field_name TEXT NOT NULL,
                    original_text TEXT NOT NULL,
                    translation TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, exact_location),
                    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
                );
                "#,
            )
            .expect("应该可建立当前标准资产测试 schema");
    }

    fn execute_test_plan(
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
                        return Err("事务要求查询返回了行".to_owned());
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

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应该合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    struct Harness {
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
        sqlite_calls: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        sqlite_query_calls: Arc<AtomicUsize>,
        current_snapshot: Arc<Mutex<Vec<SqliteRow>>>,
        response: Arc<Mutex<Option<SqliteResponse>>>,
    }

    impl Harness {
        fn new(response: Option<SqliteResponse>) -> Self {
            Self {
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
                sqlite_calls: Arc::new(Mutex::new(Vec::new())),
                sqlite_query_calls: Arc::new(AtomicUsize::new(0)),
                current_snapshot: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(response)),
            }
        }

        fn service(
            &self,
            encode_concurrency: usize,
            groups_per_job: usize,
        ) -> MzExtractionAssetStore<RecordingSqlite, RecordingCpu> {
            MzExtractionAssetStore::new(
                RecordingSqlite {
                    calls: Arc::clone(&self.sqlite_calls),
                    response: Arc::clone(&self.response),
                    query_calls: Arc::clone(&self.sqlite_query_calls),
                    current_snapshot: Arc::clone(&self.current_snapshot),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                    fail: false,
                },
                MzExtractionAssetStoreConfig::new(
                    non_zero(encode_concurrency),
                    non_zero(groups_per_job),
                ),
            )
        }
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }

    fn assert_send(_: impl Send) {}
}
