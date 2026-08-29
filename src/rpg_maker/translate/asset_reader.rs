//! 从 RPG Maker 文本表建立一致翻译语料。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, RpgMakerDiagnosticStage, RpgMakerIssue, RpgMakerJsonFailureKind,
    RpgMakerProjectProblem, RpgMakerSemanticOrderLevel, RpgMakerTranslationAssetComputeOperation,
    RpgMakerTranslationAssetProblem, RpgMakerTranslationResourceKind,
    RpgMakerTranslationSnapshotViolation, SafeIdentifier, SafePath, SqliteDiagnosticContext,
    SqliteDiagnosticStage, SqliteOperation, SqliteTransactionState, StateEffect,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::asset_storage::{
    OwnerPartitionedSqliteRow as OwnerSqliteRow,
    RPG_MAKER_ASSET_OWNER_ORDER as TRANSLATION_OWNER_ORDER, RPG_MAKER_ASSET_OWNER_STATE_PROJECTION,
    RPG_MAKER_TEXT_GROUP_CORE_PROJECTION, RPG_MAKER_TEXT_UNIT_CONTENT_PROJECTION,
    RpgMakerAssetOwnerStateStorageRow, RpgMakerAssetStorageRowDecoder,
    RpgMakerAssetStorageRowError, RpgMakerTextGroupStorageRow, RpgMakerTextUnitIdentityStorageRow,
    RpgMakerTextUnitLocationStorageRow, RpgMakerTextUnitStorageRow, merge_owner_partitions,
    rpg_maker_asset_owner_order as owner_order, sort_owner_state_rows,
};
#[cfg(test)]
use crate::rpg_maker::location_codec::RpgMakerLocationCodec;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodecError, RpgMakerProjectionCodec, RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{
    TextUnitContent, TextUnitContentStructureError, TextUnitContentView, TextUnitRole,
    validate_text_unit_content_structure,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    AssetSnapshotFingerprint, PLACEHOLDER_RULES_RESOURCE_KIND, SourceSnapshotFingerprint,
    TERMINOLOGY_RESOURCE_KIND,
};
use crate::rpg_maker::semantic_order::{
    RpgMakerSemanticOrderKey, RpgMakerSemanticOrderKeyDecodeError, RpgMakerSemanticScopeError,
    RpgMakerSemanticScopeKey,
};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::runtime::sqlite::SqliteRuntimeError;
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::{ProvenInvariantViolation, is_structural_blank};

use super::pipeline::{
    RpgMakerStoredRejectedTranslation, RpgMakerTranslationAsset, RpgMakerTranslationAssetReader,
    RpgMakerTranslationCorpus, RpgMakerTranslationGroup, RpgMakerTranslationScope,
    TranslationOwnerSnapshot, TranslationUnitIdentity,
};

const READ_TRANSLATION_METADATA: &str = "SELECT source_snapshot_fingerprint FROM metadata";

const READ_TRANSLATION_RESOURCES: &str = r#"SELECT resource_kind, canonical_json
FROM rpg_maker_translation_resource
WHERE resource_kind IN ('terminology', 'placeholder_rules')
ORDER BY resource_kind"#;

const TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT: usize = 3 + TRANSLATION_OWNER_ORDER.len() * 2;

fn read_translation_owners() -> String {
    format!(
        "SELECT\n    {RPG_MAKER_ASSET_OWNER_STATE_PROJECTION}\n\
         FROM rpg_maker_asset_owner_state"
    )
}

fn read_translation_owner_groups() -> String {
    format!(
        "SELECT\n    {RPG_MAKER_TEXT_GROUP_CORE_PROJECTION}\n\
         FROM rpg_maker_text_group AS text_group\n\
         WHERE text_group.owner = ?\n\
         ORDER BY text_group.semantic_order_key"
    )
}

fn read_translation_owner_units() -> String {
    format!(
        "SELECT\n    text_group.group_location,\n    \
         text_group.group_kind,\n    \
         text_group.semantic_order_key,\n    \
         {RPG_MAKER_TEXT_UNIT_CONTENT_PROJECTION},\n    \
         unit.translation_state,\n    \
         text_group.projection_recipe_json,\n    \
         manual.translation_json,\n    \
         manual.applicability_fingerprint,\n    \
         rejected.readable_id,\n    \
         rejected.origin,\n    \
         rejected.source_content_json,\n    \
         rejected.source_context_json,\n    \
         rejected.candidate_json,\n    \
         rejected.translation_json,\n    \
         rejected.violation_json,\n    \
         rejected.planning_state\n\
         FROM rpg_maker_text_group AS text_group\n\
         CROSS JOIN rpg_maker_text_unit AS unit\n  \
                    INDEXED BY rpg_maker_text_unit_owner_group_order_idx\n  \
           ON unit.owner = text_group.owner\n \
          AND unit.group_id = text_group.group_id\n\
         LEFT JOIN rpg_maker_manual_translation AS manual\n  \
           ON manual.owner = text_group.owner\n \
          AND manual.group_location = text_group.group_location\n \
          AND manual.unit_role = unit.unit_role\n\
         LEFT JOIN rpg_maker_rejected_translation AS rejected\n  \
           ON rejected.owner = unit.owner\n \
          AND rejected.group_id = unit.group_id\n \
          AND rejected.unit_role = unit.unit_role\n\
         WHERE text_group.owner = ?\n\
         ORDER BY text_group.semantic_order_key,\n         \
          unit.semantic_order_key"
    )
}

fn translation_snapshot_queries() -> Vec<SqliteQuery> {
    let mut queries = Vec::with_capacity(TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT);
    queries.extend([
        SqliteQuery::new(READ_TRANSLATION_METADATA, Vec::new()).with_id("translation.metadata"),
        SqliteQuery::new(read_translation_owners(), Vec::new()).with_id("translation.owners"),
        SqliteQuery::new(READ_TRANSLATION_RESOURCES, Vec::new()).with_id("translation.resources"),
    ]);
    for (kind, statement) in [
        ("groups", read_translation_owner_groups()),
        ("units", read_translation_owner_units()),
    ] {
        queries.extend(TRANSLATION_OWNER_ORDER.map(|owner| {
            SqliteQuery::new(
                statement.clone(),
                vec![SqliteValue::Text(owner.storage_name().to_owned())],
            )
            .with_id(format!("translation.{}.{kind}", owner.storage_name()))
        }));
    }
    queries
}

/// 验证 owner 新鲜度、读取当前资源，并用受控 CPU 解码 RPG Maker 翻译语料。
pub(crate) struct RpgMakerTranslationAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
}

impl<Q, C> RpgMakerTranslationAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C) -> Self {
        Self { sqlite, cpu }
    }
}

impl<Q, C> RpgMakerTranslationAssetReader for RpgMakerTranslationAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerTranslationAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
    ) -> Result<RpgMakerTranslationCorpus, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let query_results = self
            .sqlite
            .query_existing_database_snapshot(database_path.clone(), translation_snapshot_queries())
            .await
            .map_err(|error| map_query_error(database_path.clone(), error))?;
        let expected_source_snapshot = project.source_snapshot_fingerprint();
        let source_language = project.source_language().as_str().to_owned();
        let target_language = project.target_language().as_str().to_owned();
        let preparation_database_path = database_path.clone();
        let prepared = self
            .cpu
            .execute(move || prepare_snapshot(query_results, expected_source_snapshot))
            .await
            .map_err(
                |source| RpgMakerTranslationAssetReadingError::SchedulePreparation {
                    database_path: preparation_database_path,
                    source,
                },
            )?
            .map_err(|error| map_snapshot_preparation_error(database_path.clone(), error))?;
        let active_owners = prepared.active_owners;
        let decoded_groups = prepared.groups;
        let decode_database_path = database_path.clone();
        let decoded_units = self
            .cpu
            .execute_ordered_map(prepared.units, move |row| {
                decode_unit_for_language(row, active_owners, &source_language, &target_language)
            })
            .await
            .map_err(
                |source| RpgMakerTranslationAssetReadingError::ScheduleDecode {
                    database_path: decode_database_path,
                    source,
                },
            )?;
        let assembly_database_path = database_path.clone();
        let groups = self
            .cpu
            .execute(move || {
                let decoded = decoded_units.into_iter().collect::<Result<Vec<_>, _>>()?;
                assemble_corpus(decoded_groups, decoded)
            })
            .await
            .map_err(
                |source| RpgMakerTranslationAssetReadingError::ScheduleAssembly {
                    database_path: assembly_database_path,
                    source,
                },
            )?
            .map_err(
                |source| RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                    database_path,
                    source,
                },
            )?;
        Ok(RpgMakerTranslationCorpus::with_snapshot(
            groups,
            prepared.source_snapshot_fingerprint,
            prepared.owner_snapshots,
            prepared.terminology_json,
            prepared.placeholder_rules_json,
        ))
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerTranslationAssetReadingError<Q, C> {
    DatabaseNotFound {
        database_path: PathBuf,
    },
    Query {
        database_path: PathBuf,
        source: Q,
    },
    ProjectSnapshotChanged {
        database_path: PathBuf,
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
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
        source: InvalidRpgMakerTranslationAssetSnapshot,
    },
}

impl<Q: fmt::Display, C: fmt::Display> fmt::Display for RpgMakerTranslationAssetReadingError<Q, C> {
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
                "无法从 {} 读取 RPG Maker 翻译资产：{source}",
                database_path.display()
            ),
            Self::ProjectSnapshotChanged {
                database_path,
                expected,
                actual,
            } => write!(
                formatter,
                "项目打开后 {} 的 metadata 来源指纹发生变化（预期 {expected:?}，实际 {actual:?}）",
                database_path.display()
            ),
            Self::ExtractionOutOfDate {
                database_path,
                owners,
            } => write!(
                formatter,
                "{} 的 RPG Maker 资产提取已过期：{}",
                database_path.display(),
                owners
                    .iter()
                    .map(|owner| owner.storage_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::SchedulePreparation {
                database_path,
                source,
            } => {
                write!(
                    formatter,
                    "{} 的资产快照准备任务执行失败：{source}",
                    database_path.display()
                )
            }
            Self::ScheduleDecode {
                database_path,
                source,
            } => write!(
                formatter,
                "{} 的资产解码任务执行失败：{source}",
                database_path.display()
            ),
            Self::ScheduleAssembly {
                database_path,
                source,
            } => {
                write!(
                    formatter,
                    "{} 的资产语料组装任务执行失败：{source}",
                    database_path.display()
                )
            }
            Self::InvalidSnapshot {
                database_path,
                source,
            } => {
                write!(
                    formatter,
                    "{} 的 RPG Maker 翻译资产损坏：{source}",
                    database_path.display()
                )
            }
        }
    }
}

