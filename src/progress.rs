//! 终端实时进度的共享契约与渲染根适配器。
//!
//! 业务切片拥有阶段枚举，并只发布绝对快照；本模块不解释业务阶段。动态终端渲染在
//! 后台线程执行；中间观察只尝试替换一个有界的最新快照，最终确认快照通过异步渲染
//! 通道交接。线程、通道和终端 I/O 故障会记入统一健康快照，并由显式收尾结果返回给进程边界。

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

const DYNAMIC_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const DYNAMIC_BAR_WIDTH: usize = 20;
const PLAIN_PROGRESS_BUCKETS: u64 = 10;
const FIRST_STRONG_ISOLATE: char = '\u{2068}';
const POP_DIRECTIONAL_ISOLATE: char = '\u{2069}';

/// 一个阶段当前可确认的工作量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgressAmount {
    /// 尚不能建立真实分母，只能呈现阶段活动状态。
    Indeterminate,
    /// 已经建立真实分母；`completed` 表示已经确认完成的数量。
    Determinate { completed: u64, total: u64 },
}

impl ProgressAmount {
    pub(crate) const fn determinate(completed: u64, total: u64) -> Self {
        Self::Determinate {
            completed: if completed < total { completed } else { total },
            total,
        }
    }

    fn normalized(self) -> Self {
        match self {
            Self::Indeterminate => Self::Indeterminate,
            Self::Determinate { completed, total } => Self::determinate(completed, total),
        }
    }

    fn is_complete(self) -> bool {
        matches!(
            self,
            Self::Determinate { completed, total } if total > 0 && completed == total
        )
    }
}

/// 某个业务切片在一个时刻可确认的绝对进度。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProgressSnapshot<P> {
    pub(crate) phase: P,
    pub(crate) amount: ProgressAmount,
}

impl<P> ProgressSnapshot<P> {
    pub(crate) const fn new(phase: P, amount: ProgressAmount) -> Self {
        Self { phase, amount }
    }

    pub(crate) const fn indeterminate(phase: P) -> Self {
        Self::new(phase, ProgressAmount::Indeterminate)
    }

    pub(crate) const fn determinate(phase: P, completed: u64, total: u64) -> Self {
        Self::new(phase, ProgressAmount::determinate(completed, total))
    }
}

/// 不可失败的进度观察入口。
///
/// 实现可以合并或丢弃中间快照；调用方必须始终发布绝对值，不能依赖增量事件。
/// 终端实现会把已经确认完成的 `N/N` 快照交给不等待调用方的持久通道，不会因
/// 最新值槽的短暂锁竞争而永久丢失；静默观察者仍然有意忽略全部快照。
pub(crate) trait ProgressObserver<P>: Send + Sync {
    fn observe(&self, snapshot: ProgressSnapshot<P>);
}

/// 不需要实时进度时使用的空观察者。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopProgressObserver;

impl<P> ProgressObserver<P> for NoopProgressObserver {
    fn observe(&self, _snapshot: ProgressSnapshot<P>) {}
}

/// 用户选择的实时进度呈现模式。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ProgressMode {
    /// 只在标准错误流连接终端时使用动态单行呈现。
    #[default]
    Auto,
    /// 无条件输出稀疏、逐行、无控制序列的进度。
    Plain,
    /// 完全关闭实时进度。
    Off,
}

impl ProgressMode {
    #[cfg(test)]
    pub(crate) const NAMES: [&'static str; 3] = ["auto", "plain", "off"];

    #[cfg(test)]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plain => "plain",
            Self::Off => "off",
        }
    }

    const fn render_style(self, stderr_is_terminal: bool) -> RenderStyle {
        match self {
            Self::Auto if stderr_is_terminal => RenderStyle::Dynamic,
            Self::Auto | Self::Off => RenderStyle::Silent,
            Self::Plain => RenderStyle::Plain,
        }
    }
}

impl FromStr for ProgressMode {
    type Err = ProgressModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "plain" => Ok(Self::Plain),
            "off" => Ok(Self::Off),
            _ => Err(ProgressModeParseError),
        }
    }
}

/// 进度模式值不属于当前闭集。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressModeParseError;

impl fmt::Display for ProgressModeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected one of: auto, plain, off")
    }
}

impl std::error::Error for ProgressModeParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderStyle {
    Dynamic,
    Plain,
    Silent,
}

/// 终端进度呈现失败的类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalProgressFailureKind {
    RendererThreadStart,
    ControlChannelClosed,
    WriterWrite,
    WriterFlush,
    RendererThreadPanicked,
}

impl TerminalProgressFailureKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RendererThreadStart => "renderer_thread_start",
            Self::ControlChannelClosed => "control_channel_closed",
            Self::WriterWrite => "writer_write",
            Self::WriterFlush => "writer_flush",
            Self::RendererThreadPanicked => "renderer_thread_panicked",
        }
    }
}

/// 失败发生时进度渲染器正在执行的操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalProgressOperation {
    StartRenderer,
    PublishCompletion,
    RenderPlainLine,
    RenderDynamicLine,
    RenderStatus,
    ClearDynamicLine,
    RenderFinalMessage,
    Finalizing,
    SafeStopping,
    Finish,
    JoinRenderer,
}

impl TerminalProgressOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StartRenderer => "start_renderer",
            Self::PublishCompletion => "publish_completion",
            Self::RenderPlainLine => "render_plain_line",
            Self::RenderDynamicLine => "render_dynamic_line",
            Self::RenderStatus => "render_status",
            Self::ClearDynamicLine => "clear_dynamic_line",
            Self::RenderFinalMessage => "render_final_message",
            Self::Finalizing => "finalizing",
            Self::SafeStopping => "safe_stopping",
            Self::Finish => "finish",
            Self::JoinRenderer => "join_renderer",
        }
    }
}

