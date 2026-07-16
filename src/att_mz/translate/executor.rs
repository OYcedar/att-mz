#![allow(dead_code, reason = "翻译执行链尚未接入生产组合根")]

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
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::language::{
    LanguageModule, LanguageModuleCatalog, LanguageModuleCatalogError, LanguageModuleError,
    LanguageRepairApplicationError, LanguageText, LanguageTextSegment,
};
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

use super::language_projection::{
    LanguageTextProjectionError, project_protected_text, restore_protected_text,
};
use super::profile::{MzTranslationExecutionPayload, TranslationExecutionProfile};
use super::standard::{
    AcceptedTranslationDecision, AppliedPlaceholder, ChatMessage, ExpectedTranslationOutput,
    StandardTranslationProfile, StandardTranslationTaskExecutor, StandardTranslationTaskIndex,
    TranslationLanguagePair, TranslationPatch, TranslationProtocolDiagnostic, TranslationTaskBlock,
    TranslationTaskOutcome, TranslationTaskOutcomeInvariantError, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason, UnresolvedTranslationUnit,
};

/// 单次非流式 LLM 请求的结束原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LlmFinishReason {
    Stop,
    Length,
    ContentFilter,
    Other(String),
}

impl fmt::Display for LlmFinishReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stop => formatter.write_str("stop"),
            Self::Length => formatter.write_str("length"),
            Self::ContentFilter => formatter.write_str("content_filter"),
            Self::Other(value) => formatter.write_str(value),
        }
    }
}

/// 根适配器能够提供时返回的统一 token 用量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LlmUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

impl LlmUsage {
    pub(crate) const fn new(prompt_tokens: u64, completion_tokens: u64, total_tokens: u64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }
    }

    pub(crate) const fn prompt_tokens(self) -> u64 {
        self.prompt_tokens
    }

    pub(crate) const fn completion_tokens(self) -> u64 {
        self.completion_tokens
    }

    pub(crate) const fn total_tokens(self) -> u64 {
        self.total_tokens
    }
}

/// 一次模型请求的未清洗统一响应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmResponse {
    content: String,
    finish_reason: LlmFinishReason,
    request_id: Option<String>,
    usage: Option<LlmUsage>,
}

impl LlmResponse {
    pub(crate) fn new(
        content: impl Into<String>,
        finish_reason: LlmFinishReason,
        request_id: Option<String>,
        usage: Option<LlmUsage>,
    ) -> Self {
        Self {
            content: content.into(),
            finish_reason,
            request_id,
            usage,
        }
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    pub(crate) fn finish_reason(&self) -> &LlmFinishReason {
        &self.finish_reason
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub(crate) const fn usage(&self) -> Option<LlmUsage> {
        self.usage
    }
}

/// LLM 根请求在单次尝试中的失败类别。
#[derive(Debug)]
pub(crate) enum LlmRequestError<E> {
    /// 调用方可以按自身策略重试，并可尊重服务端等待时间。
    Retryable {
        source: E,
        retry_after: Option<Duration>,
    },
    /// 认证、无效请求等继续重试无意义的失败。
    Fatal(E),
}

impl<E> fmt::Display for LlmRequestError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retryable {
                source,
                retry_after,
            } => {
                write!(formatter, "LLM 请求暂时失败：{source}")?;
                if let Some(retry_after) = retry_after {
                    write!(formatter, "（建议等待 {retry_after:?}）")?;
                }
                Ok(())
            }
            Self::Fatal(source) => write!(formatter, "LLM 请求不可恢复地失败：{source}"),
        }
    }
}

impl<E> Error for LlmRequestError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Retryable { source, .. } | Self::Fatal(source) => Some(source),
        }
    }
}

/// 执行一次非流式、单 choice LLM 请求的根能力。
///
/// 根适配器不自动重试。全局活动请求数、排队容量、超时、响应字节上限和
/// 速率治理均由外部配置并由适配器执行；排队、网络和响应读取不得阻塞异步线程。
/// `Profile` 必须由外部完整提供 endpoint、凭据、模型、超时、响应上限、速率和
/// 请求选项；根能力不得用 SDK 或库默认值补齐有意义的选择。
pub(crate) trait LlmRequestExecutor: Send + Sync {
    type Profile: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn request<'a>(
        &'a self,
        profile: &'a Self::Profile,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a;
}

/// 可取消异步等待的根能力。
pub(crate) trait AsyncDelay: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn wait(&self, duration: Duration) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Executor 从受信 Profile 消费的最小配置面。
pub(crate) trait TranslationTaskExecutionProfile: StandardTranslationProfile {
    type LlmProfile: Send + Sync + 'static;

    fn llm_profile(&self) -> &Self::LlmProfile;
    fn network_retry_delays(&self) -> &[Duration];
    fn max_network_retry_after(&self) -> Duration;
}

impl<L> TranslationTaskExecutionProfile
    for TranslationExecutionProfile<MzTranslationExecutionPayload<L>>
where
    L: Send + Sync + 'static,
{
    type LlmProfile = L;

    fn llm_profile(&self) -> &Self::LlmProfile {
        self.payload().llm()
    }

    fn network_retry_delays(&self) -> &[Duration] {
        self.payload().execution().network_retry_delays()
    }

    fn max_network_retry_after(&self) -> Duration {
        self.payload().execution().max_network_retry_after()
    }
}

