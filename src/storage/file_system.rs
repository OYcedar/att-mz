#![allow(dead_code, reason = "底层接口按计划先于生产适配器定义")]

//! 文件系统能力契约。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};

/// 解析现存目录时可能发生的失败。
#[derive(Debug)]
pub(crate) enum ResolveDirectoryError<E> {
    /// 目标路径不存在。
    NotFound { path: PathBuf },
    /// 目标存在，但不是目录。
    NotDirectory { path: PathBuf },
    /// 底层文件系统操作失败。
    Io { path: PathBuf, source: E },
}

impl<E> fmt::Display for ResolveDirectoryError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "目录不存在：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "路径不是目录：{}", path.display())
            }
            Self::Io { path, source } => {
                write!(formatter, "无法解析目录 {}：{source}", path.display())
            }
        }
    }
}

impl<E> Error for ResolveDirectoryError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotFound { .. } | Self::NotDirectory { .. } => None,
        }
    }
}

/// 提供不会阻塞异步执行器线程的现存目录解析能力。
///
/// `resolve_existing_directory` 在调用时以进程当前工作目录为基准解析相对路径，
/// 并返回规范化、稳定的绝对目录路径。未来的生产实现可以使用有界专用 worker
/// 隔离阻塞式系统调用，但该机制不属于本接口。
pub(crate) trait ExistingDirectoryResolver: Send + Sync {
    /// 底层文件系统错误。
    type Error: Error + Send + Sync + 'static;

    /// 解析并确认一个现存目录。
    fn resolve_existing_directory(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<PathBuf, ResolveDirectoryError<Self::Error>>> + Send;
}

/// 列举一个目录的直接子项时可能发生的失败。
#[derive(Debug)]
pub(crate) enum ListDirectoryError<E> {
    /// 目标路径不存在。
    NotFound { path: PathBuf },
    /// 目标存在，但不是目录。
    NotDirectory { path: PathBuf },
    /// 底层文件系统操作失败。
    Io { path: PathBuf, source: E },
}

impl<E> fmt::Display for ListDirectoryError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "目录不存在：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "路径不是目录：{}", path.display())
            }
            Self::Io { path, source } => {
                write!(formatter, "无法列举目录 {}：{source}", path.display())
            }
        }
    }
}

impl<E> Error for ListDirectoryError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotFound { .. } | Self::NotDirectory { .. } => None,
        }
    }
}

/// 提供不会阻塞异步执行器线程的非递归目录列举能力。
///
/// 成功结果包含目录直接子项的规范化绝对路径。实现不递归、不排序，也不按文件
/// 类型或名称筛选；这些语义属于真正消费目录内容的上层模块。未来生产实现负责
/// 通过外部配置的全局资源预算隔离阻塞式系统调用。
pub(crate) trait DirectoryLister: Send + Sync {
    /// 底层文件系统错误。
    type Error: Error + Send + Sync + 'static;

    /// 列举一个现存目录的全部直接子项。
    fn list_directory(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<Vec<PathBuf>, ListDirectoryError<Self::Error>>> + Send;
}

/// 已从文件系统完整读取的文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadFile {
    resolved_path: PathBuf,
    bytes: Vec<u8>,
}

impl ReadFile {
    /// 建立一个已完整读取的文件结果。
    pub(crate) fn new(resolved_path: PathBuf, bytes: Vec<u8>) -> Self {
        Self {
            resolved_path,
            bytes,
        }
    }

    /// 返回规范化的稳定绝对文件路径。
    pub(crate) fn resolved_path(&self) -> &std::path::Path {
        &self.resolved_path
    }

    /// 返回文件的原始字节。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 取出文件的原始字节。
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// 读取完整文件时可能发生的失败。
#[derive(Debug)]
pub(crate) enum ReadFileError<E> {
    /// 目标路径不存在。
    NotFound { path: PathBuf },
    /// 目标存在，但不是普通文件。
    NotFile { path: PathBuf },
    /// 底层文件系统操作失败。
    Io { path: PathBuf, source: E },
}

impl<E> fmt::Display for ReadFileError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "文件不存在：{}", path.display()),
            Self::NotFile { path } => write!(formatter, "路径不是文件：{}", path.display()),
            Self::Io { path, source } => {
                write!(formatter, "无法读取文件 {}：{source}", path.display())
            }
        }
    }
}

impl<E> Error for ReadFileError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotFound { .. } | Self::NotFile { .. } => None,
        }
    }
}

/// 提供不会阻塞异步执行器线程的完整文件读取能力。
///
/// `read_file` 在调用时以进程当前工作目录为基准解析相对路径，并在成功时返回
/// 规范化绝对路径与未经变换的完整字节。未来实现负责使用有界资源隔离阻塞式
/// 系统调用；上层不感知 worker、缓冲策略或平台细节。
pub(crate) trait FileReader: Send + Sync {
    /// 底层文件系统错误。
    type Error: Error + Send + Sync + 'static;

