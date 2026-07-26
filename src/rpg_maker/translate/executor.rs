//! 标准翻译任务的模型调用、有限响应清洗与译后验收。
//!
//! 本模块只对可恢复网络失败按外部预算重试，并把网络请求、可取消等待与
//! CPU 调度停在根接口。模型内容按 ID 独立验收，完整、部分或完全不可用均
//! 是正常业务结果；网络重试始终复用 Planner 建立的完整消息。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::json_diagnostic::JsonErrorCategory;
use crate::language::{
    LanguageId, LanguageModule, LanguageModuleError, LanguagePair, LanguageRepairApplicationError,
    LanguageText, LanguageTextSegment,
};
use crate::llm::{
    LlmClientConcurrency, LlmFinishReason, LlmRequestDiagnosticSource, LlmRequestError,
    LlmRequestExecutor, LlmResponse, LlmUsage,
};
use crate::rpg_maker::model::{
    TextUnitContent, TextUnitContentStructureError, TextUnitContentView,
    validate_text_unit_content_structure, validate_text_unit_lines,
};
use crate::rpg_maker::placeholder_token;

use super::language_projection::{
    LanguageTextProjectionError, PlaceholderBindingIndex, PlaceholderMultisetError,
    PlaceholderTextScan,
};
use super::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerTranslationProfile, TranslationResponseEnvelope,
};
use super::standard::{
    AcceptedTranslationDecision, AppliedPlaceholder, ExpectedLineShape, ExpectedTranslationOutput,
    NonEmptyTaskItems, StandardTranslationProfile, StandardTranslationTaskExecutor,
    StandardTranslationTaskIndex, TranslationPatch, TranslationProtocolDiagnostic,
    TranslationTargetConstraints, TranslationTaskBlock, TranslationTaskOutcome,
    TranslationTaskOutcomeContext, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason, UnresolvedTranslationUnit,
};
use super::task_record::{
    TranslationAssistantEntry, TranslationAssistantValueError, TranslationTaskAttemptRecord,
    TranslationTaskExecution, TranslationTaskExecutionEvidence, TranslationTaskExecutionFailure,
    TranslationTaskResponseJsonErrorCategory, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind, TranslationTaskResponseRecord,
    TranslationTaskRetryWaitRecord,
};

/// 一次最终成功 HTTP 响应中可安全进入任务结果与持久日志的元数据。
///
/// `provider_request_id` 来自响应头 `x-request-id`，`provider_response_id`
/// 来自 Chat Completions 正文 `id`。供应商可以省略两者，且两者语义不同，
/// 不能相互补位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FinalLlmResponseMetadata {
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: String,
    usage: Option<LlmUsage>,
}

impl FinalLlmResponseMetadata {
    pub(crate) fn new(
        provider_request_id: Option<String>,
        provider_response_id: Option<String>,
        finish_reason: impl Into<String>,
        usage: Option<LlmUsage>,
    ) -> Self {
        Self {
            provider_request_id,
            provider_response_id,
            finish_reason: finish_reason.into(),
            usage,
        }
    }

    fn from_response(response: &LlmResponse) -> Self {
        Self::new(
            response.provider_request_id().map(str::to_owned),
            response.provider_response_id().map(str::to_owned),
            response.finish_reason().to_string(),
            response.usage(),
        )
    }

    #[cfg(test)]
    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.provider_response_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> &str {
        &self.finish_reason
    }

    #[cfg(test)]
    pub(crate) const fn usage(&self) -> Option<LlmUsage> {
        self.usage
    }
}

/// 可取消异步等待的根能力。
pub(crate) trait AsyncDelay: Send + Sync {
    fn wait(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Executor 从受信 RPG Maker Profile 消费的最小配置面。
pub(crate) trait TranslationTaskExecutionProfile: StandardTranslationProfile {
    type LlmClient: Send + Sync + 'static;

    fn llm_client(&self) -> &Self::LlmClient;
    fn network_retry_delays(&self) -> &[Duration];
    fn max_network_retry_after(&self) -> Duration;
}

impl<L> TranslationTaskExecutionProfile for RpgMakerTranslationProfile<L>
where
    L: LlmClientConcurrency + 'static,
{
    type LlmClient = L;

    fn llm_client(&self) -> &Self::LlmClient {
        self.llm_client()
    }

    fn network_retry_delays(&self) -> &[Duration] {
        self.request().network_retry_delays()
    }

    fn max_network_retry_after(&self) -> Duration {
        self.request().max_network_retry_after()
    }
}

impl<L> TranslationTaskExecutionProfile for Arc<RpgMakerTranslationProfile<L>>
where
    L: LlmClientConcurrency + 'static,
{
    type LlmClient = L;

    fn llm_client(&self) -> &Self::LlmClient {
        self.as_ref().llm_client()
    }

    fn network_retry_delays(&self) -> &[Duration] {
        self.as_ref().request().network_retry_delays()
    }

    fn max_network_retry_after(&self) -> Duration {
        self.as_ref().request().max_network_retry_after()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationCandidateInvariantLocation {
    TaskUnit {
        task_index: StandardTranslationTaskIndex,
        unit_id: usize,
    },
    PreparedCandidate,
}

#[derive(Eq, PartialEq)]
pub(crate) enum TranslationInternalInvariant {
    ResponseAttemptZero {
        task_index: StandardTranslationTaskIndex,
    },
    ExpectedOutputsEmpty {
        task_index: StandardTranslationTaskIndex,
    },
    LanguagePairMismatch {
        task_index: StandardTranslationTaskIndex,
        task_source: LanguageId,
        task_target: LanguageId,
        resolved_source: LanguageId,
        resolved_target: LanguageId,
    },
    RepairSegmentRangeMissing {
        location: TranslationCandidateInvariantLocation,
        line_index: usize,
        start: usize,
        end: usize,
        actual: usize,
    },
    RepairLineBoundaryMissing {
        location: TranslationCandidateInvariantLocation,
        line_index: usize,
        segment_index: usize,
        actual: usize,
    },
    RepairUnassignedSegments {
        location: TranslationCandidateInvariantLocation,
        consumed: usize,
        actual: usize,
    },
    ReservedTokenAfterRestore {
        location: TranslationCandidateInvariantLocation,
    },
}

impl TranslationInternalInvariant {
    pub(crate) fn safe_diagnostic(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            stage,
            self.diagnostic_subject(),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                self.safe_detail(),
            ),
            impact,
            DiagnosticAction::ReportBug,
        )
    }

    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::ResponseAttemptZero { task_index } => {
                format!(
                    "response_attempt_zero; task={}; attempt=0",
                    task_index.get()
                )
            }
            Self::ExpectedOutputsEmpty { task_index } => {
                format!(
                    "expected_outputs_empty; task={}; expected_output_count=0",
                    task_index.get()
                )
            }
            Self::LanguagePairMismatch {
                task_index,
                task_source,
                task_target,
                resolved_source,
                resolved_target,
            } => format!(
                "language_pair_mismatch; task={}; task_source={}; task_target={}; resolved_source={}; resolved_target={}",
                task_index.get(),
                task_source.as_str(),
                task_target.as_str(),
                resolved_source.as_str(),
                resolved_target.as_str()
            ),
            Self::RepairSegmentRangeMissing {
                location,
                line_index,
                start,
                end,
                actual,
            } => format!(
                "repair_segment_range_missing; {}; line_index={line_index}; start={start}; end={end}; actual={actual}",
                candidate_location_detail(*location)
            ),
            Self::RepairLineBoundaryMissing {
                location,
                line_index,
                segment_index,
                actual,
            } => format!(
                "repair_line_boundary_missing; {}; line_index={line_index}; segment_index={segment_index}; actual={actual}",
                candidate_location_detail(*location)
            ),
            Self::RepairUnassignedSegments {
                location,
                consumed,
                actual,
            } => format!(
                "repair_unassigned_segments; {}; consumed={consumed}; actual={actual}",
                candidate_location_detail(*location)
            ),
            Self::ReservedTokenAfterRestore { location } => format!(
                "reserved_token_after_restore; {}",
                candidate_location_detail(*location)
            ),
        }
    }

    fn diagnostic_subject(&self) -> DiagnosticSubject {
        match self {
            Self::ResponseAttemptZero { task_index }
            | Self::ExpectedOutputsEmpty { task_index }
            | Self::LanguagePairMismatch { task_index, .. } => {
                translation_task_subject(*task_index)
            }
            Self::RepairSegmentRangeMissing { location, .. }
            | Self::RepairLineBoundaryMissing { location, .. }
            | Self::RepairUnassignedSegments { location, .. }
            | Self::ReservedTokenAfterRestore { location } => candidate_location_subject(*location),
        }
    }
}

impl fmt::Debug for TranslationInternalInvariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TranslationInternalInvariant")
            .field(&self.safe_detail())
            .finish()
    }
}

impl fmt::Display for TranslationInternalInvariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_detail())
    }
}

impl Error for TranslationInternalInvariant {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

fn translation_task_subject(task_index: StandardTranslationTaskIndex) -> DiagnosticSubject {
    DiagnosticSubject::operation(format!("translation_task_{}", task_index.get()))
}

fn translation_unit_subject(
    task_index: StandardTranslationTaskIndex,
    unit_id: usize,
) -> DiagnosticSubject {
    DiagnosticSubject::operation(format!(
        "translation_task_{}_unit_{unit_id}",
        task_index.get()
    ))
}

fn candidate_location_subject(
    location: TranslationCandidateInvariantLocation,
) -> DiagnosticSubject {
    match location {
        TranslationCandidateInvariantLocation::TaskUnit {
            task_index,
            unit_id,
        } => translation_unit_subject(task_index, unit_id),
        TranslationCandidateInvariantLocation::PreparedCandidate => {
            DiagnosticSubject::operation("prepared_translation_candidate")
        }
    }
}

fn candidate_location_detail(location: TranslationCandidateInvariantLocation) -> String {
    match location {
        TranslationCandidateInvariantLocation::TaskUnit {
            task_index,
            unit_id,
        } => format!("scope=task_unit; task={}; unit={unit_id}", task_index.get()),
        TranslationCandidateInvariantLocation::PreparedCandidate => {
            "scope=prepared_candidate".to_owned()
        }
    }
}

/// 将一次原始模型响应验收为正常业务结果。
///
/// 模型内容完整、部分可用或完全不可用都返回 `TranslationTaskOutcome`；只有
/// CPU、语言模块或内部不变量已经无法继续履行契约时才返回错误。
pub(crate) trait TranslationTaskResponseProcessor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn process(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
        attempt: usize,
    ) -> impl Future<Output = Result<TranslationTaskOutcome, Self::Error>> + Send;

    fn process_recorded(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
        attempt: usize,
    ) -> impl Future<
        Output = Result<
            ProcessedTranslationTaskResponse,
            RecordedTranslationTaskResponseFailure<Self::Error>,
        >,
    > + Send;

    fn task_record_diagnostic(&self, _error: &Self::Error) -> Option<SafeDiagnostic> {
        None
    }

    /// 错误是否明确来自响应处理等待 CPU 入场时的合作取消。
    ///
    /// Executor 还会同时检查本次翻译的取消令牌，避免把独立 CPU 根关闭误报为取消。
    fn is_cancelled_wait(&self, _error: &Self::Error) -> bool {
        false
    }
}

/// 业务验收与任务记录共同消费的单次解析结果。
#[derive(Debug)]
pub(crate) struct ProcessedTranslationTaskResponse {
    outcome: TranslationTaskOutcome,
    record: Option<TranslationTaskResponseRecord>,
}

impl ProcessedTranslationTaskResponse {
    fn new(outcome: TranslationTaskOutcome, record: Option<TranslationTaskResponseRecord>) -> Self {
        Self { outcome, record }
    }

    fn into_parts(self) -> (TranslationTaskOutcome, TranslationTaskResponseRecord) {
        (
            self.outcome,
            self.record.expect("recorded 响应处理必须建立任务记录投影"),
        )
    }
}

/// 已开启任务记录时，响应处理技术失败及其已建立的 Assistant 投影。
#[derive(Debug)]
pub(crate) struct RecordedTranslationTaskResponseFailure<E> {
    source: E,
    response: TranslationTaskResponseRecord,
}

impl<E> RecordedTranslationTaskResponseFailure<E> {
    fn new(source: E, response: TranslationTaskResponseRecord) -> Self {
        Self { source, response }
    }

    fn into_parts(self) -> (E, TranslationTaskResponseRecord) {
        (self.source, self.response)
    }
}

/// 使用 CPU 根完成有限 JSON 清洗、严格协议校验与译后处理。
pub(crate) struct TranslationTaskResponseProcessingService<C> {
    cpu: C,
    resources: Arc<ResolvedRpgMakerTranslationResources>,
}

impl<C> TranslationTaskResponseProcessingService<C> {
    pub(crate) fn new(cpu: C, resources: Arc<ResolvedRpgMakerTranslationResources>) -> Self {
        Self { cpu, resources }
    }
}

impl<C> TranslationTaskResponseProcessingService<C>
where
    C: CpuTaskExecutor,
{
    async fn process_with_recording(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
        attempt: usize,
    ) -> Result<ProcessedTranslationTaskResponse, TranslationTaskResponseProcessingError<C::Error>>
    {
        let Some(attempt) = NonZeroUsize::new(attempt) else {
            return Err(TranslationTaskResponseProcessingError::InternalInvariant {
                invariant: TranslationInternalInvariant::ResponseAttemptZero {
                    task_index: task.index(),
                },
            });
        };
        let input = ResponseProcessingInput {
            task_index: task.index(),
            language_pair: task.language_pair().clone(),
            expected_outputs: task.expected_outputs().to_vec(),
            attempt,
        };
        let resources = Arc::clone(&self.resources);
        let outcome = self
            .cpu
            .execute(move || process_response(input, response, resources.as_ref(), false))
            .await
            .map_err(TranslationTaskResponseProcessingError::ScheduleCompute)?;
        outcome.map_err(|failure| {
            let (source, _) = failure.into_parts();
            map_response_processing_error(source)
        })
    }
}

impl<C> TranslationTaskResponseProcessor for TranslationTaskResponseProcessingService<C>
where
    C: CpuTaskExecutor,
{
    type Error = TranslationTaskResponseProcessingError<C::Error>;

    async fn process(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
        attempt: usize,
    ) -> Result<TranslationTaskOutcome, Self::Error> {
        self.process_with_recording(task, response, attempt)
            .await
            .map(|processed| processed.outcome)
    }

    async fn process_recorded(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
        attempt: usize,
    ) -> Result<ProcessedTranslationTaskResponse, RecordedTranslationTaskResponseFailure<Self::Error>>
    {
        let raw_assistant = response.content().to_owned();
        let Some(attempt) = NonZeroUsize::new(attempt) else {
            return Err(RecordedTranslationTaskResponseFailure::new(
                TranslationTaskResponseProcessingError::InternalInvariant {
                    invariant: TranslationInternalInvariant::ResponseAttemptZero {
                        task_index: task.index(),
                    },
                },
                TranslationTaskResponseRecord::unprocessed(raw_assistant),
            ));
        };
        let input = ResponseProcessingInput {
            task_index: task.index(),
            language_pair: task.language_pair().clone(),
            expected_outputs: task.expected_outputs().to_vec(),
            attempt,
        };
        let resources = Arc::clone(&self.resources);
        match self
            .cpu
            .execute(move || process_response(input, response, resources.as_ref(), true))
            .await
        {
            Err(source) => Err(RecordedTranslationTaskResponseFailure::new(
                TranslationTaskResponseProcessingError::ScheduleCompute(source),
                TranslationTaskResponseRecord::unprocessed(raw_assistant),
            )),
            Ok(Err(failure)) => {
                let (source, response) = failure.into_parts();
                Err(RecordedTranslationTaskResponseFailure::new(
                    map_response_processing_error(source),
                    response.expect("recorded 响应闭包的技术失败必须保留 Assistant 投影"),
                ))
            }
            Ok(Ok(processed)) => Ok(processed),
        }
    }

    fn task_record_diagnostic(&self, error: &Self::Error) -> Option<SafeDiagnostic> {
        Some(response_processing_task_record_diagnostic(error))
    }

    fn is_cancelled_wait(&self, error: &Self::Error) -> bool {
        matches!(
            error,
            TranslationTaskResponseProcessingError::ScheduleCompute(
                CpuTaskExecutionError::Cancelled
            )
        )
    }
}

fn response_processing_task_record_diagnostic<C>(
    error: &TranslationTaskResponseProcessingError<C>,
) -> SafeDiagnostic {
    match error {
        TranslationTaskResponseProcessingError::ScheduleCompute(source) => {
            let failure = match source {
                CpuTaskExecutionError::Cancelled => DiagnosticFailureKind::LockCancelled,
                CpuTaskExecutionError::Unavailable(_) => DiagnosticFailureKind::ExecutorClosed,
                CpuTaskExecutionError::TaskPanicked => DiagnosticFailureKind::WorkerPanicked,
            };
            SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                DiagnosticStage::Translate,
                DiagnosticSubject::component("translation_response_cpu_task"),
                DiagnosticReason::failure(failure),
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::ReportBug,
            )
        }
        TranslationTaskResponseProcessingError::LanguageProjection(source) => SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::component("translation_response_placeholder_projection"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                language_projection_detail(source),
            ),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        ),
        TranslationTaskResponseProcessingError::LanguageModule(source) => SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::component("translation_response_language_analysis"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                source.safe_diagnostic_detail(),
            ),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        ),
        TranslationTaskResponseProcessingError::LanguageRepair(source) => SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::component("translation_response_language_repair"),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                language_repair_detail(source),
            ),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        ),
        TranslationTaskResponseProcessingError::InternalInvariant { invariant } => invariant
            .safe_diagnostic(
                DiagnosticStage::Translate,
                DiagnosticImpact::ProgressPreserved,
            ),
    }
}

