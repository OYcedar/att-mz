//! RPG Maker 命令域共享的现存项目上下文与开启职责。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::language::{LanguageId, LanguagePair};
use crate::project_name::ProjectName;
#[cfg(test)]
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::dialogue::MvDialogueDefinition;
use crate::rpg_maker::project_database::{
    ProjectDatabaseRecordReader, ProjectWorkspaceLayout, SourceSnapshotFingerprint,
    StoredProjectRecord,
};
use crate::storage::file_system::{
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
    DirectoryTreeRoot, ExistingDirectoryResolver, ResolveDirectoryError,
};

pub(crate) use crate::rpg_maker::project_database::{
    MaxFullwidthChars, RpgMakerWriteBackLayoutProfile,
};

/// 已由项目开启边界建立、可供 RPG Maker 各用例直接信任的项目上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenedProject {
    record: StoredProjectRecord,
}

impl OpenedProject {
    /// 建立一个受信项目上下文。
    #[cfg(test)]
    pub(crate) fn new(
        name: ProjectName,
        workspace_root: PathBuf,
        database_path: PathBuf,
        source_language: String,
        target_language: String,
        layout_profile: RpgMakerWriteBackLayoutProfile,
    ) -> Self {
        let language_pair = LanguagePair::new(
            LanguageId::parse(&source_language).expect("测试源语言应为有效规范标签"),
            LanguageId::parse(&target_language).expect("测试目标语言应为有效规范标签"),
        );
        Self::from_record(StoredProjectRecord::new(
            name,
            workspace_root,
            database_path,
            RpgMakerLayout::MZ,
            language_pair,
            layout_profile,
        ))
    }

    fn from_record(record: StoredProjectRecord) -> Self {
        Self { record }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        self.record.name()
    }

    pub(crate) fn layout(&self) -> &ProjectWorkspaceLayout {
        self.record.layout()
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        self.record.workspace_root()
    }

    #[cfg(test)]
    pub(crate) fn source_root(&self) -> &Path {
        self.record.source_root()
    }

    pub(crate) fn write_back_root(&self) -> &Path {
        self.record.layout().write_back_root()
    }

    pub(crate) fn database_path(&self) -> &Path {
        self.record.database_path()
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        self.record.language_pair()
    }

    pub(crate) fn source_language(&self) -> &LanguageId {
        self.record.source_language()
    }

    pub(crate) fn target_language(&self) -> &LanguageId {
        self.record.target_language()
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.record.source_snapshot_fingerprint()
    }

    pub(crate) fn layout_profile(&self) -> &RpgMakerWriteBackLayoutProfile {
        self.record.layout_profile()
    }

    pub(crate) fn mv_dialogue_definition(&self) -> &MvDialogueDefinition {
        self.record.mv_dialogue_definition()
    }
}

/// 把项目名称解析为当前仍可使用的受信项目上下文。
pub(crate) trait ExistingProjectOpener: Send + Sync {
    /// 项目开启失败。
    type Error: Error + Send + Sync + 'static;

    /// 打开项目并重新观测按当前引擎布局冻结的 `data` 与 `js`。
    fn open(
        &self,
        name: &ProjectName,
    ) -> impl Future<Output = Result<OpenedProject, Self::Error>> + Send;
}

/// 通过项目数据库记录和当前目录状态建立项目上下文。
pub(crate) struct ExistingProjectOpeningService<R, D, F> {
    record_reader: R,
    directory_resolver: D,
    directory_tree_fingerprinter: F,
}

impl<R, D, F> ExistingProjectOpeningService<R, D, F> {
    pub(crate) fn new(
        record_reader: R,
        directory_resolver: D,
        directory_tree_fingerprinter: F,
    ) -> Self {
        Self {
            record_reader,
            directory_resolver,
            directory_tree_fingerprinter,
        }
    }
}

