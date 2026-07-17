//! `att mz` 命令域的顶层解析、分派和输出。

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};

pub mod extract;
pub mod init;
pub(crate) mod location_codec;
pub(crate) mod lua;
pub(crate) mod placeholder_token;
pub(crate) mod project;
mod project_name;
pub(crate) mod standard_asset;
pub(crate) mod tag;
pub(crate) mod text;
pub mod translate;
pub mod write_back;

pub use project::{MaxFullwidthChars, MaxFullwidthCharsError, MzWriteBackLayoutProfile};
pub use project_name::ProjectName;

use extract::{ExtractInput, ExtractUseCase, ExtractionSelection};
use init::{InitInput, InitUseCase};
use translate::{TranslateInput, TranslateUseCase};
use write_back::{WriteBackInput, WriteBackUseCase};

/// `att mz` 顶层 CLI。
///
/// 该类型只依赖四个命令用例，不了解这些用例的内部实现与下层依赖。
pub struct MzCli<I, E, T, W> {
    init: I,
    extract: E,
    translate: T,
    write_back: W,
}

impl<I, E, T, W> MzCli<I, E, T, W> {
    /// 使用四个直接依赖创建 MZ CLI。
    pub fn new(init: I, extract: E, translate: T, write_back: W) -> Self {
        Self {
            init,
            extract,
            translate,
            write_back,
        }
    }
}

