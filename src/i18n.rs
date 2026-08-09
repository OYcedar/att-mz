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
    CliUiLanguageHelp,
    CliMzAbout,
    CliMvAbout,
    CliGenericAbout,
    CliInitAbout,
    CliExtractAbout,
    CliTranslateAbout,
    CliWriteBackAbout,
    CliManualAbout,
    CliManualExportAbout,
    CliManualCheckAbout,
    CliManualApplyAbout,
    CliProjectLuaAbout,
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
    CliProfileHelp,
    CliTermsHelp,
    CliPlaceholdersHelp,
    CliProjectLuaScriptHelp,
    CliProjectLuaArgumentsHelp,
    CliManualFileHelp,
    CliUsageHeading,
    CliCommandsHeading,
    CliOptionsHeading,
    CliArgumentsHeading,
    CliOptionsMetavar,
    CliCommandMetavar,
    CliPrintHelp,
    CliPrintVersion,
    CliBlankValue,
    CliInvalidPositiveInteger,
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
    DiagnosticObject {
        subject: &'a str,
    },
    DiagnosticErrorHeading,
    DiagnosticWarningHeading,
    DiagnosticExplanation {
        reason: &'a str,
    },
    DiagnosticImpact {
        impact: &'a str,
    },
    DiagnosticResolution {
        action: &'a str,
    },
    DiagnosticRelated {
        relation: &'a str,
    },
    DiagnosticImpactValue {
        effect: &'a str,
    },
    DiagnosticResolutionValue {
        code: &'a str,
    },
    DiagnosticFailureValue {
        code: &'a str,
    },
    DiagnosticConfigurationRuleValue {
        code: &'a str,
        line: u64,
        column: u64,
        actual: u64,
        maximum: u64,
    },
    DiagnosticHttpStatus {
        status: u64,
    },
    DiagnosticRetryAfter {
        seconds: u64,
    },
    DiagnosticProviderCode {
        code: &'a str,
    },
    DiagnosticProviderType {
        kind: &'a str,
    },
    DiagnosticProviderMessage {
        message: &'a str,
    },
    DiagnosticJsonPosition {
        line: u64,
        column: u64,
    },
    DiagnosticPlaceholderRuleFile {
        number: u64,
        path: &'a str,
    },
    DiagnosticPlaceholderRuleProject {
        number: u64,
    },
    ManualExported {
        entries: u64,
        path: &'a str,
    },
    ManualChecked {
        valid: u64,
        unfilled: u64,
        errors: u64,
    },
    ManualApplied {
        applied: u64,
        unfilled: u64,
        errors: u64,
    },
    ManualValue {
        code: &'a str,
        line: u64,
        expected: u64,
        actual: u64,
    },
    PlanSourceExplicit,
    PlanSourceProjectState,
    PlanSourceProductDefault,
    NoticeInitReusePath {
        path: &'a str,
    },
    NoticeExtractReuseOwners {
        owners: &'a str,
    },
    NoticeTranslateReuseProfile {
        profile: &'a str,
    },
    NoticeNoModelRequest,
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
    ProgressExtractCommit,
    ProgressGenericInit,
    ProgressGenericExtract,
    ProgressTranslatePlanning,
    ProgressTranslateConfirmed,
    ProgressNoWork,
    ProgressProjectLua,
    ProgressWriteBackReadAssets,
    ProgressWriteBackPlanning,
    ProgressWriteBackDocuments,
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
    ResultGenericExtractUnchanged {
        files: u64,
        groups: u64,
        units: u64,
    },
    ResultGenericExtractUpdated {
        files: u64,
        groups: u64,
        units: u64,
        preserved: u64,
        cleared: u64,
    },
    ResultTranslateCompleted {
        project: &'a str,
        profile: &'a str,
    },
    ResultTranslateStatus {
        status: &'a str,
    },
    ResultTranslateStatusValue {
        status: &'a str,
    },
    ResultTranslateSummary {
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
    ResultGenericTranslateSummary {
        total: u64,
        complete: u64,
        partial: u64,
        unavailable: u64,
        cleared: u64,
        reused: u64,
        accepted: u64,
        written: u64,
        conflicted: u64,
        problems: u64,
    },
    ResultWriteBackCompleted {
        project: &'a str,
    },
    ResultProjectLuaCompleted {
        project: &'a str,
    },
    ResultOutputDirectory {
        path: &'a str,
    },
    ResultWriteBackSummary {
        translated: u64,
        original: u64,
        auto_wrapped: u64,
        breaks: u64,
        indents: u64,
        manual: u64,
    },
    ResultGenericWriteBackSummary {
        translated: u64,
        original: u64,
    },
    ResultSymbolRepairSummary {
        attempted: u64,
        repaired: u64,
        skipped: u64,
        replacements: u64,
    },
    ResultRunLog {
        path: &'a str,
    },
    TranslateIncompleteObject {
        project: &'a str,
    },
    TranslateIncompleteRpgMakerReason {
        partial: u64,
        unavailable: u64,
        protocol: u64,
        exhausted: u64,
        remaining_decisions: u64,
        remaining_locations: u64,
    },
    TranslateIncompleteGenericReason {
        partial: u64,
        unavailable: u64,
        conflicted: u64,
        problems: u64,
    },
    TranslateIncompleteHelp,
    ResultCancelled,
    ResultPlanSaved,
    LogRunStarted {
        command: &'a str,
    },
    LogRunSucceeded {
        command: &'a str,
    },
    LogRunFailed {
        command: &'a str,
    },
    LogRunOutcomeUnknown {
        command: &'a str,
    },
    LogRunCancelled {
        command: &'a str,
    },
    LogRunRecoveryRequired {
        command: &'a str,
    },
    LogPerformanceCounters {
        sqlite_control_attempted_total: u64,
        candidate_validation_started: u64,
        candidate_validation_completed: u64,
    },
    LogLuaPrint {
        message: &'a str,
    },
    LogPlanResolved {
        command: &'a str,
        source: &'a str,
    },
    LogPhaseStarted {
        phase: &'a str,
    },
    LogPhaseCompleted {
        phase: &'a str,
    },
    LogPhaseStopped {
        phase: &'a str,
        outcome: &'a str,
    },
    LogCancellationRequested {
        confirmed: u64,
        total: u64,
    },
    LogCancellationRequestedIndeterminate {
        confirmed: u64,
    },
    LogRunPlanFinalized {
        result: &'a str,
    },
    LogTranslationFinished {
        result: &'a str,
    },
    LogPublicationStarted {
        path: &'a str,
    },
    LogPublicationFinished {
        result: &'a str,
    },
    LogRetrySummary {
        count: u64,
    },
    LogTranslationTaskStarted {
        index: u64,
        total: u64,
    },
    LogTranslationTaskFinished {
        index: u64,
        outcome: &'a str,
    },
    LogTaskOutcomeValue {
        outcome: &'a str,
    },
    TaskRecordTitle,
    TaskRecordFinalResultHeading,
    TaskRecordFinalStatus {
        state: &'a str,
    },
    TaskRecordAcceptedWritten {
        accepted: u64,
        written: u64,
    },
    TaskRecordAcceptedOutcomeUnknown {
        accepted: u64,
    },
    TaskRecordTaskDiagnostic,
}

