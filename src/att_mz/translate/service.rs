use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use super::lua::LuaTranslation;
use super::profile::MzTranslationProfile;
use super::standard::{StandardTranslation, StandardTranslationInput};
use super::{StandardTranslationSummary, TranslateInput, TranslateOutput};
use crate::att_mz::lua::runtime::TrustedLuaTranslationSemantics;
use crate::att_mz::project::{ExistingProjectOpener, OpenedProject};
use crate::att_mz::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};
use crate::att_mz::{ProjectName, SelectedLua};
use crate::execution::{CooperativeCancellation, OperationCompletion};

/// 为当前项目语言对建立完整翻译执行切片的结果。
pub(crate) type SelectedTranslationExecutionBuildResult<C, S, L, E> =
    Result<SelectedTranslationExecution<C, S, L>, E>;

/// 打开项目后一次性建立当前语言对实际需要的翻译执行切片。
pub(crate) trait SelectedTranslationExecutionBuilder: Send + Sync {
    type Client: Send + Sync + 'static;
    type Standard: StandardTranslation<Profile = Arc<MzTranslationProfile<Self::Client>>>;
    type Lua: LuaTranslation<Client = Self::Client>;
    type Error: Error + Send + Sync + 'static;

    fn build(
        &self,
        project: &OpenedProject,
    ) -> impl std::future::Future<
        Output = SelectedTranslationExecutionBuildResult<
            Self::Client,
            Self::Standard,
            Self::Lua,
            Self::Error,
        >,
    > + Send;
}

/// 当前项目语言对唯一的一组 Standard、Profile 与可选 Lua 执行能力。
pub(crate) struct SelectedTranslationExecution<C, S, L> {
    profile: Arc<MzTranslationProfile<C>>,
    standard: S,
    lua: Option<SelectedLua<L>>,
}

impl<C, S, L> SelectedTranslationExecution<C, S, L> {
    pub(crate) fn new(
        profile: Arc<MzTranslationProfile<C>>,
        standard: S,
        lua: Option<SelectedLua<L>>,
    ) -> Self {
        Self {
            profile,
            standard,
            lua,
        }
    }
}

/// 按固定业务顺序编排一次 MZ 翻译。
///
/// 配置边界在构造本服务前已经完成 Profile 选择。本服务把完整 Profile 交给标准
/// 翻译；可选 Lua 只接收其中的公共 Client 和 Standard 交付的解析语义。
pub(crate) struct TranslateService<R, B, P> {
    project_opener: R,
    execution_builder: B,
    project_lease: P,
    cancellation: CooperativeCancellation,
}

impl<R, B, P> TranslateService<R, B, P> {
    pub(crate) fn new(
        project_opener: R,
        execution_builder: B,
        project_lease: P,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            execution_builder,
            project_lease,
            cancellation,
        }
    }
}

