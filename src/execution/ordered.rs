//! 跨业务切片复用的有序并发执行流水线。
//!
//! 调用方提交已经按自然序排列的工作，并分别拥有执行、无副作用准备和最终化
//! 语义。本模块只负责有限准入、并发执行、并发准备、按序最终化、合作取消与
//! 失败后的完整 drain；它不解释模型协议、领域结果或持久化事务。

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::future;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{CooperativeCancellation, OperationCompletion};

/// 有序流水线的活动执行宽度与本地在途窗口。
///
/// `in_flight_window_multiplier` 只限制同时处于执行、准备或等待最终化的工作，
/// 不限制调用方提交的总工作量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OrderedExecutionLimits {
    max_concurrent_executions: NonZeroUsize,
    in_flight_window_multiplier: NonZeroUsize,
}

impl OrderedExecutionLimits {
    pub(crate) const fn new(
        max_concurrent_executions: NonZeroUsize,
        in_flight_window_multiplier: NonZeroUsize,
    ) -> Self {
        Self {
            max_concurrent_executions,
            in_flight_window_multiplier,
        }
    }

    fn worker_count(self, task_count: usize) -> usize {
        task_count.min(self.max_concurrent_executions.get())
    }

    fn in_flight_window(self, task_count: usize) -> usize {
        task_count.min(
            self.max_concurrent_executions
                .get()
                .saturating_mul(self.in_flight_window_multiplier.get()),
        )
    }
}

/// 一个已经进入流水线的工作在最终化时允许采取的副作用策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OrderedFinalizationDisposition {
    /// 当前工作之前没有失败且未请求取消，可以执行领域副作用。
    Apply,
    /// 已请求合作取消；当前工作必须收敛为不产生新副作用的终态。
    CancelledNoApply,
    /// 更早自然序工作已经失败；当前工作只能记录终态，不能越过失败提交。
    AfterEarlierFailureNoApply,
}

/// 工作在进入自然序最终化前形成的结果。
#[derive(Debug)]
pub(crate) enum OrderedTaskResult<P, E> {
    Prepared(P),
    ExecutionFailed(E),
    PreparationFailed(E),
}

/// 有序流水线自身能够报告的失败。
#[derive(Debug)]
pub(crate) enum OrderedExecutionError<E> {
    /// 严格自然序最终化返回的最早失败。
    Finalization { ordinal: usize, source: E },
    /// 所有并发通道已经关闭，但一个已期待的自然序位置没有结果。
    IncompleteResultSequence {
        expected_ordinal: usize,
        actual_ordinal: Option<usize>,
    },
}

impl<E> fmt::Display for OrderedExecutionError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finalization { ordinal, source } => {
                write!(formatter, "有序工作 {ordinal} 最终化失败：{source}")
            }
            Self::IncompleteResultSequence {
                expected_ordinal,
                actual_ordinal: Some(actual_ordinal),
            } => write!(
                formatter,
                "有序结果序列损坏：期待工作 {expected_ordinal}，却先得到工作 {actual_ordinal}"
            ),
            Self::IncompleteResultSequence {
                expected_ordinal,
                actual_ordinal: None,
            } => write!(
                formatter,
                "有序结果序列不完整：通道在工作 {expected_ordinal} 返回前关闭"
            ),
        }
    }
}

impl<E> Error for OrderedExecutionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Finalization { source, .. } => Some(source),
            Self::IncompleteResultSequence { .. } => None,
        }
    }
}

/// 调用方为有序流水线提供的三个职责边界。
///
/// `execute` 与 `prepare` 可以乱序并发运行；`finalize` 始终按输入下标从小到大
/// 串行调用。任何执行或准备错误都会立即停止新工作准入，但错误仍按自然序交给
/// `finalize`，由调用方建立领域终态并转换为对上层有意义的失败。
pub(crate) trait OrderedExecutionHandler<T>: Send + Sync {
    type Executed: Send;
    type Prepared: Send;
    type StageError: Send;
    type State: Send;
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        ordinal: usize,
        task: &T,
    ) -> impl Future<Output = Result<Self::Executed, Self::StageError>> + Send;

    fn prepare(
        &self,
        ordinal: usize,
        task: &T,
        executed: Self::Executed,
    ) -> impl Future<Output = Result<Self::Prepared, Self::StageError>> + Send;

    fn finalize(
        &self,
        ordinal: usize,
        task: T,
        result: OrderedTaskResult<Self::Prepared, Self::StageError>,
        disposition: OrderedFinalizationDisposition,
        state: &mut Self::State,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// 准入首次从开放切换为停止时发出的同步观察。
    ///
    /// 该观察不能失败，也不能反向参与业务结果；调用方可用它记录指标或唤醒测试。
    fn admission_stopped(&self) {}
}