fn map_response_processing_error<C>(
    error: TranslationResponseTechnicalError,
) -> TranslationTaskResponseProcessingError<C> {
    match error {
        TranslationResponseTechnicalError::LanguageModule(source) => {
            TranslationTaskResponseProcessingError::LanguageModule(source)
        }
        TranslationResponseTechnicalError::LanguageProjection(source) => {
            TranslationTaskResponseProcessingError::LanguageProjection(source)
        }
        TranslationResponseTechnicalError::LanguageRepair(source) => {
            TranslationTaskResponseProcessingError::LanguageRepair(source)
        }
        TranslationResponseTechnicalError::InternalInvariant { invariant } => {
            TranslationTaskResponseProcessingError::InternalInvariant { invariant }
        }
    }
}

/// 一个响应无法继续处理的技术错误。
#[derive(Debug)]
pub(crate) enum TranslationTaskResponseProcessingError<C> {
    ScheduleCompute(CpuTaskExecutionError<C>),
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

impl<C> fmt::Display for TranslationTaskResponseProcessingError<C>
where
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleCompute(source) => write!(formatter, "调度译后 CPU 验收失败：{source}"),
            Self::LanguageModule(source) => write!(formatter, "译后语言事实不一致：{source}"),
            Self::LanguageProjection(source) => write!(formatter, "译后语言投影失败：{source}"),
            Self::LanguageRepair(source) => write!(formatter, "译后语言修复无法安全应用：{source}"),
            Self::InternalInvariant { invariant } => {
                write!(formatter, "翻译任务内部不变量已破坏：{invariant}")
            }
        }
    }
}

impl<C> Error for TranslationTaskResponseProcessingError<C>
where
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleCompute(source) => Some(source),
            Self::LanguageModule(source) => Some(source),
            Self::LanguageProjection(source) => Some(source),
            Self::LanguageRepair(source) => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

impl<C> SafeDiagnosticSource for TranslationTaskResponseProcessingError<C>
where
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::ScheduleCompute(CpuTaskExecutionError::TaskPanicked) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("translation_response_cpu_task"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::WorkerPanicked,
                    "response_processing_task_panicked",
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            Self::ScheduleCompute(source) => source
                .safe_diagnostic_source(stage, impact, DiagnosticAction::Retry)
                .with_recovery(RecoveryFact::component("process_translation_response")),
            Self::LanguageProjection(source) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("translation_response_placeholder_projection"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    language_projection_detail(source),
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            Self::LanguageModule(source) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("translation_response_language_analysis"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    source.safe_diagnostic_detail(),
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            Self::LanguageRepair(source) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("translation_response_language_repair"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    language_repair_detail(source),
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            Self::InternalInvariant { invariant } => invariant.safe_diagnostic(stage, impact),
        }
    }
}

pub(super) fn language_projection_detail(source: &LanguageTextProjectionError) -> String {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            "placeholder_token_index_construction_failed".to_owned()
        }
        LanguageTextProjectionError::EmptyToken => "empty_placeholder_token".to_owned(),
        LanguageTextProjectionError::MissingToken { .. } => {
            "missing_required_placeholder_token".to_owned()
        }
        LanguageTextProjectionError::RepeatedToken { .. } => {
            "repeated_placeholder_token".to_owned()
        }
        LanguageTextProjectionError::OverlappingToken { .. } => {
            "overlapping_placeholder_tokens".to_owned()
        }
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            format!("language_repair_changed_segment_count; expected={expected}; actual={actual}")
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            format!("language_repair_changed_segment_kind; segment_index={segment_index}")
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            format!("missing_ordered_placeholder_token; segment_index={segment_index}")
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            "unused_ordered_placeholder_token".to_owned()
        }
    }
}

fn language_repair_detail(source: &LanguageRepairApplicationError) -> String {
    match source {
        LanguageRepairApplicationError::InvalidNaturalSegment { segment_index } => {
            format!("repair_targets_non_natural_segment; segment_index={segment_index}")
        }
        LanguageRepairApplicationError::DuplicatePosition {
            segment_index,
            byte_offset,
        } => format!(
            "duplicate_repair_position; segment_index={segment_index}; byte_offset={byte_offset}"
        ),
        LanguageRepairApplicationError::InvalidCharacterBoundary {
            segment_index,
            byte_offset,
        } => format!(
            "repair_position_not_character_boundary; segment_index={segment_index}; byte_offset={byte_offset}"
        ),
        LanguageRepairApplicationError::MissingCharacter {
            segment_index,
            byte_offset,
        } => format!(
            "repair_position_has_no_character; segment_index={segment_index}; byte_offset={byte_offset}"
        ),
        LanguageRepairApplicationError::UnexpectedCharacter {
            segment_index,
            byte_offset,
            ..
        } => format!(
            "repair_expected_character_mismatch; segment_index={segment_index}; byte_offset={byte_offset}"
        ),
    }
}

#[derive(Debug)]
enum TranslationResponseTechnicalError {
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

struct TranslationResponseTechnicalFailure {
    source: Box<TranslationResponseTechnicalError>,
    response: Option<Box<TranslationTaskResponseRecord>>,
}

impl TranslationResponseTechnicalFailure {
    fn new(
        source: TranslationResponseTechnicalError,
        response: Option<TranslationTaskResponseRecord>,
    ) -> Self {
        Self {
            source: Box::new(source),
            response: response.map(Box::new),
        }
    }

    fn into_parts(
        self,
    ) -> (
        TranslationResponseTechnicalError,
        Option<TranslationTaskResponseRecord>,
    ) {
        (*self.source, self.response.map(|response| *response))
    }
}

struct TranslationTaskEvidenceBuilder {
    started_at: Option<OffsetDateTime>,
    task_started: Option<Instant>,
    attempt_count: usize,
    attempts: Vec<TranslationTaskAttemptRecord>,
}

impl TranslationTaskEvidenceBuilder {
    fn new(recording: bool) -> Self {
        Self {
            started_at: recording.then(OffsetDateTime::now_utc),
            task_started: recording.then(Instant::now),
            attempt_count: 0,
            attempts: Vec::new(),
        }
    }

    const fn recording(&self) -> bool {
        self.task_started.is_some()
    }

    fn begin_attempt(&mut self, attempt: NonZeroUsize) {
        self.attempt_count = self.attempt_count.max(attempt.get());
    }

    fn record(&mut self, build: impl FnOnce() -> TranslationTaskAttemptRecord) {
        if self.recording() {
            self.attempts.push(build());
        }
    }

    fn push(&mut self, record: Option<TranslationTaskAttemptRecord>) {
        if let Some(record) = record {
            self.attempts.push(record);
        }
    }

    fn attempt_started(&self) -> Option<Instant> {
        self.recording().then(Instant::now)
    }

    fn attempt_duration(started: Option<Instant>) -> Duration {
        started.map_or(Duration::ZERO, |started| started.elapsed())
    }

    fn finish(
        self,
        response: Option<TranslationTaskResponseRecord>,
    ) -> TranslationTaskExecutionEvidence {
        TranslationTaskExecutionEvidence::from_execution(
            self.started_at,
            self.task_started,
            self.attempt_count,
            self.attempts,
            response,
        )
    }
}

/// 使用根 LLM、根 Delay 和真实 ResponseProcessor 执行一个 TaskBlock。
pub(crate) struct RpgMakerStandardTranslationTaskExecutionService<L, D, R, P> {
    llm: L,
    delay: D,
    response_processor: R,
    cancellation: CooperativeCancellation,
    record_task_response: bool,
    profile: PhantomData<fn() -> P>,
}

impl<L, D, R, P> RpgMakerStandardTranslationTaskExecutionService<L, D, R, P> {
    pub(crate) fn new(
        llm: L,
        delay: D,
        response_processor: R,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            llm,
            delay,
            response_processor,
            cancellation,
            record_task_response: true,
            profile: PhantomData,
        }
    }

    /// 关闭高级任务记录时跳过原始 Assistant 与逐条 JSON 的旁路复制。
    pub(crate) fn with_task_recording(mut self, enabled: bool) -> Self {
        self.record_task_response = enabled;
        self
    }
}

impl<L, D, R, P> StandardTranslationTaskExecutor
    for RpgMakerStandardTranslationTaskExecutionService<L, D, R, P>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay,
    R: TranslationTaskResponseProcessor,
    P: TranslationTaskExecutionProfile<LlmClient = L::Client>,
{
    type Profile = P;
    type Error = RpgMakerStandardTranslationTaskExecutionError<L::Error, R::Error>;

    async fn execute(
        &self,
        profile: &Self::Profile,
        task: &TranslationTaskBlock,
    ) -> Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>> {
        let mut evidence = TranslationTaskEvidenceBuilder::new(self.record_task_response);
        if task.expected_outputs().is_empty() {
            let invariant = TranslationInternalInvariant::ExpectedOutputsEmpty {
                task_index: task.index(),
            };
            let diagnostic =
                invariant.safe_diagnostic(DiagnosticStage::Translate, DiagnosticImpact::Unchanged);
            return Err(TranslationTaskExecutionFailure::new(
                RpgMakerStandardTranslationTaskExecutionError::InternalInvariant { invariant },
                evidence.finish(None),
                Some(diagnostic),
                false,
            ));
        }
        let mut attempt = NonZeroUsize::MIN;
        let mut retry_delays = profile.network_retry_delays().iter().copied();
        let mut completed_retry_wait = None;

        loop {
            if self.cancellation.is_requested() {
                if completed_retry_wait.is_some() {
                    evidence.push(completed_retry_wait.take());
                } else {
                    evidence.begin_attempt(attempt);
                    evidence.record(|| {
                        TranslationTaskAttemptRecord::cancelled(attempt, Duration::ZERO)
                    });
                }
                return Err(TranslationTaskExecutionFailure::new(
                    RpgMakerStandardTranslationTaskExecutionError::RetryWaitCancelled {
                        attempt: attempt.get(),
                    },
                    evidence.finish(None),
                    None,
                    true,
                ));
            }
            if let Some(mut completed_retry_wait) = completed_retry_wait.take() {
                completed_retry_wait.mark_retry_started();
                evidence.push(Some(completed_retry_wait));
            }
            evidence.begin_attempt(attempt);
            let attempt_started = evidence.attempt_started();
            let response = match self
                .llm
                .request(profile.llm_client(), task.messages())
                .await
            {
                Ok(response) => response,
                Err(LlmRequestError::Fatal(source)) => {
                    let cancelled = self.cancellation.is_requested() && source.is_cancelled_wait();
                    let diagnostic = (!cancelled).then(|| {
                        source.request_diagnostic(None, DiagnosticImpact::ProgressPreserved)
                    });
                    if let Some(diagnostic) = &diagnostic {
                        evidence.record(|| {
                            TranslationTaskAttemptRecord::failed(
                                attempt,
                                TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                                diagnostic.clone(),
                            )
                        });
                    } else {
                        evidence.record(|| {
                            TranslationTaskAttemptRecord::cancelled(
                                attempt,
                                TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                            )
                        });
                    }
                    return Err(TranslationTaskExecutionFailure::new(
                        RpgMakerStandardTranslationTaskExecutionError::FatalRequest {
                            attempt: attempt.get(),
                            source,
                        },
                        evidence.finish(None),
                        diagnostic,
                        cancelled,
                    ));
                }
                Err(LlmRequestError::Retryable {
                    source,
                    retry_after,
                }) => {
                    if self.cancellation.is_requested() && source.is_cancelled_wait() {
                        evidence.record(|| {
                            TranslationTaskAttemptRecord::cancelled(
                                attempt,
                                TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                            )
                        });
                        return Err(TranslationTaskExecutionFailure::new(
                            RpgMakerStandardTranslationTaskExecutionError::FatalRequest {
                                attempt: attempt.get(),
                                source,
                            },
                            evidence.finish(None),
                            None,
                            true,
                        ));
                    }
                    let diagnostic =
                        source.request_diagnostic(retry_after, DiagnosticImpact::ProgressPreserved);
                    if let Some(retry_after) = retry_after
                        && retry_after > profile.max_network_retry_after()
                    {
                        evidence.record(|| {
                            TranslationTaskAttemptRecord::retryable(
                                attempt,
                                TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                                diagnostic.clone(),
                                Some(retry_after),
                                None,
                            )
                        });
                        let outcome = unavailable_after_request_failure(
                            task,
                            attempt,
                            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                                retry_after,
                                maximum: profile.max_network_retry_after(),
                                diagnostic: diagnostic.with_recovery(RecoveryFact::component(
                                    format!(
                                        "configured_max_retry_after_seconds={}",
                                        profile.max_network_retry_after().as_secs()
                                    ),
                                )),
                            },
                        );
                        return Ok(TranslationTaskExecution::new(
                            outcome,
                            evidence.finish(None),
                        ));
                    }
                    let Some(configured_delay) = retry_delays.next() else {
                        evidence.record(|| {
                            TranslationTaskAttemptRecord::retryable(
                                attempt,
                                TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                                diagnostic.clone(),
                                retry_after,
                                None,
                            )
                        });
                        let outcome = unavailable_after_request_failure(
                            task,
                            attempt,
                            TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                                diagnostic,
                            },
                        );
                        return Ok(TranslationTaskExecution::new(
                            outcome,
                            evidence.finish(None),
                        ));
                    };
                    let delay = configured_delay.max(retry_after.unwrap_or_default());
                    let attempt_duration =
                        TranslationTaskEvidenceBuilder::attempt_duration(attempt_started);
                    let mut diagnostic = Some(diagnostic);
                    let waiting = self.delay.wait(delay);
                    tokio::pin!(waiting);
                    let cancelled = self.cancellation.cancelled();
                    tokio::pin!(cancelled);
                    tokio::select! {
                        biased;
                        () = &mut cancelled => {
                            evidence.record(|| {
                                TranslationTaskAttemptRecord::retryable(
                                    attempt,
                                    attempt_duration,
                                    diagnostic.take().expect("可重试诊断必须只移动一次"),
                                    retry_after,
                                    Some(TranslationTaskRetryWaitRecord::CancelledWhileWaiting {
                                        planned_duration: delay,
                                    }),
                                )
                            });
                            return Err(TranslationTaskExecutionFailure::new(
                                RpgMakerStandardTranslationTaskExecutionError::RetryWaitCancelled {
                                    attempt: attempt.get(),
                                },
                                evidence.finish(None),
                                None,
                                true,
                            ));
                        }
                        () = &mut waiting => {
                            completed_retry_wait = evidence.recording().then(|| {
                                TranslationTaskAttemptRecord::retryable(
                                    attempt,
                                    attempt_duration,
                                    diagnostic.take().expect("可重试诊断必须只移动一次"),
                                    retry_after,
                                    Some(
                                        TranslationTaskRetryWaitRecord::CompletedBeforeNextAttempt {
                                            duration: delay,
                                        },
                                    ),
                                )
                            });
                        }
                    }
                    if self.cancellation.is_requested() {
                        evidence.push(completed_retry_wait.take());
                        return Err(TranslationTaskExecutionFailure::new(
                            RpgMakerStandardTranslationTaskExecutionError::RetryWaitCancelled {
                                attempt: attempt.get(),
                            },
                            evidence.finish(None),
                            None,
                            true,
                        ));
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };

            evidence.record(|| {
                TranslationTaskAttemptRecord::succeeded(
                    attempt,
                    TranslationTaskEvidenceBuilder::attempt_duration(attempt_started),
                    &response,
                )
            });
            if !self.record_task_response {
                let processed = self
                    .response_processor
                    .process(task, response, attempt.get())
                    .await;
                return match processed {
                    Ok(outcome) => Ok(TranslationTaskExecution::new(
                        outcome,
                        evidence.finish(None),
                    )),
                    Err(source) => {
                        let cancelled = self.cancellation.is_requested()
                            && self.response_processor.is_cancelled_wait(&source);
                        let diagnostic = (!cancelled)
                            .then(|| self.response_processor.task_record_diagnostic(&source))
                            .flatten();
                        Err(TranslationTaskExecutionFailure::new(
                            RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                                attempt: attempt.get(),
                                source,
                            },
                            evidence.finish(None),
                            diagnostic,
                            cancelled,
                        ))
                    }
                };
            }

            let processed = self
                .response_processor
                .process_recorded(task, response, attempt.get())
                .await;
            /*
             * 下面的显式 match 让 attempt 证据只移动到唯一分支，避免为了旁路记录
             * 克隆完整模型正文。
             */
            return match processed {
                Ok(processed) => {
                    let (outcome, response) = processed.into_parts();
                    Ok(TranslationTaskExecution::new(
                        outcome,
                        evidence.finish(Some(response)),
                    ))
                }
                Err(failure) => {
                    let (source, response) = failure.into_parts();
                    let cancelled = self.cancellation.is_requested()
                        && self.response_processor.is_cancelled_wait(&source);
                    let diagnostic = (!cancelled)
                        .then(|| self.response_processor.task_record_diagnostic(&source))
                        .flatten();
                    Err(TranslationTaskExecutionFailure::new(
                        RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                            attempt: attempt.get(),
                            source,
                        },
                        evidence.finish(Some(response)),
                        diagnostic,
                        cancelled,
                    ))
                }
            };
        }
    }
}

#[cfg(test)]
impl<L, D, R, P> RpgMakerStandardTranslationTaskExecutionService<L, D, R, P>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay,
    R: TranslationTaskResponseProcessor,
    P: TranslationTaskExecutionProfile<LlmClient = L::Client>,
{
    /// 既有 Executor 单元测试只关心权威业务结果；任务记录证据由专门测试覆盖。
    async fn execute(
        &self,
        profile: &P,
        task: TranslationTaskBlock,
    ) -> Result<
        TranslationTaskOutcome,
        RpgMakerStandardTranslationTaskExecutionError<L::Error, R::Error>,
    > {
        match <Self as StandardTranslationTaskExecutor>::execute(self, profile, &task).await {
            Ok(execution) => Ok(execution.into_parts().0),
            Err(failure) => Err(failure.into_parts().0),
        }
    }
}

