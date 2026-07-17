//! ATT 进程启动、Ctrl-C、shutdown 与退出码边界。

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use super::arguments::AttArguments;
use super::command::{CommandResultRenderer, ProductionMzCommandRunner};
use super::config::{load_configuration, resolve_configuration_path};

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
        Err(error) => return render_fatal("无法读取当前工作目录", &error, stderr),
    };
    let app_data = std::env::var_os("APPDATA");
    let configuration_path = match resolve_configuration_path(
        arguments.config.as_deref(),
        &current_directory,
        app_data.as_deref(),
    ) {
        Ok(path) => path,
        Err(error) => return render_fatal("无法定位配置文件", &error, stderr),
    };
    let configuration = match load_configuration(&configuration_path) {
        Ok(configuration) => configuration,
        Err(error) => return render_fatal("无法加载配置", &error, stderr),
    };
    let async_runtime = configuration.runtime().async_runtime();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(async_runtime.worker_threads().get())
        .max_blocking_threads(async_runtime.max_blocking_threads().get())
        .thread_keep_alive(async_runtime.blocking_thread_keep_alive())
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return render_fatal("无法构造异步运行时", &error, stderr),
    };

    let command = arguments.product.into_mz();
    let report = runtime.block_on(ProductionMzCommandRunner::new(configuration).run(command));
    // 所有根已经显式 shutdown；丢弃 Runtime 是最终进程资源终结步骤。
    drop(runtime);

    if report.interrupted {
        let command_error = report.command_result.and_then(Result::err);
        if command_error.is_some() || report.shutdown_error.is_some() {
            let _ = CommandResultRenderer::render_failure(
                command_error
                    .as_ref()
                    .map(|error| error as &dyn std::fmt::Display),
                report
                    .shutdown_error
                    .as_ref()
                    .map(|error| error as &dyn std::fmt::Display),
                stderr,
            );
            return ExitCode::FAILURE;
        }
        return ExitCode::from(130);
    }

    match (report.command_result, report.shutdown_error) {
        (Some(Ok(output)), None) => {
            if let Err(error) = CommandResultRenderer::render_success(output, stdout) {
                render_fatal("无法写入标准输出", &error, stderr)
            } else {
                ExitCode::SUCCESS
            }
        }
        (Some(result), shutdown) => {
            let command_error = result.err();
            if CommandResultRenderer::render_failure(
                command_error
                    .as_ref()
                    .map(|error| error as &dyn std::fmt::Display),
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
        (None, shutdown) => {
            if let Some(shutdown) = shutdown.as_ref() {
                let _ = CommandResultRenderer::render_failure(None, Some(shutdown), stderr);
            }
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
    }
}
