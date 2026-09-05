//! Generic 项目的失败事实、取消分类与安全诊断。

use super::transaction::GenericTransactionFinalizationFailure;
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage,
    FileSystemIssue, FileSystemOperation, FileSystemProblem, GenericDiagnosticStage, GenericIssue,
    GenericLanguageViolation, GenericProblem, GenericProjectDatabaseProblem,
    GenericProjectTranslationProblem, GenericResourceKind, IoFailure, RelatedFailureRelation,
    SafeIdentifier, SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure,
    SqliteIssue, SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect,
    TranslationIssue, TranslationPlanningResourceKind, TranslationPlanningResourceOrigin,
    TranslationPlanningResourceProblem,
};
use crate::generic::jsonl::GenericJsonlError;
use crate::generic::placeholder::GenericPlaceholderError;
use crate::generic::translate::GenericPlanningError;
use crate::language::LanguageIdError;
use crate::runtime::windows::WindowsFsError;
use crate::translation::layout_rules::LayoutRulesError;
use crate::translation::planning_resource::{
    TerminologyDefinitionError, terminology_problem, translation_json_failure,
};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{fmt, io};

#[derive(Debug)]
pub(crate) enum GenericProjectResourceError {
    InvalidSnapshot {
        resource: GenericResourceKind,
        source: serde_json::Error,
    },
    SnapshotEncoding {
        resource: GenericResourceKind,
        source: serde_json::Error,
    },
    TerminologyDefinition(TerminologyDefinitionError),
    Placeholder(GenericPlaceholderError),
    NonCanonicalSnapshot {
        resource: GenericResourceKind,
    },
}

impl fmt::Display for GenericProjectResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshot { resource, source } => {
                write!(
                    formatter,
                    "{resource:?} 资源快照不是现行规范 JSON：{source}"
                )
            }
            Self::SnapshotEncoding { resource, source } => {
                write!(formatter, "{resource:?} 资源快照无法编码：{source}")
            }
            Self::TerminologyDefinition(source) => source.fmt(formatter),
            Self::Placeholder(source) => source.fmt(formatter),
            Self::NonCanonicalSnapshot { resource } => {
                write!(formatter, "{resource:?} 资源快照不是规范紧凑 JSON")
            }
        }
    }
}

impl Error for GenericProjectResourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSnapshot { source, .. } | Self::SnapshotEncoding { source, .. } => {
                Some(source)
            }
            Self::TerminologyDefinition(source) => Some(source),
            Self::Placeholder(source) => Some(source),
            Self::NonCanonicalSnapshot { .. } => None,
        }
    }
}