fn unavailable_after_request_failure(
    task: &TranslationTaskBlock,
    attempts: NonZeroUsize,
    reason: TranslationTaskUnavailableReason,
) -> TranslationTaskOutcome {
    TranslationTaskOutcome::Unavailable {
        context: TranslationTaskOutcomeContext::new(task.index(), attempts, Vec::new()),
        final_response: None,
        reason,
        unresolved: non_empty_known(
            unresolved_all(
                task.expected_outputs(),
                TranslationUnitRejectionReason::Missing,
            ),
            "Executor 已确认任务含有预期输出",
        ),
    }
}

/// 单任务模型执行失败。
#[derive(Debug)]
pub(crate) enum RpgMakerStandardTranslationTaskExecutionError<L, R> {
    FatalRequest {
        attempt: usize,
        source: L,
    },
    ProcessResponse {
        attempt: usize,
        source: R,
    },
    RetryWaitCancelled {
        attempt: usize,
    },
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

impl<L, R> fmt::Display for RpgMakerStandardTranslationTaskExecutionError<L, R>
where
    L: fmt::Display,
    R: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FatalRequest { attempt, source } => {
                write!(formatter, "第 {attempt} 次 LLM 请求不可重试：{source}")
            }
            Self::ProcessResponse { attempt, source } => {
                write!(formatter, "第 {attempt} 次模型响应无法处理：{source}")
            }
            Self::RetryWaitCancelled { attempt } => {
                write!(formatter, "第 {attempt} 次 LLM 请求后的重试等待已取消")
            }
            Self::InternalInvariant { invariant } => {
                write!(formatter, "翻译任务内部不变量已破坏：{invariant}")
            }
        }
    }
}

impl<L, R> Error for RpgMakerStandardTranslationTaskExecutionError<L, R>
where
    L: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FatalRequest { source, .. } => Some(source),
            Self::ProcessResponse { source, .. } => Some(source),
            Self::RetryWaitCancelled { .. } | Self::InternalInvariant { .. } => None,
        }
    }
}

struct ResponseProcessingInput {
    task_index: StandardTranslationTaskIndex,
    language_pair: LanguagePair,
    expected_outputs: Vec<ExpectedTranslationOutput>,
    attempt: NonZeroUsize,
}

fn process_response(
    input: ResponseProcessingInput,
    response: LlmResponse,
    resources: &ResolvedRpgMakerTranslationResources,
    record_response: bool,
) -> Result<ProcessedTranslationTaskResponse, TranslationResponseTechnicalFailure> {
    let language_module = resources.source_language();

    let final_response = FinalLlmResponseMetadata::from_response(&response);
    let finish_reason = final_response.finish_reason().to_owned();
    let mut diagnostics = Vec::new();
    if response.finish_reason() != &LlmFinishReason::Stop {
        diagnostics.push(TranslationProtocolDiagnostic::NonStopFinish {
            reason: finish_reason.clone(),
        });
    }

    let raw_assistant = record_response.then(|| response.content().to_owned());
    let parsed = parse_model_response(
        response.content(),
        resources.system_prompt().response_envelope(),
    );
    let response_record = raw_assistant.map(|raw_assistant| match &parsed {
        Ok(parsed) => TranslationTaskResponseRecord::parsed(
            raw_assistant,
            parsed.thinking.clone(),
            parsed
                .outputs
                .iter()
                .map(|output| {
                    TranslationAssistantEntry::projected(
                        output.id.clone(),
                        output.value.clone(),
                        output.canonical_id,
                        output.translation.as_ref().err().copied(),
                    )
                })
                .collect(),
        ),
        Err(parse_error) => TranslationTaskResponseRecord::invalid(raw_assistant, *parse_error),
    });

    if input.expected_outputs.is_empty() {
        return Err(TranslationResponseTechnicalFailure::new(
            TranslationResponseTechnicalError::InternalInvariant {
                invariant: TranslationInternalInvariant::ExpectedOutputsEmpty {
                    task_index: input.task_index,
                },
            },
            response_record,
        ));
    }
    let resolved_pair = resources.language_pair();
    if &input.language_pair != resolved_pair {
        return Err(TranslationResponseTechnicalFailure::new(
            TranslationResponseTechnicalError::InternalInvariant {
                invariant: TranslationInternalInvariant::LanguagePairMismatch {
                    task_index: input.task_index,
                    task_source: input.language_pair.source().clone(),
                    task_target: input.language_pair.target().clone(),
                    resolved_source: resolved_pair.source().clone(),
                    resolved_target: resolved_pair.target().clone(),
                },
            },
            response_record,
        ));
    }

    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(parse_error) => {
            let message = parse_error.business_message();
            diagnostics.push(TranslationProtocolDiagnostic::InvalidResponse {
                message: message.clone(),
            });
            let outcome = TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    input.task_index,
                    input.attempt,
                    diagnostics,
                ),
                final_response: Some(final_response),
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                unresolved: non_empty_known(
                    unresolved_all(
                        &input.expected_outputs,
                        TranslationUnitRejectionReason::InvalidShape {
                            message: message.clone(),
                        },
                    ),
                    "Executor 已确认任务含有预期输出",
                ),
            };
            return Ok(ProcessedTranslationTaskResponse::new(
                outcome,
                response_record,
            ));
        }
    };
    let outputs = parsed.outputs;

    let expected_by_id = input
        .expected_outputs
        .iter()
        .map(|output| (output.id(), output))
        .collect::<BTreeMap<_, _>>();
    let actual_by_id = collect_model_outputs(outputs, &expected_by_id, &mut diagnostics);

    let mut accepted = Vec::with_capacity(expected_by_id.len());
    let mut unresolved = Vec::new();
    for expected in &input.expected_outputs {
        let Some(candidates) = actual_by_id.get(&expected.id()) else {
            unresolved.push(unresolved_unit(
                expected,
                TranslationUnitRejectionReason::Missing,
            ));
            continue;
        };
        if candidates.len() != 1 {
            unresolved.push(unresolved_unit(
                expected,
                TranslationUnitRejectionReason::Duplicate,
            ));
            continue;
        }
        let translation_lines = match &candidates[0] {
            Ok(lines) => lines.clone(),
            Err(message) => {
                unresolved.push(unresolved_unit(
                    expected,
                    TranslationUnitRejectionReason::InvalidShape {
                        message: message.clone(),
                    },
                ));
                continue;
            }
        };
        let target_constraints = TranslationTargetConstraints::from_identities(
            std::iter::once(expected.identity()).chain(expected.propagation_targets().iter()),
        );
        let translation = match accept_translation_lines_candidate_at(
            expected.identity(),
            target_constraints,
            expected.protected_text(),
            expected.line_shape(),
            expected.applied_placeholders(),
            expected.placeholder_bindings(),
            expected.language_analysis(),
            language_module.as_ref(),
            translation_lines,
            TranslationCandidateInvariantLocation::TaskUnit {
                task_index: input.task_index,
                unit_id: expected.id(),
            },
        ) {
            Ok(TranslationContentAcceptance::Accepted(translation)) => translation,
            Ok(TranslationContentAcceptance::Rejected(reason)) => {
                unresolved.push(unresolved_unit(expected, reason));
                continue;
            }
            Err(TranslationCandidateTechnicalError::LanguageModule(source)) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::LanguageModule(source),
                    response_record,
                ));
            }
            Err(TranslationCandidateTechnicalError::LanguageProjection(source)) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::LanguageProjection(source),
                    response_record,
                ));
            }
            Err(TranslationCandidateTechnicalError::LanguageRepair(source)) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::LanguageRepair(source),
                    response_record,
                ));
            }
            Err(TranslationCandidateTechnicalError::InternalInvariant { invariant }) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::InternalInvariant { invariant },
                    response_record,
                ));
            }
        };
        let translation_state = expected.state_context().finish(&translation);
        let propagation_targets = expected
            .propagation_targets()
            .iter()
            .cloned()
            .zip(expected.propagation_state_contexts().iter().copied())
            .map(|(identity, state_context)| {
                super::standard::TranslationPropagationTarget::new(identity, state_context)
            })
            .collect();
        accepted.push(AcceptedTranslationDecision::new(
            expected.id(),
            TranslationPatch::new(
                expected.identity().clone(),
                propagation_targets,
                translation,
                translation_state,
            ),
        ));
    }

    let outcome = if unresolved.is_empty() {
        TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response,
            accepted: non_empty_known(accepted, "所有预期输出已验收"),
        }
    } else if accepted.is_empty() {
        TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response: Some(final_response),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: non_empty_known(unresolved, "没有输出通过验收"),
        }
    } else {
        TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response,
            accepted: non_empty_known(accepted, "部分输出已验收"),
            unresolved: non_empty_known(unresolved, "部分输出未完成"),
        }
    };
    Ok(ProcessedTranslationTaskResponse::new(
        outcome,
        response_record,
    ))
}

fn non_empty_known<T>(items: Vec<T>, established_by: &'static str) -> NonEmptyTaskItems<T> {
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        unreachable!("{established_by}");
    };
    NonEmptyTaskItems::new(first, items.collect())
}

fn validate_translation_lines(
    identity: &super::standard::TranslationUnitIdentity,
    target_constraints: TranslationTargetConstraints,
    shape: ExpectedLineShape,
    lines: &[String],
) -> Result<(), TranslationUnitRejectionReason> {
    if let ExpectedLineShape::Aligned(expected) = shape
        && lines.len() != expected.get()
    {
        return Err(TranslationUnitRejectionReason::LineCountMismatch {
            expected: expected.get(),
            actual: lines.len(),
        });
    }
    if let Err(error) = validate_text_unit_lines(lines) {
        let TextUnitContentStructureError::InvalidText { line_index } = error else {
            unreachable!("物理行校验不执行内容形状判断");
        };
        return Err(TranslationUnitRejectionReason::InvalidLineText { line_index });
    }
    if target_constraints.targets_tag_value()
        && let Some(line_index) = lines.iter().position(|line| line.contains('>'))
    {
        return Err(
            TranslationUnitRejectionReason::TagValueContainsClosingDelimiter { line_index },
        );
    }
    match shape {
        ExpectedLineShape::Reflow => {
            if lines.iter().all(|line| line.trim().is_empty()) {
                return Err(TranslationUnitRejectionReason::BlankTranslation);
            }
        }
        ExpectedLineShape::Aligned(_) => {
            let source_lines = match identity.source_content() {
                TextUnitContent::Value(value) => std::slice::from_ref(value),
                TextUnitContent::Lines(lines) => lines.as_slice(),
            };
            if let Some((line_index, expected_blank)) =
                source_lines.iter().zip(lines).enumerate().find_map(
                    |(line_index, (source, translation))| {
                        let expected_blank = source.trim().is_empty();
                        let mismatched = if expected_blank {
                            !translation.is_empty()
                        } else {
                            translation.trim().is_empty()
                        };
                        mismatched.then_some((line_index, expected_blank))
                    },
                )
            {
                return Err(TranslationUnitRejectionReason::BlankLineMismatch {
                    line_index,
                    expected_blank,
                });
            }
        }
    }
    Ok(())
}

fn translation_content(
    identity: &super::standard::TranslationUnitIdentity,
    lines: Vec<String>,
) -> TextUnitContent {
    match identity.source_content() {
        TextUnitContent::Value(_) => TextUnitContent::Value(lines.join("\n")),
        TextUnitContent::Lines(_) => TextUnitContent::Lines(lines),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationContentAcceptance {
    Accepted(TextUnitContent),
    Rejected(TranslationUnitRejectionReason),
}

/// 按 Standard 模型响应使用的同一条完整规则验收一个结构化候选译文。
///
/// `Value` 与 `Lines` 的物理边界由调用方提交的 `TextUnitContent` 明确表达；
/// 本函数不会用 LF 猜测或转换两种形状。
#[allow(clippy::too_many_arguments)]
pub(crate) fn accept_translation_content_candidate(
    identity: &super::standard::TranslationUnitIdentity,
    target_constraints: TranslationTargetConstraints,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    candidate: TextUnitContent,
) -> Result<TranslationContentAcceptance, TranslationCandidateTechnicalError> {
    accept_translation_content_candidate_at(
        identity,
        target_constraints,
        protected_text,
        line_shape,
        placeholders,
        language_analysis,
        language_module,
        candidate,
        TranslationCandidateInvariantLocation::PreparedCandidate,
    )
}

#[allow(clippy::too_many_arguments)]
fn accept_translation_content_candidate_at(
    identity: &super::standard::TranslationUnitIdentity,
    target_constraints: TranslationTargetConstraints,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    candidate: TextUnitContent,
    invariant_location: TranslationCandidateInvariantLocation,
) -> Result<TranslationContentAcceptance, TranslationCandidateTechnicalError> {
    if let Err(error) =
        validate_text_unit_content_structure(identity.role(), TextUnitContentView::from(&candidate))
    {
        return Ok(TranslationContentAcceptance::Rejected(match error {
            TextUnitContentStructureError::ShapeMismatch => {
                TranslationUnitRejectionReason::InvalidShape {
                    message: match candidate {
                        TextUnitContent::Value(_) => "expected=lines; actual=value".to_owned(),
                        TextUnitContent::Lines(_) => "expected=value; actual=lines".to_owned(),
                    },
                }
            }
            TextUnitContentStructureError::InvalidText { line_index } => {
                TranslationUnitRejectionReason::InvalidLineText { line_index }
            }
        }));
    }
    let lines = match (identity.source_content(), candidate) {
        (TextUnitContent::Value(_), TextUnitContent::Value(value)) => {
            value.split('\n').map(str::to_owned).collect::<Vec<_>>()
        }
        (TextUnitContent::Lines(_), TextUnitContent::Lines(lines)) => lines,
        (TextUnitContent::Value(_), TextUnitContent::Lines(_)) => {
            return Ok(TranslationContentAcceptance::Rejected(
                TranslationUnitRejectionReason::InvalidShape {
                    message: "expected=value; actual=lines".to_owned(),
                },
            ));
        }
        (TextUnitContent::Lines(_), TextUnitContent::Value(_)) => {
            return Ok(TranslationContentAcceptance::Rejected(
                TranslationUnitRejectionReason::InvalidShape {
                    message: "expected=lines; actual=value".to_owned(),
                },
            ));
        }
    };
    let placeholder_bindings = PlaceholderBindingIndex::new(placeholders)
        .map_err(TranslationCandidateTechnicalError::LanguageProjection)?;
    accept_translation_lines_candidate_at(
        identity,
        target_constraints,
        protected_text,
        line_shape,
        placeholders,
        &placeholder_bindings,
        language_analysis,
        language_module,
        lines,
        invariant_location,
    )
}

#[allow(clippy::too_many_arguments)]
fn accept_translation_lines_candidate_at(
    identity: &super::standard::TranslationUnitIdentity,
    target_constraints: TranslationTargetConstraints,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    placeholder_bindings: &PlaceholderBindingIndex,
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    lines: Vec<String>,
    invariant_location: TranslationCandidateInvariantLocation,
) -> Result<TranslationContentAcceptance, TranslationCandidateTechnicalError> {
    if let Err(reason) =
        validate_translation_lines(identity, target_constraints, line_shape, &lines)
    {
        return Ok(TranslationContentAcceptance::Rejected(reason));
    }
    match validate_and_restore_translation_lines_at(
        lines,
        protected_text,
        line_shape,
        placeholders,
        placeholder_bindings,
        language_analysis,
        language_module,
        invariant_location,
    ) {
        Ok(lines) => Ok(TranslationContentAcceptance::Accepted(translation_content(
            identity, lines,
        ))),
        Err(TranslationCandidateValidationError::Rejected(reason)) => {
            Ok(TranslationContentAcceptance::Rejected(reason))
        }
        Err(TranslationCandidateValidationError::LanguageModule(source)) => {
            Err(TranslationCandidateTechnicalError::LanguageModule(source))
        }
        Err(TranslationCandidateValidationError::LanguageProjection(source)) => Err(
            TranslationCandidateTechnicalError::LanguageProjection(source),
        ),
        Err(TranslationCandidateValidationError::LanguageRepair(source)) => {
            Err(TranslationCandidateTechnicalError::LanguageRepair(source))
        }
        Err(TranslationCandidateValidationError::InternalInvariant { invariant }) => {
            Err(TranslationCandidateTechnicalError::InternalInvariant { invariant })
        }
    }
}

#[derive(Clone, Debug)]
struct ModelOutputWire {
    id: String,
    value: serde_json::Value,
}

#[derive(Debug)]
struct ParsedModelOutput {
    id: String,
    value: serde_json::Value,
    canonical_id: Option<usize>,
    translation: Result<Vec<String>, TranslationAssistantValueError>,
}

#[derive(Debug)]
struct ModelOutputBatch(Vec<ModelOutputWire>);

impl<'de> Deserialize<'de> for ModelOutputBatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ModelOutputBatchVisitor)
    }
}

struct ModelOutputBatchVisitor;

impl<'de> Visitor<'de> for ModelOutputBatchVisitor {
    type Value = ModelOutputBatch;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("以正整数 ID 为键、字符串数组为值的 JSON 对象")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut outputs = Vec::with_capacity(map.size_hint().unwrap_or_default());
        while let Some((id, value)) = map.next_entry::<String, serde_json::Value>()? {
            outputs.push(ModelOutputWire { id, value });
        }
        Ok(ModelOutputBatch(outputs))
    }
}

