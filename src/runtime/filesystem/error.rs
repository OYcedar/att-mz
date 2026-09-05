//! 文件系统失败事实及面向调用方的诊断投影。

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage,
    FileSystemIssue, FileSystemJournalViolation, FileSystemOperation, FileSystemOrdinalKeyPhase,
    FileSystemPathViolation, FileSystemProblem, FileSystemRecoveryViolation, IoFailure,
    PublicationBackendCause, PublicationStep, RelatedFailureRelation, RuntimeComponent,
    RuntimeIssue, RuntimeOperation, SafePath, StateEffect, public_path,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryPublicationDiagnostic, DirectoryPublicationDiagnosticSource,
    DirectoryTreeFingerprintError, ListDirectoryError, ReadFileError, ResolveDirectoryError,
};
use crate::windows_path::WindowsOrdinalCaseKeyError;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{fmt, io};

pub(super) const OBSERVATION_CREATE_OPERATION: &str = "建立可观测性临时文件";

pub(super) const OBSERVATION_WRITE_OPERATION: &str = "写入可观测性临时文件";

pub(super) const OBSERVATION_FLUSH_OPERATION: &str = "flush 可观测性临时文件";

pub(super) const OBSERVATION_SYNC_OPERATION: &str = "sync 可观测性临时文件";

pub(super) const OBSERVATION_COMMIT_OPERATION: &str = "提交可观测性文件";

pub(super) const OBSERVATION_CLEANUP_OPERATION: &str = "清理可观测性临时文件";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalObservationOperation {
    Create,
    Write,
    Flush,
    Sync,
    Cleanup,
}

#[derive(Debug)]
pub(crate) enum SystemFileSystemBuildError {
    InvalidConfiguration(&'static str),
    AvailableParallelism(io::Error),
    WorkerSpawn(io::Error),
}

impl fmt::Display for SystemFileSystemBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => formatter.write_str(message),
            Self::AvailableParallelism(source) => {
                write!(formatter, "无法取得本机可用文件并行度：{source}")
            }
            Self::WorkerSpawn(source) => write!(formatter, "无法建立文件工作线程：{source}"),
        }
    }
}

impl Error for SystemFileSystemBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AvailableParallelism(source) | Self::WorkerSpawn(source) => Some(source),
            Self::InvalidConfiguration(_) => None,
        }
    }
}

impl SystemFileSystemBuildError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        Diagnostic::runtime(match self {
            Self::InvalidConfiguration(_) => RuntimeIssue::InvalidConfiguration {
                component: RuntimeComponent::FileSystemExecutor,
            },
            Self::AvailableParallelism(source) => RuntimeIssue::Io {
                component: RuntimeComponent::FileSystemExecutor,
                operation: RuntimeOperation::DetectAvailableParallelism,
                failure: IoFailure::from_error(source),
            },
            Self::WorkerSpawn(source) => RuntimeIssue::Io {
                component: RuntimeComponent::FileSystemExecutor,
                operation: RuntimeOperation::StartWorker,
                failure: IoFailure::from_error(source),
            },
        })
    }
}

/// 生产文件系统根的结构化机制错误。
#[derive(Debug)]
pub(crate) enum SystemFileSystemError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Windows(WindowsFsError),
    Closed,
    WorkerPanicked,
    WindowsOrdinalCaseKey {
        path: PathBuf,
        source: WindowsOrdinalCaseKeyError,
    },
    InvalidPath {
        path: PathBuf,
        violation: FileSystemPathViolation,
    },
    Cancelled {
        operation: &'static str,
        path: PathBuf,
    },
    WrongPublisherInstance,
    InvalidStagedIdentity {
        path: PathBuf,
    },
    DirectChildRollbackFailed {
        path: PathBuf,
        operation: Box<SystemFileSystemError>,
        rollback: Box<SystemFileSystemError>,
    },
    ScopedEditRollbackFailed {
        path: PathBuf,
        operation: Box<SystemFileSystemError>,
        rollback: Box<SystemFileSystemError>,
    },
    ObservationCleanupFailed {
        temporary_path: PathBuf,
        operation: Box<SystemFileSystemError>,
        cleanup: Box<SystemFileSystemError>,
    },
    JournalCorrupt {
        path: PathBuf,
        violation: FileSystemJournalViolation,
    },
    RecoveryJournalCorrupt {
        path: PathBuf,
        artifacts: Vec<PathBuf>,
        violation: FileSystemJournalViolation,
    },
    RecoveryRequired {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        violation: FileSystemRecoveryViolation,
    },
    RecoveryCleanupFailed {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        source: Box<SystemFileSystemError>,
    },
    PublishedRecoveryCleanupFailed {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        source: Box<SystemFileSystemError>,
    },
    RecoveryOutcomeUnknown {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        source: Box<SystemFileSystemError>,
    },
    OutcomeUnknown {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        violation: FileSystemRecoveryViolation,
    },
}

