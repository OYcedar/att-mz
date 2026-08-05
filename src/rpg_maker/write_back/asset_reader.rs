//! 从 RPG Maker 文本资产表建立写回快照。

use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, ReportedFailure, RpgMakerClaimSummaryMismatchDetails,
    RpgMakerClaimSummaryMismatchKind, RpgMakerComputeFailure, RpgMakerIssue,
    RpgMakerJsonFailureKind, RpgMakerMutationAccess, RpgMakerSemanticOrderLevel,
    RpgMakerWriteBackAssetComputeOperation, RpgMakerWriteBackAssetProblem,
    RpgMakerWriteBackAssetSnapshotViolation, RpgMakerWriteBackModelViolation, SafeIdentifier,
    SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteOperation, SqliteTransactionState,
    StateEffect, TranslationIssue, TranslationJsonFailureKind, TranslationPlanningResourceKind,
    TranslationPlanningResourceOrigin, TranslationPlanningResourceProblem,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::{RpgMakerAssetOwner, RpgMakerTextSnapshotFingerprintBuilder};
use crate::rpg_maker::asset_storage::{
    OwnerPartitionedSqliteRow, RPG_MAKER_ASSET_OWNER_ORDER as RPG_MAKER_WRITE_BACK_OWNER_ORDER,
    RPG_MAKER_ASSET_OWNER_STATE_PROJECTION, RPG_MAKER_TEXT_GROUP_CORE_PROJECTION,
    RPG_MAKER_TEXT_UNIT_CONTENT_PROJECTION, RpgMakerAssetOwnerStateStorageRow,
    RpgMakerAssetStorageRowDecoder, RpgMakerAssetStorageRowError, RpgMakerTextGroupStorageRow,
    RpgMakerTextUnitStorageRow, merge_owner_partitions, rpg_maker_asset_owner_order,
    rpg_maker_asset_owner_order_sql, sort_owner_state_rows,
};
use crate::rpg_maker::dialogue::MvDialogueDefinitionError;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
#[cfg(test)]
use crate::rpg_maker::model::{MutationClaim, mutation_claims_for_group};
use crate::rpg_maker::model::{
    MutationResourceAccess, TextProjectionRecipe, TextUnitContent, TextUnitRole,
};
use crate::rpg_maker::mutation_claim_summary::{
    EncodedMutationClaim, MutationClaimSummaryError, collision_summary, sort_logical_claims,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::semantic_order::{
    RpgMakerSemanticOrderKey, RpgMakerSemanticOrderKeyDecodeError,
};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::rpg_maker::translate::placeholder::{
    Pcre2PlaceholderConstructionError, Pcre2PlaceholderService, PlaceholderRuleCompilationError,
    PlaceholderRuleDefinition,
};
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::runtime::sqlite::SqliteRuntimeError;
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::planner::{
    RpgMakerWriteBackAssetReader, RpgMakerWriteBackGroup, RpgMakerWriteBackSnapshot,
    RpgMakerWriteBackSnapshotError, RpgMakerWriteBackSymbolRepairContext, RpgMakerWriteBackUnit,
};

const RPG_MAKER_WRITE_BACK_QUERY_RESULT_COUNT: usize =
    2 + RPG_MAKER_WRITE_BACK_OWNER_ORDER.len() * 3;

const READ_PLACEHOLDER_RULES: &str = r#"SELECT canonical_json
FROM rpg_maker_translation_resource
WHERE resource_kind = 'placeholder_rules'"#;

fn read_rpg_maker_write_back_owner_states() -> String {
    format!(
        "SELECT\n    {RPG_MAKER_ASSET_OWNER_STATE_PROJECTION}\n\
         FROM rpg_maker_asset_owner_state\n\
         ORDER BY {}",
        rpg_maker_asset_owner_order_sql("owner")
    )
}

fn read_rpg_maker_write_back_owner_groups() -> String {
    format!(
        "SELECT\n    {RPG_MAKER_TEXT_GROUP_CORE_PROJECTION},\n    \
         projection_recipe_json\n\
         FROM rpg_maker_text_group\n\
         WHERE owner = ?\n\
         ORDER BY semantic_order_key"
    )
}

fn read_rpg_maker_write_back_owner_units() -> String {
    format!(
        "SELECT\n    text_group.group_location,\n    \
         {RPG_MAKER_TEXT_UNIT_CONTENT_PROJECTION}\n\
         FROM rpg_maker_text_group AS text_group\n\
         CROSS JOIN rpg_maker_text_unit AS unit\n  \
                    INDEXED BY rpg_maker_text_unit_owner_group_order_idx\n  \
           ON unit.owner = text_group.owner\n \
          AND text_group.group_id = unit.group_id\n\
         WHERE text_group.owner = ?\n\
         ORDER BY text_group.semantic_order_key,\n         \
          unit.semantic_order_key"
    )
}

fn read_rpg_maker_write_back_owner_claims() -> &'static str {
    r#"SELECT
    text_group.group_location,
    claim.resource_key,
    claim.access
FROM rpg_maker_mutation_claim AS claim
     INDEXED BY rpg_maker_mutation_claim_owner_resource_idx
JOIN rpg_maker_text_group AS text_group
  ON text_group.owner = claim.owner
 AND text_group.group_id = claim.group_id
WHERE claim.owner = ?
ORDER BY claim.resource_key COLLATE BINARY,
         claim.access COLLATE BINARY,
         text_group.group_location COLLATE BINARY"#
}

fn rpg_maker_write_back_snapshot_queries() -> Vec<SqliteQuery> {
    let mut queries = Vec::with_capacity(RPG_MAKER_WRITE_BACK_QUERY_RESULT_COUNT);
    queries.push(
        SqliteQuery::new(read_rpg_maker_write_back_owner_states(), Vec::new())
            .with_id("write_back.owner_states"),
    );
    queries.push(
        SqliteQuery::new(READ_PLACEHOLDER_RULES, Vec::new())
            .with_id("write_back.placeholder_rules"),
    );
    for (kind, statement) in [
        ("groups", read_rpg_maker_write_back_owner_groups()),
        ("units", read_rpg_maker_write_back_owner_units()),
        (
            "claims",
            read_rpg_maker_write_back_owner_claims().to_owned(),
        ),
    ] {
        queries.extend(RPG_MAKER_WRITE_BACK_OWNER_ORDER.map(|owner| {
            SqliteQuery::new(
                statement.clone(),
                vec![SqliteValue::Text(owner.storage_name().to_owned())],
            )
            .with_id(format!("write_back.{}.{kind}", owner.storage_name()))
        }));
    }
    queries
}

fn unpack_snapshot_query_results(
    query_results: Vec<Vec<SqliteRow>>,
) -> Result<SnapshotRows, InvalidRpgMakerWriteBackAssetSnapshot> {
    let actual = query_results.len();
    if actual != RPG_MAKER_WRITE_BACK_QUERY_RESULT_COUNT {
        return Err(
            InvalidRpgMakerWriteBackAssetSnapshot::WrongQueryResultCount {
                expected: RPG_MAKER_WRITE_BACK_QUERY_RESULT_COUNT,
                actual,
            },
        );
    }
    let mut query_results = query_results.into_iter();
    let owners = query_results.next().expect("已验证快照查询结果数量");
    let placeholder_rules = decode_placeholder_rules_rows(
        query_results
            .next()
            .expect("已验证 Placeholder 查询结果存在"),
    )?;
    let mut next_owner_partitions = || {
        merge_owner_partitions([
            query_results.next().expect("已验证 Builtin 查询结果存在"),
            query_results.next().expect("已验证 Rules 查询结果存在"),
        ])
    };
    let groups = next_owner_partitions();
    let units = next_owner_partitions();
    let claims = next_owner_partitions();
    debug_assert!(query_results.next().is_none());
    Ok(SnapshotRows {
        owners,
        placeholder_rules,
        groups,
        units,
        claims,
    })
}

fn decode_placeholder_rules_rows(
    rows: Vec<SqliteRow>,
) -> Result<String, InvalidRpgMakerWriteBackAssetSnapshot> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidRpgMakerWriteBackAssetSnapshot::PlaceholderRuleRowCount { actual: rows.len() }
    })?;
    let [value] = <[SqliteValue; 1]>::try_from(row.into_values()).map_err(|values| {
        InvalidRpgMakerWriteBackAssetSnapshot::WrongColumnCount {
            expected: 1,
            actual: values.len(),
        }
    })?;
    match value {
        SqliteValue::Text(value) if !value.is_empty() => Ok(value),
        SqliteValue::Text(_) => Err(InvalidRpgMakerWriteBackAssetSnapshot::BlankPlaceholderRules),
        value => Err(InvalidRpgMakerWriteBackAssetSnapshot::WrongColumnType {
            column: "canonical_json",
            expected: "TEXT",
            actual: value.kind_name(),
        }),
    }
}

fn build_symbol_repair_context(
    engine: RpgMakerEngine,
    placeholder_rules_json: String,
) -> Result<RpgMakerWriteBackSymbolRepairContext, InvalidRpgMakerWriteBackAssetSnapshot> {
    let placeholder_definitions: Vec<PlaceholderRuleDefinition> =
        serde_json::from_str(&placeholder_rules_json)
            .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRulesJson)?;
    let placeholder_service = match Pcre2PlaceholderService::new_with_cancellation(|| {
        Ok::<_, Infallible>(())
    }) {
        Ok(Ok(service)) => service,
        Ok(Err(source)) => {
            return Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidBuiltinPlaceholder(source));
        }
        Err(unreachable) => match unreachable {},
    };
    let placeholder_rules = match placeholder_service
        .compile_custom_with_cancellation(placeholder_definitions, || Ok::<_, Infallible>(()))
    {
        Ok(Ok(rules)) => rules,
        Ok(Err(source)) => {
            return Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRules(source));
        }
        Err(unreachable) => match unreachable {},
    };
    Ok(RpgMakerWriteBackSymbolRepairContext::new(
        engine,
        placeholder_service,
        placeholder_rules,
        placeholder_rules_json,
    ))
}

/// 先验证 active owner 与资产指纹，再用受控 CPU 解码建立写回快照。
pub(crate) struct RpgMakerWriteBackAssetReadingService<Q, C> {
    sqlite: Arc<Q>,
    cpu: Arc<C>,
}

impl<Q, C> RpgMakerWriteBackAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C) -> Self {
        Self {
            sqlite: Arc::new(sqlite),
            cpu: Arc::new(cpu),
        }
    }
}

