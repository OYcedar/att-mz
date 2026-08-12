//! 多个翻译引擎共用的模型任务记录写入能力。
//!
//! 各引擎负责把自己的 TaskBlock、响应验收和提交终态渲染成 Markdown。本模块只负责
//! 统一的 RunId 目录、文件命名、并发写入、敏感值处理和非致命写入诊断。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, ObservabilityComponent, ObservabilityIssue,
    ObservabilityPathFailure, ObservabilityWriteFailure, RelatedFailureRelation, SafePath,
    StateEffect,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::ApiKeyRedactor;
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::{
    SystemFileSystem, SystemFileSystemError, TerminalObservationOperation,
};
use crate::runtime::windows::WindowsFsError;
use crate::windows_path::WindowsOrdinalCaseKeyError;

pub(crate) trait TaskRecordDiagnosticRecorder: Send + Sync {
    fn record_task_record_diagnostic(&self, report: DiagnosticReport);
}

/// 一个已经固定业务终态、可以异步写入的模型任务记录。
pub(crate) trait TranslationTaskRecordArtifact: Send {
    /// 返回从零开始的任务序号。
    fn task_index(&self) -> usize;

    /// 渲染当前引擎的任务记录。
    fn render(&self, redactor: &ApiKeyRedactor, locale: UiLocale) -> String;
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
    documents: Vec<Box<dyn TranslationTaskRecordArtifact>>,
}

/// 生产 Markdown sink；渲染与文件故障只并入项目日志健康状态。
#[derive(Clone)]
pub(crate) struct MarkdownTranslationTaskRecordSink {
    directory: PathBuf,
    redactor: Arc<ApiKeyRedactor>,
    locale: UiLocale,
    render_parallelism: usize,
    file_system: SystemFileSystem,
    diagnostics: Arc<dyn TaskRecordDiagnosticRecorder>,
    pending: Arc<Mutex<PendingTranslationTaskRecords>>,
}

impl MarkdownTranslationTaskRecordSink {
    pub(crate) fn new<R>(
        directory: PathBuf,
        redactor: Arc<ApiKeyRedactor>,
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
            redactor,
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
        let mut writes = FuturesUnordered::new();
        let mut documents = pending.documents.into_iter();
        // 窗口限制的是同时存在的 render+write Future，不限制本轮文档总量。
        for document in documents.by_ref().take(self.render_parallelism) {
            writes.push(self.write(document));
        }
        while writes.next().await.is_some() {
            if let Some(document) = documents.next() {
                writes.push(self.write(document));
            }
        }
        if let Err(error) = self.file_system.shutdown().await {
            self.diagnostics
                .record_task_record_diagnostic(task_record_file_system_report(
                    &error,
                    &self.directory,
                    &self.redactor,
                    TaskRecordFileOperation::Shutdown,
                ));
        }
    }

    async fn write(&self, document: Box<dyn TranslationTaskRecordArtifact>) {
        let path = self.directory.join(format!(
            "task-{:06}.md",
            document.task_index().saturating_add(1)
        ));
        let redactor = Arc::clone(&self.redactor);
        let locale = self.locale;
        let markdown =
            tokio::task::spawn_blocking(move || document.render(&redactor, locale)).await;
        let markdown =
            match markdown {
                Ok(markdown) => markdown,
                Err(error) => {
                    self.diagnostics.record_task_record_diagnostic(
                        task_record_render_join_failure(&error, &path, &self.redactor),
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
                    &self.redactor,
                    TaskRecordFileOperation::Write,
                ));
        }
    }
}

fn task_record_render_join_failure(
    source: &tokio::task::JoinError,
    path: &Path,
    redactor: &ApiKeyRedactor,
) -> DiagnosticReport {
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
        WindowsFsError::RenameTargetUnconfirmed { path } => (
            redacted_safe_path(path, redactor),
            ObservabilityWriteFailure::OutcomeUnknown,
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

/// 使用 JSON 高亮呈现原始 Assistant 正文。
///
/// 模型已经返回规范围栏时，只移除这一层围栏，再由本模块建立能够安全包住正文的
/// `json` 围栏；裸 JSON 和无效响应也不会作为 Markdown 结构直接注入任务记录。
pub(crate) fn markdown_json_fence(content: &str) -> String {
    let body = canonical_json_fence_body(content).unwrap_or(content);
    markdown_fence(body, "json")
}

fn canonical_json_fence_body(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    let body = trimmed
        .strip_prefix("```json\r\n")
        .or_else(|| trimmed.strip_prefix("```json\n"))?;
    body.strip_suffix("\r\n```")
        .or_else(|| body.strip_suffix("\n```"))
}

/// 任务记录中一个连续临时 ID 集合的验收摘要。
pub(crate) struct TranslationTaskRecordOutputSummary {
    requested: usize,
    accepted: Vec<usize>,
    unaccepted: Vec<usize>,
}

impl TranslationTaskRecordOutputSummary {
    pub(crate) fn new(requested: usize, accepted: impl IntoIterator<Item = usize>) -> Self {
        assert!(requested > 0, "实际模型任务必须要求至少一项译文");
        let mut accepted_flags = vec![false; requested];
        for id in accepted {
            assert!(id < requested, "已接受的临时 ID 必须属于当前任务");
            assert!(!accepted_flags[id], "已接受的临时 ID 不得重复");
            accepted_flags[id] = true;
        }
        let mut accepted = Vec::new();
        let mut unaccepted = Vec::new();
        for (id, is_accepted) in accepted_flags.into_iter().enumerate() {
            if is_accepted {
                accepted.push(id);
            } else {
                unaccepted.push(id);
            }
        }
        Self {
            requested,
            accepted,
            unaccepted,
        }
    }

    pub(crate) const fn requested(&self) -> usize {
        self.requested
    }

    pub(crate) fn accepted(&self) -> &[usize] {
        &self.accepted
    }

    pub(crate) fn unaccepted(&self) -> &[usize] {
        &self.unaccepted
    }
}

pub(crate) fn task_record_output_ids(ids: &[usize]) -> String {
    if ids.is_empty() {
        return "—".to_owned();
    }
    ids.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 任务文档只向 Fluent 传入稳定事实，并移除终端输出使用的方向隔离符。
pub(crate) fn task_record_text(localizer: &UiLocalizer, message: UiMessage<'_>) -> String {
    localizer
        .format(message)
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_fence_is_longer_than_embedded_backticks() {
        let rendered = markdown_fence("before ``` after", "text");

        assert!(rendered.starts_with("````text\n"));
        assert!(rendered.ends_with("````\n"));
    }

    #[test]
    fn json_fence_removes_the_models_canonical_outer_fence() {
        let rendered = markdown_json_fence("```json\n{\"0\":[\"译文\"]}\n```");

        assert_eq!(rendered, "```json\n{\"0\":[\"译文\"]}\n```\n");
    }

    #[test]
    fn output_summary_lists_accepted_and_unaccepted_ids_in_natural_order() {
        let summary = TranslationTaskRecordOutputSummary::new(4, [3, 0, 2]);

        assert_eq!(summary.requested(), 4);
        assert_eq!(summary.accepted(), &[0, 2, 3]);
        assert_eq!(summary.unaccepted(), &[1]);
        assert_eq!(task_record_output_ids(summary.accepted()), "0, 2, 3");
        assert_eq!(task_record_output_ids(&[]), "—");
    }
}
