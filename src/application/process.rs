//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::cell::Cell;
use std::ffi::OsString;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Once};

use windows_sys::Win32::Globalization::{CP_UTF8, GetACP};

use super::TranslationTerminalSummary;
use super::arguments::{AttArguments, ProductCommand};
use super::command::{
    CommandPanicBoundary, CommandResultRenderer, CommandRunResult, ProductionCommandError,
    ProductionCommandRunReport, ProductionRpgMakerCommandRunner,
};
use super::config::{
    ConfigurationLoadError, ConfiguredProductCommand, DistributionLayout, DistributionLayoutError,
    load_product_configuration,
};
use super::generic_command::{
    GenericCommandOutput, GenericCommandRunReport, GenericCommandRunResult, GenericShutdownError,
    ProductionGenericCommandRunner, generic_command_error_report,
};
use super::project_log::{PendingProjectLog, ProjectLogWarning};
use super::termination::TerminationSignals;
use super::test_command::{TestCommandReport, run_test_command};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation,
    RuntimePanicBoundary, StateEffect, public_path, render_diagnostic_report,
    render_state_effect_impact,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ApiKeyRedactor, ApiKeyRedactorSet};
use crate::manual::{render_manual_command_error, render_manual_command_summary};
use crate::runtime::project_log::TranslationEngineSummary;

enum ProductCommandRunReport {
    Test(TestCommandReport),
    RpgMaker(ProductionCommandRunReport),
    Generic(GenericCommandRunReport),
}

enum ProcessOutputState {
    NeedsFlush(ExitCode),
    Flushed(ExitCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessStream {
    Stdout,
    Stderr,
}

impl ProcessStream {
    const fn write_operation(self) -> RuntimeOperation {
        match self {
            Self::Stdout => RuntimeOperation::WriteStdout,
            Self::Stderr => RuntimeOperation::WriteStderr,
        }
    }

    const fn flush_operation(self) -> RuntimeOperation {
        match self {
            Self::Stdout => RuntimeOperation::FlushStdout,
            Self::Stderr => RuntimeOperation::FlushStderr,
        }
    }
}

/// 保存一个进程输出流已经确认的呈现状态。
///
/// `unconfirmed` 保存自上次成功 flush 以来的全部逻辑正文。底层 write 成功只表示
/// 字节进入了流的缓冲区；只有 flush 成功才能确认用户已经收到并清空这些字节。
/// 首次 write 或 flush 失败后，后续逻辑输出只进入 `unconfirmed`，不再调用已经失败的
/// 底层流。最终收尾可据此把完整正文与四字段诊断一次写到另一条仍可用的流。
struct StreamPresentation<'a> {
    stream: ProcessStream,
    output: &'a mut dyn Write,
    unconfirmed: Vec<u8>,
    api_key_redactors: ApiKeyRedactorSet,
    write_failure: Option<IoFailure>,
    flush_failure: Option<IoFailure>,
    write_in_progress: bool,
    flush_in_progress: bool,
    flush_attempted: bool,
}

impl<'a> StreamPresentation<'a> {
    fn new(stream: ProcessStream, output: &'a mut dyn Write) -> Self {
        Self {
            stream,
            output,
            unconfirmed: Vec::new(),
            api_key_redactors: ApiKeyRedactorSet::default(),
            write_failure: None,
            flush_failure: None,
            write_in_progress: false,
            flush_in_progress: false,
            flush_attempted: false,
        }
    }

    fn is_unavailable(&self) -> bool {
        self.write_failure.is_some()
            || self.flush_failure.is_some()
            || self.write_in_progress
            || self.flush_in_progress
    }

    fn failure_reports(&self, effect: StateEffect) -> Vec<DiagnosticReport> {
        let mut reports = Vec::new();
        if let Some(failure) = self.write_failure.clone() {
            reports.push(process_output_failure_report_from_failure(
                effect,
                self.stream.write_operation(),
                failure,
            ));
        }
        if let Some(failure) = self.flush_failure.clone() {
            reports.push(process_output_failure_report_from_failure(
                effect,
                self.stream.flush_operation(),
                failure,
            ));
        }
        reports
    }

    fn unconfirmed(&self) -> &[u8] {
        &self.unconfirmed
    }

    fn has_unconfirmed(&self) -> bool {
        !self.unconfirmed.is_empty()
    }

    fn prepend_unconfirmed(&mut self, bytes: &[u8]) {
        debug_assert!(!self.is_unavailable());
        if bytes.is_empty() {
            return;
        }
        let mut combined = Vec::with_capacity(bytes.len() + self.unconfirmed.len());
        combined.extend_from_slice(bytes);
        combined.extend_from_slice(&self.unconfirmed);
        self.unconfirmed = combined;
        // 成功 flush 过的健康流可以接收一批新的回退正文。
        self.flush_attempted = false;
    }

    fn select_api_key_redactor(&mut self, redactor: Option<Arc<ApiKeyRedactor>>) {
        if let Some(redactor) = redactor {
            self.api_key_redactors.insert(redactor);
        }
    }

    fn select_api_key_redactors(&mut self, redactors: &[Arc<ApiKeyRedactor>]) {
        self.api_key_redactors.extend(redactors);
    }

    fn bytes_for_output(&self, bytes: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes);
        self.api_key_redactors.redact(&text).into_bytes()
    }

    /// 日常呈现和首次 flush 已结束后，只允许调用方执行一次有界后续写入。
    fn write_follow_up(
        &mut self,
        bytes: &[u8],
        effect: StateEffect,
    ) -> Result<(), DiagnosticReport> {
        debug_assert!(!self.is_unavailable());
        self.write_all(bytes)
            .expect("StreamPresentation 的逻辑 write 必须由自身吸收底层失败");
        let _ = self.flush();
        self.failure_reports(effect)
            .into_iter()
            .next()
            .map_or(Ok(()), Err)
    }
}

impl Write for StreamPresentation<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.unconfirmed.extend_from_slice(buffer);
        // 成功 flush 后出现了新的逻辑正文，下一次 flush 是一个新的确认动作。
        if !self.is_unavailable() {
            self.flush_attempted = false;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_attempted {
            return Ok(());
        }
        self.flush_attempted = true;
        // 失败后的流不得再次碰触底层输出；完整未确认正文交给相反流。
        if self.is_unavailable() {
            return Ok(());
        }
        let bytes = self.bytes_for_output(&self.unconfirmed);
        self.write_in_progress = true;
        if let Err(source) = self.output.write_all(&bytes) {
            self.write_in_progress = false;
            self.write_failure = Some(IoFailure::from_error(&source));
            return Ok(());
        }
        self.write_in_progress = false;
        self.flush_in_progress = true;
        match self.output.flush() {
            Ok(()) => {
                self.flush_in_progress = false;
                self.unconfirmed.clear();
                Ok(())
            }
            Err(source) => {
                self.flush_in_progress = false;
                self.flush_failure = Some(IoFailure::from_error(&source));
                // 失败事实由调用方从状态读取；这里不让第一个流阻止第二个流 flush。
                Ok(())
            }
        }
    }
}

#[derive(Default)]
struct RunLogPresentation {
    path: Option<PathBuf>,
    written: Cell<bool>,
}

impl RunLogPresentation {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            written: Cell::new(false),
        }
    }

    fn write(&self, localizer: &UiLocalizer, output: &mut dyn Write) -> io::Result<()> {
        if self.written.get() {
            return Ok(());
        }
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let path = public_path(path);
        writeln!(
            output,
            "{}",
            localizer.format(UiMessage::ResultRunLog { path: &path })
        )?;
        self.written.set(true);
        Ok(())
    }
}

/// 运行真实进程入口。
pub(crate) fn run() -> ExitCode {
    install_safe_panic_hook();
    match catch_unwind(AssertUnwindSafe(run_guarded)) {
        Ok(exit_code) => exit_code,
        Err(_) => render_uncaught_panic(),
    }
}

fn install_safe_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // panic payload 可能包含模型、Lua、SQL 或用户正文。进程边界只输出下方的
        // 固定结构化诊断，因此全局 hook 不读取也不打印 PanicHookInfo。
        std::panic::set_hook(Box::new(|_| {}));
    });
}

fn run_guarded() -> ExitCode {
    // 实时进度由独立线程短暂取得 stderr 锁；进程主线程不能在整个命令期间持有锁。
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    if let Some(diagnostic) = windows_utf8_process_diagnostic() {
        // 进程运行时不满足最低要求时，完整 CLI 尚未解析，固定使用英语呈现；
        // run_from 不执行此检查，避免未嵌入 manifest 的 Rust 测试宿主受进程 ACP 影响。
        let localizer = UiLocalizer::new(UiLocale::English);
        let exit = render_diagnostic_report_fatal(&localizer, &diagnostic, &mut stderr);
        return finalize_raw_process_output(
            exit,
            StateEffect::Unchanged,
            &localizer,
            &mut stdout,
            &mut stderr,
        );
    }
    run_from(std::env::args_os(), &mut stdout, &mut stderr)
}

fn windows_utf8_process_diagnostic() -> Option<DiagnosticReport> {
    // SAFETY: GetACP 没有参数，只读取当前进程的 Windows ANSI code page。
    let actual_code_page = unsafe { GetACP() };
    windows_utf8_process_diagnostic_for(actual_code_page)
}

fn windows_utf8_process_diagnostic_for(actual_code_page: u32) -> Option<DiagnosticReport> {
    (actual_code_page != CP_UTF8).then(|| {
        DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::UnsupportedWindowsCodePage {
                expected: CP_UTF8,
                actual: actual_code_page,
            }),
        )
    })
}

fn render_uncaught_panic() -> ExitCode {
    // UI locale 尚未由完整 Clap 解析确认时，现行 CLI 契约固定使用英语兜底。
    let localizer = UiLocalizer::new(UiLocale::English);
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    render_uncaught_panic_with(
        &localizer,
        RuntimePanicBoundary::ProcessStartup,
        &mut stdout,
        &mut stderr,
    )
}

fn render_uncaught_panic_with(
    localizer: &UiLocalizer,
    boundary: RuntimePanicBoundary,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let mut stdout = StreamPresentation::new(ProcessStream::Stdout, stdout);
    let mut stderr = StreamPresentation::new(ProcessStream::Stderr, stderr);
    let diagnostic = DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::ProcessPanicked { boundary }),
    );
    let exit = render_diagnostic_report_fatal(localizer, &diagnostic, &mut stderr);
    finalize_process_output(
        exit,
        StateEffect::OutcomeUnknown,
        localizer,
        &mut stdout,
        &mut stderr,
    )
}

fn run_from<A, S>(args: A, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode
where
    A: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let mut stdout = StreamPresentation::new(ProcessStream::Stdout, stdout);
    let mut stderr = StreamPresentation::new(ProcessStream::Stderr, stderr);
    let (arguments, resolved_locale) = match AttArguments::try_parse_localized_from(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let localizer = UiLocalizer::new(error.locale());
            return catch_after_cli_parsing(
                &localizer,
                &mut stdout,
                &mut stderr,
                |stdout, stderr| {
                    let rendered = if error.use_stderr() {
                        write!(stderr, "{}", error.output())
                    } else {
                        write!(stdout, "{}", error.output())
                    };
                    let exit = if rendered.is_err() {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::from(error.exit_code())
                    };
                    finalize_process_output(
                        exit,
                        StateEffect::Unchanged,
                        &localizer,
                        stdout,
                        stderr,
                    )
                },
            );
        }
    };
    let locale = resolved_locale.locale();
    let localizer = UiLocalizer::new(locale);
    catch_after_cli_parsing(&localizer, &mut stdout, &mut stderr, |stdout, stderr| {
        let state = run_after_cli_parsing(arguments, locale, &localizer, stdout, stderr);
        finish_process_output_state(state, &localizer, stdout, stderr)
    })
}