impl<Q, C> RpgMakerWriteBackAssetReader for RpgMakerWriteBackAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerWriteBackAssetReadingError<Q::Error, C::Error>;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl std::future::Future<Output = Result<RpgMakerWriteBackSnapshot, Self::Error>>
    + Send
    + use<Q, C> {
        let database_path = project.database_path().to_path_buf();
        let current_source = project.source_snapshot_fingerprint();
        let engine = project.layout().rpg_maker_layout().engine();
        let dialogue_definition = project.mv_dialogue_definition().clone();
        let sqlite = Arc::clone(&self.sqlite);
        let cpu = Arc::clone(&self.cpu);

        async move {
            let dialogue_definition_json =
                dialogue_definition.to_canonical_json().map_err(|source| {
                    RpgMakerWriteBackAssetReadingError::InvalidSnapshot {
                        database_path: database_path.clone(),
                        source: InvalidRpgMakerWriteBackAssetSnapshot::InvalidDialogueDefinition(
                            Box::new(source),
                        ),
                    }
                })?;
            let query_results = sqlite
                .query_existing_database_snapshot(
                    database_path.clone(),
                    rpg_maker_write_back_snapshot_queries(),
                )
                .await
                .map_err(|error| map_query_error(database_path.clone(), error))?;
            let rows = unpack_snapshot_query_results(query_results).map_err(|source| {
                RpgMakerWriteBackAssetReadingError::InvalidSnapshot {
                    database_path: database_path.clone(),
                    source,
                }
            })?;

            let prepared = cpu
                .execute(move || prepare_rows(rows, current_source))
                .await
                .map_err(
                    |source| RpgMakerWriteBackAssetReadingError::SchedulePreparation {
                        database_path: database_path.clone(),
                        source,
                    },
                )?
                .map_err(
                    |source| RpgMakerWriteBackAssetReadingError::InvalidSnapshot {
                        database_path: database_path.clone(),
                        source,
                    },
                )?;
            if !prepared.stale_owners.is_empty() {
                return Err(RpgMakerWriteBackAssetReadingError::ExtractionOutOfDate {
                    database_path: database_path.clone(),
                    owners: prepared.stale_owners,
                });
            }

            let decoded_records = cpu
                .execute_ordered_map(prepared.records, decode_record)
                .await
                .map_err(
                    |source| RpgMakerWriteBackAssetReadingError::ScheduleDecode {
                        database_path: database_path.clone(),
                        source,
                    },
                )?;

            let owner_states = prepared.owner_states;
            let placeholder_rules_json = prepared.placeholder_rules;
            cpu.execute(move || {
                let symbol_repair = build_symbol_repair_context(engine, placeholder_rules_json)?;
                let decoded = decoded_records.into_iter().collect::<Result<Vec<_>, _>>()?;
                assemble_snapshot(owner_states, decoded, &dialogue_definition_json)
                    .map(|snapshot| snapshot.with_symbol_repair(symbol_repair))
            })
            .await
            .map_err(
                |source| RpgMakerWriteBackAssetReadingError::ScheduleAssembly {
                    database_path: database_path.clone(),
                    source,
                },
            )?
            .map_err(
                |source| RpgMakerWriteBackAssetReadingError::InvalidSnapshot {
                    database_path,
                    source,
                },
            )
        }
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackAssetReadingError<Q, C> {
    DatabaseNotFound {
        database_path: PathBuf,
    },
    Query {
        database_path: PathBuf,
        source: Q,
    },
    ExtractionOutOfDate {
        database_path: PathBuf,
        owners: Vec<RpgMakerAssetOwner>,
    },
    SchedulePreparation {
        database_path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    ScheduleDecode {
        database_path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    ScheduleAssembly {
        database_path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    InvalidSnapshot {
        database_path: PathBuf,
        source: InvalidRpgMakerWriteBackAssetSnapshot,
    },
}

impl<Q: fmt::Display, C: fmt::Display> fmt::Display for RpgMakerWriteBackAssetReadingError<Q, C> {
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
                "无法从 {} 读取 RPG Maker 写回资产：{source}",
                database_path.display()
            ),
            Self::ExtractionOutOfDate { owners, .. } => write!(
                formatter,
                "RPG Maker 资产提取已过期：{}",
                owners
                    .iter()
                    .map(|owner| owner.storage_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::SchedulePreparation { source, .. } => {
                write!(formatter, "写回资产快照准备任务执行失败：{source}")
            }
            Self::ScheduleDecode { source, .. } => {
                write!(formatter, "写回资产解码任务执行失败：{source}")
            }
            Self::ScheduleAssembly { source, .. } => {
                write!(formatter, "写回资产快照组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot { source, .. } => {
                write!(formatter, "RPG Maker 写回资产损坏：{source}")
            }
        }
    }
}

impl<Q: Error + 'static, C: Error + 'static> Error for RpgMakerWriteBackAssetReadingError<Q, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::SchedulePreparation { source, .. }
            | Self::ScheduleDecode { source, .. }
            | Self::ScheduleAssembly { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            Self::DatabaseNotFound { .. } | Self::ExtractionOutOfDate { .. } => None,
        }
    }
}

impl RpgMakerWriteBackAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable> {
    /// 在仍掌握数据库路径、事务与快照叶子结构时建立唯一公开报告。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::DatabaseNotFound { database_path } => write_back_asset_report(
                database_path,
                RpgMakerWriteBackAssetProblem::DatabaseNotFound,
            ),
            Self::Query {
                database_path,
                source,
            } => source.diagnostic_report(
                database_path,
                SqliteDiagnosticContext::new(
                    SqliteDiagnosticStage::WriteBack,
                    SqliteOperation::Query,
                    write_back_snapshot_query_transaction(source),
                ),
                StateEffect::Unchanged,
            ),
            Self::ExtractionOutOfDate {
                database_path,
                owners,
            } => write_back_asset_report(
                database_path,
                RpgMakerWriteBackAssetProblem::ExtractionOutOfDate {
                    owners: owners
                        .iter()
                        .map(|owner| owner.diagnostic_owner())
                        .collect(),
                },
            ),
            Self::SchedulePreparation {
                database_path,
                source,
            } => write_back_asset_compute_report(
                database_path,
                RpgMakerWriteBackAssetComputeOperation::Prepare,
                source,
            ),
            Self::ScheduleDecode {
                database_path,
                source,
            } => write_back_asset_compute_report(
                database_path,
                RpgMakerWriteBackAssetComputeOperation::Decode,
                source,
            ),
            Self::ScheduleAssembly {
                database_path,
                source,
            } => write_back_asset_compute_report(
                database_path,
                RpgMakerWriteBackAssetComputeOperation::Assemble,
                source,
            ),
            Self::InvalidSnapshot {
                database_path,
                source,
            } => match source {
                InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRulesJson(source) => {
                    DiagnosticReport::new(
                        StateEffect::Unchanged,
                        Diagnostic::translation(TranslationIssue::PlanningResource {
                            resource: TranslationPlanningResourceKind::PlaceholderRules,
                            origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                            problem: TranslationPlanningResourceProblem::InvalidSnapshotJson {
                                category: translation_json_failure(source),
                                line: source.line(),
                                column: source.column(),
                            },
                        }),
                    )
                }
                InvalidRpgMakerWriteBackAssetSnapshot::InvalidBuiltinPlaceholder(source) => {
                    source.diagnostic_report()
                }
                InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRules(source) => {
                    DiagnosticReport::new(
                        StateEffect::Unchanged,
                        Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                            origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                            problem: source.diagnostic_problem(),
                        }),
                    )
                }
                _ => write_back_asset_report(
                    database_path,
                    RpgMakerWriteBackAssetProblem::InvalidSnapshot {
                        violation: source.diagnostic_violation(),
                    },
                ),
            },
        }
    }

    pub(crate) fn into_reported_failure(self) -> ReportedFailure {
        let report = self.diagnostic_report();
        ReportedFailure::new(report, self)
    }
}

fn translation_json_failure(source: &serde_json::Error) -> TranslationJsonFailureKind {
    match source.classify() {
        serde_json::error::Category::Io => TranslationJsonFailureKind::Io,
        serde_json::error::Category::Syntax => TranslationJsonFailureKind::Syntax,
        serde_json::error::Category::Data => TranslationJsonFailureKind::Data,
        serde_json::error::Category::Eof => TranslationJsonFailureKind::Eof,
    }
}

fn write_back_asset_report(
    database_path: &std::path::Path,
    problem: RpgMakerWriteBackAssetProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::write_back_asset(database_path, problem)),
    )
}

fn write_back_asset_compute_report(
    database_path: &std::path::Path,
    operation: RpgMakerWriteBackAssetComputeOperation,
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> DiagnosticReport {
    let failure = match source {
        CpuTaskExecutionError::Cancelled => RpgMakerComputeFailure::Cancelled,
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown) => {
            RpgMakerComputeFailure::ExecutorClosed
        }
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned) => {
            RpgMakerComputeFailure::StatePoisoned
        }
        CpuTaskExecutionError::TaskPanicked => RpgMakerComputeFailure::WorkerPanicked,
    };
    write_back_asset_report(
        database_path,
        RpgMakerWriteBackAssetProblem::Compute { operation, failure },
    )
}

fn write_back_snapshot_query_transaction(source: &SqliteRuntimeError) -> SqliteTransactionState {
    match source {
        SqliteRuntimeError::QueryContext { .. } => SqliteTransactionState::RolledBack,
        SqliteRuntimeError::Cleanup { .. }
        | SqliteRuntimeError::Driver { .. }
        | SqliteRuntimeError::Internal(_) => SqliteTransactionState::OutcomeUnknown,
        SqliteRuntimeError::Closed
        | SqliteRuntimeError::AvailableParallelism { .. }
        | SqliteRuntimeError::Cancelled { .. }
        | SqliteRuntimeError::InteractiveSessionAlreadyOpen
        | SqliteRuntimeError::WorkerSpawn { .. }
        | SqliteRuntimeError::WorkerPanicked(_)
        | SqliteRuntimeError::Io { .. }
        | SqliteRuntimeError::WindowsFileSystem { .. }
        | SqliteRuntimeError::InvalidTarget { .. }
        | SqliteRuntimeError::UnexpectedArtifact { .. }
        | SqliteRuntimeError::InvalidValue(_)
        | SqliteRuntimeError::BackupIncomplete(_) => SqliteTransactionState::NotStarted,
    }
}