/// Generic 项目行为失败。
#[derive(Debug)]
pub(crate) enum GenericProjectError {
    Cancelled,
    MissingInitialField(&'static str),
    WorkspaceNotDirectory {
        path: PathBuf,
    },
    SourceNotDirectory {
        path: PathBuf,
    },
    SourceWriteBackOverlap {
        source_root: PathBuf,
        write_back_root: PathBuf,
    },
    ProjectNotFound {
        path: PathBuf,
    },
    ProjectIdentityMismatch {
        expected: String,
        observed: String,
    },
    Io {
        operation: FileSystemOperation,
        path: PathBuf,
        source: io::Error,
    },
    InitialDatabaseFileSystem {
        operation: FileSystemOperation,
        source: WindowsFsError,
    },
    InitialDatabaseOutcomeUnknown(Box<GenericProjectError>),
    Sqlite {
        operation: &'static str,
        source: rusqlite::Error,
    },
    TransactionNotCommitted {
        operation: &'static str,
        source: rusqlite::Error,
    },
    TransactionOutcomeUnknown {
        operation: &'static str,
        primary: Option<Box<GenericProjectError>>,
        finalization: GenericTransactionFinalizationFailure,
    },
    InitialCandidateCleanup {
        original: Box<GenericProjectError>,
        cleanup: Vec<GenericProjectError>,
    },
    InvalidDatabase {
        problem: GenericProjectDatabaseProblem,
        source: Option<Box<GenericPlanningError>>,
    },
    InvalidLanguage(LanguageIdError),
    SameSourceAndTargetLanguage {
        language: String,
    },
    Jsonl(GenericJsonlError),
    InputChangedDuringExtract,
    ExtractRequired,
    TranslationSnapshotChanged,
    InvalidTranslation {
        group_id: Option<String>,
        unit_id: Option<String>,
        problem: GenericProjectTranslationProblem,
        source: Option<Box<GenericPlaceholderError>>,
    },
    DuplicateTranslationWrite {
        group_id: String,
        unit_id: String,
    },
    DuplicateTranslationClear {
        group_id: String,
        unit_id: String,
    },
    BlankProfileId,
    InvalidResource(GenericProjectResourceError),
    InvalidLayoutRules(LayoutRulesError),
}

impl fmt::Display for GenericProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 项目操作已取消"),
            Self::MissingInitialField(field) => {
                write!(formatter, "首次 Generic Init 必须提供 --{field}")
            }
            Self::WorkspaceNotDirectory { path } => {
                write!(formatter, "Generic 工作区路径不是目录：{}", path.display())
            }
            Self::SourceNotDirectory { path } => {
                write!(
                    formatter,
                    "Generic 输入路径不是现存目录：{}",
                    path.display()
                )
            }
            Self::SourceWriteBackOverlap {
                source_root,
                write_back_root,
            } => write!(
                formatter,
                "Generic 输入目录与写回目录不能相同或互为祖先：输入={}，写回={}",
                source_root.display(),
                write_back_root.display()
            ),
            Self::ProjectNotFound { path } => {
                write!(formatter, "Generic 项目不存在：{}", path.display())
            }
            Self::ProjectIdentityMismatch { expected, observed } => write!(
                formatter,
                "Generic 项目数据库属于 {expected:?}，不能作为 {observed:?} 打开"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{}失败：{}（{source}）",
                operation.as_str(),
                path.display()
            ),
            Self::Sqlite { operation, source } => write!(formatter, "{operation}失败：{source}"),
            Self::InitialDatabaseFileSystem { source, .. } => source.fmt(formatter),
            Self::InitialDatabaseOutcomeUnknown(source) => {
                write!(formatter, "初始数据库结果未知：{source}")
            }
            Self::TransactionNotCommitted { operation, source } => write!(
                formatter,
                "{operation}失败，事务已确认回滚且未提交：{source}"
            ),
            Self::TransactionOutcomeUnknown {
                operation,
                primary,
                finalization,
            } => {
                write!(formatter, "{operation}后事务结果未知")?;
                if let Some(primary) = primary {
                    write!(formatter, "；主失败：{primary}")?;
                }
                write!(formatter, "；终态确认失败：{finalization}")
            }
            Self::InitialCandidateCleanup { original, cleanup } => {
                write!(formatter, "{original}")?;
                for source in cleanup {
                    write!(formatter, "；清理初始数据库候选失败：{source}")?;
                }
                Ok(())
            }
            Self::InvalidDatabase { problem, .. } => {
                write!(formatter, "Generic 项目数据库无效：{problem:?}")
            }
            Self::InvalidLanguage(source) => write!(formatter, "Generic 项目语言无效：{source}"),
            Self::SameSourceAndTargetLanguage { language } => {
                write!(formatter, "Generic 源语言与目标语言不能相同：{language}")
            }
            Self::Jsonl(source) => source.fmt(formatter),
            Self::InputChangedDuringExtract => {
                formatter.write_str("Generic 输入在 Extract 期间发生变化，数据库未提交")
            }
            Self::ExtractRequired => {
                formatter.write_str("Generic 输入已变化或尚未提取，请先运行 Extract")
            }
            Self::TranslationSnapshotChanged => {
                formatter.write_str("Generic 翻译依据的 Extract 快照已经变化")
            }
            Self::InvalidTranslation {
                problem, source, ..
            } => {
                write!(formatter, "Generic 译文无效：{problem:?}")?;
                if let Some(source) = source {
                    write!(formatter, "（{source}）")?;
                }
                Ok(())
            }
            Self::DuplicateTranslationWrite { group_id, unit_id } => write!(
                formatter,
                "同一批次重复提交 Generic Unit：{group_id:?}/{unit_id:?}"
            ),
            Self::DuplicateTranslationClear { group_id, unit_id } => write!(
                formatter,
                "同一批次重复清除 Generic Unit：{group_id:?}/{unit_id:?}"
            ),
            Self::BlankProfileId => formatter.write_str("Generic Profile ID 不能为空白"),
            Self::InvalidResource(source) => write!(formatter, "Generic 翻译资源无效：{source}"),
            Self::InvalidLayoutRules(source) => {
                write!(formatter, "Generic WriteBack 排版规则无效：{source}")
            }
        }
    }
}