impl<Q: Error + 'static, C: Error + 'static> Error for RpgMakerTranslationAssetReadingError<Q, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::SchedulePreparation { source, .. }
            | Self::ScheduleDecode { source, .. }
            | Self::ScheduleAssembly { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            Self::DatabaseNotFound { .. }
            | Self::ProjectSnapshotChanged { .. }
            | Self::ExtractionOutOfDate { .. } => None,
        }
    }
}

impl RpgMakerTranslationAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable> {
    /// 在仍掌握数据库路径、快照阶段和叶子结构时建立唯一公开报告。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::DatabaseNotFound { database_path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::rpg_maker(RpgMakerIssue::project(
                    RpgMakerDiagnosticStage::TranslatePlanning,
                    RpgMakerProjectProblem::DatabaseNotFound {
                        path: SafePath::new(database_path),
                    },
                )),
            ),
            Self::Query {
                database_path,
                source,
            } => source.diagnostic_report(
                database_path,
                SqliteDiagnosticContext::new(
                    SqliteDiagnosticStage::Translate,
                    SqliteOperation::Query,
                    translation_snapshot_query_transaction(source),
                ),
                StateEffect::Unchanged,
            ),
            Self::ProjectSnapshotChanged {
                database_path,
                expected,
                actual,
            } => translation_asset_report(
                database_path,
                RpgMakerTranslationAssetProblem::ProjectSnapshotChanged {
                    expected: SafeIdentifier::from_validated(expected.hex()),
                    actual: SafeIdentifier::from_validated(actual.hex()),
                },
            ),
            Self::ExtractionOutOfDate {
                database_path,
                owners,
            } => translation_asset_report(
                database_path,
                RpgMakerTranslationAssetProblem::ExtractionOutOfDate {
                    owners: owners
                        .iter()
                        .map(|owner| owner.diagnostic_owner())
                        .collect(),
                },
            ),
            Self::SchedulePreparation {
                database_path,
                source,
            } => translation_asset_compute_report(
                database_path,
                RpgMakerTranslationAssetComputeOperation::PrepareSnapshot,
                source,
            ),
            Self::ScheduleDecode {
                database_path,
                source,
            } => translation_asset_compute_report(
                database_path,
                RpgMakerTranslationAssetComputeOperation::DecodeUnits,
                source,
            ),
            Self::ScheduleAssembly {
                database_path,
                source,
            } => translation_asset_compute_report(
                database_path,
                RpgMakerTranslationAssetComputeOperation::AssembleCorpus,
                source,
            ),
            Self::InvalidSnapshot {
                database_path,
                source,
            } => translation_asset_report(
                database_path,
                RpgMakerTranslationAssetProblem::InvalidSnapshot {
                    violation: source.diagnostic_violation(),
                },
            ),
        }
    }
}

fn translation_asset_compute_report(
    database_path: &std::path::Path,
    operation: RpgMakerTranslationAssetComputeOperation,
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> DiagnosticReport {
    let failure = crate::rpg_maker::compute_failure(source);
    translation_asset_report(
        database_path,
        RpgMakerTranslationAssetProblem::Compute { operation, failure },
    )
}

fn translation_asset_report(
    database_path: &std::path::Path,
    problem: RpgMakerTranslationAssetProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::translation_asset(database_path, problem)),
    )
}

