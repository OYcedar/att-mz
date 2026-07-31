//! 使用命令私有的 Rayon 工作池执行 CPU 密集型任务。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

/// CPU 工作池的产品配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CpuExecutorConfig {
    #[cfg(test)]
    fixed_worker_threads: Option<NonZeroUsize>,
}

impl CpuExecutorConfig {
    /// 生产运行始终使用操作系统报告的当前可用并行度。
    pub(crate) const fn production() -> Self {
        Self {
            #[cfg(test)]
            fixed_worker_threads: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(worker_threads: NonZeroUsize) -> Self {
        Self {
            fixed_worker_threads: Some(worker_threads),
        }
    }
}

/// CPU 工作池启动失败。
#[derive(Debug)]
pub(crate) enum CpuExecutorStartError {
    AvailableParallelism(std::io::Error),
    TooManyWorkerThreads { requested: usize, maximum: usize },
    Build(rayon::ThreadPoolBuildError),
}

impl fmt::Display for CpuExecutorStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AvailableParallelism(source) => {
                write!(formatter, "无法取得本机可用 CPU 并行度：{source}")
            }
            Self::TooManyWorkerThreads { requested, maximum } => write!(
                formatter,
                "CPU 工作线程数 {requested} 超过 Rayon 支持上限 {maximum}"
            ),
            Self::Build(source) => write!(formatter, "无法启动 Rayon CPU 工作池：{source}"),
        }
    }
}

impl Error for CpuExecutorStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AvailableParallelism(source) => Some(source),
            Self::Build(source) => Some(source),
            Self::TooManyWorkerThreads { .. } => None,
        }
    }
}

/// CPU 工作池当前无法接收任务的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CpuExecutorUnavailable {
    ShuttingDown,
    StatePoisoned,
}

impl fmt::Display for CpuExecutorUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("CPU 执行器正在关闭"),
            Self::StatePoisoned => formatter.write_str("CPU 执行器状态锁已经损坏"),
        }
    }
}

impl Error for CpuExecutorUnavailable {}

impl SafeDiagnosticSource for CpuTaskExecutionError<CpuExecutorUnavailable> {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        let (failure, action) = match self {
            Self::Cancelled => (
                DiagnosticFailureKind::LockCancelled,
                DiagnosticAction::Retry,
            ),
            Self::Unavailable(CpuExecutorUnavailable::ShuttingDown) => (
                DiagnosticFailureKind::ExecutorClosed,
                DiagnosticAction::Retry,
            ),
            Self::Unavailable(CpuExecutorUnavailable::StatePoisoned) | Self::TaskPanicked => (
                DiagnosticFailureKind::WorkerPanicked,
                DiagnosticAction::ReportBug,
            ),
        };
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            stage,
            DiagnosticSubject::component("CPU worker"),
            DiagnosticReason::failure(failure),
            impact,
            action,
        )
    }
}

/// CPU 工作池关闭失败。
#[derive(Debug)]
pub(crate) enum CpuExecutorShutdownError {
    ConcurrentShutdown,
    StatePoisoned,
}

impl fmt::Display for CpuExecutorShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConcurrentShutdown => formatter.write_str("CPU 执行器正在由另一个调用方关闭"),
            Self::StatePoisoned => formatter.write_str("CPU 执行器状态锁已经损坏"),
        }
    }
}

impl Error for CpuExecutorShutdownError {}

