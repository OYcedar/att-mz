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
    Diagnostic, DiagnosticReport, IoFailure, ObservabilityComponent, ObservabilityIssue,
    ObservabilityPathFailure, ObservabilityWriteFailure, RelatedFailureRelation, SafePath,
    StateEffect, render_diagnostic_report,
};
use crate::execution::llm_request::{
    LlmRequestAttemptOutcome, LlmRequestAttemptRecord, LlmRequestRetryWaitRecord,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ApiKeyRedactor, LlmClientRecordMetadata};
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::{
    SystemFileSystem, SystemFileSystemError, TerminalObservationOperation,
};
use crate::runtime::windows::WindowsFsError;
use crate::translation_protocol::TranslationResponseRepair;
use crate::windows_path::WindowsOrdinalCaseKeyError;

pub(crate) trait TaskRecordDiagnosticRecorder: Send + Sync {
    fn record_task_record_diagnostic(&self, report: DiagnosticReport);
}

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
    render_parallelism: usize,
    file_system: SystemFileSystem,
    diagnostics: Arc<dyn TaskRecordDiagnosticRecorder>,
    pending: Arc<Mutex<PendingTranslationTaskRecords>>,
}

impl MarkdownTranslationTaskRecordSink {
    pub(crate) fn new<R>(
        directory: PathBuf,
        run_id: String,
        client: LlmClientRecordMetadata,
        locale: UiLocale,
        cpu: RayonCpuExecutor,
        file_system: SystemFileSystem,
        diagnostics: R,
    ) -> Self
    where
        R: TaskRecordDiagnosticRecorder + 'static,
    {
        Self {
            directory,
            run_id,
            client,
            locale,
            // 任务记录在业务终态固定后才渲染。这里只继承已经选定的并行度，不能
            // 继续复用会被 Ctrl-C 取消或随业务根关闭的 CPU 执行器。
            render_parallelism: cpu.parallelism().get(),
            file_system,
            diagnostics: Arc::new(diagnostics),
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
        for document in documents.by_ref().take(self.render_parallelism) {
            writes.push(self.write(document, total_tasks));
        }
        while writes.next().await.is_some() {
            if let Some(document) = documents.next() {
                writes.push(self.write(document, total_tasks));
            }
        }
        if let Err(error) = self.file_system.shutdown().await {
            let redactor = self.client.api_key_redactor();
            self.diagnostics
                .record_task_record_diagnostic(task_record_file_system_report(
                    &error,
                    &self.directory,
                    redactor,
                    TaskRecordFileOperation::Shutdown,
                ));
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
        let markdown = tokio::task::spawn_blocking(move || {
            document.render(&run_id, &client, locale, total_tasks)
        })
        .await;
        let markdown =
            match markdown {
                Ok(Ok(markdown)) => markdown,
                Ok(Err(error)) => {
                    let path = redacted_safe_path(&path, self.client.api_key_redactor());
                    self.diagnostics
                        .record_task_record_diagnostic(DiagnosticReport::new(
                            StateEffect::Unchanged,
                            Diagnostic::observability(ObservabilityIssue::serialize_json(
                                ObservabilityComponent::TaskRecord,
                                path,
                                &error,
                            )),
                        ));
                    return;
                }
                Err(error) => {
                    self.diagnostics.record_task_record_diagnostic(
                        task_record_render_join_failure(&error, &path, &self.client),
                    );
                    return;
                }
            };
        if let Err(error) = self
            .file_system
            .write_new_terminal_observation_file(path.clone(), markdown.into_bytes())
            .await
        {
            self.diagnostics
                .record_task_record_diagnostic(task_record_file_system_report(
                    &error,
                    &path,
                    self.client.api_key_redactor(),
                    TaskRecordFileOperation::Write,
                ));
        }
    }
}

fn task_record_render_join_failure(
    source: &tokio::task::JoinError,
    path: &Path,
    client: &LlmClientRecordMetadata,
) -> DiagnosticReport {
    let redactor = client.api_key_redactor();
    let path = redacted_safe_path(path, redactor);
    let issue = if source.is_panic() {
        ObservabilityIssue::worker(ObservabilityComponent::TaskRecord, 1)
    } else {
        ObservabilityIssue::worker_cancelled(ObservabilityComponent::TaskRecord, 1)
    };
    DiagnosticReport::new(StateEffect::Unchanged, Diagnostic::observability(issue)).with_related(
        RelatedFailureRelation::Observability,
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::observability(ObservabilityIssue::write_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                None,
                1,
                ObservabilityWriteFailure::NotPersisted,
            )),
        ),
    )
}

