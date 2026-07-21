//! 严格 TOML 配置边界。
//!
//! 原始 TOML 只在本模块存在。结构和字段类型全部通过后，本模块继续建立非零资源
//! 上限、路径基准、语言模块与 Profile 唯一性；业务和根适配器只接收受信配置。

use std::collections::BTreeSet;
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

use crate::language::{
    EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
    JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageId,
    LanguageModule, LanguageModuleCatalog, QuotePair,
};
use crate::rpg_maker::ProjectName;
use crate::rpg_maker::extract::document::RpgMakerDocumentReadingConfig;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStoreConfig;
use crate::rpg_maker::lua::json::HostValueBudget;
use crate::rpg_maker::lua::lua54::TrustedLua54RuntimeConfiguration;
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetReadingConfig;
use crate::rpg_maker::translate::profile::RpgMakerTranslationRequestConfiguration;
use crate::rpg_maker::translate::result_store::RpgMakerStandardTranslationResultStorageConfig;
use crate::rpg_maker::{RpgMakerEngine, RpgMakerLayout};
use crate::runtime::cpu::{CpuExecutorConfig, CpuWorkerThreads};
use crate::runtime::filesystem::{
    DirectoryPublisherConfig, ExclusiveFileLeaseConfig, SystemFileSystemConfig, TreeBudget,
};
use crate::runtime::llm::{
    LlmProxyConfiguration, OpenAiChatCompletionClient, OpenAiExecutorConfiguration,
};
use crate::runtime::project_log::{ProjectLogConfig, ProjectLogConfigInput, ProjectLogLevel};
use crate::runtime::sqlite::{
    RusqliteStorageConfiguration, SqliteJournalMode as RuntimeSqliteJournalMode,
    SqliteSynchronous as RuntimeSqliteSynchronous,
};

const MAX_CONFIGURATION_BYTES: u64 = 4 * 1024 * 1024;
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
    if metadata.len() > MAX_CONFIGURATION_BYTES {
        return Err(ConfigurationLoadError::TooLarge {
            path: configuration_path,
            observed_bytes: metadata.len(),
            maximum_bytes: MAX_CONFIGURATION_BYTES,
        });
    }

    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.by_ref()
        .take(MAX_CONFIGURATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigurationLoadError::Read {
            path: configuration_path.clone(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIGURATION_BYTES {
        return Err(ConfigurationLoadError::TooLarge {
            path: configuration_path,
            observed_bytes: bytes.len() as u64,
            maximum_bytes: MAX_CONFIGURATION_BYTES,
        });
    }

    let source = std::str::from_utf8(bytes.as_slice()).map_err(|source| {
        ConfigurationLoadError::InvalidUtf8 {
            path: configuration_path.clone(),
            valid_up_to: source.valid_up_to(),
            error_len: source.error_len(),
        }
    })?;
    validate_top_level(source, &configuration_path)?;
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
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        self.command.common()
    }

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
        let requires_two_sqlite_connections = match &command {
            RpgMakerCommandArguments::Init(_) => true,
            RpgMakerCommandArguments::Extract(arguments) => arguments.lua.is_some(),
            RpgMakerCommandArguments::Translate(arguments) => arguments.lua.is_some(),
            RpgMakerCommandArguments::WriteBack(arguments) => arguments.lua.is_some(),
        };
        if requires_two_sqlite_connections && raw_common.runtime.sqlite.max_open_connections < 2 {
            return Err(ConfigurationLoadError::InvalidValue(invalid(
                "runtime.sqlite.max_open_connections",
                "Init 的数据库快照或本次所选 Lua 会话需要至少两个连接",
            )));
        }
        let supports_lua_session = raw_common.runtime.sqlite.max_open_connections >= 2;
        let common = CommonCommandConfiguration::build(configuration_directory, raw_common)
            .map_err(ConfigurationLoadError::InvalidValue)?;

        match command {
            RpgMakerCommandArguments::Init(arguments) => {
                let raw: RawInitSelection = parse_selected(source, configuration_path)?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine(),
                    raw.runtime.filesystem.publisher,
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
                let deferred_lua = DeferredLuaRuntimeConfiguration::new(
                    Arc::clone(&deferred_source),
                    supports_lua_session,
                );
                let raw: RawExtractSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration(raw.runtime.cpu)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
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
                let rpg_maker = ExtractConfiguration::build(raw.rpg_maker, builtin, rules)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
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
                let deferred_lua = DeferredLuaRuntimeConfiguration::new(
                    Arc::clone(&deferred_source),
                    supports_lua_session,
                );
                let raw: RawTranslateSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration(raw.runtime.cpu)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let llm = SelectedLlmExecutorConfiguration::build(
                    configuration_directory,
                    raw.runtime.llm,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
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
                    llm,
                    lua,
                    deferred_lua,
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
                let deferred_lua =
                    DeferredLuaRuntimeConfiguration::new(deferred_source, supports_lua_session);
                let raw: RawWriteBackSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration(raw.runtime.cpu)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine(),
                    raw.runtime.filesystem.publisher,
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
                let rpg_maker = WriteBackConfiguration::build(raw.rpg_maker)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
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

    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        match self {
            Self::Init(command) => &command.common,
            Self::Extract(command) => &command.common,
            Self::Translate(command) => &command.common,
            Self::WriteBack(command) => &command.common,
        }
    }
}

pub(crate) struct CommonCommandConfiguration {
    projects_root: PathBuf,
    async_runtime: AsyncRuntimeConfiguration,
    filesystem: SystemFileSystemConfig,
    sqlite: RusqliteStorageConfiguration,
    observability_root: PathBuf,
    project_log: ProjectLogConfig,
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
            async_runtime: AsyncRuntimeConfiguration::build(raw.runtime.async_runtime)?,
            filesystem: build_file_system_configuration(raw.runtime.filesystem)?,
            sqlite: build_sqlite_configuration(raw.runtime.sqlite)?,
            observability_root: checked_path(
                "observability.root",
                configuration_directory,
                raw.observability.root,
            )?,
            project_log: build_project_log_configuration(raw.observability.log)?,
        })
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) const fn async_runtime(&self) -> AsyncRuntimeConfiguration {
        self.async_runtime
    }

    pub(crate) const fn filesystem(&self) -> &SystemFileSystemConfig {
        &self.filesystem
    }

    pub(crate) const fn sqlite(&self) -> &RusqliteStorageConfiguration {
        &self.sqlite
    }

    pub(crate) fn observability_root(&self) -> &Path {
        &self.observability_root
    }

    pub(crate) const fn project_log(&self) -> ProjectLogConfig {
        self.project_log
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
    llm: SelectedLlmExecutorConfiguration,
    lua: Option<SelectedLuaConfiguration>,
    deferred_lua: DeferredLuaRuntimeConfiguration,
    profile: ConfiguredTranslateProfile,
}

enum ConfiguredTranslateProfile {
    Deferred {
        source: Arc<DeferredConfigurationSource>,
        configuration: PendingTranslateConfiguration,
    },
    Resolved(TranslateConfiguration),
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

    pub(crate) const fn llm(&self) -> &SelectedLlmExecutorConfiguration {
        &self.llm
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
            llm,
            lua,
            deferred_lua,
            profile,
        } = self;
        let profile = match profile {
            ConfiguredTranslateProfile::Deferred {
                source,
                configuration,
            } => ConfiguredTranslateProfile::Resolved(configuration.resolve(
                source.as_ref(),
                profile_id,
                llm.total_capacity(),
            )?),
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
            llm,
            lua,
            deferred_lua,
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

#[derive(Clone, Copy)]
pub(crate) struct AsyncRuntimeConfiguration {
    worker_threads: NonZeroUsize,
    max_blocking_threads: NonZeroUsize,
    blocking_thread_keep_alive: Duration,
}

impl AsyncRuntimeConfiguration {
    fn build(raw: RawAsyncRuntimeConfiguration) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            worker_threads: non_zero_usize("runtime.async.worker_threads", raw.worker_threads)?,
            max_blocking_threads: non_zero_usize(
                "runtime.async.max_blocking_threads",
                raw.max_blocking_threads,
            )?,
            blocking_thread_keep_alive: positive_duration(
                "runtime.async.blocking_thread_keep_alive_ms",
                raw.blocking_thread_keep_alive_ms,
            )?,
        })
    }

    pub(crate) const fn worker_threads(self) -> NonZeroUsize {
        self.worker_threads
    }

    pub(crate) const fn max_blocking_threads(self) -> NonZeroUsize {
        self.max_blocking_threads
    }

    pub(crate) const fn blocking_thread_keep_alive(self) -> Duration {
        self.blocking_thread_keep_alive
    }
}