impl<L> TranslationTaskExecutionProfile
    for Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<L>>>
where
    L: Send + Sync + 'static,
{
    type LlmProfile = L;

    fn llm_profile(&self) -> &Self::LlmProfile {
        self.as_ref().payload().llm()
    }

    fn network_retry_delays(&self) -> &[Duration] {
        self.as_ref().payload().execution().network_retry_delays()
    }

    fn max_network_retry_after(&self) -> Duration {
        self.as_ref()
            .payload()
            .execution()
            .max_network_retry_after()
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
}

/// 使用 CPU 根完成有限 JSON 清洗、严格协议校验与译后处理。
pub(crate) struct TranslationTaskResponseProcessingService<C> {
    cpu: C,
    language_modules: LanguageModuleCatalog,
}

impl<C> TranslationTaskResponseProcessingService<C> {
    pub(crate) fn new(cpu: C, language_modules: LanguageModuleCatalog) -> Self {
        Self {
            cpu,
            language_modules,
        }
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
        let input = ResponseProcessingInput {
            task_index: task.index(),
            language_pair: task.language_pair().clone(),
            expected_outputs: task.expected_outputs().to_vec(),
            attempt,
        };
        let language_modules = self.language_modules.clone();
        let outcome = self
            .cpu
            .execute(move || process_response(input, response, &language_modules))
            .await
            .map_err(TranslationTaskResponseProcessingError::ScheduleCompute)?;
        outcome.map_err(|error| match error {
            TranslationResponseTechnicalError::LanguageUnavailable(source) => {
                TranslationTaskResponseProcessingError::LanguageUnavailable(source)
            }
            TranslationResponseTechnicalError::LanguageModule(source) => {
                TranslationTaskResponseProcessingError::LanguageModule(source)
            }
            TranslationResponseTechnicalError::LanguageProjection(source) => {
                TranslationTaskResponseProcessingError::LanguageProjection(source)
            }
            TranslationResponseTechnicalError::LanguageRepair(source) => {
                TranslationTaskResponseProcessingError::LanguageRepair(source)
            }
            TranslationResponseTechnicalError::InternalInvariant { message } => {
                TranslationTaskResponseProcessingError::InternalInvariant { message }
            }
        })
    }
}

/// 一个响应无法继续处理的技术错误。
#[derive(Debug)]
pub(crate) enum TranslationTaskResponseProcessingError<C> {
    ScheduleCompute(CpuTaskExecutionError<C>),
    LanguageUnavailable(LanguageModuleCatalogError),
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant { message: String },
}

impl<C> fmt::Display for TranslationTaskResponseProcessingError<C>
where
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleCompute(source) => write!(formatter, "调度译后 CPU 验收失败：{source}"),
            Self::LanguageUnavailable(source) => write!(formatter, "译后语言模块不可用：{source}"),
            Self::LanguageModule(source) => write!(formatter, "译后语言事实不一致：{source}"),
            Self::LanguageProjection(source) => write!(formatter, "译后语言投影失败：{source}"),
            Self::LanguageRepair(source) => write!(formatter, "译后语言修复无法安全应用：{source}"),
            Self::InternalInvariant { message } => {
                write!(formatter, "翻译任务内部不变量已破坏：{message}")
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
            Self::LanguageUnavailable(source) => Some(source),
            Self::LanguageModule(source) => Some(source),
            Self::LanguageProjection(source) => Some(source),
            Self::LanguageRepair(source) => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

#[derive(Debug)]
enum TranslationResponseTechnicalError {
    LanguageUnavailable(LanguageModuleCatalogError),
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant { message: String },
}

/// 使用根 LLM、根 Delay 和真实 ResponseProcessor 执行一个 TaskBlock。
pub(crate) struct MzStandardTranslationTaskExecutionService<L, D, R, P> {
    llm: L,
    delay: D,
    response_processor: R,
    profile: PhantomData<fn() -> P>,
}

impl<L, D, R, P> MzStandardTranslationTaskExecutionService<L, D, R, P> {
    pub(crate) fn new(llm: L, delay: D, response_processor: R) -> Self {
        Self {
            llm,
            delay,
            response_processor,
            profile: PhantomData,
        }
    }
}

impl<L, D, R, P> StandardTranslationTaskExecutor
    for MzStandardTranslationTaskExecutionService<L, D, R, P>
where
    L: LlmRequestExecutor,
    D: AsyncDelay,
    R: TranslationTaskResponseProcessor,
    P: TranslationTaskExecutionProfile<LlmProfile = L::Profile>,
{
    type Profile = P;
    type Error = MzStandardTranslationTaskExecutionError<L::Error, D::Error, R::Error>;

    async fn execute(
        &self,
        profile: &Self::Profile,
        task: TranslationTaskBlock,
    ) -> Result<TranslationTaskOutcome, Self::Error> {
        if task.expected_outputs().is_empty() {
            return Err(MzStandardTranslationTaskExecutionError::InternalInvariant {
                message: "Planner 生成了没有预期输出的翻译任务".to_owned(),
            });
        }
        let mut attempt = 1_usize;
        let mut retry_delays = profile.network_retry_delays().iter().copied();

        loop {
            let response = match self
                .llm
                .request(profile.llm_profile(), task.messages())
                .await
            {
                Ok(response) => response,
                Err(LlmRequestError::Fatal(source)) => {
                    return Err(MzStandardTranslationTaskExecutionError::FatalRequest {
                        attempt,
                        source,
                    });
                }
                Err(LlmRequestError::Retryable {
                    source,
                    retry_after,
                }) => {
                    if let Some(retry_after) = retry_after
                        && retry_after > profile.max_network_retry_after()
                    {
                        return unavailable_after_request_failure(
                            &task,
                            attempt,
                            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                                attempt,
                                retry_after,
                                maximum: profile.max_network_retry_after(),
                                message: source.to_string(),
                            },
                        )
                        .map_err(outcome_invariant_execution_error);
                    }
                    let Some(configured_delay) = retry_delays.next() else {
                        return unavailable_after_request_failure(
                            &task,
                            attempt,
                            TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                                attempts: attempt,
                                message: source.to_string(),
                            },
                        )
                        .map_err(outcome_invariant_execution_error);
                    };
                    let delay = configured_delay.max(retry_after.unwrap_or_default());
                    self.delay.wait(delay).await.map_err(|delay_source| {
                        MzStandardTranslationTaskExecutionError::WaitBeforeRetry {
                            attempt,
                            delay,
                            source: delay_source,
                        }
                    })?;
                    attempt += 1;
                    continue;
                }
            };

            return self
                .response_processor
                .process(&task, response, attempt)
                .await
                .map_err(
                    |source| MzStandardTranslationTaskExecutionError::ProcessResponse {
                        attempt,
                        source,
                    },
                );
        }
    }
}