fn finish_process_output_state(
    state: ProcessOutputState,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ExitCode {
    match state {
        ProcessOutputState::NeedsFlush(exit) => {
            // 尚未进入项目日志结果边界的输出在这里完成唯一一次 flush。
            finalize_process_output(exit, StateEffect::Unchanged, localizer, stdout, stderr)
        }
        // 项目命令已经在日志关闭前完成唯一一次可诊断 flush，不能在日志关闭后重试。
        ProcessOutputState::Flushed(exit) => exit,
    }
}

fn finalize_process_output(
    exit: ExitCode,
    effect: StateEffect,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ExitCode {
    // 有正文的业务流先确认。它失败时，把完整正文和诊断放到尚未 flush 的相反流正文
    // 之前；这样原业务结果仍先于相关错误，并且健康相反流只写一批。
    let stdout_first = stdout.has_unconfirmed() || !stderr.has_unconfirmed();
    if stdout_first {
        let _ = stdout.flush();
        if stdout.is_unavailable() && !stderr.is_unavailable() {
            let payload = stream_failure_payload(stdout, effect, localizer);
            stderr.prepend_unconfirmed(&payload);
        }
        let _ = stderr.flush();
    } else {
        let _ = stderr.flush();
        if stderr.is_unavailable() && !stdout.is_unavailable() {
            let payload = stream_failure_payload(stderr, effect, localizer);
            stdout.prepend_unconfirmed(&payload);
        }
        let _ = stdout.flush();
    }
    if !stdout.is_unavailable() && !stderr.is_unavailable() {
        return exit;
    }

    // 第二个流失败时，第一个流已经成功确认，只能做一次有界回退；第一个流失败的
    // 内容已在第二个流首次 flush 前合并，不得在这里重复呈现。
    match (
        stdout_first,
        stdout.is_unavailable(),
        stderr.is_unavailable(),
    ) {
        (true, false, true) => {
            let payload = stream_failure_payload(stderr, effect, localizer);
            let _ = stdout.write_follow_up(&payload, effect);
        }
        (false, true, false) => {
            let payload = stream_failure_payload(stdout, effect, localizer);
            let _ = stderr.write_follow_up(&payload, effect);
        }
        _ => {}
    }
    ExitCode::FAILURE
}

fn finalize_raw_process_output(
    exit: ExitCode,
    effect: StateEffect,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let mut stdout = StreamPresentation::new(ProcessStream::Stdout, stdout);
    let mut stderr = StreamPresentation::new(ProcessStream::Stderr, stderr);
    finalize_process_output(exit, effect, localizer, &mut stdout, &mut stderr)
}

fn stream_failure_payload(
    stream: &StreamPresentation<'_>,
    effect: StateEffect,
    localizer: &UiLocalizer,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(stream.unconfirmed());
    for report in stream.failure_reports(effect) {
        render_primary_error(&report, localizer, &mut payload).expect("内存中的进程诊断必须可呈现");
    }
    payload
}

fn catch_after_cli_parsing(
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
    operation: impl FnOnce(&mut StreamPresentation<'_>, &mut StreamPresentation<'_>) -> ExitCode,
) -> ExitCode {
    match catch_unwind(AssertUnwindSafe(|| operation(stdout, stderr))) {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            // 与命令 panic 边界一致，payload 只触发控制流，绝不读取或格式化。
            drop(payload);
            let diagnostic = DiagnosticReport::new(
                StateEffect::OutcomeUnknown,
                Diagnostic::runtime(RuntimeIssue::ProcessPanicked {
                    boundary: RuntimePanicBoundary::AfterCliParsing,
                }),
            );
            let _ = render_primary_error(&diagnostic, localizer, stderr);
            finalize_process_output(
                ExitCode::FAILURE,
                StateEffect::OutcomeUnknown,
                localizer,
                stdout,
                stderr,
            )
        }
    }
}

fn run_after_cli_parsing(
    arguments: AttArguments,
    locale: UiLocale,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ProcessOutputState {
    let is_test_command = matches!(&arguments.product, ProductCommand::Test);
    let distribution = match DistributionLayout::from_current_executable() {
        Ok(distribution) => distribution,
        Err(error) => {
            return ProcessOutputState::NeedsFlush(render_distribution_layout_error(
                localizer, &error, stderr,
            ));
        }
    };
    let configuration = match load_product_configuration(&distribution, arguments.product) {
        Ok(configuration) => configuration,
        Err(error) => {
            if is_test_command
                && writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTestConfiguration { status: "failed" })
                )
                .is_err()
            {
                return ProcessOutputState::NeedsFlush(ExitCode::FAILURE);
            }
            return ProcessOutputState::NeedsFlush(render_configuration_load_error(
                localizer, &error, stderr,
            ));
        }
    };
    let runtime_parallelism = match std::thread::available_parallelism() {
        Ok(parallelism) => parallelism,
        Err(error) => {
            let diagnostic = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TokioRuntime,
                    operation: RuntimeOperation::DetectAvailableParallelism,
                    failure: IoFailure::from_error(&error),
                }),
            );
            return ProcessOutputState::NeedsFlush(render_diagnostic_report_fatal(
                localizer,
                &diagnostic,
                stderr,
            ));
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(runtime_parallelism.get())
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let diagnostic = DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TokioRuntime,
                    operation: RuntimeOperation::BuildAsyncRuntime,
                    failure: IoFailure::from_error(&error),
                }),
            );
            return ProcessOutputState::NeedsFlush(render_diagnostic_report_fatal(
                localizer,
                &diagnostic,
                stderr,
            ));
        }
    };

    // Translate 的生产纵向切片包含完整计划、错误与收尾状态，其 async 状态机明显大于
    // Windows 主线程的默认栈。先把顶层 future 固定在堆上，避免 block_on 将整棵
    // 状态机钉在主线程栈中；这不改变 Tokio 内部任务的调度和并发关系。
    let command_run = Box::pin(async move {
        let mut termination_signals = TerminationSignals::new();
        let report = match configuration {
            ConfiguredProductCommand::Test(command) => ProductCommandRunReport::Test(
                run_test_command(command, &mut termination_signals).await,
            ),
            ConfiguredProductCommand::RpgMaker { layout, command } => {
                ProductCommandRunReport::RpgMaker(
                    ProductionRpgMakerCommandRunner::new(layout, locale)
                        .run(command, &mut termination_signals)
                        .await,
                )
            }
            ConfiguredProductCommand::Generic(command) => ProductCommandRunReport::Generic(
                ProductionGenericCommandRunner::new(locale)
                    .run(command, &mut termination_signals)
                    .await,
            ),
        };
        (report, termination_signals)
    });
    let (report, _termination_signals) = runtime.block_on(command_run);
    // 信号订阅与 Runtime 保持到最终结果输出结束；各业务根已经在 report 返回前显式 shutdown。

    let report = match report {
        ProductCommandRunReport::Test(report) => {
            stdout.select_api_key_redactors(&report.redactors);
            stderr.select_api_key_redactors(&report.redactors);
            return ProcessOutputState::NeedsFlush(render_test_command_report(
                report, localizer, stdout, stderr,
            ));
        }
        ProductCommandRunReport::RpgMaker(report) => report,
        ProductCommandRunReport::Generic(report) => {
            return ProcessOutputState::Flushed(render_generic_command_report(
                report, localizer, stdout, stderr,
            ));
        }
    };
    let selected_api_key_redactor = report.selected_api_key_redactor;
    stdout.select_api_key_redactor(selected_api_key_redactor.clone());
    stderr.select_api_key_redactor(selected_api_key_redactor);
    let panic_log_path = report.panic_log_path;
    let mut pending_project_log = report.pending_project_log;
    if let Some(project_log) = pending_project_log.as_mut() {
        project_log.prepare_for_result_presentation();
    }
    let had_presentation_failure = pending_project_log
        .as_ref()
        .is_some_and(PendingProjectLog::has_presentation_failure);
    let panic_boundary = pending_project_log
        .as_mut()
        .map(PendingProjectLog::arm_presentation_panic)
        .map(CommandPanicBoundary::from_report);
    let run_log = RunLogPresentation::new(
        pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::log_path)
            .map(PathBuf::from)
            .or(panic_log_path),
    );
    ProcessOutputState::Flushed(catch_logged_presentation(
        panic_boundary,
        &run_log,
        localizer,
        stdout,
        stderr,
        |stdout, stderr| {
            render_command_report_with_run_log(
                report.result,
                report.shutdown_error,
                report.translation_summary,
                pending_project_log,
                LoggedPresentationContext {
                    run_log: &run_log,
                    had_presentation_failure,
                    localizer,
                },
                stdout,
                stderr,
            )
        },
    ))
}

#[cfg(test)]
fn render_command_report(
    result: CommandRunResult,
    shutdown_error: Option<super::command::ShutdownFailures>,
    pending_project_log: Option<PendingProjectLog>,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let mut stdout = StreamPresentation::new(ProcessStream::Stdout, stdout);
    let mut stderr = StreamPresentation::new(ProcessStream::Stderr, stderr);
    let run_log = RunLogPresentation::new(
        pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::log_path)
            .map(PathBuf::from),
    );
    let had_presentation_failure = pending_project_log
        .as_ref()
        .is_some_and(PendingProjectLog::has_presentation_failure);
    render_command_report_with_run_log(
        result,
        shutdown_error,
        None,
        pending_project_log,
        LoggedPresentationContext {
            run_log: &run_log,
            had_presentation_failure,
            localizer,
        },
        &mut stdout,
        &mut stderr,
    )
}

#[derive(Clone, Copy)]
struct LoggedPresentationContext<'a> {
    run_log: &'a RunLogPresentation,
    had_presentation_failure: bool,
    localizer: &'a UiLocalizer,
}

