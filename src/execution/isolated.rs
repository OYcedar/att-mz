//! 隔离没有取消回调的第三方纯计算，同时让调用方能够及时响应取消。

use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// TOML、Aho-Corasick 和 PCRE2 的相关构建 API 都没有取消回调。等待隔离 worker 时
/// 按此周期轮询调用方，避免取消必须等待第三方原子计算自行结束。
const ISOLATED_OPERATION_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

type IsolatedOperationPanic = Box<dyn Any + Send + 'static>;

#[derive(Debug)]
pub(crate) enum IsolatedOperationError<E> {
    Cancelled(E),
    Start {
        operation: &'static str,
        source: io::Error,
    },
}

/// 在独立 OS worker 中执行没有取消回调的第三方纯计算。
///
/// `operation` 必须拥有全部输入、不得借用调用方状态或执行副作用。正常路径收到结果后
/// 始终 join。取消路径会丢弃 join handle，使调用方不再等待无法中断的第三方调用；
/// worker 只继续持有本次纯计算输入，并在计算结束或进程退出时回收。调用方必须保证同一
/// 取消事实会阻止继续派生任务，使一次操作最多遗留取消发生时已经运行的有限任务。
pub(crate) fn run_isolated_operation<T, E>(
    operation_name: &'static str,
    operation: impl FnOnce() -> T + Send + 'static,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<T, IsolatedOperationError<E>>
where
    T: Send + 'static,
{
    run_isolated_operation_with_cancelled_worker(operation_name, operation, ensure_running, drop)
}

fn run_isolated_operation_with_cancelled_worker<T, E>(
    operation_name: &'static str,
    operation: impl FnOnce() -> T + Send + 'static,
    mut ensure_running: impl FnMut() -> Result<(), E>,
    on_cancelled_worker: impl FnOnce(JoinHandle<()>),
) -> Result<T, IsolatedOperationError<E>>
where
    T: Send + 'static,
{
    ensure_running().map_err(IsolatedOperationError::Cancelled)?;
    let mut on_cancelled_worker = Some(on_cancelled_worker);

    let skip_if_not_started = Arc::new(AtomicBool::new(false));
    let worker_skip = Arc::clone(&skip_if_not_started);
    let (sender, receiver) = mpsc::sync_channel::<Result<T, IsolatedOperationPanic>>(1);
    let worker = thread::Builder::new()
        .name(operation_name.to_owned())
        .spawn(move || {
            if worker_skip.load(Ordering::Acquire) {
                return;
            }
            let outcome = catch_unwind(AssertUnwindSafe(operation));
            let _ = sender.send(outcome);
        })
        .map_err(|source| IsolatedOperationError::Start {
            operation: operation_name,
            source,
        })?;

    loop {
        match receiver.try_recv() {
            Ok(outcome) => {
                return finish_isolated_operation(worker, outcome, &mut ensure_running);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => match worker.join() {
                Ok(()) => panic!("{operation_name} worker 未返回结果"),
                Err(payload) => resume_unwind(payload),
            },
        }
        if let Err(cancellation) = ensure_running() {
            skip_if_not_started.store(true, Ordering::Release);
            on_cancelled_worker
                .take()
                .expect("取消路径只能移交一次 worker")(worker);
            return Err(IsolatedOperationError::Cancelled(cancellation));
        }
        match receiver.recv_timeout(ISOLATED_OPERATION_CANCEL_POLL_INTERVAL) {
            Ok(outcome) => {
                return finish_isolated_operation(worker, outcome, &mut ensure_running);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
                Ok(()) => panic!("{operation_name} worker 未返回结果"),
                Err(payload) => resume_unwind(payload),
            },
        }
    }
}

fn finish_isolated_operation<T, E>(
    worker: JoinHandle<()>,
    outcome: Result<T, IsolatedOperationPanic>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<T, IsolatedOperationError<E>> {
    match worker.join() {
        Ok(()) => {}
        Err(payload) => resume_unwind(payload),
    }
    match outcome {
        Err(payload) => resume_unwind(payload),
        Ok(result) => {
            ensure_running().map_err(IsolatedOperationError::Cancelled)?;
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;
    #[cfg(feature = "release-stress")]
    use std::time::Instant;

    use super::*;

    struct ActiveWorkerCancellationObservation {
        result: Result<(), IsolatedOperationError<&'static str>>,
        returned_before_release: bool,
        retained_cancelled_worker: bool,
        #[cfg(feature = "release-stress")]
        elapsed: Duration,
    }

    fn observe_active_worker_cancellation() -> ActiveWorkerCancellationObservation {
        let cancellation = Arc::new(AtomicBool::new(false));
        let caller_cancellation = Arc::clone(&cancellation);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let (worker_sender, worker_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let caller = thread::spawn(move || {
            let result = run_isolated_operation_with_cancelled_worker(
                "att-isolated-test",
                move || {
                    started_sender.send(()).expect("应通知测试 worker 已启动");
                    release_receiver.recv().expect("应释放测试 worker");
                },
                move || {
                    if caller_cancellation.load(Ordering::Acquire) {
                        Err("cancelled")
                    } else {
                        Ok(())
                    }
                },
                move |worker| {
                    worker_sender
                        .send(worker)
                        .expect("测试必须保留被取消 worker 的 join handle");
                },
            );
            result_sender.send(result).expect("应返回隔离调用结果");
        });

        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("隔离 worker 应在取消前实际开始运行");
        #[cfg(feature = "release-stress")]
        let cancellation_started = Instant::now();
        cancellation.store(true, Ordering::Release);
        let first_result = result_receiver.recv_timeout(Duration::from_secs(1));
        #[cfg(feature = "release-stress")]
        let cancellation_elapsed = cancellation_started.elapsed();
        let cancelled_worker = worker_receiver.try_recv().ok();
        let returned_before_release = first_result.is_ok();

        release_sender.send(()).expect("应释放被隔离的测试 worker");
        let result = match first_result {
            Ok(result) => result,
            Err(_) => result_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("释放 worker 后隔离调用必须结束"),
        };
        let cancelled_worker =
            cancelled_worker.or_else(|| worker_receiver.recv_timeout(Duration::from_secs(1)).ok());
        let retained_cancelled_worker = cancelled_worker.is_some();
        if let Some(worker) = cancelled_worker {
            worker.join().expect("取消测试必须回收被隔离的 worker");
        }
        caller.join().expect("调用线程应正常结束");

        ActiveWorkerCancellationObservation {
            result,
            returned_before_release,
            retained_cancelled_worker,
            #[cfg(feature = "release-stress")]
            elapsed: cancellation_elapsed,
        }
    }

    #[test]
    fn active_worker_returns_cancellation_before_release_and_test_reclaims_it() {
        let observation = observe_active_worker_cancellation();

        assert!(matches!(
            observation.result,
            Err(IsolatedOperationError::Cancelled("cancelled"))
        ));
        assert!(
            observation.returned_before_release,
            "调用方取消后不应等待第三方计算结束"
        );
        assert!(
            observation.retained_cancelled_worker,
            "测试必须取得并 join 取消路径留下的 worker"
        );
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_active_worker_cancellation_returns_within_poll_budget() {
        let observation = observe_active_worker_cancellation();

        assert!(
            observation.elapsed < Duration::from_millis(500),
            "10ms 轮询应及时返回，实际耗时 {:?}",
            observation.elapsed
        );
    }
}
