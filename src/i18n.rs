//! 进程级用户界面语言选择与本地化消息。
//!
//! 游戏内容语言是开放领域值，而用户界面只支持本模块声明的闭集。所有外部文本都在
//! 进入 Fluent 前移除终端转义、换行伪装和既有双向文本控制符；Fluent 再为动态值
//! 添加方向隔离，因此阿拉伯语消息保持逻辑顺序且不会反转路径、ID 或数字。

use std::env;
use std::error::Error;
use std::fmt;

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use unic_langid::LanguageIdentifier;
use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

use crate::user_text::sanitize_user_text;

pub(crate) const ATT_UI_LANGUAGE_ENV: &str = "ATT_UI_LANGUAGE";

/// ATT 支持的进程级用户界面语言。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum UiLocale {
    Arabic,
    SimplifiedChinese,
    TraditionalChinese,
    English,
    French,
    Russian,
    Spanish,
    Japanese,
    Korean,
    Vietnamese,
}

impl UiLocale {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 10] = [
        Self::Arabic,
        Self::SimplifiedChinese,
        Self::TraditionalChinese,
        Self::English,
        Self::French,
        Self::Russian,
        Self::Spanish,
        Self::Japanese,
        Self::Korean,
        Self::Vietnamese,
    ];

    /// 返回日志和 CLI 契约使用的规范 BCP 47 标签。
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Arabic => "ar",
            Self::SimplifiedChinese => "zh-Hans",
            Self::TraditionalChinese => "zh-Hant",
            Self::English => "en",
            Self::French => "fr",
            Self::Russian => "ru",
            Self::Spanish => "es",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::Vietnamese => "vi",
        }
    }

    #[cfg(test)]
    pub(crate) const fn text_direction(self) -> UiTextDirection {
        match self {
            Self::Arabic => UiTextDirection::RightToLeft,
            Self::SimplifiedChinese
            | Self::TraditionalChinese
            | Self::English
            | Self::French
            | Self::Russian
            | Self::Spanish
            | Self::Japanese
            | Self::Korean
            | Self::Vietnamese => UiTextDirection::LeftToRight,
        }
    }

    /// 严格解析由用户显式指定的 UI locale。
    #[cfg(test)]
    pub(crate) fn parse_explicit(value: &str) -> Result<Self, UiLocaleSelectionError> {
        parse_explicit_locale(value, UiLocaleInputSource::CommandLine)
    }

    /// 宽容匹配自动探测候选；无效或不支持的候选返回 `None`。
    pub(crate) fn match_automatic(value: &str) -> Option<Self> {
        parse_language_identifier(value).and_then(|identifier| match_supported(&identifier))
    }

    fn language_identifier(self) -> LanguageIdentifier {
        self.as_str()
            .parse()
            .expect("UiLocale 的规范标签必须是有效 LanguageIdentifier")
    }

    const fn catalog(self) -> &'static str {
        match self {
            Self::Arabic => include_str!("i18n/ar.ftl"),
            Self::SimplifiedChinese => include_str!("i18n/zh-Hans.ftl"),
            Self::TraditionalChinese => include_str!("i18n/zh-Hant.ftl"),
            Self::English => include_str!("i18n/en.ftl"),
            Self::French => include_str!("i18n/fr.ftl"),
            Self::Russian => include_str!("i18n/ru.ftl"),
            Self::Spanish => include_str!("i18n/es.ftl"),
            Self::Japanese => include_str!("i18n/ja.ftl"),
            Self::Korean => include_str!("i18n/ko.ftl"),
            Self::Vietnamese => include_str!("i18n/vi.ftl"),
        }
    }
}

impl fmt::Display for UiLocale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiTextDirection {
    LeftToRight,
    RightToLeft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiLocaleSource {
    CommandLine,
    Environment,
    Windows,
    ProductDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedUiLocale {
    locale: UiLocale,
    source: UiLocaleSource,
}

impl ResolvedUiLocale {
    pub(crate) const fn locale(self) -> UiLocale {
        self.locale
    }

    #[cfg(test)]
    pub(crate) const fn source(self) -> UiLocaleSource {
        self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiLocaleInputSource {
    CommandLine,
    Environment,
}

/// 显式 UI locale 无法建立时的输入错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UiLocaleSelectionError {
    InvalidLanguageTag {
        input: UiLocaleInputSource,
        value: String,
    },
    UnsupportedLanguage {
        input: UiLocaleInputSource,
        value: String,
    },
    EnvironmentNotUnicode,
}

impl UiLocaleSelectionError {
    pub(crate) fn ui_message(&self) -> UiMessage<'_> {
        match self {
            Self::InvalidLanguageTag {
                input: UiLocaleInputSource::CommandLine,
                value,
            } => UiMessage::CliInvalidUiLanguageArgument { value },
            Self::UnsupportedLanguage {
                input: UiLocaleInputSource::CommandLine,
                value,
            } => UiMessage::CliUnsupportedUiLanguageArgument { value },
            Self::InvalidLanguageTag {
                input: UiLocaleInputSource::Environment,
                value,
            } => UiMessage::CliInvalidUiLanguageEnvironment { value },
            Self::UnsupportedLanguage {
                input: UiLocaleInputSource::Environment,
                value,
            } => UiMessage::CliUnsupportedUiLanguageEnvironment { value },
            Self::EnvironmentNotUnicode => UiMessage::CliUiLanguageEnvironmentNotUnicode,
        }
    }
}

impl fmt::Display for UiLocaleSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLanguageTag { input, value } => write!(
                formatter,
                "{} contains an invalid UI language tag: {}",
                input.english_name(),
                sanitize_user_text(value)
            ),
            Self::UnsupportedLanguage { input, value } => write!(
                formatter,
                "{} requests an unsupported UI language: {}",
                input.english_name(),
                sanitize_user_text(value)
            ),
            Self::EnvironmentNotUnicode => {
                formatter.write_str("ATT_UI_LANGUAGE is not valid Unicode")
            }
        }
    }
}

impl Error for UiLocaleSelectionError {}

impl UiLocaleInputSource {
    const fn english_name(self) -> &'static str {
        match self {
            Self::CommandLine => "--ui-language",
            Self::Environment => ATT_UI_LANGUAGE_ENV,
        }
    }
}

/// 解析调用方提供的三个优先级层级，不访问进程环境，便于组合根和测试复用。
pub(crate) fn select_ui_locale<I, S>(
    command_line: Option<&str>,
    environment: Option<&str>,
    windows_candidates: I,
) -> Result<ResolvedUiLocale, UiLocaleSelectionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if let Some(value) = command_line {
        return parse_explicit_locale(value, UiLocaleInputSource::CommandLine).map(|locale| {
            ResolvedUiLocale {
                locale,
                source: UiLocaleSource::CommandLine,
            }
        });
    }
    if let Some(value) = environment {
        return parse_explicit_locale(value, UiLocaleInputSource::Environment).map(|locale| {
            ResolvedUiLocale {
                locale,
                source: UiLocaleSource::Environment,
            }
        });
    }
    if let Some(locale) = windows_candidates
        .into_iter()
        .find_map(|candidate| UiLocale::match_automatic(candidate.as_ref()))
    {
        return Ok(ResolvedUiLocale {
            locale,
            source: UiLocaleSource::Windows,
        });
    }
    Ok(ResolvedUiLocale {
        locale: UiLocale::English,
        source: UiLocaleSource::ProductDefault,
    })
}

/// 按 CLI、环境变量、Windows 用户首选 UI 语言、英语的顺序解析运行语言。
pub(crate) fn resolve_ui_locale(
    command_line: Option<&str>,
) -> Result<ResolvedUiLocale, UiLocaleSelectionError> {
    if let Some(value) = command_line {
        return parse_explicit_locale(value, UiLocaleInputSource::CommandLine).map(|locale| {
            ResolvedUiLocale {
                locale,
                source: UiLocaleSource::CommandLine,
            }
        });
    }
    let environment = match env::var_os(ATT_UI_LANGUAGE_ENV) {
        Some(value) => Some(
            value
                .into_string()
                .map_err(|_| UiLocaleSelectionError::EnvironmentNotUnicode)?,
        ),
        None => None,
    };
    if let Some(value) = environment.as_deref() {
        return parse_explicit_locale(value, UiLocaleInputSource::Environment).map(|locale| {
            ResolvedUiLocale {
                locale,
                source: UiLocaleSource::Environment,
            }
        });
    }
    select_ui_locale(None, None, windows_user_preferred_ui_languages())
}

