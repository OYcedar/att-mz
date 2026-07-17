//! 可信 Lua VM 的根执行契约与 Host 绑定面。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::att_mz::translate::executor::LlmResponse;
use crate::att_mz::translate::standard::ChatMessage;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

use super::{LuaPhase, LuaProjectContext};

type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// 已完整读取并可交给专用 Lua worker 的主程序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnedLuaProgram {
    main_script_path: PathBuf,
    source: Vec<u8>,
}

impl OwnedLuaProgram {
    pub(crate) fn new(main_script_path: PathBuf, source: Vec<u8>) -> Self {
        Self {
            main_script_path,
            source,
        }
    }

    pub(crate) fn main_script_path(&self) -> &Path {
        &self.main_script_path
    }

    pub(crate) fn source(&self) -> &[u8] {
        &self.source
    }
}

/// Lua 主程序离开 VM 时的终止方式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaRuntimeTermination {
    Completed,
    Failed,
    Cancelled,
}

/// Host 在释放绑定资源后交还给编排层的事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaBindingFinalization {
    had_active_transaction: bool,
}

impl TrustedLuaBindingFinalization {
    pub(crate) const fn new(had_active_transaction: bool) -> Self {
        Self {
            had_active_transaction,
        }
    }

    pub(crate) const fn had_active_transaction(self) -> bool {
        self.had_active_transaction
    }
}

/// Lua 能通过 `pcall` 检查的 Host 错误事实。
///
/// `domain` 与 `kind` 是稳定的机器字段；`message` 只用于人类诊断。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaHostCallError {
    domain: &'static str,
    kind: &'static str,
    message: String,
    retry_after_ms: Option<u64>,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaHostCallError {
    pub(crate) fn new(
        domain: &'static str,
        kind: &'static str,
        message: impl Into<String>,
        retry_after_ms: Option<u64>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            domain,
            kind,
            message: message.into(),
            retry_after_ms,
            source,
        }
    }

    pub(crate) const fn domain(&self) -> &'static str {
        self.domain
    }

    pub(crate) const fn kind(&self) -> &'static str {
        self.kind
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }
}

impl fmt::Display for TrustedLuaHostCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TrustedLuaHostCallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// 唯一终结器失败。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaBindingFinalizationError {
    message: String,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaBindingFinalizationError {
    pub(crate) fn new(
        message: impl Into<String>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            message: message.into(),
            source,
        }
    }

    fn supervisor_lost() -> Self {
        Self::new("Lua job supervisor 在资源终结前退出", None)
    }
}

impl fmt::Display for TrustedLuaBindingFinalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TrustedLuaBindingFinalizationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Lua VM 可调用的 Host 能力。
///
/// 这一调用面可共享，但不拥有会话终结权。VM worker 只能将请求交回主
/// Tokio runtime 执行，不得在 Lua worker 内直接操作 SQLite 连接或网络客户端。
pub(crate) trait TrustedLuaHostCalls: Send + Sync + 'static {
    fn phase(&self) -> LuaPhase;
    fn project(&self) -> &LuaProjectContext;

    fn query(
        &self,
        query: SqliteQuery,
    ) -> HostFuture<Result<Vec<SqliteRow>, TrustedLuaHostCallError>>;

    fn execute(&self, command: SqliteCommand) -> HostFuture<Result<u64, TrustedLuaHostCallError>>;

    fn begin(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;
    fn commit(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;
    fn rollback(&self) -> HostFuture<Result<(), TrustedLuaHostCallError>>;

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> HostFuture<Result<LlmResponse, TrustedLuaHostCallError>>;
}

/// Host 会话的唯一终结权。
///
/// 终结器不可克隆；`finalize(self)` 通过按值消费保证最多执行一次。
pub(crate) trait TrustedLuaBindingFinalizer: Send + 'static {
    fn finalize(
        self: Box<Self>,
        termination: TrustedLuaRuntimeTermination,
    ) -> HostFuture<Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>>;
}

/// 一次 Runtime 启动所需的共享调用面与唯一终结器。
#[must_use = "Lua bindings 必须同步移交给 Runtime reservation"]
pub(crate) struct TrustedLuaRuntimeBindings {
    calls: Arc<dyn TrustedLuaHostCalls>,
    finalizer: Box<dyn TrustedLuaBindingFinalizer>,
}

