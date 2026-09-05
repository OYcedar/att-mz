//! RPG Maker 生产依赖错误到命令失败语义的转换。

use super::lifecycle::{ProductionProjectOpeningError, ProductionWorkspaceConvergenceError};
use super::lua::{ProjectLuaExecutionError, ProjectLuaPreflightError};
#[cfg(test)]
use super::rendering::CommandResultRenderer;
use super::run_plan::RunPlanResolutionError;
use super::translation_setup::{
    ProductionRpgMakerTranslation, ProductionTranslationExecutionBuildError,
    TranslationExecutionBuildFailureClass,
};
use crate::application::config::ConfigurationLoadError;
#[cfg(test)]
use crate::diagnostic::DiagnosticStage;
use crate::diagnostic::{
    Diagnostic, DiagnosticIssue, DiagnosticReport, IoFailure, RelatedFailureRelation,
    ReportedFailure, RpgMakerDiagnosticStage, RpgMakerIssue, RpgMakerProjectProblem,
    RuntimeComponent, RuntimeIssue, RuntimeOperation, SafeIdentifier, SafePath,
    SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure, SqliteIssue,
    SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect,
};
#[cfg(test)]
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::manual::ManualCommandError;
use crate::project_lease::ProjectCommandLeaseError;
use crate::project_lua::{ProjectLuaFailure, ProjectLuaRunError};
use crate::rpg_maker::extract::builtin::BuiltInExtractionError;
use crate::rpg_maker::extract::document::RpgMakerProjectDocumentReadingError;
use crate::rpg_maker::extract::rules::RulesExtractionError;
use crate::rpg_maker::extract::service::ExtractServiceError;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStoreError;
use crate::rpg_maker::init::ProjectWorkspaceConvergenceError;
use crate::rpg_maker::project::ExistingProjectOpeningError;
use crate::rpg_maker::project_database::{
    InvalidRunPlanValue, ProjectDatabaseReadError, ProjectRunPlanReadError,
};
use crate::rpg_maker::translate::executor::RpgMakerTranslationTaskExecutionError;
use crate::rpg_maker::translate::pipeline::RpgMakerTranslation;
use crate::rpg_maker::translate::service::TranslateServiceError;
#[cfg(test)]
use crate::rpg_maker::write_back::WriteBackPublishFailureState;
use crate::rpg_maker::write_back::asset_reader::RpgMakerWriteBackAssetReadingError;
use crate::rpg_maker::write_back::planner::{
    RpgMakerWriteBackServiceError, write_back_planning_compute_report,
};
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingError;
use crate::rpg_maker::write_back::{WriteBackPublishingDiagnostic, WriteBackServiceError};
use crate::runtime::cpu::{CpuExecutorStartError, CpuExecutorUnavailable};
use crate::runtime::filesystem::{SystemFileSystemBuildError, SystemFileSystemError};
use crate::runtime::llm::OpenAiExecutorBuildError;
use crate::runtime::sqlite::SqliteRuntimeError;
#[cfg(test)]
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError, DirectoryRecoveryError,
    StagingCleanupFailure,
};
use crate::storage::file_system::{ReadFileError, ResolveDirectoryError};
use std::error::Error;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::{fmt, io};

#[derive(Clone, Copy)]
enum InitFailureClass {
    ConfigurationOrInput,
    ProjectState,
    StateAppliedFinalizationFailed,
    RecoveryRequired,
    OutcomeUnknown,
    Internal,
}

pub(super) fn map_init_error(
    source: ProductionWorkspaceConvergenceError,
) -> ProductionCommandError {
    let (class, report) = init_workspace_failure_report(source);
    match class {
        InitFailureClass::ConfigurationOrInput => {
            ProductionCommandError::ConfigurationOrInput(Box::new(report))
        }
        InitFailureClass::ProjectState => ProductionCommandError::ProjectState(Box::new(report)),
        InitFailureClass::StateAppliedFinalizationFailed => {
            ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
        }
        InitFailureClass::RecoveryRequired => {
            ProductionCommandError::RecoveryRequired(Box::new(report))
        }
        InitFailureClass::OutcomeUnknown => {
            ProductionCommandError::OutcomeUnknown(Box::new(report))
        }
        InitFailureClass::Internal => ProductionCommandError::Internal(Box::new(report)),
    }
}

