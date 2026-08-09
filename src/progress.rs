//! 终端进度的共享契约与输出根适配器。
//!
//! 业务切片拥有阶段枚举，并只发布绝对快照；本模块不解释业务阶段。观察入口在生产者侧
//! 验证单调性并按整数百分比去重，随后把需要显示的普通文本行送入唯一 writer。进度、
//! 收尾和安全停止共用同一 FIFO，因此已经确认的进度不会被后续状态越过。线程、channel
//! 和终端 I/O 故障会记入统一健康快照，并由显式收尾结果返回给进程边界。

use std::fmt;
use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RelatedFailureRelation, RuntimeComponent,
    RuntimeIssue, RuntimeOperation, StateEffect,
};

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
/// 实现可以合并或丢弃不会改变整数百分比的中间快照；调用方必须始终发布绝对值，不能
/// 依赖增量事件。终端实现会把需要显示的快照按发布顺序交给 FIFO；静默观察者仍然有意
/// 忽略全部快照。
pub(crate) trait ProgressObserver<P>: Send + Sync {
    fn observe(&self, snapshot: ProgressSnapshot<P>);
}

/// 不需要实时进度时使用的空观察者。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopProgressObserver;

impl<P> ProgressObserver<P> for NoopProgressObserver {
    fn observe(&self, _snapshot: ProgressSnapshot<P>) {}
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
    PublishLine,
    RenderLine,
    RenderStatus,
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
            Self::PublishLine => "publish_line",
            Self::RenderLine => "render_line",
            Self::RenderStatus => "render_status",
            Self::RenderFinalMessage => "render_final_message",
            Self::Finalizing => "finalizing",
            Self::SafeStopping => "safe_stopping",
            Self::Finish => "finish",
            Self::JoinRenderer => "join_renderer",
        }
    }

    const fn diagnostic_operation(self) -> RuntimeOperation {
        match self {
            Self::StartRenderer => RuntimeOperation::StartTerminalProgressRenderer,
            Self::PublishLine => RuntimeOperation::PublishTerminalProgressLine,
            Self::RenderLine => RuntimeOperation::RenderTerminalProgressLine,
            Self::RenderStatus => RuntimeOperation::RenderTerminalProgressStatus,
            Self::RenderFinalMessage => RuntimeOperation::RenderTerminalProgressFinalMessage,
            Self::Finalizing => RuntimeOperation::FinalizeTerminalProgress,
            Self::SafeStopping => RuntimeOperation::ReportTerminalProgressSafeStop,
            Self::Finish => RuntimeOperation::FinishTerminalProgress,
            Self::JoinRenderer => RuntimeOperation::JoinTerminalProgressRenderer,
        }
    }
}

/// 一项已经确认的终端进度呈现失败。
#[derive(Clone, Debug)]
pub(crate) struct TerminalProgressFailure {
    kind: TerminalProgressFailureKind,
    operation: TerminalProgressOperation,
    source: Option<Arc<io::Error>>,
}

impl TerminalProgressFailure {
    fn io(
        kind: TerminalProgressFailureKind,
        operation: TerminalProgressOperation,
        source: io::Error,
    ) -> Self {
        Self {
            kind,
            operation,
            source: Some(Arc::new(source)),
        }
    }

    fn channel_closed(operation: TerminalProgressOperation) -> Self {
        Self {
            kind: TerminalProgressFailureKind::ControlChannelClosed,
            operation,
            source: None,
        }
    }

    fn worker_panicked() -> Self {
        Self {
            kind: TerminalProgressFailureKind::RendererThreadPanicked,
            operation: TerminalProgressOperation::JoinRenderer,
            source: None,
        }
    }

    #[cfg(test)]
    pub(crate) const fn kind(&self) -> TerminalProgressFailureKind {
        self.kind
    }

    #[cfg(test)]
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