fn collect_model_outputs(
    outputs: Vec<ParsedModelOutput>,
    expected_by_id: &BTreeMap<usize, &ExpectedTranslationOutput>,
    diagnostics: &mut Vec<TranslationProtocolDiagnostic>,
) -> BTreeMap<usize, Vec<Result<Vec<String>, String>>> {
    let mut by_id = BTreeMap::<usize, Vec<Result<Vec<String>, String>>>::new();
    for (item_index, output) in outputs.into_iter().enumerate() {
        let Some(id) = output.canonical_id else {
            diagnostics.push(TranslationProtocolDiagnostic::InvalidId { item_index });
            continue;
        };
        if !expected_by_id.contains_key(&id) {
            diagnostics.push(TranslationProtocolDiagnostic::UnknownId { item_index, id });
            continue;
        }
        by_id.entry(id).or_default().push(
            output
                .translation
                .map_err(TranslationAssistantValueError::business_message),
        );
    }
    by_id
}

fn parse_model_output_id(value: &str) -> Option<usize> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.starts_with('0')
    {
        return None;
    }
    value.parse().ok()
}

fn parse_translation_lines(
    value: &serde_json::Value,
) -> Result<Vec<String>, TranslationAssistantValueError> {
    let serde_json::Value::Array(values) = value else {
        return Err(TranslationAssistantValueError::NotStringArray);
    };
    values
        .iter()
        .enumerate()
        .map(|(line_index, value)| match value {
            serde_json::Value::String(line) => Ok(line.clone()),
            _ => Err(TranslationAssistantValueError::NonStringItem {
                item: NonZeroUsize::new(line_index + 1).expect("一基数组项编号不可能为零"),
            }),
        })
        .collect()
}

fn parse_model_response(
    value: &str,
    response_envelope: TranslationResponseEnvelope,
) -> Result<ParsedModelOutputBatch, TranslationTaskResponseParseError> {
    let value = trim_model_response(value);
    let envelope = parse_translation_response_envelope(value, response_envelope)?;
    let value = strip_single_markdown_fence(envelope.assistant_json.trim())?;
    serde_json::from_str::<ModelOutputBatch>(value.value)
        .map(|batch| ParsedModelOutputBatch {
            thinking: envelope.thinking.map(str::to_owned),
            outputs: batch
                .0
                .into_iter()
                .map(|output| {
                    let canonical_id = parse_model_output_id(&output.id);
                    let translation = parse_translation_lines(&output.value);
                    ParsedModelOutput {
                        id: output.id,
                        value: output.value,
                        canonical_id,
                        translation,
                    }
                })
                .collect(),
        })
        .map_err(|source| {
            let category = match JsonErrorCategory::from(&source) {
                JsonErrorCategory::Io => TranslationTaskResponseJsonErrorCategory::Io,
                JsonErrorCategory::Syntax | JsonErrorCategory::DuplicateObjectKey => {
                    TranslationTaskResponseJsonErrorCategory::Syntax
                }
                JsonErrorCategory::Data => TranslationTaskResponseJsonErrorCategory::Shape,
                JsonErrorCategory::Eof => TranslationTaskResponseJsonErrorCategory::UnexpectedEof,
            };
            let (line, column) = if matches!(
                category,
                TranslationTaskResponseJsonErrorCategory::UnexpectedEof
            ) {
                value.location_at(value.value.len())
            } else {
                value.location_for_local(source.line(), source.column())
            };
            TranslationTaskResponseParseError::new(
                TranslationTaskResponseParseErrorKind::Json(category),
                line,
                column,
            )
        })
}

#[derive(Debug)]
struct ParsedModelOutputBatch {
    thinking: Option<String>,
    outputs: Vec<ParsedModelOutput>,
}

#[cfg(test)]
fn parse_model_output_batch(
    value: &str,
    response_envelope: TranslationResponseEnvelope,
) -> Result<Vec<ParsedModelOutput>, TranslationTaskResponseParseError> {
    parse_model_response(value, response_envelope).map(|parsed| parsed.outputs)
}

fn unresolved_unit(
    expected: &ExpectedTranslationOutput,
    reason: TranslationUnitRejectionReason,
) -> UnresolvedTranslationUnit {
    UnresolvedTranslationUnit::new(
        expected.id(),
        expected.identity().clone(),
        expected.propagation_targets().to_vec(),
        reason,
    )
}

fn unresolved_all(
    expected_outputs: &[ExpectedTranslationOutput],
    reason: TranslationUnitRejectionReason,
) -> Vec<UnresolvedTranslationUnit> {
    expected_outputs
        .iter()
        .map(|expected| unresolved_unit(expected, reason.clone()))
        .collect()
}

#[derive(Clone, Copy)]
struct LocatedModelResponse<'a> {
    raw: &'a str,
    value: &'a str,
    start: usize,
}

impl<'a> LocatedModelResponse<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            value: raw,
            start: 0,
        }
    }

    fn advance(self, bytes: usize) -> Self {
        Self {
            raw: self.raw,
            value: &self.value[bytes..],
            start: self.start + bytes,
        }
    }

    fn prefix(self, bytes: usize) -> Self {
        Self {
            raw: self.raw,
            value: &self.value[..bytes],
            start: self.start,
        }
    }

    fn trim(self) -> Self {
        let leading = self.value.len() - self.value.trim_start().len();
        self.advance(leading).prefix(self.value.trim().len())
    }

    fn trim_start(self) -> Self {
        let leading = self.value.len() - self.value.trim_start().len();
        self.advance(leading)
    }

    fn location_at(self, local_byte_offset: usize) -> (NonZeroUsize, NonZeroUsize) {
        response_location(self.raw, self.start + local_byte_offset)
    }

    fn location_for_local(
        self,
        local_line: usize,
        local_column: usize,
    ) -> (NonZeroUsize, NonZeroUsize) {
        let (start_line, start_column) = response_location(self.raw, self.start);
        let local_line = local_line.max(1);
        let local_column = local_column.max(1);
        if local_line == 1 {
            (
                start_line,
                NonZeroUsize::new(start_column.get() + local_column - 1)
                    .expect("一基列号相加后仍非零"),
            )
        } else {
            (
                NonZeroUsize::new(start_line.get() + local_line - 1).expect("一基行号相加后仍非零"),
                NonZeroUsize::new(local_column).expect("局部列号已收窄为至少一"),
            )
        }
    }

    fn error_at(
        self,
        kind: TranslationTaskResponseParseErrorKind,
        local_byte_offset: usize,
    ) -> TranslationTaskResponseParseError {
        let (line, column) = self.location_at(local_byte_offset);
        TranslationTaskResponseParseError::new(kind, line, column)
    }

    fn error_at_raw_eof(
        self,
        kind: TranslationTaskResponseParseErrorKind,
    ) -> TranslationTaskResponseParseError {
        let (line, column) = response_location(self.raw, self.raw.len());
        TranslationTaskResponseParseError::new(kind, line, column)
    }
}

fn response_location(raw: &str, byte_offset: usize) -> (NonZeroUsize, NonZeroUsize) {
    let byte_offset = byte_offset.min(raw.len());
    let preceding = &raw[..byte_offset];
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = preceding
        .rsplit_once('\n')
        .map_or(preceding.len(), |(_, tail)| tail.len())
        + 1;
    (
        NonZeroUsize::new(line).expect("一基行号不可能为零"),
        NonZeroUsize::new(column).expect("一基列号不可能为零"),
    )
}

/// 清理响应两端，并兼容供应商可能放在正文开头的单个 BOM。
fn trim_model_response(value: &str) -> LocatedModelResponse<'_> {
    let value = LocatedModelResponse::new(value).trim();
    let value = if value.value.starts_with('\u{feff}') {
        value.advance('\u{feff}'.len_utf8())
    } else {
        value
    };
    value.trim()
}

/// 按受信 Prompt 声明剥离唯一响应信封，不修复或提取 JSON 内容。
fn parse_translation_response_envelope(
    value: LocatedModelResponse<'_>,
    response_envelope: TranslationResponseEnvelope,
) -> Result<TranslationResponseEnvelopeParts<'_>, TranslationTaskResponseParseError> {
    match response_envelope {
        TranslationResponseEnvelope::JsonOnly => {
            if starts_with_thinking_tag(value.value) {
                return Err(
                    value.error_at(TranslationTaskResponseParseErrorKind::ThinkingNotAllowed, 0)
                );
            }
            Ok(TranslationResponseEnvelopeParts {
                thinking: None,
                assistant_json: value,
            })
        }
        TranslationResponseEnvelope::ThinkingThenJson => parse_thinking_then_json(value),
    }
}

struct TranslationResponseEnvelopeParts<'a> {
    thinking: Option<&'a str>,
    assistant_json: LocatedModelResponse<'a>,
}

fn parse_thinking_then_json(
    value: LocatedModelResponse<'_>,
) -> Result<TranslationResponseEnvelopeParts<'_>, TranslationTaskResponseParseError> {
    let Some(after_opening) = value.value.strip_prefix("<why>") else {
        return Err(value.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingEnvelopeMissing,
            0,
        ));
    };
    let after_opening = value.advance(value.value.len() - after_opening.len());
    let Some(closing_start) = after_opening.value.find("</why>") else {
        return Err(
            value.error_at_raw_eof(TranslationTaskResponseParseErrorKind::ThinkingEnvelopeUnclosed)
        );
    };
    let thinking = after_opening.prefix(closing_start);
    if thinking.value.trim().is_empty() {
        return Err(after_opening.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingEmpty,
            closing_start,
        ));
    }
    if let Some(offending) = first_thinking_tag(thinking.value) {
        return Err(thinking.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingNested,
            offending,
        ));
    }

    let json = after_opening
        .advance(closing_start + "</why>".len())
        .trim_start();
    if starts_with_thinking_tag(json.value) {
        return Err(json.error_at(TranslationTaskResponseParseErrorKind::ThinkingRepeated, 0));
    }
    Ok(TranslationResponseEnvelopeParts {
        thinking: Some(thinking.value),
        assistant_json: json,
    })
}

fn starts_with_thinking_tag(value: &str) -> bool {
    value.starts_with("<why>") || value.starts_with("</why>")
}

fn first_thinking_tag(value: &str) -> Option<usize> {
    ["<why>", "</why>"]
        .into_iter()
        .filter_map(|tag| value.find(tag))
        .min()
}

fn strip_single_markdown_fence(
    value: LocatedModelResponse<'_>,
) -> Result<LocatedModelResponse<'_>, TranslationTaskResponseParseError> {
    if !value.value.starts_with("```") {
        return Ok(value);
    }
    let Some(first_line_end) = value.value.find('\n') else {
        return Err(
            value.error_at_raw_eof(TranslationTaskResponseParseErrorKind::MarkdownFenceNoBody)
        );
    };
    let opening = value.value[..first_line_end].trim_end_matches('\r');
    if opening != "```" && opening != "```json" && opening != "```JSON" {
        return Err(value.error_at(
            TranslationTaskResponseParseErrorKind::MarkdownFenceUnsupported,
            0,
        ));
    }
    let body_and_closing = value.advance(first_line_end + 1);
    let Some(closing_line_start) = body_and_closing.value.rfind('\n') else {
        if body_and_closing.value.trim_end_matches('\r') == "```" {
            return Err(body_and_closing.error_at(
                TranslationTaskResponseParseErrorKind::MarkdownFenceNoBody,
                0,
            ));
        }
        return Err(
            value.error_at_raw_eof(TranslationTaskResponseParseErrorKind::MarkdownFenceUnclosed)
        );
    };
    let closing = body_and_closing.value[closing_line_start + 1..].trim_end_matches('\r');
    if closing != "```" {
        return Err(body_and_closing.error_at(
            TranslationTaskResponseParseErrorKind::MarkdownFenceInvalidClosing,
            closing_line_start + 1,
        ));
    }
    let body = body_and_closing.prefix(closing_line_start).trim();
    if body.value.is_empty() {
        return Err(body_and_closing.error_at(
            TranslationTaskResponseParseErrorKind::MarkdownFenceNoBody,
            closing_line_start + 1,
        ));
    }
    Ok(body)
}

#[cfg(test)]
fn validate_and_restore_translation_lines(
    lines: Vec<String>,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
) -> Result<Vec<String>, TranslationCandidateValidationError> {
    let placeholder_bindings = PlaceholderBindingIndex::new(placeholders)
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
    validate_and_restore_translation_lines_at(
        lines,
        protected_text,
        line_shape,
        placeholders,
        &placeholder_bindings,
        language_analysis,
        language_module,
        TranslationCandidateInvariantLocation::PreparedCandidate,
    )
}

fn validate_and_restore_translation_lines_at(
    mut lines: Vec<String>,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    placeholder_bindings: &PlaceholderBindingIndex,
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    invariant_location: TranslationCandidateInvariantLocation,
) -> Result<Vec<String>, TranslationCandidateValidationError> {
    let initial_scans = lines
        .iter()
        .map(|line| placeholder_bindings.scan(line))
        .collect::<Vec<_>>();
    let normalized_original = normalize_original_controls_in_lines(
        &mut lines,
        placeholders,
        placeholder_bindings,
        &initial_scans,
    )
    .map_err(TranslationCandidateValidationError::Rejected)?;
    let line_scans = if normalized_original {
        lines
            .iter()
            .map(|line| placeholder_bindings.scan(line))
            .collect::<Vec<_>>()
    } else {
        initial_scans
    };
    let line_binding_indices = match line_shape {
        ExpectedLineShape::Reflow => {
            placeholder_bindings
                .validate_multiset(&line_scans, placeholder_bindings.all_binding_indices())
                .map_err(multiset_rejection)
                .map_err(TranslationCandidateValidationError::Rejected)?;
            line_scans
                .iter()
                .map(|scan| placeholder_bindings.present_binding_indices(scan))
                .collect::<Vec<_>>()
        }
        ExpectedLineShape::Aligned(_) => {
            let protected_lines = protected_text.split('\n').collect::<Vec<_>>();
            let source_scans = protected_lines
                .iter()
                .map(|source_line| placeholder_bindings.scan(source_line))
                .collect::<Vec<_>>();
            let binding_indices = source_scans
                .iter()
                .map(|scan| placeholder_bindings.present_binding_indices(scan))
                .collect::<Vec<_>>();
            for (scan, expected_bindings) in line_scans.iter().zip(&binding_indices) {
                placeholder_bindings
                    .validate_multiset(std::slice::from_ref(scan), expected_bindings)
                    .map_err(multiset_rejection)
                    .map_err(TranslationCandidateValidationError::Rejected)?;
            }
            binding_indices
        }
    };

    let mut projected_segments = Vec::new();
    let mut line_projections = Vec::with_capacity(lines.len());
    for (line_index, ((line, scanned), binding_indices)) in lines
        .iter()
        .zip(&line_scans)
        .zip(&line_binding_indices)
        .enumerate()
    {
        let projected = placeholder_bindings
            .project(line, scanned, binding_indices)
            .map_err(TranslationCandidateValidationError::LanguageProjection)?;
        projected_segments.extend(projected.language_text().segments().iter().cloned());
        line_projections.push(projected);
        if line_index + 1 < lines.len() {
            projected_segments.push(LanguageTextSegment::OpaqueBoundary);
        }
    }

    let projected = LanguageText::new(projected_segments);
    let normalized = normalize_language_text(&projected)
        .map_err(TranslationCandidateValidationError::Rejected)?;
    if let Some(residual) = language_module
        .find_source_residual(language_analysis, &normalized)
        .map_err(TranslationCandidateValidationError::LanguageModule)?
    {
        return Err(TranslationCandidateValidationError::Rejected(
            TranslationUnitRejectionReason::SourceResidual {
                fragment: residual.fragment().to_owned(),
            },
        ));
    }
    let repair = language_module
        .plan_translation_repair(language_analysis, &normalized)
        .map_err(TranslationCandidateValidationError::LanguageModule)?;
    let repaired = normalized
        .apply_repair(&repair)
        .map_err(TranslationCandidateValidationError::LanguageRepair)?;

    let mut restored = Vec::with_capacity(lines.len());
    let mut segment_offset = 0;
    for (line_index, projection) in line_projections.iter().enumerate() {
        let segment_count = projection.language_text().segments().len();
        let line_end = segment_offset + segment_count;
        let Some(repaired_segments) = repaired.segments().get(segment_offset..line_end) else {
            return Err(TranslationCandidateValidationError::InternalInvariant {
                invariant: TranslationInternalInvariant::RepairSegmentRangeMissing {
                    location: invariant_location,
                    line_index,
                    start: segment_offset,
                    end: line_end,
                    actual: repaired.segments().len(),
                },
            });
        };
        restored.push(
            placeholder_bindings
                .rebuild_original(projection, &LanguageText::new(repaired_segments.to_vec()))
                .map_err(TranslationCandidateValidationError::LanguageProjection)?,
        );
        segment_offset = line_end;
        if line_index + 1 < lines.len() {
            if !matches!(
                repaired.segments().get(segment_offset),
                Some(LanguageTextSegment::OpaqueBoundary)
            ) {
                return Err(TranslationCandidateValidationError::InternalInvariant {
                    invariant: TranslationInternalInvariant::RepairLineBoundaryMissing {
                        location: invariant_location,
                        line_index,
                        segment_index: segment_offset,
                        actual: repaired.segments().len(),
                    },
                });
            }
            segment_offset += 1;
        }
    }
    if segment_offset != repaired.segments().len() {
        return Err(TranslationCandidateValidationError::InternalInvariant {
            invariant: TranslationInternalInvariant::RepairUnassignedSegments {
                location: invariant_location,
                consumed: segment_offset,
                actual: repaired.segments().len(),
            },
        });
    }
    if restored
        .iter()
        .any(|line| placeholder_token::contains_reserved_prefix(line))
    {
        return Err(TranslationCandidateValidationError::InternalInvariant {
            invariant: TranslationInternalInvariant::ReservedTokenAfterRestore {
                location: invariant_location,
            },
        });
    }
    Ok(restored)
}