fn render_command_report_with_run_log(
    result: CommandRunResult,
    shutdown_error: Option<super::command::ShutdownFailures>,
    translation_summary: Option<TranslationTerminalSummary>,
    pending_project_log: Option<PendingProjectLog>,
    context: LoggedPresentationContext<'_>,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ExitCode {
    let LoggedPresentationContext {
        run_log,
        had_presentation_failure,
        localizer,
    } = context;
    match (result, shutdown_error) {
        (CommandRunResult::Succeeded(output), shutdown) => {
            if let Err(error) = CommandResultRenderer::render_success(&output, localizer, stdout) {
                let command_error = ProductionCommandError::stdout_write(error);
                let mut presentation_failures =
                    vec![command_error.failure_report().report().clone()];
                if let Err(error) = CommandResultRenderer::render_failure(
                    Some(&command_error),
                    shutdown.as_ref(),
                    localizer,
                    stderr,
                ) {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                if presentation_failures.len() == 1 {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stderr,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &mut presentation_failures,
                    );
                }
                let _ = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                ExitCode::FAILURE
            } else if let Err(error) =
                CommandResultRenderer::render_success_warnings(&output, localizer, stderr)
            {
                let command_error = ProductionCommandError::stderr_write(error);
                let mut presentation_failures =
                    vec![command_error.failure_report().report().clone()];
                if let Err(error) = CommandResultRenderer::render_failure(
                    Some(&command_error),
                    shutdown.as_ref(),
                    localizer,
                    stderr,
                ) {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                if presentation_failures.len() == 1 {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stderr,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &mut presentation_failures,
                    );
                }
                let _ = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                ExitCode::FAILURE
            } else {
                // shutdown 已经失败时，先确认 stdout 的成功摘要。若 stdout 本身也无法
                // 呈现，它必须成为主错误，shutdown 才能以真实的 related 关系随后呈现。
                if let Some(shutdown) = shutdown.as_ref() {
                    let _ = stdout.flush();
                    if stdout.is_unavailable() {
                        let mut presentation_failures =
                            stdout.failure_reports(StateEffect::AppliedFinalizationFailed);
                        if let Err(error) = CommandResultRenderer::render_related_shutdown_failures(
                            shutdown, localizer, stderr,
                        ) {
                            presentation_failures.push(process_output_failure_report(
                                StateEffect::AppliedFinalizationFailed,
                                RuntimeOperation::WriteStderr,
                                &error,
                            ));
                        }
                        record_run_log_path(
                            run_log,
                            localizer,
                            stderr,
                            StateEffect::AppliedFinalizationFailed,
                            RuntimeOperation::WriteStderr,
                            &mut presentation_failures,
                        );
                        let _ = finish_project_log_after_presentation(
                            pending_project_log,
                            presentation_failures,
                            StateEffect::AppliedFinalizationFailed,
                            localizer,
                            stdout,
                            stderr,
                        );
                        return ExitCode::FAILURE;
                    }
                }
                let mut presentation_failures = Vec::new();
                if let Some(shutdown) = shutdown.as_ref()
                    && let Err(error) = CommandResultRenderer::render_applied_finalization_failure(
                        shutdown, localizer, stderr,
                    )
                {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                if presentation_failures.is_empty() {
                    if shutdown.is_some() || had_presentation_failure {
                        record_run_log_path(
                            run_log,
                            localizer,
                            stderr,
                            StateEffect::AppliedFinalizationFailed,
                            RuntimeOperation::WriteStderr,
                            &mut presentation_failures,
                        );
                    } else {
                        record_run_log_path(
                            run_log,
                            localizer,
                            stdout,
                            StateEffect::AppliedFinalizationFailed,
                            RuntimeOperation::WriteStdout,
                            &mut presentation_failures,
                        );
                    }
                }
                let had_presentation_failure = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                if had_presentation_failure {
                    return ExitCode::FAILURE;
                }
                if shutdown.is_some() {
                    // 业务结果已生效；清理错误不得覆盖业务成功事实。
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
        }
        (CommandRunResult::Failed(command_error), shutdown) => {
            let presentation_effect = command_error.failure_report().report().effect();
            let mut presentation_failures = Vec::new();
            if let Err(error) = CommandResultRenderer::render_failure(
                Some(&command_error),
                shutdown.as_ref(),
                localizer,
                stderr,
            ) {
                presentation_failures.push(process_output_failure_report(
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if let Some(summary) = translation_summary
                && let Err(error) = render_translation_terminal_summary(summary, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if presentation_failures.is_empty() {
                record_run_log_path(
                    run_log,
                    localizer,
                    stderr,
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &mut presentation_failures,
                );
            }
            let _ = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                presentation_effect,
                localizer,
                stdout,
                stderr,
            );
            ExitCode::FAILURE
        }
        (CommandRunResult::Interrupted, None) => {
            let mut presentation_failures = Vec::new();
            if let Err(error) = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if let Some(summary) = translation_summary
                && let Err(error) = render_translation_terminal_summary(summary, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if presentation_failures.is_empty() {
                record_run_log_path(
                    run_log,
                    localizer,
                    stderr,
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &mut presentation_failures,
                );
            }
            let cancellation_failed = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                StateEffect::ProgressPreserved,
                localizer,
                stdout,
                stderr,
            );
            if cancellation_failed {
                ExitCode::FAILURE
            } else {
                ExitCode::from(130)
            }
        }
        (CommandRunResult::Interrupted, Some(shutdown)) => {
            let mut presentation_failures = Vec::new();
            if let Err(error) = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if let Some(summary) = translation_summary
                && let Err(error) = render_translation_terminal_summary(summary, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if let Err(error) =
                CommandResultRenderer::render_failure(None, Some(&shutdown), localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if presentation_failures.is_empty() {
                record_run_log_path(
                    run_log,
                    localizer,
                    stderr,
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &mut presentation_failures,
                );
            }
            let presentation_failed = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                StateEffect::ProgressPreserved,
                localizer,
                stdout,
                stderr,
            );
            // 取消事实与清理失败并列呈现，清理错误不吞掉“已取消”这一终态。
            if presentation_failed {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

fn record_run_log_path(
    run_log: &RunLogPresentation,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
    effect: StateEffect,
    operation: RuntimeOperation,
    presentation_failures: &mut Vec<DiagnosticReport>,
) {
    if let Err(source) = run_log.write(localizer, output) {
        presentation_failures.push(process_output_failure_report(effect, operation, &source));
    }
}

fn finish_project_log_after_presentation(
    pending_project_log: Option<PendingProjectLog>,
    mut presentation_failures: Vec<DiagnosticReport>,
    effect: StateEffect,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> bool {
    let stdout_first = stdout.has_unconfirmed() || !stderr.has_unconfirmed();
    let mut stdout_relayed_before_stderr_flush = false;
    let mut stderr_relayed_before_stdout_flush = false;
    if stdout_first {
        let _ = stdout.flush();
        if stdout.is_unavailable() && !stderr.is_unavailable() {
            let payload = stream_failure_payload(stdout, effect, localizer);
            stderr.prepend_unconfirmed(&payload);
            stdout_relayed_before_stderr_flush = true;
        }
        let _ = stderr.flush();
    } else {
        let _ = stderr.flush();
        if stderr.is_unavailable() && !stdout.is_unavailable() {
            let payload = stream_failure_payload(stderr, effect, localizer);
            stdout.prepend_unconfirmed(&payload);
            stderr_relayed_before_stdout_flush = true;
        }
        let _ = stdout.flush();
    }

    let stdout_reports = stdout.failure_reports(effect);
    let stderr_reports = stderr.failure_reports(effect);
    for report in stdout_reports.iter().chain(&stderr_reports) {
        if !presentation_failures.contains(report) {
            presentation_failures.push(report.clone());
        }
    }
    let additional_reports = presentation_failures
        .iter()
        .filter(|report| !stdout_reports.contains(report) && !stderr_reports.contains(report))
        .cloned()
        .collect::<Vec<_>>();
    let mut presentation_failed = !presentation_failures.is_empty();
    let warning = pending_project_log.and_then(|project_log| {
        if presentation_failures.is_empty() {
            project_log.finish()
        } else {
            project_log.finish_with_diagnostics(presentation_failures)
        }
    });
    presentation_failed |= warning
        .as_ref()
        .is_some_and(|warning| !warning.presentation_failures.is_empty());

    let mut warning_payload = Vec::new();
    if let Some(warning) = warning.as_ref() {
        render_project_log_warning(localizer, warning, &mut warning_payload)
            .expect("内存中的项目日志警告必须可呈现");
    }

    let mut additional_payload = Vec::new();
    for report in &additional_reports {
        render_primary_error(report, localizer, &mut additional_payload)
            .expect("内存中的进程诊断必须可呈现");
    }

    match (stdout.is_unavailable(), stderr.is_unavailable()) {
        (true, false) => {
            let mut follow_up = Vec::new();
            if !stdout_relayed_before_stderr_flush {
                follow_up.extend_from_slice(&stream_failure_payload(stdout, effect, localizer));
            }
            follow_up.extend_from_slice(&additional_payload);
            follow_up.extend_from_slice(&warning_payload);
            if !follow_up.is_empty() && stderr.write_follow_up(&follow_up, effect).is_err() {
                presentation_failed = true;
            }
        }
        (false, true) => {
            let mut follow_up = Vec::new();
            if !stderr_relayed_before_stdout_flush {
                follow_up.extend_from_slice(&stream_failure_payload(stderr, effect, localizer));
            }
            follow_up.extend_from_slice(&additional_payload);
            follow_up.extend_from_slice(&warning_payload);
            if !follow_up.is_empty() && stdout.write_follow_up(&follow_up, effect).is_err() {
                presentation_failed = true;
            }
        }
        (false, false) => {
            let mut follow_up = additional_payload;
            follow_up.extend_from_slice(&warning_payload);
            if !follow_up.is_empty()
                && let Err(fallback_failure) = stderr.write_follow_up(&follow_up, effect)
            {
                presentation_failed = true;
                let mut final_fallback = follow_up;
                render_primary_error(&fallback_failure, localizer, &mut final_fallback)
                    .expect("内存中的进程诊断必须可呈现");
                if !stdout.is_unavailable() {
                    let _ = stdout.write_follow_up(&final_fallback, effect);
                }
            }
        }
        (true, true) => {}
    }

    presentation_failed
}

fn process_output_failure_report(
    effect: StateEffect,
    operation: RuntimeOperation,
    source: &io::Error,
) -> DiagnosticReport {
    process_output_failure_report_from_failure(effect, operation, IoFailure::from_error(source))
}

fn process_output_failure_report_from_failure(
    effect: StateEffect,
    operation: RuntimeOperation,
    failure: IoFailure,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::runtime(RuntimeIssue::Io {
            component: RuntimeComponent::Process,
            operation,
            failure,
        }),
    )
}

fn render_translation_terminal_summary(
    summary: TranslationTerminalSummary,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
) -> io::Result<()> {
    let tasks = summary.tasks;
    match summary.engine {
        TranslationEngineSummary::RpgMaker(engine) => {
            writeln!(
                output,
                "{}",
                localizer.format(UiMessage::ResultTranslateSummary {
                    total: tasks.planned,
                    started: tasks.started,
                    not_started: tasks.not_started,
                    complete: tasks.complete,
                    partial: tasks.partial,
                    unavailable: tasks.unavailable,
                    failed: tasks.failed,
                    cancelled: tasks.cancelled,
                    written: engine.written_locations,
                    remaining: engine.remaining_locations,
                    rejected: engine.rejected_locations,
                })
            )?;
            writeln!(
                output,
                "{}",
                localizer.format(UiMessage::TranslateIncompleteRpgMakerReason {
                    partial: tasks.partial,
                    unavailable: tasks.unavailable,
                    protocol: engine.protocol_diagnostics,
                    exhausted: engine.recoverable_request_exhaustions,
                    admission: if engine.request_admission_stopped {
                        "stopped"
                    } else {
                        "open"
                    },
                    not_started: tasks.not_started,
                    remaining_decisions: engine.remaining_decisions,
                    remaining_locations: engine.remaining_locations,
                    rejected_locations: engine.rejected_locations,
                })
            )
        }
        TranslationEngineSummary::Generic(engine) => {
            writeln!(
                output,
                "{}",
                localizer.format(UiMessage::ResultGenericTranslateSummary {
                    total: tasks.planned,
                    started: tasks.started,
                    not_started: tasks.not_started,
                    complete: tasks.complete,
                    partial: tasks.partial,
                    unavailable: tasks.unavailable,
                    failed: tasks.failed,
                    cancelled: tasks.cancelled,
                    planned_units: engine.planned_units,
                    remaining_units: engine.remaining_units,
                    rejected_units: engine.rejected_units,
                    cleared: engine.cleared_units,
                    reused: engine.reused_units,
                    accepted: engine.accepted_units,
                    written: engine.written_units,
                    conflicted: engine.conflicted_units,
                    problems: engine.response_problems,
                })
            )?;
            writeln!(
                output,
                "{}",
                localizer.format(UiMessage::TranslateIncompleteGenericReason {
                    partial: tasks.partial,
                    unavailable: tasks.unavailable,
                    conflicted: engine.conflicted_units,
                    problems: engine.response_problems,
                    exhausted: engine.recoverable_request_exhaustions,
                    admission: if engine.request_admission_stopped {
                        "stopped"
                    } else {
                        "open"
                    },
                    not_started: tasks.not_started,
                    remaining_units: engine.remaining_units,
                    rejected_units: engine.rejected_units,
                })
            )
        }
    }
}

fn render_generic_command_report(
    report: GenericCommandRunReport,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ExitCode {
    let GenericCommandRunReport {
        result,
        shutdown_errors,
        mut pending_project_log,
        panic_log_path,
        selected_api_key_redactor,
        translation_summary,
    } = report;
    stdout.select_api_key_redactor(selected_api_key_redactor.clone());
    stderr.select_api_key_redactor(selected_api_key_redactor);
    if let Some(project_log) = pending_project_log.as_mut() {
        project_log.prepare_for_result_presentation();
    }
    let had_presentation_failure = pending_project_log
        .as_ref()
        .is_some_and(PendingProjectLog::has_presentation_failure);
    let panic_report = pending_project_log
        .as_mut()
        .map(PendingProjectLog::arm_presentation_panic);
    let run_log = RunLogPresentation::new(
        pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::log_path)
            .map(PathBuf::from)
            .or(panic_log_path),
    );
    catch_generic_logged_presentation(
        panic_report,
        &run_log,
        localizer,
        stdout,
        stderr,
        |stdout, stderr| {
            render_generic_command_result_with_run_log(
                result,
                &shutdown_errors,
                translation_summary,
                pending_project_log,
                LoggedPresentationContext {
                    run_log: &run_log,
                    had_presentation_failure,
                    localizer,
                },
                stdout,
                stderr,
            )
        },
    )
}

#[cfg(test)]
fn render_generic_command_result(
    result: GenericCommandRunResult,
    shutdown_errors: &[GenericShutdownError],
    pending_project_log: Option<PendingProjectLog>,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let mut stdout = StreamPresentation::new(ProcessStream::Stdout, stdout);
    let mut stderr = StreamPresentation::new(ProcessStream::Stderr, stderr);
    let run_log = RunLogPresentation::new(
        pending_project_log
            .as_ref()
            .and_then(PendingProjectLog::log_path)
            .map(PathBuf::from),
    );
    let had_presentation_failure = pending_project_log
        .as_ref()
        .is_some_and(PendingProjectLog::has_presentation_failure);
    render_generic_command_result_with_run_log(
        result,
        shutdown_errors,
        None,
        pending_project_log,
        LoggedPresentationContext {
            run_log: &run_log,
            had_presentation_failure,
            localizer,
        },
        &mut stdout,
        &mut stderr,
    )
}

fn render_generic_command_result_with_run_log(
    result: GenericCommandRunResult,
    shutdown_errors: &[GenericShutdownError],
    translation_summary: Option<TranslationTerminalSummary>,
    pending_project_log: Option<PendingProjectLog>,
    context: LoggedPresentationContext<'_>,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
) -> ExitCode {
    let LoggedPresentationContext {
        run_log,
        had_presentation_failure,
        localizer,
    } = context;
    match result {
        GenericCommandRunResult::Succeeded(output) => {
            if let Err(source) = render_generic_output(&output, localizer, stdout) {
                let diagnostic = process_output_failure_report(
                    StateEffect::AppliedFinalizationFailed,
                    RuntimeOperation::WriteStdout,
                    &source,
                );
                let mut presentation_failures = vec![diagnostic.clone()];
                if let Err(source) = render_primary_error(&diagnostic, localizer, stderr) {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                if let Err(source) =
                    render_generic_shutdown_errors(shutdown_errors, true, localizer, stderr)
                {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                if presentation_failures.len() == 1 {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stderr,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &mut presentation_failures,
                    );
                }
                let _ = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                ExitCode::FAILURE
            } else if let Err(source) = render_generic_success_warnings(&output, localizer, stderr)
            {
                let diagnostic = process_output_failure_report(
                    StateEffect::AppliedFinalizationFailed,
                    RuntimeOperation::WriteStderr,
                    &source,
                );
                let mut presentation_failures = vec![diagnostic.clone()];
                if let Err(source) = render_primary_error(&diagnostic, localizer, stderr) {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                if let Err(source) =
                    render_generic_shutdown_errors(shutdown_errors, true, localizer, stderr)
                {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                if presentation_failures.len() == 1 {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stderr,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &mut presentation_failures,
                    );
                }
                let _ = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                ExitCode::FAILURE
            } else {
                let mut presentation_failures = Vec::new();
                if !shutdown_errors.is_empty() {
                    if let Err(source) = writeln!(
                        stderr,
                        "{}",
                        localizer.format(UiMessage::DiagnosticErrorHeading)
                    ) {
                        presentation_failures.push(process_output_failure_report(
                            StateEffect::AppliedFinalizationFailed,
                            RuntimeOperation::WriteStderr,
                            &source,
                        ));
                    } else if let Err(source) =
                        render_generic_shutdown_errors(shutdown_errors, false, localizer, stderr)
                    {
                        presentation_failures.push(process_output_failure_report(
                            StateEffect::AppliedFinalizationFailed,
                            RuntimeOperation::WriteStderr,
                            &source,
                        ));
                    }
                }
                if presentation_failures.is_empty()
                    && (!shutdown_errors.is_empty() || had_presentation_failure)
                {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stderr,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStderr,
                        &mut presentation_failures,
                    );
                } else if presentation_failures.is_empty() {
                    record_run_log_path(
                        run_log,
                        localizer,
                        stdout,
                        StateEffect::AppliedFinalizationFailed,
                        RuntimeOperation::WriteStdout,
                        &mut presentation_failures,
                    );
                }
                let had_presentation_failure = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    StateEffect::AppliedFinalizationFailed,
                    localizer,
                    stdout,
                    stderr,
                );
                if had_presentation_failure {
                    return ExitCode::FAILURE;
                }
                if shutdown_errors.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
        }
        GenericCommandRunResult::Failed(error) => {
            let presentation_effect = generic_command_error_report(&error).effect();
            let mut presentation_failures = Vec::new();
            let diagnostic_result = if let Some(manual) = error.manual_error() {
                render_manual_command_error(manual, localizer, stderr)
            } else {
                let diagnostic = generic_command_error_report(&error);
                render_primary_error(&diagnostic, localizer, stderr)
            };
            if let Err(source) = diagnostic_result {
                presentation_failures.push(process_output_failure_report(
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if let Err(source) =
                render_generic_shutdown_errors(shutdown_errors, true, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if let Some(summary) = translation_summary
                && let Err(source) = render_translation_terminal_summary(summary, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if presentation_failures.is_empty() {
                record_run_log_path(
                    run_log,
                    localizer,
                    stderr,
                    presentation_effect,
                    RuntimeOperation::WriteStderr,
                    &mut presentation_failures,
                );
            }
            let _ = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                presentation_effect,
                localizer,
                stdout,
                stderr,
            );
            ExitCode::FAILURE
        }
        GenericCommandRunResult::Interrupted => {
            let mut presentation_failures = Vec::new();
            if let Err(source) =
                writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if let Some(summary) = translation_summary
                && let Err(source) = render_translation_terminal_summary(summary, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if !shutdown_errors.is_empty() {
                if let Err(source) = writeln!(
                    stderr,
                    "{}",
                    localizer.format(UiMessage::DiagnosticErrorHeading)
                ) {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::ProgressPreserved,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                } else if let Err(source) =
                    render_generic_shutdown_errors(shutdown_errors, false, localizer, stderr)
                {
                    presentation_failures.push(process_output_failure_report(
                        StateEffect::ProgressPreserved,
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
            }
            if presentation_failures.is_empty() {
                record_run_log_path(
                    run_log,
                    localizer,
                    stderr,
                    StateEffect::ProgressPreserved,
                    RuntimeOperation::WriteStderr,
                    &mut presentation_failures,
                );
            }
            let cancellation_failed = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                StateEffect::ProgressPreserved,
                localizer,
                stdout,
                stderr,
            );
            if cancellation_failed {
                ExitCode::FAILURE
            } else if shutdown_errors.is_empty() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn render_generic_output(
    output: &GenericCommandOutput,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
) -> io::Result<()> {
    let count =
        |value: usize| u64::try_from(value).expect("当前目标平台的结果计数必须能用 u64 表达");
    match output {
        GenericCommandOutput::Init { project } => writeln!(
            stdout,
            "{}",
            localizer.format(UiMessage::ResultInitCompleted {
                project: project.project_name().as_str(),
            })
        ),
        GenericCommandOutput::Extract { project, outcome } => {
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultExtractCompleted {
                    project: project.as_str(),
                })
            )?;
            match outcome {
                crate::generic::ExtractOutcome::Unchanged {
                    files,
                    groups,
                    units,
                } => writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultGenericExtractUnchanged {
                        files: count(*files),
                        groups: count(*groups),
                        units: count(*units),
                    })
                ),
                crate::generic::ExtractOutcome::Updated {
                    files,
                    groups,
                    units,
                    preserved_translations,
                    cleared_translations,
                } => writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultGenericExtractUpdated {
                        files: count(*files),
                        groups: count(*groups),
                        units: count(*units),
                        preserved: count(*preserved_translations),
                        cleared: count(*cleared_translations),
                    })
                ),
            }
        }
        GenericCommandOutput::Translate {
            project,
            profile_id,
            summary,
        } => {
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultTranslateCompleted {
                    project: project.as_str(),
                    profile: profile_id,
                })
            )?;
            let status = if summary.is_incomplete() {
                "incomplete"
            } else if summary.total_tasks == 0 {
                "no_work"
            } else {
                "complete"
            };
            let status = localizer.format(UiMessage::ResultTranslateStatusValue { status });
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultTranslateStatus { status: &status })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultGenericTranslateSummary {
                    total: count(summary.total_tasks),
                    started: count(summary.started_tasks),
                    not_started: count(summary.not_started_tasks),
                    complete: count(summary.complete_tasks),
                    partial: count(summary.partial_tasks),
                    unavailable: count(summary.unavailable_tasks),
                    failed: 0,
                    cancelled: 0,
                    planned_units: count(summary.planned_units),
                    remaining_units: count(summary.remaining_units),
                    rejected_units: count(summary.rejected_units),
                    cleared: count(summary.cleared_units),
                    reused: count(summary.reused_units),
                    accepted: count(summary.accepted_units),
                    written: count(summary.written_units),
                    conflicted: count(summary.conflicted_units),
                    problems: count(summary.response_problems),
                })
            )?;
            if summary.total_tasks == 0 && !summary.is_incomplete() {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::NoticeNoModelRequest)
                )?;
            }
            Ok(())
        }
        GenericCommandOutput::WriteBack {
            project,
            output_root,
            translated_units,
            retained_source_units,
        } => {
            let output_root = public_path(output_root);
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultWriteBackCompleted {
                    project: project.as_str(),
                })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultOutputDirectory { path: &output_root })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultGenericWriteBackSummary {
                    translated: count(*translated_units),
                    original: count(*retained_source_units),
                })
            )?;
            Ok(())
        }
        GenericCommandOutput::Manual { summary } => {
            render_manual_command_summary(summary, localizer, stdout)
        }
        GenericCommandOutput::Lua { project, .. } => writeln!(
            stdout,
            "{}",
            localizer.format(UiMessage::ResultProjectLuaCompleted {
                project: project.as_str(),
            })
        ),
    }
}

fn render_generic_success_warnings(
    output: &GenericCommandOutput,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let GenericCommandOutput::Translate {
        project, summary, ..
    } = output
    else {
        return Ok(());
    };
    if !summary.is_incomplete() {
        return Ok(());
    }
    let count =
        |value: usize| u64::try_from(value).expect("当前目标平台的结果计数必须能用 u64 表达");
    let object = localizer.format(UiMessage::TranslateIncompleteObject {
        project: project.as_str(),
    });
    let reason = localizer.format(UiMessage::TranslateIncompleteGenericReason {
        partial: count(summary.partial_tasks),
        unavailable: count(summary.unavailable_tasks),
        conflicted: count(summary.conflicted_units),
        problems: count(summary.response_problems),
        exhausted: count(summary.recoverable_request_exhaustions),
        admission: if summary.request_admission_stopped {
            "stopped"
        } else {
            "open"
        },
        not_started: count(summary.not_started_tasks),
        remaining_units: count(summary.remaining_units),
        rejected_units: count(summary.rejected_units),
    });
    let impact = render_state_effect_impact(StateEffect::ProgressPreserved, localizer);
    let help = localizer.format(if summary.rejected_units > 0 {
        UiMessage::TranslateIncompleteRejectedHelp
    } else {
        UiMessage::TranslateIncompleteHelp
    });
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticWarningHeading)
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticObject { subject: &object })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticExplanation { reason: &reason })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticImpact { impact: &impact })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticResolution { action: &help })
    )
}

