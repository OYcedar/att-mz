//! 多个翻译引擎共用的模型任务记录写入能力。
//!
//! 各引擎负责把自己的 TaskBlock、响应验收和提交终态渲染成 Markdown。本模块只负责
//! 统一的 RunId 目录、文件命名、并发写入、敏感值处理和非致命写入诊断。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::llm_request::{
    LlmRequestAttemptOutcome, LlmRequestAttemptRecord, LlmRequestRetryWaitRecord,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::json_diagnostic::JsonErrorCategory;
use crate::llm::{ApiKeyRedactor, LlmClientRecordMetadata};
use crate::runtime::cpu::{CpuExecutorUnavailable, RayonCpuExecutor};
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::project_log::ProjectLogger;

/// 一个已经固定业务终态、可以异步写入的模型任务记录。
pub(crate) trait TranslationTaskRecordArtifact: Send {
    /// 返回从零开始的任务序号。
    fn task_index(&self) -> usize;

    /// 返回本次 Translate 的模型任务总数。
    fn total_tasks(&self) -> usize;

    /// 使用公共运行事实渲染当前引擎的任务记录。
    fn render(
        &self,
        run_id: &str,
        client: &LlmClientRecordMetadata,
        locale: UiLocale,
        total_tasks: usize,
    ) -> Result<String, serde_json::Error>;
}

/// 组合根按显式配置建立的单一生产 sink。
#[derive(Clone)]
pub(crate) enum ConfiguredTranslationTaskRecordSink {
    Disabled,
    Markdown(Box<MarkdownTranslationTaskRecordSink>),
}

impl ConfiguredTranslationTaskRecordSink {
    pub(crate) const fn disabled() -> Self {
        Self::Disabled
    }

    pub(crate) const fn enabled(&self) -> bool {
        matches!(self, Self::Markdown(_))
    }

    /// 接收已经固定的不可变终态，不在翻译业务编排中执行渲染或文件 I/O。
    pub(crate) fn submit(&self, document: impl TranslationTaskRecordArtifact + 'static) {
        if let Self::Markdown(sink) = self {
            sink.submit(document);
        }
    }

    /// 在本轮翻译业务终态全部固定后排空旁路文档。
    pub(crate) async fn finish(&self) {
        if let Self::Markdown(sink) = self {
            sink.finish().await;
        }
    }
}

#[derive(Default)]
struct PendingTranslationTaskRecords {
    total_tasks: usize,
    documents: Vec<Box<dyn TranslationTaskRecordArtifact>>,
}

/// 生产 Markdown sink；渲染与文件故障只并入项目日志健康状态。
#[derive(Clone)]
pub(crate) struct MarkdownTranslationTaskRecordSink {
    directory: PathBuf,
    run_id: String,
    client: LlmClientRecordMetadata,
    locale: UiLocale,
    cpu: RayonCpuExecutor,
    file_system: SystemFileSystem,
    warnings: ProjectLogger,
    pending: Arc<Mutex<PendingTranslationTaskRecords>>,
}

impl MarkdownTranslationTaskRecordSink {
    pub(crate) fn new(
        directory: PathBuf,
        run_id: String,
        client: LlmClientRecordMetadata,
        locale: UiLocale,
        cpu: RayonCpuExecutor,
        file_system: SystemFileSystem,
        warnings: ProjectLogger,
    ) -> Self {
        Self {
            directory,
            run_id,
            client,
            locale,
            cpu,
            file_system,
            warnings,
            pending: Arc::new(Mutex::new(PendingTranslationTaskRecords::default())),
        }
    }

    pub(crate) fn submit(&self, document: impl TranslationTaskRecordArtifact + 'static) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.total_tasks = pending
            .total_tasks
            .max(document.total_tasks())
            .max(document.task_index().saturating_add(1));
        pending.documents.push(Box::new(document));
    }

