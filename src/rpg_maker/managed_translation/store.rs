//! Lua 托管翻译快照的 SQLite 实现。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use serde_json::Value;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::SourceSnapshotFingerprint;
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteBatch, SqliteCommand, SqliteQuery,
    SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};

use super::{
    ManagedTranslationCheckpoint, ManagedTranslationCheckpointAction, ManagedTranslationCollection,
    ManagedTranslationContent, ManagedTranslationManifestFingerprint, ManagedTranslationMetadata,
    ManagedTranslationModelError, ManagedTranslationPair, ManagedTranslationShape,
    ManagedTranslationSnapshot, ManagedTranslationUnit,
};

const READ_METADATA_SOURCE: &str = "SELECT source_snapshot_fingerprint FROM metadata";
const READ_OWNER_STATE: &str = r#"SELECT source_snapshot_fingerprint, manifest_fingerprint
FROM managed_translation_owner_state
WHERE owner = 'lua'"#;
const READ_COLLECTIONS: &str = r#"SELECT collection_name, collection_order, instruction
FROM managed_translation_collection
WHERE owner = 'lua'
ORDER BY collection_order"#;
const READ_UNITS: &str = r#"SELECT
    collection_name,
    unit_key,
    unit_order,
    kind,
    shape,
    original_content_json,
    context,
    metadata_json,
    translation_content_json,
    translation_state
FROM managed_translation_unit
WHERE owner = 'lua'
ORDER BY collection_name, unit_order"#;

const REQUIRE_PROJECT_SOURCE: &str = r#"SELECT 1
WHERE (SELECT COUNT(*) FROM metadata) <> 1
   OR NOT EXISTS (
       SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?1
   )"#;

const CREATE_INCOMING_COLLECTIONS: &str = r#"CREATE TEMP TABLE managed_translation_incoming_collection (
    collection_name  TEXT NOT NULL PRIMARY KEY,
    collection_order INTEGER NOT NULL UNIQUE,
    instruction      TEXT NOT NULL
) WITHOUT ROWID"#;
const CREATE_INCOMING_UNITS: &str = r#"CREATE TEMP TABLE managed_translation_incoming_unit (
    collection_name      TEXT NOT NULL,
    unit_key             TEXT NOT NULL,
    unit_order           INTEGER NOT NULL,
    kind                 TEXT NOT NULL,
    shape                TEXT NOT NULL,
    original_content_json TEXT NOT NULL,
    context              TEXT NOT NULL,
    metadata_json        TEXT,
    PRIMARY KEY (collection_name, unit_key),
    UNIQUE (collection_name, unit_order)
) WITHOUT ROWID"#;
const CREATE_PRESERVED_PAIRS: &str = r#"CREATE TEMP TABLE managed_translation_preserved_pair (
    collection_name          TEXT NOT NULL,
    unit_key                 TEXT NOT NULL,
    translation_content_json TEXT NOT NULL,
    translation_state        BLOB NOT NULL,
    PRIMARY KEY (collection_name, unit_key)
) WITHOUT ROWID"#;

const INSERT_INCOMING_COLLECTION_PREFIX: &str = "INSERT INTO managed_translation_incoming_collection (collection_name, collection_order, instruction)";
const INSERT_INCOMING_UNIT_PREFIX: &str = r#"INSERT INTO managed_translation_incoming_unit (
    collection_name,
    unit_key,
    unit_order,
    kind,
    shape,
    original_content_json,
    context,
    metadata_json
)"#;

const PRESERVE_CURRENT_PAIRS: &str = r#"INSERT INTO managed_translation_preserved_pair (
    collection_name,
    unit_key,
    translation_content_json,
    translation_state
)
SELECT
    current_unit.collection_name,
    current_unit.unit_key,
    current_unit.translation_content_json,
    current_unit.translation_state
FROM managed_translation_unit AS current_unit
JOIN managed_translation_collection AS current_collection
  ON current_collection.owner = current_unit.owner
 AND current_collection.collection_name = current_unit.collection_name
JOIN managed_translation_incoming_collection AS incoming_collection
  ON incoming_collection.collection_name = current_unit.collection_name
JOIN managed_translation_incoming_unit AS incoming_unit
  ON incoming_unit.collection_name = current_unit.collection_name
 AND incoming_unit.unit_key = current_unit.unit_key
WHERE current_unit.owner = 'lua'
  AND current_collection.instruction = incoming_collection.instruction
  AND current_unit.kind = incoming_unit.kind
  AND current_unit.shape = incoming_unit.shape
  AND current_unit.original_content_json = incoming_unit.original_content_json
  AND current_unit.context = incoming_unit.context
  AND current_unit.translation_content_json IS NOT NULL
  AND current_unit.translation_state IS NOT NULL"#;

const DELETE_OWNER: &str = "DELETE FROM managed_translation_owner_state WHERE owner = 'lua'";
const INSERT_OWNER: &str = r#"INSERT INTO managed_translation_owner_state (
    owner,
    source_snapshot_fingerprint,
    manifest_fingerprint
) VALUES ('lua', ?1, ?2)"#;
const INSERT_COLLECTIONS: &str = r#"INSERT INTO managed_translation_collection (
    owner,
    collection_name,
    collection_order,
    instruction
)
SELECT 'lua', collection_name, collection_order, instruction
FROM managed_translation_incoming_collection
ORDER BY collection_order"#;
const INSERT_UNITS: &str = r#"INSERT INTO managed_translation_unit (
    owner,
    collection_name,
    unit_key,
    unit_order,
    kind,
    shape,
    original_content_json,
    context,
    metadata_json,
    translation_content_json,
    translation_state
)
SELECT
    'lua',
    incoming.collection_name,
    incoming.unit_key,
    incoming.unit_order,
    incoming.kind,
    incoming.shape,
    incoming.original_content_json,
    incoming.context,
    incoming.metadata_json,
    preserved.translation_content_json,
    preserved.translation_state
FROM managed_translation_incoming_unit AS incoming
LEFT JOIN managed_translation_preserved_pair AS preserved
  ON preserved.collection_name = incoming.collection_name
 AND preserved.unit_key = incoming.unit_key
ORDER BY incoming.collection_name, incoming.unit_order"#;

const DROP_PRESERVED_PAIRS: &str = "DROP TABLE managed_translation_preserved_pair";
const DROP_INCOMING_UNITS: &str = "DROP TABLE managed_translation_incoming_unit";
const DROP_INCOMING_COLLECTIONS: &str = "DROP TABLE managed_translation_incoming_collection";

const REQUIRE_CHECKPOINT_BASELINE: &str = r#"SELECT 1
WHERE (SELECT COUNT(*) FROM metadata) <> 1
   OR NOT EXISTS (
       SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?1
   )
   OR (SELECT COUNT(*) FROM managed_translation_owner_state WHERE owner = 'lua') <> 1
   OR NOT EXISTS (
       SELECT 1
       FROM managed_translation_owner_state
       WHERE owner = 'lua'
         AND source_snapshot_fingerprint = ?1
         AND manifest_fingerprint = ?2
   )"#;
const REQUIRE_COMPLETE_CHECKPOINT_BASELINE: &str = r#"SELECT 1
WHERE (SELECT COUNT(*) FROM metadata) <> 1
   OR NOT EXISTS (
       SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?1
   )
   OR (SELECT COUNT(*) FROM managed_translation_owner_state WHERE owner = 'lua') <> 1
   OR NOT EXISTS (
       SELECT 1
       FROM managed_translation_owner_state
       WHERE owner = 'lua'
         AND source_snapshot_fingerprint = ?1
         AND manifest_fingerprint = ?2
   )
   OR (SELECT COUNT(*) FROM managed_translation_collection WHERE owner = 'lua') <> ?3
   OR (SELECT COUNT(*) FROM managed_translation_unit WHERE owner = 'lua') <> ?4"#;
