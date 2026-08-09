use std::error::Error;
use std::fmt;
use std::sync::Arc;

use super::pipeline::{RpgMakerTranslation, RpgMakerTranslationInput};
use super::profile::RpgMakerTranslationProfile;
use super::{TranslateInput, TranslateOutput, TranslationSummary};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};
use crate::project_name::ProjectName;
use crate::rpg_maker::project::{ExistingProjectOpener, OpenedProject};

/// 为当前项目语言对建立完整翻译执行切片的结果。
pub(crate) type SelectedTranslationExecutionBuildResult<C, T, E> =
    Result<SelectedTranslationExecution<C, T>, E>;

type ClassifiedSelectedTranslationExecution<C, T, E> =
    Result<OperationCompletion<SelectedTranslationExecution<C, T>>, E>;

/// 打开项目后一次性建立当前语言对实际需要的翻译执行切片。
pub(crate) trait SelectedTranslationExecutionBuilder: Send + Sync {
    type Client: Send + Sync + 'static;
    type Translation: RpgMakerTranslation<Profile = Arc<RpgMakerTranslationProfile<Self::Client>>>;
    type Error: Error + Send + Sync + 'static;

    /// 只根据构建器返回的类型化错误判断是否为合作取消。
    fn is_cancelled_error(error: &Self::Error) -> bool;

    fn build(
        &self,
        project: &OpenedProject,
    ) -> impl std::future::Future<
        Output = SelectedTranslationExecutionBuildResult<
            Self::Client,
            Self::Translation,
            Self::Error,
        >,
    > + Send;
}

/// 当前项目语言对唯一的一组 Profile 与 RPG Maker 翻译能力。
pub(crate) struct SelectedTranslationExecution<C, T> {
    profile: Arc<RpgMakerTranslationProfile<C>>,
    translation: T,
}

impl<C, T> SelectedTranslationExecution<C, T> {
    pub(crate) fn new(profile: Arc<RpgMakerTranslationProfile<C>>, translation: T) -> Self {
        Self {
            profile,
            translation,
        }
    }
}

/// 编排一次 RPG Maker 翻译。
///
/// 配置边界在构造本服务前已经完成 Profile 选择。本服务把完整 Profile 和本次
/// 术语、Placeholder 输入交给 RPG Maker 翻译能力。
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
            <B::Translation as RpgMakerTranslation>::Error,
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

        let execution = classify_execution_build::<B>(self.execution_builder.build(&project).await)
            .map_err(TranslateServiceError::BuildExecution)?;
        let OperationCompletion::Completed(execution) = execution else {
            return Ok(OperationCompletion::Cancelled);
        };
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let report = Box::pin(execution.translation.run(
            &project,
            &execution.profile,
            RpgMakerTranslationInput::new(terminology_path, placeholder_rules_path),
        ))
        .await
        .map_err(|source| TranslateServiceError::Translation { source })?;
        let OperationCompletion::Completed(report) = report else {
            return Ok(OperationCompletion::Cancelled);
        };

        Ok(OperationCompletion::Completed(TranslateOutput {
            name,
            profile_id: execution.profile.id().to_owned(),
            summary: TranslationSummary {
                total_tasks: report.total_tasks(),
                started_tasks: report.started_tasks(),
                not_started_tasks: report.not_started_tasks(),
                complete_tasks: report.complete_tasks(),
                partial_tasks: report.partial_tasks(),
                unavailable_tasks: report.unavailable_tasks(),
                accepted_decisions: report.accepted_decisions(),
                written_locations: report.written_locations(),
                remaining_decisions: report.unresolved_decisions(),
                remaining_locations: report.unresolved_locations(),
                protocol_diagnostics: report.protocol_diagnostics(),
                recoverable_request_exhaustions: report.recoverable_request_exhaustions(),
                request_admission_stopped: report.request_admission_stopped(),
                retained: report.retained(),
                invalidated: report.invalidated(),
                not_applicable: report.not_applicable(),
                reused: report.reused(),
            },
        }))
    }
}

fn classify_execution_build<B>(
    result: Result<SelectedTranslationExecution<B::Client, B::Translation>, B::Error>,
) -> ClassifiedSelectedTranslationExecution<B::Client, B::Translation, B::Error>
where
    B: SelectedTranslationExecutionBuilder,
{
    match result {
        Ok(execution) => Ok(OperationCompletion::Completed(execution)),
        Err(source) if B::is_cancelled_error(&source) => Ok(OperationCompletion::Cancelled),
        Err(source) => Err(source),
    }
}

