//! OpenAI-compatible Chat Completions 生产根。

use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Client, Proxy, StatusCode, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, watch};
use url::Url;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic,
};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmCallSite, LlmClientConcurrency, LlmClientSemanticIdentity,
    LlmFinishReason, LlmRequestError, LlmRequestExecutor, LlmResponse, LlmUsage,
};
use crate::runtime::llm_call_review::{
    LlmCallDisposition, LlmCallRecorder, LlmCallRequestRecord, LlmCallReviewError,
    LlmProviderHeaders, LlmProviderRecord,
};

/// 一个可被不同引擎及 Lua 共享的受信 LLM Client。
pub(crate) struct OpenAiChatCompletionClient {
    url: Url,
    api_key: SecretString,
    model: String,
    max_concurrent_requests: NonZeroUsize,
    request_timeout: Duration,
    parameters: Map<String, Value>,
    rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
}

impl OpenAiChatCompletionClient {
    pub(crate) fn new(
        url: Url,
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
        Self {
            url,
            api_key,
            model: model.into(),
            max_concurrent_requests,
            request_timeout,
            parameters,
            rate_limiter,
        }
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    #[cfg(test)]
    pub(crate) const fn api_key(&self) -> &SecretString {
        &self.api_key
    }
}

impl LlmClientSemanticIdentity for OpenAiChatCompletionClient {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        let canonical_parameters =
            canonical_json_semantic_bytes(&Value::Object(self.parameters.clone()));
        let mut hasher = Sha256FramedHasher::new(b"att.llm.chat-completions.semantics");
        hasher
            .frame(1, self.url.as_str().as_bytes())
            .frame(2, self.model.as_bytes())
            .frame(3, &canonical_parameters);
        hasher.finish()
    }
}

/// 为翻译语义指纹建立与配置书写形式无关的 JSON 值编码。
///
/// 对象键递归排序，数组顺序保持不变；数字按任意精度十进制值规范化，因此
/// `0.2`、`2e-1` 与 `0.20` 具有同一个语义身份。请求发送仍使用用户提供的值，
/// 这份编码只服务于失效判断。
fn canonical_json_semantic_bytes(value: &Value) -> Vec<u8> {
    fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
        output.extend_from_slice(
            &u64::try_from(bytes.len())
                .expect("x86_64 usize 必须可表示为 u64")
                .to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }

    fn encode(value: &Value, output: &mut Vec<u8>) {
        match value {
            Value::Null => output.push(0),
            Value::Bool(false) => output.push(1),
            Value::Bool(true) => output.push(2),
            Value::Number(number) => {
                output.push(3);
                let number = CanonicalJsonNumber::parse(&number.to_string());
                output.push(u8::from(number.negative));
                push_bytes(output, number.coefficient.as_bytes());
                output.push(u8::from(number.exponent.negative));
                push_bytes(output, &number.exponent.magnitude);
            }
            Value::String(value) => {
                output.push(4);
                push_bytes(output, value.as_bytes());
            }
            Value::Array(values) => {
                output.push(5);
                output.extend_from_slice(
                    &u64::try_from(values.len())
                        .expect("x86_64 usize 必须可表示为 u64")
                        .to_be_bytes(),
                );
                for value in values {
                    encode(value, output);
                }
            }
            Value::Object(object) => {
                output.push(6);
                output.extend_from_slice(
                    &u64::try_from(object.len())
                        .expect("x86_64 usize 必须可表示为 u64")
                        .to_be_bytes(),
                );
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    push_bytes(output, key.as_bytes());
                    encode(&object[key], output);
                }
            }
        }
    }

    let mut output = Vec::new();
    encode(value, &mut output);
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

impl fmt::Debug for OpenAiChatCompletionClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parameter_fields = self.parameters.keys().collect::<Vec<_>>();
        formatter
            .debug_struct("OpenAiChatCompletionClient")
            .field("url_scheme", &self.url.scheme())
            .field("url_host", &self.url.host_str())
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("request_timeout", &self.request_timeout)
            .field("rate_limited", &self.rate_limiter.is_some())
            .field("parameter_fields", &parameter_fields)
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
            Self::Explicit(_) => formatter.write_str("Explicit([REDACTED])"),
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
    proxy: LlmProxyConfiguration,
    tls: LlmTlsConfiguration,
}

