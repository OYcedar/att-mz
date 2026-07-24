//! 每次实际 LLM HTTP 调用的人类可读审阅档案。
//!
//! 本模块只拥有审阅文件的路径、呈现、阶段持久化和失败门禁。调用归属由上游以
//! [`LlmCallSite`] 建立；HTTP 根提供最终请求与原始供应商结果；Standard 或 Lua
//! 提供自己拥有的验收结论。档案不参与译文恢复、重放或业务状态判断。

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use time::OffsetDateTime;
use tokio::sync::watch;
use url::Url;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic,
};
use crate::i18n::UiLocale;
use crate::llm::{LlmCallSite, LlmResponse};
use crate::observability::RunId;

use super::windows::{
    PinnedPath, WindowsFsError, create_directories_without_reparse, pin_directory_without_reparse,
};

const CALLS_DIRECTORY: &str = "llm-calls";
const STANDARD_DIRECTORY: &str = "standard";
const LUA_DIRECTORY: &str = "lua";

/// 一次 Translate 运行内所有调用共同显示的受信归属。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmCallReviewContext {
    engine: String,
    project: String,
    profile: String,
    client: String,
}

impl LlmCallReviewContext {
    pub(crate) fn new(
        engine: impl Into<String>,
        project: impl Into<String>,
        profile: impl Into<String>,
        client: impl Into<String>,
    ) -> Self {
        Self {
            engine: engine.into(),
            project: project.into(),
            profile: profile.into(),
            client: client.into(),
        }
    }
}

/// HTTP 根即将发送的最终有效请求事实。
///
/// `body` 必须是将交给 HTTP Client 的同一份 JSON 字节；模块只为人类阅读重新缩进，
/// 不从模板、配置或消息对象反推请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmCallRequestRecord {
    endpoint: Url,
    body: Vec<u8>,
}

impl LlmCallRequestRecord {
    pub(crate) fn new(endpoint: Url, body: Vec<u8>) -> Self {
        Self { endpoint, body }
    }
}

/// HTTP 响应中允许进入敏感审阅档案的选定 Header。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LlmProviderHeaders {
    content_type: Option<String>,
    request_id: Option<String>,
    retry_after: Option<String>,
}

impl LlmProviderHeaders {
    pub(crate) fn new(
        content_type: Option<String>,
        request_id: Option<String>,
        retry_after: Option<String>,
    ) -> Self {
        Self {
            content_type,
            request_id,
            retry_after,
        }
    }
}

/// 供应商阶段的原始结果；错误文本由安全诊断负责，不能借此进入档案。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LlmProviderOutcome {
    Response {
        status: u16,
        headers: LlmProviderHeaders,
        body: Vec<u8>,
    },
    ResponseNotReceived,
    BodyReadFailed {
        status: Option<u16>,
        headers: LlmProviderHeaders,
    },
}

/// HTTP 根在释放活动请求许可后交给档案的供应商阶段事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmProviderRecord {
    elapsed: Duration,
    outcome: LlmProviderOutcome,
}

impl LlmProviderRecord {
    pub(crate) fn response(
        elapsed: Duration,
        status: u16,
        headers: LlmProviderHeaders,
        body: Vec<u8>,
    ) -> Self {
        Self {
            elapsed,
            outcome: LlmProviderOutcome::Response {
                status,
                headers,
                body,
            },
        }
    }

    pub(crate) fn response_not_received(elapsed: Duration) -> Self {
        Self {
            elapsed,
            outcome: LlmProviderOutcome::ResponseNotReceived,
        }
    }

    pub(crate) fn body_read_failed(
        elapsed: Duration,
        status: Option<u16>,
        headers: LlmProviderHeaders,
    ) -> Self {
        Self {
            elapsed,
            outcome: LlmProviderOutcome::BodyReadFailed { status, headers },
        }
    }
}

/// 已成功解析的 Chat Completions 响应元数据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmParsedResponseMetadata {
    provider_request_id: Option<String>,
    provider_response_id: Option<String>,
    finish_reason: String,
    usage: Option<LlmParsedResponseUsage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LlmParsedResponseUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

impl From<&LlmResponse> for LlmParsedResponseMetadata {
    fn from(response: &LlmResponse) -> Self {
        Self {
            provider_request_id: response.provider_request_id().map(str::to_owned),
            provider_response_id: response.provider_response_id().map(str::to_owned),
            finish_reason: response.finish_reason().to_string(),
            usage: response.usage().map(|usage| LlmParsedResponseUsage {
                prompt_tokens: usage.prompt_tokens(),
                completion_tokens: usage.completion_tokens(),
                total_tokens: usage.total_tokens(),
            }),
        }
    }
}

/// Standard 对一个模型输出 ID 的拒绝事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmRejectedOutput {
    id: usize,
    reason_code: String,
    detail: Option<String>,
}

impl LlmRejectedOutput {
    pub(crate) fn new(id: usize, reason_code: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            id,
            reason_code: reason_code.into(),
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LlmStandardDispositionOutcome {
    Complete,
    Partial,
    Unavailable,
}

/// Standard 已完成业务验收、但尚未声称 SQLite 已提交的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmStandardDisposition {
    outcome: LlmStandardDispositionOutcome,
    response: LlmParsedResponseMetadata,
    accepted_ids: Vec<usize>,
    rejected: Vec<LlmRejectedOutput>,
}

impl LlmStandardDisposition {
    pub(crate) fn new(
        outcome: LlmStandardDispositionOutcome,
        response: LlmParsedResponseMetadata,
        accepted_ids: Vec<usize>,
        rejected: Vec<LlmRejectedOutput>,
    ) -> Self {
        Self {
            outcome,
            response,
            accepted_ids,
            rejected,
        }
    }
}

/// ATT 对已经持久化的供应商结果作出的终态处置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LlmCallDisposition {
    Standard(LlmStandardDisposition),
    LuaDelivered {
        response: LlmParsedResponseMetadata,
    },
    Rejected {
        code: String,
        response: Option<LlmParsedResponseMetadata>,
    },
}

impl LlmCallDisposition {
    pub(crate) fn lua_delivered(response: LlmParsedResponseMetadata) -> Self {
        Self::LuaDelivered { response }
    }

    pub(crate) fn rejected(
        code: impl Into<String>,
        response: Option<LlmParsedResponseMetadata>,
    ) -> Self {
        Self::Rejected {
            code: code.into(),
            response,
        }
    }
}

/// 一次审阅档案持久化失败。错误可克隆，因此全局门禁可以保留同一首个失败，
/// 而不是用泛化的“已经失败”覆盖精确路径、操作和 OS 原因。
#[derive(Clone, Debug)]
pub(crate) struct LlmCallReviewError {
    operation: &'static str,
    path: PathBuf,
    site: Option<LlmCallSite>,
    cause: Arc<LlmCallReviewErrorCause>,
}

#[derive(Debug)]
enum LlmCallReviewErrorCause {
    Io(io::Error),
    Windows(WindowsFsError),
    BlockingWorker(tokio::task::JoinError),
    InvalidState(&'static str),
}

impl LlmCallReviewError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self {
            operation,
            path: path.to_path_buf(),
            site: None,
            cause: Arc::new(LlmCallReviewErrorCause::Io(source)),
        }
    }

    fn windows(operation: &'static str, path: &Path, source: WindowsFsError) -> Self {
        Self {
            operation,
            path: path.to_path_buf(),
            site: None,
            cause: Arc::new(LlmCallReviewErrorCause::Windows(source)),
        }
    }

    fn blocking_worker(
        operation: &'static str,
        path: &Path,
        source: tokio::task::JoinError,
    ) -> Self {
        Self {
            operation,
            path: path.to_path_buf(),
            site: None,
            cause: Arc::new(LlmCallReviewErrorCause::BlockingWorker(source)),
        }
    }

    fn invalid_state(operation: &'static str, path: &Path, reason: &'static str) -> Self {
        Self {
            operation,
            path: path.to_path_buf(),
            site: None,
            cause: Arc::new(LlmCallReviewErrorCause::InvalidState(reason)),
        }
    }

    fn with_site(mut self, site: LlmCallSite) -> Self {
        self.site = Some(site);
        self
    }

    /// 返回产生首个持久化失败的真实调用归属。等待者不得把自己的归属
    /// 补写到这份错误上，否则路径与任务身份会互相矛盾。
    pub(crate) const fn site(&self) -> Option<LlmCallSite> {
        self.site
    }

    #[cfg(test)]
    pub(crate) const fn operation(&self) -> &'static str {
        self.operation
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn raw_os_error(&self) -> Option<i32> {
        match self.cause.as_ref() {
            LlmCallReviewErrorCause::Io(source) => source.raw_os_error(),
            LlmCallReviewErrorCause::Windows(WindowsFsError::Io { source, .. }) => {
                source.raw_os_error()
            }
            LlmCallReviewErrorCause::Windows(_)
            | LlmCallReviewErrorCause::BlockingWorker(_)
            | LlmCallReviewErrorCause::InvalidState(_) => None,
        }
    }

    /// 建立不含请求、响应或任意错误展示文本的安全诊断投影。
    pub(crate) fn safe_diagnostic(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        let diagnostic = match self.cause.as_ref() {
            LlmCallReviewErrorCause::Io(source) => SafeDiagnostic::io(
                DiagnosticCode::FileSystemOperation,
                stage,
                DiagnosticSubject::path(&self.path),
                self.operation,
                source,
                impact,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            LlmCallReviewErrorCause::Windows(source) => source.safe_diagnostic(
                DiagnosticCode::FileSystemOperation,
                stage,
                impact,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            LlmCallReviewErrorCause::BlockingWorker(_) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::path(&self.path),
                DiagnosticReason::failure(DiagnosticFailureKind::WorkerPanicked),
                impact,
                DiagnosticAction::Retry,
            ),
            LlmCallReviewErrorCause::InvalidState(_) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::path(&self.path),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                impact,
                DiagnosticAction::ReportBug,
            ),
        };
        let diagnostic = diagnostic.with_recovery(RecoveryFact::path(&self.path));
        if matches!(
            self.operation,
            "record_provider"
                | "write_provider"
                | "sync_provider"
                | "record_disposition"
                | "write_disposition"
                | "sync_disposition"
                | "finish_call"
        ) {
            diagnostic.with_recovery(RecoveryFact::component(
                "llm_request_may_have_been_sent=true",
            ))
        } else {
            diagnostic
        }
    }
}

impl fmt::Display for LlmCallReviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "LLM 调用审阅档案操作 {} 在 {} 失败：",
            self.operation,
            self.path.display()
        )?;
        match self.cause.as_ref() {
            LlmCallReviewErrorCause::Io(source) => source.fmt(formatter),
            LlmCallReviewErrorCause::Windows(source) => source.fmt(formatter),
            LlmCallReviewErrorCause::BlockingWorker(source) => source.fmt(formatter),
            LlmCallReviewErrorCause::InvalidState(reason) => formatter.write_str(reason),
        }
    }
}

impl Error for LlmCallReviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.cause.as_ref() {
            LlmCallReviewErrorCause::Io(source) => Some(source),
            LlmCallReviewErrorCause::Windows(source) => Some(source),
            LlmCallReviewErrorCause::BlockingWorker(source) => Some(source),
            LlmCallReviewErrorCause::InvalidState(_) => None,
        }
    }
}

/// 可克隆的当前运行审阅档案根。
///
/// Disabled 是完全 no-op；Enabled 的第一个失败会阻止新调用。已经完成请求阶段并
/// 进入 HTTP 的调用仍可写完自己的 Provider 与处置阶段，调用方随后通过 [`Self::failure`]
/// 阻止业务结果提交。
#[derive(Clone, Default)]
pub(crate) struct LlmCallRecorder {
    inner: Option<Arc<LlmCallRecorderInner>>,
}

struct LlmCallRecorderInner {
    run_id: RunId,
    locale: UiLocale,
    context: LlmCallReviewContext,
    store: Arc<dyn LlmCallReviewStore>,
    active: Mutex<Vec<ActiveCall>>,
    admission: Mutex<SendAdmission>,
    failure_signal: watch::Sender<Option<LlmCallReviewError>>,
    #[cfg(test)]
    injected_operation: Mutex<Option<&'static str>>,
    #[cfg(test)]
    send_authorization_pause: Mutex<Option<SendAuthorizationPause>>,
}