#[derive(Clone, Copy)]
enum TaskRecordFileOperation {
    Create,
    Write,
    Flush,
    Sync,
    Cleanup,
    Shutdown,
}

fn task_record_file_system_report(
    error: &SystemFileSystemError,
    fallback_path: &Path,
    redactor: &ApiKeyRedactor,
    operation: TaskRecordFileOperation,
) -> DiagnosticReport {
    match error {
        SystemFileSystemError::ObservationCleanupFailed {
            temporary_path,
            operation: primary,
            cleanup,
        } => task_record_file_system_report(
            primary,
            fallback_path,
            redactor,
            TaskRecordFileOperation::Write,
        )
        .with_related(
            RelatedFailureRelation::Cleanup,
            task_record_file_system_report(
                cleanup,
                temporary_path,
                redactor,
                TaskRecordFileOperation::Cleanup,
            ),
        ),
        SystemFileSystemError::DirectChildRollbackFailed {
            operation: primary,
            rollback,
            ..
        }
        | SystemFileSystemError::ScopedEditRollbackFailed {
            operation: primary,
            rollback,
            ..
        } => task_record_file_system_report(primary, fallback_path, redactor, operation)
            .with_related(
                RelatedFailureRelation::Rollback,
                task_record_file_system_report(
                    rollback,
                    fallback_path,
                    redactor,
                    TaskRecordFileOperation::Cleanup,
                ),
            ),
        SystemFileSystemError::RecoveryCleanupFailed {
            target_root,
            source: cleanup,
            ..
        }
        | SystemFileSystemError::PublishedRecoveryCleanupFailed {
            target_root,
            source: cleanup,
            ..
        } => task_record_write_report(
            redacted_safe_path(target_root, redactor),
            ObservabilityWriteFailure::RecoveryRequired,
            operation,
        )
        .with_related(
            RelatedFailureRelation::Cleanup,
            task_record_file_system_report(
                cleanup,
                target_root,
                redactor,
                TaskRecordFileOperation::Cleanup,
            ),
        ),
        SystemFileSystemError::RecoveryOutcomeUnknown {
            target_root,
            source: finalization,
            ..
        } => task_record_write_report(
            redacted_safe_path(target_root, redactor),
            ObservabilityWriteFailure::OutcomeUnknown,
            operation,
        )
        .with_related(
            RelatedFailureRelation::Finalization,
            task_record_file_system_report(
                finalization,
                target_root,
                redactor,
                TaskRecordFileOperation::Cleanup,
            ),
        ),
        SystemFileSystemError::WorkerPanicked => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::observability(ObservabilityIssue::worker(
                ObservabilityComponent::TaskRecord,
                1,
            )),
        ),
        SystemFileSystemError::Closed => DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::observability(ObservabilityIssue::channel(
                ObservabilityComponent::TaskRecord,
                None,
                1,
            )),
        ),
        SystemFileSystemError::Io { path, source, .. } => {
            let operation = match error.terminal_observation_operation() {
                Some(TerminalObservationOperation::Create) => TaskRecordFileOperation::Create,
                Some(TerminalObservationOperation::Write) => TaskRecordFileOperation::Write,
                Some(TerminalObservationOperation::Flush) => TaskRecordFileOperation::Flush,
                Some(TerminalObservationOperation::Sync) => TaskRecordFileOperation::Sync,
                Some(TerminalObservationOperation::Cleanup) => TaskRecordFileOperation::Cleanup,
                None => operation,
            };
            task_record_write_report(
                redacted_safe_path(path, redactor),
                ObservabilityWriteFailure::Io {
                    failure: IoFailure::from_error(source),
                },
                operation,
            )
        }
        SystemFileSystemError::Windows(source) => {
            let (path, failure) = task_record_windows_failure(source, redactor);
            task_record_write_report(path, failure, operation)
        }
        SystemFileSystemError::WindowsOrdinalCaseKey { path, source } => {
            let failure = match source {
                WindowsOrdinalCaseKeyError::InputTooLarge { .. } => {
                    ObservabilityWriteFailure::Path {
                        failure: ObservabilityPathFailure::Invalid,
                    }
                }
                WindowsOrdinalCaseKeyError::WindowsApi { source, .. } => {
                    ObservabilityWriteFailure::Io {
                        failure: IoFailure::from_error(source),
                    }
                }
            };
            task_record_write_report(redacted_safe_path(path, redactor), failure, operation)
        }
        SystemFileSystemError::InvalidPath { path, .. } => task_record_write_report(
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Path {
                failure: ObservabilityPathFailure::Invalid,
            },
            operation,
        ),
        SystemFileSystemError::Cancelled { path, .. } => task_record_write_report(
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Cancelled,
            operation,
        ),
        SystemFileSystemError::WrongPublisherInstance => task_record_write_report(
            redacted_safe_path(fallback_path, redactor),
            ObservabilityWriteFailure::InvalidState,
            operation,
        ),
        SystemFileSystemError::InvalidStagedIdentity { path } => task_record_write_report(
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::IdentityChanged,
            operation,
        ),
        SystemFileSystemError::JournalCorrupt { path, .. }
        | SystemFileSystemError::RecoveryJournalCorrupt { path, .. } => task_record_write_report(
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::InvalidState,
            operation,
        ),
        SystemFileSystemError::RecoveryRequired { target_root, .. } => task_record_write_report(
            redacted_safe_path(target_root, redactor),
            ObservabilityWriteFailure::RecoveryRequired,
            operation,
        ),
        SystemFileSystemError::OutcomeUnknown { target_root, .. } => task_record_write_report(
            redacted_safe_path(target_root, redactor),
            ObservabilityWriteFailure::OutcomeUnknown,
            operation,
        ),
    }
}

