#![allow(dead_code, reason = "初始化非根能力尚未接入生产组合根")]

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;
use super::project::MzWriteBackLayoutProfile;
use crate::project_database::{NewProject, ProjectDatabaseCreator, ProjectWorkspaceLayout};
use crate::storage::file_system::{
    AtomicDirectoryDiscardError, AtomicDirectoryPrepareError, AtomicDirectoryPublishError,
    AtomicDirectoryPublisher, DirectoryPublishMode, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, ExistingDirectoryResolver, ResolveDirectoryError,
};

/// 初始化 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitInput {
    pub name: ProjectName,
    pub game_root: PathBuf,
    pub source_language: String,
    pub target_language: String,
    pub layout_profile: MzWriteBackLayoutProfile,
}

/// 初始化成功后交还给 CLI 的最小结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitOutput {
    pub name: ProjectName,
}

/// 完成一个 MZ 游戏初始化用例。
pub trait InitUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: InitInput,
    ) -> impl Future<Output = Result<InitOutput, Self::Error>> + Send;
}

/// 创建完整冻结工作区所需的受信输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewProjectWorkspace {
    source_game_root: PathBuf,
    project: NewProject,
}

impl NewProjectWorkspace {
    pub(crate) fn new(source_game_root: PathBuf, project: NewProject) -> Self {
        Self {
            source_game_root,
            project,
        }
    }

    /// 返回仅用于本次导入的原游戏根目录。
    pub(crate) fn source_game_root(&self) -> &std::path::Path {
        &self.source_game_root
    }

    pub(crate) fn project(&self) -> &NewProject {
        &self.project
    }

    /// 把导入来源与数据库 metadata 交给工作区创建服务。
    pub(crate) fn into_parts(self) -> (PathBuf, NewProject) {
        (self.source_game_root, self.project)
    }
}

/// 原子创建并发布一个冻结项目工作区的职责契约。
pub(crate) trait ProjectWorkspaceCreator: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    /// 完整复制 `data`、`js`，建立数据库和写回目录，再共同发布工作区。
    ///
    /// 成功意味着 `<name>/project.db`、`source/{data,js}` 与
    /// `write_back/{data,js}` 已经作为同一个可用工作区对外可见；失败不得暴露
    /// 半成品。
    fn create(
        &self,
        project: NewProjectWorkspace,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把原游戏快照、项目数据库和空写回目录作为一个整体创建并发布。
pub(crate) struct ProjectWorkspaceCreationService<D, A> {
    projects_root: PathBuf,
    database: D,
    directories: A,
}

impl<D, A> ProjectWorkspaceCreationService<D, A> {
    /// 使用外部配置建立的项目根目录与两个直接依赖创建服务。
    pub(crate) fn new(projects_root: PathBuf, database: D, directories: A) -> Self {
        Self {
            projects_root,
            database,
            directories,
        }
    }
}

impl<D, A> ProjectWorkspaceCreator for ProjectWorkspaceCreationService<D, A>
where
    D: ProjectDatabaseCreator,
    A: AtomicDirectoryPublisher,
{
    type Error = ProjectWorkspaceCreationError<D::Error, A::Error>;

    async fn create(&self, workspace: NewProjectWorkspace) -> Result<(), Self::Error> {
        let (source_game_root, project) = workspace.into_parts();
        let final_layout = ProjectWorkspaceLayout::for_project(&self.projects_root, project.name());
        let request = DirectoryStageRequest::new(
            final_layout.workspace_root().to_path_buf(),
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
            .prepare(request)
            .await
            .map_err(ProjectWorkspaceCreationError::Prepare)?;
        let staged_layout =
            ProjectWorkspaceLayout::from_workspace_root(staged.staging_root().to_path_buf());

        if let Err(database) = self
            .database
            .create(staged_layout.database_path().to_path_buf(), project)
            .await
        {
            return match self.directories.discard(staged).await {
                Ok(()) => Err(ProjectWorkspaceCreationError::Database(database)),
                Err(discard) => {
                    Err(ProjectWorkspaceCreationError::DatabaseAndDiscard { database, discard })
                }
            };
        }

        self.directories
            .publish(staged, DirectoryPublishMode::CreateNew)
            .await
            .map_err(ProjectWorkspaceCreationError::Publish)
    }
}

/// 创建完整工作区时可以精确定位的失败阶段。
#[derive(Debug)]
pub(crate) enum ProjectWorkspaceCreationError<D, A> {
    InvalidStageRequest(DirectoryStageRequestError),
    Prepare(AtomicDirectoryPrepareError<A>),
    Database(D),
    DatabaseAndDiscard {
        database: D,
        discard: AtomicDirectoryDiscardError<A>,
    },
    Publish(AtomicDirectoryPublishError<A>),
}

impl<D, A> From<DirectoryStageRequestError> for ProjectWorkspaceCreationError<D, A> {
    fn from(error: DirectoryStageRequestError) -> Self {
        Self::InvalidStageRequest(error)
    }
}

impl<D, A> fmt::Display for ProjectWorkspaceCreationError<D, A>
where
    D: fmt::Display,
    A: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStageRequest(error) => {
                write!(formatter, "工作区暂存请求无效：{error}")
            }
            Self::Prepare(error) => write!(formatter, "无法准备工作区候选目录：{error}"),
            Self::Database(error) => write!(formatter, "无法创建项目数据库：{error}"),
            Self::DatabaseAndDiscard { database, discard } => write!(
                formatter,
                "无法创建项目数据库，且无法清理工作区候选目录：{database}；{discard}"
            ),
            Self::Publish(error) => write!(formatter, "无法发布完整工作区：{error}"),
        }
    }
}

