//! 把 Rewriter 候选提交给可恢复目录发布根。

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::att_mz::ProjectName;
use crate::att_mz::project::OpenedProject;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryFileOverlay, DirectoryPrepareError, DirectoryPublishError,
    DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, RecoverableDirectoryPublisher, ScopedDirectoryBindError,
    ScopedDirectoryEditError, ScopedDirectoryEditor, ScopedDirectoryEntry,
    ScopedDirectoryEntryKind,
};

use super::lua::ScopedPreparedWriteBackCandidate;
use super::rewriter::MzRewrittenDocuments;
use super::{
    PreparedWriteBackCandidate, PublishedWriteBack, StandardWriteBackPublisher,
    WriteBackPublishFailure, WriteBackPublishFailureState,
};

/// 根已准备、只能发布或丢弃一次的完整写回候选。
pub(crate) struct PreparedWriteBack<S> {
    project_name: ProjectName,
    workspace_root: PathBuf,
    output_root: PathBuf,
    staged: crate::storage::file_system::StagedDirectory<S>,
}

impl<S> fmt::Debug for PreparedWriteBack<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedWriteBack")
            .field("project_name", &self.project_name)
            .field("workspace_root", &self.workspace_root)
            .field("output_root", &self.output_root)
            .field("candidate_root", &self.staged.staging_root())
            .finish_non_exhaustive()
    }
}

impl<S> PreparedWriteBackCandidate for PreparedWriteBack<S>
where
    S: Send + 'static,
{
    fn belongs_to(&self, project: &OpenedProject) -> bool {
        self.project_name == *project.name()
            && self.workspace_root == project.workspace_root()
            && self.output_root == project.write_back_root()
    }

    fn candidate_root(&self) -> &Path {
        self.staged.staging_root()
    }
}

impl<S> ScopedPreparedWriteBackCandidate<S> for PreparedWriteBack<S>
where
    S: Send + 'static,
{
    fn staged_directory(&self) -> &crate::storage::file_system::StagedDirectory<S> {
        &self.staged
    }
}

/// 用固定 `source/data`、`source/js` 基底发布 Standard 写回候选。
pub(crate) struct StandardWriteBackPublishingService<A> {
    directory_publisher: A,
}

impl<A> StandardWriteBackPublishingService<A> {
    pub(crate) fn new(directory_publisher: A) -> Self {
        Self {
            directory_publisher,
        }
    }
}

impl<A> StandardWriteBackPublisher<MzRewrittenDocuments> for StandardWriteBackPublishingService<A>
where
    A: RecoverableDirectoryPublisher
        + ScopedDirectoryEditor<
            CandidateState = <A as RecoverableDirectoryPublisher>::StagingState,
            Error = <A as RecoverableDirectoryPublisher>::Error,
        >,
{
    type Candidate = PreparedWriteBack<<A as RecoverableDirectoryPublisher>::StagingState>;
    type Error = StandardWriteBackPublishingError<<A as RecoverableDirectoryPublisher>::Error>;

    async fn prepare(
        &self,
        project: &OpenedProject,
        documents: MzRewrittenDocuments,
    ) -> Result<Self::Candidate, Self::Error> {
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
            DirectoryPublishIntent::ReplaceExisting,
            source_mappings,
            overlays,
            Vec::new(),
        )
        .map_err(StandardWriteBackPublishingError::InvalidRequest)?;

        let staged = self
            .directory_publisher
            .prepare(request)
            .await
            .map_err(StandardWriteBackPublishingError::Prepare)?;
        Ok(PreparedWriteBack {
            project_name: project.name().clone(),
            workspace_root: project.workspace_root().to_path_buf(),
            output_root: project.write_back_root().to_path_buf(),
            staged,
        })
    }

    fn validate<'a>(
        &'a self,
        candidate: &Self::Candidate,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + use<'a, A> {
        // 先同步建立不再借用候选的 bind future，避免把只承诺 Send 的根 token state
        // 通过 `&Candidate` 带入异步状态机并错误要求 Sync。
        let bind = self
            .directory_publisher
            .bind_scoped_directory(&candidate.staged, super::mz_output_scope());
        let candidate_root = candidate.candidate_root().to_path_buf();
        let directory_publisher = &self.directory_publisher;
        async move {
            let scope = bind
                .await
                .map_err(StandardWriteBackPublishingError::BindCandidate)?;
            directory_publisher
                .validate_scoped_directory(&scope)
                .await
                .map_err(StandardWriteBackPublishingError::ValidateCandidate)?;
            let entries = directory_publisher
                .list_scoped_root(&scope)
                .await
                .map_err(StandardWriteBackPublishingError::InspectCandidateRoot)?;
            validate_mz_candidate_root(&entries).map_err(|()| {
                StandardWriteBackPublishingError::InvalidCandidateRoot {
                    root: candidate_root,
                }
            })
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
                StandardWriteBackPublishingError::Publish(source),
            ));
        }
        Ok(PublishedWriteBack::new(output_root))
    }

    async fn discard(&self, candidate: Self::Candidate) -> Result<(), Self::Error> {
        self.directory_publisher
            .discard(candidate.staged)
            .await
            .map_err(StandardWriteBackPublishingError::Discard)
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

/// Standard Publisher 在候选交接、请求建立或根终结阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum StandardWriteBackPublishingError<E> {
    CandidateProjectMismatch {
        expected_name: ProjectName,
        expected_workspace_root: PathBuf,
        candidate_name: ProjectName,
        candidate_workspace_root: PathBuf,
    },
    InvalidRequest(DirectoryStageRequestError),
    Prepare(DirectoryPrepareError<E>),
    BindCandidate(ScopedDirectoryBindError<E>),
    ValidateCandidate(ScopedDirectoryEditError<E>),
    InspectCandidateRoot(ScopedDirectoryEditError<E>),
    InvalidCandidateRoot {
        root: PathBuf,
    },
    Publish(DirectoryPublishError<E>),
    Discard(DirectoryDiscardError<E>),
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
            Self::BindCandidate(source) => {
                write!(formatter, "无法绑定写回候选的物理身份：{source}")
            }
            Self::ValidateCandidate(source) => {
                write!(formatter, "写回候选未通过完整树校验：{source}")
            }
            Self::InspectCandidateRoot(source) => {
                write!(formatter, "无法检查写回候选顶层结构：{source}")
            }
            Self::InvalidCandidateRoot { root } => write!(
                formatter,
                "写回候选根必须恰好包含普通 data 与 js 目录：{}",
                root.display()
            ),
            Self::Publish(source) => source.fmt(formatter),
            Self::Discard(source) => source.fmt(formatter),
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
            Self::BindCandidate(source) => Some(source),
            Self::ValidateCandidate(source) => Some(source),
            Self::InspectCandidateRoot(source) => Some(source),
            Self::InvalidCandidateRoot { .. } => None,
            Self::Publish(source) => Some(source),
            Self::Discard(source) => Some(source),
        }
    }
}

