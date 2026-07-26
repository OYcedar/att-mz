//! 从 RPG Maker 标准文本资产表建立写回快照。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::dialogue::MvDialogueDefinitionError;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
#[cfg(test)]
use crate::rpg_maker::model::mutation_claims_for_group;
use crate::rpg_maker::model::{
    MutationClaim, MutationResource, MutationResourceAccess, TextProjectionRecipe, TextUnitContent,
    TextUnitRole,
};
use crate::rpg_maker::mutation_claim_summary::{
    EncodedMutationClaim, MutationClaimSummaryError, collision_summary, sort_logical_claims,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
use crate::rpg_maker::standard_asset::{
    RpgMakerStandardAssetOwner, StandardTextSnapshotFingerprintBuilder,
};
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardWriteBackAssetReader, StandardWriteBackGroup, StandardWriteBackSnapshot,
    StandardWriteBackSnapshotError, StandardWriteBackUnit,
};

const READ_STANDARD_WRITE_BACK_OWNER_STATES: &str = r#"SELECT
    owner,
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint
FROM standard_asset_owner_state
ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 WHEN 'lua' THEN 2 END"#;

const READ_STANDARD_WRITE_BACK_OWNER_GROUPS: &str = r#"SELECT
    owner,
    group_location,
    group_order,
    group_kind,
    projection_recipe_json
FROM standard_text_group
WHERE owner = ?
ORDER BY group_order"#;

const READ_STANDARD_WRITE_BACK_OWNER_UNITS: &str = r#"SELECT
    unit.owner,
    unit.group_location,
    unit.unit_role,
    unit.unit_order,
    unit.source_content_json,
    unit.source_context_json,
    unit.translation_content_json
FROM standard_text_group AS text_group
CROSS JOIN standard_text_unit AS unit
  ON unit.owner = text_group.owner
 AND text_group.group_location = unit.group_location
WHERE text_group.owner = ?
ORDER BY text_group.group_order,
         unit.unit_order"#;

const READ_STANDARD_WRITE_BACK_OWNER_CLAIMS: &str = r#"SELECT
    owner,
    group_location,
    resource_key,
    access
FROM standard_mutation_claim INDEXED BY standard_mutation_claim_owner_resource_idx
WHERE owner = ?
ORDER BY resource_key COLLATE BINARY,
         access COLLATE BINARY,
         group_location COLLATE BINARY"#;

const STANDARD_WRITE_BACK_OWNER_ORDER: [RpgMakerStandardAssetOwner; 3] = [
    RpgMakerStandardAssetOwner::Builtin,
    RpgMakerStandardAssetOwner::Rules,
    RpgMakerStandardAssetOwner::Lua,
];
const STANDARD_WRITE_BACK_QUERY_RESULT_COUNT: usize = 1 + STANDARD_WRITE_BACK_OWNER_ORDER.len() * 3;

fn standard_write_back_snapshot_queries() -> Vec<SqliteQuery> {
    let mut queries = Vec::with_capacity(STANDARD_WRITE_BACK_QUERY_RESULT_COUNT);
    queries.push(
        SqliteQuery::new(READ_STANDARD_WRITE_BACK_OWNER_STATES, Vec::new())
            .with_id("write_back.owner_states"),
    );
    for (kind, statement) in [
        ("groups", READ_STANDARD_WRITE_BACK_OWNER_GROUPS),
        ("units", READ_STANDARD_WRITE_BACK_OWNER_UNITS),
        ("claims", READ_STANDARD_WRITE_BACK_OWNER_CLAIMS),
    ] {
        queries.extend(STANDARD_WRITE_BACK_OWNER_ORDER.map(|owner| {
            SqliteQuery::new(
                statement,
                vec![SqliteValue::Text(owner.storage_name().to_owned())],
            )
            .with_id(format!("write_back.{}.{kind}", owner.storage_name()))
        }));
    }
    queries
}

fn merge_owner_partitions(partitions: [Vec<SqliteRow>; 3]) -> Vec<SqliteRow> {
    let capacity = partitions.iter().map(Vec::len).sum();
    let mut merged = Vec::with_capacity(capacity);
    for mut partition in partitions {
        merged.append(&mut partition);
    }
    merged
}

fn unpack_snapshot_query_results(
    query_results: Vec<Vec<SqliteRow>>,
) -> Result<SnapshotRows, InvalidStandardWriteBackAssetSnapshot> {
    let actual = query_results.len();
    if actual != STANDARD_WRITE_BACK_QUERY_RESULT_COUNT {
        return Err(
            InvalidStandardWriteBackAssetSnapshot::WrongQueryResultCount {
                expected: STANDARD_WRITE_BACK_QUERY_RESULT_COUNT,
                actual,
            },
        );
    }
    let mut query_results = query_results.into_iter();
    let owners = query_results.next().expect("已验证快照查询结果数量");
    let mut next_owner_partitions = || {
        merge_owner_partitions([
            query_results.next().expect("已验证 Builtin 查询结果存在"),
            query_results.next().expect("已验证 Rules 查询结果存在"),
            query_results.next().expect("已验证 Lua 查询结果存在"),
        ])
    };
    let groups = next_owner_partitions();
    let units = next_owner_partitions();
    let claims = next_owner_partitions();
    debug_assert!(query_results.next().is_none());
    Ok(SnapshotRows {
        owners,
        groups,
        units,
        claims,
    })
}

/// 先验证 active owner 与资产指纹，再用受控 CPU 解码建立写回快照。
pub(crate) struct RpgMakerStandardWriteBackAssetReadingService<Q, C> {
    sqlite: Arc<Q>,
    cpu: Arc<C>,
}

impl<Q, C> RpgMakerStandardWriteBackAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C) -> Self {
        Self {
            sqlite: Arc::new(sqlite),
            cpu: Arc::new(cpu),
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

        async move {
            let dialogue_definition_json =
                dialogue_definition.to_canonical_json().map_err(|source| {
                    RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot(
                        InvalidStandardWriteBackAssetSnapshot::InvalidDialogueDefinition(Box::new(
                            source,
                        )),
                    )
                })?;
            let query_results = sqlite
                .query_existing_database_snapshot(
                    database_path.clone(),
                    standard_write_back_snapshot_queries(),
                )
                .await
                .map_err(|error| map_query_error(database_path, error))?;
            let rows = unpack_snapshot_query_results(query_results)
                .map_err(RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot)?;

            let prepared = cpu
                .execute(move || prepare_rows(rows, current_source))
                .await
                .map_err(RpgMakerStandardWriteBackAssetReadingError::SchedulePreparation)?
                .map_err(RpgMakerStandardWriteBackAssetReadingError::InvalidSnapshot)?;
            if !prepared.stale_owners.is_empty() {
                return Err(
                    RpgMakerStandardWriteBackAssetReadingError::ExtractionOutOfDate {
                        owners: prepared.stale_owners,
                    },
                );
            }

            let decoded_records = cpu
                .execute_ordered_map(prepared.records, decode_record)
                .await
                .map_err(RpgMakerStandardWriteBackAssetReadingError::ScheduleDecode)?;

            let owner_states = prepared.owner_states;
            cpu.execute(move || {
                let decoded = decoded_records.into_iter().collect::<Result<Vec<_>, _>>()?;
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
    SchedulePreparation(CpuTaskExecutionError<C>),
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
            Self::SchedulePreparation(source) => {
                write!(formatter, "写回资产快照准备任务执行失败：{source}")
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
            Self::SchedulePreparation(source)
            | Self::ScheduleDecode(source)
            | Self::ScheduleAssembly(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::DatabaseNotFound { .. } | Self::ExtractionOutOfDate { .. } => None,
        }
    }
}

impl<Q, C> RpgMakerStandardWriteBackAssetReadingError<Q, C>
where
    Q: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    /// 投影项目数据库路径、SQLite/CPU 稳定原因和损坏快照的精确结构位置。
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::DatabaseNotFound { database_path } => SafeDiagnostic::new(
                DiagnosticCode::WriteBackAssetRead,
                DiagnosticStage::WriteBack,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            Self::Query {
                database_path,
                source,
            } => source
                .safe_diagnostic_source(
                    DiagnosticStage::WriteBack,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .with_recovery(RecoveryFact::path(database_path)),
            Self::ExtractionOutOfDate { owners } => SafeDiagnostic::new(
                DiagnosticCode::WriteBackAssetRead,
                DiagnosticStage::WriteBack,
                DiagnosticSubject::operation("Standard asset owner state"),
                DiagnosticReason::failure(DiagnosticFailureKind::WriteBackExtractionOutOfDate),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component(format!(
                "stale_owners={}",
                owners
                    .iter()
                    .map(|owner| owner.storage_name())
                    .collect::<Vec<_>>()
                    .join(",")
            ))),
            Self::SchedulePreparation(source) => cpu_asset_diagnostic(source, "prepare_snapshot"),
            Self::ScheduleDecode(source) => cpu_asset_diagnostic(source, "decode_snapshot"),
            Self::ScheduleAssembly(source) => cpu_asset_diagnostic(source, "assemble_snapshot"),
            Self::InvalidSnapshot(source) => source.safe_diagnostic(),
        }
    }
}

impl<Q, C> SafeDiagnosticSource for RpgMakerStandardWriteBackAssetReadingError<Q, C>
where
    Q: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        _stage: DiagnosticStage,
        _impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        self.safe_diagnostic()
    }
}

fn cpu_asset_diagnostic<C>(
    source: &CpuTaskExecutionError<C>,
    operation: &'static str,
) -> SafeDiagnostic
where
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    source
        .safe_diagnostic_source(
            DiagnosticStage::WriteBack,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        )
        .with_recovery(RecoveryFact::component(format!(
            "write_back_asset_operation={operation}"
        )))
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
    InvalidOrderValue {
        column: &'static str,
        actual: i64,
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
    InvalidGroupOrder {
        owner: String,
        expected: usize,
        actual: i64,
    },
    InvalidUnitOrder {
        owner: String,
        group_location: String,
        expected: usize,
        actual: i64,
    },
    UnknownMutationAccess(String),
    NonCanonicalMutationResource {
        owner: String,
        group_location: String,
    },
    InvalidClaimSummary {
        owner: String,
        row_index: usize,
        kind: ClaimSummaryMismatchKind,
        expected_rows: usize,
        actual_rows: usize,
        details: Box<ClaimSummaryMismatchDetails>,
    },
    AssetFingerprintMismatch {
        owner: String,
    },
    InvalidDialogueDefinition(Box<MvDialogueDefinitionError>),
    InvalidLocation(RpgMakerLocationCodecError),
    InvalidProjection(RpgMakerProjectionCodecError),
    InvalidUnitContent {
        column: &'static str,
        source: serde_json::Error,
    },
    InvalidModel(StandardWriteBackSnapshotError),
}

impl InvalidStandardWriteBackAssetSnapshot {
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        let (subject, fact) = match self {
            Self::WrongQueryResultCount { expected, actual } => (
                DiagnosticSubject::operation("Standard asset query result sets"),
                format!("expected={expected}; actual={actual}"),
            ),
            Self::WrongColumnCount { expected, actual } => (
                DiagnosticSubject::operation("Standard asset row columns"),
                format!("expected={expected}; actual={actual}"),
            ),
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => (
                DiagnosticSubject::field(column),
                format!("expected_type={expected}; actual_type={actual}"),
            ),
            Self::InvalidOrderValue { column, actual } => (
                DiagnosticSubject::field(column),
                format!("expected=non_negative_order; actual={actual}"),
            ),
            Self::UnknownOwner(owner) => (
                DiagnosticSubject::field("owner"),
                format!("unknown_owner={owner}"),
            ),
            Self::DuplicateOwner(owner) => (
                DiagnosticSubject::field("owner"),
                format!("duplicate_owner={owner}"),
            ),
            Self::InvalidFingerprintLength {
                owner,
                column,
                actual,
            } => (
                DiagnosticSubject::field(column),
                format!("owner={owner}; expected_bytes=32; actual_bytes={actual}"),
            ),
            Self::AssetWithoutOwner(owner) => (
                DiagnosticSubject::field("owner"),
                format!("asset_without_active_owner={owner}"),
            ),
            Self::UnknownGroupKind(kind) => (
                DiagnosticSubject::field("group_kind"),
                format!("unknown_group_kind={kind}"),
            ),
            Self::DuplicateGroup {
                owner,
                group_location,
            } => (
                DiagnosticSubject::field("group_location"),
                format!("owner={owner}; duplicate_group={group_location}"),
            ),
            Self::MissingGroup {
                owner,
                group_location,
            } => (
                DiagnosticSubject::field("group_location"),
                format!("owner={owner}; missing_group={group_location}"),
            ),
            Self::InvalidGroupOrder {
                owner,
                expected,
                actual,
            } => (
                DiagnosticSubject::field("group_order"),
                format!("owner={owner}; expected={expected}; actual={actual}"),
            ),
            Self::InvalidUnitOrder {
                owner,
                group_location,
                expected,
                actual,
            } => (
                DiagnosticSubject::field("unit_order"),
                format!(
                    "owner={owner}; group={group_location}; expected={expected}; actual={actual}"
                ),
            ),
            Self::UnknownMutationAccess(access) => (
                DiagnosticSubject::field("access"),
                format!("unknown_mutation_access={access}"),
            ),
            Self::NonCanonicalMutationResource {
                owner,
                group_location,
            } => (
                DiagnosticSubject::field("resource_key"),
                format!("owner={owner}; group={group_location}; mutation_resource_non_canonical"),
            ),
            Self::InvalidClaimSummary {
                owner,
                row_index,
                kind,
                expected_rows,
                actual_rows,
                details,
            } => {
                let mut facts = format!(
                    "owner={owner}; summary_mismatch={}; row_index={row_index}; expected_rows={expected_rows}; actual_rows={actual_rows}",
                    kind.as_str()
                );
                if let Some(group) = &details.expected_group {
                    facts.push_str(&format!("; expected_group={group}"));
                }
                if let Some(group) = &details.actual_group {
                    facts.push_str(&format!("; actual_group={group}"));
                }
                if let Some(resource) = &details.expected_resource {
                    facts.push_str(&format!("; expected_resource={resource}"));
                }
                if let Some(resource) = &details.actual_resource {
                    facts.push_str(&format!("; actual_resource={resource}"));
                }
                if let Some(access) = &details.expected_access {
                    facts.push_str(&format!("; expected_access={}", access.storage_name()));
                }
                if let Some(access) = &details.actual_access {
                    facts.push_str(&format!("; actual_access={}", access.storage_name()));
                }
                (
                    DiagnosticSubject::operation("standard_mutation_claim collision summary"),
                    facts,
                )
            }
            Self::AssetFingerprintMismatch { owner } => (
                DiagnosticSubject::field("asset_snapshot_fingerprint"),
                format!("owner={owner}; stored_fingerprint_does_not_match_rows"),
            ),
            Self::InvalidDialogueDefinition(source) => {
                return invalid_dialogue_definition_diagnostic(source);
            }
            Self::InvalidLocation(source) => {
                return invalid_snapshot_detail_diagnostic(
                    DiagnosticSubject::field("group_location"),
                    source.safe_diagnostic_detail(),
                );
            }
            Self::InvalidProjection(source) => {
                return invalid_snapshot_detail_diagnostic(
                    DiagnosticSubject::field("projection_recipe_json"),
                    source.safe_diagnostic_detail(),
                );
            }
            Self::InvalidUnitContent { column, source } => (
                DiagnosticSubject::field(column),
                format!(
                    "unit_content_json_invalid; {}",
                    write_back_json_error_detail(source)
                ),
            ),
            Self::InvalidModel(source) => return invalid_write_back_model_diagnostic(source),
        };
        SafeDiagnostic::new(
            DiagnosticCode::WriteBackAssetRead,
            DiagnosticStage::WriteBack,
            subject,
            DiagnosticReason::failure(DiagnosticFailureKind::WriteBackAssetSnapshotInvalid),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(RecoveryFact::component(fact))
    }
}

#[derive(Debug)]
pub(crate) struct ClaimSummaryMismatchDetails {
    expected_group: Option<RpgMakerLocation>,
    actual_group: Option<RpgMakerLocation>,
    expected_resource: Option<MutationResource>,
    actual_resource: Option<MutationResource>,
    expected_access: Option<MutationResourceAccess>,
    actual_access: Option<MutationResourceAccess>,
}

fn invalid_dialogue_definition_diagnostic(source: &MvDialogueDefinitionError) -> SafeDiagnostic {
    let mut diagnostic = source.safe_diagnostic(
        DiagnosticStage::WriteBack,
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckProjectState,
    );
    diagnostic.code = DiagnosticCode::WriteBackAssetRead;
    diagnostic.stage = DiagnosticStage::WriteBack;
    diagnostic.impact = DiagnosticImpact::Unchanged;
    diagnostic.action = DiagnosticAction::CheckProjectState;
    diagnostic.with_recovery(RecoveryFact::component(
        "snapshot_field=mv_dialogue_definition",
    ))
}

fn invalid_snapshot_detail_diagnostic(
    subject: DiagnosticSubject,
    detail: String,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::WriteBackAssetRead,
        DiagnosticStage::WriteBack,
        subject,
        DiagnosticReason::failure_with_detail(
            DiagnosticFailureKind::WriteBackAssetSnapshotInvalid,
            detail,
        ),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckProjectState,
    )
}

