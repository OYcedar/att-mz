#![allow(dead_code, reason = "翻译执行链尚未接入生产组合根")]

//! 标准翻译任务的模型调用、有限响应清洗与译后验收。
//!
//! 本模块实现业务层的重试和整任务原子验收，并把网络请求、可取消等待与
//! CPU 调度停在根接口。所有重试都复用 Planner 已经建立的完整消息，绝不
//! 追加隐式修复提示词。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

use super::language::TranslationLanguageCatalog;
use super::profile::{MzTranslationExecutionPayload, TranslationExecutionProfile};
use super::standard::{
    AppliedPlaceholder, ChatMessage, ExpectedTranslationOutput, StandardTranslationProfile,
    StandardTranslationTaskExecutor, StandardTranslationTaskIndex, TranslationLanguagePair,
    TranslationPatch, TranslationTaskBlock, ValidatedTranslationTaskResult,
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
    fn retry_delays(&self) -> &[Duration];
    fn max_retry_after(&self) -> Duration;
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

    fn retry_delays(&self) -> &[Duration] {
        self.payload().execution().retry_delays()
    }

    fn max_retry_after(&self) -> Duration {
        self.payload().execution().max_retry_after()
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

    fn retry_delays(&self) -> &[Duration] {
        self.as_ref().payload().execution().retry_delays()
    }

    fn max_retry_after(&self) -> Duration {
        self.as_ref().payload().execution().max_retry_after()
    }
}

/// 语言模块对译后文本的确定性处理结果。
#[derive(Debug)]
pub(crate) enum TranslationResponseLanguageValidationError<E> {
    /// 配置没有对应语言模块，或模块自身不可用；重试模型响应不能修复。
    Unavailable(E),
    /// 文本不满足目标语言或仍残留源语言；新的模型响应可能修复。
    Rejected { message: String },
}

impl<E> fmt::Display for TranslationResponseLanguageValidationError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "译后语言模块不可用：{source}"),
            Self::Rejected { message } => write!(formatter, "译文未通过语言验收：{message}"),
        }
    }
}

impl<E> Error for TranslationResponseLanguageValidationError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(source) => Some(source),
            Self::Rejected { .. } => None,
        }
    }
}

/// ResponseProcessor 从语言目录消费的窄能力。
///
/// 目标语言处理只接收不含占位符 token 的普通文本片段；源语言残留检查接收
/// 所有片段拼接后的文本。实现不得把 token 当作自然语言内容。
pub(crate) trait TranslationResponseLanguageValidator:
    Clone + Send + Sync + 'static
{
    type Error: Error + Send + Sync + 'static;

    fn normalize_target(
        &self,
        target_language: &str,
        text_without_tokens: &str,
    ) -> Result<String, TranslationResponseLanguageValidationError<Self::Error>>;

    fn validate_source_residual(
        &self,
        source_language: &str,
        text_without_tokens: &str,
    ) -> Result<(), TranslationResponseLanguageValidationError<Self::Error>>;
}

impl TranslationResponseLanguageValidator for TranslationLanguageCatalog {
    type Error = super::language::TranslationLanguageCatalogError;

    fn normalize_target(
        &self,
        target_language: &str,
        text_without_tokens: &str,
    ) -> Result<String, TranslationResponseLanguageValidationError<Self::Error>> {
        let target = self
            .target(target_language)
            .map_err(TranslationResponseLanguageValidationError::Unavailable)?;
        target
            .normalize_and_validate(text_without_tokens)
            .map_err(
                |source| TranslationResponseLanguageValidationError::Rejected {
                    message: source.to_string(),
                },
            )
    }

    fn validate_source_residual(
        &self,
        source_language: &str,
        text_without_tokens: &str,
    ) -> Result<(), TranslationResponseLanguageValidationError<Self::Error>> {
        let source = self
            .source(source_language)
            .map_err(TranslationResponseLanguageValidationError::Unavailable)?;
        if let Some(residual) = source.find_residual(text_without_tokens) {
            return Err(TranslationResponseLanguageValidationError::Rejected {
                message: format!("仍包含源语言片段 {:?}", residual.fragment()),
            });
        }
        Ok(())
    }
}

