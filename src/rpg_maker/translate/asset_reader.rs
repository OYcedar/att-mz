//! 从统一 RPG Maker 标准文本表建立一致翻译语料。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    AssetSnapshotFingerprint, PLACEHOLDER_RULES_RESOURCE_KIND, SourceSnapshotFingerprint,
    TERMINOLOGY_RESOURCE_KIND,
};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::rpg_maker::text::{RpgMakerLocation, TextGroupKind};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardTranslationAsset, StandardTranslationAssetReader, StandardTranslationCorpus,
    StandardTranslationGroup, TranslationOwnerSnapshot, TranslationUnitIdentity,
};

const READ_TRANSLATION_METADATA: &str = "SELECT source_snapshot_fingerprint FROM metadata";

const READ_TRANSLATION_OWNERS: &str = r#"SELECT
    owner,
    source_snapshot_fingerprint,
    asset_snapshot_fingerprint
FROM standard_asset_owner_state
ORDER BY owner COLLATE BINARY"#;

const READ_TRANSLATION_RESOURCES: &str = r#"SELECT resource_kind, canonical_json
FROM standard_translation_resource
ORDER BY resource_kind"#;

const READ_TRANSLATION_OWNER_GROUPS: &str = r#"SELECT
    group_location,
    group_kind,
    group_order
FROM standard_text_group
WHERE owner = ?
ORDER BY group_order"#;

const READ_TRANSLATION_OWNER_UNITS: &str = r#"SELECT
    unit.group_location,
    text_group.group_kind,
    text_group.group_order,
    unit.unit_role,
    unit.unit_order,
    unit.source_content_json,
    unit.source_context_json,
    unit.translation_content_json,
    unit.translation_state
FROM standard_text_group AS text_group
CROSS JOIN standard_text_unit AS unit
  ON unit.owner = text_group.owner
 AND text_group.group_location = unit.group_location
WHERE text_group.owner = ?
ORDER BY text_group.group_order,
         unit.unit_order"#;

const TRANSLATION_OWNER_ORDER: [RpgMakerStandardAssetOwner; 3] = [
    RpgMakerStandardAssetOwner::Builtin,
    RpgMakerStandardAssetOwner::Rules,
    RpgMakerStandardAssetOwner::Lua,
];
const TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT: usize = 3 + TRANSLATION_OWNER_ORDER.len() * 2;

fn translation_snapshot_queries() -> Vec<SqliteQuery> {
    let mut queries = Vec::with_capacity(TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT);
    queries.extend([
        SqliteQuery::new(READ_TRANSLATION_METADATA, Vec::new()).with_id("translation.metadata"),
        SqliteQuery::new(READ_TRANSLATION_OWNERS, Vec::new()).with_id("translation.owners"),
        SqliteQuery::new(READ_TRANSLATION_RESOURCES, Vec::new()).with_id("translation.resources"),
    ]);
    for (kind, statement) in [
        ("groups", READ_TRANSLATION_OWNER_GROUPS),
        ("units", READ_TRANSLATION_OWNER_UNITS),
    ] {
        queries.extend(TRANSLATION_OWNER_ORDER.map(|owner| {
            SqliteQuery::new(
                statement,
                vec![SqliteValue::Text(owner.storage_name().to_owned())],
            )
            .with_id(format!("translation.{}.{kind}", owner.storage_name()))
        }));
    }
    queries
}

/// 验证 owner 新鲜度、读取当前资源，并用受控 CPU 解码标准翻译语料。
pub(crate) struct RpgMakerStandardTranslationAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
}

impl<Q, C> RpgMakerStandardTranslationAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C) -> Self {
        Self { sqlite, cpu }
    }
}

impl<Q, C> StandardTranslationAssetReader for RpgMakerStandardTranslationAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerStandardTranslationAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
    ) -> Result<StandardTranslationCorpus, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let query_results = self
            .sqlite
            .query_existing_database_snapshot(database_path.clone(), translation_snapshot_queries())
            .await
            .map_err(|error| map_query_error(database_path.clone(), error))?;
        let expected_source_snapshot = project.source_snapshot_fingerprint();
        let preparation_database_path = database_path.clone();
        let prepared = self
            .cpu
            .execute(move || prepare_snapshot(query_results, expected_source_snapshot))
            .await
            .map_err(
                |source| RpgMakerStandardTranslationAssetReadingError::SchedulePreparation {
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
            .execute_ordered_map(prepared.units, move |row| decode_unit(row, active_owners))
            .await
            .map_err(
                |source| RpgMakerStandardTranslationAssetReadingError::ScheduleDecode {
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
                |source| RpgMakerStandardTranslationAssetReadingError::ScheduleAssembly {
                    database_path: assembly_database_path,
                    source,
                },
            )?
            .map_err(
                |source| RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                    database_path,
                    source,
                },
            )?;
        Ok(StandardTranslationCorpus::with_snapshot(
            groups,
            prepared.source_snapshot_fingerprint,
            prepared.owner_snapshots,
            prepared.terminology_json,
            prepared.placeholder_rules_json,
        ))
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerStandardTranslationAssetReadingError<Q, C> {
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
        owners: Vec<RpgMakerStandardAssetOwner>,
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
        source: InvalidStandardTranslationAssetSnapshot,
    },
}

impl<Q: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerStandardTranslationAssetReadingError<Q, C>
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
                "无法从 {} 读取标准翻译资产：{source}",
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
                "{} 的标准资产提取已过期：{}",
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
                    "{} 的标准翻译资产损坏：{source}",
                    database_path.display()
                )
            }
        }
    }
}