struct ExecutedTask<T, O, E> {
    ordinal: usize,
    task: T,
    result: Result<O, E>,
    in_flight_permit: OwnedSemaphorePermit,
}

struct PreparedTask<T, P, E> {
    ordinal: usize,
    task: T,
    result: OrderedTaskResult<P, E>,
    in_flight_permit: OwnedSemaphorePermit,
}

fn stop_admission<T, H>(stopped: &AtomicBool, handler: &H)
where
    H: OrderedExecutionHandler<T>,
{
    if !stopped.swap(true, Ordering::AcqRel) {
        handler.admission_stopped();
    }
}

async fn prepare_executed_task<T, H>(
    handler: &H,
    completion: ExecutedTask<T, H::Executed, H::StageError>,
) -> PreparedTask<T, H::Prepared, H::StageError>
where
    T: Send,
    H: OrderedExecutionHandler<T>,
{
    let ExecutedTask {
        ordinal,
        task,
        result,
        in_flight_permit,
    } = completion;
    let result = match result {
        Ok(executed) => match handler.prepare(ordinal, &task, executed).await {
            Ok(prepared) => OrderedTaskResult::Prepared(prepared),
            Err(source) => OrderedTaskResult::PreparationFailed(source),
        },
        Err(source) => OrderedTaskResult::ExecutionFailed(source),
    };
    PreparedTask {
        ordinal,
        task,
        result,
        in_flight_permit,
    }
}

