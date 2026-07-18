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
    ExtractArguments, InitArguments, MzCommand, TranslateArguments, WriteBackArguments,
};

use crate::att_mz::extract::builtin::BuiltInExtractionConfig;
use crate::att_mz::extract::document::MzDocumentReadingConfig;
use crate::att_mz::extract::rules::RulesExtractionConfig;
use crate::att_mz::extract::store::asset_store::MzExtractionAssetStoreConfig;
use crate::att_mz::lua::json::HostValueBudget;
use crate::att_mz::standard_asset::MzStandardAssetReadingConfig;
use crate::att_mz::translate::profile::MzTranslationRequestConfiguration;
use crate::att_mz::translate::result_store::MzStandardTranslationResultStorageConfig;
use crate::att_mz::{ENGINE_DIRECTORY_NAME, ProjectName};
use crate::language::{
    EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
    JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageId,
    LanguageModule, LanguageModuleCatalog, QuotePair,
};
use crate::runtime::cpu::CpuExecutorConfig;
use crate::runtime::filesystem::{
    DirectoryPublisherConfig, ExclusiveFileLeaseConfig, SystemFileSystemConfig, TreeBudget,
};
use crate::runtime::json_lines::JsonLinesStreamConfig;
use crate::runtime::llm::{
    LlmProxyConfiguration, OpenAiChatCompletionClient, OpenAiExecutorConfiguration,
};
use crate::runtime::lua::TrustedLua54RuntimeConfiguration;
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
pub(crate) fn load_configuration(
    requested_path: &Path,
    command: MzCommand,
) -> Result<ConfiguredMzCommand, ConfigurationLoadError> {
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
    ConfiguredMzCommand::build(
        &configuration_path,
        &configuration_directory,
        source,
        command,
    )
    .map_err(|error| error.with_configuration_path(&configuration_path))
}

/// 四个互斥命令各自拥有且只拥有现实消费的配置。
pub(crate) enum ConfiguredMzCommand {
    Init(ConfiguredInitCommand),
    Extract(ConfiguredExtractCommand),
    Translate(Box<ConfiguredTranslateCommand>),
    WriteBack(ConfiguredWriteBackCommand),
}

