//! `att` 进程入口的纯参数契约。
//!
//! 本模块只把命令行转换为用户意图，不构造运行时、读取配置或执行业务。

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use clap::error::{ContextKind, ErrorKind};
use clap::{Arg, ArgAction, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::i18n::{
    ResolvedUiLocale, UiLocaleInputSource, UiLocalizer, UiMessage,
    resolve_lower_priority_ui_locale, resolve_ui_locale,
};
use crate::language::LanguageId;
use crate::project_name::ProjectName;
use crate::rpg_maker::MaxFullwidthChars;

/// 已完成解析的 ATT 进程参数。
#[derive(Debug)]
pub(crate) struct AttArguments {
    pub(crate) product: ProductCommand,
}

impl AttArguments {
    /// 在 Clap 生成任何用户可见内容前选择 UI locale，并以该语言解析完整命令行。
    ///
    /// 成功时同时返回参数与语言来源；Help、Version 和所有参数错误通过
    /// [`LocalizedCliError`] 返回，调用方无需再次选择 locale 或解析 Clap 文本。
    pub(crate) fn try_parse_localized_from<I, T>(
        arguments: I,
    ) -> Result<(Self, ResolvedUiLocale), LocalizedCliError>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        let explicit_language = match scan_ui_language(&arguments) {
            UiLanguageScan::Absent => None,
            UiLanguageScan::Value(value) => Some(value),
            failure @ (UiLanguageScan::MissingValue | UiLanguageScan::InvalidUnicode) => {
                let resolved = resolve_lower_priority_ui_locale(UiLocaleInputSource::CommandLine);
                let localizer = UiLocalizer::new(resolved.locale());
                return Err(parse_before_ui_locale_failure(
                    &arguments, failure, &localizer,
                ));
            }
        };

        let resolved = match resolve_ui_locale(explicit_language.as_deref()) {
            Ok(resolved) => resolved,
            Err(error) => {
                let failed_input = match error {
                    crate::i18n::UiLocaleSelectionError::InvalidLanguageTag { input, .. }
                    | crate::i18n::UiLocaleSelectionError::UnsupportedLanguage { input, .. } => {
                        input
                    }
                    crate::i18n::UiLocaleSelectionError::EnvironmentNotUnicode => {
                        UiLocaleInputSource::Environment
                    }
                };
                let fallback = resolve_lower_priority_ui_locale(failed_input);
                let localizer = UiLocalizer::new(fallback.locale());
                let mut command = localized_command(&localizer);
                let usage = localized_usage(&command.render_usage().to_string(), &localizer);
                return Err(LocalizedCliError::input(
                    ErrorKind::ValueValidation,
                    localizer.format(error.ui_message()),
                    Some(usage),
                    &localizer,
                ));
            }
        };
        let localizer = UiLocalizer::new(resolved.locale());
        let command = localized_command(&localizer);
        let mut fallback_usage_command = command.clone();
        let fallback_usage = localized_usage(
            &fallback_usage_command.render_usage().to_string(),
            &localizer,
        );
        let matches = command.try_get_matches_from(arguments).map_err(|error| {
            LocalizedCliError::from_clap(error, &localizer, Some(fallback_usage.clone()))
        })?;
        let raw = RawAttArguments::from_arg_matches(&matches).map_err(|error| {
            LocalizedCliError::from_clap(error, &localizer, Some(fallback_usage))
        })?;
        let RawAttArguments {
            ui_language: _,
            product,
        } = raw;
        Ok((Self { product }, resolved))
    }

    #[cfg(test)]
    pub(crate) fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let raw = RawAttArguments::try_parse_from(arguments)?;
        let RawAttArguments {
            ui_language: _,
            product,
        } = raw;
        Ok(Self { product })
    }

    #[cfg(test)]
    pub(crate) fn command() -> clap::Command {
        RawAttArguments::command()
    }
}

/// 已完成本地化的 CLI 提前退出或输入错误。
#[derive(Debug)]
pub(crate) struct LocalizedCliError {
    kind: ErrorKind,
    output: String,
}

impl LocalizedCliError {
    #[cfg(test)]
    pub(crate) const fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub(crate) const fn exit_code(&self) -> u8 {
        match self.kind {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
            _ => 2,
        }
    }

    pub(crate) const fn use_stderr(&self) -> bool {
        self.exit_code() != 0
    }

    pub(crate) fn output(&self) -> &str {
        &self.output
    }

    fn from_clap(
        error: clap::Error,
        localizer: &UiLocalizer,
        fallback_usage: Option<String>,
    ) -> Self {
        let kind = error.kind();
        if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
            return Self {
                kind,
                output: error.to_string(),
            };
        }

        let detail = localize_clap_error(&error, localizer);
        let usage = error
            .get(ContextKind::Usage)
            .map(ToString::to_string)
            .map(|usage| localized_usage(&usage, localizer))
            .or(fallback_usage);
        Self::input(kind, detail, usage, localizer)
    }

    fn input(
        kind: ErrorKind,
        detail: String,
        usage: Option<String>,
        localizer: &UiLocalizer,
    ) -> Self {
        let mut output = format!(
            "{} {}\n",
            localizer.format(UiMessage::CliErrorHeading),
            detail
        );
        if let Some(usage) = usage {
            output.push('\n');
            output.push_str(&usage);
            output.push('\n');
        }
        output.push('\n');
        output.push_str(&localizer.format(UiMessage::CliTryHelp));
        output.push('\n');
        Self { kind, output }
    }
}

