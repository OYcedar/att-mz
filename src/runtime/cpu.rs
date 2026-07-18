//! 使用独立有界工作线程执行 CPU 密集型任务。

use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

const STATE_RUNNING: u8 = 0;
const STATE_SHUTTING_DOWN: u8 = 1;
const STATE_STOPPED: u8 = 2;

/// CPU 工作池的受信配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CpuExecutorConfig {
    worker_threads: usize,
    queue_capacity: usize,
}

impl CpuExecutorConfig {
    pub(crate) fn new(
        worker_threads: usize,
        queue_capacity: usize,
    ) -> Result<Self, CpuExecutorConfigError> {
        if worker_threads == 0 {
            return Err(CpuExecutorConfigError::ZeroWorkerThreads);
        }
        if queue_capacity == 0 {
            return Err(CpuExecutorConfigError::ZeroQueueCapacity);
        }
        Ok(Self {
            worker_threads,
            queue_capacity,
        })
    }

    pub(crate) const fn worker_threads(self) -> usize {
        self.worker_threads
    }

    pub(crate) const fn queue_capacity(self) -> usize {
        self.queue_capacity
    }
}

/// CPU 工作池配置错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CpuExecutorConfigError {
    ZeroWorkerThreads,
    ZeroQueueCapacity,
}

impl fmt::Display for CpuExecutorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWorkerThreads => formatter.write_str("CPU 工作线程数必须大于零"),
            Self::ZeroQueueCapacity => formatter.write_str("CPU 工作队列容量必须大于零"),
        }
    }
}

impl Error for CpuExecutorConfigError {}

/// CPU 工作池当前无法接收任务的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CpuExecutorUnavailable {
    ShuttingDown,
    QueueClosed,
}

impl fmt::Display for CpuExecutorUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("CPU 执行器正在关闭"),
            Self::QueueClosed => formatter.write_str("CPU 工作队列已经关闭"),
        }
    }
}

impl Error for CpuExecutorUnavailable {}

/// CPU 工作池关闭失败。
#[derive(Debug)]
pub(crate) enum CpuExecutorShutdownError {
    ConcurrentShutdown,
    WorkerPanicked,
    StatePoisoned,
}

impl fmt::Display for CpuExecutorShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConcurrentShutdown => formatter.write_str("CPU 执行器正在由另一个调用方关闭"),
            Self::WorkerPanicked => formatter.write_str("CPU 工作线程在关闭时异常退出"),
            Self::StatePoisoned => formatter.write_str("CPU 工作线程状态锁已经损坏"),
        }
    }
}

impl Error for CpuExecutorShutdownError {}

trait CpuJob: Send {
    fn run(self: Box<Self>);
}

struct TypedCpuJob<T, F> {
    task: F,
    result: oneshot::Sender<Result<T, CpuTaskExecutionError<CpuExecutorUnavailable>>>,
}

impl<T, F> CpuJob for TypedCpuJob<T, F>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    fn run(self: Box<Self>) {
        let Self { task, result } = *self;
        let outcome =
            catch_unwind(AssertUnwindSafe(task)).map_err(|_| CpuTaskExecutionError::TaskPanicked);
        // 调用 Future 可以在入队后被丢弃；结果无人接收不改变任务已经完成的事实。
        let _ = result.send(outcome);
    }
}

