//! 严格 TOML 配置边界。
//!
//! 原始 TOML 只在本模块存在。结构和字段类型全部通过后，本模块继续建立路径基准、
//! 语言模块、LLM Client 外部约束与 Profile 唯一性；业务和根适配器只接收受信配置。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use url::Url;
use zeroize::Zeroizing;

use super::arguments::{
    ExtractArguments, InitArguments, MvCommand, MzCommand, ProductCommand, TranslateArguments,
    WriteBackArguments,
};

use crate::diagnostic::ConfigurationValueRule;
use crate::i18n::UiLocale;
use crate::language::{
    EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
    JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseQuoteRepairPolicyError,
    JapaneseResidualPolicy, LanguageId, LanguageIdError, LanguageModule, LanguageModuleCatalog,
    LanguageModuleCatalogBuildError, LanguagePolicyConfigurationError, QuotePair,
};
use crate::rpg_maker::ProjectName;
use crate::rpg_maker::extract::document::RpgMakerDocumentReadingConfig;
use crate::rpg_maker::lua::lua54::TrustedLua54RuntimeConfiguration;
use crate::rpg_maker::translate::profile::RpgMakerTranslationRequestConfiguration;
use crate::rpg_maker::{RpgMakerEngine, RpgMakerLayout};
use crate::runtime::cpu::CpuExecutorConfig;
use crate::runtime::filesystem::{DirectoryPublisherConfig, SystemFileSystemConfig};
use crate::runtime::llm::{
    LlmProxyConfiguration, OpenAiChatCompletionClient, OpenAiExecutorConfiguration,
};
use crate::runtime::sqlite::RusqliteStorageConfiguration;

const RESERVED_REQUEST_BODY_FIELDS: [&str; 3] = ["model", "messages", "stream"];

/// 根据命令行显式路径选择配置文件位置。
///
/// 相对路径以当前工作目录解析。
pub(crate) fn resolve_configuration_path(
    explicit: &Path,
    current_directory: &Path,
) -> Result<PathBuf, ConfigurationPathError> {
    if !current_directory.is_absolute() {
        return Err(ConfigurationPathError::CurrentDirectoryNotAbsolute(
            current_directory.to_path_buf(),
        ));
    }

    if explicit.as_os_str().is_empty() {
        return Err(ConfigurationPathError::EmptyExplicitPath);
    }
    Ok(resolve_path(current_directory, explicit))
}

/// 读取配置，并且只建立本次命令实际消费的受信配置。
pub(crate) fn load_product_configuration(
    requested_path: &Path,
    product: ProductCommand,
) -> Result<ConfiguredProductCommand, ConfigurationLoadError> {
    let configuration_path =
        std::fs::canonicalize(requested_path).map_err(|source| ConfigurationLoadError::Open {
            path: requested_path.to_path_buf(),
            source,
        })?;
    let mut file =
        File::open(&configuration_path).map_err(|source| ConfigurationLoadError::Open {
            path: configuration_path.clone(),
            source,
        })?;
    let metadata = file
        .metadata()
        .map_err(|source| ConfigurationLoadError::Read {
            path: configuration_path.clone(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(ConfigurationLoadError::NotAFile {
            path: configuration_path,
        });
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.by_ref()
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigurationLoadError::Read {
            path: configuration_path.clone(),
            source,
        })?;

    let source = std::str::from_utf8(bytes.as_slice()).map_err(|source| {
        ConfigurationLoadError::InvalidUtf8 {
            path: configuration_path.clone(),
            valid_up_to: source.valid_up_to(),
            error_len: source.error_len(),
        }
    })?;
    validate_configuration_field_names(source, &configuration_path)?;
    let configuration_directory = configuration_path
        .parent()
        .expect("规范绝对文件路径必须拥有父目录")
        .to_path_buf();
    let (layout, command, dialogue_rules_path) = normalize_product_command(product);
    ConfiguredRpgMakerCommand::build(
        &configuration_path,
        &configuration_directory,
        source,
        layout,
        command,
        dialogue_rules_path,
    )
    .map(|command| ConfiguredProductCommand { layout, command })
    .map_err(|error| error.with_configuration_path(&configuration_path))
}

#[cfg(test)]
fn load_configuration(
    requested_path: &Path,
    command: MzCommand,
) -> Result<ConfiguredRpgMakerCommand, ConfigurationLoadError> {
    load_product_configuration(requested_path, ProductCommand::Mz { command })
        .map(|configured| configured.command)
}

pub(crate) struct ConfiguredProductCommand {
    layout: RpgMakerLayout,
    command: ConfiguredRpgMakerCommand,
}

impl ConfiguredProductCommand {
    pub(crate) fn into_parts(self) -> (RpgMakerLayout, ConfiguredRpgMakerCommand) {
        (self.layout, self.command)
    }
}

fn normalize_product_command(
    product: ProductCommand,
) -> (RpgMakerLayout, RpgMakerCommandArguments, Option<PathBuf>) {
    match product {
        ProductCommand::Mz { command } => (
            RpgMakerLayout::MZ,
            RpgMakerCommandArguments::from(command),
            None,
        ),
        ProductCommand::Mv { command } => match command {
            MvCommand::Init(arguments) => (
                RpgMakerLayout::MV,
                RpgMakerCommandArguments::Init(arguments),
                None,
            ),
            MvCommand::Extract(arguments) => (
                RpgMakerLayout::MV,
                RpgMakerCommandArguments::Extract(ExtractArguments {
                    project: arguments.project,
                    builtin: arguments.builtin,
                    rules: arguments.rules,
                    lua: arguments.lua,
                }),
                arguments.dialogue_rules,
            ),
            MvCommand::Translate(arguments) => (
                RpgMakerLayout::MV,
                RpgMakerCommandArguments::Translate(arguments),
                None,
            ),
            MvCommand::WriteBack(arguments) => (
                RpgMakerLayout::MV,
                RpgMakerCommandArguments::WriteBack(arguments),
                None,
            ),
        },
    }
}

enum RpgMakerCommandArguments {
    Init(InitArguments),
    Extract(ExtractArguments),
    Translate(TranslateArguments),
    WriteBack(WriteBackArguments),
}

impl From<MzCommand> for RpgMakerCommandArguments {
    fn from(command: MzCommand) -> Self {
        match command {
            MzCommand::Init(arguments) => Self::Init(arguments),
            MzCommand::Extract(arguments) => Self::Extract(arguments),
            MzCommand::Translate(arguments) => Self::Translate(arguments),
            MzCommand::WriteBack(arguments) => Self::WriteBack(arguments),
        }
    }
}

/// 四个互斥命令各自拥有且只拥有现实消费的配置。
pub(crate) enum ConfiguredRpgMakerCommand {
    Init(ConfiguredInitCommand),
    Extract(ConfiguredExtractCommand),
    Translate(Box<ConfiguredTranslateCommand>),
    WriteBack(ConfiguredWriteBackCommand),
}

impl ConfiguredRpgMakerCommand {
    fn build(
        configuration_path: &Path,
        configuration_directory: &Path,
        source: &str,
        layout: RpgMakerLayout,
        command: RpgMakerCommandArguments,
        dialogue_rules_path: Option<PathBuf>,
    ) -> Result<Self, ConfigurationLoadError> {
        let raw_common: RawCommonConfiguration = parse_selected(source, configuration_path)?;
        let common = CommonCommandConfiguration::build(configuration_directory, raw_common)
            .map_err(ConfigurationLoadError::InvalidValue)?;

        match command {
            RpgMakerCommandArguments::Init(arguments) => {
                let _: RawInitSelection = parse_selected(source, configuration_path)?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine(),
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                Ok(Self::Init(ConfiguredInitCommand {
                    arguments,
                    common,
                    publisher,
                }))
            }
            RpgMakerCommandArguments::Extract(arguments) => {
                let deferred_source =
                    Arc::new(DeferredConfigurationSource::new(configuration_path, source));
                let deferred_lua =
                    DeferredLuaRuntimeConfiguration::new(Arc::clone(&deferred_source));
                let _: RawExtractSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration();
                let ExtractArguments {
                    project,
                    builtin,
                    rules,
                    lua,
                } = arguments;
                let lua = lua
                    .map(|script_path| {
                        deferred_lua
                            .resolve()
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                let rpg_maker = ExtractConfiguration::build(builtin, rules);
                Ok(Self::Extract(ConfiguredExtractCommand {
                    project_name: project.name,
                    common,
                    cpu,
                    lua,
                    deferred_lua,
                    rpg_maker,
                    dialogue_rules_path,
                }))
            }
            RpgMakerCommandArguments::Translate(arguments) => {
                let deferred_source =
                    Arc::new(DeferredConfigurationSource::new(configuration_path, source));
                let deferred_lua =
                    DeferredLuaRuntimeConfiguration::new(Arc::clone(&deferred_source));
                let raw: RawTranslateSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration();
                let record_translation_tasks = raw.rpg_maker.record_translation_tasks;
                let TranslateArguments {
                    project,
                    profile_id,
                    terms,
                    placeholders,
                    lua,
                } = arguments;
                let lua = lua
                    .map(|script_path| {
                        deferred_lua
                            .resolve()
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                let rpg_maker = PendingTranslateConfiguration::build(
                    configuration_directory,
                    raw.prompts,
                    raw.languages,
                    raw.rpg_maker,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                let configured = ConfiguredTranslateCommand {
                    project_name: project.name,
                    terminology_path: terms,
                    placeholder_rules_path: placeholders,
                    common,
                    cpu,
                    lua,
                    deferred_lua,
                    record_translation_tasks,
                    profile: ConfiguredTranslateProfile::Deferred {
                        source: deferred_source,
                        configuration: rpg_maker,
                    },
                };
                let configured = match profile_id {
                    Some(profile_id) => configured.resolve_profile(&profile_id)?,
                    None => configured,
                };
                Ok(Self::Translate(Box::new(configured)))
            }
            RpgMakerCommandArguments::WriteBack(arguments) => {
                let deferred_source =
                    Arc::new(DeferredConfigurationSource::new(configuration_path, source));
                let deferred_lua = DeferredLuaRuntimeConfiguration::new(deferred_source);
                let _: RawWriteBackSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration();
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine(),
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                let WriteBackArguments { project, lua } = arguments;
                let lua = lua
                    .map(|script_path| {
                        deferred_lua
                            .resolve()
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                let rpg_maker = WriteBackConfiguration::build();
                Ok(Self::WriteBack(ConfiguredWriteBackCommand {
                    project_name: project.name,
                    common,
                    cpu,
                    publisher,
                    lua,
                    deferred_lua,
                    rpg_maker,
                }))
            }
        }
    }
}

pub(crate) struct CommonCommandConfiguration {
    projects_root: PathBuf,
    filesystem: SystemFileSystemConfig,
    sqlite: RusqliteStorageConfiguration,
}

impl CommonCommandConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawCommonConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        let projects_root =
            checked_path("projects.root", configuration_directory, raw.projects.root)?;
        Ok(Self {
            projects_root,
            filesystem: build_file_system_configuration(),
            sqlite: build_sqlite_configuration(),
        })
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) const fn filesystem(&self) -> &SystemFileSystemConfig {
        &self.filesystem
    }

    pub(crate) const fn sqlite(&self) -> &RusqliteStorageConfiguration {
        &self.sqlite
    }
}

pub(crate) struct ConfiguredInitCommand {
    pub(crate) arguments: InitArguments,
    common: CommonCommandConfiguration,
    publisher: DirectoryPublisherConfig,
}

impl ConfiguredInitCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn publisher(&self) -> &DirectoryPublisherConfig {
        &self.publisher
    }
}

pub(crate) struct ConfiguredExtractCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    lua: Option<SelectedLuaConfiguration>,
    deferred_lua: DeferredLuaRuntimeConfiguration,
    rpg_maker: ExtractConfiguration,
    dialogue_rules_path: Option<PathBuf>,
}

impl ConfiguredExtractCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn lua(&self) -> Option<&SelectedLuaConfiguration> {
        self.lua.as_ref()
    }

    /// 仅在项目状态要求复用 Lua 程序时解析并校验 Lua 运行时配置。
    pub(crate) fn resolve_lua_runtime(
        &self,
    ) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
        self.deferred_lua.resolve()
    }

    pub(crate) const fn rpg_maker(&self) -> &ExtractConfiguration {
        &self.rpg_maker
    }

    pub(crate) fn dialogue_rules_path(&self) -> Option<&Path> {
        self.dialogue_rules_path.as_deref()
    }
}

pub(crate) struct ConfiguredTranslateCommand {
    project_name: ProjectName,
    terminology_path: Option<PathBuf>,
    placeholder_rules_path: Option<PathBuf>,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    lua: Option<SelectedLuaConfiguration>,
    deferred_lua: DeferredLuaRuntimeConfiguration,
    record_translation_tasks: bool,
    profile: ConfiguredTranslateProfile,
}

enum ConfiguredTranslateProfile {
    Deferred {
        source: Arc<DeferredConfigurationSource>,
        configuration: PendingTranslateConfiguration,
    },
    Resolved(Box<TranslateConfiguration>),
}

impl ConfiguredTranslateCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn terminology_path(&self) -> Option<&Path> {
        self.terminology_path.as_deref()
    }

    pub(crate) fn placeholder_rules_path(&self) -> Option<&Path> {
        self.placeholder_rules_path.as_deref()
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) fn llm(&self) -> &SelectedLlmExecutorConfiguration {
        self.rpg_maker().llm()
    }

    pub(crate) const fn lua(&self) -> Option<&SelectedLuaConfiguration> {
        self.lua.as_ref()
    }

    pub(crate) const fn record_translation_tasks(&self) -> bool {
        self.record_translation_tasks
    }

    /// 仅在项目状态要求复用 Lua 程序时解析并校验 Lua 运行时配置。
    pub(crate) fn resolve_lua_runtime(
        &self,
    ) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
        self.deferred_lua.resolve()
    }

    /// 返回已经在命令行显式选择并于加载阶段完成校验的 Profile。
    ///
    /// `None` 表示调用方必须从项目运行方案取得 Profile，并调用
    /// [`Self::resolve_profile`]；不会隐式选择配置中的其他条目。
    pub(crate) fn resolved_profile_id(&self) -> Option<&str> {
        match &self.profile {
            ConfiguredTranslateProfile::Deferred { .. } => None,
            ConfiguredTranslateProfile::Resolved(configuration) => {
                Some(configuration.profile().id())
            }
        }
    }

    /// 消费待解析命令，并精确选择调用方提供的 Profile。
    ///
    /// 显式 Profile 已在加载阶段解析；用同一 ID 再调用是幂等的。若调用方
    /// 提供不同 ID，则返回配置错误，避免项目状态覆盖显式命令行意图。
    pub(crate) fn resolve_profile(self, profile_id: &str) -> Result<Self, ConfigurationLoadError> {
        let configuration_path = self.configuration_path().to_path_buf();
        validate_exact_identifier("Profile ID", profile_id)
            .map_err(ConfigurationLoadError::InvalidValue)
            .map_err(|error| error.with_configuration_path(&configuration_path))?;

        let Self {
            project_name,
            terminology_path,
            placeholder_rules_path,
            common,
            cpu,
            lua,
            deferred_lua,
            record_translation_tasks,
            profile,
        } = self;
        let profile = match profile {
            ConfiguredTranslateProfile::Deferred {
                source,
                configuration,
            } => ConfiguredTranslateProfile::Resolved(Box::new(
                configuration.resolve(source.as_ref(), profile_id)?,
            )),
            ConfiguredTranslateProfile::Resolved(configuration)
                if configuration.profile().id() == profile_id =>
            {
                ConfiguredTranslateProfile::Resolved(configuration)
            }
            ConfiguredTranslateProfile::Resolved(configuration) => {
                let explicit_profile = configuration.profile().id().to_owned();
                return Err(ConfigurationLoadError::ProfileSelectionConflict {
                    path: configuration_path,
                    explicit_profile,
                    requested_profile: profile_id.to_owned(),
                });
            }
        };
        Ok(Self {
            project_name,
            terminology_path,
            placeholder_rules_path,
            common,
            cpu,
            lua,
            deferred_lua,
            record_translation_tasks,
            profile,
        })
    }

    fn configuration_path(&self) -> &Path {
        self.deferred_lua.source.path()
    }

    #[cfg(test)]
    pub(crate) fn client(&self) -> &Arc<OpenAiChatCompletionClient> {
        self.rpg_maker().client()
    }

    pub(crate) fn rpg_maker(&self) -> &TranslateConfiguration {
        match &self.profile {
            ConfiguredTranslateProfile::Deferred { .. } => {
                panic!("Translate Profile 必须在业务装配前完成解析")
            }
            ConfiguredTranslateProfile::Resolved(configuration) => configuration,
        }
    }
}

pub(crate) struct ConfiguredWriteBackCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    publisher: DirectoryPublisherConfig,
    lua: Option<SelectedLuaConfiguration>,
    deferred_lua: DeferredLuaRuntimeConfiguration,
    rpg_maker: WriteBackConfiguration,
}

