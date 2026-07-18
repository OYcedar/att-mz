//! 有界文件工作池与 Windows 可恢复目录发布根。

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use std::collections::HashSet;
#[cfg(test)]
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::sync::OnceLock;

use async_channel::{Receiver, Sender};
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows_sys::Win32::Globalization::{LCMAP_UPPERCASE, LCMapStringEx, LOCALE_NAME_INVARIANT};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::fingerprint::Sha256Fingerprint;
use crate::storage::file_system::{
    BoundScopedDirectory, DirectChildDirectoryEnsurer, DirectoryDiscardError, DirectoryEntry,
    DirectoryEntryKind, DirectoryFileOverlay, DirectoryLister, DirectoryPrepareError,
    DirectoryPublishError, DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
    ExclusiveFileLease, ExclusiveFileLeaseError, ExclusiveFileLeaseProvider,
    ExclusiveFileLeaseRequest, ExistingDirectoryResolver, FileReader, ListDirectoryError, ReadFile,
    ReadFileError, RecoverableDirectoryPublisher, ResolveDirectoryError, ScopedDirectoryBindError,
    ScopedDirectoryEditError, ScopedDirectoryEditor, ScopedDirectoryEntry,
    ScopedDirectoryEntryKind, ScopedDirectoryPath, ScopedDirectoryScope, StagedDirectory,
    StagingCleanupFailure,
};
use sha2::{Digest, Sha256};

use super::windows::{
    ExclusiveFileLock, FileIdentity, PinnedPath, WindowsFsError,
    delete_empty_directory_if_identity, delete_regular_file_if_identity, number_of_links,
    open_directory, open_read_write_file_without_reparse, pin_directory_without_reparse,
    pin_path_without_reparse, pin_regular_file_for_snapshot_read,
    rename_without_replace_if_identity, secure_uuid_v4,
    validate_local_case_insensitive_ntfs_directory, windows_names_equal,
};

const RESERVED_PREFIX: &str = ".directory-publish-";
const DIRECTORY_TREE_FINGERPRINT_DOMAIN: &[u8] = b"directory-tree-fingerprint";
const JOURNAL_MAX_BYTES: usize = 1024 * 1024;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestPublishFaultPoint {
    BeforeOriginalMove,
    AfterOriginalJournal,
    AfterOriginalMove,
    AfterCandidateIntent,
    BeforeCandidateMove,
    AfterCandidateMove,
    AfterCandidateVisible,
    BeforeRestoreMove,
    BeforeBackupCleanup,
    BeforeJournalCleanup,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestPublishFaultAction {
    Error,
    Abort,
}

#[cfg(test)]
type TestPublishFaultQueue = VecDeque<(TestPublishFaultPoint, TestPublishFaultAction)>;

#[cfg(test)]
static TEST_PUBLISH_FAULTS: OnceLock<Mutex<HashMap<PathBuf, TestPublishFaultQueue>>> =
    OnceLock::new();

#[cfg(test)]
fn register_test_publish_faults(
    target_root: PathBuf,
    faults: impl IntoIterator<Item = (TestPublishFaultPoint, TestPublishFaultAction)>,
) {
    TEST_PUBLISH_FAULTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("目录发布故障测试锁不应中毒")
        .insert(target_root, faults.into_iter().collect());
}

#[cfg(test)]
fn hit_test_publish_fault(target_root: &Path, point: TestPublishFaultPoint) -> bool {
    let Some(faults) = TEST_PUBLISH_FAULTS.get() else {
        return false;
    };
    let mut faults = faults.lock().expect("目录发布故障测试锁不应中毒");
    let Some(queue) = faults.get_mut(target_root) else {
        return false;
    };
    let Some((expected, action)) = queue.front().copied() else {
        faults.remove(target_root);
        return false;
    };
    if expected != point {
        return false;
    }
    queue.pop_front();
    if queue.is_empty() {
        faults.remove(target_root);
    }
    drop(faults);
    match action {
        TestPublishFaultAction::Error => true,
        TestPublishFaultAction::Abort => std::process::abort(),
    }
}

#[cfg(test)]
fn injected_publish_error(operation: &'static str, path: &Path) -> SystemFileSystemError {
    io_error(operation, path, io::Error::other("测试注入的目录发布故障"))
}

/// 通用文件读取、目录观察和独占文件租约使用的资源预算。
#[derive(Clone, Debug)]
pub(crate) struct SystemFileSystemConfig {
    worker_threads: usize,
    queue_capacity: usize,
    max_read_bytes: u64,
    max_directory_entries: usize,
    tree: TreeBudget,
    exclusive_file_lease: ExclusiveFileLeaseConfig,
}

impl SystemFileSystemConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        worker_threads: usize,
        queue_capacity: usize,
        max_read_bytes: u64,
        max_directory_entries: usize,
        tree: TreeBudget,
        exclusive_file_lease: ExclusiveFileLeaseConfig,
    ) -> Result<Self, SystemFileSystemBuildError> {
        if worker_threads == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "runtime.filesystem.worker_threads 必须大于零",
            ));
        }
        if queue_capacity == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "runtime.filesystem.queue_capacity 必须大于零",
            ));
        }
        if max_read_bytes == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "runtime.filesystem.max_read_bytes 必须大于零",
            ));
        }
        if max_directory_entries == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "runtime.filesystem.max_directory_entries 必须大于零",
            ));
        }
        Ok(Self {
            worker_threads,
            queue_capacity,
            max_read_bytes,
            max_directory_entries,
            tree,
            exclusive_file_lease,
        })
    }
}

/// 指纹与发布候选共同使用的唯一递归目录树预算。
#[derive(Clone, Debug)]
pub(crate) struct TreeBudget {
    max_entries: usize,
    max_depth: usize,
    max_bytes: u64,
    max_single_file_bytes: u64,
}

impl TreeBudget {
    pub(crate) fn new(
        max_entries: usize,
        max_depth: usize,
        max_bytes: u64,
        max_single_file_bytes: u64,
    ) -> Result<Self, SystemFileSystemBuildError> {
        if max_entries == 0 || max_depth == 0 || max_bytes == 0 || max_single_file_bytes == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "目录树的数量、深度与字节预算必须全部大于零",
            ));
        }
        if max_single_file_bytes > max_bytes {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "目录树单文件上限不得大于总字节上限",
            ));
        }
        Ok(Self {
            max_entries,
            max_depth,
            max_bytes,
            max_single_file_bytes,
        })
    }
}

/// 通用跨进程独占文件租约的等待策略。
#[derive(Clone, Debug)]
pub(crate) struct ExclusiveFileLeaseConfig {
    timeout: Duration,
}

impl ExclusiveFileLeaseConfig {
    pub(crate) fn new(timeout: Duration) -> Result<Self, SystemFileSystemBuildError> {
        if timeout.is_zero() {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "独占文件租约超时必须大于零",
            ));
        }
        Ok(Self { timeout })
    }
}

/// 目录发布的恢复产物预算与目标锁等待策略。
#[derive(Clone, Debug)]
pub(crate) struct DirectoryPublisherConfig {
    lock_directory: PathBuf,
    max_recovery_artifacts_per_target: usize,
    target_lock_timeout: Duration,
}

impl DirectoryPublisherConfig {
    pub(crate) fn new(
        lock_directory: PathBuf,
        max_recovery_artifacts_per_target: usize,
        target_lock_timeout: Duration,
    ) -> Result<Self, SystemFileSystemBuildError> {
        if lock_directory.as_os_str().is_empty() {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "目录发布锁目录不能为空",
            ));
        }
        if max_recovery_artifacts_per_target == 0 {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "目录发布恢复产物预算必须大于零",
            ));
        }
        Ok(Self {
            lock_directory,
            max_recovery_artifacts_per_target,
            target_lock_timeout,
        })
    }

    #[cfg(test)]
    pub(crate) fn lock_directory(&self) -> &Path {
        &self.lock_directory
    }
}

#[derive(Debug)]
pub(crate) enum SystemFileSystemBuildError {
    InvalidConfiguration(&'static str),
    WorkerSpawn(io::Error),
}

impl fmt::Display for SystemFileSystemBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => formatter.write_str(message),
            Self::WorkerSpawn(source) => write!(formatter, "无法建立文件工作线程：{source}"),
        }
    }
}

impl Error for SystemFileSystemBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkerSpawn(source) => Some(source),
            Self::InvalidConfiguration(_) => None,
        }
    }
}

/// 生产文件系统根的结构化机制错误。
#[derive(Debug)]
pub(crate) enum SystemFileSystemError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Windows(WindowsFsError),
    Closed,
    WorkerPanicked,
    ResourceLimit {
        resource: &'static str,
        limit: u64,
        observed: u64,
    },
    AllocationFailed {
        resource: &'static str,
        bytes: u64,
    },
    InvalidPath {
        path: PathBuf,
        reason: &'static str,
    },
    WrongPublisherInstance,
    InvalidStagedIdentity {
        path: PathBuf,
    },
    DirectChildRollbackFailed {
        path: PathBuf,
        operation: Box<SystemFileSystemError>,
        rollback: Box<SystemFileSystemError>,
    },
    ScopedEditRollbackFailed {
        path: PathBuf,
        operation: Box<SystemFileSystemError>,
        rollback: Box<SystemFileSystemError>,
    },
    JournalCorrupt {
        path: PathBuf,
        reason: String,
    },
    RecoveryRequired {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        reason: String,
    },
    OutcomeUnknown {
        target_root: PathBuf,
        artifacts: Vec<PathBuf>,
        reason: String,
    },
}

impl fmt::Display for SystemFileSystemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} 失败：{source}", path.display()),
            Self::Windows(source) => source.fmt(formatter),
            Self::Closed => formatter.write_str("文件系统根已经停止接收工作"),
            Self::WorkerPanicked => formatter.write_str("文件系统工作线程中的任务发生 panic"),
            Self::ResourceLimit {
                resource,
                limit,
                observed,
            } => write!(
                formatter,
                "{resource} 超过资源上限（上限 {limit}，观测到 {observed}）"
            ),
            Self::AllocationFailed { resource, bytes } => {
                write!(formatter, "无法为{resource}分配 {bytes} 字节内存")
            }
            Self::InvalidPath { path, reason } => {
                write!(formatter, "文件系统路径无效 {}：{reason}", path.display())
            }
            Self::WrongPublisherInstance => formatter.write_str("目录候选被交给了另一个发布根实例"),
            Self::InvalidStagedIdentity { path } => write!(
                formatter,
                "目录候选的物理文件身份已经变化：{}",
                path.display()
            ),
            Self::DirectChildRollbackFailed {
                path,
                operation,
                rollback,
            } => write!(
                formatter,
                "直接子目录 {} 建立后父目录身份发生变化（{operation}），且无法回滚：{rollback}",
                path.display()
            ),
            Self::ScopedEditRollbackFailed {
                path,
                operation,
                rollback,
            } => write!(
                formatter,
                "候选编辑 {} 失败（{operation}），且无法恢复原内容：{rollback}",
                path.display()
            ),
            Self::JournalCorrupt { path, reason } => write!(
                formatter,
                "目录恢复 journal 损坏 {}：{reason}",
                path.display()
            ),
            Self::RecoveryRequired {
                target_root,
                artifacts,
                reason,
            } => write!(
                formatter,
                "目录 {} 需要继续恢复（{}）：{reason}",
                target_root.display(),
                display_paths(artifacts)
            ),
            Self::OutcomeUnknown {
                target_root,
                artifacts,
                reason,
            } => write!(
                formatter,
                "目录 {} 的发布结果无法归类（{}）：{reason}",
                target_root.display(),
                display_paths(artifacts)
            ),
        }
    }
}

impl Error for SystemFileSystemError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Windows(source) => Some(source),
            Self::DirectChildRollbackFailed { operation, .. }
            | Self::ScopedEditRollbackFailed { operation, .. } => Some(operation),
            Self::Closed
            | Self::WorkerPanicked
            | Self::ResourceLimit { .. }
            | Self::AllocationFailed { .. }
            | Self::InvalidPath { .. }
            | Self::WrongPublisherInstance
            | Self::InvalidStagedIdentity { .. }
            | Self::JournalCorrupt { .. }
            | Self::RecoveryRequired { .. }
            | Self::OutcomeUnknown { .. } => None,
        }
    }
}

impl From<WindowsFsError> for SystemFileSystemError {
    fn from(source: WindowsFsError) -> Self {
        Self::Windows(source)
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("、")
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> SystemFileSystemError {
    SystemFileSystemError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

type FileJob = Box<dyn FnOnce() + Send + 'static>;

struct FileWorkPool {
    sender: Sender<FileJob>,
    workers: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl FileWorkPool {
    fn new(
        worker_threads: usize,
        queue_capacity: usize,
    ) -> Result<Self, SystemFileSystemBuildError> {
        let (sender, receiver) = async_channel::bounded(queue_capacity);
        let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(worker_threads);
        for index in 0..worker_threads {
            let receiver = receiver.clone();
            let worker = match thread::Builder::new()
                .name(format!("filesystem-worker-{index}"))
                .spawn(move || file_worker(receiver))
            {
                Ok(worker) => worker,
                Err(source) => {
                    sender.close();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(SystemFileSystemBuildError::WorkerSpawn(source));
                }
            };
            workers.push(worker);
        }
        Ok(Self {
            sender,
            workers: Mutex::new(Some(workers)),
        })
    }

    async fn execute<T, F>(&self, work: F) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (response_sender, response_receiver) = async_channel::bounded(1);
        self.sender
            .send(Box::new(move || {
                let result = work();
                let _ = response_sender.send_blocking(result);
            }))
            .await
            .map_err(|_| SystemFileSystemError::Closed)?;
        response_receiver
            .recv()
            .await
            .map_err(|_| SystemFileSystemError::WorkerPanicked)
    }

    async fn execute_with_abandon<T, F, A>(
        &self,
        work: F,
        abandon: A,
    ) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        A: FnOnce(&mut T) + Send + 'static,
    {
        let (response_sender, response_receiver) = async_channel::bounded(1);
        self.sender
            .send(Box::new(move || {
                let result = work();
                match response_sender.send_blocking(result) {
                    Ok(()) => {}
                    Err(error) => {
                        let mut abandoned_result = error.0;
                        abandon(&mut abandoned_result);
                    }
                }
            }))
            .await
            .map_err(|_| SystemFileSystemError::Closed)?;
        response_receiver
            .recv()
            .await
            .map_err(|_| SystemFileSystemError::WorkerPanicked)
    }

    async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
        self.sender.close();
        let workers = self
            .workers
            .lock()
            .expect("文件工作线程所有权锁不应中毒")
            .take();
        let Some(workers) = workers else {
            return Ok(());
        };
        let (sender, receiver) = async_channel::bounded(1);
        thread::Builder::new()
            .name("filesystem-join".to_owned())
            .spawn(move || {
                let clean = workers.into_iter().all(|worker| worker.join().is_ok());
                let _ = sender.send_blocking(clean);
            })
            .map_err(|source| io_error("建立文件工作池终结线程", Path::new("<runtime>"), source))?;
        if receiver.recv().await.unwrap_or(false) {
            Ok(())
        } else {
            Err(SystemFileSystemError::WorkerPanicked)
        }
    }
}

fn file_worker(receiver: Receiver<FileJob>) {
    while let Ok(job) = receiver.recv_blocking() {
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

struct StageCleanupGuard {
    path: PathBuf,
    expected_identity: FileIdentity,
    armed: bool,
}

impl StageCleanupGuard {
    fn new(path: PathBuf, expected_identity: FileIdentity) -> Self {
        Self {
            path,
            expected_identity,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup(&mut self) -> Result<(), SystemFileSystemError> {
        if !self.armed {
            return Ok(());
        }
        match remove_directory_tree_if_identity(&self.path, self.expected_identity) {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(source) => {
                self.armed = false;
                Err(source)
            }
        }
    }
}

impl Drop for StageCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_directory_tree_if_identity(&self.path, self.expected_identity);
        }
    }
}

/// 递归删除一个已知身份的目录树。
///
/// 根目录在整个枚举期间由无删除共享句柄固定；叶子删除也使用 file ID
/// 复核后的 handle disposition。因此路径在检查与删除之间被替换时，会显式失败而不会
/// 跟随 reparse point 或删除新的根对象。
fn remove_directory_tree_if_identity(
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<(), SystemFileSystemError> {
    let pinned = match pin_directory_without_reparse(path) {
        Ok(pinned) => pinned,
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(source) => return Err(source.into()),
    };
    if FileIdentity::of(pinned.file(), path)? != expected_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: path.to_path_buf(),
        });
    }

    let entries = fs::read_dir(path)
        .map_err(|source| io_error("枚举待清理目录", path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error("读取待清理目录项", path, source))?;
    for entry in entries {
        let child_path = entry.path();
        let child = match pin_path_without_reparse(&child_path) {
            Ok(child) => child,
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(source) => return Err(source.into()),
        };
        let metadata = child.metadata()?;
        let identity = FileIdentity::of(child.file(), &child_path)?;
        drop(child);
        if metadata.is_dir() {
            remove_directory_tree_if_identity(&child_path, identity)?;
        } else if metadata.is_file() {
            delete_regular_file_if_identity(&child_path, identity)?;
        } else {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_path,
                reason: "待清理目录中存在非普通文件系统对象",
            });
        }
    }
    drop(pinned);
    delete_empty_directory_if_identity(path, expected_identity).map_err(Into::into)
}

pub(crate) struct SystemStagingState {
    publisher_identity: Arc<()>,
    operation_id: Uuid,
    parent_root: PathBuf,
    parent_identity: FileIdentity,
    stage_identity: FileIdentity,
    target_lock: Option<ExclusiveFileLock>,
    parent_handle: Option<File>,
    stage_handle: Option<File>,
    journal_path: PathBuf,
    backup_path: PathBuf,
    cleanup: StageCleanupGuard,
    abandoned_before_delivery: bool,
    finalized: bool,
}

impl SystemStagingState {
    fn mark_abandoned_before_delivery(&mut self) {
        self.abandoned_before_delivery = true;
    }
}

impl Drop for SystemStagingState {
    fn drop(&mut self) {
        if !self.finalized && !self.abandoned_before_delivery && !thread::panicking() {
            assert!(
                !self.cleanup.armed,
                "已准备目录 token 未经 publish/discard 直接丢弃"
            );
        }
    }
}

#[derive(Debug)]
pub(crate) struct SystemScopedDirectoryState {
    editor_identity: Arc<()>,
    root_identity: FileIdentity,
}

/// 一个进程内共享、显式关闭的生产文件系统根。
#[derive(Clone)]
pub(crate) struct SystemFileSystem {
    inner: Arc<SystemFileSystemInner>,
}

struct SystemFileSystemInner {
    pool: FileWorkPool,
    config: SystemFileSystemConfig,
}

impl SystemFileSystem {
    pub(crate) fn new(config: SystemFileSystemConfig) -> Result<Self, SystemFileSystemBuildError> {
        let pool = FileWorkPool::new(config.worker_threads, config.queue_capacity)?;
        Ok(Self {
            inner: Arc::new(SystemFileSystemInner { pool, config }),
        })
    }