fn unavailable_after_request_failure(
    task: &TranslationTaskBlock,
    attempts: usize,
    reason: TranslationTaskUnavailableReason,
) -> Result<TranslationTaskOutcome, TranslationTaskOutcomeInvariantError> {
    TranslationTaskOutcome::unavailable(
        task.index(),
        attempts,
        None,
        None,
        reason,
        unresolved_all(
            task.expected_outputs(),
            TranslationUnitRejectionReason::Missing,
        ),
        Vec::new(),
    )
}

fn outcome_invariant_execution_error<L, D, R>(
    source: TranslationTaskOutcomeInvariantError,
) -> MzStandardTranslationTaskExecutionError<L, D, R> {
    MzStandardTranslationTaskExecutionError::InternalInvariant {
        message: source.to_string(),
    }
}

/// 单任务模型执行失败。
#[derive(Debug)]
pub(crate) enum MzStandardTranslationTaskExecutionError<L, D, R> {
    FatalRequest {
        attempt: usize,
        source: L,
    },
    ProcessResponse {
        attempt: usize,
        source: R,
    },
    WaitBeforeRetry {
        attempt: usize,
        delay: Duration,
        source: D,
    },
    InternalInvariant {
        message: String,
    },
}

impl<L, D, R> fmt::Display for MzStandardTranslationTaskExecutionError<L, D, R>
where
    L: fmt::Display,
    D: fmt::Display,
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
            Self::WaitBeforeRetry {
                attempt,
                delay,
                source,
            } => write!(
                formatter,
                "第 {attempt} 次失败后无法等待 {delay:?} 再重试：{source}"
            ),
            Self::InternalInvariant { message } => {
                write!(formatter, "翻译任务内部不变量已破坏：{message}")
            }
        }
    }
}

impl<L, D, R> Error for MzStandardTranslationTaskExecutionError<L, D, R>
where
    L: Error + 'static,
    D: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FatalRequest { source, .. } => Some(source),
            Self::ProcessResponse { source, .. } => Some(source),
            Self::WaitBeforeRetry { source, .. } => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

struct ResponseProcessingInput {
    task_index: StandardTranslationTaskIndex,
    language_pair: TranslationLanguagePair,
    expected_outputs: Vec<ExpectedTranslationOutput>,
    attempt: usize,
}