fn render_primary_error(
    report: &DiagnosticReport,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticErrorHeading)
    )?;
    writeln!(stderr, "{}", render_diagnostic_report(report, localizer))
}

fn render_test_command_report(
    report: TestCommandReport,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let business_exit = if report.interrupted {
        ExitCode::from(130)
    } else if report.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    let rendered = (|| -> io::Result<()> {
        writeln!(
            stdout,
            "{}",
            localizer.format(UiMessage::ResultTestConfiguration { status: "passed" })
        )?;
        for client in &report.clients {
            let status = if client.diagnostic.is_some() {
                "failed"
            } else {
                "passed"
            };
            let protocol = match client.protocol {
                crate::runtime::llm::OpenAiProtocol::ChatCompletions => "chat_completions",
                crate::runtime::llm::OpenAiProtocol::Responses => "responses",
            };
            let stream = if client.stream {
                "streaming"
            } else {
                "non_streaming"
            };
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultTestClient {
                    client: &client.id,
                    status,
                    protocol,
                    stream,
                })
            )?;
            if let Some(diagnostic) = &client.diagnostic {
                render_primary_error(diagnostic, localizer, stderr)?;
            }
        }
        for diagnostic in &report.command_diagnostics {
            render_primary_error(diagnostic, localizer, stderr)?;
        }
        writeln!(
            stdout,
            "{}",
            localizer.format(UiMessage::ResultTestSummary {
                passed: u64::try_from(report.passed_clients())
                    .expect("Client 数量必须能用 u64 表达"),
                failed: u64::try_from(report.failed_clients())
                    .expect("Client 数量必须能用 u64 表达"),
                skipped: u64::try_from(report.skipped_clients())
                    .expect("Client 数量必须能用 u64 表达"),
                total: u64::try_from(report.total_clients).expect("Client 数量必须能用 u64 表达"),
            })
        )?;
        if report.interrupted {
            writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))?;
        }
        Ok(())
    })();
    if rendered.is_err() {
        ExitCode::FAILURE
    } else {
        business_exit
    }
}

fn render_generic_shutdown_errors(
    errors: &[GenericShutdownError],
    follows_primary: bool,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    for (index, error) in errors.iter().enumerate() {
        if follows_primary || index > 0 {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: "shutdown",
                })
            )?;
        }
        writeln!(
            stderr,
            "{}",
            render_diagnostic_report(&error.diagnostic_report(), localizer)
        )?;
    }
    Ok(())
}

fn catch_generic_logged_presentation(
    panic_report: Option<DiagnosticReport>,
    run_log: &RunLogPresentation,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
    presentation: impl FnOnce(&mut StreamPresentation<'_>, &mut StreamPresentation<'_>) -> ExitCode,
) -> ExitCode {
    match catch_unwind(AssertUnwindSafe(|| presentation(stdout, stderr))) {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            let Some(report) = panic_report else {
                std::panic::resume_unwind(payload);
            };
            drop(payload);
            let _ = render_primary_error(&report, localizer, stderr);
            let _ = run_log.write(localizer, stderr);
            finalize_process_output(
                ExitCode::FAILURE,
                StateEffect::OutcomeUnknown,
                localizer,
                stdout,
                stderr,
            )
        }
    }
}

fn catch_logged_presentation(
    panic_boundary: Option<CommandPanicBoundary>,
    run_log: &RunLogPresentation,
    localizer: &UiLocalizer,
    stdout: &mut StreamPresentation<'_>,
    stderr: &mut StreamPresentation<'_>,
    presentation: impl FnOnce(&mut StreamPresentation<'_>, &mut StreamPresentation<'_>) -> ExitCode,
) -> ExitCode {
    let result = catch_unwind(AssertUnwindSafe(|| presentation(stdout, stderr)));
    match result {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            let Some(panic_boundary) = panic_boundary else {
                std::panic::resume_unwind(payload);
            };
            // 与命令边界相同，payload 只负责触发控制流，绝不读取或格式化。
            drop(payload);
            let error = panic_boundary.panic_error();
            let _ = CommandResultRenderer::render_failure(Some(&error), None, localizer, stderr);
            let _ = run_log.write(localizer, stderr);
            finalize_process_output(
                ExitCode::FAILURE,
                StateEffect::OutcomeUnknown,
                localizer,
                stdout,
                stderr,
            )
        }
    }
}

fn render_diagnostic_report_fatal(
    localizer: &UiLocalizer,
    report: &DiagnosticReport,
    stderr: &mut dyn Write,
) -> ExitCode {
    if render_primary_error(report, localizer, stderr).is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::FAILURE
}

fn render_project_log_warning(
    localizer: &UiLocalizer,
    warning: &ProjectLogWarning,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    for report in &warning.project_log {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticWarningHeading)
        )?;
        writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
    }
    for report in &warning.task_records {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticWarningHeading)
        )?;
        writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
    }
    for report in &warning.presentation_failures {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticErrorHeading)
        )?;
        writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
    }
    Ok(())
}

#[cfg(test)]
fn render_project_log_warning_if_present(
    localizer: &UiLocalizer,
    warning: Option<&ProjectLogWarning>,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    if let Some(warning) = warning {
        render_project_log_warning(localizer, warning, stderr)?;
    }
    Ok(())
}

#[cfg(test)]
fn render_fatal(
    localizer: &UiLocalizer,
    _error: &dyn std::fmt::Display,
    stderr: &mut dyn Write,
) -> ExitCode {
    let diagnostic = DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::runtime(RuntimeIssue::ProcessPanicked {
            boundary: RuntimePanicBoundary::ProcessStartup,
        }),
    );
    render_diagnostic_report_fatal(localizer, &diagnostic, stderr)
}

fn render_distribution_layout_error(
    localizer: &UiLocalizer,
    error: &DistributionLayoutError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let report = DiagnosticReport::new(StateEffect::Unchanged, error.diagnostic());
    render_diagnostic_report_fatal(localizer, &report, stderr)
}

