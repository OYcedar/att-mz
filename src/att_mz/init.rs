use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;
use crate::project_database::{NewProject, ProjectDatabaseCreator};
use crate::storage::file_system::{ExistingDirectoryResolver, ResolveDirectoryError};

/// 初始化 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitInput {
    pub name: ProjectName,
    pub game_root: PathBuf,
    pub source_language: String,
    pub target_language: String,
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

/// 只负责初始化用例编排，不了解目录解析与数据库创建的内部机制。
#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
pub(crate) struct InitService<F, D> {
    file_system: F,
    database_creator: D,
}

#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
impl<F, D> InitService<F, D> {
    pub(crate) fn new(file_system: F, database_creator: D) -> Self {
        Self {
            file_system,
            database_creator,
        }
    }
}

impl<F, D> InitUseCase for InitService<F, D>
where
    F: ExistingDirectoryResolver,
    D: ProjectDatabaseCreator,
{
    type Error = InitServiceError<F::Error, D::Error>;

    async fn execute(&self, input: InitInput) -> Result<InitOutput, Self::Error> {
        let source_language =
            normalized_language(input.source_language, InitServiceError::EmptySourceLanguage)?;
        let target_language =
            normalized_language(input.target_language, InitServiceError::EmptyTargetLanguage)?;

        let game_root = self
            .file_system
            .resolve_existing_directory(input.game_root)
            .await
            .map_err(InitServiceError::GameRoot)?;
        let game_root_text = game_root
            .to_str()
            .ok_or_else(|| InitServiceError::NonUtf8GameRoot(game_root.clone()))?
            .to_owned();

        let output_name = input.name.clone();
        let project = NewProject::new(input.name, game_root_text, source_language, target_language);
        self.database_creator
            .create(project)
            .await
            .map_err(InitServiceError::Database)?;

        Ok(InitOutput { name: output_name })
    }
}

#[allow(dead_code, reason = "初始化服务按计划先实现但暂不进行生产装配")]
fn normalized_language<F, D>(
    value: String,
    empty_error: InitServiceError<F, D>,
) -> Result<String, InitServiceError<F, D>> {
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
pub(crate) enum InitServiceError<F, D> {
    EmptySourceLanguage,
    EmptyTargetLanguage,
    GameRoot(ResolveDirectoryError<F>),
    NonUtf8GameRoot(PathBuf),
    Database(D),
}

impl<F, D> fmt::Display for InitServiceError<F, D>
where
    F: Error,
    D: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySourceLanguage => formatter.write_str("源语言去除首尾空白后不能为空"),
            Self::EmptyTargetLanguage => formatter.write_str("目标语言去除首尾空白后不能为空"),
            Self::GameRoot(error) => write!(formatter, "无法使用游戏根目录：{error}"),
            Self::NonUtf8GameRoot(path) => {
                write!(
                    formatter,
                    "游戏根目录无法无损表示为 UTF-8：{}",
                    path.display()
                )
            }
            Self::Database(error) => write!(formatter, "无法创建项目数据库：{error}"),
        }
    }
}

impl<F, D> Error for InitServiceError<F, D>
where
    F: Error + 'static,
    D: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::GameRoot(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::EmptySourceLanguage | Self::EmptyTargetLanguage | Self::NonUtf8GameRoot(_) => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    use crate::project_database::CreatedProject;

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
        projects: Arc<Mutex<Vec<NewProject>>>,
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

    impl ProjectDatabaseCreator for FakeCreator {
        type Error = FakeCreatorError;

        async fn create(&self, project: NewProject) -> Result<CreatedProject, Self::Error> {
            self.projects
                .lock()
                .expect("数据库创建调用记录锁不应中毒")
                .push(project);

            if self.failure {
                Err(FakeCreatorError)
            } else {
                Ok(CreatedProject::new(PathBuf::from("C:/databases/demo.db")))
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
        }
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
            .database_creator
            .projects
            .lock()
            .expect("数据库创建调用记录锁不应中毒");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name().as_str(), "游戏 一");
        assert_eq!(
            projects[0].game_root(),
            resolved_root.to_str().expect("测试规范目录应该是 UTF-8")
        );
        assert_eq!(projects[0].source_language(), "ja");
        assert_eq!(projects[0].target_language(), "ja");
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
                    .database_creator
                    .projects
                    .lock()
                    .expect("数据库创建调用记录锁不应中毒")
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
                    .database_creator
                    .projects
                    .lock()
                    .expect("数据库创建调用记录锁不应中毒")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn preserves_creator_failure_after_one_call() {
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(PathBuf::from("C:/Games/Demo"))),
            FakeCreator::failing(),
        );

        let error = service
            .execute(input())
            .await
            .expect_err("数据库创建失败应该向上返回");

        assert!(matches!(
            error,
            InitServiceError::Database(FakeCreatorError)
        ));
        assert_eq!(
            service
                .database_creator
                .projects
                .lock()
                .expect("数据库创建调用记录锁不应中毒")
                .len(),
            1
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn rejects_non_utf8_resolved_path_without_calling_creator() {
        let service = InitService::new(
            FakeFileSystem::new(FileSystemOutcome::Resolved(non_utf8_path())),
            FakeCreator::succeeding(),
        );

        let error = service
            .execute(input())
            .await
            .expect_err("非 UTF-8 规范路径应该被拒绝");

        assert!(matches!(error, InitServiceError::NonUtf8GameRoot(_)));
        assert!(
            service
                .database_creator
                .projects
                .lock()
                .expect("数据库创建调用记录锁不应中毒")
                .is_empty()
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