impl<Q: Error + 'static, C: Error + 'static> Error
    for RpgMakerStandardTranslationAssetReadingError<Q, C>
{
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

impl<Q, C> SafeDiagnosticSource for RpgMakerStandardTranslationAssetReadingError<Q, C>
where
    Q: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
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
            Self::Query {
                database_path,
                source,
            } => diagnostic_at_database_path(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::CheckProjectState),
                database_path,
                "read_standard_translation_snapshot",
            ),
            Self::ProjectSnapshotChanged {
                database_path,
                expected,
                actual,
            } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::StateMismatch,
                    format!(
                        "expected_source_fingerprint={}; actual_source_fingerprint={}",
                        fingerprint_hex(expected.as_bytes()),
                        fingerprint_hex(actual.as_bytes())
                    ),
                ),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component(
                "metadata.source_snapshot_fingerprint",
            )),
            Self::ExtractionOutOfDate {
                database_path,
                owners,
            } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::StateMismatch,
                    format!(
                        "stale_standard_asset_owners={}",
                        owners
                            .iter()
                            .map(|owner| owner.storage_name())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                ),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component("rerun_extract")),
            Self::SchedulePreparation {
                database_path,
                source,
            } => diagnostic_at_database_path(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::Retry),
                database_path,
                "prepare_standard_translation_snapshot",
            ),
            Self::ScheduleDecode {
                database_path,
                source,
            } => diagnostic_at_database_path(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::Retry),
                database_path,
                "decode_standard_translation_units",
            ),
            Self::ScheduleAssembly {
                database_path,
                source,
            } => diagnostic_at_database_path(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::Retry),
                database_path,
                "assemble_standard_translation_corpus",
            ),
            Self::InvalidSnapshot {
                database_path,
                source,
            } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::path(database_path),
                source.safe_reason(),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component("standard_translation_snapshot")),
        }
    }
}

fn diagnostic_at_database_path(
    mut diagnostic: SafeDiagnostic,
    database_path: &std::path::Path,
    operation: &'static str,
) -> SafeDiagnostic {
    diagnostic.subject = DiagnosticSubject::path(database_path);
    diagnostic.with_recovery(RecoveryFact::component(operation))
}

fn fingerprint_hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("写入 String 不会失败");
    }
    value
}

