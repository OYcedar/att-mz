//! 文件系统能力契约。

use std::collections::HashMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, PublicationBackendCause, PublicationIssue, PublicationProblem,
    PublicationStep, RelatedFailureRelation, SafePath, StateEffect,
};
use crate::fingerprint::Sha256Fingerprint;

pub(crate) use super::scoped_path::ScopedDirectoryPath;

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

/// 在一个已经受信的现存目录下幂等建立并验证一个直接子目录。
///
/// 调用方只声明单个子目录名；实现不得递归建立父目录，也不得跟随 reparse point。
/// 生产实现负责把阻塞式目录操作隔离到受控执行资源。
pub(crate) trait DirectChildDirectoryEnsurer: Send + Sync {
    /// 底层文件系统错误。
    type Error: Error + Send + Sync + 'static;

    /// 返回已经固定、确认是普通目录的直接子目录路径。
    fn ensure_direct_child_directory(
        &self,
        parent: PathBuf,
        child: OsString,
    ) -> impl Future<Output = Result<PathBuf, Self::Error>> + Send;
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
}

/// 提供不会阻塞异步执行器线程的非递归目录列举能力。
///
/// 成功结果包含目录直接子项的规范化绝对路径和普通文件/目录类别。实现不递归、
/// 不排序，也不按名称筛选；reparse point、非普通对象和硬链接文件不形成受信结果。
/// 生产实现负责用程序拥有的有界执行资源隔离阻塞式系统调用；调用方只会自然背压或取消，
/// 不承担 worker、队列或准入窗口配置。
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

/// 在指定文件身份上取得跨进程排他租约的受检请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExclusiveFileLeaseRequest {
    lock_directory: PathBuf,
    identity: OsString,
}

impl ExclusiveFileLeaseRequest {
    pub(crate) fn new(
        lock_directory: PathBuf,
        identity: OsString,
    ) -> Result<Self, ExclusiveFileLeaseRequestError> {
        if lock_directory.as_os_str().is_empty() {
            return Err(ExclusiveFileLeaseRequestError::EmptyLockDirectory);
        }
        if identity.is_empty() {
            return Err(ExclusiveFileLeaseRequestError::EmptyIdentity);
        }
        Ok(Self {
            lock_directory,
            identity,
        })
    }

    pub(crate) fn lock_directory(&self) -> &Path {
        &self.lock_directory
    }

    pub(crate) fn identity(&self) -> &OsStr {
        &self.identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExclusiveFileLeaseRequestError {
    EmptyLockDirectory,
    EmptyIdentity,
}

impl fmt::Display for ExclusiveFileLeaseRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyLockDirectory => formatter.write_str("排他文件租约目录不能为空"),
            Self::EmptyIdentity => formatter.write_str("排他文件租约身份不能为空"),
        }
    }
}

impl Error for ExclusiveFileLeaseRequestError {}

/// 持有一个跨进程排他文件租约直到本值被丢弃。
#[must_use = "排他文件租约必须存活到需要串行化的操作结束"]
pub(crate) struct ExclusiveFileLease<T> {
    _state: T,
}

impl<T> ExclusiveFileLease<T> {
    pub(crate) const fn new(state: T) -> Self {
        Self { _state: state }
    }
}

#[derive(Debug)]
pub(crate) enum ExclusiveFileLeaseError<E> {
    Unavailable { identity: OsString, source: E },
}

impl<E: fmt::Display> fmt::Display for ExclusiveFileLeaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { identity, source } => write!(
                formatter,
                "无法取得排他文件租约 {}：{source}",
                identity.to_string_lossy()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ExclusiveFileLeaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable { source, .. } => Some(source),
        }
    }
}