/// ResponseProcessor 错误是否允许重新请求完全相同的 messages。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponseFailureDisposition {
    RetryableContent,
    Fatal,
}

/// 将一次原始模型响应验收为可原子提交结果。
pub(crate) trait TranslationTaskResponseProcessor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn process(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
    ) -> impl Future<Output = Result<ValidatedTranslationTaskResult, Self::Error>> + Send;

    fn failure_disposition(error: &Self::Error) -> ResponseFailureDisposition;
}

/// 使用 CPU 根完成有限 JSON 清洗、严格协议校验与译后处理。
pub(crate) struct TranslationTaskResponseProcessingService<C, V> {
    cpu: C,
    language_validator: V,
}

impl<C, V> TranslationTaskResponseProcessingService<C, V> {
    pub(crate) fn new(cpu: C, language_validator: V) -> Self {
        Self {
            cpu,
            language_validator,
        }
    }
}

impl<C, V> TranslationTaskResponseProcessor for TranslationTaskResponseProcessingService<C, V>
where
    C: CpuTaskExecutor,
    V: TranslationResponseLanguageValidator,
{
    type Error = TranslationTaskResponseProcessingError<C::Error, V::Error>;

    async fn process(
        &self,
        task: &TranslationTaskBlock,
        response: LlmResponse,
    ) -> Result<ValidatedTranslationTaskResult, Self::Error> {
        let input = ResponseProcessingInput {
            task_index: task.index(),
            language_pair: task.language_pair().clone(),
            expected_outputs: task.expected_outputs().to_vec(),
        };
        let language_validator = self.language_validator.clone();
        self.cpu
            .execute(move || process_response(input, response, &language_validator))
            .await
            .map_err(TranslationTaskResponseProcessingError::ScheduleCompute)?
            .map_err(TranslationTaskResponseProcessingError::Validation)
    }

    fn failure_disposition(error: &Self::Error) -> ResponseFailureDisposition {
        match error {
            TranslationTaskResponseProcessingError::ScheduleCompute(_) => {
                ResponseFailureDisposition::Fatal
            }
            TranslationTaskResponseProcessingError::Validation(error) => error.disposition(),
        }
    }
}

/// 一个响应的 CPU 调度或内容验收失败。
#[derive(Debug)]
pub(crate) enum TranslationTaskResponseProcessingError<C, L> {
    ScheduleCompute(CpuTaskExecutionError<C>),
    Validation(TranslationResponseValidationError<L>),
}

impl<C, L> fmt::Display for TranslationTaskResponseProcessingError<C, L>
where
    C: fmt::Display,
    L: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleCompute(source) => write!(formatter, "调度译后 CPU 验收失败：{source}"),
            Self::Validation(source) => source.fmt(formatter),
        }
    }
}

impl<C, L> Error for TranslationTaskResponseProcessingError<C, L>
where
    C: Error + 'static,
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleCompute(source) => Some(source),
            Self::Validation(source) => source.source(),
        }
    }
}

/// 模型内容未形成一个可整体提交的结果。
#[derive(Debug)]
pub(crate) enum TranslationResponseValidationError<L> {
    IncompleteResponse { finish_reason: LlmFinishReason },
    InvalidJson { message: String },
    InvalidId { value: String },
    DuplicateId { id: usize },
    MissingId { id: usize },
    UnknownId { id: usize },
    BlankTranslation { id: usize },
    PlaceholderMismatch { id: usize, token: String },
    PlaceholderNormalizationAmbiguous { id: usize, original: String },
    LanguageRejected { message: String },
    LanguageUnavailable(L),
    InternalInvariant { message: String },
}

impl<L> TranslationResponseValidationError<L> {
    const fn disposition(&self) -> ResponseFailureDisposition {
        match self {
            Self::LanguageUnavailable(_) | Self::InternalInvariant { .. } => {
                ResponseFailureDisposition::Fatal
            }
            _ => ResponseFailureDisposition::RetryableContent,
        }
    }
}