impl fmt::Display for LocalizedCliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.output)
    }
}

impl std::error::Error for LocalizedCliError {}

/// Clap 解析阶段允许全局参数在子命令前后出现。
#[derive(Debug, Parser)]
#[command(name = "att", bin_name = "att", about = "游戏翻译工具", version)]
struct RawAttArguments {
    /// 终端、帮助与项目日志消息使用的界面语言。
    #[arg(long, global = true, value_name = "LANG", value_parser = parse_non_blank)]
    ui_language: Option<String>,

    #[command(subcommand)]
    product: ProductCommand,
}

/// 统一产品入口当前支持的命令域。
#[derive(Debug, Subcommand)]
pub(crate) enum ProductCommand {
    /// RPG Maker MZ 游戏翻译。
    #[command(name = "mz")]
    Mz {
        #[command(subcommand)]
        command: MzCommand,
    },
    /// RPG Maker MV 游戏翻译。
    #[command(name = "mv")]
    Mv {
        #[command(subcommand)]
        command: MvCommand,
    },
    /// 约定 JSONL 的通用翻译。
    #[command(name = "generic")]
    Generic {
        #[command(subcommand)]
        command: GenericCommand,
    },
}

/// `att mz` 当前支持的用户意图。
#[derive(Debug, Subcommand)]
pub(crate) enum MzCommand {
    /// 初始化一个命名的 MZ 游戏。
    #[command(name = "init")]
    Init(InitArguments),
    /// 提取已初始化游戏中的原文。
    #[command(name = "extract")]
    Extract(ExtractArguments),
    /// 使用指定翻译 Profile 翻译已提取原文。
    #[command(name = "translate")]
    Translate(TranslateArguments),
    /// 把已验收译文写回游戏。
    #[command(name = "write-back")]
    WriteBack(WriteBackArguments),
    /// 导出、检查或应用人工译文。
    #[command(name = "manual")]
    Manual {
        #[command(subcommand)]
        command: ManualCommand,
    },
    /// 对项目数据库运行 Lua 脚本。
    #[command(name = "lua")]
    Lua(ProjectLuaArguments),
}

/// 预扫描只负责在 Clap 生成内容前选择语言，不能抢占真实参数 schema 的根错误。
///
/// 只有 Clap 确认整条命令行不存在更早的结构错误时，才呈现预扫描自身无法建立 locale
/// 的失败。这也确保新增或重排参数时，预扫描不会成为第二套命令行解析器。
fn parse_before_ui_locale_failure(
    arguments: &[OsString],
    failure: UiLanguageScan,
    localizer: &UiLocalizer,
) -> LocalizedCliError {
    let command = localized_command(localizer);
    let mut fallback_usage_command = command.clone();
    let fallback_usage = localized_usage(
        &fallback_usage_command.render_usage().to_string(),
        localizer,
    );
    if let Err(error) = command.try_get_matches_from(arguments.iter().cloned()) {
        return LocalizedCliError::from_clap(error, localizer, Some(fallback_usage));
    }

    match failure {
        UiLanguageScan::MissingValue => LocalizedCliError::input(
            ErrorKind::MissingRequiredArgument,
            localizer.format(UiMessage::CliMissingValue {
                argument: "--ui-language",
            }),
            Some(fallback_usage),
            localizer,
        ),
        UiLanguageScan::InvalidUnicode => LocalizedCliError::input(
            ErrorKind::InvalidUtf8,
            localizer.format(UiMessage::CliInvalidUtf8),
            Some(fallback_usage),
            localizer,
        ),
        UiLanguageScan::Absent | UiLanguageScan::Value(_) => {
            unreachable!("只有失败的 UI locale 预扫描才能进入本边界")
        }
    }
}

/// `att mv` 当前支持的用户意图。
#[derive(Debug, Subcommand)]
pub(crate) enum MvCommand {
    /// 初始化一个命名的 MV 游戏。
    #[command(name = "init")]
    Init(InitArguments),
    /// 提取已初始化游戏中的原文。
    #[command(name = "extract")]
    Extract(MvExtractArguments),
    /// 使用指定翻译 Profile 翻译已提取原文。
    #[command(name = "translate")]
    Translate(TranslateArguments),
    /// 把已验收译文写回游戏。
    #[command(name = "write-back")]
    WriteBack(WriteBackArguments),
    /// 导出、检查或应用人工译文。
    #[command(name = "manual")]
    Manual {
        #[command(subcommand)]
        command: ManualCommand,
    },
    /// 对项目数据库运行 Lua 脚本。
    #[command(name = "lua")]
    Lua(ProjectLuaArguments),
}

/// `att generic` 当前支持的用户意图。
#[derive(Debug, Subcommand)]
pub(crate) enum GenericCommand {
    /// 建立或更新一个绑定外部 JSONL 目录的项目。
    #[command(name = "init")]
    Init(GenericInitArguments),
    /// 把当前 JSONL 内容同步到项目数据库。
    #[command(name = "extract")]
    Extract(ProjectArguments),
    /// 使用指定翻译 Profile 翻译已同步原文。
    #[command(name = "translate")]
    Translate(TranslateArguments),
    /// 把当前译文写入项目输出目录。
    #[command(name = "write-back")]
    WriteBack(ProjectArguments),
    /// 导出、检查或应用人工译文。
    #[command(name = "manual")]
    Manual {
        #[command(subcommand)]
        command: ManualCommand,
    },
    /// 对项目数据库运行 Lua 脚本。
    #[command(name = "lua")]
    Lua(ProjectLuaArguments),
}