    /// 读取一个现存普通文件。
    fn read_file(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<ReadFile, ReadFileError<Self::Error>>> + Send;
}

/// 完整目录快照中的一棵冻结来源子树。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySnapshotSourceMapping {
    source_directory: PathBuf,
    relative_target: PathBuf,
}

impl DirectorySnapshotSourceMapping {
    /// 建立一项“来源目录 → 快照内相对目录”映射。
    pub(crate) fn new(
        source_directory: PathBuf,
        relative_target: PathBuf,
    ) -> Result<Self, DirectorySnapshotPublishRequestError> {
        if source_directory.as_os_str().is_empty() {
            return Err(DirectorySnapshotPublishRequestError::EmptySourceDirectory);
        }
        validate_snapshot_relative_path(&relative_target)?;
        Ok(Self {
            source_directory,
            relative_target,
        })
    }

    pub(crate) fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    pub(crate) fn relative_target(&self) -> &Path {
        &self.relative_target
    }
}

/// 覆盖完整目录快照中一个相对文件的确定字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySnapshotFileOverlay {
    relative_file: PathBuf,
    bytes: Vec<u8>,
}

impl DirectorySnapshotFileOverlay {
    pub(crate) fn new(
        relative_file: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, DirectorySnapshotPublishRequestError> {
        validate_snapshot_relative_path(&relative_file)?;
        Ok(Self {
            relative_file,
            bytes,
        })
    }

    pub(crate) fn relative_file(&self) -> &Path {
        &self.relative_file
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 一次完整目录快照原子发布请求。
///
/// 请求只描述业务要发布的目标、冻结来源子树和文件覆盖；暂存目录、复制策略、
/// 交换恢复、取消清理及资源背压全部属于根实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySnapshotPublishRequest {
    target_root: PathBuf,
    source_mappings: Vec<DirectorySnapshotSourceMapping>,
    overlays: Vec<DirectorySnapshotFileOverlay>,
}

impl DirectorySnapshotPublishRequest {
    pub(crate) fn new(
        target_root: PathBuf,
        source_mappings: Vec<DirectorySnapshotSourceMapping>,
        overlays: Vec<DirectorySnapshotFileOverlay>,
    ) -> Result<Self, DirectorySnapshotPublishRequestError> {
        if target_root.as_os_str().is_empty() {
            return Err(DirectorySnapshotPublishRequestError::EmptyTargetRoot);
        }
        if source_mappings.is_empty() {
            return Err(DirectorySnapshotPublishRequestError::EmptySourceMappings);
        }

        for (index, mapping) in source_mappings.iter().enumerate() {
            for other in source_mappings.iter().skip(index + 1) {
                if paths_overlap(mapping.relative_target(), other.relative_target()) {
                    return Err(DirectorySnapshotPublishRequestError::OverlappingTargets {
                        first: mapping.relative_target().to_path_buf(),
                        second: other.relative_target().to_path_buf(),
                    });
                }
            }
        }

        for (index, overlay) in overlays.iter().enumerate() {
            for other in overlays.iter().skip(index + 1) {
                if paths_overlap(overlay.relative_file(), other.relative_file()) {
                    return Err(DirectorySnapshotPublishRequestError::OverlappingOverlays {
                        first: overlay.relative_file().to_path_buf(),
                        second: other.relative_file().to_path_buf(),
                    });
                }
            }
            if !source_mappings.iter().any(|mapping| {
                overlay
                    .relative_file()
                    .starts_with(mapping.relative_target())
                    && overlay.relative_file() != mapping.relative_target()
            }) {
                return Err(
                    DirectorySnapshotPublishRequestError::OverlayOutsideSourceMappings {
                        relative_file: overlay.relative_file().to_path_buf(),
                    },
                );
            }
        }

        Ok(Self {
            target_root,
            source_mappings,
            overlays,
        })
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn source_mappings(&self) -> &[DirectorySnapshotSourceMapping] {
        &self.source_mappings
    }

    pub(crate) fn overlays(&self) -> &[DirectorySnapshotFileOverlay] {
        &self.overlays
    }
}

fn validate_snapshot_relative_path(
    path: &Path,
) -> Result<(), DirectorySnapshotPublishRequestError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
    {
        return Err(DirectorySnapshotPublishRequestError::InvalidRelativePath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

/// 目录快照请求尚未到达根实现前发现的契约错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectorySnapshotPublishRequestError {
    EmptyTargetRoot,
    EmptySourceDirectory,
    EmptySourceMappings,
    InvalidRelativePath { path: PathBuf },
    OverlappingTargets { first: PathBuf, second: PathBuf },
    OverlappingOverlays { first: PathBuf, second: PathBuf },
    OverlayOutsideSourceMappings { relative_file: PathBuf },
}

impl fmt::Display for DirectorySnapshotPublishRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTargetRoot => write!(formatter, "目录快照目标根目录不能为空"),
            Self::EmptySourceDirectory => write!(formatter, "目录快照来源目录不能为空"),
            Self::EmptySourceMappings => write!(formatter, "目录快照至少需要一棵来源子树"),
            Self::InvalidRelativePath { path } => {
                write!(
                    formatter,
                    "目录快照路径不是安全相对路径：{}",
                    path.display()
                )
            }
            Self::OverlappingTargets { first, second } => write!(
                formatter,
                "目录快照目标子树相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlappingOverlays { first, second } => write!(
                formatter,
                "目录快照文件覆盖相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlayOutsideSourceMappings { relative_file } => write!(
                formatter,
                "目录快照文件覆盖不属于任何来源子树：{}",
                relative_file.display()
            ),
        }
    }
}

impl Error for DirectorySnapshotPublishRequestError {}

/// 根实现无法完成一次原子目录快照发布时的终态。
#[derive(Debug)]
pub(crate) enum AtomicDirectorySnapshotPublishError<E> {
    /// 新快照没有成为目标，调用方可继续信任旧完整输出。
    NotPublished { source: E },
    /// 目录交换或恢复发生二次故障，根实现无法确认目标目前是哪一份完整输出。
    OutcomeUnknown { target_root: PathBuf, source: E },
}

impl<E> fmt::Display for AtomicDirectorySnapshotPublishError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPublished { source } => write!(formatter, "目录快照未发布：{source}"),
            Self::OutcomeUnknown {
                target_root,
                source,
            } => write!(
                formatter,
                "目录快照发布结果未知（目标：{}）：{source}",
                target_root.display()
            ),
        }
    }
}

