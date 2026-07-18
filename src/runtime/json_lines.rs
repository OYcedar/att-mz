//! 通用强类型 JSON Lines 持久事件流。
//!
//! 每个流拥有有界队列、OS worker、跨进程文件锁和轮转序列。worker 接管
//! 事件后，即使调用 Future 被丢弃，也会继续把该事件推进到明确的持久化终态。

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(test)]
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use async_channel::{Receiver, Sender};
use time::OffsetDateTime;
use tokio::sync::oneshot;

use super::windows::{
    ExclusiveFileLock, FileIdentity, PinnedPath, WindowsFsError,
    create_directories_without_reparse, delete_regular_file_if_identity,
    open_read_write_file_without_reparse, pin_path_without_reparse, rename_without_replace,
};

/// 一条 JSONL 流的全部资源和维护边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JsonLinesStreamConfig {
    queue_capacity: usize,
    lock_timeout: Duration,
    max_record_bytes: usize,
    max_file_bytes: u64,
    retained_rotated_files: usize,
}

impl JsonLinesStreamConfig {
    pub(crate) fn new(
        queue_capacity: usize,
        lock_timeout: Duration,
        max_record_bytes: usize,
        max_file_bytes: u64,
        retained_rotated_files: usize,
    ) -> Result<Self, JsonLinesConfigurationError> {
        if queue_capacity == 0 {
            return Err(JsonLinesConfigurationError::ZeroQueueCapacity);
        }
        if lock_timeout.is_zero() {
            return Err(JsonLinesConfigurationError::ZeroLockTimeout);
        }
        if max_record_bytes == 0 {
            return Err(JsonLinesConfigurationError::ZeroMaxRecordBytes);
        }
        if max_file_bytes == 0 {
            return Err(JsonLinesConfigurationError::ZeroMaxFileBytes);
        }
        let record_limit = u64::try_from(max_record_bytes)
            .map_err(|_| JsonLinesConfigurationError::RecordLimitDoesNotFitU64)?;
        if record_limit > max_file_bytes {
            return Err(JsonLinesConfigurationError::RecordExceedsFileLimit {
                max_record_bytes,
                max_file_bytes,
            });
        }
        Ok(Self {
            queue_capacity,
            lock_timeout,
            max_record_bytes,
            max_file_bytes,
            retained_rotated_files,
        })
    }
}

/// JSONL 流配置无法建立受信资源边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JsonLinesConfigurationError {
    ZeroQueueCapacity,
    ZeroLockTimeout,
    ZeroMaxRecordBytes,
    ZeroMaxFileBytes,
    RecordLimitDoesNotFitU64,
    RecordExceedsFileLimit {
        max_record_bytes: usize,
        max_file_bytes: u64,
    },
}

impl fmt::Display for JsonLinesConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroQueueCapacity => formatter.write_str("JSONL 队列容量必须大于零"),
            Self::ZeroLockTimeout => formatter.write_str("JSONL 文件锁等待上限必须大于零"),
            Self::ZeroMaxRecordBytes => formatter.write_str("JSONL 单条记录上限必须大于零"),
            Self::ZeroMaxFileBytes => formatter.write_str("JSONL 活动文件上限必须大于零"),
            Self::RecordLimitDoesNotFitU64 => {
                formatter.write_str("JSONL 单条记录上限无法表示为文件长度")
            }
            Self::RecordExceedsFileLimit {
                max_record_bytes,
                max_file_bytes,
            } => write!(
                formatter,
                "JSONL 单条记录上限 {max_record_bytes} 大于活动文件上限 {max_file_bytes}"
            ),
        }
    }
}

impl Error for JsonLinesConfigurationError {}

/// 日志根启动失败，尚未接纳任何事件。
#[derive(Debug)]
pub(crate) enum JsonLinesStartError {
    CreateRoot { path: PathBuf, source: io::Error },
    InvalidRoot(WindowsFsError),
    SpawnWorker(io::Error),
}

impl fmt::Display for JsonLinesStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateRoot { path, source } => {
                write!(
                    formatter,
                    "无法创建 JSONL 日志根 {}：{source}",
                    path.display()
                )
            }
            Self::InvalidRoot(source) => write!(formatter, "JSONL 日志根不满足要求：{source}"),
            Self::SpawnWorker(source) => write!(formatter, "无法启动 JSONL worker：{source}"),
        }
    }
}

impl Error for JsonLinesStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateRoot { source, .. } | Self::SpawnWorker(source) => Some(source),
            Self::InvalidRoot(source) => Some(source),
        }
    }
}

/// 一条事件追加后的准确持久化终态。
#[derive(Debug)]
pub(crate) enum JsonLinesAppendError {
    /// 当前记录没有进入活动文件，既有完整记录仍可继续信任。
    NotPersisted {
        path: PathBuf,
        stage: &'static str,
        message: String,
    },
    /// 已经开始写入或刷盘，无法确认当前记录是否完整持久化。
    OutcomeUnknown {
        path: PathBuf,
        stage: &'static str,
        message: String,
    },
    /// 当前记录已通过 `sync_data`，但轮转保留清理没有完全结束。
    PersistedButMaintenanceFailed {
        path: PathBuf,
        residual: PathBuf,
        message: String,
    },
}

