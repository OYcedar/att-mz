//! 跨引擎共享的 LLM 请求契约。
//!
//! 本模块只表达调用方与模型请求根之间共同拥有的消息、响应和单次请求失败
//! 语义。具体协议、认证、资源治理和重试策略分别由根适配器与调用方拥有。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use url::{Url, form_urlencoded};

use crate::diagnostic::{Diagnostic, DiagnosticReport};

/// 发送给 LLM 的消息角色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatMessageRole {
    System,
    User,
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

/// 单次 LLM 请求的结束原因。
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

/// 一次模型请求的未清洗统一响应。
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LlmResponse {
    content: Arc<String>,
    finish_reason: LlmFinishReason,
}

impl LlmResponse {
    pub(crate) fn new(content: impl Into<String>, finish_reason: LlmFinishReason) -> Self {
        Self {
            content: Arc::new(content.into()),
            finish_reason,
        }
    }

    pub(crate) fn content(&self) -> &str {
        self.content.as_str()
    }

    pub(crate) fn shared_content(&self) -> Arc<String> {
        Arc::clone(&self.content)
    }

    pub(crate) fn finish_reason(&self) -> &LlmFinishReason {
        &self.finish_reason
    }

    pub(crate) fn into_content_and_finish_reason(self) -> (String, LlmFinishReason) {
        (
            Arc::try_unwrap(self.content).unwrap_or_else(|content| (*content).clone()),
            self.finish_reason,
        )
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
    /// 同一运行已经确认必须停止后续模型请求；本工作没有再次调用供应商。
    AdmissionStopped { diagnostic: DiagnosticReport },
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
            Self::AdmissionStopped { .. } => {
                formatter.write_str("LLM 请求因同一运行已停止后续准入而未发送")
            }
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
            Self::AdmissionStopped { .. } => None,
        }
    }
}

/// 执行一次 LLM 请求的根能力。
///
/// 根适配器不自动重试。`Client` 由统一配置边界建立，根适配器只执行客户端
/// 已经确定的协议、认证、模型、资源上限和请求正文事实。
pub(crate) trait LlmRequestExecutor: Send + Sync {
    type Client: Send + Sync + 'static;
    type Error: LlmRequestFailure + Error + Send + Sync + 'static;

    fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a;

    /// 在本地准入完成、一次外部请求即将实际执行时同步报告该事实。
    ///
    /// 默认实现适用于没有独立本地准入阶段的执行器；拥有许可、限速或关闭门的运行根
    /// 必须在这些门之后覆盖该方法。回调不得在序列化失败或准入拒绝时执行。
    fn request_with_attempt_observer<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
        on_attempt_started: Box<dyn FnOnce() + Send + 'a>,
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a {
        async move {
            on_attempt_started();
            self.request(client, messages).await
        }
    }

    /// 在执行器仍同时持有 Client 协议事实与根错误类型时建立唯一安全诊断。
    fn request_diagnostic(
        &self,
        client: &Self::Client,
        source: &Self::Error,
        retry_after: Option<Duration>,
    ) -> Diagnostic;

    /// 用已经公开清理过的类型化事实停止同一执行器的后续请求。
    fn stop_admission(&self, _service_status: LlmServiceStatus, _diagnostic: &DiagnosticReport) {}

    /// 一个可重试响应已经由调用方确认仍可继续；解除根在响应与决定之间保持的发送门控。
    fn continue_after_retryable(&self, _service_status: LlmServiceStatus) {}
}

/// LLM 根错误只公开合作取消判断；诊断投影由仍持有 Client 协议事实的执行器负责。
pub(crate) trait LlmRequestFailure {
    /// 该根错误是否明确表示请求仍在等待本地入场资源时被合作取消。
    ///
    /// 调用方还必须同时观察自己的取消令牌；单凭错误类别不得把根关闭等技术失败
    /// 误归类为用户取消。
    fn is_cancelled_wait(&self) -> bool {
        false
    }

