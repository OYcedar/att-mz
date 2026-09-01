//! 把 Rewriter 候选提交给可恢复目录发布根。

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage,
    FileSystemIssue, FileSystemOperation, FileSystemPathViolation, FileSystemProblem,
    PublicationCandidateBindingProblem, PublicationCandidateInspectionProblem, PublicationIssue,
    PublicationProblem, PublicationRequestViolation, PublicationStep, RelatedFailureRelation,
    ReportedFailure, SafeIdentifier, SafePath, StateEffect,
};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::bootstrap::{RpgMakerBootstrapFiles, read_optional_bootstrap_files};
use crate::rpg_maker::project::OpenedProject;
use crate::runtime::filesystem::SystemFileSystemError;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryFileOverlay, DirectoryPrepareError,
    DirectoryPublicationDiagnostic, DirectoryPublicationDiagnosticSource, DirectoryPublishError,
    DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, FileReader, ReadFileError, RecoverableDirectoryPublisher,
    ScopedDirectoryBindError, ScopedDirectoryEditError, ScopedDirectoryEditor,
    ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryPath, SnapshotFileReader,
};

use super::rewriter::RpgMakerRewrittenDocuments;
use super::{
    PreparedWriteBackCandidate, PublishedWriteBack, RpgMakerWriteBackPublisher,
    WriteBackPublishFailure, WriteBackPublishFailureState, WriteBackPublishingDiagnostic,
};

/// 根已准备、只能发布或丢弃一次的完整写回候选。
pub(crate) struct PreparedWriteBack<S> {
    output_root: PathBuf,
    layout: RpgMakerLayout,
    source_root: PathBuf,
    source_bootstrap: Option<Arc<RpgMakerBootstrapFiles>>,
    candidate_bootstrap: Option<Arc<RpgMakerBootstrapFiles>>,
    staged: crate::storage::file_system::StagedDirectory<S>,
}

impl<S> fmt::Debug for PreparedWriteBack<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedWriteBack")
            .field("output_root", &self.output_root)
            .field("candidate_root", &self.staged.staging_root())
            .finish_non_exhaustive()
    }
}

impl<S> PreparedWriteBackCandidate for PreparedWriteBack<S>
where
    S: Send + 'static,
{
    fn candidate_root(&self) -> &Path {
        self.staged.staging_root()
    }
}

/// 用当前引擎布局下的完整冻结来源发布 RPG Maker 写回候选。
pub(crate) struct RpgMakerWriteBackPublishingService<F, A> {
    file_reader: F,
    directory_publisher: A,
}

impl<F, A> RpgMakerWriteBackPublishingService<F, A> {
    pub(crate) fn new(file_reader: F, directory_publisher: A) -> Self {
        Self {
            file_reader,
            directory_publisher,
        }
    }
}

impl<F, A> RpgMakerWriteBackPublisher<RpgMakerRewrittenDocuments>
    for RpgMakerWriteBackPublishingService<F, A>
where
    F: SnapshotFileReader,
    A: RecoverableDirectoryPublisher
        + ScopedDirectoryEditor<
            CandidateState = <A as RecoverableDirectoryPublisher>::StagingState,
            Error = <A as RecoverableDirectoryPublisher>::Error,
        >,
{
    type Candidate = PreparedWriteBack<<A as RecoverableDirectoryPublisher>::StagingState>;
    type Error = RpgMakerWriteBackPublishingError<
        <F as FileReader>::Error,
        <A as RecoverableDirectoryPublisher>::Error,
    >;

    async fn prepare(
        &self,
        project: &OpenedProject,
        documents: RpgMakerRewrittenDocuments,
    ) -> Result<Self::Candidate, Self::Error> {
        if documents.project_name() != project.name()
            || documents.workspace_root() != project.workspace_root()
        {
            return Err(RpgMakerWriteBackPublishingError::CandidateProjectMismatch {
                expected_name: project.name().clone(),
                expected_workspace_root: project.workspace_root().to_path_buf(),
                candidate_name: documents.project_name().clone(),
                candidate_workspace_root: documents.workspace_root().to_path_buf(),
            });
        }

        let output_root = project.layout().write_back_root().to_path_buf();
        let invalid_request = |source| RpgMakerWriteBackPublishingError::InvalidRequest {
            output_root: output_root.clone(),
            source,
        };
        let source_bootstrap = project.verified_bootstrap().cloned();
        let source_mappings = vec![
            DirectorySourceMapping::new(
                project.layout().source_data().to_path_buf(),
                project.layout().rpg_maker_layout().data_relative(),
            )
            .map_err(&invalid_request)?,
            DirectorySourceMapping::new(
                project.layout().source_js().to_path_buf(),
                project.layout().rpg_maker_layout().js_relative(),
            )
            .map_err(&invalid_request)?,
        ];
        let (rewritten_files, game_title_rewrite) = documents.into_parts();
        let rewritten_files = rewritten_files
            .into_iter()
            .map(|file| file.into_parts())
            .collect::<Vec<_>>();
        let mut bootstrap_overlays = Vec::with_capacity(2);
        let mut candidate_bootstrap = None;
        if let Some(bootstrap) = source_bootstrap.as_deref() {
            let rewritten_titles = game_title_rewrite
                .filter(|rewrite| !rewrite.original().is_empty())
                .map(|rewrite| {
                    (
                        bootstrap
                            .rewritten_package_title(rewrite.original(), rewrite.candidate())
                            .unwrap_or_else(|| bootstrap.package_bytes().to_vec()),
                        bootstrap
                            .rewritten_html_title(rewrite.original(), rewrite.candidate())
                            .unwrap_or_else(|| bootstrap.main_html_bytes().to_vec()),
                    )
                });
            let (package_bytes, html_bytes) = rewritten_titles.unwrap_or_else(|| {
                (
                    bootstrap.package_bytes().to_vec(),
                    bootstrap.main_html_bytes().to_vec(),
                )
            });
            let expected = if package_bytes == bootstrap.package_bytes()
                && html_bytes == bootstrap.main_html_bytes()
            {
                Arc::clone(source_bootstrap.as_ref().expect("已确认冻结启动壳存在"))
            } else {
                Arc::new(bootstrap.with_document_bytes(package_bytes, html_bytes))
            };
            bootstrap_overlays.push(
                DirectoryFileOverlay::new(
                    PathBuf::from("package.json"),
                    expected.package_bytes().to_vec(),
                )
                .map_err(&invalid_request)?,
            );
            bootstrap_overlays.push(
                DirectoryFileOverlay::new(
                    expected.main_relative().to_path_buf(),
                    expected.main_html_bytes().to_vec(),
                )
                .map_err(&invalid_request)?,
            );
            candidate_bootstrap = Some(expected);
        }
        let mut overlays = rewritten_files
            .into_iter()
            .map(|(relative_path, bytes)| {
                let relative_path = project
                    .layout()
                    .rpg_maker_layout()
                    .map_content_relative(&relative_path);
                DirectoryFileOverlay::new(relative_path, bytes).map_err(&invalid_request)
            })
            .collect::<Result<Vec<_>, _>>()?;
        overlays.append(&mut bootstrap_overlays);
        let request = DirectoryStageRequest::new(
            output_root.clone(),
            DirectoryPublishIntent::ReplaceExisting,
            source_mappings,
            overlays,
            Vec::new(),
        )
        .map_err(invalid_request)?;

        let staged = self
            .directory_publisher
            .prepare(request)
            .await
            .map_err(RpgMakerWriteBackPublishingError::Prepare)?;
        Ok(PreparedWriteBack {
            output_root: project.write_back_root().to_path_buf(),
            layout: project.layout().rpg_maker_layout(),
            source_root: project.layout().source_root().to_path_buf(),
            source_bootstrap,
            candidate_bootstrap,
            staged,
        })
    }

    fn validate<'a>(
        &'a self,
        candidate: &Self::Candidate,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + use<'a, F, A> {
        // 先同步建立不再借用候选的 bind future，避免把只承诺 Send 的根 token state
        // 通过 `&Candidate` 带入异步状态机并错误要求 Sync。
        let bind = self.directory_publisher.bind_scoped_directory(
            &candidate.staged,
            super::rpg_maker_output_scope(candidate.layout),
        );
        let candidate_root = candidate.candidate_root().to_path_buf();
        let layout = candidate.layout;
        let source_root = candidate.source_root.clone();
        let expected_source_bootstrap = candidate.source_bootstrap.clone();
        let expected_candidate_bootstrap = candidate.candidate_bootstrap.clone();
        let file_reader = &self.file_reader;
        let directory_publisher = &self.directory_publisher;
        async move {
            let observed_bootstrap = read_optional_bootstrap_files(file_reader, &source_root)
                .await
                .map_err(RpgMakerWriteBackPublishingError::ReadSource)?;
            if observed_bootstrap.as_ref() != expected_source_bootstrap.as_deref() {
                return Err(RpgMakerWriteBackPublishingError::SourceChanged { source_root });
            }
            let bootstrap_main = expected_candidate_bootstrap
                .as_deref()
                .map(|bootstrap| bootstrap.main_relative().to_path_buf());
            let scope =
                bind.await
                    .map_err(|source| RpgMakerWriteBackPublishingError::BindCandidate {
                        candidate_root: candidate_root.clone(),
                        source,
                    })?;
            let entries = directory_publisher
                .list_scoped_root(&scope)
                .await
                .map_err(
                    |source| RpgMakerWriteBackPublishingError::InspectCandidateRoot {
                        candidate_root: candidate_root.clone(),
                        source,
                    },
                )?;
            let structure_valid = if let Some(directory) = layout.content_directory() {
                let content_entries = directory_publisher
                    .list_scoped_directory(
                        &scope,
                        ScopedDirectoryPath::new(PathBuf::from(directory))
                            .expect("固定内容目录必须是安全相对路径"),
                    )
                    .await
                    .map_err(
                        |source| RpgMakerWriteBackPublishingError::InspectCandidateRoot {
                            candidate_root: candidate_root.clone(),
                            source,
                        },
                    )?;
                if let Some(main) = bootstrap_main.as_deref() {
                    validate_bootstrap_root(&entries, layout, main)
                        && validate_bootstrap_content(&content_entries, directory, main)
                } else {
                    validate_single_directory(&entries, directory)
                        && validate_data_and_js(&content_entries)
                }
            } else if let Some(main) = bootstrap_main.as_deref() {
                validate_bootstrap_root(&entries, layout, main)
            } else {
                validate_data_and_js(&entries)
            };
            let observed_candidate_bootstrap =
                read_optional_bootstrap_files(file_reader, &candidate_root)
                    .await
                    .map_err(
                        |source| RpgMakerWriteBackPublishingError::ReadCandidateBootstrap {
                            candidate_root: candidate_root.clone(),
                            source,
                        },
                    )?;
            let bootstrap_valid =
                observed_candidate_bootstrap.as_ref() == expected_candidate_bootstrap.as_deref();
            if structure_valid && bootstrap_valid {
                Ok(())
            } else {
                if !bootstrap_valid {
                    Err(
                        RpgMakerWriteBackPublishingError::CandidateBootstrapChanged {
                            candidate_root,
                        },
                    )
                } else {
                    Err(RpgMakerWriteBackPublishingError::InvalidCandidateRoot {
                        root: candidate_root,
                    })
                }
            }
        }
    }

    async fn publish(
        &self,
        candidate: Self::Candidate,
    ) -> Result<PublishedWriteBack, WriteBackPublishFailure<Self::Error>> {
        let PreparedWriteBack {
            output_root,
            staged,
            ..
        } = candidate;
        if let Err(source) = self.directory_publisher.publish(staged).await {
            let state = publish_failure_state(&source);
            return Err(WriteBackPublishFailure::new(
                state,
                RpgMakerWriteBackPublishingError::Publish(source),
            ));
        }
        Ok(PublishedWriteBack::new(output_root))
    }

    async fn discard(&self, candidate: Self::Candidate) -> Result<(), Self::Error> {
        self.directory_publisher
            .discard(candidate.staged)
            .await
            .map_err(RpgMakerWriteBackPublishingError::Discard)
    }
}

