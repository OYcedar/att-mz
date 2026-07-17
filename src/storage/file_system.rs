#![allow(dead_code, reason = "环境根尚未接入生产组合根")]

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
/// 并返回规范化、稳定的绝对目录路径。生产实现可以使用有界专用 worker
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
/// 类型或名称筛选；这些语义属于真正消费目录内容的上层模块。生产实现负责
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
/// 规范化绝对路径与未经变换的完整字节。生产实现负责使用有界资源隔离阻塞式
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

/// 目录候选中的一棵冻结来源子树。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySourceMapping {
    source_directory: PathBuf,
    relative_target: PathBuf,
}

impl DirectorySourceMapping {
    /// 建立一项“来源目录 → 候选内相对目录”映射。
    pub(crate) fn new(
        source_directory: PathBuf,
        relative_target: PathBuf,
    ) -> Result<Self, DirectoryStageRequestError> {
        if source_directory.as_os_str().is_empty() {
            return Err(DirectoryStageRequestError::EmptySourceDirectory);
        }
        validate_stage_relative_path(&relative_target)?;
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

/// 覆盖目录候选中一个相对文件的确定字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryFileOverlay {
    relative_file: PathBuf,
    bytes: Vec<u8>,
}

impl DirectoryFileOverlay {
    pub(crate) fn new(
        relative_file: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, DirectoryStageRequestError> {
        validate_stage_relative_path(&relative_file)?;
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

/// 一次原子目录发布的候选准备请求。
///
/// 来源映射构成冻结子树，文件覆盖必须位于某棵来源子树中，
/// `empty_directories` 则要求候选中至少存在这些目录。暂存位置、复制策略、
/// 交换恢复、取消清理及资源背压全部属于根实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryStageRequest {
    target_root: PathBuf,
    source_mappings: Vec<DirectorySourceMapping>,
    overlays: Vec<DirectoryFileOverlay>,
    empty_directories: Vec<PathBuf>,
}

impl DirectoryStageRequest {
    pub(crate) fn new(
        target_root: PathBuf,
        source_mappings: Vec<DirectorySourceMapping>,
        overlays: Vec<DirectoryFileOverlay>,
        empty_directories: Vec<PathBuf>,
    ) -> Result<Self, DirectoryStageRequestError> {
        if target_root.as_os_str().is_empty() {
            return Err(DirectoryStageRequestError::EmptyTargetRoot);
        }
        if source_mappings.is_empty() {
            return Err(DirectoryStageRequestError::EmptySourceMappings);
        }

        for (index, mapping) in source_mappings.iter().enumerate() {
            for other in source_mappings.iter().skip(index + 1) {
                if paths_overlap(mapping.relative_target(), other.relative_target()) {
                    return Err(DirectoryStageRequestError::OverlappingSourceTargets {
                        first: mapping.relative_target().to_path_buf(),
                        second: other.relative_target().to_path_buf(),
                    });
                }
            }
        }

        for (index, overlay) in overlays.iter().enumerate() {
            for other in overlays.iter().skip(index + 1) {
                if paths_overlap(overlay.relative_file(), other.relative_file()) {
                    return Err(DirectoryStageRequestError::OverlappingOverlays {
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
                return Err(DirectoryStageRequestError::OverlayOutsideSourceMappings {
                    relative_file: overlay.relative_file().to_path_buf(),
                });
            }
        }

        for (index, directory) in empty_directories.iter().enumerate() {
            validate_stage_relative_path(directory)?;
            for other in empty_directories.iter().skip(index + 1) {
                if paths_overlap(directory, other) {
                    return Err(DirectoryStageRequestError::OverlappingEmptyDirectories {
                        first: directory.to_path_buf(),
                        second: other.to_path_buf(),
                    });
                }
            }
            if let Some(overlay) = overlays
                .iter()
                .find(|overlay| paths_overlap(directory, overlay.relative_file()))
            {
                return Err(DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
                    empty_directory: directory.to_path_buf(),
                    overlay: overlay.relative_file().to_path_buf(),
                });
            }
            if let Some(mapping) = source_mappings
                .iter()
                .find(|mapping| paths_overlap(directory, mapping.relative_target()))
            {
                return Err(
                    DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
                        empty_directory: directory.to_path_buf(),
                        source_target: mapping.relative_target().to_path_buf(),
                    },
                );
            }
        }

        Ok(Self {
            target_root,
            source_mappings,
            overlays,
            empty_directories,
        })
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn source_mappings(&self) -> &[DirectorySourceMapping] {
        &self.source_mappings
    }

    pub(crate) fn overlays(&self) -> &[DirectoryFileOverlay] {
        &self.overlays
    }

    pub(crate) fn empty_directories(&self) -> &[PathBuf] {
        &self.empty_directories
    }
}

fn validate_stage_relative_path(path: &Path) -> Result<(), DirectoryStageRequestError> {
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|segment| matches!(segment, "." | ".."))
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
        return Err(DirectoryStageRequestError::InvalidRelativePath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

/// 目录候选请求尚未到达根实现前发现的契约错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryStageRequestError {
    EmptyTargetRoot,
    EmptySourceDirectory,
    EmptySourceMappings,
    InvalidRelativePath {
        path: PathBuf,
    },
    OverlappingSourceTargets {
        first: PathBuf,
        second: PathBuf,
    },
    OverlappingOverlays {
        first: PathBuf,
        second: PathBuf,
    },
    OverlappingEmptyDirectories {
        first: PathBuf,
        second: PathBuf,
    },
    OverlayOutsideSourceMappings {
        relative_file: PathBuf,
    },
    EmptyDirectoryOverlapsSourceTarget {
        empty_directory: PathBuf,
        source_target: PathBuf,
    },
    EmptyDirectoryOverlapsOverlay {
        empty_directory: PathBuf,
        overlay: PathBuf,
    },
}

impl fmt::Display for DirectoryStageRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTargetRoot => write!(formatter, "目录发布目标根目录不能为空"),
            Self::EmptySourceDirectory => write!(formatter, "目录发布来源目录不能为空"),
            Self::EmptySourceMappings => write!(formatter, "目录发布至少需要一棵来源子树"),
            Self::InvalidRelativePath { path } => {
                write!(
                    formatter,
                    "目录发布路径不是安全相对路径：{}",
                    path.display()
                )
            }
            Self::OverlappingSourceTargets { first, second } => write!(
                formatter,
                "目录发布的来源目标子树相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlappingOverlays { first, second } => write!(
                formatter,
                "目录发布的文件覆盖相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlappingEmptyDirectories { first, second } => write!(
                formatter,
                "目录发布的空目录相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlayOutsideSourceMappings { relative_file } => write!(
                formatter,
                "目录发布的文件覆盖不属于任何来源子树：{}",
                relative_file.display()
            ),
            Self::EmptyDirectoryOverlapsSourceTarget {
                empty_directory,
                source_target,
            } => write!(
                formatter,
                "目录发布的空目录 {} 与来源目标子树 {} 重叠",
                empty_directory.display(),
                source_target.display()
            ),
            Self::EmptyDirectoryOverlapsOverlay {
                empty_directory,
                overlay,
            } => write!(
                formatter,
                "目录发布的空目录 {} 与文件覆盖 {} 重叠",
                empty_directory.display(),
                overlay.display()
            ),
        }
    }
}

impl Error for DirectoryStageRequestError {}

/// 一个已准备但尚未发布的目录候选。
///
/// token 不可复制，只能交回创建它的根实现并被 `publish` 或 `discard`
/// 消费一次。根实现应将实例身份纳入 `state` 并校验归属。
#[derive(Debug, Eq, PartialEq)]
#[must_use = "已准备的目录候选必须发布或丢弃"]
pub(crate) struct StagedDirectory<T> {
    target_root: PathBuf,
    staging_root: PathBuf,
    state: T,
}

impl<T> StagedDirectory<T> {
    /// 根实现在准备成功后建立所有权 token。
    pub(crate) fn new(target_root: PathBuf, staging_root: PathBuf, state: T) -> Self {
        Self {
            target_root,
            staging_root,
            state,
        }
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    pub(crate) fn into_parts(self) -> (PathBuf, PathBuf, T) {
        (self.target_root, self.staging_root, self.state)
    }
}

/// 根实现无法删除的暂存或恢复产物。
#[derive(Debug)]
pub(crate) struct StagingCleanupFailure<E> {
    residual_path: PathBuf,
    source: E,
}

impl<E> StagingCleanupFailure<E> {
    pub(crate) fn new(residual_path: PathBuf, source: E) -> Self {
        Self {
            residual_path,
            source,
        }
    }

