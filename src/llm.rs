//! 跨引擎共享的非流式 LLM 请求契约。
//!
//! 本模块只表达调用方与模型请求根之间共同拥有的消息、响应和单次请求失败
//! 语义。具体协议、认证、资源治理和重试策略分别由根适配器与调用方拥有。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{Map, Value};
#[cfg(test)]
use url::{Url, form_urlencoded};

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

    /// 该根错误是否明确表示请求仍在等待本地入场资源时被合作取消。
    ///
    /// 调用方还必须同时观察自己的取消令牌；单凭错误类别不得把根关闭等技术失败
    /// 误归类为用户取消。
    fn is_cancelled_wait(&self) -> bool {
        false
    }
}

/// 一个 LLM Client 对供应商活动请求数的真实外部约束。
pub(crate) trait LlmClientConcurrency: Send + Sync {
    fn max_concurrent_requests(&self) -> NonZeroUsize;
}

/// 只替换当前所选 LLM Client 的 API key 实际值。
///
/// Prompt、模型正文、参数及其他普通文本不属于敏感闭集，因此替换器不删除字段、
/// 不清空正文，也不按内容类别扩大匹配范围。
#[derive(Clone)]
pub(crate) struct ApiKeyRedactor {
    api_key: SecretString,
}

impl ApiKeyRedactor {
    const REPLACEMENT: &'static str = "[REDACTED API KEY]";

    pub(crate) fn new(api_key: SecretString) -> Self {
        Self { api_key }
    }