/// 为一个调用方声明的文件身份提供跨进程排他租约。
pub(crate) trait ExclusiveFileLeaseProvider: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type LeaseState: Send + 'static;

    fn acquire_exclusive_file_lease(
        &self,
        request: ExclusiveFileLeaseRequest,
    ) -> impl Future<
        Output = Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>,
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
        let logical_roots = roots
            .iter()
            .map(DirectoryTreeRoot::logical_root)
            .collect::<Vec<_>>();
        if let Some((first, second)) = overlapping_later_paths(&logical_roots)
            .iter()
            .enumerate()
            .find_map(|(first, second)| second.map(|second| (first, second)))
        {
            return Err(
                DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots {
                    first: roots[first].logical_root().to_path_buf(),
                    second: roots[second].logical_root().to_path_buf(),
                },
            );
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
        // 空目标只在来源映射中表示“把这棵来源目录作为候选根”。Overlay 和显式空目录
        // 仍必须是非空普通相对路径，不能借此表达候选根本身。
        if !relative_target.as_os_str().is_empty() {
            validate_stage_relative_path(&relative_target)?;
        }
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

        let source_paths = source_mappings
            .iter()
            .map(DirectorySourceMapping::relative_target)
            .collect::<Vec<_>>();
        let source_overlaps = overlapping_later_paths(&source_paths);
        if let Some((first, second)) = source_overlaps
            .iter()
            .enumerate()
            .find_map(|(first, second)| second.map(|second| (first, second)))
        {
            return Err(DirectoryStageRequestError::OverlappingSourceTargets {
                first: source_paths[first].to_path_buf(),
                second: source_paths[second].to_path_buf(),
            });
        }
        let source_index = RelativePathIndex::from_paths(&source_paths);

        let overlay_paths = overlays
            .iter()
            .map(DirectoryFileOverlay::relative_file)
            .collect::<Vec<_>>();
        let overlay_overlaps = overlapping_later_paths(&overlay_paths);
        for (index, overlay) in overlays.iter().enumerate() {
            if let Some(other) = overlay_overlaps[index] {
                return Err(DirectoryStageRequestError::OverlappingOverlays {
                    first: overlay.relative_file().to_path_buf(),
                    second: overlays[other].relative_file().to_path_buf(),
                });
            }
            if source_index
                .first_strict_ancestor(overlay.relative_file())
                .is_none()
            {
                return Err(DirectoryStageRequestError::OverlayOutsideSourceMappings {
                    relative_file: overlay.relative_file().to_path_buf(),
                });
            }
        }
        let overlay_index = RelativePathIndex::from_paths(&overlay_paths);

        for directory in &empty_directories {
            validate_stage_relative_path(directory)?;
        }
        let empty_paths = empty_directories
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let empty_overlaps = overlapping_later_paths(&empty_paths);
        for (index, directory) in empty_directories.iter().enumerate() {
            if let Some(other) = empty_overlaps[index] {
                return Err(DirectoryStageRequestError::OverlappingEmptyDirectories {
                    first: directory.to_path_buf(),
                    second: empty_directories[other].to_path_buf(),
                });
            }
            if let Some(overlay) = overlay_index.first_overlapping(directory) {
                return Err(DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
                    empty_directory: directory.to_path_buf(),
                    overlay: overlays[overlay].relative_file().to_path_buf(),
                });
            }
            if let Some(mapping) = source_index.first_overlapping(directory) {
                return Err(
                    DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
                        empty_directory: directory.to_path_buf(),
                        source_target: source_mappings[mapping].relative_target().to_path_buf(),
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

#[derive(Default)]
struct RelativePathIndex {
    root: RelativePathIndexNode,
}

#[derive(Default)]
struct RelativePathIndexNode {
    children: HashMap<OsString, Self>,
    terminal_min_ordinal: Option<usize>,
    subtree_min_ordinal: Option<usize>,
}

impl Drop for RelativePathIndex {
    fn drop(&mut self) {
        // 路径深度只受真实文件系统约束；显式排空堆上节点，避免深声明在析构时递归
        // 穿过 HashMap 子节点并耗尽 Rust 调用栈。
        let mut pending = self
            .root
            .children
            .drain()
            .map(|(_, child)| child)
            .collect::<Vec<_>>();
        while let Some(mut node) = pending.pop() {
            pending.extend(node.children.drain().map(|(_, child)| child));
        }
    }
}

impl RelativePathIndex {
    fn from_paths(paths: &[&Path]) -> Self {
        let mut index = Self::default();
        for (ordinal, path) in paths.iter().enumerate() {
            index.insert(path, ordinal);
        }
        index
    }

    fn insert(&mut self, path: &Path, ordinal: usize) {
        let mut node = &mut self.root;
        node.subtree_min_ordinal = min_ordinal(node.subtree_min_ordinal, ordinal);
        for component in path.components() {
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            node = node.children.entry(component.to_os_string()).or_default();
            node.subtree_min_ordinal = min_ordinal(node.subtree_min_ordinal, ordinal);
        }
        node.terminal_min_ordinal = min_ordinal(node.terminal_min_ordinal, ordinal);
    }

    /// 返回输入顺序最早、与 `path` 相同或互为祖先的声明。
    fn first_overlapping(&self, path: &Path) -> Option<usize> {
        let mut node = &self.root;
        let mut candidate = node.terminal_min_ordinal;
        for component in path.components() {
            candidate = min_optional_ordinal(candidate, node.terminal_min_ordinal);
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            let Some(child) = node.children.get(component) else {
                return candidate;
            };
            node = child;
        }
        min_optional_ordinal(candidate, node.subtree_min_ordinal)
    }

    /// 返回输入顺序最早、且是 `path` 严格祖先的声明。
    fn first_strict_ancestor(&self, path: &Path) -> Option<usize> {
        let mut node = &self.root;
        let mut candidate = node.terminal_min_ordinal;
        for component in path.components() {
            candidate = min_optional_ordinal(candidate, node.terminal_min_ordinal);
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            let Some(child) = node.children.get(component) else {
                return candidate;
            };
            node = child;
        }
        candidate
    }
}

fn min_ordinal(current: Option<usize>, ordinal: usize) -> Option<usize> {
    Some(current.map_or(ordinal, |current| current.min(ordinal)))
}

fn min_optional_ordinal(first: Option<usize>, second: Option<usize>) -> Option<usize> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(ordinal), None) | (None, Some(ordinal)) => Some(ordinal),
        (None, None) => None,
    }
}

/// 为每条路径找到输入顺序更晚、且最早与它重叠的路径。
///
/// 反向建立后缀索引，既保持旧契约“先比较较早输入”的错误选择，又把两两互扫
/// 降为与路径组件总数近似线性的工作量。
fn overlapping_later_paths(paths: &[&Path]) -> Vec<Option<usize>> {
    let mut suffix = RelativePathIndex::default();
    let mut overlaps = vec![None; paths.len()];
    for ordinal in (0..paths.len()).rev() {
        overlaps[ordinal] = suffix.first_overlapping(paths[ordinal]);
        suffix.insert(paths[ordinal], ordinal);
    }
    overlaps
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

/// 调用方为一个目录候选声明的可编辑顶层目录集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryScope {
    roots: Vec<OsString>,
}

