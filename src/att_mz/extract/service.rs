use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::builtin::BuiltInExtraction;
use super::lua::LuaExtraction;
use super::rules::RulesExtraction;
use super::{ExtractInput, ExtractOutput, ExtractUseCase};
use crate::att_mz::project::ExistingProjectOpener;
use crate::execution::{CooperativeCancellation, OperationCancelled};

/// 按固定业务顺序编排一次 MZ 文本提取。
///
/// 用例只打开一次项目，随后按 Builtin、Rules、Lua 执行被选择的阶段。首个失败会
/// 阻止后续阶段，已经成功提交的前序阶段不由本层做组合回滚。
pub(crate) struct ExtractService<O, B, R, L> {
    project_opener: O,
    built_in_extraction: B,
    rules_extraction: R,
    lua_extraction: Option<L>,
    cancellation: CooperativeCancellation,
}

impl<O, B, R, L> ExtractService<O, B, R, L> {
    pub(crate) fn new(
        project_opener: O,
        built_in_extraction: B,
        rules_extraction: R,
        lua_extraction: Option<L>,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            built_in_extraction,
            rules_extraction,
            lua_extraction,
            cancellation,
        }
    }
}

impl<O, B, R, L> ExtractUseCase for ExtractService<O, B, R, L>
where
    O: ExistingProjectOpener,
    B: BuiltInExtraction,
    R: RulesExtraction,
    L: LuaExtraction,
{
    type Error = ExtractServiceError<O::Error, B::Error, R::Error, L::Error>;

    async fn execute(&self, input: ExtractInput) -> Result<ExtractOutput, Self::Error> {
        self.cancellation
            .check()
            .map_err(ExtractServiceError::Cancelled)?;
        let ExtractInput { name, selection } = input;
        let (builtin, rules_path, lua_script) = selection.into_parts();
        let project = self
            .project_opener
            .open(&name)
            .await
            .map_err(ExtractServiceError::OpenProject)?;
        self.cancellation
            .check()
            .map_err(ExtractServiceError::Cancelled)?;

        if builtin {
            self.built_in_extraction
                .refresh(&project)
                .await
                .map_err(ExtractServiceError::BuiltIn)?;
            self.cancellation
                .check()
                .map_err(ExtractServiceError::Cancelled)?;
        }

        if let Some(rules_path) = rules_path {
            let error_path = rules_path.clone();
            self.rules_extraction
                .replace(&project, rules_path)
                .await
                .map_err(|source| ExtractServiceError::Rules {
                    rules_path: error_path,
                    source,
                })?;
            self.cancellation
                .check()
                .map_err(ExtractServiceError::Cancelled)?;
        }

        if let Some(script_path) = lua_script {
            self.cancellation
                .check()
                .map_err(ExtractServiceError::Cancelled)?;
            let error_path = script_path.clone();
            self.lua_extraction
                .as_ref()
                .ok_or(ExtractServiceError::MissingLuaDependency)?
                .run(&project, script_path)
                .await
                .map_err(|source| ExtractServiceError::Lua {
                    script_path: error_path,
                    source,
                })?;
        }

        self.cancellation
            .check()
            .map_err(ExtractServiceError::Cancelled)?;

        Ok(ExtractOutput { name })
    }
}

/// 提取用例在四个直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum ExtractServiceError<OE, BE, RE, LE> {
    Cancelled(OperationCancelled),
    OpenProject(OE),
    BuiltIn(BE),
    Rules { rules_path: PathBuf, source: RE },
    MissingLuaDependency,
    Lua { script_path: PathBuf, source: LE },
}

