//! ATT 的可复用应用入口。

#[cfg(not(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")))]
compile_error!("ATT 仅支持 x86_64-pc-windows-msvc");

mod application;
mod diagnostic;
mod execution;
mod fingerprint;
mod generic;
mod i18n;
mod json;
mod json_diagnostic;
mod language;
mod llm;
mod lossless_json;
mod manual;
mod observability;
mod progress;
mod project_lease;
mod project_lua;
mod project_name;
mod rpg_maker;
mod runtime;
mod storage;
mod translation;
mod translation_protocol;
mod user_text;
mod windows_path;

/// 运行 ATT 的生产进程入口。
pub fn run_process() -> std::process::ExitCode {
    application::process::run()
}