impl ScopedDirectoryScope {
    pub(crate) fn new(
        roots: impl IntoIterator<Item = OsString>,
    ) -> Result<Self, ScopedDirectoryScopeError> {
        let mut roots = roots.into_iter().collect::<Vec<_>>();
        if roots.is_empty() {
            return Err(ScopedDirectoryScopeError::Empty);
        }
        for root in &roots {
            let path = Path::new(root);
            let mut components = path.components();
            if !matches!(components.next(), Some(Component::Normal(name)) if !name.to_string_lossy().contains(':'))
                || components.next().is_some()
            {
                return Err(ScopedDirectoryScopeError::InvalidRoot { root: root.clone() });
            }
        }
        roots.sort();
        for pair in roots.windows(2) {
            if pair[0] == pair[1] {
                return Err(ScopedDirectoryScopeError::DuplicateRoot {
                    root: pair[0].clone(),
                });
            }
        }
        Ok(Self { roots })
    }

    pub(crate) fn roots(&self) -> &[OsString] {
        &self.roots
    }

    pub(crate) fn contains(&self, path: &ScopedDirectoryPath) -> bool {
        self.roots
            .binary_search_by(|root| root.as_os_str().cmp(path.first_component()))
            .is_ok()
    }

    pub(crate) fn is_scope_root(&self, path: &ScopedDirectoryPath) -> bool {
        path.is_top_level() && self.contains(path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScopedDirectoryScopeError {
    Empty,
    InvalidRoot { root: OsString },
    DuplicateRoot { root: OsString },
}

impl fmt::Display for ScopedDirectoryScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("候选编辑范围必须至少声明一个顶层目录"),
            Self::InvalidRoot { root } => write!(
                formatter,
                "候选编辑范围必须使用单个安全相对路径段：{}",
                root.to_string_lossy()
            ),
            Self::DuplicateRoot { root } => write!(
                formatter,
                "候选编辑范围重复声明顶层目录：{}",
                root.to_string_lossy()
            ),
        }
    }
}

impl Error for ScopedDirectoryScopeError {}

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
    scope: ScopedDirectoryScope,
    state: T,
}

impl<T> BoundScopedDirectory<T> {
    pub(crate) fn new(root: PathBuf, scope: ScopedDirectoryScope, state: T) -> Self {
        Self { root, scope, state }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn state(&self) -> &T {
        &self.state
    }

    pub(crate) fn scope(&self) -> &ScopedDirectoryScope {
        &self.scope
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
    OutsideScope { path: PathBuf },
    ScopeRootMutation { path: PathBuf },
    NotFound { path: PathBuf },
    NotFile { path: PathBuf },
    NotDirectory { path: PathBuf },
    CandidateIdentityChanged { root: PathBuf },
    Failed { path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ScopedDirectoryEditError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEditorInstance => {
                formatter.write_str("候选编辑令牌不能交给另一个文件系统根实例")
            }
            Self::OutsideScope { path } => {
                write!(
                    formatter,
                    "候选路径不在调用方声明的编辑范围内：{}",
                    path.display()
                )
            }
            Self::ScopeRootMutation { path } => {
                write!(formatter, "不能修改候选编辑子树根：{}", path.display())
            }
            Self::NotFound { path } => write!(formatter, "候选路径不存在：{}", path.display()),
            Self::NotFile { path } => write!(formatter, "候选路径不是文件：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "候选路径不是目录：{}", path.display())
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
            | Self::OutsideScope { .. }
            | Self::ScopeRootMutation { .. }
            | Self::NotFound { .. }
            | Self::NotFile { .. }
            | Self::NotDirectory { .. }
            | Self::CandidateIdentityChanged { .. } => None,
        }
    }
}

/// 在一个未发布目录候选的调用方声明子树中执行受限文件操作。
///
/// `bind_scoped_directory` 只绑定候选根物理身份并验证声明范围根，不得为绑定重复枚举
/// 完整候选树。后续操作只重验当前目标、祖先和根身份，必须拒绝 reparse point 与
/// 硬链接；调用返回前该次操作已经终结。完整候选树只由发布根在整体交换前验证一次。
pub(crate) trait ScopedDirectoryEditor: Send + Sync {
    type CandidateState: Send + 'static;
    type ScopeState: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
        scope: ScopedDirectoryScope,
    ) -> impl Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<Self>;

    fn list_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send;

    /// 列举候选根的全部直接子项；调用方据此拥有自身的顶层结构语义。
    fn list_scoped_root(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
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
}

/// 根实现无法删除的暂存或恢复产物。
#[derive(Debug)]
pub(crate) struct StagingCleanupFailure<E> {
    residual_path: PathBuf,
    source: E,
}

/// 底层文件系统根把自己的封闭叶子报告提供给目录发布语义所有者。
pub(crate) trait DirectoryPublicationDiagnosticSource {
    fn publication_diagnostic(&self, step: PublicationStep) -> DirectoryPublicationDiagnostic;
}

pub(crate) struct DirectoryPublicationDiagnostic {
    effect: StateEffect,
    primary: PublicationBackendCause,
    related: Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
}

impl DirectoryPublicationDiagnostic {
    pub(crate) fn new(primary: PublicationBackendCause) -> Self {
        Self {
            effect: StateEffect::Unchanged,
            primary,
            related: Vec::new(),
        }
    }

    /// 根实现保留已知状态影响，发布层不能在包装原因时将其降级。
    pub(crate) fn with_effect(mut self, effect: StateEffect) -> Self {
        self.effect = effect;
        self
    }

    pub(crate) fn with_related(
        mut self,
        relation: RelatedFailureRelation,
        related: DirectoryPublicationDiagnostic,
    ) -> Self {
        self.related.push((relation, related));
        self
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        StateEffect,
        PublicationBackendCause,
        Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
    ) {
        (self.effect, self.primary, self.related)
    }
}

fn publication_report(
    effect: StateEffect,
    step: PublicationStep,
    problem: PublicationProblem,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::publication(PublicationIssue::new(step, problem)),
    )
}

fn cleanup_report<E>(failure: &StagingCleanupFailure<E>) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let projection = failure
        .source()
        .publication_diagnostic(PublicationStep::CleanupResidual);
    let (source_effect, cause, related) = projection.into_parts();
    attach_backend_related(
        publication_report(
            StateEffect::RecoveryRequired.strongest(source_effect),
            PublicationStep::CleanupResidual,
            PublicationProblem::CleanupFailed {
                residual_path: SafePath::new(failure.residual_path()),
                cause,
            },
        ),
        related,
    )
}