/// 三种引擎共享的人工补译命令。
#[derive(Debug, Subcommand)]
pub(crate) enum ManualCommand {
    /// 导出当前需要人工处理的条目。
    #[command(name = "export")]
    Export(ManualArguments),
    /// 只读检查人工译文文件。
    #[command(name = "check")]
    Check(ManualArguments),
    /// 原子应用已填写且有效的人工译文。
    #[command(name = "apply")]
    Apply(ManualArguments),
}

#[derive(Debug, Args)]
pub(crate) struct ManualArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 人工译文 TOML 文件。
    #[arg(value_name = "FILE_TOML", value_parser = parse_non_blank_path)]
    pub(crate) file: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct GenericInitArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 包含 Generic JSONL 输入的外部目录。
    #[arg(long, value_name = "DIR", value_parser = parse_non_blank_path)]
    pub(crate) path: Option<PathBuf>,
    /// JSONL 原文语言 ID。
    #[arg(long, value_name = "LANG", value_parser = parse_language_id)]
    pub(crate) source_language: Option<LanguageId>,
    /// JSONL 译文目标语言 ID。
    #[arg(long, value_name = "LANG", value_parser = parse_language_id)]
    pub(crate) target_language: Option<LanguageId>,
}

#[derive(Debug, Args)]
pub(crate) struct InitArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// RPG Maker 游戏根目录。
    #[arg(long, value_name = "DIR", value_parser = parse_non_blank_path)]
    pub(crate) path: Option<PathBuf>,
    /// 游戏原文语言 ID。
    #[arg(long, value_name = "LANG", value_parser = parse_language_id)]
    pub(crate) source_language: Option<LanguageId>,
    /// 译文目标语言 ID。
    #[arg(long, value_name = "LANG", value_parser = parse_language_id)]
    pub(crate) target_language: Option<LanguageId>,
    /// 对话正文每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    pub(crate) dialogue_max_fullwidth_chars: Option<MaxFullwidthChars>,
    /// 滚动文本每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    pub(crate) scrolling_text_max_fullwidth_chars: Option<MaxFullwidthChars>,
    /// 帮助或说明框每行允许的最大全角字符数。
    #[arg(long, value_name = "COUNT", value_parser = parse_max_fullwidth_chars)]
    pub(crate) help_description_max_fullwidth_chars: Option<MaxFullwidthChars>,
}

#[derive(Debug, Args)]
pub(crate) struct ExtractArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 使用内置 RPG Maker 文本位置规格。
    #[arg(long)]
    pub(crate) builtin: bool,
    /// 按指定 TOML 规则提取外部明确声明的位置。
    #[arg(long, value_name = "RULES_TOML", value_parser = parse_non_blank_path)]
    pub(crate) rules: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct MvExtractArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 使用内置 RPG Maker 文本位置规格。
    #[arg(long)]
    pub(crate) builtin: bool,
    /// 按指定 TOML 规则提取外部明确声明的位置。
    #[arg(long, value_name = "RULES_TOML", value_parser = parse_non_blank_path)]
    pub(crate) rules: Option<PathBuf>,
    /// 用该 TOML 定义替换项目当前 MV 对话姓名投影；省略时复用已保存定义。
    #[arg(
        long,
        value_name = "DIALOGUE_TOML",
        value_parser = parse_non_blank_path,
        requires = "builtin"
    )]
    pub(crate) dialogue_rules: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct TranslateArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 配置文件中要使用的翻译 Profile ID。
    #[arg(value_name = "PROFILE_ID", value_parser = parse_non_blank)]
    pub(crate) profile_id: Option<String>,
    /// 用该 TOML 文件替换项目当前术语表；省略时复用已保存内容。
    #[arg(long, value_name = "TERMS_TOML", value_parser = parse_non_blank_path)]
    pub(crate) terms: Option<PathBuf>,
    /// 用该 TOML 文件替换项目当前占位符规则；省略时复用已保存内容。
    #[arg(long, value_name = "PLACEHOLDERS_TOML", value_parser = parse_non_blank_path)]
    pub(crate) placeholders: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct WriteBackArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
}

#[derive(Debug, Args)]
pub(crate) struct ProjectLuaArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 要对项目数据库运行的 Lua 脚本。
    #[arg(value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    pub(crate) script: PathBuf,
    /// `--` 后原样传给 Lua 全局 `arg[1..]` 的 UTF-8 参数。
    #[arg(value_name = "ARG", last = true)]
    pub(crate) arguments: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ProjectArguments {
    /// 当前引擎项目的稳定名称。
    #[arg(long, value_name = "NAME")]
    pub(crate) name: ProjectName,
}

#[derive(Debug, Eq, PartialEq)]
enum UiLanguageScan {
    Absent,
    Value(String),
    MissingValue,
    InvalidUnicode,
}