    /// 该失败是否发生在一次 HTTP 请求已经实际发出之后。
    ///
    /// 本地准入等待、请求序列化和已经关闭的执行器都不得被上层计为模型 attempt；
    /// 传输、HTTP 状态和响应验收失败则已经消费了一次真实请求。
    fn request_was_sent(&self) -> bool {
        true
    }

    /// 返回外部服务已经以结构化状态确认的失败类别。
    ///
    /// 该事实只来自 HTTP 状态和允许公开的 provider code/type；调用方不得解析
    /// `Display` 或供应商 message 猜测是否应停止后续请求。
    fn service_status(&self) -> LlmServiceStatus {
        LlmServiceStatus::Other
    }
}

/// 跨引擎共享的外部模型服务状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LlmServiceStatus {
    Other,
    RateLimited,
    PermanentAuthorization,
    PermanentQuota,
    PermanentAccount,
}

impl LlmServiceStatus {
    pub(crate) const fn stops_admission_after_unavailable(self) -> bool {
        matches!(self, Self::RateLimited)
    }

    pub(crate) const fn is_permanent(self) -> bool {
        matches!(
            self,
            Self::PermanentAuthorization | Self::PermanentQuota | Self::PermanentAccount
        )
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

/// Translate 在确认本次实际 Client 前暂缓公开文本，确认后统一使用同一替换器。
///
/// Generic 可以从项目状态取得 Profile，因此项目日志可能早于 Client 选择建立。这个门只
/// 协调“尚未选择、已经选择、运行结束仍未选择”三种事实；非 Translate 不创建它。
#[derive(Clone, Default)]
pub(crate) struct ApiKeyRedactionGate {
    state: Arc<(Mutex<ApiKeyRedactionState>, Condvar)>,
}

#[derive(Default)]
enum ApiKeyRedactionState {
    #[default]
    Pending,
    Selected(Arc<ApiKeyRedactor>),
    NoSelection,
}

impl ApiKeyRedactionGate {
    pub(crate) fn selected(redactor: Arc<ApiKeyRedactor>) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ApiKeyRedactionState::Selected(redactor)),
                Condvar::new(),
            )),
        }
    }

    /// 保存本次实际选中的 Client。重复报告同一个选择是幂等的；选择一旦确定便不能替换。
    pub(crate) fn select(&self, redactor: Arc<ApiKeyRedactor>) {
        let (state, ready) = self.state.as_ref();
        let mut state = lock_redaction_state(state);
        match &*state {
            ApiKeyRedactionState::Pending => {
                *state = ApiKeyRedactionState::Selected(redactor);
                ready.notify_all();
            }
            ApiKeyRedactionState::Selected(current) => {
                assert!(
                    Arc::ptr_eq(current, &redactor),
                    "一次 Translate 运行不能改选另一个 API key 替换器"
                );
            }
            ApiKeyRedactionState::NoSelection => {
                panic!("Translate 运行结束后不能再选择 API key 替换器");
            }
        }
    }

    /// 只在运行没有确认 Client 时解除等待，使早期失败仍能完整呈现。
    pub(crate) fn finish_without_selection(&self) {
        let (state, ready) = self.state.as_ref();
        let mut state = lock_redaction_state(state);
        if matches!(*state, ApiKeyRedactionState::Pending) {
            *state = ApiKeyRedactionState::NoSelection;
            ready.notify_all();
        }
    }

    pub(crate) fn selected_redactor(&self) -> Option<Arc<ApiKeyRedactor>> {
        let (state, _) = self.state.as_ref();
        match &*lock_redaction_state(state) {
            ApiKeyRedactionState::Selected(redactor) => Some(Arc::clone(redactor)),
            ApiKeyRedactionState::Pending | ApiKeyRedactionState::NoSelection => None,
        }
    }

    pub(crate) fn redact_after_selection(&self, value: &str) -> String {
        self.wait_for_selection()
            .map_or_else(|| value.to_owned(), |redactor| redactor.redact(value))
    }

    fn wait_for_selection(&self) -> Option<Arc<ApiKeyRedactor>> {
        let (state, ready) = self.state.as_ref();
        let mut state = lock_redaction_state(state);
        loop {
            match &*state {
                ApiKeyRedactionState::Pending => {
                    state = ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                ApiKeyRedactionState::Selected(redactor) => return Some(Arc::clone(redactor)),
                ApiKeyRedactionState::NoSelection => return None,
            }
        }
    }
}