    /// 按调用方实际需要的发布配置建立目录发布能力。
    pub(crate) fn directory_publisher(
        &self,
        config: DirectoryPublisherConfig,
    ) -> SystemDirectoryPublisher {
        SystemDirectoryPublisher {
            inner: Arc::clone(&self.inner),
            config,
            publisher_identity: Arc::new(()),
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
        self.inner.pool.shutdown().await
    }
}

/// 目录候选、受限编辑和可恢复发布组成的单一能力实例。
///
/// 实例身份同时约束候选 token 和编辑句柄，防止它们被其他发布器终结或修改。
#[derive(Clone)]
pub(crate) struct SystemDirectoryPublisher {
    inner: Arc<SystemFileSystemInner>,
    config: DirectoryPublisherConfig,
    publisher_identity: Arc<()>,
}

impl ExistingDirectoryResolver for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn resolve_existing_directory(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<Output = Result<PathBuf, ResolveDirectoryError<Self::Error>>> + Send
    {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        async move {
            let requested = requested.map_err(|source| ResolveDirectoryError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute(move || resolve_directory_sync(requested))
                .await
                .map_err(|source| ResolveDirectoryError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl DirectChildDirectoryEnsurer for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn ensure_direct_child_directory(
        &self,
        parent: PathBuf,
        child: OsString,
    ) -> impl std::future::Future<Output = Result<PathBuf, Self::Error>> + Send {
        let parent = absolutize(parent);
        let inner = Arc::clone(&self.inner);
        async move {
            let parent = parent?;
            inner
                .pool
                .execute(move || ensure_direct_child_directory_sync(parent, child))
                .await?
        }
    }
}

impl DirectoryLister for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn list_directory(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<
        Output = Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>>,
    > + Send {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        let limit = self.inner.config.max_directory_entries;
        async move {
            let requested = requested.map_err(|source| ListDirectoryError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute(move || list_directory_sync(requested, limit))
                .await
                .map_err(|source| ListDirectoryError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl FileReader for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn read_file(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<Output = Result<ReadFile, ReadFileError<Self::Error>>> + Send
    {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        let max_bytes = self.inner.config.max_read_bytes;
        async move {
            let requested = requested.map_err(|source| ReadFileError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute(move || read_file_sync(requested, max_bytes))
                .await
                .map_err(|source| ReadFileError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl ScopedDirectoryEditor for SystemDirectoryPublisher {
    type CandidateState = SystemStagingState;
    type ScopeState = SystemScopedDirectoryState;
    type Error = Box<SystemFileSystemError>;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
        scope: ScopedDirectoryScope,
    ) -> impl std::future::Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<> {
        let root = candidate.staging_root().to_path_buf();
        let candidate_state = candidate.state();
        let same_instance = Arc::ptr_eq(
            &candidate_state.publisher_identity,
            &self.publisher_identity,
        );
        let finalized = candidate_state.finalized;
        let expected_identity = candidate_state.stage_identity;
        let editor_identity = Arc::clone(&self.publisher_identity);
        let config = self.inner.config.clone();
        let inner = Arc::clone(&self.inner);
        let verified_scope = scope.clone();
        async move {
            if !same_instance {
                return Err(ScopedDirectoryBindError::WrongEditorInstance);
            }
            if finalized {
                return Err(ScopedDirectoryBindError::CandidateFinalized { root });
            }
            let error_root = root.clone();
            let verified_root = root.clone();
            inner
                .pool
                .execute(move || {
                    bind_scoped_directory_sync(
                        &verified_root,
                        expected_identity,
                        &verified_scope,
                        &config,
                    )
                })
                .await
                .map_err(|source| ScopedDirectoryBindError::Failed {
                    root: error_root.clone(),
                    source: Box::new(source),
                })?
                .map_err(|source| match source {
                    SystemFileSystemError::InvalidStagedIdentity { .. } => {
                        ScopedDirectoryBindError::CandidateIdentityChanged {
                            root: error_root.clone(),
                        }
                    }
                    source => ScopedDirectoryBindError::Failed {
                        root: error_root.clone(),
                        source: Box::new(source),
                    },
                })?;
            Ok(BoundScopedDirectory::new(
                root,
                scope,
                SystemScopedDirectoryState {
                    editor_identity,
                    root_identity: expected_identity,
                },
            ))
        }
    }

    fn validate_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>>
    + Send
    + use<> {
        let context = scoped_root_context(self, scope);
        let declared_scope = scope.scope().clone();
        async move {
            let (inner, root, root_identity) = context?;
            let config = inner.config.clone();
            let error_path = PathBuf::from(".");
            let error_root = root.clone();
            inner
                .pool
                .execute(move || {
                    validate_scoped_directory_sync(&root, root_identity, &declared_scope, &config)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path.clone(),
                    source: Box::new(source),
                })?
                .map_err(|source| match source {
                    SystemFileSystemError::InvalidStagedIdentity { .. } => {
                        ScopedDirectoryEditError::CandidateIdentityChanged { root: error_root }
                    }
                    source => ScopedDirectoryEditError::Failed {
                        path: error_path,
                        source: Box::new(source),
                    },
                })
        }
    }

    fn read_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            let error_path = relative.as_path().to_path_buf();
            let max_bytes = inner
                .config
                .max_read_bytes
                .min(inner.config.tree.max_single_file_bytes);
            inner
                .pool
                .execute(move || {
                    read_scoped_file_sync(&root, root_identity, &relative, &absolute, max_bytes)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn list_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            let error_path = relative.as_path().to_path_buf();
            let limit = inner.config.max_directory_entries;
            inner
                .pool
                .execute(move || {
                    list_scoped_directory_sync(&root, root_identity, &relative, &absolute, limit)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn list_scoped_root(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl std::future::Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        let context = scoped_root_context(self, scope);
        async move {
            let (inner, root, root_identity) = context?;
            let limit = inner.config.max_directory_entries;
            inner
                .pool
                .execute(move || list_scoped_root_sync(&root, root_identity, limit))
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(source),
                })?
        }
    }

    fn create_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let scope_root = scope.scope().is_scope_root(&path);
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            if scope_root {
                return Err(ScopedDirectoryEditError::ScopeRootMutation {
                    path: relative.as_path().to_path_buf(),
                });
            }
            let error_path = relative.as_path().to_path_buf();
            let config = inner.config.clone();
            inner
                .pool
                .execute(move || {
                    create_scoped_directory_sync(
                        &root,
                        root_identity,
                        &relative,
                        &absolute,
                        &config,
                    )
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn write_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let scope_root = scope.scope().is_scope_root(&path);
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            if scope_root {
                return Err(ScopedDirectoryEditError::ScopeRootMutation {
                    path: relative.as_path().to_path_buf(),
                });
            }
            let error_path = relative.as_path().to_path_buf();
            let config = inner.config.clone();
            inner
                .pool
                .execute(move || {
                    write_scoped_file_sync(
                        &root,
                        root_identity,
                        &relative,
                        &absolute,
                        bytes,
                        &config,
                    )
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn remove_scoped_path(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let scope_root = scope.scope().is_scope_root(&path);
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            if scope_root {
                return Err(ScopedDirectoryEditError::ScopeRootMutation {
                    path: relative.as_path().to_path_buf(),
                });
            }
            let error_path = relative.as_path().to_path_buf();
            inner
                .pool
                .execute(move || {
                    remove_scoped_path_sync(&root, root_identity, &relative, &absolute)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }
}

type ScopedOperationContext = (
    Arc<SystemFileSystemInner>,
    PathBuf,
    FileIdentity,
    ScopedDirectoryPath,
    PathBuf,
);

type ScopedRootContext = (Arc<SystemFileSystemInner>, PathBuf, FileIdentity);

fn scoped_root_context(
    publisher: &SystemDirectoryPublisher,
    scope: &BoundScopedDirectory<SystemScopedDirectoryState>,
) -> Result<ScopedRootContext, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    if !Arc::ptr_eq(
        &scope.state().editor_identity,
        &publisher.publisher_identity,
    ) {
        return Err(ScopedDirectoryEditError::WrongEditorInstance);
    }
    Ok((
        Arc::clone(&publisher.inner),
        scope.root().to_path_buf(),
        scope.state().root_identity,
    ))
}

fn scoped_operation_context(
    publisher: &SystemDirectoryPublisher,
    scope: &BoundScopedDirectory<SystemScopedDirectoryState>,
    relative: ScopedDirectoryPath,
) -> Result<ScopedOperationContext, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    if !scope.scope().contains(&relative) {
        return Err(ScopedDirectoryEditError::OutsideScope {
            path: relative.as_path().to_path_buf(),
        });
    }
    let (inner, root, root_identity) = scoped_root_context(publisher, scope)?;
    let absolute = root.join(relative.as_path());
    Ok((inner, root, root_identity, relative, absolute))
}

impl ExclusiveFileLeaseProvider for SystemFileSystem {
    type Error = Box<SystemFileSystemError>;
    type LeaseState = ExclusiveFileLock;

    fn acquire_exclusive_file_lease(
        &self,
        request: ExclusiveFileLeaseRequest,
    ) -> impl std::future::Future<
        Output = Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>,
    > + Send {
        let inner = Arc::clone(&self.inner);
        let timeout = self.inner.config.exclusive_file_lease.timeout;
        let identity = request.identity().to_os_string();
        async move {
            let failure_identity = identity.clone();
            inner
                .pool
                .execute(move || acquire_exclusive_file_lease_sync(request, timeout))
                .await
                .map_err(|source| ExclusiveFileLeaseError::Unavailable {
                    identity: failure_identity,
                    source: Box::new(source),
                })?
        }
    }
}

impl DirectoryTreeFingerprinter for SystemFileSystem {
    type Error = Box<SystemFileSystemError>;

    fn fingerprint_directory_tree(
        &self,
        request: DirectoryTreeFingerprintRequest,
    ) -> impl std::future::Future<
        Output = Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>>,
    > + Send {
        let inner = Arc::clone(&self.inner);
        let config = self.inner.config.clone();
        let error_path = request
            .roots()
            .first()
            .expect("受检目录树指纹请求至少包含一个根")
            .physical_root()
            .to_path_buf();
        async move {
            inner
                .pool
                .execute(move || fingerprint_directory_tree_sync(request, &config))
                .await
                .map_err(|source| DirectoryTreeFingerprintError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }
}

impl RecoverableDirectoryPublisher for SystemDirectoryPublisher {
    type Error = Box<SystemFileSystemError>;
    type StagingState = SystemStagingState;

    async fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>> {
        let target_root = request.target_root().to_path_buf();
        let system_config = self.inner.config.clone();
        let publisher_config = self.config.clone();
        let publisher_identity = Arc::clone(&self.publisher_identity);
        let error_target = target_root.clone();
        self.inner
            .pool
            .execute_with_abandon(
                move || {
                    prepare_directory_sync(
                        request,
                        system_config,
                        publisher_config,
                        publisher_identity,
                    )
                },
                |result| {
                    if let Ok(staged) = result {
                        staged.state_mut().mark_abandoned_before_delivery();
                    }
                },
            )
            .await
            .map_err(|source| DirectoryPrepareError::NotPrepared {
                target_root: error_target,
                source: Box::new(source),
                cleanup_failure: None,
            })?
    }

    async fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryPublishError<Self::Error>> {
        let expected_identity = Arc::clone(&self.publisher_identity);
        let system_config = self.inner.config.clone();
        let target_root = staged.target_root().to_path_buf();
        self.inner
            .pool
            .execute(move || publish_directory_sync(staged, &expected_identity, &system_config))
            .await
            .map_err(|source| DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: Vec::new(),
                source: Box::new(source),
            })?
    }

    async fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryDiscardError<Self::Error>> {
        let expected_identity = Arc::clone(&self.publisher_identity);
        let staging_root = staged.staging_root().to_path_buf();
        self.inner
            .pool
            .execute(move || discard_directory_sync(staged, &expected_identity))
            .await
            .map_err(|source| DirectoryDiscardError::new(staging_root.clone(), Box::new(source)))?
            .map_err(|source| DirectoryDiscardError::new(staging_root, Box::new(source)))
    }
}

fn absolutize(path: PathBuf) -> Result<PathBuf, SystemFileSystemError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|source| io_error("读取当前工作目录", Path::new("."), source))
    }
}

fn resolve_directory_sync(
    path: PathBuf,
) -> Result<PathBuf, ResolveDirectoryError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveDirectoryError::NotFound { path });
        }
        Err(source) => {
            return Err(ResolveDirectoryError::Io {
                path: path.clone(),
                source: io_error("读取目录元数据", &path, source),
            });
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ResolveDirectoryError::Io {
            path: path.clone(),
            source: WindowsFsError::ReparsePoint { path }.into(),
        });
    }
    if !metadata.is_dir() {
        return Err(ResolveDirectoryError::NotDirectory { path });
    }
    let pinned = pin_path_without_reparse(&path).map_err(|source| ResolveDirectoryError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    if !pinned
        .metadata()
        .map_err(|source| ResolveDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?
        .is_dir()
    {
        return Err(ResolveDirectoryError::NotDirectory { path });
    }
    Ok(pinned.resolved_path().to_path_buf())
}

fn ensure_direct_child_directory_sync(
    parent: PathBuf,
    child: OsString,
) -> Result<PathBuf, SystemFileSystemError> {
    let parent = validate_local_case_insensitive_ntfs_directory(&parent)?;
    let parent_handle = open_directory(&parent, false)?;
    let parent_identity = FileIdentity::of(&parent_handle, &parent)?;
    let child_path = parent.join(&child);
    validate_windows_name(&child, &child_path)?;
    let created = match fs::create_dir(&child_path) {
        Ok(()) => true,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
        Err(source) => return Err(io_error("建立直接子目录", &child_path, source)),
    };
    let pinned_child = pin_directory_without_reparse(&child_path)?;
    let child_identity = FileIdentity::of(pinned_child.file(), &child_path)?;
    let resolved_child = pinned_child.resolved_path().to_path_buf();
    let parent_unchanged = (|| -> Result<bool, SystemFileSystemError> {
        let current_parent = open_directory(&parent, false)?;
        Ok(FileIdentity::of(&current_parent, &parent)? == parent_identity)
    })();
    let operation = match parent_unchanged {
        Ok(true) => return Ok(resolved_child),
        Ok(false) => SystemFileSystemError::InvalidPath {
            path: parent,
            reason: "建立直接子目录期间父目录身份发生变化",
        },
        Err(source) => source,
    };
    drop(pinned_child);
    drop(parent_handle);
    if !created {
        return Err(operation);
    }
    match delete_empty_directory_if_identity(&child_path, child_identity) {
        Ok(()) => Err(operation),
        Err(rollback) => Err(SystemFileSystemError::DirectChildRollbackFailed {
            path: child_path,
            operation: Box::new(operation),
            rollback: Box::new(rollback.into()),
        }),
    }
}

fn list_directory_sync(
    path: PathBuf,
    limit: usize,
) -> Result<Vec<DirectoryEntry>, ListDirectoryError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ListDirectoryError::NotFound { path });
        }
        Err(source) => {
            return Err(ListDirectoryError::Io {
                path: path.clone(),
                source: io_error("读取目录元数据", &path, source),
            });
        }
    };
    if !metadata.is_dir() {
        return Err(ListDirectoryError::NotDirectory { path });
    }
    let pinned_directory =
        pin_path_without_reparse(&path).map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    if !pinned_directory
        .metadata()
        .map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?
        .is_dir()
    {
        return Err(ListDirectoryError::NotDirectory { path });
    }
    let resolved_directory = pinned_directory.resolved_path().to_path_buf();
    let entries = fs::read_dir(&resolved_directory).map_err(|source| ListDirectoryError::Io {
        path: path.clone(),
        source: io_error("列举目录", &resolved_directory, source),
    })?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: io_error("读取目录项", &path, source),
        })?;
        if result.len() == limit {
            return Err(ListDirectoryError::Io {
                path: path.clone(),
                source: SystemFileSystemError::ResourceLimit {
                    resource: "单目录条目数",
                    limit: limit as u64,
                    observed: limit as u64 + 1,
                },
            });
        }
        let child = entry.path();
        let pinned_child =
            pin_path_without_reparse(&child).map_err(|source| ListDirectoryError::Io {
                path: child.clone(),
                source: source.into(),
            })?;
        let child_metadata = pinned_child
            .metadata()
            .map_err(|source| ListDirectoryError::Io {
                path: child.clone(),
                source: source.into(),
            })?;
        let kind = if child_metadata.is_dir() {
            DirectoryEntryKind::Directory
        } else if child_metadata.is_file() {
            if number_of_links(pinned_child.file(), &child).map_err(|source| {
                ListDirectoryError::Io {
                    path: child.clone(),
                    source: source.into(),
                }
            })? != 1
            {
                return Err(ListDirectoryError::Io {
                    path: child.clone(),
                    source: SystemFileSystemError::InvalidPath {
                        path: child,
                        reason: "目录列举拒绝硬链接文件",
                    },
                });
            }
            DirectoryEntryKind::RegularFile
        } else {
            return Err(ListDirectoryError::Io {
                path: child.clone(),
                source: SystemFileSystemError::InvalidPath {
                    path: child,
                    reason: "目录列举包含非普通文件系统对象",
                },
            });
        };
        result.push(DirectoryEntry::new(
            pinned_child.resolved_path().to_path_buf(),
            kind,
        ));
    }
    Ok(result)
}

fn read_file_sync(
    path: PathBuf,
    max_bytes: u64,
) -> Result<ReadFile, ReadFileError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ReadFileError::NotFound { path });
        }
        Err(source) => {
            return Err(ReadFileError::Io {
                path: path.clone(),
                source: io_error("读取文件元数据", &path, source),
            });
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ReadFileError::Io {
            path: path.clone(),
            source: WindowsFsError::ReparsePoint { path }.into(),
        });
    }
    if !metadata.is_file() {
        return Err(ReadFileError::NotFile { path });
    }
    let pinned = pin_path_without_reparse(&path).map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    let metadata = pinned.metadata().map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    if !metadata.is_file() {
        return Err(ReadFileError::NotFile { path });
    }
    if metadata.len() > max_bytes {
        return Err(ReadFileError::Io {
            path,
            source: SystemFileSystemError::ResourceLimit {
                resource: "完整文件字节数",
                limit: max_bytes,
                observed: metadata.len(),
            },
        });
    }
    let mut file = pinned.file();
    let resolved_path = pinned.resolved_path().to_path_buf();
    let capacity = usize::try_from(metadata.len()).map_err(|_| ReadFileError::Io {
        path: path.clone(),
        source: SystemFileSystemError::AllocationFailed {
            resource: "完整文件读取",
            bytes: metadata.len(),
        },
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ReadFileError::Io {
            path: path.clone(),
            source: SystemFileSystemError::AllocationFailed {
                resource: "完整文件读取",
                bytes: metadata.len(),
            },
        })?;
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: io_error("读取文件", &path, source),
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(ReadFileError::Io {
            path,
            source: SystemFileSystemError::ResourceLimit {
                resource: "完整文件字节数",
                limit: max_bytes,
                observed: bytes.len() as u64,
            },
        });
    }
    Ok(ReadFile::new(resolved_path, bytes))
}

fn bind_scoped_directory_sync(
    root: &Path,
    expected_identity: FileIdentity,
    scope: &ScopedDirectoryScope,
    config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    validate_scoped_directory_sync(root, expected_identity, scope, config)
}

fn validate_scoped_directory_sync(
    root: &Path,
    expected_identity: FileIdentity,
    scope: &ScopedDirectoryScope,
    config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    validate_complete_candidate(root, expected_identity, config)?;
    let pinned_root = pin_directory_without_reparse(root)?;
    if FileIdentity::of(pinned_root.file(), root)? != expected_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: root.to_path_buf(),
        });
    }
    for (index, declared) in scope.roots().iter().enumerate() {
        if scope.roots().iter().skip(index + 1).any(|other| {
            windows_names_equal(
                &declared.encode_wide().collect::<Vec<_>>(),
                &other.encode_wide().collect::<Vec<_>>(),
            )
        }) {
            return Err(SystemFileSystemError::InvalidPath {
                path: root.to_path_buf(),
                reason: "候选编辑范围包含 Windows 非大小写语义下重复的顶层目录",
            });
        }
        let path = pinned_root.resolved_path().join(declared);
        let child = pin_directory_without_reparse(&path)?;
        if !child.metadata()?.is_dir() {
            return Err(SystemFileSystemError::InvalidPath {
                path,
                reason: "候选编辑范围根必须是普通目录",
            });
        }
    }
    Ok(())
}

fn pin_scoped_root(
    root: &Path,
    expected_identity: FileIdentity,
) -> Result<PinnedPath, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let pinned =
        pin_directory_without_reparse(root).map_err(|source| ScopedDirectoryEditError::Failed {
            path: root.to_path_buf(),
            source: Box::new(source.into()),
        })?;
    let actual = FileIdentity::of(pinned.file(), root).map_err(|source| {
        ScopedDirectoryEditError::Failed {
            path: root.to_path_buf(),
            source: Box::new(source.into()),
        }
    })?;
    if actual != expected_identity {
        return Err(ScopedDirectoryEditError::CandidateIdentityChanged {
            root: root.to_path_buf(),
        });
    }
    Ok(pinned)
}

fn scoped_failed(
    path: &ScopedDirectoryPath,
    source: impl Into<SystemFileSystemError>,
) -> ScopedDirectoryEditError<Box<SystemFileSystemError>> {
    ScopedDirectoryEditError::Failed {
        path: path.as_path().to_path_buf(),
        source: Box::new(source.into()),
    }
}

fn read_scoped_file_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    let metadata = match fs::symlink_metadata(absolute) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ScopedDirectoryEditError::NotFound {
                path: relative.as_path().to_path_buf(),
            });
        }
        Err(source) => {
            return Err(scoped_failed(
                relative,
                io_error("读取候选文件元数据", absolute, source),
            ));
        }
    };
    if !metadata.is_file() {
        return Err(ScopedDirectoryEditError::NotFile {
            path: relative.as_path().to_path_buf(),
        });
    }
    let mut pinned =
        pin_path_without_reparse(absolute).map_err(|source| scoped_failed(relative, source))?;
    let metadata = pinned
        .metadata()
        .map_err(|source| scoped_failed(relative, source))?;
    if !metadata.is_file() {
        return Err(ScopedDirectoryEditError::NotFile {
            path: relative.as_path().to_path_buf(),
        });
    }
    if number_of_links(pinned.file(), absolute).map_err(|source| scoped_failed(relative, source))?
        != 1
    {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::InvalidPath {
                path: absolute.to_path_buf(),
                reason: "候选编辑拒绝硬链接文件",
            },
        ));
    }
    if metadata.len() > max_bytes {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::ResourceLimit {
                resource: "候选编辑单文件读取字节数",
                limit: max_bytes,
                observed: metadata.len(),
            },
        ));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| {
        scoped_failed(
            relative,
            SystemFileSystemError::AllocationFailed {
                resource: "候选编辑文件读取",
                bytes: metadata.len(),
            },
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        scoped_failed(
            relative,
            SystemFileSystemError::AllocationFailed {
                resource: "候选编辑文件读取",
                bytes: metadata.len(),
            },
        )
    })?;
    pinned
        .file_mut()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| {
            scoped_failed(relative, io_error("读取候选编辑文件", absolute, source))
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::ResourceLimit {
                resource: "候选编辑单文件读取字节数",
                limit: max_bytes,
                observed: bytes.len() as u64,
            },
        ));
    }
    Ok(bytes)
}

