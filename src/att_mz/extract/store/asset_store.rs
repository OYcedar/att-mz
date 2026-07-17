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
    ExecuteTransactionError, SqliteBatch, SqliteCheckId, SqliteCommand, SqliteQuery,
    SqliteTransactionExecutor, SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use super::super::model::{BuiltinSnapshot, ExtractedTextGroup, RulesSnapshot};
use super::{BuiltinSnapshotStore, RulesSnapshotStore};
use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};

const OWNER_CONFLICT_CHECK: &str = "mz_extraction_owner_conflict";

const CREATE_ENTRY_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS entry (
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    translation    TEXT
)"#;

const CREATE_SYSTEM_TEXT_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS system_text (
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    translation    TEXT
)"#;

const CREATE_MAP_TEXT_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS map_text (
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    translation    TEXT
)"#;

const CREATE_TEXT_BODY_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS text_body (
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    unit_type      TEXT NOT NULL CHECK (
        unit_type IN ('dialogue', 'choices', 'scrolling_text', 'event_command')
    ),
    original_text  TEXT NOT NULL,
    translation    TEXT
)"#;

const CREATE_PLUGIN_PARAM_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS plugin_param (
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    translation    TEXT
)"#;

const CREATE_TERMINOLOGY_DEPENDENCY_TABLE: &str = r#"CREATE TABLE IF NOT EXISTS translation_terminology_dependency (
    asset_table      TEXT NOT NULL,
    exact_location   TEXT NOT NULL,
    term             TEXT NOT NULL,
    term_translation TEXT NOT NULL,
    PRIMARY KEY (asset_table, exact_location, term)
)"#;

const DROP_STAGING_TABLE: &str = "DROP TABLE IF EXISTS temp.att_mz_extraction_staging";
const DROP_PREVIOUS_TABLE: &str = "DROP TABLE IF EXISTS temp.att_mz_extraction_previous";

const CREATE_STAGING_TABLE: &str = r#"CREATE TEMP TABLE att_mz_extraction_staging (
    target_table   TEXT NOT NULL,
    exact_location TEXT NOT NULL PRIMARY KEY,
    owner          TEXT NOT NULL,
    group_location TEXT NOT NULL,
    field_name     TEXT NOT NULL,
    original_text  TEXT NOT NULL,
    unit_type      TEXT,
    translation    TEXT
)"#;

const CREATE_PREVIOUS_TABLE: &str = r#"CREATE TEMP TABLE att_mz_extraction_previous AS
SELECT 'entry' AS target_table, exact_location, owner, original_text, translation FROM entry
UNION ALL
SELECT 'system_text', exact_location, owner, original_text, translation FROM system_text
UNION ALL
SELECT 'map_text', exact_location, owner, original_text, translation FROM map_text
UNION ALL
SELECT 'text_body', exact_location, owner, original_text, translation FROM text_body
UNION ALL
SELECT 'plugin_param', exact_location, owner, original_text, translation FROM plugin_param"#;

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
JOIN att_mz_extraction_previous AS previous
  ON previous.exact_location = staged.exact_location
WHERE previous.owner <> staged.owner
LIMIT 1"#;

const INHERIT_TRANSLATIONS: &str = r#"UPDATE att_mz_extraction_staging
SET translation = (
    SELECT previous.translation
    FROM att_mz_extraction_previous AS previous
    WHERE previous.exact_location = att_mz_extraction_staging.exact_location
      AND previous.owner = att_mz_extraction_staging.owner
      AND previous.original_text = att_mz_extraction_staging.original_text
    LIMIT 1
)"#;

const MIGRATE_INHERITED_TERMINOLOGY_DEPENDENCIES: &str = r#"UPDATE translation_terminology_dependency
SET asset_table = (
    SELECT staged.target_table
    FROM att_mz_extraction_previous AS previous
    JOIN att_mz_extraction_staging AS staged
      ON staged.exact_location = previous.exact_location
     AND staged.owner = previous.owner
     AND staged.original_text = previous.original_text
    WHERE previous.target_table = translation_terminology_dependency.asset_table
      AND previous.exact_location = translation_terminology_dependency.exact_location
      AND staged.translation IS NOT NULL
    LIMIT 1
)
WHERE EXISTS (
    SELECT 1
    FROM att_mz_extraction_previous AS previous
    JOIN att_mz_extraction_staging AS staged
      ON staged.exact_location = previous.exact_location
     AND staged.owner = previous.owner
     AND staged.original_text = previous.original_text
    WHERE previous.target_table = translation_terminology_dependency.asset_table
      AND previous.exact_location = translation_terminology_dependency.exact_location
      AND staged.translation IS NOT NULL
)"#;

