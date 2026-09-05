//! Generic 命令的类型化错误与诊断转换。

use super::lifecycle::GenericApplicationScopePanicked;
use super::{generic_count, generic_task_ordinal};
use crate::application::config::ConfigurationLoadError;
use crate::application::translation_prompt::PromptResourceLoadError;
use crate::diagnostic::{
    ByteRange, Diagnostic, DiagnosticReport, FileSystemDiagnosticContext,
    FileSystemDiagnosticStage, FileSystemIssue, FileSystemOperation, FileSystemPathViolation,
    FileSystemProblem, GenericDiagnosticStage, GenericIssue, GenericJsonErrorCategory,
    GenericProblem, GenericResponseReviewFinding, GenericTaskResponseJsonCategory,
    GenericTaskResponseProblem, GenericTaskUnavailableReason, GenericTranslationPreparationProblem,
    GenericUnitLocator as DiagnosticGenericUnitLocator, GenericWriteBackTextSide,
    GenericWriteBackUnitProblem, IoFailure, Pcre2Failure, Pcre2FailureKind, PlaceholderIssue,
    PlaceholderMatchRangeViolation as DiagnosticMatchRangeViolation,
    PlaceholderRuleOrigin as DiagnosticPlaceholderRuleOrigin,
    PlaceholderRuleSource as DiagnosticPlaceholderRuleSource,
    PlaceholderWorkerOperation as DiagnosticPlaceholderWorkerOperation, RelatedFailureRelation,
    ReportedFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation, SafeIdentifier, SafeIoKind,
    SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure, SqliteIssue,
    SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect, TranslationIssue,
    TranslationPlanningResourceOrigin, TranslationTaskPlanningProblem,
};
use crate::execution::cpu::CpuTaskExecutionError;
use crate::generic::write_back::materialization::GenericScratchError;
use crate::generic::{
    CommitTranslationResultsOutcome, GenericPlaceholderRuleSource, GenericPlanningError,
    GenericPlanningUnitLocator, GenericPreparationError, GenericUnitLocator, GenericWriteBackError,
    ResponseProblem, TranslationReview, generic_language_projection_problem,
    generic_placeholder_multiset_problem,
};
use crate::language::{LanguageId, LanguageModuleCatalogError};
use crate::manual::ManualCommandError;
use crate::project_lease::ProjectCommandLeaseError;
use crate::project_lua::{ProjectLuaFailure, ProjectLuaRunError};
use crate::runtime::cpu::{
    CpuExecutorShutdownError, CpuExecutorStartError, CpuExecutorUnavailable,
};
use crate::runtime::filesystem::{SystemFileSystemBuildError, SystemFileSystemError};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::ReadFileError;
use crate::translation::candidate_validation::ReviewFinding;
use crate::translation::layout_rules::LayoutRulesError;
use crate::translation::placeholder::{
    PlaceholderMatchRangeViolation, PlaceholderPcre2ErrorKind, PlaceholderProtectionError,
    PlaceholderRestoreError, PlaceholderRuleOrigin, PlaceholderWorkerOperation,
};
use crate::translation::planning_resource::TranslationPlanningResourceReadingError;
use crate::translation::task_planning::TaskPlanningError;
use crate::translation_protocol::{
    TranslationTaskResponseJsonErrorCategory, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{fmt, io};

#[derive(Debug)]
pub(crate) struct GenericShutdownError {
    pub(super) component: &'static str,
    pub(super) failure: ReportedFailure,
}

impl GenericShutdownError {
    pub(super) fn new(
        component: &'static str,
        source: impl Error + Send + Sync + 'static,
        report: DiagnosticReport,
    ) -> Self {
        Self {
            component,
            failure: ReportedFailure::new(report, source),
        }
    }

    pub(super) fn cpu(source: CpuExecutorShutdownError) -> Self {
        let report =
            DiagnosticReport::new(StateEffect::AppliedFinalizationFailed, source.diagnostic());
        Self::new("CPU executor", source, report)
    }

    pub(super) fn file_system(source: SystemFileSystemError) -> Self {
        let report = source.shutdown_diagnostic_report();
        Self::new("filesystem", source, report)
    }

    pub(super) fn terminal_progress(source: crate::progress::TerminalProgressFailure) -> Self {
        let report = source.diagnostic_report();
        Self::new("terminal progress", source, report)
    }

    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        self.failure.report().clone()
    }
}

impl fmt::Display for GenericShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} 关闭失败：{}", self.component, self.failure)
    }
}

impl Error for GenericShutdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure.source_error())
    }
}

/// 候选目录或 Generic scratch 清理的类型化失败。
#[derive(Debug)]
pub(crate) struct GenericDiscardFailure {
    pub(super) failure: ReportedFailure,
}

impl GenericDiscardFailure {
    pub(super) fn new(
        report: DiagnosticReport,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            failure: ReportedFailure::new(report, source),
        }
    }

    pub(super) fn diagnostic_report(&self) -> DiagnosticReport {
        self.failure.report().clone()
    }

    #[cfg(test)]
    pub(super) fn source_error(&self) -> &(dyn Error + 'static) {
        self.failure.source_error()
    }
}