fn build_cpu_configuration(
    raw: RawCpuRuntimeConfiguration,
) -> Result<CpuExecutorConfig, ConfigurationValueError> {
    let worker_threads = match raw.worker_threads {
        RawCpuWorkerThreads::Auto(value) if value == "auto" => CpuWorkerThreads::Auto,
        RawCpuWorkerThreads::Auto(_) => {
            return Err(invalid(
                "runtime.cpu.worker_threads",
                "字符串只接受精确小写 auto",
            ));
        }
        RawCpuWorkerThreads::Fixed(value) => {
            CpuWorkerThreads::Fixed(non_zero_usize("runtime.cpu.worker_threads", value)?)
        }
    };
    CpuExecutorConfig::new(
        worker_threads,
        usize_value("runtime.cpu.queue_capacity", raw.queue_capacity)?,
    )
    .map_err(|source| invalid("runtime.cpu", source.to_string()))
}

fn build_file_system_configuration(
    raw: RawCommonFilesystemRuntimeConfiguration,
) -> Result<SystemFileSystemConfig, ConfigurationValueError> {
    let tree = TreeBudget::new(
        usize_value("runtime.filesystem.tree.max_entries", raw.tree.max_entries)?,
        usize_value("runtime.filesystem.tree.max_depth", raw.tree.max_depth)?,
        raw.tree.max_bytes,
        raw.tree.max_single_file_bytes,
    )
    .map_err(|source| invalid("runtime.filesystem.tree", source.to_string()))?;
    let project_lease = ExclusiveFileLeaseConfig::new(positive_duration(
        "runtime.filesystem.project_lock.timeout_ms",
        raw.project_lock.timeout_ms,
    )?)
    .map_err(|source| invalid("runtime.filesystem.project_lock", source.to_string()))?;

    SystemFileSystemConfig::new(
        usize_value("runtime.filesystem.worker_threads", raw.worker_threads)?,
        usize_value("runtime.filesystem.queue_capacity", raw.queue_capacity)?,
        raw.max_read_bytes,
        usize_value(
            "runtime.filesystem.max_directory_entries",
            raw.max_directory_entries,
        )?,
        tree,
        project_lease,
    )
    .map_err(|source| invalid("runtime.filesystem", source.to_string()))
}

fn build_directory_publisher_configuration(
    projects_root: &Path,
    engine: RpgMakerEngine,
    raw: RawDirectoryPublisherConfiguration,
) -> Result<DirectoryPublisherConfig, ConfigurationValueError> {
    DirectoryPublisherConfig::new(
        projects_root
            .join(".att-locks")
            .join("directory-publish")
            .join(engine.storage_name()),
        usize_value(
            "runtime.filesystem.publisher.max_recovery_artifacts_per_target",
            raw.max_recovery_artifacts_per_target,
        )?,
        positive_duration(
            "runtime.filesystem.publisher.target_lock_timeout_ms",
            raw.target_lock_timeout_ms,
        )?,
    )
    .map_err(|source| invalid("runtime.filesystem.publisher", source.to_string()))
}

fn build_sqlite_configuration(
    raw: RawSqliteRuntimeConfiguration,
) -> Result<RusqliteStorageConfiguration, ConfigurationValueError> {
    let journal_mode = match raw.journal_mode {
        RawSqliteJournalMode::Delete => RuntimeSqliteJournalMode::Delete,
        RawSqliteJournalMode::Truncate => RuntimeSqliteJournalMode::Truncate,
        RawSqliteJournalMode::Persist => RuntimeSqliteJournalMode::Persist,
        RawSqliteJournalMode::Wal => RuntimeSqliteJournalMode::Wal,
    };
    let synchronous = match raw.synchronous {
        RawSqliteSynchronous::Normal => RuntimeSqliteSynchronous::Normal,
        RawSqliteSynchronous::Full => RuntimeSqliteSynchronous::Full,
        RawSqliteSynchronous::Extra => RuntimeSqliteSynchronous::Extra,
    };

    RusqliteStorageConfiguration::new(
        non_zero_usize(
            "runtime.sqlite.short_worker_threads",
            raw.short_worker_threads,
        )?,
        non_zero_usize(
            "runtime.sqlite.short_queue_capacity",
            raw.short_queue_capacity,
        )?,
        non_zero_usize(
            "runtime.sqlite.max_open_connections",
            raw.max_open_connections,
        )?,
        non_zero_usize("runtime.sqlite.worker_stack_bytes", raw.worker_stack_bytes)?,
        non_zero_usize(
            "runtime.sqlite.max_statement_bytes",
            raw.max_statement_bytes,
        )?,
        non_zero_usize(
            "runtime.sqlite.max_parameter_bytes",
            raw.max_parameter_bytes,
        )?,
        non_zero_usize("runtime.sqlite.max_rows_per_query", raw.max_rows_per_query)?,
        non_zero_usize(
            "runtime.sqlite.max_result_bytes_per_query",
            raw.max_result_bytes_per_query,
        )?,
        positive_duration("runtime.sqlite.busy_timeout_ms", raw.busy_timeout_ms)?,
        journal_mode,
        synchronous,
    )
    .map_err(|source| invalid("runtime.sqlite", source.to_string()))
}

fn build_project_log_configuration(
    raw: RawProjectLogConfiguration,
) -> Result<ProjectLogConfig, ConfigurationValueError> {
    let level = match raw.level {
        RawProjectLogLevel::Error => ProjectLogLevel::Error,
        RawProjectLogLevel::Warn => ProjectLogLevel::Warn,
        RawProjectLogLevel::Info => ProjectLogLevel::Info,
        RawProjectLogLevel::Debug => ProjectLogLevel::Debug,
    };
    ProjectLogConfig::try_from(ProjectLogConfigInput {
        level,
        queue_capacity: usize_value("observability.log.queue_capacity", raw.queue_capacity)?,
        batch_max_records: usize_value(
            "observability.log.batch_max_records",
            raw.batch_max_records,
        )?,
        batch_max_bytes: usize_value("observability.log.batch_max_bytes", raw.batch_max_bytes)?,
        flush_interval: positive_duration(
            "observability.log.flush_interval_ms",
            raw.flush_interval_ms,
        )?,
        shutdown_timeout: positive_duration(
            "observability.log.shutdown_timeout_ms",
            raw.shutdown_timeout_ms,
        )?,
        lock_timeout: positive_duration("observability.log.lock_timeout_ms", raw.lock_timeout_ms)?,
        max_record_bytes: usize_value("observability.log.max_record_bytes", raw.max_record_bytes)?,
        max_file_bytes: raw.max_file_bytes,
        retained_rotated_files: usize_value(
            "observability.log.retained_rotated_files",
            raw.retained_rotated_files,
        )?,
    })
    .map_err(|source| invalid("observability.log", source.to_string()))
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
    has_sqlite_session_capacity: bool,
}