    pub(crate) fn residual_path(&self) -> &Path {
        &self.residual_path
    }

    pub(crate) fn source(&self) -> &E {
        &self.source
    }

    pub(crate) fn into_source(self) -> E {
        self.source
    }
}

impl<E> fmt::Display for StagingCleanupFailure<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法清理目录 {}：{}",
            self.residual_path.display(),
            self.source
        )
    }
}

impl<E> Error for StagingCleanupFailure<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 准备目录候选时的已知未发布终态。
#[derive(Debug)]
pub(crate) enum AtomicDirectoryPrepareError<E> {
    NotPrepared {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
}

impl<E> fmt::Display for AtomicDirectoryPrepareError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPrepared {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选未准备（目标：{}）：{source}",
                    target_root.display()
                )?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
        }
    }
}

impl<E> Error for AtomicDirectoryPrepareError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotPrepared { source, .. } => Some(source),
        }
    }
}

/// 已准备目录候选的发布方式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryPublishMode {
    /// 目标必须尚不存在，并在同名并发中保证至多一个发布者成功。
    CreateNew,
    /// 目标必须是现存目录，并被候选整体替换。
    Replace,
}

/// 根实现终结一次目录发布时的可观测终态。
#[derive(Debug)]
pub(crate) enum AtomicDirectoryPublishError<E> {
    TargetAlreadyExists {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    TargetMissing {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    TargetNotDirectory {
        target_root: PathBuf,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选没有成为目标，调用方可继续信任原目标。
    NotPublished {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选已经成为目标，但旧备份或其他恢复产物未能清理。
    PublishedButCleanupFailed {
        target_root: PathBuf,
        residual_path: PathBuf,
        source: E,
    },
    /// 交换与恢复均发生故障，目标当前内容无法确定。
    OutcomeUnknown {
        target_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
        source: E,
    },
}

impl<E> fmt::Display for AtomicDirectoryPublishError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标已存在：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::TargetMissing {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标不存在：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::TargetNotDirectory {
                target_root,
                cleanup_failure,
            } => {
                write!(formatter, "目录发布目标不是目录：{}", target_root.display())?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::NotPublished {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选未发布（目标：{}）：{source}",
                    target_root.display()
                )?;
                write_cleanup_failure(formatter, cleanup_failure.as_ref())
            }
            Self::PublishedButCleanupFailed {
                target_root,
                residual_path,
                source,
            } => write!(
                formatter,
                "目录候选已发布到 {}，但无法清理恢复产物 {}：{source}",
                target_root.display(),
                residual_path.display()
            ),
            Self::OutcomeUnknown {
                target_root,
                recovery_artifacts,
                source,
            } => write!(
                formatter,
                "目录发布结果未知（目标：{}，恢复产物：{}）：{source}",
                target_root.display(),
                display_paths(recovery_artifacts)
            ),
        }
    }
}

impl<E> Error for AtomicDirectoryPublishError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TargetAlreadyExists {
                cleanup_failure, ..
            }
            | Self::TargetMissing {
                cleanup_failure, ..
            }
            | Self::TargetNotDirectory {
                cleanup_failure, ..
            } => cleanup_failure
                .as_ref()
                .map(|failure| failure as &(dyn Error + 'static)),
            Self::NotPublished { source, .. }
            | Self::PublishedButCleanupFailed { source, .. }
            | Self::OutcomeUnknown { source, .. } => Some(source),
        }
    }
}

fn write_cleanup_failure<E>(
    formatter: &mut fmt::Formatter<'_>,
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
) -> fmt::Result
where
    E: fmt::Display,
{
    if let Some(cleanup_failure) = cleanup_failure {
        write!(formatter, "；{cleanup_failure}")?;
    }
    Ok(())
}

fn display_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "无已知路径".to_owned();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("、")
}

