//! RPG Maker 项目命令与通用排他文件租约之间的唯一映射边界。

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
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
#[must_use = "项目命令租约必须存活到完整命令及运行方案最终提交结束"]
pub(crate) struct ProjectCommandLease<T> {
    _lease: Option<ExclusiveFileLease<T>>,
}

#[cfg(test)]
impl<T> ProjectCommandLease<T> {
    pub(crate) fn for_test(state: T) -> Self {
        Self {
            _lease: Some(ExclusiveFileLease::new(state)),
        }
    }
}

impl ProjectCommandLease<()> {
    /// 建立一个只用于下层既有服务签名的租约见证。
    ///
    /// 组合根必须同时持有由真实 `ProjectCommandLeaseService` 返回的租约。该见证本身
    /// 不拥有任何锁，只避免下层服务再次获取同一把不可重入的项目锁。
    const fn already_held() -> Self {
        Self { _lease: None }
    }
}

/// 组合根已经持有真实项目租约时，向既有纵向服务提供的无二次加锁见证。
///
/// 本类型只能从仍存活的真实租约借用构造；不提供 `Default` 或无参构造，因而下层服务
/// 无法在组合根未持锁时伪造“已经持有”状态。
#[derive(Clone, Copy, Debug)]
pub(crate) struct AlreadyHeldProjectCommandLeaseProvider<'lease> {
    _lease: PhantomData<&'lease ()>,
}

impl<'lease> AlreadyHeldProjectCommandLeaseProvider<'lease> {
    pub(crate) const fn new<T>(_lease: &'lease ProjectCommandLease<T>) -> Self {
        Self {
            _lease: PhantomData,
        }
    }
}

impl ProjectCommandLeaseProvider for AlreadyHeldProjectCommandLeaseProvider<'_> {
    type Error = Infallible;
    type LeaseState = ();

    async fn acquire(
        &self,
        _project: &ProjectName,
    ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>> {
        Ok(ProjectCommandLease::already_held())
    }
}

/// 为同一项目串行化五类 RPG Maker 命令的业务能力。
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
                ExclusiveFileLeaseError::Unavailable { source, .. } => {
                    ProjectCommandLeaseError::Unavailable {
                        project: project.clone(),
                        source,
                    }
                }
            })?;
        Ok(ProjectCommandLease {
            _lease: Some(lease),
        })
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
            RpgMakerEngine::Mz,
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

    #[tokio::test]
    async fn already_held_provider_is_borrowed_from_a_live_real_lease() {
        let real_lease = ProjectCommandLease::for_test(());
        let provider = AlreadyHeldProjectCommandLeaseProvider::new(&real_lease);
        let project = "借用见证".parse().expect("测试项目名应该合法");

        let nested_witness = provider
            .acquire(&project)
            .await
            .expect("存活的真实租约应可建立下层见证");

        drop(nested_witness);
        drop(real_lease);
    }
}
