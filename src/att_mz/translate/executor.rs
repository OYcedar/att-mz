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
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

use crate::att_mz::placeholder_token;
use crate::language::{
    LanguageModule, LanguageModuleCatalog, LanguageModuleCatalogError, LanguageModuleError,
    LanguageRepairApplicationError, LanguageText, LanguageTextSegment,
};
use crate::llm::{LlmFinishReason, LlmRequestError, LlmRequestExecutor, LlmResponse, LlmUsage};
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

use super::language_projection::{
    LanguageTextProjectionError, project_protected_text, restore_protected_text,
};
use super::profile::{MzTranslationExecutionPayload, TranslationExecutionProfile};
use super::standard::{
    AcceptedTranslationDecision, AppliedPlaceholder, ExpectedTranslationOutput, NonEmptyTaskItems,
    StandardTranslationProfile, StandardTranslationTaskExecutor, StandardTranslationTaskIndex,
    TranslationLanguagePair, TranslationPatch, TranslationProtocolDiagnostic, TranslationTaskBlock,
    TranslationTaskOutcome, TranslationTaskOutcomeContext, TranslationTaskUnavailableReason,
    TranslationUnitRejectionReason, UnresolvedTranslationUnit,
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

    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }

    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.provider_response_id.as_deref()
    }

    pub(crate) fn finish_reason(&self) -> &str {
        &self.finish_reason
    }

    pub(crate) const fn usage(&self) -> Option<LlmUsage> {
        self.usage
    }
}