impl JsonLinesAppendError {
    fn not_persisted(path: &Path, stage: &'static str, error: impl fmt::Display) -> Self {
        Self::NotPersisted {
            path: path.to_path_buf(),
            stage,
            message: error.to_string(),
        }
    }

    fn outcome_unknown(path: &Path, stage: &'static str, error: impl fmt::Display) -> Self {
        Self::OutcomeUnknown {
            path: path.to_path_buf(),
            stage,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for JsonLinesAppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPersisted {
                path,
                stage,
                message,
            } => write!(
                formatter,
                "JSONL 记录未持久化（{stage}，{}）：{message}",
                path.display()
            ),
            Self::OutcomeUnknown {
                path,
                stage,
                message,
            } => write!(
                formatter,
                "JSONL 记录持久化结果未知（{stage}，{}）：{message}",
                path.display()
            ),
            Self::PersistedButMaintenanceFailed {
                path,
                residual,
                message,
            } => write!(
                formatter,
                "JSONL 记录已持久化到 {}，但维护残留 {}：{message}",
                path.display(),
                residual.display()
            ),
        }
    }
}

impl Error for JsonLinesAppendError {}

/// 日志 worker 无法完成显式排空和终结。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JsonLinesShutdownError {
    WorkerCompletionLost,
    WorkerPanicked,
}

impl fmt::Display for JsonLinesShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerCompletionLost => formatter.write_str("JSONL worker 未交还终结报告"),
            Self::WorkerPanicked => formatter.write_str("JSONL worker 发生 panic"),
        }
    }
}

impl Error for JsonLinesShutdownError {}

/// 由领域边界定义的强类型 JSONL 记录。
///
/// Runtime 只负责为记录注入确认时刻、验证已经存在的完整行并持久化字节；
/// 它不解释任何领域事件。
pub(crate) trait JsonLineRecord: Send + 'static {
    fn serialize(self, recorded_at_utc: String) -> Result<Vec<u8>, String>;

    fn validate(bytes: &[u8]) -> Result<(), String>;
}

struct AppendJob<R> {
    record: R,
    acknowledgement: oneshot::Sender<Result<(), JsonLinesAppendError>>,
}

struct StreamPaths {
    // worker 存活期内持有整条无删除共享的目录句柄链，防止已验证的
    // 日志根在路径级枚举期间被替换或改为 reparse point。
    _pinned_root: PinnedPath,
    root: PathBuf,
    stem: &'static str,
    active: PathBuf,
    lock: PathBuf,
}

impl StreamPaths {
    fn new(root: PathBuf, stem: &'static str, pinned_root: PinnedPath) -> Self {
        Self {
            active: root.join(format!("{stem}.jsonl")),
            lock: root.join(format!(".{stem}.lock")),
            _pinned_root: pinned_root,
            root,
            stem,
        }
    }

    fn rotated(&self, sequence: u64) -> PathBuf {
        self.root
            .join(format!("{}.{sequence:020}.jsonl", self.stem))
    }
}

#[derive(Debug)]
pub(crate) struct JsonLinesEventLog<R> {
    sender: Sender<AppendJob<R>>,
    active_path: Arc<PathBuf>,
}

impl<R> Clone for JsonLinesEventLog<R> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            active_path: Arc::clone(&self.active_path),
        }
    }
}

impl<R> JsonLinesEventLog<R>
where
    R: JsonLineRecord,
{
    pub(crate) async fn append(&self, record: R) -> Result<(), JsonLinesAppendError> {
        let (acknowledgement, response) = oneshot::channel();
        self.sender
            .send(AppendJob {
                record,
                acknowledgement,
            })
            .await
            .map_err(|_| {
                JsonLinesAppendError::not_persisted(
                    &self.active_path,
                    "queue",
                    "日志流已经停止接纳事件",
                )
            })?;

        response.await.map_err(|_| {
            JsonLinesAppendError::outcome_unknown(
                &self.active_path,
                "worker",
                "worker 接管事件后未返回终态",
            )
        })?
    }
}

trait ChannelCloser: Send {
    fn close(&self);
}

impl<R> ChannelCloser for Sender<AppendJob<R>>
where
    R: Send + 'static,
{
    fn close(&self) {
        Self::close(self);
    }
}

/// 唯一拥有 JSONL worker 终结权的令牌。
#[must_use = "必须显式排空并终结 JSONL worker"]
pub(crate) struct JsonLinesEventLogFinalizer {
    closer: Option<Box<dyn ChannelCloser>>,
    completion: Option<oneshot::Receiver<WorkerExit>>,
    worker: Option<JoinHandle<()>>,
}