fn write_back_json_error_detail(source: &serde_json::Error) -> String {
    let category = match source.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    format!(
        "json_category={category}; json_line={}; json_column={}",
        source.line(),
        source.column()
    )
}

fn invalid_write_back_model_diagnostic(source: &StandardWriteBackSnapshotError) -> SafeDiagnostic {
    let (subject, detail) = match source {
        StandardWriteBackSnapshotError::BlankSourceContent { role } => (
            DiagnosticSubject::field("source_content"),
            format!(
                "model_error=blank_source_content; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::BlankTranslationContent { role } => (
            DiagnosticSubject::field("translation_content"),
            format!(
                "model_error=blank_translation_content; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::ContentShapeMismatch { role } => (
            DiagnosticSubject::field("content_shape"),
            format!(
                "model_error=content_shape_mismatch; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::EmptyLineContent { role, column } => (
            DiagnosticSubject::field(column),
            format!(
                "model_error=empty_line_content; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::InvalidContentLine {
            role,
            column,
            line_index,
        } => (
            DiagnosticSubject::field(column),
            format!(
                "model_error=invalid_content_line; {}; line_index={line_index}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::AlignedLineCountMismatch {
            role,
            expected,
            actual,
        } => (
            DiagnosticSubject::field("translation_content"),
            format!(
                "model_error=aligned_line_count_mismatch; {}; expected={expected}; actual={actual}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::AlignedBlankLineMismatch { role, line_index } => (
            DiagnosticSubject::field("translation_content"),
            format!(
                "model_error=aligned_blank_line_mismatch; {}; line_index={line_index}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::EmptyProjection { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=empty_projection".to_owned(),
        ),
        StandardWriteBackSnapshotError::InvalidRole { kind, role } => (
            DiagnosticSubject::field("unit_role"),
            format!(
                "model_error=invalid_role; group_kind={}; {}",
                write_back_group_kind(*kind),
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::DuplicateRole {
            group_location,
            role,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!(
                "model_error=duplicate_role; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::RecipeRoleMismatch {
            group_location,
            units,
            recipes,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!(
                "model_error=recipe_role_mismatch; unit_roles={}; recipe_roles={}",
                write_back_role_set_detail(units),
                write_back_role_set_detail(recipes)
            ),
        ),
        StandardWriteBackSnapshotError::RecipeLineMismatch {
            group_location,
            role,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!(
                "model_error=recipe_line_mismatch; {}",
                write_back_role_detail(role)
            ),
        ),
        StandardWriteBackSnapshotError::RecipeClaimMismatch { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=recipe_claim_mismatch".to_owned(),
        ),
        StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
            group_location,
            target,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!("model_error=recipe_does_not_rebuild_original; target={target}"),
        ),
        StandardWriteBackSnapshotError::MutationClaimConflict { resource } => (
            DiagnosticSubject::operation("standard_mutation_claim"),
            format!("model_error=mutation_claim_conflict; resource={resource}"),
        ),
        StandardWriteBackSnapshotError::MismatchedClaimSource {
            group_location,
            claim,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!(
                "model_error=mismatched_claim_source; {}",
                write_back_claim_detail(claim)
            ),
        ),
        StandardWriteBackSnapshotError::MismatchedClaimResourceSource {
            group_location,
            resource,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!("model_error=mismatched_claim_resource_source; resource={resource}"),
        ),
        StandardWriteBackSnapshotError::InvalidDialogueProjection { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=invalid_dialogue_projection".to_owned(),
        ),
        StandardWriteBackSnapshotError::InvalidScrollingProjection { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=invalid_scrolling_projection".to_owned(),
        ),
        StandardWriteBackSnapshotError::InvalidScrollingRecipe { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=invalid_scrolling_recipe".to_owned(),
        ),
        StandardWriteBackSnapshotError::InvalidChoicesProjection { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=invalid_choices_projection".to_owned(),
        ),
        StandardWriteBackSnapshotError::InvalidDirectProjection { group_location } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            "model_error=invalid_direct_projection".to_owned(),
        ),
        StandardWriteBackSnapshotError::MismatchedDialogueGroup {
            group_location,
            recipe_location,
        } => (
            DiagnosticSubject::operation(format!("group_location={group_location}")),
            format!("model_error=mismatched_dialogue_group; recipe_location={recipe_location}"),
        ),
    };
    invalid_snapshot_detail_diagnostic(subject, detail)
}

fn write_back_role_detail(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::Scalar(_) => "role=scalar",
        TextUnitRole::DialogueSpeaker => "role=dialogue_speaker",
        TextUnitRole::DialogueBody => "role=dialogue_body",
        TextUnitRole::Choices => "role=choices",
        TextUnitRole::ScrollingText => "role=scrolling_text",
    }
}

fn write_back_role_set_detail(roles: &std::collections::BTreeSet<TextUnitRole>) -> String {
    roles
        .iter()
        .map(write_back_role_detail)
        .map(|role| role.strip_prefix("role=").unwrap_or(role))
        .collect::<Vec<_>>()
        .join(",")
}

fn write_back_group_kind(kind: TextGroupKind) -> &'static str {
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

fn write_back_claim_detail(claim: &MutationClaim) -> String {
    match claim {
        MutationClaim::Value(location) => format!("claim_kind=value; claim_location={location}"),
        MutationClaim::NoteTag(location) => {
            format!("claim_kind=note_tag; claim_location={location}")
        }
        MutationClaim::CommentTag { location, .. } => {
            format!("claim_kind=comment_tag; claim_location={location}")
        }
        MutationClaim::EventBlock { header, .. } => {
            format!("claim_kind=event_block; claim_location={header}")
        }
    }
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
            Self::InvalidOrderValue { column, actual } => {
                write!(
                    formatter,
                    "列 {column} 必须是可表示的非负顺序，实际为 {actual}"
                )
            }
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
                "单元或目标没有对应资产组：{owner} / {group_location}"
            ),
            Self::InvalidGroupOrder {
                owner,
                expected,
                actual,
            } => write!(
                formatter,
                "owner {owner} 的 group_order 必须从 0 连续：期待 {expected}，实际 {actual}"
            ),
            Self::InvalidUnitOrder {
                owner,
                group_location,
                expected,
                actual,
            } => write!(
                formatter,
                "组 {owner} / {group_location} 的 unit_order 必须从 0 连续：期待 {expected}，实际 {actual}"
            ),
            Self::UnknownMutationAccess(access) => {
                write!(formatter, "未知物理修改访问方式：{access}")
            }
            Self::NonCanonicalMutationResource {
                owner,
                group_location,
            } => write!(
                formatter,
                "owner {owner} 的组 {group_location} 使用了非规范 resource_key 编码"
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
                "owner {owner} 的 Claim 冲突摘要损坏：{}，第 {row_index} 行，期待 {expected_rows} 行，实际 {actual_rows} 行",
                kind.as_str()
            ),
            Self::AssetFingerprintMismatch { owner } => {
                write!(formatter, "资产所有者 {owner} 的快照指纹与三表内容不一致")
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

impl Error for InvalidStandardWriteBackAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidProjection(source) => Some(source),
            Self::InvalidUnitContent { source, .. } => Some(source),
            Self::InvalidModel(source) => Some(source),
            Self::InvalidDialogueDefinition(source) => Some(source.as_ref()),
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
    groups: Vec<SqliteRow>,
    units: Vec<SqliteRow>,
    claims: Vec<SqliteRow>,
}

enum SnapshotAssetRow {
    Group(SqliteRow),
    Unit(SqliteRow),
    Claim(SqliteRow),
}

struct PreparedRows {
    stale_owners: Vec<RpgMakerStandardAssetOwner>,
    owner_states: HashMap<RpgMakerStandardAssetOwner, OwnerState>,
    records: Vec<SnapshotAssetRow>,
}

fn prepare_rows(
    rows: SnapshotRows,
    current_source: SourceSnapshotFingerprint,
) -> Result<PreparedRows, InvalidStandardWriteBackAssetSnapshot> {
    let mut owner_states = HashMap::new();
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
                owner,
                OwnerState {
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
        records,
    })
}

enum DecodedRecord {
    Group {
        owner: RpgMakerStandardAssetOwner,
        group_location_raw: String,
        group_location: RpgMakerLocation,
        group_order: usize,
        kind: TextGroupKind,
        group_kind_raw: String,
        recipes: Vec<TextProjectionRecipe>,
        recipes_raw: String,
    },
    Unit {
        owner: RpgMakerStandardAssetOwner,
        group_location_raw: String,
        role: TextUnitRole,
        role_raw: String,
        unit_order: usize,
        source_content: TextUnitContent,
        source_content_json: String,
        source_context_json: String,
        translation_content: Option<TextUnitContent>,
    },
    Claim {
        owner: RpgMakerStandardAssetOwner,
        group_location_raw: String,
        access: MutationResourceAccess,
        resource_key_raw: String,
    },
}

fn decode_record(
    row: SnapshotAssetRow,
) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    match row {
        SnapshotAssetRow::Group(row) => decode_group(row),
        SnapshotAssetRow::Unit(row) => decode_unit(row),
        SnapshotAssetRow::Claim(row) => decode_claim(row),
    }
}

fn decode_group(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [
        owner,
        group_location,
        group_order,
        group_kind,
        projection_recipe_json,
    ] = values.try_into().map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 5,
            actual,
        },
    )?;
    let owner = parse_owner(&owned_text(owner, "owner")?)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let group_kind_raw = owned_text(group_kind, "group_kind")?;
    let recipes_raw = owned_text(projection_recipe_json, "projection_recipe_json")?;
    Ok(DecodedRecord::Group {
        owner,
        group_location: RpgMakerLocationCodec::decode(&group_location_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidLocation)?,
        group_location_raw,
        group_order: owned_non_negative_order(group_order, "group_order")?,
        kind: parse_group_kind(&group_kind_raw)?,
        group_kind_raw,
        recipes: RpgMakerProjectionCodec::decode_recipes(&recipes_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
        recipes_raw,
    })
}

fn decode_unit(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [
        owner,
        group_location,
        unit_role,
        unit_order,
        source_content_json,
        source_context_json,
        translation_content_json,
    ] = values.try_into().map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            expected: 7,
            actual,
        },
    )?;
    let owner = parse_owner(&owned_text(owner, "owner")?)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let role_raw = owned_text(unit_role, "unit_role")?;
    let source_content_json = owned_text(source_content_json, "source_content_json")?;
    let source_content = serde_json::from_str(&source_content_json).map_err(|source| {
        InvalidStandardWriteBackAssetSnapshot::InvalidUnitContent {
            column: "source_content_json",
            source,
        }
    })?;
    let source_context_json = owned_text(source_context_json, "source_context_json")?;
    let translation_content_json =
        optional_owned_text(translation_content_json, "translation_content_json")?;
    let translation_content = translation_content_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(
            |source| InvalidStandardWriteBackAssetSnapshot::InvalidUnitContent {
                column: "translation_content_json",
                source,
            },
        )?;
    Ok(DecodedRecord::Unit {
        owner,
        group_location_raw,
        role: RpgMakerProjectionCodec::decode_role(&role_raw)
            .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
        role_raw,
        unit_order: owned_non_negative_order(unit_order, "unit_order")?,
        source_content,
        source_content_json,
        source_context_json,
        translation_content,
    })
}

fn decode_claim(row: SqliteRow) -> Result<DecodedRecord, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    let actual = values.len();
    let [owner, group_location, resource_key, access] =
        values.try_into().map_err(
            |_| InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
                expected: 4,
                actual,
            },
        )?;
    let owner = parse_owner(&owned_text(owner, "owner")?)?;
    let group_location_raw = owned_text(group_location, "group_location")?;
    let resource_key_raw = owned_text(resource_key, "resource_key")?;
    let access_raw = owned_text(access, "access")?;
    let access = MutationResourceAccess::from_storage_name(&access_raw).ok_or_else(|| {
        InvalidStandardWriteBackAssetSnapshot::UnknownMutationAccess(access_raw.clone())
    })?;
    Ok(DecodedRecord::Claim {
        owner,
        group_location_raw,
        access,
        resource_key_raw,
    })
}

