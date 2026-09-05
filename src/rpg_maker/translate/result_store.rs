//! RPG Maker 译文状态的原子对账与提交。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, DiagnosticStage, RelatedFailureRelation, ReportedFailure,
    RpgMakerIssue, RpgMakerJsonFailureKind, RpgMakerResultStorePlanViolation,
    RpgMakerResultStoreProblem, RuntimeBoundaryOperation, RuntimeComponent, RuntimeIssue,
    RuntimeOperation, SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteOperation,
    SqliteTransactionState, StateEffect,
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
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::runtime::sqlite::SqliteRuntimeError;
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
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::ProvenInvariantViolation;

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
            let diagnostic = source.task_commit_diagnostic_report();
            TranslationTaskCommitFailure::not_applied(source, diagnostic)
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
                let diagnostic = source.task_commit_diagnostic_report();
                TranslationTaskCommitFailure::new(source, impact, diagnostic)
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
        let rejections = self
            .cpu
            .execute_ordered_map(work.rejections, encode_rejected_unit)
            .await
            .map_err(RpgMakerTranslationResultStorageError::ScheduleEncoding)?;

        self.cpu
            .execute(move || {
                Ok::<_, ResultStoragePlanError>(RpgMakerPreparedTranslationCommit {
                    plan: finish_commit_plan(
                        decisions.into_iter().collect::<Result<Vec<_>, _>>()?,
                        units.into_iter().collect::<Result<Vec<_>, _>>()?,
                        rejections.into_iter().collect::<Result<Vec<_>, _>>()?,
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

impl<S, C> RpgMakerTranslationResultStorageError<S, C>
where
    S: Error + Send + Sync + 'static,
    C: Error + Send + Sync + 'static,
{
    /// Task 仍在作用域内时建立原子提交诊断；生产根错误按具体类型保留 SQLite/CPU 事实。
    fn task_commit_diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::ScheduleEncoding(source) => {
                let issue = match source {
                    CpuTaskExecutionError::Cancelled => RuntimeIssue::Cancelled {
                        component: RuntimeComponent::CpuExecutor,
                        operation: RuntimeOperation::EncodeRpgMakerTranslationResult,
                    },
                    CpuTaskExecutionError::Unavailable(source) => {
                        match (source as &(dyn Error + 'static))
                            .downcast_ref::<CpuExecutorUnavailable>()
                        {
                            Some(CpuExecutorUnavailable::StatePoisoned) => {
                                RuntimeIssue::StatePoisoned {
                                    component: RuntimeComponent::CpuExecutor,
                                    operation: RuntimeOperation::EncodeRpgMakerTranslationResult,
                                }
                            }
                            Some(CpuExecutorUnavailable::ShuttingDown) | None => {
                                RuntimeIssue::ExecutorClosed {
                                    component: RuntimeComponent::CpuExecutor,
                                    operation: RuntimeOperation::EncodeRpgMakerTranslationResult,
                                }
                            }
                        }
                    }
                    CpuTaskExecutionError::TaskPanicked => RuntimeIssue::WorkerPanicked {
                        component: RuntimeComponent::CpuExecutor,
                        operation: RuntimeOperation::EncodeRpgMakerTranslationResult,
                    },
                };
                DiagnosticReport::new(StateEffect::ProgressPreserved, Diagnostic::runtime(issue))
            }
            Self::InvalidPlan(source) => source.diagnostic_report(),
            Self::DatabaseNotFound { database_path } => DiagnosticReport::new(
                StateEffect::ProgressPreserved,
                Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                    RpgMakerResultStoreProblem::DatabaseNotFound {
                        path: SafePath::new(database_path),
                    },
                )),
            ),
            Self::StalePlan { database_path } => DiagnosticReport::new(
                StateEffect::ProgressPreserved,
                Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                    RpgMakerResultStoreProblem::StalePlan {
                        path: SafePath::new(database_path),
                    },
                )),
            ),
            Self::NotCommitted {
                database_path,
                source,
            } => sqlite_source_report(
                source,
                database_path,
                SqliteOperation::Transaction,
                SqliteTransactionState::RolledBack,
                StateEffect::ProgressPreserved,
            ),
            Self::OutcomeUnknown {
                database_path,
                source,
            } => sqlite_source_report(
                source,
                database_path,
                SqliteOperation::Transaction,
                SqliteTransactionState::OutcomeUnknown,
                StateEffect::OutcomeUnknown,
            ),
            Self::SessionDatabaseChanged {
                opened_path,
                requested_path,
            } => DiagnosticReport::new(
                StateEffect::ProgressPreserved,
                Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                    RpgMakerResultStoreProblem::SessionDatabaseChanged {
                        opened_path: SafePath::new(opened_path),
                        requested_path: SafePath::new(requested_path),
                    },
                )),
            ),
            Self::SessionFinalized { database_path } => DiagnosticReport::new(
                StateEffect::ProgressPreserved,
                Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                    RpgMakerResultStoreProblem::SessionFinalized {
                        path: SafePath::new(database_path),
                    },
                )),
            ),
            Self::FinalizationRolledBackTransaction { database_path } => DiagnosticReport::new(
                StateEffect::AppliedFinalizationFailed,
                Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                    RpgMakerResultStoreProblem::FinalizationRolledBackTransaction {
                        path: SafePath::new(database_path),
                    },
                )),
            ),
            Self::FinalizationFailed {
                database_path,
                source,
            } => {
                let primary = source.primary();
                let (transaction, effect) = finalization_state(primary);
                let mut report = sqlite_source_report(
                    primary.source(),
                    database_path,
                    SqliteOperation::Cleanup,
                    transaction,
                    effect,
                );
                if let Some(connection_close) = source.connection_close() {
                    report = report.with_related(
                        RelatedFailureRelation::Finalization,
                        sqlite_source_report(
                            connection_close,
                            database_path,
                            SqliteOperation::Shutdown,
                            SqliteTransactionState::FinalizationFailed,
                            StateEffect::AppliedFinalizationFailed,
                        ),
                    );
                }
                report
            }
        }
    }

    /// 消费生产错误并把原始 source 与唯一的结构化诊断绑定为一个不可拆分的失败。
    pub(crate) fn into_reported_failure(self) -> ReportedFailure {
        match self {
            Self::FinalizationFailed {
                database_path,
                source,
            } => {
                let (primary, connection_close) = source.into_parts();
                let (transaction, effect) = finalization_state(&primary);
                let primary_report = sqlite_source_report(
                    primary.source(),
                    &database_path,
                    SqliteOperation::Cleanup,
                    transaction,
                    effect,
                );
                let primary_source = match primary {
                    SqliteInteractiveSessionFinalizationFailure::CleanupFailed(source)
                    | SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(source) => source,
                };
                let mut failure = ReportedFailure::new(primary_report, primary_source);
                if let Some(connection_close) = connection_close {
                    let report = sqlite_source_report(
                        &connection_close,
                        &database_path,
                        SqliteOperation::Shutdown,
                        SqliteTransactionState::FinalizationFailed,
                        StateEffect::AppliedFinalizationFailed,
                    );
                    failure = failure.with_related(
                        RelatedFailureRelation::Finalization,
                        ReportedFailure::new(report, connection_close),
                    );
                }
                failure
            }
            source => {
                let report = source.task_commit_diagnostic_report();
                ReportedFailure::new(report, source)
            }
        }
    }
}

