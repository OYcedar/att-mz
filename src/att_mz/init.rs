use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;
use super::project::MzWriteBackLayoutProfile;
use super::standard_asset::MzStandardAssetOwner;
use crate::execution::{CooperativeCancellation, OperationCancelled};
use crate::project_database::{
    NewProject, ProjectDatabaseCreator, ProjectDatabaseStateReconciler, ProjectWorkspaceLayout,
    SourceSnapshotFingerprint,
};
use crate::storage::file_system::{
    DIRECTORY_PUBLISH_LOCK_NAMESPACE, DirectoryDiscardError, DirectoryEntry, DirectoryEntryKind,
    DirectoryLister, DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent,
    DirectorySourceMapping, DirectoryStageRequest, DirectoryStageRequestError,
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
    DirectoryTreeRoot, ExistingDirectoryResolver, ListDirectoryError, ProjectOperationLeaseError,
    ProjectOperationLeaseProvider, ProjectOperationLeaseRequest, ProjectOperationLeaseRequestError,
    RecoverableDirectoryPublisher, ResolveDirectoryError, StagedDirectory,
};
use crate::storage::sqlite::{SnapshotDatabaseError, SqliteDatabaseSnapshotter};

/// 初始化 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitInput {
    pub name: ProjectName,
    pub game_root: PathBuf,
    pub source_language: String,
    pub target_language: String,
    pub layout_profile: MzWriteBackLayoutProfile,
}

/// Init 更新后可能需要重新提取的标准资产 owner。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitStaleOwner {
    Builtin,
    Rules,
    Lua,
}

/// 本次 Init 把项目工作区收敛到的终态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    Created,
    Unchanged,
    Updated { stale_owners: Vec<InitStaleOwner> },
}

/// 初始化成功后交还给 CLI 的项目与收敛结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitOutput {
    pub name: ProjectName,
    pub outcome: InitOutcome,
}

/// 完成一个 MZ 游戏初始化用例。
pub trait InitUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: InitInput,
    ) -> impl Future<Output = Result<InitOutput, Self::Error>> + Send;
}

/// 完成一次工作区状态收敛所需的全部受信事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectWorkspaceConvergenceRequest {
    source_game_root: PathBuf,
    name: ProjectName,
    source_language: String,
    target_language: String,
    layout_profile: MzWriteBackLayoutProfile,
}

impl ProjectWorkspaceConvergenceRequest {
    pub(crate) fn new(
        source_game_root: PathBuf,
        name: ProjectName,
        source_language: String,
        target_language: String,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        Self {
            source_game_root,
            name,
            source_language,
            target_language,
            layout_profile,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectWorkspaceConvergence {
    Created,
    Unchanged,
    Updated {
        stale_owners: Vec<MzStandardAssetOwner>,
    },
}

/// 把项目工作区收敛到本次请求的唯一当前状态。
pub(crate) trait ProjectWorkspaceConverger: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn converge(
        &self,
        request: ProjectWorkspaceConvergenceRequest,
    ) -> impl Future<Output = Result<ProjectWorkspaceConvergence, Self::Error>> + Send;
}

/// 项目工作区收敛服务；持有项目租约直到候选被发布或明确丢弃。
pub(crate) struct ProjectWorkspaceConvergenceService<D, S, R, F, A> {
    projects_root: PathBuf,
    database_creator: D,
    database_snapshotter: S,
    database_reconciler: R,
    file_system: F,
    directories: A,
    cancellation: CooperativeCancellation,
}

impl<D, S, R, F, A> ProjectWorkspaceConvergenceService<D, S, R, F, A> {
    #[allow(
        clippy::too_many_arguments,
        reason = "每项参数都是本职责的直接依赖或构造事实"
    )]
    pub(crate) fn new(
        projects_root: PathBuf,
        database_creator: D,
        database_snapshotter: S,
        database_reconciler: R,
        file_system: F,
        directories: A,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            projects_root,
            database_creator,
            database_snapshotter,
            database_reconciler,
            file_system,
            directories,
            cancellation,
        }
    }
}

