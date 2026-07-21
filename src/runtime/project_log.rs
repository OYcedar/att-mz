//! 普通项目日志的最佳努力运行时。
//!
//! 该模块只承担可观测性：生产者同步地尝试把已经类型化的事件放入有界队列，
//! 独立 worker 批量写入 JSONL。启动、排队、写入、轮转或关闭失败只会进入健康
//! 状态，永远不会作为业务错误返回。

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use async_channel::{Receiver, Sender, TryRecvError, TrySendError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::user_text::sanitize_user_text;

use super::windows::{
    FileIdentity, PinnedPath, ReusableExclusiveFileLock, WindowsFsError,
    create_directories_without_reparse, delete_regular_file_if_identity,
    open_read_write_file_without_reparse, pin_path_without_reparse, rename_without_replace,
};

const ACTIVE_FILE_NAME: &str = "att.log.jsonl";
const LOCK_FILE_NAME: &str = ".att.log.lock";
const ROTATED_PREFIX: &str = "att.log.";
const ROTATED_SUFFIX: &str = ".jsonl";

/// 项目日志的过滤级别。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectLogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl ProjectLogLevel {
    const fn priority(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }

    const fn allows(self, event: Self) -> bool {
        event.priority() <= self.priority()
    }
}

impl FromStr for ProjectLogLevel {
    type Err = ProjectLogLevelParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            _ => Err(ProjectLogLevelParseError),
        }
    }
}

/// 日志级别文本不属于当前闭集。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogLevelParseError;

impl fmt::Display for ProjectLogLevelParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("项目日志级别必须是 error、warn、info 或 debug")
    }
}

impl Error for ProjectLogLevelParseError {}

/// 尚未校验的项目日志配置值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogConfigInput {
    pub(crate) level: ProjectLogLevel,
    pub(crate) queue_capacity: usize,
    pub(crate) batch_max_records: usize,
    pub(crate) batch_max_bytes: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) lock_timeout: Duration,
    pub(crate) max_record_bytes: usize,
    pub(crate) max_file_bytes: u64,
    pub(crate) retained_rotated_files: usize,
}

/// 已经建立全部资源边界的项目日志配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogConfig {
    level: ProjectLogLevel,
    queue_capacity: usize,
    batch_max_records: usize,
    batch_max_bytes: usize,
    flush_interval: Duration,
    shutdown_timeout: Duration,
    lock_timeout: Duration,
    max_record_bytes: usize,
    max_file_bytes: u64,
    retained_rotated_files: usize,
    #[cfg(test)]
    test_faults: ProjectLogTestFaults,
}

/// 只在模块测试中启用的确定性故障注入点。
///
/// 这些开关随单个运行时配置传递，不使用进程级全局状态，因此并行测试不会互相
/// 污染；生产构建中该类型和全部分支都会被移除。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProjectLogTestFaults {
    partial_write: bool,
    rotation: bool,
    retention: bool,
    shutdown_delay: Duration,
}

impl TryFrom<ProjectLogConfigInput> for ProjectLogConfig {
    type Error = ProjectLogConfigurationError;

    fn try_from(input: ProjectLogConfigInput) -> Result<Self, Self::Error> {
        if input.queue_capacity == 0 {
            return Err(ProjectLogConfigurationError::ZeroQueueCapacity);
        }
        if input.batch_max_records == 0 {
            return Err(ProjectLogConfigurationError::ZeroBatchMaxRecords);
        }
        if input.batch_max_bytes == 0 {
            return Err(ProjectLogConfigurationError::ZeroBatchMaxBytes);
        }
        if input.flush_interval.is_zero() {
            return Err(ProjectLogConfigurationError::ZeroFlushInterval);
        }
        if input.shutdown_timeout.is_zero() {
            return Err(ProjectLogConfigurationError::ZeroShutdownTimeout);
        }
        if input.lock_timeout.is_zero() {
            return Err(ProjectLogConfigurationError::ZeroLockTimeout);
        }
        if input.max_record_bytes == 0 {
            return Err(ProjectLogConfigurationError::ZeroMaxRecordBytes);
        }
        if input.max_file_bytes == 0 {
            return Err(ProjectLogConfigurationError::ZeroMaxFileBytes);
        }
        let record_limit = u64::try_from(input.max_record_bytes)
            .map_err(|_| ProjectLogConfigurationError::RecordLimitDoesNotFitU64)?;
        if record_limit > input.max_file_bytes {
            return Err(ProjectLogConfigurationError::RecordExceedsFileLimit {
                max_record_bytes: input.max_record_bytes,
                max_file_bytes: input.max_file_bytes,
            });
        }
        Ok(Self {
            level: input.level,
            queue_capacity: input.queue_capacity,
            batch_max_records: input.batch_max_records,
            batch_max_bytes: input.batch_max_bytes,
            flush_interval: input.flush_interval,
            shutdown_timeout: input.shutdown_timeout,
            lock_timeout: input.lock_timeout,
            max_record_bytes: input.max_record_bytes,
            max_file_bytes: input.max_file_bytes,
            retained_rotated_files: input.retained_rotated_files,
            #[cfg(test)]
            test_faults: ProjectLogTestFaults::default(),
        })
    }
}

/// 项目日志配置没有建立有效的有界资源策略。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectLogConfigurationError {
    ZeroQueueCapacity,
    ZeroBatchMaxRecords,
    ZeroBatchMaxBytes,
    ZeroFlushInterval,
    ZeroShutdownTimeout,
    ZeroLockTimeout,
    ZeroMaxRecordBytes,
    ZeroMaxFileBytes,
    RecordLimitDoesNotFitU64,
    RecordExceedsFileLimit {
        max_record_bytes: usize,
        max_file_bytes: u64,
    },
}

impl fmt::Display for ProjectLogConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroQueueCapacity => formatter.write_str("项目日志队列容量必须大于零"),
            Self::ZeroBatchMaxRecords => formatter.write_str("项目日志批次记录数必须大于零"),
            Self::ZeroBatchMaxBytes => formatter.write_str("项目日志批次字节数必须大于零"),
            Self::ZeroFlushInterval => formatter.write_str("项目日志刷新间隔必须大于零"),
            Self::ZeroShutdownTimeout => formatter.write_str("项目日志关闭等待上限必须大于零"),
            Self::ZeroLockTimeout => formatter.write_str("项目日志文件锁等待上限必须大于零"),
            Self::ZeroMaxRecordBytes => formatter.write_str("项目日志单条记录上限必须大于零"),
            Self::ZeroMaxFileBytes => formatter.write_str("项目日志活动文件上限必须大于零"),
            Self::RecordLimitDoesNotFitU64 => {
                formatter.write_str("项目日志单条记录上限无法表示为文件长度")
            }
            Self::RecordExceedsFileLimit {
                max_record_bytes,
                max_file_bytes,
            } => write!(
                formatter,
                "项目日志单条记录上限 {max_record_bytes} 大于活动文件上限 {max_file_bytes}"
            ),
        }
    }
}

