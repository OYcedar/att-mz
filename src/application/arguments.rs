//! `att` 进程入口的纯参数契约。
//!
//! 本模块只把命令行转换为用户意图，不构造运行时、读取配置或执行业务。

use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::att_mz::{MaxFullwidthChars, ProjectName};
use crate::language::LanguageId;

/// 已确认显式配置路径的 ATT 进程参数。
#[derive(Debug)]
pub(crate) struct AttArguments {
    pub(crate) config: PathBuf,
    pub(crate) product: ProductCommand,
}

impl AttArguments {
    pub(crate) fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let raw = RawAttArguments::try_parse_from(arguments)?;
        let Some(config) = raw.config else {
            return Err(RawAttArguments::command().error(
                ErrorKind::MissingRequiredArgument,
                "缺少必需的配置路径 `--config <FILE>`",
            ));
        };
        Ok(Self {
            config,
            product: raw.product,
        })
    }

    #[cfg(test)]
    pub(crate) fn command() -> clap::Command {
        RawAttArguments::command()
    }
}

/// Clap 解析阶段允许全局参数在子命令前后出现，再由上方边界建立必填不变量。
#[derive(Debug, Parser)]
#[command(name = "att", bin_name = "att", about = "游戏翻译工具", version)]
struct RawAttArguments {
    /// 本次进程使用的严格 TOML 配置文件。
    ///
    /// 相对路径以进程当前工作目录为基准。
    #[arg(long, global = true, value_name = "FILE", value_parser = parse_non_blank_path)]
    config: Option<PathBuf>,

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
}

impl ProductCommand {
    pub(crate) fn into_mz(self) -> MzCommand {
        match self {
            Self::Mz { command } => command,
        }
    }
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
}

#[derive(Debug, Args)]
pub(crate) struct InitArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// RPG Maker MZ 游戏根目录。
    #[arg(long, value_name = "DIR", value_parser = parse_non_blank_path)]
    pub(crate) path: PathBuf,
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
#[command(group(
    clap::ArgGroup::new("extract_tasks")
        .required(true)
        .multiple(true)
        .args(["builtin", "rules", "lua"])
))]
pub(crate) struct ExtractArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 使用内置 MZ 文本位置规格。
    #[arg(long)]
    pub(crate) builtin: bool,
    /// 按指定 JSON 规则提取外部明确声明的位置。
    #[arg(long, value_name = "RULES_JSON", value_parser = parse_non_blank_path)]
    pub(crate) rules: Option<PathBuf>,
    /// 运行指定可信 Lua 程序。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    pub(crate) lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct TranslateArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 配置文件中要使用的翻译 Profile ID。
    #[arg(value_name = "PROFILE_ID", value_parser = parse_non_blank)]
    pub(crate) profile_id: String,
    /// 用该 JSON 文件替换项目当前术语表；省略时复用已保存内容。
    #[arg(long, value_name = "TERMS_JSON", value_parser = parse_non_blank_path)]
    pub(crate) terms: Option<PathBuf>,
    /// 用该 JSON 文件替换项目当前占位符规则；省略时复用已保存内容。
    #[arg(long, value_name = "PLACEHOLDERS_JSON", value_parser = parse_non_blank_path)]
    pub(crate) placeholders: Option<PathBuf>,
    /// 运行指定可信 Lua 程序。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    pub(crate) lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct WriteBackArguments {
    #[command(flatten)]
    pub(crate) project: ProjectArguments,
    /// 运行指定可信 Lua 程序。
    #[arg(long, value_name = "SCRIPT_LUA", value_parser = parse_non_blank_path)]
    pub(crate) lua: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct ProjectArguments {
    /// MZ 游戏的稳定项目名称。
    #[arg(long, value_name = "NAME")]
    pub(crate) name: ProjectName,
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

    #[test]
    fn command_schema_is_self_consistent() {
        AttArguments::command().debug_assert();
    }

    #[test]
    fn global_config_is_accepted_before_or_after_mz() {
        for arguments in [
            [
                "att",
                "--config",
                "settings/config.toml",
                "mz",
                "write-back",
                "--name",
                "demo",
            ],
            [
                "att",
                "mz",
                "--config",
                "settings/config.toml",
                "write-back",
                "--name",
                "demo",
            ],
        ] {
            let parsed = AttArguments::try_parse_from(arguments).expect("参数应合法");
            assert_eq!(parsed.config.as_path(), Path::new("settings/config.toml"));
            assert!(matches!(parsed.product.into_mz(), MzCommand::WriteBack(_)));
        }
    }

    #[test]
    fn extract_requires_at_least_one_explicit_task() {
        let error = AttArguments::try_parse_from([
            "att",
            "--config",
            "config.toml",
            "mz",
            "extract",
            "--name",
            "demo",
        ])
        .expect_err("空提取选择必须拒绝");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn ordinary_commands_require_explicit_configuration() {
        let error = AttArguments::try_parse_from(["att", "mz", "write-back", "--name", "demo"])
            .expect_err("普通命令不得推断默认配置路径");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn translate_preserves_exact_profile_id_and_paths() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "--config",
            "config.toml",
            "mz",
            "translate",
            "--name",
            "demo",
            "Profile-A",
            "--terms",
            "input/terms.json",
            "--placeholders",
            "input/placeholders.json",
            "--lua",
            "scripts/translate.lua",
        ])
        .expect("翻译参数应合法");

        let MzCommand::Translate(arguments) = parsed.product.into_mz() else {
            panic!("应解析为翻译命令");
        };
        assert_eq!(arguments.profile_id, "Profile-A");
        assert_eq!(
            arguments.terms.as_deref(),
            Some(Path::new("input/terms.json"))
        );
        assert_eq!(
            arguments.placeholders.as_deref(),
            Some(Path::new("input/placeholders.json"))
        );
        assert_eq!(
            arguments.lua.as_deref(),
            Some(Path::new("scripts/translate.lua"))
        );
    }

    #[test]
    fn init_accepts_only_project_and_game_path() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "--config",
            "config.toml",
            "mz",
            "init",
            "--name",
            "demo",
            "--path",
            "game",
        ])
        .expect("后续 Init 应允许复用已经保存的语言和布局");
        let MzCommand::Init(arguments) = parsed.product.into_mz() else {
            panic!("应解析为 Init 命令");
        };
        assert_eq!(arguments.path, Path::new("game"));
        assert!(arguments.source_language.is_none());
        assert!(arguments.target_language.is_none());
        assert!(arguments.dialogue_max_fullwidth_chars.is_none());
        assert!(arguments.scrolling_text_max_fullwidth_chars.is_none());
        assert!(arguments.help_description_max_fullwidth_chars.is_none());
    }

    #[test]
    fn init_parses_each_explicit_override_independently() {
        let parsed = AttArguments::try_parse_from([
            "att",
            "--config",
            "config.toml",
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
        let MzCommand::Init(arguments) = parsed.product.into_mz() else {
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
            "--config",
            "config.toml",
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
}