fn attach_backend_related(
    mut report: DiagnosticReport,
    related: Vec<(RelatedFailureRelation, DirectoryPublicationDiagnostic)>,
) -> DiagnosticReport {
    for (relation, projection) in related {
        let (effect, cause, nested) = projection.into_parts();
        let related_report = attach_backend_related(
            DiagnosticReport::new(effect, cause.into_diagnostic()),
            nested,
        );
        report = report.with_related(relation, related_report);
    }
    report
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
    NotPrepared {
        target_root: PathBuf,
        source: E,
        cleanup_failure: Option<StagingCleanupFailure<E>>,
    },
}

impl<E> DirectoryPrepareError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::NotPrepared {
                target_root,
                source,
                cleanup_failure,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::PrepareCandidate);
                let (source_effect, cause, related) = projection.into_parts();
                let mut report = attach_backend_related(
                    publication_report(
                        StateEffect::Unchanged.strongest(source_effect),
                        PublicationStep::PrepareCandidate,
                        PublicationProblem::PrepareFailed {
                            output_root: SafePath::new(target_root),
                            candidate_root: cleanup_failure
                                .as_ref()
                                .map(|failure| SafePath::new(failure.residual_path())),
                            cause,
                        },
                    ),
                    related,
                );
                if let Some(cleanup_failure) = cleanup_failure {
                    report = report.with_related(
                        RelatedFailureRelation::Cleanup,
                        cleanup_report(cleanup_failure),
                    );
                }
                report
            }
        }
    }
}

/// 显式恢复是否实际处理了属于该目标的受管发布产物。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryRecoveryOutcome {
    Unchanged,
    Recovered,
}

/// 在调用方观察目标状态之前，恢复该目标的受管发布产物失败。
#[derive(Debug)]
pub(crate) struct DirectoryRecoveryError<E> {
    target_root: PathBuf,
    source: E,
}

impl<E> DirectoryRecoveryError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let projection = self.source.publication_diagnostic(PublicationStep::Recover);
        let (source_effect, cause, related) = projection.into_parts();
        attach_backend_related(
            publication_report(
                StateEffect::RecoveryRequired.strongest(source_effect),
                PublicationStep::Recover,
                PublicationProblem::RecoveryFailed {
                    output_root: SafePath::new(&self.target_root),
                    cause,
                },
            ),
            related,
        )
    }
}

impl<E> DirectoryRecoveryError<E> {
    pub(crate) fn new(target_root: PathBuf, source: E) -> Self {
        Self {
            target_root,
            source,
        }
    }

    #[cfg(test)]
    pub(crate) fn source_error(&self) -> &E {
        &self.source
    }
}

impl<E> fmt::Display for DirectoryRecoveryError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "无法恢复目录发布目标 {}：{}",
            self.target_root.display(),
            self.source
        )
    }
}

impl<E> Error for DirectoryRecoveryError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl<E> fmt::Display for DirectoryPrepareError<E>
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

impl<E> Error for DirectoryPrepareError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotPrepared { source, .. } => Some(source),
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

impl<E> DirectoryPublishError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetAlreadyExists {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::TargetMissing {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetMissing {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::TargetNotDirectory {
                target_root,
                cleanup_failure,
            } => publication_state_report(
                cleanup_failure.as_ref(),
                PublicationProblem::TargetNotDirectory {
                    output_root: SafePath::new(target_root),
                },
            ),
            Self::NotAttempted {
                target_root,
                source,
                cleanup_failure,
            } => publication_failure_report(
                source,
                cleanup_failure.as_ref(),
                StateEffect::Unchanged,
                PublicationStep::Publish,
                |cause| PublicationProblem::NotAttempted {
                    output_root: SafePath::new(target_root),
                    cause,
                },
            ),
            Self::NotPublished {
                target_root,
                source,
                cleanup_failure,
            } => publication_failure_report(
                source,
                cleanup_failure.as_ref(),
                StateEffect::Unchanged,
                PublicationStep::Publish,
                |cause| PublicationProblem::NotPublished {
                    output_root: SafePath::new(target_root),
                    cause,
                },
            ),
            Self::PublishedWithResiduals {
                target_root,
                residual_path,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::CleanupResidual);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::AppliedFinalizationFailed.strongest(source_effect),
                        PublicationStep::Finalize,
                        PublicationProblem::PublishedFinalizationFailed {
                            output_root: SafePath::new(target_root),
                            residual_path: SafePath::new(residual_path),
                            cause,
                        },
                    ),
                    related,
                )
            }
            Self::RecoveryRequired {
                target_root,
                recovery_artifacts,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::Recover);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::RecoveryRequired.strongest(source_effect),
                        PublicationStep::Publish,
                        PublicationProblem::RecoveryRequired {
                            output_root: SafePath::new(target_root),
                            recovery_artifacts: recovery_artifacts
                                .iter()
                                .map(SafePath::new)
                                .collect(),
                            cause,
                        },
                    ),
                    related,
                )
            }
            Self::OutcomeUnknown {
                target_root,
                recovery_artifacts,
                source,
            } => {
                let projection = source.publication_diagnostic(PublicationStep::Recover);
                let (source_effect, cause, related) = projection.into_parts();
                attach_backend_related(
                    publication_report(
                        StateEffect::OutcomeUnknown.strongest(source_effect),
                        PublicationStep::Publish,
                        PublicationProblem::OutcomeUnknown {
                            output_root: SafePath::new(target_root),
                            recovery_artifacts: recovery_artifacts
                                .iter()
                                .map(SafePath::new)
                                .collect(),
                            cause,
                        },
                    ),
                    related,
                )
            }
        }
    }
}