    pub(crate) async fn finish(&self) {
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *pending)
        };
        let total_tasks = pending.total_tasks;
        let mut writes = FuturesUnordered::new();
        let mut documents = pending.documents.into_iter();
        // 窗口限制的是同时存在的 render+write Future，不限制本轮文档总量。
        for document in documents.by_ref().take(self.cpu.parallelism().get()) {
            writes.push(self.write(document, total_tasks));
        }
        while writes.next().await.is_some() {
            if let Some(document) = documents.next() {
                writes.push(self.write(document, total_tasks));
            }
        }
        if let Err(error) = self.file_system.shutdown().await {
            let redactor = self.client.api_key_redactor();
            let diagnostics = error
                .into_failure_report(
                    DiagnosticStage::Logging,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .public_diagnostics()
                .cloned()
                .map(|diagnostic| diagnostic.map_dynamic_text(|value| redactor.redact(value)))
                .collect::<Vec<_>>();
            self.warnings.record_task_record_failures(diagnostics);
        }
    }

    async fn write(&self, document: Box<dyn TranslationTaskRecordArtifact>, total_tasks: usize) {
        let path = self.directory.join(format!(
            "task-{:06}.md",
            document.task_index().saturating_add(1)
        ));
        let run_id = self.run_id.clone();
        let client = self.client.clone();
        let locale = self.locale;
        let markdown = self
            .cpu
            .execute(move || document.render(&run_id, &client, locale, total_tasks))
            .await;
        let markdown = match markdown {
            Ok(Ok(markdown)) => markdown,
            Ok(Err(error)) => {
                let path = self
                    .client
                    .api_key_redactor()
                    .redact(&path.to_string_lossy());
                let category = JsonErrorCategory::from(&error);
                self.warnings.record_task_record_failure(
                    SafeDiagnostic::new(
                        DiagnosticCode::LogSerialize,
                        DiagnosticStage::Logging,
                        DiagnosticSubject::path(&path),
                        DiagnosticReason::failure_with_detail(
                            DiagnosticFailureKind::RequestSerializationFailed,
                            format!(
                                "json_category={category}; line={}; column={}",
                                error.line(),
                                error.column()
                            ),
                        ),
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::ReportBug,
                    )
                    .with_recovery(RecoveryFact::path(&path)),
                );
                return;
            }
            Err(error) => {
                self.warnings
                    .record_task_record_failure(task_record_render_cpu_failure(
                        &error,
                        &path,
                        &self.client,
                    ));
                return;
            }
        };
        if let Err(error) = self
            .file_system
            .write_new_terminal_observation_file(path.clone(), markdown.into_bytes())
            .await
        {
            let diagnostics = error
                .into_failure_report(
                    DiagnosticStage::Logging,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .public_diagnostics()
                .cloned()
                .map(|diagnostic| {
                    let redactor = self.client.api_key_redactor();
                    let recovery_path = redactor.redact(&path.to_string_lossy());
                    diagnostic
                        .map_dynamic_text(|value| redactor.redact(value))
                        .with_recovery(RecoveryFact::path(recovery_path))
                })
                .collect::<Vec<_>>();
            self.warnings.record_task_record_failures(diagnostics);
        }
    }
}

fn task_record_render_cpu_failure(
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
    path: &Path,
    client: &LlmClientRecordMetadata,
) -> SafeDiagnostic {
    let redactor = client.api_key_redactor();
    let path = redactor.redact(&path.to_string_lossy());
    source
        .safe_diagnostic_source(
            DiagnosticStage::Logging,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        )
        .map_dynamic_text(|value| redactor.redact(value))
        .with_recovery(RecoveryFact::path(path))
}