impl fmt::Display for SystemFileSystemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} 失败：{source}", path.display()),
            Self::Windows(source) => source.fmt(formatter),
            Self::Closed => formatter.write_str("文件系统根已经停止接收工作"),
            Self::WorkerPanicked => formatter.write_str("文件系统工作线程中的任务发生 panic"),
            Self::WindowsOrdinalCaseKey { path, source } => write!(
                formatter,
                "无法建立路径 {} 的 Windows ordinal 非大小写身份：{source}",
                path.display()
            ),
            Self::InvalidPath { path, violation } => {
                write!(
                    formatter,
                    "文件系统路径无效 {}：{violation:?}",
                    path.display()
                )
            }
            Self::Cancelled { operation, path } => {
                write!(formatter, "{operation}已取消：{}", path.display())
            }
            Self::WrongPublisherInstance => formatter.write_str("目录候选被交给了另一个发布根实例"),
            Self::InvalidStagedIdentity { path } => write!(
                formatter,
                "目录候选的物理文件身份已经变化：{}",
                path.display()
            ),
            Self::DirectChildRollbackFailed {
                path,
                operation,
                rollback,
            } => write!(
                formatter,
                "直接子目录 {} 建立后父目录身份发生变化（{operation}），且无法回滚：{rollback}",
                path.display()
            ),
            Self::ScopedEditRollbackFailed {
                path,
                operation,
                rollback,
            } => write!(
                formatter,
                "候选编辑 {} 失败（{operation}），且无法恢复原内容：{rollback}",
                path.display()
            ),
            Self::ObservationCleanupFailed {
                temporary_path,
                operation,
                cleanup,
            } => write!(
                formatter,
                "可观测性文件写入失败（{operation}），且无法清理临时文件 {}：{cleanup}",
                temporary_path.display()
            ),
            Self::JournalCorrupt { path, violation } => write!(
                formatter,
                "目录恢复 journal 损坏 {}：{violation:?}",
                path.display()
            ),
            Self::RecoveryJournalCorrupt {
                path,
                artifacts,
                violation,
            } => write!(
                formatter,
                "目录恢复 journal 损坏 {}（恢复产物：{}）：{violation:?}",
                path.display(),
                display_paths(artifacts)
            ),
            Self::RecoveryRequired {
                target_root,
                artifacts,
                violation,
            } => write!(
                formatter,
                "目录 {} 需要继续恢复（{}）：{violation:?}",
                target_root.display(),
                display_paths(artifacts)
            ),
            Self::RecoveryCleanupFailed {
                target_root,
                artifacts,
                source,
            } => write!(
                formatter,
                "目录 {} 已恢复到明确状态，但清理受管产物失败（{}）：{source}",
                target_root.display(),
                display_paths(artifacts)
            ),
            Self::PublishedRecoveryCleanupFailed {
                target_root,
                artifacts,
                source,
            } => write!(
                formatter,
                "目录 {} 已确认发布，但清理受管产物失败（{}）：{source}",
                target_root.display(),
                display_paths(artifacts)
            ),
            Self::RecoveryOutcomeUnknown {
                target_root,
                artifacts,
                source,
            } => write!(
                formatter,
                "无法确认目录 {} 与受管恢复产物的关系（{}）：{source}",
                target_root.display(),
                display_paths(artifacts)
            ),
            Self::OutcomeUnknown {
                target_root,
                artifacts,
                violation,
            } => write!(
                formatter,
                "目录 {} 的发布结果无法归类（{}）：{violation:?}",
                target_root.display(),
                display_paths(artifacts)
            ),
        }
    }
}

