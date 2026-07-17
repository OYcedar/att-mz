#![allow(dead_code, reason = "Standard WriteBack 尚未接入生产组合根")]

//! 把 Rewriter 候选提交给原子目录候选发布根。

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::att_mz::ProjectName;
use crate::att_mz::project::OpenedProject;
use crate::storage::file_system::{
    AtomicDirectoryPrepareError, AtomicDirectoryPublishError, AtomicDirectoryPublisher,
    DirectoryFileOverlay, DirectoryPublishMode, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError,
};

use super::rewriter::MzRewrittenDocuments;
use super::standard::StandardWriteBackPublisher;

/// 用固定 `source/data`、`source/js` 基底发布 Standard 写回候选。
pub(crate) struct StandardWriteBackPublishingService<A> {
    atomic_publisher: A,
}

impl<A> StandardWriteBackPublishingService<A> {
    pub(crate) const fn new(atomic_publisher: A) -> Self {
        Self { atomic_publisher }
    }
}

impl<A> StandardWriteBackPublisher<MzRewrittenDocuments> for StandardWriteBackPublishingService<A>
where
    A: AtomicDirectoryPublisher,
{
    type Error = StandardWriteBackPublishingError<A::Error>;

    async fn publish(
        &self,
        project: &OpenedProject,
        documents: MzRewrittenDocuments,
    ) -> Result<(), Self::Error> {
        if documents.project_name() != project.name()
            || documents.workspace_root() != project.workspace_root()
        {
            return Err(StandardWriteBackPublishingError::CandidateProjectMismatch {
                expected_name: project.name().clone(),
                expected_workspace_root: project.workspace_root().to_path_buf(),
                candidate_name: documents.project_name().clone(),
                candidate_workspace_root: documents.workspace_root().to_path_buf(),
            });
        }

        let source_mappings = vec![
            DirectorySourceMapping::new(
                project.layout().source_data().to_path_buf(),
                PathBuf::from("data"),
            )
            .map_err(StandardWriteBackPublishingError::InvalidRequest)?,
            DirectorySourceMapping::new(
                project.layout().source_js().to_path_buf(),
                PathBuf::from("js"),
            )
            .map_err(StandardWriteBackPublishingError::InvalidRequest)?,
        ];
        let overlays = documents
            .into_files()
            .into_iter()
            .map(|file| {
                let (relative_path, bytes) = file.into_parts();
                DirectoryFileOverlay::new(relative_path, bytes)
                    .map_err(StandardWriteBackPublishingError::InvalidRequest)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = DirectoryStageRequest::new(
            project.layout().write_back_root().to_path_buf(),
            source_mappings,
            overlays,
            Vec::new(),
        )
        .map_err(StandardWriteBackPublishingError::InvalidRequest)?;

        let staged = self
            .atomic_publisher
            .prepare(request)
            .await
            .map_err(StandardWriteBackPublishingError::Prepare)?;
        self.atomic_publisher
            .publish(staged, DirectoryPublishMode::Replace)
            .await
            .map_err(StandardWriteBackPublishingError::Publish)
    }
}

/// Standard Publisher 在候选交接、请求建立或根发布阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum StandardWriteBackPublishingError<E> {
    CandidateProjectMismatch {
        expected_name: ProjectName,
        expected_workspace_root: PathBuf,
        candidate_name: ProjectName,
        candidate_workspace_root: PathBuf,
    },
    InvalidRequest(DirectoryStageRequestError),
    Prepare(AtomicDirectoryPrepareError<E>),
    Publish(AtomicDirectoryPublishError<E>),
}

impl<E> fmt::Display for StandardWriteBackPublishingError<E>
where
    E: fmt::Display,
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
            Self::InvalidRequest(source) => write!(formatter, "写回候选请求无效：{source}"),
            Self::Prepare(source) => source.fmt(formatter),
            Self::Publish(source) => source.fmt(formatter),
        }
    }
}