fn translation_snapshot_query_transaction(source: &SqliteRuntimeError) -> SqliteTransactionState {
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

impl InvalidRpgMakerTranslationAssetSnapshot {
    fn diagnostic_violation(&self) -> RpgMakerTranslationSnapshotViolation {
        match self {
            Self::WrongQueryResultSetCount { expected, actual } => {
                RpgMakerTranslationSnapshotViolation::WrongQueryResultSetCount {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::WrongColumnCount { expected, actual } => {
                RpgMakerTranslationSnapshotViolation::WrongColumnCount {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => RpgMakerTranslationSnapshotViolation::WrongColumnType {
                column: SafeIdentifier::from_validated(column),
                expected: SafeIdentifier::from_validated(expected),
                actual: SafeIdentifier::from_validated(actual),
            },
            Self::InvalidSemanticOrderKey { column, source } => {
                RpgMakerTranslationSnapshotViolation::InvalidSemanticOrderKey {
                    column: SafeIdentifier::from_validated(column),
                    violation: source.diagnostic_violation(),
                }
            }
            Self::UnknownOwner => RpgMakerTranslationSnapshotViolation::UnknownOwner,
            Self::InactiveOwner(owner) => RpgMakerTranslationSnapshotViolation::InactiveOwner {
                owner: owner.diagnostic_owner(),
            },
            Self::DuplicateOwner(owner) => RpgMakerTranslationSnapshotViolation::DuplicateOwner {
                owner: owner.diagnostic_owner(),
            },
            Self::InvalidOwnerSourceFingerprintLength { owner, actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidOwnerSourceFingerprintLength {
                    owner: owner.diagnostic_owner(),
                    actual: *actual,
                }
            }
            Self::InvalidOwnerAssetFingerprintLength { owner, actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidOwnerAssetFingerprintLength {
                    owner: owner.diagnostic_owner(),
                    actual: *actual,
                }
            }
            Self::InvalidMetadataRowCount { actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidMetadataRowCount { actual: *actual }
            }
            Self::InvalidMetadataFingerprintLength { actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidMetadataFingerprintLength {
                    actual: *actual,
                }
            }
            Self::MissingTranslationResource(kind) => {
                RpgMakerTranslationSnapshotViolation::MissingTranslationResource {
                    resource_kind: kind.diagnostic_kind(),
                }
            }
            Self::DuplicateTranslationResource(kind) => {
                RpgMakerTranslationSnapshotViolation::DuplicateTranslationResource {
                    resource_kind: kind.diagnostic_kind(),
                }
            }
            Self::UnknownTranslationResource => {
                RpgMakerTranslationSnapshotViolation::UnknownTranslationResource
            }
            Self::BlankTranslationResource(kind) => {
                RpgMakerTranslationSnapshotViolation::BlankTranslationResource {
                    resource_kind: kind.diagnostic_kind(),
                }
            }
            Self::UnknownGroupKind => RpgMakerTranslationSnapshotViolation::UnknownGroupKind,
            Self::InvalidLocation(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidLocation {
                    failure: source.diagnostic_failure(),
                }
            }
            Self::InvalidSemanticScope(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidSemanticScope {
                    group_location: source.location().diagnostic_location(),
                }
            }
            Self::InvalidRole(source) => RpgMakerTranslationSnapshotViolation::InvalidRole {
                failure: source.diagnostic_failure(),
            },
            Self::RoleDoesNotBelongToGroup { role, kind } => {
                RpgMakerTranslationSnapshotViolation::RoleDoesNotBelongToGroup {
                    role: role.diagnostic_role(),
                    group_kind: kind.diagnostic_group_kind(),
                }
            }
            Self::InvalidSourceContent(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidSourceContent {
                    category: asset_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::InvalidTranslationContent(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationContent {
                    category: asset_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::SourceContentShapeMismatch { role } => {
                RpgMakerTranslationSnapshotViolation::SourceContentShapeMismatch {
                    role: role.diagnostic_role(),
                }
            }
            Self::TranslationContentShapeMismatch { role } => {
                RpgMakerTranslationSnapshotViolation::TranslationContentShapeMismatch {
                    role: role.diagnostic_role(),
                }
            }
            Self::BlankSourceContent => RpgMakerTranslationSnapshotViolation::BlankSourceContent,
            Self::BlankTranslationContent => {
                RpgMakerTranslationSnapshotViolation::BlankTranslationContent
            }
            Self::InvalidSourceLineText { index } => {
                RpgMakerTranslationSnapshotViolation::InvalidSourceLineText { index: *index }
            }
            Self::InvalidTranslationLineText { index } => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationLineText { index: *index }
            }
            Self::AlignedLineCountMismatch { expected, actual } => {
                RpgMakerTranslationSnapshotViolation::AlignedLineCountMismatch {
                    expected: *expected,
                    actual: *actual,
                }
            }
            Self::AlignedBlankSlotMismatch { index } => {
                RpgMakerTranslationSnapshotViolation::AlignedBlankSlotMismatch { index: *index }
            }
            Self::InvalidSourceContext(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidSourceContext {
                    category: asset_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::SourceContextMustBeObject => {
                RpgMakerTranslationSnapshotViolation::SourceContextMustBeObject
            }
            Self::InvalidTranslationStatePair => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationStatePair
            }
            Self::InvalidTranslationStateLength { actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationStateLength {
                    actual: *actual,
                }
            }
            Self::InvalidRejectedTranslation(source) | Self::InvalidRejectedViolation(source) => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationContent {
                    category: asset_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::InvalidRejectedStatePair => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationStatePair
            }
            Self::InvalidRejectedStateLength { actual } => {
                RpgMakerTranslationSnapshotViolation::InvalidTranslationStateLength {
                    actual: *actual,
                }
            }
            Self::DuplicateGroup {
                owner,
                group_location,
            } => RpgMakerTranslationSnapshotViolation::DuplicateGroup {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::MissingGroup {
                owner,
                group_location,
            } => RpgMakerTranslationSnapshotViolation::MissingGroup {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::EmptyGroup {
                owner,
                group_location,
            } => RpgMakerTranslationSnapshotViolation::EmptyGroup {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => RpgMakerTranslationSnapshotViolation::InconsistentGroupDefinition {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
            },
            Self::DuplicateSemanticOrderKey { level } => {
                RpgMakerTranslationSnapshotViolation::DuplicateSemanticOrderKey {
                    level: level.diagnostic_level(),
                }
            }
            Self::DuplicateLogicalUnit {
                owner,
                group_location,
                role,
            } => RpgMakerTranslationSnapshotViolation::DuplicateLogicalUnit {
                owner: owner.diagnostic_owner(),
                group_location: group_location.diagnostic_location(),
                role: role.diagnostic_role(),
            },
        }
    }
}

impl TranslationSnapshotResourceKind {
    const fn diagnostic_kind(self) -> RpgMakerTranslationResourceKind {
        match self {
            Self::Terminology => RpgMakerTranslationResourceKind::Terminology,
            Self::PlaceholderRules => RpgMakerTranslationResourceKind::PlaceholderRules,
        }
    }
}

impl TranslationSnapshotSemanticOrderLevel {
    const fn diagnostic_level(self) -> RpgMakerSemanticOrderLevel {
        match self {
            Self::Group => RpgMakerSemanticOrderLevel::Group,
            Self::Unit => RpgMakerSemanticOrderLevel::Unit,
        }
    }
}

fn asset_json_failure(source: &serde_json::Error) -> RpgMakerJsonFailureKind {
    JsonErrorCategory::from(source).into()
}

#[derive(Debug)]
pub(crate) enum InvalidRpgMakerTranslationAssetSnapshot {
    WrongQueryResultSetCount {
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
    InvalidSemanticOrderKey {
        column: &'static str,
        source: RpgMakerSemanticOrderKeyDecodeError,
    },
    UnknownOwner,
    InactiveOwner(RpgMakerAssetOwner),
    DuplicateOwner(RpgMakerAssetOwner),
    InvalidOwnerSourceFingerprintLength {
        owner: RpgMakerAssetOwner,
        actual: usize,
    },
    InvalidOwnerAssetFingerprintLength {
        owner: RpgMakerAssetOwner,
        actual: usize,
    },
    InvalidMetadataRowCount {
        actual: usize,
    },
    InvalidMetadataFingerprintLength {
        actual: usize,
    },
    MissingTranslationResource(TranslationSnapshotResourceKind),
    DuplicateTranslationResource(TranslationSnapshotResourceKind),
    UnknownTranslationResource,
    BlankTranslationResource(TranslationSnapshotResourceKind),
    UnknownGroupKind,
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidSemanticScope(RpgMakerSemanticScopeError),
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
    InvalidRejectedTranslation(serde_json::Error),
    InvalidRejectedViolation(serde_json::Error),
    InvalidRejectedStatePair,
    InvalidRejectedStateLength {
        actual: usize,
    },
    DuplicateGroup {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    MissingGroup {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    EmptyGroup {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InconsistentGroupDefinition {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    DuplicateSemanticOrderKey {
        level: TranslationSnapshotSemanticOrderLevel,
    },
    DuplicateLogicalUnit {
        owner: RpgMakerAssetOwner,
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationSnapshotResourceKind {
    Terminology,
    PlaceholderRules,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationSnapshotSemanticOrderLevel {
    Group,
    Unit,
}

impl fmt::Display for InvalidRpgMakerTranslationAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongQueryResultSetCount { expected, actual } => write!(
                formatter,
                "翻译快照应返回 {expected} 组查询结果，实际为 {actual} 组"
            ),
            Self::WrongColumnCount { expected, actual } => {
                write!(formatter, "查询行应包含 {expected} 列，实际为 {actual} 列")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "列 {column} 应为 {expected}，实际为 {actual}"),
            Self::InvalidSemanticOrderKey { column, source } => {
                write!(formatter, "列 {column} 不是规范语义顺序键：{source}")
            }
            Self::UnknownOwner => formatter.write_str("未知资产所有者"),
            Self::InactiveOwner(owner) => write!(
                formatter,
                "文本单元引用未激活 owner：{}",
                owner.storage_name()
            ),
            Self::DuplicateOwner(owner) => {
                write!(formatter, "资产 owner 状态重复：{}", owner.storage_name())
            }
            Self::InvalidOwnerSourceFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {} 的来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节",
                owner.storage_name()
            ),
            Self::InvalidOwnerAssetFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {} 的资产指纹必须是 32 字节 BLOB，实际为 {actual} 字节",
                owner.storage_name()
            ),
            Self::InvalidMetadataRowCount { actual } => {
                write!(formatter, "metadata 必须恰好一行，实际为 {actual} 行")
            }
            Self::InvalidMetadataFingerprintLength { actual } => write!(
                formatter,
                "metadata 来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::MissingTranslationResource(kind) => write!(formatter, "缺少翻译资源 {kind:?}"),
            Self::DuplicateTranslationResource(kind) => {
                write!(formatter, "翻译资源重复：{kind:?}")
            }
            Self::UnknownTranslationResource => formatter.write_str("未知翻译资源"),
            Self::BlankTranslationResource(kind) => {
                write!(formatter, "翻译资源 {kind:?} 为空")
            }
            Self::UnknownGroupKind => formatter.write_str("未知文本组类型"),
            Self::InvalidLocation(source) => write!(formatter, "组位置无效：{source}"),
            Self::InvalidSemanticScope(source) => write!(formatter, "语义范围无效：{source}"),
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
            Self::BlankSourceContent => formatter.write_str("RPG Maker 文本源内容仅包含空白"),
            Self::BlankTranslationContent => {
                formatter.write_str("RPG Maker 文本译文内容仅包含空白")
            }
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
            Self::InvalidRejectedTranslation(source) => {
                write!(formatter, "Rejected 的字符串数组投影无效：{source}")
            }
            Self::InvalidRejectedViolation(source) => {
                write!(formatter, "Rejected 的硬不变量原因无效：{source}")
            }
            Self::InvalidRejectedStatePair => {
                formatter.write_str("Rejected 的持久字段必须同时存在或同时为空")
            }
            Self::InvalidRejectedStateLength { actual } => write!(
                formatter,
                "Rejected planning_state 必须是 32 字节 BLOB，实际为 {actual} 字节"
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
            Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => write!(
                formatter,
                "同一资产组的类型或 semantic_order_key 不一致：{} / {group_location}",
                owner.storage_name()
            ),
            Self::DuplicateSemanticOrderKey { level } => {
                write!(formatter, "不同 {level:?} 使用了相同 semantic_order_key")
            }
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

impl Error for InvalidRpgMakerTranslationAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidSemanticOrderKey { source, .. } => Some(source),
            Self::InvalidSemanticScope(source) => Some(source),
            Self::InvalidRole(source) => Some(source),
            Self::InvalidSourceContent(source)
            | Self::InvalidTranslationContent(source)
            | Self::InvalidSourceContext(source)
            | Self::InvalidRejectedTranslation(source)
            | Self::InvalidRejectedViolation(source) => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> RpgMakerTranslationAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerTranslationAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerTranslationAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ActiveOwners([bool; TRANSLATION_OWNER_ORDER.len()]);

impl ActiveOwners {
    fn insert(&mut self, owner: RpgMakerAssetOwner) -> bool {
        let active = &mut self.0[owner_order(owner)];
        let inserted = !*active;
        *active = true;
        inserted
    }

    fn contains(self, owner: RpgMakerAssetOwner) -> bool {
        self.0[owner_order(owner)]
    }
}

struct PreparedSnapshot {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    owner_snapshots: Vec<TranslationOwnerSnapshot>,
    active_owners: ActiveOwners,
    terminology_json: String,
    placeholder_rules_json: String,
    groups: Vec<DecodedGroup>,
    units: Vec<OwnerSqliteRow>,
}

enum SnapshotPreparationError {
    Invalid(InvalidRpgMakerTranslationAssetSnapshot),
    WrongQueryResultSetCount {
        actual: usize,
    },
    ProjectSnapshotChanged {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    ExtractionOutOfDate {
        owners: Vec<RpgMakerAssetOwner>,
    },
}

fn prepare_snapshot(
    query_results: Vec<Vec<SqliteRow>>,
    expected_source_snapshot: SourceSnapshotFingerprint,
) -> Result<PreparedSnapshot, SnapshotPreparationError> {
    let actual = query_results.len();
    let [
        metadata,
        owners,
        resources,
        builtin_groups,
        rules_groups,
        builtin_units,
        rules_units,
    ]: [Vec<SqliteRow>; TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT] = query_results
        .try_into()
        .map_err(|_| SnapshotPreparationError::WrongQueryResultSetCount { actual })?;
    let source_snapshot_fingerprint =
        decode_metadata(metadata).map_err(SnapshotPreparationError::Invalid)?;
    if source_snapshot_fingerprint != expected_source_snapshot {
        return Err(SnapshotPreparationError::ProjectSnapshotChanged {
            expected: expected_source_snapshot,
            actual: source_snapshot_fingerprint,
        });
    }

    let owner_states = decode_owner_states(owners, source_snapshot_fingerprint)
        .map_err(SnapshotPreparationError::Invalid)?;
    if !owner_states.stale.is_empty() {
        return Err(SnapshotPreparationError::ExtractionOutOfDate {
            owners: owner_states.stale,
        });
    }
    let (terminology_json, placeholder_rules_json) =
        decode_resources(resources).map_err(SnapshotPreparationError::Invalid)?;
    let groups = decode_groups(
        merge_owner_partitions([builtin_groups, rules_groups]),
        owner_states.active,
    )
    .map_err(SnapshotPreparationError::Invalid)?;
    let units = merge_owner_partitions([builtin_units, rules_units]);

    Ok(PreparedSnapshot {
        source_snapshot_fingerprint,
        owner_snapshots: owner_states.snapshots,
        active_owners: owner_states.active,
        terminology_json,
        placeholder_rules_json,
        groups,
        units,
    })
}

fn map_snapshot_preparation_error<Q, C>(
    database_path: PathBuf,
    error: SnapshotPreparationError,
) -> RpgMakerTranslationAssetReadingError<Q, C> {
    match error {
        SnapshotPreparationError::Invalid(source) => {
            RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source,
            }
        }
        SnapshotPreparationError::WrongQueryResultSetCount { actual } => {
            RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source: InvalidRpgMakerTranslationAssetSnapshot::WrongQueryResultSetCount {
                    expected: TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT,
                    actual,
                },
            }
        }
        SnapshotPreparationError::ProjectSnapshotChanged { expected, actual } => {
            RpgMakerTranslationAssetReadingError::ProjectSnapshotChanged {
                database_path,
                expected,
                actual,
            }
        }
        SnapshotPreparationError::ExtractionOutOfDate { owners } => {
            RpgMakerTranslationAssetReadingError::ExtractionOutOfDate {
                database_path,
                owners,
            }
        }
    }
}

fn decode_metadata(
    rows: Vec<SqliteRow>,
) -> Result<SourceSnapshotFingerprint, InvalidRpgMakerTranslationAssetSnapshot> {
    if rows.len() != 1 {
        return Err(
            InvalidRpgMakerTranslationAssetSnapshot::InvalidMetadataRowCount { actual: rows.len() },
        );
    }
    let mut row =
        RpgMakerAssetStorageRowDecoder::new(rows.into_iter().next().expect("已确认有一行"), 1)
            .map_err(map_storage_row_error)?;
    let bytes = row
        .required_blob("metadata.source_snapshot_fingerprint")
        .map_err(map_storage_row_error)?;
    SourceSnapshotFingerprint::from_slice(&bytes).map_err(|error| {
        InvalidRpgMakerTranslationAssetSnapshot::InvalidMetadataFingerprintLength {
            actual: error.actual(),
        }
    })
}

struct DecodedOwnerStates {
    stale: Vec<RpgMakerAssetOwner>,
    active: ActiveOwners,
    snapshots: Vec<TranslationOwnerSnapshot>,
}

fn decode_owner_states(
    mut rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
) -> Result<DecodedOwnerStates, InvalidRpgMakerTranslationAssetSnapshot> {
    sort_owner_state_rows(rows.as_mut_slice());
    let mut active = ActiveOwners::default();
    let mut stale = Vec::new();
    let mut snapshots = Vec::new();
    for row in rows {
        let RpgMakerAssetOwnerStateStorageRow {
            owner,
            source_snapshot_fingerprint: source_bytes,
            asset_snapshot_fingerprint: asset_bytes,
        } = RpgMakerAssetOwnerStateStorageRow::decode(row).map_err(map_storage_row_error)?;
        if !active.insert(owner) {
            return Err(InvalidRpgMakerTranslationAssetSnapshot::DuplicateOwner(
                owner,
            ));
        }
        let source = SourceSnapshotFingerprint::from_slice(&source_bytes).map_err(|error| {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidOwnerSourceFingerprintLength {
                owner,
                actual: error.actual(),
            }
        })?;
        let asset = AssetSnapshotFingerprint::from_slice(&asset_bytes).map_err(|error| {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidOwnerAssetFingerprintLength {
                owner,
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

fn decode_resources(
    rows: Vec<SqliteRow>,
) -> Result<(String, String), InvalidRpgMakerTranslationAssetSnapshot> {
    let mut terminology = None;
    let mut placeholders = None;
    for row in rows {
        let mut row = RpgMakerAssetStorageRowDecoder::new(row, 2).map_err(map_storage_row_error)?;
        let kind = row
            .required_text("resource_kind")
            .map_err(map_storage_row_error)?;
        let (resource, resource_kind) = match kind.as_str() {
            TERMINOLOGY_RESOURCE_KIND => (
                &mut terminology,
                TranslationSnapshotResourceKind::Terminology,
            ),
            PLACEHOLDER_RULES_RESOURCE_KIND => (
                &mut placeholders,
                TranslationSnapshotResourceKind::PlaceholderRules,
            ),
            _ => {
                return Err(InvalidRpgMakerTranslationAssetSnapshot::UnknownTranslationResource);
            }
        };
        let canonical_json = row
            .required_text("canonical_json")
            .map_err(map_storage_row_error)?;
        if canonical_json.is_empty() {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::BlankTranslationResource(resource_kind),
            );
        }
        if resource.is_some() {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::DuplicateTranslationResource(
                    resource_kind,
                ),
            );
        }
        *resource = Some(canonical_json);
    }
    let terminology = terminology.ok_or(
        InvalidRpgMakerTranslationAssetSnapshot::MissingTranslationResource(
            TranslationSnapshotResourceKind::Terminology,
        ),
    )?;
    let placeholders = placeholders.ok_or(
        InvalidRpgMakerTranslationAssetSnapshot::MissingTranslationResource(
            TranslationSnapshotResourceKind::PlaceholderRules,
        ),
    )?;
    Ok((terminology, placeholders))
}

#[derive(Debug)]
struct DecodedGroup {
    owner: RpgMakerAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    semantic_order_key: RpgMakerSemanticOrderKey,
}

fn decode_groups(
    rows: Vec<OwnerSqliteRow>,
    active_owners: ActiveOwners,
) -> Result<Vec<DecodedGroup>, InvalidRpgMakerTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|OwnerSqliteRow { owner, row }| {
            if !active_owners.contains(owner) {
                return Err(InvalidRpgMakerTranslationAssetSnapshot::InactiveOwner(
                    owner,
                ));
            }
            let mut row =
                RpgMakerAssetStorageRowDecoder::new(row, 3).map_err(map_storage_row_error)?;
            let RpgMakerTextGroupStorageRow {
                group_location,
                kind,
                semantic_order_key,
                ..
            } = RpgMakerTextGroupStorageRow::decode(&mut row).map_err(map_storage_row_error)?;
            Ok(DecodedGroup {
                owner,
                kind,
                group_location,
                semantic_order_key,
            })
        })
        .collect()
}

#[derive(Debug)]
struct DecodedUnit {
    owner: RpgMakerAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    group_semantic_order_key: RpgMakerSemanticOrderKey,
    role: TextUnitRole,
    semantic_order_key: RpgMakerSemanticOrderKey,
    source_content: TextUnitContent,
    source_context_json: String,
    recipe_shape: String,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
    manual: bool,
    rejected: Option<RpgMakerStoredRejectedTranslation>,
}

fn decode_unit_for_language(
    OwnerSqliteRow { owner, row }: OwnerSqliteRow,
    active_owners: ActiveOwners,
    source_language: &str,
    target_language: &str,
) -> Result<DecodedUnit, InvalidRpgMakerTranslationAssetSnapshot> {
    if !active_owners.contains(owner) {
        return Err(InvalidRpgMakerTranslationAssetSnapshot::InactiveOwner(
            owner,
        ));
    }
    let mut row = RpgMakerAssetStorageRowDecoder::new(row, 20).map_err(map_storage_row_error)?;
    let location =
        RpgMakerTextUnitLocationStorageRow::decode(&mut row).map_err(map_storage_row_error)?;
    let group_kind_raw = row
        .required_text("group_kind")
        .map_err(map_storage_row_error)?;
    let kind = TextGroupKind::from_storage_name(group_kind_raw.as_str())
        .ok_or(InvalidRpgMakerTranslationAssetSnapshot::UnknownGroupKind)?;
    let group_semantic_order_key = row
        .required_blob("semantic_order_key")
        .and_then(|encoded| {
            RpgMakerSemanticOrderKey::decode(&encoded).map_err(|source| {
                RpgMakerAssetStorageRowError::InvalidSemanticOrderKey {
                    column: "semantic_order_key",
                    source,
                }
            })
        })
        .map_err(map_storage_row_error)?;
    let identity = RpgMakerTextUnitIdentityStorageRow::decode_after_location(&mut row, location)
        .map_err(map_storage_row_error)?;
    let storage = RpgMakerTextUnitStorageRow::decode_after_identity(&mut row, identity)
        .map_err(map_storage_row_error)?;
    validate_persisted_source_content(kind, &storage.role, &storage.source_content)?;
    if storage.source_content.is_blank() {
        return Err(InvalidRpgMakerTranslationAssetSnapshot::BlankSourceContent);
    }
    let context: serde_json::Value = serde_json::from_str(storage.source_context_json.as_str())
        .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidSourceContext)?;
    if !context.is_object() {
        return Err(InvalidRpgMakerTranslationAssetSnapshot::SourceContextMustBeObject);
    }
    let translation = storage
        .decode_translation_content()
        .map_err(map_storage_row_error)?;
    if let Some(translation) = &translation {
        validate_persisted_translation_content(kind, &storage.role, translation)?;
    }
    if translation.as_ref().is_some_and(TextUnitContent::is_blank) {
        return Err(InvalidRpgMakerTranslationAssetSnapshot::BlankTranslationContent);
    }
    validate_persisted_alignment(&storage.role, &storage.source_content, translation.as_ref())?;
    let translation_state = row
        .optional_blob("translation_state")
        .map_err(map_storage_row_error)?;
    let automatic_translation_state = match (translation.as_ref(), translation_state) {
        (None, None) => None,
        (Some(_), Some(bytes)) => Some(Sha256Fingerprint::from_slice(&bytes).map_err(|error| {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationStateLength {
                actual: error.actual(),
            }
        })?),
        _ => return Err(InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationStatePair),
    };
    let projection_recipe_json = row
        .required_text("projection_recipe_json")
        .map_err(map_storage_row_error)?;
    let manual_translation_json = row
        .optional_text("manual.translation_json")
        .map_err(map_storage_row_error)?;
    let manual_state = row
        .optional_blob("manual.applicability_fingerprint")
        .map_err(map_storage_row_error)?;
    let rejected_readable_id = row
        .optional_text("rejected.readable_id")
        .map_err(map_storage_row_error)?;
    let rejected_origin = row
        .optional_text("rejected.origin")
        .map_err(map_storage_row_error)?;
    let rejected_source_content_json = row
        .optional_text("rejected.source_content_json")
        .map_err(map_storage_row_error)?;
    let rejected_source_context_json = row
        .optional_text("rejected.source_context_json")
        .map_err(map_storage_row_error)?;
    let rejected_candidate_json = row
        .optional_text("rejected.candidate_json")
        .map_err(map_storage_row_error)?;
    let rejected_translation_json = row
        .optional_text("rejected.translation_json")
        .map_err(map_storage_row_error)?;
    let rejected_violation_json = row
        .optional_text("rejected.violation_json")
        .map_err(map_storage_row_error)?;
    let rejected_planning_state = row
        .optional_blob("rejected.planning_state")
        .map_err(map_storage_row_error)?;
    let RpgMakerTextUnitStorageRow {
        group_location_raw,
        group_location,
        role_raw,
        role,
        semantic_order_key,
        source_content,
        source_context_json,
        ..
    } = storage;
    let identity = TranslationUnitIdentity::new(
        owner,
        kind,
        group_location.clone(),
        role.clone(),
        source_content.clone(),
        source_context_json.clone(),
    );
    let manual_type = crate::manual::rpg_maker_manual_type(&identity);
    let source_lines = crate::manual::rpg_maker_manual_source_lines(&source_content);
    let recipe_shape =
        RpgMakerProjectionCodec::encode_role_recipe_shape(&projection_recipe_json, &role)
            .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidRole)?;
    let expected_manual_state = crate::manual::rpg_maker_manual_applicability(
        crate::manual::RpgMakerManualApplicabilityFacts {
            owner: owner.storage_name(),
            group_location: &group_location_raw,
            kind: kind.storage_name(),
            role: &role_raw,
            recipe_shape: &recipe_shape,
            translation_type: manual_type,
            source_language,
            target_language,
            source: &source_lines,
        },
    );
    let manual_translation = match (manual_translation_json, manual_state) {
        (None, None) => None,
        (Some(translation_json), Some(state)) => {
            let state = Sha256Fingerprint::from_slice(&state).map_err(|error| {
                InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationStateLength {
                    actual: error.actual(),
                }
            })?;
            if state != expected_manual_state {
                None
            } else {
                let lines = serde_json::from_str::<Vec<String>>(&translation_json)
                    .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationContent)?;
                if lines.is_empty() {
                    return Err(InvalidRpgMakerTranslationAssetSnapshot::BlankTranslationContent);
                }
                let content =
                    crate::manual::rpg_maker_manual_translation_content(&source_content, lines);
                validate_persisted_translation_content(kind, &role, &content)?;
                validate_persisted_alignment(&role, &source_content, Some(&content))?;
                Some(content)
            }
        }
        _ => return Err(InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationStatePair),
    };
    let manual = manual_translation.is_some();
    let (translation, translation_state) = if let Some(translation) = manual_translation {
        (Some(translation), Some(expected_manual_state))
    } else {
        (translation, automatic_translation_state)
    };
    let rejected = match (
        rejected_readable_id,
        rejected_origin,
        rejected_source_content_json,
        rejected_source_context_json,
        rejected_candidate_json,
        rejected_violation_json,
        rejected_planning_state,
    ) {
        (None, None, None, None, None, None, None) if rejected_translation_json.is_none() => None,
        (
            Some(readable_id),
            Some(origin),
            Some(source_content_json),
            Some(source_context_json),
            Some(candidate_json),
            Some(violation_json),
            Some(planning_state),
        ) => {
            let origin = TranslationOrigin::from_storage_name(&origin)
                .ok_or(InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedStatePair)?;
            serde_json::from_str::<serde_json::Value>(&candidate_json)
                .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedTranslation)?;
            let source_content = serde_json::from_str::<TextUnitContent>(&source_content_json)
                .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidSourceContent)?;
            let translation = rejected_translation_json
                .map(|json| {
                    serde_json::from_str::<Vec<String>>(&json).map_err(
                        InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedTranslation,
                    )
                })
                .transpose()?;
            let violation = serde_json::from_str::<ProvenInvariantViolation>(&violation_json)
                .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedViolation)?;
            let planning_state =
                Sha256Fingerprint::from_slice(&planning_state).map_err(|error| {
                    InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedStateLength {
                        actual: error.actual(),
                    }
                })?;
            Some(RpgMakerStoredRejectedTranslation::new(
                readable_id,
                origin,
                source_content,
                source_context_json,
                candidate_json,
                translation,
                violation,
                planning_state,
            ))
        }
        _ => return Err(InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedStatePair),
    };
    Ok(DecodedUnit {
        owner,
        kind,
        group_location,
        group_semantic_order_key,
        role,
        semantic_order_key,
        source_content,
        source_context_json,
        recipe_shape,
        translation,
        translation_state,
        manual,
        rejected,
    })
}

#[cfg(test)]
fn decode_unit(
    row: OwnerSqliteRow,
    active_owners: ActiveOwners,
) -> Result<DecodedUnit, InvalidRpgMakerTranslationAssetSnapshot> {
    decode_unit_for_language(row, active_owners, "ja", "zh-Hans")
}

fn validate_persisted_source_content(
    kind: TextGroupKind,
    role: &TextUnitRole,
    source: &TextUnitContent,
) -> Result<(), InvalidRpgMakerTranslationAssetSnapshot> {
    validate_text_unit_content_structure(kind, role, TextUnitContentView::from(source)).map_err(
        |error| match error {
            TextUnitContentStructureError::KindRoleMismatch => {
                InvalidRpgMakerTranslationAssetSnapshot::RoleDoesNotBelongToGroup {
                    role: role.clone(),
                    kind,
                }
            }
            TextUnitContentStructureError::ShapeMismatch => {
                InvalidRpgMakerTranslationAssetSnapshot::SourceContentShapeMismatch {
                    role: role.clone(),
                }
            }
            TextUnitContentStructureError::InvalidText { line_index } => {
                InvalidRpgMakerTranslationAssetSnapshot::InvalidSourceLineText { index: line_index }
            }
        },
    )
}

fn validate_persisted_translation_content(
    kind: TextGroupKind,
    role: &TextUnitRole,
    translation: &TextUnitContent,
) -> Result<(), InvalidRpgMakerTranslationAssetSnapshot> {
    validate_text_unit_content_structure(kind, role, TextUnitContentView::from(translation))
        .map_err(|error| match error {
            TextUnitContentStructureError::KindRoleMismatch => {
                InvalidRpgMakerTranslationAssetSnapshot::RoleDoesNotBelongToGroup {
                    role: role.clone(),
                    kind,
                }
            }
            TextUnitContentStructureError::ShapeMismatch => {
                InvalidRpgMakerTranslationAssetSnapshot::TranslationContentShapeMismatch {
                    role: role.clone(),
                }
            }
            TextUnitContentStructureError::InvalidText { line_index } => {
                InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationLineText {
                    index: line_index,
                }
            }
        })
}

fn validate_persisted_alignment(
    role: &TextUnitRole,
    source: &TextUnitContent,
    translation: Option<&TextUnitContent>,
) -> Result<(), InvalidRpgMakerTranslationAssetSnapshot> {
    let Some(translation) = translation else {
        return Ok(());
    };
    if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
        let source_lines = source.as_lines().expect("严格对齐角色的源内容形状已验证");
        let translation_lines = translation
            .as_lines()
            .expect("严格对齐角色的译文内容形状已验证");
        if source_lines.len() != translation_lines.len() {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::AlignedLineCountMismatch {
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
                    is_structural_blank(source) != is_structural_blank(translation)
                })
        {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::AlignedBlankSlotMismatch { index },
            );
        }
    }
    Ok(())
}

fn map_storage_row_error(
    error: RpgMakerAssetStorageRowError,
) -> InvalidRpgMakerTranslationAssetSnapshot {
    match error {
        RpgMakerAssetStorageRowError::WrongColumnCount { expected, actual } => {
            InvalidRpgMakerTranslationAssetSnapshot::WrongColumnCount { expected, actual }
        }
        RpgMakerAssetStorageRowError::WrongColumnType {
            column,
            expected,
            actual,
        } => InvalidRpgMakerTranslationAssetSnapshot::WrongColumnType {
            column,
            expected,
            actual,
        },
        RpgMakerAssetStorageRowError::InvalidSemanticOrderKey { column, source } => {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidSemanticOrderKey { column, source }
        }
        RpgMakerAssetStorageRowError::UnknownOwner(_) => {
            InvalidRpgMakerTranslationAssetSnapshot::UnknownOwner
        }
        RpgMakerAssetStorageRowError::UnknownGroupKind(_) => {
            InvalidRpgMakerTranslationAssetSnapshot::UnknownGroupKind
        }
        RpgMakerAssetStorageRowError::InvalidLocation(source) => {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidLocation(source)
        }
        RpgMakerAssetStorageRowError::InvalidRole(source) => {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidRole(source)
        }
        RpgMakerAssetStorageRowError::InvalidSourceContent(source) => {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidSourceContent(source)
        }
        RpgMakerAssetStorageRowError::InvalidTranslationContent(source) => {
            InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationContent(source)
        }
    }
}

fn assemble_corpus(
    group_rows: Vec<DecodedGroup>,
    units: Vec<DecodedUnit>,
) -> Result<Vec<RpgMakerTranslationScope>, InvalidRpgMakerTranslationAssetSnapshot> {
    struct GroupBuilder {
        first_owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        semantic_order_key: RpgMakerSemanticOrderKey,
        assets: Vec<RpgMakerTranslationAsset>,
        roles: HashSet<TextUnitRole>,
    }

    let mut owner_group_locations =
        HashSet::<(RpgMakerAssetOwner, RpgMakerLocation)>::with_capacity(group_rows.len());
    let mut group_locations = HashMap::<RpgMakerLocation, usize>::with_capacity(group_rows.len());
    let mut group_order_keys =
        HashMap::<RpgMakerSemanticOrderKey, usize>::with_capacity(group_rows.len());
    let mut groups = Vec::<GroupBuilder>::with_capacity(group_rows.len());
    for group in group_rows {
        if !owner_group_locations.insert((group.owner, group.group_location.clone())) {
            return Err(InvalidRpgMakerTranslationAssetSnapshot::DuplicateGroup {
                owner: group.owner,
                group_location: Box::new(group.group_location),
            });
        }
        if let Some(index) = group_locations.get(&group.group_location).copied() {
            let existing = &groups[index];
            if existing.kind != group.kind
                || existing.semantic_order_key != group.semantic_order_key
            {
                return Err(
                    InvalidRpgMakerTranslationAssetSnapshot::InconsistentGroupDefinition {
                        owner: group.owner,
                        group_location: Box::new(group.group_location),
                    },
                );
            }
            continue;
        }
        if group_order_keys.contains_key(&group.semantic_order_key) {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::DuplicateSemanticOrderKey {
                    level: TranslationSnapshotSemanticOrderLevel::Group,
                },
            );
        }
        let index = groups.len();
        group_locations.insert(group.group_location.clone(), index);
        group_order_keys.insert(group.semantic_order_key.clone(), index);
        groups.push(GroupBuilder {
            first_owner: group.owner,
            kind: group.kind,
            group_location: group.group_location,
            semantic_order_key: group.semantic_order_key,
            assets: Vec::new(),
            roles: HashSet::new(),
        });
    }

    let mut unit_order_keys =
        HashMap::<RpgMakerSemanticOrderKey, (RpgMakerLocation, TextUnitRole)>::new();
    for unit in units {
        if !owner_group_locations.contains(&(unit.owner, unit.group_location.clone())) {
            return Err(InvalidRpgMakerTranslationAssetSnapshot::MissingGroup {
                owner: unit.owner,
                group_location: Box::new(unit.group_location),
            });
        }
        let Some(group_index) = group_locations.get(&unit.group_location).copied() else {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                },
            );
        };
        let group = &mut groups[group_index];
        if group.kind != unit.kind
            || group.group_location != unit.group_location
            || group.semantic_order_key != unit.group_semantic_order_key
        {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                },
            );
        }
        if !group.roles.insert(unit.role.clone()) {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::DuplicateLogicalUnit {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                    role: unit.role,
                },
            );
        }
        if unit_order_keys
            .insert(
                unit.semantic_order_key.clone(),
                (unit.group_location.clone(), unit.role.clone()),
            )
            .is_some()
        {
            return Err(
                InvalidRpgMakerTranslationAssetSnapshot::DuplicateSemanticOrderKey {
                    level: TranslationSnapshotSemanticOrderLevel::Unit,
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
        let asset = if unit.manual {
            RpgMakerTranslationAsset::with_manual_semantic_order_key(
                identity,
                unit.semantic_order_key,
                unit.recipe_shape,
                unit.translation.expect("当前人工译文必须包含正文"),
                unit.translation_state
                    .expect("当前人工译文必须包含结构指纹"),
            )
        } else {
            RpgMakerTranslationAsset::with_rejected_semantic_order_key(
                identity,
                unit.semantic_order_key,
                unit.recipe_shape,
                unit.translation,
                unit.translation_state,
                unit.rejected,
            )
        };
        group.assets.push(asset);
    }

    if let Some(group) = groups.iter().find(|group| group.assets.is_empty()) {
        return Err(InvalidRpgMakerTranslationAssetSnapshot::EmptyGroup {
            owner: group.first_owner,
            group_location: Box::new(group.group_location.clone()),
        });
    }
    groups.sort_by(|left, right| left.semantic_order_key.cmp(&right.semantic_order_key));
    let mut scope_indexes = HashMap::<RpgMakerSemanticScopeKey, usize>::new();
    let mut scopes = Vec::<(RpgMakerSemanticScopeKey, Vec<RpgMakerTranslationGroup>)>::new();
    for mut group in groups {
        group
            .assets
            .sort_by(|left, right| left.semantic_order_key().cmp(right.semantic_order_key()));
        let scope = RpgMakerSemanticScopeKey::from_group_location(&group.group_location)
            .map_err(InvalidRpgMakerTranslationAssetSnapshot::InvalidSemanticScope)?;
        let group = RpgMakerTranslationGroup::with_semantic_order_key(
            group.kind,
            group.group_location,
            group.semantic_order_key,
            group.assets,
        );
        if let Some(index) = scope_indexes.get(&scope).copied() {
            scopes[index].1.push(group);
        } else {
            let index = scopes.len();
            scope_indexes.insert(scope.clone(), index);
            scopes.push((scope, vec![group]));
        }
    }
    Ok(scopes
        .into_iter()
        .map(|(key, groups)| RpgMakerTranslationScope::new(key, groups))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::project_name::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource};
    use rusqlite::params_from_iter;

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
        results: Arc<Mutex<Option<Vec<Vec<SqliteRow>>>>>,
    }

    impl SqliteQueryExecutor for FakeQuery {
        type Error = FakeError;

        async fn query_existing_database(
            &self,
            _path: PathBuf,
            _query: SqliteQuery,
        ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
            panic!("翻译资产必须通过同一快照中的窄查询读取")
        }

        async fn query_existing_database_snapshot(
            &self,
            path: PathBuf,
            queries: Vec<SqliteQuery>,
        ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
            for query in queries {
                self.calls
                    .lock()
                    .expect("查询锁")
                    .push((path.clone(), query));
            }
            Ok(self
                .results
                .lock()
                .expect("响应锁")
                .take()
                .expect("单次响应"))
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

    type ProductionAssetReadingError =
        RpgMakerTranslationAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable>;

    #[test]
    fn asset_reading_diagnostic_preserves_database_path_and_fingerprint_facts() {
        let error: ProductionAssetReadingError =
            RpgMakerTranslationAssetReadingError::ProjectSnapshotChanged {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                expected: SourceSnapshotFingerprint::from_bytes([0x11; 32]),
                actual: SourceSnapshotFingerprint::from_bytes([0x22; 32]),
            };

        assert_eq!(
            serde_json::to_value(error.diagnostic_report()).expect("报告可序列化"),
            serde_json::json!({
                "effect": "unchanged",
                "primary": {
                    "code": "rpg_maker.translate.asset_snapshot.project_changed",
                    "stage": "translate",
                    "issue": {
                        "family": "rpg_maker",
                        "details": {
                            "stage": "translate_planning",
                            "problem": {
                                "kind": "translation_asset",
                                "database_path": "C:/projects/demo/project.db",
                                "problem": {
                                    "kind": "project_snapshot_changed",
                                    "expected": "1111111111111111111111111111111111111111111111111111111111111111",
                                    "actual": "2222222222222222222222222222222222222222222222222222222222222222"
                                }
                            }
                        }
                    },
                    "resolution": "check_project_state"
                },
                "related": []
            })
        );
    }

    #[test]
    fn asset_reading_diagnostic_distinguishes_cancelled_cpu_wait_and_hides_raw_rows() {
        let cancelled: ProductionAssetReadingError =
            RpgMakerTranslationAssetReadingError::ScheduleDecode {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                source: CpuTaskExecutionError::Cancelled,
            };
        assert_eq!(
            serde_json::to_value(cancelled.diagnostic_report()).expect("报告可序列化"),
            serde_json::json!({
                "effect": "unchanged",
                "primary": {
                    "code": "rpg_maker.translate.asset_snapshot.decode_units.cancelled",
                    "stage": "translate",
                    "issue": {
                        "family": "rpg_maker",
                        "details": {
                            "stage": "translate_planning",
                            "problem": {
                                "kind": "translation_asset",
                                "database_path": "C:/projects/demo/project.db",
                                "problem": {
                                    "kind": "compute",
                                    "operation": "decode_units",
                                    "failure": "cancelled"
                                }
                            }
                        }
                    },
                    "resolution": "retry"
                },
                "related": []
            })
        );

        let sentinel = "RAW_DATABASE_ROW_SENTINEL";
        let invalid: ProductionAssetReadingError =
            RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                source: map_storage_row_error(RpgMakerAssetStorageRowError::UnknownOwner(
                    sentinel.to_owned(),
                )),
            };
        let wire = serde_json::to_string(&invalid.diagnostic_report()).expect("报告可序列化");
        assert!(wire.contains("rpg_maker.translate.asset_snapshot.unknown_owner"));
        assert!(!wire.contains(sentinel));
    }

    #[test]
    fn asset_snapshot_diagnostic_keeps_codec_structure_and_group_location() {
        let database_path = PathBuf::from("C:/projects/demo/project.db");
        let projection: ProductionAssetReadingError =
            RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                database_path: database_path.clone(),
                source: InvalidRpgMakerTranslationAssetSnapshot::InvalidRole(
                    RpgMakerProjectionCodecError::Projection(
                        crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                            expected: 2,
                            actual: 4,
                        },
                    ),
                ),
            };
        assert_eq!(
            serde_json::to_value(projection.diagnostic_report()).expect("报告可序列化")["primary"]
                ["issue"]["details"]["problem"]["problem"]["violation"],
            serde_json::json!({
                "kind": "invalid_role",
                "failure": {
                    "kind": "projection",
                    "violation": {
                        "kind": "non_contiguous_dialogue_body_lines",
                        "expected": 2,
                        "actual": 4
                    }
                }
            })
        );

        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(3),
            vec![RpgMakerLocationStep::key("events")],
        );
        let group: ProductionAssetReadingError =
            RpgMakerTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source: InvalidRpgMakerTranslationAssetSnapshot::DuplicateLogicalUnit {
                    owner: RpgMakerAssetOwner::Builtin,
                    group_location: Box::new(group_location),
                    role: TextUnitRole::DialogueBody,
                },
            };
        assert_eq!(
            serde_json::to_value(group.diagnostic_report()).expect("报告可序列化")["primary"]["issue"]
                ["details"]["problem"]["problem"]["violation"],
            serde_json::json!({
                "kind": "duplicate_logical_unit",
                "owner": "builtin",
                "group_location": {
                    "source": { "kind": "map", "map_id": 3 },
                    "steps": [
                        { "kind": "object_key", "key": "events" }
                    ]
                },
                "role": { "kind": "dialogue_body" }
            })
        );
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
        let service = RpgMakerTranslationAssetReadingService::new(
            FakeQuery {
                calls: Arc::clone(&calls),
                results: Arc::new(Mutex::new(Some(rows))),
            },
            InlineCpu,
        );

        let corpus = service.read(&project()).await.expect("统一表应可读取");

        assert_eq!(corpus.scopes().len(), 1);
        let assets = corpus.scopes()[0].groups()[0].assets();
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
        assert_eq!(calls.len(), TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT);
        assert!(
            calls
                .iter()
                .all(|(_, query)| !query.statement().contains("UNION ALL"))
        );
        assert!(
            calls
                .iter()
                .any(|(_, query)| query.statement().contains("rpg_maker_text_unit"))
        );
    }

    #[test]
    fn corpus_merges_same_location_groups_across_owners() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let owners = [RpgMakerAssetOwner::Builtin, RpgMakerAssetOwner::Rules];
        let group_rows = owners
            .into_iter()
            .map(|owner| DecodedGroup {
                owner,
                kind: TextGroupKind::DatabaseEntry,
                group_location: group_location.clone(),
                semantic_order_key: RpgMakerSemanticOrderKey::new(vec![1], 0),
            })
            .collect::<Vec<_>>();
        let units = owners
            .into_iter()
            .map(|owner| {
                let role = TextUnitRole::Scalar(
                    ScalarFieldKey::new(format!("{}_name", owner.storage_name()))
                        .expect("测试角色应合法"),
                );
                DecodedUnit {
                    owner,
                    kind: TextGroupKind::DatabaseEntry,
                    group_location: group_location.clone(),
                    group_semantic_order_key: RpgMakerSemanticOrderKey::new(vec![1], 0),
                    role,
                    semantic_order_key: RpgMakerSemanticOrderKey::new(
                        vec![1],
                        match owner {
                            RpgMakerAssetOwner::Builtin => 1,
                            RpgMakerAssetOwner::Rules => 2,
                        },
                    ),
                    source_content: TextUnitContent::Value(owner.storage_name().to_owned()),
                    source_context_json: "{}".to_owned(),
                    recipe_shape: "[]".to_owned(),
                    translation: None,
                    translation_state: None,
                    manual: false,
                    rejected: None,
                }
            })
            .collect::<Vec<_>>();

        let scopes = assemble_corpus(group_rows, units).expect("两个 owner 的语料应能组装");

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].groups().len(), 1, "同一逻辑位置必须跨 owner 合并");
        assert_eq!(
            scopes[0].groups()[0]
                .assets()
                .iter()
                .map(|asset| asset.identity().owner())
                .collect::<Vec<_>>(),
            owners,
            "Unit 必须按统一 semantic_order_key 排序"
        );
    }

    #[test]
    fn corpus_preserves_scope_first_appearance_in_physical_semantic_order() {
        let common_event = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::CommonEvents),
            vec![RpgMakerLocationStep::index(1)],
        );
        let map = RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new());
        let common_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let map_order = RpgMakerSemanticOrderKey::new(vec![2], 0);
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键必须有效"));

        let scopes = assemble_corpus(
            vec![
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &map,
                    map_order.clone(),
                ),
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &common_event,
                    common_order.clone(),
                ),
            ],
            vec![
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &map,
                    map_order,
                    role.clone(),
                    RpgMakerSemanticOrderKey::new(vec![2], 1),
                ),
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &common_event,
                    common_order,
                    role,
                    RpgMakerSemanticOrderKey::new(vec![1], 1),
                ),
            ],
        )
        .expect("跨 Scope 语料应可组装");

        assert_eq!(scopes.len(), 2);
        assert!(matches!(
            scopes[0].key(),
            RpgMakerSemanticScopeKey::CommonEvent(1)
        ));
        assert!(matches!(
            scopes[1].key(),
            RpgMakerSemanticScopeKey::Map(map_id)
                if *map_id == crate::rpg_maker::text::MapId::new(1).expect("测试 Map ID 应有效")
        ));
    }

    fn decoded_group(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        group_location: &RpgMakerLocation,
        semantic_order_key: RpgMakerSemanticOrderKey,
    ) -> DecodedGroup {
        DecodedGroup {
            owner,
            kind,
            group_location: group_location.clone(),
            semantic_order_key,
        }
    }

    fn decoded_unit(
        owner: RpgMakerAssetOwner,
        group_location: &RpgMakerLocation,
        group_semantic_order_key: RpgMakerSemanticOrderKey,
        role: TextUnitRole,
        semantic_order_key: RpgMakerSemanticOrderKey,
    ) -> DecodedUnit {
        DecodedUnit {
            owner,
            kind: TextGroupKind::DatabaseEntry,
            group_location: group_location.clone(),
            group_semantic_order_key,
            role,
            semantic_order_key,
            source_content: TextUnitContent::Value(owner.storage_name().to_owned()),
            source_context_json: "{}".to_owned(),
            recipe_shape: "[]".to_owned(),
            translation: None,
            translation_state: None,
            manual: false,
            rejected: None,
        }
    }

    #[test]
    fn corpus_rejects_inconsistent_cross_owner_group_definitions() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let canonical_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let conflicts = [
            (
                TextGroupKind::Map,
                canonical_order.clone(),
                "跨 owner 的 kind 冲突必须失败",
            ),
            (
                TextGroupKind::DatabaseEntry,
                RpgMakerSemanticOrderKey::new(vec![2], 0),
                "跨 owner 的 Group 顺序键冲突必须失败",
            ),
        ];

        for (rules_kind, rules_order, message) in conflicts {
            let error = assemble_corpus(
                vec![
                    decoded_group(
                        RpgMakerAssetOwner::Builtin,
                        TextGroupKind::DatabaseEntry,
                        &group_location,
                        canonical_order.clone(),
                    ),
                    decoded_group(
                        RpgMakerAssetOwner::Rules,
                        rules_kind,
                        &group_location,
                        rules_order,
                    ),
                ],
                Vec::new(),
            )
            .expect_err(message);

            assert!(matches!(
                error,
                InvalidRpgMakerTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: RpgMakerAssetOwner::Rules,
                    group_location: ref actual,
                } if actual.as_ref() == &group_location
            ));
        }
    }

    #[test]
    fn corpus_rejects_distinct_groups_with_the_same_semantic_order_key() {
        let first_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let second_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(2)],
        );
        let duplicate_order = RpgMakerSemanticOrderKey::new(vec![1], 0);

        let error = assemble_corpus(
            vec![
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &first_location,
                    duplicate_order.clone(),
                ),
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &second_location,
                    duplicate_order,
                ),
            ],
            Vec::new(),
        )
        .expect_err("不同 Group 共用 semantic_order_key 必须失败");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::DuplicateSemanticOrderKey {
                level: TranslationSnapshotSemanticOrderLevel::Group,
            }
        ));
    }

    #[test]
    fn corpus_rejects_a_duplicate_logical_role_across_owners() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键必须有效"));

        let error = assemble_corpus(
            vec![
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &group_location,
                    group_order.clone(),
                ),
                decoded_group(
                    RpgMakerAssetOwner::Rules,
                    TextGroupKind::DatabaseEntry,
                    &group_location,
                    group_order.clone(),
                ),
            ],
            vec![
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &group_location,
                    group_order.clone(),
                    role.clone(),
                    RpgMakerSemanticOrderKey::new(vec![1], 1),
                ),
                decoded_unit(
                    RpgMakerAssetOwner::Rules,
                    &group_location,
                    group_order,
                    role.clone(),
                    RpgMakerSemanticOrderKey::new(vec![1], 2),
                ),
            ],
        )
        .expect_err("跨 owner 的同一逻辑角色必须失败");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::DuplicateLogicalUnit {
                owner: RpgMakerAssetOwner::Rules,
                group_location: ref actual_location,
                role: ref actual_role,
            } if actual_location.as_ref() == &group_location && actual_role == &role
        ));
    }

    #[test]
    fn corpus_rejects_distinct_units_with_the_same_semantic_order_key() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let first_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键必须有效"));
        let second_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键必须有效"));
        let duplicate_order = RpgMakerSemanticOrderKey::new(vec![1], 1);

        let error = assemble_corpus(
            vec![decoded_group(
                RpgMakerAssetOwner::Builtin,
                TextGroupKind::DatabaseEntry,
                &group_location,
                group_order.clone(),
            )],
            vec![
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &group_location,
                    group_order.clone(),
                    first_role.clone(),
                    duplicate_order.clone(),
                ),
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &group_location,
                    group_order,
                    second_role.clone(),
                    duplicate_order,
                ),
            ],
        )
        .expect_err("同一 Group 的不同 Unit 共用 semantic_order_key 必须失败");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::DuplicateSemanticOrderKey {
                level: TranslationSnapshotSemanticOrderLevel::Unit,
            }
        ));
    }

    #[test]
    fn corpus_rejects_a_unit_order_key_reused_by_another_owner_and_group() {
        let first_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let second_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(2)],
        );
        let first_group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let second_group_order = RpgMakerSemanticOrderKey::new(vec![2], 0);
        let duplicate_unit_order = RpgMakerSemanticOrderKey::new(vec![9], 1);
        let first_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("测试字段键必须有效"));
        let second_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键必须有效"));

        let error = assemble_corpus(
            vec![
                decoded_group(
                    RpgMakerAssetOwner::Builtin,
                    TextGroupKind::DatabaseEntry,
                    &first_location,
                    first_group_order.clone(),
                ),
                decoded_group(
                    RpgMakerAssetOwner::Rules,
                    TextGroupKind::DatabaseEntry,
                    &second_location,
                    second_group_order.clone(),
                ),
            ],
            vec![
                decoded_unit(
                    RpgMakerAssetOwner::Builtin,
                    &first_location,
                    first_group_order,
                    first_role.clone(),
                    duplicate_unit_order.clone(),
                ),
                decoded_unit(
                    RpgMakerAssetOwner::Rules,
                    &second_location,
                    second_group_order,
                    second_role.clone(),
                    duplicate_unit_order,
                ),
            ],
        )
        .expect_err("不同 owner 和 Group 的 Unit 共用 semantic_order_key 必须失败");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::DuplicateSemanticOrderKey {
                level: TranslationSnapshotSemanticOrderLevel::Unit,
            }
        ));
    }

    #[test]
    fn snapshot_queries_partition_large_tables_without_temporary_sorting() {
        let connection = rusqlite::Connection::open_in_memory().expect("应可建立内存数据库");
        connection
            .execute_batch(
                r#"
                CREATE TABLE metadata (
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE rpg_maker_asset_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE rpg_maker_translation_resource (
                    resource_kind TEXT NOT NULL PRIMARY KEY,
                    canonical_json TEXT NOT NULL
                );
                CREATE TABLE rpg_maker_text_group (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL CHECK (group_id > 0),
                    group_location TEXT NOT NULL,
                    semantic_order_key BLOB NOT NULL,
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL DEFAULT '[]',
                    PRIMARY KEY (owner, group_id),
                    UNIQUE (owner, group_location),
                    UNIQUE (owner, semantic_order_key)
                );
                CREATE TABLE rpg_maker_text_unit (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL CHECK (group_id > 0),
                    unit_role TEXT NOT NULL,
                    rule_number INTEGER,
                    semantic_order_key BLOB NOT NULL,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state BLOB,
                    PRIMARY KEY (owner, group_id, unit_role),
                    UNIQUE (owner, semantic_order_key)
                );
                CREATE INDEX rpg_maker_text_unit_owner_group_order_idx
                    ON rpg_maker_text_unit(owner, group_id, semantic_order_key);
                 CREATE TABLE rpg_maker_manual_translation (
                     owner TEXT NOT NULL,
                     group_location TEXT NOT NULL,
                     unit_role TEXT NOT NULL,
                     translation_json TEXT,
                     applicability_fingerprint BLOB,
                     PRIMARY KEY (owner, group_location, unit_role)
                 );
                 CREATE TABLE rpg_maker_rejected_translation (
                     owner TEXT NOT NULL,
                     group_id INTEGER NOT NULL,
                     unit_role TEXT NOT NULL,
                     readable_id TEXT NOT NULL,
                     origin TEXT NOT NULL,
                     source_content_json TEXT NOT NULL,
                     source_context_json TEXT NOT NULL,
                     candidate_json TEXT NOT NULL,
                     translation_json TEXT,
                     violation_json TEXT NOT NULL,
                     planning_state BLOB NOT NULL,
                     PRIMARY KEY (owner, group_id, unit_role)
                 );

                INSERT INTO metadata VALUES (zeroblob(32));
                INSERT INTO rpg_maker_asset_owner_state VALUES
                    ('rules', zeroblob(32), zeroblob(32)),
                    ('builtin', zeroblob(32), zeroblob(32));
                INSERT INTO rpg_maker_translation_resource VALUES
                    ('terminology', '[]'),
                    ('placeholder_rules', '[]');
                INSERT INTO rpg_maker_text_group VALUES
                    ('builtin', 2, 'group-b', X'010000000000000001000000000000000000', 'map', '[]'),
                    ('builtin', 1, 'group-a', X'010000000000000000000000000000000000', 'map', '[]'),
                    ('rules', 1, 'group-r', X'010000000000000000000000000000000000', 'map', '[]');
                INSERT INTO rpg_maker_text_unit (
                    owner, group_id, unit_role, semantic_order_key,
                    source_content_json, source_context_json,
                    translation_content_json, translation_state
                ) VALUES
                    ('builtin', 2, 'role-z', X'010000000000000001000000000000000000', '"z"', '{}', NULL, NULL),
                    ('builtin', 1, 'role-y', X'010000000000000000000000000000000000', '"y"', '{}', NULL, NULL);
                "#,
            )
            .expect("测试快照表与行应可建立");

        let groups = connection
            .prepare(&read_translation_owner_groups())
            .expect("owner group 查询应可建立")
            .query_map(["builtin"], |row| row.get::<_, String>(0))
            .expect("owner group 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner group 行应可读取");
        let units = connection
            .prepare(&read_translation_owner_units())
            .expect("owner unit 查询应可建立")
            .query_map(["builtin"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(3)?))
            })
            .expect("owner unit 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner unit 行应可读取");
        assert_eq!(groups, ["group-a", "group-b"]);
        assert_eq!(
            units,
            [
                ("group-a".to_owned(), "role-y".to_owned()),
                ("group-b".to_owned(), "role-z".to_owned()),
            ]
        );

        let queries = translation_snapshot_queries();
        assert_eq!(queries.len(), TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT);
        assert!(queries.iter().all(|query| {
            !query.statement().contains("CASE") && !query.statement().contains("UNION ALL")
        }));
        assert_eq!(
            queries
                .iter()
                .skip(3)
                .map(|query| match query.parameters() {
                    [SqliteValue::Text(owner)] => owner.as_str(),
                    parameters => panic!("owner 分区查询参数无效：{parameters:?}"),
                })
                .collect::<Vec<_>>(),
            ["builtin", "rules", "builtin", "rules"]
        );

        for query in queries {
            let explain = format!("EXPLAIN QUERY PLAN {}", query.statement());
            let mut statement = connection.prepare(&explain).expect("查询计划应可建立");
            let details = statement
                .query_map(
                    params_from_iter(query.parameters().iter().map(|parameter| match parameter {
                        SqliteValue::Text(value) => value.as_str(),
                        _ => unreachable!("翻译快照窄查询只携带 owner 文本参数"),
                    })),
                    |row| row.get::<_, String>(3),
                )
                .expect("查询计划应可执行")
                .collect::<Result<Vec<_>, _>>()
                .expect("查询计划应可读取");
            assert!(
                details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
                "翻译快照查询不得建立全局临时排序：{details:?}"
            );
            if query.statement().contains("rpg_maker_text_unit AS unit") {
                assert!(
                    details.iter().any(|detail| {
                        detail.contains("rpg_maker_text_unit_owner_group_order_idx")
                            && detail.contains("group_id=?")
                    }),
                    "Unit 必须按 owner 与内部 group_id 定位，不能为每个 Group 扫描整个 owner：{details:?}"
                );
            }
        }
    }

    #[test]
    fn owner_state_decode_restores_builtin_rules_natural_order() {
        let current = SourceSnapshotFingerprint::from_bytes([0x31; 32]);
        let rows = [RpgMakerAssetOwner::Builtin, RpgMakerAssetOwner::Rules]
            .into_iter()
            .map(|owner| {
                SqliteRow::new(vec![
                    text(owner.storage_name()),
                    SqliteValue::Blob(current.as_bytes().to_vec()),
                    SqliteValue::Blob(vec![owner_order(owner) as u8; 32]),
                ])
            })
            .collect();

        let decoded = decode_owner_states(rows, current).expect("owner 状态应可解码");

        assert_eq!(
            decoded
                .snapshots
                .iter()
                .map(|snapshot| snapshot.owner())
                .collect::<Vec<_>>(),
            TRANSLATION_OWNER_ORDER
        );
    }

    #[test]
    fn body_context_must_be_a_json_object() {
        let role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let error = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: unit_payload_row(
                    &dialogue_group(),
                    "event_dialogue",
                    &role,
                    r#"["正文"]"#,
                    "[]",
                ),
            },
            active_builtin(),
        )
        .expect_err("数组不能充当源上下文");
        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::SourceContextMustBeObject
        ));
    }

    #[test]
    fn group_columns_keep_first_error_before_corrupt_role_and_source_json() {
        let role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let corrupt_tail = |kind: &str, group_order_key: SqliteValue| {
            let mut values =
                unit_payload_row(&dialogue_group(), kind, &role, r#"["正文"]"#, "{}").into_values();
            values[2] = group_order_key;
            values[3] = text("{");
            values[5] = text("{");
            SqliteRow::new(values)
        };

        let unknown_kind = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: corrupt_tail(
                    "unknown_kind",
                    SqliteValue::Blob(
                        RpgMakerSemanticOrderKey::new(vec![0], 0)
                            .encode()
                            .expect("应编码顺序键"),
                    ),
                ),
            },
            active_builtin(),
        )
        .expect_err("未知 group_kind 必须先于损坏的 role 和 source JSON 报告");
        assert!(matches!(
            unknown_kind,
            InvalidRpgMakerTranslationAssetSnapshot::UnknownGroupKind
        ));

        let invalid_group_order = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: corrupt_tail("event_dialogue", SqliteValue::Blob(vec![0])),
            },
            active_builtin(),
        )
        .expect_err("非法 semantic_order_key 必须先于损坏的 role 和 source JSON 报告");
        assert!(matches!(
            invalid_group_order,
            InvalidRpgMakerTranslationAssetSnapshot::InvalidSemanticOrderKey {
                column: "semantic_order_key",
                ..
            }
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
        values[7] = text(r#""错误形状""#);
        values[8] = SqliteValue::Blob(vec![0x44; 32]);

        let error = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: SqliteRow::new(values),
            },
            active_builtin(),
        )
        .expect_err("正文译文必须保持 Lines 形状");
        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::TranslationContentShapeMismatch {
                role: TextUnitRole::DialogueBody
            }
        ));
    }

    #[test]
    fn persisted_unit_rejects_kind_role_mismatch_before_content_rules() {
        let body_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        let error = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: unit_payload_row(
                    &dialogue_group(),
                    "event_choices",
                    &body_role,
                    r#"["正文"]"#,
                    "{}",
                ),
            },
            active_builtin(),
        )
        .expect_err("DialogueBody 不得进入 Choices 组");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::RoleDoesNotBelongToGroup {
                role: TextUnitRole::DialogueBody,
                kind: TextGroupKind::EventChoices,
            }
        ));
    }

    #[test]
    fn persisted_source_and_translation_lines_reject_cr_lf_and_nul() {
        let body_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::DialogueBody)
            .expect("角色应可编码");
        for escaped in [
            r#"["正文\r续行"]"#,
            r#"["正文\n续行"]"#,
            r#"["正文\u0000续行"]"#,
        ] {
            let source_error = decode_unit(
                OwnerSqliteRow {
                    owner: RpgMakerAssetOwner::Builtin,
                    row: unit_payload_row(
                        &dialogue_group(),
                        "event_dialogue",
                        &body_role,
                        escaped,
                        "{}",
                    ),
                },
                active_builtin(),
            )
            .expect_err("持久化源 Lines 的元素不得包含 CR、LF 或 NUL");
            assert!(matches!(
                source_error,
                InvalidRpgMakerTranslationAssetSnapshot::InvalidSourceLineText { index: 0 }
            ));

            let mut values = unit_payload_row(
                &dialogue_group(),
                "event_dialogue",
                &body_role,
                r#"["正文"]"#,
                "{}",
            )
            .into_values();
            values[7] = text(escaped);
            values[8] = SqliteValue::Blob(vec![0x44; 32]);
            let translation_error = decode_unit(
                OwnerSqliteRow {
                    owner: RpgMakerAssetOwner::Builtin,
                    row: SqliteRow::new(values),
                },
                active_builtin(),
            )
            .expect_err("持久化译文 Lines 的元素不得包含 CR、LF 或 NUL");
            assert!(matches!(
                translation_error,
                InvalidRpgMakerTranslationAssetSnapshot::InvalidTranslationLineText { index: 0 }
            ));
        }
    }

    #[test]
    fn persisted_rejected_violation_must_be_a_structured_closed_value() {
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
        values[12] = text("Builtin/Map001/1");
        values[13] = text("automatic");
        values[14] = text(r#"["正文"]"#);
        values[15] = text("{}");
        values[16] = text(r#"["候选"]"#);
        values[18] = text("[]");
        values[19] = SqliteValue::Blob(vec![0x55; 32]);

        let error = decode_unit(
            OwnerSqliteRow {
                owner: RpgMakerAssetOwner::Builtin,
                row: SqliteRow::new(values),
            },
            active_builtin(),
        )
        .expect_err("数据库中的 Rejected 违反项必须是闭集结构对象");

        assert!(matches!(
            error,
            InvalidRpgMakerTranslationAssetSnapshot::InvalidRejectedViolation(_)
        ));
    }

    fn snapshot_rows(group: &RpgMakerLocation, units: Vec<SqliteRow>) -> Vec<Vec<SqliteRow>> {
        vec![
            vec![SqliteRow::new(vec![SqliteValue::Blob(vec![0xa5; 32])])],
            vec![SqliteRow::new(vec![
                text("builtin"),
                SqliteValue::Blob(vec![0xa5; 32]),
                SqliteValue::Blob(vec![0xb4; 32]),
            ])],
            vec![
                SqliteRow::new(vec![text(TERMINOLOGY_RESOURCE_KIND), text("[]")]),
                SqliteRow::new(vec![text(PLACEHOLDER_RULES_RESOURCE_KIND), text("[]")]),
            ],
            vec![SqliteRow::new(vec![
                text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
                text("event_dialogue"),
                SqliteValue::Blob(
                    RpgMakerSemanticOrderKey::from_group_location(group)
                        .encode()
                        .expect("应编码 Group 顺序键"),
                ),
            ])],
            Vec::new(),
            units,
            Vec::new(),
        ]
    }

    fn unit_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        unit_order: i64,
        source_content_json: &str,
        context: &str,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
            text(kind),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::from_group_location(group)
                    .encode()
                    .expect("应编码 Group 顺序键"),
            ),
            text(role),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::new(
                    vec![u64::try_from(unit_order).expect("测试顺序必须非负")],
                    0,
                )
                .encode()
                .expect("应编码 Unit 顺序键"),
            ),
            text(source_content_json),
            text(context),
            SqliteValue::Null,
            SqliteValue::Null,
            text("[]"),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
        ])
    }

    fn unit_payload_row(
        group: &RpgMakerLocation,
        kind: &str,
        role: &str,
        source_content_json: &str,
        context: &str,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            text(RpgMakerLocationCodec::encode(group).expect("位置应可编码")),
            text(kind),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::from_group_location(group)
                    .encode()
                    .expect("应编码 Group 顺序键"),
            ),
            text(role),
            SqliteValue::Blob(
                RpgMakerSemanticOrderKey::new(vec![0], 0)
                    .encode()
                    .expect("应编码 Unit 顺序键"),
            ),
            text(source_content_json),
            text(context),
            SqliteValue::Null,
            SqliteValue::Null,
            text("[]"),
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
        ])
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
        )
    }

    fn active_builtin() -> ActiveOwners {
        let mut owners = ActiveOwners::default();
        assert!(owners.insert(RpgMakerAssetOwner::Builtin));
        owners
    }

    fn text(value: impl Into<String>) -> SqliteValue {
        SqliteValue::Text(value.into())
    }
}