impl DeferredLuaRuntimeConfiguration {
    fn new(source: Arc<DeferredConfigurationSource>, has_sqlite_session_capacity: bool) -> Self {
        Self {
            source,
            has_sqlite_session_capacity,
        }
    }

    fn resolve(&self) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
        if !self.has_sqlite_session_capacity {
            return Err(ConfigurationLoadError::InvalidValueAtPath {
                path: self.source.path().to_path_buf(),
                source: invalid(
                    "runtime.sqlite.max_open_connections",
                    "项目状态所选 Lua 会话与命令短操作共享连接预算，必须拥有第二个连接",
                ),
            });
        }
        parse_lua_configuration(self.source.source(), self.source.path())
            .map_err(|error| error.with_configuration_path(self.source.path()))
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
    total_capacity: NonZeroUsize,
    additional_pem_files: Vec<PathBuf>,
}

impl SelectedLlmExecutorConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawLlmRuntimeConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        let proxy = match raw.proxy {
            RawProxyConfiguration::Disabled(false) => LlmProxyConfiguration::Disabled,
            RawProxyConfiguration::Disabled(true) => {
                return Err(invalid(
                    "runtime.llm.proxy",
                    "代理只能用 false 关闭，或提供完整 URL",
                ));
            }
            RawProxyConfiguration::Url(value) => {
                let url = Url::parse(&value)
                    .map_err(|_| invalid("runtime.llm.proxy", "代理 URL 无效"))?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(invalid(
                        "runtime.llm.proxy",
                        "代理 URL 只接受 http 或 https",
                    ));
                }
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(invalid("runtime.llm.proxy", "代理 URL 不得内嵌凭据"));
                }
                LlmProxyConfiguration::Explicit(url)
            }
        };

        let mut additional_pem_files = Vec::with_capacity(raw.tls.additional_pem_files.len());
        let mut seen_pem_files = BTreeSet::new();
        for (index, path) in raw.tls.additional_pem_files.into_iter().enumerate() {
            let field = format!("runtime.llm.tls.additional_pem_files[{index}]");
            let path = checked_path(&field, configuration_directory, path)?;
            if !seen_pem_files.insert(path.clone()) {
                return Err(invalid(&field, "PEM 路径重复"));
            }
            additional_pem_files.push(path);
        }

        let max_active_requests =
            non_zero_usize("runtime.llm.max_active_requests", raw.max_active_requests)?;
        let queue_capacity = usize_value("runtime.llm.queue_capacity", raw.queue_capacity)?;
        let total_capacity = max_active_requests
            .get()
            .checked_add(queue_capacity)
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| invalid("runtime.llm.queue_capacity", "活动与排队总容量溢出"))?;
        if total_capacity.get() > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(invalid(
                "runtime.llm.queue_capacity",
                format!(
                    "活动与排队总容量超过调度器支持上限 {}",
                    tokio::sync::Semaphore::MAX_PERMITS
                ),
            ));
        }
        let runtime = OpenAiExecutorConfiguration::new(
            max_active_requests,
            total_capacity,
            positive_duration("runtime.llm.admission_timeout_ms", raw.admission_timeout_ms)?,
            positive_duration("runtime.llm.connect_timeout_ms", raw.connect_timeout_ms)?,
            positive_duration("runtime.llm.read_timeout_ms", raw.read_timeout_ms)?,
            positive_duration("runtime.llm.pool_idle_timeout_ms", raw.pool_idle_timeout_ms)?,
            usize_value(
                "runtime.llm.pool_max_idle_per_host",
                raw.pool_max_idle_per_host,
            )?,
            proxy,
        );
        Ok(Self {
            runtime,
            total_capacity,
            additional_pem_files,
        })
    }

    pub(crate) fn additional_pem_files(&self) -> &[PathBuf] {
        &self.additional_pem_files
    }

    pub(crate) const fn total_capacity(&self) -> NonZeroUsize {
        self.total_capacity
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
    extract_store: RpgMakerExtractionAssetStoreConfig,
}

impl ExtractConfiguration {
    fn build(
        raw: RawExtractRpgMakerSelection,
        select_builtin: bool,
        rules_path: Option<PathBuf>,
    ) -> Result<Self, ConfigurationValueError> {
        let rules = rules_path.map(|rules_path| SelectedRulesConfiguration { rules_path });
        Ok(Self {
            document: build_document_configuration(raw.document)?,
            builtin: select_builtin,
            rules,
            extract_store: build_extraction_store_configuration(raw.extract.store)?,
        })
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

    pub(crate) const fn extract_store(&self) -> RpgMakerExtractionAssetStoreConfig {
        self.extract_store
    }
}

pub(crate) struct TranslateConfiguration {
    standard_asset: RpgMakerStandardAssetReadingConfig,
    translate_store: RpgMakerStandardTranslationResultStorageConfig,
    prompt_root: PathBuf,
    language_modules: LanguageModuleCatalog,
    profile: TranslationProfileConfiguration,
    client: Arc<OpenAiChatCompletionClient>,
}

struct PendingTranslateConfiguration {
    standard_asset: RpgMakerStandardAssetReadingConfig,
    translate_store: RpgMakerStandardTranslationResultStorageConfig,
    prompt_root: PathBuf,
    language_modules: LanguageModuleCatalog,
}

impl PendingTranslateConfiguration {
    fn build(
        configuration_directory: &Path,
        raw_prompts: RawPromptsConfiguration,
        raw_languages: Vec<RawLanguageConfiguration>,
        raw: RawTranslateRpgMakerSelection,
    ) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            standard_asset: build_standard_asset_configuration(raw.standard_asset)?,
            translate_store: build_translation_store_configuration(raw.translate.store)?,
            prompt_root: checked_path("prompts.root", configuration_directory, raw_prompts.root)?,
            language_modules: build_language_modules(raw_languages)?,
        })
    }

    fn resolve(
        self,
        source: &DeferredConfigurationSource,
        profile_id: &str,
        llm_capacity: NonZeroUsize,
    ) -> Result<TranslateConfiguration, ConfigurationLoadError> {
        let selected_profile =
            parse_selected_translation_profile(source.source(), source.path(), profile_id)?;
        let llm_client_id = selected_profile.llm_client.clone();
        let raw_client = parse_selected_llm_client(source.source(), source.path(), &llm_client_id)?;
        let client = Arc::new(
            build_llm_client(format!("llm.clients.{llm_client_id}").as_str(), raw_client)
                .map_err(ConfigurationLoadError::InvalidValue)
                .map_err(|error| error.with_configuration_path(source.path()))?,
        );
        let profile =
            build_selected_translation_profile("rpg_maker.translation_profiles", selected_profile)
                .map_err(ConfigurationLoadError::InvalidValue)
                .map_err(|error| error.with_configuration_path(source.path()))?;
        if profile.max_in_flight_tasks() > llm_capacity {
            return Err(ConfigurationLoadError::InvalidValueAtPath {
                path: source.path().to_path_buf(),
                source: invalid(
                    "rpg_maker.translation_profiles.max_in_flight_tasks",
                    format!(
                        "任务并发数 {} 超过 runtime.llm 的活动与排队总容量 {}",
                        profile.max_in_flight_tasks(),
                        llm_capacity
                    ),
                ),
            });
        }
        if profile.max_in_flight_tasks().get() > tokio::sync::Semaphore::MAX_PERMITS / 2 {
            return Err(ConfigurationLoadError::InvalidValueAtPath {
                path: source.path().to_path_buf(),
                source: invalid(
                    "rpg_maker.translation_profiles.max_in_flight_tasks",
                    format!(
                        "任务并发数超过顺序最终化窗口支持上限 {}",
                        tokio::sync::Semaphore::MAX_PERMITS / 2
                    ),
                ),
            });
        }
        Ok(TranslateConfiguration {
            standard_asset: self.standard_asset,
            translate_store: self.translate_store,
            prompt_root: self.prompt_root,
            language_modules: self.language_modules,
            profile,
            client,
        })
    }
}