    pub(crate) fn redact(&self, value: &str) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            value.to_owned()
        } else {
            Self::replace_literal(value, api_key)
        }
    }

    /// 递归序列化结构化数据，并在 JSON 转义发生前后都只替换 API key 实际值。
    ///
    /// 直接改写 `Map` 会在替换后键碰撞时丢字段；对完整序列化结果替换精确的 JSON
    /// string fragment 可以保留对象条目、数组顺序和值类型，同时覆盖键和值中的匹配。
    pub(crate) fn redact_json<T>(&self, value: &T) -> Result<String, serde_json::Error>
    where
        T: Serialize,
    {
        serde_json::to_string(value).map(|serialized| self.redact_serialized_json(&serialized))
    }

    pub(crate) fn redact_json_pretty<T>(&self, value: &T) -> Result<String, serde_json::Error>
    where
        T: Serialize,
    {
        serde_json::to_string_pretty(value)
            .map(|serialized| self.redact_serialized_json(&serialized))
    }

    /// 替换普通正文中的 API key，并识别 JSON string 内的转义表示。
    ///
    /// 无效或尚未解析的 Assistant 必须原样保留结构，不能先反序列化再整体重写；
    /// 因此这里只替换 key 对应的原始字节，闭合与未闭合 string 的其余内容都逐字保留。
    pub(crate) fn redact_text_with_json_strings(&self, value: &str) -> String {
        self.redact_json_string_tokens(value, false)
    }

    /// 替换整个 endpoint 中出现的 API key 实际值，逐字保留其余内容。
    ///
    /// URL 的原始分隔符(RFC 3986 gen/sub-delims)是结构而非内容:path 与
    /// fragment 按分隔符切段后只在段内容(percent-decode 后)中匹配;query 的
    /// key/value 按既有 `+`→空格 语义逐 pair 匹配。部分代理网关把 key 编入
    /// path,闭集替换不允许该位置成为漏洞。
    pub(crate) fn redact_url(&self, value: &str) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let fragment_delimiter = value.find('#');
        let query_end = fragment_delimiter.unwrap_or(value.len());
        let query_delimiter = value[..query_end].find('?');
        let path_end = query_delimiter.unwrap_or(query_end);

        let mut replacements = scan_delimited_url_region(value, 0, path_end, api_key);
        if let Some(fragment_delimiter) = fragment_delimiter {
            replacements.extend(scan_delimited_url_region(
                value,
                fragment_delimiter + 1,
                value.len(),
                api_key,
            ));
        }
        if let Some(query_delimiter) = query_delimiter {
            let query_start = query_delimiter + 1;
            let mut pair_start = query_start;
            while pair_start <= query_end {
                let pair_end = value[pair_start..query_end]
                    .find('&')
                    .map_or(query_end, |delimiter| pair_start + delimiter);
                let value_delimiter = value[pair_start..pair_end]
                    .find('=')
                    .map(|delimiter| pair_start + delimiter);
                let key_end = value_delimiter.unwrap_or(pair_end);
                replacements.extend(
                    decode_url_component(value, pair_start, key_end, true)
                        .api_key_source_ranges(api_key),
                );
                if let Some(value_delimiter) = value_delimiter {
                    replacements.extend(
                        decode_url_component(value, value_delimiter + 1, pair_end, true)
                            .api_key_source_ranges(api_key),
                    );
                }
                if pair_end == query_end {
                    break;
                }
                pair_start = pair_end + 1;
            }
        }

        replacements.sort_by_key(|source| (source.start, source.end));
        let mut output = String::with_capacity(value.len());
        let mut copied_until = 0usize;
        for source in replacements {
            output.push_str(&value[copied_until..source.start]);
            output.push_str(Self::REPLACEMENT);
            copied_until = source.end;
        }
        output.push_str(&value[copied_until..]);
        output
    }

    fn redact_serialized_json(&self, value: &str) -> String {
        self.redact_json_string_tokens(value, true)
    }

    fn redact_json_string_tokens(&self, value: &str, serialized_json: bool) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let bytes = value.as_bytes();
        let mut output = String::with_capacity(value.len());
        let mut copied_until = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            let remaining = &value[index..];
            if remaining.starts_with(Self::REPLACEMENT) {
                index += Self::REPLACEMENT.len();
                continue;
            }
            if bytes[index] == b'"' {
                let decoded = decode_json_string(value, index + 1);
                assert!(
                    !serialized_json || decoded.closed,
                    "serde_json 必须只生成闭合的 JSON string token"
                );
                for source in decoded.api_key_source_ranges(api_key) {
                    output.push_str(&value[copied_until..source.start]);
                    output.push_str(Self::REPLACEMENT);
                    copied_until = source.end;
                }
                index = decoded.next_index;
                continue;
            }
            if !serialized_json && remaining.starts_with(api_key) {
                output.push_str(&value[copied_until..index]);
                output.push_str(Self::REPLACEMENT);
                index += api_key.len();
                copied_until = index;
                continue;
            }
            index += remaining
                .chars()
                .next()
                .expect("非空 UTF-8 后缀必须包含字符")
                .len_utf8();
        }
        output.push_str(&value[copied_until..]);
        output
    }

    fn replace_literal(value: &str, needle: &str) -> String {
        if needle.is_empty() {
            return value.to_owned();
        }
        let mut output = String::with_capacity(value.len());
        let mut copied_until = 0usize;
        let mut index = 0usize;
        while index < value.len() {
            let remaining = &value[index..];
            if remaining.starts_with(Self::REPLACEMENT) {
                index += Self::REPLACEMENT.len();
            } else if remaining.starts_with(needle) {
                output.push_str(&value[copied_until..index]);
                output.push_str(Self::REPLACEMENT);
                index += needle.len();
                copied_until = index;
            } else {
                index += remaining
                    .chars()
                    .next()
                    .expect("非空 UTF-8 后缀必须包含字符")
                    .len_utf8();
            }
        }
        output.push_str(&value[copied_until..]);
        output
    }
}

#[derive(Clone, Copy)]
struct DecodedSourceRange {
    start: usize,
    end: usize,
}

struct DecodedJsonString {
    decoded: String,
    source_by_decoded_byte: Vec<DecodedSourceRange>,
    next_index: usize,
    closed: bool,
}

