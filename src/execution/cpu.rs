//! 受控执行 CPU 密集型纯计算的根能力契约。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

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
/// 闭包不得执行文件、数据库、Lua、网络等副作用，也不得启动脱离闭包生命周期的
/// 后台计算任务。
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

    /// 并行计算一组相互独立的输入，并按输入顺序返回结果。
    ///
    /// 生产适配器可以在自己的受控执行资源中覆盖此方法；串行测试替身无需理解
    /// 并行调度，默认实现仍通过同一个 CPU 根执行整批计算。
    fn execute_ordered_map<I, T, F>(
        &self,
        inputs: Vec<I>,
        operation: F,
    ) -> impl Future<Output = Result<Vec<T>, CpuTaskExecutionError<Self::Error>>> + Send
    where
        I: Send + 'static,
        T: Send + 'static,
        F: Fn(I) -> T + Send + Sync + 'static,
    {
        self.execute(move || inputs.into_iter().map(operation).collect())
    }

    /// 并行计算一组输入，并在每个工作单元真实返回后观察绝对完成数。
    ///
    /// 观察回调不返回错误，而且会按 `1..=N` 的顺序被调用；它不得执行
    /// 阻塞 I/O 或依赖输入顺序。结果仍按输入顺序返回，完成观察只反映
    /// 实际调度顺序。
    fn execute_ordered_map_observed<I, T, F, O>(
        &self,
        inputs: Vec<I>,
        operation: F,
        on_completed: O,
    ) -> impl Future<Output = Result<Vec<T>, CpuTaskExecutionError<Self::Error>>> + Send
    where
        I: Send + 'static,
        T: Send + 'static,
        F: Fn(I) -> T + Send + Sync + 'static,
        O: Fn(u64) + Send + Sync + 'static,
    {
        let completed = Arc::new(Mutex::new(0_u64));
        self.execute_ordered_map(inputs, move |input| {
            let output = operation(input);
            let mut completed = completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *completed = completed.saturating_add(1);
            on_completed(*completed);
            output
        })
    }
}
