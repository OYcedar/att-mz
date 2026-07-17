//! MZ 两条全局 JSON Lines 持久事件流。
//!
//! 每个流拥有独立的有界队列、OS worker、跨进程文件锁和轮转序列。worker 接管
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
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::windows::{
    ExclusiveFileLock, FileIdentity, PinnedPath, WindowsFsError,
    create_directories_without_reparse, delete_regular_file_if_identity,
    open_read_write_file_without_reparse, pin_path_without_reparse, rename_without_replace,
    validate_local_case_insensitive_ntfs_directory,
};
use crate::att_mz::project::MzWriteBackLayoutProfile;
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource};
use crate::att_mz::translate::executor::LlmUsage;
use crate::att_mz::translate::standard::{
    LoggedAcceptedTranslationDecision, LoggedUnresolvedTranslationUnit,
    StandardTranslationRunReport, TranslationLogEvent, TranslationProtocolDiagnostic,
    TranslationTaskLogRecord, TranslationTaskStatus, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason,
};
use crate::att_mz::write_back::StandardWriteBackSummary;
use crate::att_mz::write_back::standard::{
    ManualLayoutDiagnostic, MzWriteBackLayoutRegion, StandardWriteBackRunLog,
};
use crate::observability::{PersistentEventLog, RunId};

const TRANSLATION_STEM: &str = "translation";
const WRITE_BACK_STEM: &str = "write_back";

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

/// 同一 Standard Translate 运行的稳定日志上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationRunLogContext {
    run_id: RunId,
    project: String,
    profile: String,
}

impl TranslationRunLogContext {
    pub(crate) fn new(
        run_id: RunId,
        project: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            project: project.into(),
            profile: profile.into(),
        }
    }
}

/// 同一 Standard WriteBack 运行的稳定日志上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WriteBackRunLogContext {
    run_id: RunId,
    project: String,
}

impl WriteBackRunLogContext {
    pub(crate) fn new(run_id: RunId, project: impl Into<String>) -> Self {
        Self {
            run_id,
            project: project.into(),
        }
    }
}

trait JsonLineRecord: Send + 'static {
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