fn list_scoped_directory_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    limit: usize,
) -> Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    let metadata = match fs::symlink_metadata(absolute) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ScopedDirectoryEditError::NotFound {
                path: relative.as_path().to_path_buf(),
            });
        }
        Err(source) => {
            return Err(scoped_failed(
                relative,
                io_error("读取候选目录元数据", absolute, source),
            ));
        }
    };
    if !metadata.is_dir() {
        return Err(ScopedDirectoryEditError::NotDirectory {
            path: relative.as_path().to_path_buf(),
        });
    }
    let pinned = pin_directory_without_reparse(absolute)
        .map_err(|source| scoped_failed(relative, source))?;
    let resolved = pinned.resolved_path();
    let mut entries = Vec::new();
    for entry in fs::read_dir(resolved)
        .map_err(|source| scoped_failed(relative, io_error("列举候选编辑目录", resolved, source)))?
    {
        if entries.len() == limit {
            return Err(scoped_failed(
                relative,
                SystemFileSystemError::ResourceLimit {
                    resource: "候选编辑单目录条目数",
                    limit: limit as u64,
                    observed: limit as u64 + 1,
                },
            ));
        }
        let entry = entry.map_err(|source| {
            scoped_failed(relative, io_error("读取候选编辑目录项", resolved, source))
        })?;
        let name = entry.file_name();
        let child_path = entry.path();
        validate_windows_name(&name, &child_path)
            .map_err(|source| scoped_failed(relative, source))?;
        let child = pin_path_without_reparse(&child_path)
            .map_err(|source| scoped_failed(relative, source))?;
        let metadata = child
            .metadata()
            .map_err(|source| scoped_failed(relative, source))?;
        let kind = if metadata.is_dir() {
            ScopedDirectoryEntryKind::Directory
        } else if metadata.is_file() {
            if number_of_links(child.file(), &child_path)
                .map_err(|source| scoped_failed(relative, source))?
                != 1
            {
                return Err(scoped_failed(
                    relative,
                    SystemFileSystemError::InvalidPath {
                        path: child_path,
                        reason: "候选编辑拒绝硬链接文件",
                    },
                ));
            }
            ScopedDirectoryEntryKind::File
        } else {
            return Err(scoped_failed(
                relative,
                SystemFileSystemError::InvalidPath {
                    path: child_path,
                    reason: "候选编辑目录包含非普通文件系统对象",
                },
            ));
        };
        entries.push(ScopedDirectoryEntry::new(name, kind));
    }
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(entries)
}

fn list_scoped_root_sync(
    root: &Path,
    root_identity: FileIdentity,
    limit: usize,
) -> Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let pinned = pin_scoped_root(root, root_identity)?;
    let resolved = pinned.resolved_path();
    let mut entries = Vec::new();
    for entry in fs::read_dir(resolved).map_err(|source| ScopedDirectoryEditError::Failed {
        path: PathBuf::from("."),
        source: Box::new(io_error("列举候选根", resolved, source)),
    })? {
        if entries.len() == limit {
            return Err(ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(SystemFileSystemError::ResourceLimit {
                    resource: "候选根直接条目数",
                    limit: limit as u64,
                    observed: limit as u64 + 1,
                }),
            });
        }
        let entry = entry.map_err(|source| ScopedDirectoryEditError::Failed {
            path: PathBuf::from("."),
            source: Box::new(io_error("读取候选根目录项", resolved, source)),
        })?;
        let name = entry.file_name();
        let child_path = entry.path();
        validate_windows_name(&name, &child_path).map_err(|source| {
            ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source),
            }
        })?;
        let child = pin_path_without_reparse(&child_path).map_err(|source| {
            ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source.into()),
            }
        })?;
        let metadata = child
            .metadata()
            .map_err(|source| ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source.into()),
            })?;
        let kind = if metadata.is_dir() {
            ScopedDirectoryEntryKind::Directory
        } else if metadata.is_file() {
            if number_of_links(child.file(), &child_path).map_err(|source| {
                ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(source.into()),
                }
            })? != 1
            {
                return Err(ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(SystemFileSystemError::InvalidPath {
                        path: child_path,
                        reason: "候选编辑拒绝硬链接文件",
                    }),
                });
            }
            ScopedDirectoryEntryKind::File
        } else {
            return Err(ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    reason: "候选根包含非普通文件系统对象",
                }),
            });
        };
        entries.push(ScopedDirectoryEntry::new(name, kind));
    }
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(entries)
}

fn create_scoped_directory_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    config: &SystemFileSystemConfig,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let root_pin = pin_scoped_root(root, root_identity)?;
    validate_relative_windows_path(relative.as_path())
        .map_err(|source| scoped_failed(relative, source))?;
    validate_complete_candidate(root, root_identity, config)
        .map_err(|source| scoped_failed(relative, source))?;

    let mut current = root.to_path_buf();
    let mut pins = Vec::new();
    let mut created = Vec::new();
    for component in relative.as_path().components() {
        let Component::Normal(name) = component else {
            unreachable!("ScopedDirectoryPath 已建立普通相对段不变量")
        };
        current.push(name);
        match pin_directory_without_reparse(&current) {
            Ok(pin) => pins.push(pin),
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| {
                    scoped_failed(relative, io_error("建立候选编辑目录", &current, source))
                })?;
                let pin = pin_directory_without_reparse(&current)
                    .map_err(|source| scoped_failed(relative, source))?;
                let identity = FileIdentity::of(pin.file(), &current)
                    .map_err(|source| scoped_failed(relative, source))?;
                created.push((current.clone(), identity));
                pins.push(pin);
            }
            Err(source) => return Err(scoped_failed(relative, source)),
        }
    }

    let validation = validate_complete_candidate(root, root_identity, config);
    drop(pins);
    drop(root_pin);
    if let Err(operation) = validation {
        return rollback_created_directories(relative, created, operation);
    }
    debug_assert_eq!(current, absolute);
    Ok(())
}

fn rollback_created_directories(
    relative: &ScopedDirectoryPath,
    mut created: Vec<(PathBuf, FileIdentity)>,
    operation: SystemFileSystemError,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    while let Some((path, identity)) = created.pop() {
        if let Err(rollback) = delete_empty_directory_if_identity(&path, identity) {
            return Err(scoped_failed(
                relative,
                SystemFileSystemError::ScopedEditRollbackFailed {
                    path,
                    operation: Box::new(operation),
                    rollback: Box::new(rollback.into()),
                },
            ));
        }
    }
    Err(scoped_failed(relative, operation))
}

fn write_scoped_file_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
    config: &SystemFileSystemConfig,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    validate_relative_windows_path(relative.as_path())
        .map_err(|source| scoped_failed(relative, source))?;
    validate_complete_candidate(root, root_identity, config)
        .map_err(|source| scoped_failed(relative, source))?;
    if bytes.len() as u64 > config.tree.max_single_file_bytes {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::ResourceLimit {
                resource: "候选编辑单文件字节数",
                limit: config.tree.max_single_file_bytes,
                observed: bytes.len() as u64,
            },
        ));
    }
    let parent = absolute.parent().expect("受检候选文件路径必须包含父目录");
    let _parent =
        pin_directory_without_reparse(parent).map_err(|source| scoped_failed(relative, source))?;

    match fs::symlink_metadata(absolute) {
        Ok(metadata) if metadata.is_file() => {
            write_existing_scoped_file(root, root_identity, relative, absolute, bytes, config)
        }
        Ok(_) => Err(ScopedDirectoryEditError::NotFile {
            path: relative.as_path().to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            write_new_scoped_file(root, root_identity, relative, absolute, bytes, config)
        }
        Err(source) => Err(scoped_failed(
            relative,
            io_error("读取候选写入目标元数据", absolute, source),
        )),
    }
}

fn write_existing_scoped_file(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
    config: &SystemFileSystemConfig,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let mut pinned = open_read_write_file_without_reparse(absolute, false)
        .map_err(|source| scoped_failed(relative, source))?;
    if number_of_links(pinned.file(), absolute).map_err(|source| scoped_failed(relative, source))?
        != 1
    {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::InvalidPath {
                path: absolute.to_path_buf(),
                reason: "候选编辑拒绝硬链接文件",
            },
        ));
    }
    let old_len = pinned
        .metadata()
        .map_err(|source| scoped_failed(relative, source))?
        .len();
    let capacity = usize::try_from(old_len).map_err(|_| {
        scoped_failed(
            relative,
            SystemFileSystemError::AllocationFailed {
                resource: "候选编辑回滚副本",
                bytes: old_len,
            },
        )
    })?;
    let mut original = Vec::new();
    original.try_reserve_exact(capacity).map_err(|_| {
        scoped_failed(
            relative,
            SystemFileSystemError::AllocationFailed {
                resource: "候选编辑回滚副本",
                bytes: old_len,
            },
        )
    })?;
    pinned
        .file_mut()
        .seek(SeekFrom::Start(0))
        .and_then(|_| pinned.file_mut().read_to_end(&mut original))
        .map_err(|source| {
            scoped_failed(relative, io_error("读取候选文件回滚副本", absolute, source))
        })?;

    if let Err(operation) = replace_pinned_file_contents(&mut pinned, absolute, &bytes) {
        return restore_scoped_file_after_failure(relative, absolute, pinned, original, operation);
    }
    if let Err(operation) = validate_complete_candidate(root, root_identity, config) {
        return restore_scoped_file_after_failure(relative, absolute, pinned, original, operation);
    }
    Ok(())
}

fn write_new_scoped_file(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
    config: &SystemFileSystemConfig,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(absolute)
        .map_err(|source| {
            scoped_failed(relative, io_error("建立候选编辑文件", absolute, source))
        })?;
    let identity =
        FileIdentity::of(&file, absolute).map_err(|source| scoped_failed(relative, source))?;
    let operation = file
        .write_all(&bytes)
        .and_then(|()| file.sync_data())
        .map_err(|source| io_error("写入候选编辑文件", absolute, source))
        .and_then(|()| validate_complete_candidate(root, root_identity, config));
    drop(file);
    if let Err(operation) = operation {
        return match delete_regular_file_if_identity(absolute, identity) {
            Ok(()) => Err(scoped_failed(relative, operation)),
            Err(rollback) => Err(scoped_failed(
                relative,
                SystemFileSystemError::ScopedEditRollbackFailed {
                    path: absolute.to_path_buf(),
                    operation: Box::new(operation),
                    rollback: Box::new(rollback.into()),
                },
            )),
        };
    }
    Ok(())
}

fn replace_pinned_file_contents(
    pinned: &mut PinnedPath,
    absolute: &Path,
    bytes: &[u8],
) -> Result<(), SystemFileSystemError> {
    pinned
        .file_mut()
        .set_len(0)
        .and_then(|()| pinned.file_mut().seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| pinned.file_mut().write_all(bytes))
        .and_then(|()| pinned.file_mut().sync_data())
        .map_err(|source| io_error("替换候选编辑文件内容", absolute, source))
}

fn restore_scoped_file_after_failure(
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    mut pinned: PinnedPath,
    original: Vec<u8>,
    operation: SystemFileSystemError,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    match replace_pinned_file_contents(&mut pinned, absolute, &original) {
        Ok(()) => Err(scoped_failed(relative, operation)),
        Err(rollback) => Err(scoped_failed(
            relative,
            SystemFileSystemError::ScopedEditRollbackFailed {
                path: absolute.to_path_buf(),
                operation: Box::new(operation),
                rollback: Box::new(rollback),
            },
        )),
    }
}

fn remove_scoped_path_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    let pinned = match pin_path_without_reparse(absolute) {
        Ok(pinned) => pinned,
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ScopedDirectoryEditError::NotFound {
                path: relative.as_path().to_path_buf(),
            });
        }
        Err(source) => return Err(scoped_failed(relative, source)),
    };
    let metadata = pinned
        .metadata()
        .map_err(|source| scoped_failed(relative, source))?;
    let identity = FileIdentity::of(pinned.file(), absolute)
        .map_err(|source| scoped_failed(relative, source))?;
    if metadata.is_file()
        && number_of_links(pinned.file(), absolute)
            .map_err(|source| scoped_failed(relative, source))?
            != 1
    {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::InvalidPath {
                path: absolute.to_path_buf(),
                reason: "候选编辑拒绝硬链接文件",
            },
        ));
    }
    drop(pinned);
    if metadata.is_file() {
        delete_regular_file_if_identity(absolute, identity)
            .map_err(|source| scoped_failed(relative, source))
    } else if metadata.is_dir() {
        delete_empty_directory_if_identity(absolute, identity).map_err(|source| {
            if matches!(
                &source,
                WindowsFsError::Io { source, .. }
                    if source.kind() == io::ErrorKind::DirectoryNotEmpty
            ) {
                ScopedDirectoryEditError::DirectoryNotEmpty {
                    path: relative.as_path().to_path_buf(),
                }
            } else {
                scoped_failed(relative, source)
            }
        })
    } else {
        Err(scoped_failed(
            relative,
            SystemFileSystemError::InvalidPath {
                path: absolute.to_path_buf(),
                reason: "候选编辑拒绝非普通文件系统对象",
            },
        ))
    }
}

fn acquire_exclusive_file_lease_sync(
    request: ExclusiveFileLeaseRequest,
    timeout: Duration,
) -> Result<
    ExclusiveFileLease<ExclusiveFileLock>,
    ExclusiveFileLeaseError<Box<SystemFileSystemError>>,
> {
    let identity = request.identity().to_os_string();
    let result: Result<_, SystemFileSystemError> = (|| {
        let lock_directory = trusted_lock_directory(request.lock_directory())?;
        let lock_path = stable_lock_path(&lock_directory, request.identity())?;
        ExclusiveFileLock::acquire(&lock_path, timeout)
            .map(ExclusiveFileLease::new)
            .map_err(Into::into)
    })();
    match result {
        Ok(lease) => Ok(lease),
        Err(SystemFileSystemError::Windows(WindowsFsError::LockTimeout { .. })) => {
            Err(ExclusiveFileLeaseError::Busy { identity, timeout })
        }
        Err(source) => Err(ExclusiveFileLeaseError::Unavailable {
            identity,
            source: Box::new(source),
        }),
    }
}

#[derive(Clone)]
struct FingerprintRoot {
    physical_root: PathBuf,
    logical_root: PathBuf,
    logical_key: Vec<u16>,
}

#[derive(Eq, PartialEq)]
struct FingerprintIdentityObservation {
    entry_type: u8,
    logical_key: Vec<u16>,
    physical_path: PathBuf,
    identity: FileIdentity,
}

struct FingerprintObservation {
    fingerprint: Sha256Fingerprint,
    identities: Vec<FingerprintIdentityObservation>,
}

struct FingerprintPass<'a> {
    budget: TreeBudgetUsage,
    file_identities: HashSet<FileIdentity>,
    identities: Vec<FingerprintIdentityObservation>,
    system_config: &'a SystemFileSystemConfig,
    hasher: Sha256,
}

#[derive(Clone, Copy)]
struct ExpectedFingerprintFile {
    identity: FileIdentity,
    size: u64,
}

fn fingerprint_directory_tree_sync(
    request: DirectoryTreeFingerprintRequest,
    system_config: &SystemFileSystemConfig,
) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    fingerprint_directory_tree_sync_with_between(request, system_config, || {})
}

fn fingerprint_directory_tree_sync_with_between<F>(
    request: DirectoryTreeFingerprintRequest,
    system_config: &SystemFileSystemConfig,
    between_observations: F,
) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>>
where
    F: FnOnce(),
{
    let mut roots = Vec::with_capacity(request.roots().len());
    for root in request.roots() {
        let physical_root = absolutize(root.physical_root().to_path_buf()).map_err(|source| {
            DirectoryTreeFingerprintError::Failed {
                path: root.physical_root().to_path_buf(),
                source: Box::new(source),
            }
        })?;
        validate_relative_windows_path(root.logical_root()).map_err(|source| {
            DirectoryTreeFingerprintError::Failed {
                path: physical_root.clone(),
                source: Box::new(source),
            }
        })?;
        roots.push(FingerprintRoot {
            physical_root,
            logical_root: root.logical_root().to_path_buf(),
            logical_key: path_utf16_key(root.logical_root()),
        });
    }
    roots.sort_by(|first, second| first.logical_key.cmp(&second.logical_key));
    let first_path = roots
        .first()
        .expect("受检目录树指纹请求至少包含一个根")
        .physical_root
        .clone();
    let first = fingerprint_directory_tree_once(&roots, system_config)?;
    between_observations();
    let second = fingerprint_directory_tree_once(&roots, system_config)?;
    if first.fingerprint != second.fingerprint || first.identities != second.identities {
        let path = first
            .identities
            .iter()
            .zip(&second.identities)
            .find_map(|(first, second)| (first != second).then(|| first.physical_path.clone()))
            .or_else(|| {
                first
                    .identities
                    .get(second.identities.len())
                    .map(|entry| entry.physical_path.clone())
            })
            .or_else(|| {
                second
                    .identities
                    .get(first.identities.len())
                    .map(|entry| entry.physical_path.clone())
            })
            .unwrap_or(first_path);
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation { path });
    }
    Ok(second.fingerprint)
}

fn fingerprint_directory_tree_once(
    roots: &[FingerprintRoot],
    system_config: &SystemFileSystemConfig,
) -> Result<FingerprintObservation, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    let mut pass = FingerprintPass {
        budget: TreeBudgetUsage::default(),
        file_identities: HashSet::new(),
        identities: Vec::new(),
        system_config,
        hasher: Sha256::new(),
    };
    hash_frame(&mut pass.hasher, 0, DIRECTORY_TREE_FINGERPRINT_DOMAIN);
    for root in roots {
        fingerprint_directory(&root.physical_root, &root.logical_root, None, 1, &mut pass)?;
    }
    Ok(FingerprintObservation {
        fingerprint: Sha256Fingerprint::from_bytes(pass.hasher.finalize().into()),
        identities: pass.identities,
    })
}

fn fingerprint_directory(
    physical_path: &Path,
    logical_path: &Path,
    expected_identity: Option<FileIdentity>,
    depth: usize,
    pass: &mut FingerprintPass<'_>,
) -> Result<(), DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    if depth > pass.system_config.tree.max_depth {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::ResourceLimit {
                resource: "目录树深度",
                limit: pass.system_config.tree.max_depth as u64,
                observed: depth as u64,
            },
        ));
    }
    let metadata = match fs::symlink_metadata(physical_path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(DirectoryTreeFingerprintError::NotFound {
                path: physical_path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(fingerprint_failed(
                physical_path,
                io_error("读取目录树根元数据", physical_path, source),
            ));
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(fingerprint_failed(
            physical_path,
            WindowsFsError::ReparsePoint {
                path: physical_path.to_path_buf(),
            }
            .into(),
        ));
    }
    if !metadata.is_dir() {
        return Err(DirectoryTreeFingerprintError::NotDirectory {
            path: physical_path.to_path_buf(),
        });
    }
    let pinned = pin_directory_without_reparse(physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    pass.budget
        .add_entry(&pass.system_config.tree)
        .map_err(|source| fingerprint_failed(physical_path, source))?;
    hash_tree_path(&mut pass.hasher, 1, logical_path);
    pass.identities.push(FingerprintIdentityObservation {
        entry_type: 1,
        logical_key: path_utf16_key(logical_path),
        physical_path: physical_path.to_path_buf(),
        identity,
    });

    let resolved = pinned.resolved_path();
    let mut entries = Vec::new();
    for entry in fs::read_dir(resolved)
        .map_err(|source| fingerprint_failed(resolved, io_error("列举目录树", resolved, source)))?
    {
        if entries.len() == pass.system_config.max_directory_entries {
            return Err(fingerprint_failed(
                resolved,
                SystemFileSystemError::ResourceLimit {
                    resource: "目录树单目录条目数",
                    limit: pass.system_config.max_directory_entries as u64,
                    observed: pass.system_config.max_directory_entries as u64 + 1,
                },
            ));
        }
        let entry = entry.map_err(|source| {
            fingerprint_failed(resolved, io_error("读取目录树目录项", resolved, source))
        })?;
        let name = entry.file_name();
        let path = entry.path();
        validate_windows_name(&name, &path).map_err(|source| fingerprint_failed(&path, source))?;
        let wide = name.encode_wide().collect::<Vec<_>>();
        if entries
            .iter()
            .any(|existing: &FingerprintEntry| windows_names_equal(&existing.wide_name, &wide))
        {
            return Err(fingerprint_failed(
                &path,
                SystemFileSystemError::InvalidPath {
                    path: path.clone(),
                    reason: "目录树包含 Windows 大小写等价名称",
                },
            ));
        }
        let pinned_entry = pin_path_without_reparse(&path)
            .map_err(|source| fingerprint_failed(&path, source.into()))?;
        let metadata = pinned_entry
            .metadata()
            .map_err(|source| fingerprint_failed(&path, source.into()))?;
        let identity = FileIdentity::of(pinned_entry.file(), &path)
            .map_err(|source| fingerprint_failed(&path, source.into()))?;
        let kind = if metadata.is_dir() {
            FingerprintEntryKind::Directory
        } else if metadata.is_file() {
            FingerprintEntryKind::File {
                size: metadata.len(),
            }
        } else {
            return Err(fingerprint_failed(
                &path,
                SystemFileSystemError::InvalidPath {
                    path: path.clone(),
                    reason: "目录树包含非普通文件系统对象",
                },
            ));
        };
        entries.push(FingerprintEntry {
            name,
            wide_name: wide,
            physical_path: entry.path(),
            identity,
            kind,
        });
    }
    entries.sort_by(|first, second| first.wide_name.cmp(&second.wide_name));
    for entry in entries {
        let logical_child = logical_path.join(&entry.name);
        match entry.kind {
            FingerprintEntryKind::Directory => fingerprint_directory(
                &entry.physical_path,
                &logical_child,
                Some(entry.identity),
                depth + 1,
                pass,
            )?,
            FingerprintEntryKind::File { size } => fingerprint_file(
                &entry.physical_path,
                &logical_child,
                ExpectedFingerprintFile {
                    identity: entry.identity,
                    size,
                },
                depth + 1,
                pass,
            )?,
        }
    }
    let after = pin_directory_without_reparse(physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let after_identity = FileIdentity::of(after.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let held_identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if after_identity != identity || held_identity != identity {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    Ok(())
}

struct FingerprintEntry {
    name: OsString,
    wide_name: Vec<u16>,
    physical_path: PathBuf,
    identity: FileIdentity,
    kind: FingerprintEntryKind,
}

enum FingerprintEntryKind {
    Directory,
    File { size: u64 },
}

fn fingerprint_file(
    physical_path: &Path,
    logical_path: &Path,
    expected: ExpectedFingerprintFile,
    depth: usize,
    pass: &mut FingerprintPass<'_>,
) -> Result<(), DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    if depth > pass.system_config.tree.max_depth {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::ResourceLimit {
                resource: "目录树深度",
                limit: pass.system_config.tree.max_depth as u64,
                observed: depth as u64,
            },
        ));
    }
    let mut pinned = pin_regular_file_for_snapshot_read(physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let before = pinned
        .metadata()
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if !before.is_file() {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                reason: "目录树项不再是普通文件",
            },
        ));
    }
    if number_of_links(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?
        != 1
    {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                reason: "目录树包含硬链接文件",
            },
        ));
    }
    if before.len() > pass.system_config.tree.max_single_file_bytes {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::ResourceLimit {
                resource: "目录树单文件字节数",
                limit: pass.system_config.tree.max_single_file_bytes,
                observed: before.len(),
            },
        ));
    }
    let identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if identity != expected.identity || before.len() != expected.size {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    if !pass.file_identities.insert(identity) {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                reason: "目录树包含共享同一物理文件身份的硬链接",
            },
        ));
    }
    pass.budget
        .add_entry(&pass.system_config.tree)
        .map_err(|source| fingerprint_failed(physical_path, source))?;
    pass.budget
        .add_bytes(before.len(), &pass.system_config.tree)
        .map_err(|source| fingerprint_failed(physical_path, source))?;
    hash_tree_path(&mut pass.hasher, 2, logical_path);
    hash_frame(&mut pass.hasher, 4, &before.len().to_be_bytes());
    hash_frame_prefix(&mut pass.hasher, 5, before.len());
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = pinned.file_mut().read(&mut buffer).map_err(|source| {
            fingerprint_failed(
                physical_path,
                io_error("读取目录树文件", physical_path, source),
            )
        })?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        pass.hasher.update(&buffer[..read]);
    }
    let after = pinned
        .metadata()
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let after_identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let after_links = number_of_links(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if observed != before.len()
        || after.len() != before.len()
        || after_identity != identity
        || after_links != 1
    {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    pass.identities.push(FingerprintIdentityObservation {
        entry_type: 2,
        logical_key: path_utf16_key(logical_path),
        physical_path: physical_path.to_path_buf(),
        identity,
    });
    Ok(())
}

fn fingerprint_failed(
    path: &Path,
    source: SystemFileSystemError,
) -> DirectoryTreeFingerprintError<Box<SystemFileSystemError>> {
    DirectoryTreeFingerprintError::Failed {
        path: path.to_path_buf(),
        source: Box::new(source),
    }
}

fn path_utf16_key(path: &Path) -> Vec<u16> {
    let mut key = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        key.push(0);
        key.extend(name.encode_wide());
    }
    key
}

