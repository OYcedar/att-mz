//! RPG Maker 翻译任务的模型调用、有限响应清洗与译后验收。
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

use aho_corasick::{Anchored, MatchKind, automaton::Automaton, nfa::noncontiguous::NFA};
use serde_json::value::RawValue;
use time::OffsetDateTime;

#[cfg(test)]
use crate::diagnostic::RpgMakerModelNonStopFinishReason;
use crate::diagnostic::{
    RpgMakerBackendCause, RpgMakerIssue, RpgMakerLanguageModuleKind, RpgMakerModelFinishReason,
    RpgMakerResponseInvariantProblem, RpgMakerResponseLanguageProjectionProblem,
    RpgMakerResponseProcessingProblem, RpgMakerResponseProcessingScope, SafeIdentifier,
    StateEffect,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::isolated::{IsolatedOperationError, run_isolated_operation};
pub(crate) use crate::execution::llm_request::AsyncDelay;
use crate::execution::llm_request::{
    LlmRequestAttemptEvidence, LlmRequestExecutionOutcome, LlmRequestRetryPolicy,
    execute_llm_request_with_retry,
};
use crate::fingerprint::Sha256FramedHasher;
use crate::language::{
    LanguageId, LanguageModule, LanguageModuleError, LanguageModuleKind,
    LanguageOperationCancelled, LanguagePair, LanguageText, LanguageTextSegment,
};
#[cfg(test)]
use crate::llm::LlmRequestError;
use crate::llm::{
    LlmClientConcurrency, LlmFinishReason, LlmRequestExecutor, LlmRequestFailure, LlmResponse,
    LlmUsage,
};
use crate::rpg_maker::model::TextUnitContent;
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, PlaceholderBindingIndex, PlaceholderMultisetError,
    PlaceholderTextScan,
};
use crate::translation::placeholder_token;
use crate::translation::task_planning::TaskId;
use crate::translation_protocol::{
    DecodedJsonStringArray, DecodedSourceEchoFieldsError, DecodedSourceEchoValue,
    DecodedTranslationAssistantValue, TranslationResponseMode,
    parse_translation_response_with_cancellation,
};

use super::pipeline::rpg_maker_diagnostic_unit;
use super::pipeline::{
    AcceptedTranslationDecision, AppliedPlaceholder, ExpectedLineShape, ExpectedTranslationOutput,
    NonEmptyTaskItems, PlaceholderRuleOrigin, RpgMakerExecutableTask,
    RpgMakerTranslationExecutionProfile, RpgMakerTranslationTaskExecutor,
    RpgMakerTranslationTaskIndex, TranslationPatch, TranslationProtocolDiagnostic,
    TranslationTaskOutcome, TranslationTaskOutcomeContext, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason, UnresolvedTranslationUnit,
};
use super::profile::{ResolvedRpgMakerTranslationResources, RpgMakerTranslationProfile};
use super::task_record::{
    TranslationAssistantEntry, TranslationAssistantRecordedValue, TranslationAssistantValueError,
    TranslationTaskAttemptRecord, TranslationTaskExecution, TranslationTaskExecutionEvidence,
    TranslationTaskExecutionFailure, TranslationTaskResponseParseError,
    TranslationTaskResponseRecord,
};
const RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

/// 一次最终成功 HTTP 响应中可安全进入任务结果与持久日志的元数据。
///
/// `provider_request_id` 来自响应头 `x-request-id`，`provider_response_id`
/// 来自 Chat Completions 正文 `id`。供应商可以省略两者，且两者语义不同，
/// 不能相互补位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FinalLlmResponseMetadata {
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: RpgMakerModelFinishReason,
    usage: Option<LlmUsage>,
}

impl FinalLlmResponseMetadata {
    pub(crate) fn new(
        provider_request_id: Option<String>,
        provider_response_id: Option<String>,
        finish_reason: RpgMakerModelFinishReason,
        usage: Option<LlmUsage>,
    ) -> Self {
        Self {
            provider_request_id,
            provider_response_id,
            finish_reason,
            usage,
        }
    }

    fn from_response_with_cancellation<E>(
        response: &LlmResponse,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let provider_request_id = match response.provider_request_id() {
            Some(value) => Some(clone_response_processing_text_with_cancellation(
                value,
                ensure_running,
            )?),
            None => None,
        };
        let provider_response_id = match response.provider_response_id() {
            Some(value) => Some(clone_response_processing_text_with_cancellation(
                value,
                ensure_running,
            )?),
            None => None,
        };
        let finish_reason = match response.finish_reason() {
            LlmFinishReason::Stop => RpgMakerModelFinishReason::Stop,
            LlmFinishReason::Length => RpgMakerModelFinishReason::Length,
            LlmFinishReason::ContentFilter => RpgMakerModelFinishReason::ContentFilter,
            LlmFinishReason::Other(value) => RpgMakerModelFinishReason::provider_specific(
                clone_response_processing_text_with_cancellation(value, ensure_running)?,
            ),
        };
        ensure_running()?;
        Ok(Self::new(
            provider_request_id,
            provider_response_id,
            finish_reason,
            response.usage(),
        ))
    }

    #[cfg(test)]
    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.provider_response_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> &RpgMakerModelFinishReason {
        &self.finish_reason
    }

    #[cfg(test)]
    pub(crate) const fn usage(&self) -> Option<LlmUsage> {
        self.usage
    }
}

/// Executor 从受信 RPG Maker Profile 消费的最小配置面。
pub(crate) trait TranslationTaskExecutionProfile:
    RpgMakerTranslationExecutionProfile
{
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
        task_index: RpgMakerTranslationTaskIndex,
        unit_id: TaskId,
    },
    #[cfg(test)]
    PreparedCandidate,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum TranslationInternalInvariant {
    ResponseAttemptZero {
        task_index: RpgMakerTranslationTaskIndex,
    },
    ExpectedOutputsEmpty {
        task_index: RpgMakerTranslationTaskIndex,
    },
    LanguagePairMismatch {
        task_index: RpgMakerTranslationTaskIndex,
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

impl fmt::Display for TranslationInternalInvariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("翻译响应处理内部不变量已破坏")
    }
}

impl Error for TranslationInternalInvariant {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

pub(crate) trait ResponseProcessingCpuFailure:
    Error + Send + Sync + Sized + 'static
{
    fn diagnostic(error: &CpuTaskExecutionError<Self>) -> crate::diagnostic::Diagnostic;
}

impl ResponseProcessingCpuFailure for CpuExecutorUnavailable {
    fn diagnostic(error: &CpuTaskExecutionError<Self>) -> crate::diagnostic::Diagnostic {
        error.diagnostic()
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
        task: &RpgMakerExecutableTask,
        response: LlmResponse,
        attempt: usize,
    ) -> impl Future<Output = Result<TranslationTaskOutcome, Self::Error>> + Send;

    fn process_recorded(
        &self,
        task: &RpgMakerExecutableTask,
        response: LlmResponse,
        attempt: usize,
    ) -> impl Future<
        Output = Result<
            ProcessedTranslationTaskResponse,
            RecordedTranslationTaskResponseFailure<Self::Error>,
        >,
    > + Send;

    fn task_record_diagnostic(
        &self,
        _task: &RpgMakerExecutableTask,
        _error: &Self::Error,
    ) -> crate::diagnostic::DiagnosticReport;

    /// 错误是否明确来自响应处理等待 CPU 入场或已入场计算中的合作取消。
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
    cancellation: CooperativeCancellation,
}

impl<C> TranslationTaskResponseProcessingService<C> {
    pub(crate) fn new(cpu: C, resources: Arc<ResolvedRpgMakerTranslationResources>) -> Self {
        Self {
            cpu,
            resources,
            cancellation: CooperativeCancellation::default(),
        }
    }

    pub(crate) fn with_cancellation(mut self, cancellation: CooperativeCancellation) -> Self {
        self.cancellation = cancellation;
        self
    }
}

impl<C> TranslationTaskResponseProcessingService<C>
where
    C: CpuTaskExecutor,
{
    async fn process_with_recording(
        &self,
        task: &RpgMakerExecutableTask,
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
            expected_outputs: task.shared_expected_outputs(),
            attempt,
        };
        let resources = Arc::clone(&self.resources);
        let cancellation = self.cancellation.clone();
        let outcome = self
            .cpu
            .execute(move || {
                process_response(input, &response, resources.as_ref(), false, &cancellation)
            })
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
    C::Error: ResponseProcessingCpuFailure,
{
    type Error = TranslationTaskResponseProcessingError<C::Error>;

    async fn process(
        &self,
        task: &RpgMakerExecutableTask,
        response: LlmResponse,
        attempt: usize,
    ) -> Result<TranslationTaskOutcome, Self::Error> {
        self.process_with_recording(task, response, attempt)
            .await
            .map(|processed| processed.outcome)
    }

    async fn process_recorded(
        &self,
        task: &RpgMakerExecutableTask,
        response: LlmResponse,
        attempt: usize,
    ) -> Result<ProcessedTranslationTaskResponse, RecordedTranslationTaskResponseFailure<Self::Error>>
    {
        let raw_assistant = response.shared_content();
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
            expected_outputs: task.shared_expected_outputs(),
            attempt,
        };
        let resources = Arc::clone(&self.resources);
        let cancellation = self.cancellation.clone();
        match self
            .cpu
            .execute(move || {
                process_response(input, &response, resources.as_ref(), true, &cancellation)
            })
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

    fn task_record_diagnostic(
        &self,
        task: &RpgMakerExecutableTask,
        error: &Self::Error,
    ) -> crate::diagnostic::DiagnosticReport {
        error.diagnostic_report(task)
    }

    fn is_cancelled_wait(&self, error: &Self::Error) -> bool {
        matches!(
            error,
            TranslationTaskResponseProcessingError::Cancelled
                | TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Cancelled
                )
        )
    }
}

fn map_response_processing_error<C>(
    error: TranslationResponseTechnicalError,
) -> TranslationTaskResponseProcessingError<C> {
    match error {
        TranslationResponseTechnicalError::Cancelled => {
            TranslationTaskResponseProcessingError::Cancelled
        }
        TranslationResponseTechnicalError::LanguageModule { unit_id, source } => {
            TranslationTaskResponseProcessingError::LanguageModule { unit_id, source }
        }
        TranslationResponseTechnicalError::LanguageProjection { unit_id, source } => {
            TranslationTaskResponseProcessingError::LanguageProjection { unit_id, source }
        }
        TranslationResponseTechnicalError::InternalInvariant { invariant } => {
            TranslationTaskResponseProcessingError::InternalInvariant { invariant }
        }
    }
}

/// 一个响应无法继续处理的技术错误。
#[derive(Debug)]
pub(crate) enum TranslationTaskResponseProcessingError<C> {
    Cancelled,
    ScheduleCompute(CpuTaskExecutionError<C>),
    LanguageModule {
        unit_id: TaskId,
        source: LanguageModuleError,
    },
    LanguageProjection {
        unit_id: TaskId,
        source: LanguageTextProjectionError,
    },
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
            Self::Cancelled => formatter.write_str("译后响应处理已取消"),
            Self::ScheduleCompute(source) => write!(formatter, "调度译后 CPU 验收失败：{source}"),
            Self::LanguageModule { source, .. } => {
                write!(formatter, "译后语言事实不一致：{source}")
            }
            Self::LanguageProjection { source, .. } => {
                write!(formatter, "译后语言投影失败：{source}")
            }
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
            Self::Cancelled => None,
            Self::ScheduleCompute(source) => Some(source),
            Self::LanguageModule { source, .. } => Some(source),
            Self::LanguageProjection { source, .. } => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

impl<C> TranslationTaskResponseProcessingError<C>
where
    C: ResponseProcessingCpuFailure,
{
    /// 在响应处理仍持有 Planner Task 与 Unit 身份时建立唯一公开诊断。
    ///
    /// 状态影响固定为已保存既有进度；调用方不能覆盖阶段、代码、影响或处理办法。
    pub(crate) fn diagnostic_report(
        &self,
        task: &RpgMakerExecutableTask,
    ) -> crate::diagnostic::DiagnosticReport {
        let task_scope = || RpgMakerResponseProcessingScope::task(task.index().get());
        let unit_scope = |unit_id| response_processing_unit_scope(task, unit_id);
        let (scope, problem) = match self {
            Self::Cancelled => (task_scope(), RpgMakerResponseProcessingProblem::Cancelled),
            Self::ScheduleCompute(source) => (
                task_scope(),
                RpgMakerResponseProcessingProblem::Compute {
                    cause: RpgMakerBackendCause::new(C::diagnostic(source)),
                },
            ),
            Self::LanguageModule { unit_id, source } => (
                unit_scope(*unit_id),
                RpgMakerResponseProcessingProblem::LanguageModuleMismatch {
                    expected: language_module_kind(source.expected()),
                    actual: language_module_kind(source.actual()),
                },
            ),
            Self::LanguageProjection { unit_id, source } => (
                unit_scope(*unit_id),
                RpgMakerResponseProcessingProblem::LanguageProjection {
                    problem: response_language_projection_problem(source),
                },
            ),
            Self::InternalInvariant { invariant } => (
                invariant_scope(task, invariant),
                RpgMakerResponseProcessingProblem::InternalInvariant {
                    problem: response_invariant_problem(invariant),
                },
            ),
        };
        crate::diagnostic::DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            crate::diagnostic::Diagnostic::rpg_maker(RpgMakerIssue::response_processing(
                scope, problem,
            )),
        )
    }
}

fn response_processing_unit_scope(
    task: &RpgMakerExecutableTask,
    unit_id: TaskId,
) -> RpgMakerResponseProcessingScope {
    let unit = task
        .expected_outputs()
        .iter()
        .find(|output| output.id() == unit_id)
        .map(|output| rpg_maker_diagnostic_unit(output.identity()));
    match unit {
        Some(unit) => RpgMakerResponseProcessingScope::unit(task.index().get(), unit),
        None => RpgMakerResponseProcessingScope::task(task.index().get()),
    }
}

fn invariant_scope(
    task: &RpgMakerExecutableTask,
    invariant: &TranslationInternalInvariant,
) -> RpgMakerResponseProcessingScope {
    match invariant {
        TranslationInternalInvariant::RepairSegmentRangeMissing { location, .. }
        | TranslationInternalInvariant::RepairLineBoundaryMissing { location, .. }
        | TranslationInternalInvariant::RepairUnassignedSegments { location, .. }
        | TranslationInternalInvariant::ReservedTokenAfterRestore { location } => match location {
            TranslationCandidateInvariantLocation::TaskUnit { unit_id, .. } => {
                response_processing_unit_scope(task, *unit_id)
            }
            #[cfg(test)]
            TranslationCandidateInvariantLocation::PreparedCandidate => {
                RpgMakerResponseProcessingScope::task(task.index().get())
            }
        },
        TranslationInternalInvariant::ResponseAttemptZero { .. }
        | TranslationInternalInvariant::ExpectedOutputsEmpty { .. }
        | TranslationInternalInvariant::LanguagePairMismatch { .. } => {
            RpgMakerResponseProcessingScope::task(task.index().get())
        }
    }
}

const fn language_module_kind(kind: LanguageModuleKind) -> RpgMakerLanguageModuleKind {
    match kind {
        LanguageModuleKind::Japanese => RpgMakerLanguageModuleKind::Japanese,
        LanguageModuleKind::English => RpgMakerLanguageModuleKind::English,
    }
}

fn response_language_projection_problem(
    source: &LanguageTextProjectionError,
) -> RpgMakerResponseLanguageProjectionProblem {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            RpgMakerResponseLanguageProjectionProblem::TokenIndexConstruction
        }
        LanguageTextProjectionError::EmptyToken => {
            RpgMakerResponseLanguageProjectionProblem::EmptyToken
        }
        LanguageTextProjectionError::MissingToken { .. } => {
            RpgMakerResponseLanguageProjectionProblem::MissingToken
        }
        LanguageTextProjectionError::RepeatedToken { .. } => {
            RpgMakerResponseLanguageProjectionProblem::RepeatedToken
        }
        LanguageTextProjectionError::OverlappingToken { .. } => {
            RpgMakerResponseLanguageProjectionProblem::OverlappingToken
        }
        LanguageTextProjectionError::ChangedTokenOrder { position, .. } => {
            RpgMakerResponseLanguageProjectionProblem::ChangedTokenOrder {
                position: *position,
            }
        }
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            RpgMakerResponseLanguageProjectionProblem::ChangedSegmentCount {
                expected: *expected,
                actual: *actual,
            }
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            RpgMakerResponseLanguageProjectionProblem::ChangedSegmentKind {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            RpgMakerResponseLanguageProjectionProblem::MissingOrderedToken {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            RpgMakerResponseLanguageProjectionProblem::UnusedOrderedToken
        }
    }
}