    /// 投影一项终端呈现收尾失败；panic 正文不进入公开诊断。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let operation = self.operation.diagnostic_operation();
        let issue = match self.kind {
            TerminalProgressFailureKind::RendererThreadStart
            | TerminalProgressFailureKind::WriterWrite
            | TerminalProgressFailureKind::WriterFlush => RuntimeIssue::Io {
                component: RuntimeComponent::TerminalProgress,
                operation,
                failure: self.source.as_deref().map_or_else(
                    || IoFailure::from_parts(io::ErrorKind::Other.into(), None),
                    IoFailure::from_error,
                ),
            },
            TerminalProgressFailureKind::ControlChannelClosed => RuntimeIssue::ExecutorClosed {
                component: RuntimeComponent::TerminalProgress,
                operation,
            },
            TerminalProgressFailureKind::RendererThreadPanicked => RuntimeIssue::WorkerPanicked {
                component: RuntimeComponent::TerminalProgress,
                operation,
            },
        };
        DiagnosticReport::new(
            StateEffect::AppliedFinalizationFailed,
            Diagnostic::runtime(issue),
        )
    }

    fn same_fact(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.operation == other.operation
            && self.io_error_kind() == other.io_error_kind()
            && self.raw_os_error() == other.raw_os_error()
    }
}

impl fmt::Display for TerminalProgressFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "terminal progress {} failed during {}",
            self.kind.as_str(),
            self.operation.as_str()
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

    /// 保留首项和全部后续失败的自然发生顺序；每项都是独立的 Shutdown 相关失败。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let Some((primary, related)) = self.failures.split_first() else {
            unreachable!("TerminalProgressFailures 只会由至少一项已记录失败构造")
        };
        related
            .iter()
            .fold(primary.diagnostic_report(), |report, failure| {
                report.with_related(
                    RelatedFailureRelation::Shutdown,
                    failure.diagnostic_report(),
                )
            })
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

fn lock_after_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct PublishState<P> {
    sender: mpsc::Sender<RendererControl>,
    snapshot: Option<ProgressSnapshot<P>>,
}

struct SnapshotDispatch<P> {
    state: Mutex<PublishState<P>>,
    render_phase: Arc<dyn Fn(&P) -> String + Send + Sync>,
    no_work_message: String,
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
        P: Eq + Send + 'static,
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
    P: Eq + Send + 'static,
{
    fn observe(&self, mut snapshot: ProgressSnapshot<P>) {
        let Some(dispatch) = &self.dispatch else {
            return;
        };

        snapshot.amount = snapshot.amount.normalized();
        let mut state = lock_after_poison(&dispatch.state);
        let update = snapshot_update(state.snapshot.as_ref(), &snapshot);
        if !update.accepted {
            return;
        }

        let line = update.emitted.then(|| {
            render_progress_line(
                dispatch.render_phase.as_ref(),
                &dispatch.no_work_message,
                &snapshot,
            )
        });
        state.snapshot = Some(snapshot);
        if let Some(line) = line
            && state.sender.send(RendererControl::Progress(line)).is_err()
        {
            dispatch
                .health
                .record(TerminalProgressFailure::channel_closed(
                    TerminalProgressOperation::PublishLine,
                ));
        }
    }
}

enum RendererControl {
    Progress(String),
    Status {
        message: String,
        operation: TerminalProgressOperation,
        acknowledgement: mpsc::Sender<()>,
    },
    Finish(Option<String>),
}

struct RendererControlHandle<P> {
    dispatch: Arc<SnapshotDispatch<P>>,
}

impl<P> RendererControlHandle<P> {
    fn send_status(
        &self,
        message: String,
        operation: TerminalProgressOperation,
    ) -> Result<(), TerminalProgressFailures> {
        let (acknowledgement, acknowledged) = mpsc::channel();
        let send_result =
            lock_after_poison(&self.dispatch.state)
                .sender
                .send(RendererControl::Status {
                    message: sanitize_terminal_line(&message),
                    operation,
                    acknowledgement,
                });
        if send_result.is_err() {
            self.dispatch
                .health
                .record(TerminalProgressFailure::channel_closed(operation));
            return self.dispatch.health.result();
        }
        if acknowledged.recv().is_err() {
            self.dispatch
                .health
                .record(TerminalProgressFailure::channel_closed(operation));
        }
        self.dispatch.health.result()
    }

    fn send_finish(&self, message: Option<String>) {
        let message = message.map(|message| sanitize_terminal_line(&message));
        if lock_after_poison(&self.dispatch.state)
            .sender
            .send(RendererControl::Finish(message))
            .is_err()
        {
            self.dispatch
                .health
                .record(TerminalProgressFailure::channel_closed(
                    TerminalProgressOperation::Finish,
                ));
        }
    }
}

/// 拥有后台终端渲染生命周期的进度控制器。
///
/// `render_phase` 只负责把切片自己的阶段枚举转换为本地化标签。确定进度的百分比与
/// `completed/total` 由本模块追加并使用 Unicode 方向隔离；零工作量使用调用方提供的
/// 本地化说明，永远不会生成 `0/0` 或虚假的 `100%`。
#[must_use = "进度控制器必须存活到命令结束，以便完成后台 writer 收尾"]
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
        P: Eq + Send + 'static,
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