/// 为更高优先级输入本身的错误选择诊断语言。
///
/// 例如 `--ui-language` 无效时不能使用该值渲染错误，因此从环境变量、Windows
/// 首选语言和英语中选择；环境变量无效时则从 Windows 首选语言和英语中选择。低优先级
/// 候选仅用于呈现当前错误，其无效值会被跳过，避免遮蔽真正的高优先级错误。
pub(crate) fn resolve_lower_priority_ui_locale(
    failed_input: UiLocaleInputSource,
) -> ResolvedUiLocale {
    if failed_input == UiLocaleInputSource::CommandLine
        && let Some(value) = env::var_os(ATT_UI_LANGUAGE_ENV)
        && let Ok(value) = value.into_string()
        && let Some(locale) = UiLocale::match_automatic(&value)
    {
        return ResolvedUiLocale {
            locale,
            source: UiLocaleSource::Environment,
        };
    }

    select_ui_locale(None, None, windows_user_preferred_ui_languages())
        .expect("自动 UI locale 候选只会回退，不会返回错误")
}

fn parse_explicit_locale(
    value: &str,
    input: UiLocaleInputSource,
) -> Result<UiLocale, UiLocaleSelectionError> {
    let Some(identifier) = parse_language_identifier(value) else {
        return Err(UiLocaleSelectionError::InvalidLanguageTag {
            input,
            value: value.to_owned(),
        });
    };
    match_supported(&identifier).ok_or_else(|| UiLocaleSelectionError::UnsupportedLanguage {
        input,
        value: value.to_owned(),
    })
}

fn parse_language_identifier(value: &str) -> Option<LanguageIdentifier> {
    if value.is_empty() || value.trim() != value {
        return None;
    }
    value.parse().ok()
}

fn match_supported(identifier: &LanguageIdentifier) -> Option<UiLocale> {
    match identifier.language.as_str() {
        "ar" => Some(UiLocale::Arabic),
        "zh" => {
            let traditional_script = identifier
                .script
                .is_some_and(|script| script.as_str().eq_ignore_ascii_case("Hant"));
            let traditional_region = identifier
                .region
                .is_some_and(|region| matches!(region.as_str(), "TW" | "HK" | "MO"));
            if traditional_script || traditional_region {
                Some(UiLocale::TraditionalChinese)
            } else {
                Some(UiLocale::SimplifiedChinese)
            }
        }
        "en" => Some(UiLocale::English),
        "fr" => Some(UiLocale::French),
        "ru" => Some(UiLocale::Russian),
        "es" => Some(UiLocale::Spanish),
        "ja" => Some(UiLocale::Japanese),
        "ko" => Some(UiLocale::Korean),
        "vi" => Some(UiLocale::Vietnamese),
        _ => None,
    }
}

fn windows_user_preferred_ui_languages() -> Vec<String> {
    let mut language_count = 0_u32;
    let mut buffer_length = 0_u32;
    // SAFETY: 第一次调用按 Win32 契约使用空缓冲区查询所需 UTF-16 单元数，两个输出指针
    // 均指向本函数内有效的 `u32`。
    let size_query_succeeded = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut language_count,
            std::ptr::null_mut(),
            &mut buffer_length,
        )
    } != 0;
    if !size_query_succeeded || buffer_length == 0 {
        return Vec::new();
    }

    let mut buffer = vec![0_u16; buffer_length as usize];
    // SAFETY: `buffer` 至少包含上一次调用报告的 UTF-16 单元数，指针在调用期间有效且
    // 可写；其余输出指针仍指向有效的局部 `u32`。
    let succeeded = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut language_count,
            buffer.as_mut_ptr(),
            &mut buffer_length,
        )
    } != 0;
    if !succeeded {
        return Vec::new();
    }

    buffer
        .split(|unit| *unit == 0)
        .filter(|candidate| !candidate.is_empty())
        .take(language_count as usize)
        .filter_map(|candidate| String::from_utf16(candidate).ok())
        .collect()
}

/// Fluent 消息的类型化调用面；字符串字段一律按不可信外部文本处理。
#[derive(Clone, Copy, Debug)]
pub(crate) enum UiMessage<'a> {
    AppAbout,
    CliConfigHelp,
    CliUiLanguageHelp,
    CliProgressHelp,
    CliMzAbout,
    CliMvAbout,
    CliInitAbout,
    CliExtractAbout,
    CliTranslateAbout,
    CliWriteBackAbout,
    CliProjectNameHelp,
    CliInitPathHelp,
    CliSourceLanguageHelp,
    CliTargetLanguageHelp,
    CliDialogueWidthHelp,
    CliScrollingWidthHelp,
    CliHelpWidthHelp,
    CliBuiltinHelp,
    CliRulesHelp,
    CliDialogueRulesHelp,
    CliLuaHelp,
    CliProfileHelp,
    CliTermsHelp,
    CliPlaceholdersHelp,
    CliUsageHeading,
    CliCommandsHeading,
    CliOptionsHeading,
    CliArgumentsHeading,
    CliOptionsMetavar,
    CliCommandMetavar,
    CliPrintHelp,
    CliPrintVersion,
    CliMissingConfig,
    CliBlankValue,
    CliInvalidPositiveInteger,
    CliInvalidProgress {
        value: &'a str,
    },
    CliInvalidUiLanguageArgument {
        value: &'a str,
    },
    CliUnsupportedUiLanguageArgument {
        value: &'a str,
    },
    CliInvalidUiLanguageEnvironment {
        value: &'a str,
    },
    CliUnsupportedUiLanguageEnvironment {
        value: &'a str,
    },
    CliUiLanguageEnvironmentNotUnicode,
    CliUnexpectedArgument {
        value: &'a str,
    },
    CliMissingRequiredArgument {
        value: &'a str,
    },
    CliInvalidValue {
        value: &'a str,
        argument: &'a str,
    },
    CliErrorHeading,
    CliTryHelp,
    CliMissingValue {
        argument: &'a str,
    },
    CliMissingSubcommand,
    CliArgumentConflict {
        argument: &'a str,
    },
    CliWrongNumberOfValues {
        argument: &'a str,
    },
    CliInvalidUtf8,
    CliParseFailure,
    ErrorConfigurationOrInputGeneric,
    ErrorConfigCurrentDirectoryNotAbsolute {
        path: &'a str,
    },
    ErrorConfigEmptyPath,
    ErrorConfigOpen {
        path: &'a str,
    },
    ErrorConfigNotAFile {
        path: &'a str,
    },
    ErrorConfigTooLarge {
        path: &'a str,
        observed_bytes: u64,
        maximum_bytes: u64,
    },
    ErrorConfigRead {
        path: &'a str,
    },
    ErrorConfigInvalidUtf8KnownLength {
        path: &'a str,
        valid_up_to: u64,
        error_len: u64,
    },
    ErrorConfigInvalidUtf8UnknownLength {
        path: &'a str,
        valid_up_to: u64,
    },
    ErrorConfigInvalidTomlAt {
        path: &'a str,
        line: u64,
        column: u64,
        resource: &'a str,
    },
    ErrorConfigInvalidToml {
        path: &'a str,
        resource: &'a str,
    },
    ErrorConfigInvalidValue {
        field: &'a str,
    },
    ErrorConfigInvalidValueAtPath {
        path: &'a str,
        field: &'a str,
    },
    ErrorConfigProfileNotFound {
        path: &'a str,
        profile: &'a str,
    },
    ErrorConfigProfileConflict {
        path: &'a str,
        explicit_profile: &'a str,
        requested_profile: &'a str,
    },
    ErrorRpgMakerPromptUnavailable {
        locale: &'a str,
        component: &'a str,
        path: &'a str,
    },
    ErrorRpgMakerLanguageModuleUnavailable {
        source_language: &'a str,
        target_language: &'a str,
    },
    ErrorProjectUnavailable,
    ErrorProjectState,
    ErrorExternalModel,
    ErrorStateAppliedFinalization,
    ErrorOutcomeUnknown,
    ErrorInternal,
    ErrorShutdown,
    ErrorNoReusableExtractPlan,
    ErrorInitPathRequired,
    ErrorProfileRequired,
    ErrorSavedProfileUnavailable {
        profile: &'a str,
    },
    ErrorNoExecutableExtractOwner,
    ErrorPlanSaveFailedApplied,
    ErrorPlanSaveOutcomeUnknown,
    PlanSourceExplicit,
    PlanSourceProjectState,
    PlanSourceProductDefault,
    LogLabelPhaseCheckProject,
    LogLabelPhaseScanSource,
    LogLabelPhasePrepareCandidate,
    LogLabelPhaseUpdateDatabase,
    LogLabelPhasePublish,
    LogLabelPhaseBuiltin,
    LogLabelPhaseRules,
    LogLabelPhaseLua,
    LogLabelPhasePlanning,
    LogLabelPhaseConfirmedTasks,
    LogLabelPhaseNoWork,
    LogLabelPhaseReadAssets,
    LogLabelPhasePlanStandard,
    LogLabelPhaseRewriteDocuments,
    LogLabelPhaseValidateCandidate,
    LogLabelTaskComplete,
    LogLabelTaskPartial,
    LogLabelTaskUnavailable,
    LogLabelTaskFailed,
    NoticeInitReusePath {
        path: &'a str,
    },
    NoticeExtractReuseOwners {
        owners: &'a str,
    },
    NoticeTranslateReuseProfile {
        profile: &'a str,
    },
    NoticeTranslateReuseLua,
    NoticeWriteBackReuseLua,
    NoticeWriteBackStandardOnly,
    NoticeOwnerDisabled {
        owner: &'a str,
    },
    NoticeLuaCleared {
        phase: &'a str,
    },
    NoticeNoModelRequest,
    NoticeManualLayout {
        count: u64,
    },
    NoticeLogDegraded,
    ProgressInitCheckProject,
    ProgressInitScanSource,
    ProgressInitBuildCandidate,
    ProgressInitConvergeDatabase,
    ProgressInitPublish,
    ProgressSaveRunPlan,
    ProgressExtractOwner {
        owner: &'a str,
    },
    ProgressExtractDocuments,
    ProgressExtractBuiltin,
    ProgressExtractRules,
    ProgressExtractLua,
    ProgressExtractCommit,
    ProgressTranslatePlanning,
    ProgressTranslateConfirmed,
    ProgressTranslateNoWork,
    ProgressWriteBackReadAssets,
    ProgressWriteBackPlanning,
    ProgressWriteBackDocuments,
    ProgressWriteBackLua,
    ProgressWriteBackValidateCandidate,
    ProgressWriteBackPublish,
    ProgressFinalizing,
    ProgressSafeStopping,
    ResultInitCompleted {
        project: &'a str,
    },
    ResultInitCreated,
    ResultInitUnchanged,
    ResultInitUpdated,
    ResultInitStaleOwners {
        owners: &'a str,
    },
    ResultExtractCompleted {
        project: &'a str,
    },
    ResultTranslateCompleted {
        project: &'a str,
        profile: &'a str,
    },
    ResultTranslateStandard {
        total: u64,
        complete: u64,
        partial: u64,
        unavailable: u64,
        written: u64,
        remaining: u64,
    },
    ResultTranslateConvergence {
        retained: u64,
        invalidated: u64,
        not_applicable: u64,
        reused: u64,
    },
    ResultWriteBackCompleted {
        project: &'a str,
    },
    ResultOutputDirectory {
        path: &'a str,
    },
    ResultWriteBackStandard {
        translated: u64,
        original: u64,
        auto_wrapped: u64,
        breaks: u64,
        indents: u64,
        manual: u64,
    },
    ResultLuaExecuted,
    ResultLuaNotExecuted,
    ResultCancelled,
    ResultPlanSaved,
    ResultTranslatePlanSources {
        profile_source: &'a str,
        lua_source: &'a str,
    },
    LogRunStarted {
        command: &'a str,
    },
    LogRunSucceeded {
        command: &'a str,
    },
    LogRunFailed {
        command: &'a str,
    },
    LogRunCancelled {
        command: &'a str,
    },
    LogPlanResolved {
        command: &'a str,
        source: &'a str,
    },
    LogTranslatePlanResolved {
        profile_source: &'a str,
        lua_source: &'a str,
    },
    LogPhaseStarted {
        phase: &'a str,
    },
    LogPhaseFinished {
        phase: &'a str,
    },
    LogRetrySummary {
        count: u64,
    },
    LogNoWork {
        reason: &'a str,
    },
    LogPartialResult {
        count: u64,
    },
    LogPublishFinished {
        path: &'a str,
    },
    LogTranslationTaskStarted {
        index: u64,
        total: u64,
    },
    LogTranslationTaskFinished {
        index: u64,
        outcome: &'a str,
    },
}

