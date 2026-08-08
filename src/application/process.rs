//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::ffi::OsString;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::Once;

use windows_sys::Win32::Globalization::{CP_UTF8, GetACP};

use super::arguments::AttArguments;
use super::command::{
    CommandPanicBoundary, CommandResultRenderer, CommandRunResult, ProductionCommandError,
    ProductionCommandRunReport, ProductionRpgMakerCommandRunner, TerminationSignals,
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
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation,
    RuntimePanicBoundary, StateEffect, render_diagnostic_report,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::manual::{render_manual_command_error, render_manual_command_summary};

enum ProductCommandRunReport {
    RpgMaker(ProductionCommandRunReport),
    Generic(GenericCommandRunReport),
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
        return finalize_process_output(exit, &mut stdout, &mut stderr);
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
    let mut stderr = io::stderr();
    render_uncaught_panic_with(
        &localizer,
        RuntimePanicBoundary::ProcessStartup,
        &mut stderr,
    )
}

fn render_uncaught_panic_with(
    localizer: &UiLocalizer,
    boundary: RuntimePanicBoundary,
    stderr: &mut dyn Write,
) -> ExitCode {
    let diagnostic = DiagnosticReport::new(
        StateEffect::OutcomeUnknown,
        Diagnostic::runtime(RuntimeIssue::ProcessPanicked { boundary }),
    );
    render_diagnostic_report_fatal(localizer, &diagnostic, stderr)
}

fn run_from<A, S>(args: A, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode
where
    A: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let (arguments, resolved_locale) = match AttArguments::try_parse_localized_from(args) {
        Ok(parsed) => parsed,
        Err(error) => {
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
            return finalize_process_output(exit, stdout, stderr);
        }
    };
    let locale = resolved_locale.locale();
    let localizer = UiLocalizer::new(locale);
    catch_after_cli_parsing(&localizer, stderr, |stderr| {
        let exit = run_after_cli_parsing(arguments, locale, &localizer, stdout, stderr);
        // 已选定 locale 后的最终 flush 也属于 after-CLI 呈现边界；panic 不能退化成
        // 英语的 ProcessStartup，Err 则必须把进程结果改为失败。
        finalize_process_output(exit, stdout, stderr)
    })
}

fn finalize_process_output(
    exit: ExitCode,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    // 两个流都必须尝试刷新，不能让第一个失败阻止另一个流完成收尾。
    let stdout_flush = stdout.flush();
    let stderr_flush = stderr.flush();
    if stdout_flush.is_err() || stderr_flush.is_err() {
        ExitCode::FAILURE
    } else {
        exit
    }
}

fn catch_after_cli_parsing(
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
    operation: impl FnOnce(&mut dyn Write) -> ExitCode,
) -> ExitCode {
    match catch_unwind(AssertUnwindSafe(|| operation(stderr))) {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            // 与命令 panic 边界一致，payload 只触发控制流，绝不读取或格式化。
            drop(payload);
            render_uncaught_panic_with(localizer, RuntimePanicBoundary::AfterCliParsing, stderr)
        }
    }
}

