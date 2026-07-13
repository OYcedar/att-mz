#![allow(dead_code, reason = "CPU 根接口按计划先于生产适配器定义")]

//! 受控执行 CPU 密集型纯计算的根能力契约。

use std::error::Error;
use std::fmt;
use std::future::Future;

/// CPU 任务未能产生结果的原因。
#[derive(Debug)]
pub(crate) enum CpuTaskExecutionError<E> {
    /// 执行器已经关闭或当前无法再接收任务。
    Unavailable(E),
    /// 工作线程捕获到任务 panic。
    TaskPanicked,
}

impl<E> fmt::Display for CpuTaskExecutionError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "CPU 执行器不可用：{source}"),
            Self::TaskPanicked => formatter.write_str("CPU 任务执行时发生 panic"),
        }
    }
}

impl<E> Error for CpuTaskExecutionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(source) => Some(source),
            Self::TaskPanicked => None,
        }
    }
}

/// 在外部配置的有界工作线程与队列中执行纯 CPU 计算。
///
/// 实现必须保证闭包不在异步 I/O 执行器线程上运行。队列满时应异步背压而不是
/// 无界堆积；已经开始的任务即使调用 Future 被丢弃，也必须安全运行至结束。
/// 闭包不得执行文件、数据库、Lua、网络等副作用。
pub(crate) trait CpuTaskExecutor: Send + Sync {
    /// CPU 执行器自身的不可用原因。
    type Error: Error + Send + Sync + 'static;

    /// 调度并等待一个拥有所有权的纯计算任务。
    fn execute<T, F>(
        &self,
        task: F,
    ) -> impl Future<Output = Result<T, CpuTaskExecutionError<Self::Error>>> + Send
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static;
}