fn hash_tree_path(hasher: &mut Sha256, entry_type: u8, path: &Path) {
    hash_frame(hasher, 1, &[entry_type]);
    let components = path.components().count() as u64;
    hash_frame(hasher, 2, &components.to_be_bytes());
    for component in path.components() {
        let Component::Normal(name) = component else {
            unreachable!("受检逻辑路径只能包含普通相对段")
        };
        let mut bytes = Vec::new();
        for unit in name.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        hash_frame(hasher, 3, &bytes);
    }
}

fn hash_frame(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hash_frame_prefix(hasher, tag, bytes.len() as u64);
    hasher.update(bytes);
}

fn hash_frame_prefix(hasher: &mut Sha256, tag: u8, length: u64) {
    hasher.update([tag]);
    hasher.update(length.to_be_bytes());
}

#[derive(Default)]
struct TreeBudgetUsage {
    entries: usize,
    bytes: u64,
}

impl TreeBudgetUsage {
    fn add_entry(&mut self, config: &TreeBudget) -> Result<(), SystemFileSystemError> {
        self.entries += 1;
        if self.entries > config.max_entries {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录树条目数",
                limit: config.max_entries as u64,
                observed: self.entries as u64,
            });
        }
        Ok(())
    }

    fn add_bytes(&mut self, bytes: u64, config: &TreeBudget) -> Result<(), SystemFileSystemError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(SystemFileSystemError::ResourceLimit {
                resource: "目录树总字节数",
                limit: config.max_bytes,
                observed: u64::MAX,
            })?;
        if self.bytes > config.max_bytes {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录树总字节数",
                limit: config.max_bytes,
                observed: self.bytes,
            });
        }
        Ok(())
    }

    fn replace_bytes(
        &mut self,
        old: u64,
        new: u64,
        config: &TreeBudget,
    ) -> Result<(), SystemFileSystemError> {
        self.bytes = self.bytes.saturating_sub(old);
        self.add_bytes(new, config)
    }
}

fn prepare_directory_sync(
    request: DirectoryStageRequest,
    system_config: SystemFileSystemConfig,
    publisher_config: DirectoryPublisherConfig,
    publisher_identity: Arc<()>,
) -> Result<StagedDirectory<SystemStagingState>, DirectoryPrepareError<Box<SystemFileSystemError>>>
{
    let target_root = request.target_root().to_path_buf();
    let result: Result<_, PrepareSyncFailure> = (|| {
        if !target_root.is_absolute() {
            return Err(SystemFileSystemError::InvalidPath {
                path: target_root.clone(),
                reason: "发布目标必须是绝对路径",
            }
            .into());
        }
        let parent = target_root
            .parent()
            .ok_or_else(|| SystemFileSystemError::InvalidPath {
                path: target_root.clone(),
                reason: "发布目标必须拥有父目录",
            })?;
        let parent_root = validate_local_case_insensitive_ntfs_directory(parent)?;
        let parent_handle = open_directory(&parent_root, false)?;
        let parent_identity = FileIdentity::of(&parent_handle, &parent_root)?;
        let target_name =
            target_root
                .file_name()
                .ok_or_else(|| SystemFileSystemError::InvalidPath {
                    path: target_root.clone(),
                    reason: "发布目标必须拥有目录名",
                })?;
        validate_windows_name(target_name, &target_root)?;
        let target_root = parent_root.join(target_name);
        let lock_path = target_lock_path(&publisher_config.lock_directory, &target_root)?;
        let target_lock =
            ExclusiveFileLock::acquire(&lock_path, publisher_config.target_lock_timeout)?;
        let target_artifact_key = target_lock.identity(&lock_path)?.stable_hex();
        recover_target(&target_root, &target_artifact_key, &publisher_config)?;

        let operation_id = secure_uuid_v4("生成目录发布操作 ID")?;
        let stem = format!("{RESERVED_PREFIX}{target_artifact_key}-{operation_id}");
        let stage_root = parent_root.join(format!("{stem}.stage"));
        let backup_path = parent_root.join(format!("{stem}.backup"));
        let journal_path = parent_root.join(format!("{stem}.journal"));
        fs::create_dir(&stage_root)
            .map_err(|source| io_error("建立目录候选", &stage_root, source))?;
        let stage_handle = open_directory(&stage_root, true).map_err(|source| {
            PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: Box::new(source.into()),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    stage_root.clone(),
                    Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root.clone(),
                    }),
                )),
            })
        })?;
        let stage_identity = FileIdentity::of(&stage_handle, &stage_root).map_err(|source| {
            PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: Box::new(source.into()),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    stage_root.clone(),
                    Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root.clone(),
                    }),
                )),
            })
        })?;
        let mut cleanup = StageCleanupGuard::new(stage_root.clone(), stage_identity);
        let build_result = build_candidate(
            &stage_root,
            &target_root,
            request.source_mappings(),
            request.overlays(),
            request.empty_directories(),
            &system_config,
        );
        if let Err(source) = build_result {
            return Err(prepare_terminal_failure(
                target_root,
                &stage_root,
                &mut cleanup,
                source,
            ));
        }
        Ok(StagedDirectory::new(
            target_root,
            stage_root,
            request.publish_intent(),
            SystemStagingState {
                publisher_identity,
                operation_id,
                parent_root,
                parent_identity,
                stage_identity,
                target_lock: Some(target_lock),
                parent_handle: Some(parent_handle),
                stage_handle: Some(stage_handle),
                journal_path,
                backup_path,
                cleanup,
                abandoned_before_delivery: false,
                finalized: false,
            },
        ))
    })();

    match result {
        Ok(staged) => Ok(staged),
        Err(PrepareSyncFailure::Root(source)) => Err(DirectoryPrepareError::NotPrepared {
            target_root,
            source: Box::new(source),
            cleanup_failure: None,
        }),
        Err(PrepareSyncFailure::Terminal(error)) => Err(error),
    }
}

enum PrepareSyncFailure {
    Root(SystemFileSystemError),
    Terminal(DirectoryPrepareError<Box<SystemFileSystemError>>),
}

impl From<SystemFileSystemError> for PrepareSyncFailure {
    fn from(source: SystemFileSystemError) -> Self {
        Self::Root(source)
    }
}

impl From<WindowsFsError> for PrepareSyncFailure {
    fn from(source: WindowsFsError) -> Self {
        Self::Root(source.into())
    }
}

fn prepare_terminal_failure(
    target_root: PathBuf,
    stage_root: &Path,
    cleanup: &mut StageCleanupGuard,
    source: SystemFileSystemError,
) -> PrepareSyncFailure {
    let cleanup_failure = cleanup.cleanup().err().map(|cleanup_source| {
        StagingCleanupFailure::new(stage_root.to_path_buf(), Box::new(cleanup_source))
    });
    PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
        target_root,
        source: Box::new(source),
        cleanup_failure,
    })
}

fn build_candidate(
    stage_root: &Path,
    target_root: &Path,
    source_mappings: &[DirectorySourceMapping],
    overlays: &[DirectoryFileOverlay],
    empty_directories: &[PathBuf],
    system_config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    validate_declared_windows_paths(
        source_mappings,
        overlays,
        empty_directories,
        &system_config.tree,
    )?;
    let mut budget = TreeBudgetUsage::default();
    for mapping in source_mappings {
        validate_relative_windows_path(mapping.relative_target())?;
        ensure_source_is_physically_disjoint(mapping.source_directory(), stage_root, target_root)?;
        if let Some(relative_parent) = mapping.relative_target().parent() {
            ensure_empty_directory(
                stage_root,
                relative_parent,
                &mut budget,
                &system_config.tree,
            )?;
        }
        let destination = stage_root.join(mapping.relative_target());
        copy_directory_tree(
            mapping.source_directory(),
            &destination,
            None,
            mapping.relative_target().components().count(),
            &mut budget,
            system_config,
        )?;
    }
    for overlay in overlays {
        validate_relative_windows_path(overlay.relative_file())?;
        let depth = overlay.relative_file().components().count();
        if depth > system_config.tree.max_depth {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录候选深度",
                limit: system_config.tree.max_depth as u64,
                observed: depth as u64,
            });
        }
        if overlay.bytes().len() as u64 > system_config.tree.max_single_file_bytes {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录候选单文件字节数",
                limit: system_config.tree.max_single_file_bytes,
                observed: overlay.bytes().len() as u64,
            });
        }
        let destination = stage_root.join(overlay.relative_file());
        let mut pinned = open_read_write_file_without_reparse(&destination, false)?;
        let metadata = pinned.metadata()?;
        budget.replace_bytes(
            metadata.len(),
            overlay.bytes().len() as u64,
            &system_config.tree,
        )?;
        pinned
            .file_mut()
            .set_len(0)
            .map_err(|source| io_error("截断覆盖目标", &destination, source))?;
        pinned
            .file_mut()
            .write_all(overlay.bytes())
            .map_err(|source| io_error("写入候选覆盖", &destination, source))?;
        pinned
            .file()
            .sync_data()
            .map_err(|source| io_error("同步候选覆盖", &destination, source))?;
    }
    for directory in empty_directories {
        validate_relative_windows_path(directory)?;
        let depth = directory.components().count();
        if depth > system_config.tree.max_depth {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录候选深度",
                limit: system_config.tree.max_depth as u64,
                observed: depth as u64,
            });
        }
        ensure_empty_directory(stage_root, directory, &mut budget, &system_config.tree)?;
    }
    Ok(())
}

fn validate_declared_windows_paths(
    source_mappings: &[DirectorySourceMapping],
    overlays: &[DirectoryFileOverlay],
    empty_directories: &[PathBuf],
    config: &TreeBudget,
) -> Result<(), SystemFileSystemError> {
    let declared_count = source_mappings
        .len()
        .saturating_add(overlays.len())
        .saturating_add(empty_directories.len());
    if declared_count > config.max_entries {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录候选声明路径数",
            limit: config.max_entries as u64,
            observed: declared_count as u64,
        });
    }

    let declared_paths = source_mappings
        .iter()
        .map(DirectorySourceMapping::relative_target)
        .chain(overlays.iter().map(DirectoryFileOverlay::relative_file))
        .chain(empty_directories.iter().map(PathBuf::as_path));
    let mut seen_prefixes: Vec<Vec<OsString>> = Vec::new();
    for path in declared_paths {
        validate_relative_windows_path(path)?;
        let depth = path.components().count();
        if depth > config.max_depth {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录候选深度",
                limit: config.max_depth as u64,
                observed: depth as u64,
            });
        }
        let mut prefix = Vec::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                unreachable!("候选声明路径已经过结构校验");
            };
            prefix.push(name.to_os_string());
            if let Some(existing) = seen_prefixes.iter().find(|existing| {
                existing.len() == prefix.len()
                    && existing.iter().zip(&prefix).all(|(first, second)| {
                        windows_names_equal(
                            &first.encode_wide().collect::<Vec<_>>(),
                            &second.encode_wide().collect::<Vec<_>>(),
                        )
                    })
            }) {
                if existing != &prefix {
                    return Err(SystemFileSystemError::InvalidPath {
                        path: path.to_path_buf(),
                        reason: "候选声明对同一 Windows 路径使用了冲突的大小写拼写",
                    });
                }
            } else {
                seen_prefixes.push(prefix.clone());
            }
        }
    }
    Ok(())
}

/// 发布前重新观测完整候选，把 `prepare` 返回后由受信非根服务新增的文件
/// 调用方在候选内新增的普通文件也纳入同一组资源、名称和 reparse 不变量。
fn validate_complete_candidate(
    stage_root: &Path,
    expected_identity: FileIdentity,
    system_config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    let pinned_root = pin_directory_without_reparse(stage_root)?;
    if FileIdentity::of(pinned_root.file(), stage_root)? != expected_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    let mut budget = TreeBudgetUsage::default();
    let mut file_identities = HashSet::new();
    validate_candidate_directory(
        stage_root,
        0,
        false,
        &mut budget,
        &mut file_identities,
        system_config,
    )
}

fn validate_candidate_directory(
    directory: &Path,
    depth: usize,
    count_directory: bool,
    budget: &mut TreeBudgetUsage,
    file_identities: &mut HashSet<FileIdentity>,
    system_config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    if depth > system_config.tree.max_depth {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录候选深度",
            limit: system_config.tree.max_depth as u64,
            observed: depth as u64,
        });
    }
    let pinned_directory = pin_directory_without_reparse(directory)?;
    if count_directory {
        budget.add_entry(&system_config.tree)?;
    }
    let resolved = pinned_directory.resolved_path().to_path_buf();
    let mut names: Vec<Vec<u16>> = Vec::new();
    let mut direct_entries = 0_usize;
    for entry in fs::read_dir(&resolved)
        .map_err(|source| io_error("发布前枚举完整候选", &resolved, source))?
    {
        direct_entries += 1;
        if direct_entries > system_config.max_directory_entries {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "候选单目录条目数",
                limit: system_config.max_directory_entries as u64,
                observed: direct_entries as u64,
            });
        }
        let entry = entry.map_err(|source| io_error("发布前读取候选目录项", &resolved, source))?;
        let name = entry.file_name();
        let child_path = entry.path();
        validate_windows_name(&name, &child_path)?;
        let wide = name.encode_wide().collect::<Vec<_>>();
        if names
            .iter()
            .any(|existing| windows_names_equal(existing, &wide))
        {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_path,
                reason: "发布候选的同一目录包含 Windows 大小写等价名称",
            });
        }
        names.push(wide);

        let child = pin_path_without_reparse(&child_path)?;
        let metadata = child.metadata()?;
        let child_depth = depth + 1;
        if metadata.is_dir() {
            validate_candidate_directory(
                &child_path,
                child_depth,
                true,
                budget,
                file_identities,
                system_config,
            )?;
        } else if metadata.is_file() {
            if child_depth > system_config.tree.max_depth {
                return Err(SystemFileSystemError::ResourceLimit {
                    resource: "目录候选深度",
                    limit: system_config.tree.max_depth as u64,
                    observed: child_depth as u64,
                });
            }
            if metadata.len() > system_config.tree.max_single_file_bytes {
                return Err(SystemFileSystemError::ResourceLimit {
                    resource: "目录候选单文件字节数",
                    limit: system_config.tree.max_single_file_bytes,
                    observed: metadata.len(),
                });
            }
            if number_of_links(child.file(), &child_path)? != 1 {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    reason: "发布候选包含硬链接文件",
                });
            }
            let identity = FileIdentity::of(child.file(), &child_path)?;
            if !file_identities.insert(identity) {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    reason: "发布候选包含共享同一物理文件身份的硬链接",
                });
            }
            budget.add_entry(&system_config.tree)?;
            budget.add_bytes(metadata.len(), &system_config.tree)?;
        } else {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_path,
                reason: "发布候选包含非普通文件系统对象",
            });
        }
    }
    Ok(())
}

fn ensure_source_is_physically_disjoint(
    source: &Path,
    stage_root: &Path,
    target_root: &Path,
) -> Result<(), SystemFileSystemError> {
    let source_path = pin_directory_without_reparse(source)?;
    let source_root = source_path.resolved_path().to_path_buf();
    let source_identity = FileIdentity::of(source_path.file(), &source_root)?;
    let stage_ancestors = directory_ancestor_identities(stage_root)?;
    if stage_ancestors.contains(&source_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source_root,
            reason: "复制来源在物理文件树中包含候选目录",
        });
    }
    let source_ancestors = directory_ancestor_identities(&source_root)?;
    let stage_identity =
        identity_at(stage_root)?.expect("候选目录在复制开始前已经建立并持有文件身份");
    if source_ancestors.contains(&stage_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source_root,
            reason: "复制来源位于候选目录内部",
        });
    }
    let target_identity = match fs::symlink_metadata(target_root) {
        Ok(metadata)
            if metadata.is_dir()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 =>
        {
            identity_at(target_root)?
        }
        Ok(_) => None,
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(io_error("读取发布目标物理身份", target_root, source));
        }
    };
    if let Some(target_identity) = target_identity {
        if source_ancestors.contains(&target_identity) {
            return Err(SystemFileSystemError::InvalidPath {
                path: source_root,
                reason: "复制来源位于现存发布目标内部",
            });
        }
        let target_ancestors = directory_ancestor_identities(target_root)?;
        if target_ancestors.contains(&source_identity) {
            return Err(SystemFileSystemError::InvalidPath {
                path: source_root,
                reason: "复制来源在物理文件树中包含现存发布目标",
            });
        }
    }
    Ok(())
}

fn directory_ancestor_identities(path: &Path) -> Result<Vec<FileIdentity>, SystemFileSystemError> {
    pin_directory_without_reparse(path)?
        .component_identities()
        .map_err(Into::into)
}

