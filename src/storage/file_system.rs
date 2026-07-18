//! 文件系统能力契约。

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::fingerprint::Sha256Fingerprint;

/// 可恢复目录发布器在目标父目录中持有的锁命名空间。
///
/// 该名称同时是工作区结构观察需要精确识别的受管基础设施事实，因此由目录能力
/// 契约统一拥有，避免发布实现与业务收敛边界各自复制字符串常量。
pub(crate) const DIRECTORY_PUBLISH_LOCK_NAMESPACE: &str = ".att-dirpub-locks";

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

/// 已由文件系统根固定并分类的一个直接目录项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryEntryKind {
    RegularFile,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryEntry {
    resolved_path: PathBuf,
    kind: DirectoryEntryKind,
}

impl DirectoryEntry {
    pub(crate) fn new(resolved_path: PathBuf, kind: DirectoryEntryKind) -> Self {
        Self {
            resolved_path,
            kind,
        }
    }

    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    pub(crate) const fn kind(&self) -> DirectoryEntryKind {
        self.kind
    }

    pub(crate) fn into_path(self) -> PathBuf {
        self.resolved_path
    }
}

/// 提供不会阻塞异步执行器线程的非递归目录列举能力。
///
/// 成功结果包含目录直接子项的规范化绝对路径和普通文件/目录类别。实现不递归、
/// 不排序，也不按名称筛选；reparse point、非普通对象和硬链接文件不形成受信结果。
/// 生产实现负责通过外部配置的全局资源预算隔离阻塞式系统调用。
pub(crate) trait DirectoryLister: Send + Sync {
    /// 底层文件系统错误。
    type Error: Error + Send + Sync + 'static;

    /// 列举一个现存目录的全部直接子项。
    fn list_directory(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>>> + Send;
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

/// 一个项目级排他操作租约的受检请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectOperationLeaseRequest {
    projects_root: PathBuf,
    project_directory_name: OsString,
}

impl ProjectOperationLeaseRequest {
    pub(crate) fn new(
        projects_root: PathBuf,
        project_directory_name: OsString,
    ) -> Result<Self, ProjectOperationLeaseRequestError> {
        if projects_root.as_os_str().is_empty() {
            return Err(ProjectOperationLeaseRequestError::EmptyProjectsRoot);
        }
        let project_path = Path::new(&project_directory_name);
        let mut components = project_path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(
                ProjectOperationLeaseRequestError::InvalidProjectDirectoryName {
                    name: project_directory_name,
                },
            );
        }
        Ok(Self {
            projects_root,
            project_directory_name,
        })
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) fn project_directory_name(&self) -> &std::ffi::OsStr {
        &self.project_directory_name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectOperationLeaseRequestError {
    EmptyProjectsRoot,
    InvalidProjectDirectoryName { name: OsString },
}

impl fmt::Display for ProjectOperationLeaseRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProjectsRoot => formatter.write_str("项目集合根不能为空"),
            Self::InvalidProjectDirectoryName { name } => write!(
                formatter,
                "项目目录名必须是一个普通路径段：{}",
                name.to_string_lossy()
            ),
        }
    }
}

impl Error for ProjectOperationLeaseRequestError {}

/// 持有一个项目级排他操作直到本值被丢弃。
#[must_use = "项目操作租约必须存活到整个项目命令结束"]
pub(crate) struct ProjectOperationLease<T> {
    _state: T,
}

impl<T> ProjectOperationLease<T> {
    pub(crate) const fn new(state: T) -> Self {
        Self { _state: state }
    }
}

#[derive(Debug)]
pub(crate) enum ProjectOperationLeaseError<E> {
    Busy {
        project_directory_name: OsString,
        timeout: Duration,
    },
    Unavailable {
        project_directory_name: OsString,
        source: E,
    },
}

impl<E: fmt::Display> fmt::Display for ProjectOperationLeaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy {
                project_directory_name,
                timeout,
            } => write!(
                formatter,
                "项目 {} 正由另一条命令处理，等待 {timeout:?} 后仍未取得租约",
                project_directory_name.to_string_lossy()
            ),
            Self::Unavailable {
                project_directory_name,
                source,
            } => write!(
                formatter,
                "无法取得项目 {} 的操作租约：{source}",
                project_directory_name.to_string_lossy()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ProjectOperationLeaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable { source, .. } => Some(source),
            Self::Busy { .. } => None,
        }
    }
}