fn process_response(
    input: ResponseProcessingInput,
    response: LlmResponse,
    language_modules: &LanguageModuleCatalog,
) -> Result<TranslationTaskOutcome, TranslationResponseTechnicalError> {
    if input.expected_outputs.is_empty() {
        return Err(TranslationResponseTechnicalError::InternalInvariant {
            message: "Planner 生成了没有预期输出的翻译任务".to_owned(),
        });
    }
    let language_module = language_modules
        .resolve(input.language_pair.source_language())
        .map_err(TranslationResponseTechnicalError::LanguageUnavailable)?;

    let request_id = response.request_id.clone();
    let finish_reason = response.finish_reason.to_string();
    let mut diagnostics = Vec::new();
    if response.finish_reason != LlmFinishReason::Stop {
        diagnostics.push(TranslationProtocolDiagnostic::NonStopFinish {
            reason: finish_reason.clone(),
        });
    }

    let values = match clean_model_json(&response.content)
        .and_then(|cleaned| parse_model_output_values(&cleaned))
    {
        Ok(values) => values,
        Err(message) => {
            diagnostics.push(TranslationProtocolDiagnostic::InvalidJson {
                message: message.clone(),
            });
            return TranslationTaskOutcome::unavailable(
                input.task_index,
                input.attempt,
                request_id,
                Some(finish_reason),
                TranslationTaskUnavailableReason::ModelResponseUnusable,
                unresolved_all(
                    &input.expected_outputs,
                    TranslationUnitRejectionReason::InvalidShape { message },
                ),
                diagnostics,
            )
            .map_err(outcome_invariant_response_error);
        }
    };

    let expected_by_id = input
        .expected_outputs
        .iter()
        .map(|output| (output.id(), output))
        .collect::<BTreeMap<_, _>>();
    let actual_by_id = collect_model_outputs(values, &expected_by_id, &mut diagnostics);

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
        let translation = match &candidates[0] {
            ParsedModelOutput::Translation(translation) => translation.clone(),
            ParsedModelOutput::InvalidShape(message) => {
                unresolved.push(unresolved_unit(
                    expected,
                    TranslationUnitRejectionReason::InvalidShape {
                        message: message.clone(),
                    },
                ));
                continue;
            }
        };
        if translation.trim().is_empty() {
            unresolved.push(unresolved_unit(
                expected,
                TranslationUnitRejectionReason::BlankTranslation,
            ));
            continue;
        }
        let translation = match validate_and_restore_translation(
            translation,
            expected.applied_placeholders(),
            expected.language_analysis(),
            language_module.as_ref(),
        ) {
            Ok(translation) => translation,
            Err(TranslationCandidateValidationError::Rejected(reason)) => {
                unresolved.push(unresolved_unit(expected, reason));
                continue;
            }
            Err(TranslationCandidateValidationError::LanguageModule(source)) => {
                return Err(TranslationResponseTechnicalError::LanguageModule(source));
            }
            Err(TranslationCandidateValidationError::LanguageProjection(source)) => {
                return Err(TranslationResponseTechnicalError::LanguageProjection(
                    source,
                ));
            }
            Err(TranslationCandidateValidationError::LanguageRepair(source)) => {
                return Err(TranslationResponseTechnicalError::LanguageRepair(source));
            }
            Err(TranslationCandidateValidationError::InternalInvariant { message }) => {
                return Err(TranslationResponseTechnicalError::InternalInvariant { message });
            }
        };
        accepted.push(AcceptedTranslationDecision::new(
            expected.id(),
            TranslationPatch::new(
                expected.identity().clone(),
                expected.propagation_targets().to_vec(),
                translation,
                expected.terminology_dependencies().to_vec(),
            ),
        ));
    }

    if unresolved.is_empty() {
        TranslationTaskOutcome::complete(
            input.task_index,
            input.attempt,
            request_id,
            Some(finish_reason),
            accepted,
            diagnostics,
        )
        .map_err(outcome_invariant_response_error)
    } else if accepted.is_empty() {
        TranslationTaskOutcome::unavailable(
            input.task_index,
            input.attempt,
            request_id,
            Some(finish_reason),
            TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved,
            diagnostics,
        )
        .map_err(outcome_invariant_response_error)
    } else {
        TranslationTaskOutcome::partial(
            input.task_index,
            input.attempt,
            request_id,
            Some(finish_reason),
            accepted,
            unresolved,
            diagnostics,
        )
        .map_err(outcome_invariant_response_error)
    }
}

fn outcome_invariant_response_error(
    source: TranslationTaskOutcomeInvariantError,
) -> TranslationResponseTechnicalError {
    TranslationResponseTechnicalError::InternalInvariant {
        message: source.to_string(),
    }
}

#[derive(Debug)]
enum ParsedModelOutput {
    Translation(String),
    InvalidShape(String),
}

fn collect_model_outputs(
    values: Vec<Value>,
    expected_by_id: &BTreeMap<usize, &ExpectedTranslationOutput>,
    diagnostics: &mut Vec<TranslationProtocolDiagnostic>,
) -> BTreeMap<usize, Vec<ParsedModelOutput>> {
    let mut outputs = BTreeMap::<usize, Vec<ParsedModelOutput>>::new();
    for (item_index, value) in values.into_iter().enumerate() {
        let Value::Object(object) = value else {
            diagnostics.push(TranslationProtocolDiagnostic::UnattributedItem {
                item_index,
                message: "响应元素不是对象".to_owned(),
            });
            continue;
        };
        let Some(raw_id) = object.get("id") else {
            diagnostics.push(TranslationProtocolDiagnostic::MissingId { item_index });
            continue;
        };
        let id = match parse_translation_id(raw_id) {
            Ok(id) => id,
            Err(value) => {
                diagnostics.push(TranslationProtocolDiagnostic::InvalidId { item_index, value });
                continue;
            }
        };
        if !expected_by_id.contains_key(&id) {
            diagnostics.push(TranslationProtocolDiagnostic::UnknownId { item_index, id });
            continue;
        }
        outputs
            .entry(id)
            .or_default()
            .push(parse_known_output(object));
    }
    outputs
}

fn parse_translation_id(value: &Value) -> Result<usize, String> {
    match value {
        Value::Number(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| value.to_string()),
        Value::String(value)
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            value.parse::<usize>().map_err(|_| value.clone())
        }
        _ => Err(value.to_string()),
    }
}

fn parse_known_output(mut object: Map<String, Value>) -> ParsedModelOutput {
    if object.len() != 2 || !object.contains_key("translation") {
        let mut unexpected = object
            .keys()
            .filter(|key| key.as_str() != "id" && key.as_str() != "translation")
            .cloned()
            .collect::<Vec<_>>();
        unexpected.sort();
        let message = if !object.contains_key("translation") {
            "缺少 translation 字段".to_owned()
        } else {
            format!("包含未知字段：{}", unexpected.join(", "))
        };
        return ParsedModelOutput::InvalidShape(message);
    }
    match object.remove("translation") {
        Some(Value::String(translation)) => ParsedModelOutput::Translation(translation),
        Some(_) => ParsedModelOutput::InvalidShape("translation 必须是字符串".to_owned()),
        None => ParsedModelOutput::InvalidShape("缺少 translation 字段".to_owned()),
    }
}

