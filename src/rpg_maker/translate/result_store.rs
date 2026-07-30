//! RPG Maker 译文状态的原子对账与提交。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact, ReportedFailure,
    SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    PLACEHOLDER_RULES_RESOURCE_KIND, TERMINOLOGY_RESOURCE_KIND,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, SqliteBatch, SqliteCommand, SqliteQuery, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};
use crate::storage::sqlite_session::{
    SqliteInteractiveSessionFinalizationError, SqliteInteractiveSessionFinalizationFailure,
    SqliteInteractiveSessionFinalizer,
};
use crate::storage::sqlite_transaction_session::{
    OpenSqliteTransactionSessionError, SqliteTransactionSessionFactory,
    SqliteTransactionSessionOperations,
};

use super::pipeline::{
    RpgMakerTranslationResultStore, TranslationPlanPreparation, TranslationSnapshotBaseline,
    TranslationTaskOutcome, TranslationUnitIdentity,
};
use super::task_record::{TranslationTaskCommitFailure, TranslationTaskCommitFailureImpact};

pub(crate) struct RpgMakerTranslationResultStorageService<S, C>
where
    S: SqliteTransactionSessionFactory,
{
    sqlite: S,
    cpu: C,
    session: tokio::sync::Mutex<TranslationTransactionSession<S::Operations, S::Finalizer>>,
}

enum TranslationTransactionSession<O, F> {
    Unopened,
    Open {
        database_path: PathBuf,
        operations: Arc<O>,
        finalizer: F,
    },
    Finalized,
}

impl<S, C> RpgMakerTranslationResultStorageService<S, C>
where
    S: SqliteTransactionSessionFactory,
{
    pub(crate) fn new(sqlite: S, cpu: C) -> Self {
        Self {
            sqlite,
            cpu,
            session: tokio::sync::Mutex::new(TranslationTransactionSession::Unopened),
        }
    }
}

impl<S, C> RpgMakerTranslationResultStore for RpgMakerTranslationResultStorageService<S, C>
where
    S: SqliteTransactionSessionFactory,
    C: CpuTaskExecutor,
{
    type PreparedCommit = RpgMakerPreparedTranslationCommit;
    type Error = RpgMakerTranslationResultStorageError<S::Error, C::Error>;

    async fn apply_preparation(
        &self,
        project: &OpenedProject,
        preparation: TranslationPlanPreparation,
    ) -> Result<(), Self::Error> {
        let plan = self.encode_preparation_plan(preparation).await?;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }

    async fn prepare_commit(
        &self,
        outcome: Arc<TranslationTaskOutcome>,
    ) -> Result<Self::PreparedCommit, TranslationTaskCommitFailure<Self::Error>> {
        self.encode_commit_plan(outcome).await.map_err(|source| {
            let diagnostic = source.task_commit_safe_diagnostic();
            TranslationTaskCommitFailure::not_applied(source, Some(diagnostic))
        })
    }

    async fn commit_prepared(
        &self,
        project: &OpenedProject,
        prepared: Self::PreparedCommit,
    ) -> Result<(), TranslationTaskCommitFailure<Self::Error>> {
        let RpgMakerPreparedTranslationCommit { plan } = prepared;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
            .map_err(|source| {
                let impact = source.commit_failure_impact();
                let diagnostic = source.task_commit_safe_diagnostic();
                TranslationTaskCommitFailure::new(source, impact, Some(diagnostic))
            })
    }

    async fn finalize(&self) -> Result<(), Self::Error> {
        self.finalize_session().await
    }
}

/// 已完成全部纯计算编码与校验、只等待独立事务提交的任务结果。
pub(crate) struct RpgMakerPreparedTranslationCommit {
    plan: SqliteTransactionPlan,
}

impl<S, C> RpgMakerTranslationResultStorageService<S, C>
where
    S: SqliteTransactionSessionFactory,
    C: CpuTaskExecutor,
{
    async fn encode_preparation_plan(
        &self,
        preparation: TranslationPlanPreparation,
    ) -> Result<SqliteTransactionPlan, RpgMakerTranslationResultStorageError<S::Error, C::Error>>
    {
        let (work, terminology_json, placeholder_rules_json, snapshot_baseline) = self
            .cpu
            .execute(move || preparation_work(preparation))
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerTranslationResultStorageError::InvalidPlan)?;
        let units = self
            .cpu
            .execute_ordered_map(work, encode_preparation_unit)
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?;

        self.cpu
            .execute(move || {
                finish_preparation_plan(
                    units.into_iter().collect::<Result<Vec<_>, _>>()?,
                    terminology_json,
                    placeholder_rules_json,
                    snapshot_baseline,
                )
            })
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerTranslationResultStorageError::InvalidPlan)
    }

    async fn encode_commit_plan(
        &self,
        outcome: Arc<TranslationTaskOutcome>,
    ) -> Result<
        RpgMakerPreparedTranslationCommit,
        RpgMakerTranslationResultStorageError<S::Error, C::Error>,
    > {
        let work = self
            .cpu
            .execute(move || commit_work(outcome))
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerTranslationResultStorageError::InvalidPlan)?;
        let decisions = self
            .cpu
            .execute_ordered_map(work.decisions, encode_commit_decision)
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?;
        let units = self
            .cpu
            .execute_ordered_map(work.units, encode_commit_unit)
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?;

        self.cpu
            .execute(move || {
                Ok::<_, ResultStoragePlanError>(RpgMakerPreparedTranslationCommit {
                    plan: finish_commit_plan(
                        decisions.into_iter().collect::<Result<Vec<_>, _>>()?,
                        units.into_iter().collect::<Result<Vec<_>, _>>()?,
                    )?,
                })
            })
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerTranslationResultStorageError::InvalidPlan)
    }

    async fn execute(
        &self,
        database_path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> Result<(), RpgMakerTranslationResultStorageError<S::Error, C::Error>> {
        let mut session = self.session.lock().await;
        if matches!(*session, TranslationTransactionSession::Unopened) {
            let opened = self
                .sqlite
                .open_existing_transaction_session(database_path.clone())
                .await
                .map_err(|error| map_session_open_error(database_path.clone(), error))?;
            let (operations, finalizer) = opened.into_parts();
            *session = TranslationTransactionSession::Open {
                database_path: database_path.clone(),
                operations,
                finalizer,
            };
        }

        let TranslationTransactionSession::Open {
            database_path: opened_path,
            operations,
            ..
        } = &*session
        else {
            return Err(RpgMakerTranslationResultStorageError::SessionFinalized { database_path });
        };
        if opened_path != &database_path {
            return Err(
                RpgMakerTranslationResultStorageError::SessionDatabaseChanged {
                    opened_path: opened_path.clone(),
                    requested_path: database_path,
                },
            );
        }
        operations
            .execute_transaction(plan)
            .await
            .map_err(|error| map_transaction_error(database_path, error))
    }

    async fn finalize_session(
        &self,
    ) -> Result<(), RpgMakerTranslationResultStorageError<S::Error, C::Error>> {
        let open = {
            let mut session = self.session.lock().await;
            match std::mem::replace(&mut *session, TranslationTransactionSession::Finalized) {
                TranslationTransactionSession::Unopened
                | TranslationTransactionSession::Finalized => None,
                TranslationTransactionSession::Open {
                    database_path,
                    operations,
                    finalizer,
                } => {
                    // 先释放最后一个操作面引用，随后终结令牌关闭命令通道并等待 actor。
                    drop(operations);
                    Some((database_path, finalizer))
                }
            }
        };
        let Some((database_path, finalizer)) = open else {
            return Ok(());
        };
        let report = finalizer.finalize().await.map_err(|source| {
            RpgMakerTranslationResultStorageError::FinalizationFailed {
                database_path: database_path.clone(),
                source,
            }
        })?;
        if report.had_unclosed_transaction() {
            return Err(
                RpgMakerTranslationResultStorageError::FinalizationRolledBackTransaction {
                    database_path,
                },
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerTranslationResultStorageError<S, C> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    InvalidPlan(ResultStoragePlanError),
    DatabaseNotFound {
        database_path: PathBuf,
    },
    StalePlan {
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
    SessionDatabaseChanged {
        opened_path: PathBuf,
        requested_path: PathBuf,
    },
    SessionFinalized {
        database_path: PathBuf,
    },
    FinalizationRolledBackTransaction {
        database_path: PathBuf,
    },
    FinalizationFailed {
        database_path: PathBuf,
        source: SqliteInteractiveSessionFinalizationError<S>,
    },
}

impl<S, C> RpgMakerTranslationResultStorageError<S, C> {
    fn commit_failure_impact(&self) -> TranslationTaskCommitFailureImpact {
        if matches!(self, Self::OutcomeUnknown { .. }) {
            TranslationTaskCommitFailureImpact::OutcomeUnknown
        } else {
            TranslationTaskCommitFailureImpact::NotApplied
        }
    }
}

impl<S, C> RpgMakerTranslationResultStorageError<S, C> {
    /// 在任务提交边界仍持有数据库路径和事务终态时建立窄小公开投影。
    fn task_commit_safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::ScheduleEncoding(source) => translation_storage_cpu_task_diagnostic(source),
            Self::InvalidPlan(source) => source.safe_diagnostic(),
            Self::DatabaseNotFound { database_path } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::CheckProjectState,
            ),
            Self::StalePlan { database_path } => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            ),
            Self::NotCommitted { database_path, .. } => SafeDiagnostic::new(
                DiagnosticCode::SqliteOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::TransactionRolledBack),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            )
            .with_recovery(RecoveryFact::transaction("rolled_back")),
            Self::OutcomeUnknown { database_path, .. } => SafeDiagnostic::new(
                DiagnosticCode::SqliteOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::TransactionOutcomeUnknown),
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(RecoveryFact::transaction("outcome_unknown")),
            Self::SessionDatabaseChanged {
                opened_path,
                requested_path,
            } => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(requested_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    "storage_error=session_database_changed",
                ),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            )
            .with_recovery(RecoveryFact::path(opened_path)),
            Self::SessionFinalized { database_path } => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    "storage_error=session_finalized",
                ),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            ),
            Self::FinalizationRolledBackTransaction { database_path } => SafeDiagnostic::new(
                DiagnosticCode::StateFinalizationFailed,
                DiagnosticStage::Translate,
                DiagnosticSubject::path(database_path),
                DiagnosticReason::failure(DiagnosticFailureKind::TransactionRolledBack),
                DiagnosticImpact::StateAppliedFinalizationFailed,
                DiagnosticAction::Retry,
            )
            .with_recovery(RecoveryFact::transaction("rolled_back_during_finalization")),
            Self::FinalizationFailed {
                database_path,
                source: finalization,
            } => {
                let (reason, impact, action, transaction) = match finalization.primary() {
                    SqliteInteractiveSessionFinalizationFailure::CleanupFailed(_) => (
                        DiagnosticFailureKind::FinalizationFailed,
                        DiagnosticImpact::StateAppliedFinalizationFailed,
                        DiagnosticAction::Retry,
                        "cleanup_failed",
                    ),
                    SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(_) => (
                        DiagnosticFailureKind::TransactionOutcomeUnknown,
                        DiagnosticImpact::OutcomeUnknown,
                        DiagnosticAction::PreserveRecoveryArtifacts,
                        "outcome_unknown",
                    ),
                };
                let mut diagnostic = SafeDiagnostic::new(
                    DiagnosticCode::StateFinalizationFailed,
                    DiagnosticStage::Translate,
                    DiagnosticSubject::path(database_path),
                    DiagnosticReason::failure(reason),
                    impact,
                    action,
                )
                .with_recovery(RecoveryFact::transaction(transaction));
                if finalization.connection_close().is_some() {
                    diagnostic = diagnostic
                        .with_recovery(RecoveryFact::component("sqlite_connection_close=failed"));
                }
                diagnostic
            }
        }
    }
}

