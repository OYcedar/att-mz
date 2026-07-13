#![allow(dead_code, reason = "项目开启服务尚未进行生产装配")]

//! MZ 命令域共享的现存项目上下文与开启职责。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::ProjectName;
use crate::project_database::ProjectDatabaseRecordReader;
use crate::storage::file_system::{ExistingDirectoryResolver, ResolveDirectoryError};

/// 已由项目开启边界建立、可供 MZ 各用例直接信任的项目上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenedProject {
    name: ProjectName,
    game_root: PathBuf,
    database_path: PathBuf,
    source_language: String,
    target_language: String,
}

impl OpenedProject {
    /// 建立一个受信项目上下文。
    pub(crate) fn new(
        name: ProjectName,
        game_root: PathBuf,
        database_path: PathBuf,
        source_language: String,
        target_language: String,
    ) -> Self {
        Self {
            name,
            game_root,
            database_path,
            source_language,
            target_language,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn game_root(&self) -> &Path {
        &self.game_root
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }
}

/// 把项目名称解析为当前仍可使用的受信项目上下文。
pub(crate) trait ExistingProjectOpener: Send + Sync {
    /// 项目开启失败。
    type Error: Error + Send + Sync + 'static;

    /// 打开项目并重新观测游戏根目录。
    fn open(
        &self,
        name: &ProjectName,
    ) -> impl Future<Output = Result<OpenedProject, Self::Error>> + Send;
}

/// 通过项目数据库记录和当前目录状态建立项目上下文。
pub(crate) struct ExistingProjectOpeningService<R, D> {
    record_reader: R,
    directory_resolver: D,
}

impl<R, D> ExistingProjectOpeningService<R, D> {
    pub(crate) fn new(record_reader: R, directory_resolver: D) -> Self {
        Self {
            record_reader,
            directory_resolver,
        }
    }
}

impl<R, D> ExistingProjectOpener for ExistingProjectOpeningService<R, D>
where
    R: ProjectDatabaseRecordReader,
    D: ExistingDirectoryResolver,
{
    type Error = ExistingProjectOpeningError<R::Error, D::Error>;

    async fn open(&self, name: &ProjectName) -> Result<OpenedProject, Self::Error> {
        let record = self
            .record_reader
            .read(name)
            .await
            .map_err(ExistingProjectOpeningError::ReadProjectRecord)?;
        let game_root = self
            .directory_resolver
            .resolve_existing_directory(record.game_root().to_path_buf())
            .await
            .map_err(ExistingProjectOpeningError::ResolveGameRoot)?;

        Ok(OpenedProject::new(
            record.name().clone(),
            game_root,
            record.database_path().to_path_buf(),
            record.source_language().to_owned(),
            record.target_language().to_owned(),
        ))
    }
}

/// 项目开启服务在自身职责边界内产生的阶段错误。
#[derive(Debug)]
pub(crate) enum ExistingProjectOpeningError<R, D> {
    /// 无法读取项目数据库记录。
    ReadProjectRecord(R),
    /// metadata 中记录的游戏根目录当前不可用。
    ResolveGameRoot(ResolveDirectoryError<D>),
}

impl<R, D> fmt::Display for ExistingProjectOpeningError<R, D>
where
    R: fmt::Display,
    D: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadProjectRecord(error) => write!(formatter, "无法读取项目记录：{error}"),
            Self::ResolveGameRoot(error) => write!(formatter, "游戏根目录当前不可用：{error}"),
        }
    }
}