impl Error for GenericProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InitialDatabaseFileSystem { source, .. } => Some(source),
            Self::InitialDatabaseOutcomeUnknown(source) => Some(source.as_ref()),
            Self::Sqlite { source, .. } | Self::TransactionNotCommitted { source, .. } => {
                Some(source)
            }
            Self::TransactionOutcomeUnknown {
                primary: Some(primary),
                ..
            } => Some(primary.as_ref()),
            Self::TransactionOutcomeUnknown {
                primary: None,
                finalization,
                ..
            } => finalization.source(),
            Self::InitialCandidateCleanup { original, .. } => Some(original.as_ref()),
            Self::InvalidLanguage(source) => Some(source),
            Self::Jsonl(source) => Some(source),
            Self::InvalidTranslation {
                source: Some(source),
                ..
            } => Some(source.as_ref()),
            Self::InvalidResource(source) => Some(source),
            Self::InvalidLayoutRules(source) => Some(source),
            Self::InvalidDatabase {
                source: Some(source),
                ..
            } => Some(source.as_ref()),
            Self::Cancelled
            | Self::MissingInitialField(_)
            | Self::WorkspaceNotDirectory { .. }
            | Self::SourceNotDirectory { .. }
            | Self::SourceWriteBackOverlap { .. }
            | Self::ProjectNotFound { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::InvalidDatabase { source: None, .. }
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::InvalidTranslation { source: None, .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::BlankProfileId => None,
        }
    }
}