fn publish_failure_state<E>(source: &DirectoryPublishError<E>) -> WriteBackPublishFailureState {
    match source {
        DirectoryPublishError::TargetAlreadyExists {
            target_root,
            cleanup_failure,
        }
        | DirectoryPublishError::TargetMissing {
            target_root,
            cleanup_failure,
        }
        | DirectoryPublishError::TargetNotDirectory {
            target_root,
            cleanup_failure,
        }
        | DirectoryPublishError::NotAttempted {
            target_root,
            cleanup_failure,
            ..
        }
        | DirectoryPublishError::NotPublished {
            target_root,
            cleanup_failure,
            ..
        } => WriteBackPublishFailureState::NotPublished {
            output_root: target_root.clone(),
            residual_paths: cleanup_failure
                .iter()
                .map(|failure| failure.residual_path().to_path_buf())
                .collect(),
        },
        DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path,
            ..
        } => WriteBackPublishFailureState::PublishedWithResiduals {
            output_root: target_root.clone(),
            residual_paths: vec![residual_path.clone()],
        },
        DirectoryPublishError::RecoveryRequired {
            target_root,
            recovery_artifacts,
            ..
        } => WriteBackPublishFailureState::RecoveryRequired {
            output_root: target_root.clone(),
            recovery_artifacts: recovery_artifacts.clone(),
        },
        DirectoryPublishError::OutcomeUnknown {
            target_root,
            recovery_artifacts,
            ..
        } => WriteBackPublishFailureState::OutcomeUnknown {
            output_root: target_root.clone(),
            recovery_artifacts: recovery_artifacts.clone(),
        },
    }
}

/// RPG Maker Publisher 在候选交接、请求建立或根终结阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackPublishingError<F, A> {
    CandidateProjectMismatch {
        expected_name: ProjectName,
        expected_workspace_root: PathBuf,
        candidate_name: ProjectName,
        candidate_workspace_root: PathBuf,
    },
    InvalidRequest {
        output_root: PathBuf,
        source: DirectoryStageRequestError,
    },
    ReadSource(ReadFileError<F>),
    ReadCandidateBootstrap {
        candidate_root: PathBuf,
        source: ReadFileError<F>,
    },
    SourceChanged {
        source_root: PathBuf,
    },
    CandidateBootstrapChanged {
        candidate_root: PathBuf,
    },
    Prepare(DirectoryPrepareError<A>),
    BindCandidate {
        candidate_root: PathBuf,
        source: ScopedDirectoryBindError<A>,
    },
    InspectCandidateRoot {
        candidate_root: PathBuf,
        source: ScopedDirectoryEditError<A>,
    },
    InvalidCandidateRoot {
        root: PathBuf,
    },
    Publish(DirectoryPublishError<A>),
    Discard(DirectoryDiscardError<A>),
}