fn parse_model_output_values(value: &str) -> Result<Vec<Value>, String> {
    serde_json::from_str(value).map_err(|source| source.to_string())
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

/// 只执行已确认安全的修复：BOM、单层 Markdown 围栏、唯一完整顶层数组和尾逗号。
fn clean_model_json(value: &str) -> Result<String, String> {
    let value = value.trim();
    let value = value.strip_prefix('\u{feff}').unwrap_or(value).trim();
    let value = strip_single_markdown_fence(value)?;
    let array = extract_unique_top_level_array(value)?;
    Ok(remove_trailing_commas(array))
}

fn strip_single_markdown_fence(value: &str) -> Result<&str, String> {
    if !value.starts_with("```") {
        return Ok(value);
    }
    let Some(first_line_end) = value.find('\n') else {
        return Err("Markdown 围栏没有正文".to_owned());
    };
    let opening = value[..first_line_end].trim_end_matches('\r');
    if opening != "```" && opening != "```json" && opening != "```JSON" {
        return Err("只接受无语言标记或 json 标记的单层 Markdown 围栏".to_owned());
    }
    let body_and_closing = &value[first_line_end + 1..];
    let Some(closing_start) = body_and_closing.rfind("```") else {
        return Err("Markdown 围栏没有闭合".to_owned());
    };
    if !body_and_closing[closing_start + 3..].trim().is_empty() {
        return Err("Markdown 围栏闭合后仍有额外内容".to_owned());
    }
    let body = body_and_closing[..closing_start].trim();
    if body.contains("```") {
        return Err("不接受嵌套或多层 Markdown 围栏".to_owned());
    }
    Ok(body)
}

fn extract_unique_top_level_array(value: &str) -> Result<&str, String> {
    let bytes = value.as_bytes();
    let mut arrays = Vec::new();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'[' {
            let end = find_balanced_array_end(bytes, index)?;
            arrays.push((index, end));
            index = end + 1;
            continue;
        }
        index += 1;
    }
    match arrays.as_slice() {
        [(start, end)] => Ok(&value[*start..=*end]),
        [] => Err("没有找到完整顶层数组".to_owned()),
        _ => Err("响应中存在多个完整顶层数组".to_owned()),
    }
}

fn find_balanced_array_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut square_depth = 0_usize;
    let mut curly_depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => square_depth += 1,
            b']' => {
                square_depth = square_depth
                    .checked_sub(1)
                    .ok_or_else(|| "数组括号不平衡".to_owned())?;
                if square_depth == 0 && curly_depth == 0 {
                    return Ok(start + offset);
                }
            }
            b'{' => curly_depth += 1,
            b'}' => {
                curly_depth = curly_depth
                    .checked_sub(1)
                    .ok_or_else(|| "对象括号不平衡".to_owned())?;
            }
            _ => {}
        }
    }
    Err("顶层数组没有闭合".to_owned())
}

fn remove_trailing_commas(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && matches!(bytes[lookahead], b']' | b'}') {
                index += 1;
                continue;
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).expect("删除 ASCII 逗号不会破坏原 UTF-8")
}

fn validate_and_restore_translation(
    mut translation: String,
    placeholders: &[AppliedPlaceholder],
    language_analysis: &crate::language::LanguageAnalysis,
    language_module: &dyn LanguageModule,
) -> Result<String, TranslationCandidateValidationError> {
    normalize_original_controls(&mut translation, placeholders)
        .map_err(TranslationCandidateValidationError::Rejected)?;
    validate_token_multiset(&translation, placeholders)
        .map_err(TranslationCandidateValidationError::Rejected)?;

    let projected = project_protected_text(&translation, placeholders)
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
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
    let restored = restore_protected_text(&translation, placeholders, &repaired)
        .map_err(TranslationCandidateValidationError::LanguageProjection)?;
    Ok(restored)
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
    InternalInvariant { message: String },
}

fn normalize_original_controls(
    translation: &mut String,
    placeholders: &[AppliedPlaceholder],
) -> Result<(), TranslationUnitRejectionReason> {
    let mut originals = BTreeMap::<&str, Vec<&AppliedPlaceholder>>::new();
    for placeholder in placeholders {
        originals
            .entry(placeholder.original())
            .or_default()
            .push(placeholder);
    }
    for (original, bindings) in originals {
        if bindings.len() != 1 {
            if translation.contains(original)
                && bindings
                    .iter()
                    .any(|binding| !translation.contains(binding.token()))
            {
                return Err(
                    TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                        original: original.to_owned(),
                    },
                );
            }
            continue;
        }
        let binding = bindings[0];
        let token_count = translation.matches(binding.token()).count();
        let original_count = translation.matches(original).count();
        if token_count == 0 && original_count == 1 {
            *translation = translation.replacen(original, binding.token(), 1);
        } else if original_count > 0 && token_count != 0 {
            return Err(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: original.to_owned(),
                },
            );
        }
    }
    Ok(())
}