/// 一项已经确认的终端进度呈现失败。
#[derive(Clone, Debug)]
pub(crate) struct TerminalProgressFailure {
    kind: TerminalProgressFailureKind,
    operation: TerminalProgressOperation,
    source: Option<Arc<io::Error>>,
    detail: String,
}

impl TerminalProgressFailure {
    fn io(
        kind: TerminalProgressFailureKind,
        operation: TerminalProgressOperation,
        source: io::Error,
    ) -> Self {
        let detail = source.to_string();
        Self {
            kind,
            operation,
            source: Some(Arc::new(source)),
            detail,
        }
    }

    fn channel_closed(operation: TerminalProgressOperation) -> Self {
        Self {
            kind: TerminalProgressFailureKind::ControlChannelClosed,
            operation,
            source: None,
            detail: String::from("终端进度渲染线程的 control channel 已关闭"),
        }
    }

    fn worker_panicked(detail: String) -> Self {
        Self {
            kind: TerminalProgressFailureKind::RendererThreadPanicked,
            operation: TerminalProgressOperation::JoinRenderer,
            source: None,
            detail,
        }
    }

    pub(crate) const fn kind(&self) -> TerminalProgressFailureKind {
        self.kind
    }

    pub(crate) const fn operation(&self) -> TerminalProgressOperation {
        self.operation
    }

    pub(crate) fn io_error_kind(&self) -> Option<io::ErrorKind> {
        self.source.as_ref().map(|source| source.kind())
    }

    pub(crate) fn raw_os_error(&self) -> Option<i32> {
        self.source
            .as_ref()
            .and_then(|source| source.raw_os_error())
    }

    #[cfg(test)]
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    fn same_fact(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.operation == other.operation
            && self.io_error_kind() == other.io_error_kind()
            && self.raw_os_error() == other.raw_os_error()
            && self.detail == other.detail
    }
}

impl fmt::Display for TerminalProgressFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "terminal progress {} failed during {}: {}",
            self.kind.as_str(),
            self.operation.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for TerminalProgressFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// 一次进度命令期间已确认的全部呈现失败。
#[derive(Clone, Debug)]
pub(crate) struct TerminalProgressFailures {
    failures: Vec<TerminalProgressFailure>,
}

impl TerminalProgressFailures {
    pub(crate) fn failures(&self) -> &[TerminalProgressFailure] {
        &self.failures
    }
}

impl fmt::Display for TerminalProgressFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            failure.fmt(formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for TerminalProgressFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures
            .first()
            .map(|failure| failure as &(dyn std::error::Error + 'static))
    }
}

#[derive(Default)]
struct ProgressHealth {
    failures: Mutex<Vec<TerminalProgressFailure>>,
}

impl ProgressHealth {
    fn record(&self, failure: TerminalProgressFailure) {
        let mut failures = lock_after_poison(&self.failures);
        if !failures.iter().any(|current| current.same_fact(&failure)) {
            failures.push(failure);
        }
    }

    fn result(&self) -> Result<(), TerminalProgressFailures> {
        let failures = lock_after_poison(&self.failures).clone();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TerminalProgressFailures { failures })
        }
    }
}

struct PendingSnapshot<P> {
    sequence: u64,
    snapshot: ProgressSnapshot<P>,
}

struct LatestSnapshot<P> {
    next_sequence: AtomicU64,
    pending: Mutex<Option<PendingSnapshot<P>>>,
}

impl<P> LatestSnapshot<P> {
    fn new() -> Self {
        Self {
            next_sequence: AtomicU64::new(0),
            pending: Mutex::new(None),
        }
    }

    fn try_replace(&self, snapshot: ProgressSnapshot<P>) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let mut pending = match self.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let should_replace = pending
            .as_ref()
            .is_none_or(|current| sequence > current.sequence);
        if should_replace {
            *pending = Some(PendingSnapshot { sequence, snapshot });
        }
    }

    fn take(&self) -> Option<ProgressSnapshot<P>> {
        let mut pending = match self.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        pending.take().map(|pending| pending.snapshot)
    }
}

fn lock_after_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct SnapshotDispatch<P> {
    latest: Arc<LatestSnapshot<P>>,
    control_sender: Arc<mpsc::Sender<RendererControl<P>>>,
    worker_thread: Thread,
    health: Arc<ProgressHealth>,
}

/// 可廉价克隆并交给并发业务工作的终端进度观察者。
pub(crate) struct TerminalProgressObserver<P> {
    dispatch: Option<Arc<SnapshotDispatch<P>>>,
}

impl<P> TerminalProgressObserver<P> {
    fn silent() -> Self {
        Self { dispatch: None }
    }

    #[cfg(test)]
    pub(crate) fn observe(&self, snapshot: ProgressSnapshot<P>)
    where
        P: Send + 'static,
    {
        <Self as ProgressObserver<P>>::observe(self, snapshot);
    }
}

impl<P> Clone for TerminalProgressObserver<P> {
    fn clone(&self) -> Self {
        Self {
            dispatch: self.dispatch.clone(),
        }
    }
}

impl<P> ProgressObserver<P> for TerminalProgressObserver<P>
where
    P: Send + 'static,
{
    fn observe(&self, snapshot: ProgressSnapshot<P>) {
        let Some(dispatch) = &self.dispatch else {
            return;
        };
        if snapshot.amount.is_complete() {
            // 完成快照和后续 Status/Finish 共用同一个 Sender，确保调用方先发布的
            // N/N 不会被收尾消息越过；异步 channel 的 send 不等待渲染线程。
            if dispatch
                .control_sender
                .send(RendererControl::Completion(snapshot))
                .is_err()
            {
                dispatch
                    .health
                    .record(TerminalProgressFailure::channel_closed(
                        TerminalProgressOperation::PublishCompletion,
                    ));
            }
        } else {
            dispatch.latest.try_replace(snapshot);
        }
        dispatch.worker_thread.unpark();
    }
}

enum RendererControl<P> {
    Completion(ProgressSnapshot<P>),
    Status {
        message: String,
        operation: TerminalProgressOperation,
        acknowledgement: mpsc::Sender<()>,
    },
    Finish(Option<String>),
}

