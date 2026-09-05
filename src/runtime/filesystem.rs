//! 文件系统根的构造、共享生命周期与能力入口。

mod access;
mod candidate;
mod error;
mod fingerprint;
mod journal;
mod lease;
mod observation;
mod path;
mod publication;
mod recovery;
mod scoped;
#[cfg(test)]
mod test_faults;
mod work_pool;
mod workspace;

use crate::runtime::performance::RunPerformanceCounters;
pub(crate) use error::{
    SystemFileSystemBuildError, SystemFileSystemError, TerminalObservationOperation,
};
use observation::write_new_terminal_observation_file_sync;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
pub(crate) use test_faults::{
    TestPublishFaultAction, TestPublishFaultPoint, register_test_publish_faults,
};
use work_pool::FileWorkPool;

/// 目录发布锁位置。
#[derive(Clone, Debug)]
pub(crate) struct DirectoryPublisherConfig {
    lock_directory: PathBuf,
}

impl DirectoryPublisherConfig {
    pub(crate) fn production(lock_directory: PathBuf) -> Result<Self, SystemFileSystemBuildError> {
        if lock_directory.as_os_str().is_empty() {
            return Err(SystemFileSystemBuildError::InvalidConfiguration(
                "目录发布锁目录不能为空",
            ));
        }
        Ok(Self { lock_directory })
    }

    #[cfg(test)]
    pub(crate) fn lock_directory(&self) -> &Path {
        &self.lock_directory
    }
}

/// 一个进程内共享、显式关闭的生产文件系统根。
#[derive(Clone)]
pub(crate) struct SystemFileSystem {
    inner: Arc<SystemFileSystemInner>,
}

struct SystemFileSystemInner {
    pool: FileWorkPool,
    performance: Arc<RunPerformanceCounters>,
}

impl SystemFileSystem {
    #[cfg(test)]
    pub(crate) fn new() -> Result<Self, SystemFileSystemBuildError> {
        Self::new_with_performance(Arc::new(RunPerformanceCounters::default()))
    }

    pub(crate) fn new_with_performance(
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<Self, SystemFileSystemBuildError> {
        let worker_threads = std::thread::available_parallelism()
            .map_err(SystemFileSystemBuildError::AvailableParallelism)?
            .get()
            .min(4);
        Self::new_with_worker_threads(worker_threads, performance)
    }

    fn new_with_worker_threads(
        worker_threads: usize,
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<Self, SystemFileSystemBuildError> {
        let pool = FileWorkPool::new(worker_threads)?;
        Ok(Self {
            inner: Arc::new(SystemFileSystemInner { pool, performance }),
        })
    }

    /// 按调用方实际需要的发布配置建立目录发布能力。
    pub(crate) fn directory_publisher(
        &self,
        config: DirectoryPublisherConfig,
    ) -> SystemDirectoryPublisher {
        SystemDirectoryPublisher {
            inner: Arc::clone(&self.inner),
            config,
            publisher_identity: Arc::new(()),
        }
    }

    /// 同步唤醒项目租约与目录发布锁等待；已接管的清理和业务文件任务继续完成。
    pub(crate) fn cancel_waits(&self) {
        self.inner.pool.cancel_waits();
    }

    /// 原子提交一份终态非权威可观测性文件。
    ///
    /// 内容先写入同目录临时文件，完成 `write_all`、`flush` 和关闭后再执行无覆盖
    /// 重命名。该入口在业务取消后仍可接收工作；调用方必须在文件系统根
    /// `shutdown` 前等待其完成。
    pub(crate) async fn write_new_terminal_observation_file(
        &self,
        path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<(), SystemFileSystemError> {
        self.inner
            .pool
            .execute_terminal(move || write_new_terminal_observation_file_sync(&path, &bytes))
            .await?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
        self.inner.pool.shutdown().await
    }
}

/// 目录候选、受限编辑和可恢复发布组成的单一能力实例。
///
/// 实例身份同时约束候选 token 和编辑句柄，防止它们被其他发布器终结或修改。
#[derive(Clone)]
pub(crate) struct SystemDirectoryPublisher {
    inner: Arc<SystemFileSystemInner>,
    config: DirectoryPublisherConfig,
    publisher_identity: Arc<()>,
}

#[cfg(test)]
mod test_support;