impl fmt::Display for GenericDiscardFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl Error for GenericDiscardFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failure.source_error())
    }
}

/// Generic 命令仍掌握完整阶段时建立的具体失败。
#[derive(Debug)]
pub(crate) enum GenericCommandError {
    Cancelled,
    Operation {
        failure: ReportedFailure,
    },
    Signal {
        source: io::Error,
        operation: Option<Box<GenericCommandError>>,
        state_applied: bool,
    },
    PublishDiscard {
        operation: Box<GenericCommandError>,
        discard: GenericDiscardFailure,
    },
}

impl GenericCommandError {
    pub(super) fn reported(
        source: impl Error + Send + Sync + 'static,
        report: DiagnosticReport,
    ) -> Self {
        Self::Operation {
            failure: ReportedFailure::new(report, source),
        }
    }

    pub(super) fn configuration(source: ConfigurationLoadError) -> Self {
        let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
        Self::reported(source, report)
    }

    pub(super) fn missing_profile_id() -> Self {
        Self::reported(
            MissingGenericProfileId,
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::Translate,
                    GenericProblem::MissingProfileId,
                )),
            ),
        )
    }

    pub(super) fn language_module(
        source: LanguageModuleCatalogError,
        target_language: &LanguageId,
    ) -> Self {
        let LanguageModuleCatalogError::UnknownLanguageId {
            language_id,
            available_ids,
        } = &source;
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::LanguageModuleUnavailable {
                requested_language: SafeIdentifier::from_validated(language_id.as_str()),
                target_language: SafeIdentifier::from_validated(target_language.as_str()),
                available_languages: available_ids
                    .iter()
                    .map(|language| SafeIdentifier::from_validated(language.as_str()))
                    .collect(),
            }),
        );
        Self::reported(source, report)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    pub(super) fn is_application_scope_panic(&self) -> bool {
        match self {
            Self::Operation { failure } => failure
                .source_error()
                .is::<GenericApplicationScopePanicked>(),
            Self::Signal {
                operation: Some(operation),
                ..
            }
            | Self::PublishDiscard { operation, .. } => operation.is_application_scope_panic(),
            Self::Cancelled
            | Self::Signal {
                operation: None, ..
            } => false,
        }
    }
}

impl fmt::Display for GenericCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 命令已取消"),
            Self::Operation { failure } => failure.fmt(formatter),
            Self::Signal {
                source, operation, ..
            } => {
                write!(formatter, "接收 Windows 终止信号失败：{source}")?;
                if let Some(operation) = operation {
                    write!(formatter, "；同时发生业务失败：{operation}")?;
                }
                Ok(())
            }
            Self::PublishDiscard {
                operation, discard, ..
            } => {
                write!(formatter, "{operation}；清理未发布候选也失败：{discard}")
            }
        }
    }
}

impl Error for GenericCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Operation { failure } => Some(failure.source_error()),
            Self::Signal { source, .. } => Some(source),
            Self::PublishDiscard { operation, .. } => Some(operation.as_ref()),
            Self::Cancelled => None,
        }
    }
}

impl GenericCommandError {
    pub(crate) fn manual_error(&self) -> Option<&ManualCommandError> {
        match self {
            Self::Operation { failure } => {
                failure.source_error().downcast_ref::<ManualCommandError>()
            }
            Self::Cancelled | Self::Signal { .. } | Self::PublishDiscard { .. } => None,
        }
    }
}

pub(crate) fn generic_command_error_report(error: &GenericCommandError) -> DiagnosticReport {
    match error {
        GenericCommandError::Cancelled => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericCommandError::Operation { failure } => failure.report().clone(),
        GenericCommandError::Signal {
            source,
            operation,
            state_applied,
        } => {
            let effect = if *state_applied {
                StateEffect::AppliedFinalizationFailed
            } else {
                operation
                    .as_ref()
                    .map_or(StateEffect::Unchanged, |operation| {
                        generic_command_error_report(operation).effect()
                    })
            };
            let mut report = DiagnosticReport::new(
                effect,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TerminationSignals,
                    operation: RuntimeOperation::ReceiveTerminationSignal,
                    failure: IoFailure::from_error(source),
                }),
            );
            if let Some(operation) = operation {
                report = report.with_related(
                    RelatedFailureRelation::Finalization,
                    generic_command_error_report(operation),
                );
            }
            report
        }
        GenericCommandError::PublishDiscard { operation, discard } => {
            generic_command_error_report(operation)
                .with_related(RelatedFailureRelation::Discard, discard.diagnostic_report())
        }
    }
}

