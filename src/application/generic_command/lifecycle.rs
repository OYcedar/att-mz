//! Generic 命令的取消、panic 边界、终端进度与资源收尾。

use super::diagnostics::{
    GenericCommandError, GenericShutdownError, generic_blocking_join_failure,
    generic_command_error_report, generic_scratch_command_error,
};
use super::project_log::{
    GenericExtractProjectLogStateRef, GenericProjectLogSlot, GenericTerminalOccurrence,
    emit_generic_cancellation_requested, finish_generic_extract_project_log,
    take_generic_project_log,
};
use super::{GENERIC_ENGINE_NAME, GenericCommandOutput};
use crate::application::TranslationTerminalSummary;
use crate::application::config::ConfiguredGenericCommand;
use crate::application::project_log::{ActiveProjectLog, PendingProjectLog};
use crate::application::termination::{
    TerminationOutcome, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, GenericDiagnosticStage, RelatedFailureRelation, RuntimeIssue,
    SafePath, StateEffect,
};
use crate::execution::CooperativeCancellation;
use crate::generic::GenericProjectError;
use crate::generic::write_back::materialization::GenericScratchError;
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::ApiKeyRedactor;
use crate::progress::{TerminalProgress, TerminalProgressFailures, TerminalProgressObserver};
use crate::runtime::filesystem::SystemFileSystem;
use futures_util::FutureExt;
use std::error::Error;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fmt, io};

/// Generic 纵向切片能够确认的实时阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GenericProgressPhase {
    Initializing,
    Extracting,
    PlanningTranslation,
    ConfirmedTasks,
    RunningLua,
    PreparingWriteBack,
    PublishingWriteBack,
}

pub(super) struct GenericTerminalProgress {
    pub(super) terminal: TerminalProgress<GenericProgressPhase>,
    pub(super) safe_stopping: String,
    pub(super) finalizing: String,
}

impl GenericTerminalProgress {
    pub(super) fn observer(&self) -> TerminalProgressObserver<GenericProgressPhase> {
        self.terminal.observer()
    }

    pub(super) fn safe_stopping(&self) {
        defer_generic_terminal_progress_status(
            self.terminal.safe_stopping(self.safe_stopping.clone()),
        );
    }

    pub(super) fn finalizing(&self) {
        defer_generic_terminal_progress_status(self.terminal.finalizing(self.finalizing.clone()));
    }

    pub(super) fn finish(self) -> Result<(), TerminalProgressFailures> {
        self.terminal.finish()
    }
}

fn defer_generic_terminal_progress_status(result: Result<(), TerminalProgressFailures>) {
    if let Err(failures) = result {
        // 健康状态仍由 `TerminalProgress` 持有，最终 `finish` 会再次返回全部失败。
        debug_assert!(!failures.failures().is_empty());
    }
}

pub(super) fn record_generic_terminal_progress_failures(
    result: Result<(), TerminalProgressFailures>,
    shutdown_errors: &mut Vec<GenericShutdownError>,
) {
    if let Err(failures) = result {
        shutdown_errors.extend(
            failures
                .failures()
                .iter()
                .cloned()
                .map(GenericShutdownError::terminal_progress),
        );
    }
}

pub(super) fn generic_terminal_progress(locale: UiLocale) -> GenericTerminalProgress {
    let localizer = UiLocalizer::new(locale);
    let initializing = localizer.format(UiMessage::ProgressGenericInit);
    let extracting = localizer.format(UiMessage::ProgressGenericExtract);
    let planning = localizer.format(UiMessage::ProgressTranslatePlanning);
    let confirmed = localizer.format(UiMessage::ProgressTranslateConfirmed);
    let lua = localizer.format(UiMessage::ProgressProjectLua);
    let preparing_write_back = localizer.format(UiMessage::ProgressWriteBackPlanning);
    let publishing_write_back = localizer.format(UiMessage::ProgressWriteBackPublish);
    let no_progress_work = localizer.format(UiMessage::ProgressNoWork);
    let terminal = TerminalProgress::stderr(
        move |phase| match phase {
            GenericProgressPhase::Initializing => initializing.clone(),
            GenericProgressPhase::Extracting => extracting.clone(),
            GenericProgressPhase::PlanningTranslation => planning.clone(),
            GenericProgressPhase::ConfirmedTasks => confirmed.clone(),
            GenericProgressPhase::RunningLua => lua.clone(),
            GenericProgressPhase::PreparingWriteBack => preparing_write_back.clone(),
            GenericProgressPhase::PublishingWriteBack => publishing_write_back.clone(),
        },
        no_progress_work,
    );
    GenericTerminalProgress {
        terminal,
        safe_stopping: localizer.format(UiMessage::ProgressSafeStopping),
        finalizing: localizer.format(UiMessage::ProgressFinalizing),
    }
}

