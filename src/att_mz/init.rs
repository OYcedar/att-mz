use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;
use super::project::MzWriteBackLayoutProfile;
use crate::project_database::NewProject;
use crate::storage::file_system::{ExistingDirectoryResolver, ResolveDirectoryError};

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
#[allow(dead_code, reason = "真实工作区创建器将在后续任务进行生产装配")]
pub(crate) struct NewProjectWorkspace {
    source_game_root: PathBuf,
    project: NewProject,
}

#[allow(dead_code, reason = "真实工作区创建器将在后续任务进行生产装配")]
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

    /// 把导入来源与数据库 metadata 交给真实工作区实现。
    pub(crate) fn into_parts(self) -> (PathBuf, NewProject) {
        (self.source_game_root, self.project)
    }
}

/// 原子创建并发布一个冻结项目工作区的职责契约。
#[allow(dead_code, reason = "真实工作区创建器将在后续任务进行生产装配")]
pub(crate) trait ProjectWorkspaceCreator: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    /// 完整复制 `data`、`js`，建立数据库和写回目录，再共同发布工作区。
    ///
    /// 成功意味着 `<name>/project.db`、`source/{data,js}` 与
    /// `write_back/{data,js}` 已经作为同一个可用工作区对外可见；失败不得暴露
    /// 半成品。真实复制和暂存发布实现不属于本批范围。
    fn create(
        &self,
        project: NewProjectWorkspace,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 只负责初始化用例编排，不了解目录解析与数据库创建的内部机制。
#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
pub(crate) struct InitService<F, W> {
    file_system: F,
    workspace_creator: W,
}

#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
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

#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
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
#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
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