fn generic_read_file_report(
    source: &ReadFileError<SystemFileSystemError>,
    stage: FileSystemDiagnosticStage,
) -> DiagnosticReport {
    let context = FileSystemDiagnosticContext::new(stage, FileSystemOperation::Read);
    match source {
        ReadFileError::NotFound { path } => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::file_system(FileSystemIssue::new(
                context,
                FileSystemProblem::NotFound {
                    path: SafePath::new(path),
                },
            )),
        ),
        ReadFileError::NotFile { path } => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::file_system(FileSystemIssue::new(
                context,
                FileSystemProblem::NotFile {
                    path: SafePath::new(path),
                },
            )),
        ),
        ReadFileError::Io { source, .. } => {
            source.diagnostic_report(context, StateEffect::Unchanged)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MissingGenericProfileId;

impl fmt::Display for MissingGenericProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("首次 Generic Translate 必须显式提供 Profile ID")
    }
}

impl Error for MissingGenericProfileId {}

pub(super) fn generic_blocking_join_failure(
    source: tokio::task::JoinError,
    effect: StateEffect,
) -> GenericCommandError {
    let issue = if source.is_panic() {
        RuntimeIssue::WorkerPanicked {
            component: RuntimeComponent::TokioRuntime,
            operation: RuntimeOperation::ExecuteTask,
        }
    } else {
        RuntimeIssue::ExecutorClosed {
            component: RuntimeComponent::TokioRuntime,
            operation: RuntimeOperation::ExecuteTask,
        }
    };
    GenericCommandError::reported(
        source,
        DiagnosticReport::new(effect, Diagnostic::runtime(issue)),
    )
}

#[derive(Debug)]
pub(super) enum GenericLuaExecutionError {
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    Run(ProjectLuaRunError),
}

#[derive(Debug)]
pub(super) struct GenericLuaPreflightError(pub(super) ProjectLuaFailure);

impl fmt::Display for GenericLuaPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for GenericLuaPreflightError {}

impl GenericLuaExecutionError {
    pub(super) fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Run(
                ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
                    | ProjectLuaRunError::Failed(ProjectLuaFailure::Cancelled)
                    | ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
            )
        )
    }

    pub(super) fn diagnostic_report(&self, database_path: &Path) -> DiagnosticReport {
        match self {
            Self::Open { path, source } => DiagnosticReport::new(
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
            Self::Run(source) => source.diagnostic_report(database_path),
        }
    }
}

impl fmt::Display for GenericLuaExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "打开项目数据库 {} 失败：{source}",
                    path.display()
                )
            }
            Self::Run(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericLuaExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Run(source) => Some(source),
        }
    }
}

pub(super) fn generic_cpu_execution_failure(
    source: CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> GenericCommandError {
    match source {
        CpuTaskExecutionError::Cancelled => GenericCommandError::Cancelled,
        source @ (CpuTaskExecutionError::Unavailable(_) | CpuTaskExecutionError::TaskPanicked) => {
            let report = DiagnosticReport::new(StateEffect::ProgressPreserved, source.diagnostic());
            GenericCommandError::reported(source, report)
        }
    }
}

pub(super) fn generic_project_lease_failure(
    source: ProjectCommandLeaseError<Box<SystemFileSystemError>>,
) -> GenericCommandError {
    match &source {
        ProjectCommandLeaseError::Unavailable {
            source: operation, ..
        } if system_file_system_error_is_cancelled(operation.as_ref()) => {
            GenericCommandError::Cancelled
        }
        ProjectCommandLeaseError::Unavailable { .. } => {
            let report = source.diagnostic_report_at(FileSystemDiagnosticStage::Project);
            GenericCommandError::reported(source, report)
        }
    }
}

pub(super) fn generic_manual_failure(source: ManualCommandError) -> GenericCommandError {
    if source.is_cancelled() {
        return GenericCommandError::Cancelled;
    }
    let report = source.diagnostic_report();
    GenericCommandError::reported(source, report)
}

fn system_file_system_error_is_cancelled(source: &SystemFileSystemError) -> bool {
    matches!(
        source,
        SystemFileSystemError::Cancelled { .. }
            | SystemFileSystemError::Windows(WindowsFsError::LockCancelled { .. })
    )
}

fn read_file_error_is_cancelled(source: &ReadFileError<SystemFileSystemError>) -> bool {
    matches!(
        source,
        ReadFileError::Io { source, .. } if system_file_system_error_is_cancelled(source)
    )
}

pub(super) fn generic_read_file_failure(
    source: ReadFileError<SystemFileSystemError>,
    stage: FileSystemDiagnosticStage,
) -> GenericCommandError {
    if read_file_error_is_cancelled(&source) {
        GenericCommandError::Cancelled
    } else {
        let report = generic_read_file_report(&source, stage);
        GenericCommandError::reported(source, report)
    }
}

pub(super) fn generic_prompt_resource_failure(
    source: PromptResourceLoadError,
) -> GenericCommandError {
    if matches!(
        &source,
        PromptResourceLoadError::Read(source) if read_file_error_is_cancelled(source)
    ) {
        GenericCommandError::Cancelled
    } else {
        let report = source.diagnostic_report();
        GenericCommandError::reported(source, report)
    }
}

pub(super) fn generic_translation_resource_failure(
    source: TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>,
) -> GenericCommandError {
    let cancelled = match &source {
        TranslationPlanningResourceReadingError::Cancelled => true,
        TranslationPlanningResourceReadingError::ReadTerminology { source, .. }
        | TranslationPlanningResourceReadingError::ReadPlaceholderRules { source, .. } => {
            read_file_error_is_cancelled(source)
        }
        TranslationPlanningResourceReadingError::ParseTerminologyCompute { source, .. }
        | TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
            source, ..
        } => matches!(source, CpuTaskExecutionError::Cancelled),
        TranslationPlanningResourceReadingError::InvalidTerminology { .. }
        | TranslationPlanningResourceReadingError::InvalidPlaceholderRules { .. } => false,
    };
    if cancelled {
        GenericCommandError::Cancelled
    } else {
        let report = generic_translation_resource_report(&source);
        GenericCommandError::reported(source, report)
    }
}