impl<R, D> Error for ExistingProjectOpeningError<R, D>
where
    R: Error + 'static,
    D: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadProjectRecord(error) => Some(error),
            Self::ResolveGameRoot(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::project_database::StoredProjectRecord;

    use super::*;

    #[derive(Clone)]
    struct FakeRecordReader {
        response: Result<StoredProjectRecord, FakeRecordError>,
        calls: Arc<Mutex<Vec<ProjectName>>>,
    }

    impl ProjectDatabaseRecordReader for FakeRecordReader {
        type Error = FakeRecordError;

        async fn read(&self, name: &ProjectName) -> Result<StoredProjectRecord, Self::Error> {
            self.calls
                .lock()
                .expect("项目记录调用锁不应中毒")
                .push(name.clone());
            self.response.clone()
        }
    }

    #[derive(Clone)]
    struct FakeDirectoryResolver {
        response: Result<PathBuf, DirectoryResponseError>,
        calls: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl ExistingDirectoryResolver for FakeDirectoryResolver {
        type Error = FakeDirectoryError;

        async fn resolve_existing_directory(
            &self,
            path: PathBuf,
        ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
            self.calls
                .lock()
                .expect("目录解析调用锁不应中毒")
                .push(path.clone());
            self.response.clone().map_err(|error| match error {
                DirectoryResponseError::NotFound => ResolveDirectoryError::NotFound { path },
                DirectoryResponseError::NotDirectory => {
                    ResolveDirectoryError::NotDirectory { path }
                }
                DirectoryResponseError::Io => ResolveDirectoryError::Io {
                    path,
                    source: FakeDirectoryError,
                },
            })
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeRecordError;

    impl fmt::Display for FakeRecordError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake record failure")
        }
    }

    impl Error for FakeRecordError {}

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeDirectoryError;

    impl fmt::Display for FakeDirectoryError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake directory failure")
        }
    }

    impl Error for FakeDirectoryError {}

    #[derive(Clone, Copy)]
    enum DirectoryResponseError {
        NotFound,
        NotDirectory,
        Io,
    }

    fn record() -> StoredProjectRecord {
        StoredProjectRecord::new(
            "游戏 一".parse().expect("测试项目名称应该有效"),
            PathBuf::from("./Game One"),
            PathBuf::from("C:/att/projects/游戏 一.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        )
    }

    fn succeeding_service() -> ExistingProjectOpeningService<FakeRecordReader, FakeDirectoryResolver>
    {
        ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver {
                response: Ok(PathBuf::from("C:/Games/Game One")),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
    }

    #[tokio::test]
    async fn reads_record_and_reobserves_directory_exactly_once() {
        let service = succeeding_service();

        let opened = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect("项目应该成功开启");

        assert_eq!(opened.name().as_str(), "游戏 一");
        assert_eq!(opened.game_root(), Path::new("C:/Games/Game One"));
        assert_eq!(
            opened.database_path(),
            Path::new("C:/att/projects/游戏 一.db")
        );
        assert_eq!(opened.source_language(), "ja");
        assert_eq!(opened.target_language(), "zh-Hans");
        assert_eq!(
            service
                .record_reader
                .calls
                .lock()
                .expect("项目记录调用锁不应中毒")
                .len(),
            1
        );
        assert_eq!(
            service
                .directory_resolver
                .calls
                .lock()
                .expect("目录解析调用锁不应中毒")
                .as_slice(),
            &[PathBuf::from("./Game One")]
        );
    }

    #[tokio::test]
    async fn record_failure_stops_before_directory_resolution() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Err(FakeRecordError),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver {
                response: Ok(PathBuf::from("C:/Games/Game One")),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );

        let error = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect_err("读取失败应该阻止项目开启");

        assert!(matches!(
            error,
            ExistingProjectOpeningError::ReadProjectRecord(FakeRecordError)
        ));
        assert!(
            service
                .directory_resolver
                .calls
                .lock()
                .expect("目录解析调用锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn directory_failure_keeps_stage_and_source() {
        for response in [
            DirectoryResponseError::NotFound,
            DirectoryResponseError::NotDirectory,
            DirectoryResponseError::Io,
        ] {
            let service = ExistingProjectOpeningService::new(
                FakeRecordReader {
                    response: Ok(record()),
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
                FakeDirectoryResolver {
                    response: Err(response),
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
            );

            let error = service
                .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
                .await
                .expect_err("失效目录应该阻止项目开启");

            match error {
                ExistingProjectOpeningError::ResolveGameRoot(ResolveDirectoryError::NotFound {
                    path,
                }) => assert_eq!(path, PathBuf::from("./Game One")),
                ExistingProjectOpeningError::ResolveGameRoot(
                    ResolveDirectoryError::NotDirectory { path },
                ) => assert_eq!(path, PathBuf::from("./Game One")),
                ExistingProjectOpeningError::ResolveGameRoot(ResolveDirectoryError::Io {
                    source,
                    ..
                }) => assert_eq!(source, FakeDirectoryError),
                other => panic!("未预期的项目开启错误：{other}"),
            }
        }
    }

    #[test]
    fn opening_future_is_send() {
        let service = succeeding_service();
        let name: ProjectName = "游戏 一".parse().expect("测试项目名称应该有效");

        assert_send(service.open(&name));
    }

    fn assert_send(_: impl Send) {}
}