impl Error for ProjectLogConfigurationError {}

/// 稳定的项目日志事件代码。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ProjectLogCode {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.finished")]
    RunFinished,
    #[serde(rename = "run.cancel_requested")]
    CancellationRequested,
    #[serde(rename = "run.safe_stop_finished")]
    SafeStopFinished,
    #[serde(rename = "run_plan.resolved")]
    RunPlanResolved,
    #[serde(rename = "run_plan.saved")]
    RunPlanSaved,
    #[serde(rename = "run_plan.save_failed")]
    RunPlanSaveFailed,
    #[serde(rename = "run_plan.save_outcome_unknown")]
    RunPlanSaveOutcomeUnknown,
    #[serde(rename = "run_plan.saved_finalization_failed")]
    RunPlanSavedFinalizationFailed,
    #[serde(rename = "phase.started")]
    PhaseStarted,
    #[serde(rename = "phase.finished")]
    PhaseFinished,
    #[serde(rename = "retry.summary")]
    RetrySummary,
    #[serde(rename = "work.none")]
    NoWork,
    #[serde(rename = "result.partial")]
    PartialResult,
    #[serde(rename = "publication.started")]
    PublicationStarted,
    #[serde(rename = "publication.finished")]
    PublicationFinished,
    #[serde(rename = "task.started")]
    TaskStarted,
    #[serde(rename = "task.finished")]
    TaskFinished,
}

/// 运行方案字段的实际来源。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogValueSource {
    Explicit,
    ProjectState,
    ProductDefault,
}

/// 进度的绝对量；项目日志不会从增量事件自行推导状态。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogAmount {
    Indeterminate,
    Determinate { completed: u64, total: u64 },
}

/// 一次运行的终态。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogRunOutcome {
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

/// 一个翻译任务的业务终态。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogTaskOutcome {
    Complete,
    Partial,
    Unavailable,
    Failed,
}

/// 目录发布的业务终态。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectLogPublicationOutcome {
    Published,
    NotPublished,
    RecoveryRequired,
    OutcomeUnknown,
}

/// 项目日志允许持久化的结构化业务事实。
///
/// 该闭集刻意不提供任意 JSON 或模型正文载荷，避免调用方把 prompt、原文、译文、
/// API 凭据或 Header 误塞入日志。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectLogPayload {
    None,
    Run {
        outcome: Option<ProjectLogRunOutcome>,
    },
    RunPlan {
        source: ProjectLogValueSource,
        lua_source: Option<ProjectLogValueSource>,
        selections: Vec<String>,
        lua_enabled: Option<bool>,
    },
    Phase {
        phase: String,
        amount: ProjectLogAmount,
    },
    RetrySummary {
        attempted: u64,
        recovered: u64,
        exhausted: u64,
    },
    NoWork {
        reason_code: String,
    },
    ResultSummary {
        complete: u64,
        partial: u64,
        unavailable: u64,
        manual_review: u64,
    },
    Publication {
        outcome: ProjectLogPublicationOutcome,
        published_items: Option<u64>,
    },
    Task {
        ordinal: u64,
        total: u64,
        outcome: Option<ProjectLogTaskOutcome>,
        attempts: Option<u64>,
    },
    Cancellation {
        confirmed: u64,
        total: Option<u64>,
    },
}

/// 一条事件共有的、非秘密运行上下文。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectLogContext {
    run_id: Option<String>,
    engine: Option<String>,
    project: Option<String>,
    command: Option<String>,
    profile: Option<String>,
    locale: String,
}

impl ProjectLogContext {
    pub(crate) fn new(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            ..Self::default()
        }
    }

    pub(crate) fn with_run_id(mut self, value: impl Into<String>) -> Self {
        self.run_id = Some(value.into());
        self
    }

    pub(crate) fn with_engine(mut self, value: impl Into<String>) -> Self {
        self.engine = Some(value.into());
        self
    }

    pub(crate) fn with_project(mut self, value: impl Into<String>) -> Self {
        self.project = Some(value.into());
        self
    }

    pub(crate) fn with_command(mut self, value: impl Into<String>) -> Self {
        self.command = Some(value.into());
        self
    }

    pub(crate) fn with_profile(mut self, value: impl Into<String>) -> Self {
        self.profile = Some(value.into());
        self
    }
}

/// 调用方提交给项目日志的一条类型化事件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogEvent {
    level: ProjectLogLevel,
    code: ProjectLogCode,
    context: ProjectLogContext,
    message: String,
    payload: ProjectLogPayload,
}

impl ProjectLogEvent {
    pub(crate) fn new(
        level: ProjectLogLevel,
        code: ProjectLogCode,
        context: ProjectLogContext,
        message: impl Into<String>,
        payload: ProjectLogPayload,
    ) -> Self {
        Self {
            level,
            code,
            context,
            message: message.into(),
            payload,
        }
    }
}

/// 业务层唯一依赖的不可失败项目日志入口。
pub(crate) trait ProjectLog: Send + Sync {
    fn emit(&self, event: ProjectLogEvent);
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectLogRecord {
    time: String,
    level: ProjectLogLevel,
    code: ProjectLogCode,
    pid: u32,
    run_id: Option<String>,
    sequence: u64,
    engine: Option<String>,
    project: Option<String>,
    command: Option<String>,
    profile: Option<String>,
    locale: String,
    message: String,
    payload: ProjectLogPayload,
}

struct QueuedProjectLogEvent {
    emitted_at: OffsetDateTime,
    event: ProjectLogEvent,
}

struct LoggerInner {
    sender: Option<Sender<QueuedProjectLogEvent>>,
    health: Arc<ProjectLogHealth>,
    level: ProjectLogLevel,
}

/// 可克隆的同步项目日志句柄。
#[derive(Clone)]
pub(crate) struct ProjectLogger {
    inner: Arc<LoggerInner>,
}

impl ProjectLogger {
    fn no_op(health: Arc<ProjectLogHealth>, level: ProjectLogLevel) -> Self {
        Self {
            inner: Arc::new(LoggerInner {
                sender: None,
                health,
                level,
            }),
        }
    }

    pub(crate) fn health(&self) -> ProjectLogHealthSnapshot {
        self.inner.health.snapshot()
    }

