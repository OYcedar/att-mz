//! 跨引擎共享的非流式 LLM 请求契约。
//!
//! 本模块只表达调用方与模型请求根之间共同拥有的消息、响应和单次请求失败
//! 语义。具体协议、认证、资源治理和重试策略分别由根适配器与调用方拥有。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::diagnostic::{DiagnosticImpact, SafeDiagnostic};
use crate::fingerprint::Sha256Fingerprint;

/// 发送给 LLM 的消息角色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatMessageRole {
    System,
    User,
    Assistant,
}

/// 调用方已经建立的一条确定性消息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChatMessage {
    role: ChatMessageRole,
    content: String,
}

impl ChatMessage {
    pub(crate) fn new(role: ChatMessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    pub(crate) const fn role(&self) -> ChatMessageRole {
        self.role
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }
}

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
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    usage: Option<LlmUsage>,
}

impl LlmResponse {
    pub(crate) fn new(
        content: impl Into<String>,
        finish_reason: LlmFinishReason,
        provider_request_id: Option<String>,
        provider_response_id: Option<String>,
        usage: Option<LlmUsage>,
    ) -> Self {
        Self {
            content: content.into(),
            finish_reason,
            provider_request_id,
            provider_response_id,
            usage,
        }
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    pub(crate) fn finish_reason(&self) -> &LlmFinishReason {
        &self.finish_reason
    }

    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }

    pub(crate) fn provider_response_id(&self) -> Option<&str> {
        self.provider_response_id.as_deref()
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

/// 执行一次非流式 LLM 请求的根能力。
///
/// 根适配器不自动重试。`Client` 由统一配置边界建立，根适配器只执行客户端
/// 已经确定的协议、认证、模型、资源上限和请求正文事实。
pub(crate) trait LlmRequestExecutor: Send + Sync {
    type Client: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a;
}

/// 一个受信 LLM Client 对译文结果有影响的稳定语义身份。
///
/// 实现只纳入会改变模型输出语义的协议事实；密钥、TLS、代理、
/// 超时、限速与并发等运行资源事实不得进入身份。这是公共 LLM
/// Client 对会消费已接受译文的上层提供的能力，不携带 MZ 特有规则。
pub(crate) trait LlmClientSemanticIdentity: Send + Sync {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint;
}

/// LLM 根错误能够公开的唯一结构化投影。
///
/// 实现必须直接读取具体根错误的类型化字段；不得返回请求、响应正文、Header 值，
/// 也不得通过 `Display` 或 source 链补猜事实。`retry_after` 来自同一次请求的响应头，
/// 因而由仍同时持有根错误与请求包装事实的位置传入。
pub(crate) trait LlmRequestDiagnosticSource {
    fn request_diagnostic(
        &self,
        retry_after: Option<Duration>,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic;
}

/// 一个 LLM Client 对供应商活动请求数的真实外部约束。
pub(crate) trait LlmClientConcurrency: Send + Sync {
    fn max_concurrent_requests(&self) -> NonZeroUsize;
}