impl JsonLinesEventLogFinalizer {
    pub(crate) async fn finalize(mut self) -> Result<(), JsonLinesShutdownError> {
        if let Some(closer) = self.closer.take() {
            closer.close();
        }
        let exit = self
            .completion
            .take()
            .expect("JSONL finalizer 必须唯一拥有完成接收端")
            .await
            .map_err(|_| JsonLinesShutdownError::WorkerCompletionLost)?;
        let worker = self
            .worker
            .take()
            .expect("JSONL finalizer 必须唯一拥有 worker handle");
        if worker.join().is_err() || exit == WorkerExit::Panicked {
            return Err(JsonLinesShutdownError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for JsonLinesEventLogFinalizer {
    fn drop(&mut self) {
        if let Some(closer) = &self.closer {
            closer.close();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerExit {
    Drained,
    Panicked,
}

pub(crate) fn start_stream<R>(
    root: PathBuf,
    stem: &'static str,
    config: JsonLinesStreamConfig,
) -> Result<(JsonLinesEventLog<R>, JsonLinesEventLogFinalizer), JsonLinesStartError>
where
    R: JsonLineRecord,
{
    let pinned_root = create_directories_without_reparse(&root).map_err(|source| match source {
        WindowsFsError::Io {
            operation: "建立目录组件",
            path,
            source,
        } => JsonLinesStartError::CreateRoot { path, source },
        source => JsonLinesStartError::InvalidRoot(source),
    })?;
    let root = pinned_root.resolved_path().to_path_buf();

    let paths = StreamPaths::new(root, stem, pinned_root);
    let active_path = Arc::new(paths.active.clone());
    let (sender, receiver) = async_channel::bounded(config.queue_capacity);
    let (completion_sender, completion) = oneshot::channel();
    let worker = thread::Builder::new()
        .name(format!("att-jsonl-{stem}"))
        .spawn(move || {
            let exit = if catch_unwind(AssertUnwindSafe(|| {
                run_worker::<R>(receiver, paths, config)
            }))
            .is_ok()
            {
                WorkerExit::Drained
            } else {
                WorkerExit::Panicked
            };
            let _ = completion_sender.send(exit);
        })
        .map_err(JsonLinesStartError::SpawnWorker)?;

    Ok((
        JsonLinesEventLog {
            sender: sender.clone(),
            active_path,
        },
        JsonLinesEventLogFinalizer {
            closer: Some(Box::new(sender)),
            completion: Some(completion),
            worker: Some(worker),
        },
    ))
}

fn run_worker<R>(
    receiver: Receiver<AppendJob<R>>,
    paths: StreamPaths,
    config: JsonLinesStreamConfig,
) where
    R: JsonLineRecord,
{
    let mut validation = ActiveValidationCursor::default();
    while let Ok(job) = receiver.recv_blocking() {
        let result = persist_record::<R>(job.record, &paths, config, &mut validation);
        let _ = job.acknowledgement.send(result);
    }
}

#[derive(Default)]
struct ActiveValidationCursor {
    identity: Option<FileIdentity>,
    validated_length: u64,
}

impl ActiveValidationCursor {
    fn reset(&mut self) {
        self.identity = None;
        self.validated_length = 0;
    }
}

fn persist_record<R>(
    record: R,
    paths: &StreamPaths,
    config: JsonLinesStreamConfig,
    validation: &mut ActiveValidationCursor,
) -> Result<(), JsonLinesAppendError>
where
    R: JsonLineRecord,
{
    let recorded_at = recorded_at_utc();
    let mut bytes = record.serialize(recorded_at).map_err(|source| {
        JsonLinesAppendError::not_persisted(&paths.active, "serialize", source)
    })?;
    bytes.push(b'\n');
    if bytes.len() > config.max_record_bytes {
        return Err(JsonLinesAppendError::not_persisted(
            &paths.active,
            "record_limit",
            format!(
                "记录 {} 字节，超过配置上限 {}",
                bytes.len(),
                config.max_record_bytes
            ),
        ));
    }

    let _lock = ExclusiveFileLock::acquire(&paths.lock, config.lock_timeout)
        .map_err(|source| JsonLinesAppendError::not_persisted(&paths.active, "lock", source))?;
    let current_size = recover_and_validate_active::<R>(paths, config, validation)?;
    let record_length = u64::try_from(bytes.len()).expect("受检记录长度必须可表示为 u64");
    let rotated =
        current_size > 0 && current_size.saturating_add(record_length) > config.max_file_bytes;
    if rotated {
        if let Err(error) = rotate_active(paths) {
            validation.reset();
            return Err(error);
        }
        validation.reset();
    }

    let mut active =
        open_read_write_file_without_reparse(&paths.active, true).map_err(|source| {
            JsonLinesAppendError::not_persisted(&paths.active, "open_active", source)
        })?;
    active.file_mut().seek(SeekFrom::End(0)).map_err(|source| {
        JsonLinesAppendError::not_persisted(&paths.active, "seek_active_end", source)
    })?;
    if let Err(source) = write_record_bytes(active.file_mut(), &paths.active, &bytes) {
        validation.reset();
        return Err(JsonLinesAppendError::outcome_unknown(
            &paths.active,
            "write",
            source,
        ));
    }
    if let Err(source) = sync_record_data(active.file(), &paths.active) {
        validation.reset();
        return Err(JsonLinesAppendError::outcome_unknown(
            &paths.active,
            "sync_data",
            source,
        ));
    }
    validation.identity = FileIdentity::of(active.file(), &paths.active).ok();
    validation.validated_length = if validation.identity.is_some() {
        if rotated {
            record_length
        } else {
            current_size + record_length
        }
    } else {
        0
    };

    maintain_retention(paths, config.retained_rotated_files).map_err(|(residual, source)| {
        JsonLinesAppendError::PersistedButMaintenanceFailed {
            path: paths.active.clone(),
            residual,
            message: source.to_string(),
        }
    })?;
    Ok(())
}

fn write_record_bytes(file: &mut fs::File, _active: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if let Some(TestFault::PartialWrite { bytes_to_write }) =
        take_test_fault(_active, TestFaultKind::PartialWrite)
    {
        let prefix_length = bytes_to_write.min(bytes.len());
        file.write_all(&bytes[..prefix_length])?;
        return Err(test_io_error("JSONL 记录部分写入后失败"));
    }

    file.write_all(bytes)
}

fn sync_record_data(file: &fs::File, _active: &Path) -> io::Result<()> {
    #[cfg(test)]
    if take_test_fault(_active, TestFaultKind::SyncRecord).is_some() {
        return Err(test_io_error("JSONL 记录 sync_data 失败"));
    }

    file.sync_data()
}

fn recover_and_validate_active<R>(
    paths: &StreamPaths,
    config: JsonLinesStreamConfig,
    validation: &mut ActiveValidationCursor,
) -> Result<u64, JsonLinesAppendError>
where
    R: JsonLineRecord,
{
    let file = open_read_write_file_without_reparse(&paths.active, true).map_err(|source| {
        JsonLinesAppendError::not_persisted(&paths.active, "open_for_recovery", source)
    })?;
    let identity = FileIdentity::of(file.file(), &paths.active).ok();
    let file_length = file
        .metadata()
        .map_err(|source| {
            JsonLinesAppendError::not_persisted(&paths.active, "read_active_metadata", source)
        })?
        .len();
    let start = if identity.is_some()
        && identity == validation.identity
        && validation.validated_length <= file_length
    {
        validation.validated_length
    } else {
        0
    };
    let mut reader_file = file.file().try_clone().map_err(|source| {
        JsonLinesAppendError::not_persisted(&paths.active, "clone_for_validation", source)
    })?;
    reader_file.seek(SeekFrom::Start(start)).map_err(|source| {
        JsonLinesAppendError::not_persisted(&paths.active, "seek_for_validation", source)
    })?;
    let mut reader = BufReader::new(reader_file);
    let mut valid_length = start;
    let mut incomplete_tail = false;

    loop {
        let mut line = Vec::new();
        let max_read = u64::try_from(config.max_record_bytes)
            .expect("JSONL 配置已确认记录上限可表示为 u64")
            .saturating_add(1);
        let read = reader
            .by_ref()
            .take(max_read)
            .read_until(b'\n', &mut line)
            .map_err(|source| {
                JsonLinesAppendError::not_persisted(&paths.active, "validate_existing", source)
            })?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            if line.len() > config.max_record_bytes
                && remaining_line_contains_lf(&mut reader).map_err(|source| {
                    JsonLinesAppendError::not_persisted(&paths.active, "validate_existing", source)
                })?
            {
                return Err(JsonLinesAppendError::not_persisted(
                    &paths.active,
                    "validate_existing",
                    "活动文件包含超过单条记录上限的完整记录",
                ));
            }
            incomplete_tail = true;
            break;
        }
        if line.len() > config.max_record_bytes {
            return Err(JsonLinesAppendError::not_persisted(
                &paths.active,
                "validate_existing",
                "活动文件包含超过单条记录上限的完整记录",
            ));
        }
        R::validate(&line[..line.len() - 1]).map_err(|message| {
            JsonLinesAppendError::not_persisted(
                &paths.active,
                "validate_existing",
                format!("活动文件包含完整但损坏的记录：{message}"),
            )
        })?;
        valid_length = valid_length
            .checked_add(u64::try_from(line.len()).expect("记录长度必须可表示为 u64"))
            .ok_or_else(|| {
                JsonLinesAppendError::not_persisted(
                    &paths.active,
                    "validate_existing",
                    "活动文件长度溢出",
                )
            })?;
    }
    drop(reader);

    if incomplete_tail {
        file.file().set_len(valid_length).map_err(|source| {
            JsonLinesAppendError::not_persisted(&paths.active, "truncate_incomplete_tail", source)
        })?;
        file.file().sync_data().map_err(|source| {
            JsonLinesAppendError::not_persisted(&paths.active, "sync_tail_recovery", source)
        })?;
    }
    validation.identity = identity;
    validation.validated_length = valid_length;
    Ok(valid_length)
}

fn remaining_line_contains_lf(reader: &mut impl BufRead) -> io::Result<bool> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(false);
        }
        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(true);
        }
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn rotate_active(paths: &StreamPaths) -> Result<(), JsonLinesAppendError> {
    let sequence = next_rotation_sequence(paths)?;
    let rotated = paths.rotated(sequence);

    #[cfg(test)]
    if take_test_fault(&paths.active, TestFaultKind::RotateOld).is_some() {
        return Err(JsonLinesAppendError::not_persisted(
            &paths.active,
            "rotate",
            test_io_error("轮转原活动文件失败"),
        ));
    }

    rename_without_replace(&paths.active, &rotated)
        .map_err(|source| JsonLinesAppendError::not_persisted(&paths.active, "rotate", source))?;
    let create_result = if take_create_rotated_active_fault(&paths.active) {
        Err(io::Error::other("轮转后建立新活动文件失败"))
    } else {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&paths.active)
            .map(|_| ())
    };
    match create_result {
        Ok(_) => Ok(()),
        Err(source) => match restore_rotated_active(&rotated, &paths.active) {
            Ok(()) => Err(JsonLinesAppendError::not_persisted(
                &paths.active,
                "create_rotated_active",
                source,
            )),
            Err(restoration) => Err(JsonLinesAppendError::outcome_unknown(
                &paths.active,
                "restore_rotated_active",
                format!("创建新活动文件失败：{source}；恢复原活动文件也失败：{restoration}"),
            )),
        },
    }
}

#[cfg(test)]
fn take_create_rotated_active_fault(active: &Path) -> bool {
    take_test_fault(active, TestFaultKind::CreateRotatedActive).is_some()
}

#[cfg(not(test))]
fn take_create_rotated_active_fault(_active: &Path) -> bool {
    false
}

fn restore_rotated_active(rotated: &Path, active: &Path) -> Result<(), String> {
    #[cfg(test)]
    if take_test_fault(active, TestFaultKind::RestoreRotatedActive).is_some() {
        return Err(test_io_error("轮转失败后恢复原活动文件失败").to_string());
    }

    rename_without_replace(rotated, active).map_err(|source| source.to_string())
}

fn next_rotation_sequence(paths: &StreamPaths) -> Result<u64, JsonLinesAppendError> {
    let maximum = scan_rotation_entries(paths)
        .map_err(|failure| {
            JsonLinesAppendError::not_persisted(
                &paths.active,
                "scan_rotations",
                format!("{} （{}）", failure.message, failure.path.display()),
            )
        })?
        .into_iter()
        .map(|entry| entry.sequence)
        .max()
        .unwrap_or(0);
    maximum.checked_add(1).ok_or_else(|| {
        JsonLinesAppendError::not_persisted(
            &paths.active,
            "scan_rotations",
            "JSONL 轮转序号已经耗尽",
        )
    })
}

fn maintain_retention(paths: &StreamPaths, retained: usize) -> Result<(), (PathBuf, String)> {
    let mut rotations = scan_rotation_entries(paths).map_err(|failure| {
        (
            failure.path,
            format!("无法安全枚举轮转文件：{}", failure.message),
        )
    })?;
    rotations.sort_unstable_by_key(|entry| entry.sequence);
    let delete_count = rotations.len().saturating_sub(retained);
    for entry in rotations.into_iter().take(delete_count) {
        #[cfg(test)]
        if take_test_fault(&paths.active, TestFaultKind::RetentionRemove).is_some() {
            return Err((entry.path, "删除超出保留数量的轮转文件失败".to_owned()));
        }
        #[cfg(test)]
        replace_retention_candidate_for_test(&entry.path, &paths.active).map_err(|source| {
            (
                entry.path.clone(),
                format!("建立保留竞态测试失败：{source}"),
            )
        })?;
        delete_regular_file_if_identity(&entry.path, entry.identity)
            .map_err(|source| (entry.path, source.to_string()))?;
    }
    Ok(())
}

struct RotationEntry {
    sequence: u64,
    path: PathBuf,
    identity: FileIdentity,
}

struct RotationScanFailure {
    path: PathBuf,
    message: String,
}

fn scan_rotation_entries(paths: &StreamPaths) -> Result<Vec<RotationEntry>, RotationScanFailure> {
    let entries = fs::read_dir(&paths.root).map_err(|source| RotationScanFailure {
        path: paths.root.clone(),
        message: source.to_string(),
    })?;
    let mut rotations = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RotationScanFailure {
            path: paths.root.clone(),
            message: source.to_string(),
        })?;
        let Some(sequence) = rotation_sequence(paths.stem, &entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let pinned = pin_path_without_reparse(&path).map_err(|source| RotationScanFailure {
            path: path.clone(),
            message: source.to_string(),
        })?;
        let metadata = pinned.metadata().map_err(|source| RotationScanFailure {
            path: path.clone(),
            message: source.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(RotationScanFailure {
                path,
                message: "已知轮转名称不是普通文件".to_owned(),
            });
        }
        let identity =
            FileIdentity::of(pinned.file(), &path).map_err(|source| RotationScanFailure {
                path: path.clone(),
                message: source.to_string(),
            })?;
        rotations.push(RotationEntry {
            sequence,
            path,
            identity,
        });
    }
    Ok(rotations)
}

#[cfg(test)]
fn replace_retention_candidate_for_test(path: &Path, active: &Path) -> io::Result<()> {
    if take_test_fault(active, TestFaultKind::ReplaceRetentionCandidate).is_none() {
        return Ok(());
    }
    let displaced = path.with_extension("enumerated");
    fs::rename(path, &displaced)?;
    fs::write(path, b"foreign replacement")
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestFaultKind {
    PartialWrite,
    SyncRecord,
    RotateOld,
    CreateRotatedActive,
    RestoreRotatedActive,
    RetentionRemove,
    ReplaceRetentionCandidate,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestFault {
    PartialWrite { bytes_to_write: usize },
    SyncRecord,
    RotateOld,
    CreateRotatedActive,
    RestoreRotatedActive,
    RetentionRemove,
    ReplaceRetentionCandidate,
}

#[cfg(test)]
impl TestFault {
    fn kind(self) -> TestFaultKind {
        match self {
            Self::PartialWrite { .. } => TestFaultKind::PartialWrite,
            Self::SyncRecord => TestFaultKind::SyncRecord,
            Self::RotateOld => TestFaultKind::RotateOld,
            Self::CreateRotatedActive => TestFaultKind::CreateRotatedActive,
            Self::RestoreRotatedActive => TestFaultKind::RestoreRotatedActive,
            Self::RetentionRemove => TestFaultKind::RetentionRemove,
            Self::ReplaceRetentionCandidate => TestFaultKind::ReplaceRetentionCandidate,
        }
    }
}

#[cfg(test)]
static TEST_FAULTS: LazyLock<Mutex<HashMap<PathBuf, VecDeque<TestFault>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
struct TestFaultGuard {
    active: PathBuf,
}

#[cfg(test)]
impl Drop for TestFaultGuard {
    fn drop(&mut self) {
        TEST_FAULTS
            .lock()
            .expect("测试故障表不应中毒")
            .remove(&self.active);
    }
}

#[cfg(test)]
fn install_test_faults(
    active: impl Into<PathBuf>,
    faults: impl IntoIterator<Item = TestFault>,
) -> TestFaultGuard {
    let active = active.into();
    let previous = TEST_FAULTS
        .lock()
        .expect("测试故障表不应中毒")
        .insert(active.clone(), faults.into_iter().collect());
    assert!(previous.is_none(), "同一活动文件不得重复安装故障脚本");
    TestFaultGuard { active }
}

#[cfg(test)]
fn take_test_fault(active: &Path, expected: TestFaultKind) -> Option<TestFault> {
    let mut faults = TEST_FAULTS.lock().expect("测试故障表不应中毒");
    let (fault, empty) = {
        let queue = faults.get_mut(active)?;
        let fault = queue
            .front()
            .copied()
            .filter(|fault| fault.kind() == expected)?;
        queue.pop_front();
        (fault, queue.is_empty())
    };
    if empty {
        faults.remove(active);
    }
    Some(fault)
}

#[cfg(test)]
fn test_io_error(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn rotation_sequence(stem: &str, name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let prefix = format!("{stem}.");
    let digits = name.strip_prefix(&prefix)?.strip_suffix(".jsonl")?;
    (digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn recorded_at_utc() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000,
    )
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    use super::*;

    const TEST_STEM: &str = "audit";

    #[derive(Clone, Debug)]
    struct TestRecord {
        sequence: usize,
        padding: String,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct TestWire {
        recorded_at_utc: String,
        sequence: usize,
        padding: String,
    }

    impl JsonLineRecord for TestRecord {
        fn serialize(self, recorded_at_utc: String) -> Result<Vec<u8>, String> {
            serde_json::to_vec(&TestWire {
                recorded_at_utc,
                sequence: self.sequence,
                padding: self.padding,
            })
            .map_err(|error| error.to_string())
        }

        fn validate(bytes: &[u8]) -> Result<(), String> {
            let wire: TestWire =
                serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
            let canonical = serde_json::to_vec(&wire).map_err(|error| error.to_string())?;
            if canonical != bytes {
                return Err("记录不是当前紧凑 wire".to_owned());
            }
            Ok(())
        }
    }

    fn config() -> JsonLinesStreamConfig {
        JsonLinesStreamConfig::new(8, Duration::from_secs(1), 4096, 4096, 2)
            .expect("测试配置应合法")
    }

    fn record(sequence: usize) -> TestRecord {
        TestRecord {
            sequence,
            padding: String::new(),
        }
    }

    fn rotation_record(sequence: usize) -> TestRecord {
        TestRecord {
            sequence,
            padding: "x".repeat(256),
        }
    }

    fn rotation_config(retained_rotated_files: usize) -> JsonLinesStreamConfig {
        JsonLinesStreamConfig::new(8, Duration::from_secs(1), 512, 512, retained_rotated_files)
            .expect("轮转测试配置应合法")
    }

    fn start(
        root: &Path,
        configuration: JsonLinesStreamConfig,
    ) -> (JsonLinesEventLog<TestRecord>, JsonLinesEventLogFinalizer) {
        start_stream(root.to_path_buf(), TEST_STEM, configuration).expect("测试流应可启动")
    }

    #[test]
    fn rejects_invalid_resource_boundaries() {
        assert_eq!(
            JsonLinesStreamConfig::new(0, Duration::from_secs(1), 1, 1, 0),
            Err(JsonLinesConfigurationError::ZeroQueueCapacity)
        );
        assert_eq!(
            JsonLinesStreamConfig::new(1, Duration::ZERO, 1, 1, 0),
            Err(JsonLinesConfigurationError::ZeroLockTimeout)
        );
        assert!(matches!(
            JsonLinesStreamConfig::new(1, Duration::from_secs(1), 2, 1, 0),
            Err(JsonLinesConfigurationError::RecordExceedsFileLimit { .. })
        ));
    }

    #[tokio::test]
    async fn append_acknowledges_only_a_complete_lf_terminated_record() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), config());

        log.append(record(1)).await.expect("追加应成功");
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes = fs::read(directory.path().join("audit.jsonl")).expect("活动文件应存在");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        TestRecord::validate(&bytes[..bytes.len() - 1]).expect("完整记录应可读取");
    }

    #[tokio::test]
    async fn partial_write_and_sync_failure_never_acknowledge_success() {
        for fault in [
            TestFault::PartialWrite { bytes_to_write: 7 },
            TestFault::SyncRecord,
        ] {
            let directory = tempdir().expect("临时目录应可创建");
            let (log, finalizer) = start(directory.path(), config());
            let active = log.active_path.as_ref().clone();
            let _faults = install_test_faults(active, [fault]);

            assert!(matches!(
                log.append(record(1)).await,
                Err(JsonLinesAppendError::OutcomeUnknown { .. })
            ));
            finalizer.finalize().await.expect("worker 应可排空");
        }
    }

    #[tokio::test]
    async fn rotate_failure_keeps_the_previous_active_file_and_rejects_the_record() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), rotation_config(2));
        log.append(rotation_record(1))
            .await
            .expect("首条记录应成功");
        let active = log.active_path.as_ref().clone();
        let previous = fs::read(&active).expect("原活动文件应存在");
        let _faults = install_test_faults(&active, [TestFault::RotateOld]);

        assert!(matches!(
            log.append(rotation_record(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "rotate",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        assert_eq!(fs::read(&active).expect("原活动文件应保留"), previous);
        assert!(
            !directory
                .path()
                .join("audit.00000000000000000001.jsonl")
                .exists()
        );
    }

    #[tokio::test]
    async fn new_active_creation_failure_restores_the_previous_active_file() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), rotation_config(2));
        log.append(rotation_record(1))
            .await
            .expect("首条记录应成功");
        let active = log.active_path.as_ref().clone();
        let previous = fs::read(&active).expect("原活动文件应存在");
        let _faults = install_test_faults(&active, [TestFault::CreateRotatedActive]);

        assert!(matches!(
            log.append(rotation_record(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "create_rotated_active",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        assert_eq!(fs::read(&active).expect("原活动文件应被恢复"), previous);
        assert!(
            !directory
                .path()
                .join("audit.00000000000000000001.jsonl")
                .exists()
        );
    }

    #[tokio::test]
    async fn failed_active_restoration_reports_unknown_and_preserves_the_rotated_file() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), rotation_config(2));
        log.append(rotation_record(1))
            .await
            .expect("首条记录应成功");
        let active = log.active_path.as_ref().clone();
        let previous = fs::read(&active).expect("原活动文件应存在");
        let rotated = active
            .parent()
            .expect("活动文件应有父目录")
            .join("audit.00000000000000000001.jsonl");
        let _faults = install_test_faults(
            &active,
            [
                TestFault::CreateRotatedActive,
                TestFault::RestoreRotatedActive,
            ],
        );

        assert!(matches!(
            log.append(rotation_record(2)).await,
            Err(JsonLinesAppendError::OutcomeUnknown {
                stage: "restore_rotated_active",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        assert!(!active.exists());
        assert_eq!(fs::read(rotated).expect("原记录的轮转文件应保留"), previous);
    }

    #[tokio::test]
    async fn retention_removal_failure_reports_persisted_record_and_exact_residual() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), rotation_config(2));
        let active = log.active_path.as_ref().clone();
        let root = active.parent().expect("活动文件应有父目录");
        let oldest = root.join("audit.00000000000000000001.jsonl");
        let middle = root.join("audit.00000000000000000002.jsonl");
        let newest = root.join("audit.00000000000000000003.jsonl");
        fs::write(&oldest, b"oldest").expect("最旧轮转文件应可建立");
        fs::write(&middle, b"middle").expect("中间轮转文件应可建立");
        fs::write(&newest, b"newest").expect("最新轮转文件应可建立");
        let _faults = install_test_faults(&active, [TestFault::RetentionRemove]);

        let error = log
            .append(record(1))
            .await
            .expect_err("保留清理失败不得确认完全成功");
        assert!(matches!(
            error,
            JsonLinesAppendError::PersistedButMaintenanceFailed {
                residual,
                ..
            } if residual == oldest
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        let active_bytes = fs::read(&active).expect("已同步的活动记录应保留");
        assert_eq!(
            active_bytes.iter().filter(|byte| **byte == b'\n').count(),
            1
        );
        TestRecord::validate(&active_bytes[..active_bytes.len() - 1])
            .expect("已持久化记录应完整有效");
        assert_eq!(fs::read(oldest).expect("清理残留应保留"), b"oldest");
        assert_eq!(fs::read(middle).expect("后续轮转文件不应被删除"), b"middle");
        assert_eq!(fs::read(newest).expect("后续轮转文件不应被删除"), b"newest");
    }

    #[tokio::test]
    async fn retention_identity_race_preserves_both_files_and_reports_maintenance_failure() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), rotation_config(2));
        let active = log.active_path.as_ref().clone();
        let root = active.parent().expect("活动文件应有父目录");
        let oldest = root.join("audit.00000000000000000001.jsonl");
        let middle = root.join("audit.00000000000000000002.jsonl");
        let newest = root.join("audit.00000000000000000003.jsonl");
        fs::write(&oldest, b"enumerated identity").expect("最旧轮转文件应可建立");
        fs::write(&middle, b"middle").expect("中间轮转文件应可建立");
        fs::write(&newest, b"newest").expect("最新轮转文件应可建立");
        let displaced = oldest.with_extension("enumerated");
        let _faults = install_test_faults(&active, [TestFault::ReplaceRetentionCandidate]);

        let error = log
            .append(record(1))
            .await
            .expect_err("身份竞态不得确认维护完成");
        assert!(matches!(
            error,
            JsonLinesAppendError::PersistedButMaintenanceFailed {
                residual,
                ..
            } if residual == oldest
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        let active_bytes = fs::read(&active).expect("已同步的活动记录应保留");
        TestRecord::validate(&active_bytes[..active_bytes.len() - 1])
            .expect("已持久化记录应完整有效");
        assert_eq!(
            fs::read(&oldest).expect("替换对象不得被误删"),
            b"foreign replacement"
        );
        assert_eq!(
            fs::read(displaced).expect("枚举时锁定的原文件应保留"),
            b"enumerated identity"
        );
        assert_eq!(fs::read(middle).expect("后续轮转文件不应被删除"), b"middle");
        assert_eq!(fs::read(newest).expect("后续轮转文件不应被删除"), b"newest");
    }

    #[tokio::test]
    async fn incomplete_tail_is_recovered_but_a_complete_bad_line_blocks_append() {
        let directory = tempdir().expect("临时目录应可创建");
        let active = directory.path().join("audit.jsonl");
        fs::write(&active, b"{\"recorded_at_utc\":\"cut").expect("半行应可写入");
        let (log, finalizer) = start(directory.path(), config());
        log.append(record(1)).await.expect("半行应被截断后追加");
        finalizer.finalize().await.expect("worker 应可排空");
        let bytes = fs::read(&active).expect("活动文件应存在");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);

        fs::write(&active, b"{\"bad\":true}\n").expect("坏行应可写入");
        let (log, finalizer) = start(directory.path(), config());
        assert!(matches!(
            log.append(record(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "validate_existing",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(fs::read(active).expect("坏行应保留"), b"{\"bad\":true}\n");
    }

    #[tokio::test]
    async fn rotates_monotonically_and_only_deletes_recognized_old_files() {
        let directory = tempdir().expect("临时目录应可创建");
        let small = JsonLinesStreamConfig::new(8, Duration::from_secs(1), 1024, 1024, 2)
            .expect("测试配置应合法");
        fs::write(directory.path().join("audit.notes.jsonl"), b"keep").expect("未知文件应可建立");
        let (log, finalizer) = start(directory.path(), small);
        for sequence in 0..20 {
            log.append(TestRecord {
                sequence,
                padding: "x".repeat(128),
            })
            .await
            .expect("追加应成功");
        }
        finalizer.finalize().await.expect("worker 应可排空");

        let rotations = fs::read_dir(directory.path())
            .expect("日志根应可枚举")
            .filter_map(Result::ok)
            .filter(|entry| rotation_sequence(TEST_STEM, &entry.file_name()).is_some())
            .count();
        assert!((1..=2).contains(&rotations));
        assert!(directory.path().join("audit.notes.jsonl").exists());
    }

    #[tokio::test]
    async fn a_dropped_append_future_does_not_cancel_an_accepted_record() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = start(directory.path(), config());
        let task = tokio::spawn(async move { log.append(record(1)).await });
        tokio::task::yield_now().await;
        task.abort();

        finalizer.finalize().await.expect("worker 应可排空");
        let bytes =
            fs::read(directory.path().join("audit.jsonl")).expect("worker 接管的事件应完成持久化");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[tokio::test]
    async fn independent_workers_share_one_complete_physical_order() {
        let directory = tempdir().expect("临时目录应可创建");
        let shared = JsonLinesStreamConfig::new(8, Duration::from_secs(1), 4096, 65_536, 2)
            .expect("测试配置应合法");
        let (first, first_finalizer) = start(directory.path(), shared);
        let (second, second_finalizer) = start(directory.path(), shared);

        let first_task = tokio::spawn(async move {
            for sequence in 0..8 {
                first.append(record(sequence)).await.expect("第一流应成功");
            }
        });
        let second_task = tokio::spawn(async move {
            for sequence in 8..16 {
                second.append(record(sequence)).await.expect("第二流应成功");
            }
        });
        first_task.await.expect("第一任务不应 panic");
        second_task.await.expect("第二任务不应 panic");
        first_finalizer.finalize().await.expect("第一流应排空");
        second_finalizer.finalize().await.expect("第二流应排空");

        let bytes = fs::read(directory.path().join("audit.jsonl")).expect("活动文件应存在");
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 16);
        for line in lines {
            TestRecord::validate(line).expect("每条物理记录都应完整");
        }
    }
}