#[derive(Debug)]
pub(crate) enum InvalidStandardTranslationAssetSnapshot {
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
    InvalidOrderValue {
        column: &'static str,
        actual: i64,
    },
    UnknownOwner(String),
    InactiveOwner(String),
    DuplicateOwner(String),
    InvalidOwnerSourceFingerprintLength {
        owner: String,
        actual: usize,
    },
    InvalidOwnerAssetFingerprintLength {
        owner: String,
        actual: usize,
    },
    InvalidMetadataRowCount {
        actual: usize,
    },
    InvalidMetadataFingerprintLength {
        actual: usize,
    },
    MissingTranslationResource(&'static str),
    DuplicateTranslationResource(String),
    UnknownTranslationResource(String),
    BlankTranslationResource(String),
    UnknownGroupKind(String),
    InvalidLocation(RpgMakerLocationCodecError),
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
    DuplicateGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    MissingGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    EmptyGroup {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InvalidGroupOrder {
        owner: RpgMakerStandardAssetOwner,
        expected: usize,
        actual: usize,
    },
    InconsistentGroupDefinition {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
    },
    InvalidUnitOrder {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
        expected: usize,
        actual: usize,
    },
    DuplicateLogicalUnit {
        owner: RpgMakerStandardAssetOwner,
        group_location: Box<RpgMakerLocation>,
        role: TextUnitRole,
    },
}

impl fmt::Display for InvalidStandardTranslationAssetSnapshot {
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
            Self::InvalidOrderValue { column, actual } => {
                write!(
                    formatter,
                    "列 {column} 必须是可表示的非负顺序，实际为 {actual}"
                )
            }
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::InactiveOwner(owner) => write!(formatter, "文本单元引用未激活 owner：{owner}"),
            Self::DuplicateOwner(owner) => write!(formatter, "资产 owner 状态重复：{owner}"),
            Self::InvalidOwnerSourceFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {owner} 的来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::InvalidOwnerAssetFingerprintLength { owner, actual } => write!(
                formatter,
                "owner {owner} 的资产指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::InvalidMetadataRowCount { actual } => {
                write!(formatter, "metadata 必须恰好一行，实际为 {actual} 行")
            }
            Self::InvalidMetadataFingerprintLength { actual } => write!(
                formatter,
                "metadata 来源指纹必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
            Self::MissingTranslationResource(kind) => write!(formatter, "缺少翻译资源 {kind}"),
            Self::DuplicateTranslationResource(kind) => write!(formatter, "翻译资源重复：{kind}"),
            Self::UnknownTranslationResource(kind) => write!(formatter, "未知翻译资源：{kind}"),
            Self::BlankTranslationResource(kind) => write!(formatter, "翻译资源 {kind} 为空"),
            Self::UnknownGroupKind(kind) => write!(formatter, "未知文本组类型：{kind}"),
            Self::InvalidLocation(source) => write!(formatter, "组位置无效：{source}"),
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
            Self::BlankSourceContent => formatter.write_str("标准文本源内容仅包含空白"),
            Self::BlankTranslationContent => formatter.write_str("标准文本译文内容仅包含空白"),
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
            Self::InvalidGroupOrder {
                owner,
                expected,
                actual,
            } => write!(
                formatter,
                "owner {} 的 group_order 必须从 0 连续：期待 {expected}，实际 {actual}",
                owner.storage_name()
            ),
            Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => write!(
                formatter,
                "同一资产组的类型或 group_order 不一致：{} / {group_location}",
                owner.storage_name()
            ),
            Self::InvalidUnitOrder {
                owner,
                group_location,
                expected,
                actual,
            } => write!(
                formatter,
                "组 {} / {group_location} 的 unit_order 必须从 0 连续：期待 {expected}，实际 {actual}",
                owner.storage_name()
            ),
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

impl Error for InvalidStandardTranslationAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation(source) => Some(source),
            Self::InvalidRole(source) => Some(source),
            Self::InvalidSourceContent(source)
            | Self::InvalidTranslationContent(source)
            | Self::InvalidSourceContext(source) => Some(source),
            _ => None,
        }
    }
}

impl InvalidStandardTranslationAssetSnapshot {
    fn safe_reason(&self) -> DiagnosticReason {
        let detail = match self {
            Self::WrongQueryResultSetCount { expected, actual } => {
                format!("wrong_query_result_set_count: expected={expected}; actual={actual}")
            }
            Self::WrongColumnCount { expected, actual } => {
                format!("wrong_column_count: expected={expected}; actual={actual}")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => {
                format!("wrong_column_type: column={column}; expected={expected}; actual={actual}")
            }
            Self::InvalidOrderValue { column, actual } => {
                format!("invalid_order_value: column={column}; actual={actual}")
            }
            Self::UnknownOwner(_) => "unknown_standard_asset_owner".to_owned(),
            Self::InactiveOwner(_) => "unit_references_inactive_standard_asset_owner".to_owned(),
            Self::DuplicateOwner(_) => "duplicate_standard_asset_owner_state".to_owned(),
            Self::InvalidOwnerSourceFingerprintLength { actual, .. } => {
                format!("invalid_owner_source_fingerprint_length: expected=32; actual={actual}")
            }
            Self::InvalidOwnerAssetFingerprintLength { actual, .. } => {
                format!("invalid_owner_asset_fingerprint_length: expected=32; actual={actual}")
            }
            Self::InvalidMetadataRowCount { actual } => {
                format!("invalid_metadata_row_count: expected=1; actual={actual}")
            }
            Self::InvalidMetadataFingerprintLength { actual } => {
                format!("invalid_metadata_source_fingerprint_length: expected=32; actual={actual}")
            }
            Self::MissingTranslationResource(kind) => {
                format!("missing_translation_resource: kind={kind}")
            }
            Self::DuplicateTranslationResource(_) => "duplicate_translation_resource".to_owned(),
            Self::UnknownTranslationResource(_) => "unknown_translation_resource".to_owned(),
            Self::BlankTranslationResource(_) => "blank_translation_resource".to_owned(),
            Self::UnknownGroupKind(_) => "unknown_text_group_kind".to_owned(),
            Self::InvalidLocation(source) => {
                format!("invalid_group_location: {}", location_codec_detail(source))
            }
            Self::InvalidRole(source) => {
                format!("invalid_unit_role: {}", projection_codec_detail(source))
            }
            Self::RoleDoesNotBelongToGroup { role, kind } => format!(
                "role_does_not_belong_to_group: role={}; group_kind={}",
                unit_role_kind(role),
                text_group_kind(*kind)
            ),
            Self::InvalidSourceContent(source) => {
                format!("invalid_source_content_json: {}", json_error_detail(source))
            }
            Self::InvalidTranslationContent(source) => format!(
                "invalid_translation_content_json: {}",
                json_error_detail(source)
            ),
            Self::SourceContentShapeMismatch { role } => format!(
                "source_content_shape_mismatch: role={}",
                unit_role_kind(role)
            ),
            Self::TranslationContentShapeMismatch { role } => format!(
                "translation_content_shape_mismatch: role={}",
                unit_role_kind(role)
            ),
            Self::BlankSourceContent => "blank_source_content".to_owned(),
            Self::BlankTranslationContent => "blank_translation_content".to_owned(),
            Self::InvalidSourceLineText { index } => {
                format!("invalid_source_line_text: line_index={index}")
            }
            Self::InvalidTranslationLineText { index } => {
                format!("invalid_translation_line_text: line_index={index}")
            }
            Self::AlignedLineCountMismatch { expected, actual } => {
                format!("aligned_line_count_mismatch: expected={expected}; actual={actual}")
            }
            Self::AlignedBlankSlotMismatch { index } => {
                format!("aligned_blank_slot_mismatch: line_index={index}")
            }
            Self::InvalidSourceContext(source) => {
                format!("invalid_source_context_json: {}", json_error_detail(source))
            }
            Self::SourceContextMustBeObject => "source_context_must_be_object".to_owned(),
            Self::InvalidTranslationStatePair => {
                "translation_content_and_state_presence_mismatch".to_owned()
            }
            Self::InvalidTranslationStateLength { actual } => {
                format!("invalid_translation_state_length: expected=32; actual={actual}")
            }
            Self::DuplicateGroup {
                owner,
                group_location,
            } => format!(
                "duplicate_standard_text_group: owner={}; group_location={group_location}",
                owner.storage_name()
            ),
            Self::MissingGroup {
                owner,
                group_location,
            } => format!(
                "standard_text_unit_missing_group: owner={}; group_location={group_location}",
                owner.storage_name()
            ),
            Self::EmptyGroup {
                owner,
                group_location,
            } => format!(
                "standard_text_group_is_empty: owner={}; group_location={group_location}",
                owner.storage_name()
            ),
            Self::InvalidGroupOrder {
                owner,
                expected,
                actual,
            } => format!(
                "invalid_group_order: owner={}; expected={expected}; actual={actual}",
                owner.storage_name()
            ),
            Self::InconsistentGroupDefinition {
                owner,
                group_location,
            } => format!(
                "inconsistent_group_definition: owner={}; group_location={group_location}",
                owner.storage_name()
            ),
            Self::InvalidUnitOrder {
                owner,
                group_location,
                expected,
                actual,
            } => format!(
                "invalid_unit_order: owner={}; group_location={group_location}; expected={expected}; actual={actual}",
                owner.storage_name()
            ),
            Self::DuplicateLogicalUnit {
                owner,
                group_location,
                role,
            } => format!(
                "duplicate_logical_unit: owner={}; group_location={group_location}; role={}",
                owner.storage_name(),
                unit_role_kind(role)
            ),
        };
        DiagnosticReason::failure_with_detail(DiagnosticFailureKind::StateMismatch, detail)
    }
}

fn json_error_detail(source: &serde_json::Error) -> String {
    let category = match source.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    format!(
        "category={category}; line={}; column={}",
        source.line(),
        source.column()
    )
}

fn location_codec_detail(source: &RpgMakerLocationCodecError) -> String {
    source.safe_diagnostic_detail()
}

fn projection_codec_detail(source: &RpgMakerProjectionCodecError) -> String {
    source.safe_diagnostic_detail()
}

const fn unit_role_kind(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::Scalar(_) => "scalar",
        TextUnitRole::DialogueSpeaker => "dialogue_speaker",
        TextUnitRole::DialogueBody => "dialogue_body",
        TextUnitRole::Choices => "choices",
        TextUnitRole::ScrollingText => "scrolling_text",
    }
}

const fn text_group_kind(kind: TextGroupKind) -> &'static str {
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

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> RpgMakerStandardTranslationAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            RpgMakerStandardTranslationAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            RpgMakerStandardTranslationAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

struct OwnerSqliteRow {
    owner: RpgMakerStandardAssetOwner,
    row: SqliteRow,
}

fn merge_owner_partitions(partitions: [Vec<SqliteRow>; 3]) -> Vec<OwnerSqliteRow> {
    let capacity = partitions.iter().map(Vec::len).sum();
    let mut merged = Vec::with_capacity(capacity);
    for (owner, partition) in TRANSLATION_OWNER_ORDER.into_iter().zip(partitions) {
        merged.extend(
            partition
                .into_iter()
                .map(|row| OwnerSqliteRow { owner, row }),
        );
    }
    merged
}

#[derive(Clone, Copy, Default)]
struct ActiveOwners([bool; TRANSLATION_OWNER_ORDER.len()]);

impl ActiveOwners {
    fn insert(&mut self, owner: RpgMakerStandardAssetOwner) -> bool {
        let active = &mut self.0[owner_order(owner)];
        let inserted = !*active;
        *active = true;
        inserted
    }

