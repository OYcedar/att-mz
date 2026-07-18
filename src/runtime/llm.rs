//! OpenAI-compatible Chat Completions 生产根。

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
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, watch};
use tokio::time::{Instant, timeout_at};
use url::Url;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientSemanticIdentity, LlmFinishReason, LlmRequestError,
    LlmRequestExecutor, LlmResponse, LlmUsage,
};

/// 一个可被不同引擎及 Lua 共享的受信 LLM Client。
pub(crate) struct OpenAiChatCompletionClient {
    url: Url,
    api_key: SecretString,
    model: String,
    timeout: Duration,
    parameters: Map<String, Value>,
    rate_limiter: Arc<DefaultDirectRateLimiter>,
}

impl OpenAiChatCompletionClient {
    pub(crate) fn new(
        url: Url,
        api_key: SecretString,
        model: impl Into<String>,
        timeout: Duration,
        rpm: NonZeroU32,
        burst: NonZeroU32,
        parameters: Map<String, Value>,
    ) -> Self {
        let quota = Quota::per_minute(rpm).allow_burst(burst);
        Self {
            url,
            api_key,
            model: model.into(),
            timeout,
            parameters,
            rate_limiter: Arc::new(RateLimiter::direct(quota)),
        }
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
            .field("timeout", &self.timeout)
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
    queue_capacity: usize,
    admission_timeout: Duration,
    connect_timeout: Duration,
    read_timeout: Duration,
    pool_idle_timeout: Duration,
    pool_max_idle_per_host: usize,
    proxy: LlmProxyConfiguration,
    tls: LlmTlsConfiguration,
}

impl OpenAiExecutorConfiguration {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max_active_requests: NonZeroUsize,
        queue_capacity: usize,
        admission_timeout: Duration,
        connect_timeout: Duration,
        read_timeout: Duration,
        pool_idle_timeout: Duration,
        pool_max_idle_per_host: usize,
        proxy: LlmProxyConfiguration,
        tls: LlmTlsConfiguration,
    ) -> Self {
        Self {
            max_active_requests,
            queue_capacity,
            admission_timeout,
            connect_timeout,
            read_timeout,
            pool_idle_timeout,
            pool_max_idle_per_host,
            proxy,
            tls,
        }
    }
}

/// 根构造无法建立安全的共享 HTTP Client。
#[derive(Debug)]
pub(crate) enum OpenAiExecutorBuildError {
    CapacityOverflow,
    InvalidProxy(reqwest::Error),
    InvalidCertificate(reqwest::Error),
    BuildClient(reqwest::Error),
}

impl fmt::Display for OpenAiExecutorBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityOverflow => formatter.write_str("LLM active + queue 容量溢出"),
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
            Self::CapacityOverflow => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiChatCompletionExecutor {
    client: Client,
    total_capacity: Arc<Semaphore>,
    active_capacity: Arc<Semaphore>,
    admission_timeout: Duration,
    lifecycle: Arc<LlmLifecycle>,
}