/// 为同一项目跨进程串行化完整命令的环境根能力。
pub(crate) trait ProjectOperationLeaseProvider: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type LeaseState: Send + 'static;

    fn acquire_project_operation_lease(
        &self,
        request: ProjectOperationLeaseRequest,
    ) -> impl Future<
        Output = Result<
            ProjectOperationLease<Self::LeaseState>,
            ProjectOperationLeaseError<Self::Error>,
        >,
    > + Send;
}

/// 一棵物理目录树在指纹中的稳定逻辑根。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryTreeRoot {
    physical_root: PathBuf,
    logical_root: PathBuf,
}

impl DirectoryTreeRoot {
    pub(crate) fn new(
        physical_root: PathBuf,
        logical_root: PathBuf,
    ) -> Result<Self, DirectoryTreeFingerprintRequestError> {
        if physical_root.as_os_str().is_empty() {
            return Err(DirectoryTreeFingerprintRequestError::EmptyPhysicalRoot);
        }
        validate_tree_logical_root(&logical_root)?;
        Ok(Self {
            physical_root,
            logical_root,
        })
    }

    pub(crate) fn physical_root(&self) -> &Path {
        &self.physical_root
    }

    pub(crate) fn logical_root(&self) -> &Path {
        &self.logical_root
    }
}

/// 多棵目录树共同构成一个精确内容身份的受检请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryTreeFingerprintRequest {
    roots: Vec<DirectoryTreeRoot>,
}

impl DirectoryTreeFingerprintRequest {
    pub(crate) fn new(
        roots: Vec<DirectoryTreeRoot>,
    ) -> Result<Self, DirectoryTreeFingerprintRequestError> {
        if roots.is_empty() {
            return Err(DirectoryTreeFingerprintRequestError::EmptyRoots);
        }
        for (index, root) in roots.iter().enumerate() {
            for other in roots.iter().skip(index + 1) {
                if paths_overlap(root.logical_root(), other.logical_root()) {
                    return Err(
                        DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots {
                            first: root.logical_root().to_path_buf(),
                            second: other.logical_root().to_path_buf(),
                        },
                    );
                }
            }
        }
        Ok(Self { roots })
    }

    pub(crate) fn roots(&self) -> &[DirectoryTreeRoot] {
        &self.roots
    }
}

fn validate_tree_logical_root(path: &Path) -> Result<(), DirectoryTreeFingerprintRequestError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DirectoryTreeFingerprintRequestError::InvalidLogicalRoot {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryTreeFingerprintRequestError {
    EmptyRoots,
    EmptyPhysicalRoot,
    InvalidLogicalRoot { path: PathBuf },
    OverlappingLogicalRoots { first: PathBuf, second: PathBuf },
}

impl fmt::Display for DirectoryTreeFingerprintRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoots => formatter.write_str("目录树指纹至少需要一个逻辑根"),
            Self::EmptyPhysicalRoot => formatter.write_str("目录树物理根不能为空"),
            Self::InvalidLogicalRoot { path } => {
                write!(
                    formatter,
                    "目录树逻辑根必须是安全相对路径：{}",
                    path.display()
                )
            }
            Self::OverlappingLogicalRoots { first, second } => write!(
                formatter,
                "目录树逻辑根不能重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
        }
    }
}

impl Error for DirectoryTreeFingerprintRequestError {}

#[derive(Debug)]
pub(crate) enum DirectoryTreeFingerprintError<E> {
    NotFound { path: PathBuf },
    NotDirectory { path: PathBuf },
    ChangedDuringObservation { path: PathBuf },
    Failed { path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for DirectoryTreeFingerprintError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "目录树不存在：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "目录树根不是目录：{}", path.display())
            }
            Self::ChangedDuringObservation { path } => write!(
                formatter,
                "目录树在建立指纹期间发生变化：{}",
                path.display()
            ),
            Self::Failed { path, source } => {
                write!(formatter, "无法建立目录树指纹 {}：{source}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for DirectoryTreeFingerprintError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::NotFound { .. }
            | Self::NotDirectory { .. }
            | Self::ChangedDuringObservation { .. } => None,
        }
    }
}

