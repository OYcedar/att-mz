#![allow(dead_code, reason = "Lua Host 尚未接入生产组合根")]

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use crate::att_mz::lua::LuaProjectContext;
pub(crate) use crate::att_mz::lua::{LuaInvocation, TrustedLuaExecutionHost};
use crate::att_mz::project::OpenedProject;

/// 执行一次自由 Lua 提取。
pub(crate) trait LuaExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把 Extract 阶段已经建立的项目事实交给可信 Lua Host。
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
        let error_path = script_path.clone();
        let invocation =
            LuaInvocation::extract(script_path, LuaProjectContext::from_opened_project(project));

        self.host
            .execute(invocation)
            .await
            .map_err(|source| LuaExtractionError::ExecuteHost {
                script_path: error_path,
                source,
            })
    }
}

/// Lua Extract 阶段的 Host 执行失败。
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
                "执行可信 Lua 提取 Host 失败 {}：{source}",
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
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::{LuaPhase, LuaProjectContext};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedInvocation {
        phase: LuaPhase,
        script_path: PathBuf,
        project: LuaProjectContext,
    }

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<RecordedInvocation>>>,
        fail: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationProfile = ();
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationProfile>,
        ) -> Result<(), Self::Error> {
            let recorded = match invocation {
                LuaInvocation::Extract {
                    script_path,
                    project,
                } => RecordedInvocation {
                    phase: LuaPhase::Extract,
                    script_path,
                    project,
                },
                LuaInvocation::Translate { .. } => {
                    panic!("提取服务不应提交 Translate 调用")
                }
                LuaInvocation::WriteBack { .. } => {
                    panic!("提取服务不应提交 WriteBack 调用")
                }
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(recorded);

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
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-CN".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    #[tokio::test]
    async fn passes_complete_extract_context_to_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaExtractionService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
        });

        service
            .run(&opened_project(), PathBuf::from("scripts/extract.lua"))
            .await
            .expect("Lua 提取应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应该收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::Extract);
        assert_eq!(invocation.script_path, PathBuf::from("scripts/extract.lua"));
        assert_eq!(invocation.project.name().as_str(), "alice");
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
        assert_eq!(invocation.project.source_language(), "ja");
        assert_eq!(invocation.project.target_language(), "zh-CN");
    }

    #[tokio::test]
    async fn preserves_script_path_and_host_source() {
        let service = LuaExtractionService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
        });

        let error = service
            .run(&opened_project(), PathBuf::from("broken extract.lua"))
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaExtractionError::ExecuteHost {
                script_path,
                source: FakeError
            } if script_path == &PathBuf::from("broken extract.lua")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError)
        );
        assert!(error.to_string().contains("broken extract.lua"));
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