fn validate_mz_candidate_root(entries: &[ScopedDirectoryEntry]) -> Result<(), ()> {
    if entries.len() != 2 {
        return Err(());
    }
    let has_data = entries.iter().any(|entry| {
        entry.name() == std::ffi::OsStr::new("data")
            && entry.kind() == ScopedDirectoryEntryKind::Directory
    });
    let has_js = entries.iter().any(|entry| {
        entry.name() == std::ffi::OsStr::new("js")
            && entry.kind() == ScopedDirectoryEntryKind::Directory
    });
    if has_data && has_js { Ok(()) } else { Err(()) }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::write_back::rewriter::MzRewrittenFile;

    use crate::storage::file_system::{
        BoundScopedDirectory, ScopedDirectoryEntry, ScopedDirectoryPath, ScopedDirectoryScope,
        StagedDirectory, StagingCleanupFailure,
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

        fn validate_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>>
        + Send
        + use<> {
            std::future::ready(Ok(()))
        }

        fn read_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>,
        > + Send {
            std::future::ready(Ok(Vec::new()))
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

        fn remove_scoped_path(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
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

    #[test]
    fn mz_candidate_root_owns_exact_data_and_js_structure() {
        let directory = |name: &str| {
            ScopedDirectoryEntry::new(name.into(), ScopedDirectoryEntryKind::Directory)
        };
        assert_eq!(
            validate_mz_candidate_root(&[directory("data"), directory("js")]),
            Ok(())
        );
        for entries in [
            vec![directory("data")],
            vec![directory("data"), directory("js"), directory("other")],
            vec![directory("Data"), directory("js")],
            vec![
                ScopedDirectoryEntry::new("data".into(), ScopedDirectoryEntryKind::File),
                directory("js"),
            ],
        ] {
            assert_eq!(validate_mz_candidate_root(&entries), Err(()));
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
        StandardWriteBackPublishingService<FakeRecoverablePublisher>,
        PrepareCalls,
        PublishCalls,
    ) {
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publish_calls = Arc::new(Mutex::new(Vec::new()));
        (
            StandardWriteBackPublishingService::new(FakeRecoverablePublisher {
                prepare_calls: Arc::clone(&prepare_calls),
                publish_calls: Arc::clone(&publish_calls),
                prepare_error: Arc::new(Mutex::new(prepare_error)),
                publish_result: Arc::new(Mutex::new(Some(result))),
                discard_calls: Arc::new(Mutex::new(Vec::new())),
                discard_error: Arc::new(Mutex::new(None)),
            }),
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
                    vec![("data/Items.json", b"items"), ("js/plugins.js", b"plugins")],
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
    async fn prepared_candidate_can_be_explicitly_discarded_without_publishing() {
        let project = project("alice", "C:/projects");
        let prepare_calls = Arc::new(Mutex::new(Vec::new()));
        let publish_calls = Arc::new(Mutex::new(Vec::new()));
        let discard_calls = Arc::new(Mutex::new(Vec::new()));
        let publisher = StandardWriteBackPublishingService::new(FakeRecoverablePublisher {
            prepare_calls,
            publish_calls: Arc::clone(&publish_calls),
            prepare_error: Arc::new(Mutex::new(None)),
            publish_result: Arc::new(Mutex::new(Some(Ok(())))),
            discard_calls: Arc::clone(&discard_calls),
            discard_error: Arc::new(Mutex::new(None)),
        });

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
        let publisher = StandardWriteBackPublishingService::new(FakeRecoverablePublisher {
            prepare_calls: Arc::new(Mutex::new(Vec::new())),
            publish_calls: Arc::new(Mutex::new(Vec::new())),
            prepare_error: Arc::new(Mutex::new(None)),
            publish_result: Arc::new(Mutex::new(Some(Ok(())))),
            discard_calls: Arc::new(Mutex::new(Vec::new())),
            discard_error: Arc::new(Mutex::new(Some(FakeError("cleanup")))),
        });
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
            StandardWriteBackPublishingError::Discard(source)
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
            StandardWriteBackPublishingError::Prepare(DirectoryPrepareError::NotPrepared {
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
        StandardWriteBackPublishingError<FakeError>,
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
            StandardWriteBackPublishingError::Publish(
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
            StandardWriteBackPublishingError::Publish(
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
            StandardWriteBackPublishingError::Publish(
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
            StandardWriteBackPublishingError::Publish(
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
            StandardWriteBackPublishingError::Publish(
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