impl OpenAiExecutorConfiguration {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max_active_requests: NonZeroUsize,
        connect_timeout: Duration,
        read_timeout: Duration,
        proxy: LlmProxyConfiguration,
    ) -> Self {
        Self {
            max_active_requests,
            connect_timeout,
            read_timeout,
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
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        let (subject, reason, action) = match self {
            Self::InvalidProxy(_) => (
                DiagnosticSubject::field("llm.proxy"),
                DiagnosticFailureKind::InvalidValue,
                DiagnosticAction::FixConfiguration,
            ),
            Self::InvalidCertificate(_) => (
                DiagnosticSubject::field("llm.additional_pem_files"),
                DiagnosticFailureKind::InvalidEncoding,
                DiagnosticAction::FixConfiguration,
            ),
            Self::BuildClient(_) => (
                DiagnosticSubject::component("LLM HTTP client"),
                DiagnosticFailureKind::TransportFailed,
                DiagnosticAction::Retry,
            ),
        };
        SafeDiagnostic::new(
            DiagnosticCode::HttpClientBuild,
            DiagnosticStage::CommandPreparation,
            subject,
            DiagnosticReason::failure(reason),
            DiagnosticImpact::Unchanged,
            action,
        )
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
pub(crate) struct OpenAiChatCompletionExecutor {
    client: Client,
    active_capacity: Arc<Semaphore>,
    lifecycle: Arc<LlmLifecycle>,
    call_recorder: LlmCallRecorder,
}

impl OpenAiChatCompletionExecutor {
    pub(crate) fn new(
        configuration: OpenAiExecutorConfiguration,
        call_recorder: LlmCallRecorder,
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
            lifecycle: Arc::new(LlmLifecycle::new()),
            call_recorder,
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
        client: &OpenAiChatCompletionClient,
        call_site: LlmCallSite,
        messages: &[ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<OpenAiChatCompletionError>> {
        let request_body = serialize_request(client, messages).map_err(LlmRequestError::Fatal)?;

        let job = self
            .lifecycle
            .register()
            .ok_or_else(|| LlmRequestError::Fatal(OpenAiChatCompletionError::ExecutorClosed))?;

        let rate_wait = wait_for_rate(client, &self.lifecycle);
        tokio::pin!(rate_wait);
        let review_failure = self.call_recorder.wait_for_failure();
        tokio::pin!(review_failure);
        tokio::select! {
            biased;
            source = &mut review_failure => {
                drop(job);
                return Err(self.call_review_failure(source, None));
            }
            result = &mut rate_wait => result?,
        }

        let active_wait = wait_for_active(Arc::clone(&self.active_capacity), &self.lifecycle);
        tokio::pin!(active_wait);
        let review_failure = self.call_recorder.wait_for_failure();
        tokio::pin!(review_failure);
        let active_permit = tokio::select! {
            biased;
            source = &mut review_failure => {
                drop(job);
                return Err(self.call_review_failure(source, None));
            }
            result = &mut active_wait => result?,
        };

        if self.call_recorder.is_enabled()
            && let Err(source) = self
                .call_recorder
                .record_request(
                    call_site,
                    LlmCallRequestRecord::new(client.url.clone(), request_body.clone()),
                )
                .await
        {
            drop(active_permit);
            drop(job);
            return Err(self.call_review_failure(source, None));
        }

        let request = self
            .client
            .post(client.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .timeout(client.request_timeout)
            .bearer_auth(client.api_key.expose_secret())
            .body(request_body);

        if let Err(source) = self.call_recorder.authorize_send(call_site) {
            drop(active_permit);
            drop(job);
            return Err(self.call_review_failure(source, None));
        }

        let started_at = Instant::now();
        let response = match request.send().await {
            Ok(response) => response,
            Err(source) => {
                let elapsed = started_at.elapsed();
                drop(active_permit);
                let original = classify_transport_error(source);
                let error = self
                    .complete_terminal_review(
                        call_site,
                        LlmProviderRecord::response_not_received(elapsed),
                        "response_not_received",
                        original,
                    )
                    .await;
                drop(job);
                return Err(error);
            }
        };
        let status = response.status();
        let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER));
        let review_headers = self
            .call_recorder
            .is_enabled()
            .then(|| review_provider_headers(response.headers()));
        let provider_request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let response_body = match response.bytes().await {
            Ok(body) => body,
            Err(source) => {
                let elapsed = started_at.elapsed();
                drop(active_permit);
                let original = classify_transport_error(source);
                let error = self
                    .complete_terminal_review(
                        call_site,
                        LlmProviderRecord::body_read_failed(
                            elapsed,
                            Some(status.as_u16()),
                            review_headers.unwrap_or_default(),
                        ),
                        "body_read_failed",
                        original,
                    )
                    .await;
                drop(job);
                return Err(error);
            }
        };
        drop(active_permit);

        if self.call_recorder.is_enabled()
            && let Err(source) = self
                .call_recorder
                .record_provider(
                    call_site,
                    LlmProviderRecord::response(
                        started_at.elapsed(),
                        status.as_u16(),
                        review_headers.unwrap_or_default(),
                        response_body.to_vec(),
                    ),
                )
                .await
        {
            let related = (status != StatusCode::OK).then(|| {
                Box::new(OpenAiChatCompletionError::HttpStatus {
                    status: status.as_u16(),
                    provider_code: None,
                    provider_type: None,
                })
            });
            drop(job);
            return Err(self.call_review_failure(source, related));
        }

        if status != StatusCode::OK {
            let (provider_code, provider_type) =
                parse_provider_error_identifiers(&response_body).unwrap_or((None, None));
            let source = OpenAiChatCompletionError::HttpStatus {
                status: status.as_u16(),
                provider_code,
                provider_type,
            };
            let original = if is_retryable_status(status) {
                LlmRequestError::Retryable {
                    source,
                    retry_after,
                }
            } else {
                LlmRequestError::Fatal(source)
            };
            let error = self
                .complete_disposition_review(
                    call_site,
                    LlmCallDisposition::rejected("http_status_rejected", None),
                    original,
                )
                .await;
            drop(job);
            return Err(error);
        }

        let parsed = parse_success_response(&response_body, provider_request_id);
        let parsed = match parsed {
            Ok(response) => response,
            Err(original) => {
                let code = response_rejection(&original);
                let error = self
                    .complete_disposition_review(
                        call_site,
                        LlmCallDisposition::rejected(code, None),
                        original,
                    )
                    .await;
                drop(job);
                return Err(error);
            }
        };
        drop(job);
        Ok(parsed)
    }

    fn call_review_failure(
        &self,
        source: LlmCallReviewError,
        related: Option<Box<OpenAiChatCompletionError>>,
    ) -> LlmRequestError<OpenAiChatCompletionError> {
        self.lifecycle.stop_accepting();
        LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview { source, related })
    }

    async fn complete_terminal_review(
        &self,
        call_site: LlmCallSite,
        provider: LlmProviderRecord,
        code: &'static str,
        original: LlmRequestError<OpenAiChatCompletionError>,
    ) -> LlmRequestError<OpenAiChatCompletionError> {
        if self.call_recorder.is_enabled()
            && let Err(source) = self
                .call_recorder
                .record_terminal_provider(call_site, provider, code)
                .await
        {
            return self
                .call_review_failure(source, Some(Box::new(request_error_source(original))));
        }
        if let Some(source) = self.call_recorder.failure() {
            return self
                .call_review_failure(source, Some(Box::new(request_error_source(original))));
        }
        original
    }

    async fn complete_disposition_review(
        &self,
        call_site: LlmCallSite,
        disposition: LlmCallDisposition,
        original: LlmRequestError<OpenAiChatCompletionError>,
    ) -> LlmRequestError<OpenAiChatCompletionError> {
        if self.call_recorder.is_enabled()
            && let Err(source) = self
                .call_recorder
                .record_disposition(call_site, disposition)
                .await
        {
            return self
                .call_review_failure(source, Some(Box::new(request_error_source(original))));
        }
        if let Some(source) = self.call_recorder.failure() {
            return self
                .call_review_failure(source, Some(Box::new(request_error_source(original))));
        }
        original
    }
}

impl fmt::Debug for OpenAiChatCompletionExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiChatCompletionExecutor")
            .finish_non_exhaustive()
    }
}

impl LlmRequestExecutor for OpenAiChatCompletionExecutor {
    type Client = OpenAiChatCompletionClient;
    type Error = OpenAiChatCompletionError;

    async fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        call_site: LlmCallSite,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        self.execute_request(client, call_site, messages).await
    }
}

#[derive(Debug)]
pub(crate) enum OpenAiChatCompletionError {
    WaitCancelled,
    ExecutorClosed,
    SerializeRequest(serde_json::Error),
    Transport(reqwest::Error),
    HttpStatus {
        status: u16,
        provider_code: Option<String>,
        provider_type: Option<String>,
    },
    ParseResponse(serde_json::Error),
    InvalidResponseWire {
        reason: &'static str,
    },
    CallReview {
        source: LlmCallReviewError,
        related: Option<Box<OpenAiChatCompletionError>>,
    },
}

impl fmt::Display for OpenAiChatCompletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WaitCancelled => formatter.write_str("LLM 请求在等待本地许可时被取消"),
            Self::ExecutorClosed => formatter.write_str("LLM 根已关闭"),
            Self::SerializeRequest(_) => formatter.write_str("无法序列化 LLM 请求"),
            Self::Transport(_) => formatter.write_str("LLM HTTP 传输失败"),
            Self::HttpStatus { status, .. } => write!(formatter, "LLM HTTP 状态 {status}"),
            Self::ParseResponse(_) => formatter.write_str("LLM 成功响应不是有效 JSON"),
            Self::InvalidResponseWire { reason } => {
                write!(
                    formatter,
                    "LLM 成功响应不符合 Chat Completions 契约：{reason}"
                )
            }
            Self::CallReview {
                source,
                related: Some(related),
            } => write!(
                formatter,
                "LLM 调用审阅档案失败：{source}；相关模型请求结果：{related}"
            ),
            Self::CallReview {
                source,
                related: None,
            } => write!(formatter, "LLM 调用审阅档案失败：{source}"),
        }
    }
}