fn generic_translation_resource_report(
    source: &TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>,
) -> DiagnosticReport {
    source.diagnostic_report()
}

pub(super) fn generic_preparation_failure(source: GenericPreparationError) -> GenericCommandError {
    generic_preparation_failure_at(GenericDiagnosticStage::Translate, source)
}

pub(super) fn generic_write_back_preparation_failure(
    source: GenericPreparationError,
) -> GenericCommandError {
    generic_preparation_failure_at(GenericDiagnosticStage::WriteBack, source)
}

#[derive(Debug)]
pub(super) struct GenericLayoutRulesFailure {
    pub(super) path: Option<PathBuf>,
    pub(super) project_snapshot: bool,
    pub(super) source: LayoutRulesError,
}

impl fmt::Display for GenericLayoutRulesFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(
                formatter,
                "排版规则无效 {}：{}",
                path.display(),
                self.source
            ),
            None => write!(formatter, "项目保存的排版规则无效：{}", self.source),
        }
    }
}

impl Error for GenericLayoutRulesFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

pub(super) fn generic_layout_rules_failure(
    source: LayoutRulesError,
    path: Option<PathBuf>,
    project_snapshot: bool,
) -> GenericCommandError {
    let failure = GenericLayoutRulesFailure {
        path,
        project_snapshot,
        source,
    };
    let report = DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::WriteBack,
            GenericProblem::WriteBackLayoutRules {
                path: failure.path.as_ref().map(SafePath::new),
                rule_number: failure.source.rule_number(),
                project_snapshot: failure.project_snapshot,
            },
        )),
    );
    GenericCommandError::reported(failure, report)
}

fn generic_preparation_failure_at(
    stage: GenericDiagnosticStage,
    source: GenericPreparationError,
) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else {
        let report = generic_preparation_report_at(&source, stage);
        GenericCommandError::reported(source, report)
    }
}

fn generic_preparation_report_at(
    source: &GenericPreparationError,
    stage: GenericDiagnosticStage,
) -> DiagnosticReport {
    if let Some(report) = generic_placeholder_protection_report(source, stage) {
        return report;
    }
    let preparation = |unit, problem| {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::TranslationPreparation { unit, problem },
            )),
        )
    };
    match source {
        GenericPreparationError::Cancelled => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericPreparationError::Placeholder {
            rule_source,
            source,
        } => match source {
            crate::generic::GenericPlaceholderError::InvalidResourceSnapshot(source) => {
                preparation(
                    None,
                    GenericTranslationPreparationProblem::InvalidPlaceholderSnapshot {
                        category: GenericJsonErrorCategory::from(
                            crate::json_diagnostic::JsonErrorCategory::from(source),
                        ),
                        line: source.line(),
                        column: source.column(),
                    },
                )
            }
            crate::generic::GenericPlaceholderError::Compilation(source) => {
                let origin = match rule_source {
                    GenericPlaceholderRuleSource::ExternalFile(path) => {
                        TranslationPlanningResourceOrigin::external(path)
                    }
                    GenericPlaceholderRuleSource::ProjectSnapshot => {
                        TranslationPlanningResourceOrigin::ProjectSnapshot
                    }
                };
                DiagnosticReport::new(
                    StateEffect::Unchanged,
                    Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                        origin,
                        problem: source.diagnostic_problem(),
                    }),
                )
            }
            crate::generic::GenericPlaceholderError::Protection(_) => preparation(
                None,
                GenericTranslationPreparationProblem::UnexpectedUnlocatedPlaceholderProtection,
            ),
            crate::generic::GenericPlaceholderError::Restore(source) => match source {
                PlaceholderRestoreError::Projection(source) => preparation(
                    None,
                    GenericTranslationPreparationProblem::PlaceholderRestoreProjection {
                        problem: generic_language_projection_problem(source),
                    },
                ),
                PlaceholderRestoreError::Multiset(source) => preparation(
                    None,
                    GenericTranslationPreparationProblem::PlaceholderRestoreMultiset {
                        problem: generic_placeholder_multiset_problem(source),
                    },
                ),
            },
            crate::generic::GenericPlaceholderError::ManualTranslationMismatch => preparation(
                None,
                GenericTranslationPreparationProblem::ManualTranslationPlaceholderMismatch,
            ),
        },
        GenericPreparationError::PlaceholderProtection { .. } => {
            unreachable!("带定位的 Placeholder 失败已在函数入口处理")
        }
        GenericPreparationError::LanguageProjection { locator, source }
            if stage == GenericDiagnosticStage::WriteBack =>
        {
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: diagnostic_generic_unit_locator(locator),
                        problem: GenericWriteBackUnitProblem::LanguageProjection {
                            side: GenericWriteBackTextSide::Source,
                            problem: generic_language_projection_problem(source),
                        },
                    },
                )),
            )
        }
        GenericPreparationError::LanguageProjection { locator, source } => preparation(
            Some(diagnostic_generic_unit_locator(locator)),
            GenericTranslationPreparationProblem::LanguageProjection {
                problem: generic_language_projection_problem(source),
            },
        ),
        GenericPreparationError::Planning(source) => generic_planning_report(source),
    }
}