impl<S, C> RpgMakerTranslationResultStorageError<S, C>
where
    S: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    /// 在结果存储仍持有数据库路径、事务终态和具体根错误时建立公开投影。
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::ScheduleEncoding(source) => source.safe_diagnostic_source(
                DiagnosticStage::Translate,
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            ),
            Self::NotCommitted {
                database_path,
                source,
            } => translation_storage_source_diagnostic(
                source,
                database_path,
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            )
            .with_recovery(RecoveryFact::transaction("rolled_back")),
            Self::OutcomeUnknown {
                database_path,
                source,
            } => translation_storage_source_diagnostic(
                source,
                database_path,
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(RecoveryFact::transaction("outcome_unknown")),
            Self::FinalizationFailed {
                database_path,
                source: finalization,
            } => {
                let mut diagnostic = translation_storage_finalization_diagnostic(
                    finalization.primary(),
                    database_path,
                );
                if finalization.connection_close().is_some() {
                    diagnostic = diagnostic
                        .with_recovery(RecoveryFact::component("sqlite_connection_close=failed"));
                }
                diagnostic
            }
            _ => self.task_commit_safe_diagnostic(),
        }
    }
}

impl<S, C> RpgMakerTranslationResultStorageError<S, C>
where
    S: Error + SafeDiagnosticSource + Send + Sync + 'static,
    C: Error + Send + Sync + 'static,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    /// 消费结果存储错误；SQLite 收尾的主失败与连接关闭失败分别进入报告。
    pub(crate) fn into_failure_report(self) -> FailureReport {
        match self {
            Self::FinalizationFailed {
                database_path,
                source,
            } => {
                let (primary, connection_close) = source.into_parts();
                let primary_diagnostic =
                    translation_storage_finalization_diagnostic(&primary, &database_path);
                let primary_source = match primary {
                    SqliteInteractiveSessionFinalizationFailure::CleanupFailed(source)
                    | SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(source) => source,
                };
                let mut report =
                    FailureReport::new(ReportedFailure::new(primary_diagnostic, primary_source));
                if let Some(source) = connection_close {
                    let diagnostic = translation_storage_source_diagnostic(
                        &source,
                        &database_path,
                        DiagnosticImpact::StateAppliedFinalizationFailed,
                        DiagnosticAction::Retry,
                    )
                    .with_recovery(RecoveryFact::component("sqlite_connection_close=failed"));
                    report = report.with_related(ReportedFailure::new(diagnostic, source));
                }
                report
            }
            source => {
                let diagnostic = source.safe_diagnostic();
                FailureReport::new(ReportedFailure::new(diagnostic, source))
            }
        }
    }
}

fn translation_storage_cpu_task_diagnostic<C>(source: &CpuTaskExecutionError<C>) -> SafeDiagnostic {
    let (failure, action) = match source {
        CpuTaskExecutionError::Cancelled => (
            DiagnosticFailureKind::LockCancelled,
            DiagnosticAction::Retry,
        ),
        CpuTaskExecutionError::Unavailable(_) => (
            DiagnosticFailureKind::ExecutorClosed,
            DiagnosticAction::Retry,
        ),
        CpuTaskExecutionError::TaskPanicked => (
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticAction::ReportBug,
        ),
    };
    SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Translate,
        DiagnosticSubject::component("CPU worker"),
        DiagnosticReason::failure(failure),
        DiagnosticImpact::ProgressPreserved,
        action,
    )
}

fn translation_storage_finalization_diagnostic<S>(
    finalization: &SqliteInteractiveSessionFinalizationFailure<S>,
    database_path: &std::path::Path,
) -> SafeDiagnostic
where
    S: SafeDiagnosticSource,
{
    let (source, impact, action, transaction) = match finalization {
        SqliteInteractiveSessionFinalizationFailure::CleanupFailed(source) => (
            source,
            DiagnosticImpact::StateAppliedFinalizationFailed,
            DiagnosticAction::Retry,
            "cleanup_failed",
        ),
        SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(source) => (
            source,
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::PreserveRecoveryArtifacts,
            "outcome_unknown",
        ),
    };
    translation_storage_source_diagnostic(source, database_path, impact, action)
        .with_recovery(RecoveryFact::transaction(transaction))
}

fn translation_storage_source_diagnostic<S>(
    source: &S,
    database_path: &std::path::Path,
    impact: DiagnosticImpact,
    action: DiagnosticAction,
) -> SafeDiagnostic
where
    S: SafeDiagnosticSource,
{
    let mut diagnostic = source.safe_diagnostic_source(DiagnosticStage::Translate, impact, action);
    diagnostic.stage = DiagnosticStage::Translate;
    diagnostic.subject = DiagnosticSubject::path(database_path);
    diagnostic.impact = impact;
    diagnostic.action = action;
    diagnostic
}

impl<S: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerTranslationResultStorageError<S, C>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleEncoding(source) => write!(formatter, "译文写入编码任务失败：{source}"),
            Self::InvalidPlan(source) => write!(formatter, "译文写入计划无效：{source}"),
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::StalePlan { database_path } => write!(
                formatter,
                "翻译计划建立后资产已发生变化（{}）",
                database_path.display()
            ),
            Self::NotCommitted {
                database_path,
                source,
            } => write!(
                formatter,
                "译文事务未提交到 {}：{source}",
                database_path.display()
            ),
            Self::OutcomeUnknown {
                database_path,
                source,
            } => write!(
                formatter,
                "无法确认译文事务是否已提交到 {}：{source}",
                database_path.display()
            ),
            Self::SessionDatabaseChanged {
                opened_path,
                requested_path,
            } => write!(
                formatter,
                "同一轮翻译不能把数据库会话从 {} 切换到 {}",
                opened_path.display(),
                requested_path.display()
            ),
            Self::SessionFinalized { database_path } => write!(
                formatter,
                "项目数据库会话已终结，不能继续写入 {}",
                database_path.display()
            ),
            Self::FinalizationRolledBackTransaction { database_path } => write!(
                formatter,
                "终结项目数据库会话时发现并回滚了未结束事务：{}",
                database_path.display()
            ),
            Self::FinalizationFailed {
                database_path,
                source,
            } => write!(
                formatter,
                "无法终结项目数据库会话 {}：{source}",
                database_path.display()
            ),
        }
    }
}