    /// 领取本次进程唯一一次日志降级警告资格。
    pub(crate) fn take_warning(&self) -> Option<ProjectLogWarning> {
        self.inner.health.take_warning()
    }
}

impl ProjectLog for ProjectLogger {
    fn emit(&self, event: ProjectLogEvent) {
        if !self.inner.level.allows(event.level) {
            return;
        }
        let Some(sender) = &self.inner.sender else {
            self.inner.health.add_dropped_records(1);
            return;
        };
        match sender.try_send(QueuedProjectLogEvent {
            emitted_at: OffsetDateTime::now_utc(),
            event,
        }) {
            Ok(()) => self.inner.health.add_accepted_records(1),
            Err(TrySendError::Full(_)) => {
                self.inner.health.record_queue_full();
                self.inner.health.add_dropped_records(1);
            }
            Err(TrySendError::Closed(_)) => {
                self.inner.health.record_queue_closed();
                self.inner.health.add_dropped_records(1);
            }
        }
    }
}

/// 项目日志运行期健康状态的稳定快照。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectLogHealthSnapshot {
    pub(crate) accepted_records: u64,
    pub(crate) persisted_records: u64,
    pub(crate) dropped_records: u64,
    pub(crate) startup_failures: u64,
    pub(crate) queue_full: u64,
    pub(crate) queue_closed: u64,
    pub(crate) serialization_failures: u64,
    pub(crate) oversized_records: u64,
    pub(crate) lock_timeouts: u64,
    pub(crate) lock_failures: u64,
    pub(crate) recovered_incomplete_tails: u64,
    pub(crate) malformed_records: u64,
    pub(crate) write_failures: u64,
    pub(crate) rotation_failures: u64,
    pub(crate) retention_failures: u64,
    pub(crate) worker_panics: u64,
    pub(crate) shutdown_timeouts: u64,
}

impl ProjectLogHealthSnapshot {
    pub(crate) const fn is_degraded(self) -> bool {
        self.startup_failures > 0
            || self.queue_full > 0
            || self.queue_closed > 0
            || self.serialization_failures > 0
            || self.oversized_records > 0
            || self.lock_timeouts > 0
            || self.lock_failures > 0
            || self.recovered_incomplete_tails > 0
            || self.malformed_records > 0
            || self.write_failures > 0
            || self.rotation_failures > 0
            || self.retention_failures > 0
            || self.worker_panics > 0
            || self.shutdown_timeouts > 0
    }
}

/// UI 可以本地化渲染的一次性日志降级警告事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectLogWarning {
    pub(crate) health: ProjectLogHealthSnapshot,
}

#[derive(Default)]
struct ProjectLogHealth {
    accepted_records: AtomicU64,
    persisted_records: AtomicU64,
    dropped_records: AtomicU64,
    startup_failures: AtomicU64,
    queue_full: AtomicU64,
    queue_closed: AtomicU64,
    serialization_failures: AtomicU64,
    oversized_records: AtomicU64,
    lock_timeouts: AtomicU64,
    lock_failures: AtomicU64,
    recovered_incomplete_tails: AtomicU64,
    malformed_records: AtomicU64,
    write_failures: AtomicU64,
    rotation_failures: AtomicU64,
    retention_failures: AtomicU64,
    worker_panics: AtomicU64,
    shutdown_timeouts: AtomicU64,
    warning_claimed: AtomicBool,
}

impl ProjectLogHealth {
    fn increment(counter: &AtomicU64, amount: u64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }

    fn add_accepted_records(&self, amount: u64) {
        Self::increment(&self.accepted_records, amount);
    }

    fn add_persisted_records(&self, amount: u64) {
        Self::increment(&self.persisted_records, amount);
    }

    fn add_dropped_records(&self, amount: u64) {
        Self::increment(&self.dropped_records, amount);
    }

    fn record_startup_failure(&self) {
        Self::increment(&self.startup_failures, 1);
    }

    fn record_queue_full(&self) {
        Self::increment(&self.queue_full, 1);
    }

    fn record_queue_closed(&self) {
        Self::increment(&self.queue_closed, 1);
    }

    fn record_serialization_failure(&self) {
        Self::increment(&self.serialization_failures, 1);
    }

    fn record_oversized_record(&self) {
        Self::increment(&self.oversized_records, 1);
    }

    fn record_lock_failure(&self, timeout: bool) {
        if timeout {
            Self::increment(&self.lock_timeouts, 1);
        } else {
            Self::increment(&self.lock_failures, 1);
        }
    }

    fn record_recovered_incomplete_tail(&self) {
        Self::increment(&self.recovered_incomplete_tails, 1);
    }

    fn record_malformed_record(&self) {
        Self::increment(&self.malformed_records, 1);
    }

    fn record_write_failure(&self) {
        Self::increment(&self.write_failures, 1);
    }

    fn record_rotation_failure(&self) {
        Self::increment(&self.rotation_failures, 1);
    }

    fn record_retention_failure(&self) {
        Self::increment(&self.retention_failures, 1);
    }

    fn record_worker_panic(&self) {
        Self::increment(&self.worker_panics, 1);
    }

    fn record_shutdown_timeout(&self) {
        Self::increment(&self.shutdown_timeouts, 1);
    }

    fn snapshot(&self) -> ProjectLogHealthSnapshot {
        ProjectLogHealthSnapshot {
            accepted_records: self.accepted_records.load(Ordering::Relaxed),
            persisted_records: self.persisted_records.load(Ordering::Relaxed),
            dropped_records: self.dropped_records.load(Ordering::Relaxed),
            startup_failures: self.startup_failures.load(Ordering::Relaxed),
            queue_full: self.queue_full.load(Ordering::Relaxed),
            queue_closed: self.queue_closed.load(Ordering::Relaxed),
            serialization_failures: self.serialization_failures.load(Ordering::Relaxed),
            oversized_records: self.oversized_records.load(Ordering::Relaxed),
            lock_timeouts: self.lock_timeouts.load(Ordering::Relaxed),
            lock_failures: self.lock_failures.load(Ordering::Relaxed),
            recovered_incomplete_tails: self.recovered_incomplete_tails.load(Ordering::Relaxed),
            malformed_records: self.malformed_records.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
            rotation_failures: self.rotation_failures.load(Ordering::Relaxed),
            retention_failures: self.retention_failures.load(Ordering::Relaxed),
            worker_panics: self.worker_panics.load(Ordering::Relaxed),
            shutdown_timeouts: self.shutdown_timeouts.load(Ordering::Relaxed),
        }
    }

    fn take_warning(&self) -> Option<ProjectLogWarning> {
        let health = self.snapshot();
        if !health.is_degraded() {
            return None;
        }
        self.warning_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ProjectLogWarning { health })
    }
}

struct StreamPaths {
    _pinned_root: PinnedPath,
    root: PathBuf,
    active: PathBuf,
    lock: PathBuf,
}

impl StreamPaths {
    fn new(root: PathBuf, pinned_root: PinnedPath) -> Self {
        Self {
            active: root.join(ACTIVE_FILE_NAME),
            lock: root.join(LOCK_FILE_NAME),
            _pinned_root: pinned_root,
            root,
        }
    }

    fn rotated(&self, sequence: u64) -> PathBuf {
        self.root
            .join(format!("{ROTATED_PREFIX}{sequence:020}{ROTATED_SUFFIX}"))
    }
}