fn validate_token_multiset(
    translation: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<(), TranslationUnitRejectionReason> {
    let mut expected = BTreeMap::<&str, usize>::new();
    for placeholder in placeholders {
        *expected.entry(placeholder.token()).or_default() += 1;
    }
    for (token, count) in expected {
        if translation.matches(token).count() != count {
            return Err(TranslationUnitRejectionReason::PlaceholderMismatch {
                token: token.to_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };
    use crate::att_mz::translate::profile::{
        MzTranslationExecutionConfiguration, MzTranslationPlanningConfiguration,
        TranslationProfileLanguagePair,
    };
    use crate::att_mz::translate::standard::{
        AppliedPlaceholder, ChatMessageRole, ExpectedTranslationOutput, PlaceholderRuleOrigin,
        PlaceholderSegment, StandardTranslationTaskIndex, TerminologyDependency,
        TranslationLanguagePair, TranslationLeafIdentity, TranslationTaskStatus,
    };
    use crate::language::{
        EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
        JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageModule,
        LanguageModuleCatalog, LanguageText, QuotePair,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

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

    fn language_catalog() -> LanguageModuleCatalog {
        LanguageModuleCatalog::new([("ja".to_owned(), japanese_module())])
            .expect("测试语言目录有效")
    }

    fn japanese_analysis() -> crate::language::LanguageAnalysis {
        japanese_module().analyze_source(&LanguageText::natural("炎の剣"))
    }

    fn identity() -> TranslationLeafIdentity {
        let group = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(1)],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group.clone(),
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(1), MzLocationStep::key("description")],
            ),
            "炎の剣",
        )
    }

    fn placeholder() -> AppliedPlaceholder {
        AppliedPlaceholder::new(
            "⟦ATT:ACTOR_NAME:0⟧",
            "\\N[1]",
            PlaceholderRuleOrigin::BuiltIn,
            "ACTOR_NAME",
            "event_dialogue",
            PlaceholderSegment::Whole,
        )
    }

    fn propagation_target() -> TranslationLeafIdentity {
        let group = MzLocation::value(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(2)],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group.clone(),
            MzLocation::value(
                MzSource::data(StandardDataFile::Items),
                vec![MzLocationStep::index(2), MzLocationStep::key("description")],
            ),
            "炎の剣",
        )
    }

    fn task() -> TranslationTaskBlock {
        task_with_output_count(1)
    }

    fn task_with_output_count(output_count: usize) -> TranslationTaskBlock {
        task_with_language_pair("ja", "zh-Hans", output_count)
    }

    fn task_with_language_pair(
        source_language: &str,
        target_language: &str,
        output_count: usize,
    ) -> TranslationTaskBlock {
        TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(2),
            TranslationLanguagePair::new(source_language, target_language),
            Vec::new(),
            vec![TerminologyDependency::new("炎の剣", "炎之剑")],
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            (0..output_count)
                .map(|id| {
                    ExpectedTranslationOutput::new(
                        id,
                        identity(),
                        vec![propagation_target()],
                        vec![placeholder()],
                        japanese_analysis(),
                        vec![TerminologyDependency::new("炎の剣", "炎之剑")],
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn cleaner_only_applies_the_confirmed_safe_repairs() {
        let cleaned = clean_model_json(
            "\u{feff}```json\n说明：[{\"id\":\"0\",\"translation\":\"炎之剑\",},]\n```",
        )
        .expect("有限清洗应成功");
        assert_eq!(cleaned, "[{\"id\":\"0\",\"translation\":\"炎之剑\"}]");
        assert!(clean_model_json("[{}] and [{}]").is_err());
        assert!(clean_model_json("```yaml\n[]\n```").is_err());
    }

    #[tokio::test]
    async fn response_processor_accepts_string_id_and_restores_original_control() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let result = processor
            .process(
                &task_with_language_pair("ja", "unconfigured-target", 1),
                LlmResponse::new(
                    r#"[{"id":"0","translation":"炎之剑\\N[1]！"}]"#,
                    LlmFinishReason::Stop,
                    Some("request-1".to_owned()),
                    Some(LlmUsage::new(10, 5, 15)),
                ),
                1,
            )
            .await
            .expect("原控制符能够唯一对应时应规范化并恢复");

        assert_eq!(result.task_index(), StandardTranslationTaskIndex::new(2));
        assert!(matches!(result.status(), TranslationTaskStatus::Complete));
        assert_eq!(result.attempts(), 1);
        assert_eq!(result.request_id(), Some("request-1"));
        assert_eq!(result.accepted()[0].translation(), "炎之剑\\N[1]！");
        assert_eq!(
            result.accepted()[0].propagation_targets(),
            &[propagation_target()]
        );
        assert_eq!(result.accepted()[0].terminology_dependencies().len(), 1);
    }

    #[tokio::test]
    async fn response_processor_keeps_generic_text_checks_as_per_id_normal_results() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());

        let bom = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"炎\uFEFF之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
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
                    r#"[{"id":0,"translation":" \r\n⟦ATT:ACTOR_NAME:0⟧\r"}]"#,
                    LlmFinishReason::Stop,
                    None,
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

        let normalized = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"第一行\r\n第二行\r⟦ATT:ACTOR_NAME:0⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("通用换行正规化应成功");
        assert_eq!(
            normalized.accepted()[0].translation(),
            "第一行\n第二行\n\\N[1]"
        );
    }

    #[test]
    fn language_repair_rebuilds_tokens_in_their_translated_order() {
        let first = AppliedPlaceholder::new(
            "<first>",
            "<FIRST_ORIGINAL>",
            PlaceholderRuleOrigin::Custom,
            "FIRST",
            "all",
            PlaceholderSegment::Whole,
        );
        let second = AppliedPlaceholder::new(
            "<second>",
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
            "他说：“甲<second>乙‘<first>’丙。”".to_owned(),
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let result = processor
            .process(
                &task_with_output_count(6),
                LlmResponse::new(
                    r#"[
                        {"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"},
                        {"id":1,"translation":"甲⟦ATT:ACTOR_NAME:0⟧"},
                        {"id":"1","translation":"乙⟦ATT:ACTOR_NAME:0⟧"},
                        {"id":3,"translation":"丙⟦ATT:ACTOR_NAME:0⟧","extra":true},
                        {"id":4,"translation":"缺少控制符"},
                        {"id":5,"translation":"译文です⟦ATT:ACTOR_NAME:0⟧"},
                        {"id":99,"translation":"未知"},
                        {"id":"bad","translation":"非法"},
                        {"translation":"无 ID"},
                        "不是对象"
                    ]"#,
                    LlmFinishReason::Length,
                    Some("request-partial".to_owned()),
                    None,
                ),
                2,
            )
            .await
            .expect("模型内容的部分不可用必须是正常结果");

        assert!(matches!(result.status(), TranslationTaskStatus::Partial));
        assert_eq!(result.attempts(), 2);
        assert_eq!(result.accepted().len(), 1);
        assert_eq!(result.unresolved().len(), 5);
        assert_eq!(result.unresolved()[0].id(), 1);
        assert!(matches!(
            result.unresolved()[0].reason(),
            TranslationUnitRejectionReason::Duplicate
        ));
        assert_eq!(result.unresolved()[1].id(), 2);
        assert!(matches!(
            result.unresolved()[1].reason(),
            TranslationUnitRejectionReason::Missing
        ));
        assert_eq!(result.unresolved()[2].id(), 3);
        assert!(matches!(
            result.unresolved()[2].reason(),
            TranslationUnitRejectionReason::InvalidShape { .. }
        ));
        assert_eq!(result.unresolved()[3].id(), 4);
        assert!(matches!(
            result.unresolved()[3].reason(),
            TranslationUnitRejectionReason::PlaceholderMismatch { .. }
        ));
        assert_eq!(result.unresolved()[4].id(), 5);
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
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::InvalidId { .. }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::MissingId { .. }
        )));
        assert!(result.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            TranslationProtocolDiagnostic::UnattributedItem { .. }
        )));
    }

    #[tokio::test]
    async fn response_processor_returns_persistable_unavailable_outcomes() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let invalid_json = processor
            .process(
                &task(),
                LlmResponse::new("not-json", LlmFinishReason::Stop, None, None),
                1,
            )
            .await
            .expect("JSON 无法解析属于正常不可用结果");
        assert!(matches!(
            invalid_json.status(),
            TranslationTaskStatus::Unavailable(
                TranslationTaskUnavailableReason::ModelResponseUnusable
            )
        ));
        assert!(matches!(
            invalid_json.unresolved()[0].reason(),
            TranslationUnitRejectionReason::InvalidShape { .. }
        ));
        assert!(matches!(
            invalid_json.diagnostics(),
            [TranslationProtocolDiagnostic::InvalidJson { .. }]
        ));

        let all_rejected = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":0,"translation":""}]"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect("所有 ID 不合格也属于正常不可用结果");
        assert!(matches!(
            all_rejected.status(),
            TranslationTaskStatus::Unavailable(
                TranslationTaskUnavailableReason::AllOutputsRejected
            )
        ));
        assert!(matches!(
            all_rejected.unresolved()[0].reason(),
            TranslationUnitRejectionReason::BlankTranslation
        ));
    }

    #[tokio::test]
    async fn response_cpu_unavailable_is_fatal_instead_of_model_retryable() {
        let processor =
            TranslationTaskResponseProcessingService::new(UnavailableCpu, language_catalog());
        let error = processor
            .process(
                &task(),
                LlmResponse::new("[]", LlmFinishReason::Stop, None, None),
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
    async fn response_language_unavailable_and_internal_invariant_are_technical_errors() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let language_error = processor
            .process(
                &task_with_language_pair("unknown", "any-target", 1),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                ),
                1,
            )
            .await
            .expect_err("语言模块不可用必须是技术错误");
        assert!(matches!(
            language_error,
            TranslationTaskResponseProcessingError::LanguageUnavailable(
                LanguageModuleCatalogError::UnknownLanguageId { .. }
            )
        ));

        let mismatched_catalog = LanguageModuleCatalog::new([("ja".to_owned(), english_module())])
            .expect("测试语言目录有效");
        let mismatch_error =
            TranslationTaskResponseProcessingService::new(InlineCpu, mismatched_catalog)
                .process(
                    &task_with_language_pair("ja", "arbitrary-target", 1),
                    LlmResponse::new(
                        r#"[{"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                        LlmFinishReason::Stop,
                        None,
                        None,
                    ),
                    1,
                )
                .await
                .expect_err("译前分析与当前源语言模块不匹配必须是技术错误");
        assert!(matches!(
            mismatch_error,
            TranslationTaskResponseProcessingError::LanguageModule(_)
        ));

        let invariant_error = processor
            .process(
                &task_with_output_count(0),
                LlmResponse::new("[]", LlmFinishReason::Stop, None, None),
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
        type Profile = &'static str;
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _profile: &'a Self::Profile,
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
    struct FakeDelay {
        waits: Arc<Mutex<Vec<Duration>>>,
    }

    impl AsyncDelay for FakeDelay {
        type Error = FakeError;

        async fn wait(&self, duration: Duration) -> Result<(), Self::Error> {
            self.waits.lock().expect("等待锁不应中毒").push(duration);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct FailingDelay;

    impl AsyncDelay for FailingDelay {
        type Error = FakeError;

        async fn wait(&self, _duration: Duration) -> Result<(), Self::Error> {
            Err(FakeError("delay"))
        }
    }

    fn profile() -> TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>> {
        let language_pair =
            TranslationProfileLanguagePair::new("ja", "zh-Hans").expect("测试语言对应合法");
        let planning = MzTranslationPlanningConfiguration::new(
            NonZeroUsize::new(2).expect("非零"),
            NonZeroUsize::new(4096).expect("非零"),
            [(language_pair, "# Contract".to_owned())],
        )
        .expect("规划配置应合法");
        TranslationExecutionProfile::new(
            "quality",
            NonZeroUsize::new(3).expect("非零"),
            MzTranslationExecutionPayload::new(
                planning,
                MzTranslationExecutionConfiguration::new(
                    vec![Duration::from_millis(10), Duration::from_millis(20)],
                    Duration::from_secs(2),
                ),
                "llm-profile",
            ),
        )
    }

    #[tokio::test]
    async fn executor_retries_identical_messages_and_uses_larger_retry_after() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let service = MzStandardTranslationTaskExecutionService::<_, _, _, _>::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Err(LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: Some(Duration::from_millis(50)),
                    }),
                    Ok(LlmResponse::new(
                        r#"[{"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                        LlmFinishReason::Stop,
                        None,
                        None,
                    )),
                ]))),
                messages: Arc::clone(&messages),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
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
    async fn executor_never_retries_invalid_model_content() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let service = MzStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    Ok(LlmResponse::new("[]", LlmFinishReason::Stop, None, None)),
                    Ok(LlmResponse::new(
                        r#"[{"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                        LlmFinishReason::Stop,
                        None,
                        None,
                    )),
                ]))),
                messages: Arc::clone(&messages),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );
        let outcome = service
            .execute(&profile(), task())
            .await
            .expect("模型内容不可用是正常结果");
        assert!(matches!(
            outcome.status(),
            TranslationTaskStatus::Unavailable(
                TranslationTaskUnavailableReason::AllOutputsRejected
            )
        ));
        assert_eq!(messages.lock().expect("消息锁不应中毒").len(), 1);
        assert!(waits.lock().expect("等待锁不应中毒").is_empty());
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
            let service = MzStandardTranslationTaskExecutionService::<
                _,
                _,
                _,
                TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
            >::new(
                FakeLlm {
                    responses: Arc::new(Mutex::new(responses)),
                    messages: Arc::new(Mutex::new(Vec::new())),
                },
                FakeDelay {
                    waits: Arc::new(Mutex::new(Vec::new())),
                },
                TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
            );

            let outcome = service
                .execute(&profile(), task())
                .await
                .expect("可恢复网络预算不足属于正常不可用结果");
            assert_eq!(outcome.attempts(), expected_attempts);
            match (expected_status, outcome.status()) {
                (
                    "exhausted",
                    TranslationTaskStatus::Unavailable(
                        TranslationTaskUnavailableReason::RecoverableRequestExhausted { .. },
                    ),
                )
                | (
                    "retry-after",
                    TranslationTaskStatus::Unavailable(
                        TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                            ..
                        },
                    ),
                ) => {}
                (_, status) => panic!("意外任务状态：{status:?}"),
            }
            assert_eq!(outcome.unresolved().len(), 1);
            assert!(matches!(
                outcome.unresolved()[0].reason(),
                TranslationUnitRejectionReason::Missing
            ));
        }
    }

    #[tokio::test]
    async fn executor_stops_on_fatal_request() {
        let service = MzStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );

        assert!(matches!(
            service.execute(&profile(), task()).await,
            Err(MzStandardTranslationTaskExecutionError::FatalRequest { .. })
        ));
    }

    #[tokio::test]
    async fn executor_stops_when_network_backoff_cannot_wait() {
        let service = MzStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Err(
                    LlmRequestError::Retryable {
                        source: FakeError("busy"),
                        retry_after: None,
                    },
                )]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FailingDelay,
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );

        assert!(matches!(
            service.execute(&profile(), task()).await,
            Err(MzStandardTranslationTaskExecutionError::WaitBeforeRetry {
                source: FakeError("delay"),
                ..
            })
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = MzStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
        >::new(
            FakeLlm {
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(LlmResponse::new(
                    r#"[{"id":0,"translation":"炎之剑⟦ATT:ACTOR_NAME:0⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
                    None,
                ))]))),
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::new(Mutex::new(Vec::new())),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );
        let profile = profile();
        assert_send(service.execute(&profile, task()));
    }
}