fn init_workspace_failure_report(
    source: ProductionWorkspaceConvergenceError,
) -> (InitFailureClass, ReportedFailure) {
    let diagnostic = source.diagnostic_report();
    let class = match diagnostic.effect() {
        StateEffect::AppliedFinalizationFailed => InitFailureClass::StateAppliedFinalizationFailed,
        StateEffect::RecoveryRequired => InitFailureClass::RecoveryRequired,
        StateEffect::OutcomeUnknown => InitFailureClass::OutcomeUnknown,
        StateEffect::Unchanged
        | StateEffect::ProgressPreserved
        | StateEffect::Applied
        | StateEffect::AppliedRunPlanNotSaved => match &source {
            ProjectWorkspaceConvergenceError::SourceGameRoot(_)
            | ProjectWorkspaceConvergenceError::ObserveGameLayout(_)
            | ProjectWorkspaceConvergenceError::InvalidGameLayout { .. }
            | ProjectWorkspaceConvergenceError::EngineWorkspaceRoot(_)
            | ProjectWorkspaceConvergenceError::MissingInitialSettings(_)
            | ProjectWorkspaceConvergenceError::ObserveInputSource(_) => {
                InitFailureClass::ConfigurationOrInput
            }
            ProjectWorkspaceConvergenceError::InvalidStageRequest(_) => InitFailureClass::Internal,
            _ => InitFailureClass::ProjectState,
        },
    };
    (class, ReportedFailure::new(diagnostic, source))
}

type ProductionDocumentReadError = RpgMakerProjectDocumentReadingError<
    SystemFileSystemError,
    SystemFileSystemError,
    CpuExecutorUnavailable,
>;
type ProductionExtractionStoreError =
    RpgMakerExtractionAssetStoreError<CpuExecutorUnavailable, SqliteRuntimeError>;
type ProductionExtractError = ExtractServiceError<
    BuiltInExtractionError<
        ProductionDocumentReadError,
        ProductionExtractionStoreError,
        CpuExecutorUnavailable,
    >,
    RulesExtractionError<
        ProductionDocumentReadError,
        ProductionExtractionStoreError,
        CpuExecutorUnavailable,
    >,
>;

pub(super) fn map_extract_error(error: ProductionExtractError) -> ProductionCommandError {
    match error {
        ExtractServiceError::BuiltIn(source) => {
            map_project_failure_report(source.into_diagnostic_failure())
        }
        ExtractServiceError::Rules {
            rules_path: _,
            completed_owners,
            source,
        } => {
            let mut report = source.into_diagnostic_failure();
            if !completed_owners.is_empty() {
                report = report.with_effect(StateEffect::ProgressPreserved);
            }
            map_project_failure_report(report)
        }
    }
}

type ProductionTranslationFailure = <ProductionRpgMakerTranslation as RpgMakerTranslation>::Error;

fn map_translation_failure(error: ProductionTranslationFailure) -> ProductionCommandError {
    use crate::rpg_maker::translate::pipeline::RpgMakerTranslationServiceError as TranslationError;

    match error {
        TranslationError::ReadAssets(source) => {
            let diagnostic = source.diagnostic_report();
            map_project_diagnostic(source, diagnostic)
        }
        TranslationError::PlanTasks(source) => {
            let report = source.into_reported_failure();
            if matches!(
                report.report().primary().resolution(),
                crate::diagnostic::DiagnosticResolution::FixInput
                    | crate::diagnostic::DiagnosticResolution::FixConfiguration
                    | crate::diagnostic::DiagnosticResolution::FixPlaceholderRules
                    | crate::diagnostic::DiagnosticResolution::CheckPathAndPermissions
            ) {
                ProductionCommandError::ConfigurationOrInput(Box::new(report))
            } else {
                map_project_failure_report(report)
            }
        }
        TranslationError::ApplyPreparation(source) => {
            map_project_failure_report(source.into_reported_failure())
        }
        TranslationError::ExecuteTask {
            task_index: _,
            source,
            diagnostic,
        } => match source {
            source @ RpgMakerTranslationTaskExecutionError::FatalRequest { .. } => {
                ProductionCommandError::ExternalModel(Box::new(ReportedFailure::new(
                    diagnostic, source,
                )))
            }
            source @ RpgMakerTranslationTaskExecutionError::ProcessResponse { .. } => {
                map_project_diagnostic(source, diagnostic)
            }
            source @ RpgMakerTranslationTaskExecutionError::LlmRequestCancelled { .. } => {
                ProductionCommandError::ExternalModel(Box::new(ReportedFailure::new(
                    diagnostic, source,
                )))
            }
            source @ RpgMakerTranslationTaskExecutionError::InternalInvariant { .. } => {
                ProductionCommandError::Internal(Box::new(ReportedFailure::new(diagnostic, source)))
            }
        },
        TranslationError::CommitTask {
            task_index: _,
            source,
            diagnostic,
        } => map_project_diagnostic(source, diagnostic),
        source @ TranslationError::InvalidTaskResultSequence { .. } => {
            let diagnostic = match &source {
                TranslationError::InvalidTaskResultSequence { diagnostic, .. } => {
                    diagnostic.clone()
                }
                _ => unreachable!("匹配的翻译结果序列错误必须保留其结构化诊断"),
            };
            ProductionCommandError::Internal(Box::new(ReportedFailure::new(diagnostic, source)))
        }
        TranslationError::FinalizeResultStore(source) => {
            map_project_failure_report(source.into_reported_failure())
        }
        TranslationError::OperationAndFinalization {
            primary,
            finalization,
        } => map_translation_failure(*primary)
            .with_related_finalization_report(finalization.into_reported_failure()),
    }
}
pub(super) fn map_project_diagnostic(
    source: impl Error + Send + Sync + 'static,
    diagnostic: DiagnosticReport,
) -> ProductionCommandError {
    let report = ReportedFailure::new(diagnostic, source);
    map_project_failure_report(report)
}