impl TranslateConfiguration {
    pub(crate) const fn standard_asset(&self) -> RpgMakerStandardAssetReadingConfig {
        self.standard_asset
    }

    pub(crate) const fn translate_store(&self) -> RpgMakerStandardTranslationResultStorageConfig {
        self.translate_store
    }

    pub(crate) const fn profile(&self) -> &TranslationProfileConfiguration {
        &self.profile
    }

    pub(crate) fn prompt_root(&self) -> &Path {
        &self.prompt_root
    }

    pub(crate) const fn language_modules(&self) -> &LanguageModuleCatalog {
        &self.language_modules
    }

    pub(crate) const fn client(&self) -> &Arc<OpenAiChatCompletionClient> {
        &self.client
    }
}

pub(crate) struct WriteBackConfiguration {
    document: RpgMakerDocumentReadingConfig,
    standard_asset: RpgMakerStandardAssetReadingConfig,
}

impl WriteBackConfiguration {
    fn build(raw: RawWriteBackRpgMakerSelection) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            document: build_document_configuration(raw.document)?,
            standard_asset: build_standard_asset_configuration(raw.standard_asset)?,
        })
    }

    pub(crate) const fn document(&self) -> RpgMakerDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn standard_asset(&self) -> RpgMakerStandardAssetReadingConfig {
        self.standard_asset
    }
}

fn build_document_configuration(
    raw: RawRpgMakerDocumentConfiguration,
) -> Result<RpgMakerDocumentReadingConfig, ConfigurationValueError> {
    Ok(RpgMakerDocumentReadingConfig::new(non_zero_usize(
        "rpg_maker.document.read_concurrency",
        raw.read_concurrency,
    )?))
}

fn build_standard_asset_configuration(
    raw: RawRpgMakerStandardAssetConfiguration,
) -> Result<RpgMakerStandardAssetReadingConfig, ConfigurationValueError> {
    Ok(RpgMakerStandardAssetReadingConfig::new(non_zero_usize(
        "rpg_maker.standard_asset.units_per_decode_job",
        raw.units_per_decode_job,
    )?))
}

fn build_extraction_store_configuration(
    raw: RawRpgMakerExtractStoreConfiguration,
) -> Result<RpgMakerExtractionAssetStoreConfig, ConfigurationValueError> {
    Ok(RpgMakerExtractionAssetStoreConfig::new(non_zero_usize(
        "rpg_maker.extract.store.groups_per_encode_job",
        raw.groups_per_encode_job,
    )?))
}

fn build_translation_store_configuration(
    raw: RawRpgMakerTranslateStoreConfiguration,
) -> Result<RpgMakerStandardTranslationResultStorageConfig, ConfigurationValueError> {
    Ok(RpgMakerStandardTranslationResultStorageConfig::new(
        non_zero_usize(
            "rpg_maker.translate.store.units_per_encode_job",
            raw.units_per_encode_job,
        )?,
    ))
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationPlanningConfiguration {
    max_message_characters: NonZeroUsize,
}

impl TranslationPlanningConfiguration {
    pub(crate) const fn max_message_characters(&self) -> NonZeroUsize {
        self.max_message_characters
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationProfileConfiguration {
    id: String,
    max_in_flight_tasks: NonZeroUsize,
    planning: TranslationPlanningConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
}

impl TranslationProfileConfiguration {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn max_in_flight_tasks(&self) -> NonZeroUsize {
        self.max_in_flight_tasks
    }

    pub(crate) const fn planning(&self) -> &TranslationPlanningConfiguration {
        &self.planning
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
                .map_err(|source| invalid(field.as_str(), source.to_string()))?;
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
                    .map_err(|source| invalid(field.as_str(), source.to_string()))?;
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
                .map_err(|source| invalid(field.as_str(), source.to_string()))?;
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
                .map_err(|source| invalid(field.as_str(), source.to_string()))?;
                (
                    id,
                    Arc::new(EnglishLanguageModule::new(detection, residual)),
                )
            }
        };
        let id = LanguageId::parse(&id)
            .map_err(|source| invalid(format!("{field}.id").as_str(), source.to_string()))?;
        if !language_ids.insert(id.as_str().to_owned()) {
            return Err(invalid(field.as_str(), format!("源语言 ID 重复：{id}")));
        }
        bindings.push((id, module));
    }

    let catalog = LanguageModuleCatalog::new(bindings)
        .map_err(|source| invalid("languages", source.to_string()))?;
    Ok(catalog)
}

fn build_selected_translation_profile(
    field: &str,
    raw: RawSelectedTranslationProfileConfiguration,
) -> Result<TranslationProfileConfiguration, ConfigurationValueError> {
    validate_exact_identifier(format!("{field}.id").as_str(), &raw.id)?;
    validate_exact_identifier(format!("{field}.llm_client").as_str(), &raw.llm_client)?;
    Ok(TranslationProfileConfiguration {
        id: raw.id,
        max_in_flight_tasks: non_zero_usize(
            format!("{field}.max_in_flight_tasks").as_str(),
            raw.max_in_flight_tasks,
        )?,
        planning: TranslationPlanningConfiguration {
            max_message_characters: non_zero_usize(
                format!("{field}.planning.max_message_characters").as_str(),
                raw.planning.max_message_characters,
            )?,
        },
        request: RpgMakerTranslationRequestConfiguration::new(
            raw.execution
                .network_retry_delays_ms
                .into_iter()
                .map(Duration::from_millis)
                .collect(),
            Duration::from_millis(raw.execution.max_network_retry_after_ms),
        ),
    })
}

fn build_llm_client(
    field: &str,
    raw: RawLlmClientConfiguration,
) -> Result<OpenAiChatCompletionClient, ConfigurationValueError> {
    let url =
        Url::parse(&raw.url).map_err(|_| invalid(format!("{field}.url").as_str(), "URL 无效"))?;
    validate_llm_url(format!("{field}.url").as_str(), &url)?;
    validate_exact_identifier(format!("{field}.model").as_str(), &raw.model)?;

    let exposed_api_key = raw.api_key.expose_secret();
    if exposed_api_key.trim().is_empty() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            "API key 不能为空白",
        ));
    }
    if exposed_api_key.trim() != exposed_api_key {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            "API key 不能包含首尾空白",
        ));
    }
    if reqwest::header::HeaderValue::from_bytes(exposed_api_key.as_bytes()).is_err() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            "API key 不能安全写入 HTTP Header",
        ));
    }

    // 任意精度数字会在 Serde 访问器内使用一个私有 map 信封传递原始十进制
    // 文本。第一遍自定义访问器只负责递归拒绝重复键；第二遍由
    // `serde_json::Value` 自身还原真正的 Number，避免把内部信封泄漏到请求正文。
    serde_json::from_str::<StrictJsonValue>(&raw.parameters).map_err(|error| {
        invalid(
            format!("{field}.parameters").as_str(),
            format!(
                "不是有效的严格 JSON（第 {} 行，第 {} 列）",
                error.line(),
                error.column()
            ),
        )
    })?;
    let parameter_value = serde_json::from_str::<JsonValue>(&raw.parameters)
        .expect("已通过同一 serde_json 语法边界的源文必须可重建为 Value");
    let JsonValue::Object(parameters) = parameter_value else {
        return Err(invalid(
            format!("{field}.parameters").as_str(),
            "必须是 JSON 对象",
        ));
    };
    for reserved in RESERVED_REQUEST_BODY_FIELDS {
        if parameters.contains_key(reserved) {
            return Err(invalid(
                format!("{field}.parameters.{reserved}").as_str(),
                "该顶层字段由请求协议固定拥有，不能通过 parameters 覆盖",
            ));
        }
    }

    Ok(OpenAiChatCompletionClient::new(
        url,
        raw.api_key,
        raw.model,
        positive_duration(format!("{field}.timeout_ms").as_str(), raw.timeout_ms)?,
        non_zero_u32(format!("{field}.rpm").as_str(), raw.rpm)?,
        non_zero_u32(format!("{field}.burst").as_str(), raw.burst)?,
        parameters,
    ))
}