impl Error for SystemFileSystemError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Windows(source) => Some(source),
            Self::WindowsOrdinalCaseKey { source, .. } => Some(source),
            Self::DirectChildRollbackFailed { operation, .. }
            | Self::ScopedEditRollbackFailed { operation, .. }
            | Self::ObservationCleanupFailed { operation, .. }
            | Self::RecoveryCleanupFailed {
                source: operation, ..
            }
            | Self::PublishedRecoveryCleanupFailed {
                source: operation, ..
            }
            | Self::RecoveryOutcomeUnknown {
                source: operation, ..
            } => Some(operation),
            Self::Closed
            | Self::WorkerPanicked
            | Self::InvalidPath { .. }
            | Self::Cancelled { .. }
            | Self::WrongPublisherInstance
            | Self::InvalidStagedIdentity { .. }
            | Self::JournalCorrupt { .. }
            | Self::RecoveryJournalCorrupt { .. }
            | Self::RecoveryRequired { .. }
            | Self::OutcomeUnknown { .. } => None,
        }
    }
}

impl SystemFileSystemError {
    pub(crate) fn terminal_observation_operation(&self) -> Option<TerminalObservationOperation> {
        let Self::Io { operation, .. } = self else {
            return None;
        };
        match *operation {
            OBSERVATION_CREATE_OPERATION => Some(TerminalObservationOperation::Create),
            OBSERVATION_WRITE_OPERATION | OBSERVATION_COMMIT_OPERATION => {
                Some(TerminalObservationOperation::Write)
            }
            OBSERVATION_FLUSH_OPERATION => Some(TerminalObservationOperation::Flush),
            OBSERVATION_SYNC_OPERATION => Some(TerminalObservationOperation::Sync),
            OBSERVATION_CLEANUP_OPERATION => Some(TerminalObservationOperation::Cleanup),
            _ => None,
        }
    }

    pub(crate) fn shutdown_diagnostic_report(&self) -> DiagnosticReport {
        self.diagnostic_report(
            FileSystemDiagnosticContext::new(
                FileSystemDiagnosticStage::Shutdown,
                FileSystemOperation::Shutdown,
            ),
            StateEffect::AppliedFinalizationFailed,
        )
    }