/// 翻译用例在直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum TranslateServiceError<RE, BE, TE, PE> {
    ProjectLease(ProjectCommandLeaseError<PE>),
    ReadProject { name: ProjectName, source: RE },
    BuildExecution(BE),
    Translation { source: TE },
}

impl<RE, BE, TE, PE> fmt::Display for TranslateServiceError<RE, BE, TE, PE>
where
    RE: Error,
    BE: Error,
    TE: Error,
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
            Self::Translation { source } => write!(formatter, "RPG Maker 翻译失败：{source}"),
        }
    }
}

impl<RE, BE, TE, PE> Error for TranslateServiceError<RE, BE, TE, PE>
where
    RE: Error + 'static,
    BE: Error + 'static,
    TE: Error + 'static,
    PE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::ReadProject { source, .. } => Some(source),
            Self::BuildExecution(source) => Some(source),
            Self::Translation { source } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::project_lease::{ProjectCommandLease, ProjectCommandLeaseProvider};
    use crate::rpg_maker::project::test_layout_profile;
    use crate::rpg_maker::translate::pipeline::RpgMakerTranslationRunReport;
    use crate::rpg_maker::translate::profile::RpgMakerTranslationPlanningConfiguration;
    use crate::translation::profile::TranslationRequestConfiguration;

    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake failure")
        }
    }

    impl Error for FakeError {}

    struct FakeProjectOpener {
        project: OpenedProject,
    }

    impl ExistingProjectOpener for FakeProjectOpener {
        type Error = FakeError;

        async fn open(&self, _name: &ProjectName) -> Result<OpenedProject, Self::Error> {
            Ok(self.project.clone())
        }
    }

    struct FakeLeaseProvider;

    impl ProjectCommandLeaseProvider for FakeLeaseProvider {
        type Error = FakeError;
        type LeaseState = ();

        async fn acquire(
            &self,
            _project: &ProjectName,
        ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>>
        {
            Ok(ProjectCommandLease::for_test(()))
        }
    }

    struct UnusedTranslation;

    impl RpgMakerTranslation for UnusedTranslation {
        type Profile = Arc<RpgMakerTranslationProfile<()>>;
        type Error = FakeError;

        async fn run(
            &self,
            _project: &OpenedProject,
            _profile: &Self::Profile,
            _input: RpgMakerTranslationInput,
        ) -> Result<OperationCompletion<RpgMakerTranslationRunReport>, Self::Error> {
            panic!("构建失败后不得开始翻译")
        }
    }

    struct CancellingBuilder {
        cancellation: CooperativeCancellation,
    }

    #[derive(Debug)]
    struct TypedCancellationError;

    impl fmt::Display for TypedCancellationError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("typed cancellation")
        }
    }

    impl Error for TypedCancellationError {}

    struct TypedCancellingBuilder;

    impl SelectedTranslationExecutionBuilder for TypedCancellingBuilder {
        type Client = ();
        type Translation = UnusedTranslation;
        type Error = TypedCancellationError;

        fn is_cancelled_error(_error: &Self::Error) -> bool {
            true
        }

        async fn build(
            &self,
            _project: &OpenedProject,
        ) -> SelectedTranslationExecutionBuildResult<Self::Client, Self::Translation, Self::Error>
        {
            Err(TypedCancellationError)
        }
    }

    struct CompletedAfterCancellationTranslation {
        cancellation: CooperativeCancellation,
    }

    impl RpgMakerTranslation for CompletedAfterCancellationTranslation {
        type Profile = Arc<RpgMakerTranslationProfile<()>>;
        type Error = FakeError;

        async fn run(
            &self,
            _project: &OpenedProject,
            _profile: &Self::Profile,
            _input: RpgMakerTranslationInput,
        ) -> Result<OperationCompletion<RpgMakerTranslationRunReport>, Self::Error> {
            self.cancellation.request();
            Ok(OperationCompletion::Completed(
                RpgMakerTranslationRunReport::with_reconciliation(1, 0, 0, 1, 0, 0, 0),
            ))
        }
    }

    struct CompletedAfterCancellationBuilder {
        cancellation: CooperativeCancellation,
    }

    impl SelectedTranslationExecutionBuilder for CompletedAfterCancellationBuilder {
        type Client = ();
        type Translation = CompletedAfterCancellationTranslation;
        type Error = FakeError;

        fn is_cancelled_error(_error: &Self::Error) -> bool {
            false
        }

        async fn build(
            &self,
            _project: &OpenedProject,
        ) -> SelectedTranslationExecutionBuildResult<Self::Client, Self::Translation, Self::Error>
        {
            let profile = Arc::new(RpgMakerTranslationProfile::new(
                "test-profile",
                RpgMakerTranslationPlanningConfiguration::new(
                    NonZeroUsize::new(1).expect("测试规划字符数必须非零"),
                ),
                TranslationRequestConfiguration::new(Vec::new(), Duration::ZERO),
                Arc::new(()),
            ));
            Ok(SelectedTranslationExecution::new(
                profile,
                CompletedAfterCancellationTranslation {
                    cancellation: self.cancellation.clone(),
                },
            ))
        }
    }

    impl SelectedTranslationExecutionBuilder for CancellingBuilder {
        type Client = ();
        type Translation = UnusedTranslation;
        type Error = FakeError;

        fn is_cancelled_error(_error: &Self::Error) -> bool {
            false
        }

        async fn build(
            &self,
            _project: &OpenedProject,
        ) -> SelectedTranslationExecutionBuildResult<Self::Client, Self::Translation, Self::Error>
        {
            self.cancellation.request();
            Err(FakeError)
        }
    }

    #[tokio::test]
    async fn shared_cancellation_flag_does_not_overwrite_a_real_builder_error() {
        let cancellation = CooperativeCancellation::default();
        let name = "cancelled-build"
            .parse::<ProjectName>()
            .expect("测试项目名应合法");
        let project = OpenedProject::new(
            name.clone(),
            PathBuf::from("C:/att-test/workspace"),
            PathBuf::from("C:/att-test/workspace/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        );
        let service = TranslateService::new(
            FakeProjectOpener { project },
            CancellingBuilder {
                cancellation: cancellation.clone(),
            },
            FakeLeaseProvider,
            cancellation,
        );

        let result = service
            .execute(TranslateInput {
                name,
                terminology_path: None,
                placeholder_rules_path: None,
            })
            .await;

        assert!(matches!(
            result,
            Err(TranslateServiceError::BuildExecution(FakeError))
        ));
    }

    #[tokio::test]
    async fn typed_builder_cancellation_is_a_normal_cancelled_result() {
        let cancellation = CooperativeCancellation::default();
        let name = "typed-cancelled-build"
            .parse::<ProjectName>()
            .expect("测试项目名应合法");
        let project = OpenedProject::new(
            name.clone(),
            PathBuf::from("C:/att-test/workspace"),
            PathBuf::from("C:/att-test/workspace/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        );
        let service = TranslateService::new(
            FakeProjectOpener { project },
            TypedCancellingBuilder,
            FakeLeaseProvider,
            cancellation,
        );

        let result = service
            .execute(TranslateInput {
                name,
                terminology_path: None,
                placeholder_rules_path: None,
            })
            .await;

        assert!(matches!(result, Ok(OperationCompletion::Cancelled)));
    }

    #[tokio::test]
    async fn cancellation_after_translation_completion_keeps_the_completed_report() {
        let cancellation = CooperativeCancellation::default();
        let name = "completed-before-cancel"
            .parse::<ProjectName>()
            .expect("测试项目名应合法");
        let project = OpenedProject::new(
            name.clone(),
            PathBuf::from("C:/att-test/workspace"),
            PathBuf::from("C:/att-test/workspace/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        );
        let service = TranslateService::new(
            FakeProjectOpener { project },
            CompletedAfterCancellationBuilder {
                cancellation: cancellation.clone(),
            },
            FakeLeaseProvider,
            cancellation,
        );

        let completion = service
            .execute(TranslateInput {
                name,
                terminology_path: None,
                placeholder_rules_path: None,
            })
            .await
            .expect("翻译已完成后到达的取消不得改写结果");
        let OperationCompletion::Completed(output) = completion else {
            panic!("翻译已完成后应返回完成结果")
        };
        assert_eq!(output.profile_id, "test-profile");
        assert_eq!(output.summary.total_tasks, 1);
        assert_eq!(output.summary.retained, 1);
    }
}