fn diagnostic_generic_unit_locator(locator: &GenericUnitLocator) -> DiagnosticGenericUnitLocator {
    let (line, unit) = locator.natural_position();
    DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    )
    .with_natural_position(line, unit)
}

pub(super) fn generic_planning_report(source: &GenericPlanningError) -> DiagnosticReport {
    match source {
        GenericPlanningError::Cancelled => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::TaskPlanning {
                problem: TranslationTaskPlanningProblem::Cancelled,
            }),
        ),
        GenericPlanningError::TaskPlanning(source) => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::TaskPlanning {
                problem: generic_task_planning_problem(source),
            }),
        ),
        GenericPlanningError::MissingCurrentContext(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::MissingCurrentContext {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Missing(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::MissingPlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Unknown(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::UnknownPlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
        GenericPlanningError::Duplicate(locator) => generic_planning_fact_report(
            locator,
            GenericTranslationPreparationProblem::DuplicatePlanningFact {
                group_id: SafeIdentifier::from_validated(locator.group_id()),
                unit_id: SafeIdentifier::from_validated(locator.unit_id()),
            },
        ),
    }
}

fn generic_planning_fact_report(
    locator: &GenericPlanningUnitLocator,
    problem: GenericTranslationPreparationProblem,
) -> DiagnosticReport {
    let unit = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    );
    let unit = match locator.natural_position() {
        Some((line, unit_ordinal)) => unit.with_natural_position(line, unit_ordinal),
        None => unit,
    };
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TranslationPreparation {
                unit: Some(unit),
                problem,
            },
        )),
    )
}

const fn generic_task_planning_problem(
    source: &TaskPlanningError,
) -> TranslationTaskPlanningProblem {
    match source {
        TaskPlanningError::Cancelled => TranslationTaskPlanningProblem::Cancelled,
        TaskPlanningError::EmptyScope => TranslationTaskPlanningProblem::EmptyScope,
        TaskPlanningError::EmptyGroup => TranslationTaskPlanningProblem::EmptyGroup,
        TaskPlanningError::UnitCountOverflow => TranslationTaskPlanningProblem::UnitCountOverflow,
        TaskPlanningError::CharacterCountOverflow => {
            TranslationTaskPlanningProblem::CharacterCountOverflow
        }
        TaskPlanningError::ResponsibilityCountMismatch { expected, actual } => {
            TranslationTaskPlanningProblem::ResponsibilityCountMismatch {
                expected: *expected,
                actual: *actual,
            }
        }
        TaskPlanningError::TaskIdOverflow => TranslationTaskPlanningProblem::TaskIdOverflow,
    }
}

pub(super) fn generic_placeholder_protection_report(
    source: &GenericPreparationError,
    stage: GenericDiagnosticStage,
) -> Option<DiagnosticReport> {
    let GenericPreparationError::PlaceholderProtection {
        rule_source,
        locator,
        source,
    } = source
    else {
        return None;
    };
    let rule_source = match rule_source {
        GenericPlaceholderRuleSource::ExternalFile(path) => {
            DiagnosticPlaceholderRuleSource::external_file(path)
        }
        GenericPlaceholderRuleSource::ProjectSnapshot => {
            DiagnosticPlaceholderRuleSource::ProjectSnapshot
        }
    };
    let (line, unit) = locator.natural_position();
    let unit = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    )
    .with_natural_position(line, unit);
    let problem = placeholder_protection_issue(source);
    if stage == GenericDiagnosticStage::WriteBack {
        return Some(DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackUnit {
                    unit,
                    problem: GenericWriteBackUnitProblem::PlaceholderProtection {
                        side: GenericWriteBackTextSide::Source,
                        problem,
                    },
                },
            )),
        ));
    }
    Some(DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::translation(TranslationIssue::Placeholder {
            rule_source,
            unit,
            problem,
        }),
    ))
}