impl OpenAiChatCompletionExecutor {
    pub(crate) fn new(
        configuration: OpenAiExecutorConfiguration,
    ) -> Result<Self, OpenAiExecutorBuildError> {
        let total_capacity = configuration
            .max_active_requests
            .get()
            .checked_add(configuration.queue_capacity)
            .ok_or(OpenAiExecutorBuildError::CapacityOverflow)?;

        let mut builder = Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(configuration.connect_timeout)
            .read_timeout(configuration.read_timeout)
            .pool_idle_timeout(configuration.pool_idle_timeout)
            .pool_max_idle_per_host(configuration.pool_max_idle_per_host);
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
            total_capacity: Arc::new(Semaphore::new(total_capacity)),
            active_capacity: Arc::new(Semaphore::new(configuration.max_active_requests.get())),
            admission_timeout: configuration.admission_timeout,
            lifecycle: Arc::new(LlmLifecycle::new()),
        })
    }

    /// 停止新请求并等待已准入请求归还所有许可。
    pub(crate) async fn shutdown(&self) {
        self.lifecycle.stop_accepting();
        self.lifecycle.wait_until_idle().await;
    }

    async fn execute_request(
        &self,
        client: &OpenAiChatCompletionClient,
        messages: &[ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<OpenAiChatCompletionError>> {
        let request_body = serialize_request(client, messages).map_err(LlmRequestError::Fatal)?;

        let total_permit = Arc::clone(&self.total_capacity)
            .try_acquire_owned()
            .map_err(|_| retryable(OpenAiChatCompletionError::QueueFull))?;
        let job = self
            .lifecycle
            .register()
            .ok_or_else(|| LlmRequestError::Fatal(OpenAiChatCompletionError::ShuttingDown))?;
        let deadline = Instant::now() + self.admission_timeout;

        wait_for_rate(client, &self.lifecycle, deadline).await?;
        let active_permit =
            wait_for_active(Arc::clone(&self.active_capacity), &self.lifecycle, deadline).await?;

        let request = self
            .client
            .post(client.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .timeout(client.timeout)
            .bearer_auth(client.api_key.expose_secret())
            .body(request_body);

        let response = request.send().await.map_err(classify_transport_error)?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER));
        if status != StatusCode::OK {
            let error = OpenAiChatCompletionError::HttpStatus {
                status: status.as_u16(),
            };
            drop(active_permit);
            drop(total_permit);
            drop(job);
            return if is_retryable_status(status) {
                Err(LlmRequestError::Retryable {
                    source: error,
                    retry_after,
                })
            } else {
                Err(LlmRequestError::Fatal(error))
            };
        }

        validate_json_content_type(response.headers().get(CONTENT_TYPE))
            .map_err(LlmRequestError::Fatal)?;
        let provider_request_id = response
            .headers()
            .get("x-request-id")
            .map(|value| {
                value.to_str().map(str::to_owned).map_err(|_| {
                    LlmRequestError::Fatal(OpenAiChatCompletionError::InvalidProviderRequestId)
                })
            })
            .transpose()?;
        let response_body = response.bytes().await.map_err(classify_transport_error)?;
        let parsed = parse_success_response(&response_body, provider_request_id)?;

        drop(active_permit);
        drop(total_permit);
        drop(job);
        Ok(parsed)
    }
}

impl fmt::Debug for OpenAiChatCompletionExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiChatCompletionExecutor")
            .field("admission_timeout", &self.admission_timeout)
            .finish_non_exhaustive()
    }
}

impl LlmRequestExecutor for OpenAiChatCompletionExecutor {
    type Client = OpenAiChatCompletionClient;
    type Error = OpenAiChatCompletionError;

    async fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        self.execute_request(client, messages).await
    }
}

#[derive(Debug)]
pub(crate) enum OpenAiChatCompletionError {
    ShuttingDown,
    QueueFull,
    AdmissionTimeout { stage: AdmissionStage },
    AdmissionClosed,
    SerializeRequest(serde_json::Error),
    Transport(reqwest::Error),
    HttpStatus { status: u16 },
    MissingJsonContentType,
    InvalidJsonContentType,
    InvalidProviderRequestId,
    ParseResponse(serde_json::Error),
    InvalidResponseWire { reason: &'static str },
}

impl fmt::Display for OpenAiChatCompletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("LLM 根正在关闭"),
            Self::QueueFull => formatter.write_str("LLM 请求队列已满"),
            Self::AdmissionTimeout { stage } => write!(formatter, "LLM {stage} 准入超时"),
            Self::AdmissionClosed => formatter.write_str("LLM 活动请求通道已关闭"),
            Self::SerializeRequest(_) => formatter.write_str("无法序列化 LLM 请求"),
            Self::Transport(_) => formatter.write_str("LLM HTTP 传输失败"),
            Self::HttpStatus { status } => write!(formatter, "LLM HTTP 状态 {status}"),
            Self::MissingJsonContentType => {
                formatter.write_str("LLM 成功响应缺少 JSON Content-Type")
            }
            Self::InvalidJsonContentType => {
                formatter.write_str("LLM 成功响应的 Content-Type 不是 JSON")
            }
            Self::InvalidProviderRequestId => {
                formatter.write_str("LLM x-request-id 响应头不是有效文本")
            }
            Self::ParseResponse(_) => formatter.write_str("LLM 成功响应不是有效 JSON"),
            Self::InvalidResponseWire { reason } => {
                write!(
                    formatter,
                    "LLM 成功响应不符合 Chat Completions 契约：{reason}"
                )
            }
        }
    }
}