#[derive(Debug)]
pub(crate) enum InvalidRpgMakerWriteBackAssetSnapshot {
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
    PlaceholderRuleRowCount {
        actual: usize,
    },
    BlankPlaceholderRules,
    InvalidPlaceholderRulesJson(serde_json::Error),
    InvalidBuiltinPlaceholder(Pcre2PlaceholderConstructionError),
    InvalidPlaceholderRules(PlaceholderRuleCompilationError),
    InvalidSemanticOrderKey {
        column: &'static str,
        source: RpgMakerSemanticOrderKeyDecodeError,
    },
    UnknownOwner,
    DuplicateOwner(RpgMakerAssetOwner),
    InvalidFingerprintLength {
        owner: RpgMakerAssetOwner,
        column: &'static str,
        actual: usize,
    },
    AssetWithoutOwner(RpgMakerAssetOwner),
    UnknownGroupKind,
    DuplicateGroup {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    MissingGroup {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    DuplicateSemanticOrderKey {
        owner: RpgMakerAssetOwner,
        level: WriteBackSnapshotSemanticOrderLevel,
    },
    UnknownMutationAccess,
    NonCanonicalMutationResource {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InvalidClaimSummary {
        owner: RpgMakerAssetOwner,
        row_index: usize,
        kind: ClaimSummaryMismatchKind,
        expected_rows: usize,
        actual_rows: usize,
        details: Box<ClaimSummaryMismatchDetails>,
    },
    AssetFingerprintMismatch {
        owner: RpgMakerAssetOwner,
    },
    InvalidDialogueDefinition(Box<MvDialogueDefinitionError>),
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidProjection(RpgMakerProjectionCodecError),
    InvalidUnitContent {
        column: &'static str,
        source: serde_json::Error,
    },
    InvalidModel(RpgMakerWriteBackSnapshotError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackSnapshotSemanticOrderLevel {
    Group,
    Unit,
}

impl WriteBackSnapshotSemanticOrderLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Unit => "unit",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClaimSummaryMismatchDetails {
    expected_group: Option<RpgMakerLocation>,
    actual_group: Option<RpgMakerLocation>,
    expected_resource: Option<RpgMakerLocation>,
    actual_resource: Option<RpgMakerLocation>,
    expected_access: Option<MutationResourceAccess>,
    actual_access: Option<MutationResourceAccess>,
}

impl InvalidRpgMakerWriteBackAssetSnapshot {
    fn diagnostic_violation(&self) -> RpgMakerWriteBackAssetSnapshotViolation {
        match self {
            Self::WrongQueryResultCount { expected, actual } => {
                RpgMakerWriteBackAssetSnapshotViolation::WrongQueryResultSetCount {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::WrongColumnCount { expected, actual } => {
                RpgMakerWriteBackAssetSnapshotViolation::WrongColumnCount {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => RpgMakerWriteBackAssetSnapshotViolation::WrongColumnType {
                column: SafeIdentifier::from_validated(*column),
                expected: SafeIdentifier::from_validated(*expected),
                actual: SafeIdentifier::from_validated(*actual),
            },
            Self::PlaceholderRuleRowCount { actual } => {
                RpgMakerWriteBackAssetSnapshotViolation::PlaceholderRuleRowCount {
                    expected: 1,
                    actual: *actual,
                }
            }
            Self::BlankPlaceholderRules => {
                RpgMakerWriteBackAssetSnapshotViolation::BlankPlaceholderRules
            }
            Self::InvalidPlaceholderRulesJson(_)
            | Self::InvalidBuiltinPlaceholder(_)
            | Self::InvalidPlaceholderRules(_) => {
                unreachable!("Placeholder 解析或编译错误由原始语义所有者建立诊断")
            }
            Self::InvalidSemanticOrderKey { column, source } => {
                RpgMakerWriteBackAssetSnapshotViolation::InvalidSemanticOrderKey {
                    column: SafeIdentifier::from_validated(*column),
                    violation: source.diagnostic_violation(),
                }
            }
            Self::UnknownOwner => RpgMakerWriteBackAssetSnapshotViolation::UnknownOwner,
            Self::DuplicateOwner(owner) => {
                RpgMakerWriteBackAssetSnapshotViolation::DuplicateOwner {
                    owner: owner.diagnostic_owner(),
                }
            }
            Self::InvalidFingerprintLength {
                owner,
                column,
                actual,
            } => RpgMakerWriteBackAssetSnapshotViolation::InvalidFingerprintLength {
                owner: owner.diagnostic_owner(),
                column: SafeIdentifier::from_validated(*column),
                expected: 32,
                actual: *actual,
            },
            Self::AssetWithoutOwner(owner) => {
                RpgMakerWriteBackAssetSnapshotViolation::AssetWithoutOwner {
                    owner: owner.diagnostic_owner(),
                }
            }
            Self::UnknownGroupKind => RpgMakerWriteBackAssetSnapshotViolation::UnknownGroupKind,
            Self::DuplicateGroup {
                owner,
                group_location,
            } => RpgMakerWriteBackAssetSnapshotViolation::DuplicateGroup {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::MissingGroup {
                owner,
                group_location,
            } => RpgMakerWriteBackAssetSnapshotViolation::MissingGroup {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::DuplicateSemanticOrderKey { owner, level } => {
                RpgMakerWriteBackAssetSnapshotViolation::DuplicateSemanticOrderKey {
                    owner: owner.diagnostic_owner(),
                    level: match level {
                        WriteBackSnapshotSemanticOrderLevel::Group => {
                            RpgMakerSemanticOrderLevel::Group
                        }
                        WriteBackSnapshotSemanticOrderLevel::Unit => {
                            RpgMakerSemanticOrderLevel::Unit
                        }
                    },
                }
            }
            Self::UnknownMutationAccess => {
                RpgMakerWriteBackAssetSnapshotViolation::UnknownMutationAccess
            }
            Self::NonCanonicalMutationResource {
                owner,
                group_location,
            } => RpgMakerWriteBackAssetSnapshotViolation::NonCanonicalMutationResource {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::InvalidClaimSummary {
                owner,
                row_index,
                kind,
                expected_rows,
                actual_rows,
                details,
            } => RpgMakerWriteBackAssetSnapshotViolation::InvalidClaimSummary {
                owner: owner.diagnostic_owner(),
                row_index: *row_index,
                mismatch: diagnostic_claim_summary_kind(*kind),
                expected_rows: *expected_rows,
                actual_rows: *actual_rows,
                details: diagnostic_claim_summary_details(details),
            },
            Self::AssetFingerprintMismatch { owner } => {
                RpgMakerWriteBackAssetSnapshotViolation::AssetFingerprintMismatch {
                    owner: owner.diagnostic_owner(),
                }
            }
            Self::InvalidDialogueDefinition(source) => {
                RpgMakerWriteBackAssetSnapshotViolation::InvalidDialogueDefinition {
                    problem: source.diagnostic_problem(),
                }
            }
            Self::InvalidLocation(source) => {
                RpgMakerWriteBackAssetSnapshotViolation::InvalidLocation {
                    failure: source.diagnostic_failure(),
                }
            }
            Self::InvalidProjection(source) => {
                RpgMakerWriteBackAssetSnapshotViolation::InvalidProjection {
                    failure: source.diagnostic_failure(),
                }
            }
            Self::InvalidUnitContent { column, source } => {
                RpgMakerWriteBackAssetSnapshotViolation::InvalidUnitContent {
                    column: SafeIdentifier::from_validated(*column),
                    category: write_back_json_failure(source),
                    line: source.line(),
                    json_column: source.column(),
                }
            }
            Self::InvalidModel(source) => RpgMakerWriteBackAssetSnapshotViolation::InvalidModel {
                violation: write_back_model_violation(source),
            },
        }
    }
}

fn diagnostic_claim_summary_kind(
    kind: ClaimSummaryMismatchKind,
) -> RpgMakerClaimSummaryMismatchKind {
    match kind {
        ClaimSummaryMismatchKind::DuplicateResource => {
            RpgMakerClaimSummaryMismatchKind::DuplicateResource
        }
        ClaimSummaryMismatchKind::MissingRow => RpgMakerClaimSummaryMismatchKind::MissingRow,
        ClaimSummaryMismatchKind::UnexpectedRow => RpgMakerClaimSummaryMismatchKind::UnexpectedRow,
        ClaimSummaryMismatchKind::Resource => RpgMakerClaimSummaryMismatchKind::Resource,
        ClaimSummaryMismatchKind::Access => RpgMakerClaimSummaryMismatchKind::Access,
        ClaimSummaryMismatchKind::Representative => {
            RpgMakerClaimSummaryMismatchKind::Representative
        }
    }
}

fn diagnostic_claim_summary_details(
    details: &ClaimSummaryMismatchDetails,
) -> RpgMakerClaimSummaryMismatchDetails {
    RpgMakerClaimSummaryMismatchDetails {
        expected_group: details
            .expected_group
            .as_ref()
            .map(RpgMakerLocation::diagnostic_location),
        actual_group: details
            .actual_group
            .as_ref()
            .map(RpgMakerLocation::diagnostic_location),
        expected_resource: details
            .expected_resource
            .as_ref()
            .map(RpgMakerLocation::diagnostic_location),
        actual_resource: details
            .actual_resource
            .as_ref()
            .map(RpgMakerLocation::diagnostic_location),
        expected_access: details.expected_access.map(diagnostic_mutation_access),
        actual_access: details.actual_access.map(diagnostic_mutation_access),
    }
}

const fn diagnostic_mutation_access(access: MutationResourceAccess) -> RpgMakerMutationAccess {
    match access {
        MutationResourceAccess::Intent => RpgMakerMutationAccess::Intent,
        MutationResourceAccess::Exclusive => RpgMakerMutationAccess::Exclusive,
    }
}

fn write_back_json_failure(source: &serde_json::Error) -> RpgMakerJsonFailureKind {
    match JsonErrorCategory::from(source) {
        JsonErrorCategory::Io => RpgMakerJsonFailureKind::Io,
        JsonErrorCategory::Syntax => RpgMakerJsonFailureKind::Syntax,
        JsonErrorCategory::Data => RpgMakerJsonFailureKind::Data,
        JsonErrorCategory::Eof => RpgMakerJsonFailureKind::Eof,
        JsonErrorCategory::DuplicateObjectKey => RpgMakerJsonFailureKind::DuplicateObjectKey,
    }
}

fn write_back_model_violation(
    source: &RpgMakerWriteBackSnapshotError,
) -> RpgMakerWriteBackModelViolation {
    match source {
        RpgMakerWriteBackSnapshotError::BlankSourceContent { role } => {
            RpgMakerWriteBackModelViolation::BlankSourceContent {
                role: role.diagnostic_role(),
            }
        }
        RpgMakerWriteBackSnapshotError::BlankTranslationContent { role } => {
            RpgMakerWriteBackModelViolation::BlankTranslationContent {
                role: role.diagnostic_role(),
            }
        }
        RpgMakerWriteBackSnapshotError::ContentShapeMismatch { role } => {
            RpgMakerWriteBackModelViolation::ContentShapeMismatch {
                role: role.diagnostic_role(),
            }
        }
        RpgMakerWriteBackSnapshotError::EmptyLineContent { role, column } => {
            RpgMakerWriteBackModelViolation::EmptyLineContent {
                role: role.diagnostic_role(),
                column: SafeIdentifier::from_validated(*column),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidContentLine {
            role,
            column,
            line_index,
        } => RpgMakerWriteBackModelViolation::InvalidContentLine {
            role: role.diagnostic_role(),
            column: SafeIdentifier::from_validated(*column),
            line_index: *line_index,
        },
        RpgMakerWriteBackSnapshotError::AlignedLineCountMismatch {
            role,
            expected,
            actual,
        } => RpgMakerWriteBackModelViolation::AlignedLineCountMismatch {
            role: role.diagnostic_role(),
            expected: *expected,
            actual: *actual,
        },
        RpgMakerWriteBackSnapshotError::AlignedBlankLineMismatch { role, line_index } => {
            RpgMakerWriteBackModelViolation::AlignedBlankLineMismatch {
                role: role.diagnostic_role(),
                line_index: *line_index,
            }
        }
        RpgMakerWriteBackSnapshotError::EmptyProjection { group_location } => {
            RpgMakerWriteBackModelViolation::EmptyProjection {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidRole { kind, role } => {
            RpgMakerWriteBackModelViolation::InvalidRole {
                group_kind: kind.diagnostic_group_kind(),
                role: role.diagnostic_role(),
            }
        }
        RpgMakerWriteBackSnapshotError::DuplicateRole {
            group_location,
            role,
        } => RpgMakerWriteBackModelViolation::DuplicateRole {
            group_location: group_location.diagnostic_location(),
            role: role.diagnostic_role(),
        },
        RpgMakerWriteBackSnapshotError::RecipeRoleMismatch {
            group_location,
            units,
            recipes,
        } => RpgMakerWriteBackModelViolation::RecipeRoleMismatch {
            group_location: group_location.diagnostic_location(),
            units: units.iter().map(TextUnitRole::diagnostic_role).collect(),
            recipes: recipes.iter().map(TextUnitRole::diagnostic_role).collect(),
        },
        RpgMakerWriteBackSnapshotError::RecipeLineMismatch {
            group_location,
            role,
        } => RpgMakerWriteBackModelViolation::RecipeLineMismatch {
            group_location: group_location.diagnostic_location(),
            role: role.diagnostic_role(),
        },
        RpgMakerWriteBackSnapshotError::RecipeClaimMismatch { group_location } => {
            RpgMakerWriteBackModelViolation::RecipeClaimMismatch {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
            group_location,
            target,
        } => RpgMakerWriteBackModelViolation::RecipeDoesNotRebuildOriginal {
            group_location: group_location.diagnostic_location(),
            target: target.diagnostic_location(),
        },
        RpgMakerWriteBackSnapshotError::MutationClaimConflict { resource } => {
            RpgMakerWriteBackModelViolation::MutationClaimConflict {
                resource: resource.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::MismatchedClaimSource { group_location, .. } => {
            RpgMakerWriteBackModelViolation::MismatchedClaimSource {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::MismatchedClaimResourceSource {
            group_location,
            resource,
        } => RpgMakerWriteBackModelViolation::MismatchedClaimResourceSource {
            group_location: group_location.diagnostic_location(),
            resource: resource.diagnostic_location(),
        },
        RpgMakerWriteBackSnapshotError::InvalidDialogueProjection { group_location } => {
            RpgMakerWriteBackModelViolation::InvalidDialogueProjection {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidScrollingProjection { group_location } => {
            RpgMakerWriteBackModelViolation::InvalidScrollingProjection {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidScrollingRecipe { group_location } => {
            RpgMakerWriteBackModelViolation::InvalidScrollingRecipe {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidChoicesProjection { group_location } => {
            RpgMakerWriteBackModelViolation::InvalidChoicesProjection {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::InvalidDirectProjection { group_location } => {
            RpgMakerWriteBackModelViolation::InvalidDirectProjection {
                group_location: group_location.diagnostic_location(),
            }
        }
        RpgMakerWriteBackSnapshotError::MismatchedDialogueGroup {
            group_location,
            recipe_location,
        } => RpgMakerWriteBackModelViolation::MismatchedDialogueGroup {
            group_location: group_location.diagnostic_location(),
            recipe_location: recipe_location.diagnostic_location(),
        },
    }
}

impl fmt::Display for InvalidRpgMakerWriteBackAssetSnapshot {
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
            Self::PlaceholderRuleRowCount { actual } => write!(
                formatter,
                "写回快照应包含一行当前 Placeholder 规则，实际为 {actual} 行"
            ),
            Self::BlankPlaceholderRules => formatter.write_str("当前 Placeholder 规则资源不能为空"),
            Self::InvalidPlaceholderRulesJson(source) => {
                write!(
                    formatter,
                    "当前 Placeholder 规则资源不是合法 JSON：{source}"
                )
            }
            Self::InvalidBuiltinPlaceholder(source) => source.fmt(formatter),
            Self::InvalidPlaceholderRules(source) => {
                write!(formatter, "当前 Placeholder 规则无效：{source}")
            }
            Self::InvalidSemanticOrderKey { column, source } => {
                write!(formatter, "列 {column} 不是规范语义顺序键：{source}")
            }
            Self::UnknownOwner => formatter.write_str("未知资产所有者"),
            Self::DuplicateOwner(owner) => {
                write!(formatter, "资产所有者状态重复：{}", owner.storage_name())
            }
            Self::InvalidFingerprintLength {
                owner,
                column,
                actual,
            } => write!(
                formatter,
                "资产所有者 {} 的 {column} 应为 32 字节，实际为 {actual} 字节",
                owner.storage_name()
            ),
            Self::AssetWithoutOwner(owner) => {
                write!(
                    formatter,
                    "资产没有 active owner state：{}",
                    owner.storage_name()
                )
            }
            Self::UnknownGroupKind => formatter.write_str("未知文本组类型"),
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
                "单元或目标没有对应资产组：{} / {group_location}",
                owner.storage_name()
            ),
            Self::DuplicateSemanticOrderKey { owner, level } => write!(
                formatter,
                "owner {} 的不同 {} 使用了相同 semantic_order_key",
                owner.storage_name(),
                level.as_str()
            ),
            Self::UnknownMutationAccess => formatter.write_str("未知物理修改访问方式"),
            Self::NonCanonicalMutationResource {
                owner,
                group_location,
            } => write!(
                formatter,
                "owner {} 的组 {group_location} 使用了非规范 resource_key 编码",
                owner.storage_name()
            ),
            Self::InvalidClaimSummary {
                owner,
                row_index,
                kind,
                expected_rows,
                actual_rows,
                ..
            } => write!(
                formatter,
                "owner {} 的 Claim 冲突摘要损坏：{}，第 {row_index} 行，期待 {expected_rows} 行，实际 {actual_rows} 行",
                owner.storage_name(),
                kind.as_str()
            ),
            Self::AssetFingerprintMismatch { owner } => {
                write!(
                    formatter,
                    "资产所有者 {} 的快照指纹与三表内容不一致",
                    owner.storage_name()
                )
            }
            Self::InvalidDialogueDefinition(source) => {
                write!(formatter, "项目中的 MV 对话定义无法编码：{source}")
            }
            Self::InvalidLocation(source) => write!(formatter, "组位置无效：{source}"),
            Self::InvalidProjection(source) => write!(formatter, "文本投影无效：{source}"),
            Self::InvalidUnitContent { column, source } => {
                write!(formatter, "列 {column} 不是合法文本单元内容：{source}")
            }
            Self::InvalidModel(source) => source.fmt(formatter),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaimSummaryMismatchKind {
    DuplicateResource,
    MissingRow,
    UnexpectedRow,
    Resource,
    Access,
    Representative,
}

impl ClaimSummaryMismatchKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateResource => "duplicate_resource",
            Self::MissingRow => "missing_row",
            Self::UnexpectedRow => "unexpected_row",
            Self::Resource => "resource_mismatch",
            Self::Access => "access_mismatch",
            Self::Representative => "representative_mismatch",
        }
    }
}

impl Error for InvalidRpgMakerWriteBackAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSemanticOrderKey { source, .. } => Some(source),
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidProjection(source) => Some(source),
            Self::InvalidUnitContent { source, .. } => Some(source),
            Self::InvalidModel(source) => Some(source),
            Self::InvalidDialogueDefinition(source) => Some(source.as_ref()),
            Self::InvalidPlaceholderRulesJson(source) => Some(source),
            Self::InvalidBuiltinPlaceholder(source) => Some(source),
            Self::InvalidPlaceholderRules(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct OwnerState {
    asset_fingerprint: AssetSnapshotFingerprint,
}

struct SnapshotRows {
    owners: Vec<SqliteRow>,
    placeholder_rules: String,
    groups: Vec<OwnerPartitionedSqliteRow>,
    units: Vec<OwnerPartitionedSqliteRow>,
    claims: Vec<OwnerPartitionedSqliteRow>,
}

enum SnapshotAssetRow {
    Group(OwnerPartitionedSqliteRow),
    Unit(OwnerPartitionedSqliteRow),
    Claim(OwnerPartitionedSqliteRow),
}

struct PreparedRows {
    stale_owners: Vec<RpgMakerAssetOwner>,
    owner_states: HashMap<RpgMakerAssetOwner, OwnerState>,
    placeholder_rules: String,
    records: Vec<SnapshotAssetRow>,
}

fn prepare_rows(
    rows: SnapshotRows,
    current_source: SourceSnapshotFingerprint,
) -> Result<PreparedRows, InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut owner_states = HashMap::new();
    let mut stale_owners = Vec::new();
    let mut owners = rows.owners;
    sort_owner_state_rows(owners.as_mut_slice());
    for row in owners {
        let RpgMakerAssetOwnerStateStorageRow {
            owner,
            source_snapshot_fingerprint,
            asset_snapshot_fingerprint,
        } = RpgMakerAssetOwnerStateStorageRow::decode(row).map_err(map_storage_row_error)?;
        let source = exact_fingerprint(
            source_snapshot_fingerprint,
            owner,
            "source_snapshot_fingerprint",
        )?;
        let asset = exact_fingerprint(
            asset_snapshot_fingerprint,
            owner,
            "asset_snapshot_fingerprint",
        )?;
        if owner_states
            .insert(
                owner,
                OwnerState {
                    asset_fingerprint: AssetSnapshotFingerprint::from_bytes(asset),
                },
            )
            .is_some()
        {
            return Err(InvalidRpgMakerWriteBackAssetSnapshot::DuplicateOwner(owner));
        }
        if SourceSnapshotFingerprint::from_bytes(source) != current_source {
            stale_owners.push(owner);
        }
    }
    stale_owners.sort_by_key(|owner| rpg_maker_asset_owner_order(*owner));

    let records = rows
        .groups
        .into_iter()
        .map(SnapshotAssetRow::Group)
        .chain(rows.units.into_iter().map(SnapshotAssetRow::Unit))
        .chain(rows.claims.into_iter().map(SnapshotAssetRow::Claim))
        .collect();
    Ok(PreparedRows {
        stale_owners,
        owner_states,
        placeholder_rules: rows.placeholder_rules,
        records,
    })
}

enum DecodedRecord {
    Group {
        owner: RpgMakerAssetOwner,
        group_location_raw: String,
        group_location: RpgMakerLocation,
        semantic_order_key: RpgMakerSemanticOrderKey,
        kind: TextGroupKind,
        group_kind_raw: String,
        recipes: Vec<TextProjectionRecipe>,
        recipes_raw: String,
    },
    Unit {
        owner: RpgMakerAssetOwner,
        group_location_raw: String,
        group_location: RpgMakerLocation,
        role: TextUnitRole,
        role_raw: String,
        semantic_order_key: RpgMakerSemanticOrderKey,
        source_content: TextUnitContent,
        source_content_json: String,
        source_context_json: String,
        translation_content: Option<TextUnitContent>,
    },
    Claim {
        owner: RpgMakerAssetOwner,
        group_location_raw: String,
        group_location: RpgMakerLocation,
        access: MutationResourceAccess,
        resource_key_raw: String,
    },
}

fn decode_record(
    row: SnapshotAssetRow,
) -> Result<DecodedRecord, InvalidRpgMakerWriteBackAssetSnapshot> {
    match row {
        SnapshotAssetRow::Group(row) => decode_group(row),
        SnapshotAssetRow::Unit(row) => decode_unit(row),
        SnapshotAssetRow::Claim(row) => decode_claim(row),
    }
}

fn decode_group(
    OwnerPartitionedSqliteRow { owner, row }: OwnerPartitionedSqliteRow,
) -> Result<DecodedRecord, InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut row = RpgMakerAssetStorageRowDecoder::new(row, 4).map_err(map_storage_row_error)?;
    let RpgMakerTextGroupStorageRow {
        group_location_raw,
        group_location,
        group_kind_raw,
        kind,
        semantic_order_key,
    } = RpgMakerTextGroupStorageRow::decode(&mut row).map_err(map_storage_row_error)?;
    let recipes_raw = row
        .required_text("projection_recipe_json")
        .map_err(map_storage_row_error)?;
    Ok(DecodedRecord::Group {
        owner,
        group_location,
        group_location_raw,
        semantic_order_key,
        kind,
        group_kind_raw,
        recipes: RpgMakerProjectionCodec::decode_recipes(&recipes_raw)
            .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection)?,
        recipes_raw,
    })
}

fn decode_unit(
    OwnerPartitionedSqliteRow { owner, row }: OwnerPartitionedSqliteRow,
) -> Result<DecodedRecord, InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut row = RpgMakerAssetStorageRowDecoder::new(row, 6).map_err(map_storage_row_error)?;
    let storage = RpgMakerTextUnitStorageRow::decode(&mut row).map_err(map_storage_row_error)?;
    let translation_content = storage
        .decode_translation_content()
        .map_err(map_storage_row_error)?;
    let RpgMakerTextUnitStorageRow {
        group_location_raw,
        group_location,
        role,
        role_raw,
        semantic_order_key,
        source_content,
        source_content_json,
        source_context_json,
        ..
    } = storage;
    Ok(DecodedRecord::Unit {
        owner,
        group_location_raw,
        group_location,
        role,
        role_raw,
        semantic_order_key,
        source_content,
        source_content_json,
        source_context_json,
        translation_content,
    })
}

fn decode_claim(
    OwnerPartitionedSqliteRow { owner, row }: OwnerPartitionedSqliteRow,
) -> Result<DecodedRecord, InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut row = RpgMakerAssetStorageRowDecoder::new(row, 3).map_err(map_storage_row_error)?;
    let group_location_raw = row
        .required_text("group_location")
        .map_err(map_storage_row_error)?;
    let group_location = RpgMakerLocationCodec::decode(&group_location_raw)
        .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidLocation)?;
    let resource_key_raw = row
        .required_text("resource_key")
        .map_err(map_storage_row_error)?;
    let access_raw = row.required_text("access").map_err(map_storage_row_error)?;
    let access = MutationResourceAccess::from_storage_name(&access_raw)
        .ok_or(InvalidRpgMakerWriteBackAssetSnapshot::UnknownMutationAccess)?;
    Ok(DecodedRecord::Claim {
        owner,
        group_location_raw,
        group_location,
        access,
        resource_key_raw,
    })
}

struct GroupBuilder {
    owner: RpgMakerAssetOwner,
    group_location_raw: String,
    semantic_order_key: RpgMakerSemanticOrderKey,
    kind: TextGroupKind,
    location: RpgMakerLocation,
    recipes: Vec<TextProjectionRecipe>,
    units: Vec<RpgMakerWriteBackUnit>,
    unit_order_keys: HashMap<RpgMakerSemanticOrderKey, TextUnitRole>,
}

/// 校验侧对 `rpg_maker_asset` 唯一 framing 定义的薄包装。
///
/// project_definition 帧只属于 Builtin owner:该 owner 的对话定义是快照语义的一
/// 部分，Rules 快照不携带项目定义。这一 owner 判断与写入侧“提供即掺入”的
/// 调用约定共同构成同一事实,framing 本身由 `RpgMakerTextSnapshotFingerprintBuilder`
/// 唯一拥有。
struct SnapshotFingerprintAccumulator {
    builder: RpgMakerTextSnapshotFingerprintBuilder,
}

impl SnapshotFingerprintAccumulator {
    fn new(owner: RpgMakerAssetOwner, dialogue_definition_json: &str) -> Self {
        let project_definition_json =
            (owner == RpgMakerAssetOwner::Builtin).then_some(dialogue_definition_json);
        Self {
            builder: RpgMakerTextSnapshotFingerprintBuilder::new(owner, project_definition_json),
        }
    }

    fn group(
        &mut self,
        group_location: &str,
        semantic_order_key: &RpgMakerSemanticOrderKey,
        group_kind: &str,
        recipes: &str,
    ) {
        self.builder
            .group(group_location, semantic_order_key, group_kind, recipes);
    }

    fn unit(
        &mut self,
        group_location: &str,
        role: &str,
        semantic_order_key: &RpgMakerSemanticOrderKey,
        source: &str,
        context: &str,
    ) {
        self.builder
            .unit(group_location, role, semantic_order_key, source, context);
    }

    fn claim(&mut self, resource_key: &str, access: &str, group_location: &str) {
        self.builder.claim(resource_key, access, group_location);
    }

    fn finish(self) -> AssetSnapshotFingerprint {
        AssetSnapshotFingerprint::from_bytes(self.builder.finish().into_bytes())
    }
}

fn assemble_snapshot(
    owner_states: HashMap<RpgMakerAssetOwner, OwnerState>,
    records: impl IntoIterator<Item = DecodedRecord>,
    dialogue_definition_json: &str,
) -> Result<RpgMakerWriteBackSnapshot, InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut groups = Vec::<GroupBuilder>::new();
    let mut group_indexes = HashMap::<RpgMakerAssetOwner, HashMap<String, usize>>::new();
    let mut group_order_keys =
        HashMap::<RpgMakerAssetOwner, HashMap<RpgMakerSemanticOrderKey, String>>::new();
    let mut stored_claim_summaries =
        HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    let mut fingerprint_accumulators = owner_states
        .keys()
        .map(|owner| {
            (
                *owner,
                SnapshotFingerprintAccumulator::new(*owner, dialogue_definition_json),
            )
        })
        .collect::<HashMap<_, _>>();

    for record in records {
        let owner = match &record {
            DecodedRecord::Group { owner, .. }
            | DecodedRecord::Unit { owner, .. }
            | DecodedRecord::Claim { owner, .. } => owner,
        };
        let owner = *owner;
        if !owner_states.contains_key(&owner) {
            return Err(InvalidRpgMakerWriteBackAssetSnapshot::AssetWithoutOwner(
                owner,
            ));
        }
        match record {
            DecodedRecord::Group {
                owner,
                group_location_raw,
                group_location,
                semantic_order_key,
                kind,
                group_kind_raw,
                recipes,
                recipes_raw,
            } => {
                fingerprint_accumulators
                    .get_mut(&owner)
                    .expect("active owner 已在循环入口确认")
                    .group(
                        &group_location_raw,
                        &semantic_order_key,
                        &group_kind_raw,
                        &recipes_raw,
                    );
                let owner_group_indexes = group_indexes.entry(owner).or_default();
                if owner_group_indexes.contains_key(&group_location_raw) {
                    return Err(InvalidRpgMakerWriteBackAssetSnapshot::DuplicateGroup {
                        owner,
                        group_location: Box::new(group_location),
                    });
                }
                if group_order_keys
                    .entry(owner)
                    .or_default()
                    .insert(semantic_order_key.clone(), group_location_raw.clone())
                    .is_some()
                {
                    return Err(
                        InvalidRpgMakerWriteBackAssetSnapshot::DuplicateSemanticOrderKey {
                            owner,
                            level: WriteBackSnapshotSemanticOrderLevel::Group,
                        },
                    );
                }
                let index = groups.len();
                owner_group_indexes.insert(group_location_raw.clone(), index);
                groups.push(GroupBuilder {
                    owner,
                    group_location_raw,
                    semantic_order_key,
                    kind,
                    location: group_location,
                    recipes,
                    units: Vec::new(),
                    unit_order_keys: HashMap::new(),
                });
            }
            DecodedRecord::Unit {
                owner,
                group_location_raw,
                group_location,
                role,
                role_raw,
                semantic_order_key,
                source_content,
                source_content_json,
                source_context_json,
                translation_content,
            } => {
                fingerprint_accumulators
                    .get_mut(&owner)
                    .expect("active owner 已在循环入口确认")
                    .unit(
                        &group_location_raw,
                        &role_raw,
                        &semantic_order_key,
                        &source_content_json,
                        &source_context_json,
                    );
                let index = group_indexes
                    .get(&owner)
                    .and_then(|indexes| indexes.get(&group_location_raw))
                    .copied()
                    .ok_or(InvalidRpgMakerWriteBackAssetSnapshot::MissingGroup {
                        owner,
                        group_location: Box::new(group_location),
                    })?;
                let group = &mut groups[index];
                if group
                    .unit_order_keys
                    .insert(semantic_order_key, role.clone())
                    .is_some()
                {
                    return Err(
                        InvalidRpgMakerWriteBackAssetSnapshot::DuplicateSemanticOrderKey {
                            owner,
                            level: WriteBackSnapshotSemanticOrderLevel::Unit,
                        },
                    );
                }
                group.units.push(
                    RpgMakerWriteBackUnit::new(role, source_content, translation_content)
                        .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidModel)?,
                );
            }
            DecodedRecord::Claim {
                owner,
                group_location_raw,
                group_location,
                access,
                resource_key_raw,
            } => {
                let index = group_indexes
                    .get(&owner)
                    .and_then(|indexes| indexes.get(&group_location_raw))
                    .copied()
                    .ok_or_else(|| InvalidRpgMakerWriteBackAssetSnapshot::MissingGroup {
                        owner,
                        group_location: Box::new(group_location),
                    })?;
                let group = &groups[index];
                stored_claim_summaries
                    .entry(owner)
                    .or_default()
                    .push(EncodedMutationClaim::new(
                        resource_key_raw,
                        access,
                        group_location_raw,
                        group.semantic_order_key.clone(),
                    ));
            }
        }
    }

    let mut logical_claims = HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    let mut validated_groups = Vec::with_capacity(groups.len());
    for group in groups {
        let group_location_raw = group.group_location_raw;
        let semantic_order_key = group.semantic_order_key;
        let owner = group.owner;
        let group = RpgMakerWriteBackGroup::from_recipes(
            owner,
            group.kind,
            group.location,
            group.units,
            group.recipes,
        )
        .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidModel)?;
        let owner_claims = logical_claims.entry(owner).or_default();
        for lock in group.mutation_claims().locks() {
            owner_claims.push(EncodedMutationClaim::new(
                RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                    .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection)?,
                lock.access(),
                group_location_raw.clone(),
                semantic_order_key.clone(),
            ));
        }
        validated_groups.push((semantic_order_key, group));
    }

    validated_groups.sort_by(|left, right| left.0.cmp(&right.0));
    let snapshot = RpgMakerWriteBackSnapshot::new(
        validated_groups
            .into_iter()
            .map(|(_, group)| group)
            .collect(),
    )
    .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidModel)?;

    for owner in RPG_MAKER_WRITE_BACK_OWNER_ORDER {
        let Some(state) = owner_states.get(&owner) else {
            continue;
        };
        let owner_logical_claims = logical_claims.entry(owner).or_default();
        sort_logical_claims(owner_logical_claims);
        let stored_summary = stored_claim_summaries.entry(owner).or_default();
        validate_claim_summary(owner, owner_logical_claims, stored_summary)?;

        let accumulator = fingerprint_accumulators
            .get_mut(&owner)
            .expect("每个 active owner 都应建立指纹累加器");
        for claim in owner_logical_claims.iter() {
            accumulator.claim(
                &claim.resource_key,
                claim.access.storage_name(),
                &claim.group_location,
            );
        }
        let actual = fingerprint_accumulators
            .remove(&owner)
            .expect("每个 active owner 都应建立指纹累加器")
            .finish();
        if actual != state.asset_fingerprint {
            return Err(InvalidRpgMakerWriteBackAssetSnapshot::AssetFingerprintMismatch { owner });
        }
    }

    Ok(snapshot)
}

fn validate_claim_summary(
    owner: RpgMakerAssetOwner,
    logical_claims: &[EncodedMutationClaim],
    actual: &[EncodedMutationClaim],
) -> Result<(), InvalidRpgMakerWriteBackAssetSnapshot> {
    let mut expected = borrowed_collision_summary(logical_claims);
    let mut row_index = 0;
    loop {
        let expected_row = expected
            .next()
            .transpose()
            .expect("RpgMakerWriteBackSnapshot 已验证 owner 内 Claim 不冲突");
        let actual_row = actual.get(row_index);
        let kind = if row_index > 0
            && actual_row.is_some_and(|row| row.resource_key == actual[row_index - 1].resource_key)
        {
            ClaimSummaryMismatchKind::DuplicateResource
        } else {
            match (expected_row, actual_row) {
                (Some(_), None) => ClaimSummaryMismatchKind::MissingRow,
                (None, Some(_)) => ClaimSummaryMismatchKind::UnexpectedRow,
                (Some(expected), Some(actual)) if expected.resource_key != actual.resource_key => {
                    ClaimSummaryMismatchKind::Resource
                }
                (Some(expected), Some(actual)) if expected.access != actual.access => {
                    ClaimSummaryMismatchKind::Access
                }
                (Some(expected), Some(actual))
                    if expected.group_location != actual.group_location
                        || expected.semantic_order_key != actual.semantic_order_key =>
                {
                    ClaimSummaryMismatchKind::Representative
                }
                (Some(_), Some(_)) => {
                    row_index += 1;
                    continue;
                }
                (None, None) => return Ok(()),
            }
        };
        let expected_rows = collision_summary(logical_claims)
            .expect("RpgMakerWriteBackSnapshot 已验证 owner 内 Claim 不冲突")
            .len();
        let actual_resource = decode_actual_summary_resource(owner, actual_row)?;
        return Err(claim_summary_mismatch(
            owner,
            row_index,
            kind,
            expected_rows,
            actual,
            ComparedClaimSummaryRows {
                expected: expected_row,
                actual: actual_row,
            },
            actual_resource,
        ));
    }
}

fn borrowed_collision_summary(
    logical_claims: &[EncodedMutationClaim],
) -> impl Iterator<Item = Result<&EncodedMutationClaim, MutationClaimSummaryError>> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= logical_claims.len() {
            return None;
        }
        let resource_key = logical_claims[start].resource_key.as_str();
        let mut end = start + 1;
        while end < logical_claims.len()
            && logical_claims[end].resource_key.as_str() == resource_key
        {
            end += 1;
        }
        let claims = &logical_claims[start..end];
        start = end;

        let access = claims[0].access;
        if claims.iter().any(|claim| claim.access != access) {
            return Some(Err(MutationClaimSummaryError::MixedAccess {
                resource_key: resource_key.to_owned(),
            }));
        }
        let representative = match access {
            MutationResourceAccess::Intent => claims
                .iter()
                .min_by(|left, right| {
                    left.semantic_order_key
                        .cmp(&right.semantic_order_key)
                        .then_with(|| left.group_location.cmp(&right.group_location))
                })
                .expect("非空资源分组必须存在代表"),
            MutationResourceAccess::Exclusive => {
                if claims.len() != 1 {
                    return Some(Err(MutationClaimSummaryError::MultipleExclusive {
                        resource_key: resource_key.to_owned(),
                    }));
                }
                &claims[0]
            }
        };
        Some(Ok(representative))
    })
}

struct ComparedClaimSummaryRows<'a> {
    expected: Option<&'a EncodedMutationClaim>,
    actual: Option<&'a EncodedMutationClaim>,
}

fn claim_summary_mismatch(
    owner: RpgMakerAssetOwner,
    row_index: usize,
    kind: ClaimSummaryMismatchKind,
    expected_rows: usize,
    actual_rows: &[EncodedMutationClaim],
    compared: ComparedClaimSummaryRows<'_>,
    actual_resource: Option<RpgMakerLocation>,
) -> InvalidRpgMakerWriteBackAssetSnapshot {
    let decode_group = |claim: Option<&EncodedMutationClaim>| {
        claim.map(|claim| {
            RpgMakerLocationCodec::decode(&claim.group_location)
                .expect("摘要 group_location 已在同一读取边界完成规范解码")
        })
    };
    InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
        owner,
        row_index,
        kind,
        expected_rows,
        actual_rows: actual_rows.len(),
        details: Box::new(ClaimSummaryMismatchDetails {
            expected_group: decode_group(compared.expected),
            actual_group: decode_group(compared.actual),
            expected_resource: decode_expected_summary_resource(compared.expected),
            actual_resource,
            expected_access: compared.expected.map(|claim| claim.access),
            actual_access: compared.actual.map(|claim| claim.access),
        }),
    }
}

fn decode_expected_summary_resource(
    claim: Option<&EncodedMutationClaim>,
) -> Option<RpgMakerLocation> {
    claim.map(|claim| {
        RpgMakerProjectionCodec::decode_mutation_resource(&claim.resource_key)
            .expect("重建的摘要 resource_key 必须是规范编码")
    })
}

fn decode_actual_summary_resource(
    owner: RpgMakerAssetOwner,
    claim: Option<&EncodedMutationClaim>,
) -> Result<Option<RpgMakerLocation>, InvalidRpgMakerWriteBackAssetSnapshot> {
    let Some(claim) = claim else {
        return Ok(None);
    };
    match RpgMakerProjectionCodec::decode_mutation_resource(&claim.resource_key) {
        Ok(resource) => Ok(Some(resource)),
        Err(RpgMakerProjectionCodecError::NonCanonical) => {
            let group_location = RpgMakerLocationCodec::decode(&claim.group_location)
                .map_err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidLocation)?;
            Err(
                InvalidRpgMakerWriteBackAssetSnapshot::NonCanonicalMutationResource {
                    owner,
                    group_location: Box::new(group_location),
                },
            )
        }
        Err(source) => Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection(
            source,
        )),
    }
}

#[cfg(test)]
#[derive(Default)]
struct FingerprintRows {
    groups: Vec<(String, RpgMakerSemanticOrderKey, String, String)>,
    units: Vec<(String, String, RpgMakerSemanticOrderKey, String, String)>,
    claims: Vec<(String, String, String)>,
}

#[cfg(test)]
fn snapshot_fingerprint(
    owner: RpgMakerAssetOwner,
    mut rows: FingerprintRows,
    dialogue_definition_json: &str,
) -> AssetSnapshotFingerprint {
    rows.groups.sort_by(|left, right| left.1.cmp(&right.1));
    rows.units.sort_by(|left, right| left.2.cmp(&right.2));
    rows.claims.sort();
    let mut accumulator = SnapshotFingerprintAccumulator::new(owner, dialogue_definition_json);
    for (group_location, semantic_order_key, group_kind, recipes) in rows.groups {
        accumulator.group(&group_location, &semantic_order_key, &group_kind, &recipes);
    }
    for (group_location, role, semantic_order_key, source, context) in rows.units {
        accumulator.unit(
            &group_location,
            &role,
            &semantic_order_key,
            &source,
            &context,
        );
    }
    for (resource_key, access, group_location) in rows.claims {
        accumulator.claim(&resource_key, &access, &group_location);
    }
    accumulator.finish()
}

fn exact_fingerprint(
    bytes: Vec<u8>,
    owner: RpgMakerAssetOwner,
    column: &'static str,
) -> Result<[u8; 32], InvalidRpgMakerWriteBackAssetSnapshot> {
    let actual = bytes.len();
    bytes.try_into().map_err(
        |_| InvalidRpgMakerWriteBackAssetSnapshot::InvalidFingerprintLength {
            owner,
            column,
            actual,
        },
    )
}

fn map_storage_row_error(
    error: RpgMakerAssetStorageRowError,
) -> InvalidRpgMakerWriteBackAssetSnapshot {
    match error {
        RpgMakerAssetStorageRowError::WrongColumnCount { expected, actual } => {
            InvalidRpgMakerWriteBackAssetSnapshot::WrongColumnCount { expected, actual }
        }
        RpgMakerAssetStorageRowError::WrongColumnType {
            column,
            expected,
            actual,
        } => InvalidRpgMakerWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected,
            actual,
        },
        RpgMakerAssetStorageRowError::InvalidSemanticOrderKey { column, source } => {
            InvalidRpgMakerWriteBackAssetSnapshot::InvalidSemanticOrderKey { column, source }
        }
        RpgMakerAssetStorageRowError::UnknownOwner(_) => {
            InvalidRpgMakerWriteBackAssetSnapshot::UnknownOwner
        }
        RpgMakerAssetStorageRowError::UnknownGroupKind(_) => {
            InvalidRpgMakerWriteBackAssetSnapshot::UnknownGroupKind
        }
        RpgMakerAssetStorageRowError::InvalidLocation(source) => {
            InvalidRpgMakerWriteBackAssetSnapshot::InvalidLocation(source)
        }
        RpgMakerAssetStorageRowError::InvalidRole(source) => {
            InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection(source)
        }
        RpgMakerAssetStorageRowError::InvalidSourceContent(source) => {
            InvalidRpgMakerWriteBackAssetSnapshot::InvalidUnitContent {
                column: "source_content_json",
                source,
            }
        }
        RpgMakerAssetStorageRowError::InvalidTranslationContent(source) => {
            InvalidRpgMakerWriteBackAssetSnapshot::InvalidUnitContent {
                column: "translation_content_json",
                source,
            }
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> RpgMakerWriteBackAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerWriteBackAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerWriteBackAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::rpg_maker::model::{DirectTextPart, DirectTextRecipe, ScalarFieldKey};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};
    use rusqlite::params_from_iter;

    #[test]
    fn write_back_placeholder_rules_are_strictly_parsed_and_compiled() {
        assert!(matches!(
            build_symbol_repair_context(RpgMakerEngine::Mz, "{not-json".to_owned()),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRulesJson(_))
        ));
        assert!(matches!(
            build_symbol_repair_context(RpgMakerEngine::Mv, r#"[{"pattern":"("}]"#.to_owned(),),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidPlaceholderRules(_))
        ));
        build_symbol_repair_context(
            RpgMakerEngine::Mz,
            r#"[{"scopes":["event_dialogue"],"pattern":"<msg>(?<text>.*?)</msg>"}]"#.to_owned(),
        )
        .expect("当前规范 Placeholder 快照应可建立写回修复上下文");
    }

    #[test]
    fn write_back_placeholder_resource_requires_one_nonblank_text_row() {
        assert!(matches!(
            decode_placeholder_rules_rows(Vec::new()),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::PlaceholderRuleRowCount { actual: 0 })
        ));
        assert!(matches!(
            decode_placeholder_rules_rows(vec![SqliteRow::new(vec![SqliteValue::Text(
                String::new(),
            )])]),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::BlankPlaceholderRules)
        ));
        assert_eq!(
            decode_placeholder_rules_rows(vec![SqliteRow::new(vec![SqliteValue::Text(
                "[]".to_owned(),
            )])])
            .expect("单行当前规则应可读取"),
            "[]"
        );
    }

    #[test]
    fn write_back_snapshot_model_variants_keep_typed_facts_on_cli_and_jsonl() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let target = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let recipe_location = RpgMakerLocation::value(RpgMakerSource::map(2), Vec::new());
        let scalar =
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试标量字段键应有效"));
        let resource = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let cases = vec![
            (
                RpgMakerWriteBackSnapshotError::BlankSourceContent {
                    role: TextUnitRole::DialogueBody,
                },
                vec!["model_error=blank_source_content", "role=dialogue_body"],
            ),
            (
                RpgMakerWriteBackSnapshotError::BlankTranslationContent {
                    role: scalar.clone(),
                },
                vec!["model_error=blank_translation_content", "role=scalar"],
            ),
            (
                RpgMakerWriteBackSnapshotError::ContentShapeMismatch {
                    role: TextUnitRole::Choices,
                },
                vec!["model_error=content_shape_mismatch", "role=choices"],
            ),
            (
                RpgMakerWriteBackSnapshotError::EmptyLineContent {
                    role: TextUnitRole::ScrollingText,
                    column: "source_content",
                },
                vec![
                    "model_error=empty_line_content",
                    "role=scrolling_text",
                    "source_content",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidContentLine {
                    role: TextUnitRole::DialogueBody,
                    column: "translation_content",
                    line_index: 3,
                },
                vec![
                    "model_error=invalid_content_line",
                    "role=dialogue_body",
                    "line_index=3",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::AlignedLineCountMismatch {
                    role: TextUnitRole::DialogueBody,
                    expected: 2,
                    actual: 5,
                },
                vec![
                    "model_error=aligned_line_count_mismatch",
                    "expected=2",
                    "actual=5",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::AlignedBlankLineMismatch {
                    role: TextUnitRole::Choices,
                    line_index: 4,
                },
                vec![
                    "model_error=aligned_blank_line_mismatch",
                    "role=choices",
                    "line_index=4",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::EmptyProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=empty_projection", "data/Items.json[1]"],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidRole {
                    kind: TextGroupKind::EventDialogue,
                    role: scalar.clone(),
                },
                vec![
                    "model_error=invalid_role",
                    "group_kind=event_dialogue",
                    "role=scalar",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::DuplicateRole {
                    group_location: Box::new(group_location.clone()),
                    role: TextUnitRole::DialogueSpeaker,
                },
                vec![
                    "model_error=duplicate_role",
                    "role=dialogue_speaker",
                    "data/Items.json[1]",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::RecipeRoleMismatch {
                    group_location: Box::new(group_location.clone()),
                    units: BTreeSet::from([TextUnitRole::DialogueBody]),
                    recipes: BTreeSet::from([TextUnitRole::DialogueSpeaker]),
                },
                vec![
                    "model_error=recipe_role_mismatch",
                    "unit_roles=dialogue_body",
                    "recipe_roles=dialogue_speaker",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::RecipeLineMismatch {
                    group_location: Box::new(group_location.clone()),
                    role: TextUnitRole::ScrollingText,
                },
                vec!["model_error=recipe_line_mismatch", "role=scrolling_text"],
            ),
            (
                RpgMakerWriteBackSnapshotError::RecipeClaimMismatch {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=recipe_claim_mismatch", "data/Items.json[1]"],
            ),
            (
                RpgMakerWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                    group_location: Box::new(group_location.clone()),
                    target: Box::new(target.clone()),
                },
                vec![
                    "model_error=recipe_does_not_rebuild_original",
                    "target=data/Items.json[1].name",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::MutationClaimConflict {
                    resource: Box::new(resource.clone()),
                },
                vec![
                    "model_error=mutation_claim_conflict",
                    "resource=data/Items.json[1].name",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::MismatchedClaimSource {
                    group_location: Box::new(group_location.clone()),
                    claim: Box::new(MutationClaim::Value(target.clone())),
                },
                vec![
                    "model_error=mismatched_claim_source",
                    "claim_kind=value",
                    "claim_location=data/Items.json[1].name",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::MismatchedClaimResourceSource {
                    group_location: Box::new(group_location.clone()),
                    resource: Box::new(resource),
                },
                vec![
                    "model_error=mismatched_claim_resource_source",
                    "resource=data/Items.json[1].name",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidDialogueProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_dialogue_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidScrollingProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_scrolling_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidScrollingRecipe {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=invalid_scrolling_recipe", "data/Items.json[1]"],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidChoicesProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_choices_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::InvalidDirectProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_direct_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                RpgMakerWriteBackSnapshotError::MismatchedDialogueGroup {
                    group_location: Box::new(group_location),
                    recipe_location: Box::new(recipe_location),
                },
                vec![
                    "model_error=mismatched_dialogue_group",
                    "recipe_location=data/Map002.json",
                ],
            ),
        ];

        for (source, _legacy_expected_facts) in cases {
            let expected = write_back_model_violation(&source);
            let actual =
                InvalidRpgMakerWriteBackAssetSnapshot::InvalidModel(source).diagnostic_violation();
            assert_eq!(
                actual,
                RpgMakerWriteBackAssetSnapshotViolation::InvalidModel {
                    violation: expected,
                }
            );
        }
    }

    #[test]
    fn write_back_snapshot_typed_sources_keep_stable_facts_without_copying_body_text() {
        const SOURCE_BODY: &str = "SENTINEL_WRITE_BACK_SNAPSHOT_BODY_329c";

        let pcre_source = pcre2::bytes::RegexBuilder::new()
            .build(&format!("(?<{SOURCE_BODY}"))
            .expect_err("测试 PCRE2 应无效");
        let dialogue = InvalidRpgMakerWriteBackAssetSnapshot::InvalidDialogueDefinition(Box::new(
            MvDialogueDefinitionError::InvalidPattern {
                rule_number: 7,
                source: pcre_source,
            },
        ))
        .diagnostic_violation();
        let location = InvalidRpgMakerWriteBackAssetSnapshot::InvalidLocation(
            RpgMakerLocationCodecError::InvalidDataFile(SOURCE_BODY.to_owned()),
        )
        .diagnostic_violation();
        let projection = InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection(
            RpgMakerProjectionCodecError::Projection(
                crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                    expected: 2,
                    actual: 5,
                },
            ),
        )
        .diagnostic_violation();
        let invalid_json = format!("{{\"{SOURCE_BODY}\":");
        let unit_content = InvalidRpgMakerWriteBackAssetSnapshot::InvalidUnitContent {
            column: "translation_content_json",
            source: serde_json::from_str::<serde_json::Value>(&invalid_json)
                .expect_err("测试 JSON 应不完整"),
        }
        .diagnostic_violation();

        for (violation, _legacy_expected_facts) in [
            (
                dialogue,
                &["rule_number=7", "engine=pcre2", "code=", "offset="][..],
            ),
            (location, &["codec=location", "kind=invalid_data_file"][..]),
            (
                projection,
                &[
                    "codec=projection",
                    "structure=non_contiguous_dialogue_body_lines",
                    "expected=2",
                    "actual=5",
                ][..],
            ),
            (
                unit_content,
                &[
                    "unit_content_json_invalid",
                    "json_category=eof",
                    "json_line=1",
                    "json_column=",
                ][..],
            ),
        ] {
            let report = write_back_asset_report(
                std::path::Path::new("C:/project/att.db"),
                RpgMakerWriteBackAssetProblem::InvalidSnapshot { violation },
            );
            let json = serde_json::to_string(&report).expect("报告应可序列化");
            assert!(!json.contains(SOURCE_BODY), "JSONL 不应复制正文：{json}");
            assert!(json.contains("rpg_maker.write_back.asset_snapshot"));
        }
    }

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
            placeholder_rules: "[]".to_owned(),
            groups: Vec::new(),
            units: Vec::new(),
            claims: Vec::new(),
        }
    }

    fn builtin_partition(values: Vec<SqliteValue>) -> OwnerPartitionedSqliteRow {
        OwnerPartitionedSqliteRow {
            owner: RpgMakerAssetOwner::Builtin,
            row: SqliteRow::new(values),
        }
    }

    fn scalar_snapshot_rows(indices: &[usize]) -> SnapshotRows {
        const OWNER: RpgMakerAssetOwner = RpgMakerAssetOwner::Builtin;
        const SOURCE_TEXT: &str = "原文";
        const DIALOGUE_DEFINITION: &str = "{\"rules\":[]}";
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"));
        let role_raw = RpgMakerProjectionCodec::encode_role(&role).expect("角色应可编码");
        let source_content_json =
            serde_json::to_string(&TextUnitContent::Value(SOURCE_TEXT.to_owned()))
                .expect("源内容应可编码");
        let mut groups = Vec::new();
        let mut units = Vec::new();
        let mut logical_claims = Vec::new();
        let mut fingerprint_rows = FingerprintRows::default();

        for index in indices.iter().copied() {
            let group_location =
                RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(index)]);
            let target = RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(index),
                    RpgMakerLocationStep::key("name"),
                ],
            );
            let recipes = vec![TextProjectionRecipe::Direct(
                DirectTextRecipe::new(
                    target.clone(),
                    SOURCE_TEXT,
                    vec![DirectTextPart::TextSlot { role: role.clone() }],
                )
                .expect("直接配方应合法"),
            )];
            let group_location_raw =
                RpgMakerLocationCodec::encode(&group_location).expect("组位置应可编码");
            let recipes_raw =
                RpgMakerProjectionCodec::encode_recipes(&recipes).expect("配方应可编码");
            let group_semantic_order_key =
                RpgMakerSemanticOrderKey::from_group_location(&group_location);
            let unit_semantic_order_key =
                RpgMakerSemanticOrderKey::from_unit_location(&target, &role);
            groups.push((
                group_semantic_order_key.clone(),
                builtin_partition(vec![
                    SqliteValue::Text(group_location_raw.clone()),
                    SqliteValue::Text("database_entry".to_owned()),
                    SqliteValue::Blob(
                        group_semantic_order_key
                            .encode()
                            .expect("Group 顺序键应可编码"),
                    ),
                    SqliteValue::Text(recipes_raw.clone()),
                ]),
            ));
            units.push((
                group_semantic_order_key.clone(),
                unit_semantic_order_key.clone(),
                builtin_partition(vec![
                    SqliteValue::Text(group_location_raw.clone()),
                    SqliteValue::Text(role_raw.clone()),
                    SqliteValue::Blob(
                        unit_semantic_order_key
                            .encode()
                            .expect("Unit 顺序键应可编码"),
                    ),
                    SqliteValue::Text(source_content_json.clone()),
                    SqliteValue::Text("{}".to_owned()),
                    SqliteValue::Null,
                ]),
            ));
            fingerprint_rows.groups.push((
                group_location_raw.clone(),
                group_semantic_order_key.clone(),
                "database_entry".to_owned(),
                recipes_raw,
            ));
            fingerprint_rows.units.push((
                group_location_raw.clone(),
                role_raw.clone(),
                unit_semantic_order_key,
                source_content_json.clone(),
                "{}".to_owned(),
            ));
            let claims =
                mutation_claims_for_group(TextGroupKind::DatabaseEntry, &group_location, &recipes)
                    .expect("测试组 Claim 应合法");
            for lock in claims.locks() {
                logical_claims.push(EncodedMutationClaim::new(
                    RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                        .expect("资源应可编码"),
                    lock.access(),
                    group_location_raw.clone(),
                    group_semantic_order_key.clone(),
                ));
            }
        }

        sort_logical_claims(&mut logical_claims);
        fingerprint_rows.claims = logical_claims
            .iter()
            .map(|claim| {
                (
                    claim.resource_key.clone(),
                    claim.access.storage_name().to_owned(),
                    claim.group_location.clone(),
                )
            })
            .collect();
        let claims = collision_summary(&logical_claims)
            .expect("测试 Claim 应可建立摘要")
            .into_iter()
            .map(|claim| {
                builtin_partition(vec![
                    SqliteValue::Text(claim.group_location),
                    SqliteValue::Text(claim.resource_key),
                    SqliteValue::Text(claim.access.storage_name().to_owned()),
                ])
            })
            .collect();
        let fingerprint = snapshot_fingerprint(OWNER, fingerprint_rows, DIALOGUE_DEFINITION);
        groups.sort_by(|left, right| left.0.cmp(&right.0));
        units.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        SnapshotRows {
            owners: vec![owner_row([1; 32], *fingerprint.as_bytes())],
            placeholder_rules: "[]".to_owned(),
            groups: groups.into_iter().map(|(_, row)| row).collect(),
            units: units.into_iter().map(|(_, _, row)| row).collect(),
            claims,
        }
    }

    fn assemble_test_rows(
        rows: SnapshotRows,
    ) -> Result<RpgMakerWriteBackSnapshot, InvalidRpgMakerWriteBackAssetSnapshot> {
        let prepared = prepare_rows(rows, SourceSnapshotFingerprint::from_bytes([1; 32]))?;
        let records = prepared
            .records
            .into_iter()
            .map(decode_record)
            .collect::<Result<Vec<_>, _>>()?;
        assemble_snapshot(prepared.owner_states, records, "{\"rules\":[]}")
    }

    #[test]
    fn snapshot_queries_follow_persisted_natural_order() {
        let connection = rusqlite::Connection::open_in_memory().expect("应可建立内存数据库");
        connection
            .execute_batch(
                r#"
                CREATE TABLE rpg_maker_asset_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE rpg_maker_text_group (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL CHECK (group_id > 0),
                    group_location TEXT NOT NULL,
                    semantic_order_key BLOB NOT NULL,
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL,
                    PRIMARY KEY (owner, group_id),
                    UNIQUE (owner, group_location),
                    UNIQUE (owner, semantic_order_key)
                );
                CREATE TABLE rpg_maker_text_unit (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL,
                    unit_role TEXT NOT NULL,
                    semantic_order_key BLOB NOT NULL,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state TEXT NOT NULL,
                    PRIMARY KEY (owner, group_id, unit_role),
                    UNIQUE (owner, semantic_order_key)
                );
                CREATE INDEX rpg_maker_text_unit_owner_group_order_idx
                    ON rpg_maker_text_unit(owner, group_id, semantic_order_key);
                CREATE TABLE rpg_maker_mutation_claim (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL,
                    resource_key TEXT NOT NULL,
                    access TEXT NOT NULL,
                    PRIMARY KEY (owner, group_id, resource_key)
                );
                CREATE INDEX rpg_maker_mutation_claim_owner_resource_idx
                    ON rpg_maker_mutation_claim(owner, resource_key, access, group_id);

                INSERT INTO rpg_maker_asset_owner_state VALUES ('rules', zeroblob(32), zeroblob(32));
                INSERT INTO rpg_maker_asset_owner_state VALUES ('builtin', zeroblob(32), zeroblob(32));
                INSERT INTO rpg_maker_text_group VALUES ('builtin', 2, 'group-b', X'010000000000000001000000000000000000', 'map', '[]');
                INSERT INTO rpg_maker_text_group VALUES ('builtin', 1, 'group-a', X'010000000000000000000000000000000000', 'map', '[]');
                INSERT INTO rpg_maker_text_unit VALUES ('builtin', 2, 'role-z', X'010000000000000001000000000000000000', '"z"', '{}', NULL, 'untranslated');
                INSERT INTO rpg_maker_text_unit VALUES ('builtin', 1, 'role-y', X'010000000000000000000000000000000000', '"y"', '{}', NULL, 'untranslated');
                INSERT INTO rpg_maker_mutation_claim VALUES ('builtin', 1, 'resource-z', 'exclusive');
                INSERT INTO rpg_maker_mutation_claim VALUES ('builtin', 2, 'resource-a', 'intent');
                "#,
            )
            .expect("测试快照表与行应可建立");

        let owners = connection
            .prepare(&read_rpg_maker_write_back_owner_states())
            .expect("owner 查询应可建立")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("owner 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner 行应可读取");
        let groups = connection
            .prepare(&read_rpg_maker_write_back_owner_groups())
            .expect("group 查询应可建立")
            .query_map(["builtin"], |row| row.get::<_, String>(0))
            .expect("group 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("group 行应可读取");
        let units = connection
            .prepare(&read_rpg_maker_write_back_owner_units())
            .expect("unit 查询应可建立")
            .query_map(["builtin"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("unit 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("unit 行应可读取");
        let claims = connection
            .prepare(read_rpg_maker_write_back_owner_claims())
            .expect("Claim 查询应可建立")
            .query_map(["builtin"], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .expect("Claim 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("Claim 行应可读取");

        assert_eq!(owners, ["builtin", "rules"]);
        assert_eq!(groups, ["group-a", "group-b"]);
        assert_eq!(
            units,
            [
                ("group-a".to_owned(), "role-y".to_owned()),
                ("group-b".to_owned(), "role-z".to_owned()),
            ]
        );
        assert_eq!(
            claims,
            [
                ("resource-a".to_owned(), "intent".to_owned()),
                ("resource-z".to_owned(), "exclusive".to_owned()),
            ]
        );

        let queries = rpg_maker_write_back_snapshot_queries();
        assert_eq!(queries.len(), RPG_MAKER_WRITE_BACK_QUERY_RESULT_COUNT);
        let (owner_query, remaining_queries) = queries
            .split_first()
            .expect("写回快照查询至少包含 owner 状态");
        assert!(
            owner_query.statement().contains("CASE owner"),
            "至多两行的 owner 状态查询必须恢复 Builtin、Rules 规范顺序"
        );
        let (placeholder_query, partition_queries) = remaining_queries
            .split_first()
            .expect("写回快照查询必须包含当前 Placeholder 规则");
        assert_eq!(placeholder_query.statement(), READ_PLACEHOLDER_RULES);
        assert!(
            partition_queries
                .iter()
                .all(|query| !query.statement().contains("CASE owner"))
        );
        for query in partition_queries {
            let explain = format!("EXPLAIN QUERY PLAN {}", query.statement());
            let mut statement = connection.prepare(&explain).expect("查询计划应可建立");
            let details = statement
                .query_map(
                    params_from_iter(query.parameters().iter().map(|parameter| match parameter {
                        SqliteValue::Text(value) => value.as_str(),
                        _ => unreachable!("窄查询只携带 owner 文本参数"),
                    })),
                    |row| row.get::<_, String>(3),
                )
                .expect("查询计划应可执行")
                .collect::<Result<Vec<_>, _>>()
                .expect("查询计划应可读取");
            if query.statement().contains("rpg_maker_text_unit AS unit") {
                assert!(
                    details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
                    "owner 窄 Unit 查询不得建立全表临时排序：{details:?}"
                );
                assert!(
                    details.iter().any(|detail| {
                        detail.contains("rpg_maker_text_unit_owner_group_order_idx")
                            && detail.contains("group_id=?")
                    }),
                    "写回 Unit 必须按 owner 与 group_id 关联：{details:?}"
                );
            }
        }
    }

    #[test]
    fn prepare_rows_moves_asset_rows_into_indexed_natural_order_work() {
        let mut pointers = Vec::new();
        let mut make_row = |label: &str, columns: usize| {
            let payload = label.to_owned();
            pointers.push(payload.as_ptr());
            let mut values = Vec::with_capacity(columns);
            values.push(SqliteValue::Text(payload));
            values.resize(columns, SqliteValue::Null);
            builtin_partition(values)
        };
        let rows = SnapshotRows {
            owners: Vec::new(),
            placeholder_rules: "[]".to_owned(),
            groups: vec![make_row("group-0", 4), make_row("group-1", 4)],
            units: vec![make_row("unit-0", 6), make_row("unit-1", 6)],
            claims: vec![make_row("claim-0", 3)],
        };

        let prepared = prepare_rows(rows, SourceSnapshotFingerprint::from_bytes([1; 32]))
            .expect("非 owner 行应保留自然顺序并进入 indexed CPU 工作集");

        assert_eq!(prepared.records.len(), 5);
        assert_eq!(
            prepared
                .records
                .iter()
                .map(|row| match row {
                    SnapshotAssetRow::Group(row)
                    | SnapshotAssetRow::Unit(row)
                    | SnapshotAssetRow::Claim(row) => match &row.row.values()[0] {
                        SqliteValue::Text(value) => value.as_ptr(),
                        value => panic!("首列载荷应为 TEXT，实际为 {}", value.kind_name()),
                    },
                })
                .collect::<Vec<_>>(),
            pointers
        );
    }

    #[test]
    fn decode_record_compacts_owner_and_moves_large_payload_text_out_of_sqlite_values() {
        let group_location = RpgMakerLocationCodec::encode(&RpgMakerLocation::value(
            crate::rpg_maker::text::RpgMakerSource::map(1),
            vec![crate::rpg_maker::text::RpgMakerLocationStep::key("name")],
        ))
        .expect("测试位置应可编码");
        let group_location_pointer = group_location.as_ptr();
        let role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
            ScalarFieldKey::new("name").expect("测试字段键应合法"),
        ))
        .expect("测试角色应可编码");
        let role_pointer = role.as_ptr();
        let source_content_json = r#""原文""#.to_owned();
        let source_content_json_pointer = source_content_json.as_ptr();
        let context = "{}".to_owned();
        let context_pointer = context.as_ptr();
        let translation_content_json = r#""译文""#.to_owned();
        let row = builtin_partition(vec![
            SqliteValue::Text(group_location),
            SqliteValue::Text(role),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::new(vec![0], 0)
                    .encode()
                    .expect("测试顺序键应可编码"),
            ),
            SqliteValue::Text(source_content_json),
            SqliteValue::Text(context),
            SqliteValue::Text(translation_content_json),
        ]);

        let DecodedRecord::Unit {
            owner,
            group_location_raw,
            role_raw,
            source_content,
            source_content_json,
            source_context_json,
            translation_content: Some(translation_content),
            ..
        } = decode_unit(row).expect("测试单元行应可解码")
        else {
            panic!("测试行应解码为 unit")
        };

        assert_eq!(owner, RpgMakerAssetOwner::Builtin);
        assert_eq!(group_location_raw.as_ptr(), group_location_pointer);
        assert_eq!(role_raw.as_ptr(), role_pointer);
        assert_eq!(source_content_json.as_ptr(), source_content_json_pointer);
        assert_eq!(source_context_json.as_ptr(), context_pointer);
        assert_eq!(source_content.as_value(), Some("原文"));
        assert_eq!(translation_content.as_value(), Some("译文"));
    }

    #[test]
    fn stale_source_and_asset_fingerprint_corruption_are_distinct_failures() {
        const DIALOGUE_DEFINITION: &str = "{\"rules\":[]}";
        let stale = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], [2; 32])]),
            SourceSnapshotFingerprint::from_bytes([9; 32]),
        )
        .expect("owner 行应可解码");
        assert_eq!(stale.stale_owners, [RpgMakerAssetOwner::Builtin]);

        let prepared = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], [2; 32])]),
            SourceSnapshotFingerprint::from_bytes([1; 32]),
        )
        .expect("owner 行应可解码");
        assert!(matches!(
            assemble_snapshot(
                prepared.owner_states,
                Vec::new(),
                DIALOGUE_DEFINITION,
            ),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::AssetFingerprintMismatch {
                owner
            }) if owner == RpgMakerAssetOwner::Builtin
        ));

        let valid_fingerprint = snapshot_fingerprint(
            RpgMakerAssetOwner::Builtin,
            FingerprintRows::default(),
            DIALOGUE_DEFINITION,
        );
        let prepared = prepare_rows(
            snapshot_rows(vec![owner_row([1; 32], *valid_fingerprint.as_bytes())]),
            SourceSnapshotFingerprint::from_bytes([1; 32]),
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
        let row = builtin_partition(vec![
            SqliteValue::Text(RpgMakerLocationCodec::encode(&location).expect("位置应可编码")),
            SqliteValue::Text("event_dialogue".to_owned()),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::from_group_location(&location)
                    .encode()
                    .expect("测试顺序键应可编码"),
            ),
            SqliteValue::Text("{not-json".to_owned()),
        ]);
        assert!(matches!(
            decode_group(row),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection(_))
        ));
    }