struct CpuExecutorInner {
    sender: async_channel::Sender<Box<dyn CpuJob>>,
    state: AtomicU8,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for CpuExecutorInner {
    fn drop(&mut self) {
        self.sender.close();
        let current_thread = thread::current().id();
        let workers = self
            .workers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for worker in std::mem::take(workers) {
            // 作业可以持有执行器的最后一个引用。此时 Drop 会在工作线程自身发生，
            // 当前线程不能 join 自己；关闭 sender 后让它在作业返回时自然退出。
            if worker.thread().id() != current_thread {
                let _ = worker.join();
            }
        }
        self.state.store(STATE_STOPPED, Ordering::Release);
    }
}

/// 固定线程数、固定队列容量的生产 CPU 执行器。
#[derive(Clone)]
pub(crate) struct BoundedCpuExecutor {
    inner: Arc<CpuExecutorInner>,
}

impl BoundedCpuExecutor {
    pub(crate) fn start(config: CpuExecutorConfig) -> Result<Self, std::io::Error> {
        let (sender, receiver) = async_channel::bounded(config.queue_capacity());
        let inner = Arc::new(CpuExecutorInner {
            sender,
            state: AtomicU8::new(STATE_RUNNING),
            workers: Mutex::new(Vec::with_capacity(config.worker_threads())),
        });

        let mut started = Vec::with_capacity(config.worker_threads());
        for index in 0..config.worker_threads() {
            let receiver = receiver.clone();
            let worker = thread::Builder::new()
                .name(format!("att-cpu-{index}"))
                .spawn(move || worker_loop(receiver));
            match worker {
                Ok(worker) => started.push(worker),
                Err(error) => {
                    inner.sender.close();
                    for worker in started {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }

        *inner
            .workers
            .lock()
            .expect("刚构造的 CPU 工作线程锁不可能中毒") = started;
        Ok(Self { inner })
    }

    /// 停止准入，排空全部已接管任务并等待工作线程退出。
    pub(crate) fn shutdown(&self) -> Result<(), CpuExecutorShutdownError> {
        if self
            .inner
            .state
            .compare_exchange(
                STATE_RUNNING,
                STATE_SHUTTING_DOWN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return if self.inner.state.load(Ordering::Acquire) == STATE_STOPPED {
                Ok(())
            } else {
                Err(CpuExecutorShutdownError::ConcurrentShutdown)
            };
        }

        self.inner.sender.close();
        let workers = {
            let mut workers = self
                .inner
                .workers
                .lock()
                .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
            std::mem::take(&mut *workers)
        };
        let mut worker_panicked = false;
        for worker in workers {
            worker_panicked |= worker.join().is_err();
        }
        self.inner.state.store(STATE_STOPPED, Ordering::Release);
        if worker_panicked {
            Err(CpuExecutorShutdownError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

fn worker_loop(receiver: async_channel::Receiver<Box<dyn CpuJob>>) {
    while let Ok(job) = receiver.recv_blocking() {
        // 每个作业仍有外层隔离，防止作业封装自身的缺陷杀死长期工作线程。
        let _ = catch_unwind(AssertUnwindSafe(|| job.run()));
    }
}

impl CpuTaskExecutor for BoundedCpuExecutor {
    type Error = CpuExecutorUnavailable;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        if self.inner.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(CpuTaskExecutionError::Unavailable(
                CpuExecutorUnavailable::ShuttingDown,
            ));
        }

        let (result_sender, result_receiver) = oneshot::channel();
        let job: Box<dyn CpuJob> = Box::new(TypedCpuJob {
            task,
            result: result_sender,
        });
        self.inner
            .sender
            .send(job)
            .await
            .map_err(|_| CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::QueueClosed))?;
        result_receiver
            .await
            .unwrap_or(Err(CpuTaskExecutionError::TaskPanicked))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar};
    use std::time::Duration;

    use super::*;

    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn config_rejects_implicit_resource_choices() {
        assert_eq!(
            CpuExecutorConfig::new(0, 1),
            Err(CpuExecutorConfigError::ZeroWorkerThreads)
        );
        assert_eq!(
            CpuExecutorConfig::new(1, 0),
            Err(CpuExecutorConfigError::ZeroQueueCapacity)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_isolated_worker_continues_and_future_is_send() {
        let executor = BoundedCpuExecutor::start(CpuExecutorConfig::new(1, 2).unwrap()).unwrap();
        assert_send(executor.execute(|| 1usize));
        assert!(matches!(
            executor.execute(|| panic!("boom")).await,
            Err(CpuTaskExecutionError::TaskPanicked)
        ));
        assert_eq!(executor.execute(|| 7).await.unwrap(), 7);
        executor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepted_task_finishes_after_its_waiter_is_dropped() {
        let executor = BoundedCpuExecutor::start(CpuExecutorConfig::new(1, 1).unwrap()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let completed = Arc::new(AtomicUsize::new(0));
        let mut future = Box::pin(executor.execute({
            let barrier = Arc::clone(&barrier);
            let completed = Arc::clone(&completed);
            move || {
                barrier.wait();
                completed.store(1, Ordering::Release);
            }
        }));
        assert!(matches!(
            futures_util::poll!(future.as_mut()),
            std::task::Poll::Pending
        ));
        drop(future);
        barrier.wait();
        for _ in 0..100 {
            if completed.load(Ordering::Acquire) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(completed.load(Ordering::Acquire), 1);
        executor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_jobs_and_rejects_new_admission() {
        let executor = BoundedCpuExecutor::start(CpuExecutorConfig::new(1, 2).unwrap()).unwrap();
        let accepted = executor.execute(|| 11usize);
        assert_eq!(accepted.await.unwrap(), 11);
        executor.shutdown().unwrap();
        assert!(matches!(
            executor.execute(|| 12).await,
            Err(CpuTaskExecutionError::Unavailable(
                CpuExecutorUnavailable::ShuttingDown
            ))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_count_is_the_hard_parallelism_limit() {
        let executor = BoundedCpuExecutor::start(CpuExecutorConfig::new(2, 4).unwrap()).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let (started, started_receiver) = async_channel::bounded(4);
        let mut tasks = Vec::new();

        for _ in 0..4 {
            let executor = executor.clone();
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let gate = Arc::clone(&gate);
            let started = started.clone();
            tasks.push(tokio::spawn(async move {
                executor
                    .execute(move || {
                        let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                        maximum.fetch_max(now, Ordering::AcqRel);
                        started.send_blocking(()).expect("启动信号应可写入");
                        let (lock, wake) = &*gate;
                        let mut released = lock.lock().expect("测试门锁不应中毒");
                        while !*released {
                            released = wake.wait(released).expect("测试门锁不应中毒");
                        }
                        active.fetch_sub(1, Ordering::AcqRel);
                    })
                    .await
            }));
        }

        started_receiver.recv().await.expect("第一个任务应启动");
        started_receiver.recv().await.expect("第二个任务应启动");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), started_receiver.recv())
                .await
                .is_err(),
            "两个 worker 被占用时不得启动第三个任务"
        );
        assert_eq!(maximum.load(Ordering::Acquire), 2);

        let (lock, wake) = &*gate;
        *lock.lock().expect("测试门锁不应中毒") = true;
        wake.notify_all();
        for task in tasks {
            task.await
                .expect("Tokio 任务应完成")
                .expect("CPU 任务应完成");
        }
        executor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_before_enqueue_never_executes_the_task() {
        let executor = BoundedCpuExecutor::start(CpuExecutorConfig::new(1, 1).unwrap()).unwrap();
        let first_gate = Arc::new(Barrier::new(2));
        let (first_started, first_started_receiver) = oneshot::channel();
        let mut first = Box::pin(executor.execute({
            let first_gate = Arc::clone(&first_gate);
            move || {
                let _ = first_started.send(());
                first_gate.wait();
            }
        }));
        assert!(matches!(
            futures_util::poll!(first.as_mut()),
            std::task::Poll::Pending
        ));
        first_started_receiver
            .await
            .expect("第一个任务应已被 worker 接管");

        let second_ran = Arc::new(AtomicUsize::new(0));
        let mut second = Box::pin(executor.execute({
            let second_ran = Arc::clone(&second_ran);
            move || {
                second_ran.store(1, Ordering::Release);
            }
        }));
        assert!(matches!(
            futures_util::poll!(second.as_mut()),
            std::task::Poll::Pending
        ));

        let cancelled_ran = Arc::new(AtomicUsize::new(0));
        let mut cancelled = Box::pin(executor.execute({
            let cancelled_ran = Arc::clone(&cancelled_ran);
            move || {
                cancelled_ran.store(1, Ordering::Release);
            }
        }));
        assert!(matches!(
            futures_util::poll!(cancelled.as_mut()),
            std::task::Poll::Pending
        ));
        drop(cancelled);

        first_gate.wait();
        first.await.expect("第一个任务应完成");
        second.await.expect("第二个任务应完成");
        assert_eq!(second_ran.load(Ordering::Acquire), 1);
        assert_eq!(cancelled_ran.load(Ordering::Acquire), 0);
        executor.shutdown().unwrap();
    }
}