fn publication_state_report<E>(
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
    problem: PublicationProblem,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let mut report = publication_report(StateEffect::Unchanged, PublicationStep::Publish, problem);
    if let Some(cleanup_failure) = cleanup_failure {
        report = report.with_related(
            RelatedFailureRelation::Cleanup,
            cleanup_report(cleanup_failure),
        );
    }
    report
}

fn publication_failure_report<E>(
    source: &E,
    cleanup_failure: Option<&StagingCleanupFailure<E>>,
    effect: StateEffect,
    step: PublicationStep,
    problem: impl FnOnce(PublicationBackendCause) -> PublicationProblem,
) -> DiagnosticReport
where
    E: DirectoryPublicationDiagnosticSource,
{
    let projection = source.publication_diagnostic(step);
    let (source_effect, cause, related) = projection.into_parts();
    let mut report = attach_backend_related(
        publication_report(effect.strongest(source_effect), step, problem(cause)),
        related,
    );
    if let Some(cleanup_failure) = cleanup_failure {
        report = report.with_related(
            RelatedFailureRelation::Cleanup,
            cleanup_report(cleanup_failure),
        );
    }
    report
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

impl<E> DirectoryDiscardError<E>
where
    E: DirectoryPublicationDiagnosticSource,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let projection = self
            .source
            .publication_diagnostic(PublicationStep::DiscardCandidate);
        let (source_effect, cause, related) = projection.into_parts();
        attach_backend_related(
            publication_report(
                StateEffect::RecoveryRequired.strongest(source_effect),
                PublicationStep::DiscardCandidate,
                PublicationProblem::DiscardFailed {
                    candidate_root: SafePath::new(&self.staging_root),
                    cause,
                },
            ),
            related,
        )
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
/// `prepare` 必须拒绝符号链接与 reparse point，并且不改变最终目标。`publish` 是
/// 完整候选树的唯一全量验证入口；验证后必须对同一目标线性化，并将一次目录交换、
/// 恢复与清理收敛为一个明确终态。所有操作一旦开始产生副作用，调用方必须等待
/// future 完成。
pub(crate) trait RecoverableDirectoryPublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type StagingState: Send + 'static;

    /// 在调用方观察目标状态之前，恢复或清理当前目标命名空间中的受管发布产物。
    fn recover(
        &self,
        target_root: PathBuf,
    ) -> impl Future<Output = Result<DirectoryRecoveryOutcome, DirectoryRecoveryError<Self::Error>>> + Send;

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
    fn scoped_paths_only_establish_generic_safe_relative_path_invariants() {
        for path in [
            "assets",
            "assets/catalog.json",
            "scripts",
            "scripts/modules/task.lua",
        ] {
            assert_eq!(
                ScopedDirectoryPath::new(PathBuf::from(path))
                    .expect("受限候选路径应合法")
                    .as_path(),
                Path::new(path)
            );
        }

        for path in [
            "",
            "../assets/file",
            "assets/../scripts/file",
            "assets/file:stream",
            "C:/assets/file",
            r"assets\catalog.json",
            "assets//catalog.json",
            "assets/catalog.json/",
            "assets/./catalog.json",
            "assets/catalog.json.",
            "assets/catalog.json ",
            "assets/cache./catalog.json",
            "assets/cache /catalog.json",
        ] {
            assert!(
                ScopedDirectoryPath::new(PathBuf::from(path)).is_err(),
                "路径必须拒绝：{path}"
            );
        }
    }

    #[test]
    fn scoped_directory_scope_owns_allowed_top_level_directories() {
        let scope =
            ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
                .expect("两个普通顶层目录应该可建立编辑范围");
        let assets = ScopedDirectoryPath::new(PathBuf::from("assets/image.png"))
            .expect("范围内路径应该合法");
        let outside = ScopedDirectoryPath::new(PathBuf::from("other/catalog.json"))
            .expect("通用安全路径不负责业务范围");
        assert!(scope.contains(&assets));
        assert!(!scope.contains(&outside));
        assert!(scope.is_scope_root(
            &ScopedDirectoryPath::new(PathBuf::from("scripts")).expect("范围根路径应该合法")
        ));
        assert!(matches!(
            ScopedDirectoryScope::new(Vec::<OsString>::new()),
            Err(ScopedDirectoryScopeError::Empty)
        ));
        assert!(matches!(
            ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("assets")]),
            Err(ScopedDirectoryScopeError::DuplicateRoot { .. })
        ));
    }

    #[test]
    fn stage_request_keeps_all_validated_candidate_parts() {
        let request = DirectoryStageRequest::new(
            PathBuf::from("C:/workspaces/sample/output"),
            DirectoryPublishIntent::ReplaceExisting,
            vec![
                mapping("C:/workspaces/sample/input/assets", "assets"),
                mapping("C:/workspaces/sample/input/scripts", "scripts"),
            ],
            vec![overlay("assets/catalog.json"), overlay("scripts/main.lua")],
            vec![PathBuf::from("logs"), PathBuf::from("empty/cache")],
        )
        .expect("标准目录候选请求应该合法");

        assert_eq!(
            request.target_root(),
            Path::new("C:/workspaces/sample/output")
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
            ".",
            "../assets",
            "assets/../scripts",
            "assets/./catalog.json",
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
        assert!(
            DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::new()).is_ok(),
            "来源映射可以精确声明候选根"
        );
        assert!(matches!(
            DirectoryFileOverlay::new(PathBuf::new(), Vec::new()),
            Err(DirectoryStageRequestError::InvalidRelativePath { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source", "source")],
                Vec::new(),
                vec![PathBuf::new()],
            ),
            Err(DirectoryStageRequestError::InvalidRelativePath { .. })
        ));
    }

    #[test]
    fn root_source_mapping_owns_the_whole_candidate_and_must_be_unique() {
        let request = DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::ReplaceExisting,
            vec![
                DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::new())
                    .expect("根来源映射应合法"),
            ],
            vec![overlay("dialogue.jsonl"), overlay("nested/name.jsonl")],
            Vec::new(),
        )
        .expect("根来源映射应覆盖全部文件");
        assert!(
            request.source_mappings()[0]
                .relative_target()
                .as_os_str()
                .is_empty()
        );

        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::ReplaceExisting,
                vec![
                    DirectorySourceMapping::new(PathBuf::from("source/root"), PathBuf::new(),)
                        .expect("根来源映射应合法"),
                    mapping("source/nested", "nested"),
                ],
                Vec::new(),
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlappingSourceTargets { .. })
        ));
    }

    #[test]
    fn stage_request_rejects_missing_roots_and_sources() {
        assert!(matches!(
            DirectorySourceMapping::new(PathBuf::new(), PathBuf::from("assets")),
            Err(DirectoryStageRequestError::EmptySourceDirectory)
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::new(),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source", "assets")],
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
                    mapping("source/assets", "assets"),
                    mapping("source/catalog", "assets/catalog")
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
                vec![mapping("source/assets", "assets")],
                vec![
                    overlay("assets/catalog.json"),
                    overlay("assets/catalog.json")
                ],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlappingOverlays { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/assets", "assets")],
                Vec::new(),
                vec![PathBuf::from("empty"), PathBuf::from("empty/child")],
            ),
            Err(DirectoryStageRequestError::OverlappingEmptyDirectories { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/assets", "assets")],
                Vec::new(),
                vec![PathBuf::from("assets/empty")],
            ),
            Err(DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/assets", "assets")],
                vec![overlay("assets/catalog.json")],
                vec![PathBuf::from("assets/catalog.json/child")],
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
                vec![mapping("source/assets", "assets")],
                vec![overlay("scripts/main.lua")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlayOutsideSourceMappings { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source/assets", "assets")],
                vec![overlay("assets")],
                Vec::new(),
            ),
            Err(DirectoryStageRequestError::OverlayOutsideSourceMappings { .. })
        ));
    }

    #[test]
    fn stage_request_prefix_index_preserves_input_order_error_selection() {
        let error = DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![
                mapping("source/z", "z"),
                mapping("source/a", "a"),
                mapping("source/a-child", "a/child"),
                mapping("source/z-child", "z/child"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect_err("输入顺序最早的重叠声明必须失败");
        assert_eq!(
            error,
            DirectoryStageRequestError::OverlappingSourceTargets {
                first: PathBuf::from("z"),
                second: PathBuf::from("z/child"),
            }
        );

        let error = DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            vec![
                overlay("scripts/outside.js"),
                overlay("assets/catalog.json"),
                overlay("assets/catalog.json/child"),
            ],
            Vec::new(),
        )
        .expect_err("较早覆盖的来源范围错误必须先于较晚覆盖间冲突");
        assert_eq!(
            error,
            DirectoryStageRequestError::OverlayOutsideSourceMappings {
                relative_file: PathBuf::from("scripts/outside.js"),
            }
        );
    }

    #[test]
    fn stage_request_accepts_many_disjoint_overlays_without_pairwise_scanning() {
        let overlays = (0..10_000)
            .map(|ordinal| overlay(&format!("assets/file-{ordinal:05}.json")))
            .collect();
        let request = DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            overlays,
            Vec::new(),
        )
        .expect("互不重叠的大型覆盖 manifest 应通过前缀索引一次校验");
        assert_eq!(request.overlays().len(), 10_000);
    }

    #[test]
    fn path_index_drops_deep_declarations_without_recursive_rust_stack_use() {
        let mut deep = PathBuf::new();
        for _ in 0..20_000 {
            deep.push("d");
        }
        let mut descendant = deep.clone();
        descendant.push("file.json");

        let index = RelativePathIndex::from_paths(&[deep.as_path()]);
        assert_eq!(index.first_strict_ancestor(&descendant), Some(0));
        drop(index);
    }

    #[test]
    fn suffix_path_index_matches_pairwise_overlap_and_earliest_ordinal_semantics() {
        let paths = [
            "z",
            "a/first",
            "other/value",
            "a",
            "z/last",
            "other/value",
            "independent",
        ]
        .map(PathBuf::from);
        let path_refs = paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        let expected = path_refs
            .iter()
            .enumerate()
            .map(|(first, path)| {
                path_refs
                    .iter()
                    .enumerate()
                    .skip(first + 1)
                    .find_map(|(second, other)| {
                        (path == other || path.starts_with(other) || other.starts_with(path))
                            .then_some(second)
                    })
            })
            .collect::<Vec<_>>();

        assert_eq!(overlapping_later_paths(&path_refs), expected);
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
    fn exclusive_file_lease_request_requires_a_directory_and_identity() {
        assert!(matches!(
            ExclusiveFileLeaseRequest::new(PathBuf::new(), OsString::from("game")),
            Err(ExclusiveFileLeaseRequestError::EmptyLockDirectory)
        ));
        assert!(matches!(
            ExclusiveFileLeaseRequest::new(
                PathBuf::from("C:/workspaces/locks/leases"),
                OsString::new(),
            ),
            Err(ExclusiveFileLeaseRequestError::EmptyIdentity)
        ));

        let request = ExclusiveFileLeaseRequest::new(
            PathBuf::from("C:/workspaces/locks/leases"),
            OsString::from("游戏 一"),
        )
        .expect("Unicode 文件租约身份应该合法");
        assert_eq!(
            request.lock_directory(),
            Path::new("C:/workspaces/locks/leases")
        );
        assert_eq!(request.identity(), OsStr::new("游戏 一"));
    }

    #[test]
    fn tree_fingerprint_request_requires_non_overlapping_safe_logical_roots() {
        assert!(matches!(
            DirectoryTreeFingerprintRequest::new(Vec::new()),
            Err(DirectoryTreeFingerprintRequestError::EmptyRoots)
        ));
        for logical in [
            "",
            ".",
            "../assets",
            "assets/../scripts",
            "/assets",
            "C:/assets",
        ] {
            assert!(matches!(
                DirectoryTreeRoot::new(PathBuf::from("physical"), PathBuf::from(logical)),
                Err(DirectoryTreeFingerprintRequestError::InvalidLogicalRoot { .. })
            ));
        }
        assert!(matches!(
            DirectoryTreeFingerprintRequest::new(vec![
                DirectoryTreeRoot::new(PathBuf::from("physical/assets"), PathBuf::from("assets"))
                    .expect("资源逻辑根应该合法"),
                DirectoryTreeRoot::new(
                    PathBuf::from("physical/catalog"),
                    PathBuf::from("assets/catalog"),
                )
                .expect("资源子逻辑根应该合法"),
            ]),
            Err(DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots { .. })
        ));

        let request = DirectoryTreeFingerprintRequest::new(vec![
            DirectoryTreeRoot::new(PathBuf::from("physical/assets"), PathBuf::from("assets"))
                .expect("资源逻辑根应该合法"),
            DirectoryTreeRoot::new(PathBuf::from("physical/scripts"), PathBuf::from("scripts"))
                .expect("脚本逻辑根应该合法"),
        ])
        .expect("资源与脚本逻辑根互不重叠");
        assert_eq!(request.roots().len(), 2);
    }

    struct SendContractPublisher;

    struct SendContractDirectoryEnsurer;

    impl DirectChildDirectoryEnsurer for SendContractDirectoryEnsurer {
        type Error = Infallible;

        async fn ensure_direct_child_directory(
            &self,
            parent: PathBuf,
            child: OsString,
        ) -> Result<PathBuf, Self::Error> {
            Ok(parent.join(child))
        }
    }

    struct SendContractLeaseProvider;

    impl ExclusiveFileLeaseProvider for SendContractLeaseProvider {
        type Error = Infallible;
        type LeaseState = ();

        async fn acquire_exclusive_file_lease(
            &self,
            _request: ExclusiveFileLeaseRequest,
        ) -> Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>
        {
            Ok(ExclusiveFileLease::new(()))
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

        async fn recover(
            &self,
            _target_root: PathBuf,
        ) -> Result<DirectoryRecoveryOutcome, DirectoryRecoveryError<Self::Error>> {
            Ok(DirectoryRecoveryOutcome::Unchanged)
        }

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
        let directory_ensurer = SendContractDirectoryEnsurer;
        let lease_provider = SendContractLeaseProvider;
        let fingerprinter = SendContractFingerprinter;
        let request = DirectoryStageRequest::new(
            PathBuf::from("target"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source", "assets")],
            Vec::new(),
            Vec::new(),
        )
        .expect("测试准备请求应该合法");

        assert_send(publisher.prepare(request));
        assert_send(publisher.recover(PathBuf::from("target")));
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
            directory_ensurer.ensure_direct_child_directory(
                PathBuf::from("C:/workspaces"),
                OsString::from("mz"),
            ),
        );
        assert_send(
            lease_provider.acquire_exclusive_file_lease(
                ExclusiveFileLeaseRequest::new(
                    PathBuf::from("C:/workspaces/locks/leases"),
                    OsString::from("game"),
                )
                .expect("文件租约请求应该合法"),
            ),
        );
        assert_send(
            fingerprinter.fingerprint_directory_tree(
                DirectoryTreeFingerprintRequest::new(vec![
                    DirectoryTreeRoot::new(PathBuf::from("source/assets"), PathBuf::from("assets"))
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

    impl DirectoryPublicationDiagnosticSource for TestError {
        fn publication_diagnostic(&self, step: PublicationStep) -> DirectoryPublicationDiagnostic {
            let operation = match step {
                PublicationStep::Recover => crate::diagnostic::FileSystemOperation::RecoverTarget,
                PublicationStep::PrepareCandidate => {
                    crate::diagnostic::FileSystemOperation::PrepareCandidate
                }
                PublicationStep::Publish | PublicationStep::Finalize => {
                    crate::diagnostic::FileSystemOperation::Rename
                }
                PublicationStep::DiscardCandidate | PublicationStep::CleanupResidual => {
                    crate::diagnostic::FileSystemOperation::Remove
                }
            };
            let projection = DirectoryPublicationDiagnostic::new(PublicationBackendCause::new(
                Diagnostic::file_system(crate::diagnostic::FileSystemIssue::new(
                    crate::diagnostic::FileSystemDiagnosticContext::new(
                        crate::diagnostic::FileSystemDiagnosticStage::Publication,
                        operation,
                    ),
                    crate::diagnostic::FileSystemProblem::ExecutorClosed,
                )),
            ));
            if self.0 == "backend rollback failed" {
                projection.with_related(
                    RelatedFailureRelation::Rollback,
                    DirectoryPublicationDiagnostic::new(PublicationBackendCause::new(
                        Diagnostic::file_system(crate::diagnostic::FileSystemIssue::new(
                            crate::diagnostic::FileSystemDiagnosticContext::new(
                                crate::diagnostic::FileSystemDiagnosticStage::Publication,
                                crate::diagnostic::FileSystemOperation::Remove,
                            ),
                            crate::diagnostic::FileSystemProblem::ExecutorClosed,
                        )),
                    )),
                )
            } else {
                projection
            }
        }
    }

    #[test]
    fn published_residual_wire_keeps_output_residual_and_cleanup_relation() {
        let error = DirectoryPublishError::PublishedWithResiduals {
            target_root: PathBuf::from("D:/output/game"),
            residual_path: PathBuf::from("D:/output/.directory-publish/game/backup"),
            source: TestError("must not enter wire"),
        };

        assert_eq!(
            serde_json::to_value(error.diagnostic_report()).expect("发布诊断必须可序列化"),
            serde_json::json!({
                "effect": "applied_finalization_failed",
                "primary": {
                    "code": "publication.finalization_failed",
                    "stage": "publication",
                    "issue": {
                        "family": "publication",
                        "details": {
                            "step": "finalize",
                            "problem": {
                                "kind": "published_finalization_failed",
                                "output_root": "D:/output/game",
                                "residual_path": "D:/output/.directory-publish/game/backup",
                                "cause": {
                                    "diagnostic": {
                                        "code": "filesystem.executor_closed",
                                        "stage": "publication",
                                        "issue": {
                                            "family": "file_system",
                                            "details": {
                                                "context": {
                                                    "stage": "publication",
                                                    "operation": "remove"
                                                },
                                                "problem": {
                                                    "kind": "executor_closed"
                                                }
                                            }
                                        },
                                        "resolution": "retry"
                                    }
                                }
                            }
                        }
                    },
                    "resolution": "preserve_recovery_artifacts"
                },
                "related": []
            })
        );
    }

    #[test]
    fn unpublished_main_and_cleanup_failures_are_one_recursive_report() {
        let error = DirectoryPublishError::NotPublished {
            target_root: PathBuf::from("D:/output/game"),
            source: TestError("publish failed"),
            cleanup_failure: Some(StagingCleanupFailure::new(
                PathBuf::from("D:/output/.directory-publish/game/stage"),
                TestError("cleanup failed"),
            )),
        };
        let value = serde_json::to_value(error.diagnostic_report())
            .expect("主错误和清理错误必须可原子序列化");

        assert_eq!(value["effect"], "recovery_required");
        assert_eq!(value["primary"]["code"], "publication.not_published");
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]["issue"]["family"],
            "file_system"
        );
        assert_eq!(value["related"][0]["relation"], "cleanup");
        assert_eq!(
            value["related"][0]["report"]["primary"]["code"],
            "publication.cleanup_failed"
        );
        assert_eq!(
            value["related"][0]["report"]["primary"]["issue"]["details"]["problem"]["residual_path"],
            "D:/output/.directory-publish/game/stage"
        );
        assert_eq!(
            value["related"][0]["report"]["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]
                ["issue"]["family"],
            "file_system"
        );
        assert_eq!(
            value["related"][0]["report"]["related"],
            serde_json::json!([])
        );
        assert!(!value.to_string().contains("publish failed"));
        assert!(!value.to_string().contains("cleanup failed"));
    }

    #[test]
    fn backend_related_failure_is_lifted_out_of_publication_issue() {
        let error = DirectoryPublishError::NotPublished {
            target_root: PathBuf::from("D:/output/game"),
            source: TestError("backend rollback failed"),
            cleanup_failure: None,
        };
        let value = serde_json::to_value(error.diagnostic_report())
            .expect("底层相关失败必须提升到报告关系树");

        assert_eq!(value["related"][0]["relation"], "rollback");
        assert_eq!(
            value["related"][0]["report"]["primary"]["issue"]["family"],
            "file_system"
        );
        assert_eq!(
            value["related"][0]["report"]["primary"]["issue"]["details"]["context"]["operation"],
            "remove"
        );
        assert_eq!(
            value["related"][0]["report"]["related"],
            serde_json::json!([])
        );
    }

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
