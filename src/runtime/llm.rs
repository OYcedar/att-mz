//! OpenAI-compatible Chat Completions 与 Responses 生产根。

use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Client, Proxy, StatusCode, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, watch};
use url::Url;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, HttpEndpoint, HttpEnvelopeViolation, HttpIssue, HttpJsonCategory,
    HttpResponseReadFailure, HttpTransportKind, HttpTransportPhase, SafeIdentifier, SafeText,
    StateEffect,
};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::llm::{
    ApiKeyRedactor, ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity,
    LlmFinishReason, LlmRequestError, LlmRequestExecutor, LlmRequestFailure, LlmResponse,
    LlmServiceStatus,
};
use crate::user_text::sanitize_user_text;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenAiProtocol {
    ChatCompletions,
    Responses,
}

impl OpenAiProtocol {
    fn complete_endpoint(self, mut url: Url) -> Url {
        let path_segments = url
            .path_segments()
            .expect("HTTP(S) URL 必须能够作为分层基础 URL")
            .collect::<Vec<_>>();
        let trailing_empty_segments = path_segments
            .iter()
            .rev()
            .take_while(|segment| segment.is_empty())
            .count();
        let segments = &path_segments[..path_segments.len() - trailing_empty_segments];
        let has_chat_completions = segments.ends_with(&["chat", "completions"]);
        let has_responses = segments.ends_with(&["responses"]);

        let mut path = url
            .path_segments_mut()
            .expect("HTTP(S) URL 必须能够修改路径");
        for _ in 0..trailing_empty_segments {
            path.pop();
        }
        if has_chat_completions {
            path.pop();
            path.pop();
        } else if has_responses {
            path.pop();
        }
        match self {
            Self::ChatCompletions => {
                path.push("chat");
                path.push("completions");
            }
            Self::Responses => {
                path.push("responses");
            }
        }
        drop(path);
        url
    }

    const fn semantic_domain(self) -> &'static [u8] {
        match self {
            // 保持现有 Chat Completions 语义指纹，使仅升级程序不会让当前译文失效。
            Self::ChatCompletions => b"att.llm.chat-completions.semantics",
            Self::Responses => b"att.llm.responses.semantics",
        }
    }
}

pub(crate) struct OpenAiEndpoint {
    url: Url,
    protocol: OpenAiProtocol,
}

impl OpenAiEndpoint {
    pub(crate) fn new(url: Url, protocol: OpenAiProtocol) -> Self {
        Self {
            url: protocol.complete_endpoint(url),
            protocol,
        }
    }
}

/// 一个可被不同翻译引擎共享的受信 LLM Client。
pub(crate) struct OpenAiCompatibleClient {
    url: Url,
    protocol: OpenAiProtocol,
    api_key: SecretString,
    model: Arc<str>,
    semantic_fingerprint: Sha256Fingerprint,
    max_concurrent_requests: NonZeroUsize,
    request_timeout: Duration,
    parameters: Arc<Map<String, Value>>,
    api_key_redactor: Arc<ApiKeyRedactor>,
    rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
}

impl OpenAiCompatibleClient {
    #[cfg(test)]
    pub(crate) fn new(
        url: Url,
        api_key: SecretString,
        model: impl Into<String>,
        max_concurrent_requests: NonZeroUsize,
        request_timeout: Duration,
        rate_limit: Option<(NonZeroU32, NonZeroU32)>,
        parameters: Map<String, Value>,
    ) -> Self {
        Self::new_with_endpoint(
            OpenAiEndpoint::new(url, OpenAiProtocol::ChatCompletions),
            api_key,
            model,
            max_concurrent_requests,
            request_timeout,
            rate_limit,
            parameters,
        )
    }

    pub(crate) fn new_with_endpoint(
        endpoint: OpenAiEndpoint,
        api_key: SecretString,
        model: impl Into<String>,
        max_concurrent_requests: NonZeroUsize,
        request_timeout: Duration,
        rate_limit: Option<(NonZeroU32, NonZeroU32)>,
        parameters: Map<String, Value>,
    ) -> Self {
        let rate_limiter = rate_limit.map(|(rpm, burst)| {
            Arc::new(RateLimiter::direct(
                Quota::per_minute(rpm).allow_burst(burst),
            ))
        });
        let model: String = model.into();
        let model: Arc<str> = Arc::from(model);
        let parameters = Arc::new(parameters);
        let OpenAiEndpoint { url, protocol } = endpoint;
        let semantic_fingerprint = openai_compatible_semantic_fingerprint(
            protocol,
            &url,
            model.as_ref(),
            parameters.as_ref(),
        );
        let api_key_redactor = Arc::new(ApiKeyRedactor::new(api_key.clone()));
        Self {
            url,
            protocol,
            api_key,
            model,
            semantic_fingerprint,
            max_concurrent_requests,
            request_timeout,
            parameters,
            api_key_redactor,
            rate_limiter,
        }
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> &str {
        self.model.as_ref()
    }

    #[cfg(test)]
    pub(crate) const fn api_key(&self) -> &SecretString {
        &self.api_key
    }

    #[cfg(test)]
    pub(crate) const fn protocol(&self) -> OpenAiProtocol {
        self.protocol
    }

    #[cfg(test)]
    pub(crate) const fn endpoint(&self) -> &Url {
        &self.url
    }

    pub(crate) fn api_key_redactor(&self) -> Arc<ApiKeyRedactor> {
        Arc::clone(&self.api_key_redactor)
    }
}

impl LlmClientSemanticIdentity for OpenAiCompatibleClient {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        self.semantic_fingerprint
    }
}

fn openai_compatible_semantic_fingerprint(
    protocol: OpenAiProtocol,
    url: &Url,
    model: &str,
    parameters: &Map<String, Value>,
) -> Sha256Fingerprint {
    let canonical_parameters = canonical_json_object_semantic_bytes(parameters);
    let mut hasher = Sha256FramedHasher::new(protocol.semantic_domain());
    hasher
        .frame(1, url.as_str().as_bytes())
        .frame(2, model.as_bytes())
        .frame(3, &canonical_parameters);
    hasher.finish()
}

/// 为翻译语义指纹建立与配置书写形式无关的 JSON object 编码。
///
/// 对象键递归排序，数组顺序保持不变；数字按任意精度十进制值规范化，因此
/// `0.2`、`2e-1` 与 `0.20` 具有同一个语义身份。显式工作栈避免自定义参数的
/// 合法嵌套深度进入 Rust 调用栈。
fn canonical_json_object_semantic_bytes(object: &Map<String, Value>) -> Vec<u8> {
    enum Work<'a> {
        Value(&'a Value),
        ObjectEntry(&'a str, &'a Value),
    }

    fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
        output.extend_from_slice(
            &u64::try_from(bytes.len())
                .expect("x86_64 usize 必须可表示为 u64")
                .to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }

    fn push_object<'a>(
        object: &'a Map<String, Value>,
        output: &mut Vec<u8>,
        work: &mut Vec<Work<'a>>,
    ) {
        output.push(6);
        output.extend_from_slice(
            &u64::try_from(object.len())
                .expect("x86_64 usize 必须可表示为 u64")
                .to_be_bytes(),
        );
        let mut entries = object.iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(key, _)| *key);
        work.extend(
            entries
                .into_iter()
                .rev()
                .map(|(key, value)| Work::ObjectEntry(key, value)),
        );
    }

    let mut output = Vec::new();
    let mut work = Vec::new();
    push_object(object, &mut output, &mut work);
    while let Some(item) = work.pop() {
        match item {
            Work::ObjectEntry(key, value) => {
                push_bytes(&mut output, key.as_bytes());
                work.push(Work::Value(value));
            }
            Work::Value(Value::Null) => output.push(0),
            Work::Value(Value::Bool(false)) => output.push(1),
            Work::Value(Value::Bool(true)) => output.push(2),
            Work::Value(Value::Number(number)) => {
                output.push(3);
                let number = CanonicalJsonNumber::parse(&number.to_string());
                output.push(u8::from(number.negative));
                push_bytes(&mut output, number.coefficient.as_bytes());
                output.push(u8::from(number.exponent.negative));
                push_bytes(&mut output, &number.exponent.magnitude);
            }
            Work::Value(Value::String(value)) => {
                output.push(4);
                push_bytes(&mut output, value.as_bytes());
            }
            Work::Value(Value::Array(values)) => {
                output.push(5);
                output.extend_from_slice(
                    &u64::try_from(values.len())
                        .expect("x86_64 usize 必须可表示为 u64")
                        .to_be_bytes(),
                );
                work.extend(values.iter().rev().map(Work::Value));
            }
            Work::Value(Value::Object(object)) => {
                push_object(object, &mut output, &mut work);
            }
        }
    }
    output
}

struct CanonicalJsonNumber {
    negative: bool,
    coefficient: String,
    exponent: SignedDecimal,
}

impl CanonicalJsonNumber {
    fn parse(value: &str) -> Self {
        let (negative, unsigned) = value
            .strip_prefix('-')
            .map_or((false, value), |value| (true, value));
        let (mantissa, exponent) = unsigned
            .split_once(['e', 'E'])
            .map_or((unsigned, "0"), |(mantissa, exponent)| (mantissa, exponent));
        let (integer, fraction) = mantissa
            .split_once('.')
            .map_or((mantissa, ""), |(integer, fraction)| (integer, fraction));
        let mut digits = String::with_capacity(integer.len() + fraction.len());
        digits.push_str(integer);
        digits.push_str(fraction);
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Self {
                negative: false,
                coefficient: "0".to_owned(),
                exponent: SignedDecimal::zero(),
            };
        }

        let trailing_zeroes = digits
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'0')
            .count();
        let coefficient = digits[..digits.len() - trailing_zeroes].to_owned();
        let mut exponent = SignedDecimal::parse(exponent);
        exponent.add_unsigned(fraction.len(), true);
        exponent.add_unsigned(trailing_zeroes, false);
        Self {
            negative,
            coefficient,
            exponent,
        }
    }
}

struct SignedDecimal {
    negative: bool,
    /// 无前导零的 ASCII 十进制绝对值；零固定为单个 `0`。
    magnitude: Vec<u8>,
}

impl SignedDecimal {
    fn zero() -> Self {
        Self {
            negative: false,
            magnitude: vec![b'0'],
        }
    }

    fn parse(value: &str) -> Self {
        let (negative, digits) = value.strip_prefix('-').map_or_else(
            || (false, value.strip_prefix('+').unwrap_or(value)),
            |digits| (true, digits),
        );
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            Self::zero()
        } else {
            Self {
                negative,
                magnitude: digits.as_bytes().to_vec(),
            }
        }
    }

    fn add_unsigned(&mut self, value: usize, negative: bool) {
        if value == 0 {
            return;
        }
        let right = value.to_string().into_bytes();
        if self.is_zero() {
            self.negative = negative;
            self.magnitude = right;
        } else if self.negative == negative {
            self.magnitude = add_decimal_magnitudes(&self.magnitude, &right);
        } else {
            match compare_decimal_magnitudes(&self.magnitude, &right) {
                std::cmp::Ordering::Greater => {
                    self.magnitude = subtract_decimal_magnitudes(&self.magnitude, &right);
                }
                std::cmp::Ordering::Less => {
                    self.magnitude = subtract_decimal_magnitudes(&right, &self.magnitude);
                    self.negative = negative;
                }
                std::cmp::Ordering::Equal => *self = Self::zero(),
            }
        }
        if self.is_zero() {
            self.negative = false;
        }
    }

    fn is_zero(&self) -> bool {
        self.magnitude == b"0"
    }
}

fn compare_decimal_magnitudes(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn add_decimal_magnitudes(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(left.len().max(right.len()) + 1);
    let mut left = left.iter().rev();
    let mut right = right.iter().rev();
    let mut carry = 0_u8;
    loop {
        let left = left.next().map(|digit| digit - b'0');
        let right = right.next().map(|digit| digit - b'0');
        if left.is_none() && right.is_none() {
            break;
        }
        let sum = left.unwrap_or(0) + right.unwrap_or(0) + carry;
        output.push(b'0' + sum % 10);
        carry = sum / 10;
    }
    if carry != 0 {
        output.push(b'0' + carry);
    }
    output.reverse();
    output
}

/// `left` 必须大于 `right`。
fn subtract_decimal_magnitudes(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(left.len());
    let mut right = right.iter().rev();
    let mut borrow = 0_i8;
    for left_digit in left.iter().rev() {
        let mut value = i8::try_from(left_digit - b'0').expect("十进制位必须小于 10") - borrow;
        let right_digit = right.next().map_or(0, |digit| {
            i8::try_from(digit - b'0').expect("十进制位必须小于 10")
        });
        if value < right_digit {
            value += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        output.push(b'0' + u8::try_from(value - right_digit).expect("十进制差必须非负"));
    }
    debug_assert_eq!(borrow, 0);
    while output.len() > 1 && output.last() == Some(&b'0') {
        output.pop();
    }
    output.reverse();
    output
}

#[cfg(test)]
fn canonical_json_number(value: &str) -> (bool, String, bool, String) {
    let value = CanonicalJsonNumber::parse(value);
    (
        value.negative,
        value.coefficient,
        value.exponent.negative,
        String::from_utf8(value.exponent.magnitude).expect("指数必须保持 ASCII"),
    )
}

impl fmt::Debug for OpenAiCompatibleClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redactor = ApiKeyRedactor::new(self.api_key.clone());
        let endpoint = redactor.redact_url(self.url.as_str());
        let model = redactor.redact(self.model.as_ref());
        let parameters = redactor
            .redact_json(self.parameters.as_ref())
            .expect("已验证的 LLM 自定义参数必须能够序列化");
        formatter
            .debug_struct("OpenAiCompatibleClient")
            .field("protocol", &self.protocol)
            .field("endpoint", &endpoint)
            .field("model", &model)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("request_timeout", &self.request_timeout)
            .field("rate_limited", &self.rate_limiter.is_some())
            .field("parameters", &parameters)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) enum LlmProxyConfiguration {
    Disabled,
    Explicit(Url),
}

impl fmt::Debug for LlmProxyConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Explicit(url) => formatter.debug_tuple("Explicit").field(url).finish(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct LlmTlsConfiguration {
    additional_pem_roots: Vec<Vec<u8>>,
}

impl fmt::Debug for LlmTlsConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmTlsConfiguration")
            .field(
                "additional_pem_root_count",
                &self.additional_pem_roots.len(),
            )
            .finish()
    }
}