#[derive(Default)]
struct SendAdmission {
    failure: Option<LlmCallReviewError>,
}

#[cfg(test)]
struct SendAuthorizationPause {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

struct ActiveCall {
    site: LlmCallSite,
    path: PathBuf,
    state: Arc<Mutex<ActiveCallState>>,
}

struct ActiveCallState {
    file: Box<dyn LlmCallReviewFile>,
    phase: ActiveCallPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveCallPhase {
    Request,
    SendAuthorized,
    Provider,
    Disposition,
}

trait LlmCallReviewStore: Send + Sync {
    fn run_root(&self) -> &Path;

    fn create_call_file(
        &self,
        path: &Path,
        request: &[u8],
    ) -> Result<Box<dyn LlmCallReviewFile>, LlmCallReviewError>;
}

trait LlmCallReviewFile: Send {
    fn append_and_sync(
        &mut self,
        path: &Path,
        stage: ReviewStage,
        content: &[u8],
    ) -> Result<(), LlmCallReviewError>;
}

#[derive(Clone, Copy)]
enum ReviewStage {
    Provider,
    Disposition,
}

impl ReviewStage {
    const fn write_operation(self) -> &'static str {
        match self {
            Self::Provider => "write_provider",
            Self::Disposition => "write_disposition",
        }
    }

    const fn sync_operation(self) -> &'static str {
        match self {
            Self::Provider => "sync_provider",
            Self::Disposition => "sync_disposition",
        }
    }
}

struct SystemLlmCallReviewStore {
    run_root: PinnedPath,
}

struct SystemLlmCallReviewFile {
    file: File,
    _pinned_parent: PinnedPath,
}

impl LlmCallReviewStore for SystemLlmCallReviewStore {
    fn run_root(&self) -> &Path {
        self.run_root.resolved_path()
    }

    fn create_call_file(
        &self,
        path: &Path,
        request: &[u8],
    ) -> Result<Box<dyn LlmCallReviewFile>, LlmCallReviewError> {
        let parent = path.parent().ok_or_else(|| {
            LlmCallReviewError::invalid_state("create_call_parent", path, "调用文件路径没有父目录")
        })?;
        let pinned_parent = create_directories_without_reparse(parent)
            .map_err(|source| LlmCallReviewError::windows("create_call_parent", parent, source))?;
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)
            .map_err(|source| LlmCallReviewError::io("create_new", path, source))?;
        file.write_all(request)
            .map_err(|source| LlmCallReviewError::io("write_request", path, source))?;
        file.sync_data()
            .map_err(|source| LlmCallReviewError::io("sync_request", path, source))?;
        Ok(Box::new(SystemLlmCallReviewFile {
            file,
            _pinned_parent: pinned_parent,
        }))
    }
}

impl LlmCallReviewFile for SystemLlmCallReviewFile {
    fn append_and_sync(
        &mut self,
        path: &Path,
        stage: ReviewStage,
        content: &[u8],
    ) -> Result<(), LlmCallReviewError> {
        self.file
            .write_all(content)
            .map_err(|source| LlmCallReviewError::io(stage.write_operation(), path, source))?;
        self.file
            .sync_data()
            .map_err(|source| LlmCallReviewError::io(stage.sync_operation(), path, source))
    }
}

impl LlmCallRecorder {
    pub(crate) const fn disabled() -> Self {
        Self { inner: None }
    }

    /// 独占建立 `<workspace>/llm-calls/<run-id>`；存在同名 Run 目录即硬失败。
    pub(crate) async fn start(
        workspace: PathBuf,
        run_id: RunId,
        locale: UiLocale,
        context: LlmCallReviewContext,
    ) -> Result<Self, LlmCallReviewError> {
        let intended_root = workspace.join(CALLS_DIRECTORY).join(run_id.to_string());
        let store = run_blocking("create_run_root", intended_root.clone(), move || {
            create_system_store(&workspace, run_id)
        })
        .await?;
        Ok(Self {
            inner: Some(Arc::new(LlmCallRecorderInner {
                run_id,
                locale,
                context,
                store: Arc::new(store),
                active: Mutex::new(Vec::new()),
                admission: Mutex::new(SendAdmission::default()),
                failure_signal: watch::channel(None).0,
                #[cfg(test)]
                injected_operation: Mutex::new(None),
                #[cfg(test)]
                send_authorization_pause: Mutex::new(None),
            })),
        })
    }

    pub(crate) const fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    #[cfg(test)]
    pub(crate) fn run_root(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.store.run_root())
    }

    pub(crate) fn failure(&self) -> Option<LlmCallReviewError> {
        self.inner.as_ref().and_then(|inner| inner.failure())
    }

    /// 等待本轮首个档案故障；关闭记录时永远不就绪。
    pub(crate) async fn wait_for_failure(&self) -> LlmCallReviewError {
        let Some(inner) = &self.inner else {
            return std::future::pending().await;
        };
        let mut failure = inner.failure_signal.subscribe();
        loop {
            if let Some(error) = failure.borrow_and_update().clone() {
                return error;
            }
            failure
                .changed()
                .await
                .expect("调用档案故障 Sender 与 Recorder 同生命周期");
        }
    }

    /// 在调用方已经取得本地准入许可、但尚未发送 HTTP 请求时持久化请求。
    pub(crate) async fn record_request(
        &self,
        site: LlmCallSite,
        request: LlmCallRequestRecord,
    ) -> Result<(), LlmCallReviewError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        inner.ensure_healthy()?;
        let path = inner.call_path(site);
        #[cfg(test)]
        if let Some(error) =
            inner.take_injected_failure(site, &path, &["write_request", "sync_request"])
        {
            return Err(inner.latch(error));
        }
        let markdown = render_request(
            inner.locale,
            inner.run_id,
            &inner.context,
            site,
            &request,
            OffsetDateTime::now_utc(),
        );
        let store = Arc::clone(&inner.store);
        let write_path = path.clone();
        let file = match run_blocking("record_request", path.clone(), move || {
            store.create_call_file(&write_path, markdown.as_bytes())
        })
        .await
        {
            Ok(file) => file,
            Err(error) => return Err(inner.latch(error.with_site(site))),
        };

        // 另一条并发调用可能在本次 sync_data 期间触发了全局门禁。此时不得发送。
        inner.ensure_healthy()?;
        let mut active = lock_unpoisoned(&inner.active);
        if active.iter().any(|record| record.site == site) {
            let error = LlmCallReviewError::invalid_state(
                "register_call",
                &path,
                "同一调用归属已经处于记录中",
            )
            .with_site(site);
            drop(active);
            return Err(inner.latch(error));
        }
        active.push(ActiveCall {
            site,
            path,
            state: Arc::new(Mutex::new(ActiveCallState {
                file,
                phase: ActiveCallPhase::Request,
            })),
        });
        Ok(())
    }

    /// 把已经完整同步请求阶段的调用线性化为“已经发出”。
    ///
    /// 本操作与首个全局档案故障共享同一同步边界：先取得准入的调用可以继续完成
    /// HTTP 与档案生命周期；故障先锁存时，本调用必须在 Provider 收到请求前停止。
    pub(crate) fn authorize_send(&self, site: LlmCallSite) -> Result<(), LlmCallReviewError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        #[cfg(test)]
        if let Some(pause) = lock_unpoisoned(&inner.send_authorization_pause).take() {
            pause
                .entered
                .send(())
                .expect("测试发送准入暂停点必须仍有接收方");
            pause.release.recv().expect("测试发送准入暂停点必须被释放");
        }

        let active = inner.active_call(site, "authorize_send")?;
        let admission = lock_unpoisoned(&inner.admission);
        if let Some(error) = &admission.failure {
            return Err(error.clone());
        }
        let mut state = lock_unpoisoned(&active.state);
        if state.phase != ActiveCallPhase::Request {
            let error = LlmCallReviewError::invalid_state(
                "authorize_send",
                &active.path,
                "调用没有等待发送准入的已同步请求阶段",
            )
            .with_site(site);
            drop(state);
            drop(admission);
            return Err(inner.latch(error));
        }
        state.phase = ActiveCallPhase::SendAuthorized;
        Ok(())
    }

    /// 持久化完整供应商结果；成功后仍保留文件，等待根解析以及 Standard/Lua 处置。
    pub(crate) async fn record_provider(
        &self,
        site: LlmCallSite,
        provider: LlmProviderRecord,
    ) -> Result<(), LlmCallReviewError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let active = inner.active_call(site, "record_provider")?;
        #[cfg(test)]
        if let Some(error) =
            inner.take_injected_failure(site, &active.path, &["write_provider", "sync_provider"])
        {
            return Err(inner.latch(error));
        }
        let markdown = render_provider(inner.locale, &provider, OffsetDateTime::now_utc());
        if let Err(error) =
            append_stage(active.state, active.path, ReviewStage::Provider, markdown).await
        {
            let error = error.with_site(site);
            inner.latch(error.clone());
            return Err(error);
        }
        Ok(())
    }

    /// 持久化不能继续解析的供应商终态，并关闭当前调用文件。
    pub(crate) async fn record_terminal_provider(
        &self,
        site: LlmCallSite,
        provider: LlmProviderRecord,
        code: impl Into<String>,
    ) -> Result<(), LlmCallReviewError> {
        self.record_provider(site, provider).await?;
        self.record_disposition(site, LlmCallDisposition::rejected(code, None))
            .await
    }

    /// 在业务结果可以继续进入 Lua 或 Standard 提交边界前，追加并同步处置终态。
    pub(crate) async fn record_disposition(
        &self,
        site: LlmCallSite,
        disposition: LlmCallDisposition,
    ) -> Result<(), LlmCallReviewError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let active = inner.active_call(site, "record_disposition")?;
        #[cfg(test)]
        if let Some(error) = inner.take_injected_failure(
            site,
            &active.path,
            &["write_disposition", "sync_disposition"],
        ) {
            return Err(inner.latch(error));
        }
        let markdown = render_disposition(inner.locale, &disposition, OffsetDateTime::now_utc());
        if let Err(error) = append_stage(
            active.state,
            active.path.clone(),
            ReviewStage::Disposition,
            markdown,
        )
        .await
        {
            let error = error.with_site(site);
            inner.latch(error.clone());
            return Err(error);
        }
        inner.remove_active(site, &active.path)?;
        Ok(())
    }

    /// 返回一个固定、无内容派生的调用文件相对路径，供普通安全日志建立关联。
    #[cfg(test)]
    pub(crate) fn relative_path(site: LlmCallSite) -> PathBuf {
        relative_call_path(site)
    }

    #[cfg(test)]
    fn with_store(
        run_id: RunId,
        locale: UiLocale,
        context: LlmCallReviewContext,
        store: Arc<dyn LlmCallReviewStore>,
    ) -> Self {
        Self {
            inner: Some(Arc::new(LlmCallRecorderInner {
                run_id,
                locale,
                context,
                store,
                active: Mutex::new(Vec::new()),
                admission: Mutex::new(SendAdmission::default()),
                failure_signal: watch::channel(None).0,
                injected_operation: Mutex::new(None),
                send_authorization_pause: Mutex::new(None),
            })),
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_test_failure(&self, operation: &'static str) {
        assert!(
            matches!(
                operation,
                "write_request"
                    | "sync_request"
                    | "write_provider"
                    | "sync_provider"
                    | "write_disposition"
                    | "sync_disposition"
            ),
            "测试只能注入调用文件写入或同步操作"
        );
        let inner = self.inner.as_ref().expect("测试记录器必须启用");
        *lock_unpoisoned(&inner.injected_operation) = Some(operation);
    }

    #[cfg(test)]
    pub(crate) fn pause_next_send_authorization(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let inner = self.inner.as_ref().expect("测试记录器必须启用");
        let (entered_sender, entered_receiver) = std::sync::mpsc::sync_channel(1);
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
        let previous =
            lock_unpoisoned(&inner.send_authorization_pause).replace(SendAuthorizationPause {
                entered: entered_sender,
                release: release_receiver,
            });
        assert!(previous.is_none(), "同一时间只能暂停一个发送准入点");
        (entered_receiver, release_sender)
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |inner| lock_unpoisoned(&inner.active).len())
    }
}