struct GroupBuilder {
    owner: RpgMakerStandardAssetOwner,
    group_location_raw: String,
    group_order: usize,
    kind: TextGroupKind,
    location: RpgMakerLocation,
    recipes: Vec<TextProjectionRecipe>,
    units: Vec<StandardWriteBackUnit>,
}

/// 校验侧对 `standard_asset` 唯一 framing 定义的薄包装。
///
/// project_definition 帧只属于 Builtin owner:该 owner 的对话定义是快照语义的一
/// 部分,Rules/Lua 快照不携带项目定义。这一 owner 判断与写入侧"提供即掺入"的
/// 调用约定共同构成同一事实,framing 本身由 `StandardTextSnapshotFingerprintBuilder`
/// 唯一拥有。
struct SnapshotFingerprintAccumulator {
    builder: StandardTextSnapshotFingerprintBuilder,
}

impl SnapshotFingerprintAccumulator {
    fn new(owner: RpgMakerStandardAssetOwner, dialogue_definition_json: &str) -> Self {
        let project_definition_json =
            (owner == RpgMakerStandardAssetOwner::Builtin).then_some(dialogue_definition_json);
        Self {
            builder: StandardTextSnapshotFingerprintBuilder::new(owner, project_definition_json),
        }
    }

    fn group(&mut self, group_location: &str, group_order: usize, group_kind: &str, recipes: &str) {
        self.builder
            .group(group_location, group_order, group_kind, recipes);
    }