impl ConfiguredWriteBackCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn publisher(&self) -> &DirectoryPublisherConfig {
        &self.publisher
    }

    pub(crate) const fn lua(&self) -> Option<&SelectedLuaConfiguration> {
        self.lua.as_ref()
    }

    /// 仅在项目状态要求复用 Lua 程序时解析并校验 Lua 运行时配置。
    pub(crate) fn resolve_lua_runtime(
        &self,
    ) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
        self.deferred_lua.resolve()
    }

    pub(crate) const fn rpg_maker(&self) -> &WriteBackConfiguration {
        &self.rpg_maker
    }
}

fn build_cpu_configuration() -> CpuExecutorConfig {
    CpuExecutorConfig::production()
}

fn build_file_system_configuration() -> SystemFileSystemConfig {
    SystemFileSystemConfig::production()
}

fn build_directory_publisher_configuration(
    projects_root: &Path,
    engine: RpgMakerEngine,
) -> Result<DirectoryPublisherConfig, ConfigurationValueError> {
    DirectoryPublisherConfig::production(
        projects_root
            .join(".att-locks")
            .join("directory-publish")
            .join(engine.storage_name()),
    )
    .map_err(|_| {
        invalid(
            "projects.root",
            ConfigurationValueRule::RuntimeConfigurationInvalid,
        )
    })
}

fn build_sqlite_configuration() -> RusqliteStorageConfiguration {
    RusqliteStorageConfiguration::production()
}

/// 保留仅能在项目运行方案解析后按需消费的配置原文。
///
/// 配置可能包含凭据，因此不实现 `Debug`，并在最后一个引用释放时清零正文。
struct DeferredConfigurationSource {
    path: PathBuf,
    source: Zeroizing<String>,
}

impl DeferredConfigurationSource {
    fn new(path: &Path, source: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            source: Zeroizing::new(source.to_owned()),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source(&self) -> &str {
        self.source.as_str()
    }
}

struct DeferredLuaRuntimeConfiguration {
    source: Arc<DeferredConfigurationSource>,
}

impl DeferredLuaRuntimeConfiguration {
    fn new(source: Arc<DeferredConfigurationSource>) -> Self {
        Self { source }
    }

    fn resolve(&self) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
        Ok(TrustedLua54RuntimeConfiguration::production())
    }
}

pub(crate) struct SelectedLuaConfiguration {
    script_path: PathBuf,
    runtime: TrustedLua54RuntimeConfiguration,
}

impl SelectedLuaConfiguration {
    fn new(script_path: PathBuf, runtime: TrustedLua54RuntimeConfiguration) -> Self {
        Self {
            script_path,
            runtime,
        }
    }

    pub(crate) fn script_path(&self) -> &Path {
        &self.script_path
    }

    pub(crate) const fn runtime(&self) -> TrustedLua54RuntimeConfiguration {
        self.runtime
    }
}

#[derive(Clone)]
pub(crate) struct SelectedLlmExecutorConfiguration {
    runtime: OpenAiExecutorConfiguration,
    additional_pem_files: Vec<PathBuf>,
}

impl SelectedLlmExecutorConfiguration {
    pub(crate) fn additional_pem_files(&self) -> &[PathBuf] {
        &self.additional_pem_files
    }

    pub(crate) fn with_pem_roots(&self, roots: Vec<Vec<u8>>) -> OpenAiExecutorConfiguration {
        self.runtime.clone().with_additional_pem_roots(roots)
    }
}

pub(crate) struct SelectedRulesConfiguration {
    rules_path: PathBuf,
}

impl SelectedRulesConfiguration {
    pub(crate) fn rules_path(&self) -> &Path {
        &self.rules_path
    }
}

pub(crate) struct ExtractConfiguration {
    document: RpgMakerDocumentReadingConfig,
    builtin: bool,
    rules: Option<SelectedRulesConfiguration>,
}

impl ExtractConfiguration {
    fn build(select_builtin: bool, rules_path: Option<PathBuf>) -> Self {
        let rules = rules_path.map(|rules_path| SelectedRulesConfiguration { rules_path });
        Self {
            document: build_document_configuration(),
            builtin: select_builtin,
            rules,
        }
    }

    pub(crate) const fn document(&self) -> RpgMakerDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn builtin(&self) -> bool {
        self.builtin
    }

    pub(crate) const fn rules(&self) -> Option<&SelectedRulesConfiguration> {
        self.rules.as_ref()
    }
}

pub(crate) struct TranslateConfiguration {
    prompt_root: PathBuf,
    prompt_locale: PromptLocaleSelection,
    thinking_output: bool,
    language_modules: LanguageModuleCatalog,
    profile: TranslationProfileConfiguration,
    client: Arc<OpenAiChatCompletionClient>,
    llm: SelectedLlmExecutorConfiguration,
}

struct PendingTranslateConfiguration {
    prompt_root: PathBuf,
    prompt_locale: PromptLocaleSelection,
    thinking_output: bool,
    language_modules: LanguageModuleCatalog,
}

/// Prompt 资源语言由显式配置决定，或复用组合根已经解析的 UI locale。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptLocaleSelection {
    Auto,
    Explicit(UiLocale),
}

impl PromptLocaleSelection {
    pub(crate) const fn resolve(self, effective_ui_locale: UiLocale) -> UiLocale {
        match self {
            Self::Auto => effective_ui_locale,
            Self::Explicit(locale) => locale,
        }
    }
}

