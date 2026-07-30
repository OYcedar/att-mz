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

/// 打开项目后一次性建立当前语言对实际需要的翻译执行切片。
pub(crate) trait SelectedTranslationExecutionBuilder: Send + Sync {
    type Client: Send + Sync + 'static;
    type Translation: RpgMakerTranslation<Profile = Arc<RpgMakerTranslationProfile<Self::Client>>>;
    type Error: Error + Send + Sync + 'static;

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

        let execution = self
            .execution_builder
            .build(&project)
            .await
            .map_err(TranslateServiceError::BuildExecution)?;
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
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        Ok(OperationCompletion::Completed(TranslateOutput {
            name,
            profile_id: execution.profile.id().to_owned(),
            summary: TranslationSummary {
                total_tasks: report.total_tasks(),
                complete_tasks: report.complete_tasks(),
                partial_tasks: report.partial_tasks(),
                unavailable_tasks: report.unavailable_tasks(),
                accepted_decisions: report.accepted_decisions(),
                written_locations: report.written_locations(),
                remaining_decisions: report.unresolved_decisions(),
                remaining_locations: report.unresolved_locations(),
                protocol_diagnostics: report.protocol_diagnostics(),
                recoverable_request_exhaustions: report.recoverable_request_exhaustions(),
                retained: report.retained(),
                invalidated: report.invalidated(),
                not_applicable: report.not_applicable(),
                reused: report.reused(),
            },
        }))
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