    fn contains(self, owner: RpgMakerStandardAssetOwner) -> bool {
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
    Invalid(InvalidStandardTranslationAssetSnapshot),
    WrongQueryResultSetCount {
        actual: usize,
    },
    ProjectSnapshotChanged {
        expected: SourceSnapshotFingerprint,
        actual: SourceSnapshotFingerprint,
    },
    ExtractionOutOfDate {
        owners: Vec<RpgMakerStandardAssetOwner>,
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
        lua_groups,
        builtin_units,
        rules_units,
        lua_units,
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
        merge_owner_partitions([builtin_groups, rules_groups, lua_groups]),
        owner_states.active,
    )
    .map_err(SnapshotPreparationError::Invalid)?;
    let units = merge_owner_partitions([builtin_units, rules_units, lua_units]);

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
) -> RpgMakerStandardTranslationAssetReadingError<Q, C> {
    match error {
        SnapshotPreparationError::Invalid(source) => {
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source,
            }
        }
        SnapshotPreparationError::WrongQueryResultSetCount { actual } => {
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source: InvalidStandardTranslationAssetSnapshot::WrongQueryResultSetCount {
                    expected: TRANSLATION_SNAPSHOT_QUERY_RESULT_COUNT,
                    actual,
                },
            }
        }
        SnapshotPreparationError::ProjectSnapshotChanged { expected, actual } => {
            RpgMakerStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                database_path,
                expected,
                actual,
            }
        }
        SnapshotPreparationError::ExtractionOutOfDate { owners } => {
            RpgMakerStandardTranslationAssetReadingError::ExtractionOutOfDate {
                database_path,
                owners,
            }
        }
    }
}

fn decode_metadata(
    rows: Vec<SqliteRow>,
) -> Result<SourceSnapshotFingerprint, InvalidStandardTranslationAssetSnapshot> {
    if rows.len() != 1 {
        return Err(
            InvalidStandardTranslationAssetSnapshot::InvalidMetadataRowCount { actual: rows.len() },
        );
    }
    let mut values = rows.into_iter().next().expect("已确认有一行").into_values();
    if values.len() != 1 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 1,
            actual: values.len(),
        });
    }
    let value = values.pop().expect("已确认有一列");
    let SqliteValue::Blob(bytes) = value else {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column: "metadata.source_snapshot_fingerprint",
            expected: "BLOB",
            actual: value.kind_name(),
        });
    };
    SourceSnapshotFingerprint::from_slice(&bytes).map_err(|error| {
        InvalidStandardTranslationAssetSnapshot::InvalidMetadataFingerprintLength {
            actual: error.actual(),
        }
    })
}

struct DecodedOwnerStates {
    stale: Vec<RpgMakerStandardAssetOwner>,
    active: ActiveOwners,
    snapshots: Vec<TranslationOwnerSnapshot>,
}