impl SafeDiagnosticSource for CpuExecutorShutdownError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        let (reason, action) = match self {
            Self::ConcurrentShutdown => (
                DiagnosticReason::failure(DiagnosticFailureKind::ConcurrentShutdown),
                DiagnosticAction::Retry,
            ),
            Self::StatePoisoned => (
                DiagnosticReason::failure(DiagnosticFailureKind::ExecutorStatePoisoned),
                DiagnosticAction::ReportBug,
            ),
        };
        SafeDiagnostic::new(
            DiagnosticCode::ShutdownComponent,
            stage,
            DiagnosticSubject::component("CPU executor"),
            reason,
            impact,
            action,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleState {
    Running,
    ShuttingDown,
    Stopped,
}

struct Lifecycle {
    state: LifecycleState,
    pool: Option<ThreadPool>,
}

#[derive(Default)]
struct ActiveTasks {
    count: Mutex<usize>,
    drained: Condvar,
}

impl ActiveTasks {
    fn begin(self: &Arc<Self>) -> Result<ActiveTaskGuard, CpuExecutorUnavailable> {
        let mut count = self
            .count
            .lock()
            .map_err(|_| CpuExecutorUnavailable::StatePoisoned)?;
        *count += 1;
        Ok(ActiveTaskGuard {
            tasks: Arc::clone(self),
        })
    }

    fn wait_until_drained(&self) -> Result<(), CpuExecutorShutdownError> {
        let mut count = self
            .count
            .lock()
            .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
        while *count != 0 {
            count = self
                .drained
                .wait(count)
                .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
        }
        Ok(())
    }
}

struct ActiveTaskGuard {
    tasks: Arc<ActiveTasks>,
}

impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        let mut count = self
            .tasks
            .count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(*count > 0, "已接管 CPU 任务计数不得下溢");
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.tasks.drained.notify_all();
        }
    }
}

struct WorkerExits {
    remaining: Mutex<usize>,
    all_exited: Condvar,
    #[cfg(test)]
    test_gate: Mutex<Option<TestWorkerExitGate>>,
}

impl WorkerExits {
    fn new(worker_threads: usize) -> Self {
        Self {
            remaining: Mutex::new(worker_threads),
            all_exited: Condvar::new(),
            #[cfg(test)]
            test_gate: Mutex::new(None),
        }
    }