fn finalization_state<S>(
    failure: &SqliteInteractiveSessionFinalizationFailure<S>,
) -> (SqliteTransactionState, StateEffect) {
    match failure {
        SqliteInteractiveSessionFinalizationFailure::CleanupFailed(_) => (
            SqliteTransactionState::FinalizationFailed,
            StateEffect::AppliedFinalizationFailed,
        ),
        SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(_) => (
            SqliteTransactionState::OutcomeUnknown,
            StateEffect::OutcomeUnknown,
        ),
    }
}

fn result_store_internal_report(
    operation: RuntimeBoundaryOperation,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::runtime(RuntimeIssue::InternalInvariant {
            stage: DiagnosticStage::Translate,
            component: RuntimeComponent::SqliteExecutor,
            operation,
        }),
    )
}

fn sqlite_source_report(
    source: &(impl Error + 'static),
    database_path: &std::path::Path,
    operation: SqliteOperation,
    transaction: SqliteTransactionState,
    effect: StateEffect,
) -> DiagnosticReport {
    if let Some(source) = (source as &(dyn Error + 'static)).downcast_ref::<SqliteRuntimeError>() {
        return source.diagnostic_report(
            database_path,
            SqliteDiagnosticContext::new(SqliteDiagnosticStage::Translate, operation, transaction),
            effect,
        );
    }
    result_store_internal_report(
        RuntimeBoundaryOperation::TranslateResultStorePlanInvalid,
        effect,
    )
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
    fn diagnostic_report(&self) -> DiagnosticReport {
        let violation = match self {
            Self::Location(source) => RpgMakerResultStorePlanViolation::LocationCodec {
                failure: source.diagnostic_failure(),
            },
            Self::Projection(source) => RpgMakerResultStorePlanViolation::ProjectionCodec {
                failure: source.diagnostic_failure(),
            },
            Self::Content(source) => RpgMakerResultStorePlanViolation::ContentJson {
                category: result_store_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
            Self::EmptyTaskResult => RpgMakerResultStorePlanViolation::EmptyTaskResult,
            Self::EmptyReuseTargets => RpgMakerResultStorePlanViolation::EmptyReuseTargets,
            Self::BlankTranslation => RpgMakerResultStorePlanViolation::BlankTranslation,
            Self::InconsistentTranslationState => {
                RpgMakerResultStorePlanViolation::InconsistentTranslationState
            }
            Self::MismatchedReuseSourceContent => {
                RpgMakerResultStorePlanViolation::MismatchedReuseSourceContent
            }
            Self::MismatchedReuseSourceContext => {
                RpgMakerResultStorePlanViolation::MismatchedReuseSourceContext
            }
            Self::MismatchedPropagationSourceContent => {
                RpgMakerResultStorePlanViolation::MismatchedPropagationSourceContent
            }
            Self::MismatchedPropagationSourceContext => {
                RpgMakerResultStorePlanViolation::MismatchedPropagationSourceContext
            }
            Self::DuplicateUnit => RpgMakerResultStorePlanViolation::DuplicateUnit,
            Self::InvalidCommitDecisionSequence => {
                RpgMakerResultStorePlanViolation::InvalidCommitDecisionSequence
            }
            Self::MissingCommitDecisionUnit => {
                RpgMakerResultStorePlanViolation::MissingCommitDecisionUnit
            }
        };
        DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::rpg_maker(RpgMakerIssue::result_store(
                RpgMakerResultStoreProblem::InvalidPlan { violation },
            )),
        )
    }
}

fn result_store_json_failure(source: &serde_json::Error) -> RpgMakerJsonFailureKind {
    JsonErrorCategory::from(source).into()
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
        rejection: Option<(
            ProvenInvariantViolation,
            Sha256Fingerprint,
            TranslationOrigin,
        )>,
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
        rejection: Option<EncodedRejectedUnit>,
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
    rejections: Vec<CommitRejectedUnitWork>,
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

struct CommitRejectedUnitWork {
    outcome: Arc<TranslationTaskOutcome>,
    unresolved_index: usize,
    target_index: usize,
}

struct EncodedCommitDecision {
    decision_index: usize,
    translation: String,
}

struct EncodedCommitUnit {
    decision_index: usize,
    identity: EncodedIdentity,
    translation_state: Sha256Fingerprint,
    expected_translation: Option<String>,
    expected_translation_state: Option<Sha256Fingerprint>,
}

struct EncodedRejectedUnit {
    identity: EncodedIdentity,
    readable_id: String,
    origin: TranslationOrigin,
    candidate_json: String,
    translation_json: Option<String>,
    violation_json: String,
    planning_state: Sha256Fingerprint,
    expected_translation: Option<String>,
    expected_translation_state: Option<Sha256Fingerprint>,
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
        let (identity, expected_translation, expected_translation_state, rejection) =
            invalidation.into_parts();
        work.push(PreparationUnitWork::Invalidation {
            identity,
            expected_translation,
            expected_translation_state,
            rejection,
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
            rejection,
        } => {
            ensure_nonblank(&expected_translation)?;
            let encoded_identity = encode_identity(&identity)?;
            let encoded_translation = encode_content(&expected_translation)?;
            let rejection = rejection
                .map(|(violation, planning_state, origin)| {
                    encode_invalidated_rejection(
                        &identity,
                        encoded_identity.clone(),
                        &expected_translation,
                        violation,
                        planning_state,
                        origin,
                    )
                })
                .transpose()?;
            Ok(EncodedPreparationUnit::Invalidation {
                identity: encoded_identity,
                expected_translation: encoded_translation,
                expected_translation_state,
                rejection,
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

fn encode_invalidated_rejection(
    identity: &TranslationUnitIdentity,
    encoded_identity: EncodedIdentity,
    translation: &TextUnitContent,
    violation: ProvenInvariantViolation,
    planning_state: Sha256Fingerprint,
    origin: TranslationOrigin,
) -> Result<EncodedRejectedUnit, ResultStoragePlanError> {
    let lines = match translation {
        TextUnitContent::Value(value) => value.split('\n').map(str::to_owned).collect::<Vec<_>>(),
        TextUnitContent::Lines(lines) => lines.clone(),
    };
    let translation_json =
        serde_json::to_string(&lines).map_err(ResultStoragePlanError::Content)?;
    Ok(EncodedRejectedUnit {
        identity: encoded_identity,
        readable_id: crate::manual::readable_rpg_maker_id(
            identity.group_location(),
            identity.kind(),
            identity.role(),
        ),
        origin,
        candidate_json: translation_json.clone(),
        translation_json: Some(translation_json),
        violation_json: serde_json::to_string(&violation)
            .map_err(ResultStoragePlanError::Content)?,
        planning_state,
        expected_translation: None,
        expected_translation_state: None,
    })
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
    let mut clear_manual_parameter_sets = Vec::new();
    let mut rejected_parameter_sets = Vec::new();
    let mut reuse_parameter_sets = Vec::new();
    for unit in units {
        match unit {
            EncodedPreparationUnit::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
                rejection,
            } => {
                ensure_unique(&mut seen, &identity)?;
                let clears_manual = rejection
                    .as_ref()
                    .is_some_and(|rejection| rejection.origin == TranslationOrigin::Manual);
                if clears_manual {
                    clear_manual_parameter_sets.push(clear_manual_translation_parameters(
                        &identity,
                        &expected_translation,
                        expected_translation_state,
                    )?);
                } else {
                    clear_parameter_sets.push(clear_translation_parameters(
                        identity,
                        expected_translation,
                        expected_translation_state,
                    ));
                }
                if let Some(rejection) = rejection {
                    rejected_parameter_sets.push(rejected_translation_parameters(rejection));
                }
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
    if !clear_manual_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(
                CLEAR_MANUAL_TRANSLATION_FROM_SNAPSHOT,
                clear_manual_parameter_sets,
            ),
        ));
    }
    if !rejected_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(UPSERT_REJECTED_TRANSLATION, rejected_parameter_sets),
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
    let rejected_count = outcome.rejected_location_count();
    if decisions.is_empty() && rejected_count == 0 {
        return Err(ResultStoragePlanError::EmptyTaskResult);
    }
    let location_count = decisions
        .iter()
        .map(|decision| 1 + decision.propagation_targets().len())
        .sum();
    let mut decision_work = Vec::with_capacity(decisions.len());
    let mut unit_work = Vec::with_capacity(location_count);
    let mut rejected_work = Vec::with_capacity(rejected_count);
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
    for (unresolved_index, unresolved) in outcome.unresolved().iter().enumerate() {
        let Some(candidate) = unresolved.rejected_candidate() else {
            continue;
        };
        for target_index in 0..candidate.targets().len() {
            rejected_work.push(CommitRejectedUnitWork {
                outcome: Arc::clone(&outcome),
                unresolved_index,
                target_index,
            });
        }
    }
    Ok(CommitPlanWork {
        decisions: decision_work,
        units: unit_work,
        rejections: rejected_work,
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
    let (identity, translation_state, expected_previous) = match work.position {
        CommitUnitPosition::Representative => (
            patch.identity(),
            patch.translation_state(),
            patch.expected_previous(),
        ),
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
                target.state_context().applicability(),
                target.expected_previous(),
            )
        }
    };
    let (expected_translation, expected_translation_state) = match expected_previous {
        Some((translation, state)) => (Some(encode_content(translation)?), Some(state)),
        None => (None, None),
    };
    Ok(EncodedCommitUnit {
        decision_index: work.decision_index,
        identity: encode_identity(identity)?,
        translation_state,
        expected_translation,
        expected_translation_state,
    })
}

fn encode_rejected_unit(
    work: CommitRejectedUnitWork,
) -> Result<EncodedRejectedUnit, ResultStoragePlanError> {
    let unresolved = work
        .outcome
        .unresolved()
        .get(work.unresolved_index)
        .expect("Rejected 提交工作必须引用当前 unresolved Unit");
    let candidate = unresolved
        .rejected_candidate()
        .expect("Rejected 提交工作必须引用硬拒绝候选");
    let target = candidate
        .targets()
        .get(work.target_index)
        .expect("Rejected 提交工作必须引用当前传播目标");
    serde_json::from_str::<serde_json::Value>(candidate.candidate_json())
        .map_err(ResultStoragePlanError::Content)?;
    let translation_json = candidate
        .translation()
        .filter(|lines| !lines.is_empty())
        .map(serde_json::to_string)
        .transpose()
        .map_err(ResultStoragePlanError::Content)?;
    let violation_json =
        serde_json::to_string(candidate.violation()).map_err(ResultStoragePlanError::Content)?;
    let identity = target.identity();
    let (expected_translation, expected_translation_state) = match target.expected_previous() {
        Some((translation, state)) => (Some(encode_content(translation)?), Some(state)),
        None => (None, None),
    };
    let readable_id = crate::manual::readable_rpg_maker_id(
        identity.group_location(),
        identity.kind(),
        identity.role(),
    );
    Ok(EncodedRejectedUnit {
        identity: encode_identity(identity)?,
        readable_id,
        origin: TranslationOrigin::Automatic,
        candidate_json: candidate.candidate_json().to_owned(),
        translation_json,
        violation_json,
        planning_state: target.planning_state(),
        expected_translation,
        expected_translation_state,
    })
}

fn finish_commit_plan(
    decisions: Vec<EncodedCommitDecision>,
    units: Vec<EncodedCommitUnit>,
    rejections: Vec<EncodedRejectedUnit>,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut seen = HashSet::with_capacity(units.len());
    let mut batches = Vec::with_capacity(decisions.len());
    let mut rejected_clears = Vec::with_capacity(units.len());
    for (expected_index, decision) in decisions.into_iter().enumerate() {
        if decision.decision_index != expected_index {
            return Err(ResultStoragePlanError::InvalidCommitDecisionSequence);
        }
        batches.push((decision.translation, Vec::new()));
    }
    for unit in units {
        ensure_unique(&mut seen, &unit.identity)?;
        rejected_clears.push(rejected_key_parameters(&unit.identity));
        let decision = batches
            .get_mut(unit.decision_index)
            .ok_or(ResultStoragePlanError::InvalidCommitDecisionSequence)?;
        decision.1.push(commit_translation_parameters(unit));
    }
    let mut rejected_writes = Vec::with_capacity(rejections.len());
    for rejection in rejections {
        ensure_unique(&mut seen, &rejection.identity)?;
        rejected_writes.push(rejected_translation_parameters(rejection));
    }
    let mut steps = Vec::with_capacity(
        batches.len()
            + usize::from(!rejected_clears.is_empty())
            + usize::from(!rejected_writes.is_empty()),
    );
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
    if !rejected_clears.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            DELETE_REJECTED_TRANSLATION,
            rejected_clears,
        )));
    }
    if !rejected_writes.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(UPSERT_REJECTED_TRANSLATION, rejected_writes),
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
            "SELECT 1 WHERE (SELECT COUNT(*) FROM metadata) <> 1 OR NOT EXISTS (SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?) OR {owner_condition} OR (SELECT COUNT(*) FROM rpg_maker_translation_resource) <> 3 OR NOT EXISTS (SELECT 1 FROM rpg_maker_translation_resource WHERE resource_kind = ? AND canonical_json = ?) OR NOT EXISTS (SELECT 1 FROM rpg_maker_translation_resource WHERE resource_kind = ? AND canonical_json = ?)"
        ),
        parameters,
    ))
}

