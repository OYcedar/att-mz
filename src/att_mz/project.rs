#![allow(dead_code, reason = "项目开启服务尚未进行生产装配")]

//! MZ 命令域共享的现存项目上下文与开启职责。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::ProjectName;
use crate::project_database::ProjectDatabaseRecordReader;
use crate::storage::file_system::{ExistingDirectoryResolver, ResolveDirectoryError};

pub use crate::project_database::{
    MaxFullwidthChars, MaxFullwidthCharsError, MzWriteBackLayoutProfile,
};

/// 已由项目开启边界建立、可供 MZ 各用例直接信任的项目上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenedProject {
    name: ProjectName,
    workspace_root: PathBuf,
    source_root: PathBuf,
    write_back_root: PathBuf,
    database_path: PathBuf,
    source_language: String,
    target_language: String,
    layout_profile: MzWriteBackLayoutProfile,
}

impl OpenedProject {
    /// 建立一个受信项目上下文。
    pub(crate) fn new(
        name: ProjectName,
        workspace_root: PathBuf,
        database_path: PathBuf,
        source_language: String,
        target_language: String,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        let source_root = workspace_root.join("source");
        let write_back_root = workspace_root.join("write_back");
        Self {
            name,
            workspace_root,
            source_root,
            write_back_root,
            database_path,
            source_language,
            target_language,
            layout_profile,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub(crate) fn write_back_root(&self) -> &Path {
        &self.write_back_root
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

    pub(crate) fn layout_profile(&self) -> &MzWriteBackLayoutProfile {
        &self.layout_profile
    }
}

/// 把项目名称解析为当前仍可使用的受信项目上下文。
pub(crate) trait ExistingProjectOpener: Send + Sync {
    /// 项目开启失败。
    type Error: Error + Send + Sync + 'static;

    /// 打开项目并重新观测冻结原文所需的 `source/data` 与 `source/js`。
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
        let source_root = record.workspace_root().join("source");
        self.directory_resolver
            .resolve_existing_directory(source_root.join("data"))
            .await
            .map_err(ExistingProjectOpeningError::ResolveSourceData)?;
        self.directory_resolver
            .resolve_existing_directory(source_root.join("js"))
            .await
            .map_err(ExistingProjectOpeningError::ResolveSourceJs)?;

        Ok(OpenedProject::new(
            record.name().clone(),
            record.workspace_root().to_path_buf(),
            record.database_path().to_path_buf(),
            record.source_language().to_owned(),
            record.target_language().to_owned(),
            *record.layout_profile(),
        ))
    }
}

/// 项目开启服务在自身职责边界内产生的阶段错误。
#[derive(Debug)]
pub(crate) enum ExistingProjectOpeningError<R, D> {
    /// 无法读取项目数据库记录。
    ReadProjectRecord(R),
    /// 冻结的 `source/data` 当前不可用。
    ResolveSourceData(ResolveDirectoryError<D>),
    /// 冻结的 `source/js` 当前不可用。
    ResolveSourceJs(ResolveDirectoryError<D>),
}

impl<R, D> fmt::Display for ExistingProjectOpeningError<R, D>
where
    R: fmt::Display,
    D: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadProjectRecord(error) => write!(formatter, "无法读取项目记录：{error}"),
            Self::ResolveSourceData(error) => {
                write!(formatter, "冻结的 data 目录当前不可用：{error}")
            }
            Self::ResolveSourceJs(error) => {
                write!(formatter, "冻结的 js 目录当前不可用：{error}")
            }
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
            Self::ResolveSourceData(error) | Self::ResolveSourceJs(error) => Some(error),
        }
    }
}