struct RendererControlHandle<P> {
    sender: Arc<mpsc::Sender<RendererControl<P>>>,
    worker_thread: Thread,
    health: Arc<ProgressHealth>,
}

impl<P> RendererControlHandle<P> {
    fn send_status(
        &self,
        message: String,
        operation: TerminalProgressOperation,
    ) -> Result<(), TerminalProgressFailures> {
        let (acknowledgement, acknowledged) = mpsc::channel();
        if self
            .sender
            .send(RendererControl::Status {
                message,
                operation,
                acknowledgement,
            })
            .is_err()
        {
            self.health
                .record(TerminalProgressFailure::channel_closed(operation));
            return self.health.result();
        }
        self.worker_thread.unpark();
        if acknowledged.recv().is_err() {
            self.health
                .record(TerminalProgressFailure::channel_closed(operation));
        }
        self.health.result()
    }

    fn send_finish(&self, message: Option<String>) {
        if self.sender.send(RendererControl::Finish(message)).is_err() {
            self.health.record(TerminalProgressFailure::channel_closed(
                TerminalProgressOperation::Finish,
            ));
        }
        self.worker_thread.unpark();
    }
}

/// 拥有后台终端渲染生命周期的进度控制器。
///
/// `render_phase` 只负责把切片自己的阶段枚举转换为本地化标签。确定进度的
/// `completed/total` 由本模块追加并使用 Unicode 方向隔离；零工作量只显示标签，
/// 永远不会生成 `0/0`。
#[must_use = "进度控制器必须存活到命令结束，以便完成清行和后台线程收尾"]
pub(crate) struct TerminalProgress<P> {
    observer: TerminalProgressObserver<P>,
    control: Option<RendererControlHandle<P>>,
    worker: Option<JoinHandle<()>>,
    health: Arc<ProgressHealth>,
    finished: bool,
}

impl<P> TerminalProgress<P> {
    fn silent() -> Self {
        let health = Arc::new(ProgressHealth::default());
        Self {
            observer: TerminalProgressObserver::silent(),
            control: None,
            worker: None,
            health,
            finished: false,
        }
    }

    fn silent_with_failure(failure: TerminalProgressFailure) -> Self {
        let progress = Self::silent();
        progress.health.record(failure);
        progress
    }

    pub(crate) fn observer(&self) -> TerminalProgressObserver<P> {
        self.observer.clone()
    }

    #[cfg(test)]
    pub(crate) fn observe(&self, snapshot: ProgressSnapshot<P>)
    where
        P: Send + 'static,
    {
        self.observer.observe(snapshot);
    }

    /// 切换到本地化的收尾状态，并等待后台线程确认呈现结果。
    pub(crate) fn finalizing(
        &self,
        message: impl Into<String>,
    ) -> Result<(), TerminalProgressFailures> {
        self.send_status(message.into(), TerminalProgressOperation::Finalizing)
    }

    /// 切换到本地化的安全停止状态，保留此前确认的业务进度事实，并等待呈现确认。
    pub(crate) fn safe_stopping(
        &self,
        message: impl Into<String>,
    ) -> Result<(), TerminalProgressFailures> {
        self.send_status(message.into(), TerminalProgressOperation::SafeStopping)
    }

    /// 清除实时状态、等待渲染线程结束，并返回命令期间已经确认的全部呈现失败。
    pub(crate) fn finish(mut self) -> Result<(), TerminalProgressFailures> {
        self.shutdown(None)
    }

    /// 用一行本地化文本结束实时状态并等待渲染线程结束。
    #[cfg(test)]
    pub(crate) fn finish_with_message(
        mut self,
        message: impl Into<String>,
    ) -> Result<(), TerminalProgressFailures> {
        self.shutdown(Some(message.into()))
    }

    /// 返回当前已经确认的呈现失败，不停止渲染线程。
    #[cfg(test)]
    pub(crate) fn check_health(&self) -> Result<(), TerminalProgressFailures> {
        self.health.result()
    }

    fn send_status(
        &self,
        message: String,
        operation: TerminalProgressOperation,
    ) -> Result<(), TerminalProgressFailures> {
        if let Some(handle) = &self.control {
            handle.send_status(message, operation)
        } else {
            self.health.result()
        }
    }

    fn shutdown(&mut self, message: Option<String>) -> Result<(), TerminalProgressFailures> {
        if self.finished {
            return self.health.result();
        }
        if let Some(handle) = self.control.take() {
            handle.send_finish(message);
        }
        if let Some(worker) = self.worker.take()
            && let Err(panic) = worker.join()
        {
            self.health
                .record(TerminalProgressFailure::worker_panicked(panic_detail(
                    panic,
                )));
        }
        self.observer = TerminalProgressObserver::silent();
        self.finished = true;
        self.health.result()
    }
}