/// 可取消异步等待的根能力。
pub(crate) trait AsyncDelay: Send + Sync {
    fn wait(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Executor 从受信 MZ Profile 消费的最小配置面。
pub(crate) trait TranslationTaskExecutionProfile: StandardTranslationProfile {
    type LlmClient: Send + Sync + 'static;

    fn llm_client(&self) -> &Self::LlmClient;
    fn network_retry_delays(&self) -> &[Duration];
    fn max_network_retry_after(&self) -> Duration;
}

impl<L> TranslationTaskExecutionProfile
    for TranslationExecutionProfile<MzTranslationExecutionPayload<L>>
where
    L: Send + Sync + 'static,
{
    type LlmClient = L;

    fn llm_client(&self) -> &Self::LlmClient {
        self.payload().llm_client()
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
    type LlmClient = L;

    fn llm_client(&self) -> &Self::LlmClient {
        self.as_ref().payload().llm_client()
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
        let Some(attempt) = NonZeroUsize::new(attempt) else {
            return Err(TranslationTaskResponseProcessingError::InternalInvariant {
                message: "翻译任务尝试次数必须大于零".to_owned(),
            });
        };
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
    P: TranslationTaskExecutionProfile<LlmClient = L::Client>,
{
    type Profile = P;
    type Error = MzStandardTranslationTaskExecutionError<L::Error, R::Error>;

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
        let mut attempt = NonZeroUsize::MIN;
        let mut retry_delays = profile.network_retry_delays().iter().copied();

        loop {
            let response = match self
                .llm
                .request(profile.llm_client(), task.messages())
                .await
            {
                Ok(response) => response,
                Err(LlmRequestError::Fatal(source)) => {
                    return Err(MzStandardTranslationTaskExecutionError::FatalRequest {
                        attempt: attempt.get(),
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
                        return Ok(unavailable_after_request_failure(
                            &task,
                            attempt,
                            TranslationTaskUnavailableReason::RetryAfterExceedsConfiguredMaximum {
                                retry_after,
                                maximum: profile.max_network_retry_after(),
                                message: source.to_string(),
                            },
                        ));
                    }
                    let Some(configured_delay) = retry_delays.next() else {
                        return Ok(unavailable_after_request_failure(
                            &task,
                            attempt,
                            TranslationTaskUnavailableReason::RecoverableRequestExhausted {
                                message: source.to_string(),
                            },
                        ));
                    };
                    let delay = configured_delay.max(retry_after.unwrap_or_default());
                    self.delay.wait(delay).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };

            return self
                .response_processor
                .process(&task, response, attempt.get())
                .await
                .map_err(
                    |source| MzStandardTranslationTaskExecutionError::ProcessResponse {
                        attempt: attempt.get(),
                        source,
                    },
                );
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
pub(crate) enum MzStandardTranslationTaskExecutionError<L, R> {
    FatalRequest { attempt: usize, source: L },
    ProcessResponse { attempt: usize, source: R },
    InternalInvariant { message: String },
}

impl<L, R> fmt::Display for MzStandardTranslationTaskExecutionError<L, R>
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
            Self::InternalInvariant { message } => {
                write!(formatter, "翻译任务内部不变量已破坏：{message}")
            }
        }
    }
}

impl<L, R> Error for MzStandardTranslationTaskExecutionError<L, R>
where
    L: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FatalRequest { source, .. } => Some(source),
            Self::ProcessResponse { source, .. } => Some(source),
            Self::InternalInvariant { .. } => None,
        }
    }
}

struct ResponseProcessingInput {
    task_index: StandardTranslationTaskIndex,
    language_pair: TranslationLanguagePair,
    expected_outputs: Vec<ExpectedTranslationOutput>,
    attempt: NonZeroUsize,
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

    let final_response = FinalLlmResponseMetadata::from_response(&response);
    let finish_reason = final_response.finish_reason().to_owned();
    let mut diagnostics = Vec::new();
    if response.finish_reason() != &LlmFinishReason::Stop {
        diagnostics.push(TranslationProtocolDiagnostic::NonStopFinish {
            reason: finish_reason.clone(),
        });
    }

    let outputs = match parse_model_output_batch(response.content()) {
        Ok(outputs) => outputs,
        Err(message) => {
            diagnostics.push(TranslationProtocolDiagnostic::InvalidResponse {
                message: message.clone(),
            });
            return Ok(TranslationTaskOutcome::Unavailable {
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
                        TranslationUnitRejectionReason::InvalidShape { message },
                    ),
                    "Executor 已确认任务含有预期输出",
                ),
            });
        }
    };

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
        let translation = candidates[0].clone();
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

    if unresolved.is_empty() {
        Ok(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response,
            accepted: non_empty_known(accepted, "所有预期输出已验收"),
        })
    } else if accepted.is_empty() {
        Ok(TranslationTaskOutcome::Unavailable {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response: Some(final_response),
            reason: TranslationTaskUnavailableReason::AllOutputsRejected,
            unresolved: non_empty_known(unresolved, "没有输出通过验收"),
        })
    } else {
        Ok(TranslationTaskOutcome::Partial {
            context: TranslationTaskOutcomeContext::new(
                input.task_index,
                input.attempt,
                diagnostics,
            ),
            final_response,
            accepted: non_empty_known(accepted, "部分输出已验收"),
            unresolved: non_empty_known(unresolved, "部分输出未完成"),
        })
    }
}

fn non_empty_known<T>(items: Vec<T>, established_by: &'static str) -> NonEmptyTaskItems<T> {
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        unreachable!("{established_by}");
    };
    NonEmptyTaskItems::new(first, items.collect())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelOutputWire {
    id: ModelOutputId,
    translation: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelOutputId(usize);

impl<'de> Deserialize<'de> for ModelOutputId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ModelOutputIdVisitor)
    }
}

struct ModelOutputIdVisitor;

impl Visitor<'_> for ModelOutputIdVisitor {
    type Value = ModelOutputId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("非负整数或非空 ASCII 十进制字符串")
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        usize::try_from(value)
            .map(ModelOutputId)
            .map_err(|_| E::custom("id 整数超出 usize 范围"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(E::custom("id 字符串必须是非空 ASCII 十进制整数"));
        }
        value
            .parse::<usize>()
            .map(ModelOutputId)
            .map_err(|_| E::custom("id 字符串超出 usize 范围"))
    }
}