pub(super) fn map_project_failure_report(report: ReportedFailure) -> ProductionCommandError {
    let effect = report.report().effect();
    let resolution = report.report().primary().resolution();
    if effect == StateEffect::OutcomeUnknown {
        ProductionCommandError::OutcomeUnknown(Box::new(report))
    } else if effect == StateEffect::RecoveryRequired {
        ProductionCommandError::RecoveryRequired(Box::new(report))
    } else if effect == StateEffect::AppliedFinalizationFailed {
        ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(report))
    } else if resolution == crate::diagnostic::DiagnosticResolution::ReportBug {
        ProductionCommandError::Internal(Box::new(report))
    } else {
        ProductionCommandError::ProjectState(Box::new(report))
    }
}

pub(super) fn map_translate_error(
    error: TranslateServiceError<
        ProductionTranslationExecutionBuildError,
        ProductionTranslationFailure,
    >,
) -> ProductionCommandError {
    match error {
        TranslateServiceError::BuildExecution(source) => {
            ProductionCommandError::translation_execution_build(source)
        }
        TranslateServiceError::Translation { source } => map_translation_failure(source),
    }
}

type ProductionWriteBackPreparationError = RpgMakerWriteBackServiceError<
    RpgMakerWriteBackAssetReadingError<SqliteRuntimeError, CpuExecutorUnavailable>,
    RpgMakerWriteBackDocumentRewritingError<ProductionDocumentReadError, CpuExecutorUnavailable>,
    CpuExecutorUnavailable,
>;