const REQUIRE_CHECKPOINT_COLLECTION: &str = r#"SELECT 1
WHERE NOT EXISTS (
    SELECT 1
    FROM managed_translation_collection
    WHERE owner = 'lua'
      AND collection_name = ?1
      AND collection_order IS ?2
      AND instruction IS ?3
)"#;
const REQUIRE_CHECKPOINT_UNIT: &str = r#"SELECT 1
WHERE NOT EXISTS (
    SELECT 1
    FROM managed_translation_unit AS unit
    JOIN managed_translation_collection AS collection
      ON collection.owner = unit.owner
     AND collection.collection_name = unit.collection_name
    WHERE unit.owner = 'lua'
      AND unit.collection_name = ?1
      AND unit.unit_key = ?2
      AND collection.collection_order IS ?3
      AND collection.instruction IS ?4
      AND unit.unit_order IS ?5
      AND unit.kind IS ?6
      AND unit.shape IS ?7
      AND unit.original_content_json IS ?8
      AND unit.context IS ?9
      AND unit.metadata_json IS ?10
      AND unit.translation_content_json IS ?11
      AND unit.translation_state IS ?12
)"#;
const UPDATE_CHECKPOINT_UNIT: &str = r#"UPDATE managed_translation_unit
SET translation_content_json = ?1,
    translation_state = ?2
WHERE owner = 'lua'
  AND collection_name = ?3
  AND unit_key = ?4
  AND translation_content_json IS ?5
  AND translation_state IS ?6"#;

/// 可与 Standard Extract 事务直接拼接的托管快照变更。
///
/// 变更自带 metadata 来源 CAS，但不执行事务；组合方把 `into_steps()` 返回的步骤放入
/// 自己唯一的事务计划，即可保证 Standard intent 与 managed intent 同生共死。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ManagedTranslationSnapshotMutation {
    steps: Vec<SqliteTransactionStep>,
}

impl ManagedTranslationSnapshotMutation {
    pub(crate) fn replace(snapshot: &ManagedTranslationSnapshot) -> Self {
        let mut collection_values = Vec::new();
        let mut unit_values = Vec::new();
        for (collection_order, collection) in snapshot.collections().iter().enumerate() {
            collection_values.extend([
                text(collection.name()),
                order(collection_order),
                text(collection.instruction()),
            ]);
            for (unit_order, unit) in collection.units().iter().enumerate() {
                unit_values.extend([
                    text(collection.name()),
                    text(unit.key()),
                    order(unit_order),
                    text(unit.kind()),
                    text(unit.shape().storage_name()),
                    text(unit.original().canonical_json()),
                    text(unit.context()),
                    unit.metadata().map_or(SqliteValue::Null, |metadata| {
                        text(metadata.canonical_json())
                    }),
                ]);
            }
        }

        let mut steps = vec![
            require_project_source(snapshot.source_snapshot_fingerprint()),
            execute(CREATE_INCOMING_COLLECTIONS, Vec::new()),
            execute(CREATE_INCOMING_UNITS, Vec::new()),
            execute(CREATE_PRESERVED_PAIRS, Vec::new()),
        ];
        if !collection_values.is_empty() {
            steps.push(SqliteTransactionStep::ExecuteMany(
                SqliteBatch::bulk_insert_flat(
                    INSERT_INCOMING_COLLECTION_PREFIX,
                    3,
                    Vec::new(),
                    collection_values,
                ),
            ));
        }
        if !unit_values.is_empty() {
            steps.push(SqliteTransactionStep::ExecuteMany(
                SqliteBatch::bulk_insert_flat(
                    INSERT_INCOMING_UNIT_PREFIX,
                    8,
                    Vec::new(),
                    unit_values,
                ),
            ));
        }
        steps.extend([
            execute(PRESERVE_CURRENT_PAIRS, Vec::new()),
            execute(DELETE_OWNER, Vec::new()),
            execute(
                INSERT_OWNER,
                vec![
                    blob(snapshot.source_snapshot_fingerprint().as_bytes()),
                    blob(snapshot.manifest_fingerprint().as_bytes()),
                ],
            ),
            execute(INSERT_COLLECTIONS, Vec::new()),
            execute(INSERT_UNITS, Vec::new()),
            execute(DROP_PRESERVED_PAIRS, Vec::new()),
            execute(DROP_INCOMING_UNITS, Vec::new()),
            execute(DROP_INCOMING_COLLECTIONS, Vec::new()),
        ]);
        Self { steps }
    }

    pub(crate) fn deactivate(source_snapshot_fingerprint: SourceSnapshotFingerprint) -> Self {
        Self {
            steps: vec![
                require_project_source(source_snapshot_fingerprint),
                execute(DELETE_OWNER, Vec::new()),
            ],
        }
    }

    #[cfg(test)]
    pub(crate) fn steps(&self) -> &[SqliteTransactionStep] {
        &self.steps
    }

    pub(crate) fn into_steps(self) -> Vec<SqliteTransactionStep> {
        self.steps
    }
}

/// 跨 Extract、Translate 与 WriteBack 共用的托管快照项目存储契约。
pub(crate) trait ManagedTranslationRepository: Send + Sync {
    type DriverError: Error + Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn load(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<Option<ManagedTranslationSnapshot>, Self::Error>> + Send;

    /// 判断一次读取失败是否明确表示项目来源与托管 owner 已不再是同一 Extract 快照。
    fn is_source_stale(error: &Self::Error) -> bool;

    fn checkpoint(
        &self,
        project: &OpenedProject,
        checkpoint: ManagedTranslationCheckpoint,
    ) -> impl Future<
        Output = Result<
            ManagedTranslationCheckpointOutcome<Self::DriverError>,
            ManagedTranslationCheckpointError<Self::DriverError>,
        >,
    > + Send;
}

/// 使用 SQLite 一致快照与短写事务实现托管翻译存储。
#[derive(Clone)]
pub(crate) struct ManagedTranslationSqliteRepository<S> {
    sqlite: S,
}

impl<S> ManagedTranslationSqliteRepository<S> {
    pub(crate) fn new(sqlite: S) -> Self {
        Self { sqlite }
    }
}

impl<S> ManagedTranslationRepository for ManagedTranslationSqliteRepository<S>
where
    S: SqliteQueryExecutor + SqliteTransactionExecutor<Error = <S as SqliteQueryExecutor>::Error>,
{
    type DriverError = <S as SqliteQueryExecutor>::Error;
    type Error = ManagedTranslationStoreError<<S as SqliteQueryExecutor>::Error>;

    fn is_source_stale(error: &Self::Error) -> bool {
        matches!(
            error,
            ManagedTranslationStoreError::ProjectSourceChanged { .. }
                | ManagedTranslationStoreError::OwnerSourceStale { .. }
        )
    }

    async fn load(
        &self,
        project: &OpenedProject,
    ) -> Result<Option<ManagedTranslationSnapshot>, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let results = self
            .sqlite
            .query_existing_database_snapshot(
                database_path.clone(),
                vec![
                    SqliteQuery::new(READ_METADATA_SOURCE, Vec::new())
                        .with_id("managed_translation.metadata"),
                    SqliteQuery::new(READ_OWNER_STATE, Vec::new())
                        .with_id("managed_translation.owner"),
                    SqliteQuery::new(READ_COLLECTIONS, Vec::new())
                        .with_id("managed_translation.collections"),
                    SqliteQuery::new(READ_UNITS, Vec::new()).with_id("managed_translation.units"),
                ],
            )
            .await
            .map_err(|error| map_query_error(database_path.clone(), error))?;
        decode_snapshot(results, project.source_snapshot_fingerprint()).map_err(|source| {
            match source {
                StoredSnapshotError::ProjectSourceChanged { expected, actual } => {
                    ManagedTranslationStoreError::ProjectSourceChanged {
                        database_path,
                        expected,
                        actual,
                    }
                }
                StoredSnapshotError::OwnerSourceStale { expected, actual } => {
                    ManagedTranslationStoreError::OwnerSourceStale {
                        database_path,
                        expected,
                        actual,
                    }
                }
                source => ManagedTranslationStoreError::InvalidSnapshot {
                    database_path,
                    source,
                },
            }
        })
    }

