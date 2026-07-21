//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use super::arguments::AttArguments;
use super::arguments::ProgressArgument;
use super::command::{
    CommandResultRenderer, CommandRunResult, ProductionRpgMakerCommandRunner, TerminationSignals,
};
use super::config::{
    ConfigurationLoadError, ConfigurationPathError, load_product_configuration,
    resolve_configuration_path,
};
use crate::i18n::{UiLocalizer, UiMessage};
use crate::progress::ProgressMode;

/// 运行真实进程入口。
pub(crate) fn run() -> ExitCode {
    // 实时进度由独立线程短暂取得 stderr 锁；进程主线程不能在整个命令期间持有锁。
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    run_from(std::env::args_os(), &mut stdout, &mut stderr)
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
        Err(error) => return render_fatal(&localizer, &error, stderr),
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
    let async_runtime = configuration.common().async_runtime();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(async_runtime.worker_threads().get())
        .max_blocking_threads(async_runtime.max_blocking_threads().get())
        .thread_keep_alive(async_runtime.blocking_thread_keep_alive())
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return render_fatal(&localizer, &error, stderr),
    };

    let (layout, command) = configuration.into_parts();
    let (report, _termination_signals) = runtime.block_on(async {
        let mut termination_signals = TerminationSignals::new();
        let report = ProductionRpgMakerCommandRunner::new(layout, locale, progress_mode)
            .run(command, &mut termination_signals)
            .await;
        (report, termination_signals)
    });
    // 信号订阅与 Runtime 保持到最终结果输出结束；各业务根已经在 report 返回前显式 shutdown。

    if report.log_warning.is_some() {
        let _ = writeln!(stderr, "{}", localizer.format(UiMessage::NoticeLogDegraded));
    }

    match (report.result, report.shutdown_error) {
        (CommandRunResult::Succeeded(output), None) => {
            if let Err(error) = CommandResultRenderer::render_success(output, &localizer, stdout) {
                render_fatal(&localizer, &error, stderr)
            } else {
                ExitCode::SUCCESS
            }
        }
        (CommandRunResult::Failed(command_error), shutdown) => {
            if CommandResultRenderer::render_failure(
                Some(&command_error),
                shutdown
                    .as_ref()
                    .map(|error| error as &dyn std::fmt::Display),
                &localizer,
                stderr,
            )
            .is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
        (CommandRunResult::Interrupted, None) => {
            let _ = writeln!(stderr, "{}", localizer.format(UiMessage::ResultCancelled));
            ExitCode::from(130)
        }
        (CommandRunResult::Interrupted, Some(shutdown)) => {
            let _ =
                CommandResultRenderer::render_failure(None, Some(&shutdown), &localizer, stderr);
            ExitCode::FAILURE
        }
        (CommandRunResult::Succeeded(_), Some(shutdown)) => {
            let _ = CommandResultRenderer::render_applied_finalization_failure(
                &shutdown, &localizer, stderr,
            );
            ExitCode::FAILURE
        }
    }
}

fn render_fatal(
    localizer: &UiLocalizer,
    error: &dyn std::fmt::Display,
    stderr: &mut dyn Write,
) -> ExitCode {
    let _ = error;
    let _ = writeln!(stderr, "{}", localizer.format(UiMessage::ErrorInternal));
    ExitCode::FAILURE
}

fn render_configuration_path_error(
    localizer: &UiLocalizer,
    error: &ConfigurationPathError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let rendered = match error {
        ConfigurationPathError::CurrentDirectoryNotAbsolute(path) => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigCurrentDirectoryNotAbsolute { path: &path })
        }
        ConfigurationPathError::EmptyExplicitPath => {
            localizer.format(UiMessage::ErrorConfigEmptyPath)
        }
    };
    let _ = writeln!(stderr, "{rendered}");
    ExitCode::FAILURE
}