/// 执行一组已经按自然序排列的工作。
///
/// 返回 `Completed(state)` 表示每项工作都完成了自然序最终化；返回
/// `Cancelled` 表示已启动工作已经全部 drain 并建立无副作用终态。最终化失败时
/// 仍会 drain 所有已启动工作，并只返回自然序最早的失败。
pub(crate) async fn execute_ordered<T, H>(
    tasks: Vec<T>,
    limits: OrderedExecutionLimits,
    cancellation: &CooperativeCancellation,
    handler: &H,
    state: H::State,
) -> Result<OperationCompletion<H::State>, OrderedExecutionError<H::Error>>
where
    T: Send,
    H: OrderedExecutionHandler<T>,
{
    let task_count = tasks.len();
    if cancellation.is_requested() {
        return Ok(OperationCompletion::Cancelled);
    }

    let worker_count = limits.worker_count(task_count);
    let in_flight_window = Arc::new(Semaphore::new(limits.in_flight_window(task_count)));
    let pending_tasks = Arc::new(std::sync::Mutex::new(
        tasks.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let (execution_sender, mut execution_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ExecutedTask<T, H::Executed, H::StageError>>();
    let (prepared_sender, mut prepared_receiver) =
        tokio::sync::mpsc::unbounded_channel::<PreparedTask<T, H::Prepared, H::StageError>>();

    let execution_lane = {
        let pending_tasks = Arc::clone(&pending_tasks);
        let stop = Arc::clone(&stop);
        let in_flight_window = Arc::clone(&in_flight_window);
        async move {
            let mut workers = FuturesUnordered::new();
            for _ in 0..worker_count {
                let pending_tasks = Arc::clone(&pending_tasks);
                let stop = Arc::clone(&stop);
                let in_flight_window = Arc::clone(&in_flight_window);
                let execution_sender = execution_sender.clone();
                workers.push(async move {
                    loop {
                        if stop.load(Ordering::Acquire) || cancellation.is_requested() {
                            break;
                        }

                        let permit = Arc::clone(&in_flight_window)
                            .acquire_owned()
                            .await
                            .expect("有序执行的本地在途窗口在运行期间不得关闭");
                        if stop.load(Ordering::Acquire) || cancellation.is_requested() {
                            break;
                        }

                        // 临界区只移动一个已经物化的工作，不跨 await 持锁。
                        let pending = pending_tasks
                            .lock()
                            .expect("有序执行的待处理队列锁不应中毒")
                            .pop_front();
                        let Some((ordinal, task)) = pending else {
                            break;
                        };

                        let result = handler.execute(ordinal, &task).await;
                        if result.is_err() {
                            stop_admission(stop.as_ref(), handler);
                        }
                        if execution_sender
                            .send(ExecutedTask {
                                ordinal,
                                task,
                                result,
                                in_flight_permit: permit,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
            drop(execution_sender);
            while workers.next().await.is_some() {}
        }
    };

    let preparation_lane = {
        let stop = Arc::clone(&stop);
        async move {
            let mut preparations = FuturesUnordered::new();
            let mut execution_closed = false;

            loop {
                if execution_closed && preparations.is_empty() {
                    break;
                }
                tokio::select! {
                    completion = execution_receiver.recv(), if !execution_closed => {
                        match completion {
                            Some(completion) => {
                                preparations.push(prepare_executed_task(handler, completion));
                            }
                            None => execution_closed = true,
                        }
                    }
                    prepared = preparations.next(), if !preparations.is_empty() => {
                        let prepared =
                            prepared.expect("非空的有序准备集合必须返回一个完成项");
                        if !matches!(prepared.result, OrderedTaskResult::Prepared(_)) {
                            stop_admission(stop.as_ref(), handler);
                        }
                        if prepared_sender.send(prepared).is_err() {
                            break;
                        }
                    }
                }
            }
            drop(prepared_sender);
        }
    };

    let finalization_lane = {
        let stop = Arc::clone(&stop);
        async move {
            let mut slots = std::iter::repeat_with(|| None)
                .take(task_count)
                .collect::<Vec<Option<PreparedTask<T, H::Prepared, H::StageError>>>>();
            let mut next_ordinal = 0usize;
            let mut primary_failure = None;
            let mut state = state;

            while let Some(completion) = prepared_receiver.recv().await {
                let ordinal = completion.ordinal;
                let slot = slots
                    .get_mut(ordinal)
                    .expect("有序执行只能返回输入范围内的工作序号");
                assert!(slot.is_none(), "有序执行不得重复返回同一工作");
                *slot = Some(completion);

                while let Some(completion) = slots.get_mut(next_ordinal).and_then(Option::take) {
                    next_ordinal += 1;
                    let PreparedTask {
                        ordinal,
                        task,
                        result,
                        in_flight_permit,
                    } = completion;
                    let disposition = if cancellation.is_requested() {
                        OrderedFinalizationDisposition::CancelledNoApply
                    } else if primary_failure.is_some() {
                        OrderedFinalizationDisposition::AfterEarlierFailureNoApply
                    } else {
                        OrderedFinalizationDisposition::Apply
                    };

                    if let Err(source) = handler
                        .finalize(ordinal, task, result, disposition, &mut state)
                        .await
                    {
                        stop_admission(stop.as_ref(), handler);
                        if primary_failure.is_none() {
                            primary_failure =
                                Some(OrderedExecutionError::Finalization { ordinal, source });
                        }
                    }
                    drop(in_flight_permit);
                }
            }

            if let Some(primary_failure) = primary_failure {
                return Err(primary_failure);
            }
            if let Some(offset) = slots[next_ordinal..].iter().position(Option::is_some) {
                return Err(OrderedExecutionError::IncompleteResultSequence {
                    expected_ordinal: next_ordinal,
                    actual_ordinal: Some(next_ordinal + offset),
                });
            }
            if !cancellation.is_requested() && next_ordinal < task_count {
                return Err(OrderedExecutionError::IncompleteResultSequence {
                    expected_ordinal: next_ordinal,
                    actual_ordinal: None,
                });
            }

            if cancellation.is_requested() {
                Ok(OperationCompletion::Cancelled)
            } else {
                Ok(OperationCompletion::Completed(state))
            }
        }
    };

    // 三条流水线各自拥有其并发集合。装箱只缩小调用方 Future 的静态布局，
    // 不改变背压、顺序或任务生命周期。
    let (_, _, finalization) = future::join3(
        Box::pin(execution_lane),
        Box::pin(preparation_lane),
        Box::pin(finalization_lane),
    )
    .await;
    finalization
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct OrderingHarness {
        execution_gates: Vec<Arc<Semaphore>>,
        preparation_gates: Vec<Arc<Semaphore>>,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        finalized: Mutex<Vec<(usize, OrderedFinalizationDisposition)>>,
    }

    impl OrderingHarness {
        fn new(task_count: usize) -> Self {
            Self {
                execution_gates: (0..task_count)
                    .map(|_| Arc::new(Semaphore::new(1)))
                    .collect(),
                preparation_gates: (0..task_count)
                    .map(|_| Arc::new(Semaphore::new(1)))
                    .collect(),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                finalized: Mutex::new(Vec::new()),
            }
        }
    }

    impl OrderedExecutionHandler<usize> for OrderingHarness {
        type Executed = usize;
        type Prepared = usize;
        type StageError = Infallible;
        type State = usize;
        type Error = Infallible;

        async fn execute(
            &self,
            ordinal: usize,
            task: &usize,
        ) -> Result<Self::Executed, Self::StageError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active.fetch_max(active, Ordering::AcqRel);
            self.execution_gates[ordinal]
                .acquire()
                .await
                .expect("测试执行闸门不应关闭")
                .forget();
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(*task)
        }

        async fn prepare(
            &self,
            ordinal: usize,
            _task: &usize,
            executed: Self::Executed,
        ) -> Result<Self::Prepared, Self::StageError> {
            self.preparation_gates[ordinal]
                .acquire()
                .await
                .expect("测试准备闸门不应关闭")
                .forget();
            Ok(executed)
        }

        async fn finalize(
            &self,
            ordinal: usize,
            task: usize,
            result: OrderedTaskResult<Self::Prepared, Self::StageError>,
            disposition: OrderedFinalizationDisposition,
            state: &mut Self::State,
        ) -> Result<(), Self::Error> {
            let prepared = match result {
                OrderedTaskResult::Prepared(prepared) => prepared,
                OrderedTaskResult::ExecutionFailed(source)
                | OrderedTaskResult::PreparationFailed(source) => match source {},
            };
            assert_eq!(task, ordinal);
            assert_eq!(prepared, ordinal);
            self.finalized
                .lock()
                .expect("测试最终化记录锁不应中毒")
                .push((ordinal, disposition));
            *state += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn unordered_execution_and_preparation_are_finalized_in_natural_order() {
        let mut harness = OrderingHarness::new(6);
        harness.execution_gates[0] = Arc::new(Semaphore::new(0));
        let first_gate = Arc::clone(&harness.execution_gates[0]);
        let cancellation = CooperativeCancellation::default();
        let limits = OrderedExecutionLimits::new(
            NonZeroUsize::new(2).expect("测试执行宽度必须非零"),
            NonZeroUsize::new(3).expect("测试窗口倍率必须非零"),
        );

        let run = execute_ordered((0..6).collect(), limits, &cancellation, &harness, 0);
        let release = async move {
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
            first_gate.add_permits(1);
        };
        let (result, ()) = tokio::join!(run, release);

        assert_eq!(
            result.expect("测试流水线应成功"),
            OperationCompletion::Completed(6)
        );
        assert_eq!(
            *harness.finalized.lock().expect("测试最终化记录锁不应中毒"),
            (0..6)
                .map(|ordinal| (ordinal, OrderedFinalizationDisposition::Apply))
                .collect::<Vec<_>>()
        );
        assert!(harness.maximum_active.load(Ordering::Acquire) <= 2);
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct TestError(usize);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "test failure {}", self.0)
        }
    }

    impl Error for TestError {}

    struct FailureHarness {
        execute_failures: Vec<usize>,
        gates: Vec<Arc<Semaphore>>,
        started: Mutex<Vec<usize>>,
        finalized: Mutex<Vec<(usize, OrderedFinalizationDisposition)>>,
    }

    impl OrderedExecutionHandler<usize> for FailureHarness {
        type Executed = usize;
        type Prepared = usize;
        type StageError = TestError;
        type State = ();
        type Error = TestError;

        async fn execute(
            &self,
            ordinal: usize,
            _task: &usize,
        ) -> Result<Self::Executed, Self::StageError> {
            self.started
                .lock()
                .expect("测试准入记录锁不应中毒")
                .push(ordinal);
            self.gates[ordinal]
                .acquire()
                .await
                .expect("测试执行闸门不应关闭")
                .forget();
            if self.execute_failures.contains(&ordinal) {
                Err(TestError(ordinal))
            } else {
                Ok(ordinal)
            }
        }

        async fn prepare(
            &self,
            _ordinal: usize,
            _task: &usize,
            executed: Self::Executed,
        ) -> Result<Self::Prepared, Self::StageError> {
            Ok(executed)
        }

        async fn finalize(
            &self,
            ordinal: usize,
            _task: usize,
            result: OrderedTaskResult<Self::Prepared, Self::StageError>,
            disposition: OrderedFinalizationDisposition,
            _state: &mut Self::State,
        ) -> Result<(), Self::Error> {
            self.finalized
                .lock()
                .expect("测试最终化记录锁不应中毒")
                .push((ordinal, disposition));
            match result {
                OrderedTaskResult::ExecutionFailed(source)
                | OrderedTaskResult::PreparationFailed(source) => Err(source),
                OrderedTaskResult::Prepared(_) => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn earliest_ordinal_failure_wins_and_started_work_is_drained() {
        let mut harness = FailureHarness {
            execute_failures: vec![1, 3],
            gates: (0..8).map(|_| Arc::new(Semaphore::new(1))).collect(),
            started: Mutex::new(Vec::new()),
            finalized: Mutex::new(Vec::new()),
        };
        harness.gates[1] = Arc::new(Semaphore::new(0));
        let earlier_gate = Arc::clone(&harness.gates[1]);
        let cancellation = CooperativeCancellation::default();
        let run = execute_ordered(
            (0..8).collect(),
            OrderedExecutionLimits::new(
                NonZeroUsize::new(4).expect("测试执行宽度必须非零"),
                NonZeroUsize::new(3).expect("测试窗口倍率必须非零"),
            ),
            &cancellation,
            &harness,
            (),
        );
        let release = async move {
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
            earlier_gate.add_permits(1);
        };
        let (result, ()) = tokio::join!(run, release);

        assert!(matches!(
            result,
            Err(OrderedExecutionError::Finalization {
                ordinal: 1,
                source: TestError(1),
            })
        ));
        let finalized = harness.finalized.lock().expect("测试最终化记录锁不应中毒");
        assert_eq!(
            finalized
                .iter()
                .map(|(ordinal, _)| *ordinal)
                .collect::<Vec<_>>(),
            {
                let mut started = harness
                    .started
                    .lock()
                    .expect("测试准入记录锁不应中毒")
                    .clone();
                started.sort_unstable();
                started
            }
        );
        assert!(
            finalized
                .iter()
                .filter(|(ordinal, _)| *ordinal > 1)
                .all(|(_, disposition)| {
                    *disposition == OrderedFinalizationDisposition::AfterEarlierFailureNoApply
                })
        );
    }

    #[tokio::test]
    async fn cancellation_drains_started_work_without_returning_state() {
        let mut harness = OrderingHarness::new(4);
        harness.execution_gates[0] = Arc::new(Semaphore::new(0));
        let first_gate = Arc::clone(&harness.execution_gates[0]);
        let cancellation = CooperativeCancellation::default();
        let request_cancellation = cancellation.clone();
        let run = execute_ordered(
            (0..4).collect(),
            OrderedExecutionLimits::new(
                NonZeroUsize::new(2).expect("测试执行宽度必须非零"),
                NonZeroUsize::new(3).expect("测试窗口倍率必须非零"),
            ),
            &cancellation,
            &harness,
            0,
        );
        let cancel = async move {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            request_cancellation.request();
            first_gate.add_permits(1);
        };
        let (result, ()) = tokio::join!(run, cancel);

        assert_eq!(
            result.expect("合作取消不应成为技术失败"),
            OperationCompletion::Cancelled
        );
        assert!(
            harness
                .finalized
                .lock()
                .expect("测试最终化记录锁不应中毒")
                .iter()
                .all(|(_, disposition)| {
                    *disposition == OrderedFinalizationDisposition::CancelledNoApply
                })
        );
    }

    #[test]
    fn limits_bound_workers_and_in_flight_work_without_limiting_total_work() {
        let limits = OrderedExecutionLimits::new(
            NonZeroUsize::new(8).expect("测试执行宽度必须非零"),
            NonZeroUsize::new(3).expect("测试窗口倍率必须非零"),
        );

        assert_eq!(limits.worker_count(0), 0);
        assert_eq!(limits.worker_count(3), 3);
        assert_eq!(limits.worker_count(100), 8);
        assert_eq!(limits.in_flight_window(0), 0);
        assert_eq!(limits.in_flight_window(3), 3);
        assert_eq!(limits.in_flight_window(100), 24);
        assert_eq!(limits.in_flight_window(usize::MAX), 24);
    }
}