fn task_record_windows_failure(
    source: &WindowsFsError,
    redactor: &ApiKeyRedactor,
) -> (SafePath, ObservabilityWriteFailure) {
    match source {
        WindowsFsError::Io { path, source, .. } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Io {
                failure: IoFailure::from_error(source),
            },
        ),
        WindowsFsError::ReparsePoint { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Path {
                failure: ObservabilityPathFailure::ReparsePoint,
            },
        ),
        WindowsFsError::NonLocalVolume { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Path {
                failure: ObservabilityPathFailure::NonLocalVolume,
            },
        ),
        WindowsFsError::NonNtfsVolume { path, .. } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Path {
                failure: ObservabilityPathFailure::NonNtfsVolume,
            },
        ),
        WindowsFsError::CaseSensitiveDirectory { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Path {
                failure: ObservabilityPathFailure::CaseSensitiveDirectory,
            },
        ),
        WindowsFsError::LockCancelled { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::Cancelled,
        ),
        WindowsFsError::RenameTargetExists { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::TargetExists,
        ),
        WindowsFsError::FileIdentityChanged { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::IdentityChanged,
        ),
    }
}

fn task_record_write_report(
    path: SafePath,
    failure: ObservabilityWriteFailure,
    operation: TaskRecordFileOperation,
) -> DiagnosticReport {
    let issue = match operation {
        TaskRecordFileOperation::Create => match failure {
            ObservabilityWriteFailure::Io { failure } => ObservabilityIssue::create_failure(
                ObservabilityComponent::TaskRecord,
                path,
                failure,
            ),
            _ => ObservabilityIssue::write_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                None,
                1,
                failure,
            ),
        },
        TaskRecordFileOperation::Write => ObservabilityIssue::write_failure(
            ObservabilityComponent::TaskRecord,
            Some(path),
            None,
            1,
            failure,
        ),
        TaskRecordFileOperation::Flush => match failure {
            ObservabilityWriteFailure::Io { failure } => ObservabilityIssue::flush_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                failure,
            ),
            _ => ObservabilityIssue::write_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                None,
                1,
                failure,
            ),
        },
        TaskRecordFileOperation::Sync => match failure {
            ObservabilityWriteFailure::Io { failure } => ObservabilityIssue::sync_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                failure,
            ),
            _ => ObservabilityIssue::write_failure(
                ObservabilityComponent::TaskRecord,
                Some(path),
                None,
                1,
                failure,
            ),
        },
        TaskRecordFileOperation::Cleanup => {
            ObservabilityIssue::cleanup_failure(ObservabilityComponent::TaskRecord, path, failure)
        }
        TaskRecordFileOperation::Shutdown => match failure {
            ObservabilityWriteFailure::ExecutorClosed => {
                ObservabilityIssue::channel(ObservabilityComponent::TaskRecord, None, 1)
            }
            _ => ObservabilityIssue::worker_cancelled(ObservabilityComponent::TaskRecord, 1),
        },
    };
    DiagnosticReport::new(StateEffect::Unchanged, Diagnostic::observability(issue))
}