impl LlmTlsConfiguration {
    pub(crate) fn new(additional_pem_roots: Vec<Vec<u8>>) -> Self {
        Self {
            additional_pem_roots,
        }
    }
}

/// 进程内共享 HTTP 连接池与准入边界。
#[derive(Clone, Debug)]
pub(crate) struct OpenAiExecutorConfiguration {
    max_active_requests: NonZeroUsize,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_retry_after: Duration,
    proxy: LlmProxyConfiguration,
    tls: LlmTlsConfiguration,
}

impl OpenAiExecutorConfiguration {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max_active_requests: NonZeroUsize,
        connect_timeout: Duration,
        read_timeout: Duration,
        max_retry_after: Duration,
        proxy: LlmProxyConfiguration,
    ) -> Self {
        Self {
            max_active_requests,
            connect_timeout,
            read_timeout,
            max_retry_after,
            proxy,
            tls: LlmTlsConfiguration::default(),
        }
    }

    /// 注入配置边界已经读取完成的附加根证书。
    pub(crate) fn with_additional_pem_roots(mut self, roots: Vec<Vec<u8>>) -> Self {
        self.tls = LlmTlsConfiguration::new(roots);
        self
    }
}

/// 根构造无法建立安全的共享 HTTP Client。
#[derive(Debug)]
pub(crate) enum OpenAiExecutorBuildError {
    InvalidProxy(reqwest::Error),
    InvalidCertificate(reqwest::Error),
    BuildClient(reqwest::Error),
}

impl OpenAiExecutorBuildError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        Diagnostic::http(match self {
            Self::InvalidProxy(_) => HttpIssue::InvalidProxy,
            Self::InvalidCertificate(_) => HttpIssue::InvalidCertificate,
            Self::BuildClient(_) => HttpIssue::ClientBuild,
        })
    }
}

impl fmt::Display for OpenAiExecutorBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProxy(_) => formatter.write_str("LLM 显式代理配置无效"),
            Self::InvalidCertificate(_) => formatter.write_str("LLM 额外 PEM 根证书无效"),
            Self::BuildClient(_) => formatter.write_str("无法构造 LLM HTTP Client"),
        }
    }
}

impl Error for OpenAiExecutorBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidProxy(source)
            | Self::InvalidCertificate(source)
            | Self::BuildClient(source) => Some(source),
        }
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiCompatibleExecutor {
    client: Client,
    active_capacity: Arc<Semaphore>,
    lifecycle: Arc<LlmLifecycle>,
}

impl OpenAiCompatibleExecutor {
    pub(crate) fn new(
        configuration: OpenAiExecutorConfiguration,
    ) -> Result<Self, OpenAiExecutorBuildError> {
        let mut builder = Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(configuration.connect_timeout)
            .read_timeout(configuration.read_timeout);
        if let LlmProxyConfiguration::Explicit(url) = configuration.proxy {
            let proxy = Proxy::all(url.as_str()).map_err(OpenAiExecutorBuildError::InvalidProxy)?;
            builder = builder.proxy(proxy.no_proxy(None));
        }
        for pem in configuration.tls.additional_pem_roots {
            let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(OpenAiExecutorBuildError::InvalidCertificate)?;
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let client = builder
            .build()
            .map_err(OpenAiExecutorBuildError::BuildClient)?;

        Ok(Self {
            client,
            active_capacity: Arc::new(Semaphore::new(configuration.max_active_requests.get())),
            lifecycle: Arc::new(LlmLifecycle::new(configuration.max_retry_after)),
        })
    }

    /// 停止新请求并立即唤醒正在等待供应商速率或活动许可的请求。
    pub(crate) fn cancel_waits(&self) {
        self.lifecycle.stop_accepting();
    }

    /// 停止新请求并等待已准入请求归还所有许可。
    pub(crate) async fn shutdown(&self) {
        self.cancel_waits();
        self.lifecycle.wait_until_idle().await;
    }

    async fn execute_request(
        &self,
        client: &OpenAiCompatibleClient,
        messages: &[ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<OpenAiCompatibleError>> {
        let request_body = serialize_request(client, messages).map_err(LlmRequestError::Fatal)?;

        let job = self.lifecycle.register()?;

        wait_for_rate(client, &self.lifecycle).await?;
        self.lifecycle.wait_for_retry_gate().await?;
        let active_permit =
            wait_for_active(Arc::clone(&self.active_capacity), &self.lifecycle).await?;
        // 等待活动许可期间可能刚收到新的 Retry-After；发送前必须再次观察共享门控。
        self.lifecycle.wait_for_retry_gate().await?;

        let request = self
            .client
            .post(client.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .timeout(client.request_timeout)
            .bearer_auth(client.api_key.expose_secret())
            .body(request_body);

        let response = match request.send().await {
            Ok(response) => response,
            Err(source) => {
                drop(active_permit);
                drop(job);
                return Err(classify_transport_error(HttpTransportPhase::Send, source));
            }
        };
        let status = response.status();
        let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER));
        if status != StatusCode::OK {
            let redactor = ApiKeyRedactor::new(client.api_key.clone());
            let decision_held =
                if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                    let preliminary = OpenAiCompatibleError::HttpStatus {
                        status: status.as_u16(),
                        provider_code: None,
                        provider_type: None,
                        provider_message: None,
                        response_body_error: None,
                        service_status: LlmServiceStatus::PermanentAuthorization,
                    };
                    let diagnostic = DiagnosticReport::new(
                        StateEffect::ProgressPreserved,
                        preliminary.diagnostic_for_endpoint(
                            safe_http_endpoint_with_redactor(&client.url, &redactor),
                            retry_after,
                        ),
                    );
                    self.lifecycle
                        .stop_for_service(LlmServiceStatus::PermanentAuthorization, diagnostic);
                    false
                } else {
                    // provider code 可能把普通非 2xx 进一步确认为 quota/account 永久错误。
                    // 在完整读取并分类响应前，不允许等待中的 worker 补位发送。
                    self.lifecycle.hold_service_decision();
                    true
                };
            let provider_body = response.bytes().await;
            let (provider_error, response_body_error) = match provider_body {
                Ok(body) => (parse_provider_error(&body).unwrap_or_default(), None),
                Err(source) => (ProviderErrorProjection::default(), Some(source)),
            };
            let service_status = classify_service_status(
                status,
                provider_error.code.as_deref(),
                provider_error.kind.as_deref(),
            );
            if service_status == LlmServiceStatus::RateLimited {
                self.lifecycle.extend_retry_gate(retry_after);
            }
            let provider_code = provider_error.code.map(|value| redactor.redact(&value));
            let provider_type = provider_error.kind.map(|value| redactor.redact(&value));
            let provider_message = provider_error.message.and_then(|value| {
                let value = sanitize_user_text(&redactor.redact(&value));
                (!value.trim().is_empty()).then_some(value)
            });
            let error = OpenAiCompatibleError::HttpStatus {
                status: status.as_u16(),
                provider_code,
                provider_type,
                provider_message,
                response_body_error,
                service_status,
            };
            if service_status.is_permanent() {
                let diagnostic = DiagnosticReport::new(
                    StateEffect::ProgressPreserved,
                    error.diagnostic_for_endpoint(
                        safe_http_endpoint_with_redactor(&client.url, &redactor),
                        retry_after,
                    ),
                );
                self.lifecycle.stop_for_service(service_status, diagnostic);
            } else if service_status == LlmServiceStatus::RateLimited {
                // 保持 header 阶段建立的决定门，直到请求状态机确认重试或耗尽。
                debug_assert!(decision_held);
            } else if decision_held {
                self.lifecycle.resolve_service_decision();
            }
            let result = if is_retryable_status(status) && !service_status.is_permanent() {
                Err(LlmRequestError::Retryable {
                    source: error,
                    retry_after,
                })
            } else {
                Err(LlmRequestError::Fatal(error))
            };
            drop(active_permit);
            drop(job);
            return result;
        }

        let response_body = match response.bytes().await {
            Ok(body) => body,
            Err(source) => {
                drop(active_permit);
                drop(job);
                return Err(classify_transport_error(
                    HttpTransportPhase::ReadSuccessResponse,
                    source,
                ));
            }
        };
        drop(active_permit);
        drop(job);
        parse_success_response(client.protocol, &response_body)
    }
}

impl fmt::Debug for OpenAiCompatibleExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleExecutor")
            .finish_non_exhaustive()
    }
}

impl LlmRequestExecutor for OpenAiCompatibleExecutor {
    type Client = OpenAiCompatibleClient;
    type Error = OpenAiCompatibleError;

    async fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        self.execute_request(client, messages).await
    }

    fn request_diagnostic(
        &self,
        client: &Self::Client,
        source: &Self::Error,
        retry_after: Option<Duration>,
    ) -> Diagnostic {
        let redactor = ApiKeyRedactor::new(client.api_key.clone());
        source.diagnostic_for_endpoint(
            safe_http_endpoint_with_redactor(&client.url, &redactor),
            retry_after,
        )
    }

    fn stop_admission(&self, service_status: LlmServiceStatus, diagnostic: &DiagnosticReport) {
        self.lifecycle
            .stop_for_service(service_status, diagnostic.clone());
    }

    fn continue_after_retryable(&self, service_status: LlmServiceStatus) {
        if service_status == LlmServiceStatus::RateLimited {
            self.lifecycle.resolve_service_decision();
        }
    }
}

#[derive(Debug)]
pub(crate) enum OpenAiCompatibleError {
    WaitCancelled,
    ExecutorClosed,
    SerializeRequest(serde_json::Error),
    Transport {
        phase: HttpTransportPhase,
        source: reqwest::Error,
    },
    HttpStatus {
        status: u16,
        provider_code: Option<String>,
        provider_type: Option<String>,
        provider_message: Option<String>,
        response_body_error: Option<reqwest::Error>,
        service_status: LlmServiceStatus,
    },
    ParseResponse(serde_json::Error),
    InvalidResponseWire {
        violation: HttpEnvelopeViolation,
    },
}

impl fmt::Display for OpenAiCompatibleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WaitCancelled => formatter.write_str("LLM 请求在等待本地许可时被取消"),
            Self::ExecutorClosed => formatter.write_str("LLM 根已关闭"),
            Self::SerializeRequest(_) => formatter.write_str("无法序列化 LLM 请求"),
            Self::Transport { .. } => formatter.write_str("LLM HTTP 传输失败"),
            Self::HttpStatus { status, .. } => write!(formatter, "LLM HTTP 状态 {status}"),
            Self::ParseResponse(_) => formatter.write_str("LLM 成功响应不是有效 JSON"),
            Self::InvalidResponseWire { violation } => {
                write!(formatter, "LLM 成功响应不符合所选协议契约：{violation:?}")
            }
        }
    }
}

impl Error for OpenAiCompatibleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeRequest(source) => Some(source),
            Self::Transport { source, .. } => Some(source),
            Self::HttpStatus {
                response_body_error: Some(source),
                ..
            } => Some(source),
            Self::ParseResponse(source) => Some(source),
            _ => None,
        }
    }
}

impl OpenAiCompatibleError {
    /// 只公开 endpoint 的 scheme/host/port；路径、查询、请求和响应正文不会进入诊断。
    #[cfg(test)]
    pub(crate) fn diagnostic(&self, endpoint: &Url, retry_after: Option<Duration>) -> Diagnostic {
        self.diagnostic_for_endpoint(safe_http_endpoint(endpoint), retry_after)
    }

    fn diagnostic_for_endpoint(
        &self,
        endpoint: HttpEndpoint,
        retry_after: Option<Duration>,
    ) -> Diagnostic {
        let issue = match self {
            Self::WaitCancelled => HttpIssue::WaitCancelled { endpoint },
            Self::ExecutorClosed => HttpIssue::ExecutorClosed { endpoint },
            Self::SerializeRequest(source) => HttpIssue::RequestSerialization {
                endpoint,
                category: http_json_category(source),
                line: source.line(),
                column: source.column(),
            },
            Self::Transport { phase, source } => {
                let io = error_chain_io(source);
                HttpIssue::Transport {
                    endpoint,
                    phase: *phase,
                    transport: typed_transport_kind(source, *phase),
                    io_kind: io.map(|source| source.kind().into()),
                    raw_os_code: io.and_then(std::io::Error::raw_os_error),
                }
            }
            Self::HttpStatus {
                status,
                provider_code,
                provider_type,
                provider_message,
                response_body_error,
                ..
            } => HttpIssue::Status {
                endpoint,
                status: *status,
                retry_after_seconds: retry_after.map(|value| value.as_secs()),
                provider_code: provider_code
                    .as_ref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                provider_type: provider_type
                    .as_ref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                provider_message: provider_message.as_ref().map(SafeText::new),
                response_read_failure: response_body_error.as_ref().map(|source| {
                    let io = error_chain_io(source);
                    HttpResponseReadFailure {
                        phase: HttpTransportPhase::ReadErrorResponse,
                        transport: typed_transport_kind(
                            source,
                            HttpTransportPhase::ReadErrorResponse,
                        ),
                        io_kind: io.map(|source| source.kind().into()),
                        raw_os_code: io.and_then(std::io::Error::raw_os_error),
                    }
                }),
            },
            Self::ParseResponse(source) => HttpIssue::ResponseJson {
                endpoint,
                category: http_json_category(source),
                line: source.line(),
                column: source.column(),
            },
            Self::InvalidResponseWire { violation } => HttpIssue::InvalidEnvelope {
                endpoint,
                violation: *violation,
            },
        };
        Diagnostic::http(issue)
    }
}