impl UiMessage<'_> {
    fn key(self) -> &'static str {
        match self {
            Self::AppAbout => "app-about",
            Self::CliConfigHelp => "cli-config-help",
            Self::CliUiLanguageHelp => "cli-ui-language-help",
            Self::CliProgressHelp => "cli-progress-help",
            Self::CliMzAbout => "cli-mz-about",
            Self::CliMvAbout => "cli-mv-about",
            Self::CliInitAbout => "cli-init-about",
            Self::CliExtractAbout => "cli-extract-about",
            Self::CliTranslateAbout => "cli-translate-about",
            Self::CliWriteBackAbout => "cli-write-back-about",
            Self::CliProjectNameHelp => "cli-project-name-help",
            Self::CliInitPathHelp => "cli-init-path-help",
            Self::CliSourceLanguageHelp => "cli-source-language-help",
            Self::CliTargetLanguageHelp => "cli-target-language-help",
            Self::CliDialogueWidthHelp => "cli-dialogue-width-help",
            Self::CliScrollingWidthHelp => "cli-scrolling-width-help",
            Self::CliHelpWidthHelp => "cli-help-width-help",
            Self::CliBuiltinHelp => "cli-builtin-help",
            Self::CliRulesHelp => "cli-rules-help",
            Self::CliDialogueRulesHelp => "cli-dialogue-rules-help",
            Self::CliLuaHelp => "cli-lua-help",
            Self::CliProfileHelp => "cli-profile-help",
            Self::CliTermsHelp => "cli-terms-help",
            Self::CliPlaceholdersHelp => "cli-placeholders-help",
            Self::CliUsageHeading => "cli-usage-heading",
            Self::CliCommandsHeading => "cli-commands-heading",
            Self::CliOptionsHeading => "cli-options-heading",
            Self::CliArgumentsHeading => "cli-arguments-heading",
            Self::CliOptionsMetavar => "cli-options-metavar",
            Self::CliCommandMetavar => "cli-command-metavar",
            Self::CliPrintHelp => "cli-print-help",
            Self::CliPrintVersion => "cli-print-version",
            Self::CliMissingConfig => "cli-missing-config",
            Self::CliBlankValue => "cli-blank-value",
            Self::CliInvalidPositiveInteger => "cli-invalid-positive-integer",
            Self::CliInvalidProgress { .. } => "cli-invalid-progress",
            Self::CliInvalidUiLanguageArgument { .. } => "cli-invalid-ui-language-argument",
            Self::CliUnsupportedUiLanguageArgument { .. } => "cli-unsupported-ui-language-argument",
            Self::CliInvalidUiLanguageEnvironment { .. } => "cli-invalid-ui-language-environment",
            Self::CliUnsupportedUiLanguageEnvironment { .. } => {
                "cli-unsupported-ui-language-environment"
            }
            Self::CliUiLanguageEnvironmentNotUnicode => "cli-ui-language-environment-not-unicode",
            Self::CliUnexpectedArgument { .. } => "cli-unexpected-argument",
            Self::CliMissingRequiredArgument { .. } => "cli-missing-required-argument",
            Self::CliInvalidValue { .. } => "cli-invalid-value",
            Self::CliErrorHeading => "cli-error-heading",
            Self::CliTryHelp => "cli-try-help",
            Self::CliMissingValue { .. } => "cli-missing-value",
            Self::CliMissingSubcommand => "cli-missing-subcommand",
            Self::CliArgumentConflict { .. } => "cli-argument-conflict",
            Self::CliWrongNumberOfValues { .. } => "cli-wrong-number-of-values",
            Self::CliInvalidUtf8 => "cli-invalid-utf8",
            Self::CliParseFailure => "cli-parse-failure",
            Self::ErrorConfigurationOrInputGeneric => "error-configuration-or-input-generic",
            Self::ErrorConfigCurrentDirectoryNotAbsolute { .. } => {
                "error-config-current-directory-not-absolute"
            }
            Self::ErrorConfigEmptyPath => "error-config-empty-path",
            Self::ErrorConfigOpen { .. } => "error-config-open",
            Self::ErrorConfigNotAFile { .. } => "error-config-not-a-file",
            Self::ErrorConfigTooLarge { .. } => "error-config-too-large",
            Self::ErrorConfigRead { .. } => "error-config-read",
            Self::ErrorConfigInvalidUtf8KnownLength { .. } => {
                "error-config-invalid-utf8-known-length"
            }
            Self::ErrorConfigInvalidUtf8UnknownLength { .. } => {
                "error-config-invalid-utf8-unknown-length"
            }
            Self::ErrorConfigInvalidTomlAt { .. } => "error-config-invalid-toml-at",
            Self::ErrorConfigInvalidToml { .. } => "error-config-invalid-toml",
            Self::ErrorConfigInvalidValue { .. } => "error-config-invalid-value",
            Self::ErrorConfigInvalidValueAtPath { .. } => "error-config-invalid-value-at-path",
            Self::ErrorConfigProfileNotFound { .. } => "error-config-profile-not-found",
            Self::ErrorConfigProfileConflict { .. } => "error-config-profile-conflict",
            Self::ErrorRpgMakerPromptUnavailable { .. } => "error-rpg-maker-prompt-unavailable",
            Self::ErrorRpgMakerLanguageModuleUnavailable { .. } => {
                "error-rpg-maker-language-module-unavailable"
            }
            Self::ErrorProjectUnavailable => "error-project-unavailable",
            Self::ErrorProjectState => "error-project-state",
            Self::ErrorExternalModel => "error-external-model",
            Self::ErrorStateAppliedFinalization => "error-state-applied-finalization",
            Self::ErrorOutcomeUnknown => "error-outcome-unknown",
            Self::ErrorInternal => "error-internal",
            Self::ErrorShutdown => "error-shutdown",
            Self::ErrorNoReusableExtractPlan => "error-no-reusable-extract-plan",
            Self::ErrorInitPathRequired => "error-init-path-required",
            Self::ErrorProfileRequired => "error-profile-required",
            Self::ErrorSavedProfileUnavailable { .. } => "error-saved-profile-unavailable",
            Self::ErrorNoExecutableExtractOwner => "error-no-executable-extract-owner",
            Self::ErrorPlanSaveFailedApplied => "error-plan-save-failed-applied",
            Self::ErrorPlanSaveOutcomeUnknown => "error-plan-save-outcome-unknown",
            Self::PlanSourceExplicit => "plan-source-explicit",
            Self::PlanSourceProjectState => "plan-source-project-state",
            Self::PlanSourceProductDefault => "plan-source-product-default",
            Self::LogLabelPhaseCheckProject => "log-label-phase-check-project",
            Self::LogLabelPhaseScanSource => "log-label-phase-scan-source",
            Self::LogLabelPhasePrepareCandidate => "log-label-phase-prepare-candidate",
            Self::LogLabelPhaseUpdateDatabase => "log-label-phase-update-database",
            Self::LogLabelPhasePublish => "log-label-phase-publish",
            Self::LogLabelPhaseBuiltin => "log-label-phase-builtin",
            Self::LogLabelPhaseRules => "log-label-phase-rules",
            Self::LogLabelPhaseLua => "log-label-phase-lua",
            Self::LogLabelPhasePlanning => "log-label-phase-planning",
            Self::LogLabelPhaseConfirmedTasks => "log-label-phase-confirmed-tasks",
            Self::LogLabelPhaseNoWork => "log-label-phase-no-work",
            Self::LogLabelPhaseReadAssets => "log-label-phase-read-assets",
            Self::LogLabelPhasePlanStandard => "log-label-phase-plan-standard",
            Self::LogLabelPhaseRewriteDocuments => "log-label-phase-rewrite-documents",
            Self::LogLabelPhaseValidateCandidate => "log-label-phase-validate-candidate",
            Self::LogLabelTaskComplete => "log-label-task-complete",
            Self::LogLabelTaskPartial => "log-label-task-partial",
            Self::LogLabelTaskUnavailable => "log-label-task-unavailable",
            Self::LogLabelTaskFailed => "log-label-task-failed",
            Self::NoticeInitReusePath { .. } => "notice-init-reuse-path",
            Self::NoticeExtractReuseOwners { .. } => "notice-extract-reuse-owners",
            Self::NoticeTranslateReuseProfile { .. } => "notice-translate-reuse-profile",
            Self::NoticeTranslateReuseLua => "notice-translate-reuse-lua",
            Self::NoticeWriteBackReuseLua => "notice-write-back-reuse-lua",
            Self::NoticeWriteBackStandardOnly => "notice-write-back-standard-only",
            Self::NoticeOwnerDisabled { .. } => "notice-owner-disabled",
            Self::NoticeLuaCleared { .. } => "notice-lua-cleared",
            Self::NoticeNoModelRequest => "notice-no-model-request",
            Self::NoticeManualLayout { .. } => "notice-manual-layout",
            Self::NoticeLogDegraded => "notice-log-degraded",
            Self::ProgressInitCheckProject => "progress-init-check-project",
            Self::ProgressInitScanSource => "progress-init-scan-source",
            Self::ProgressInitBuildCandidate => "progress-init-build-candidate",
            Self::ProgressInitConvergeDatabase => "progress-init-converge-database",
            Self::ProgressInitPublish => "progress-init-publish",
            Self::ProgressSaveRunPlan => "progress-save-run-plan",
            Self::ProgressExtractOwner { .. } => "progress-extract-owner",
            Self::ProgressExtractDocuments => "progress-extract-documents",
            Self::ProgressExtractBuiltin => "progress-extract-builtin",
            Self::ProgressExtractRules => "progress-extract-rules",
            Self::ProgressExtractLua => "progress-extract-lua",
            Self::ProgressExtractCommit => "progress-extract-commit",
            Self::ProgressTranslatePlanning => "progress-translate-planning",
            Self::ProgressTranslateConfirmed => "progress-translate-confirmed",
            Self::ProgressTranslateNoWork => "progress-translate-no-work",
            Self::ProgressWriteBackReadAssets => "progress-write-back-read-assets",
            Self::ProgressWriteBackPlanning => "progress-write-back-planning",
            Self::ProgressWriteBackDocuments => "progress-write-back-documents",
            Self::ProgressWriteBackLua => "progress-write-back-lua",
            Self::ProgressWriteBackValidateCandidate => "progress-write-back-validate-candidate",
            Self::ProgressWriteBackPublish => "progress-write-back-publish",
            Self::ProgressFinalizing => "progress-finalizing",
            Self::ProgressSafeStopping => "progress-safe-stopping",
            Self::ResultInitCompleted { .. } => "result-init-completed",
            Self::ResultInitCreated => "result-init-created",
            Self::ResultInitUnchanged => "result-init-unchanged",
            Self::ResultInitUpdated => "result-init-updated",
            Self::ResultInitStaleOwners { .. } => "result-init-stale-owners",
            Self::ResultExtractCompleted { .. } => "result-extract-completed",
            Self::ResultTranslateCompleted { .. } => "result-translate-completed",
            Self::ResultTranslateStandard { .. } => "result-translate-standard",
            Self::ResultTranslateConvergence { .. } => "result-translate-convergence",
            Self::ResultWriteBackCompleted { .. } => "result-write-back-completed",
            Self::ResultOutputDirectory { .. } => "result-output-directory",
            Self::ResultWriteBackStandard { .. } => "result-write-back-standard",
            Self::ResultLuaExecuted => "result-lua-executed",
            Self::ResultLuaNotExecuted => "result-lua-not-executed",
            Self::ResultCancelled => "result-cancelled",
            Self::ResultPlanSaved => "result-plan-saved",
            Self::ResultTranslatePlanSources { .. } => "result-translate-plan-sources",
            Self::LogRunStarted { .. } => "log-run-started",
            Self::LogRunSucceeded { .. } => "log-run-succeeded",
            Self::LogRunFailed { .. } => "log-run-failed",
            Self::LogRunCancelled { .. } => "log-run-cancelled",
            Self::LogPlanResolved { .. } => "log-plan-resolved",
            Self::LogTranslatePlanResolved { .. } => "log-translate-plan-resolved",
            Self::LogPhaseStarted { .. } => "log-phase-started",
            Self::LogPhaseFinished { .. } => "log-phase-finished",
            Self::LogRetrySummary { .. } => "log-retry-summary",
            Self::LogNoWork { .. } => "log-no-work",
            Self::LogPartialResult { .. } => "log-partial-result",
            Self::LogPublishFinished { .. } => "log-publish-finished",
            Self::LogTranslationTaskStarted { .. } => "log-translation-task-started",
            Self::LogTranslationTaskFinished { .. } => "log-translation-task-finished",
        }
    }

    fn arguments(self) -> FluentArgs<'static> {
        let mut arguments = FluentArgs::new();
        match self {
            Self::CliInvalidProgress { value }
            | Self::CliInvalidUiLanguageArgument { value }
            | Self::CliUnsupportedUiLanguageArgument { value }
            | Self::CliInvalidUiLanguageEnvironment { value }
            | Self::CliUnsupportedUiLanguageEnvironment { value }
            | Self::CliUnexpectedArgument { value }
            | Self::CliMissingRequiredArgument { value } => {
                set_text(&mut arguments, "value", value);
            }
            Self::CliInvalidValue { value, argument } => {
                set_text(&mut arguments, "value", value);
                set_text(&mut arguments, "argument", argument);
            }
            Self::CliMissingValue { argument }
            | Self::CliArgumentConflict { argument }
            | Self::CliWrongNumberOfValues { argument } => {
                set_text(&mut arguments, "argument", argument);
            }
            Self::ErrorConfigCurrentDirectoryNotAbsolute { path }
            | Self::ErrorConfigOpen { path }
            | Self::ErrorConfigNotAFile { path }
            | Self::ErrorConfigRead { path } => set_text(&mut arguments, "path", path),
            Self::ErrorConfigTooLarge {
                path,
                observed_bytes,
                maximum_bytes,
            } => {
                set_text(&mut arguments, "path", path);
                set_number(&mut arguments, "observed", observed_bytes);
                set_number(&mut arguments, "maximum", maximum_bytes);
            }
            Self::ErrorConfigInvalidUtf8KnownLength {
                path,
                valid_up_to,
                error_len,
            } => {
                set_text(&mut arguments, "path", path);
                set_number(&mut arguments, "valid", valid_up_to);
                set_number(&mut arguments, "length", error_len);
            }
            Self::ErrorConfigInvalidUtf8UnknownLength { path, valid_up_to } => {
                set_text(&mut arguments, "path", path);
                set_number(&mut arguments, "valid", valid_up_to);
            }
            Self::ErrorConfigInvalidTomlAt {
                path,
                line,
                column,
                resource,
            } => {
                set_text(&mut arguments, "path", path);
                set_number(&mut arguments, "line", line);
                set_number(&mut arguments, "column", column);
                set_text(&mut arguments, "resource", resource);
            }
            Self::ErrorConfigInvalidToml { path, resource } => {
                set_text(&mut arguments, "path", path);
                set_text(&mut arguments, "resource", resource);
            }
            Self::ErrorConfigInvalidValue { field } => {
                set_text(&mut arguments, "field", field);
            }
            Self::ErrorConfigInvalidValueAtPath { path, field } => {
                set_text(&mut arguments, "path", path);
                set_text(&mut arguments, "field", field);
            }
            Self::ErrorConfigProfileNotFound { path, profile } => {
                set_text(&mut arguments, "path", path);
                set_text(&mut arguments, "profile", profile);
            }
            Self::ErrorConfigProfileConflict {
                path,
                explicit_profile,
                requested_profile,
            } => {
                set_text(&mut arguments, "path", path);
                set_text(&mut arguments, "explicit", explicit_profile);
                set_text(&mut arguments, "requested", requested_profile);
            }
            Self::ErrorRpgMakerPromptUnavailable {
                locale,
                component,
                path,
            } => {
                set_text(&mut arguments, "locale", locale);
                set_text(&mut arguments, "component", component);
                set_text(&mut arguments, "path", path);
            }
            Self::ErrorRpgMakerLanguageModuleUnavailable {
                source_language,
                target_language,
            } => {
                set_text(&mut arguments, "source", source_language);
                set_text(&mut arguments, "target", target_language);
            }
            Self::ErrorSavedProfileUnavailable { profile }
            | Self::NoticeTranslateReuseProfile { profile } => {
                set_text(&mut arguments, "profile", profile);
            }
            Self::ResultTranslatePlanSources {
                profile_source,
                lua_source,
            }
            | Self::LogTranslatePlanResolved {
                profile_source,
                lua_source,
            } => {
                set_text(&mut arguments, "profile_source", profile_source);
                set_text(&mut arguments, "lua_source", lua_source);
            }
            Self::NoticeInitReusePath { path }
            | Self::ResultOutputDirectory { path }
            | Self::LogPublishFinished { path } => set_text(&mut arguments, "path", path),
            Self::NoticeExtractReuseOwners { owners } | Self::ResultInitStaleOwners { owners } => {
                set_text(&mut arguments, "owners", owners);
            }
            Self::NoticeOwnerDisabled { owner } => set_text(&mut arguments, "owner", owner),
            Self::NoticeLuaCleared { phase } => set_text(&mut arguments, "phase", phase),
            Self::NoticeManualLayout { count }
            | Self::LogRetrySummary { count }
            | Self::LogPartialResult { count } => set_number(&mut arguments, "count", count),
            Self::ProgressExtractOwner { owner } => {
                set_text(&mut arguments, "owner", owner);
            }
            Self::ResultInitCompleted { project }
            | Self::ResultExtractCompleted { project }
            | Self::ResultWriteBackCompleted { project } => {
                set_text(&mut arguments, "project", project);
            }
            Self::ResultTranslateCompleted { project, profile } => {
                set_text(&mut arguments, "project", project);
                set_text(&mut arguments, "profile", profile);
            }
            Self::ResultTranslateStandard {
                total,
                complete,
                partial,
                unavailable,
                written,
                remaining,
            } => {
                set_number(&mut arguments, "total", total);
                set_number(&mut arguments, "complete", complete);
                set_number(&mut arguments, "partial", partial);
                set_number(&mut arguments, "unavailable", unavailable);
                set_number(&mut arguments, "written", written);
                set_number(&mut arguments, "remaining", remaining);
            }
            Self::ResultTranslateConvergence {
                retained,
                invalidated,
                not_applicable,
                reused,
            } => {
                set_number(&mut arguments, "retained", retained);
                set_number(&mut arguments, "invalidated", invalidated);
                set_number(&mut arguments, "not_applicable", not_applicable);
                set_number(&mut arguments, "reused", reused);
            }
            Self::ResultWriteBackStandard {
                translated,
                original,
                auto_wrapped,
                breaks,
                indents,
                manual,
            } => {
                set_number(&mut arguments, "translated", translated);
                set_number(&mut arguments, "original", original);
                set_number(&mut arguments, "auto_wrapped", auto_wrapped);
                set_number(&mut arguments, "breaks", breaks);
                set_number(&mut arguments, "indents", indents);
                set_number(&mut arguments, "manual", manual);
            }
            Self::LogRunStarted { command }
            | Self::LogRunSucceeded { command }
            | Self::LogRunFailed { command }
            | Self::LogRunCancelled { command } => set_text(&mut arguments, "command", command),
            Self::LogPlanResolved { command, source } => {
                set_text(&mut arguments, "command", command);
                set_text(&mut arguments, "source", source);
            }
            Self::LogPhaseStarted { phase } | Self::LogPhaseFinished { phase } => {
                set_text(&mut arguments, "phase", phase);
            }
            Self::LogNoWork { reason } => set_text(&mut arguments, "reason", reason),
            Self::LogTranslationTaskStarted { index, total } => {
                set_number(&mut arguments, "index", index);
                set_number(&mut arguments, "total", total);
            }
            Self::LogTranslationTaskFinished { index, outcome } => {
                set_number(&mut arguments, "index", index);
                set_text(&mut arguments, "outcome", outcome);
            }
            Self::AppAbout
            | Self::CliConfigHelp
            | Self::CliUiLanguageHelp
            | Self::CliProgressHelp
            | Self::CliMzAbout
            | Self::CliMvAbout
            | Self::CliInitAbout
            | Self::CliExtractAbout
            | Self::CliTranslateAbout
            | Self::CliWriteBackAbout
            | Self::CliProjectNameHelp
            | Self::CliInitPathHelp
            | Self::CliSourceLanguageHelp
            | Self::CliTargetLanguageHelp
            | Self::CliDialogueWidthHelp
            | Self::CliScrollingWidthHelp
            | Self::CliHelpWidthHelp
            | Self::CliBuiltinHelp
            | Self::CliRulesHelp
            | Self::CliDialogueRulesHelp
            | Self::CliLuaHelp
            | Self::CliProfileHelp
            | Self::CliTermsHelp
            | Self::CliPlaceholdersHelp
            | Self::CliUsageHeading
            | Self::CliCommandsHeading
            | Self::CliOptionsHeading
            | Self::CliArgumentsHeading
            | Self::CliOptionsMetavar
            | Self::CliCommandMetavar
            | Self::CliPrintHelp
            | Self::CliPrintVersion
            | Self::CliMissingConfig
            | Self::CliBlankValue
            | Self::CliInvalidPositiveInteger
            | Self::CliUiLanguageEnvironmentNotUnicode
            | Self::CliErrorHeading
            | Self::CliTryHelp
            | Self::CliMissingSubcommand
            | Self::CliInvalidUtf8
            | Self::CliParseFailure
            | Self::ErrorConfigurationOrInputGeneric
            | Self::ErrorConfigEmptyPath
            | Self::ErrorProjectUnavailable
            | Self::ErrorProjectState
            | Self::ErrorExternalModel
            | Self::ErrorStateAppliedFinalization
            | Self::ErrorOutcomeUnknown
            | Self::ErrorInternal
            | Self::ErrorShutdown
            | Self::ErrorNoReusableExtractPlan
            | Self::ErrorInitPathRequired
            | Self::ErrorProfileRequired
            | Self::ErrorNoExecutableExtractOwner
            | Self::ErrorPlanSaveFailedApplied
            | Self::ErrorPlanSaveOutcomeUnknown
            | Self::PlanSourceExplicit
            | Self::PlanSourceProjectState
            | Self::PlanSourceProductDefault
            | Self::LogLabelPhaseCheckProject
            | Self::LogLabelPhaseScanSource
            | Self::LogLabelPhasePrepareCandidate
            | Self::LogLabelPhaseUpdateDatabase
            | Self::LogLabelPhasePublish
            | Self::LogLabelPhaseBuiltin
            | Self::LogLabelPhaseRules
            | Self::LogLabelPhaseLua
            | Self::LogLabelPhasePlanning
            | Self::LogLabelPhaseConfirmedTasks
            | Self::LogLabelPhaseNoWork
            | Self::LogLabelPhaseReadAssets
            | Self::LogLabelPhasePlanStandard
            | Self::LogLabelPhaseRewriteDocuments
            | Self::LogLabelPhaseValidateCandidate
            | Self::LogLabelTaskComplete
            | Self::LogLabelTaskPartial
            | Self::LogLabelTaskUnavailable
            | Self::LogLabelTaskFailed
            | Self::NoticeTranslateReuseLua
            | Self::NoticeWriteBackReuseLua
            | Self::NoticeWriteBackStandardOnly
            | Self::NoticeNoModelRequest
            | Self::NoticeLogDegraded
            | Self::ProgressInitCheckProject
            | Self::ProgressInitScanSource
            | Self::ProgressInitBuildCandidate
            | Self::ProgressInitConvergeDatabase
            | Self::ProgressInitPublish
            | Self::ProgressSaveRunPlan
            | Self::ProgressExtractDocuments
            | Self::ProgressExtractBuiltin
            | Self::ProgressExtractRules
            | Self::ProgressExtractLua
            | Self::ProgressExtractCommit
            | Self::ProgressTranslatePlanning
            | Self::ProgressTranslateConfirmed
            | Self::ProgressTranslateNoWork
            | Self::ProgressWriteBackReadAssets
            | Self::ProgressWriteBackPlanning
            | Self::ProgressWriteBackDocuments
            | Self::ProgressWriteBackLua
            | Self::ProgressWriteBackValidateCandidate
            | Self::ProgressWriteBackPublish
            | Self::ProgressFinalizing
            | Self::ProgressSafeStopping
            | Self::ResultInitCreated
            | Self::ResultInitUnchanged
            | Self::ResultInitUpdated
            | Self::ResultLuaExecuted
            | Self::ResultLuaNotExecuted
            | Self::ResultCancelled
            | Self::ResultPlanSaved => {}
        }
        arguments
    }
}

