//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::error::ErrorKind;

use super::arguments::AttArguments;
use super::command::{CommandResultRenderer, CommandRunResult, ProductionRpgMakerCommandRunner};
use super::config::{load_product_configuration, resolve_configuration_path};

/// 运行真实进程入口。
pub(crate) fn run() -> ExitCode {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    run_from(std::env::args_os(), &mut stdout, &mut stderr)
}

fn run_from<A, S>(args: A, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode
where
    A: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let arguments = match AttArguments::try_parse_from(args) {
        Ok(arguments) => arguments,
        Err(error) => return render_parse_error(error, stdout, stderr),
    };
    let current_directory = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => return render_fatal("内部技术故障", &error, stderr),
    };
    let configuration_path = match resolve_configuration_path(&arguments.config, &current_directory)
    {
        Ok(path) => path,
        Err(error) => return render_user_error("配置或输入错误", &error, stderr),
    };
    let configuration = match load_product_configuration(&configuration_path, arguments.product) {
        Ok(configuration) => configuration,
        Err(error) => return render_user_error("配置或输入错误", &error, stderr),
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
        Err(error) => return render_fatal("内部技术故障", &error, stderr),
    };

    let (layout, command) = configuration.into_parts();
    let report = runtime.block_on(ProductionRpgMakerCommandRunner::new(layout).run(command));
    // 所有根已经显式 shutdown；丢弃 Runtime 是最终进程资源终结步骤。
    drop(runtime);

    match (report.result, report.shutdown_error) {
        (CommandRunResult::Succeeded(output), None) => {
            if let Err(error) = CommandResultRenderer::render_success(output, stdout) {
                render_fatal("内部技术故障", &error, stderr)
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
                stderr,
            )
            .is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
        (CommandRunResult::Interrupted, None) => ExitCode::from(130),
        (CommandRunResult::Interrupted, Some(shutdown)) => {
            let _ = CommandResultRenderer::render_failure(None, Some(&shutdown), stderr);
            ExitCode::FAILURE
        }
        (CommandRunResult::Succeeded(_), Some(shutdown)) => {
            let _ = CommandResultRenderer::render_applied_finalization_failure(&shutdown, stderr);
            ExitCode::FAILURE
        }
    }
}

fn render_parse_error(
    error: clap::Error,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            if write!(stdout, "{error}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            let _ = write!(stderr, "{error}");
            ExitCode::from(2)
        }
    }
}

fn render_fatal(stage: &str, error: &dyn std::fmt::Display, stderr: &mut dyn Write) -> ExitCode {
    let _ = error;
    let _ = writeln!(stderr, "{stage}");
    ExitCode::FAILURE
}

fn render_user_error(
    stage: &str,
    error: &dyn std::fmt::Display,
    stderr: &mut dyn Write,
) -> ExitCode {
    let _ = writeln!(stderr, "{stage}：{error}");
    ExitCode::FAILURE
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
    fn user_errors_include_safe_detail_but_internal_errors_do_not() {
        let mut stderr = Vec::new();
        let exit = render_user_error(
            "配置或输入错误",
            &"配置文件 settings.toml 第 3 行无效",
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(
            String::from_utf8(stderr).expect("诊断应为 UTF-8"),
            "配置或输入错误：配置文件 settings.toml 第 3 行无效\n"
        );

        let mut stderr = Vec::new();
        let exit = render_fatal("内部技术故障", &"SECRET_SENTINEL", &mut stderr);
        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(
            String::from_utf8(stderr).expect("诊断应为 UTF-8"),
            "内部技术故障\n"
        );
    }
}