impl<D, A> Error for ProjectWorkspaceCreationError<D, A>
where
    D: Error + 'static,
    A: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidStageRequest(error) => Some(error),
            Self::Prepare(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::DatabaseAndDiscard { database, .. } => Some(database),
            Self::Publish(error) => Some(error),
        }
    }
}

/// 只负责初始化用例编排，不了解目录解析与数据库创建的内部机制。
pub(crate) struct InitService<F, W> {
    file_system: F,
    workspace_creator: W,
}

impl<F, W> InitService<F, W> {
    pub(crate) fn new(file_system: F, workspace_creator: W) -> Self {
        Self {
            file_system,
            workspace_creator,
        }
    }
}

impl<F, W> InitUseCase for InitService<F, W>
where
    F: ExistingDirectoryResolver,
    W: ProjectWorkspaceCreator,
{
    type Error = InitServiceError<F::Error, W::Error>;

    async fn execute(&self, input: InitInput) -> Result<InitOutput, Self::Error> {
        let source_language =
            normalized_language(input.source_language, InitServiceError::EmptySourceLanguage)?;
        let target_language =
            normalized_language(input.target_language, InitServiceError::EmptyTargetLanguage)?;

        let source_game_root = self
            .file_system
            .resolve_existing_directory(input.game_root)
            .await
            .map_err(InitServiceError::GameRoot)?;

        let output_name = input.name.clone();
        let project = NewProject::new(
            input.name,
            source_language,
            target_language,
            input.layout_profile,
        );
        self.workspace_creator
            .create(NewProjectWorkspace::new(source_game_root, project))
            .await
            .map_err(InitServiceError::Workspace)?;

        Ok(InitOutput { name: output_name })
    }
}

fn normalized_language<F, W>(
    value: String,
    empty_error: InitServiceError<F, W>,
) -> Result<String, InitServiceError<F, W>> {
    let normalized = value.trim();
    if normalized.is_empty() {
        Err(empty_error)
    } else {
        Ok(normalized.to_owned())
    }
}

/// 初始化编排在本职责边界内能够产生的错误。
#[derive(Debug)]
pub(crate) enum InitServiceError<F, W> {
    EmptySourceLanguage,
    EmptyTargetLanguage,
    GameRoot(ResolveDirectoryError<F>),
    Workspace(W),
}

impl<F, W> fmt::Display for InitServiceError<F, W>
where
    F: Error,
    W: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySourceLanguage => formatter.write_str("源语言去除首尾空白后不能为空"),
            Self::EmptyTargetLanguage => formatter.write_str("目标语言去除首尾空白后不能为空"),
            Self::GameRoot(error) => write!(formatter, "无法使用游戏根目录：{error}"),
            Self::Workspace(error) => write!(formatter, "无法创建冻结项目工作区：{error}"),
        }
    }
}