fn lock_redaction_state(
    state: &Mutex<ApiKeyRedactionState>,
) -> MutexGuard<'_, ApiKeyRedactionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        T: Serialize + ?Sized,
    {
        serde_json::to_string(value).map(|serialized| self.redact_serialized_json(&serialized))
    }

    /// 替换普通正文中的 API key，并识别 JSON string 内的转义表示。
    ///
    /// 无效或尚未解析的 Assistant 必须原样保留结构，不能先反序列化再整体重写；
    /// 因此这里只替换 key 对应的原始字节，闭合与未闭合 string 的其余内容都逐字保留。
    pub(crate) fn redact_text_with_json_strings(&self, value: &str) -> String {
        self.redact_json_string_tokens(value, false)
    }

    /// 替换 RPG Maker User message 中 API key 的原文和 Markdown 转义表示。
    ///
    /// RPG Maker Prompt 会在每个 ASCII 标点前插入反斜杠。任务记录接收的是已经渲染
    /// 的消息，因此必须同时识别原文和该确定性表示，且不能把普通反斜杠当作通用转义
    /// 再解释。
    pub(crate) fn redact_text_with_markdown_ascii_punctuation_escaped(
        &self,
        value: &str,
    ) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let mut escaped_api_key = String::with_capacity(api_key.len().saturating_mul(2));
        for character in api_key.chars() {
            if character.is_ascii_punctuation() {
                escaped_api_key.push('\\');
            }
            escaped_api_key.push(character);
        }
        let mut replacements = Self::literal_source_ranges(value, api_key);
        replacements.extend(Self::literal_source_ranges(value, &escaped_api_key));
        Self::replace_source_ranges(value, Self::merge_source_ranges(replacements))
    }

    /// 替换整个 endpoint 中出现的 API key 实际值，逐字保留其余内容。
    ///
    /// 全串字面匹配覆盖跨 URL 分隔符的实际值；同时按 URL component 解码后匹配
    /// percent-encoded 表示，query 沿用 `+`→空格语义。两条路径只收集源 range，
    /// 合并重叠后一次渲染，避免改写邻接字节或重复处理替换标记。
    pub(crate) fn redact_url(&self, value: &str) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let fragment_delimiter = value.find('#');
        let query_end = fragment_delimiter.unwrap_or(value.len());
        let query_delimiter = value[..query_end].find('?');
        let path_end = query_delimiter.unwrap_or(query_end);

        let mut replacements = Self::literal_source_ranges(value, api_key);
        replacements.extend(scan_delimited_url_region(value, 0, path_end, api_key));
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

        Self::replace_source_ranges(value, Self::merge_source_ranges(replacements))
    }

    fn redact_serialized_json(&self, value: &str) -> String {
        self.redact_json_string_tokens(value, true)
    }

    /// 只替换已经序列化的封闭 schema 中 JSON value 的字符串内容。
    ///
    /// 项目日志的字段名由当前 schema 固定；即使使用者把一个很短的 API key 选成项目名，
    /// 也不能因为替换命中 `event` 等固定 key 而破坏日志格式。该边界只处理 string
    /// value，跳过后接冒号的 object key，并保持原字段顺序和其余字节不变。
    #[cfg(test)]
    pub(crate) fn redact_serialized_json_values(&self, value: &str) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let bytes = value.as_bytes();
        let mut replacements = Vec::new();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                let decoded = decode_json_string(value, index + 1);
                assert!(
                    decoded.closed,
                    "serde_json 必须只生成闭合的 JSON string token"
                );
                let mut next = decoded.next_index;
                while bytes
                    .get(next)
                    .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    next += 1;
                }
                if bytes.get(next) != Some(&b':') {
                    replacements.extend(decoded.api_key_source_ranges(api_key));
                }
                index = decoded.next_index;
                continue;
            }
            index += value[index..]
                .chars()
                .next()
                .expect("非空 UTF-8 后缀必须包含字符")
                .len_utf8();
        }
        Self::replace_source_ranges(value, Self::merge_source_ranges(replacements))
    }

    fn redact_json_string_tokens(&self, value: &str, serialized_json: bool) -> String {
        let api_key = self.api_key.expose_secret();
        if api_key.is_empty() {
            return value.to_owned();
        }
        let bytes = value.as_bytes();
        let mut replacements = if serialized_json {
            Vec::new()
        } else {
            Self::literal_source_ranges(value, api_key)
        };
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                let decoded = decode_json_string(value, index + 1);
                assert!(
                    !serialized_json || decoded.closed,
                    "serde_json 必须只生成闭合的 JSON string token"
                );
                replacements.extend(decoded.api_key_source_ranges(api_key));
                index = decoded.next_index;
                continue;
            }
            index += value[index..]
                .chars()
                .next()
                .expect("非空 UTF-8 后缀必须包含字符")
                .len_utf8();
        }
        Self::replace_source_ranges(value, Self::merge_source_ranges(replacements))
    }

    fn replace_literal(value: &str, needle: &str) -> String {
        if needle.is_empty() {
            return value.to_owned();
        }
        Self::replace_source_ranges(value, Self::literal_source_ranges(value, needle))
    }

    fn literal_source_ranges(value: &str, needle: &str) -> Vec<DecodedSourceRange> {
        let mut replacement_ranges = value
            .match_indices(Self::REPLACEMENT)
            .map(|(start, replacement)| start..start + replacement.len())
            .peekable();
        value
            .match_indices(needle)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                while replacement_ranges
                    .peek()
                    .is_some_and(|replacement| replacement.end <= start)
                {
                    replacement_ranges.next();
                }
                if replacement_ranges
                    .peek()
                    .is_some_and(|replacement| start >= replacement.start && end <= replacement.end)
                {
                    return None;
                }
                Some(DecodedSourceRange { start, end })
            })
            .collect()
    }

    fn merge_source_ranges(mut replacements: Vec<DecodedSourceRange>) -> Vec<DecodedSourceRange> {
        replacements.sort_by_key(|source| (source.start, source.end));
        let mut merged: Vec<DecodedSourceRange> = Vec::with_capacity(replacements.len());
        for source in replacements {
            if let Some(previous) = merged.last_mut()
                && source.start < previous.end
            {
                previous.end = previous.end.max(source.end);
            } else {
                merged.push(source);
            }
        }
        merged
    }

    fn replace_source_ranges(value: &str, replacements: Vec<DecodedSourceRange>) -> String {
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
}