fn render_configuration_load_error(
    localizer: &UiLocalizer,
    error: &ConfigurationLoadError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let report = DiagnosticReport::new(StateEffect::Unchanged, error.diagnostic());
    render_diagnostic_report_fatal(localizer, &report, stderr)
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use secrecy::SecretString;

    use super::*;
    use crate::application::command::{RpgMakerCommandOutput, ShutdownFailures};
    use crate::application::config::CommonCommandConfiguration;
    use crate::application::generic_command::{GenericCommandError, GenericTranslationSummary};
    use crate::application::project_log::{ActiveProjectLog, CommandLogStart, start_command_log};
    use crate::diagnostic::DiagnosticStage;
    use crate::rpg_maker::extract::{
        ExtractOutput, RulesCommandNonStringType, RulesCommandNonStringWarning,
    };
    use crate::rpg_maker::translate::{TranslateOutput, TranslationSummary};
    use crate::runtime::performance::RunPerformanceCounters;
    use crate::runtime::project_log::{
        GenericTranslationSummary as LoggedGenericTranslationSummary, ProjectLogCommand,
        ProjectLogEngine, RpgMakerTranslationSummary as LoggedRpgMakerTranslationSummary,
        RunPlanValueSource as ProjectLogValueSource, TranslationTaskCounters,
    };

    fn active_project_log(root: &Path, project: &str) -> (ActiveProjectLog, PathBuf) {
        let common = CommonCommandConfiguration::for_test(root);
        fs::create_dir_all(root.join("generic").join(project)).expect("应建立项目工作区");
        let active = start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::SimplifiedChinese,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Lua,
            performance: Arc::new(RunPerformanceCounters::default()),
            selected_api_key_redactor: None,
        });
        let run_id = active.run_id().expect("项目日志必须取得 RunId").to_owned();
        let path = root
            .join("generic")
            .join(project)
            .join("logs")
            .join(format!("{run_id}.jsonl"));
        (active, path)
    }

    fn cancelled_project_log(root: &Path, project: &str) -> (PendingProjectLog, PathBuf) {
        let (active, path) = active_project_log(root, project);
        (active.pending_cancelled(), path)
    }

    fn failed_project_log(
        root: &Path,
        project: &str,
        report: DiagnosticReport,
    ) -> (PendingProjectLog, PathBuf) {
        let (active, path) = active_project_log(root, project);
        (active.pending_failure(report), path)
    }

    fn run_finished_kind(path: &Path) -> String {
        fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("项目日志每行都必须是 JSON")
            })
            .find(|record| record["event"] == "run.finished")
            .expect("项目日志必须有运行终态")["payload"]["result"]["kind"]
            .as_str()
            .expect("运行终态 kind 必须是字符串")
            .to_owned()
    }

    fn run_diagnostic_count(path: &Path) -> usize {
        fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("项目日志每行都必须是 JSON")
            })
            .filter(|record| record["event"] == "diagnostic.run")
            .count()
    }

    fn run_diagnostic_relations(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .expect("项目日志应可读取")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("项目日志每行都必须是 JSON")
            })
            .filter(|record| record["event"] == "diagnostic.run")
            .map(|record| {
                record["payload"]["relation"]
                    .as_str()
                    .expect("诊断 relation 必须是字符串")
                    .to_owned()
            })
            .collect()
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ObservedStream {
        Stdout,
        Stderr,
    }

    struct OrderedOutput {
        stream: ObservedStream,
        order: Arc<Mutex<Vec<ObservedStream>>>,
        bytes: Vec<u8>,
    }

    impl OrderedOutput {
        fn new(stream: ObservedStream, order: Arc<Mutex<Vec<ObservedStream>>>) -> Self {
            Self {
                stream,
                order,
                bytes: Vec::new(),
            }
        }
    }

    impl Write for OrderedOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if !buffer.is_empty() {
                self.order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(self.stream);
                self.bytes.extend_from_slice(buffer);
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TestShutdownError;

    impl fmt::Display for TestShutdownError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试关闭失败")
        }
    }

    impl std::error::Error for TestShutdownError {}

    #[derive(Default)]
    struct FailingOutput {
        write_attempts: usize,
    }

    impl Write for FailingOutput {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            self.write_attempts += 1;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "测试 stdout 已关闭",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FlushFailingOutput {
        bytes: Vec<u8>,
        flush_attempts: usize,
    }

    #[derive(Default)]
    struct FlushCountingOutput {
        bytes: Vec<u8>,
        flush_attempts: usize,
    }

    #[derive(Default)]
    struct SecondFlushFailingOutput {
        bytes: Vec<u8>,
        flush_attempts: usize,
    }

    impl Write for FlushCountingOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_attempts += 1;
            Ok(())
        }
    }

    impl Write for FlushFailingOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_attempts += 1;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "测试输出流最终刷新失败",
            ))
        }
    }

    impl Write for SecondFlushFailingOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_attempts += 1;
            if self.flush_attempts == 2 {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "测试关闭项目日志后的最终 flush 失败",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FlushPanickingOutput {
        bytes: Vec<u8>,
        flush_attempts: usize,
    }

    impl Write for FlushPanickingOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_attempts += 1;
            panic!("测试输出流最终刷新 panic")
        }
    }

    #[test]
    fn successful_extract_keeps_summary_on_stdout_and_warnings_on_stderr() {
        let output = RpgMakerCommandOutput::Extract {
            output: ExtractOutput {
                name: "project".parse().expect("测试项目名应合法"),
                rules_warnings: vec![RulesCommandNonStringWarning {
                    rule_number: 2,
                    source_file: "Map001.json".to_owned(),
                    command_code: 355,
                    parameter: 0,
                    actual_type: RulesCommandNonStringType::Number,
                    skipped_count: 3,
                }],
            },
            plan_source: ProjectLogValueSource::Explicit,
            owners: vec!["Rules".to_owned()],
            run_plan_warnings: Vec::new(),
            has_saved_plan: true,
        };
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = render_command_report(
            CommandRunResult::Succeeded(output),
            None,
            None,
            &localizer,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::SUCCESS);
        let stdout = String::from_utf8(stdout).expect("stdout 应为 UTF-8");
        let stderr = String::from_utf8(stderr).expect("stderr 应为 UTF-8");
        let plain_stderr = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(stdout.contains("提取完成"));
        assert!(stdout.contains("project"));
        assert!(!stdout.contains("非字符串"));
        assert!(plain_stderr.contains("Rules rule 2"));
        assert!(plain_stderr.contains("Map001.json"));
        assert!(plain_stderr.contains("code=355"));
        assert!(plain_stderr.contains("parameter=0"));
        assert!(plain_stderr.contains("type=number"));
        assert!(plain_stderr.contains("skipped=3"));
    }

    #[test]
    fn run_log_path_is_shown_once_on_the_stream_for_the_final_result() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let temporary = tempfile::tempdir().expect("应建立测试目录");

        let (rpg_active, rpg_path) = active_project_log(temporary.path(), "rpg-success-log");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let rpg_exit = render_command_report(
            CommandRunResult::Succeeded(RpgMakerCommandOutput::Lua {
                project: "rpg-success-log".parse().expect("测试项目名应合法"),
            }),
            None,
            Some(rpg_active.pending_succeeded()),
            &localizer,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(rpg_exit, ExitCode::SUCCESS);
        let plain_stdout = String::from_utf8(stdout)
            .expect("stdout 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(plain_stdout.matches("运行记录：").count(), 1);
        assert!(plain_stdout.contains(&rpg_path.to_string_lossy().to_string()));
        assert!(stderr.is_empty(), "成功运行的日志路径不得写入 stderr");

        let generic_error = GenericCommandError::Signal {
            source: io::Error::other("测试 Generic 业务失败"),
            operation: None,
            state_applied: false,
        };
        let generic_report = generic_command_error_report(&generic_error);
        let (generic_active, generic_path) =
            active_project_log(temporary.path(), "generic-failed-log");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let generic_exit = render_generic_command_result(
            GenericCommandRunResult::Failed(generic_error),
            &[],
            Some(generic_active.pending_failure(generic_report)),
            &localizer,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(generic_exit, ExitCode::FAILURE);
        assert!(stdout.is_empty(), "失败运行的日志路径不得写入 stdout");
        let plain_stderr = String::from_utf8(stderr)
            .expect("stderr 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(plain_stderr.matches("运行记录：").count(), 1);
        assert!(plain_stderr.contains(&generic_path.to_string_lossy().to_string()));

        let (cancelled, cancelled_path) =
            cancelled_project_log(temporary.path(), "generic-cancelled-log");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let cancelled_exit = render_generic_command_result(
            GenericCommandRunResult::Interrupted,
            &[],
            Some(cancelled),
            &localizer,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(cancelled_exit, ExitCode::from(130));
        assert!(stdout.is_empty(), "取消运行的日志路径不得写入 stdout");
        let plain_stderr = String::from_utf8(stderr)
            .expect("stderr 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(plain_stderr.matches("运行记录：").count(), 1);
        assert!(plain_stderr.contains(&cancelled_path.to_string_lossy().to_string()));

        let panic_path = temporary.path().join("generic-panic/logs/run-000001.jsonl");
        let panic_error = GenericCommandError::Signal {
            source: io::Error::other("测试顶层业务 panic"),
            operation: None,
            state_applied: false,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let panic_exit = render_generic_command_report(
            GenericCommandRunReport {
                result: GenericCommandRunResult::Failed(panic_error),
                shutdown_errors: Vec::new(),
                pending_project_log: None,
                panic_log_path: Some(panic_path.clone()),
                selected_api_key_redactor: None,
                translation_summary: None,
            },
            &localizer,
            &mut StreamPresentation::new(ProcessStream::Stdout, &mut stdout),
            &mut StreamPresentation::new(ProcessStream::Stderr, &mut stderr),
        );
        assert_eq!(panic_exit, ExitCode::FAILURE);
        assert!(stdout.is_empty());
        let plain_stderr = String::from_utf8(stderr)
            .expect("stderr 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(plain_stderr.matches("运行记录：").count(), 1);
        assert!(plain_stderr.contains(&panic_path.to_string_lossy().to_string()));
    }

    #[test]
    fn run_log_presentation_removes_windows_verbatim_drive_and_unc_prefixes() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        for (path, expected) in [
            (
                r"\\?\C:\project\logs\run-000001.jsonl",
                r"C:\project\logs\run-000001.jsonl",
            ),
            (
                r"\\?\UNC\server\share\logs\run-000001.jsonl",
                r"\\server\share\logs\run-000001.jsonl",
            ),
        ] {
            let presentation = RunLogPresentation::new(Some(PathBuf::from(path)));
            let mut output = Vec::new();
            presentation.write(&localizer, &mut output).unwrap();
            let output = String::from_utf8(output)
                .unwrap()
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(output.contains(expected));
            assert!(!output.contains(r"\\?\"));
        }
    }

    #[test]
    fn generic_translate_prints_no_work_complete_and_structured_incomplete_results() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let output = |summary| GenericCommandOutput::Translate {
            project: "generic-status".parse().expect("测试项目名应合法"),
            profile_id: "default".to_owned(),
            summary,
        };

        for (summary, expected) in [
            (GenericTranslationSummary::default(), "状态：无需处理"),
            (
                GenericTranslationSummary {
                    total_tasks: 1,
                    complete_tasks: 1,
                    ..GenericTranslationSummary::default()
                },
                "状态：完整",
            ),
            (
                GenericTranslationSummary {
                    conflicted_units: 1,
                    ..GenericTranslationSummary::default()
                },
                "状态：未完整",
            ),
            (
                GenericTranslationSummary {
                    planned_units: 1,
                    remaining_units: 1,
                    rejected_units: 1,
                    ..GenericTranslationSummary::default()
                },
                "状态：未完整",
            ),
        ] {
            let mut stdout = Vec::new();
            render_generic_output(&output(summary), &localizer, &mut stdout)
                .expect("Generic Translate 结果应可呈现");
            let stdout = String::from_utf8(stdout)
                .expect("stdout 应为 UTF-8")
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(stdout.contains(expected), "实际输出：{stdout}");
        }

        let incomplete = output(GenericTranslationSummary {
            total_tasks: 2,
            complete_tasks: 1,
            partial_tasks: 1,
            unavailable_tasks: 0,
            conflicted_units: 3,
            response_problems: 4,
            ..GenericTranslationSummary::default()
        });
        let mut stdout = Vec::new();
        render_generic_output(&incomplete, &localizer, &mut stdout).expect("未完整结果应可呈现");
        let stdout = String::from_utf8(stdout)
            .expect("stdout 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(stdout.contains("状态：未完整"));

        let mut stderr = Vec::new();
        render_generic_success_warnings(&incomplete, &localizer, &mut stderr)
            .expect("未完整警告应可呈现");
        let stderr = String::from_utf8(stderr)
            .expect("stderr 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        for expected in [
            "警告：",
            "对象：项目 generic-status 的本次 Translate",
            "部分任务 1",
            "写入冲突 3",
            "响应问题 4",
            "影响：",
            "处理办法：",
        ] {
            assert!(stderr.contains(expected), "缺少 {expected:?}：{stderr}");
        }

        let rejected = output(GenericTranslationSummary {
            planned_units: 157,
            remaining_units: 157,
            rejected_units: 157,
            ..GenericTranslationSummary::default()
        });
        let mut stdout = Vec::new();
        render_generic_output(&rejected, &localizer, &mut stdout).unwrap();
        let stdout = String::from_utf8(stdout)
            .unwrap()
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(stdout.contains("Rejected Unit 157"));
        assert!(!stdout.contains("全部翻译单元均为最新状态"));
        let mut stderr = Vec::new();
        render_generic_success_warnings(&rejected, &localizer, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr)
            .unwrap()
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(stderr.contains("--retry-rejected"));
        assert!(stderr.contains("manual export --selection rejected"));
    }

    #[test]
    fn failed_and_cancelled_translate_reports_show_current_summary_on_stderr_for_both_engines() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let failed_tasks = TranslationTaskCounters::new(5, 3, 1, 0, 1, 1, 0, 2)
            .expect("失败任务汇总必须满足恒等式");
        let cancelled_tasks = TranslationTaskCounters::new(5, 2, 1, 0, 0, 0, 1, 3)
            .expect("取消任务汇总必须满足恒等式");
        let rpg = TranslationEngineSummary::RpgMaker(LoggedRpgMakerTranslationSummary {
            accepted_decisions: 2,
            written_locations: 3,
            remaining_decisions: 6,
            remaining_locations: 8,
            rejected_locations: 2,
            protocol_diagnostics: 1,
            recoverable_request_exhaustions: 1,
            request_admission_stopped: true,
            retained: 0,
            invalidated: 0,
            not_applicable: 0,
            reused: 0,
        });
        let generic = TranslationEngineSummary::Generic(LoggedGenericTranslationSummary {
            planned_units: 10,
            remaining_units: 7,
            rejected_units: 2,
            cleared_units: 0,
            reused_units: 0,
            accepted_units: 3,
            written_units: 3,
            conflicted_units: 0,
            response_problems: 1,
            recoverable_request_exhaustions: 1,
            request_admission_stopped: true,
        });

        for (tasks, engine, failed, expected_exit) in [
            (failed_tasks, rpg, true, ExitCode::FAILURE),
            (cancelled_tasks, rpg, false, ExitCode::from(130)),
        ] {
            let result = if failed {
                CommandRunResult::Failed(ProductionCommandError::stdout_write(io::Error::other(
                    "测试失败正文不得公开",
                )))
            } else {
                CommandRunResult::Interrupted
            };
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = render_command_report_with_run_log(
                result,
                None,
                Some(TranslationTerminalSummary { tasks, engine }),
                None,
                LoggedPresentationContext {
                    run_log: &RunLogPresentation::new(None),
                    had_presentation_failure: false,
                    localizer: &localizer,
                },
                &mut StreamPresentation::new(ProcessStream::Stdout, &mut stdout),
                &mut StreamPresentation::new(ProcessStream::Stderr, &mut stderr),
            );
            assert_eq!(exit, expected_exit);
            assert!(stdout.is_empty());
            let stderr = String::from_utf8(stderr)
                .expect("stderr 应为 UTF-8")
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(stderr.contains("计划 5 个任务"), "实际 stderr：{stderr}");
            assert!(stderr.contains(if failed { "已开始 3" } else { "已开始 2" }));
            assert!(stderr.contains(if failed { "未开始 2" } else { "未开始 3" }));
            assert!(stderr.contains("请求准入已停止"), "实际 stderr：{stderr}");
            assert!(!stderr.contains("测试失败正文不得公开"));
        }

        for (tasks, failed, expected_exit) in [
            (failed_tasks, true, ExitCode::FAILURE),
            (cancelled_tasks, false, ExitCode::from(130)),
        ] {
            let result = if failed {
                GenericCommandRunResult::Failed(GenericCommandError::Signal {
                    source: io::Error::other("测试 Generic 失败正文不得公开"),
                    operation: None,
                    state_applied: false,
                })
            } else {
                GenericCommandRunResult::Interrupted
            };
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = render_generic_command_result_with_run_log(
                result,
                &[],
                Some(TranslationTerminalSummary {
                    tasks,
                    engine: generic,
                }),
                None,
                LoggedPresentationContext {
                    run_log: &RunLogPresentation::new(None),
                    had_presentation_failure: false,
                    localizer: &localizer,
                },
                &mut StreamPresentation::new(ProcessStream::Stdout, &mut stdout),
                &mut StreamPresentation::new(ProcessStream::Stderr, &mut stderr),
            );
            assert_eq!(exit, expected_exit);
            assert!(stdout.is_empty());
            let stderr = String::from_utf8(stderr)
                .expect("stderr 应为 UTF-8")
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(stderr.contains("计划 5 个任务"), "实际 stderr：{stderr}");
            assert!(stderr.contains(if failed { "已开始 3" } else { "已开始 2" }));
            assert!(stderr.contains(if failed { "未开始 2" } else { "未开始 3" }));
            assert!(stderr.contains("请求准入已停止"), "实际 stderr：{stderr}");
            assert!(!stderr.contains("测试 Generic 失败正文不得公开"));
        }
    }

    #[test]
    fn rpg_translate_with_no_tasks_but_remaining_content_is_incomplete() {
        let output = RpgMakerCommandOutput::Translate {
            output: TranslateOutput {
                name: "rpg-incomplete".parse().expect("测试项目名应合法"),
                profile_id: "default".to_owned(),
                summary: TranslationSummary {
                    total_tasks: 0,
                    started_tasks: 0,
                    not_started_tasks: 0,
                    complete_tasks: 0,
                    partial_tasks: 0,
                    unavailable_tasks: 0,
                    accepted_decisions: 0,
                    written_locations: 0,
                    remaining_decisions: 1,
                    remaining_locations: 2,
                    rejected_locations: 1,
                    protocol_diagnostics: 0,
                    recoverable_request_exhaustions: 0,
                    request_admission_stopped: false,
                    retained: 0,
                    invalidated: 0,
                    not_applicable: 0,
                    reused: 0,
                },
            },
            profile_source: ProjectLogValueSource::Explicit,
        };
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = render_command_report(
            CommandRunResult::Succeeded(output),
            None,
            None,
            &localizer,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::SUCCESS, "正常未完整结果仍使用成功退出码");
        let stdout = String::from_utf8(stdout)
            .expect("stdout 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        let stderr = String::from_utf8(stderr)
            .expect("stderr 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(stdout.contains("状态：未完整"), "实际 stdout：{stdout}");
        assert!(!stdout.contains("状态：无需处理"), "实际 stdout：{stdout}");
        assert!(stdout.contains("Rejected 1 处"), "实际 stdout：{stdout}");
        assert!(!stdout.contains("全部翻译单元均为最新状态"));
        assert!(stderr.contains("警告："), "实际 stderr：{stderr}");
        assert!(stderr.contains("剩余决策 1"), "实际 stderr：{stderr}");
        assert!(stderr.contains("剩余位置 2"), "实际 stderr：{stderr}");
        assert!(stderr.contains("Rejected 1 处"), "实际 stderr：{stderr}");
        assert!(stderr.contains("--retry-rejected"));
        assert!(stderr.contains("manual export --selection rejected"));
    }

    #[test]
    fn extract_warning_write_failure_is_a_process_output_failure() {
        let output = RpgMakerCommandOutput::Extract {
            output: ExtractOutput {
                name: "project".parse().expect("测试项目名应合法"),
                rules_warnings: vec![RulesCommandNonStringWarning {
                    rule_number: 2,
                    source_file: "Map001.json".to_owned(),
                    command_code: 355,
                    parameter: 0,
                    actual_type: RulesCommandNonStringType::Number,
                    skipped_count: 1,
                }],
            },
            plan_source: ProjectLogValueSource::Explicit,
            owners: vec!["Rules".to_owned()],
            run_plan_warnings: Vec::new(),
            has_saved_plan: true,
        };
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stdout = Vec::new();
        let mut stderr = FailingOutput::default();

        let exit = render_command_report(
            CommandRunResult::Succeeded(output),
            None,
            None,
            &localizer,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(stderr.write_attempts > 0);
        let stdout = String::from_utf8_lossy(&stdout);
        assert!(stdout.contains("提取完成"));
        assert!(stdout.contains("project"));
    }

    #[test]
    fn completed_output_is_fully_rendered_before_shutdown_failure() {
        let mut shutdown = ShutdownFailures::default();
        shutdown.push_for_test("test shutdown root", TestShutdownError);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut stdout = OrderedOutput::new(ObservedStream::Stdout, Arc::clone(&order));
        let mut stderr = OrderedOutput::new(ObservedStream::Stderr, Arc::clone(&order));
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let output = RpgMakerCommandOutput::Lua {
            project: "project".parse().expect("测试项目名应合法"),
        };

        let exit = render_command_report(
            CommandRunResult::Succeeded(output),
            Some(shutdown),
            None,
            &localizer,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(
            String::from_utf8(stdout.bytes)
                .expect("stdout 应为 UTF-8")
                .contains("project")
        );
        assert!(
            String::from_utf8(stderr.bytes)
                .expect("stderr 应为 UTF-8")
                .contains(&localizer.format(UiMessage::DiagnosticFailureValue {
                    code: "worker_panicked",
                }))
        );
        let order = order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_stderr = order
            .iter()
            .position(|stream| *stream == ObservedStream::Stderr)
            .expect("清理失败必须写入 stderr");
        assert!(
            order[..first_stderr]
                .iter()
                .all(|stream| *stream == ObservedStream::Stdout)
        );
        assert!(
            order[first_stderr..]
                .iter()
                .all(|stream| *stream == ObservedStream::Stderr),
            "成功输出必须完整写完后才开始呈现清理失败"
        );
    }

    #[test]
    fn utf8_process_runtime_accepts_only_utf8_code_page() {
        assert!(windows_utf8_process_diagnostic_for(CP_UTF8).is_none());

        let diagnostic =
            windows_utf8_process_diagnostic_for(936).expect("非 UTF-8 代码页必须被拒绝");
        assert_eq!(diagnostic.primary().code(), "runtime.windows_code_page");
        assert_eq!(
            diagnostic.primary().stage(),
            DiagnosticStage::ProcessStartup
        );
        assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
        assert_eq!(
            diagnostic.primary().resolution(),
            crate::diagnostic::DiagnosticResolution::ReportBug
        );
        let wire = serde_json::to_value(diagnostic).expect("进程诊断必须可序列化");
        assert_eq!(
            wire["primary"]["issue"]["details"]["actual"],
            serde_json::json!(936)
        );
    }

    #[test]
    fn stdout_failure_is_primary_when_shutdown_also_failed() {
        let mut shutdown = ShutdownFailures::default();
        shutdown.push_for_test("test shutdown root", TestShutdownError);
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let output = RpgMakerCommandOutput::Lua {
            project: "project".parse().expect("测试项目名应合法"),
        };
        let mut stdout = FailingOutput::default();
        let mut stderr = Vec::new();

        let exit = render_command_report(
            CommandRunResult::Succeeded(output),
            Some(shutdown),
            None,
            &localizer,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(
            stdout.write_attempts, 1,
            "stdout 写入失败后不得重试成功摘要"
        );
        let stderr = String::from_utf8(stderr).expect("stderr 应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        let stdout_position = stderr
            .find(&localizer.format(UiMessage::DiagnosticFailureValue {
                code: "stdout_write_failed",
            }))
            .expect("stdout 写入失败必须成为主错误");
        let shutdown_position = stderr
            .find(&localizer.format(UiMessage::DiagnosticFailureValue {
                code: "worker_panicked",
            }))
            .expect("shutdown 失败必须继续呈现");
        assert!(
            stdout_position < shutdown_position,
            "stdout 主错误必须先于 shutdown 相关错误"
        );
        assert!(plain.contains("同时，关闭失败"));
    }

    #[test]
    fn process_panic_hook_is_installed_once_without_exposing_payload() {
        const CHILD_ENV: &str = "ATT_SAFE_PANIC_HOOK_TEST_CHILD";
        const CHILD_MARKER: &str = "ATT_SAFE_PANIC_HOOK_CHILD_COMPLETED";
        const PANIC_BODY: &str = "PANIC_BODY_SENTINEL";
        if std::env::var_os(CHILD_ENV).is_some() {
            install_safe_panic_hook();
            install_safe_panic_hook();
            let outcome = catch_unwind(AssertUnwindSafe(|| panic!("{PANIC_BODY}")));
            assert!(outcome.is_err());
            println!("{CHILD_MARKER}");
            return;
        }

        // 全局 panic hook 无法在同一测试进程中隔离；子进程验证避免污染并行测试。
        let output = Command::new(std::env::current_exe().expect("测试进程路径应可读取"))
            .args([
                "--exact",
                "application::process::tests::process_panic_hook_is_installed_once_without_exposing_payload",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("panic hook 子进程应可启动");
        assert!(output.status.success(), "panic hook 子进程必须成功");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(CHILD_MARKER),
            "panic hook 子测试必须实际执行"
        );
        assert!(!stdout.contains(PANIC_BODY));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(PANIC_BODY));
    }

    #[test]
    fn logged_presentation_panic_uses_pre_registered_report_without_exposing_payload() {
        const PANIC_BODY: &str = "PRESENTATION_PANIC_BODY_SENTINEL";
        let project_workspace = std::path::PathBuf::from("C:/project/workspace");
        let log_path = std::path::PathBuf::from("C:/project/logs/run.jsonl");
        let report = DiagnosticReport::new(
            StateEffect::OutcomeUnknown,
            Diagnostic::runtime(RuntimeIssue::ResultPresentationPanicked {
                engine: crate::diagnostic::RuntimeEngine::RpgMakerMz,
                command: crate::diagnostic::RuntimeCommand::WriteBack,
                project_workspace: crate::diagnostic::SafePath::new(&project_workspace),
                log_path: Some(crate::diagnostic::SafePath::new(&log_path)),
            }),
        );
        let boundary = CommandPanicBoundary::from_report(report);
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let run_log = RunLogPresentation::new(Some(log_path.clone()));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_presentation = StreamPresentation::new(ProcessStream::Stdout, &mut stdout);
        let mut stderr_presentation = StreamPresentation::new(ProcessStream::Stderr, &mut stderr);

        let exit = catch_logged_presentation(
            Some(boundary),
            &run_log,
            &localizer,
            &mut stdout_presentation,
            &mut stderr_presentation,
            |_stdout, _stderr| std::panic::panic_any(PANIC_BODY),
        );
        drop(stdout_presentation);
        drop(stderr_presentation);

        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("panic 诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("内部不变量被破坏"));
        assert!(plain.contains(&project_workspace.to_string_lossy().to_string()));
        assert_eq!(plain.matches("运行记录：").count(), 1);
        assert!(plain.contains(&log_path.to_string_lossy().to_string()));
        assert!(!plain.contains("runtime.result_presentation_panicked"));
        assert!(!stderr.contains(PANIC_BODY));
    }

    #[test]
    fn help_and_parse_errors_do_not_load_the_fixed_configuration() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from(["att", "--help"], &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(!stdout.is_empty());
        assert!(stderr.is_empty());

        stdout.clear();
        let exit = run_from(["att", "--version"], &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(!stdout.is_empty());
        assert!(stderr.is_empty());

        stdout.clear();
        let exit = run_from(["att", "unknown"], &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(!stderr.is_empty());

        stdout.clear();
        stderr.clear();
        let exit = run_from(
            ["att", "mz", "write-back", "--name", "demo"],
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8(stderr.clone())
                .expect("诊断应为 UTF-8")
                .contains("config.toml"),
            "合法业务命令应读取测试可执行文件旁的固定配置"
        );
    }

    #[test]
    fn help_and_version_output_failures_return_failure() {
        let mut stdout = FailingOutput::default();
        let mut stderr = Vec::new();

        let help_exit = run_from(["att", "--help"], &mut stdout, &mut stderr);
        assert_eq!(help_exit, ExitCode::FAILURE);
        assert!(stdout.write_attempts > 0);

        let mut stdout = FailingOutput::default();
        let version_exit = run_from(["att", "--version"], &mut stdout, &mut stderr);
        assert_eq!(version_exit, ExitCode::FAILURE);
        assert!(stdout.write_attempts > 0);

        let mut stdout = FlushFailingOutput::default();
        let mut stderr = FlushCountingOutput::default();
        let help_flush_exit = run_from(["att", "--help"], &mut stdout, &mut stderr);
        assert_eq!(help_flush_exit, ExitCode::FAILURE);
        assert_eq!(
            stdout.flush_attempts, 1,
            "失败的 help stdout flush 不得重试"
        );
        let help_body = String::from_utf8(stdout.bytes)
            .expect("原 help 正文应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        let fallback = String::from_utf8(stderr.bytes)
            .expect("help 回退应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(
            fallback.contains(&help_body),
            "完整 help 正文必须转到 stderr"
        );
        assert!(fallback.contains("对象：stdout"));
        assert!(fallback.contains("原因：无法刷新标准输出"));

        let mut stdout = FlushCountingOutput::default();
        let mut stderr = FlushFailingOutput::default();
        let parse_flush_exit = run_from(["att", "unknown"], &mut stdout, &mut stderr);
        assert_eq!(parse_flush_exit, ExitCode::FAILURE);
        assert_eq!(
            stderr.flush_attempts, 1,
            "失败的 parse stderr flush 不得重试"
        );
        let parse_body = String::from_utf8(stderr.bytes)
            .expect("原 parse error 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        let fallback = String::from_utf8(stdout.bytes)
            .expect("parse error 回退应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(
            fallback.contains(&parse_body),
            "完整 parse error 必须转到 stdout"
        );
        assert!(fallback.contains("对象：stderr"));
        assert!(fallback.contains("原因：无法刷新标准错误"));
    }

    #[test]
    fn final_stdout_or_stderr_flush_failure_returns_failure_and_both_streams_are_attempted() {
        let mut stdout = FlushFailingOutput::default();
        let mut stderr = FlushFailingOutput::default();

        let exit = run_from(["att", "--help"], &mut stdout, &mut stderr);

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(!stdout.bytes.is_empty());
        assert_eq!(stdout.flush_attempts, 1);
        assert_eq!(stderr.flush_attempts, 1);
    }

    #[test]
    fn localized_parse_error_flush_panic_uses_after_parsing_boundary_and_selected_locale() {
        let selected = UiLocalizer::new(UiLocale::Japanese);
        let mut stdout = Vec::new();
        let mut stderr = FlushPanickingOutput::default();

        let exit = run_from(
            ["att", "--ui-language", "ja", "unknown"],
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(stderr.flush_attempts, 1, "panic 后不得重试坏流");
        let original_error = String::from_utf8(stderr.bytes)
            .expect("原 parse error 应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        let fallback = String::from_utf8(stdout)
            .expect("panic 回退应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(
            fallback.contains(&original_error),
            "完整 parse error 必须转到 stdout"
        );
        assert!(
            fallback.contains(&selected.format(UiMessage::DiagnosticFailureValue {
                code: "internal_invariant",
            }))
        );
        assert!(!fallback.contains("PROCESS_STARTUP"));
    }

    #[test]
    fn successful_write_then_failed_flush_moves_complete_body_and_four_fields_once() {
        const BODY: &str = "完整业务正文和运行记录：C:\\project\\logs\\run-000001.jsonl\n";
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut raw_stdout = FlushFailingOutput::default();
        let mut raw_stderr = FlushCountingOutput::default();
        let exit;
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut raw_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut raw_stderr);
            stdout
                .write_all(BODY.as_bytes())
                .expect("逻辑正文应先进入呈现缓冲");
            exit = finalize_process_output(
                ExitCode::SUCCESS,
                StateEffect::Applied,
                &localizer,
                &mut stdout,
                &mut stderr,
            );
        }

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(raw_stdout.flush_attempts, 1, "坏流不得重试 flush");
        assert_eq!(raw_stdout.bytes, BODY.as_bytes(), "底层 write 必须已成功");
        assert_eq!(
            raw_stderr.flush_attempts, 1,
            "健康相反流必须只写入并 flush 一批"
        );
        let fallback = String::from_utf8(raw_stderr.bytes)
            .expect("回退输出应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(fallback.matches(BODY.trim_end()).count(), 1);
        for expected in [
            "错误：",
            "对象：stdout",
            "原因：无法刷新标准输出",
            "影响：相关业务结果已经生效",
            "处理办法：",
        ] {
            assert!(fallback.contains(expected), "缺少 {expected:?}：{fallback}");
        }
    }

    #[test]
    fn previously_flushed_opposite_stream_accepts_a_new_fallback_batch() {
        const BODY: &str = "stderr 原始业务错误\n";
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut raw_stdout = FlushCountingOutput::default();
        let mut raw_stderr = FlushFailingOutput::default();
        let exit;
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut raw_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut raw_stderr);
            stdout.flush().expect("健康 stdout 的先前 flush 应成功");
            stderr
                .write_all(BODY.as_bytes())
                .expect("stderr 业务正文应进入缓冲");
            exit = finalize_process_output(
                ExitCode::FAILURE,
                StateEffect::Unchanged,
                &localizer,
                &mut stdout,
                &mut stderr,
            );
        }

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(raw_stdout.flush_attempts, 2, "新回退批次必须再次 flush");
        let fallback = String::from_utf8(raw_stdout.bytes)
            .expect("回退输出应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(fallback.matches(BODY.trim_end()).count(), 1);
        assert_eq!(fallback.matches("对象：stderr").count(), 1);
    }

    #[test]
    fn api_key_split_across_writes_and_invalid_bytes_cannot_bypass_redaction() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let redactor = Arc::new(ApiKeyRedactor::new(SecretString::from("split-secret")));
        let mut raw_stdout = FlushCountingOutput::default();
        let mut raw_stderr = FlushCountingOutput::default();
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut raw_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut raw_stderr);
            stdout.select_api_key_redactor(Some(redactor.clone()));
            stderr.select_api_key_redactor(Some(redactor));
            stdout
                .write_all(&[0xff])
                .expect("无效 UTF-8 测试前缀应进入逻辑缓冲");
            stdout
                .write_all(b"endpoint=https://example.test/split-")
                .expect("首段逻辑正文应进入缓冲");
            stdout
                .write_all(b"secret/v1")
                .expect("第二段逻辑正文应进入缓冲");
            stdout
                .write_all(&[0xfe])
                .expect("无效 UTF-8 测试后缀应进入逻辑缓冲");
            let exit = finalize_process_output(
                ExitCode::SUCCESS,
                StateEffect::Applied,
                &localizer,
                &mut stdout,
                &mut stderr,
            );
            assert_eq!(exit, ExitCode::SUCCESS);
        }

        let stdout = String::from_utf8(raw_stdout.bytes).expect("stdout 应为 UTF-8");
        assert!(!stdout.contains("split-secret"));
        assert!(stdout.contains("[REDACTED API KEY]"));
        assert!(
            stdout.contains('\u{fffd}'),
            "无效字节必须以 UTF-8 replacement 呈现"
        );
    }

    #[test]
    fn multiple_api_keys_use_one_combined_pass_without_changing_single_key_output() {
        let short = Arc::new(ApiKeyRedactor::new(SecretString::from("secret")));
        let long = Arc::new(ApiKeyRedactor::new(SecretString::from("secret-suffix")));
        let mut multiple_output = FlushCountingOutput::default();
        {
            let mut presentation =
                StreamPresentation::new(ProcessStream::Stdout, &mut multiple_output);
            presentation.select_api_key_redactors(&[short.clone(), long]);
            presentation
                .write_all(b"secret-suffix secret")
                .expect("正文应进入逻辑缓冲");
            presentation.flush().expect("多 key 输出应完成 flush");
        }
        assert_eq!(
            String::from_utf8(multiple_output.bytes).expect("输出应为 UTF-8"),
            "[REDACTED API KEY] [REDACTED API KEY]"
        );

        let mut single_output = FlushCountingOutput::default();
        {
            let mut presentation =
                StreamPresentation::new(ProcessStream::Stdout, &mut single_output);
            presentation.select_api_key_redactor(Some(short));
            presentation
                .write_all(b"before secret after")
                .expect("正文应进入逻辑缓冲");
            presentation.flush().expect("单 key 输出应完成 flush");
        }
        assert_eq!(
            String::from_utf8(single_output.bytes).expect("输出应为 UTF-8"),
            "before [REDACTED API KEY] after"
        );
    }

    #[test]
    fn project_result_is_not_flushed_again_after_its_log_has_closed() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stdout = FlushCountingOutput::default();
        let mut stderr = FlushCountingOutput::default();
        let exit;
        {
            let mut stdout_presentation =
                StreamPresentation::new(ProcessStream::Stdout, &mut stdout);
            let mut stderr_presentation =
                StreamPresentation::new(ProcessStream::Stderr, &mut stderr);
            exit = finish_process_output_state(
                ProcessOutputState::Flushed(ExitCode::SUCCESS),
                &localizer,
                &mut stdout_presentation,
                &mut stderr_presentation,
            );
        }

        assert_eq!(exit, ExitCode::SUCCESS);
        assert_eq!(stdout.flush_attempts, 0);
        assert_eq!(stderr.flush_attempts, 0);
    }

    #[test]
    fn process_output_failure_keeps_context_effect_and_flush_operation() {
        let source = io::Error::new(io::ErrorKind::BrokenPipe, "测试 flush 失败");
        for (effect, operation, code) in [
            (
                StateEffect::AppliedFinalizationFailed,
                RuntimeOperation::FlushStdout,
                "runtime.stdout_flush",
            ),
            (
                StateEffect::Unchanged,
                RuntimeOperation::FlushStderr,
                "runtime.stderr_flush",
            ),
            (
                StateEffect::ProgressPreserved,
                RuntimeOperation::WriteStderr,
                "runtime.stderr_write",
            ),
        ] {
            let report = process_output_failure_report(effect, operation, &source);
            assert_eq!(report.effect(), effect);
            assert_eq!(report.primary().code(), code);
        }
    }

    #[test]
    fn project_log_warning_write_failure_is_returned() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let source = io::Error::from_raw_os_error(5);
        let warning = ProjectLogWarning {
            project_log: vec![DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(crate::diagnostic::ObservabilityIssue::write(
                    crate::diagnostic::ObservabilityComponent::ProjectLog,
                    Some(crate::diagnostic::SafePath::new(
                        "C:\\project\\logs\\run.jsonl",
                    )),
                    None,
                    1,
                    &source,
                )),
            )],
            task_records: Vec::new(),
            presentation_failures: Vec::new(),
        };
        let mut stderr = FailingOutput::default();

        let result = render_project_log_warning_if_present(&localizer, Some(&warning), &mut stderr);

        assert!(result.is_err());
        assert!(stderr.write_attempts > 0);
    }

    #[test]
    fn post_close_diagnostic_flush_failure_is_shown_once_on_the_opposite_stream() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let report = process_output_failure_report_from_failure(
            StateEffect::AppliedFinalizationFailed,
            RuntimeOperation::StartWorker,
            IoFailure::from_error(&io::Error::other("测试 worker 启动失败")),
        );
        let mut raw_stdout = FlushCountingOutput::default();
        let mut raw_stderr = SecondFlushFailingOutput::default();
        let failed;
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut raw_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut raw_stderr);
            failed = finish_project_log_after_presentation(
                None,
                vec![report],
                StateEffect::AppliedFinalizationFailed,
                &localizer,
                &mut stdout,
                &mut stderr,
            );
        }

        assert!(failed);
        assert_eq!(raw_stderr.flush_attempts, 2, "失败的最终 flush 不得重试");
        assert_eq!(raw_stdout.flush_attempts, 2, "相反流只执行一次回退 flush");
        let stdout = String::from_utf8(raw_stdout.bytes)
            .expect("回退输出应为 UTF-8")
            .replace(['\u{2068}', '\u{2069}'], "");
        let worker_reason = localizer.format(UiMessage::DiagnosticFailureValue {
            code: "worker_spawn_failed",
        });
        assert_eq!(stdout.matches(&format!("原因：{worker_reason}")).count(), 1);
        assert_eq!(stdout.matches("原因：无法刷新标准错误").count(), 1);
        assert_eq!(stdout.matches("对象：stderr").count(), 1);
    }

    #[test]
    fn cancellation_notice_write_failure_changes_exit_code_to_failure() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stdout = Vec::new();
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let (rpg_log, rpg_log_path) = cancelled_project_log(temporary.path(), "rpg-cancel-write");
        let mut rpg_stderr = FailingOutput::default();
        let rpg_exit = render_command_report(
            CommandRunResult::Interrupted,
            None,
            Some(rpg_log),
            &localizer,
            &mut stdout,
            &mut rpg_stderr,
        );
        assert_eq!(rpg_exit, ExitCode::FAILURE);
        assert!(rpg_stderr.write_attempts > 0);
        assert_eq!(run_finished_kind(&rpg_log_path), "failed");

        let (generic_log, generic_log_path) =
            cancelled_project_log(temporary.path(), "generic-cancel-write");
        let mut generic_stderr = FailingOutput::default();
        let generic_exit = render_generic_command_result(
            GenericCommandRunResult::Interrupted,
            &[],
            Some(generic_log),
            &localizer,
            &mut stdout,
            &mut generic_stderr,
        );
        assert_eq!(generic_exit, ExitCode::FAILURE);
        assert!(generic_stderr.write_attempts > 0);
        assert_eq!(run_finished_kind(&generic_log_path), "failed");
    }

    #[test]
    fn terminal_flush_failures_are_recorded_before_both_project_logs_close() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let mut stdout = Vec::new();

        let (rpg_log, rpg_log_path) = cancelled_project_log(temporary.path(), "rpg-cancel-flush");
        let mut rpg_stderr = FlushFailingOutput::default();
        let rpg_exit = render_command_report(
            CommandRunResult::Interrupted,
            None,
            Some(rpg_log),
            &localizer,
            &mut stdout,
            &mut rpg_stderr,
        );
        assert_eq!(rpg_exit, ExitCode::FAILURE);
        assert_eq!(rpg_stderr.flush_attempts, 1);
        assert_eq!(run_finished_kind(&rpg_log_path), "failed");
        assert_eq!(run_diagnostic_count(&rpg_log_path), 1);
        assert_eq!(run_diagnostic_relations(&rpg_log_path), ["primary"]);

        let (generic_log, generic_log_path) =
            cancelled_project_log(temporary.path(), "generic-cancel-flush");
        let mut generic_stderr = FlushFailingOutput::default();
        let generic_exit = render_generic_command_result(
            GenericCommandRunResult::Interrupted,
            &[],
            Some(generic_log),
            &localizer,
            &mut stdout,
            &mut generic_stderr,
        );
        assert_eq!(generic_exit, ExitCode::FAILURE);
        assert_eq!(generic_stderr.flush_attempts, 1);
        assert_eq!(run_finished_kind(&generic_log_path), "failed");
        assert_eq!(run_diagnostic_count(&generic_log_path), 1);
        assert_eq!(run_diagnostic_relations(&generic_log_path), ["primary"]);
    }

    #[test]
    fn terminal_flush_panic_is_logged_as_unknown_before_the_catch_renders_it() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let (pending, log_path) = cancelled_project_log(temporary.path(), "rpg-flush-panic");
        let mut pending = Some(pending);
        pending
            .as_mut()
            .expect("测试项目日志必须存在")
            .prepare_for_result_presentation();
        let panic_boundary = pending
            .as_mut()
            .map(PendingProjectLog::arm_presentation_panic)
            .map(CommandPanicBoundary::from_report);
        let mut stdout = Vec::new();
        let mut stderr = FlushPanickingOutput::default();
        let run_log = RunLogPresentation::new(Some(log_path.clone()));
        let exit;
        {
            let mut stdout_presentation =
                StreamPresentation::new(ProcessStream::Stdout, &mut stdout);
            let mut stderr_presentation =
                StreamPresentation::new(ProcessStream::Stderr, &mut stderr);
            exit = catch_logged_presentation(
                panic_boundary,
                &run_log,
                &localizer,
                &mut stdout_presentation,
                &mut stderr_presentation,
                |stdout, stderr| {
                    render_command_report_with_run_log(
                        CommandRunResult::Interrupted,
                        None,
                        None,
                        pending,
                        LoggedPresentationContext {
                            run_log: &run_log,
                            had_presentation_failure: false,
                            localizer: &localizer,
                        },
                        stdout,
                        stderr,
                    )
                },
            );
        }

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(stderr.flush_attempts, 1);
        assert!(
            !stderr.bytes.is_empty(),
            "catch 必须继续呈现安全 panic 诊断"
        );
        let stderr_text =
            String::from_utf8_lossy(&stderr.bytes).replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(stderr_text.matches("运行记录：").count(), 1);
        assert_eq!(run_finished_kind(&log_path), "outcome_unknown");
        assert_eq!(run_diagnostic_count(&log_path), 1);
    }

    #[test]
    fn failed_result_write_failures_are_recorded_before_the_project_log_closes() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let mut stdout = Vec::new();

        let rpg_error = ProductionCommandError::stderr_write(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "测试 RPG stderr 已关闭",
        ));
        let (rpg_log, rpg_log_path) = failed_project_log(
            temporary.path(),
            "rpg-failed-write",
            rpg_error.failure_report().report().clone(),
        );
        let mut rpg_stderr = FailingOutput::default();
        let rpg_exit = render_command_report(
            CommandRunResult::Failed(rpg_error),
            None,
            Some(rpg_log),
            &localizer,
            &mut stdout,
            &mut rpg_stderr,
        );
        assert_eq!(rpg_exit, ExitCode::FAILURE);
        assert_eq!(run_finished_kind(&rpg_log_path), "failed");
        assert_eq!(
            run_diagnostic_count(&rpg_log_path),
            2,
            "业务错误和最终 stderr 呈现错误都必须写入项目日志"
        );
        assert_eq!(
            run_diagnostic_relations(&rpg_log_path),
            ["primary", "observability"],
            "已有业务错误时，最终呈现错误只能是 observability related"
        );

        let generic_error = GenericCommandError::Signal {
            source: io::Error::new(io::ErrorKind::BrokenPipe, "测试 signal 失败"),
            operation: None,
            state_applied: false,
        };
        let (generic_log, generic_log_path) = failed_project_log(
            temporary.path(),
            "generic-failed-write",
            generic_command_error_report(&generic_error),
        );
        let mut generic_stderr = FailingOutput::default();
        let generic_exit = render_generic_command_result(
            GenericCommandRunResult::Failed(generic_error),
            &[],
            Some(generic_log),
            &localizer,
            &mut stdout,
            &mut generic_stderr,
        );
        assert_eq!(generic_exit, ExitCode::FAILURE);
        assert_eq!(run_finished_kind(&generic_log_path), "failed");
        assert_eq!(
            run_diagnostic_count(&generic_log_path),
            2,
            "Generic 业务错误和最终 stderr 呈现错误都必须写入项目日志"
        );
        assert_eq!(
            run_diagnostic_relations(&generic_log_path),
            ["primary", "observability"],
            "Generic 已有业务错误时，最终呈现错误只能是 observability related"
        );
    }

    #[test]
    fn configuration_errors_render_public_four_fields() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        let exit = render_configuration_load_error(
            &localizer,
            &ConfigurationLoadError::InvalidToml {
                path: "settings.toml".into(),
                location: Some(super::super::config::SourceLocation::new(3, 7)),
                resource: "llm.clients.primary".to_owned(),
                failure: crate::diagnostic::ConfigurationTomlFailureKind::TypeMismatch {
                    expected: crate::diagnostic::ConfigurationTomlValueKind::Table,
                },
            },
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("对象：settings.toml"));
        assert!(plain.contains("原因：字段类型不符合要求"));
        assert!(plain.contains("字段：llm.clients.primary"));
        assert!(plain.contains("要求的类型：表"));
        assert!(plain.contains("第 3 行，第 7 列"));
        assert!(plain.contains("影响：业务状态没有修改"));
        assert!(plain.contains("处理办法：修正指出的配置字段后重试"));
        for forbidden in [
            "configuration.invalid_toml",
            "line=",
            "column=",
            "toml_failure=",
            "expected=",
        ] {
            assert!(!plain.contains(forbidden));
        }
    }

    #[test]
    fn panic_fallback_uses_selected_locale_after_cli_parsing_and_english_before_it() {
        const PANIC_BODY: &str = "PROCESS_STARTUP_PANIC_BODY_SENTINEL";
        let selected = UiLocalizer::new(UiLocale::Japanese);
        let mut selected_stdout = Vec::new();
        let mut selected_stderr = Vec::new();
        let selected_exit;
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut selected_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut selected_stderr);
            selected_exit =
                catch_after_cli_parsing(&selected, &mut stdout, &mut stderr, |_stdout, _stderr| {
                    panic!("{PANIC_BODY}")
                });
        }
        assert_eq!(selected_exit, ExitCode::FAILURE);
        let selected_stderr =
            String::from_utf8(selected_stderr).expect("已选 locale 诊断应为 UTF-8");
        assert!(
            selected_stderr.contains(&selected.format(UiMessage::DiagnosticFailureValue {
                code: "internal_invariant",
            }))
        );
        assert!(!selected_stderr.contains(PANIC_BODY));

        let mut flush_stdout = FlushPanickingOutput::default();
        let mut flush_stderr = Vec::new();
        let flush_exit;
        {
            let mut stdout = StreamPresentation::new(ProcessStream::Stdout, &mut flush_stdout);
            let mut stderr = StreamPresentation::new(ProcessStream::Stderr, &mut flush_stderr);
            flush_exit =
                catch_after_cli_parsing(&selected, &mut stdout, &mut stderr, |stdout, stderr| {
                    finalize_process_output(
                        ExitCode::SUCCESS,
                        StateEffect::Unchanged,
                        &selected,
                        stdout,
                        stderr,
                    )
                });
        }
        assert_eq!(flush_exit, ExitCode::FAILURE);
        assert_eq!(flush_stdout.flush_attempts, 1);
        let expected_flush_panic = render_diagnostic_report(
            &DiagnosticReport::new(
                StateEffect::OutcomeUnknown,
                Diagnostic::runtime(RuntimeIssue::ProcessPanicked {
                    boundary: RuntimePanicBoundary::AfterCliParsing,
                }),
            ),
            &selected,
        );
        assert!(
            String::from_utf8(flush_stderr)
                .expect("最终 flush panic 诊断应为 UTF-8")
                .contains(&expected_flush_panic),
            "CLI 解析后的 flush panic 必须使用已选 locale 和 AfterCliParsing 边界"
        );

        let english = UiLocalizer::new(UiLocale::English);
        let mut startup_stdout = Vec::new();
        let mut startup_stderr = Vec::new();
        let startup_exit = render_uncaught_panic_with(
            &english,
            RuntimePanicBoundary::ProcessStartup,
            &mut startup_stdout,
            &mut startup_stderr,
        );
        assert_eq!(startup_exit, ExitCode::FAILURE);
        let startup_stderr = String::from_utf8(startup_stderr).expect("解析前诊断应为 UTF-8");
        assert!(
            startup_stderr.contains(&english.format(UiMessage::DiagnosticFailureValue {
                code: "internal_invariant",
            }))
        );
    }

    #[test]
    fn english_configuration_value_error_uses_typed_localization() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::English);
        let error = super::super::config::invalid(
            "llm.clients.primary.max_concurrent_requests",
            crate::diagnostic::ConfigurationValueRule::RuntimeMaximumExceeded {
                actual: 2_000_000,
                maximum: 1_000_000,
            },
        );
        let mut stderr = Vec::new();
        let exit = render_configuration_load_error(
            &localizer,
            &ConfigurationLoadError::InvalidValueAtPath {
                path: "C:\\ATT\\att.toml".into(),
                source: error,
            },
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(stderr.contains("llm.clients.primary.max_concurrent_requests"));
        assert!(stderr.contains('\u{2068}') && stderr.contains('\u{2069}'));
        assert!(
            plain.contains(
                "Reason: Value exceeds runtime maximum (actual=2000000, maximum=1000000)"
            )
        );
        assert!(plain.contains("Action: Correct the named configuration field and retry"));
        for forbidden in ["configuration.invalid_value", "C:\\ATT\\att.toml"] {
            assert!(!plain.contains(forbidden));
        }
    }

    #[test]
    fn arabic_configuration_paths_are_sanitized_and_directionally_isolated() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::Arabic);
        let mut stderr = Vec::new();
        let exit = render_configuration_load_error(
            &localizer,
            &ConfigurationLoadError::NotAFile {
                path: "C:\\Games\\att\u{202e}\u{2068}\u{1b}[31m.toml".into(),
            },
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        assert!(stderr.contains("C:\\Games\\att[31m.toml"));
        assert!(stderr.contains('\u{2068}') && stderr.contains('\u{2069}'));
        assert!(!stderr.contains('\u{202e}'));
        assert!(!stderr.contains('\u{1b}'));

        let mut stderr = Vec::new();
        let exit = render_fatal(&localizer, &"UNTYPED_FATAL_SOURCE_SENTINEL", &mut stderr);
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(
            !String::from_utf8(stderr)
                .expect("诊断应为 UTF-8")
                .contains("UNTYPED_FATAL_SOURCE_SENTINEL")
        );
    }

    #[test]
    fn log_degradation_renders_readable_paths_without_internal_codes() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let source = io::Error::from_raw_os_error(5);
        let task_record = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::observability(crate::diagnostic::ObservabilityIssue::write(
                crate::diagnostic::ObservabilityComponent::TaskRecord,
                Some(crate::diagnostic::SafePath::new(
                    "C:\\project\\task-records\\run\\task-000001.md",
                )),
                None,
                1,
                &source,
            )),
        )
        .with_related(
            crate::diagnostic::RelatedFailureRelation::Cleanup,
            DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(crate::diagnostic::ObservabilityIssue::cleanup(
                    crate::diagnostic::ObservabilityComponent::TaskRecord,
                    crate::diagnostic::SafePath::new(
                        "C:\\project\\task-records\\run\\.task-000001.tmp",
                    ),
                    &source,
                )),
            ),
        );
        let warning = ProjectLogWarning {
            project_log: vec![DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::observability(crate::diagnostic::ObservabilityIssue::write(
                    crate::diagnostic::ObservabilityComponent::ProjectLog,
                    Some(crate::diagnostic::SafePath::new(
                        "C:\\project\\logs\\run.jsonl",
                    )),
                    None,
                    1,
                    &source,
                )),
            )],
            task_records: vec![task_record],
            presentation_failures: Vec::new(),
        };
        let mut stderr = Vec::new();

        render_project_log_warning(&localizer, &warning, &mut stderr).expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let warning_heading = localizer.format(UiMessage::DiagnosticWarningHeading);
        assert_eq!(stderr.matches(warning_heading.as_str()).count(), 2);
        assert!(stderr.contains("C:\\project\\logs\\run.jsonl"));
        assert!(stderr.contains("C:\\project\\task-records\\run\\task-000001.md"));
        assert!(stderr.contains("C:\\project\\task-records\\run\\.task-000001.tmp"));
        assert!(stderr.contains("操作失败"));
        for forbidden in [
            "observability.project_log.write",
            "raw_os_code=",
            "io_kind=",
            "operation=",
            "component=",
        ] {
            assert!(!stderr.contains(forbidden));
        }
    }
}