impl Error for OpenAiChatCompletionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeRequest(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::ParseResponse(source) => Some(source),
            Self::CallReview { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl OpenAiChatCompletionError {
    /// 只公开 HTTP/传输与 JSON 解析器的稳定事实；请求、响应正文和
    /// serde/reqwest 原始文本始终留在 source。
    pub(crate) fn safe_diagnostic(
        &self,
        retry_after: Option<Duration>,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        match self {
            Self::WaitCancelled => model_failure(
                DiagnosticFailureKind::LockCancelled,
                impact,
                DiagnosticAction::Retry,
            ),
            Self::ExecutorClosed => model_failure(
                DiagnosticFailureKind::ExecutorClosed,
                impact,
                DiagnosticAction::Retry,
            ),
            Self::SerializeRequest(source) => json_model_failure(
                DiagnosticFailureKind::RequestSerializationFailed,
                impact,
                DiagnosticAction::ReportBug,
                source,
            ),
            Self::Transport(source) => model_failure(
                DiagnosticFailureKind::TransportFailed,
                impact,
                DiagnosticAction::CheckModelService,
            )
            .with_recovery(RecoveryFact::component(transport_classification(source))),
            Self::HttpStatus {
                status,
                provider_code,
                provider_type,
            } => SafeDiagnostic::new(
                DiagnosticCode::ModelRequest,
                DiagnosticStage::ModelRequest,
                DiagnosticSubject::component("LLM provider"),
                DiagnosticReason::Http {
                    status: Some(*status),
                    retry_after_seconds: retry_after.map(|value| value.as_secs()),
                    provider_code: provider_code.clone(),
                    provider_type: provider_type.clone(),
                },
                impact,
                if *status == 401 || *status == 403 {
                    DiagnosticAction::FixConfiguration
                } else {
                    DiagnosticAction::CheckModelService
                },
            ),
            Self::ParseResponse(source) => json_model_failure(
                DiagnosticFailureKind::ResponseParsingFailed,
                impact,
                DiagnosticAction::CheckModelService,
                source,
            ),
            Self::InvalidResponseWire { reason } => model_failure(
                DiagnosticFailureKind::InvalidResponseContract,
                impact,
                DiagnosticAction::CheckModelService,
            )
            .with_recovery(RecoveryFact::component(format!("contract={reason}"))),
            Self::CallReview { source, .. } => {
                source.safe_diagnostic(DiagnosticStage::ModelRequest, impact)
            }
        }
    }
}

impl crate::llm::LlmRequestDiagnosticSource for OpenAiChatCompletionError {
    fn request_diagnostic(
        &self,
        retry_after: Option<Duration>,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        self.safe_diagnostic(retry_after, impact)
    }

    fn related_request_diagnostics(
        &self,
        _retry_after: Option<Duration>,
        impact: DiagnosticImpact,
    ) -> Vec<SafeDiagnostic> {
        match self {
            Self::CallReview {
                related: Some(related),
                ..
            } => vec![related.safe_diagnostic(None, impact)],
            _ => Vec::new(),
        }
    }
}

fn model_failure(
    failure: DiagnosticFailureKind,
    impact: DiagnosticImpact,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::ModelRequest,
        DiagnosticStage::ModelRequest,
        DiagnosticSubject::component("LLM request"),
        DiagnosticReason::failure(failure),
        impact,
        action,
    )
}

fn json_model_failure(
    failure: DiagnosticFailureKind,
    impact: DiagnosticImpact,
    action: DiagnosticAction,
    source: &serde_json::Error,
) -> SafeDiagnostic {
    let category = match source.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    SafeDiagnostic::new(
        DiagnosticCode::ModelRequest,
        DiagnosticStage::ModelRequest,
        DiagnosticSubject::component("LLM request"),
        DiagnosticReason::failure_with_detail(
            failure,
            format!(
                "json_category={category}; line={}; column={}",
                source.line(),
                source.column()
            ),
        ),
        impact,
        action,
    )
}

fn transport_classification(source: &reqwest::Error) -> &'static str {
    if source.is_timeout() {
        "transport=timeout"
    } else if source.is_connect() {
        "transport=connect"
    } else if source.is_request() {
        "transport=request"
    } else if source.is_body() {
        "transport=body"
    } else if source.is_decode() {
        "transport=decode"
    } else if source.is_redirect() {
        "transport=redirect"
    } else {
        "transport=other"
    }
}

#[derive(Deserialize)]
struct ProviderErrorEnvelope {
    error: ProviderErrorIdentifiers,
}

#[derive(Deserialize)]
struct ProviderErrorIdentifiers {
    code: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn parse_provider_error_identifiers(body: &[u8]) -> Option<(Option<String>, Option<String>)> {
    let envelope = serde_json::from_slice::<ProviderErrorEnvelope>(body).ok()?;
    Some((
        envelope.error.code.and_then(provider_identifier),
        envelope.error.kind.and_then(provider_identifier),
    ))
}

fn review_provider_headers(headers: &reqwest::header::HeaderMap) -> LlmProviderHeaders {
    fn value(
        headers: &reqwest::header::HeaderMap,
        name: impl reqwest::header::AsHeaderName,
    ) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    LlmProviderHeaders::new(
        value(headers, CONTENT_TYPE),
        value(headers, "x-request-id"),
        value(headers, RETRY_AFTER),
    )
}

fn provider_identifier(value: String) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }))
    .then_some(value)
}

fn request_error_source(
    error: LlmRequestError<OpenAiChatCompletionError>,
) -> OpenAiChatCompletionError {
    match error {
        LlmRequestError::Retryable { source, .. } | LlmRequestError::Fatal(source) => source,
    }
}

fn response_rejection(error: &LlmRequestError<OpenAiChatCompletionError>) -> &'static str {
    match error {
        LlmRequestError::Fatal(OpenAiChatCompletionError::ParseResponse(_)) => {
            "response_json_invalid"
        }
        LlmRequestError::Fatal(OpenAiChatCompletionError::InvalidResponseWire { .. }) => {
            "response_contract_invalid"
        }
        LlmRequestError::Retryable { .. } | LlmRequestError::Fatal(_) => "response_rejected",
    }
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

fn serialize_request(
    client: &OpenAiChatCompletionClient,
    messages: &[ChatMessage],
) -> Result<Vec<u8>, OpenAiChatCompletionError> {
    let messages = messages
        .iter()
        .map(|message| RequestMessageWire {
            role: match message.role() {
                ChatMessageRole::System => "system",
                ChatMessageRole::User => "user",
                ChatMessageRole::Assistant => "assistant",
            },
            content: message.content(),
        })
        .collect::<Vec<_>>();
    let wire = ChatCompletionRequestWire {
        model: &client.model,
        messages,
        stream: false,
        parameters: &client.parameters,
    };
    serde_json::to_vec(&wire).map_err(OpenAiChatCompletionError::SerializeRequest)
}