impl<S: Error + 'static, C: Error + 'static> Error for RpgMakerTranslationResultStorageError<S, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleEncoding(source) => Some(source),
            Self::InvalidPlan(source) => Some(source),
            Self::NotCommitted { source, .. } | Self::OutcomeUnknown { source, .. } => Some(source),
            Self::FinalizationFailed { source, .. } => Some(source),
            Self::DatabaseNotFound { .. }
            | Self::StalePlan { .. }
            | Self::SessionDatabaseChanged { .. }
            | Self::SessionFinalized { .. }
            | Self::FinalizationRolledBackTransaction { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ResultStoragePlanError {
    Location(RpgMakerLocationCodecError),
    Projection(RpgMakerProjectionCodecError),
    Content(serde_json::Error),
    EmptyTaskResult,
    EmptyReuseTargets,
    BlankTranslation,
    InconsistentTranslationState,
    MismatchedReuseSourceContent,
    MismatchedReuseSourceContext,
    MismatchedPropagationSourceContent,
    MismatchedPropagationSourceContext,
    DuplicateUnit,
    InvalidCommitDecisionSequence,
    MissingCommitDecisionUnit,
}

impl ResultStoragePlanError {
    /// 只投影闭集不变量、结构字段和类型化编解码事实，不公开正文或任意错误文本。
    fn safe_diagnostic(&self) -> SafeDiagnostic {
        let (subject, detail) = match self {
            Self::Location(source) => (
                DiagnosticSubject::field("group_location"),
                result_storage_location_codec_detail(source),
            ),
            Self::Projection(source) => (
                DiagnosticSubject::field("projection_recipe_json"),
                result_storage_projection_codec_detail(source),
            ),
            Self::Content(source) => (
                DiagnosticSubject::field("text_unit_content_json"),
                result_storage_json_detail("content_encode", source),
            ),
            Self::EmptyTaskResult => (
                DiagnosticSubject::component("translation_task_result"),
                "plan_error=empty_task_result".to_owned(),
            ),
            Self::EmptyReuseTargets => (
                DiagnosticSubject::component("translation_reuse_plan"),
                "plan_error=empty_reuse_targets".to_owned(),
            ),
            Self::BlankTranslation => (
                DiagnosticSubject::field("translation"),
                "plan_error=blank_translation".to_owned(),
            ),
            Self::InconsistentTranslationState => (
                DiagnosticSubject::field("translation_state"),
                "plan_error=inconsistent_translation_state".to_owned(),
            ),
            Self::MismatchedReuseSourceContent => (
                DiagnosticSubject::field("source_content"),
                "plan_error=mismatched_reuse_source_content".to_owned(),
            ),
            Self::MismatchedReuseSourceContext => (
                DiagnosticSubject::field("source_context"),
                "plan_error=mismatched_reuse_source_context".to_owned(),
            ),
            Self::MismatchedPropagationSourceContent => (
                DiagnosticSubject::field("source_content"),
                "plan_error=mismatched_propagation_source_content".to_owned(),
            ),
            Self::MismatchedPropagationSourceContext => (
                DiagnosticSubject::field("source_context"),
                "plan_error=mismatched_propagation_source_context".to_owned(),
            ),
            Self::DuplicateUnit => (
                DiagnosticSubject::component("translation_commit_units"),
                "plan_error=duplicate_unit".to_owned(),
            ),
            Self::InvalidCommitDecisionSequence => (
                DiagnosticSubject::component("translation_commit_decisions"),
                "plan_error=invalid_commit_decision_sequence".to_owned(),
            ),
            Self::MissingCommitDecisionUnit => (
                DiagnosticSubject::component("translation_commit_decisions"),
                "plan_error=missing_commit_decision_unit".to_owned(),
            ),
        };
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            subject,
            DiagnosticReason::failure_with_detail(DiagnosticFailureKind::InternalInvariant, detail),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        )
    }
}

fn result_storage_location_codec_detail(source: &RpgMakerLocationCodecError) -> String {
    format!(
        "plan_error=location_codec; {}",
        source.safe_diagnostic_detail()
    )
}

fn result_storage_projection_codec_detail(source: &RpgMakerProjectionCodecError) -> String {
    format!(
        "plan_error=projection_codec; {}",
        source.safe_diagnostic_detail()
    )
}

fn result_storage_json_detail(operation: &'static str, source: &serde_json::Error) -> String {
    let category = JsonErrorCategory::from(source);
    format!(
        "plan_error=json; operation={operation}; json_category={category}; json_line={}; json_column={}",
        source.line(),
        source.column()
    )
}

impl fmt::Display for ResultStoragePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => source.fmt(formatter),
            Self::Projection(source) => source.fmt(formatter),
            Self::Content(source) => write!(formatter, "文本单元内容无法编码为 JSON：{source}"),
            Self::EmptyTaskResult => formatter.write_str("任务结果不包含任何译文"),
            Self::EmptyReuseTargets => formatter.write_str("译文复用计划不包含任何目标"),
            Self::BlankTranslation => formatter.write_str("任务结果包含空白译文"),
            Self::InconsistentTranslationState => {
                formatter.write_str("读取时的译文与译文状态没有同时存在或同时缺失")
            }
            Self::MismatchedReuseSourceContent => {
                formatter.write_str("译文复用种子与目标的完整源内容不一致")
            }
            Self::MismatchedReuseSourceContext => {
                formatter.write_str("译文复用种子与目标的源上下文不一致")
            }
            Self::MismatchedPropagationSourceContent => {
                formatter.write_str("译文代表与传播目标的完整源内容不一致")
            }
            Self::MismatchedPropagationSourceContext => {
                formatter.write_str("译文代表与传播目标的源上下文不一致")
            }
            Self::DuplicateUnit => formatter.write_str("同一事务重复修改同一文本单元"),
            Self::InvalidCommitDecisionSequence => {
                formatter.write_str("任务 decision 与提交工作之间的自然顺序不一致")
            }
            Self::MissingCommitDecisionUnit => {
                formatter.write_str("任务 decision 没有代表单元或传播目标提交工作")
            }
        }
    }
}

impl Error for ResultStoragePlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::Content(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct EncodedIdentity {
    owner: &'static str,
    group_location: String,
    unit_role: String,
    source_content_json: String,
    source_context_json: String,
}

enum PreparationUnitWork {
    Invalidation {
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseSeed {
        identity: TranslationUnitIdentity,
        expected_translation: Arc<TextUnitContent>,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseTarget {
        seed_source_content: Arc<TextUnitContent>,
        seed_source_context_json: Arc<str>,
        translation: Arc<TextUnitContent>,
        identity: TranslationUnitIdentity,
        expected_translation: Option<TextUnitContent>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    },
}

enum EncodedPreparationUnit {
    Invalidation {
        identity: EncodedIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseSeed {
        identity: EncodedIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseTarget {
        translation: String,
        identity: EncodedIdentity,
        expected_translation: Option<String>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    },
}

#[derive(Clone, Copy)]
enum CommitUnitPosition {
    Representative,
    PropagationTarget(usize),
}

struct CommitPlanWork {
    decisions: Vec<CommitDecisionWork>,
    units: Vec<CommitUnitWork>,
}

struct CommitDecisionWork {
    outcome: Arc<TranslationTaskOutcome>,
    decision_index: usize,
}

struct CommitUnitWork {
    outcome: Arc<TranslationTaskOutcome>,
    decision_index: usize,
    position: CommitUnitPosition,
}

struct EncodedCommitDecision {
    decision_index: usize,
    translation: String,
}

struct EncodedCommitUnit {
    decision_index: usize,
    identity: EncodedIdentity,
    translation_state: Sha256Fingerprint,
}

fn preparation_work(
    preparation: TranslationPlanPreparation,
) -> Result<
    (
        Vec<PreparationUnitWork>,
        String,
        String,
        TranslationSnapshotBaseline,
    ),
    ResultStoragePlanError,
> {
    let (invalidations, reuses, terminology_json, placeholder_rules_json, _, _, snapshot_baseline) =
        preparation.into_parts();
    let work_capacity = invalidations.len()
        + reuses
            .iter()
            .map(|reuse| 1 + reuse.targets().len())
            .sum::<usize>();
    let mut work = Vec::with_capacity(work_capacity);

    for invalidation in invalidations {
        let (identity, expected_translation, expected_translation_state) =
            invalidation.into_parts();
        work.push(PreparationUnitWork::Invalidation {
            identity,
            expected_translation,
            expected_translation_state,
        });
    }

    for reuse in reuses {
        let (seed, targets) = reuse.into_parts();
        if targets.is_empty() {
            return Err(ResultStoragePlanError::EmptyReuseTargets);
        }
        let (seed_identity, translation, expected_translation_state) = seed.into_parts();
        let seed_source_content = Arc::new(seed_identity.source_content().clone());
        let seed_source_context_json = Arc::<str>::from(seed_identity.source_context_json());
        let translation = Arc::new(translation);
        work.push(PreparationUnitWork::ReuseSeed {
            identity: seed_identity,
            expected_translation: Arc::clone(&translation),
            expected_translation_state,
        });

        for target in targets {
            let (
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            ) = target.into_parts();
            work.push(PreparationUnitWork::ReuseTarget {
                seed_source_content: Arc::clone(&seed_source_content),
                seed_source_context_json: Arc::clone(&seed_source_context_json),
                translation: Arc::clone(&translation),
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            });
        }
    }

    Ok((
        work,
        terminology_json,
        placeholder_rules_json,
        snapshot_baseline,
    ))
}

fn encode_preparation_unit(
    work: PreparationUnitWork,
) -> Result<EncodedPreparationUnit, ResultStoragePlanError> {
    match work {
        PreparationUnitWork::Invalidation {
            identity,
            expected_translation,
            expected_translation_state,
        } => {
            ensure_nonblank(&expected_translation)?;
            Ok(EncodedPreparationUnit::Invalidation {
                identity: encode_identity(&identity)?,
                expected_translation: encode_content(&expected_translation)?,
                expected_translation_state,
            })
        }
        PreparationUnitWork::ReuseSeed {
            identity,
            expected_translation,
            expected_translation_state,
        } => {
            ensure_nonblank(&expected_translation)?;
            Ok(EncodedPreparationUnit::ReuseSeed {
                identity: encode_identity(&identity)?,
                expected_translation: encode_content(&expected_translation)?,
                expected_translation_state,
            })
        }
        PreparationUnitWork::ReuseTarget {
            seed_source_content,
            seed_source_context_json,
            translation,
            identity,
            expected_translation,
            expected_translation_state,
            replacement_translation_state,
        } => {
            ensure_nonblank(translation.as_ref())?;
            if identity.source_content() != seed_source_content.as_ref() {
                return Err(ResultStoragePlanError::MismatchedReuseSourceContent);
            }
            if identity.source_context_json() != seed_source_context_json.as_ref() {
                return Err(ResultStoragePlanError::MismatchedReuseSourceContext);
            }
            if expected_translation.is_some() != expected_translation_state.is_some() {
                return Err(ResultStoragePlanError::InconsistentTranslationState);
            }
            if expected_translation
                .as_ref()
                .is_some_and(TextUnitContent::is_blank)
            {
                return Err(ResultStoragePlanError::BlankTranslation);
            }
            Ok(EncodedPreparationUnit::ReuseTarget {
                translation: encode_content(translation.as_ref())?,
                identity: encode_identity(&identity)?,
                expected_translation: expected_translation
                    .as_ref()
                    .map(encode_content)
                    .transpose()?,
                expected_translation_state,
                replacement_translation_state,
            })
        }
    }
}

fn finish_preparation_plan(
    units: Vec<EncodedPreparationUnit>,
    terminology_json: String,
    placeholder_rules_json: String,
    snapshot_baseline: TranslationSnapshotBaseline,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut steps = vec![require_snapshot_baseline(&snapshot_baseline)];
    let mut seen = HashSet::with_capacity(units.len());
    let mut snapshot_parameter_sets = Vec::new();
    let mut clear_parameter_sets = Vec::new();
    let mut reuse_parameter_sets = Vec::new();
    for unit in units {
        match unit {
            EncodedPreparationUnit::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                clear_parameter_sets.push(clear_translation_parameters(
                    identity,
                    expected_translation,
                    expected_translation_state,
                ));
            }
            EncodedPreparationUnit::ReuseSeed {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                snapshot_parameter_sets.push(snapshot_parameters(
                    &identity,
                    Some((&expected_translation, expected_translation_state)),
                ));
            }
            EncodedPreparationUnit::ReuseTarget {
                translation,
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                reuse_parameter_sets.push(write_translation_from_snapshot_parameters(
                    identity,
                    translation,
                    replacement_translation_state,
                    expected_translation,
                    expected_translation_state,
                ));
            }
        }
    }
    if !snapshot_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
            REQUIRE_SNAPSHOT,
            snapshot_parameter_sets,
        )));
    }
    if !clear_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(CLEAR_TRANSLATION_FROM_SNAPSHOT, clear_parameter_sets),
        ));
    }
    if !reuse_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(WRITE_TRANSLATION_FROM_SNAPSHOT, reuse_parameter_sets),
        ));
    }
    steps.extend(resource_updates(terminology_json, placeholder_rules_json));
    Ok(SqliteTransactionPlan::new(steps))
}