impl PendingTranslateConfiguration {
    fn build(
        configuration_directory: &Path,
        raw_prompts: RawPromptsConfiguration,
        raw_languages: Vec<RawLanguageConfiguration>,
        _raw: RawTranslateRpgMakerSelection,
    ) -> Result<Self, ConfigurationValueError> {
        let prompt_locale = if raw_prompts.locale == "auto" {
            PromptLocaleSelection::Auto
        } else {
            PromptLocaleSelection::Explicit(
                UiLocale::match_automatic(&raw_prompts.locale).ok_or_else(|| {
                    invalid(
                        "prompts.locale",
                        ConfigurationValueRule::UnsupportedPromptLocale,
                    )
                })?,
            )
        };
        Ok(Self {
            prompt_root: checked_path("prompts.root", configuration_directory, raw_prompts.root)?,
            prompt_locale,
            thinking_output: raw_prompts.thinking_output,
            language_modules: build_language_modules(raw_languages)?,
        })
    }

    fn resolve(
        self,
        source: &DeferredConfigurationSource,
        profile_id: &str,
    ) -> Result<TranslateConfiguration, ConfigurationLoadError> {
        let selected_profile =
            parse_selected_translation_profile(source.source(), source.path(), profile_id)?;
        let llm_client_id = selected_profile.llm_client.clone();
        let raw_client = parse_selected_llm_client(source.source(), source.path(), &llm_client_id)?;
        let built_client = build_llm_client(
            format!("llm.clients.{llm_client_id}").as_str(),
            source.path().parent().expect("配置文件必须拥有父目录"),
            raw_client,
        )
        .map_err(ConfigurationLoadError::InvalidValue)
        .map_err(|error| error.with_configuration_path(source.path()))?;
        let profile = build_selected_translation_profile(
            "rpg_maker.translation_profiles",
            selected_profile,
            built_client.request,
        )
        .map_err(ConfigurationLoadError::InvalidValue)
        .map_err(|error| error.with_configuration_path(source.path()))?;
        Ok(TranslateConfiguration {
            prompt_root: self.prompt_root,
            prompt_locale: self.prompt_locale,
            thinking_output: self.thinking_output,
            language_modules: self.language_modules,
            profile,
            client: Arc::new(built_client.client),
            llm: built_client.executor,
        })
    }
}

impl TranslateConfiguration {
    pub(crate) const fn profile(&self) -> &TranslationProfileConfiguration {
        &self.profile
    }

    pub(crate) fn prompt_root(&self) -> &Path {
        &self.prompt_root
    }

    pub(crate) const fn prompt_locale(&self) -> PromptLocaleSelection {
        self.prompt_locale
    }

    pub(crate) const fn thinking_output(&self) -> bool {
        self.thinking_output
    }

    pub(crate) const fn language_modules(&self) -> &LanguageModuleCatalog {
        &self.language_modules
    }

    pub(crate) const fn client(&self) -> &Arc<OpenAiChatCompletionClient> {
        &self.client
    }

    pub(crate) const fn llm(&self) -> &SelectedLlmExecutorConfiguration {
        &self.llm
    }
}

pub(crate) struct WriteBackConfiguration {
    document: RpgMakerDocumentReadingConfig,
}

impl WriteBackConfiguration {
    fn build() -> Self {
        Self {
            document: build_document_configuration(),
        }
    }

    pub(crate) const fn document(&self) -> RpgMakerDocumentReadingConfig {
        self.document
    }
}

fn build_document_configuration() -> RpgMakerDocumentReadingConfig {
    RpgMakerDocumentReadingConfig::new(NonZeroUsize::new(8).expect("产品并发值必须非零"))
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationProfileConfiguration {
    id: String,
    target_task_user_message_characters: NonZeroUsize,
    request: RpgMakerTranslationRequestConfiguration,
}

impl TranslationProfileConfiguration {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn target_task_user_message_characters(&self) -> NonZeroUsize {
        self.target_task_user_message_characters
    }

    pub(crate) const fn request(&self) -> &RpgMakerTranslationRequestConfiguration {
        &self.request
    }
}

fn build_language_modules(
    raw_languages: Vec<RawLanguageConfiguration>,
) -> Result<LanguageModuleCatalog, ConfigurationValueError> {
    let mut bindings = Vec::<(LanguageId, Arc<dyn LanguageModule>)>::new();
    let mut language_ids = BTreeSet::new();
    for (index, raw) in raw_languages.into_iter().enumerate() {
        let field = format!("languages[{index}]");
        let (id, module): (String, Arc<dyn LanguageModule>) = match raw {
            RawLanguageConfiguration::Japanese {
                id,
                minimum_kana_characters,
                allowed_terms,
                quote_repair_pairs,
            } => {
                let residual = JapaneseResidualPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_kana_characters").as_str(),
                        minimum_kana_characters,
                    )?,
                    allowed_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                let quote_repair = (!quote_repair_pairs.is_empty())
                    .then(|| {
                        JapaneseQuoteRepairPolicy::new(
                            quote_repair_pairs
                                .into_iter()
                                .map(|[opening, closing]| QuotePair::new(opening, closing))
                                .collect(),
                        )
                    })
                    .transpose()
                    .map_err(|source| invalid(field.as_str(), quote_repair_rule(&source)))?;
                (
                    id,
                    Arc::new(JapaneseLanguageModule::new(residual, quote_repair)),
                )
            }
            RawLanguageConfiguration::English {
                id,
                minimum_word_count,
                minimum_letter_count,
                ignored_terms,
                minimum_copied_word_count,
                minimum_copied_letter_count,
                allowed_terms,
            } => {
                let detection = EnglishTranslationDetectionPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_word_count").as_str(),
                        minimum_word_count,
                    )?,
                    non_zero_usize(
                        format!("{field}.minimum_letter_count").as_str(),
                        minimum_letter_count,
                    )?,
                    ignored_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                let residual = EnglishResidualPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_copied_word_count").as_str(),
                        minimum_copied_word_count,
                    )?,
                    non_zero_usize(
                        format!("{field}.minimum_copied_letter_count").as_str(),
                        minimum_copied_letter_count,
                    )?,
                    allowed_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                (
                    id,
                    Arc::new(EnglishLanguageModule::new(detection, residual)),
                )
            }
        };
        let id = LanguageId::parse(&id)
            .map_err(|source| invalid(format!("{field}.id").as_str(), language_id_rule(&source)))?;
        if !language_ids.insert(id.as_str().to_owned()) {
            return Err(invalid(
                field.as_str(),
                ConfigurationValueRule::LanguageIdDuplicate,
            ));
        }
        bindings.push((id, module));
    }

    let catalog = LanguageModuleCatalog::new(bindings)
        .map_err(|source| invalid("languages", language_catalog_rule(&source)))?;
    Ok(catalog)
}

const fn language_policy_rule(source: &LanguagePolicyConfigurationError) -> ConfigurationValueRule {
    match source {
        LanguagePolicyConfigurationError::BlankTerm => {
            ConfigurationValueRule::LanguagePolicyTermBlank
        }
        LanguagePolicyConfigurationError::SurroundingWhitespace { .. } => {
            ConfigurationValueRule::LanguagePolicyTermSurroundingWhitespace
        }
        LanguagePolicyConfigurationError::DuplicateTerm { .. } => {
            ConfigurationValueRule::LanguagePolicyTermDuplicate
        }
    }
}

const fn quote_repair_rule(source: &JapaneseQuoteRepairPolicyError) -> ConfigurationValueRule {
    match source {
        JapaneseQuoteRepairPolicyError::EmptyCandidatePairs => {
            ConfigurationValueRule::QuoteRepairCandidatesEmpty
        }
        JapaneseQuoteRepairPolicyError::InvalidDelimiterCharacter { .. } => {
            ConfigurationValueRule::QuoteRepairDelimiterInvalid
        }
        JapaneseQuoteRepairPolicyError::DuplicatePair { .. } => {
            ConfigurationValueRule::QuoteRepairPairDuplicate
        }
        JapaneseQuoteRepairPolicyError::AmbiguousCharacter { .. } => {
            ConfigurationValueRule::QuoteRepairDelimiterAmbiguous
        }
    }
}

const fn language_id_rule(source: &LanguageIdError) -> ConfigurationValueRule {
    match source {
        LanguageIdError::Blank => ConfigurationValueRule::LanguageIdBlank,
        LanguageIdError::SurroundingWhitespace { .. } => {
            ConfigurationValueRule::LanguageIdSurroundingWhitespace
        }
        LanguageIdError::Underscore { .. } => ConfigurationValueRule::LanguageIdUsesUnderscore,
        LanguageIdError::InvalidSyntax { .. } => ConfigurationValueRule::LanguageIdInvalidSyntax,
        LanguageIdError::InvalidRegistryTag { .. } => {
            ConfigurationValueRule::LanguageIdInvalidRegistryTag
        }
        LanguageIdError::CanonicalizationFailed { .. } => {
            ConfigurationValueRule::LanguageIdCanonicalizationFailed
        }
        LanguageIdError::UndefinedPrimaryLanguage { .. } => {
            ConfigurationValueRule::LanguageIdUndefinedPrimaryLanguage
        }
    }
}

const fn language_catalog_rule(source: &LanguageModuleCatalogBuildError) -> ConfigurationValueRule {
    match source {
        LanguageModuleCatalogBuildError::MissingLanguageModule => {
            ConfigurationValueRule::LanguageCatalogEmpty
        }
        LanguageModuleCatalogBuildError::DuplicateLanguageId { .. } => {
            ConfigurationValueRule::LanguageIdDuplicate
        }
    }
}

fn build_selected_translation_profile(
    field: &str,
    raw: RawSelectedTranslationProfileConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
) -> Result<TranslationProfileConfiguration, ConfigurationValueError> {
    validate_exact_identifier(format!("{field}.id").as_str(), &raw.id)?;
    validate_exact_identifier(format!("{field}.llm_client").as_str(), &raw.llm_client)?;
    Ok(TranslationProfileConfiguration {
        id: raw.id,
        target_task_user_message_characters: non_zero_usize(
            format!("{field}.target_task_user_message_characters").as_str(),
            raw.target_task_user_message_characters,
        )?,
        request,
    })
}

struct BuiltLlmClient {
    executor: SelectedLlmExecutorConfiguration,
    client: OpenAiChatCompletionClient,
    request: RpgMakerTranslationRequestConfiguration,
}