impl GenericProjectError {
    /// 在 Generic 项目边界仍掌握数据库路径、查询 ID、事务终态和后端类别时建立公开报告。
    /// 原始错误正文只保留在 `Error::source`，不会进入 CLI 或 JSONL。
    pub(crate) fn diagnostic_report(
        &self,
        stage: GenericDiagnosticStage,
        database: &Path,
        effect: StateEffect,
    ) -> DiagnosticReport {
        let generic = |problem| {
            DiagnosticReport::new(
                effect,
                Diagnostic::generic(GenericIssue::project(stage, problem)),
            )
        };
        match self {
            Self::Cancelled => generic(GenericProblem::ProjectCancelled),
            Self::MissingInitialField(field) => generic(GenericProblem::MissingInitialField {
                field: project_safe_identifier(field, "initial_field"),
            }),
            Self::WorkspaceNotDirectory { path } | Self::SourceNotDirectory { path } => {
                file_system_project_report(
                    stage,
                    FileSystemOperation::Open,
                    FileSystemProblem::NotDirectory {
                        path: SafePath::new(path),
                    },
                    effect,
                )
            }
            Self::SourceWriteBackOverlap {
                source_root,
                write_back_root,
            } => generic(GenericProblem::SourceWriteBackOverlap {
                source_root: SafePath::new(source_root),
                write_back_root: SafePath::new(write_back_root),
            }),
            Self::ProjectNotFound { path } => file_system_project_report(
                stage,
                FileSystemOperation::Open,
                FileSystemProblem::NotFound {
                    path: SafePath::new(path),
                },
                effect,
            ),
            Self::ProjectIdentityMismatch { expected, observed } => {
                generic(GenericProblem::ProjectIdentityMismatch {
                    expected: project_safe_identifier(expected, "expected_project"),
                    observed: project_safe_identifier(observed, "observed_project"),
                })
            }
            Self::Io {
                operation,
                path,
                source,
            } => file_system_project_report(
                stage,
                *operation,
                FileSystemProblem::Io {
                    path: SafePath::new(path),
                    failure: IoFailure::from_error(source),
                },
                effect,
            ),
            Self::Sqlite { operation, source } => sqlite_project_report(
                stage,
                database,
                operation,
                source,
                SqliteOperation::Execute,
                SqliteTransactionState::Active,
                effect,
            ),
            Self::TransactionNotCommitted { operation, source } => sqlite_project_report(
                stage,
                database,
                operation,
                source,
                SqliteOperation::Transaction,
                SqliteTransactionState::RolledBack,
                StateEffect::Unchanged,
            ),
            Self::InitialDatabaseFileSystem { operation, source } => DiagnosticReport::new(
                effect,
                source.diagnostic(FileSystemDiagnosticContext::new(
                    file_system_project_stage(stage),
                    *operation,
                )),
            ),
            Self::InitialDatabaseOutcomeUnknown(source) => {
                source.diagnostic_report(stage, database, StateEffect::OutcomeUnknown)
            }
            Self::TransactionOutcomeUnknown {
                operation: _,
                primary,
                finalization,
            } => {
                let finalization = match finalization {
                    GenericTransactionFinalizationFailure::Sqlite { operation, source } => {
                        sqlite_project_report(
                            stage,
                            database,
                            operation,
                            source,
                            SqliteOperation::Transaction,
                            SqliteTransactionState::OutcomeUnknown,
                            StateEffect::OutcomeUnknown,
                        )
                    }
                    GenericTransactionFinalizationFailure::InvalidState { .. } => {
                        DiagnosticReport::new(
                            StateEffect::OutcomeUnknown,
                            Diagnostic::sqlite(SqliteIssue::new(
                                SqliteDiagnosticContext::new(
                                    sqlite_project_stage(stage),
                                    SqliteOperation::Transaction,
                                    SqliteTransactionState::OutcomeUnknown,
                                ),
                                SqliteProblem::InternalInvariant {
                                    database: SafePath::new(database),
                                },
                            )),
                        )
                    }
                };
                primary.as_deref().map_or(finalization.clone(), |primary| {
                    primary
                        .diagnostic_report(stage, database, StateEffect::Unchanged)
                        .with_related(RelatedFailureRelation::Finalization, finalization)
                })
            }
            Self::InitialCandidateCleanup { original, cleanup } => {
                let mut report = original.diagnostic_report(stage, database, effect);
                for source in cleanup {
                    report = report.with_related(
                        RelatedFailureRelation::Cleanup,
                        source.diagnostic_report(stage, database, StateEffect::RecoveryRequired),
                    );
                }
                report
            }
            Self::InvalidDatabase { problem, .. } => {
                generic(GenericProblem::InvalidProjectDatabase {
                    problem: problem.clone(),
                })
            }
            Self::InvalidLanguage(source) => generic(GenericProblem::InvalidLanguage {
                violation: generic_language_violation(source),
            }),
            Self::SameSourceAndTargetLanguage { language } => {
                generic(GenericProblem::SameSourceAndTargetLanguage {
                    language: project_safe_identifier(language, "language"),
                })
            }
            Self::Jsonl(source) => DiagnosticReport::new(effect, source.diagnostic(stage)),
            Self::InputChangedDuringExtract => generic(GenericProblem::InputChangedDuringExtract),
            Self::ExtractRequired => generic(GenericProblem::ExtractRequired),
            Self::TranslationSnapshotChanged => generic(GenericProblem::TranslationSnapshotChanged),
            Self::InvalidTranslation {
                group_id,
                unit_id,
                problem,
                ..
            } => generic(GenericProblem::InvalidTranslation {
                group_id: group_id
                    .as_deref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                unit_id: unit_id
                    .as_deref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                problem: problem.clone(),
            }),
            Self::DuplicateTranslationWrite { group_id, unit_id } => {
                generic(GenericProblem::DuplicateTranslationWrite {
                    group_id: project_safe_identifier(group_id, "group_id"),
                    unit_id: project_safe_identifier(unit_id, "unit_id"),
                })
            }
            Self::DuplicateTranslationClear { group_id, unit_id } => {
                generic(GenericProblem::DuplicateTranslationClear {
                    group_id: project_safe_identifier(group_id, "group_id"),
                    unit_id: project_safe_identifier(unit_id, "unit_id"),
                })
            }
            Self::BlankProfileId => generic(GenericProblem::BlankProfileId),
            Self::InvalidResource(source) => generic_project_resource_report(source, stage, effect),
            Self::InvalidLayoutRules(source) => generic(GenericProblem::WriteBackLayoutRules {
                path: None,
                rule_number: source.rule_number(),
                project_snapshot: true,
            }),
        }
    }
}