fn scan_ui_language(arguments: &[OsString]) -> UiLanguageScan {
    let mut index = 1;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            index += 1;
            continue;
        };
        if argument == "--" {
            break;
        }
        if let Some(value) = argument.strip_prefix("--ui-language=") {
            return UiLanguageScan::Value(value.to_owned());
        }
        if argument == "--ui-language" {
            let Some(value) = arguments.get(index + 1) else {
                return UiLanguageScan::MissingValue;
            };
            let Some(value) = value.to_str() else {
                return UiLanguageScan::InvalidUnicode;
            };
            if value.starts_with('-') {
                return UiLanguageScan::MissingValue;
            }
            return UiLanguageScan::Value(value.to_owned());
        }
        index += 1;
    }
    UiLanguageScan::Absent
}

fn localized_command(localizer: &UiLocalizer) -> Command {
    let command = RawAttArguments::command();
    let command_path = command.get_name().to_owned();
    localize_command_tree(command, localizer, &command_path)
}

fn localize_command_tree(
    mut command: Command,
    localizer: &UiLocalizer,
    command_path: &str,
) -> Command {
    let command_name = command.get_name().to_owned();
    let about = localizer.format(command_about(&command_name));
    let help_template = localized_help_template(&command, localizer);
    let usage = localized_usage_syntax(command_path, &command_name, localizer);

    command = command
        .about(about.clone())
        .long_about(about)
        .help_template(help_template)
        .override_usage(usage)
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .disable_version_flag(true);

    const ARGUMENT_IDENTIFIERS: [&str; 18] = [
        "ui_language",
        "progress",
        "name",
        "path",
        "source_language",
        "target_language",
        "dialogue_max_fullwidth_chars",
        "scrolling_text_max_fullwidth_chars",
        "help_description_max_fullwidth_chars",
        "builtin",
        "rules",
        "dialogue_rules",
        "profile_id",
        "terms",
        "placeholders",
        "script",
        "arguments",
        "file",
    ];
    for identifier in ARGUMENT_IDENTIFIERS {
        let Some(takes_values) = command
            .get_arguments()
            .find(|argument| argument.get_id().as_str() == identifier)
            .map(|argument| argument.get_action().takes_values())
        else {
            continue;
        };
        let message = argument_help(identifier).expect("已列出的参数必须拥有本地化帮助消息");
        let help = localizer.format(message);
        command = command.mut_arg(identifier, move |argument| {
            let localized = argument.help(help.clone()).long_help(help);
            if takes_values {
                localized.hide_possible_values(true)
            } else {
                localized
            }
        });
    }

    command = command.arg(
        Arg::new("help")
            .short('h')
            .long("help")
            .action(ArgAction::Help)
            .help(localizer.format(UiMessage::CliPrintHelp)),
    );
    if command_name == "att" {
        command = command.arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help(localizer.format(UiMessage::CliPrintVersion)),
        );
    }

    for subcommand in command.get_subcommands_mut() {
        let subcommand_path = format!("{command_path} {}", subcommand.get_name());
        *subcommand = localize_command_tree(subcommand.clone(), localizer, &subcommand_path);
    }
    command
}

fn localized_usage_syntax(
    command_path: &str,
    command_name: &str,
    localizer: &UiLocalizer,
) -> String {
    let options = localizer.format(UiMessage::CliOptionsMetavar);
    let nested_command = localizer.format(UiMessage::CliCommandMetavar);
    let syntax = match command_name {
        "att" | "mz" | "mv" | "generic" | "manual" => {
            format!("{command_path} [{options}] <{nested_command}>")
        }
        "translate" => format!("{command_path} --name <NAME> [PROFILE_ID] [{options}]"),
        "lua" => format!("{command_path} --name <NAME> [{options}] <SCRIPT_LUA> [-- <ARG>...]"),
        "export" | "check" | "apply" => {
            format!("{command_path} --name <NAME> <FILE_TOML> [{options}]")
        }
        "init" | "extract" | "write-back" => {
            format!("{command_path} --name <NAME> [{options}]")
        }
        _ => format!("{command_path} [{options}]"),
    };
    format!("\u{2068}{syntax}\u{2069}")
}

fn localized_help_template(command: &Command, localizer: &UiLocalizer) -> String {
    let mut template = format!(
        "{{before-help}}{{about-with-newline}}\n{} {{usage}}",
        localizer.format(UiMessage::CliUsageHeading)
    );
    if command.get_subcommands().next().is_some() {
        template.push_str("\n\n");
        template.push_str(&localizer.format(UiMessage::CliCommandsHeading));
        template.push_str("\n{subcommands}");
    }
    if command.get_positionals().next().is_some() {
        template.push_str("\n\n");
        template.push_str(&localizer.format(UiMessage::CliArgumentsHeading));
        template.push_str("\n{positionals}");
    }
    // 每个命令都显式提供本地化的 `--help`，因此 Options 段始终存在；全局选项在
    // Clap build 时传播到子命令后也会由同一个占位符呈现。
    template.push_str("\n\n");
    template.push_str(&localizer.format(UiMessage::CliOptionsHeading));
    template.push_str("\n{options}{after-help}");
    template
}

fn command_about(name: &str) -> UiMessage<'static> {
    match name {
        "att" => UiMessage::AppAbout,
        "mz" => UiMessage::CliMzAbout,
        "mv" => UiMessage::CliMvAbout,
        "generic" => UiMessage::CliGenericAbout,
        "init" => UiMessage::CliInitAbout,
        "extract" => UiMessage::CliExtractAbout,
        "translate" => UiMessage::CliTranslateAbout,
        "write-back" => UiMessage::CliWriteBackAbout,
        "manual" => UiMessage::CliManualAbout,
        "export" => UiMessage::CliManualExportAbout,
        "check" => UiMessage::CliManualCheckAbout,
        "apply" => UiMessage::CliManualApplyAbout,
        "lua" => UiMessage::CliProjectLuaAbout,
        _ => UiMessage::AppAbout,
    }
}