#[derive(Clone, Copy)]
struct DecodedSourceRange {
    start: usize,
    end: usize,
}

struct DecodedJsonString<'a> {
    source: &'a str,
    content_start: usize,
    content_end: usize,
    decoded: String,
    next_index: usize,
    closed: bool,
}

impl DecodedJsonString<'_> {
    fn api_key_source_ranges(&self, api_key: &str) -> Vec<DecodedSourceRange> {
        let mut replacement_ranges = self
            .decoded
            .match_indices(ApiKeyRedactor::REPLACEMENT)
            .map(|(start, replacement)| start..start + replacement.len())
            .peekable();
        let decoded_ranges =
            self.decoded
                .match_indices(api_key)
                .filter_map(|(start, matched)| {
                    let end = start + matched.len();
                    while replacement_ranges
                        .peek()
                        .is_some_and(|replacement| replacement.end <= start)
                    {
                        replacement_ranges.next();
                    }
                    if replacement_ranges.peek().is_some_and(|replacement| {
                        start >= replacement.start && end <= replacement.end
                    }) {
                        return None;
                    }
                    Some(DecodedSourceRange { start, end })
                })
                .collect::<Vec<_>>();
        self.map_decoded_ranges_to_source(&decoded_ranges)
    }

    fn map_decoded_ranges_to_source(
        &self,
        decoded_ranges: &[DecodedSourceRange],
    ) -> Vec<DecodedSourceRange> {
        let mut source_ranges = Vec::with_capacity(decoded_ranges.len());
        let mut range_index = 0usize;
        let mut decoded_index = 0usize;
        let mut source_index = self.content_start;
        let mut current_source_start = None;

        while source_index < self.content_end && range_index < decoded_ranges.len() {
            let target = decoded_ranges[range_index];
            if decoded_index == target.start {
                current_source_start = Some(source_index);
            }

            let (decoded_char, source_end) = decode_json_character(self.source, source_index);
            let decoded_end = decoded_index + decoded_char.len_utf8();
            if decoded_index < target.start {
                assert!(
                    decoded_end <= target.start,
                    "decoded JSON match 必须从字符边界开始"
                );
            } else if decoded_end == target.end {
                source_ranges.push(DecodedSourceRange {
                    start: current_source_start
                        .take()
                        .expect("decoded JSON match 必须记录源起点"),
                    end: source_end,
                });
                range_index += 1;
            } else {
                assert!(
                    decoded_end < target.end,
                    "decoded JSON match 必须在字符边界结束"
                );
            }

            decoded_index = decoded_end;
            source_index = source_end;
        }

        assert_eq!(
            range_index,
            decoded_ranges.len(),
            "每个 decoded JSON match 都必须映射回原始输入"
        );
        source_ranges
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
        let replacement_ranges = overlapping_byte_match_starts(&self.decoded, replacement)
            .into_iter()
            .map(|start| start..start + replacement.len())
            .collect::<Vec<_>>();
        let mut replacements = Vec::new();
        let mut replacement_index = 0usize;
        let mut accepted_until = 0usize;
        for index in overlapping_byte_match_starts(&self.decoded, api_key) {
            if index < accepted_until {
                continue;
            }
            while replacement_ranges
                .get(replacement_index)
                .is_some_and(|replacement| replacement.end <= index)
            {
                replacement_index += 1;
            }
            let match_end = index.saturating_add(api_key.len());
            let wholly_inside_replacement =
                replacement_ranges
                    .get(replacement_index)
                    .is_some_and(|replacement| {
                        index >= replacement.start && match_end <= replacement.end
                    });
            if wholly_inside_replacement {
                continue;
            }
            replacements.push(DecodedSourceRange {
                start: self.source_by_decoded_byte[index].start,
                end: self.source_by_decoded_byte[match_end - 1].end,
            });
            accepted_until = match_end;
        }
        replacements
    }
}