impl<E> Error for StandardWriteBackPublishingError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CandidateProjectMismatch { .. } => None,
            Self::InvalidRequest(source) => Some(source),
            Self::Prepare(source) => Some(source),
            Self::Publish(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::write_back::rewriter::MzRewrittenFile;

    use crate::storage::file_system::{
        AtomicDirectoryDiscardError, StagedDirectory, StagingCleanupFailure,
    };

    type PrepareError = AtomicDirectoryPrepareError<FakeError>;
    type PublishResult = Result<(), AtomicDirectoryPublishError<FakeError>>;
    type PrepareCalls = Arc<Mutex<Vec<DirectoryStageRequest>>>;
    type PublishCalls = Arc<Mutex<Vec<PublishCall>>>;

    #[derive(Debug, Eq, PartialEq)]
    struct PublishCall {
        target_root: PathBuf,
        staging_root: PathBuf,
        mode: DirectoryPublishMode,
    }

    #[derive(Clone)]
    struct FakeAtomicPublisher {
        prepare_calls: Arc<Mutex<Vec<DirectoryStageRequest>>>,
        publish_calls: Arc<Mutex<Vec<PublishCall>>>,
        prepare_error: Arc<Mutex<Option<PrepareError>>>,
        publish_result: Arc<Mutex<Option<PublishResult>>>,
    }

    impl AtomicDirectoryPublisher for FakeAtomicPublisher {
        type Error = FakeError;
        type StagingState = usize;

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, PrepareError> {
            let target_root = request.target_root().to_path_buf();
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
            Ok(StagedDirectory::new(target_root, staging_root, 7))
        }

        async fn publish(
            &self,
            staged: StagedDirectory<Self::StagingState>,
            mode: DirectoryPublishMode,
        ) -> PublishResult {
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
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), AtomicDirectoryDiscardError<Self::Error>> {
            panic!("Standard WriteBack 不应显式丢弃已暂存候选")
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

    fn project(name: &str, projects_root: &str) -> OpenedProject {
        let workspace_root = PathBuf::from(projects_root).join(name);
        OpenedProject::new(
            name.parse().expect("项目名应合法"),
            workspace_root.clone(),
            workspace_root.join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn documents(project: &OpenedProject, files: Vec<(&str, &[u8])>) -> MzRewrittenDocuments {
        MzRewrittenDocuments::new(
            project.name().clone(),
            project.workspace_root().to_path_buf(),
            files
                .into_iter()
                .map(|(path, bytes)| {
                    MzRewrittenFile::new(PathBuf::from(path), bytes.to_vec())
                        .expect("测试候选文件应合法")
                })
                .collect(),
        )
        .expect("测试候选文档应合法")
    }

    fn harness(
        prepare_error: Option<PrepareError>,
        result: PublishResult,
    ) -> (
        StandardWriteBackPublishingService<FakeAtomicPublisher>,
        PrepareCalls,
        PublishCalls,
    ) {
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publish_calls = Arc::new(Mutex::new(Vec::new()));
        (
            StandardWriteBackPublishingService::new(FakeAtomicPublisher {
                prepare_calls: Arc::clone(&prepare_calls),
                publish_calls: Arc::clone(&publish_calls),
                prepare_error: Arc::new(Mutex::new(prepare_error)),
                publish_result: Arc::new(Mutex::new(Some(result))),
            }),
            prepare_calls,
            publish_calls,
        )
    }

    #[tokio::test]
    async fn publishes_frozen_data_js_and_exact_candidate_overlays() {
        let project = project("alice", "C:/projects");
        let (publisher, prepare_calls, publish_calls) = harness(None, Ok(()));

        publisher
            .publish(
                &project,
                documents(
                    &project,
                    vec![("data/Items.json", b"items"), ("js/plugins.js", b"plugins")],
                ),
            )
            .await
            .expect("目录候选应该发布成功");

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
        assert_eq!(publish_calls[0].mode, DirectoryPublishMode::Replace);
        assert_eq!(publish_calls[0].target_root, project.write_back_root());
    }

    #[tokio::test]
    async fn empty_candidate_still_publishes_complete_frozen_subtrees() {
        let project = project("alice", "C:/projects");
        let (publisher, calls, publish_calls) = harness(None, Ok(()));

        publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect("空候选仍应发布完整副本");

        let calls = calls.lock().expect("发布调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].source_mappings().len(), 2);
        assert!(calls[0].overlays().is_empty());
        assert_eq!(
            publish_calls.lock().expect("发布调用锁不应中毒")[0].mode,
            DirectoryPublishMode::Replace
        );
    }

    #[tokio::test]
    async fn rejects_candidate_from_same_named_project_in_another_workspace() {
        let current_project = project("alice", "C:/projects");
        let other = project("alice", "D:/other-projects");
        let (publisher, calls, publish_calls) = harness(None, Ok(()));

        let error = publisher
            .publish(&current_project, documents(&other, Vec::new()))
            .await
            .expect_err("跨工作区候选必须拒绝");

        assert!(matches!(
            error,
            StandardWriteBackPublishingError::CandidateProjectMismatch { .. }
        ));
        assert!(calls.lock().expect("发布调用锁不应中毒").is_empty());
        assert!(publish_calls.lock().expect("发布调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn prepare_failure_stops_before_publish() {
        let project = project("alice", "C:/projects");
        let target_root = project.write_back_root().to_path_buf();
        let (publisher, prepare_calls, publish_calls) = harness(
            Some(AtomicDirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: FakeError("copy"),
                cleanup_failure: None,
            }),
            Ok(()),
        );
        let error = publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect_err("暂存失败必须传播");
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Prepare(AtomicDirectoryPrepareError::NotPrepared {
                target_root: failed_target,
                source: FakeError("copy"),
                cleanup_failure: None,
            }) if failed_target == target_root
        ));
        assert_eq!(prepare_calls.lock().expect("暂存调用锁不应中毒").len(), 1);
        assert!(publish_calls.lock().expect("发布调用锁不应中毒").is_empty());
    }

    async fn assert_publish_error(
        root_error: AtomicDirectoryPublishError<FakeError>,
    ) -> StandardWriteBackPublishingError<FakeError> {
        let project = project("alice", "C:/projects");
        let (publisher, _, publish_calls) = harness(None, Err(root_error));
        let error = publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect_err("根发布失败必须传播");
        assert_eq!(
            publish_calls.lock().expect("发布调用锁不应中毒")[0].mode,
            DirectoryPublishMode::Replace
        );
        error
    }

    #[tokio::test]
    async fn preserves_replace_target_missing_and_not_directory_states() {
        let target_root = PathBuf::from("C:/projects/alice/write_back");
        let error = assert_publish_error(AtomicDirectoryPublishError::TargetMissing {
            target_root: target_root.clone(),
            cleanup_failure: None,
        })
        .await;
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectoryPublishError::TargetMissing {
                    target_root: failed_target,
                    cleanup_failure: None,
                }
            ) if failed_target == target_root
        ));

        let error = assert_publish_error(AtomicDirectoryPublishError::TargetNotDirectory {
            target_root: target_root.clone(),
            cleanup_failure: None,
        })
        .await;
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectoryPublishError::TargetNotDirectory {
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
        let error = assert_publish_error(AtomicDirectoryPublishError::NotPublished {
            target_root: target_root.clone(),
            source: FakeError("replace"),
            cleanup_failure: Some(StagingCleanupFailure::new(
                residual_path.clone(),
                FakeError("cleanup"),
            )),
        })
        .await;

        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectoryPublishError::NotPublished {
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
        let error = assert_publish_error(AtomicDirectoryPublishError::PublishedButCleanupFailed {
            target_root: target_root.clone(),
            residual_path: residual_path.clone(),
            source: FakeError("cleanup"),
        })
        .await;
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectoryPublishError::PublishedButCleanupFailed {
                    target_root: failed_target,
                    residual_path: residual,
                    source: FakeError("cleanup"),
                }
            ) if failed_target == target_root && residual == residual_path
        ));

        let recovery_artifacts = vec![PathBuf::from("C:/projects/alice/.write_back-recovery")];
        let error = assert_publish_error(AtomicDirectoryPublishError::OutcomeUnknown {
            target_root: target_root.clone(),
            recovery_artifacts: recovery_artifacts.clone(),
            source: FakeError("restore"),
        })
        .await;
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectoryPublishError::OutcomeUnknown {
                    target_root: failed_target,
                    recovery_artifacts: artifacts,
                    source: FakeError("restore"),
                }
            ) if failed_target == target_root && artifacts == recovery_artifacts
        ));
    }

    #[test]
    fn publishing_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let project = project("alice", "C:/projects");
        let candidate = documents(&project, Vec::new());
        let (publisher, _, _) = harness(None, Ok(()));
        assert_send(publisher.publish(&project, candidate));
    }
}