fn collect_model_outputs(
    outputs: Vec<ModelOutputWire>,
    expected_by_id: &BTreeMap<usize, &ExpectedTranslationOutput>,
    diagnostics: &mut Vec<TranslationProtocolDiagnostic>,
) -> BTreeMap<usize, Vec<String>> {
    let mut by_id = BTreeMap::<usize, Vec<String>>::new();
    for (item_index, output) in outputs.into_iter().enumerate() {
        let id = output.id.0;
        if !expected_by_id.contains_key(&id) {
            diagnostics.push(TranslationProtocolDiagnostic::UnknownId { item_index, id });
            continue;
        }
        by_id.entry(id).or_default().push(output.translation);
    }
    by_id
}

fn parse_model_output_batch(value: &str) -> Result<Vec<ModelOutputWire>, String> {
    let value = strip_model_response_envelope(value)?;
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

/// 只剥离协议明确允许的响应信封，不修复或提取 JSON 内容。
fn strip_model_response_envelope(value: &str) -> Result<&str, String> {
    let value = value.trim();
    let value = value.strip_prefix('\u{feff}').unwrap_or(value).trim();
    strip_single_markdown_fence(value)
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
    let Some(closing_line_start) = body_and_closing.rfind('\n') else {
        if body_and_closing.trim_end_matches('\r') == "```" {
            return Err("Markdown 围栏没有正文".to_owned());
        }
        return Err("Markdown 围栏没有闭合".to_owned());
    };
    let closing = body_and_closing[closing_line_start + 1..].trim_end_matches('\r');
    if closing != "```" {
        return Err("Markdown 围栏必须以最终独立行闭合".to_owned());
    }
    let body = body_and_closing[..closing_line_start].trim();
    if body.is_empty() {
        return Err("Markdown 围栏没有正文".to_owned());
    }
    Ok(body)
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
    if placeholder_token::contains_reserved_prefix(&restored) {
        return Err(TranslationCandidateValidationError::InternalInvariant {
            message: "恢复占位符原片段后仍残留 ATT token 保留前缀".to_owned(),
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
        Err(TranslationCandidateValidationError::InternalInvariant { message }) => {
            Err(TranslationCandidateTechnicalError::InternalInvariant { message })
        }
    }
}

#[derive(Debug)]
pub(crate) enum TranslationCandidateTechnicalError {
    LanguageModule(LanguageModuleError),
    LanguageProjection(LanguageTextProjectionError),
    LanguageRepair(LanguageRepairApplicationError),
    InternalInvariant { message: String },
}

impl fmt::Display for TranslationCandidateTechnicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LanguageModule(source) => write!(formatter, "语言模块失败：{source}"),
            Self::LanguageProjection(source) => write!(formatter, "语言投影失败：{source}"),
            Self::LanguageRepair(source) => write!(formatter, "语言修复失败：{source}"),
            Self::InternalInvariant { message } => formatter.write_str(message),
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

    for (&token, &count) in &expected {
        if translation.matches(token).count() != count {
            return Err(TranslationUnitRejectionReason::PlaceholderMismatch {
                token: token.to_owned(),
            });
        }
    }

    let scanned = placeholder_token::scan_envelopes(translation).map_err(|error| {
        TranslationUnitRejectionReason::UnexpectedPlaceholderToken {
            token: error.into_fragment(),
        }
    })?;
    if let Some(token) = scanned
        .into_iter()
        .find(|token| !expected.contains_key(*token))
    {
        return Err(TranslationUnitRejectionReason::UnexpectedPlaceholderToken {
            token: token.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::standard_asset::MzStandardAssetOwner;
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };
    use crate::att_mz::translate::profile::{
        MzTranslationExecutionConfiguration, MzTranslationPlanningConfiguration,
        TranslationProfileLanguagePair,
    };
    use crate::att_mz::translate::standard::{
        AppliedPlaceholder, ExpectedTranslationOutput, PlaceholderRuleOrigin, PlaceholderSegment,
        StandardTranslationTaskIndex, TerminologyDependency, TranslationLanguagePair,
        TranslationLeafIdentity, TranslationStateContext,
    };
    use crate::fingerprint::Sha256Fingerprint;
    use crate::language::{
        EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
        JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageModule,
        LanguageModuleCatalog, LanguageText, QuotePair,
    };
    use crate::llm::{ChatMessage, ChatMessageRole};

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
            MzStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            "description",
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
            "⟦ATT_ACTOR_NAME_WHOLE_0000⟧",
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
            MzStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            "description",
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
                        state_context(id as u8 + 1),
                        vec![state_context(id as u8 + 101)],
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn response_envelope_accepts_only_the_explicit_contract() {
        for value in [
            "[]",
            " \r\n [] \n ",
            "\u{feff}[]",
            "```\n[]\n```",
            "```json\r\n[]\r\n```",
            "```JSON\n[{\"id\":0,\"translation\":\"括号 [ ]、逗号 ,} 与反引号 ```\"}]\n```",
        ] {
            assert!(
                parse_model_output_batch(value).is_ok(),
                "合法响应信封应通过：{value:?}"
            );
        }

        for value in [
            "说明：[]",
            "[] 后记",
            "{\"result\":[]}",
            "[]\n[]",
            "[{\"id\":0,\"translation\":\"译文\",}]",
            "[{\"id\":0,\"translation\":\"译文\"},]",
            "[// comment\n]",
            "```yaml\n[]\n```",
            "```json\n[]",
            "```json\n[]```",
            "```json\n\n```",
            "```json\n[]\n```\n后记",
            "[{\"id\":0,\"translation\":\"截断",
            "\u{feff}\u{feff}[]",
        ] {
            assert!(
                parse_model_output_batch(value).is_err(),
                "协议外响应必须拒绝：{value:?}"
            );
        }
    }

    #[test]
    fn model_output_id_accepts_only_explicit_numeric_forms() {
        let outputs = parse_model_output_batch(
            r#"[{"id":0,"translation":"甲"},{"id":"001","translation":"乙"}]"#,
        )
        .expect("数字和 ASCII 十进制字符串 ID 都应合法");

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].id, ModelOutputId(0));
        assert_eq!(outputs[1].id, ModelOutputId(1));
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
        assert_eq!(result.accepted()[0].translation(), "炎之剑\\N[1]！");
        assert_eq!(
            result.accepted()[0].propagation_targets(),
            &[super::super::standard::TranslationPropagationTarget::new(
                propagation_target(),
                state_context(101),
            )]
        );
    }

    #[tokio::test]
    async fn response_processor_preserves_missing_provider_metadata() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let result = processor
            .process(
                &task_with_language_pair("ja", "unconfigured-target", 1),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"炎之剑\\N[1]！"}]"#,
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());

        let bom = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"炎\uFEFF之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
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
                    r#"[{"id":0,"translation":" \r\n⟦ATT_ACTOR_NAME_WHOLE_0000⟧\r"}]"#,
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

        let normalized = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"第一行\r\n第二行\r⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-normalized".to_owned()),
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

    #[tokio::test]
    async fn unexpected_token_only_rejects_its_own_id() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let result = processor
            .process(
                &task_with_output_count(2),
                LlmResponse::new(
                    r#"[
                        {"id":0,"translation":"甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧"},
                        {"id":1,"translation":"乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧⟦ATT_UNKNOWN_WHOLE_9999⟧"}
                    ]"#,
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
        assert_eq!(result.accepted()[0].id(), 0);
        assert_eq!(result.unresolved().len(), 1);
        assert_eq!(result.unresolved()[0].id(), 1);
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let result = processor
            .process(
                &task_with_output_count(6),
                LlmResponse::new(
                    r#"[
                        {"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"},
                        {"id":1,"translation":"甲⟦ATT_ACTOR_NAME_WHOLE_0000⟧"},
                        {"id":"1","translation":"乙⟦ATT_ACTOR_NAME_WHOLE_0000⟧"},
                        {"id":3,"translation":""},
                        {"id":4,"translation":"缺少控制符"},
                        {"id":5,"translation":"译文です⟦ATT_ACTOR_NAME_WHOLE_0000⟧"},
                        {"id":99,"translation":"未知"}
                    ]"#,
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
            TranslationUnitRejectionReason::BlankTranslation
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
        assert_eq!(result.diagnostics().len(), 2);
    }

    #[tokio::test]
    async fn response_schema_errors_reject_the_entire_batch() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        for invalid_item in [
            r#""不是对象""#,
            r#"{"translation":"无 ID"}"#,
            r#"{"id":"bad","translation":"非法 ID"}"#,
            r#"{"id":"","translation":"空 ID"}"#,
            r#"{"id":-1,"translation":"负数 ID"}"#,
            r#"{"id":1.5,"translation":"浮点 ID"}"#,
            r#"{"id":true,"translation":"布尔 ID"}"#,
            r#"{"id":"999999999999999999999999999999999999","translation":"溢出 ID"}"#,
            r#"{"id":1}"#,
            r#"{"id":1,"translation":123}"#,
            r#"{"id":1,"translation":"未知字段","extra":true}"#,
            r#"{"id":1,"id":2,"translation":"重复 ID 字段"}"#,
            r#"{"id":1,"translation":"甲","translation":"重复译文字段"}"#,
        ] {
            let content = format!(
                r#"[{{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}},{invalid_item}]"#
            );
            let result = processor
                .process(
                    &task_with_output_count(2),
                    LlmResponse::new(
                        content,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-schema".to_owned()),
                        None,
                    ),
                    1,
                )
                .await
                .expect("模型结构错误应成为正常不可用结果");

            assert!(matches!(
                &result,
                TranslationTaskOutcome::Unavailable {
                    reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                    ..
                }
            ));
            assert!(result.accepted().is_empty());
            assert_eq!(result.unresolved().len(), 2);
            assert!(result.unresolved().iter().all(|unit| matches!(
                unit.reason(),
                TranslationUnitRejectionReason::InvalidShape { .. }
            )));
            assert!(matches!(
                result.diagnostics(),
                [TranslationProtocolDiagnostic::InvalidResponse { .. }]
            ));
        }
    }

    #[tokio::test]
    async fn response_processor_returns_persistable_unavailable_outcomes() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
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
                    r#"[{"id":0,"translation":""}]"#,
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
                LlmResponse::new(
                    "[]",
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
    async fn response_language_unavailable_and_internal_invariant_are_technical_errors() {
        let processor =
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog());
        let language_error = processor
            .process(
                &task_with_language_pair("unknown", "any-target", 1),
                LlmResponse::new(
                    r#"[{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
                    LlmFinishReason::Stop,
                    None,
                    Some("response-language".to_owned()),
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
                        r#"[{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-mismatch".to_owned()),
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
                LlmResponse::new(
                    "[]",
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
    struct FakeDelay {
        waits: Arc<Mutex<Vec<Duration>>>,
    }

    impl AsyncDelay for FakeDelay {
        async fn wait(&self, duration: Duration) {
            self.waits.lock().expect("等待锁不应中毒").push(duration);
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
                Arc::new("llm-client"),
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
                        r#"[{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
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
    async fn executor_never_retries_invalid_response_schema() {
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
                    Ok(LlmResponse::new(
                        r#"[{"id":0,"translation":123}]"#,
                        LlmFinishReason::Stop,
                        None,
                        Some("response-invalid-shape".to_owned()),
                        None,
                    )),
                    Ok(LlmResponse::new(
                        r#"[{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );
        let outcome = service
            .execute(&profile(), task())
            .await
            .expect("模型内容不可用是正常结果");
        assert!(matches!(
            &outcome,
            TranslationTaskOutcome::Unavailable {
                reason: TranslationTaskUnavailableReason::ModelResponseUnusable,
                ..
            }
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
                    r#"[{"id":0,"translation":"炎之剑⟦ATT_ACTOR_NAME_WHOLE_0000⟧"}]"#,
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
            TranslationTaskResponseProcessingService::new(InlineCpu, language_catalog()),
        );
        let profile = profile();
        assert_send(service.execute(&profile, task()));
    }
}