/// 把项目日志 payload 中的稳定来源代码映射到本地化标签；未知代码不猜测。
pub(crate) fn project_log_value_source_label(code: &str) -> Option<UiMessage<'static>> {
    match code {
        "explicit" => Some(UiMessage::PlanSourceExplicit),
        "project_state" => Some(UiMessage::PlanSourceProjectState),
        "product_default" => Some(UiMessage::PlanSourceProductDefault),
        _ => None,
    }
}

/// 把各纵向切片写入 payload 的稳定阶段代码映射到本地化标签。
pub(crate) fn project_log_phase_label(code: &str) -> Option<UiMessage<'static>> {
    match code {
        "check_project" => Some(UiMessage::LogLabelPhaseCheckProject),
        "scan_source" => Some(UiMessage::LogLabelPhaseScanSource),
        "prepare_candidate" => Some(UiMessage::LogLabelPhasePrepareCandidate),
        "update_database" => Some(UiMessage::LogLabelPhaseUpdateDatabase),
        "publish" => Some(UiMessage::LogLabelPhasePublish),
        "builtin" => Some(UiMessage::LogLabelPhaseBuiltin),
        "builtin_documents" => Some(UiMessage::ProgressExtractDocuments),
        "builtin_work_units" => Some(UiMessage::ProgressExtractBuiltin),
        "builtin_commit" => Some(UiMessage::ProgressExtractCommit),
        "rules" => Some(UiMessage::LogLabelPhaseRules),
        "rules_documents" => Some(UiMessage::ProgressExtractDocuments),
        "rules_matches" => Some(UiMessage::ProgressExtractRules),
        "rules_commit" => Some(UiMessage::ProgressExtractCommit),
        "lua" => Some(UiMessage::LogLabelPhaseLua),
        "lua_execution" => Some(UiMessage::ProgressExtractLua),
        "lua_commit" => Some(UiMessage::ProgressExtractCommit),
        "planning" => Some(UiMessage::LogLabelPhasePlanning),
        "confirmed_tasks" => Some(UiMessage::LogLabelPhaseConfirmedTasks),
        "no_work" => Some(UiMessage::LogLabelPhaseNoWork),
        "read_assets" => Some(UiMessage::LogLabelPhaseReadAssets),
        "plan_standard" => Some(UiMessage::LogLabelPhasePlanStandard),
        "rewrite_documents" => Some(UiMessage::LogLabelPhaseRewriteDocuments),
        "validate_candidate" => Some(UiMessage::LogLabelPhaseValidateCandidate),
        _ => None,
    }
}