fn run_after_cli_parsing(
    arguments: AttArguments,
    locale: UiLocale,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let distribution = match DistributionLayout::from_current_executable() {
        Ok(distribution) => distribution,
        Err(error) => return render_distribution_layout_error(localizer, &error, stderr),
    };
    let configuration = match load_product_configuration(&distribution, arguments.product) {
        Ok(configuration) => configuration,
        Err(error) => return render_configuration_load_error(localizer, &error, stderr),
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
            return render_diagnostic_report_fatal(localizer, &diagnostic, stderr);
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
            return render_diagnostic_report_fatal(localizer, &diagnostic, stderr);
        }
    };

    // Translate 的生产纵向切片包含完整计划、错误与收尾状态，其 async 状态机明显大于
    // Windows 主线程的默认栈。先把顶层 future 固定在堆上，避免 block_on 将整棵
    // 状态机钉在主线程栈中；这不改变 Tokio 内部任务的调度和并发关系。
    let command_run = Box::pin(async move {
        let mut termination_signals = TerminationSignals::new();
        let report = match configuration {
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
        ProductCommandRunReport::RpgMaker(report) => report,
        ProductCommandRunReport::Generic(report) => {
            return render_generic_command_report(report, localizer, stdout, stderr);
        }
    };
    let mut pending_project_log = report.pending_project_log;
    if let Some(project_log) = pending_project_log.as_mut() {
        project_log.prepare_for_result_presentation();
    }
    let panic_boundary = pending_project_log
        .as_mut()
        .map(PendingProjectLog::arm_presentation_panic)
        .map(CommandPanicBoundary::from_report);
    catch_logged_presentation(panic_boundary, localizer, stderr, |stderr| {
        render_command_report(
            report.result,
            report.shutdown_error,
            pending_project_log,
            localizer,
            stdout,
            stderr,
        )
    })
}

fn render_command_report(
    result: CommandRunResult,
    shutdown_error: Option<super::command::ShutdownFailures>,
    pending_project_log: Option<PendingProjectLog>,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
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
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                let (warning, _) = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    stdout,
                    stderr,
                );
                let warning_failed =
                    render_project_log_warning_if_present(localizer, warning.as_ref(), stderr)
                        .is_err();
                if warning_failed {
                    return ExitCode::FAILURE;
                }
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
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                let (warning, _) = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    stdout,
                    stderr,
                );
                let warning_failed =
                    render_project_log_warning_if_present(localizer, warning.as_ref(), stderr)
                        .is_err();
                if warning_failed {
                    return ExitCode::FAILURE;
                }
                ExitCode::FAILURE
            } else {
                let mut presentation_failures = Vec::new();
                if let Some(shutdown) = shutdown.as_ref()
                    && let Err(error) = CommandResultRenderer::render_applied_finalization_failure(
                        shutdown, localizer, stderr,
                    )
                {
                    presentation_failures.push(process_output_failure_report(
                        RuntimeOperation::WriteStderr,
                        &error,
                    ));
                }
                let (warning, had_presentation_failure) = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    stdout,
                    stderr,
                );
                let warning_presentation_failed = warning
                    .as_ref()
                    .is_some_and(|warning| !warning.presentation_failures.is_empty());
                if render_project_log_warning_if_present(localizer, warning.as_ref(), stderr)
                    .is_err()
                    || warning_presentation_failed
                    || had_presentation_failure
                {
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
            let mut presentation_failures = Vec::new();
            if let Err(error) = CommandResultRenderer::render_failure(
                Some(&command_error),
                shutdown.as_ref(),
                localizer,
                stderr,
            ) {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            let (warning, _) = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                stdout,
                stderr,
            );
            let warning_failed =
                render_project_log_warning_if_present(localizer, warning.as_ref(), stderr).is_err();
            if warning_failed {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
        (CommandRunResult::Interrupted, None) => {
            let mut presentation_failures = Vec::new();
            if let Err(error) = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))
            {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            let (warning, cancellation_failed) = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                stdout,
                stderr,
            );
            let warning_presentation_failed = warning
                .as_ref()
                .is_some_and(|warning| !warning.presentation_failures.is_empty());
            let warning_result =
                render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            if warning_result.is_err() || cancellation_failed || warning_presentation_failed {
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
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            if let Err(error) =
                CommandResultRenderer::render_failure(None, Some(&shutdown), localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &error,
                ));
            }
            let (warning, presentation_failed) = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                stdout,
                stderr,
            );
            // 取消事实与清理失败并列呈现，清理错误不吞掉“已取消”这一终态。
            let warning_failed =
                render_project_log_warning_if_present(localizer, warning.as_ref(), stderr).is_err();
            if warning_failed || presentation_failed {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

fn finish_project_log_after_presentation(
    pending_project_log: Option<PendingProjectLog>,
    mut presentation_failures: Vec<DiagnosticReport>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> (Option<ProjectLogWarning>, bool) {
    if let Err(source) = stdout.flush() {
        presentation_failures.push(process_output_failure_report(
            RuntimeOperation::WriteStdout,
            &source,
        ));
    }
    if let Err(source) = stderr.flush() {
        presentation_failures.push(process_output_failure_report(
            RuntimeOperation::WriteStderr,
            &source,
        ));
    }
    let presentation_failed = !presentation_failures.is_empty();
    let warning = pending_project_log.and_then(|project_log| {
        if presentation_failures.is_empty() {
            project_log.finish()
        } else {
            project_log.finish_with_diagnostics(presentation_failures)
        }
    });
    (warning, presentation_failed)
}

fn process_output_failure_report(
    operation: RuntimeOperation,
    source: &io::Error,
) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::AppliedFinalizationFailed,
        Diagnostic::runtime(RuntimeIssue::Io {
            component: RuntimeComponent::Process,
            operation,
            failure: IoFailure::from_error(source),
        }),
    )
}

