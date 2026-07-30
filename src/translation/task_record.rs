//! 多个翻译引擎共用的模型任务记录写入能力。
//!
//! 各引擎负责把自己的 TaskBlock、响应验收和提交终态渲染成 Markdown。本模块只负责
//! 统一的 RunId 目录、文件命名、并发写入、敏感值处理和非致命写入诊断。

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::llm_request::{
    LlmRequestAttemptOutcome, LlmRequestAttemptRecord, LlmRequestRetryWaitRecord,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::json_diagnostic::JsonErrorCategory;
use crate::llm::{ApiKeyRedactor, LlmClientRecordMetadata};
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

/// 生产 Markdown sink；文件故障只并入项目日志健康状态。
#[derive(Clone)]
pub(crate) struct MarkdownTranslationTaskRecordSink {
    directory: PathBuf,
    run_id: String,
    client: LlmClientRecordMetadata,
    locale: UiLocale,
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
        file_system: SystemFileSystem,
        warnings: ProjectLogger,
    ) -> Self {
        Self {
            directory,
            run_id,
            client,
            locale,
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
        pending.total_tasks = pending.total_tasks.max(document.total_tasks());
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
        let total_tasks =
            pending
                .documents
                .iter()
                .fold(pending.total_tasks, |total_tasks, document| {
                    total_tasks
                        .max(document.total_tasks())
                        .max(document.task_index().saturating_add(1))
                });
        let mut writes = FuturesUnordered::new();
        for document in pending.documents {
            writes.push(self.write(document, total_tasks));
        }
        while writes.next().await.is_some() {}
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
        let markdown = document.render(&self.run_id, &self.client, self.locale, total_tasks);
        let markdown = match markdown {
            Ok(markdown) => markdown,
            Err(error) => {
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
        .redact_json_pretty(&Value::Object(client.parameters().clone()))
}