pub(super) fn project_safe_identifier(
    value: impl AsRef<str>,
    fallback: &'static str,
) -> SafeIdentifier {
    SafeIdentifier::new(value).unwrap_or_else(|_| SafeIdentifier::from_validated(fallback))
}

pub(super) fn project_optional_safe_identifier(value: impl AsRef<str>) -> Option<SafeIdentifier> {
    SafeIdentifier::new(value).ok()
}

pub(super) fn invalid_database(problem: GenericProjectDatabaseProblem) -> GenericProjectError {
    GenericProjectError::InvalidDatabase {
        problem,
        source: None,
    }
}

fn generic_project_resource_report(
    source: &GenericProjectResourceError,
    stage: GenericDiagnosticStage,
    effect: StateEffect,
) -> DiagnosticReport {
    let planning_resource = |resource, problem| {
        DiagnosticReport::new(
            effect,
            Diagnostic::translation(TranslationIssue::PlanningResource {
                resource,
                origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                problem,
            }),
        )
    };
    match source {
        GenericProjectResourceError::InvalidSnapshot { resource, source } => planning_resource(
            translation_planning_resource_kind(*resource),
            TranslationPlanningResourceProblem::InvalidSnapshotJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::SnapshotEncoding { resource, source } => planning_resource(
            translation_planning_resource_kind(*resource),
            TranslationPlanningResourceProblem::SnapshotEncodingJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::TerminologyDefinition(source) => planning_resource(
            TranslationPlanningResourceKind::Terminology,
            terminology_problem(source),
        ),
        GenericProjectResourceError::Placeholder(
            GenericPlaceholderError::InvalidResourceSnapshot(source),
        ) => planning_resource(
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::InvalidSnapshotJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::Placeholder(GenericPlaceholderError::Compilation(source)) => {
            DiagnosticReport::new(
                effect,
                Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                    origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                    problem: source.diagnostic_problem(),
                }),
            )
        }
        GenericProjectResourceError::Placeholder(_) => DiagnosticReport::new(
            effect,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::UnexpectedResourceState {
                    resource: GenericResourceKind::PlaceholderRules,
                },
            )),
        ),
        GenericProjectResourceError::NonCanonicalSnapshot { resource } => DiagnosticReport::new(
            effect,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::NonCanonicalResourceSnapshot {
                    resource: *resource,
                },
            )),
        ),
    }
}

const fn translation_planning_resource_kind(
    resource: GenericResourceKind,
) -> TranslationPlanningResourceKind {
    match resource {
        GenericResourceKind::Terminology => TranslationPlanningResourceKind::Terminology,
        GenericResourceKind::PlaceholderRules => TranslationPlanningResourceKind::PlaceholderRules,
    }
}

fn file_system_project_report(
    stage: GenericDiagnosticStage,
    operation: FileSystemOperation,
    problem: FileSystemProblem,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::file_system(FileSystemIssue::new(
            FileSystemDiagnosticContext::new(file_system_project_stage(stage), operation),
            problem,
        )),
    )
}

fn sqlite_project_report(
    stage: GenericDiagnosticStage,
    database: &Path,
    query_id: &'static str,
    source: &rusqlite::Error,
    operation: SqliteOperation,
    transaction: SqliteTransactionState,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::sqlite(SqliteIssue::new(
            SqliteDiagnosticContext::new(sqlite_project_stage(stage), operation, transaction),
            SqliteProblem::Driver {
                database: SafePath::new(database),
                query_id: SafeIdentifier::new(query_id).ok(),
                query_ordinal: None,
                failure: SqliteDriverFailure::from_error(source),
            },
        )),
    )
}