impl DecodedJsonString {
    fn api_key_source_ranges(&self, api_key: &str) -> Vec<DecodedSourceRange> {
        let replacement_ranges = self
            .decoded
            .match_indices(ApiKeyRedactor::REPLACEMENT)
            .map(|(start, replacement)| start..start + replacement.len())
            .collect::<Vec<_>>();
        self.decoded
            .match_indices(api_key)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                if replacement_ranges
                    .iter()
                    .any(|replacement| start >= replacement.start && end <= replacement.end)
                {
                    return None;
                }
                Some(DecodedSourceRange {
                    start: self.source_by_decoded_byte[start].start,
                    end: self.source_by_decoded_byte[end - 1].end,
                })
            })
            .collect()
    }
}

struct DecodedQueryComponent {
    decoded: Vec<u8>,
    source_by_decoded_byte: Vec<DecodedSourceRange>,
}

impl DecodedQueryComponent {
    fn api_key_source_ranges(&self, api_key: &str) -> Vec<DecodedSourceRange> {
        let api_key = api_key.as_bytes();
        let replacement = ApiKeyRedactor::REPLACEMENT.as_bytes();
        let mut replacements = Vec::new();
        let mut index = 0usize;
        while index < self.decoded.len() {
            let remaining = &self.decoded[index..];
            if remaining.starts_with(replacement) {
                index += replacement.len();
            } else if remaining.starts_with(api_key) {
                replacements.push(DecodedSourceRange {
                    start: self.source_by_decoded_byte[index].start,
                    end: self.source_by_decoded_byte[index + api_key.len() - 1].end,
                });
                index += api_key.len();
            } else {
                index += 1;
            }
        }
        replacements
    }
}

/// 在 path 或 fragment 区间内按原始分隔符切段并对段内容做解码感知匹配。
///
/// 原始 `/`、`;`、`,`、`:`、`@` 是结构分隔符,只有 percent-encoded 形式才属于
/// 段内容;因此含分隔符字符的 key 不会与结构本身误匹配。
fn scan_delimited_url_region(
    value: &str,
    start: usize,
    end: usize,
    api_key: &str,
) -> Vec<DecodedSourceRange> {
    let bytes = value.as_bytes();
    let mut replacements = Vec::new();
    let mut segment_start = start;
    let mut index = start;
    while index <= end {
        let at_delimiter =
            index < end && matches!(bytes[index], b'/' | b';' | b',' | b':' | b'@');
        if index == end || at_delimiter {
            if segment_start < index {
                replacements.extend(
                    decode_url_component(value, segment_start, index, false)
                        .api_key_source_ranges(api_key),
                );
            }
            segment_start = index + 1;
        }
        index += 1;
    }
    replacements
}

fn decode_url_component(
    value: &str,
    start: usize,
    end: usize,
    plus_is_space: bool,
) -> DecodedQueryComponent {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(end - start);
    let mut source_by_decoded_byte = Vec::with_capacity(end - start);
    let mut index = start;
    while index < end {
        if plus_is_space && bytes[index] == b'+' {
            decoded.push(b' ');
            source_by_decoded_byte.push(DecodedSourceRange {
                start: index,
                end: index + 1,
            });
            index += 1;
            continue;
        }
        if bytes[index] == b'%'
            && index + 2 < end
            && let (Some(high), Some(low)) = (
                decode_hex_digit(bytes[index + 1]),
                decode_hex_digit(bytes[index + 2]),
            )
        {
            decoded.push((high << 4) | low);
            source_by_decoded_byte.push(DecodedSourceRange {
                start: index,
                end: index + 3,
            });
            index += 3;
            continue;
        }

        let decoded_char = value[index..end]
            .chars()
            .next()
            .expect("非空 UTF-8 query component 必须包含字符");
        let source_end = index + decoded_char.len_utf8();
        decoded.extend_from_slice(&bytes[index..source_end]);
        source_by_decoded_byte.resize(
            source_by_decoded_byte.len() + decoded_char.len_utf8(),
            DecodedSourceRange {
                start: index,
                end: source_end,
            },
        );
        index = source_end;
    }
    DecodedQueryComponent {
        decoded,
        source_by_decoded_byte,
    }
}