impl<P> TerminalProgress<P>
where
    P: Eq + Send + 'static,
{
    /// 使用进程标准错误流创建渲染器，并据实际 TTY 状态解析 `auto`。
    pub(crate) fn stderr<R>(mode: ProgressMode, render_phase: R) -> Self
    where
        R: Fn(&P) -> String + Send + 'static,
    {
        let stderr = io::stderr();
        let stderr_is_terminal = stderr.is_terminal();
        Self::with_writer(mode, stderr_is_terminal, stderr, render_phase)
    }

    /// 使用调用方提供的输出能力创建渲染器。
    ///
    /// `stderr_is_terminal` 必须来自承载标准错误流的真实终端判断；该参数也使终端
    /// 策略能够在不依赖真实控制台的测试中完整验证。渲染线程无法创建时，返回携带
    /// 启动失败健康状态的静默控制器；调用方仍可执行业务，但必须处理后续健康检查或收尾结果。
    pub(crate) fn with_writer<W, R>(
        mode: ProgressMode,
        stderr_is_terminal: bool,
        writer: W,
        render_phase: R,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Fn(&P) -> String + Send + 'static,
    {
        Self::with_writer_and_spawner(
            mode,
            stderr_is_terminal,
            writer,
            render_phase,
            spawn_renderer_thread,
        )
    }

    fn with_writer_and_spawner<W, R, S>(
        mode: ProgressMode,
        stderr_is_terminal: bool,
        writer: W,
        render_phase: R,
        spawn: S,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Fn(&P) -> String + Send + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>>,
    {
        let style = mode.render_style(stderr_is_terminal);
        if style == RenderStyle::Silent {
            return Self::silent();
        }

        let latest = Arc::new(LatestSnapshot::new());
        let worker_latest = Arc::clone(&latest);
        let (control_sender, control_receiver) = mpsc::channel();
        let control_sender = Arc::new(control_sender);
        let health = Arc::new(ProgressHealth::default());
        let worker_health = Arc::clone(&health);
        let worker = spawn(Box::new(move || {
            run_renderer(
                style,
                writer,
                render_phase,
                worker_latest,
                control_receiver,
                worker_health,
            );
        }));
        let worker = match worker {
            Ok(worker) => worker,
            Err(source) => {
                return Self::silent_with_failure(TerminalProgressFailure::io(
                    TerminalProgressFailureKind::RendererThreadStart,
                    TerminalProgressOperation::StartRenderer,
                    source,
                ));
            }
        };
        let worker_thread = worker.thread().clone();
        Self {
            observer: TerminalProgressObserver {
                dispatch: Some(Arc::new(SnapshotDispatch {
                    latest,
                    control_sender: Arc::clone(&control_sender),
                    worker_thread: worker_thread.clone(),
                    health: Arc::clone(&health),
                })),
            },
            control: Some(RendererControlHandle {
                sender: control_sender,
                worker_thread,
                health: Arc::clone(&health),
            }),
            worker: Some(worker),
            health,
            finished: false,
        }
    }
}

impl<P> Drop for TerminalProgress<P> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(failures) = self.shutdown(None)
            && !thread::panicking()
        {
            panic!("终端进度控制器在 Drop 收尾时发现未处理的呈现失败: {failures}");
        }
    }
}

fn spawn_renderer_thread(task: Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(String::from("att-terminal-progress"))
        .spawn(task)
}

fn panic_detail(panic: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        return (*message).to_owned();
    }
    if let Some(message) = panic.downcast_ref::<String>() {
        return message.clone();
    }
    String::from("渲染线程以非文本 panic payload 结束")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveDisplay {
    Snapshot,
    Status,
    Empty,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SnapshotUpdate {
    accepted: bool,
    plain_significant: bool,
}

struct RendererState<P> {
    snapshot: Option<ProgressSnapshot<P>>,
    status: Option<String>,
    active: ActiveDisplay,
    dynamic_dirty: bool,
    last_dynamic_render: Option<Instant>,
    spinner_frame: usize,
    dynamic_line_width: usize,
    last_plain_line: Option<String>,
}

impl<P> RendererState<P>
where
    P: Eq,
{
    fn new() -> Self {
        Self {
            snapshot: None,
            status: None,
            active: ActiveDisplay::Empty,
            dynamic_dirty: false,
            last_dynamic_render: None,
            spinner_frame: 0,
            dynamic_line_width: 0,
            last_plain_line: None,
        }
    }

    fn accept_snapshot(&mut self, mut incoming: ProgressSnapshot<P>) -> SnapshotUpdate {
        incoming.amount = incoming.amount.normalized();
        let update = snapshot_update(self.snapshot.as_ref(), &incoming);
        if update.accepted {
            self.snapshot = Some(incoming);
            self.status = None;
            self.active = ActiveDisplay::Snapshot;
            self.dynamic_dirty = true;
        }
        update
    }

    fn set_status(&mut self, message: String) {
        self.status = Some(sanitize_terminal_line(&message));
        self.active = ActiveDisplay::Status;
        self.dynamic_dirty = true;
    }

    fn active_is_completed_snapshot(&self) -> bool {
        self.active == ActiveDisplay::Snapshot
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.amount.is_complete())
    }
}

fn snapshot_update<P>(
    previous: Option<&ProgressSnapshot<P>>,
    incoming: &ProgressSnapshot<P>,
) -> SnapshotUpdate
where
    P: Eq,
{
    let Some(previous) = previous else {
        return SnapshotUpdate {
            accepted: true,
            plain_significant: true,
        };
    };
    if previous.phase != incoming.phase {
        return SnapshotUpdate {
            accepted: true,
            plain_significant: true,
        };
    }

    match (previous.amount, incoming.amount) {
        (ProgressAmount::Indeterminate, ProgressAmount::Indeterminate) => SnapshotUpdate::default(),
        (ProgressAmount::Determinate { .. }, ProgressAmount::Indeterminate) => {
            SnapshotUpdate::default()
        }
        (ProgressAmount::Indeterminate, ProgressAmount::Determinate { .. }) => SnapshotUpdate {
            accepted: true,
            plain_significant: true,
        },
        (
            ProgressAmount::Determinate {
                completed: previous_completed,
                total: previous_total,
            },
            ProgressAmount::Determinate { completed, total },
        ) => {
            if total != previous_total || completed <= previous_completed {
                return SnapshotUpdate::default();
            }
            SnapshotUpdate {
                accepted: true,
                plain_significant: plain_count_is_significant(previous_completed, completed, total),
            }
        }
    }
}

fn plain_count_is_significant(previous: u64, completed: u64, total: u64) -> bool {
    if total == 0 {
        return false;
    }
    if completed == total {
        return true;
    }
    if total <= PLAIN_PROGRESS_BUCKETS {
        return true;
    }
    progress_bucket(completed, total) > progress_bucket(previous, total)
}

fn progress_bucket(completed: u64, total: u64) -> u64 {
    let completed = u128::from(completed);
    let total = u128::from(total);
    ((completed * u128::from(PLAIN_PROGRESS_BUCKETS)) / total) as u64
}