/// 唯一拥有日志 worker 关闭权的运行时。
pub(crate) struct ProjectLogRuntime {
    logger: ProjectLogger,
    completion: Option<mpsc::Receiver<()>>,
    worker: Option<JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl ProjectLogRuntime {
    pub(crate) fn logger(&self) -> ProjectLogger {
        self.logger.clone()
    }

    /// 停止接纳新事件并在配置上限内等待已接纳批次。
    ///
    /// 超时、worker panic 或最终刷盘失败只反映在返回的健康快照中。
    pub(crate) fn shutdown(mut self) -> ProjectLogHealthSnapshot {
        self.shutdown_inner();
        self.logger.health()
    }

    fn shutdown_inner(&mut self) {
        if let Some(sender) = &self.logger.inner.sender {
            sender.close();
        }
        let Some(completion) = self.completion.take() else {
            return;
        };
        match completion.recv_timeout(self.shutdown_timeout) {
            Ok(()) => {
                if let Some(worker) = self.worker.take()
                    && worker.join().is_err()
                {
                    self.logger.inner.health.record_worker_panic();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.logger.inner.health.record_shutdown_timeout();
                self.worker.take();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.logger.inner.health.record_worker_panic();
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
        }
    }
}

impl Drop for ProjectLogRuntime {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// 启动普通项目日志；任何启动失败都会返回已经降级的 no-op 运行时。
pub(crate) fn start_project_log(root: PathBuf, config: ProjectLogConfig) -> ProjectLogRuntime {
    let health = Arc::new(ProjectLogHealth::default());
    let pinned_root = match create_directories_without_reparse(&root) {
        Ok(root) => root,
        Err(_) => {
            health.record_startup_failure();
            return no_op_runtime(health, config);
        }
    };
    if !matches!(pinned_root.metadata(), Ok(metadata) if metadata.is_dir()) {
        health.record_startup_failure();
        return no_op_runtime(health, config);
    }
    let root = pinned_root.resolved_path().to_path_buf();
    let paths = StreamPaths::new(root, pinned_root);
    let (sender, receiver) = async_channel::bounded(config.queue_capacity);
    let logger = ProjectLogger {
        inner: Arc::new(LoggerInner {
            sender: Some(sender),
            health: Arc::clone(&health),
            level: config.level,
        }),
    };
    let (completion_sender, completion) = mpsc::sync_channel(1);
    let worker_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            health.record_startup_failure();
            return no_op_runtime(health, config);
        }
    };
    let worker_health = Arc::clone(&health);
    let worker = match thread::Builder::new()
        .name("att-project-log".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                worker_runtime.block_on(run_worker(
                    receiver,
                    paths,
                    config,
                    Arc::clone(&worker_health),
                ));
            }));
            if result.is_err() {
                worker_health.record_worker_panic();
            }
            let _ = completion_sender.send(());
        }) {
        Ok(worker) => worker,
        Err(_) => {
            health.record_startup_failure();
            return no_op_runtime(health, config);
        }
    };
    ProjectLogRuntime {
        logger,
        completion: Some(completion),
        worker: Some(worker),
        shutdown_timeout: config.shutdown_timeout,
    }
}

fn no_op_runtime(health: Arc<ProjectLogHealth>, config: ProjectLogConfig) -> ProjectLogRuntime {
    ProjectLogRuntime {
        logger: ProjectLogger::no_op(health, config.level),
        completion: None,
        worker: None,
        shutdown_timeout: config.shutdown_timeout,
    }
}

async fn run_worker(
    receiver: Receiver<QueuedProjectLogEvent>,
    paths: StreamPaths,
    config: ProjectLogConfig,
    health: Arc<ProjectLogHealth>,
) {
    let mut state = WorkerState::default();
    let mut pending = None;
    let mut sequence = 0_u64;
    loop {
        let first = match pending.take() {
            Some(record) => record,
            None => match receive_serialized(&receiver, config, &health, &mut sequence).await {
                Some(record) => record,
                None => break,
            },
        };
        let mut bytes = first.len();
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + config.flush_interval;
        while batch.len() < config.batch_max_records {
            let next_event = match receiver.try_recv() {
                Ok(event) => Some(event),
                Err(TryRecvError::Closed) => None,
                Err(TryRecvError::Empty) => {
                    match tokio::time::timeout_at(deadline, receiver.recv()).await {
                        Ok(Ok(event)) => Some(event),
                        Ok(Err(_)) | Err(_) => None,
                    }
                }
            };
            let Some(next_event) = next_event else {
                break;
            };
            let Some(record) = serialize_event(next_event, config, &health, &mut sequence) else {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                continue;
            };
            if !batch.is_empty() && bytes.saturating_add(record.len()) > config.batch_max_bytes {
                pending = Some(record);
                break;
            }
            bytes = bytes.saturating_add(record.len());
            batch.push(record);
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        persist_batch(&batch, &paths, config, &mut state, &health);
    }
    sync_active_on_shutdown(&paths, config, &mut state, &health);
}

async fn receive_serialized(
    receiver: &Receiver<QueuedProjectLogEvent>,
    config: ProjectLogConfig,
    health: &ProjectLogHealth,
    sequence: &mut u64,
) -> Option<Vec<u8>> {
    loop {
        let event = receiver.recv().await.ok()?;
        if let Some(bytes) = serialize_event(event, config, health, sequence) {
            return Some(bytes);
        }
    }
}

fn serialize_event(
    queued: QueuedProjectLogEvent,
    config: ProjectLogConfig,
    health: &ProjectLogHealth,
    sequence: &mut u64,
) -> Option<Vec<u8>> {
    let Some(record_sequence) = sequence.checked_add(1) else {
        health.record_serialization_failure();
        health.add_dropped_records(1);
        return None;
    };
    let ProjectLogEvent {
        level,
        code,
        context,
        message,
        payload,
    } = queued.event;
    let record = ProjectLogRecord {
        time: recorded_at_utc(queued.emitted_at),
        level,
        code,
        pid: std::process::id(),
        run_id: sanitize_optional_text(context.run_id),
        sequence: record_sequence,
        engine: sanitize_optional_text(context.engine),
        project: sanitize_optional_text(context.project),
        command: sanitize_optional_text(context.command),
        profile: sanitize_optional_text(context.profile),
        locale: sanitize_user_text(&context.locale),
        message: sanitize_user_text(&message),
        payload: sanitize_payload_text(payload),
    };
    let mut bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(_) => {
            health.record_serialization_failure();
            health.add_dropped_records(1);
            return None;
        }
    };
    bytes.push(b'\n');
    if bytes.len() > config.max_record_bytes {
        health.record_oversized_record();
        health.add_dropped_records(1);
        return None;
    }
    *sequence = record_sequence;
    Some(bytes)
}

fn sanitize_optional_text(value: Option<String>) -> Option<String> {
    value.map(|value| sanitize_user_text(&value))
}