fn argument_help(identifier: &str) -> Option<UiMessage<'static>> {
    match identifier {
        "ui_language" => Some(UiMessage::CliUiLanguageHelp),
        "name" => Some(UiMessage::CliProjectNameHelp),
        "path" => Some(UiMessage::CliInitPathHelp),
        "source_language" => Some(UiMessage::CliSourceLanguageHelp),
        "target_language" => Some(UiMessage::CliTargetLanguageHelp),
        "dialogue_max_fullwidth_chars" => Some(UiMessage::CliDialogueWidthHelp),
        "scrolling_text_max_fullwidth_chars" => Some(UiMessage::CliScrollingWidthHelp),
        "help_description_max_fullwidth_chars" => Some(UiMessage::CliHelpWidthHelp),
        "builtin" => Some(UiMessage::CliBuiltinHelp),
        "rules" => Some(UiMessage::CliRulesHelp),
        "dialogue_rules" => Some(UiMessage::CliDialogueRulesHelp),
        "profile_id" => Some(UiMessage::CliProfileHelp),
        "terms" => Some(UiMessage::CliTermsHelp),
        "placeholders" => Some(UiMessage::CliPlaceholdersHelp),
        "script" => Some(UiMessage::CliProjectLuaScriptHelp),
        "arguments" => Some(UiMessage::CliProjectLuaArgumentsHelp),
        "file" => Some(UiMessage::CliManualFileHelp),
        _ => None,
    }
}

fn localize_clap_error(error: &clap::Error, localizer: &UiLocalizer) -> String {
    let argument = error_context(error, ContextKind::InvalidArg)
        .or_else(|| error_context(error, ContextKind::PriorArg))
        .unwrap_or_default();
    let value = error_context(error, ContextKind::InvalidValue)
        .or_else(|| error_context(error, ContextKind::InvalidSubcommand))
        .or_else(|| error_context(error, ContextKind::TrailingArg))
        .unwrap_or_default();

    match error.kind() {
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            if value.trim().is_empty() {
                localizer.format(UiMessage::CliBlankValue)
            } else if argument.contains("fullwidth") {
                localizer.format(UiMessage::CliInvalidPositiveInteger)
            } else if argument.is_empty() {
                localizer.format(UiMessage::CliParseFailure)
            } else {
                localizer.format(UiMessage::CliInvalidValue {
                    value: &value,
                    argument: &argument,
                })
            }
        }
        ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand => {
            let unexpected = if value.is_empty() { &argument } else { &value };
            localizer.format(UiMessage::CliUnexpectedArgument { value: unexpected })
        }
        ErrorKind::MissingRequiredArgument => {
            if argument.is_empty() {
                localizer.format(UiMessage::CliParseFailure)
            } else {
                localizer.format(UiMessage::CliMissingRequiredArgument { value: &argument })
            }
        }
        ErrorKind::MissingSubcommand => localizer.format(UiMessage::CliMissingSubcommand),
        ErrorKind::ArgumentConflict if !argument.is_empty() => {
            localizer.format(UiMessage::CliArgumentConflict {
                argument: &argument,
            })
        }
        ErrorKind::TooFewValues | ErrorKind::NoEquals => {
            if argument.is_empty() {
                localizer.format(UiMessage::CliParseFailure)
            } else {
                localizer.format(UiMessage::CliMissingValue {
                    argument: &argument,
                })
            }
        }
        ErrorKind::TooManyValues | ErrorKind::WrongNumberOfValues => {
            if argument.is_empty() {
                localizer.format(UiMessage::CliParseFailure)
            } else {
                localizer.format(UiMessage::CliWrongNumberOfValues {
                    argument: &argument,
                })
            }
        }
        ErrorKind::InvalidUtf8 => localizer.format(UiMessage::CliInvalidUtf8),
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::DisplayVersion
        | ErrorKind::Io
        | ErrorKind::Format => localizer.format(UiMessage::CliParseFailure),
        _ => localizer.format(UiMessage::CliParseFailure),
    }
}

fn error_context(error: &clap::Error, kind: ContextKind) -> Option<String> {
    error.get(kind).map(ToString::to_string)
}