/// Generic 命令成功完成后的类型化结果。
pub(crate) enum GenericCommandRunResult {
    Succeeded(GenericCommandOutput),
    Interrupted,
    Failed(GenericCommandError),
}

/// Generic 自有业务结果与公共运行根收尾结果。
pub(crate) struct GenericCommandRunReport {
    pub(crate) result: GenericCommandRunResult,
    pub(crate) shutdown_errors: Vec<GenericShutdownError>,
    pub(crate) pending_project_log: Option<PendingProjectLog>,
    pub(crate) panic_log_path: Option<PathBuf>,
    pub(crate) selected_api_key_redactor: Option<Arc<ApiKeyRedactor>>,
    pub(crate) translation_summary: Option<TranslationTerminalSummary>,
}

/// 一个运行根在业务 future 完成后关闭失败。
#[derive(Clone, Debug)]
pub(super) struct GenericCommandPanicContext {
    pub(super) command: crate::diagnostic::RuntimeCommand,
    pub(super) project_workspace: PathBuf,
    pub(super) panic_log_path: Arc<Mutex<Option<PathBuf>>>,
    pub(super) selected_api_key_redactor: Arc<Mutex<Option<Arc<ApiKeyRedactor>>>>,
}

impl GenericCommandPanicContext {
    pub(super) fn new(
        command: crate::diagnostic::RuntimeCommand,
        project_workspace: PathBuf,
    ) -> Self {
        Self {
            command,
            project_workspace,
            panic_log_path: Arc::new(Mutex::new(None)),
            selected_api_key_redactor: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn observe_project_log(&self, project_log: &ActiveProjectLog) {
        let Some(path) = project_log.established_log_path().map(Path::to_path_buf) else {
            return;
        };
        *self
            .panic_log_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }

    pub(super) fn observe_project_log_slot(&self, slot: &GenericProjectLogSlot) {
        if let Some(project_log) = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            self.observe_project_log(project_log);
        }
    }

    pub(super) fn log_path(&self) -> Option<PathBuf> {
        self.panic_log_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn observe_selected_api_key_redactor(&self, redactor: Arc<ApiKeyRedactor>) {
        let mut selected = self
            .selected_api_key_redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = &*selected {
            assert!(
                Arc::ptr_eq(current, &redactor),
                "一次 Generic Translate 运行不能改选另一个 API key 替换器"
            );
        } else {
            *selected = Some(redactor);
        }
    }

    pub(super) fn selected_api_key_redactor(&self) -> Option<Arc<ApiKeyRedactor>> {
        self.selected_api_key_redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Debug)]
pub(super) struct GenericApplicationScopePanicked;

impl fmt::Display for GenericApplicationScopePanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("application scope panicked")
    }
}

impl Error for GenericApplicationScopePanicked {}