const REQUIRE_SNAPSHOT: &str = "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM rpg_maker_text_unit WHERE owner = ?1 AND group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = ?1 AND text_group.group_location = ?2) AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND ((?6 IS NULL AND ?7 IS NULL AND translation_content_json IS NULL AND translation_state IS NULL) OR (translation_content_json = ?6 AND translation_state = ?7)))";

const CLEAR_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = NULL, translation_state = NULL WHERE owner = ?1 AND group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = ?1 AND text_group.group_location = ?2) AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND translation_content_json = ?6 AND translation_state = ?7";

const CLEAR_MANUAL_TRANSLATION_FROM_SNAPSHOT: &str = "DELETE FROM rpg_maker_manual_translation WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3 AND translation_json = ?4 AND applicability_fingerprint = ?5";

const WRITE_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = ?3 AND text_group.group_location = ?4) AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND (translation_content_json = ?8 OR (translation_content_json IS NULL AND ?8 IS NULL)) AND (translation_state = ?9 OR (translation_state IS NULL AND ?9 IS NULL))";

const COMMIT_TRANSLATION: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = ?3 AND text_group.group_location = ?4) AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND ((?8 IS NULL AND translation_content_json IS NULL AND translation_state IS NULL) OR (translation_content_json = ?8 AND translation_state = ?9))";

const DELETE_REJECTED_TRANSLATION: &str = "DELETE FROM rpg_maker_rejected_translation WHERE owner = ?1 AND group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = ?1 AND text_group.group_location = ?2) AND unit_role = ?3";