/// 把翻译任务 payload 的稳定结果代码映射到本地化标签。
pub(crate) fn project_log_task_outcome_label(code: &str) -> Option<UiMessage<'static>> {
    match code {
        "complete" => Some(UiMessage::LogLabelTaskComplete),
        "partial" => Some(UiMessage::LogLabelTaskPartial),
        "unavailable" => Some(UiMessage::LogLabelTaskUnavailable),
        "failed" => Some(UiMessage::LogLabelTaskFailed),
        _ => None,
    }
}

fn set_text(arguments: &mut FluentArgs<'static>, name: &'static str, value: &str) {
    arguments.set(name, sanitize_user_text(value));
}

fn set_number(arguments: &mut FluentArgs<'static>, name: &'static str, value: u64) {
    arguments.set(name, value);
}

/// 使用嵌入式 Fluent catalog 格式化用户可见消息。
pub(crate) struct UiLocalizer {
    #[cfg(test)]
    locale: UiLocale,
    selected: FluentBundle<FluentResource>,
    english_fallback: FluentBundle<FluentResource>,
}

impl UiLocalizer {
    pub(crate) fn new(locale: UiLocale) -> Self {
        Self {
            #[cfg(test)]
            locale,
            selected: build_bundle(locale),
            english_fallback: build_bundle(UiLocale::English),
        }
    }