impl LlmCallRecorderInner {
    fn ensure_healthy(&self) -> Result<(), LlmCallReviewError> {
        match self.failure() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn failure(&self) -> Option<LlmCallReviewError> {
        lock_unpoisoned(&self.admission).failure.clone()
    }

    #[cfg(test)]
    fn take_injected_failure(
        &self,
        site: LlmCallSite,
        path: &Path,
        allowed: &[&'static str],
    ) -> Option<LlmCallReviewError> {
        let mut injected = lock_unpoisoned(&self.injected_operation);
        let operation = (*injected).filter(|operation| allowed.contains(operation))?;
        *injected = None;
        Some(
            LlmCallReviewError::io(operation, path, io::Error::from_raw_os_error(5))
                .with_site(site),
        )
    }

    fn latch(&self, error: LlmCallReviewError) -> LlmCallReviewError {
        let mut admission = lock_unpoisoned(&self.admission);
        if let Some(first) = &admission.failure {
            return first.clone();
        }
        admission.failure = Some(error.clone());
        self.failure_signal.send_replace(Some(error.clone()));
        error
    }

    fn call_path(&self, site: LlmCallSite) -> PathBuf {
        self.store.run_root().join(relative_call_path(site))
    }

    fn active_call(
        &self,
        site: LlmCallSite,
        operation: &'static str,
    ) -> Result<ActiveCallSnapshot, LlmCallReviewError> {
        let snapshot = lock_unpoisoned(&self.active)
            .iter()
            .find(|record| record.site == site)
            .map(|record| ActiveCallSnapshot {
                path: record.path.clone(),
                state: Arc::clone(&record.state),
            });
        snapshot.ok_or_else(|| {
            let error = LlmCallReviewError::invalid_state(
                operation,
                &self.call_path(site),
                "调用没有已同步的请求阶段或已经终结",
            )
            .with_site(site);
            self.latch(error)
        })
    }

    fn remove_active(&self, site: LlmCallSite, path: &Path) -> Result<(), LlmCallReviewError> {
        let mut active = lock_unpoisoned(&self.active);
        let Some(index) = active.iter().position(|record| record.site == site) else {
            drop(active);
            return Err(self.latch(
                LlmCallReviewError::invalid_state(
                    "finish_call",
                    path,
                    "调用终态无法关联到活动记录",
                )
                .with_site(site),
            ));
        };
        active.swap_remove(index);
        Ok(())
    }
}

struct ActiveCallSnapshot {
    path: PathBuf,
    state: Arc<Mutex<ActiveCallState>>,
}

fn create_system_store(
    workspace: &Path,
    run_id: RunId,
) -> Result<SystemLlmCallReviewStore, LlmCallReviewError> {
    let calls_root = workspace.join(CALLS_DIRECTORY);
    let pinned_calls_root = create_directories_without_reparse(&calls_root)
        .map_err(|source| LlmCallReviewError::windows("create_calls_root", &calls_root, source))?;
    let run_root = pinned_calls_root.resolved_path().join(run_id.to_string());
    std::fs::create_dir(&run_root)
        .map_err(|source| LlmCallReviewError::io("create_run_root", &run_root, source))?;
    let run_root = pin_directory_without_reparse(&run_root)
        .map_err(|source| LlmCallReviewError::windows("pin_run_root", &run_root, source))?;
    Ok(SystemLlmCallReviewStore { run_root })
}

async fn append_stage(
    state: Arc<Mutex<ActiveCallState>>,
    path: PathBuf,
    stage: ReviewStage,
    markdown: String,
) -> Result<(), LlmCallReviewError> {
    let operation = stage.write_operation();
    let work_path = path.clone();
    run_blocking(operation, path, move || {
        let mut state = lock_unpoisoned(&state);
        let (expected, next) = match stage {
            ReviewStage::Provider => (ActiveCallPhase::SendAuthorized, ActiveCallPhase::Provider),
            ReviewStage::Disposition => (ActiveCallPhase::Provider, ActiveCallPhase::Disposition),
        };
        if state.phase != expected {
            return Err(LlmCallReviewError::invalid_state(
                operation,
                &work_path,
                "调用审阅阶段顺序无效",
            ));
        }
        state
            .file
            .append_and_sync(&work_path, stage, markdown.as_bytes())?;
        state.phase = next;
        Ok(())
    })
    .await
}

async fn run_blocking<T, F>(
    operation: &'static str,
    path: PathBuf,
    work: F,
) -> Result<T, LlmCallReviewError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, LlmCallReviewError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|source| LlmCallReviewError::blocking_worker(operation, &path, source))?
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn relative_call_path(site: LlmCallSite) -> PathBuf {
    match site {
        LlmCallSite::Standard {
            task_ordinal,
            attempt,
        } => PathBuf::from(STANDARD_DIRECTORY)
            .join(format!("task-{:06}", task_ordinal.get()))
            .join(format!("attempt-{:03}.md", attempt.get())),
        LlmCallSite::Lua { call } => {
            PathBuf::from(LUA_DIRECTORY).join(format!("call-{:06}.md", call.get()))
        }
    }
}

fn render_request(
    locale: UiLocale,
    run_id: RunId,
    context: &LlmCallReviewContext,
    site: LlmCallSite,
    request: &LlmCallRequestRecord,
    recorded_at: OffsetDateTime,
) -> String {
    let labels = labels(locale);
    let mut output = String::new();
    output.push_str("# ");
    output.push_str(labels.title);
    output.push_str("\n\n> ⚠️ ");
    output.push_str(labels.sensitive_warning);
    output.push_str("\n>\n> ");
    output.push_str(labels.incomplete_warning);
    output.push_str("\n\n## ");
    output.push_str(labels.attribution);
    output.push('\n');

    let mut attribution = String::new();
    push_json_field(&mut attribution, "run_id", &run_id.to_string());
    push_json_field(&mut attribution, "ui_locale", locale.as_str());
    push_json_field(&mut attribution, "engine", &context.engine);
    push_json_field(&mut attribution, "project", &context.project);
    push_json_field(&mut attribution, "profile", &context.profile);
    push_json_field(&mut attribution, "client", &context.client);
    match site {
        LlmCallSite::Standard {
            task_ordinal,
            attempt,
        } => {
            push_json_field(&mut attribution, "call_kind", "standard");
            attribution.push_str(&format!("task_ordinal = {}\n", task_ordinal.get()));
            attribution.push_str(&format!("attempt = {}\n", attempt.get()));
        }
        LlmCallSite::Lua { call } => {
            push_json_field(&mut attribution, "call_kind", "lua");
            attribution.push_str(&format!("call = {}\n", call.get()));
        }
    }
    push_json_field(
        &mut attribution,
        "request_recorded_at_utc",
        &recorded_at_utc(recorded_at),
    );
    output.push_str(&fenced_block("text", &attribution));

    output.push_str("\n## ");
    output.push_str(labels.request);
    output.push_str(" (`request_recorded`)\n");
    let mut request_metadata = String::new();
    push_json_field(
        &mut request_metadata,
        "endpoint_without_query",
        &review_endpoint(&request.endpoint),
    );
    request_metadata.push_str(&format!("request_body_bytes = {}\n", request.body.len()));
    output.push_str(&fenced_block("text", &request_metadata));
    output.push('\n');
    output.push_str(&render_json_bytes(&request.body));
    output.push_str("\n<!-- att-llm-call-review-stage: request_complete -->\n");
    output
}

fn render_provider(
    locale: UiLocale,
    provider: &LlmProviderRecord,
    recorded_at: OffsetDateTime,
) -> String {
    let labels = labels(locale);
    let mut output = String::new();
    output.push_str("\n## ");
    output.push_str(labels.provider);
    output.push_str(" (`provider_recorded`)\n");
    let mut metadata = String::new();
    push_json_field(
        &mut metadata,
        "provider_recorded_at_utc",
        &recorded_at_utc(recorded_at),
    );
    metadata.push_str(&format!(
        "elapsed_milliseconds = {}\n",
        provider.elapsed.as_millis()
    ));

    match &provider.outcome {
        LlmProviderOutcome::Response {
            status,
            headers,
            body,
        } => {
            push_json_field(&mut metadata, "outcome", "response_received");
            metadata.push_str(&format!("http_status = {status}\n"));
            push_optional_json_field(&mut metadata, "content_type", &headers.content_type);
            push_optional_json_field(&mut metadata, "x_request_id", &headers.request_id);
            push_optional_json_field(&mut metadata, "retry_after", &headers.retry_after);
            output.push_str(&fenced_block("text", &metadata));
            output.push_str("\n### ");
            output.push_str(labels.raw_response);
            output.push('\n');
            output.push_str(&render_raw_body(body));
        }
        LlmProviderOutcome::ResponseNotReceived => {
            push_json_field(&mut metadata, "outcome", "response_not_received");
            output.push_str(&fenced_block("text", &metadata));
            output.push('\n');
            output.push_str(labels.response_not_received);
            output.push('\n');
        }
        LlmProviderOutcome::BodyReadFailed { status, headers } => {
            push_json_field(&mut metadata, "outcome", "body_read_failed");
            if let Some(status) = status {
                metadata.push_str(&format!("http_status = {status}\n"));
            } else {
                metadata.push_str("http_status = null\n");
            }
            push_optional_json_field(&mut metadata, "content_type", &headers.content_type);
            push_optional_json_field(&mut metadata, "x_request_id", &headers.request_id);
            push_optional_json_field(&mut metadata, "retry_after", &headers.retry_after);
            output.push_str(&fenced_block("text", &metadata));
            output.push('\n');
            output.push_str(labels.body_read_failed);
            output.push('\n');
        }
    }
    output.push_str("\n<!-- att-llm-call-review-stage: provider_complete -->\n");
    output
}

fn render_disposition(
    locale: UiLocale,
    disposition: &LlmCallDisposition,
    recorded_at: OffsetDateTime,
) -> String {
    let labels = labels(locale);
    let mut output = String::new();
    output.push_str("\n## ");
    output.push_str(labels.disposition);
    output.push_str(" (`att_disposition_recorded`)\n");
    let mut metadata = String::new();
    push_json_field(
        &mut metadata,
        "disposition_recorded_at_utc",
        &recorded_at_utc(recorded_at),
    );

    match disposition {
        LlmCallDisposition::Standard(standard) => {
            let outcome_code = match standard.outcome {
                LlmStandardDispositionOutcome::Complete => "complete",
                LlmStandardDispositionOutcome::Partial => "partial",
                LlmStandardDispositionOutcome::Unavailable => "unavailable",
            };
            push_json_field(&mut metadata, "disposition", "standard_validation");
            push_json_field(&mut metadata, "envelope_parse_status", "parsed");
            push_json_field(&mut metadata, "validation_outcome", outcome_code);
            output.push_str(&fenced_block("text", &metadata));
            output.push('\n');
            output.push_str(standard_outcome_explanation(locale, standard.outcome));
            output.push('\n');
            output.push_str(&render_response_metadata(&standard.response));

            output.push_str("\n### ");
            output.push_str(labels.accepted);
            output.push('\n');
            if standard.accepted_ids.is_empty() {
                output.push_str(labels.none);
                output.push('\n');
            } else {
                let ids = standard
                    .accepted_ids
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&fenced_block("text", &format!("{ids}\n")));
            }

            output.push_str("\n### ");
            output.push_str(labels.rejected);
            output.push('\n');
            if standard.rejected.is_empty() {
                output.push_str(labels.none);
                output.push('\n');
            } else {
                for rejected in &standard.rejected {
                    let mut rejection = String::new();
                    rejection.push_str(&format!("id = {}\n", rejected.id));
                    push_json_field(&mut rejection, "reason_code", &rejected.reason_code);
                    push_json_field(
                        &mut rejection,
                        "explanation",
                        standard_rejection_explanation(locale, &rejected.reason_code),
                    );
                    push_optional_json_field(&mut rejection, "detail", &rejected.detail);
                    output.push_str(&fenced_block("text", &rejection));
                }
            }
        }
        LlmCallDisposition::LuaDelivered { response } => {
            push_json_field(&mut metadata, "disposition", "delivered_to_lua");
            push_json_field(&mut metadata, "envelope_parse_status", "parsed");
            output.push_str(&fenced_block("text", &metadata));
            output.push('\n');
            output.push_str(labels.lua_delivered);
            output.push('\n');
            output.push_str(&render_response_metadata(response));
        }
        LlmCallDisposition::Rejected { code, response } => {
            push_json_field(&mut metadata, "disposition", "rejected");
            push_json_field(
                &mut metadata,
                "envelope_parse_status",
                if response.is_some() {
                    "parsed"
                } else {
                    "unavailable"
                },
            );
            push_json_field(&mut metadata, "reason_code", code);
            push_json_field(
                &mut metadata,
                "explanation",
                disposition_rejection_explanation(locale, code),
            );
            output.push_str(&fenced_block("text", &metadata));
            if let Some(response) = response {
                output.push('\n');
                output.push_str(&render_response_metadata(response));
            }
        }
    }
    output.push_str("\n<!-- att-llm-call-review-stage: disposition_complete -->\n");
    output
}