fn copy_directory_tree(
    source: &Path,
    destination: &Path,
    expected_identity: Option<FileIdentity>,
    depth: usize,
    budget: &mut TreeBudgetUsage,
    system_config: &SystemFileSystemConfig,
) -> Result<(), SystemFileSystemError> {
    if depth > system_config.tree.max_depth {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录候选深度",
            limit: system_config.tree.max_depth as u64,
            observed: depth as u64,
        });
    }
    let source_path = pin_directory_without_reparse(source)?;
    let source_resolved = source_path.resolved_path().to_path_buf();
    let source_identity = FileIdentity::of(source_path.file(), source)?;
    if expected_identity.is_some_and(|expected| expected != source_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源目录在枚举后物理身份发生变化",
        });
    }
    fs::create_dir(destination)
        .map_err(|source_error| io_error("建立候选目录", destination, source_error))?;
    let destination_path = pin_directory_without_reparse(destination)?;
    let destination_resolved = destination_path.resolved_path().to_path_buf();
    budget.add_entry(&system_config.tree)?;

    let mut names: Vec<Vec<u16>> = Vec::new();
    let mut direct_entries = 0_usize;
    for entry in fs::read_dir(&source_resolved)
        .map_err(|source_error| io_error("列举复制来源", &source_resolved, source_error))?
    {
        direct_entries += 1;
        if direct_entries > system_config.max_directory_entries {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "复制来源单目录条目数",
                limit: system_config.max_directory_entries as u64,
                observed: direct_entries as u64,
            });
        }
        let entry = entry.map_err(|source_error| {
            io_error("读取复制来源目录项", &source_resolved, source_error)
        })?;
        let name = entry.file_name();
        let child_source = entry.path();
        validate_windows_name(&name, &child_source)?;
        let wide: Vec<u16> = name.encode_wide().collect();
        if names
            .iter()
            .any(|existing| windows_names_equal(existing, &wide))
        {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_source,
                reason: "同一目录包含 Windows 大小写等价名称",
            });
        }
        names.push(wide);
        let child_destination = destination_resolved.join(&name);
        let pinned_child = pin_path_without_reparse(&child_source)?;
        let metadata = pinned_child.metadata()?;
        let child_identity = FileIdentity::of(pinned_child.file(), &child_source)?;
        if metadata.is_dir() {
            copy_directory_tree(
                &child_source,
                &child_destination,
                Some(child_identity),
                depth + 1,
                budget,
                system_config,
            )?;
        } else if metadata.is_file() {
            copy_regular_file(
                &child_source,
                &child_destination,
                child_identity,
                metadata.len(),
                budget,
                &system_config.tree,
            )?;
        } else {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_source,
                reason: "复制来源包含非普通文件对象",
            });
        }
    }
    let after = pin_directory_without_reparse(source)?;
    let after_identity = FileIdentity::of(after.file(), source)?;
    let held_identity = FileIdentity::of(source_path.file(), source)?;
    if after_identity != source_identity || held_identity != source_identity {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源目录在枚举期间物理身份发生变化",
        });
    }
    Ok(())
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    expected_identity: FileIdentity,
    observed_size: u64,
    budget: &mut TreeBudgetUsage,
    config: &TreeBudget,
) -> Result<(), SystemFileSystemError> {
    if observed_size > config.max_single_file_bytes {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录候选单文件字节数",
            limit: config.max_single_file_bytes,
            observed: observed_size,
        });
    }
    budget.add_entry(config)?;
    let mut input = pin_regular_file_for_snapshot_read(source)?;
    let before = input.metadata()?;
    if !before.is_file() {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源不再是普通文件",
        });
    }
    let identity = FileIdentity::of(input.file(), source)?;
    if identity != expected_identity || before.len() != observed_size {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源文件在枚举后身份或大小发生变化",
        });
    }
    if number_of_links(input.file(), source)? != 1 {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源包含硬链接文件",
        });
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|source_error| io_error("建立候选文件", destination, source_error))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = input
            .file_mut()
            .read(&mut buffer)
            .map_err(|source_error| io_error("读取复制来源文件", source, source_error))?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > config.max_single_file_bytes {
            return Err(SystemFileSystemError::ResourceLimit {
                resource: "目录候选单文件字节数",
                limit: config.max_single_file_bytes,
                observed: copied,
            });
        }
        output
            .write_all(&buffer[..read])
            .map_err(|source_error| io_error("写入候选文件", destination, source_error))?;
    }
    let after = input.metadata()?;
    let after_identity = FileIdentity::of(input.file(), source)?;
    let after_links = number_of_links(input.file(), source)?;
    if copied != observed_size
        || after.len() != observed_size
        || after_identity != expected_identity
        || after_links != 1
    {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            reason: "复制来源文件在稳定读取期间发生变化",
        });
    }
    budget.add_bytes(copied, config)?;
    output
        .sync_data()
        .map_err(|source_error| io_error("同步候选文件", destination, source_error))?;
    Ok(())
}

fn ensure_empty_directory(
    stage_root: &Path,
    relative: &Path,
    budget: &mut TreeBudgetUsage,
    config: &TreeBudget,
) -> Result<(), SystemFileSystemError> {
    let mut current = stage_root.to_path_buf();
    let mut pinned_directories = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(SystemFileSystemError::InvalidPath {
                path: relative.to_path_buf(),
                reason: "空目录路径包含非普通段",
            });
        };
        current.push(name);
        match fs::create_dir(&current) {
            Ok(()) => budget.add_entry(config)?,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(io_error("建立候选空目录", &current, source)),
        }
        pinned_directories.push(pin_directory_without_reparse(&current)?);
    }
    Ok(())
}

fn validate_relative_windows_path(path: &Path) -> Result<(), SystemFileSystemError> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(SystemFileSystemError::InvalidPath {
                path: path.to_path_buf(),
                reason: "候选路径包含非普通相对段",
            });
        };
        validate_windows_name(name, path)?;
    }
    Ok(())
}

fn validate_windows_name(name: &OsStr, full_path: &Path) -> Result<(), SystemFileSystemError> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || matches!(wide.last(), Some(unit) if *unit == b'.' as u16 || *unit == b' ' as u16)
    {
        return Err(SystemFileSystemError::InvalidPath {
            path: full_path.to_path_buf(),
            reason: "Windows 名称为空或以点/空格结尾",
        });
    }
    if wide.iter().any(|unit| {
        matches!(
            *unit,
            0 | 1..=31 | 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124
        )
    }) {
        return Err(SystemFileSystemError::InvalidPath {
            path: full_path.to_path_buf(),
            reason: "Windows 名称包含控制字符、ADS 或保留符号",
        });
    }
    let ascii = String::from_utf16(&wide)
        .ok()
        .filter(|value| value.is_ascii())
        .map(|value| value.to_ascii_uppercase());
    if let Some(ascii) = ascii {
        let base = ascii.split('.').next().unwrap_or_default();
        let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
            || base.strip_prefix("COM").is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
            || base.strip_prefix("LPT").is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            });
        if reserved || ascii.to_ascii_lowercase().starts_with(RESERVED_PREFIX) {
            return Err(SystemFileSystemError::InvalidPath {
                path: full_path.to_path_buf(),
                reason: "Windows 名称属于设备名或发布根保留命名空间",
            });
        }
    }
    Ok(())
}

fn target_lock_path(
    configured_lock_directory: &Path,
    target_root: &Path,
) -> Result<PathBuf, SystemFileSystemError> {
    let lock_directory = trusted_lock_directory(configured_lock_directory)?;
    stable_lock_path(&lock_directory, target_root.as_os_str())
}

fn trusted_lock_directory(path: &Path) -> Result<PathBuf, SystemFileSystemError> {
    let lock_directory = absolutize(path.to_path_buf())?;
    let mut current = PathBuf::new();
    for component in lock_directory.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(io_error("建立锁目录", &current, source)),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|source| io_error("读取锁目录元数据", &current, source))?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SystemFileSystemError::InvalidPath {
                path: current,
                reason: "锁目录路径包含非普通目录或 reparse point",
            });
        }
    }
    let pinned = pin_directory_without_reparse(&lock_directory)?;
    Ok(pinned.resolved_path().to_path_buf())
}

fn stable_lock_path(
    lock_directory: &Path,
    identity: &OsStr,
) -> Result<PathBuf, SystemFileSystemError> {
    let key = invariant_uppercase_utf16(identity)?;
    let mut hasher = Sha256::new();
    hasher.update(b"exclusive-file-lease");
    hasher.update((key.len() as u64).to_le_bytes());
    for unit in key {
        hasher.update(unit.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut file_name = String::with_capacity(digest.len() * 2 + 5);
    use std::fmt::Write as _;
    for byte in digest {
        write!(&mut file_name, "{byte:02x}").expect("写入 String 不会失败");
    }
    file_name.push_str(".lock");
    Ok(lock_directory.join(file_name))
}

fn invariant_uppercase_utf16(value: &OsStr) -> Result<Vec<u16>, SystemFileSystemError> {
    let input = value.encode_wide().collect::<Vec<_>>();
    let input_len =
        i32::try_from(input.len()).map_err(|_| SystemFileSystemError::ResourceLimit {
            resource: "文件租约身份 UTF-16 单元数",
            limit: i32::MAX as u64,
            observed: input.len() as u64,
        })?;
    // SAFETY: 输入切片在调用期间有效；第一次调用只查询所需长度，不写入目标缓冲区。
    let required = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            input.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if required == 0 {
        return Err(io_error(
            "建立 Windows 非大小写敏感租约键",
            Path::new("<lock-identity>"),
            io::Error::last_os_error(),
        ));
    }
    let mut output = vec![0_u16; required as usize];
    // SAFETY: 目标缓冲区使用上一次系统调用给出的精确长度；输入与输出在调用期间有效。
    let written = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            input.as_ptr(),
            input_len,
            output.as_mut_ptr(),
            required,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if written != required {
        return Err(io_error(
            "建立 Windows 非大小写敏感租约键",
            Path::new("<lock-identity>"),
            io::Error::last_os_error(),
        ));
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum JournalPhase {
    OriginalMoveIntent,
    CandidateMoveIntent,
    CandidateVisible,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    operation_id: String,
    target_name: Vec<u16>,
    stage_name: Vec<u16>,
    backup_name: Vec<u16>,
    original_identity: FileIdentity,
    candidate_identity: FileIdentity,
    phase: JournalPhase,
}

fn append_journal(
    path: &Path,
    record: &JournalRecord,
    create_new: bool,
) -> Result<(), SystemFileSystemError> {
    let payload =
        serde_json::to_vec(record).map_err(|source| SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })?;
    if payload.len() > JOURNAL_MAX_BYTES {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录发布 journal 单帧字节数",
            limit: JOURNAL_MAX_BYTES as u64,
            observed: payload.len() as u64,
        });
    }
    let mut hasher = Hasher::new();
    hasher.update(&payload);
    let crc = hasher.finalize();
    let parent = path
        .parent()
        .ok_or_else(|| SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            reason: "目录发布 journal 路径没有父目录",
        })?;
    let _pinned_parent = pin_directory_without_reparse(parent)?;
    let mut options = OpenOptions::new();
    options
        .append(true)
        .create_new(create_new)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .map_err(|source| io_error("打开目录发布 journal", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("复核目录发布 journal", path, source))?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            reason: "journal 路径不是普通文件".to_owned(),
        });
    }
    file.write_all(&(payload.len() as u32).to_le_bytes())
        .and_then(|()| file.write_all(&payload))
        .and_then(|()| file.write_all(&crc.to_le_bytes()))
        .map_err(|source| io_error("写入目录发布 journal", path, source))?;
    file.sync_data()
        .map_err(|source| io_error("同步目录发布 journal", path, source))
}

fn read_journal(path: &Path) -> Result<Vec<JournalRecord>, SystemFileSystemError> {
    let mut pinned = pin_path_without_reparse(path)?;
    let metadata = pinned.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            reason: "journal 不是普通文件".to_owned(),
        });
    }
    if metadata.len() > JOURNAL_MAX_BYTES as u64 {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录发布 journal 字节数",
            limit: JOURNAL_MAX_BYTES as u64,
            observed: metadata.len(),
        });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(metadata.len() as usize)
        .map_err(|_| SystemFileSystemError::AllocationFailed {
            resource: "目录发布 journal",
            bytes: metadata.len(),
        })?;
    Read::by_ref(pinned.file_mut())
        .take(JOURNAL_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("读取目录发布 journal", path, source))?;
    if bytes.len() > JOURNAL_MAX_BYTES {
        return Err(SystemFileSystemError::ResourceLimit {
            resource: "目录发布 journal 字节数",
            limit: JOURNAL_MAX_BYTES as u64,
            observed: bytes.len() as u64,
        });
    }
    let mut offset = 0_usize;
    let mut records: Vec<JournalRecord> = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < size_of_u32() {
            break;
        }
        let length = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("已确认 journal 长度头完整"),
        ) as usize;
        if length > JOURNAL_MAX_BYTES {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: "帧长度超过 journal 上限".to_owned(),
            });
        }
        let frame_end = offset + 4 + length + 4;
        if frame_end > bytes.len() {
            break;
        }
        let payload = &bytes[offset + 4..offset + 4 + length];
        let expected_crc = u32::from_le_bytes(
            bytes[offset + 4 + length..frame_end]
                .try_into()
                .expect("已确认 journal CRC 完整"),
        );
        let mut hasher = Hasher::new();
        hasher.update(payload);
        if hasher.finalize() != expected_crc {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: "完整帧 CRC 不匹配".to_owned(),
            });
        }
        let record: JournalRecord = serde_json::from_slice(payload).map_err(|source| {
            SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: format!("完整帧 JSON 无效：{source}"),
            }
        })?;
        let parsed_operation = Uuid::parse_str(&record.operation_id).map_err(|source| {
            SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: format!("operation_id 无效：{source}"),
            }
        })?;
        if parsed_operation.get_version_num() != 4
            || parsed_operation.to_string() != record.operation_id
        {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: "operation_id 不是规范 UUID v4".to_owned(),
            });
        }
        if let Some(first) = records.first()
            && (first.operation_id != record.operation_id
                || first.target_name != record.target_name
                || first.stage_name != record.stage_name
                || first.backup_name != record.backup_name
                || first.original_identity != record.original_identity
                || first.candidate_identity != record.candidate_identity)
        {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: "同一 journal 的完整帧身份不一致".to_owned(),
            });
        }
        let expected_phase = match records.len() {
            0 => JournalPhase::OriginalMoveIntent,
            1 => JournalPhase::CandidateMoveIntent,
            2 => JournalPhase::CandidateVisible,
            _ => {
                return Err(SystemFileSystemError::JournalCorrupt {
                    path: path.to_path_buf(),
                    reason: "journal 包含多余完整帧".to_owned(),
                });
            }
        };
        if record.phase != expected_phase {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                reason: "journal 阶段顺序无效".to_owned(),
            });
        }
        records.push(record);
        offset = frame_end;
    }
    Ok(records)
}

const fn size_of_u32() -> usize {
    std::mem::size_of::<u32>()
}

fn publish_directory_sync(
    staged: StagedDirectory<SystemStagingState>,
    expected_identity: &Arc<()>,
    config: &SystemFileSystemConfig,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let (target_root, stage_root, intent, mut state) = staged.into_parts();
    state.finalized = true;
    if !Arc::ptr_eq(&state.publisher_identity, expected_identity) {
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(SystemFileSystemError::WrongPublisherInstance),
            cleanup_failure,
        });
    }
    if state.target_lock.is_none() {
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(SystemFileSystemError::InvalidStagedIdentity {
                path: state.parent_root.clone(),
            }),
            cleanup_failure,
        });
    }
    if let Err(source) = verify_staged_state(&state, &stage_root) {
        let artifacts = vec![stage_root.clone()];
        state.cleanup.disarm();
        return Err(DirectoryPublishError::OutcomeUnknown {
            target_root,
            recovery_artifacts: artifacts,
            source: Box::new(source),
        });
    }
    if let Err(source) = validate_complete_candidate(&stage_root, state.stage_identity, config) {
        if matches!(&source, SystemFileSystemError::InvalidStagedIdentity { .. }) {
            state.cleanup.disarm();
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![stage_root],
                source: Box::new(source),
            });
        }
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    match intent {
        DirectoryPublishIntent::CreateNew => {
            publish_create_new(target_root, stage_root, &mut state)
        }
        DirectoryPublishIntent::ReplaceExisting => {
            publish_replace(target_root, stage_root, &mut state)
        }
    }
}

fn verify_staged_state(
    state: &SystemStagingState,
    stage_root: &Path,
) -> Result<(), SystemFileSystemError> {
    let parent_handle = state.parent_handle.as_ref().ok_or_else(|| {
        SystemFileSystemError::InvalidStagedIdentity {
            path: state.parent_root.clone(),
        }
    })?;
    if FileIdentity::of(parent_handle, &state.parent_root)? != state.parent_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: state.parent_root.clone(),
        });
    }
    let stage_handle = state.stage_handle.as_ref().ok_or_else(|| {
        SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        }
    })?;
    if FileIdentity::of(stage_handle, stage_root)? != state.stage_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    let current = open_directory(stage_root, true)?;
    if FileIdentity::of(&current, stage_root)? != state.stage_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    Ok(())
}

fn publish_create_new(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    match fs::symlink_metadata(&target_root) {
        Ok(_) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            let source = io_error("读取新建发布目标元数据", &target_root, source);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            });
        }
    }
    state.stage_handle.take();
    match rename_without_replace_if_identity(&stage_root, &target_root, state.stage_identity) {
        Ok(()) => {
            state.cleanup.disarm();
            match identity_at(&target_root) {
                Ok(Some(identity)) if identity == state.stage_identity => Ok(()),
                _ => Err(DirectoryPublishError::OutcomeUnknown {
                    target_root,
                    recovery_artifacts: Vec::new(),
                    source: Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root,
                    }),
                }),
            }
        }
        Err(WindowsFsError::RenameTargetExists { .. }) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            Err(DirectoryPublishError::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            })
        }
        Err(WindowsFsError::FileIdentityChanged { .. }) => {
            state.cleanup.disarm();
            Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![stage_root.clone()],
                source: Box::new(SystemFileSystemError::InvalidStagedIdentity { path: stage_root }),
            })
        }
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            Err(DirectoryPublishError::NotPublished {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            })
        }
    }
}

fn publish_replace(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let target_metadata = match fs::symlink_metadata(&target_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::TargetMissing {
                target_root,
                cleanup_failure,
            });
        }
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            let source = io_error("读取替换目标元数据", &target_root, source);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            });
        }
    };
    if !target_metadata.is_dir()
        || target_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        let cleanup_failure = cleanup_state(state, &stage_root);
        return Err(DirectoryPublishError::TargetNotDirectory {
            target_root,
            cleanup_failure,
        });
    }
    let target_handle = match open_directory(&target_root, true) {
        Ok(handle) => handle,
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            });
        }
    };
    let original_identity = match FileIdentity::of(&target_handle, &target_root) {
        Ok(identity) => identity,
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            });
        }
    };
    drop(target_handle);
    let mut record = JournalRecord {
        operation_id: state.operation_id.to_string(),
        target_name: target_root
            .file_name()
            .expect("受信目标必有名称")
            .encode_wide()
            .collect(),
        stage_name: stage_root
            .file_name()
            .expect("受信候选必有名称")
            .encode_wide()
            .collect(),
        backup_name: state
            .backup_path
            .file_name()
            .expect("受信备份必有名称")
            .encode_wide()
            .collect(),
        original_identity,
        candidate_identity: state.stage_identity,
        phase: JournalPhase::OriginalMoveIntent,
    };
    if let Err(source) = append_journal(&state.journal_path, &record, true) {
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterOriginalJournal);
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeOriginalMove) {
        let source = injected_publish_error("移动旧目标", &target_root);
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    if let Err(source) =
        rename_without_replace_if_identity(&target_root, &state.backup_path, original_identity)
    {
        if matches!(source, WindowsFsError::FileIdentityChanged { .. }) {
            state.cleanup.disarm();
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root: target_root.clone(),
                recovery_artifacts: vec![target_root, stage_root, state.journal_path.clone()],
                source: Box::new(source.into()),
            });
        }
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source.into()),
            cleanup_failure,
        });
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterOriginalMove);
    }
    record.phase = JournalPhase::CandidateMoveIntent;
    if let Err(source) = append_journal(&state.journal_path, &record, false) {
        return restore_old_after_failure(
            target_root,
            stage_root,
            state,
            original_identity,
            source,
            false,
        );
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateIntent);
    }
    state.stage_handle.take();
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeCandidateMove) {
        return restore_old_after_failure(
            target_root,
            stage_root.clone(),
            state,
            original_identity,
            injected_publish_error("移动候选目录", &stage_root),
            false,
        );
    }
    if let Err(source) =
        rename_without_replace_if_identity(&stage_root, &target_root, state.stage_identity)
    {
        let preserve_unknown_stage = matches!(source, WindowsFsError::FileIdentityChanged { .. });
        let source = if preserve_unknown_stage {
            SystemFileSystemError::InvalidStagedIdentity {
                path: stage_root.clone(),
            }
        } else {
            source.into()
        };
        return restore_old_after_failure(
            target_root,
            stage_root,
            state,
            original_identity,
            source,
            preserve_unknown_stage,
        );
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateMove);
    }
    state.cleanup.disarm();
    record.phase = JournalPhase::CandidateVisible;
    if let Err(source) = append_journal(&state.journal_path, &record, false) {
        return if identity_at(&target_root)
            .is_ok_and(|identity| identity == Some(state.stage_identity))
        {
            Err(DirectoryPublishError::PublishedWithResiduals {
                target_root,
                residual_path: state.journal_path.clone(),
                source: Box::new(source),
            })
        } else {
            Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![state.backup_path.clone(), state.journal_path.clone()],
                source: Box::new(source),
            })
        };
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateVisible);
    }
    match identity_at(&target_root) {
        Ok(Some(identity)) if identity == state.stage_identity => {}
        Ok(_) | Err(_) => {
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![state.backup_path.clone(), state.journal_path.clone()],
                source: Box::new(SystemFileSystemError::InvalidStagedIdentity { path: stage_root }),
            });
        }
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeBackupCleanup) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.backup_path.clone(),
            source: Box::new(injected_publish_error(
                "清理目录发布备份",
                &state.backup_path,
            )),
        });
    }
    if let Err(source) = remove_directory_tree_if_identity(&state.backup_path, original_identity) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.backup_path.clone(),
            source: Box::new(source),
        });
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeJournalCleanup) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.journal_path.clone(),
            source: Box::new(injected_publish_error(
                "清理目录发布 journal",
                &state.journal_path,
            )),
        });
    }
    if let Err(source) = remove_file_if_exists(&state.journal_path) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.journal_path.clone(),
            source: Box::new(source),
        });
    }
    Ok(())
}