fn decode_json_string(value: &str, content_start: usize) -> DecodedJsonString {
    let bytes = value.as_bytes();
    let mut decoded = String::new();
    let mut source_by_decoded_byte = Vec::new();
    let mut index = content_start;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            return DecodedJsonString {
                decoded,
                source_by_decoded_byte,
                next_index: index + 1,
                closed: true,
            };
        }
        if bytes[index] == b'\\'
            && let Some((decoded_char, source_end)) = decode_json_escape(bytes, index)
        {
            push_decoded_char(
                &mut decoded,
                &mut source_by_decoded_byte,
                decoded_char,
                DecodedSourceRange {
                    start: index,
                    end: source_end,
                },
            );
            index = source_end;
            continue;
        }

        let decoded_char = value[index..]
            .chars()
            .next()
            .expect("非空 UTF-8 后缀必须包含字符");
        let source_end = index + decoded_char.len_utf8();
        push_decoded_char(
            &mut decoded,
            &mut source_by_decoded_byte,
            decoded_char,
            DecodedSourceRange {
                start: index,
                end: source_end,
            },
        );
        index = source_end;
    }
    DecodedJsonString {
        decoded,
        source_by_decoded_byte,
        next_index: value.len(),
        closed: false,
    }
}

fn push_decoded_char(
    decoded: &mut String,
    source_by_decoded_byte: &mut Vec<DecodedSourceRange>,
    decoded_char: char,
    source: DecodedSourceRange,
) {
    decoded.push(decoded_char);
    source_by_decoded_byte.resize(
        source_by_decoded_byte.len() + decoded_char.len_utf8(),
        source,
    );
}

fn decode_json_escape(bytes: &[u8], slash: usize) -> Option<(char, usize)> {
    let escaped = *bytes.get(slash + 1)?;
    let decoded = match escaped {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{0008}',
        b'f' => '\u{000c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => return decode_json_unicode_escape(bytes, slash),
        _ => return None,
    };
    Some((decoded, slash + 2))
}

fn decode_json_unicode_escape(bytes: &[u8], slash: usize) -> Option<(char, usize)> {
    let first = decode_hex_quad(bytes, slash + 2)?;
    if (0xd800..=0xdbff).contains(&first) {
        let second_slash = slash + 6;
        if bytes.get(second_slash..second_slash + 2) != Some(b"\\u") {
            return None;
        }
        let second = decode_hex_quad(bytes, second_slash + 2)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return None;
        }
        let codepoint =
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
        return char::from_u32(codepoint).map(|decoded| (decoded, second_slash + 6));
    }
    if (0xdc00..=0xdfff).contains(&first) {
        return None;
    }
    char::from_u32(u32::from(first)).map(|decoded| (decoded, slash + 6))
}

fn decode_hex_quad(bytes: &[u8], start: usize) -> Option<u16> {
    let digits = bytes.get(start..start + 4)?;
    digits.iter().try_fold(0_u16, |value, digit| {
        Some((value << 4) | u16::from(decode_hex_digit(*digit)?))
    })
}

fn decode_hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
fn form_encoded_value(value: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair("", value)
        .finish()
        .strip_prefix('=')
        .expect("空 query key 必须产生等号")
        .to_owned()
}

impl fmt::Debug for ApiKeyRedactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiKeyRedactor")
            .finish_non_exhaustive()
    }
}

