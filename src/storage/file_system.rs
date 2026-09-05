//! 文件系统能力入口及完整读取、目录解析与直接子项列举契约。

mod candidate;
mod fingerprint;
mod lease;
mod path_index;
mod publication;
mod scoped;

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

pub(crate) use super::scoped_path::ScopedDirectoryPath;
pub(crate) use candidate::{
    DirectoryFileOverlay, DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, StagedDirectory,
};
pub(crate) use fingerprint::{
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
    DirectoryTreeRoot,
};
pub(crate) use lease::{
    ExclusiveFileLease, ExclusiveFileLeaseError, ExclusiveFileLeaseProvider,
    ExclusiveFileLeaseRequest,
};
pub(crate) use publication::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublicationDiagnostic,
    DirectoryPublicationDiagnosticSource, DirectoryPublishError, DirectoryRecoveryError,
    DirectoryRecoveryOutcome, RecoverableDirectoryPublisher, StagingCleanupFailure,
};
pub(crate) use scoped::{
    BoundScopedDirectory, ScopedDirectoryBindError, ScopedDirectoryEditError,
    ScopedDirectoryEditor, ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryScope,
};

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

/// 提供不会阻塞异步执行器线程的稳定普通文件快照读取能力。
///
/// `read_snapshot_file` 成功前必须固定完整父路径链并拒绝 reparse point，确认最终对象是
/// 单链接普通文件，并复核读取前后的文件身份、长度和链接数。该能力只服务需要把读取字节
/// 纳入受信来源快照的调用方；普通 [`FileReader`] 的语义保持不变。
pub(crate) trait SnapshotFileReader: FileReader {
    /// 读取一个稳定的现存普通文件快照。
    fn read_snapshot_file(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<ReadFile, ReadFileError<Self::Error>>> + Send;
}