    /// 排空已经发布的进度行、等待 writer 结束，并返回命令期间已经确认的全部呈现失败。
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
            && worker.join().is_err()
        {
            // panic payload 可能包含游戏正文、模型响应或路径；终端呈现只能公开已确认的
            // worker/component/operation，不能把 payload 转为字符串或写入日志。
            self.health
                .record(TerminalProgressFailure::worker_panicked());
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
    /// 使用进程标准错误流创建普通换行输出器。
    pub(crate) fn stderr<R>(render_phase: R, no_work_message: String) -> Self
    where
        R: Fn(&P) -> String + Send + Sync + 'static,
    {
        Self::with_writer_inner(
            io::stderr(),
            render_phase,
            no_work_message,
            spawn_renderer_thread,
        )
    }

    /// 使用调用方提供的输出能力创建渲染器。
    ///
    /// 渲染线程无法创建时，返回携带启动失败健康状态的静默控制器；调用方仍可执行业务，
    /// 但必须处理后续健康检查或收尾结果。
    #[cfg(test)]
    pub(crate) fn with_writer<W, R>(writer: W, render_phase: R) -> Self
    where
        W: Write + Send + 'static,
        R: Fn(&P) -> String + Send + Sync + 'static,
    {
        Self::with_writer_and_spawner(
            writer,
            render_phase,
            String::from("无需处理"),
            spawn_renderer_thread,
        )
    }

    #[cfg(test)]
    fn with_writer_and_spawner<W, R, S>(
        writer: W,
        render_phase: R,
        no_work_message: String,
        spawn: S,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Fn(&P) -> String + Send + Sync + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>>,
    {
        Self::with_writer_inner(writer, render_phase, no_work_message, spawn)
    }

    fn with_writer_inner<W, R, S>(
        writer: W,
        render_phase: R,
        no_work_message: String,
        spawn: S,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Fn(&P) -> String + Send + Sync + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>>,
    {
        let (control_sender, control_receiver) = mpsc::channel();
        let health = Arc::new(ProgressHealth::default());
        let worker_health = Arc::clone(&health);
        let worker = spawn(Box::new(move || {
            run_renderer(writer, control_receiver, worker_health);
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
        let dispatch = Arc::new(SnapshotDispatch {
            state: Mutex::new(PublishState {
                sender: control_sender,
                snapshot: None,
            }),
            render_phase: Arc::new(render_phase),
            no_work_message: sanitize_terminal_line(&no_work_message),
            health: Arc::clone(&health),
        });
        Self {
            observer: TerminalProgressObserver {
                dispatch: Some(Arc::clone(&dispatch)),
            },
            control: Some(RendererControlHandle { dispatch }),
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SnapshotUpdate {
    accepted: bool,
    emitted: bool,
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
            emitted: true,
        };
    };
    if previous.phase != incoming.phase {
        return SnapshotUpdate {
            accepted: true,
            emitted: true,
        };
    }

    match (previous.amount, incoming.amount) {
        (ProgressAmount::Indeterminate, ProgressAmount::Indeterminate) => SnapshotUpdate::default(),
        (ProgressAmount::Determinate { .. }, ProgressAmount::Indeterminate) => {
            SnapshotUpdate::default()
        }
        (ProgressAmount::Indeterminate, ProgressAmount::Determinate { .. }) => SnapshotUpdate {
            accepted: true,
            emitted: true,
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
                emitted: total > 0
                    && integer_percentage(completed, total)
                        != integer_percentage(previous_completed, previous_total),
            }
        }
    }
}

fn integer_percentage(completed: u64, total: u64) -> u8 {
    debug_assert!(total > 0);
    ((u128::from(completed) * 100) / u128::from(total)) as u8
}

fn render_progress_line<P, R>(
    render_phase: &R,
    no_work_message: &str,
    snapshot: &ProgressSnapshot<P>,
) -> String
where
    R: Fn(&P) -> String + ?Sized,
{
    let label = sanitize_terminal_line(&render_phase(&snapshot.phase));
    match snapshot.amount {
        ProgressAmount::Indeterminate => label,
        ProgressAmount::Determinate { total: 0, .. } => {
            format!("{label}: {no_work_message}")
        }
        ProgressAmount::Determinate { completed, total } => {
            let percentage = integer_percentage(completed, total);
            format!(
                "{label}: {FIRST_STRONG_ISOLATE}{percentage}% ({completed}/{total}){POP_DIRECTIONAL_ISOLATE}"
            )
        }
    }
}

fn run_renderer<W>(
    mut writer: W,
    control_receiver: mpsc::Receiver<RendererControl>,
    health: Arc<ProgressHealth>,
) where
    W: Write,
{
    while let Ok(control) = control_receiver.recv() {
        match control {
            RendererControl::Progress(line) => {
                if write_line(
                    &mut writer,
                    &line,
                    TerminalProgressOperation::RenderLine,
                    &health,
                )
                .is_err()
                {
                    return;
                }
            }
            RendererControl::Status {
                message,
                operation,
                acknowledgement,
            } => {
                let written = write_line(
                    &mut writer,
                    &message,
                    TerminalProgressOperation::RenderStatus,
                    &health,
                );
                if acknowledgement.send(()).is_err() {
                    health.record(TerminalProgressFailure::channel_closed(operation));
                }
                if written.is_err() {
                    return;
                }
            }
            RendererControl::Finish(message) => {
                if let Some(message) = message {
                    let _ = write_line(
                        &mut writer,
                        &message,
                        TerminalProgressOperation::RenderFinalMessage,
                        &health,
                    );
                }
                return;
            }
        }
    }
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

fn write_line<W: Write>(
    writer: &mut W,
    line: &str,
    operation: TerminalProgressOperation,
    health: &ProgressHealth,
) -> Result<(), ()> {
    write_and_flush(writer, operation, health, |writer| {
        writeln!(writer, "{line}")
    })
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
    fn non_tty_uses_the_same_plain_lines_as_a_terminal() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
        progress.finalizing("正在收尾").expect("状态应成功输出");
        progress
            .safe_stopping("正在安全停止")
            .expect("状态应成功输出");
        progress
            .finish_with_message("完成")
            .expect("最终消息应成功输出");
        assert_eq!(
            output.text().lines().collect::<Vec<_>>(),
            ["规划", "正在收尾", "正在安全停止", "完成"]
        );
    }

    #[test]
    fn renderer_thread_start_failure_is_returned_with_io_context() {
        let progress = TerminalProgress::<Phase>::with_writer_and_spawner(
            SharedWriter::default(),
            phase_label,
            String::from("无需处理"),
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
        let progress = TerminalProgress::with_writer(WriteFailingWriter, phase_label);

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
        let progress = TerminalProgress::with_writer(FlushFailingWriter, phase_label);

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
        let progress = TerminalProgress::with_writer(WriteFailingWriter, phase_label);

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
            SharedWriter::default(),
            phase_label,
            String::from("无需处理"),
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
    fn progress_observer_records_closed_control_channel_in_health_snapshot() {
        let progress = TerminalProgress::<Phase>::with_writer_and_spawner(
            SharedWriter::default(),
            phase_label,
            String::from("无需处理"),
            |_renderer| Ok(thread::spawn(|| {})),
        );
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 1, 1));

        let failures = progress
            .check_health()
            .expect_err("观察入口不能返回 Result 时必须记入健康快照");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::ControlChannelClosed,
            TerminalProgressOperation::PublishLine,
        ));
        progress.finish().expect_err("收尾必须保留观察失败");
    }

    #[test]
    fn worker_panic_is_returned_without_exposing_payload() {
        let progress = TerminalProgress::with_writer(PanickingWriter, phase_label);
        let _ = progress.finalizing("正在收尾");

        let failures = progress
            .finish()
            .expect_err("渲染线程 panic 必须由 finish 返回");
        assert!(has_failure(
            &failures,
            TerminalProgressFailureKind::RendererThreadPanicked,
            TerminalProgressOperation::JoinRenderer,
        ));
        let rendered = failures.to_string();
        let diagnostic =
            serde_json::to_string(&failures.diagnostic_report()).expect("进度诊断必须可序列化");
        assert!(
            !rendered.contains("injected renderer panic")
                && !diagnostic.contains("injected renderer panic"),
            "panic payload 不能进入进程呈现或结构化诊断"
        );
    }

    #[test]
    fn progress_failure_uses_io_category_without_exposing_error_message() {
        let failure = TerminalProgressFailure::io(
            TerminalProgressFailureKind::WriterWrite,
            TerminalProgressOperation::RenderStatus,
            io::Error::other("progress secret sentinel"),
        );
        let rendered = failure.to_string();
        let diagnostic =
            serde_json::to_string(&failure.diagnostic_report()).expect("进度诊断必须可序列化");
        assert_eq!(failure.io_error_kind(), Some(io::ErrorKind::Other));
        assert!(
            !rendered.contains("progress secret sentinel")
                && !diagnostic.contains("progress secret sentinel"),
            "I/O 错误正文不能成为呈现或诊断协议的一部分"
        );
    }

    #[test]
    fn snapshots_do_not_regress_within_a_phase() {
        let previous = ProgressSnapshot::determinate(Phase::Translating, 4, 10);
        assert!(
            !snapshot_update(
                Some(&previous),
                &ProgressSnapshot::determinate(Phase::Translating, 3, 10)
            )
            .accepted
        );
        assert!(
            !snapshot_update(
                Some(&previous),
                &ProgressSnapshot::indeterminate(Phase::Translating)
            )
            .accepted
        );
        assert!(
            !snapshot_update(
                Some(&previous),
                &ProgressSnapshot::determinate(Phase::Translating, 5, 11)
            )
            .accepted
        );
        let phase_change = snapshot_update(
            Some(&previous),
            &ProgressSnapshot::indeterminate(Phase::Planning),
        );
        assert!(phase_change.accepted && phase_change.emitted);
    }

    #[test]
    fn advancing_inside_the_same_integer_percentage_updates_state_without_emitting() {
        let previous = ProgressSnapshot::determinate(Phase::Translating, 1, 1_000);
        let update = snapshot_update(
            Some(&previous),
            &ProgressSnapshot::determinate(Phase::Translating, 9, 1_000),
        );
        assert!(update.accepted);
        assert!(!update.emitted);

        let changed_percentage = snapshot_update(
            Some(&previous),
            &ProgressSnapshot::determinate(Phase::Translating, 10, 1_000),
        );
        assert!(changed_percentage.accepted && changed_percentage.emitted);
    }

    #[test]
    fn plain_output_sanitizes_terminal_control_sequences() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, |_phase: &Phase| {
            String::from("翻译\u{001b}[31m\n阶段")
        });

        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 5, 10));
        progress
            .safe_stopping("安全\u{001b}[2J\r\n停止")
            .expect("状态应成功呈现");
        progress
            .finish_with_message("结束\n完成")
            .expect("收尾应成功");

        let text = output.text();
        assert!(
            !text.contains('\u{001b}'),
            "进度不得包含 ANSI ESC：{text:?}"
        );
        assert!(!text.contains('\r'), "普通进度不得使用回车刷新：{text:?}");
        assert!(text.contains("翻译 [31m 阶段"), "{text:?}");
        assert!(text.contains("安全 [2J  停止"), "{text:?}");
        assert!(text.contains("结束 完成"), "{text:?}");
    }