async fn wait_for_rate(
    client: &OpenAiChatCompletionClient,
    lifecycle: &LlmLifecycle,
) -> Result<(), LlmRequestError<OpenAiChatCompletionError>> {
    if !lifecycle.is_accepting() {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::WaitCancelled,
        ));
    }
    let Some(rate_limiter) = &client.rate_limiter else {
        return if lifecycle.is_accepting() {
            Ok(())
        } else {
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled,
            ))
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
        Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::WaitCancelled,
        ))
    }
}

async fn wait_for_active(
    semaphore: Arc<Semaphore>,
    lifecycle: &LlmLifecycle,
) -> Result<tokio::sync::OwnedSemaphorePermit, LlmRequestError<OpenAiChatCompletionError>> {
    if !lifecycle.is_accepting() {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::WaitCancelled,
        ));
    }
    let stopped = lifecycle.wait_for_stop();
    tokio::pin!(stopped);
    let permit = semaphore.acquire_owned();
    tokio::pin!(permit);
    let permit = tokio::select! {
        biased;
        () = &mut stopped => {
            return Err(LlmRequestError::Fatal(OpenAiChatCompletionError::WaitCancelled));
        }
        result = &mut permit => result
            .map_err(|_| LlmRequestError::Fatal(OpenAiChatCompletionError::ExecutorClosed)),
    }?;
    if lifecycle.is_accepting() {
        Ok(permit)
    } else {
        drop(permit);
        Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::WaitCancelled,
        ))
    }
}

impl LlmClientConcurrency for OpenAiChatCompletionClient {
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.max_concurrent_requests
    }
}

fn retryable(source: OpenAiChatCompletionError) -> LlmRequestError<OpenAiChatCompletionError> {
    LlmRequestError::Retryable {
        source,
        retry_after: None,
    }
}

fn classify_transport_error(source: reqwest::Error) -> LlmRequestError<OpenAiChatCompletionError> {
    let is_tls = error_chain_contains::<native_tls::Error>(&source);
    let retry = !source.is_builder()
        && !is_tls
        && (source.is_timeout() || source.is_connect() || source.is_request() || source.is_body());
    if retry {
        retryable(OpenAiChatCompletionError::Transport(source))
    } else {
        LlmRequestError::Fatal(OpenAiChatCompletionError::Transport(source))
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
    body: &[u8],
    provider_request_id: Option<String>,
) -> Result<LlmResponse, LlmRequestError<OpenAiChatCompletionError>> {
    let wire: Value = serde_json::from_slice(body).map_err(|source| {
        LlmRequestError::Fatal(OpenAiChatCompletionError::ParseResponse(source))
    })?;
    let object = wire
        .as_object()
        .ok_or_else(|| invalid_response("顶层必须为对象"))?;
    let choices = object
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("choices 必须为数组"))?;
    let mut matching_choices = choices.iter().filter(|choice| {
        choice
            .as_object()
            .and_then(|choice| choice.get("index"))
            .and_then(Value::as_u64)
            == Some(0)
    });
    let Some(choice) = matching_choices.next() else {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::InvalidResponseWire {
                reason: "choices 必须包含唯一的数值 index 0",
            },
        ));
    };
    if matching_choices.next().is_some() {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::InvalidResponseWire {
                reason: "choices 必须包含唯一的数值 index 0",
            },
        ));
    }
    let choice = choice
        .as_object()
        .expect("index 0 choice 已经确认是 JSON 对象");
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("index 0 choice 的 message 必须为对象"))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("index 0 choice 的 message.content 必须为字符串"))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("index 0 choice 的 finish_reason 必须为字符串"))?;
    let finish_reason = match finish_reason {
        "stop" => LlmFinishReason::Stop,
        "length" => LlmFinishReason::Length,
        "content_filter" => LlmFinishReason::ContentFilter,
        other => LlmFinishReason::Other(other.to_owned()),
    };
    let provider_response_id = object.get("id").and_then(Value::as_str).map(str::to_owned);
    let usage = object.get("usage").and_then(parse_usage);
    Ok(LlmResponse::new(
        content,
        finish_reason,
        provider_request_id,
        provider_response_id,
        usage,
    ))
}

fn invalid_response(reason: &'static str) -> LlmRequestError<OpenAiChatCompletionError> {
    LlmRequestError::Fatal(OpenAiChatCompletionError::InvalidResponseWire { reason })
}

fn parse_usage(value: &Value) -> Option<LlmUsage> {
    let usage = value.as_object()?;
    Some(LlmUsage::new(
        usage.get("prompt_tokens")?.as_u64()?,
        usage.get("completion_tokens")?.as_u64()?,
        usage.get("total_tokens")?.as_u64()?,
    ))
}

struct LlmLifecycle {
    accepting: AtomicBool,
    state: Mutex<LlmLifecycleState>,
    stopping: watch::Sender<bool>,
    jobs: watch::Sender<usize>,
}

struct LlmLifecycleState {
    jobs: usize,
}