impl<L> fmt::Display for TranslationResponseValidationError<L>
where
    L: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteResponse { finish_reason } => {
                write!(formatter, "模型响应未完整结束：{finish_reason}")
            }
            Self::InvalidJson { message } => write!(formatter, "模型响应 JSON 无效：{message}"),
            Self::InvalidId { value } => write!(formatter, "模型响应包含无效 ID：{value}"),
            Self::DuplicateId { id } => write!(formatter, "模型响应重复返回 ID {id}"),
            Self::MissingId { id } => write!(formatter, "模型响应缺少 ID {id}"),
            Self::UnknownId { id } => write!(formatter, "模型响应包含未知 ID {id}"),
            Self::BlankTranslation { id } => write!(formatter, "ID {id} 的译文为空"),
            Self::PlaceholderMismatch { id, token } => {
                write!(formatter, "ID {id} 的占位符数量不匹配：{token}")
            }
            Self::PlaceholderNormalizationAmbiguous { id, original } => write!(
                formatter,
                "ID {id} 返回的原控制符无法唯一映射回占位符：{original:?}"
            ),
            Self::LanguageRejected { message } => {
                write!(formatter, "模型响应未通过语言验收：{message}")
            }
            Self::LanguageUnavailable(source) => write!(formatter, "译后语言模块不可用：{source}"),
            Self::InternalInvariant { message } => {
                write!(formatter, "翻译任务内部不变量已破坏：{message}")
            }
        }
    }
}

impl<L> Error for TranslationResponseValidationError<L>
where
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LanguageUnavailable(source) => Some(source),
            _ => None,
        }
    }
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
    ) -> Result<ValidatedTranslationTaskResult, Self::Error> {
        let mut attempt = 1_usize;
        let mut retry_delays = profile.retry_delays().iter().copied();

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
                    let Some(configured_delay) = retry_delays.next() else {
                        return Err(
                            MzStandardTranslationTaskExecutionError::RequestRetriesExhausted {
                                attempts: attempt,
                                source,
                            },
                        );
                    };
                    let delay = choose_retry_delay(
                        configured_delay,
                        retry_after,
                        profile.max_retry_after(),
                    )
                    .map_err(|retry_after| {
                        MzStandardTranslationTaskExecutionError::RetryAfterExceedsLimit {
                            attempt,
                            retry_after,
                            maximum: profile.max_retry_after(),
                            source,
                        }
                    })?;
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

            match self.response_processor.process(&task, response).await {
                Ok(result) => return Ok(result),
                Err(source)
                    if R::failure_disposition(&source)
                        == ResponseFailureDisposition::RetryableContent =>
                {
                    let Some(delay) = retry_delays.next() else {
                        return Err(
                            MzStandardTranslationTaskExecutionError::ResponseRetriesExhausted {
                                attempts: attempt,
                                source,
                            },
                        );
                    };
                    self.delay.wait(delay).await.map_err(|delay_source| {
                        MzStandardTranslationTaskExecutionError::WaitBeforeRetry {
                            attempt,
                            delay,
                            source: delay_source,
                        }
                    })?;
                    attempt += 1;
                }
                Err(source) => {
                    return Err(MzStandardTranslationTaskExecutionError::ProcessResponse {
                        attempt,
                        source,
                    });
                }
            }
        }
    }
}

fn choose_retry_delay(
    configured_delay: Duration,
    retry_after: Option<Duration>,
    maximum_retry_after: Duration,
) -> Result<Duration, Duration> {
    if let Some(retry_after) = retry_after {
        if retry_after > maximum_retry_after {
            return Err(retry_after);
        }
        Ok(configured_delay.max(retry_after))
    } else {
        Ok(configured_delay)
    }
}

