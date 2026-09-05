//! 目录准备、发布、丢弃和恢复的终态与诊断契约。

use super::candidate::{DirectoryStageRequest, StagedDirectory};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, PublicationBackendCause, PublicationIssue, PublicationProblem,
    PublicationStep, RelatedFailureRelation, SafePath, StateEffect, public_path,
};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

/// 根实现无法删除的暂存或恢复产物。
#[derive(Debug)]
pub(crate) struct StagingCleanupFailure<E> {
    residual_path: PathBuf,
    source: E,
}

/// 底层文件系统根把自己的封闭叶子报告提供给目录发布语义所有者。
pub(crate) trait DirectoryPublicationDiagnosticSource {
    fn publication_diagnostic(&self, step: PublicationStep) -> DirectoryPublicationDiagnostic;
}

pub(crate) struct DirectoryPublicationDiagnostic {
    effect: StateEffect,
    primary: PublicationBackendCause,
    related: Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
}

impl DirectoryPublicationDiagnostic {
    pub(crate) fn new(primary: PublicationBackendCause) -> Self {
        Self {
            effect: StateEffect::Unchanged,
            primary,
            related: Vec::new(),
        }
    }

    /// 根实现保留已知状态影响，发布层不能在包装原因时将其降级。
    pub(crate) fn with_effect(mut self, effect: StateEffect) -> Self {
        self.effect = effect;
        self
    }

    pub(crate) fn with_related(
        mut self,
        relation: RelatedFailureRelation,
        related: DirectoryPublicationDiagnostic,
    ) -> Self {
        self.related.push((relation, related));
        self
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        StateEffect,
        PublicationBackendCause,
        Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
    ) {
        (self.effect, self.primary, self.related)
    }
}

fn publication_report(
    effect: StateEffect,
    step: PublicationStep,
    problem: PublicationProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::publication(PublicationIssue::new(step, problem)),
    )
}

fn cleanup_report<E>(failure: &StagingCleanupFailure<E>) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let projection = failure
        .source()
        .publication_diagnostic(PublicationStep::CleanupResidual);
    let (source_effect, cause, related) = projection.into_parts();
    attach_backend_related(
        publication_report(
            StateEffect::RecoveryRequired.strongest(source_effect),
            PublicationStep::CleanupResidual,
            PublicationProblem::CleanupFailed {
                residual_path: SafePath::new(failure.residual_path()),
                cause,
            },
        ),
        related,
    )
}

fn attach_backend_related(
    mut report: DiagnosticReport,
    related: Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
) -> DiagnosticReport {
    for (relation, projection) in related {
        let (effect, cause, nested) = projection.into_parts();
        let related_report = attach_backend_related(
            DiagnosticReport::new(effect, cause.into_diagnostic()),
            nested,
        );
        report = report.with_related(relation, related_report);
    }
    report
}

impl<E> StagingCleanupFailure<E> {
    pub(crate) fn new(residual_path: PathBuf, source: E) -> Self {
        Self {
            residual_path,
            source,
        }
    }

    pub(crate) fn residual_path(&self) -> &Path {
        &self.residual_path
    }

    pub(crate) fn source(&self) -> &E {
        &self.source
    }
}

impl<E> fmt::Display for StagingCleanupFailure<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法清理目录 {}：{}",
            self.residual_path.display(),
            self.source
        )
    }
}

impl<E> Error for StagingCleanupFailure<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 准备目录候选时的已知未发布终态。
#[derive(Debug)]
pub(crate) enum DirectoryPrepareError<E> {
    NotPrepared {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
}

impl<E> DirectoryPrepareError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::NotPrepared {
                target_root,
                source,
                cleanup_failure,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::PrepareCandidate);
                let (source_effect, cause, related) = projection.into_parts();
                let mut report = attach_backend_related(
                    publication_report(
                        StateEffect::Unchanged.strongest(source_effect),
                        PublicationStep::PrepareCandidate,
                        PublicationProblem::PrepareFailed {
                            output_root: SafePath::new(target_root),
                            candidate_root: cleanup_failure
                                .as_ref()
                                .map(|failure| SafePath::new(failure.residual_path())),
                            cause,
                        },
                    ),
                    related,
                );
                if let Some(cleanup_failure) = cleanup_failure {
                    report = report.with_related(
                        RelatedFailureRelation::Cleanup,
                        cleanup_report(cleanup_failure),
                    );
                }
                report
            }
        }
    }
}

/// 显式恢复是否实际处理了属于该目标的受管发布产物。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryRecoveryOutcome {
    Unchanged,
    Recovered,
}

/// 在调用方观察目标状态之前，恢复该目标的受管发布产物失败。
#[derive(Debug)]
pub(crate) struct DirectoryRecoveryError<E> {
    target_root: PathBuf,
    source: E,
}

