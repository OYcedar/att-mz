use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::builtin::BuiltInExtraction;
use super::lua::LuaExtraction;
use super::rules::RulesExtraction;
use super::{ExtractInput, ExtractOutput, SelectedRules};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::rpg_maker::SelectedLua;
use crate::rpg_maker::project::ExistingProjectOpener;
use crate::rpg_maker::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};

/// 按固定业务顺序编排一次 RPG Maker 文本提取。
///
/// 用例只打开一次项目，随后按 Builtin、Rules、Lua 执行被选择的阶段。首个失败会
/// 阻止后续阶段，已经成功提交的前序阶段不由本层做组合回滚。
pub(crate) struct ExtractService<O, B, R, L, P> {
    project_opener: O,
    built_in_extraction: Option<B>,
    selected_rules: Option<SelectedRules<R>>,
    selected_lua: Option<SelectedLua<L>>,
    project_lease: P,
    cancellation: CooperativeCancellation,
}

impl<O, B, R, L, P> ExtractService<O, B, R, L, P> {
    pub(crate) fn new(
        project_opener: O,
        built_in_extraction: Option<B>,
        selected_rules: Option<SelectedRules<R>>,
        selected_lua: Option<SelectedLua<L>>,
        project_lease: P,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            built_in_extraction,
            selected_rules,
            selected_lua,
            project_lease,
            cancellation,
        }
    }
}

impl<O, B, R, L, P> ExtractService<O, B, R, L, P>
where
    O: ExistingProjectOpener,
    B: BuiltInExtraction,
    R: RulesExtraction,
    L: LuaExtraction,
    P: ProjectCommandLeaseProvider,
{
    pub(crate) async fn execute(
        &self,
        input: ExtractInput,
    ) -> Result<
        OperationCompletion<ExtractOutput>,
        ExtractServiceError<O::Error, B::Error, R::Error, L::Error, P::Error>,
    > {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let ExtractInput { name } = input;
        let _lease = self
            .project_lease
            .acquire(&name)
            .await
            .map_err(ExtractServiceError::ProjectLease)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let project = self
            .project_opener
            .open(&name)
            .await
            .map_err(ExtractServiceError::OpenProject)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        if let Some(built_in_extraction) = &self.built_in_extraction {
            built_in_extraction
                .refresh(&project)
                .await
                .map_err(ExtractServiceError::BuiltIn)?;
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
        }

        if let Some(selected_rules) = &self.selected_rules {
            let error_path = selected_rules.rules_path().to_path_buf();
            selected_rules
                .executor()
                .replace(&project, error_path.clone())
                .await
                .map_err(|source| ExtractServiceError::Rules {
                    rules_path: error_path,
                    source,
                })?;
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
        }

        if let Some(selected_lua) = &self.selected_lua {
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
            let error_path = selected_lua.script_path().to_path_buf();
            let completion = selected_lua
                .executor()
                .run(&project, error_path.clone())
                .await
                .map_err(|source| ExtractServiceError::Lua {
                    script_path: error_path,
                    source,
                })?;
            let OperationCompletion::Completed(()) = completion else {
                return Ok(OperationCompletion::Cancelled);
            };
        }

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        Ok(OperationCompletion::Completed(ExtractOutput { name }))
    }
}

/// 提取用例在四个直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum ExtractServiceError<OE, BE, RE, LE, PE> {
    ProjectLease(ProjectCommandLeaseError<PE>),
    OpenProject(OE),
    BuiltIn(BE),
    Rules { rules_path: PathBuf, source: RE },
    Lua { script_path: PathBuf, source: LE },
}

impl<OE, BE, RE, LE, PE> fmt::Display for ExtractServiceError<OE, BE, RE, LE, PE>
where
    OE: Error,
    BE: Error,
    RE: Error,
    LE: Error,
    PE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectLease(error) => error.fmt(formatter),
            Self::OpenProject(source) => write!(formatter, "打开项目失败：{source}"),
            Self::BuiltIn(source) => write!(formatter, "内置提取失败：{source}"),
            Self::Rules { rules_path, source } => {
                write!(formatter, "规则提取失败 {}：{source}", rules_path.display())
            }
            Self::Lua {
                script_path,
                source,
            } => write!(
                formatter,
                "Lua 提取失败 {}：{source}",
                script_path.display()
            ),
        }
    }
}