fn sanitize_payload_text(payload: ProjectLogPayload) -> ProjectLogPayload {
    match payload {
        ProjectLogPayload::RunPlan {
            source,
            lua_source,
            selections,
            lua_enabled,
        } => ProjectLogPayload::RunPlan {
            source,
            lua_source,
            selections: selections
                .into_iter()
                .map(|selection| sanitize_user_text(&selection))
                .collect(),
            lua_enabled,
        },
        ProjectLogPayload::Phase { phase, amount } => ProjectLogPayload::Phase {
            phase: sanitize_user_text(&phase),
            amount,
        },
        ProjectLogPayload::NoWork { reason_code } => ProjectLogPayload::NoWork {
            reason_code: sanitize_user_text(&reason_code),
        },
        payload => payload,
    }
}

#[derive(Default)]
struct WorkerState {
    validation: ActiveValidationCursor,
    validation_line: Vec<u8>,
    lock_file: Option<ReusableExclusiveFileLock>,
    discard_lock_file: bool,
}

#[derive(Default)]
struct ActiveValidationCursor {
    identity: Option<FileIdentity>,
    validated_length: u64,
    modified: Option<SystemTime>,
}

impl ActiveValidationCursor {
    fn reset(&mut self) {
        self.identity = None;
        self.validated_length = 0;
        self.modified = None;
    }
}

fn persist_batch(
    batch: &[Vec<u8>],
    paths: &StreamPaths,
    config: ProjectLogConfig,
    state: &mut WorkerState,
    health: &ProjectLogHealth,
) {
    let total = u64::try_from(batch.len()).unwrap_or(u64::MAX);
    let WorkerState {
        validation,
        validation_line,
        lock_file,
        discard_lock_file,
    } = state;
    if *discard_lock_file {
        *lock_file = None;
        *discard_lock_file = false;
    }
    if lock_file.is_none() {
        match ReusableExclusiveFileLock::open(&paths.lock) {
            Ok(lock) => *lock_file = Some(lock),
            Err(error) => {
                health.record_lock_failure(matches!(error, WindowsFsError::LockTimeout { .. }));
                health.add_dropped_records(total);
                return;
            }
        }
    }
    let lock = lock_file
        .as_mut()
        .expect("项目日志 worker 必须持有已经成功打开的锁文件");
    let _guard = match lock.lock(&paths.lock, config.lock_timeout) {
        Ok(guard) => guard,
        Err(error) => {
            health.record_lock_failure(matches!(error, WindowsFsError::LockTimeout { .. }));
            health.add_dropped_records(total);
            *discard_lock_file = true;
            return;
        }
    };
    let recovered =
        match recover_and_validate_active(paths, config, validation, validation_line, health) {
            Ok(recovered) => recovered,
            Err(ActiveRecoveryError::Malformed) => {
                health.record_malformed_record();
                health.add_dropped_records(total);
                validation.reset();
                return;
            }
            Err(ActiveRecoveryError::Io) => {
                health.record_write_failure();
                health.add_dropped_records(total);
                validation.reset();
                return;
            }
        };
    let mut active = recovered.file;
    let mut current_size = recovered.length;
    let mut persisted = 0_u64;
    let mut rotated = false;
    for record in batch {
        let record_length = u64::try_from(record.len()).expect("受检日志记录长度必须可表示为 u64");
        if current_size > 0 && current_size.saturating_add(record_length) > config.max_file_bytes {
            if active.file_mut().flush().is_err() || active.file().sync_data().is_err() {
                health.record_write_failure();
                break;
            }
            drop(active);
            if rotate_active_best_effort(paths, config).is_err() {
                health.record_rotation_failure();
                validation.reset();
                health.add_persisted_records(persisted);
                health.add_dropped_records(total.saturating_sub(persisted));
                return;
            }
            rotated = true;
            validation.reset();
            active = match open_read_write_file_without_reparse(&paths.active, true) {
                Ok(active) => active,
                Err(_) => {
                    health.record_write_failure();
                    health.add_persisted_records(persisted);
                    health.add_dropped_records(total.saturating_sub(persisted));
                    return;
                }
            };
            current_size = 0;
        }
        if active.file_mut().seek(SeekFrom::End(0)).is_err()
            || write_record_best_effort(&mut active, record, config).is_err()
        {
            health.record_write_failure();
            validation.reset();
            break;
        }
        current_size = current_size.saturating_add(record_length);
        persisted = persisted.saturating_add(1);
    }
    if active.file_mut().flush().is_err() {
        health.record_write_failure();
        validation.reset();
    } else {
        validation.identity = FileIdentity::of(active.file(), &paths.active).ok();
        validation.modified = active
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        validation.validated_length = if validation.identity.is_some() {
            current_size
        } else {
            0
        };
    }
    health.add_persisted_records(persisted);
    health.add_dropped_records(total.saturating_sub(persisted));
    if rotated
        && maintain_retention_best_effort(paths, config.retained_rotated_files, config).is_err()
    {
        health.record_retention_failure();
    }
}

fn write_record_best_effort(
    active: &mut PinnedPath,
    record: &[u8],
    _config: ProjectLogConfig,
) -> io::Result<()> {
    #[cfg(test)]
    if _config.test_faults.partial_write {
        let partial_length = (record.len() / 2).max(1);
        active.file_mut().write_all(&record[..partial_length])?;
        return Err(io::Error::other("测试注入的日志部分写故障"));
    }
    active.file_mut().write_all(record)
}

fn rotate_active_best_effort(paths: &StreamPaths, _config: ProjectLogConfig) -> Result<(), ()> {
    #[cfg(test)]
    if _config.test_faults.rotation {
        return Err(());
    }
    rotate_active(paths)
}

fn maintain_retention_best_effort(
    paths: &StreamPaths,
    retained: usize,
    _config: ProjectLogConfig,
) -> Result<(), ()> {
    #[cfg(test)]
    if _config.test_faults.retention {
        return Err(());
    }
    maintain_retention(paths, retained)
}

fn sync_active_on_shutdown(
    paths: &StreamPaths,
    config: ProjectLogConfig,
    state: &mut WorkerState,
    health: &ProjectLogHealth,
) {
    #[cfg(test)]
    if !config.test_faults.shutdown_delay.is_zero() {
        thread::sleep(config.test_faults.shutdown_delay);
    }
    if state.discard_lock_file {
        state.lock_file = None;
    }
    if state.lock_file.is_none() {
        state.lock_file = ReusableExclusiveFileLock::open(&paths.lock).ok();
    }
    let Some(lock) = state.lock_file.as_mut() else {
        health.record_lock_failure(false);
        return;
    };
    let _guard = match lock.lock(&paths.lock, config.lock_timeout) {
        Ok(guard) => guard,
        Err(error) => {
            health.record_lock_failure(matches!(error, WindowsFsError::LockTimeout { .. }));
            return;
        }
    };
    match open_read_write_file_without_reparse(&paths.active, false) {
        Ok(active) => {
            if active.file().sync_data().is_err() {
                health.record_write_failure();
            }
        }
        Err(_) if health.snapshot().accepted_records == 0 => {}
        Err(_) => health.record_write_failure(),
    }
}

