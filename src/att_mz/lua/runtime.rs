//! 可信 Lua VM 的根执行契约与 Host 绑定面。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::att_mz::translate::executor::LlmResponse;
use crate::att_mz::translate::standard::ChatMessage;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

use super::{LuaPhase, LuaProjectContext};

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

/// Lua VM 通过这些高级调用访问 Host 明确注入的项目能力。
///
/// 真实 VM 适配器只把这些方法映射到 `ctx`；它不得读取 Profile、凭据或底层连接。
/// `finalize` 由 Runtime 根在成功、失败和取消后都调用。`execute` 的 Future 一旦首次
/// 被轮询并接管 `bindings`，即使任务仍在有界队列中、尚未进入 worker，或调用 Future
/// 随后被丢弃，根实现也必须以 `Cancelled` 终态执行一次 `finalize`。这样 Host 在提交
/// Runtime 前已经打开的数据库会话不会因排队期取消而泄漏。
pub(crate) trait TrustedLuaHostBindings: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn phase(&self) -> LuaPhase;
    fn project(&self) -> &LuaProjectContext;

    fn query(
        &self,
        query: SqliteQuery,
    ) -> impl Future<Output = Result<Vec<SqliteRow>, Self::Error>> + Send;

    fn execute(
        &self,
        command: SqliteCommand,
    ) -> impl Future<Output = Result<u64, Self::Error>> + Send;

    fn begin(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn commit(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn rollback(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> impl Future<Output = Result<LlmResponse, Self::Error>> + Send;

    fn finalize(
        &self,
        termination: TrustedLuaRuntimeTermination,
    ) -> impl Future<Output = Result<TrustedLuaBindingFinalization, Self::Error>> + Send;
}

/// Lua 根执行器自身的失败，不包含 Host 绑定方法返回的错误。
#[derive(Debug)]
pub(crate) enum TrustedLuaRuntimeExecutionError<R, B> {
    Unavailable(R),
    Compile(R),
    Execute(R),
    Binding(B),
    Cancelled,
}

impl<R, B> fmt::Display for TrustedLuaRuntimeExecutionError<R, B>
where
    R: fmt::Display,
    B: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "Lua 执行器不可用：{source}"),
            Self::Compile(source) => write!(formatter, "Lua 主程序编译失败：{source}"),
            Self::Execute(source) => write!(formatter, "Lua 主程序运行失败：{source}"),
            Self::Binding(source) => write!(formatter, "Lua Host 能力调用失败：{source}"),
            Self::Cancelled => formatter.write_str("Lua 主程序已取消"),
        }
    }
}

impl<R, B> Error for TrustedLuaRuntimeExecutionError<R, B>
where
    R: Error + 'static,
    B: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(source) | Self::Compile(source) | Self::Execute(source) => {
                Some(source)
            }
            Self::Binding(source) => Some(source),
            Self::Cancelled => None,
        }
    }
}

/// VM 执行与 Host 资源收尾的两个独立终态。
pub(crate) struct TrustedLuaRuntimeExecutionReport<R, B> {
    runtime: Result<(), TrustedLuaRuntimeExecutionError<R, B>>,
    finalization: Result<TrustedLuaBindingFinalization, B>,
}

impl<R, B> TrustedLuaRuntimeExecutionReport<R, B> {
    pub(crate) fn new(
        runtime: Result<(), TrustedLuaRuntimeExecutionError<R, B>>,
        finalization: Result<TrustedLuaBindingFinalization, B>,
    ) -> Self {
        Self {
            runtime,
            finalization,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Result<(), TrustedLuaRuntimeExecutionError<R, B>>,
        Result<TrustedLuaBindingFinalization, B>,
    ) {
        (self.runtime, self.finalization)
    }
}

/// 在外部配置的专用有界 worker 与队列中运行完全可信的 Lua 程序。
///
/// 根实现提供完整标准库，并允许脚本主动使用 `require` 和本机文件能力。编译、执行、
/// 标准库 I/O 和 VM 释放均不得阻塞异步执行器线程。队列满时异步背压。Runtime 的
/// Future 一旦首次轮询并取得 `bindings` 所有权，取消可以阻止编译或脚本执行，但不能
/// 放弃资源收尾：根实现必须在排队期取消、运行期取消、成功和失败四种路径中恰好调用
/// 一次 `bindings.finalize`。如果调用方继续 await，则通过报告返回 Runtime 与清理终态；
/// 如果调用 Future 被丢弃，根实现仍须在自己的受控任务中完成清理。
pub(crate) trait TrustedLuaRuntimeExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute<B>(
        &self,
        program: OwnedLuaProgram,
        bindings: Arc<B>,
    ) -> impl Future<Output = TrustedLuaRuntimeExecutionReport<Self::Error, B::Error>> + Send
    where
        B: TrustedLuaHostBindings;
}