impl LlmRequestFailure for OpenAiCompatibleError {
    fn is_cancelled_wait(&self) -> bool {
        matches!(self, Self::WaitCancelled)
    }

    fn service_status(&self) -> LlmServiceStatus {
        match self {
            Self::HttpStatus { service_status, .. } => *service_status,
            _ => LlmServiceStatus::Other,
        }
    }
}

#[derive(Default)]
struct ProviderErrorProjection {
    code: Option<String>,
    kind: Option<String>,
    message: Option<String>,
}

fn parse_provider_error(body: &[u8]) -> Option<ProviderErrorProjection> {
    let root = serde_json::from_slice::<Value>(body).ok()?;
    let error = root.get("error")?.as_object()?;
    Some(ProviderErrorProjection {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .and_then(provider_identifier),
        kind: error
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .and_then(provider_identifier),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn provider_identifier(value: String) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }))
    .then_some(value)
}

#[derive(Serialize)]
struct RequestMessageWire<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatCompletionRequestWire<'a> {
    model: &'a str,
    messages: Vec<RequestMessageWire<'a>>,
    stream: bool,
    #[serde(flatten)]
    parameters: &'a Map<String, Value>,
}

#[derive(Serialize)]
struct ResponsesRequestWire<'a> {
    model: &'a str,
    input: Vec<RequestMessageWire<'a>>,
    stream: bool,
    background: bool,
    #[serde(flatten)]
    parameters: &'a Map<String, Value>,
}

fn serialize_request(
    client: &OpenAiCompatibleClient,
    messages: &[ChatMessage],
) -> Result<Vec<u8>, OpenAiCompatibleError> {
    let messages = messages
        .iter()
        .map(|message| RequestMessageWire {
            role: match message.role() {
                ChatMessageRole::System => "system",
                ChatMessageRole::User => "user",
            },
            content: message.content(),
        })
        .collect::<Vec<_>>();
    match client.protocol {
        OpenAiProtocol::ChatCompletions => serde_json::to_vec(&ChatCompletionRequestWire {
            model: client.model.as_ref(),
            messages,
            stream: false,
            parameters: client.parameters.as_ref(),
        }),
        OpenAiProtocol::Responses => serde_json::to_vec(&ResponsesRequestWire {
            model: client.model.as_ref(),
            input: messages,
            stream: false,
            background: false,
            parameters: client.parameters.as_ref(),
        }),
    }
    .map_err(OpenAiCompatibleError::SerializeRequest)
}

async fn wait_for_rate(
    client: &OpenAiCompatibleClient,
    lifecycle: &LlmLifecycle,
) -> Result<(), LlmRequestError<OpenAiCompatibleError>> {
    if !lifecycle.is_accepting() {
        return Err(lifecycle.stopped_wait_error());
    }
    let Some(rate_limiter) = &client.rate_limiter else {
        return if lifecycle.is_accepting() {
            Ok(())
        } else {
            Err(lifecycle.stopped_wait_error())
        };
    };
    let stopped = lifecycle.wait_for_stop();
    tokio::pin!(stopped);
    let ready = rate_limiter.until_ready();
    tokio::pin!(ready);
    let admitted = tokio::select! {
        biased;
        () = &mut stopped => false,
        () = &mut ready => true,
    };
    if admitted && lifecycle.is_accepting() {
        Ok(())
    } else {
        Err(lifecycle.stopped_wait_error())
    }
}

async fn wait_for_active(
    semaphore: Arc<Semaphore>,
    lifecycle: &LlmLifecycle,
) -> Result<tokio::sync::OwnedSemaphorePermit, LlmRequestError<OpenAiCompatibleError>> {
    if !lifecycle.is_accepting() {
        return Err(lifecycle.stopped_wait_error());
    }
    let stopped = lifecycle.wait_for_stop();
    tokio::pin!(stopped);
    let permit = semaphore.acquire_owned();
    tokio::pin!(permit);
    let permit = tokio::select! {
        biased;
        () = &mut stopped => {
            return Err(lifecycle.stopped_wait_error());
        }
        result = &mut permit => result
            .map_err(|_| LlmRequestError::Fatal(OpenAiCompatibleError::ExecutorClosed)),
    }?;
    if lifecycle.is_accepting() {
        Ok(permit)
    } else {
        drop(permit);
        Err(lifecycle.stopped_wait_error())
    }
}

impl LlmClientConcurrency for OpenAiCompatibleClient {
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.max_concurrent_requests
    }
}

fn retryable(source: OpenAiCompatibleError) -> LlmRequestError<OpenAiCompatibleError> {
    LlmRequestError::Retryable {
        source,
        retry_after: None,
    }
}

fn classify_transport_error(
    phase: HttpTransportPhase,
    source: reqwest::Error,
) -> LlmRequestError<OpenAiCompatibleError> {
    let phase = effective_transport_phase(phase, &source);
    let is_tls = error_chain_contains::<native_tls::Error>(&source);
    let retry = !source.is_builder()
        && !is_tls
        && (source.is_timeout() || source.is_connect() || source.is_request() || source.is_body());
    if retry {
        retryable(OpenAiCompatibleError::Transport { phase, source })
    } else {
        LlmRequestError::Fatal(OpenAiCompatibleError::Transport { phase, source })
    }
}

#[cfg(test)]
fn safe_http_endpoint(endpoint: &Url) -> HttpEndpoint {
    safe_http_endpoint_with_host(
        endpoint,
        endpoint
            .host_str()
            .expect("LLM endpoint 在配置边界已经确认包含 host"),
    )
}

fn safe_http_endpoint_with_redactor(endpoint: &Url, redactor: &ApiKeyRedactor) -> HttpEndpoint {
    let host = endpoint
        .host_str()
        .expect("LLM endpoint 在配置边界已经确认包含 host");
    safe_http_endpoint_with_host(endpoint, &redactor.redact(host))
}

fn safe_http_endpoint_with_host(endpoint: &Url, host: &str) -> HttpEndpoint {
    let scheme = match endpoint.scheme() {
        "http" => crate::diagnostic::HttpScheme::Http,
        "https" => crate::diagnostic::HttpScheme::Https,
        _ => unreachable!("LLM endpoint 在配置边界已经确认使用 HTTP(S)"),
    };
    HttpEndpoint::new(scheme, host, endpoint.port())
}

fn http_json_category(source: &serde_json::Error) -> HttpJsonCategory {
    match source.classify() {
        serde_json::error::Category::Io => HttpJsonCategory::Io,
        serde_json::error::Category::Syntax => HttpJsonCategory::Syntax,
        serde_json::error::Category::Data => HttpJsonCategory::Data,
        serde_json::error::Category::Eof => HttpJsonCategory::Eof,
    }
}

fn error_chain_io<'a>(source: &'a (dyn Error + 'static)) -> Option<&'a std::io::Error> {
    let mut current = Some(source);
    while let Some(error) = current {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        current = error.source();
    }
    None
}

fn typed_transport_kind(source: &reqwest::Error, phase: HttpTransportPhase) -> HttpTransportKind {
    if error_chain_contains::<native_tls::Error>(source) {
        return HttpTransportKind::Tls;
    }
    if source.is_timeout() {
        return HttpTransportKind::Timeout;
    }
    if source.is_connect()
        && error_chain_io(source)
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| matches!(code, 11001..=11004))
    {
        return HttpTransportKind::Dns;
    }
    if source.is_connect() {
        return HttpTransportKind::Connect;
    }
    if source.is_decode() {
        return HttpTransportKind::Decode;
    }
    if source.is_redirect() {
        return HttpTransportKind::Redirect;
    }
    match phase {
        HttpTransportPhase::ReadErrorResponse | HttpTransportPhase::ReadSuccessResponse => {
            HttpTransportKind::Read
        }
        HttpTransportPhase::Connect => HttpTransportKind::Connect,
        HttpTransportPhase::Send => HttpTransportKind::Send,
    }
}

fn effective_transport_phase(
    declared: HttpTransportPhase,
    source: &reqwest::Error,
) -> HttpTransportPhase {
    if source.is_connect() {
        HttpTransportPhase::Connect
    } else {
        declared
    }
}

fn error_chain_contains<T>(source: &(dyn Error + 'static)) -> bool
where
    T: Error + 'static,
{
    let mut current = Some(source);
    while let Some(error) = current {
        if error.is::<T>() {
            return true;
        }
        current = error.source();
    }
    false
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

fn classify_service_status(
    status: StatusCode,
    provider_code: Option<&str>,
    provider_type: Option<&str>,
) -> LlmServiceStatus {
    let identifiers = [provider_code, provider_type];
    if identifiers.into_iter().flatten().any(|identifier| {
        matches!(
            identifier.to_ascii_lowercase().as_str(),
            "insufficient_quota"
                | "quota_exceeded"
                | "billing_hard_limit_reached"
                | "usage_limit_reached"
        )
    }) {
        return LlmServiceStatus::PermanentQuota;
    }
    if identifiers.into_iter().flatten().any(|identifier| {
        matches!(
            identifier.to_ascii_lowercase().as_str(),
            "account_deactivated"
                | "account_disabled"
                | "organization_deactivated"
                | "organization_disabled"
                | "billing_not_active"
        )
    }) {
        return LlmServiceStatus::PermanentAccount;
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return LlmServiceStatus::PermanentAuthorization;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return LlmServiceStatus::RateLimited;
    }
    LlmServiceStatus::Other
}

fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_time = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_time
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn parse_success_response(
    protocol: OpenAiProtocol,
    body: &[u8],
) -> Result<LlmResponse, LlmRequestError<OpenAiCompatibleError>> {
    let wire: Value = serde_json::from_slice(body)
        .map_err(|source| LlmRequestError::Fatal(OpenAiCompatibleError::ParseResponse(source)))?;
    let object = wire
        .as_object()
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::InvalidContract))?;
    match protocol {
        OpenAiProtocol::ChatCompletions => parse_chat_completions_response(object),
        OpenAiProtocol::Responses => parse_responses_response(object),
    }
}

fn parse_chat_completions_response(
    object: &Map<String, Value>,
) -> Result<LlmResponse, LlmRequestError<OpenAiCompatibleError>> {
    let choices = object
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::MissingChoices))?;
    let mut matching_choices = choices.iter().filter(|choice| {
        choice
            .as_object()
            .and_then(|choice| choice.get("index"))
            .and_then(Value::as_u64)
            == Some(0)
    });
    let Some(choice) = matching_choices.next() else {
        return Err(LlmRequestError::Fatal(
            OpenAiCompatibleError::InvalidResponseWire {
                violation: HttpEnvelopeViolation::EmptyChoices,
            },
        ));
    };
    if matching_choices.next().is_some() {
        return Err(LlmRequestError::Fatal(
            OpenAiCompatibleError::InvalidResponseWire {
                violation: HttpEnvelopeViolation::InvalidContract,
            },
        ));
    }
    let choice = choice
        .as_object()
        .expect("index 0 choice 已经确认是 JSON 对象");
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::MissingMessage))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::MissingContent))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::InvalidContract))?;
    let finish_reason = match finish_reason {
        "stop" => LlmFinishReason::Stop,
        "length" => LlmFinishReason::Length,
        "content_filter" => LlmFinishReason::ContentFilter,
        other => LlmFinishReason::Other(other.to_owned()),
    };
    Ok(LlmResponse::new(content, finish_reason))
}

fn parse_responses_response(
    object: &Map<String, Value>,
) -> Result<LlmResponse, LlmRequestError<OpenAiCompatibleError>> {
    let finish_reason = match object.get("status").and_then(Value::as_str) {
        Some("completed") => LlmFinishReason::Stop,
        Some("incomplete") => {
            let reason = object
                .get("incomplete_details")
                .and_then(Value::as_object)
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_response(HttpEnvelopeViolation::InvalidContract))?;
            match reason {
                "max_output_tokens" => LlmFinishReason::Length,
                "content_filter" => LlmFinishReason::ContentFilter,
                other => LlmFinishReason::Other(other.to_owned()),
            }
        }
        _ => return Err(invalid_response(HttpEnvelopeViolation::InvalidContract)),
    };
    let output = object
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response(HttpEnvelopeViolation::MissingOutput))?;
    let mut text = String::new();
    let mut found_output_text = false;
    let mut refusal = String::new();
    let mut found_refusal = false;
    for item in output {
        let Some(message) = item.as_object().filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("assistant")
        }) else {
            continue;
        };
        let content = message
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_response(HttpEnvelopeViolation::MissingOutputText))?;
        for part in content {
            let Some(part) = part.as_object() else {
                continue;
            };
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    let value = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                        invalid_response(HttpEnvelopeViolation::MissingOutputText)
                    })?;
                    text.push_str(value);
                    found_output_text = true;
                }
                Some("refusal") => {
                    let value = part.get("refusal").and_then(Value::as_str).ok_or_else(|| {
                        invalid_response(HttpEnvelopeViolation::MissingOutputText)
                    })?;
                    refusal.push_str(value);
                    found_refusal = true;
                }
                _ => {}
            }
        }
    }
    if found_output_text {
        return Ok(LlmResponse::new(text, finish_reason));
    }
    if found_refusal {
        return Ok(LlmResponse::new(refusal, LlmFinishReason::ContentFilter));
    }
    if matches!(&finish_reason, LlmFinishReason::Stop) {
        return Err(invalid_response(HttpEnvelopeViolation::MissingOutputText));
    }
    Ok(LlmResponse::new(String::new(), finish_reason))
}