impl<D, S, R, F, A> ProjectWorkspaceConverger for ProjectWorkspaceConvergenceService<D, S, R, F, A>
where
    D: ProjectDatabaseCreator,
    S: SqliteDatabaseSnapshotter,
    R: ProjectDatabaseStateReconciler,
    F: ExistingDirectoryResolver
        + DirectoryLister<Error = <F as ExistingDirectoryResolver>::Error>
        + DirectoryTreeFingerprinter
        + ProjectOperationLeaseProvider,
    A: RecoverableDirectoryPublisher,
{
    type Error = ProjectWorkspaceConvergenceError<
        D::Error,
        S::Error,
        R::InspectionError,
        R::ReconciliationError,
        <F as ExistingDirectoryResolver>::Error,
        <F as DirectoryTreeFingerprinter>::Error,
        <F as ProjectOperationLeaseProvider>::Error,
        A::Error,
    >;

    async fn converge(
        &self,
        request: ProjectWorkspaceConvergenceRequest,
    ) -> Result<ProjectWorkspaceConvergence, Self::Error> {
        self.cancellation
            .check()
            .map_err(ProjectWorkspaceConvergenceError::CancelledBeforeCandidate)?;
        let lease_request = ProjectOperationLeaseRequest::new(
            self.projects_root.clone(),
            request.name.as_str().into(),
        )
        .map_err(ProjectWorkspaceConvergenceError::InvalidLeaseRequest)?;
        let _lease = self
            .file_system
            .acquire_project_operation_lease(lease_request)
            .await
            .map_err(ProjectWorkspaceConvergenceError::Lease)?;
        let source_game_root = self
            .file_system
            .resolve_existing_directory(request.source_game_root)
            .await
            .map_err(ProjectWorkspaceConvergenceError::SourceGameRoot)?;

        let final_layout = ProjectWorkspaceLayout::for_project(&self.projects_root, &request.name);
        let target_exists = match self
            .file_system
            .resolve_existing_directory(final_layout.workspace_root().to_path_buf())
            .await
        {
            Ok(_) => true,
            Err(ResolveDirectoryError::NotFound { .. }) => false,
            Err(error) => return Err(ProjectWorkspaceConvergenceError::WorkspaceRoot(error)),
        };

        let (current_state, workspace_complete) = if target_exists {
            let state = self
                .database_reconciler
                .inspect(
                    final_layout.database_path().to_path_buf(),
                    request.name.clone(),
                )
                .await
                .map_err(ProjectWorkspaceConvergenceError::InspectExistingDatabase)?;
            let structure_matches =
                observe_required_workspace_structure(&self.file_system, &final_layout)
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::ObserveWorkspaceStructure)?;
            let source_matches = if structure_matches {
                match fingerprint_source(&self.file_system, &final_layout, true)
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::ObserveExistingSource)?
                {
                    Some(actual) => actual == state.source_snapshot_fingerprint(),
                    None => false,
                }
            } else {
                false
            };
            let output_data = if structure_matches {
                optional_directory_exists(
                    &self.file_system,
                    final_layout.write_back_data().to_path_buf(),
                )
                .await
                .map_err(ProjectWorkspaceConvergenceError::ObserveWorkspaceDirectory)?
            } else {
                false
            };
            let output_js = if structure_matches {
                optional_directory_exists(
                    &self.file_system,
                    final_layout.write_back_js().to_path_buf(),
                )
                .await
                .map_err(ProjectWorkspaceConvergenceError::ObserveWorkspaceDirectory)?
            } else {
                false
            };
            (
                Some(state),
                structure_matches && source_matches && output_data && output_js,
            )
        } else {
            (None, false)
        };

        self.cancellation
            .check()
            .map_err(ProjectWorkspaceConvergenceError::CancelledBeforeCandidate)?;
        let publish_intent = if target_exists {
            DirectoryPublishIntent::ReplaceExisting
        } else {
            DirectoryPublishIntent::CreateNew
        };
        let stage_request = DirectoryStageRequest::new(
            final_layout.workspace_root().to_path_buf(),
            publish_intent,
            vec![
                DirectorySourceMapping::new(
                    source_game_root.join("data"),
                    PathBuf::from("source/data"),
                )?,
                DirectorySourceMapping::new(
                    source_game_root.join("js"),
                    PathBuf::from("source/js"),
                )?,
            ],
            Vec::new(),
            vec![
                PathBuf::from("write_back/data"),
                PathBuf::from("write_back/js"),
            ],
        )?;
        let staged = self
            .directories
            .prepare(stage_request)
            .await
            .map_err(ProjectWorkspaceConvergenceError::Prepare)?;
        let staged_layout =
            ProjectWorkspaceLayout::from_workspace_root(staged.staging_root().to_path_buf());

        if let Err(cancellation) = self.cancellation.check() {
            return Err(discard_candidate_failure(
                &self.directories,
                staged,
                ProjectWorkspaceCandidateFailure::Cancelled(cancellation),
            )
            .await);
        }
        let candidate_fingerprint =
            match fingerprint_source(&self.file_system, &staged_layout, false).await {
                Ok(Some(fingerprint)) => fingerprint,
                Ok(None) => unreachable!("候选来源缺失必须由指纹根返回错误"),
                Err(source) => {
                    return Err(discard_candidate_failure(
                        &self.directories,
                        staged,
                        ProjectWorkspaceCandidateFailure::FingerprintCandidate(source),
                    )
                    .await);
                }
            };
        let requested_project = NewProject::new(
            request.name,
            request.source_language,
            request.target_language,
            candidate_fingerprint,
            request.layout_profile,
        );

        if target_exists {
            if let Err(source) = self
                .database_snapshotter
                .snapshot_database(
                    final_layout.database_path().to_path_buf(),
                    staged_layout.database_path().to_path_buf(),
                )
                .await
            {
                return Err(discard_candidate_failure(
                    &self.directories,
                    staged,
                    ProjectWorkspaceCandidateFailure::SnapshotDatabase(source),
                )
                .await);
            }
        } else if let Err(source) = self
            .database_creator
            .create(
                staged_layout.database_path().to_path_buf(),
                requested_project.clone(),
            )
            .await
        {
            return Err(discard_candidate_failure(
                &self.directories,
                staged,
                ProjectWorkspaceCandidateFailure::CreateDatabase(source),
            )
            .await);
        }

        let reconciliation = match self
            .database_reconciler
            .reconcile(
                staged_layout.database_path().to_path_buf(),
                requested_project,
            )
            .await
        {
            Ok(value) => value,
            Err(source) => {
                return Err(discard_candidate_failure(
                    &self.directories,
                    staged,
                    ProjectWorkspaceCandidateFailure::ReconcileDatabase(source),
                )
                .await);
            }
        };

        if let Err(cancellation) = self.cancellation.check() {
            return Err(discard_candidate_failure(
                &self.directories,
                staged,
                ProjectWorkspaceCandidateFailure::Cancelled(cancellation),
            )
            .await);
        }
        if target_exists && workspace_complete && !reconciliation.changed() {
            self.directories
                .discard(staged)
                .await
                .map_err(ProjectWorkspaceConvergenceError::DiscardUnchanged)?;
            return Ok(ProjectWorkspaceConvergence::Unchanged);
        }

        self.directories
            .publish(staged)
            .await
            .map_err(ProjectWorkspaceConvergenceError::Publish)?;
        if current_state.is_some() {
            Ok(ProjectWorkspaceConvergence::Updated {
                stale_owners: reconciliation.stale_owners(),
            })
        } else {
            Ok(ProjectWorkspaceConvergence::Created)
        }
    }
}