const fn file_system_project_stage(stage: GenericDiagnosticStage) -> FileSystemDiagnosticStage {
    match stage {
        GenericDiagnosticStage::ProjectOpening => FileSystemDiagnosticStage::Project,
        GenericDiagnosticStage::Init => FileSystemDiagnosticStage::Project,
        GenericDiagnosticStage::Extract => FileSystemDiagnosticStage::Extract,
        GenericDiagnosticStage::Translate | GenericDiagnosticStage::TaskRecord => {
            FileSystemDiagnosticStage::Translate
        }
        GenericDiagnosticStage::WriteBack => FileSystemDiagnosticStage::WriteBack,
    }
}

const fn sqlite_project_stage(stage: GenericDiagnosticStage) -> SqliteDiagnosticStage {
    match stage {
        GenericDiagnosticStage::ProjectOpening => SqliteDiagnosticStage::Project,
        GenericDiagnosticStage::Init => SqliteDiagnosticStage::Init,
        GenericDiagnosticStage::Extract => SqliteDiagnosticStage::Extract,
        GenericDiagnosticStage::Translate | GenericDiagnosticStage::TaskRecord => {
            SqliteDiagnosticStage::Translate
        }
        GenericDiagnosticStage::WriteBack => SqliteDiagnosticStage::WriteBack,
    }
}

const fn generic_language_violation(source: &LanguageIdError) -> GenericLanguageViolation {
    match source {
        LanguageIdError::Blank => GenericLanguageViolation::Blank,
        LanguageIdError::SurroundingWhitespace { .. } => {
            GenericLanguageViolation::SurroundingWhitespace
        }
        LanguageIdError::Underscore { .. } => GenericLanguageViolation::Underscore,
        LanguageIdError::InvalidSyntax { .. } => GenericLanguageViolation::InvalidSyntax,
        LanguageIdError::InvalidRegistryTag { .. } => GenericLanguageViolation::InvalidRegistryTag,
        LanguageIdError::CanonicalizationFailed { .. } => {
            GenericLanguageViolation::CanonicalizationFailed
        }
        LanguageIdError::UndefinedPrimaryLanguage { .. } => {
            GenericLanguageViolation::UndefinedPrimaryLanguage
        }
    }
}

impl GenericProjectError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::Jsonl(source) if source.is_cancelled())
    }

    pub(super) fn is_sqlite_cancellation_without_cleanup_failure(&self) -> bool {
        match self {
            Self::Sqlite { source, .. } => {
                sqlite_error_is_busy(source) || sqlite_error_is_interrupted(source)
            }
            Self::Jsonl(source) => source.is_cancelled(),
            Self::Cancelled => true,
            Self::InitialCandidateCleanup { .. }
            | Self::MissingInitialField(_)
            | Self::WorkspaceNotDirectory { .. }
            | Self::SourceNotDirectory { .. }
            | Self::SourceWriteBackOverlap { .. }
            | Self::ProjectNotFound { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::Io { .. }
            | Self::InitialDatabaseFileSystem { .. }
            | Self::InitialDatabaseOutcomeUnknown(_)
            | Self::TransactionNotCommitted { .. }
            | Self::TransactionOutcomeUnknown { .. }
            | Self::InvalidDatabase { .. }
            | Self::InvalidLanguage(_)
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::InvalidTranslation { .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::BlankProfileId
            | Self::InvalidResource(_)
            | Self::InvalidLayoutRules(_) => false,
        }
    }
}

pub(super) fn sqlite_error_is_busy(source: &rusqlite::Error) -> bool {
    matches!(
        source.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

pub(super) fn sqlite_error_is_interrupted(source: &rusqlite::Error) -> bool {
    matches!(
        source.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    )
}

pub(super) fn sqlite_operation_error(
    operation: &'static str,
    source: rusqlite::Error,
) -> GenericProjectError {
    if sqlite_error_is_interrupted(&source) {
        GenericProjectError::Cancelled
    } else {
        GenericProjectError::Sqlite { operation, source }
    }
}

impl From<GenericJsonlError> for GenericProjectError {
    fn from(source: GenericJsonlError) -> Self {
        Self::Jsonl(source)
    }
}

impl From<LanguageIdError> for GenericProjectError {
    fn from(source: LanguageIdError) -> Self {
        Self::InvalidLanguage(source)
    }
}