/// 对一个或多个逻辑根建立与绝对路径无关的精确 SHA-256 内容指纹。
pub(crate) trait DirectoryTreeFingerprinter: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn fingerprint_directory_tree(
        &self,
        request: DirectoryTreeFingerprintRequest,
    ) -> impl Future<Output = Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>>> + Send;
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

/// 目录候选准备完成后允许执行的唯一发布意图。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryPublishIntent {
    /// 目标必须尚不存在，并在同名并发中保证至多一个发布者成功。
    CreateNew,
    /// 目标必须是现存目录，并被候选整体替换。
    ReplaceExisting,
}

/// 一次可恢复目录发布的候选准备请求。
///
/// 来源映射构成冻结子树，文件覆盖必须位于某棵来源子树中，
/// `empty_directories` 则要求候选中至少存在这些目录。暂存位置、复制策略、
/// 交换恢复、取消清理及资源背压全部属于根实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryStageRequest {
    target_root: PathBuf,
    publish_intent: DirectoryPublishIntent,
    source_mappings: Vec<DirectorySourceMapping>,
    overlays: Vec<DirectoryFileOverlay>,
    empty_directories: Vec<PathBuf>,
}

impl DirectoryStageRequest {
    pub(crate) fn new(
        target_root: PathBuf,
        publish_intent: DirectoryPublishIntent,
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
            publish_intent,
            source_mappings,
            overlays,
            empty_directories,
        })
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn publish_intent(&self) -> DirectoryPublishIntent {
        self.publish_intent
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
    publish_intent: DirectoryPublishIntent,
    state: T,
}

impl<T> StagedDirectory<T> {
    /// 根实现在准备成功后建立所有权 token。
    pub(crate) fn new(
        target_root: PathBuf,
        staging_root: PathBuf,
        publish_intent: DirectoryPublishIntent,
        state: T,
    ) -> Self {
        Self {
            target_root,
            staging_root,
            publish_intent,
            state,
        }
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    #[cfg(test)]
    pub(crate) fn publish_intent(&self) -> DirectoryPublishIntent {
        self.publish_intent
    }

    /// 仅供创建 token 的根在交付失败时标记清理责任。
    pub(crate) fn state_mut(&mut self) -> &mut T {
        &mut self.state
    }

    /// 仅供与发布根共享候选身份的能力建立受限借用令牌。
    pub(crate) fn state(&self) -> &T {
        &self.state
    }

    pub(crate) fn into_parts(self) -> (PathBuf, PathBuf, DirectoryPublishIntent, T) {
        (
            self.target_root,
            self.staging_root,
            self.publish_intent,
            self.state,
        )
    }
}

/// Lua 可在未发布候选内访问的受检相对路径。
///
/// 路径只允许精确位于 `data` 或 `js` 子树，不接受绝对路径、当前/父级段、前缀、
/// ADS 分隔符或其他候选根外表达。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ScopedDirectoryPath(PathBuf);

impl ScopedDirectoryPath {
    pub(crate) fn new(path: PathBuf) -> Result<Self, ScopedDirectoryPathError> {
        let mut components = path.components();
        let Some(Component::Normal(root)) = components.next() else {
            return Err(ScopedDirectoryPathError { path });
        };
        if root != OsStr::new("data") && root != OsStr::new("js") {
            return Err(ScopedDirectoryPathError { path });
        }
        if root.to_string_lossy().contains(':')
            || components.any(|component| {
                !matches!(component, Component::Normal(name) if !name.to_string_lossy().contains(':'))
            })
        {
            return Err(ScopedDirectoryPathError { path });
        }
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn is_scope_root(&self) -> bool {
        self.0.components().count() == 1
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryPathError {
    path: PathBuf,
}

impl fmt::Display for ScopedDirectoryPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "候选编辑路径必须是 data 或 js 下的安全相对路径：{}",
            self.path.display()
        )
    }
}

impl Error for ScopedDirectoryPathError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScopedDirectoryEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryEntry {
    name: OsString,
    kind: ScopedDirectoryEntryKind,
}

impl ScopedDirectoryEntry {
    pub(crate) fn new(name: OsString, kind: ScopedDirectoryEntryKind) -> Self {
        Self { name, kind }
    }

    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) const fn kind(&self) -> ScopedDirectoryEntryKind {
        self.kind
    }
}