    fn unit(
        &mut self,
        group_location: &str,
        role: &str,
        unit_order: usize,
        source: &str,
        context: &str,
    ) {
        self.builder
            .unit(group_location, role, unit_order, source, context);
    }

    fn claim(&mut self, resource_key: &str, access: &str, group_location: &str) {
        self.builder.claim(resource_key, access, group_location);
    }

    fn finish(self) -> AssetSnapshotFingerprint {
        AssetSnapshotFingerprint::from_bytes(self.builder.finish().into_bytes())
    }
}

fn assemble_snapshot(
    owner_states: HashMap<RpgMakerStandardAssetOwner, OwnerState>,
    records: impl IntoIterator<Item = DecodedRecord>,
    dialogue_definition_json: &str,
) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
    let mut groups = Vec::<GroupBuilder>::new();
    let mut group_indexes = HashMap::<RpgMakerStandardAssetOwner, HashMap<String, usize>>::new();
    let mut next_group_orders = HashMap::<RpgMakerStandardAssetOwner, usize>::new();
    let mut stored_claim_summaries =
        HashMap::<RpgMakerStandardAssetOwner, Vec<EncodedMutationClaim>>::new();
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
            return Err(InvalidStandardWriteBackAssetSnapshot::AssetWithoutOwner(
                owner.storage_name().to_owned(),
            ));
        }
        match record {
            DecodedRecord::Group {
                owner,
                group_location_raw,
                group_location,
                group_order,
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
                        group_order,
                        &group_kind_raw,
                        &recipes_raw,
                    );
                let owner_group_indexes = group_indexes.entry(owner).or_default();
                if owner_group_indexes.contains_key(&group_location_raw) {
                    return Err(InvalidStandardWriteBackAssetSnapshot::DuplicateGroup {
                        owner: owner.storage_name().to_owned(),
                        group_location: group_location_raw,
                    });
                }
                let expected = *next_group_orders.entry(owner).or_default();
                if group_order != expected {
                    return Err(InvalidStandardWriteBackAssetSnapshot::InvalidGroupOrder {
                        owner: owner.storage_name().to_owned(),
                        expected,
                        actual: i64::try_from(group_order).unwrap_or(i64::MAX),
                    });
                }
                *next_group_orders
                    .get_mut(&owner)
                    .expect("owner group_order 计数已建立") += 1;
                let index = groups.len();
                owner_group_indexes.insert(group_location_raw.clone(), index);
                groups.push(GroupBuilder {
                    owner,
                    group_location_raw,
                    group_order,
                    kind,
                    location: group_location,
                    recipes,
                    units: Vec::new(),
                });
            }
            DecodedRecord::Unit {
                owner,
                group_location_raw,
                role,
                role_raw,
                unit_order,
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
                        unit_order,
                        &source_content_json,
                        &source_context_json,
                    );
                let index = group_indexes
                    .get(&owner)
                    .and_then(|indexes| indexes.get(&group_location_raw))
                    .copied()
                    .ok_or(InvalidStandardWriteBackAssetSnapshot::MissingGroup {
                        owner: owner.storage_name().to_owned(),
                        group_location: group_location_raw.clone(),
                    })?;
                let group = &mut groups[index];
                let expected = group.units.len();
                if unit_order != expected {
                    return Err(InvalidStandardWriteBackAssetSnapshot::InvalidUnitOrder {
                        owner: owner.storage_name().to_owned(),
                        group_location: group_location_raw,
                        expected,
                        actual: i64::try_from(unit_order).unwrap_or(i64::MAX),
                    });
                }
                group.units.push(
                    StandardWriteBackUnit::new(role, source_content, translation_content)
                        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?,
                );
            }
            DecodedRecord::Claim {
                owner,
                group_location_raw,
                access,
                resource_key_raw,
            } => {
                let index = group_indexes
                    .get(&owner)
                    .and_then(|indexes| indexes.get(&group_location_raw))
                    .copied()
                    .ok_or_else(|| InvalidStandardWriteBackAssetSnapshot::MissingGroup {
                        owner: owner.storage_name().to_owned(),
                        group_location: group_location_raw.clone(),
                    })?;
                let group = &groups[index];
                stored_claim_summaries
                    .entry(owner)
                    .or_default()
                    .push(EncodedMutationClaim::new(
                        resource_key_raw,
                        access,
                        group_location_raw,
                        group.group_order,
                    ));
            }
        }
    }

    let mut logical_claims =
        HashMap::<RpgMakerStandardAssetOwner, Vec<EncodedMutationClaim>>::new();
    let mut validated_groups = Vec::with_capacity(groups.len());
    for group in groups {
        let group_location_raw = group.group_location_raw;
        let group_order = group.group_order;
        let owner = group.owner;
        let group = StandardWriteBackGroup::from_recipes(
            group.kind,
            group.location,
            group.units,
            group.recipes,
        )
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?;
        let owner_claims = logical_claims.entry(owner).or_default();
        for lock in group.mutation_claims().locks() {
            owner_claims.push(EncodedMutationClaim::new(
                RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                    .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection)?,
                lock.access(),
                group_location_raw.clone(),
                group_order,
            ));
        }
        validated_groups.push(group);
    }

    let snapshot = StandardWriteBackSnapshot::new(validated_groups)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?;

    for owner in STANDARD_WRITE_BACK_OWNER_ORDER {
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
            return Err(
                InvalidStandardWriteBackAssetSnapshot::AssetFingerprintMismatch {
                    owner: owner.storage_name().to_owned(),
                },
            );
        }
    }

    Ok(snapshot)
}