impl<E> DirectoryRecoveryError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let projection = self.source.publication_diagnostic(PublicationStep::Recover);
        let (source_effect, cause, related) = projection.into_parts();
        attach_backend_related(
            publication_report(
                StateEffect::RecoveryRequired.strongest(source_effect),
                PublicationStep::Recover,
                PublicationProblem::RecoveryFailed {
                    output_root: SafePath::new(&self.target_root),
                    cause,
                },
            ),
            related,
        )
    }
}

impl<E> DirectoryRecoveryError<E> {
    pub(crate) fn new(target_root: PathBuf, source: E) -> Self {
        Self {
            target_root,
            source,
        }
    }

    #[cfg(test)]
    pub(crate) fn source_error(&self) -> &E {
        &self.source
    }
}

impl<E> fmt::Display for DirectoryRecoveryError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法恢复目录发布目标 {}：{}",
            self.target_root.display(),
            self.source
        )
    }
}

impl<E> Error for DirectoryRecoveryError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl<E> fmt::Display for DirectoryPrepareError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPrepared {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选未准备（目标：{}）：{source}",
                    target_root.display()
                )?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
        }
    }
}

impl<E> Error for DirectoryPrepareError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotPrepared { source, .. } => Some(source),
        }
    }
}

/// 根实现终结一次目录发布时的可观测终态。
#[derive(Debug)]
pub(crate) enum DirectoryPublishError<E> {
    TargetAlreadyExists {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    TargetMissing {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    TargetNotDirectory {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 根在接管交换副作用前拒绝本次发布。
    NotAttempted {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选没有成为目标，调用方可继续信任原目标。
    NotPublished {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选已经成为目标，但旧备份或其他恢复产物未能清理。
    PublishedWithResiduals {
        target_root: PathBuf,
        residual_path: PathBuf,
        source: E,
    },
    /// 目标暂时缺失，但旧目录与候选身份仍然确定，后续同目标操作可以恢复。
    RecoveryRequired {
        target_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
        source: E,
    },
    /// 交换与恢复均发生故障，目标当前内容无法确定。
    OutcomeUnknown {
        target_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
        source: E,
    },
}

impl<E> DirectoryPublishError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetAlreadyExists {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::TargetMissing {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetMissing {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::TargetNotDirectory {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetNotDirectory {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::NotAttempted {
                target_root,
                source,
                cleanup_failure,
            } => publication_failure_report(
                source,
                cleanup_failure.as_ref(),
                StateEffect::Unchanged,
                PublicationStep::Publish,
                |cause| PublicationProblem::NotAttempted {
                    output_root: SafePath::new(target_root),
                    cause,
                },
            ),
            Self::NotPublished {
                target_root,
                source,
                cleanup_failure,
            } => publication_failure_report(
                source,
                cleanup_failure.as_ref(),
                StateEffect::Unchanged,
                PublicationStep::Publish,
                |cause| PublicationProblem::NotPublished {
                    output_root: SafePath::new(target_root),
                    cause,
                },
            ),
            Self::PublishedWithResiduals {
                target_root,
                residual_path,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::CleanupResidual);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::AppliedFinalizationFailed.strongest(source_effect),
                        PublicationStep::Finalize,
                        PublicationProblem::PublishedFinalizationFailed {
                            output_root: SafePath::new(target_root),
                            residual_path: SafePath::new(residual_path),
                            cause,
                        },
                    ),
                    related,
                )
            }
            Self::RecoveryRequired {
                target_root,
                recovery_artifacts,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::Recover);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::RecoveryRequired.strongest(source_effect),
                        PublicationStep::Publish,
                        PublicationProblem::RecoveryRequired {
                            output_root: SafePath::new(target_root),
                            recovery_artifacts: recovery_artifacts
                                .iter()
                                .map(SafePath::new)
                                .collect(),
                            cause,
                        },
                    ),
                    related,
                )
            }
            Self::OutcomeUnknown {
                target_root,
                recovery_artifacts,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::Recover);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::OutcomeUnknown.strongest(source_effect),
                        PublicationStep::Publish,
                        PublicationProblem::OutcomeUnknown {
                            output_root: SafePath::new(target_root),
                            recovery_artifacts: recovery_artifacts
                                .iter()
                                .map(SafePath::new)
                                .collect(),
                            cause,
                        },
                    ),
                    related,
                )
            }
        }
    }
}

fn publication_state_report<E>(
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
    problem: PublicationProblem,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let mut report = publication_report(StateEffect::Unchanged, PublicationStep::Publish, problem);
    if let Some(cleanup_failure) = cleanup_failure {
        report = report.with_related(
            RelatedFailureRelation::Cleanup,
            cleanup_report(cleanup_failure),
        );
    }
    report
}

