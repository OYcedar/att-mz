//! RPG Maker 项目命令与通用排他文件租约之间的唯一映射边界。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;
use crate::rpg_maker::RpgMakerEngine;
use crate::storage::file_system::{
    ExclusiveFileLease, ExclusiveFileLeaseError, ExclusiveFileLeaseProvider,
    ExclusiveFileLeaseRequest,
};

const ATT_LOCK_DIRECTORY: &str = ".att-locks";
const PROJECT_LOCK_DIRECTORY: &str = "projects";

/// 持有同一 RPG Maker 项目的跨进程排他权直到完整命令结束。
#[must_use = "项目命令租约必须存活到完整命令及其审计终态结束"]
pub(crate) struct ProjectCommandLease<T> {
    _lease: ExclusiveFileLease<T>,
}

#[cfg(test)]
impl<T> ProjectCommandLease<T> {
    pub(crate) fn for_test(state: T) -> Self {
        Self {
            _lease: ExclusiveFileLease::new(state),
        }
    }
}

/// 为同一项目串行化四类 RPG Maker 命令的业务能力。
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
    pub(crate) fn new(projects_root: PathBuf, engine: RpgMakerEngine, lease_provider: L) -> Self {
        Self {
            lock_directory: projects_root
                .join(ATT_LOCK_DIRECTORY)
                .join(PROJECT_LOCK_DIRECTORY)
                .join(engine.storage_name()),
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
                ExclusiveFileLeaseError::Busy { timeout, .. } => ProjectCommandLeaseError::Busy {
                    project: project.clone(),
                    timeout,
                },
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
    Busy {
        project: ProjectName,
        timeout: std::time::Duration,
    },
    Unavailable {
        project: ProjectName,
        source: E,
    },
}

impl<E: fmt::Display> fmt::Display for ProjectCommandLeaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { project, timeout } => write!(
                formatter,
                "项目 {project} 正由另一条命令处理，等待 {timeout:?} 后仍未取得租约"
            ),
            Self::Unavailable { project, source } => {
                write!(formatter, "无法取得项目 {project} 的命令租约：{source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for ProjectCommandLeaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Busy { .. } => None,
            Self::Unavailable { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    #[derive(Clone)]
    struct RecordingLeaseProvider {
        requests: Arc<Mutex<Vec<ExclusiveFileLeaseRequest>>>,
        busy: bool,
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
            if self.busy {
                Err(ExclusiveFileLeaseError::Busy {
                    identity: request.identity().to_os_string(),
                    timeout: Duration::from_millis(25),
                })
            } else {
                Ok(ExclusiveFileLease::new(()))
            }
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
            RpgMakerEngine::Mz,
            RecordingLeaseProvider {
                requests: Arc::clone(&requests),
                busy: false,
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

    #[tokio::test]
    async fn translates_generic_busy_state_to_project_semantics() {
        let service = ProjectCommandLeaseService::new(
            PathBuf::from("C:/att/projects"),
            RpgMakerEngine::Mv,
            RecordingLeaseProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
                busy: true,
            },
        );
        let project = "Game".parse().expect("测试项目名应该合法");

        assert!(matches!(
            service.acquire(&project).await,
            Err(ProjectCommandLeaseError::Busy {
                project: busy_project,
                timeout,
            }) if busy_project == project && timeout == Duration::from_millis(25)
        ));
    }
}