fn invalid_response(violation: HttpEnvelopeViolation) -> LlmRequestError<OpenAiCompatibleError> {
    LlmRequestError::Fatal(OpenAiCompatibleError::InvalidResponseWire { violation })
}

struct LlmLifecycle {
    accepting: AtomicBool,
    state: Mutex<LlmLifecycleState>,
    stopping: watch::Sender<bool>,
    jobs: watch::Sender<usize>,
    retry_gate: watch::Sender<Option<tokio::time::Instant>>,
    service_decisions: watch::Sender<usize>,
    max_retry_after: Duration,
}

struct LlmLifecycleState {
    jobs: usize,
    service_stop: Option<LlmServiceAdmissionStop>,
    pending_service_decisions: usize,
}

#[derive(Clone)]
struct LlmServiceAdmissionStop {
    service_status: LlmServiceStatus,
    diagnostic: DiagnosticReport,
}

impl LlmLifecycle {
    fn new(max_retry_after: Duration) -> Self {
        let (stopping, _) = watch::channel(false);
        let (jobs, _) = watch::channel(0);
        let (retry_gate, _) = watch::channel(None);
        let (service_decisions, _) = watch::channel(0);
        Self {
            accepting: AtomicBool::new(true),
            state: Mutex::new(LlmLifecycleState {
                jobs: 0,
                service_stop: None,
                pending_service_decisions: 0,
            }),
            stopping,
            jobs,
            retry_gate,
            service_decisions,
            max_retry_after,
        }
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    fn register(self: &Arc<Self>) -> Result<LlmJobGuard, LlmRequestError<OpenAiCompatibleError>> {
        let mut state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        if !self.is_accepting() {
            return Err(state.service_stop.as_ref().map_or_else(
                || LlmRequestError::Fatal(OpenAiCompatibleError::ExecutorClosed),
                |stopped| LlmRequestError::AdmissionStopped {
                    service_status: stopped.service_status,
                    diagnostic: stopped.diagnostic.clone(),
                },
            ));
        }
        state.jobs += 1;
        self.jobs.send_replace(state.jobs);
        Ok(LlmJobGuard {
            lifecycle: Arc::clone(self),
        })
    }

    fn stop_accepting(&self) {
        let _state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        self.accepting.store(false, Ordering::Release);
        self.stopping.send_replace(true);
    }

    fn stop_for_service(&self, service_status: LlmServiceStatus, diagnostic: DiagnosticReport) {
        debug_assert!(
            service_status.is_permanent() || service_status.stops_admission_after_unavailable(),
            "只有明确要求停发的服务状态才能关闭请求准入"
        );
        let mut state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        let should_replace = state.service_stop.as_ref().is_none_or(|current| {
            service_status.is_permanent() && !current.service_status.is_permanent()
        });
        if should_replace {
            state.service_stop = Some(LlmServiceAdmissionStop {
                service_status,
                diagnostic,
            });
        }
        self.accepting.store(false, Ordering::Release);
        self.stopping.send_replace(true);
    }

    fn stopped_wait_error(&self) -> LlmRequestError<OpenAiCompatibleError> {
        let state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        state.service_stop.as_ref().map_or_else(
            || LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled),
            |stopped| LlmRequestError::AdmissionStopped {
                service_status: stopped.service_status,
                diagnostic: stopped.diagnostic.clone(),
            },
        )
    }

    fn hold_service_decision(&self) {
        let mut state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        state.pending_service_decisions = state.pending_service_decisions.saturating_add(1);
        self.service_decisions
            .send_replace(state.pending_service_decisions);
    }

    fn resolve_service_decision(&self) {
        let mut state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        state.pending_service_decisions = state
            .pending_service_decisions
            .checked_sub(1)
            .expect("每个非成功响应必须且只能完成一次准入决定");
        self.service_decisions
            .send_replace(state.pending_service_decisions);
    }

    /// 只把配置允许等待的普通 429 Retry-After 分享给同一执行器的其他请求。
    /// 超过上限的响应由请求状态机立即结束并触发业务停发，不能把在途任务挂到超长等待。
    fn extend_retry_gate(&self, retry_after: Option<Duration>) {
        let Some(retry_after) = retry_after.filter(|value| *value <= self.max_retry_after) else {
            return;
        };
        let deadline = tokio::time::Instant::now() + retry_after;
        self.retry_gate.send_if_modified(|current| {
            if current.is_none_or(|current| current < deadline) {
                *current = Some(deadline);
                true
            } else {
                false
            }
        });
    }

    async fn wait_for_retry_gate(&self) -> Result<(), LlmRequestError<OpenAiCompatibleError>> {
        let mut retry_gate = self.retry_gate.subscribe();
        let mut service_decisions = self.service_decisions.subscribe();
        let mut stopping = self.stopping.subscribe();
        loop {
            if *stopping.borrow_and_update() {
                return Err(self.stopped_wait_error());
            }
            if *service_decisions.borrow_and_update() != 0 {
                tokio::select! {
                    biased;
                    changed = stopping.changed() => {
                        if changed.is_err() || *stopping.borrow_and_update() {
                            return Err(self.stopped_wait_error());
                        }
                    }
                    changed = service_decisions.changed() => {
                        if changed.is_err() {
                            return Err(LlmRequestError::Fatal(
                                OpenAiCompatibleError::ExecutorClosed,
                            ));
                        }
                    }
                }
                continue;
            }
            let Some(deadline) = *retry_gate.borrow_and_update() else {
                return Ok(());
            };
            if deadline <= tokio::time::Instant::now() {
                return Ok(());
            }
            tokio::select! {
                biased;
                changed = stopping.changed() => {
                    if changed.is_err() || *stopping.borrow_and_update() {
                        return Err(self.stopped_wait_error());
                    }
                }
                changed = retry_gate.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep_until(deadline) => return Ok(()),
            }
        }
    }

    async fn wait_until_idle(&self) {
        let mut jobs = self.jobs.subscribe();
        loop {
            if *jobs.borrow_and_update() == 0 {
                return;
            }
            if jobs.changed().await.is_err() {
                return;
            }
        }
    }

    async fn wait_for_stop(&self) {
        let mut stopping = self.stopping.subscribe();
        loop {
            if *stopping.borrow_and_update() {
                return;
            }
            if stopping.changed().await.is_err() {
                return;
            }
        }
    }
}

struct LlmJobGuard {
    lifecycle: Arc<LlmLifecycle>,
}

impl Drop for LlmJobGuard {
    fn drop(&mut self) {
        let mut state = self.lifecycle.state.lock().expect("LLM 生命周期锁不应中毒");
        state.jobs = state.jobs.checked_sub(1).expect("LLM 作业计数不得下溢");
        self.lifecycle.jobs.send_replace(state.jobs);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use crate::execution::CooperativeCancellation;
    use crate::execution::llm_request::{
        AsyncDelay, LlmRequestExecutionOutcome, LlmRequestRetryPolicy,
        execute_llm_request_with_retry,
    };

    use super::*;

    #[derive(Clone, Copy)]
    struct ImmediateDelay;

    impl AsyncDelay for ImmediateDelay {
        async fn wait(&self, _duration: Duration) {}
    }

    #[test]
    fn http_status_leaf_uses_safe_endpoint_and_typed_provider_fields() {
        let source = OpenAiCompatibleError::HttpStatus {
            status: 503,
            provider_code: Some("server_busy".to_owned()),
            provider_type: Some("temporary".to_owned()),
            provider_message: Some("try_later".to_owned()),
            response_body_error: None,
            service_status: LlmServiceStatus::Other,
        };
        let endpoint =
            Url::parse("https://api.example.test:8443/v1/chat/completions?api_key=must-not-leak")
                .expect("测试 URL 有效");
        assert_eq!(
            serde_json::to_value(source.diagnostic(&endpoint, Some(Duration::from_secs(7))))
                .expect("诊断应可序列化"),
            serde_json::json!({
                "code": "http.status",
                "stage": "model_request",
                "issue": {
                    "family": "http",
                    "details": {
                        "kind": "status",
                        "endpoint": {
                            "scheme": "https",
                            "host": "api.example.test",
                            "port": 8443
                        },
                        "status": 503,
                        "retry_after_seconds": 7,
                        "provider_code": "server_busy",
                        "provider_type": "temporary",
                        "provider_message": "try_later",
                        "response_read_failure": null
                    }
                },
                "resolution": "check_model_service"
            })
        );
    }

    #[test]
    fn executor_request_diagnostic_redacts_selected_api_key_from_host() {
        let client = OpenAiCompatibleClient::new(
            Url::parse("https://test-secret.example.test/v1/chat/completions")
                .expect("测试 URL 有效"),
            SecretString::from("test-secret"),
            "test-model",
            NonZeroUsize::new(1).expect("测试并发非零"),
            Duration::from_secs(2),
            None,
            Map::new(),
        );
        let executor = executor(1);
        let diagnostic = LlmRequestExecutor::request_diagnostic(
            &executor,
            &client,
            &OpenAiCompatibleError::WaitCancelled,
            None,
        );
        let wire = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!wire.contains("test-secret"));
        assert!(wire.contains("[REDACTED API KEY]"));
    }

    fn non_zero_usize(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试值必须非零")
    }