fn build_llm_client(
    field: &str,
    configuration_directory: &Path,
    raw: RawLlmClientConfiguration,
) -> Result<BuiltLlmClient, ConfigurationValueError> {
    let url = Url::parse(&raw.url).map_err(|_| {
        invalid(
            format!("{field}.url").as_str(),
            ConfigurationValueRule::UrlInvalid,
        )
    })?;
    validate_llm_url(format!("{field}.url").as_str(), &url)?;
    validate_exact_identifier(format!("{field}.model").as_str(), &raw.model)?;

    let exposed_api_key = raw.api_key.expose_secret();
    if exposed_api_key.trim().is_empty() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeyBlank,
        ));
    }
    if exposed_api_key.trim() != exposed_api_key {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeySurroundingWhitespace,
        ));
    }
    if reqwest::header::HeaderValue::from_bytes(exposed_api_key.as_bytes()).is_err() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeyInvalidHeader,
        ));
    }

    // 任意精度数字会在 Serde 访问器内使用一个内部 map 信封传递原始十进制
    // 文本。第一遍自定义访问器只负责递归拒绝重复键；第二遍由
    // `serde_json::Value` 自身还原真正的 Number，避免把内部信封写入请求正文。
    serde_json::from_str::<StrictJsonValue>(&raw.parameters).map_err(|error| {
        invalid(
            format!("{field}.parameters").as_str(),
            ConfigurationValueRule::StrictJsonInvalid {
                line: u64::try_from(error.line()).unwrap_or(u64::MAX),
                column: u64::try_from(error.column()).unwrap_or(u64::MAX),
            },
        )
    })?;
    let parameter_value = serde_json::from_str::<JsonValue>(&raw.parameters)
        .expect("已通过同一 serde_json 语法边界的源文必须可重建为 Value");
    let JsonValue::Object(parameters) = parameter_value else {
        return Err(invalid(
            format!("{field}.parameters").as_str(),
            ConfigurationValueRule::JsonObjectRequired,
        ));
    };
    for reserved in RESERVED_REQUEST_BODY_FIELDS {
        if parameters.contains_key(reserved) {
            return Err(invalid(
                format!("{field}.parameters.{reserved}").as_str(),
                ConfigurationValueRule::ReservedRequestField,
            ));
        }
    }

    let proxy = match raw.proxy {
        RawProxyConfiguration::Disabled(false) => LlmProxyConfiguration::Disabled,
        RawProxyConfiguration::Disabled(true) => {
            return Err(invalid(
                format!("{field}.proxy").as_str(),
                ConfigurationValueRule::ProxyMustBeFalseOrUrl,
            ));
        }
        RawProxyConfiguration::Url(value) => {
            let url = Url::parse(&value).map_err(|_| {
                invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlInvalid,
                )
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlSchemeUnsupported,
                ));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlCredentialsForbidden,
                ));
            }
            LlmProxyConfiguration::Explicit(url)
        }
    };
    let mut additional_pem_files = Vec::with_capacity(raw.additional_pem_files.len());
    let mut seen_pem_files = BTreeSet::new();
    for (index, path) in raw.additional_pem_files.into_iter().enumerate() {
        let pem_field = format!("{field}.additional_pem_files[{index}]");
        let path = checked_path(&pem_field, configuration_directory, path)?;
        if !seen_pem_files.insert(path.clone()) {
            return Err(invalid(
                &pem_field,
                ConfigurationValueRule::PemPathDuplicate,
            ));
        }
        additional_pem_files.push(path);
    }
    let max_concurrent_requests = non_zero_usize(
        format!("{field}.max_concurrent_requests").as_str(),
        raw.max_concurrent_requests,
    )?;
    if max_concurrent_requests.get() > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(invalid(
            format!("{field}.max_concurrent_requests").as_str(),
            ConfigurationValueRule::RuntimeMaximumExceeded {
                actual: u64::try_from(max_concurrent_requests.get()).unwrap_or(u64::MAX),
                maximum: u64::try_from(tokio::sync::Semaphore::MAX_PERMITS).unwrap_or(u64::MAX),
            },
        ));
    }
    let connect_timeout = positive_duration(
        format!("{field}.connect_timeout_ms").as_str(),
        raw.connect_timeout_ms,
    )?;
    let read_timeout = positive_duration(
        format!("{field}.read_timeout_ms").as_str(),
        raw.read_timeout_ms,
    )?;
    let request_timeout = positive_duration(
        format!("{field}.request_timeout_ms").as_str(),
        raw.request_timeout_ms,
    )?;
    let rate_limit = raw
        .rate_limit
        .map(|rate| {
            Ok((
                non_zero_u32(
                    format!("{field}.rate_limit.requests_per_minute").as_str(),
                    rate.requests_per_minute,
                )?,
                non_zero_u32(format!("{field}.rate_limit.burst").as_str(), rate.burst)?,
            ))
        })
        .transpose()?;
    let request = RpgMakerTranslationRequestConfiguration::new(
        raw.retry_delays_ms
            .into_iter()
            .map(Duration::from_millis)
            .collect(),
        Duration::from_millis(raw.max_retry_after_ms),
    );
    let client = OpenAiChatCompletionClient::new(
        url,
        raw.api_key,
        raw.model,
        max_concurrent_requests,
        request_timeout,
        rate_limit,
        parameters,
    );
    Ok(BuiltLlmClient {
        executor: SelectedLlmExecutorConfiguration {
            runtime: OpenAiExecutorConfiguration::new(
                max_concurrent_requests,
                connect_timeout,
                read_timeout,
                proxy,
            ),
            additional_pem_files,
        },
        client,
        request,
    })
}

fn validate_llm_url(field: &str, url: &Url) -> Result<(), ConfigurationValueError> {
    if url.username() != "" || url.password().is_some() {
        return Err(invalid(
            field,
            ConfigurationValueRule::UrlCredentialsForbidden,
        ));
    }
    if url.fragment().is_some() {
        return Err(invalid(field, ConfigurationValueRule::UrlFragmentForbidden));
    }
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => Err(invalid(field, ConfigurationValueRule::UrlSchemeUnsupported)),
    }
}

fn validate_exact_identifier(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    validate_non_blank(field, value)?;
    if value.trim() != value {
        return Err(invalid(
            field,
            ConfigurationValueRule::ValueSurroundingWhitespace,
        ));
    }
    Ok(())
}

fn validate_non_blank(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    if value.trim().is_empty() {
        Err(invalid(field, ConfigurationValueRule::ValueBlank))
    } else {
        Ok(())
    }
}

fn checked_path(
    field: &str,
    configuration_directory: &Path,
    value: PathBuf,
) -> Result<PathBuf, ConfigurationValueError> {
    if value.as_os_str().is_empty() {
        return Err(invalid(field, ConfigurationValueRule::PathBlank));
    }
    Ok(resolve_path(configuration_directory, &value))
}

fn resolve_path(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

fn source_location(source: &str, byte_offset: usize) -> SourceLocation {
    let prefix = source.get(..byte_offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, current_line)| current_line)
        .chars()
        .count()
        + 1;
    SourceLocation { line, column }
}

struct StrictJsonValue(JsonValue);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor).map(Self)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("任意合法 JSON 值")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        JsonNumber::deserialize(serde::de::value::I128Deserializer::new(value))
            .map(JsonValue::Number)
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        JsonNumber::deserialize(serde::de::value::U128Deserializer::new(value))
            .map(JsonValue::Number)
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsonNumber::from_f64(value).map_or(JsonValue::Null, JsonValue::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictJsonValue::deserialize(deserializer).map(|value| value.0)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(JsonValue::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = JsonMap::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("JSON 对象包含重复字段"));
            }
            let value = object.next_value::<StrictJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(JsonValue::Object(values))
    }
}

fn deserialize_api_key<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(SecretString::from)
}

fn non_zero_usize(field: &str, value: u64) -> Result<NonZeroUsize, ConfigurationValueError> {
    let value = usize_value(field, value)?;
    NonZeroUsize::new(value).ok_or_else(|| {
        invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        )
    })
}

fn usize_value(field: &str, value: u64) -> Result<usize, ConfigurationValueError> {
    usize::try_from(value).map_err(|_| {
        invalid(
            field,
            ConfigurationValueRule::UsizeRangeExceeded { actual: value },
        )
    })
}

fn non_zero_u32(field: &str, value: u64) -> Result<NonZeroU32, ConfigurationValueError> {
    let value = u32::try_from(value).map_err(|_| {
        invalid(
            field,
            ConfigurationValueRule::U32RangeExceeded { actual: value },
        )
    })?;
    NonZeroU32::new(value).ok_or_else(|| {
        invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        )
    })
}

fn positive_duration(field: &str, milliseconds: u64) -> Result<Duration, ConfigurationValueError> {
    if milliseconds == 0 {
        return Err(invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

pub(super) fn invalid(field: &str, rule: ConfigurationValueRule) -> ConfigurationValueError {
    ConfigurationValueError {
        field: field.to_owned(),
        rule,
    }
}

#[derive(Debug)]
pub(crate) enum ConfigurationPathError {
    CurrentDirectoryNotAbsolute(PathBuf),
    EmptyExplicitPath,
}

impl fmt::Display for ConfigurationPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectoryNotAbsolute(path) => {
                write!(formatter, "当前工作目录不是绝对路径：{}", path.display())
            }
            Self::EmptyExplicitPath => formatter.write_str("--config 路径不能为空"),
        }
    }
}

impl Error for ConfigurationPathError {}

#[derive(Debug)]
pub(crate) enum ConfigurationLoadError {
    Open {
        path: PathBuf,
        source: io::Error,
    },
    NotAFile {
        path: PathBuf,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidToml {
        path: PathBuf,
        location: Option<SourceLocation>,
        resource: String,
        reason: &'static str,
    },
    InvalidValue(ConfigurationValueError),
    InvalidValueAtPath {
        path: PathBuf,
        source: ConfigurationValueError,
    },
    TranslationProfileNotFound {
        path: PathBuf,
        profile_id: String,
    },
    ProfileSelectionConflict {
        path: PathBuf,
        explicit_profile: String,
        requested_profile: String,
    },
}

impl ConfigurationLoadError {
    fn with_configuration_path(self, path: &Path) -> Self {
        match self {
            Self::InvalidValue(source) => Self::InvalidValueAtPath {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        }
    }
}

impl fmt::Display for ConfigurationLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(formatter, "无法打开配置文件 {}：{source}", path.display())
            }
            Self::NotAFile { path } => {
                write!(formatter, "配置路径不是普通文件：{}", path.display())
            }
            Self::Read { path, source } => {
                write!(formatter, "无法读取配置文件 {}：{source}", path.display())
            }
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => {
                write!(
                    formatter,
                    "配置文件 {} 不是有效 UTF-8（有效前缀为 {valid_up_to} 字节，非法序列长度{}）",
                    path.display(),
                    error_len.map_or_else(|| "未知".to_owned(), |length| length.to_string())
                )
            }
            Self::InvalidToml {
                path,
                location,
                resource,
                reason,
            } => {
                write!(formatter, "{}", path.display())?;
                if let Some(location) = location {
                    write!(formatter, ":{}:{}", location.line, location.column)?;
                }
                write!(formatter, "：{resource}：{reason}")
            }
            Self::InvalidValue(source) => write!(formatter, "配置值无效：{source}"),
            Self::InvalidValueAtPath { path, source } => {
                write!(
                    formatter,
                    "配置文件 {}：配置值无效：{source}",
                    path.display()
                )
            }
            Self::TranslationProfileNotFound { path, profile_id } => write!(
                formatter,
                "配置文件 {}：rpg_maker.translation_profiles 中不存在 ID 为 {profile_id} 的 Profile",
                path.display()
            ),
            Self::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => write!(
                formatter,
                "配置文件 {}：命令行已显式选择 Profile {explicit_profile}，不能再改用 {requested_profile}",
                path.display()
            ),
        }
    }
}

impl Error for ConfigurationLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Read { source, .. } => Some(source),
            Self::InvalidValue(source) | Self::InvalidValueAtPath { source, .. } => Some(source),
            Self::NotAFile { .. }
            | Self::InvalidUtf8 { .. }
            | Self::InvalidToml { .. }
            | Self::TranslationProfileNotFound { .. }
            | Self::ProfileSelectionConflict { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceLocation {
    line: usize,
    column: usize,
}

impl SourceLocation {
    #[cfg(test)]
    pub(super) const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    pub(crate) const fn line(self) -> usize {
        self.line
    }

    pub(crate) const fn column(self) -> usize {
        self.column
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationValueError {
    field: String,
    rule: ConfigurationValueRule,
}

impl ConfigurationValueError {
    /// 返回配置契约中的稳定字段身份。
    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    /// 返回生产校验器在仍持有具体规则时保存的闭集安全原因。
    pub(crate) const fn reason(&self) -> &ConfigurationValueRule {
        &self.rule
    }
}

impl fmt::Display for ConfigurationValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.field, self.rule.render())
    }
}