fn run_renderer<P, W, R>(
    style: RenderStyle,
    mut writer: W,
    render_phase: R,
    latest: Arc<LatestSnapshot<P>>,
    control_receiver: mpsc::Receiver<RendererControl<P>>,
    health: Arc<ProgressHealth>,
) where
    P: Eq,
    W: Write,
    R: Fn(&P) -> String,
{
    let mut state = RendererState::new();
    let mut writer_available = true;

    loop {
        if let Some(snapshot) = latest.take() {
            let update = state.accept_snapshot(snapshot);
            if style == RenderStyle::Plain && update.plain_significant {
                write_plain_active(
                    &mut writer,
                    &render_phase,
                    &mut state,
                    &mut writer_available,
                    &health,
                );
            }
        }

        let mut should_finish = None;
        for control in control_receiver.try_iter() {
            match control {
                RendererControl::Completion(snapshot) => {
                    let update = state.accept_snapshot(snapshot);
                    if style == RenderStyle::Plain && update.plain_significant {
                        write_plain_active(
                            &mut writer,
                            &render_phase,
                            &mut state,
                            &mut writer_available,
                            &health,
                        );
                    }
                }
                RendererControl::Status {
                    message,
                    operation,
                    acknowledgement,
                } => {
                    if style == RenderStyle::Dynamic && state.active_is_completed_snapshot() {
                        write_dynamic_active(
                            &mut writer,
                            &render_phase,
                            &mut state,
                            &mut writer_available,
                            &health,
                        );
                    }
                    state.set_status(message);
                    match style {
                        RenderStyle::Dynamic => write_dynamic_active(
                            &mut writer,
                            &render_phase,
                            &mut state,
                            &mut writer_available,
                            &health,
                        ),
                        RenderStyle::Plain => write_plain_active(
                            &mut writer,
                            &render_phase,
                            &mut state,
                            &mut writer_available,
                            &health,
                        ),
                        RenderStyle::Silent => {}
                    }
                    if acknowledgement.send(()).is_err() {
                        health.record(TerminalProgressFailure::channel_closed(operation));
                    }
                }
                RendererControl::Finish(message) => {
                    should_finish = Some(message);
                    break;
                }
            }
        }

        if let Some(message) = should_finish {
            finish_rendering(
                style,
                &mut writer,
                &mut state,
                message.as_deref(),
                &mut writer_available,
                &health,
            );
            return;
        }

        if style == RenderStyle::Dynamic && dynamic_render_is_due(&state) {
            write_dynamic_active(
                &mut writer,
                &render_phase,
                &mut state,
                &mut writer_available,
                &health,
            );
        }

        if !writer_available {
            return;
        }

        let wait = match style {
            RenderStyle::Dynamic => dynamic_wait(&state),
            RenderStyle::Plain | RenderStyle::Silent => Duration::from_secs(60),
        };
        thread::park_timeout(wait);
    }
}

fn dynamic_render_is_due<P>(state: &RendererState<P>) -> bool {
    if state.active == ActiveDisplay::Empty {
        return false;
    }
    let spinner_is_active = match state.active {
        ActiveDisplay::Status => true,
        ActiveDisplay::Snapshot => state.snapshot.as_ref().is_some_and(|snapshot| {
            matches!(snapshot.amount, ProgressAmount::Indeterminate)
                || matches!(
                    snapshot.amount,
                    ProgressAmount::Determinate { total: 0, .. }
                )
        }),
        ActiveDisplay::Empty => false,
    };
    let due = state
        .last_dynamic_render
        .is_none_or(|last| last.elapsed() >= DYNAMIC_REFRESH_INTERVAL);
    due && (state.dynamic_dirty || spinner_is_active)
}

fn dynamic_wait<P>(state: &RendererState<P>) -> Duration {
    if state.active == ActiveDisplay::Empty {
        return Duration::from_secs(60);
    }
    state.last_dynamic_render.map_or(Duration::ZERO, |last| {
        DYNAMIC_REFRESH_INTERVAL.saturating_sub(last.elapsed())
    })
}

fn write_plain_active<P, W, R>(
    writer: &mut W,
    render_phase: &R,
    state: &mut RendererState<P>,
    writer_available: &mut bool,
    health: &ProgressHealth,
) where
    W: Write,
    R: Fn(&P) -> String,
{
    if !*writer_available {
        return;
    }
    let Some(line) = render_active_line(render_phase, state, None) else {
        return;
    };
    if state.last_plain_line.as_deref() == Some(line.as_str()) {
        return;
    }
    state.last_plain_line = Some(line.clone());
    let operation = if state.active == ActiveDisplay::Status {
        TerminalProgressOperation::RenderStatus
    } else {
        TerminalProgressOperation::RenderPlainLine
    };
    if write_and_flush(writer, operation, health, |writer| {
        writeln!(writer, "{line}")
    })
    .is_err()
    {
        *writer_available = false;
    }
}

fn write_dynamic_active<P, W, R>(
    writer: &mut W,
    render_phase: &R,
    state: &mut RendererState<P>,
    writer_available: &mut bool,
    health: &ProgressHealth,
) where
    W: Write,
    R: Fn(&P) -> String,
{
    if !*writer_available {
        return;
    }
    let frame = state.spinner_frame;
    let Some(line) = render_active_line(render_phase, state, Some(frame)) else {
        return;
    };
    state.spinner_frame = state.spinner_frame.wrapping_add(1);
    let line_width = UnicodeWidthStr::width(line.as_str());
    let trailing_spaces = state.dynamic_line_width.saturating_sub(line_width);
    let operation = if state.active == ActiveDisplay::Status {
        TerminalProgressOperation::RenderStatus
    } else {
        TerminalProgressOperation::RenderDynamicLine
    };
    if write_and_flush(writer, operation, health, |writer| {
        write!(writer, "\r{line}{:trailing_spaces$}", "")
    })
    .is_err()
    {
        *writer_available = false;
        return;
    }
    state.dynamic_line_width = line_width;
    state.dynamic_dirty = false;
    state.last_dynamic_render = Some(Instant::now());
}