impl<E> Error for AtomicDirectorySnapshotPublishError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotPublished { source } | Self::OutcomeUnknown { source, .. } => Some(source),
        }
    }
}

/// 原子发布一份完整目录快照的环境根能力。
///
/// 成功意味着请求中的所有来源子树与文件覆盖已经共同成为 `target_root` 的唯一
/// 可见完整版本。根实现必须在目标同级暂存、限制递归复制资源、不跟随越界符号链接
/// 或 reparse point，并在取消或失败时清理暂存物。
pub(crate) trait AtomicDirectorySnapshotPublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn publish_snapshot(
        &self,
        request: DirectorySnapshotPublishRequest,
    ) -> impl Future<Output = Result<(), AtomicDirectorySnapshotPublishError<Self::Error>>> + Send;
}

#[cfg(test)]
mod directory_snapshot_tests {
    use super::*;

    fn mapping(source: &str, target: &str) -> DirectorySnapshotSourceMapping {
        DirectorySnapshotSourceMapping::new(PathBuf::from(source), PathBuf::from(target))
            .expect("测试来源映射应该合法")
    }

    fn overlay(path: &str) -> DirectorySnapshotFileOverlay {
        DirectorySnapshotFileOverlay::new(PathBuf::from(path), vec![1, 2, 3])
            .expect("测试文件覆盖应该合法")
    }

    #[test]
    fn snapshot_request_keeps_validated_sources_and_overlays() {
        let request = DirectorySnapshotPublishRequest::new(
            PathBuf::from("C:/projects/demo/write_back"),
            vec![
                mapping("C:/projects/demo/source/data", "data"),
                mapping("C:/projects/demo/source/js", "js"),
            ],
            vec![overlay("data/Items.json"), overlay("js/plugins.js")],
        )
        .expect("标准写回快照请求应该合法");

        assert_eq!(
            request.target_root(),
            Path::new("C:/projects/demo/write_back")
        );
        assert_eq!(request.source_mappings().len(), 2);
        assert_eq!(request.overlays().len(), 2);
        assert_eq!(request.overlays()[0].bytes(), &[1, 2, 3]);
    }

    #[test]
    fn snapshot_paths_reject_escape_absolute_duplicate_and_overlap() {
        for path in ["", "../data", "data/../js", "C:/outside"] {
            assert!(matches!(
                DirectorySnapshotFileOverlay::new(PathBuf::from(path), Vec::new()),
                Err(DirectorySnapshotPublishRequestError::InvalidRelativePath { .. })
            ));
        }

        assert!(matches!(
            DirectorySnapshotPublishRequest::new(
                PathBuf::from("out"),
                vec![
                    mapping("source/data", "data"),
                    mapping("source/maps", "data/maps")
                ],
                Vec::new(),
            ),
            Err(DirectorySnapshotPublishRequestError::OverlappingTargets { .. })
        ));
        assert!(matches!(
            DirectorySnapshotPublishRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("data/Items.json"), overlay("data/Items.json")],
            ),
            Err(DirectorySnapshotPublishRequestError::OverlappingOverlays { .. })
        ));
        assert!(matches!(
            DirectorySnapshotPublishRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("js/plugins.js")],
            ),
            Err(DirectorySnapshotPublishRequestError::OverlayOutsideSourceMappings { .. })
        ));
    }
}
