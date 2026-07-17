use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::lua::LuaTranslation;
use super::profile::TranslationExecutionProfileResolver;
use super::standard::{StandardTranslation, StandardTranslationInput};
use super::{StandardTranslationSummary, TranslateInput, TranslateOutput, TranslateUseCase};
use crate::att_mz::ProjectName;
use crate::execution::{CooperativeCancellation, OperationCancelled};
use crate::project_database::ProjectDatabaseRecordReader;

/// 按固定业务顺序编排一次 MZ 翻译。
///
/// 用例先选择调用方明确指定的执行配置，再读取一次项目记录，随后执行标准翻译，
/// 最后按需执行可信 Lua 翻译。首个失败会阻止后续阶段；本层不回滚依赖已经提交
/// 的副作用。
pub(crate) struct TranslateService<C, R, S, L> {
    profile_resolver: C,
    project_reader: R,
    standard_translation: S,
    lua_translation: Option<L>,
    cancellation: CooperativeCancellation,
}

impl<C, R, S, L> TranslateService<C, R, S, L> {
    pub(crate) fn new(
        profile_resolver: C,
        project_reader: R,
        standard_translation: S,
        lua_translation: Option<L>,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            profile_resolver,
            project_reader,
            standard_translation,
            lua_translation,
            cancellation,
        }
    }
}

impl<C, R, S, L> TranslateUseCase for TranslateService<C, R, S, L>
where
    C: TranslationExecutionProfileResolver,
    R: ProjectDatabaseRecordReader,
    S: StandardTranslation<Profile = C::Profile>,
    L: LuaTranslation<Profile = C::Profile>,
{
    type Error = TranslateServiceError<C::Error, R::Error, S::Error, L::Error>;

    async fn execute(&self, input: TranslateInput) -> Result<TranslateOutput, Self::Error> {
        self.cancellation
            .check()
            .map_err(TranslateServiceError::Cancelled)?;
        let TranslateInput {
            name,
            profile_id,
            terminology_path,
            placeholder_rules_path,
            lua_script,
        } = input;

        let profile = self
            .profile_resolver
            .resolve(&profile_id)
            .map_err(|source| TranslateServiceError::ResolveProfile {
                profile_id: profile_id.clone(),
                source,
            })?;
        self.cancellation
            .check()
            .map_err(TranslateServiceError::Cancelled)?;
        let project = self.project_reader.read(&name).await.map_err(|source| {
            TranslateServiceError::ReadProject {
                name: name.clone(),
                source,
            }
        })?;
        self.cancellation
            .check()
            .map_err(TranslateServiceError::Cancelled)?;

        let standard_report = self
            .standard_translation
            .run(
                &project,
                &profile,
                StandardTranslationInput::new(terminology_path, placeholder_rules_path),
            )
            .await
            .map_err(|source| TranslateServiceError::Standard { source })?;
        self.cancellation
            .check()
            .map_err(TranslateServiceError::Cancelled)?;

        let lua_executed = if let Some(script_path) = lua_script {
            let error_path = script_path.clone();
            self.lua_translation
                .as_ref()
                .ok_or(TranslateServiceError::MissingLuaDependency)?
                .run(&project, &profile, script_path)
                .await
                .map_err(|source| TranslateServiceError::Lua {
                    script_path: error_path,
                    source,
                })?;
            true
        } else {
            false
        };

        Ok(TranslateOutput {
            name,
            profile_id,
            standard: StandardTranslationSummary {
                total_tasks: standard_report.total_tasks(),
                complete_tasks: standard_report.complete_tasks(),
                partial_tasks: standard_report.partial_tasks(),
                unavailable_tasks: standard_report.unavailable_tasks(),
                accepted_decisions: standard_report.accepted_decisions(),
                written_locations: standard_report.written_locations(),
                remaining_decisions: standard_report.unresolved_decisions(),
                remaining_locations: standard_report.unresolved_locations(),
                protocol_diagnostics: standard_report.protocol_diagnostics(),
                recoverable_request_exhaustions: standard_report.recoverable_request_exhaustions(),
            },
            lua_executed,
        })
    }
}