    async fn checkpoint(
        &self,
        project: &OpenedProject,
        checkpoint: ManagedTranslationCheckpoint,
    ) -> Result<
        ManagedTranslationCheckpointOutcome<Self::DriverError>,
        ManagedTranslationCheckpointError<Self::DriverError>,
    > {
        if checkpoint.source_snapshot_fingerprint() != project.source_snapshot_fingerprint() {
            return Ok(ManagedTranslationCheckpointOutcome::NotApplied);
        }
        if checkpoint.is_empty() {
            return Ok(ManagedTranslationCheckpointOutcome::Applied);
        }
        let plan = checkpoint_plan(checkpoint);
        match self
            .sqlite
            .execute_transaction(project.database_path().to_path_buf(), plan)
            .await
        {
            Ok(()) => Ok(ManagedTranslationCheckpointOutcome::Applied),
            Err(
                ExecuteTransactionError::RequirementFailed
                | ExecuteTransactionError::RequirementFailedWithRow { .. },
            ) => Ok(ManagedTranslationCheckpointOutcome::NotApplied),
            Err(ExecuteTransactionError::RequirementFailedWithRowOutcomeUnknown {
                source, ..
            }) => Ok(ManagedTranslationCheckpointOutcome::OutcomeUnknown(*source)),
            Err(ExecuteTransactionError::OutcomeUnknown(source)) => {
                Ok(ManagedTranslationCheckpointOutcome::OutcomeUnknown(source))
            }
            Err(ExecuteTransactionError::NotFound) => {
                Err(ManagedTranslationCheckpointError::DatabaseNotFound {
                    database_path: project.database_path().to_path_buf(),
                })
            }
            Err(ExecuteTransactionError::NotCommitted(source)) => {
                Err(ManagedTranslationCheckpointError::NotCommitted {
                    database_path: project.database_path().to_path_buf(),
                    source,
                })
            }
        }
    }
}

fn checkpoint_plan(checkpoint: ManagedTranslationCheckpoint) -> SqliteTransactionPlan {
    let collection_guards = checkpoint
        .collections
        .iter()
        .map(|collection| {
            vec![
                text(&collection.name),
                order(collection.order),
                text(&collection.instruction),
            ]
        })
        .collect::<Vec<_>>();
    let mut guards = Vec::with_capacity(checkpoint.writes.len());
    let mut updates = Vec::with_capacity(checkpoint.writes.len());
    for write in checkpoint.writes {
        let (expected_content, expected_state) = pair_values(write.expected.as_ref());
        guards.push(vec![
            text(&write.collection),
            text(&write.key),
            order(write.collection_order),
            text(&write.instruction),
            order(write.unit_order),
            text(&write.kind),
            text(write.shape.storage_name()),
            text(write.original.canonical_json()),
            text(&write.context),
            write
                .metadata
                .as_ref()
                .map_or(SqliteValue::Null, |metadata| {
                    text(metadata.canonical_json())
                }),
            expected_content.clone(),
            expected_state.clone(),
        ]);
        if let ManagedTranslationCheckpointAction::Replace(replacement) = write.action {
            let (replacement_content, replacement_state) = pair_values(replacement.as_ref());
            updates.push(vec![
                replacement_content,
                replacement_state,
                text(write.collection),
                text(write.key),
                expected_content,
                expected_state,
            ]);
        }
    }
    let baseline = if checkpoint.complete_guard {
        SqliteQuery::new(
            REQUIRE_COMPLETE_CHECKPOINT_BASELINE,
            vec![
                blob(checkpoint.source_snapshot_fingerprint.as_bytes()),
                blob(checkpoint.manifest_fingerprint.as_bytes()),
                order(checkpoint.collections.len()),
                order(checkpoint.unit_count),
            ],
        )
    } else {
        SqliteQuery::new(
            REQUIRE_CHECKPOINT_BASELINE,
            vec![
                blob(checkpoint.source_snapshot_fingerprint.as_bytes()),
                blob(checkpoint.manifest_fingerprint.as_bytes()),
            ],
        )
    };
    let mut steps = vec![SqliteTransactionStep::RequireNoRows(baseline)];
    if !collection_guards.is_empty() {
        steps.push(SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
            REQUIRE_CHECKPOINT_COLLECTION,
            collection_guards,
        )));
    }
    if !guards.is_empty() {
        steps.push(SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
            REQUIRE_CHECKPOINT_UNIT,
            guards,
        )));
    }
    if !updates.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(UPDATE_CHECKPOINT_UNIT, updates),
        ));
    }
    SqliteTransactionPlan::new(steps)
}

fn pair_values(pair: Option<&ManagedTranslationPair>) -> (SqliteValue, SqliteValue) {
    pair.map_or((SqliteValue::Null, SqliteValue::Null), |pair| {
        (
            text(pair.content().canonical_json()),
            blob(pair.state().as_bytes()),
        )
    })
}

fn require_project_source(
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
) -> SqliteTransactionStep {
    SqliteTransactionStep::RequireNoRows(
        SqliteQuery::new(
            REQUIRE_PROJECT_SOURCE,
            vec![blob(source_snapshot_fingerprint.as_bytes())],
        )
        .with_id("managed_translation.require_project_source"),
    )
}

fn execute(statement: &str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn blob(value: &[u8]) -> SqliteValue {
    SqliteValue::Blob(value.to_vec())
}

fn order(value: usize) -> SqliteValue {
    SqliteValue::Integer(
        i64::try_from(value).expect("内存中的托管翻译自然顺序必须可写入 SQLite INTEGER"),
    )
}

fn map_query_error<E>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<E>,
) -> ManagedTranslationStoreError<E> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            ManagedTranslationStoreError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => ManagedTranslationStoreError::Read {
            database_path,
            source,
        },
    }
}

#[derive(Debug)]
pub(crate) enum ManagedTranslationStoreError<E> {
    DatabaseNotFound {
        database_path: PathBuf,
    },
    Read {
        database_path: PathBuf,
        source: E,
    },
    ProjectSourceChanged {
        database_path: PathBuf,
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    OwnerSourceStale {
        database_path: PathBuf,
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    InvalidSnapshot {
        database_path: PathBuf,
        source: StoredSnapshotError,
    },
}

impl<E: fmt::Display> fmt::Display for ManagedTranslationStoreError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::Read {
                database_path,
                source,
            } => write!(
                formatter,
                "无法读取 {} 的托管翻译快照：{source}",
                database_path.display()
            ),
            Self::ProjectSourceChanged {
                database_path,
                expected,
                actual,
            } => write!(
                formatter,
                "{} 的项目来源在打开后发生变化：expected={expected:?}, actual={actual:?}",
                database_path.display()
            ),
            Self::OwnerSourceStale {
                database_path,
                expected,
                actual,
            } => write!(
                formatter,
                "{} 的托管翻译 owner 来源已过期：expected={expected:?}, actual={actual:?}；请重新 Extract",
                database_path.display()
            ),
            Self::InvalidSnapshot {
                database_path,
                source,
            } => write!(
                formatter,
                "{} 的托管翻译快照无效：{source}",
                database_path.display()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ManagedTranslationStoreError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl<E> SafeDiagnosticSource for ManagedTranslationStoreError<E>
where
    E: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::DatabaseNotFound { database_path } => SafeDiagnostic::new(
                DiagnosticCode::ProjectUnavailable,
                stage,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component("project_database")),
            Self::Read {
                database_path,
                source,
            } => managed_translation_storage_source_diagnostic(
                source,
                database_path,
                stage,
                impact,
                fallback_action,
            )
            .with_recovery(RecoveryFact::component("read_managed_translation_snapshot")),
            Self::ProjectSourceChanged {
                database_path,
                expected,
                actual,
            } => managed_translation_stale_diagnostic(
                database_path,
                stage,
                impact,
                "project_source_changed",
                expected,
                actual,
            ),
            Self::OwnerSourceStale {
                database_path,
                expected,
                actual,
            } => managed_translation_stale_diagnostic(
                database_path,
                stage,
                impact,
                "managed_owner_source_stale",
                expected,
                actual,
            ),
            Self::InvalidSnapshot {
                database_path,
                source,
            } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    source.safe_diagnostic_detail(),
                ),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component("reinitialize_project_database")),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ManagedTranslationCheckpointOutcome<E> {
    Applied,
    NotApplied,
    OutcomeUnknown(E),
}

#[derive(Debug)]
pub(crate) enum ManagedTranslationCheckpointError<E> {
    DatabaseNotFound { database_path: PathBuf },
    NotCommitted { database_path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ManagedTranslationCheckpointError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::NotCommitted {
                database_path,
                source,
            } => write!(
                formatter,
                "{} 的托管翻译 checkpoint 确认未提交：{source}",
                database_path.display()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ManagedTranslationCheckpointError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DatabaseNotFound { .. } => None,
            Self::NotCommitted { source, .. } => Some(source),
        }
    }
}

impl<E> SafeDiagnosticSource for ManagedTranslationCheckpointError<E>
where
    E: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::DatabaseNotFound { database_path } => SafeDiagnostic::new(
                DiagnosticCode::ProjectUnavailable,
                stage,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component("project_database")),
            Self::NotCommitted {
                database_path,
                source,
            } => managed_translation_storage_source_diagnostic(
                source,
                database_path,
                stage,
                impact,
                fallback_action,
            )
            .with_recovery(RecoveryFact::transaction("rolled_back")),
        }
    }
}