#[cfg(test)]
pub(crate) fn test_layout_profile() -> MzWriteBackLayoutProfile {
    MzWriteBackLayoutProfile::new(
        MaxFullwidthChars::new(24).expect("测试对话宽度应该合法"),
        MaxFullwidthChars::new(30).expect("测试滚动文本宽度应该合法"),
        MaxFullwidthChars::new(18).expect("测试帮助说明宽度应该合法"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
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
        responses: Arc<Mutex<VecDeque<Result<PathBuf, DirectoryResponseError>>>>,
        calls: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl FakeDirectoryResolver {
        fn new(
            responses: impl IntoIterator<Item = Result<PathBuf, DirectoryResponseError>>,
        ) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
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
            self.responses
                .lock()
                .expect("目录响应锁不应中毒")
                .pop_front()
                .expect("测试应为每次目录调用提供响应")
                .map_err(|error| match error {
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
            PathBuf::from("C:/att/projects/游戏 一"),
            PathBuf::from("C:/att/projects/游戏 一/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            profile(),
        )
    }

    fn profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试宽度应该是正整数")
    }

    fn succeeding_service() -> ExistingProjectOpeningService<FakeRecordReader, FakeDirectoryResolver>
    {
        ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
        )
    }

    #[tokio::test]
    async fn reads_record_and_reobserves_both_frozen_source_directories() {
        let service = succeeding_service();

        let opened = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect("项目应该成功开启");

        assert_eq!(opened.name().as_str(), "游戏 一");
        assert_eq!(
            opened.workspace_root(),
            Path::new("C:/att/projects/游戏 一")
        );
        assert_eq!(
            opened.source_root(),
            Path::new("C:/att/projects/游戏 一/source")
        );
        assert_eq!(
            opened.write_back_root(),
            Path::new("C:/att/projects/游戏 一/write_back")
        );
        assert_eq!(
            opened.database_path(),
            Path::new("C:/att/projects/游戏 一/project.db")
        );
        assert_eq!(opened.source_language(), "ja");
        assert_eq!(opened.target_language(), "zh-Hans");
        assert_eq!(opened.layout_profile(), &profile());
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
            &[
                PathBuf::from("C:/att/projects/游戏 一/source/data"),
                PathBuf::from("C:/att/projects/游戏 一/source/js"),
            ]
        );
    }

    #[tokio::test]
    async fn record_failure_stops_before_directory_resolution() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Err(FakeRecordError),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
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
    async fn data_directory_failure_stops_before_js_validation() {
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
                FakeDirectoryResolver::new([Err(response)]),
            );

            let error = service
                .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
                .await
                .expect_err("失效目录应该阻止项目开启");

            match error {
                ExistingProjectOpeningError::ResolveSourceData(
                    ResolveDirectoryError::NotFound { path },
                ) => assert_eq!(path, PathBuf::from("C:/att/projects/游戏 一/source/data")),
                ExistingProjectOpeningError::ResolveSourceData(
                    ResolveDirectoryError::NotDirectory { path },
                ) => assert_eq!(path, PathBuf::from("C:/att/projects/游戏 一/source/data")),
                ExistingProjectOpeningError::ResolveSourceData(ResolveDirectoryError::Io {
                    source,
                    ..
                }) => assert_eq!(source, FakeDirectoryError),
                other => panic!("未预期的项目开启错误：{other}"),
            }
            assert_eq!(
                service
                    .directory_resolver
                    .calls
                    .lock()
                    .expect("目录解析调用锁不应中毒")
                    .as_slice(),
                &[PathBuf::from("C:/att/projects/游戏 一/source/data")]
            );
        }
    }

    #[tokio::test]
    async fn js_directory_failure_is_reported_after_data_validation() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Err(DirectoryResponseError::NotFound),
            ]),
        );

        let error = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect_err("缺少冻结 js 应该阻止项目开启");

        let ExistingProjectOpeningError::ResolveSourceJs(ResolveDirectoryError::NotFound { path }) =
            error
        else {
            panic!("未预期的项目开启错误：{error}")
        };
        assert_eq!(path, PathBuf::from("C:/att/projects/游戏 一/source/js"));
        assert_eq!(
            service
                .directory_resolver
                .calls
                .lock()
                .expect("目录解析调用锁不应中毒")
                .as_slice(),
            &[
                PathBuf::from("C:/att/projects/游戏 一/source/data"),
                PathBuf::from("C:/att/projects/游戏 一/source/js"),
            ]
        );
    }

    #[test]
    fn opening_future_is_send() {
        let service = succeeding_service();
        let name: ProjectName = "游戏 一".parse().expect("测试项目名称应该有效");

        assert_send(service.open(&name));
    }

    fn assert_send(_: impl Send) {}
}