/// 主动丢弃目录候选时的清理失败。
#[derive(Debug)]
pub(crate) struct AtomicDirectoryDiscardError<E> {
    staging_root: PathBuf,
    source: E,
}

impl<E> AtomicDirectoryDiscardError<E> {
    pub(crate) fn new(staging_root: PathBuf, source: E) -> Self {
        Self {
            staging_root,
            source,
        }
    }

    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    pub(crate) fn source(&self) -> &E {
        &self.source
    }

    pub(crate) fn into_source(self) -> E {
        self.source
    }
}

impl<E> fmt::Display for AtomicDirectoryDiscardError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法丢弃暂存目录 {}：{}",
            self.staging_root.display(),
            self.source
        )
    }
}

impl<E> Error for AtomicDirectoryDiscardError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 在目标同级准备并原子发布完整目录的环境根能力。
///
/// `prepare` 必须限制递归复制资源，拒绝符号链接与 reparse point，并且不改变
/// 最终目标。`publish` 必须对同一目标线性化，并将交换、恢复与清理收敛为一个
/// 明确终态。所有操作一旦开始产生副作用，调用方必须等待 future 完成。
pub(crate) trait AtomicDirectoryPublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type StagingState: Send + 'static;

    fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> impl Future<
        Output = Result<
            StagedDirectory<Self::StagingState>,
            AtomicDirectoryPrepareError<Self::Error>,
        >,
    > + Send;

    fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
        mode: DirectoryPublishMode,
    ) -> impl Future<Output = Result<(), AtomicDirectoryPublishError<Self::Error>>> + Send;

    fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> impl Future<Output = Result<(), AtomicDirectoryDiscardError<Self::Error>>> + Send;
}