impl<R, B, P> TranslateService<R, B, P>
where
    R: ExistingProjectOpener,
    B: SelectedTranslationExecutionBuilder,
    P: ProjectCommandLeaseProvider,
{
    pub(crate) async fn execute(
        &self,
        input: TranslateInput,
    ) -> Result<
        OperationCompletion<TranslateOutput>,
        TranslateServiceError<
            R::Error,
            B::Error,
            <B::Standard as StandardTranslation>::Error,
            <B::Lua as LuaTranslation>::Error,
            P::Error,
        >,
    > {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let TranslateInput {
            name,
            terminology_path,
            placeholder_rules_path,
        } = input;

        let _lease = self
            .project_lease
            .acquire(&name)
            .await
            .map_err(TranslateServiceError::ProjectLease)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let project = self.project_opener.open(&name).await.map_err(|source| {
            TranslateServiceError::ReadProject {
                name: name.clone(),
                source,
            }
        })?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let execution = self
            .execution_builder
            .build(&project)
            .await
            .map_err(TranslateServiceError::BuildExecution)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let standard_report = self
            .run_standard(
                &execution,
                &project,
                StandardTranslationInput::new(terminology_path, placeholder_rules_path),
            )
            .await?;
        let OperationCompletion::Completed(standard_report) = standard_report else {
            return Ok(OperationCompletion::Cancelled);
        };
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let lua_executed = if let Some(selected_lua) = &execution.lua {
            let error_path = selected_lua.script_path().to_path_buf();
            let semantics: Arc<dyn TrustedLuaTranslationSemantics> = standard_report
                .resolved_semantics()
                .cloned()
                .ok_or(TranslateServiceError::MissingResolvedTranslationSemantics)?;
            let completion = selected_lua
                .executor()
                .run(
                    &project,
                    execution.profile.shared_llm_client(),
                    semantics,
                    error_path.clone(),
                )
                .await
                .map_err(|source| TranslateServiceError::Lua {
                    script_path: error_path,
                    source,
                })?;
            let OperationCompletion::Completed(()) = completion else {
                return Ok(OperationCompletion::Cancelled);
            };
            true
        } else {
            false
        };

        Ok(OperationCompletion::Completed(TranslateOutput {
            name,
            profile_id: execution.profile.id().to_owned(),
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
                retained: standard_report.retained(),
                invalidated: standard_report.invalidated(),
                not_applicable: standard_report.not_applicable(),
                reused: standard_report.reused(),
            },
            lua_executed,
        }))
    }

    async fn run_standard(
        &self,
        execution: &SelectedTranslationExecution<B::Client, B::Standard, B::Lua>,
        project: &OpenedProject,
        input: StandardTranslationInput,
    ) -> Result<
        OperationCompletion<super::standard::StandardTranslationRunReport>,
        TranslateServiceError<
            R::Error,
            B::Error,
            <B::Standard as StandardTranslation>::Error,
            <B::Lua as LuaTranslation>::Error,
            P::Error,
        >,
    > {
        execution
            .standard
            .run(project, &execution.profile, input)
            .await
            .map_err(|source| TranslateServiceError::Standard { source })
    }
}

/// 翻译用例在直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum TranslateServiceError<RE, BE, SE, LE, PE> {
    ProjectLease(ProjectCommandLeaseError<PE>),
    ReadProject { name: ProjectName, source: RE },
    BuildExecution(BE),
    Standard { source: SE },
    MissingResolvedTranslationSemantics,
    Lua { script_path: PathBuf, source: LE },
}

