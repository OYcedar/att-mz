use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::att_mz::lua::{LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost};
use crate::project_database::StoredProjectRecord;

/// 使用可信 Lua 程序翻译其自有数据的职责契约。
///
/// Lua 翻译完整拥有自己的数据协议、事务划分、重试和幂等语义。标准翻译和顶层
/// 翻译用例不解释 Lua 产物，也不回滚 Lua 或前序标准翻译已经提交的副作用。
pub(crate) trait LuaTranslation: Send + Sync {
    /// 与配置解析器产物一致的执行配置。
    type Profile: Send + Sync + 'static;
    /// Lua 翻译失败。
    type Error: Error + Send + Sync + 'static;

    /// 使用本次执行配置运行调用方明确指定的可信 Lua 程序。
    fn run(
        &self,
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把 Translate 阶段已经建立的项目事实和 Profile 交给可信 Lua Host。
pub(crate) struct LuaTranslationService<H> {
    host: H,
}

impl<H> LuaTranslationService<H> {
    pub(crate) fn new(host: H) -> Self {
        Self { host }
    }
}

impl<H> LuaTranslation for LuaTranslationService<H>
where
    H: TrustedLuaExecutionHost,
{
    type Profile = Arc<H::TranslationProfile>;
    type Error = LuaTranslationError<H::Error>;

    async fn run(
        &self,
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        script_path: PathBuf,
    ) -> Result<(), Self::Error> {
        let error_path = script_path.clone();
        let invocation = LuaInvocation::translate(
            script_path,
            LuaProjectContext::from_stored_record(project),
            Arc::clone(profile),
        );

        self.host
            .execute(invocation)
            .await
            .map_err(|source| LuaTranslationError::ExecuteHost {
                script_path: error_path,
                source,
            })
    }
}

/// Lua Translate 阶段的 Host 执行失败。
#[derive(Debug)]
pub(crate) enum LuaTranslationError<E> {
    ExecuteHost { script_path: PathBuf, source: E },
}

impl<E> fmt::Display for LuaTranslationError<E>
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
                "执行可信 Lua 翻译 Host 失败 {}：{source}",
                script_path.display()
            ),
        }
    }
}

impl<E> Error for LuaTranslationError<E>
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
    use crate::att_mz::lua::LuaPhase;

    #[derive(Debug)]
    struct FakeProfile {
        name: &'static str,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedInvocation {
        phase: LuaPhase,
        script_path: PathBuf,
        project: LuaProjectContext,
        profile_address: usize,
        profile_name: &'static str,
    }

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<RecordedInvocation>>>,
        fail: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationProfile = FakeProfile;
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationProfile>,
        ) -> Result<(), Self::Error> {
            let recorded = match invocation {
                LuaInvocation::Translate {
                    script_path,
                    project,
                    profile,
                } => RecordedInvocation {
                    phase: LuaPhase::Translate,
                    script_path,
                    project,
                    profile_address: Arc::as_ptr(&profile).addr(),
                    profile_name: profile.name,
                },
                LuaInvocation::Extract { .. } => panic!("翻译服务不应提交 Extract 调用"),
                LuaInvocation::WriteBack { .. } => {
                    panic!("翻译服务不应提交 WriteBack 调用")
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

    fn project() -> StoredProjectRecord {
        StoredProjectRecord::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    #[tokio::test]
    async fn passes_complete_translate_context_and_the_same_profile_to_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
        });
        let profile = Arc::new(FakeProfile { name: "quality" });
        let profile_address = Arc::as_ptr(&profile).addr();

        service
            .run(&project(), &profile, PathBuf::from("scripts/translate.lua"))
            .await
            .expect("Lua 翻译应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应该收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::Translate);
        assert_eq!(
            invocation.script_path,
            PathBuf::from("scripts/translate.lua")
        );
        assert_eq!(invocation.profile_address, profile_address);
        assert_eq!(invocation.profile_name, "quality");
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
        assert_eq!(invocation.project.target_language(), "zh-Hans");
    }

    #[tokio::test]
    async fn preserves_script_path_and_host_source() {
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
        });

        let error = service
            .run(
                &project(),
                &Arc::new(FakeProfile { name: "quality" }),
                PathBuf::from("broken translation.lua"),
            )
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaTranslationError::ExecuteHost {
                script_path,
                source: FakeError
            } if script_path == &PathBuf::from("broken translation.lua")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError)
        );
        assert!(error.to_string().contains("broken translation.lua"));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
        });
        let project = project();
        let profile = Arc::new(FakeProfile { name: "quality" });
        assert_send(service.run(&project, &profile, PathBuf::from("translate.lua")));
    }
}