fn render_configuration_load_error(
    localizer: &UiLocalizer,
    error: &ConfigurationLoadError,
    stderr: &mut dyn Write,
) -> ExitCode {
    let rendered = match error {
        ConfigurationLoadError::Open { path, .. } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigOpen { path: &path })
        }
        ConfigurationLoadError::NotAFile { path } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigNotAFile { path: &path })
        }
        ConfigurationLoadError::TooLarge {
            path,
            observed_bytes,
            maximum_bytes,
        } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigTooLarge {
                path: &path,
                observed_bytes: *observed_bytes,
                maximum_bytes: *maximum_bytes,
            })
        }
        ConfigurationLoadError::Read { path, .. } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigRead { path: &path })
        }
        ConfigurationLoadError::InvalidUtf8 {
            path,
            valid_up_to,
            error_len,
        } => {
            let path = path.to_string_lossy();
            if let Some(error_len) = error_len {
                localizer.format(UiMessage::ErrorConfigInvalidUtf8KnownLength {
                    path: &path,
                    valid_up_to: usize_as_u64(*valid_up_to),
                    error_len: usize_as_u64(*error_len),
                })
            } else {
                localizer.format(UiMessage::ErrorConfigInvalidUtf8UnknownLength {
                    path: &path,
                    valid_up_to: usize_as_u64(*valid_up_to),
                })
            }
        }
        ConfigurationLoadError::InvalidToml {
            path,
            location,
            resource,
            ..
        } => {
            let path = path.to_string_lossy();
            if let Some(location) = location {
                localizer.format(UiMessage::ErrorConfigInvalidTomlAt {
                    path: &path,
                    line: usize_as_u64(location.line()),
                    column: usize_as_u64(location.column()),
                    resource,
                })
            } else {
                localizer.format(UiMessage::ErrorConfigInvalidToml {
                    path: &path,
                    resource,
                })
            }
        }
        ConfigurationLoadError::InvalidValue(source) => {
            localizer.format(UiMessage::ErrorConfigInvalidValue {
                field: source.field(),
            })
        }
        ConfigurationLoadError::InvalidValueAtPath { path, source } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigInvalidValueAtPath {
                path: &path,
                field: source.field(),
            })
        }
        ConfigurationLoadError::TranslationProfileNotFound { path, profile_id } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigProfileNotFound {
                path: &path,
                profile: profile_id,
            })
        }
        ConfigurationLoadError::ProfileSelectionConflict {
            path,
            explicit_profile,
            requested_profile,
        } => {
            let path = path.to_string_lossy();
            localizer.format(UiMessage::ErrorConfigProfileConflict {
                path: &path,
                explicit_profile,
                requested_profile,
            })
        }
    };
    let _ = writeln!(stderr, "{rendered}");
    ExitCode::FAILURE
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn configuration_errors_are_localized_without_display_text_leaking() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        let exit = render_configuration_load_error(
            &localizer,
            &ConfigurationLoadError::InvalidToml {
                path: "settings.toml".into(),
                location: Some(super::super::config::SourceLocation::new(3, 7)),
                resource: "runtime.sqlite".to_owned(),
                reason: "不应呈现的内部分类",
            },
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");
        assert!(stderr.starts_with("配置文件"));
        assert!(stderr.contains("settings.toml"));
        let plain = stderr.replace(['\u{2068}', '\u{2069}'], "");
        assert!(plain.contains("第 3 行第 7 列"));
        assert!(stderr.contains("runtime.sqlite"));
        assert!(!stderr.contains("不应呈现的内部分类"));
    }

    #[test]
    fn english_configuration_value_error_uses_typed_localization() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::English);
        let error = super::super::config::invalid(
            "runtime.sqlite.max_open_connections",
            "不应注入英语消息的中文原因",
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
        assert!(stderr.starts_with("Invalid value for configuration field"));
        assert!(stderr.contains("runtime.sqlite.max_open_connections"));
        assert!(stderr.contains("C:\\ATT\\att.toml"));
        assert!(stderr.contains('\u{2068}') && stderr.contains('\u{2069}'));
        assert!(!stderr.contains("不应注入英语消息的中文原因"));
    }

    #[test]
    fn arabic_configuration_paths_are_sanitized_and_directionally_isolated() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::Arabic);
        let mut stderr = Vec::new();
        let exit = render_configuration_load_error(
            &localizer,
            &ConfigurationLoadError::TooLarge {
                path: "C:\\Games\\att\u{202e}\u{2068}\u{1b}[31m.toml".into(),
                observed_bytes: 8_000_000,
                maximum_bytes: 4_194_304,
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
        let exit = render_fatal(&localizer, &"SECRET_SENTINEL", &mut stderr);
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(
            !String::from_utf8(stderr)
                .expect("诊断应为 UTF-8")
                .contains("SECRET_SENTINEL")
        );
    }
}