    #[test]
    fn zero_total_never_renders_zero_over_zero() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 0, 0));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 0, 0));
        progress.finish().expect("收尾应成功");

        let text = output.text();
        assert!(!text.contains("0/0"), "零工作量不得伪造比例：{text:?}");
        assert!(!text.contains("100%"), "零工作量不得伪造完成：{text:?}");
        assert_eq!(text, "翻译: 无需处理\n");
    }

    #[test]
    fn indeterminate_progress_prints_one_phase_line_without_a_spinner() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
        progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
        progress.finish().expect("收尾应成功");

        assert_eq!(output.text(), "规划\n");
    }

    #[test]
    fn determinate_progress_prints_only_observed_integer_percentage_changes() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        for completed in [0, 1, 9, 10, 11, 20, 330, 360, 1_000] {
            progress.observe(ProgressSnapshot::determinate(
                Phase::Translating,
                completed,
                1_000,
            ));
        }
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            6,
            "只应打印 0、1、2、33、36、100 六个百分比：{text:?}"
        );
        for expected in ["0%", "1%", "2%", "33%", "36%", "100%"] {
            assert!(
                lines.iter().any(|line| line.contains(expected)),
                "缺少已观测百分比 {expected}：{text:?}"
            );
        }
        assert!(
            !text.contains("34%") && !text.contains("35%"),
            "不得补造跳过的百分比：{text:?}"
        );
        assert!(
            !text.contains('\r') && !text.contains('\u{001b}'),
            "只能使用普通换行：{text:?}"
        );
    }

    #[test]
    fn a_phase_emits_at_most_101_percentage_lines() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        for completed in 0..=10_000 {
            progress.observe(ProgressSnapshot::determinate(
                Phase::Translating,
                completed,
                10_000,
            ));
        }
        progress.finish().expect("收尾应成功");

        assert_eq!(output.text().lines().count(), 101);
    }

    #[test]
    fn completion_count_is_rendered_before_finalizing_status() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 10));
        progress.finalizing("正在保存运行方案").expect("状态应呈现");
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let completed = text.find("10/10").expect("必须呈现最终确认计数");
        let finalizing = text.find("正在保存运行方案").expect("必须呈现收尾阶段");
        assert!(completed < finalizing, "最终计数必须先于收尾阶段：{text:?}");
    }

    #[test]
    fn safe_stopping_preserves_incomplete_progress_without_fabricating_completion() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 36, 100));
        progress
            .safe_stopping("正在安全停止")
            .expect("安全停止状态应呈现");
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let confirmed = text.find("36% (36/100)").expect("必须显示最后确认进度");
        let stopping = text.find("正在安全停止").expect("必须显示安全停止");
        assert!(confirmed < stopping, "确认进度必须先于安全停止：{text:?}");
        assert!(!text.contains("100%"), "取消收尾不得伪造完成：{text:?}");
    }

    #[test]
    fn first_phase_snapshot_in_fifo_precedes_finalizing_status() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let (renderer_start, renderer_start_receiver) = mpsc::channel();
        let progress = TerminalProgress::with_writer_and_spawner(
            writer,
            phase_label,
            String::from("无需处理"),
            move |renderer| {
                Ok(thread::spawn(move || {
                    renderer_start_receiver
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .expect("测试应允许渲染线程开始");
                    renderer();
                }))
            },
        );
        progress.observe(ProgressSnapshot::indeterminate(Phase::Planning));
        renderer_start.send(()).expect("测试渲染线程不应关闭");
        progress.finalizing("正在收尾").expect("状态应呈现");
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let planning = text.find("规划").expect("锁竞争不能丢失首个阶段快照");
        let finalizing = text.find("正在收尾").expect("必须呈现收尾阶段");
        assert!(
            planning < finalizing,
            "FIFO 中先发布的阶段快照必须先于收尾阶段：{text:?}"
        );
    }

    #[test]
    fn a_suppressed_snapshot_still_prevents_later_regression() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 1, 1_000));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 9, 1_000));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 5, 1_000));
        progress.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 1_000));
        progress.finish().expect("收尾应成功");

        let text = output.text();
        assert_eq!(
            text.lines().count(),
            2,
            "0% 内的更新和倒退都不得增加输出：{text:?}"
        );
        assert!(!text.contains("(5/1000)"), "被拒绝的倒退不得输出：{text:?}");
        assert!(text.contains("(1/1000)") && text.contains("(10/1000)"));
    }

    #[test]
    fn publisher_waits_for_sequence_lock_and_completion_still_precedes_finalizing() {
        let writer = SharedWriter::default();
        let output = writer.clone();
        let progress = TerminalProgress::with_writer(writer, phase_label);
        let observer = progress.observer();
        let dispatch = Arc::clone(
            &observer
                .dispatch
                .as_ref()
                .expect("所有输出环境必须启动进度 writer"),
        );
        let sequence_guard = lock_after_poison(&dispatch.state);
        let (published_sender, published_receiver) = mpsc::channel();

        let publisher = thread::spawn(move || {
            observer.observe(ProgressSnapshot::determinate(Phase::Translating, 10, 10));
            published_sender
                .send(())
                .expect("测试协调 channel 不应关闭");
        });
        assert!(
            matches!(
                published_receiver.recv_timeout(std::time::Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "发布必须等待顺序锁，不能绕过已经占用的发布位置"
        );

        drop(sequence_guard);
        published_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("释放顺序锁后必须完成发布");
        publisher.join().expect("发布线程不应 panic");

        progress.finalizing("正在保存运行方案").expect("状态应呈现");
        progress.finish().expect("收尾应成功");

        let text = output.text();
        let completed = text.find("10/10").expect("锁竞争不能丢失最终确认计数");
        let finalizing = text.find("正在保存运行方案").expect("必须呈现收尾阶段");
        assert!(completed < finalizing, "最终计数必须先于收尾阶段：{text:?}");
    }
}