fn managed_translation_storage_source_diagnostic<S>(
    source: &S,
    database_path: &std::path::Path,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    action: DiagnosticAction,
) -> SafeDiagnostic
where
    S: SafeDiagnosticSource,
{
    let mut diagnostic = source.safe_diagnostic_source(stage, impact, action);
    diagnostic.stage = stage;
    diagnostic.subject = DiagnosticSubject::path(database_path);
    diagnostic.impact = impact;
    diagnostic.action = action;
    diagnostic
}

fn managed_translation_stale_diagnostic(
    database_path: &std::path::Path,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    reason: &'static str,
    expected: &SourceSnapshotFingerprint,
    actual: &SourceSnapshotFingerprint,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::ProjectState,
        stage,
        DiagnosticSubject::path(database_path),
        DiagnosticReason::failure_with_detail(
            DiagnosticFailureKind::StateMismatch,
            format!(
                "reason={reason}; expected_source_fingerprint={}; actual_source_fingerprint={}",
                expected.hex(),
                actual.hex()
            ),
        ),
        impact,
        DiagnosticAction::CheckProjectState,
    )
    .with_recovery(RecoveryFact::component(
        "rerun_extract_for_managed_translations",
    ))
}

#[derive(Debug)]
pub(crate) enum StoredSnapshotError {
    WrongQueryResultCount {
        expected: usize,
        actual: usize,
    },
    WrongRowCount {
        subject: &'static str,
        expected: usize,
        actual: usize,
    },
    WrongColumnCount {
        subject: &'static str,
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    InvalidFingerprint {
        column: &'static str,
        actual: usize,
    },
    ProjectSourceChanged {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    OwnerSourceStale {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    OrphanRowsWithoutOwner,
    InvalidOrder {
        column: &'static str,
        expected: usize,
        actual: i64,
    },
    UnknownCollection(String),
    InvalidShape(String),
    InvalidContentJson {
        column: &'static str,
    },
    InvalidTranslationPair,
    Model(ManagedTranslationModelError),
}

impl fmt::Display for StoredSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongQueryResultCount { expected, actual } => {
                write!(
                    formatter,
                    "读取结果应包含 {expected} 组查询，实际为 {actual}"
                )
            }
            Self::WrongRowCount {
                subject,
                expected,
                actual,
            } => write!(formatter, "{subject} 应包含 {expected} 行，实际为 {actual}"),
            Self::WrongColumnCount {
                subject,
                expected,
                actual,
            } => write!(
                formatter,
                "{subject} 行应包含 {expected} 列，实际为 {actual}"
            ),
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "{column} 应为 {expected}，实际为 {actual}"),
            Self::InvalidFingerprint { column, actual } => {
                write!(formatter, "{column} 应为 32 字节指纹，实际为 {actual} 字节")
            }
            Self::ProjectSourceChanged { expected, actual } => write!(
                formatter,
                "项目来源指纹发生变化：expected={expected:?}, actual={actual:?}"
            ),
            Self::OwnerSourceStale { expected, actual } => write!(
                formatter,
                "托管翻译 owner 来源已过期：expected={expected:?}, actual={actual:?}"
            ),
            Self::OrphanRowsWithoutOwner => {
                formatter.write_str("没有 active owner 时仍存在 collection 或 unit")
            }
            Self::InvalidOrder {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "{column} 必须从 0 连续：expected={expected}, actual={actual}"
            ),
            Self::UnknownCollection(name) => {
                write!(formatter, "unit 引用了未知 collection：{name}")
            }
            Self::InvalidShape(shape) => write!(formatter, "未知托管翻译 shape：{shape}"),
            Self::InvalidContentJson { column } => {
                write!(formatter, "{column} 不是规范字符串或字符串数组 JSON")
            }
            Self::InvalidTranslationPair => {
                formatter.write_str("译文 JSON 与 translation_state 必须成对")
            }
            Self::Model(source) => source.fmt(formatter),
        }
    }
}

impl Error for StoredSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Model(source) => Some(source),
            _ => None,
        }
    }
}

impl StoredSnapshotError {
    fn safe_diagnostic_detail(&self) -> &'static str {
        match self {
            Self::WrongQueryResultCount { .. } => "wrong_query_result_count",
            Self::WrongRowCount { .. } => "wrong_row_count",
            Self::WrongColumnCount { .. } => "wrong_column_count",
            Self::WrongColumnType { .. } => "wrong_column_type",
            Self::InvalidFingerprint { .. } => "invalid_fingerprint",
            Self::ProjectSourceChanged { .. } => "project_source_changed",
            Self::OwnerSourceStale { .. } => "managed_owner_source_stale",
            Self::OrphanRowsWithoutOwner => "orphan_rows_without_owner",
            Self::InvalidOrder { .. } => "invalid_natural_order",
            Self::UnknownCollection(_) => "unknown_collection",
            Self::InvalidShape(_) => "invalid_shape",
            Self::InvalidContentJson { .. } => "invalid_content_json",
            Self::InvalidTranslationPair => "invalid_translation_pair",
            Self::Model(_) => "invalid_managed_translation_model",
        }
    }
}