fn redacted_safe_path(path: &Path, redactor: &ApiKeyRedactor) -> SafePath {
    SafePath::new(redactor.redact(&path.to_string_lossy()))
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
            usage,
            ..
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
    diagnostic: &DiagnosticReport,
) {
    let reason = api_key_redactor.redact(&render_diagnostic_report(diagnostic, localizer));
    output.push_str(&markdown_fence(&reason, "text"));
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

/// 把 Chat Completions 的 `message.content` 投影成可安全嵌入任务记录的证据。
///
/// 这里只执行现行敏感信息规则要求的文本与 JSON 字符串替换，并动态选择 Markdown
/// 围栏长度；调用方不能把结果解释为供应商响应或未经修改的字节副本。
pub(crate) fn render_raw_assistant(
    raw_assistant: &str,
    api_key_redactor: &ApiKeyRedactor,
) -> String {
    render_raw_assistant_with_language(raw_assistant, api_key_redactor, "json")
}

/// 把经过 JSON 修复的原始 Assistant 作为普通文本保存，避免围栏暗示正文自身是合法 JSON。
pub(crate) fn render_repaired_raw_assistant(
    raw_assistant: &str,
    api_key_redactor: &ApiKeyRedactor,
) -> String {
    render_raw_assistant_with_language(raw_assistant, api_key_redactor, "text")
}

fn render_raw_assistant_with_language(
    raw_assistant: &str,
    api_key_redactor: &ApiKeyRedactor,
    language: &str,
) -> String {
    markdown_fence(
        &api_key_redactor.redact_text_with_json_strings(raw_assistant),
        language,
    )
}

/// 渲染两个引擎共同使用的 JSON 修复事实，不复制 Assistant 正文。
pub(crate) fn render_json_repairs(output: &mut String, repairs: &[TranslationResponseRepair]) {
    if repairs.is_empty() {
        return;
    }
    output.push_str("\n## JSON Repairs\n\n");
    output.push_str("| Kind | Line | Column |\n");
    output.push_str("| --- | ---: | ---: |\n");
    for repair in repairs {
        let _ = writeln!(
            output,
            "| `{}` | {} | {} |",
            repair.kind_code(),
            repair.line(),
            repair.column(),
        );
    }
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
    use crate::observability::RunId;
    use crate::runtime::cpu::CpuExecutorConfig;
    use crate::runtime::filesystem::{
        SystemFileSystemConfig, TestObservationFaultPoint, register_test_observation_faults,
    };
    use crate::runtime::performance::RunPerformanceCounters;
    use crate::runtime::project_log::{
        DiagnosticScope, ProjectLogCommand, ProjectLogContext, ProjectLogEngine, ProjectLogRuntime,
        ProjectLogSink, ProjectLogger, RunFinished,
    };

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

    #[derive(Clone, Default)]
    struct SharedLogBytes(Arc<Mutex<Vec<u8>>>);

    impl SharedLogBytes {
        fn records(&self) -> Vec<serde_json::Value> {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            String::from_utf8(bytes)
                .expect("项目日志必须是 UTF-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("每行必须是 JSON"))
                .collect()
        }
    }

    impl ProjectLogSink for SharedLogBytes {
        fn write_record(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn sync(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestTaskRecordRecorder(ProjectLogger);

    impl TaskRecordDiagnosticRecorder for TestTaskRecordRecorder {
        fn record_task_record_diagnostic(&self, report: DiagnosticReport) {
            self.0
                .record_diagnostic(DiagnosticScope::TaskRecord, report)
                .expect("测试 TaskRecord occurrence 必须可写入");
        }
    }

    fn project_log(project: &str) -> (ProjectLogRuntime, TestTaskRecordRecorder, SharedLogBytes) {
        let bytes = SharedLogBytes::default();
        let context = ProjectLogContext::new(
            UiLocale::SimplifiedChinese,
            ProjectLogEngine::Generic,
            project,
            ProjectLogCommand::Extract,
        )
        .expect("测试项目日志 context 必须有效");
        let drop_report = DiagnosticReport::new(
            StateEffect::OutcomeUnknown,
            Diagnostic::observability(ObservabilityIssue::worker(
                ObservabilityComponent::ProjectLog,
                1,
            )),
        );
        let runtime = ProjectLogRuntime::start(
            context,
            RunId::for_test(1),
            bytes.clone(),
            Arc::new(RunPerformanceCounters::default()),
            drop_report,
        )
        .expect("测试项目日志必须启动");
        let logger = TestTaskRecordRecorder(runtime.logger());
        (runtime, logger, bytes)
    }

    fn finish_project_log(
        runtime: ProjectLogRuntime,
        bytes: &SharedLogBytes,
    ) -> Vec<serde_json::Value> {
        runtime
            .finish(RunFinished::Succeeded, Vec::new())
            .expect("测试项目日志必须正常结束");
        bytes.records()
    }

    fn public_diagnostic<'a>(
        records: &'a [serde_json::Value],
        event: &str,
    ) -> &'a serde_json::Value {
        let diagnostic = records
            .iter()
            .find(|record| record["event"] == event)
            .expect("预期的公开诊断必须存在");
        let payload = diagnostic["payload"]
            .as_object()
            .expect("公开诊断 payload 必须是对象");
        assert_eq!(payload.len(), 3);
        for field in ["object", "reason", "help"] {
            assert!(
                payload[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "公开诊断 {field} 必须是非空文本"
            );
        }
        diagnostic
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

    struct InvalidJsonArtifact;

    impl TranslationTaskRecordArtifact for InvalidJsonArtifact {
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
            serde_json::from_str::<serde_json::Value>("{").map(|_| String::new())
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
    async fn render_runs_on_a_blocking_worker_instead_of_the_async_calling_thread() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let (log_runtime, logger, log_bytes) = project_log("render-thread");
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
        let records = finish_project_log(log_runtime, &log_bytes);
        assert!(
            records
                .iter()
                .all(|record| record["event"] != "diagnostic.task_record")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_render_failure_is_logged_without_creating_a_record_file() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let (log_runtime, logger, log_bytes) = project_log("render-panic");
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
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        let records = finish_project_log(log_runtime, &log_bytes);
        let diagnostics = records
            .iter()
            .filter(|record| record["event"] == "diagnostic.task_record")
            .collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 2, "主故障和关联的未写入故障都必须记录");
        public_diagnostic(&records, "diagnostic.task_record");
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic["payload"].get("report").is_none())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serialization_failure_is_reported_without_internal_json_details() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let (log_runtime, logger, log_bytes) = project_log("render-json");
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "render-json".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger,
        );
        sink.submit(InvalidJsonArtifact);

        sink.finish().await;

        assert!(!record_directory.join("task-000001.md").exists());
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        let records = finish_project_log(log_runtime, &log_bytes);
        let diagnostic = public_diagnostic(&records, "diagnostic.task_record");
        let payload = serde_json::to_string(&diagnostic["payload"]).expect("诊断必须可序列化");
        for internal in ["report", "category", "line", "column"] {
            assert!(!payload.contains(internal));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flush_and_sync_failures_have_readable_public_diagnostics() {
        for (name, fault, with_cleanup_failure) in [
            ("flush", TestObservationFaultPoint::BeforeFlush, true),
            ("sync", TestObservationFaultPoint::BeforeSync, false),
        ] {
            let temporary = tempdir().expect("应建立临时目录");
            let record_directory = temporary.path().join("task-records");
            std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
            let target = record_directory.join("task-000001.md");
            let faults = if with_cleanup_failure {
                vec![fault, TestObservationFaultPoint::BeforeCleanup]
            } else {
                vec![fault]
            };
            register_test_observation_faults(target.clone(), faults);
            let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
                .expect("文件系统执行根应可建立");
            let cpu = cpu(1);
            let (log_runtime, logger, log_bytes) = project_log(name);
            let sink = MarkdownTranslationTaskRecordSink::new(
                record_directory,
                name.to_owned(),
                client(),
                UiLocale::SimplifiedChinese,
                cpu.clone(),
                file_system,
                logger,
            );
            sink.submit(ThreadRecordingArtifact {
                rendered_on: Arc::new(Mutex::new(None)),
            });

            sink.finish().await;

            assert!(!target.exists(), "写入故障不得暴露最终任务记录");
            cpu.shutdown().expect("测试 CPU 根应可关闭");
            let records = finish_project_log(log_runtime, &log_bytes);
            let diagnostic = public_diagnostic(&records, "diagnostic.task_record");
            assert!(diagnostic["payload"].get("related").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn existing_record_is_preserved_and_reports_redacted_typed_write_failure() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("unused-key").join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let target = record_directory.join("task-000001.md");
        std::fs::write(&target, "existing").expect("应建立既有任务记录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let (log_runtime, logger, log_bytes) = project_log("record-conflict");
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory,
            "record-conflict".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger,
        );
        sink.submit(ThreadRecordingArtifact {
            rendered_on: Arc::new(Mutex::new(None)),
        });

        sink.finish().await;

        assert_eq!(
            std::fs::read_to_string(&target).expect("既有任务记录必须保留"),
            "existing"
        );
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        let records = finish_project_log(log_runtime, &log_bytes);
        public_diagnostic(&records, "diagnostic.task_record");
        let serialized = serde_json::to_string(&records).expect("日志记录必须可序列化");
        assert!(!serialized.contains("unused-key"));
        assert!(!serialized.contains("target_exists"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_business_cpu_does_not_cancel_terminal_task_record_rendering() {
        let temporary = tempdir().expect("应建立临时目录");
        let record_directory = temporary.path().join("task-records");
        std::fs::create_dir_all(&record_directory).expect("应建立任务记录目录");
        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("文件系统执行根应可建立");
        let cpu = cpu(1);
        let (log_runtime, logger, log_bytes) = project_log("cancelled-cpu");
        let rendered_on = Arc::new(Mutex::new(None));
        let sink = MarkdownTranslationTaskRecordSink::new(
            record_directory.clone(),
            "cancelled-cpu".to_owned(),
            client(),
            UiLocale::SimplifiedChinese,
            cpu.clone(),
            file_system,
            logger.clone(),
        );
        sink.submit(ThreadRecordingArtifact {
            rendered_on: Arc::clone(&rendered_on),
        });
        cpu.cancel_waits();

        sink.finish().await;

        assert_eq!(
            std::fs::read_to_string(record_directory.join("task-000001.md"))
                .expect("业务 CPU 取消后仍应写入终态任务记录"),
            "# rendered\n"
        );
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        let records = finish_project_log(log_runtime, &log_bytes);
        assert!(
            records
                .iter()
                .all(|record| record["event"] != "diagnostic.task_record")
        );
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
        let (log_runtime, logger, log_bytes) = project_log("bounded-window");
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
        cpu.shutdown().expect("测试 CPU 根应可关闭");
        let records = finish_project_log(log_runtime, &log_bytes);
        assert!(
            records
                .iter()
                .all(|record| record["event"] != "diagnostic.task_record")
        );
    }
}