fn render_response_metadata(metadata: &LlmParsedResponseMetadata) -> String {
    let mut output = String::new();
    push_optional_json_field(
        &mut output,
        "provider_request_id",
        &metadata.provider_request_id,
    );
    push_optional_json_field(
        &mut output,
        "provider_response_id",
        &metadata.provider_response_id,
    );
    push_json_field(&mut output, "finish_reason", &metadata.finish_reason);
    match metadata.usage {
        Some(usage) => {
            output.push_str(&format!("prompt_tokens = {}\n", usage.prompt_tokens));
            output.push_str(&format!(
                "completion_tokens = {}\n",
                usage.completion_tokens
            ));
            output.push_str(&format!("total_tokens = {}\n", usage.total_tokens));
        }
        None => {
            output.push_str("prompt_tokens = null\n");
            output.push_str("completion_tokens = null\n");
            output.push_str("total_tokens = null\n");
        }
    }
    fenced_block("text", &output)
}

fn render_json_bytes(body: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(body)
        .and_then(|value| serde_json::to_string_pretty(&value))
    {
        Ok(pretty) => fenced_block("json", &pretty),
        Err(_) => render_raw_body(body),
    }
}

fn render_raw_body(body: &[u8]) -> String {
    match std::str::from_utf8(body) {
        Ok(text) => {
            let mut output = format!(
                "encoding = \"utf-8\"\nbyte_count = {}\nends_with_line_feed = {}\n",
                body.len(),
                body.ends_with(b"\n")
            );
            output.push_str(&fenced_block("text", text));
            output
        }
        Err(_) => {
            let encoded = BASE64_STANDARD.encode(body);
            let mut output = format!("encoding = \"base64\"\nbyte_count = {}\n", body.len());
            output.push_str(&fenced_block("base64", &encoded));
            output
        }
    }
}

fn fenced_block(language: &str, content: &str) -> String {
    let fence_length = longest_backtick_run(content).saturating_add(1).max(3);
    let fence = "`".repeat(fence_length);
    let mut output = String::new();
    output.push_str(&fence);
    output.push_str(language);
    output.push('\n');
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output.push('\n');
    output
}

fn longest_backtick_run(value: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in value.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn review_endpoint(endpoint: &Url) -> String {
    let mut endpoint = endpoint.clone();
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    if endpoint.set_username("").is_err() || endpoint.set_password(None).is_err() {
        return format!("{}://<redacted-invalid-authority>", endpoint.scheme());
    }
    endpoint.to_string()
}

fn push_json_field(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(" = ");
    output
        .push_str(&serde_json::to_string(value).expect("Rust 字符串必须可以序列化为 JSON 字符串"));
    output.push('\n');
}

fn push_optional_json_field(output: &mut String, key: &str, value: &Option<String>) {
    match value {
        Some(value) => push_json_field(output, key, value),
        None => {
            output.push_str(key);
            output.push_str(" = null\n");
        }
    }
}

fn recorded_at_utc(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000,
    )
}

struct ReviewLabels {
    title: &'static str,
    sensitive_warning: &'static str,
    incomplete_warning: &'static str,
    attribution: &'static str,
    request: &'static str,
    provider: &'static str,
    disposition: &'static str,
    raw_response: &'static str,
    response_not_received: &'static str,
    body_read_failed: &'static str,
    accepted: &'static str,
    rejected: &'static str,
    none: &'static str,
    lua_delivered: &'static str,
}

fn labels(locale: UiLocale) -> ReviewLabels {
    match locale {
        UiLocale::Arabic => ReviewLabels {
            title: "سجل مراجعة استدعاء LLM",
            sensitive_warning: "يحتوي هذا الملف على الطلب والاستجابة الكاملين وقد يتضمن محتوى حساسًا. لا تشاركه أو تودعه في نظام التحكم بالإصدارات دون مراجعة.",
            incomplete_warning: "إذا انتهى الملف بعد قسم الطلب فقط، فنتيجة الاستدعاء غير معروفة؛ لا تفترض أن الطلب لم يُرسل.",
            attribution: "بيانات الاستدعاء",
            request: "الطلب الفعلي",
            provider: "نتيجة المزوّد",
            disposition: "قرار ATT",
            raw_response: "نص الاستجابة الخام",
            response_not_received: "لم تكتمل استجابة من المزوّد (`response_not_received`).",
            body_read_failed: "بدأت الاستجابة، لكن تعذّر إكمال قراءة النص (`body_read_failed`).",
            accepted: "المعرّفات المقبولة",
            rejected: "المعرّفات المرفوضة",
            none: "لا يوجد.",
            lua_delivered: "سُلّمت الاستجابة المحللة إلى Lua؛ ولا يفسر ATT منطق Lua اللاحق.",
        },
        UiLocale::SimplifiedChinese => ReviewLabels {
            title: "LLM 调用审阅档案",
            sensitive_warning: "本文件包含完整请求与响应，可能含敏感内容。未经检查不得分享或提交版本库。",
            incomplete_warning: "若文件只停在请求阶段，则调用结果未知；不能据此推断请求尚未发送。",
            attribution: "调用归属",
            request: "实际请求",
            provider: "Provider 结果",
            disposition: "ATT 处置",
            raw_response: "完整原始响应正文",
            response_not_received: "未取得完整 Provider 响应（`response_not_received`）。",
            body_read_failed: "Provider 已开始响应，但正文读取未完成（`body_read_failed`）。",
            accepted: "已接受 ID",
            rejected: "已拒绝 ID",
            none: "无。",
            lua_delivered: "已把解析后的响应交给 Lua；ATT 不解释之后的任意 Lua 业务逻辑。",
        },
        UiLocale::TraditionalChinese => ReviewLabels {
            title: "LLM 呼叫審閱檔案",
            sensitive_warning: "本檔案包含完整請求與回應，可能含敏感內容。未經檢查不得分享或提交版本庫。",
            incomplete_warning: "若檔案只停在請求階段，則呼叫結果未知；不能據此推斷請求尚未傳送。",
            attribution: "呼叫歸屬",
            request: "實際請求",
            provider: "Provider 結果",
            disposition: "ATT 處置",
            raw_response: "完整原始回應本文",
            response_not_received: "未取得完整 Provider 回應（`response_not_received`）。",
            body_read_failed: "Provider 已開始回應，但本文讀取未完成（`body_read_failed`）。",
            accepted: "已接受 ID",
            rejected: "已拒絕 ID",
            none: "無。",
            lua_delivered: "已將解析後的回應交給 Lua；ATT 不解釋之後的任意 Lua 業務邏輯。",
        },
        UiLocale::English => ReviewLabels {
            title: "LLM call review archive",
            sensitive_warning: "This file contains the complete request and response and may include sensitive content. Do not share it or commit it to version control without review.",
            incomplete_warning: "If this file ends after the request section, the call outcome is unknown; do not infer that the request was not sent.",
            attribution: "Call attribution",
            request: "Effective request",
            provider: "Provider result",
            disposition: "ATT disposition",
            raw_response: "Complete raw response body",
            response_not_received: "No complete provider response was received (`response_not_received`).",
            body_read_failed: "The provider began responding, but the response body could not be read completely (`body_read_failed`).",
            accepted: "Accepted IDs",
            rejected: "Rejected IDs",
            none: "None.",
            lua_delivered: "The parsed response was delivered to Lua; ATT does not interpret subsequent arbitrary Lua logic.",
        },
        UiLocale::French => ReviewLabels {
            title: "Archive de révision des appels LLM",
            sensitive_warning: "Ce fichier contient la requête et la réponse complètes et peut inclure des données sensibles. Ne le partagez pas et ne le validez pas dans le contrôle de version sans vérification.",
            incomplete_warning: "Si ce fichier s’arrête après la section de requête, le résultat de l’appel est inconnu ; n’en déduisez pas que la requête n’a pas été envoyée.",
            attribution: "Attribution de l’appel",
            request: "Requête effective",
            provider: "Résultat du fournisseur",
            disposition: "Décision d’ATT",
            raw_response: "Corps brut complet de la réponse",
            response_not_received: "Aucune réponse complète du fournisseur n’a été reçue (`response_not_received`).",
            body_read_failed: "Le fournisseur a commencé à répondre, mais la lecture du corps n’a pas pu être terminée (`body_read_failed`).",
            accepted: "ID acceptés",
            rejected: "ID rejetés",
            none: "Aucun.",
            lua_delivered: "La réponse analysée a été remise à Lua ; ATT n’interprète pas la logique Lua arbitraire qui suit.",
        },
        UiLocale::Russian => ReviewLabels {
            title: "Архив проверки вызовов LLM",
            sensitive_warning: "Этот файл содержит полный запрос и ответ и может включать конфиденциальные данные. Не передавайте его и не добавляйте в систему контроля версий без проверки.",
            incomplete_warning: "Если файл заканчивается после раздела запроса, результат вызова неизвестен; нельзя считать, что запрос не был отправлен.",
            attribution: "Принадлежность вызова",
            request: "Фактический запрос",
            provider: "Результат провайдера",
            disposition: "Решение ATT",
            raw_response: "Полное исходное тело ответа",
            response_not_received: "Полный ответ провайдера не получен (`response_not_received`).",
            body_read_failed: "Провайдер начал отвечать, но тело ответа не удалось прочитать полностью (`body_read_failed`).",
            accepted: "Принятые ID",
            rejected: "Отклонённые ID",
            none: "Нет.",
            lua_delivered: "Разобранный ответ передан Lua; ATT не интерпретирует последующую произвольную логику Lua.",
        },
        UiLocale::Spanish => ReviewLabels {
            title: "Archivo de revisión de llamadas LLM",
            sensitive_warning: "Este archivo contiene la solicitud y la respuesta completas y puede incluir contenido sensible. No lo comparta ni lo confirme en el control de versiones sin revisarlo.",
            incomplete_warning: "Si el archivo termina después de la sección de solicitud, el resultado de la llamada es desconocido; no deduzca que la solicitud no se envió.",
            attribution: "Atribución de la llamada",
            request: "Solicitud efectiva",
            provider: "Resultado del proveedor",
            disposition: "Decisión de ATT",
            raw_response: "Cuerpo de respuesta sin procesar completo",
            response_not_received: "No se recibió una respuesta completa del proveedor (`response_not_received`).",
            body_read_failed: "El proveedor comenzó a responder, pero no se pudo leer todo el cuerpo (`body_read_failed`).",
            accepted: "ID aceptados",
            rejected: "ID rechazados",
            none: "Ninguno.",
            lua_delivered: "La respuesta analizada se entregó a Lua; ATT no interpreta la lógica Lua arbitraria posterior.",
        },
        UiLocale::Japanese => ReviewLabels {
            title: "LLM 呼び出しレビュー記録",
            sensitive_warning: "このファイルには完全なリクエストとレスポンスが含まれ、機密情報を含む場合があります。確認せずに共有またはバージョン管理へ追加しないでください。",
            incomplete_warning: "リクエスト節だけでファイルが終わっている場合、呼び出し結果は不明です。未送信だったとは判断できません。",
            attribution: "呼び出しの帰属",
            request: "実際のリクエスト",
            provider: "プロバイダー結果",
            disposition: "ATT の処置",
            raw_response: "完全な生レスポンス本文",
            response_not_received: "完全なプロバイダー応答を受信できませんでした（`response_not_received`）。",
            body_read_failed: "プロバイダーは応答を開始しましたが、本文を最後まで読み取れませんでした（`body_read_failed`）。",
            accepted: "受理した ID",
            rejected: "拒否した ID",
            none: "なし。",
            lua_delivered: "解析済みレスポンスを Lua に渡しました。ATT はその後の任意の Lua ロジックを解釈しません。",
        },
        UiLocale::Korean => ReviewLabels {
            title: "LLM 호출 검토 기록",
            sensitive_warning: "이 파일에는 전체 요청과 응답이 포함되며 민감한 내용이 있을 수 있습니다. 검토 없이 공유하거나 버전 관리에 커밋하지 마십시오.",
            incomplete_warning: "파일이 요청 섹션 뒤에서 끝나면 호출 결과를 알 수 없습니다. 요청이 전송되지 않았다고 추정하지 마십시오.",
            attribution: "호출 귀속",
            request: "실제 요청",
            provider: "공급자 결과",
            disposition: "ATT 처리",
            raw_response: "전체 원시 응답 본문",
            response_not_received: "완전한 공급자 응답을 받지 못했습니다(`response_not_received`).",
            body_read_failed: "공급자가 응답을 시작했지만 본문을 끝까지 읽지 못했습니다(`body_read_failed`).",
            accepted: "수락한 ID",
            rejected: "거부한 ID",
            none: "없음.",
            lua_delivered: "파싱한 응답을 Lua에 전달했습니다. ATT는 이후의 임의 Lua 로직을 해석하지 않습니다.",
        },
        UiLocale::Vietnamese => ReviewLabels {
            title: "Kho lưu rà soát lệnh gọi LLM",
            sensitive_warning: "Tệp này chứa toàn bộ yêu cầu và phản hồi, có thể gồm nội dung nhạy cảm. Không chia sẻ hoặc cam kết vào hệ thống quản lý phiên bản khi chưa rà soát.",
            incomplete_warning: "Nếu tệp kết thúc sau phần yêu cầu, kết quả lệnh gọi chưa xác định; không được suy ra rằng yêu cầu chưa được gửi.",
            attribution: "Nguồn gốc lệnh gọi",
            request: "Yêu cầu thực tế",
            provider: "Kết quả từ nhà cung cấp",
            disposition: "Cách ATT xử lý",
            raw_response: "Toàn bộ nội dung phản hồi thô",
            response_not_received: "Không nhận được phản hồi hoàn chỉnh từ nhà cung cấp (`response_not_received`).",
            body_read_failed: "Nhà cung cấp đã bắt đầu phản hồi nhưng không thể đọc hết nội dung (`body_read_failed`).",
            accepted: "ID được chấp nhận",
            rejected: "ID bị từ chối",
            none: "Không có.",
            lua_delivered: "Phản hồi đã phân tích được chuyển cho Lua; ATT không diễn giải logic Lua tùy ý sau đó.",
        },
    }
}

