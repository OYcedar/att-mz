//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::ffi::OsString;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::Once;

use super::arguments::AttArguments;
use super::arguments::ProgressArgument;
use super::command::{
    CommandPanicBoundary, CommandResultRenderer, CommandRunResult, PendingProjectLog,
    ProductionCommandError, ProductionRpgMakerCommandRunner, TerminationSignals,
};
use super::config::{
    ConfigurationLoadError, ConfigurationPathError, load_product_configuration,
    resolve_configuration_path,
};
use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, render_safe_diagnostic,
};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::progress::ProgressMode;
use crate::runtime::project_log::{ObservabilityWarning, ProjectLogWarning};

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
    run_from(std::env::args_os(), &mut stdout, &mut stderr)
}

fn render_uncaught_panic() -> ExitCode {
    // UI locale 尚未由完整 Clap 解析确认时，现行 CLI 契约固定使用英语兜底。
    let localizer = UiLocalizer::new(UiLocale::English);
    let mut stderr = io::stderr();
    render_uncaught_panic_with(&localizer, &mut stderr)
}

fn render_uncaught_panic_with(localizer: &UiLocalizer, stderr: &mut dyn Write) -> ExitCode {
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::ProcessStartup,
        DiagnosticSubject::Process,
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::ReportBug,
    );
    render_diagnostic_fatal(localizer, &diagnostic, stderr)
}

fn run_from<A, S>(args: A, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode
where
    A: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let (arguments, resolved_locale) = match AttArguments::try_parse_localized_from(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            if error.use_stderr() {
                let _ = write!(stderr, "{}", error.output());
            } else {
                let _ = write!(stdout, "{}", error.output());
            }
            return ExitCode::from(error.exit_code());
        }
    };
    let locale = resolved_locale.locale();
    let localizer = UiLocalizer::new(locale);
    catch_after_cli_parsing(&localizer, stderr, |stderr| {
        run_after_cli_parsing(arguments, locale, &localizer, stdout, stderr)
    })
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
            render_uncaught_panic_with(localizer, stderr)
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
    let progress_mode = match arguments.progress {
        ProgressArgument::Auto => ProgressMode::Auto,
        ProgressArgument::Plain => ProgressMode::Plain,
        ProgressArgument::Off => ProgressMode::Off,
    };
    let current_directory = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            let diagnostic = SafeDiagnostic::io(
                DiagnosticCode::ProcessCurrentDirectory,
                DiagnosticStage::ProcessStartup,
                DiagnosticSubject::Process,
                "resolve_current_directory",
                &error,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            );
            return render_diagnostic_fatal(localizer, &diagnostic, stderr);
        }
    };
    let configuration_path = match resolve_configuration_path(&arguments.config, &current_directory)
    {
        Ok(path) => path,
        Err(error) => return render_configuration_path_error(localizer, &error, stderr),
    };
    let configuration = match load_product_configuration(&configuration_path, arguments.product) {
        Ok(configuration) => configuration,
        Err(error) => return render_configuration_load_error(localizer, &error, stderr),
    };
    let runtime_parallelism = match std::thread::available_parallelism() {
        Ok(parallelism) => parallelism,
        Err(error) => {
            let diagnostic = SafeDiagnostic::io(
                DiagnosticCode::ProcessRuntimeStart,
                DiagnosticStage::ProcessStartup,
                DiagnosticSubject::component("Tokio"),
                "detect_available_parallelism",
                &error,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            );
            return render_diagnostic_fatal(localizer, &diagnostic, stderr);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(runtime_parallelism.get())
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let diagnostic = SafeDiagnostic::io(
                DiagnosticCode::ProcessRuntimeStart,
                DiagnosticStage::ProcessStartup,
                DiagnosticSubject::component("Tokio"),
                "build_runtime",
                &error,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            );
            return render_diagnostic_fatal(localizer, &diagnostic, stderr);
        }
    };

    let (layout, command) = configuration.into_parts();
    // Translate 的生产纵向切片包含完整计划、错误与收尾状态，其 async 状态机明显大于
    // Windows 主线程的默认栈。先把顶层 future 固定在堆上，避免 block_on 将整棵
    // 状态机钉在主线程栈中；这不改变 Tokio 内部任务的调度和并发关系。
    let command_run = Box::pin(async move {
        let mut termination_signals = TerminationSignals::new();
        let report = ProductionRpgMakerCommandRunner::new(layout, locale, progress_mode)
            .run(command, &mut termination_signals)
            .await;
        (report, termination_signals)
    });
    let (report, _termination_signals) = runtime.block_on(command_run);
    // 信号订阅与 Runtime 保持到最终结果输出结束；各业务根已经在 report 返回前显式 shutdown。

    let mut pending_project_log = report.pending_project_log;
    let panic_boundary = pending_project_log
        .as_mut()
        .map(PendingProjectLog::arm_presentation_panic);
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
        (CommandRunResult::Succeeded(output), None) => {
            if let Err(error) = CommandResultRenderer::render_success(output, localizer, stdout) {
                let command_error = ProductionCommandError::stdout_write(error);
                let warning = pending_project_log
                    .and_then(|project_log| project_log.finish_with_failure(&command_error));
                render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
                let _ = CommandResultRenderer::render_failure(
                    Some(&command_error),
                    None,
                    localizer,
                    stderr,
                );
                ExitCode::FAILURE
            } else {
                let warning = pending_project_log.and_then(PendingProjectLog::finish);
                render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
                ExitCode::SUCCESS
            }
        }
        (CommandRunResult::Failed(command_error), shutdown) => {
            let warning = pending_project_log.and_then(PendingProjectLog::finish);
            render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            let _ = CommandResultRenderer::render_failure(
                Some(&command_error),
                shutdown.as_ref(),
                localizer,
                stderr,
            );
            ExitCode::FAILURE
        }
        (CommandRunResult::Interrupted, None) => {
            let warning = pending_project_log.and_then(PendingProjectLog::finish);
            render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            let _ = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled));
            ExitCode::from(130)
        }
        (CommandRunResult::Interrupted, Some(shutdown)) => {
            let warning = pending_project_log.and_then(PendingProjectLog::finish);
            render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            // 取消事实与清理失败并列呈现，清理错误不吞掉“已取消”这一终态。
            let _ = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled));
            let _ = CommandResultRenderer::render_failure(None, Some(&shutdown), localizer, stderr);
            ExitCode::FAILURE
        }
        (CommandRunResult::Succeeded(output), Some(shutdown)) => {
            let warning = pending_project_log.and_then(PendingProjectLog::finish);
            render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
            // 业务结果已生效：先完整呈现成功输出，再报告收尾失败，
            // 清理错误不得覆盖业务成功事实。
            let _ = CommandResultRenderer::render_success(output, localizer, stdout);
            let _ = CommandResultRenderer::render_applied_finalization_failure(
                &shutdown, localizer, stderr,
            );
            ExitCode::FAILURE
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
            let _ = CommandResultRenderer::render_failure(Some(&error), None, localizer, stderr);
            ExitCode::FAILURE
        }
    }
}