pub(super) fn generic_command_panic_context(
    configured: &ConfiguredGenericCommand,
) -> GenericCommandPanicContext {
    let (command, projects_root, project_name, selected_api_key_redactor) = match configured {
        ConfiguredGenericCommand::Init { arguments, common } => (
            crate::diagnostic::RuntimeCommand::Init,
            common.projects_root(),
            arguments.project.name.as_str(),
            None,
        ),
        ConfiguredGenericCommand::Extract {
            project_name,
            common,
        } => (
            crate::diagnostic::RuntimeCommand::Extract,
            common.projects_root(),
            project_name.as_str(),
            None,
        ),
        ConfiguredGenericCommand::Translate(command) => {
            let redactor = command
                .resolved_profile_id()
                .map(|_| command.translation().client().api_key_redactor());
            (
                crate::diagnostic::RuntimeCommand::Translate,
                command.common().projects_root(),
                command.project_name().as_str(),
                redactor,
            )
        }
        ConfiguredGenericCommand::WriteBack(command) => (
            crate::diagnostic::RuntimeCommand::WriteBack,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Manual(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Translation(command) => (
            crate::diagnostic::RuntimeCommand::Manual,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
        ConfiguredGenericCommand::Lua(command) => (
            crate::diagnostic::RuntimeCommand::Lua,
            command.common().projects_root(),
            command.project_name().as_str(),
            None,
        ),
    };
    let context = GenericCommandPanicContext::new(
        command,
        projects_root.join(GENERIC_ENGINE_NAME).join(project_name),
    );
    if let Some(redactor) = selected_api_key_redactor {
        context.observe_selected_api_key_redactor(redactor);
    }
    context
}

fn generic_command_panic_error(context: &GenericCommandPanicContext) -> GenericCommandError {
    let report = DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: crate::diagnostic::RuntimeEngine::Generic,
            command: context.command,
            project_workspace: SafePath::new(&context.project_workspace),
            log_path: context.log_path().map(SafePath::new),
        }),
    );
    GenericCommandError::reported(GenericApplicationScopePanicked, report)
}

pub(super) fn generic_translate_panic_error(
    context: &GenericCommandPanicContext,
) -> GenericCommandError {
    let report = DiagnosticReport::new(
        StateEffect::ProgressPreserved,
        Diagnostic::runtime(RuntimeIssue::CommandPanicked {
            engine: crate::diagnostic::RuntimeEngine::Generic,
            command: context.command,
            project_workspace: SafePath::new(&context.project_workspace),
            log_path: context.log_path().map(SafePath::new),
        }),
    );
    GenericCommandError::reported(GenericApplicationScopePanicked, report)
}

pub(super) async fn catch_generic_command_panic(
    context: GenericCommandPanicContext,
    future: impl Future<Output = GenericCommandRunReport>,
) -> GenericCommandRunReport {
    let mut report = match AssertUnwindSafe(future).catch_unwind().await {
        Ok(report) => report,
        Err(payload) => {
            // panic payload 可能包含模型正文、Lua、SQL 或用户文本；只丢弃，绝不读取。
            drop(payload);
            let panic_log_path = context.log_path();
            GenericCommandRunReport::panicked(generic_command_panic_error(&context), panic_log_path)
        }
    };
    report.selected_api_key_redactor = context.selected_api_key_redactor().or_else(|| {
        report
            .pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::selected_api_key_redactor)
    });
    report
}

#[derive(Clone, Copy, Debug)]
pub(super) struct GenericOperationCancelled;

impl From<GenericOperationCancelled> for GenericCommandError {
    fn from(_: GenericOperationCancelled) -> Self {
        Self::Cancelled
    }
}

pub(super) fn ensure_generic_operation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericOperationCancelled> {
    if cancellation.is_requested() {
        Err(GenericOperationCancelled)
    } else {
        Ok(())
    }
}

pub(super) async fn run_project_blocking<T>(
    diagnostic_stage: GenericDiagnosticStage,
    effect: StateEffect,
    database_path: PathBuf,
    operation: impl FnOnce() -> Result<T, GenericProjectError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| generic_blocking_join_failure(source, effect))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) if source.is_cancelled() => Err(GenericCommandError::Cancelled),
        Err(source) => {
            let report = source.diagnostic_report(diagnostic_stage, &database_path, effect);
            Err(GenericCommandError::reported(source, report))
        }
    }
}

pub(super) async fn run_scratch_blocking<T>(
    operation: impl FnOnce() -> Result<T, GenericScratchError> + Send + 'static,
) -> Result<T, GenericCommandError>
where
    T: Send + 'static,
{
    let result = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| generic_blocking_join_failure(source, StateEffect::Unchanged))?;
    match result {
        Ok(output) => Ok(output),
        Err(source) => Err(generic_scratch_command_error(source)),
    }
}