impl Error for OpenAiChatCompletionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeRequest(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::ParseResponse(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionStage {
    Rate,
    Active,
}

impl fmt::Display for AdmissionStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rate => formatter.write_str("速率"),
            Self::Active => formatter.write_str("活动容量"),
        }
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
    deadline: Instant,
) -> Result<(), LlmRequestError<OpenAiChatCompletionError>> {
    if !lifecycle.is_accepting() {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::ShuttingDown,
        ));
    }
    let stopped = lifecycle.wait_for_stop();
    tokio::pin!(stopped);
    let ready = client.rate_limiter.until_ready();
    tokio::pin!(ready);
    let admitted = timeout_at(deadline, async {
        tokio::select! {
            () = &mut ready => true,
            () = &mut stopped => false,
        }
    })
    .await
    .map_err(|_| {
        retryable(OpenAiChatCompletionError::AdmissionTimeout {
            stage: AdmissionStage::Rate,
        })
    })?;
    if admitted {
        Ok(())
    } else {
        Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::ShuttingDown,
        ))
    }
}

async fn wait_for_active(
    semaphore: Arc<Semaphore>,
    lifecycle: &LlmLifecycle,
    deadline: Instant,
) -> Result<tokio::sync::OwnedSemaphorePermit, LlmRequestError<OpenAiChatCompletionError>> {
    if !lifecycle.is_accepting() {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::ShuttingDown,
        ));
    }
    let stopped = lifecycle.wait_for_stop();
    tokio::pin!(stopped);
    let permit = semaphore.acquire_owned();
    tokio::pin!(permit);
    timeout_at(deadline, async {
        tokio::select! {
            result = &mut permit => result
                .map_err(|_| LlmRequestError::Fatal(OpenAiChatCompletionError::AdmissionClosed)),
            () = &mut stopped => Err(LlmRequestError::Fatal(OpenAiChatCompletionError::ShuttingDown)),
        }
    })
    .await
    .map_err(|_| retryable(OpenAiChatCompletionError::AdmissionTimeout {
        stage: AdmissionStage::Active,
    }))?
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

fn validate_json_content_type(
    value: Option<&reqwest::header::HeaderValue>,
) -> Result<(), OpenAiChatCompletionError> {
    let value = value.ok_or(OpenAiChatCompletionError::MissingJsonContentType)?;
    let value = value
        .to_str()
        .map_err(|_| OpenAiChatCompletionError::InvalidJsonContentType)?;
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type == "application/json"
        || (media_type.starts_with("application/") && media_type.ends_with("+json"))
    {
        Ok(())
    } else {
        Err(OpenAiChatCompletionError::InvalidJsonContentType)
    }
}

#[derive(Deserialize)]
struct ChatCompletionResponseWire {
    id: String,
    choices: Vec<ChatCompletionChoiceWire>,
    usage: Option<ChatCompletionUsageWire>,
}

#[derive(Deserialize)]
struct ChatCompletionChoiceWire {
    index: u64,
    message: ChatCompletionMessageWire,
    finish_reason: String,
}