const UPSERT_REJECTED_TRANSLATION: &str = r#"INSERT INTO rpg_maker_rejected_translation (
    owner, group_id, unit_role, readable_id, origin, source_content_json,
    source_context_json, candidate_json, translation_json, violation_json, planning_state
)
SELECT unit.owner, unit.group_id, unit.unit_role, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
FROM rpg_maker_text_unit AS unit
JOIN rpg_maker_text_group AS text_group
  ON text_group.owner = unit.owner AND text_group.group_id = unit.group_id
WHERE unit.owner = ?9
  AND text_group.group_location = ?10
  AND unit.unit_role = ?11
  AND unit.source_content_json = ?3
  AND unit.source_context_json = ?4
  AND (
      (?12 IS NULL AND unit.translation_content_json IS NULL AND unit.translation_state IS NULL)
      OR (unit.translation_content_json = ?12 AND unit.translation_state = ?13)
  )
ON CONFLICT (owner, group_id, unit_role) DO UPDATE SET
    readable_id = excluded.readable_id,
    origin = excluded.origin,
    source_content_json = excluded.source_content_json,
    source_context_json = excluded.source_context_json,
    candidate_json = excluded.candidate_json,
    translation_json = excluded.translation_json,
    violation_json = excluded.violation_json,
    planning_state = excluded.planning_state"#;

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

fn clear_manual_translation_parameters(
    identity: &EncodedIdentity,
    expected_translation: &str,
    expected_translation_state: Sha256Fingerprint,
) -> Result<Vec<SqliteValue>, ResultStoragePlanError> {
    let content = serde_json::from_str::<TextUnitContent>(expected_translation)
        .map_err(ResultStoragePlanError::Content)?;
    let lines = crate::manual::rpg_maker_manual_source_lines(&content);
    Ok(vec![
        text(identity.owner),
        text(identity.group_location.clone()),
        text(identity.unit_role.clone()),
        text(serde_json::to_string(&lines).map_err(ResultStoragePlanError::Content)?),
        blob(expected_translation_state),
    ])
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
        unit.expected_translation.map_or(SqliteValue::Null, text),
        unit.expected_translation_state
            .map_or(SqliteValue::Null, blob),
    ]
}

fn rejected_key_parameters(identity: &EncodedIdentity) -> Vec<SqliteValue> {
    vec![
        text(identity.owner),
        text(identity.group_location.clone()),
        text(identity.unit_role.clone()),
    ]
}