fn localized_usage(usage: &str, localizer: &UiLocalizer) -> String {
    let usage = usage.trim();
    let syntax = usage
        .strip_prefix("Usage:")
        .map(str::trim_start)
        .unwrap_or(usage);
    format!(
        "{} {}",
        localizer.format(UiMessage::CliUsageHeading),
        syntax
    )
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

fn parse_language_id(value: &str) -> Result<LanguageId, String> {
    LanguageId::parse(value).map_err(|error| error.to_string())
}

fn parse_max_fullwidth_chars(value: &str) -> Result<MaxFullwidthChars, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| "每行最大全角字符数必须是正整数".to_owned())?;
    MaxFullwidthChars::new(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::i18n::UiLocale;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn command_schema_is_self_consistent() {
        AttArguments::command().debug_assert();
    }

    #[test]
    fn custom_configuration_path_is_rejected_as_an_unknown_argument() {
        for arguments in [
            vec![
                "att",
                "--config",
                "settings/config.toml",
                "mz",
                "write-back",
                "--name",
                "demo",
            ],
            vec![
                "att",
                "mz",
                "--config",
                "settings/config.toml",
                "write-back",
                "--name",
                "demo",
            ],
        ] {
            let error = AttArguments::try_parse_from(arguments)
                .expect_err("固定发行目录模式不得接受自定义配置路径");
            assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn extract_without_explicit_owner_is_preserved_for_project_state_resolution() {
        let parsed = AttArguments::try_parse_from(["att", "mz", "extract", "--name", "demo"])
            .expect("省略 owner 应交给项目状态解析");
        let MzCommand::Extract(arguments) = expect_mz(parsed.product) else {
            panic!("应解析为提取命令");
        };
        assert!(!arguments.builtin);
        assert!(arguments.rules.is_none());
    }

    #[test]
    fn ordinary_commands_parse_without_a_configuration_argument() {
        let parsed = AttArguments::try_parse_from(["att", "mz", "write-back", "--name", "demo"])
            .expect("配置位置由发行目录确定，不属于命令意图");
        assert!(matches!(expect_mz(parsed.product), MzCommand::WriteBack(_)));
    }

    #[test]
    fn translate_preserves_exact_profile_id_and_paths() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "mz",
            "translate",
            "--name",
            "demo",
            "Profile-A",
            "--terms",
            "input/terms.toml",
            "--placeholders",
            "input/placeholders.toml",
        ])
        .expect("翻译参数应合法");

        let MzCommand::Translate(arguments) = expect_mz(parsed.product) else {
            panic!("应解析为翻译命令");
        };
        assert_eq!(arguments.profile_id.as_deref(), Some("Profile-A"));
        assert_eq!(
            arguments.terms.as_deref(),
            Some(Path::new("input/terms.toml"))
        );
        assert_eq!(
            arguments.placeholders.as_deref(),
            Some(Path::new("input/placeholders.toml"))
        );
    }

    #[test]
    fn project_lua_preserves_script_and_delimited_arguments() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "mz",
            "lua",
            "--name",
            "demo",
            "scripts/manual.lua",
            "--",
            "--replace",
            "值",
        ])
        .expect("项目 Lua 参数应合法");

        let MzCommand::Lua(arguments) = expect_mz(parsed.product) else {
            panic!("应解析为项目 Lua 命令");
        };
        assert_eq!(arguments.project.name.as_str(), "demo");
        assert_eq!(arguments.script.as_path(), Path::new("scripts/manual.lua"));
        assert_eq!(arguments.arguments, ["--replace", "值"]);
    }

    #[test]
    fn stage_lua_and_project_lua_profile_are_rejected() {
        for arguments in [
            vec![
                "att",
                "mz",
                "extract",
                "--name",
                "demo",
                "--lua",
                "stage.lua",
            ],
            vec![
                "att",
                "mz",
                "translate",
                "--name",
                "demo",
                "--lua",
                "stage.lua",
            ],
            vec![
                "att",
                "mz",
                "write-back",
                "--name",
                "demo",
                "--lua",
                "stage.lua",
            ],
            vec![
                "att",
                "mz",
                "lua",
                "--name",
                "demo",
                "--profile",
                "primary",
                "script.lua",
            ],
        ] {
            let error = AttArguments::try_parse_from(arguments)
                .expect_err("已删除的 Lua 参数必须作为普通未知参数拒绝");
            assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn generic_commands_preserve_live_input_and_translation_options() {
        let init = AttArguments::try_parse_from([
            "att",
            "generic",
            "init",
            "--name",
            "demo",
            "--path",
            "jsonl",
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
        ])
        .expect("Generic Init 参数应合法");
        let ProductCommand::Generic {
            command: GenericCommand::Init(arguments),
        } = init.product
        else {
            panic!("应解析为 Generic Init");
        };
        assert_eq!(arguments.path.as_deref(), Some(Path::new("jsonl")));
        assert_eq!(
            arguments.source_language.as_ref().map(LanguageId::as_str),
            Some("ja")
        );
        assert_eq!(
            arguments.target_language.as_ref().map(LanguageId::as_str),
            Some("zh-Hans")
        );

        let translate = AttArguments::try_parse_from([
            "att",
            "generic",
            "translate",
            "--name",
            "demo",
            "primary",
            "--terms",
            "terms.toml",
            "--placeholders",
            "placeholders.toml",
        ])
        .expect("Generic Translate 参数应合法");
        assert!(matches!(
            translate.product,
            ProductCommand::Generic {
                command: GenericCommand::Translate(_)
            }
        ));
    }

    #[test]
    fn project_lua_requires_a_script_and_delimits_script_arguments() {
        let missing_script = AttArguments::try_parse_from(["att", "mv", "lua", "--name", "demo"])
            .expect_err("项目 Lua 必须提供脚本");
        assert_eq!(missing_script.kind(), ErrorKind::MissingRequiredArgument);

        let unexpected_argument = AttArguments::try_parse_from([
            "att",
            "mv",
            "lua",
            "--name",
            "demo",
            "script.lua",
            "argument-without-delimiter",
        ])
        .expect_err("脚本参数必须位于 -- 之后");
        assert_eq!(unexpected_argument.kind(), ErrorKind::UnknownArgument);
    }

    #[cfg(windows)]
    #[test]
    fn project_lua_rejects_non_utf8_delimited_arguments_before_execution() {
        let invalid_argument = OsString::from_wide(&[0xD800]);
        let arguments = [
            OsString::from("att"),
            OsString::from("mz"),
            OsString::from("lua"),
            OsString::from("--name"),
            OsString::from("demo"),
            OsString::from("script.lua"),
            OsString::from("--"),
            invalid_argument,
        ];

        let error = AttArguments::try_parse_localized_from(arguments)
            .expect_err("项目 Lua 参数不能无损表示为 UTF-8 时必须在执行前失败");
        assert_eq!(error.kind(), ErrorKind::InvalidUtf8);
    }

    #[test]
    fn init_accepts_only_project_and_game_path() {
        let parsed =
            AttArguments::try_parse_from(["att", "mz", "init", "--name", "demo", "--path", "game"])
                .expect("后续 Init 应允许复用已经保存的语言和布局");
        let MzCommand::Init(arguments) = expect_mz(parsed.product) else {
            panic!("应解析为 Init 命令");
        };
        assert_eq!(arguments.path.as_deref(), Some(Path::new("game")));
        assert!(arguments.source_language.is_none());
        assert!(arguments.target_language.is_none());
        assert!(arguments.dialogue_max_fullwidth_chars.is_none());
        assert!(arguments.scrolling_text_max_fullwidth_chars.is_none());
        assert!(arguments.help_description_max_fullwidth_chars.is_none());
    }

    #[test]
    fn init_and_translate_allow_project_state_reuse() {
        let init = AttArguments::try_parse_from(["att", "mz", "init", "--name", "demo"])
            .expect("后续 Init 可省略来源路径");
        let MzCommand::Init(arguments) = expect_mz(init.product) else {
            panic!("应解析为 Init 命令");
        };
        assert!(arguments.path.is_none());

        let translate = AttArguments::try_parse_from(["att", "mz", "translate", "--name", "demo"])
            .expect("后续 Translate 可省略 Profile");
        let MzCommand::Translate(arguments) = expect_mz(translate.product) else {
            panic!("应解析为 Translate 命令");
        };
        assert!(arguments.profile_id.is_none());
    }

    #[test]
    fn parses_global_ui_language_without_progress_choice() {
        let (parsed, resolved) = AttArguments::try_parse_localized_from([
            "att",
            "--ui-language",
            "zh-Hant",
            "mz",
            "write-back",
            "--name",
            "demo",
        ])
        .expect("全局界面选项应可解析");
        assert_eq!(resolved.locale(), UiLocale::TraditionalChinese);
        assert!(matches!(
            parsed.product,
            ProductCommand::Mz {
                command: MzCommand::WriteBack(_)
            }
        ));
    }

    #[test]
    fn localized_parser_renders_root_and_subcommand_help_in_all_supported_locales() {
        for locale in UiLocale::ALL {
            let root_error = AttArguments::try_parse_localized_from([
                "att",
                "--ui-language",
                locale.as_str(),
                "--help",
            ])
            .expect_err("Help 应作为正常提前退出返回");
            let localizer = UiLocalizer::new(locale);
            assert_eq!(root_error.kind(), ErrorKind::DisplayHelp);
            assert_eq!(root_error.exit_code(), 0);
            assert!(!root_error.use_stderr());
            assert!(!root_error.output().contains("--progress"));
            for expected in [
                localizer.format(UiMessage::AppAbout),
                localizer.format(UiMessage::CliUsageHeading),
                localizer.format(UiMessage::CliCommandsHeading),
                localizer.format(UiMessage::CliOptionsHeading),
                localizer.format(UiMessage::CliPrintHelp),
            ] {
                assert!(
                    root_error.output().contains(&expected),
                    "{} 根帮助缺少 {expected:?}:\n{}",
                    locale,
                    root_error.output()
                );
            }

            let init_error = AttArguments::try_parse_localized_from([
                "att",
                "mz",
                "init",
                "--ui-language",
                locale.as_str(),
                "--help",
            ])
            .expect_err("子命令 Help 应作为正常提前退出返回");
            assert!(!init_error.output().contains("--progress"));
            for expected in [
                localizer.format(UiMessage::CliInitAbout),
                localizer.format(UiMessage::CliUsageHeading),
                localizer.format(UiMessage::CliOptionsHeading),
                localizer.format(UiMessage::CliProjectNameHelp),
                localizer.format(UiMessage::CliInitPathHelp),
                localizer.format(UiMessage::CliSourceLanguageHelp),
            ] {
                assert!(
                    init_error.output().contains(&expected),
                    "{} Init 帮助缺少 {expected:?}:\n{}",
                    locale,
                    init_error.output()
                );
            }
        }

        let chinese =
            AttArguments::try_parse_localized_from(["att", "--ui-language", "zh-Hans", "--help"])
                .expect_err("Help 应作为正常提前退出返回");
        assert!(chinese.output().contains("[选项]"));
        assert!(chinese.output().contains("<命令>"));
        assert!(!chinese.output().contains("[OPTIONS]"));
        assert!(!chinese.output().contains("<COMMAND>"));

        let arabic =
            AttArguments::try_parse_localized_from(["att", "--ui-language", "ar", "--help"])
                .expect_err("Help 应作为正常提前退出返回");
        assert!(arabic.output().contains('\u{2068}'));
        assert!(arabic.output().contains('\u{2069}'));
    }

    #[test]
    fn removed_progress_argument_is_a_localized_unknown_argument() {
        let error = AttArguments::try_parse_localized_from([
            "att",
            "--ui-language",
            "fr",
            "--progress",
            "fast",
            "mz",
            "write-back",
            "--name",
            "demo",
        ])
        .expect_err("已删除的 progress 参数必须被拒绝");
        let localizer = UiLocalizer::new(UiLocale::French);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert_eq!(error.exit_code(), 2);
        assert!(error.use_stderr());
        assert!(
            error
                .output()
                .contains(&localizer.format(UiMessage::CliUnexpectedArgument {
                    value: "--progress",
                }))
        );
        assert!(
            error
                .output()
                .contains(&localizer.format(UiMessage::CliUsageHeading))
        );
        assert!(
            error
                .output()
                .contains(&localizer.format(UiMessage::CliTryHelp))
        );
        assert!(!error.output().contains("Usage:"));
    }

    #[test]
    fn ui_language_scan_failure_preserves_the_earlier_clap_root_error() {
        let error = AttArguments::try_parse_localized_from([
            "att",
            "--definitely-unknown",
            "--ui-language",
        ])
        .expect_err("未知参数必须优先于后续 UI locale 预扫描失败");

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(
            error.output().contains("--definitely-unknown"),
            "必须呈现 Clap 依据真实 schema 选择的根错误：{}",
            error.output()
        );
        assert!(
            !error
                .output()
                .contains(&UiLocalizer::new(UiLocale::English).format(
                    UiMessage::CliMissingValue {
                        argument: "--ui-language",
                    }
                )),
            "预扫描不得覆盖更早的 Clap 根错误"
        );
    }

    #[test]
    fn localized_parser_rejects_removed_config_argument_in_the_selected_language() {
        let error = AttArguments::try_parse_localized_from([
            "att",
            "--ui-language",
            "ja",
            "--config",
            "config.toml",
            "mz",
            "write-back",
            "--name",
            "demo",
        ])
        .expect_err("已移除的配置参数必须作为未知参数拒绝");
        let localizer = UiLocalizer::new(UiLocale::Japanese);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(
            error.output().contains(
                &localizer.format(UiMessage::CliUnexpectedArgument { value: "--config" })
            )
        );
    }

    #[test]
    fn invalid_explicit_ui_language_is_a_localized_input_error() {
        let error =
            AttArguments::try_parse_localized_from(["att", "--ui-language=de-DE", "--help"])
                .expect_err("不支持的显式 locale 必须优先于 Help 报错");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
        assert!(UiLocale::ALL.into_iter().any(|locale| {
            error.output().contains(
                &UiLocalizer::new(locale)
                    .format(UiMessage::CliUnsupportedUiLanguageArgument { value: "de-DE" }),
            )
        }));
    }

    #[test]
    fn init_parses_each_explicit_override_independently() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "mz",
            "init",
            "--name",
            "demo",
            "--path",
            "game",
            "--source-language",
            "JA",
            "--dialogue-max-fullwidth-chars",
            "24",
        ])
        .expect("Init 覆盖值应可以逐项提供");
        let MzCommand::Init(arguments) = expect_mz(parsed.product) else {
            panic!("应解析为 Init 命令");
        };
        assert_eq!(
            arguments.source_language.as_ref().map(LanguageId::as_str),
            Some("ja")
        );
        assert_eq!(
            arguments
                .dialogue_max_fullwidth_chars
                .map(MaxFullwidthChars::get),
            Some(24)
        );
        assert!(arguments.target_language.is_none());
        assert!(arguments.scrolling_text_max_fullwidth_chars.is_none());
        assert!(arguments.help_description_max_fullwidth_chars.is_none());
    }

    #[test]
    fn init_rejects_language_ids_with_surrounding_whitespace() {
        let error = AttArguments::try_parse_from([
            "att",
            "mz",
            "init",
            "--name",
            "demo",
            "--path",
            "game",
            "--source-language",
            " ja ",
        ])
        .expect_err("语言 ID 的首尾空白不得被静默裁剪");

        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn mv_exposes_all_project_command_domains() {
        for command in ["init", "extract", "translate", "write-back", "lua"] {
            let mut arguments = vec!["att", "mv", command, "--name", "demo"];
            match command {
                "init" => arguments.extend(["--path", "game"]),
                "extract" => arguments.push("--builtin"),
                "translate" => arguments.push("profile"),
                "write-back" => {}
                "lua" => arguments.push("script.lua"),
                _ => unreachable!(),
            }
            let parsed = AttArguments::try_parse_from(arguments).expect("MV 项目命令参数应合法");
            assert!(matches!(parsed.product, ProductCommand::Mv { .. }));
        }
    }

    #[test]
    fn mv_dialogue_rules_require_builtin_extraction() {
        let error = AttArguments::try_parse_from([
            "att",
            "mv",
            "extract",
            "--name",
            "demo",
            "--rules",
            "rules.toml",
            "--dialogue-rules",
            "dialogue.toml",
        ])
        .expect_err("姓名投影只能作为 Builtin 对话提取的输入");

        assert_eq!(error.exit_code(), 2);
    }

    fn expect_mz(product: ProductCommand) -> MzCommand {
        match product {
            ProductCommand::Mz { command } => command,
            ProductCommand::Mv { .. } => panic!("应解析为 MZ 命令"),
            ProductCommand::Generic { .. } => panic!("应解析为 MZ 命令"),
        }
    }
}
