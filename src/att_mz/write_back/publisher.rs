#![allow(dead_code, reason = "Standard WriteBack 尚未接入生产组合根")]

//! 把 Rewriter 候选提交给原子目录快照发布根。

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::att_mz::ProjectName;
use crate::att_mz::project::OpenedProject;
use crate::storage::file_system::{
    AtomicDirectorySnapshotPublishError, AtomicDirectorySnapshotPublisher,
    DirectorySnapshotFileOverlay, DirectorySnapshotPublishRequest,
    DirectorySnapshotPublishRequestError, DirectorySnapshotSourceMapping,
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
    A: AtomicDirectorySnapshotPublisher,
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
            DirectorySnapshotSourceMapping::new(
                project.source_root().join("data"),
                PathBuf::from("data"),
            )
            .map_err(StandardWriteBackPublishingError::InvalidRequest)?,
            DirectorySnapshotSourceMapping::new(
                project.source_root().join("js"),
                PathBuf::from("js"),
            )
            .map_err(StandardWriteBackPublishingError::InvalidRequest)?,
        ];
        let overlays = documents
            .into_files()
            .into_iter()
            .map(|file| {
                let (relative_path, bytes) = file.into_parts();
                DirectorySnapshotFileOverlay::new(relative_path, bytes)
                    .map_err(StandardWriteBackPublishingError::InvalidRequest)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = DirectorySnapshotPublishRequest::new(
            project.write_back_root().to_path_buf(),
            source_mappings,
            overlays,
        )
        .map_err(StandardWriteBackPublishingError::InvalidRequest)?;

        self.atomic_publisher
            .publish_snapshot(request)
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
    InvalidRequest(DirectorySnapshotPublishRequestError),
    Publish(AtomicDirectorySnapshotPublishError<E>),
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
            Self::InvalidRequest(source) => write!(formatter, "写回快照请求无效：{source}"),
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

    type PublishResult = Result<(), AtomicDirectorySnapshotPublishError<FakeError>>;

    #[derive(Clone)]
    struct FakeAtomicPublisher {
        calls: Arc<Mutex<Vec<DirectorySnapshotPublishRequest>>>,
        result: Arc<Mutex<Option<PublishResult>>>,
    }

    impl AtomicDirectorySnapshotPublisher for FakeAtomicPublisher {
        type Error = FakeError;

        async fn publish_snapshot(
            &self,
            request: DirectorySnapshotPublishRequest,
        ) -> PublishResult {
            self.calls.lock().expect("发布调用锁不应中毒").push(request);
            self.result
                .lock()
                .expect("发布结果锁不应中毒")
                .take()
                .expect("测试发布结果只应消费一次")
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
        result: PublishResult,
    ) -> (
        StandardWriteBackPublishingService<FakeAtomicPublisher>,
        Arc<Mutex<Vec<DirectorySnapshotPublishRequest>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            StandardWriteBackPublishingService::new(FakeAtomicPublisher {
                calls: Arc::clone(&calls),
                result: Arc::new(Mutex::new(Some(result))),
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn publishes_frozen_data_js_and_exact_candidate_overlays() {
        let project = project("alice", "C:/projects");
        let (publisher, calls) = harness(Ok(()));

        publisher
            .publish(
                &project,
                documents(
                    &project,
                    vec![("data/Items.json", b"items"), ("js/plugins.js", b"plugins")],
                ),
            )
            .await
            .expect("目录快照应该发布成功");

        let calls = calls.lock().expect("发布调用锁不应中毒");
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
    }

    #[tokio::test]
    async fn empty_candidate_still_publishes_complete_frozen_subtrees() {
        let project = project("alice", "C:/projects");
        let (publisher, calls) = harness(Ok(()));

        publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect("空候选仍应发布完整副本");

        let calls = calls.lock().expect("发布调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].source_mappings().len(), 2);
        assert!(calls[0].overlays().is_empty());
    }

    #[tokio::test]
    async fn rejects_candidate_from_same_named_project_in_another_workspace() {
        let current_project = project("alice", "C:/projects");
        let other = project("alice", "D:/other-projects");
        let (publisher, calls) = harness(Ok(()));

        let error = publisher
            .publish(&current_project, documents(&other, Vec::new()))
            .await
            .expect_err("跨工作区候选必须拒绝");

        assert!(matches!(
            error,
            StandardWriteBackPublishingError::CandidateProjectMismatch { .. }
        ));
        assert!(calls.lock().expect("发布调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn preserves_not_published_and_outcome_unknown_root_states() {
        let project = project("alice", "C:/projects");
        let (publisher, _) = harness(Err(AtomicDirectorySnapshotPublishError::NotPublished {
            source: FakeError("copy"),
        }));
        let error = publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect_err("未发布终态必须传播");
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectorySnapshotPublishError::NotPublished {
                    source: FakeError("copy")
                }
            )
        ));

        let (publisher, _) = harness(Err(AtomicDirectorySnapshotPublishError::OutcomeUnknown {
            target_root: project.write_back_root().to_path_buf(),
            source: FakeError("restore"),
        }));
        let error = publisher
            .publish(&project, documents(&project, Vec::new()))
            .await
            .expect_err("结果未知终态必须传播");
        assert!(matches!(
            error,
            StandardWriteBackPublishingError::Publish(
                AtomicDirectorySnapshotPublishError::OutcomeUnknown {
                    target_root,
                    source: FakeError("restore")
                }
            ) if target_root == project.write_back_root()
        ));
    }

    #[test]
    fn publishing_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let project = project("alice", "C:/projects");
        let candidate = documents(&project, Vec::new());
        let (publisher, _) = harness(Ok(()));
        assert_send(publisher.publish(&project, candidate));
    }
}