#[derive(Deserialize)]
struct ChatCompletionMessageWire {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatCompletionUsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

fn parse_success_response(
    body: &[u8],
    provider_request_id: Option<String>,
) -> Result<LlmResponse, LlmRequestError<OpenAiChatCompletionError>> {
    let wire: ChatCompletionResponseWire = serde_json::from_slice(body).map_err(|source| {
        LlmRequestError::Fatal(OpenAiChatCompletionError::ParseResponse(source))
    })?;
    let [choice] = wire.choices.as_slice() else {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::InvalidResponseWire {
                reason: "choices 必须恰好包含一项",
            },
        ));
    };
    if choice.index != 0 {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::InvalidResponseWire {
                reason: "choice index 必须为 0",
            },
        ));
    }
    if choice.message.role != "assistant" {
        return Err(LlmRequestError::Fatal(
            OpenAiChatCompletionError::InvalidResponseWire {
                reason: "message role 必须为 assistant",
            },
        ));
    }
    let finish_reason = match choice.finish_reason.as_str() {
        "stop" => LlmFinishReason::Stop,
        "length" => LlmFinishReason::Length,
        "content_filter" => LlmFinishReason::ContentFilter,
        other => LlmFinishReason::Other(other.to_owned()),
    };
    let usage = wire.usage.map(|usage| {
        LlmUsage::new(
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
        )
    });
    Ok(LlmResponse::new(
        choice.message.content.clone(),
        finish_reason,
        provider_request_id,
        wire.id,
        usage,
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
    use std::sync::mpsc;
    use std::thread;

    use super::*;

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
            Duration::from_secs(2),
            non_zero_u32(rpm),
            non_zero_u32(burst),
            parameters,
        )
    }

    fn executor(max_active_requests: usize, queue_capacity: usize) -> OpenAiChatCompletionExecutor {
        executor_with_admission_timeout(max_active_requests, queue_capacity, Duration::from_secs(2))
    }

    fn executor_with_admission_timeout(
        max_active_requests: usize,
        queue_capacity: usize,
        admission_timeout: Duration,
    ) -> OpenAiChatCompletionExecutor {
        OpenAiChatCompletionExecutor::new(OpenAiExecutorConfiguration::new(
            non_zero_usize(max_active_requests),
            queue_capacity,
            admission_timeout,
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(30),
            2,
            LlmProxyConfiguration::Disabled,
            LlmTlsConfiguration::default(),
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
            Duration::from_secs(2),
            non_zero_u32(60),
            non_zero_u32(2),
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
            Duration::from_secs(2),
            non_zero_u32(60),
            non_zero_u32(2),
            extra.clone(),
        );
        let operationally_different = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("different-secret"),
            "model-a",
            Duration::from_secs(90),
            non_zero_u32(1),
            non_zero_u32(1),
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
            Duration::from_secs(2),
            non_zero_u32(60),
            non_zero_u32(2),
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
            Duration::from_secs(2),
            non_zero_u32(60),
            non_zero_u32(2),
            numerically_equivalent_extra,
        );
        let different_model = OpenAiChatCompletionClient::new(
            Url::parse("https://example.com/v1/chat/completions").unwrap(),
            SecretString::from("first-secret"),
            "model-b",
            Duration::from_secs(2),
            non_zero_u32(60),
            non_zero_u32(2),
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
        assert_eq!(response.provider_response_id(), "chatcmpl-response");
        assert_eq!(response.usage(), Some(LlmUsage::new(3, 2, 5)));
    }

    #[test]
    fn response_id_accepts_every_json_string() {
        let response = parse_success_response(
            br#"{
                "id":"",
                "choices":[{
                    "index":0,
                    "message":{"role":"assistant","content":"[]"},
                    "finish_reason":"stop"
                }]
            }"#,
            Some("http-request".to_owned()),
        )
        .expect("正文 id 的契约只要求 JSON 字符串");

        assert_eq!(response.provider_response_id(), "");
    }

    #[test]
    fn successful_wire_requires_one_assistant_choice() {
        for body in [
            br#"{"id":"r","choices":[]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"},{"index":1,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":1,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"user","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
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
    fn successful_wire_rejects_required_field_type_drift_and_partial_usage() {
        for body in [
            br#"{"choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":1,"choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":[]},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":null}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3.5}}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_success_response(body, None),
                Err(LlmRequestError::Fatal(_))
            ));
        }

        for body in [
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}],"usage":null}"#.as_slice(),
        ] {
            assert!(parse_success_response(body, None).is_ok());
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
        let executor = executor(1, 1);

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
        assert_eq!(response.provider_request_id(), Some("request-header"));
        assert_eq!(response.provider_response_id(), "response-body");
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
    async fn api_key_is_sent_exactly_once() {
        let server = spawn_test_server(
            vec![success_response("response-body", "request-header", "[]")],
            false,
        );
        let mut client = client(&server.endpoint, Map::new());
        client.api_key = SecretString::from("exact-secret");
        let executor = executor(1, 0);

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
    async fn active_and_queue_capacity_apply_before_a_third_request() {
        let mut server = spawn_test_server(
            vec![
                success_response("response-1", "request-1", "[]"),
                success_response("response-2", "request-2", "[]"),
            ],
            true,
        );
        let client = Arc::new(client_with_rate(&server.endpoint, Map::new(), 60_000, 3));
        let executor = executor(1, 1);
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
        tokio::time::timeout(Duration::from_secs(1), async {
            while executor.total_capacity.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("第二个请求应占用排队容量");

        let third = executor
            .request(
                client.as_ref(),
                &[ChatMessage::new(ChatMessageRole::User, "third")],
            )
            .await;
        assert!(matches!(
            third,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::QueueFull,
                retry_after: None,
            })
        ));

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
        executor.shutdown().await;
        server.worker.join().expect("测试服务器应正常退出");
    }

    #[tokio::test]
    async fn rate_burst_and_admission_deadlines_are_enforced() {
        let client = client_with_rate("http://127.0.0.1:1/v1/chat/completions", Map::new(), 60, 2);
        let lifecycle = LlmLifecycle::new();

        wait_for_rate(
            &client,
            &lifecycle,
            Instant::now() + Duration::from_millis(50),
        )
        .await
        .expect("burst 内第一个请求应立即准入");
        wait_for_rate(
            &client,
            &lifecycle,
            Instant::now() + Duration::from_millis(50),
        )
        .await
        .expect("burst 内第二个请求应立即准入");
        assert!(matches!(
            wait_for_rate(
                &client,
                &lifecycle,
                Instant::now() + Duration::from_millis(10)
            )
            .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::AdmissionTimeout {
                    stage: AdmissionStage::Rate,
                },
                retry_after: None,
            })
        ));

        let unavailable = Arc::new(Semaphore::new(0));
        assert!(matches!(
            wait_for_active(
                unavailable,
                &lifecycle,
                Instant::now() + Duration::from_millis(10)
            )
            .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::AdmissionTimeout {
                    stage: AdmissionStage::Active,
                },
                retry_after: None,
            })
        ));
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
        let executor = executor_with_admission_timeout(1, 0, Duration::from_secs(1));
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
        assert_eq!(executor.total_capacity.available_permits(), 0);
        assert_eq!(executor.active_capacity.available_permits(), 0);

        request.abort();
        let _ = request.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if executor.total_capacity.available_permits() == 1
                    && executor.active_capacity.available_permits() == 1
                {
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
        let executor = executor(1, 0);

        assert!(matches!(
            executor
                .request(
                    &client,
                    &[ChatMessage::new(ChatMessageRole::User, "content")]
                )
                .await,
            Err(LlmRequestError::Retryable {
                source: OpenAiChatCompletionError::HttpStatus { status: 429 },
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
        let executor = executor(1, 0);

        assert!(matches!(
            executor
                .request(
                    &client,
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