impl ConfiguredMzCommand {
    fn build(
        configuration_path: &Path,
        configuration_directory: &Path,
        source: &str,
        command: MzCommand,
    ) -> Result<Self, ConfigurationLoadError> {
        let raw_common: RawCommonConfiguration = parse_selected(source, configuration_path)?;
        let requires_two_sqlite_connections = match &command {
            MzCommand::Init(_) => true,
            MzCommand::Extract(arguments) => arguments.lua.is_some(),
            MzCommand::Translate(arguments) => arguments.lua.is_some(),
            MzCommand::WriteBack(arguments) => arguments.lua.is_some(),
        };
        if requires_two_sqlite_connections && raw_common.runtime.sqlite.max_open_connections < 2 {
            return Err(ConfigurationLoadError::InvalidValue(invalid(
                "runtime.sqlite.max_open_connections",
                "Init 的数据库快照或本次所选 Lua 会话需要至少两个连接",
            )));
        }
        let common = CommonCommandConfiguration::build(configuration_directory, raw_common)
            .map_err(ConfigurationLoadError::InvalidValue)?;

        match command {
            MzCommand::Init(arguments) => {
                let raw: RawInitSelection = parse_selected(source, configuration_path)?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    raw.runtime.filesystem.publisher,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                Ok(Self::Init(ConfiguredInitCommand {
                    arguments,
                    common,
                    publisher,
                }))
            }
            MzCommand::Extract(arguments) => {
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
                        parse_lua_configuration(source, configuration_path)
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                let mz = ExtractConfiguration::build(raw.mz, builtin, rules)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                Ok(Self::Extract(ConfiguredExtractCommand {
                    project_name: project.name,
                    common,
                    cpu,
                    lua,
                    mz,
                }))
            }
            MzCommand::Translate(arguments) => {
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
                        parse_lua_configuration(source, configuration_path)
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                validate_exact_identifier("命令行 PROFILE_ID", &profile_id)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let selected_profile =
                    parse_selected_translation_profile(source, configuration_path, &profile_id)?;
                let llm_client_id = selected_profile.llm_client.clone();
                let raw_client =
                    parse_selected_llm_client(source, configuration_path, &llm_client_id)?;
                let client = Arc::new(
                    build_llm_client(format!("llm.clients.{llm_client_id}").as_str(), raw_client)
                        .map_err(ConfigurationLoadError::InvalidValue)?,
                );
                let mz = TranslateConfiguration::build(
                    configuration_directory,
                    raw.prompts,
                    raw.languages,
                    raw.mz,
                    selected_profile,
                    client,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                Ok(Self::Translate(Box::new(ConfiguredTranslateCommand {
                    project_name: project.name,
                    terminology_path: terms,
                    placeholder_rules_path: placeholders,
                    common,
                    cpu,
                    llm,
                    lua,
                    mz,
                })))
            }
            MzCommand::WriteBack(arguments) => {
                let raw: RawWriteBackSelection = parse_selected(source, configuration_path)?;
                let cpu = build_cpu_configuration(raw.runtime.cpu)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    raw.runtime.filesystem.publisher,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                let WriteBackArguments { project, lua } = arguments;
                let lua = lua
                    .map(|script_path| {
                        parse_lua_configuration(source, configuration_path)
                            .map(|runtime| SelectedLuaConfiguration::new(script_path, runtime))
                    })
                    .transpose()?;
                let mz = WriteBackConfiguration::build(raw.mz)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                Ok(Self::WriteBack(ConfiguredWriteBackCommand {
                    project_name: project.name,
                    common,
                    cpu,
                    publisher,
                    lua,
                    mz,
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
    audit_root: PathBuf,
    audit: JsonLinesStreamConfig,
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
            audit_root: checked_path(
                "observability.root",
                configuration_directory,
                raw.observability.root,
            )?,
            audit: build_audit_configuration(raw.observability.audit)?,
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

    pub(crate) fn audit_root(&self) -> &Path {
        &self.audit_root
    }

    pub(crate) const fn audit(&self) -> JsonLinesStreamConfig {
        self.audit
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
    mz: ExtractConfiguration,
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

    pub(crate) const fn mz(&self) -> &ExtractConfiguration {
        &self.mz
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
    mz: TranslateConfiguration,
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

    #[cfg(test)]
    pub(crate) const fn client(&self) -> &Arc<OpenAiChatCompletionClient> {
        self.mz.client()
    }

    pub(crate) const fn mz(&self) -> &TranslateConfiguration {
        &self.mz
    }
}

pub(crate) struct ConfiguredWriteBackCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    publisher: DirectoryPublisherConfig,
    lua: Option<SelectedLuaConfiguration>,
    mz: WriteBackConfiguration,
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

    pub(crate) const fn mz(&self) -> &WriteBackConfiguration {
        &self.mz
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
    CpuExecutorConfig::new(
        usize_value("runtime.cpu.worker_threads", raw.worker_threads)?,
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
    raw: RawDirectoryPublisherConfiguration,
) -> Result<DirectoryPublisherConfig, ConfigurationValueError> {
    DirectoryPublisherConfig::new(
        projects_root
            .join(".att-locks")
            .join("directory-publish")
            .join(ENGINE_DIRECTORY_NAME),
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

fn build_audit_configuration(
    raw: RawEventLogConfiguration,
) -> Result<JsonLinesStreamConfig, ConfigurationValueError> {
    JsonLinesStreamConfig::new(
        usize_value("observability.audit.queue_capacity", raw.queue_capacity)?,
        positive_duration("observability.audit.lock_timeout_ms", raw.lock_timeout_ms)?,
        usize_value("observability.audit.max_record_bytes", raw.max_record_bytes)?,
        raw.max_file_bytes,
        usize_value(
            "observability.audit.retained_rotated_files",
            raw.retained_rotated_files,
        )?,
    )
    .map_err(|source| invalid("observability.audit", source.to_string()))
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
            additional_pem_files,
        })
    }

    pub(crate) fn additional_pem_files(&self) -> &[PathBuf] {
        &self.additional_pem_files
    }

    pub(crate) fn with_pem_roots(&self, roots: Vec<Vec<u8>>) -> OpenAiExecutorConfiguration {
        self.runtime.clone().with_additional_pem_roots(roots)
    }
}

pub(crate) struct SelectedRulesConfiguration {
    rules_path: PathBuf,
    runtime: RulesExtractionConfig,
}

impl SelectedRulesConfiguration {
    pub(crate) fn rules_path(&self) -> &Path {
        &self.rules_path
    }

    pub(crate) const fn runtime(&self) -> RulesExtractionConfig {
        self.runtime
    }
}

pub(crate) struct ExtractConfiguration {
    document: MzDocumentReadingConfig,
    builtin: Option<BuiltInExtractionConfig>,
    rules: Option<SelectedRulesConfiguration>,
    extract_store: MzExtractionAssetStoreConfig,
}

impl ExtractConfiguration {
    fn build(
        raw: RawExtractMzSelection,
        select_builtin: bool,
        rules_path: Option<PathBuf>,
    ) -> Result<Self, ConfigurationValueError> {
        let builtin = select_optional_scan_concurrency(
            "mz.extract.builtin",
            select_builtin,
            raw.extract.builtin,
        )?
        .map(BuiltInExtractionConfig::new);
        let rules = select_optional_scan_concurrency(
            "mz.extract.rules",
            rules_path.is_some(),
            raw.extract.rules,
        )?
        .zip(rules_path)
        .map(
            |(scan_concurrency, rules_path)| SelectedRulesConfiguration {
                rules_path,
                runtime: RulesExtractionConfig::new(scan_concurrency),
            },
        );
        Ok(Self {
            document: build_document_configuration(raw.document)?,
            builtin,
            rules,
            extract_store: build_extraction_store_configuration(raw.extract.store)?,
        })
    }

    pub(crate) const fn document(&self) -> MzDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn builtin(&self) -> Option<BuiltInExtractionConfig> {
        self.builtin
    }

    pub(crate) const fn rules(&self) -> Option<&SelectedRulesConfiguration> {
        self.rules.as_ref()
    }

    pub(crate) const fn extract_store(&self) -> MzExtractionAssetStoreConfig {
        self.extract_store
    }
}

pub(crate) struct TranslateConfiguration {
    standard_asset: MzStandardAssetReadingConfig,
    translate_store: MzStandardTranslationResultStorageConfig,
    prompt_root: PathBuf,
    language_modules: LanguageModuleCatalog,
    profile: TranslationProfileConfiguration,
    client: Arc<OpenAiChatCompletionClient>,
}

impl TranslateConfiguration {
    fn build(
        configuration_directory: &Path,
        raw_prompts: RawPromptsConfiguration,
        raw_languages: Vec<RawLanguageConfiguration>,
        raw: RawTranslateMzSelection,
        selected_profile: RawSelectedTranslationProfileConfiguration,
        client: Arc<OpenAiChatCompletionClient>,
    ) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            standard_asset: build_standard_asset_configuration(raw.standard_asset)?,
            translate_store: build_translation_store_configuration(raw.translate.store)?,
            prompt_root: checked_path("prompts.root", configuration_directory, raw_prompts.root)?,
            language_modules: build_language_modules(raw_languages)?,
            profile: build_selected_translation_profile(
                "mz.translation_profiles",
                selected_profile,
            )?,
            client,
        })
    }

    pub(crate) const fn standard_asset(&self) -> MzStandardAssetReadingConfig {
        self.standard_asset
    }

    pub(crate) const fn translate_store(&self) -> MzStandardTranslationResultStorageConfig {
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
    document: MzDocumentReadingConfig,
    standard_asset: MzStandardAssetReadingConfig,
}

impl WriteBackConfiguration {
    fn build(raw: RawWriteBackMzSelection) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            document: build_document_configuration(raw.document)?,
            standard_asset: build_standard_asset_configuration(raw.standard_asset)?,
        })
    }

    pub(crate) const fn document(&self) -> MzDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn standard_asset(&self) -> MzStandardAssetReadingConfig {
        self.standard_asset
    }
}

fn build_document_configuration(
    raw: RawMzDocumentConfiguration,
) -> Result<MzDocumentReadingConfig, ConfigurationValueError> {
    Ok(MzDocumentReadingConfig::new(
        non_zero_usize("mz.document.read_concurrency", raw.read_concurrency)?,
        non_zero_usize("mz.document.parse_concurrency", raw.parse_concurrency)?,
    ))
}

fn build_standard_asset_configuration(
    raw: RawMzStandardAssetConfiguration,
) -> Result<MzStandardAssetReadingConfig, ConfigurationValueError> {
    Ok(MzStandardAssetReadingConfig::new(
        non_zero_usize(
            "mz.standard_asset.decode_concurrency",
            raw.decode_concurrency,
        )?,
        non_zero_usize(
            "mz.standard_asset.leaves_per_decode_job",
            raw.leaves_per_decode_job,
        )?,
    ))
}

fn build_extraction_store_configuration(
    raw: RawMzExtractStoreConfiguration,
) -> Result<MzExtractionAssetStoreConfig, ConfigurationValueError> {
    Ok(MzExtractionAssetStoreConfig::new(
        non_zero_usize(
            "mz.extract.store.encode_concurrency",
            raw.encode_concurrency,
        )?,
        non_zero_usize(
            "mz.extract.store.groups_per_encode_job",
            raw.groups_per_encode_job,
        )?,
    ))
}

fn build_translation_store_configuration(
    raw: RawMzTranslateStoreConfiguration,
) -> Result<MzStandardTranslationResultStorageConfig, ConfigurationValueError> {
    Ok(MzStandardTranslationResultStorageConfig::new(
        non_zero_usize(
            "mz.translate.store.encode_concurrency",
            raw.encode_concurrency,
        )?,
        non_zero_usize(
            "mz.translate.store.leaves_per_encode_job",
            raw.leaves_per_encode_job,
        )?,
    ))
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationPlanningConfiguration {
    scope_concurrency: NonZeroUsize,
    max_message_characters: NonZeroUsize,
}

impl TranslationPlanningConfiguration {
    pub(crate) const fn scope_concurrency(&self) -> NonZeroUsize {
        self.scope_concurrency
    }

    pub(crate) const fn max_message_characters(&self) -> NonZeroUsize {
        self.max_message_characters
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationProfileConfiguration {
    id: String,
    max_in_flight_tasks: NonZeroUsize,
    planning: TranslationPlanningConfiguration,
    request: MzTranslationRequestConfiguration,
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

    pub(crate) const fn request(&self) -> &MzTranslationRequestConfiguration {
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
            scope_concurrency: non_zero_usize(
                format!("{field}.planning.scope_concurrency").as_str(),
                raw.planning.scope_concurrency,
            )?,
            max_message_characters: non_zero_usize(
                format!("{field}.planning.max_message_characters").as_str(),
                raw.planning.max_message_characters,
            )?,
        },
        request: MzTranslationRequestConfiguration::new(
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

fn invalid(field: &str, message: impl Into<String>) -> ConfigurationValueError {
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
            | Self::InvalidToml { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceLocation {
    line: usize,
    column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationValueError {
    field: String,
    message: String,
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
        _mz,
    } = raw;
    let _ = (
        _projects,
        _runtime,
        _observability,
        _llm,
        _prompts,
        _languages,
        _mz,
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

fn select_optional_scan_concurrency(
    field: &str,
    selected: bool,
    raw: Option<toml::Value>,
) -> Result<Option<NonZeroUsize>, ConfigurationValueError> {
    if !selected {
        return Ok(None);
    }
    let raw = raw.ok_or_else(|| invalid(field, "本次选择需要该配置"))?;
    let raw: RawMzScanConfiguration = raw
        .try_into()
        .map_err(|_| invalid(field, "结构或字段类型无效"))?;
    Ok(Some(non_zero_usize(
        format!("{field}.scan_concurrency").as_str(),
        raw.scan_concurrency,
    )?))
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
            "mz.translation_profiles",
            format!("ID 重复：{requested_id}"),
        )));
    }
    let selected_index = selection.selected_index.ok_or_else(|| {
        ConfigurationLoadError::InvalidValue(invalid(
            "mz.translation_profiles",
            format!("没有 ID 为 {requested_id} 的条目"),
        ))
    })?;

    let profile_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, &error))?;
    SelectedTranslationProfileTopSeed { selected_index }
        .deserialize(profile_deserializer)
        .map_err(|error| invalid_toml(path, source, &error))?
        .ok_or_else(|| {
            ConfigurationLoadError::InvalidValue(invalid(
                "mz.translation_profiles",
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
        let mut seen_mz = false;
        let mut selection = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "mz" {
                if seen_mz {
                    return Err(de::Error::duplicate_field("mz"));
                }
                seen_mz = true;
                selection = map.next_value_seed(TranslationProfileIndexMzSeed {
                    requested_id: self.requested_id,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selection)
    }
}

struct TranslationProfileIndexMzSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexMzSeed<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIndexMzVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexMzVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexMzVisitor<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MZ 配置")
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
        formatter.write_str("MZ translation profile 数组")
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
        formatter.write_str("只读取 id 的 MZ translation profile")
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
        let mut seen_mz = false;
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "mz" {
                if seen_mz {
                    return Err(de::Error::duplicate_field("mz"));
                }
                seen_mz = true;
                selected = map.next_value_seed(SelectedTranslationProfileMzSeed {
                    selected_index: self.selected_index,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileMzSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileMzSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedTranslationProfileMzVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileMzVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileMzVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MZ 配置")
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
        formatter.write_str("MZ translation profile 数组")
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
    #[serde(default, rename = "mz")]
    _mz: Option<IgnoredAny>,
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
    #[serde(default, rename = "mz")]
    _mz: Option<IgnoredAny>,
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
    #[serde(default, rename = "mz")]
    _mz: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractSelection {
    runtime: RawCpuRuntimeSelection,
    mz: RawExtractMzSelection,
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
    mz: RawTranslateMzSelection,
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
    mz: RawWriteBackMzSelection,
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
    #[serde(default, rename = "mz")]
    _mz: Option<IgnoredAny>,
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
struct RawExtractMzSelection {
    document: RawMzDocumentConfiguration,
    extract: RawSelectedMzExtractConfiguration,
    #[serde(default, rename = "standard_asset")]
    _standard_asset: Option<IgnoredAny>,
    #[serde(default, rename = "translate")]
    _translate: Option<IgnoredAny>,
    #[serde(default, rename = "translation_profiles")]
    _translation_profiles: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateMzSelection {
    standard_asset: RawMzStandardAssetConfiguration,
    translate: RawMzTranslateConfiguration,
    #[serde(rename = "translation_profiles")]
    _translation_profiles: IgnoredAny,
    #[serde(default, rename = "document")]
    _document: Option<IgnoredAny>,
    #[serde(default, rename = "extract")]
    _extract: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackMzSelection {
    document: RawMzDocumentConfiguration,
    standard_asset: RawMzStandardAssetConfiguration,
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
    worker_threads: u64,
    queue_capacity: u64,
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
    audit: RawEventLogConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventLogConfiguration {
    queue_capacity: u64,
    lock_timeout_ms: u64,
    max_record_bytes: u64,
    max_file_bytes: u64,
    retained_rotated_files: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzDocumentConfiguration {
    read_concurrency: u64,
    parse_concurrency: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzStandardAssetConfiguration {
    decode_concurrency: u64,
    leaves_per_decode_job: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelectedMzExtractConfiguration {
    #[serde(default)]
    builtin: Option<toml::Value>,
    #[serde(default)]
    rules: Option<toml::Value>,
    store: RawMzExtractStoreConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzScanConfiguration {
    scan_concurrency: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzExtractStoreConfiguration {
    encode_concurrency: u64,
    groups_per_encode_job: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzTranslateConfiguration {
    store: RawMzTranslateStoreConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMzTranslateStoreConfiguration {
    encode_concurrency: u64,
    leaves_per_encode_job: u64,
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
    scope_concurrency: u64,
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

        let configured =
            load_configuration(&path, init_command()).expect("Init 不应解析 LLM、Lua 或 MZ 配置");
        assert!(matches!(configured, ConfiguredMzCommand::Init(_)));
    }

    #[test]
    fn init_allows_known_unselected_sections_to_be_absent() {
        let directory = TestDirectory::new();
        let path = directory.write("minimal-init.toml", minimal_init_configuration());

        let configured = load_configuration(&path, init_command())
            .expect("Init 不应要求 CPU、LLM、Lua 或 MZ 配置存在");
        assert!(matches!(configured, ConfiguredMzCommand::Init(_)));
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
                "rules.json",
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
    fn extract_only_parses_selected_rule_kinds_and_lua() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replacen("scan_concurrency = 4", "scan_concurrency = \"invalid\"", 1)
            .replace(
                "worker_stack_bytes = 8388608",
                "worker_stack_bytes = \"invalid\"",
            );
        let path = directory.write("extract.toml", &source);

        load_configuration(&path, extract_command(false))
            .expect("仅 Rules 且没有 Lua 时不应解析 Builtin 或 Lua");
        assert!(load_configuration(&path, extract_command(true)).is_err());
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
                    "rules.json",
                    "--lua",
                    "script.lua",
                ]),
            )
            .is_err(),
            "显式选择 Lua 时必须严格校验 Lua 配置"
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

[[mz.translation_profiles]]
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
        let ConfiguredMzCommand::Translate(configured) =
            load_configuration(&path, translate_command(false, "primary"))
                .expect("无关客户端和 Profile 不应阻止本次翻译")
        else {
            panic!("应建立 Translate 配置");
        };
        configured
            .mz()
            .language_modules()
            .resolve(&LanguageId::parse("ja").expect("测试语言应合法"))
            .expect("应建立日语模块");
        configured
            .mz()
            .language_modules()
            .resolve(&LanguageId::parse("en").expect("测试语言应合法"))
            .expect("应建立英语模块");
        assert_eq!(configured.client().model(), "replace-with-model-id");
        assert_eq!(
            configured.client().api_key().expose_secret(),
            "replace-with-api-key"
        );
        let profile_debug = format!("{:?}", configured.mz().profile());
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
        let ConfiguredMzCommand::Translate(configured) =
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
        assert_eq!(configured.mz().prompt_root(), expected);
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
                remove_configuration_range(&source, "[[languages]]", "[[mz.translation_profiles]]"),
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
                "planning-scope-concurrency",
                source.replacen("scope_concurrency = 4\n", "", 1),
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
                "mz.translation_profiles.llm_client",
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
            "{}\n[[mz.translation_profiles]]\nid = \"primary\"\n",
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

        let invalid_audit = include_str!("../../config.example.toml")
            .replace("queue_capacity = 256", "queue_capacity = 0");
        let path = directory.write("audit.toml", &invalid_audit);
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
        let ConfiguredMzCommand::Translate(configured) =
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
        .replace("queue_capacity = 256", "queue_capacity = 0");
        let path = directory.write("unselected-secret.toml", &source);
        let error = match load_configuration(&path, translate_command(false, "primary")) {
            Ok(_) => panic!("无效审计配置必须拒绝"),
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
                "rules.json",
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
        let ProductCommand::Mz { command } = parsed.product;
        command
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

[observability.audit]
queue_capacity = 1
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