impl LlmLifecycle {
    fn new() -> Self {
        let (stopping, _) = watch::channel(false);
        let (jobs, _) = watch::channel(0);
        Self {
            accepting: AtomicBool::new(true),
            state: Mutex::new(LlmLifecycleState { jobs: 0 }),
            stopping,
            jobs,
        }
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    fn register(self: &Arc<Self>) -> Option<LlmJobGuard> {
        let mut state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        if !self.is_accepting() {
            return None;
        }
        state.jobs += 1;
        self.jobs.send_replace(state.jobs);
        Some(LlmJobGuard {
            lifecycle: Arc::clone(self),
        })
    }

    fn stop_accepting(&self) {
        let _state = self.state.lock().expect("LLM 生命周期锁不应中毒");
        self.accepting.store(false, Ordering::Release);
        self.stopping.send_replace(true);
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
    use std::num::NonZeroU64;
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::i18n::UiLocale;
    use crate::observability::RunId;
    use crate::runtime::llm_call_review::{
        LlmCallDisposition, LlmCallReviewContext, LlmParsedResponseMetadata,
    };

    fn non_zero_usize(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试值必须非零")
    }

    fn non_zero_u32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("测试值必须非零")
    }

    fn client(url: &str, parameters: Map<String, Value>) -> OpenAiChatCompletionClient {
        client_with_rate(url, parameters, 60, 2)
    }

    fn client_with_rate(
        url: &str,
        parameters: Map<String, Value>,
        rpm: u32,
        burst: u32,
    ) -> OpenAiChatCompletionClient {
        OpenAiChatCompletionClient::new(
            Url::parse(url).expect("测试 URL 有效"),
            SecretString::from("test-secret"),
            "test-model",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(rpm), non_zero_u32(burst))),
            parameters,
        )
    }

    fn executor(max_active_requests: usize) -> OpenAiChatCompletionExecutor {
        executor_with_recorder(max_active_requests, LlmCallRecorder::disabled())
    }

    fn executor_with_recorder(
        max_active_requests: usize,
        call_recorder: LlmCallRecorder,
    ) -> OpenAiChatCompletionExecutor {
        OpenAiChatCompletionExecutor::new(
            OpenAiExecutorConfiguration::new(
                non_zero_usize(max_active_requests),
                Duration::from_secs(2),
                Duration::from_secs(2),
                LlmProxyConfiguration::Disabled,
            ),
            call_recorder,
        )
        .expect("测试 LLM 根应构造成功")
    }

    async fn call_recorder(workspace: &std::path::Path, run_id: &str) -> LlmCallRecorder {
        LlmCallRecorder::start(
            workspace.to_path_buf(),
            RunId::from_uuid(uuid::Uuid::parse_str(run_id).expect("测试 RunId 必须有效")),
            UiLocale::SimplifiedChinese,
            LlmCallReviewContext::new("mz", "review-project", "quality", "primary"),
        )
        .await
        .expect("测试调用档案应建立")
    }

    fn call_site(value: u64) -> LlmCallSite {
        LlmCallSite::Lua {
            call: NonZeroU64::new(value).expect("测试调用序号必须非零"),
        }
    }

    struct TestServer {
        endpoint: String,
        requests: mpsc::Receiver<Vec<u8>>,
        first_request_seen: mpsc::Receiver<()>,
        release_first: Option<mpsc::Sender<()>>,
        worker: thread::JoinHandle<()>,
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
    fn debug_redacts_api_key_and_parameter_values() {
        let mut parameters = Map::new();
        parameters.insert(
            "vendor_secret".to_owned(),
            Value::String("must-not-appear".to_owned()),
        );
        let client = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").expect("测试 URL 有效"),
            SecretString::from("api-secret"),
            "test-model",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            parameters,
        );
        let debug = format!("{client:?}");
        assert!(debug.contains("vendor_secret"));
        assert!(!debug.contains("must-not-appear"));
        assert!(!debug.contains("api-secret"));
    }

    #[test]
    fn semantic_identity_includes_only_url_model_and_parameters() {
        let mut extra = Map::new();
        extra.insert("temperature".to_owned(), serde_json::json!(0.2));
        extra.insert(
            "vendor".to_owned(),
            serde_json::json!({"mode": "quality", "nested": {"b": 2, "a": 1}}),
        );
        let first = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-a",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            extra.clone(),
        );
        let operationally_different = OpenAiChatCompletionClient::new(
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
        let textually_reordered = OpenAiChatCompletionClient::new(
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
        let numerically_equivalent = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-a",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            numerically_equivalent_extra,
        );
        let different_model = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-b",
            non_zero_usize(8),
            Duration::from_secs(2),
            Some((non_zero_u32(60), non_zero_u32(2))),
            extra,
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
    fn successful_wire_keeps_request_and_response_id_distinct() {
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
            Some("http-request".to_owned()),
        )
        .expect("合法响应应通过");

        assert_eq!(response.provider_request_id(), Some("http-request"));
        assert_eq!(response.provider_response_id(), Some("chatcmpl-response"));
        assert_eq!(response.usage(), Some(LlmUsage::new(3, 2, 5)));
    }

    #[test]
    fn optional_response_metadata_never_invalidates_core_response() {
        for (metadata, expected_id, expected_usage) in [
            ("", None, None),
            (r#", "id": null, "usage": null"#, None, None),
            (r#", "id": 7, "usage": true"#, None, None),
            (
                r#", "id": "", "usage": {"prompt_tokens":1,"completion_tokens":2}"#,
                Some(""),
                None,
            ),
            (
                r#", "id": "response", "usage": {"prompt_tokens":1,"completion_tokens":2,"total_tokens":3.5}"#,
                Some("response"),
                None,
            ),
        ] {
            let body = format!(
                r#"{{"choices":[{{"index":0,"message":{{"content":"[]"}},"finish_reason":"stop"}}]{metadata}}}"#
            );
            let response = parse_success_response(body.as_bytes(), None)
                .expect("可选元数据异常不应否定核心响应");
            assert_eq!(response.provider_response_id(), expected_id);
            assert_eq!(response.usage(), expected_usage);
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
            None,
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
                parse_success_response(body, None),
                Err(LlmRequestError::Fatal(
                    OpenAiChatCompletionError::InvalidResponseWire { .. }
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
    fn provider_error_projection_keeps_only_stable_identifiers() {
        let (code, kind) = parse_provider_error_identifiers(
            br#"{"error":{"code":"rate_limit_exceeded","type":"requests/rate-limit","message":"MODEL_BODY_SECRET"}}"#,
        )
        .expect("供应商错误信封应可解析");
        assert_eq!(code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(kind.as_deref(), Some("requests/rate-limit"));

        let (code, kind) = parse_provider_error_identifiers(
            br#"{"error":{"code":"API_KEY_SECRET\r\nforged","type":"MODEL_BODY_SECRET!","message":"MODEL_BODY_SECRET"}}"#,
        )
        .expect("无效供应商标识不应使信封解析失败");
        assert_eq!(code, None);
        assert_eq!(kind, None);
    }

    #[test]
    fn http_diagnostic_never_exposes_provider_body_or_invalid_identifier() {
        let source = OpenAiChatCompletionError::HttpStatus {
            status: 429,
            provider_code: Some("API_KEY_SECRET\r\nforged".to_owned()),
            provider_type: Some("rate_limit".to_owned()),
        };
        let diagnostic = source.safe_diagnostic(
            Some(Duration::from_secs(3)),
            DiagnosticImpact::ProgressPreserved,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(!serialized.contains("API_KEY_SECRET"));
        assert!(!serialized.contains("forged"));
        assert!(serialized.contains("\"status\":429"));
        assert!(serialized.contains("\"retry_after_seconds\":3"));
        assert!(serialized.contains("\"provider_type\":\"rate_limit\""));
        assert!(serialized.contains("\"provider_code\":null"));
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
        let serialization = OpenAiChatCompletionError::SerializeRequest(serialization_source)
            .safe_diagnostic(None, DiagnosticImpact::Unchanged);
        let serialization = serde_json::to_string(&serialization).expect("序列化诊断应可序列化");
        assert!(serialization.contains("request_serialization_failed"));
        assert!(serialization.contains("json_category=data; line=0; column=0"));
        assert!(!serialization.contains("REQUEST_AND_PARAMETER_BODY_SENTINEL"));

        let response_body = br#"{"secret":"RESPONSE_MODEL_BODY_SENTINEL",]"#;
        let parsing_source =
            serde_json::from_slice::<Value>(response_body).expect_err("测试响应必须是无效 JSON");
        let line = parsing_source.line();
        let column = parsing_source.column();
        let parsing = OpenAiChatCompletionError::ParseResponse(parsing_source)
            .safe_diagnostic(None, DiagnosticImpact::ProgressPreserved);
        let parsing = serde_json::to_string(&parsing).expect("解析诊断应可序列化");
        assert!(parsing.contains("response_parsing_failed"));
        assert!(parsing.contains(&format!(
            "json_category=syntax; line={line}; column={column}"
        )));
        assert!(!parsing.contains("RESPONSE_MODEL_BODY_SENTINEL"));
    }

    #[test]
    fn cancelled_local_admission_is_distinct_from_a_closed_executor() {
        let cancelled = OpenAiChatCompletionError::WaitCancelled
            .safe_diagnostic(None, DiagnosticImpact::ProgressPreserved);
        assert!(cancelled.reason.is_wait_cancelled());

        let closed = OpenAiChatCompletionError::ExecutorClosed
            .safe_diagnostic(None, DiagnosticImpact::ProgressPreserved);
        assert!(!closed.reason.is_wait_cancelled());
        assert_ne!(cancelled.reason, closed.reason);
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
    async fn local_server_observes_exact_request_and_ids_stay_distinct() {
        let server = spawn_test_server(
            vec![success_response("response-body", "request-header", "[]")],
            false,
        );
        let client = client(&server.endpoint, Map::new());
        let executor = executor(1);

        let response = executor
            .request(
                &client,
                call_site(1),
                &[
                    ChatMessage::new(ChatMessageRole::System, "contract"),
                    ChatMessage::new(ChatMessageRole::User, "content"),
                ],
            )
            .await
            .expect("本地响应应成功");
        assert_eq!(response.provider_request_id(), Some("request-header"));
        assert_eq!(response.provider_response_id(), Some("response-body"));
        assert_eq!(response.usage(), Some(LlmUsage::new(4, 2, 6)));

        let request = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("测试请求应被记录");
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
    async fn enabled_call_review_records_final_wire_and_raw_response_without_credentials() {
        let response = String::from_utf8(success_response(
            "response-body",
            "request-header",
            "MODEL_BODY_SENTINEL ```",
        ))
        .expect("测试响应必须是 UTF-8")
        .replace(
            "Content-Length:",
            "X-Private-Provider-Header: RESPONSE_HEADER_SECRET_SENTINEL\r\nContent-Length:",
        )
        .into_bytes();
        let server = spawn_test_server(vec![response], false);
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").await;
        let mut parameters = Map::new();
        parameters.insert(
            "vendor_evidence".to_owned(),
            Value::String("CUSTOM_PARAMETER_SENTINEL".to_owned()),
        );
        let mut client = client(
            &format!("{}?private_query=QUERY_SECRET_SENTINEL", server.endpoint),
            parameters,
        );
        client.api_key = SecretString::from("API_KEY_SECRET_SENTINEL");
        let executor = executor_with_recorder(1, recorder.clone());
        let site = call_site(1);

        let response = executor
            .request(
                &client,
                site,
                &[
                    ChatMessage::new(ChatMessageRole::System, "SYSTEM_MESSAGE_SENTINEL"),
                    ChatMessage::new(ChatMessageRole::User, "USER_MESSAGE_SENTINEL"),
                ],
            )
            .await
            .expect("本地响应应成功");
        recorder
            .record_disposition(
                site,
                LlmCallDisposition::lua_delivered(LlmParsedResponseMetadata::from(&response)),
            )
            .await
            .expect("Lua 交付终态应同步");

        let archive = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua")
                .join("call-000001.md"),
        )
        .expect("调用档案应可读");
        for expected in [
            "request_complete",
            "provider_complete",
            "disposition_complete",
            "SYSTEM_MESSAGE_SENTINEL",
            "USER_MESSAGE_SENTINEL",
            "CUSTOM_PARAMETER_SENTINEL",
            "MODEL_BODY_SENTINEL ```",
            "request-header",
            "response-body",
            "delivered_to_lua",
        ] {
            assert!(
                archive.contains(expected),
                "调用档案缺少最终调用证据：{expected}"
            );
        }
        for secret in [
            "API_KEY_SECRET_SENTINEL",
            "QUERY_SECRET_SENTINEL",
            "Authorization",
            "RESPONSE_HEADER_SECRET_SENTINEL",
            "X-Private-Provider-Header",
        ] {
            assert!(!archive.contains(secret), "调用档案泄露了凭据：{secret}");
        }

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn retryable_status_is_durable_before_the_retry_decision() {
        let server = spawn_test_server(
            vec![status_response(
                "429 Too Many Requests",
                "Content-Type: application/problem+json\r\nRetry-After: 7\r\nx-request-id: request-429\r\n",
                r#"{"error":{"code":"rate_limited","type":"quota"},"raw":"RESPONSE_429_SENTINEL"}"#,
            )],
            false,
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").await;
        let client = client(&server.endpoint, Map::new());
        let executor = executor_with_recorder(1, recorder.clone());

        let error = executor
            .request(&client, call_site(1), &[])
            .await
            .expect_err("429 应保持可重试");
        assert!(matches!(
            error,
            LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::HttpStatus { status: 429, .. },
                retry_after: Some(value),
            } if value == Duration::from_secs(7)
        ));

        let archive = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua")
                .join("call-000001.md"),
        )
        .expect("429 调用档案应可读");
        for expected in [
            "http_status = 429",
            "request-429",
            "RESPONSE_429_SENTINEL",
            "http_status_rejected",
            "provider_complete",
            "disposition_complete",
        ] {
            assert!(archive.contains(expected), "429 档案缺少 {expected}");
        }

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn request_archive_failure_prevents_the_http_request() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应建立");
        listener
            .set_nonblocking(true)
            .expect("测试监听应切换为非阻塞");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("测试监听地址应可读")
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "cccccccc-cccc-4ccc-8ccc-cccccccccccc").await;
        std::fs::write(
            recorder.run_root().expect("启用时应有 Run 根").join("lua"),
            b"directory-conflict",
        )
        .expect("应建立与调用目录冲突的文件");
        let client = client(&endpoint, Map::new());
        let executor = executor_with_recorder(1, recorder);

        assert!(matches!(
            executor.request(&client, call_site(1), &[]).await,
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::CallReview { related: None, .. }
            ))
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "请求阶段没有同步成功时 Provider 必须收到零连接"
        );

        executor.shutdown().await;
    }

    #[tokio::test]
    async fn request_sync_failure_is_a_zero_send_root_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应建立");
        listener
            .set_nonblocking(true)
            .expect("测试监听应切换为非阻塞");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("测试监听地址应可读")
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "25252525-2525-4252-8252-252525252525").await;
        recorder.inject_test_failure("sync_request");
        let executor = executor_with_recorder(1, recorder);

        let error = executor
            .request(&client(&endpoint, Map::new()), call_site(1), &[])
            .await
            .expect_err("请求阶段同步失败必须阻止 HTTP");
        assert!(matches!(
            error,
            LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview {
                source,
                related: None,
            }) if source.operation() == "sync_request"
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "请求阶段 sync_data 失败时 Provider 必须收到零连接"
        );

        executor.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn latched_failure_wins_the_request_complete_to_http_send_race() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应建立");
        listener
            .set_nonblocking(true)
            .expect("测试监听应切换为非阻塞");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("测试监听地址应可读")
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "24242424-2424-4242-8242-242424242424").await;
        let (authorization_entered, release_authorization) =
            recorder.pause_next_send_authorization();
        let executor = executor_with_recorder(2, recorder.clone());
        let request_executor = executor.clone();
        let request = tokio::spawn(async move {
            request_executor
                .request(
                    &client(&endpoint, Map::new()),
                    call_site(1),
                    &[ChatMessage::new(ChatMessageRole::User, "must-not-send")],
                )
                .await
        });

        authorization_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("请求应在 request_complete 后、发送准入前暂停");
        let standard_root = recorder
            .run_root()
            .expect("启用时应有 Run 根")
            .join("standard");
        std::fs::create_dir_all(&standard_root).expect("Standard 根应建立");
        std::fs::write(standard_root.join("task-000002"), b"directory-conflict")
            .expect("应建立另一调用的档案故障");
        let latched = recorder
            .record_request(
                LlmCallSite::Standard {
                    task_ordinal: NonZeroU64::new(2).expect("任务序号非零"),
                    attempt: NonZeroU64::MIN,
                },
                LlmCallRequestRecord::new(
                    url::Url::parse("https://example.invalid/v1/chat/completions")
                        .expect("测试 URL 必须有效"),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect_err("另一调用的档案故障应先锁存");
        release_authorization
            .send(())
            .expect("暂停的发送准入应可释放");

        let error = request
            .await
            .expect("请求任务不应 panic")
            .expect_err("档案故障先锁存时不得发出请求");
        assert!(matches!(
            error,
            LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview {
                source,
                related: None,
            }) if source.path() == latched.path()
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "请求阶段虽已同步，但发送准入前锁存故障时 Provider 必须收到零连接"
        );

        executor.shutdown().await;
    }

    #[tokio::test]
    async fn provider_archive_failure_prevents_parsing_and_model_retry() {
        let server = spawn_test_server(
            vec![status_response(
                "200 OK",
                "Content-Type: application/json\r\n",
                "MALFORMED_RESPONSE_SENTINEL",
            )],
            false,
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "34343434-3434-4343-8343-343434343434").await;
        recorder.inject_test_failure("write_provider");
        let executor = executor_with_recorder(1, recorder.clone());

        let error = executor
            .request(&client(&server.endpoint, Map::new()), call_site(1), &[])
            .await
            .expect_err("Provider 原始正文无法持久化时不得继续解析");
        assert!(matches!(
            error,
            LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview {
                source,
                related: None,
            }) if source.operation() == "write_provider"
        ));
        server.requests.recv().expect("唯一请求应到达 Provider");
        server.worker.join().expect("测试服务器应正常退出");
        assert!(
            matches!(
                server.requests.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ),
            "档案失败不得触发模型重试"
        );

        let archive = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua")
                .join("call-000001.md"),
        )
        .expect("请求阶段档案应可读");
        assert!(archive.contains("request_complete"));
        assert!(!archive.contains("provider_complete"));
        assert!(!archive.contains("MALFORMED_RESPONSE_SENTINEL"));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn disposition_archive_failure_preserves_the_provider_error_without_retry() {
        let server = spawn_test_server(
            vec![status_response(
                "429 Too Many Requests",
                "Retry-After: 2\r\nContent-Type: application/json\r\n",
                r#"{"error":{"code":"busy","type":"rate_limit"}}"#,
            )],
            false,
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "45454545-4545-4454-8454-454545454545").await;
        recorder.inject_test_failure("sync_disposition");
        let executor = executor_with_recorder(1, recorder.clone());

        let error = executor
            .request(&client(&server.endpoint, Map::new()), call_site(1), &[])
            .await
            .expect_err("HTTP 错误的档案终态同步失败必须成为组合错误");
        assert!(matches!(
            error,
            LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview {
                source,
                related: Some(related),
            }) if source.operation() == "sync_disposition"
                && matches!(
                    *related,
                    OpenAiChatCompletionError::HttpStatus { status: 429, .. }
                )
        ));
        server.requests.recv().expect("唯一请求应到达 Provider");
        server.worker.join().expect("测试服务器应正常退出");
        assert!(
            matches!(
                server.requests.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ),
            "处置档案失败不得触发模型重试"
        );

        let archive = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua")
                .join("call-000001.md"),
        )
        .expect("Provider 阶段档案应可读");
        assert!(archive.contains("provider_complete"));
        assert!(!archive.contains("disposition_complete"));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn enabled_review_durably_classifies_transport_body_http_and_json_failures() {
        let refused_listener = TcpListener::bind("127.0.0.1:0").expect("测试监听应建立");
        let refused_endpoint = format!(
            "http://{}/v1/chat/completions",
            refused_listener.local_addr().expect("测试监听地址应可读")
        );
        drop(refused_listener);
        let transport_directory = tempfile::tempdir().expect("测试目录应建立");
        let transport_recorder = call_recorder(
            transport_directory.path(),
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        )
        .await;
        let transport_executor = executor_with_recorder(1, transport_recorder.clone());
        assert!(matches!(
            transport_executor
                .request(&client(&refused_endpoint, Map::new()), call_site(1), &[],)
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::Transport(_),
                ..
            })
        ));
        let transport_archive = std::fs::read_to_string(
            transport_recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("传输失败档案应可读");
        for expected in [
            "response_not_received",
            "provider_complete",
            "disposition_complete",
        ] {
            assert!(
                transport_archive.contains(expected),
                "传输失败档案缺少 {expected}"
            );
        }
        transport_executor.shutdown().await;

        let truncated_server = spawn_test_server(
            vec![
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-request-id: truncated-request\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"partial\":true}"
                    .to_vec(),
            ],
            false,
        );
        let body_directory = tempfile::tempdir().expect("测试目录应建立");
        let body_recorder = call_recorder(
            body_directory.path(),
            "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
        )
        .await;
        let body_executor = executor_with_recorder(1, body_recorder.clone());
        assert!(matches!(
            body_executor
                .request(
                    &client(&truncated_server.endpoint, Map::new()),
                    call_site(1),
                    &[],
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::Transport(_),
                ..
            }) | Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::Transport(_)
            ))
        ));
        let body_archive = std::fs::read_to_string(
            body_recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("正文读取失败档案应可读");
        for expected in [
            "body_read_failed",
            "http_status = 200",
            "truncated-request",
            "provider_complete",
            "disposition_complete",
        ] {
            assert!(
                body_archive.contains(expected),
                "正文读取失败档案缺少 {expected}"
            );
        }
        assert!(
            !body_archive.contains(r#"{"partial":true}"#),
            "不完整正文不得伪装成完整原始响应"
        );
        body_executor.shutdown().await;
        truncated_server
            .worker
            .join()
            .expect("截断正文服务器应正常退出");

        let http_server = spawn_test_server(
            vec![status_response(
                "400 Bad Request",
                "Content-Type: application/problem+json\r\nx-request-id: request-400\r\n",
                r#"{"error":{"code":"invalid_request"},"raw":"HTTP_400_SENTINEL"}"#,
            )],
            false,
        );
        let http_directory = tempfile::tempdir().expect("测试目录应建立");
        let http_recorder = call_recorder(
            http_directory.path(),
            "abababab-abab-4aba-8aba-abababababab",
        )
        .await;
        let http_executor = executor_with_recorder(1, http_recorder.clone());
        assert!(matches!(
            http_executor
                .request(
                    &client(&http_server.endpoint, Map::new()),
                    call_site(1),
                    &[],
                )
                .await,
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::HttpStatus { status: 400, .. }
            ))
        ));
        let http_archive = std::fs::read_to_string(
            http_recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("HTTP 失败档案应可读");
        for expected in [
            "http_status = 400",
            "HTTP_400_SENTINEL",
            "http_status_rejected",
            "provider_complete",
            "disposition_complete",
        ] {
            assert!(
                http_archive.contains(expected),
                "HTTP 失败档案缺少 {expected}"
            );
        }
        http_executor.shutdown().await;
        http_server
            .worker
            .join()
            .expect("HTTP 失败服务器应正常退出");

        let malformed_server = spawn_test_server(
            vec![status_response(
                "200 OK",
                "Content-Type: application/json\r\nx-request-id: malformed-request\r\n",
                r#"{"malformed":"JSON_SENTINEL""#,
            )],
            false,
        );
        let malformed_directory = tempfile::tempdir().expect("测试目录应建立");
        let malformed_recorder = call_recorder(
            malformed_directory.path(),
            "cdcdcdcd-cdcd-4cdc-8dcd-cdcdcdcdcdcd",
        )
        .await;
        let malformed_executor = executor_with_recorder(1, malformed_recorder.clone());
        assert!(matches!(
            malformed_executor
                .request(
                    &client(&malformed_server.endpoint, Map::new()),
                    call_site(1),
                    &[],
                )
                .await,
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::ParseResponse(_)
            ))
        ));
        let malformed_archive = std::fs::read_to_string(
            malformed_recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("畸形 JSON 档案应可读");
        for expected in [
            "JSON_SENTINEL",
            "response_json_invalid",
            "provider_complete",
            "disposition_complete",
        ] {
            assert!(
                malformed_archive.contains(expected),
                "畸形 JSON 档案缺少 {expected}"
            );
        }
        malformed_executor.shutdown().await;
        malformed_server
            .worker
            .join()
            .expect("畸形 JSON 服务器应正常退出");
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
                .request(&client, call_site(1), &[])
                .await
                .expect("Content-Type 与请求 ID 不是核心响应字段");
            assert_eq!(response.provider_request_id(), None);
            assert_eq!(response.provider_response_id(), None);
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
                call_site(1),
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
                    call_site(1),
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
                    call_site(2),
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
    async fn review_latch_wakes_active_waiter_without_abandoning_sent_call_lifecycle() {
        let mut server = spawn_test_server(
            vec![success_response("response-1", "request-1", "[]")],
            true,
        );
        let temporary = tempfile::tempdir().expect("测试目录应建立");
        let recorder =
            call_recorder(temporary.path(), "23232323-2323-4232-8232-232323232323").await;
        let client = Arc::new(client_with_rate(&server.endpoint, Map::new(), 60_000, 3));
        let executor = executor_with_recorder(1, recorder.clone());

        let first_executor = executor.clone();
        let first_client = Arc::clone(&client);
        let first = tokio::spawn(async move {
            first_executor
                .request(
                    first_client.as_ref(),
                    call_site(1),
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
                    call_site(2),
                    &[ChatMessage::new(ChatMessageRole::User, "second")],
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!second.is_finished(), "第二个请求应正在等待活动许可");

        let standard_root = recorder
            .run_root()
            .expect("启用时应有 Run 根")
            .join("standard");
        std::fs::create_dir_all(&standard_root).expect("Standard 根应建立");
        std::fs::write(standard_root.join("task-000003"), b"directory-conflict")
            .expect("应建立独立调用的档案故障");
        let latched = recorder
            .record_request(
                LlmCallSite::Standard {
                    task_ordinal: NonZeroU64::new(3).expect("任务序号非零"),
                    attempt: NonZeroU64::MIN,
                },
                LlmCallRequestRecord::new(
                    url::Url::parse("https://example.invalid/v1/chat/completions")
                        .expect("测试 URL 必须有效"),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect_err("档案故障必须锁存并通知准入等待者");

        let second_error = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("档案 latch 必须立即唤醒活动许可等待")
            .expect("第二个请求任务不应 panic")
            .expect_err("等待者不得在档案失效后发送");
        assert!(matches!(
            second_error,
            LlmRequestError::Fatal(OpenAiChatCompletionError::CallReview {
                source,
                related: None,
            }) if source.path() == latched.path()
        ));
        assert!(
            matches!(server.requests.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "等待中的第二个请求不得到达 Provider"
        );

        server
            .release_first
            .take()
            .expect("首个响应应有释放端")
            .send(())
            .expect("首个响应应可释放");
        let first_response = first
            .await
            .expect("首个请求任务不应 panic")
            .expect("已经发送的首个调用仍应完成 Provider 记录与解析");
        recorder
            .record_disposition(
                call_site(1),
                LlmCallDisposition::lua_delivered(LlmParsedResponseMetadata::from(&first_response)),
            )
            .await
            .expect("已经发送的调用必须能够完成自己的记录生命周期");

        let first_archive = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用时应有 Run 根")
                .join("lua")
                .join("call-000001.md"),
        )
        .expect("首个调用档案应可读");
        assert!(first_archive.contains("provider_complete"));
        assert!(first_archive.contains("disposition_complete"));

        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn rate_and_active_waits_have_no_local_deadline() {
        let client = client_with_rate("http://127.0.0.1:1/v1/chat/completions", Map::new(), 60, 2);
        let lifecycle = LlmLifecycle::new();

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
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled
            ))
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
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled
            ))
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
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled
            ))
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
        let lifecycle = Arc::new(LlmLifecycle::new());
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
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled
            ))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_wins_and_releases_permit_when_active_admission_is_simultaneously_ready() {
        let lifecycle = Arc::new(LlmLifecycle::new());
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
            Err(LlmRequestError::Fatal(
                OpenAiChatCompletionError::WaitCancelled
            ))
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
                    call_site(1),
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
                "Retry-After: 3\r\nContent-Type: text/plain\r\n",
                "error-body-is-not-retained",
            )],
            false,
        );
        let client = client_with_rate(&server.endpoint, Map::new(), 60, 1);
        let executor = executor(1);

        assert!(matches!(
            executor
                .request(
                    &client,
                    call_site(1),
                    &[ChatMessage::new(ChatMessageRole::User, "content")]
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::HttpStatus { status: 429, .. },
                retry_after: Some(duration),
            }) if duration == Duration::from_secs(3)
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
                    call_site(1),
                    &[ChatMessage::new(ChatMessageRole::User, "content")]
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::Transport(_),
                retry_after: None,
            })
        ));
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn lifecycle_stop_and_idle_state_cannot_miss_a_notification() {
        let lifecycle = Arc::new(LlmLifecycle::new());
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