struct RecoveredActive {
    file: PinnedPath,
    length: u64,
}

enum ActiveRecoveryError {
    Malformed,
    Io,
}

fn recover_and_validate_active(
    paths: &StreamPaths,
    config: ProjectLogConfig,
    validation: &mut ActiveValidationCursor,
    line: &mut Vec<u8>,
    health: &ProjectLogHealth,
) -> Result<RecoveredActive, ActiveRecoveryError> {
    let mut file = open_read_write_file_without_reparse(&paths.active, true)
        .map_err(|_| ActiveRecoveryError::Io)?;
    let identity = FileIdentity::of(file.file(), &paths.active).ok();
    let metadata = file.metadata().map_err(|_| ActiveRecoveryError::Io)?;
    let file_length = metadata.len();
    let modified = metadata.modified().ok();
    let start = if identity.is_some()
        && identity == validation.identity
        && validation.validated_length == file_length
        && modified.is_some()
        && modified == validation.modified
    {
        validation.validated_length
    } else {
        0
    };
    file.file_mut()
        .seek(SeekFrom::Start(start))
        .map_err(|_| ActiveRecoveryError::Io)?;
    let mut valid_length = start;
    let mut incomplete_tail = false;
    {
        let mut reader = BufReader::new(file.file_mut());
        loop {
            line.clear();
            let max_read = u64::try_from(config.max_record_bytes)
                .expect("项目日志配置已确认记录上限可表示为 u64")
                .saturating_add(1);
            let read = reader
                .by_ref()
                .take(max_read)
                .read_until(b'\n', line)
                .map_err(|_| ActiveRecoveryError::Io)?;
            if read == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                if line.len() > config.max_record_bytes
                    && remaining_line_contains_lf(&mut reader)
                        .map_err(|_| ActiveRecoveryError::Io)?
                {
                    return Err(ActiveRecoveryError::Malformed);
                }
                incomplete_tail = true;
                break;
            }
            if line.len() > config.max_record_bytes
                || serde_json::from_slice::<ProjectLogRecord>(&line[..line.len() - 1]).is_err()
            {
                return Err(ActiveRecoveryError::Malformed);
            }
            valid_length = valid_length
                .checked_add(u64::try_from(line.len()).expect("日志记录长度必须可表示为 u64"))
                .ok_or(ActiveRecoveryError::Malformed)?;
        }
    }
    if incomplete_tail {
        file.file()
            .set_len(valid_length)
            .map_err(|_| ActiveRecoveryError::Io)?;
        file.file()
            .sync_data()
            .map_err(|_| ActiveRecoveryError::Io)?;
        health.record_recovered_incomplete_tail();
    }
    validation.identity = identity;
    validation.validated_length = valid_length;
    validation.modified = if incomplete_tail { None } else { modified };
    Ok(RecoveredActive {
        file,
        length: valid_length,
    })
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

fn rotate_active(paths: &StreamPaths) -> Result<(), ()> {
    let sequence = next_rotation_sequence(paths).ok_or(())?;
    let rotated = paths.rotated(sequence);
    rename_without_replace(&paths.active, &rotated).map_err(|_| ())?;
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&paths.active)
    {
        Ok(_) => Ok(()),
        Err(_) => {
            let _ = rename_without_replace(&rotated, &paths.active);
            Err(())
        }
    }
}

fn next_rotation_sequence(paths: &StreamPaths) -> Option<u64> {
    scan_rotation_entries(paths)
        .ok()?
        .into_iter()
        .map(|entry| entry.sequence)
        .max()
        .unwrap_or(0)
        .checked_add(1)
}

fn maintain_retention(paths: &StreamPaths, retained: usize) -> Result<(), ()> {
    let mut rotations = scan_rotation_entries(paths)?;
    rotations.sort_unstable_by_key(|entry| entry.sequence);
    let delete_count = rotations.len().saturating_sub(retained);
    for entry in rotations.into_iter().take(delete_count) {
        delete_regular_file_if_identity(&entry.path, entry.identity).map_err(|_| ())?;
    }
    Ok(())
}

struct RotationEntry {
    sequence: u64,
    path: PathBuf,
    identity: FileIdentity,
}

fn scan_rotation_entries(paths: &StreamPaths) -> Result<Vec<RotationEntry>, ()> {
    let entries = fs::read_dir(&paths.root).map_err(|_| ())?;
    let mut rotations = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let Some(sequence) = rotation_sequence(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let pinned = pin_path_without_reparse(&path).map_err(|_| ())?;
        if !pinned.metadata().map_err(|_| ())?.is_file() {
            return Err(());
        }
        let identity = FileIdentity::of(pinned.file(), &path).map_err(|_| ())?;
        rotations.push(RotationEntry {
            sequence,
            path,
            identity,
        });
    }
    Ok(rotations)
}