fn decode_snapshot(
    results: Vec<Vec<SqliteRow>>,
    expected_project_source: SourceSnapshotFingerprint,
) -> Result<Option<ManagedTranslationSnapshot>, StoredSnapshotError> {
    let actual = results.len();
    let [metadata_rows, owner_rows, collection_rows, unit_rows] =
        results
            .try_into()
            .map_err(|_| StoredSnapshotError::WrongQueryResultCount {
                expected: 4,
                actual,
            })?;
    let metadata_source = SourceSnapshotFingerprint::from_bytes(
        decode_single_fingerprint(
            metadata_rows,
            "metadata",
            "metadata.source_snapshot_fingerprint",
        )?
        .into_bytes(),
    );
    if metadata_source != expected_project_source {
        return Err(StoredSnapshotError::ProjectSourceChanged {
            expected: expected_project_source,
            actual: metadata_source,
        });
    }
    if owner_rows.is_empty() {
        if collection_rows.is_empty() && unit_rows.is_empty() {
            return Ok(None);
        }
        return Err(StoredSnapshotError::OrphanRowsWithoutOwner);
    }
    if owner_rows.len() != 1 {
        return Err(StoredSnapshotError::WrongRowCount {
            subject: "managed owner",
            expected: 1,
            actual: owner_rows.len(),
        });
    }
    let owner = owner_rows
        .into_iter()
        .next()
        .expect("已确认 owner 恰好一行");
    let values = owner.into_values();
    if values.len() != 2 {
        return Err(StoredSnapshotError::WrongColumnCount {
            subject: "managed owner",
            expected: 2,
            actual: values.len(),
        });
    }
    let mut values = values.into_iter();
    let source_snapshot_fingerprint = SourceSnapshotFingerprint::from_bytes(
        decode_fingerprint(
            values.next().expect("已确认 owner 有两列"),
            "managed_translation_owner_state.source_snapshot_fingerprint",
        )?
        .into_bytes(),
    );
    if source_snapshot_fingerprint != expected_project_source {
        return Err(StoredSnapshotError::OwnerSourceStale {
            expected: expected_project_source,
            actual: source_snapshot_fingerprint,
        });
    }
    let manifest_fingerprint = ManagedTranslationManifestFingerprint::from_bytes(
        decode_fingerprint(
            values.next().expect("已确认 owner 有两列"),
            "managed_translation_owner_state.manifest_fingerprint",
        )?
        .into_bytes(),
    );

    let mut decoded_collections = Vec::with_capacity(collection_rows.len());
    let mut collection_indexes = HashMap::with_capacity(collection_rows.len());
    for (expected_order, row) in collection_rows.into_iter().enumerate() {
        let values = row.into_values();
        if values.len() != 3 {
            return Err(StoredSnapshotError::WrongColumnCount {
                subject: "managed collection",
                expected: 3,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let name = decode_text(
            values.next().expect("已确认 collection 有三列"),
            "collection_name",
        )?;
        let actual_order = decode_order(
            values.next().expect("已确认 collection 有三列"),
            "collection_order",
        )?;
        if actual_order != expected_order {
            return Err(StoredSnapshotError::InvalidOrder {
                column: "collection_order",
                expected: expected_order,
                actual: i64::try_from(actual_order).unwrap_or(i64::MAX),
            });
        }
        let instruction = decode_text(
            values.next().expect("已确认 collection 有三列"),
            "instruction",
        )?;
        collection_indexes.insert(name.clone(), decoded_collections.len());
        decoded_collections.push((name, instruction, Vec::new()));
    }

    let mut next_unit_order = vec![0usize; decoded_collections.len()];
    let mut unit_identities = HashSet::with_capacity(unit_rows.len());
    for row in unit_rows {
        let values = row.into_values();
        if values.len() != 10 {
            return Err(StoredSnapshotError::WrongColumnCount {
                subject: "managed unit",
                expected: 10,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let collection_name = decode_text(
            values.next().expect("已确认 unit 有十列"),
            "collection_name",
        )?;
        let Some(collection_index) = collection_indexes.get(&collection_name).copied() else {
            return Err(StoredSnapshotError::UnknownCollection(collection_name));
        };
        let key = decode_text(values.next().expect("已确认 unit 有十列"), "unit_key")?;
        if !unit_identities.insert((collection_name.clone(), key.clone())) {
            return Err(StoredSnapshotError::Model(
                ManagedTranslationModelError::DuplicateUnitKey {
                    collection: collection_name,
                    key,
                },
            ));
        }
        let actual_order = decode_order(values.next().expect("已确认 unit 有十列"), "unit_order")?;
        let expected_order = next_unit_order[collection_index];
        if actual_order != expected_order {
            return Err(StoredSnapshotError::InvalidOrder {
                column: "unit_order",
                expected: expected_order,
                actual: i64::try_from(actual_order).unwrap_or(i64::MAX),
            });
        }
        next_unit_order[collection_index] += 1;
        let kind = decode_text(values.next().expect("已确认 unit 有十列"), "kind")?;
        let shape_text = decode_text(values.next().expect("已确认 unit 有十列"), "shape")?;
        let shape = ManagedTranslationShape::from_storage_name(&shape_text)
            .ok_or(StoredSnapshotError::InvalidShape(shape_text))?;
        let original_json = decode_text(
            values.next().expect("已确认 unit 有十列"),
            "original_content_json",
        )?;
        let original = decode_content(&original_json, "original_content_json")?;
        let context = decode_text(values.next().expect("已确认 unit 有十列"), "context")?;
        let metadata =
            decode_optional_text(values.next().expect("已确认 unit 有十列"), "metadata_json")?
                .map(ManagedTranslationMetadata::from_canonical_json)
                .transpose()
                .map_err(StoredSnapshotError::Model)?;
        let translation_json = decode_optional_text(
            values.next().expect("已确认 unit 有十列"),
            "translation_content_json",
        )?;
        let translation_state = decode_optional_fingerprint(
            values.next().expect("已确认 unit 有十列"),
            "translation_state",
        )?;
        let translation = match (translation_json, translation_state) {
            (None, None) => None,
            (Some(content), Some(state)) => Some(ManagedTranslationPair::new_trusted(
                decode_content(&content, "translation_content_json")?,
                state,
            )),
            _ => return Err(StoredSnapshotError::InvalidTranslationPair),
        };
        let unit = ManagedTranslationUnit::new(key, kind, shape, original, context, metadata)
            .and_then(|unit| unit.with_stored_translation(translation))
            .map_err(StoredSnapshotError::Model)?;
        decoded_collections[collection_index].2.push(unit);
    }

    let collections = decoded_collections
        .into_iter()
        .map(|(name, instruction, units)| {
            ManagedTranslationCollection::new(name, instruction, units)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoredSnapshotError::Model)?;
    ManagedTranslationSnapshot::from_stored(
        source_snapshot_fingerprint,
        manifest_fingerprint,
        collections,
    )
    .map(Some)
    .map_err(StoredSnapshotError::Model)
}

fn decode_single_fingerprint(
    rows: Vec<SqliteRow>,
    subject: &'static str,
    column: &'static str,
) -> Result<Sha256Fingerprint, StoredSnapshotError> {
    if rows.len() != 1 {
        return Err(StoredSnapshotError::WrongRowCount {
            subject,
            expected: 1,
            actual: rows.len(),
        });
    }
    let row = rows.into_iter().next().expect("已确认恰好一行");
    let values = row.into_values();
    if values.len() != 1 {
        return Err(StoredSnapshotError::WrongColumnCount {
            subject,
            expected: 1,
            actual: values.len(),
        });
    }
    decode_fingerprint(values.into_iter().next().expect("已确认恰好一列"), column)
}

fn decode_fingerprint(
    value: SqliteValue,
    column: &'static str,
) -> Result<Sha256Fingerprint, StoredSnapshotError> {
    let SqliteValue::Blob(bytes) = value else {
        return Err(StoredSnapshotError::WrongColumnType {
            column,
            expected: "BLOB",
            actual: value.kind_name(),
        });
    };
    Sha256Fingerprint::from_slice(&bytes).map_err(|error| StoredSnapshotError::InvalidFingerprint {
        column,
        actual: error.actual(),
    })
}

fn decode_optional_fingerprint(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<Sha256Fingerprint>, StoredSnapshotError> {
    match value {
        SqliteValue::Null => Ok(None),
        value => decode_fingerprint(value, column).map(Some),
    }
}

fn decode_text(value: SqliteValue, column: &'static str) -> Result<String, StoredSnapshotError> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(StoredSnapshotError::WrongColumnType {
            column,
            expected: "TEXT",
            actual: value.kind_name(),
        }),
    }
}

fn decode_optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, StoredSnapshotError> {
    match value {
        SqliteValue::Null => Ok(None),
        value => decode_text(value, column).map(Some),
    }
}

fn decode_order(value: SqliteValue, column: &'static str) -> Result<usize, StoredSnapshotError> {
    let SqliteValue::Integer(value) = value else {
        return Err(StoredSnapshotError::WrongColumnType {
            column,
            expected: "INTEGER",
            actual: value.kind_name(),
        });
    };
    usize::try_from(value).map_err(|_| StoredSnapshotError::InvalidOrder {
        column,
        expected: 0,
        actual: value,
    })
}

fn decode_content(
    source: &str,
    column: &'static str,
) -> Result<ManagedTranslationContent, StoredSnapshotError> {
    let value = serde_json::from_str::<Value>(source)
        .map_err(|_| StoredSnapshotError::InvalidContentJson { column })?;
    let content = match value {
        Value::String(value) => ManagedTranslationContent::Scalar(value),
        Value::Array(values) => ManagedTranslationContent::Array(
            values
                .into_iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value),
                    _ => Err(StoredSnapshotError::InvalidContentJson { column }),
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err(StoredSnapshotError::InvalidContentJson { column }),
    };
    if content.canonical_json() != source {
        return Err(StoredSnapshotError::InvalidContentJson { column });
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::sync::{Arc, Mutex};

    use rusqlite::types::Value as RusqliteValue;
    use rusqlite::{Connection, params_from_iter};

    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::project::test_layout_profile;

    use super::*;

    #[derive(Debug)]
    struct TestConnectionError(String);

    impl fmt::Display for TestConnectionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl Error for TestConnectionError {}

    #[derive(Clone)]
    struct SharedTestConnection {
        connection: Arc<Mutex<Connection>>,
    }

    impl SharedTestConnection {
        fn new(connection: Connection) -> Self {
            Self {
                connection: Arc::new(Mutex::new(connection)),
            }
        }
    }

    impl SqliteQueryExecutor for SharedTestConnection {
        type Error = TestConnectionError;

        fn query_existing_database(
            &self,
            _path: PathBuf,
            _query: SqliteQuery,
        ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
        {
            future::ready(Err(QueryExistingDatabaseError::QueryFailed(
                TestConnectionError("本测试不应读取仓库".to_owned()),
            )))
        }

        fn query_existing_database_snapshot(
            &self,
            _path: PathBuf,
            _queries: Vec<SqliteQuery>,
        ) -> impl Future<
            Output = Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>>,
        > + Send {
            future::ready(Err(QueryExistingDatabaseError::QueryFailed(
                TestConnectionError("本测试不应读取仓库".to_owned()),
            )))
        }
    }

    impl SqliteTransactionExecutor for SharedTestConnection {
        type Error = TestConnectionError;

        fn execute_transaction(
            &self,
            _path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            let result = execute_plan(
                &mut self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                plan.steps(),
            )
            .map_err(|message| {
                if message == "requirement failed" || message == "exactly one failed" {
                    ExecuteTransactionError::RequirementFailed
                } else {
                    ExecuteTransactionError::NotCommitted(TestConnectionError(message))
                }
            });
            future::ready(result)
        }
    }

    fn source(value: u8) -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([value; 32])
    }

    fn unit(key: &str, context: &str, metadata: Option<&str>) -> ManagedTranslationUnit {
        ManagedTranslationUnit::new(
            key,
            "plugin_parameter",
            ManagedTranslationShape::Single,
            ManagedTranslationContent::scalar("原文"),
            context,
            metadata.map(|value| {
                ManagedTranslationMetadata::from_canonical_json(value)
                    .expect("测试 metadata 应合法")
            }),
        )
        .expect("测试 unit 应合法")
    }

    fn snapshot(
        source: SourceSnapshotFingerprint,
        instruction: &str,
        context: &str,
        metadata: Option<&str>,
    ) -> ManagedTranslationSnapshot {
        ManagedTranslationSnapshot::new(
            source,
            vec![
                ManagedTranslationCollection::new(
                    "quests",
                    instruction,
                    vec![unit("q:1", context, metadata)],
                )
                .expect("测试 collection 应合法"),
            ],
        )
        .expect("测试 snapshot 应合法")
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "managed-store-tests"
                .parse::<ProjectName>()
                .expect("项目名应合法"),
            PathBuf::from("C:/projects/managed-store-tests"),
            PathBuf::from("C:/projects/managed-store-tests/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        )
    }

    fn create_schema(connection: &Connection, source: SourceSnapshotFingerprint) {
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("测试必须启用外键");
        connection
            .execute_batch(
                r#"CREATE TABLE metadata (
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE managed_translation_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY CHECK (owner = 'lua'),
                    source_snapshot_fingerprint BLOB NOT NULL,
                    manifest_fingerprint BLOB NOT NULL
                );
                CREATE TABLE managed_translation_collection (
                    owner TEXT NOT NULL,
                    collection_name TEXT NOT NULL,
                    collection_order INTEGER NOT NULL,
                    instruction TEXT NOT NULL,
                    PRIMARY KEY (owner, collection_name),
                    UNIQUE (owner, collection_order),
                    FOREIGN KEY (owner)
                        REFERENCES managed_translation_owner_state(owner) ON DELETE CASCADE
                );
                CREATE TABLE managed_translation_unit (
                    owner TEXT NOT NULL,
                    collection_name TEXT NOT NULL,
                    unit_key TEXT NOT NULL,
                    unit_order INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    shape TEXT NOT NULL,
                    original_content_json TEXT NOT NULL,
                    context TEXT NOT NULL,
                    metadata_json TEXT,
                    translation_content_json TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, collection_name, unit_key),
                    UNIQUE (owner, collection_name, unit_order),
                    FOREIGN KEY (owner, collection_name)
                        REFERENCES managed_translation_collection(owner, collection_name)
                        ON DELETE CASCADE
                );"#,
            )
            .expect("测试 schema 应可创建");
        connection
            .execute(
                "INSERT INTO metadata VALUES (?1)",
                [source.as_bytes().to_vec()],
            )
            .expect("metadata 应可写入");
    }

    fn execute_plan(
        connection: &mut Connection,
        steps: &[SqliteTransactionStep],
    ) -> Result<(), String> {
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        for step in steps {
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
                    let Some((prefix, width, values)) = batch.bulk_insert_spec() else {
                        return Err("测试只预期 bulk ExecuteMany".to_owned());
                    };
                    for values in values.chunks_exact(width) {
                        let placeholders = (1..=width)
                            .map(|index| format!("?{index}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let statement = format!("{prefix} VALUES ({placeholders})");
                        transaction
                            .execute(
                                &statement,
                                params_from_iter(values.iter().map(to_rusqlite_value)),
                            )
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
                        return Err("requirement failed".to_owned());
                    }
                }
                SqliteTransactionStep::RequireNoRowsMany(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .map_err(|error| error.to_string())?;
                    for parameters in batch.parameter_rows() {
                        let mut rows = statement
                            .query(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .map_err(|error| error.to_string())?;
                        if rows.next().map_err(|error| error.to_string())?.is_some() {
                            return Err("requirement failed".to_owned());
                        }
                    }
                }
                SqliteTransactionStep::ExecuteManyExactlyOne(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .map_err(|error| error.to_string())?;
                    for parameters in batch.parameter_rows() {
                        let changed = statement
                            .execute(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .map_err(|error| error.to_string())?;
                        if changed != 1 {
                            return Err("exactly one failed".to_owned());
                        }
                    }
                }
                SqliteTransactionStep::RequireNoRowsReturningFirstRow(_) => {
                    return Err("测试不预期诊断行 guard".to_owned());
                }
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    fn read_rows(connection: &Connection, query: &str) -> Vec<SqliteRow> {
        let mut statement = connection.prepare(query).expect("测试查询应可准备");
        let column_count = statement.column_count();
        statement
            .query_map([], |row| {
                let values = (0..column_count)
                    .map(|index| {
                        row.get::<_, RusqliteValue>(index)
                            .map(sqlite_value_from_rusqlite)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SqliteRow::new(values))
            })
            .expect("测试查询应可执行")
            .map(|row| row.expect("测试行应可读取"))
            .collect()
    }

    fn load_direct(
        connection: &Connection,
        expected_source: SourceSnapshotFingerprint,
    ) -> Option<ManagedTranslationSnapshot> {
        decode_snapshot(
            vec![
                read_rows(connection, READ_METADATA_SOURCE),
                read_rows(connection, READ_OWNER_STATE),
                read_rows(connection, READ_COLLECTIONS),
                read_rows(connection, READ_UNITS),
            ],
            expected_source,
        )
        .expect("真实 SQLite 快照应可解码")
    }

    fn semantic_unit(
        key: &str,
        kind: &str,
        shape: ManagedTranslationShape,
        original: ManagedTranslationContent,
        context: &str,
        metadata: Option<&str>,
    ) -> ManagedTranslationUnit {
        ManagedTranslationUnit::new(
            key,
            kind,
            shape,
            original,
            context,
            metadata.map(|value| {
                ManagedTranslationMetadata::from_canonical_json(value)
                    .expect("测试 metadata 应合法")
            }),
        )
        .expect("测试 unit 应合法")
    }

    fn semantic_snapshot(
        collection_name: &str,
        instruction: &str,
        unit: ManagedTranslationUnit,
    ) -> ManagedTranslationSnapshot {
        ManagedTranslationSnapshot::new(
            source(1),
            vec![
                ManagedTranslationCollection::new(collection_name, instruction, vec![unit])
                    .expect("测试 collection 应合法"),
            ],
        )
        .expect("测试 snapshot 应合法")
    }

    fn replacement_retains_seeded_pair(
        replacement: &ManagedTranslationSnapshot,
        collection_name: &str,
        key: &str,
    ) -> Option<ManagedTranslationPair> {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(1));
        let initial = snapshot(source(1), "翻译标题", "菜单", Some(r#"{"order":1}"#));
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(&initial).steps(),
        )
        .expect("初始快照应写入");
        let pair = initial
            .collection("quests")
            .expect("初始 collection 应存在")
            .unit("q:1")
            .expect("初始 unit 应存在")
            .translation_pair(
                ManagedTranslationContent::scalar("译文"),
                Sha256Fingerprint::from_bytes([9; 32]),
            )
            .expect("测试译文应合法");
        let checkpoint = ManagedTranslationCheckpoint::new(
            &initial,
            vec![super::super::ManagedTranslationReplacement::new(
                "quests",
                "q:1",
                Some(pair),
            )],
        )
        .expect("checkpoint 应合法");
        execute_plan(&mut connection, checkpoint_plan(checkpoint).steps())
            .expect("译文应 checkpoint");
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(replacement).steps(),
        )
        .expect("替换快照应提交");

        load_direct(&connection, source(1))
            .expect("替换后的 active 快照应存在")
            .collection(collection_name)
            .expect("替换后的 collection 应存在")
            .unit(key)
            .expect("替换后的 unit 应存在")
            .translation()
            .cloned()
    }

    #[test]
    fn replacement_preserves_pairs_across_metadata_and_order_only_changes() {
        let changed_metadata = semantic_snapshot(
            "quests",
            "翻译标题",
            semantic_unit(
                "q:1",
                "plugin_parameter",
                ManagedTranslationShape::Single,
                ManagedTranslationContent::scalar("原文"),
                "菜单",
                Some(r#"{"order":2}"#),
            ),
        );
        let preserved = replacement_retains_seeded_pair(&changed_metadata, "quests", "q:1")
            .expect("metadata 不属于翻译语义，必须继承译文");
        assert_eq!(
            preserved.content(),
            &ManagedTranslationContent::scalar("译文")
        );
        assert_eq!(preserved.state(), Sha256Fingerprint::from_bytes([9; 32]));

        let changed_order = ManagedTranslationSnapshot::new(
            source(1),
            vec![
                ManagedTranslationCollection::new("other", "", Vec::new())
                    .expect("前置 collection 应合法"),
                ManagedTranslationCollection::new(
                    "quests",
                    "翻译标题",
                    vec![
                        unit("new-first", "", None),
                        unit("q:1", "菜单", Some(r#"{"order":1}"#)),
                    ],
                )
                .expect("更新快照应合法"),
            ],
        )
        .expect("更新快照应合法");
        assert!(
            replacement_retains_seeded_pair(&changed_order, "quests", "q:1").is_some(),
            "collection 与 unit 的自然顺序不属于翻译语义，必须继承译文"
        );
    }

    #[test]
    fn replacement_invalidates_pairs_for_each_changed_semantic_identity_or_field() {
        let cases = [
            (
                "collection name",
                semantic_snapshot(
                    "renamed",
                    "翻译标题",
                    semantic_unit(
                        "q:1",
                        "plugin_parameter",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "renamed",
                "q:1",
            ),
            (
                "unit key",
                semantic_snapshot(
                    "quests",
                    "翻译标题",
                    semantic_unit(
                        "renamed",
                        "plugin_parameter",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "renamed",
            ),
            (
                "instruction",
                semantic_snapshot(
                    "quests",
                    "翻译任务标题",
                    semantic_unit(
                        "q:1",
                        "plugin_parameter",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "q:1",
            ),
            (
                "kind",
                semantic_snapshot(
                    "quests",
                    "翻译标题",
                    semantic_unit(
                        "q:1",
                        "database_entry",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "q:1",
            ),
            (
                "shape",
                semantic_snapshot(
                    "quests",
                    "翻译标题",
                    semantic_unit(
                        "q:1",
                        "plugin_parameter",
                        ManagedTranslationShape::Reflow,
                        ManagedTranslationContent::scalar("原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "q:1",
            ),
            (
                "original",
                semantic_snapshot(
                    "quests",
                    "翻译标题",
                    semantic_unit(
                        "q:1",
                        "plugin_parameter",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("别的原文"),
                        "菜单",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "q:1",
            ),
            (
                "context",
                semantic_snapshot(
                    "quests",
                    "翻译标题",
                    semantic_unit(
                        "q:1",
                        "plugin_parameter",
                        ManagedTranslationShape::Single,
                        ManagedTranslationContent::scalar("原文"),
                        "战斗",
                        Some(r#"{"order":1}"#),
                    ),
                ),
                "quests",
                "q:1",
            ),
        ];

        for (changed_field, replacement, collection_name, key) in cases {
            assert!(
                replacement_retains_seeded_pair(&replacement, collection_name, key).is_none(),
                "{changed_field} 变化必须使旧译文失效"
            );
        }
    }

    #[test]
    fn active_empty_snapshot_and_deactivation_are_distinct() {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(2));
        let empty = ManagedTranslationSnapshot::new(source(2), Vec::new()).expect("空快照应合法");
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(&empty).steps(),
        )
        .expect("active 空快照应写入 owner");
        assert_eq!(
            load_direct(&connection, source(2))
                .expect("active 空快照应存在")
                .collections()
                .len(),
            0
        );

        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::deactivate(source(2)).steps(),
        )
        .expect("deactivate 应删除 owner");
        assert!(load_direct(&connection, source(2)).is_none());
    }

    #[test]
    fn stale_checkpoint_rolls_back_the_complete_batch() {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(3));
        let snapshot = ManagedTranslationSnapshot::new(
            source(3),
            vec![
                ManagedTranslationCollection::new(
                    "quests",
                    "",
                    vec![unit("q:1", "", None), unit("q:2", "", None)],
                )
                .expect("测试集合应合法"),
            ],
        )
        .expect("测试快照应合法");
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(&snapshot).steps(),
        )
        .expect("快照应写入");

        let pair = |key: &str, value: &str, state: u8| {
            snapshot
                .collection("quests")
                .unwrap()
                .unit(key)
                .unwrap()
                .translation_pair(
                    ManagedTranslationContent::scalar(value),
                    Sha256Fingerprint::from_bytes([state; 32]),
                )
                .expect("译文应合法")
        };
        connection
            .execute(
                r#"UPDATE managed_translation_unit
SET translation_content_json = '"外部译文"', translation_state = ?1
WHERE collection_name = 'quests' AND unit_key = 'q:2'"#,
                [vec![8_u8; 32]],
            )
            .expect("外部并发 checkpoint 应模拟成功");
        let checkpoint = ManagedTranslationCheckpoint::new(
            &snapshot,
            vec![
                super::super::ManagedTranslationReplacement::new(
                    "quests",
                    "q:1",
                    Some(pair("q:1", "一", 4)),
                ),
                super::super::ManagedTranslationReplacement::new(
                    "quests",
                    "q:2",
                    Some(pair("q:2", "二", 5)),
                ),
            ],
        )
        .expect("批量 checkpoint 应合法");
        assert!(
            execute_plan(&mut connection, checkpoint_plan(checkpoint).steps()).is_err(),
            "第二个 unit 的旧 pair 已变化，整批必须 CAS 失败"
        );
        let q1: Option<String> = connection
            .query_row(
                "SELECT translation_content_json FROM managed_translation_unit WHERE unit_key = 'q:1'",
                [],
                |row| row.get(0),
            )
            .expect("q:1 应可读取");
        assert_eq!(q1, None, "首个写入也必须随事务回滚");
    }

    #[tokio::test]
    async fn guarded_checkpoint_rejects_every_concurrent_dependency_without_partial_write() {
        let project = project();
        let source = project.source_snapshot_fingerprint();
        let cases = [
            (
                "collection instruction",
                "UPDATE managed_translation_collection SET instruction = ?1 WHERE collection_name = 'quests'",
                RusqliteValue::Text("并发指令".to_owned()),
            ),
            (
                "unit kind",
                "UPDATE managed_translation_unit SET kind = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text("event_dialogue".to_owned()),
            ),
            (
                "unit shape",
                "UPDATE managed_translation_unit SET shape = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text("reflow".to_owned()),
            ),
            (
                "unit original",
                "UPDATE managed_translation_unit SET original_content_json = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text(r#""并发原文""#.to_owned()),
            ),
            (
                "unit context",
                "UPDATE managed_translation_unit SET context = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text("并发上下文".to_owned()),
            ),
            (
                "unit metadata",
                "UPDATE managed_translation_unit SET metadata_json = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text(r#"{"external":true}"#.to_owned()),
            ),
            (
                "translation content",
                "UPDATE managed_translation_unit SET translation_content_json = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Text(r#""外部译文""#.to_owned()),
            ),
            (
                "translation state",
                "UPDATE managed_translation_unit SET translation_state = ?1 WHERE unit_key = 'q:2'",
                RusqliteValue::Blob(vec![0x77; 32]),
            ),
        ];

        for (field, statement, concurrent_value) in cases {
            let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
            create_schema(&connection, source);
            let snapshot = ManagedTranslationSnapshot::new(
                source,
                vec![
                    ManagedTranslationCollection::new(
                        "quests",
                        "原指令",
                        vec![
                            unit("q:1", "原上下文", Some(r#"{"before":true}"#)),
                            unit("q:2", "原上下文", Some(r#"{"before":true}"#)),
                        ],
                    )
                    .expect("测试集合应合法"),
                ],
            )
            .expect("测试快照应合法");
            execute_plan(
                &mut connection,
                ManagedTranslationSnapshotMutation::replace(&snapshot).steps(),
            )
            .expect("初始快照应写入");

            let replacement = snapshot
                .collection("quests")
                .expect("集合应存在")
                .unit("q:1")
                .expect("q:1 应存在")
                .translation_pair(
                    ManagedTranslationContent::scalar("本轮译文"),
                    Sha256Fingerprint::from_bytes([0x44; 32]),
                )
                .expect("替换 pair 应合法");
            let checkpoint = ManagedTranslationCheckpoint::guarded(
                &snapshot,
                vec![super::super::ManagedTranslationReplacement::new(
                    "quests",
                    "q:1",
                    Some(replacement),
                )],
            )
            .expect("guarded checkpoint 应合法");

            connection
                .execute(statement, [concurrent_value])
                .unwrap_or_else(|error| panic!("{field} 并发改动应可模拟：{error}"));
            let sqlite = SharedTestConnection::new(connection);
            let repository = ManagedTranslationSqliteRepository::new(sqlite.clone());

            let outcome = repository
                .checkpoint(&project, checkpoint)
                .await
                .unwrap_or_else(|error| panic!("{field} checkpoint 应有明确终态：{error}"));
            assert!(
                matches!(outcome, ManagedTranslationCheckpointOutcome::NotApplied),
                "{field} 变化必须得到 NotApplied"
            );

            let stored_q1: Option<String> = sqlite
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .query_row(
                    "SELECT translation_content_json FROM managed_translation_unit WHERE unit_key = 'q:1'",
                    [],
                    |row| row.get(0),
                )
                .expect("q:1 应可读取");
            assert_eq!(
                stored_q1, None,
                "{field} guard 失败后不得提交同 checkpoint 的其他 replacement"
            );
        }
    }

    #[test]
    fn source_guard_prevents_replacement_after_project_changes() {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(4));
        let stale = snapshot(source(4), "", "", None);
        connection
            .execute(
                "UPDATE metadata SET source_snapshot_fingerprint = ?1",
                [source(5).as_bytes().to_vec()],
            )
            .expect("测试应可改变来源");
        assert!(
            execute_plan(
                &mut connection,
                ManagedTranslationSnapshotMutation::replace(&stale).steps()
            )
            .is_err()
        );
        assert!(read_rows(&connection, READ_OWNER_STATE).is_empty());
    }

    #[test]
    fn load_rejects_owner_from_an_earlier_extract_source() {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(4));
        let snapshot = snapshot(source(4), "", "", None);
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(&snapshot).steps(),
        )
        .expect("初始托管快照应写入");
        connection
            .execute(
                "UPDATE metadata SET source_snapshot_fingerprint = ?1",
                [source(5).as_bytes().to_vec()],
            )
            .expect("测试应可模拟来源更新但未重新 Extract");

        assert!(matches!(
            decode_snapshot(
                vec![
                    read_rows(&connection, READ_METADATA_SOURCE),
                    read_rows(&connection, READ_OWNER_STATE),
                    read_rows(&connection, READ_COLLECTIONS),
                    read_rows(&connection, READ_UNITS),
                ],
                source(5),
            ),
            Err(StoredSnapshotError::OwnerSourceStale {
                expected,
                actual,
            }) if expected == source(5) && actual == source(4)
        ));
    }

    #[test]
    fn stored_manifest_and_pair_are_strictly_revalidated() {
        let mut connection = Connection::open_in_memory().expect("内存数据库应可打开");
        create_schema(&connection, source(6));
        let snapshot = snapshot(source(6), "", "", None);
        execute_plan(
            &mut connection,
            ManagedTranslationSnapshotMutation::replace(&snapshot).steps(),
        )
        .expect("快照应写入");
        connection
            .execute(
                "UPDATE managed_translation_owner_state SET manifest_fingerprint = ?1",
                [vec![0xff_u8; 32]],
            )
            .expect("测试应可破坏 manifest");
        assert!(matches!(
            decode_snapshot(
                vec![
                    read_rows(&connection, READ_METADATA_SOURCE),
                    read_rows(&connection, READ_OWNER_STATE),
                    read_rows(&connection, READ_COLLECTIONS),
                    read_rows(&connection, READ_UNITS),
                ],
                source(6)
            ),
            Err(StoredSnapshotError::Model(
                ManagedTranslationModelError::ManifestFingerprintMismatch { .. }
            ))
        ));
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

    fn sqlite_value_from_rusqlite(value: RusqliteValue) -> SqliteValue {
        match value {
            RusqliteValue::Null => SqliteValue::Null,
            RusqliteValue::Integer(value) => SqliteValue::Integer(value),
            RusqliteValue::Real(value) => SqliteValue::Real(value),
            RusqliteValue::Text(value) => SqliteValue::Text(value),
            RusqliteValue::Blob(value) => SqliteValue::Blob(value),
        }
    }
}