impl<F, W> Error for InitServiceError<F, W>
where
    F: Error + 'static,
    W: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::GameRoot(error) => Some(error),
            Self::Workspace(error) => Some(error),
            Self::EmptySourceLanguage | Self::EmptyTargetLanguage => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Debug)]
    enum FileSystemOutcome {
        Resolved(PathBuf),
        NotFound,
        NotDirectory,
        Io,
    }

    #[derive(Clone)]
    struct FakeFileSystem {
        outcome: FileSystemOutcome,
        calls: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl FakeFileSystem {
        fn new(outcome: FileSystemOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ExistingDirectoryResolver for FakeFileSystem {
        type Error = FakeFileSystemError;

        async fn resolve_existing_directory(
            &self,
            path: PathBuf,
        ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
            self.calls
                .lock()
                .expect("文件系统调用记录锁不应中毒")
                .push(path.clone());

            match &self.outcome {
                FileSystemOutcome::Resolved(resolved) => Ok(resolved.clone()),
                FileSystemOutcome::NotFound => Err(ResolveDirectoryError::NotFound { path }),
                FileSystemOutcome::NotDirectory => {
                    Err(ResolveDirectoryError::NotDirectory { path })
                }
                FileSystemOutcome::Io => Err(ResolveDirectoryError::Io {
                    path,
                    source: FakeFileSystemError,
                }),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeFileSystemError;

    impl fmt::Display for FakeFileSystemError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake filesystem failure")
        }
    }

    impl Error for FakeFileSystemError {}

    #[derive(Clone)]
    struct FakeCreator {
        projects: Arc<Mutex<Vec<NewProjectWorkspace>>>,
        failure: bool,
    }

    impl FakeCreator {
        fn succeeding() -> Self {
            Self {
                projects: Arc::new(Mutex::new(Vec::new())),
                failure: false,
            }
        }

        fn failing() -> Self {
            Self {
                projects: Arc::new(Mutex::new(Vec::new())),
                failure: true,
            }
        }
    }

    impl ProjectWorkspaceCreator for FakeCreator {
        type Error = FakeCreatorError;

        async fn create(&self, project: NewProjectWorkspace) -> Result<(), Self::Error> {
            self.projects
                .lock()
                .expect("工作区创建调用记录锁不应中毒")
                .push(project);

            if self.failure {
                Err(FakeCreatorError)
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeCreatorError;

    impl fmt::Display for FakeCreatorError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake database creation failure")
        }
    }

    impl Error for FakeCreatorError {}

    fn input() -> InitInput {
        InitInput {
            name: "游戏 一".parse().expect("测试项目名称应该合法"),
            game_root: PathBuf::from("./Game One"),
            source_language: " ja ".to_owned(),
            target_language: " ja ".to_owned(),
            layout_profile: profile(),
        }
    }

    fn profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn width(value: u32) -> super::super::project::MaxFullwidthChars {
        super::super::project::MaxFullwidthChars::new(value).expect("测试宽度应该是正整数")
    }

    #[tokio::test]
    async fn resolves_game_root_and_creates_one_normalized_project() {
        let resolved_root = PathBuf::from("C:/Games/Game One");
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(resolved_root.clone())),
            FakeCreator::succeeding(),
        );

        let output = service.execute(input()).await.expect("初始化编排应该成功");

        assert_eq!(output.name.as_str(), "游戏 一");
        assert_eq!(
            service
                .file_system
                .calls
                .lock()
                .expect("文件系统调用记录锁不应中毒")
                .as_slice(),
            &[PathBuf::from("./Game One")]
        );

        let projects = service
            .workspace_creator
            .projects
            .lock()
            .expect("工作区创建调用记录锁不应中毒");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].source_game_root(), resolved_root);
        assert_eq!(projects[0].project().name().as_str(), "游戏 一");
        assert_eq!(projects[0].project().source_language(), "ja");
        assert_eq!(projects[0].project().target_language(), "ja");
        assert_eq!(projects[0].project().layout_profile(), &profile());
    }

    #[tokio::test]
    async fn rejects_blank_languages_before_any_dependency_call() {
        for (source_language, target_language, source_is_invalid) in
            [("   ", "zh-Hans", true), ("ja", "\t", false)]
        {
            let service = InitService::new(
                FakeFileSystem::new(FileSystemOutcome::Resolved(PathBuf::from("C:/Games/Demo"))),
                FakeCreator::succeeding(),
            );
            let mut request = input();
            request.source_language = source_language.to_owned();
            request.target_language = target_language.to_owned();

            let error = service
                .execute(request)
                .await
                .expect_err("空白语言应该被拒绝");

            assert_eq!(
                matches!(error, InitServiceError::EmptySourceLanguage),
                source_is_invalid
            );
            assert_eq!(
                matches!(error, InitServiceError::EmptyTargetLanguage),
                !source_is_invalid
            );
            assert!(
                service
                    .file_system
                    .calls
                    .lock()
                    .expect("文件系统调用记录锁不应中毒")
                    .is_empty()
            );
            assert!(
                service
                    .workspace_creator
                    .projects
                    .lock()
                    .expect("工作区创建调用记录锁不应中毒")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn preserves_each_directory_failure_without_calling_creator() {
        for (outcome, expected_kind) in [
            (FileSystemOutcome::NotFound, "not-found"),
            (FileSystemOutcome::NotDirectory, "not-directory"),
            (FileSystemOutcome::Io, "io"),
        ] {
            let service = InitService::new(FakeFileSystem::new(outcome), FakeCreator::succeeding());

            let error = service
                .execute(input())
                .await
                .expect_err("目录失败应该向上返回");
            let actual_kind = match error {
                InitServiceError::GameRoot(ResolveDirectoryError::NotFound { .. }) => "not-found",
                InitServiceError::GameRoot(ResolveDirectoryError::NotDirectory { .. }) => {
                    "not-directory"
                }
                InitServiceError::GameRoot(ResolveDirectoryError::Io { source, .. }) => {
                    assert_eq!(source, FakeFileSystemError);
                    "io"
                }
                other => panic!("未预期的初始化错误：{other}"),
            };

            assert_eq!(actual_kind, expected_kind);
            assert!(
                service
                    .workspace_creator
                    .projects
                    .lock()
                    .expect("工作区创建调用记录锁不应中毒")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn preserves_workspace_failure_after_one_call() {
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(PathBuf::from("C:/Games/Demo"))),
            FakeCreator::failing(),
        );

        let error = service
            .execute(input())
            .await
            .expect_err("工作区创建失败应该向上返回");

        assert!(matches!(
            error,
            InitServiceError::Workspace(FakeCreatorError)
        ));
        assert_eq!(
            service
                .workspace_creator
                .projects
                .lock()
                .expect("工作区创建调用记录锁不应中毒")
                .len(),
            1
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn passes_non_utf8_resolved_path_to_workspace_creator_losslessly() {
        let resolved_path = non_utf8_path();
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(resolved_path.clone())),
            FakeCreator::succeeding(),
        );

        service
            .execute(input())
            .await
            .expect("非 UTF-8 路径不应经过文本转换");

        assert_eq!(
            service
                .workspace_creator
                .projects
                .lock()
                .expect("工作区创建调用记录锁不应中毒")[0]
                .source_game_root(),
            resolved_path
        );
    }

    #[test]
    fn execution_future_is_send() {
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(PathBuf::from("C:/Games/Demo"))),
            FakeCreator::succeeding(),
        );

        assert_send(service.execute(input()));
    }

    fn assert_send(_: impl Send) {}

    #[cfg(windows)]
    fn non_utf8_path() -> PathBuf {
        use std::os::windows::ffi::OsStringExt;

        PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ]))
    }

    #[cfg(unix)]
    fn non_utf8_path() -> PathBuf {
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(vec![b'/', 0xff]))
    }
}

