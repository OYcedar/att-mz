//! 翻译项目命令与通用排他文件租约之间的唯一映射边界。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use crate::diagnostic::{
    DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage, FileSystemOperation,
    StateEffect,
};
use crate::project_name::ProjectName;
use crate::runtime::filesystem::SystemFileSystemError;
use crate::storage::file_system::{
    ExclusiveFileLease, ExclusiveFileLeaseError, ExclusiveFileLeaseProvider,
    ExclusiveFileLeaseRequest,
};

const ATT_LOCK_DIRECTORY: &str = ".att-locks";
const PROJECT_LOCK_DIRECTORY: &str = "projects";

/// 持有同一翻译项目的跨进程排他权直到完整命令结束。
#[must_use = "项目命令租约必须存活到完整命令及运行方案最终提交结束"]
pub(crate) struct ProjectCommandLease<T> {
    _lease: ExclusiveFileLease<T>,
}

/// 为同一引擎内的同名项目串行化命令。
pub(crate) trait ProjectCommandLeaseProvider: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type LeaseState: Send + 'static;

    fn acquire(
        &self,
        project: &ProjectName,
    ) -> impl Future<
        Output = Result<
            ProjectCommandLease<Self::LeaseState>,
            ProjectCommandLeaseError<Self::Error>,
        >,
    > + Send;
}

/// 把项目名称确定性映射到产品锁根下的通用文件租约。
pub(crate) struct ProjectCommandLeaseService<L> {
    lock_directory: PathBuf,
    lease_provider: L,
}

impl<L> ProjectCommandLeaseService<L> {
    pub(crate) fn new(
        projects_root: PathBuf,
        engine_storage_name: &str,
        lease_provider: L,
    ) -> Self {
        Self {
            lock_directory: projects_root
                .join(ATT_LOCK_DIRECTORY)
                .join(PROJECT_LOCK_DIRECTORY)
                .join(engine_storage_name),
            lease_provider,
        }
    }
}

impl<L> ProjectCommandLeaseProvider for ProjectCommandLeaseService<L>
where
    L: ExclusiveFileLeaseProvider,
{
    type Error = L::Error;
    type LeaseState = L::LeaseState;

    async fn acquire(
        &self,
        project: &ProjectName,
    ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>> {
        let request =
            ExclusiveFileLeaseRequest::new(self.lock_directory.clone(), project.as_str().into())
                .expect("受信项目根与 ProjectName 必须能建立通用文件租约请求");
        let lease = self
            .lease_provider
            .acquire_exclusive_file_lease(request)
            .await
            .map_err(|source| match source {
                ExclusiveFileLeaseError::Unavailable { source, .. } => {
                    ProjectCommandLeaseError::Unavailable {
                        project: project.clone(),
                        source,
                    }
                }
            })?;
        Ok(ProjectCommandLease { _lease: lease })
    }
}

#[derive(Debug)]
pub(crate) enum ProjectCommandLeaseError<E> {
    Unavailable { project: ProjectName, source: E },
}

impl<E: fmt::Display> fmt::Display for ProjectCommandLeaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { project, source } => {
                write!(formatter, "无法取得项目 {project} 的命令租约：{source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for ProjectCommandLeaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable { source, .. } => Some(source),
        }
    }
}

impl ProjectCommandLeaseError<Box<SystemFileSystemError>> {
    pub(crate) fn diagnostic_report_at(
        &self,
        stage: FileSystemDiagnosticStage,
    ) -> DiagnosticReport {
        match self {
            Self::Unavailable { source, .. } => source.diagnostic_report(
                FileSystemDiagnosticContext::new(stage, FileSystemOperation::AcquireExclusiveLease),
                StateEffect::Unchanged,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct RecordingLeaseProvider {
        requests: Arc<Mutex<Vec<ExclusiveFileLeaseRequest>>>,
    }

    impl ExclusiveFileLeaseProvider for RecordingLeaseProvider {
        type Error = TestError;
        type LeaseState = ();

        async fn acquire_exclusive_file_lease(
            &self,
            request: ExclusiveFileLeaseRequest,
        ) -> Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>
        {
            self.requests
                .lock()
                .expect("租约请求记录锁不应中毒")
                .push(request.clone());
            Ok(ExclusiveFileLease::new(()))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct TestError;

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("test error")
        }
    }

    impl Error for TestError {}

    #[tokio::test]
    async fn maps_project_to_fixed_product_lock_namespace() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let service = ProjectCommandLeaseService::new(
            PathBuf::from("C:/att/projects"),
            "mz",
            RecordingLeaseProvider {
                requests: Arc::clone(&requests),
            },
        );
        let project = "游戏 One".parse().expect("测试项目名应该合法");

        let lease = service.acquire(&project).await.expect("应该取得项目租约");

        let requests = requests.lock().expect("租约请求记录锁不应中毒");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].lock_directory(),
            std::path::Path::new("C:/att/projects/.att-locks/projects/mz")
        );
        assert_eq!(requests[0].identity(), std::ffi::OsStr::new("游戏 One"));
        drop(lease);
    }
}