/// 单任务模型执行失败。
#[derive(Debug)]
pub(crate) enum MzStandardTranslationTaskExecutionError<L, D, R> {
    FatalRequest {
        attempt: usize,
        source: L,
    },
    RequestRetriesExhausted {
        attempts: usize,
        source: L,
    },
    RetryAfterExceedsLimit {
        attempt: usize,
        retry_after: Duration,
        maximum: Duration,
        source: L,
    },
    ResponseRetriesExhausted {
        attempts: usize,
        source: R,
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
            Self::RequestRetriesExhausted { attempts, source } => {
                write!(formatter, "LLM 请求在 {attempts} 次尝试后仍失败：{source}")
            }
            Self::RetryAfterExceedsLimit {
                attempt,
                retry_after,
                maximum,
                source,
            } => write!(
                formatter,
                "第 {attempt} 次 LLM 请求要求等待 {retry_after:?}，超过外部上限 {maximum:?}：{source}"
            ),
            Self::ResponseRetriesExhausted { attempts, source } => write!(
                formatter,
                "模型内容在 {attempts} 次尝试后仍未通过验收：{source}"
            ),
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
            Self::FatalRequest { source, .. }
            | Self::RequestRetriesExhausted { source, .. }
            | Self::RetryAfterExceedsLimit { source, .. } => Some(source),
            Self::ResponseRetriesExhausted { source, .. }
            | Self::ProcessResponse { source, .. } => Some(source),
            Self::WaitBeforeRetry { source, .. } => Some(source),
        }
    }
}