fn render_diagnostic_fatal(
    localizer: &UiLocalizer,
    diagnostic: &SafeDiagnostic,
    stderr: &mut dyn Write,
) -> ExitCode {
    let _ = render_safe_diagnostic(diagnostic, localizer, stderr);
    ExitCode::FAILURE
}

fn render_project_log_warning(
    localizer: &UiLocalizer,
    warning: &ProjectLogWarning,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    if let Some(project_log) = &warning.project_log {
        render_observability_warning(localizer, UiMessage::NoticeLogDegraded, project_log, stderr)?;
    }
    if let Some(task_records) = &warning.task_records {
        render_observability_warning(
            localizer,
            UiMessage::NoticeTaskRecordsDegraded,
            task_records,
            stderr,
        )?;
    }
    Ok(())
}

fn render_observability_warning(
    localizer: &UiLocalizer,
    banner: UiMessage<'_>,
    warning: &ObservabilityWarning,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    writeln!(stderr, "{}", localizer.format(banner))?;
    if let Some(diagnostic) = &warning.diagnostic {
        render_safe_diagnostic(diagnostic, localizer, stderr)?;
    }
    for diagnostic in &warning.related_diagnostics {
        render_safe_diagnostic(diagnostic, localizer, stderr)?;
    }
    Ok(())
}

fn render_project_log_warning_if_present(
    localizer: &UiLocalizer,
    warning: Option<&ProjectLogWarning>,
    stderr: &mut dyn Write,
) {
    if let Some(warning) = warning {
        let _ = render_project_log_warning(localizer, warning, stderr);
    }
}

#[cfg(test)]
fn render_fatal(
    localizer: &UiLocalizer,
    _error: &dyn std::fmt::Display,
    stderr: &mut dyn Write,
) -> ExitCode {
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::ProcessStartup,
        DiagnosticSubject::Process,
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::ReportBug,
    );
    render_diagnostic_fatal(localizer, &diagnostic, stderr)
}