fn overlapping_byte_match_starts(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }

    let mut prefix_lengths = vec![0usize; needle.len()];
    let mut matched = 0usize;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix_lengths[index] = matched;
    }

    let mut starts = Vec::new();
    matched = 0;
    for (index, byte) in haystack.iter().copied().enumerate() {
        while matched > 0 && byte != needle[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if byte == needle[matched] {
            matched += 1;
        }
        if matched == needle.len() {
            starts.push(index + 1 - needle.len());
            matched = prefix_lengths[matched - 1];
        }
    }
    starts
}

/// 在 path 或 fragment 区间内按原始分隔符切段并对段内容做解码感知匹配。
///
/// 这条 component 解码路径把原始 `/`、`;`、`,`、`:`、`@` 视为结构；
/// 全串字面路径另行覆盖实际 key 跨越这些分隔符的情况。
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
        let at_delimiter = index < end && matches!(bytes[index], b'/' | b';' | b',' | b':' | b'@');
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

fn decode_json_string(value: &str, content_start: usize) -> DecodedJsonString<'_> {
    let bytes = value.as_bytes();
    let mut decoded = String::new();
    let mut index = content_start;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            return DecodedJsonString {
                source: value,
                content_start,
                content_end: index,
                decoded,
                next_index: index + 1,
                closed: true,
            };
        }
        let (decoded_char, source_end) = decode_json_character(value, index);
        decoded.push(decoded_char);
        index = source_end;
    }
    DecodedJsonString {
        source: value,
        content_start,
        content_end: value.len(),
        decoded,
        next_index: value.len(),
        closed: false,
    }
}

