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
use crate::runtime::project_log::ProjectLogWarning;

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
    let localizer = UiLocalizer::new(UiLocale::SimplifiedChinese);
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::ProcessStartup,
        DiagnosticSubject::Process,
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::ReportBug,
    );
    let mut stderr = io::stderr();
    render_diagnostic_fatal(&localizer, &diagnostic, &mut stderr)
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
            return render_diagnostic_fatal(&localizer, &diagnostic, stderr);
        }
    };
    let configuration_path = match resolve_configuration_path(&arguments.config, &current_directory)
    {
        Ok(path) => path,
        Err(error) => return render_configuration_path_error(&localizer, &error, stderr),
    };
    let configuration = match load_product_configuration(&configuration_path, arguments.product) {
        Ok(configuration) => configuration,
        Err(error) => return render_configuration_load_error(&localizer, &error, stderr),
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
            return render_diagnostic_fatal(&localizer, &diagnostic, stderr);
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
            return render_diagnostic_fatal(&localizer, &diagnostic, stderr);
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
    catch_logged_presentation(panic_boundary, &localizer, stderr, |stderr| {
        render_command_report(
            report.result,
            report.shutdown_error,
            pending_project_log,
            &localizer,
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
            let _ = CommandResultRenderer::render_failure(None, Some(&shutdown), localizer, stderr);
            ExitCode::FAILURE
        }
        (CommandRunResult::Succeeded(_), Some(shutdown)) => {
            let warning = pending_project_log.and_then(PendingProjectLog::finish);
            render_project_log_warning_if_present(localizer, warning.as_ref(), stderr);
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
    writeln!(stderr, "{}", localizer.format(UiMessage::NoticeLogDegraded))?;
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
    let diagnostic = match error {
        ConfigurationLoadError::Open { path, source } => SafeDiagnostic::io(
            DiagnosticCode::ConfigurationOpen,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            "open_configuration",
            source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        ConfigurationLoadError::NotAFile { path } => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationNotFile,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        ConfigurationLoadError::Read { path, source } => SafeDiagnostic::io(
            DiagnosticCode::ConfigurationRead,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            "read_configuration",
            source,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        ConfigurationLoadError::InvalidUtf8 {
            path,
            valid_up_to,
            error_len,
        } => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationInvalidUtf8,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            DiagnosticReason::InvalidUtf8 {
                valid_up_to: usize_as_u64(*valid_up_to),
                error_len: error_len.map(usize_as_u64),
            },
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        ConfigurationLoadError::InvalidToml {
            path,
            location,
            resource,
            reason,
        } => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationInvalidToml,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            DiagnosticReason::InvalidToml {
                line: location.map(|value| usize_as_u64(value.line())),
                column: location.map(|value| usize_as_u64(value.column())),
                resource: crate::user_text::sanitize_user_text(resource),
                classification: crate::user_text::sanitize_user_text(reason),
            },
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        ConfigurationLoadError::InvalidValue(source) => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationInvalidValue,
            DiagnosticStage::Configuration,
            DiagnosticSubject::field(source.field()),
            DiagnosticReason::InvalidConfigurationValue {
                rule: source.reason().clone(),
            },
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        ),
        ConfigurationLoadError::InvalidValueAtPath { path, source } => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationInvalidValue,
            DiagnosticStage::Configuration,
            DiagnosticSubject::field(source.field()),
            DiagnosticReason::InvalidConfigurationValue {
                rule: source.reason().clone(),
            },
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        )
        .with_recovery(crate::diagnostic::RecoveryFact::path(path)),
        ConfigurationLoadError::TranslationProfileNotFound { path, profile_id } => {
            SafeDiagnostic::new(
                DiagnosticCode::ConfigurationProfileNotFound,
                DiagnosticStage::Configuration,
                DiagnosticSubject::profile(profile_id),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(crate::diagnostic::RecoveryFact::path(path))
        }
        ConfigurationLoadError::ProfileSelectionConflict {
            path,
            explicit_profile,
            requested_profile,
        } => SafeDiagnostic::new(
            DiagnosticCode::ConfigurationProfileConflict,
            DiagnosticStage::Configuration,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::ConflictingValues),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixConfiguration,
        )
        .with_recovery(crate::diagnostic::RecoveryFact::component(format!(
            "explicit_profile={}; requested_profile={}",
            crate::user_text::sanitize_user_text(explicit_profile),
            crate::user_text::sanitize_user_text(requested_profile)
        ))),
    };
    render_diagnostic_fatal(localizer, &diagnostic, stderr)
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

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
                reason: "不应呈现的内部分类",
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
        assert!(stderr.contains("不应呈现的内部分类"));
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
            diagnostic: Some(SafeDiagnostic::io(
                DiagnosticCode::LogWrite,
                DiagnosticStage::Logging,
                DiagnosticSubject::path("C:\\project\\logs\\run.jsonl"),
                "write_all",
                &source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )),
            related_diagnostics: vec![SafeDiagnostic::io(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::Logging,
                DiagnosticSubject::path("C:\\project\\task-records\\run\\task-000001.md"),
                "cleanup_temporary_file",
                &source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )],
        };
        let mut stderr = Vec::new();

        render_project_log_warning(&localizer, &warning, &mut stderr).expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        assert!(stderr.contains("log.write"));
        assert!(stderr.contains("C:\\project\\logs\\run.jsonl"));
        assert!(stderr.contains("write_all"));
        assert!(stderr.contains("C:\\project\\task-records\\run\\task-000001.md"));
        assert!(stderr.contains("cleanup_temporary_file"));
        assert!(stderr.contains("OS 5"));
    }
}