impl<F, A> fmt::Display for RpgMakerWriteBackPublishingError<F, A>
where
    F: fmt::Display,
    A: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateProjectMismatch {
                expected_name,
                expected_workspace_root,
                candidate_name,
                candidate_workspace_root,
            } => write!(
                formatter,
                "写回候选不属于当前项目（当前：{} @ {}，候选：{} @ {}）",
                expected_name,
                expected_workspace_root.display(),
                candidate_name,
                candidate_workspace_root.display()
            ),
            Self::InvalidRequest { source, .. } => {
                write!(formatter, "写回候选请求无效：{source}")
            }
            Self::ReadSource(source) => write!(formatter, "无法读取冻结来源：{source}"),
            Self::ReadCandidateBootstrap {
                candidate_root,
                source,
            } => write!(
                formatter,
                "无法读取写回候选启动壳 {}：{source}",
                candidate_root.display()
            ),
            Self::SourceChanged { source_root } => write!(
                formatter,
                "冻结启动壳已与项目开启时的快照不一致：{}",
                source_root.display()
            ),
            Self::CandidateBootstrapChanged { candidate_root } => write!(
                formatter,
                "写回候选启动壳在验证前发生变化：{}",
                candidate_root.display()
            ),
            Self::Prepare(source) => source.fmt(formatter),
            Self::BindCandidate { source, .. } => {
                write!(formatter, "无法绑定写回候选的物理身份：{source}")
            }
            Self::InspectCandidateRoot { source, .. } => {
                write!(formatter, "无法检查写回候选顶层结构：{source}")
            }
            Self::InvalidCandidateRoot { root } => write!(
                formatter,
                "写回候选根不符合当前 RPG Maker 内容与标准启动壳结构：{}",
                root.display()
            ),
            Self::Publish(source) => source.fmt(formatter),
            Self::Discard(source) => source.fmt(formatter),
        }
    }
}

impl<F, A> Error for RpgMakerWriteBackPublishingError<F, A>
where
    F: Error + 'static,
    A: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CandidateProjectMismatch { .. } => None,
            Self::InvalidRequest { source, .. } => Some(source),
            Self::ReadSource(source) => Some(source),
            Self::ReadCandidateBootstrap { source, .. } => Some(source),
            Self::SourceChanged { .. } => None,
            Self::CandidateBootstrapChanged { .. } => None,
            Self::Prepare(source) => Some(source),
            Self::BindCandidate { source, .. } => Some(source),
            Self::InspectCandidateRoot { source, .. } => Some(source),
            Self::InvalidCandidateRoot { .. } => None,
            Self::Publish(source) => Some(source),
            Self::Discard(source) => Some(source),
        }
    }
}

impl<A> RpgMakerWriteBackPublishingError<SystemFileSystemError, A>
where
    A: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::CandidateProjectMismatch {
                expected_name,
                expected_workspace_root,
                candidate_name,
                candidate_workspace_root,
            } => publication_report(
                StateEffect::Unchanged,
                PublicationStep::PrepareCandidate,
                PublicationProblem::CandidateProjectMismatch {
                    expected_project: SafeIdentifier::from_validated(expected_name.as_str()),
                    expected_workspace_root: SafePath::new(expected_workspace_root),
                    candidate_project: SafeIdentifier::from_validated(candidate_name.as_str()),
                    candidate_workspace_root: SafePath::new(candidate_workspace_root),
                },
            ),
            Self::InvalidRequest {
                output_root,
                source,
            } => publication_report(
                StateEffect::Unchanged,
                PublicationStep::PrepareCandidate,
                PublicationProblem::InvalidRequest {
                    output_root: SafePath::new(output_root),
                    violation: request_violation(source),
                },
            ),
            Self::ReadSource(source) => {
                source.diagnostic_report_at(crate::diagnostic::FileSystemDiagnosticStage::WriteBack)
            }
            Self::ReadCandidateBootstrap { source, .. } => {
                source.diagnostic_report_at(crate::diagnostic::FileSystemDiagnosticStage::WriteBack)
            }
            Self::SourceChanged { source_root } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    FileSystemDiagnosticContext::new(
                        FileSystemDiagnosticStage::WriteBack,
                        FileSystemOperation::Read,
                    ),
                    FileSystemProblem::InvalidPath {
                        path: SafePath::new(source_root),
                        violation: FileSystemPathViolation::SourceChanged,
                    },
                )),
            ),
            Self::CandidateBootstrapChanged { candidate_root } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::file_system(FileSystemIssue::new(
                    FileSystemDiagnosticContext::new(
                        FileSystemDiagnosticStage::WriteBack,
                        FileSystemOperation::Read,
                    ),
                    FileSystemProblem::InvalidPath {
                        path: SafePath::new(candidate_root),
                        violation: FileSystemPathViolation::SourceChanged,
                    },
                )),
            ),
            Self::Prepare(source) => source.diagnostic_report(),
            Self::BindCandidate {
                candidate_root,
                source,
            } => candidate_bind_report(candidate_root, source),
            Self::InspectCandidateRoot {
                candidate_root,
                source,
            } => candidate_inspection_report(candidate_root, source),
            Self::InvalidCandidateRoot { root } => publication_report(
                StateEffect::Unchanged,
                PublicationStep::PrepareCandidate,
                PublicationProblem::InvalidCandidateStructure {
                    candidate_root: SafePath::new(root),
                },
            ),
            Self::Publish(source) => source.diagnostic_report(),
            Self::Discard(source) => source.diagnostic_report(),
        }
    }
}

impl<A> RpgMakerWriteBackPublishingError<SystemFileSystemError, A>
where
    A: DirectoryPublicationDiagnosticSource + Error + Send + Sync + 'static,
{
    pub(crate) fn into_reported_failure(self) -> ReportedFailure {
        let report = self.diagnostic_report();
        ReportedFailure::new(report, self)
    }
}