fn validate_and_restore_translation(
    translation: String,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
) -> Result<String, TranslationCandidateValidationError> {
    validate_and_restore_translation_at(
        translation,
        placeholders,
        language_analysis,
        language_module,
        TranslationCandidateInvariantLocation::PreparedCandidate,
    )
}

fn validate_and_restore_translation_at(
    mut translation: String,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    invariant_location: TranslationCandidateInvariantLocation,
) -> Result<String, TranslationCandidateValidationError> {
    let placeholder_bindings = PlaceholderBindingIndex::new(placeholders)
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
    let initial_scan = placeholder_bindings.scan(&translation);
    let normalized_original = normalize_original_controls_in_lines(
        std::slice::from_mut(&mut translation),
        placeholders,
        &placeholder_bindings,
        std::slice::from_ref(&initial_scan),
    )
    .map_err(TranslationCandidateValidationError::Rejected)?;
    let scanned = if normalized_original {
        placeholder_bindings.scan(&translation)
    } else {
        initial_scan
    };
    placeholder_bindings
        .validate_multiset(
            std::slice::from_ref(&scanned),
            placeholder_bindings.all_binding_indices(),
        )
        .map_err(multiset_rejection)
        .map_err(TranslationCandidateValidationError::Rejected)?;

    let projected = placeholder_bindings
        .project(
            &translation,
            &scanned,
            placeholder_bindings.all_binding_indices(),
        )
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
    let normalized = normalize_language_text(projected.language_text())
        .map_err(TranslationCandidateValidationError::Rejected)?;
    if let Some(residual) = language_module
        .find_source_residual(language_analysis, &normalized)
        .map_err(TranslationCandidateValidationError::LanguageModule)?
    {
        return Err(TranslationCandidateValidationError::Rejected(
            TranslationUnitRejectionReason::SourceResidual {
                fragment: residual.fragment().to_owned(),
            },
        ));
    }
    let repair = language_module
        .plan_translation_repair(language_analysis, &normalized)
        .map_err(TranslationCandidateValidationError::LanguageModule)?;
    let repaired = normalized
        .apply_repair(&repair)
        .map_err(TranslationCandidateValidationError::LanguageRepair)?;
    let restored = placeholder_bindings
        .rebuild_original(&projected, &repaired)
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
    if placeholder_token::contains_reserved_prefix(&restored) {
        return Err(TranslationCandidateValidationError::InternalInvariant {
            invariant: TranslationInternalInvariant::ReservedTokenAfterRestore {
                location: invariant_location,
            },
        });
    }
    Ok(restored)
}

pub(super) fn accept_prepared_translation_candidate(
    translation: String,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
) -> Result<super::semantics::PreparedTranslationAcceptance, TranslationCandidateTechnicalError> {
    if translation.trim().is_empty() {
        return Ok(super::semantics::PreparedTranslationAcceptance::Rejected(
            super::semantics::PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::BlankTranslation,
            ),
        ));
    }
    if let Some(byte_index) = translation.find(['\r', '\0']) {
        let line_index = translation[..byte_index]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        return Ok(super::semantics::PreparedTranslationAcceptance::Rejected(
            super::semantics::PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::InvalidLineText { line_index },
            ),
        ));
    }
    match validate_and_restore_translation(
        translation,
        placeholders,
        language_analysis,
        language_module,
    ) {
        Ok(translation) => Ok(super::semantics::PreparedTranslationAcceptance::Accepted(
            translation,
        )),
        Err(TranslationCandidateValidationError::Rejected(reason)) => {
            Ok(super::semantics::PreparedTranslationAcceptance::Rejected(
                super::semantics::PreparedTranslationRejection::Candidate(reason),
            ))
        }
        Err(TranslationCandidateValidationError::LanguageModule(source)) => {
            Err(TranslationCandidateTechnicalError::LanguageModule(source))
        }
        Err(TranslationCandidateValidationError::LanguageProjection(source)) => Err(
            TranslationCandidateTechnicalError::LanguageProjection(source),
        ),
        Err(TranslationCandidateValidationError::LanguageRepair(source)) => {
            Err(TranslationCandidateTechnicalError::LanguageRepair(source))
        }
        Err(TranslationCandidateValidationError::InternalInvariant { invariant }) => {
            Err(TranslationCandidateTechnicalError::InternalInvariant { invariant })
        }
    }
}

pub(crate) enum TranslationCandidateTechnicalError {
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

impl TranslationCandidateTechnicalError {
    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::LanguageModule(source) => source.safe_diagnostic_detail(),
            Self::LanguageProjection(source) => language_projection_detail(source),
            Self::LanguageRepair(source) => language_repair_detail(source),
            Self::InternalInvariant { invariant } => invariant.safe_detail(),
        }
    }
}

impl fmt::Debug for TranslationCandidateTechnicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TranslationCandidateTechnicalError")
            .field(&self.safe_detail())
            .finish()
    }
}

impl fmt::Display for TranslationCandidateTechnicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LanguageModule(source) => write!(formatter, "语言模块失败：{source}"),
            Self::LanguageProjection(source) => write!(formatter, "语言投影失败：{source}"),
            Self::LanguageRepair(source) => write!(formatter, "语言修复失败：{source}"),
            Self::InternalInvariant { invariant } => {
                write!(formatter, "翻译候选内部不变量已破坏：{invariant}")
            }
        }
    }
}

impl Error for TranslationCandidateTechnicalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LanguageModule(source) => Some(source),
            Self::LanguageProjection(source) => Some(source),
            Self::LanguageRepair(source) => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

fn normalize_language_text(
    language_text: &LanguageText,
) -> Result<LanguageText, TranslationUnitRejectionReason> {
    let mut segments = Vec::with_capacity(language_text.segments().len());
    for segment in language_text.segments() {
        match segment {
            LanguageTextSegment::NaturalText(text) => {
                if text.contains('\u{feff}') {
                    return Err(TranslationUnitRejectionReason::ContainsByteOrderMark);
                }
                segments.push(LanguageTextSegment::NaturalText(
                    text.replace("\r\n", "\n").replace('\r', "\n"),
                ));
            }
            LanguageTextSegment::OpaqueBoundary => {
                segments.push(LanguageTextSegment::OpaqueBoundary);
            }
        }
    }
    let normalized = LanguageText::new(segments);
    if !normalized.has_non_whitespace_natural_text() {
        return Err(TranslationUnitRejectionReason::NoNaturalLanguageText);
    }
    Ok(normalized)
}

#[derive(Debug)]
enum TranslationCandidateValidationError {
    Rejected(TranslationUnitRejectionReason),
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

fn normalize_original_controls_in_lines(
    lines: &mut [String],
    placeholders: &[AppliedPlaceholder],
    placeholder_bindings: &PlaceholderBindingIndex,
    scans: &[PlaceholderTextScan],
) -> Result<bool, TranslationUnitRejectionReason> {
    let mut originals = BTreeMap::<&str, Vec<usize>>::new();
    let mut token_counts = BTreeMap::<&str, usize>::new();
    for (binding_index, placeholder) in placeholders.iter().enumerate() {
        originals
            .entry(placeholder.original())
            .or_default()
            .push(binding_index);
        token_counts
            .entry(placeholder.token())
            .or_insert_with(|| placeholder_bindings.token_occurrences(scans, binding_index));
    }
    let mut changed = false;
    for (original, bindings) in originals {
        if bindings.len() != 1 {
            // 同一 original 对应多个 token 时任何字面出现都无法唯一归位：
            // 无论对应 token 是缺失还是全部在场(多抄了一份原始片段)，都属于
            // 无法无歧义恢复的混排，与单绑定分支的拒绝语义一致。
            if lines.iter().any(|line| line.contains(original)) {
                return Err(
                    TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                        original: original.to_owned(),
                    },
                );
            }
            continue;
        }
        let binding = &placeholders[bindings[0]];
        let token_count = token_counts[binding.token()];
        let original_count = lines
            .iter()
            .map(|line| line.matches(original).count())
            .sum::<usize>();
        if token_count == 0 && original_count > 0 {
            if original_count == 1 {
                let line = lines
                    .iter_mut()
                    .find(|line| line.contains(original))
                    .expect("已确认唯一原片段存在于某个译文行");
                *line = line.replacen(original, binding.token(), 1);
                *token_counts
                    .get_mut(binding.token())
                    .expect("binding token 已建立计数") += 1;
                changed = true;
            } else {
                return Err(
                    TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                        original: original.to_owned(),
                    },
                );
            }
        } else if original_count > 0 && token_count != 0 {
            return Err(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: original.to_owned(),
                },
            );
        }
    }
    Ok(changed)
}

fn multiset_rejection(error: PlaceholderMultisetError) -> TranslationUnitRejectionReason {
    match error {
        PlaceholderMultisetError::Mismatch { token } => {
            TranslationUnitRejectionReason::PlaceholderMismatch { token }
        }
        PlaceholderMultisetError::Unexpected { token } => {
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token }
        }
    }
}