fn standard_outcome_explanation(
    locale: UiLocale,
    outcome: LlmStandardDispositionOutcome,
) -> &'static str {
    match (locale, outcome) {
        (UiLocale::Arabic, LlmStandardDispositionOutcome::Complete) => {
            "اكتملت عملية التحقق القياسية وقُبلت جميع المخرجات المتوقعة."
        }
        (UiLocale::Arabic, LlmStandardDispositionOutcome::Partial) => {
            "اكتملت عملية التحقق القياسية؛ قُبلت بعض المخرجات وبقي بعضها دون حل."
        }
        (UiLocale::Arabic, LlmStandardDispositionOutcome::Unavailable) => {
            "اكتملت عملية التحقق القياسية، لكن لم تتوفر نتيجة مكتملة لهذه المهمة."
        }
        (UiLocale::SimplifiedChinese, LlmStandardDispositionOutcome::Complete) => {
            "Standard 验收完成，全部预期输出均已接受。"
        }
        (UiLocale::SimplifiedChinese, LlmStandardDispositionOutcome::Partial) => {
            "Standard 验收完成；部分输出已接受，部分仍未解决。"
        }
        (UiLocale::SimplifiedChinese, LlmStandardDispositionOutcome::Unavailable) => {
            "Standard 验收完成，但本任务没有可用的完整结果。"
        }
        (UiLocale::TraditionalChinese, LlmStandardDispositionOutcome::Complete) => {
            "Standard 驗收完成，全部預期輸出均已接受。"
        }
        (UiLocale::TraditionalChinese, LlmStandardDispositionOutcome::Partial) => {
            "Standard 驗收完成；部分輸出已接受，部分仍未解決。"
        }
        (UiLocale::TraditionalChinese, LlmStandardDispositionOutcome::Unavailable) => {
            "Standard 驗收完成，但本任務沒有可用的完整結果。"
        }
        (UiLocale::English, LlmStandardDispositionOutcome::Complete) => {
            "Standard validation completed and accepted every expected output."
        }
        (UiLocale::English, LlmStandardDispositionOutcome::Partial) => {
            "Standard validation completed; some outputs were accepted and some remain unresolved."
        }
        (UiLocale::English, LlmStandardDispositionOutcome::Unavailable) => {
            "Standard validation completed, but no complete result is available for this task."
        }
        (UiLocale::French, LlmStandardDispositionOutcome::Complete) => {
            "La validation Standard est terminée et toutes les sorties attendues ont été acceptées."
        }
        (UiLocale::French, LlmStandardDispositionOutcome::Partial) => {
            "La validation Standard est terminée ; certaines sorties sont acceptées et d’autres restent non résolues."
        }
        (UiLocale::French, LlmStandardDispositionOutcome::Unavailable) => {
            "La validation Standard est terminée, mais aucun résultat complet n’est disponible pour cette tâche."
        }
        (UiLocale::Russian, LlmStandardDispositionOutcome::Complete) => {
            "Проверка Standard завершена, все ожидаемые результаты приняты."
        }
        (UiLocale::Russian, LlmStandardDispositionOutcome::Partial) => {
            "Проверка Standard завершена: часть результатов принята, часть осталась нерешённой."
        }
        (UiLocale::Russian, LlmStandardDispositionOutcome::Unavailable) => {
            "Проверка Standard завершена, но полного результата для задачи нет."
        }
        (UiLocale::Spanish, LlmStandardDispositionOutcome::Complete) => {
            "La validación Standard finalizó y aceptó todos los resultados esperados."
        }
        (UiLocale::Spanish, LlmStandardDispositionOutcome::Partial) => {
            "La validación Standard finalizó; algunos resultados se aceptaron y otros siguen sin resolverse."
        }
        (UiLocale::Spanish, LlmStandardDispositionOutcome::Unavailable) => {
            "La validación Standard finalizó, pero no hay un resultado completo disponible para esta tarea."
        }
        (UiLocale::Japanese, LlmStandardDispositionOutcome::Complete) => {
            "Standard 検証が完了し、すべての期待出力を受理しました。"
        }
        (UiLocale::Japanese, LlmStandardDispositionOutcome::Partial) => {
            "Standard 検証が完了しました。一部を受理し、一部は未解決です。"
        }
        (UiLocale::Japanese, LlmStandardDispositionOutcome::Unavailable) => {
            "Standard 検証は完了しましたが、このタスクで利用できる完全な結果はありません。"
        }
        (UiLocale::Korean, LlmStandardDispositionOutcome::Complete) => {
            "Standard 검증을 완료했으며 모든 예상 출력을 수락했습니다."
        }
        (UiLocale::Korean, LlmStandardDispositionOutcome::Partial) => {
            "Standard 검증을 완료했습니다. 일부 출력은 수락했고 일부는 해결되지 않았습니다."
        }
        (UiLocale::Korean, LlmStandardDispositionOutcome::Unavailable) => {
            "Standard 검증을 완료했지만 이 작업에 사용할 완전한 결과가 없습니다."
        }
        (UiLocale::Vietnamese, LlmStandardDispositionOutcome::Complete) => {
            "Quá trình xác thực Standard đã hoàn tất và chấp nhận mọi đầu ra dự kiến."
        }
        (UiLocale::Vietnamese, LlmStandardDispositionOutcome::Partial) => {
            "Quá trình xác thực Standard đã hoàn tất; một số đầu ra được chấp nhận và một số chưa được giải quyết."
        }
        (UiLocale::Vietnamese, LlmStandardDispositionOutcome::Unavailable) => {
            "Quá trình xác thực Standard đã hoàn tất nhưng không có kết quả đầy đủ cho tác vụ này."
        }
    }
}