fn validate_claim_summary(
    owner: RpgMakerStandardAssetOwner,
    logical_claims: &[EncodedMutationClaim],
    actual: &[EncodedMutationClaim],
) -> Result<(), InvalidStandardWriteBackAssetSnapshot> {
    let mut expected = borrowed_collision_summary(logical_claims);
    let mut row_index = 0;
    loop {
        let expected_row = expected
            .next()
            .transpose()
            .expect("StandardWriteBackSnapshot 已验证 owner 内 Claim 不冲突");
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
                        || expected.group_order != actual.group_order =>
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
            .expect("StandardWriteBackSnapshot 已验证 owner 内 Claim 不冲突")
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
                    left.group_order
                        .cmp(&right.group_order)
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
    owner: RpgMakerStandardAssetOwner,
    row_index: usize,
    kind: ClaimSummaryMismatchKind,
    expected_rows: usize,
    actual_rows: &[EncodedMutationClaim],
    compared: ComparedClaimSummaryRows<'_>,
    actual_resource: Option<MutationResource>,
) -> InvalidStandardWriteBackAssetSnapshot {
    let decode_group = |claim: Option<&EncodedMutationClaim>| {
        claim.map(|claim| {
            RpgMakerLocationCodec::decode(&claim.group_location)
                .expect("摘要 group_location 已在同一读取边界完成规范解码")
        })
    };
    InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
        owner: owner.storage_name().to_owned(),
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
) -> Option<MutationResource> {
    claim.map(|claim| {
        RpgMakerProjectionCodec::decode_mutation_resource(&claim.resource_key)
            .expect("重建的摘要 resource_key 必须是规范编码")
    })
}

fn decode_actual_summary_resource(
    owner: RpgMakerStandardAssetOwner,
    claim: Option<&EncodedMutationClaim>,
) -> Result<Option<MutationResource>, InvalidStandardWriteBackAssetSnapshot> {
    let Some(claim) = claim else {
        return Ok(None);
    };
    match RpgMakerProjectionCodec::decode_mutation_resource(&claim.resource_key) {
        Ok(resource) => Ok(Some(resource)),
        Err(RpgMakerProjectionCodecError::NonCanonical) => Err(
            InvalidStandardWriteBackAssetSnapshot::NonCanonicalMutationResource {
                owner: owner.storage_name().to_owned(),
                group_location: claim.group_location.clone(),
            },
        ),
        Err(source) => Err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection(
            source,
        )),
    }
}

#[cfg(test)]
#[derive(Default)]
struct FingerprintRows {
    groups: Vec<(String, usize, String, String)>,
    units: Vec<(String, String, usize, String, String)>,
    claims: Vec<(String, String, String)>,
}