const DELETE_INVALIDATED_TERMINOLOGY_DEPENDENCIES: &str = r#"DELETE FROM translation_terminology_dependency
WHERE EXISTS (
    SELECT 1
    FROM att_mz_extraction_previous AS previous
    WHERE previous.target_table = translation_terminology_dependency.asset_table
      AND previous.exact_location = translation_terminology_dependency.exact_location
      AND previous.owner = ?
      AND NOT EXISTS (
          SELECT 1
          FROM att_mz_extraction_staging AS staged
          WHERE staged.target_table = previous.target_table
            AND staged.exact_location = previous.exact_location
            AND staged.owner = previous.owner
            AND staged.original_text = previous.original_text
            AND staged.translation IS NOT NULL
      )
)"#;

const DELETE_OWNER_FROM_TABLES: [&str; 5] = [
    "DELETE FROM entry WHERE owner = ?",
    "DELETE FROM system_text WHERE owner = ?",
    "DELETE FROM map_text WHERE owner = ?",
    "DELETE FROM text_body WHERE owner = ?",
    "DELETE FROM plugin_param WHERE owner = ?",
];

const INSERT_ENTRY: &str = r#"INSERT INTO entry (
    exact_location, owner, group_location, field_name, original_text, translation
)
SELECT exact_location, owner, group_location, field_name, original_text, translation
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_SYSTEM_TEXT: &str = r#"INSERT INTO system_text (
    exact_location, owner, group_location, field_name, original_text, translation
)
SELECT exact_location, owner, group_location, field_name, original_text, translation
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_MAP_TEXT: &str = r#"INSERT INTO map_text (
    exact_location, owner, group_location, field_name, original_text, translation
)
SELECT exact_location, owner, group_location, field_name, original_text, translation
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_TEXT_BODY: &str = r#"INSERT INTO text_body (
    exact_location, owner, group_location, field_name, unit_type, original_text, translation
)
SELECT exact_location, owner, group_location, field_name, unit_type, original_text, translation
FROM att_mz_extraction_staging
WHERE target_table = ?
ORDER BY exact_location"#;

const INSERT_PLUGIN_PARAM: &str = r#"INSERT INTO plugin_param (
    exact_location, owner, group_location, field_name, original_text, translation
)
SELECT exact_location, owner, group_location, field_name, original_text, translation
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
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    async fn replace(
        &self,
        project: &OpenedProject,
        owner: MzStandardAssetOwner,
        groups: Vec<ExtractedTextGroup>,
    ) -> Result<(), MzExtractionAssetStoreError<C::Error, S::Error>> {
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

        let parameter_sets = parameter_batches.into_iter().flatten().collect();
        let plan = build_transaction_plan(owner, parameter_sets);
        let database_path = project.database_path().to_path_buf();

        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_persist_error(database_path, error))
    }
}

impl<S, C> BuiltinSnapshotStore for MzExtractionAssetStore<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzExtractionAssetStoreError<C::Error, S::Error>;

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
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzExtractionAssetStoreError<C::Error, S::Error>;

    async fn replace_rules(
        &self,
        project: &OpenedProject,
        snapshot: RulesSnapshot,
    ) -> Result<(), Self::Error> {
        self.replace(project, MzStandardAssetOwner::Rules, snapshot.into_groups())
            .await
    }
}