fn disposition_rejection_explanation(locale: UiLocale, code: &str) -> &'static str {
    match code {
        "response_not_received" => localized(
            locale,
            [
                "لم يستلم ATT استجابة كاملة، لذلك لم يحلل أو يسلّم أي نتيجة.",
                "ATT 未取得完整响应，因此没有解析、验收或交付任何结果。",
                "ATT 未取得完整回應，因此沒有解析、驗收或交付任何結果。",
                "ATT did not receive a complete response, so no result was parsed, validated, or delivered.",
                "ATT n’a pas reçu de réponse complète ; aucun résultat n’a donc été analysé, validé ou remis.",
                "ATT не получил полный ответ, поэтому результат не разбирался, не проверялся и не передавался.",
                "ATT no recibió una respuesta completa, por lo que no analizó, validó ni entregó ningún resultado.",
                "ATT は完全な応答を受信できなかったため、結果の解析・検証・引き渡しを行っていません。",
                "ATT가 완전한 응답을 받지 못했으므로 결과를 파싱, 검증 또는 전달하지 않았습니다.",
                "ATT không nhận được phản hồi hoàn chỉnh nên không phân tích, xác thực hoặc chuyển giao kết quả nào.",
            ],
        ),
        "body_read_failed" => localized(
            locale,
            [
                "لم يكتمل نص الاستجابة، لذلك لم يحلل ATT أو يسلّم نتيجة جزئية.",
                "响应正文不完整，因此 ATT 没有解析或交付部分结果。",
                "回應本文不完整，因此 ATT 沒有解析或交付部分結果。",
                "The response body was incomplete, so ATT did not parse or deliver a partial result.",
                "Le corps de la réponse était incomplet ; ATT n’a donc ni analysé ni remis de résultat partiel.",
                "Тело ответа неполно, поэтому ATT не разбирал и не передавал частичный результат.",
                "El cuerpo de la respuesta estaba incompleto, por lo que ATT no analizó ni entregó un resultado parcial.",
                "応答本文が不完全だったため、ATT は部分的な結果を解析または引き渡していません。",
                "응답 본문이 불완전하여 ATT는 부분 결과를 파싱하거나 전달하지 않았습니다.",
                "Nội dung phản hồi không đầy đủ nên ATT không phân tích hoặc chuyển giao kết quả một phần.",
            ],
        ),
        "http_status_rejected" => localized(
            locale,
            [
                "رفض ATT استجابة HTTP غير الناجحة قبل تحليل محتوى النموذج.",
                "ATT 在解析模型内容前拒绝了非成功 HTTP 响应。",
                "ATT 在解析模型內容前拒絕了非成功 HTTP 回應。",
                "ATT rejected the non-success HTTP response before parsing model content.",
                "ATT a rejeté la réponse HTTP en échec avant d’analyser le contenu du modèle.",
                "ATT отклонил HTTP-ответ с ошибочным статусом до разбора содержимого модели.",
                "ATT rechazó la respuesta HTTP no satisfactoria antes de analizar el contenido del modelo.",
                "ATT はモデル内容を解析する前に、成功以外の HTTP 応答を拒否しました。",
                "ATT는 모델 내용을 파싱하기 전에 성공하지 않은 HTTP 응답을 거부했습니다.",
                "ATT từ chối phản hồi HTTP không thành công trước khi phân tích nội dung mô hình.",
            ],
        ),
        "response_json_invalid" => localized(
            locale,
            [
                "تعذر على ATT تحليل استجابة النجاح بصفتها JSON صالحًا.",
                "ATT 无法把成功响应解析为有效 JSON。",
                "ATT 無法將成功回應解析為有效 JSON。",
                "ATT could not parse the successful response as valid JSON.",
                "ATT n’a pas pu analyser la réponse réussie comme un JSON valide.",
                "ATT не смог разобрать успешный ответ как допустимый JSON.",
                "ATT no pudo analizar la respuesta satisfactoria como JSON válido.",
                "ATT は成功応答を有効な JSON として解析できませんでした。",
                "ATT가 성공 응답을 유효한 JSON으로 파싱하지 못했습니다.",
                "ATT không thể phân tích phản hồi thành công dưới dạng JSON hợp lệ.",
            ],
        ),
        "response_contract_invalid" => localized(
            locale,
            [
                "رفض ATT استجابة النجاح لأنها لا تحقق عقد Chat Completions.",
                "成功响应不符合 Chat Completions 契约，已被 ATT 拒绝。",
                "成功回應不符合 Chat Completions 契約，已被 ATT 拒絕。",
                "ATT rejected the successful response because it did not satisfy the Chat Completions contract.",
                "ATT a rejeté la réponse réussie, car elle ne respectait pas le contrat Chat Completions.",
                "ATT отклонил успешный ответ, поскольку он не соответствовал контракту Chat Completions.",
                "ATT rechazó la respuesta satisfactoria porque no cumplía el contrato de Chat Completions.",
                "成功応答が Chat Completions 契約を満たさなかったため、ATT は拒否しました。",
                "성공 응답이 Chat Completions 계약을 충족하지 않아 ATT가 거부했습니다.",
                "ATT từ chối phản hồi thành công vì phản hồi không đáp ứng hợp đồng Chat Completions.",
            ],
        ),
        "response_processing_failed" => localized(
            locale,
            [
                "نجح تحليل عقد Chat Completions، لكن ATT لم يتمكن من إكمال التحقق المحلي من استجابة Standard.",
                "Chat Completions 信封已解析，但 ATT 无法完成 Standard 本地验收。",
                "Chat Completions 信封已解析，但 ATT 無法完成 Standard 本機驗收。",
                "The Chat Completions envelope was parsed, but ATT could not complete local Standard validation.",
                "L’enveloppe Chat Completions a été analysée, mais ATT n’a pas pu terminer la validation Standard locale.",
                "Конверт Chat Completions разобран, но ATT не смог завершить локальную проверку Standard.",
                "Se analizó el sobre de Chat Completions, pero ATT no pudo completar la validación Standard local.",
                "Chat Completions エンベロープは解析できましたが、ATT はローカルの Standard 検証を完了できませんでした。",
                "Chat Completions 봉투는 파싱했지만 ATT가 로컬 Standard 검증을 완료하지 못했습니다.",
                "Đã phân tích được phong bì Chat Completions nhưng ATT không thể hoàn tất xác thực Standard cục bộ.",
            ],
        ),
        "lua_binding_failed" => localized(
            locale,
            [
                "تم تحليل استجابة المزوّد، لكن تعذر على ATT تحويلها إلى قيمة إرجاع Lua؛ ولم يستلمها البرنامج النصي.",
                "Provider 响应已解析，但 ATT 无法将其物化为 Lua 返回值；脚本没有收到该响应。",
                "Provider 回應已解析，但 ATT 無法將其具現化為 Lua 回傳值；腳本沒有收到該回應。",
                "The provider response was parsed, but ATT could not materialize it as a Lua return value; the script did not receive it.",
                "La réponse du fournisseur a été analysée, mais ATT n’a pas pu la matérialiser comme valeur de retour Lua ; le script ne l’a pas reçue.",
                "Ответ провайдера разобран, но ATT не смог материализовать его как возвращаемое значение Lua; скрипт его не получил.",
                "La respuesta del proveedor se analizó, pero ATT no pudo materializarla como valor de retorno de Lua; el script no la recibió.",
                "プロバイダー応答は解析されましたが、ATT は Lua の戻り値として生成できず、スクリプトには渡されませんでした。",
                "공급자 응답을 파싱했지만 ATT가 Lua 반환값으로 구체화하지 못해 스크립트에 전달되지 않았습니다.",
                "Phản hồi của nhà cung cấp đã được phân tích nhưng ATT không thể tạo thành giá trị trả về Lua; tập lệnh không nhận được phản hồi.",
            ],
        ),
        _ => localized(
            locale,
            [
                "رفض ATT استجابة المزوّد للسبب ذي الرمز المبين أعلاه.",
                "ATT 按上方稳定原因代码拒绝了 Provider 响应。",
                "ATT 依上方穩定原因代碼拒絕了 Provider 回應。",
                "ATT rejected the provider response for the stable reason code shown above.",
                "ATT a rejeté la réponse du fournisseur pour le code de motif stable indiqué ci-dessus.",
                "ATT отклонил ответ провайдера по указанному выше стабильному коду причины.",
                "ATT rechazó la respuesta del proveedor por el código de motivo estable indicado arriba.",
                "ATT は上記の安定した理由コードによりプロバイダー応答を拒否しました。",
                "ATT는 위에 표시된 안정적인 이유 코드에 따라 공급자 응답을 거부했습니다.",
                "ATT từ chối phản hồi của nhà cung cấp theo mã lý do ổn định nêu trên.",
            ],
        ),
    }
}

fn standard_rejection_explanation(locale: UiLocale, code: &str) -> &'static str {
    let translations = match code {
        "missing" => [
            "لم تُرجع الاستجابة هذا المعرّف المتوقع.",
            "响应中缺少这个预期 ID。",
            "回應中缺少這個預期 ID。",
            "The response did not return this expected ID.",
            "La réponse ne contient pas cet ID attendu.",
            "В ответе отсутствует этот ожидаемый ID.",
            "La respuesta no devolvió este ID esperado.",
            "応答にこの期待 ID がありません。",
            "응답에 이 예상 ID가 없습니다.",
            "Phản hồi không trả về ID dự kiến này.",
        ],
        "duplicate" => [
            "أعادت الاستجابة هذا المعرّف أكثر من مرة.",
            "响应重复返回了这个 ID。",
            "回應重複傳回了這個 ID。",
            "The response returned this ID more than once.",
            "La réponse contient plusieurs fois cet ID.",
            "Ответ содержит этот ID более одного раза.",
            "La respuesta devolvió este ID más de una vez.",
            "応答でこの ID が複数回返されました。",
            "응답이 이 ID를 두 번 이상 반환했습니다.",
            "Phản hồi trả về ID này nhiều hơn một lần.",
        ],
        "invalid_shape" => [
            "لا يطابق شكل خرج هذا المعرّف العقد المتوقع.",
            "这个 ID 的输出结构不符合预期契约。",
            "這個 ID 的輸出結構不符合預期契約。",
            "This ID's output shape did not satisfy the expected contract.",
            "La structure de sortie de cet ID ne respecte pas le contrat attendu.",
            "Структура результата для этого ID не соответствует ожидаемому контракту.",
            "La estructura de salida de este ID no cumplía el contrato esperado.",
            "この ID の出力構造が期待される契約を満たしていません。",
            "이 ID의 출력 구조가 예상 계약을 충족하지 않았습니다.",
            "Cấu trúc đầu ra của ID này không đáp ứng hợp đồng dự kiến.",
        ],
        "line_count_mismatch" => [
            "لا يطابق عدد أسطر الترجمة عدد أسطر المصدر.",
            "译文行数与原文行数不一致。",
            "譯文行數與原文行數不一致。",
            "The translation line count did not match the source line count.",
            "Le nombre de lignes traduites ne correspond pas à celui de la source.",
            "Число строк перевода не совпадает с числом строк исходного текста.",
            "El número de líneas de la traducción no coincidía con el del texto original.",
            "翻訳の行数が原文の行数と一致しません。",
            "번역 줄 수가 원문 줄 수와 일치하지 않았습니다.",
            "Số dòng bản dịch không khớp với số dòng nguồn.",
        ],
        "invalid_line_text" => [
            "يحتوي سطر الترجمة على قيمة نصية غير صالحة.",
            "译文中的某一行不是有效文本。",
            "譯文中的某一行不是有效文字。",
            "A translation line did not contain valid text.",
            "Une ligne de traduction ne contient pas de texte valide.",
            "Одна из строк перевода содержит недопустимый текст.",
            "Una línea de traducción no contenía texto válido.",
            "翻訳行に有効なテキストがありません。",
            "번역 줄에 유효한 텍스트가 없습니다.",
            "Một dòng bản dịch không chứa văn bản hợp lệ.",
        ],
        "blank_line_mismatch" => [
            "لم يحافظ الإخراج على موضع السطر الفارغ.",
            "输出没有保持原文的空行位置。",
            "輸出沒有保持原文的空行位置。",
            "The output did not preserve the source blank-line position.",
            "La sortie n’a pas conservé la position de la ligne vide de la source.",
            "Результат не сохранил положение пустой строки исходного текста.",
            "La salida no conservó la posición de la línea en blanco del original.",
            "出力が原文の空行位置を保持していません。",
            "출력이 원문의 빈 줄 위치를 유지하지 않았습니다.",
            "Đầu ra không giữ nguyên vị trí dòng trống của nguồn.",
        ],
        "blank_translation" => [
            "الترجمة فارغة.",
            "译文为空。",
            "譯文為空。",
            "The translation was blank.",
            "La traduction est vide.",
            "Перевод пуст.",
            "La traducción estaba vacía.",
            "翻訳が空です。",
            "번역이 비어 있습니다.",
            "Bản dịch để trống.",
        ],
        "no_natural_language_text" => [
            "لا تحتوي الترجمة على نص لغوي طبيعي قابل للاستخدام.",
            "译文中没有可用的自然语言文本。",
            "譯文中沒有可用的自然語言文字。",
            "The translation contained no usable natural-language text.",
            "La traduction ne contient aucun texte en langue naturelle utilisable.",
            "Перевод не содержит пригодного естественного текста.",
            "La traducción no contenía texto utilizable en lenguaje natural.",
            "翻訳に使用可能な自然言語テキストがありません。",
            "번역에 사용할 수 있는 자연어 텍스트가 없습니다.",
            "Bản dịch không chứa văn bản ngôn ngữ tự nhiên có thể dùng.",
        ],
        "contains_byte_order_mark" => [
            "تحتوي الترجمة على علامة ترتيب بايت غير مسموح بها.",
            "译文包含不允许出现的字节顺序标记。",
            "譯文包含不允許出現的位元組順序標記。",
            "The translation contained a forbidden byte-order mark.",
            "La traduction contient une marque d’ordre des octets interdite.",
            "Перевод содержит запрещённую метку порядка байтов.",
            "La traducción contenía una marca de orden de bytes no permitida.",
            "翻訳に禁止されたバイトオーダーマークが含まれています。",
            "번역에 허용되지 않은 바이트 순서 표시가 있습니다.",
            "Bản dịch chứa dấu thứ tự byte không được phép.",
        ],
        "placeholder_mismatch" => [
            "لم تحتفظ الترجمة بالعنصر النائب المطلوب.",
            "译文没有保持必需的 Placeholder。",
            "譯文沒有保持必要的 Placeholder。",
            "The translation did not preserve a required placeholder.",
            "La traduction n’a pas conservé un placeholder requis.",
            "Перевод не сохранил обязательный placeholder.",
            "La traducción no conservó un marcador de posición obligatorio.",
            "翻訳が必須プレースホルダーを保持していません。",
            "번역이 필수 자리표시자를 유지하지 않았습니다.",
            "Bản dịch không giữ lại placeholder bắt buộc.",
        ],
        "unexpected_placeholder_token" => [
            "أدخلت الترجمة عنصرًا نائبًا غير متوقع.",
            "译文引入了意外的 Placeholder token。",
            "譯文引入了非預期的 Placeholder token。",
            "The translation introduced an unexpected placeholder token.",
            "La traduction a introduit un jeton de placeholder inattendu.",
            "Перевод добавил неожиданный токен placeholder.",
            "La traducción introdujo un token de marcador de posición inesperado.",
            "翻訳に予期しないプレースホルダートークンが追加されました。",
            "번역에 예상하지 않은 자리표시자 토큰이 추가되었습니다.",
            "Bản dịch đưa vào một token placeholder ngoài dự kiến.",
        ],
        "placeholder_normalization_ambiguous" => [
            "لا يمكن تطبيع العنصر النائب بأمان دون غموض.",
            "Placeholder 无法在没有歧义的情况下安全规范化。",
            "Placeholder 無法在沒有歧義的情況下安全正規化。",
            "The placeholder could not be normalized safely without ambiguity.",
            "Le placeholder ne peut pas être normalisé de façon sûre et non ambiguë.",
            "Placeholder невозможно безопасно нормализовать без неоднозначности.",
            "El marcador de posición no pudo normalizarse de forma segura y sin ambigüedad.",
            "プレースホルダーを曖昧さなく安全に正規化できません。",
            "자리표시자를 모호함 없이 안전하게 정규화할 수 없습니다.",
            "Không thể chuẩn hóa placeholder một cách an toàn mà không gây mơ hồ.",
        ],
        "source_residual" => [
            "ما زال جزء من نص المصدر غير المترجم موجودًا في الناتج.",
            "输出中仍残留未翻译的原文片段。",
            "輸出中仍殘留未翻譯的原文片段。",
            "The output still contained an untranslated source fragment.",
            "La sortie contient encore un fragment source non traduit.",
            "В результате остался непереведённый фрагмент исходного текста.",
            "La salida aún contenía un fragmento sin traducir del texto original.",
            "出力に未翻訳の原文断片が残っています。",
            "출력에 번역되지 않은 원문 조각이 남아 있습니다.",
            "Đầu ra vẫn chứa một đoạn nguồn chưa được dịch.",
        ],
        _ => [
            "رُفض هذا المعرّف للسبب ذي الرمز المبين أعلاه.",
            "这个 ID 因上方稳定原因代码而被拒绝。",
            "這個 ID 因上方穩定原因代碼而被拒絕。",
            "This ID was rejected for the stable reason code shown above.",
            "Cet ID a été rejeté pour le code de motif stable indiqué ci-dessus.",
            "Этот ID отклонён по указанному выше стабильному коду причины.",
            "Este ID se rechazó por el código de motivo estable indicado arriba.",
            "この ID は上記の安定した理由コードにより拒否されました。",
            "이 ID는 위에 표시된 안정적인 이유 코드에 따라 거부되었습니다.",
            "ID này bị từ chối theo mã lý do ổn định nêu trên.",
        ],
    };
    localized(locale, translations)
}