impl UiMessage<'_> {
    fn key(self) -> &'static str {
        match self {
            Self::AppAbout => "app-about",
            Self::CliUiLanguageHelp => "cli-ui-language-help",
            Self::CliMzAbout => "cli-mz-about",
            Self::CliMvAbout => "cli-mv-about",
            Self::CliGenericAbout => "cli-generic-about",
            Self::CliInitAbout => "cli-init-about",
            Self::CliExtractAbout => "cli-extract-about",
            Self::CliTranslateAbout => "cli-translate-about",
            Self::CliWriteBackAbout => "cli-write-back-about",
            Self::CliManualAbout => "cli-manual-about",
            Self::CliManualExportAbout => "cli-manual-export-about",
            Self::CliManualCheckAbout => "cli-manual-check-about",
            Self::CliManualApplyAbout => "cli-manual-apply-about",
            Self::CliProjectLuaAbout => "cli-project-lua-about",
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
            Self::CliProfileHelp => "cli-profile-help",
            Self::CliTermsHelp => "cli-terms-help",
            Self::CliPlaceholdersHelp => "cli-placeholders-help",
            Self::CliProjectLuaScriptHelp => "cli-project-lua-script-help",
            Self::CliProjectLuaArgumentsHelp => "cli-project-lua-arguments-help",
            Self::CliManualFileHelp => "cli-manual-file-help",
            Self::CliUsageHeading => "cli-usage-heading",
            Self::CliCommandsHeading => "cli-commands-heading",
            Self::CliOptionsHeading => "cli-options-heading",
            Self::CliArgumentsHeading => "cli-arguments-heading",
            Self::CliOptionsMetavar => "cli-options-metavar",
            Self::CliCommandMetavar => "cli-command-metavar",
            Self::CliPrintHelp => "cli-print-help",
            Self::CliPrintVersion => "cli-print-version",
            Self::CliBlankValue => "cli-blank-value",
            Self::CliInvalidPositiveInteger => "cli-invalid-positive-integer",
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
            Self::DiagnosticObject { .. } => "diagnostic-object",
            Self::DiagnosticErrorHeading => "diagnostic-error-heading",
            Self::DiagnosticWarningHeading => "diagnostic-warning-heading",
            Self::DiagnosticExplanation { .. } => "diagnostic-explanation",
            Self::DiagnosticImpact { .. } => "diagnostic-impact",
            Self::DiagnosticResolution { .. } => "diagnostic-resolution",
            Self::DiagnosticRelated { .. } => "diagnostic-related",
            Self::DiagnosticImpactValue { .. } => "diagnostic-impact-value",
            Self::DiagnosticResolutionValue { .. } => "diagnostic-resolution-value",
            Self::DiagnosticFailureValue { .. } => "diagnostic-failure-value",
            Self::DiagnosticConfigurationRuleValue { .. } => "diagnostic-configuration-rule-value",
            Self::DiagnosticHttpStatus { .. } => "diagnostic-http-status",
            Self::DiagnosticRetryAfter { .. } => "diagnostic-retry-after",
            Self::DiagnosticProviderCode { .. } => "diagnostic-provider-code",
            Self::DiagnosticProviderType { .. } => "diagnostic-provider-type",
            Self::DiagnosticProviderMessage { .. } => "diagnostic-provider-message",
            Self::DiagnosticJsonPosition { .. } => "diagnostic-json-position",
            Self::DiagnosticPlaceholderRuleFile { .. } => "diagnostic-placeholder-rule-file",
            Self::DiagnosticPlaceholderRuleProject { .. } => "diagnostic-placeholder-rule-project",
            Self::ManualExported { .. } => "manual-exported",
            Self::ManualChecked { .. } => "manual-checked",
            Self::ManualApplied { .. } => "manual-applied",
            Self::ManualValue { .. } => "manual-value",
            Self::PlanSourceExplicit => "plan-source-explicit",
            Self::PlanSourceProjectState => "plan-source-project-state",
            Self::PlanSourceProductDefault => "plan-source-product-default",
            Self::NoticeInitReusePath { .. } => "notice-init-reuse-path",
            Self::NoticeExtractReuseOwners { .. } => "notice-extract-reuse-owners",
            Self::NoticeTranslateReuseProfile { .. } => "notice-translate-reuse-profile",
            Self::NoticeNoModelRequest => "notice-no-model-request",
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
            Self::ProgressExtractCommit => "progress-extract-commit",
            Self::ProgressGenericInit => "progress-generic-init",
            Self::ProgressGenericExtract => "progress-generic-extract",
            Self::ProgressTranslatePlanning => "progress-translate-planning",
            Self::ProgressTranslateConfirmed => "progress-translate-confirmed",
            Self::ProgressNoWork => "progress-no-work",
            Self::ProgressProjectLua => "progress-project-lua",
            Self::ProgressWriteBackReadAssets => "progress-write-back-read-assets",
            Self::ProgressWriteBackPlanning => "progress-write-back-planning",
            Self::ProgressWriteBackDocuments => "progress-write-back-documents",
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
            Self::ResultGenericExtractUnchanged { .. } => "result-generic-extract-unchanged",
            Self::ResultGenericExtractUpdated { .. } => "result-generic-extract-updated",
            Self::ResultTranslateCompleted { .. } => "result-translate-completed",
            Self::ResultTranslateStatus { .. } => "result-translate-status",
            Self::ResultTranslateStatusValue { .. } => "result-translate-status-value",
            Self::ResultTranslateSummary { .. } => "result-translate-summary",
            Self::ResultTranslateConvergence { .. } => "result-translate-convergence",
            Self::ResultGenericTranslateSummary { .. } => "result-generic-translate-summary",
            Self::ResultWriteBackCompleted { .. } => "result-write-back-completed",
            Self::ResultProjectLuaCompleted { .. } => "result-project-lua-completed",
            Self::ResultOutputDirectory { .. } => "result-output-directory",
            Self::ResultWriteBackSummary { .. } => "result-write-back-summary",
            Self::ResultGenericWriteBackSummary { .. } => "result-generic-write-back-summary",
            Self::ResultSymbolRepairSummary { .. } => "result-symbol-repair-summary",
            Self::ResultRunLog { .. } => "result-run-log",
            Self::TranslateIncompleteObject { .. } => "translate-incomplete-object",
            Self::TranslateIncompleteRpgMakerReason { .. } => {
                "translate-incomplete-rpg-maker-reason"
            }
            Self::TranslateIncompleteGenericReason { .. } => "translate-incomplete-generic-reason",
            Self::TranslateIncompleteHelp => "translate-incomplete-help",
            Self::ResultCancelled => "result-cancelled",
            Self::ResultPlanSaved => "result-plan-saved",
            Self::LogRunStarted { .. } => "log-run-started",
            Self::LogRunSucceeded { .. } => "log-run-succeeded",
            Self::LogRunFailed { .. } => "log-run-failed",
            Self::LogRunOutcomeUnknown { .. } => "log-run-outcome-unknown",
            Self::LogRunCancelled { .. } => "log-run-cancelled",
            Self::LogRunRecoveryRequired { .. } => "log-run-recovery-required",
            Self::LogPerformanceCounters { .. } => "log-performance-counters",
            Self::LogLuaPrint { .. } => "log-lua-print",
            Self::LogPlanResolved { .. } => "log-plan-resolved",
            Self::LogPhaseStarted { .. } => "log-phase-started",
            Self::LogPhaseCompleted { .. } => "log-phase-completed",
            Self::LogPhaseStopped { .. } => "log-phase-stopped",
            Self::LogCancellationRequested { .. } => "log-cancellation-requested",
            Self::LogCancellationRequestedIndeterminate { .. } => {
                "log-cancellation-requested-indeterminate"
            }
            Self::LogRunPlanFinalized { .. } => "log-run-plan-finalized",
            Self::LogTranslationFinished { .. } => "log-translation-finished",
            Self::LogPublicationStarted { .. } => "log-publication-started",
            Self::LogPublicationFinished { .. } => "log-publication-finished",
            Self::LogRetrySummary { .. } => "log-retry-summary",
            Self::LogTranslationTaskStarted { .. } => "log-translation-task-started",
            Self::LogTranslationTaskFinished { .. } => "log-translation-task-finished",
            Self::LogTaskOutcomeValue { .. } => "log-task-outcome-value",
            Self::TaskRecordTitle => "task-record-title",
            Self::TaskRecordFinalResultHeading => "task-record-final-result-heading",
            Self::TaskRecordFinalStatus { .. } => "task-record-final-status",
            Self::TaskRecordAcceptedWritten { .. } => "task-record-accepted-written",
            Self::TaskRecordAcceptedOutcomeUnknown { .. } => "task-record-accepted-outcome-unknown",
            Self::TaskRecordTaskDiagnostic => "task-record-task-diagnostic",
        }
    }

    fn arguments(self) -> FluentArgs<'static> {
        let mut arguments = FluentArgs::new();
        match self {
            Self::CliInvalidUiLanguageArgument { value }
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
            Self::NoticeTranslateReuseProfile { profile } => {
                set_text(&mut arguments, "profile", profile);
            }
            Self::NoticeInitReusePath { path } | Self::ResultOutputDirectory { path } => {
                set_text(&mut arguments, "path", path)
            }
            Self::NoticeExtractReuseOwners { owners } | Self::ResultInitStaleOwners { owners } => {
                set_text(&mut arguments, "owners", owners);
            }
            Self::LogRetrySummary { count } => {
                set_number(&mut arguments, "count", count);
            }
            Self::ProgressExtractOwner { owner } => {
                set_text(&mut arguments, "owner", owner);
            }
            Self::ResultInitCompleted { project }
            | Self::ResultExtractCompleted { project }
            | Self::ResultWriteBackCompleted { project }
            | Self::ResultProjectLuaCompleted { project } => {
                set_text(&mut arguments, "project", project);
            }
            Self::ResultTranslateCompleted { project, profile } => {
                set_text(&mut arguments, "project", project);
                set_text(&mut arguments, "profile", profile);
            }
            Self::ResultGenericExtractUnchanged {
                files,
                groups,
                units,
            } => {
                set_number(&mut arguments, "files", files);
                set_number(&mut arguments, "groups", groups);
                set_number(&mut arguments, "units", units);
            }
            Self::ResultGenericExtractUpdated {
                files,
                groups,
                units,
                preserved,
                cleared,
            } => {
                set_number(&mut arguments, "files", files);
                set_number(&mut arguments, "groups", groups);
                set_number(&mut arguments, "units", units);
                set_number(&mut arguments, "preserved", preserved);
                set_number(&mut arguments, "cleared", cleared);
            }
            Self::ResultTranslateSummary {
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
            Self::ResultGenericTranslateSummary {
                total,
                complete,
                partial,
                unavailable,
                cleared,
                reused,
                accepted,
                written,
                conflicted,
                problems,
            } => {
                set_number(&mut arguments, "total", total);
                set_number(&mut arguments, "complete", complete);
                set_number(&mut arguments, "partial", partial);
                set_number(&mut arguments, "unavailable", unavailable);
                set_number(&mut arguments, "cleared", cleared);
                set_number(&mut arguments, "reused", reused);
                set_number(&mut arguments, "accepted", accepted);
                set_number(&mut arguments, "written", written);
                set_number(&mut arguments, "conflicted", conflicted);
                set_number(&mut arguments, "problems", problems);
            }
            Self::ResultWriteBackSummary {
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
            Self::ResultGenericWriteBackSummary {
                translated,
                original,
            } => {
                set_number(&mut arguments, "translated", translated);
                set_number(&mut arguments, "original", original);
            }
            Self::ResultSymbolRepairSummary {
                attempted,
                repaired,
                skipped,
                replacements,
            } => {
                set_number(&mut arguments, "attempted", attempted);
                set_number(&mut arguments, "repaired", repaired);
                set_number(&mut arguments, "skipped", skipped);
                set_number(&mut arguments, "replacements", replacements);
            }
            Self::LogRunStarted { command }
            | Self::LogRunSucceeded { command }
            | Self::LogRunFailed { command }
            | Self::LogRunOutcomeUnknown { command }
            | Self::LogRunCancelled { command }
            | Self::LogRunRecoveryRequired { command } => {
                set_text(&mut arguments, "command", command);
            }
            Self::LogPerformanceCounters {
                sqlite_control_attempted_total,
                candidate_validation_started,
                candidate_validation_completed,
            } => {
                set_number(
                    &mut arguments,
                    "sqlite_control_attempted_total",
                    sqlite_control_attempted_total,
                );
                set_number(
                    &mut arguments,
                    "candidate_validation_started",
                    candidate_validation_started,
                );
                set_number(
                    &mut arguments,
                    "candidate_validation_completed",
                    candidate_validation_completed,
                );
            }
            Self::LogLuaPrint { message } => set_text(&mut arguments, "message", message),
            Self::LogPlanResolved { command, source } => {
                set_text(&mut arguments, "command", command);
                set_text(&mut arguments, "source", source);
            }
            Self::LogPhaseStarted { phase } | Self::LogPhaseCompleted { phase } => {
                set_text(&mut arguments, "phase", phase);
            }
            Self::LogPhaseStopped { phase, outcome } => {
                set_text(&mut arguments, "phase", phase);
                set_text(&mut arguments, "outcome", outcome);
            }
            Self::LogCancellationRequested { confirmed, total } => {
                set_number(&mut arguments, "confirmed", confirmed);
                set_number(&mut arguments, "total", total);
            }
            Self::LogCancellationRequestedIndeterminate { confirmed } => {
                set_number(&mut arguments, "confirmed", confirmed);
            }
            Self::LogRunPlanFinalized { result }
            | Self::LogTranslationFinished { result }
            | Self::LogPublicationFinished { result } => {
                set_text(&mut arguments, "result", result);
            }
            Self::LogPublicationStarted { path } => set_text(&mut arguments, "path", path),
            Self::LogTranslationTaskStarted { index, total } => {
                set_number(&mut arguments, "index", index);
                set_number(&mut arguments, "total", total);
            }
            Self::LogTranslationTaskFinished { index, outcome } => {
                set_number(&mut arguments, "index", index);
                set_text(&mut arguments, "outcome", outcome);
            }
            Self::LogTaskOutcomeValue { outcome } => {
                set_text(&mut arguments, "outcome", outcome);
            }
            Self::TaskRecordFinalStatus { state } => {
                set_text(&mut arguments, "state", state);
            }
            Self::TaskRecordAcceptedWritten { accepted, written } => {
                set_number(&mut arguments, "accepted", accepted);
                set_number(&mut arguments, "written", written);
            }
            Self::TaskRecordAcceptedOutcomeUnknown { accepted } => {
                set_number(&mut arguments, "accepted", accepted);
            }
            Self::TaskRecordTaskDiagnostic => {}
            Self::DiagnosticObject { subject } => set_text(&mut arguments, "subject", subject),
            Self::DiagnosticExplanation { reason } => set_text(&mut arguments, "reason", reason),
            Self::DiagnosticImpact { impact } => set_text(&mut arguments, "impact", impact),
            Self::DiagnosticResolution { action } => set_text(&mut arguments, "action", action),
            Self::DiagnosticRelated { relation } => {
                set_text(&mut arguments, "relation", relation);
            }
            Self::DiagnosticImpactValue { effect } => {
                set_text(&mut arguments, "effect", effect);
            }
            Self::DiagnosticResolutionValue { code } | Self::DiagnosticFailureValue { code } => {
                set_text(&mut arguments, "code", code);
            }
            Self::DiagnosticConfigurationRuleValue {
                code,
                line,
                column,
                actual,
                maximum,
            } => {
                set_text(&mut arguments, "code", code);
                set_number(&mut arguments, "line", line);
                set_number(&mut arguments, "column", column);
                set_number(&mut arguments, "actual", actual);
                set_number(&mut arguments, "maximum", maximum);
            }
            Self::DiagnosticHttpStatus { status } => {
                set_number(&mut arguments, "status", status);
            }
            Self::DiagnosticRetryAfter { seconds } => {
                set_number(&mut arguments, "seconds", seconds);
            }
            Self::DiagnosticProviderCode { code } => set_text(&mut arguments, "code", code),
            Self::DiagnosticProviderType { kind } => set_text(&mut arguments, "kind", kind),
            Self::DiagnosticProviderMessage { message } => {
                set_text(&mut arguments, "message", message);
            }
            Self::DiagnosticJsonPosition { line, column } => {
                set_number(&mut arguments, "line", line);
                set_number(&mut arguments, "column", column);
            }
            Self::DiagnosticPlaceholderRuleFile { number, path } => {
                set_number(&mut arguments, "number", number);
                set_text(&mut arguments, "path", path);
            }
            Self::DiagnosticPlaceholderRuleProject { number } => {
                set_number(&mut arguments, "number", number);
            }
            Self::ManualExported { entries, path } => {
                set_number(&mut arguments, "entries", entries);
                set_text(&mut arguments, "path", path);
            }
            Self::ManualChecked {
                valid,
                unfilled,
                errors,
            } => {
                set_number(&mut arguments, "valid", valid);
                set_number(&mut arguments, "unfilled", unfilled);
                set_number(&mut arguments, "errors", errors);
            }
            Self::ManualApplied {
                applied,
                unfilled,
                errors,
            } => {
                set_number(&mut arguments, "applied", applied);
                set_number(&mut arguments, "unfilled", unfilled);
                set_number(&mut arguments, "errors", errors);
            }
            Self::ManualValue {
                code,
                line,
                expected,
                actual,
            } => {
                set_text(&mut arguments, "code", code);
                set_number(&mut arguments, "line", line);
                set_number(&mut arguments, "expected", expected);
                set_number(&mut arguments, "actual", actual);
            }
            Self::ResultTranslateStatus { status }
            | Self::ResultTranslateStatusValue { status } => {
                set_text(&mut arguments, "status", status);
            }
            Self::ResultRunLog { path } => set_text(&mut arguments, "path", path),
            Self::TranslateIncompleteObject { project } => {
                set_text(&mut arguments, "project", project);
            }
            Self::TranslateIncompleteRpgMakerReason {
                partial,
                unavailable,
                protocol,
                exhausted,
                remaining_decisions,
                remaining_locations,
            } => {
                set_number(&mut arguments, "partial", partial);
                set_number(&mut arguments, "unavailable", unavailable);
                set_number(&mut arguments, "protocol", protocol);
                set_number(&mut arguments, "exhausted", exhausted);
                set_number(&mut arguments, "remaining_decisions", remaining_decisions);
                set_number(&mut arguments, "remaining_locations", remaining_locations);
            }
            Self::TranslateIncompleteGenericReason {
                partial,
                unavailable,
                conflicted,
                problems,
            } => {
                set_number(&mut arguments, "partial", partial);
                set_number(&mut arguments, "unavailable", unavailable);
                set_number(&mut arguments, "conflicted", conflicted);
                set_number(&mut arguments, "problems", problems);
            }
            Self::AppAbout
            | Self::CliUiLanguageHelp
            | Self::CliMzAbout
            | Self::CliMvAbout
            | Self::CliGenericAbout
            | Self::CliInitAbout
            | Self::CliExtractAbout
            | Self::CliTranslateAbout
            | Self::CliWriteBackAbout
            | Self::CliManualAbout
            | Self::CliManualExportAbout
            | Self::CliManualCheckAbout
            | Self::CliManualApplyAbout
            | Self::CliProjectLuaAbout
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
            | Self::CliProfileHelp
            | Self::CliTermsHelp
            | Self::CliPlaceholdersHelp
            | Self::CliProjectLuaScriptHelp
            | Self::CliProjectLuaArgumentsHelp
            | Self::CliManualFileHelp
            | Self::CliUsageHeading
            | Self::CliCommandsHeading
            | Self::CliOptionsHeading
            | Self::CliArgumentsHeading
            | Self::CliOptionsMetavar
            | Self::CliCommandMetavar
            | Self::CliPrintHelp
            | Self::CliPrintVersion
            | Self::CliBlankValue
            | Self::CliInvalidPositiveInteger
            | Self::CliUiLanguageEnvironmentNotUnicode
            | Self::CliErrorHeading
            | Self::CliTryHelp
            | Self::CliMissingSubcommand
            | Self::CliInvalidUtf8
            | Self::CliParseFailure
            | Self::DiagnosticErrorHeading
            | Self::DiagnosticWarningHeading
            | Self::PlanSourceExplicit
            | Self::PlanSourceProjectState
            | Self::PlanSourceProductDefault
            | Self::NoticeNoModelRequest
            | Self::ProgressInitCheckProject
            | Self::ProgressInitScanSource
            | Self::ProgressInitBuildCandidate
            | Self::ProgressInitConvergeDatabase
            | Self::ProgressInitPublish
            | Self::ProgressSaveRunPlan
            | Self::ProgressExtractDocuments
            | Self::ProgressExtractBuiltin
            | Self::ProgressExtractRules
            | Self::ProgressExtractCommit
            | Self::ProgressGenericInit
            | Self::ProgressGenericExtract
            | Self::ProgressTranslatePlanning
            | Self::ProgressTranslateConfirmed
            | Self::ProgressNoWork
            | Self::ProgressProjectLua
            | Self::ProgressWriteBackReadAssets
            | Self::ProgressWriteBackPlanning
            | Self::ProgressWriteBackDocuments
            | Self::ProgressWriteBackValidateCandidate
            | Self::ProgressWriteBackPublish
            | Self::ProgressFinalizing
            | Self::ProgressSafeStopping
            | Self::ResultInitCreated
            | Self::ResultInitUnchanged
            | Self::ResultInitUpdated
            | Self::ResultCancelled
            | Self::ResultPlanSaved
            | Self::TranslateIncompleteHelp
            | Self::TaskRecordTitle
            | Self::TaskRecordFinalResultHeading => {}
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

fn set_text(arguments: &mut FluentArgs<'static>, name: &'static str, value: &str) {
    arguments.set(name, sanitize_user_text(value));
}

fn set_number(arguments: &mut FluentArgs<'static>, name: &'static str, value: u64) {
    arguments.set(name, value);
}

/// 使用嵌入式 Fluent catalog 格式化用户可见消息。
pub(crate) struct UiLocalizer {
    locale: UiLocale,
    selected: FluentBundle<FluentResource>,
    english_fallback: FluentBundle<FluentResource>,
}

impl UiLocalizer {
    pub(crate) fn new(locale: UiLocale) -> Self {
        Self {
            locale,
            selected: build_bundle(locale),
            english_fallback: build_bundle(UiLocale::English),
        }
    }

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
    fn current_diagnostic_summaries_render_in_every_locale() {
        const SUMMARIES: &[&str] = &[
            "already_exists",
            "backup_incomplete",
            "cancelled",
            "concurrent_modification",
            "concurrent_shutdown",
            "conflicting_values",
            "duplicate_identifier",
            "empty_text_capture",
            "executor_closed",
            "executor_state_poisoned",
            "external_service_rejected",
            "external_service_unavailable",
            "extraction_out_of_date",
            "file_identity_changed",
            "finalization_failed",
            "generic_extract_required",
            "interactive_session_already_open",
            "internal_invariant",
            "invalid_content",
            "invalid_encoding",
            "invalid_path",
            "invalid_response_contract",
            "invalid_syntax",
            "invalid_value",
            "journal_corrupt",
            "lock_cancelled",
            "lua_compilation_failed",
            "lua_execution_failed",
            "manual_layout_required",
            "missing_required_value",
            "non_local_volume",
            "non_ntfs_volume",
            "not_found",
            "operation_failed",
            "placeholder_projection_failed",
            "profile_not_found",
            "recovery_required",
            "reparse_point_forbidden",
            "request_serialization_failed",
            "unsupported_windows_code_page",
            "resource_limit",
            "resource_limit_exceeded",
            "response_parsing_failed",
            "rollback_failed",
            "rules_owner_disabled",
            "rules_invalid_capture_range",
            "rules_missing_text_capture",
            "rules_overlapping_capture",
            "rules_pattern_match_failed",
            "rules_zero_width_match",
            "source_snapshot_mismatch",
            "state_mismatch",
            "stderr_flush_failed",
            "stderr_write_failed",
            "stdout_flush_failed",
            "stdout_write_failed",
            "target_already_exists",
            "transaction_outcome_unknown",
            "transaction_rolled_back",
            "transport_failed",
            "unavailable",
            "unexpected_artifact",
            "worker_channel_closed",
            "worker_panicked",
            "worker_spawn_failed",
            "write_back_candidate_invalid",
            "write_back_recovery_required",
            "wrong_publisher_instance",
            "case_sensitive_directory",
        ];

        for locale in UiLocale::ALL {
            let localizer = UiLocalizer::new(locale);
            for code in SUMMARIES {
                let rendered = localizer.format(UiMessage::DiagnosticFailureValue { code });
                assert!(
                    !rendered.contains("__ATT_FALLBACK__"),
                    "{locale} 缺少诊断摘要 {code}"
                );
            }
        }
    }

    #[test]
    fn current_diagnostic_resolutions_render_in_every_locale() {
        for locale in UiLocale::ALL {
            let localizer = UiLocalizer::new(locale);
            for code in [
                "fix_configuration",
                "fix_input",
                "fix_placeholder_rules",
                "review_disabled_rules",
                "adjust_manual_layout",
                "check_path_and_permissions",
                "check_project_state",
                "resolve_contention",
                "check_model_service",
                "preserve_recovery_artifacts",
                "retry",
                "report_bug",
            ] {
                let rendered = localizer.format(UiMessage::DiagnosticResolutionValue { code });
                assert!(
                    !rendered.contains("__ATT_FALLBACK__"),
                    "{locale} 缺少诊断处理办法 {code}"
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
    fn project_log_value_sources_have_closed_localized_labels() {
        for source in ["explicit", "project_state", "product_default"] {
            assert!(project_log_value_source_label(source).is_some(), "{source}");
        }
        assert!(project_log_value_source_label("future").is_none());
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
            UiMessage::CliUiLanguageHelp,
            UiMessage::CliMzAbout,
            UiMessage::CliMvAbout,
            UiMessage::CliGenericAbout,
            UiMessage::CliInitAbout,
            UiMessage::CliExtractAbout,
            UiMessage::CliTranslateAbout,
            UiMessage::CliWriteBackAbout,
            UiMessage::CliManualAbout,
            UiMessage::CliManualExportAbout,
            UiMessage::CliManualCheckAbout,
            UiMessage::CliManualApplyAbout,
            UiMessage::CliProjectLuaAbout,
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
            UiMessage::CliProfileHelp,
            UiMessage::CliTermsHelp,
            UiMessage::CliPlaceholdersHelp,
            UiMessage::CliProjectLuaScriptHelp,
            UiMessage::CliProjectLuaArgumentsHelp,
            UiMessage::CliManualFileHelp,
            UiMessage::CliUsageHeading,
            UiMessage::CliCommandsHeading,
            UiMessage::CliOptionsHeading,
            UiMessage::CliArgumentsHeading,
            UiMessage::CliOptionsMetavar,
            UiMessage::CliCommandMetavar,
            UiMessage::CliPrintHelp,
            UiMessage::CliPrintVersion,
            UiMessage::CliBlankValue,
            UiMessage::CliInvalidPositiveInteger,
            UiMessage::CliInvalidUiLanguageArgument { value: "bad" },
            UiMessage::CliUnsupportedUiLanguageArgument { value: "de" },
            UiMessage::CliInvalidUiLanguageEnvironment { value: "bad" },
            UiMessage::CliUnsupportedUiLanguageEnvironment { value: "de" },
            UiMessage::CliUiLanguageEnvironmentNotUnicode,
            UiMessage::CliUnexpectedArgument { value: "--bad" },
            UiMessage::CliMissingRequiredArgument { value: "--name" },
            UiMessage::CliInvalidValue {
                value: "bad",
                argument: "--source-language",
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
            UiMessage::DiagnosticObject {
                subject: "project demo",
            },
            UiMessage::DiagnosticErrorHeading,
            UiMessage::DiagnosticWarningHeading,
            UiMessage::DiagnosticExplanation {
                reason: "state mismatch",
            },
            UiMessage::DiagnosticImpact {
                impact: "state unchanged",
            },
            UiMessage::DiagnosticResolution {
                action: "check project state",
            },
            UiMessage::DiagnosticRelated {
                relation: "cleanup",
            },
            UiMessage::DiagnosticImpactValue {
                effect: "unchanged",
            },
            UiMessage::DiagnosticResolutionValue { code: "retry" },
            UiMessage::DiagnosticFailureValue { code: "not_found" },
            UiMessage::DiagnosticConfigurationRuleValue {
                code: "value_blank",
                line: 0,
                column: 0,
                actual: 0,
                maximum: 0,
            },
            UiMessage::DiagnosticHttpStatus { status: 429 },
            UiMessage::DiagnosticRetryAfter { seconds: 30 },
            UiMessage::DiagnosticProviderCode { code: "rate_limit" },
            UiMessage::DiagnosticProviderType { kind: "request" },
            UiMessage::DiagnosticProviderMessage {
                message: "try later",
            },
            UiMessage::DiagnosticJsonPosition { line: 1, column: 2 },
            UiMessage::DiagnosticPlaceholderRuleFile {
                number: 3,
                path: "placeholders.toml",
            },
            UiMessage::DiagnosticPlaceholderRuleProject { number: 3 },
            UiMessage::ManualExported {
                entries: 2,
                path: "manual.toml",
            },
            UiMessage::ManualChecked {
                valid: 1,
                unfilled: 2,
                errors: 3,
            },
            UiMessage::ManualApplied {
                applied: 1,
                unfilled: 2,
                errors: 3,
            },
            UiMessage::ManualValue {
                code: "fixed_length",
                line: 1,
                expected: 2,
                actual: 1,
            },
            UiMessage::PlanSourceExplicit,
            UiMessage::PlanSourceProjectState,
            UiMessage::PlanSourceProductDefault,
            UiMessage::NoticeInitReusePath { path: "path" },
            UiMessage::NoticeExtractReuseOwners { owners: "owners" },
            UiMessage::NoticeTranslateReuseProfile { profile: "profile" },
            UiMessage::NoticeNoModelRequest,
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
            UiMessage::ProgressExtractCommit,
            UiMessage::ProgressGenericInit,
            UiMessage::ProgressGenericExtract,
            UiMessage::ProgressTranslatePlanning,
            UiMessage::ProgressTranslateConfirmed,
            UiMessage::ProgressNoWork,
            UiMessage::ProgressProjectLua,
            UiMessage::ProgressWriteBackReadAssets,
            UiMessage::ProgressWriteBackPlanning,
            UiMessage::ProgressWriteBackDocuments,
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
            UiMessage::ResultGenericExtractUnchanged {
                files: 1,
                groups: 2,
                units: 3,
            },
            UiMessage::ResultGenericExtractUpdated {
                files: 1,
                groups: 2,
                units: 3,
                preserved: 4,
                cleared: 5,
            },
            UiMessage::ResultTranslateCompleted {
                project: "demo",
                profile: "main",
            },
            UiMessage::ResultTranslateStatus { status: "complete" },
            UiMessage::ResultTranslateStatusValue { status: "complete" },
            UiMessage::ResultTranslateSummary {
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
            UiMessage::ResultGenericTranslateSummary {
                total: 1,
                complete: 2,
                partial: 3,
                unavailable: 4,
                cleared: 5,
                reused: 6,
                accepted: 7,
                written: 8,
                conflicted: 9,
                problems: 10,
            },
            UiMessage::ResultRunLog {
                path: "logs/run-000001.jsonl",
            },
            UiMessage::TranslateIncompleteObject { project: "demo" },
            UiMessage::TranslateIncompleteRpgMakerReason {
                partial: 1,
                unavailable: 2,
                protocol: 3,
                exhausted: 4,
                remaining_decisions: 5,
                remaining_locations: 6,
            },
            UiMessage::TranslateIncompleteGenericReason {
                partial: 1,
                unavailable: 2,
                conflicted: 3,
                problems: 4,
            },
            UiMessage::TranslateIncompleteHelp,
            UiMessage::ResultWriteBackCompleted { project: "demo" },
            UiMessage::ResultProjectLuaCompleted { project: "demo" },
            UiMessage::ResultOutputDirectory { path: "output" },
            UiMessage::ResultWriteBackSummary {
                translated: 1,
                original: 2,
                auto_wrapped: 3,
                breaks: 4,
                indents: 5,
                manual: 6,
            },
            UiMessage::ResultGenericWriteBackSummary {
                translated: 1,
                original: 2,
            },
            UiMessage::ResultSymbolRepairSummary {
                attempted: 1,
                repaired: 2,
                skipped: 3,
                replacements: 4,
            },
            UiMessage::ResultCancelled,
            UiMessage::ResultPlanSaved,
            UiMessage::LogRunStarted { command: "extract" },
            UiMessage::LogRunSucceeded { command: "extract" },
            UiMessage::LogRunFailed { command: "extract" },
            UiMessage::LogRunOutcomeUnknown { command: "extract" },
            UiMessage::LogRunCancelled { command: "extract" },
            UiMessage::LogRunRecoveryRequired { command: "extract" },
            UiMessage::LogPerformanceCounters {
                sqlite_control_attempted_total: 7,
                candidate_validation_started: 11,
                candidate_validation_completed: 13,
            },
            UiMessage::LogLuaPrint { message: "message" },
            UiMessage::LogPlanResolved {
                command: "extract",
                source: "explicit",
            },
            UiMessage::LogPhaseStarted { phase: "scan" },
            UiMessage::LogPhaseCompleted { phase: "scan" },
            UiMessage::LogPhaseStopped {
                phase: "scan",
                outcome: "failed",
            },
            UiMessage::LogCancellationRequested {
                confirmed: 2,
                total: 3,
            },
            UiMessage::LogCancellationRequestedIndeterminate { confirmed: 2 },
            UiMessage::LogRunPlanFinalized { result: "saved" },
            UiMessage::LogTranslationFinished { result: "complete" },
            UiMessage::LogPublicationStarted { path: "output" },
            UiMessage::LogPublicationFinished {
                result: "published",
            },
            UiMessage::LogRetrySummary { count: 3 },
            UiMessage::LogTranslationTaskStarted { index: 1, total: 3 },
            UiMessage::LogTranslationTaskFinished {
                index: 1,
                outcome: "complete",
            },
            UiMessage::LogTaskOutcomeValue {
                outcome: "complete",
            },
            UiMessage::TaskRecordTitle,
            UiMessage::TaskRecordFinalResultHeading,
            UiMessage::TaskRecordFinalStatus { state: "complete" },
            UiMessage::TaskRecordAcceptedWritten {
                accepted: 4,
                written: 6,
            },
            UiMessage::TaskRecordAcceptedOutcomeUnknown { accepted: 4 },
            UiMessage::TaskRecordTaskDiagnostic,
        ]
    }
}