fn restore_old_after_failure(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
    original_identity: FileIdentity,
    source: SystemFileSystemError,
    preserve_unknown_stage: bool,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let restore_result: Result<(), SystemFileSystemError> = {
        #[cfg(test)]
        {
            if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeRestoreMove) {
                Err(injected_publish_error("恢复旧发布目标", &state.backup_path))
            } else {
                rename_without_replace_if_identity(
                    &state.backup_path,
                    &target_root,
                    original_identity,
                )
                .map_err(Into::into)
            }
        }
        #[cfg(not(test))]
        {
            rename_without_replace_if_identity(&state.backup_path, &target_root, original_identity)
                .map_err(Into::into)
        }
    };
    match restore_result {
        Ok(()) => {
            let cleanup_failure = cleanup_after_restore(state, &stage_root, preserve_unknown_stage);
            let cleanup_failure = include_file_cleanup_failure(
                cleanup_failure,
                &state.journal_path,
                remove_file_if_exists(&state.journal_path),
            );
            Err(DirectoryPublishError::NotPublished {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            })
        }
        Err(restore) => {
            let target_identity = identity_at(&target_root);
            match target_identity {
                Ok(Some(identity)) if identity == state.stage_identity => {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::PublishedWithResiduals {
                        target_root,
                        residual_path: state.backup_path.clone(),
                        source: Box::new(restore),
                    })
                }
                Ok(Some(identity)) if identity == original_identity => {
                    let cleanup_failure =
                        cleanup_after_restore(state, &stage_root, preserve_unknown_stage);
                    let cleanup_failure = include_file_cleanup_failure(
                        cleanup_failure,
                        &state.journal_path,
                        remove_file_if_exists(&state.journal_path),
                    );
                    Err(DirectoryPublishError::NotPublished {
                        target_root,
                        source: Box::new(source),
                        cleanup_failure,
                    })
                }
                Ok(None)
                    if identity_at(&state.backup_path)
                        .is_ok_and(|identity| identity == Some(original_identity)) =>
                {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::RecoveryRequired {
                        target_root,
                        recovery_artifacts: vec![
                            state.backup_path.clone(),
                            stage_root,
                            state.journal_path.clone(),
                        ],
                        source: Box::new(restore),
                    })
                }
                Ok(_) | Err(_) => {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::OutcomeUnknown {
                        target_root,
                        recovery_artifacts: vec![
                            state.backup_path.clone(),
                            stage_root,
                            state.journal_path.clone(),
                        ],
                        source: Box::new(restore),
                    })
                }
            }
        }
    }
}

fn cleanup_after_restore(
    state: &mut SystemStagingState,
    stage_root: &Path,
    preserve_unknown_stage: bool,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    if preserve_unknown_stage {
        state.stage_handle.take();
        state.cleanup.disarm();
        Some(StagingCleanupFailure::new(
            stage_root.to_path_buf(),
            Box::new(SystemFileSystemError::InvalidStagedIdentity {
                path: stage_root.to_path_buf(),
            }),
        ))
    } else {
        cleanup_state(state, stage_root)
    }
}

fn cleanup_state(
    state: &mut SystemStagingState,
    stage_root: &Path,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    state.stage_handle.take();
    state
        .cleanup
        .cleanup()
        .err()
        .map(|source| StagingCleanupFailure::new(stage_root.to_path_buf(), Box::new(source)))
}

fn include_file_cleanup_failure(
    existing: Option<StagingCleanupFailure<Box<SystemFileSystemError>>>,
    residual_path: &Path,
    result: Result<(), SystemFileSystemError>,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    match (existing, result) {
        (Some(failure), _) => Some(failure),
        (None, Ok(())) => None,
        (None, Err(source)) => Some(StagingCleanupFailure::new(
            residual_path.to_path_buf(),
            Box::new(source),
        )),
    }
}

fn discard_directory_sync(
    staged: StagedDirectory<SystemStagingState>,
    expected_identity: &Arc<()>,
) -> Result<(), SystemFileSystemError> {
    let (_target_root, stage_root, _intent, mut state) = staged.into_parts();
    state.finalized = true;
    if !Arc::ptr_eq(&state.publisher_identity, expected_identity) {
        return Err(SystemFileSystemError::WrongPublisherInstance);
    }
    verify_staged_state(&state, &stage_root)?;
    state.stage_handle.take();
    state.cleanup.cleanup().map_err(|source| match source {
        SystemFileSystemError::Io { .. } => source,
        other => io_error(
            "丢弃目录候选",
            &stage_root,
            io::Error::other(other.to_string()),
        ),
    })
}

fn remove_file_if_exists(path: &Path) -> Result<(), SystemFileSystemError> {
    let pinned = match pin_path_without_reparse(path) {
        Ok(pinned) => pinned,
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(source) => return Err(source.into()),
    };
    if !pinned.metadata()?.is_file() {
        return Err(SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            reason: "目录发布 journal 路径不是普通文件",
        });
    }
    let identity = FileIdentity::of(pinned.file(), path)?;
    drop(pinned);
    delete_regular_file_if_identity(path, identity).map_err(Into::into)
}

fn identity_at(path: &Path) -> Result<Option<FileIdentity>, SystemFileSystemError> {
    match open_directory(path, true) {
        Ok(file) => FileIdentity::of(&file, path).map(Some).map_err(Into::into),
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(source) => Err(source.into()),
    }
}

fn recover_target(
    target_root: &Path,
    target_artifact_key: &str,
    config: &DirectoryPublisherConfig,
) -> Result<(), SystemFileSystemError> {
    let parent = target_root.parent().expect("受信发布目标必有父目录");
    let prefix = format!("{RESERVED_PREFIX}{target_artifact_key}-");
    let mut artifacts = Vec::new();
    for entry in
        fs::read_dir(parent).map_err(|source| io_error("列举目录恢复产物", parent, source))?
    {
        let entry = entry.map_err(|source| io_error("读取目录恢复产物", parent, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix)
            && (name.ends_with(".journal") || name.ends_with(".stage") || name.ends_with(".backup"))
        {
            artifacts.push(entry.path());
            if artifacts.len() > config.max_recovery_artifacts_per_target {
                return Err(SystemFileSystemError::ResourceLimit {
                    resource: "单目标目录恢复产物数",
                    limit: config.max_recovery_artifacts_per_target as u64,
                    observed: artifacts.len() as u64,
                });
            }
        }
    }
    let journals = artifacts
        .iter()
        .filter(|path| path.extension() == Some(OsStr::new("journal")))
        .cloned()
        .collect::<Vec<_>>();
    for journal in journals {
        recover_journal(target_root, &journal)?;
    }
    for artifact in artifacts {
        let metadata = match fs::symlink_metadata(&artifact) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(io_error("读取目录恢复产物元数据", &artifact, source));
            }
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: vec![artifact],
                reason: "恢复产物路径被 reparse point 占用".to_owned(),
            });
        }
        match artifact.extension().and_then(OsStr::to_str) {
            Some("stage") => {
                let journal = artifact.with_extension("journal");
                if !path_exists(&journal)? {
                    let pinned = pin_directory_without_reparse(&artifact)?;
                    let identity = FileIdentity::of(pinned.file(), &artifact)?;
                    drop(pinned);
                    remove_directory_tree_if_identity(&artifact, identity)?;
                }
            }
            Some("backup") => {
                let journal = artifact.with_extension("journal");
                if !path_exists(&journal)? {
                    return Err(SystemFileSystemError::OutcomeUnknown {
                        target_root: target_root.to_path_buf(),
                        artifacts: vec![artifact],
                        reason: "备份存在但缺少对应 journal".to_owned(),
                    });
                }
            }
            Some("journal") => {
                return Err(SystemFileSystemError::OutcomeUnknown {
                    target_root: target_root.to_path_buf(),
                    artifacts: vec![artifact],
                    reason: "journal 恢复后仍然残留".to_owned(),
                });
            }
            _ => unreachable!("恢复产物已按受信后缀筛选"),
        }
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, SystemFileSystemError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("读取路径存在状态", path, source)),
    }
}

fn recover_journal(target_root: &Path, journal: &Path) -> Result<(), SystemFileSystemError> {
    let parent = target_root.parent().expect("受信发布目标必有父目录");
    let records = read_journal(journal)?;
    if records.is_empty() {
        remove_file_if_exists(journal)?;
        return Ok(());
    }
    let record = records.last().expect("非空 journal 必有末帧");
    let target_name: Vec<u16> = target_root
        .file_name()
        .expect("受信发布目标必有名称")
        .encode_wide()
        .collect();
    if !windows_names_equal(&record.target_name, &target_name) {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: vec![journal.to_path_buf()],
            reason: "journal 目标名称与恢复请求不一致".to_owned(),
        });
    }
    let journal_stem = journal.file_stem().and_then(OsStr::to_str).ok_or_else(|| {
        SystemFileSystemError::JournalCorrupt {
            path: journal.to_path_buf(),
            reason: "journal 文件名不是受信 ASCII 发布名称".to_owned(),
        }
    })?;
    let operation_suffix = format!("-{}", record.operation_id);
    let artifact_key = journal_stem
        .strip_prefix(RESERVED_PREFIX)
        .and_then(|value| value.strip_suffix(&operation_suffix))
        .filter(|value| value.len() == 48 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| SystemFileSystemError::JournalCorrupt {
            path: journal.to_path_buf(),
            reason: "journal 文件名与操作 ID 或目标锁身份不一致".to_owned(),
        })?;
    let expected_stem = format!("{RESERVED_PREFIX}{artifact_key}{operation_suffix}");
    let expected_stage_name = format!("{expected_stem}.stage")
        .encode_utf16()
        .collect::<Vec<_>>();
    let expected_backup_name = format!("{expected_stem}.backup")
        .encode_utf16()
        .collect::<Vec<_>>();
    if record.stage_name != expected_stage_name || record.backup_name != expected_backup_name {
        return Err(SystemFileSystemError::JournalCorrupt {
            path: journal.to_path_buf(),
            reason: "journal 恢复产物名称不属于该发布操作".to_owned(),
        });
    }
    let stage = parent.join(OsString::from_wide(&record.stage_name));
    let backup = parent.join(OsString::from_wide(&record.backup_name));
    let target_identity = identity_at(target_root)?;
    let stage_identity = identity_at(&stage)?;
    let backup_identity = identity_at(&backup)?;
    if target_identity == Some(record.candidate_identity) {
        remove_matching_directory(&backup, record.original_identity, target_root, journal)?;
        remove_matching_directory(&stage, record.candidate_identity, target_root, journal)?;
        remove_file_if_exists(journal)?;
        return Ok(());
    }
    if target_identity == Some(record.original_identity) {
        remove_matching_directory(&stage, record.candidate_identity, target_root, journal)?;
        remove_matching_directory(&backup, record.original_identity, target_root, journal)?;
        remove_file_if_exists(journal)?;
        return Ok(());
    }
    if target_identity.is_some() {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: vec![stage, backup, journal.to_path_buf()],
            reason: "目标被未知文件身份占用".to_owned(),
        });
    }
    if backup_identity == Some(record.original_identity) {
        if let Err(source) =
            rename_without_replace_if_identity(&backup, target_root, record.original_identity)
        {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: vec![stage, backup, journal.to_path_buf()],
                reason: format!("恢复旧目标时备份身份或重命名状态变化：{source}"),
            });
        }
        if identity_at(target_root)? != Some(record.original_identity) {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: vec![stage, backup, journal.to_path_buf()],
                reason: "恢复旧目标后文件身份不匹配".to_owned(),
            });
        }
        if stage_identity == Some(record.candidate_identity) {
            remove_directory_tree_if_identity(&stage, record.candidate_identity)?;
        } else if stage_identity.is_some() {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: vec![stage, journal.to_path_buf()],
                reason: "候选路径出现未知文件身份".to_owned(),
            });
        }
        remove_file_if_exists(journal)?;
        return Ok(());
    }
    if backup_identity.is_some() {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: vec![stage, backup, journal.to_path_buf()],
            reason: "备份路径出现未知文件身份".to_owned(),
        });
    }
    Err(SystemFileSystemError::RecoveryRequired {
        target_root: target_root.to_path_buf(),
        artifacts: vec![stage, journal.to_path_buf()],
        reason: "目标与已知旧目录均缺失".to_owned(),
    })
}