impl<R, D, F> ExistingProjectOpener for ExistingProjectOpeningService<R, D, F>
where
    R: ProjectDatabaseRecordReader,
    D: ExistingDirectoryResolver,
    F: DirectoryTreeFingerprinter,
{
    type Error = ExistingProjectOpeningError<R::Error, D::Error, F::Error>;

    async fn open(&self, name: &ProjectName) -> Result<OpenedProject, Self::Error> {
        let record = self
            .record_reader
            .read(name)
            .await
            .map_err(ExistingProjectOpeningError::ReadProjectRecord)?;
        let source_data = self
            .directory_resolver
            .resolve_existing_directory(record.layout().source_data().to_path_buf())
            .await
            .map_err(ExistingProjectOpeningError::ResolveSourceData)?;
        let source_js = self
            .directory_resolver
            .resolve_existing_directory(record.layout().source_js().to_path_buf())
            .await
            .map_err(ExistingProjectOpeningError::ResolveSourceJs)?;

        let request = DirectoryTreeFingerprintRequest::new(vec![
            DirectoryTreeRoot::new(source_data, "data".into())
                .expect("固定 data 逻辑根必须符合目录树指纹契约"),
            DirectoryTreeRoot::new(source_js, "js".into())
                .expect("固定 js 逻辑根必须符合目录树指纹契约"),
        ])
        .expect("固定 data 与 js 逻辑根必须互不重叠");
        let observed = self
            .directory_tree_fingerprinter
            .fingerprint_directory_tree(request)
            .await
            .map_err(ExistingProjectOpeningError::FingerprintSource)?;
        let observed = SourceSnapshotFingerprint::from_bytes(observed.into_bytes());
        let persisted = record.source_snapshot_fingerprint();
        if observed != persisted {
            return Err(ExistingProjectOpeningError::SourceSnapshotMismatch {
                persisted,
                observed,
            });
        }

        Ok(OpenedProject::from_record(record))
    }
}

/// 项目开启服务在自身职责边界内产生的阶段错误。
#[derive(Debug)]
pub(crate) enum ExistingProjectOpeningError<R, D, F> {
    /// 无法读取项目数据库记录。
    ReadProjectRecord(R),
    /// 冻结的 `data` 目录当前不可用。
    ResolveSourceData(ResolveDirectoryError<D>),
    /// 冻结的 `js` 目录当前不可用。
    ResolveSourceJs(ResolveDirectoryError<D>),
    /// 无法建立冻结来源的当前内容指纹。
    FingerprintSource(DirectoryTreeFingerprintError<F>),
    /// 工作区的实际冻结来源已与数据库记录分离。
    SourceSnapshotMismatch {
        persisted: SourceSnapshotFingerprint,
        observed: SourceSnapshotFingerprint,
    },
}

impl<R, D, F> fmt::Display for ExistingProjectOpeningError<R, D, F>
where
    R: fmt::Display,
    D: fmt::Display,
    F: fmt::Display,
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
            Self::FingerprintSource(error) => {
                write!(formatter, "无法建立冻结来源指纹：{error}")
            }
            Self::SourceSnapshotMismatch {
                persisted,
                observed,
            } => write!(
                formatter,
                "冻结来源内容与项目数据库记录不一致（记录 {persisted:?}，实际 {observed:?}）"
            ),
        }
    }
}