/// 当前实际所选 LLM Client 交给高级任务记录的稳定展示事实。
///
/// 这里不包含 HTTP Header，也不把请求正文反向解析成 Client 配置。API key 只保留在
/// 专用替换器中，字段本身永远不会进入任务文档。
#[derive(Clone)]
pub(crate) struct LlmClientRecordMetadata {
    endpoint: String,
    model: String,
    parameters: Map<String, Value>,
    api_key_redactor: ApiKeyRedactor,
}

impl LlmClientRecordMetadata {
    pub(crate) fn new(
        endpoint: String,
        model: String,
        parameters: Map<String, Value>,
        api_key_redactor: ApiKeyRedactor,
    ) -> Self {
        let endpoint = api_key_redactor.redact_url(&endpoint);
        let model = api_key_redactor.redact(&model);
        Self {
            endpoint,
            model,
            parameters,
            api_key_redactor,
        }
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn parameters(&self) -> &Map<String, Value> {
        &self.parameters
    }

    pub(crate) const fn api_key_redactor(&self) -> &ApiKeyRedactor {
        &self.api_key_redactor
    }
}

impl fmt::Debug for LlmClientRecordMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parameters = self
            .api_key_redactor
            .redact_json(&self.parameters)
            .expect("受信 LLM Client 参数必须能够序列化");
        formatter
            .debug_struct("LlmClientRecordMetadata")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("parameters", &parameters)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn api_key_redactor_covers_json_escaping_url_encoding_and_metadata_debug() {
        const API_KEY: &str = "quote\"slash\\value";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let structured = json!({
            format!("before-{API_KEY}-after"): [
                format!("left-{API_KEY}-right"),
                {"nested": API_KEY},
            ],
        });
        let json = redactor
            .redact_json_pretty(&structured)
            .expect("测试 JSON 应可序列化");
        let escaped_api_key = serde_json::to_string(API_KEY).expect("API key 应可序列化");
        let escaped_api_key = &escaped_api_key[1..escaped_api_key.len() - 1];

        assert!(!json.contains(API_KEY));
        assert!(!json.contains(escaped_api_key));
        assert_eq!(json.matches(ApiKeyRedactor::REPLACEMENT).count(), 3);
        assert!(json.contains("before-"));
        assert!(json.contains("-after"));
        assert!(json.contains("left-"));
        assert!(json.contains("-right"));

        let mut endpoint =
            Url::parse("https://example.test/v1/chat/completions").expect("测试 endpoint 应合法");
        endpoint
            .query_pairs_mut()
            .append_pair("token", &format!("before-{API_KEY}-after"));
        let endpoint = redactor.redact_url(endpoint.as_str());
        assert!(!endpoint.contains(API_KEY));
        assert!(!endpoint.contains(&form_encoded_value(API_KEY)));
        assert!(endpoint.contains(&format!("before-{}-after", ApiKeyRedactor::REPLACEMENT)));

        let metadata = LlmClientRecordMetadata::new(
            endpoint,
            format!("model-{API_KEY}"),
            structured.as_object().expect("测试参数必须是对象").clone(),
            redactor,
        );
        let debug = format!("{metadata:?}");
        assert!(!debug.contains(API_KEY));
        assert!(!debug.contains(escaped_api_key));
        assert!(debug.contains(ApiKeyRedactor::REPLACEMENT));
        assert!(debug.contains("before-"));
        assert!(debug.contains("-after"));
    }

    #[test]
    fn url_redaction_preserves_source_and_never_matches_query_syntax() {
        for (api_key, encoded_api_key) in [
            ("+", "%2B"),
            ("=", "%3d"),
            ("%", "%25"),
            ("&", "%26"),
            ("?", "%3F"),
            (":", "%3a"),
            ("#", "%23"),
        ] {
            let redactor = ApiKeyRedactor::new(SecretString::from(api_key));
            let endpoint = format!(
                "https://example.test/a%20b?ordinary=left+right&target={encoded_api_key}&neighbor=%2f#fragment%20x"
            );

            assert_eq!(
                redactor.redact_url(&endpoint),
                format!(
                    "https://example.test/a%20b?ordinary=left+right&target={}&neighbor=%2f#fragment%20x",
                    ApiKeyRedactor::REPLACEMENT
                ),
                "query 语法字符 {api_key:?} 只能匹配 component 解码后的实际内容"
            );
        }
    }