async fn observe_required_workspace_structure<F>(
    file_system: &F,
    layout: &ProjectWorkspaceLayout,
) -> Result<bool, ListDirectoryError<F::Error>>
where
    F: DirectoryLister,
{
    let mut has_publish_lock_namespace = false;
    for (root, expected_children) in [
        (
            layout.workspace_root(),
            &[
                ("project.db", DirectoryEntryKind::RegularFile),
                ("source", DirectoryEntryKind::Directory),
                ("write_back", DirectoryEntryKind::Directory),
            ][..],
        ),
        (
            layout.source_root(),
            &[
                ("data", DirectoryEntryKind::Directory),
                ("js", DirectoryEntryKind::Directory),
            ][..],
        ),
        (
            layout.write_back_root(),
            &[
                ("data", DirectoryEntryKind::Directory),
                ("js", DirectoryEntryKind::Directory),
            ][..],
        ),
    ] {
        let children = match file_system.list_directory(root.to_path_buf()).await {
            Ok(children) => children,
            Err(ListDirectoryError::NotFound { .. } | ListDirectoryError::NotDirectory { .. }) => {
                return Ok(false);
            }
            Err(error @ ListDirectoryError::Io { .. }) => return Err(error),
        };
        let matches = if root == layout.workspace_root() {
            has_required_and_optional_child_names(
                &children,
                expected_children,
                &[
                    ("project.db-journal", DirectoryEntryKind::RegularFile),
                    ("project.db-wal", DirectoryEntryKind::RegularFile),
                    ("project.db-shm", DirectoryEntryKind::RegularFile),
                    (
                        DIRECTORY_PUBLISH_LOCK_NAMESPACE,
                        DirectoryEntryKind::Directory,
                    ),
                ],
            )
        } else {
            has_exact_child_names(&children, expected_children)
        };
        if !matches {
            return Ok(false);
        }
        if root == layout.workspace_root() {
            has_publish_lock_namespace = count_child(
                &children,
                (
                    DIRECTORY_PUBLISH_LOCK_NAMESPACE,
                    DirectoryEntryKind::Directory,
                ),
            ) == 1;
        }
    }
    if has_publish_lock_namespace {
        let lock_namespace = layout
            .workspace_root()
            .join(DIRECTORY_PUBLISH_LOCK_NAMESPACE);
        let children = match file_system.list_directory(lock_namespace).await {
            Ok(children) => children,
            Err(ListDirectoryError::NotFound { .. } | ListDirectoryError::NotDirectory { .. }) => {
                return Ok(false);
            }
            Err(error @ ListDirectoryError::Io { .. }) => return Err(error),
        };
        if !has_exact_child_names(
            &children,
            &[("write_back", DirectoryEntryKind::RegularFile)],
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn has_exact_child_names(
    children: &[DirectoryEntry],
    expected: &[(&str, DirectoryEntryKind)],
) -> bool {
    has_required_and_optional_child_names(children, expected, &[])
}

fn has_required_and_optional_child_names(
    children: &[DirectoryEntry],
    required: &[(&str, DirectoryEntryKind)],
    optional: &[(&str, DirectoryEntryKind)],
) -> bool {
    children.iter().all(|child| {
        child.resolved_path().file_name().is_some_and(|name| {
            required
                .iter()
                .chain(optional)
                .any(|(expected_name, expected_kind)| {
                    name == *expected_name && child.kind() == *expected_kind
                })
        })
    }) && required
        .iter()
        .all(|expected| count_child(children, *expected) == 1)
        && optional
            .iter()
            .all(|expected| count_child(children, *expected) <= 1)
}

fn count_child(children: &[DirectoryEntry], expected: (&str, DirectoryEntryKind)) -> usize {
    children
        .iter()
        .filter(|child| {
            child
                .resolved_path()
                .file_name()
                .is_some_and(|name| name == expected.0)
                && child.kind() == expected.1
        })
        .count()
}

async fn optional_directory_exists<F>(
    file_system: &F,
    path: PathBuf,
) -> Result<bool, ResolveDirectoryError<F::Error>>
where
    F: ExistingDirectoryResolver,
{
    match file_system.resolve_existing_directory(path).await {
        Ok(_) => Ok(true),
        Err(
            ResolveDirectoryError::NotFound { .. } | ResolveDirectoryError::NotDirectory { .. },
        ) => Ok(false),
        Err(error @ ResolveDirectoryError::Io { .. }) => Err(error),
    }
}

async fn fingerprint_source<F>(
    file_system: &F,
    layout: &ProjectWorkspaceLayout,
    absence_is_repairable: bool,
) -> Result<Option<SourceSnapshotFingerprint>, DirectoryTreeFingerprintError<F::Error>>
where
    F: DirectoryTreeFingerprinter,
{
    let request = DirectoryTreeFingerprintRequest::new(vec![
        DirectoryTreeRoot::new(layout.source_data().to_path_buf(), PathBuf::from("data"))
            .expect("固定 data 逻辑根必须合法"),
        DirectoryTreeRoot::new(layout.source_js().to_path_buf(), PathBuf::from("js"))
            .expect("固定 js 逻辑根必须合法"),
    ])
    .expect("固定 data 与 js 逻辑根必须互不重叠");
    match file_system.fingerprint_directory_tree(request).await {
        Ok(value) => Ok(Some(SourceSnapshotFingerprint::from_bytes(
            value.into_bytes(),
        ))),
        Err(
            DirectoryTreeFingerprintError::NotFound { .. }
            | DirectoryTreeFingerprintError::NotDirectory { .. },
        ) if absence_is_repairable => Ok(None),
        Err(error) => Err(error),
    }
}

async fn discard_candidate_failure<A, D, S, I, R, E, P, L>(
    directories: &A,
    staged: StagedDirectory<A::StagingState>,
    failure: ProjectWorkspaceCandidateFailure<D, S, R, P>,
) -> ProjectWorkspaceConvergenceError<D, S, I, R, E, P, L, A::Error>
where
    A: RecoverableDirectoryPublisher,
{
    let discard = directories.discard(staged).await.err();
    ProjectWorkspaceConvergenceError::CandidateFailure { failure, discard }
}

#[derive(Debug)]
pub(crate) enum ProjectWorkspaceCandidateFailure<D, S, R, P> {
    Cancelled(OperationCancelled),
    FingerprintCandidate(DirectoryTreeFingerprintError<P>),
    CreateDatabase(D),
    SnapshotDatabase(SnapshotDatabaseError<S>),
    ReconcileDatabase(R),
}

#[derive(Debug)]
pub(crate) enum ProjectWorkspaceConvergenceError<D, S, I, R, E, P, L, A> {
    CancelledBeforeCandidate(OperationCancelled),
    InvalidLeaseRequest(ProjectOperationLeaseRequestError),
    Lease(ProjectOperationLeaseError<L>),
    SourceGameRoot(ResolveDirectoryError<E>),
    WorkspaceRoot(ResolveDirectoryError<E>),
    InspectExistingDatabase(I),
    ObserveWorkspaceStructure(ListDirectoryError<E>),
    ObserveExistingSource(DirectoryTreeFingerprintError<P>),
    ObserveWorkspaceDirectory(ResolveDirectoryError<E>),
    InvalidStageRequest(DirectoryStageRequestError),
    Prepare(DirectoryPrepareError<A>),
    CandidateFailure {
        failure: ProjectWorkspaceCandidateFailure<D, S, R, P>,
        discard: Option<DirectoryDiscardError<A>>,
    },
    DiscardUnchanged(DirectoryDiscardError<A>),
    Publish(DirectoryPublishError<A>),
}

impl<D, S, I, R, E, P, L, A> From<DirectoryStageRequestError>
    for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, L, A>
{
    fn from(error: DirectoryStageRequestError) -> Self {
        Self::InvalidStageRequest(error)
    }
}

impl<D, S, I, R, E, P, L, A> fmt::Display
    for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, L, A>
where
    D: fmt::Display,
    S: fmt::Display,
    I: fmt::Display,
    R: fmt::Display,
    E: fmt::Display,
    P: fmt::Display,
    L: fmt::Display,
    A: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CancelledBeforeCandidate(error) => error.fmt(formatter),
            Self::InvalidLeaseRequest(error) => write!(formatter, "项目租约请求无效：{error}"),
            Self::Lease(error) => error.fmt(formatter),
            Self::SourceGameRoot(error) => write!(formatter, "无法使用游戏根目录：{error}"),
            Self::WorkspaceRoot(error) => write!(formatter, "项目工作区根无效：{error}"),
            Self::InspectExistingDatabase(error) => {
                write!(formatter, "现存项目数据库无效：{error}")
            }
            Self::ObserveWorkspaceStructure(error) => {
                write!(formatter, "无法检查现存工作区结构：{error}")
            }
            Self::ObserveExistingSource(error) => {
                write!(formatter, "无法检查现存冻结来源：{error}")
            }
            Self::ObserveWorkspaceDirectory(error) => {
                write!(formatter, "无法检查现存工作区结构：{error}")
            }
            Self::InvalidStageRequest(error) => write!(formatter, "工作区候选请求无效：{error}"),
            Self::Prepare(error) => write!(formatter, "无法准备工作区候选：{error}"),
            Self::CandidateFailure { failure, discard } => {
                write!(formatter, "工作区候选处理失败：{failure}")?;
                if let Some(discard) = discard {
                    write!(formatter, "；且候选清理失败：{discard}")?;
                }
                Ok(())
            }
            Self::DiscardUnchanged(error) => {
                write!(formatter, "工作区事实未变化，但无法丢弃候选：{error}")
            }
            Self::Publish(error) => write!(formatter, "无法发布完整工作区：{error}"),
        }
    }
}

