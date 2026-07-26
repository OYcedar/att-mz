//! 严格 TOML 配置边界。
//!
//! 原始 TOML 只在本模块存在。结构和字段类型全部通过后，本模块继续建立路径基准、
//! 语言模块、LLM Client 外部约束与 Profile 唯一性；业务和根适配器只接收受信配置。

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use toml_parser::decoder::ScalarKind;
use toml_parser::parser::{Event, EventKind, ValidateWhitespace, parse_document};
use toml_parser::{ParseError, Source, Span};
use url::Url;
use zeroize::Zeroizing;

use super::arguments::{
    ExtractArguments, InitArguments, MvCommand, MzCommand, ProductCommand, ProjectLuaArguments,
    TranslateArguments, WriteBackArguments,
};

use crate::diagnostic::{
    ConfigurationTomlFailureKind, ConfigurationTomlValueKind, ConfigurationValueRule,
};
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
    let toml_index = Arc::new(ConfigurationTomlIndex::build(source, &configuration_path)?);
    toml_index.validate_complete_field_set(source, &configuration_path)?;
    let configuration_directory = configuration_path
        .parent()
        .expect("规范绝对文件路径必须拥有父目录")
        .to_path_buf();
    let (layout, command, dialogue_rules_path) = normalize_product_command(product);
    ConfiguredRpgMakerCommand::build(
        &configuration_path,
        &configuration_directory,
        source,
        toml_index,
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
            MvCommand::Lua(arguments) => (
                RpgMakerLayout::MV,
                RpgMakerCommandArguments::Lua(arguments),
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
    Lua(ProjectLuaArguments),
}

impl From<MzCommand> for RpgMakerCommandArguments {
    fn from(command: MzCommand) -> Self {
        match command {
            MzCommand::Init(arguments) => Self::Init(arguments),
            MzCommand::Extract(arguments) => Self::Extract(arguments),
            MzCommand::Translate(arguments) => Self::Translate(arguments),
            MzCommand::WriteBack(arguments) => Self::WriteBack(arguments),
            MzCommand::Lua(arguments) => Self::Lua(arguments),
        }
    }
}

/// 五个互斥命令各自拥有且只拥有现实消费的配置。
pub(crate) enum ConfiguredRpgMakerCommand {
    Init(ConfiguredInitCommand),
    Extract(ConfiguredExtractCommand),
    Translate(Box<ConfiguredTranslateCommand>),
    WriteBack(ConfiguredWriteBackCommand),
    Lua(ConfiguredProjectLuaCommand),
}

impl ConfiguredRpgMakerCommand {
    fn build(
        configuration_path: &Path,
        configuration_directory: &Path,
        source: &str,
        toml_index: Arc<ConfigurationTomlIndex>,
        layout: RpgMakerLayout,
        command: RpgMakerCommandArguments,
        dialogue_rules_path: Option<PathBuf>,
    ) -> Result<Self, ConfigurationLoadError> {
        let raw_common: RawCommonConfiguration = parse_selected(
            source,
            configuration_path,
            toml_index.as_ref(),
            ConfigurationSelection::Common,
        )?;
        let common = CommonCommandConfiguration::build(configuration_directory, raw_common)
            .map_err(ConfigurationLoadError::InvalidValue)?;

        match command {
            RpgMakerCommandArguments::Init(arguments) => {
                let _: RawInitSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
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
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let deferred_lua =
                    DeferredLuaRuntimeConfiguration::new(Arc::clone(&deferred_source));
                let _: RawExtractSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
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
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let deferred_lua =
                    DeferredLuaRuntimeConfiguration::new(Arc::clone(&deferred_source));
                let raw: RawTranslateSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::Translate,
                )?;
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
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let deferred_lua = DeferredLuaRuntimeConfiguration::new(deferred_source);
                let _: RawWriteBackSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
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
            RpgMakerCommandArguments::Lua(arguments) => {
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let runtime =
                    DeferredLuaRuntimeConfiguration::new(Arc::clone(&deferred_source)).resolve()?;
                let ProjectLuaArguments {
                    project,
                    profile,
                    script,
                    arguments,
                } = arguments;
                let standard_profile =
                    ConfiguredProjectLuaStandardProfile::new(deferred_source, profile.as_deref())?;
                Ok(Self::Lua(ConfiguredProjectLuaCommand {
                    project_name: project.name,
                    common,
                    cpu: build_cpu_configuration(),
                    lua: SelectedLuaConfiguration::new(script, runtime),
                    arguments,
                    standard_profile,
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

/// 一次性项目 Lua 命令已经建立的进程级配置。
pub(crate) struct ConfiguredProjectLuaCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    lua: SelectedLuaConfiguration,
    arguments: Vec<String>,
    standard_profile: ConfiguredProjectLuaStandardProfile,
}

impl ConfiguredProjectLuaCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn lua(&self) -> &SelectedLuaConfiguration {
        &self.lua
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub(crate) fn into_standard_profile(self) -> ConfiguredProjectLuaStandardProfile {
        self.standard_profile
    }
}

/// 项目 Lua 的 Standard Profile 选择；省略 Profile 时保留到 `ctx.standard.open()`。
#[derive(Clone)]
pub(crate) enum ConfiguredProjectLuaStandardProfile {
    Deferred {
        source: Arc<DeferredConfigurationSource>,
    },
    Resolved {
        configuration_path: PathBuf,
        configuration: Arc<TranslateConfiguration>,
    },
}

impl ConfiguredProjectLuaStandardProfile {
    fn new(
        source: Arc<DeferredConfigurationSource>,
        explicit_profile_id: Option<&str>,
    ) -> Result<Self, ConfigurationLoadError> {
        match explicit_profile_id {
            Some(profile_id) => {
                let configuration_path = source.path().to_path_buf();
                resolve_project_lua_standard_profile(&source, profile_id).map(|configuration| {
                    Self::Resolved {
                        configuration_path,
                        configuration: Arc::new(configuration),
                    }
                })
            }
            None => Ok(Self::Deferred { source }),
        }
    }

    pub(crate) fn explicit_profile_id(&self) -> Option<&str> {
        match self {
            Self::Deferred { .. } => None,
            Self::Resolved { configuration, .. } => Some(configuration.profile().id()),
        }
    }

    /// 在 Standard 能力真正打开时精确解析显式或项目状态给出的 Profile。
    pub(crate) fn resolve(
        &self,
        profile_id: &str,
    ) -> Result<Arc<TranslateConfiguration>, ConfigurationLoadError> {
        match self {
            Self::Deferred { source } => {
                resolve_project_lua_standard_profile(source.as_ref(), profile_id).map(Arc::new)
            }
            Self::Resolved { configuration, .. } if configuration.profile().id() == profile_id => {
                Ok(Arc::clone(configuration))
            }
            Self::Resolved {
                configuration_path,
                configuration,
            } => Err(ConfigurationLoadError::ProfileSelectionConflict {
                path: configuration_path.clone(),
                explicit_profile: configuration.profile().id().to_owned(),
                requested_profile: profile_id.to_owned(),
            }),
        }
    }
}

fn resolve_project_lua_standard_profile(
    source: &DeferredConfigurationSource,
    profile_id: &str,
) -> Result<TranslateConfiguration, ConfigurationLoadError> {
    validate_exact_identifier("Profile ID", profile_id)
        .map_err(ConfigurationLoadError::InvalidValue)
        .map_err(|error| error.with_configuration_path(source.path()))?;
    let raw: RawTranslateSelection = parse_selected(
        source.source(),
        source.path(),
        source.toml_index(),
        ConfigurationSelection::Translate,
    )?;
    PendingTranslateConfiguration::build(
        source.path().parent().expect("配置文件必须拥有父目录"),
        raw.prompts,
        raw.languages,
        raw.rpg_maker,
    )
    .map_err(ConfigurationLoadError::InvalidValue)
    .map_err(|error| error.with_configuration_path(source.path()))?
    .resolve(source, profile_id)
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
pub(crate) struct DeferredConfigurationSource {
    path: PathBuf,
    source: Zeroizing<String>,
    toml_index: Arc<ConfigurationTomlIndex>,
}

impl DeferredConfigurationSource {
    fn new(path: &Path, source: &str, toml_index: Arc<ConfigurationTomlIndex>) -> Self {
        Self {
            path: path.to_path_buf(),
            source: Zeroizing::new(source.to_owned()),
            toml_index,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source(&self) -> &str {
        self.source.as_str()
    }

    fn toml_index(&self) -> &ConfigurationTomlIndex {
        self.toml_index.as_ref()
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
        let selected_profile = parse_selected_translation_profile(
            source.source(),
            source.path(),
            source.toml_index(),
            profile_id,
        )?;
        let llm_client_id = selected_profile.llm_client.clone();
        let raw_client = parse_selected_llm_client(
            source.source(),
            source.path(),
            source.toml_index(),
            &llm_client_id,
        )?;
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
        failure: ConfigurationTomlFailureKind,
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

    /// 配置加载失败的唯一安全结构化投影。
    ///
    /// 进程启动路径与命令内延迟配置解析路径都消费这一份映射,同一失败在
    /// 不同触发时机呈现完全相同的 code、reason 与恢复事实。
    pub(crate) fn safe_diagnostic(&self) -> crate::diagnostic::SafeDiagnostic {
        use crate::diagnostic::{
            DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact,
            DiagnosticReason, DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic,
        };

        fn as_u64(value: usize) -> u64 {
            u64::try_from(value).unwrap_or(u64::MAX)
        }

        match self {
            Self::Open { path, source } => SafeDiagnostic::io(
                DiagnosticCode::ConfigurationOpen,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                "open_configuration",
                source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            Self::NotAFile { path } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationNotFile,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Read { path, source } => SafeDiagnostic::io(
                DiagnosticCode::ConfigurationRead,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                "read_configuration",
                source,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidUtf8,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: as_u64(*valid_up_to),
                    error_len: error_len.map(as_u64),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::InvalidToml {
                path,
                location,
                resource,
                failure,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidToml,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidToml {
                    line: location.map(|value| as_u64(value.line())),
                    column: location.map(|value| as_u64(value.column())),
                    resource: crate::user_text::sanitize_user_text(resource),
                    failure: *failure,
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::InvalidValue(source) => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidValue,
                DiagnosticStage::Configuration,
                DiagnosticSubject::field(source.field()),
                DiagnosticReason::InvalidConfigurationValue {
                    rule: source.reason().clone(),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::InvalidValueAtPath { path, source } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidValue,
                DiagnosticStage::Configuration,
                DiagnosticSubject::field(source.field()),
                DiagnosticReason::InvalidConfigurationValue {
                    rule: source.reason().clone(),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(RecoveryFact::path(path)),
            Self::TranslationProfileNotFound { path, profile_id } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationProfileNotFound,
                DiagnosticStage::Configuration,
                DiagnosticSubject::profile(profile_id),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(RecoveryFact::path(path)),
            Self::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationProfileConflict,
                DiagnosticStage::Configuration,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::ConflictingValues),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            )
            .with_recovery(RecoveryFact::component(format!(
                "explicit_profile={}; requested_profile={}",
                crate::user_text::sanitize_user_text(explicit_profile),
                crate::user_text::sanitize_user_text(requested_profile)
            ))),
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
                failure,
            } => {
                write!(formatter, "{}", path.display())?;
                if let Some(location) = location {
                    write!(formatter, ":{}:{}", location.line, location.column)?;
                }
                write!(
                    formatter,
                    "：{resource}：{}",
                    configuration_toml_failure_description(*failure)
                )
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

/// 按现实消费范围选择需要建立的配置值不变量。
#[derive(Clone, Copy)]
enum ConfigurationSelection {
    Common,
    NoAdditionalFields,
    Translate,
    SelectedProfile(usize),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IndexedTableKind {
    Table,
    TableArray,
}

impl IndexedTableKind {
    const fn expected(self) -> ConfigurationTomlValueKind {
        match self {
            Self::Table => ConfigurationTomlValueKind::Table,
            Self::TableArray => ConfigurationTomlValueKind::TableArray,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IndexedScalarKind {
    String,
    Integer,
    Boolean,
    Other,
}

#[derive(Clone)]
enum IndexedValueShape {
    Scalar {
        kind: IndexedScalarKind,
        span: Range<usize>,
    },
    Array {
        items: Vec<IndexedValueShape>,
        span: Range<usize>,
    },
    InlineTable {
        fields: Vec<IndexedInlineField>,
        span: Range<usize>,
    },
}

impl IndexedValueShape {
    fn span(&self) -> &Range<usize> {
        match self {
            Self::Scalar { span, .. }
            | Self::Array { span, .. }
            | Self::InlineTable { span, .. } => span,
        }
    }

    fn matches(&self, expected: ConfigurationTomlValueKind) -> bool {
        match expected {
            ConfigurationTomlValueKind::String => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::String,
                    ..
                }
            ),
            ConfigurationTomlValueKind::Integer => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::Integer,
                    ..
                }
            ),
            ConfigurationTomlValueKind::Boolean => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::Boolean,
                    ..
                }
            ),
            ConfigurationTomlValueKind::StringOrBoolean => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::String | IndexedScalarKind::Boolean,
                    ..
                }
            ),
            ConfigurationTomlValueKind::StringArray => {
                self.array_items_match(|item| item.matches(ConfigurationTomlValueKind::String))
            }
            ConfigurationTomlValueKind::IntegerArray => {
                self.array_items_match(|item| item.matches(ConfigurationTomlValueKind::Integer))
            }
            ConfigurationTomlValueKind::StringPairArray => self.array_items_match(|item| {
                matches!(
                    item,
                    Self::Array { items, .. }
                        if items.len() == 2
                            && items
                                .iter()
                                .all(|part| part.matches(ConfigurationTomlValueKind::String))
                )
            }),
            ConfigurationTomlValueKind::Table => {
                matches!(self, Self::InlineTable { .. })
            }
            ConfigurationTomlValueKind::TableArray => false,
        }
    }

    fn array_items_match(&self, predicate: impl Fn(&IndexedValueShape) -> bool) -> bool {
        let Self::Array { items, .. } = self else {
            return false;
        };
        items.iter().all(predicate)
    }
}

#[derive(Clone)]
struct IndexedInlineField {
    path: Vec<String>,
    shape: IndexedValueShape,
    key_span: Range<usize>,
}

#[derive(Clone)]
struct IndexedTable {
    path: Vec<String>,
    kind: IndexedTableKind,
    occurrence: Option<usize>,
    span: Range<usize>,
}

#[derive(Clone)]
struct IndexedField {
    path: Vec<String>,
    table_occurrence: Option<usize>,
    shape: IndexedValueShape,
    key_span: Range<usize>,
}

/// TOML 原文的结构与 span 索引。
///
/// 索引只解码字段名；值正文交给 `()` 作为 decoder sink，仅取得标量/数组/表形态并完成
/// TOML 语法校验。API key、Prompt、parameters 和其他正文不会进入索引或错误对象。
#[derive(Clone)]
struct ConfigurationTomlIndex {
    tables: Vec<IndexedTable>,
    fields: Vec<IndexedField>,
}

impl ConfigurationTomlIndex {
    fn build(source: &str, path: &Path) -> Result<Self, ConfigurationLoadError> {
        let source_view = Source::new(source);
        let tokens = source_view.lex().into_vec();
        let mut events = Vec::<Event>::new();
        let mut errors = Vec::<ParseError>::new();
        {
            let mut validating = ValidateWhitespace::new(&mut events, source_view);
            parse_document(&tokens, &mut validating, &mut errors);
        }

        for event in &events {
            let raw = source_view
                .get(event)
                .expect("toml_parser 事件 span 必须来自同一原文");
            match event.kind() {
                EventKind::SimpleKey => {
                    let mut decoded = String::new();
                    raw.decode_key(&mut decoded, &mut errors);
                }
                EventKind::Scalar => {
                    let _ = raw.decode_scalar(&mut (), &mut errors);
                }
                _ => {}
            }
        }

        if let Some(offset) = earliest_parse_error_offset(&errors) {
            return Err(configuration_toml_failure(
                path,
                source,
                Some(offset..offset),
                "TOML 文档".to_owned(),
                ConfigurationTomlFailureKind::Syntax,
            ));
        }

        ConfigurationTomlIndexParser::new(source_view, &events)
            .parse()
            .map_err(|failure| {
                configuration_toml_failure(
                    path,
                    source,
                    Some(failure.span),
                    failure.resource,
                    failure.failure,
                )
            })
    }

    fn validate_complete_field_set(
        &self,
        source: &str,
        path: &Path,
    ) -> Result<(), ConfigurationLoadError> {
        for table in &self.tables {
            if let Some((value_path, expected)) =
                ConfigurationFieldContract::owning_value_field(&table.path)
            {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    value_path,
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
            let Some(expected) = ConfigurationFieldContract::table_kind(&table.path) else {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    &table.path,
                    ConfigurationTomlFailureKind::UnknownField,
                ));
            };
            if table.kind != expected {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    &table.path,
                    ConfigurationTomlFailureKind::TypeMismatch {
                        expected: expected.expected(),
                    },
                ));
            }
        }

        for field in &self.fields {
            // 一个本应为标量或数组的字段即使实际写成内联表，内联表成员也不成为
            // 独立配置字段。所选消费范围会在校验该字段时报告其 shape 不符；未选择
            // 的动态 client/profile 仍保持不读取、不校验正文的既有契约。
            if ConfigurationFieldContract::is_descendant_of_value_field(&field.path) {
                continue;
            }
            let expected = ConfigurationFieldContract::field_kind(&field.path)
                .or_else(|| ConfigurationFieldContract::structural_kind(&field.path));
            let Some(expected) = expected else {
                return Err(self.failure_at(
                    source,
                    path,
                    &field.key_span,
                    &field.path,
                    ConfigurationTomlFailureKind::UnknownField,
                ));
            };
            if ConfigurationFieldContract::structural_kind(&field.path).is_some()
                && !field.shape.matches(expected)
            {
                return Err(self.failure_at(
                    source,
                    path,
                    field.shape.span(),
                    &field.path,
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
        }
        Ok(())
    }

    fn validate_selection(
        &self,
        source: &str,
        path: &Path,
        selection: ConfigurationSelection,
    ) -> Result<(), ConfigurationLoadError> {
        match selection {
            ConfigurationSelection::Common => {
                for field_path in ConfigurationFieldContract::COMMON_REQUIRED_FIELDS {
                    self.require_contract_field(source, path, field_path, None)?;
                }
            }
            ConfigurationSelection::NoAdditionalFields => {}
            ConfigurationSelection::Translate => self.validate_translate(source, path)?,
            ConfigurationSelection::SelectedProfile(occurrence) => {
                for field in ConfigurationFieldContract::PROFILE_REQUIRED_FIELDS {
                    self.require_contract_field(
                        source,
                        path,
                        &["rpg_maker", "translation_profiles", field],
                        Some(occurrence),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn validate_selected_client(
        &self,
        source: &str,
        path: &Path,
        client_id: &str,
    ) -> Result<(), ConfigurationLoadError> {
        let mut field_path = vec!["llm", "clients", client_id, ""];
        for field in ConfigurationFieldContract::CLIENT_REQUIRED_FIELDS {
            field_path[3] = field;
            self.require_contract_field(source, path, &field_path, None)?;
        }

        let rate_limit_path = ["llm", "clients", client_id, "rate_limit"];
        if self.has_table(&rate_limit_path, None) {
            for field in ConfigurationFieldContract::RATE_LIMIT_REQUIRED_FIELDS {
                self.require_contract_field(
                    source,
                    path,
                    &["llm", "clients", client_id, "rate_limit", field],
                    None,
                )?;
            }
        }
        Ok(())
    }

    fn validate_translate(&self, source: &str, path: &Path) -> Result<(), ConfigurationLoadError> {
        for field_path in ConfigurationFieldContract::TRANSLATE_REQUIRED_FIELDS {
            self.require_contract_field(source, path, field_path, None)?;
        }
        if self
            .field(&["rpg_maker", "record_translation_tasks"], None)
            .is_some()
        {
            self.require_contract_field(
                source,
                path,
                &["rpg_maker", "record_translation_tasks"],
                None,
            )?;
        }

        let language_tables = self.table_occurrences(&["languages"]);
        if language_tables.is_empty() {
            return Err(self.missing_field(
                source,
                path,
                &["languages"],
                None,
                ConfigurationTomlValueKind::TableArray,
            ));
        }
        for occurrence in language_tables {
            for field in ConfigurationFieldContract::LANGUAGE_BASE_REQUIRED_FIELDS {
                self.require_contract_field(source, path, &["languages", field], Some(occurrence))?;
            }
            for field in ConfigurationFieldContract::LANGUAGE_OPTIONAL_FIELDS {
                self.validate_optional_contract_field(
                    source,
                    path,
                    &["languages", field],
                    Some(occurrence),
                )?;
            }
        }

        let profile_tables = self.table_occurrences(&["rpg_maker", "translation_profiles"]);
        if profile_tables.is_empty() {
            return Err(self.missing_field(
                source,
                path,
                &["rpg_maker", "translation_profiles"],
                None,
                ConfigurationTomlValueKind::TableArray,
            ));
        }
        for occurrence in profile_tables {
            self.require_contract_field(
                source,
                path,
                &["rpg_maker", "translation_profiles", "id"],
                Some(occurrence),
            )?;
        }
        Ok(())
    }

    fn validate_language_variants(
        &self,
        source: &str,
        path: &Path,
        language_types: &[String],
    ) -> Result<(), ConfigurationLoadError> {
        for (occurrence, language_type) in language_types.iter().enumerate() {
            let required =
                match ConfigurationFieldContract::language_variant_required_fields(language_type) {
                    Some(required) => required,
                    _ => {
                        let field = self
                            .field(&["languages", "type"], Some(occurrence))
                            .expect("Translate 基础 contract 已建立每个语言 type 字段");
                        return Err(self.failure_at(
                            source,
                            path,
                            field.shape.span(),
                            &field.path,
                            ConfigurationTomlFailureKind::InvalidValue,
                        ));
                    }
                };
            for field in required {
                self.require_contract_field(source, path, &["languages", field], Some(occurrence))?;
            }
        }
        Ok(())
    }

    fn require_contract_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
    ) -> Result<(), ConfigurationLoadError> {
        let expected = ConfigurationFieldContract::expected_field_kind(field_path);
        self.require_field(source, path, field_path, occurrence, expected)
    }

    fn require_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
        expected: ConfigurationTomlValueKind,
    ) -> Result<(), ConfigurationLoadError> {
        let Some(field) = self.field(field_path, occurrence) else {
            if let Some(descendant) = self.fields.iter().find(|candidate| {
                candidate.table_occurrence == occurrence
                    && field_path.len() < candidate.path.len()
                    && candidate
                        .path
                        .iter()
                        .zip(field_path)
                        .all(|(actual, expected)| actual == expected)
            }) {
                return Err(configuration_toml_failure(
                    path,
                    source,
                    Some(descendant.key_span.clone()),
                    field_path.join("."),
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
            return Err(self.missing_field(source, path, field_path, occurrence, expected));
        };
        if !field.shape.matches(expected) {
            return Err(self.failure_at(
                source,
                path,
                field.shape.span(),
                &field.path,
                ConfigurationTomlFailureKind::TypeMismatch { expected },
            ));
        }
        Ok(())
    }

    fn validate_optional_contract_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
    ) -> Result<(), ConfigurationLoadError> {
        if self.field(field_path, occurrence).is_some() {
            self.require_contract_field(source, path, field_path, occurrence)?;
        }
        Ok(())
    }

    fn field(&self, path: &[&str], occurrence: Option<usize>) -> Option<&IndexedField> {
        self.fields.iter().find(|field| {
            field.table_occurrence == occurrence
                && field.path.len() == path.len()
                && field
                    .path
                    .iter()
                    .zip(path)
                    .all(|(actual, expected)| actual == expected)
        })
    }

    fn has_table(&self, path: &[&str], occurrence: Option<usize>) -> bool {
        self.tables.iter().any(|table| {
            table.occurrence == occurrence
                && table.path.len() == path.len()
                && table
                    .path
                    .iter()
                    .zip(path)
                    .all(|(actual, expected)| actual == expected)
        }) || self
            .field(path, occurrence)
            .is_some_and(|field| field.shape.matches(ConfigurationTomlValueKind::Table))
    }

    fn table_occurrences(&self, path: &[&str]) -> Vec<usize> {
        self.tables
            .iter()
            .filter(|table| {
                table.kind == IndexedTableKind::TableArray
                    && table.path.len() == path.len()
                    && table
                        .path
                        .iter()
                        .zip(path)
                        .all(|(actual, expected)| actual == expected)
            })
            .filter_map(|table| table.occurrence)
            .collect()
    }

    fn missing_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
        _expected: ConfigurationTomlValueKind,
    ) -> ConfigurationLoadError {
        let owner = &field_path[..field_path.len().saturating_sub(1)];
        let span = self
            .tables
            .iter()
            .find(|table| {
                table.occurrence == occurrence
                    && table.path.len() == owner.len()
                    && table
                        .path
                        .iter()
                        .zip(owner)
                        .all(|(actual, expected)| actual == expected)
            })
            .map(|table| table.span.clone());
        configuration_toml_failure(
            path,
            source,
            span,
            field_path.join("."),
            ConfigurationTomlFailureKind::MissingField,
        )
    }

    fn failure_at(
        &self,
        source: &str,
        path: &Path,
        span: &Range<usize>,
        resource: &[String],
        failure: ConfigurationTomlFailureKind,
    ) -> ConfigurationLoadError {
        configuration_toml_failure(
            path,
            source,
            Some(span.clone()),
            resource.join("."),
            failure,
        )
    }

    fn resource_at(&self, offset: usize) -> String {
        self.fields
            .iter()
            .filter(|field| field.key_span.start <= offset || field.shape.span().start <= offset)
            .max_by_key(|field| field.key_span.start.max(field.shape.span().start))
            .map_or_else(|| "TOML 文档".to_owned(), |field| field.path.join("."))
    }
}

struct ConfigurationFieldContract;

impl ConfigurationFieldContract {
    const COMMON_REQUIRED_FIELDS: &'static [&'static [&'static str]] = &[&["projects", "root"]];
    const TRANSLATE_REQUIRED_FIELDS: &'static [&'static [&'static str]] = &[
        &["prompts", "root"],
        &["prompts", "locale"],
        &["prompts", "thinking_output"],
    ];
    const LANGUAGE_BASE_REQUIRED_FIELDS: &'static [&'static str] = &["type", "id"];
    const LANGUAGE_OPTIONAL_FIELDS: &'static [&'static str] = &[
        "minimum_kana_characters",
        "minimum_word_count",
        "minimum_letter_count",
        "minimum_copied_word_count",
        "minimum_copied_letter_count",
        "allowed_terms",
        "ignored_terms",
        "quote_repair_pairs",
    ];
    const JAPANESE_REQUIRED_FIELDS: &'static [&'static str] = &[
        "minimum_kana_characters",
        "allowed_terms",
        "quote_repair_pairs",
    ];
    const ENGLISH_REQUIRED_FIELDS: &'static [&'static str] = &[
        "minimum_word_count",
        "minimum_letter_count",
        "ignored_terms",
        "minimum_copied_word_count",
        "minimum_copied_letter_count",
        "allowed_terms",
    ];
    const PROFILE_REQUIRED_FIELDS: &'static [&'static str] =
        &["id", "llm_client", "target_task_user_message_characters"];
    const CLIENT_REQUIRED_FIELDS: &'static [&'static str] = &[
        "url",
        "api_key",
        "model",
        "max_concurrent_requests",
        "connect_timeout_ms",
        "read_timeout_ms",
        "request_timeout_ms",
        "proxy",
        "additional_pem_files",
        "retry_delays_ms",
        "max_retry_after_ms",
        "parameters",
    ];
    const RATE_LIMIT_REQUIRED_FIELDS: &'static [&'static str] = &["requests_per_minute", "burst"];

    fn language_variant_required_fields(language_type: &str) -> Option<&'static [&'static str]> {
        match language_type {
            "japanese" => Some(Self::JAPANESE_REQUIRED_FIELDS),
            "english" => Some(Self::ENGLISH_REQUIRED_FIELDS),
            _ => None,
        }
    }

    fn expected_field_kind(path: &[&str]) -> ConfigurationTomlValueKind {
        let path = path
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>();
        Self::field_kind(&path).expect("必填或可选字段必须由唯一配置字段契约声明")
    }

    fn owning_value_field(path: &[String]) -> Option<(&[String], ConfigurationTomlValueKind)> {
        (1..=path.len()).find_map(|prefix_len| {
            let prefix = &path[..prefix_len];
            Self::field_kind(prefix).map(|expected| (prefix, expected))
        })
    }

    fn is_descendant_of_value_field(path: &[String]) -> bool {
        path.len() > 1 && Self::owning_value_field(&path[..path.len() - 1]).is_some()
    }

    fn table_kind(path: &[String]) -> Option<IndexedTableKind> {
        match path {
            [first] if matches!(first.as_str(), "projects" | "prompts" | "llm" | "rpg_maker") => {
                Some(IndexedTableKind::Table)
            }
            [llm, clients] if llm == "llm" && clients == "clients" => Some(IndexedTableKind::Table),
            [llm, clients, _] if llm == "llm" && clients == "clients" => {
                Some(IndexedTableKind::Table)
            }
            [llm, clients, _, rate_limit]
                if llm == "llm" && clients == "clients" && rate_limit == "rate_limit" =>
            {
                Some(IndexedTableKind::Table)
            }
            [languages] if languages == "languages" => Some(IndexedTableKind::TableArray),
            [rpg_maker, profiles]
                if rpg_maker == "rpg_maker" && profiles == "translation_profiles" =>
            {
                Some(IndexedTableKind::TableArray)
            }
            _ => None,
        }
    }

    fn structural_kind(path: &[String]) -> Option<ConfigurationTomlValueKind> {
        Self::table_kind(path).map(IndexedTableKind::expected)
    }

    fn field_kind(path: &[String]) -> Option<ConfigurationTomlValueKind> {
        let kind = match path {
            [projects, root] if projects == "projects" && root == "root" => {
                ConfigurationTomlValueKind::String
            }
            [prompts, field]
                if prompts == "prompts" && matches!(field.as_str(), "root" | "locale") =>
            {
                ConfigurationTomlValueKind::String
            }
            [prompts, thinking] if prompts == "prompts" && thinking == "thinking_output" => {
                ConfigurationTomlValueKind::Boolean
            }
            [llm, clients, _, field]
                if llm == "llm"
                    && clients == "clients"
                    && matches!(field.as_str(), "url" | "api_key" | "model" | "parameters") =>
            {
                ConfigurationTomlValueKind::String
            }
            [llm, clients, _, field]
                if llm == "llm"
                    && clients == "clients"
                    && matches!(
                        field.as_str(),
                        "max_concurrent_requests"
                            | "connect_timeout_ms"
                            | "read_timeout_ms"
                            | "request_timeout_ms"
                            | "max_retry_after_ms"
                    ) =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [llm, clients, _, proxy]
                if llm == "llm" && clients == "clients" && proxy == "proxy" =>
            {
                ConfigurationTomlValueKind::StringOrBoolean
            }
            [llm, clients, _, pem]
                if llm == "llm" && clients == "clients" && pem == "additional_pem_files" =>
            {
                ConfigurationTomlValueKind::StringArray
            }
            [llm, clients, _, delays]
                if llm == "llm" && clients == "clients" && delays == "retry_delays_ms" =>
            {
                ConfigurationTomlValueKind::IntegerArray
            }
            [llm, clients, _, rate_limit, field]
                if llm == "llm"
                    && clients == "clients"
                    && rate_limit == "rate_limit"
                    && matches!(field.as_str(), "requests_per_minute" | "burst") =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [languages, field]
                if languages == "languages" && matches!(field.as_str(), "type" | "id") =>
            {
                ConfigurationTomlValueKind::String
            }
            [languages, field]
                if languages == "languages"
                    && matches!(
                        field.as_str(),
                        "minimum_kana_characters"
                            | "minimum_word_count"
                            | "minimum_letter_count"
                            | "minimum_copied_word_count"
                            | "minimum_copied_letter_count"
                    ) =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [languages, field]
                if languages == "languages"
                    && matches!(field.as_str(), "allowed_terms" | "ignored_terms") =>
            {
                ConfigurationTomlValueKind::StringArray
            }
            [languages, pairs] if languages == "languages" && pairs == "quote_repair_pairs" => {
                ConfigurationTomlValueKind::StringPairArray
            }
            [rpg_maker, record]
                if rpg_maker == "rpg_maker" && record == "record_translation_tasks" =>
            {
                ConfigurationTomlValueKind::Boolean
            }
            [rpg_maker, profiles, field]
                if rpg_maker == "rpg_maker"
                    && profiles == "translation_profiles"
                    && matches!(field.as_str(), "id" | "llm_client") =>
            {
                ConfigurationTomlValueKind::String
            }
            [rpg_maker, profiles, target]
                if rpg_maker == "rpg_maker"
                    && profiles == "translation_profiles"
                    && target == "target_task_user_message_characters" =>
            {
                ConfigurationTomlValueKind::Integer
            }
            _ => return None,
        };
        Some(kind)
    }
}

struct IndexedBuildFailure {
    span: Range<usize>,
    resource: String,
    failure: ConfigurationTomlFailureKind,
}

struct ConfigurationTomlIndexParser<'a> {
    source: Source<'a>,
    events: &'a [Event],
    cursor: usize,
    current_table: Vec<String>,
    current_occurrence: Option<usize>,
    table_occurrences: HashMap<Vec<String>, usize>,
    declared_tables: BTreeSet<Vec<String>>,
    assigned_fields: BTreeSet<(Vec<String>, Option<usize>)>,
    tables: Vec<IndexedTable>,
    fields: Vec<IndexedField>,
}

impl<'a> ConfigurationTomlIndexParser<'a> {
    fn new(source: Source<'a>, events: &'a [Event]) -> Self {
        Self {
            source,
            events,
            cursor: 0,
            current_table: Vec::new(),
            current_occurrence: None,
            table_occurrences: HashMap::new(),
            declared_tables: BTreeSet::new(),
            assigned_fields: BTreeSet::new(),
            tables: Vec::new(),
            fields: Vec::new(),
        }
    }

    fn parse(mut self) -> Result<ConfigurationTomlIndex, IndexedBuildFailure> {
        while self.skip_trivia() {
            match self.peek_kind() {
                Some(EventKind::StdTableOpen | EventKind::ArrayTableOpen) => {
                    self.parse_table_header()?;
                }
                Some(EventKind::SimpleKey) => self.parse_assignment()?,
                Some(_) => return Err(self.syntax_failure(self.current_span())),
                None => break,
            }
        }
        Ok(ConfigurationTomlIndex {
            tables: self.tables,
            fields: self.fields,
        })
    }

    fn parse_table_header(&mut self) -> Result<(), IndexedBuildFailure> {
        let open = self.next().expect("已确认存在表头开始事件");
        let (kind, close_kind) = match open.kind() {
            EventKind::StdTableOpen => (IndexedTableKind::Table, EventKind::StdTableClose),
            EventKind::ArrayTableOpen => (IndexedTableKind::TableArray, EventKind::ArrayTableClose),
            _ => unreachable!("调用方只在表头事件调用"),
        };
        let (table_path, key_span) = self.parse_key_path_until(close_kind)?;
        if table_path.is_empty() {
            return Err(self.syntax_failure(span_range(open.span())));
        }
        let close = self.next().expect("键路径解析必须停在表头结束事件");
        let span = open.span().start()..close.span().end();

        let conflicts_with_value =
            self.assigned_fields
                .iter()
                .any(|(assigned_path, assigned_occurrence)| {
                    !(kind == IndexedTableKind::TableArray && assigned_occurrence.is_some())
                        && value_paths_conflict(assigned_path, &table_path)
                });
        let conflicts_with_table = self.tables.iter().any(|declared| {
            if declared.path == table_path {
                kind != IndexedTableKind::TableArray
                    || declared.kind != IndexedTableKind::TableArray
            } else {
                path_is_prefix(&table_path, &declared.path)
            }
        });
        if conflicts_with_value || conflicts_with_table {
            return Err(IndexedBuildFailure {
                span: key_span,
                resource: table_path.join("."),
                failure: ConfigurationTomlFailureKind::DuplicateField,
            });
        }

        let occurrence = if kind == IndexedTableKind::TableArray {
            let next = self
                .table_occurrences
                .entry(table_path.clone())
                .or_default();
            let occurrence = *next;
            *next += 1;
            Some(occurrence)
        } else {
            let inserted = self.declared_tables.insert(table_path.clone());
            debug_assert!(inserted, "重复显式表必须已由结构冲突检查拒绝");
            None
        };
        self.current_table.clone_from(&table_path);
        self.current_occurrence = occurrence;
        self.tables.push(IndexedTable {
            path: table_path,
            kind,
            occurrence,
            span,
        });
        Ok(())
    }

    fn parse_assignment(&mut self) -> Result<(), IndexedBuildFailure> {
        let (local_path, key_span) = self.parse_key_path_until(EventKind::KeyValSep)?;
        if local_path.is_empty() {
            return Err(self.syntax_failure(key_span));
        }
        let _separator = self.next().expect("键路径解析必须停在等号事件");
        let shape = self.parse_value()?;
        let mut path = self.current_table.clone();
        path.extend(local_path);
        let identity = (path.clone(), self.current_occurrence);
        let conflicts_with_value =
            self.assigned_fields
                .iter()
                .any(|(assigned_path, assigned_occurrence)| {
                    *assigned_occurrence == self.current_occurrence
                        && value_paths_conflict(assigned_path, &path)
                });
        let conflicts_with_table = self
            .tables
            .iter()
            .any(|table| path_is_prefix(&path, &table.path));
        if conflicts_with_value || conflicts_with_table {
            return Err(IndexedBuildFailure {
                span: key_span,
                resource: path.join("."),
                failure: ConfigurationTomlFailureKind::DuplicateField,
            });
        }
        let inserted = self.assigned_fields.insert(identity);
        debug_assert!(inserted, "重复赋值必须已由结构冲突检查拒绝");
        self.fields.push(IndexedField {
            path: path.clone(),
            table_occurrence: self.current_occurrence,
            shape: shape.clone(),
            key_span: key_span.clone(),
        });
        self.flatten_inline_fields(&path, self.current_occurrence, &shape)?;
        Ok(())
    }

    fn flatten_inline_fields(
        &mut self,
        prefix: &[String],
        occurrence: Option<usize>,
        shape: &IndexedValueShape,
    ) -> Result<(), IndexedBuildFailure> {
        let IndexedValueShape::InlineTable { fields, .. } = shape else {
            return Ok(());
        };
        for inline in fields {
            let mut path = prefix.to_vec();
            path.extend(inline.path.iter().cloned());
            if !self.assigned_fields.insert((path.clone(), occurrence)) {
                return Err(IndexedBuildFailure {
                    span: inline.key_span.clone(),
                    resource: path.join("."),
                    failure: ConfigurationTomlFailureKind::DuplicateField,
                });
            }
            self.fields.push(IndexedField {
                path: path.clone(),
                table_occurrence: occurrence,
                shape: inline.shape.clone(),
                key_span: inline.key_span.clone(),
            });
            self.flatten_inline_fields(&path, occurrence, &inline.shape)?;
        }
        Ok(())
    }

    fn parse_key_path_until(
        &mut self,
        terminator: EventKind,
    ) -> Result<(Vec<String>, Range<usize>), IndexedBuildFailure> {
        let mut path = Vec::new();
        let mut start = None;
        let mut end = None;
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(terminator) {
                let point = start.unwrap_or_else(|| self.current_span().start);
                return Ok((path, point..end.unwrap_or(point)));
            }
            let Some(event) = self.next() else {
                return Err(self.syntax_failure(self.current_span()));
            };
            if event.kind() != EventKind::SimpleKey {
                return Err(self.syntax_failure(span_range(event.span())));
            }
            let raw = self
                .source
                .get(event)
                .expect("键事件 span 必须来自同一原文");
            let mut key = String::new();
            let mut errors = Vec::new();
            raw.decode_key(&mut key, &mut errors);
            if !errors.is_empty() {
                return Err(self.syntax_failure(span_range(event.span())));
            }
            start.get_or_insert(event.span().start());
            end = Some(event.span().end());
            path.push(key);
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::KeySep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(terminator) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn parse_value(&mut self) -> Result<IndexedValueShape, IndexedBuildFailure> {
        self.skip_trivia();
        let Some(event) = self.next() else {
            return Err(self.syntax_failure(self.current_span()));
        };
        match event.kind() {
            EventKind::Scalar => {
                let raw = self
                    .source
                    .get(event)
                    .expect("值事件 span 必须来自同一原文");
                let mut errors = Vec::new();
                let kind = match raw.decode_scalar(&mut (), &mut errors) {
                    ScalarKind::String => IndexedScalarKind::String,
                    ScalarKind::Integer(_) => IndexedScalarKind::Integer,
                    ScalarKind::Boolean(_) => IndexedScalarKind::Boolean,
                    ScalarKind::DateTime | ScalarKind::Float => IndexedScalarKind::Other,
                };
                if !errors.is_empty() {
                    return Err(self.syntax_failure(span_range(event.span())));
                }
                Ok(IndexedValueShape::Scalar {
                    kind,
                    span: span_range(event.span()),
                })
            }
            EventKind::ArrayOpen => self.parse_array(event.span()),
            EventKind::InlineTableOpen => self.parse_inline_table(event.span()),
            _ => Err(self.syntax_failure(span_range(event.span()))),
        }
    }

    fn parse_array(&mut self, open: Span) -> Result<IndexedValueShape, IndexedBuildFailure> {
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ArrayClose) {
                let close = self.next().expect("已确认数组结束事件存在");
                return Ok(IndexedValueShape::Array {
                    items,
                    span: open.start()..close.span().end(),
                });
            }
            items.push(self.parse_value()?);
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ValueSep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(EventKind::ArrayClose) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn parse_inline_table(&mut self, open: Span) -> Result<IndexedValueShape, IndexedBuildFailure> {
        let mut fields = Vec::new();
        let mut seen = BTreeSet::<Vec<String>>::new();
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::InlineTableClose) {
                let close = self.next().expect("已确认内联表结束事件存在");
                return Ok(IndexedValueShape::InlineTable {
                    fields,
                    span: open.start()..close.span().end(),
                });
            }
            let (path, key_span) = self.parse_key_path_until(EventKind::KeyValSep)?;
            let _separator = self.next().expect("内联表字段必须拥有等号");
            let shape = self.parse_value()?;
            if seen
                .iter()
                .any(|existing| value_paths_conflict(existing, &path))
            {
                return Err(IndexedBuildFailure {
                    span: key_span,
                    resource: path.join("."),
                    failure: ConfigurationTomlFailureKind::DuplicateField,
                });
            }
            let inserted = seen.insert(path.clone());
            debug_assert!(inserted, "重复内联字段必须已由结构冲突检查拒绝");
            fields.push(IndexedInlineField {
                path,
                shape,
                key_span,
            });
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ValueSep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(EventKind::InlineTableClose) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn skip_trivia(&mut self) -> bool {
        while matches!(
            self.peek_kind(),
            Some(EventKind::Whitespace | EventKind::Comment | EventKind::Newline)
        ) {
            self.cursor += 1;
        }
        self.cursor < self.events.len()
    }

    fn peek_kind(&self) -> Option<EventKind> {
        self.events.get(self.cursor).map(Event::kind)
    }

    fn next(&mut self) -> Option<Event> {
        let event = self.events.get(self.cursor).copied()?;
        self.cursor += 1;
        Some(event)
    }

    fn current_span(&self) -> Range<usize> {
        self.events
            .get(self.cursor)
            .map(|event| span_range(event.span()))
            .unwrap_or_else(|| {
                let end = self.source.input().len();
                end..end
            })
    }

    fn syntax_failure(&self, span: Range<usize>) -> IndexedBuildFailure {
        IndexedBuildFailure {
            span,
            resource: if self.current_table.is_empty() {
                "TOML 文档".to_owned()
            } else {
                self.current_table.join(".")
            },
            failure: ConfigurationTomlFailureKind::Syntax,
        }
    }
}

fn path_is_prefix(prefix: &[String], path: &[String]) -> bool {
    prefix.len() <= path.len()
        && prefix
            .iter()
            .zip(path)
            .all(|(expected, actual)| expected == actual)
}

fn value_paths_conflict(left: &[String], right: &[String]) -> bool {
    path_is_prefix(left, right) || path_is_prefix(right, left)
}

fn earliest_parse_error_offset(errors: &[ParseError]) -> Option<usize> {
    errors
        .iter()
        .filter_map(|error| error.unexpected().or_else(|| error.context()))
        .map(|span| span.start())
        .min()
        .or_else(|| (!errors.is_empty()).then_some(0))
}

fn span_range(span: Span) -> Range<usize> {
    span.start()..span.end()
}

fn configuration_toml_failure(
    path: &Path,
    source: &str,
    span: Option<Range<usize>>,
    resource: String,
    failure: ConfigurationTomlFailureKind,
) -> ConfigurationLoadError {
    let location = span
        .as_ref()
        .map(|span| source_location(source, span.start));
    ConfigurationLoadError::InvalidToml {
        path: path.to_path_buf(),
        location,
        resource,
        failure,
    }
}

fn invalid_toml(
    path: &Path,
    source: &str,
    index: &ConfigurationTomlIndex,
    error: &toml::de::Error,
) -> ConfigurationLoadError {
    let span = error.span();
    let resource = span.as_ref().map_or_else(
        || "TOML 文档".to_owned(),
        |span| index.resource_at(span.start),
    );
    configuration_toml_failure(
        path,
        source,
        span,
        resource,
        ConfigurationTomlFailureKind::InvalidValue,
    )
}

fn parse_selected<T>(
    source: &str,
    path: &Path,
    index: &ConfigurationTomlIndex,
    selection: ConfigurationSelection,
) -> Result<T, ConfigurationLoadError>
where
    T: serde::de::DeserializeOwned,
{
    index.validate_selection(source, path, selection)?;
    if matches!(selection, ConfigurationSelection::Translate) {
        let discriminators: RawLanguageDiscriminatorSelection =
            toml::from_str(source).map_err(|error| invalid_toml(path, source, index, &error))?;
        let language_types = discriminators
            .languages
            .into_iter()
            .map(|language| language.language_type)
            .collect::<Vec<_>>();
        index.validate_language_variants(source, path, &language_types)?;
    }
    toml::from_str(source).map_err(|error| invalid_toml(path, source, index, &error))
}

fn configuration_toml_failure_description(failure: ConfigurationTomlFailureKind) -> &'static str {
    match failure {
        ConfigurationTomlFailureKind::Syntax => "TOML 语法无效",
        ConfigurationTomlFailureKind::MissingField => "缺少必填字段",
        ConfigurationTomlFailureKind::UnknownField => "当前配置契约不接受该字段",
        ConfigurationTomlFailureKind::DuplicateField => "字段重复",
        ConfigurationTomlFailureKind::TypeMismatch { .. } => "字段类型不符合当前配置契约",
        ConfigurationTomlFailureKind::InvalidValue => "字段值不符合当前配置契约",
    }
}

fn parse_selected_translation_profile(
    source: &str,
    path: &Path,
    index: &ConfigurationTomlIndex,
    requested_id: &str,
) -> Result<RawSelectedTranslationProfileConfiguration, ConfigurationLoadError> {
    let index_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    let selection = TranslationProfileIndexTopSeed { requested_id }
        .deserialize(index_deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?
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
    index.validate_selection(
        source,
        path,
        ConfigurationSelection::SelectedProfile(selected_index),
    )?;

    let profile_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    SelectedTranslationProfileTopSeed { selected_index }
        .deserialize(profile_deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?
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
    index: &ConfigurationTomlIndex,
    requested_id: &str,
) -> Result<RawLlmClientConfiguration, ConfigurationLoadError> {
    validate_exact_identifier("llm client id", requested_id)
        .map_err(ConfigurationLoadError::InvalidValue)?;
    if !index.has_table(&["llm", "clients", requested_id], None) {
        return Err(ConfigurationLoadError::InvalidValue(invalid(
            "llm.clients",
            ConfigurationValueRule::ReferencedClientNotFound,
        )));
    }
    index.validate_selected_client(source, path, requested_id)?;
    let seed = SelectedLlmClientTopSeed { requested_id };
    let deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    let selected = seed
        .deserialize(deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
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
struct RawLanguageDiscriminatorSelection {
    languages: Vec<RawLanguageDiscriminator>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct RawLanguageDiscriminator {
    #[serde(rename = "type")]
    language_type: String,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
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
            project_lua_command(Some("primary")),
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
    fn omitted_project_lua_profile_remains_deferred_until_standard_is_opened() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("locale = \"auto\"", "locale = \"unsupported-locale\"");
        let path = directory.write("deferred-project-lua-profile.toml", &source);

        let configured = load_configuration(&path, project_lua_command(None))
            .expect("普通 Lua 不应提前解析翻译配置");
        let ConfiguredRpgMakerCommand::Lua(configured) = configured else {
            panic!("应建立项目 Lua 配置");
        };
        let standard_profile = configured.into_standard_profile();
        assert_eq!(standard_profile.explicit_profile_id(), None);
        assert!(matches!(
            standard_profile.resolve("primary"),
            Err(ConfigurationLoadError::InvalidValueAtPath { .. })
        ));
    }

    #[test]
    fn explicit_project_lua_profile_is_validated_during_configuration_load() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "explicit-project-lua-profile.toml",
            include_str!("../../config.example.toml"),
        );

        let error = load_configuration(&path, project_lua_command(Some("missing")))
            .err()
            .expect("显式 Profile 必须精确校验");
        assert!(matches!(
            error,
            ConfigurationLoadError::TranslationProfileNotFound { profile_id, .. }
                if profile_id == "missing"
        ));
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
                ConfigurationTomlFailureKind::MissingField,
                None,
            ),
            (
                "missing",
                source.replacen("root = \"prompts\"\n", "", 1),
                "prompts.root",
                "缺少必填字段",
                ConfigurationTomlFailureKind::MissingField,
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
                ConfigurationTomlFailureKind::UnknownField,
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
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::String,
                },
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
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::String,
                },
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
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Boolean,
                },
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
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Integer,
                },
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
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::String,
                },
                Some("API_KEY_TYPE_SENTINEL"),
            ),
        ];

        for (name, source, expected_resource, expected_reason, expected_failure, forbidden_value) in
            cases
        {
            let path = directory.write(format!("diagnostic-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command(false, "primary")) {
                Ok(_) => panic!("无效配置必须失败"),
                Err(error) => error,
            };
            let diagnostic = error.to_string();
            let canonical_path = path.canonicalize().expect("测试配置应可规范化");
            let ConfigurationLoadError::InvalidToml {
                path: error_path,
                location,
                resource,
                failure,
            } = &error
            else {
                panic!("结构、字段和类型失败必须保留 typed TOML 诊断：{error:?}");
            };

            assert!(
                diagnostic.starts_with(canonical_path.display().to_string().as_str()),
                "诊断必须以配置路径开始：{diagnostic}"
            );
            assert_eq!(error_path, &canonical_path);
            assert!(location.is_some(), "已定位的配置失败必须携带一基行列");
            assert_eq!(resource, expected_resource);
            assert_eq!(*failure, expected_failure);
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

        let error = match load_configuration(&path, init_command()) {
            Ok(_) => panic!("完整 TOML 的重复键不得因分区未选中而忽略"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::InvalidToml {
                ref resource,
                failure: ConfigurationTomlFailureKind::DuplicateField,
                ..
            } if resource == "llm.clients.unused.api_key"
        ));
    }

    #[test]
    fn selected_scalar_shape_conflicts_are_typed_without_value_echo() {
        const SENTINEL: &str = "SCALAR_SHAPE_VALUE_SENTINEL";
        const PARAMETERS: &str = "parameters = '''\n{}\n'''";
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml").replace("\r\n", "\n");
        assert!(example.contains(PARAMETERS));

        for (name, replacement) in [
            (
                "table-header",
                format!("[llm.clients.primary.parameters]\nvendor = \"{SENTINEL}\""),
            ),
            ("dotted-key", format!("parameters.vendor = \"{SENTINEL}\"")),
            (
                "inline-table",
                format!("parameters = {{ vendor = \"{SENTINEL}\" }}"),
            ),
        ] {
            let source = example.replacen(PARAMETERS, &replacement, 1);
            let path = directory.write(format!("scalar-shape-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command(false, "primary")) {
                Ok(_) => panic!("所选字符串字段的表形态必须拒绝：{name}"),
                Err(error) => error,
            };
            assert!(matches!(
                &error,
                ConfigurationLoadError::InvalidToml {
                    resource,
                    failure: ConfigurationTomlFailureKind::TypeMismatch {
                        expected: ConfigurationTomlValueKind::String,
                    },
                    ..
                } if resource == "llm.clients.primary.parameters"
            ));
            assert!(
                !format!("{error:?}\n{error}").contains(SENTINEL),
                "shape 诊断不得回显字段正文：{name}"
            );
        }
    }

    #[test]
    fn string_pair_array_shape_requires_exact_pairs() {
        const SENTINEL: &str = "PAIR_SHAPE_VALUE_SENTINEL";
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let original = "quote_repair_pairs = [[\"“\", \"”\"], [\"‘\", \"’\"]]";

        for (name, replacement) in [
            ("one", format!("quote_repair_pairs = [[\"{SENTINEL}\"]]")),
            (
                "three",
                format!("quote_repair_pairs = [[\"a\", \"b\", \"{SENTINEL}\"]]"),
            ),
        ] {
            let source = example.replacen(original, &replacement, 1);
            let path = directory.write(format!("pair-shape-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command(false, "primary")) {
                Ok(_) => panic!("一项必须恰好包含两个字符串：{name}"),
                Err(error) => error,
            };
            assert!(matches!(
                &error,
                ConfigurationLoadError::InvalidToml {
                    resource,
                    failure: ConfigurationTomlFailureKind::TypeMismatch {
                        expected: ConfigurationTomlValueKind::StringPairArray,
                    },
                    ..
                } if resource == "languages.quote_repair_pairs"
            ));
            assert!(!format!("{error:?}\n{error}").contains(SENTINEL));
        }
    }

    #[test]
    fn toml_value_table_structure_conflicts_are_duplicate_fields() {
        let directory = TestDirectory::new();
        for (name, conflict) in [
            ("value-then-table", "a = 1\n[a]\n"),
            ("table-value-then-table", "[a]\nb = 1\n[a.b]\n"),
            (
                "inline-value-then-dotted",
                "a = { value = 1, value.child = 2 }\n",
            ),
            ("value-then-dotted", "a = 1\na.child = 2\n"),
        ] {
            let source = format!("{conflict}\n{}", minimal_init_configuration());
            let path =
                directory.write(format!("duplicate-structure-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, init_command()) {
                Ok(_) => panic!("标量与表结构占用冲突必须拒绝：{name}"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    error,
                    ConfigurationLoadError::InvalidToml {
                        failure: ConfigurationTomlFailureKind::DuplicateField,
                        ..
                    }
                ),
                "结构占用冲突必须稳定分类为 DuplicateField：{name}"
            );
        }
    }

    #[test]
    fn syntax_and_discriminator_value_failures_are_typed_without_value_echo() {
        let directory = TestDirectory::new();
        let syntax_path = directory.write(
            "syntax.toml",
            "[projects]\nroot = \"UNTERMINATED_VALUE_SENTINEL\n",
        );
        let syntax = match load_configuration(&syntax_path, init_command()) {
            Ok(_) => panic!("无效 TOML 语法必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            &syntax,
            ConfigurationLoadError::InvalidToml {
                failure: ConfigurationTomlFailureKind::Syntax,
                ..
            }
        ));
        assert!(!format!("{syntax:?}\n{syntax}").contains("UNTERMINATED_VALUE_SENTINEL"));

        const INVALID_TYPE: &str = "INVALID_LANGUAGE_TYPE_VALUE_SENTINEL";
        let source = include_str!("../../config.example.toml").replacen(
            "type = \"japanese\"",
            format!("type = \"{INVALID_TYPE}\"").as_str(),
            1,
        );
        let value_path = directory.write("language-type.toml", &source);
        let value = match load_configuration(&value_path, translate_command(false, "primary")) {
            Ok(_) => panic!("未知语言类型必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            &value,
            ConfigurationLoadError::InvalidToml {
                resource,
                failure: ConfigurationTomlFailureKind::InvalidValue,
                ..
            } if resource == "languages.type"
        ));
        assert!(!format!("{value:?}\n{value}").contains(INVALID_TYPE));
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

    fn project_lua_command(profile: Option<&str>) -> MzCommand {
        let mut arguments = vec!["att", "mz", "lua", "--name", "demo", "script.lua"];
        if let Some(profile) = profile {
            arguments.splice(5..5, ["--profile", profile]);
        }
        parse_command_vec(arguments)
    }

    fn parse_command<const N: usize>(arguments: [&str; N]) -> MzCommand {
        parse_command_vec(arguments.into_iter().collect())
    }

    fn parse_command_vec(arguments: Vec<&str>) -> MzCommand {
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