    #[test]
    fn url_redaction_covers_path_and_fragment_occurrences() {
        // 部分代理网关把 API key 编入 path;闭集替换必须覆盖整个 endpoint,
        // 不因 URL 没有 query 而早退。
        const API_KEY: &str = "sk-proxy-key-123";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));

        assert_eq!(
            redactor.redact_url(&format!(
                "https://gateway.test/{API_KEY}/v1/chat/completions"
            )),
            format!(
                "https://gateway.test/{}/v1/chat/completions",
                ApiKeyRedactor::REPLACEMENT
            ),
        );
        assert_eq!(
            redactor.redact_url("https://gateway.test/sk%2Dproxy%2Dkey%2D123/v1"),
            format!("https://gateway.test/{}/v1", ApiKeyRedactor::REPLACEMENT),
        );
        assert_eq!(
            redactor.redact_url(&format!("https://gateway.test/v1#{API_KEY}")),
            format!("https://gateway.test/v1#{}", ApiKeyRedactor::REPLACEMENT),
        );
        assert_eq!(
            redactor.redact_url(&format!(
                "https://gateway.test/{API_KEY}/v1?token={API_KEY}#{API_KEY}"
            )),
            format!(
                "https://gateway.test/{0}/v1?token={0}#{0}",
                ApiKeyRedactor::REPLACEMENT
            ),
        );
        // path 分隔符是结构而非内容:含 '/' 的 key 不与结构误匹配,
        // percent-encoded 形式才是段内容。
        let slash_redactor = ApiKeyRedactor::new(SecretString::from("a/b"));
        assert_eq!(
            slash_redactor.redact_url("https://gateway.test/a/b/v1?x=a%2Fb"),
            format!(
                "https://gateway.test/a/b/v1?x={}",
                ApiKeyRedactor::REPLACEMENT
            ),
        );
    }

    #[test]
    fn url_redaction_maps_across_mixed_query_encoding_without_normalizing_neighbors() {
        const API_KEY: &str = "a+b=c%";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let endpoint = "https://example.test/v1?\
                        a%2Bb%3Dc%25=value&\
                        token=before-a%2Bb%3Dc%25-after&\
                        ordinary=x+y&case=%2f";

        assert_eq!(
            redactor.redact_url(endpoint),
            "https://example.test/v1?\
             [REDACTED API KEY]=value&\
             token=before-[REDACTED API KEY]-after&\
             ordinary=x+y&case=%2f"
        );
        assert_eq!(
            redactor.redact_url("https://example.test/#literal?%25"),
            "https://example.test/#literal?%25",
            "fragment 内的问号不得反向建立 query"
        );
    }

    #[test]
    fn json_redaction_changes_only_string_tokens_not_adjacent_scalar_values() {
        let redactor = ApiKeyRedactor::new(SecretString::from("1"));
        let structured = json!({
            "1": "before-1-after",
            "number": 1,
            "exponent": 1e10,
            "boolean": true,
            "nothing": null,
        });
        let redacted = redactor
            .redact_json(&structured)
            .expect("测试 JSON 应可序列化");
        let reparsed: Value = serde_json::from_str(&redacted).expect("替换后必须仍是合法 JSON");

        assert_eq!(
            reparsed[ApiKeyRedactor::REPLACEMENT],
            "before-[REDACTED API KEY]-after"
        );
        assert_eq!(reparsed["number"], 1);
        assert_eq!(reparsed["exponent"], 1e10);
        assert_eq!(reparsed["boolean"], true);
        assert!(reparsed["nothing"].is_null());
    }

    #[test]
    fn json_redaction_prefers_the_complete_escape_when_the_key_is_its_prefix() {
        let redactor = ApiKeyRedactor::new(SecretString::from("\\"));
        let structured = json!({
            "\\": "before-\\-after",
        });
        let redacted = redactor
            .redact_json(&structured)
            .expect("测试 JSON 应可序列化");
        let reparsed: Value = serde_json::from_str(&redacted).expect("替换后必须仍是合法 JSON");

        assert_eq!(
            reparsed[ApiKeyRedactor::REPLACEMENT],
            "before-[REDACTED API KEY]-after"
        );
    }

    #[test]
    fn json_redaction_round_trips_every_visible_single_byte_header_value() {
        for byte in b'!'..=b'~' {
            let api_key = char::from(byte).to_string();
            let redactor = ApiKeyRedactor::new(SecretString::from(api_key.clone()));
            let redacted = redactor
                .redact_json(&json!([api_key]))
                .expect("测试 JSON 应可序列化");
            let reparsed: Value =
                serde_json::from_str(&redacted).expect("任何单字符 key 替换后都必须仍是合法 JSON");

            assert_eq!(
                reparsed[0],
                ApiKeyRedactor::REPLACEMENT,
                "单字符 key {byte:#04x} 必须只替换 string 内容"
            );
        }
    }

    #[test]
    fn backslash_key_does_not_match_the_escape_introducer_of_an_unrelated_quote() {
        let redactor = ApiKeyRedactor::new(SecretString::from("\\"));
        let redacted = redactor
            .redact_json(&json!(["\"", "\\"]))
            .expect("测试 JSON 应可序列化");
        let reparsed: Value = serde_json::from_str(&redacted).expect("替换后必须仍是合法 JSON");

        assert_eq!(reparsed[0], "\"");
        assert_eq!(reparsed[1], ApiKeyRedactor::REPLACEMENT);
    }

    #[test]
    fn arbitrary_text_redaction_recognizes_alternative_unicode_escapes() {
        let redactor = ApiKeyRedactor::new(SecretString::from("abc"));
        let raw = r#"{"value":"before-\u0061\u0062\u0063-after"#;

        let redacted = redactor.redact_text_with_json_strings(raw);

        assert_eq!(redacted, r#"{"value":"before-[REDACTED API KEY]-after"#);
    }

    #[test]
    fn arbitrary_text_redaction_preserves_unclosed_string_and_adjacent_escapes() {
        const API_KEY: &str = "quote\"slash\\value";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let encoded_key = serde_json::to_string(API_KEY).expect("API key 应可序列化");
        let encoded_key = &encoded_key[1..encoded_key.len() - 1];
        let raw =
            format!(r#"prefix {{"value":"\u0061\/\t-before-{encoded_key}-after-\u0062\\tail"#);

        let redacted = redactor.redact_text_with_json_strings(&raw);

        assert!(!redacted.contains(API_KEY));
        assert!(!redacted.contains(encoded_key));
        assert_eq!(
            redacted,
            format!(
                r#"prefix {{"value":"\u0061\/\t-before-{}-after-\u0062\\tail"#,
                ApiKeyRedactor::REPLACEMENT
            )
        );
    }

    #[test]
    fn short_api_key_does_not_reprocess_existing_or_inserted_replacement() {
        let redactor = ApiKeyRedactor::new(SecretString::from("API"));
        let raw = r#"{"value":"API [REDACTED API KEY] API"#;
        let expected = r#"{"value":"[REDACTED API KEY] [REDACTED API KEY] [REDACTED API KEY]"#;

        let redacted = redactor.redact_text_with_json_strings(raw);

        assert_eq!(redacted, expected);
        assert_eq!(
            redactor.redact_text_with_json_strings(&redacted),
            expected,
            "重复替换不得改写替换标记本身"
        );
    }
}
