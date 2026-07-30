use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::builtin::BuiltInExtraction;
use super::rules::RulesExtraction;
use super::{ExtractInput, ExtractOutput, ExtractProgress, ExtractProgressPhase, SelectedRules};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::ProgressObserver;
use crate::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};
use crate::rpg_maker::project::ExistingProjectOpener;

/// 按固定业务顺序编排一次 RPG Maker 文本提取。
///
/// 用例只打开一次项目，随后按 Builtin、Rules 执行被选择的阶段。首个失败会
/// 阻止后续阶段，已经成功提交的前序阶段不由本层做组合回滚。
pub(crate) struct ExtractService<O, B, R, P> {
    project_opener: O,
    built_in_extraction: Option<B>,
    selected_rules: Option<SelectedRules<R>>,
    project_lease: P,
    cancellation: CooperativeCancellation,
    progress: ExtractProgress,
}

impl<O, B, R, P> ExtractService<O, B, R, P> {
    pub(crate) fn new(
        project_opener: O,
        built_in_extraction: Option<B>,
        selected_rules: Option<SelectedRules<R>>,
        project_lease: P,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            built_in_extraction,
            selected_rules,
            project_lease,
            cancellation,
            progress: ExtractProgress::default(),
        }
    }

    /// 为本次 Extract 绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<ExtractProgressPhase> + 'static,
    {
        self.progress = ExtractProgress::new(progress);
        self
    }

    fn observe_owner(&self, phase: ExtractProgressPhase, completed: u64, total: u64) {
        self.progress.determinate(phase, completed, total);
    }
}

impl<O, B, R, P> ExtractService<O, B, R, P>
where
    O: ExistingProjectOpener,
    B: BuiltInExtraction,
    R: RulesExtraction,
    P: ProjectCommandLeaseProvider,
{
    pub(crate) async fn execute(
        &self,
        input: ExtractInput,
    ) -> Result<
        OperationCompletion<ExtractOutput>,
        ExtractServiceError<O::Error, B::Error, R::Error, P::Error>,
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

        let total_owners = u64::from(self.built_in_extraction.is_some() as u8)
            + u64::from(self.selected_rules.is_some() as u8);
        let mut completed_owners = 0_u64;

        if let Some(built_in_extraction) = &self.built_in_extraction {
            self.observe_owner(
                ExtractProgressPhase::Builtin,
                completed_owners,
                total_owners,
            );
            built_in_extraction
                .refresh(&project, self.progress.clone())
                .await
                .map_err(ExtractServiceError::BuiltIn)?;
            completed_owners += 1;
            self.observe_owner(
                ExtractProgressPhase::Builtin,
                completed_owners,
                total_owners,
            );
            if self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
        }

        if let Some(selected_rules) = &self.selected_rules {
            self.observe_owner(ExtractProgressPhase::Rules, completed_owners, total_owners);
            let error_path = selected_rules.program().diagnostic_path().to_path_buf();
            selected_rules
                .executor()
                .replace(
                    &project,
                    selected_rules.program().clone(),
                    self.progress.clone(),
                )
                .await
                .map_err(|source| ExtractServiceError::Rules {
                    rules_path: error_path,
                    source,
                })?;
            completed_owners += 1;
            self.observe_owner(ExtractProgressPhase::Rules, completed_owners, total_owners);
        }

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        Ok(OperationCompletion::Completed(ExtractOutput { name }))
    }
}

/// 提取用例在直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum ExtractServiceError<OE, BE, RE, PE> {
    ProjectLease(ProjectCommandLeaseError<PE>),
    OpenProject(OE),
    BuiltIn(BE),
    Rules { rules_path: PathBuf, source: RE },
}

impl<OE, BE, RE, PE> fmt::Display for ExtractServiceError<OE, BE, RE, PE>
where
    OE: Error,
    BE: Error,
    RE: Error,
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
        }
    }
}

impl<OE, BE, RE, PE> Error for ExtractServiceError<OE, BE, RE, PE>
where
    OE: Error + 'static,
    BE: Error + 'static,
    RE: Error + 'static,
    PE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::OpenProject(source) => Some(source),
            Self::BuiltIn(source) => Some(source),
            Self::Rules { source, .. } => Some(source),
        }
    }
}