    fn non_zero_u32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("测试值必须非零")
    }

    fn parse_success_response(
        body: &[u8],
    ) -> Result<LlmResponse, LlmRequestError<OpenAiCompatibleError>> {
        super::parse_success_response(OpenAiProtocol::ChatCompletions, body)
    }

    fn client(url: &str, parameters: Map<String, Value>) -> OpenAiCompatibleClient {
        client_with_rate(url, parameters, 60, 2)
    }

    fn client_with_protocol(
        url: &str,
        protocol: OpenAiProtocol,
        parameters: Map<String, Value>,
    ) -> OpenAiCompatibleClient {
        OpenAiCompatibleClient::new_with_endpoint(
            OpenAiEndpoint::new(Url::parse(url).expect("测试 URL 有效"), protocol),
            SecretString::from("test-secret"),
            "test-model",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            parameters,
        )
    }

    fn client_with_rate(
        url: &str,
        parameters: Map<String, Value>,
        rpm: u32,
        burst: u32,
    ) -> OpenAiCompatibleClient {
        OpenAiCompatibleClient::new(
            Url::parse(url).expect("测试 URL 有效"),
            SecretString::from("test-secret"),
            "test-model",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(rpm), non_zero_u32(burst))),
            parameters,
        )
    }

    fn executor(max_active_requests: usize) -> OpenAiCompatibleExecutor {
        OpenAiCompatibleExecutor::new(OpenAiExecutorConfiguration::new(
            non_zero_usize(max_active_requests),
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(60),
            LlmProxyConfiguration::Disabled,
        ))
        .expect("测试 LLM 根应构造成功")
    }

    struct TestServer {
        endpoint: String,
        requests: mpsc::Receiver<Vec<u8>>,
        first_request_seen: mpsc::Receiver<()>,
        release_first: Option<mpsc::Sender<()>>,
        worker: thread::JoinHandle<()>,
    }

    struct ConcurrentDecisionServer {
        endpoint: String,
        initial_requests_seen: mpsc::Receiver<()>,
        release_decision: mpsc::Sender<()>,
        extra_request_seen: mpsc::Receiver<bool>,
        worker: thread::JoinHandle<()>,
    }

    struct SlowBodyDecisionServer {
        endpoint: String,
        initial_requests_seen: mpsc::Receiver<()>,
        release_responses: mpsc::Sender<()>,
        extra_before_body: mpsc::Receiver<bool>,
        extra_after_body: mpsc::Receiver<bool>,
        worker: thread::JoinHandle<()>,
    }

    fn spawn_slow_quota_body_server() -> SlowBodyDecisionServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应成功");
        let address = listener.local_addr().expect("测试监听地址应可读");
        let (seen_sender, initial_requests_seen) = mpsc::channel();
        let (release_responses, release_receiver) = mpsc::channel();
        let (before_sender, extra_before_body) = mpsc::channel();
        let (after_sender, extra_after_body) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("llm-slow-quota-body-server".to_owned())
            .spawn(move || {
                let mut slow = None;
                let mut fast = None;
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().expect("初始活动窗口请求应可接受");
                    let request = read_http_request(&mut stream);
                    let body = String::from_utf8_lossy(request_body(&request));
                    if body.contains("slow-quota-body") {
                        slow = Some(stream);
                    } else if body.contains("fast-success") {
                        fast = Some(stream);
                    } else {
                        panic!("初始活动窗口收到未识别的测试请求：{body}");
                    }
                }
                seen_sender.send(()).expect("初始活动窗口应可通知");
                release_receiver.recv().expect("测试响应应被释放");

                let quota_body = r#"{"error":{"code":"insufficient_quota","type":"billing","message":"SECRET_BODY"}}"#;
                let mut slow = slow.expect("应收到慢 quota 请求");
                let headers = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    quota_body.len()
                );
                slow.write_all(headers.as_bytes()).expect("慢响应头应可写入");
                slow.flush().expect("慢响应头应可刷新");

                let mut fast = fast.expect("应收到快成功请求");
                fast.write_all(&success_response("fast", "fast", "[]"))
                    .expect("快成功响应应可写入");
                fast.flush().expect("快成功响应应可刷新");
                drop(fast);

                listener.set_nonblocking(true).expect("监听应切换为非阻塞");
                thread::sleep(Duration::from_millis(100));
                let extra = match listener.accept() {
                    Ok((_stream, _)) => true,
                    Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(source) => panic!("检查慢正文前额外请求失败：{source}"),
                };
                before_sender.send(extra).expect("慢正文前事实应可返回");

                slow.write_all(quota_body.as_bytes()).expect("quota 正文应可写入");
                slow.flush().expect("quota 正文应可刷新");
                drop(slow);
                thread::sleep(Duration::from_millis(100));
                let extra = match listener.accept() {
                    Ok((_stream, _)) => true,
                    Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(source) => panic!("检查慢正文后额外请求失败：{source}"),
                };
                after_sender.send(extra).expect("慢正文后事实应可返回");
            })
            .expect("慢 quota 正文测试服务器应创建成功");
        SlowBodyDecisionServer {
            endpoint: format!("http://{address}/v1/chat/completions"),
            initial_requests_seen,
            release_responses,
            extra_before_body,
            extra_after_body,
            worker,
        }
    }

    fn spawn_concurrent_decision_server(
        slow_marker: &'static str,
        decision_marker: &'static str,
        decision_response: Vec<u8>,
        slow_response: Vec<u8>,
    ) -> ConcurrentDecisionServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应成功");
        let address = listener.local_addr().expect("测试监听地址应可读");
        let (seen_sender, initial_requests_seen) = mpsc::channel();
        let (release_decision, release_receiver) = mpsc::channel();
        let (extra_sender, extra_request_seen) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("llm-concurrent-decision-server".to_owned())
            .spawn(move || {
                let mut initial = Vec::with_capacity(2);
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().expect("初始活动窗口请求应可接受");
                    let request = read_http_request(&mut stream);
                    initial.push((stream, request));
                }
                seen_sender.send(()).expect("初始活动窗口应可通知");
                release_receiver.recv().expect("服务决定响应应被释放");

                let mut slow = None;
                let mut decision = None;
                for (stream, request) in initial {
                    let body = String::from_utf8_lossy(request_body(&request));
                    if body.contains(slow_marker) {
                        slow = Some(stream);
                    } else if body.contains(decision_marker) {
                        decision = Some(stream);
                    } else {
                        panic!("初始活动窗口收到未识别的测试请求：{body}");
                    }
                }
                let mut decision = decision.expect("应收到服务决定请求");
                decision
                    .write_all(&decision_response)
                    .expect("服务决定响应应可写入");
                decision.flush().expect("服务决定响应应可刷新");
                drop(decision);

                thread::sleep(Duration::from_millis(50));
                let mut slow = slow.expect("应收到慢成功请求");
                slow.write_all(&slow_response).expect("慢成功响应应可写入");
                slow.flush().expect("慢成功响应应可刷新");
                drop(slow);

                listener
                    .set_nonblocking(true)
                    .expect("测试监听应切换为非阻塞");
                let deadline = std::time::Instant::now() + Duration::from_millis(150);
                let mut extra = false;
                while std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((_stream, _)) => {
                            extra = true;
                            break;
                        }
                        Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(source) => panic!("检查额外请求失败：{source}"),
                    }
                }
                extra_sender.send(extra).expect("额外请求事实应可返回");
            })
            .expect("并发决定测试服务器应创建成功");
        ConcurrentDecisionServer {
            endpoint: format!("http://{address}/v1/chat/completions"),
            initial_requests_seen,
            release_decision,
            extra_request_seen,
            worker,
        }
    }

    fn spawn_test_server(responses: Vec<Vec<u8>>, gate_first: bool) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应成功");
        let address = listener.local_addr().expect("测试监听地址应可读");
        let (request_sender, requests) = mpsc::channel();
        let (seen_sender, first_request_seen) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("llm-test-server".to_owned())
            .spawn(move || {
                for (index, response) in responses.into_iter().enumerate() {
                    let (mut stream, _) = listener.accept().expect("测试请求应可接受");
                    let request = read_http_request(&mut stream);
                    request_sender.send(request).expect("测试请求应可记录");
                    if index == 0 {
                        seen_sender.send(()).expect("首个请求应可通知");
                        if gate_first {
                            release_receiver.recv().expect("首个响应应被释放");
                        }
                    }
                    stream.write_all(&response).expect("测试响应应可写入");
                    stream.flush().expect("测试响应应可刷新");
                }
            })
            .expect("测试服务器线程应创建成功");
        TestServer {
            endpoint: format!("http://{address}/v1/chat/completions"),
            requests,
            first_request_seen,
            release_first: gate_first.then_some(release_sender),
            worker,
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("测试读取超时应可设置");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("测试请求应可读取");
            assert!(read > 0, "测试请求不得在头部结束前关闭");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(offset) = find_subslice(&bytes, b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("HTTP 头应为 UTF-8");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("Content-Length 应有效")
                })
            })
            .expect("测试请求必须包含 Content-Length");
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("测试正文应可读取");
            assert!(read > 0, "测试请求正文不得提前关闭");
            bytes.extend_from_slice(&buffer[..read]);
        }
        bytes
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn success_response(response_id: &str, request_id: &str, content: &str) -> Vec<u8> {
        let body = serde_json::json!({
            "id": response_id,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 4,
                "completion_tokens": 2,
                "total_tokens": 6
            }
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-request-id: {request_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn responses_success_response(content: &str) -> Vec<u8> {
        let body = serde_json::json!({
            "id": "response-e2e",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": content }]
            }]
        })
        .to_string();
        status_response("200 OK", "Content-Type: application/json\r\n", &body)
    }

    fn status_response(status: &str, headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn core_success_body() -> &'static str {
        r#"{"choices":[{"index":0,"message":{"content":"[]"},"finish_reason":"stop"}]}"#
    }

    fn response_with_invalid_request_id() -> Vec<u8> {
        let body = core_success_body();
        let mut response = b"HTTP/1.1 200 OK\r\nx-request-id: ".to_vec();
        response.push(0xff);
        response.extend_from_slice(
            format!(
                "\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        response
    }

    fn request_body(request: &[u8]) -> &[u8] {
        let header_end = find_subslice(request, b"\r\n\r\n").expect("请求应包含头部终止符");
        &request[header_end + 4..]
    }

    fn request_headers(request: &[u8]) -> String {
        let header_end = find_subslice(request, b"\r\n\r\n").expect("请求应包含头部终止符");
        String::from_utf8(request[..header_end].to_vec()).expect("测试请求头应为 UTF-8")
    }

    #[test]
    fn debug_shows_ordinary_parameters_and_replaces_api_key() {
        let mut parameters = Map::new();
        parameters.insert(
            "vendor_option".to_owned(),
            Value::String("ordinary-value/api-secret".to_owned()),
        );
        let client = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").expect("测试 URL 有效"),
            SecretString::from("api-secret"),
            "test-model",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            parameters,
        );
        let debug = format!("{client:?}");
        assert!(debug.contains("vendor_option"));
        assert!(debug.contains("ordinary-value"));
        assert!(debug.contains("[REDACTED API KEY]"));
        assert!(!debug.contains("api-secret"));
    }

    #[test]
    fn semantic_identity_includes_protocol_url_model_and_parameters() {
        let mut extra = Map::new();
        extra.insert("temperature".to_owned(), serde_json::json!(0.2));
        extra.insert(
            "vendor".to_owned(),
            serde_json::json!({"mode": "quality", "nested": {"b": 2, "a": 1}}),
        );
        let first = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-a",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            extra.clone(),
        );
        let operationally_different = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("different-secret"),
            "model-a",
            non_zero_usize(1),
            Duration::from_secs(90),
            Some((non_zero_u32(1), non_zero_u32(1))),
            extra.clone(),
        );
        let mut nested_reordered = Map::new();
        nested_reordered.insert("a".to_owned(), serde_json::json!(1));
        nested_reordered.insert("b".to_owned(), serde_json::json!(2));
        let mut vendor_reordered = Map::new();
        vendor_reordered.insert("nested".to_owned(), Value::Object(nested_reordered));
        vendor_reordered.insert("mode".to_owned(), serde_json::json!("quality"));
        let mut extra_reordered = Map::new();
        extra_reordered.insert("vendor".to_owned(), Value::Object(vendor_reordered));
        extra_reordered.insert("temperature".to_owned(), serde_json::json!(0.2));
        let textually_reordered = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-a",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            extra_reordered.clone(),
        );
        let mut numerically_equivalent_extra = extra_reordered;
        numerically_equivalent_extra.insert(
            "temperature".to_owned(),
            serde_json::from_str("2e-1").expect("测试任意精度数字应有效"),
        );
        let numerically_equivalent = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-a",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            numerically_equivalent_extra,
        );
        let different_model = OpenAiCompatibleClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-b",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            extra,
        );
        let responses = client_with_protocol(
            "https://example.com/v1",
            OpenAiProtocol::Responses,
            first.parameters.as_ref().clone(),
        );

        assert_eq!(
            first.semantic_fingerprint(),
            operationally_different.semantic_fingerprint(),
            "凭据和运行资源参数不应使已接受译文失效"
        );
        assert_eq!(
            first.semantic_fingerprint(),
            textually_reordered.semantic_fingerprint(),
            "对象键书写顺序不属于扩展正文的值语义"
        );
        assert_eq!(
            first.semantic_fingerprint(),
            numerically_equivalent.semantic_fingerprint(),
            "JSON 数字的等价十进制写法不应使已接受译文失效"
        );
        assert_ne!(
            first.semantic_fingerprint(),
            different_model.semantic_fingerprint(),
            "模型是译文语义的一部分"
        );
        assert_ne!(
            first.semantic_fingerprint(),
            responses.semantic_fingerprint(),
            "请求协议是译文语义的一部分"
        );
    }

    #[test]
    fn arbitrary_precision_json_number_semantics_are_exact_and_not_textual() {
        let expected = canonical_json_number("0.2");
        for equivalent in ["2e-1", "0.20", "20e-2", "200e-3"] {
            assert_eq!(canonical_json_number(equivalent), expected);
        }
        assert_eq!(
            canonical_json_number("-0.000e999999999999"),
            canonical_json_number("0")
        );
        assert_eq!(
            canonical_json_number("1e1000000000000000000000000"),
            canonical_json_number("10e999999999999999999999999"),
            "任意长度指数的进位必须保持精确"
        );
        assert_ne!(
            canonical_json_number("1234567890123456789012345678901234567890"),
            canonical_json_number("1234567890123456789012345678901234567891"),
            "相邻任意精度整数不能碰撞"
        );
    }

    #[test]
    fn protocol_completes_base_or_full_endpoint_without_losing_prefix_or_query() {
        for (protocol, source, expected) in [
            (
                OpenAiProtocol::ChatCompletions,
                "https://example.com/provider/v1",
                "https://example.com/provider/v1/chat/completions",
            ),
            (
                OpenAiProtocol::ChatCompletions,
                "https://example.com/provider/v1/chat/completions/",
                "https://example.com/provider/v1/chat/completions",
            ),
            (
                OpenAiProtocol::Responses,
                "https://example.com/provider/v1//",
                "https://example.com/provider/v1/responses",
            ),
            (
                OpenAiProtocol::Responses,
                "https://example.com/provider/v1/chat/completions?api-version=current",
                "https://example.com/provider/v1/responses?api-version=current",
            ),
            (
                OpenAiProtocol::ChatCompletions,
                "https://example.com/provider/v1/responses",
                "https://example.com/provider/v1/chat/completions",
            ),
        ] {
            assert_eq!(
                OpenAiEndpoint::new(Url::parse(source).expect("测试 URL 有效"), protocol)
                    .url
                    .as_str(),
                expected
            );
        }
    }

    #[test]
    fn request_wire_without_parameters_has_exactly_three_top_level_fields() {
        let client = client("https://example.com/v1/chat/completions", Map::new());
        let bytes = serialize_request(
            &client,
            &[ChatMessage::new(ChatMessageRole::User, "待翻译内容")],
        )
        .expect("请求应可序列化");
        let wire: Value = serde_json::from_slice(&bytes).expect("请求应为 JSON");
        let object = wire.as_object().expect("请求顶层应为对象");

        assert_eq!(object.len(), 3);
        assert_eq!(object["model"], "test-model");
        assert_eq!(object["stream"], false);
        assert_eq!(object["messages"][0]["role"], "user");
        assert_eq!(object["messages"][0]["content"], "待翻译内容");
    }

    #[test]
    fn responses_request_uses_input_and_preserves_message_roles() {
        let client = client_with_protocol(
            "https://example.com/v1",
            OpenAiProtocol::Responses,
            Map::new(),
        );
        let bytes = serialize_request(
            &client,
            &[
                ChatMessage::new(ChatMessageRole::System, "翻译规则"),
                ChatMessage::new(ChatMessageRole::User, "待翻译内容"),
            ],
        )
        .expect("Responses 请求应可序列化");
        let wire: Value = serde_json::from_slice(&bytes).expect("请求应为 JSON");
        let object = wire.as_object().expect("请求顶层应为对象");

        assert_eq!(object.len(), 4);
        assert_eq!(object["model"], "test-model");
        assert_eq!(object["stream"], false);
        assert_eq!(object["background"], false);
        assert!(!object.contains_key("messages"));
        assert_eq!(object["input"][0]["role"], "system");
        assert_eq!(object["input"][0]["content"], "翻译规则");
        assert_eq!(object["input"][1]["role"], "user");
        assert_eq!(object["input"][1]["content"], "待翻译内容");
    }

    #[test]
    fn request_wire_preserves_every_user_supplied_parameter() {
        let parameters = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "n": 2,
            "max_tokens": 32,
            "max_completion_tokens": 64,
            "temperature": 0.2,
            "provider": { "thinking": true }
        }))
        .expect("测试扩展正文应为对象");
        let client = client("https://example.com/v1/chat/completions", parameters);
        let bytes = serialize_request(&client, &[]).expect("请求应可序列化");
        let wire: Value = serde_json::from_slice(&bytes).expect("请求应为 JSON");

        assert_eq!(wire["n"], 2);
        assert_eq!(wire["max_tokens"], 32);
        assert_eq!(wire["max_completion_tokens"], 64);
        assert_eq!(wire["temperature"], 0.2);
        assert_eq!(wire["provider"]["thinking"], true);
    }

    #[test]
    fn successful_wire_ignores_optional_provider_metadata() {
        let response = parse_success_response(
            br#"{
                "id":"chatcmpl-response",
                "choices":[{
                    "index":0,
                    "message":{"role":"assistant","content":"[]"},
                    "finish_reason":"stop",
                    "provider_extension":true
                }],
                "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5},
                "provider_extension":true
            }"#,
        )
        .expect("合法响应应通过");

        assert_eq!(response.content(), "[]");
        assert_eq!(response.finish_reason(), &LlmFinishReason::Stop);
    }

    #[test]
    fn optional_response_metadata_never_invalidates_core_response() {
        for metadata in [
            "",
            r#", "id": null, "usage": null"#,
            r#", "id": 7, "usage": true"#,
            r#", "id": "", "usage": {"prompt_tokens":1,"completion_tokens":2}"#,
            r#", "id": "response", "usage": {"prompt_tokens":1,"completion_tokens":2,"total_tokens":3.5}"#,
        ] {
            let body = format!(
                r#"{{"choices":[{{"index":0,"message":{{"content":"[]"}},"finish_reason":"stop"}}]{metadata}}}"#
            );
            let response =
                parse_success_response(body.as_bytes()).expect("可选元数据异常不应否定核心响应");
            assert_eq!(response.content(), "[]");
        }
    }

    #[test]
    fn successful_wire_selects_unique_index_zero_and_ignores_other_choices_and_role() {
        let response = parse_success_response(
            br#"{
                "choices":[
                    null,
                    {"index":1,"message":false},
                    {"index":"0","message":{"content":[]}},
                    {"index":0,"message":{"role":"user","content":"selected"},"finish_reason":"length"},
                    {"index":2,"finish_reason":null}
                ]
            }"#,
        )
        .expect("只应验收唯一 index 0 choice 的必要字段");

        assert_eq!(response.content(), "selected");
        assert_eq!(response.finish_reason(), &LlmFinishReason::Length);
    }

    #[test]
    fn successful_wire_rejects_missing_or_duplicate_index_zero_and_invalid_core_fields() {
        for body in [
            br#"[]"#.as_slice(),
            br#"{}"#.as_slice(),
            br#"{"choices":null}"#.as_slice(),
            br#"{"choices":[]}"#.as_slice(),
            br#"{"choices":[{"index":1}]}"#.as_slice(),
            br#"{"choices":[{"index":"0"}]}"#.as_slice(),
            br#"{"choices":[{"index":0,"message":{"content":"[]"},"finish_reason":"stop"},{"index":0,"message":{"content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"choices":[{"index":0,"message":null,"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"choices":[{"index":0,"message":{"content":[]},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"choices":[{"index":0,"message":{"content":"[]"},"finish_reason":null}]}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_success_response(body),
                Err(LlmRequestError::Fatal(
                    OpenAiCompatibleError::InvalidResponseWire { .. }
                ))
            ));
        }
    }

    #[test]
    fn responses_wire_collects_assistant_output_text_and_maps_completion_status() {
        let response = super::parse_success_response(
            OpenAiProtocol::Responses,
            r#"{
                "status":"completed",
                "output":[
                    {"type":"reasoning","summary":[]},
                    {"type":"message","role":"assistant","content":[
                        {"type":"output_text","text":"{\"0\":"},
                        {"type":"refusal","refusal":"ignored"},
                        {"type":"output_text","text":"[\"译文\"]}"}
                    ]}
                ],
                "usage":{"input_tokens":3,"output_tokens":2}
            }"#
            .as_bytes(),
        )
        .expect("合法 Responses 信封应通过");

        assert_eq!(response.content(), r#"{"0":["译文"]}"#);
        assert_eq!(response.finish_reason(), &LlmFinishReason::Stop);
    }

    #[test]
    fn responses_wire_maps_incomplete_reasons() {
        for (reason, expected) in [
            ("max_output_tokens", LlmFinishReason::Length),
            ("content_filter", LlmFinishReason::ContentFilter),
            (
                "provider_limit",
                LlmFinishReason::Other("provider_limit".to_owned()),
            ),
        ] {
            let body = format!(
                r#"{{"status":"incomplete","incomplete_details":{{"reason":"{reason}"}},"output":[{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"partial"}}]}}]}}"#
            );
            let response =
                super::parse_success_response(OpenAiProtocol::Responses, body.as_bytes())
                    .expect("带部分正文的 Responses incomplete 应建立统一响应");
            assert_eq!(response.content(), "partial");
            assert_eq!(response.finish_reason(), &expected);
        }
    }

    #[test]
    fn responses_wire_preserves_incomplete_without_output_text() {
        let response = super::parse_success_response(
            OpenAiProtocol::Responses,
            br#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"reasoning","summary":[]}]}"#,
        )
        .expect("没有正文的 incomplete Responses 仍应保留结束原因");

        assert_eq!(response.content(), "");
        assert_eq!(response.finish_reason(), &LlmFinishReason::Length);
    }

    #[test]
    fn responses_wire_preserves_completed_refusal_as_content_filter() {
        let response = super::parse_success_response(
            OpenAiProtocol::Responses,
            r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"不能处理该请求"}]}]}"#
                .as_bytes(),
        )
        .expect("Responses refusal 应建立统一的内容过滤响应");

        assert_eq!(response.content(), "不能处理该请求");
        assert_eq!(response.finish_reason(), &LlmFinishReason::ContentFilter);
    }

    #[test]
    fn responses_wire_rejects_invalid_status_output_or_text() {
        for body in [
            br#"[]"#.as_slice(),
            br#"{}"#.as_slice(),
            br#"{"status":"completed"}"#.as_slice(),
            br#"{"status":"queued","output":[]}"#.as_slice(),
            br#"{"status":"incomplete","incomplete_details":null,"output":[]}"#.as_slice(),
            br#"{"status":"completed","output":[]}"#.as_slice(),
            br#"{"status":"completed","output":[{"type":"message","role":"assistant","content":null}]}"#.as_slice(),
            br#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":null}]}]}"#.as_slice(),
            br#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":null}]}]}"#.as_slice(),
        ] {
            assert!(matches!(
                super::parse_success_response(OpenAiProtocol::Responses, body),
                Err(LlmRequestError::Fatal(
                    OpenAiCompatibleError::InvalidResponseWire { .. }
                ))
            ));
        }
    }

    #[test]
    fn retryable_http_status_set_is_exact() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(
                StatusCode::from_u16(status).expect("测试状态码有效")
            ));
        }
        for status in [200, 400, 401, 403, 404, 409, 422, 501, 505] {
            assert!(!is_retryable_status(
                StatusCode::from_u16(status).expect("测试状态码有效")
            ));
        }
    }

    #[test]
    fn provider_error_projection_reads_each_standard_field_independently() {
        let projection = parse_provider_error(
            br#"{"error":{"code":"rate_limit_exceeded","type":"requests/rate-limit","message":"MODEL_BODY_SENTINEL"}}"#,
        )
        .expect("供应商错误信封应可解析");
        assert_eq!(projection.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(projection.kind.as_deref(), Some("requests/rate-limit"));
        assert_eq!(projection.message.as_deref(), Some("MODEL_BODY_SENTINEL"));

        let projection = parse_provider_error(
            br#"{"error":{"code":429,"type":"MODEL_BODY_SENTINEL!","message":"independent message"}}"#,
        )
        .expect("一个字段类型错误不应抹掉其他标准字段");
        assert_eq!(projection.code, None);
        assert_eq!(projection.kind, None);
        assert_eq!(projection.message.as_deref(), Some("independent message"));

        let projection = parse_provider_error(
            br#"{"error":{"code":"bad_request","type":"request_error","message":[]}}"#,
        )
        .expect("message 类型错误不应抹掉合法标识");
        assert_eq!(projection.code.as_deref(), Some("bad_request"));
        assert_eq!(projection.kind.as_deref(), Some("request_error"));
        assert_eq!(projection.message, None);

        assert!(parse_provider_error(br#"{"message":"top-level"}"#).is_none());
        assert!(parse_provider_error(br#"{"error":"plain text"}"#).is_none());
        assert!(parse_provider_error(b"not-json").is_none());

        let long_message = "x".repeat(20_000);
        let body = serde_json::json!({"error":{"message":long_message.clone()}}).to_string();
        let projection = parse_provider_error(body.as_bytes()).expect("长标准消息应可解析");
        assert_eq!(projection.message.as_deref(), Some(long_message.as_str()));
    }

    #[test]
    fn http_diagnostic_uses_only_stable_provider_identifiers() {
        let source = OpenAiCompatibleError::HttpStatus {
            status: 429,
            provider_code: Some("PROVIDER_CODE_WITH_CONTROL\r\nforged".to_owned()),
            provider_type: Some("rate_limit".to_owned()),
            provider_message: Some("request\r\nforged".to_owned()),
            response_body_error: None,
            service_status: LlmServiceStatus::RateLimited,
        };
        let endpoint =
            Url::parse("https://api.example.test/v1/chat/completions").expect("测试 endpoint 合法");
        let diagnostic = source.diagnostic(&endpoint, Some(Duration::from_secs(3)));
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains("PROVIDER_CODE_WITH_CONTROL"));
        assert!(!serialized.contains("\\r"));
        assert!(!serialized.contains("\\n"));
        assert!(serialized.contains("\"status\":429"));
        assert!(serialized.contains("\"retry_after_seconds\":3"));
        assert!(serialized.contains("\"provider_type\":\"rate_limit\""));
        assert!(serialized.contains("\"provider_code\":null"));
        assert!(serialized.contains("\"provider_message\":\"request forged\""));
    }

    #[test]
    fn service_status_uses_only_http_status_and_structured_provider_identifiers() {
        assert_eq!(
            classify_service_status(
                StatusCode::TOO_MANY_REQUESTS,
                Some("insufficient_quota"),
                None,
            ),
            LlmServiceStatus::PermanentQuota
        );
        assert_eq!(
            classify_service_status(
                StatusCode::TOO_MANY_REQUESTS,
                Some("account_deactivated"),
                None,
            ),
            LlmServiceStatus::PermanentAccount
        );
        assert_eq!(
            classify_service_status(StatusCode::UNAUTHORIZED, None, None),
            LlmServiceStatus::PermanentAuthorization
        );
        assert_eq!(
            classify_service_status(StatusCode::TOO_MANY_REQUESTS, None, None),
            LlmServiceStatus::RateLimited
        );
        assert_eq!(
            classify_service_status(StatusCode::INTERNAL_SERVER_ERROR, None, None),
            LlmServiceStatus::Other
        );
    }

    #[tokio::test]
    async fn ordinary_429_retry_after_is_shared_without_accepting_an_overlong_wait() {
        let lifecycle = LlmLifecycle::new(Duration::from_millis(100));
        lifecycle.extend_retry_gate(Some(Duration::from_millis(40)));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), lifecycle.wait_for_retry_gate())
                .await
                .is_err(),
            "同一执行器的其他请求必须观察普通 429 的共享等待"
        );
        tokio::time::timeout(Duration::from_millis(100), lifecycle.wait_for_retry_gate())
            .await
            .expect("共享 Retry-After 到期后请求应继续")
            .expect("共享 Retry-After 等待不应失败");

        let overlong = LlmLifecycle::new(Duration::from_millis(10));
        overlong.extend_retry_gate(Some(Duration::from_secs(1)));
        tokio::time::timeout(Duration::from_millis(10), overlong.wait_for_retry_gate())
            .await
            .expect("超过配置上限的 Retry-After 不得挂起其他在途任务")
            .expect("忽略超长共享等待不应失败");
    }

    #[tokio::test]
    async fn fatal_http_provider_message_is_redacted_and_sanitized_before_error_storage() {
        let body = serde_json::json!({
            "error": {
                "code": "bad_request",
                "type": "invalid_request_error",
                "message": "before test-secret\r\n\u{0000}\u{202e} after test-secret"
            }
        })
        .to_string();
        let server = spawn_test_server(
            vec![status_response(
                "400 Bad Request",
                "Content-Type: application/json\r\n",
                &body,
            )],
            false,
        );
        let client = client(&server.endpoint, Map::new());
        let executor = executor(1);

        let source = match executor.request(&client, &[]).await {
            Err(LlmRequestError::Fatal(source)) => source,
            other => panic!("400 应是 Fatal，实际为 {other:?}"),
        };
        let debug = format!("{source:?}");
        assert!(!debug.contains("test-secret"));
        assert!(
            !debug
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0' | '\u{202e}'))
        );
        assert_eq!(debug.matches("[REDACTED API KEY]").count(), 2);

        let diagnostic = source.diagnostic(&client.url, None);
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains("test-secret"));
        assert!(serialized.contains("before [REDACTED API KEY] after [REDACTED API KEY]"));
        assert!(serialized.contains("\"provider_code\":\"bad_request\""));
        assert!(serialized.contains("\"provider_type\":\"invalid_request_error\""));

        assert!(server.requests.recv_timeout(Duration::from_secs(1)).is_ok());
        server.worker.join().expect("测试服务器应正常退出");
        executor.shutdown().await;
    }

    struct SerializationFailureSentinel;

    impl Serialize for SerializationFailureSentinel {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom(
                "REQUEST_AND_PARAMETER_BODY_SENTINEL",
            ))
        }
    }

    #[test]
    fn json_diagnostics_keep_category_and_coordinates_without_request_or_response_text() {
        let serialization_source =
            serde_json::to_vec(&SerializationFailureSentinel).expect_err("测试序列化器必须失败");
        let endpoint =
            Url::parse("https://api.example.test/v1/chat/completions").expect("测试 endpoint 合法");
        let serialization = OpenAiCompatibleError::SerializeRequest(serialization_source)
            .diagnostic(&endpoint, None);
        let serialization = serde_json::to_string(&serialization).expect("序列化诊断应可序列化");
        assert!(serialization.contains("http.request_serialization"));
        assert!(serialization.contains("\"category\":\"data\""));
        assert!(serialization.contains("\"line\":0"));
        assert!(serialization.contains("\"column\":0"));
        assert!(!serialization.contains("REQUEST_AND_PARAMETER_BODY_SENTINEL"));

        let response_body = br#"{"payload":"RESPONSE_MODEL_BODY_SENTINEL",]"#;
        let parsing_source =
            serde_json::from_slice::<Value>(response_body).expect_err("测试响应必须是无效 JSON");
        let line = parsing_source.line();
        let column = parsing_source.column();
        let parsing =
            OpenAiCompatibleError::ParseResponse(parsing_source).diagnostic(&endpoint, None);
        let parsing = serde_json::to_string(&parsing).expect("解析诊断应可序列化");
        assert!(parsing.contains("http.response_json"));
        assert!(parsing.contains("\"category\":\"syntax\""));
        assert!(parsing.contains(&format!("\"line\":{line}")));
        assert!(parsing.contains(&format!("\"column\":{column}")));
        assert!(!parsing.contains("RESPONSE_MODEL_BODY_SENTINEL"));
    }

    #[test]
    fn cancelled_local_admission_is_distinct_from_a_closed_executor() {
        let endpoint =
            Url::parse("https://api.example.test/v1/chat/completions").expect("测试 endpoint 合法");
        let cancelled = OpenAiCompatibleError::WaitCancelled.diagnostic(&endpoint, None);
        let closed = OpenAiCompatibleError::ExecutorClosed.diagnostic(&endpoint, None);
        assert_eq!(cancelled.code(), "http.wait_cancelled");
        assert_eq!(closed.code(), "http.executor_closed");
        assert_ne!(cancelled, closed);
    }

    #[test]
    fn retry_after_supports_seconds_and_http_date() {
        let seconds = reqwest::header::HeaderValue::from_static("7");
        assert_eq!(
            parse_retry_after(Some(&seconds)),
            Some(Duration::from_secs(7))
        );

        let future = SystemTime::now() + Duration::from_secs(60);
        let date = reqwest::header::HeaderValue::from_str(&httpdate::fmt_http_date(future))
            .expect("HTTP date 响应头有效");
        let parsed = parse_retry_after(Some(&date)).expect("HTTP date 应可解析");
        assert!(parsed <= Duration::from_secs(60));
        assert!(parsed >= Duration::from_secs(58));
    }

    #[tokio::test]
    async fn local_server_observes_exact_request_and_ignores_provider_metadata() {
        let server = spawn_test_server(
            vec![success_response("response-body", "request-header", "[]")],
            false,
        );
        let client = client(&server.endpoint, Map::new());
        let executor = executor(1);

        let response = executor
            .request(
                &client,
                &[
                    ChatMessage::new(ChatMessageRole::System, "contract"),
                    ChatMessage::new(ChatMessageRole::User, "content"),
                ],
            )
            .await
            .expect("本地响应应成功");
        assert_eq!(response.content(), "[]");

        let request = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("测试请求应被记录");
        assert!(request_headers(&request).starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            request_headers(&request)
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret")
        );
        let wire: Value = serde_json::from_slice(request_body(&request)).expect("请求应为 JSON");
        assert_eq!(wire.as_object().expect("请求顶层应为对象").len(), 3);
        assert_eq!(wire["model"], "test-model");
        assert_eq!(wire["stream"], false);
        assert!(wire.get("n").is_none());
        assert!(wire.get("max_tokens").is_none());
        assert!(wire.get("max_completion_tokens").is_none());
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][1]["content"], "content");

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn local_server_observes_completed_responses_endpoint_and_wire() {
        let server = spawn_test_server(vec![responses_success_response("[]")], false);
        let base = server
            .endpoint
            .strip_suffix("/chat/completions")
            .expect("测试服务器 endpoint 应使用 Chat 后缀");
        let client = client_with_protocol(base, OpenAiProtocol::Responses, Map::new());
        let executor = executor(1);

        let response = executor
            .request(
                &client,
                &[
                    ChatMessage::new(ChatMessageRole::System, "contract"),
                    ChatMessage::new(ChatMessageRole::User, "content"),
                ],
            )
            .await
            .expect("Responses 本地响应应成功");
        assert_eq!(response.content(), "[]");

        let request = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("Responses 测试请求应被记录");
        assert!(request_headers(&request).starts_with("POST /v1/responses HTTP/1.1"));
        let wire: Value = serde_json::from_slice(request_body(&request)).expect("请求应为 JSON");
        assert!(wire.get("messages").is_none());
        assert_eq!(wire["background"], false);
        assert_eq!(wire["input"][0]["role"], "system");
        assert_eq!(wire["input"][1]["content"], "content");

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn success_ignores_content_type_and_invalid_request_id_metadata() {
        let server = spawn_test_server(
            vec![
                status_response("200 OK", "", core_success_body()),
                status_response(
                    "200 OK",
                    "Content-Type: text/plain\r\n",
                    core_success_body(),
                ),
                response_with_invalid_request_id(),
            ],
            false,
        );
        let client = client(&server.endpoint, Map::new());
        let executor = executor(1);

        for _ in 0..3 {
            let response = executor
                .request(&client, &[])
                .await
                .expect("Content-Type 与请求 ID 不是核心响应字段");
            assert_eq!(response.content(), "[]");
        }

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn api_key_is_sent_exactly_once() {
        let server = spawn_test_server(
            vec![success_response("response-body", "request-header", "[]")],
            false,
        );
        let mut client = client(&server.endpoint, Map::new());
        client.api_key = SecretString::from("exact-secret");
        let executor = executor(1);

        executor
            .request(
                &client,
                &[ChatMessage::new(ChatMessageRole::User, "content")],
            )
            .await
            .expect("本地响应应成功");
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("测试请求应被记录");
        let headers = request_headers(&request);
        let authorization_values = headers
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim())
            })
            .collect::<Vec<_>>();
        assert_eq!(authorization_values, ["Bearer exact-secret"]);

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn active_capacity_backpressures_without_rejecting_waiters() {
        let mut server = spawn_test_server(
            vec![
                success_response("response-1", "request-1", "[]"),
                success_response("response-2", "request-2", "[]"),
            ],
            true,
        );
        let client = Arc::new(client_with_rate(&server.endpoint, Map::new(), 60_000, 3));
        let executor = executor(1);
        let first_executor = executor.clone();
        let first_client = Arc::clone(&client);
        let first = tokio::spawn(async move {
            first_executor
                .request(
                    first_client.as_ref(),
                    &[ChatMessage::new(ChatMessageRole::User, "first")],
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match server.first_request_seen.try_recv() {
                    Ok(()) => break,
                    Err(mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("测试服务器在首个请求前退出")
                    }
                }
            }
        })
        .await
        .expect("首个活动请求应到达服务器");
        server.requests.recv().expect("应记录首个请求");

        let second_executor = executor.clone();
        let second_client = Arc::clone(&client);
        let second = tokio::spawn(async move {
            second_executor
                .request(
                    second_client.as_ref(),
                    &[ChatMessage::new(ChatMessageRole::User, "second")],
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            matches!(server.requests.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "第二个请求必须在活动许可前自然等待，而不是提前发送或失败"
        );

        server
            .release_first
            .take()
            .expect("首个响应应有释放端")
            .send(())
            .expect("首个响应应可释放");
        first
            .await
            .expect("首个任务不应 panic")
            .expect("首个请求应成功");
        second
            .await
            .expect("第二个任务不应 panic")
            .expect("第二个请求应成功");
        server.requests.recv().expect("许可释放后应发送第二个请求");
        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn permanent_service_failure_stops_waiting_http_requests_and_keeps_inflight_success() {
        let server = spawn_concurrent_decision_server(
            "slow-success",
            "fast-permanent",
            status_response(
                "401 Unauthorized",
                "Content-Type: application/json\r\n",
                r#"{"error":{"code":"invalid_api_key","type":"authentication_error","message":"SECRET_BODY"}}"#,
            ),
            success_response("slow-response", "slow-request", "[]"),
        );
        let ConcurrentDecisionServer {
            endpoint,
            initial_requests_seen,
            release_decision,
            extra_request_seen,
            worker,
        } = server;
        let client = Arc::new(client_with_rate(&endpoint, Map::new(), 60_000, 3));
        let executor = executor(2);
        let cancellation = CooperativeCancellation::default();

        let slow_executor = executor.clone();
        let slow_client = Arc::clone(&client);
        let slow_cancellation = cancellation.clone();
        let slow = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &slow_executor,
                slow_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "slow-success")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &slow_cancellation,
            )
            .await
        });
        let failure_executor = executor.clone();
        let failure_client = Arc::clone(&client);
        let failure_cancellation = cancellation.clone();
        let failure = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &failure_executor,
                failure_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "fast-permanent")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &failure_cancellation,
            )
            .await
        });
        tokio::task::spawn_blocking(move || {
            initial_requests_seen
                .recv_timeout(Duration::from_secs(1))
                .expect("两个初始活动请求应到达服务器");
        })
        .await
        .expect("等待初始窗口不应 panic");

        let waiting_executor = executor.clone();
        let waiting_client = Arc::clone(&client);
        let waiting_cancellation = cancellation.clone();
        let waiting = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &waiting_executor,
                waiting_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "must-not-send")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &waiting_cancellation,
            )
            .await
        });
        release_decision.send(()).expect("永久错误响应应可释放");

        let (slow_outcome, _) = slow.await.expect("慢成功任务不应 panic").into_parts();
        let (failure_outcome, _) = failure.await.expect("永久错误任务不应 panic").into_parts();
        let (waiting_outcome, _) = waiting.await.expect("等待任务不应 panic").into_parts();
        assert!(matches!(
            slow_outcome,
            LlmRequestExecutionOutcome::Response { .. }
        ));
        assert!(matches!(
            failure_outcome,
            LlmRequestExecutionOutcome::Fatal { source, .. }
                if source.service_status().is_permanent()
        ));
        assert!(matches!(
            waiting_outcome,
            LlmRequestExecutionOutcome::AdmissionStopped {
                service_status: LlmServiceStatus::PermanentAuthorization,
                ..
            }
        ));
        assert!(
            !extra_request_seen
                .recv_timeout(Duration::from_secs(1))
                .expect("额外请求事实应返回"),
            "永久错误确认后不得再发送等待中的模型请求"
        );

        executor.shutdown().await;
        worker.join().expect("并发决定测试服务器应正常退出");
    }

    #[tokio::test]
    async fn slow_quota_body_blocks_replacement_request_before_permanent_classification() {
        let SlowBodyDecisionServer {
            endpoint,
            initial_requests_seen,
            release_responses,
            extra_before_body,
            extra_after_body,
            worker,
        } = spawn_slow_quota_body_server();
        let client = Arc::new(client_with_rate(&endpoint, Map::new(), 60_000, 3));
        let executor = executor(2);
        let cancellation = CooperativeCancellation::default();

        let slow_executor = executor.clone();
        let slow_client = Arc::clone(&client);
        let slow_cancellation = cancellation.clone();
        let slow = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &slow_executor,
                slow_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "slow-quota-body")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &slow_cancellation,
            )
            .await
        });
        let fast_executor = executor.clone();
        let fast_client = Arc::clone(&client);
        let fast_cancellation = cancellation.clone();
        let fast = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &fast_executor,
                fast_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "fast-success")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &fast_cancellation,
            )
            .await
        });
        tokio::task::spawn_blocking(move || {
            initial_requests_seen
                .recv_timeout(Duration::from_secs(1))
                .expect("两个初始活动请求应到达服务器");
        })
        .await
        .expect("等待初始窗口不应 panic");

        let waiting_executor = executor.clone();
        let waiting_client = Arc::clone(&client);
        let waiting_cancellation = cancellation.clone();
        let waiting = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &waiting_executor,
                waiting_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "must-not-send")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &waiting_cancellation,
            )
            .await
        });
        release_responses.send(()).expect("测试响应应可释放");

        assert!(
            !extra_before_body
                .recv_timeout(Duration::from_secs(1))
                .expect("慢正文前事实应返回"),
            "非 2xx 正文尚未完成分类时不得让等待任务补位发送"
        );
        let (fast, _) = fast.await.expect("快成功任务不应 panic").into_parts();
        let (slow, _) = slow.await.expect("quota 任务不应 panic").into_parts();
        let (waiting, _) = waiting.await.expect("等待任务不应 panic").into_parts();
        assert!(matches!(fast, LlmRequestExecutionOutcome::Response { .. }));
        assert!(matches!(
            slow,
            LlmRequestExecutionOutcome::Fatal { source, .. }
                if source.service_status() == LlmServiceStatus::PermanentQuota
        ));
        assert!(matches!(
            waiting,
            LlmRequestExecutionOutcome::AdmissionStopped {
                service_status: LlmServiceStatus::PermanentQuota,
                ..
            }
        ));
        assert!(
            !extra_after_body
                .recv_timeout(Duration::from_secs(1))
                .expect("慢正文后事实应返回"),
            "永久 quota 分类后不得发送等待请求"
        );

        executor.shutdown().await;
        worker.join().expect("慢 quota 正文测试服务器应正常退出");
    }

    #[tokio::test]
    async fn exhausted_rate_limit_stops_waiting_http_requests_and_keeps_inflight_success() {
        let server = spawn_concurrent_decision_server(
            "slow-success",
            "rate-limit",
            status_response(
                "429 Too Many Requests",
                "Content-Type: application/json\r\n",
                r#"{"error":{"code":"rate_limit_exceeded","type":"requests","message":"SECRET_BODY"}}"#,
            ),
            success_response("slow-response", "slow-request", "[]"),
        );
        let ConcurrentDecisionServer {
            endpoint,
            initial_requests_seen,
            release_decision,
            extra_request_seen,
            worker,
        } = server;
        let client = Arc::new(client_with_rate(&endpoint, Map::new(), 60_000, 3));
        let executor = executor(2);
        let cancellation = CooperativeCancellation::default();

        let slow_executor = executor.clone();
        let slow_client = Arc::clone(&client);
        let slow_cancellation = cancellation.clone();
        let slow = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &slow_executor,
                slow_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "slow-success")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &slow_cancellation,
            )
            .await
        });
        let limited_executor = executor.clone();
        let limited_client = Arc::clone(&client);
        let limited_cancellation = cancellation.clone();
        let limited = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &limited_executor,
                limited_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "rate-limit")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &limited_cancellation,
            )
            .await
        });
        tokio::task::spawn_blocking(move || {
            initial_requests_seen
                .recv_timeout(Duration::from_secs(1))
                .expect("两个初始活动请求应到达服务器");
        })
        .await
        .expect("等待初始窗口不应 panic");

        let waiting_executor = executor.clone();
        let waiting_client = Arc::clone(&client);
        let waiting_cancellation = cancellation.clone();
        let waiting = tokio::spawn(async move {
            execute_llm_request_with_retry(
                &waiting_executor,
                waiting_client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "must-not-send")],
                LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
                &ImmediateDelay,
                &waiting_cancellation,
            )
            .await
        });
        release_decision.send(()).expect("429 响应应可释放");

        let (slow_outcome, _) = slow.await.expect("慢成功任务不应 panic").into_parts();
        let (limited_outcome, _) = limited.await.expect("429 任务不应 panic").into_parts();
        let (waiting_outcome, _) = waiting.await.expect("等待任务不应 panic").into_parts();
        assert!(matches!(
            slow_outcome,
            LlmRequestExecutionOutcome::Response { .. }
        ));
        assert!(matches!(
            limited_outcome,
            LlmRequestExecutionOutcome::RetryBudgetExhausted {
                service_status: LlmServiceStatus::RateLimited,
                ..
            }
        ));
        assert!(matches!(
            waiting_outcome,
            LlmRequestExecutionOutcome::AdmissionStopped {
                service_status: LlmServiceStatus::RateLimited,
                ..
            }
        ));
        assert!(
            !extra_request_seen
                .recv_timeout(Duration::from_secs(1))
                .expect("额外请求事实应返回"),
            "429 重试耗尽后不得再发送等待中的模型请求"
        );

        executor.shutdown().await;
        worker.join().expect("并发决定测试服务器应正常退出");
    }

    #[tokio::test]
    async fn exhausted_server_error_does_not_stop_later_http_requests() {
        let server = spawn_test_server(
            vec![
                status_response(
                    "500 Internal Server Error",
                    "Content-Type: application/json\r\n",
                    r#"{"error":{"code":"server_error","type":"temporary"}}"#,
                ),
                success_response("later-response", "later-request", "[]"),
            ],
            false,
        );
        let client = client_with_rate(&server.endpoint, Map::new(), 60_000, 2);
        let executor = executor(1);
        let cancellation = CooperativeCancellation::default();

        let first = execute_llm_request_with_retry(
            &executor,
            &client,
            &[ChatMessage::new(ChatMessageRole::User, "server-error")],
            LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
            &ImmediateDelay,
            &cancellation,
        )
        .await;
        let (first, _) = first.into_parts();
        assert!(matches!(
            first,
            LlmRequestExecutionOutcome::RetryBudgetExhausted {
                service_status: LlmServiceStatus::Other,
                ..
            }
        ));

        let later = execute_llm_request_with_retry(
            &executor,
            &client,
            &[ChatMessage::new(ChatMessageRole::User, "later")],
            LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
            &ImmediateDelay,
            &cancellation,
        )
        .await;
        assert!(matches!(
            later.into_parts().0,
            LlmRequestExecutionOutcome::Response { .. }
        ));
        assert!(server.requests.recv_timeout(Duration::from_secs(1)).is_ok());
        assert!(server.requests.recv_timeout(Duration::from_secs(1)).is_ok());

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn rate_and_active_waits_have_no_local_deadline() {
        let client = client_with_rate("http://127.0.0.1:1/v1/chat/completions", Map::new(), 60, 2);
        let lifecycle = LlmLifecycle::new(Duration::from_secs(60));

        wait_for_rate(&client, &lifecycle)
            .await
            .expect("burst 内第一个请求应立即准入");
        wait_for_rate(&client, &lifecycle)
            .await
            .expect("burst 内第二个请求应立即准入");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                wait_for_rate(&client, &lifecycle)
            )
            .await
            .is_err(),
            "burst 用尽后必须继续等待供应商速率，不得产生本地超时"
        );

        let unavailable = Arc::new(Semaphore::new(0));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                wait_for_active(Arc::clone(&unavailable), &lifecycle),
            )
            .await
            .is_err(),
            "活动许可耗尽后必须继续等待，不得产生本地超时"
        );
        lifecycle.stop_accepting();
        assert!(matches!(
            wait_for_active(unavailable, &lifecycle).await,
            Err(LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled))
        ));
    }

    #[tokio::test]
    async fn business_cancellation_wakes_a_request_waiting_for_rate() {
        let client = Arc::new(client_with_rate(
            "http://127.0.0.1:1/v1/chat/completions",
            Map::new(),
            60,
            1,
        ));
        let executor = executor(1);
        wait_for_rate(client.as_ref(), &executor.lifecycle)
            .await
            .expect("burst 内第一个请求应立即准入");

        let waiting_executor = executor.clone();
        let waiting_client = Arc::clone(&client);
        let mut waiting = tokio::spawn(async move {
            wait_for_rate(waiting_client.as_ref(), &waiting_executor.lifecycle).await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err(),
            "第二个请求必须正在等待低 RPM，而不是提前完成"
        );

        executor.cancel_waits();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("业务取消必须立即唤醒 RPM 等待")
            .expect("RPM 等待任务不应 panic");
        assert!(matches!(
            result,
            Err(LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled))
        ));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn business_cancellation_wakes_a_request_waiting_for_active_capacity() {
        let executor = executor(1);
        let held = Arc::clone(&executor.active_capacity)
            .acquire_owned()
            .await
            .expect("测试应取得唯一活动许可");
        let waiting_executor = executor.clone();
        let mut waiting = tokio::spawn(async move {
            wait_for_active(
                Arc::clone(&waiting_executor.active_capacity),
                &waiting_executor.lifecycle,
            )
            .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err(),
            "第二个请求必须正在等待活动许可，而不是提前完成"
        );

        executor.cancel_waits();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("业务取消必须立即唤醒活动许可等待")
            .expect("活动许可等待任务不应 panic");
        assert!(matches!(
            result,
            Err(LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled))
        ));
        drop(held);
        executor.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_wins_when_rate_admission_becomes_ready_at_the_same_time() {
        let client = Arc::new(client_with_rate(
            "http://127.0.0.1:1/v1/chat/completions",
            Map::new(),
            6_000,
            1,
        ));
        let lifecycle = Arc::new(LlmLifecycle::new(Duration::from_secs(60)));
        wait_for_rate(client.as_ref(), lifecycle.as_ref())
            .await
            .expect("首个 burst 令牌应立即可用");

        let waiting_client = Arc::clone(&client);
        let waiting_lifecycle = Arc::clone(&lifecycle);
        let waiting = tokio::spawn(async move {
            wait_for_rate(waiting_client.as_ref(), waiting_lifecycle.as_ref()).await
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "第二次准入应先等待速率令牌");

        // current-thread runtime 在这里不会轮询等待任务；恢复轮询前令牌和停止信号
        // 已同时就绪，固定验证停止优先，而不是依赖 select 的随机分支。
        std::thread::sleep(Duration::from_millis(15));
        lifecycle.stop_accepting();
        let result = waiting.await.expect("速率等待任务不应 panic");
        assert!(matches!(
            result,
            Err(LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_wins_and_releases_permit_when_active_admission_is_simultaneously_ready() {
        let lifecycle = Arc::new(LlmLifecycle::new(Duration::from_secs(60)));
        let capacity = Arc::new(Semaphore::new(0));
        let waiting_lifecycle = Arc::clone(&lifecycle);
        let waiting_capacity = Arc::clone(&capacity);
        let waiting = tokio::spawn(async move {
            wait_for_active(waiting_capacity, waiting_lifecycle.as_ref()).await
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "活动许可应仍在等待");

        lifecycle.stop_accepting();
        capacity.add_permits(1);
        let result = waiting.await.expect("活动许可等待任务不应 panic");
        assert!(matches!(
            result,
            Err(LlmRequestError::Fatal(OpenAiCompatibleError::WaitCancelled))
        ));
        assert_eq!(capacity.available_permits(), 1, "取消必须归还并发许可");
    }

    #[tokio::test]
    async fn cancelling_an_active_request_releases_every_admission_resource() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应成功");
        let address = listener.local_addr().expect("测试监听地址应可读");
        let (seen_sender, seen_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("llm-cancellation-test-server".to_owned())
            .spawn(move || {
                let (mut stream, _) = listener.accept().expect("测试请求应可接受");
                let _request = read_http_request(&mut stream);
                seen_sender.send(()).expect("测试请求应可通知");
                release_receiver.recv().expect("测试服务器应被释放");
                let _ = stream.write_all(&success_response("response", "request", "[]"));
            })
            .expect("测试服务器线程应创建成功");

        let endpoint = format!("http://{address}/v1/chat/completions");
        let client = Arc::new(client_with_rate(&endpoint, Map::new(), 60, 1));
        let executor = executor(1);
        let request_executor = executor.clone();
        let request_client = Arc::clone(&client);
        let request = tokio::spawn(async move {
            request_executor
                .request(
                    request_client.as_ref(),
                    &[ChatMessage::new(ChatMessageRole::User, "cancel")],
                )
                .await
        });

        tokio::task::spawn_blocking(move || {
            seen_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("活动请求应到达服务器");
        })
        .await
        .expect("等待线程不应 panic");
        assert_eq!(executor.active_capacity.available_permits(), 0);

        request.abort();
        let _ = request.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if executor.active_capacity.available_permits() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("取消后全部准入许可都应归还");
        tokio::time::timeout(Duration::from_secs(1), executor.lifecycle.wait_until_idle())
            .await
            .expect("取消后生命周期作业应归零");

        release_sender.send(()).expect("测试服务器应可释放");
        executor.shutdown().await;
        worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn retryable_http_status_preserves_retry_after_without_root_retry() {
        let server = spawn_test_server(
            vec![status_response(
                "429 Too Many Requests",
                "Retry-After: 3\r\nContent-Type: application/json\r\n",
                r#"{"error":{"message":"retry later","code":"rate_limit","type":"service_error"}}"#,
            )],
            false,
        );
        let client = client_with_rate(&server.endpoint, Map::new(), 60, 1);
        let executor = executor(1);

        assert!(matches!(
            executor
                .request(
                    &client,
                    &[ChatMessage::new(ChatMessageRole::User, "content")]
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiCompatibleError::HttpStatus {
                    status: 429,
                    provider_message: Some(message),
                    ..
                },
                retry_after: Some(duration),
            }) if duration == Duration::from_secs(3) && message == "retry later"
        ));
        assert!(server.requests.recv_timeout(Duration::from_secs(1)).is_ok());
        server.worker.join().expect("测试服务器应正常退出");
        assert!(matches!(
            server.requests.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn non_success_status_preserves_truncated_body_read_failure() {
        let response = b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 128\r\nConnection: close\r\n\r\n{"
            .to_vec();
        let server = spawn_test_server(vec![response], false);
        let client = client_with_rate(&server.endpoint, Map::new(), 60, 1);
        let executor = executor(1);

        let result = executor
            .request(
                &client,
                &[ChatMessage::new(ChatMessageRole::User, "content")],
            )
            .await;

        let Err(LlmRequestError::Fatal(error)) = result else {
            panic!("截断的非成功响应必须保留为致命 HTTP 状态错误");
        };
        assert!(matches!(
            &error,
            OpenAiCompatibleError::HttpStatus {
                status: 400,
                response_body_error: Some(_),
                ..
            }
        ));
        assert!(
            Error::source(&error).is_some(),
            "HTTP 状态错误的 source 必须保留响应正文读取错误"
        );
        let diagnostic = error.diagnostic(&client.url, None);
        let diagnostic = serde_json::to_value(diagnostic).expect("HTTP 诊断应可序列化");
        assert_eq!(
            diagnostic["issue"]["details"]["response_read_failure"]["phase"],
            "read_error_response"
        );
        assert!(server.requests.recv_timeout(Duration::from_secs(1)).is_ok());
        server.worker.join().expect("测试服务器应正常退出");
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn connection_failure_is_retryable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试端口应可占用");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("测试地址应可读")
        );
        drop(listener);
        let client = client(&endpoint, Map::new());
        let executor = executor(1);

        assert!(matches!(
            executor
                .request(
                    &client,
                    &[ChatMessage::new(ChatMessageRole::User, "content")]
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiCompatibleError::Transport { .. },
                retry_after: None,
            })
        ));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn lifecycle_stop_and_idle_state_cannot_miss_a_notification() {
        let lifecycle = Arc::new(LlmLifecycle::new(Duration::from_secs(60)));
        let job = lifecycle.register().expect("停止前应可注册作业");
        lifecycle.stop_accepting();

        tokio::time::timeout(Duration::from_millis(20), lifecycle.wait_for_stop())
            .await
            .expect("停止事实应由新订阅者立即观察");
        drop(job);
        tokio::time::timeout(Duration::from_millis(20), lifecycle.wait_until_idle())
            .await
            .expect("空闲事实应由新订阅者立即观察");
    }
}