/// 与一个仍未发布候选的物理根身份绑定的编辑令牌。
#[derive(Debug)]
pub(crate) struct BoundScopedDirectory<T> {
    root: PathBuf,
    state: T,
}

impl<T> BoundScopedDirectory<T> {
    pub(crate) fn new(root: PathBuf, state: T) -> Self {
        Self { root, state }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn state(&self) -> &T {
        &self.state
    }
}

#[derive(Debug)]
pub(crate) enum ScopedDirectoryBindError<E> {
    WrongEditorInstance,
    CandidateFinalized { root: PathBuf },
    CandidateIdentityChanged { root: PathBuf },
    Failed { root: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ScopedDirectoryBindError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEditorInstance => {
                formatter.write_str("目录候选不能绑定到另一个文件系统根实例")
            }
            Self::CandidateFinalized { root } => {
                write!(formatter, "目录候选已经终结：{}", root.display())
            }
            Self::CandidateIdentityChanged { root } => {
                write!(formatter, "目录候选物理身份已经变化：{}", root.display())
            }
            Self::Failed { root, source } => {
                write!(formatter, "无法绑定目录候选 {}：{source}", root.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ScopedDirectoryBindError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::WrongEditorInstance
            | Self::CandidateFinalized { .. }
            | Self::CandidateIdentityChanged { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ScopedDirectoryEditError<E> {
    WrongEditorInstance,
    ScopeRootMutation { path: PathBuf },
    NotFound { path: PathBuf },
    NotFile { path: PathBuf },
    NotDirectory { path: PathBuf },
    DirectoryNotEmpty { path: PathBuf },
    CandidateIdentityChanged { root: PathBuf },
    Failed { path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ScopedDirectoryEditError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEditorInstance => {
                formatter.write_str("候选编辑令牌不能交给另一个文件系统根实例")
            }
            Self::ScopeRootMutation { path } => {
                write!(formatter, "不能修改候选编辑子树根：{}", path.display())
            }
            Self::NotFound { path } => write!(formatter, "候选路径不存在：{}", path.display()),
            Self::NotFile { path } => write!(formatter, "候选路径不是文件：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "候选路径不是目录：{}", path.display())
            }
            Self::DirectoryNotEmpty { path } => {
                write!(formatter, "候选目录不是空目录：{}", path.display())
            }
            Self::CandidateIdentityChanged { root } => {
                write!(formatter, "目录候选物理身份已经变化：{}", root.display())
            }
            Self::Failed { path, source } => {
                write!(formatter, "候选目录操作失败 {}：{source}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ScopedDirectoryEditError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::WrongEditorInstance
            | Self::ScopeRootMutation { .. }
            | Self::NotFound { .. }
            | Self::NotFile { .. }
            | Self::NotDirectory { .. }
            | Self::DirectoryNotEmpty { .. }
            | Self::CandidateIdentityChanged { .. } => None,
        }
    }
}

/// 在一个未发布目录候选的 `data`/`js` 子树中执行受限文件操作。
///
/// `bind_scoped_directory` 必须先把令牌绑定到候选根的物理身份。所有操作必须拒绝
/// reparse point 与硬链接，并经有界根资源接管；调用返回前该次操作已经终结。发布根
/// 仍负责在整体交换前重新验证完整候选树。
pub(crate) trait ScopedDirectoryEditor: Send + Sync {
    type CandidateState: Send + 'static;
    type ScopeState: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
    ) -> impl Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<Self>;

    /// 在候选交回发布根前重验物理身份、整树预算与顶层 `data`/`js` 结构。
    fn validate_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send + use<Self>;

    fn read_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>> + Send;

    fn list_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send;

    fn create_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send;

    fn write_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send;

    /// 删除一个普通文件或空目录；`data`、`js` 根本身不可删除。
    fn remove_scoped_path(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send;
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

    #[cfg(test)]
    pub(crate) fn residual_path(&self) -> &Path {
        &self.residual_path
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &E {
        &self.source
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
pub(crate) enum DirectoryPrepareError<E> {
    /// 本次操作尚未接管任何副作用。
    NotAttempted { target_root: PathBuf, source: E },
    NotPrepared {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
}

impl<E> fmt::Display for DirectoryPrepareError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAttempted {
                target_root,
                source,
            } => write!(
                formatter,
                "目录候选尚未开始准备（目标：{}）：{source}",
                target_root.display()
            ),
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

impl<E> Error for DirectoryPrepareError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotAttempted { source, .. } | Self::NotPrepared { source, .. } => Some(source),
        }
    }
}

/// 根实现终结一次目录发布时的可观测终态。
#[derive(Debug)]
pub(crate) enum DirectoryPublishError<E> {
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
    /// 根在接管交换副作用前拒绝本次发布。
    NotAttempted {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选没有成为目标，调用方可继续信任原目标。
    NotPublished {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
    /// 候选已经成为目标，但旧备份或其他恢复产物未能清理。
    PublishedWithResiduals {
        target_root: PathBuf,
        residual_path: PathBuf,
        source: E,
    },
    /// 目标暂时缺失，但旧目录与候选身份仍然确定，后续同目标操作可以恢复。
    RecoveryRequired {
        target_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
        source: E,
    },
    /// 交换与恢复均发生故障，目标当前内容无法确定。
    OutcomeUnknown {
        target_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
        source: E,
    },
}

impl<E> fmt::Display for DirectoryPublishError<E>
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
            Self::NotAttempted {
                target_root,
                source,
                cleanup_failure,
            } => {
                write!(
                    formatter,
                    "目录候选尚未开始发布（目标：{}）：{source}",
                    target_root.display()
                )?;
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
            Self::PublishedWithResiduals {
                target_root,
                residual_path,
                source,
            } => write!(
                formatter,
                "目录候选已发布到 {}，但无法清理恢复产物 {}：{source}",
                target_root.display(),
                residual_path.display()
            ),
            Self::RecoveryRequired {
                target_root,
                recovery_artifacts,
                source,
            } => write!(
                formatter,
                "目录发布需要继续恢复（目标：{}，恢复产物：{}）：{source}",
                target_root.display(),
                display_paths(recovery_artifacts)
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

impl<E> Error for DirectoryPublishError<E>
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
            }
            | Self::NotAttempted {
                cleanup_failure, ..
            } => cleanup_failure
                .as_ref()
                .map(|failure| failure as &(dyn Error + 'static)),
            Self::NotPublished { source, .. }
            | Self::PublishedWithResiduals { source, .. }
            | Self::RecoveryRequired { source, .. }
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
pub(crate) struct DirectoryDiscardError<E> {
    staging_root: PathBuf,
    source: E,
}

impl<E> DirectoryDiscardError<E> {
    pub(crate) fn new(staging_root: PathBuf, source: E) -> Self {
        Self {
            staging_root,
            source,
        }
    }

    #[cfg(test)]
    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &E {
        &self.source
    }
}

impl<E> fmt::Display for DirectoryDiscardError<E>
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

impl<E> Error for DirectoryDiscardError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 在目标同级准备并可恢复地发布完整目录的环境根能力。
///
/// `prepare` 必须限制递归复制资源，拒绝符号链接与 reparse point，并且不改变
/// 最终目标。`publish` 必须对同一目标线性化，并将交换、恢复与清理收敛为一个
/// 明确终态。所有操作一旦开始产生副作用，调用方必须等待 future 完成。
pub(crate) trait RecoverableDirectoryPublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type StagingState: Send + 'static;

    fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> impl Future<
        Output = Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>>,
    > + Send;

    fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> impl Future<Output = Result<(), DirectoryPublishError<Self::Error>>> + Send;

    fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> impl Future<Output = Result<(), DirectoryDiscardError<Self::Error>>> + Send;
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
    fn scoped_paths_only_accept_exact_data_and_js_subtrees() {
        for path in ["data", "data/Map001.json", "js", "js/plugins/Quest.js"] {
            assert_eq!(
                ScopedDirectoryPath::new(PathBuf::from(path))
                    .expect("受限候选路径应合法")
                    .as_path(),
                Path::new(path)
            );
        }

        for path in [
            "",
            "Data/Map001.json",
            "data2/file.json",
            "other/file",
            "../data/file",
            "data/../js/file",
            "data/file:stream",
            "C:/data/file",
        ] {
            assert!(
                ScopedDirectoryPath::new(PathBuf::from(path)).is_err(),
                "路径必须拒绝：{path}"
            );
        }
    }

    #[test]
    fn stage_request_keeps_all_validated_candidate_parts() {
        let request = DirectoryStageRequest::new(
            PathBuf::from("C:/projects/demo/write_back"),
            DirectoryPublishIntent::ReplaceExisting,
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
                    DirectoryPublishIntent::CreateNew,
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
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source", "data")],
                Vec::new(),
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::EmptyTargetRoot)
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::EmptySourceMappings)
        ));
    }

    #[test]
    fn stage_request_rejects_overlapping_targets_overlays_and_empty_directories() {
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
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
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/data", "data")],
                vec![overlay("data/Items.json"), overlay("data/Items.json")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlappingOverlays { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/data", "data")],
                Vec::new(),
                vec![PathBuf::from("empty"), PathBuf::from("empty/child")],
            ),
            Err(DirectoryStageRequestError::OverlappingEmptyDirectories { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/data", "data")],
                Vec::new(),
                vec![PathBuf::from("data/empty")],
            ),
            Err(DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
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
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/data", "data")],
                vec![overlay("js/plugins.js")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlayOutsideSourceMappings { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
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
            DirectoryPublishIntent::ReplaceExisting,
            42_u8,
        );

        assert_eq!(staged.target_root(), Path::new("target"));
        assert_eq!(staged.staging_root(), Path::new("target.stage"));
        assert_eq!(
            staged.into_parts(),
            (
                PathBuf::from("target"),
                PathBuf::from("target.stage"),
                DirectoryPublishIntent::ReplaceExisting,
                42
            )
        );
    }

    #[test]
    fn project_lease_request_requires_a_root_and_one_project_name_component() {
        assert!(matches!(
            ProjectOperationLeaseRequest::new(PathBuf::new(), OsString::from("game")),
            Err(ProjectOperationLeaseRequestError::EmptyProjectsRoot)
        ));
        for name in ["", ".", "..", "nested/game", "nested\\game", "C:/game"] {
            assert!(matches!(
                ProjectOperationLeaseRequest::new(
                    PathBuf::from("C:/projects"),
                    OsString::from(name),
                ),
                Err(ProjectOperationLeaseRequestError::InvalidProjectDirectoryName { .. })
            ));
        }

        let request = ProjectOperationLeaseRequest::new(
            PathBuf::from("C:/projects"),
            OsString::from("游戏 一"),
        )
        .expect("单个 Unicode 项目目录名应该合法");
        assert_eq!(request.projects_root(), Path::new("C:/projects"));
        assert_eq!(request.project_directory_name(), OsStr::new("游戏 一"));
    }

    #[test]
    fn tree_fingerprint_request_requires_non_overlapping_safe_logical_roots() {
        assert!(matches!(
            DirectoryTreeFingerprintRequest::new(Vec::new()),
            Err(DirectoryTreeFingerprintRequestError::EmptyRoots)
        ));
        for logical in ["", ".", "../data", "data/../js", "/data", "C:/data"] {
            assert!(matches!(
                DirectoryTreeRoot::new(PathBuf::from("physical"), PathBuf::from(logical)),
                Err(DirectoryTreeFingerprintRequestError::InvalidLogicalRoot { .. })
            ));
        }
        assert!(matches!(
            DirectoryTreeFingerprintRequest::new(vec![
                DirectoryTreeRoot::new(PathBuf::from("physical/data"), PathBuf::from("data"))
                    .expect("data 逻辑根应该合法"),
                DirectoryTreeRoot::new(PathBuf::from("physical/maps"), PathBuf::from("data/maps"),)
                    .expect("data/maps 逻辑根应该合法"),
            ]),
            Err(DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots { .. })
        ));

        let request = DirectoryTreeFingerprintRequest::new(vec![
            DirectoryTreeRoot::new(PathBuf::from("physical/data"), PathBuf::from("data"))
                .expect("data 逻辑根应该合法"),
            DirectoryTreeRoot::new(PathBuf::from("physical/js"), PathBuf::from("js"))
                .expect("js 逻辑根应该合法"),
        ])
        .expect("data 与 js 逻辑根互不重叠");
        assert_eq!(request.roots().len(), 2);
    }

    struct SendContractPublisher;

    struct SendContractLeaseProvider;

    impl ProjectOperationLeaseProvider for SendContractLeaseProvider {
        type Error = Infallible;
        type LeaseState = ();

        async fn acquire_project_operation_lease(
            &self,
            _request: ProjectOperationLeaseRequest,
        ) -> Result<ProjectOperationLease<Self::LeaseState>, ProjectOperationLeaseError<Self::Error>>
        {
            Ok(ProjectOperationLease::new(()))
        }
    }

    struct SendContractFingerprinter;

    impl DirectoryTreeFingerprinter for SendContractFingerprinter {
        type Error = Infallible;

        async fn fingerprint_directory_tree(
            &self,
            _request: DirectoryTreeFingerprintRequest,
        ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
            Ok(Sha256Fingerprint::from_bytes([0; 32]))
        }
    }

    impl RecoverableDirectoryPublisher for SendContractPublisher {
        type Error = Infallible;
        type StagingState = ();

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>>
        {
            Ok(StagedDirectory::new(
                request.target_root().to_path_buf(),
                PathBuf::from("stage"),
                request.publish_intent(),
                (),
            ))
        }

        async fn publish(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryPublishError<Self::Error>> {
            Ok(())
        }

        async fn discard(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryDiscardError<Self::Error>> {
            Ok(())
        }
    }

    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn every_root_operation_returns_a_send_future() {
        let publisher = SendContractPublisher;
        let lease_provider = SendContractLeaseProvider;
        let fingerprinter = SendContractFingerprinter;
        let request = DirectoryStageRequest::new(
            PathBuf::from("target"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source", "data")],
            Vec::new(),
            Vec::new(),
        )
        .expect("测试准备请求应该合法");

        assert_send(publisher.prepare(request));
        assert_send(publisher.publish(StagedDirectory::new(
            PathBuf::from("target"),
            PathBuf::from("stage"),
            DirectoryPublishIntent::CreateNew,
            (),
        )));
        assert_send(publisher.discard(StagedDirectory::new(
            PathBuf::from("target"),
            PathBuf::from("stage"),
            DirectoryPublishIntent::CreateNew,
            (),
        )));
        assert_send(
            lease_provider.acquire_project_operation_lease(
                ProjectOperationLeaseRequest::new(
                    PathBuf::from("C:/projects"),
                    OsString::from("game"),
                )
                .expect("项目租约请求应该合法"),
            ),
        );
        assert_send(
            fingerprinter.fingerprint_directory_tree(
                DirectoryTreeFingerprintRequest::new(vec![
                    DirectoryTreeRoot::new(PathBuf::from("source/data"), PathBuf::from("data"))
                        .expect("目录树根应该合法"),
                ])
                .expect("目录树指纹请求应该合法"),
            ),
        );
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
            DirectoryPublishError::TargetAlreadyExists {
                target_root: PathBuf::from("target"),
                cleanup_failure: cleanup(),
            },
            DirectoryPublishError::TargetMissing {
                target_root: PathBuf::from("target"),
                cleanup_failure: cleanup(),
            },
            DirectoryPublishError::TargetNotDirectory {
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

        let error = DirectoryPublishError::NotPublished {
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
        let published = DirectoryPublishError::PublishedWithResiduals {
            target_root: PathBuf::from("target"),
            residual_path: PathBuf::from("target.backup"),
            source: TestError("backup cleanup failed"),
        };
        assert!(published.to_string().contains("已发布"));
        assert!(published.to_string().contains("target.backup"));

        let unknown = DirectoryPublishError::OutcomeUnknown {
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
        let prepare = DirectoryPrepareError::NotPrepared {
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

        let discard =
            DirectoryDiscardError::new(PathBuf::from("target.stage"), TestError("delete failed"));
        assert_eq!(discard.staging_root(), Path::new("target.stage"));
        assert_eq!(discard.source().0, "delete failed");
        assert!(discard.to_string().contains("target.stage"));
        assert_eq!(
            Error::source(&discard).map(ToString::to_string),
            Some("delete failed".to_owned())
        );
    }
}