fn decode_owner_states(
    mut rows: Vec<SqliteRow>,
    current: SourceSnapshotFingerprint,
) -> Result<DecodedOwnerStates, InvalidStandardTranslationAssetSnapshot> {
    rows.sort_by_key(owner_row_order);
    let mut active = ActiveOwners::default();
    let mut stale = Vec::new();
    let mut snapshots = Vec::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 3 {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 3,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let owner_name = required_text(next(&mut values), "owner")?;
        let owner =
            RpgMakerStandardAssetOwner::from_storage_name(&owner_name).ok_or_else(|| {
                InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner_name.clone())
            })?;
        if !active.insert(owner) {
            return Err(InvalidStandardTranslationAssetSnapshot::DuplicateOwner(
                owner_name,
            ));
        }
        let source_bytes = required_blob(next(&mut values), "source_snapshot_fingerprint")?;
        let source = SourceSnapshotFingerprint::from_slice(&source_bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidOwnerSourceFingerprintLength {
                owner: owner.storage_name().to_owned(),
                actual: error.actual(),
            }
        })?;
        let asset_bytes = required_blob(next(&mut values), "asset_snapshot_fingerprint")?;
        let asset = AssetSnapshotFingerprint::from_slice(&asset_bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidOwnerAssetFingerprintLength {
                owner: owner.storage_name().to_owned(),
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

fn owner_row_order(row: &SqliteRow) -> usize {
    row.values()
        .first()
        .and_then(|value| match value {
            SqliteValue::Text(owner) => RpgMakerStandardAssetOwner::from_storage_name(owner),
            _ => None,
        })
        .map_or(TRANSLATION_OWNER_ORDER.len(), owner_order)
}

const fn owner_order(owner: RpgMakerStandardAssetOwner) -> usize {
    match owner {
        RpgMakerStandardAssetOwner::Builtin => 0,
        RpgMakerStandardAssetOwner::Rules => 1,
        RpgMakerStandardAssetOwner::Lua => 2,
    }
}

fn decode_resources(
    rows: Vec<SqliteRow>,
) -> Result<(String, String), InvalidStandardTranslationAssetSnapshot> {
    let mut terminology = None;
    let mut placeholders = None;
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                expected: 2,
                actual: values.len(),
            });
        }
        let mut values = values.into_iter();
        let kind = required_text(next(&mut values), "resource_kind")?;
        let resource = match kind.as_str() {
            TERMINOLOGY_RESOURCE_KIND => &mut terminology,
            PLACEHOLDER_RULES_RESOURCE_KIND => &mut placeholders,
            _ => {
                return Err(
                    InvalidStandardTranslationAssetSnapshot::UnknownTranslationResource(kind),
                );
            }
        };
        let canonical_json = required_text(next(&mut values), "canonical_json")?;
        if canonical_json.is_empty() {
            return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslationResource(kind));
        }
        if resource.is_some() {
            return Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateTranslationResource(kind),
            );
        }
        *resource = Some(canonical_json);
    }
    let terminology = terminology.ok_or(
        InvalidStandardTranslationAssetSnapshot::MissingTranslationResource(
            TERMINOLOGY_RESOURCE_KIND,
        ),
    )?;
    let placeholders = placeholders.ok_or(
        InvalidStandardTranslationAssetSnapshot::MissingTranslationResource(
            PLACEHOLDER_RULES_RESOURCE_KIND,
        ),
    )?;
    Ok((terminology, placeholders))
}

#[derive(Debug)]
struct DecodedGroup {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    group_order: usize,
}

fn decode_groups(
    rows: Vec<OwnerSqliteRow>,
    active_owners: ActiveOwners,
) -> Result<Vec<DecodedGroup>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter()
        .map(|OwnerSqliteRow { owner, row }| {
            let values = row.into_values();
            if values.len() != 3 {
                return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
                    expected: 3,
                    actual: values.len(),
                });
            }
            let mut values = values.into_iter();
            if !active_owners.contains(owner) {
                return Err(InvalidStandardTranslationAssetSnapshot::InactiveOwner(
                    owner.storage_name().to_owned(),
                ));
            }
            let group_location_raw = required_text(next(&mut values), "group_location")?;
            let group_location = RpgMakerLocationCodec::decode(&group_location_raw)
                .map_err(InvalidStandardTranslationAssetSnapshot::InvalidLocation)?;
            let kind = decode_group_kind(&required_text(next(&mut values), "group_kind")?)?;
            let group_order = required_non_negative_order(next(&mut values), "group_order")?;
            Ok(DecodedGroup {
                owner,
                kind,
                group_location,
                group_order,
            })
        })
        .collect()
}

#[derive(Debug)]
struct DecodedUnit {
    owner: RpgMakerStandardAssetOwner,
    kind: TextGroupKind,
    group_location: RpgMakerLocation,
    group_order: usize,
    role: TextUnitRole,
    unit_order: usize,
    source_content: TextUnitContent,
    source_context_json: String,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
}

fn decode_unit(
    OwnerSqliteRow { owner, row }: OwnerSqliteRow,
    active_owners: ActiveOwners,
) -> Result<DecodedUnit, InvalidStandardTranslationAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 9 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
            expected: 9,
            actual: values.len(),
        });
    }
    let mut values = values.into_iter();
    if !active_owners.contains(owner) {
        return Err(InvalidStandardTranslationAssetSnapshot::InactiveOwner(
            owner.storage_name().to_owned(),
        ));
    }
    let group_location_raw = required_text(next(&mut values), "group_location")?;
    let group_location = RpgMakerLocationCodec::decode(&group_location_raw)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidLocation)?;
    let kind = decode_group_kind(&required_text(next(&mut values), "group_kind")?)?;
    let group_order = required_non_negative_order(next(&mut values), "group_order")?;
    let role_raw = required_text(next(&mut values), "unit_role")?;
    let role = RpgMakerProjectionCodec::decode_role(&role_raw)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidRole)?;
    let unit_order = required_non_negative_order(next(&mut values), "unit_order")?;
    validate_role(&role, kind)?;
    let source_content_json = required_text(next(&mut values), "source_content_json")?;
    let source_content: TextUnitContent = serde_json::from_str(&source_content_json)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidSourceContent)?;
    if source_content.is_blank() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankSourceContent);
    }
    if role.expects_lines() != source_content.as_lines().is_some() {
        return Err(
            InvalidStandardTranslationAssetSnapshot::SourceContentShapeMismatch {
                role: role.clone(),
            },
        );
    }
    let source_context_json = required_text(next(&mut values), "source_context_json")?;
    let context: serde_json::Value = serde_json::from_str(&source_context_json)
        .map_err(InvalidStandardTranslationAssetSnapshot::InvalidSourceContext)?;
    if !context.is_object() {
        return Err(InvalidStandardTranslationAssetSnapshot::SourceContextMustBeObject);
    }
    let translation_content_json = optional_text(next(&mut values), "translation_content_json")?;
    let translation = translation_content_json
        .map(|translation| {
            serde_json::from_str::<TextUnitContent>(&translation)
                .map_err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationContent)
        })
        .transpose()?;
    if translation.as_ref().is_some_and(TextUnitContent::is_blank) {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslationContent);
    }
    if translation
        .as_ref()
        .is_some_and(|translation| translation.as_lines().is_some() != role.expects_lines())
    {
        return Err(
            InvalidStandardTranslationAssetSnapshot::TranslationContentShapeMismatch {
                role: role.clone(),
            },
        );
    }
    validate_persisted_content(&role, &source_content, translation.as_ref())?;
    let translation_state = optional_blob(next(&mut values), "translation_state")?;
    let translation_state = match (translation.as_ref(), translation_state) {
        (None, None) => None,
        (Some(_), Some(bytes)) => Some(Sha256Fingerprint::from_slice(&bytes).map_err(|error| {
            InvalidStandardTranslationAssetSnapshot::InvalidTranslationStateLength {
                actual: error.actual(),
            }
        })?),
        _ => return Err(InvalidStandardTranslationAssetSnapshot::InvalidTranslationStatePair),
    };
    Ok(DecodedUnit {
        owner,
        kind,
        group_location,
        group_order,
        role,
        unit_order,
        source_content,
        source_context_json,
        translation,
        translation_state,
    })
}