fn response_invariant_problem(
    invariant: &TranslationInternalInvariant,
) -> RpgMakerResponseInvariantProblem {
    match invariant {
        TranslationInternalInvariant::ResponseAttemptZero { .. } => {
            RpgMakerResponseInvariantProblem::ResponseAttemptZero
        }
        TranslationInternalInvariant::ExpectedOutputsEmpty { .. } => {
            RpgMakerResponseInvariantProblem::ExpectedOutputsEmpty
        }
        TranslationInternalInvariant::LanguagePairMismatch {
            task_source,
            task_target,
            resolved_source,
            resolved_target,
            ..
        } => RpgMakerResponseInvariantProblem::LanguagePairMismatch {
            task_source: SafeIdentifier::from_validated(task_source.as_str()),
            task_target: SafeIdentifier::from_validated(task_target.as_str()),
            resolved_source: SafeIdentifier::from_validated(resolved_source.as_str()),
            resolved_target: SafeIdentifier::from_validated(resolved_target.as_str()),
        },
        TranslationInternalInvariant::RepairSegmentRangeMissing {
            line_index,
            start,
            end,
            actual,
            ..
        } => RpgMakerResponseInvariantProblem::RepairSegmentRangeMissing {
            line_index: *line_index,
            start: *start,
            end: *end,
            actual: *actual,
        },
        TranslationInternalInvariant::RepairLineBoundaryMissing {
            line_index,
            segment_index,
            actual,
            ..
        } => RpgMakerResponseInvariantProblem::RepairLineBoundaryMissing {
            line_index: *line_index,
            segment_index: *segment_index,
            actual: *actual,
        },
        TranslationInternalInvariant::RepairUnassignedSegments {
            consumed, actual, ..
        } => RpgMakerResponseInvariantProblem::RepairUnassignedSegments {
            consumed: *consumed,
            actual: *actual,
        },
        TranslationInternalInvariant::ReservedTokenAfterRestore { .. } => {
            RpgMakerResponseInvariantProblem::ReservedTokenAfterRestore
        }
    }
}