fn localized(locale: UiLocale, translations: [&'static str; 10]) -> &'static str {
    let index = match locale {
        UiLocale::Arabic => 0,
        UiLocale::SimplifiedChinese => 1,
        UiLocale::TraditionalChinese => 2,
        UiLocale::English => 3,
        UiLocale::French => 4,
        UiLocale::Russian => 5,
        UiLocale::Spanish => 6,
        UiLocale::Japanese => 7,
        UiLocale::Korean => 8,
        UiLocale::Vietnamese => 9,
    };
    translations[index]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;
    use crate::llm::{LlmFinishReason, LlmUsage};

    fn run_id() -> RunId {
        RunId::from_uuid(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("测试 RunId 应合法"),
        )
    }

    fn context() -> LlmCallReviewContext {
        LlmCallReviewContext::new("rpg_maker_mz", "project", "default", "openai")
    }

    fn standard_site(task: u64, attempt: u64) -> LlmCallSite {
        LlmCallSite::Standard {
            task_ordinal: NonZeroU64::new(task).expect("任务序号必须非零"),
            attempt: NonZeroU64::new(attempt).expect("尝试序号必须非零"),
        }
    }

    fn response() -> LlmResponse {
        LlmResponse::new(
            "content",
            LlmFinishReason::Stop,
            Some("request-id".to_owned()),
            Some("response-id".to_owned()),
            Some(LlmUsage::new(10, 20, 30)),
        )
    }

    #[tokio::test]
    async fn disabled_recorder_is_a_complete_no_op() {
        let recorder = LlmCallRecorder::disabled();
        let site = standard_site(1, 1);
        recorder
            .record_request(
                site,
                LlmCallRequestRecord::new(
                    Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                    br#"{"model":"m"}"#.to_vec(),
                ),
            )
            .await
            .expect("关闭记录时请求应为 no-op");
        recorder
            .authorize_send(site)
            .expect("关闭记录时发送准入应为 no-op");
        recorder
            .record_provider(
                site,
                LlmProviderRecord::response(
                    Duration::from_millis(1),
                    200,
                    LlmProviderHeaders::default(),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect("关闭记录时 Provider 应为 no-op");
        recorder
            .record_disposition(
                site,
                LlmCallDisposition::lua_delivered((&response()).into()),
            )
            .await
            .expect("关闭记录时处置应为 no-op");
        assert!(!recorder.is_enabled());
        assert!(recorder.run_root().is_none());
        assert!(recorder.failure().is_none());
    }

    #[tokio::test]
    async fn enabled_recorder_creates_fixed_path_and_all_three_synced_stages() {
        let directory = tempdir().expect("临时目录应建立成功");
        let recorder = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::SimplifiedChinese,
            context(),
        )
        .await
        .expect("审阅档案根应建立成功");
        let site = standard_site(12, 2);
        let request_body =
            r#"{"model":"model","messages":[{"role":"user","content":"姓名\n```inside"}]}"#
                .as_bytes()
                .to_vec();
        recorder
            .record_request(
                site,
                LlmCallRequestRecord::new(
                    Url::parse("https://user:secret@example.com/v1/chat?api_key=secret#fragment")
                        .expect("URL 应合法"),
                    request_body,
                ),
            )
            .await
            .expect("请求阶段应持久化");
        recorder
            .authorize_send(site)
            .expect("同步请求应取得发送准入");
        recorder
            .record_provider(
                site,
                LlmProviderRecord::response(
                    Duration::from_millis(15),
                    200,
                    LlmProviderHeaders::new(
                        Some("application/json".to_owned()),
                        Some("request-id".to_owned()),
                        None,
                    ),
                    b"{\"body\":\"````response\"}".to_vec(),
                ),
            )
            .await
            .expect("Provider 阶段应持久化");
        recorder
            .record_disposition(
                site,
                LlmCallDisposition::Standard(LlmStandardDisposition::new(
                    LlmStandardDispositionOutcome::Partial,
                    (&response()).into(),
                    vec![1],
                    vec![LlmRejectedOutput::new(
                        2,
                        "missing",
                        Some("expected_id=2".to_owned()),
                    )],
                )),
            )
            .await
            .expect("处置阶段应持久化");

        let path = recorder
            .run_root()
            .expect("启用后应有 Run 根")
            .join("standard/task-000012/attempt-002.md");
        let markdown = std::fs::read_to_string(path).expect("审阅 Markdown 应可读");
        assert!(markdown.contains("# LLM 调用审阅档案"));
        assert!(markdown.contains("request_complete"));
        assert!(markdown.contains("provider_complete"));
        assert!(markdown.contains("disposition_complete"));
        assert!(markdown.contains("envelope_parse_status = \"parsed\""));
        assert!(markdown.contains("\"content\": \"姓名\\n```inside\""));
        assert!(markdown.contains("{\"body\":\"````response\"}"));
        assert!(markdown.contains("reason_code = \"missing\""));
        assert!(markdown.contains("endpoint_without_query = \"https://example.com/v1/chat\""));
        assert!(!markdown.contains("api_key"));
        assert!(!markdown.contains("user:secret"));
        assert_eq!(recorder.active_count(), 0);
    }

    #[tokio::test]
    async fn concurrent_standard_and_lua_calls_stay_isolated_by_site_and_run_id() {
        async fn complete_call(recorder: LlmCallRecorder, site: LlmCallSite) {
            recorder
                .record_request(
                    site,
                    LlmCallRequestRecord::new(
                        Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                        b"{}".to_vec(),
                    ),
                )
                .await
                .expect("并发请求阶段应成功");
            recorder
                .authorize_send(site)
                .expect("并发调用应取得发送准入");
            tokio::task::yield_now().await;
            recorder
                .record_terminal_provider(
                    site,
                    LlmProviderRecord::response(
                        Duration::from_millis(1),
                        503,
                        LlmProviderHeaders::default(),
                        b"busy".to_vec(),
                    ),
                    "test_terminal",
                )
                .await
                .expect("并发调用应完成全部记录阶段");
        }

        let directory = tempdir().expect("临时目录应建立成功");
        let first_run = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::English,
            context(),
        )
        .await
        .expect("首个 Run 应建立");
        let second_run_id = RunId::from_uuid(
            Uuid::parse_str("660e8400-e29b-41d4-a716-446655440000")
                .expect("第二个测试 RunId 应合法"),
        );
        let second_run = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            second_run_id,
            UiLocale::English,
            context(),
        )
        .await
        .expect("不同 RunId 应建立独立根");

        tokio::join!(
            complete_call(first_run.clone(), standard_site(1, 1)),
            complete_call(first_run.clone(), standard_site(2, 1)),
            complete_call(
                first_run.clone(),
                LlmCallSite::Lua {
                    call: NonZeroU64::MIN
                }
            ),
        );

        let first_root = first_run.run_root().expect("首个 Run 根应存在");
        for relative in [
            "standard/task-000001/attempt-001.md",
            "standard/task-000002/attempt-001.md",
            "lua/call-000001.md",
        ] {
            assert!(
                first_root.join(relative).is_file(),
                "并发调用文件应保持固定路径：{relative}"
            );
        }
        assert_ne!(
            first_root,
            second_run.run_root().expect("第二个 Run 根应存在"),
            "不同 RunId 不得共享目录"
        );
        assert_eq!(first_run.active_count(), 0);
        assert_eq!(second_run.active_count(), 0);
    }

    #[tokio::test]
    async fn non_utf8_response_is_losslessly_encoded_as_base64() {
        let directory = tempdir().expect("临时目录应建立成功");
        let recorder = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::English,
            context(),
        )
        .await
        .expect("审阅档案根应建立成功");
        let site = LlmCallSite::Lua {
            call: NonZeroU64::new(1).expect("调用序号必须非零"),
        };
        recorder
            .record_request(
                site,
                LlmCallRequestRecord::new(
                    Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect("请求阶段应持久化");
        recorder
            .authorize_send(site)
            .expect("同步请求应取得发送准入");
        recorder
            .record_terminal_provider(
                site,
                LlmProviderRecord::response(
                    Duration::from_millis(1),
                    500,
                    LlmProviderHeaders::default(),
                    vec![0xff, 0x00, 0x80],
                ),
                "http_status",
            )
            .await
            .expect("终态 Provider 应持久化");

        let markdown = std::fs::read_to_string(
            recorder
                .run_root()
                .expect("启用后应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("审阅 Markdown 应可读");
        assert!(markdown.contains("encoding = \"base64\""));
        assert!(markdown.contains("byte_count = 3"));
        assert!(markdown.contains("/wCA"));
        assert_eq!(recorder.active_count(), 0);
    }

    #[tokio::test]
    async fn valid_utf8_response_preserves_control_text_and_line_endings() {
        let directory = tempdir().expect("临时目录应建立成功");
        let recorder = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::English,
            context(),
        )
        .await
        .expect("审阅档案根应建立成功");
        let site = LlmCallSite::Lua {
            call: NonZeroU64::MIN,
        };
        recorder
            .record_request(
                site,
                LlmCallRequestRecord::new(
                    Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect("请求阶段应持久化");
        recorder
            .authorize_send(site)
            .expect("同步请求应取得发送准入");
        let raw_body = "开头\0\r\n````\n结尾".as_bytes().to_vec();
        recorder
            .record_terminal_provider(
                site,
                LlmProviderRecord::response(
                    Duration::from_millis(1),
                    500,
                    LlmProviderHeaders::default(),
                    raw_body.clone(),
                ),
                "http_status_rejected",
            )
            .await
            .expect("有效 UTF-8 原始正文应持久化");

        let markdown = std::fs::read(
            recorder
                .run_root()
                .expect("启用后应有 Run 根")
                .join("lua/call-000001.md"),
        )
        .expect("审阅 Markdown 应可读");
        assert!(
            markdown
                .windows(raw_body.len())
                .any(|window| window == raw_body),
            "有效 UTF-8 正文的控制字符和原始换行必须逐字保留"
        );
    }

    #[tokio::test]
    async fn run_root_and_call_files_are_exclusive() {
        let directory = tempdir().expect("临时目录应建立成功");
        let first = LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::English,
            context(),
        )
        .await
        .expect("首个 Run 根应建立成功");
        let second = match LlmCallRecorder::start(
            directory.path().to_path_buf(),
            run_id(),
            UiLocale::English,
            context(),
        )
        .await
        {
            Ok(_) => panic!("同一 RunId 不得复用目录"),
            Err(error) => error,
        };
        assert_eq!(second.operation(), "create_run_root");

        let site = standard_site(1, 1);
        let request = || {
            LlmCallRequestRecord::new(
                Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                b"{}".to_vec(),
            )
        };
        first
            .record_request(site, request())
            .await
            .expect("首个调用文件应建立成功");
        first.authorize_send(site).expect("首个调用应取得发送准入");
        first
            .record_provider(
                site,
                LlmProviderRecord::response_not_received(Duration::ZERO),
            )
            .await
            .expect("首个调用 Provider 阶段应记录");
        first
            .record_disposition(site, LlmCallDisposition::rejected("test", None))
            .await
            .expect("首个调用应终结");
        let duplicate = first
            .record_request(site, request())
            .await
            .expect_err("同一调用文件不得覆盖");
        assert_eq!(duplicate.operation(), "create_new");
        let latched = first
            .record_request(standard_site(2, 1), request())
            .await
            .expect_err("首个失败后必须立即返回同一失败");
        assert_eq!(latched.operation(), duplicate.operation());
        assert_eq!(latched.path(), duplicate.path());
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum InjectedFailure {
        None,
        RequestCreate,
        RequestWrite,
        RequestSync,
        ProviderWrite,
        ProviderSync,
        DispositionWrite,
        DispositionSync,
    }

    struct MemoryStore {
        root: PathBuf,
        files: Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>,
        fail: InjectedFailure,
    }

    struct MemoryFile {
        content: Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>,
        fail: InjectedFailure,
    }

    impl LlmCallReviewStore for MemoryStore {
        fn run_root(&self) -> &Path {
            &self.root
        }

        fn create_call_file(
            &self,
            path: &Path,
            request: &[u8],
        ) -> Result<Box<dyn LlmCallReviewFile>, LlmCallReviewError> {
            let operation = match self.fail {
                InjectedFailure::RequestCreate => Some("create_new"),
                InjectedFailure::RequestWrite => Some("write_request"),
                InjectedFailure::RequestSync => Some("sync_request"),
                _ => None,
            };
            if let Some(operation) = operation {
                return Err(LlmCallReviewError::io(
                    operation,
                    path,
                    io::Error::from_raw_os_error(5),
                ));
            }
            lock_unpoisoned(&self.files).insert(path.to_path_buf(), request.to_vec());
            Ok(Box::new(MemoryFile {
                content: Arc::clone(&self.files),
                fail: self.fail,
            }))
        }
    }

    impl LlmCallReviewFile for MemoryFile {
        fn append_and_sync(
            &mut self,
            path: &Path,
            stage: ReviewStage,
            content: &[u8],
        ) -> Result<(), LlmCallReviewError> {
            let operation = match (self.fail, stage) {
                (InjectedFailure::ProviderWrite, ReviewStage::Provider)
                | (InjectedFailure::DispositionWrite, ReviewStage::Disposition) => {
                    Some(stage.write_operation())
                }
                (InjectedFailure::ProviderSync, ReviewStage::Provider)
                | (InjectedFailure::DispositionSync, ReviewStage::Disposition) => {
                    Some(stage.sync_operation())
                }
                _ => None,
            };
            if let Some(operation) = operation {
                return Err(LlmCallReviewError::io(
                    operation,
                    path,
                    io::Error::from_raw_os_error(5),
                ));
            }
            lock_unpoisoned(&self.content)
                .get_mut(path)
                .expect("内存调用文件必须存在")
                .extend_from_slice(content);
            Ok(())
        }
    }

    fn memory_recorder(fail: InjectedFailure) -> LlmCallRecorder {
        LlmCallRecorder::with_store(
            run_id(),
            UiLocale::English,
            context(),
            Arc::new(MemoryStore {
                root: PathBuf::from(r"C:\review"),
                files: Arc::new(Mutex::new(BTreeMap::new())),
                fail,
            }),
        )
    }

    #[tokio::test]
    async fn every_request_persistence_operation_is_a_pre_send_hard_latch() {
        for (failure, expected_operation) in [
            (InjectedFailure::RequestCreate, "create_new"),
            (InjectedFailure::RequestWrite, "write_request"),
            (InjectedFailure::RequestSync, "sync_request"),
        ] {
            let recorder = memory_recorder(failure);
            let first = recorder
                .record_request(
                    standard_site(1, 1),
                    LlmCallRequestRecord::new(
                        Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                        b"{}".to_vec(),
                    ),
                )
                .await
                .expect_err("注入的请求阶段失败必须返回");
            assert_eq!(first.operation(), expected_operation);
            assert_eq!(first.raw_os_error(), Some(5));
            assert_eq!(first.site(), Some(standard_site(1, 1)));
            assert_eq!(recorder.active_count(), 0);

            let same = recorder
                .record_provider(
                    standard_site(1, 1),
                    LlmProviderRecord::response_not_received(Duration::ZERO),
                )
                .await
                .expect_err("后续阶段必须返回同一首错");
            assert_eq!(same.operation(), first.operation());
            assert_eq!(same.path(), first.path());
            assert_eq!(same.raw_os_error(), first.raw_os_error());
        }
    }

    #[tokio::test]
    async fn every_provider_and_disposition_write_or_sync_failure_is_a_hard_latch() {
        for (failure, expected_operation) in [
            (InjectedFailure::ProviderWrite, "write_provider"),
            (InjectedFailure::ProviderSync, "sync_provider"),
            (InjectedFailure::DispositionWrite, "write_disposition"),
            (InjectedFailure::DispositionSync, "sync_disposition"),
        ] {
            let recorder = memory_recorder(failure);
            let site = standard_site(1, 1);
            recorder
                .record_request(
                    site,
                    LlmCallRequestRecord::new(
                        Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                        b"{}".to_vec(),
                    ),
                )
                .await
                .expect("请求阶段应成功");
            recorder
                .authorize_send(site)
                .expect("同步请求应取得发送准入");
            let result = if matches!(
                failure,
                InjectedFailure::ProviderWrite | InjectedFailure::ProviderSync
            ) {
                recorder
                    .record_provider(
                        site,
                        LlmProviderRecord::response_not_received(Duration::ZERO),
                    )
                    .await
            } else {
                recorder
                    .record_provider(
                        site,
                        LlmProviderRecord::response_not_received(Duration::ZERO),
                    )
                    .await
                    .expect("Provider 阶段应成功");
                recorder
                    .record_disposition(site, LlmCallDisposition::rejected("test", None))
                    .await
            };
            let error = result.expect_err("注入的同步失败必须返回");
            assert_eq!(error.operation(), expected_operation);
            assert_eq!(
                recorder.failure().expect("失败应被锁存").path(),
                error.path()
            );
        }
    }

    #[test]
    fn post_send_archive_diagnostic_states_that_the_request_may_have_happened() {
        let provider = LlmCallReviewError::io(
            "write_provider",
            Path::new(r"C:\review\lua\call-000001.md"),
            io::Error::from_raw_os_error(5),
        )
        .with_site(LlmCallSite::Lua {
            call: NonZeroU64::MIN,
        });
        let provider = serde_json::to_string(&provider.safe_diagnostic(
            DiagnosticStage::ModelRequest,
            DiagnosticImpact::ProgressPreserved,
        ))
        .expect("安全诊断应可序列化");
        assert!(provider.contains("llm_request_may_have_been_sent=true"));
        assert!(provider.contains("write_provider"));

        let request = LlmCallReviewError::io(
            "sync_request",
            Path::new(r"C:\review\lua\call-000001.md"),
            io::Error::from_raw_os_error(5),
        )
        .with_site(LlmCallSite::Lua {
            call: NonZeroU64::MIN,
        });
        let request = serde_json::to_string(
            &request.safe_diagnostic(DiagnosticStage::ModelRequest, DiagnosticImpact::Unchanged),
        )
        .expect("安全诊断应可序列化");
        assert!(
            !request.contains("llm_request_may_have_been_sent=true"),
            "请求阶段同步失败已经保证零发送，诊断不得制造不确定性"
        );
    }

    #[tokio::test]
    async fn active_call_finishes_after_another_task_latches_the_run_failure() {
        let recorder = memory_recorder(InjectedFailure::None);
        let active_site = standard_site(1, 1);
        let failing_site = standard_site(2, 1);
        for site in [active_site, failing_site] {
            recorder
                .record_request(
                    site,
                    LlmCallRequestRecord::new(
                        Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                        b"{}".to_vec(),
                    ),
                )
                .await
                .expect("两次请求均已进入 HTTP 前的持久化终点");
            recorder
                .authorize_send(site)
                .expect("两个测试调用均视为已经发出");
        }

        let failure_ready = Arc::new(tokio::sync::Notify::new());
        let failing_recorder = recorder.clone();
        let failing_ready = Arc::clone(&failure_ready);
        let failing = tokio::spawn(async move {
            let inner = failing_recorder.inner.as_ref().expect("测试记录器必须启用");
            let error = LlmCallReviewError::io(
                "sync_provider",
                &inner.call_path(failing_site),
                io::Error::from_raw_os_error(5),
            )
            .with_site(failing_site);
            inner.latch(error);
            failing_ready.notify_one();
        });

        let active_recorder = recorder.clone();
        let active = tokio::spawn(async move {
            failure_ready.notified().await;
            active_recorder
                .record_provider(
                    active_site,
                    LlmProviderRecord::response(
                        Duration::from_millis(1),
                        200,
                        LlmProviderHeaders::default(),
                        b"{}".to_vec(),
                    ),
                )
                .await
                .expect("已经发出的健康调用必须继续记录 Provider");
            active_recorder
                .record_disposition(
                    active_site,
                    LlmCallDisposition::lua_delivered((&response()).into()),
                )
                .await
                .expect("已经发出的健康调用必须继续记录处置终态");
        });
        failing.await.expect("故障任务不应 panic");
        active.await.expect("活动调用任务不应 panic");

        assert_eq!(recorder.active_count(), 1, "故障调用仍保留活动句柄");
        assert_eq!(
            recorder
                .failure()
                .expect("全局故障必须保持可见")
                .operation(),
            "sync_provider"
        );
        let blocked = recorder
            .record_request(
                standard_site(3, 1),
                LlmCallRequestRecord::new(
                    Url::parse("https://example.com/v1/chat").expect("URL 应合法"),
                    b"{}".to_vec(),
                ),
            )
            .await
            .expect_err("故障锁存后不得开始新调用");
        assert_eq!(blocked.operation(), "sync_provider");
    }

    #[test]
    fn dynamic_fence_is_longer_than_every_content_fence() {
        let rendered = fenced_block("text", "before\n``````\nafter");
        assert!(rendered.starts_with("```````text\n"));
        assert!(rendered.ends_with("```````\n"));
    }

    #[test]
    fn standard_disposition_renders_every_business_outcome_code() {
        for (outcome, code) in [
            (LlmStandardDispositionOutcome::Complete, "complete"),
            (LlmStandardDispositionOutcome::Partial, "partial"),
            (LlmStandardDispositionOutcome::Unavailable, "unavailable"),
        ] {
            let rendered = render_disposition(
                UiLocale::English,
                &LlmCallDisposition::Standard(LlmStandardDisposition::new(
                    outcome,
                    (&response()).into(),
                    vec![1],
                    vec![LlmRejectedOutput::new(2, "missing", None)],
                )),
                OffsetDateTime::UNIX_EPOCH,
            );
            assert!(rendered.contains(&format!("validation_outcome = \"{code}\"")));
            assert!(rendered.contains("envelope_parse_status = \"parsed\""));
            assert!(rendered.contains("id = 2"));
            assert!(rendered.contains("reason_code = \"missing\""));
        }
    }

    #[test]
    fn relative_paths_are_one_based_and_minimum_width_only() {
        assert_eq!(
            LlmCallRecorder::relative_path(standard_site(1_000_000, 1_000)),
            PathBuf::from("standard/task-1000000/attempt-1000.md")
        );
        assert_eq!(
            LlmCallRecorder::relative_path(LlmCallSite::Lua {
                call: NonZeroU64::new(1).expect("调用序号必须非零")
            }),
            PathBuf::from("lua/call-000001.md")
        );
    }
}