fn placeholder_protection_issue(source: &PlaceholderProtectionError) -> PlaceholderIssue {
    match source {
        PlaceholderProtectionError::StartWorker { operation, source } => {
            PlaceholderIssue::WorkerStart {
                operation: diagnostic_placeholder_worker_operation(*operation),
                io_kind: SafeIoKind::from(source.kind()),
                raw_os_code: source.raw_os_error(),
            }
        }
        PlaceholderProtectionError::Match { rule, source } => PlaceholderIssue::PatternMatch {
            rule_origin: Some(diagnostic_placeholder_rule_origin(rule.origin())),
            rule_number: rule.rule_number(),
            pcre2: Pcre2Failure {
                kind: diagnostic_pcre2_failure_kind(source.kind()),
                code: source.code(),
                offset: source.offset(),
            },
        },
        PlaceholderProtectionError::EmptyMatch { matched } => PlaceholderIssue::EmptyMatch {
            rule_origin: diagnostic_placeholder_rule_origin(matched.rule().origin()),
            rule_number: matched.rule().rule_number(),
            match_range: known_placeholder_range(matched.start_byte(), matched.end_byte()),
        },
        PlaceholderProtectionError::MissingTextCapture {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
        } => PlaceholderIssue::MissingTextCapture {
            rule_number: *rule_number,
            match_range: known_placeholder_range(*whole_match_start_byte, *whole_match_end_byte),
        },
        PlaceholderProtectionError::InvalidMatchRange {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
            capture_start_byte,
            capture_end_byte,
            violation,
        } => PlaceholderIssue::InvalidMatchRange {
            rule_number: *rule_number,
            whole_match_start_byte: *whole_match_start_byte,
            whole_match_end_byte: *whole_match_end_byte,
            capture_start_byte: *capture_start_byte,
            capture_end_byte: *capture_end_byte,
            violation: diagnostic_match_range_violation(*violation),
        },
        PlaceholderProtectionError::OverlappingMatches { first, second } => {
            PlaceholderIssue::OverlappingMatches {
                first_origin: diagnostic_placeholder_rule_origin(first.rule().origin()),
                first_rule_number: first.rule().rule_number(),
                first_range: known_placeholder_range(first.start_byte(), first.end_byte()),
                second_origin: diagnostic_placeholder_rule_origin(second.rule().origin()),
                second_rule_number: second.rule().rule_number(),
                second_range: known_placeholder_range(second.start_byte(), second.end_byte()),
            }
        }
        PlaceholderProtectionError::CrossesLineBoundary {
            matched,
            source_line_index,
        } => PlaceholderIssue::CrossesLineBoundary {
            rule_origin: diagnostic_placeholder_rule_origin(matched.rule().origin()),
            rule_number: matched.rule().rule_number(),
            source_line_index: *source_line_index,
        },
        PlaceholderProtectionError::ReservedTokenNamespace {
            start_byte,
            end_byte,
        } => PlaceholderIssue::ReservedTokenNamespace {
            range: known_placeholder_range(*start_byte, *end_byte),
        },
    }
}

fn known_placeholder_range(start: usize, end: usize) -> ByteRange {
    ByteRange::new(start, end).expect("Placeholder 叶子错误必须保持已确认的正向匹配范围")
}

const fn diagnostic_placeholder_rule_origin(
    origin: PlaceholderRuleOrigin,
) -> DiagnosticPlaceholderRuleOrigin {
    match origin {
        PlaceholderRuleOrigin::BuiltIn => DiagnosticPlaceholderRuleOrigin::Builtin,
        PlaceholderRuleOrigin::Custom => DiagnosticPlaceholderRuleOrigin::Custom,
    }
}

const fn diagnostic_placeholder_worker_operation(
    operation: PlaceholderWorkerOperation,
) -> DiagnosticPlaceholderWorkerOperation {
    match operation {
        PlaceholderWorkerOperation::CompileCustomRules => {
            DiagnosticPlaceholderWorkerOperation::CompileCustomRules
        }
        PlaceholderWorkerOperation::MatchText => DiagnosticPlaceholderWorkerOperation::MatchText,
    }
}

const fn diagnostic_pcre2_failure_kind(kind: PlaceholderPcre2ErrorKind) -> Pcre2FailureKind {
    match kind {
        PlaceholderPcre2ErrorKind::Compile => Pcre2FailureKind::Compile,
        PlaceholderPcre2ErrorKind::Jit => Pcre2FailureKind::Jit,
        PlaceholderPcre2ErrorKind::Match => Pcre2FailureKind::Match,
        PlaceholderPcre2ErrorKind::Info => Pcre2FailureKind::Info,
        PlaceholderPcre2ErrorKind::Option => Pcre2FailureKind::Option,
        PlaceholderPcre2ErrorKind::Unrecognized => Pcre2FailureKind::Unrecognized,
    }
}