impl<A> WriteBackPublishingDiagnostic for RpgMakerWriteBackPublishingError<SystemFileSystemError, A>
where
    A: DirectoryPublicationDiagnosticSource + Error + Send + Sync + 'static,
{
    fn into_write_back_failure_report(self) -> ReportedFailure {
        self.into_reported_failure()
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

fn request_violation(source: &DirectoryStageRequestError) -> PublicationRequestViolation {
    match source {
        DirectoryStageRequestError::EmptyTargetRoot => PublicationRequestViolation::EmptyTargetRoot,
        DirectoryStageRequestError::EmptySourceDirectory => {
            PublicationRequestViolation::EmptySourceDirectory
        }
        DirectoryStageRequestError::EmptySourceMappings => {
            PublicationRequestViolation::EmptySourceMappings
        }
        DirectoryStageRequestError::InvalidRelativePath { path } => {
            PublicationRequestViolation::InvalidRelativePath {
                path: SafePath::new(path),
            }
        }
        DirectoryStageRequestError::OverlappingSourceTargets { first, second } => {
            PublicationRequestViolation::OverlappingSourceTargets {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlappingOverlays { first, second } => {
            PublicationRequestViolation::OverlappingOverlays {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlappingEmptyDirectories { first, second } => {
            PublicationRequestViolation::OverlappingEmptyDirectories {
                first: SafePath::new(first),
                second: SafePath::new(second),
            }
        }
        DirectoryStageRequestError::OverlayOverlapsSourceTarget {
            overlay,
            source_target,
        } => PublicationRequestViolation::OverlayOverlapsSourceTarget {
            overlay: SafePath::new(overlay),
            source_target: SafePath::new(source_target),
        },
        DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
            empty_directory,
            source_target,
        } => PublicationRequestViolation::EmptyDirectoryOverlapsSourceTarget {
            empty_directory: SafePath::new(empty_directory),
            source_target: SafePath::new(source_target),
        },
        DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
            empty_directory,
            overlay,
        } => PublicationRequestViolation::EmptyDirectoryOverlapsOverlay {
            empty_directory: SafePath::new(empty_directory),
            overlay: SafePath::new(overlay),
        },
    }
}

fn candidate_bind_report<E>(
    candidate_root: &Path,
    source: &ScopedDirectoryBindError<E>,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let (candidate_root, problem, related, source_effect) = match source {
        ScopedDirectoryBindError::WrongEditorInstance => (
            candidate_root,
            PublicationCandidateBindingProblem::WrongPublisherInstance,
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryBindError::CandidateFinalized { root } => (
            root.as_path(),
            PublicationCandidateBindingProblem::CandidateFinalized,
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryBindError::CandidateIdentityChanged { root } => (
            root.as_path(),
            PublicationCandidateBindingProblem::CandidateIdentityChanged,
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryBindError::Failed { root, source } => {
            let projection = source.publication_diagnostic(PublicationStep::PrepareCandidate);
            let (source_effect, cause, related) = projection.into_parts();
            (
                root.as_path(),
                PublicationCandidateBindingProblem::BackendFailed {
                    path: Some(SafePath::new(root)),
                    cause,
                },
                related,
                source_effect,
            )
        }
    };
    attach_backend_related(
        publication_report(
            StateEffect::Unchanged.strongest(source_effect),
            PublicationStep::PrepareCandidate,
            PublicationProblem::CandidateBindingFailed {
                candidate_root: SafePath::new(candidate_root),
                problem,
            },
        ),
        related,
    )
}

fn candidate_inspection_report<E>(
    candidate_root: &Path,
    source: &ScopedDirectoryEditError<E>,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let (problem, related, source_effect) = match source {
        ScopedDirectoryEditError::WrongEditorInstance => (
            PublicationCandidateInspectionProblem::WrongPublisherInstance,
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::OutsideScope { path } => (
            PublicationCandidateInspectionProblem::OutsideScope {
                path: SafePath::new(path),
            },
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::ScopeRootMutation { path } => (
            PublicationCandidateInspectionProblem::ScopeRootMutation {
                path: SafePath::new(path),
            },
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::NotFound { path } => (
            PublicationCandidateInspectionProblem::EntryNotFound {
                path: SafePath::new(path),
            },
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::NotFile { path } => (
            PublicationCandidateInspectionProblem::EntryNotFile {
                path: SafePath::new(path),
            },
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::NotDirectory { path } => (
            PublicationCandidateInspectionProblem::EntryNotDirectory {
                path: SafePath::new(path),
            },
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::CandidateIdentityChanged { .. } => (
            PublicationCandidateInspectionProblem::CandidateIdentityChanged,
            Vec::new(),
            StateEffect::Unchanged,
        ),
        ScopedDirectoryEditError::Failed { path, source } => {
            let projection = source.publication_diagnostic(PublicationStep::PrepareCandidate);
            let (source_effect, cause, related) = projection.into_parts();
            (
                PublicationCandidateInspectionProblem::BackendFailed {
                    path: Some(SafePath::new(path)),
                    cause,
                },
                related,
                source_effect,
            )
        }
    };
    attach_backend_related(
        publication_report(
            StateEffect::Unchanged.strongest(source_effect),
            PublicationStep::PrepareCandidate,
            PublicationProblem::CandidateInspectionFailed {
                candidate_root: SafePath::new(candidate_root),
                problem,
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

fn validate_data_and_js(entries: &[ScopedDirectoryEntry]) -> bool {
    if entries.len() != 2 {
        return false;
    }
    contains_data_and_js(entries)
}

fn contains_data_and_js(entries: &[ScopedDirectoryEntry]) -> bool {
    let has_data = entries.iter().any(|entry| {
        entry.name() == std::ffi::OsStr::new("data")
            && entry.kind() == ScopedDirectoryEntryKind::Directory
    });
    let has_js = entries.iter().any(|entry| {
        entry.name() == std::ffi::OsStr::new("js")
            && entry.kind() == ScopedDirectoryEntryKind::Directory
    });
    has_data && has_js
}

fn validate_bootstrap_root(
    entries: &[ScopedDirectoryEntry],
    layout: RpgMakerLayout,
    main: &Path,
) -> bool {
    let mut expected = std::collections::BTreeMap::new();
    expected.insert(
        std::ffi::OsString::from("package.json"),
        ScopedDirectoryEntryKind::File,
    );
    match layout.content_directory() {
        Some(directory) => {
            expected.insert(
                std::ffi::OsString::from(directory),
                ScopedDirectoryEntryKind::Directory,
            );
        }
        None => {
            expected.insert(
                std::ffi::OsString::from("data"),
                ScopedDirectoryEntryKind::Directory,
            );
            expected.insert(
                std::ffi::OsString::from("js"),
                ScopedDirectoryEntryKind::Directory,
            );
        }
    }
    let mut components = main.components();
    let Some(std::path::Component::Normal(root)) = components.next() else {
        return false;
    };
    let kind = if components.next().is_some() {
        ScopedDirectoryEntryKind::Directory
    } else {
        ScopedDirectoryEntryKind::File
    };
    if expected
        .insert(root.to_os_string(), kind)
        .is_some_and(|existing| existing != kind)
    {
        return false;
    }
    entries.len() == expected.len()
        && entries.iter().all(|entry| {
            expected
                .get(entry.name())
                .is_some_and(|kind| *kind == entry.kind())
        })
}

fn validate_bootstrap_content(
    entries: &[ScopedDirectoryEntry],
    content_directory: &str,
    main: &Path,
) -> bool {
    let mut expected = std::collections::BTreeMap::from([
        (
            std::ffi::OsString::from("data"),
            ScopedDirectoryEntryKind::Directory,
        ),
        (
            std::ffi::OsString::from("js"),
            ScopedDirectoryEntryKind::Directory,
        ),
    ]);
    let mut components = main.components();
    if !matches!(
        components.next(),
        Some(std::path::Component::Normal(root))
            if root == std::ffi::OsStr::new(content_directory)
    ) {
        return entries.len() == expected.len()
            && entries.iter().all(|entry| {
                expected
                    .get(entry.name())
                    .is_some_and(|kind| *kind == entry.kind())
            });
    }
    let Some(std::path::Component::Normal(child)) = components.next() else {
        return false;
    };
    let kind = if components.next().is_some() {
        ScopedDirectoryEntryKind::Directory
    } else {
        ScopedDirectoryEntryKind::File
    };
    if expected
        .insert(child.to_os_string(), kind)
        .is_some_and(|existing| existing != kind)
    {
        return false;
    }
    entries.len() == expected.len()
        && entries.iter().all(|entry| {
            expected
                .get(entry.name())
                .is_some_and(|kind| *kind == entry.kind())
        })
}

fn validate_single_directory(entries: &[ScopedDirectoryEntry], name: &str) -> bool {
    matches!(entries, [entry] if entry.name() == std::ffi::OsStr::new(name)
        && entry.kind() == ScopedDirectoryEntryKind::Directory)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rpg_maker::write_back::rewriter::RpgMakerRewrittenFile;
    use crate::runtime::filesystem::{
        DirectoryPublisherConfig, SystemFileSystem, SystemFileSystemConfig,
    };

    use crate::storage::file_system::{
        BoundScopedDirectory, DirectoryRecoveryError, DirectoryRecoveryOutcome,
        ScopedDirectoryEntry, ScopedDirectoryPath, ScopedDirectoryScope, StagedDirectory,
        StagingCleanupFailure,
    };

    type PrepareError = DirectoryPrepareError<FakeError>;
    type PublishResult = Result<(), DirectoryPublishError<FakeError>>;
    type PrepareCalls = Arc<Mutex<Vec<DirectoryStageRequest>>>;
    type PublishCalls = Arc<Mutex<Vec<PublishCall>>>;

    #[derive(Debug, Eq, PartialEq)]
    struct PublishCall {
        target_root: PathBuf,
        staging_root: PathBuf,
        mode: DirectoryPublishIntent,
    }

    #[derive(Clone)]
    struct FakeRecoverablePublisher {
        prepare_calls: Arc<Mutex<Vec<DirectoryStageRequest>>>,
        publish_calls: Arc<Mutex<Vec<PublishCall>>>,
        prepare_error: Arc<Mutex<Option<PrepareError>>>,
        publish_result: Arc<Mutex<Option<PublishResult>>>,
        discard_calls: Arc<Mutex<Vec<PathBuf>>>,
        discard_error: Arc<Mutex<Option<FakeError>>>,
    }

    impl RecoverableDirectoryPublisher for FakeRecoverablePublisher {
        type Error = FakeError;
        type StagingState = usize;

        async fn recover(
            &self,
            _target_root: PathBuf,
        ) -> Result<DirectoryRecoveryOutcome, DirectoryRecoveryError<Self::Error>> {
            Ok(DirectoryRecoveryOutcome::Unchanged)
        }

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, PrepareError> {
            let target_root = request.target_root().to_path_buf();
            let publish_intent = request.publish_intent();
            self.prepare_calls
                .lock()
                .expect("暂存调用锁不应中毒")
                .push(request);
            if let Some(error) = self
                .prepare_error
                .lock()
                .expect("暂存结果锁不应中毒")
                .take()
            {
                return Err(error);
            }
            let staging_root = target_root.with_extension("att-stage");
            Ok(StagedDirectory::new(
                target_root,
                staging_root,
                publish_intent,
                7,
            ))
        }

        async fn publish(&self, staged: StagedDirectory<Self::StagingState>) -> PublishResult {
            let mode = staged.publish_intent();
            self.publish_calls
                .lock()
                .expect("发布调用锁不应中毒")
                .push(PublishCall {
                    target_root: staged.target_root().to_path_buf(),
                    staging_root: staged.staging_root().to_path_buf(),
                    mode,
                });
            self.publish_result
                .lock()
                .expect("发布结果锁不应中毒")
                .take()
                .expect("测试发布结果只应消费一次")
        }

        async fn discard(
            &self,
            staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryDiscardError<Self::Error>> {
            let staging_root = staged.staging_root().to_path_buf();
            self.discard_calls
                .lock()
                .expect("丢弃调用锁不应中毒")
                .push(staging_root.clone());
            match self
                .discard_error
                .lock()
                .expect("丢弃结果锁不应中毒")
                .take()
            {
                Some(error) => Err(DirectoryDiscardError::new(staging_root, error)),
                None => Ok(()),
            }
        }
    }

    impl ScopedDirectoryEditor for FakeRecoverablePublisher {
        type CandidateState = usize;
        type ScopeState = ();
        type Error = FakeError;

        fn bind_scoped_directory(
            &self,
            candidate: &StagedDirectory<Self::CandidateState>,
            scope: ScopedDirectoryScope,
        ) -> impl std::future::Future<
            Output = Result<
                BoundScopedDirectory<Self::ScopeState>,
                ScopedDirectoryBindError<Self::Error>,
            >,
        > + Send
        + use<> {
            let root = candidate.staging_root().to_path_buf();
            std::future::ready(Ok(BoundScopedDirectory::new(root, scope, ())))
        }

        fn list_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn list_scoped_root(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
        ) -> impl std::future::Future<
            Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
        > + Send {
            std::future::ready(Ok(vec![
                ScopedDirectoryEntry::new("data".into(), ScopedDirectoryEntryKind::Directory),
                ScopedDirectoryEntry::new("js".into(), ScopedDirectoryEntryKind::Directory),
            ]))
        }

        fn create_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }

        fn write_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
            _bytes: Vec<u8>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
    struct MissingFileReader;

    impl FileReader for MissingFileReader {
        type Error = FakeError;

        fn read_file(
            &self,
            path: PathBuf,
        ) -> impl std::future::Future<
            Output = Result<crate::storage::file_system::ReadFile, ReadFileError<Self::Error>>,
        > + Send {
            std::future::ready(Err(ReadFileError::NotFound { path }))
        }
    }

    impl SnapshotFileReader for MissingFileReader {
        fn read_snapshot_file(
            &self,
            path: PathBuf,
        ) -> impl std::future::Future<
            Output = Result<crate::storage::file_system::ReadFile, ReadFileError<Self::Error>>,
        > + Send {
            self.read_file(path)
        }
    }

    #[derive(Clone, Default)]
    struct MemoryFileReader {
        files: Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>,
    }

    impl MemoryFileReader {
        fn new(files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>) -> Self {
            Self {
                files: Arc::new(Mutex::new(files.into_iter().collect())),
            }
        }

        fn replace(&self, path: PathBuf, bytes: Vec<u8>) {
            self.files
                .lock()
                .expect("内存文件锁不应中毒")
                .insert(path, bytes);
        }
    }

    impl FileReader for MemoryFileReader {
        type Error = FakeError;

        async fn read_file(
            &self,
            path: PathBuf,
        ) -> Result<crate::storage::file_system::ReadFile, ReadFileError<Self::Error>> {
            self.files
                .lock()
                .expect("内存文件锁不应中毒")
                .get(&path)
                .cloned()
                .map(|bytes| crate::storage::file_system::ReadFile::new(path.clone(), bytes))
                .ok_or(ReadFileError::NotFound { path })
        }
    }

    impl SnapshotFileReader for MemoryFileReader {
        async fn read_snapshot_file(
            &self,
            path: PathBuf,
        ) -> Result<crate::storage::file_system::ReadFile, ReadFileError<Self::Error>> {
            self.read_file(path).await
        }
    }

    impl DirectoryPublicationDiagnosticSource for FakeError {
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
            DirectoryPublicationDiagnostic::new(crate::diagnostic::PublicationBackendCause::new(
                Diagnostic::file_system(crate::diagnostic::FileSystemIssue::new(
                    crate::diagnostic::FileSystemDiagnosticContext::new(
                        crate::diagnostic::FileSystemDiagnosticStage::Publication,
                        operation,
                    ),
                    crate::diagnostic::FileSystemProblem::ExecutorClosed,
                )),
            ))
        }
    }
    #[test]
    fn invalid_stage_request_wire_preserves_output_and_both_conflicting_paths() {
        let error =
            RpgMakerWriteBackPublishingError::<SystemFileSystemError, FakeError>::InvalidRequest {
                output_root: PathBuf::from("D:/games/output"),
                source: DirectoryStageRequestError::OverlappingOverlays {
                    first: PathBuf::from("data/Items.json"),
                    second: PathBuf::from("data/Items.json/name"),
                },
            };

        assert_eq!(
            serde_json::to_value(error.diagnostic_report()).expect("诊断必须可序列化"),
            serde_json::json!({
                "effect": "unchanged",
                "primary": {
                    "code": "publication.request.overlapping_overlays",
                    "stage": "publication",
                    "issue": {
                        "family": "publication",
                        "details": {
                            "step": "prepare_candidate",
                            "problem": {
                                "kind": "invalid_request",
                                "output_root": "D:/games/output",
                                "violation": {
                                    "kind": "overlapping_overlays",
                                    "first": "data/Items.json",
                                    "second": "data/Items.json/name"
                                }
                            }
                        }
                    },
                    "resolution": "report_bug"
                },
                "related": []
            })
        );
    }

    #[test]
    fn publish_report_keeps_cleanup_as_related_failure_and_strongest_effect() {
        let error = RpgMakerWriteBackPublishingError::<SystemFileSystemError, FakeError>::Publish(
            DirectoryPublishError::NotPublished {
                target_root: PathBuf::from("D:/games/output"),
                source: FakeError("must-not-leak-primary"),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    PathBuf::from("D:/games/.output.stage"),
                    FakeError("must-not-leak-cleanup"),
                )),
            },
        );

        let report = error.diagnostic_report();
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.primary().code(), "publication.not_published");
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Cleanup
        );
        assert_eq!(
            report.related()[0].report().primary().code(),
            "publication.cleanup_failed"
        );
        let wire = serde_json::to_value(report).expect("发布报告必须可序列化");
        assert_eq!(
            wire.pointer("/primary/issue/details/problem/output_root"),
            Some(&serde_json::json!("D:/games/output"))
        );
        assert_eq!(
            wire.pointer("/related/0/report/primary/issue/details/problem/residual_path"),
            Some(&serde_json::json!("D:/games/.output.stage"))
        );
        assert!(!wire.to_string().contains("must-not-leak-primary"));
        assert!(!wire.to_string().contains("must-not-leak-cleanup"));
    }

    #[test]
    fn mz_candidate_root_owns_exact_data_and_js_structure() {
        let directory = |name: &str| {
            ScopedDirectoryEntry::new(name.into(), ScopedDirectoryEntryKind::Directory)
        };
        assert!(validate_data_and_js(&[directory("data"), directory("js"),]));
        for entries in [
            vec![directory("data")],
            vec![directory("data"), directory("js"), directory("other")],
            vec![directory("Data"), directory("js")],
            vec![
                ScopedDirectoryEntry::new("data".into(), ScopedDirectoryEntryKind::File),
                directory("js"),
            ],
        ] {
            assert!(!validate_data_and_js(&entries));
        }
    }

    fn project(name: &str, projects_root: &str) -> OpenedProject {
        let workspace_root = PathBuf::from(projects_root).join(name);
        OpenedProject::new(
            name.parse().expect("项目名应合法"),
            workspace_root.clone(),
            workspace_root.join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        )
    }

    fn project_with_layout(
        name: &str,
        projects_root: &str,
        layout: RpgMakerLayout,
    ) -> OpenedProject {
        let workspace_root = PathBuf::from(projects_root).join(name);
        OpenedProject::new_with_layout(
            name.parse().expect("项目名应合法"),
            workspace_root.clone(),
            workspace_root.join("project.db"),
            layout,
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        )
    }

    fn documents(project: &OpenedProject, files: Vec<(&str, &[u8])>) -> RpgMakerRewrittenDocuments {
        RpgMakerRewrittenDocuments::new(
            project.name().clone(),
            project.workspace_root().to_path_buf(),
            files
                .into_iter()
                .map(|(path, bytes)| {
                    RpgMakerRewrittenFile::new(PathBuf::from(path), bytes.to_vec())
                        .expect("测试候选文件应合法")
                })
                .collect(),
        )
        .expect("测试候选文档应合法")
    }

    fn documents_with_game_title(
        project: &OpenedProject,
        original: &str,
        candidate: &str,
        files: Vec<(&str, &[u8])>,
    ) -> RpgMakerRewrittenDocuments {
        documents(project, files).with_game_title_rewrite(original, candidate)
    }

    fn harness(
        prepare_error: Option<PrepareError>,
        result: PublishResult,
    ) -> (
        RpgMakerWriteBackPublishingService<MissingFileReader, FakeRecoverablePublisher>,
        PrepareCalls,
        PublishCalls,
    ) {
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publish_calls = Arc::new(Mutex::new(Vec::new()));
        (
            RpgMakerWriteBackPublishingService::new(
                MissingFileReader,
                FakeRecoverablePublisher {
                    prepare_calls: Arc::clone(&prepare_calls),
                    publish_calls: Arc::clone(&publish_calls),
                    prepare_error: Arc::new(Mutex::new(prepare_error)),
                    publish_result: Arc::new(Mutex::new(Some(result))),
                    discard_calls: Arc::new(Mutex::new(Vec::new())),
                    discard_error: Arc::new(Mutex::new(None)),
                },
            ),
            prepare_calls,
            publish_calls,
        )
    }

    #[tokio::test]
    async fn publishes_frozen_data_js_and_exact_candidate_overlays() {
        let project = project("alice", "C:/projects");
        let (publisher, prepare_calls, publish_calls) = harness(None, Ok(()));

        let candidate = publisher
            .prepare(
                &project,
                documents(
                    &project,
                    vec![("js/plugins.js", b"plugins"), ("data/Items.json", b"items")],
                ),
            )
            .await
            .expect("目录候选应该准备成功");
        assert_eq!(
            candidate.candidate_root(),
            Path::new("C:/projects/alice/write_back.att-stage")
        );
        let published = publisher
            .publish(candidate)
            .await
            .expect("目录候选应该发布成功");
        assert_eq!(published.output_root(), project.write_back_root());

        let calls = prepare_calls.lock().expect("暂存调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        let request = &calls[0];
        assert_eq!(
            request.target_root(),
            Path::new("C:/projects/alice/write_back")
        );
        assert_eq!(request.source_mappings().len(), 2);
        assert_eq!(
            request.source_mappings()[0].source_directory(),
            Path::new("C:/projects/alice/source/data")
        );
        assert_eq!(
            request.source_mappings()[0].relative_target(),
            Path::new("data")
        );
        assert_eq!(
            request.source_mappings()[1].source_directory(),
            Path::new("C:/projects/alice/source/js")
        );
        assert_eq!(request.overlays().len(), 2);
        assert_eq!(
            request.overlays()[0].relative_file(),
            Path::new("data/Items.json")
        );
        assert_eq!(request.overlays()[0].bytes(), b"items");
        assert_eq!(
            request.overlays()[1].relative_file(),
            Path::new("js/plugins.js")
        );
        assert!(request.empty_directories().is_empty());
        let publish_calls = publish_calls.lock().expect("发布调用锁不应中毒");
        assert_eq!(publish_calls.len(), 1);
        assert_eq!(
            publish_calls[0].mode,
            DirectoryPublishIntent::ReplaceExisting
        );
        assert_eq!(publish_calls[0].target_root, project.write_back_root());
    }

    #[tokio::test]
    async fn mv_and_mz_sync_standard_bootstrap_titles_from_the_system_unit() {
        for (name, layout, main) in [
            ("mz-title", RpgMakerLayout::MZ, "index.html"),
            ("mv-title", RpgMakerLayout::MV, "www/index.html"),
        ] {
            let project = project_with_layout(name, "C:/projects", layout);
            let package = format!(
                r#"{{"name":"game","main":"{main}","window":{{"title":"原题","width":816}}}}"#
            );
            let html = b"<head><title>\xe5\x8e\x9f\xe9\xa2\x98</title></head>".to_vec();
            let reader = MemoryFileReader::new([
                (
                    project.layout().source_root().join("package.json"),
                    package.into_bytes(),
                ),
                (project.layout().source_root().join(main), html),
            ]);
            let bootstrap = read_optional_bootstrap_files(&reader, project.layout().source_root())
                .await
                .expect("测试启动壳读取不应失败")
                .expect("测试启动壳应存在");
            let project = project.with_verified_bootstrap(bootstrap);
            let prepare_calls = Arc::new(Mutex::new(Vec::new()));
            let publisher = RpgMakerWriteBackPublishingService::new(
                reader,
                FakeRecoverablePublisher {
                    prepare_calls: Arc::clone(&prepare_calls),
                    publish_calls: Arc::new(Mutex::new(Vec::new())),
                    prepare_error: Arc::new(Mutex::new(None)),
                    publish_result: Arc::new(Mutex::new(Some(Ok(())))),
                    discard_calls: Arc::new(Mutex::new(Vec::new())),
                    discard_error: Arc::new(Mutex::new(None)),
                },
            );

            let candidate = publisher
                .prepare(
                    &project,
                    documents_with_game_title(
                        &project,
                        "原题",
                        "译题",
                        vec![(
                            "data/System.json",
                            r#"{"gameTitle":"译题","currencyUnit":"G"}"#.as_bytes(),
                        )],
                    ),
                )
                .await
                .expect("标准启动壳候选应准备成功");

            {
                let calls = prepare_calls.lock().expect("暂存调用锁不应中毒");
                let request = calls.last().expect("应准备一个目录候选");
                assert_eq!(request.source_mappings().len(), 2, "{name}");
                assert_eq!(
                    request.source_mappings()[0].source_directory(),
                    project.layout().source_data(),
                    "{name}"
                );
                assert_eq!(
                    request.source_mappings()[0].relative_target(),
                    layout.data_relative(),
                    "{name}"
                );
                assert_eq!(
                    request.source_mappings()[1].source_directory(),
                    project.layout().source_js(),
                    "{name}"
                );
                assert_eq!(
                    request.source_mappings()[1].relative_target(),
                    layout.js_relative(),
                    "{name}"
                );
                let overlay = |path: &Path| {
                    request
                        .overlays()
                        .iter()
                        .find(|overlay| overlay.relative_file() == path)
                        .unwrap_or_else(|| panic!("{name} 应包含 {} 覆盖", path.display()))
                        .bytes()
                };
                assert_eq!(
                    overlay(Path::new("package.json")),
                    format!(
                        r#"{{"name":"game","main":"{main}","window":{{"title":"译题","width":816}}}}"#
                    )
                    .as_bytes(),
                    "{name}"
                );
                assert_eq!(
                    overlay(Path::new(main)),
                    b"<head><title>\xe8\xaf\x91\xe9\xa2\x98</title></head>",
                    "{name}"
                );
                assert_eq!(
                    overlay(&layout.map_content_relative(Path::new("data/System.json"))),
                    r#"{"gameTitle":"译题","currencyUnit":"G"}"#.as_bytes(),
                    "{name}"
                );
            }
            publisher
                .discard(candidate)
                .await
                .expect("测试候选应可丢弃");
        }
    }

    #[tokio::test]
    async fn bootstrap_changed_after_open_uses_verified_snapshot_and_fails_validation() {
        let project = project("bootstrap-change", "C:/projects");
        let source_root = project.layout().source_root().to_path_buf();
        let reader = MemoryFileReader::new([
            (
                source_root.join("package.json"),
                br#"{"main":"index.html","window":{"title":"demo"}}"#.to_vec(),
            ),
            (
                source_root.join("index.html"),
                b"<title>demo</title>".to_vec(),
            ),
            (
                project.layout().source_data().join("System.json"),
                br#"{"gameTitle":"demo"}"#.to_vec(),
            ),
        ]);
        let bootstrap = read_optional_bootstrap_files(&reader, &source_root)
            .await
            .expect("测试启动壳读取不应失败")
            .expect("测试启动壳应存在");
        let project = project.with_verified_bootstrap(bootstrap);
        reader.replace(
            source_root.join("package.json"),
            br#"{"main":"index.html","window":{"title":"changed"}}"#.to_vec(),
        );
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publisher = RpgMakerWriteBackPublishingService::new(
            reader.clone(),
            FakeRecoverablePublisher {
                prepare_calls: Arc::clone(&prepare_calls),
                publish_calls: Arc::new(Mutex::new(Vec::new())),
                prepare_error: Arc::new(Mutex::new(None)),
                publish_result: Arc::new(Mutex::new(Some(Ok(())))),
                discard_calls: Arc::new(Mutex::new(Vec::new())),
                discard_error: Arc::new(Mutex::new(None)),
            },
        );
        let candidate = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect("应先准备启动壳候选");
        {
            let calls = prepare_calls.lock().expect("暂存调用锁不应中毒");
            let request = calls.last().expect("应准备一个目录候选");
            let package = request
                .overlays()
                .iter()
                .find(|overlay| overlay.relative_file() == Path::new("package.json"))
                .expect("候选应包含已验证 package");
            assert_eq!(
                package.bytes(),
                br#"{"main":"index.html","window":{"title":"demo"}}"#
            );
        }

        let error = publisher
            .validate(&candidate)
            .await
            .expect_err("来源启动壳变化必须阻止发布");
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::SourceChanged { source_root: changed }
                if changed == source_root
        ));
        publisher
            .discard(candidate)
            .await
            .expect("变化测试候选应可丢弃");
    }

    #[tokio::test]
    async fn real_candidate_validation_rejects_package_or_html_byte_changes() {
        let temporary = tempfile::tempdir().expect("应建立真实候选临时目录");
        let workspace = temporary.path().join("project");
        for directory in ["source/data", "source/js", "write_back"] {
            fs::create_dir_all(workspace.join(directory)).expect("应建立项目目录");
        }
        fs::write(
            workspace.join("source/data/System.json"),
            r#"{"gameTitle":"source"}"#,
        )
        .expect("应建立冻结 System");
        fs::write(workspace.join("source/js/plugins.js"), b"plugins").expect("应建立冻结 JS");
        fs::write(
            workspace.join("source/package.json"),
            r#"{"main":"index.html","window":{"title":"source"}}"#,
        )
        .expect("应建立冻结 package");
        fs::write(
            workspace.join("source/index.html"),
            b"<title>source</title>",
        )
        .expect("应建立冻结 HTML");
        let project = OpenedProject::new(
            "real-bootstrap".parse().expect("项目名应合法"),
            workspace.clone(),
            workspace.join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        );
        let file_system =
            SystemFileSystem::new(SystemFileSystemConfig::production()).expect("应建立文件系统根");
        let bootstrap = read_optional_bootstrap_files(&file_system, project.layout().source_root())
            .await
            .expect("真实启动壳读取不应失败")
            .expect("真实启动壳应存在");
        let project = project.with_verified_bootstrap(bootstrap);
        let directory_publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(temporary.path().join("locks"))
                .expect("发布配置应合法"),
        );

        for (relative, changed) in [
            (
                "package.json",
                br#"{"main":"index.html","window":{"title":"tampered"}}"#.as_slice(),
            ),
            ("index.html", b"<title>tampered</title>".as_slice()),
        ] {
            let service = RpgMakerWriteBackPublishingService::new(
                file_system.clone(),
                directory_publisher.clone(),
            );
            let candidate = service
                .prepare(
                    &project,
                    documents_with_game_title(
                        &project,
                        "source",
                        "translated",
                        vec![(
                            "data/System.json",
                            r#"{"gameTitle":"translated"}"#.as_bytes(),
                        )],
                    ),
                )
                .await
                .expect("应准备真实写回候选");
            fs::write(candidate.candidate_root().join(relative), changed)
                .expect("应篡改候选启动壳");

            let error = service
                .validate(&candidate)
                .await
                .expect_err("候选启动壳任一字节变化都必须拒绝");
            assert!(matches!(
                error,
                RpgMakerWriteBackPublishingError::CandidateBootstrapChanged { .. }
            ));
            service.discard(candidate).await.expect("篡改候选应可丢弃");
        }
        file_system.shutdown().await.expect("文件系统根应终结");
    }

    #[tokio::test]
    async fn prepared_candidate_can_be_explicitly_discarded_without_publishing() {
        let project = project("alice", "C:/projects");
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publish_calls = Arc::new(Mutex::new(Vec::new()));
        let discard_calls = Arc::new(Mutex::new(Vec::new()));
        let publisher = RpgMakerWriteBackPublishingService::new(
            MissingFileReader,
            FakeRecoverablePublisher {
                prepare_calls,
                publish_calls: Arc::clone(&publish_calls),
                prepare_error: Arc::new(Mutex::new(None)),
                publish_result: Arc::new(Mutex::new(Some(Ok(())))),
                discard_calls: Arc::clone(&discard_calls),
                discard_error: Arc::new(Mutex::new(None)),
            },
        );

        let candidate = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect("候选应准备成功");
        publisher
            .discard(candidate)
            .await
            .expect("候选应只丢弃一次");

        assert!(publish_calls.lock().expect("发布调用锁不应中毒").is_empty());
        assert_eq!(discard_calls.lock().expect("丢弃调用锁不应中毒").len(), 1);
    }

    #[tokio::test]
    async fn discard_failure_preserves_the_exact_staging_root() {
        let project = project("alice", "C:/projects");
        let publisher = RpgMakerWriteBackPublishingService::new(
            MissingFileReader,
            FakeRecoverablePublisher {
                prepare_calls: Arc::new(Mutex::new(Vec::new())),
                publish_calls: Arc::new(Mutex::new(Vec::new())),
                prepare_error: Arc::new(Mutex::new(None)),
                publish_result: Arc::new(Mutex::new(Some(Ok(())))),
                discard_calls: Arc::new(Mutex::new(Vec::new())),
                discard_error: Arc::new(Mutex::new(Some(FakeError("cleanup")))),
            },
        );
        let candidate = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect("候选应准备成功");

        let error = publisher
            .discard(candidate)
            .await
            .expect_err("根清理失败必须传播");

        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Discard(source)
                if source.staging_root()
                    == Path::new("C:/projects/alice/write_back.att-stage")
                    && *source.source() == FakeError("cleanup")
        ));
    }

    #[tokio::test]
    async fn empty_candidate_still_publishes_complete_frozen_subtrees() {
        let project = project("alice", "C:/projects");
        let (publisher, calls, publish_calls) = harness(None, Ok(()));

        let candidate = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect("空候选仍应准备完整副本");
        publisher.publish(candidate).await.expect("空候选仍应发布");

        let calls = calls.lock().expect("发布调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].source_mappings().len(), 2);
        assert!(calls[0].overlays().is_empty());
        assert_eq!(
            publish_calls.lock().expect("发布调用锁不应中毒")[0].mode,
            DirectoryPublishIntent::ReplaceExisting
        );
    }

    #[tokio::test]
    async fn rejects_candidate_from_same_named_project_in_another_workspace() {
        let current_project = project("alice", "C:/projects");
        let other = project("alice", "D:/other-projects");
        let (publisher, calls, publish_calls) = harness(None, Ok(()));

        let error = publisher
            .prepare(&current_project, documents(&other, Vec::new()))
            .await
            .expect_err("跨工作区候选必须拒绝");

        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::CandidateProjectMismatch { .. }
        ));
        assert!(calls.lock().expect("发布调用锁不应中毒").is_empty());
        assert!(publish_calls.lock().expect("发布调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn prepare_failure_stops_before_publish() {
        let project = project("alice", "C:/projects");
        let target_root = project.write_back_root().to_path_buf();
        let (publisher, prepare_calls, publish_calls) = harness(
            Some(DirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: FakeError("copy"),
                cleanup_failure: None,
            }),
            Ok(()),
        );
        let error = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect_err("暂存失败必须传播");
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Prepare(DirectoryPrepareError::NotPrepared {
                target_root: failed_target,
                source: FakeError("copy"),
                cleanup_failure: None,
            }) if failed_target == target_root
        ));
        assert_eq!(prepare_calls.lock().expect("暂存调用锁不应中毒").len(), 1);
        assert!(publish_calls.lock().expect("发布调用锁不应中毒").is_empty());
    }

    async fn assert_publish_error(
        root_error: DirectoryPublishError<FakeError>,
    ) -> (
        WriteBackPublishFailureState,
        RpgMakerWriteBackPublishingError<FakeError, FakeError>,
    ) {
        let project = project("alice", "C:/projects");
        let (publisher, _, publish_calls) = harness(None, Err(root_error));
        let candidate = publisher
            .prepare(&project, documents(&project, Vec::new()))
            .await
            .expect("发布错误测试应先准备候选");
        let error = publisher
            .publish(candidate)
            .await
            .expect_err("根发布失败必须传播");
        assert_eq!(
            publish_calls.lock().expect("发布调用锁不应中毒")[0].mode,
            DirectoryPublishIntent::ReplaceExisting
        );
        error.into_parts()
    }

    #[tokio::test]
    async fn preserves_replace_target_missing_and_not_directory_states() {
        let target_root = PathBuf::from("C:/projects/alice/write_back");
        let (state, error) = assert_publish_error(DirectoryPublishError::TargetMissing {
            target_root: target_root.clone(),
            cleanup_failure: None,
        })
        .await;
        assert_eq!(
            state,
            WriteBackPublishFailureState::NotPublished {
                output_root: target_root.clone(),
                residual_paths: Vec::new(),
            }
        );
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Publish(
                DirectoryPublishError::TargetMissing {
                    target_root: failed_target,
                    cleanup_failure: None,
                }
            ) if failed_target == target_root
        ));

        let (state, error) = assert_publish_error(DirectoryPublishError::TargetNotDirectory {
            target_root: target_root.clone(),
            cleanup_failure: None,
        })
        .await;
        assert_eq!(
            state,
            WriteBackPublishFailureState::NotPublished {
                output_root: target_root.clone(),
                residual_paths: Vec::new(),
            }
        );
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Publish(
                DirectoryPublishError::TargetNotDirectory {
                    target_root: failed_target,
                    cleanup_failure: None,
                }
            ) if failed_target == target_root
        ));
    }

    #[tokio::test]
    async fn preserves_known_not_published_state_and_candidate_cleanup_failure() {
        let target_root = PathBuf::from("C:/projects/alice/write_back");
        let residual_path = PathBuf::from("C:/projects/alice/.write_back-stage");
        let (state, error) = assert_publish_error(DirectoryPublishError::NotPublished {
            target_root: target_root.clone(),
            source: FakeError("replace"),
            cleanup_failure: Some(StagingCleanupFailure::new(
                residual_path.clone(),
                FakeError("cleanup"),
            )),
        })
        .await;
        assert_eq!(
            state,
            WriteBackPublishFailureState::NotPublished {
                output_root: target_root.clone(),
                residual_paths: vec![residual_path.clone()],
            }
        );

        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Publish(
                DirectoryPublishError::NotPublished {
                    target_root: failed_target,
                    source: FakeError("replace"),
                    cleanup_failure: Some(cleanup_failure),
                }
            ) if failed_target == target_root
                && cleanup_failure.residual_path() == residual_path
                && *cleanup_failure.source() == FakeError("cleanup")
        ));
    }

    #[tokio::test]
    async fn preserves_published_cleanup_failure_and_outcome_unknown_states() {
        let target_root = PathBuf::from("C:/projects/alice/write_back");
        let residual_path = PathBuf::from("C:/projects/alice/.write_back-old");
        let (state, error) = assert_publish_error(DirectoryPublishError::PublishedWithResiduals {
            target_root: target_root.clone(),
            residual_path: residual_path.clone(),
            source: FakeError("cleanup"),
        })
        .await;
        assert_eq!(
            state,
            WriteBackPublishFailureState::PublishedWithResiduals {
                output_root: target_root.clone(),
                residual_paths: vec![residual_path.clone()],
            }
        );
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Publish(
                DirectoryPublishError::PublishedWithResiduals {
                    target_root: failed_target,
                    residual_path: residual,
                    source: FakeError("cleanup"),
                }
            ) if failed_target == target_root && residual == residual_path
        ));

        let recovery_artifacts = vec![PathBuf::from("C:/projects/alice/.write_back-recovery")];
        let (state, error) = assert_publish_error(DirectoryPublishError::OutcomeUnknown {
            target_root: target_root.clone(),
            recovery_artifacts: recovery_artifacts.clone(),
            source: FakeError("restore"),
        })
        .await;
        assert_eq!(
            state,
            WriteBackPublishFailureState::OutcomeUnknown {
                output_root: target_root.clone(),
                recovery_artifacts: recovery_artifacts.clone(),
            }
        );
        assert!(matches!(
            error,
            RpgMakerWriteBackPublishingError::Publish(
                DirectoryPublishError::OutcomeUnknown {
                    target_root: failed_target,
                    recovery_artifacts: artifacts,
                    source: FakeError("restore"),
                }
            ) if failed_target == target_root && artifacts == recovery_artifacts
        ));
    }

    #[test]
    fn preparing_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let project = project("alice", "C:/projects");
        let candidate = documents(&project, Vec::new());
        let (publisher, _, _) = harness(None, Ok(()));
        assert_send(publisher.prepare(&project, candidate));
    }
}