impl<I, E, T, W> MzCli<I, E, T, W>
where
    I: InitUseCase,
    E: ExtractUseCase,
    T: TranslateUseCase,
    W: WriteBackUseCase,
{
    /// 解析 MZ 局部参数，执行一个命令，并把最终输出写入指定 writer。
    ///
    /// `args` 从 MZ 子命令开始，不包含通用入口消费的 `att mz`。例如翻译命令
    /// 应传入 `translate --name game llm-id`。
    pub async fn run_from<A, S>(
        &self,
        args: A,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> io::Result<ExitCode>
    where
        A: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut parse_args = vec![OsString::from("att mz")];
        parse_args.extend(args.into_iter().map(Into::into));

        let arguments = match MzArguments::try_parse_from(parse_args) {
            Ok(arguments) => arguments,
            Err(error) => return render_parse_error(error, stdout, stderr),
        };

        match arguments.command {
            MzCommand::Init(arguments) => {
                let result = self
                    .init
                    .execute(InitInput {
                        name: arguments.project.name,
                        game_root: arguments.path,
                        source_language: arguments.source_language,
                        target_language: arguments.target_language,
                        layout_profile: MzWriteBackLayoutProfile::new(
                            arguments.dialogue_max_fullwidth_chars,
                            arguments.scrolling_text_max_fullwidth_chars,
                            arguments.help_description_max_fullwidth_chars,
                        ),
                    })
                    .await;

                match result {
                    Ok(output) => {
                        writeln!(stdout, "初始化完成：{}", output.name)?;
                        Ok(ExitCode::SUCCESS)
                    }
                    Err(error) => render_use_case_error(error, stderr),
                }
            }
            MzCommand::Extract(arguments) => {
                let selection =
                    ExtractionSelection::new(arguments.builtin, arguments.rules, arguments.lua)
                        .expect("Clap 参数组应保证至少选择一种提取方式");

                let result = self
                    .extract
                    .execute(ExtractInput {
                        name: arguments.project.name,
                        selection,
                    })
                    .await;

                match result {
                    Ok(output) => {
                        writeln!(stdout, "提取完成：{}", output.name)?;
                        Ok(ExitCode::SUCCESS)
                    }
                    Err(error) => render_use_case_error(error, stderr),
                }
            }
            MzCommand::Translate(arguments) => {
                let result = self
                    .translate
                    .execute(TranslateInput {
                        name: arguments.project.name,
                        llm_id: arguments.llm_id,
                        terminology_path: arguments.terms,
                        placeholder_rules_path: arguments.placeholders,
                        lua_script: arguments.lua,
                    })
                    .await;

                match result {
                    Ok(output) => {
                        writeln!(
                            stdout,
                            "翻译执行完成：{}（LLM：{}）",
                            output.name, output.llm_id
                        )?;
                        writeln!(
                            stdout,
                            "标准翻译：任务 {}，完整 {}，部分 {}，不可用 {}；写入 {} 处，剩余 {} 处",
                            output.standard.total_tasks,
                            output.standard.complete_tasks,
                            output.standard.partial_tasks,
                            output.standard.unavailable_tasks,
                            output.standard.written_locations,
                            output.standard.remaining_locations,
                        )?;
                        if output.lua_executed {
                            writeln!(stdout, "Lua 翻译：已执行")?;
                        }
                        Ok(ExitCode::SUCCESS)
                    }
                    Err(error) => render_use_case_error(error, stderr),
                }
            }
            MzCommand::WriteBack(arguments) => {
                let result = self
                    .write_back
                    .execute(WriteBackInput {
                        name: arguments.project.name,
                        lua_script: arguments.lua,
                    })
                    .await;

                match result {
                    Ok(output) => {
                        writeln!(stdout, "写回完成：{}", output.name)?;
                        writeln!(stdout, "输出目录：{}", output.output_root.display())?;
                        writeln!(
                            stdout,
                            "标准写回：应用译文 {} 处，保留原文 {} 处；自动换行 {} 段，新增换行 {} 处；续行全角缩进 {} 处；需人工换行 {} 段",
                            output.standard.translated_locations,
                            output.standard.original_locations,
                            output.standard.auto_wrapped_units,
                            output.standard.inserted_line_breaks,
                            output.standard.inserted_fullwidth_indents,
                            output.standard.manual_layout_units,
                        )?;
                        if output.standard.manual_layout_units > 0 {
                            writeln!(
                                stdout,
                                "人工处理：{} 段文本需要手动换行",
                                output.standard.manual_layout_units
                            )?;
                        }
                        writeln!(
                            stdout,
                            "Lua 写回：{}",
                            if output.lua_executed {
                                "已执行"
                            } else {
                                "未执行"
                            }
                        )?;
                        Ok(ExitCode::SUCCESS)
                    }
                    Err(error) => render_use_case_error(error, stderr),
                }
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "att mz", about = "RPG Maker MZ 游戏翻译命令域")]
struct MzArguments {
    #[command(subcommand)]
    command: MzCommand,
}

#[derive(Debug, Subcommand)]
enum MzCommand {
    /// 初始化一个命名的 MZ 游戏。
    #[command(name = "init")]
    Init(InitArguments),
    /// 提取已初始化游戏中的原文。
    #[command(name = "extract")]
    Extract(ExtractArguments),
    /// 使用指定 LLM 配置翻译已提取原文。
    #[command(name = "translate")]
    Translate(TranslateArguments),
    /// 把已验收译文写回游戏。
    #[command(name = "write-back")]
    WriteBack(WriteBackArguments),
}

#[derive(Debug, Args)]
struct InitArguments {
    #[command(flatten)]
    project: NameArgs,
    /// RPG Maker MZ 游戏根目录。
    #[arg(long, value_name = "DIR", value_parser = parse_non_blank_path)]
    path: PathBuf,
    /// 游戏原文语言。
    #[arg(long, value_name = "LANG", value_parser = parse_non_blank)]
    source_language: String,
    /// 译文目标语言。
    #[arg(long, value_name = "LANG", value_parser = parse_non_blank)]
    target_language: String,
    /// 对话正文每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    dialogue_max_fullwidth_chars: MaxFullwidthChars,
    /// 滚动文本每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    scrolling_text_max_fullwidth_chars: MaxFullwidthChars,
    /// 帮助或说明框每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    help_description_max_fullwidth_chars: MaxFullwidthChars,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("extract_tasks")
        .required(true)
        .multiple(true)
        .args(["builtin", "rules", "lua"])
))]
struct ExtractArguments {
    #[command(flatten)]
    project: NameArgs,
    /// 使用项目内置的 MZ 文本位置规格。
    #[arg(long)]
    builtin: bool,
    /// 按指定 JSON 规则补充提取位置。
    #[arg(long, value_name = "RULES_JSON", value_parser = parse_non_blank_path)]
    rules: Option<PathBuf>,
    /// 通过指定 Lua 脚本执行自由提取。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct TranslateArguments {
    #[command(flatten)]
    project: NameArgs,
    /// 配置文件中要使用的 LLM 配置 ID。
    #[arg(value_name = "LLM_ID", value_parser = parse_non_blank)]
    llm_id: String,
    /// 本次标准翻译使用的术语表 JSON 文件。
    #[arg(long, value_name = "TERMS_JSON", value_parser = parse_non_blank_path)]
    terms: Option<PathBuf>,
    /// 本次标准翻译使用的自定义占位符规则 JSON 文件。
    #[arg(
        long,
        value_name = "PLACEHOLDERS_JSON",
        value_parser = parse_non_blank_path
    )]
    placeholders: Option<PathBuf>,
    /// 使用可信 Lua 程序处理自由翻译数据。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct WriteBackArguments {
    #[command(flatten)]
    project: NameArgs,
    /// 使用可信 Lua 程序处理自由写回数据。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct NameArgs {
    /// MZ 游戏的稳定项目名称。
    #[arg(long, value_name = "NAME")]
    name: ProjectName,
}

fn parse_non_blank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("值不能为空".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn parse_non_blank_path(value: &str) -> Result<PathBuf, String> {
    parse_non_blank(value).map(PathBuf::from)
}

fn parse_max_fullwidth_chars(value: &str) -> Result<MaxFullwidthChars, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| "每行最大全角字符数必须是正整数".to_owned())?;
    MaxFullwidthChars::new(value).map_err(|error| error.to_string())
}

fn render_parse_error(
    error: clap::Error,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<ExitCode> {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            write!(stdout, "{error}")?;
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            write!(stderr, "{error}")?;
            Ok(ExitCode::from(2))
        }
    }
}

fn render_use_case_error(error: impl Display, stderr: &mut dyn Write) -> io::Result<ExitCode> {
    writeln!(stderr, "错误：{error}")?;
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests;