impl<D, S, R, P> fmt::Display for ProjectWorkspaceCandidateFailure<D, S, R, P>
where
    D: fmt::Display,
    S: fmt::Display,
    R: fmt::Display,
    P: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::FingerprintCandidate(error) => {
                write!(formatter, "无法建立候选来源指纹：{error}")
            }
            Self::CreateDatabase(error) => write!(formatter, "无法创建候选数据库：{error}"),
            Self::SnapshotDatabase(error) => write!(formatter, "无法复制现存数据库：{error}"),
            Self::ReconcileDatabase(error) => write!(formatter, "无法对账候选数据库：{error}"),
        }
    }
}

impl<D, S, I, R, E, P, L, A> Error for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, L, A>
where
    D: Error + 'static,
    S: Error + 'static,
    I: Error + 'static,
    R: Error + 'static,
    E: Error + 'static,
    P: Error + 'static,
    L: Error + 'static,
    A: Error + 'static,
{
}

/// 只负责验证初始化意图并交给工作区收敛边界。
pub(crate) struct InitService<W> {
    workspace_converger: W,
    cancellation: CooperativeCancellation,
}

impl<W> InitService<W> {
    pub(crate) fn new(workspace_converger: W, cancellation: CooperativeCancellation) -> Self {
        Self {
            workspace_converger,
            cancellation,
        }
    }
}

impl<W> InitUseCase for InitService<W>
where
    W: ProjectWorkspaceConverger,
{
    type Error = InitServiceError<W::Error>;

    async fn execute(&self, input: InitInput) -> Result<InitOutput, Self::Error> {
        self.cancellation
            .check()
            .map_err(InitServiceError::Cancelled)?;
        let source_language =
            normalized_language(input.source_language, InitServiceError::EmptySourceLanguage)?;
        let target_language =
            normalized_language(input.target_language, InitServiceError::EmptyTargetLanguage)?;

        let output_name = input.name.clone();
        let outcome = self
            .workspace_converger
            .converge(ProjectWorkspaceConvergenceRequest::new(
                input.game_root,
                input.name,
                source_language,
                target_language,
                input.layout_profile,
            ))
            .await
            .map_err(InitServiceError::Workspace)?;
        let outcome = match outcome {
            ProjectWorkspaceConvergence::Created => InitOutcome::Created,
            ProjectWorkspaceConvergence::Unchanged => InitOutcome::Unchanged,
            ProjectWorkspaceConvergence::Updated { stale_owners } => InitOutcome::Updated {
                stale_owners: stale_owners.into_iter().map(InitStaleOwner::from).collect(),
            },
        };

        Ok(InitOutput {
            name: output_name,
            outcome,
        })
    }
}

impl From<MzStandardAssetOwner> for InitStaleOwner {
    fn from(owner: MzStandardAssetOwner) -> Self {
        match owner {
            MzStandardAssetOwner::Builtin => Self::Builtin,
            MzStandardAssetOwner::Rules => Self::Rules,
            MzStandardAssetOwner::Lua => Self::Lua,
        }
    }
}

fn normalized_language<W>(
    value: String,
    empty_error: InitServiceError<W>,
) -> Result<String, InitServiceError<W>> {
    let normalized = value.trim();
    if normalized.is_empty() {
        Err(empty_error)
    } else {
        Ok(normalized.to_owned())
    }
}

/// 初始化编排在本职责边界内能够产生的错误。
#[derive(Debug)]
pub(crate) enum InitServiceError<W> {
    Cancelled(OperationCancelled),
    EmptySourceLanguage,
    EmptyTargetLanguage,
    Workspace(W),
}

impl<W> fmt::Display for InitServiceError<W>
where
    W: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::EmptySourceLanguage => formatter.write_str("源语言去除首尾空白后不能为空"),
            Self::EmptyTargetLanguage => formatter.write_str("目标语言去除首尾空白后不能为空"),
            Self::Workspace(error) => write!(formatter, "无法收敛项目工作区：{error}"),
        }
    }
}