/// 按任务记录本地化规则渲染一次共享 LLM attempt。
pub(crate) fn render_task_record_attempt(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    attempt: &LlmRequestAttemptRecord,
) -> Result<(), serde_json::Error> {
    let number = attempt.attempt.get();
    let duration = render_duration(localizer, attempt.duration);
    match &attempt.outcome {
        LlmRequestAttemptOutcome::Succeeded {
            finish_reason,
            provider_request_id,
            provider_response_id,
            usage,
        } => {
            let finish_reason =
                markdown_inline_code(&api_key_redactor.redact(&finish_reason.to_string()));
            let _ = write!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptSucceeded {
                        number: number as u64,
                        finish_reason: &finish_reason,
                    }
                )
            );
            if let Some(usage) = usage {
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptTokenUsage {
                        prompt: usage.prompt_tokens(),
                        completion: usage.completion_tokens(),
                        total: usage.total_tokens(),
                    },
                ));
            }
            output.push_str(&task_record_text(
                localizer,
                UiMessage::TaskRecordAttemptDuration {
                    duration: &duration,
                },
            ));
            if let Some(request_id) = provider_request_id {
                let request_id = markdown_inline_code(&api_key_redactor.redact(request_id));
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRequestId {
                        request_id: &request_id,
                    },
                ));
            }
            if let Some(response_id) = provider_response_id {
                let response_id = markdown_inline_code(&api_key_redactor.redact(response_id));
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptResponseId {
                        response_id: &response_id,
                    },
                ));
            }
            output.push('\n');
        }
        LlmRequestAttemptOutcome::Retryable {
            diagnostic,
            retry_after,
            retry_wait,
        } => {
            let _ = write!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRetryable {
                        number: number as u64,
                        code: diagnostic.code.as_str(),
                        duration: &duration,
                    }
                )
            );
            if let Some(retry_after) = retry_after {
                let retry_after = render_duration(localizer, *retry_after);
                output.push_str(&task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptRetryAfter {
                        duration: &retry_after,
                    },
                ));
            }
            if let Some(retry_wait) = retry_wait {
                let rendered = match retry_wait {
                    LlmRequestRetryWaitRecord::Retried { duration } => {
                        let duration = render_duration(localizer, *duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitRetry {
                                duration: &duration,
                            },
                        )
                    }
                    LlmRequestRetryWaitRecord::CompletedBeforeNextAttempt { duration } => {
                        let duration = render_duration(localizer, *duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitCompleted {
                                duration: &duration,
                            },
                        )
                    }
                    LlmRequestRetryWaitRecord::CancelledWhileWaiting { planned_duration } => {
                        let duration = render_duration(localizer, *planned_duration);
                        task_record_text(
                            localizer,
                            UiMessage::TaskRecordAttemptWaitCancelled {
                                duration: &duration,
                            },
                        )
                    }
                };
                output.push_str(&rendered);
            }
            output.push('\n');
            render_diagnostic_reason(output, localizer, api_key_redactor, diagnostic);
        }
        LlmRequestAttemptOutcome::Failed { diagnostic } => {
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptFailed {
                        number: number as u64,
                        code: diagnostic.code.as_str(),
                        duration: &duration,
                    }
                )
            );
            render_diagnostic_reason(output, localizer, api_key_redactor, diagnostic);
        }
        LlmRequestAttemptOutcome::Cancelled => {
            let _ = writeln!(
                output,
                "- {}",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordAttemptCancelled {
                        number: number as u64,
                        duration: &duration,
                    }
                )
            );
        }
    }
    Ok(())
}

fn render_diagnostic_reason(
    output: &mut String,
    localizer: &UiLocalizer,
    api_key_redactor: &ApiKeyRedactor,
    diagnostic: &SafeDiagnostic,
) {
    let reason = markdown_inline_code(
        &api_key_redactor.redact(&diagnostic.reason.render_localized(localizer)),
    );
    let _ = writeln!(
        output,
        "  - {}",
        task_record_text(
            localizer,
            UiMessage::TaskRecordStructuredReason { reason: &reason }
        )
    );
}

pub(crate) fn markdown_heading_id(id: &str) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        id.to_owned()
    } else {
        markdown_inline_code(id)
    }
}