#[cfg(test)]
mod workspace_creation_service_tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use crate::project_database::{
        CreatedProject, ProjectDatabaseCreateError, ProjectDatabaseCreationService,
        ProjectDatabaseCreator,
    };
    use crate::storage::file_system::{StagedDirectory, StagingCleanupFailure};
    use crate::storage::sqlite::{
        CreateDatabaseError, SqliteCommand, SqliteDatabaseCreator, SqliteValue,
    };

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Resolve,
        Prepare,
        Database,
        Discard,
        Publish(DirectoryPublishMode),
    }

    #[derive(Debug, Default)]
    struct Trace {
        events: Vec<Event>,
        stage: Option<CapturedStageRequest>,
        database: Option<CapturedDatabaseCreation>,
        publication: Option<CapturedPublication>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedStageRequest {
        target_root: PathBuf,
        source_mappings: Vec<(PathBuf, PathBuf)>,
        overlay_count: usize,
        empty_directories: Vec<PathBuf>,
    }

    #[derive(Debug)]
    struct CapturedDatabaseCreation {
        path: PathBuf,
        commands: Vec<SqliteCommand>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedPublication {
        target_root: PathBuf,
        staging_root: PathBuf,
        mode: DirectoryPublishMode,
    }

    #[derive(Clone)]
    struct RecordingDirectoryResolver {
        resolved: PathBuf,
        trace: Arc<Mutex<Trace>>,
    }

    impl ExistingDirectoryResolver for RecordingDirectoryResolver {
        type Error = RootError;

        async fn resolve_existing_directory(
            &self,
            _path: PathBuf,
        ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
            self.trace
                .lock()
                .expect("记录锁不应中毒")
                .events
                .push(Event::Resolve);
            Ok(self.resolved.clone())
        }
    }

    #[derive(Clone)]
    struct RecordingSqliteCreator {
        trace: Arc<Mutex<Trace>>,
    }

    impl SqliteDatabaseCreator for RecordingSqliteCreator {
        type Error = RootError;

        async fn create_new_database(
            &self,
            path: PathBuf,
            commands: Vec<SqliteCommand>,
        ) -> Result<(), CreateDatabaseError<Self::Error>> {
            let mut trace = self.trace.lock().expect("记录锁不应中毒");
            trace.events.push(Event::Database);
            trace.database = Some(CapturedDatabaseCreation { path, commands });
            Ok(())
        }
    }

    struct RecordingAtomicPublisher {
        staging_root: PathBuf,
        trace: Arc<Mutex<Trace>>,
        prepare_fails: bool,
        prepare_cleanup_fails: bool,
        discard_fails: bool,
        publish_failure: Option<PublishFailure>,
    }

    impl AtomicDirectoryPublisher for RecordingAtomicPublisher {
        type Error = RootError;
        type StagingState = ();

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, AtomicDirectoryPrepareError<Self::Error>>
        {
            let target_root = request.target_root().to_path_buf();
            let captured = CapturedStageRequest {
                target_root: target_root.clone(),
                source_mappings: request
                    .source_mappings()
                    .iter()
                    .map(|mapping| {
                        (
                            mapping.source_directory().to_path_buf(),
                            mapping.relative_target().to_path_buf(),
                        )
                    })
                    .collect(),
                overlay_count: request.overlays().len(),
                empty_directories: request.empty_directories().to_vec(),
            };
            let mut trace = self.trace.lock().expect("记录锁不应中毒");
            trace.events.push(Event::Prepare);
            trace.stage = Some(captured);
            drop(trace);

            if self.prepare_fails {
                return Err(AtomicDirectoryPrepareError::NotPrepared {
                    target_root,
                    source: RootError,
                    cleanup_failure: self
                        .prepare_cleanup_fails
                        .then(|| StagingCleanupFailure::new(self.staging_root.clone(), RootError)),
                });
            }

            Ok(StagedDirectory::new(
                target_root,
                self.staging_root.clone(),
                (),
            ))
        }

        async fn publish(
            &self,
            staged: StagedDirectory<Self::StagingState>,
            mode: DirectoryPublishMode,
        ) -> Result<(), AtomicDirectoryPublishError<Self::Error>> {
            let mut trace = self.trace.lock().expect("记录锁不应中毒");
            trace.events.push(Event::Publish(mode));
            trace.publication = Some(CapturedPublication {
                target_root: staged.target_root().to_path_buf(),
                staging_root: staged.staging_root().to_path_buf(),
                mode,
            });
            let target_root = staged.target_root().to_path_buf();
            match self.publish_failure {
                None => Ok(()),
                Some(PublishFailure::TargetAlreadyExists) => {
                    Err(AtomicDirectoryPublishError::TargetAlreadyExists {
                        target_root,
                        cleanup_failure: None,
                    })
                }
                Some(PublishFailure::NotPublished) => {
                    Err(AtomicDirectoryPublishError::NotPublished {
                        target_root,
                        source: RootError,
                        cleanup_failure: None,
                    })
                }
                Some(PublishFailure::PublishedButCleanupFailed) => {
                    Err(AtomicDirectoryPublishError::PublishedButCleanupFailed {
                        target_root,
                        residual_path: PathBuf::from("C:/ATT/projects/.old-demo"),
                        source: RootError,
                    })
                }
                Some(PublishFailure::OutcomeUnknown) => {
                    Err(AtomicDirectoryPublishError::OutcomeUnknown {
                        target_root,
                        recovery_artifacts: vec![PathBuf::from("C:/ATT/projects/.recovery-demo")],
                        source: RootError,
                    })
                }
            }
        }

        async fn discard(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), AtomicDirectoryDiscardError<Self::Error>> {
            self.trace
                .lock()
                .expect("记录锁不应中毒")
                .events
                .push(Event::Discard);
            if self.discard_fails {
                Err(AtomicDirectoryDiscardError::new(
                    self.staging_root.clone(),
                    RootError,
                ))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PublishFailure {
        TargetAlreadyExists,
        NotPublished,
        PublishedButCleanupFailed,
        OutcomeUnknown,
    }

    struct FailingDatabaseCreator {
        trace: Arc<Mutex<Trace>>,
    }

    impl ProjectDatabaseCreator for FailingDatabaseCreator {
        type Error = DatabaseError;

        async fn create(
            &self,
            _destination_path: PathBuf,
            _project: NewProject,
        ) -> Result<CreatedProject, Self::Error> {
            self.trace
                .lock()
                .expect("记录锁不应中毒")
                .events
                .push(Event::Database);
            Err(DatabaseError)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct RootError;

    impl fmt::Display for RootError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试根失败")
        }
    }

    impl Error for RootError {}

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct DatabaseError;

    impl fmt::Display for DatabaseError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试建库失败")
        }
    }

    impl Error for DatabaseError {}

    struct ScriptedDatabaseCreator {
        trace: Arc<Mutex<Trace>>,
        error: Mutex<Option<ProjectDatabaseCreateError<RootError>>>,
    }

    impl ProjectDatabaseCreator for ScriptedDatabaseCreator {
        type Error = ProjectDatabaseCreateError<RootError>;

        async fn create(
            &self,
            _destination_path: PathBuf,
            _project: NewProject,
        ) -> Result<CreatedProject, Self::Error> {
            self.trace
                .lock()
                .expect("记录锁不应中毒")
                .events
                .push(Event::Database);
            Err(self
                .error
                .lock()
                .expect("建库响应锁不应中毒")
                .take()
                .expect("测试应为每次建库提供响应"))
        }
    }

    #[derive(Clone, Default)]
    struct SuccessfulDatabaseCreator {
        paths: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl ProjectDatabaseCreator for SuccessfulDatabaseCreator {
        type Error = DatabaseError;

        async fn create(
            &self,
            destination_path: PathBuf,
            _project: NewProject,
        ) -> Result<CreatedProject, Self::Error> {
            self.paths
                .lock()
                .expect("建库路径记录锁不应中毒")
                .push(destination_path.clone());
            tokio::task::yield_now().await;
            Ok(CreatedProject::new(destination_path))
        }
    }

    #[derive(Debug)]
    struct LinearizingStageState {
        owner_id: u64,
        target_root: PathBuf,
        sequence: usize,
    }

    #[derive(Debug, Default)]
    struct LinearizingState {
        next_sequence: usize,
        published_targets: HashSet<PathBuf>,
        published_tokens: Vec<(PathBuf, usize)>,
    }

    #[derive(Clone)]
    struct LinearizingAtomicPublisher {
        owner_id: u64,
        state: Arc<Mutex<LinearizingState>>,
    }

    impl LinearizingAtomicPublisher {
        fn new(owner_id: u64) -> Self {
            Self {
                owner_id,
                state: Arc::new(Mutex::new(LinearizingState::default())),
            }
        }
    }

    impl AtomicDirectoryPublisher for LinearizingAtomicPublisher {
        type Error = RootError;
        type StagingState = LinearizingStageState;

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, AtomicDirectoryPrepareError<Self::Error>>
        {
            let target_root = request.target_root().to_path_buf();
            let sequence = {
                let mut state = self.state.lock().expect("线性化状态锁不应中毒");
                let sequence = state.next_sequence;
                state.next_sequence += 1;
                sequence
            };
            let staging_root = target_root.with_extension(format!("att-stage-{sequence}"));
            tokio::task::yield_now().await;
            Ok(StagedDirectory::new(
                target_root.clone(),
                staging_root,
                LinearizingStageState {
                    owner_id: self.owner_id,
                    target_root,
                    sequence,
                },
            ))
        }

        async fn publish(
            &self,
            staged: StagedDirectory<Self::StagingState>,
            mode: DirectoryPublishMode,
        ) -> Result<(), AtomicDirectoryPublishError<Self::Error>> {
            let (target_root, _staging_root, token) = staged.into_parts();
            if token.owner_id != self.owner_id || token.target_root != target_root {
                return Err(AtomicDirectoryPublishError::NotPublished {
                    target_root,
                    source: RootError,
                    cleanup_failure: None,
                });
            }
            assert_eq!(mode, DirectoryPublishMode::CreateNew);

            let mut state = self.state.lock().expect("线性化状态锁不应中毒");
            if !state.published_targets.insert(target_root.clone()) {
                return Err(AtomicDirectoryPublishError::TargetAlreadyExists {
                    target_root,
                    cleanup_failure: None,
                });
            }
            state.published_tokens.push((target_root, token.sequence));
            Ok(())
        }

        async fn discard(
            &self,
            staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), AtomicDirectoryDiscardError<Self::Error>> {
            let (target_root, staging_root, token) = staged.into_parts();
            if token.owner_id == self.owner_id && token.target_root == target_root {
                Ok(())
            } else {
                Err(AtomicDirectoryDiscardError::new(staging_root, RootError))
            }
        }
    }

    #[tokio::test]
    async fn full_non_root_chain_stages_database_and_publishes_one_complete_workspace() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let service = InitService::new(
            RecordingDirectoryResolver {
                resolved: PathBuf::from("C:/Games/Game One"),
                trace: Arc::clone(&trace),
            },
            ProjectWorkspaceCreationService::new(
                PathBuf::from("C:/ATT/projects"),
                ProjectDatabaseCreationService::new(RecordingSqliteCreator {
                    trace: Arc::clone(&trace),
                }),
                atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-game-one"),
            ),
        );

        let output = service
            .execute(init_input())
            .await
            .expect("完整非根链应该创建工作区");

        assert_eq!(output.name.as_str(), "游戏 一");
        let trace = trace.lock().expect("记录锁不应中毒");
        assert_eq!(
            trace.events,
            [
                Event::Resolve,
                Event::Prepare,
                Event::Database,
                Event::Publish(DirectoryPublishMode::CreateNew),
            ]
        );
        assert_eq!(
            trace.stage.as_ref(),
            Some(&CapturedStageRequest {
                target_root: PathBuf::from("C:/ATT/projects/游戏 一"),
                source_mappings: vec![
                    (
                        PathBuf::from("C:/Games/Game One/data"),
                        PathBuf::from("source/data"),
                    ),
                    (
                        PathBuf::from("C:/Games/Game One/js"),
                        PathBuf::from("source/js"),
                    ),
                ],
                overlay_count: 0,
                empty_directories: vec![
                    PathBuf::from("write_back/data"),
                    PathBuf::from("write_back/js"),
                ],
            })
        );

        let database = trace.database.as_ref().expect("应该创建数据库");
        assert_eq!(
            database.path,
            PathBuf::from("C:/ATT/projects/.stage-game-one/project.db")
        );
        assert_eq!(database.commands.len(), 2);
        assert_eq!(
            database.commands[1].parameters(),
            &[
                SqliteValue::Text("游戏 一".to_owned()),
                SqliteValue::Text("ja".to_owned()),
                SqliteValue::Text("zh-Hans".to_owned()),
                SqliteValue::Integer(24),
                SqliteValue::Integer(30),
                SqliteValue::Integer(18),
            ]
        );
        assert_eq!(
            trace.publication.as_ref(),
            Some(&CapturedPublication {
                target_root: PathBuf::from("C:/ATT/projects/游戏 一"),
                staging_root: PathBuf::from("C:/ATT/projects/.stage-game-one"),
                mode: DirectoryPublishMode::CreateNew,
            })
        );
    }

    #[tokio::test]
    async fn concurrent_same_name_init_is_linearized_only_by_create_new_publish() {
        let root = LinearizingAtomicPublisher::new(7);
        let database = SuccessfulDatabaseCreator::default();
        let trace = Arc::new(Mutex::new(Trace::default()));
        let first = concurrent_init_service(root.clone(), database.clone(), Arc::clone(&trace));
        let second = concurrent_init_service(root.clone(), database.clone(), trace);

        let (first_result, second_result) =
            tokio::join!(first.execute(init_input()), second.execute(init_input()));
        let results = [first_result, second_result];

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        Err(InitServiceError::Workspace(
                            ProjectWorkspaceCreationError::Publish(
                                AtomicDirectoryPublishError::TargetAlreadyExists { .. }
                            )
                        ))
                    )
                })
                .count(),
            1
        );
        assert_eq!(
            database.paths.lock().expect("建库路径记录锁不应中毒").len(),
            2,
            "两个候选都应在唯一线性化点之前完成建库"
        );
        let state = root.state.lock().expect("线性化状态锁不应中毒");
        assert_eq!(state.published_targets.len(), 1);
        assert!(
            state
                .published_targets
                .contains(&PathBuf::from("C:/ATT/projects/游戏 一"))
        );
    }

    #[tokio::test]
    async fn concurrent_different_projects_publish_only_their_own_tokens() {
        let root = LinearizingAtomicPublisher::new(11);
        let database = SuccessfulDatabaseCreator::default();
        let trace = Arc::new(Mutex::new(Trace::default()));
        let first = concurrent_init_service(root.clone(), database.clone(), Arc::clone(&trace));
        let second = concurrent_init_service(root.clone(), database, trace);
        let first_input = init_input();
        let mut second_input = init_input();
        second_input.name = "游戏 二".parse().expect("测试项目名应该合法");

        let (first_result, second_result) =
            tokio::join!(first.execute(first_input), second.execute(second_input));

        first_result.expect("第一个独立项目应该发布成功");
        second_result.expect("第二个独立项目应该发布成功");
        let state = root.state.lock().expect("线性化状态锁不应中毒");
        assert_eq!(state.published_targets.len(), 2);
        assert_eq!(state.published_tokens.len(), 2);
        assert!(
            state
                .published_targets
                .contains(&PathBuf::from("C:/ATT/projects/游戏 一"))
        );
        assert!(
            state
                .published_targets
                .contains(&PathBuf::from("C:/ATT/projects/游戏 二"))
        );
    }

    #[tokio::test]
    async fn staged_token_is_rejected_by_a_different_publisher_instance() {
        let first_root = LinearizingAtomicPublisher::new(21);
        let second_root = LinearizingAtomicPublisher::new(22);
        let request = DirectoryStageRequest::new(
            PathBuf::from("C:/ATT/projects/demo"),
            vec![
                DirectorySourceMapping::new(
                    PathBuf::from("C:/Games/Demo/data"),
                    PathBuf::from("source/data"),
                )
                .expect("测试来源映射应该合法"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect("测试候选请求应该合法");
        let staged = first_root
            .prepare(request.clone())
            .await
            .expect("第一个根应该准备候选");

        let error = second_root
            .publish(staged, DirectoryPublishMode::CreateNew)
            .await
            .expect_err("候选 token 不得交给另一个根实例");

        assert!(matches!(
            error,
            AtomicDirectoryPublishError::NotPublished {
                target_root,
                cleanup_failure: None,
                ..
            } if target_root == std::path::Path::new("C:/ATT/projects/demo")
        ));

        let staged = first_root
            .prepare(request)
            .await
            .expect("第一个根应该准备另一个候选");
        let staging_root = staged.staging_root().to_path_buf();
        let error = second_root
            .discard(staged)
            .await
            .expect_err("候选 token 也不得由另一个根实例丢弃");
        assert_eq!(error.staging_root(), staging_root);
        assert!(
            first_root
                .state
                .lock()
                .expect("线性化状态锁不应中毒")
                .published_targets
                .is_empty()
        );
        assert!(
            second_root
                .state
                .lock()
                .expect("线性化状态锁不应中毒")
                .published_targets
                .is_empty()
        );
    }

    #[tokio::test]
    async fn database_failure_discards_once_and_never_publishes() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let service = ProjectWorkspaceCreationService::new(
            PathBuf::from("C:/ATT/projects"),
            FailingDatabaseCreator {
                trace: Arc::clone(&trace),
            },
            atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-demo"),
        );

        let error = service
            .create(workspace())
            .await
            .expect_err("建库失败应该使工作区创建失败");

        assert!(matches!(
            error,
            ProjectWorkspaceCreationError::Database(DatabaseError)
        ));
        assert_eq!(
            trace.lock().expect("记录锁不应中毒").events,
            [Event::Prepare, Event::Database, Event::Discard]
        );
    }

    #[tokio::test]
    async fn prepare_failure_stops_before_database_publish_and_discard() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut directories = atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-demo");
        directories.prepare_fails = true;
        let service = ProjectWorkspaceCreationService::new(
            PathBuf::from("C:/ATT/projects"),
            FailingDatabaseCreator {
                trace: Arc::clone(&trace),
            },
            directories,
        );

        let error = service
            .create(workspace())
            .await
            .expect_err("候选目录未准备时应该立即失败");

        assert!(matches!(
            error,
            ProjectWorkspaceCreationError::Prepare(AtomicDirectoryPrepareError::NotPrepared { .. })
        ));
        assert_eq!(
            trace.lock().expect("记录锁不应中毒").events,
            [Event::Prepare]
        );
    }

    #[tokio::test]
    async fn prepare_failure_preserves_candidate_cleanup_failure_without_extra_discard() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let staging_root = PathBuf::from("C:/ATT/projects/.stage-demo");
        let mut directories = atomic_publisher(
            Arc::clone(&trace),
            staging_root.to_str().expect("测试路径应为 UTF-8"),
        );
        directories.prepare_fails = true;
        directories.prepare_cleanup_fails = true;
        let service = ProjectWorkspaceCreationService::new(
            PathBuf::from("C:/ATT/projects"),
            FailingDatabaseCreator {
                trace: Arc::clone(&trace),
            },
            directories,
        );

        let error = service
            .create(workspace())
            .await
            .expect_err("候选准备与内部清理双重失败应该一起返回");

        let ProjectWorkspaceCreationError::Prepare(AtomicDirectoryPrepareError::NotPrepared {
            cleanup_failure: Some(cleanup_failure),
            ..
        }) = error
        else {
            panic!("准备错误应该保留根返回的候选清理失败")
        };
        assert_eq!(cleanup_failure.residual_path(), staging_root);
        assert_eq!(*cleanup_failure.source(), RootError);
        assert_eq!(
            trace.lock().expect("记录锁不应中毒").events,
            [Event::Prepare]
        );
    }

    #[tokio::test]
    async fn every_database_terminal_error_discards_once_and_preserves_its_kind() {
        let database_path = PathBuf::from("C:/ATT/projects/.stage-demo/project.db");
        let cases = [
            (
                ProjectDatabaseCreateError::AlreadyExists {
                    path: database_path.clone(),
                },
                DatabaseFailureKind::AlreadyExists,
            ),
            (
                ProjectDatabaseCreateError::NotCreated {
                    path: database_path.clone(),
                    source: RootError,
                },
                DatabaseFailureKind::NotCreated,
            ),
            (
                ProjectDatabaseCreateError::OutcomeUnknown {
                    path: database_path.clone(),
                    source: RootError,
                },
                DatabaseFailureKind::OutcomeUnknown,
            ),
            (
                ProjectDatabaseCreateError::ResidualArtifact {
                    path: database_path,
                    source: RootError,
                },
                DatabaseFailureKind::ResidualArtifact,
            ),
        ];

        for (database_error, expected_kind) in cases {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let service = ProjectWorkspaceCreationService::new(
                PathBuf::from("C:/ATT/projects"),
                ScriptedDatabaseCreator {
                    trace: Arc::clone(&trace),
                    error: Mutex::new(Some(database_error)),
                },
                atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-demo"),
            );

            let error = service
                .create(workspace())
                .await
                .expect_err("建库终态错误应该使工作区创建失败");
            let ProjectWorkspaceCreationError::Database(error) = error else {
                panic!("建库错误且清理成功时应保留原建库错误")
            };

            assert_eq!(DatabaseFailureKind::from(&error), expected_kind);
            assert_eq!(
                trace.lock().expect("记录锁不应中毒").events,
                [Event::Prepare, Event::Database, Event::Discard]
            );
        }
    }

    #[tokio::test]
    async fn database_and_discard_failures_are_both_preserved() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut directories = atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-demo");
        directories.discard_fails = true;
        let service = ProjectWorkspaceCreationService::new(
            PathBuf::from("C:/ATT/projects"),
            FailingDatabaseCreator {
                trace: Arc::clone(&trace),
            },
            directories,
        );

        let error = service
            .create(workspace())
            .await
            .expect_err("建库与清理双重失败应该返回");

        let ProjectWorkspaceCreationError::DatabaseAndDiscard { database, discard } = error else {
            panic!("应该同时保留建库与清理失败")
        };
        assert_eq!(database, DatabaseError);
        assert_eq!(
            discard.staging_root(),
            PathBuf::from("C:/ATT/projects/.stage-demo")
        );
        assert_eq!(*discard.source(), RootError);
        assert_eq!(
            trace.lock().expect("记录锁不应中毒").events,
            [Event::Prepare, Event::Database, Event::Discard]
        );
    }

    #[tokio::test]
    async fn publish_terminal_errors_are_preserved_without_second_cleanup() {
        for publish_failure in [
            PublishFailure::TargetAlreadyExists,
            PublishFailure::NotPublished,
            PublishFailure::PublishedButCleanupFailed,
            PublishFailure::OutcomeUnknown,
        ] {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let mut directories =
                atomic_publisher(Arc::clone(&trace), "C:/ATT/projects/.stage-demo");
            directories.publish_failure = Some(publish_failure);
            let service = ProjectWorkspaceCreationService::new(
                PathBuf::from("C:/ATT/projects"),
                ProjectDatabaseCreationService::new(RecordingSqliteCreator {
                    trace: Arc::clone(&trace),
                }),
                directories,
            );

            let error = service
                .create(workspace())
                .await
                .expect_err("发布终态错误应该使工作区创建失败");
            let ProjectWorkspaceCreationError::Publish(error) = error else {
                panic!("应该保留发布终态错误")
            };

            assert_eq!(PublishFailure::from(&error), publish_failure);
            assert_eq!(
                trace.lock().expect("记录锁不应中毒").events,
                [
                    Event::Prepare,
                    Event::Database,
                    Event::Publish(DirectoryPublishMode::CreateNew),
                ]
            );
        }
    }

    #[test]
    fn workspace_creation_future_is_send() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let service = ProjectWorkspaceCreationService::new(
            PathBuf::from("C:/ATT/projects"),
            FailingDatabaseCreator {
                trace: Arc::clone(&trace),
            },
            atomic_publisher(trace, "C:/ATT/projects/.stage-demo"),
        );

        assert_send(service.create(workspace()));
    }

    fn init_input() -> InitInput {
        InitInput {
            name: "游戏 一".parse().expect("测试项目名应该合法"),
            game_root: PathBuf::from("./Game One"),
            source_language: " ja ".to_owned(),
            target_language: " zh-Hans ".to_owned(),
            layout_profile: layout_profile(),
        }
    }

    fn concurrent_init_service(
        directories: LinearizingAtomicPublisher,
        database: SuccessfulDatabaseCreator,
        trace: Arc<Mutex<Trace>>,
    ) -> InitService<
        RecordingDirectoryResolver,
        ProjectWorkspaceCreationService<SuccessfulDatabaseCreator, LinearizingAtomicPublisher>,
    > {
        InitService::new(
            RecordingDirectoryResolver {
                resolved: PathBuf::from("C:/Games/Game One"),
                trace,
            },
            ProjectWorkspaceCreationService::new(
                PathBuf::from("C:/ATT/projects"),
                database,
                directories,
            ),
        )
    }

    fn workspace() -> NewProjectWorkspace {
        NewProjectWorkspace::new(
            PathBuf::from("C:/Games/Demo"),
            NewProject::new(
                "demo".parse().expect("测试项目名应该合法"),
                "ja".to_owned(),
                "zh-Hans".to_owned(),
                layout_profile(),
            ),
        )
    }

    fn layout_profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn width(value: u32) -> super::super::project::MaxFullwidthChars {
        super::super::project::MaxFullwidthChars::new(value).expect("测试宽度应该是正整数")
    }

    fn assert_send(_: impl Send) {}

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DatabaseFailureKind {
        AlreadyExists,
        NotCreated,
        OutcomeUnknown,
        ResidualArtifact,
    }

    impl<E> From<&ProjectDatabaseCreateError<E>> for DatabaseFailureKind {
        fn from(error: &ProjectDatabaseCreateError<E>) -> Self {
            match error {
                ProjectDatabaseCreateError::AlreadyExists { .. } => Self::AlreadyExists,
                ProjectDatabaseCreateError::NotCreated { .. } => Self::NotCreated,
                ProjectDatabaseCreateError::OutcomeUnknown { .. } => Self::OutcomeUnknown,
                ProjectDatabaseCreateError::ResidualArtifact { .. } => Self::ResidualArtifact,
            }
        }
    }

    impl From<&AtomicDirectoryPublishError<RootError>> for PublishFailure {
        fn from(error: &AtomicDirectoryPublishError<RootError>) -> Self {
            match error {
                AtomicDirectoryPublishError::TargetAlreadyExists { .. } => {
                    Self::TargetAlreadyExists
                }
                AtomicDirectoryPublishError::NotPublished { .. } => Self::NotPublished,
                AtomicDirectoryPublishError::PublishedButCleanupFailed { .. } => {
                    Self::PublishedButCleanupFailed
                }
                AtomicDirectoryPublishError::OutcomeUnknown { .. } => Self::OutcomeUnknown,
                AtomicDirectoryPublishError::TargetMissing { .. }
                | AtomicDirectoryPublishError::TargetNotDirectory { .. } => {
                    panic!("CreateNew 根不应返回 Replace 专属终态")
                }
            }
        }
    }

    fn atomic_publisher(trace: Arc<Mutex<Trace>>, staging_root: &str) -> RecordingAtomicPublisher {
        RecordingAtomicPublisher {
            staging_root: PathBuf::from(staging_root),
            trace,
            prepare_fails: false,
            prepare_cleanup_fails: false,
            discard_fails: false,
            publish_failure: None,
        }
    }
}
