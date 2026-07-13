#![allow(dead_code, reason = "Lua 宿主尚未接入生产组合根")]

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::att_mz::project::OpenedProject;

/// 可以由可信 Lua 程序参与的业务阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LuaPhase {
    Extract,
}

/// 交给可信 Lua 宿主的一次完整调用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LuaInvocation {
    script_path: PathBuf,
    phase: LuaPhase,
    project: OpenedProject,
}

impl LuaInvocation {
    pub(crate) fn new(script_path: PathBuf, phase: LuaPhase, project: OpenedProject) -> Self {
        Self {
            script_path,
            phase,
            project,
        }
    }

    pub(crate) fn script_path(&self) -> &Path {
        &self.script_path
    }

    pub(crate) fn phase(&self) -> LuaPhase {
        self.phase
    }

    pub(crate) fn project(&self) -> &OpenedProject {
        &self.project
    }
}

/// 完整拥有可信 Lua 程序生命周期与项目数据库桥接的宿主。
///
/// Lua 是用户明确选择并完全信任的本机程序，不建立沙箱。宿主负责加载脚本、建立
/// VM、打开同一个项目数据库并注入 `ctx.db`、执行以及关闭所有资源。Lua 自己拥有
/// schema、数据身份、译文继承、事务划分和跨阶段协议；Rust 不扫描、解释、迁移或
/// 默认消费 Lua 产物，也不会把整个脚本隐式包进长事务。宿主还负责 worker 与阻塞
/// 隔离，公开 Future 不得阻塞异步执行器线程；脚本失败时回滚未提交事务，再关闭
/// 数据库连接和 VM。
pub(crate) trait TrustedLuaExecutionHost: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        invocation: LuaInvocation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 执行一次自由 Lua 提取。
pub(crate) trait LuaExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把提取阶段语义交给可信 Lua 宿主。
pub(crate) struct LuaExtractionService<H> {
    host: H,
}

impl<H> LuaExtractionService<H> {
    pub(crate) fn new(host: H) -> Self {
        Self { host }
    }
}

impl<H> LuaExtraction for LuaExtractionService<H>
where
    H: TrustedLuaExecutionHost,
{
    type Error = LuaExtractionError<H::Error>;

    async fn run(&self, project: &OpenedProject, script_path: PathBuf) -> Result<(), Self::Error> {
        let invocation =
            LuaInvocation::new(script_path.clone(), LuaPhase::Extract, project.clone());
        self.host
            .execute(invocation)
            .await
            .map_err(|source| LuaExtractionError::ExecuteHost {
                script_path,
                source,
            })
    }
}

/// Lua 提取宿主执行失败。
#[derive(Debug)]
pub(crate) enum LuaExtractionError<E> {
    ExecuteHost { script_path: PathBuf, source: E },
}

impl<E> fmt::Display for LuaExtractionError<E>
where
    E: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecuteHost {
                script_path,
                source,
            } => write!(
                formatter,
                "执行可信 Lua 宿主失败 {}：{source}",
                script_path.display()
            ),
        }
    }
}

impl<E> Error for LuaExtractionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecuteHost { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<LuaInvocation>>>,
        fail: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type Error = FakeError;

        async fn execute(&self, invocation: LuaInvocation) -> Result<(), Self::Error> {
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(invocation);
            if self.fail { Err(FakeError) } else { Ok(()) }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("host failed")
        }
    }

    impl Error for FakeError {}

    fn opened_project() -> OpenedProject {
        OpenedProject::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/games/alice"),
            PathBuf::from("C:/projects/alice.db"),
            "ja".to_owned(),
            "zh-CN".to_owned(),
        )
    }

    #[tokio::test]
    async fn passes_the_complete_extract_invocation_to_the_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaExtractionService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
        });
        let project = opened_project();

        service
            .run(&project, PathBuf::from("scripts/extract.lua"))
            .await
            .expect("Lua 提取应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("宿主应该收到一次调用");
        assert_eq!(invocation.phase(), LuaPhase::Extract);
        assert_eq!(invocation.script_path(), Path::new("scripts/extract.lua"));
        assert_eq!(invocation.project(), &project);
    }

    #[tokio::test]
    async fn preserves_the_script_path_and_host_error() {
        let service = LuaExtractionService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
        });

        let error = service
            .run(&opened_project(), PathBuf::from("broken.lua"))
            .await
            .expect_err("宿主失败应该传播");

        assert!(matches!(
            &error,
            LuaExtractionError::ExecuteHost {
                script_path,
                source: FakeError
            } if script_path == &PathBuf::from("broken.lua")
        ));
        assert_eq!(
            error.source().and_then(|error| error.downcast_ref()),
            Some(&FakeError)
        );
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaExtractionService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
        });
        let project = opened_project();
        assert_send(service.run(&project, PathBuf::from("extract.lua")));
    }
}