const fn diagnostic_match_range_violation(
    violation: PlaceholderMatchRangeViolation,
) -> DiagnosticMatchRangeViolation {
    match violation {
        PlaceholderMatchRangeViolation::WholeStartAfterEnd => {
            DiagnosticMatchRangeViolation::WholeStartAfterEnd
        }
        PlaceholderMatchRangeViolation::WholeEndBeyondText => {
            DiagnosticMatchRangeViolation::WholeEndBeyondText
        }
        PlaceholderMatchRangeViolation::WholeStartNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::WholeStartNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::WholeEndNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::WholeEndNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureStartAfterEnd => {
            DiagnosticMatchRangeViolation::CaptureStartAfterEnd
        }
        PlaceholderMatchRangeViolation::CaptureEndBeyondText => {
            DiagnosticMatchRangeViolation::CaptureEndBeyondText
        }
        PlaceholderMatchRangeViolation::CaptureStartNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::CaptureStartNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureEndNotUtf8Boundary => {
            DiagnosticMatchRangeViolation::CaptureEndNotUtf8Boundary
        }
        PlaceholderMatchRangeViolation::CaptureStartsBeforeWhole => {
            DiagnosticMatchRangeViolation::CaptureStartsBeforeWhole
        }
        PlaceholderMatchRangeViolation::CaptureEndsAfterWhole => {
            DiagnosticMatchRangeViolation::CaptureEndsAfterWhole
        }
    }
}

pub(super) fn generic_write_back_candidate_failure(
    source: GenericWriteBackError,
) -> GenericCommandError {
    if source.is_cancelled() {
        GenericCommandError::Cancelled
    } else {
        let report = source.diagnostic_report(StateEffect::Unchanged);
        GenericCommandError::reported(source, report)
    }
}

pub(super) fn generic_task_response_diagnostic(
    task_index: usize,
    total_tasks: usize,
    problem: GenericTaskResponseProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskResponse {
                task_ordinal: generic_task_ordinal(task_index),
                total_tasks: generic_count(total_tasks),
                problem,
            },
        )),
    )
}

pub(super) fn generic_response_problem_diagnostic(
    task_index: usize,
    total_tasks: usize,
    problem: &ResponseProblem,
) -> DiagnosticReport {
    generic_task_response_diagnostic(task_index, total_tasks, problem.clone())
}

fn generic_response_review_diagnostic(
    task_index: usize,
    total_tasks: usize,
    review: &TranslationReview,
) -> DiagnosticReport {
    let locator = review.locator();
    let destination = DiagnosticGenericUnitLocator::new(
        locator.relative_path(),
        locator.group_id(),
        locator.unit_id(),
        Some(locator.role()),
    );
    let destination = match locator.natural_position() {
        Some((line, unit)) => destination.with_natural_position(line, unit),
        None => destination,
    };
    let finding = match review.finding() {
        ReviewFinding::SourceResidual => GenericResponseReviewFinding::SourceResidual,
        ReviewFinding::NonStopFinish => GenericResponseReviewFinding::NonStopFinish,
    };
    generic_task_response_diagnostic(
        task_index,
        total_tasks,
        GenericTaskResponseProblem::DestinationReview {
            output_id: u64::try_from(review.output_id().get())
                .expect("当前平台 usize 必须能够无损表示为 u64"),
            destination,
            finding,
        },
    )
}

fn generic_review_effect(
    commit: Option<&CommitTranslationResultsOutcome>,
    destination: Option<&GenericPlanningUnitLocator>,
) -> StateEffect {
    let Some(commit) = commit else {
        return StateEffect::ProgressPreserved;
    };
    let applied = destination.map_or(commit.committed > 0, |destination| {
        commit.committed > 0
            && !commit.conflicts.iter().any(|(group_id, unit_id)| {
                group_id == destination.group_id() && unit_id == destination.unit_id()
            })
    });
    if applied {
        StateEffect::Applied
    } else {
        StateEffect::ProgressPreserved
    }
}

pub(super) fn generic_accepted_task_diagnostics(
    task_index: usize,
    total_tasks: usize,
    finish_review: bool,
    mut response_problem_diagnostics: Vec<DiagnosticReport>,
    reviews: Vec<TranslationReview>,
    commit: Option<&CommitTranslationResultsOutcome>,
) -> Vec<DiagnosticReport> {
    let mut diagnostics = Vec::with_capacity(
        response_problem_diagnostics.len() + reviews.len() + usize::from(finish_review),
    );
    if finish_review {
        diagnostics.push(
            generic_task_response_diagnostic(
                task_index,
                total_tasks,
                GenericTaskResponseProblem::ResponseReview {
                    finding: GenericResponseReviewFinding::NonStopFinish,
                },
            )
            .with_effect(generic_review_effect(commit, None)),
        );
    }
    diagnostics.append(&mut response_problem_diagnostics);
    for review in reviews {
        let effect = generic_review_effect(commit, Some(review.locator()));
        diagnostics.push(
            generic_response_review_diagnostic(task_index, total_tasks, &review)
                .with_effect(effect),
        );
    }
    diagnostics
}