    #[test]
    fn write_back_rebuilds_full_claims_and_accepts_the_earliest_intent_summary() {
        let rows = scalar_snapshot_rows(&[9, 1]);
        assert!(
            rows.claims.len() < 6,
            "共享来源根 Intent 必须在持久层折叠，而不是保存两组的完整 locks"
        );

        assemble_test_rows(rows).expect("合法冲突摘要应可重建完整写回快照");
    }

    #[test]
    fn damaged_claim_summary_reports_the_exact_mismatch_kind() {
        let mut missing = scalar_snapshot_rows(&[9, 1]);
        missing.claims.pop();
        assert!(matches!(
            assemble_test_rows(missing),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::MissingRow,
                ..
            })
        ));

        let mut duplicate = scalar_snapshot_rows(&[9, 1]);
        let duplicated = duplicate.claims[0].clone();
        duplicate.claims.insert(1, duplicated);
        assert!(matches!(
            assemble_test_rows(duplicate),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::DuplicateResource,
                ..
            })
        ));

        let mut wrong_access = scalar_snapshot_rows(&[9, 1]);
        let row_index = wrong_access
            .claims
            .iter()
            .position(|row| {
                matches!(
                    row.row.values(),
                    [_, _, SqliteValue::Text(access)] if access == "intent"
                )
            })
            .expect("测试摘要应包含 Intent");
        let mut values = wrong_access.claims[row_index].clone().row.into_values();
        values[2] = SqliteValue::Text("exclusive".to_owned());
        wrong_access.claims[row_index].row = SqliteRow::new(values);
        assert!(matches!(
            assemble_test_rows(wrong_access),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Access,
                ..
            })
        ));

        let mut wrong_representative = scalar_snapshot_rows(&[9, 1]);
        let second_group = match &wrong_representative.groups[1].row.values()[0] {
            SqliteValue::Text(value) => value.clone(),
            _ => unreachable!("group_location 必须为 TEXT"),
        };
        let root_resource = RpgMakerProjectionCodec::encode_mutation_resource(
            &RpgMakerLocation::value(RpgMakerSource::data(StandardDataFile::Items), Vec::new()),
        )
        .expect("根资源应可编码");
        let row_index = wrong_representative
            .claims
            .iter()
            .position(|row| {
                matches!(
                    row.row.values(),
                    [_, SqliteValue::Text(resource), _] if resource == &root_resource
                )
            })
            .expect("测试摘要应包含共享根 Intent");
        let mut values = wrong_representative.claims[row_index]
            .clone()
            .row
            .into_values();
        values[0] = SqliteValue::Text(second_group);
        wrong_representative.claims[row_index].row = SqliteRow::new(values);
        assert!(matches!(
            assemble_test_rows(wrong_representative),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Representative,
                ..
            })
        ));
    }

    #[test]
    fn claim_summary_diagnostic_exposes_human_resources_without_compact_json() {
        let mut rows = scalar_snapshot_rows(&[9, 1]);
        rows.claims.pop();
        let error = assemble_test_rows(rows).expect_err("缺少摘要行必须失败");
        let violation = error.diagnostic_violation();
        let RpgMakerWriteBackAssetSnapshotViolation::InvalidClaimSummary { details, .. } =
            &violation
        else {
            panic!("缺少摘要行应保留类型化资源位置")
        };
        assert!(details.expected_resource.is_some());
        let diagnostic = serde_json::to_string(&violation).expect("安全诊断应可序列化");

        assert!(diagnostic.contains("expected_resource"));
        assert!(!diagnostic.contains("[\\\"v\\\""));
    }

    #[test]
    fn collision_summary_validation_borrows_the_earliest_representative() {
        let mut claims = vec![
            EncodedMutationClaim::new(
                "resource".to_owned(),
                MutationResourceAccess::Intent,
                "group-a".to_owned(),
                RpgMakerSemanticOrderKey::new(vec![8], 0),
            ),
            EncodedMutationClaim::new(
                "resource".to_owned(),
                MutationResourceAccess::Intent,
                "group-z".to_owned(),
                RpgMakerSemanticOrderKey::new(vec![2], 0),
            ),
        ];
        sort_logical_claims(&mut claims);
        let earliest = claims
            .iter()
            .find(|claim| claim.semantic_order_key.physical_path() == [2])
            .expect("测试 Claim 应包含最早自然组");

        let representative = borrowed_collision_summary(&claims)
            .next()
            .expect("测试摘要应包含一行")
            .expect("测试 Claim 不应冲突");

        assert!(std::ptr::eq(representative, earliest));
    }

    #[test]
    fn claim_summary_validation_consumes_sql_order_without_resorting() {
        let mut rows = scalar_snapshot_rows(&[9, 1]);
        assert!(rows.claims.len() >= 2, "测试摘要至少需要两行");
        rows.claims.swap(0, 1);

        assert!(matches!(
            assemble_test_rows(rows),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Resource,
                row_index: 0,
                ..
            })
        ));
    }

    #[test]
    fn later_duplicate_claim_does_not_hide_an_earlier_access_mismatch() {
        let mut rows = scalar_snapshot_rows(&[9, 1]);
        assert!(rows.claims.len() >= 2, "测试摘要至少需要两行");
        let duplicated = rows.claims.last().expect("测试摘要必须有末行").clone();
        rows.claims.push(duplicated);

        let mut first = rows.claims[0].clone().row.into_values();
        let SqliteValue::Text(access) = &mut first[2] else {
            unreachable!("access 必须为 TEXT")
        };
        *access = match access.as_str() {
            "intent" => "exclusive".to_owned(),
            "exclusive" => "intent".to_owned(),
            other => unreachable!("测试 access 无效：{other}"),
        };
        rows.claims[0].row = SqliteRow::new(first);

        assert!(matches!(
            assemble_test_rows(rows),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Access,
                row_index: 0,
                ..
            })
        ));
    }

    #[test]
    fn claim_resource_decode_is_deferred_until_the_first_summary_mismatch() {
        let mut rows = scalar_snapshot_rows(&[1]);
        let mut values = rows.claims[0].clone().row.into_values();
        let SqliteValue::Text(resource) = &mut values[1] else {
            unreachable!("resource_key 必须为 TEXT")
        };
        *resource = format!(" {resource} ");
        rows.claims[0].row = SqliteRow::new(values);

        assert!(matches!(
            decode_claim(rows.claims[0].clone()),
            Ok(DecodedRecord::Claim {
                resource_key_raw,
                ..
            }) if resource_key_raw.starts_with(' ')
        ));
        match assemble_test_rows(rows) {
            Err(InvalidRpgMakerWriteBackAssetSnapshot::NonCanonicalMutationResource { .. }) => {}
            Err(error) => panic!("实际错误：{error:?}"),
            Ok(_) => panic!("非规范 resource_key 不得成功"),
        }
    }

    #[test]
    fn invalid_claim_resource_keeps_projection_error_after_lazy_decode() {
        let mut rows = scalar_snapshot_rows(&[1]);
        let mut values = rows.claims[0].clone().row.into_values();
        values[1] = SqliteValue::Text("!".to_owned());
        rows.claims[0].row = SqliteRow::new(values);

        assert!(matches!(
            decode_claim(rows.claims[0].clone()),
            Ok(DecodedRecord::Claim {
                resource_key_raw,
                ..
            }) if resource_key_raw == "!"
        ));
        assert!(matches!(
            assemble_test_rows(rows),
            Err(InvalidRpgMakerWriteBackAssetSnapshot::InvalidProjection(_))
        ));
    }
}
