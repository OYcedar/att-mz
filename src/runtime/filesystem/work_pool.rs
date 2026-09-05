//! 文件工作池的准入、派发、取消与显式关闭。

use super::error::{SystemFileSystemBuildError, SystemFileSystemError};
use async_channel::{Receiver, Sender};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

pub(super) fn ensure_operation_active(
    cancellation: &AtomicBool,
    operation: &'static str,
    path: &Path,
) -> Result<(), SystemFileSystemError> {
    if cancellation.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(SystemFileSystemError::Cancelled {
            operation,
            path: path.to_path_buf(),
        })
    }
}

type FileJob = Box<dyn FnOnce() + Send + 'static>;

pub(super) struct FileWorkPool {
    sender: Sender<FileJob>,
    workers: Mutex<Option<Vec<JoinHandle<()>>>>,
    admission: Arc<Semaphore>,
    waits_active: Arc<AtomicBool>,
    waits_cancelled: Arc<Notify>,
    width: usize,
}

impl FileWorkPool {
    pub(super) fn new(worker_threads: usize) -> Result<Self, SystemFileSystemBuildError> {
        let (sender, receiver) = async_channel::unbounded();
        let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(worker_threads);
        for index in 0..worker_threads {
            let receiver = receiver.clone();
            let worker = match thread::Builder::new()
                .name(format!("filesystem-worker-{index}"))
                .spawn(move || file_worker(receiver))
            {
                Ok(worker) => worker,
                Err(source) => {
                    sender.close();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(SystemFileSystemBuildError::WorkerSpawn(source));
                }
            };
            workers.push(worker);
        }
        Ok(Self {
            sender,
            workers: Mutex::new(Some(workers)),
            admission: Arc::new(Semaphore::new(worker_threads)),
            waits_active: Arc::new(AtomicBool::new(true)),
            waits_cancelled: Arc::new(Notify::new()),
            width: worker_threads,
        })
    }

    async fn acquire_admission(
        &self,
        operation: &'static str,
        path: &Path,
    ) -> Result<OwnedSemaphorePermit, SystemFileSystemError> {
        let cancelled = self.waits_cancelled.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        ensure_operation_active(&self.waits_active, operation, path)?;
        tokio::select! {
            biased;
            _ = &mut cancelled => Err(SystemFileSystemError::Cancelled {
                operation,
                path: path.to_path_buf(),
            }),
            permit = Arc::clone(&self.admission).acquire_owned() => {
                permit.map_err(|_| SystemFileSystemError::Closed)
            }
        }
    }

    pub(super) async fn execute<T, F>(
        &self,
        operation: &'static str,
        path: &Path,
        work: F,
    ) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let admission = self.acquire_admission(operation, path).await?;
        self.dispatch(admission, work).await
    }

    /// 接收业务取消后仍需运行至终态的工作：终态可观测性提交，以及按值接管已准备
    /// 目录 token 的 publish/discard——token 一旦交付就必须由本次执行走到终态，
    /// 否则取消窗口内的正常清理会以“token 未经 publish/discard 丢弃”告终。
    ///
    /// 该入口只绕过 `cancel_waits`，仍受同一个执行许可和 `shutdown` 的关闭边界约束。
    pub(super) async fn execute_terminal<T, F>(&self, work: F) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let admission = Arc::clone(&self.admission)
            .acquire_owned()
            .await
            .map_err(|_| SystemFileSystemError::Closed)?;
        self.dispatch(admission, work).await
    }

    async fn dispatch<T, F>(
        &self,
        admission: OwnedSemaphorePermit,
        work: F,
    ) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (response_sender, response_receiver) = async_channel::bounded(1);
        self.sender
            .send(Box::new(move || {
                let _admission = admission;
                let result = work();
                let _ = response_sender.send_blocking(result);
            }))
            .await
            .map_err(|_| SystemFileSystemError::Closed)?;
        response_receiver
            .recv()
            .await
            .map_err(|_| SystemFileSystemError::WorkerPanicked)
    }

    pub(super) async fn execute_with_abandon<T, F, A>(
        &self,
        operation: &'static str,
        path: &Path,
        work: F,
        abandon: A,
    ) -> Result<T, SystemFileSystemError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        A: FnOnce(&mut T) + Send + 'static,
    {
        let admission = self.acquire_admission(operation, path).await?;
        let (response_sender, response_receiver) = async_channel::bounded(1);
        self.sender
            .send(Box::new(move || {
                let _admission = admission;
                let result = work();
                match response_sender.send_blocking(result) {
                    Ok(()) => {}
                    Err(error) => {
                        let mut abandoned_result = error.0;
                        abandon(&mut abandoned_result);
                    }
                }
            }))
            .await
            .map_err(|_| SystemFileSystemError::Closed)?;
        response_receiver
            .recv()
            .await
            .map_err(|_| SystemFileSystemError::WorkerPanicked)
    }

    pub(super) async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
        self.cancel_waits();
        self.sender.close();
        let workers = self
            .workers
            .lock()
            .expect("文件工作线程所有权锁不应中毒")
            .take();
        let Some(workers) = workers else {
            return Ok(());
        };
        let clean = tokio::task::spawn_blocking(move || join_file_workers(workers))
            .await
            .map_err(|_| SystemFileSystemError::WorkerPanicked)?;
        if clean {
            Ok(())
        } else {
            Err(SystemFileSystemError::WorkerPanicked)
        }
    }

    pub(super) fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.waits_active)
    }

    pub(super) fn cancel_waits(&self) {
        self.waits_active.store(false, Ordering::Release);
        self.waits_cancelled.notify_waiters();
    }

    pub(super) const fn width(&self) -> usize {
        self.width
    }
}

fn join_file_workers(workers: Vec<thread::JoinHandle<()>>) -> bool {
    let mut clean = true;
    for worker in workers {
        if worker.join().is_err() {
            clean = false;
        }
    }
    clean
}

impl Drop for FileWorkPool {
    fn drop(&mut self) {
        self.cancel_waits();
        self.sender.close();
    }
}

fn file_worker(receiver: Receiver<FileJob>) {
    while let Ok(job) = receiver.recv_blocking() {
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

#[cfg(test)]
mod tests;