fn write_back_preparation_failure(error: ProductionWriteBackPreparationError) -> ReportedFailure {
    match error {
        RpgMakerWriteBackServiceError::ReadAssets(source) => source.into_reported_failure(),
        RpgMakerWriteBackServiceError::SchedulePlanning(source) => {
            let report = write_back_planning_compute_report(&source);
            ReportedFailure::new(report, source)
        }
        RpgMakerWriteBackServiceError::InvalidPlaceholder(source) => {
            let report = source.diagnostic_report();
            ReportedFailure::new(report, source)
        }
        RpgMakerWriteBackServiceError::InvalidPlan(source) => {
            let report = source.diagnostic_report();
            ReportedFailure::new(report, source)
        }
        RpgMakerWriteBackServiceError::RewriteDocuments(source) => source.into_reported_failure(),
    }
}
pub(super) fn map_write_back_error<PE>(
    error: WriteBackServiceError<ProductionWriteBackPreparationError, PE>,
) -> ProductionCommandError
where
    PE: Error + WriteBackPublishingDiagnostic + Send + Sync + 'static,
{
    match error {
        WriteBackServiceError::CancellationDiscard {
            candidate_root: _,
            discard,
        } => {
            let report = discard.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::Prepare(source) => {
            map_project_failure_report(write_back_preparation_failure(source))
        }
        WriteBackServiceError::PrepareCandidate(source) => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::ValidateCandidate {
            candidate_root: _,
            source,
        } => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
        WriteBackServiceError::ValidateCandidateAndDiscard {
            candidate_root: _,
            source,
            discard,
        } => {
            let report = source.into_write_back_failure_report().with_related(
                RelatedFailureRelation::Discard,
                discard.into_write_back_failure_report(),
            );
            map_project_failure_report(report)
        }
        WriteBackServiceError::Publish { state: _, source } => {
            let report = source.into_write_back_failure_report();
            map_project_failure_report(report)
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProductionCommandError {
    ConfigurationOrInput(Box<ReportedFailure>),
    ProjectUnavailable(Box<ReportedFailure>),
    ProjectState(Box<ReportedFailure>),
    ExternalModel(Box<ReportedFailure>),
    ResultAppliedButRunPlanNotSaved(Box<ReportedFailure>),
    RunPlanOutcomeUnknown(Box<ReportedFailure>),
    StateAppliedButFinalizationFailed(Box<ReportedFailure>),
    RecoveryRequired(Box<ReportedFailure>),
    OutcomeUnknown(Box<ReportedFailure>),
    Internal(Box<ReportedFailure>),
    Signal(Box<ReportedFailure>),
}

impl ProductionCommandError {
    pub(super) fn manual(source: ManualCommandError) -> Self {
        let report = source.diagnostic_report();
        let failure = Box::new(ReportedFailure::new(report.clone(), source));
        match report.effect() {
            StateEffect::Unchanged => Self::ConfigurationOrInput(failure),
            StateEffect::ProgressPreserved => Self::ProjectState(failure),
            StateEffect::Applied => Self::StateAppliedButFinalizationFailed(failure),
            StateEffect::AppliedRunPlanNotSaved => Self::ResultAppliedButRunPlanNotSaved(failure),
            StateEffect::AppliedFinalizationFailed => {
                Self::StateAppliedButFinalizationFailed(failure)
            }
            StateEffect::RecoveryRequired => Self::RecoveryRequired(failure),
            StateEffect::OutcomeUnknown => Self::OutcomeUnknown(failure),
        }
    }

    pub(crate) fn stdout_write(source: io::Error) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::WriteStdout,
                failure: IoFailure::from_error(&source),
            }),
        );
        Self::StateAppliedButFinalizationFailed(Box::new(ReportedFailure::new(report, source)))
    }

    pub(crate) fn stderr_write(source: io::Error) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::WriteStderr,
                failure: IoFailure::from_error(&source),
            }),
        );
        Self::StateAppliedButFinalizationFailed(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn into_reported_failure(self) -> ReportedFailure {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::RecoveryRequired(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => *report,
        }
    }

    pub(super) fn with_related_finalization_report(self, related: ReportedFailure) -> Self {
        let primary_outcome_unknown = matches!(
            &self,
            Self::OutcomeUnknown(_) | Self::RunPlanOutcomeUnknown(_)
        );
        let related_outcome_unknown = related.report().effect() == StateEffect::OutcomeUnknown;
        let primary_recovery_required = matches!(&self, Self::RecoveryRequired(_));
        let related_recovery_required = related.report().effect() == StateEffect::RecoveryRequired;
        let report = self
            .into_reported_failure()
            .with_related(RelatedFailureRelation::Finalization, related);
        if primary_outcome_unknown || related_outcome_unknown {
            Self::OutcomeUnknown(Box::new(report))
        } else if primary_recovery_required || related_recovery_required {
            Self::RecoveryRequired(Box::new(report))
        } else {
            Self::StateAppliedButFinalizationFailed(Box::new(report))
        }
    }

    pub(super) fn configuration_load(source: ConfigurationLoadError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn input_directory(source: ResolveDirectoryError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn run_plan_resolution(source: RunPlanResolutionError) -> Self {
        let problem = match &source {
            RunPlanResolutionError::InitPathRequired
            | RunPlanResolutionError::NoReusableExtractPlan
            | RunPlanResolutionError::ProfileRequired => RpgMakerProjectProblem::RunPlanRequired,
            RunPlanResolutionError::SavedProfileUnavailable { profile_id } => {
                RpgMakerProjectProblem::SavedProfileUnavailable {
                    profile_id: SafeIdentifier::from_validated(profile_id),
                }
            }
        };
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::project(
                RpgMakerDiagnosticStage::CommandPreparation,
                problem,
            )),
        );
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn translation_execution_build(
        source: ProductionTranslationExecutionBuildError,
    ) -> Self {
        let class = source.class;
        let diagnostic = source.diagnostic().clone();
        let report = Box::new(ReportedFailure::new(diagnostic, source));
        match class {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput => {
                Self::ConfigurationOrInput(report)
            }
            TranslationExecutionBuildFailureClass::Internal => Self::Internal(report),
        }
    }

    pub(super) fn project_lease(
        source: ProjectCommandLeaseError<Box<SystemFileSystemError>>,
    ) -> Self {
        let diagnostic =
            source.diagnostic_report_at(crate::diagnostic::FileSystemDiagnosticStage::Project);
        Self::ProjectUnavailable(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn existing_project_opening(source: ProductionProjectOpeningError) -> Self {
        let diagnostic = source.diagnostic_report();
        let unavailable = matches!(
            &source,
            ExistingProjectOpeningError::ReadProjectRecord(
                ProjectDatabaseReadError::DatabaseNotFound { .. }
            ) | ExistingProjectOpeningError::ResolveSourceData(
                ResolveDirectoryError::NotFound { .. } | ResolveDirectoryError::NotDirectory { .. }
            ) | ExistingProjectOpeningError::ResolveSourceJs(
                ResolveDirectoryError::NotFound { .. } | ResolveDirectoryError::NotDirectory { .. }
            )
        );
        let report = ReportedFailure::new(diagnostic, source);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    pub(super) fn project_run_plan_read(
        source: ProjectRunPlanReadError<SqliteRuntimeError>,
    ) -> Self {
        let unavailable = matches!(source, ProjectRunPlanReadError::DatabaseNotFound { .. });
        let diagnostic = source.diagnostic_report();
        let report = ReportedFailure::new(diagnostic, source);
        if unavailable {
            Self::ProjectUnavailable(Box::new(report))
        } else {
            Self::ProjectState(Box::new(report))
        }
    }

    pub(super) fn file_system_build(source: SystemFileSystemBuildError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn sqlite_start(source: SqliteRuntimeError) -> Self {
        let diagnostic = source.startup_diagnostic_report();
        Self::Internal(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn http_client_build(source: OpenAiExecutorBuildError) -> Self {
        let configuration = matches!(
            source,
            OpenAiExecutorBuildError::InvalidProxy(_)
                | OpenAiExecutorBuildError::InvalidCertificate(_)
        );
        let report = ReportedFailure::new(
            DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic()),
            source,
        );
        if configuration {
            Self::ConfigurationOrInput(Box::new(report))
        } else {
            Self::Internal(Box::new(report))
        }
    }

    pub(super) fn cpu_start(source: CpuExecutorStartError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn pem_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn lua_script_read(source: ReadFileError<SystemFileSystemError>) -> Self {
        let diagnostic = source.command_preparation_diagnostic_report();
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn project_lua_worker(source: tokio::task::JoinError) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn manual_worker(source: tokio::task::JoinError) -> Self {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        Self::Internal(Box::new(ReportedFailure::new(report, source)))
    }

    pub(super) fn project_lua_preflight(source: ProjectLuaFailure, database_path: &Path) -> Self {
        let class = match &source {
            ProjectLuaFailure::Context(_) | ProjectLuaFailure::Panicked => 1,
            _ => 2,
        };
        let report = source.preflight_diagnostic_report(database_path);
        let reported = ReportedFailure::new(report, ProjectLuaPreflightError(source));
        match class {
            0 => Self::ProjectState(Box::new(reported)),
            1 => Self::Internal(Box::new(reported)),
            _ => Self::ConfigurationOrInput(Box::new(reported)),
        }
    }

    pub(super) fn project_lua_execution(source: ProjectLuaExecutionError) -> Self {
        let (class, report) = match &source {
            ProjectLuaExecutionError::Open { path, source } => (
                0_u8,
                DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::sqlite(SqliteIssue::new(
                        SqliteDiagnosticContext::new(
                            SqliteDiagnosticStage::Lua,
                            SqliteOperation::Open,
                            SqliteTransactionState::NotStarted,
                        ),
                        SqliteProblem::Driver {
                            database: SafePath::new(path),
                            query_id: None,
                            query_ordinal: None,
                            failure: SqliteDriverFailure::from_error(source),
                        },
                    )),
                ),
            ),
            ProjectLuaExecutionError::Run { path, source } => {
                let class = match source {
                    ProjectLuaRunError::RollbackOutcomeUnknown { .. }
                    | ProjectLuaRunError::SavepointOutcomeUnknown(_) => 1,
                    ProjectLuaRunError::NotStarted(failure)
                    | ProjectLuaRunError::Failed(failure)
                    | ProjectLuaRunError::RolledBack(failure) => match failure {
                        ProjectLuaFailure::Database(_) | ProjectLuaFailure::Cancelled => 0,
                        ProjectLuaFailure::Context(_) | ProjectLuaFailure::Panicked => 3,
                        _ => 2,
                    },
                };
                (class, source.diagnostic_report(path))
            }
        };
        let reported = ReportedFailure::new(report, source);
        match class {
            0 => Self::ProjectState(Box::new(reported)),
            1 => Self::OutcomeUnknown(Box::new(reported)),
            2 => Self::ConfigurationOrInput(Box::new(reported)),
            _ => Self::Internal(Box::new(reported)),
        }
    }

    pub(super) fn invalid_run_plan(source: InvalidRunPlanValue) -> Self {
        let diagnostic = source.diagnostic_report(RpgMakerDiagnosticStage::CommandPreparation);
        Self::ConfigurationOrInput(Box::new(ReportedFailure::new(diagnostic, source)))
    }

    pub(super) fn signal(source: io::Error, outcome: SignalOutcomeSource) -> Self {
        let effect = match &outcome {
            SignalOutcomeSource::CompletedStateApplied => StateEffect::AppliedFinalizationFailed,
            SignalOutcomeSource::Cancelled | SignalOutcomeSource::CommandFailed(_) => {
                StateEffect::Unchanged
            }
        };
        let signal = ReportedFailure::new(
            DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TerminationSignals,
                    operation: RuntimeOperation::ReceiveTerminationSignal,
                    failure: IoFailure::from_error(&source),
                }),
            ),
            source,
        );
        match outcome {
            SignalOutcomeSource::CommandFailed(command) => Self::Signal(Box::new(
                command
                    .into_reported_failure()
                    .with_related(RelatedFailureRelation::Shutdown, signal),
            )),
            SignalOutcomeSource::CompletedStateApplied | SignalOutcomeSource::Cancelled => {
                Self::Signal(Box::new(signal))
            }
        }
    }

    pub(crate) fn failure_report(&self) -> &ReportedFailure {
        match self {
            Self::ConfigurationOrInput(report)
            | Self::ProjectUnavailable(report)
            | Self::ProjectState(report)
            | Self::ExternalModel(report)
            | Self::ResultAppliedButRunPlanNotSaved(report)
            | Self::RunPlanOutcomeUnknown(report)
            | Self::StateAppliedButFinalizationFailed(report)
            | Self::RecoveryRequired(report)
            | Self::OutcomeUnknown(report)
            | Self::Internal(report)
            | Self::Signal(report) => report.as_ref(),
        }
    }

    pub(crate) fn manual_error(&self) -> Option<&ManualCommandError> {
        let Self::ConfigurationOrInput(report) = self else {
            return None;
        };
        report
            .report()
            .related()
            .is_empty()
            .then(|| report.source_error().downcast_ref::<ManualCommandError>())
            .flatten()
    }

    pub(super) fn was_cancelled_wait(&self) -> bool {
        let report = self.failure_report();
        report.report().related().is_empty()
            && matches!(
                report.report().primary().issue(),
                DiagnosticIssue::Runtime(RuntimeIssue::Cancelled { .. })
            )
    }
}

pub(super) enum SignalOutcomeSource {
    CompletedStateApplied,
    Cancelled,
    CommandFailed(ProductionCommandError),
}

impl fmt::Display for ProductionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure_report().fmt(formatter)
    }
}

impl Error for ProductionCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure_report().source_error())
    }
}
#[cfg(test)]
mod command_result_renderer_tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct TestFailure;

    impl fmt::Display for TestFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试失败")
        }
    }

    impl Error for TestFailure {}

    fn test_report(effect: StateEffect) -> ReportedFailure {
        ReportedFailure::new(
            DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
                    component: RuntimeComponent::Process,
                    operation: RuntimeOperation::Shutdown,
                }),
            ),
            TestFailure,
        )
    }

    #[derive(Debug)]
    struct TestPublishingFailure {
        effect: StateEffect,
        related_effect: Option<StateEffect>,
    }

    impl fmt::Display for TestPublishingFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试发布失败")
        }
    }

    impl Error for TestPublishingFailure {}

    impl WriteBackPublishingDiagnostic for TestPublishingFailure {
        fn into_write_back_failure_report(self) -> ReportedFailure {
            let Self {
                effect,
                related_effect,
            } = self;
            let report = ReportedFailure::new(
                test_report(effect).into_report(),
                TestPublishingFailure {
                    effect,
                    related_effect,
                },
            );
            match related_effect {
                Some(related) => {
                    report.with_related(RelatedFailureRelation::Cleanup, test_report(related))
                }
                None => report,
            }
        }
    }

    fn report_tree_contains(
        report: &DiagnosticReport,
        predicate: &impl Fn(&DiagnosticReport) -> bool,
    ) -> bool {
        predicate(report)
            || report
                .related()
                .iter()
                .any(|related| report_tree_contains(related.report(), predicate))
    }

    fn manual_read_failure() -> ManualCommandError {
        ManualCommandError::Document(crate::manual::ManualDocumentError::Read {
            path: PathBuf::from("C:/project/manual.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "测试读取失败"),
        })
    }

    #[test]
    fn only_direct_manual_failure_uses_detailed_manual_renderer() {
        let direct = ProductionCommandError::manual(manual_read_failure());
        assert!(direct.manual_error().is_some());

        let signal = ProductionCommandError::signal(
            io::Error::other("测试信号失败"),
            SignalOutcomeSource::CommandFailed(ProductionCommandError::manual(
                manual_read_failure(),
            )),
        );
        assert!(
            signal.manual_error().is_none(),
            "Signal 外层的类型化主错误和 related 不能被递归 Manual 呈现替换"
        );
        assert_eq!(signal.failure_report().report().related().len(), 1);
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(&signal), None, &localizer, &mut stderr)
            .expect("Signal 外层诊断应可呈现");
        let stderr = String::from_utf8(stderr).expect("诊断必须是 UTF-8");
        assert!(
            stderr.contains(&localizer.format(UiMessage::DiagnosticRelated {
                relation: RelatedFailureRelation::Shutdown.as_str(),
            }))
        );

        let finalization = ProductionCommandError::manual(manual_read_failure())
            .with_related_finalization_report(test_report(StateEffect::AppliedFinalizationFailed));
        assert!(
            finalization.manual_error().is_none(),
            "Finalization 外层必须保留类型化相关报告"
        );
        assert_eq!(finalization.failure_report().report().related().len(), 1);
    }

    #[test]
    fn recovery_required_is_not_collapsed_into_state_applied_finalization_failure() {
        let report = test_report(StateEffect::Unchanged).with_related(
            RelatedFailureRelation::Cleanup,
            test_report(StateEffect::RecoveryRequired),
        );
        let error = map_project_failure_report(report);
        assert!(matches!(error, ProductionCommandError::RecoveryRequired(_)));

        assert_eq!(
            error.failure_report().report().effect(),
            StateEffect::RecoveryRequired
        );
    }

    #[test]
    fn outcome_unknown_has_priority_over_recovery_required() {
        let report = test_report(StateEffect::RecoveryRequired).with_related(
            RelatedFailureRelation::Finalization,
            test_report(StateEffect::OutcomeUnknown),
        );
        assert!(matches!(
            map_project_failure_report(report),
            ProductionCommandError::OutcomeUnknown(_)
        ));
    }

    #[test]
    fn init_prepare_preserves_recovery_and_unknown_source_impacts() {
        for (source, expected) in [
            (
                SystemFileSystemError::JournalCorrupt {
                    path: PathBuf::from("C:/project/.directory-publish/workspace/journal"),
                    violation: crate::diagnostic::FileSystemJournalViolation::CrcMismatch {
                        frame_index: 1,
                    },
                },
                StateEffect::RecoveryRequired,
            ),
            (
                SystemFileSystemError::OutcomeUnknown {
                    target_root: PathBuf::from("C:/project/workspace"),
                    artifacts: vec![PathBuf::from(
                        "C:/project/.directory-publish/workspace/journal",
                    )],
                    violation:
                        crate::diagnostic::FileSystemRecoveryViolation::TargetIdentityUnknown,
                },
                StateEffect::OutcomeUnknown,
            ),
        ] {
            let mapped = map_init_error(ProjectWorkspaceConvergenceError::Prepare(
                DirectoryPrepareError::NotPrepared {
                    target_root: PathBuf::from("C:/project/workspace"),
                    source: Box::new(source),
                    cleanup_failure: None,
                },
            ));
            assert_eq!(mapped.failure_report().report().effect(), expected);
            assert!(matches!(
                (expected, mapped),
                (
                    StateEffect::RecoveryRequired,
                    ProductionCommandError::RecoveryRequired(_)
                ) | (
                    StateEffect::OutcomeUnknown,
                    ProductionCommandError::OutcomeUnknown(_)
                )
            ));
        }
    }

    #[test]
    fn init_explicit_recovery_preserves_recovery_and_unknown_impacts() {
        for (source, expected) in [
            (
                SystemFileSystemError::JournalCorrupt {
                    path: PathBuf::from("C:/project/.directory-publish/workspace/journal"),
                    violation: crate::diagnostic::FileSystemJournalViolation::CrcMismatch {
                        frame_index: 1,
                    },
                },
                StateEffect::RecoveryRequired,
            ),
            (
                SystemFileSystemError::OutcomeUnknown {
                    target_root: PathBuf::from("C:/project/workspace"),
                    artifacts: vec![PathBuf::from(
                        "C:/project/.directory-publish/workspace/journal",
                    )],
                    violation:
                        crate::diagnostic::FileSystemRecoveryViolation::TargetIdentityUnknown,
                },
                StateEffect::OutcomeUnknown,
            ),
        ] {
            let workspace_error: ProductionWorkspaceConvergenceError =
                ProjectWorkspaceConvergenceError::Recover(DirectoryRecoveryError::new(
                    PathBuf::from("C:/project/workspace"),
                    Box::new(source),
                ));
            let mapped = map_init_error(workspace_error);
            assert_eq!(mapped.failure_report().report().effect(), expected);
            assert!(matches!(
                (expected, mapped),
                (
                    StateEffect::RecoveryRequired,
                    ProductionCommandError::RecoveryRequired(_)
                ) | (
                    StateEffect::OutcomeUnknown,
                    ProductionCommandError::OutcomeUnknown(_)
                )
            ));
        }
    }

    #[test]
    fn write_back_preparation_and_discard_preserve_strongest_impact() {
        type Error =
            WriteBackServiceError<ProductionWriteBackPreparationError, TestPublishingFailure>;

        let prepared = map_write_back_error(Error::PrepareCandidate(TestPublishingFailure {
            effect: StateEffect::OutcomeUnknown,
            related_effect: None,
        }));
        assert!(matches!(
            prepared,
            ProductionCommandError::OutcomeUnknown(_)
        ));

        let discarded = map_write_back_error(Error::ValidateCandidateAndDiscard {
            candidate_root: PathBuf::from("C:/project/candidate"),
            source: TestPublishingFailure {
                effect: StateEffect::Unchanged,
                related_effect: None,
            },
            discard: TestPublishingFailure {
                effect: StateEffect::RecoveryRequired,
                related_effect: None,
            },
        });
        assert!(matches!(
            discarded,
            ProductionCommandError::RecoveryRequired(_)
        ));

        let publish_cleanup = map_write_back_error(Error::Publish {
            state: WriteBackPublishFailureState::NotPublished {
                output_root: PathBuf::from("C:/project/write_back"),
                residual_paths: vec![PathBuf::from("C:/project/.write-back-residual")],
            },
            source: TestPublishingFailure {
                effect: StateEffect::Unchanged,
                related_effect: Some(StateEffect::RecoveryRequired),
            },
        });
        assert!(matches!(
            publish_cleanup,
            ProductionCommandError::RecoveryRequired(_)
        ));
    }

    fn assert_init_recovery_required(error: ProductionWorkspaceConvergenceError) {
        let mapped = map_init_error(error);
        let ProductionCommandError::RecoveryRequired(report) = mapped else {
            panic!("Init 清理失败必须映射为 RecoveryRequired");
        };
        assert_eq!(report.report().effect(), StateEffect::RecoveryRequired);
    }

    #[test]
    fn init_cancellation_cleanup_failure_requires_recovery() {
        assert_init_recovery_required(ProjectWorkspaceConvergenceError::CancellationCleanup(
            DirectoryDiscardError::new(
                PathBuf::from("C:/project/.init-candidate"),
                Box::new(SystemFileSystemError::Closed),
            ),
        ));
    }

    #[test]
    fn init_prepare_cleanup_failure_preserves_related_recovery_diagnostic() {
        assert_init_recovery_required(ProjectWorkspaceConvergenceError::Prepare(
            DirectoryPrepareError::NotPrepared {
                target_root: PathBuf::from("C:/project/workspace"),
                source: Box::new(SystemFileSystemError::Closed),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("C:/project/.prepare-residual"),
                    Box::new(SystemFileSystemError::Closed),
                )),
            },
        ));
    }

    #[test]
    fn init_publish_cleanup_failure_preserves_publication_recovery_diagnostic() {
        let mapped = map_init_error(ProjectWorkspaceConvergenceError::Publish(
            DirectoryPublishError::NotPublished {
                target_root: PathBuf::from("C:/project/workspace"),
                source: Box::new(SystemFileSystemError::Closed),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("C:/project/.publish-residual"),
                    Box::new(SystemFileSystemError::Closed),
                )),
            },
        ));
        let ProductionCommandError::RecoveryRequired(report) = mapped else {
            panic!("Init 发布清理失败必须映射为 RecoveryRequired");
        };
        assert!(report_tree_contains(report.report(), &|diagnostic| {
            diagnostic.primary().stage() == DiagnosticStage::Publication
                && diagnostic.effect() == StateEffect::RecoveryRequired
        }));
    }
}