fn rotation_sequence(name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let digits = name
        .strip_prefix(ROTATED_PREFIX)?
        .strip_suffix(ROTATED_SUFFIX)?;
    (digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn recorded_at_utc(now: OffsetDateTime) -> String {
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
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::runtime::windows::ExclusiveFileLock;

    fn config_input() -> ProjectLogConfigInput {
        ProjectLogConfigInput {
            level: ProjectLogLevel::Info,
            queue_capacity: 8,
            batch_max_records: 4,
            batch_max_bytes: 4096,
            flush_interval: Duration::from_millis(10),
            shutdown_timeout: Duration::from_secs(2),
            lock_timeout: Duration::from_secs(1),
            max_record_bytes: 4096,
            max_file_bytes: 65_536,
            retained_rotated_files: 2,
        }
    }

    fn event(sequence_hint: u64) -> ProjectLogEvent {
        ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::PhaseFinished,
            ProjectLogContext::new("zh-Hans")
                .with_run_id("run")
                .with_engine("rpg_maker_mv")
                .with_project("project")
                .with_command("translate")
                .with_profile("default"),
            format!("完成阶段 {sequence_hint}"),
            ProjectLogPayload::Phase {
                phase: "translate".to_owned(),
                amount: ProjectLogAmount::Determinate {
                    completed: sequence_hint,
                    total: 4,
                },
            },
        )
    }

    fn padded_event(sequence_hint: u64) -> ProjectLogEvent {
        ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::PhaseFinished,
            ProjectLogContext::new("en"),
            format!("{}-{sequence_hint}", "x".repeat(128)),
            ProjectLogPayload::None,
        )
    }

    fn read_records(path: &Path) -> Vec<ProjectLogRecord> {
        fs::read(path)
            .expect("日志文件应可读取")
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("每一行都应是项目日志记录"))
            .collect()
    }

    #[test]
    fn validates_all_configured_resource_bounds() {
        let mut input = config_input();
        input.queue_capacity = 0;
        assert_eq!(
            ProjectLogConfig::try_from(input),
            Err(ProjectLogConfigurationError::ZeroQueueCapacity)
        );

        let mut input = config_input();
        input.batch_max_records = 0;
        assert_eq!(
            ProjectLogConfig::try_from(input),
            Err(ProjectLogConfigurationError::ZeroBatchMaxRecords)
        );

        let mut input = config_input();
        input.max_file_bytes = 1;
        assert!(matches!(
            ProjectLogConfig::try_from(input),
            Err(ProjectLogConfigurationError::RecordExceedsFileLimit { .. })
        ));
    }

    #[test]
    fn level_parser_accepts_only_the_documented_closed_set() {
        assert_eq!("error".parse(), Ok(ProjectLogLevel::Error));
        assert_eq!("warn".parse(), Ok(ProjectLogLevel::Warn));
        assert_eq!("info".parse(), Ok(ProjectLogLevel::Info));
        assert_eq!("debug".parse(), Ok(ProjectLogLevel::Debug));
        assert_eq!(
            "trace".parse::<ProjectLogLevel>(),
            Err(ProjectLogLevelParseError)
        );
    }

    #[test]
    fn serialization_sanitizes_every_free_text_field_at_the_log_boundary() {
        let hostile = "visible\n\u{202e}reordered\u{2068}\u{1b}[31m";
        let queued = QueuedProjectLogEvent {
            emitted_at: OffsetDateTime::now_utc(),
            event: ProjectLogEvent::new(
                ProjectLogLevel::Info,
                ProjectLogCode::RunPlanResolved,
                ProjectLogContext::new(hostile)
                    .with_run_id(hostile)
                    .with_engine(hostile)
                    .with_project(hostile)
                    .with_command(hostile)
                    .with_profile(hostile),
                hostile,
                ProjectLogPayload::RunPlan {
                    source: ProjectLogValueSource::Explicit,
                    lua_source: Some(ProjectLogValueSource::ProjectState),
                    selections: vec![hostile.to_owned()],
                    lua_enabled: Some(true),
                },
            ),
        };
        let config = ProjectLogConfig::try_from(config_input()).expect("日志配置应有效");
        let health = ProjectLogHealth::default();
        let mut sequence = 0;
        let bytes = serialize_event(queued, config, &health, &mut sequence)
            .expect("恶意显示文本不得阻断日志序列化");
        let record: ProjectLogRecord =
            serde_json::from_slice(&bytes).expect("净化后的记录应是合法 JSONL");

        let mut values = vec![record.locale, record.message];
        values.extend([
            record.run_id.expect("run id 应存在"),
            record.engine.expect("engine 应存在"),
            record.project.expect("project 应存在"),
            record.command.expect("command 应存在"),
            record.profile.expect("profile 应存在"),
        ]);
        let ProjectLogPayload::RunPlan { selections, .. } = record.payload else {
            panic!("记录应保留 RunPlan payload 类型");
        };
        values.extend(selections);
        for value in values {
            assert_eq!(value, sanitize_user_text(&value));
            assert!(!value.contains('\n'));
            assert!(!value.contains('\u{1b}'));
            assert!(!value.contains('\u{202e}'));
            assert!(!value.contains('\u{2068}'));
        }

        assert_eq!(
            sanitize_payload_text(ProjectLogPayload::Phase {
                phase: hostile.to_owned(),
                amount: ProjectLogAmount::Indeterminate,
            }),
            ProjectLogPayload::Phase {
                phase: sanitize_user_text(hostile),
                amount: ProjectLogAmount::Indeterminate,
            }
        );
        assert_eq!(
            sanitize_payload_text(ProjectLogPayload::NoWork {
                reason_code: hostile.to_owned(),
            }),
            ProjectLogPayload::NoWork {
                reason_code: sanitize_user_text(hostile),
            }
        );
    }

    #[test]
    fn writes_compact_typed_jsonl_in_batches() {
        let directory = tempdir().expect("临时目录应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        for sequence in 1..=4 {
            logger.emit(event(sequence));
        }
        let health = runtime.shutdown();

        assert!(!health.is_degraded());
        assert_eq!(health.accepted_records, 4);
        assert_eq!(health.persisted_records, 4);
        let records = read_records(&directory.path().join(ACTIVE_FILE_NAME));
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[3].sequence, 4);
        assert_eq!(records[0].run_id.as_deref(), Some("run"));
        assert_eq!(records[0].locale, "zh-Hans");
        assert!(
            !fs::read(directory.path().join(ACTIVE_FILE_NAME))
                .expect("日志文件应可读取")
                .windows(2)
                .any(|window| window == b"\r\n")
        );
    }

    #[test]
    fn debug_is_filtered_before_it_uses_queue_capacity() {
        let directory = tempdir().expect("临时目录应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Debug,
            ProjectLogCode::TaskStarted,
            ProjectLogContext::new("en"),
            "task",
            ProjectLogPayload::Task {
                ordinal: 1,
                total: 1,
                outcome: None,
                attempts: None,
            },
        ));
        let health = runtime.shutdown();

        assert_eq!(health.accepted_records, 0);
        assert_eq!(health.dropped_records, 0);
        assert!(!directory.path().join(ACTIVE_FILE_NAME).exists());
    }

    #[test]
    fn startup_failure_returns_no_op_and_one_warning() {
        let directory = tempdir().expect("临时目录应可建立");
        let root_file = directory.path().join("not-a-directory");
        fs::write(&root_file, b"file").expect("占位文件应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let runtime = start_project_log(root_file, config);
        let logger = runtime.logger();
        logger.emit(event(1));

        assert_eq!(logger.health().startup_failures, 1);
        assert!(logger.take_warning().is_some());
        assert!(logger.take_warning().is_none());
        let health = runtime.shutdown();
        assert!(health.is_degraded());
    }

    #[test]
    fn incomplete_tail_is_recovered_without_blocking_new_records() {
        let directory = tempdir().expect("临时目录应可建立");
        fs::write(directory.path().join(ACTIVE_FILE_NAME), b"{\"time\":\"cut")
            .expect("不完整尾部应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        runtime.logger().emit(event(1));
        let health = runtime.shutdown();

        assert_eq!(health.recovered_incomplete_tails, 1);
        assert_eq!(health.persisted_records, 1);
        assert_eq!(
            read_records(&directory.path().join(ACTIVE_FILE_NAME)).len(),
            1
        );
    }

    #[test]
    fn complete_bad_line_is_preserved_and_only_degrades_logging() {
        let directory = tempdir().expect("临时目录应可建立");
        let active = directory.path().join(ACTIVE_FILE_NAME);
        fs::write(&active, b"{\"bad\":true}\n").expect("损坏记录应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        runtime.logger().emit(event(1));
        let health = runtime.shutdown();

        assert_eq!(health.malformed_records, 1);
        assert_eq!(health.dropped_records, 1);
        assert_eq!(fs::read(active).expect("坏行应保留"), b"{\"bad\":true}\n");
    }

    #[test]
    fn queue_pressure_and_lock_timeout_only_degrade_log_health() {
        let directory = tempdir().expect("临时目录应可建立");
        let held = ExclusiveFileLock::acquire(
            &directory.path().join(LOCK_FILE_NAME),
            Duration::from_secs(1),
        )
        .expect("测试应先持有日志锁");
        let mut input = config_input();
        input.queue_capacity = 1;
        input.batch_max_records = 1;
        input.lock_timeout = Duration::from_millis(20);
        let config = ProjectLogConfig::try_from(input).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        for sequence in 1..=128 {
            logger.emit(event(sequence));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while logger.health().lock_timeouts == 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let during_run = logger.health();
        assert!(during_run.queue_full > 0);
        assert!(during_run.lock_timeouts > 0);
        drop(held);

        let after_shutdown = runtime.shutdown();
        assert!(after_shutdown.is_degraded());
        assert!(after_shutdown.dropped_records > 0);
    }

    #[test]
    fn partial_write_is_counted_and_never_escapes_the_logging_boundary() {
        let directory = tempdir().expect("临时目录应可建立");
        let mut config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        config.test_faults.partial_write = true;
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();

        logger.emit(event(1));
        let health = runtime.shutdown();

        assert_eq!(health.accepted_records, 1);
        assert_eq!(health.persisted_records, 0);
        assert_eq!(health.dropped_records, 1);
        assert_eq!(health.write_failures, 1);
        assert!(health.is_degraded());
        let partial = fs::read(directory.path().join(ACTIVE_FILE_NAME))
            .expect("部分写入的活动文件应保留给下次恢复");
        assert!(!partial.is_empty());
        assert_ne!(partial.last(), Some(&b'\n'));

        // 即使 worker 已经关闭，业务线程继续发事件也只更新健康状态。
        logger.emit(event(2));
        assert_eq!(logger.health().queue_closed, 1);
    }

    #[test]
    fn rotation_failure_drops_only_log_records() {
        let directory = tempdir().expect("临时目录应可建立");
        let mut input = config_input();
        input.queue_capacity = 16;
        input.batch_max_records = 16;
        input.max_record_bytes = 512;
        input.max_file_bytes = 512;
        let mut config = ProjectLogConfig::try_from(input).expect("配置应合法");
        config.test_faults.rotation = true;
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        for sequence in 1..=4 {
            logger.emit(padded_event(sequence));
        }

        let health = runtime.shutdown();

        assert_eq!(health.accepted_records, 4);
        assert!(health.persisted_records > 0);
        assert!(health.dropped_records > 0);
        assert!(health.rotation_failures > 0);
        assert!(health.is_degraded());
        assert!(!read_records(&directory.path().join(ACTIVE_FILE_NAME)).is_empty());
    }

    #[test]
    fn retention_failure_preserves_successfully_written_records() {
        let directory = tempdir().expect("临时目录应可建立");
        let mut input = config_input();
        input.queue_capacity = 32;
        input.batch_max_records = 32;
        input.batch_max_bytes = 16_384;
        input.max_record_bytes = 512;
        input.max_file_bytes = 512;
        input.retained_rotated_files = 1;
        let mut config = ProjectLogConfig::try_from(input).expect("配置应合法");
        config.test_faults.retention = true;
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        for sequence in 1..=20 {
            logger.emit(padded_event(sequence));
        }

        let health = runtime.shutdown();

        assert_eq!(health.accepted_records, 20);
        assert_eq!(health.persisted_records, 20);
        assert_eq!(health.dropped_records, 0);
        assert!(health.retention_failures > 0);
        assert_eq!(health.rotation_failures, 0);
        assert!(health.is_degraded());
        let rotations = fs::read_dir(directory.path())
            .expect("日志根应可枚举")
            .filter_map(Result::ok)
            .filter(|entry| rotation_sequence(&entry.file_name()).is_some())
            .count();
        assert!(rotations > input.retained_rotated_files);
    }

    #[test]
    fn shutdown_timeout_returns_degraded_health_without_waiting_for_worker() {
        let directory = tempdir().expect("临时目录应可建立");
        let mut input = config_input();
        input.shutdown_timeout = Duration::from_millis(5);
        let mut config = ProjectLogConfig::try_from(input).expect("配置应合法");
        config.test_faults.shutdown_delay = Duration::from_millis(100);
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();

        let health = runtime.shutdown();

        assert_eq!(health.shutdown_timeouts, 1);
        assert!(health.is_degraded());
        for sequence in 1..=128 {
            logger.emit(event(sequence));
        }
        let after_closed_emits = logger.health();
        assert_eq!(after_closed_emits.queue_closed, 128);
        assert_eq!(after_closed_emits.dropped_records, 128);

        // 让已经脱离等待的 worker 完成，确认其延迟收尾不会 panic。
        thread::sleep(Duration::from_millis(150));
        assert_eq!(logger.health().worker_panics, 0);
    }

    #[test]
    fn rotates_monotonically_and_keeps_only_configured_count() {
        let directory = tempdir().expect("临时目录应可建立");
        let mut input = config_input();
        input.batch_max_records = 1;
        input.max_record_bytes = 512;
        input.max_file_bytes = 512;
        input.retained_rotated_files = 2;
        let config = ProjectLogConfig::try_from(input).expect("配置应合法");
        let runtime = start_project_log(directory.path().to_path_buf(), config);
        let logger = runtime.logger();
        for sequence in 1..=20 {
            logger.emit(padded_event(sequence));
        }
        let health = runtime.shutdown();

        assert_eq!(health.rotation_failures, 0);
        let rotations = fs::read_dir(directory.path())
            .expect("日志根应可枚举")
            .filter_map(Result::ok)
            .filter(|entry| rotation_sequence(&entry.file_name()).is_some())
            .count();
        assert!((1..=2).contains(&rotations));
    }

    #[test]
    fn independent_workers_never_interleave_physical_lines() {
        let directory = tempdir().expect("临时目录应可建立");
        let config = ProjectLogConfig::try_from(config_input()).expect("配置应合法");
        let first = start_project_log(directory.path().to_path_buf(), config);
        let second = start_project_log(directory.path().to_path_buf(), config);
        let first_logger = first.logger();
        let second_logger = second.logger();
        let first_thread = thread::spawn(move || {
            for sequence in 1..=8 {
                first_logger.emit(event(sequence));
            }
        });
        let second_thread = thread::spawn(move || {
            for sequence in 9..=16 {
                second_logger.emit(event(sequence));
            }
        });
        first_thread.join().expect("第一生产线程不应 panic");
        second_thread.join().expect("第二生产线程不应 panic");
        let first_health = first.shutdown();
        let second_health = second.shutdown();

        assert_eq!(
            first_health.persisted_records + second_health.persisted_records,
            16
        );
        assert_eq!(
            read_records(&directory.path().join(ACTIVE_FILE_NAME)).len(),
            16
        );
    }
}