    fn record_exit(&self) {
        #[cfg(test)]
        {
            let gate = self
                .test_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(gate) = gate {
                gate.entered.wait();
                gate.release.wait();
            }
        }

        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(*remaining > 0, "Rayon worker 退出计数不得下溢");
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            self.all_exited.notify_all();
        }
    }

    fn wait_until_all_exited(&self) -> Result<(), CpuExecutorShutdownError> {
        let mut remaining = self
            .remaining
            .lock()
            .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
        while *remaining != 0 {
            remaining = self
                .all_exited
                .wait(remaining)
                .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn set_test_gate(&self, gate: TestWorkerExitGate) {
        *self
            .test_gate
            .lock()
            .expect("测试 exit handler 门锁不应中毒") = Some(gate);
    }

    #[cfg(test)]
    fn exited_count(&self, worker_threads: usize) -> usize {
        worker_threads
            - *self
                .remaining
                .lock()
                .expect("测试 worker 退出计数锁不应中毒")
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestWorkerExitGate {
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}

struct CpuExecutorInner {
    lifecycle: Mutex<Lifecycle>,
    parallelism: NonZeroUsize,
    admission: Arc<Semaphore>,
    waits_cancelled: AtomicBool,
    active_tasks: Arc<ActiveTasks>,
    worker_exits: Arc<WorkerExits>,
}

/// 命令生命周期内唯一、按本机并行度运行并自然背压的 CPU 执行器。
#[derive(Clone)]
pub(crate) struct RayonCpuExecutor {
    inner: Arc<CpuExecutorInner>,
}

impl RayonCpuExecutor {
    pub(crate) fn start(config: CpuExecutorConfig) -> Result<Self, CpuExecutorStartError> {
        Self::start_with_available_parallelism(config, std::thread::available_parallelism)
    }

    fn start_with_available_parallelism<F>(
        config: CpuExecutorConfig,
        available_parallelism: F,
    ) -> Result<Self, CpuExecutorStartError>
    where
        F: FnOnce() -> Result<NonZeroUsize, std::io::Error>,
    {
        #[cfg(test)]
        let worker_threads = match config.fixed_worker_threads {
            Some(worker_threads) => worker_threads.get(),
            None => available_parallelism()
                .map_err(CpuExecutorStartError::AvailableParallelism)?
                .get(),
        };
        #[cfg(not(test))]
        let worker_threads = {
            let _ = config;
            available_parallelism()
                .map_err(CpuExecutorStartError::AvailableParallelism)?
                .get()
        };
        validate_supported_worker_threads(worker_threads).map_err(|(requested, maximum)| {
            CpuExecutorStartError::TooManyWorkerThreads { requested, maximum }
        })?;
        let worker_exits = Arc::new(WorkerExits::new(worker_threads));
        let exit_handler_state = Arc::clone(&worker_exits);
        let pool = ThreadPoolBuilder::new()
            .num_threads(worker_threads)
            .thread_name(|index| format!("att-cpu-{index}"))
            .exit_handler(move |_| exit_handler_state.record_exit())
            .build()
            .map_err(CpuExecutorStartError::Build)?;

        Ok(Self {
            inner: Arc::new(CpuExecutorInner {
                lifecycle: Mutex::new(Lifecycle {
                    state: LifecycleState::Running,
                    pool: Some(pool),
                }),
                parallelism: NonZeroUsize::new(worker_threads).expect("CPU worker 数已经确认非零"),
                // 许可与 Rayon worker 一一对应：饱和调用在提交前自然背压，
                // 不再另造可配置等待队列。
                admission: Arc::new(Semaphore::new(worker_threads)),
                waits_cancelled: AtomicBool::new(false),
                active_tasks: Arc::new(ActiveTasks::default()),
                worker_exits,
            }),
        })
    }

    /// 返回这个执行根实际建立的 worker 数。
    ///
    /// 调用方可以据此限制同时存在的 CPU 工作 Future；它不限制一次命令的总工作量。
    pub(crate) fn parallelism(&self) -> NonZeroUsize {
        self.inner.parallelism
    }

    /// 停止准入，排空全部已接管任务，再释放私有 Rayon 池。
    pub(crate) fn shutdown(&self) -> Result<(), CpuExecutorShutdownError> {
        {
            let mut lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
            match lifecycle.state {
                LifecycleState::Running => {
                    lifecycle.state = LifecycleState::ShuttingDown;
                    self.inner.admission.close();
                }
                LifecycleState::ShuttingDown => {
                    return Err(CpuExecutorShutdownError::ConcurrentShutdown);
                }
                LifecycleState::Stopped => return Ok(()),
            }
        }

        self.inner.active_tasks.wait_until_drained()?;

        let pool = {
            let mut lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?;
            lifecycle
                .pool
                .take()
                .expect("正在关闭的 CPU 执行器必须拥有 Rayon 池")
        };
        drop(pool);
        self.inner.worker_exits.wait_until_all_exited()?;
        self.inner
            .lifecycle
            .lock()
            .map_err(|_| CpuExecutorShutdownError::StatePoisoned)?
            .state = LifecycleState::Stopped;
        Ok(())
    }

    /// 取消尚未取得 CPU 执行许可的工作；已经交给 Rayon 的闭包仍安全运行到结束。
    ///
    /// ATT 每个进程只执行一条命令，因此收到终止信号后不需要重新开放准入。
    pub(crate) fn cancel_waits(&self) {
        self.inner.waits_cancelled.store(true, Ordering::Release);
        self.inner.admission.close();
    }
}

fn validate_supported_worker_threads(worker_threads: usize) -> Result<(), (usize, usize)> {
    let maximum = rayon::max_num_threads();
    if worker_threads > maximum {
        Err((worker_threads, maximum))
    } else {
        Ok(())
    }
}

fn run_task<T, F>(
    task: F,
    result: oneshot::Sender<Result<T, CpuTaskExecutionError<CpuExecutorUnavailable>>>,
    permit: OwnedSemaphorePermit,
    active_task: ActiveTaskGuard,
) where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // 这些守卫必须活到闭包及结果交接全部结束；即使结果值析构时 panic，
    // 准入容量与 shutdown 的活动计数也会由 RAII 归还。
    let _permit = permit;
    let _active_task = active_task;
    let outcome =
        catch_unwind(AssertUnwindSafe(task)).map_err(|_| CpuTaskExecutionError::TaskPanicked);
    let _ = result.send(outcome);
}

impl CpuTaskExecutor for RayonCpuExecutor {
    type Error = CpuExecutorUnavailable;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = Arc::clone(&self.inner.admission)
            .acquire_owned()
            .await
            .map_err(|_| {
                if self.inner.waits_cancelled.load(Ordering::Acquire) {
                    CpuTaskExecutionError::Cancelled
                } else {
                    CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown)
                }
            })?;
        let (result_sender, result_receiver) = oneshot::channel();

        {
            let lifecycle = self.inner.lifecycle.lock().map_err(|_| {
                CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned)
            })?;
            if lifecycle.state != LifecycleState::Running {
                return Err(CpuTaskExecutionError::Unavailable(
                    CpuExecutorUnavailable::ShuttingDown,
                ));
            }
            let active_task = self
                .inner
                .active_tasks
                .begin()
                .map_err(CpuTaskExecutionError::Unavailable)?;
            let pool = lifecycle
                .pool
                .as_ref()
                .expect("运行中的 CPU 执行器必须拥有 Rayon 池");
            pool.spawn_fifo(move || {
                // 防止任务封装或结果析构中的异常越过长期存活的 Rayon worker。
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    run_task(task, result_sender, permit, active_task);
                }));
            });
        }

        result_receiver
            .await
            .unwrap_or(Err(CpuTaskExecutionError::TaskPanicked))
    }

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
        self.execute(move || inputs.into_par_iter().map(operation).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar};
    use std::time::Duration;

    use super::*;

    fn assert_send<T: Send>(_: T) {}

    fn fixed_config(worker_threads: usize) -> Result<CpuExecutorConfig, std::convert::Infallible> {
        Ok(CpuExecutorConfig::fixed(
            NonZeroUsize::new(worker_threads).expect("测试 worker 数必须非零"),
        ))
    }

    #[test]
    fn auto_parallelism_probe_failure_is_explicit() {
        let config = CpuExecutorConfig::production();
        let result = RayonCpuExecutor::start_with_available_parallelism(config, || {
            Err(std::io::Error::other("probe failed"))
        });
        assert!(matches!(
            result,
            Err(CpuExecutorStartError::AvailableParallelism(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn auto_parallelism_is_explicitly_applied_to_private_pool() {
        let executor = RayonCpuExecutor::start_with_available_parallelism(
            CpuExecutorConfig::production(),
            || Ok(NonZeroUsize::new(2).unwrap()),
        )
        .unwrap();
        assert_eq!(executor.parallelism(), NonZeroUsize::new(2).unwrap());
        assert_eq!(
            executor.execute(rayon::current_num_threads).await.unwrap(),
            2
        );
        executor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_isolated_pool_continues_and_future_is_send() {
        let executor = RayonCpuExecutor::start(fixed_config(1).unwrap()).unwrap();
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
        let executor = RayonCpuExecutor::start(fixed_config(1).unwrap()).unwrap();
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
    async fn shutdown_drains_accepted_job_and_rejects_new_admission() {
        let executor = RayonCpuExecutor::start(fixed_config(1).unwrap()).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let completed = Arc::new(AtomicUsize::new(0));
        let mut accepted = Box::pin(executor.execute({
            let gate = Arc::clone(&gate);
            let completed = Arc::clone(&completed);
            move || {
                gate.wait();
                completed.store(1, Ordering::Release);
            }
        }));
        assert!(matches!(
            futures_util::poll!(accepted.as_mut()),
            std::task::Poll::Pending
        ));
        drop(accepted);

        let shutdown_executor = executor.clone();
        let shutdown = std::thread::spawn(move || shutdown_executor.shutdown());
        std::thread::sleep(Duration::from_millis(20));
        assert!(!shutdown.is_finished(), "shutdown 必须等待已接管任务");
        gate.wait();
        shutdown.join().unwrap().unwrap();
        assert_eq!(completed.load(Ordering::Acquire), 1);
        assert!(matches!(
            executor.execute(|| 12).await,
            Err(CpuTaskExecutionError::Unavailable(
                CpuExecutorUnavailable::ShuttingDown
            ))
        ));
    }

    #[test]
    fn shutdown_waits_until_every_rayon_exit_handler_finishes() {
        let worker_threads = 2;
        let executor = RayonCpuExecutor::start(fixed_config(worker_threads).unwrap()).unwrap();
        let entered = Arc::new(Barrier::new(worker_threads + 1));
        let release = Arc::new(Barrier::new(worker_threads + 1));
        executor
            .inner
            .worker_exits
            .set_test_gate(TestWorkerExitGate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            });

        let shutdown_executor = executor.clone();
        let shutdown = std::thread::spawn(move || shutdown_executor.shutdown());
        entered.wait();

        assert!(
            !shutdown.is_finished(),
            "exit handler 未完成时 shutdown 不得返回"
        );
        assert_eq!(
            executor.inner.worker_exits.exited_count(worker_threads),
            0,
            "测试门后的 exit handler 才能记为完成"
        );

        release.wait();
        shutdown.join().unwrap().unwrap();
        assert_eq!(
            executor.inner.worker_exits.exited_count(worker_threads),
            worker_threads
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_count_is_the_hard_parallelism_limit() {
        let executor = RayonCpuExecutor::start(fixed_config(2).unwrap()).unwrap();
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
    async fn cancelling_before_admission_never_executes_the_task() {
        let executor = RayonCpuExecutor::start(fixed_config(1).unwrap()).unwrap();
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
            .expect("第一个任务应已被 Rayon 接管");

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

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_admission_is_released_by_cancellation_without_running_waiting_work() {
        let executor = RayonCpuExecutor::start(fixed_config(1).unwrap()).unwrap();
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
            .expect("第一个任务应已占用唯一 CPU 许可");

        let waiting_ran = Arc::new(AtomicUsize::new(0));
        let mut waiting = Box::pin(executor.execute({
            let waiting_ran = Arc::clone(&waiting_ran);
            move || waiting_ran.fetch_add(1, Ordering::AcqRel)
        }));
        assert!(matches!(
            futures_util::poll!(waiting.as_mut()),
            std::task::Poll::Pending
        ));

        executor.cancel_waits();
        assert!(matches!(
            waiting.await,
            Err(CpuTaskExecutionError::Cancelled)
        ));
        assert_eq!(waiting_ran.load(Ordering::Acquire), 0);

        first_gate.wait();
        first.await.expect("已开始的任务应安全完成");
        executor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordered_map_parallelizes_but_preserves_input_order() {
        let executor = RayonCpuExecutor::start(fixed_config(2).unwrap()).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let output = executor
            .execute_ordered_map((0usize..16).collect(), {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move |value| {
                    let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum.fetch_max(now, Ordering::AcqRel);
                    std::thread::sleep(Duration::from_millis(if value % 2 == 0 { 3 } else { 1 }));
                    active.fetch_sub(1, Ordering::AcqRel);
                    value * 2
                }
            })
            .await
            .unwrap();

        assert_eq!(
            output,
            (0usize..16).map(|value| value * 2).collect::<Vec<_>>()
        );
        assert_eq!(maximum.load(Ordering::Acquire), 2);
        executor.shutdown().unwrap();
    }
}