#[cfg(test)]
fn validate_token_multiset(
    translation: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<(), TranslationUnitRejectionReason> {
    let bindings = PlaceholderBindingIndex::new(placeholders).expect("测试 token 索引应可建立");
    let scanned = bindings.scan(translation);
    bindings
        .validate_multiset(
            std::slice::from_ref(&scanned),
            bindings.all_binding_indices(),
        )
        .map_err(multiset_rejection)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::fingerprint::Sha256Fingerprint;
    use crate::language::{
        EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
        JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageId,
        LanguageModule, LanguagePair, LanguageText, QuotePair,
    };
    use crate::llm::{ChatMessage, ChatMessageRole};
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::rpg_maker::translate::profile::{
        ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
        RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
        RpgMakerTranslationRequestConfiguration, TranslationResponseEnvelope,
    };
    use crate::rpg_maker::translate::standard::{
        AppliedPlaceholder, ExpectedLineShape, ExpectedTranslationOutput,
        ExpectedTranslationValidation, PlaceholderRuleOrigin, PlaceholderSegment,
        StandardTranslationTaskIndex, TranslationStateContext, TranslationUnitIdentity,
    };
    use crate::runtime::cpu::CpuExecutorUnavailable;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    impl LlmRequestDiagnosticSource for FakeError {
        fn request_diagnostic(
            &self,
            retry_after: Option<Duration>,
            impact: DiagnosticImpact,
        ) -> crate::diagnostic::SafeDiagnostic {
            crate::diagnostic::SafeDiagnostic::new(
                crate::diagnostic::DiagnosticCode::ModelRequest,
                crate::diagnostic::DiagnosticStage::ModelRequest,
                crate::diagnostic::DiagnosticSubject::component("fake LLM provider"),
                crate::diagnostic::DiagnosticReason::Http {
                    status: Some(503),
                    retry_after_seconds: retry_after.map(|value| value.as_secs()),
                    provider_code: Some("temporarily_unavailable".to_owned()),
                    provider_type: Some("service_error".to_owned()),
                },
                impact,
                crate::diagnostic::DiagnosticAction::CheckModelService,
            )
        }

        fn is_cancelled_wait(&self) -> bool {
            self.0 == "cancelled-wait"
        }
    }

    impl LlmClientConcurrency for &'static str {
        fn max_concurrent_requests(&self) -> NonZeroUsize {
            NonZeroUsize::new(3).expect("测试并发数必须非零")
        }
    }

    #[derive(Clone, Copy)]
    struct InlineCpu;

    impl CpuTaskExecutor for InlineCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[derive(Clone)]
    struct CancellingCpu {
        cancellation: CooperativeCancellation,
    }

    impl CpuTaskExecutor for CancellingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.cancellation.request();
            Err(CpuTaskExecutionError::Cancelled)
        }
    }

    #[derive(Clone, Copy)]
    struct UnavailableCpu;

    impl CpuTaskExecutor for UnavailableCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Err(CpuTaskExecutionError::Unavailable(FakeError("cpu")))
        }
    }

    type ProductionResponseError = TranslationTaskResponseProcessingError<CpuExecutorUnavailable>;

    #[test]
    fn response_diagnostic_distinguishes_cpu_cancel_unavailable_and_task_panic() {
        let cases: [(ProductionResponseError, DiagnosticFailureKind); 3] = [
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Cancelled,
                ),
                DiagnosticFailureKind::LockCancelled,
            ),
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown),
                ),
                DiagnosticFailureKind::ExecutorClosed,
            ),
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::TaskPanicked,
                ),
                DiagnosticFailureKind::WorkerPanicked,
            ),
        ];

        for (error, expected) in cases {
            let diagnostic = error.safe_diagnostic_source(
                DiagnosticStage::ModelRequest,
                DiagnosticImpact::ProgressPreserved,
                DiagnosticAction::Retry,
            );
            match diagnostic.reason {
                DiagnosticReason::Failure { failure }
                | DiagnosticReason::FailureWithDetail { failure, .. } => {
                    assert_eq!(failure, expected);
                }
                reason => panic!("CPU 诊断原因类型错误：{reason:?}"),
            }
        }
    }

    #[test]
    fn response_diagnostic_treats_projection_and_repair_as_internal_without_copying_text() {
        let sentinel = "MODEL_OR_TOKEN_BODY_SENTINEL";
        let projection: ProductionResponseError =
            TranslationTaskResponseProcessingError::LanguageProjection(
                LanguageTextProjectionError::MissingToken {
                    token: sentinel.to_owned(),
                },
            );
        let projection = projection.safe_diagnostic_source(
            DiagnosticStage::ModelRequest,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckModelService,
        );
        assert_eq!(projection.code, DiagnosticCode::InternalOperation);
        assert_eq!(projection.action, DiagnosticAction::ReportBug);
        assert!(
            projection
                .reason
                .render()
                .contains("missing_required_placeholder_token")
        );
        assert!(!projection.reason.render().contains(sentinel));

        let repair: ProductionResponseError =
            TranslationTaskResponseProcessingError::LanguageRepair(
                LanguageRepairApplicationError::UnexpectedCharacter {
                    segment_index: 4,
                    byte_offset: 9,
                    expected: '密',
                    actual: '钥',
                },
            );
        let repair = repair.safe_diagnostic_source(
            DiagnosticStage::ModelRequest,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckModelService,
        );
        assert_eq!(repair.action, DiagnosticAction::ReportBug);
        assert!(repair.reason.render().contains("segment_index=4"));
        assert!(!repair.reason.render().contains('密'));
        assert!(!repair.reason.render().contains('钥'));
    }

    fn japanese_module() -> Arc<dyn LanguageModule> {
        Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::new(2).expect("测试阈值非零"), Vec::new())
                .expect("日文残留策略有效"),
            Some(
                JapaneseQuoteRepairPolicy::new(vec![
                    QuotePair::new('“', '”'),
                    QuotePair::new('‘', '’'),
                ])
                .expect("日文引号修复策略有效"),
            ),
        ))
    }

    fn english_module() -> Arc<dyn LanguageModule> {
        Arc::new(EnglishLanguageModule::new(
            EnglishTranslationDetectionPolicy::new(
                NonZeroUsize::new(2).expect("测试阈值非零"),
                NonZeroUsize::new(4).expect("测试阈值非零"),
                Vec::new(),
            )
            .expect("英文译前策略有效"),
            EnglishResidualPolicy::new(
                NonZeroUsize::new(2).expect("测试阈值非零"),
                NonZeroUsize::new(4).expect("测试阈值非零"),
                Vec::new(),
            )
            .expect("英文残留策略有效"),
        ))
    }

    fn translation_resources_with(
        source_language: &str,
        target_language: &str,
        module: Arc<dyn LanguageModule>,
    ) -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with_envelope(
            source_language,
            target_language,
            module,
            TranslationResponseEnvelope::JsonOnly,
        )
    }

    fn translation_resources_with_envelope(
        source_language: &str,
        target_language: &str,
        module: Arc<dyn LanguageModule>,
        response_envelope: TranslationResponseEnvelope,
    ) -> Arc<ResolvedRpgMakerTranslationResources> {
        let pair = LanguagePair::new(
            LanguageId::parse(source_language).expect("测试源语言合法"),
            LanguageId::parse(target_language).expect("测试目标语言合法"),
        );
        let prompt = RpgMakerSystemPrompt::new(pair, "# Contract".to_owned(), response_envelope)
            .expect("测试 Prompt 合法");
        Arc::new(ResolvedRpgMakerTranslationResources::new(prompt, module))
    }

    fn translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with("ja", "zh-Hans", japanese_module())
    }

    fn thinking_translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with_envelope(
            "ja",
            "zh-Hans",
            japanese_module(),
            TranslationResponseEnvelope::ThinkingThenJson,
        )
    }

    fn japanese_analysis() -> crate::language::LanguageAnalysis {
        japanese_module().analyze_source(&LanguageText::natural("炎の剣"))
    }

    fn identity() -> TranslationUnitIdentity {
        let group = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group,
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法")),
            TextUnitContent::Value("炎の剣\\N[1]".to_owned()),
            "{}",
        )
    }

    fn placeholder() -> AppliedPlaceholder {
        AppliedPlaceholder::new(
            "⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            "event_dialogue",
            PlaceholderSegment::Whole,
        )
    }

    fn propagation_target() -> TranslationUnitIdentity {
        let group = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(2)],
        );
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group,
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法")),
            TextUnitContent::Value("炎の剣\\N[1]".to_owned()),
            "{}",
        )
    }

    fn reflow_value_identity() -> TranslationUnitIdentity {
        let group = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(3)],
        );
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group,
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("炎の剣。\n装備すると攻撃力が上がる。".to_owned()),
            "{}",
        )
    }

    fn speaker_identity() -> TranslationUnitIdentity {
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(0)]);
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group,
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("炎の剣".to_owned()),
            "{}",
        )
    }

    fn dialogue_body_identity() -> TranslationUnitIdentity {
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(1)]);
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group,
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec![
                "今日はいい天気ですね。".to_owned(),
                "一緒に町へ".to_owned(),
                "行きませんか？".to_owned(),
            ]),
            r#"{"source_speaker":"アリス"}"#,
        )
    }

    fn choices_identity() -> TranslationUnitIdentity {
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(2)]);
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventChoices,
            group,
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["はい".to_owned(), "いいえ".to_owned()]),
            "{}",
        )
    }

    fn scrolling_identity_with_blank_slot() -> TranslationUnitIdentity {
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(3)]);
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventScrollingText,
            group,
            TextUnitRole::ScrollingText,
            TextUnitContent::Lines(vec![
                "スタッフ".to_owned(),
                String::new(),
                "アリス".to_owned(),
            ]),
            "{}",
        )
    }

    fn line_content_analysis(lines: &[&str]) -> crate::language::LanguageAnalysis {
        let mut segments = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                segments.push(LanguageTextSegment::OpaqueBoundary);
            }
            segments.push(LanguageTextSegment::NaturalText((*line).to_owned()));
        }
        japanese_module().analyze_source(&LanguageText::new(segments))
    }

    fn line_task(
        identity: TranslationUnitIdentity,
        line_shape: ExpectedLineShape,
        analysis: crate::language::LanguageAnalysis,
    ) -> TranslationTaskBlock {
        line_task_with_propagation(identity, Vec::new(), line_shape, analysis)
    }

    fn line_task_with_propagation(
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        line_shape: ExpectedLineShape,
        analysis: crate::language::LanguageAnalysis,
    ) -> TranslationTaskBlock {
        let protected_text = match identity.source_content() {
            TextUnitContent::Value(value) => value.clone(),
            TextUnitContent::Lines(lines) => lines.join("\n"),
        };
        let propagation_state_contexts = (0..propagation_targets.len())
            .map(|index| state_context(index as u8 + 5))
            .collect();
        TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(4),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                1,
                identity,
                propagation_targets,
                ExpectedTranslationValidation::new(
                    line_shape,
                    protected_text,
                    Vec::new(),
                    analysis,
                ),
                state_context(4),
                propagation_state_contexts,
            )],
        )
    }

    fn task() -> TranslationTaskBlock {
        task_with_output_count(1)
    }

    fn task_with_output_count(output_count: usize) -> TranslationTaskBlock {
        task_with_language_pair("ja", "zh-Hans", output_count)
    }

    fn speaker_task() -> TranslationTaskBlock {
        TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(3),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                1,
                speaker_identity(),
                Vec::new(),
                ExpectedTranslationValidation::new(
                    ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                    "炎の剣",
                    Vec::new(),
                    japanese_analysis(),
                ),
                state_context(3),
                Vec::new(),
            )],
        )
    }

    fn task_with_language_pair(
        source_language: &str,
        target_language: &str,
        output_count: usize,
    ) -> TranslationTaskBlock {
        TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(2),
            LanguagePair::new(
                LanguageId::parse(source_language).expect("测试源语言合法"),
                LanguageId::parse(target_language).expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            (1..=output_count)
                .map(|id| {
                    ExpectedTranslationOutput::new(
                        id,
                        identity(),
                        vec![propagation_target()],
                        ExpectedTranslationValidation::new(
                            ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                            "炎の剣⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
                            vec![placeholder()],
                            japanese_analysis(),
                        ),
                        state_context(id as u8 + 1),
                        vec![state_context(id as u8 + 101)],
                    )
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn response_processing_reuses_the_planner_placeholder_index() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let task = task();
        let bindings = task.expected_outputs()[0].placeholder_bindings();
        let construction_scans = bindings.scan_passes();
        assert_eq!(
            construction_scans, 1,
            "Planner 构造期只扫描一次受保护原文契约"
        );

        let outcome = processor
            .process(
                &task,
                LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("合法响应应使用缓存索引完成验收");

        assert!(matches!(outcome, TranslationTaskOutcome::Complete { .. }));
        assert!(
            bindings.scan_passes() > construction_scans,
            "响应验收必须继续使用 ExpectedTranslationOutput 缓存的同一索引"
        );
    }

    #[test]
    fn json_only_response_envelope_accepts_only_the_explicit_contract() {
        for value in [
            "{}",
            " \r\n {} \n ",
            "\u{feff}{}",
            "```\n{}\n```",
            "```json\r\n{}\r\n```",
            "```JSON\n{\"0\":[\"括号 [ ]、逗号 ,} 与反引号 ```\"]}\n```",
            r#"{"1":["业务文本可以包含 <why> 与 </why> 标签"]}"#,
            "```json\n{\"1\":[\"<why>围栏内仍是普通业务文本</why>\"]}\n```",
        ] {
            assert!(
                parse_model_output_batch(value, TranslationResponseEnvelope::JsonOnly).is_ok(),
                "合法响应信封应通过：{value:?}"
            );
        }

        for value in [
            "说明：{}",
            "<why>不应输出思考</why>{}",
            "</why>{}",
            "{} 后记",
            "{}\n{}",
            "{\"0\":[\"译文\",]}",
            "{\"0\":[\"译文\"] ,}",
            "{// comment\n}",
            "```yaml\n{}\n```",
            "```json\n{}",
            "```json\n{}```",
            "```json\n\n```",
            "```json\n{}\n```\n后记",
            "{\"0\":[\"截断",
            "\u{feff}\u{feff}{}",
            "[]",
        ] {
            assert!(
                parse_model_output_batch(value, TranslationResponseEnvelope::JsonOnly).is_err(),
                "协议外响应必须拒绝：{value:?}"
            );
        }
    }

    #[test]
    fn thinking_response_envelope_accepts_one_non_blank_exact_tag_pair() {
        for value in [
            "<why>逐项分析。</why>{}",
            "<why>第一行\n第二行</why>\n{}",
            " \r\n\u{feff}<why>\n　逐项分析\t\n</why>\r\n```json\r\n{}\r\n``` \n",
            r#"<why>逐项分析。</why>{"1":["业务文本可以包含 <why> 与 </why> 标签"]}"#,
            "<why>逐项分析。</why>\n```json\n{\"1\":[\"<why>围栏内仍是普通业务文本</why>\"]}\n```",
        ] {
            assert!(
                parse_model_output_batch(value, TranslationResponseEnvelope::ThinkingThenJson)
                    .is_ok(),
                "合法思考信封应通过：{value:?}"
            );
        }
    }

    #[test]
    fn thinking_response_envelope_rejects_every_protocol_variant() {
        for value in [
            "{}",
            "<why></why>{}",
            "<why> \n　\t</why>{}",
            "<why>未闭合{}",
            "<why>外层<why>内层</why></why>{}",
            "<why>第一组</why><why>第二组</why>{}",
            "<why>第一组</why></why>{}",
            "<WHY>大小写错误</WHY>{}",
            "<Why>大小写错误</why>{}",
            "<why reason=\"analysis\">带属性</why>{}",
            "说明文字<why>分析</why>{}",
            "<why>分析</WHY>{}",
            "<why>分析</why>说明文字{}",
            "<why>分析</why>{}后记",
            "```\n<why>分析</why>{}\n```",
        ] {
            assert!(
                parse_model_output_batch(value, TranslationResponseEnvelope::ThinkingThenJson)
                    .is_err(),
                "协议外思考信封必须拒绝：{value:?}"
            );
        }
    }

    #[test]
    fn response_parse_errors_use_complete_raw_assistant_coordinates() {
        let cases = [
            (
                " \r\n\u{feff}<why>\n分析\n</why>\r\n```json\r\n{\r\n  \"1\": [\"ok\"],\r\n  \"2\": ]\r\n}\r\n``` \n",
                TranslationResponseEnvelope::ThinkingThenJson,
                TranslationTaskResponseParseErrorKind::Json(
                    TranslationTaskResponseJsonErrorCategory::Syntax,
                ),
                8,
                8,
            ),
            (
                " \n<why>分析\n",
                TranslationResponseEnvelope::ThinkingThenJson,
                TranslationTaskResponseParseErrorKind::ThinkingEnvelopeUnclosed,
                3,
                1,
            ),
            (
                "\n<why>外层\n  <why>内层</why></why>{}",
                TranslationResponseEnvelope::ThinkingThenJson,
                TranslationTaskResponseParseErrorKind::ThinkingNested,
                3,
                3,
            ),
            (
                "<why>第一组</why>\n  </why>{}",
                TranslationResponseEnvelope::ThinkingThenJson,
                TranslationTaskResponseParseErrorKind::ThinkingRepeated,
                2,
                3,
            ),
            (
                " \n```yaml\n{}\n```",
                TranslationResponseEnvelope::JsonOnly,
                TranslationTaskResponseParseErrorKind::MarkdownFenceUnsupported,
                2,
                1,
            ),
            (
                "\n```json\n{}\n",
                TranslationResponseEnvelope::JsonOnly,
                TranslationTaskResponseParseErrorKind::MarkdownFenceUnclosed,
                4,
                1,
            ),
            (
                "```json\n{}\n``` trailing",
                TranslationResponseEnvelope::JsonOnly,
                TranslationTaskResponseParseErrorKind::MarkdownFenceInvalidClosing,
                3,
                1,
            ),
            (
                "\n  </why>{}",
                TranslationResponseEnvelope::JsonOnly,
                TranslationTaskResponseParseErrorKind::ThinkingNotAllowed,
                2,
                3,
            ),
        ];

        for (raw, envelope, kind, line, column) in cases {
            let error = parse_model_response(raw, envelope).expect_err("测试响应必须解析失败");
            assert_eq!(
                error,
                TranslationTaskResponseParseError::new(
                    kind,
                    NonZeroUsize::new(line).expect("测试行号非零"),
                    NonZeroUsize::new(column).expect("测试列号非零"),
                ),
                "原始 Assistant 坐标错误：{raw:?}"
            );
            let message = error.business_message();
            assert!(message.contains(&format!("第 {line} 行、第 {column} 列")));
        }
    }

    #[test]
    fn model_output_id_accepts_only_canonical_object_keys() {
        let outputs = parse_model_output_batch(
            r#"{"1":["甲"],"2":["乙"]}"#,
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect("无前导零的 ASCII 十进制键应合法");

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].id, "1");
        assert_eq!(outputs[1].id, "2");
        assert_eq!(parse_model_output_id("1"), Some(1));
        for invalid in ["", "0", "01", "-1", "1.5", "true"] {
            assert_eq!(parse_model_output_id(invalid), None);
        }
        assert_eq!(
            parse_model_output_id("999999999999999999999999999999999999"),
            None
        );
    }

    #[tokio::test]
    async fn reflow_output_accepts_a_different_non_empty_line_count_as_one_unit() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        for expected_lines in [
            vec!["今天天气真好。", "要不要一起去城里？"],
            vec!["今天", "天气真好。", "要不要一起", "去城里？"],
        ] {
            let task = line_task(
                dialogue_body_identity(),
                ExpectedLineShape::Reflow,
                line_content_analysis(&["今日はいい天気ですね。", "一緒に町へ", "行きませんか？"]),
            );
            let content = serde_json::to_string(&serde_json::json!({"1": &expected_lines}))
                .expect("测试响应应可序列化");
            let outcome = processor
                .process(
                    &task,
                    LlmResponse::new(content, LlmFinishReason::Stop, None, None, None),
                    1,
                )
                .await
                .expect("自由断行应按一个语义单元验收");

            assert!(matches!(outcome, TranslationTaskOutcome::Complete { .. }));
            assert_eq!(
                outcome.accepted()[0].translation(),
                &TextUnitContent::Lines(expected_lines.into_iter().map(str::to_owned).collect())
            );
        }
    }

    #[tokio::test]
    async fn reflow_blank_collections_reject_only_their_id() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let body = dialogue_body_identity();
        let body_protected_text = match body.source_content() {
            TextUnitContent::Lines(lines) => lines.join("\n"),
            TextUnitContent::Value(_) => unreachable!("对话正文必须是完整行序列"),
        };
        let task = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(6),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![
                ExpectedTranslationOutput::new(
                    1,
                    body,
                    Vec::new(),
                    ExpectedTranslationValidation::new(
                        ExpectedLineShape::Reflow,
                        body_protected_text,
                        Vec::new(),
                        line_content_analysis(&[
                            "今日はいい天気ですね。",
                            "一緒に町へ",
                            "行きませんか？",
                        ]),
                    ),
                    state_context(6),
                    Vec::new(),
                ),
                ExpectedTranslationOutput::new(
                    2,
                    speaker_identity(),
                    Vec::new(),
                    ExpectedTranslationValidation::new(
                        ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                        "炎の剣",
                        Vec::new(),
                        japanese_analysis(),
                    ),
                    state_context(7),
                    Vec::new(),
                ),
            ],
        );

        for response in [
            r#"{"1":[],"2":["爱丽丝"]}"#,
            r#"{"1":["","   "],"2":["爱丽丝"]}"#,
        ] {
            let outcome = processor
                .process(
                    &task,
                    LlmResponse::new(response, LlmFinishReason::Stop, None, None, None),
                    1,
                )
                .await
                .expect("自由断行的空集合应成为单 ID 正常拒绝");

            assert!(matches!(outcome, TranslationTaskOutcome::Partial { .. }));
            assert_eq!(outcome.accepted().len(), 1);
            assert_eq!(outcome.accepted()[0].id(), 2);
            assert_eq!(outcome.unresolved().len(), 1);
            assert_eq!(outcome.unresolved()[0].id(), 1);
            assert!(matches!(
                outcome.unresolved()[0].reason(),
                TranslationUnitRejectionReason::BlankTranslation
            ));
        }
    }

    #[tokio::test]
    async fn reflow_value_joins_model_lines_into_one_scalar_value() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let task = line_task(
            reflow_value_identity(),
            ExpectedLineShape::Reflow,
            japanese_module().analyze_source(&LanguageText::natural(
                "炎の剣。\n装備すると攻撃力が上がる。",
            )),
        );
        let outcome = processor
            .process(
                &task,
                LlmResponse::new(
                    r#"{"1":["炎之剑。","装备后可提升攻击力。"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("自由断行的标量字段应按一个值验收");

        assert_eq!(
            outcome.accepted()[0].translation(),
            &TextUnitContent::Value("炎之剑。\n装备后可提升攻击力。".to_owned())
        );
    }

    #[tokio::test]
    async fn aligned_output_rejects_only_the_id_with_the_wrong_line_count() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let task = line_task(
            choices_identity(),
            ExpectedLineShape::Aligned(NonZeroUsize::new(2).expect("选项数非零")),
            line_content_analysis(&["はい", "いいえ"]),
        );
        let outcome = processor
            .process(
                &task,
                LlmResponse::new(
                    r#"{"1":["是／否"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("行数不符是当前 ID 的正常拒绝");

        assert!(matches!(
            outcome.unresolved()[0].reason(),
            TranslationUnitRejectionReason::LineCountMismatch {
                expected: 2,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn single_line_speaker_rejects_multiple_array_elements() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let outcome = processor
            .process(
                &speaker_task(),
                LlmResponse::new(
                    r#"{"1":["爱丽","丝"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("姓名多行应成为当前 ID 的正常拒绝");

        assert!(matches!(
            outcome.unresolved()[0].reason(),
            TranslationUnitRejectionReason::LineCountMismatch {
                expected: 1,
                actual: 2
            }
        ));
    }

    #[tokio::test]
    async fn aligned_output_preserves_source_blank_slots() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let task = line_task(
            scrolling_identity_with_blank_slot(),
            ExpectedLineShape::Aligned(NonZeroUsize::new(3).expect("滚动文本行数非零")),
            line_content_analysis(&["スタッフ", "", "アリス"]),
        );

        let accepted = processor
            .process(
                &task,
                LlmResponse::new(
                    r#"{"1":["制作人员","","爱丽丝"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("保留空槽位的对齐译文应通过");
        assert_eq!(
            accepted.accepted()[0].translation(),
            &TextUnitContent::Lines(vec![
                "制作人员".to_owned(),
                String::new(),
                "爱丽丝".to_owned(),
            ])
        );

        for lines in [
            r#"{"1":["制作人员","填充了空槽","爱丽丝"]}"#,
            r#"{"1":["制作人员","   ","爱丽丝"]}"#,
            r#"{"1":["","","爱丽丝"]}"#,
        ] {
            let rejected = processor
                .process(
                    &task,
                    LlmResponse::new(lines, LlmFinishReason::Stop, None, None, None),
                    1,
                )
                .await
                .expect("空槽不对齐应成为当前 ID 的正常拒绝");
            assert!(matches!(
                rejected.unresolved()[0].reason(),
                TranslationUnitRejectionReason::BlankLineMismatch { .. }
            ));
        }
    }

    #[tokio::test]
    async fn aligned_output_does_not_allow_placeholders_to_move_between_slots() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(4)]);
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventChoices,
            group,
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["\\N[1]に話す".to_owned(), "やめる".to_owned()]),
            "{}",
        );
        let task = TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(5),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                1,
                identity,
                Vec::new(),
                ExpectedTranslationValidation::new(
                    ExpectedLineShape::Aligned(NonZeroUsize::new(2).expect("选项数非零")),
                    "⟦ATT_ACTOR_NAME_WHOLE_0000⟧に話す\nやめる",
                    vec![placeholder()],
                    line_content_analysis(&["彼に話す", "やめる"]),
                ),
                state_context(5),
                Vec::new(),
            )],
        );
        let outcome = processor
            .process(
                &task,
                LlmResponse::new(
                    r#"{"1":["和他交谈","取消⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("占位符跨槽位应成为当前 ID 的正常拒绝");

        assert!(matches!(
            outcome.unresolved()[0].reason(),
            TranslationUnitRejectionReason::PlaceholderMismatch { .. }
        ));
    }

    #[test]
    fn expected_output_construction_rejects_a_line_crossing_placeholder() {
        let group =
            RpgMakerLocation::value(RpgMakerSource::map(1), vec![RpgMakerLocationStep::index(5)]);
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group,
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec![
                "翻訳<opaque>前半".to_owned(),
                "後半</opaque>続き".to_owned(),
            ]),
            "{}",
        );
        let error = ExpectedTranslationOutput::try_new(
            1,
            identity,
            Vec::new(),
            ExpectedTranslationValidation::new(
                ExpectedLineShape::Reflow,
                "翻訳⟦ATT_CUSTOM_WHOLE_0000⟧続き",
                vec![AppliedPlaceholder::new(
                    "⟦ATT_CUSTOM_WHOLE_0000⟧",
                    "<opaque>前半\n後半</opaque>",
                    PlaceholderRuleOrigin::Custom,
                    "CUSTOM",
                    "event_dialogue",
                    PlaceholderSegment::Whole,
                )],
                line_content_analysis(&["翻訳前半", "後半続き"]),
            ),
            state_context(8),
            Vec::new(),
        )
        .expect_err("Planner 输出构造期必须拒绝跨物理行的占位符");

        assert!(matches!(
            error,
            super::super::standard::ExpectedTranslationOutputContractError::
                ProtectedPlaceholderCrossesLineBoundary {
                    unit_id: 1,
                    placeholder_index: 0,
                    ..
                }
        ));
    }

    #[tokio::test]
    async fn response_processor_restores_original_control_in_a_mapped_id() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let result = processor
            .process(
                &task_with_language_pair("ja", "zh-Hans", 1),
                LlmResponse::new(
                    r#"{"1":["炎之剑\\N[1]！"]}"#,
                    LlmFinishReason::Stop,
                    Some("request-1".to_owned()),
                    Some("response-1".to_owned()),
                    Some(LlmUsage::new(10, 5, 15)),
                ),
                1,
            )
            .await
            .expect("原控制符能够唯一对应时应规范化并恢复");

        assert_eq!(result.task_index(), StandardTranslationTaskIndex::new(2));
        assert!(matches!(&result, TranslationTaskOutcome::Complete { .. }));
        assert_eq!(result.attempts().get(), 1);
        assert_eq!(result.provider_request_id(), Some("request-1"));
        assert_eq!(result.provider_response_id(), Some("response-1"));
        assert_eq!(
            result.final_response_usage(),
            Some(LlmUsage::new(10, 5, 15))
        );
        assert_eq!(
            result.accepted()[0].translation(),
            &TextUnitContent::Value("炎之剑\\N[1]！".to_owned())
        );
        assert_eq!(
            result.accepted()[0].propagation_targets(),
            &[super::super::standard::TranslationPropagationTarget::new(
                propagation_target(),
                state_context(102),
            )]
        );
    }

    #[tokio::test]
    async fn response_processor_preserves_missing_provider_metadata() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let result = processor
            .process(
                &task_with_language_pair("ja", "zh-Hans", 1),
                LlmResponse::new(
                    r#"{"1":["炎之剑\\N[1]！"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("供应商元数据缺失不应否定模型正文");

        assert!(matches!(&result, TranslationTaskOutcome::Complete { .. }));
        assert_eq!(result.provider_request_id(), None);
        assert_eq!(result.provider_response_id(), None);
        assert_eq!(result.final_response_usage(), None);
    }

    fn state_context(byte: u8) -> TranslationStateContext {
        TranslationStateContext::new(Sha256Fingerprint::from_bytes([byte; 32]))
    }

    #[tokio::test]
    async fn response_processor_keeps_generic_text_checks_as_per_id_normal_results() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());

        let bom = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"1":["炎\uFEFF之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-bom".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("BOM 属于当前 ID 的正常拒绝");
        assert!(matches!(
            bom.unresolved()[0].reason(),
            TranslationUnitRejectionReason::ContainsByteOrderMark
        ));

        let no_natural = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"1":["  ⟦ATT_ACTOR_NAME_WHOLE_0000⟧  "]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-natural".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("只有占位符和空白属于当前 ID 的正常拒绝");
        assert!(matches!(
            no_natural.unresolved()[0].reason(),
            TranslationUnitRejectionReason::NoNaturalLanguageText
        ));
    }

    #[tokio::test]
    async fn response_processor_rejects_controls_embedded_in_any_returned_line() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());

        for translation in ["炎之\r剑", "炎之\n剑", "炎之\0剑"] {
            let content = serde_json::to_string(&serde_json::json!({"1": [translation]}))
                .expect("测试响应应可序列化");
            let result = processor
                .process(
                    &speaker_task(),
                    LlmResponse::new(
                        content,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-invalid-speaker".to_owned()),
                        None,
                    ),
                    1,
                )
                .await
                .expect("行内控制字符应成为当前 ID 的正常拒绝");

            assert!(matches!(
                &result,
                TranslationTaskOutcome::Unavailable {
                    reason: TranslationTaskUnavailableReason::AllOutputsRejected,
                    ..
                }
            ));
            assert!(matches!(
                result.unresolved()[0].reason(),
                TranslationUnitRejectionReason::InvalidLineText { line_index: 0 }
            ));
        }
    }

    #[tokio::test]
    async fn response_processor_rejects_closing_delimiter_in_tag_value_translations() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());

        let rejected = processor
            .process(
                &line_task(
                    tag_value_identity(),
                    ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                    line_content_analysis(&["炎の剣の説明"]),
                ),
                LlmResponse::new(
                    r#"{"1":["炎之剑<强化>说明"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-tag-value".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("标签值译文含 '>' 属于当前 ID 的正常拒绝");
        assert!(matches!(
            rejected.unresolved()[0].reason(),
            TranslationUnitRejectionReason::TagValueContainsClosingDelimiter { line_index: 0 }
        ));

        let propagated_tag = processor
            .process(
                &line_task_with_propagation(
                    plain_tag_family_identity(),
                    vec![tag_value_identity()],
                    ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                    line_content_analysis(&["炎の剣の説明"]),
                ),
                LlmResponse::new(
                    r#"{"1":["炎之剑>说明"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-propagated-tag-value".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("传播族任一标签目标含 '>' 属于当前 ID 的正常拒绝");
        assert!(matches!(
            propagated_tag.unresolved()[0].reason(),
            TranslationUnitRejectionReason::TagValueContainsClosingDelimiter { line_index: 0 }
        ));

        // 普通 Value 单元不受标签值约束：同样含 '>' 的译文照常进入语言验收流程。
        let accepted = processor
            .process(
                &line_task(
                    reflow_value_identity(),
                    ExpectedLineShape::Reflow,
                    line_content_analysis(&["炎の剣。", "装備すると攻撃力が上がる。"]),
                ),
                LlmResponse::new(
                    r#"{"1":["炎之剑>装备后攻击力上升。"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-plain-value".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("普通 Value 译文应正常处理");
        assert!(!matches!(
            &accepted,
            TranslationTaskOutcome::Unavailable { .. }
        ));
    }

    fn tag_value_identity() -> TranslationUnitIdentity {
        let group = RpgMakerLocation::note_tag(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
            "Help",
            0,
        );
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group,
            TextUnitRole::Scalar(ScalarFieldKey::new("note_tag").expect("字段键应合法")),
            TextUnitContent::Value("炎の剣の説明".to_owned()),
            "{}",
        )
    }

    fn plain_tag_family_identity() -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![RpgMakerLocationStep::index(4)],
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("note_tag").expect("字段键应合法")),
            TextUnitContent::Value("炎の剣の説明".to_owned()),
            "{}",
        )
    }

    #[test]
    fn token_multiset_validation_is_strict_but_allows_reordering() {
        let first = placeholder();
        let second = AppliedPlaceholder::new(
            "⟦ATT_ICON_WHOLE_0001⟧",
            "\\I[2]",
            PlaceholderRuleOrigin::BuiltIn,
            "ICON",
            "event_dialogue",
            PlaceholderSegment::Whole,
        );
        let placeholders = [first, second];

        assert!(
            validate_token_multiset(
                "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧乙⟦ATT_ICON_WHOLE_0001⟧",
                &placeholders,
            )
            .is_ok()
        );
        assert!(
            validate_token_multiset(
                "甲⟦ATT_ICON_WHOLE_0001⟧乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
                &placeholders,
            )
            .is_ok()
        );

        for text in [
            "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
            "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_ICON_WHOLE_0001⟧",
        ] {
            assert!(matches!(
                validate_token_multiset(text, &placeholders),
                Err(TranslationUnitRejectionReason::PlaceholderMismatch { .. })
            ));
        }
        for text in [
            "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_UNKNOWN_WHOLE_9999⟧",
            "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_BROKEN",
        ] {
            assert!(matches!(
                validate_token_multiset(text, &placeholders),
                Err(TranslationUnitRejectionReason::PlaceholderMismatch { token })
                    if token == "⟦ATT_ICON_WHOLE_0001⟧"
            ));
        }

        assert!(matches!(
            validate_token_multiset(
                "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_ICON_WHOLE_0001⟧⟦ATT_UNKNOWN_WHOLE_9999⟧",
                &placeholders,
            ),
            Err(TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token })
                if token == "⟦ATT_UNKNOWN_WHOLE_9999⟧"
        ));
        assert!(matches!(
            validate_token_multiset(
                "甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_ICON_WHOLE_0001⟧⟦ATT_BROKEN",
                &placeholders,
            ),
            Err(TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token })
                if token == "⟦ATT_BROKEN"
        ));
        assert!(matches!(
            validate_token_multiset("甲⟦ATT_UNKNOWN_WHOLE_9999⟧", &[]),
            Err(TranslationUnitRejectionReason::UnexpectedPlaceholderToken { .. })
        ));
        assert!(validate_token_multiset("ATT 说明与 ⟦ATTENTION⟧", &[]).is_ok());
    }

    #[test]
    fn reflow_validation_allows_a_placeholder_to_move_between_lines() {
        let module = japanese_module();
        let restored = validate_and_restore_translation_lines(
            vec![
                "第一行".to_owned(),
                "第二行⟦ATT_ACTOR_NAME_WHOLE_0000⟧".to_owned(),
            ],
            "⟦ATT_ACTOR_NAME_WHOLE_0000⟧と炎の剣",
            ExpectedLineShape::Reflow,
            &[placeholder()],
            &japanese_analysis(),
            module.as_ref(),
        )
        .expect("自由断行只约束整个单元的占位符集合");

        assert_eq!(restored, ["第一行", "第二行\\N[1]"]);
    }

    #[tokio::test]
    async fn unexpected_token_only_rejects_its_own_id() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let result = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    r#"{
                        "1":["甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "2":["乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_UNKNOWN_WHOLE_9999⟧"]
                    }"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-unknown-token".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("未知 token 应作为单个 ID 的正常拒绝");

        assert!(matches!(&result, TranslationTaskOutcome::Partial { .. }));
        assert_eq!(result.accepted().len(), 1);
        assert_eq!(result.accepted()[0].id(), 1);
        assert_eq!(result.unresolved().len(), 1);
        assert_eq!(result.unresolved()[0].id(), 2);
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token }
                if token == "⟦ATT_UNKNOWN_WHOLE_9999⟧"
        ));
    }

    #[test]
    fn restored_reserved_prefix_is_an_internal_invariant_error() {
        let binding = AppliedPlaceholder::new(
            "⟦ATT_TEST_WHOLE_0000⟧",
            "ATT_RESIDUAL",
            PlaceholderRuleOrigin::Custom,
            "TEST",
            "all",
            PlaceholderSegment::Whole,
        );
        let module = japanese_module();
        let analysis = japanese_analysis();

        assert!(matches!(
            validate_and_restore_translation(
                "译文⟦⟦ATT_TEST_WHOLE_0000⟧".to_owned(),
                &[binding],
                &analysis,
                module.as_ref(),
            ),
            Err(TranslationCandidateValidationError::InternalInvariant { .. })
        ));
    }

    #[test]
    fn language_repair_rebuilds_tokens_in_their_translated_order() {
        let first = AppliedPlaceholder::new(
            "⟦ATT_FIRST_WHOLE_0000⟧",
            "<FIRST_ORIGINAL>",
            PlaceholderRuleOrigin::Custom,
            "FIRST",
            "all",
            PlaceholderSegment::Whole,
        );
        let second = AppliedPlaceholder::new(
            "⟦ATT_SECOND_WHOLE_0001⟧",
            "<SECOND_ORIGINAL>",
            PlaceholderRuleOrigin::Custom,
            "SECOND",
            "all",
            PlaceholderSegment::Whole,
        );
        let module = japanese_module();
        let analysis =
            module.analyze_source(&LanguageText::natural("彼は「甲『乙』丙」と言った。"));

        let restored = validate_and_restore_translation(
            "他说：“甲⟦ATT_SECOND_WHOLE_0001⟧乙‘⟦ATT_FIRST_WHOLE_0000⟧’丙。”".to_owned(),
            &[first, second],
            &analysis,
            module.as_ref(),
        )
        .expect("唯一引号结构应修复，并保留译文 token 实际顺序");

        assert_eq!(
            restored,
            "他说：「甲<SECOND_ORIGINAL>乙『<FIRST_ORIGINAL>』丙。」"
        );
    }

    #[tokio::test]
    async fn response_processor_keeps_valid_ids_and_records_every_unavailable_part() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let result = processor
            .process(
                &task_with_output_count(6),
                LlmResponse::new(
                    r#"{
                        "1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "2":123,
                        "2":["乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "4":[""],
                        "5":["缺少控制符"],
                        "6":["译文です⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "99":["未知"]
                    }"#,
                    LlmFinishReason::Length,
                    Some("request-partial".to_owned()),
                    Some("response-partial".to_owned()),
                    None,
                ),
                2,
            )
            .await
            .expect("模型内容的部分不可用必须是正常结果");

        assert!(matches!(&result, TranslationTaskOutcome::Partial { .. }));
        assert_eq!(result.attempts().get(), 2);
        assert_eq!(result.accepted().len(), 1);
        assert_eq!(result.unresolved().len(), 5);
        assert_eq!(result.unresolved()[0].id(), 2);
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
        assert_eq!(result.unresolved()[1].id(), 3);
        assert!(matches!(
            result.unresolved()[1].reason(),
            TranslationUnitRejectionReason::Missing
        ));
        assert_eq!(result.unresolved()[2].id(), 4);
        assert!(matches!(
            result.unresolved()[2].reason(),
            TranslationUnitRejectionReason::BlankLineMismatch {
                line_index: 0,
                expected_blank: false
            }
        ));
        assert_eq!(result.unresolved()[3].id(), 5);
        assert!(matches!(
            result.unresolved()[3].reason(),
            TranslationUnitRejectionReason::PlaceholderMismatch { .. }
        ));
        assert_eq!(result.unresolved()[4].id(), 6);
        assert!(matches!(
            result.unresolved()[4].reason(),
            TranslationUnitRejectionReason::SourceResidual { .. }
        ));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::NonStopFinish { .. }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::UnknownId { id: 99, .. }
        )));
        assert_eq!(result.diagnostics().len(), 2);
    }

    #[tokio::test]
    async fn invalid_ids_and_per_id_shapes_do_not_discard_valid_ids() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let result = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    r#"{
                        "1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "bad":["非法 ID"],
                        "2":[123],
                        "99":["未知 ID"]
                    }"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-schema".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("单项协议错误应成为可持久的部分结果");

        assert!(matches!(&result, TranslationTaskOutcome::Partial { .. }));
        assert_eq!(result.accepted().len(), 1);
        assert_eq!(result.accepted()[0].id(), 1);
        assert_eq!(result.unresolved().len(), 1);
        assert_eq!(result.unresolved()[0].id(), 2);
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::InvalidShape { message }
                if message == "译文数组第 1 项必须是字符串"
        ));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::InvalidId { .. }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::UnknownId { id: 99, .. }
        )));
    }

    #[tokio::test]
    async fn response_processor_returns_persistable_unavailable_outcomes() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let invalid_json = processor
            .process(
                &task(),
                LlmResponse::new(
                    "not-json",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-invalid-json".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("JSON 无法解析属于正常不可用结果");
        assert!(matches!(
            &invalid_json,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
        assert!(matches!(
            invalid_json.unresolved()[0].reason(),
            TranslationUnitRejectionReason::InvalidShape { .. }
        ));
        assert!(matches!(
            invalid_json.diagnostics(),
            [TranslationProtocolDiagnostic::InvalidResponse { .. }]
        ));

        let all_rejected = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"1":[""]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-rejected".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("所有 ID 不合格也属于正常不可用结果");
        assert!(matches!(
            &all_rejected,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::AllOutputsRejected,
                ..
            }
        ));
        assert!(matches!(
            all_rejected.unresolved()[0].reason(),
            TranslationUnitRejectionReason::BlankLineMismatch {
                line_index: 0,
                expected_blank: false
            }
        ));
    }

    #[tokio::test]
    async fn thinking_envelope_preserves_existing_outcome_classification() {
        let processor = TranslationTaskResponseProcessingService::new(
            InlineCpu,
            thinking_translation_resources(),
        );

        let complete = processor
            .process(
                &task(),
                LlmResponse::new(
                    "<why>确认语境、敬语、token 与单行结构。</why>\n{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-thinking-complete".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("合法思考信封后仍应使用既有逐 ID 验收");
        assert!(matches!(complete, TranslationTaskOutcome::Complete { .. }));

        let partial = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    "<why>两个 ID 均已逐项分析，但第二项未能产出。</why>\n{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-thinking-partial".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("思考模式不得改变部分结果语义");
        assert!(matches!(&partial, TranslationTaskOutcome::Partial { .. }));
        assert_eq!(partial.accepted().len(), 1);
        assert!(matches!(
            partial.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Missing
        ));

        let all_rejected = processor
            .process(
                &task(),
                LlmResponse::new(
                    "<why>已分析该 ID，但最终数组留下了不合法的空槽。</why>\n{\"1\":[\"\"]}",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-thinking-rejected".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("思考模式不得改变逐 ID 全拒绝语义");
        assert!(matches!(
            all_rejected,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::AllOutputsRejected,
                ..
            }
        ));

        let unusable = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-thinking-missing".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("思考模式的裸 JSON 应成为模型响应不可用");
        assert!(matches!(
            &unusable,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn thinking_content_is_discarded_before_results_and_diagnostics() {
        const THINKING_SENTINEL: &str = "ATT_THINKING_BODY_SENTINEL_7C4E";

        let processor = TranslationTaskResponseProcessingService::new(
            InlineCpu,
            thinking_translation_resources(),
        );
        let complete = processor
            .process(
                &task(),
                LlmResponse::new(
                    format!(
                        "<why>{THINKING_SENTINEL}</why>{{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}}"
                    ),
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("合法思考内容应在 JSON 验收前丢弃");
        assert!(!format!("{complete:?}").contains(THINKING_SENTINEL));

        let unusable = processor
            .process(
                &task(),
                LlmResponse::new(
                    format!("<why>{THINKING_SENTINEL}</why>not-json"),
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("信封后的非法 JSON 应成为模型响应不可用");
        assert!(matches!(
            &unusable,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
        assert!(!format!("{unusable:?}").contains(THINKING_SENTINEL));
        assert!(
            unusable
                .diagnostics()
                .iter()
                .all(|diagnostic| { !format!("{diagnostic:?}").contains(THINKING_SENTINEL) })
        );
        assert!(
            unusable
                .unresolved()
                .iter()
                .all(|unit| { !format!("{:?}", unit.reason()).contains(THINKING_SENTINEL) })
        );
    }

    #[tokio::test]
    async fn json_only_mode_rejects_thinking_as_model_response_unusable() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let outcome = processor
            .process(
                &task(),
                LlmResponse::new(
                    "<why>不应出现。</why>{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}",
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("JSON-only 模式中的思考内容应成为正常不可用结果");

        assert!(matches!(
            outcome,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn response_cpu_unavailable_is_fatal_instead_of_model_retryable() {
        let processor =
            TranslationTaskResponseProcessingService::new(UnavailableCpu, translation_resources());
        let error = processor
            .process(
                &task(),
                LlmResponse::new(
                    "{}",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-cpu".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect_err("CPU 根不可用必须传播");

        assert!(matches!(
            error,
            TranslationTaskResponseProcessingError::ScheduleCompute(
                CpuTaskExecutionError::Unavailable(FakeError("cpu"))
            )
        ));
    }

    #[tokio::test]
    async fn response_language_pair_mismatch_and_module_mismatch_are_technical_errors() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let language_error = processor
            .process(
                &task_with_language_pair("en", "zh-Hant", 1),
                LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-language".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect_err("任务语言对不一致必须是技术错误");
        assert!(matches!(
            language_error,
            TranslationTaskResponseProcessingError::InternalInvariant { .. }
        ));

        let mismatched_catalog = translation_resources_with("ja", "zh-Hans", english_module());
        let mismatch_error =
            TranslationTaskResponseProcessingService::new(InlineCpu, mismatched_catalog)
                .process(
                    &task_with_language_pair("ja", "zh-Hans", 1),
                    LlmResponse::new(
                        r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-mismatch".to_owned()),
                        None,
                    ),
                    1,
                )
                .await
                .expect_err("译前分析与当前源语言模块不匹配必须是技术错误");
        let TranslationTaskResponseProcessingError::LanguageModule(source) = &mismatch_error else {
            panic!("应返回语言模块错配");
        };
        let detail = source.safe_diagnostic_detail();
        assert!(detail.contains("expected=EnglishLanguageModule"));
        assert!(detail.contains("actual=JapaneseLanguageModule"));

        let invariant_error = processor
            .process(
                &task_with_output_count(0),
                LlmResponse::new(
                    "{}",
                    LlmFinishReason::Stop,
                    None,
                    Some("response-invariant".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect_err("空预期输出破坏 Planner 不变量");
        assert!(matches!(
            invariant_error,
            TranslationTaskResponseProcessingError::InternalInvariant { .. }
        ));
    }

    #[derive(Clone)]
    struct FakeLlm {
        responses: RecordedLlmResponses,
        messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    }

    type FakeLlmResponse = Result<LlmResponse, LlmRequestError<FakeError>>;
    type RecordedLlmResponses = Arc<Mutex<VecDeque<FakeLlmResponse>>>;

    impl LlmRequestExecutor for FakeLlm {
        type Client = &'static str;
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _client: &'a Self::Client,
            messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            self.messages
                .lock()
                .expect("消息锁不应中毒")
                .push(messages.to_vec());
            self.responses
                .lock()
                .expect("响应锁不应中毒")
                .pop_front()
                .expect("测试应准备足够响应")
        }
    }

    #[derive(Clone)]
    struct CancellingLlm {
        cancellation: CooperativeCancellation,
    }

    impl LlmRequestExecutor for CancellingLlm {
        type Client = &'static str;
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _client: &'a Self::Client,
            _messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            self.cancellation.request();
            Err(LlmRequestError::Fatal(FakeError("cancelled-wait")))
        }
    }

    #[derive(Clone)]
    struct FakeDelay {
        waits: Arc<Mutex<Vec<Duration>>>,
    }

    impl AsyncDelay for FakeDelay {
        async fn wait(&self, duration: Duration) {
            self.waits.lock().expect("等待锁不应中毒").push(duration);
        }
    }

    #[derive(Clone)]
    struct BlockingDelay {
        started: Arc<tokio::sync::Semaphore>,
    }

    impl AsyncDelay for BlockingDelay {
        async fn wait(&self, _duration: Duration) {
            self.started.add_permits(1);
            std::future::pending().await
        }
    }

    fn profile() -> RpgMakerTranslationProfile<&'static str> {
        let planning =
            RpgMakerTranslationPlanningConfiguration::new(NonZeroUsize::new(4096).expect("非零"));
        RpgMakerTranslationProfile::new(
            "quality",
            planning,
            RpgMakerTranslationRequestConfiguration::new(
                vec![Duration::from_millis(10), Duration::from_millis(20)],
                Duration::from_secs(2),
            ),
            Arc::new("llm-client"),
        )
    }

    #[tokio::test]
    async fn executor_retries_identical_messages_and_uses_larger_retry_after() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: Some(Duration::from_millis(50)),
                    }),
                    Ok(LlmResponse::new(
                        r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-retry".to_owned()),
                        None,
                    )),
                ]))),
                messages: Arc::clone(&messages),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            CooperativeCancellation::default(),
        );

        service
            .execute(&profile(), task())
            .await
            .expect("第二次响应应成功");

        let messages = messages.lock().expect("消息锁不应中毒");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], messages[1]);
        assert_eq!(
            waits.lock().expect("等待锁不应中毒").as_slice(),
            &[Duration::from_millis(50)]
        );
    }

    #[tokio::test]
    async fn disabled_task_recording_keeps_only_the_logical_attempt_count() {
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-without-record".to_owned()),
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            CooperativeCancellation::default(),
        )
        .with_task_recording(false);
        let task = task();
        let execution =
            <RpgMakerStandardTranslationTaskExecutionService<_, _, _, _> as StandardTranslationTaskExecutor>::execute(
                &service,
                &profile(),
                &task,
            )
            .await
            .expect("关闭任务记录不应改变业务结果");
        let (outcome, evidence) = execution.into_parts();

        assert_eq!(evidence.attempt_count(), outcome.attempts().get());
        assert!(
            !evidence.has_recorded_payload(),
            "关闭记录时不得保留时钟、attempt 文档或原始 Assistant 旁路"
        );
    }

    #[tokio::test]
    async fn business_cancellation_interrupts_retry_after_without_another_request() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let delay_started = Arc::new(tokio::sync::Semaphore::new(0));
        let cancellation = CooperativeCancellation::default();
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: Some(Duration::from_secs(1)),
                    }),
                    Ok(LlmResponse::new(
                        r#"{"1":["不应请求"]}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-unused".to_owned()),
                        None,
                    )),
                ]))),
                messages: Arc::clone(&messages),
            },
            BlockingDelay {
                started: Arc::clone(&delay_started),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            cancellation.clone(),
        );
        let execution = tokio::spawn(async move {
            let profile = profile();
            let task = task();
            <RpgMakerStandardTranslationTaskExecutionService<_, _, _, _> as StandardTranslationTaskExecutor>::execute(
                &service,
                &profile,
                &task,
            )
            .await
        });

        delay_started
            .acquire()
            .await
            .expect("重试等待应开始")
            .forget();
        cancellation.request();
        let result = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("取消必须立即打断 Retry-After 等待")
            .expect("Executor 任务不应 panic");
        let failure = result.expect_err("等待期间取消必须返回已取消执行证据");
        let (source, evidence, diagnostic, cancelled) = failure.into_parts();
        assert!(matches!(
            source,
            RpgMakerStandardTranslationTaskExecutionError::RetryWaitCancelled { attempt: 1 }
        ));
        assert!(cancelled);
        assert!(diagnostic.is_none());
        assert!(
            evidence.has_cancelled_retry_wait(),
            "证据必须区分等待期间取消，不能声称已经等待后重试"
        );
        assert_eq!(
            messages.lock().expect("消息锁不应中毒").len(),
            1,
            "取消后不得再调用模型"
        );
    }

    #[tokio::test]
    async fn llm_admission_cancellation_is_returned_as_a_cancelled_started_task() {
        let cancellation = CooperativeCancellation::default();
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            CancellingLlm {
                cancellation: cancellation.clone(),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            cancellation,
        );
        let task = task();
        let profile = profile();

        let failure = StandardTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("等待 LLM 本地入场时的合作取消必须返回取消终态");
        let (source, evidence, diagnostic, cancelled) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerStandardTranslationTaskExecutionError::FatalRequest {
                attempt: 1,
                source: FakeError("cancelled-wait"),
            }
        ));
        assert_eq!(evidence.attempt_count(), 1);
        assert!(diagnostic.is_none());
        assert!(cancelled);
    }

    #[tokio::test]
    async fn response_cpu_admission_cancellation_is_returned_as_a_cancelled_started_task() {
        let cancellation = CooperativeCancellation::default();
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-cancelled".to_owned()),
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(
                CancellingCpu {
                    cancellation: cancellation.clone(),
                },
                translation_resources(),
            ),
            cancellation,
        );
        let task = task();
        let profile = profile();

        let failure = StandardTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("等待响应 CPU 入场时的合作取消必须返回取消终态");
        let (source, evidence, diagnostic, cancelled) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                attempt: 1,
                source: TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Cancelled
                ),
            }
        ));
        assert_eq!(evidence.attempt_count(), 1);
        assert_eq!(
            evidence.response(),
            Some(&TranslationTaskResponseRecord::unprocessed(
                r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#.to_owned()
            )),
            "CPU 闭包入场前取消时只能保留未处理的原始 Assistant"
        );
        assert!(diagnostic.is_none());
        assert!(cancelled);
    }

    #[tokio::test]
    async fn executor_keeps_parsed_thinking_record_when_later_validation_fails() {
        let raw_assistant =
            "<why>已建立解析证据。</why>\n{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}";
        let service = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    raw_assistant,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-parsed-before-failure".to_owned()),
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(
                InlineCpu,
                thinking_translation_resources(),
            ),
            CooperativeCancellation::default(),
        );
        let task = task_with_language_pair("en", "zh-Hant", 1);
        let profile = profile();

        let failure = StandardTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("解析后的任务语言对不一致必须返回技术失败");
        let (source, evidence, _diagnostic, cancelled) = failure.into_parts();

        assert!(matches!(
            source,
            RpgMakerStandardTranslationTaskExecutionError::ProcessResponse {
                attempt: 1,
                source: TranslationTaskResponseProcessingError::InternalInvariant {
                    invariant: TranslationInternalInvariant::LanguagePairMismatch { .. },
                },
            }
        ));
        assert!(!cancelled);
        assert_eq!(
            evidence.response(),
            Some(&TranslationTaskResponseRecord::parsed(
                raw_assistant.to_owned(),
                Some("已建立解析证据。".to_owned()),
                vec![TranslationAssistantEntry::projected(
                    "1".to_owned(),
                    serde_json::json!(["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]),
                    Some(1),
                    None,
                )],
            )),
            "解析成功后建立的 Thinking 与有序条目必须随技术失败进入 Executor evidence"
        );
    }

    #[tokio::test]
    async fn executor_never_retries_a_per_id_shape_rejection() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let service = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Ok(LlmResponse::new(
                        r#"{"1":123}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-invalid-shape".to_owned()),
                        None,
                    )),
                    Ok(LlmResponse::new(
                        r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-unused".to_owned()),
                        None,
                    )),
                ]))),
                messages: Arc::clone(&messages),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            CooperativeCancellation::default(),
        );
        let outcome = service
            .execute(&profile(), task())
            .await
            .expect("模型内容不可用是正常结果");
        assert!(matches!(
            &outcome,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::AllOutputsRejected,
                ..
            }
        ));
        assert_eq!(messages.lock().expect("消息锁不应中毒").len(), 1);
        assert!(waits.lock().expect("等待锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn executor_never_retries_an_invalid_thinking_envelope() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from([
            Ok(LlmResponse::new(
                r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                LlmFinishReason::Stop,
                None,
                Some("response-missing-thinking".to_owned()),
                None,
            )),
            Ok(LlmResponse::new(
                "<why>该响应不应被消费。</why>{\"1\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}",
                LlmFinishReason::Stop,
                None,
                Some("response-unused".to_owned()),
                None,
            )),
        ])));
        let service = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::clone(&responses),
                messages: Arc::clone(&messages),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(
                InlineCpu,
                thinking_translation_resources(),
            ),
            CooperativeCancellation::default(),
        );

        let outcome = service
            .execute(&profile(), task())
            .await
            .expect("模型信封错误应成为正常不可用结果");
        assert!(matches!(
            outcome,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
        ));
        assert_eq!(messages.lock().expect("消息锁不应中毒").len(), 1);
        assert!(waits.lock().expect("等待锁不应中毒").is_empty());
        assert_eq!(responses.lock().expect("响应锁不应中毒").len(), 1);
    }

    #[tokio::test]
    async fn executor_returns_unavailable_after_network_budget_or_retry_after_limit() {
        for (responses, expected_status, expected_attempts) in [
            (
                VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy-1"),
                        retry_after: None,
                    }),
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy-2"),
                        retry_after: None,
                    }),
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy-3"),
                        retry_after: None,
                    }),
                ]),
                "exhausted",
                3,
            ),
            (
                VecDeque::from([Err(LlmRequestError::Retryable {
                    source: FakeError("busy"),
                    retry_after: Some(Duration::from_secs(3)),
                })]),
                "retry-after",
                1,
            ),
        ] {
            let service = RpgMakerStandardTranslationTaskExecutionService::<
                _,
                _,
                _,
                RpgMakerTranslationProfile<&'static str>,
            >::new(
                FakeLlm {
                    responses: Arc::new(Mutex::new(responses)),
                    messages: Arc::new(Mutex::new(Vec::new())),
                },
                FakeDelay {
                    waits: Arc::new(Mutex::new(Vec::new())),
                },
                TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
                CooperativeCancellation::default(),
            );

            let outcome = service
                .execute(&profile(), task())
                .await
                .expect("可恢复网络预算不足属于正常不可用结果");
            assert_eq!(outcome.attempts().get(), expected_attempts);
            match (expected_status, &outcome) {
                (
                    "exhausted",
                    TranslationTaskOutcome::Unavailable {
                        reason: TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. },
                        ..
                    },
                )
                | (
                    "retry-after",
                    TranslationTaskOutcome::Unavailable {
                        reason:
                            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                                ..
                            },
                        ..
                    },
                ) => {}
                (_, status) => panic!("意外任务状态：{status:?}"),
            }
            let diagnostic = outcome
                .request_diagnostic()
                .expect("重试耗尽必须保留具体安全诊断");
            assert_eq!(
                diagnostic.code,
                crate::diagnostic::DiagnosticCode::ModelRequest
            );
            assert_eq!(
                diagnostic.stage,
                crate::diagnostic::DiagnosticStage::ModelRequest
            );
            assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
            assert!(matches!(
                &diagnostic.reason,
                crate::diagnostic::DiagnosticReason::Http {
                    status: Some(503),
                    retry_after_seconds,
                    provider_code: Some(code),
                    provider_type: Some(kind),
                } if *retry_after_seconds
                    == (expected_status == "retry-after").then_some(3)
                    && code == "temporarily_unavailable"
                    && kind == "service_error"
            ));
            let serialized = serde_json::to_string(diagnostic).expect("任务诊断应可序列化");
            assert!(
                !serialized.contains("busy"),
                "底层任意错误正文不得进入任务诊断"
            );
            assert_eq!(outcome.unresolved().len(), 1);
            assert!(matches!(
                outcome.unresolved()[0].reason(),
                TranslationUnitRejectionReason::Missing
            ));
        }
    }

    #[tokio::test]
    async fn executor_stops_on_fatal_request() {
        let service = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Err(LlmRequestError::Fatal(
                    FakeError("auth"),
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            CooperativeCancellation::default(),
        );

        assert!(matches!(
            service.execute(&profile(), task()).await,
            Err(RpgMakerStandardTranslationTaskExecutionError::FatalRequest { .. })
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-send".to_owned()),
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources()),
            CooperativeCancellation::default(),
        );
        let profile = profile();
        assert_send(service.execute(&profile, task()));
    }
}