fn remove_matching_directory(
    path: &Path,
    expected: FileIdentity,
    target_root: &Path,
    journal: &Path,
) -> Result<(), SystemFileSystemError> {
    match identity_at(path)? {
        None => Ok(()),
        Some(identity) if identity == expected => remove_directory_tree_if_identity(path, expected),
        Some(_) => Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: vec![path.to_path_buf(), journal.to_path_buf()],
            reason: "恢复产物路径出现未知文件身份".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Deref;

    #[derive(Clone)]
    struct TestDirectoryPublisher {
        file_system: SystemFileSystem,
        publisher: SystemDirectoryPublisher,
    }

    impl TestDirectoryPublisher {
        fn new(config: SystemFileSystemConfig) -> Self {
            let file_system = SystemFileSystem::new(config).expect("应该可建立文件系统根");
            let publisher = file_system.directory_publisher(publisher_config());
            Self {
                file_system,
                publisher,
            }
        }

        fn with_publisher_config(
            config: SystemFileSystemConfig,
            publisher_config: DirectoryPublisherConfig,
        ) -> Self {
            let file_system = SystemFileSystem::new(config).expect("应该可建立文件系统根");
            let publisher = file_system.directory_publisher(publisher_config);
            Self {
                file_system,
                publisher,
            }
        }

        async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
            self.file_system.shutdown().await
        }
    }

    impl Deref for TestDirectoryPublisher {
        type Target = SystemDirectoryPublisher;

        fn deref(&self) -> &Self::Target {
            &self.publisher
        }
    }

    fn symlink_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) || error.raw_os_error() == Some(1314)
    }

    fn publisher_config() -> DirectoryPublisherConfig {
        publisher_config_for_lock_directory(
            std::env::temp_dir().join("filesystem-test-publisher-locks"),
        )
    }

    fn publisher_config_for_lock_directory(lock_directory: PathBuf) -> DirectoryPublisherConfig {
        DirectoryPublisherConfig::new(lock_directory, 8, Duration::from_secs(1))
            .expect("测试发布配置应该合法")
    }

    fn tree_budget() -> TreeBudget {
        TreeBudget::new(128, 16, 1024 * 1024, 512 * 1024).expect("测试目录树预算应该合法")
    }

    fn exclusive_file_lease_config() -> ExclusiveFileLeaseConfig {
        ExclusiveFileLeaseConfig::new(Duration::from_secs(1)).expect("测试租约配置应该合法")
    }

    fn file_system_config() -> SystemFileSystemConfig {
        file_system_config_with_lease_timeout(Duration::from_secs(1))
    }

    fn file_system_config_with_lease_timeout(timeout: Duration) -> SystemFileSystemConfig {
        SystemFileSystemConfig::new(
            2,
            8,
            1024,
            16,
            tree_budget(),
            ExclusiveFileLeaseConfig::new(timeout).expect("测试租约配置应该合法"),
        )
        .expect("测试文件系统配置应该合法")
    }

    fn tree_fingerprint_request(assets: &Path, scripts: &Path) -> DirectoryTreeFingerprintRequest {
        DirectoryTreeFingerprintRequest::new(vec![
            crate::storage::file_system::DirectoryTreeRoot::new(
                assets.to_path_buf(),
                PathBuf::from("assets"),
            )
            .expect("资源指纹根应该合法"),
            crate::storage::file_system::DirectoryTreeRoot::new(
                scripts.to_path_buf(),
                PathBuf::from("scripts"),
            )
            .expect("脚本指纹根应该合法"),
        ])
        .expect("资源与脚本指纹根应该互不重叠")
    }

    fn stage_request(
        target: PathBuf,
        source: PathBuf,
        intent: DirectoryPublishIntent,
    ) -> DirectoryStageRequest {
        DirectoryStageRequest::new(
            target,
            intent,
            vec![
                DirectorySourceMapping::new(source, PathBuf::from("snapshot/content"))
                    .expect("测试来源映射应该合法"),
            ],
            Vec::new(),
            vec![PathBuf::from("empty")],
        )
        .expect("测试候选请求应该合法")
    }

    fn scoped_stage_request(
        target: PathBuf,
        source_root: &Path,
        intent: DirectoryPublishIntent,
    ) -> DirectoryStageRequest {
        DirectoryStageRequest::new(
            target,
            intent,
            vec![
                DirectorySourceMapping::new(source_root.join("assets"), PathBuf::from("assets"))
                    .expect("资源候选映射应该合法"),
                DirectorySourceMapping::new(source_root.join("scripts"), PathBuf::from("scripts"))
                    .expect("脚本候选映射应该合法"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect("受限编辑候选请求应该合法")
    }

    fn scoped_path(path: &str) -> ScopedDirectoryPath {
        ScopedDirectoryPath::new(PathBuf::from(path)).expect("测试候选路径应该合法")
    }

    fn scoped_scope() -> ScopedDirectoryScope {
        ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
            .expect("测试候选范围应该合法")
    }

    fn scoped_source(root: &Path) {
        fs::create_dir_all(root.join("assets")).expect("应该可建立资源来源");
        fs::create_dir_all(root.join("scripts")).expect("应该可建立脚本来源");
        fs::write(root.join("assets/catalog.json"), b"items").expect("应该可建立资源文件");
        fs::write(root.join("scripts/main.lua"), b"scripts").expect("应该可建立脚本文件");
    }

    fn canonical_target(path: &Path) -> PathBuf {
        path.parent()
            .expect("测试目标应有父目录")
            .canonicalize()
            .expect("测试目标父目录应可规范化")
            .join(path.file_name().expect("测试目标应有名称"))
    }

    fn subprocess_command(mode: &str, target: &Path, source: &Path) -> std::process::Command {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("应可定位当前测试进程"));
        command
            .arg("--exact")
            .arg("runtime::filesystem::tests::publisher_subprocess_entrypoint")
            .arg("--nocapture")
            .env("FILESYSTEM_PUBLISHER_CHILD_MODE", mode)
            .env("FILESYSTEM_PUBLISHER_CHILD_TARGET", target)
            .env("FILESYSTEM_PUBLISHER_CHILD_SOURCE", source)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    #[test]
    fn windows_name_validation_rejects_devices_ads_and_reserved_namespace() {
        for name in [
            "CON",
            "nul.txt",
            "file:ads",
            "trailing.",
            ".directory-publish-x",
        ] {
            assert!(validate_windows_name(OsStr::new(name), Path::new(name)).is_err());
        }
        validate_windows_name(OsStr::new("剧情 数据.json"), Path::new("剧情 数据.json"))
            .expect("Unicode 普通名称应该合法");
    }

    #[test]
    fn declared_paths_reject_conflicting_windows_case_spelling() {
        let mappings = vec![
            DirectorySourceMapping::new(PathBuf::from("first"), PathBuf::from("content/first"))
                .expect("第一条声明应合法"),
            DirectorySourceMapping::new(PathBuf::from("second"), PathBuf::from("Content/second"))
                .expect("大小写不同的原始声明尚未进入 Windows 边界"),
        ];
        assert!(matches!(
            validate_declared_windows_paths(&mappings, &[], &[], &tree_budget()),
            Err(SystemFileSystemError::InvalidPath { reason, .. })
                if reason.contains("大小写")
        ));
    }

    #[test]
    fn journal_ignores_only_the_final_incomplete_frame() {
        let root = std::env::temp_dir().join(format!("journal-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("测试目录应该可创建");
        let path = root.join("state.journal");
        let record = JournalRecord {
            operation_id: Uuid::new_v4().to_string(),
            target_name: "target".encode_utf16().collect(),
            stage_name: "stage".encode_utf16().collect(),
            backup_name: "backup".encode_utf16().collect(),
            original_identity: FileIdentity::from_parts(1, [2; 16]),
            candidate_identity: FileIdentity::from_parts(1, [3; 16]),
            phase: JournalPhase::OriginalMoveIntent,
        };
        append_journal(&path, &record, true).expect("完整帧应该可写入");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("journal 应该可追加")
            .write_all(&[5, 0])
            .expect("应该可写入截断帧");
        let records = read_journal(&path).expect("最终不完整帧应该回退");
        assert_eq!(records.len(), 1);
        fs::remove_dir_all(root).expect("测试目录应该可清理");
    }

    #[test]
    fn publisher_configuration_rejects_zero_resource_budget() {
        assert!(DirectoryPublisherConfig::new(PathBuf::from("locks"), 0, Duration::ZERO).is_err());
        assert!(DirectoryPublisherConfig::new(PathBuf::new(), 1, Duration::ZERO).is_err());
        assert!(TreeBudget::new(0, 1, 1, 1).is_err());
        assert!(ExclusiveFileLeaseConfig::new(Duration::ZERO).is_err());
        let _ = publisher_config();
    }

    #[tokio::test]
    async fn direct_child_directory_ensure_is_concurrently_idempotent() {
        let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
        let parent = temporary.path().join("projects");
        fs::create_dir(&parent).expect("应该可创建现存父目录");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        let first = root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"));
        let second = root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"));
        let (first, second) = tokio::join!(first, second);
        let expected = parent
            .join("mz")
            .canonicalize()
            .expect("MZ 直接子目录应该可规范化");
        assert_eq!(first.expect("首次建立应该成功"), expected);
        assert_eq!(second.expect("并发重复建立应该成功"), expected);
        assert_eq!(fs::read_dir(&parent).expect("应该可列举项目根").count(), 1);

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn direct_child_directory_ensure_rejects_non_segment_and_existing_file() {
        let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
        let parent = temporary.path().join("projects");
        fs::create_dir(&parent).expect("应该可创建现存父目录");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        for child in ["", ".", "nested/mz"] {
            assert!(
                root.ensure_direct_child_directory(parent.clone(), OsString::from(child))
                    .await
                    .is_err(),
                "非普通单段名称必须被拒绝：{child:?}"
            );
        }
        assert!(!parent.join("nested").exists());

        let missing_parent = temporary.path().join("missing/projects");
        assert!(
            root.ensure_direct_child_directory(missing_parent.clone(), OsString::from("mz"),)
                .await
                .is_err(),
            "能力不得递归建立缺失父目录"
        );
        assert!(!temporary.path().join("missing").exists());

        let occupied = parent.join("mz");
        fs::write(&occupied, b"occupied").expect("应该可用普通文件占用名称");
        assert!(
            root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"))
                .await
                .is_err(),
            "已有普通文件不得被当作目录"
        );
        assert!(occupied.is_file());

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn direct_child_directory_ensure_rejects_reparse_point() {
        let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
        let parent = temporary.path().join("projects");
        let target = temporary.path().join("foreign");
        fs::create_dir(&parent).expect("应该可创建现存父目录");
        fs::create_dir(&target).expect("应该可创建链接目标");
        let link = parent.join("mz");
        if let Err(error) = std::os::windows::fs::symlink_dir(&target, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建目录符号链接：{error}");
        }
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        assert!(
            root.ensure_direct_child_directory(parent, OsString::from("mz"))
                .await
                .is_err(),
            "reparse point 不得成为受信直接子目录"
        );
        assert!(target.is_dir(), "拒绝 reparse point 不得修改链接目标");

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn exclusive_file_lease_serializes_same_windows_identity_and_allows_other_identities() {
        let temporary = tempfile::tempdir().expect("应该可创建临时项目集合根");
        let owner = SystemFileSystem::new(file_system_config()).expect("应该可建立锁所有者根");
        let contender = SystemFileSystem::new(file_system_config_with_lease_timeout(
            Duration::from_millis(25),
        ))
        .expect("应该可建立锁竞争者根");
        let lock_directory = temporary.path().join("locks/leases");
        let request = |name: &str| {
            ExclusiveFileLeaseRequest::new(lock_directory.clone(), OsString::from(name))
                .expect("测试文件租约请求应该合法")
        };

        let lease = owner
            .acquire_exclusive_file_lease(request("游戏One"))
            .await
            .expect("首个同项目命令应该取得租约");
        let other_lease = contender
            .acquire_exclusive_file_lease(request("游戏Two"))
            .await
            .expect("不同身份应该可以并行");
        assert!(matches!(
            contender
                .acquire_exclusive_file_lease(request("游戏one"))
                .await,
            Err(ExclusiveFileLeaseError::Busy { identity, .. })
                if identity == OsStr::new("游戏one")
        ));
        assert!(lock_directory.is_dir());
        assert_eq!(
            fs::read_dir(&lock_directory)
                .expect("应该可列举锁目录")
                .count(),
            2,
            "两个 Windows 身份应该各自只建立一个摘要锁文件"
        );

        drop(other_lease);
        drop(lease);
        let reacquired = contender
            .acquire_exclusive_file_lease(request("游戏one"))
            .await
            .expect("所有者释放后应该可重新取得同身份租约");
        drop(reacquired);
        contender.shutdown().await.expect("竞争者根应该可终结");
        owner.shutdown().await.expect("所有者根应该可终结");
    }

    #[tokio::test]
    async fn publisher_lock_lives_in_the_configured_lock_directory() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let lock_directory = temporary.path().join("locks/publish");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建候选来源");
        let target = temporary.path().join("outputs/target");
        fs::create_dir(target.parent().expect("测试目标应该有父目录"))
            .expect("应该可创建目标父目录");
        let root = TestDirectoryPublisher::with_publisher_config(
            file_system_config(),
            publisher_config_for_lock_directory(lock_directory.clone()),
        );

        let staged = root
            .prepare(stage_request(
                target.clone(),
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");

        assert!(lock_directory.is_dir());
        let lock_files = fs::read_dir(&lock_directory)
            .expect("应该可列举目录发布锁")
            .collect::<Result<Vec<_>, _>>()
            .expect("应该可读取目录发布锁项");
        assert_eq!(lock_files.len(), 1);
        assert_eq!(lock_files[0].path().extension(), Some(OsStr::new("lock")));
        assert!(!target.parent().unwrap().join("locks").exists());

        root.discard(staged).await.expect("应该可丢弃候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn directory_tree_fingerprint_depends_only_on_framed_logical_names_kinds_and_bytes() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        for root in [&first, &second] {
            fs::create_dir_all(root.join("assets/catalog")).expect("应该可建立资源子目录");
            fs::create_dir_all(root.join("assets/empty")).expect("应该可建立空目录");
            fs::create_dir_all(root.join("scripts/modules")).expect("应该可建立脚本子目录");
        }
        fs::write(first.join("scripts/modules/main.lua"), b"module").expect("应该可写入第一份脚本");
        fs::write(first.join("assets/catalog/index.json"), b"catalog")
            .expect("应该可写入第一份资源");
        // 反转创建顺序，证明枚举顺序不参与指纹。
        fs::write(second.join("assets/catalog/index.json"), b"catalog")
            .expect("应该可写入第二份资源");
        fs::write(second.join("scripts/modules/main.lua"), b"module")
            .expect("应该可写入第二份脚本");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        let first_fingerprint = root
            .fingerprint_directory_tree(tree_fingerprint_request(
                &first.join("assets"),
                &first.join("scripts"),
            ))
            .await
            .expect("第一份目录树应该可建立指纹");
        let second_fingerprint = root
            .fingerprint_directory_tree(tree_fingerprint_request(
                &second.join("assets"),
                &second.join("scripts"),
            ))
            .await
            .expect("第二份目录树应该可建立指纹");
        assert_eq!(first_fingerprint, second_fingerprint);

        fs::write(second.join("assets/catalog/index.json"), b"changed")
            .expect("应该可修改指纹来源");
        let changed_bytes = root
            .fingerprint_directory_tree(tree_fingerprint_request(
                &second.join("assets"),
                &second.join("scripts"),
            ))
            .await
            .expect("修改后应该仍可建立指纹");
        assert_ne!(changed_bytes, first_fingerprint);

        fs::write(second.join("assets/catalog/index.json"), b"catalog").expect("应该可恢复原文件");
        fs::create_dir(second.join("assets/another-empty")).expect("应该可增加空目录");
        let changed_empty_directory = root
            .fingerprint_directory_tree(tree_fingerprint_request(
                &second.join("assets"),
                &second.join("scripts"),
            ))
            .await
            .expect("增加空目录后应该可建立指纹");
        assert_ne!(changed_empty_directory, first_fingerprint);

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn directory_tree_fingerprint_rejects_hardlinks() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let assets = temporary.path().join("assets");
        let scripts = temporary.path().join("scripts");
        fs::create_dir(&assets).expect("应该可建立资源目录");
        fs::create_dir(&scripts).expect("应该可建立脚本目录");
        fs::write(assets.join("first.json"), b"same physical file").expect("应该可建立原始文件");
        fs::hard_link(assets.join("first.json"), assets.join("second.json"))
            .expect("本地 NTFS 测试目录应该支持硬链接");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        let error = root
            .fingerprint_directory_tree(tree_fingerprint_request(&assets, &scripts))
            .await
            .expect_err("硬链接必须阻止精确目录树指纹");
        assert!(matches!(
            error,
            DirectoryTreeFingerprintError::Failed { source, .. }
                if matches!(
                    *source,
                    SystemFileSystemError::InvalidPath {
                        reason: "目录树包含硬链接文件",
                        ..
                    }
                )
        ));

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[test]
    fn directory_tree_fingerprint_rejects_same_bytes_with_new_identity_between_rounds() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let assets = temporary.path().join("assets");
        let scripts = temporary.path().join("scripts");
        fs::create_dir(&assets).expect("应该可建立资源目录");
        fs::create_dir(&scripts).expect("应该可建立脚本目录");
        let observed = assets.join("catalog.json");
        let replacement = temporary.path().join("replacement.json");
        fs::write(&observed, b"same bytes").expect("应该可建立首个对象");
        fs::write(&replacement, b"same bytes").expect("应该可建立不同身份的替换对象");

        let error = fingerprint_directory_tree_sync_with_between(
            tree_fingerprint_request(&assets, &scripts),
            &file_system_config(),
            || {
                fs::remove_file(&observed).expect("第一轮结束后应该可移除原对象");
                fs::rename(&replacement, &observed).expect("应该可换入相同内容的新身份对象");
            },
        )
        .expect_err("内容相同但物理身份变化仍必须使观察失败");

        assert!(matches!(
            error,
            DirectoryTreeFingerprintError::ChangedDuringObservation { path }
                if path.ends_with("catalog.json")
        ));
    }

    #[test]
    fn fingerprint_file_rechecks_the_identity_captured_during_enumeration() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let observed = temporary.path().join("catalog.json");
        let replacement = temporary.path().join("replacement.json");
        fs::write(&observed, b"same bytes").expect("应该可建立首个对象");
        fs::write(&replacement, b"same bytes").expect("应该可建立替换对象");
        let enumerated = pin_path_without_reparse(&observed).expect("枚举阶段应该可固定原对象");
        let expected_identity =
            FileIdentity::of(enumerated.file(), &observed).expect("应该可读取枚举身份");
        let expected_size = enumerated.metadata().expect("应该可读取枚举大小").len();
        drop(enumerated);
        fs::remove_file(&observed).expect("应该可移除已枚举对象");
        fs::rename(&replacement, &observed).expect("应该可换入同内容的新对象");

        let config = file_system_config();
        let mut pass = FingerprintPass {
            budget: TreeBudgetUsage::default(),
            file_identities: HashSet::new(),
            identities: Vec::new(),
            system_config: &config,
            hasher: Sha256::new(),
        };
        let error = fingerprint_file(
            &observed,
            Path::new("assets/catalog.json"),
            ExpectedFingerprintFile {
                identity: expected_identity,
                size: expected_size,
            },
            2,
            &mut pass,
        )
        .expect_err("目录项在枚举窗口被替换后必须失败");

        assert!(matches!(
            error,
            DirectoryTreeFingerprintError::ChangedDuringObservation { path }
                if path == observed
        ));
    }

    #[tokio::test]
    async fn preparing_a_candidate_rejects_hardlinked_source_files() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可建立来源目录");
        fs::write(source.join("first.json"), b"shared").expect("应该可建立来源文件");
        fs::hard_link(source.join("first.json"), source.join("second.json"))
            .expect("本地 NTFS 测试目录应该支持硬链接");
        let root = TestDirectoryPublisher::new(file_system_config());

        let error = match root
            .prepare(stage_request(
                temporary.path().join("target"),
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
        {
            Ok(_) => panic!("复制来源中的硬链接必须阻止候选准备"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            DirectoryPrepareError::NotPrepared { source, .. }
                if matches!(*source, SystemFileSystemError::InvalidPath {
                    reason: "复制来源包含硬链接文件",
                    ..
                })
        ));
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[test]
    fn copying_a_file_rechecks_the_enumerated_identity_and_size() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source.json");
        let replacement = temporary.path().join("replacement.json");
        let destination = temporary.path().join("candidate.json");
        fs::write(&source, b"same bytes").expect("应该可建立原来源文件");
        fs::write(&replacement, b"same bytes").expect("应该可建立替换来源文件");
        let enumerated = pin_path_without_reparse(&source).expect("枚举阶段应该可固定来源");
        let expected_identity =
            FileIdentity::of(enumerated.file(), &source).expect("应该可读取来源身份");
        let expected_size = enumerated.metadata().expect("应该可读取来源大小").len();
        drop(enumerated);
        fs::remove_file(&source).expect("应该可移除枚举时来源");
        fs::rename(&replacement, &source).expect("应该可换入相同大小和内容的新身份来源");

        let mut budget = TreeBudgetUsage::default();
        let error = copy_regular_file(
            &source,
            &destination,
            expected_identity,
            expected_size,
            &mut budget,
            &tree_budget(),
        )
        .expect_err("来源在枚举与复制之间替换必须失败");
        assert!(matches!(
            error,
            SystemFileSystemError::InvalidPath {
                reason: "复制来源文件在枚举后身份或大小发生变化",
                ..
            }
        ));
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn ordinary_file_capabilities_use_real_unicode_paths_and_enforce_limits() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let directory = temporary.path().join("剧情 数据");
        fs::create_dir(&directory).expect("应该可创建 Unicode 目录");
        let file = directory.join("角色.json");
        fs::write(&file, b"1234").expect("应该可创建 Unicode 文件");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        let resolved = root
            .resolve_existing_directory(directory.clone())
            .await
            .expect("现存目录应该可解析");
        assert!(resolved.is_absolute());
        assert_eq!(
            root.list_directory(directory.clone())
                .await
                .expect("目录应该可列举"),
            vec![DirectoryEntry::new(
                file.canonicalize().expect("文件应该可规范化"),
                DirectoryEntryKind::RegularFile,
            )]
        );
        assert_eq!(
            root.read_file(file.clone())
                .await
                .expect("文件应该可读取")
                .into_bytes(),
            b"1234"
        );

        fs::write(&file, vec![0_u8; 1025]).expect("应该可扩大测试文件");
        assert!(matches!(
            root.read_file(file).await,
            Err(ReadFileError::Io {
                source: SystemFileSystemError::ResourceLimit { .. },
                ..
            })
        ));
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn directory_listing_reports_entry_kinds_and_rejects_hardlinked_files() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let directory = temporary.path().join("source");
        let nested = directory.join("nested");
        fs::create_dir_all(&nested).expect("应该可创建测试目录");
        let ordinary = directory.join("state.db");
        fs::write(&ordinary, b"state").expect("应该可创建普通文件");
        let root = SystemFileSystem::new(file_system_config()).expect("应该可建立文件系统根");

        let mut entries = root
            .list_directory(directory.clone())
            .await
            .expect("目录列举应该返回受信种类");
        entries.sort_by(|left, right| left.resolved_path().cmp(right.resolved_path()));
        let mut expected = vec![
            DirectoryEntry::new(
                nested.canonicalize().expect("目录应该可规范化"),
                DirectoryEntryKind::Directory,
            ),
            DirectoryEntry::new(
                ordinary.canonicalize().expect("文件应该可规范化"),
                DirectoryEntryKind::RegularFile,
            ),
        ];
        expected.sort_by(|left, right| left.resolved_path().cmp(right.resolved_path()));
        assert_eq!(entries, expected);

        let hardlink = directory.join("state-copy.db");
        fs::hard_link(&ordinary, &hardlink).expect("应该可创建硬链接测试入口");
        assert!(matches!(
            root.list_directory(directory).await,
            Err(ListDirectoryError::Io {
                source: SystemFileSystemError::InvalidPath {
                    reason: "目录列举拒绝硬链接文件",
                    ..
                },
                ..
            })
        ));

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn scoped_editor_mutates_only_declared_roots_and_the_publisher_revalidates_the_result() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        scoped_source(&source);
        let target = temporary.path().join("output");
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(scoped_stage_request(
                target.clone(),
                &source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");
        let scope = root
            .bind_scoped_directory(&staged, scoped_scope())
            .await
            .expect("应该可绑定未发布候选");

        assert_eq!(
            root.read_scoped_file(&scope, scoped_path("assets/catalog.json"))
                .await
                .expect("应该可读取候选文件"),
            b"items"
        );
        assert!(matches!(
            root.read_scoped_file(&scope, scoped_path("other/file.txt"))
                .await,
            Err(ScopedDirectoryEditError::OutsideScope { .. })
        ));
        assert_eq!(
            root.list_scoped_directory(&scope, scoped_path("assets"))
                .await
                .expect("应该可列举资源目录"),
            vec![ScopedDirectoryEntry::new(
                OsString::from("catalog.json"),
                ScopedDirectoryEntryKind::File,
            )]
        );
        root.create_scoped_directory(&scope, scoped_path("assets/generated/nested"))
            .await
            .expect("应该可逐段建立候选目录");
        root.write_scoped_file(
            &scope,
            scoped_path("assets/generated/nested/result.json"),
            b"generated".to_vec(),
        )
        .await
        .expect("应该可建立候选文件");
        root.write_scoped_file(&scope, scoped_path("scripts/main.lua"), b"changed".to_vec())
            .await
            .expect("应该可替换候选文件");
        root.remove_scoped_path(&scope, scoped_path("assets/catalog.json"))
            .await
            .expect("应该可删除候选文件");

        for operation in [
            root.create_scoped_directory(&scope, scoped_path("assets"))
                .await,
            root.write_scoped_file(&scope, scoped_path("assets"), Vec::new())
                .await,
            root.remove_scoped_path(&scope, scoped_path("assets")).await,
        ] {
            assert!(matches!(
                operation,
                Err(ScopedDirectoryEditError::ScopeRootMutation { .. })
            ));
        }

        fn assert_send<T: Send>(_: T) {}
        assert_send(root.read_scoped_file(&scope, scoped_path("scripts/main.lua")));
        assert_send(root.list_scoped_directory(&scope, scoped_path("scripts")));
        root.validate_scoped_directory(&scope)
            .await
            .expect("编辑后候选应仍满足完整写回结构");
        drop(scope);

        root.publish(staged)
            .await
            .expect("发布根应该重新验证并整体发布编辑后候选");
        assert!(!target.join("assets/catalog.json").exists());
        assert_eq!(
            fs::read(target.join("assets/generated/nested/result.json"))
                .expect("应该可读取已发布新文件"),
            b"generated"
        );
        assert_eq!(
            fs::read(target.join("scripts/main.lua")).expect("应该可读取已发布替换文件"),
            b"changed"
        );
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn scoped_root_validation_is_generic_and_reports_all_top_level_entries() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        scoped_source(&source);
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(scoped_stage_request(
                temporary.path().join("output"),
                &source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");
        let candidate_root = staged.staging_root().to_path_buf();
        let scope = root
            .bind_scoped_directory(&staged, scoped_scope())
            .await
            .expect("应该可绑定候选");

        fs::create_dir(candidate_root.join("evil")).expect("测试应可模拟可信 Lua 绕过门面写入");
        root.validate_scoped_directory(&scope)
            .await
            .expect("通用文件根不应猜测调用方的顶层目录语义");
        assert_eq!(
            root.list_scoped_root(&scope)
                .await
                .expect("应该可列举候选根")
                .into_iter()
                .map(|entry| entry.name().to_os_string())
                .collect::<Vec<_>>(),
            vec![
                OsString::from("assets"),
                OsString::from("evil"),
                OsString::from("scripts"),
            ]
        );
        fs::remove_dir(candidate_root.join("evil")).expect("应该可清理测试目录");
        root.validate_scoped_directory(&scope)
            .await
            .expect("清理后候选应重新合法");

        drop(scope);
        root.discard(staged).await.expect("应该可丢弃候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn scoped_editor_accepts_arbitrary_declared_top_level_directories() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("assets")).expect("应该可建立 assets 来源");
        fs::create_dir_all(source.join("scripts")).expect("应该可建立 scripts 来源");
        fs::write(source.join("assets/title.png"), b"image").expect("应该可建立资源文件");
        let request = DirectoryStageRequest::new(
            temporary.path().join("output"),
            DirectoryPublishIntent::CreateNew,
            vec![
                DirectorySourceMapping::new(source.join("assets"), PathBuf::from("assets"))
                    .expect("assets 映射应该合法"),
                DirectorySourceMapping::new(source.join("scripts"), PathBuf::from("scripts"))
                    .expect("scripts 映射应该合法"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect("通用候选请求应该合法");
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root.prepare(request).await.expect("候选应该可准备");
        let scope = root
            .bind_scoped_directory(
                &staged,
                ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
                    .expect("通用范围应该合法"),
            )
            .await
            .expect("通用顶层目录应该可绑定");

        assert_eq!(
            root.read_scoped_file(&scope, scoped_path("assets/title.png"))
                .await
                .expect("应该可读取声明范围内文件"),
            b"image"
        );
        assert!(matches!(
            root.read_scoped_file(&scope, scoped_path("private/catalog.json"))
                .await,
            Err(ScopedDirectoryEditError::OutsideScope { .. })
        ));

        drop(scope);
        root.discard(staged).await.expect("应该可丢弃候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn scoped_tokens_are_bound_to_the_creating_file_system_instance() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        scoped_source(&source);
        let owner = TestDirectoryPublisher::new(file_system_config());
        let foreign = TestDirectoryPublisher::new(file_system_config());
        let staged = owner
            .prepare(scoped_stage_request(
                temporary.path().join("output"),
                &source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");

        assert!(matches!(
            foreign.bind_scoped_directory(&staged, scoped_scope()).await,
            Err(ScopedDirectoryBindError::WrongEditorInstance)
        ));
        let scope = owner
            .bind_scoped_directory(&staged, scoped_scope())
            .await
            .expect("所有者应该可绑定候选");
        assert!(matches!(
            foreign
                .read_scoped_file(&scope, scoped_path("assets/catalog.json"))
                .await,
            Err(ScopedDirectoryEditError::WrongEditorInstance)
        ));
        drop(scope);
        owner.discard(staged).await.expect("所有者应该可丢弃候选");
        foreign.shutdown().await.expect("外来根应该可终结");
        owner.shutdown().await.expect("所有者根应该可终结");
    }

    #[tokio::test]
    async fn scoped_editor_rejects_hardlinks_and_reparse_points() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        scoped_source(&source);
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(scoped_stage_request(
                temporary.path().join("output"),
                &source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");
        let staging_root = staged.staging_root().to_path_buf();
        let scope = root
            .bind_scoped_directory(&staged, scoped_scope())
            .await
            .expect("应该可绑定候选");

        let hardlink = staging_root.join("assets/hardlink.json");
        fs::hard_link(staging_root.join("assets/catalog.json"), &hardlink)
            .expect("本地 NTFS 测试目录应支持硬链接");
        assert!(matches!(
            root.read_scoped_file(&scope, scoped_path("assets/hardlink.json"))
                .await,
            Err(ScopedDirectoryEditError::Failed { source, .. })
                if matches!(*source, SystemFileSystemError::InvalidPath { reason, .. }
                    if reason.contains("硬链接"))
        ));
        fs::remove_file(&hardlink).expect("应该可移除硬链接测试入口");

        let external = temporary.path().join("external.txt");
        let link = staging_root.join("assets/linked.txt");
        fs::write(&external, b"external").expect("应该可建立外部文件");
        match std::os::windows::fs::symlink_file(&external, &link) {
            Ok(()) => {
                assert!(matches!(
                    root.read_scoped_file(&scope, scoped_path("assets/linked.txt"))
                        .await,
                    Err(ScopedDirectoryEditError::Failed { source, .. })
                        if matches!(*source, SystemFileSystemError::Windows(
                            WindowsFsError::ReparsePoint { .. }
                        ))
                ));
                fs::remove_file(&link).expect("应该可移除 reparse 测试入口");
            }
            Err(error) if symlink_unavailable(&error) => {}
            Err(error) => panic!("应该可创建文件符号链接：{error}"),
        }

        drop(scope);
        root.discard(staged).await.expect("应该可丢弃候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn scoped_write_rolls_back_when_the_complete_tree_exceeds_its_budget() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("assets")).expect("应该可建立资源来源");
        fs::create_dir_all(source.join("scripts")).expect("应该可建立脚本来源");
        fs::write(source.join("assets/a.json"), b"1234").expect("应该可写入资源来源");
        fs::write(source.join("scripts/a.lua"), b"12").expect("应该可写入脚本来源");
        let constrained = SystemFileSystemConfig::new(
            2,
            8,
            1024,
            16,
            TreeBudget::new(16, 8, 6, 6).expect("受限目录树预算应该合法"),
            exclusive_file_lease_config(),
        )
        .expect("受限文件系统配置应该合法");
        let root = TestDirectoryPublisher::new(constrained);
        let staged = root
            .prepare(scoped_stage_request(
                temporary.path().join("output"),
                &source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("恰好用尽字节预算的候选应可准备");
        let candidate_root = staged.staging_root().to_path_buf();
        let scope = root
            .bind_scoped_directory(&staged, scoped_scope())
            .await
            .expect("应该可绑定候选");

        assert!(matches!(
            root.write_scoped_file(&scope, scoped_path("assets/overflow.json"), vec![1])
                .await,
            Err(ScopedDirectoryEditError::Failed { source, .. })
                if matches!(*source, SystemFileSystemError::ResourceLimit {
                    resource: "目录树总字节数",
                    ..
                })
        ));
        assert!(
            !candidate_root.join("assets/overflow.json").exists(),
            "超出预算的新文件必须回滚"
        );

        drop(scope);
        root.discard(staged).await.expect("应该可丢弃候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn create_new_and_replace_existing_publish_complete_directory_snapshots() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source-a");
        fs::create_dir(&source).expect("应该可创建来源目录");
        fs::write(source.join("value.txt"), b"first").expect("应该可写入来源文件");
        let target = temporary.path().join("output");
        let root = TestDirectoryPublisher::new(file_system_config());

        let staged = root
            .prepare(stage_request(
                target.clone(),
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("首次候选应该可准备");
        fs::write(staged.staging_root().join("prepared.txt"), b"prepared")
            .expect("非根服务应该可在候选内建立文件");
        let database = rusqlite::Connection::open(staged.staging_root().join("state.db"))
            .expect("应该可在候选内建立真实 SQLite 数据库");
        database
            .execute_batch(
                "CREATE TABLE metadata (value TEXT NOT NULL); INSERT INTO metadata VALUES ('ok');",
            )
            .expect("应该可初始化候选数据库");
        drop(database);
        root.publish(staged).await.expect("首次候选应该可发布");
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).unwrap(),
            b"first"
        );
        assert_eq!(fs::read(target.join("prepared.txt")).unwrap(), b"prepared");
        assert!(target.join("empty").is_dir());

        let replacement = temporary.path().join("source-b");
        fs::create_dir(&replacement).expect("应该可创建替换来源");
        fs::write(replacement.join("value.txt"), b"second").expect("应该可写入替换来源");
        let staged = root
            .prepare(stage_request(
                target.clone(),
                replacement,
                DirectoryPublishIntent::ReplaceExisting,
            ))
            .await
            .expect("替换候选应该可准备");
        root.publish(staged).await.expect("替换候选应该可发布");
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).unwrap(),
            b"second"
        );
        assert!(!target.join("prepared.txt").exists());

        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn publish_revalidates_files_added_after_prepare_against_candidate_budgets() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        fs::write(source.join("value.txt"), b"small").expect("应该可写入来源文件");
        let target = temporary.path().join("target");
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(stage_request(
                target.clone(),
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应可准备");
        let staging_root = staged.staging_root().to_path_buf();
        fs::write(
            staging_root.join("oversized.db"),
            vec![0_u8; 512 * 1024 + 1],
        )
        .expect("非根服务应可在候选中新增文件");

        assert!(matches!(
            root.publish(staged).await,
            Err(DirectoryPublishError::NotAttempted { source, .. })
                if matches!(*source, SystemFileSystemError::ResourceLimit {
                    resource: "目录候选单文件字节数",
                    ..
                })
        ));
        assert!(!target.exists());
        assert!(!staging_root.exists());
        root.shutdown().await.expect("文件系统根应可终结");
    }

    #[tokio::test]
    async fn publish_revalidation_rejects_hardlinks_added_after_prepare() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        fs::write(source.join("value.txt"), b"value").expect("应该可写入来源文件");
        let target = temporary.path().join("target");
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(stage_request(
                target.clone(),
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应可准备");
        let staging_root = staged.staging_root().to_path_buf();
        fs::hard_link(
            staging_root.join("snapshot/content/value.txt"),
            staging_root.join("same-file.txt"),
        )
        .expect("测试卷应支持硬链接");

        assert!(matches!(
            root.publish(staged).await,
            Err(DirectoryPublishError::NotAttempted { source, .. })
                if matches!(*source, SystemFileSystemError::InvalidPath { reason, .. }
                    if reason.contains("硬链接"))
        ));
        assert!(!target.exists());
        assert!(!staging_root.exists());
        root.shutdown().await.expect("文件系统根应可终结");
    }

    #[tokio::test]
    async fn concurrent_create_new_has_exactly_one_winner() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let first_source = temporary.path().join("first");
        let second_source = temporary.path().join("second");
        fs::create_dir(&first_source).expect("应该可创建第一来源");
        fs::create_dir(&second_source).expect("应该可创建第二来源");
        fs::write(first_source.join("winner.txt"), b"first").unwrap();
        fs::write(second_source.join("winner.txt"), b"second").unwrap();
        let target = temporary.path().join("destination");
        let root = TestDirectoryPublisher::new(file_system_config());

        let publish_one = |source: PathBuf| {
            let root = root.clone();
            let target = target.clone();
            async move {
                let staged = root
                    .prepare(stage_request(
                        target,
                        source,
                        DirectoryPublishIntent::CreateNew,
                    ))
                    .await?;
                Ok::<_, DirectoryPrepareError<Box<SystemFileSystemError>>>(
                    root.publish(staged).await,
                )
            }
        };
        let (first, second) = tokio::join!(publish_one(first_source), publish_one(second_source));
        let outcomes = [first, second];
        let successes = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(Ok(()))))
            .count();
        let already_exists = outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Ok(Err(DirectoryPublishError::TargetAlreadyExists { .. }))
                )
            })
            .count();
        assert_eq!(successes, 1);
        assert_eq!(already_exists, 1);
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[tokio::test]
    async fn a_staged_token_cannot_be_finalized_by_another_root_instance() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        let target = temporary.path().join("target");
        let owner = TestDirectoryPublisher::new(file_system_config());
        let foreign = TestDirectoryPublisher::new(file_system_config());
        let staged = owner
            .prepare(stage_request(
                target,
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");
        let staging_root = staged.staging_root().to_path_buf();

        assert!(matches!(
            foreign.publish(staged).await,
            Err(DirectoryPublishError::NotAttempted { source, .. })
                if matches!(*source, SystemFileSystemError::WrongPublisherInstance)
        ));
        assert!(!staging_root.exists());
        owner.shutdown().await.expect("所有者根应该可终结");
        foreign.shutdown().await.expect("外来根应该可终结");
    }

    #[test]
    fn identity_fixed_cleanup_removes_the_complete_expected_directory_tree() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let root = temporary.path().join("candidate");
        fs::create_dir_all(root.join("嵌套/更深")).expect("应该可创建候选目录树");
        fs::write(root.join("root.txt"), b"root").expect("应该可创建根文件");
        fs::write(root.join("嵌套/child.txt"), b"child").expect("应该可创建子文件");
        fs::write(root.join("嵌套/更深/leaf.bin"), b"leaf").expect("应该可创建叶子文件");
        let handle = open_directory(&root, true).expect("应该可打开候选根");
        let identity = FileIdentity::of(&handle, &root).expect("应该可读取候选根身份");
        drop(handle);

        remove_directory_tree_if_identity(&root, identity)
            .expect("身份匹配的整棵候选目录树应该被精确删除");

        assert!(!root.exists());
    }

    #[test]
    fn identity_fixed_cleanup_refuses_and_preserves_a_foreign_root_replacement() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let root = temporary.path().join("candidate");
        let displaced = temporary.path().join("displaced-candidate");
        fs::create_dir(&root).expect("应该可创建原候选根");
        fs::write(root.join("owned.txt"), b"owned").expect("应该可创建候选文件");
        let handle = open_directory(&root, true).expect("应该可打开原候选根");
        let original_identity = FileIdentity::of(&handle, &root).expect("应该可读取原候选根身份");
        drop(handle);

        fs::rename(&root, &displaced).expect("应该可移开原候选根");
        fs::create_dir(&root).expect("应该可在同路径创建外来目录");
        fs::write(root.join("foreign.txt"), b"foreign").expect("应该可创建外来文件");

        assert!(matches!(
            remove_directory_tree_if_identity(&root, original_identity),
            Err(SystemFileSystemError::InvalidStagedIdentity { path }) if path == root
        ));
        assert_eq!(
            fs::read(root.join("foreign.txt")).expect("外来文件应该被保留"),
            b"foreign"
        );
        assert!(displaced.join("owned.txt").is_file());
    }

    #[test]
    fn identity_fixed_cleanup_rejects_a_reparse_child_without_following_it() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let root = temporary.path().join("candidate");
        let external = temporary.path().join("external.txt");
        let link = root.join("linked.txt");
        fs::create_dir(&root).expect("应该可创建候选根");
        fs::write(&external, b"must-stay").expect("应该可创建外部目标");
        if let Err(error) = std::os::windows::fs::symlink_file(&external, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建文件符号链接：{error}");
        }
        let handle = open_directory(&root, true).expect("应该可打开候选根");
        let identity = FileIdentity::of(&handle, &root).expect("应该可读取候选根身份");
        drop(handle);

        assert!(matches!(
            remove_directory_tree_if_identity(&root, identity),
            Err(SystemFileSystemError::Windows(WindowsFsError::ReparsePoint { path }))
                if path == link
        ));
        assert_eq!(
            fs::read(&external).expect("外部目标应该仍可读取"),
            b"must-stay"
        );
        assert!(root.exists());
    }

    #[tokio::test]
    async fn directly_dropping_a_staged_token_panics_and_still_cleans_the_candidate() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        fs::write(source.join("value.txt"), b"value").expect("应该可创建来源文件");
        let target = temporary.path().join("target");
        let root = TestDirectoryPublisher::new(file_system_config());
        let staged = root
            .prepare(stage_request(
                target,
                source,
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("候选应该可准备");
        let staging_root = staged.staging_root().to_path_buf();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(staged)));

        assert!(panic.is_err(), "直接丢弃 token 必须显式违反内部契约");
        assert!(!staging_root.exists(), "panic 展开时仍必须精确清理候选");
        root.shutdown().await.expect("文件系统根应该可终结");
    }

    #[test]
    fn a_complete_corrupt_journal_frame_is_rejected() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let path = temporary.path().join("corrupt.journal");
        let payload = b"{}";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        fs::write(&path, bytes).expect("应该可写入完整损坏帧");

        assert!(matches!(
            read_journal(&path),
            Err(SystemFileSystemError::JournalCorrupt { reason, .. })
                if reason.contains("CRC")
        ));
    }

    #[tokio::test]
    async fn target_lock_timeout_is_a_precise_prepare_failure() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        let target = temporary.path().join("target");
        let lock_directory = temporary.path().join("locks");
        let owner = TestDirectoryPublisher::with_publisher_config(
            file_system_config(),
            publisher_config_for_lock_directory(lock_directory.clone()),
        );
        let staged = owner
            .prepare(stage_request(
                target.clone(),
                source.clone(),
                DirectoryPublishIntent::CreateNew,
            ))
            .await
            .expect("所有者应可持有目标锁");
        let zero_timeout = DirectoryPublisherConfig::new(lock_directory, 8, Duration::ZERO)
            .expect("零等待只表示立即尝试");
        let contender = TestDirectoryPublisher::with_publisher_config(
            SystemFileSystemConfig::new(
                1,
                2,
                1024,
                16,
                tree_budget(),
                exclusive_file_lease_config(),
            )
            .expect("竞争根配置应合法"),
            zero_timeout,
        );

        assert!(matches!(
            contender
                .prepare(stage_request(
                    target,
                    source,
                    DirectoryPublishIntent::CreateNew,
                ))
                .await,
            Err(DirectoryPrepareError::NotPrepared { source, .. })
                if matches!(*source, SystemFileSystemError::Windows(
                    WindowsFsError::LockTimeout { .. }
                ))
        ));
        owner.discard(staged).await.expect("应该可丢弃所有者候选");
        owner.shutdown().await.expect("所有者根应可终结");
        contender.shutdown().await.expect("竞争根应可终结");
    }

    #[tokio::test]
    async fn replace_faults_preserve_precise_terminal_states_and_recover() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源目录");
        fs::write(source.join("value.txt"), b"new").expect("应该可写入新内容");
        let target = temporary.path().join("target");
        fs::create_dir_all(target.join("snapshot/content")).expect("应该可创建旧目标");
        fs::write(target.join("snapshot/content/value.txt"), b"old").expect("应该可写入旧内容");
        let root = TestDirectoryPublisher::new(file_system_config());
        let trusted_target = canonical_target(&target);

        let staged = root
            .prepare(stage_request(
                target.clone(),
                source.clone(),
                DirectoryPublishIntent::ReplaceExisting,
            ))
            .await
            .expect("替换候选应可准备");
        register_test_publish_faults(
            trusted_target.clone(),
            [(
                TestPublishFaultPoint::BeforeCandidateMove,
                TestPublishFaultAction::Error,
            )],
        );
        assert!(matches!(
            root.publish(staged).await,
            Err(DirectoryPublishError::NotPublished { .. })
        ));
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).expect("旧目标应已恢复"),
            b"old"
        );

        let staged = root
            .prepare(stage_request(
                target.clone(),
                source.clone(),
                DirectoryPublishIntent::ReplaceExisting,
            ))
            .await
            .expect("第二个替换候选应可准备");
        register_test_publish_faults(
            trusted_target,
            [(
                TestPublishFaultPoint::BeforeBackupCleanup,
                TestPublishFaultAction::Error,
            )],
        );
        assert!(matches!(
            root.publish(staged).await,
            Err(DirectoryPublishError::PublishedWithResiduals { .. })
        ));
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).expect("新目标应已可见"),
            b"new"
        );

        let staged = root
            .prepare(stage_request(
                target.clone(),
                source,
                DirectoryPublishIntent::ReplaceExisting,
            ))
            .await
            .expect("下次操作应先完成恢复");
        root.discard(staged).await.expect("恢复后候选应可丢弃");
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).expect("恢复不应回退已发布目标"),
            b"new"
        );
        root.shutdown().await.expect("文件系统根应可终结");
    }

    #[test]
    fn publisher_subprocess_entrypoint() {
        let Some(mode) = std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_MODE") else {
            return;
        };
        let target = PathBuf::from(
            std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_TARGET").expect("子进程应提供目标路径"),
        );
        let source = PathBuf::from(
            std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_SOURCE").expect("子进程应提供来源路径"),
        );
        let mode = mode.to_string_lossy();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("应该可建立子进程运行时");
        runtime.block_on(async move {
            let root = TestDirectoryPublisher::new(file_system_config());
            let intent = if mode == "create" {
                DirectoryPublishIntent::CreateNew
            } else {
                DirectoryPublishIntent::ReplaceExisting
            };
            let staged = root
                .prepare(stage_request(target.clone(), source, intent))
                .await
                .expect("子进程应可准备候选");
            if let Some(point) = mode.strip_prefix("abort:") {
                let point = match point {
                    "original-journal" => TestPublishFaultPoint::AfterOriginalJournal,
                    "original-move" => TestPublishFaultPoint::AfterOriginalMove,
                    "candidate-intent" => TestPublishFaultPoint::AfterCandidateIntent,
                    "candidate-move" => TestPublishFaultPoint::AfterCandidateMove,
                    "candidate-visible" => TestPublishFaultPoint::AfterCandidateVisible,
                    _ => panic!("未知子进程故障点：{point}"),
                };
                register_test_publish_faults(
                    canonical_target(&target),
                    [(point, TestPublishFaultAction::Abort)],
                );
            }
            let result = root.publish(staged).await;
            if mode == "create" {
                let result_path = PathBuf::from(
                    std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_RESULT")
                        .expect("新建子进程应提供结果路径"),
                );
                let outcome = match result {
                    Ok(()) => "success",
                    Err(DirectoryPublishError::TargetAlreadyExists { .. }) => "already-exists",
                    Err(error) => panic!("子进程发布结果不可归类：{error}"),
                };
                fs::write(result_path, outcome).expect("应该可写入子进程结果");
                root.shutdown().await.expect("子进程根应可终结");
            } else {
                panic!("故障子进程应在 publish 内直接 abort：{result:?}");
            }
        });
    }

    #[test]
    fn two_processes_create_new_with_exactly_one_winner() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let target = temporary.path().join("target");
        let mut children = Vec::new();
        let mut results = Vec::new();
        for index in 0..2 {
            let source = temporary.path().join(format!("source-{index}"));
            fs::create_dir(&source).expect("应该可创建子进程来源");
            fs::write(source.join("value.txt"), index.to_string()).expect("应该可写入来源");
            let result = temporary.path().join(format!("result-{index}"));
            let mut command = subprocess_command("create", &target, &source);
            command.env("FILESYSTEM_PUBLISHER_CHILD_RESULT", &result);
            children.push(command.spawn().expect("应该可启动发布子进程"));
            results.push(result);
        }
        for mut child in children {
            assert!(child.wait().expect("应该可等待发布子进程").success());
        }
        let outcomes = results
            .iter()
            .map(|path| fs::read_to_string(path).expect("应该可读取子进程结果"))
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes.iter().filter(|value| *value == "success").count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| *value == "already-exists")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn process_abort_states_are_recovered_idempotently() {
        for (phase, expected) in [
            ("original-journal", b"old".as_slice()),
            ("original-move", b"old".as_slice()),
            ("candidate-intent", b"old".as_slice()),
            ("candidate-move", b"new".as_slice()),
            ("candidate-visible", b"new".as_slice()),
        ] {
            let temporary = tempfile::tempdir().expect("应该可创建临时目录");
            let source = temporary.path().join("source");
            fs::create_dir(&source).expect("应该可创建来源");
            fs::write(source.join("value.txt"), b"new").expect("应该可写入新内容");
            let target = temporary.path().join("target");
            fs::create_dir_all(target.join("snapshot/content")).expect("应该可创建旧目标");
            fs::write(target.join("snapshot/content/value.txt"), b"old").expect("应该可写入旧内容");
            let status = subprocess_command(&format!("abort:{phase}"), &target, &source)
                .status()
                .expect("应该可等待故障子进程");
            assert!(!status.success(), "故障点 {phase} 必须终止子进程");

            let root = TestDirectoryPublisher::new(file_system_config());
            for _ in 0..2 {
                let staged = root
                    .prepare(stage_request(
                        target.clone(),
                        source.clone(),
                        DirectoryPublishIntent::ReplaceExisting,
                    ))
                    .await
                    .expect("恢复应幂等且允许继续准备");
                root.discard(staged).await.expect("恢复后候选应可丢弃");
            }
            assert_eq!(
                fs::read(target.join("snapshot/content/value.txt")).expect("应该可读取恢复目标"),
                expected,
                "故障点 {phase} 恢复了错误一侧"
            );
            root.shutdown().await.expect("恢复根应可终结");
        }
    }
}