fn commit_work(
    outcome: Arc<TranslationTaskOutcome>,
) -> Result<CommitPlanWork, ResultStoragePlanError> {
    let decisions = outcome.accepted();
    if decisions.is_empty() {
        return Err(ResultStoragePlanError::EmptyTaskResult);
    }
    let location_count = decisions
        .iter()
        .map(|decision| 1 + decision.propagation_targets().len())
        .sum();
    let mut decision_work = Vec::with_capacity(decisions.len());
    let mut unit_work = Vec::with_capacity(location_count);
    for (decision_index, decision) in decisions.iter().enumerate() {
        decision_work.push(CommitDecisionWork {
            outcome: Arc::clone(&outcome),
            decision_index,
        });
        unit_work.push(CommitUnitWork {
            outcome: Arc::clone(&outcome),
            decision_index,
            position: CommitUnitPosition::Representative,
        });
        for target_index in 0..decision.propagation_targets().len() {
            unit_work.push(CommitUnitWork {
                outcome: Arc::clone(&outcome),
                decision_index,
                position: CommitUnitPosition::PropagationTarget(target_index),
            });
        }
    }
    Ok(CommitPlanWork {
        decisions: decision_work,
        units: unit_work,
    })
}

fn encode_commit_decision(
    work: CommitDecisionWork,
) -> Result<EncodedCommitDecision, ResultStoragePlanError> {
    encode_commit_decision_with(work, encode_content)
}

fn encode_commit_decision_with(
    work: CommitDecisionWork,
    encode: impl FnOnce(&TextUnitContent) -> Result<String, ResultStoragePlanError>,
) -> Result<EncodedCommitDecision, ResultStoragePlanError> {
    let decision = work
        .outcome
        .accepted()
        .get(work.decision_index)
        .expect("提交工作必须引用已验收的 decision");
    let translation = decision.patch().translation();
    ensure_nonblank(translation)?;
    Ok(EncodedCommitDecision {
        decision_index: work.decision_index,
        translation: encode(translation)?,
    })
}

fn encode_commit_unit(work: CommitUnitWork) -> Result<EncodedCommitUnit, ResultStoragePlanError> {
    let decision = work
        .outcome
        .accepted()
        .get(work.decision_index)
        .expect("提交工作必须引用已验收的 decision");
    let patch = decision.patch();
    let (identity, translation_state) = match work.position {
        CommitUnitPosition::Representative => (patch.identity(), patch.translation_state()),
        CommitUnitPosition::PropagationTarget(target_index) => {
            let target = patch
                .propagation_targets()
                .get(target_index)
                .expect("提交工作必须引用已验收的传播目标");
            if target.identity().source_content() != patch.identity().source_content() {
                return Err(ResultStoragePlanError::MismatchedPropagationSourceContent);
            }
            if target.identity().source_context_json() != patch.identity().source_context_json() {
                return Err(ResultStoragePlanError::MismatchedPropagationSourceContext);
            }
            (
                target.identity(),
                target.state_context().finish(patch.translation()),
            )
        }
    };
    Ok(EncodedCommitUnit {
        decision_index: work.decision_index,
        identity: encode_identity(identity)?,
        translation_state,
    })
}

fn finish_commit_plan(
    decisions: Vec<EncodedCommitDecision>,
    units: Vec<EncodedCommitUnit>,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut seen = HashSet::with_capacity(units.len());
    let mut batches = Vec::with_capacity(decisions.len());
    for (expected_index, decision) in decisions.into_iter().enumerate() {
        if decision.decision_index != expected_index {
            return Err(ResultStoragePlanError::InvalidCommitDecisionSequence);
        }
        batches.push((decision.translation, Vec::new()));
    }
    for unit in units {
        ensure_unique(&mut seen, &unit.identity)?;
        let decision = batches
            .get_mut(unit.decision_index)
            .ok_or(ResultStoragePlanError::InvalidCommitDecisionSequence)?;
        decision.1.push(commit_translation_parameters(unit));
    }
    let mut steps = Vec::with_capacity(batches.len());
    for (translation, parameter_sets) in batches {
        if parameter_sets.is_empty() {
            return Err(ResultStoragePlanError::MissingCommitDecisionUnit);
        }
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::with_shared_parameters(
                COMMIT_TRANSLATION,
                vec![text(translation)],
                parameter_sets,
            ),
        ));
    }
    Ok(SqliteTransactionPlan::new(steps))
}

fn encode_identity(
    identity: &TranslationUnitIdentity,
) -> Result<EncodedIdentity, ResultStoragePlanError> {
    Ok(EncodedIdentity {
        owner: identity.owner().storage_name(),
        group_location: RpgMakerLocationCodec::encode(identity.group_location())
            .map_err(ResultStoragePlanError::Location)?,
        unit_role: RpgMakerProjectionCodec::encode_role(identity.role())
            .map_err(ResultStoragePlanError::Projection)?,
        source_content_json: encode_content(identity.source_content())?,
        source_context_json: identity.source_context_json().to_owned(),
    })
}

fn encode_content(content: &TextUnitContent) -> Result<String, ResultStoragePlanError> {
    serde_json::to_string(content).map_err(ResultStoragePlanError::Content)
}