impl<R, D, F> Error for ExistingProjectOpeningError<R, D, F>
where
    R: Error + 'static,
    D: Error + 'static,
    F: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadProjectRecord(error) => Some(error),
            Self::ResolveSourceData(error) | Self::ResolveSourceJs(error) => Some(error),
            Self::FingerprintSource(error) => Some(error),
            Self::SourceSnapshotMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn test_layout_profile() -> RpgMakerWriteBackLayoutProfile {
    RpgMakerWriteBackLayoutProfile::new(
        MaxFullwidthChars::new(24).expect("测试对话宽度应该合法"),
        MaxFullwidthChars::new(30).expect("测试滚动文本宽度应该合法"),
        MaxFullwidthChars::new(18).expect("测试帮助说明宽度应该合法"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::fingerprint::Sha256Fingerprint;

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

    #[derive(Clone)]
    struct FakeDirectoryTreeFingerprinter {
        response: Result<Sha256Fingerprint, FingerprintResponseError>,
        calls: Arc<Mutex<Vec<DirectoryTreeFingerprintRequest>>>,
    }

    impl FakeDirectoryTreeFingerprinter {
        fn matching() -> Self {
            Self {
                response: Ok(Sha256Fingerprint::from_bytes([0xa5; 32])),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl DirectoryTreeFingerprinter for FakeDirectoryTreeFingerprinter {
        type Error = FakeFingerprintError;

        async fn fingerprint_directory_tree(
            &self,
            request: DirectoryTreeFingerprintRequest,
        ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
            self.calls
                .lock()
                .expect("目录树指纹调用锁不应中毒")
                .push(request);
            self.response.map_err(|error| match error {
                FingerprintResponseError::Changed => {
                    DirectoryTreeFingerprintError::ChangedDuringObservation {
                        path: PathBuf::from("C:/att/projects/游戏 一/source/data"),
                    }
                }
                FingerprintResponseError::Failed => DirectoryTreeFingerprintError::Failed {
                    path: PathBuf::from("C:/att/projects/游戏 一/source/data"),
                    source: FakeFingerprintError,
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeFingerprintError;

    impl fmt::Display for FakeFingerprintError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake fingerprint failure")
        }
    }

    impl Error for FakeFingerprintError {}

    #[derive(Clone, Copy)]
    enum DirectoryResponseError {
        NotFound,
        NotDirectory,
        Io,
    }

    #[derive(Clone, Copy)]
    enum FingerprintResponseError {
        Changed,
        Failed,
    }

    fn record() -> StoredProjectRecord {
        StoredProjectRecord::new(
            "游戏 一".parse().expect("测试项目名称应该有效"),
            PathBuf::from("C:/att/projects/游戏 一"),
            PathBuf::from("C:/att/projects/游戏 一/project.db"),
            RpgMakerLayout::MZ,
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言应合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
            ),
            profile(),
        )
    }

    fn profile() -> RpgMakerWriteBackLayoutProfile {
        RpgMakerWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试宽度应该是正整数")
    }

    fn succeeding_service() -> ExistingProjectOpeningService<
        FakeRecordReader,
        FakeDirectoryResolver,
        FakeDirectoryTreeFingerprinter,
    > {
        ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
            FakeDirectoryTreeFingerprinter::matching(),
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
            opened.layout().source_root(),
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
        assert_eq!(
            opened.layout().source_data(),
            Path::new("C:/att/projects/游戏 一/source/data")
        );
        assert_eq!(
            opened.layout().source_js(),
            Path::new("C:/att/projects/游戏 一/source/js")
        );
        assert_eq!(opened.source_language().as_str(), "ja");
        assert_eq!(opened.target_language().as_str(), "zh-Hans");
        assert_eq!(
            opened.source_snapshot_fingerprint(),
            SourceSnapshotFingerprint::from_bytes([0xa5; 32])
        );
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
        let fingerprint_calls = service
            .directory_tree_fingerprinter
            .calls
            .lock()
            .expect("目录树指纹调用锁不应中毒");
        assert_eq!(fingerprint_calls.len(), 1);
        assert_eq!(
            fingerprint_calls[0]
                .roots()
                .iter()
                .map(|root| (root.physical_root(), root.logical_root()))
                .collect::<Vec<_>>(),
            vec![
                (
                    Path::new("C:/att/projects/游戏 一/source/data"),
                    Path::new("data"),
                ),
                (
                    Path::new("C:/att/projects/游戏 一/source/js"),
                    Path::new("js"),
                ),
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
            FakeDirectoryTreeFingerprinter::matching(),
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
                FakeDirectoryTreeFingerprinter::matching(),
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
            FakeDirectoryTreeFingerprinter::matching(),
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

    #[tokio::test]
    async fn fingerprint_failure_stops_project_opening_after_both_directories_are_resolved() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
            FakeDirectoryTreeFingerprinter {
                response: Err(FingerprintResponseError::Failed),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );

        let error = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect_err("指纹失败应该阻止项目开启");

        assert!(matches!(
            error,
            ExistingProjectOpeningError::FingerprintSource(DirectoryTreeFingerprintError::Failed {
                source: FakeFingerprintError,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn changed_source_snapshot_is_rejected_even_when_directories_still_exist() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
            FakeDirectoryTreeFingerprinter {
                response: Ok(Sha256Fingerprint::from_bytes([0xb4; 32])),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );

        let error = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect_err("冻结来源与记录不一致应该阻止项目开启");

        assert!(matches!(
            error,
            ExistingProjectOpeningError::SourceSnapshotMismatch {
                persisted,
                observed,
            } if persisted == SourceSnapshotFingerprint::from_bytes([0xa5; 32])
                && observed == SourceSnapshotFingerprint::from_bytes([0xb4; 32])
        ));
    }

    #[tokio::test]
    async fn changing_tree_during_observation_is_preserved_as_fingerprint_error() {
        let service = ExistingProjectOpeningService::new(
            FakeRecordReader {
                response: Ok(record()),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDirectoryResolver::new([
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/data")),
                Ok(PathBuf::from("C:/att/projects/游戏 一/source/js")),
            ]),
            FakeDirectoryTreeFingerprinter {
                response: Err(FingerprintResponseError::Changed),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );

        let error = service
            .open(&"游戏 一".parse().expect("测试项目名称应该有效"))
            .await
            .expect_err("指纹观察期间变化应该阻止项目开启");

        assert!(matches!(
            error,
            ExistingProjectOpeningError::FingerprintSource(
                DirectoryTreeFingerprintError::ChangedDuringObservation { .. }
            )
        ));
    }

    #[test]
    fn opening_future_is_send() {
        let service = succeeding_service();
        let name: ProjectName = "游戏 一".parse().expect("测试项目名称应该有效");

        assert_send(service.open(&name));
    }

    fn assert_send(_: impl Send) {}
}
