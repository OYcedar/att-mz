//! 可恢复目录发布的封闭诊断模型。

use serde::{Deserialize, Serialize};

use super::{Diagnostic, DiagnosticResolution, DiagnosticStage, SafeIdentifier, SafePath};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationBackendCause {
    diagnostic: Box<Diagnostic>,
}

impl PublicationBackendCause {
    pub(crate) fn new(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
        }
    }

    pub(crate) fn into_diagnostic(self) -> Diagnostic {
        *self.diagnostic
    }

    const fn resolution(&self) -> DiagnosticResolution {
        self.diagnostic.resolution()
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("backend_code", self.diagnostic.code().to_owned())];
        facts.extend(self.diagnostic.issue().facts());
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationStep {
    Recover,
    PrepareCandidate,
    Publish,
    Finalize,
    DiscardCandidate,
    CleanupResidual,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PublicationRequestViolation {
    EmptyTargetRoot,
    EmptySourceDirectory,
    EmptySourceMappings,
    InvalidRelativePath {
        path: SafePath,
    },
    OverlappingSourceTargets {
        first: SafePath,
        second: SafePath,
    },
    OverlappingOverlays {
        first: SafePath,
        second: SafePath,
    },
    OverlappingEmptyDirectories {
        first: SafePath,
        second: SafePath,
    },
    OverlayOutsideSourceMappings {
        relative_file: SafePath,
    },
    EmptyDirectoryOverlapsSourceTarget {
        empty_directory: SafePath,
        source_target: SafePath,
    },
    EmptyDirectoryOverlapsOverlay {
        empty_directory: SafePath,
        overlay: SafePath,
    },
}

impl PublicationRequestViolation {
    const fn code(&self) -> &'static str {
        match self {
            Self::EmptyTargetRoot => "publication.request.empty_target_root",
            Self::EmptySourceDirectory => "publication.request.empty_source_directory",
            Self::EmptySourceMappings => "publication.request.empty_source_mappings",
            Self::InvalidRelativePath { .. } => "publication.request.invalid_relative_path",
            Self::OverlappingSourceTargets { .. } => {
                "publication.request.overlapping_source_targets"
            }
            Self::OverlappingOverlays { .. } => "publication.request.overlapping_overlays",
            Self::OverlappingEmptyDirectories { .. } => {
                "publication.request.overlapping_empty_directories"
            }
            Self::OverlayOutsideSourceMappings { .. } => {
                "publication.request.overlay_outside_source_mappings"
            }
            Self::EmptyDirectoryOverlapsSourceTarget { .. } => {
                "publication.request.empty_directory_overlaps_source_target"
            }
            Self::EmptyDirectoryOverlapsOverlay { .. } => {
                "publication.request.empty_directory_overlaps_overlay"
            }
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::EmptyTargetRoot | Self::EmptySourceDirectory | Self::EmptySourceMappings => {
                Vec::new()
            }
            Self::InvalidRelativePath { path } => vec![("path", path.to_string())],
            Self::OverlappingSourceTargets { first, second }
            | Self::OverlappingOverlays { first, second }
            | Self::OverlappingEmptyDirectories { first, second } => vec![
                ("first_path", first.to_string()),
                ("second_path", second.to_string()),
            ],
            Self::OverlayOutsideSourceMappings { relative_file } => {
                vec![("relative_file", relative_file.to_string())]
            }
            Self::EmptyDirectoryOverlapsSourceTarget {
                empty_directory,
                source_target,
            } => vec![
                ("empty_directory", empty_directory.to_string()),
                ("source_target", source_target.to_string()),
            ],
            Self::EmptyDirectoryOverlapsOverlay {
                empty_directory,
                overlay,
            } => vec![
                ("empty_directory", empty_directory.to_string()),
                ("overlay", overlay.to_string()),
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PublicationCandidateBindingProblem {
    WrongPublisherInstance,
    CandidateFinalized,
    CandidateIdentityChanged,
    BackendFailed {
        path: Option<SafePath>,
        cause: PublicationBackendCause,
    },
}

impl PublicationCandidateBindingProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::WrongPublisherInstance => "publication.candidate.bind.wrong_publisher_instance",
            Self::CandidateFinalized => "publication.candidate.bind.already_finalized",
            Self::CandidateIdentityChanged => "publication.candidate.bind.identity_changed",
            Self::BackendFailed { .. } => "publication.candidate.bind.backend_failed",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::WrongPublisherInstance | Self::CandidateFinalized => {
                DiagnosticResolution::ReportBug
            }
            Self::CandidateIdentityChanged => DiagnosticResolution::CheckPathAndPermissions,
            Self::BackendFailed { cause, .. } => cause.resolution(),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::BackendFailed { path, cause } => {
                let mut facts = Vec::new();
                if let Some(path) = path {
                    facts.push(("path", path.to_string()));
                }
                facts.extend(cause.facts());
                facts
            }
            Self::WrongPublisherInstance
            | Self::CandidateFinalized
            | Self::CandidateIdentityChanged => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PublicationCandidateInspectionProblem {
    WrongPublisherInstance,
    CandidateIdentityChanged,
    OutsideScope {
        path: SafePath,
    },
    ScopeRootMutation {
        path: SafePath,
    },
    EntryNotFound {
        path: SafePath,
    },
    EntryNotFile {
        path: SafePath,
    },
    EntryNotDirectory {
        path: SafePath,
    },
    BackendFailed {
        path: Option<SafePath>,
        cause: PublicationBackendCause,
    },
}

impl PublicationCandidateInspectionProblem {
    const fn code(&self) -> &'static str {
        match self {
            Self::WrongPublisherInstance => {
                "publication.candidate.inspect.wrong_publisher_instance"
            }
            Self::CandidateIdentityChanged => "publication.candidate.inspect.identity_changed",
            Self::OutsideScope { .. } => "publication.candidate.inspect.outside_scope",
            Self::ScopeRootMutation { .. } => "publication.candidate.inspect.scope_root_mutation",
            Self::EntryNotFound { .. } => "publication.candidate.inspect.entry_not_found",
            Self::EntryNotFile { .. } => "publication.candidate.inspect.entry_not_file",
            Self::EntryNotDirectory { .. } => "publication.candidate.inspect.entry_not_directory",
            Self::BackendFailed { .. } => "publication.candidate.inspect.backend_failed",
        }
    }

    const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::WrongPublisherInstance => DiagnosticResolution::ReportBug,
            Self::CandidateIdentityChanged => DiagnosticResolution::CheckPathAndPermissions,
            Self::BackendFailed { cause, .. } => cause.resolution(),
            Self::OutsideScope { .. } | Self::ScopeRootMutation { .. } => {
                DiagnosticResolution::ReportBug
            }
            Self::EntryNotFound { .. }
            | Self::EntryNotFile { .. }
            | Self::EntryNotDirectory { .. } => DiagnosticResolution::CheckProjectState,
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::OutsideScope { path }
            | Self::ScopeRootMutation { path }
            | Self::EntryNotFound { path }
            | Self::EntryNotFile { path }
            | Self::EntryNotDirectory { path } => vec![("path", path.to_string())],
            Self::BackendFailed { path, cause } => {
                let mut facts = Vec::new();
                if let Some(path) = path {
                    facts.push(("path", path.to_string()));
                }
                facts.extend(cause.facts());
                facts
            }
            Self::WrongPublisherInstance | Self::CandidateIdentityChanged => Vec::new(),
        }
    }
}

impl PublicationStep {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Recover => "recover",
            Self::PrepareCandidate => "prepare_candidate",
            Self::Publish => "publish",
            Self::Finalize => "finalize",
            Self::DiscardCandidate => "discard_candidate",
            Self::CleanupResidual => "cleanup_residual",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum PublicationProblem {
    CandidateProjectMismatch {
        expected_project: SafeIdentifier,
        expected_workspace_root: SafePath,
        candidate_project: SafeIdentifier,
        candidate_workspace_root: SafePath,
    },
    InvalidRequest {
        output_root: SafePath,
        violation: PublicationRequestViolation,
    },
    CandidateBindingFailed {
        candidate_root: SafePath,
        problem: PublicationCandidateBindingProblem,
    },
    CandidateInspectionFailed {
        candidate_root: SafePath,
        problem: PublicationCandidateInspectionProblem,
    },
    InvalidCandidateStructure {
        candidate_root: SafePath,
    },
    PrepareFailed {
        output_root: SafePath,
        candidate_root: Option<SafePath>,
        cause: PublicationBackendCause,
    },
    TargetAlreadyExists {
        output_root: SafePath,
    },
    TargetMissing {
        output_root: SafePath,
    },
    TargetNotDirectory {
        output_root: SafePath,
    },
    NotAttempted {
        output_root: SafePath,
        cause: PublicationBackendCause,
    },
    NotPublished {
        output_root: SafePath,
        cause: PublicationBackendCause,
    },
    PublishedFinalizationFailed {
        output_root: SafePath,
        residual_path: SafePath,
        cause: PublicationBackendCause,
    },
    RecoveryRequired {
        output_root: SafePath,
        recovery_artifacts: Vec<SafePath>,
        cause: PublicationBackendCause,
    },
    OutcomeUnknown {
        output_root: SafePath,
        recovery_artifacts: Vec<SafePath>,
        cause: PublicationBackendCause,
    },
    RecoveryFailed {
        output_root: SafePath,
        cause: PublicationBackendCause,
    },
    DiscardFailed {
        candidate_root: SafePath,
        cause: PublicationBackendCause,
    },
    CleanupFailed {
        residual_path: SafePath,
        cause: PublicationBackendCause,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationIssue {
    step: PublicationStep,
    problem: PublicationProblem,
}

impl PublicationIssue {
    pub(crate) const fn new(step: PublicationStep, problem: PublicationProblem) -> Self {
        Self { step, problem }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        DiagnosticStage::Publication
    }

    pub(crate) fn code(&self) -> &'static str {
        match &self.problem {
            PublicationProblem::CandidateProjectMismatch { .. } => {
                "publication.candidate.project_mismatch"
            }
            PublicationProblem::InvalidRequest { violation, .. } => violation.code(),
            PublicationProblem::CandidateBindingFailed { problem, .. } => problem.code(),
            PublicationProblem::CandidateInspectionFailed { problem, .. } => problem.code(),
            PublicationProblem::InvalidCandidateStructure { .. } => {
                "publication.candidate.invalid_structure"
            }
            PublicationProblem::PrepareFailed { .. } => "publication.prepare_failed",
            PublicationProblem::TargetAlreadyExists { .. } => "publication.target_already_exists",
            PublicationProblem::TargetMissing { .. } => "publication.target_missing",
            PublicationProblem::TargetNotDirectory { .. } => "publication.target_not_directory",
            PublicationProblem::NotAttempted { .. } => "publication.not_attempted",
            PublicationProblem::NotPublished { .. } => "publication.not_published",
            PublicationProblem::PublishedFinalizationFailed { .. } => {
                "publication.finalization_failed"
            }
            PublicationProblem::RecoveryRequired { .. } => "publication.recovery_required",
            PublicationProblem::OutcomeUnknown { .. } => "publication.outcome_unknown",
            PublicationProblem::RecoveryFailed { .. } => "publication.recovery_failed",
            PublicationProblem::DiscardFailed { .. } => "publication.discard_failed",
            PublicationProblem::CleanupFailed { .. } => "publication.cleanup_failed",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match &self.problem {
            PublicationProblem::CandidateProjectMismatch { .. }
            | PublicationProblem::InvalidRequest { .. } => DiagnosticResolution::ReportBug,
            PublicationProblem::CandidateBindingFailed { problem, .. } => problem.resolution(),
            PublicationProblem::CandidateInspectionFailed { problem, .. } => problem.resolution(),
            PublicationProblem::InvalidCandidateStructure { .. } => {
                DiagnosticResolution::CheckProjectState
            }
            PublicationProblem::TargetAlreadyExists { .. }
            | PublicationProblem::TargetMissing { .. }
            | PublicationProblem::TargetNotDirectory { .. } => {
                DiagnosticResolution::CheckProjectState
            }
            PublicationProblem::PublishedFinalizationFailed { .. }
            | PublicationProblem::RecoveryRequired { .. }
            | PublicationProblem::OutcomeUnknown { .. }
            | PublicationProblem::RecoveryFailed { .. }
            | PublicationProblem::DiscardFailed { .. }
            | PublicationProblem::CleanupFailed { .. } => {
                DiagnosticResolution::PreserveRecoveryArtifacts
            }
            PublicationProblem::PrepareFailed { cause, .. }
            | PublicationProblem::NotAttempted { cause, .. }
            | PublicationProblem::NotPublished { cause, .. } => cause.resolution(),
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match &self.problem {
            PublicationProblem::CandidateProjectMismatch { .. }
            | PublicationProblem::InvalidRequest { .. } => "internal_invariant",
            PublicationProblem::CandidateBindingFailed { .. }
            | PublicationProblem::CandidateInspectionFailed { .. } => "operation_failed",
            PublicationProblem::InvalidCandidateStructure { .. } => "invalid_content",
            PublicationProblem::TargetAlreadyExists { .. } => "already_exists",
            PublicationProblem::TargetMissing { .. } => "not_found",
            PublicationProblem::TargetNotDirectory { .. } => "invalid_path",
            PublicationProblem::OutcomeUnknown { .. } => "transaction_outcome_unknown",
            PublicationProblem::PublishedFinalizationFailed { .. } => "finalization_failed",
            PublicationProblem::RecoveryRequired { .. }
            | PublicationProblem::RecoveryFailed { .. }
            | PublicationProblem::DiscardFailed { .. }
            | PublicationProblem::CleanupFailed { .. } => "recovery_required",
            PublicationProblem::PrepareFailed { .. }
            | PublicationProblem::NotAttempted { .. }
            | PublicationProblem::NotPublished { .. } => "operation_failed",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match &self.problem {
            PublicationProblem::CandidateProjectMismatch {
                candidate_workspace_root,
                ..
            } => candidate_workspace_root.to_string(),
            PublicationProblem::InvalidRequest { output_root, .. } => output_root.to_string(),
            PublicationProblem::CandidateBindingFailed { candidate_root, .. }
            | PublicationProblem::CandidateInspectionFailed { candidate_root, .. }
            | PublicationProblem::InvalidCandidateStructure { candidate_root } => {
                candidate_root.to_string()
            }
            PublicationProblem::PrepareFailed { output_root, .. }
            | PublicationProblem::TargetAlreadyExists { output_root }
            | PublicationProblem::TargetMissing { output_root }
            | PublicationProblem::TargetNotDirectory { output_root }
            | PublicationProblem::NotAttempted { output_root, .. }
            | PublicationProblem::NotPublished { output_root, .. }
            | PublicationProblem::PublishedFinalizationFailed { output_root, .. }
            | PublicationProblem::RecoveryRequired { output_root, .. }
            | PublicationProblem::OutcomeUnknown { output_root, .. }
            | PublicationProblem::RecoveryFailed { output_root, .. } => output_root.to_string(),
            PublicationProblem::DiscardFailed { candidate_root, .. } => candidate_root.to_string(),
            PublicationProblem::CleanupFailed { residual_path, .. } => residual_path.to_string(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("step", self.step.as_str().to_owned())];
        match &self.problem {
            PublicationProblem::CandidateProjectMismatch {
                expected_project,
                expected_workspace_root,
                candidate_project,
                candidate_workspace_root,
            } => facts.extend([
                ("expected_project", expected_project.to_string()),
                (
                    "expected_workspace_root",
                    expected_workspace_root.to_string(),
                ),
                ("candidate_project", candidate_project.to_string()),
                (
                    "candidate_workspace_root",
                    candidate_workspace_root.to_string(),
                ),
            ]),
            PublicationProblem::InvalidRequest {
                output_root,
                violation,
            } => {
                facts.push(("output_root", output_root.to_string()));
                facts.extend(violation.facts());
            }
            PublicationProblem::CandidateBindingFailed {
                candidate_root,
                problem,
            } => {
                facts.push(("candidate_root", candidate_root.to_string()));
                facts.extend(problem.facts());
            }
            PublicationProblem::CandidateInspectionFailed {
                candidate_root,
                problem,
            } => {
                facts.push(("candidate_root", candidate_root.to_string()));
                facts.extend(problem.facts());
            }
            PublicationProblem::InvalidCandidateStructure { candidate_root } => {
                facts.push(("candidate_root", candidate_root.to_string()));
            }
            PublicationProblem::PrepareFailed {
                output_root,
                candidate_root,
                cause,
            } => {
                facts.push(("output_root", output_root.to_string()));
                if let Some(candidate_root) = candidate_root {
                    facts.push(("candidate_root", candidate_root.to_string()));
                }
                facts.extend(cause.facts());
            }
            PublicationProblem::TargetAlreadyExists { output_root }
            | PublicationProblem::TargetMissing { output_root }
            | PublicationProblem::TargetNotDirectory { output_root } => {
                facts.push(("output_root", output_root.to_string()));
            }
            PublicationProblem::NotAttempted { output_root, cause }
            | PublicationProblem::NotPublished { output_root, cause }
            | PublicationProblem::RecoveryFailed { output_root, cause } => {
                facts.push(("output_root", output_root.to_string()));
                facts.extend(cause.facts());
            }
            PublicationProblem::PublishedFinalizationFailed {
                output_root,
                residual_path,
                cause,
            } => {
                facts.push(("output_root", output_root.to_string()));
                facts.push(("residual_path", residual_path.to_string()));
                facts.extend(cause.facts());
            }
            PublicationProblem::RecoveryRequired {
                output_root,
                recovery_artifacts,
                cause,
            }
            | PublicationProblem::OutcomeUnknown {
                output_root,
                recovery_artifacts,
                cause,
            } => {
                facts.push(("output_root", output_root.to_string()));
                for artifact in recovery_artifacts {
                    facts.push(("recovery_artifact", artifact.to_string()));
                }
                facts.extend(cause.facts());
            }
            PublicationProblem::DiscardFailed {
                candidate_root,
                cause,
            } => {
                facts.push(("candidate_root", candidate_root.to_string()));
                facts.extend(cause.facts());
            }
            PublicationProblem::CleanupFailed {
                residual_path,
                cause,
            } => {
                facts.push(("residual_path", residual_path.to_string()));
                facts.extend(cause.facts());
            }
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepublication_failure_uses_backend_resolution() {
        let cause = PublicationBackendCause::new(Diagnostic::file_system(
            crate::diagnostic::FileSystemIssue::new(
                crate::diagnostic::FileSystemDiagnosticContext::new(
                    crate::diagnostic::FileSystemDiagnosticStage::Publication,
                    crate::diagnostic::FileSystemOperation::PrepareCandidate,
                ),
                crate::diagnostic::FileSystemProblem::WrongPublisherInstance,
            ),
        ));
        let issue = PublicationIssue::new(
            PublicationStep::PrepareCandidate,
            PublicationProblem::PrepareFailed {
                output_root: SafePath::new("D:/output/game"),
                candidate_root: None,
                cause,
            },
        );

        assert_eq!(issue.resolution(), DiagnosticResolution::ReportBug);
    }
}