pub(crate) fn markdown_inline_code(value: &str) -> String {
    let encoded;
    let value = if value.chars().any(char::is_control)
        || value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace)
    {
        encoded = serde_json::to_string(value).expect("字符串必须能够序列化为 JSON");
        encoded.as_str()
    } else {
        value
    };
    let longest_backtick_run = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_backtick_run.saturating_add(1).max(1));
    if value.starts_with('`') || value.ends_with('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

pub(crate) fn markdown_fence(content: &str, language: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in content.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    let mut output = format!("{fence}{language}\n");
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output.push('\n');
    output
}

pub(crate) fn recorded_at_utc(now: OffsetDateTime) -> String {
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

/// 任务文档只向 Fluent 传入稳定事实，并移除终端输出使用的方向隔离符。
pub(crate) fn task_record_text(localizer: &UiLocalizer, message: UiMessage<'_>) -> String {
    localizer
        .format(message)
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect()
}

pub(crate) fn render_duration(localizer: &UiLocalizer, duration: Duration) -> String {
    if duration.as_secs() != 0 {
        let value = format!("{:.3}", duration.as_secs_f64());
        task_record_text(
            localizer,
            UiMessage::TaskRecordDurationSeconds { value: &value },
        )
    } else {
        let value = duration.as_millis().to_string();
        task_record_text(
            localizer,
            UiMessage::TaskRecordDurationMilliseconds { value: &value },
        )
    }
}

pub(crate) fn render_client_parameters(
    client: &LlmClientRecordMetadata,
) -> Result<String, serde_json::Error> {
    client
        .api_key_redactor()
        .redact_json_pretty(client.parameters())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::ThreadId;

    use secrecy::SecretString;
    use serde_json::Map;
    use tempfile::tempdir;

    use super::*;
    use crate::runtime::cpu::CpuExecutorConfig;
    use crate::runtime::filesystem::SystemFileSystemConfig;
    use crate::runtime::project_log::start_project_log;

    fn client() -> LlmClientRecordMetadata {
        LlmClientRecordMetadata::new(
            "https://example.test".to_owned(),
            "model".to_owned(),
            Map::new(),
            ApiKeyRedactor::new(SecretString::from("unused-key")),
        )
    }

    fn cpu(worker_threads: usize) -> RayonCpuExecutor {
        RayonCpuExecutor::start(CpuExecutorConfig::fixed(
            NonZeroUsize::new(worker_threads).expect("测试 CPU worker 数必须非零"),
        ))
        .expect("测试 CPU 根应可启动")
    }

    struct ThreadRecordingArtifact {
        rendered_on: Arc<Mutex<Option<ThreadId>>>,
    }

    impl TranslationTaskRecordArtifact for ThreadRecordingArtifact {
        fn task_index(&self) -> usize {
            0
        }

        fn total_tasks(&self) -> usize {
            1
        }

        fn render(
            &self,
            _run_id: &str,
            _client: &LlmClientRecordMetadata,
            _locale: UiLocale,
            _total_tasks: usize,
        ) -> Result<String, serde_json::Error> {
            *self
                .rendered_on
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(std::thread::current().id());
            Ok("# rendered\n".to_owned())
        }
    }

    struct PanickingArtifact;

    impl TranslationTaskRecordArtifact for PanickingArtifact {
        fn task_index(&self) -> usize {
            0
        }

        fn total_tasks(&self) -> usize {
            1
        }

        fn render(
            &self,
            _run_id: &str,
            _client: &LlmClientRecordMetadata,
            _locale: UiLocale,
            _total_tasks: usize,
        ) -> Result<String, serde_json::Error> {
            panic!("测试任务记录渲染 panic")
        }
    }

    #[derive(Default)]
    struct InFlightTracker {
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    impl InFlightTracker {
        fn enter(&self) {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(active, Ordering::AcqRel);
        }

        fn leave(&self) {
            let previous = self.active.fetch_sub(1, Ordering::AcqRel);
            assert!(previous > 0, "测试 in-flight 计数不得下溢");
        }
    }

    #[derive(Default)]
    struct RenderGate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl RenderGate {
        fn wait(&self) {
            let mut open = self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = self
                    .changed
                    .wait(open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn release(&self) {
            *self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.changed.notify_all();
        }
    }

    struct WindowArtifact {
        index: usize,
        total_tasks: usize,
        task_index_calls: AtomicUsize,
        admitted: AtomicBool,
        tracker: Arc<InFlightTracker>,
        gate: Arc<RenderGate>,
    }

    impl TranslationTaskRecordArtifact for WindowArtifact {
        fn task_index(&self) -> usize {
            let previous_calls = self.task_index_calls.fetch_add(1, Ordering::AcqRel);
            if previous_calls > 0 && !self.admitted.swap(true, Ordering::AcqRel) {
                self.tracker.enter();
            }
            self.index
        }

        fn total_tasks(&self) -> usize {
            self.total_tasks
        }

        fn render(
            &self,
            _run_id: &str,
            _client: &LlmClientRecordMetadata,
            _locale: UiLocale,
            _total_tasks: usize,
        ) -> Result<String, serde_json::Error> {
            self.gate.wait();
            Ok(format!("# task {}\n", self.index + 1))
        }
    }

    impl Drop for WindowArtifact {
        fn drop(&mut self) {
            if self.admitted.load(Ordering::Acquire) {
                self.tracker.leave();
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn render_runs_on_the_cpu_pool_instead_of_the_async_calling_thread() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let log_runtime =
            start_project_log(temporary.path().join("logs"), "render-thread".to_owned());
        let logger = log_runtime.logger();
        let rendered_on = Arc::new(Mutex::new(None));
        let calling_thread = std::thread::current().id();
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "render-thread".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger,
        );
        sink.submit(ThreadRecordingArtifact {
            rendered_on: Arc::clone(&rendered_on),
        });

        sink.finish().await;

        let rendered_thread = rendered_on
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("任务记录 render 应实际执行");
        assert_ne!(
            rendered_thread, calling_thread,
            "任务记录 render 不能占用 async 调用线程"
        );
        assert_eq!(
            std::fs::read_to_string(record_directory.join("task-000001.md"))
                .expect("应写入渲染结果"),
            "# rendered\n"
        );
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        drop(log_runtime);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cpu_render_failure_is_logged_without_creating_a_record_file() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let log_runtime =
            start_project_log(temporary.path().join("logs"), "render-panic".to_owned());
        let logger = log_runtime.logger();
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "render-panic".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger.clone(),
        );
        sink.submit(PanickingArtifact);

        sink.finish().await;

        assert!(!record_directory.join("task-000001.md").exists());
        assert_eq!(logger.health().task_record_failures, 1);
        let diagnostic = logger
            .take_warning()
            .and_then(|warning| warning.task_records)
            .and_then(|warning| warning.diagnostic)
            .expect("CPU render 失败应留下任务记录诊断");
        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::Logging);
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::failure(DiagnosticFailureKind::WorkerPanicked)
        );
        assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
        assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        drop(log_runtime);
    }

    #[test]
    fn cpu_render_failures_keep_their_specific_logging_diagnostics() {
        let path = PathBuf::from("task-000001.md");
        let client = client();
        let cases = [
            (
                CpuTaskExecutionError::Cancelled,
                DiagnosticFailureKind::LockCancelled,
                DiagnosticAction::Retry,
            ),
            (
                CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown),
                DiagnosticFailureKind::ExecutorClosed,
                DiagnosticAction::Retry,
            ),
            (
                CpuTaskExecutionError::TaskPanicked,
                DiagnosticFailureKind::WorkerPanicked,
                DiagnosticAction::ReportBug,
            ),
        ];
        for (source, failure, action) in cases {
            let diagnostic = task_record_render_cpu_failure(&source, &path, &client);
            assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
            assert_eq!(diagnostic.stage, DiagnosticStage::Logging);
            assert_eq!(diagnostic.reason, DiagnosticReason::failure(failure));
            assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
            assert_eq!(diagnostic.action, action);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn many_documents_keep_only_cpu_parallelism_writes_in_flight() {
        const WORKERS: usize = 2;
        const DOCUMENTS: usize = 5;

        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(WORKERS);
        assert_eq!(cpu.parallelism().get(), WORKERS);
        let log_runtime =
            start_project_log(temporary.path().join("logs"), "bounded-window".to_owned());
        let logger = log_runtime.logger();
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "bounded-window".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger.clone(),
        );
        let tracker = Arc::new(InFlightTracker::default());
        let gate = Arc::new(RenderGate::default());
        for index in 0..DOCUMENTS {
            sink.submit(WindowArtifact {
                index,
                total_tasks: DOCUMENTS,
                task_index_calls: AtomicUsize::new(0),
                admitted: AtomicBool::new(false),
                tracker: Arc::clone(&tracker),
                gate: Arc::clone(&gate),
            });
        }

        let observe_window = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while tracker.active.load(Ordering::Acquire) < WORKERS {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("首个任务记录窗口应及时开始");
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                tracker.peak.load(Ordering::Acquire),
                WORKERS,
                "等待 CPU 的任务记录 Future 不得按文档总量建立"
            );
            gate.release();
        };
        tokio::join!(sink.finish(), observe_window);

        assert_eq!(tracker.active.load(Ordering::Acquire), 0);
        assert_eq!(
            std::fs::read_dir(&record_directory)
                .expect("应读取任务记录目录")
                .count(),
            DOCUMENTS
        );
        assert_eq!(logger.health().task_record_failures, 0);
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        drop(log_runtime);
    }
}