fn render_active_line<P, R>(
    render_phase: &R,
    state: &RendererState<P>,
    spinner_frame: Option<usize>,
) -> Option<String>
where
    R: Fn(&P) -> String,
{
    match state.active {
        ActiveDisplay::Empty => None,
        ActiveDisplay::Status => {
            let status = state.status.as_deref()?;
            Some(match spinner_frame {
                Some(frame) => format!("[{}] {status}", spinner(frame)),
                None => status.to_owned(),
            })
        }
        ActiveDisplay::Snapshot => {
            let snapshot = state.snapshot.as_ref()?;
            let label = sanitize_terminal_line(&render_phase(&snapshot.phase));
            Some(match (spinner_frame, snapshot.amount) {
                (Some(frame), ProgressAmount::Indeterminate)
                | (Some(frame), ProgressAmount::Determinate { total: 0, .. }) => {
                    format!("[{}] {label}", spinner(frame))
                }
                (Some(_), ProgressAmount::Determinate { completed, total }) => format!(
                    "[{}] {label} {}",
                    progress_bar(completed, total),
                    isolated_count(completed, total)
                ),
                (None, ProgressAmount::Indeterminate)
                | (None, ProgressAmount::Determinate { total: 0, .. }) => label,
                (None, ProgressAmount::Determinate { completed, total }) => {
                    format!("{label} {}", isolated_count(completed, total))
                }
            })
        }
    }
}

fn spinner(frame: usize) -> char {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    FRAMES[frame % FRAMES.len()]
}

fn progress_bar(completed: u64, total: u64) -> String {
    debug_assert!(total > 0);
    let filled = ((u128::from(completed) * DYNAMIC_BAR_WIDTH as u128) / u128::from(total)) as usize;
    format!(
        "{}{}",
        "#".repeat(filled),
        "-".repeat(DYNAMIC_BAR_WIDTH - filled)
    )
}

fn isolated_count(completed: u64, total: u64) -> String {
    format!("{FIRST_STRONG_ISOLATE}{completed}/{total}{POP_DIRECTIONAL_ISOLATE}")
}

fn sanitize_terminal_line(message: &str) -> String {
    message
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '\u{001b}' => ' ',
            character if character < '\u{0020}' && character != '\t' => ' ',
            character => character,
        })
        .collect()
}

fn clear_dynamic_line<P, W>(
    writer: &mut W,
    state: &mut RendererState<P>,
    writer_available: &mut bool,
    health: &ProgressHealth,
) where
    W: Write,
{
    if !*writer_available || state.dynamic_line_width == 0 {
        return;
    }
    let width = state.dynamic_line_width;
    if write_and_flush(
        writer,
        TerminalProgressOperation::ClearDynamicLine,
        health,
        |writer| write!(writer, "\r{:width$}\r", "", width = width),
    )
    .is_err()
    {
        *writer_available = false;
    }
    state.dynamic_line_width = 0;
}

fn finish_rendering<P, W>(
    style: RenderStyle,
    writer: &mut W,
    state: &mut RendererState<P>,
    message: Option<&str>,
    writer_available: &mut bool,
    health: &ProgressHealth,
) where
    W: Write,
{
    if !*writer_available {
        return;
    }
    let message = message.map(sanitize_terminal_line);
    match style {
        RenderStyle::Dynamic => {
            clear_dynamic_line(writer, state, writer_available, health);
            if let Some(message) = message
                && *writer_available
                && write_and_flush(
                    writer,
                    TerminalProgressOperation::RenderFinalMessage,
                    health,
                    |writer| writeln!(writer, "{message}"),
                )
                .is_err()
            {
                *writer_available = false;
            }
        }
        RenderStyle::Plain => {
            if let Some(message) = message
                && state.last_plain_line.as_deref() != Some(message.as_str())
                && write_and_flush(
                    writer,
                    TerminalProgressOperation::RenderFinalMessage,
                    health,
                    |writer| writeln!(writer, "{message}"),
                )
                .is_err()
            {
                *writer_available = false;
            }
        }
        RenderStyle::Silent => {}
    }
}