pub(super) enum Driven<T> {
    Finished(T),
    Interrupted(T),
    CancellationWon(T),
    SignalFailed { source: io::Error, result: T },
}

async fn drive<T>(
    future: impl Future<Output = T>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
) -> Driven<T> {
    match drive_with_termination(future, termination_signals, cancel, || {}).await {
        TerminationOutcome::Finished(result) => Driven::Finished(result),
        TerminationOutcome::Interrupted(result) => Driven::Interrupted(result),
        TerminationOutcome::SignalFailed { source, result } => {
            Driven::SignalFailed { source, result }
        }
    }
}

pub(super) async fn drive_generic_translate_with_panic_boundary(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    panic_context: GenericCommandPanicContext,
) -> Driven<Result<GenericCommandOutput, GenericCommandError>> {
    match AssertUnwindSafe(drive(operation, termination_signals, cancel))
        .catch_unwind()
        .await
    {
        Ok(driven) => driven,
        Err(payload) => {
            // 业务正文可能进入 panic payload；引擎边界只保留安全、类型化诊断。
            drop(payload);
            Driven::Finished(Err(generic_translate_panic_error(&panic_context)))
        }
    }
}

/// WriteBack 的取消只有在目录发布根接管候选前才能生效。
///
/// `cancel` 返回 `false` 表示发布边界已经先发生；此时继续等待业务 future，并按它的真实
/// 终态处理，而不是把已经开始的目录交换伪报为取消。
pub(super) async fn drive_write_back<T>(
    future: impl Future<Output = T>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce() -> bool,
) -> Driven<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = termination_signals.recv() => match signal {
            Ok(()) => {
                let cancellation_started = cancel();
                let result = future.await;
                write_back_signal_result(cancellation_started, result)
            }
            Err(source) => {
                // 信号接收器本身失败时仍发起合作取消；最终分类由 SignalFailed 固定拥有。
                cancel();
                Driven::SignalFailed {
                    source,
                    result: future.await,
                }
            }
        },
        result = &mut future => Driven::Finished(result),
    }
}

pub(super) fn write_back_signal_result<T>(cancellation_started: bool, result: T) -> Driven<T> {
    if cancellation_started {
        Driven::CancellationWon(result)
    } else {
        Driven::Finished(result)
    }
}

pub(super) async fn drive_and_shutdown(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    file_system: SystemFileSystem,
    mut shutdown_errors: Vec<GenericShutdownError>,
    project_log: GenericProjectLogSlot,
    progress: GenericTerminalProgress,
) -> GenericCommandRunReport {
    let cancellation_project_log = Arc::clone(&project_log);
    let driven = drive(operation, termination_signals, || {
        emit_generic_cancellation_requested(&cancellation_project_log);
        progress.safe_stopping();
        cancel();
    })
    .await;
    progress.finalizing();
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::file_system(source));
    }
    record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
    GenericCommandRunReport::from_driven(
        driven,
        shutdown_errors,
        take_generic_project_log(&project_log),
    )
}

// 进程驱动器显式持有信号、日志和终止依赖，避免由无类型上下文重新解释生命周期。
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_extract_and_shutdown(
    operation: impl Future<Output = Result<GenericCommandOutput, GenericCommandError>>,
    termination_signals: &mut TerminationSignals,
    cancel: impl FnOnce(),
    file_system: SystemFileSystem,
    mut shutdown_errors: Vec<GenericShutdownError>,
    project_log: GenericProjectLogSlot,
    extract_project_log: GenericExtractProjectLogStateRef,
    progress: GenericTerminalProgress,
) -> GenericCommandRunReport {
    let cancellation_project_log = Arc::clone(&project_log);
    let driven = drive(operation, termination_signals, || {
        emit_generic_cancellation_requested(&cancellation_project_log);
        progress.safe_stopping();
        cancel();
    })
    .await;
    progress.finalizing();
    if let Err(source) = file_system.shutdown().await {
        shutdown_errors.push(GenericShutdownError::file_system(source));
    }
    record_generic_terminal_progress_failures(progress.finish(), &mut shutdown_errors);
    let terminal_occurrence =
        finish_generic_extract_project_log(&project_log, &extract_project_log, &driven);
    GenericCommandRunReport::from_driven_with_terminal_occurrence(
        driven,
        shutdown_errors,
        take_generic_project_log(&project_log),
        terminal_occurrence,
    )
}