fn render_generic_command_report(
    report: GenericCommandRunReport,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let GenericCommandRunReport {
        result,
        shutdown_errors,
        mut pending_project_log,
    } = report;
    if let Some(project_log) = pending_project_log.as_mut() {
        project_log.prepare_for_result_presentation();
    }
    let panic_report = pending_project_log
        .as_mut()
        .map(PendingProjectLog::arm_presentation_panic);
    catch_generic_logged_presentation(panic_report, localizer, stderr, |stderr| {
        render_generic_command_result(
            result,
            &shutdown_errors,
            pending_project_log,
            localizer,
            stdout,
            stderr,
        )
    })
}

fn render_generic_command_result(
    result: GenericCommandRunResult,
    shutdown_errors: &[GenericShutdownError],
    pending_project_log: Option<PendingProjectLog>,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    match result {
        GenericCommandRunResult::Succeeded(output) => {
            if let Err(source) = render_generic_output(output, localizer, stdout) {
                let diagnostic =
                    process_output_failure_report(RuntimeOperation::WriteStdout, &source);
                let mut presentation_failures = vec![diagnostic.clone()];
                if let Err(source) = writeln!(
                    stderr,
                    "{}",
                    render_diagnostic_report(&diagnostic, localizer)
                ) {
                    presentation_failures.push(process_output_failure_report(
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                if let Err(source) =
                    render_generic_shutdown_errors(shutdown_errors, localizer, stderr)
                {
                    presentation_failures.push(process_output_failure_report(
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                let (warning, _) = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    stdout,
                    stderr,
                );
                let warning_failed = render_generic_project_log_warning_if_present(
                    localizer,
                    warning.as_ref(),
                    stderr,
                )
                .is_err();
                if warning_failed {
                    return ExitCode::FAILURE;
                }
                ExitCode::FAILURE
            } else {
                let mut presentation_failures = Vec::new();
                if let Err(source) =
                    render_generic_shutdown_errors(shutdown_errors, localizer, stderr)
                {
                    presentation_failures.push(process_output_failure_report(
                        RuntimeOperation::WriteStderr,
                        &source,
                    ));
                }
                let (warning, had_presentation_failure) = finish_project_log_after_presentation(
                    pending_project_log,
                    presentation_failures,
                    stdout,
                    stderr,
                );
                let warning_presentation_failed = warning
                    .as_ref()
                    .is_some_and(|warning| !warning.presentation_failures.is_empty());
                if render_generic_project_log_warning_if_present(
                    localizer,
                    warning.as_ref(),
                    stderr,
                )
                .is_err()
                    || warning_presentation_failed
                    || had_presentation_failure
                {
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
            let mut presentation_failures = Vec::new();
            let diagnostic_result = if let Some(manual) = error.manual_error() {
                render_manual_command_error(manual, localizer, stderr)
            } else {
                let diagnostic = generic_command_error_report(&error);
                writeln!(
                    stderr,
                    "{}",
                    render_diagnostic_report(&diagnostic, localizer)
                )
            };
            if let Err(source) = diagnostic_result {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if let Err(source) = render_generic_shutdown_errors(shutdown_errors, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            let (warning, _) = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                stdout,
                stderr,
            );
            let warning_failed =
                render_generic_project_log_warning_if_present(localizer, warning.as_ref(), stderr)
                    .is_err();
            if warning_failed {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
        GenericCommandRunResult::Interrupted => {
            let mut presentation_failures = Vec::new();
            if let Err(source) =
                writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled))
            {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            if let Err(source) = render_generic_shutdown_errors(shutdown_errors, localizer, stderr)
            {
                presentation_failures.push(process_output_failure_report(
                    RuntimeOperation::WriteStderr,
                    &source,
                ));
            }
            let (warning, cancellation_failed) = finish_project_log_after_presentation(
                pending_project_log,
                presentation_failures,
                stdout,
                stderr,
            );
            let warning_presentation_failed = warning
                .as_ref()
                .is_some_and(|warning| !warning.presentation_failures.is_empty());
            let warning_result =
                render_generic_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            if warning_result.is_err() || cancellation_failed || warning_presentation_failed {
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
    output: GenericCommandOutput,
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
                        files: count(files),
                        groups: count(groups),
                        units: count(units),
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
                        files: count(files),
                        groups: count(groups),
                        units: count(units),
                        preserved: count(preserved_translations),
                        cleared: count(cleared_translations),
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
                    profile: &profile_id,
                })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultGenericTranslateSummary {
                    total: count(summary.total_tasks),
                    complete: count(summary.complete_tasks),
                    partial: count(summary.partial_tasks),
                    unavailable: count(summary.unavailable_tasks),
                    cleared: count(summary.cleared_units),
                    reused: count(summary.reused_units),
                    accepted: count(summary.accepted_units),
                    written: count(summary.written_units),
                    conflicted: count(summary.conflicted_units),
                    problems: count(summary.response_problems),
                })
            )?;
            if summary.total_tasks == 0 {
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
            symbol_repair_attempted_units,
            symbol_repair_repaired_units,
            symbol_repair_skipped_units,
            symbol_repair_replacements,
        } => {
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
                localizer.format(UiMessage::ResultOutputDirectory {
                    path: &output_root.to_string_lossy(),
                })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultGenericWriteBackSummary {
                    translated: count(translated_units),
                    original: count(retained_source_units),
                })
            )?;
            writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultSymbolRepairSummary {
                    attempted: count(symbol_repair_attempted_units),
                    repaired: count(symbol_repair_repaired_units),
                    skipped: count(symbol_repair_skipped_units),
                    replacements: count(symbol_repair_replacements),
                })
            )
        }
        GenericCommandOutput::Manual { summary } => {
            render_manual_command_summary(&summary, localizer, stdout)
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

fn render_generic_shutdown_errors(
    errors: &[GenericShutdownError],
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    for error in errors {
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
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
    presentation: impl FnOnce(&mut dyn Write) -> ExitCode,
) -> ExitCode {
    match catch_unwind(AssertUnwindSafe(|| presentation(stderr))) {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            let Some(report) = panic_report else {
                std::panic::resume_unwind(payload);
            };
            drop(payload);
            render_diagnostic_report_fatal(localizer, &report, stderr)
        }
    }
}

fn catch_logged_presentation(
    panic_boundary: Option<CommandPanicBoundary>,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
    presentation: impl FnOnce(&mut dyn Write) -> ExitCode,
) -> ExitCode {
    let result = catch_unwind(AssertUnwindSafe(|| presentation(stderr)));
    match result {
        Ok(exit_code) => exit_code,
        Err(payload) => {
            let Some(panic_boundary) = panic_boundary else {
                std::panic::resume_unwind(payload);
            };
            // 与命令边界相同，payload 只负责触发控制流，绝不读取或格式化。
            drop(payload);
            let error = panic_boundary.panic_error();
            if CommandResultRenderer::render_failure(Some(&error), None, localizer, stderr).is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

fn render_diagnostic_report_fatal(
    localizer: &UiLocalizer,
    report: &DiagnosticReport,
    stderr: &mut dyn Write,
) -> ExitCode {
    if writeln!(stderr, "{}", render_diagnostic_report(report, localizer)).is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::FAILURE
}

fn render_project_log_warning(
    localizer: &UiLocalizer,
    warning: &ProjectLogWarning,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    // 日志降级后的诊断必须给出实际 JSONL 路径，调用者才能检查已保留的证据；路径
    // 来自本地项目工作区，不从错误正文或外部响应拼接。
    if let Some(path) = &warning.log_path {
        writeln!(stderr, "project_log_path={}", path.display())?;
    }
    if !warning.project_log.is_empty() {
        writeln!(stderr, "{}", localizer.format(UiMessage::NoticeLogDegraded))?;
        for report in &warning.project_log {
            writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
        }
    }
    if !warning.task_records.is_empty() {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::NoticeTaskRecordsDegraded)
        )?;
        for report in &warning.task_records {
            writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
        }
    }
    for report in &warning.presentation_failures {
        writeln!(stderr, "{}", render_diagnostic_report(report, localizer))?;
    }
    Ok(())
}

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

fn render_generic_project_log_warning_if_present(
    localizer: &UiLocalizer,
    warning: Option<&ProjectLogWarning>,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    render_project_log_warning_if_present(localizer, warning, stderr)
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

    use super::*;
    use crate::application::command::{RpgMakerCommandOutput, ShutdownFailures};
    use crate::application::config::CommonCommandConfiguration;
    use crate::application::generic_command::GenericCommandError;
    use crate::application::project_log::{CommandLogStart, start_command_log};
    use crate::diagnostic::DiagnosticStage;
    use crate::rpg_maker::extract::{
        ExtractOutput, RulesCommandNonStringType, RulesCommandNonStringWarning,
    };
    use crate::runtime::performance::RunPerformanceCounters;
    use crate::runtime::project_log::{
        ProjectLogCommand, ProjectLogEngine, RunPlanValueSource as ProjectLogValueSource,
    };

    fn cancelled_project_log(root: &Path, project: &str) -> (PendingProjectLog, PathBuf) {
        let common = CommonCommandConfiguration::for_test(root);
        fs::create_dir_all(root.join("generic").join(project)).expect("应建立项目工作区");
        let active = start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::SimplifiedChinese,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Lua,
            performance: Arc::new(RunPerformanceCounters::default()),
        });
        let run_id = active.run_id().expect("项目日志必须取得 RunId").to_owned();
        let path = root
            .join("generic")
            .join(project)
            .join("logs")
            .join(format!("{run_id}.jsonl"));
        (active.pending_cancelled(), path)
    }

    fn failed_project_log(
        root: &Path,
        project: &str,
        report: DiagnosticReport,
    ) -> (PendingProjectLog, PathBuf) {
        let common = CommonCommandConfiguration::for_test(root);
        fs::create_dir_all(root.join("generic").join(project)).expect("应建立项目工作区");
        let active = start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::SimplifiedChinese,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Lua,
            performance: Arc::new(RunPerformanceCounters::default()),
        });
        let run_id = active.run_id().expect("项目日志必须取得 RunId").to_owned();
        let path = root
            .join("generic")
            .join(project)
            .join("logs")
            .join(format!("{run_id}.jsonl"));
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
            disabled_owners: Vec::new(),
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
        assert!(plain_stderr.contains("Rules 规则 2"));
        assert!(plain_stderr.contains("Map001.json"));
        assert!(plain_stderr.contains("code=355"));
        assert!(plain_stderr.contains("parameter=0"));
        assert!(plain_stderr.contains("类型 number"));
        assert!(plain_stderr.contains("3 个"));
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
            disabled_owners: Vec::new(),
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
                code: "operation_failed",
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
        assert!(plain.contains("相关错误 1"));
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
        let mut stderr = Vec::new();

        let exit = catch_logged_presentation(Some(boundary), &localizer, &mut stderr, |_stderr| {
            std::panic::panic_any(PANIC_BODY)
        });

        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("panic 诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("内部不变量被破坏"));
        assert!(plain.contains(&project_workspace.to_string_lossy().to_string()));
        assert!(!plain.contains(&log_path.to_string_lossy().to_string()));
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
    fn project_log_warning_write_failure_is_returned() {
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let source = io::Error::from_raw_os_error(5);
        let warning = ProjectLogWarning {
            log_path: Some("C:\\project\\logs\\run.jsonl".into()),
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

        let exit = catch_logged_presentation(panic_boundary, &localizer, &mut stderr, |stderr| {
            render_command_report(
                CommandRunResult::Interrupted,
                None,
                pending,
                &localizer,
                &mut stdout,
                stderr,
            )
        });

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(stderr.flush_attempts, 1);
        assert!(
            !stderr.bytes.is_empty(),
            "catch 必须继续呈现安全 panic 诊断"
        );
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
    }

    #[test]
    fn configuration_errors_render_only_object_reason_and_help() {
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
        assert!(plain.contains("位置：settings.toml"));
        assert!(plain.contains("原因：值的语法无效"));
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
        let mut selected_stderr = Vec::new();
        let selected_exit = catch_after_cli_parsing(&selected, &mut selected_stderr, |_stderr| {
            panic!("{PANIC_BODY}")
        });
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
        let flush_exit = catch_after_cli_parsing(&selected, &mut flush_stderr, |stderr| {
            finalize_process_output(ExitCode::SUCCESS, &mut flush_stdout, stderr)
        });
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
        let mut startup_stderr = Vec::new();
        let startup_exit = render_uncaught_panic_with(
            &english,
            RuntimePanicBoundary::ProcessStartup,
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
            log_path: Some("C:\\project\\logs\\run.jsonl".into()),
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
        let project_log_banner = localizer.format(UiMessage::NoticeLogDegraded);
        let task_record_banner = localizer.format(UiMessage::NoticeTaskRecordsDegraded);
        assert_eq!(stderr.matches(project_log_banner.as_str()).count(), 1);
        assert_eq!(stderr.matches(task_record_banner.as_str()).count(), 1);
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