struct JsonLinesEventLog<R> {
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
    async fn append(&self, record: R) -> Result<(), JsonLinesAppendError> {
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

fn start_stream<R>(
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
    let root = validate_local_case_insensitive_ntfs_directory(pinned_root.resolved_path())
        .map_err(JsonLinesStartError::InvalidRoot)?;

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

/// Standard Translate 使用的生产日志根。
#[derive(Clone)]
pub(crate) struct TranslationJsonLinesEventLog {
    context: Arc<TranslationRunLogContext>,
    stream: JsonLinesEventLog<TranslationRecord>,
}

impl TranslationJsonLinesEventLog {
    pub(crate) fn start(
        root: PathBuf,
        config: JsonLinesStreamConfig,
        context: TranslationRunLogContext,
    ) -> Result<(Self, JsonLinesEventLogFinalizer), JsonLinesStartError> {
        let (stream, finalizer) = start_stream(root, TRANSLATION_STEM, config)?;
        Ok((
            Self {
                context: Arc::new(context),
                stream,
            },
            finalizer,
        ))
    }
}

impl PersistentEventLog<TranslationLogEvent> for TranslationJsonLinesEventLog {
    type Error = JsonLinesAppendError;

    async fn append(&self, event: TranslationLogEvent) -> Result<(), Self::Error> {
        self.stream
            .append(TranslationRecord {
                context: Arc::clone(&self.context),
                event,
            })
            .await
    }
}

/// Standard WriteBack 使用的生产日志根。
#[derive(Clone)]
pub(crate) struct WriteBackJsonLinesEventLog {
    context: Arc<WriteBackRunLogContext>,
    stream: JsonLinesEventLog<WriteBackRecord>,
}

impl WriteBackJsonLinesEventLog {
    pub(crate) fn start(
        root: PathBuf,
        config: JsonLinesStreamConfig,
        context: WriteBackRunLogContext,
    ) -> Result<(Self, JsonLinesEventLogFinalizer), JsonLinesStartError> {
        let (stream, finalizer) = start_stream(root, WRITE_BACK_STEM, config)?;
        Ok((
            Self {
                context: Arc::new(context),
                stream,
            },
            finalizer,
        ))
    }
}

impl PersistentEventLog<StandardWriteBackRunLog> for WriteBackJsonLinesEventLog {
    type Error = JsonLinesAppendError;

    async fn append(&self, event: StandardWriteBackRunLog) -> Result<(), Self::Error> {
        self.stream
            .append(WriteBackRecord {
                context: Arc::clone(&self.context),
                event,
            })
            .await
    }
}

struct TranslationRecord {
    context: Arc<TranslationRunLogContext>,
    event: TranslationLogEvent,
}

struct WriteBackRecord {
    context: Arc<WriteBackRunLogContext>,
    event: StandardWriteBackRunLog,
}

// DTO 定义位于本文件后半部分；领域类型不直接承担持久化格式。

impl JsonLineRecord for TranslationRecord {
    fn serialize(self, recorded_at_utc: String) -> Result<Vec<u8>, String> {
        let context = &self.context;
        let run_id = context.run_id.to_string();
        validate_common_fields(
            &recorded_at_utc,
            &run_id,
            &context.project,
            Some(&context.profile),
        )?;
        match self.event {
            TranslationLogEvent::TaskProcessed(task) => {
                serde_json::to_vec(&TranslationTaskProcessedWire {
                    recorded_at_utc,
                    run_id: run_id.clone(),
                    project: context.project.clone(),
                    profile: context.profile.clone(),
                    event: TaskProcessedEvent::TaskProcessed,
                    task: TranslationTaskWire::from(&task),
                })
            }
            TranslationLogEvent::TaskCommitFailed(failure) => {
                serde_json::to_vec(&TranslationTaskCommitFailedWire {
                    recorded_at_utc,
                    run_id: run_id.clone(),
                    project: context.project.clone(),
                    profile: context.profile.clone(),
                    event: TaskCommitFailedEvent::TaskCommitFailed,
                    task: TranslationTaskWire::from(failure.outcome()),
                    commit_failure: failure.commit_failure().to_owned(),
                })
            }
            TranslationLogEvent::RunCompleted(summary) => {
                serde_json::to_vec(&TranslationRunCompletedWire {
                    recorded_at_utc,
                    run_id,
                    project: context.project.clone(),
                    profile: context.profile.clone(),
                    event: TranslationRunCompletedEvent::RunCompleted,
                    summary: TranslationSummaryWire::from(&summary),
                })
            }
        }
        .map_err(|error| error.to_string())
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        let probe: TranslationEventProbe =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        match probe.event.as_str() {
            "task_processed" => {
                let wire: TranslationTaskProcessedWire =
                    serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
                validate_common_fields(
                    &wire.recorded_at_utc,
                    &wire.run_id,
                    &wire.project,
                    Some(&wire.profile),
                )?;
                validate_canonical_wire(bytes, &wire)
            }
            "task_commit_failed" => {
                let wire: TranslationTaskCommitFailedWire =
                    serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
                validate_common_fields(
                    &wire.recorded_at_utc,
                    &wire.run_id,
                    &wire.project,
                    Some(&wire.profile),
                )?;
                validate_canonical_wire(bytes, &wire)
            }
            "run_completed" => {
                let wire: TranslationRunCompletedWire =
                    serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
                validate_common_fields(
                    &wire.recorded_at_utc,
                    &wire.run_id,
                    &wire.project,
                    Some(&wire.profile),
                )?;
                validate_canonical_wire(bytes, &wire)
            }
            event => Err(format!("未知 Translation event：{event}")),
        }
    }
}

#[derive(Deserialize)]
struct TranslationEventProbe {
    event: String,
}

impl JsonLineRecord for WriteBackRecord {
    fn serialize(self, recorded_at_utc: String) -> Result<Vec<u8>, String> {
        let context = &self.context;
        let run_id = context.run_id.to_string();
        validate_common_fields(&recorded_at_utc, &run_id, &context.project, None)?;
        if self.event.name().as_str() != context.project {
            return Err("WriteBack 事件项目与运行上下文不一致".to_owned());
        }
        let output_root = self
            .event
            .output_root()
            .to_str()
            .ok_or_else(|| "WriteBack 输出路径无法无损表示为 UTF-8".to_owned())?
            .to_owned();
        let wire = WriteBackRunCompletedWire {
            recorded_at_utc,
            run_id,
            project: context.project.clone(),
            layout_profile: LayoutProfileWire::from(self.event.layout_profile()),
            event: WriteBackRunCompletedEvent::RunCompleted,
            output_root,
            summary: WriteBackSummaryWire::from(self.event.summary()),
            manual_layout_diagnostics: self
                .event
                .manual_layout_diagnostics()
                .iter()
                .map(ManualLayoutDiagnosticWire::from)
                .collect(),
        };
        serde_json::to_vec(&wire).map_err(|error| error.to_string())
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        let wire: WriteBackRunCompletedWire =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        validate_common_fields(&wire.recorded_at_utc, &wire.run_id, &wire.project, None)?;
        if wire.output_root.trim().is_empty() {
            return Err("output_root 不能为空".to_owned());
        }
        if wire.layout_profile.dialogue_body_max_fullwidth_chars == 0
            || wire.layout_profile.scrolling_text_max_fullwidth_chars == 0
            || wire.layout_profile.help_description_max_fullwidth_chars == 0
        {
            return Err("layout_profile 的三个实际宽度都必须大于零".to_owned());
        }
        if wire.summary.manual_layout_units != wire.manual_layout_diagnostics.len() {
            return Err("manual_layout_units 与结构化诊断数量不一致".to_owned());
        }
        validate_canonical_wire(bytes, &wire)
    }
}

fn validate_canonical_wire(bytes: &[u8], wire: &impl Serialize) -> Result<(), String> {
    let canonical = serde_json::to_vec(wire).map_err(|error| error.to_string())?;
    if canonical != bytes {
        return Err("记录不是当前紧凑 UTF-8 wire".to_owned());
    }
    Ok(())
}

fn validate_common_fields(
    recorded_at_utc: &str,
    run_id: &str,
    project: &str,
    profile: Option<&str>,
) -> Result<(), String> {
    validate_recorded_at_utc(recorded_at_utc)?;
    let parsed = Uuid::parse_str(run_id).map_err(|_| "run_id 不是规范 UUID".to_owned())?;
    if parsed.get_version_num() != 4 || parsed.to_string() != run_id {
        return Err("run_id 不是规范小写 UUID v4".to_owned());
    }
    if project.trim().is_empty() {
        return Err("project 不能为空".to_owned());
    }
    if profile.is_some_and(|value| value.trim().is_empty()) {
        return Err("profile 不能为空".to_owned());
    }
    Ok(())
}

fn validate_recorded_at_utc(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err("recorded_at_utc 不是 UTC 毫秒格式".to_owned());
    }
    let number = |range: std::ops::Range<usize>| -> Result<u32, String> {
        std::str::from_utf8(&bytes[range])
            .ok()
            .and_then(|part| part.parse().ok())
            .ok_or_else(|| "recorded_at_utc 包含非法数字".to_owned())
    };
    let year = i32::try_from(number(0..4)?).map_err(|_| "年份越界".to_owned())?;
    let month =
        time::Month::try_from(u8::try_from(number(5..7)?).map_err(|_| "月份越界".to_owned())?)
            .map_err(|_| "月份越界".to_owned())?;
    let day = u8::try_from(number(8..10)?).map_err(|_| "日期越界".to_owned())?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| "日期越界".to_owned())?;
    let hour = u8::try_from(number(11..13)?).map_err(|_| "小时越界".to_owned())?;
    let minute = u8::try_from(number(14..16)?).map_err(|_| "分钟越界".to_owned())?;
    let second = u8::try_from(number(17..19)?).map_err(|_| "秒越界".to_owned())?;
    let millisecond = u16::try_from(number(20..23)?).map_err(|_| "毫秒越界".to_owned())?;
    time::Time::from_hms_milli(hour, minute, second, millisecond)
        .map_err(|_| "时间越界".to_owned())?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum TaskProcessedEvent {
    #[serde(rename = "task_processed")]
    TaskProcessed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum TaskCommitFailedEvent {
    #[serde(rename = "task_commit_failed")]
    TaskCommitFailed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum TranslationRunCompletedEvent {
    #[serde(rename = "run_completed")]
    RunCompleted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum WriteBackRunCompletedEvent {
    #[serde(rename = "run_completed")]
    RunCompleted,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskProcessedWire {
    recorded_at_utc: String,
    run_id: String,
    project: String,
    profile: String,
    event: TaskProcessedEvent,
    task: TranslationTaskWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskCommitFailedWire {
    recorded_at_utc: String,
    run_id: String,
    project: String,
    profile: String,
    event: TaskCommitFailedEvent,
    task: TranslationTaskWire,
    commit_failure: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationRunCompletedWire {
    recorded_at_utc: String,
    run_id: String,
    project: String,
    profile: String,
    event: TranslationRunCompletedEvent,
    summary: TranslationSummaryWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationTaskWire {
    task_index: usize,
    status: TranslationTaskStatusWire,
    attempts: usize,
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: Option<String>,
    final_response_usage: Option<LlmUsageWire>,
    accepted_decisions: usize,
    confirmed_written_locations: Option<usize>,
    accepted: Vec<AcceptedTranslationWire>,
    unresolved: Vec<UnresolvedTranslationWire>,
    diagnostics: Vec<ProtocolDiagnosticWire>,
}

impl From<&TranslationTaskLogRecord> for TranslationTaskWire {
    fn from(record: &TranslationTaskLogRecord) -> Self {
        Self {
            task_index: record.task_index().get(),
            status: TranslationTaskStatusWire::from(record.status()),
            attempts: record.attempts(),
            provider_request_id: record.provider_request_id().map(str::to_owned),
            provider_response_id: record.provider_response_id().map(str::to_owned),
            finish_reason: record.finish_reason().map(str::to_owned),
            final_response_usage: record.final_response_usage().map(LlmUsageWire::from),
            accepted_decisions: record.accepted_decisions(),
            confirmed_written_locations: record.confirmed_written_locations(),
            accepted: record
                .accepted()
                .iter()
                .map(AcceptedTranslationWire::from)
                .collect(),
            unresolved: record
                .unresolved()
                .iter()
                .map(UnresolvedTranslationWire::from)
                .collect(),
            diagnostics: record
                .diagnostics()
                .iter()
                .map(ProtocolDiagnosticWire::from)
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationTaskStatusWire {
    Complete,
    Partial,
    Unavailable {
        reason: TranslationTaskUnavailableReasonWire,
    },
}

impl From<&TranslationTaskStatus> for TranslationTaskStatusWire {
    fn from(status: &TranslationTaskStatus) -> Self {
        match status {
            TranslationTaskStatus::Complete => Self::Complete,
            TranslationTaskStatus::Partial => Self::Partial,
            TranslationTaskStatus::Unavailable(reason) => Self::Unavailable {
                reason: TranslationTaskUnavailableReasonWire::from(reason),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationTaskUnavailableReasonWire {
    ModelResponseUnusable,
    AllOutputsRejected,
    RecoverableRequestExhausted {
        attempts: usize,
        message: String,
    },
    RetryAfterExceedsConfiguredMaximum {
        attempt: usize,
        retry_after_ms: u128,
        maximum_ms: u128,
        message: String,
    },
}

impl From<&TranslationTaskUnavailableReason> for TranslationTaskUnavailableReasonWire {
    fn from(reason: &TranslationTaskUnavailableReason) -> Self {
        match reason {
            TranslationTaskUnavailableReason::ModelResponseUnusable => Self::ModelResponseUnusable,
            TranslationTaskUnavailableReason::AllOutputsRejected => Self::AllOutputsRejected,
            TranslationTaskUnavailableReason::RecoverableRequestExhausted { attempts, message } => {
                Self::RecoverableRequestExhausted {
                    attempts: *attempts,
                    message: message.clone(),
                }
            }
            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                attempt,
                retry_after,
                maximum,
                message,
            } => Self::RetryAfterExceedsConfiguredMaximum {
                attempt: *attempt,
                retry_after_ms: retry_after.as_millis(),
                maximum_ms: maximum.as_millis(),
                message: message.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LlmUsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

impl From<LlmUsage> for LlmUsageWire {
    fn from(usage: LlmUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens(),
            completion_tokens: usage.completion_tokens(),
            total_tokens: usage.total_tokens(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedTranslationWire {
    id: usize,
    leader: MzLocationWire,
    propagation_targets: Vec<MzLocationWire>,
}

impl From<&LoggedAcceptedTranslationDecision> for AcceptedTranslationWire {
    fn from(decision: &LoggedAcceptedTranslationDecision) -> Self {
        Self {
            id: decision.id(),
            leader: MzLocationWire::from(decision.leader()),
            propagation_targets: decision
                .propagation_targets()
                .iter()
                .map(MzLocationWire::from)
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnresolvedTranslationWire {
    id: usize,
    locations: Vec<MzLocationWire>,
    reason: TranslationUnitRejectionReasonWire,
}

impl From<&LoggedUnresolvedTranslationUnit> for UnresolvedTranslationWire {
    fn from(unit: &LoggedUnresolvedTranslationUnit) -> Self {
        Self {
            id: unit.id(),
            locations: unit.locations().iter().map(MzLocationWire::from).collect(),
            reason: TranslationUnitRejectionReasonWire::from(unit.reason()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TranslationUnitRejectionReasonWire {
    Missing,
    Duplicate,
    InvalidShape { message: String },
    BlankTranslation,
    NoNaturalLanguageText,
    ContainsByteOrderMark,
    PlaceholderMismatch { token: String },
    UnexpectedPlaceholderToken { token: String },
    PlaceholderNormalizationAmbiguous { original: String },
    SourceResidual { fragment: String },
}

impl From<&TranslationUnitRejectionReason> for TranslationUnitRejectionReasonWire {
    fn from(reason: &TranslationUnitRejectionReason) -> Self {
        match reason {
            TranslationUnitRejectionReason::Missing => Self::Missing,
            TranslationUnitRejectionReason::Duplicate => Self::Duplicate,
            TranslationUnitRejectionReason::InvalidShape { message } => Self::InvalidShape {
                message: message.clone(),
            },
            TranslationUnitRejectionReason::BlankTranslation => Self::BlankTranslation,
            TranslationUnitRejectionReason::NoNaturalLanguageText => Self::NoNaturalLanguageText,
            TranslationUnitRejectionReason::ContainsByteOrderMark => Self::ContainsByteOrderMark,
            TranslationUnitRejectionReason::PlaceholderMismatch { token } => {
                Self::PlaceholderMismatch {
                    token: token.clone(),
                }
            }
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token } => {
                Self::UnexpectedPlaceholderToken {
                    token: token.clone(),
                }
            }
            TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { original } => {
                Self::PlaceholderNormalizationAmbiguous {
                    original: original.clone(),
                }
            }
            TranslationUnitRejectionReason::SourceResidual { fragment } => Self::SourceResidual {
                fragment: fragment.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProtocolDiagnosticWire {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    UnknownId { item_index: usize, id: usize },
}

impl From<&TranslationProtocolDiagnostic> for ProtocolDiagnosticWire {
    fn from(diagnostic: &TranslationProtocolDiagnostic) -> Self {
        match diagnostic {
            TranslationProtocolDiagnostic::NonStopFinish { reason } => Self::NonStopFinish {
                reason: reason.clone(),
            },
            TranslationProtocolDiagnostic::InvalidResponse { message } => Self::InvalidResponse {
                message: message.clone(),
            },
            TranslationProtocolDiagnostic::UnknownId { item_index, id } => Self::UnknownId {
                item_index: *item_index,
                id: *id,
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationSummaryWire {
    total_tasks: usize,
    complete_tasks: usize,
    partial_tasks: usize,
    unavailable_tasks: usize,
    accepted_decisions: usize,
    written_locations: usize,
    unresolved_decisions: usize,
    unresolved_locations: usize,
    protocol_diagnostics: usize,
    recoverable_request_exhaustions: usize,
}

impl From<&StandardTranslationRunReport> for TranslationSummaryWire {
    fn from(summary: &StandardTranslationRunReport) -> Self {
        Self {
            total_tasks: summary.total_tasks(),
            complete_tasks: summary.complete_tasks(),
            partial_tasks: summary.partial_tasks(),
            unavailable_tasks: summary.unavailable_tasks(),
            accepted_decisions: summary.accepted_decisions(),
            written_locations: summary.written_locations(),
            unresolved_decisions: summary.unresolved_decisions(),
            unresolved_locations: summary.unresolved_locations(),
            protocol_diagnostics: summary.protocol_diagnostics(),
            recoverable_request_exhaustions: summary.recoverable_request_exhaustions(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackRunCompletedWire {
    recorded_at_utc: String,
    run_id: String,
    project: String,
    layout_profile: LayoutProfileWire,
    event: WriteBackRunCompletedEvent,
    output_root: String,
    summary: WriteBackSummaryWire,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnosticWire>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LayoutProfileWire {
    dialogue_body_max_fullwidth_chars: u32,
    scrolling_text_max_fullwidth_chars: u32,
    help_description_max_fullwidth_chars: u32,
}

impl From<MzWriteBackLayoutProfile> for LayoutProfileWire {
    fn from(profile: MzWriteBackLayoutProfile) -> Self {
        Self {
            dialogue_body_max_fullwidth_chars: profile.dialogue_body().get(),
            scrolling_text_max_fullwidth_chars: profile.scrolling_text().get(),
            help_description_max_fullwidth_chars: profile.help_description().get(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteBackSummaryWire {
    translated_locations: usize,
    original_locations: usize,
    auto_wrapped_units: usize,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
    manual_layout_units: usize,
}

impl From<StandardWriteBackSummary> for WriteBackSummaryWire {
    fn from(summary: StandardWriteBackSummary) -> Self {
        Self {
            translated_locations: summary.translated_locations,
            original_locations: summary.original_locations,
            auto_wrapped_units: summary.auto_wrapped_units,
            inserted_line_breaks: summary.inserted_line_breaks,
            inserted_fullwidth_indents: summary.inserted_fullwidth_indents,
            manual_layout_units: summary.manual_layout_units,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualLayoutDiagnosticWire {
    unit_location: MzLocationWire,
    region: LayoutRegionWire,
    max_fullwidth_chars: u32,
}

impl From<&ManualLayoutDiagnostic> for ManualLayoutDiagnosticWire {
    fn from(diagnostic: &ManualLayoutDiagnostic) -> Self {
        Self {
            unit_location: MzLocationWire::from(diagnostic.unit_location()),
            region: LayoutRegionWire::from(diagnostic.region()),
            max_fullwidth_chars: diagnostic.max_fullwidth_chars().get(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LayoutRegionWire {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

impl From<MzWriteBackLayoutRegion> for LayoutRegionWire {
    fn from(region: MzWriteBackLayoutRegion) -> Self {
        match region {
            MzWriteBackLayoutRegion::DialogueBody => Self::DialogueBody,
            MzWriteBackLayoutRegion::ScrollingText => Self::ScrollingText,
            MzWriteBackLayoutRegion::HelpDescription => Self::HelpDescription,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MzLocationWire {
    Value {
        source: MzSourceWire,
        steps: Vec<MzLocationStepWire>,
    },
    NoteTag {
        source: MzSourceWire,
        container_steps: Vec<MzLocationStepWire>,
        tag_name: String,
        occurrence: usize,
    },
    CommentTag {
        source: MzSourceWire,
        command_steps: Vec<MzLocationStepWire>,
        tag_name: String,
        occurrence: usize,
    },
}

impl From<&MzLocation> for MzLocationWire {
    fn from(location: &MzLocation) -> Self {
        match location {
            MzLocation::Value { source, steps } => Self::Value {
                source: MzSourceWire::from(source),
                steps: steps.iter().map(MzLocationStepWire::from).collect(),
            },
            MzLocation::NoteTag {
                source,
                container_steps,
                tag_name,
                occurrence,
            } => Self::NoteTag {
                source: MzSourceWire::from(source),
                container_steps: container_steps
                    .iter()
                    .map(MzLocationStepWire::from)
                    .collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
            MzLocation::CommentTag {
                source,
                command_steps,
                tag_name,
                occurrence,
            } => Self::CommentTag {
                source: MzSourceWire::from(source),
                command_steps: command_steps.iter().map(MzLocationStepWire::from).collect(),
                tag_name: tag_name.clone(),
                occurrence: *occurrence,
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MzSourceWire {
    Data {
        file: String,
    },
    Map {
        map_id: u32,
    },
    PluginParameter {
        plugin_index: usize,
        plugin_name: String,
        parameter_name: String,
    },
}

impl From<&MzSource> for MzSourceWire {
    fn from(source: &MzSource) -> Self {
        match source {
            MzSource::Data(file) => Self::Data {
                file: file.file_name().to_owned(),
            },
            MzSource::Map(map_id) => Self::Map { map_id: *map_id },
            MzSource::PluginParameter {
                plugin_index,
                plugin_name,
                parameter_name,
            } => Self::PluginParameter {
                plugin_index: *plugin_index,
                plugin_name: plugin_name.clone(),
                parameter_name: parameter_name.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MzLocationStepWire {
    ObjectKey { key: String },
    ArrayIndex { index: usize },
    DecodeJsonString,
}

impl From<&MzLocationStep> for MzLocationStepWire {
    fn from(step: &MzLocationStep) -> Self {
        match step {
            MzLocationStep::ObjectKey(key) => Self::ObjectKey { key: key.clone() },
            MzLocationStep::ArrayIndex(index) => Self::ArrayIndex { index: *index },
            MzLocationStep::DecodeJsonString => Self::DecodeJsonString,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::att_mz::project::MaxFullwidthChars;
    use crate::att_mz::text::{StandardDataFile, TextGroupKind};
    use crate::att_mz::translate::executor::FinalLlmResponseMetadata;
    use crate::att_mz::translate::standard::{
        StandardTranslationTaskIndex, TranslationLeafIdentity,
        TranslationTaskCommitFailureLogRecord, TranslationTaskOutcome, UnresolvedTranslationUnit,
    };

    fn run_id() -> RunId {
        RunId::from_uuid(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("测试 UUID 应合法"),
        )
    }

    fn context() -> TranslationRunLogContext {
        TranslationRunLogContext::new(run_id(), "游戏 一", "main")
    }

    fn config() -> JsonLinesStreamConfig {
        JsonLinesStreamConfig::new(8, Duration::from_secs(1), 4096, 4096, 2)
            .expect("测试配置应合法")
    }

    fn symlink_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) || error.raw_os_error() == Some(1314)
    }

    fn summary_event(total_tasks: usize) -> TranslationLogEvent {
        TranslationLogEvent::RunCompleted(StandardTranslationRunReport::empty(total_tasks))
    }

    fn serialized_summary(total_tasks: usize) -> Vec<u8> {
        TranslationRecord {
            context: Arc::new(context()),
            event: summary_event(total_tasks),
        }
        .serialize("2026-07-17T12:34:56.789Z".to_owned())
        .expect("测试记录应可序列化")
    }

    fn unavailable_task_outcome() -> TranslationTaskOutcome {
        let group = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(1)],
        );
        let exact = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(1), MzLocationStep::key("description")],
        );
        let identity = TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group,
            exact,
            "不得进入日志的原文",
        );
        TranslationTaskOutcome::unavailable(
            StandardTranslationTaskIndex::new(7),
            2,
            Some(FinalLlmResponseMetadata::new(
                Some("request-7".to_owned()),
                "response-7",
                "length",
                Some(LlmUsage::new(101, 23, 124)),
            )),
            TranslationTaskUnavailableReason::ModelResponseUnusable,
            vec![UnresolvedTranslationUnit::new(
                0,
                identity,
                Vec::new(),
                TranslationUnitRejectionReason::Missing,
            )],
            vec![TranslationProtocolDiagnostic::NonStopFinish {
                reason: "length".to_owned(),
            }],
        )
        .expect("测试任务结果应满足不变量")
    }

    fn unavailable_task_record() -> TranslationTaskLogRecord {
        TranslationTaskLogRecord::from_outcome(&unavailable_task_outcome())
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

    #[test]
    fn start_rejects_a_reparse_component_in_the_log_root_path() {
        let temporary = tempdir().expect("临时目录应可创建");
        let real = temporary.path().join("real");
        fs::create_dir(&real).expect("真实父目录应可创建");
        let link = temporary.path().join("log-parent-link");
        if let Err(error) = std::os::windows::fs::symlink_dir(&real, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("目录符号链接应可创建：{error}");
        }

        let error =
            match TranslationJsonLinesEventLog::start(link.join("new-logs"), config(), context()) {
                Ok(_) => panic!("日志根路径中的 reparse point 必须被拒绝"),
                Err(error) => error,
            };
        assert!(matches!(
            error,
            JsonLinesStartError::InvalidRoot(WindowsFsError::ReparsePoint { path })
                if path == link
        ));
        assert!(
            !real.join("new-logs").exists(),
            "拒绝 reparse 路径前不得穿越链接创建缺失后缀"
        );
    }

    #[tokio::test]
    async fn append_rejects_an_active_file_reparse_point_without_touching_its_target() {
        let directory = tempdir().expect("临时目录应可创建");
        let target = directory.path().join("outside-target.jsonl");
        fs::write(&target, b"sentinel").expect("链接目标应可创建");
        let active = directory.path().join("translation.jsonl");
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &active) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("文件符号链接应可创建：{error}");
        }
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根目录本身应合法");

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "open_for_recovery",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(fs::read(target).expect("链接目标应保持存在"), b"sentinel");
    }

    #[test]
    fn validates_utc_millisecond_timestamp_and_uuid_v4() {
        assert!(validate_recorded_at_utc("2026-07-17T12:34:56.789Z").is_ok());
        assert!(validate_recorded_at_utc("2026-02-30T12:34:56.789Z").is_err());
        assert!(validate_recorded_at_utc("2026-07-17T12:34:56Z").is_err());
        assert!(
            validate_common_fields(
                "2026-07-17T12:34:56.789Z",
                "550e8400-e29b-41d4-a716-446655440000",
                "game",
                Some("main")
            )
            .is_ok()
        );
        assert!(
            validate_common_fields(
                "2026-07-17T12:34:56.789Z",
                "550e8400-e29b-11d4-a716-446655440000",
                "game",
                Some("main")
            )
            .is_err()
        );
    }

    #[test]
    fn translation_wire_is_compact_strict_and_omits_text_content() {
        let bytes = serialized_summary(3);
        assert!(!bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        assert!(!bytes.contains(&b'\n'));
        TranslationRecord::validate(&bytes).expect("自产记录应可强类型读取");

        let value: Value = serde_json::from_slice(&bytes).expect("记录应是 JSON");
        assert_eq!(value["event"], "run_completed");
        assert_eq!(value["run_id"], run_id().to_string());
        assert_eq!(value["project"], "游戏 一");
        assert_eq!(value["profile"], "main");
        assert_eq!(value["summary"]["total_tasks"], 3);
        assert!(value.get("messages").is_none());
        assert!(value.get("response").is_none());

        let mut damaged = value;
        damaged
            .as_object_mut()
            .expect("记录应是对象")
            .insert("unknown".to_owned(), json!(true));
        assert!(
            TranslationRecord::validate(
                &serde_json::to_vec(&damaged).expect("损坏测试仍应是 JSON")
            )
            .is_err()
        );
        let text = std::str::from_utf8(&bytes).expect("JSONL 必须是 UTF-8");
        let duplicated_event = format!("{{\"event\":\"run_completed\",{}", &text[1..]);
        assert!(TranslationRecord::validate(duplicated_event.as_bytes()).is_err());
    }

    #[test]
    fn task_wire_separates_provider_identities_and_final_usage_without_text() {
        let bytes = TranslationRecord {
            context: Arc::new(context()),
            event: TranslationLogEvent::TaskProcessed(unavailable_task_record()),
        }
        .serialize("2026-07-17T12:34:56.789Z".to_owned())
        .expect("任务记录应可序列化");
        TranslationRecord::validate(&bytes).expect("任务记录应可强类型读取");
        let value: Value = serde_json::from_slice(&bytes).expect("任务记录应是 JSON");

        assert_eq!(value["event"], "task_processed");
        assert_eq!(value["task"]["provider_request_id"], "request-7");
        assert_eq!(value["task"]["provider_response_id"], "response-7");
        assert_eq!(value["task"]["final_response_usage"]["prompt_tokens"], 101);
        assert_eq!(
            value["task"]["final_response_usage"]["completion_tokens"],
            23
        );
        assert_eq!(value["task"]["final_response_usage"]["total_tokens"], 124);
        let text = std::str::from_utf8(&bytes).expect("JSONL 必须是 UTF-8");
        assert!(!text.contains("不得进入日志的原文"));
    }

    #[test]
    fn commit_failure_wire_keeps_both_content_outcome_and_store_failure() {
        let failure = TranslationTaskCommitFailureLogRecord::new(
            &unavailable_task_outcome(),
            "提交事务结果未知".to_owned(),
        );
        let bytes = TranslationRecord {
            context: Arc::new(context()),
            event: TranslationLogEvent::TaskCommitFailed(failure),
        }
        .serialize("2026-07-17T12:34:56.789Z".to_owned())
        .expect("提交失败记录应可序列化");
        TranslationRecord::validate(&bytes).expect("提交失败记录应可强类型读取");
        let value: Value = serde_json::from_slice(&bytes).expect("提交失败记录应是 JSON");

        assert_eq!(value["event"], "task_commit_failed");
        assert_eq!(value["commit_failure"], "提交事务结果未知");
        assert!(value["task"]["confirmed_written_locations"].is_null());
    }

    #[test]
    fn location_wire_preserves_all_source_step_and_tag_semantics() {
        let source = MzSource::plugin_parameter(3, "任务插件", "Entries");
        let locations = [
            MzLocation::value(
                source.clone(),
                vec![
                    MzLocationStep::DecodeJsonString,
                    MzLocationStep::index(2),
                    MzLocationStep::key("Title"),
                ],
            ),
            MzLocation::note_tag(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(1)],
                "HelpText",
                2,
            ),
            MzLocation::comment_tag(
                MzSource::map(7),
                vec![MzLocationStep::key("events"), MzLocationStep::index(4)],
                "QuestDescription",
                1,
            ),
        ];

        let value = serde_json::to_value(
            locations
                .iter()
                .map(MzLocationWire::from)
                .collect::<Vec<_>>(),
        )
        .expect("位置 wire 应可序列化");
        assert_eq!(value[0]["kind"], "value");
        assert_eq!(value[0]["source"]["kind"], "plugin_parameter");
        assert_eq!(value[0]["source"]["plugin_name"], "任务插件");
        assert_eq!(value[0]["steps"][0]["kind"], "decode_json_string");
        assert_eq!(value[1]["kind"], "note_tag");
        assert_eq!(value[1]["tag_name"], "HelpText");
        assert_eq!(value[2]["kind"], "comment_tag");
        assert_eq!(value[2]["source"]["map_id"], 7);
    }

    #[test]
    fn write_back_wire_contains_actual_layout_and_structured_diagnostics() {
        let width = |value| MaxFullwidthChars::new(value).expect("测试宽度应合法");
        let wire = WriteBackRunCompletedWire {
            recorded_at_utc: "2026-07-17T12:34:56.789Z".to_owned(),
            run_id: run_id().to_string(),
            project: "游戏 一".to_owned(),
            layout_profile: LayoutProfileWire::from(MzWriteBackLayoutProfile::new(
                width(24),
                width(30),
                width(18),
            )),
            event: WriteBackRunCompletedEvent::RunCompleted,
            output_root: "C:/ATT/项目/write_back".to_owned(),
            summary: WriteBackSummaryWire::from(StandardWriteBackSummary {
                translated_locations: 2,
                original_locations: 1,
                auto_wrapped_units: 1,
                inserted_line_breaks: 2,
                inserted_fullwidth_indents: 1,
                manual_layout_units: 1,
            }),
            manual_layout_diagnostics: vec![ManualLayoutDiagnosticWire {
                unit_location: MzLocationWire::from(&MzLocation::value(
                    MzSource::map(1),
                    vec![MzLocationStep::index(3)],
                )),
                region: LayoutRegionWire::DialogueBody,
                max_fullwidth_chars: 24,
            }],
        };
        let bytes = serde_json::to_vec(&wire).expect("写回 wire 应可序列化");
        WriteBackRecord::validate(&bytes).expect("写回 wire 应满足当前完整契约");
        let decoded: WriteBackRunCompletedWire =
            serde_json::from_slice(&bytes).expect("写回 wire 应可强类型读取");

        assert_eq!(decoded.layout_profile.dialogue_body_max_fullwidth_chars, 24);
        assert_eq!(
            decoded.layout_profile.scrolling_text_max_fullwidth_chars,
            30
        );
        assert_eq!(
            decoded.layout_profile.help_description_max_fullwidth_chars,
            18
        );
        assert_eq!(decoded.manual_layout_diagnostics.len(), 1);
    }

    #[tokio::test]
    async fn write_back_adapter_uses_the_profile_owned_by_the_completed_event() {
        let directory = tempdir().expect("临时目录应可创建");
        let width = |value| MaxFullwidthChars::new(value).expect("测试宽度应合法");
        let event_profile = MzWriteBackLayoutProfile::new(width(19), width(27), width(13));
        let project = crate::att_mz::project::OpenedProject::new(
            "游戏 一".parse().expect("测试项目名应合法"),
            PathBuf::from("C:/ATT/projects/游戏 一"),
            PathBuf::from("C:/ATT/projects/游戏 一/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            event_profile,
        );
        let event = StandardWriteBackRunLog::new(
            &project,
            event_profile,
            StandardWriteBackSummary::default(),
            Vec::new(),
        );
        let (log, finalizer) = WriteBackJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            WriteBackRunLogContext::new(run_id(), "游戏 一"),
        )
        .expect("写回日志根应可启动");

        log.append(event).await.expect("写回记录应持久化");
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes =
            fs::read(directory.path().join("write_back.jsonl")).expect("写回活动日志应存在");
        assert_eq!(bytes.last(), Some(&b'\n'));
        WriteBackRecord::validate(&bytes[..bytes.len() - 1]).expect("写回记录应满足当前 wire");
        let value: Value =
            serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("写回记录应是 JSON");
        assert_eq!(value["project"], "游戏 一");
        assert_eq!(
            value["layout_profile"]["dialogue_body_max_fullwidth_chars"],
            19
        );
        assert_eq!(
            value["layout_profile"]["scrolling_text_max_fullwidth_chars"],
            27
        );
        assert_eq!(
            value["layout_profile"]["help_description_max_fullwidth_chars"],
            13
        );
        assert!(!directory.path().join("translation.jsonl").exists());
    }

    #[tokio::test]
    async fn append_acknowledges_only_a_complete_synced_lf_record() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");

        log.append(summary_event(2)).await.expect("追加应成功");
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes = fs::read(directory.path().join("translation.jsonl")).expect("活动日志应存在");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        TranslationRecord::validate(&bytes[..bytes.len() - 1]).expect("持久记录应可强类型读取");
    }

    #[tokio::test]
    async fn partial_write_has_an_unknown_outcome_and_never_acknowledges_success() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");
        let active = log.stream.active_path.as_ref().clone();
        let _faults = install_test_faults(
            active.clone(),
            [TestFault::PartialWrite { bytes_to_write: 17 }],
        );

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::OutcomeUnknown { stage: "write", .. })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes = fs::read(active).expect("部分写入的文件应保留供下次恢复");
        assert_eq!(bytes.len(), 17);
        assert_ne!(bytes.last(), Some(&b'\n'));
    }

    #[tokio::test]
    async fn sync_data_failure_has_an_unknown_outcome_even_when_the_line_is_visible() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");
        let active = log.stream.active_path.as_ref().clone();
        let _faults = install_test_faults(active.clone(), [TestFault::SyncRecord]);

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::OutcomeUnknown {
                stage: "sync_data",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes = fs::read(active).expect("写入完成但未确认刷盘的记录应保留");
        assert_eq!(bytes.last(), Some(&b'\n'));
        TranslationRecord::validate(&bytes[..bytes.len() - 1]).expect("可见记录的 wire 应完整");
    }

    #[tokio::test]
    async fn rotation_failure_before_the_old_file_moves_is_definitively_not_persisted() {
        let directory = tempdir().expect("临时目录应可创建");
        let active = directory.path().join("translation.jsonl");
        let mut existing = serialized_summary(1);
        existing.push(b'\n');
        fs::write(&active, &existing).expect("现有完整记录应可建立");
        let limit = existing.len();
        let rotating = JsonLinesStreamConfig::new(
            4,
            Duration::from_secs(1),
            limit,
            u64::try_from(limit).expect("测试上限应可表示"),
            2,
        )
        .expect("轮转测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            rotating,
            context(),
        )
        .expect("日志根应可启动");
        let _faults = install_test_faults(
            log.stream.active_path.as_ref().clone(),
            [TestFault::RotateOld],
        );

        assert!(matches!(
            log.append(summary_event(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "rotate",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(fs::read(active).expect("原活动文件应保持完整"), existing);
    }

    #[tokio::test]
    async fn failed_new_active_creation_restores_the_old_file_as_not_persisted() {
        let directory = tempdir().expect("临时目录应可创建");
        let active = directory.path().join("translation.jsonl");
        let mut existing = serialized_summary(1);
        existing.push(b'\n');
        fs::write(&active, &existing).expect("现有完整记录应可建立");
        let limit = existing.len();
        let rotating = JsonLinesStreamConfig::new(
            4,
            Duration::from_secs(1),
            limit,
            u64::try_from(limit).expect("测试上限应可表示"),
            2,
        )
        .expect("轮转测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            rotating,
            context(),
        )
        .expect("日志根应可启动");
        let _faults = install_test_faults(
            log.stream.active_path.as_ref().clone(),
            [TestFault::CreateRotatedActive],
        );

        assert!(matches!(
            log.append(summary_event(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "create_rotated_active",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(fs::read(active).expect("原活动文件应已恢复"), existing);
    }

    #[tokio::test]
    async fn failed_rotation_restoration_has_an_unknown_outcome() {
        let directory = tempdir().expect("临时目录应可创建");
        let active = directory.path().join("translation.jsonl");
        let mut existing = serialized_summary(1);
        existing.push(b'\n');
        fs::write(&active, &existing).expect("现有完整记录应可建立");
        let limit = existing.len();
        let rotating = JsonLinesStreamConfig::new(
            4,
            Duration::from_secs(1),
            limit,
            u64::try_from(limit).expect("测试上限应可表示"),
            2,
        )
        .expect("轮转测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            rotating,
            context(),
        )
        .expect("日志根应可启动");
        let _faults = install_test_faults(
            log.stream.active_path.as_ref().clone(),
            [
                TestFault::CreateRotatedActive,
                TestFault::RestoreRotatedActive,
            ],
        );

        assert!(matches!(
            log.append(summary_event(2)).await,
            Err(JsonLinesAppendError::OutcomeUnknown {
                stage: "restore_rotated_active",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert!(!active.exists(), "恢复失败后不得伪报活动文件仍存在");
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join("translation.00000000000000000001.jsonl")
            )
            .expect("已移动的原记录应保留供诊断"),
            existing
        );
    }

    #[tokio::test]
    async fn retention_failure_reports_persisted_but_maintenance_failed() {
        let directory = tempdir().expect("临时目录应可创建");
        let residual = directory
            .path()
            .join("translation.00000000000000000001.jsonl");
        fs::write(&residual, b"rotation").expect("待清理轮转文件应可建立");
        let no_retention = JsonLinesStreamConfig::new(4, Duration::from_secs(1), 4096, 4096, 0)
            .expect("测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            no_retention,
            context(),
        )
        .expect("日志根应可启动");
        let active = log.stream.active_path.as_ref().clone();
        let _faults = install_test_faults(active.clone(), [TestFault::RetentionRemove]);

        match log.append(summary_event(1)).await {
            Err(JsonLinesAppendError::PersistedButMaintenanceFailed {
                path,
                residual: actual_residual,
                ..
            }) => {
                assert_eq!(path, active);
                assert_eq!(
                    fs::canonicalize(actual_residual).expect("报告的残留路径应可规范化"),
                    fs::canonicalize(&residual).expect("预期残留路径应可规范化")
                );
            }
            other => panic!("应报告已持久化但维护失败，实际为 {other:?}"),
        }
        finalizer.finalize().await.expect("worker 应可排空");

        let bytes = fs::read(&active).expect("当前记录应已持久化");
        assert_eq!(bytes.last(), Some(&b'\n'));
        TranslationRecord::validate(&bytes[..bytes.len() - 1]).expect("已持久化记录应完整");
        assert!(residual.exists(), "返回的维护残留路径必须真实存在");
    }

    #[tokio::test]
    async fn log_root_identity_stays_pinned_for_the_complete_worker_lifetime() {
        let directory = tempdir().expect("临时目录应可创建");
        let root = directory.path().join("logs");
        let moved = directory.path().join("foreign-logs");
        let (log, finalizer) =
            TranslationJsonLinesEventLog::start(root.clone(), config(), context())
                .expect("日志根应可启动");

        assert!(
            fs::rename(&root, &moved).is_err(),
            "worker 存活期内日志根必须由无删除共享句柄阻止替换"
        );
        log.append(summary_event(1))
            .await
            .expect("原日志根应仍可追加");
        finalizer.finalize().await.expect("worker 应可排空");
        assert!(root.join("translation.jsonl").exists());

        fs::rename(&root, &moved).expect("明确终结 worker 后应释放日志根身份句柄");
        assert!(moved.join("translation.jsonl").exists());
    }

    #[tokio::test]
    async fn retention_refuses_to_delete_a_foreign_replacement_after_enumeration() {
        let directory = tempdir().expect("临时目录应可创建");
        let residual = directory
            .path()
            .join("translation.00000000000000000001.jsonl");
        fs::write(&residual, b"enumerated original").expect("待清理轮转文件应可建立");
        let no_retention = JsonLinesStreamConfig::new(4, Duration::from_secs(1), 4096, 4096, 0)
            .expect("测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            no_retention,
            context(),
        )
        .expect("日志根应可启动");
        let active = log.stream.active_path.as_ref().clone();
        let _faults = install_test_faults(active.clone(), [TestFault::ReplaceRetentionCandidate]);

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::PersistedButMaintenanceFailed { .. })
        ));
        finalizer.finalize().await.expect("worker 应可排空");

        assert_eq!(
            fs::read(&residual).expect("枚举后换入的外来文件不得被删除"),
            b"foreign replacement"
        );
        assert_eq!(
            fs::read(residual.with_extension("enumerated")).expect("枚举时的原文件应保留供诊断"),
            b"enumerated original"
        );
        let bytes = fs::read(active).expect("当前记录应已在维护前持久化");
        TranslationRecord::validate(&bytes[..bytes.len() - 1]).expect("已持久化记录应完整");
    }

    #[tokio::test]
    async fn retention_rejects_a_recognized_rotation_reparse_point() {
        let directory = tempdir().expect("临时目录应可创建");
        let target = directory.path().join("outside.jsonl");
        fs::write(&target, b"outside").expect("链接目标应可建立");
        let residual = directory
            .path()
            .join("translation.00000000000000000001.jsonl");
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &residual) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("轮转文件符号链接应可建立：{error}");
        }
        let no_retention = JsonLinesStreamConfig::new(4, Duration::from_secs(1), 4096, 4096, 0)
            .expect("测试配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            no_retention,
            context(),
        )
        .expect("日志根应可启动");

        let result = log.append(summary_event(1)).await;
        match result {
            Err(JsonLinesAppendError::PersistedButMaintenanceFailed {
                residual: actual, ..
            }) => {
                let expected = fs::canonicalize(directory.path())
                    .expect("日志根应可规范化")
                    .join(residual.file_name().expect("轮转文件应有名称"));
                assert_eq!(actual, expected);
            }
            other => panic!("已识别轮转名称的 reparse point 必须使维护失败：{other:?}"),
        }
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(fs::read(target).expect("链接目标不得被修改"), b"outside");
    }

    #[tokio::test]
    async fn dropped_future_after_enqueue_does_not_cancel_persistence() {
        let directory = tempdir().expect("临时目录应可创建");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");
        let task = tokio::spawn(async move { log.append(summary_event(1)).await });
        tokio::task::yield_now().await;
        task.abort();

        finalizer.finalize().await.expect("worker 应可排空");
        let bytes = fs::read(directory.path().join("translation.jsonl"))
            .expect("worker 接管的事件应完成持久化");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[tokio::test]
    async fn independent_workers_share_one_physical_order_through_the_windows_lock() {
        let directory = tempdir().expect("临时目录应可创建");
        let shared_config = JsonLinesStreamConfig::new(8, Duration::from_secs(1), 4096, 65_536, 2)
            .expect("并发测试配置应合法");
        let (first, first_finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            shared_config,
            TranslationRunLogContext::new(run_id(), "游戏 一", "first"),
        )
        .expect("第一日志根应可启动");
        let second_run_id = RunId::from_uuid(
            Uuid::parse_str("7c9e6679-7425-40de-944b-e07fc1f90ae7").expect("第二测试 UUID 应合法"),
        );
        let (second, second_finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            shared_config,
            TranslationRunLogContext::new(second_run_id, "游戏 二", "second"),
        )
        .expect("第二日志根应可启动");

        let first_task = tokio::spawn(async move {
            for index in 0..8 {
                first
                    .append(summary_event(index))
                    .await
                    .expect("第一 worker 追加应成功");
            }
        });
        let second_task = tokio::spawn(async move {
            for index in 0..8 {
                second
                    .append(summary_event(index))
                    .await
                    .expect("第二 worker 追加应成功");
            }
        });
        first_task.await.expect("第一追加任务不应 panic");
        second_task.await.expect("第二追加任务不应 panic");
        first_finalizer
            .finalize()
            .await
            .expect("第一 worker 应可排空");
        second_finalizer
            .finalize()
            .await
            .expect("第二 worker 应可排空");

        let bytes =
            fs::read(directory.path().join("translation.jsonl")).expect("共享活动日志应存在");
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 16);
        for line in lines {
            TranslationRecord::validate(line).expect("物理顺序中的每条记录都必须完整");
        }
    }

    #[tokio::test]
    async fn subprocess_appends_translation_records() {
        let Some(root) = std::env::var_os("ATT_JSONL_TEST_CHILD_ROOT") else {
            return;
        };
        let profile =
            std::env::var("ATT_JSONL_TEST_CHILD_PROFILE").expect("子进程 profile 必须存在");
        let run_id = std::env::var("ATT_JSONL_TEST_CHILD_RUN_ID").expect("子进程 run_id 必须存在");
        let run_id = RunId::from_uuid(Uuid::parse_str(&run_id).expect("子进程 run_id 应合法"));
        let child_config = JsonLinesStreamConfig::new(8, Duration::from_secs(10), 4096, 65_536, 2)
            .expect("子进程日志配置应合法");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            PathBuf::from(root),
            child_config,
            TranslationRunLogContext::new(run_id, "子进程项目", profile),
        )
        .expect("子进程日志根应可启动");
        for index in 0..8 {
            log.append(summary_event(index))
                .await
                .expect("子进程追加应成功");
        }
        finalizer.finalize().await.expect("子进程 worker 应可排空");
    }

    #[test]
    fn separate_processes_share_the_same_complete_physical_record_order() {
        let directory = tempdir().expect("临时目录应可创建");
        let executable = std::env::current_exe().expect("应可定位当前测试进程");
        let spawn = |profile: &str, run_id: &str| {
            std::process::Command::new(&executable)
                .arg("--exact")
                .arg("runtime::json_lines::tests::subprocess_appends_translation_records")
                .arg("--nocapture")
                .env("ATT_JSONL_TEST_CHILD_ROOT", directory.path())
                .env("ATT_JSONL_TEST_CHILD_PROFILE", profile)
                .env("ATT_JSONL_TEST_CHILD_RUN_ID", run_id)
                .spawn()
                .expect("子进程应可启动")
        };
        let mut first = spawn("child-a", "550e8400-e29b-41d4-a716-446655440000");
        let mut second = spawn("child-b", "7c9e6679-7425-40de-944b-e07fc1f90ae7");

        assert!(first.wait().expect("第一子进程应可终结").success());
        assert!(second.wait().expect("第二子进程应可终结").success());

        let bytes =
            fs::read(directory.path().join("translation.jsonl")).expect("共享活动日志应存在");
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 16);
        let mut contexts = std::collections::BTreeMap::new();
        for line in lines {
            TranslationRecord::validate(line).expect("子进程物理顺序中的每条记录都必须完整");
            let wire: TranslationRunCompletedWire =
                serde_json::from_slice(line).expect("子进程记录应是运行汇总 wire");
            *contexts.entry((wire.profile, wire.run_id)).or_insert(0) += 1;
        }
        assert_eq!(
            contexts,
            std::collections::BTreeMap::from([
                (
                    (
                        "child-a".to_owned(),
                        "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                    ),
                    8,
                ),
                (
                    (
                        "child-b".to_owned(),
                        "7c9e6679-7425-40de-944b-e07fc1f90ae7".to_owned(),
                    ),
                    8,
                ),
            ])
        );
    }

    #[tokio::test]
    async fn incomplete_tail_is_truncated_before_next_record() {
        let directory = tempdir().expect("临时目录应可创建");
        let mut existing = serialized_summary(1);
        existing.push(b'\n');
        existing.extend_from_slice(b"{\"recorded_at_utc\":\"cut");
        fs::write(directory.path().join("translation.jsonl"), existing)
            .expect("测试活动文件应可写入");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");

        log.append(summary_event(2))
            .await
            .expect("尾部恢复后应可追加");
        finalizer.finalize().await.expect("worker 应可排空");
        let bytes = fs::read(directory.path().join("translation.jsonl")).expect("活动日志应存在");
        let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        TranslationRecord::validate(lines[0]).expect("首条应保持完整");
        TranslationRecord::validate(lines[1]).expect("新记录应完整");
        assert!(lines[2].is_empty());
    }

    #[tokio::test]
    async fn overlong_incomplete_tail_is_still_recovered_by_lf_boundary() {
        let directory = tempdir().expect("临时目录应可创建");
        fs::write(directory.path().join("translation.jsonl"), vec![b'x'; 2048])
            .expect("测试超长半行应可写入");
        let bounded = JsonLinesStreamConfig::new(4, Duration::from_secs(1), 1024, 4096, 1)
            .expect("测试配置应合法");
        let (log, finalizer) =
            TranslationJsonLinesEventLog::start(directory.path().to_path_buf(), bounded, context())
                .expect("日志根应可启动");

        log.append(summary_event(1))
            .await
            .expect("缺少 LF 的超长尾部应被整体截断");
        finalizer.finalize().await.expect("worker 应可排空");
        let bytes = fs::read(directory.path().join("translation.jsonl")).expect("活动日志应存在");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        TranslationRecord::validate(&bytes[..bytes.len() - 1]).expect("恢复后的唯一记录应完整");
    }

    #[tokio::test]
    async fn overlong_complete_line_is_rejected_instead_of_truncated_as_a_tail() {
        let directory = tempdir().expect("临时目录应可创建");
        let mut complete = vec![b'x'; 2048];
        complete.push(b'\n');
        fs::write(directory.path().join("translation.jsonl"), &complete)
            .expect("测试超长完整行应可写入");
        let bounded = JsonLinesStreamConfig::new(4, Duration::from_secs(1), 1024, 4096, 1)
            .expect("测试配置应合法");
        let (log, finalizer) =
            TranslationJsonLinesEventLog::start(directory.path().to_path_buf(), bounded, context())
                .expect("日志根应可启动");

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "validate_existing",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(
            fs::read(directory.path().join("translation.jsonl")).expect("超长完整行应保持可诊断"),
            complete
        );
    }

    #[tokio::test]
    async fn complete_corrupt_line_blocks_append_without_guessing_repair() {
        let directory = tempdir().expect("临时目录应可创建");
        fs::write(
            directory.path().join("translation.jsonl"),
            b"{\"bad\":true}\n",
        )
        .expect("测试坏行应可写入");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "validate_existing",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(
            fs::read(directory.path().join("translation.jsonl")).expect("坏文件应保持可诊断"),
            b"{\"bad\":true}\n"
        );
    }

    #[tokio::test]
    async fn a_complete_crlf_record_is_rejected_by_the_single_lf_contract() {
        let directory = tempdir().expect("临时目录应可创建");
        let mut existing = serialized_summary(1);
        existing.extend_from_slice(b"\r\n");
        fs::write(directory.path().join("translation.jsonl"), &existing)
            .expect("测试 CRLF 记录应可写入");
        let (log, finalizer) = TranslationJsonLinesEventLog::start(
            directory.path().to_path_buf(),
            config(),
            context(),
        )
        .expect("日志根应可启动");

        assert!(matches!(
            log.append(summary_event(2)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "validate_existing",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert_eq!(
            fs::read(directory.path().join("translation.jsonl")).expect("CRLF 记录应保持可诊断"),
            existing
        );
    }

    #[tokio::test]
    async fn rotates_monotonically_and_retains_only_configured_known_files() {
        let directory = tempdir().expect("临时目录应可创建");
        let small = JsonLinesStreamConfig::new(8, Duration::from_secs(1), 1024, 1024, 2)
            .expect("测试配置应合法");
        fs::write(directory.path().join("translation.notes.jsonl"), b"keep")
            .expect("未知文件应可建立");
        let (log, finalizer) =
            TranslationJsonLinesEventLog::start(directory.path().to_path_buf(), small, context())
                .expect("日志根应可启动");
        for index in 0..12 {
            log.append(summary_event(index)).await.expect("追加应成功");
        }
        finalizer.finalize().await.expect("worker 应可排空");

        let mut rotations = fs::read_dir(directory.path())
            .expect("日志根应可枚举")
            .filter_map(Result::ok)
            .filter(|entry| rotation_sequence(TRANSLATION_STEM, &entry.file_name()).is_some())
            .collect::<Vec<_>>();
        rotations.sort_by_key(|entry| entry.file_name());
        assert!(!rotations.is_empty());
        assert!(rotations.len() <= 2);
        assert!(directory.path().join("translation.notes.jsonl").exists());
    }

    #[tokio::test]
    async fn oversized_record_is_definitively_not_persisted() {
        let directory = tempdir().expect("临时目录应可创建");
        let tiny = JsonLinesStreamConfig::new(1, Duration::from_secs(1), 64, 64, 0)
            .expect("测试配置应合法");
        let (log, finalizer) =
            TranslationJsonLinesEventLog::start(directory.path().to_path_buf(), tiny, context())
                .expect("日志根应可启动");

        assert!(matches!(
            log.append(summary_event(1)).await,
            Err(JsonLinesAppendError::NotPersisted {
                stage: "record_limit",
                ..
            })
        ));
        finalizer.finalize().await.expect("worker 应可排空");
        assert!(!directory.path().join("translation.jsonl").exists());
    }
}