fn render_configuration_path_error(
    localizer: &UiLocalizer,
    error: &ConfigurationPathError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let diagnostic = match error {
        ConfigurationPathError::CurrentDirectoryNotAbsolute(path) => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationPath,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        ConfigurationPathError::EmptyExplicitPath => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationPath,
            DiagnosticStage::Configuration,
            DiagnosticSubject::field("--config"),
            DiagnosticReason::failure(DiagnosticFailureKind::MissingRequiredValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
    };
    render_diagnostic_fatal(localizer, &diagnostic, stderr)
}

fn render_configuration_load_error(
    localizer: &UiLocalizer,
    error: &ConfigurationLoadError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let diagnostic = error.safe_diagnostic();
    render_diagnostic_fatal(localizer, &diagnostic, stderr)
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::application::command::{RpgMakerCommandOutput, ShutdownFailures};
    use crate::diagnostic::SafeDiagnosticSource;

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

    impl SafeDiagnosticSource for TestShutdownError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("test shutdown root"),
                DiagnosticReason::failure(DiagnosticFailureKind::FinalizationFailed),
                impact,
                fallback_action,
            )
        }
    }

    struct PanickingOutput;

    impl Write for PanickingOutput {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            std::panic::panic_any(Box::new("PRESENTATION_PANIC_BODY_SENTINEL"));
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
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
                .contains("shutdown.component")
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
    fn logged_presentation_panic_reports_the_same_safe_projection_to_cli_and_jsonl() {
        use crate::diagnostic::RecoveryFact;
        use crate::runtime::performance::RunPerformanceCounters;
        use crate::runtime::project_log::{
            ProjectLog, ProjectLogCode, ProjectLogContext, ProjectLogEvent, ProjectLogLevel,
            ProjectLogPayload, start_project_log,
        };
        use std::sync::Arc;

        const PANIC_BODY: &str = "PRESENTATION_PANIC_BODY_SENTINEL";
        let directory = tempfile::tempdir().expect("临时日志目录应可建立");
        let project_workspace = directory.path().join("rpg_maker_mz").join("project");
        let logs_root = project_workspace.join("logs");
        let run_id = "550e8400-e29b-41d4-a716-446655440098";
        let mut runtime = start_project_log(logs_root, run_id.to_owned());
        let log_path = runtime.path().expect("真实日志应有路径").to_path_buf();
        let logger = runtime.logger();
        let context = ProjectLogContext::new("zh-Hans")
            .with_engine("rpg_maker_mz")
            .with_project("project")
            .with_command("write-back");
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::ProcessOutput,
            DiagnosticSubject::command("write-back"),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::ReportBug,
        )
        .with_recovery(RecoveryFact::path(&project_workspace))
        .with_recovery(RecoveryFact::path(&log_path));
        runtime.arm_unfinished_terminal(
            context.clone(),
            vec![diagnostic.clone()],
            Arc::new(RunPerformanceCounters::default()),
        );
        logger.emit(ProjectLogEvent::new(
            ProjectLogLevel::Info,
            ProjectLogCode::RunStarted,
            context,
            ProjectLogPayload::Run { outcome: None },
        ));
        let panic_boundary = CommandPanicBoundary::from_logged(vec![diagnostic.clone()], logger);
        let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        let mut stdout = PanickingOutput;

        let exit = catch_logged_presentation(
            Some(panic_boundary),
            &localizer,
            &mut stderr,
            move |_stderr| {
                let _runtime = runtime;
                let _ = stdout.write_all(b"result");
                ExitCode::SUCCESS
            },
        );

        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("panic 诊断应为 UTF-8");
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("internal.operation"));
        assert!(plain.contains("进程输出"));
        assert!(plain.contains("write-back"));
        assert!(plain.contains(&project_workspace.to_string_lossy().to_string()));
        assert!(plain.contains(&log_path.to_string_lossy().to_string()));
        assert!(!stderr.contains(PANIC_BODY));

        let raw = std::fs::read_to_string(&log_path).expect("panic 项目日志应可读取");
        assert!(!raw.contains(PANIC_BODY));
        let records = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行应为 JSON"))
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .map(|record| record["code"].as_str().expect("日志 code 应为文本"))
                .collect::<Vec<_>>(),
            [
                "run.started",
                "performance.counters",
                "failure.reported",
                "run.finished",
            ]
        );
        assert_eq!(
            records[2]["payload"]["diagnostic"],
            serde_json::to_value(diagnostic).expect("安全诊断应可序列化")
        );
        assert_eq!(records[3]["payload"]["outcome"], "outcome_unknown");
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
    fn help_and_parse_errors_do_not_require_configuration() {
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
        assert_eq!(exit, ExitCode::from(2));
        assert!(!stderr.is_empty());

        stdout.clear();
        stderr.clear();
        let exit = run_from(
            ["att", "mz", "write-back", "--name", "demo"],
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::from(2));
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8(stderr.clone())
                .expect("诊断应为 UTF-8")
                .contains("--config"),
            "缺少配置路径应由 clap 呈现缺参错误"
        );
    }

    #[test]
    fn configuration_errors_render_the_typed_safe_reason_without_using_display() {
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
        assert!(plain.starts_with("错误 [configuration.invalid_toml]"));
        assert!(stderr.contains("settings.toml"));
        assert!(plain.contains("TOML 第 3 行、第 7 列无效"));
        assert!(stderr.contains("llm.clients.primary"));
        let expected_kind =
            localizer.format(UiMessage::DiagnosticTomlExpectedKindValue { code: "table" });
        let expected_failure = localizer.format(UiMessage::DiagnosticTomlFailureValue {
            code: "type_mismatch",
            expected: &expected_kind,
        });
        let expected_failure = expected_failure.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains(&expected_failure));
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
            selected_stderr.contains(&selected.format(UiMessage::DiagnosticTitle {
                code: DiagnosticCode::InternalOperation.as_str(),
            }))
        );
        assert!(!selected_stderr.contains(PANIC_BODY));

        let english = UiLocalizer::new(UiLocale::English);
        let mut startup_stderr = Vec::new();
        let startup_exit = render_uncaught_panic_with(&english, &mut startup_stderr);
        assert_eq!(startup_exit, ExitCode::FAILURE);
        let startup_stderr = String::from_utf8(startup_stderr).expect("解析前诊断应为 UTF-8");
        assert!(
            startup_stderr.contains(&english.format(UiMessage::DiagnosticTitle {
                code: DiagnosticCode::InternalOperation.as_str(),
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
        assert!(plain.starts_with("Error [configuration.invalid_value]"));
        assert!(stderr.contains("llm.clients.primary.max_concurrent_requests"));
        assert!(stderr.contains("C:\\ATT\\att.toml"));
        assert!(stderr.contains('\u{2068}') && stderr.contains('\u{2069}'));
        assert!(stderr.contains("actual=2000000"));
        assert!(stderr.contains("maximum=1000000"));
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
    fn log_degradation_renders_the_safe_operation_path_and_os_code() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let source = io::Error::from_raw_os_error(5);
        let warning = ProjectLogWarning {
            project_log: Some(ObservabilityWarning {
                diagnostic: Some(SafeDiagnostic::io(
                    DiagnosticCode::LogWrite,
                    DiagnosticStage::Logging,
                    DiagnosticSubject::path("C:\\project\\logs\\run.jsonl"),
                    "write_all",
                    &source,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )),
                related_diagnostics: Vec::new(),
            }),
            task_records: Some(ObservabilityWarning {
                diagnostic: Some(SafeDiagnostic::io(
                    DiagnosticCode::FileSystemOperation,
                    DiagnosticStage::Logging,
                    DiagnosticSubject::path("C:\\project\\task-records\\run\\task-000001.md"),
                    "persist_task_record",
                    &source,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )),
                related_diagnostics: vec![SafeDiagnostic::io(
                    DiagnosticCode::FileSystemOperation,
                    DiagnosticStage::Logging,
                    DiagnosticSubject::path("C:\\project\\task-records\\run\\.task-000001.tmp"),
                    "cleanup_temporary_file",
                    &source,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )],
            }),
        };
        let mut stderr = Vec::new();

        render_project_log_warning(&localizer, &warning, &mut stderr).expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        let project_log_banner = localizer.format(UiMessage::NoticeLogDegraded);
        let task_record_banner = localizer.format(UiMessage::NoticeTaskRecordsDegraded);
        assert_eq!(stderr.matches(project_log_banner.as_str()).count(), 1);
        assert_eq!(stderr.matches(task_record_banner.as_str()).count(), 1);
        assert!(stderr.contains("log.write"));
        assert!(stderr.contains("C:\\project\\logs\\run.jsonl"));
        assert!(stderr.contains("write_all"));
        assert!(stderr.contains("C:\\project\\task-records\\run\\task-000001.md"));
        assert!(stderr.contains("persist_task_record"));
        assert!(stderr.contains("C:\\project\\task-records\\run\\.task-000001.tmp"));
        assert!(stderr.contains("cleanup_temporary_file"));
        assert!(stderr.contains("OS 5"));
    }
}