fn write_and_flush<W, F>(
    writer: &mut W,
    operation: TerminalProgressOperation,
    health: &ProgressHealth,
    write: F,
) -> Result<(), ()>
where
    W: Write,
    F: FnOnce(&mut W) -> io::Result<()>,
{
    if let Err(source) = write(writer) {
        health.record(TerminalProgressFailure::io(
            TerminalProgressFailureKind::WriterWrite,
            operation,
            source,
        ));
        return Err(());
    }
    if let Err(source) = writer.flush() {
        health.record(TerminalProgressFailure::io(
            TerminalProgressFailureKind::WriterFlush,
            operation,
            source,
        ));
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Phase {
        Planning,
        Translating,
    }

    #[derive(Clone, Default)]
    struct SharedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(lock_after_poison(&self.bytes).clone())
                .expect("进度测试输出必须是 UTF-8")
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock_after_poison(&self.bytes).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WriteFailingWriter;

    impl Write for WriteFailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(5))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailingWriter;

    impl Write for FlushFailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected flush failure",
            ))
        }
    }

    struct PanickingWriter;

    impl Write for PanickingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            panic!("injected renderer panic")
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn has_failure(
        failures: &TerminalProgressFailures,
        kind: TerminalProgressFailureKind,
        operation: TerminalProgressOperation,
    ) -> bool {
        failures
            .failures()
            .iter()
            .any(|failure| failure.kind() == kind && failure.operation() == operation)
    }

    fn phase_label(phase: &Phase) -> String {
        match phase {
            Phase::Planning => String::from("规划"),
            Phase::Translating => String::from("翻译"),
        }
    }

    #[test]
    fn progress_mode_has_exact_names_and_tty_policy() {
        assert_eq!(ProgressMode::NAMES, ["auto", "plain", "off"]);
        for mode in ProgressMode::NAMES {
            assert_eq!(
                mode.parse::<ProgressMode>().expect("模式应有效").as_str(),
                mode
            );
        }
        assert!("AUTO".parse::<ProgressMode>().is_err());

        assert_eq!(ProgressMode::Auto.render_style(true), RenderStyle::Dynamic);
        assert_eq!(ProgressMode::Auto.render_style(false), RenderStyle::Silent);
        assert_eq!(ProgressMode::Plain.render_style(true), RenderStyle::Plain);
        assert_eq!(ProgressMode::Plain.render_style(false), RenderStyle::Plain);
        assert_eq!(ProgressMode::Off.render_style(true), RenderStyle::Silent);
        assert_eq!(ProgressMode::Off.render_style(false), RenderStyle::Silent);
    }

    #[test]
    fn auto_non_tty_and_off_are_completely_silent() {
        for (mode, terminal) in [(ProgressMode::Auto, false), (ProgressMode::Off, true)] {
            let writer = SharedWriter::default();
            let output = writer.clone();
            let progress = TerminalProgress::with_writer(mode, terminal, writer, phase_label);
            progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
            progress.finalizing("正在收尾").expect("静默模式应成功");
            progress
                .safe_stopping("正在安全停止")
                .expect("静默模式应成功");
            progress
                .finish_with_message("完成")
                .expect("静默模式应成功");
            assert!(output.text().is_empty());
        }
    }

    #[test]
    fn renderer_thread_start_failure_is_returned_with_io_context() {
        let progress = TerminalProgress::<Phase>::with_writer_and_spawner(
            ProgressMode::Plain,
            false,
            SharedWriter::default(),
            phase_label,
            |_task| Err(io::Error::from_raw_os_error(8)),
        );
        let failures = progress
            .check_health()
            .expect_err("线程创建失败必须立即进入健康快照");
        let failure = failures.failures().first().expect("必须返回失败");
        assert_eq!(
            failure.kind(),
            TerminalProgressFailureKind::RendererThreadStart
        );
        assert_eq!(
            failure.operation(),
            TerminalProgressOperation::StartRenderer
        );
        assert_eq!(failure.raw_os_error(), Some(8));
        progress.finish().expect_err("收尾必须保留线程创建失败");
    }

    #[test]
    fn finalizing_returns_writer_write_failure_without_waiting_for_finish() {
        let progress = TerminalProgress::with_writer(
            ProgressMode::Plain,
            false,
            WriteFailingWriter,
            phase_label,
        );

        let failures = progress
            .finalizing("正在收尾")
            .expect_err("写入失败必须由 finalizing 立即返回");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::WriterWrite,
            TerminalProgressOperation::RenderStatus,
        ));
        let write_failure = failures
            .failures()
            .iter()
            .find(|failure| failure.kind() == TerminalProgressFailureKind::WriterWrite)
            .expect("必须保留写入失败");
        assert_eq!(write_failure.raw_os_error(), Some(5));

        progress.finish().expect_err("收尾仍必须保留既有失败");
    }

    #[test]
    fn safe_stopping_returns_writer_flush_failure() {
        let progress = TerminalProgress::with_writer(
            ProgressMode::Plain,
            false,
            FlushFailingWriter,
            phase_label,
        );

        let failures = progress
            .safe_stopping("正在安全停止")
            .expect_err("flush 失败必须由 safe_stopping 立即返回");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::WriterFlush,
            TerminalProgressOperation::RenderStatus,
        ));
        let flush_failure = failures
            .failures()
            .iter()
            .find(|failure| failure.kind() == TerminalProgressFailureKind::WriterFlush)
            .expect("必须保留 flush 失败");
        assert_eq!(
            flush_failure.io_error_kind(),
            Some(io::ErrorKind::BrokenPipe)
        );

        progress.finish().expect_err("收尾仍必须保留既有失败");
    }

    #[test]
    fn finish_returns_final_message_write_failure() {
        let progress = TerminalProgress::with_writer(
            ProgressMode::Plain,
            false,
            WriteFailingWriter,
            phase_label,
        );

        let failures = progress
            .finish_with_message("完成")
            .expect_err("最终消息写入失败必须由 finish 返回");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::WriterWrite,
            TerminalProgressOperation::RenderFinalMessage,
        ));
    }

    #[test]
    fn closed_control_channel_is_returned_by_status_and_finish() {
        let progress = TerminalProgress::<Phase>::with_writer_and_spawner(
            ProgressMode::Plain,
            false,
            SharedWriter::default(),
            phase_label,
            |_renderer| Ok(thread::spawn(|| {})),
        );

        let failures = progress
            .finalizing("正在收尾")
            .expect_err("已关闭 channel 必须返回失败");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::ControlChannelClosed,
            TerminalProgressOperation::Finalizing,
        ));

        let failures = progress.finish().expect_err("finish 必须返回 channel 失败");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::ControlChannelClosed,
            TerminalProgressOperation::Finish,
        ));
    }

    #[test]
    fn completion_observer_records_closed_control_channel_in_health_snapshot() {
        let progress = TerminalProgress::<Phase>::with_writer_and_spawner(
            ProgressMode::Plain,
            false,
            SharedWriter::default(),
            phase_label,
            |_renderer| Ok(thread::spawn(|| {})),
        );
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 1, 1));

        let failures = progress
            .check_health()
            .expect_err("观察入口不能返回 Result 时必须记入健康快照");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::ControlChannelClosed,
            TerminalProgressOperation::PublishCompletion,
        ));
        progress.finish().expect_err("收尾必须保留观察失败");
    }

    #[test]
    fn worker_panic_is_returned_by_finish_with_payload() {
        let progress =
            TerminalProgress::with_writer(ProgressMode::Plain, false, PanickingWriter, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 1, 1));

        let failures = progress
            .finish()
            .expect_err("渲染线程 panic 必须由 finish 返回");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::RendererThreadPanicked,
            TerminalProgressOperation::JoinRenderer,
        ));
        assert!(
            failures
                .failures()
                .iter()
                .any(|failure| failure.detail().contains("injected renderer panic")),
            "panic payload 必须保留: {failures}"
        );
    }

    #[test]
    fn snapshots_do_not_regress_within_a_phase() {
        let mut state = RendererState::new();
        assert!(
            state
                .accept_snapshot(ProgressSnapshot::determinate(Phase::Translating, 4, 10))
                .accepted
        );
        assert!(
            !state
                .accept_snapshot(ProgressSnapshot::determinate(Phase::Translating, 3, 10))
                .accepted
        );
        assert!(
            !state
                .accept_snapshot(ProgressSnapshot::indeterminate(Phase::Translating))
                .accepted
        );
        assert!(
            !state
                .accept_snapshot(ProgressSnapshot::determinate(Phase::Translating, 5, 11))
                .accepted
        );
        assert_eq!(
            state.snapshot,
            Some(ProgressSnapshot::determinate(Phase::Translating, 4, 10))
        );

        assert!(
            state
                .accept_snapshot(ProgressSnapshot::indeterminate(Phase::Planning))
                .accepted
        );
    }

    #[test]
    fn plain_output_is_sparse_and_contains_no_terminal_control_sequences() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress =
            TerminalProgress::with_writer(ProgressMode::Plain, false, writer, |_phase: &Phase| {
                String::from("翻译\u{001b}[31m\n阶段")
            });

        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 0, 100));
        thread::sleep(Duration::from_millis(10));
        for completed in 1..10 {
            progress.observe(ProgressSnapshot::determinate(
                Phase::Translating,
                completed,
                100,
            ));
        }
        thread::sleep(Duration::from_millis(10));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 100));
        thread::sleep(Duration::from_millis(10));
        progress
            .safe_stopping("安全\u{001b}[2J\r\n停止")
            .expect("状态应成功呈现");
        thread::sleep(Duration::from_millis(10));
        progress
            .finish_with_message("结束\n完成")
            .expect("收尾应成功");

        let text = output.text();
        assert!(
            !text.contains('\u{001b}'),
            "plain 不得包含 ANSI ESC：{text:?}"
        );
        assert!(!text.contains('\r'), "plain 不得包含回车刷新：{text:?}");
        assert!(text.contains("翻译 [31m 阶段"), "{text:?}");
        assert!(text.contains("安全 [2J  停止"), "{text:?}");
        assert!(text.contains("结束 完成"), "{text:?}");
        assert!(text.lines().count() <= 5, "逐项更新必须被稀疏化：{text:?}");
    }

    #[test]
    fn zero_total_never_renders_zero_over_zero() {
        let mut state = RendererState::new();
        assert!(
            state
                .accept_snapshot(ProgressSnapshot::determinate(Phase::Translating, 0, 0))
                .accepted
        );

        for spinner_frame in [None, Some(0)] {
            let text =
                render_active_line(&phase_label, &state, spinner_frame).expect("零工作量仍有阶段");
            assert!(!text.contains("0/0"), "零工作量不得伪造比例：{text:?}");
            assert!(text.contains("翻译"), "零工作量仍可呈现阶段：{text:?}");
        }
    }

    #[test]
    fn dynamic_tty_uses_ascii_bar_and_clears_without_ansi() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(ProgressMode::Auto, true, writer, phase_label);
        progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
        thread::sleep(Duration::from_millis(15));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 5, 10));
        thread::sleep(DYNAMIC_REFRESH_INTERVAL + Duration::from_millis(20));
        progress.finalizing("正在收尾").expect("状态应呈现");
        thread::sleep(Duration::from_millis(15));
        progress.finish().expect("收尾应成功");

        let text = output.text();
        assert!(text.contains('\r'), "动态终端必须使用单行刷新：{text:?}");
        assert!(
            text.contains('#'),
            "动态终端必须包含 ASCII 进度条：{text:?}"
        );
        assert!(
            text.contains('-'),
            "动态终端必须包含 ASCII 进度条：{text:?}"
        );
        assert!(text.contains("正在收尾"), "{text:?}");
        assert!(
            !text.contains('\u{001b}'),
            "不需要 ANSI 也能完成渲染：{text:?}"
        );
    }

    #[test]
    fn completion_count_is_rendered_before_finalizing_status() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress =
            TerminalProgress::with_writer(ProgressMode::Plain, false, writer, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 10));
        progress.finalizing("正在保存运行方案").expect("状态应呈现");
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let completed = text.find("10/10").expect("必须呈现最终确认计数");
        let finalizing = text.find("正在保存运行方案").expect("必须呈现收尾阶段");
        assert!(completed < finalizing, "最终计数必须先于收尾阶段：{text:?}");
    }

    #[test]
    fn completed_snapshot_survives_latest_slot_contention() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress =
            TerminalProgress::with_writer(ProgressMode::Plain, false, writer, phase_label);
        let observer = progress.observer();
        let latest = Arc::clone(
            &observer
                .dispatch
                .as_ref()
                .expect("plain 模式必须启动进度渲染器")
                .latest,
        );
        let pending_guard = lock_after_poison(&latest.pending);
        let (published_sender, published_receiver) = mpsc::channel();

        let publisher = thread::spawn(move || {
            observer.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 10));
            published_sender
                .send(())
                .expect("测试协调 channel 不应关闭");
        });
        published_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("发布最终快照不得等待最新值槽的互斥锁");
        publisher.join().expect("发布线程不应 panic");

        progress.finalizing("正在保存运行方案").expect("状态应呈现");
        drop(pending_guard);
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let completed = text.find("10/10").expect("锁竞争不能丢失最终确认计数");
        let finalizing = text.find("正在保存运行方案").expect("必须呈现收尾阶段");
        assert!(completed < finalizing, "最终计数必须先于收尾阶段：{text:?}");
    }
}