    #[cfg(test)]
    pub(crate) const fn locale(&self) -> UiLocale {
        self.locale
    }

    /// 格式化是不可失败的呈现操作；嵌入资源异常时依次回退英语和值稳定的消息 key。
    pub(crate) fn format(&self, message: UiMessage<'_>) -> String {
        let key = message.key();
        let arguments = message.arguments();
        try_format(&self.selected, key, &arguments)
            .or_else(|| try_format(&self.english_fallback, key, &arguments))
            .unwrap_or_else(|| key.to_owned())
    }
}

fn build_bundle(locale: UiLocale) -> FluentBundle<FluentResource> {
    let resource = match FluentResource::try_new(locale.catalog().to_owned()) {
        Ok(resource) => resource,
        Err((_, errors)) => panic!("嵌入式 {} Fluent catalog 无效：{errors:?}", locale),
    };
    let mut bundle = FluentBundle::new_concurrent(vec![locale.language_identifier()]);
    bundle.set_use_isolating(true);
    if let Err(errors) = bundle.add_resource(resource) {
        panic!("嵌入式 {} Fluent catalog 包含重复消息：{errors:?}", locale);
    }
    bundle
}

fn try_format(
    bundle: &FluentBundle<FluentResource>,
    key: &str,
    arguments: &FluentArgs<'_>,
) -> Option<String> {
    let message = bundle.get_message(key)?;
    let pattern = message.value()?;
    let mut errors = Vec::new();
    let rendered = bundle
        .format_pattern(pattern, Some(arguments), &mut errors)
        .into_owned();
    errors.is_empty().then_some(rendered)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn explicit_and_automatic_locale_matching_obey_the_contract() {
        assert_eq!(
            UiLocale::parse_explicit("zh-TW").expect("区域变体应受支持"),
            UiLocale::TraditionalChinese
        );
        assert_eq!(
            UiLocale::parse_explicit("zh-SG").expect("区域变体应受支持"),
            UiLocale::SimplifiedChinese
        );
        assert_eq!(
            UiLocale::parse_explicit("fr-CA").expect("受支持主语言的区域变体应受支持"),
            UiLocale::French
        );
        assert!(matches!(
            UiLocale::parse_explicit("de-DE"),
            Err(UiLocaleSelectionError::UnsupportedLanguage { .. })
        ));
        assert!(matches!(
            UiLocale::parse_explicit("not_a_tag"),
            Err(UiLocaleSelectionError::InvalidLanguageTag { .. })
        ));
    }

    #[test]
    fn selection_uses_explicit_layers_and_skips_bad_automatic_candidates() {
        let selected =
            select_ui_locale(Some("ja-JP"), Some("fr"), ["ru"]).expect("CLI locale 应成功");
        assert_eq!(selected.locale(), UiLocale::Japanese);
        assert_eq!(selected.source(), UiLocaleSource::CommandLine);

        let selected = select_ui_locale(None, Some("ko-KR"), ["ru"]).expect("环境 locale 应成功");
        assert_eq!(selected.locale(), UiLocale::Korean);
        assert_eq!(selected.source(), UiLocaleSource::Environment);

        let selected = select_ui_locale(None, None, ["not_a_tag", "de-DE", "ru-RU"])
            .expect("自动探测不应上交坏候选");
        assert_eq!(selected.locale(), UiLocale::Russian);
        assert_eq!(selected.source(), UiLocaleSource::Windows);

        let selected = select_ui_locale(None, None, ["de-DE"]).expect("不支持的自动候选应回退");
        assert_eq!(selected.locale(), UiLocale::English);
        assert_eq!(selected.source(), UiLocaleSource::ProductDefault);

        assert!(matches!(
            select_ui_locale(Some("de"), Some("en"), ["fr"]),
            Err(UiLocaleSelectionError::UnsupportedLanguage {
                input: UiLocaleInputSource::CommandLine,
                ..
            })
        ));
        assert!(matches!(
            select_ui_locale(None, Some("de"), ["en"]),
            Err(UiLocaleSelectionError::UnsupportedLanguage {
                input: UiLocaleInputSource::Environment,
                ..
            })
        ));
    }

    #[test]
    fn concurrent_localizer_can_be_shared_by_terminal_and_log_renderers() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<UiLocalizer>();
    }

    #[test]
    fn every_catalog_has_the_same_keys_and_argument_sets() {
        let expected = catalog_schema(UiLocale::English.catalog());
        assert!(!expected.is_empty());
        for locale in UiLocale::ALL {
            let actual = catalog_schema(locale.catalog());
            assert_eq!(actual, expected, "{locale} catalog schema 不一致");
        }

        let facade_keys = all_test_messages()
            .into_iter()
            .map(UiMessage::key)
            .collect::<BTreeSet<_>>();
        let catalog_keys = expected.keys().map(String::as_str).collect::<BTreeSet<_>>();
        assert_eq!(facade_keys, catalog_keys, "类型化门面与 catalog key 不一致");
    }

    #[test]
    fn every_typed_message_formats_in_every_locale_without_fallback_errors() {
        for locale in UiLocale::ALL {
            let bundle = build_bundle(locale);
            for message in all_test_messages() {
                let key = message.key();
                let arguments = message.arguments();
                assert!(
                    try_format(&bundle, key, &arguments).is_some(),
                    "{locale} 的 {key} 格式化失败"
                );
            }
        }
    }

    #[test]
    fn russian_and_arabic_retry_summaries_use_locale_plural_rules() {
        let russian = UiLocalizer::new(UiLocale::Russian);
        assert!(
            without_fluent_isolation(&russian.format(UiMessage::LogRetrySummary { count: 1 }))
                .contains("1 повтор")
        );
        assert!(
            without_fluent_isolation(&russian.format(UiMessage::LogRetrySummary { count: 2 }))
                .contains("2 повтора")
        );
        assert!(
            without_fluent_isolation(&russian.format(UiMessage::LogRetrySummary { count: 5 }))
                .contains("5 повторов")
        );

        let arabic = UiLocalizer::new(UiLocale::Arabic);
        assert!(
            arabic
                .format(UiMessage::LogRetrySummary { count: 0 })
                .contains("لا توجد")
        );
        assert!(
            arabic
                .format(UiMessage::LogRetrySummary { count: 1 })
                .contains("محاولة واحدة")
        );
        assert!(
            arabic
                .format(UiMessage::LogRetrySummary { count: 2 })
                .contains("محاولتان")
        );
        assert!(
            arabic
                .format(UiMessage::LogRetrySummary { count: 3 })
                .contains("محاولات")
        );
    }

    #[test]
    fn rtl_dynamic_values_are_sanitized_and_directionally_isolated() {
        let localizer = UiLocalizer::new(UiLocale::Arabic);
        assert_eq!(localizer.locale(), UiLocale::Arabic);
        assert_eq!(
            localizer.locale().text_direction(),
            UiTextDirection::RightToLeft
        );
        let rendered = localizer.format(UiMessage::ResultOutputDirectory {
            path: "C:\\Games\n\u{202e}demo\u{2068}\u{1b}[31m",
        });
        assert!(rendered.contains("C:\\Games demo[31m"));
        assert!(rendered.contains('\u{2068}'));
        assert!(rendered.contains('\u{2069}'));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn project_log_stable_codes_have_closed_localized_labels() {
        for source in ["explicit", "project_state", "product_default"] {
            assert!(project_log_value_source_label(source).is_some(), "{source}");
        }
        for phase in [
            "check_project",
            "scan_source",
            "prepare_candidate",
            "update_database",
            "publish",
            "builtin",
            "builtin_documents",
            "builtin_work_units",
            "builtin_commit",
            "rules",
            "rules_documents",
            "rules_matches",
            "rules_commit",
            "lua",
            "lua_execution",
            "lua_commit",
            "planning",
            "confirmed_tasks",
            "no_work",
            "read_assets",
            "plan_standard",
            "rewrite_documents",
            "validate_candidate",
        ] {
            assert!(project_log_phase_label(phase).is_some(), "{phase}");
        }
        for outcome in ["complete", "partial", "unavailable", "failed"] {
            assert!(
                project_log_task_outcome_label(outcome).is_some(),
                "{outcome}"
            );
        }
        assert!(project_log_value_source_label("future").is_none());
        assert!(project_log_phase_label("future").is_none());
        assert!(project_log_task_outcome_label("future").is_none());
    }

    fn catalog_schema(source: &str) -> BTreeMap<String, BTreeSet<String>> {
        let mut messages = BTreeMap::<String, String>::new();
        let mut current = None::<String>;
        for line in source.lines() {
            if !line.starts_with(char::is_whitespace)
                && !line.starts_with('#')
                && let Some((identifier, value)) = line.split_once(" =")
            {
                let identifier = identifier.trim();
                if !identifier.is_empty() {
                    current = Some(identifier.to_owned());
                    messages
                        .entry(identifier.to_owned())
                        .or_default()
                        .push_str(value);
                    continue;
                }
            }
            if let Some(identifier) = &current {
                messages
                    .entry(identifier.clone())
                    .or_default()
                    .push_str(line);
            }
        }

        messages
            .into_iter()
            .map(|(identifier, pattern)| (identifier, fluent_variables(&pattern)))
            .collect()
    }

    fn fluent_variables(pattern: &str) -> BTreeSet<String> {
        let characters = pattern.as_bytes();
        let mut variables = BTreeSet::new();
        let mut index = 0;
        while index < characters.len() {
            if characters[index] != b'$' {
                index += 1;
                continue;
            }
            index += 1;
            let start = index;
            while index < characters.len()
                && (characters[index].is_ascii_alphanumeric()
                    || matches!(characters[index], b'_' | b'-'))
            {
                index += 1;
            }
            if start != index {
                variables.insert(pattern[start..index].to_owned());
            }
        }
        variables
    }

    fn without_fluent_isolation(value: &str) -> String {
        value
            .chars()
            .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn all_test_messages() -> Vec<UiMessage<'static>> {
        vec![
            UiMessage::AppAbout,
            UiMessage::CliConfigHelp,
            UiMessage::CliUiLanguageHelp,
            UiMessage::CliProgressHelp,
            UiMessage::CliMzAbout,
            UiMessage::CliMvAbout,
            UiMessage::CliInitAbout,
            UiMessage::CliExtractAbout,
            UiMessage::CliTranslateAbout,
            UiMessage::CliWriteBackAbout,
            UiMessage::CliProjectNameHelp,
            UiMessage::CliInitPathHelp,
            UiMessage::CliSourceLanguageHelp,
            UiMessage::CliTargetLanguageHelp,
            UiMessage::CliDialogueWidthHelp,
            UiMessage::CliScrollingWidthHelp,
            UiMessage::CliHelpWidthHelp,
            UiMessage::CliBuiltinHelp,
            UiMessage::CliRulesHelp,
            UiMessage::CliDialogueRulesHelp,
            UiMessage::CliLuaHelp,
            UiMessage::CliProfileHelp,
            UiMessage::CliTermsHelp,
            UiMessage::CliPlaceholdersHelp,
            UiMessage::CliUsageHeading,
            UiMessage::CliCommandsHeading,
            UiMessage::CliOptionsHeading,
            UiMessage::CliArgumentsHeading,
            UiMessage::CliOptionsMetavar,
            UiMessage::CliCommandMetavar,
            UiMessage::CliPrintHelp,
            UiMessage::CliPrintVersion,
            UiMessage::CliMissingConfig,
            UiMessage::CliBlankValue,
            UiMessage::CliInvalidPositiveInteger,
            UiMessage::CliInvalidProgress { value: "fast" },
            UiMessage::CliInvalidUiLanguageArgument { value: "bad" },
            UiMessage::CliUnsupportedUiLanguageArgument { value: "de" },
            UiMessage::CliInvalidUiLanguageEnvironment { value: "bad" },
            UiMessage::CliUnsupportedUiLanguageEnvironment { value: "de" },
            UiMessage::CliUiLanguageEnvironmentNotUnicode,
            UiMessage::CliUnexpectedArgument { value: "--bad" },
            UiMessage::CliMissingRequiredArgument { value: "--name" },
            UiMessage::CliInvalidValue {
                value: "bad",
                argument: "--progress",
            },
            UiMessage::CliErrorHeading,
            UiMessage::CliTryHelp,
            UiMessage::CliMissingValue { argument: "--path" },
            UiMessage::CliMissingSubcommand,
            UiMessage::CliArgumentConflict {
                argument: "--dialogue-rules",
            },
            UiMessage::CliWrongNumberOfValues { argument: "--path" },
            UiMessage::CliInvalidUtf8,
            UiMessage::CliParseFailure,
            UiMessage::ErrorConfigurationOrInputGeneric,
            UiMessage::ErrorConfigCurrentDirectoryNotAbsolute { path: "path" },
            UiMessage::ErrorConfigEmptyPath,
            UiMessage::ErrorConfigOpen { path: "path" },
            UiMessage::ErrorConfigNotAFile { path: "path" },
            UiMessage::ErrorConfigTooLarge {
                path: "path",
                observed_bytes: 5,
                maximum_bytes: 4,
            },
            UiMessage::ErrorConfigRead { path: "path" },
            UiMessage::ErrorConfigInvalidUtf8KnownLength {
                path: "path",
                valid_up_to: 3,
                error_len: 1,
            },
            UiMessage::ErrorConfigInvalidUtf8UnknownLength {
                path: "path",
                valid_up_to: 3,
            },
            UiMessage::ErrorConfigInvalidTomlAt {
                path: "path",
                line: 2,
                column: 3,
                resource: "runtime.sqlite",
            },
            UiMessage::ErrorConfigInvalidToml {
                path: "path",
                resource: "runtime.sqlite",
            },
            UiMessage::ErrorConfigInvalidValue { field: "field" },
            UiMessage::ErrorConfigInvalidValueAtPath {
                path: "path",
                field: "field",
            },
            UiMessage::ErrorConfigProfileNotFound {
                path: "path",
                profile: "main",
            },
            UiMessage::ErrorConfigProfileConflict {
                path: "path",
                explicit_profile: "main",
                requested_profile: "other",
            },
            UiMessage::ErrorRpgMakerPromptUnavailable {
                locale: "zh-Hans",
                component: "system",
                path: "prompts/rpg_maker/zh-Hans/system.md",
            },
            UiMessage::ErrorRpgMakerLanguageModuleUnavailable {
                source_language: "ja",
                target_language: "zh-Hans",
            },
            UiMessage::ErrorProjectUnavailable,
            UiMessage::ErrorProjectState,
            UiMessage::ErrorExternalModel,
            UiMessage::ErrorStateAppliedFinalization,
            UiMessage::ErrorOutcomeUnknown,
            UiMessage::ErrorInternal,
            UiMessage::ErrorShutdown,
            UiMessage::ErrorNoReusableExtractPlan,
            UiMessage::ErrorInitPathRequired,
            UiMessage::ErrorProfileRequired,
            UiMessage::ErrorSavedProfileUnavailable { profile: "profile" },
            UiMessage::ErrorNoExecutableExtractOwner,
            UiMessage::ErrorPlanSaveFailedApplied,
            UiMessage::ErrorPlanSaveOutcomeUnknown,
            UiMessage::PlanSourceExplicit,
            UiMessage::PlanSourceProjectState,
            UiMessage::PlanSourceProductDefault,
            UiMessage::LogLabelPhaseCheckProject,
            UiMessage::LogLabelPhaseScanSource,
            UiMessage::LogLabelPhasePrepareCandidate,
            UiMessage::LogLabelPhaseUpdateDatabase,
            UiMessage::LogLabelPhasePublish,
            UiMessage::LogLabelPhaseBuiltin,
            UiMessage::LogLabelPhaseRules,
            UiMessage::LogLabelPhaseLua,
            UiMessage::LogLabelPhasePlanning,
            UiMessage::LogLabelPhaseConfirmedTasks,
            UiMessage::LogLabelPhaseNoWork,
            UiMessage::LogLabelPhaseReadAssets,
            UiMessage::LogLabelPhasePlanStandard,
            UiMessage::LogLabelPhaseRewriteDocuments,
            UiMessage::LogLabelPhaseValidateCandidate,
            UiMessage::LogLabelTaskComplete,
            UiMessage::LogLabelTaskPartial,
            UiMessage::LogLabelTaskUnavailable,
            UiMessage::LogLabelTaskFailed,
            UiMessage::NoticeInitReusePath { path: "path" },
            UiMessage::NoticeExtractReuseOwners { owners: "owners" },
            UiMessage::NoticeTranslateReuseProfile { profile: "profile" },
            UiMessage::NoticeTranslateReuseLua,
            UiMessage::NoticeWriteBackReuseLua,
            UiMessage::NoticeWriteBackStandardOnly,
            UiMessage::NoticeOwnerDisabled { owner: "owner" },
            UiMessage::NoticeLuaCleared { phase: "phase" },
            UiMessage::NoticeNoModelRequest,
            UiMessage::NoticeManualLayout { count: 3 },
            UiMessage::NoticeLogDegraded,
            UiMessage::ProgressInitCheckProject,
            UiMessage::ProgressInitScanSource,
            UiMessage::ProgressInitBuildCandidate,
            UiMessage::ProgressInitConvergeDatabase,
            UiMessage::ProgressInitPublish,
            UiMessage::ProgressSaveRunPlan,
            UiMessage::ProgressExtractOwner { owner: "Builtin" },
            UiMessage::ProgressExtractDocuments,
            UiMessage::ProgressExtractBuiltin,
            UiMessage::ProgressExtractRules,
            UiMessage::ProgressExtractLua,
            UiMessage::ProgressExtractCommit,
            UiMessage::ProgressTranslatePlanning,
            UiMessage::ProgressTranslateConfirmed,
            UiMessage::ProgressTranslateNoWork,
            UiMessage::ProgressWriteBackReadAssets,
            UiMessage::ProgressWriteBackPlanning,
            UiMessage::ProgressWriteBackDocuments,
            UiMessage::ProgressWriteBackLua,
            UiMessage::ProgressWriteBackValidateCandidate,
            UiMessage::ProgressWriteBackPublish,
            UiMessage::ProgressFinalizing,
            UiMessage::ProgressSafeStopping,
            UiMessage::ResultInitCompleted { project: "demo" },
            UiMessage::ResultInitCreated,
            UiMessage::ResultInitUnchanged,
            UiMessage::ResultInitUpdated,
            UiMessage::ResultInitStaleOwners { owners: "Rules" },
            UiMessage::ResultExtractCompleted { project: "demo" },
            UiMessage::ResultTranslateCompleted {
                project: "demo",
                profile: "main",
            },
            UiMessage::ResultTranslateStandard {
                total: 1,
                complete: 1,
                partial: 0,
                unavailable: 0,
                written: 2,
                remaining: 0,
            },
            UiMessage::ResultTranslateConvergence {
                retained: 1,
                invalidated: 0,
                not_applicable: 0,
                reused: 1,
            },
            UiMessage::ResultWriteBackCompleted { project: "demo" },
            UiMessage::ResultOutputDirectory { path: "output" },
            UiMessage::ResultWriteBackStandard {
                translated: 1,
                original: 2,
                auto_wrapped: 3,
                breaks: 4,
                indents: 5,
                manual: 6,
            },
            UiMessage::ResultLuaExecuted,
            UiMessage::ResultLuaNotExecuted,
            UiMessage::ResultCancelled,
            UiMessage::ResultPlanSaved,
            UiMessage::ResultTranslatePlanSources {
                profile_source: "explicit",
                lua_source: "project state",
            },
            UiMessage::LogRunStarted { command: "extract" },
            UiMessage::LogRunSucceeded { command: "extract" },
            UiMessage::LogRunFailed { command: "extract" },
            UiMessage::LogRunCancelled { command: "extract" },
            UiMessage::LogPlanResolved {
                command: "extract",
                source: "explicit",
            },
            UiMessage::LogTranslatePlanResolved {
                profile_source: "explicit",
                lua_source: "project state",
            },
            UiMessage::LogPhaseStarted { phase: "scan" },
            UiMessage::LogPhaseFinished { phase: "scan" },
            UiMessage::LogRetrySummary { count: 3 },
            UiMessage::LogNoWork { reason: "current" },
            UiMessage::LogPartialResult { count: 3 },
            UiMessage::LogPublishFinished { path: "output" },
            UiMessage::LogTranslationTaskStarted { index: 1, total: 3 },
            UiMessage::LogTranslationTaskFinished {
                index: 1,
                outcome: "complete",
            },
        ]
    }
}