    /// 以调用处给出的闭集 stage/operation 建立报告；旧错误中的显示文本不参与投影。
    pub(crate) fn diagnostic_report(
        &self,
        context: FileSystemDiagnosticContext,
        effect: StateEffect,
    ) -> DiagnosticReport {
        let report = |problem| {
            DiagnosticReport::new(
                effect,
                Diagnostic::file_system(FileSystemIssue::new(context, problem)),
            )
        };
        match self {
            Self::Io { path, source, .. } => report(FileSystemProblem::Io {
                path: SafePath::new(path),
                failure: IoFailure::from_error(source),
            }),
            Self::Windows(source) => DiagnosticReport::new(effect, source.diagnostic(context)),
            Self::Closed => report(FileSystemProblem::ExecutorClosed),
            Self::WorkerPanicked => report(FileSystemProblem::WorkerPanicked),
            Self::WindowsOrdinalCaseKey { path, source } => match source {
                WindowsOrdinalCaseKeyError::InputTooLarge { maximum, observed } => {
                    report(FileSystemProblem::OrdinalKeyTooLarge {
                        path: SafePath::new(path),
                        observed: *observed,
                        maximum: *maximum,
                    })
                }
                WindowsOrdinalCaseKeyError::WindowsApi { phase, source } => {
                    report(FileSystemProblem::OrdinalKeyIo {
                        path: SafePath::new(path),
                        phase: match phase {
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Measure => {
                                FileSystemOrdinalKeyPhase::Measure
                            }
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Map => {
                                FileSystemOrdinalKeyPhase::Map
                            }
                        },
                        failure: IoFailure::from_error(source),
                    })
                }
            },
            Self::InvalidPath { path, violation } => report(FileSystemProblem::InvalidPath {
                path: SafePath::new(path),
                violation: *violation,
            }),
            Self::Cancelled { path, .. } => report(FileSystemProblem::Cancelled {
                path: SafePath::new(path),
            }),
            Self::WrongPublisherInstance => report(FileSystemProblem::WrongPublisherInstance),
            Self::InvalidStagedIdentity { path } => report(FileSystemProblem::IdentityChanged {
                path: SafePath::new(path),
            }),
            Self::DirectChildRollbackFailed {
                operation,
                rollback,
                ..
            }
            | Self::ScopedEditRollbackFailed {
                operation,
                rollback,
                ..
            } => operation.diagnostic_report(context, effect).with_related(
                RelatedFailureRelation::Rollback,
                rollback.diagnostic_report(context, StateEffect::RecoveryRequired),
            ),
            Self::ObservationCleanupFailed {
                operation, cleanup, ..
            } => operation.diagnostic_report(context, effect).with_related(
                RelatedFailureRelation::Cleanup,
                cleanup.diagnostic_report(context, effect),
            ),
            Self::JournalCorrupt { path, violation } => report(FileSystemProblem::JournalCorrupt {
                path: SafePath::new(path),
                artifacts: Vec::new(),
                violation: violation.clone(),
            }),
            Self::RecoveryJournalCorrupt {
                path,
                artifacts,
                violation,
            } => DiagnosticReport::new(
                StateEffect::RecoveryRequired,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::JournalCorrupt {
                        path: SafePath::new(path),
                        artifacts: safe_paths(artifacts),
                        violation: violation.clone(),
                    },
                )),
            ),
            Self::RecoveryRequired {
                target_root,
                artifacts,
                violation,
            } => DiagnosticReport::new(
                StateEffect::RecoveryRequired,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::RecoveryRequired {
                        target_root: SafePath::new(target_root),
                        artifacts: safe_paths(artifacts),
                        violation: *violation,
                    },
                )),
            ),
            Self::RecoveryCleanupFailed {
                target_root,
                artifacts,
                source,
            }
            | Self::PublishedRecoveryCleanupFailed {
                target_root,
                artifacts,
                source,
            } => DiagnosticReport::new(
                if matches!(self, Self::PublishedRecoveryCleanupFailed { .. }) {
                    StateEffect::AppliedFinalizationFailed
                } else {
                    StateEffect::RecoveryRequired
                },
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::RecoveryCleanupFailed {
                        target_root: SafePath::new(target_root),
                        artifacts: safe_paths(artifacts),
                    },
                )),
            )
            .with_related(
                RelatedFailureRelation::Cleanup,
                source.diagnostic_report(context, effect),
            ),
            Self::RecoveryOutcomeUnknown {
                target_root,
                artifacts,
                source,
            } => DiagnosticReport::new(
                StateEffect::OutcomeUnknown,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::OutcomeUnknown {
                        target_root: SafePath::new(target_root),
                        artifacts: safe_paths(artifacts),
                        violation: FileSystemRecoveryViolation::ObservationFailed,
                    },
                )),
            )
            .with_related(
                RelatedFailureRelation::Finalization,
                source.diagnostic_report(context, StateEffect::OutcomeUnknown),
            ),
            Self::OutcomeUnknown {
                target_root,
                artifacts,
                violation,
            } => DiagnosticReport::new(
                StateEffect::OutcomeUnknown,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::OutcomeUnknown {
                        target_root: SafePath::new(target_root),
                        artifacts: safe_paths(artifacts),
                        violation: *violation,
                    },
                )),
            ),
        }
    }
}