#[cfg(test)]
mod directory_stage_tests {
    use super::*;
    use std::convert::Infallible;

    fn mapping(source: &str, target: &str) -> DirectorySourceMapping {
        DirectorySourceMapping::new(PathBuf::from(source), PathBuf::from(target))
            .expect("测试来源映射应该合法")
    }

    fn overlay(path: &str) -> DirectoryFileOverlay {
        DirectoryFileOverlay::new(PathBuf::from(path), vec![1, 2, 3]).expect("测试文件覆盖应该合法")
    }

    #[test]
    fn stage_request_keeps_all_validated_candidate_parts() {
        let request = DirectoryStageRequest::new(
            PathBuf::from("C:/projects/demo/write_back"),
            vec![
                mapping("C:/projects/demo/source/data", "data"),
                mapping("C:/projects/demo/source/js", "js"),
            ],
            vec![overlay("data/Items.json"), overlay("js/plugins.js")],
            vec![PathBuf::from("logs"), PathBuf::from("empty/cache")],
        )
        .expect("标准目录候选请求应该合法");

        assert_eq!(
            request.target_root(),
            Path::new("C:/projects/demo/write_back")
        );
        assert_eq!(request.source_mappings().len(), 2);
        assert_eq!(request.overlays().len(), 2);
        assert_eq!(request.overlays()[0].bytes(), &[1, 2, 3]);
        assert_eq!(
            request.empty_directories(),
            &[PathBuf::from("logs"), PathBuf::from("empty/cache")]
        );
    }