struct ResponseProcessingInput {
    task_index: StandardTranslationTaskIndex,
    language_pair: TranslationLanguagePair,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

fn process_response<V>(
    input: ResponseProcessingInput,
    response: LlmResponse,
    language_validator: &V,
) -> Result<ValidatedTranslationTaskResult, TranslationResponseValidationError<V::Error>>
where
    V: TranslationResponseLanguageValidator,
{
    if response.finish_reason != LlmFinishReason::Stop {
        return Err(TranslationResponseValidationError::IncompleteResponse {
            finish_reason: response.finish_reason,
        });
    }

    let cleaned = clean_model_json(&response.content)
        .map_err(|message| TranslationResponseValidationError::InvalidJson { message })?;
    let outputs = parse_model_outputs(&cleaned)?;
    let expected_by_id = input
        .expected_outputs
        .iter()
        .map(|output| (output.id(), output))
        .collect::<BTreeMap<_, _>>();

    let mut actual_by_id = BTreeMap::new();
    for output in outputs {
        let id = output.id.into_usize()?;
        if output.translation.trim().is_empty() {
            return Err(TranslationResponseValidationError::BlankTranslation { id });
        }
        if actual_by_id.insert(id, output.translation).is_some() {
            return Err(TranslationResponseValidationError::DuplicateId { id });
        }
    }

    for id in actual_by_id.keys() {
        if !expected_by_id.contains_key(id) {
            return Err(TranslationResponseValidationError::UnknownId { id: *id });
        }
    }
    for id in expected_by_id.keys() {
        if !actual_by_id.contains_key(id) {
            return Err(TranslationResponseValidationError::MissingId { id: *id });
        }
    }

    let mut updates = Vec::with_capacity(expected_by_id.len());
    for expected in &input.expected_outputs {
        let translation = actual_by_id.remove(&expected.id()).ok_or_else(|| {
            TranslationResponseValidationError::InternalInvariant {
                message: format!("已验证存在的 ID {} 无法再次取得", expected.id()),
            }
        })?;
        let translation = validate_and_restore_translation(
            expected.id(),
            translation,
            expected.applied_placeholders(),
            &input.language_pair,
            language_validator,
        )?;
        updates.push(TranslationPatch::new(
            expected.identity().clone(),
            expected.propagation_targets().to_vec(),
            translation,
            expected.terminology_dependencies().to_vec(),
        ));
    }

    Ok(ValidatedTranslationTaskResult::new(
        input.task_index,
        updates,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelOutput {
    id: RawTranslationId,
    translation: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawTranslationId {
    Integer(u64),
    DecimalString(String),
}

impl RawTranslationId {
    fn into_usize<L>(self) -> Result<usize, TranslationResponseValidationError<L>> {
        match self {
            Self::Integer(value) => {
                usize::try_from(value).map_err(|_| TranslationResponseValidationError::InvalidId {
                    value: value.to_string(),
                })
            }
            Self::DecimalString(value) => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(TranslationResponseValidationError::InvalidId { value });
                }
                value
                    .parse::<usize>()
                    .map_err(|_| TranslationResponseValidationError::InvalidId {
                        value: value.clone(),
                    })
            }
        }
    }
}

fn parse_model_outputs<L>(
    value: &str,
) -> Result<Vec<RawModelOutput>, TranslationResponseValidationError<L>> {
    serde_json::from_str(value).map_err(|source| TranslationResponseValidationError::InvalidJson {
        message: source.to_string(),
    })
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

fn validate_and_restore_translation<V>(
    id: usize,
    mut translation: String,
    placeholders: &[AppliedPlaceholder],
    language_pair: &TranslationLanguagePair,
    language_validator: &V,
) -> Result<String, TranslationResponseValidationError<V::Error>>
where
    V: TranslationResponseLanguageValidator,
{
    normalize_original_controls(id, &mut translation, placeholders)?;
    validate_token_multiset(id, &translation, placeholders)?;

    let token_set = placeholders
        .iter()
        .map(|placeholder| placeholder.token().to_owned())
        .collect::<BTreeSet<_>>();
    let segments = split_around_tokens(&translation, &token_set);
    let mut normalized = String::with_capacity(translation.len());
    let mut language_text = String::new();
    for segment in segments {
        match segment {
            ProtectedSegment::Text(text) => {
                let text = if text.trim().is_empty() {
                    text.to_owned()
                } else {
                    language_validator
                        .normalize_target(language_pair.target_language(), text)
                        .map_err(map_language_error)?
                };
                language_text.push_str(&text);
                normalized.push_str(&text);
            }
            ProtectedSegment::Token(token) => normalized.push_str(token),
        }
    }
    if language_text.trim().is_empty() {
        return Err(TranslationResponseValidationError::LanguageRejected {
            message: "译文去除占位符后没有任何有效文本".to_owned(),
        });
    }
    language_validator
        .validate_source_residual(language_pair.source_language(), &language_text)
        .map_err(map_language_error)?;

    for placeholder in placeholders {
        normalized = normalized.replace(placeholder.token(), placeholder.original());
    }
    for token in token_set {
        if normalized.contains(&token) {
            return Err(TranslationResponseValidationError::InternalInvariant {
                message: format!("恢复后仍残留任务占位符 {token}"),
            });
        }
    }
    Ok(normalized)
}

fn map_language_error<E>(
    error: TranslationResponseLanguageValidationError<E>,
) -> TranslationResponseValidationError<E> {
    match error {
        TranslationResponseLanguageValidationError::Unavailable(source) => {
            TranslationResponseValidationError::LanguageUnavailable(source)
        }
        TranslationResponseLanguageValidationError::Rejected { message } => {
            TranslationResponseValidationError::LanguageRejected { message }
        }
    }
}

fn normalize_original_controls<L>(
    id: usize,
    translation: &mut String,
    placeholders: &[AppliedPlaceholder],
) -> Result<(), TranslationResponseValidationError<L>> {
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
                    TranslationResponseValidationError::PlaceholderNormalizationAmbiguous {
                        id,
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
                TranslationResponseValidationError::PlaceholderNormalizationAmbiguous {
                    id,
                    original: original.to_owned(),
                },
            );
        }
    }
    Ok(())
}

fn validate_token_multiset<L>(
    id: usize,
    translation: &str,
    placeholders: &[AppliedPlaceholder],
) -> Result<(), TranslationResponseValidationError<L>> {
    let mut expected = BTreeMap::<&str, usize>::new();
    for placeholder in placeholders {
        *expected.entry(placeholder.token()).or_default() += 1;
    }
    for (token, count) in expected {
        if translation.matches(token).count() != count {
            return Err(TranslationResponseValidationError::PlaceholderMismatch {
                id,
                token: token.to_owned(),
            });
        }
    }
    Ok(())
}

enum ProtectedSegment<'a> {
    Text(&'a str),
    Token(&'a str),
}

fn split_around_tokens<'a>(
    value: &'a str,
    tokens: &'a BTreeSet<String>,
) -> Vec<ProtectedSegment<'a>> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    while cursor < value.len() {
        let next = tokens
            .iter()
            .filter_map(|token| {
                value[cursor..]
                    .find(token)
                    .map(|offset| (cursor + offset, token.as_str()))
            })
            .min_by(|(left_offset, left_token), (right_offset, right_token)| {
                left_offset
                    .cmp(right_offset)
                    .then_with(|| right_token.len().cmp(&left_token.len()))
            });
        let Some((offset, token)) = next else {
            segments.push(ProtectedSegment::Text(&value[cursor..]));
            break;
        };
        if offset > cursor {
            segments.push(ProtectedSegment::Text(&value[cursor..offset]));
        }
        segments.push(ProtectedSegment::Token(
            &value[offset..offset + token.len()],
        ));
        cursor = offset + token.len();
    }
    if value.is_empty() {
        segments.push(ProtectedSegment::Text(value));
    }
    segments
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
        TranslationLanguagePair, TranslationLeafIdentity,
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