#[derive(Debug)]
enum TranslationResponseTechnicalError {
    Cancelled,
    LanguageModule {
        unit_id: TaskId,
        source: LanguageModuleError,
    },
    LanguageProjection {
        unit_id: TaskId,
        source: LanguageTextProjectionError,
    },
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResponseProcessingCancelled;

fn ensure_response_processing_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ResponseProcessingCancelled> {
    if cancellation.is_requested() {
        Err(ResponseProcessingCancelled)
    } else {
        Ok(())
    }
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

fn cancelled_response_processing_failure(
    raw_assistant: Option<Arc<String>>,
) -> TranslationResponseTechnicalFailure {
    TranslationResponseTechnicalFailure::new(
        TranslationResponseTechnicalError::Cancelled,
        raw_assistant.map(TranslationTaskResponseRecord::unprocessed),
    )
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

    fn absorb_request_evidence(&mut self, evidence: LlmRequestAttemptEvidence) {
        let (attempt_count, attempts) = evidence.into_parts();
        self.attempt_count = self.attempt_count.max(attempt_count);
        self.attempts.extend(attempts);
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
pub(crate) struct RpgMakerTranslationTaskExecutionService<L, D, R, P> {
    llm: L,
    delay: D,
    response_processor: R,
    cancellation: CooperativeCancellation,
    record_task_response: bool,
    profile: PhantomData<fn() -> P>,
}

impl<L, D, R, P> RpgMakerTranslationTaskExecutionService<L, D, R, P> {
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

impl<L, D, R, P> RpgMakerTranslationTaskExecutor
    for RpgMakerTranslationTaskExecutionService<L, D, R, P>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestFailure,
    D: AsyncDelay,
    R: TranslationTaskResponseProcessor,
    P: TranslationTaskExecutionProfile<LlmClient = L::Client>,
{
    type Profile = P;
    type Error = RpgMakerTranslationTaskExecutionError<L::Error, R::Error>;

    async fn execute(
        &self,
        profile: &Self::Profile,
        task: &RpgMakerExecutableTask,
    ) -> Result<TranslationTaskExecution, TranslationTaskExecutionFailure<Self::Error>> {
        let mut evidence = TranslationTaskEvidenceBuilder::new(self.record_task_response);
        if task.expected_outputs().is_empty() {
            let invariant = TranslationInternalInvariant::ExpectedOutputsEmpty {
                task_index: task.index(),
            };
            let diagnostic = crate::diagnostic::DiagnosticReport::new(
                StateEffect::Unchanged,
                crate::diagnostic::Diagnostic::rpg_maker(RpgMakerIssue::response_processing(
                    invariant_scope(task, &invariant),
                    RpgMakerResponseProcessingProblem::InternalInvariant {
                        problem: response_invariant_problem(&invariant),
                    },
                )),
            );
            return Err(TranslationTaskExecutionFailure::failed(
                RpgMakerTranslationTaskExecutionError::InternalInvariant { invariant },
                evidence.finish(None),
                diagnostic,
            ));
        }
        let request_execution = execute_llm_request_with_retry(
            &self.llm,
            profile.llm_client(),
            task.messages(),
            LlmRequestRetryPolicy::new(
                profile.network_retry_delays(),
                profile.max_network_retry_after(),
            ),
            &self.delay,
            &self.cancellation,
            self.record_task_response,
        )
        .await;
        let (request_outcome, request_evidence) = request_execution.into_parts();
        evidence.absorb_request_evidence(request_evidence);
        let (response, attempt) = match request_outcome {
            LlmRequestExecutionOutcome::Response { response, attempt } => (response, attempt),
            LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
                attempt,
                diagnostic,
                retry_after,
                maximum,
            } => {
                let outcome = unavailable_after_request_failure(
                    task,
                    attempt,
                    TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                        retry_after,
                        maximum,
                        diagnostic,
                    },
                );
                return Ok(TranslationTaskExecution::new(
                    outcome,
                    evidence.finish(None),
                ));
            }
            LlmRequestExecutionOutcome::RetryBudgetExhausted {
                attempt,
                diagnostic,
            } => {
                let outcome = unavailable_after_request_failure(
                    task,
                    attempt,
                    TranslationTaskUnavailableReason::RecoverableRequestExhausted { diagnostic },
                );
                return Ok(TranslationTaskExecution::new(
                    outcome,
                    evidence.finish(None),
                ));
            }
            LlmRequestExecutionOutcome::Fatal {
                attempt,
                source,
                diagnostic,
            } => {
                return Err(TranslationTaskExecutionFailure::failed(
                    RpgMakerTranslationTaskExecutionError::FatalRequest {
                        attempt: attempt.get(),
                        source,
                    },
                    evidence.finish(None),
                    diagnostic,
                ));
            }
            LlmRequestExecutionOutcome::Cancelled { attempt } => {
                return Err(TranslationTaskExecutionFailure::cancelled(
                    RpgMakerTranslationTaskExecutionError::LlmRequestCancelled {
                        attempt: attempt.get(),
                    },
                    evidence.finish(None),
                ));
            }
        };

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
                    let error = RpgMakerTranslationTaskExecutionError::ProcessResponse {
                        attempt: attempt.get(),
                        source,
                    };
                    if cancelled {
                        Err(TranslationTaskExecutionFailure::cancelled(
                            error,
                            evidence.finish(None),
                        ))
                    } else {
                        let diagnostic = match &error {
                            RpgMakerTranslationTaskExecutionError::ProcessResponse {
                                source,
                                ..
                            } => self.response_processor.task_record_diagnostic(task, source),
                            _ => unreachable!("刚建立的响应处理错误必须保持原始原因"),
                        };
                        Err(TranslationTaskExecutionFailure::failed(
                            error,
                            evidence.finish(None),
                            diagnostic,
                        ))
                    }
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
        match processed {
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
                let error = RpgMakerTranslationTaskExecutionError::ProcessResponse {
                    attempt: attempt.get(),
                    source,
                };
                if cancelled {
                    Err(TranslationTaskExecutionFailure::cancelled(
                        error,
                        evidence.finish(Some(response)),
                    ))
                } else {
                    let diagnostic = match &error {
                        RpgMakerTranslationTaskExecutionError::ProcessResponse {
                            source, ..
                        } => self.response_processor.task_record_diagnostic(task, source),
                        _ => unreachable!("刚建立的响应处理错误必须保持原始原因"),
                    };
                    Err(TranslationTaskExecutionFailure::failed(
                        error,
                        evidence.finish(Some(response)),
                        diagnostic,
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
impl<L, D, R, P> RpgMakerTranslationTaskExecutionService<L, D, R, P>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestFailure,
    D: AsyncDelay,
    R: TranslationTaskResponseProcessor,
    P: TranslationTaskExecutionProfile<LlmClient = L::Client>,
{
    /// 既有 Executor 单元测试只关心权威业务结果；任务记录证据由专门测试覆盖。
    async fn execute(
        &self,
        profile: &P,
        task: RpgMakerExecutableTask,
    ) -> Result<TranslationTaskOutcome, RpgMakerTranslationTaskExecutionError<L::Error, R::Error>>
    {
        match <Self as RpgMakerTranslationTaskExecutor>::execute(self, profile, &task).await {
            Ok(execution) => Ok(execution.into_parts().0),
            Err(TranslationTaskExecutionFailure::Failed { source, .. })
            | Err(TranslationTaskExecutionFailure::Cancelled { source, .. }) => Err(source),
        }
    }
}

fn unavailable_after_request_failure(
    task: &RpgMakerExecutableTask,
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
pub(crate) enum RpgMakerTranslationTaskExecutionError<L, R> {
    FatalRequest {
        attempt: usize,
        source: L,
    },
    ProcessResponse {
        attempt: usize,
        source: R,
    },
    LlmRequestCancelled {
        attempt: usize,
    },
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

impl<L, R> fmt::Display for RpgMakerTranslationTaskExecutionError<L, R>
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
            Self::LlmRequestCancelled { attempt } => {
                write!(formatter, "第 {attempt} 次 LLM 请求已取消")
            }
            Self::InternalInvariant { invariant } => {
                write!(formatter, "翻译任务内部不变量已破坏：{invariant}")
            }
        }
    }
}

impl<L, R> Error for RpgMakerTranslationTaskExecutionError<L, R>
where
    L: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FatalRequest { source, .. } => Some(source),
            Self::ProcessResponse { source, .. } => Some(source),
            Self::LlmRequestCancelled { .. } | Self::InternalInvariant { .. } => None,
        }
    }
}

struct ResponseProcessingInput {
    task_index: RpgMakerTranslationTaskIndex,
    language_pair: LanguagePair,
    expected_outputs: Arc<[ExpectedTranslationOutput]>,
    attempt: NonZeroUsize,
}

fn process_response(
    input: ResponseProcessingInput,
    response: &LlmResponse,
    resources: &ResolvedRpgMakerTranslationResources,
    record_response: bool,
    cancellation: &CooperativeCancellation,
) -> Result<ProcessedTranslationTaskResponse, TranslationResponseTechnicalFailure> {
    let raw_assistant = record_response.then(|| response.shared_content());
    let mut ensure_running = || ensure_response_processing_running(cancellation);
    let language_module = resources.source_language();

    let final_response = match FinalLlmResponseMetadata::from_response_with_cancellation(
        response,
        &mut ensure_running,
    ) {
        Ok(final_response) => final_response,
        Err(ResponseProcessingCancelled) => {
            return Err(cancelled_response_processing_failure(raw_assistant));
        }
    };
    let mut diagnostics = Vec::new();
    if let Some(reason) = final_response.finish_reason().non_stop() {
        diagnostics.push(TranslationProtocolDiagnostic::NonStopFinish { reason });
    }

    let parsed = match parse_model_response_with_cancellation(
        response.content(),
        resources.system_prompt().response_mode(),
        &mut ensure_running,
    ) {
        Ok(parsed) => parsed,
        Err(ResponseProcessingCancelled) => {
            return Err(cancelled_response_processing_failure(raw_assistant));
        }
    };
    let (parsed, response_record) = match parsed {
        Ok(ParsedModelOutputBatch { thinking, outputs }) => match raw_assistant {
            Some(raw_assistant) => {
                let mut entries = Vec::with_capacity(outputs.len());
                let mut acceptance_outputs = Vec::with_capacity(outputs.len());
                for output in outputs {
                    if let Err(ResponseProcessingCancelled) = ensure_running() {
                        return Err(cancelled_response_processing_failure(Some(raw_assistant)));
                    }
                    let ParsedModelOutput {
                        id,
                        value,
                        canonical_id,
                        translation,
                    } = output;
                    let value_error = translation.as_ref().err().copied();
                    let recorded_value = match &translation {
                        Ok(lines) => {
                            let mut recorded_lines = Vec::with_capacity(lines.len());
                            for line in lines {
                                let line = match clone_response_processing_text_with_cancellation(
                                    line,
                                    &mut ensure_running,
                                ) {
                                    Ok(line) => line,
                                    Err(ResponseProcessingCancelled) => {
                                        return Err(cancelled_response_processing_failure(Some(
                                            raw_assistant,
                                        )));
                                    }
                                };
                                recorded_lines.push(line);
                            }
                            TranslationAssistantRecordedValue::Lines(recorded_lines)
                        }
                        Err(_) => TranslationAssistantRecordedValue::RawJson(value),
                    };
                    entries.push(TranslationAssistantEntry::projected(
                        id,
                        recorded_value,
                        canonical_id,
                        value_error,
                    ));
                    acceptance_outputs.push(ModelOutputForAcceptance {
                        canonical_id,
                        translation,
                    });
                }
                let record =
                    TranslationTaskResponseRecord::parsed(raw_assistant, thinking, entries);
                (Ok(acceptance_outputs), Some(record))
            }
            None => {
                let mut acceptance_outputs = Vec::with_capacity(outputs.len());
                for output in outputs {
                    if let Err(ResponseProcessingCancelled) = ensure_running() {
                        return Err(cancelled_response_processing_failure(None));
                    }
                    let ParsedModelOutput {
                        canonical_id,
                        translation,
                        ..
                    } = output;
                    acceptance_outputs.push(ModelOutputForAcceptance {
                        canonical_id,
                        translation,
                    });
                }
                (Ok(acceptance_outputs), None)
            }
        },
        Err(parse_error) => {
            let record = raw_assistant.map(|raw_assistant| {
                TranslationTaskResponseRecord::invalid(raw_assistant, parse_error)
            });
            (Err(parse_error), record)
        }
    };

    if let Err(ResponseProcessingCancelled) = ensure_running() {
        return Err(TranslationResponseTechnicalFailure::new(
            TranslationResponseTechnicalError::Cancelled,
            response_record,
        ));
    }
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

    let outputs = match parsed {
        Ok(outputs) => outputs,
        Err(parse_error) => {
            diagnostics.push(TranslationProtocolDiagnostic::InvalidResponse { error: parse_error });
            let unresolved = match unresolved_all_with_cancellation(
                &input.expected_outputs,
                TranslationUnitRejectionReason::InvalidResponse,
                &mut ensure_running,
            ) {
                Ok(unresolved) => unresolved,
                Err(ResponseProcessingCancelled) => {
                    return Err(TranslationResponseTechnicalFailure::new(
                        TranslationResponseTechnicalError::Cancelled,
                        response_record,
                    ));
                }
            };
            let outcome = TranslationTaskOutcome::Unavailable {
                context: TranslationTaskOutcomeContext::new(
                    input.task_index,
                    input.attempt,
                    diagnostics,
                ),
                final_response: Some(final_response),
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                unresolved: non_empty_known(unresolved, "Executor 已确认任务含有预期输出"),
            };
            return Ok(ProcessedTranslationTaskResponse::new(
                outcome,
                response_record,
            ));
        }
    };

    let mut expected_by_id = BTreeMap::new();
    for output in input.expected_outputs.iter() {
        if let Err(ResponseProcessingCancelled) = ensure_running() {
            return Err(TranslationResponseTechnicalFailure::new(
                TranslationResponseTechnicalError::Cancelled,
                response_record,
            ));
        }
        expected_by_id.insert(output.id(), output);
    }
    let mut actual_by_id = match collect_model_outputs_with_cancellation(
        outputs,
        &expected_by_id,
        &mut diagnostics,
        &mut ensure_running,
    ) {
        Ok(outputs) => outputs,
        Err(ResponseProcessingCancelled) => {
            return Err(TranslationResponseTechnicalFailure::new(
                TranslationResponseTechnicalError::Cancelled,
                response_record,
            ));
        }
    };

    let mut accepted = Vec::with_capacity(expected_by_id.len());
    let mut unresolved = Vec::new();
    for expected in input.expected_outputs.iter() {
        if let Err(ResponseProcessingCancelled) = ensure_running() {
            return Err(TranslationResponseTechnicalFailure::new(
                TranslationResponseTechnicalError::Cancelled,
                response_record,
            ));
        }
        let Some(mut candidates) = actual_by_id.remove(&expected.id()) else {
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
        let translation_lines = match candidates.pop().expect("唯一候选必须存在") {
            Ok(lines) => lines,
            Err(problem) => {
                unresolved.push(unresolved_unit(
                    expected,
                    TranslationUnitRejectionReason::InvalidShape { problem },
                ));
                continue;
            }
        };
        let acceptance = match accept_translation_lines_candidate_at_with_cancellation(
            expected.identity(),
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
            &mut ensure_running,
        ) {
            Ok(acceptance) => acceptance,
            Err(ResponseProcessingCancelled) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::Cancelled,
                    response_record,
                ));
            }
        };
        let translation = match acceptance {
            Ok(TranslationContentAcceptance::Accepted(translation)) => translation,
            Ok(TranslationContentAcceptance::Rejected(reason)) => {
                unresolved.push(unresolved_unit(expected, reason));
                continue;
            }
            Err(TranslationCandidateTechnicalError::LanguageModule(source)) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::LanguageModule {
                        unit_id: expected.id(),
                        source,
                    },
                    response_record,
                ));
            }
            Err(TranslationCandidateTechnicalError::LanguageProjection(source)) => {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::LanguageProjection {
                        unit_id: expected.id(),
                        source,
                    },
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
        let mut propagation_targets = Vec::with_capacity(expected.propagation_targets().len());
        for (identity, state_context) in expected
            .propagation_targets()
            .iter()
            .zip(expected.propagation_state_contexts().iter().copied())
        {
            if let Err(ResponseProcessingCancelled) = ensure_running() {
                return Err(TranslationResponseTechnicalFailure::new(
                    TranslationResponseTechnicalError::Cancelled,
                    response_record,
                ));
            }
            propagation_targets.push(super::pipeline::TranslationPropagationTarget::new(
                identity.clone(),
                state_context,
            ));
        }
        if let Err(ResponseProcessingCancelled) = ensure_running() {
            return Err(TranslationResponseTechnicalFailure::new(
                TranslationResponseTechnicalError::Cancelled,
                response_record,
            ));
        }
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

    if let Err(ResponseProcessingCancelled) = ensure_running() {
        return Err(TranslationResponseTechnicalFailure::new(
            TranslationResponseTechnicalError::Cancelled,
            response_record,
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
    let Some(items) = NonEmptyTaskItems::from_vec(items) else {
        unreachable!("{established_by}");
    };
    items
}

fn validate_translation_lines_with_cancellation<E>(
    identity: &super::pipeline::TranslationUnitIdentity,
    shape: ExpectedLineShape,
    lines: &[String],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<(), TranslationUnitRejectionReason>, E> {
    ensure_running()?;
    if let ExpectedLineShape::Aligned(expected) = shape
        && lines.len() != expected.get()
    {
        return Ok(Err(TranslationUnitRejectionReason::LineCountMismatch {
            expected: expected.get(),
            actual: lines.len(),
        }));
    }
    for (line_index, line) in lines.iter().enumerate() {
        if response_text_contains_invalid_line_character_with_cancellation(
            line,
            &mut ensure_running,
        )? {
            return Ok(Err(TranslationUnitRejectionReason::InvalidLineText {
                line_index,
            }));
        }
    }
    match shape {
        ExpectedLineShape::Reflow => {
            let mut blank = true;
            for line in lines {
                ensure_running()?;
                if !response_text_is_whitespace_with_cancellation(line, &mut ensure_running)? {
                    blank = false;
                    break;
                }
            }
            if blank {
                return Ok(Err(TranslationUnitRejectionReason::BlankTranslation));
            }
        }
        ExpectedLineShape::Aligned(_) => {
            let source_lines = match identity.source_content() {
                TextUnitContent::Value(value) => std::slice::from_ref(value),
                TextUnitContent::Lines(lines) => lines.as_slice(),
            };
            for (line_index, (source, translation)) in source_lines.iter().zip(lines).enumerate() {
                ensure_running()?;
                let expected_blank =
                    response_text_is_whitespace_with_cancellation(source, &mut ensure_running)?;
                let mismatched = if expected_blank {
                    !translation.is_empty()
                } else {
                    response_text_is_whitespace_with_cancellation(translation, &mut ensure_running)?
                };
                if mismatched {
                    return Ok(Err(TranslationUnitRejectionReason::BlankLineMismatch {
                        line_index,
                        expected_blank,
                    }));
                }
            }
        }
    }
    ensure_running()?;
    Ok(Ok(()))
}

fn translation_content_with_cancellation<E>(
    identity: &super::pipeline::TranslationUnitIdentity,
    lines: Vec<String>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<TextUnitContent, E> {
    match identity.source_content() {
        TextUnitContent::Value(_) => {
            let mut capacity = lines.len().saturating_sub(1);
            for line in &lines {
                ensure_running()?;
                capacity = capacity
                    .checked_add(line.len())
                    .expect("译文内容长度必须能由 usize 表示");
            }
            let mut joined = String::with_capacity(capacity);
            for (line_index, line) in lines.iter().enumerate() {
                ensure_running()?;
                if line_index != 0 {
                    joined.push('\n');
                }
                append_response_processing_text_with_cancellation(
                    &mut joined,
                    line,
                    &mut ensure_running,
                )?;
            }
            ensure_running()?;
            Ok(TextUnitContent::Value(joined))
        }
        TextUnitContent::Lines(_) => {
            ensure_running()?;
            Ok(TextUnitContent::Lines(lines))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationContentAcceptance {
    Accepted(TextUnitContent),
    Rejected(TranslationUnitRejectionReason),
}

#[allow(clippy::too_many_arguments)]
fn accept_translation_lines_candidate_at_with_cancellation(
    identity: &super::pipeline::TranslationUnitIdentity,
    protected_text: &str,
    line_shape: ExpectedLineShape,
    placeholders: &[AppliedPlaceholder],
    placeholder_bindings: &PlaceholderBindingIndex,
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
    lines: Vec<String>,
    invariant_location: TranslationCandidateInvariantLocation,
    mut ensure_running: impl FnMut() -> Result<(), ResponseProcessingCancelled>,
) -> Result<
    Result<TranslationContentAcceptance, TranslationCandidateTechnicalError>,
    ResponseProcessingCancelled,
> {
    if let Err(reason) = validate_translation_lines_with_cancellation(
        identity,
        line_shape,
        &lines,
        &mut ensure_running,
    )? {
        return Ok(Ok(TranslationContentAcceptance::Rejected(reason)));
    }
    match validate_and_restore_translation_lines_at_with_cancellation(
        lines,
        TranslationLinesValidationContract {
            protected_text,
            line_shape,
            placeholders,
            placeholder_bindings,
            language_analysis,
            language_module,
        },
        invariant_location,
        &mut ensure_running,
    )? {
        Ok(lines) => Ok(Ok(TranslationContentAcceptance::Accepted(
            translation_content_with_cancellation(identity, lines, &mut ensure_running)?,
        ))),
        Err(TranslationCandidateValidationError::Rejected(reason)) => {
            Ok(Ok(TranslationContentAcceptance::Rejected(reason)))
        }
        Err(TranslationCandidateValidationError::LanguageModule(source)) => Ok(Err(
            TranslationCandidateTechnicalError::LanguageModule(source),
        )),
        Err(TranslationCandidateValidationError::LanguageProjection(source)) => Ok(Err(
            TranslationCandidateTechnicalError::LanguageProjection(source),
        )),
        Err(TranslationCandidateValidationError::InternalInvariant { invariant }) => {
            Ok(Err(TranslationCandidateTechnicalError::InternalInvariant {
                invariant,
            }))
        }
    }
}

#[derive(Debug)]
struct ParsedModelOutput {
    id: String,
    value: Box<RawValue>,
    canonical_id: Option<TaskId>,
    translation: Result<Vec<String>, TranslationAssistantValueError>,
}

#[derive(Debug)]
struct ModelOutputForAcceptance {
    canonical_id: Option<TaskId>,
    translation: Result<Vec<String>, TranslationAssistantValueError>,
}

type ModelOutputsById = BTreeMap<TaskId, Vec<Result<Vec<String>, TranslationAssistantValueError>>>;

fn append_response_processing_text_with_cancellation<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
}

fn clone_response_processing_text_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut cloned = String::with_capacity(text.len());
    append_response_processing_text_with_cancellation(&mut cloned, text, ensure_running)?;
    Ok(cloned)
}

fn response_text_contains_invalid_line_character_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let mut next_check = 0_usize;
    for (byte_offset, character) in text.char_indices() {
        if byte_offset >= next_check {
            ensure_running()?;
            next_check = byte_offset.saturating_add(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES);
        }
        if matches!(character, '\r' | '\n' | '\0') {
            return Ok(true);
        }
    }
    ensure_running()?;
    Ok(false)
}

fn response_text_is_whitespace_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let mut next_check = 0_usize;
    for (byte_offset, character) in text.char_indices() {
        if byte_offset >= next_check {
            ensure_running()?;
            next_check = byte_offset.saturating_add(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES);
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

fn contains_reserved_placeholder_prefix_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    let overlap = placeholder_token::PREFIX.len().saturating_sub(1);
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut primary_end = start
            .saturating_add(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while primary_end < text.len() && !text.is_char_boundary(primary_end) {
            primary_end -= 1;
        }
        let mut search_end = primary_end.saturating_add(overlap).min(text.len());
        while search_end < text.len() && !text.is_char_boundary(search_end) {
            search_end += 1;
        }
        if placeholder_token::contains_reserved_prefix(&text[start..search_end]) {
            return Ok(true);
        }
        if primary_end == text.len() {
            break;
        }
        start = primary_end;
    }
    ensure_running()?;
    Ok(false)
}

fn collect_model_outputs_with_cancellation<E>(
    outputs: Vec<ModelOutputForAcceptance>,
    expected_by_id: &BTreeMap<TaskId, &ExpectedTranslationOutput>,
    diagnostics: &mut Vec<TranslationProtocolDiagnostic>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<ModelOutputsById, E> {
    let mut by_id = ModelOutputsById::new();
    for (item_index, output) in outputs.into_iter().enumerate() {
        ensure_running()?;
        let Some(id) = output.canonical_id else {
            diagnostics.push(TranslationProtocolDiagnostic::InvalidId { item_index });
            continue;
        };
        if !expected_by_id.contains_key(&id) {
            diagnostics.push(TranslationProtocolDiagnostic::UnknownId { item_index, id });
            continue;
        }
        by_id.entry(id).or_default().push(output.translation);
    }
    ensure_running()?;
    Ok(by_id)
}

#[cfg(test)]
fn parse_model_output_id(value: &str) -> Option<TaskId> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok().map(TaskId::new)
}

#[cfg(test)]
fn parse_model_response(
    value: &str,
    response_mode: TranslationResponseMode,
) -> Result<ParsedModelOutputBatch, TranslationTaskResponseParseError> {
    match parse_model_response_with_cancellation(value, response_mode, || {
        Ok::<_, std::convert::Infallible>(())
    }) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

fn parse_model_response_with_cancellation<E>(
    value: &str,
    response_mode: TranslationResponseMode,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<ParsedModelOutputBatch, TranslationTaskResponseParseError>, E> {
    let parsed = match parse_translation_response_with_cancellation(
        value,
        response_mode,
        &mut ensure_running,
    )? {
        Ok(parsed) => parsed,
        Err(source) => return Ok(Err(source)),
    };
    let (thinking, entries) = parsed.into_parts();
    let mut outputs = Vec::with_capacity(entries.len());
    for entry in entries {
        ensure_running()?;
        let decoded = entry.decode_translation_value_with_cancellation(&mut ensure_running)?;
        let translation = translation_lines_from_decoded_value(decoded);
        let (id, value, canonical_id) = entry.into_parts();
        outputs.push(ParsedModelOutput {
            id,
            value,
            canonical_id,
            translation,
        });
    }
    ensure_running()?;
    Ok(Ok(ParsedModelOutputBatch { thinking, outputs }))
}

fn translation_lines_from_decoded_value(
    value: DecodedTranslationAssistantValue,
) -> Result<Vec<String>, TranslationAssistantValueError> {
    match value {
        DecodedTranslationAssistantValue::Translation(translation) => {
            translation_lines_from_array(translation, false)
        }
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::NotObject) => {
            Err(TranslationAssistantValueError::SourceEchoNotObject)
        }
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
            error,
        )) => Err(match error {
            DecodedSourceEchoFieldsError::MissingSource => {
                TranslationAssistantValueError::SourceEchoMissingSource
            }
            DecodedSourceEchoFieldsError::MissingTranslation => {
                TranslationAssistantValueError::SourceEchoMissingTranslation
            }
            DecodedSourceEchoFieldsError::DuplicateSource => {
                TranslationAssistantValueError::SourceEchoDuplicateSource
            }
            DecodedSourceEchoFieldsError::DuplicateTranslation => {
                TranslationAssistantValueError::SourceEchoDuplicateTranslation
            }
            DecodedSourceEchoFieldsError::UnexpectedField { .. } => {
                TranslationAssistantValueError::SourceEchoUnexpectedField
            }
        }),
        DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
            source,
            translation,
        }) => {
            translation_lines_from_array(source, true)?;
            translation_lines_from_array(translation, false)
        }
    }
}

fn translation_lines_from_array(
    value: DecodedJsonStringArray,
    source: bool,
) -> Result<Vec<String>, TranslationAssistantValueError> {
    match value {
        DecodedJsonStringArray::NotArray if source => {
            Err(TranslationAssistantValueError::SourceNotStringArray)
        }
        DecodedJsonStringArray::NonStringItem { item } if source => {
            Err(TranslationAssistantValueError::SourceNonStringItem { item })
        }
        DecodedJsonStringArray::NotArray => Err(TranslationAssistantValueError::NotStringArray),
        DecodedJsonStringArray::NonStringItem { item } => {
            Err(TranslationAssistantValueError::NonStringItem { item })
        }
        DecodedJsonStringArray::Strings(lines) => Ok(lines),
    }
}

#[derive(Debug)]
struct ParsedModelOutputBatch {
    thinking: Option<String>,
    outputs: Vec<ParsedModelOutput>,
}

#[cfg(test)]
fn parse_model_output_batch(
    value: &str,
    response_mode: TranslationResponseMode,
) -> Result<Vec<ParsedModelOutput>, TranslationTaskResponseParseError> {
    parse_model_response(value, response_mode).map(|parsed| parsed.outputs)
}

fn unresolved_unit(
    expected: &ExpectedTranslationOutput,
    reason: TranslationUnitRejectionReason,
) -> UnresolvedTranslationUnit {
    UnresolvedTranslationUnit::new(
        expected.id(),
        rpg_maker_diagnostic_unit(expected.identity()),
        expected.propagation_targets().len(),
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

fn unresolved_all_with_cancellation<E>(
    expected_outputs: &[ExpectedTranslationOutput],
    reason: TranslationUnitRejectionReason,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<UnresolvedTranslationUnit>, E> {
    let mut unresolved = Vec::with_capacity(expected_outputs.len());
    for expected in expected_outputs {
        ensure_running()?;
        unresolved.push(unresolved_unit(expected, reason.clone()));
    }
    ensure_running()?;
    Ok(unresolved)
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
    match validate_and_restore_translation_lines_at_with_cancellation(
        lines,
        TranslationLinesValidationContract {
            protected_text,
            line_shape,
            placeholders,
            placeholder_bindings: &placeholder_bindings,
            language_analysis,
            language_module,
        },
        TranslationCandidateInvariantLocation::PreparedCandidate,
        || Ok(()),
    ) {
        Ok(result) => result,
        Err(ResponseProcessingCancelled) => unreachable!("测试未请求取消"),
    }
}

#[derive(Clone, Copy)]
struct TranslationLinesValidationContract<'a> {
    protected_text: &'a str,
    line_shape: ExpectedLineShape,
    placeholders: &'a [AppliedPlaceholder],
    placeholder_bindings: &'a PlaceholderBindingIndex,
    language_analysis: &'a crate::language::LanguageAnalysis,
    language_module: &'a dyn LanguageModule,
}

fn validate_and_restore_translation_lines_at_with_cancellation(
    mut lines: Vec<String>,
    contract: TranslationLinesValidationContract<'_>,
    invariant_location: TranslationCandidateInvariantLocation,
    mut ensure_running: impl FnMut() -> Result<(), ResponseProcessingCancelled>,
) -> Result<Result<Vec<String>, TranslationCandidateValidationError>, ResponseProcessingCancelled> {
    let TranslationLinesValidationContract {
        protected_text,
        line_shape,
        placeholders,
        placeholder_bindings,
        language_analysis,
        language_module,
    } = contract;
    let mut initial_scans = Vec::with_capacity(lines.len());
    for line in &lines {
        ensure_running()?;
        initial_scans.push(placeholder_bindings.scan_with_cancellation(line, &mut ensure_running)?);
    }
    let normalized_original =
        match normalize_original_placeholder_literals_in_lines_with_cancellation(
            &mut lines,
            placeholders,
            placeholder_bindings,
            &initial_scans,
            &mut ensure_running,
        )? {
            Ok(normalized) => normalized,
            Err(source) => {
                return Ok(Err(source));
            }
        };
    let line_scans = if normalized_original {
        let mut scans = Vec::with_capacity(lines.len());
        for line in &lines {
            ensure_running()?;
            scans.push(placeholder_bindings.scan_with_cancellation(line, &mut ensure_running)?);
        }
        scans
    } else {
        initial_scans
    };
    let line_binding_indices = match line_shape {
        ExpectedLineShape::Reflow => {
            if let Err(source) = placeholder_bindings.validate_multiset_with_cancellation(
                &line_scans,
                placeholder_bindings.all_binding_indices(),
                &mut ensure_running,
            )? {
                return Ok(Err(TranslationCandidateValidationError::Rejected(
                    multiset_rejection(source),
                )));
            }
            let mut indices = Vec::with_capacity(line_scans.len());
            for scan in &line_scans {
                indices.push(
                    placeholder_bindings
                        .present_binding_indices_with_cancellation(scan, &mut ensure_running)?,
                );
            }
            indices
        }
        ExpectedLineShape::Aligned(_) => {
            let mut source_scans = Vec::new();
            for source_line in protected_text.split('\n') {
                ensure_running()?;
                source_scans.push(
                    placeholder_bindings
                        .scan_with_cancellation(source_line, &mut ensure_running)?,
                );
            }
            let mut binding_indices = Vec::with_capacity(source_scans.len());
            for scan in &source_scans {
                binding_indices.push(
                    placeholder_bindings
                        .present_binding_indices_with_cancellation(scan, &mut ensure_running)?,
                );
            }
            for (scan, expected_bindings) in line_scans.iter().zip(&binding_indices) {
                ensure_running()?;
                if let Err(source) = placeholder_bindings.validate_multiset_with_cancellation(
                    std::slice::from_ref(scan),
                    expected_bindings,
                    &mut ensure_running,
                )? {
                    return Ok(Err(TranslationCandidateValidationError::Rejected(
                        multiset_rejection(source),
                    )));
                }
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
        ensure_running()?;
        let projected = match placeholder_bindings.project_with_cancellation(
            line,
            scanned,
            binding_indices,
            &mut ensure_running,
        )? {
            Ok(projected) => projected,
            Err(source) => {
                return Ok(Err(
                    TranslationCandidateValidationError::LanguageProjection(source),
                ));
            }
        };
        for segment in projected.language_text().segments() {
            projected_segments.push(clone_language_segment_with_cancellation(
                segment,
                &mut ensure_running,
            )?);
        }
        line_projections.push(projected);
        if line_index + 1 < lines.len() {
            projected_segments.push(LanguageTextSegment::OpaqueBoundary);
        }
    }

    let projected = LanguageText::new_with_cancellation(projected_segments, &mut ensure_running)?;
    let normalized =
        match normalize_language_text_with_cancellation(&projected, &mut ensure_running)? {
            Ok(normalized) => normalized,
            Err(reason) => {
                return Ok(Err(TranslationCandidateValidationError::Rejected(reason)));
            }
        };
    let residual = {
        let mut language_check =
            || ensure_running().map_err(|ResponseProcessingCancelled| LanguageOperationCancelled);
        match language_module.find_source_residual_with_cancellation(
            language_analysis,
            &normalized,
            &mut language_check,
        ) {
            Ok(Ok(residual)) => residual,
            Ok(Err(source)) => {
                return Ok(Err(TranslationCandidateValidationError::LanguageModule(
                    source,
                )));
            }
            Err(LanguageOperationCancelled) => return Err(ResponseProcessingCancelled),
        }
    };
    if let Some(residual) = residual {
        return Ok(Err(TranslationCandidateValidationError::Rejected(
            TranslationUnitRejectionReason::SourceResidual {
                fragment: clone_response_processing_text_with_cancellation(
                    residual.fragment(),
                    &mut ensure_running,
                )?,
            },
        )));
    }
    let mut restored = Vec::with_capacity(lines.len());
    let mut segment_offset = 0;
    for (line_index, projection) in line_projections.iter().enumerate() {
        let segment_count = projection.language_text().segments().len();
        let line_end = segment_offset + segment_count;
        let Some(repaired_segments) = normalized.segments().get(segment_offset..line_end) else {
            return Ok(Err(
                TranslationCandidateValidationError::InternalInvariant {
                    invariant: TranslationInternalInvariant::RepairSegmentRangeMissing {
                        location: invariant_location,
                        line_index,
                        start: segment_offset,
                        end: line_end,
                        actual: normalized.segments().len(),
                    },
                },
            ));
        };
        let mut line_segments = Vec::with_capacity(repaired_segments.len());
        for segment in repaired_segments {
            line_segments.push(clone_language_segment_with_cancellation(
                segment,
                &mut ensure_running,
            )?);
        }
        let repaired_line =
            LanguageText::new_with_cancellation(line_segments, &mut ensure_running)?;
        let restored_line = match placeholder_bindings.rebuild_original_with_cancellation(
            projection,
            &repaired_line,
            &mut ensure_running,
        )? {
            Ok(restored) => restored,
            Err(source) => {
                return Ok(Err(
                    TranslationCandidateValidationError::LanguageProjection(source),
                ));
            }
        };
        restored.push(restored_line);
        segment_offset = line_end;
        if line_index + 1 < lines.len() {
            if !matches!(
                normalized.segments().get(segment_offset),
                Some(LanguageTextSegment::OpaqueBoundary)
            ) {
                return Ok(Err(
                    TranslationCandidateValidationError::InternalInvariant {
                        invariant: TranslationInternalInvariant::RepairLineBoundaryMissing {
                            location: invariant_location,
                            line_index,
                            segment_index: segment_offset,
                            actual: normalized.segments().len(),
                        },
                    },
                ));
            }
            segment_offset += 1;
        }
    }
    if segment_offset != normalized.segments().len() {
        return Ok(Err(
            TranslationCandidateValidationError::InternalInvariant {
                invariant: TranslationInternalInvariant::RepairUnassignedSegments {
                    location: invariant_location,
                    consumed: segment_offset,
                    actual: normalized.segments().len(),
                },
            },
        ));
    }
    for line in &restored {
        ensure_running()?;
        if contains_reserved_placeholder_prefix_with_cancellation(line, &mut ensure_running)? {
            return Ok(Err(
                TranslationCandidateValidationError::InternalInvariant {
                    invariant: TranslationInternalInvariant::ReservedTokenAfterRestore {
                        location: invariant_location,
                    },
                },
            ));
        }
    }
    ensure_running()?;
    Ok(Ok(restored))
}

#[cfg(test)]
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

#[cfg(test)]
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
    let normalized_original =
        match normalize_original_placeholder_literals_in_lines_with_cancellation(
            std::slice::from_mut(&mut translation),
            placeholders,
            &placeholder_bindings,
            std::slice::from_ref(&initial_scan),
            || Ok::<_, std::convert::Infallible>(()),
        ) {
            Ok(Ok(normalized)) => normalized,
            Ok(Err(source)) => return Err(source),
            Err(unreachable) => match unreachable {},
        };
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
    let restored = placeholder_bindings
        .rebuild_original(&projected, &normalized)
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

#[cfg(test)]
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
        Err(TranslationCandidateValidationError::InternalInvariant { invariant }) => {
            Err(TranslationCandidateTechnicalError::InternalInvariant { invariant })
        }
    }
}

#[derive(Debug)]
pub(crate) enum TranslationCandidateTechnicalError {
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

impl fmt::Display for TranslationCandidateTechnicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LanguageModule(source) => write!(formatter, "语言模块失败：{source}"),
            Self::LanguageProjection(source) => write!(formatter, "语言投影失败：{source}"),
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
            Self::InternalInvariant { .. } => None,
        }
    }
}

#[cfg(test)]
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

fn normalize_language_text_with_cancellation<E>(
    language_text: &LanguageText,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<LanguageText, TranslationUnitRejectionReason>, E> {
    let mut segments = Vec::with_capacity(language_text.segments().len());
    for segment in language_text.segments() {
        ensure_running()?;
        match segment {
            LanguageTextSegment::NaturalText(text) => {
                let mut normalized = String::with_capacity(text.len());
                let mut copy_start = 0_usize;
                let mut skip_until = 0_usize;
                let mut next_check = 0_usize;
                for (byte_offset, character) in text.char_indices() {
                    if byte_offset < skip_until {
                        continue;
                    }
                    if byte_offset >= next_check {
                        ensure_running()?;
                        next_check = byte_offset
                            .saturating_add(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES);
                    }
                    if character == '\u{feff}' {
                        return Ok(Err(TranslationUnitRejectionReason::ContainsByteOrderMark));
                    }
                    if character != '\r' {
                        continue;
                    }
                    append_response_processing_text_with_cancellation(
                        &mut normalized,
                        &text[copy_start..byte_offset],
                        &mut ensure_running,
                    )?;
                    normalized.push('\n');
                    skip_until = byte_offset + 1;
                    if text.as_bytes().get(skip_until) == Some(&b'\n') {
                        skip_until += 1;
                    }
                    copy_start = skip_until;
                }
                append_response_processing_text_with_cancellation(
                    &mut normalized,
                    &text[copy_start..],
                    &mut ensure_running,
                )?;
                segments.push(LanguageTextSegment::NaturalText(normalized));
            }
            LanguageTextSegment::OpaqueBoundary => {
                segments.push(LanguageTextSegment::OpaqueBoundary);
            }
        }
    }
    let normalized = LanguageText::new_with_cancellation(segments, &mut ensure_running)?;
    let mut has_non_whitespace = false;
    for segment in normalized.segments() {
        ensure_running()?;
        if let LanguageTextSegment::NaturalText(text) = segment
            && !response_text_is_whitespace_with_cancellation(text, &mut ensure_running)?
        {
            has_non_whitespace = true;
            break;
        }
    }
    if !has_non_whitespace {
        return Ok(Err(TranslationUnitRejectionReason::NoNaturalLanguageText));
    }
    ensure_running()?;
    Ok(Ok(normalized))
}

fn clone_language_segment_with_cancellation<E>(
    segment: &LanguageTextSegment,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<LanguageTextSegment, E> {
    match segment {
        LanguageTextSegment::NaturalText(text) => Ok(LanguageTextSegment::NaturalText(
            clone_response_processing_text_with_cancellation(text, ensure_running)?,
        )),
        LanguageTextSegment::OpaqueBoundary => {
            ensure_running()?;
            Ok(LanguageTextSegment::OpaqueBoundary)
        }
    }
}

#[derive(Debug)]
enum TranslationCandidateValidationError {
    Rejected(TranslationUnitRejectionReason),
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    InternalInvariant {
        invariant: TranslationInternalInvariant,
    },
}

fn normalize_original_placeholder_literals_in_lines_with_cancellation<E>(
    lines: &mut [String],
    placeholders: &[AppliedPlaceholder],
    placeholder_bindings: &PlaceholderBindingIndex,
    scans: &[PlaceholderTextScan],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<bool, TranslationCandidateValidationError>, E> {
    debug_assert_eq!(lines.len(), scans.len());
    let mut groups = Vec::<OriginalPlaceholderGroup>::new();
    let mut fingerprints =
        std::collections::HashMap::<crate::fingerprint::Sha256Fingerprint, Vec<usize>>::new();
    for (binding_index, placeholder) in placeholders.iter().enumerate() {
        ensure_running()?;
        let fingerprint = response_text_fingerprint_with_cancellation(
            placeholder.original(),
            &mut ensure_running,
        )?;
        let bucket = fingerprints.entry(fingerprint).or_default();
        let mut group_index = None;
        for candidate in bucket.iter().copied() {
            if response_text_equal_with_cancellation(
                placeholders[groups[candidate].representative].original(),
                placeholder.original(),
                &mut ensure_running,
            )? {
                group_index = Some(candidate);
                break;
            }
        }
        let group_index = match group_index {
            Some(group_index) => group_index,
            None => {
                let group_index = groups.len();
                groups.push(OriginalPlaceholderGroup {
                    representative: binding_index,
                    bindings: Vec::new(),
                });
                bucket.push(group_index);
                group_index
            }
        };
        groups[group_index].bindings.push(binding_index);
    }
    let token_counts = placeholder_bindings
        .all_binding_token_occurrences_with_cancellation(scans, &mut ensure_running)?;
    let group_order = sorted_original_placeholder_group_order_with_cancellation(
        &groups,
        placeholders,
        &mut ensure_running,
    )?;

    let mut group_token_states = Vec::with_capacity(groups.len());
    for group in &groups {
        ensure_running()?;
        let mut all_tokens_present = true;
        let mut has_builtin = false;
        for &binding_index in &group.bindings {
            ensure_running()?;
            all_tokens_present &= token_counts[binding_index] != 0;
            has_builtin |= placeholders[binding_index].origin() == PlaceholderRuleOrigin::BuiltIn;
        }
        group_token_states.push(OriginalPlaceholderGroupTokenState {
            all_tokens_present,
            has_builtin,
        });
    }
    let mut groups_requiring_scan = Vec::new();
    for &group_index in &group_order {
        ensure_running()?;
        let state = group_token_states[group_index];
        if !state.all_tokens_present || state.has_builtin {
            groups_requiring_scan.push(group_index);
        }
    }
    let occurrences = match index_original_placeholder_occurrences_with_cancellation(
        lines,
        scans,
        &groups,
        placeholders,
        &groups_requiring_scan,
        &mut ensure_running,
    )? {
        Ok(occurrences) => occurrences,
        Err(source) => {
            return Ok(Err(
                TranslationCandidateValidationError::LanguageProjection(source),
            ));
        }
    };

    let mut replacements = Vec::<OriginalPlaceholderLiteralReplacement<'_>>::new();
    for group_index in group_order {
        ensure_running()?;
        let group = &groups[group_index];
        let original = placeholders[group.representative].original();
        let matched = occurrences.by_group[group_index];
        if matched.count == 0 {
            continue;
        }
        let state = group_token_states[group_index];
        if state.all_tokens_present {
            if state.has_builtin {
                return Ok(Err(TranslationCandidateValidationError::Rejected(
                    TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                        original: clone_response_processing_text_with_cancellation(
                            original,
                            &mut ensure_running,
                        )?,
                    },
                )));
            }
            continue;
        }
        if group.bindings.len() != 1 {
            return Ok(Err(TranslationCandidateValidationError::Rejected(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: clone_response_processing_text_with_cancellation(
                        original,
                        &mut ensure_running,
                    )?,
                },
            )));
        }
        let binding_index = group.bindings[0];
        let binding = &placeholders[binding_index];
        if token_counts[binding_index] == 0 && matched.count == 1 {
            let (line_index, start, end) = matched.first.expect("一次匹配必须保留精确位置");
            replacements.push(OriginalPlaceholderLiteralReplacement {
                line_index,
                start,
                end,
                token: binding.token(),
                original,
            });
        } else {
            return Ok(Err(TranslationCandidateValidationError::Rejected(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: clone_response_processing_text_with_cancellation(
                        original,
                        &mut ensure_running,
                    )?,
                },
            )));
        }
    }

    stable_sort_original_replacements_with_cancellation(&mut replacements, &mut ensure_running)?;
    for pair in replacements.windows(2) {
        ensure_running()?;
        let [previous, current] = pair else {
            unreachable!("windows(2) 始终返回两个元素");
        };
        if previous.line_index == current.line_index && current.start < previous.end {
            return Ok(Err(TranslationCandidateValidationError::Rejected(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: clone_response_processing_text_with_cancellation(
                        current.original,
                        &mut ensure_running,
                    )?,
                },
            )));
        }
    }
    let changed = !replacements.is_empty();
    let mut replacement_index = 0_usize;
    for (line_index, line) in lines.iter_mut().enumerate() {
        ensure_running()?;
        let first = replacement_index;
        while replacements
            .get(replacement_index)
            .is_some_and(|replacement| replacement.line_index == line_index)
        {
            ensure_running()?;
            replacement_index += 1;
        }
        if first == replacement_index {
            continue;
        }
        let line_replacements = &replacements[first..replacement_index];
        let mut capacity = line.len();
        for replacement in line_replacements {
            ensure_running()?;
            capacity = capacity
                .checked_sub(replacement.end - replacement.start)
                .and_then(|capacity| capacity.checked_add(replacement.token.len()))
                .expect("Placeholder 规范化结果长度必须能由 usize 表示");
        }
        let original_line = std::mem::take(line);
        let mut rebuilt = String::with_capacity(capacity);
        let mut cursor = 0_usize;
        for replacement in line_replacements {
            append_response_processing_text_with_cancellation(
                &mut rebuilt,
                &original_line[cursor..replacement.start],
                &mut ensure_running,
            )?;
            append_response_processing_text_with_cancellation(
                &mut rebuilt,
                replacement.token,
                &mut ensure_running,
            )?;
            cursor = replacement.end;
        }
        append_response_processing_text_with_cancellation(
            &mut rebuilt,
            &original_line[cursor..],
            &mut ensure_running,
        )?;
        *line = rebuilt;
    }
    ensure_running()?;
    Ok(Ok(changed))
}

struct OriginalPlaceholderGroup {
    representative: usize,
    bindings: Vec<usize>,
}

#[derive(Clone, Copy)]
struct OriginalPlaceholderGroupTokenState {
    all_tokens_present: bool,
    has_builtin: bool,
}

#[derive(Clone, Copy, Default)]
struct OriginalPlaceholderOccurrences {
    count: u8,
    first: Option<(usize, usize, usize)>,
}

struct OriginalPlaceholderOccurrenceIndex {
    by_group: Vec<OriginalPlaceholderOccurrences>,
    #[cfg(test)]
    scanned_lines: usize,
}

fn index_original_placeholder_occurrences_with_cancellation<E>(
    lines: &[String],
    scans: &[PlaceholderTextScan],
    groups: &[OriginalPlaceholderGroup],
    placeholders: &[AppliedPlaceholder],
    groups_requiring_scan: &[usize],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<OriginalPlaceholderOccurrenceIndex, LanguageTextProjectionError>, E> {
    debug_assert_eq!(lines.len(), scans.len());
    let mut by_group = Vec::with_capacity(groups.len());
    for _ in groups {
        ensure_running()?;
        by_group.push(OriginalPlaceholderOccurrences::default());
    }
    if groups_requiring_scan.is_empty() {
        return Ok(Ok(OriginalPlaceholderOccurrenceIndex {
            by_group,
            #[cfg(test)]
            scanned_lines: 0,
        }));
    }

    let mut patterns = Vec::with_capacity(groups_requiring_scan.len());
    for &group_index in groups_requiring_scan {
        ensure_running()?;
        let original = placeholders[groups[group_index].representative].original();
        if original.is_empty() {
            return Ok(Err(LanguageTextProjectionError::TokenIndexConstruction));
        }
        patterns.push(clone_response_processing_text_with_cancellation(
            original,
            &mut ensure_running,
        )?);
    }
    let pattern_count = patterns.len();
    let matcher = match build_original_placeholder_matcher_with_cancellation(
        patterns,
        &mut ensure_running,
    )? {
        Ok(matcher) => matcher,
        Err(source) => return Ok(Err(source)),
    };
    #[cfg(test)]
    let mut scanned_lines = 0_usize;
    for (line_index, (line, scan)) in lines.iter().zip(scans).enumerate() {
        ensure_running()?;
        #[cfg(test)]
        {
            scanned_lines += 1;
        }
        let mut state = match matcher.start_state(Anchored::No) {
            Ok(state) => state,
            Err(_) => return Ok(Err(LanguageTextProjectionError::TokenIndexConstruction)),
        };
        let mut chunk_start = 0_usize;
        for chunk in line
            .as_bytes()
            .chunks(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
        {
            ensure_running()?;
            for (chunk_offset, &byte) in chunk.iter().enumerate() {
                state = matcher.next_state(Anchored::No, state, byte);
                if !matcher.is_match(state) {
                    continue;
                }
                let end = chunk_start + chunk_offset + 1;
                for match_index in 0..matcher.match_len(state) {
                    ensure_running()?;
                    let pattern_id = matcher.match_pattern(state, match_index);
                    let pattern_index = pattern_id.as_usize();
                    let start = end - matcher.pattern_len(pattern_id);
                    if token_ranges_overlap(scan.token_ranges(), start, end) {
                        continue;
                    }

                    debug_assert!(pattern_index < pattern_count);
                    let group_index = groups_requiring_scan[pattern_index];
                    let matched = &mut by_group[group_index];
                    if matched.count == 0 {
                        matched.first = Some((line_index, start, end));
                    }
                    matched.count = matched.count.saturating_add(1).min(2);
                }
            }
            chunk_start += chunk.len();
        }
    }
    ensure_running()?;
    Ok(Ok(OriginalPlaceholderOccurrenceIndex {
        by_group,
        #[cfg(test)]
        scanned_lines,
    }))
}

fn build_original_placeholder_matcher_with_cancellation<E>(
    patterns: Vec<String>,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<NFA, LanguageTextProjectionError>, E> {
    run_original_placeholder_matcher_build_with_cancellation(
        patterns,
        |patterns| {
            NFA::builder()
                .match_kind(MatchKind::Standard)
                .build(patterns.iter().map(String::as_bytes))
                .map_err(|_| ())
        },
        ensure_running,
    )
}

fn run_original_placeholder_matcher_build_with_cancellation<E>(
    patterns: Vec<String>,
    build: impl FnOnce(Vec<String>) -> Result<NFA, ()> + Send + 'static,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<NFA, LanguageTextProjectionError>, E> {
    match run_isolated_operation(
        "att-response-placeholder-matcher",
        move || build(patterns),
        ensure_running,
    ) {
        Ok(Ok(matcher)) => Ok(Ok(matcher)),
        Ok(Err(())) | Err(IsolatedOperationError::Start { .. }) => {
            Ok(Err(LanguageTextProjectionError::TokenIndexConstruction))
        }
        Err(IsolatedOperationError::Cancelled(cancellation)) => Err(cancellation),
    }
}

fn token_ranges_overlap(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    let candidate = ranges.partition_point(|&(_, token_end)| token_end <= start);
    ranges
        .get(candidate)
        .is_some_and(|&(token_start, _)| token_start < end)
}

fn response_text_fingerprint_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<crate::fingerprint::Sha256Fingerprint, E> {
    let chunk_size = NonZeroUsize::new(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
        .expect("取消检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.response-placeholder-original");
    hasher.try_frame_chunks(1, text.as_bytes(), chunk_size, ensure_running)?;
    Ok(hasher.finish())
}

fn response_text_equal_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    if left.len() != right.len() {
        ensure_running()?;
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
        .zip(
            right
                .as_bytes()
                .chunks(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES),
        )
    {
        ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

fn response_text_cmp_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<std::cmp::Ordering, E> {
    for (left, right) in left
        .as_bytes()
        .chunks(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES)
        .zip(
            right
                .as_bytes()
                .chunks(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES),
        )
    {
        ensure_running()?;
        let ordering = left.cmp(right);
        if ordering != std::cmp::Ordering::Equal {
            return Ok(ordering);
        }
    }
    ensure_running()?;
    Ok(left.len().cmp(&right.len()))
}

fn sorted_original_placeholder_group_order_with_cancellation<E>(
    groups: &[OriginalPlaceholderGroup],
    placeholders: &[AppliedPlaceholder],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<usize>, E> {
    let mut order = Vec::with_capacity(groups.len());
    let mut scratch = Vec::with_capacity(groups.len());
    for index in 0..groups.len() {
        ensure_running()?;
        order.push(index);
        scratch.push(0_usize);
    }
    let mut width = 1_usize;
    while width < order.len() {
        let run_width = width.saturating_mul(2);
        let mut run_start = 0_usize;
        while run_start < order.len() {
            let middle = run_start.saturating_add(width).min(order.len());
            let run_end = run_start.saturating_add(run_width).min(order.len());
            let mut left = run_start;
            let mut right = middle;
            let mut output = run_start;
            while output < run_end {
                ensure_running()?;
                let take_left = right == run_end
                    || (left < middle
                        && response_text_cmp_with_cancellation(
                            placeholders[groups[order[left]].representative].original(),
                            placeholders[groups[order[right]].representative].original(),
                            ensure_running,
                        )? != std::cmp::Ordering::Greater);
                scratch[output] = if take_left {
                    let index = order[left];
                    left += 1;
                    index
                } else {
                    let index = order[right];
                    right += 1;
                    index
                };
                output += 1;
            }
            run_start = run_end;
        }
        std::mem::swap(&mut order, &mut scratch);
        width = run_width;
    }
    ensure_running()?;
    Ok(order)
}

fn stable_sort_original_replacements_with_cancellation<E>(
    replacements: &mut Vec<OriginalPlaceholderLiteralReplacement<'_>>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut scratch = Vec::with_capacity(replacements.len());
    for replacement in replacements.iter().copied() {
        ensure_running()?;
        scratch.push(replacement);
    }
    let mut width = 1_usize;
    while width < replacements.len() {
        let run_width = width.saturating_mul(2);
        let mut run_start = 0_usize;
        while run_start < replacements.len() {
            let middle = run_start.saturating_add(width).min(replacements.len());
            let run_end = run_start.saturating_add(run_width).min(replacements.len());
            let mut left = run_start;
            let mut right = middle;
            let mut output = run_start;
            while output < run_end {
                ensure_running()?;
                let left_key = replacements.get(left).map(|replacement| {
                    (replacement.line_index, replacement.start, replacement.end)
                });
                let right_key = replacements.get(right).map(|replacement| {
                    (replacement.line_index, replacement.start, replacement.end)
                });
                let take_left = right == run_end
                    || (left < middle
                        && left_key.expect("左归并项必须存在")
                            <= right_key.expect("右归并项必须存在"));
                scratch[output] = if take_left {
                    let replacement = replacements[left];
                    left += 1;
                    replacement
                } else {
                    let replacement = replacements[right];
                    right += 1;
                    replacement
                };
                output += 1;
            }
            run_start = run_end;
        }
        std::mem::swap(replacements, &mut scratch);
        width = run_width;
    }
    ensure_running()
}

#[derive(Clone, Copy)]
struct OriginalPlaceholderLiteralReplacement<'a> {
    line_index: usize,
    start: usize,
    end: usize,
    token: &'a str,
    original: &'a str,
}

fn multiset_rejection(error: PlaceholderMultisetError) -> TranslationUnitRejectionReason {
    match error {
        PlaceholderMultisetError::Mismatch { token } => {
            TranslationUnitRejectionReason::PlaceholderMismatch { token }
        }
        PlaceholderMultisetError::Unexpected { token } => {
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { token }
        }
        PlaceholderMultisetError::OrderMismatch { actual_token, .. } => {
            TranslationUnitRejectionReason::PlaceholderMismatch {
                token: actual_token,
            }
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
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex, mpsc};

    use super::*;
    use crate::fingerprint::Sha256Fingerprint;
    use crate::language::{
        EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
        JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageId,
        LanguageModule, LanguagePair, LanguageText, QuotePair,
    };
    use crate::llm::{ChatMessage, ChatMessageRole};
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::rpg_maker::translate::pipeline::{
        AppliedPlaceholder, ExpectedLineShape, ExpectedTranslationOutput,
        ExpectedTranslationValidation, PlaceholderRuleOrigin, PlaceholderSegment,
        RpgMakerTranslationTaskIndex, TranslationStateContext, TranslationUnitIdentity,
    };
    use crate::rpg_maker::translate::profile::{
        ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
        RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
        TranslationResponseMode,
    };
    use crate::runtime::cpu::CpuExecutorUnavailable;
    use crate::translation::profile::TranslationRequestConfiguration;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    fn split_execution_failure<E>(
        failure: TranslationTaskExecutionFailure<E>,
    ) -> (
        E,
        TranslationTaskExecutionEvidence,
        Option<crate::diagnostic::DiagnosticReport>,
        bool,
    ) {
        match failure {
            TranslationTaskExecutionFailure::Failed {
                source,
                evidence,
                diagnostic,
            } => (source, evidence, Some(diagnostic), false),
            TranslationTaskExecutionFailure::Cancelled { source, evidence } => {
                (source, evidence, None, true)
            }
        }
    }

    impl ResponseProcessingCpuFailure for FakeError {
        fn diagnostic(error: &CpuTaskExecutionError<Self>) -> crate::diagnostic::Diagnostic {
            use crate::diagnostic::{RuntimeComponent, RuntimeIssue, RuntimeOperation};

            crate::diagnostic::Diagnostic::runtime(match error {
                CpuTaskExecutionError::Cancelled => RuntimeIssue::Cancelled {
                    component: RuntimeComponent::CpuExecutor,
                    operation: RuntimeOperation::ExecuteTask,
                },
                CpuTaskExecutionError::Unavailable(_) => RuntimeIssue::ExecutorClosed {
                    component: RuntimeComponent::CpuExecutor,
                    operation: RuntimeOperation::ExecuteTask,
                },
                CpuTaskExecutionError::TaskPanicked => RuntimeIssue::WorkerPanicked {
                    component: RuntimeComponent::CpuExecutor,
                    operation: RuntimeOperation::ExecuteTask,
                },
            })
        }
    }

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    impl LlmRequestFailure for FakeError {
        fn is_cancelled_wait(&self) -> bool {
            self.0 == "cancelled-wait"
        }
    }

    fn fake_request_diagnostic(retry_after: Option<Duration>) -> crate::diagnostic::Diagnostic {
        crate::diagnostic::Diagnostic::http(crate::diagnostic::HttpIssue::Status {
            endpoint: crate::diagnostic::HttpEndpoint::new(
                crate::diagnostic::HttpScheme::Https,
                "api.example.test",
                None,
            ),
            status: 503,
            retry_after_seconds: retry_after.map(|value| value.as_secs()),
            provider_code: Some(
                crate::diagnostic::SafeIdentifier::new("temporarily_unavailable")
                    .expect("测试 provider code 合法"),
            ),
            provider_type: Some(
                crate::diagnostic::SafeIdentifier::new("service_error")
                    .expect("测试 provider type 合法"),
            ),
            provider_message: None,
            response_read_failure: None,
        })
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

    #[derive(Clone)]
    struct ActiveCancellingCpu {
        cancellation: CooperativeCancellation,
    }

    impl CpuTaskExecutor for ActiveCancellingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                task()
            });
            barrier.wait();
            self.cancellation.request();
            worker
                .join()
                .map_err(|_| CpuTaskExecutionError::TaskPanicked)
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
        let cases: [(ProductionResponseError, &str); 3] = [
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Cancelled,
                ),
                "runtime.cancelled",
            ),
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown),
                ),
                "runtime.executor_closed",
            ),
            (
                TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::TaskPanicked,
                ),
                "runtime.worker_panicked",
            ),
        ];

        let task = task();
        for (error, expected) in cases {
            let report = error.diagnostic_report(&task);
            assert_eq!(report.effect(), StateEffect::ProgressPreserved);
            assert_eq!(
                report.primary().code(),
                "rpg_maker.translation.response.compute_failed"
            );
            let serialized = serde_json::to_string(&report).expect("响应诊断应可序列化");
            assert!(serialized.contains(expected));
        }
    }

    #[test]
    fn response_diagnostic_treats_projection_as_internal_without_copying_text() {
        let sentinel = "MODEL_OR_TOKEN_BODY_SENTINEL";
        let projection: ProductionResponseError =
            TranslationTaskResponseProcessingError::LanguageProjection {
                unit_id: TaskId::new(0),
                source: LanguageTextProjectionError::MissingToken {
                    token: sentinel.to_owned(),
                },
            };
        let task = task();
        let projection = projection.diagnostic_report(&task);
        assert_eq!(
            projection.primary().code(),
            "rpg_maker.translation.response.missing_token"
        );
        let projection = serde_json::to_string(&projection).expect("投影诊断应可序列化");
        assert!(!projection.contains(sentinel));
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
        translation_resources_with_mode(
            source_language,
            target_language,
            module,
            TranslationResponseMode::new(false, false),
        )
    }

    fn translation_resources_with_mode(
        source_language: &str,
        target_language: &str,
        module: Arc<dyn LanguageModule>,
        response_mode: TranslationResponseMode,
    ) -> Arc<ResolvedRpgMakerTranslationResources> {
        let pair = LanguagePair::new(
            LanguageId::parse(source_language).expect("测试源语言合法"),
            LanguageId::parse(target_language).expect("测试目标语言合法"),
        );
        let prompt = RpgMakerSystemPrompt::new(pair, "# Contract".to_owned(), response_mode)
            .expect("测试 Prompt 合法");
        Arc::new(ResolvedRpgMakerTranslationResources::new(prompt, module))
    }

    fn translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with("ja", "zh-Hans", japanese_module())
    }

    fn thinking_translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with_mode(
            "ja",
            "zh-Hans",
            japanese_module(),
            TranslationResponseMode::new(true, false),
        )
    }

    fn source_echo_translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_with_mode(
            "ja",
            "zh-Hans",
            japanese_module(),
            TranslationResponseMode::new(false, true),
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
            RpgMakerAssetOwner::Builtin,
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
    ) -> RpgMakerExecutableTask {
        line_task_with_propagation(identity, Vec::new(), line_shape, analysis)
    }

    fn line_task_with_propagation(
        identity: TranslationUnitIdentity,
        propagation_targets: Vec<TranslationUnitIdentity>,
        line_shape: ExpectedLineShape,
        analysis: crate::language::LanguageAnalysis,
    ) -> RpgMakerExecutableTask {
        let protected_text = match identity.source_content() {
            TextUnitContent::Value(value) => value.clone(),
            TextUnitContent::Lines(lines) => lines.join("\n"),
        };
        let propagation_state_contexts = (0..propagation_targets.len())
            .map(|index| state_context(index as u8 + 5))
            .collect();
        RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(4),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                task_id(0),
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

    fn task() -> RpgMakerExecutableTask {
        task_with_output_count(1)
    }

    fn task_with_output_count(output_count: usize) -> RpgMakerExecutableTask {
        task_with_language_pair("ja", "zh-Hans", output_count)
    }

    fn speaker_task() -> RpgMakerExecutableTask {
        RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(3),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                task_id(0),
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
    ) -> RpgMakerExecutableTask {
        RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(2),
            LanguagePair::new(
                LanguageId::parse(source_language).expect("测试源语言合法"),
                LanguageId::parse(target_language).expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            (0..output_count)
                .map(|id| {
                    ExpectedTranslationOutput::new(
                        task_id(id),
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
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
    fn four_response_modes_decode_the_same_translation_lines() {
        let cases = [
            (
                TranslationResponseMode::new(false, false),
                r#"{"0":["甲","乙"]}"#,
            ),
            (
                TranslationResponseMode::new(true, false),
                r#"{"think":"结合上下文判断语气。","translations":{"0":["甲","乙"]}}"#,
            ),
            (
                TranslationResponseMode::new(false, true),
                r#"{"0":{"source":["任意回显"],"translation":["甲","乙"]}}"#,
            ),
            (
                TranslationResponseMode::new(true, true),
                r#"{"think":"结合上下文判断语气。","translations":{"0":{"source":["任意回显"],"translation":["甲","乙"]}}}"#,
            ),
        ];

        for (mode, value) in cases {
            let outputs = parse_model_output_batch(value, mode).expect("当前模式响应应合法");
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].id, "0");
            assert!(matches!(
                &outputs[0].translation,
                Ok(lines) if lines == &["甲".to_owned(), "乙".to_owned()]
            ));
        }
    }

    #[test]
    fn thinking_mode_requires_exact_non_blank_json_wrapper() {
        let mode = TranslationResponseMode::new(true, false);
        for value in [
            "{}",
            r#"{"think":"","translations":{}}"#,
            r#"{"think":" \n　\t","translations":{}}"#,
            r#"{"think":[],"translations":{}}"#,
            r#"{"think":"判断","translations":{},"extra":true}"#,
            r#"{"think":"判断","think":"重复","translations":{}}"#,
            r#"{"think":"判断"}"#,
            "```json\n{\"think\":\"判断\",\"translations\":{}}\n```",
        ] {
            assert!(
                parse_model_output_batch(value, mode).is_err(),
                "协议外 thinking 响应必须拒绝：{value:?}"
            );
        }
    }

    #[test]
    fn source_echo_shape_errors_only_reject_the_affected_id() {
        let outputs = parse_model_output_batch(
            r#"{"0":{"source":["不同原文"],"translation":["可接受"]},"1":{"source":"错误","translation":["拒绝"]},"2":{"translation":["拒绝"]},"3":["拒绝"]}"#,
            TranslationResponseMode::new(false, true),
        )
        .expect("逐 ID 形状错误不使整份 JSON 失效");

        assert!(matches!(
            &outputs[0].translation,
            Ok(lines) if lines == &["可接受".to_owned()]
        ));
        assert!(matches!(
            outputs[1].translation,
            Err(TranslationAssistantValueError::SourceNotStringArray)
        ));
        assert!(matches!(
            outputs[2].translation,
            Err(TranslationAssistantValueError::SourceEchoMissingSource)
        ));
        assert!(matches!(
            outputs[3].translation,
            Err(TranslationAssistantValueError::SourceEchoNotObject)
        ));
    }

    #[test]
    fn response_parser_rejects_non_json_and_trailing_content() {
        let mode = TranslationResponseMode::new(false, false);
        for value in [
            "说明：{}",
            "{} 后记",
            "{}\n{}",
            "{\"0\":[\"译文\",]}",
            "{// comment\n}",
            "```json\n{}\n```",
            "{\"0\":[\"截断",
            "[]",
        ] {
            assert!(parse_model_output_batch(value, mode).is_err());
        }
    }

    #[test]
    fn model_output_id_accepts_zero_and_canonical_decimal_keys() {
        let outputs = parse_model_output_batch(
            r#"{"0":["甲"],"1":["乙"]}"#,
            TranslationResponseMode::new(false, false),
        )
        .expect("零和无前导零的 ASCII 十进制键应合法");

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].id, "0");
        assert_eq!(outputs[1].id, "1");
        assert_eq!(parse_model_output_id("0"), Some(task_id(0)));
        assert_eq!(parse_model_output_id("1"), Some(task_id(1)));
        for invalid in ["", "00", "01", "-1", "1.5", "true"] {
            assert_eq!(parse_model_output_id(invalid), None);
        }
        assert_eq!(
            parse_model_output_id("999999999999999999999999999999999999"),
            None
        );
    }

    #[test]
    fn deeply_nested_per_id_values_keep_exact_rpg_shape_errors_and_drop_safely() {
        const DEPTH: usize = 10_000;
        let mode = TranslationResponseMode::new(false, false);

        let deep_object = format!("{}null{}", r#"{"next":"#.repeat(DEPTH), "}".repeat(DEPTH));
        let root_outputs =
            parse_model_output_batch(&format!(r#"{{"0":{deep_object},"1":["保留"]}}"#), mode)
                .expect("深层对象仍是合法的逐 ID JSON 值");
        assert_eq!(root_outputs.len(), 2);
        assert!(matches!(
            &root_outputs[0].translation,
            Err(TranslationAssistantValueError::NotStringArray)
        ));
        assert!(matches!(
            &root_outputs[1].translation,
            Ok(lines) if lines == &["保留".to_owned()]
        ));
        drop(root_outputs);

        let deep_array = format!("{}0{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let item_outputs = parse_model_output_batch(
            &format!(r#"{{"0":["第一项","第二项",{deep_array}],"1":["保留"]}}"#),
            mode,
        )
        .expect("含深层非法数组项的响应仍应按 ID 解析");
        assert!(matches!(
            &item_outputs[0].translation,
            Err(TranslationAssistantValueError::NonStringItem { item })
                if item.get() == 3
        ));
        assert!(matches!(
            &item_outputs[1].translation,
            Ok(lines) if lines == &["保留".to_owned()]
        ));
        drop(item_outputs);
    }

    #[test]
    fn long_escaped_rpg_string_array_decode_observes_cancellation_after_outer_parse() {
        let long_line =
            r"\u4e2d".repeat(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES.saturating_mul(4));
        let raw_assistant = format!(r#"{{"0":["{long_line}"]}}"#);
        let mode = TranslationResponseMode::new(false, false);

        let outer_polls = Cell::new(0_usize);
        let parsed = parse_translation_response_with_cancellation(&raw_assistant, mode, || {
            outer_polls.set(outer_polls.get() + 1);
            Ok::<_, ResponseProcessingCancelled>(())
        })
        .expect("计数运行不应取消")
        .expect("合法外层响应必须解析");
        drop(parsed);

        let cancel_at = outer_polls.get() + 4;
        let polls = Cell::new(0_usize);
        let result = parse_model_response_with_cancellation(&raw_assistant, mode, || {
            let current = polls.get() + 1;
            polls.set(current);
            if current == cancel_at {
                Err(ResponseProcessingCancelled)
            } else {
                Ok(())
            }
        });

        assert!(matches!(result, Err(ResponseProcessingCancelled)));
        assert_eq!(polls.get(), cancel_at);
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
            let content = serde_json::to_string(&serde_json::json!({"0": &expected_lines}))
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
        let task = RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(6),
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
                    task_id(0),
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
                    task_id(1),
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
            r#"{"0":[],"1":["爱丽丝"]}"#,
            r#"{"0":["","   "],"1":["爱丽丝"]}"#,
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
            assert_eq!(outcome.accepted()[0].id(), task_id(1));
            assert_eq!(outcome.unresolved().len(), 1);
            assert_eq!(outcome.unresolved()[0].id(), task_id(0));
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
                    r#"{"0":["炎之剑。","装备后可提升攻击力。"]}"#,
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
                    r#"{"0":["是／否"]}"#,
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
                    r#"{"0":["爱丽","丝"]}"#,
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
                    r#"{"0":["制作人员","","爱丽丝"]}"#,
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
            r#"{"0":["制作人员","填充了空槽","爱丽丝"]}"#,
            r#"{"0":["制作人员","   ","爱丽丝"]}"#,
            r#"{"0":["","","爱丽丝"]}"#,
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
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventChoices,
            group,
            TextUnitRole::Choices,
            TextUnitContent::Lines(vec!["\\N[1]に話す".to_owned(), "やめる".to_owned()]),
            "{}",
        );
        let task = RpgMakerExecutableTask::new(
            RpgMakerTranslationTaskIndex::new(5),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
            ),
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                task_id(0),
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
                    r#"{"0":["和他交谈","取消⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
            RpgMakerAssetOwner::Builtin,
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
            task_id(0),
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
            super::super::pipeline::ExpectedTranslationOutputContractError::
                ProtectedPlaceholderCrossesLineBoundary {
                    unit_id,
                    placeholder_index: 0,
                    ..
                } if unit_id == task_id(0)
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
                    r#"{"0":["炎之剑\\N[1]！"]}"#,
                    LlmFinishReason::Stop,
                    Some("request-1".to_owned()),
                    Some("response-1".to_owned()),
                    Some(LlmUsage::new(10, 5, 15)),
                ),
                1,
            )
            .await
            .expect("原控制符能够唯一对应时应规范化并恢复");

        assert_eq!(result.task_index(), RpgMakerTranslationTaskIndex::new(2));
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
            &[super::super::pipeline::TranslationPropagationTarget::new(
                propagation_target(),
                state_context(101),
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
                    r#"{"0":["炎之剑\\N[1]！"]}"#,
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
                    r#"{"0":["炎\uFEFF之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
                    r#"{"0":["  ⟦ATT_ACTOR_NAME_WHOLE_0000⟧  "]}"#,
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
            let content = serde_json::to_string(&serde_json::json!({"0": [translation]}))
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
    async fn response_processor_preserves_plain_angle_brackets() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());

        let accepted = processor
            .process(
                &line_task(
                    reflow_value_identity(),
                    ExpectedLineShape::Reflow,
                    line_content_analysis(&["炎の剣。", "装備すると攻撃力が上がる。"]),
                ),
                LlmResponse::new(
                    r#"{"0":["<Help:炎之剑>装备后攻击力上升。"]}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-plain-value".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("裸尖括号属于普通文本内容");
        assert_eq!(
            accepted.accepted()[0].translation(),
            &TextUnitContent::Value("<Help:炎之剑>装备后攻击力上升。".to_owned())
        );
    }

    #[test]
    fn overlapping_original_echoes_are_rejected_as_ambiguous() {
        let placeholders = vec![AppliedPlaceholder::new(
            "⟦ATT_CUSTOM_WHOLE_0000⟧",
            "aa",
            PlaceholderRuleOrigin::Custom,
            "CUSTOM",
            "event_dialogue",
            PlaceholderSegment::Whole,
        )];
        let bindings =
            PlaceholderBindingIndex::new(&placeholders).expect("测试 token 索引应可建立");
        let mut lines = vec!["aaa".to_owned()];
        let scans = lines
            .iter()
            .map(|line| bindings.scan(line))
            .collect::<Vec<_>>();

        let result = normalize_original_placeholder_literals_in_lines_with_cancellation(
            &mut lines,
            &placeholders,
            &bindings,
            &scans,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试没有请求取消");

        assert!(matches!(
            result,
            Err(TranslationCandidateValidationError::Rejected(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { original }
            )) if original == "aa"
        ));
    }

    #[test]
    fn token_overlapping_echo_does_not_hide_adjacent_natural_echo() {
        let missing_token = "⟦ATT_CUSTOM_WHOLE_0000⟧";
        let present_token = "⟦ATT_OTHER_WHOLE_0001⟧";
        let placeholders = vec![
            AppliedPlaceholder::new(
                missing_token,
                "⟧⟧",
                PlaceholderRuleOrigin::Custom,
                "CUSTOM",
                "event_dialogue",
                PlaceholderSegment::Whole,
            ),
            AppliedPlaceholder::new(
                present_token,
                "<other>",
                PlaceholderRuleOrigin::Custom,
                "OTHER",
                "event_dialogue",
                PlaceholderSegment::Whole,
            ),
        ];
        let bindings =
            PlaceholderBindingIndex::new(&placeholders).expect("测试 token 索引应可建立");
        let mut lines = vec![format!("{present_token}⟧⟧")];
        let scans = lines
            .iter()
            .map(|line| bindings.scan(line))
            .collect::<Vec<_>>();

        let changed = normalize_original_placeholder_literals_in_lines_with_cancellation(
            &mut lines,
            &placeholders,
            &bindings,
            &scans,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试没有请求取消")
        .expect("token 外唯一回显应该可以规范化");

        assert!(changed);
        assert_eq!(lines, [format!("{present_token}{missing_token}")]);
    }

    #[test]
    fn active_original_matcher_build_observes_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let caller_cancelled = Arc::clone(&cancelled);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let caller = std::thread::spawn(move || {
            let result = run_original_placeholder_matcher_build_with_cancellation(
                vec!["aa".to_owned()],
                move |_| {
                    started_sender
                        .send(())
                        .expect("应通知测试 matcher worker 已启动");
                    release_receiver.recv().expect("应释放测试 matcher worker");
                    finished_sender
                        .send(())
                        .expect("应通知测试 matcher worker 已结束");
                    Err(())
                },
                move || {
                    if caller_cancelled.load(Ordering::Acquire) {
                        Err("cancelled")
                    } else {
                        Ok(())
                    }
                },
            );
            result_sender.send(result).expect("应返回 matcher 构建结果");
        });

        started_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("matcher worker 应在取消前实际开始运行");
        cancelled.store(true, Ordering::Release);
        let result = result_receiver.recv_timeout(std::time::Duration::from_secs(1));
        release_sender
            .send(())
            .expect("取消测试必须释放 matcher worker");
        finished_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("取消测试必须回收 matcher worker 的纯计算");
        caller.join().expect("matcher 调用线程应正常结束");

        assert!(matches!(result, Ok(Err("cancelled"))));
    }

    #[test]
    fn thousands_of_distinct_original_placeholders_scan_a_long_candidate_once() {
        const PLACEHOLDER_COUNT: usize = 2_048;

        let placeholders = (0..PLACEHOLDER_COUNT)
            .map(|index| {
                AppliedPlaceholder::new(
                    format!("⟦ATT_STRESS_WHOLE_{index:04}⟧"),
                    format!("<ORIGINAL_{index:04}>"),
                    PlaceholderRuleOrigin::Custom,
                    "STRESS",
                    "all",
                    PlaceholderSegment::Whole,
                )
            })
            .collect::<Vec<_>>();
        let bindings = PlaceholderBindingIndex::new(&placeholders).expect("token 索引应可建立");
        let mut candidate =
            "长".repeat(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES * 4 / "长".len());
        let mut expected = candidate.clone();
        for placeholder in &placeholders {
            candidate.push('|');
            candidate.push_str(placeholder.original());
            expected.push('|');
            expected.push_str(placeholder.token());
        }
        let scans = vec![bindings.scan(&candidate)];
        let groups = placeholders
            .iter()
            .enumerate()
            .map(|(binding_index, _)| OriginalPlaceholderGroup {
                representative: binding_index,
                bindings: vec![binding_index],
            })
            .collect::<Vec<_>>();
        let group_indices = (0..groups.len()).collect::<Vec<_>>();
        let indexed = index_original_placeholder_occurrences_with_cancellation(
            std::slice::from_ref(&candidate),
            &scans,
            &groups,
            &placeholders,
            &group_indices,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试没有请求取消")
        .expect("多模式索引应可建立");
        assert_eq!(
            indexed.scanned_lines, 1,
            "所有原片段必须在同一次候选扫描中完成匹配"
        );
        assert!(
            indexed.by_group.iter().all(|matched| matched.count == 1),
            "每个 distinct 原片段都应精确匹配一次"
        );

        let normalized = normalize_original_placeholder_literals_in_lines_with_cancellation(
            std::slice::from_mut(&mut candidate),
            &placeholders,
            &bindings,
            &scans,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试没有请求取消")
        .expect("唯一原片段应可规范化");
        assert!(normalized);
        assert_eq!(candidate, expected);

        candidate.push_str("|<ORIGINAL_0000>");
        let all_tokens_present_scan = vec![bindings.scan(&candidate)];
        let unchanged = candidate.clone();
        let normalized = normalize_original_placeholder_literals_in_lines_with_cancellation(
            std::slice::from_mut(&mut candidate),
            &placeholders,
            &bindings,
            &all_tokens_present_scan,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试没有请求取消")
        .expect("Custom 原片段在对应 token 已存在时属于自然文本");
        assert!(!normalized);
        assert_eq!(candidate, unchanged);
    }

    #[test]
    fn token_validation_rejects_count_identity_and_order_changes() {
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
        assert!(matches!(
            validate_token_multiset(
                "甲⟦ATT_ICON_WHOLE_0001⟧乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
                &placeholders,
            ),
            Err(TranslationUnitRejectionReason::PlaceholderMismatch { token })
                if token == "⟦ATT_ICON_WHOLE_0001⟧"
        ));

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
                        "0":["甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "1":["乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_UNKNOWN_WHOLE_9999⟧"]
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
        assert_eq!(result.accepted()[0].id(), task_id(0));
        assert_eq!(result.unresolved().len(), 1);
        assert_eq!(result.unresolved()[0].id(), task_id(1));
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
    fn translation_acceptance_preserves_quote_style_and_rebuilds_tokens_in_source_order() {
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
            "他说：“甲⟦ATT_FIRST_WHOLE_0000⟧乙‘⟦ATT_SECOND_WHOLE_0001⟧’丙。”".to_owned(),
            &[first, second],
            &analysis,
            module.as_ref(),
        )
        .expect("合格译文应保持原样，并恢复源 token 顺序");

        assert_eq!(
            restored,
            "他说：“甲<FIRST_ORIGINAL>乙‘<SECOND_ORIGINAL>’丙。”"
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
                        "0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "1":123,
                        "1":["乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "3":[""],
                        "4":["缺少控制符"],
                        "5":["译文です⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
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
        assert_eq!(result.unresolved()[0].id(), task_id(1));
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
        assert_eq!(result.unresolved()[1].id(), task_id(2));
        assert!(matches!(
            result.unresolved()[1].reason(),
            TranslationUnitRejectionReason::Missing
        ));
        assert_eq!(result.unresolved()[2].id(), task_id(3));
        assert!(matches!(
            result.unresolved()[2].reason(),
            TranslationUnitRejectionReason::BlankLineMismatch {
                line_index: 0,
                expected_blank: false
            }
        ));
        assert_eq!(result.unresolved()[3].id(), task_id(4));
        assert!(matches!(
            result.unresolved()[3].reason(),
            TranslationUnitRejectionReason::PlaceholderMismatch { .. }
        ));
        assert_eq!(result.unresolved()[4].id(), task_id(5));
        assert!(matches!(
            result.unresolved()[4].reason(),
            TranslationUnitRejectionReason::SourceResidual { .. }
        ));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::NonStopFinish {
                reason: RpgMakerModelNonStopFinishReason::Length
            }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::UnknownId { id, .. } if *id == task_id(99)
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
                        "0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"],
                        "bad":["非法 ID"],
                        "1":[123],
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
        assert_eq!(result.accepted()[0].id(), task_id(0));
        assert_eq!(result.unresolved().len(), 1);
        assert_eq!(result.unresolved()[0].id(), task_id(1));
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::InvalidShape {
                problem: TranslationAssistantValueError::NonStringItem { item }
            } if *item == NonZeroUsize::MIN
        ));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::InvalidId { .. }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::UnknownId { id, .. } if *id == task_id(99)
        )));
    }

    #[tokio::test]
    async fn ten_thousand_deep_wrong_value_rejects_only_its_id_as_partial() {
        const DEPTH: usize = 10_000;

        let deep_value = format!("{}0{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let raw_assistant =
            format!(r#"{{"0":{deep_value},"1":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#);
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let outcome = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    raw_assistant,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-deep-partial".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("深层逐 ID 形状错误必须保留其他 ID 的可持久结果");

        assert!(matches!(&outcome, TranslationTaskOutcome::Partial { .. }));
        assert_eq!(outcome.accepted().len(), 1);
        assert_eq!(outcome.accepted()[0].id(), task_id(1));
        assert_eq!(outcome.unresolved().len(), 1);
        assert_eq!(outcome.unresolved()[0].id(), task_id(0));
        assert!(matches!(
            outcome.unresolved()[0].reason(),
            TranslationUnitRejectionReason::InvalidShape {
                problem: TranslationAssistantValueError::NonStringItem { item }
            } if *item == NonZeroUsize::MIN
        ));
        drop(outcome);
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
            TranslationUnitRejectionReason::InvalidResponse
        ));
        assert!(matches!(
            invalid_json.diagnostics(),
            [TranslationProtocolDiagnostic::InvalidResponse { .. }]
        ));

        let all_rejected = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"0":[""]}"#,
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
    async fn thinking_mode_preserves_existing_outcome_classification() {
        let processor = TranslationTaskResponseProcessingService::new(
            InlineCpu,
            thinking_translation_resources(),
        );

        let complete = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"think":"确认语境、敬语、token 与单行结构。","translations":{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-thinking-complete".to_owned()),
                    None,
                ),
                1,
            )
            .await
            .expect("合法 thinking 响应仍应使用既有逐 ID 验收");
        assert!(matches!(complete, TranslationTaskOutcome::Complete { .. }));

        let partial = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    r#"{"think":"两个 ID 均已逐项分析，但第二项未能产出。","translations":{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#,
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
                    r#"{"think":"已分析该 ID，但最终数组留下了不合法的空槽。","translations":{"0":[""]}}"#,
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
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
    async fn source_echo_content_does_not_participate_in_rpg_acceptance() {
        let processor = TranslationTaskResponseProcessingService::new(
            InlineCpu,
            source_echo_translation_resources(),
        );
        let outcome = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"0":{"source":["与请求完全不同"],"translation":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("source 内容不同不应影响按 ID 验收");

        assert!(matches!(outcome, TranslationTaskOutcome::Complete { .. }));
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
                        "{{\"think\":\"{THINKING_SENTINEL}\",\"translations\":{{\"0\":[\"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧\"]}}}}"
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
                    format!("{{\"think\":\"{THINKING_SENTINEL}\",\"translations\":not-json}}"),
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("非法 translations JSON 应成为模型响应不可用");
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
    async fn plain_mode_treats_thinking_fields_as_invalid_ids() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, translation_resources());
        let outcome = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"{"think":"不应出现。","translations":{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("plain 模式中的额外思考字段应成为正常不可用结果");

        assert!(matches!(
            outcome,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::AllOutputsRejected,
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
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
                        r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-mismatch".to_owned()),
                        None,
                    ),
                    1,
                )
                .await
                .expect_err("译前分析与当前源语言模块不匹配必须是技术错误");
        let TranslationTaskResponseProcessingError::LanguageModule { source, .. } = &mismatch_error
        else {
            panic!("应返回语言模块错配");
        };
        assert_eq!(source.expected(), LanguageModuleKind::English);
        assert_eq!(source.actual(), LanguageModuleKind::Japanese);

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

        fn request_diagnostic(
            &self,
            _client: &Self::Client,
            _source: &Self::Error,
            retry_after: Option<Duration>,
        ) -> crate::diagnostic::Diagnostic {
            fake_request_diagnostic(retry_after)
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

        fn request_diagnostic(
            &self,
            _client: &Self::Client,
            _source: &Self::Error,
            retry_after: Option<Duration>,
        ) -> crate::diagnostic::Diagnostic {
            fake_request_diagnostic(retry_after)
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
            TranslationRequestConfiguration::new(
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
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: Some(Duration::from_millis(50)),
                    }),
                    Ok(LlmResponse::new(
                        r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
            <RpgMakerTranslationTaskExecutionService<_, _, _, _> as RpgMakerTranslationTaskExecutor>::execute(
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
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: Some(Duration::from_secs(1)),
                    }),
                    Ok(LlmResponse::new(
                        r#"{"0":["不应请求"]}"#,
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
            <RpgMakerTranslationTaskExecutionService<_, _, _, _> as RpgMakerTranslationTaskExecutor>::execute(
                &service,
                &profile,
                &task,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), delay_started.acquire())
            .await
            .expect("重试等待必须在一秒内开始")
            .expect("重试等待应开始")
            .forget();
        cancellation.request();
        let result = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("取消必须立即打断 Retry-After 等待")
            .expect("Executor 任务不应 panic");
        let failure = result.expect_err("等待期间取消必须返回已取消执行证据");
        let (source, evidence, diagnostic, cancelled) = split_execution_failure(failure);
        assert!(matches!(
            source,
            RpgMakerTranslationTaskExecutionError::LlmRequestCancelled { attempt: 1 }
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
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
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

        let failure = RpgMakerTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("等待 LLM 本地入场时的合作取消必须返回取消终态");
        let (source, evidence, diagnostic, cancelled) = split_execution_failure(failure);

        assert!(matches!(
            source,
            RpgMakerTranslationTaskExecutionError::LlmRequestCancelled { attempt: 1 }
        ));
        assert_eq!(evidence.attempt_count(), 1);
        assert!(diagnostic.is_none());
        assert!(cancelled);
        assert!(
            !evidence.has_cancelled_retry_wait(),
            "等待 LLM 本地入场的取消不得伪装成 Retry-After 等待"
        );
    }

    #[tokio::test]
    async fn response_cpu_admission_cancellation_is_returned_as_a_cancelled_started_task() {
        let cancellation = CooperativeCancellation::default();
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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

        let failure = RpgMakerTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("等待响应 CPU 入场时的合作取消必须返回取消终态");
        let (source, evidence, diagnostic, cancelled) = split_execution_failure(failure);

        assert!(matches!(
            source,
            RpgMakerTranslationTaskExecutionError::ProcessResponse {
                attempt: 1,
                source: TranslationTaskResponseProcessingError::ScheduleCompute(
                    CpuTaskExecutionError::Cancelled
                ),
            }
        ));
        assert_eq!(evidence.attempt_count(), 1);
        let response = evidence
            .response()
            .expect("CPU 闭包入场前取消时应保留原始 Assistant");
        assert_eq!(
            response.raw_assistant(),
            r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#
        );
        assert!(response.thinking().is_none());
        assert!(response.ordered_entries().is_none());
        assert!(response.parse_error().is_none());
        assert!(diagnostic.is_none());
        assert!(cancelled);
    }

    #[tokio::test]
    async fn active_response_cpu_processing_observes_shared_cancellation() {
        let cancellation = CooperativeCancellation::default();
        let long_translation =
            "x".repeat(RESPONSE_PROCESSING_CANCELLATION_CHECK_BYTES.saturating_mul(8));
        let raw_assistant = serde_json::to_string(&serde_json::json!({
            "0": [long_translation],
        }))
        .expect("测试响应应可序列化");
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    raw_assistant.clone(),
                    LlmFinishReason::Stop,
                    None,
                    Some("response-active-cancelled".to_owned()),
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(
                ActiveCancellingCpu {
                    cancellation: cancellation.clone(),
                },
                translation_resources(),
            )
            .with_cancellation(cancellation.clone()),
            cancellation,
        );
        let task = task();
        let profile = profile();

        let failure = RpgMakerTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("已经进入 CPU 的响应处理必须观察共享取消");
        let (source, evidence, diagnostic, cancelled) = split_execution_failure(failure);

        assert!(matches!(
            source,
            RpgMakerTranslationTaskExecutionError::ProcessResponse {
                attempt: 1,
                source: TranslationTaskResponseProcessingError::Cancelled,
            }
        ));
        assert_eq!(evidence.attempt_count(), 1);
        assert_eq!(
            evidence
                .response()
                .map(TranslationTaskResponseRecord::raw_assistant),
            Some(raw_assistant.as_str()),
            "开启记录后必须保留已建立投影或未处理的原始 Assistant"
        );
        assert!(diagnostic.is_none());
        assert!(cancelled);
    }

    #[tokio::test]
    async fn executor_keeps_parsed_thinking_record_when_later_validation_fails() {
        let raw_assistant = r#"{"think":"已建立解析证据。","translations":{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#;
        let service = RpgMakerTranslationTaskExecutionService::<_, _, _, _>::new(
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

        let failure = RpgMakerTranslationTaskExecutor::execute(&service, &profile, &task)
            .await
            .expect_err("解析后的任务语言对不一致必须返回技术失败");
        let (source, evidence, _diagnostic, cancelled) = split_execution_failure(failure);

        assert!(matches!(
            source,
            RpgMakerTranslationTaskExecutionError::ProcessResponse {
                attempt: 1,
                source: TranslationTaskResponseProcessingError::InternalInvariant {
                    invariant: TranslationInternalInvariant::LanguagePairMismatch { .. },
                },
            }
        ));
        assert!(!cancelled);
        let response = evidence
            .response()
            .expect("解析成功后建立的响应投影必须随技术失败进入 Executor evidence");
        assert_eq!(response.raw_assistant(), raw_assistant);
        assert_eq!(response.thinking(), Some("已建立解析证据。"));
        assert!(response.parse_error().is_none());
        let entries = response
            .ordered_entries()
            .expect("合法响应必须保留有序条目");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id(), "0");
        assert_eq!(entries[0].canonical_id(), Some(task_id(0)));
        assert_eq!(entries[0].value_error(), None);
        assert_eq!(
            entries[0].lines(),
            Some(["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧".to_owned()].as_slice()),
            "解析成功后的合法行正文必须保持原样"
        );
        assert!(entries[0].raw_json().is_none());
    }

    #[tokio::test]
    async fn executor_never_retries_a_per_id_shape_rejection() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let service = RpgMakerTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Ok(LlmResponse::new(
                        r#"{"0":123}"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-invalid-shape".to_owned()),
                        None,
                    )),
                    Ok(LlmResponse::new(
                        r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
    async fn executor_never_retries_an_invalid_thinking_response() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from([
            Ok(LlmResponse::new(
                r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
                LlmFinishReason::Stop,
                None,
                Some("response-missing-thinking".to_owned()),
                None,
            )),
            Ok(LlmResponse::new(
                r#"{"think":"该响应不应被消费。","translations":{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}}"#,
                LlmFinishReason::Stop,
                None,
                Some("response-unused".to_owned()),
                None,
            )),
        ])));
        let service = RpgMakerTranslationTaskExecutionService::<
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
            .expect("thinking 响应错误应成为正常不可用结果");
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
            let service = RpgMakerTranslationTaskExecutionService::<
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
            let diagnostic = match &outcome {
                TranslationTaskOutcome::Unavailable {
                    reason:
                        TranslationTaskUnavailableReason::RecoverableRequestExhausted { diagnostic }
                        | TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                            diagnostic,
                            ..
                        },
                    ..
                } => diagnostic,
                _ => unreachable!("上方已经确认这是带请求诊断的 Unavailable 结果"),
            };
            assert_eq!(diagnostic.primary().code(), "http.status");
            assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
            let value = serde_json::to_value(diagnostic).expect("任务诊断应可序列化");
            let details = &value["primary"]["issue"]["details"];
            assert_eq!(details["status"], 503);
            assert_eq!(
                details["retry_after_seconds"],
                (expected_status == "retry-after")
                    .then_some(3)
                    .map_or(serde_json::Value::Null, serde_json::Value::from)
            );
            assert_eq!(details["provider_code"], "temporarily_unavailable");
            assert_eq!(details["provider_type"], "service_error");
            assert!(details["provider_message"].is_null());
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
        let service = RpgMakerTranslationTaskExecutionService::<
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
            Err(RpgMakerTranslationTaskExecutionError::FatalRequest { .. })
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = RpgMakerTranslationTaskExecutionService::<
            _,
            _,
            _,
            RpgMakerTranslationProfile<&'static str>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"{"0":["炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"]}"#,
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