impl Error for ConfigurationValueError {}

/// 先验证整份配置的字段集合，再按命令选择性解析实际值。
///
/// 这里的叶子统一使用 `IgnoredAny`：未知字段在任何分区都会失败，但未被本次命令
/// 选择的密钥、Prompt 和业务值不会被物化，也不会在这里触发类型或语义校验。
fn validate_configuration_field_names(
    source: &str,
    path: &Path,
) -> Result<(), ConfigurationLoadError> {
    let _: RawConfigurationFieldNames = parse_selected(source, path)?;
    Ok(())
}

fn parse_selected<T>(source: &str, path: &Path) -> Result<T, ConfigurationLoadError>
where
    T: serde::de::DeserializeOwned,
{
    toml::from_str(source).map_err(|error| invalid_toml(path, source, &error))
}

fn invalid_toml(path: &Path, source: &str, error: &toml::de::Error) -> ConfigurationLoadError {
    let span = error.span();
    let location = span
        .as_ref()
        .map(|span| source_location(source, span.start));
    let (resource, reason) = safe_toml_diagnostic(source, span.as_ref(), error.message());
    ConfigurationLoadError::InvalidToml {
        path: path.to_path_buf(),
        location,
        resource,
        reason,
    }
}

/// 从 TOML/Serde 错误中只提取字段身份和失败类别。
///
/// 不保留原始错误文本，因为 `invalid type` 等 Serde 诊断可能嵌入实际字符串值。
/// 资源名只由 TOML 表头、赋值左侧或 Serde 报告的字段名组成。
fn safe_toml_diagnostic(
    source: &str,
    span: Option<&std::ops::Range<usize>>,
    message: &str,
) -> (String, &'static str) {
    let (reported_field, reason) = if message.starts_with("missing field ") {
        (
            backticked_field_after(message, "missing field "),
            "缺少必填字段",
        )
    } else if message.starts_with("unknown field ") {
        (
            backticked_field_after(message, "unknown field "),
            "当前配置契约不接受该字段",
        )
    } else if message.starts_with("duplicate field ") {
        (
            backticked_field_after(message, "duplicate field "),
            "字段重复",
        )
    } else if message.starts_with("invalid type:") {
        (None, "字段类型不符合当前配置契约")
    } else if message.starts_with("invalid value:") || message.starts_with("unknown variant ") {
        (None, "字段值不符合当前配置契约")
    } else if message.contains("duplicate key") {
        (None, "TOML 字段或表重复")
    } else {
        (None, "TOML 语法无效")
    };

    let offset = span.map_or(source.len(), |span| span.start.min(source.len()));
    let assignment_field = assignment_field_at(source, offset);
    let field = reported_field.or(assignment_field.as_deref());
    let table = table_path_at(source, offset);
    let resource = match (table.as_deref(), field) {
        (Some(table), Some(field)) if table == field || table.ends_with(&format!(".{field}")) => {
            table.to_owned()
        }
        (Some(table), Some(field)) => format!("{table}.{field}"),
        (None, Some(field)) => field.to_owned(),
        (Some(table), None) => table.to_owned(),
        (None, None) => "TOML 文档".to_owned(),
    };
    (resource, reason)
}

fn backticked_field_after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    let remainder = message.strip_prefix(prefix)?;
    let field = remainder.strip_prefix('`')?.split_once('`')?.0;
    safe_toml_key_path(field).then_some(field)
}

fn assignment_field_at(source: &str, offset: usize) -> Option<String> {
    let offset = offset.min(source.len());
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    let line = source.get(line_start..line_end)?.trim();
    let field = line.split_once('=')?.0.trim();
    safe_toml_key_path(field).then(|| field.to_owned())
}

fn table_path_at(source: &str, offset: usize) -> Option<String> {
    let offset = offset.min(source.len());
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    source[..line_end].lines().rev().find_map(|line| {
        let line = line.trim();
        let table = line
            .strip_prefix("[[")
            .and_then(|value| value.strip_suffix("]]"))
            .or_else(|| {
                line.strip_prefix('[')
                    .and_then(|value| value.strip_suffix(']'))
            })?
            .trim();
        safe_toml_key_path(table).then(|| table.to_owned())
    })
}

fn safe_toml_key_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 128
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn parse_selected_translation_profile(
    source: &str,
    path: &Path,
    requested_id: &str,
) -> Result<RawSelectedTranslationProfileConfiguration, ConfigurationLoadError> {
    let index_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, &error))?;
    let selection = TranslationProfileIndexTopSeed { requested_id }
        .deserialize(index_deserializer)
        .map_err(|error| invalid_toml(path, source, &error))?
        .unwrap_or_default();
    if selection.duplicate {
        return Err(ConfigurationLoadError::InvalidValue(invalid(
            "rpg_maker.translation_profiles",
            ConfigurationValueRule::DuplicateProfileId,
        )));
    }
    let selected_index = selection.selected_index.ok_or_else(|| {
        ConfigurationLoadError::TranslationProfileNotFound {
            path: path.to_path_buf(),
            profile_id: requested_id.to_owned(),
        }
    })?;

    let profile_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, &error))?;
    SelectedTranslationProfileTopSeed { selected_index }
        .deserialize(profile_deserializer)
        .map_err(|error| invalid_toml(path, source, &error))?
        .ok_or_else(|| {
            ConfigurationLoadError::InvalidValue(invalid(
                "rpg_maker.translation_profiles",
                ConfigurationValueRule::SelectedProfileInvalid,
            ))
        })
}

#[derive(Default)]
struct TranslationProfileIndexSelection {
    selected_index: Option<usize>,
    duplicate: bool,
}

struct TranslationProfileIndexTopSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexTopSeed<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIndexTopVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexTopVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexTopVisitor<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_rpg_maker = false;
        let mut selection = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "rpg_maker" {
                if seen_rpg_maker {
                    return Err(de::Error::duplicate_field("rpg_maker"));
                }
                seen_rpg_maker = true;
                selection = map.next_value_seed(TranslationProfileIndexRpgMakerSeed {
                    requested_id: self.requested_id,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selection)
    }
}

struct TranslationProfileIndexRpgMakerSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexRpgMakerSeed<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIndexRpgMakerVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexRpgMakerVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexRpgMakerVisitor<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_profiles = false;
        let mut selection = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "translation_profiles" {
                if seen_profiles {
                    return Err(de::Error::duplicate_field("translation_profiles"));
                }
                seen_profiles = true;
                selection = Some(map.next_value_seed(TranslationProfileIndexSequenceSeed {
                    requested_id: self.requested_id,
                })?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selection)
    }
}

struct TranslationProfileIndexSequenceSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexSequenceSeed<'_> {
    type Value = TranslationProfileIndexSelection;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(TranslationProfileIndexSequenceVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexSequenceVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexSequenceVisitor<'_> {
    type Value = TranslationProfileIndexSelection;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker translation profile 数组")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut selection = TranslationProfileIndexSelection::default();
        let mut index = 0usize;
        while let Some(id) = sequence.next_element_seed(TranslationProfileIdSeed)? {
            if id.as_deref() == Some(self.requested_id)
                && selection.selected_index.replace(index).is_some()
            {
                selection.duplicate = true;
            }
            index += 1;
        }
        Ok(selection)
    }
}

struct TranslationProfileIdSeed;

impl<'de> DeserializeSeed<'de> for TranslationProfileIdSeed {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIdVisitor)
    }
}

struct TranslationProfileIdVisitor;

impl<'de> Visitor<'de> for TranslationProfileIdVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("只读取 id 的 RPG Maker translation profile")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_id = false;
        let mut id = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "id" {
                if seen_id {
                    return Err(de::Error::duplicate_field("id"));
                }
                seen_id = true;
                id = map.next_value_seed(OptionalStringSeed)?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(id)
    }
}

struct OptionalStringSeed;

impl<'de> DeserializeSeed<'de> for OptionalStringSeed {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OptionalStringVisitor)
    }
}

struct OptionalStringVisitor;

impl<'de> Visitor<'de> for OptionalStringVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("字符串或未选择的任意 TOML 值")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Some(value))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

struct SelectedTranslationProfileTopSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileTopSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedTranslationProfileTopVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileTopVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileTopVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_rpg_maker = false;
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "rpg_maker" {
                if seen_rpg_maker {
                    return Err(de::Error::duplicate_field("rpg_maker"));
                }
                seen_rpg_maker = true;
                selected = map.next_value_seed(SelectedTranslationProfileRpgMakerSeed {
                    selected_index: self.selected_index,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileRpgMakerSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileRpgMakerSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedTranslationProfileRpgMakerVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileRpgMakerVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileRpgMakerVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_profiles = false;
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "translation_profiles" {
                if seen_profiles {
                    return Err(de::Error::duplicate_field("translation_profiles"));
                }
                seen_profiles = true;
                selected = map.next_value_seed(SelectedTranslationProfileSequenceSeed {
                    selected_index: self.selected_index,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileSequenceSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileSequenceSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(SelectedTranslationProfileSequenceVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileSequenceVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileSequenceVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPG Maker translation profile 数组")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut selected = None;
        let mut index = 0usize;
        while let Some(profile) =
            sequence.next_element_seed(SelectedTranslationProfileCandidateSeed {
                selected: index == self.selected_index,
            })?
        {
            if profile.is_some() {
                selected = profile;
            }
            index += 1;
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileCandidateSeed {
    selected: bool,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileCandidateSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.selected {
            RawSelectedTranslationProfileConfiguration::deserialize(deserializer).map(Some)
        } else {
            IgnoredAny::deserialize(deserializer).map(|_| None)
        }
    }
}

fn parse_selected_llm_client(
    source: &str,
    path: &Path,
    requested_id: &str,
) -> Result<RawLlmClientConfiguration, ConfigurationLoadError> {
    validate_exact_identifier("llm client id", requested_id)
        .map_err(ConfigurationLoadError::InvalidValue)?;
    let seed = SelectedLlmClientTopSeed { requested_id };
    let deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, &error))?;
    let selected = seed
        .deserialize(deserializer)
        .map_err(|error| invalid_toml(path, source, &error))?;
    selected.ok_or_else(|| {
        ConfigurationLoadError::InvalidValue(invalid(
            "llm.clients",
            ConfigurationValueRule::ReferencedClientNotFound,
        ))
    })
}

struct SelectedLlmClientTopSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientTopSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientTopVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientTopVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientTopVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "llm" {
                if selected.is_some() {
                    return Err(de::Error::duplicate_field("llm"));
                }
                selected = map.next_value_seed(SelectedLlmClientSectionSeed {
                    requested_id: self.requested_id,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedLlmClientSectionSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientSectionSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientSectionVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientSectionVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientSectionVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("只包含 clients 的 LLM 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "clients" => {
                    if selected.is_some() {
                        return Err(de::Error::duplicate_field("clients"));
                    }
                    selected = map.next_value_seed(SelectedLlmClientMapSeed {
                        requested_id: self.requested_id,
                    })?;
                }
                _ => return Err(de::Error::unknown_field(&key, &["clients"])),
            }
        }
        Ok(selected)
    }
}

struct SelectedLlmClientMapSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientMapSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientMapVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientMapVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientMapVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("按 ID 命名的 LLM 客户端表")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(id) = map.next_key::<String>()? {
            if id == self.requested_id {
                if selected.is_some() {
                    return Err(de::Error::custom("所选 LLM 客户端 ID 重复"));
                }
                selected = Some(map.next_value::<RawLlmClientConfiguration>()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigurationFieldNames {
    #[serde(default, rename = "projects")]
    _projects: Option<RawProjectsFieldNames>,
    #[serde(default, rename = "llm")]
    _llm: Option<RawLlmFieldNames>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<RawPromptsFieldNames>,
    #[serde(default, rename = "languages")]
    _languages: Option<Vec<RawLanguageFieldNames>>,
    #[serde(default, rename = "rpg_maker")]
    _rpg_maker: Option<RawRpgMakerFieldNames>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectsFieldNames {
    #[serde(default, rename = "root")]
    _root: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPromptsFieldNames {
    #[serde(default, rename = "root")]
    _root: Option<IgnoredAny>,
    #[serde(default, rename = "locale")]
    _locale: Option<IgnoredAny>,
    #[serde(default, rename = "thinking_output")]
    _thinking_output: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmFieldNames {
    #[serde(default, rename = "clients")]
    _clients: Option<BTreeMap<String, RawLlmClientFieldNames>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmClientFieldNames {
    #[serde(default, rename = "url")]
    _url: Option<IgnoredAny>,
    #[serde(default, rename = "api_key")]
    _api_key: Option<IgnoredAny>,
    #[serde(default, rename = "model")]
    _model: Option<IgnoredAny>,
    #[serde(default, rename = "max_concurrent_requests")]
    _max_concurrent_requests: Option<IgnoredAny>,
    #[serde(default, rename = "connect_timeout_ms")]
    _connect_timeout_ms: Option<IgnoredAny>,
    #[serde(default, rename = "read_timeout_ms")]
    _read_timeout_ms: Option<IgnoredAny>,
    #[serde(default, rename = "request_timeout_ms")]
    _request_timeout_ms: Option<IgnoredAny>,
    #[serde(default, rename = "proxy")]
    _proxy: Option<IgnoredAny>,
    #[serde(default, rename = "additional_pem_files")]
    _additional_pem_files: Option<IgnoredAny>,
    #[serde(default, rename = "retry_delays_ms")]
    _retry_delays_ms: Option<IgnoredAny>,
    #[serde(default, rename = "max_retry_after_ms")]
    _max_retry_after_ms: Option<IgnoredAny>,
    #[serde(default, rename = "parameters")]
    _parameters: Option<IgnoredAny>,
    #[serde(default, rename = "rate_limit")]
    _rate_limit: Option<RawLlmRateLimitFieldNames>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmRateLimitFieldNames {
    #[serde(default, rename = "requests_per_minute")]
    _requests_per_minute: Option<IgnoredAny>,
    #[serde(default, rename = "burst")]
    _burst: Option<IgnoredAny>,
}

/// 字段名层只验证所有现行语言模块允许出现的字段集合；具体 `type` 对应哪些字段，
/// 仍由 Translate 的第二遍解析负责，因此非 Translate 命令不会消费语言策略值。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguageFieldNames {
    #[serde(default, rename = "type")]
    _language_type: Option<IgnoredAny>,
    #[serde(default, rename = "id")]
    _id: Option<IgnoredAny>,
    #[serde(default, rename = "minimum_kana_characters")]
    _minimum_kana_characters: Option<IgnoredAny>,
    #[serde(default, rename = "minimum_word_count")]
    _minimum_word_count: Option<IgnoredAny>,
    #[serde(default, rename = "minimum_letter_count")]
    _minimum_letter_count: Option<IgnoredAny>,
    #[serde(default, rename = "ignored_terms")]
    _ignored_terms: Option<IgnoredAny>,
    #[serde(default, rename = "minimum_copied_word_count")]
    _minimum_copied_word_count: Option<IgnoredAny>,
    #[serde(default, rename = "minimum_copied_letter_count")]
    _minimum_copied_letter_count: Option<IgnoredAny>,
    #[serde(default, rename = "allowed_terms")]
    _allowed_terms: Option<IgnoredAny>,
    #[serde(default, rename = "quote_repair_pairs")]
    _quote_repair_pairs: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerFieldNames {
    #[serde(default, rename = "record_translation_tasks")]
    _record_translation_tasks: Option<IgnoredAny>,
    #[serde(default, rename = "translation_profiles")]
    _translation_profiles: Option<Vec<RawTranslationProfileFieldNames>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslationProfileFieldNames {
    #[serde(default, rename = "id")]
    _id: Option<IgnoredAny>,
    #[serde(default, rename = "llm_client")]
    _llm_client: Option<IgnoredAny>,
    #[serde(default, rename = "target_task_user_message_characters")]
    _target_task_user_message_characters: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommonConfiguration {
    projects: RawProjectsConfiguration,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "rpg_maker")]
    _rpg_maker: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInitSelection {
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "rpg_maker")]
    _rpg_maker: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractSelection {
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "rpg_maker")]
    _rpg_maker: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateSelection {
    prompts: RawPromptsConfiguration,
    languages: Vec<RawLanguageConfiguration>,
    rpg_maker: RawTranslateRpgMakerSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackSelection {
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "rpg_maker")]
    _rpg_maker: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateRpgMakerSelection {
    #[serde(default)]
    record_translation_tasks: bool,
    #[serde(rename = "translation_profiles")]
    _translation_profiles: IgnoredAny,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPromptsConfiguration {
    root: PathBuf,
    locale: String,
    thinking_output: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectsConfiguration {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawProxyConfiguration {
    Disabled(bool),
    Url(String),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawLanguageConfiguration {
    Japanese {
        id: String,
        minimum_kana_characters: u64,
        allowed_terms: Vec<String>,
        quote_repair_pairs: Vec<[char; 2]>,
    },
    English {
        id: String,
        minimum_word_count: u64,
        minimum_letter_count: u64,
        ignored_terms: Vec<String>,
        minimum_copied_word_count: u64,
        minimum_copied_letter_count: u64,
        allowed_terms: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelectedTranslationProfileConfiguration {
    id: String,
    llm_client: String,
    target_task_user_message_characters: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmClientConfiguration {
    url: String,
    #[serde(deserialize_with = "deserialize_api_key")]
    api_key: SecretString,
    model: String,
    max_concurrent_requests: u64,
    connect_timeout_ms: u64,
    read_timeout_ms: u64,
    request_timeout_ms: u64,
    proxy: RawProxyConfiguration,
    additional_pem_files: Vec<PathBuf>,
    retry_delays_ms: Vec<u64>,
    max_retry_after_ms: u64,
    parameters: String,
    #[serde(default)]
    rate_limit: Option<RawLlmRateLimitConfiguration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmRateLimitConfiguration {
    requests_per_minute: u64,
    burst: u64,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use secrecy::ExposeSecret;

    use super::*;
    use crate::application::arguments::{AttArguments, ProductCommand};
    use crate::llm::LlmClientSemanticIdentity;

    #[test]
    fn repository_example_is_valid_for_every_command() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", include_str!("../../config.example.toml"));

        for command in [
            init_command(),
            extract_command(false),
            translate_command(false, "primary"),
            write_back_command(false),
        ] {
            load_configuration(&path, command).expect("仓库示例必须满足每个命令的当前契约");
        }
    }

    #[test]
    fn explicit_configuration_path_uses_current_directory_as_its_base() {
        let current = absolute_test_path("cwd");
        assert_eq!(
            resolve_configuration_path(Path::new("settings/att.toml"), &current)
                .expect("显式配置路径应合法"),
            current.join("settings/att.toml")
        );
        assert_eq!(
            resolve_configuration_path(&absolute_test_path("explicit.toml"), &current)
                .expect("绝对配置路径应保持不变"),
            absolute_test_path("explicit.toml")
        );
    }

    #[test]
    fn directory_publisher_lock_root_is_namespaced_by_engine() {
        let projects_root = absolute_test_path("projects");
        let configured =
            build_directory_publisher_configuration(&projects_root, RpgMakerEngine::Mz)
                .expect("目录发布配置应合法");
        assert_eq!(
            configured.lock_directory(),
            projects_root.join(".att-locks/directory-publish/mz")
        );

        let configured =
            build_directory_publisher_configuration(&projects_root, RpgMakerEngine::Mv)
                .expect("MV 目录发布配置应合法");
        assert_eq!(
            configured.lock_directory(),
            projects_root.join(".att-locks/directory-publish/mv")
        );
    }

    #[test]
    fn init_does_not_parse_unselected_product_sections() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("api_key = \"replace-with-api-key\"", "api_key = []");
        let path = directory.write("init.toml", &source);

        let configured =
            load_configuration(&path, init_command()).expect("Init 不应解析未选择的 LLM 配置");
        assert!(matches!(configured, ConfiguredRpgMakerCommand::Init(_)));
    }

    #[test]
    fn non_translate_commands_load_their_minimal_configuration() {
        let directory = TestDirectory::new();
        let path = directory.write("minimal.toml", minimal_init_configuration());

        for command in [
            init_command(),
            extract_command(false),
            write_back_command(false),
        ] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不应要求无现实消费的配置存在");
        }
    }

    #[test]
    fn omitted_translate_profile_remains_deferred_until_project_state_is_known() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "deferred-profile.toml",
            include_str!("../../config.example.toml"),
        );
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command_without_profile())
                .expect("省略 Profile 应先完成与 Profile 无关的配置加载")
        else {
            panic!("应建立 Translate 配置");
        };
        assert_eq!(configured.resolved_profile_id(), None);

        let configured = (*configured)
            .resolve_profile("primary")
            .expect("项目状态中的现行 Profile 应可精确解析");
        assert_eq!(configured.resolved_profile_id(), Some("primary"));
        assert_eq!(configured.rpg_maker().profile().id(), "primary");
    }

    #[test]
    fn missing_project_state_profile_has_a_distinct_configuration_error() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "missing-profile.toml",
            include_str!("../../config.example.toml"),
        );
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command_without_profile())
                .expect("省略 Profile 应等待项目状态解析")
        else {
            panic!("应建立 Translate 配置");
        };

        let error = match (*configured).resolve_profile("removed-profile") {
            Ok(_) => panic!("已从当前配置移除的保存 Profile 必须显式失败"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::TranslationProfileNotFound {
                ref profile_id,
                ..
            } if profile_id == "removed-profile"
        ));
    }

    #[test]
    fn explicit_translate_profile_is_still_validated_during_load() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "target_task_user_message_characters = 24000",
            "target_task_user_message_characters = 0",
        );
        let path = directory.write("invalid-explicit-profile.toml", &source);

        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("显式 Profile 的无效字段必须在配置加载阶段被拒绝"),
            Err(error) => error,
        };
        let ConfigurationLoadError::InvalidValueAtPath { source: error, .. } = error else {
            panic!("user message 字符装箱目标为零时必须返回配置值错误");
        };
        assert_eq!(
            error.field(),
            "rpg_maker.translation_profiles.target_task_user_message_characters"
        );
    }

    fn configuration_with_unselected_profile_sentinel(sentinel: &str) -> String {
        format!(
            r#"{}
[llm.clients.unused]
url = []
api_key = []
model = []
max_concurrent_requests = []
connect_timeout_ms = []
read_timeout_ms = []
request_timeout_ms = []
proxy = []
additional_pem_files = []
retry_delays_ms = []
max_retry_after_ms = []
parameters = []

[[rpg_maker.translation_profiles]]
llm_client = ["{sentinel}"]
target_task_user_message_characters = {{ marker = "{sentinel}" }}
id = "unused"
"#,
            include_str!("../../config.example.toml")
        )
    }

    #[test]
    fn translate_streams_past_unselected_client_and_profile_without_materializing_values() {
        const SENTINEL: &str = "UNSELECTED_PROFILE_DATA_SENTINEL";
        let directory = TestDirectory::new();
        let source = configuration_with_unselected_profile_sentinel(SENTINEL);
        let path = directory.write("translate.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command(false, "primary"))
                .expect("无关客户端和 Profile 不应阻止本次翻译")
        else {
            panic!("应建立 Translate 配置");
        };
        configured
            .rpg_maker()
            .language_modules()
            .resolve(&LanguageId::parse("ja").expect("测试语言应合法"))
            .expect("应建立日语模块");
        configured
            .rpg_maker()
            .language_modules()
            .resolve(&LanguageId::parse("en").expect("测试语言应合法"))
            .expect("应建立英语模块");
        assert_eq!(configured.client().model(), "replace-with-model-id");
        assert_eq!(
            configured.client().api_key().expose_secret(),
            "replace-with-api-key"
        );
        let profile_debug = format!("{:?}", configured.rpg_maker().profile());
        assert!(profile_debug.contains("primary"));
        assert!(!profile_debug.contains(SENTINEL));
    }

    #[test]
    fn unselected_profile_data_never_enters_configuration_diagnostics() {
        const SENTINEL: &str = "UNSELECTED_PROFILE_DIAGNOSTIC_SENTINEL";
        let directory = TestDirectory::new();
        let source = configuration_with_unselected_profile_sentinel(SENTINEL).replacen(
            "minimum_kana_characters = 1",
            "minimum_kana_characters = 0",
            1,
        );
        let path = directory.write("invalid-language-with-unselected-profile.toml", &source);
        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("无效语言策略必须拒绝"),
            Err(error) => error,
        };

        let mut diagnostics = format!("{error:?}\n{error}");
        let mut source = error.source();
        while let Some(error) = source {
            diagnostics.push_str(format!("\n{error:?}\n{error}").as_str());
            source = error.source();
        }
        assert!(!diagnostics.contains(SENTINEL));
    }

    #[test]
    fn translate_validates_the_complete_global_language_catalog() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[[languages]]\ntype = \"english\"\nid = \"fr\"\nminimum_word_count = \"invalid\"\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("invalid-language.toml", &source);
        assert!(
            load_configuration(&path, translate_command(false, "primary")).is_err(),
            "Translate 必须在执行前验证全部语言条目"
        );
    }

    #[test]
    fn prompt_root_is_resolved_from_the_configuration_directory() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", include_str!("../../config.example.toml"));
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command(false, "primary"))
                .expect("示例 Translate 配置应合法")
        else {
            panic!("应建立 Translate 配置");
        };
        let expected = path
            .canonicalize()
            .expect("测试配置应可规范化")
            .parent()
            .expect("规范配置路径应有父目录")
            .join("prompts");
        assert_eq!(configured.rpg_maker().prompt_root(), expected);
    }

    #[test]
    fn prompt_locale_auto_and_thinking_output_are_preserved_for_the_composition_root() {
        let directory = TestDirectory::new();
        for (thinking_output, expected) in [("false", false), ("true", true)] {
            let source = replace_thinking_output(
                include_str!("../../config.example.toml"),
                format!("thinking_output = {thinking_output}").as_str(),
            );
            let path = directory.write(
                format!("prompt-auto-thinking-{thinking_output}.toml").as_str(),
                &source,
            );
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command(false, "primary"))
                    .expect("auto locale 与布尔思考输出开关应建立受信配置")
            else {
                panic!("应建立 Translate 配置");
            };

            assert_eq!(
                configured.rpg_maker().prompt_locale(),
                PromptLocaleSelection::Auto
            );
            assert_eq!(
                configured
                    .rpg_maker()
                    .prompt_locale()
                    .resolve(UiLocale::French),
                UiLocale::French,
                "auto 必须复用组合根提供的已解析 UI locale"
            );
            assert_eq!(configured.rpg_maker().thinking_output(), expected);
        }
    }

    #[test]
    fn explicit_prompt_locale_uses_the_existing_ui_locale_matching_contract() {
        let directory = TestDirectory::new();
        let mut cases = UiLocale::ALL
            .into_iter()
            .map(|locale| (locale.as_str(), locale))
            .collect::<Vec<_>>();
        cases.extend([
            ("ja-JP", UiLocale::Japanese),
            ("zh-TW", UiLocale::TraditionalChinese),
        ]);

        for (locale_input, expected) in cases {
            let source = include_str!("../../config.example.toml").replace(
                "locale = \"auto\"",
                format!("locale = \"{locale_input}\"").as_str(),
            );
            let path = directory.write(
                format!("prompt-locale-{}.toml", locale_input.replace('-', "_")).as_str(),
                &source,
            );
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command(false, "primary"))
                    .expect("受支持的 BCP 47 UI locale 应建立显式 Prompt locale")
            else {
                panic!("应建立 Translate 配置");
            };

            assert_eq!(
                configured.rpg_maker().prompt_locale(),
                PromptLocaleSelection::Explicit(expected),
                "显式 locale {locale_input} 应规范化为所选 UI locale"
            );
            assert_eq!(
                configured
                    .rpg_maker()
                    .prompt_locale()
                    .resolve(UiLocale::English),
                expected,
                "显式 locale 必须覆盖组合根提供的 UI locale"
            );
        }
    }

    #[test]
    fn prompt_locale_rejects_invalid_or_unsupported_choices_without_echoing_them() {
        const SENTINEL: &str = "UNSUPPORTED_PROMPT_LOCALE_SENTINEL";
        let directory = TestDirectory::new();
        for (name, locale) in [
            ("uppercase-auto", "AUTO"),
            ("surrounding-whitespace", " ja "),
            ("unsupported", SENTINEL),
        ] {
            let source = include_str!("../../config.example.toml").replace(
                "locale = \"auto\"",
                format!("locale = \"{locale}\"").as_str(),
            );
            let path = directory.write(format!("prompt-locale-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command(false, "primary")) {
                Ok(_) => panic!("无效或不支持的 Prompt locale 必须失败"),
                Err(error) => error,
            };
            let diagnostics = format!("{error:?}\n{error}");

            assert!(diagnostics.contains("prompts.locale"));
            assert!(diagnostics.contains("BCP 47 UI locale"));
            assert!(!diagnostics.contains(SENTINEL));
        }
    }

    #[test]
    fn commands_other_than_translate_do_not_consume_prompt_values() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("root = \"prompts\"", "root = []")
            .replace("locale = \"auto\"", "locale = []");
        let source = replace_thinking_output(&source, "thinking_output = []");
        let path = directory.write("unselected-prompts.toml", &source);

        for command in [
            init_command(),
            extract_command(false),
            write_back_command(false),
        ] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不得物化或校验 prompts 的字段值");
        }
    }

    #[test]
    fn translate_defaults_and_preserves_task_recording_selection() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let cases = [
            (
                "omitted",
                example.replace("record_translation_tasks = false", ""),
                false,
            ),
            ("false", example.to_owned(), false),
            (
                "true",
                example.replace(
                    "record_translation_tasks = false",
                    "record_translation_tasks = true",
                ),
                true,
            ),
        ];

        let mut semantic_fingerprints = Vec::new();
        for (name, source, expected) in cases {
            let path = directory.write(
                format!("record-translation-tasks-{name}.toml").as_str(),
                &source,
            );
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command(false, "primary"))
                    .expect("任务记录开关应建立受信 Translate 配置")
            else {
                panic!("应建立 Translate 配置");
            };

            assert_eq!(configured.record_translation_tasks(), expected);
            semantic_fingerprints.push(configured.client().semantic_fingerprint());
        }
        assert!(
            semantic_fingerprints
                .windows(2)
                .all(|pair| pair[0] == pair[1])
        );
    }

    #[test]
    fn only_translate_consumes_the_task_recording_value() {
        const SENTINEL: &str = "RECORD_TRANSLATION_TASKS_TYPE_SENTINEL";
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "record_translation_tasks = false",
            format!("record_translation_tasks = [\"{SENTINEL}\"]").as_str(),
        );
        let path = directory.write("record-translation-tasks-type.toml", &source);

        for command in [
            init_command(),
            extract_command(false),
            write_back_command(false),
        ] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不得物化或校验任务记录开关");
        }

        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("Translate 必须拒绝非布尔任务记录开关"),
            Err(error) => error,
        };
        let diagnostics = format!("{error:?}\n{error}");
        assert!(diagnostics.contains("rpg_maker.record_translation_tasks"));
        assert!(!diagnostics.contains(SENTINEL));
    }

    #[test]
    fn selected_profile_rejects_unknown_fields() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "target_task_user_message_characters = 24000",
            "target_task_user_message_characters = 24000\nunexpected_field = []",
        );
        let path = directory.write("unknown-profile-field.toml", &source);
        assert!(
            load_configuration(&path, translate_command(false, "primary")).is_err(),
            "所选 Profile 必须严格拒绝未知字段"
        );
    }

    #[test]
    fn translate_rejects_every_missing_consumed_configuration_field() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace("\r\n", "\n");
        let cases = [
            (
                "prompts-root",
                source.replacen("root = \"prompts\"\n", "", 1),
            ),
            (
                "prompts-locale",
                source.replacen("locale = \"auto\"\n", "", 1),
            ),
            (
                "prompts-thinking-output",
                replace_thinking_output(&source, ""),
            ),
            (
                "languages",
                remove_configuration_range(
                    &source,
                    "[[languages]]",
                    "[[rpg_maker.translation_profiles]]",
                ),
            ),
            ("profile-id", source.replacen("id = \"primary\"\n", "", 1)),
            (
                "profile-client",
                source.replacen("llm_client = \"primary\"\n", "", 1),
            ),
            (
                "profile-target-task-user-message-characters",
                source.replacen("target_task_user_message_characters = 24000\n", "", 1),
            ),
            (
                "client-retry-delays",
                source.replacen("retry_delays_ms = [500, 1500, 5000]\n", "", 1),
            ),
            (
                "client-max-retry-after",
                source.replacen("max_retry_after_ms = 30000\n", "", 1),
            ),
        ];

        for (name, source) in cases {
            let path = directory.write(format!("missing-{name}.toml").as_str(), &source);
            assert!(
                load_configuration(&path, translate_command(false, "primary")).is_err(),
                "Translate 消费的必填项 {name} 缺失时必须显式失败"
            );
        }
    }

    #[test]
    fn toml_diagnostics_identify_missing_unknown_and_mistyped_fields_without_values() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace("\r\n", "\n");
        let cases = [
            (
                "selected-profile-missing",
                source.replacen("llm_client = \"primary\"\n", "", 1),
                "rpg_maker.translation_profiles.llm_client",
                "缺少必填字段",
                None,
            ),
            (
                "missing",
                source.replacen("root = \"prompts\"\n", "", 1),
                "prompts.root",
                "缺少必填字段",
                None,
            ),
            (
                "unknown",
                source.replacen(
                    "[prompts]\n",
                    "[prompts]\nunexpected_prompt_field = \"UNKNOWN_VALUE_SENTINEL\"\n",
                    1,
                ),
                "prompts.unexpected_prompt_field",
                "当前配置契约不接受该字段",
                Some("UNKNOWN_VALUE_SENTINEL"),
            ),
            (
                "prompt-root-type",
                source.replacen(
                    "root = \"prompts\"",
                    "root = [\"PROMPT_ROOT_TYPE_SENTINEL\"]",
                    1,
                ),
                "prompts.root",
                "字段类型不符合当前配置契约",
                Some("PROMPT_ROOT_TYPE_SENTINEL"),
            ),
            (
                "prompt-locale-type",
                source.replacen(
                    "locale = \"auto\"",
                    "locale = [\"PROMPT_LOCALE_TYPE_SENTINEL\"]",
                    1,
                ),
                "prompts.locale",
                "字段类型不符合当前配置契约",
                Some("PROMPT_LOCALE_TYPE_SENTINEL"),
            ),
            (
                "prompt-thinking-output-type",
                replace_thinking_output(
                    &source,
                    "thinking_output = [\"PROMPT_THINKING_TYPE_SENTINEL\"]",
                ),
                "prompts.thinking_output",
                "字段类型不符合当前配置契约",
                Some("PROMPT_THINKING_TYPE_SENTINEL"),
            ),
            (
                "type",
                source.replacen(
                    "max_concurrent_requests = 8",
                    "max_concurrent_requests = \"TYPE_VALUE_SENTINEL\"",
                    1,
                ),
                "llm.clients.primary.max_concurrent_requests",
                "字段类型不符合当前配置契约",
                Some("TYPE_VALUE_SENTINEL"),
            ),
            (
                "api-key-type",
                source.replacen(
                    "api_key = \"replace-with-api-key\"",
                    "api_key = [\"API_KEY_TYPE_SENTINEL\"]",
                    1,
                ),
                "llm.clients.primary.api_key",
                "字段类型不符合当前配置契约",
                Some("API_KEY_TYPE_SENTINEL"),
            ),
        ];

        for (name, source, expected_resource, expected_reason, forbidden_value) in cases {
            let path = directory.write(format!("diagnostic-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command(false, "primary")) {
                Ok(_) => panic!("无效配置必须失败"),
                Err(error) => error,
            };
            let diagnostic = error.to_string();
            let canonical_path = path.canonicalize().expect("测试配置应可规范化");

            assert!(
                diagnostic.starts_with(canonical_path.display().to_string().as_str()),
                "诊断必须以配置路径开始：{diagnostic}"
            );
            assert!(diagnostic.contains(expected_resource), "{diagnostic}");
            assert!(diagnostic.contains(expected_reason), "{diagnostic}");
            if let Some(value) = forbidden_value {
                assert!(
                    !diagnostic.contains(value),
                    "诊断不得回显原始值：{diagnostic}"
                );
                assert!(
                    !format!("{error:?}").contains(value),
                    "Debug 诊断不得回显原始值"
                );
            }
        }
    }

    #[test]
    fn translate_rejects_duplicate_current_profile_and_language_ids() {
        let directory = TestDirectory::new();
        let duplicate_profile = format!(
            "{}\n[[rpg_maker.translation_profiles]]\nid = \"primary\"\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("duplicate-profile.toml", &duplicate_profile);
        assert!(load_configuration(&path, translate_command(false, "primary")).is_err());

        let duplicate_language = format!(
            "{}\n[[languages]]\ntype = \"japanese\"\nid = \"JA\"\nminimum_kana_characters = 1\nallowed_terms = []\nquote_repair_pairs = []\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("duplicate-language.toml", &duplicate_language);
        assert!(load_configuration(&path, translate_command(false, "primary")).is_err());
    }

    #[test]
    fn selected_subtrees_remain_strict() {
        let directory = TestDirectory::new();
        let invalid_client = include_str!("../../config.example.toml")
            .replace("model = \"replace-with-model-id\"", "model = []");
        let path = directory.write("client.toml", &invalid_client);
        assert!(load_configuration(&path, translate_command(false, "primary")).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected_across_selected_and_unselected_sections() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let cases = [
            ("top", format!("{example}\n[unknown]\nvalue = 1\n")),
            (
                "projects",
                example.replace("root = \"projects\"", "root = \"projects\"\nunexpected = 1"),
            ),
            (
                "prompts",
                replace_thinking_output(example, "thinking_output = false\nunexpected = 1"),
            ),
            (
                "llm",
                example.replace(
                    "[llm.clients.primary]",
                    "[llm]\nunexpected = 1\n\n[llm.clients.primary]",
                ),
            ),
            (
                "client",
                example.replace(
                    "model = \"replace-with-model-id\"",
                    "model = \"replace-with-model-id\"\nunexpected = 1",
                ),
            ),
            (
                "rate-limit",
                format!(
                    "{example}\n[llm.clients.primary.rate_limit]\nrequests_per_minute = 60\nburst = 8\nunexpected = 1\n"
                ),
            ),
            (
                "language",
                example.replacen(
                    "allowed_terms = []",
                    "allowed_terms = []\nunexpected = 1",
                    1,
                ),
            ),
            (
                "profile",
                example.replace(
                    "target_task_user_message_characters = 24000",
                    "target_task_user_message_characters = 24000\nunexpected = 1",
                ),
            ),
            (
                "rpg-maker",
                example.replace(
                    "record_translation_tasks = false",
                    "record_translation_tasks = false\nunexpected = 1",
                ),
            ),
        ];

        for (name, source) in cases {
            let path = directory.write(format!("unknown-{name}.toml").as_str(), &source);
            assert!(
                load_configuration(&path, init_command()).is_err(),
                "Init 未选择的分区也必须拒绝未知字段：{name}"
            );
        }
    }

    #[test]
    fn duplicate_toml_keys_are_rejected_even_inside_unselected_sections() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[llm.clients.unused]\napi_key = \"first\"\napi_key = \"second\"\n",
            minimal_init_configuration()
        );
        let path = directory.write("duplicate-unselected-key.toml", &source);

        assert!(
            load_configuration(&path, init_command()).is_err(),
            "完整 TOML 的重复键属于语法错误，不得因分区未选中而忽略"
        );
    }

    #[test]
    fn selected_llm_client_debug_hides_only_api_key() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml").replace("\r\n", "\n");
        const EMPTY_PARAMETERS: &str = "parameters = '''\n{}\n'''";
        const CUSTOM_PARAMETERS: &str =
            "parameters = '''\n{\"vendor_value\":\"PARAMETER_SENTINEL\"}\n'''";
        assert!(
            example.contains(EMPTY_PARAMETERS),
            "示例配置必须默认使用空 parameters，测试再显式注入普通参数"
        );
        let source = example
            .replace("replace-with-api-key", "API_KEY_SENTINEL")
            .replace(EMPTY_PARAMETERS, CUSTOM_PARAMETERS);
        assert!(source.contains("PARAMETER_SENTINEL"));
        let path = directory.write("api-key.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command(false, "primary"))
                .expect("所选客户端应合法")
        else {
            panic!("应建立 Translate 配置");
        };
        let debug = format!("{:?}", configured.client());
        assert!(!debug.contains("API_KEY_SENTINEL"));
        assert!(debug.contains("PARAMETER_SENTINEL"));
    }

    #[test]
    fn unselected_client_api_key_never_enters_configuration_diagnostics() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[llm.clients.unused]\nurl = []\napi_key = \"UNSELECTED_API_KEY_SENTINEL\"\nmodel = []\nmax_concurrent_requests = []\nconnect_timeout_ms = []\nread_timeout_ms = []\nrequest_timeout_ms = []\nproxy = []\nadditional_pem_files = []\nretry_delays_ms = []\nmax_retry_after_ms = []\nparameters = []\n",
            include_str!("../../config.example.toml")
        );
        let source = replace_thinking_output(&source, "thinking_output = []");
        let path = directory.write("unselected-api-key.toml", &source);
        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("无效 Prompt 配置必须拒绝"),
            Err(error) => error,
        };
        let mut diagnostics = format!("{error:?}\n{error}");
        let mut source = error.source();
        while let Some(error) = source {
            diagnostics.push_str(format!("\n{error:?}\n{error}").as_str());
            source = error.source();
        }
        assert!(!diagnostics.contains("UNSELECTED_API_KEY_SENTINEL"));
    }

    #[test]
    fn large_configuration_file_is_loaded_without_an_att_size_limit() {
        let directory = TestDirectory::new();
        let path = directory.path().join("large.toml");
        let mut source = minimal_init_configuration().to_owned();
        source.push_str("\n#");
        source.push_str(&"x".repeat(5 * 1024 * 1024));
        fs::write(&path, source).expect("应写入大配置");
        load_configuration(&path, init_command()).expect("配置大小不得触发 ATT 自行规定的拒绝");
    }

    fn init_command() -> MzCommand {
        parse_command(["att", "mz", "init", "--name", "demo", "--path", "game"])
    }

    fn extract_command(builtin: bool) -> MzCommand {
        if builtin {
            parse_command(["att", "mz", "extract", "--name", "demo", "--builtin"])
        } else {
            parse_command([
                "att",
                "mz",
                "extract",
                "--name",
                "demo",
                "--rules",
                "rules.toml",
            ])
        }
    }

    fn translate_command(lua: bool, profile: &str) -> MzCommand {
        if lua {
            parse_command([
                "att",
                "mz",
                "translate",
                "--name",
                "demo",
                profile,
                "--lua",
                "script.lua",
            ])
        } else {
            parse_command(["att", "mz", "translate", "--name", "demo", profile])
        }
    }

    fn translate_command_without_profile() -> MzCommand {
        parse_command(["att", "mz", "translate", "--name", "demo"])
    }

    fn write_back_command(lua: bool) -> MzCommand {
        if lua {
            parse_command([
                "att",
                "mz",
                "write-back",
                "--name",
                "demo",
                "--lua",
                "script.lua",
            ])
        } else {
            parse_command(["att", "mz", "write-back", "--name", "demo"])
        }
    }

    fn parse_command<const N: usize>(arguments: [&str; N]) -> MzCommand {
        let arguments = ["att", "--config", "config.toml"]
            .into_iter()
            .chain(arguments.into_iter().skip(1));
        let parsed = AttArguments::try_parse_from(arguments).expect("测试命令应合法");
        match parsed.product {
            ProductCommand::Mz { command } => command,
            ProductCommand::Mv { .. } => panic!("配置测试只应构造 MZ 命令"),
        }
    }

    fn absolute_test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join("att-config-tests").join(label)
    }

    fn minimal_init_configuration() -> &'static str {
        r#"
[projects]
root = "projects"
"#
    }

    fn remove_configuration_range(source: &str, start: &str, end: &str) -> String {
        let start_offset = source.find(start).expect("测试配置应包含起始标记");
        let relative_end = source[start_offset..]
            .find(end)
            .expect("测试配置应包含结束标记");
        let end_offset = start_offset + relative_end;
        format!("{}{}", &source[..start_offset], &source[end_offset..])
    }

    fn replace_thinking_output(source: &str, replacement: &str) -> String {
        for current in ["thinking_output = false", "thinking_output = true"] {
            if source.contains(current) {
                return source.replacen(current, replacement, 1);
            }
        }
        panic!("测试配置应包含 thinking_output 布尔字段");
    }

    struct TestDirectory {
        root: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "att-config-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&root).expect("应创建测试目录");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, content).expect("应写入测试配置");
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