    #[derive(Clone, Copy)]
    struct FakeLanguage;

    impl TranslationResponseLanguageValidator for FakeLanguage {
        type Error = FakeError;

        fn normalize_target(
            &self,
            target_language: &str,
            text: &str,
        ) -> Result<String, TranslationResponseLanguageValidationError<Self::Error>> {
            if target_language != "zh-Hans" {
                return Err(TranslationResponseLanguageValidationError::Unavailable(
                    FakeError("target"),
                ));
            }
            Ok(text.replace('！', "!"))
        }

        fn validate_source_residual(
            &self,
            source_language: &str,
            text: &str,
        ) -> Result<(), TranslationResponseLanguageValidationError<Self::Error>> {
            if source_language != "ja" {
                return Err(TranslationResponseLanguageValidationError::Unavailable(
                    FakeError("source"),
                ));
            }
            if text.contains("残留") {
                Err(TranslationResponseLanguageValidationError::Rejected {
                    message: "发现源文残留".to_owned(),
                })
            } else {
                Ok(())
            }
        }
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
        TranslationTaskBlock::new(
            StandardTranslationTaskIndex::new(2),
            TranslationLanguagePair::new("ja", "zh-Hans"),
            Vec::new(),
            vec![TerminologyDependency::new("炎の剣", "炎之剑")],
            vec![
                ChatMessage::new(ChatMessageRole::System, "# Contract"),
                ChatMessage::new(ChatMessageRole::User, "# Task"),
            ],
            vec![ExpectedTranslationOutput::new(
                0,
                identity(),
                vec![propagation_target()],
                vec![placeholder()],
                vec![TerminologyDependency::new("炎の剣", "炎之剑")],
            )],
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
        let processor = TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage);
        let result = processor
            .process(
                &task(),
                LlmResponse::new(
                    r#"[{"id":"0","translation":"炎之剑\\N[1]！"}]"#,
                    LlmFinishReason::Stop,
                    Some("request-1".to_owned()),
                    Some(LlmUsage::new(10, 5, 15)),
                ),
            )
            .await
            .expect("原控制符能够唯一对应时应规范化并恢复");

        assert_eq!(result.task_index(), StandardTranslationTaskIndex::new(2));
        assert_eq!(result.updates()[0].translation(), "炎之剑\\N[1]!");
        assert_eq!(
            result.updates()[0].propagation_targets(),
            &[propagation_target()]
        );
        assert_eq!(result.updates()[0].terminology_dependencies().len(), 1);
    }