fn decode_json_character(value: &str, index: usize) -> (char, usize) {
    let bytes = value.as_bytes();
    if bytes[index] == b'\\'
        && let Some(decoded) = decode_json_escape(bytes, index)
    {
        return decoded;
    }

    let decoded_char = value[index..]
        .chars()
        .next()
        .expect("非空 UTF-8 后缀必须包含字符");
    (decoded_char, index + decoded_char.len_utf8())
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn api_key_redactor_covers_json_escaping_and_url_encoding() {
        const API_KEY: &str = "quote\"slash\\value";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let structured = json!({
            format!("before-{API_KEY}-after"): [
                format!("left-{API_KEY}-right"),
                {"nested": API_KEY},
            ],
        });
        let json = redactor
            .redact_json(&structured)
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

        let debug = format!("{redactor:?}");
        assert!(!debug.contains(API_KEY));
    }

    #[test]
    fn url_redaction_covers_raw_syntax_and_decoded_components() {
        let replacement = ApiKeyRedactor::REPLACEMENT;
        let cases = [
            (
                "+",
                "https://example.test/v1?left+right=%2B",
                format!("https://example.test/v1?left{replacement}right={replacement}"),
            ),
            (
                "=",
                "https://example.test/v1?left=right&encoded=%3D",
                format!(
                    "https://example.test/v1?left{replacement}right&encoded{replacement}{replacement}"
                ),
            ),
            (
                "%",
                "https://example.test/v1?encoded=%25&neighbor=%2f",
                format!("https://example.test/v1?encoded={replacement}&neighbor={replacement}2f"),
            ),
            (
                "&",
                "https://example.test/v1?left=right&encoded=%26",
                format!("https://example.test/v1?left=right{replacement}encoded={replacement}"),
            ),
            (
                "?",
                "https://example.test/v1?encoded=%3F#tail",
                format!("https://example.test/v1{replacement}encoded={replacement}#tail"),
            ),
            (
                ":",
                "https://example.test/v1?encoded=%3A",
                format!("https{replacement}//example.test/v1?encoded={replacement}"),
            ),
            (
                "#",
                "https://example.test/v1?encoded=%23#tail",
                format!("https://example.test/v1?encoded={replacement}{replacement}tail"),
            ),
        ];

        for (api_key, endpoint, expected) in cases {
            let redactor = ApiKeyRedactor::new(SecretString::from(api_key));

            assert_eq!(
                redactor.redact_url(endpoint),
                expected,
                "API key {api_key:?} 的原始 URL 语法位置与编码内容都必须替换"
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
        // 全串字面兜底覆盖跨 path 分隔符的实际 key；component 解码路径
        // 同时覆盖它的 percent-encoded 表示。
        let slash_redactor = ApiKeyRedactor::new(SecretString::from("a/b"));
        assert_eq!(
            slash_redactor.redact_url("https://gateway.test/a/b/v1?x=a%2Fb"),
            format!(
                "https://gateway.test/{0}/v1?x={0}",
                ApiKeyRedactor::REPLACEMENT,
            ),
        );
    }

    #[test]
    fn url_redaction_merges_overlapping_ranges_and_does_not_reprocess_markers() {
        let percent_redactor = ApiKeyRedactor::new(SecretString::from("%"));
        assert_eq!(
            percent_redactor.redact_url("https://example.test/%25?neighbor=%2f"),
            format!(
                "https://example.test/{0}?neighbor={0}2f",
                ApiKeyRedactor::REPLACEMENT
            ),
            "字面 '%' 与解码后的 '%25' 重叠时必须只渲染一次替换"
        );

        let redactor = ApiKeyRedactor::new(SecretString::from("API"));
        let once = redactor.redact_url("https://example.test/API?token=API");
        assert_eq!(
            once,
            format!(
                "https://example.test/{0}?token={0}",
                ApiKeyRedactor::REPLACEMENT
            )
        );
        assert_eq!(
            redactor.redact_url(&once),
            once,
            "替换标记自身包含 API key 文本时不得被二次处理"
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
    fn serialized_json_value_redaction_preserves_fixed_schema_keys() {
        let redactor = ApiKeyRedactor::new(SecretString::from("event"));
        let serialized = r#"{"event":"before-event-after","nested":{"fixed":"event"}}"#;

        let redacted = redactor.redact_serialized_json_values(serialized);

        assert_eq!(
            redacted,
            r#"{"event":"before-[REDACTED API KEY]-after","nested":{"fixed":"[REDACTED API KEY]"}}"#
        );
        let reparsed: Value = serde_json::from_str(&redacted).expect("替换后必须仍是合法 JSON");
        assert_eq!(reparsed["event"], "before-[REDACTED API KEY]-after");
    }

    #[test]
    fn deferred_redaction_waits_for_the_selected_client() {
        let gate = ApiKeyRedactionGate::default();
        let worker_gate = gate.clone();
        let worker =
            std::thread::spawn(move || worker_gate.redact_after_selection("project-demo-secret"));

        gate.select(Arc::new(ApiKeyRedactor::new(SecretString::from("secret"))));
        let redacted = worker.join().expect("延迟替换线程不应 panic");

        assert_eq!(redacted, "project-demo-[REDACTED API KEY]");
    }

    #[test]
    fn deferred_redaction_can_finish_before_a_client_is_selected() {
        let gate = ApiKeyRedactionGate::default();
        gate.finish_without_selection();

        assert_eq!(gate.redact_after_selection("plain text"), "plain text");
        assert!(gate.selected_redactor().is_none());
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

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_wide_json_string_maps_an_escaped_key_to_its_exact_source_interval() {
        const API_KEY: &str = "a\"\\😀z";
        const PADDING_LEN: usize = 1 << 20;
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let padding = "x".repeat(PADDING_LEN);
        let serialized = format!(r#"{{"value":"{padding}\u0061\"\\\uD83D\uDE00\u007A{padding}"}}"#);
        let expected = format!(
            r#"{{"value":"{padding}{}{padding}"}}"#,
            ApiKeyRedactor::REPLACEMENT
        );

        let redacted = redactor.redact_serialized_json(&serialized);

        assert_eq!(redacted, expected);
        let reparsed: Value =
            serde_json::from_str(&redacted).expect("宽字符串替换后必须仍是合法 JSON");
        assert_eq!(
            reparsed["value"]
                .as_str()
                .expect("value 必须保持为字符串")
                .len(),
            PADDING_LEN * 2 + ApiKeyRedactor::REPLACEMENT.len()
        );
    }

    #[test]
    fn arbitrary_text_redaction_also_matches_a_raw_key_starting_with_a_quote() {
        const API_KEY: &str = "\"secret";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let raw = "prefix \"secret trailing {";

        assert_eq!(
            redactor.redact_text_with_json_strings(raw),
            format!("prefix {} trailing {{", ApiKeyRedactor::REPLACEMENT)
        );
    }

    #[test]
    fn markdown_ascii_punctuation_redaction_covers_raw_and_rendered_keys() {
        const API_KEY: &str = "quote\"slash\\[value]?";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let escaped = API_KEY
            .chars()
            .fold(String::new(), |mut output, character| {
                if character.is_ascii_punctuation() {
                    output.push('\\');
                }
                output.push(character);
                output
            });
        let raw = format!(
            "raw={API_KEY}; rendered={escaped}; marker={}",
            ApiKeyRedactor::REPLACEMENT
        );
        let expected = format!(
            "raw={0}; rendered={0}; marker={0}",
            ApiKeyRedactor::REPLACEMENT
        );

        let redacted = redactor.redact_text_with_markdown_ascii_punctuation_escaped(&raw);
        assert_eq!(redacted, expected);
        assert_eq!(
            redactor.redact_text_with_markdown_ascii_punctuation_escaped(&redacted),
            expected,
            "重复替换不得改写替换标记"
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

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_many_json_markers_and_keys_are_redacted_in_one_ordered_scan() {
        const ITEMS: usize = 4_096;
        let redactor = ApiKeyRedactor::new(SecretString::from("API"));
        let values = vec![format!("API {} API", ApiKeyRedactor::REPLACEMENT); ITEMS];

        let redacted = redactor
            .redact_json(&values)
            .expect("大量测试字符串应可序列化");
        assert_eq!(
            redacted.matches(ApiKeyRedactor::REPLACEMENT).count(),
            ITEMS * 3
        );
        assert_eq!(
            redactor.redact_text_with_json_strings(&redacted),
            redacted,
            "大量既有替换标记必须保持幂等"
        );
    }

    #[test]
    fn api_key_extending_the_replacement_marker_is_still_fully_redacted() {
        let api_key = format!("{}-secret", ApiKeyRedactor::REPLACEMENT);
        let redactor = ApiKeyRedactor::new(SecretString::from(api_key.clone()));

        assert_eq!(
            redactor.redact(&format!("before-{api_key}-after")),
            format!("before-{}-after", ApiKeyRedactor::REPLACEMENT),
        );
        assert_eq!(
            redactor.redact_text_with_json_strings(&format!("plain={api_key}; json=\"{api_key}\"")),
            format!("plain={0}; json=\"{0}\"", ApiKeyRedactor::REPLACEMENT),
        );
        let redacted_json = redactor
            .redact_json(&json!([api_key.as_str()]))
            .expect("测试 JSON 应可序列化");
        let reparsed: Value =
            serde_json::from_str(&redacted_json).expect("替换后必须仍是合法 JSON");
        assert_eq!(reparsed[0], ApiKeyRedactor::REPLACEMENT);

        let mut endpoint = Url::parse("https://example.test/v1").expect("测试 endpoint 应合法");
        endpoint.query_pairs_mut().append_pair("token", &api_key);
        assert_eq!(
            redactor.redact_url(endpoint.as_str()),
            format!(
                "https://example.test/v1?token={}",
                ApiKeyRedactor::REPLACEMENT
            ),
        );
    }

    #[test]
    fn api_key_starting_inside_the_replacement_marker_and_extending_past_it_is_redacted() {
        const API_KEY: &str = "KEY]-secret";
        let redactor = ApiKeyRedactor::new(SecretString::from(API_KEY));
        let raw = format!("{}-secret", ApiKeyRedactor::REPLACEMENT);
        let expected = format!("[REDACTED API {}", ApiKeyRedactor::REPLACEMENT);

        assert_eq!(redactor.redact(&raw), expected);
        assert_eq!(redactor.redact_text_with_json_strings(&raw), expected);
        assert_eq!(
            redactor.redact_url("https://example.test/?token=%5BREDACTED+API+KEY%5D-secret"),
            format!(
                "https://example.test/?token=%5BREDACTED+API+{}",
                ApiKeyRedactor::REPLACEMENT
            ),
            "percent 解码后的匹配即使从既有 marker 内开始，也不能漏掉越界后缀"
        );
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_url_redaction_handles_a_degenerate_long_key_and_component_linearly() {
        const KEY_PREFIX_LEN: usize = 32 * 1024;
        const COMPONENT_LEN: usize = 256 * 1024;
        let api_key = format!("{}b", "a".repeat(KEY_PREFIX_LEN));
        let redactor = ApiKeyRedactor::new(SecretString::from(api_key));
        let component = "a".repeat(COMPONENT_LEN);
        let endpoint = format!("https://example.test/?token={component}%62");
        let expected = format!(
            "https://example.test/?token={}{}",
            "a".repeat(COMPONENT_LEN - KEY_PREFIX_LEN),
            ApiKeyRedactor::REPLACEMENT
        );

        assert_eq!(redactor.redact_url(&endpoint), expected);
    }
}