fn publication_failure_report<E>(
    source: &E,
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
    effect: StateEffect,
    step: PublicationStep,
    problem: impl FnOnce(PublicationBackendCause) -> PublicationProblem,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let projection = source.publication_diagnostic(step);
    let (source_effect, cause, related) = projection.into_parts();
    let mut report = attach_backend_related(
        publication_report(effect.strongest(source_effect), step, problem(cause)),
        related,
    );
    if let Some(cleanup_failure) = cleanup_failure {
        report = report.with_related(
            RelatedFailureRelation::Cleanup,
            cleanup_report(cleanup_failure),
        );
    }
    report
}

impl<E> fmt::Display for DirectoryPublishError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标已存在：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::TargetMissing {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标不存在：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::TargetNotDirectory {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标不是目录：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::NotAttempted {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选尚未开始发布（目标：{}）：{source}",
                    target_root.display()
                )?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::NotPublished {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选未发布（目标：{}）：{source}",
                    target_root.display()
                )?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::PublishedWithResiduals {
                target_root,
                residual_path,
                source,
            } => write!(
                formatter,
                "目录候选已发布到 {}，但无法清理恢复产物 {}：{source}",
                target_root.display(),
                residual_path.display()
            ),
            Self::RecoveryRequired {
                target_root,
                recovery_artifacts,
                source,
            } => write!(
                formatter,
                "目录发布需要继续恢复（目标：{}，恢复产物：{}）：{source}",
                target_root.display(),
                display_paths(recovery_artifacts)
            ),
            Self::OutcomeUnknown {
                target_root,
                recovery_artifacts,
                source,
            } => write!(
                formatter,
                "目录发布结果未知（目标：{}，恢复产物：{}）：{source}",
                target_root.display(),
                display_paths(recovery_artifacts)
            ),
        }
    }
}

impl<E> Error for DirectoryPublishError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TargetAlreadyExists {
                cleanup_failure, ..
            }
            | Self::TargetMissing {
                cleanup_failure, ..
            }
            | Self::TargetNotDirectory {
                cleanup_failure, ..
            }
            | Self::NotAttempted {
                cleanup_failure, ..
            } => cleanup_failure
                .as_ref()
                .map(|failure| failure as &(dyn Error + 'static)),
            Self::NotPublished { source, .. }
            | Self::PublishedWithResiduals { source, .. }
            | Self::RecoveryRequired { source, .. }
            | Self::OutcomeUnknown { source, .. } => Some(source),
        }
    }
}

fn write_cleanup_failure<E>(
    formatter: &mut fmt::Formatter<'_>,
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
) -> fmt::Result
where
    E: fmt::Display,
{
    if let Some(cleanup_failure) = cleanup_failure {
        write!(formatter, "；{cleanup_failure}")?;
    }
    Ok(())
}

fn display_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "无已知路径".to_owned();
    }
    paths.iter().map(public_path).collect::<Vec<_>>().join("、")
}

/// 主动丢弃目录候选时的清理失败。
#[derive(Debug)]
pub(crate) struct DirectoryDiscardError<E> {
    staging_root: PathBuf,
    source: E,
}

impl<E> DirectoryDiscardError<E> {
    pub(crate) fn new(staging_root: PathBuf, source: E) -> Self {
        Self {
            staging_root,
            source,
        }
    }

    #[cfg(test)]
    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &E {
        &self.source
    }
}

impl<E> DirectoryDiscardError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let projection = self
            .source
            .publication_diagnostic(PublicationStep::DiscardCandidate);
        let (source_effect, cause, related) = projection.into_parts();
        attach_backend_related(
            publication_report(
                StateEffect::RecoveryRequired.strongest(source_effect),
                PublicationStep::DiscardCandidate,
                PublicationProblem::DiscardFailed {
                    candidate_root: SafePath::new(&self.staging_root),
                    cause,
                },
            ),
            related,
        )
    }
}

impl<E> fmt::Display for DirectoryDiscardError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法丢弃暂存目录 {}：{}",
            self.staging_root.display(),
            self.source
        )
    }
}

impl<E> Error for DirectoryDiscardError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 在目标同级准备并可恢复地发布完整目录的环境根能力。
///
/// `prepare` 必须拒绝符号链接与 reparse point，并且不改变最终目标。`publish` 是
/// 完整候选树的唯一全量验证入口；验证后必须对同一目标线性化，并将一次目录交换、
/// 恢复与清理收敛为一个明确终态。所有操作一旦开始产生副作用，调用方必须等待
/// future 完成。
pub(crate) trait RecoverableDirectoryPublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type StagingState: Send + 'static;

    /// 在调用方观察目标状态之前，恢复或清理当前目标命名空间中的受管发布产物。
    fn recover(
        &self,
        target_root: PathBuf,
    ) -> impl Future<Output = Result<DirectoryRecoveryOutcome, DirectoryRecoveryError<Self::Error>>> + Send;

    fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> impl Future<
        Output = Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>>,
    > + Send;

    fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> impl Future<Output = Result<(), DirectoryPublishError<Self::Error>>> + Send;

    fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> impl Future<Output = Result<(), DirectoryDiscardError<Self::Error>>> + Send;
}

#[cfg(test)]
mod tests;