impl<W> Error for InitServiceError<W>
where
    W: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::Workspace(error) => Some(error),
            Self::EmptySourceLanguage | Self::EmptyTargetLanguage => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;

    type SnapshotDatabaseResponses = VecDeque<Result<(), SnapshotDatabaseError<FakeError>>>;
    use crate::fingerprint::Sha256Fingerprint;
    use crate::project_database::{
        CreatedProject, ProjectDatabaseReconciliation, ProjectDatabaseState,
    };
    use crate::storage::file_system::ProjectOperationLease;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Default)]
    struct Observations {
        events: Arc<Mutex<Vec<&'static str>>>,
        stage_requests: Arc<Mutex<Vec<DirectoryStageRequest>>>,
        created_projects: Arc<Mutex<Vec<(PathBuf, NewProject)>>>,
        snapshots: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        reconciled_projects: Arc<Mutex<Vec<(PathBuf, NewProject)>>>,
    }

    impl Observations {
        fn event(&self, event: &'static str) {
            self.events
                .lock()
                .expect("events mutex should not be poisoned")
                .push(event);
        }

        fn events(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .expect("events mutex should not be poisoned")
                .clone()
        }
    }

    struct LeaseState(Observations);

    impl Drop for LeaseState {
        fn drop(&mut self) {
            self.0.event("lease_drop");
        }
    }

    #[derive(Clone, Copy)]
    enum ExistingSourceObservation {
        Fingerprint([u8; 32]),
        Missing,
    }

    #[derive(Clone, Copy, Debug)]
    enum WorkspaceStructureObservation {
        Complete,
        SqliteSidecars,
        SqliteSidecarNotFile,
        DirectoryPublishLock,
        DirectoryPublishLockNamespaceNotDirectory,
        DirectoryPublishLockFileNotRegular,
        DirectoryPublishLockMissingFile,
        DirectoryPublishLockExtraEntry,
        DatabaseNotFile,
        SourceNotDirectory,
        SourceDataNotDirectory,
        WriteBackDataNotDirectory,
        ExtraWorkspaceEntry,
        ExtraSourceEntry,
        ExtraWriteBackEntry,
        MissingSourceEntry,
        WriteBackNotDirectory,
        Io,
    }

    #[derive(Clone)]
    struct FakeWorkspaceFileSystem {
        observations: Observations,
        target_exists: bool,
        workspace_structure: WorkspaceStructureObservation,
        existing_source: ExistingSourceObservation,
        candidate_fingerprint: [u8; 32],
    }

    impl ExistingDirectoryResolver for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn resolve_existing_directory(
            &self,
            path: PathBuf,
        ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
            if path == Path::new("C:/games/source") {
                self.observations.event("game_root");
                return Ok(path);
            }
            if path == Path::new("C:/projects/game") {
                self.observations.event("workspace_root");
                if self.target_exists {
                    return Ok(path);
                }
                return Err(ResolveDirectoryError::NotFound { path });
            }
            if path.ends_with("write_back/data") {
                self.observations.event("output_data");
                return Ok(path);
            }
            if path.ends_with("write_back/js") {
                self.observations.event("output_js");
                return Ok(path);
            }
            Ok(path)
        }
    }

    impl DirectoryLister for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn list_directory(
            &self,
            path: PathBuf,
        ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
            if matches!(self.workspace_structure, WorkspaceStructureObservation::Io) {
                return Err(ListDirectoryError::Io {
                    path,
                    source: FakeError("list workspace"),
                });
            }
            if path == Path::new("C:/projects/game") {
                self.observations.event("list_workspace");
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("project.db"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::DatabaseNotFile
                        ) {
                            DirectoryEntryKind::Directory
                        } else {
                            DirectoryEntryKind::RegularFile
                        },
                    ),
                    DirectoryEntry::new(
                        path.join("source"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::SourceNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("write_back"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraWorkspaceEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::SqliteSidecars
                        | WorkspaceStructureObservation::SqliteSidecarNotFile
                ) {
                    children.extend([
                        DirectoryEntry::new(
                            path.join("project.db-journal"),
                            if matches!(
                                self.workspace_structure,
                                WorkspaceStructureObservation::SqliteSidecarNotFile
                            ) {
                                DirectoryEntryKind::Directory
                            } else {
                                DirectoryEntryKind::RegularFile
                            },
                        ),
                        DirectoryEntry::new(
                            path.join("project.db-wal"),
                            DirectoryEntryKind::RegularFile,
                        ),
                        DirectoryEntry::new(
                            path.join("project.db-shm"),
                            DirectoryEntryKind::RegularFile,
                        ),
                    ]);
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::DirectoryPublishLock
                        | WorkspaceStructureObservation::DirectoryPublishLockNamespaceNotDirectory
                        | WorkspaceStructureObservation::DirectoryPublishLockFileNotRegular
                        | WorkspaceStructureObservation::DirectoryPublishLockMissingFile
                        | WorkspaceStructureObservation::DirectoryPublishLockExtraEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join(DIRECTORY_PUBLISH_LOCK_NAMESPACE),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::DirectoryPublishLockNamespaceNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ));
                }
                return Ok(children);
            }
            if path == Path::new("C:/projects/game/source") {
                self.observations.event("list_source");
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("data"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::SourceDataNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::MissingSourceEntry
                ) {
                    children.pop();
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraSourceEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                return Ok(children);
            }
            if path == Path::new("C:/projects/game/write_back") {
                self.observations.event("list_write_back");
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::WriteBackNotDirectory
                ) {
                    return Err(ListDirectoryError::NotDirectory { path });
                }
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("data"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::WriteBackDataNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraWriteBackEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                return Ok(children);
            }
            if path == Path::new("C:/projects/game").join(DIRECTORY_PUBLISH_LOCK_NAMESPACE) {
                self.observations.event("list_directory_publish_locks");
                let mut children = if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::DirectoryPublishLockMissingFile
                ) {
                    Vec::new()
                } else {
                    vec![DirectoryEntry::new(
                        path.join("write_back"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::DirectoryPublishLockFileNotRegular
                        ) {
                            DirectoryEntryKind::Directory
                        } else {
                            DirectoryEntryKind::RegularFile
                        },
                    )]
                };
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::DirectoryPublishLockExtraEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                return Ok(children);
            }
            panic!("测试未声明目录列举：{}", path.display());
        }
    }

    impl ProjectOperationLeaseProvider for FakeWorkspaceFileSystem {
        type Error = FakeError;
        type LeaseState = LeaseState;

        async fn acquire_project_operation_lease(
            &self,
            request: ProjectOperationLeaseRequest,
        ) -> Result<ProjectOperationLease<Self::LeaseState>, ProjectOperationLeaseError<Self::Error>>
        {
            assert_eq!(request.projects_root(), Path::new("C:/projects"));
            assert_eq!(request.project_directory_name(), "game");
            self.observations.event("lease_acquire");
            Ok(ProjectOperationLease::new(LeaseState(
                self.observations.clone(),
            )))
        }
    }

    impl DirectoryTreeFingerprinter for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn fingerprint_directory_tree(
            &self,
            request: DirectoryTreeFingerprintRequest,
        ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
            assert_eq!(
                request
                    .roots()
                    .iter()
                    .map(|root| root.logical_root())
                    .collect::<Vec<_>>(),
                vec![Path::new("data"), Path::new("js")]
            );
            let candidate = request
                .roots()
                .iter()
                .all(|root| root.physical_root().starts_with("C:/projects/.game-stage"));
            if candidate {
                self.observations.event("fingerprint_candidate");
                return Ok(Sha256Fingerprint::from_bytes(self.candidate_fingerprint));
            }
            self.observations.event("fingerprint_existing");
            match self.existing_source {
                ExistingSourceObservation::Fingerprint(value) => {
                    Ok(Sha256Fingerprint::from_bytes(value))
                }
                ExistingSourceObservation::Missing => {
                    Err(DirectoryTreeFingerprintError::NotFound {
                        path: PathBuf::from("C:/projects/game/source/data"),
                    })
                }
            }
        }
    }

    #[derive(Clone)]
    struct FakeDatabaseCreator {
        observations: Observations,
    }

    impl ProjectDatabaseCreator for FakeDatabaseCreator {
        type Error = FakeError;

        async fn create(
            &self,
            destination_path: PathBuf,
            project: NewProject,
        ) -> Result<CreatedProject, Self::Error> {
            self.observations.event("create_database");
            self.observations
                .created_projects
                .lock()
                .expect("created projects mutex should not be poisoned")
                .push((destination_path.clone(), project));
            Ok(CreatedProject::new(destination_path))
        }
    }

    #[derive(Clone)]
    struct FakeSnapshotter {
        observations: Observations,
        responses: Arc<Mutex<SnapshotDatabaseResponses>>,
    }

    impl SqliteDatabaseSnapshotter for FakeSnapshotter {
        type Error = FakeError;

        async fn snapshot_database(
            &self,
            source: PathBuf,
            destination: PathBuf,
        ) -> Result<(), SnapshotDatabaseError<Self::Error>> {
            self.observations.event("snapshot_database");
            self.observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .push((source, destination));
            self.responses
                .lock()
                .expect("snapshot responses mutex should not be poisoned")
                .pop_front()
                .expect("测试必须提供快照响应")
        }
    }

    #[derive(Clone)]
    struct FakeReconciler {
        observations: Observations,
        inspection: Result<ProjectDatabaseState, FakeError>,
        reconciliation: Result<ProjectDatabaseReconciliation, FakeError>,
    }

    impl ProjectDatabaseStateReconciler for FakeReconciler {
        type InspectionError = FakeError;
        type ReconciliationError = FakeError;

        async fn inspect(
            &self,
            _database_path: PathBuf,
            _expected_name: ProjectName,
        ) -> Result<ProjectDatabaseState, Self::InspectionError> {
            self.observations.event("inspect_database");
            self.inspection.clone()
        }

        async fn reconcile(
            &self,
            database_path: PathBuf,
            requested: NewProject,
        ) -> Result<ProjectDatabaseReconciliation, Self::ReconciliationError> {
            self.observations.event("reconcile_database");
            self.observations
                .reconciled_projects
                .lock()
                .expect("reconciled projects mutex should not be poisoned")
                .push((database_path, requested));
            self.reconciliation.clone()
        }
    }

    #[derive(Clone)]
    struct FakePublisher {
        observations: Observations,
        discard_error: Arc<Mutex<Option<FakeError>>>,
    }

    impl RecoverableDirectoryPublisher for FakePublisher {
        type Error = FakeError;
        type StagingState = usize;

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>>
        {
            self.observations.event("prepare");
            self.observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")
                .push(request.clone());
            Ok(StagedDirectory::new(
                request.target_root().to_path_buf(),
                PathBuf::from("C:/projects/.game-stage"),
                request.publish_intent(),
                1,
            ))
        }

        async fn publish(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryPublishError<Self::Error>> {
            self.observations.event("publish");
            Ok(())
        }

        async fn discard(
            &self,
            staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryDiscardError<Self::Error>> {
            self.observations.event("discard");
            match self
                .discard_error
                .lock()
                .expect("discard error mutex should not be poisoned")
                .take()
            {
                Some(source) => Err(DirectoryDiscardError::new(
                    staged.staging_root().to_path_buf(),
                    source,
                )),
                None => Ok(()),
            }
        }
    }

    fn width(value: u32) -> super::super::project::MaxFullwidthChars {
        super::super::project::MaxFullwidthChars::new(value).expect("宽度应合法")
    }

    fn profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn fingerprint(value: u8) -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([value; 32])
    }

    fn database_state(
        source_fingerprint: u8,
        owners: Vec<(MzStandardAssetOwner, SourceSnapshotFingerprint)>,
    ) -> ProjectDatabaseState {
        ProjectDatabaseState::for_test(
            "game".parse().expect("项目名应合法"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            fingerprint(source_fingerprint),
            profile(),
            owners,
        )
    }

    fn request() -> ProjectWorkspaceConvergenceRequest {
        ProjectWorkspaceConvergenceRequest::new(
            PathBuf::from("C:/games/source"),
            "game".parse().expect("项目名应合法"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            profile(),
        )
    }

    fn service(
        target_exists: bool,
        workspace_structure: WorkspaceStructureObservation,
        existing_source: ExistingSourceObservation,
        candidate_fingerprint: u8,
        inspection: Result<ProjectDatabaseState, FakeError>,
        reconciliation: Result<ProjectDatabaseReconciliation, FakeError>,
        snapshot_response: Result<(), SnapshotDatabaseError<FakeError>>,
    ) -> (
        ProjectWorkspaceConvergenceService<
            FakeDatabaseCreator,
            FakeSnapshotter,
            FakeReconciler,
            FakeWorkspaceFileSystem,
            FakePublisher,
        >,
        Observations,
    ) {
        let observations = Observations::default();
        (
            ProjectWorkspaceConvergenceService::new(
                PathBuf::from("C:/projects"),
                FakeDatabaseCreator {
                    observations: observations.clone(),
                },
                FakeSnapshotter {
                    observations: observations.clone(),
                    responses: Arc::new(Mutex::new(VecDeque::from([snapshot_response]))),
                },
                FakeReconciler {
                    observations: observations.clone(),
                    inspection,
                    reconciliation,
                },
                FakeWorkspaceFileSystem {
                    observations: observations.clone(),
                    target_exists,
                    workspace_structure,
                    existing_source,
                    candidate_fingerprint: [candidate_fingerprint; 32],
                },
                FakePublisher {
                    observations: observations.clone(),
                    discard_error: Arc::new(Mutex::new(None)),
                },
                CooperativeCancellation::default(),
            ),
            observations,
        )
    }

    #[tokio::test]
    async fn first_init_builds_candidate_database_then_publishes_create_new() {
        let candidate_state = database_state(0x22, Vec::new());
        let (service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x22,
            Ok(candidate_state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(
                false,
                candidate_state,
            )),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("首次 Init 应创建工作区");

        assert_eq!(outcome, ProjectWorkspaceConvergence::Created);
        assert_eq!(
            observations.events(),
            vec![
                "lease_acquire",
                "game_root",
                "workspace_root",
                "prepare",
                "fingerprint_candidate",
                "create_database",
                "reconcile_database",
                "publish",
                "lease_drop",
            ]
        );
        let stage_requests = observations
            .stage_requests
            .lock()
            .expect("stage requests mutex should not be poisoned");
        assert_eq!(
            stage_requests[0].publish_intent(),
            DirectoryPublishIntent::CreateNew
        );
        assert_eq!(
            stage_requests[0].target_root(),
            Path::new("C:/projects/game")
        );
        assert_eq!(stage_requests[0].source_mappings().len(), 2);
        assert_eq!(
            stage_requests[0].source_mappings()[0].source_directory(),
            Path::new("C:/games/source/data")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[0].relative_target(),
            Path::new("source/data")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[1].source_directory(),
            Path::new("C:/games/source/js")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[1].relative_target(),
            Path::new("source/js")
        );
        assert_eq!(
            stage_requests[0].empty_directories(),
            &[
                PathBuf::from("write_back/data"),
                PathBuf::from("write_back/js")
            ]
        );
        let created = observations
            .created_projects
            .lock()
            .expect("created projects mutex should not be poisoned");
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].0,
            Path::new("C:/projects/.game-stage/project.db")
        );
        assert_eq!(
            created[0].1.source_snapshot_fingerprint(),
            fingerprint(0x22)
        );
        let reconciled = observations
            .reconciled_projects
            .lock()
            .expect("reconciled projects mutex should not be poisoned");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(
            reconciled[0].0,
            Path::new("C:/projects/.game-stage/project.db")
        );
        assert_eq!(
            reconciled[0].1.source_snapshot_fingerprint(),
            fingerprint(0x22)
        );
        assert!(
            observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn identical_existing_project_discards_candidate_and_preserves_output() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("完全相同的项目应成功 no-op");

        assert_eq!(outcome, ProjectWorkspaceConvergence::Unchanged);
        assert_eq!(
            observations.events(),
            vec![
                "lease_acquire",
                "game_root",
                "workspace_root",
                "inspect_database",
                "list_workspace",
                "list_source",
                "list_write_back",
                "fingerprint_existing",
                "output_data",
                "output_js",
                "prepare",
                "fingerprint_candidate",
                "snapshot_database",
                "reconcile_database",
                "discard",
                "lease_drop",
            ]
        );
        assert_eq!(
            observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")[0]
                .publish_intent(),
            DirectoryPublishIntent::ReplaceExisting
        );
        assert_eq!(
            observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .as_slice(),
            &[(
                PathBuf::from("C:/projects/game/project.db"),
                PathBuf::from("C:/projects/.game-stage/project.db"),
            )]
        );
    }

    #[tokio::test]
    async fn sqlite_sidecars_do_not_make_an_identical_workspace_look_changed() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::SqliteSidecars,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("SQLite sidecar 属于已检查数据库的存储语义");

        assert_eq!(outcome, ProjectWorkspaceConvergence::Unchanged);
        assert!(observations.events().contains(&"discard"));
        assert!(!observations.events().contains(&"publish"));
    }

    #[tokio::test]
    async fn exact_directory_publish_lock_does_not_make_an_identical_workspace_look_changed() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::DirectoryPublishLock,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("WriteBack 发布器留下的精确锁目录属于受管基础设施");

        assert_eq!(outcome, ProjectWorkspaceConvergence::Unchanged);
        assert!(
            observations
                .events()
                .contains(&"list_directory_publish_locks")
        );
        assert!(observations.events().contains(&"discard"));
        assert!(!observations.events().contains(&"publish"));
    }

    #[tokio::test]
    async fn changed_source_updates_workspace_and_reports_stale_owners() {
        let current = database_state(
            0x33,
            vec![(MzStandardAssetOwner::Builtin, fingerprint(0x33))],
        );
        let updated = database_state(
            0x44,
            vec![(MzStandardAssetOwner::Builtin, fingerprint(0x33))],
        );
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(true, updated)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("来源变化应发布更新");

        assert_eq!(
            outcome,
            ProjectWorkspaceConvergence::Updated {
                stale_owners: vec![MzStandardAssetOwner::Builtin],
            }
        );
        assert!(observations.events().contains(&"snapshot_database"));
        assert!(observations.events().contains(&"publish"));
        assert!(!observations.events().contains(&"discard"));
    }

    #[tokio::test]
    async fn non_exact_workspace_structure_is_repaired_even_when_requested_facts_match() {
        for structure in [
            WorkspaceStructureObservation::SqliteSidecarNotFile,
            WorkspaceStructureObservation::DirectoryPublishLockNamespaceNotDirectory,
            WorkspaceStructureObservation::DirectoryPublishLockFileNotRegular,
            WorkspaceStructureObservation::DirectoryPublishLockMissingFile,
            WorkspaceStructureObservation::DirectoryPublishLockExtraEntry,
            WorkspaceStructureObservation::DatabaseNotFile,
            WorkspaceStructureObservation::SourceNotDirectory,
            WorkspaceStructureObservation::SourceDataNotDirectory,
            WorkspaceStructureObservation::WriteBackDataNotDirectory,
            WorkspaceStructureObservation::ExtraWorkspaceEntry,
            WorkspaceStructureObservation::ExtraSourceEntry,
            WorkspaceStructureObservation::ExtraWriteBackEntry,
            WorkspaceStructureObservation::MissingSourceEntry,
            WorkspaceStructureObservation::WriteBackNotDirectory,
        ] {
            let current = database_state(0x55, Vec::new());
            let (service, observations) = service(
                true,
                structure,
                ExistingSourceObservation::Fingerprint([0x55; 32]),
                0x55,
                Ok(current.clone()),
                Ok(ProjectDatabaseReconciliation::for_test(false, current)),
                Ok(()),
            );

            let outcome = service
                .converge(request())
                .await
                .unwrap_or_else(|error| panic!("{structure:?} 应执行 repair：{error}"));

            assert_eq!(
                outcome,
                ProjectWorkspaceConvergence::Updated {
                    stale_owners: Vec::new(),
                },
                "{structure:?}"
            );
            assert!(observations.events().contains(&"publish"), "{structure:?}");
            assert!(!observations.events().contains(&"discard"), "{structure:?}");
            assert!(
                !observations.events().contains(&"fingerprint_existing"),
                "结构不完整时不应把其内容身份当作可复用事实：{structure:?}"
            );
        }
    }

    #[tokio::test]
    async fn workspace_structure_listing_io_failure_is_technical_error() {
        let current = database_state(0x55, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Io,
            ExistingSourceObservation::Fingerprint([0x55; 32]),
            0x55,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Ok(()),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("真实列举 I/O 失败不得被误判为可修复结构偏差");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::ObserveWorkspaceStructure(ListDirectoryError::Io {
                source: FakeError("list workspace"),
                ..
            })
        ));
        assert!(!observations.events().contains(&"prepare"));
    }

    #[tokio::test]
    async fn invalid_existing_database_stops_before_candidate_preparation() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Err(FakeError("invalid database")),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Ok(()),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("无效数据库绝不能被覆盖");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::InspectExistingDatabase(FakeError(
                "invalid database"
            ))
        ));
        assert_eq!(
            observations.events(),
            vec![
                "lease_acquire",
                "game_root",
                "workspace_root",
                "inspect_database",
                "lease_drop",
            ]
        );
    }

    #[tokio::test]
    async fn snapshot_failure_discards_candidate_once_and_preserves_primary_error() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Err(SnapshotDatabaseError::NotCreated(FakeError("backup"))),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("快照失败必须拒绝候选");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::CandidateFailure {
                failure: ProjectWorkspaceCandidateFailure::SnapshotDatabase(
                    SnapshotDatabaseError::NotCreated(FakeError("backup"))
                ),
                discard: None,
            }
        ));
        assert_eq!(
            observations
                .events()
                .into_iter()
                .filter(|event| *event == "discard")
                .count(),
            1
        );
        assert!(!observations.events().contains(&"reconcile_database"));
    }

    #[tokio::test]
    async fn candidate_failure_preserves_primary_and_discard_errors() {
        let current = database_state(0x33, Vec::new());
        let (service, _) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, current)),
            Err(SnapshotDatabaseError::NotCreated(FakeError("backup"))),
        );
        *service
            .directories
            .discard_error
            .lock()
            .expect("discard error mutex should not be poisoned") = Some(FakeError("discard"));

        let error = service
            .converge(request())
            .await
            .expect_err("快照与候选清理双重失败必须同时保留");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::CandidateFailure {
                failure: ProjectWorkspaceCandidateFailure::SnapshotDatabase(
                    SnapshotDatabaseError::NotCreated(FakeError("backup"))
                ),
                discard: Some(ref discard),
            } if discard.source() == &FakeError("discard")
                && discard.staging_root() == Path::new("C:/projects/.game-stage")
        ));
    }

    #[derive(Clone)]
    struct FakeWorkspaceConverger {
        requests: Arc<Mutex<Vec<ProjectWorkspaceConvergenceRequest>>>,
        responses: Arc<Mutex<VecDeque<Result<ProjectWorkspaceConvergence, FakeError>>>>,
    }

    impl ProjectWorkspaceConverger for FakeWorkspaceConverger {
        type Error = FakeError;

        async fn converge(
            &self,
            request: ProjectWorkspaceConvergenceRequest,
        ) -> Result<ProjectWorkspaceConvergence, Self::Error> {
            self.requests
                .lock()
                .expect("workspace requests mutex should not be poisoned")
                .push(request);
            self.responses
                .lock()
                .expect("workspace responses mutex should not be poisoned")
                .pop_front()
                .expect("测试必须提供响应")
        }
    }

    fn init_input() -> InitInput {
        InitInput {
            name: "game".parse().expect("项目名应合法"),
            game_root: PathBuf::from("./Game"),
            source_language: " ja ".to_owned(),
            target_language: " zh-Hans ".to_owned(),
            layout_profile: profile(),
        }
    }

    #[tokio::test]
    async fn init_service_normalizes_input_and_maps_updated_owner_result() {
        let converger = FakeWorkspaceConverger {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::from([Ok(
                ProjectWorkspaceConvergence::Updated {
                    stale_owners: vec![MzStandardAssetOwner::Builtin, MzStandardAssetOwner::Lua],
                },
            )]))),
        };
        let service = InitService::new(converger, CooperativeCancellation::default());

        let output = service
            .execute(init_input())
            .await
            .expect("Init 编排应成功");

        assert_eq!(output.name.as_str(), "game");
        assert_eq!(
            output.outcome,
            InitOutcome::Updated {
                stale_owners: vec![InitStaleOwner::Builtin, InitStaleOwner::Lua],
            }
        );
        let requests = service
            .workspace_converger
            .requests
            .lock()
            .expect("workspace requests mutex should not be poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source_game_root, Path::new("./Game"));
        assert_eq!(requests[0].source_language, "ja");
        assert_eq!(requests[0].target_language, "zh-Hans");
    }

    #[tokio::test]
    async fn blank_language_stops_before_workspace_convergence() {
        let converger = FakeWorkspaceConverger {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::new())),
        };
        let service = InitService::new(converger, CooperativeCancellation::default());
        let mut input = init_input();
        input.source_language = " \t ".to_owned();

        assert!(matches!(
            service.execute(input).await,
            Err(InitServiceError::EmptySourceLanguage)
        ));
        assert!(
            service
                .workspace_converger
                .requests
                .lock()
                .expect("workspace requests mutex should not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn init_and_workspace_futures_are_send() {
        fn assert_send(_: impl Send) {}

        let state = database_state(0x11, Vec::new());
        let (workspace, _) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x11,
            Ok(state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(false, state)),
            Ok(()),
        );
        assert_send(workspace.converge(request()));

        let init = InitService::new(
            FakeWorkspaceConverger {
                requests: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(
                    ProjectWorkspaceConvergence::Created,
                )]))),
            },
            CooperativeCancellation::default(),
        );
        assert_send(init.execute(init_input()));
    }
}