fn validate_llm_url(field: &str, url: &Url) -> Result<(), ConfigurationValueError> {
    if url.username() != "" || url.password().is_some() {
        return Err(invalid(field, "URL 不得内嵌凭据"));
    }
    if url.fragment().is_some() {
        return Err(invalid(field, "URL 不得包含 fragment"));
    }
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => Err(invalid(field, "URL 只接受 http 或 https")),
    }
}

fn validate_exact_identifier(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    validate_non_blank(field, value)?;
    if value.trim() != value {
        return Err(invalid(field, "值含首尾空白"));
    }
    Ok(())
}

fn validate_non_blank(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    if value.trim().is_empty() {
        Err(invalid(field, "值不能为空白"))
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
        return Err(invalid(field, "路径不能为空"));
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

fn deserialize_secret_string<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(SecretString::from)
}

fn deserialize_zeroizing_string<'de, D>(deserializer: D) -> Result<Zeroizing<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Zeroizing::new)
}

fn non_zero_usize(field: &str, value: u64) -> Result<NonZeroUsize, ConfigurationValueError> {
    let value = usize_value(field, value)?;
    NonZeroUsize::new(value).ok_or_else(|| invalid(field, "值必须大于零"))
}

fn usize_value(field: &str, value: u64) -> Result<usize, ConfigurationValueError> {
    usize::try_from(value).map_err(|_| invalid(field, "值超出本平台 usize 范围"))
}

fn non_zero_u32(field: &str, value: u64) -> Result<NonZeroU32, ConfigurationValueError> {
    let value = u32::try_from(value).map_err(|_| invalid(field, "值超出 u32 范围"))?;
    NonZeroU32::new(value).ok_or_else(|| invalid(field, "值必须大于零"))
}

fn positive_duration(field: &str, milliseconds: u64) -> Result<Duration, ConfigurationValueError> {
    if milliseconds == 0 {
        return Err(invalid(field, "时长必须大于零"));
    }
    Ok(Duration::from_millis(milliseconds))
}

pub(super) fn invalid(field: &str, message: impl Into<String>) -> ConfigurationValueError {
    ConfigurationValueError {
        field: field.to_owned(),
        message: message.into(),
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
    TooLarge {
        path: PathBuf,
        observed_bytes: u64,
        maximum_bytes: u64,
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
            Self::TooLarge {
                path,
                observed_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "配置文件 {} 大小为 {observed_bytes} 字节，超过 {maximum_bytes} 字节上限",
                path.display()
            ),
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
            | Self::TooLarge { .. }
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
    message: String,
}

impl ConfigurationValueError {
    /// 返回配置契约中的稳定字段身份；面向用户的原因由 i18n 闭集负责呈现。
    pub(crate) fn field(&self) -> &str {
        &self.field
    }
}

impl fmt::Display for ConfigurationValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.field, self.message)
    }
}

impl Error for ConfigurationValueError {}

