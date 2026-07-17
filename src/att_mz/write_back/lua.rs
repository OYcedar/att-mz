//! 把已经发布的 Standard 输出交给共享可信 Lua Host。

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::att_mz::lua::{LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost};
use crate::att_mz::project::OpenedProject;

use super::{LuaWriteBack, PublishedWriteBack};

/// 在 Standard 已发布输出上运行可信 Lua 写回程序。
pub(crate) struct LuaWriteBackService<H> {
    host: H,
}

impl<H> LuaWriteBackService<H> {
    pub(crate) fn new(host: H) -> Self {
        Self { host }
    }
}

impl<H> LuaWriteBack for LuaWriteBackService<H>
where
    H: TrustedLuaExecutionHost,
{
    type Error = LuaWriteBackServiceError<H::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        published: &PublishedWriteBack,
        script_path: PathBuf,
    ) -> Result<(), Self::Error> {
        if !published.belongs_to(project) {
            return Err(LuaWriteBackServiceError::PublishedProjectMismatch {
                project_root: project.workspace_root().to_path_buf(),
                published_root: published.workspace_root().to_path_buf(),
            });
        }

        let error_path = script_path.clone();
        let output_root = published.output_root().to_path_buf();
        let invocation = LuaInvocation::write_back(
            script_path,
            LuaProjectContext::for_published_write_back(project, output_root.clone()),
        );
        self.host.execute(invocation).await.map_err(|source| {
            LuaWriteBackServiceError::ExecuteHost {
                script_path: error_path,
                output_root,
                source,
            }
        })
    }
}

/// Lua WriteBack 在项目交接或 Host 执行边界遇到的失败。
#[derive(Debug)]
pub(crate) enum LuaWriteBackServiceError<E> {
    PublishedProjectMismatch {
        project_root: PathBuf,
        published_root: PathBuf,
    },
    ExecuteHost {
        script_path: PathBuf,
        output_root: PathBuf,
        source: E,
    },
}

impl<E> fmt::Display for LuaWriteBackServiceError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublishedProjectMismatch {
                project_root,
                published_root,
            } => write!(
                formatter,
                "已发布写回输出不属于当前项目（当前：{}，发布：{}）",
                project_root.display(),
                published_root.display()
            ),
            Self::ExecuteHost {
                script_path,
                output_root,
                source,
            } => write!(
                formatter,
                "执行可信 Lua 写回 Host 失败（脚本：{}，输出：{}）：{source}",
                script_path.display(),
                output_root.display()
            ),
        }
    }
}

impl<E> Error for LuaWriteBackServiceError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PublishedProjectMismatch { .. } => None,
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
    use crate::att_mz::lua::LuaPhase;

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
            let LuaInvocation::WriteBack {
                script_path,
                project,
            } = invocation
            else {
                panic!("Lua 写回服务只应提交 WriteBack 调用")
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(RecordedInvocation {
                phase: LuaPhase::WriteBack,
                script_path,
                project,
            });
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

    fn project(name: &str) -> OpenedProject {
        OpenedProject::new(
            name.parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects").join(name),
            PathBuf::from("C:/projects").join(name).join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    #[tokio::test]
    async fn passes_write_back_phase_and_only_this_phase_receives_output_root() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaWriteBackService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
        });
        let project = project("alice");
        let published = PublishedWriteBack::new(&project);

        service
            .run(&project, &published, PathBuf::from("scripts/write.lua"))
            .await
            .expect("Lua 写回应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::WriteBack);
        assert_eq!(invocation.script_path, PathBuf::from("scripts/write.lua"));
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.output_root(),
            Some(Path::new("C:/projects/alice/write_back"))
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
    }

    #[tokio::test]
    async fn rejects_a_published_token_from_another_project_before_host() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaWriteBackService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
        });
        let current_project = project("alice");
        let other = project("bob");

        let error = service
            .run(
                &current_project,
                &PublishedWriteBack::new(&other),
                PathBuf::from("write.lua"),
            )
            .await
            .expect_err("跨项目 Published token 必须拒绝");

        assert!(matches!(
            error,
            LuaWriteBackServiceError::PublishedProjectMismatch { .. }
        ));
        assert!(recorded.lock().expect("调用记录锁不应中毒").is_none());
    }

    #[tokio::test]
    async fn preserves_script_output_and_host_source() {
        let service = LuaWriteBackService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
        });
        let project = project("alice");
        let error = service
            .run(
                &project,
                &PublishedWriteBack::new(&project),
                PathBuf::from("broken write.lua"),
            )
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaWriteBackServiceError::ExecuteHost {
                script_path,
                output_root,
                source: FakeError,
            } if script_path == &PathBuf::from("broken write.lua")
                && output_root == &PathBuf::from("C:/projects/alice/write_back")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError)
        );
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaWriteBackService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
        });
        let project = project("alice");
        let published = PublishedWriteBack::new(&project);
        assert_send(service.run(&project, &published, PathBuf::from("write.lua")));
    }
}
