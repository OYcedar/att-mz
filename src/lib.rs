//! ATT 的可复用应用入口。

#[cfg(not(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")))]
compile_error!("ATT 仅支持 x86_64-pc-windows-msvc");

pub mod att_mz;

mod application;
mod execution;
mod language;
mod llm;
mod observability;
mod project_database;
mod runtime;
mod storage;

/// 运行 ATT 的生产进程入口。
pub fn run_process() -> std::process::ExitCode {
    application::process::run()
}