impl<RE, BE, SE, LE, PE> fmt::Display for TranslateServiceError<RE, BE, SE, LE, PE>
where
    RE: Error,
    BE: Error,
    SE: Error,
    LE: Error,
    PE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectLease(error) => error.fmt(formatter),
            Self::ReadProject { name, source } => {
                write!(formatter, "无法打开项目 {name}：{source}")
            }
            Self::BuildExecution(source) => {
                write!(formatter, "无法建立当前翻译执行上下文：{source}")
            }
            Self::Standard { source } => write!(formatter, "标准翻译失败：{source}"),
            Self::MissingResolvedTranslationSemantics => {
                formatter.write_str("标准翻译未交付 Lua 所需的当前翻译语义")
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

impl<RE, BE, SE, LE, PE> Error for TranslateServiceError<RE, BE, SE, LE, PE>
where
    RE: Error + 'static,
    BE: Error + 'static,
    SE: Error + 'static,
    LE: Error + 'static,
    PE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::ReadProject { source, .. } => Some(source),
            Self::BuildExecution(source) => Some(source),
            Self::Standard { source } => Some(source),
            Self::MissingResolvedTranslationSemantics => None,
            Self::Lua { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::att_mz::project::{ExistingProjectOpener, OpenedProject};
    use crate::att_mz::standard_asset::MzStandardAssetOwner;
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };
    use crate::att_mz::translate::executor::FinalLlmResponseMetadata;
    use crate::att_mz::translate::profile::{
        MzTranslationPlanningConfiguration, MzTranslationRequestConfiguration,
    };
    use crate::att_mz::translate::semantics::ResolvedTranslationSemantics;
    use crate::att_mz::translate::standard::{
        NonEmptyTaskItems, StandardTranslationRunReport, StandardTranslationTaskIndex,
        TranslationLeafIdentity, TranslationTaskOutcome, TranslationTaskOutcomeContext,
        TranslationTaskUnavailableReason, TranslationUnitRejectionReason,
        UnresolvedTranslationUnit,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Failure {
        Read,
        Standard,
        Lua,
        LuaCancelled,
    }

    #[derive(Debug)]
    struct FakeClient(&'static str);

    type SelectedProfile = Arc<MzTranslationProfile<FakeClient>>;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Read(ProjectName),
        Standard {
            project: OpenedProject,
            profile_id: String,
            input: StandardTranslationInput,
        },
        Lua {
            project: OpenedProject,
            client_name: String,
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
    struct FakeProjectReader {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
    }

    impl ExistingProjectOpener for FakeProjectReader {
        type Error = FakeError;

        async fn open(&self, name: &ProjectName) -> Result<OpenedProject, Self::Error> {
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
        semantics: Arc<ResolvedTranslationSemantics>,
        expected_profile: SelectedProfile,
    }

    impl StandardTranslation for FakeStandardTranslation {
        type Profile = SelectedProfile;
        type Error = FakeError;

        async fn run(
            &self,
            project: &OpenedProject,
            profile: &Self::Profile,
            input: StandardTranslationInput,
        ) -> Result<OperationCompletion<StandardTranslationRunReport>, Self::Error> {
            assert!(Arc::ptr_eq(profile, &self.expected_profile));
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Standard {
                    project: project.clone(),
                    profile_id: profile.id().to_owned(),
                    input,
                });

            if self.failure == Some(Failure::Standard) {
                Err(FakeError("standard"))
            } else {
                Ok(OperationCompletion::Completed(unavailable_report(
                    Arc::clone(&self.semantics),
                )))
            }
        }
    }

    #[derive(Clone)]
    struct FakeLuaTranslation {
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
        expected_semantics: Arc<ResolvedTranslationSemantics>,
        expected_client: Arc<FakeClient>,
    }

    impl LuaTranslation for FakeLuaTranslation {
        type Client = FakeClient;
        type Error = FakeError;

        async fn run(
            &self,
            project: &OpenedProject,
            llm_client: Arc<Self::Client>,
            semantics: Arc<dyn TrustedLuaTranslationSemantics>,
            script_path: PathBuf,
        ) -> Result<OperationCompletion<()>, Self::Error> {
            assert!(Arc::ptr_eq(&llm_client, &self.expected_client));
            assert_eq!(
                Arc::as_ptr(&semantics) as *const (),
                Arc::as_ptr(&self.expected_semantics) as *const (),
                "Lua 必须收到 Standard 交付的同一个语义快照",
            );
            self.events
                .lock()
                .expect("事件记录锁不应中毒")
                .push(Event::Lua {
                    project: project.clone(),
                    client_name: llm_client.0.to_owned(),
                    script_path,
                });

            if self.failure == Some(Failure::Lua) {
                Err(FakeError("lua"))
            } else if self.failure == Some(Failure::LuaCancelled) {
                Ok(OperationCompletion::Cancelled)
            } else {
                Ok(OperationCompletion::Completed(()))
            }
        }
    }

    struct FakeBuilder {
        profile: SelectedProfile,
        standard: FakeStandardTranslation,
        lua: Option<SelectedLua<FakeLuaTranslation>>,
    }

    impl SelectedTranslationExecutionBuilder for FakeBuilder {
        type Client = FakeClient;
        type Standard = FakeStandardTranslation;
        type Lua = FakeLuaTranslation;
        type Error = FakeError;

        async fn build(
            &self,
            _: &OpenedProject,
        ) -> Result<
            SelectedTranslationExecution<Self::Client, Self::Standard, Self::Lua>,
            Self::Error,
        > {
            Ok(SelectedTranslationExecution::new(
                Arc::clone(&self.profile),
                self.standard.clone(),
                self.lua.as_ref().map(|selected| {
                    SelectedLua::new(
                        selected.script_path().to_path_buf(),
                        selected.executor().clone(),
                    )
                }),
            ))
        }
    }

    #[derive(Clone, Copy)]
    struct FakeProjectLease;

    impl ProjectCommandLeaseProvider for FakeProjectLease {
        type Error = FakeError;
        type LeaseState = ();

        async fn acquire(
            &self,
            _: &ProjectName,
        ) -> Result<
            crate::att_mz::project_lease::ProjectCommandLease<Self::LeaseState>,
            ProjectCommandLeaseError<Self::Error>,
        > {
            Ok(crate::att_mz::project_lease::ProjectCommandLease::for_test(
                (),
            ))
        }
    }

    type Service = TranslateService<FakeProjectReader, FakeBuilder, FakeProjectLease>;

    fn service(
        events: Arc<Mutex<Vec<Event>>>,
        failure: Option<Failure>,
        lua_script: Option<&str>,
    ) -> Service {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let client = Arc::new(FakeClient("shared-client"));
        let profile = Arc::new(MzTranslationProfile::new(
            "quality-profile",
            std::num::NonZeroUsize::new(2).expect("测试并发数必须非零"),
            MzTranslationPlanningConfiguration::new(NonZeroUsize::MIN, NonZeroUsize::MIN),
            MzTranslationRequestConfiguration::new(Vec::new(), Duration::ZERO),
            Arc::clone(&client),
        ));
        TranslateService::new(
            FakeProjectReader {
                events: Arc::clone(&events),
                failure,
            },
            FakeBuilder {
                profile: Arc::clone(&profile),
                standard: FakeStandardTranslation {
                    events: Arc::clone(&events),
                    failure,
                    semantics: Arc::clone(&semantics),
                    expected_profile: Arc::clone(&profile),
                },
                lua: lua_script.map(|path| {
                    SelectedLua::new(
                        PathBuf::from(path),
                        FakeLuaTranslation {
                            events,
                            failure,
                            expected_semantics: semantics,
                            expected_client: client,
                        },
                    )
                }),
            },
            FakeProjectLease,
            CooperativeCancellation::default(),
        )
    }

    fn input(_: Option<&str>) -> TranslateInput {
        TranslateInput {
            name: project_name(),
            terminology_path: Some(PathBuf::from("config/terms.json")),
            placeholder_rules_path: Some(PathBuf::from("config/placeholders.json")),
        }
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("测试项目名称应该合法")
    }

    fn project_record() -> OpenedProject {
        OpenedProject::new(
            project_name(),
            PathBuf::from("C:/Projects/alice"),
            PathBuf::from("C:/Projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn standard_input() -> StandardTranslationInput {
        StandardTranslationInput::new(
            Some(PathBuf::from("config/terms.json")),
            Some(PathBuf::from("config/placeholders.json")),
        )
    }

    fn unavailable_report(
        semantics: Arc<ResolvedTranslationSemantics>,
    ) -> StandardTranslationRunReport {
        let source = MzSource::data(StandardDataFile::Items);
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let identity = TranslationLeafIdentity::new(
            MzStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            "name",
            group_location,
            MzLocation::value(
                source,
                vec![MzLocationStep::index(1), MzLocationStep::key("name")],
            ),
            "宝剑",
        );
        let outcome = TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::new(1).expect("测试尝试数应非零"),
                Vec::new(),
            ),
            final_response: Some(FinalLlmResponseMetadata::new(
                Some("request-1".to_owned()),
                Some("response-1".to_owned()),
                "stop",
                None,
            )),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: NonEmptyTaskItems::new(
                UnresolvedTranslationUnit::new(
                    0,
                    identity,
                    Vec::new(),
                    TranslationUnitRejectionReason::Missing,
                ),
                Vec::new(),
            ),
        };
        let mut report = StandardTranslationRunReport::empty(1);
        report.record(&outcome);
        report.with_semantics(semantics)
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
            retained: 0,
            invalidated: 0,
            not_applicable: 0,
            reused: 0,
        }
    }

    fn events(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().expect("事件记录锁不应中毒").clone()
    }

    #[tokio::test]
    async fn standard_receives_the_profile_and_lua_receives_its_client_and_resolved_semantics() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(Arc::clone(&recorded), None, Some("scripts/translate.lua"));

        let output = service
            .execute(input(Some("scripts/translate.lua")))
            .await
            .expect("完整翻译编排应该成功");

        let OperationCompletion::Completed(output) = output else {
            panic!("翻译应正常完成")
        };
        assert_eq!(output.name, project_name());
        assert_eq!(output.profile_id, "quality-profile");
        assert_eq!(output.standard, expected_summary());
        assert!(output.lua_executed);
        assert_eq!(
            events(&recorded),
            vec![
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile_id: "quality-profile".to_owned(),
                    input: standard_input(),
                },
                Event::Lua {
                    project: project_record(),
                    client_name: "shared-client".to_owned(),
                    script_path: PathBuf::from("scripts/translate.lua"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn lua_cancellation_is_the_normal_top_level_completion() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let completion = service(
            Arc::clone(&recorded),
            Some(Failure::LuaCancelled),
            Some("scripts/translate.lua"),
        )
        .execute(input(Some("scripts/translate.lua")))
        .await
        .expect("Lua 取消应作为正常结果传播");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert!(matches!(events(&recorded).last(), Some(Event::Lua { .. })));
    }

    #[tokio::test]
    async fn omits_only_the_unselected_lua_stage() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let output = service(Arc::clone(&recorded), None, None)
            .execute(input(None))
            .await
            .expect("没有 Lua 的标准翻译应该成功");

        let OperationCompletion::Completed(output) = output else {
            panic!("翻译应正常完成")
        };
        assert_eq!(output.standard, expected_summary());
        assert!(!output.lua_executed);

        assert_eq!(
            events(&recorded),
            vec![
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile_id: "quality-profile".to_owned(),
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

        service(Arc::clone(&recorded), None, None)
            .execute(request)
            .await
            .expect("没有可选文件的标准翻译应该成功");

        assert_eq!(
            events(&recorded),
            vec![
                Event::Read(project_name()),
                Event::Standard {
                    project: project_record(),
                    profile_id: "quality-profile".to_owned(),
                    input: StandardTranslationInput::new(None, None),
                },
            ]
        );
    }

    #[tokio::test]
    async fn each_failure_stops_every_later_stage() {
        let cases = [
            (Failure::Read, vec![Event::Read(project_name())]),
            (
                Failure::Standard,
                vec![
                    Event::Read(project_name()),
                    Event::Standard {
                        project: project_record(),
                        profile_id: "quality-profile".to_owned(),
                        input: standard_input(),
                    },
                ],
            ),
            (
                Failure::Lua,
                vec![
                    Event::Read(project_name()),
                    Event::Standard {
                        project: project_record(),
                        profile_id: "quality-profile".to_owned(),
                        input: standard_input(),
                    },
                    Event::Lua {
                        project: project_record(),
                        client_name: "shared-client".to_owned(),
                        script_path: PathBuf::from("scripts/translate.lua"),
                    },
                ],
            ),
        ];

        for (failure, expected) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let error = service(
                Arc::clone(&recorded),
                Some(failure),
                Some("scripts/translate.lua"),
            )
            .execute(input(Some("scripts/translate.lua")))
            .await
            .expect_err("被选择的失败阶段应该向上返回");

            assert_eq!(events(&recorded), expected, "失败阶段：{failure:?}");
            let rendered = error.to_string();
            assert_eq!(
                error.source().expect("阶段错误应该保留 source").to_string(),
                match failure {
                    Failure::Read => "read",
                    Failure::Standard => "standard",
                    Failure::Lua => "lua",
                    Failure::LuaCancelled => unreachable!("取消不属于技术失败矩阵"),
                }
            );

            match (failure, &error) {
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
                Failure::Read => assert!(rendered.contains("项目 alice")),
                Failure::Standard => assert!(rendered.starts_with("标准翻译失败")),
                Failure::Lua => assert!(rendered.contains("scripts/translate.lua")),
                Failure::LuaCancelled => unreachable!("取消不属于技术失败矩阵"),
            }
        }
    }

    #[test]
    fn execution_future_is_send() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service = service(recorded, None, None);

        assert_send(service.execute(input(None)));
    }

    fn assert_send(_: impl Send) {}
}