pub(super) fn generic_response_parse_diagnostic(
    task_index: usize,
    total_tasks: usize,
    error: TranslationTaskResponseParseError,
) -> DiagnosticReport {
    let problem = match error.kind() {
        TranslationTaskResponseParseErrorKind::Json(category) => {
            GenericTaskResponseProblem::InvalidJson {
                category: match category {
                    TranslationTaskResponseJsonErrorCategory::Io => {
                        GenericTaskResponseJsonCategory::Io
                    }
                    TranslationTaskResponseJsonErrorCategory::Syntax => {
                        GenericTaskResponseJsonCategory::Syntax
                    }
                    TranslationTaskResponseJsonErrorCategory::Shape => {
                        GenericTaskResponseJsonCategory::Shape
                    }
                    TranslationTaskResponseJsonErrorCategory::UnexpectedEof => {
                        GenericTaskResponseJsonCategory::UnexpectedEof
                    }
                },
                line: error.line(),
                column: error.column(),
            }
        }
        TranslationTaskResponseParseErrorKind::ThinkingEmpty => {
            GenericTaskResponseProblem::ThinkingEmpty {
                line: error.line(),
                column: error.column(),
            }
        }
    };
    generic_task_response_diagnostic(task_index, total_tasks, problem)
}

fn generic_unavailable_task_diagnostic(
    task_index: usize,
    total_tasks: usize,
    reason: GenericTaskUnavailableReason,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::Translate,
            GenericProblem::TaskUnavailable {
                task_ordinal: generic_task_ordinal(task_index),
                total_tasks: generic_count(total_tasks),
                reason,
            },
        )),
    )
}

pub(super) fn generic_task_execution_error_report(
    error: &GenericCommandError,
    task_index: usize,
    total_tasks: usize,
) -> DiagnosticReport {
    let report = generic_command_error_report(error);
    if matches!(report.primary().code(), "generic.translation.failed") {
        generic_unavailable_task_diagnostic(
            task_index,
            total_tasks,
            GenericTaskUnavailableReason::RequestFailed,
        )
    } else {
        report.with_effect(StateEffect::ProgressPreserved)
    }
}

pub(super) fn generic_file_system_build_failure(
    source: SystemFileSystemBuildError,
) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
    GenericCommandError::reported(source, report)
}

pub(super) fn generic_cpu_start_failure(source: CpuExecutorStartError) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic());
    GenericCommandError::reported(source, report)
}

pub(super) fn generic_scratch_command_error(source: GenericScratchError) -> GenericCommandError {
    if let GenericScratchError::CleanupAfterFailure { operation, cleanup } = source {
        let operation = if matches!(operation.as_ref(), GenericScratchError::Cancelled) {
            GenericCommandError::Cancelled
        } else {
            let report = generic_scratch_report(
                operation.as_ref(),
                FileSystemDiagnosticStage::WriteBack,
                StateEffect::Unchanged,
            );
            GenericCommandError::reported(*operation, report)
        };
        let discard = generic_scratch_discard_failure(*cleanup);
        return GenericCommandError::PublishDiscard {
            operation: Box::new(operation),
            discard,
        };
    }
    if matches!(source, GenericScratchError::Cancelled) {
        GenericCommandError::Cancelled
    } else {
        let report = generic_scratch_report(
            &source,
            FileSystemDiagnosticStage::WriteBack,
            StateEffect::Unchanged,
        );
        GenericCommandError::reported(source, report)
    }
}

pub(super) fn generic_scratch_discard_failure(
    source: GenericScratchError,
) -> GenericDiscardFailure {
    let report = generic_scratch_report(
        &source,
        FileSystemDiagnosticStage::Publication,
        StateEffect::RecoveryRequired,
    );
    GenericDiscardFailure::new(report, source)
}

pub(super) fn generic_scratch_report(
    source: &GenericScratchError,
    stage: FileSystemDiagnosticStage,
    effect: StateEffect,
) -> DiagnosticReport {
    let file_system = |operation, problem| {
        DiagnosticReport::new(
            effect,
            Diagnostic::file_system(FileSystemIssue::new(
                FileSystemDiagnosticContext::new(stage, operation),
                problem,
            )),
        )
    };
    match source {
        GenericScratchError::Io {
            operation,
            path,
            source,
        } => file_system(
            *operation,
            FileSystemProblem::Io {
                path: SafePath::new(path),
                failure: IoFailure::from_error(source),
            },
        ),
        GenericScratchError::UnsafeCleanupTarget {
            workspace_root,
            scratch_root,
        } => file_system(
            FileSystemOperation::Remove,
            FileSystemProblem::OutsideScope {
                root: SafePath::new(workspace_root),
                path: SafePath::new(scratch_root),
            },
        ),
        GenericScratchError::CleanupAfterFailure { cleanup, .. } => {
            generic_scratch_report(cleanup, stage, effect)
        }
        GenericScratchError::Cancelled => DiagnosticReport::new(
            effect,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::FileSystemExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        ),
        GenericScratchError::InvalidRelativePath(path) => file_system(
            FileSystemOperation::ResolveDirectory,
            FileSystemProblem::InvalidPath {
                path: SafePath::new(path),
                violation: FileSystemPathViolation::OutsideScope,
            },
        ),
        GenericScratchError::TargetNotDirectory(path) => file_system(
            FileSystemOperation::Metadata,
            FileSystemProblem::NotDirectory {
                path: SafePath::new(path),
            },
        ),
        GenericScratchError::InvalidMaterializedFile { source, .. } => {
            source.diagnostic_report(effect)
        }
    }
}