fn ensure_unique(
    seen: &mut HashSet<(&'static str, String, String)>,
    identity: &EncodedIdentity,
) -> Result<(), ResultStoragePlanError> {
    if seen.insert((
        identity.owner,
        identity.group_location.clone(),
        identity.unit_role.clone(),
    )) {
        Ok(())
    } else {
        Err(ResultStoragePlanError::DuplicateUnit)
    }
}

fn ensure_nonblank(translation: &TextUnitContent) -> Result<(), ResultStoragePlanError> {
    if translation.is_blank() {
        Err(ResultStoragePlanError::BlankTranslation)
    } else {
        Ok(())
    }
}

fn require_snapshot_baseline(baseline: &TranslationSnapshotBaseline) -> SqliteTransactionStep {
    let mut parameters = vec![SqliteValue::Blob(
        baseline.source_snapshot_fingerprint().as_bytes().to_vec(),
    )];
    let owner_condition = if baseline.owner_snapshots().is_empty() {
        "(SELECT COUNT(*) FROM rpg_maker_asset_owner_state) <> 0".to_owned()
    } else {
        let clauses = baseline
            .owner_snapshots()
            .iter()
            .map(|snapshot| {
                parameters.push(text(snapshot.owner().storage_name()));
                parameters.push(SqliteValue::Blob(
                    snapshot.source_snapshot_fingerprint().as_bytes().to_vec(),
                ));
                parameters.push(SqliteValue::Blob(
                    snapshot.asset_snapshot_fingerprint().as_bytes().to_vec(),
                ));
                "(owner = ? AND source_snapshot_fingerprint = ? AND asset_snapshot_fingerprint = ?)"
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        format!(
            "(SELECT COUNT(*) FROM rpg_maker_asset_owner_state) <> {} OR EXISTS (SELECT 1 FROM rpg_maker_asset_owner_state WHERE NOT ({clauses}))",
            baseline.owner_snapshots().len()
        )
    };
    parameters.extend([
        text(TERMINOLOGY_RESOURCE_KIND),
        text(baseline.terminology_json()),
        text(PLACEHOLDER_RULES_RESOURCE_KIND),
        text(baseline.placeholder_rules_json()),
    ]);

    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        format!(
            "SELECT 1 WHERE (SELECT COUNT(*) FROM metadata) <> 1 OR NOT EXISTS (SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?) OR {owner_condition} OR (SELECT COUNT(*) FROM rpg_maker_translation_resource) <> 2 OR NOT EXISTS (SELECT 1 FROM rpg_maker_translation_resource WHERE resource_kind = ? AND canonical_json = ?) OR NOT EXISTS (SELECT 1 FROM rpg_maker_translation_resource WHERE resource_kind = ? AND canonical_json = ?)"
        ),
        parameters,
    ))
}

const REQUIRE_SNAPSHOT: &str = "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM rpg_maker_text_unit WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND ((?6 IS NULL AND ?7 IS NULL AND translation_content_json IS NULL AND translation_state IS NULL) OR (translation_content_json = ?6 AND translation_state = ?7)))";

const CLEAR_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = NULL, translation_state = NULL WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND translation_content_json = ?6 AND translation_state = ?7";

const WRITE_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_location = ?4 AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND (translation_content_json = ?8 OR (translation_content_json IS NULL AND ?8 IS NULL)) AND (translation_state = ?9 OR (translation_state IS NULL AND ?9 IS NULL))";

const COMMIT_TRANSLATION: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_location = ?4 AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND translation_content_json IS NULL AND translation_state IS NULL";

fn snapshot_parameters(
    identity: &EncodedIdentity,
    expected_translation: Option<(&str, Sha256Fingerprint)>,
) -> Vec<SqliteValue> {
    let mut parameters = vec![
        text(identity.owner),
        text(identity.group_location.clone()),
        text(identity.unit_role.clone()),
        text(identity.source_content_json.clone()),
        text(identity.source_context_json.clone()),
    ];
    if let Some((translation, state)) = expected_translation {
        parameters.push(text(translation));
        parameters.push(blob(state));
    } else {
        parameters.extend([SqliteValue::Null, SqliteValue::Null]);
    }
    parameters
}

fn clear_translation_parameters(
    identity: EncodedIdentity,
    expected_translation: String,
    expected_translation_state: Sha256Fingerprint,
) -> Vec<SqliteValue> {
    vec![
        text(identity.owner),
        text(identity.group_location),
        text(identity.unit_role),
        text(identity.source_content_json),
        text(identity.source_context_json),
        text(expected_translation),
        blob(expected_translation_state),
    ]
}

fn write_translation_from_snapshot_parameters(
    identity: EncodedIdentity,
    translation: String,
    replacement_translation_state: Sha256Fingerprint,
    expected_translation: Option<String>,
    expected_translation_state: Option<Sha256Fingerprint>,
) -> Vec<SqliteValue> {
    vec![
        text(translation),
        blob(replacement_translation_state),
        text(identity.owner),
        text(identity.group_location),
        text(identity.unit_role),
        text(identity.source_content_json),
        text(identity.source_context_json),
        expected_translation.map_or(SqliteValue::Null, text),
        expected_translation_state.map_or(SqliteValue::Null, blob),
    ]
}

fn commit_translation_parameters(unit: EncodedCommitUnit) -> Vec<SqliteValue> {
    vec![
        blob(unit.translation_state),
        text(unit.identity.owner),
        text(unit.identity.group_location),
        text(unit.identity.unit_role),
        text(unit.identity.source_content_json),
        text(unit.identity.source_context_json),
    ]
}

fn resource_updates(
    terminology_json: String,
    placeholder_rules_json: String,
) -> [SqliteTransactionStep; 2] {
    [
        update_resource(TERMINOLOGY_RESOURCE_KIND, terminology_json),
        update_resource(PLACEHOLDER_RULES_RESOURCE_KIND, placeholder_rules_json),
    ]
}

fn update_resource(kind: &'static str, canonical_json: String) -> SqliteTransactionStep {
    execute(
        "UPDATE rpg_maker_translation_resource SET canonical_json = ?1 WHERE resource_kind = ?2 AND canonical_json <> ?1",
        vec![text(canonical_json), text(kind)],
    )
}

fn execute(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn blob(value: Sha256Fingerprint) -> SqliteValue {
    SqliteValue::Blob(value.as_bytes().to_vec())
}

fn map_session_open_error<S, C>(
    database_path: PathBuf,
    error: OpenSqliteTransactionSessionError<S>,
) -> RpgMakerTranslationResultStorageError<S, C> {
    match error {
        OpenSqliteTransactionSessionError::NotFound => {
            RpgMakerTranslationResultStorageError::DatabaseNotFound { database_path }
        }
        OpenSqliteTransactionSessionError::OpenFailed(source) => {
            RpgMakerTranslationResultStorageError::NotCommitted {
                database_path,
                source,
            }
        }
    }
}

fn map_transaction_error<S, C>(
    database_path: PathBuf,
    error: ExecuteTransactionError<S>,
) -> RpgMakerTranslationResultStorageError<S, C> {
    match error {
        ExecuteTransactionError::NotFound => {
            RpgMakerTranslationResultStorageError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed
        | ExecuteTransactionError::RequirementFailedWithRow { .. } => {
            RpgMakerTranslationResultStorageError::StalePlan { database_path }
        }
        ExecuteTransactionError::RequirementFailedWithRowOutcomeUnknown { source, .. } => {
            RpgMakerTranslationResultStorageError::OutcomeUnknown {
                database_path,
                source: *source,
            }
        }
        ExecuteTransactionError::NotCommitted(source) => {
            RpgMakerTranslationResultStorageError::NotCommitted {
                database_path,
                source,
            }
        }
        ExecuteTransactionError::OutcomeUnknown(source) => {
            RpgMakerTranslationResultStorageError::OutcomeUnknown {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::{Future, ready};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::project_name::ProjectName;
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::runtime::sqlite::{RusqliteStorage, RusqliteStorageConfiguration};
    use crate::storage::sqlite::SqliteTransactionStep;
    use crate::storage::sqlite_session::{
        SqliteInteractiveSessionFinalization, SqliteInteractiveSessionFinalizationError,
    };
    use crate::storage::sqlite_transaction_session::OpenedSqliteTransactionSession;
    use rusqlite::{Connection, params};

    use super::*;
    use crate::rpg_maker::translate::executor::FinalLlmResponseMetadata;
    use crate::rpg_maker::translate::pipeline::{
        AcceptedTranslationDecision, NonEmptyTaskItems, RpgMakerTranslationTaskIndex,
        TranslationInvalidation, TranslationOwnerSnapshot, TranslationPatch,
        TranslationPropagationTarget, TranslationReuse, TranslationReuseSeed,
        TranslationReuseTarget, TranslationSnapshotBaseline, TranslationStateContext,
        TranslationTaskOutcomeContext,
    };

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    #[test]
    fn closed_result_storage_plan_invariants_keep_stable_safe_facts() {
        let cases = [
            (
                ResultStoragePlanError::EmptyTaskResult,
                DiagnosticSubject::component("translation_task_result"),
                "plan_error=empty_task_result",
            ),
            (
                ResultStoragePlanError::EmptyReuseTargets,
                DiagnosticSubject::component("translation_reuse_plan"),
                "plan_error=empty_reuse_targets",
            ),
            (
                ResultStoragePlanError::BlankTranslation,
                DiagnosticSubject::field("translation"),
                "plan_error=blank_translation",
            ),
            (
                ResultStoragePlanError::InconsistentTranslationState,
                DiagnosticSubject::field("translation_state"),
                "plan_error=inconsistent_translation_state",
            ),
            (
                ResultStoragePlanError::MismatchedReuseSourceContent,
                DiagnosticSubject::field("source_content"),
                "plan_error=mismatched_reuse_source_content",
            ),
            (
                ResultStoragePlanError::MismatchedReuseSourceContext,
                DiagnosticSubject::field("source_context"),
                "plan_error=mismatched_reuse_source_context",
            ),
            (
                ResultStoragePlanError::MismatchedPropagationSourceContent,
                DiagnosticSubject::field("source_content"),
                "plan_error=mismatched_propagation_source_content",
            ),
            (
                ResultStoragePlanError::MismatchedPropagationSourceContext,
                DiagnosticSubject::field("source_context"),
                "plan_error=mismatched_propagation_source_context",
            ),
            (
                ResultStoragePlanError::DuplicateUnit,
                DiagnosticSubject::component("translation_commit_units"),
                "plan_error=duplicate_unit",
            ),
            (
                ResultStoragePlanError::InvalidCommitDecisionSequence,
                DiagnosticSubject::component("translation_commit_decisions"),
                "plan_error=invalid_commit_decision_sequence",
            ),
            (
                ResultStoragePlanError::MissingCommitDecisionUnit,
                DiagnosticSubject::component("translation_commit_decisions"),
                "plan_error=missing_commit_decision_unit",
            ),
        ];

        for (source, expected_subject, expected_detail) in cases {
            let diagnostic = source.safe_diagnostic();
            assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
            assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
            assert_eq!(diagnostic.subject, expected_subject);
            assert_eq!(
                diagnostic.reason,
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    expected_detail,
                )
            );
            assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
            assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        }
    }

    #[test]
    fn result_storage_plan_codecs_keep_typed_facts_without_copying_source_text() {
        const SOURCE_BODY: &str = "SENTINEL_RESULT_STORAGE_BODY_8ebfa3";

        let invalid_json = format!("{{\"{SOURCE_BODY}\":");
        let json_error =
            serde_json::from_str::<serde_json::Value>(&invalid_json).expect_err("JSON 应不完整");
        let content_diagnostic = ResultStoragePlanError::Content(json_error).safe_diagnostic();
        let content_json = serde_json::to_string(&content_diagnostic).expect("公开诊断应可序列化");
        assert!(!content_json.contains(SOURCE_BODY));
        assert!(content_json.contains("operation=content_encode"));
        assert!(content_json.contains("json_category=eof"));
        assert!(content_json.contains("json_line=1"));
        assert!(content_json.contains("json_column="));

        let location_diagnostic = ResultStoragePlanError::Location(
            RpgMakerLocationCodecError::InvalidDataFile(SOURCE_BODY.to_owned()),
        )
        .safe_diagnostic();
        let location_json =
            serde_json::to_string(&location_diagnostic).expect("公开诊断应可序列化");
        assert!(!location_json.contains(SOURCE_BODY));
        assert!(location_json.contains("kind=invalid_data_file"));

        let projection_diagnostic =
            ResultStoragePlanError::Projection(RpgMakerProjectionCodecError::Projection(
                crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                    expected: 2,
                    actual: 4,
                },
            ))
            .safe_diagnostic();
        let projection_json =
            serde_json::to_string(&projection_diagnostic).expect("公开诊断应可序列化");
        assert!(projection_json.contains("kind=invalid_projection"));
        assert!(projection_json.contains("expected=2"));
        assert!(projection_json.contains("actual=4"));
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

    #[derive(Clone, Default)]
    struct RecordingSqlite {
        plans: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        opens: Arc<AtomicUsize>,
        finalizations: Arc<AtomicUsize>,
        transaction_result: RecordingTransactionResult,
    }

    #[derive(Clone, Copy, Default)]
    enum RecordingTransactionResult {
        #[default]
        Committed,
        NotApplied,
        OutcomeUnknown,
    }

    struct RecordingOperations {
        path: PathBuf,
        plans: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        transaction_result: RecordingTransactionResult,
    }

    impl SqliteTransactionSessionOperations for RecordingOperations {
        type Error = FakeError;

        fn execute_transaction(
            &self,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            self.plans
                .lock()
                .expect("事务锁")
                .push((self.path.clone(), plan));
            ready(match self.transaction_result {
                RecordingTransactionResult::Committed => Ok(()),
                RecordingTransactionResult::NotApplied => {
                    Err(ExecuteTransactionError::NotCommitted(FakeError))
                }
                RecordingTransactionResult::OutcomeUnknown => {
                    Err(ExecuteTransactionError::OutcomeUnknown(FakeError))
                }
            })
        }
    }

    struct RecordingFinalizer {
        finalizations: Arc<AtomicUsize>,
    }

    impl SqliteInteractiveSessionFinalizer for RecordingFinalizer {
        type Error = FakeError;

        fn finalize(
            self,
        ) -> impl Future<
            Output = Result<
                SqliteInteractiveSessionFinalization,
                SqliteInteractiveSessionFinalizationError<Self::Error>,
            >,
        > + Send {
            self.finalizations.fetch_add(1, Ordering::SeqCst);
            ready(Ok(SqliteInteractiveSessionFinalization::new(false)))
        }
    }

    impl SqliteTransactionSessionFactory for RecordingSqlite {
        type Operations = RecordingOperations;
        type Finalizer = RecordingFinalizer;
        type Error = FakeError;

        fn open_existing_transaction_session(
            &self,
            path: PathBuf,
        ) -> impl Future<
            Output = Result<
                OpenedSqliteTransactionSession<Self::Operations, Self::Finalizer>,
                OpenSqliteTransactionSessionError<Self::Error>,
            >,
        > + Send {
            self.opens.fetch_add(1, Ordering::SeqCst);
            ready(Ok(OpenedSqliteTransactionSession::new(
                Arc::new(RecordingOperations {
                    path,
                    plans: Arc::clone(&self.plans),
                    transaction_result: self.transaction_result,
                }),
                RecordingFinalizer {
                    finalizations: Arc::clone(&self.finalizations),
                },
            )))
        }
    }

    #[tokio::test]
    async fn preparation_and_task_commits_share_one_explicitly_finalized_session() {
        let sqlite = RecordingSqlite::default();
        let opens = Arc::clone(&sqlite.opens);
        let finalizations = Arc::clone(&sqlite.finalizations);
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            1,
            0,
            0,
        );
        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("准备应建立会话");

        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x42);
        let translation = value("译文");
        for task_index in 0..2 {
            let outcome = Arc::new(TranslationTaskOutcome::Complete {
                context: TranslationTaskOutcomeContext::new(
                    RpgMakerTranslationTaskIndex::new(task_index),
                    NonZeroUsize::MIN,
                    Vec::new(),
                ),
                final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
                accepted: NonEmptyTaskItems::new(
                    AcceptedTranslationDecision::new(
                        0,
                        TranslationPatch::new(
                            identity.clone(),
                            Vec::new(),
                            translation.clone(),
                            context.finish(&translation),
                        ),
                    ),
                    Vec::new(),
                ),
            });
            let prepared = service.prepare_commit(outcome).await.expect("提交应可编码");
            service
                .commit_prepared(&project(), prepared)
                .await
                .expect("任务应提交到同一会话");
        }

        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(plans.lock().expect("事务锁").len(), 3);
        assert_eq!(finalizations.load(Ordering::SeqCst), 0);
        service.finalize().await.expect("会话应显式终结");
        assert_eq!(finalizations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prepared_commit_uses_one_conditional_batch_update_for_all_unit_checks() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x11);
        let translation = value("译文");
        let outcome = Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    0,
                    TranslationPatch::new(
                        identity.clone(),
                        Vec::new(),
                        translation.clone(),
                        context.finish(&translation),
                    ),
                ),
                Vec::new(),
            ),
        });

        let prepared = service
            .prepare_commit(Arc::clone(&outcome))
            .await
            .expect("提交准备应成功");
        assert_eq!(Arc::strong_count(&outcome), 1);
        assert!(plans.lock().expect("事务锁").is_empty());
        service
            .commit_prepared(&project(), prepared)
            .await
            .expect("提交应成功");

        let plans = plans.lock().expect("事务锁");
        let plan = &plans[0].1;
        assert_eq!(plan.steps().len(), 1);
        let SqliteTransactionStep::ExecuteManyExactlyOne(update) = &plan.steps()[0] else {
            panic!("提交应以一条条件批量修改核对并写入逻辑单元");
        };
        assert_eq!(update.statement(), COMMIT_TRANSLATION);
        assert!(update.statement().contains("source_content_json = ?6"));
        assert!(update.statement().contains("source_context_json = ?7"));
        assert!(
            update
                .statement()
                .contains("translation_content_json IS NULL")
        );
        assert!(update.statement().contains("translation_state IS NULL"));
        assert!(!update.statement().contains("exact_location"));
        assert_eq!(
            update.shared_parameters(),
            &[SqliteValue::Text(r#""译文""#.to_owned())]
        );
        let parameters = update.parameter_rows().next().expect("应包含一组提交参数");
        assert_eq!(
            parameters[2],
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(identity.group_location()).expect("组位置应可编码")
            )
        );
        assert_eq!(
            parameters[3],
            SqliteValue::Text(
                RpgMakerProjectionCodec::encode_role(identity.role()).expect("角色应可编码")
            )
        );
        assert_eq!(parameters[4], SqliteValue::Text(r#""原文""#.to_owned()));
        assert_eq!(parameters[5], SqliteValue::Text("{}".to_owned()));
    }

    #[tokio::test]
    async fn commit_preparation_failure_keeps_its_typed_diagnostic() {
        let service =
            RpgMakerTranslationResultStorageService::new(RecordingSqlite::default(), InlineCpu);
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x31);
        let translation = value("译文");
        let patch = TranslationPatch::new(
            identity,
            Vec::new(),
            translation.clone(),
            context.finish(&translation),
        );

        let failure = match service
            .prepare_commit(complete_outcome(vec![patch.clone(), patch]))
            .await
        {
            Ok(_) => panic!("重复逻辑单元必须在提交准备阶段失败"),
            Err(failure) => failure,
        };
        let (source, impact, diagnostic) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerTranslationResultStorageError::InvalidPlan(
                ResultStoragePlanError::DuplicateUnit
            )
        ));
        assert_eq!(impact, TranslationTaskCommitFailureImpact::NotApplied);
        let diagnostic = diagnostic.expect("提交准备失败必须携带既有结构化诊断");
        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::component("translation_commit_units")
        );
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                "plan_error=duplicate_unit",
            )
        );
        assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
    }

    #[tokio::test]
    async fn definitely_not_applied_commit_keeps_database_diagnostic() {
        let sqlite = RecordingSqlite {
            transaction_result: RecordingTransactionResult::NotApplied,
            ..RecordingSqlite::default()
        };
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let expected_project = project();
        let prepared = service
            .prepare_commit(complete_outcome(vec![translation_patch(
                scalar_identity(1, "name", "原文", "{}"),
                "译文",
                0x41,
            )]))
            .await
            .expect("提交计划应可编码");

        let failure = service
            .commit_prepared(&expected_project, prepared)
            .await
            .expect_err("测试事务必须明确报告未应用");
        let (source, impact, diagnostic) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerTranslationResultStorageError::NotCommitted { .. }
        ));
        assert_eq!(impact, TranslationTaskCommitFailureImpact::NotApplied);
        let diagnostic = diagnostic.expect("确定未应用必须携带数据库结构化诊断");
        assert_eq!(diagnostic.code, DiagnosticCode::SqliteOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::path(expected_project.database_path())
        );
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::failure(DiagnosticFailureKind::TransactionRolledBack)
        );
        assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
        assert_eq!(
            diagnostic.recovery,
            vec![RecoveryFact::transaction("rolled_back")]
        );
        assert!(
            !serde_json::to_string(&diagnostic)
                .expect("提交诊断应可序列化")
                .contains("fake"),
            "任务提交诊断不得解析或复制底层 Display 文本"
        );
    }

    #[tokio::test]
    async fn outcome_unknown_commit_keeps_database_diagnostic() {
        let sqlite = RecordingSqlite {
            transaction_result: RecordingTransactionResult::OutcomeUnknown,
            ..RecordingSqlite::default()
        };
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let expected_project = project();
        let prepared = service
            .prepare_commit(complete_outcome(vec![translation_patch(
                scalar_identity(1, "name", "原文", "{}"),
                "译文",
                0x51,
            )]))
            .await
            .expect("提交计划应可编码");

        let failure = service
            .commit_prepared(&expected_project, prepared)
            .await
            .expect_err("测试事务必须报告终态未知");
        let (source, impact, diagnostic) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerTranslationResultStorageError::OutcomeUnknown { .. }
        ));
        assert_eq!(impact, TranslationTaskCommitFailureImpact::OutcomeUnknown);
        let diagnostic = diagnostic.expect("终态未知必须携带数据库结构化诊断");
        assert_eq!(diagnostic.code, DiagnosticCode::SqliteOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::path(expected_project.database_path())
        );
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::failure(DiagnosticFailureKind::TransactionOutcomeUnknown)
        );
        assert_eq!(diagnostic.impact, DiagnosticImpact::OutcomeUnknown);
        assert_eq!(
            diagnostic.recovery,
            vec![RecoveryFact::transaction("outcome_unknown")]
        );
        assert!(
            !serde_json::to_string(&diagnostic)
                .expect("提交诊断应可序列化")
                .contains("fake"),
            "任务提交诊断不得解析或复制底层 Display 文本"
        );
    }

    #[test]
    fn huge_propagation_family_encodes_and_owns_translation_once_per_decision() {
        const TARGETS: usize = 50_000;

        let representative = scalar_identity(0, "name", "共同原文", "{}");
        let propagation_targets = (1..=TARGETS)
            .map(|index| {
                TranslationPropagationTarget::new(
                    scalar_identity(index, "name", "共同原文", "{}"),
                    state_context(u8::try_from(index % 251).expect("余数应可表示为 u8")),
                )
            })
            .collect();
        let translation = value("超大全族共享译文");
        let outcome = complete_outcome(vec![TranslationPatch::new(
            representative,
            propagation_targets,
            translation.clone(),
            state_context(0x5a).finish(&translation),
        )]);
        let work = commit_work(outcome).expect("超大全族应可建立提交工作");
        assert_eq!(work.decisions.len(), 1);
        assert_eq!(work.units.len(), TARGETS + 1);

        let encoding_count = Cell::new(0_usize);
        let decisions = work
            .decisions
            .into_iter()
            .map(|work| {
                encode_commit_decision_with(work, |translation| {
                    encoding_count.set(encoding_count.get() + 1);
                    encode_content(translation)
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("共享译文应可编码");
        let units = work
            .units
            .into_iter()
            .map(encode_commit_unit)
            .collect::<Result<Vec<_>, _>>()
            .expect("全部传播目标身份应可并行编码");
        assert_eq!(encoding_count.get(), 1, "每个 decision 只能编码一次译文");

        let plan = finish_commit_plan(decisions, units).expect("超大全族应形成原子提交计划");
        assert_eq!(plan.steps().len(), 1);
        let SqliteTransactionStep::ExecuteManyExactlyOne(batch) = &plan.steps()[0] else {
            panic!("一个 decision 应形成一个精确共享参数批次");
        };
        let encoded_translation = SqliteValue::Text(r#""超大全族共享译文""#.to_owned());
        assert_eq!(
            batch.shared_parameters(),
            std::slice::from_ref(&encoded_translation)
        );
        assert_eq!(batch.parameter_set_count(), TARGETS + 1);
        assert!(batch.parameter_rows().all(|parameters| {
            parameters.len() == 6 && !parameters.contains(&encoded_translation)
        }));
    }

    #[test]
    fn commit_batches_preserve_decision_and_target_natural_order() {
        let first_representative = scalar_identity(1, "name", "共同原文", "{}");
        let first_target = scalar_identity(2, "name", "共同原文", "{}");
        let first_translation = value("第一条译文");
        let second_representative = scalar_identity(3, "name", "另一原文", "{}");
        let second_translation = value("第二条译文");
        let outcome = complete_outcome(vec![
            TranslationPatch::new(
                first_representative,
                vec![TranslationPropagationTarget::new(
                    first_target,
                    state_context(0x12),
                )],
                first_translation.clone(),
                state_context(0x11).finish(&first_translation),
            ),
            TranslationPatch::new(
                second_representative,
                Vec::new(),
                second_translation.clone(),
                state_context(0x21).finish(&second_translation),
            ),
        ]);
        let work = commit_work(outcome).expect("提交工作应保持任务自然顺序");
        let decisions = work
            .decisions
            .into_iter()
            .map(encode_commit_decision)
            .collect::<Result<Vec<_>, _>>()
            .expect("decision 译文应可编码");
        let units = work
            .units
            .into_iter()
            .map(encode_commit_unit)
            .collect::<Result<Vec<_>, _>>()
            .expect("提交目标应可编码");

        let plan = finish_commit_plan(decisions, units).expect("提交计划应可建立");
        assert_eq!(plan.steps().len(), 2);
        let SqliteTransactionStep::ExecuteManyExactlyOne(first) = &plan.steps()[0] else {
            panic!("第一条 decision 应形成第一批提交");
        };
        let SqliteTransactionStep::ExecuteManyExactlyOne(second) = &plan.steps()[1] else {
            panic!("第二条 decision 应形成第二批提交");
        };
        assert_eq!(
            first.shared_parameters(),
            &[SqliteValue::Text(r#""第一条译文""#.to_owned())]
        );
        assert_eq!(
            second.shared_parameters(),
            &[SqliteValue::Text(r#""第二条译文""#.to_owned())]
        );
        assert_eq!(first.parameter_set_count(), 2);
        assert_eq!(second.parameter_set_count(), 1);
        let first_parameters = first.parameter_rows().collect::<Vec<_>>();
        assert_eq!(
            first_parameters[0][2],
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(&data_group(1)).expect("代表单元位置应可编码")
            )
        );
        assert_eq!(
            first_parameters[1][2],
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(&data_group(2)).expect("传播目标位置应可编码")
            )
        );
    }

    #[tokio::test]
    async fn preparation_uses_updates_as_cas_and_only_guards_read_only_seeds() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let invalidation_one = scalar_identity(1, "name", "原文一", "{}");
        let invalidation_two = scalar_identity(2, "name", "原文二", "{}");
        let reuse_seed = scalar_identity(3, "name", "共享原文", "{}");
        let reuse_target = scalar_identity(4, "name", "共享原文", "{}");
        let preparation = TranslationPlanPreparation::new(
            vec![
                TranslationInvalidation::new(
                    invalidation_one.clone(),
                    value("旧译文一"),
                    Sha256Fingerprint::from_bytes([0x11; 32]),
                ),
                TranslationInvalidation::new(
                    invalidation_two.clone(),
                    value("旧译文二"),
                    Sha256Fingerprint::from_bytes([0x22; 32]),
                ),
            ],
            vec![TranslationReuse::new(
                TranslationReuseSeed::new(
                    reuse_seed.clone(),
                    value("复用译文"),
                    Sha256Fingerprint::from_bytes([0x33; 32]),
                ),
                vec![TranslationReuseTarget::new(
                    reuse_target.clone(),
                    None,
                    None,
                    Sha256Fingerprint::from_bytes([0x44; 32]),
                )],
            )],
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            2,
            0,
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("准备事务应成功");

        let plans = plans.lock().expect("事务锁");
        let steps = plans[0].1.steps();
        assert!(matches!(steps[0], SqliteTransactionStep::RequireNoRows(_)));
        let SqliteTransactionStep::RequireNoRowsMany(guards) = &steps[1] else {
            panic!("只有不写入的复用种子需要独立快照查询");
        };
        assert_eq!(guards.statement(), REQUIRE_SNAPSHOT);
        assert_eq!(guards.parameter_set_count(), 1);
        let expected_group = SqliteValue::Text(
            RpgMakerLocationCodec::encode(reuse_seed.group_location()).expect("位置应可编码"),
        );
        let guard_parameters = guards
            .parameter_rows()
            .next()
            .expect("应包含一组快照核对参数");
        assert_eq!(
            guard_parameters[1], expected_group,
            "会被条件 UPDATE 核对的失效项和复用目标不得先重复查询"
        );

        let SqliteTransactionStep::ExecuteManyExactlyOne(clears) = &steps[2] else {
            panic!("失效清理应批量条件修改");
        };
        assert_eq!(clears.statement(), CLEAR_TRANSLATION_FROM_SNAPSHOT);
        assert_eq!(clears.parameter_set_count(), 2);
        let clear_parameters = clears.parameter_rows().next().expect("应包含失效清理参数");
        assert_eq!(
            clear_parameters[5],
            SqliteValue::Text(r#""旧译文一""#.to_owned())
        );

        let SqliteTransactionStep::ExecuteManyExactlyOne(reuses) = &steps[3] else {
            panic!("复用写入应批量条件修改");
        };
        assert_eq!(reuses.statement(), WRITE_TRANSLATION_FROM_SNAPSHOT);
        assert_eq!(reuses.parameter_set_count(), 1);
        let reuse_parameters = reuses.parameter_rows().next().expect("应包含复用写入参数");
        assert_eq!(
            reuse_parameters[0],
            SqliteValue::Text(r#""复用译文""#.to_owned())
        );
        assert_eq!(reuse_parameters[7], SqliteValue::Null);
        assert_eq!(reuse_parameters[8], SqliteValue::Null);
    }

    #[tokio::test]
    async fn preparation_without_writes_still_checks_the_complete_snapshot_baseline() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            1,
            0,
            0,
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("无状态写入的计划仍必须在请求模型前核对读取基线");

        let plans = plans.lock().expect("事务锁");
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].1.steps().is_empty());
        assert!(matches!(
            plans[0].1.steps()[0],
            SqliteTransactionStep::RequireNoRows(_)
        ));
    }

    #[test]
    fn duplicate_logical_unit_is_rejected_before_a_commit_plan_is_created() {
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x55);
        let translation = value("译文");
        let patch = TranslationPatch::new(
            identity,
            Vec::new(),
            translation.clone(),
            context.finish(&translation),
        );
        let work = commit_work(complete_outcome(vec![patch.clone(), patch]))
            .expect("重复应由最终计划阶段按编码身份识别");
        let decisions = work
            .decisions
            .into_iter()
            .map(encode_commit_decision)
            .collect::<Result<Vec<_>, _>>()
            .expect("decision 译文本身应可编码");
        let encoded = work
            .units
            .into_iter()
            .map(encode_commit_unit)
            .collect::<Result<Vec<_>, _>>()
            .expect("重复单元本身应可编码");

        let error = finish_commit_plan(decisions, encoded).expect_err("重复逻辑单元不得进入事务");
        assert!(matches!(error, ResultStoragePlanError::DuplicateUnit));
    }

    #[tokio::test]
    async fn real_sqlite_rejects_stale_source_context_and_translation_state() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let storage = runtime_storage();
        let cases = [
            ("stale-source.db", "旧原文", "{}", None, None),
            ("stale-context.db", "原文", r#"{"old":true}"#, None, None),
            (
                "stale-translation.db",
                "原文",
                "{}",
                Some("旧译文"),
                Some(vec![0x61; 32]),
            ),
            ("orphan-state.db", "原文", "{}", None, Some(vec![0x62; 32])),
        ];

        for (name, actual_source, actual_context, translation, translation_state) in cases {
            let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
            let database_path = directory.path().join(name).join("project.db");
            let identity = scalar_identity(1, "name", "原文", "{}");
            create_unit_database(
                &database_path,
                &[StoredUnit::new(
                    &identity,
                    actual_source,
                    actual_context,
                    translation,
                    translation_state.as_deref(),
                )],
            );
            let prepared = service
                .prepare_commit(complete_outcome(vec![translation_patch(
                    identity, "译文", 0x71,
                )]))
                .await
                .expect("提交计划编码应成功");
            let error = service
                .commit_prepared(&project_at(database_path.clone()), prepared)
                .await
                .expect_err("过时快照必须拒绝");
            assert!(matches!(
                error.source(),
                RpgMakerTranslationResultStorageError::StalePlan { .. }
            ));
            assert_eq!(
                stored_translation(&database_path, 1),
                (translation.map(value), translation_state,),
                "被拒绝的条件 UPDATE 不得改变行"
            );
            service.finalize().await.expect("测试数据库会话应正常终结");
        }

        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_rolls_back_earlier_batch_updates_when_a_later_unit_is_stale() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("rollback").join("project.db");
        let first = scalar_identity(1, "name", "原文一", "{}");
        let second = scalar_identity(2, "name", "原文二", "{}");
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(&first, "原文一", "{}", None, None),
                StoredUnit::new(&second, "原文二", r#"{"old":true}"#, None, None),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let prepared = service
            .prepare_commit(complete_outcome(vec![
                translation_patch(first, "译文一", 0x81),
                translation_patch(second, "译文二", 0x82),
            ]))
            .await
            .expect("提交计划编码应成功");

        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("后序单元过时必须回滚整笔任务");
        assert!(matches!(
            error.source(),
            RpgMakerTranslationResultStorageError::StalePlan { .. }
        ));
        assert_eq!(stored_translation(&database_path, 1), (None, None));
        assert_eq!(stored_translation(&database_path, 2), (None, None));

        drop(service);
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_rejects_duplicate_physical_rows_and_rolls_back_them_both() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("duplicate-rows").join("project.db");
        let identity = scalar_identity(1, "name", "原文", "{}");
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(&identity, "原文", "{}", None, None),
                StoredUnit::new(&identity, "原文", "{}", None, None),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let prepared = service
            .prepare_commit(complete_outcome(vec![translation_patch(
                identity, "译文", 0x91,
            )]))
            .await
            .expect("提交计划编码应成功");

        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("修改多行必须视为快照失效");
        assert!(matches!(
            error.source(),
            RpgMakerTranslationResultStorageError::StalePlan { .. }
        ));
        let connection = Connection::open(&database_path).expect("数据库应可重开");
        let translated: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM rpg_maker_text_unit WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("应可核对回滚结果");
        assert_eq!(translated, 0);

        drop(connection);
        drop(service);
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[test]
    fn body_propagation_rejects_a_different_source_speaker_context() {
        let leader = dialogue_body_identity(1, "同一句", r#"{"source_speaker":"甲"}"#);
        let target = dialogue_body_identity(2, "同一句", r#"{"source_speaker":"乙"}"#);
        let context = state_context(0x22);
        let translation = lines(&["相同译文"]);
        let result = complete_outcome(vec![TranslationPatch::new(
            leader,
            vec![TranslationPropagationTarget::new(target, context)],
            translation.clone(),
            context.finish(&translation),
        )]);

        let work = commit_work(result).expect("结果形状应合法");
        let error = work
            .units
            .into_iter()
            .map(encode_commit_unit)
            .find_map(Result::err)
            .expect("不同 Speaker 上下文不得传播");

        assert!(matches!(
            error,
            ResultStoragePlanError::MismatchedPropagationSourceContext
        ));
    }

    #[test]
    fn baseline_guard_includes_both_owner_fingerprints() {
        let baseline = TranslationSnapshotBaseline::new(
            SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            vec![TranslationOwnerSnapshot::new(
                RpgMakerAssetOwner::Builtin,
                SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
                AssetSnapshotFingerprint::from_bytes([0xb4; 32]),
            )],
            "[]".to_owned(),
            "[]".to_owned(),
        );

        let SqliteTransactionStep::RequireNoRows(query) = require_snapshot_baseline(&baseline)
        else {
            panic!("baseline 应生成核对查询");
        };

        assert!(query.statement().contains("asset_snapshot_fingerprint"));
        assert!(
            query
                .parameters()
                .contains(&SqliteValue::Blob(vec![0xb4; 32]))
        );
    }

    struct StoredUnit<'a> {
        identity: &'a TranslationUnitIdentity,
        source_content_json: String,
        source_context_json: &'a str,
        translation_content_json: Option<String>,
        translation_state: Option<&'a [u8]>,
    }

    impl<'a> StoredUnit<'a> {
        fn new(
            identity: &'a TranslationUnitIdentity,
            source_content: &str,
            source_context_json: &'a str,
            translation: Option<&'a str>,
            translation_state: Option<&'a [u8]>,
        ) -> Self {
            Self {
                identity,
                source_content_json: encode_content(&value(source_content))
                    .expect("测试源内容应可编码"),
                source_context_json,
                translation_content_json: translation
                    .map(value)
                    .as_ref()
                    .map(encode_content)
                    .transpose()
                    .expect("测试译文应可编码"),
                translation_state,
            }
        }
    }

    fn create_unit_database(path: &std::path::Path, units: &[StoredUnit<'_>]) {
        std::fs::create_dir_all(path.parent().expect("测试数据库必须有父目录"))
            .expect("测试数据库目录应可创建");
        let connection = Connection::open(path).expect("测试数据库应可创建");
        connection
            .execute_batch(
                "CREATE TABLE rpg_maker_text_unit (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    unit_role TEXT NOT NULL,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state BLOB
                );",
            )
            .expect("测试表应可创建");
        for unit in units {
            connection
                .execute(
                    "INSERT INTO rpg_maker_text_unit (
                        owner, group_location, unit_role, source_content_json,
                        source_context_json, translation_content_json, translation_state
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![
                        unit.identity.owner().storage_name(),
                        RpgMakerLocationCodec::encode(unit.identity.group_location())
                            .expect("组位置应可编码"),
                        RpgMakerProjectionCodec::encode_role(unit.identity.role())
                            .expect("单元角色应可编码"),
                        &unit.source_content_json,
                        unit.source_context_json,
                        &unit.translation_content_json,
                        unit.translation_state,
                    ],
                )
                .expect("测试单元应可写入");
        }
    }

    fn stored_translation(
        path: &std::path::Path,
        rowid: usize,
    ) -> (Option<TextUnitContent>, Option<Vec<u8>>) {
        Connection::open(path)
            .expect("数据库应可重开")
            .query_row(
                "SELECT translation_content_json, translation_state FROM rpg_maker_text_unit WHERE rowid = ?",
                [i64::try_from(rowid).expect("测试 rowid 应可表示为 i64")],
                |row| {
                    let translation_json: Option<String> = row.get(0)?;
                    Ok((
                        translation_json.map(|json| {
                            serde_json::from_str(&json).expect("测试译文 JSON 应可解码")
                        }),
                        row.get(1)?,
                    ))
                },
            )
            .expect("测试单元应仍存在")
    }

    fn translation_patch(
        identity: TranslationUnitIdentity,
        translation: &str,
        state_byte: u8,
    ) -> TranslationPatch {
        let translation = value(translation);
        TranslationPatch::new(
            identity,
            Vec::new(),
            translation.clone(),
            state_context(state_byte).finish(&translation),
        )
    }

    fn complete_outcome(patches: Vec<TranslationPatch>) -> Arc<TranslationTaskOutcome> {
        let mut decisions = patches
            .into_iter()
            .enumerate()
            .map(|(id, patch)| AcceptedTranslationDecision::new(id, patch));
        let first = decisions.next().expect("测试任务至少包含一项译文");
        Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(first, decisions.collect()),
        })
    }

    fn runtime_storage() -> RusqliteStorage {
        let nonzero = |value| NonZeroUsize::new(value).expect("测试资源预算必须非零");
        let config = RusqliteStorageConfiguration::new(nonzero(2), nonzero(4 * 1024 * 1024));
        RusqliteStorage::start(config).expect("测试 SQLite 根应可启动")
    }

    fn project_at(database_path: PathBuf) -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应合法"),
            database_path
                .parent()
                .expect("测试数据库必须有父目录")
                .to_path_buf(),
            database_path,
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn scalar_identity(
        index: usize,
        field: &str,
        source: &str,
        context: &str,
    ) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            data_group(index),
            TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("字段键应合法")),
            value(source),
            context,
        )
    }

    fn dialogue_body_identity(
        index: usize,
        source: &str,
        context: &str,
    ) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(
                RpgMakerSource::map(1),
                vec![
                    RpgMakerLocationStep::key("events"),
                    RpgMakerLocationStep::index(index),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(0),
                ],
            ),
            TextUnitRole::DialogueBody,
            lines(&[source]),
            context,
        )
    }

    fn value(text: &str) -> TextUnitContent {
        TextUnitContent::Value(text.to_owned())
    }

    fn lines(lines: &[&str]) -> TextUnitContent {
        TextUnitContent::Lines(lines.iter().map(|line| (*line).to_owned()).collect())
    }

    fn data_group(index: usize) -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(index)],
        )
    }

    fn state_context(byte: u8) -> TranslationStateContext {
        TranslationStateContext::new(Sha256Fingerprint::from_bytes([byte; 32]))
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
}