/// 标准提取快照替换的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzExtractionAssetStoreError<C, S> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    EncodeLocation(MzLocationCodecError),
    DatabaseNotFound { database_path: PathBuf },
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
            Self::OwnershipConflict { database_path } => write!(
                formatter,
                "Builtin 与 Rules 拥有了同一文本位置：{}",
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

fn build_transaction_plan(
    owner: MzStandardAssetOwner,
    parameter_sets: Vec<Vec<SqliteValue>>,
) -> SqliteTransactionPlan {
    let mut steps = Vec::with_capacity(24);
    for statement in [
        CREATE_ENTRY_TABLE,
        CREATE_SYSTEM_TEXT_TABLE,
        CREATE_MAP_TEXT_TABLE,
        CREATE_TEXT_BODY_TABLE,
        CREATE_PLUGIN_PARAM_TABLE,
        CREATE_TERMINOLOGY_DEPENDENCY_TABLE,
        DROP_STAGING_TABLE,
        DROP_PREVIOUS_TABLE,
        CREATE_STAGING_TABLE,
        CREATE_PREVIOUS_TABLE,
    ] {
        steps.push(execute(statement, Vec::new()));
    }

    if !parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_STAGING,
            parameter_sets,
        )));
    }

    steps.push(SqliteTransactionStep::RequireNoRows {
        check_id: SqliteCheckId::new(OWNER_CONFLICT_CHECK),
        query: SqliteQuery::new(FIND_OWNER_CONFLICT, Vec::new()),
    });
    steps.push(execute(INHERIT_TRANSLATIONS, Vec::new()));
    steps.push(execute(
        MIGRATE_INHERITED_TERMINOLOGY_DEPENDENCIES,
        Vec::new(),
    ));
    steps.push(execute(
        DELETE_INVALIDATED_TERMINOLOGY_DEPENDENCIES,
        vec![text(owner.storage_name())],
    ));

    for statement in DELETE_OWNER_FROM_TABLES {
        steps.push(execute(statement, vec![text(owner.storage_name())]));
    }

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
        ExecuteTransactionError::RequirementFailed { .. } => {
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
    use crate::att_mz::extract::document::StandardDataFile;
    use crate::att_mz::extract::model::{ExtractedTextField, MzLocation, MzLocationStep, MzSource};
    use crate::att_mz::text::TextGroupKind;

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
                Some(SqliteResponse::Conflict) => Err(ExecuteTransactionError::RequirementFailed {
                    check_id: SqliteCheckId::new(OWNER_CONFLICT_CHECK),
                }),
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
            .position(|step| matches!(step, SqliteTransactionStep::RequireNoRows { .. }))
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
    async fn domain_tables_keep_original_and_translation_together_and_inherit_by_leaf() {
        let harness = Harness::new(None);

        harness
            .service(1, 1)
            .replace_builtin(&project(), BuiltinSnapshot::new(all_kind_groups()).unwrap())
            .await
            .expect("快照应该成功编码");

        let calls = harness.sqlite_calls.lock().expect("SQLite 调用锁不应中毒");
        let steps = calls[0].1.steps();
        let table_definitions = steps
            .iter()
            .filter_map(|step| match step {
                SqliteTransactionStep::Execute(command)
                    if command
                        .statement()
                        .starts_with("CREATE TABLE IF NOT EXISTS") =>
                {
                    Some(command.statement())
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(table_definitions.len(), 6);
        let domain_tables = table_definitions
            .iter()
            .copied()
            .filter(|statement| !statement.contains("translation_terminology_dependency"))
            .collect::<Vec<_>>();
        assert_eq!(domain_tables.len(), 5);
        assert!(domain_tables.iter().all(|statement| {
            statement.contains("original_text")
                && statement.contains("translation")
                && !statement.contains(" status")
        }));
        assert!(table_definitions.iter().any(|statement| {
            statement.contains("translation_terminology_dependency")
                && statement.contains("PRIMARY KEY (asset_table, exact_location, term)")
        }));
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement() == INHERIT_TRANSLATIONS
                        && command.statement().contains("previous.exact_location")
                        && command.statement().contains("previous.original_text")
            )
        }));
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement() == MIGRATE_INHERITED_TERMINOLOGY_DEPENDENCIES
                        && command.statement().contains("SET asset_table")
                        && command.statement().contains("staged.translation IS NOT NULL")
            )
        }));
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement() == DELETE_INVALIDATED_TERMINOLOGY_DEPENDENCIES
                        && command.parameters() == [text("builtin")]
                        && command.statement().contains("staged.original_text = previous.original_text")
                        && command.statement().contains("staged.translation IS NOT NULL")
            )
        }));
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
        response: Arc<Mutex<Option<SqliteResponse>>>,
    }

    impl Harness {
        fn new(response: Option<SqliteResponse>) -> Self {
            Self {
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
                sqlite_calls: Arc::new(Mutex::new(Vec::new())),
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