impl TrustedLuaRuntimeBindings {
    pub(crate) fn new(
        calls: Arc<dyn TrustedLuaHostCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> Self {
        Self { calls, finalizer }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<dyn TrustedLuaHostCalls>,
        Box<dyn TrustedLuaBindingFinalizer>,
    ) {
        (self.calls, self.finalizer)
    }
}

/// Lua 根执行器自身的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaRuntimeExecutionError<R> {
    Unavailable(R),
    Context(R),
    Compile(R),
    Execute(R),
    Binding(TrustedLuaHostCallError),
    Cancelled,
    WorkerPanicked,
    SupervisorLost,
}

impl<R> fmt::Display for TrustedLuaRuntimeExecutionError<R>
where
    R: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "Lua 执行器不可用：{source}"),
            Self::Context(source) => write!(formatter, "Lua 上下文构造失败：{source}"),
            Self::Compile(source) => write!(formatter, "Lua 主程序编译失败：{source}"),
            Self::Execute(source) => write!(formatter, "Lua 主程序运行失败：{source}"),
            Self::Binding(source) => write!(formatter, "Lua Host 能力调用失败：{source}"),
            Self::Cancelled => formatter.write_str("Lua 主程序已取消"),
            Self::WorkerPanicked => formatter.write_str("Lua worker 意外 panic"),
            Self::SupervisorLost => formatter.write_str("Lua job supervisor 意外退出"),
        }
    }
}

impl<R> Error for TrustedLuaRuntimeExecutionError<R>
where
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(source)
            | Self::Context(source)
            | Self::Compile(source)
            | Self::Execute(source) => Some(source),
            Self::Binding(source) => Some(source),
            Self::Cancelled | Self::WorkerPanicked | Self::SupervisorLost => None,
        }
    }
}

/// VM 执行与 Host 资源收尾的两个独立终态。
pub(crate) struct TrustedLuaRuntimeExecutionReport<R> {
    runtime: Result<(), TrustedLuaRuntimeExecutionError<R>>,
    finalization: Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
}

impl<R> TrustedLuaRuntimeExecutionReport<R> {
    pub(crate) fn new(
        runtime: Result<(), TrustedLuaRuntimeExecutionError<R>>,
        finalization: Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
    ) -> Self {
        Self {
            runtime,
            finalization,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Result<(), TrustedLuaRuntimeExecutionError<R>>,
        Result<TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError>,
    ) {
        (self.runtime, self.finalization)
    }
}

/// `start` 同步移交资源后返回的执行句柄。
///
/// 丢弃句柄只请求合作式取消；job supervisor 继续拥有唯一终结器并完成收尾。
pub(crate) struct TrustedLuaExecutionHandle<R> {
    receiver: oneshot::Receiver<TrustedLuaRuntimeExecutionReport<R>>,
    cancelled: Arc<AtomicBool>,
    completed: bool,
}

impl<R> TrustedLuaExecutionHandle<R> {
    pub(crate) fn new(
        receiver: oneshot::Receiver<TrustedLuaRuntimeExecutionReport<R>>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            receiver,
            cancelled,
            completed: false,
        }
    }
}

impl<R> Future for TrustedLuaExecutionHandle<R> {
    type Output = TrustedLuaRuntimeExecutionReport<R>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(context) {
            Poll::Ready(Ok(report)) => {
                self.completed = true;
                Poll::Ready(report)
            }
            Poll::Ready(Err(_)) => {
                self.completed = true;
                Poll::Ready(TrustedLuaRuntimeExecutionReport::new(
                    Err(TrustedLuaRuntimeExecutionError::SupervisorLost),
                    Err(TrustedLuaBindingFinalizationError::supervisor_lost()),
                ))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R> Drop for TrustedLuaExecutionHandle<R> {
    fn drop(&mut self) {
        if !self.completed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

/// 已预留的 Lua 队列与 worker 容量。
///
/// reservation 不可克隆；丢弃只释放容量。`start` 不返回移交失败，一旦接收
/// bindings，Runtime 必须最终产生执行与清理报告。
pub(crate) trait TrustedLuaRuntimeReservation: Send + 'static {
    type Error: Error + Send + Sync + 'static;

    fn start(
        self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error>;
}

/// 在外部配置的专用有界 worker 与队列中运行完全可信的 Lua 程序。
pub(crate) trait TrustedLuaRuntimeExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type Reservation: TrustedLuaRuntimeReservation<Error = Self::Error>;

    fn reserve(&self) -> impl Future<Output = Result<Self::Reservation, Self::Error>> + Send;
}