fn validate_top_level(source: &str, path: &Path) -> Result<(), ConfigurationLoadError> {
    let raw: RawTopLevelSyntax = parse_selected(source, path)?;
    let RawTopLevelSyntax {
        _projects,
        _runtime,
        _observability,
        _llm,
        _prompts,
        _languages,
        _rpg_maker,
    } = raw;
    let _ = (
        _projects,
        _runtime,
        _observability,
        _llm,
        _prompts,
        _languages,
        _rpg_maker,
    );
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

fn parse_lua_configuration(
    source: &str,
    path: &Path,
) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationLoadError> {
    let raw: RawLuaSelection = parse_selected(source, path)?;
    build_lua_configuration(raw.runtime.lua).map_err(ConfigurationLoadError::InvalidValue)
}

fn build_lua_configuration(
    raw: RawLuaRuntimeConfiguration,
) -> Result<TrustedLua54RuntimeConfiguration, ConfigurationValueError> {
    Ok(TrustedLua54RuntimeConfiguration::new(
        non_zero_usize("runtime.lua.worker_stack_bytes", raw.worker_stack_bytes)?,
        non_zero_usize(
            "runtime.lua.memory_limit_bytes_per_vm",
            raw.memory_limit_bytes_per_vm,
        )?,
        non_zero_u32(
            "runtime.lua.cancel_check_instruction_interval",
            raw.cancel_check_instruction_interval,
        )?,
        non_zero_usize("runtime.lua.max_error_bytes", raw.max_error_bytes)?,
        HostValueBudget::new(
            non_zero_usize(
                "runtime.lua.host_values.max_bytes",
                raw.host_values.max_bytes,
            )?,
            non_zero_usize(
                "runtime.lua.host_values.max_nodes",
                raw.host_values.max_nodes,
            )?,
            non_zero_usize(
                "runtime.lua.host_values.max_depth",
                raw.host_values.max_depth,
            )?,
        ),
    ))
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
            format!("ID 重复：{requested_id}"),
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
                "所选 Profile 结构或字段类型无效",
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
            format!("没有 ID 为 {requested_id} 的客户端"),
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
struct RawTopLevelSyntax {
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "runtime")]
    _runtime: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
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
struct RawCommonConfiguration {
    projects: RawProjectsConfiguration,
    runtime: RawCommonRuntimeConfiguration,
    observability: RawObservabilityConfiguration,
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
    runtime: RawPublisherRuntimeSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
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
    runtime: RawCpuRuntimeSelection,
    rpg_maker: RawExtractRpgMakerSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateSelection {
    runtime: RawTranslateRuntimeSelection,
    prompts: RawPromptsConfiguration,
    languages: Vec<RawLanguageConfiguration>,
    rpg_maker: RawTranslateRpgMakerSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackSelection {
    runtime: RawWriteBackRuntimeSelection,
    rpg_maker: RawWriteBackRpgMakerSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLuaSelection {
    runtime: RawLuaRuntimeSelection,
    #[serde(default, rename = "projects")]
    _projects: Option<IgnoredAny>,
    #[serde(default, rename = "observability")]
    _observability: Option<IgnoredAny>,
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
struct RawCpuRuntimeSelection {
    cpu: RawCpuRuntimeConfiguration,
    #[serde(rename = "async", default)]
    _async_runtime: Option<IgnoredAny>,
    #[serde(default, rename = "filesystem")]
    _filesystem: Option<IgnoredAny>,
    #[serde(default, rename = "sqlite")]
    _sqlite: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "lua")]
    _lua: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateRuntimeSelection {
    cpu: RawCpuRuntimeConfiguration,
    llm: RawLlmRuntimeConfiguration,
    #[serde(rename = "async", default)]
    _async_runtime: Option<IgnoredAny>,
    #[serde(default, rename = "filesystem")]
    _filesystem: Option<IgnoredAny>,
    #[serde(default, rename = "sqlite")]
    _sqlite: Option<IgnoredAny>,
    #[serde(default, rename = "lua")]
    _lua: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublisherRuntimeSelection {
    filesystem: RawPublisherFilesystemSelection,
    #[serde(rename = "async", default)]
    _async_runtime: Option<IgnoredAny>,
    #[serde(default, rename = "cpu")]
    _cpu: Option<IgnoredAny>,
    #[serde(default, rename = "sqlite")]
    _sqlite: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "lua")]
    _lua: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackRuntimeSelection {
    cpu: RawCpuRuntimeConfiguration,
    filesystem: RawPublisherFilesystemSelection,
    #[serde(rename = "async", default)]
    _async_runtime: Option<IgnoredAny>,
    #[serde(default, rename = "sqlite")]
    _sqlite: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "lua")]
    _lua: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLuaRuntimeSelection {
    lua: RawLuaRuntimeConfiguration,
    #[serde(rename = "async", default)]
    _async_runtime: Option<IgnoredAny>,
    #[serde(default, rename = "cpu")]
    _cpu: Option<IgnoredAny>,
    #[serde(default, rename = "filesystem")]
    _filesystem: Option<IgnoredAny>,
    #[serde(default, rename = "sqlite")]
    _sqlite: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublisherFilesystemSelection {
    publisher: RawDirectoryPublisherConfiguration,
    #[serde(default, rename = "worker_threads")]
    _worker_threads: Option<IgnoredAny>,
    #[serde(default, rename = "queue_capacity")]
    _queue_capacity: Option<IgnoredAny>,
    #[serde(default, rename = "max_read_bytes")]
    _max_read_bytes: Option<IgnoredAny>,
    #[serde(default, rename = "max_directory_entries")]
    _max_directory_entries: Option<IgnoredAny>,
    #[serde(default, rename = "tree")]
    _tree: Option<IgnoredAny>,
    #[serde(default, rename = "project_lock")]
    _project_lock: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractRpgMakerSelection {
    document: RawRpgMakerDocumentConfiguration,
    extract: RawSelectedRpgMakerExtractConfiguration,
    #[serde(default, rename = "standard_asset")]
    _standard_asset: Option<IgnoredAny>,
    #[serde(default, rename = "translate")]
    _translate: Option<IgnoredAny>,
    #[serde(default, rename = "translation_profiles")]
    _translation_profiles: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateRpgMakerSelection {
    standard_asset: RawRpgMakerStandardAssetConfiguration,
    translate: RawRpgMakerTranslateConfiguration,
    #[serde(rename = "translation_profiles")]
    _translation_profiles: IgnoredAny,
    #[serde(default, rename = "document")]
    _document: Option<IgnoredAny>,
    #[serde(default, rename = "extract")]
    _extract: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackRpgMakerSelection {
    document: RawRpgMakerDocumentConfiguration,
    standard_asset: RawRpgMakerStandardAssetConfiguration,
    #[serde(default, rename = "extract")]
    _extract: Option<IgnoredAny>,
    #[serde(default, rename = "translate")]
    _translate: Option<IgnoredAny>,
    #[serde(default, rename = "translation_profiles")]
    _translation_profiles: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPromptsConfiguration {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectsConfiguration {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommonRuntimeConfiguration {
    #[serde(rename = "async")]
    async_runtime: RawAsyncRuntimeConfiguration,
    filesystem: RawCommonFilesystemRuntimeConfiguration,
    sqlite: RawSqliteRuntimeConfiguration,
    #[serde(default, rename = "cpu")]
    _cpu: Option<IgnoredAny>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "lua")]
    _lua: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsyncRuntimeConfiguration {
    worker_threads: u64,
    max_blocking_threads: u64,
    blocking_thread_keep_alive_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCpuRuntimeConfiguration {
    worker_threads: RawCpuWorkerThreads,
    queue_capacity: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawCpuWorkerThreads {
    Fixed(u64),
    Auto(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommonFilesystemRuntimeConfiguration {
    worker_threads: u64,
    queue_capacity: u64,
    max_read_bytes: u64,
    max_directory_entries: u64,
    tree: RawDirectoryTreeConfiguration,
    project_lock: RawProjectLockConfiguration,
    #[serde(default, rename = "publisher")]
    _publisher: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirectoryPublisherConfiguration {
    max_recovery_artifacts_per_target: u64,
    target_lock_timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirectoryTreeConfiguration {
    max_entries: u64,
    max_depth: u64,
    max_bytes: u64,
    max_single_file_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectLockConfiguration {
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSqliteRuntimeConfiguration {
    short_worker_threads: u64,
    short_queue_capacity: u64,
    max_open_connections: u64,
    worker_stack_bytes: u64,
    max_statement_bytes: u64,
    max_parameter_bytes: u64,
    max_rows_per_query: u64,
    max_result_bytes_per_query: u64,
    busy_timeout_ms: u64,
    journal_mode: RawSqliteJournalMode,
    synchronous: RawSqliteSynchronous,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawSqliteJournalMode {
    Delete,
    Truncate,
    Persist,
    Wal,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawSqliteSynchronous {
    Normal,
    Full,
    Extra,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmRuntimeConfiguration {
    max_active_requests: u64,
    queue_capacity: u64,
    admission_timeout_ms: u64,
    connect_timeout_ms: u64,
    read_timeout_ms: u64,
    pool_idle_timeout_ms: u64,
    pool_max_idle_per_host: u64,
    proxy: RawProxyConfiguration,
    tls: RawLlmTlsConfiguration,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawProxyConfiguration {
    Disabled(bool),
    Url(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmTlsConfiguration {
    additional_pem_files: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLuaRuntimeConfiguration {
    worker_stack_bytes: u64,
    memory_limit_bytes_per_vm: u64,
    cancel_check_instruction_interval: u64,
    max_error_bytes: u64,
    host_values: RawLuaHostValueConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLuaHostValueConfiguration {
    max_bytes: u64,
    max_nodes: u64,
    max_depth: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservabilityConfiguration {
    root: PathBuf,
    log: RawProjectLogConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectLogConfiguration {
    level: RawProjectLogLevel,
    queue_capacity: u64,
    batch_max_records: u64,
    batch_max_bytes: u64,
    flush_interval_ms: u64,
    shutdown_timeout_ms: u64,
    lock_timeout_ms: u64,
    max_record_bytes: u64,
    max_file_bytes: u64,
    retained_rotated_files: u64,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawProjectLogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerDocumentConfiguration {
    read_concurrency: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerStandardAssetConfiguration {
    units_per_decode_job: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelectedRpgMakerExtractConfiguration {
    store: RawRpgMakerExtractStoreConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerExtractStoreConfiguration {
    groups_per_encode_job: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerTranslateConfiguration {
    store: RawRpgMakerTranslateStoreConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpgMakerTranslateStoreConfiguration {
    units_per_encode_job: u64,
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
    max_in_flight_tasks: u64,
    planning: RawSelectedTranslationPlanningConfiguration,
    execution: RawTranslationExecutionConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelectedTranslationPlanningConfiguration {
    max_message_characters: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslationExecutionConfiguration {
    network_retry_delays_ms: Vec<u64>,
    max_network_retry_after_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmClientConfiguration {
    url: String,
    #[serde(deserialize_with = "deserialize_secret_string")]
    api_key: SecretString,
    model: String,
    timeout_ms: u64,
    rpm: u64,
    burst: u64,
    #[serde(deserialize_with = "deserialize_zeroizing_string")]
    parameters: Zeroizing<String>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use secrecy::ExposeSecret;

    use super::*;
    use crate::application::arguments::{AttArguments, ProductCommand};

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
    fn cpu_worker_threads_accepts_auto_or_a_positive_integer() {
        let directory = TestDirectory::new();
        let auto_path = directory.write("cpu-auto.toml", include_str!("../../config.example.toml"));
        let ConfiguredRpgMakerCommand::Extract(auto) =
            load_configuration(&auto_path, extract_command(false)).expect("auto 应为合法选择")
        else {
            panic!("应建立 Extract 配置");
        };
        assert_eq!(auto.cpu().worker_threads(), CpuWorkerThreads::Auto);

        let fixed_source = include_str!("../../config.example.toml")
            .replace("worker_threads = \"auto\"", "worker_threads = 2");
        let fixed_path = directory.write("cpu-fixed.toml", &fixed_source);
        let ConfiguredRpgMakerCommand::Extract(fixed) =
            load_configuration(&fixed_path, extract_command(false)).expect("正整数应为合法选择")
        else {
            panic!("应建立 Extract 配置");
        };
        assert_eq!(
            fixed.cpu().worker_threads(),
            CpuWorkerThreads::Fixed(NonZeroUsize::new(2).expect("测试值非零"))
        );
    }

    #[test]
    fn cpu_worker_threads_rejects_invalid_choices() {
        let directory = TestDirectory::new();
        for (name, value) in [
            ("misspelled", "\"atuo\""),
            ("uppercase", "\"AUTO\""),
            ("zero", "0"),
        ] {
            let source = include_str!("../../config.example.toml").replace(
                "worker_threads = \"auto\"",
                format!("worker_threads = {value}").as_str(),
            );
            let path = directory.write(format!("cpu-{name}.toml").as_str(), &source);
            assert!(
                load_configuration(&path, extract_command(false)).is_err(),
                "无效 CPU 线程选择 {name} 必须被拒绝"
            );
        }
    }

    #[test]
    fn selected_profile_cannot_exceed_http_admission_capacity() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("max_active_requests = 8", "max_active_requests = 1")
            .replace("queue_capacity = 64", "queue_capacity = 1");
        let path = directory.write("profile-over-http-capacity.toml", &source);

        assert!(
            load_configuration(&path, translate_command(false, "primary")).is_err(),
            "Profile 并发超过 HTTP 活动与排队总容量时必须在启动前失败"
        );
    }

    #[test]
    fn http_admission_capacity_cannot_exceed_the_runtime_semaphore_limit() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("\r\n", "\n")
            .replace(
                "max_active_requests = 8\nqueue_capacity = 64",
                format!(
                    "max_active_requests = {}\nqueue_capacity = 1",
                    tokio::sync::Semaphore::MAX_PERMITS
                )
                .as_str(),
            );
        let path = directory.write("http-capacity-overflow.toml", &source);

        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("HTTP 总准入量不得在生产构造 Semaphore 时 panic"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("活动与排队总容量超过调度器支持上限")
        );
    }

    #[test]
    fn profile_concurrency_cannot_overflow_the_ordered_finalization_window() {
        let directory = TestDirectory::new();
        let task_limit = tokio::sync::Semaphore::MAX_PERMITS / 2 + 1;
        let source = include_str!("../../config.example.toml")
            .replace("\r\n", "\n")
            .replace(
                "max_active_requests = 8\nqueue_capacity = 64",
                format!("max_active_requests = {task_limit}\nqueue_capacity = 0").as_str(),
            )
            .replace(
                "max_in_flight_tasks = 4",
                format!("max_in_flight_tasks = {task_limit}").as_str(),
            );
        let path = directory.write("profile-window-overflow.toml", &source);

        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("2N 顺序最终化窗口超过 Semaphore 上限时必须在启动前失败"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("任务并发数超过顺序最终化窗口支持上限")
        );
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
        let configured = build_directory_publisher_configuration(
            &projects_root,
            RpgMakerEngine::Mz,
            RawDirectoryPublisherConfiguration {
                max_recovery_artifacts_per_target: 1,
                target_lock_timeout_ms: 1,
            },
        )
        .expect("目录发布配置应合法");
        assert_eq!(
            configured.lock_directory(),
            projects_root.join(".att-locks/directory-publish/mz")
        );

        let configured = build_directory_publisher_configuration(
            &projects_root,
            RpgMakerEngine::Mv,
            RawDirectoryPublisherConfiguration {
                max_recovery_artifacts_per_target: 1,
                target_lock_timeout_ms: 1,
            },
        )
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
            .replace("api_key = \"replace-with-api-key\"", "api_key = []")
            .replace(
                "worker_stack_bytes = 8388608",
                "worker_stack_bytes = \"invalid\"",
            )
            .replace("read_concurrency = 8", "read_concurrency = \"invalid\"");
        let path = directory.write("init.toml", &source);

        let configured = load_configuration(&path, init_command())
            .expect("Init 不应解析未选择的 LLM、Lua 或 RPG Maker 执行配置");
        assert!(matches!(configured, ConfiguredRpgMakerCommand::Init(_)));
    }

    #[test]
    fn init_allows_known_unselected_sections_to_be_absent() {
        let directory = TestDirectory::new();
        let path = directory.write("minimal-init.toml", minimal_init_configuration());

        let configured = load_configuration(&path, init_command())
            .expect("Init 不应要求未选择的 CPU、LLM、Lua 或 RPG Maker 执行配置存在");
        assert!(matches!(configured, ConfiguredRpgMakerCommand::Init(_)));
    }

    #[test]
    fn sqlite_connection_capacity_is_checked_only_for_the_selected_command() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("max_open_connections = 16", "max_open_connections = 1");
        let path = directory.write("single-sqlite-connection.toml", &source);

        assert!(
            load_configuration(&path, init_command()).is_err(),
            "Init 的数据库快照固定需要两个连接"
        );
        for command in [
            extract_command(false),
            translate_command(false, "primary"),
            write_back_command(false),
        ] {
            load_configuration(&path, command)
                .expect("没有 Lua 的命令不应为未使用的第二个 SQLite 连接失败");
        }
        for command in [
            parse_command([
                "att",
                "mz",
                "extract",
                "--name",
                "demo",
                "--rules",
                "rules.toml",
                "--lua",
                "script.lua",
            ]),
            translate_command(true, "primary"),
            write_back_command(true),
        ] {
            assert!(
                load_configuration(&path, command).is_err(),
                "显式 Lua 会话与命令短操作共享连接预算，必须拥有第二个连接"
            );
        }
    }

    #[test]
    fn extract_only_parses_lua_when_selected() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "worker_stack_bytes = 8388608",
            "worker_stack_bytes = \"invalid\"",
        );
        let path = directory.write("extract.toml", &source);

        load_configuration(&path, extract_command(false)).expect("没有 Lua 时不应解析 Lua 配置");
        assert!(
            load_configuration(
                &path,
                parse_command([
                    "att",
                    "mz",
                    "extract",
                    "--name",
                    "demo",
                    "--rules",
                    "rules.toml",
                    "--lua",
                    "script.lua",
                ]),
            )
            .is_err(),
            "显式选择 Lua 时必须严格校验 Lua 配置"
        );
    }

    #[test]
    fn project_state_lua_runtime_is_validated_only_when_consumed() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "worker_stack_bytes = 8388608",
            "worker_stack_bytes = \"invalid\"",
        );
        let path = directory.write("deferred-lua.toml", &source);

        let ConfiguredRpgMakerCommand::Extract(extract) =
            load_configuration(&path, extract_command(false))
                .expect("未复用 Lua 时 Extract 不应解析 Lua 配置")
        else {
            panic!("应建立 Extract 配置");
        };
        assert!(extract.resolve_lua_runtime().is_err());

        let ConfiguredRpgMakerCommand::Translate(translate) =
            load_configuration(&path, translate_command_without_profile())
                .expect("未复用 Lua 时 Translate 不应解析 Lua 配置")
        else {
            panic!("应建立 Translate 配置");
        };
        assert!(translate.resolve_lua_runtime().is_err());

        let ConfiguredRpgMakerCommand::WriteBack(write_back) =
            load_configuration(&path, write_back_command(false))
                .expect("未复用 Lua 时 WriteBack 不应解析 Lua 配置")
        else {
            panic!("应建立 WriteBack 配置");
        };
        assert!(write_back.resolve_lua_runtime().is_err());
    }

    #[test]
    fn project_state_lua_runtime_checks_sqlite_session_capacity_on_demand() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("max_open_connections = 16", "max_open_connections = 1");
        let path = directory.write("deferred-lua-capacity.toml", &source);
        let ConfiguredRpgMakerCommand::WriteBack(configured) =
            load_configuration(&path, write_back_command(false))
                .expect("没有实际 Lua 消费时单连接配置应合法")
        else {
            panic!("应建立 WriteBack 配置");
        };

        let error = configured
            .resolve_lua_runtime()
            .expect_err("复用项目 Lua 时必须检查第二个 SQLite 连接");
        assert!(error.to_string().contains("必须拥有第二个连接"));
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
            "max_message_characters = 24000",
            "max_message_characters = 0",
        );
        let path = directory.write("invalid-explicit-profile.toml", &source);

        assert!(
            load_configuration(&path, translate_command(false, "primary")).is_err(),
            "显式 Profile 的无效字段必须在配置加载阶段被拒绝"
        );
    }

    fn configuration_with_unselected_profile_sentinel(sentinel: &str) -> String {
        format!(
            r#"{}
[llm.clients.unused]
url = []
api_key = []
model = []
timeout_ms = []
rpm = []
burst = []
parameters = []

[[rpg_maker.translation_profiles]]
llm_client = ["{sentinel}"]
max_in_flight_tasks = {{ secret = "{sentinel}" }}
planning = ["{sentinel}"]
execution = ["{sentinel}"]
private_secret = "{sentinel}"
id = "unused"
"#,
            include_str!("../../config.example.toml")
        )
    }

    #[test]
    fn translate_streams_past_unselected_client_and_profile_without_materializing_values() {
        const SENTINEL: &str = "UNSELECTED_PROFILE_SECRET_SENTINEL";
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
    fn unselected_profile_secret_never_enters_configuration_diagnostics() {
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
    fn selected_profile_rejects_unknown_planning_fields() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "max_message_characters = 24000",
            "max_message_characters = 24000\nunexpected_field = []",
        );
        let path = directory.write("unknown-planning-field.toml", &source);
        assert!(
            load_configuration(&path, translate_command(false, "primary")).is_err(),
            "所选 Profile 的 planning 表必须严格拒绝未知字段"
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
                "profile-max-in-flight",
                source.replacen("max_in_flight_tasks = 4\n", "", 1),
            ),
            (
                "planning-max-message-characters",
                source.replacen("max_message_characters = 24000\n", "", 1),
            ),
            (
                "execution-network-retry-delays",
                source.replacen("network_retry_delays_ms = [500, 1500, 5000]\n", "", 1),
            ),
            (
                "execution-max-network-retry-after",
                source.replacen("max_network_retry_after_ms = 30000\n", "", 1),
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
                "type",
                source.replacen(
                    "max_active_requests = 8",
                    "max_active_requests = \"TYPE_VALUE_SENTINEL\"",
                    1,
                ),
                "runtime.llm.max_active_requests",
                "字段类型不符合当前配置契约",
                Some("TYPE_VALUE_SENTINEL"),
            ),
            (
                "secret-type",
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

        let invalid_project_log = include_str!("../../config.example.toml")
            .replace("queue_capacity = 1024", "queue_capacity = 0");
        let path = directory.write("project-log.toml", &invalid_project_log);
        assert!(load_configuration(&path, init_command()).is_err());
    }

    #[test]
    fn unknown_top_level_and_runtime_fields_are_rejected() {
        let directory = TestDirectory::new();
        for (name, source) in [
            (
                "top.toml",
                format!(
                    "{}\n[unknown]\nvalue = 1\n",
                    include_str!("../../config.example.toml")
                ),
            ),
            (
                "runtime.toml",
                include_str!("../../config.example.toml")
                    .replace("[runtime.cpu]", "unexpected = 1\n\n[runtime.cpu]"),
            ),
        ] {
            let path = directory.write(name, &source);
            assert!(load_configuration(&path, init_command()).is_err());
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
    fn selected_llm_client_debug_redacts_secret_and_parameters() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("replace-with-api-key", "SECRET_SENTINEL")
            .replace(
                "\"temperature\": 0.2",
                "\"private_vendor_value\": \"PRIVATE_SENTINEL\"",
            );
        let path = directory.write("secret.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command(false, "primary"))
                .expect("所选客户端应合法")
        else {
            panic!("应建立 Translate 配置");
        };
        let debug = format!("{:?}", configured.client());
        assert!(!debug.contains("SECRET_SENTINEL"));
        assert!(!debug.contains("PRIVATE_SENTINEL"));
    }

    #[test]
    fn unselected_client_secret_never_enters_configuration_diagnostics() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[llm.clients.unused]\nurl = []\napi_key = \"UNSELECTED_SECRET_SENTINEL\"\nmodel = []\ntimeout_ms = []\nrpm = []\nburst = []\nparameters = []\n",
            include_str!("../../config.example.toml")
        )
        .replace("queue_capacity = 1024", "queue_capacity = 0");
        let path = directory.write("unselected-secret.toml", &source);
        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("无效项目日志配置必须拒绝"),
            Err(error) => error,
        };
        let mut diagnostics = format!("{error:?}\n{error}");
        let mut source = error.source();
        while let Some(error) = source {
            diagnostics.push_str(format!("\n{error:?}\n{error}").as_str());
            source = error.source();
        }
        assert!(!diagnostics.contains("UNSELECTED_SECRET_SENTINEL"));
    }

    #[test]
    fn configuration_file_has_fixed_bootstrap_size_limit() {
        let directory = TestDirectory::new();
        let path = directory.path().join("large.toml");
        fs::write(&path, vec![b'x'; MAX_CONFIGURATION_BYTES as usize + 1]).expect("应写入超限配置");
        assert!(matches!(
            load_configuration(&path, init_command()),
            Err(ConfigurationLoadError::TooLarge { .. })
        ));
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

[runtime.async]
worker_threads = 1
max_blocking_threads = 1
blocking_thread_keep_alive_ms = 1

[runtime.filesystem]
worker_threads = 1
queue_capacity = 1
max_read_bytes = 1
max_directory_entries = 1

[runtime.filesystem.tree]
max_entries = 1
max_depth = 1
max_bytes = 1
max_single_file_bytes = 1

[runtime.filesystem.project_lock]
timeout_ms = 1

[runtime.filesystem.publisher]
max_recovery_artifacts_per_target = 1
target_lock_timeout_ms = 1

[runtime.sqlite]
short_worker_threads = 1
short_queue_capacity = 1
max_open_connections = 2
worker_stack_bytes = 1
max_statement_bytes = 1
max_parameter_bytes = 1
max_rows_per_query = 1
max_result_bytes_per_query = 1
busy_timeout_ms = 1
journal_mode = "delete"
synchronous = "full"

[observability]
root = "logs"

[observability.log]
level = "info"
queue_capacity = 1
batch_max_records = 1
batch_max_bytes = 1
flush_interval_ms = 1
shutdown_timeout_ms = 1
lock_timeout_ms = 1
max_record_bytes = 1
max_file_bytes = 1
retained_rotated_files = 0
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