impl<OE, BE, RE, LE, PE> Error for ExtractServiceError<OE, BE, RE, LE, PE>
where
    OE: Error + 'static,
    BE: Error + 'static,
    RE: Error + 'static,
    LE: Error + 'static,
    PE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::OpenProject(source) => Some(source),
            Self::BuiltIn(source) => Some(source),
            Self::Rules { source, .. } => Some(source),
            Self::Lua { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::project::OpenedProject;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Lease,
        Open,
        BuiltIn,
        Rules(PathBuf),
        Lua(PathBuf),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeLeaseProvider {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl ProjectCommandLeaseProvider for FakeLeaseProvider {
        type Error = FakeError;
        type LeaseState = ();

        async fn acquire(
            &self,
            _: &ProjectName,
        ) -> Result<
            crate::rpg_maker::project_lease::ProjectCommandLease<Self::LeaseState>,
            ProjectCommandLeaseError<Self::Error>,
        > {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lease);
            if self.fail {
                Err(ProjectCommandLeaseError::Unavailable {
                    project: project_name(),
                    source: FakeError("lease"),
                })
            } else {
                Ok(crate::rpg_maker::project_lease::ProjectCommandLease::for_test(()))
            }
        }
    }

    #[derive(Clone)]
    struct FakeOpener {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
        cancel: Option<CooperativeCancellation>,
    }

    impl ExistingProjectOpener for FakeOpener {
        type Error = FakeError;

        async fn open(&self, _: &ProjectName) -> Result<OpenedProject, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Open);
            if let Some(cancellation) = &self.cancel {
                cancellation.request();
            }
            if self.fail {
                return Err(FakeError("open"));
            }
            Ok(opened_project())
        }
    }

    #[derive(Clone)]
    struct FakeBuiltIn {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl BuiltInExtraction for FakeBuiltIn {
        type Error = FakeError;

        async fn refresh(&self, _: &OpenedProject) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::BuiltIn);
            if self.fail {
                Err(FakeError("builtin"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeRules {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl RulesExtraction for FakeRules {
        type Error = FakeError;

        async fn replace(&self, _: &OpenedProject, path: PathBuf) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Rules(path));
            if self.fail {
                Err(FakeError("rules"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeLua {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
        cancelled: bool,
    }

    impl LuaExtraction for FakeLua {
        type Error = FakeError;

        async fn run(
            &self,
            _: &OpenedProject,
            path: PathBuf,
        ) -> Result<OperationCompletion<()>, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lua(path));
            if self.fail {
                Err(FakeError("lua"))
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else {
                Ok(OperationCompletion::Completed(()))
            }
        }
    }

    type Service = ExtractService<FakeOpener, FakeBuiltIn, FakeRules, FakeLua, FakeLeaseProvider>;

    fn service(
        events: Arc<Mutex<Vec<Event>>>,
        failing_stage: Option<&str>,
        builtin: bool,
        rules_path: Option<&str>,
        lua_script: Option<&str>,
    ) -> Service {
        ExtractService::new(
            FakeOpener {
                events: Arc::clone(&events),
                fail: failing_stage == Some("open"),
                cancel: None,
            },
            builtin.then(|| FakeBuiltIn {
                events: Arc::clone(&events),
                fail: failing_stage == Some("builtin"),
            }),
            rules_path.map(|path| {
                SelectedRules::new(
                    PathBuf::from(path),
                    FakeRules {
                        events: Arc::clone(&events),
                        fail: failing_stage == Some("rules"),
                    },
                )
            }),
            lua_script.map(|path| {
                SelectedLua::new(
                    PathBuf::from(path),
                    FakeLua {
                        events: Arc::clone(&events),
                        fail: failing_stage == Some("lua"),
                        cancelled: failing_stage == Some("cancel-lua"),
                    },
                )
            }),
            FakeLeaseProvider {
                events,
                fail: failing_stage == Some("lease"),
            },
            CooperativeCancellation::default(),
        )
    }

    fn opened_project() -> OpenedProject {
        OpenedProject::new(
            project_name(),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-CN".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("项目名应合法")
    }

    #[tokio::test]
    async fn cancellation_after_open_prevents_every_extraction_stage() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CooperativeCancellation::default();
        let service = ExtractService::new(
            FakeOpener {
                events: Arc::clone(&events),
                fail: false,
                cancel: Some(cancellation.clone()),
            },
            Some(FakeBuiltIn {
                events: Arc::clone(&events),
                fail: false,
            }),
            Some(SelectedRules::new(
                PathBuf::from("rules.toml"),
                FakeRules {
                    events: Arc::clone(&events),
                    fail: false,
                },
            )),
            Some(SelectedLua::new(
                PathBuf::from("translate.lua"),
                FakeLua {
                    events: Arc::clone(&events),
                    fail: false,
                    cancelled: false,
                },
            )),
            FakeLeaseProvider {
                events: Arc::clone(&events),
                fail: false,
            },
            cancellation,
        );

        let completion = service.execute(input()).await.expect("取消是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(
            *events.lock().expect("事件锁不应中毒"),
            [Event::Lease, Event::Open]
        );
    }

    fn input() -> ExtractInput {
        ExtractInput {
            name: project_name(),
        }
    }

    fn events(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().expect("事件锁不应中毒").clone()
    }

    #[tokio::test]
    async fn always_dispatches_selected_stages_in_business_order() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(
            Arc::clone(&recorded),
            None,
            true,
            Some("rules.toml"),
            Some("extract.lua"),
        );

        let output = service.execute(input()).await.expect("组合提取应该成功");

        assert_eq!(
            output,
            OperationCompletion::Completed(ExtractOutput {
                name: project_name()
            })
        );
        assert_eq!(
            events(&recorded),
            vec![
                Event::Lease,
                Event::Open,
                Event::BuiltIn,
                Event::Rules(PathBuf::from("rules.toml")),
                Event::Lua(PathBuf::from("extract.lua")),
            ]
        );
    }

    #[tokio::test]
    async fn each_stage_can_be_selected_on_its_own() {
        let cases = [
            (true, None, None, Event::BuiltIn),
            (
                false,
                Some("rules.toml"),
                None,
                Event::Rules(PathBuf::from("rules.toml")),
            ),
            (
                false,
                None,
                Some("extract.lua"),
                Event::Lua(PathBuf::from("extract.lua")),
            ),
        ];

        for (builtin, rules, lua, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            service(Arc::clone(&recorded), None, builtin, rules, lua)
                .execute(input())
                .await
                .expect("单阶段提取应该成功");
            assert_eq!(events(&recorded), vec![Event::Lease, Event::Open, expected]);
        }
    }

    #[tokio::test]
    async fn lua_cancellation_is_the_normal_top_level_completion() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let completion = service(
            Arc::clone(&recorded),
            Some("cancel-lua"),
            false,
            None,
            Some("extract.lua"),
        )
        .execute(input())
        .await
        .expect("Lua 取消应作为正常结果传播");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(
            events(&recorded),
            vec![
                Event::Lease,
                Event::Open,
                Event::Lua(PathBuf::from("extract.lua")),
            ]
        );
    }

    #[tokio::test]
    async fn the_first_failure_stops_all_later_stages() {
        let cases = [
            ("lease", vec![Event::Lease]),
            ("open", vec![Event::Lease, Event::Open]),
            ("builtin", vec![Event::Lease, Event::Open, Event::BuiltIn]),
            (
                "rules",
                vec![
                    Event::Lease,
                    Event::Open,
                    Event::BuiltIn,
                    Event::Rules(PathBuf::from("rules.toml")),
                ],
            ),
            (
                "lua",
                vec![
                    Event::Lease,
                    Event::Open,
                    Event::BuiltIn,
                    Event::Rules(PathBuf::from("rules.toml")),
                    Event::Lua(PathBuf::from("extract.lua")),
                ],
            ),
        ];

        for (stage, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            service(
                Arc::clone(&recorded),
                Some(stage),
                true,
                Some("rules.toml"),
                Some("extract.lua"),
            )
            .execute(input())
            .await
            .expect_err("指定阶段应该失败");
            assert_eq!(events(&recorded), expected);
        }
    }

    #[tokio::test]
    async fn rules_and_lua_errors_keep_their_file_paths_and_sources() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rules_error = service(
            Arc::clone(&recorded),
            Some("rules"),
            false,
            Some("custom/rules.toml"),
            None,
        )
        .execute(input())
        .await
        .expect_err("Rules 应该失败");
        assert!(matches!(
            &rules_error,
            ExtractServiceError::Rules {
                rules_path,
                source: FakeError("rules")
            } if rules_path == &PathBuf::from("custom/rules.toml")
        ));
        assert_eq!(
            rules_error
                .source()
                .and_then(|source| source.downcast_ref()),
            Some(&FakeError("rules"))
        );

        let lua_error = service(
            Arc::new(Mutex::new(Vec::new())),
            Some("lua"),
            false,
            None,
            Some("scripts/extract.lua"),
        )
        .execute(input())
        .await
        .expect_err("Lua 应该失败");
        assert!(matches!(
            &lua_error,
            ExtractServiceError::Lua {
                script_path,
                source: FakeError("lua")
            } if script_path == &PathBuf::from("scripts/extract.lua")
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = service(Arc::new(Mutex::new(Vec::new())), None, true, None, None);
        assert_send(service.execute(input()));
    }
}