fn rejected_translation_parameters(rejection: EncodedRejectedUnit) -> Vec<SqliteValue> {
    vec![
        text(rejection.readable_id),
        text(rejection.origin.storage_name()),
        text(rejection.identity.source_content_json),
        text(rejection.identity.source_context_json),
        text(rejection.candidate_json),
        rejection.translation_json.map_or(SqliteValue::Null, text),
        text(rejection.violation_json),
        blob(rejection.planning_state),
        text(rejection.identity.owner),
        text(rejection.identity.group_location),
        text(rejection.identity.unit_role),
        rejection
            .expected_translation
            .map_or(SqliteValue::Null, text),
        rejection
            .expected_translation_state
            .map_or(SqliteValue::Null, blob),
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
    #[cfg(feature = "release-stress")]
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
    use crate::translation::task_planning::TaskId;
    use rusqlite::{Connection, params};

    use super::*;
    use crate::rpg_maker::translate::pipeline::{
        AcceptedTranslationDecision, NonEmptyTaskItems, RejectedTranslationCandidate,
        RejectedTranslationTarget, RpgMakerTranslationTaskIndex, TranslationInvalidation,
        TranslationOwnerSnapshot, TranslationPatch, TranslationPropagationTarget, TranslationReuse,
        TranslationReuseSeed, TranslationReuseTarget, TranslationSnapshotBaseline,
        TranslationStateContext, TranslationTaskOutcomeContext, TranslationTaskUnavailableReason,
        TranslationUnitRejectionReason, UnresolvedTranslationUnit, rpg_maker_diagnostic_unit,
    };

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    #[test]
    fn closed_result_storage_plan_invariants_keep_typed_violations() {
        let cases = [
            (ResultStoragePlanError::EmptyTaskResult, "empty_task_result"),
            (
                ResultStoragePlanError::EmptyReuseTargets,
                "empty_reuse_targets",
            ),
            (
                ResultStoragePlanError::BlankTranslation,
                "blank_translation",
            ),
            (
                ResultStoragePlanError::InconsistentTranslationState,
                "inconsistent_translation_state",
            ),
            (
                ResultStoragePlanError::MismatchedReuseSourceContent,
                "mismatched_reuse_source_content",
            ),
            (
                ResultStoragePlanError::MismatchedReuseSourceContext,
                "mismatched_reuse_source_context",
            ),
            (
                ResultStoragePlanError::MismatchedPropagationSourceContent,
                "mismatched_propagation_source_content",
            ),
            (
                ResultStoragePlanError::MismatchedPropagationSourceContext,
                "mismatched_propagation_source_context",
            ),
            (ResultStoragePlanError::DuplicateUnit, "duplicate_unit"),
            (
                ResultStoragePlanError::InvalidCommitDecisionSequence,
                "invalid_commit_decision_sequence",
            ),
            (
                ResultStoragePlanError::MissingCommitDecisionUnit,
                "missing_commit_decision_unit",
            ),
        ];

        for (source, expected_violation) in cases {
            let report = source.diagnostic_report();
            assert_eq!(report.effect(), StateEffect::ProgressPreserved);
            assert_eq!(
                report.primary().code(),
                "rpg_maker.translate.result_store.invalid_plan"
            );
            assert_eq!(report.primary().stage(), DiagnosticStage::Translate);
            let wire = serde_json::to_value(report).expect("结果存储诊断应可序列化");
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["kind"],
                "result_store"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["problem"]["kind"],
                "invalid_plan"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["problem"]["violation"]["kind"],
                expected_violation
            );
        }
    }

    #[test]
    fn result_storage_plan_codecs_keep_typed_facts_without_copying_source_text() {
        const SOURCE_BODY: &str = "SENTINEL_RESULT_STORAGE_BODY_8ebfa3";

        let invalid_json = format!("{{\"{SOURCE_BODY}\":");
        let json_error =
            serde_json::from_str::<serde_json::Value>(&invalid_json).expect_err("JSON 应不完整");
        let content_diagnostic = ResultStoragePlanError::Content(json_error).diagnostic_report();
        let content_json = serde_json::to_value(content_diagnostic).expect("公开诊断应可序列化");
        let content_violation =
            &content_json["primary"]["issue"]["details"]["problem"]["problem"]["violation"];
        assert_eq!(content_violation["kind"], "content_json");
        assert_eq!(content_violation["category"], "eof");
        assert_eq!(content_violation["line"], 1);
        assert!(content_violation["column"].is_number());
        assert!(!content_json.to_string().contains(SOURCE_BODY));

        let location_diagnostic = ResultStoragePlanError::Location(
            RpgMakerLocationCodecError::InvalidDataFile(SOURCE_BODY.to_owned()),
        )
        .diagnostic_report();
        let location_json = serde_json::to_value(location_diagnostic).expect("公开诊断应可序列化");
        let location_violation =
            &location_json["primary"]["issue"]["details"]["problem"]["problem"]["violation"];
        assert_eq!(location_violation["kind"], "location_codec");
        assert_eq!(location_violation["failure"]["kind"], "invalid_data_file");
        assert!(!location_json.to_string().contains(SOURCE_BODY));

        let projection_diagnostic =
            ResultStoragePlanError::Projection(RpgMakerProjectionCodecError::Projection(
                crate::rpg_maker::model::ProjectionModelError::NonContiguousDialogueBodyLines {
                    expected: 2,
                    actual: 4,
                },
            ))
            .diagnostic_report();
        let projection_json =
            serde_json::to_value(projection_diagnostic).expect("公开诊断应可序列化");
        let projection_violation =
            &projection_json["primary"]["issue"]["details"]["problem"]["problem"]["violation"];
        assert_eq!(projection_violation["kind"], "projection_codec");
        assert_eq!(projection_violation["failure"]["kind"], "projection");
        assert_eq!(
            projection_violation["failure"]["violation"]["kind"],
            "non_contiguous_dialogue_body_lines"
        );
        assert_eq!(projection_violation["failure"]["violation"]["expected"], 2);
        assert_eq!(projection_violation["failure"]["violation"]["actual"], 4);
    }

    #[test]
    fn cpu_failures_keep_the_typed_runtime_cause() {
        let error: RpgMakerTranslationResultStorageError<
            SqliteRuntimeError,
            CpuExecutorUnavailable,
        > = RpgMakerTranslationResultStorageError::ScheduleEncoding(
            CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned),
        );

        let report = error.task_commit_diagnostic_report();

        assert_eq!(report.effect(), StateEffect::ProgressPreserved);
        assert_eq!(report.primary().stage(), DiagnosticStage::Translate);
        assert_eq!(report.primary().code(), "runtime.state_poisoned");
        let wire = serde_json::to_value(report).expect("CPU 诊断应可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["component"],
            "cpu_executor"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["operation"],
            "encode_rpg_maker_translation_result"
        );
    }

    #[test]
    fn finalization_keeps_primary_and_connection_close_as_one_related_report_tree() {
        fn error() -> RpgMakerTranslationResultStorageError<SqliteRuntimeError, FakeError> {
            let database_path = PathBuf::from(r"C:\projects\demo\project.db");
            RpgMakerTranslationResultStorageError::FinalizationFailed {
                database_path: database_path.clone(),
                source: SqliteInteractiveSessionFinalizationError::new(
                    SqliteInteractiveSessionFinalizationFailure::CleanupFailed(
                        SqliteRuntimeError::Io {
                            operation: "rollback_translation_session",
                            path: database_path.clone(),
                            source: std::io::Error::from_raw_os_error(5),
                        },
                    ),
                    Some(SqliteRuntimeError::Io {
                        operation: "close_translation_session",
                        path: database_path,
                        source: std::io::Error::from_raw_os_error(6),
                    }),
                ),
            }
        }

        let borrowed = error().task_commit_diagnostic_report();
        let borrowed_wire = serde_json::to_value(&borrowed).expect("收尾诊断应可序列化");
        assert_eq!(borrowed.effect(), StateEffect::AppliedFinalizationFailed);
        assert_eq!(borrowed.related().len(), 1);
        assert_eq!(
            borrowed_wire["primary"]["issue"]["details"]["context"]["operation"],
            "cleanup"
        );
        assert_eq!(
            borrowed_wire["primary"]["issue"]["details"]["context"]["transaction"],
            "finalization_failed"
        );
        assert_eq!(
            borrowed_wire["primary"]["issue"]["details"]["problem"]["failure"]["raw_os_code"],
            5
        );
        assert_eq!(borrowed_wire["related"][0]["relation"], "finalization");
        assert_eq!(
            borrowed_wire["related"][0]["report"]["primary"]["issue"]["details"]["context"]["operation"],
            "shutdown"
        );
        assert_eq!(
            borrowed_wire["related"][0]["report"]["primary"]["issue"]["details"]["problem"]["failure"]
                ["raw_os_code"],
            6
        );

        let reported = error().into_reported_failure();
        assert!(reported.source_error().is::<SqliteRuntimeError>());
        assert_eq!(
            serde_json::to_value(reported.report()).expect("消费后的收尾诊断应可序列化"),
            borrowed_wire
        );
    }

    #[test]
    fn finalization_outcome_unknown_keeps_unknown_transaction_state() {
        let database_path = PathBuf::from(r"C:\projects\demo\project.db");
        let error: RpgMakerTranslationResultStorageError<SqliteRuntimeError, FakeError> =
            RpgMakerTranslationResultStorageError::FinalizationFailed {
                database_path: database_path.clone(),
                source: SqliteInteractiveSessionFinalizationError::new(
                    SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
                        SqliteRuntimeError::Io {
                            operation: "finalize_translation_session",
                            path: database_path,
                            source: std::io::Error::from_raw_os_error(1117),
                        },
                    ),
                    None,
                ),
            };

        let report = error.task_commit_diagnostic_report();
        let wire = serde_json::to_value(&report).expect("未知终态诊断应可序列化");
        assert_eq!(report.effect(), StateEffect::OutcomeUnknown);
        assert!(report.related().is_empty());
        assert_eq!(
            wire["primary"]["issue"]["details"]["context"]["transaction"],
            "outcome_unknown"
        );
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
        type Error = SqliteRuntimeError;

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
                RecordingTransactionResult::NotApplied => Err(
                    ExecuteTransactionError::NotCommitted(SqliteRuntimeError::Io {
                        operation: "commit_recording_translation",
                        path: self.path.clone(),
                        source: std::io::Error::from_raw_os_error(5),
                    }),
                ),
                RecordingTransactionResult::OutcomeUnknown => Err(
                    ExecuteTransactionError::OutcomeUnknown(SqliteRuntimeError::Io {
                        operation: "commit_recording_translation",
                        path: self.path.clone(),
                        source: std::io::Error::from_raw_os_error(1117),
                    }),
                ),
            })
        }
    }

    struct RecordingFinalizer {
        finalizations: Arc<AtomicUsize>,
    }

    impl SqliteInteractiveSessionFinalizer for RecordingFinalizer {
        type Error = SqliteRuntimeError;

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
        type Error = SqliteRuntimeError;

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
                accepted: NonEmptyTaskItems::new(
                    AcceptedTranslationDecision::new(
                        task_id(0),
                        TranslationPatch::new(
                            identity.clone(),
                            Vec::new(),
                            translation.clone(),
                            context.applicability(),
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
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    task_id(0),
                    TranslationPatch::new(
                        identity.clone(),
                        Vec::new(),
                        translation.clone(),
                        context.applicability(),
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
        assert_eq!(plan.steps().len(), 2);
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
        assert!(
            update
                .statement()
                .contains("FROM rpg_maker_text_group AS text_group")
        );
        assert!(update.statement().contains("group_id = (SELECT"));
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
        let SqliteTransactionStep::ExecuteMany(rejected_clear) = &plan.steps()[1] else {
            panic!("合法自动译文必须在同一事务清除旧 Rejected 候选");
        };
        assert_eq!(rejected_clear.statement(), DELETE_REJECTED_TRANSLATION);
        assert_eq!(rejected_clear.parameter_set_count(), 1);
    }

    #[tokio::test]
    async fn rejected_commit_uses_the_exact_retained_stale_body_as_its_cas_baseline() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let identity = scalar_identity(1, "name", "原文", "{}");
        let previous = value("旧语境译文");
        let previous_state = Sha256Fingerprint::from_bytes([0x61; 32]);
        let target = RejectedTranslationTarget::new(
            identity.clone(),
            Sha256Fingerprint::from_bytes([0x62; 32]),
            Some((previous.clone(), previous_state)),
        );
        let candidate = RejectedTranslationCandidate::new(
            r#"[""]"#.to_owned(),
            Some(vec![String::new()]),
            ProvenInvariantViolation::BlankTranslation,
            vec![target],
        );
        let unresolved = UnresolvedTranslationUnit::with_rejected_candidate(
            task_id(0),
            rpg_maker_diagnostic_unit(&identity),
            TranslationUnitRejectionReason::BlankTranslation,
            candidate,
        );
        let outcome = Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(unresolved, Vec::new()),
        });

        let prepared = service
            .prepare_commit(outcome)
            .await
            .expect("Rejected 应可编码");
        service
            .commit_prepared(&project(), prepared)
            .await
            .expect("精确旧正文应允许保存 Rejected");

        let plans = plans.lock().expect("事务锁");
        let [SqliteTransactionStep::ExecuteManyExactlyOne(write)] = plans[0].1.steps() else {
            panic!("纯 Rejected 任务应只有一批条件写入")
        };
        assert_eq!(write.statement(), UPSERT_REJECTED_TRANSLATION);
        let parameters = write
            .parameter_rows()
            .next()
            .expect("应有一组 Rejected 参数");
        assert_eq!(parameters.len(), 13);
        assert_eq!(
            parameters[11],
            SqliteValue::Text(r#""旧语境译文""#.to_owned())
        );
        assert_eq!(parameters[12], SqliteValue::Blob(vec![0x61; 32]));
    }

    #[test]
    fn unit_writes_resolve_internal_group_id_from_the_domain_location() {
        for (statement, owner_parameter, location_parameter) in [
            (REQUIRE_SNAPSHOT, "?1", "?2"),
            (CLEAR_TRANSLATION_FROM_SNAPSHOT, "?1", "?2"),
            (WRITE_TRANSLATION_FROM_SNAPSHOT, "?3", "?4"),
            (COMMIT_TRANSLATION, "?3", "?4"),
        ] {
            let expected_lookup = format!(
                "group_id = (SELECT text_group.group_id FROM rpg_maker_text_group AS text_group WHERE text_group.owner = {owner_parameter} AND text_group.group_location = {location_parameter})"
            );
            assert!(
                statement.contains(&expected_lookup),
                "Unit 写入必须由 owner 与领域 group_location 查找内部 group_id：{statement}"
            );
        }
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
            context.applicability(),
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
        assert_eq!(
            diagnostic.primary().code(),
            "rpg_maker.translate.result_store.invalid_plan"
        );
        assert_eq!(diagnostic.primary().stage(), DiagnosticStage::Translate);
        assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
        let wire = serde_json::to_value(diagnostic).expect("提交准备诊断应可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["problem"]["violation"]["kind"],
            "duplicate_unit"
        );
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
        assert_eq!(diagnostic.primary().code(), "sqlite.io");
        assert_eq!(diagnostic.primary().stage(), DiagnosticStage::Translate);
        assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
        let wire = serde_json::to_value(diagnostic).expect("提交诊断应可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["context"]["operation"],
            "transaction"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["context"]["transaction"],
            "rolled_back"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["database"],
            expected_project.database_path().to_string_lossy().as_ref()
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["failure"]["raw_os_code"],
            5
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
        assert_eq!(diagnostic.primary().code(), "sqlite.io");
        assert_eq!(diagnostic.primary().stage(), DiagnosticStage::Translate);
        assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
        let wire = serde_json::to_value(diagnostic).expect("终态未知诊断应可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["context"]["operation"],
            "transaction"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["context"]["transaction"],
            "outcome_unknown"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["database"],
            expected_project.database_path().to_string_lossy().as_ref()
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["failure"]["raw_os_code"],
            1117
        );
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_huge_propagation_family_encodes_and_owns_translation_once_per_decision() {
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
            state_context(0x5a).applicability(),
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

        let plan =
            finish_commit_plan(decisions, units, Vec::new()).expect("超大全族应形成原子提交计划");
        assert_eq!(plan.steps().len(), 2);
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
            parameters.len() == 8 && !parameters.contains(&encoded_translation)
        }));
        let SqliteTransactionStep::ExecuteMany(rejected_clear) = &plan.steps()[1] else {
            panic!("全部传播目标必须批量清除旧 Rejected 候选");
        };
        assert_eq!(rejected_clear.statement(), DELETE_REJECTED_TRANSLATION);
        assert_eq!(rejected_clear.parameter_set_count(), TARGETS + 1);
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
                state_context(0x11).applicability(),
            ),
            TranslationPatch::new(
                second_representative,
                Vec::new(),
                second_translation.clone(),
                state_context(0x21).applicability(),
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

        let plan = finish_commit_plan(decisions, units, Vec::new()).expect("提交计划应可建立");
        assert_eq!(plan.steps().len(), 3);
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
        let SqliteTransactionStep::ExecuteMany(rejected_clear) = &plan.steps()[2] else {
            panic!("合法译文提交后必须按自然 Unit 顺序清除旧 Rejected 候选");
        };
        assert_eq!(rejected_clear.statement(), DELETE_REJECTED_TRANSLATION);
        assert_eq!(rejected_clear.parameter_set_count(), 3);
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
    async fn preparation_moves_invalid_manual_translation_to_rejected_in_one_transaction() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerTranslationResultStorageService::new(sqlite, InlineCpu);
        let identity = scalar_identity(1, "name", "原文", "{}");
        let manual_state = Sha256Fingerprint::from_bytes([0x31; 32]);
        let rejection_state = Sha256Fingerprint::from_bytes([0x32; 32]);
        let preparation = TranslationPlanPreparation::new(
            vec![TranslationInvalidation::rejected(
                identity,
                value("失效人工译文"),
                manual_state,
                ProvenInvariantViolation::PlaceholderMismatch,
                rejection_state,
                TranslationOrigin::Manual,
            )],
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            1,
            0,
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("失效人工译文应该可原子转入 Rejected");

        let plans = plans.lock().expect("事务锁");
        let steps = plans[0].1.steps();
        assert!(matches!(steps[0], SqliteTransactionStep::RequireNoRows(_)));
        let SqliteTransactionStep::ExecuteManyExactlyOne(clear_manual) = &steps[1] else {
            panic!("人工 Current 必须先以 CAS 从人工译文表删除");
        };
        assert_eq!(
            clear_manual.statement(),
            CLEAR_MANUAL_TRANSLATION_FROM_SNAPSHOT
        );
        assert_eq!(clear_manual.parameter_set_count(), 1);
        let SqliteTransactionStep::ExecuteManyExactlyOne(save_rejected) = &steps[2] else {
            panic!("同一事务必须保存 Rejected 正文与来源");
        };
        assert_eq!(save_rejected.statement(), UPSERT_REJECTED_TRANSLATION);
        let parameters = save_rejected
            .parameter_rows()
            .next()
            .expect("Rejected 应包含一组参数");
        assert_eq!(parameters[1], SqliteValue::Text("manual".to_owned()));
        assert_eq!(
            parameters[5],
            SqliteValue::Text(r#"["失效人工译文"]"#.to_owned())
        );
        assert_eq!(parameters[7], SqliteValue::Blob(vec![0x32; 32]));
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

    #[tokio::test]
    async fn real_sqlite_rejected_commit_preserves_the_exact_retained_stale_body() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory
            .path()
            .join("retained-rejected")
            .join("project.db");
        let identity = scalar_identity(1, "name", "翻訳対象", "{}");
        let previous = value("旧语境译文");
        let previous_state = Sha256Fingerprint::from_bytes([0x61; 32]);
        create_unit_database(
            &database_path,
            &[StoredUnit::new(
                &identity,
                "翻訳対象",
                "{}",
                Some("旧语境译文"),
                Some(previous_state.as_bytes()),
            )],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let target = RejectedTranslationTarget::new(
            identity.clone(),
            Sha256Fingerprint::from_bytes([0x62; 32]),
            Some((previous.clone(), previous_state)),
        );
        let candidate = RejectedTranslationCandidate::new(
            r#"[""]"#.to_owned(),
            Some(vec![String::new()]),
            ProvenInvariantViolation::BlankTranslation,
            vec![target],
        );
        let outcome = Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::with_rejected_candidate(
                    task_id(0),
                    rpg_maker_diagnostic_unit(&identity),
                    TranslationUnitRejectionReason::BlankTranslation,
                    candidate,
                ),
                Vec::new(),
            ),
        });

        let prepared = service
            .prepare_commit(outcome)
            .await
            .expect("Rejected 应可编码");
        service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect("数据库仍是精确旧正文时，应保存 Rejected");

        assert_eq!(
            stored_translation(&database_path, 1),
            (Some(previous), Some(previous_state.as_bytes().to_vec()),),
            "保存模型 Rejected 不得清除仍可恢复的旧正文"
        );
        assert_eq!(stored_rejection_count(&database_path), 1);

        service.finalize().await.expect("测试数据库会话应正常终结");
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_rejected_commit_rolls_back_when_the_retained_body_changed() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory
            .path()
            .join("retained-rejected-race")
            .join("project.db");
        let identity = scalar_identity(1, "name", "翻訳対象", "{}");
        let expected_previous = value("规划时旧译文");
        let expected_previous_state = Sha256Fingerprint::from_bytes([0x71; 32]);
        create_unit_database(
            &database_path,
            &[StoredUnit::new(
                &identity,
                "翻訳対象",
                "{}",
                Some("规划时旧译文"),
                Some(expected_previous_state.as_bytes()),
            )],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let target = RejectedTranslationTarget::new(
            identity.clone(),
            Sha256Fingerprint::from_bytes([0x72; 32]),
            Some((expected_previous, expected_previous_state)),
        );
        let candidate = RejectedTranslationCandidate::new(
            r#"[""]"#.to_owned(),
            Some(vec![String::new()]),
            ProvenInvariantViolation::BlankTranslation,
            vec![target],
        );
        let outcome = Arc::new(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::with_rejected_candidate(
                    task_id(0),
                    rpg_maker_diagnostic_unit(&identity),
                    TranslationUnitRejectionReason::BlankTranslation,
                    candidate,
                ),
                Vec::new(),
            ),
        });
        let prepared = service
            .prepare_commit(outcome)
            .await
            .expect("Rejected 应可编码");
        Connection::open(&database_path)
            .expect("数据库应可并发重开")
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?, translation_state = ?
                 WHERE rowid = 1",
                params![r#""并发新译文""#, vec![0x7f_u8; 32]],
            )
            .expect("应可模拟规划后到达的新译文");

        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("旧正文或状态已变化时，Rejected 不得覆盖并发结果");
        assert!(matches!(
            error.source(),
            RpgMakerTranslationResultStorageError::StalePlan { .. }
        ));
        assert_eq!(
            stored_translation(&database_path, 1),
            (Some(value("并发新译文")), Some(vec![0x7f; 32]))
        );
        assert_eq!(
            stored_rejection_count(&database_path),
            0,
            "CAS 失败必须回滚 Rejected 写入"
        );

        service.finalize().await.expect("测试数据库会话应正常终结");
        storage.shutdown().await.expect("SQLite 根应正常关闭");
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
            context.applicability(),
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

        let error = finish_commit_plan(decisions, encoded, Vec::new())
            .expect_err("重复逻辑单元不得进入事务");
        assert!(matches!(error, ResultStoragePlanError::DuplicateUnit));
    }

    #[tokio::test]
    async fn real_sqlite_commits_retried_strong_rejections_from_the_post_preparation_baseline() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("retry-rejected").join("project.db");
        let representative = scalar_identity(1, "name", r"翻訳対象 \V[1]", "{}");
        let propagation = scalar_identity(2, "name", r"翻訳対象 \V[1]", "{}");
        let representative_state = Sha256Fingerprint::from_bytes([0x31; 32]);
        let propagation_state = Sha256Fingerprint::from_bytes([0x32; 32]);
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(
                    &representative,
                    r"翻訳対象 \V[1]",
                    "{}",
                    Some("缺少占位符的旧译文"),
                    Some(representative_state.as_bytes()),
                ),
                StoredUnit::new(
                    &propagation,
                    r"翻訳対象 \V[1]",
                    "{}",
                    Some("另一条缺少占位符的旧译文"),
                    Some(propagation_state.as_bytes()),
                ),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let preparation = TranslationPlanPreparation::new(
            vec![
                TranslationInvalidation::rejected(
                    representative.clone(),
                    value("缺少占位符的旧译文"),
                    representative_state,
                    ProvenInvariantViolation::PlaceholderMismatch,
                    Sha256Fingerprint::from_bytes([0x41; 32]),
                    TranslationOrigin::Automatic,
                ),
                TranslationInvalidation::rejected(
                    propagation.clone(),
                    value("另一条缺少占位符的旧译文"),
                    propagation_state,
                    ProvenInvariantViolation::PlaceholderMismatch,
                    Sha256Fingerprint::from_bytes([0x42; 32]),
                    TranslationOrigin::Automatic,
                ),
            ],
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            2,
            0,
        );

        service
            .apply_preparation(&project_at(database_path.clone()), preparation)
            .await
            .expect("强不变量旧译文应先原子转入 Rejected");
        assert_eq!(stored_translation(&database_path, 1), (None, None));
        assert_eq!(stored_translation(&database_path, 2), (None, None));
        assert_eq!(stored_rejection_count(&database_path), 2);

        let translation = value(r"已修复的译文 \V[1]");
        let representative_context = state_context(0x51);
        let propagation_context = state_context(0x52);
        let committed_representative_state = representative_context.applicability();
        let committed_propagation_state = propagation_context.applicability();
        let outcome = complete_outcome(vec![TranslationPatch::new(
            representative,
            vec![TranslationPropagationTarget::new(
                propagation,
                propagation_context,
            )],
            translation.clone(),
            committed_representative_state,
        )]);
        let prepared = service
            .prepare_commit(outcome)
            .await
            .expect("修复结果应可编码");
        service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect("Preparation 后的空 Current 基线必须允许代表和传播目标原子提交");

        assert_eq!(
            stored_translation(&database_path, 1),
            (
                Some(translation.clone()),
                Some(committed_representative_state.as_bytes().to_vec()),
            )
        );
        assert_eq!(
            stored_translation(&database_path, 2),
            (
                Some(translation),
                Some(committed_propagation_state.as_bytes().to_vec()),
            )
        );
        assert_eq!(
            stored_rejection_count(&database_path),
            0,
            "合法替换提交后应清除两项旧 Rejected"
        );

        service.finalize().await.expect("测试数据库会话应正常终结");
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_retry_rejected_commit_rolls_back_when_a_target_changes_after_preparation()
    {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("retry-race").join("project.db");
        let representative = scalar_identity(1, "name", r"翻訳対象 \V[1]", "{}");
        let propagation = scalar_identity(2, "name", r"翻訳対象 \V[1]", "{}");
        let representative_state = Sha256Fingerprint::from_bytes([0x61; 32]);
        let propagation_state = Sha256Fingerprint::from_bytes([0x62; 32]);
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(
                    &representative,
                    r"翻訳対象 \V[1]",
                    "{}",
                    Some("缺少占位符的旧译文"),
                    Some(representative_state.as_bytes()),
                ),
                StoredUnit::new(
                    &propagation,
                    r"翻訳対象 \V[1]",
                    "{}",
                    Some("另一条缺少占位符的旧译文"),
                    Some(propagation_state.as_bytes()),
                ),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        let preparation = TranslationPlanPreparation::new(
            vec![
                TranslationInvalidation::rejected(
                    representative.clone(),
                    value("缺少占位符的旧译文"),
                    representative_state,
                    ProvenInvariantViolation::PlaceholderMismatch,
                    Sha256Fingerprint::from_bytes([0x71; 32]),
                    TranslationOrigin::Automatic,
                ),
                TranslationInvalidation::rejected(
                    propagation.clone(),
                    value("另一条缺少占位符的旧译文"),
                    propagation_state,
                    ProvenInvariantViolation::PlaceholderMismatch,
                    Sha256Fingerprint::from_bytes([0x72; 32]),
                    TranslationOrigin::Automatic,
                ),
            ],
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            2,
            0,
        );
        service
            .apply_preparation(&project_at(database_path.clone()), preparation)
            .await
            .expect("强不变量旧译文应先原子转入 Rejected");

        Connection::open(&database_path)
            .expect("数据库应可并发重开")
            .execute(
                "UPDATE rpg_maker_text_unit SET translation_content_json = ?, translation_state = ? WHERE rowid = 2",
                params![r#""并发译文""#, vec![0x7f_u8; 32]],
            )
            .expect("应可模拟 Preparation 后到达的并发译文");
        let translation = value(r"已修复的译文 \V[1]");
        let outcome = complete_outcome(vec![TranslationPatch::new(
            representative,
            vec![TranslationPropagationTarget::new(
                propagation,
                state_context(0x82),
            )],
            translation.clone(),
            state_context(0x81).applicability(),
        )]);
        let prepared = service
            .prepare_commit(outcome)
            .await
            .expect("修复结果应可编码");
        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("任一传播目标在 Preparation 后变化时，整项决定必须拒绝");
        assert!(matches!(
            error.source(),
            RpgMakerTranslationResultStorageError::StalePlan { .. }
        ));
        assert_eq!(
            stored_translation(&database_path, 1),
            (None, None),
            "后一个目标 CAS 失败必须回滚先写入的代表"
        );
        assert_eq!(
            stored_translation(&database_path, 2),
            (Some(value("并发译文")), Some(vec![0x7f; 32]))
        );
        assert_eq!(
            stored_rejection_count(&database_path),
            2,
            "失败事务不得提前清除 Rejected 恢复证据"
        );

        service.finalize().await.expect("测试数据库会话应正常终结");
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_request_failure_path_preserves_a_non_strong_outdated_translation() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("request-failed").join("project.db");
        let identity = scalar_identity(1, "name", "翻訳対象", "{}");
        let previous_state =
            crate::rpg_maker::applicability::unrelated_rpg_maker_applicability_for_test();
        create_unit_database(
            &database_path,
            &[StoredUnit::new(
                &identity,
                "翻訳対象",
                "{}",
                Some("仍可恢复的旧译文"),
                Some(previous_state.as_bytes()),
            )],
        );
        let storage = runtime_storage();
        let service = RpgMakerTranslationResultStorageService::new(storage.clone(), InlineCpu);
        service
            .apply_preparation(
                &project_at(database_path.clone()),
                TranslationPlanPreparation::new(
                    Vec::new(),
                    Vec::new(),
                    "[]".to_owned(),
                    "[]".to_owned(),
                    0,
                    1,
                    0,
                ),
            )
            .await
            .expect("没有强不变量违反时，Preparation 应只核对快照");

        // 模型请求失败时不会产生任务提交；Preparation 必须已经保留可恢复正文。
        assert_eq!(
            stored_translation(&database_path, 1),
            (
                Some(value("仍可恢复的旧译文")),
                Some(previous_state.as_bytes().to_vec()),
            )
        );

        service.finalize().await.expect("测试数据库会话应正常终结");
        storage.shutdown().await.expect("SQLite 根应正常关闭");
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
            context.applicability(),
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
                "CREATE TABLE metadata (
                    source_snapshot_fingerprint BLOB NOT NULL
                );
                INSERT INTO metadata (source_snapshot_fingerprint) VALUES (zeroblob(32));
                CREATE TABLE rpg_maker_asset_owner_state (
                    owner TEXT NOT NULL,
                    source_snapshot_fingerprint BLOB NOT NULL,
                    asset_snapshot_fingerprint BLOB NOT NULL
                );
                CREATE TABLE rpg_maker_translation_resource (
                    resource_kind TEXT PRIMARY KEY,
                    canonical_json TEXT NOT NULL
                );
                INSERT INTO rpg_maker_translation_resource (resource_kind, canonical_json) VALUES
                    ('terminology', '[]'),
                    ('placeholder_rules', '[]'),
                    ('write_back_layout_rules', '[]');
                CREATE TABLE rpg_maker_text_group (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL CHECK (group_id > 0),
                    group_location TEXT NOT NULL,
                    PRIMARY KEY (owner, group_id),
                    UNIQUE (owner, group_location)
                );
                CREATE TABLE rpg_maker_text_unit (
                    owner TEXT NOT NULL,
                    group_id INTEGER NOT NULL CHECK (group_id > 0),
                    unit_role TEXT NOT NULL,
                    rule_number INTEGER,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state BLOB
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
                );",
            )
            .expect("测试表应可创建");
        let mut group_ids = std::collections::HashMap::<(&'static str, String), i64>::new();
        for unit in units {
            let owner = unit.identity.owner().storage_name();
            let group_location = RpgMakerLocationCodec::encode(unit.identity.group_location())
                .expect("组位置应可编码");
            let group_key = (owner, group_location.clone());
            let group_id = if let Some(group_id) = group_ids.get(&group_key) {
                *group_id
            } else {
                let group_id =
                    i64::try_from(group_ids.len() + 1).expect("测试 Group 数量应可表示为 i64");
                connection
                    .execute(
                        "INSERT INTO rpg_maker_text_group (
                            owner, group_id, group_location
                        ) VALUES (?, ?, ?)",
                        params![owner, group_id, &group_location],
                    )
                    .expect("测试 Group 应可写入");
                group_ids.insert(group_key, group_id);
                group_id
            };
            connection
                .execute(
                    "INSERT INTO rpg_maker_text_unit (
                        owner, group_id, unit_role, rule_number, source_content_json,
                        source_context_json, translation_content_json, translation_state
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        owner,
                        group_id,
                        RpgMakerProjectionCodec::encode_role(unit.identity.role())
                            .expect("单元角色应可编码"),
                        match unit.identity.owner() {
                            RpgMakerAssetOwner::Builtin => None,
                            RpgMakerAssetOwner::Rules => Some(1_i64),
                        },
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

    fn stored_rejection_count(path: &std::path::Path) -> usize {
        let count = Connection::open(path)
            .expect("数据库应可重开")
            .query_row(
                "SELECT COUNT(*) FROM rpg_maker_rejected_translation",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("应可统计 Rejected");
        usize::try_from(count).expect("Rejected 数量应可表示为 usize")
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
            state_context(state_byte).applicability(),
        )
    }

    fn complete_outcome(patches: Vec<TranslationPatch>) -> Arc<TranslationTaskOutcome> {
        let mut decisions = patches
            .into_iter()
            .enumerate()
            .map(|(index, patch)| AcceptedTranslationDecision::new(task_id(index), patch));
        let first = decisions.next().expect("测试任务至少包含一项译文");
        Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                RpgMakerTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
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
        )
    }
}