    #[tokio::test]
    async fn response_processor_rejects_missing_ids_unknown_fields_and_residual_source() {
        let processor = TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage);
        for content in [
            "[]",
            r#"[{"id":0,"translation":"炎之剑","extra":true}]"#,
            r#"[{"id":1,"translation":"未知"}]"#,
            r#"[{"id":0,"translation":"甲"},{"id":"0","translation":"乙"}]"#,
            r#"[{"id":0,"translation":"缺少控制符"}]"#,
            r#"[{"id":0,"translation":"⟦ATT:ACTOR_NAME:0⟧"}]"#,
            r#"[{"id":0,"translation":"残留⟦ATT:ACTOR_NAME:0⟧"}]"#,
        ] {
            let error = processor
                .process(
                    &task(),
                    LlmResponse::new(content, LlmFinishReason::Stop, None, None),
                )
                .await
                .expect_err("不完整或不合规响应必须整任务失败");
            assert_eq!(
                TranslationTaskResponseProcessingService::<InlineCpu, FakeLanguage>::failure_disposition(&error),
                ResponseFailureDisposition::RetryableContent
            );
        }
    }

    #[tokio::test]
    async fn response_cpu_unavailable_is_fatal_instead_of_model_retryable() {
        let processor = TranslationTaskResponseProcessingService::new(UnavailableCpu, FakeLanguage);
        let error = processor
            .process(
                &task(),
                LlmResponse::new("[]", LlmFinishReason::Stop, None, None),
            )
            .await
            .expect_err("CPU 根不可用必须传播");

        assert_eq!(
            TranslationTaskResponseProcessingService::<UnavailableCpu, FakeLanguage>::failure_disposition(&error),
            ResponseFailureDisposition::Fatal
        );
        assert!(matches!(
            error,
            TranslationTaskResponseProcessingError::ScheduleCompute(
                CpuTaskExecutionError::Unavailable(FakeError("cpu"))
            )
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
            TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage),
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
    async fn executor_retries_invalid_model_content_with_same_budget() {
        let waits = Arc::new(Mutex::new(Vec::new()));
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
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            FakeDelay {
                waits: Arc::clone(&waits),
            },
            TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage),
        );
        service
            .execute(&profile(), task())
            .await
            .expect("模型内容失败应按外部延迟重试");
        assert_eq!(
            waits.lock().expect("等待锁不应中毒").as_slice(),
            &[Duration::from_millis(10)]
        );
    }

    #[tokio::test]
    async fn executor_does_not_retry_fatal_request_or_excessive_retry_after() {
        for (response, expected_retry_after_error) in [
            (Err(LlmRequestError::Fatal(FakeError("auth"))), false),
            (
                Err(LlmRequestError::Retryable {
                    source: FakeError("busy"),
                    retry_after: Some(Duration::from_secs(3)),
                }),
                true,
            ),
        ] {
            let messages = Arc::new(Mutex::new(Vec::new()));
            let waits = Arc::new(Mutex::new(Vec::new()));
            let service = MzStandardTranslationTaskExecutionService::<
                _,
                _,
                _,
                TranslationExecutionProfile<MzTranslationExecutionPayload<&'static str>>,
            >::new(
                FakeLlm {
                    responses: Arc::new(Mutex::new(VecDeque::from([response]))),
                    messages: Arc::clone(&messages),
                },
                FakeDelay {
                    waits: Arc::clone(&waits),
                },
                TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage),
            );

            let error = service
                .execute(&profile(), task())
                .await
                .expect_err("不可恢复请求或超长 Retry-After 必须立即停止");
            assert_eq!(messages.lock().expect("消息锁不应中毒").len(), 1);
            assert!(waits.lock().expect("等待锁不应中毒").is_empty());
            assert_eq!(
                matches!(
                    error,
                    MzStandardTranslationTaskExecutionError::RetryAfterExceedsLimit { .. }
                ),
                expected_retry_after_error
            );
        }
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
            TranslationTaskResponseProcessingService::new(InlineCpu, FakeLanguage),
        );
        let profile = profile();
        assert_send(service.execute(&profile, task()));
    }
}