fn validate_persisted_content(
    role: &TextUnitRole,
    source: &TextUnitContent,
    translation: Option<&TextUnitContent>,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    if let Some(lines) = source.as_lines() {
        if let Some(index) = lines.iter().position(|line| contains_line_separator(line)) {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidSourceLineText { index });
        }
    } else if matches!(role, TextUnitRole::DialogueSpeaker)
        && source.as_value().is_some_and(contains_line_separator)
    {
        return Err(InvalidStandardTranslationAssetSnapshot::InvalidSourceLineText { index: 0 });
    }

    let Some(translation) = translation else {
        return Ok(());
    };
    if let Some(lines) = translation.as_lines() {
        if let Some(index) = lines.iter().position(|line| contains_line_separator(line)) {
            return Err(
                InvalidStandardTranslationAssetSnapshot::InvalidTranslationLineText { index },
            );
        }
    } else if matches!(role, TextUnitRole::DialogueSpeaker)
        && translation.as_value().is_some_and(contains_line_separator)
    {
        return Err(
            InvalidStandardTranslationAssetSnapshot::InvalidTranslationLineText { index: 0 },
        );
    }

    if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
        let source_lines = source.as_lines().expect("严格对齐角色的源内容形状已验证");
        let translation_lines = translation
            .as_lines()
            .expect("严格对齐角色的译文内容形状已验证");
        if source_lines.len() != translation_lines.len() {
            return Err(
                InvalidStandardTranslationAssetSnapshot::AlignedLineCountMismatch {
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
                    source.trim().is_empty() != translation.trim().is_empty()
                })
        {
            return Err(
                InvalidStandardTranslationAssetSnapshot::AlignedBlankSlotMismatch { index },
            );
        }
    }
    Ok(())
}

fn contains_line_separator(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
}

fn decode_group_kind(
    value: &str,
) -> Result<TextGroupKind, InvalidStandardTranslationAssetSnapshot> {
    match value {
        "database_entry" => Ok(TextGroupKind::DatabaseEntry),
        "system" => Ok(TextGroupKind::System),
        "map" => Ok(TextGroupKind::Map),
        "event_dialogue" => Ok(TextGroupKind::EventDialogue),
        "event_choices" => Ok(TextGroupKind::EventChoices),
        "event_scrolling_text" => Ok(TextGroupKind::EventScrollingText),
        "event_command" => Ok(TextGroupKind::EventCommand),
        "plugin_parameter" => Ok(TextGroupKind::PluginParameter),
        unknown => Err(InvalidStandardTranslationAssetSnapshot::UnknownGroupKind(
            unknown.to_owned(),
        )),
    }
}

fn validate_role(
    role: &TextUnitRole,
    kind: TextGroupKind,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    if role.matches_kind(kind) {
        Ok(())
    } else {
        Err(
            InvalidStandardTranslationAssetSnapshot::RoleDoesNotBelongToGroup {
                role: role.clone(),
                kind,
            },
        )
    }
}

fn assemble_corpus(
    group_rows: Vec<DecodedGroup>,
    units: Vec<DecodedUnit>,
) -> Result<Vec<StandardTranslationGroup>, InvalidStandardTranslationAssetSnapshot> {
    struct GroupBuilder {
        owner: RpgMakerStandardAssetOwner,
        kind: TextGroupKind,
        group_location: RpgMakerLocation,
        group_order: usize,
        assets: Vec<StandardTranslationAsset>,
    }

    let mut next_group_orders = [0_usize; TRANSLATION_OWNER_ORDER.len()];
    let mut group_locations: [HashSet<RpgMakerLocation>; TRANSLATION_OWNER_ORDER.len()] =
        std::array::from_fn(|_| HashSet::new());
    let mut groups = Vec::<GroupBuilder>::with_capacity(group_rows.len());
    for group in group_rows {
        let owner_index = owner_order(group.owner);
        let expected = next_group_orders[owner_index];
        if group.group_order != expected {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidGroupOrder {
                owner: group.owner,
                expected,
                actual: group.group_order,
            });
        }
        next_group_orders[owner_index] += 1;
        if !group_locations[owner_index].insert(group.group_location.clone()) {
            return Err(InvalidStandardTranslationAssetSnapshot::DuplicateGroup {
                owner: group.owner,
                group_location: Box::new(group.group_location),
            });
        }
        groups.push(GroupBuilder {
            owner: group.owner,
            kind: group.kind,
            group_location: group.group_location,
            group_order: group.group_order,
            assets: Vec::new(),
        });
    }

    let group_starts = [
        0,
        next_group_orders[0],
        next_group_orders[0] + next_group_orders[1],
    ];
    let mut seen_logical_units = HashSet::with_capacity(units.len());
    for unit in units {
        let owner_index = owner_order(unit.owner);
        if !group_locations[owner_index].contains(&unit.group_location) {
            return Err(InvalidStandardTranslationAssetSnapshot::MissingGroup {
                owner: unit.owner,
                group_location: Box::new(unit.group_location),
            });
        }
        let group_index = group_starts[owner_index].checked_add(unit.group_order);
        let Some((group_index, group)) =
            group_index.and_then(|index| groups.get_mut(index).map(|group| (index, group)))
        else {
            return Err(
                InvalidStandardTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                },
            );
        };
        if group.owner != unit.owner
            || group.kind != unit.kind
            || group.group_location != unit.group_location
            || group.group_order != unit.group_order
        {
            return Err(
                InvalidStandardTranslationAssetSnapshot::InconsistentGroupDefinition {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                },
            );
        }
        let expected_unit_order = group.assets.len();
        if unit.unit_order != expected_unit_order {
            return Err(InvalidStandardTranslationAssetSnapshot::InvalidUnitOrder {
                owner: unit.owner,
                group_location: Box::new(unit.group_location),
                expected: expected_unit_order,
                actual: unit.unit_order,
            });
        }
        if !seen_logical_units.insert((group_index, unit.role.clone())) {
            return Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateLogicalUnit {
                    owner: unit.owner,
                    group_location: Box::new(unit.group_location),
                    role: unit.role,
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
        group.assets.push(StandardTranslationAsset::new(
            identity,
            unit.translation,
            unit.translation_state,
        ));
    }

    if let Some(group) = groups.iter().find(|group| group.assets.is_empty()) {
        return Err(InvalidStandardTranslationAssetSnapshot::EmptyGroup {
            owner: group.owner,
            group_location: Box::new(group.group_location.clone()),
        });
    }
    Ok(groups
        .into_iter()
        .map(|group| StandardTranslationGroup::new(group.kind, group.group_location, group.assets))
        .collect())
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，标准文本查询行必须具有完整投影")
}