    #[test]
    fn every_candidate_relative_path_rejects_empty_absolute_and_escape_forms() {
        for path in [
            "",
            ".",
            "../data",
            "data/../js",
            "data/./Items.json",
            "/outside",
            "C:/outside",
        ] {
            assert!(matches!(
                DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::from(path)),
                Err(DirectoryStageRequestError::InvalidRelativePath { .. })
            ));
            assert!(matches!(
                DirectoryFileOverlay::new(PathBuf::from(path), Vec::new()),
                Err(DirectoryStageRequestError::InvalidRelativePath { .. })
            ));
            assert!(matches!(
                DirectoryStageRequest::new(
                    PathBuf::from("out"),
                    vec![mapping("source", "source")],
                    Vec::new(),
                    vec![PathBuf::from(path)],
                ),
                Err(DirectoryStageRequestError::InvalidRelativePath { .. })
            ));
        }
    }

    #[test]
    fn stage_request_rejects_missing_roots_and_sources() {
        assert!(matches!(
            DirectorySourceMapping::new(PathBuf::new(), PathBuf::from("data")),
            Err(DirectoryStageRequestError::EmptySourceDirectory)
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::new(),
                vec![mapping("source", "data")],
                Vec::new(),
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::EmptyTargetRoot)
        ));
        assert!(matches!(
            DirectoryStageRequest::new(PathBuf::from("out"), Vec::new(), Vec::new(), Vec::new(),),
            Err(DirectoryStageRequestError::EmptySourceMappings)
        ));
    }

    #[test]
    fn stage_request_rejects_overlapping_targets_overlays_and_empty_directories() {
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![
                    mapping("source/data", "data"),
                    mapping("source/maps", "data/maps")
                ],
                Vec::new(),
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlappingSourceTargets { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("data/Items.json"), overlay("data/Items.json")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlappingOverlays { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                Vec::new(),
                vec![PathBuf::from("empty"), PathBuf::from("empty/child")],
            ),
            Err(DirectoryStageRequestError::OverlappingEmptyDirectories { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                Vec::new(),
                vec![PathBuf::from("data/empty")],
            ),
            Err(DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("data/Items.json")],
                vec![PathBuf::from("data/Items.json/child")],
            ),
            Err(DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay { .. })
        ));
    }

    #[test]
    fn overlay_must_be_a_file_below_a_source_target() {
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("js/plugins.js")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlayOutsideSourceMappings { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                vec![mapping("source/data", "data")],
                vec![overlay("data")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlayOutsideSourceMappings { .. })
        ));
    }

    #[test]
    fn staged_directory_exposes_paths_and_can_only_be_decomposed_by_value() {
        let staged = StagedDirectory::new(
            PathBuf::from("target"),
            PathBuf::from("target.stage"),
            42_u8,
        );

        assert_eq!(staged.target_root(), Path::new("target"));
        assert_eq!(staged.staging_root(), Path::new("target.stage"));
        assert_eq!(
            staged.into_parts(),
            (PathBuf::from("target"), PathBuf::from("target.stage"), 42)
        );
    }

    struct SendContractPublisher;

    impl AtomicDirectoryPublisher for SendContractPublisher {
        type Error = Infallible;
        type StagingState = ();

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, AtomicDirectoryPrepareError<Self::Error>>
        {
            Ok(StagedDirectory::new(
                request.target_root().to_path_buf(),
                PathBuf::from("stage"),
                (),
            ))
        }

        async fn publish(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
            _mode: DirectoryPublishMode,
        ) -> Result<(), AtomicDirectoryPublishError<Self::Error>> {
            Ok(())
        }

        async fn discard(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), AtomicDirectoryDiscardError<Self::Error>> {
            Ok(())
        }
    }

    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn every_root_operation_returns_a_send_future() {
        let publisher = SendContractPublisher;
        let request = DirectoryStageRequest::new(
            PathBuf::from("target"),
            vec![mapping("source", "data")],
            Vec::new(),
            Vec::new(),
        )
        .expect("测试准备请求应该合法");

        assert_send(publisher.prepare(request));
        assert_send(publisher.publish(
            StagedDirectory::new(PathBuf::from("target"), PathBuf::from("stage"), ()),
            DirectoryPublishMode::CreateNew,
        ));
        assert_send(publisher.discard(StagedDirectory::new(
            PathBuf::from("target"),
            PathBuf::from("stage"),
            (),
        )));
    }

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    #[test]
    fn known_unpublished_states_preserve_candidate_cleanup_failure() {
        let cleanup = || {
            Some(StagingCleanupFailure::new(
                PathBuf::from("target.stage"),
                TestError("cleanup failed"),
            ))
        };
        let errors = [
            AtomicDirectoryPublishError::TargetAlreadyExists {
                target_root: PathBuf::from("target"),
                cleanup_failure: cleanup(),
            },
            AtomicDirectoryPublishError::TargetMissing {
                target_root: PathBuf::from("target"),
                cleanup_failure: cleanup(),
            },
            AtomicDirectoryPublishError::TargetNotDirectory {
                target_root: PathBuf::from("target"),
                cleanup_failure: cleanup(),
            },
        ];

        for error in errors {
            let display = error.to_string();
            assert!(display.contains("target"));
            assert!(display.contains("target.stage"));
            assert!(display.contains("cleanup failed"));
            assert_eq!(
                Error::source(&error).map(ToString::to_string),
                Some("无法清理目录 target.stage：cleanup failed".to_owned())
            );
        }

        let error = AtomicDirectoryPublishError::NotPublished {
            target_root: PathBuf::from("target"),
            source: TestError("swap failed"),
            cleanup_failure: cleanup(),
        };
        assert!(error.to_string().contains("target.stage"));
        assert_eq!(
            Error::source(&error).map(ToString::to_string),
            Some("swap failed".to_owned())
        );
    }

    #[test]
    fn terminal_errors_report_published_and_unknown_outcomes_without_collapsing_them() {
        let published = AtomicDirectoryPublishError::PublishedButCleanupFailed {
            target_root: PathBuf::from("target"),
            residual_path: PathBuf::from("target.backup"),
            source: TestError("backup cleanup failed"),
        };
        assert!(published.to_string().contains("已发布"));
        assert!(published.to_string().contains("target.backup"));

        let unknown = AtomicDirectoryPublishError::OutcomeUnknown {
            target_root: PathBuf::from("target"),
            recovery_artifacts: vec![
                PathBuf::from("target.stage"),
                PathBuf::from("target.backup"),
            ],
            source: TestError("recovery failed"),
        };
        let display = unknown.to_string();
        assert!(display.contains("结果未知"));
        assert!(display.contains("target.stage"));
        assert!(display.contains("target.backup"));
        assert_eq!(
            Error::source(&unknown).map(ToString::to_string),
            Some("recovery failed".to_owned())
        );
    }

    #[test]
    fn prepare_and_discard_errors_keep_the_exact_residual_paths() {
        let prepare = AtomicDirectoryPrepareError::NotPrepared {
            target_root: PathBuf::from("target"),
            source: TestError("copy failed"),
            cleanup_failure: Some(StagingCleanupFailure::new(
                PathBuf::from("target.stage"),
                TestError("cleanup failed"),
            )),
        };
        assert!(prepare.to_string().contains("target.stage"));
        assert_eq!(
            Error::source(&prepare).map(ToString::to_string),
            Some("copy failed".to_owned())
        );

        let discard = AtomicDirectoryDiscardError::new(
            PathBuf::from("target.stage"),
            TestError("delete failed"),
        );
        assert_eq!(discard.staging_root(), Path::new("target.stage"));
        assert_eq!(discard.source().0, "delete failed");
        assert!(discard.to_string().contains("target.stage"));
        assert_eq!(
            Error::source(&discard).map(ToString::to_string),
            Some("delete failed".to_owned())
        );
    }
}