impl<OE, BE, RE, LE> fmt::Display for ExtractServiceError<OE, BE, RE, LE>
where
    OE: Error,
    BE: Error,
    RE: Error,
    LE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::OpenProject(source) => write!(formatter, "打开项目失败：{source}"),
            Self::BuiltIn(source) => write!(formatter, "内置提取失败：{source}"),
            Self::Rules { rules_path, source } => {
                write!(formatter, "规则提取失败 {}：{source}", rules_path.display())
            }
            Self::MissingLuaDependency => {
                formatter.write_str("本次选择了 Lua 提取，但未构造 Lua Runtime")
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

impl<OE, BE, RE, LE> Error for ExtractServiceError<OE, BE, RE, LE>
where
    OE: Error + 'static,
    BE: Error + 'static,
    RE: Error + 'static,
    LE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::OpenProject(source) => Some(source),
            Self::BuiltIn(source) => Some(source),
            Self::Rules { source, .. } => Some(source),
            Self::MissingLuaDependency => None,
            Self::Lua { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::extract::ExtractionSelection;
    use crate::att_mz::project::OpenedProject;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
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
    }

    impl LuaExtraction for FakeLua {
        type Error = FakeError;

        async fn run(&self, _: &OpenedProject, path: PathBuf) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lua(path));
            if self.fail {
                Err(FakeError("lua"))
            } else {
                Ok(())
            }
        }
    }

    type Service = ExtractService<FakeOpener, FakeBuiltIn, FakeRules, FakeLua>;

    fn service(events: Arc<Mutex<Vec<Event>>>, failing_stage: Option<&str>) -> Service {
        ExtractService::new(
            FakeOpener {
                events: Arc::clone(&events),
                fail: failing_stage == Some("open"),
                cancel: None,
            },
            FakeBuiltIn {
                events: Arc::clone(&events),
                fail: failing_stage == Some("builtin"),
            },
            FakeRules {
                events: Arc::clone(&events),
                fail: failing_stage == Some("rules"),
            },
            Some(FakeLua {
                events,
                fail: failing_stage == Some("lua"),
            }),
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
            crate::att_mz::project::test_layout_profile(),
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
            FakeBuiltIn {
                events: Arc::clone(&events),
                fail: false,
            },
            FakeRules {
                events: Arc::clone(&events),
                fail: false,
            },
            Some(FakeLua {
                events: Arc::clone(&events),
                fail: false,
            }),
            cancellation,
        );

        let error = service
            .execute(input(true, Some("rules.json"), Some("translate.lua")))
            .await
            .expect_err("取消后不得启动提取阶段");

        assert!(matches!(error, ExtractServiceError::Cancelled(_)));
        assert_eq!(*events.lock().expect("事件锁不应中毒"), [Event::Open]);
    }

    fn input(builtin: bool, rules: Option<&str>, lua: Option<&str>) -> ExtractInput {
        ExtractInput {
            name: project_name(),
            selection: ExtractionSelection::new(
                builtin,
                rules.map(PathBuf::from),
                lua.map(PathBuf::from),
            )
            .expect("测试选择不应为空"),
        }
    }

    fn events(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().expect("事件锁不应中毒").clone()
    }

    #[tokio::test]
    async fn always_dispatches_selected_stages_in_business_order() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(Arc::clone(&recorded), None);

        let output = service
            .execute(input(true, Some("rules.json"), Some("extract.lua")))
            .await
            .expect("组合提取应该成功");

        assert_eq!(output.name, project_name());
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open,
                Event::BuiltIn,
                Event::Rules(PathBuf::from("rules.json")),
                Event::Lua(PathBuf::from("extract.lua")),
            ]
        );
    }

    #[tokio::test]
    async fn each_stage_can_be_selected_on_its_own() {
        let cases = [
            (input(true, None, None), Event::BuiltIn),
            (
                input(false, Some("rules.json"), None),
                Event::Rules(PathBuf::from("rules.json")),
            ),
            (
                input(false, None, Some("extract.lua")),
                Event::Lua(PathBuf::from("extract.lua")),
            ),
        ];

        for (input, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            service(Arc::clone(&recorded), None)
                .execute(input)
                .await
                .expect("单阶段提取应该成功");
            assert_eq!(events(&recorded), vec![Event::Open, expected]);
        }
    }

    #[tokio::test]
    async fn the_first_failure_stops_all_later_stages() {
        let cases = [
            ("open", vec![Event::Open]),
            ("builtin", vec![Event::Open, Event::BuiltIn]),
            (
                "rules",
                vec![
                    Event::Open,
                    Event::BuiltIn,
                    Event::Rules(PathBuf::from("rules.json")),
                ],
            ),
            (
                "lua",
                vec![
                    Event::Open,
                    Event::BuiltIn,
                    Event::Rules(PathBuf::from("rules.json")),
                    Event::Lua(PathBuf::from("extract.lua")),
                ],
            ),
        ];

        for (stage, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            service(Arc::clone(&recorded), Some(stage))
                .execute(input(true, Some("rules.json"), Some("extract.lua")))
                .await
                .expect_err("指定阶段应该失败");
            assert_eq!(events(&recorded), expected);
        }
    }

    #[tokio::test]
    async fn rules_and_lua_errors_keep_their_file_paths_and_sources() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rules_error = service(Arc::clone(&recorded), Some("rules"))
            .execute(input(false, Some("custom/rules.json"), None))
            .await
            .expect_err("Rules 应该失败");
        assert!(matches!(
            &rules_error,
            ExtractServiceError::Rules {
                rules_path,
                source: FakeError("rules")
            } if rules_path == &PathBuf::from("custom/rules.json")
        ));
        assert_eq!(
            rules_error
                .source()
                .and_then(|source| source.downcast_ref()),
            Some(&FakeError("rules"))
        );

        let lua_error = service(Arc::new(Mutex::new(Vec::new())), Some("lua"))
            .execute(input(false, None, Some("scripts/extract.lua")))
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

        let service = service(Arc::new(Mutex::new(Vec::new())), None);
        assert_send(service.execute(input(true, None, None)));
    }
}