impl GenericCommandRunReport {
    pub(super) fn failed(error: GenericCommandError) -> Self {
        Self {
            result: GenericCommandRunResult::Failed(error),
            shutdown_errors: Vec::new(),
            pending_project_log: None,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    pub(super) fn panicked(error: GenericCommandError, panic_log_path: Option<PathBuf>) -> Self {
        Self {
            result: GenericCommandRunResult::Failed(error),
            shutdown_errors: Vec::new(),
            pending_project_log: None,
            panic_log_path,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    pub(super) fn from_driven(
        driven: Driven<Result<GenericCommandOutput, GenericCommandError>>,
        shutdown_errors: Vec<GenericShutdownError>,
        project_log: Option<ActiveProjectLog>,
    ) -> Self {
        Self::from_driven_with_terminal_occurrence(driven, shutdown_errors, project_log, None)
    }

    pub(super) fn from_driven_with_terminal_occurrence(
        driven: Driven<Result<GenericCommandOutput, GenericCommandError>>,
        shutdown_errors: Vec<GenericShutdownError>,
        project_log: Option<ActiveProjectLog>,
        terminal_occurrence: Option<GenericTerminalOccurrence>,
    ) -> Self {
        let result = match driven {
            // 信号到达后业务仍返回完整成功，说明事务或其他终态已经生效。
            Driven::Finished(Ok(output)) | Driven::Interrupted(Ok(output)) => {
                GenericCommandRunResult::Succeeded(output)
            }
            Driven::Finished(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            Driven::Interrupted(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            // WriteBack 的发布门确认取消先于发布取得终态；即使内部 future
            // 随后错误地返回成功，也不能把未发布候选报告为成功。
            Driven::CancellationWon(Ok(_)) => GenericCommandRunResult::Interrupted,
            Driven::CancellationWon(Err(error)) => {
                if error.is_cancelled() {
                    GenericCommandRunResult::Interrupted
                } else {
                    GenericCommandRunResult::Failed(error)
                }
            }
            Driven::SignalFailed { source, result } => match result {
                Ok(_) => GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source,
                    operation: None,
                    state_applied: true,
                }),
                Err(error) => GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source,
                    operation: Some(Box::new(error)),
                    state_applied: false,
                }),
            },
        };
        let pending_project_log = project_log.map(|project_log| {
            if shutdown_errors.is_empty() {
                return match &result {
                    GenericCommandRunResult::Succeeded(_) => project_log.pending_succeeded(),
                    GenericCommandRunResult::Interrupted => project_log.pending_cancelled(),
                    GenericCommandRunResult::Failed(error) => match terminal_occurrence {
                        Some(occurrence) => occurrence.into_pending(project_log),
                        None => project_log.pending_failure(generic_command_error_report(error)),
                    },
                };
            }

            let mut reports = shutdown_errors
                .iter()
                .map(GenericShutdownError::diagnostic_report);
            let first_shutdown = reports
                .next()
                .expect("非空 shutdown_errors 必须产生至少一份报告");
            let mut report = match &result {
                GenericCommandRunResult::Failed(error) => generic_command_error_report(error)
                    .with_related(RelatedFailureRelation::Shutdown, first_shutdown),
                GenericCommandRunResult::Succeeded(_) | GenericCommandRunResult::Interrupted => {
                    first_shutdown
                }
            };
            for related in reports {
                report = report.with_related(RelatedFailureRelation::Shutdown, related);
            }
            project_log.pending_failure(report)
        });
        Self {
            result,
            shutdown_errors,
            pending_project_log,
            panic_log_path: None,
            selected_api_key_redactor: None,
            translation_summary: None,
        }
    }

    pub(super) fn with_translation_summary(
        mut self,
        summary: Option<TranslationTerminalSummary>,
    ) -> Self {
        self.translation_summary = summary;
        self
    }
}

// 发布编排必须独立持有候选、门闩和日志终态，保持副作用顺序可审计。