fn required_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: actual.kind_name(),
        }),
    }
}

fn required_blob(
    value: SqliteValue,
    column: &'static str,
) -> Result<Vec<u8>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Blob(value) => Ok(value),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "BLOB",
            actual: actual.kind_name(),
        }),
    }
}

fn required_non_negative_order(
    value: SqliteValue,
    column: &'static str,
) -> Result<usize, InvalidStandardTranslationAssetSnapshot> {
    let SqliteValue::Integer(value) = value else {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "INTEGER",
            actual: value.kind_name(),
        });
    };
    usize::try_from(value).map_err(
        |_| InvalidStandardTranslationAssetSnapshot::InvalidOrderValue {
            column,
            actual: value,
        },
    )
}

fn optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_blob(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<Vec<u8>>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Blob(value) => Ok(Some(value)),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "BLOB 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::text::{RpgMakerLocationStep, RpgMakerSource};
    use crate::runtime::cpu::CpuExecutorUnavailable;
    use crate::runtime::sqlite::SqliteRuntimeError;
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
        RpgMakerStandardTranslationAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable>;

    #[test]
    fn asset_reading_diagnostic_preserves_database_path_and_fingerprint_facts() {
        let error: ProductionAssetReadingError =
            RpgMakerStandardTranslationAssetReadingError::ProjectSnapshotChanged {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                expected: SourceSnapshotFingerprint::from_bytes([0x11; 32]),
                actual: SourceSnapshotFingerprint::from_bytes([0x22; 32]),
            };

        let diagnostic = error.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );

        assert!(matches!(
            diagnostic.subject,
            DiagnosticSubject::Path { ref path } if path.ends_with("project.db")
        ));
        let DiagnosticReason::FailureWithDetail { failure, detail } = diagnostic.reason else {
            panic!("指纹变化必须保留结构化详情");
        };
        assert_eq!(failure, DiagnosticFailureKind::StateMismatch);
        assert!(detail.contains(&"11".repeat(32)));
        assert!(detail.contains(&"22".repeat(32)));
    }

    #[test]
    fn asset_reading_diagnostic_distinguishes_cancelled_cpu_wait_and_hides_raw_rows() {
        let cancelled: ProductionAssetReadingError =
            RpgMakerStandardTranslationAssetReadingError::ScheduleDecode {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                source: CpuTaskExecutionError::Cancelled,
            };
        let cancelled = cancelled.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );
        assert!(matches!(
            cancelled.reason,
            DiagnosticReason::Failure {
                failure: DiagnosticFailureKind::LockCancelled
            }
        ));
        assert!(matches!(cancelled.subject, DiagnosticSubject::Path { .. }));

        let sentinel = "RAW_DATABASE_ROW_SENTINEL";
        let invalid: ProductionAssetReadingError =
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                database_path: PathBuf::from("C:/projects/demo/project.db"),
                source: InvalidStandardTranslationAssetSnapshot::UnknownOwner(sentinel.to_owned()),
            };
        let invalid = invalid.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );
        let rendered = invalid.reason.render();
        assert!(rendered.contains("unknown_standard_asset_owner"));
        assert!(!rendered.contains(sentinel));
    }

    #[test]
    fn asset_snapshot_diagnostic_keeps_codec_structure_and_group_location() {
        let database_path = PathBuf::from("C:/projects/demo/project.db");
        let projection: ProductionAssetReadingError =
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                database_path: database_path.clone(),
                source: InvalidStandardTranslationAssetSnapshot::InvalidRole(
                    RpgMakerProjectionCodecError::Projection(
                        crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                            expected: 2,
                            actual: 4,
                        },
                    ),
                ),
            };
        let projection = projection.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );
        let projection = projection.reason.render();
        for fact in [
            "codec=projection",
            "structure=non_contiguous_dialogue_body_lines",
            "expected=2",
            "actual=4",
        ] {
            assert!(
                projection.contains(fact),
                "投影诊断缺少 {fact}: {projection}"
            );
        }

        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(3),
            vec![RpgMakerLocationStep::key("events")],
        );
        let group: ProductionAssetReadingError =
            RpgMakerStandardTranslationAssetReadingError::InvalidSnapshot {
                database_path,
                source: InvalidStandardTranslationAssetSnapshot::DuplicateLogicalUnit {
                    owner: RpgMakerStandardAssetOwner::Builtin,
                    group_location: Box::new(group_location),
                    role: TextUnitRole::DialogueBody,
                },
            };
        let group = group
            .safe_diagnostic_source(
                DiagnosticStage::Translate,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
            .reason
            .render();
        assert!(group.contains("group_location=data/Map003.json.events"));
        assert!(group.contains("role=dialogue_body"));
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
        let service = RpgMakerStandardTranslationAssetReadingService::new(
            FakeQuery {
                calls: Arc::clone(&calls),
                results: Arc::new(Mutex::new(Some(rows))),
            },
            InlineCpu,
        );

        let corpus = service.read(&project()).await.expect("统一表应可读取");

        assert_eq!(corpus.groups().len(), 1);
        let assets = corpus.groups()[0].assets();
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
                .any(|(_, query)| query.statement().contains("standard_text_unit"))
        );
    }

    #[test]
    fn corpus_keeps_builtin_rules_lua_owner_order_and_independent_same_location_groups() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let owners = [
            RpgMakerStandardAssetOwner::Builtin,
            RpgMakerStandardAssetOwner::Rules,
            RpgMakerStandardAssetOwner::Lua,
        ];
        let group_rows = owners
            .into_iter()
            .map(|owner| DecodedGroup {
                owner,
                kind: TextGroupKind::DatabaseEntry,
                group_location: group_location.clone(),
                group_order: 0,
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
                    group_order: 0,
                    role,
                    unit_order: 0,
                    source_content: TextUnitContent::Value(owner.storage_name().to_owned()),
                    source_context_json: "{}".to_owned(),
                    translation: None,
                    translation_state: None,
                }
            })
            .collect::<Vec<_>>();

        let groups = assemble_corpus(group_rows, units).expect("三 owner 语料应能组装");

        assert_eq!(groups.len(), 3, "同一逻辑位置不得跨 owner 合并");
        assert_eq!(
            groups
                .iter()
                .map(|group| group.assets()[0].identity().owner())
                .collect::<Vec<_>>(),
            owners,
            "Standard 总顺序必须固定为 Builtin、Rules、Lua"
        );
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
                CREATE TABLE standard_asset_owner_state (
                    owner TEXT NOT NULL PRIMARY KEY,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE standard_translation_resource (
                    resource_kind TEXT NOT NULL PRIMARY KEY,
                    canonical_json TEXT NOT NULL
                );
                CREATE TABLE standard_text_group (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    group_order INTEGER NOT NULL,
                    group_kind TEXT NOT NULL,
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
                    translation_state BLOB,
                    PRIMARY KEY (owner, group_location, unit_role),
                    UNIQUE (owner, group_location, unit_order)
                );

                INSERT INTO metadata VALUES (zeroblob(32));
                INSERT INTO standard_asset_owner_state VALUES
                    ('rules', zeroblob(32), zeroblob(32)),
                    ('lua', zeroblob(32), zeroblob(32)),
                    ('builtin', zeroblob(32), zeroblob(32));
                INSERT INTO standard_translation_resource VALUES
                    ('terminology', '[]'),
                    ('placeholder_rules', '[]');
                INSERT INTO standard_text_group VALUES
                    ('builtin', 'group-b', 1, 'map'),
                    ('builtin', 'group-a', 0, 'map'),
                    ('rules', 'group-r', 0, 'map'),
                    ('lua', 'group-l', 0, 'map');
                INSERT INTO standard_text_unit VALUES
                    ('builtin', 'group-b', 'role-z', 0, '"z"', '{}', NULL, NULL),
                    ('builtin', 'group-a', 'role-y', 0, '"y"', '{}', NULL, NULL);
                "#,
            )
            .expect("测试快照表与行应可建立");

        let groups = connection
            .prepare(READ_TRANSLATION_OWNER_GROUPS)
            .expect("owner group 查询应可建立")
            .query_map(["builtin"], |row| row.get::<_, String>(0))
            .expect("owner group 查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("owner group 行应可读取");
        let units = connection
            .prepare(READ_TRANSLATION_OWNER_UNITS)
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
            ["builtin", "rules", "lua", "builtin", "rules", "lua"]
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
        }
    }

    #[test]
    fn owner_state_decode_restores_builtin_rules_lua_natural_order() {
        let current = SourceSnapshotFingerprint::from_bytes([0x31; 32]);
        let rows = [
            RpgMakerStandardAssetOwner::Builtin,
            RpgMakerStandardAssetOwner::Lua,
            RpgMakerStandardAssetOwner::Rules,
        ]
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
                owner: RpgMakerStandardAssetOwner::Builtin,
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
            InvalidStandardTranslationAssetSnapshot::SourceContextMustBeObject
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
                owner: RpgMakerStandardAssetOwner::Builtin,
                row: SqliteRow::new(values),
            },
            active_builtin(),
        )
        .expect_err("正文译文必须保持 Lines 形状");
        assert!(matches!(
            error,
            InvalidStandardTranslationAssetSnapshot::TranslationContentShapeMismatch {
                role: TextUnitRole::DialogueBody
            }
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
                SqliteValue::Integer(0),
            ])],
            Vec::new(),
            Vec::new(),
            units,
            Vec::new(),
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
            SqliteValue::Integer(0),
            text(role),
            SqliteValue::Integer(unit_order),
            text(source_content_json),
            text(context),
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
            SqliteValue::Integer(0),
            text(role),
            SqliteValue::Integer(0),
            text(source_content_json),
            text(context),
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
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn active_builtin() -> ActiveOwners {
        let mut owners = ActiveOwners::default();
        assert!(owners.insert(RpgMakerStandardAssetOwner::Builtin));
        owners
    }

    fn text(value: impl Into<String>) -> SqliteValue {
        SqliteValue::Text(value.into())
    }
}