#[cfg(test)]
fn snapshot_fingerprint(
    owner: RpgMakerStandardAssetOwner,
    mut rows: FingerprintRows,
    dialogue_definition_json: &str,
) -> AssetSnapshotFingerprint {
    rows.groups.sort_by_key(|row| row.1);
    rows.units.sort_by_key(|row| row.2);
    rows.claims.sort();
    let mut accumulator = SnapshotFingerprintAccumulator::new(owner, dialogue_definition_json);
    for (group_location, group_order, group_kind, recipes) in rows.groups {
        accumulator.group(&group_location, group_order, &group_kind, &recipes);
    }
    for (group_location, role, unit_order, source, context) in rows.units {
        accumulator.unit(&group_location, &role, unit_order, &source, &context);
    }
    for (resource_key, access, group_location) in rows.claims {
        accumulator.claim(&resource_key, &access, &group_location);
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

fn owned_non_negative_order(
    value: SqliteValue,
    column: &'static str,
) -> Result<usize, InvalidStandardWriteBackAssetSnapshot> {
    let SqliteValue::Integer(value) = value else {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "INTEGER",
            actual: value.kind_name(),
        });
    };
    usize::try_from(value).map_err(
        |_| InvalidStandardWriteBackAssetSnapshot::InvalidOrderValue {
            column,
            actual: value,
        },
    )
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
    use std::collections::BTreeSet;

    use super::*;
    use crate::diagnostic::render_safe_diagnostic;
    use crate::i18n::{UiLocale, UiLocalizer};
    use crate::rpg_maker::model::{DirectTextPart, DirectTextRecipe, ScalarFieldKey};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource, StandardDataFile};
    use rusqlite::params_from_iter;

    fn diagnostic_surfaces(diagnostic: &SafeDiagnostic) -> (String, String) {
        let json = serde_json::to_string(diagnostic).expect("安全诊断应可序列化");
        let mut cli = Vec::new();
        render_safe_diagnostic(
            diagnostic,
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &mut cli,
        )
        .expect("安全诊断应可渲染");
        (json, String::from_utf8(cli).expect("CLI 诊断应为 UTF-8"))
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
        let resource = MutationResource::Value {
            source: source.clone(),
            steps: vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        };
        let cases = vec![
            (
                StandardWriteBackSnapshotError::BlankSourceContent {
                    role: TextUnitRole::DialogueBody,
                },
                vec!["model_error=blank_source_content", "role=dialogue_body"],
            ),
            (
                StandardWriteBackSnapshotError::BlankTranslationContent {
                    role: scalar.clone(),
                },
                vec!["model_error=blank_translation_content", "role=scalar"],
            ),
            (
                StandardWriteBackSnapshotError::ContentShapeMismatch {
                    role: TextUnitRole::Choices,
                },
                vec!["model_error=content_shape_mismatch", "role=choices"],
            ),
            (
                StandardWriteBackSnapshotError::EmptyLineContent {
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
                StandardWriteBackSnapshotError::InvalidContentLine {
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
                StandardWriteBackSnapshotError::AlignedLineCountMismatch {
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
                StandardWriteBackSnapshotError::AlignedBlankLineMismatch {
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
                StandardWriteBackSnapshotError::EmptyProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=empty_projection", "data/Items.json[1]"],
            ),
            (
                StandardWriteBackSnapshotError::InvalidRole {
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
                StandardWriteBackSnapshotError::DuplicateRole {
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
                StandardWriteBackSnapshotError::RecipeRoleMismatch {
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
                StandardWriteBackSnapshotError::RecipeLineMismatch {
                    group_location: Box::new(group_location.clone()),
                    role: TextUnitRole::ScrollingText,
                },
                vec!["model_error=recipe_line_mismatch", "role=scrolling_text"],
            ),
            (
                StandardWriteBackSnapshotError::RecipeClaimMismatch {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=recipe_claim_mismatch", "data/Items.json[1]"],
            ),
            (
                StandardWriteBackSnapshotError::RecipeDoesNotRebuildOriginal {
                    group_location: Box::new(group_location.clone()),
                    target: Box::new(target.clone()),
                },
                vec![
                    "model_error=recipe_does_not_rebuild_original",
                    "target=data/Items.json[1].name",
                ],
            ),
            (
                StandardWriteBackSnapshotError::MutationClaimConflict {
                    resource: Box::new(resource.clone()),
                },
                vec![
                    "model_error=mutation_claim_conflict",
                    "resource=data/Items.json[1].name",
                ],
            ),
            (
                StandardWriteBackSnapshotError::MismatchedClaimSource {
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
                StandardWriteBackSnapshotError::MismatchedClaimResourceSource {
                    group_location: Box::new(group_location.clone()),
                    resource: Box::new(resource),
                },
                vec![
                    "model_error=mismatched_claim_resource_source",
                    "resource=data/Items.json[1].name",
                ],
            ),
            (
                StandardWriteBackSnapshotError::InvalidDialogueProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_dialogue_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                StandardWriteBackSnapshotError::InvalidScrollingProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_scrolling_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                StandardWriteBackSnapshotError::InvalidScrollingRecipe {
                    group_location: Box::new(group_location.clone()),
                },
                vec!["model_error=invalid_scrolling_recipe", "data/Items.json[1]"],
            ),
            (
                StandardWriteBackSnapshotError::InvalidChoicesProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_choices_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                StandardWriteBackSnapshotError::InvalidDirectProjection {
                    group_location: Box::new(group_location.clone()),
                },
                vec![
                    "model_error=invalid_direct_projection",
                    "data/Items.json[1]",
                ],
            ),
            (
                StandardWriteBackSnapshotError::MismatchedDialogueGroup {
                    group_location: Box::new(group_location),
                    recipe_location: Box::new(recipe_location),
                },
                vec![
                    "model_error=mismatched_dialogue_group",
                    "recipe_location=data/Map002.json",
                ],
            ),
        ];

        for (source, expected_facts) in cases {
            let diagnostic =
                InvalidStandardWriteBackAssetSnapshot::InvalidModel(source).safe_diagnostic();
            assert_eq!(diagnostic.code, DiagnosticCode::WriteBackAssetRead);
            assert_eq!(diagnostic.stage, DiagnosticStage::WriteBack);
            assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
            assert_eq!(diagnostic.action, DiagnosticAction::CheckProjectState);
            let (json, cli) = diagnostic_surfaces(&diagnostic);
            for fact in expected_facts {
                assert!(json.contains(fact), "JSONL 缺少 {fact}: {json}");
                assert!(cli.contains(fact), "CLI 缺少 {fact}: {cli}");
            }
        }
    }

    #[test]
    fn write_back_snapshot_typed_sources_keep_stable_facts_without_copying_body_text() {
        const SOURCE_BODY: &str = "SENTINEL_WRITE_BACK_SNAPSHOT_BODY_329c";

        let pcre_source = pcre2::bytes::RegexBuilder::new()
            .build(&format!("(?<{SOURCE_BODY}"))
            .expect_err("测试 PCRE2 应无效");
        let dialogue = InvalidStandardWriteBackAssetSnapshot::InvalidDialogueDefinition(Box::new(
            MvDialogueDefinitionError::InvalidPattern {
                rule_number: 7,
                source: pcre_source,
            },
        ))
        .safe_diagnostic();
        let location = InvalidStandardWriteBackAssetSnapshot::InvalidLocation(
            RpgMakerLocationCodecError::InvalidDataFile(SOURCE_BODY.to_owned()),
        )
        .safe_diagnostic();
        let projection = InvalidStandardWriteBackAssetSnapshot::InvalidProjection(
            RpgMakerProjectionCodecError::Projection(
                crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                    expected: 2,
                    actual: 5,
                },
            ),
        )
        .safe_diagnostic();
        let invalid_json = format!("{{\"{SOURCE_BODY}\":");
        let unit_content = InvalidStandardWriteBackAssetSnapshot::InvalidUnitContent {
            column: "translation_content_json",
            source: serde_json::from_str::<serde_json::Value>(&invalid_json)
                .expect_err("测试 JSON 应不完整"),
        }
        .safe_diagnostic();

        for (diagnostic, expected_facts) in [
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
            let (json, cli) = diagnostic_surfaces(&diagnostic);
            assert!(!json.contains(SOURCE_BODY), "JSONL 不应复制正文：{json}");
            assert!(!cli.contains(SOURCE_BODY), "CLI 不应复制正文：{cli}");
            for fact in expected_facts {
                assert!(json.contains(fact), "JSONL 缺少 {fact}: {json}");
                assert!(cli.contains(fact), "CLI 缺少 {fact}: {cli}");
            }
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
            groups: Vec::new(),
            units: Vec::new(),
            claims: Vec::new(),
        }
    }

    fn scalar_snapshot_rows(indices: &[usize]) -> SnapshotRows {
        const OWNER: RpgMakerStandardAssetOwner = RpgMakerStandardAssetOwner::Builtin;
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

        for (group_order, index) in indices.iter().copied().enumerate() {
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
                    target,
                    SOURCE_TEXT,
                    vec![DirectTextPart::TextSlot { role: role.clone() }],
                )
                .expect("直接配方应合法"),
            )];
            let group_location_raw =
                RpgMakerLocationCodec::encode(&group_location).expect("组位置应可编码");
            let recipes_raw =
                RpgMakerProjectionCodec::encode_recipes(&recipes).expect("配方应可编码");
            groups.push(SqliteRow::new(vec![
                SqliteValue::Text(OWNER.storage_name().to_owned()),
                SqliteValue::Text(group_location_raw.clone()),
                SqliteValue::Integer(i64::try_from(group_order).expect("顺序应可编码")),
                SqliteValue::Text("database_entry".to_owned()),
                SqliteValue::Text(recipes_raw.clone()),
            ]));
            units.push(SqliteRow::new(vec![
                SqliteValue::Text(OWNER.storage_name().to_owned()),
                SqliteValue::Text(group_location_raw.clone()),
                SqliteValue::Text(role_raw.clone()),
                SqliteValue::Integer(0),
                SqliteValue::Text(source_content_json.clone()),
                SqliteValue::Text("{}".to_owned()),
                SqliteValue::Null,
            ]));
            fingerprint_rows.groups.push((
                group_location_raw.clone(),
                group_order,
                "database_entry".to_owned(),
                recipes_raw,
            ));
            fingerprint_rows.units.push((
                group_location_raw.clone(),
                role_raw.clone(),
                0,
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
                    group_order,
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
                SqliteRow::new(vec![
                    SqliteValue::Text(OWNER.storage_name().to_owned()),
                    SqliteValue::Text(claim.group_location),
                    SqliteValue::Text(claim.resource_key),
                    SqliteValue::Text(claim.access.storage_name().to_owned()),
                ])
            })
            .collect();
        let fingerprint = snapshot_fingerprint(OWNER, fingerprint_rows, DIALOGUE_DEFINITION);
        SnapshotRows {
            owners: vec![owner_row([1; 32], *fingerprint.as_bytes())],
            groups,
            units,
            claims,
        }
    }

    fn assemble_test_rows(
        rows: SnapshotRows,
    ) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
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
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE standard_text_group (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    group_order INTEGER NOT NULL,
                    group_kind TEXT NOT NULL,
                    projection_recipe_json TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location),
                    UNIQUE (owner, group_order)
                );
                CREATE TABLE standard_text_unit (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    unit_role TEXT NOT NULL,
                    unit_order INTEGER NOT NULL,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location, unit_role),
                    UNIQUE (owner, group_location, unit_order)
                );
                CREATE TABLE standard_mutation_claim (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    resource_key TEXT NOT NULL,
                    access TEXT NOT NULL,
                    PRIMARY KEY (owner, group_location, resource_key)
                );
                CREATE INDEX standard_mutation_claim_owner_resource_idx
                    ON standard_mutation_claim(owner, resource_key, access, group_location);

                INSERT INTO standard_asset_owner_state VALUES ('lua', zeroblob(32), zeroblob(32));
                INSERT INTO standard_asset_owner_state VALUES ('rules', zeroblob(32), zeroblob(32));
                INSERT INTO standard_asset_owner_state VALUES ('builtin', zeroblob(32), zeroblob(32));
                INSERT INTO standard_text_group VALUES ('builtin', 'group-b', 1, 'map', '[]');
                INSERT INTO standard_text_group VALUES ('builtin', 'group-a', 0, 'map', '[]');
                INSERT INTO standard_text_unit VALUES ('builtin', 'group-b', 'role-z', 0, '"z"', '{}', NULL, 'untranslated');
                INSERT INTO standard_text_unit VALUES ('builtin', 'group-a', 'role-y', 0, '"y"', '{}', NULL, 'untranslated');
                INSERT INTO standard_mutation_claim VALUES ('builtin', 'group-a', 'resource-z', 'exclusive');
                INSERT INTO standard_mutation_claim VALUES ('builtin', 'group-b', 'resource-a', 'intent');
                "#,
            )
            .expect("测试快照表与行应可建立");

        let owners = connection
            .prepare(READ_STANDARD_WRITE_BACK_OWNER_STATES)
            .expect("owner 查询应可建立")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("owner 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner 行应可读取");
        let groups = connection
            .prepare(READ_STANDARD_WRITE_BACK_OWNER_GROUPS)
            .expect("group 查询应可建立")
            .query_map(["builtin"], |row| row.get::<_, String>(1))
            .expect("group 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("group 行应可读取");
        let units = connection
            .prepare(READ_STANDARD_WRITE_BACK_OWNER_UNITS)
            .expect("unit 查询应可建立")
            .query_map(["builtin"], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .expect("unit 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("unit 行应可读取");
        let claims = connection
            .prepare(READ_STANDARD_WRITE_BACK_OWNER_CLAIMS)
            .expect("Claim 查询应可建立")
            .query_map(["builtin"], |row| {
                Ok((row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })
            .expect("Claim 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("Claim 行应可读取");

        assert_eq!(owners, ["builtin", "rules", "lua"]);
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

        let queries = standard_write_back_snapshot_queries();
        assert_eq!(queries.len(), STANDARD_WRITE_BACK_QUERY_RESULT_COUNT);
        let (owner_query, partition_queries) = queries
            .split_first()
            .expect("写回快照查询至少包含 owner 状态");
        assert!(
            owner_query.statement().contains("CASE owner"),
            "至多三行的 owner 状态查询必须恢复 Builtin、Rules、Lua 规范顺序"
        );
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
            assert!(
                details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
                "owner 窄查询不得建立全表临时排序：{details:?}"
            );
        }
    }

    #[test]
    fn prepare_rows_moves_asset_rows_into_indexed_natural_order_work() {
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
            groups: vec![make_row("group-0", 5), make_row("group-1", 5)],
            units: vec![make_row("unit-0", 7), make_row("unit-1", 7)],
            claims: vec![make_row("claim-0", 4)],
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
                    | SnapshotAssetRow::Claim(row) => match &row.values()[0] {
                        SqliteValue::Text(value) => value.as_ptr(),
                        value => panic!("owner 应为 TEXT，实际为 {}", value.kind_name()),
                    },
                })
                .collect::<Vec<_>>(),
            pointers
        );
    }

    #[test]
    fn decode_record_compacts_owner_and_moves_large_payload_text_out_of_sqlite_values() {
        let owner = "builtin".to_owned();
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
        let row = SqliteRow::new(vec![
            SqliteValue::Text(owner),
            SqliteValue::Text(group_location),
            SqliteValue::Text(role),
            SqliteValue::Integer(0),
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

        assert_eq!(owner, RpgMakerStandardAssetOwner::Builtin);
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
        assert_eq!(stale.stale_owners, [RpgMakerStandardAssetOwner::Builtin]);

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
            SqliteValue::Integer(0),
            SqliteValue::Text("event_dialogue".to_owned()),
            SqliteValue::Text("{not-json".to_owned()),
        ]);
        assert!(matches!(
            decode_group(row),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection(_))
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
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::MissingRow,
                ..
            })
        ));

        let mut duplicate = scalar_snapshot_rows(&[9, 1]);
        let duplicated = duplicate.claims[0].clone();
        duplicate.claims.insert(1, duplicated);
        assert!(matches!(
            assemble_test_rows(duplicate),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
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
                    row.values(),
                    [_, _, _, SqliteValue::Text(access)] if access == "intent"
                )
            })
            .expect("测试摘要应包含 Intent");
        let mut values = wrong_access.claims[row_index].clone().into_values();
        values[3] = SqliteValue::Text("exclusive".to_owned());
        wrong_access.claims[row_index] = SqliteRow::new(values);
        assert!(matches!(
            assemble_test_rows(wrong_access),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Access,
                ..
            })
        ));

        let mut wrong_representative = scalar_snapshot_rows(&[9, 1]);
        let second_group = match &wrong_representative.groups[1].values()[1] {
            SqliteValue::Text(value) => value.clone(),
            _ => unreachable!("group_location 必须为 TEXT"),
        };
        let root_resource =
            RpgMakerProjectionCodec::encode_mutation_resource(&MutationResource::Value {
                source: RpgMakerSource::data(StandardDataFile::Items),
                steps: Vec::new(),
            })
            .expect("根资源应可编码");
        let row_index = wrong_representative
            .claims
            .iter()
            .position(|row| {
                matches!(
                    row.values(),
                    [_, _, SqliteValue::Text(resource), _] if resource == &root_resource
                )
            })
            .expect("测试摘要应包含共享根 Intent");
        let mut values = wrong_representative.claims[row_index].clone().into_values();
        values[1] = SqliteValue::Text(second_group);
        wrong_representative.claims[row_index] = SqliteRow::new(values);
        assert!(matches!(
            assemble_test_rows(wrong_representative),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
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
        let diagnostic =
            serde_json::to_string(&error.safe_diagnostic()).expect("安全诊断应可序列化");

        assert!(diagnostic.contains("expected_resource=data/Items.json"));
        assert!(!diagnostic.contains("[\\\"v\\\""));
    }

    #[test]
    fn collision_summary_validation_borrows_the_earliest_representative() {
        let mut claims = vec![
            EncodedMutationClaim::new(
                "resource".to_owned(),
                MutationResourceAccess::Intent,
                "group-a".to_owned(),
                8,
            ),
            EncodedMutationClaim::new(
                "resource".to_owned(),
                MutationResourceAccess::Intent,
                "group-z".to_owned(),
                2,
            ),
        ];
        sort_logical_claims(&mut claims);
        let earliest = claims
            .iter()
            .find(|claim| claim.group_order == 2)
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
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
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

        let mut first = rows.claims[0].clone().into_values();
        let SqliteValue::Text(access) = &mut first[3] else {
            unreachable!("access 必须为 TEXT")
        };
        *access = match access.as_str() {
            "intent" => "exclusive".to_owned(),
            "exclusive" => "intent".to_owned(),
            other => unreachable!("测试 access 无效：{other}"),
        };
        rows.claims[0] = SqliteRow::new(first);

        assert!(matches!(
            assemble_test_rows(rows),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidClaimSummary {
                kind: ClaimSummaryMismatchKind::Access,
                row_index: 0,
                ..
            })
        ));
    }

    #[test]
    fn claim_resource_decode_is_deferred_until_the_first_summary_mismatch() {
        let mut rows = scalar_snapshot_rows(&[1]);
        let mut values = rows.claims[0].clone().into_values();
        let SqliteValue::Text(resource) = &mut values[2] else {
            unreachable!("resource_key 必须为 TEXT")
        };
        *resource = format!(" {resource} ");
        rows.claims[0] = SqliteRow::new(values);

        assert!(matches!(
            decode_claim(rows.claims[0].clone()),
            Ok(DecodedRecord::Claim {
                resource_key_raw,
                ..
            }) if resource_key_raw.starts_with(' ')
        ));
        match assemble_test_rows(rows) {
            Err(InvalidStandardWriteBackAssetSnapshot::NonCanonicalMutationResource { .. }) => {}
            Err(error) => panic!("实际错误：{error:?}"),
            Ok(_) => panic!("非规范 resource_key 不得成功"),
        }
    }

    #[test]
    fn invalid_claim_resource_keeps_projection_error_after_lazy_decode() {
        let mut rows = scalar_snapshot_rows(&[1]);
        let mut values = rows.claims[0].clone().into_values();
        values[2] = SqliteValue::Text("!".to_owned());
        rows.claims[0] = SqliteRow::new(values);

        assert!(matches!(
            decode_claim(rows.claims[0].clone()),
            Ok(DecodedRecord::Claim {
                resource_key_raw,
                ..
            }) if resource_key_raw == "!"
        ));
        assert!(matches!(
            assemble_test_rows(rows),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidProjection(_))
        ));
    }
}