impl ResolveDirectoryError<SystemFileSystemError> {
    /// 解析目录的领域边界固定无状态变更；调用方只选择闭集阶段。
    pub(crate) fn diagnostic_report_at(
        &self,
        stage: FileSystemDiagnosticStage,
    ) -> DiagnosticReport {
        let context =
            FileSystemDiagnosticContext::new(stage, FileSystemOperation::ResolveDirectory);
        match self {
            Self::NotFound { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotFound {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::NotDirectory { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotDirectory {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::Io { source, .. } => source.diagnostic_report(context, StateEffect::Unchanged),
        }
    }

    pub(crate) fn command_preparation_diagnostic_report(&self) -> DiagnosticReport {
        self.diagnostic_report_at(FileSystemDiagnosticStage::CommandPreparation)
    }
}

impl ListDirectoryError<SystemFileSystemError> {
    pub(crate) fn diagnostic_report_at(
        &self,
        stage: FileSystemDiagnosticStage,
    ) -> DiagnosticReport {
        let context = FileSystemDiagnosticContext::new(stage, FileSystemOperation::ListDirectory);
        match self {
            Self::NotFound { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotFound {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::NotDirectory { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotDirectory {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::Io { source, .. } => source.diagnostic_report(context, StateEffect::Unchanged),
        }
    }
}

impl DirectoryTreeFingerprintError<Box<SystemFileSystemError>> {
    pub(crate) fn diagnostic_report_at(
        &self,
        stage: FileSystemDiagnosticStage,
    ) -> DiagnosticReport {
        let context = FileSystemDiagnosticContext::new(stage, FileSystemOperation::FingerprintTree);
        match self {
            Self::NotFound { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotFound {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::NotDirectory { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotDirectory {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::ChangedDuringObservation { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::IdentityChanged {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::Failed { source, .. } => {
                source.diagnostic_report(context, StateEffect::Unchanged)
            }
        }
    }
}

impl ReadFileError<SystemFileSystemError> {
    /// 完整读取文件的领域边界固定无状态变更；调用方只选择闭集阶段。
    pub(crate) fn diagnostic_report_at(
        &self,
        stage: FileSystemDiagnosticStage,
    ) -> DiagnosticReport {
        let context = FileSystemDiagnosticContext::new(stage, FileSystemOperation::Read);
        match self {
            Self::NotFound { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotFound {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::NotFile { path } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    context,
                    FileSystemProblem::NotFile {
                        path: SafePath::new(path),
                    },
                )),
            ),
            Self::Io { source, .. } => source.diagnostic_report(context, StateEffect::Unchanged),
        }
    }

    pub(crate) fn command_preparation_diagnostic_report(&self) -> DiagnosticReport {
        self.diagnostic_report_at(FileSystemDiagnosticStage::CommandPreparation)
    }
}

impl DirectoryPublicationDiagnosticSource for Box<SystemFileSystemError> {
    fn publication_diagnostic(&self, step: PublicationStep) -> DirectoryPublicationDiagnostic {
        let operation = match step {
            PublicationStep::Recover => crate::diagnostic::FileSystemOperation::RecoverTarget,
            PublicationStep::PrepareCandidate => {
                crate::diagnostic::FileSystemOperation::PrepareCandidate
            }
            PublicationStep::Publish | PublicationStep::Finalize => {
                crate::diagnostic::FileSystemOperation::Rename
            }
            PublicationStep::DiscardCandidate | PublicationStep::CleanupResidual => {
                crate::diagnostic::FileSystemOperation::Remove
            }
        };
        let effect = if matches!(self.as_ref(), SystemFileSystemError::JournalCorrupt { .. }) {
            StateEffect::RecoveryRequired
        } else {
            StateEffect::Unchanged
        };
        let report = self.as_ref().diagnostic_report(
            FileSystemDiagnosticContext::new(
                crate::diagnostic::FileSystemDiagnosticStage::Publication,
                operation,
            ),
            effect,
        );
        publication_projection(report)
    }
}

fn publication_projection(report: DiagnosticReport) -> DirectoryPublicationDiagnostic {
    let mut projection =
        DirectoryPublicationDiagnostic::new(PublicationBackendCause::new(report.primary().clone()))
            .with_effect(report.effect());
    for related in report.related() {
        projection = projection.with_related(
            related.relation(),
            publication_projection(related.report().clone()),
        );
    }
    projection
}

fn safe_paths(paths: &[PathBuf]) -> Vec<SafePath> {
    paths.iter().map(SafePath::new).collect()
}

impl From<WindowsFsError> for SystemFileSystemError {
    fn from(source: WindowsFsError) -> Self {
        Self::Windows(source)
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(public_path).collect::<Vec<_>>().join("、")
}

pub(super) fn io_error(
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> SystemFileSystemError {
    SystemFileSystemError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests;