/// 翻译用例在四个直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum TranslateServiceError<CE, RE, SE, LE> {
    Cancelled(OperationCancelled),
    ResolveProfile { profile_id: String, source: CE },
    ReadProject { name: ProjectName, source: RE },
    Standard { source: SE },
    MissingLuaDependency,
    Lua { script_path: PathBuf, source: LE },
}

impl<CE, RE, SE, LE> fmt::Display for TranslateServiceError<CE, RE, SE, LE>
where
    CE: Error,
    RE: Error,
    SE: Error,
    LE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::ResolveProfile { profile_id, source } => {
                write!(formatter, "无法选择翻译 Profile {profile_id}：{source}")
            }
            Self::ReadProject { name, source } => {
                write!(formatter, "无法读取项目 {name}：{source}")
            }
            Self::Standard { source } => write!(formatter, "标准翻译失败：{source}"),
            Self::MissingLuaDependency => {
                formatter.write_str("本次选择了 Lua 翻译，但未构造 Lua Runtime")
            }
            Self::Lua {
                script_path,
                source,
            } => write!(
                formatter,
                "Lua 翻译失败 {}：{source}",
                script_path.display()
            ),
        }
    }
}

impl<CE, RE, SE, LE> Error for TranslateServiceError<CE, RE, SE, LE>
where
    CE: Error + 'static,
    RE: Error + 'static,
    SE: Error + 'static,
    LE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::ResolveProfile { source, .. } => Some(source),
            Self::ReadProject { source, .. } => Some(source),
            Self::Standard { source } => Some(source),
            Self::MissingLuaDependency => None,
            Self::Lua { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };
    use crate::att_mz::translate::executor::FinalLlmResponseMetadata;
    use crate::att_mz::translate::standard::{
        StandardTranslationRunReport, StandardTranslationTaskIndex, TranslationLeafIdentity,
        TranslationTaskOutcome, TranslationTaskUnavailableReason, TranslationUnitRejectionReason,
        UnresolvedTranslationUnit,
    };
    use crate::project_database::StoredProjectRecord;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Failure {
        Resolve,
        Read,
        Standard,
        Lua,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeProfile {
        profile_id: String,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Resolve(String),
        Read(ProjectName),
        Standard {
            project: StoredProjectRecord,
            profile: FakeProfile,
            input: StandardTranslationInput,
        },
        Lua {
            project: StoredProjectRecord,
            profile: FakeProfile,
            script_path: PathBuf,
        },
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
    struct FakeProfileResolver {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
    }

    impl TranslationExecutionProfileResolver for FakeProfileResolver {
        type Profile = FakeProfile;
        type Error = FakeError;

        fn resolve(&self, profile_id: &str) -> Result<Self::Profile, Self::Error> {
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Resolve(profile_id.to_owned()));

            if self.failure == Some(Failure::Resolve) {
                Err(FakeError("resolve"))
            } else {
                Ok(FakeProfile {
                    profile_id: profile_id.to_owned(),
                })
            }
        }
    }

    #[derive(Clone)]
    struct FakeProjectReader {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
    }

    impl ProjectDatabaseRecordReader for FakeProjectReader {
        type Error = FakeError;

        async fn read(&self, name: &ProjectName) -> Result<StoredProjectRecord, Self::Error> {
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Read(name.clone()));

            if self.failure == Some(Failure::Read) {
                Err(FakeError("read"))
            } else {
                Ok(project_record())
            }
        }
    }

    #[derive(Clone)]
    struct FakeStandardTranslation {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
    }

    impl StandardTranslation for FakeStandardTranslation {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn run(
            &self,
            project: &StoredProjectRecord,
            profile: &Self::Profile,
            input: StandardTranslationInput,
        ) -> Result<StandardTranslationRunReport, Self::Error> {
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Standard {
                    project: project.clone(),
                    profile: profile.clone(),
                    input,
                });

            if self.failure == Some(Failure::Standard) {
                Err(FakeError("standard"))
            } else {
                Ok(unavailable_report())
            }
        }
    }

    #[derive(Clone)]
    struct FakeLuaTranslation {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
    }

    impl LuaTranslation for FakeLuaTranslation {
        type Profile = FakeProfile;
        type Error = FakeError;

        async fn run(
            &self,
            project: &StoredProjectRecord,
            profile: &Self::Profile,
            script_path: PathBuf,
        ) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Lua {
                    project: project.clone(),
                    profile: profile.clone(),
                    script_path,
                });

            if self.failure == Some(Failure::Lua) {
                Err(FakeError("lua"))
            } else {
                Ok(())
            }
        }
    }

    type Service = TranslateService<
        FakeProfileResolver,
        FakeProjectReader,
        FakeStandardTranslation,
        FakeLuaTranslation,
    >;

    fn service(events: Arc<Mutex<Vec<Event>>>, failure: Option<Failure>) -> Service {
        TranslateService::new(
            FakeProfileResolver {
                events: Arc::clone(&events),
                failure,
            },
            FakeProjectReader {
                events: Arc::clone(&events),
                failure,
            },
            FakeStandardTranslation {
                events: Arc::clone(&events),
                failure,
            },
            Some(FakeLuaTranslation { events, failure }),
            CooperativeCancellation::default(),
        )
    }

    fn input(lua_script: Option<&str>) -> TranslateInput {
        TranslateInput {
            name: project_name(),
            profile_id: "quality-profile".to_owned(),
            terminology_path: Some(PathBuf::from("config/terms.json")),
            placeholder_rules_path: Some(PathBuf::from("config/placeholders.json")),
            lua_script: lua_script.map(PathBuf::from),
        }
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("测试项目名称应该合法")
    }

    fn project_record() -> StoredProjectRecord {
        StoredProjectRecord::new(
            project_name(),
            PathBuf::from("C:/Projects/alice"),
            PathBuf::from("C:/Projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn profile() -> FakeProfile {
        FakeProfile {
            profile_id: "quality-profile".to_owned(),
        }
    }

    fn standard_input() -> StandardTranslationInput {
        StandardTranslationInput::new(
            Some(PathBuf::from("config/terms.json")),
            Some(PathBuf::from("config/placeholders.json")),
        )
    }

    fn unavailable_report() -> StandardTranslationRunReport {
        let source = MzSource::data(StandardDataFile::Items);
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let identity = TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            MzLocation::value(
                source,
                vec![MzLocationStep::index(1), MzLocationStep::key("name")],
            ),
            "宝剑",
        );
        let outcome = TranslationTaskOutcome::unavailable(
            StandardTranslationTaskIndex::new(0),
            1,
            Some(FinalLlmResponseMetadata::new(
                Some("request-1".to_owned()),
                "response-1",
                "stop",
                None,
            )),
            TranslationTaskUnavailableReason::AllOutputsRejected,
            vec![UnresolvedTranslationUnit::new(
                0,
                identity,
                Vec::new(),
                TranslationUnitRejectionReason::Missing,
            )],
            Vec::new(),
        )
        .expect("测试不可用结果必须满足状态不变量");
        let mut report = StandardTranslationRunReport::empty(1);
        report.record(&outcome);
        report
    }

    fn expected_summary() -> StandardTranslationSummary {
        StandardTranslationSummary {
            total_tasks: 1,
            complete_tasks: 0,
            partial_tasks: 0,
            unavailable_tasks: 1,
            accepted_decisions: 0,
            written_locations: 0,
            remaining_decisions: 1,
            remaining_locations: 1,
            protocol_diagnostics: 0,
            recoverable_request_exhaustions: 0,
        }
    }

    fn events(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().expect("事件记录锁不应中毒").clone()
    }

    #[tokio::test]
    async fn resolves_reads_and_runs_both_stages_in_fixed_order() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(Arc::clone(&recorded), None);

        let output = service
            .execute(input(Some("scripts/translate.lua")))
            .await
            .expect("完整翻译编排应该成功");

        assert_eq!(output.name, project_name());
        assert_eq!(output.profile_id, "quality-profile");
        assert_eq!(output.standard, expected_summary());
        assert!(output.lua_executed);
        assert_eq!(
            events(&recorded),
            vec![
                Event::Resolve("quality-profile".to_owned()),
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile: profile(),
                    input: standard_input(),
                },
                Event::Lua {
                    project: project_record(),
                    profile: profile(),
                    script_path: PathBuf::from("scripts/translate.lua"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn omits_only_the_unselected_lua_stage() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let output = service(Arc::clone(&recorded), None)
            .execute(input(None))
            .await
            .expect("没有 Lua 的标准翻译应该成功");

        assert_eq!(output.standard, expected_summary());
        assert!(!output.lua_executed);

        assert_eq!(
            events(&recorded),
            vec![
                Event::Resolve("quality-profile".to_owned()),
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile: profile(),
                    input: standard_input(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn forwards_absent_optional_inputs_without_inventing_defaults() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut request = input(None);
        request.terminology_path = None;
        request.placeholder_rules_path = None;

        service(Arc::clone(&recorded), None)
            .execute(request)
            .await
            .expect("没有可选文件的标准翻译应该成功");

        assert_eq!(
            events(&recorded),
            vec![
                Event::Resolve("quality-profile".to_owned()),
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile: profile(),
                    input: StandardTranslationInput::new(None, None),
                },
            ]
        );
    }

    #[tokio::test]
    async fn each_failure_stops_every_later_stage() {
        let cases = [
            (
                Failure::Resolve,
                vec![Event::Resolve("quality-profile".to_owned())],
            ),
            (
                Failure::Read,
                vec![
                    Event::Resolve("quality-profile".to_owned()),
                    Event::Read(project_name()),
                ],
            ),
            (
                Failure::Standard,
                vec![
                    Event::Resolve("quality-profile".to_owned()),
                    Event::Read(project_name()),
                    Event::Standard {
                        project: project_record(),
                        profile: profile(),
                        input: standard_input(),
                    },
                ],
            ),
            (
                Failure::Lua,
                vec![
                    Event::Resolve("quality-profile".to_owned()),
                    Event::Read(project_name()),
                    Event::Standard {
                        project: project_record(),
                        profile: profile(),
                        input: standard_input(),
                    },
                    Event::Lua {
                        project: project_record(),
                        profile: profile(),
                        script_path: PathBuf::from("scripts/translate.lua"),
                    },
                ],
            ),
        ];

        for (failure, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let error = service(Arc::clone(&recorded), Some(failure))
                .execute(input(Some("scripts/translate.lua")))
                .await
                .expect_err("被选择的失败阶段应该向上返回");

            assert_eq!(events(&recorded), expected, "失败阶段：{failure:?}");
            let rendered = error.to_string();
            assert_eq!(
                error.source().expect("阶段错误应该保留 source").to_string(),
                match failure {
                    Failure::Resolve => "resolve",
                    Failure::Read => "read",
                    Failure::Standard => "standard",
                    Failure::Lua => "lua",
                }
            );

            match (failure, &error) {
                (
                    Failure::Resolve,
                    TranslateServiceError::ResolveProfile { profile_id, source },
                ) => {
                    assert_eq!(profile_id, "quality-profile");
                    assert_eq!(*source, FakeError("resolve"));
                }
                (Failure::Read, TranslateServiceError::ReadProject { name, source }) => {
                    assert_eq!(name, &project_name());
                    assert_eq!(*source, FakeError("read"));
                }
                (Failure::Standard, TranslateServiceError::Standard { source }) => {
                    assert_eq!(*source, FakeError("standard"));
                }
                (
                    Failure::Lua,
                    TranslateServiceError::Lua {
                        script_path,
                        source,
                    },
                ) => {
                    assert_eq!(script_path, &PathBuf::from("scripts/translate.lua"));
                    assert_eq!(*source, FakeError("lua"));
                }
                _ => panic!("失败阶段应映射为对应的顶层错误：{error}"),
            }

            match failure {
                Failure::Resolve => {
                    assert!(rendered.contains("翻译 Profile quality-profile"));
                }
                Failure::Read => assert!(rendered.contains("项目 alice")),
                Failure::Standard => assert!(rendered.starts_with("标准翻译失败")),
                Failure::Lua => assert!(rendered.contains("scripts/translate.lua")),
            }
        }
    }

    #[test]
    fn execution_future_is_send() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(recorded, None);

        assert_send(service.execute(input(None)));
    }

    fn assert_send(_: impl Send) {}
}
